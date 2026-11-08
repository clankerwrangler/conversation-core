//! History as the core hands it out: one complete entry per message or note, in pi's
//! entry shape (`type`, `id`, `timestamp`), with stored data read back. How records are
//! kept on disk (a reply saved in parts, bulky parts in side files, short field names) is
//! the store's business and does not show here.
//!
//! - a message: `{"type": "message", "message"}` (user, assistant, toolResult);
//! - a message the runner added (pi's custom message):
//!   `{"type": "custom_message", "customType", "content", "display", "details"?}`;
//! - runner bookkeeping (pi's custom entry): `{"type": "custom", "customType", "data"}`;
//! - `model_change` / `thinking_level_change` as pi has them.
//!
//! `id` is the record's sequence number, as text. There are no parent links: the history
//! has no branches. A reply still being written is not handed out, nor is anything after
//! it, so an entry never changes once handed out and `from` = last id + 1 continues.
//! An entry whose stored data has been deleted keeps its references and has `"expired": true`.

use serde_json::{Map, Value, json};

/// Entries for records numbered `from..=to`, from the records numbered `from` onward (read
/// to the end, so a reply's later parts are found).
pub fn entries(records: &[Value], to: u64) -> Vec<Value> {
    let mut out = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let n = record.get("n").and_then(Value::as_u64).unwrap_or(0);
        if n > to {
            break;
        }
        let text = |key: &str| record.get(key).and_then(Value::as_str);
        let mut entry = match text("k") {
            Some("user") => match text("source") {
                Some(source) => custom_message(record, source),
                None => message(record),
            },
            Some("assistant") if record.get("open") == Some(&Value::Bool(true)) => {
                match joined_reply(record, n, &records[index + 1..]) {
                    Some(entry) => entry,
                    None => break, // still being written
                }
            }
            Some("assistant") | Some("tool") => message(record),
            Some("app") => {
                json!({"type": "custom", "customType": record.get("ns"), "data": record.get("data")})
            }
            Some("settings") => {
                let mut fields = Map::new();
                if let Value::Object(map) = record {
                    for (key, value) in map {
                        if !["n", "t", "k"].contains(&key.as_str()) {
                            fields.insert(key.clone(), value.clone());
                        }
                    }
                }
                Value::Object(fields)
            }
            // chunk starts, checkpoint markers, a reply's later parts: storage only.
            _ => continue,
        };
        entry["id"] = Value::String(n.to_string());
        entry["timestamp"] = record.get("t").cloned().unwrap_or(Value::Null);
        if record.get("expired") == Some(&Value::Bool(true)) {
            entry["expired"] = Value::Bool(true);
        }
        out.push(entry);
    }
    out
}

fn message(record: &Value) -> Value {
    json!({"type": "message", "message": record.get("m")})
}

/// pi keeps a custom message's content as it was given: text stays text.
fn custom_message(record: &Value, source: &str) -> Value {
    let blocks = record.pointer("/m/content").cloned().unwrap_or(Value::Null);
    let content = match (record.get("text"), blocks.as_array().map(Vec::as_slice)) {
        (Some(Value::Bool(true)), Some([block]))
            if block.get("type").and_then(Value::as_str) == Some("text") =>
        {
            block.get("text").cloned().unwrap_or(Value::Null)
        }
        _ => blocks,
    };
    let mut entry = json!({"type": "custom_message", "customType": source, "content": content,
        "display": record.get("display").cloned().unwrap_or(Value::Bool(true))});
    if let Some(details) = record.get("details") {
        entry["details"] = details.clone();
    }
    entry
}

