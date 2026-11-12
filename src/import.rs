//! `core import <session-dir> <id>`: a new session from an older transcript, one JSON line
//! per record on standard input, in order. Each line is a record as the store keeps it,
//! without its number: `{"t", "k", ...}`.
//!
//! - `k` user / assistant / tool: `m` is the message, written like `append` writes it
//!   (bulky parts to side files); other fields (`source`, `display`, ...) are kept.
//! - `k` app / settings: written like `append_entry`. A `model_change` or
//!   `thinking_level_change` also becomes the current choice in meta.json, as `select` does.
//! - `k` checkpoint: `messages` is the complete context at that point, written like
//!   `checkpoint` (content, marker, prune, seal).
//! - `k` meta: its other fields are added to meta.json (for example `created`).
//!
//! `t` is the record's original time. The folder must not exist yet: an import never
//! writes into a session that has records of its own.

use crate::store::{Error, Session};
use serde_json::Value;
use std::io::BufRead;
use std::path::Path;

/// Imports the lines of `input` into a new session in `dir`. Returns the number of records.
pub fn import(dir: &Path, id: &str, input: impl BufRead) -> Result<u64, Error> {
    if dir.exists() {
        return Err(Error::Format(format!("{} already exists", dir.display())));
    }
    let session = Session::create(dir, id)?;
    let mut meta = session.meta()?;
    let (mut chunk, mut last) = session.position()?;
    for (index, line) in input.lines().enumerate() {
        let line = line.map_err(|e| Error::Io("<stdin>".into(), e))?;
        if line.trim().is_empty() {
            continue;
        }
        let mut record: Value = serde_json::from_str(&line)?;
        let at = |what: &str| Error::Format(format!("line {}: {what}", index + 1));
        let kind = record
            .get("k")
            .and_then(Value::as_str)
            .ok_or_else(|| at("no k"))?
            .to_string();
        let t = match record.get("t").and_then(Value::as_str) {
            Some(t) => t.to_string(),
            None if kind == "meta" => String::new(),
            None => return Err(at("no t")),
        };
        match kind.as_str() {
            "user" | "assistant" | "tool" => {
                let message = record
                    .get_mut("m")
                    .map(Value::take)
                    .ok_or_else(|| at("no m"))?;
                last += 1;
                session.write_record(chunk, last, &t, &kind, &message, record)?;
            }
            "app" | "settings" => {
                if kind == "settings" {
                    match record.get("type").and_then(Value::as_str) {
                        Some("model_change") => {
                            meta["model"] = serde_json::json!({"provider": record["provider"], "id": record["modelId"]})
                        }
                        Some("thinking_level_change") => {
                            meta["reasoning"] = record["thinkingLevel"].clone()
                        }
                        _ => {}
                    }
                }
                last += 1;
                session.write_entry(chunk, last, &t, &kind, record)?;
            }
            "checkpoint" => {
                let Some(Value::Array(messages)) = record.get("messages") else {
                    return Err(at("checkpoint without messages"));
                };
                session.checkpoint_at(messages, &t)?;
                (chunk, last) = session.position()?;
            }
            "meta" => {
                if let (Value::Object(m), Value::Object(fields)) = (&mut meta, record) {
                    for (key, value) in fields {
                        if !["k", "t", "format", "id"].contains(&key.as_str()) {
                            m.insert(key, value);
                        }
                    }
                }
            }
            other => return Err(at(&format!("unknown kind {other}"))),
        }
    }
    session.write_file("meta.json", &serde_json::to_string(&meta)?)?;
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fresh() -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!("core-import-test-{}-{n}", std::process::id()))
    }

    fn lines(values: &[Value]) -> String {
        values.iter().map(|v| format!("{v}\n")).collect()
    }

    #[test]
    fn records_keep_their_times_and_context_follows_the_checkpoint() {
        let dir = fresh();
        let user =
            |text: &str| json!({"role": "user", "content": [{"type": "text", "text": text}]});
        let input = lines(&[
            json!({"k": "meta", "created": "2026-01-01T00:00:00.000Z"}),
            json!({"t": "2026-01-01T00:00:01.000Z", "k": "user", "m": user("one"), "pi": "a1"}),
            json!({"t": "2026-01-01T00:00:02.000Z", "k": "settings", "type": "model_change", "provider": "p", "modelId": "m"}),
            json!({"t": "2026-01-01T00:00:03.000Z", "k": "tool", "m": {"role": "toolResult", "toolCallId": "c", "content": [{"type": "text", "text": "big"}]}}),
            json!({"t": "2026-01-01T00:00:04.000Z", "k": "checkpoint", "messages": [user("summary")]}),
            json!({"t": "2026-01-01T00:00:05.000Z", "k": "user", "m": user("two"), "source": "family", "display": false}),
        ]);
        let last = import(&dir, "s1", input.as_bytes()).unwrap();
        let session = Session::open(&dir).unwrap();
        assert_eq!(last, session.last_seq().unwrap());
        let context = session.context().unwrap();
        assert_eq!(context, vec![user("summary"), user("two")]);
        let records = session.history(1, u64::MAX, true).unwrap();
        let first = records.iter().find(|r| r["k"] == "user").unwrap();
        assert_eq!(first["t"], "2026-01-01T00:00:01.000Z");
        assert_eq!(first["pi"], "a1");
        let tool = records.iter().find(|r| r["k"] == "tool").unwrap();
        assert_eq!(
            tool["m"]["content"][0]["text"], "big",
            "tool output read back from its side file"
        );
        let meta = session.meta().unwrap();
        assert_eq!(meta["model"], json!({"provider": "p", "id": "m"}));
        assert_eq!(meta["created"], "2026-01-01T00:00:00.000Z");
        assert_eq!(meta["id"], "s1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn never_writes_into_an_existing_folder() {
        let dir = fresh();
        std::fs::create_dir_all(&dir).unwrap();
        assert!(import(&dir, "s", "".as_bytes()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
