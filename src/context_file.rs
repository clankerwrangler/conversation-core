//! Context = file, file = context.
//!
//! Before each request the core synchronizes `<session>/context.jsonl` under
//! `references.lock`: if the file was edited since the last publication, the
//! edited messages plus every record added since that publication become the new
//! context (a checkpoint). A file that does not parse changes nothing and is
//! reported.
//!
//! The core rewrites the file (publishes the mirror, one message per line exactly
//! as the model receives it) only while no tool call is running: before a request
//! and again right after a reply. While a tool call runs, an edit is still taken
//! in, but the file is left as it is, so a tool editing the file is never
//! overwritten; the accepted file is recorded as the current publication so it is
//! not taken in twice. Before writing `context.jsonl`, sync verifies the file on
//! disk still matches what it read, retrying on a mid-sync write and leaving the
//! file untouched if contention persists (specification/ContextFileSync.tla).
//!
//! `state.json` remembers the last two publications (`sha256`, `through`). It is
//! written before the file, so a crash between the two leaves the previous file in
//! place, which still matches the previous publication and is not read as an edit.

use crate::store::{Error, Session};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, Error>;

pub const FILE: &str = "context.jsonl";
const MAX_SYNC_ATTEMPTS: usize = 3;

/// The line in the core-built instructions that tells the model about the file.
pub fn note(session_dir: &std::path::Path) -> String {
    format!(
        "Your context is mirrored at {}. Edit the file to change it; edits take effect at your next request.",
        session_dir.join(FILE).display()
    )
}

/// What happened to the file at a request boundary.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// No file yet, or it is exactly what the core last wrote.
    Unchanged,
    /// The edit became the new context.
    Accepted { messages: usize },
    /// The file did not parse or could not be published safely; the context is unchanged.
    Rejected(String),
}

/// Snapshot and edit outcome produced by synchronizing `context.jsonl`.
pub struct Synced {
    pub outcome: Outcome,
    pub checkpointed: bool,
    pub refs: Vec<Value>,
    pub from_checkpoint: usize,
}

fn sha(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn render(messages: &[Value]) -> Result<String> {
    let mut text = String::new();
    for message in messages {
        text.push_str(&serde_json::to_string(message)?);
        text.push('\n');
    }
    Ok(text)
}

/// One JSON object per non-empty line, each with a string `role`.
fn parse(text: &str) -> std::result::Result<Vec<Value>, String> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| {
            let value: Value =
                serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
            match value.get("role") {
                Some(Value::String(_)) => Ok(value),
                _ => Err(format!("line {}: not a message (no role)", i + 1)),
            }
        })
        .collect()
}

enum Plan {
    Unchanged,
    Accept(Vec<Value>),
    Reject(String),
}

fn plan_accept(session: &Session, observed: Option<&str>, state: &Value) -> Result<Plan> {
    let Some(text) = observed else {
        return Ok(Plan::Unchanged);
    };
    let published = |which: &str| {
        state
            .pointer(&format!("/contextFile/{which}"))
            .filter(|v| !v.is_null())
            .cloned()
    };
    let Some(current) = published("current") else {
        return Ok(Plan::Unchanged);
    };
    let digest = sha(text);
    let matches = |p: &Value| p.get("sha256").and_then(Value::as_str) == Some(digest.as_str());
    if matches(&current) || published("previous").is_some_and(|p| matches(&p)) {
        return Ok(Plan::Unchanged);
    }
    let edited = match parse(text) {
        Ok(messages) => messages,
        Err(reason) => return Ok(Plan::Reject(reason)),
    };
    // Every reference must name stored data of this session.
    if let Err(err) = session.resolve(&edited) {
        return Ok(Plan::Reject(err.to_string()));
    }
    let through = current.get("through").and_then(Value::as_u64).unwrap_or(0);
    let mut context = edited;
    context.extend(session.model_messages_after(through)?);
    Ok(Plan::Accept(context))
}

/// Synchronize `context.jsonl` at a request boundary: accept an edited file (if
/// any), run `before_publish` once (for example, starting the turn and appending
/// the new user message), load the resulting context snapshot, and, if `publish`,
/// publish the mirror. Callers publish only while no tool call is running, so
/// edits by the session's own tool calls are kept. An edit by another process
/// that lands between the last check and the rename is overwritten. Caller holds
/// `references.lock`.
pub fn sync_before_publish(
    session: &Session,
    publish: bool,
    before_publish: impl FnOnce() -> Result<()>,
) -> Result<Synced> {
    sync_with(session, publish, before_publish, |_| {})
}