/// A reply saved in parts, joined: its blocks in order and its closing fields (usage, stop
/// reason, error). None while it has no closing part yet.
fn joined_reply(open: &Value, n: u64, later: &[Value]) -> Option<Value> {
    let mut message = open.get("m").cloned().unwrap_or_else(|| json!({}));
    let mut expired = false;
    for record in later {
        if record.get("k").and_then(Value::as_str) != Some("assistant-more")
            || record.get("reply").and_then(Value::as_u64) != Some(n)
        {
            continue;
        }
        expired |= record.get("expired") == Some(&Value::Bool(true));
        if let (Some(Value::Array(content)), Some(Value::Array(more))) =
            (message.get_mut("content"), record.pointer("/m/content"))
        {
            content.extend(more.iter().cloned());
        }
        if let Some(Value::Object(end)) = record.get("end") {
            for (key, value) in end {
                message[key.as_str()] = value.clone();
            }
            let mut entry = json!({"type": "message", "message": message});
            if expired {
                entry["expired"] = Value::Bool(true);
            }
            return Some(entry);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(n: u64, k: &str, extra: Value) -> Value {
        let mut r = json!({"n": n, "t": format!("t{n}"), "k": k});
        if let Value::Object(e) = extra {
            r.as_object_mut().unwrap().extend(e);
        }
        r
    }

    #[test]
    fn records_become_complete_entries() {
        let records = vec![
            rec(1, "chunk", json!({"chunk": 1})),
            rec(
                2,
                "user",
                json!({"m": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}),
            ),
            rec(
                3,
                "assistant",
                json!({"open": true, "m": {"role": "assistant", "content": [{"type": "toolCall", "id": "c"}]}}),
            ),
            rec(
                4,
                "tool",
                json!({"m": {"role": "toolResult", "toolCallId": "c"}}),
            ),
            rec(
                5,
                "assistant-more",
                json!({"reply": 3, "m": {"role": "assistant", "content": [{"type": "text", "text": "done"}]},
                "end": {"stopReason": "toolUse", "usage": {"input": 1}}}),
            ),
            rec(6, "app", json!({"ns": "harness.usage", "data": {"x": 1}})),
            rec(
                7,
                "user",
                json!({"source": "family", "display": true, "text": true, "details": {"messageId": "m1"},
                "m": {"role": "user", "content": [{"type": "text", "text": "hello"}]}}),
            ),
            rec(
                8,
                "settings",
                json!({"type": "thinking_level_change", "thinkingLevel": "high"}),
            ),
            rec(9, "checkpoint", json!({"id": "c1", "through": 8})),
        ];
        let out = entries(&records, u64::MAX);
        let ids: Vec<&str> = out.iter().map(|e| e["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["2", "3", "4", "6", "7", "8"]);
        assert_eq!(
            out[1]["message"]["content"].as_array().unwrap().len(),
            2,
            "reply parts joined"
        );
        assert_eq!(out[1]["message"]["stopReason"], "toolUse");
        assert_eq!(out[1]["timestamp"], "t3");
        assert_eq!(
            out[3],
            json!({"type": "custom", "customType": "harness.usage", "data": {"x": 1}, "id": "6", "timestamp": "t6"})
        );
        assert_eq!(out[4]["content"], "hello", "text given as text stays text");
        assert_eq!(out[4]["details"]["messageId"], "m1");
        assert_eq!(out[5]["type"], "thinking_level_change");
        assert!(out.iter().all(|e| e.get("parentId").is_none()));
    }

    #[test]
    fn nothing_from_a_reply_still_being_written_onward() {
        let records = vec![
            rec(1, "user", json!({"m": {"role": "user", "content": []}})),
            rec(
                2,
                "assistant",
                json!({"open": true, "m": {"role": "assistant", "content": []}}),
            ),
            rec(3, "tool", json!({"m": {"role": "toolResult"}})),
        ];
        let ids: Vec<String> = entries(&records, u64::MAX)
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, ["1"]);
    }

    #[test]
    fn to_bounds_the_entries() {
        let records: Vec<Value> = (1..=4)
            .map(|n| rec(n, "app", json!({"ns": "a", "data": n})))
            .collect();
        assert_eq!(entries(&records, 2).len(), 2);
    }
}
