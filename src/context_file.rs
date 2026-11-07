//! Context = file, file = context.
//!
//! Before each request the core writes the context, one message per line, exactly
//! as the model receives it, to `<session>/context.jsonl`. At the next request
//! boundary, if the file was edited, the edited messages plus every record added
//! since the file was written become the new context (a checkpoint). A file that
//! does not parse changes nothing and is reported.
//!
//! `state.json` remembers the last two publications (`sha256`, `through`). It is
//! written before the file, so a crash between the two leaves the previous file in
//! place, which still matches the previous publication and is not read as an edit.

use crate::store::{Error, Session};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, Error>;

pub const FILE: &str = "context.jsonl";

/// What happened to the file at a request boundary.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// No file yet, or it is exactly what the core last wrote.
    Unchanged,
    /// The edit became the new context.
    Accepted { messages: usize },
    /// The file did not parse; the context is unchanged.
    Rejected(String),
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

/// Write the current context to the file.
pub fn publish(session: &Session) -> Result<()> {
    let text = render(&session.context()?)?;
    let through = session.last_seq()?;
    let mut state = session.read_state()?;
    let previous = state
        .get("contextFile")
        .and_then(|f| f.get("current"))
        .cloned()
        .unwrap_or(Value::Null);
    state["contextFile"] =
        json!({"current": {"sha256": sha(&text), "through": through}, "previous": previous});
    session.write_state(&state)?;
    session.write_file(FILE, &text)
}

/// At a request boundary: take an edited file as the new context.
pub fn accept(session: &Session) -> Result<Outcome> {
    let Some(text) = session.read_file(FILE)? else {
        return Ok(Outcome::Unchanged);
    };
    let state = session.read_state()?;
    let published = |which: &str| {
        state
            .pointer(&format!("/contextFile/{which}"))
            .filter(|v| !v.is_null())
            .cloned()
    };
    let Some(current) = published("current") else {
        return Ok(Outcome::Unchanged);
    };
    let digest = sha(&text);
    let matches = |p: &Value| p.get("sha256").and_then(Value::as_str) == Some(digest.as_str());
    if matches(&current) || published("previous").is_some_and(|p| matches(&p)) {
        return Ok(Outcome::Unchanged);
    }
    let edited = match parse(&text) {
        Ok(messages) => messages,
        Err(reason) => return Ok(Outcome::Rejected(reason)),
    };
    let through = current.get("through").and_then(Value::as_u64).unwrap_or(0);
    let mut context = edited;
    context.extend(session.model_messages_after(through)?);
    let messages = context.len();
    session.checkpoint(&context)?;
    Ok(Outcome::Accepted { messages })
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
    fn the_file_is_the_context_one_message_per_line() {
        let s = fresh();
        s.append("user", &text("a")).unwrap();
        s.append("assistant", &json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "t", "thinkingSignature": "enc"}]})).unwrap();
        publish(&s).unwrap();
        let file = s.read_file(FILE).unwrap().unwrap();
        assert_eq!(
            parse(&file).unwrap(),
            s.context().unwrap(),
            "full messages, encrypted parts included"
        );
        assert_eq!(accept(&s).unwrap(), Outcome::Unchanged);
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
}