#[cfg(test)]
pub fn sync(session: &Session) -> Result<Synced> {
    sync_before_publish(session, true, || Ok(()))
}

fn sync_with(
    session: &Session,
    publish: bool,
    before_publish: impl FnOnce() -> Result<()>,
    mut on_before_commit: impl FnMut(usize),
) -> Result<Synced> {
    let mut before_publish = Some(before_publish);
    let mut checkpointed = false;
    for attempt in 0..MAX_SYNC_ATTEMPTS {
        let observed = session.read_file(FILE)?;
        let mut state = session.read_state()?;
        let plan = plan_accept(session, observed.as_deref(), &state)?;

        // Check for a concurrent external write before mutating the session log or disk.
        on_before_commit(attempt);
        if session.read_file(FILE)? != observed {
            continue;
        }

        let outcome = match plan {
            Plan::Unchanged => Outcome::Unchanged,
            Plan::Reject(reason) => Outcome::Rejected(reason),
            Plan::Accept(context) => {
                let messages = context.len();
                session.checkpoint(&context)?;
                checkpointed = true;
                Outcome::Accepted { messages }
            }
        };
        if !publish {
            // Leave the file as it is. Record an accepted file as the current
            // publication, covering the context through its checkpoint, so it is not
            // taken in again and a later edit keeps what came after.
            if let (Outcome::Accepted { .. }, Some(text)) = (&outcome, observed.as_deref()) {
                let previous = state
                    .pointer("/contextFile/current")
                    .cloned()
                    .unwrap_or(Value::Null);
                state["contextFile"] = json!({"current": {"sha256": sha(text), "through": session.last_seq()?}, "previous": previous});
                session.write_state(&state)?;
            }
            if let Some(hook) = before_publish.take() {
                hook()?;
            }
            let (refs, from_checkpoint, _) = session.context_parts_with_seq()?;
            return Ok(Synced {
                outcome,
                checkpointed,
                refs,
                from_checkpoint,
            });
        }
        if let Some(hook) = before_publish.take() {
            hook()?;
        }
        let (refs, from_checkpoint, through) = session.context_parts_with_seq()?;
        let text = render(&refs)?;
        let digest = sha(&text);
        let previous = state
            .get("contextFile")
            .and_then(|f| f.get("current"))
            .cloned()
            .unwrap_or(Value::Null);
        let same_meta = previous.get("sha256").and_then(Value::as_str) == Some(digest.as_str())
            && previous.get("through").and_then(Value::as_u64) == Some(through);
        let existing_matches = observed.as_deref() == Some(text.as_str());

        // Re-check immediately before writing state.json and context.jsonl.
        if session.read_file(FILE)? != observed {
            continue;
        }

        if !same_meta {
            state["contextFile"] =
                json!({"current": {"sha256": digest, "through": through}, "previous": previous});
            session.write_state(&state)?;
        }
        if !existing_matches {
            session.write_file(FILE, &text)?;
        }
        return Ok(Synced {
            outcome,
            checkpointed,
            refs,
            from_checkpoint,
        });
    }
    if let Some(hook) = before_publish.take() {
        hook()?;
    }
    let (refs, from_checkpoint, _) = session.context_parts_with_seq()?;
    Ok(Synced {
        outcome: Outcome::Rejected(
            "context.jsonl changed concurrently during publish; left unmodified".into(),
        ),
        checkpointed,
        refs,
        from_checkpoint,
    })
}

/// Write the current context to the file: readable text inline, tool data, reasoning
/// and images as references to their stored data.
#[cfg(test)]
pub fn publish(session: &Session) -> Result<()> {
    let (refs, _, through) = session.context_parts_with_seq()?;
    let text = render(&refs)?;
    let digest = sha(&text);
    let mut state = session.read_state()?;
    let previous = state
        .get("contextFile")
        .and_then(|f| f.get("current"))
        .cloned()
        .unwrap_or(Value::Null);
    let same_meta = previous.get("sha256").and_then(Value::as_str) == Some(digest.as_str())
        && previous.get("through").and_then(Value::as_u64) == Some(through);
    let existing_matches = session.read_file(FILE)?.as_deref() == Some(text.as_str());
    if same_meta && existing_matches {
        return Ok(());
    }
    if !same_meta {
        state["contextFile"] =
            json!({"current": {"sha256": digest, "through": through}, "previous": previous});
        session.write_state(&state)?;
    }
    if !existing_matches {
        session.write_file(FILE, &text)?;
    }
    Ok(())
}

/// At a request boundary: take an edited file as the new context.
#[cfg(test)]
pub fn accept(session: &Session) -> Result<Outcome> {
    let observed = session.read_file(FILE)?;
    let state = session.read_state()?;
    match plan_accept(session, observed.as_deref(), &state)? {
        Plan::Unchanged => Ok(Outcome::Unchanged),
        Plan::Reject(reason) => Ok(Outcome::Rejected(reason)),
        Plan::Accept(context) => {
            let messages = context.len();
            session.checkpoint(&context)?;
            Ok(Outcome::Accepted { messages })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT: AtomicU32 = AtomicU32::new(0);

    fn fresh() -> Session {
        let dir = std::env::temp_dir().join(format!(
            "core-cf-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        Session::create(&dir, "t").unwrap()
    }

    fn text(t: &str) -> Value {
        json!({"role": "user", "content": [{"type": "text", "text": t}]})
    }

    #[test]
    fn the_file_and_its_references_are_the_context() {
        let s = fresh();
        s.append("user", &text("a")).unwrap();
        s.append(
            "assistant",
            &json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "readable summary", "thinkingSignature": "enc"},
            {"type": "toolCall", "id": "c", "name": "ipython", "arguments": {"code": "1+1"}}]}),
        )
        .unwrap();
        s.append("tool", &json!({"role": "toolResult", "toolCallId": "c", "content": [{"type": "text", "text": "2"}]})).unwrap();
        publish(&s).unwrap();
        let file = s.read_file(FILE).unwrap().unwrap();
        assert!(
            file.contains("readable summary") && file.contains("\"text\":\"a\""),
            "readable text stays inline"
        );
        assert!(
            !file.contains("\"enc\"") && !file.contains("1+1"),
            "bulky parts are references"
        );
        assert!(
            file.contains("log/000001.reasoning.jsonl")
                && file.contains("log/000001.payload.jsonl")
        );
        assert_eq!(
            s.resolve(&parse(&file).unwrap()).unwrap(),
            s.context().unwrap(),
            "file + references = context"
        );
        assert_eq!(accept(&s).unwrap(), Outcome::Unchanged);
    }

    #[test]
    fn an_edit_can_replace_a_reference_with_text_and_keep_others() {
        let s = fresh();
        s.append("user", &text("a")).unwrap();
        s.append("tool", &json!({"role": "toolResult", "toolCallId": "c", "content": [{"type": "text", "text": "long output"}]})).unwrap();
        s.append("tool", &json!({"role": "toolResult", "toolCallId": "d", "content": [{"type": "text", "text": "keep me"}]})).unwrap();
        publish(&s).unwrap();
        let mut lines = parse(&s.read_file(FILE).unwrap().unwrap()).unwrap();
        lines[1]["content"] = json!([{"type": "text", "text": "short"}]);
        s.write_file(FILE, &render(&lines).unwrap()).unwrap();
        assert!(matches!(accept(&s).unwrap(), Outcome::Accepted { .. }));
        let ctx = s.context().unwrap();
        assert_eq!(ctx[1]["content"][0]["text"], json!("short"));
        assert_eq!(
            ctx[2]["content"][0]["text"],
            json!("keep me"),
            "a kept reference still resolves"
        );
    }

    #[test]
    fn references_outside_the_session_log_are_rejected() {
        let s = fresh();
        s.append("user", &text("a")).unwrap();
        publish(&s).unwrap();
        for bad in [
            json!({"role": "user", "content": {"$ref": {"file": "../../etc/passwd", "key": "x"}}}),
            json!({"role": "user", "content": {"$ref": {"file": "log/000001.payload.jsonl", "key": "missing"}}}),
            json!({"role": "user", "content": [{"type": "image", "$img": "/etc/hostname"}]}),
        ] {
            s.write_file(FILE, &render(&[bad]).unwrap()).unwrap();
            assert!(matches!(accept(&s).unwrap(), Outcome::Rejected(_)));
        }
        assert_eq!(s.context().unwrap(), vec![text("a")]);
    }

    #[test]
    fn an_edit_becomes_the_context_and_keeps_what_came_after() {
        let s = fresh();
        s.append("user", &text("a")).unwrap();
        s.append("user", &text("b")).unwrap();
        publish(&s).unwrap();
        s.write_file(FILE, &render(&[text("summary")]).unwrap())
            .unwrap();
        s.append("user", &text("c")).unwrap();
        assert_eq!(accept(&s).unwrap(), Outcome::Accepted { messages: 2 });
        assert_eq!(s.context().unwrap(), vec![text("summary"), text("c")]);
        publish(&s).unwrap();
        assert_eq!(
            accept(&s).unwrap(),
            Outcome::Unchanged,
            "the republished file is not an edit"
        );
    }

    #[test]
    fn a_file_that_does_not_parse_changes_nothing() {
        let s = fresh();
        s.append("user", &text("a")).unwrap();
        publish(&s).unwrap();
        s.write_file(FILE, "{\"role\":\"user\"\nnot json\n")
            .unwrap();
        assert!(matches!(accept(&s).unwrap(), Outcome::Rejected(_)));
        assert_eq!(s.context().unwrap(), vec![text("a")]);
    }

    #[test]
    fn a_crash_between_state_and_file_is_not_an_edit() {
        let s = fresh();
        s.append("user", &text("a")).unwrap();
        publish(&s).unwrap();
        let old = s.read_file(FILE).unwrap().unwrap();
        s.append("user", &text("b")).unwrap();
        publish(&s).unwrap();
        // Simulate the crash: the second publication's state was saved, its file was not.
        s.write_file(FILE, &old).unwrap();
        assert_eq!(accept(&s).unwrap(), Outcome::Unchanged);
        assert_eq!(s.context().unwrap(), vec![text("a"), text("b")]);
    }

    #[test]
    fn no_file_or_no_publication_is_unchanged() {
        let s = fresh();
        assert_eq!(accept(&s).unwrap(), Outcome::Unchanged);
        s.write_file(FILE, "garbage").unwrap();
        assert_eq!(
            accept(&s).unwrap(),
            Outcome::Unchanged,
            "only files the core published are compared"
        );
    }

    #[test]
    fn republishing_unchanged_context_skips_rewrites() {
        use std::os::unix::fs::MetadataExt;
        let s = fresh();
        s.append("user", &text("a")).unwrap();
        publish(&s).unwrap();
        let state_ino = std::fs::metadata(s.dir().join("state.json")).unwrap().ino();
        let ctx_ino = std::fs::metadata(s.dir().join(FILE)).unwrap().ino();
        // Exact same context and sequence: neither file is rewritten.
        publish(&s).unwrap();
        assert_eq!(
            std::fs::metadata(s.dir().join("state.json")).unwrap().ino(),
            state_ino
        );
        assert_eq!(
            std::fs::metadata(s.dir().join(FILE)).unwrap().ino(),
            ctx_ino
        );
        // Non-model record advances `through` without changing context text: state updates, context file is untouched.
        s.append_entry("app", json!({"ns": "note", "data": {"x": 1}}))
            .unwrap();
        publish(&s).unwrap();
        assert_eq!(
            std::fs::metadata(s.dir().join(FILE)).unwrap().ino(),
            ctx_ino
        );
        assert_eq!(
            s.read_state()
                .unwrap()
                .pointer("/contextFile/current/through")
                .and_then(Value::as_u64),
            Some(s.last_seq().unwrap())
        );
        assert_eq!(accept(&s).unwrap(), Outcome::Unchanged);
    }

    #[test]
    fn sync_accepts_edit_and_publishes_without_double_accept() {
        use std::os::unix::fs::MetadataExt;
        let s = fresh();
        s.append("user", &text("a")).unwrap();
        s.append("user", &text("b")).unwrap();
        let first = sync(&s).unwrap();
        assert_eq!(first.outcome, Outcome::Unchanged);
        assert!(!first.checkpointed);

        s.write_file(FILE, &render(&[text("summary")]).unwrap())
            .unwrap();
        s.append("user", &text("c")).unwrap();

        let second = sync(&s).unwrap();
        assert_eq!(second.outcome, Outcome::Accepted { messages: 2 });
        assert!(second.checkpointed);
        assert_eq!(s.context().unwrap(), vec![text("summary"), text("c")]);
        assert_eq!(
            parse(&s.read_file(FILE).unwrap().unwrap()).unwrap(),
            vec![text("summary"), text("c")]
        );

        let state_ino = std::fs::metadata(s.dir().join("state.json")).unwrap().ino();
        let ctx_ino = std::fs::metadata(s.dir().join(FILE)).unwrap().ino();
        let third = sync(&s).unwrap();
        assert_eq!(third.outcome, Outcome::Unchanged);
        assert!(!third.checkpointed);
        assert_eq!(
            std::fs::metadata(s.dir().join("state.json")).unwrap().ino(),
            state_ino
        );
        assert_eq!(
            std::fs::metadata(s.dir().join(FILE)).unwrap().ino(),
            ctx_ino
        );
    }

    #[test]
    fn sync_without_publish_accepts_once_and_leaves_the_file() {
        use std::os::unix::fs::MetadataExt;
        let s = fresh();
        s.append("user", &text("a")).unwrap();
        s.append("user", &text("b")).unwrap();
        assert_eq!(sync(&s).unwrap().outcome, Outcome::Unchanged);

        // A tool call is running: the edit is taken in, the file is not rewritten.
        let edited = render(&[text("summary")]).unwrap();
        s.write_file(FILE, &edited).unwrap();
        let ino = std::fs::metadata(s.dir().join(FILE)).unwrap().ino();
        let held = sync_before_publish(&s, false, || s.append("user", &text("c")).map(|_| ()))
            .unwrap();
        assert_eq!(held.outcome, Outcome::Accepted { messages: 1 });
        assert!(held.checkpointed);
        assert_eq!(s.context().unwrap(), vec![text("summary"), text("c")]);
        assert_eq!(s.read_file(FILE).unwrap().unwrap(), edited);
        assert_eq!(std::fs::metadata(s.dir().join(FILE)).unwrap().ino(), ino);

        // Still running: the same file is not taken in again.
        s.append("user", &text("d")).unwrap();
        let again = sync_before_publish(&s, false, || Ok(())).unwrap();
        assert_eq!(again.outcome, Outcome::Unchanged);
        assert!(!again.checkpointed);

        // A second edit keeps what came after the first one.
        s.write_file(FILE, &render(&[text("shorter")]).unwrap())
            .unwrap();
        let second = sync_before_publish(&s, false, || Ok(())).unwrap();
        assert_eq!(second.outcome, Outcome::Accepted { messages: 3 });
        assert_eq!(
            s.context().unwrap(),
            vec![text("shorter"), text("c"), text("d")]
        );

        // Once nothing runs, the file is rewritten from the context and is not an edit.
        assert_eq!(sync(&s).unwrap().outcome, Outcome::Unchanged);
        assert_eq!(
            parse(&s.read_file(FILE).unwrap().unwrap()).unwrap(),
            vec![text("shorter"), text("c"), text("d")]
        );
        assert_eq!(sync(&s).unwrap().outcome, Outcome::Unchanged);
    }

    #[test]
    fn sync_retries_when_concurrent_write_lands_before_publish() {
        let s = fresh();
        s.append("user", &text("a")).unwrap();
        assert_eq!(sync(&s).unwrap().outcome, Outcome::Unchanged);

        s.append("user", &text("b")).unwrap();
        let synced = sync_with(
            &s,
            true,
            || Ok(()),
            |attempt| {
                if attempt == 0 {
                    s.write_file(FILE, &render(&[text("concurrent")]).unwrap())
                        .unwrap();
                }
            },
        )
        .unwrap();
        assert_eq!(synced.outcome, Outcome::Accepted { messages: 2 });
        assert!(synced.checkpointed);
        assert_eq!(s.context().unwrap(), vec![text("concurrent"), text("b")]);
        assert_eq!(
            parse(&s.read_file(FILE).unwrap().unwrap()).unwrap(),
            vec![text("concurrent"), text("b")]
        );
        assert_eq!(sync(&s).unwrap().outcome, Outcome::Unchanged);
    }

    #[test]
    fn sync_leaves_disk_untouched_and_reports_rejection_on_persistent_contention() {
        let s = fresh();
        s.append("user", &text("a")).unwrap();
        assert_eq!(sync(&s).unwrap().outcome, Outcome::Unchanged);
        let state_before = s.read_state().unwrap();

        s.append("user", &text("b")).unwrap();
        let synced = sync_with(
            &s,
            true,
            || Ok(()),
            |attempt| {
                s.write_file(
                    FILE,
                    &render(&[text(&format!("writer-{attempt}"))]).unwrap(),
                )
                .unwrap();
            },
        )
        .unwrap();
        assert!(
            matches!(synced.outcome, Outcome::Rejected(ref msg) if msg.contains("changed concurrently")),
            "expected contention rejection, got {:?}",
            synced.outcome
        );
        assert!(!synced.checkpointed);
        assert_eq!(
            parse(&s.read_file(FILE).unwrap().unwrap()).unwrap(),
            vec![text("writer-2")],
            "external writer's latest file must stay intact on disk"
        );
        assert_eq!(
            s.read_state().unwrap(),
            state_before,
            "state.json must stay at the prior publication so the edit is accepted next"
        );

        let recovered = sync(&s).unwrap();
        assert_eq!(recovered.outcome, Outcome::Accepted { messages: 2 });
        assert!(recovered.checkpointed);
        assert_eq!(s.context().unwrap(), vec![text("writer-2"), text("b")]);
    }
}
