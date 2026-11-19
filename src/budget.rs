//! The context budget: token estimates, the context usage line, and the overflow guard.
//!
//! The estimate uses the provider's exact count for the last
//! reply (`usage.totalTokens`) plus chars/4 for every message after it. A count from before
//! the latest checkpoint is not trusted (the context it measured was replaced).
//!
//! The overflow guard changes only the request about to be sent, never the saved context:
//! 1. tool results are set aside, oldest first, each replaced by a note naming the tool, the
//!    call, its size and where its full text is stored, until the request fits;
//! 2. if it still does not fit, tool-call arguments and images are replaced by references
//!    to their stored data, and a note at the end says why;
//! 3. if it still does not fit, the request is not sent (`context_too_large`).

use serde_json::{Value, json};
use std::path::Path;

/// Estimated characters per image.
const IMAGE_CHARS: usize = 4800;

fn utf16(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

fn text_and_image_chars(content: &Value) -> usize {
    match content {
        Value::String(s) => utf16(s),
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map_or(0, utf16),
                Some("image") => IMAGE_CHARS,
                _ => 0,
            })
            .sum(),
        _ => 0,
    }
}

/// Estimate tokens for one message: chars / 4, rounded up.
pub fn estimate_tokens(message: &Value) -> u64 {
    let chars = match message.get("role").and_then(Value::as_str) {
        // A system note (the context usage line) is sent like user text, so it counts like it.
        Some("user" | "toolResult" | "system") => {
            text_and_image_chars(message.get("content").unwrap_or(&Value::Null))
        }
        Some("assistant") => message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map_or(0, utf16),
                Some("thinking") => b.get("thinking").and_then(Value::as_str).map_or(0, utf16),
                Some("toolCall") => {
                    b.get("name").and_then(Value::as_str).map_or(0, utf16)
                        + utf16(
                            &serde_json::to_string(b.get("arguments").unwrap_or(&Value::Null))
                                .unwrap_or_default(),
                        )
                }
                _ => 0,
            })
            .sum(),
        _ => 0,
    };
    chars.div_ceil(4) as u64
}

/// Context tokens for a reply whose usage counts (not aborted, failed or zero).
fn usage_tokens(message: &Value) -> Option<u64> {
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let stop = message.get("stopReason").and_then(Value::as_str);
    if matches!(stop, Some("aborted" | "error")) {
        return None;
    }
    context_tokens(message.get("usage")?)
}

/// Context tokens: the provider's total, or the sum of its parts; None if zero.
pub fn context_tokens(usage: &Value) -> Option<u64> {
    let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    let tokens = match n("totalTokens") {
        0 => n("input") + n("output") + n("cacheRead") + n("cacheWrite"),
        t => t,
    };
    (tokens > 0).then_some(tokens)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    pub total: u64,
    /// The provider's count for the last reply, when one is trusted.
    pub measured: Option<u64>,
    /// The estimate for what came after it (everything, without a measured count).
    pub new: u64,
}

/// Estimate the context; usage is trusted only on messages at `trusted_from` or later.
pub fn estimate(messages: &[Value], trusted_from: usize) -> Estimate {
    let last = (trusted_from..messages.len())
        .rev()
        .find_map(|i| usage_tokens(&messages[i]).map(|u| (i, u)));
    match last {
        Some((i, measured)) => {
            let new: u64 = messages[i + 1..].iter().map(estimate_tokens).sum();
            Estimate {
                total: measured + new,
                measured: Some(measured),
                new,
            }
        }
        None => {
            let new = messages.iter().map(estimate_tokens).sum();
            Estimate {
                total: new,
                measured: None,
                new,
            }
        }
    }
}

fn k(tokens: u64) -> String {
    format!("{}k", (tokens + 500) / 1000)
}

/// "Context: about 91k of 272k tokens (84k measured + ~7k new)".
pub fn usage_line(est: &Estimate, window: Option<u64>) -> String {
    let of = window.map(|w| format!(" of {}", k(w))).unwrap_or_default();
    let how = match est.measured {
        Some(m) => format!("{} measured + ~{} new", k(m), k(est.new)),
        None => "estimated".to_string(),
    };
    format!("Context: about {}{of} tokens ({how})", k(est.total))
}

pub fn text_message(role: &str, text: &str) -> Value {
    json!({"role": role, "content": [{"type": "text", "text": text}], "timestamp": crate::provider::now_ms()})
}

pub enum Guarded {
    Fits(Vec<Value>),
    TooLarge { tokens: u64 },
}

/// Where a reference's stored data is, for a note: `<session>/log/... key K` or the image path.
fn stored_at(dir: &Path, reference: Option<&Value>) -> Option<String> {
    let r = reference?;
    if let Some(Value::String(path)) = r.get("$img") {
        return Some(dir.join(path).display().to_string());
    }
    let inner = r.get("$ref")?;
    let file = inner.get("file")?.as_str()?;
    let key = inner.get("key")?.as_str()?;
    Some(format!("{}, key {key}", dir.join(file).display()))
}

fn in_context_file(dir: &Path, index: usize) -> String {
    format!(
        "{}, message {}",
        dir.join("context.jsonl").display(),
        index + 1
    )
}

/// Apply the overflow guard. `refs` is the same context with references, when the runner
/// did not replace it; `total` is the estimate for `messages`.
pub fn guard(
    messages: Vec<Value>,
    refs: Option<&[Value]>,
    mut total: u64,
    window: u64,
    dir: &Path,
) -> Guarded {
    let mut messages = messages;
    if total <= window {
        return Guarded::Fits(messages);
    }
    let reference = |i: usize| refs.and_then(|r| r.get(i));
    // 1. Set tool results aside, oldest first.
    for i in 0..messages.len() {
        if total <= window {
            return Guarded::Fits(messages);
        }
        if messages[i].get("role").and_then(Value::as_str) != Some("toolResult") {
            continue;
        }
        let before = estimate_tokens(&messages[i]);
        let at = stored_at(dir, reference(i).and_then(|m| m.get("content")))
            .unwrap_or_else(|| in_context_file(dir, i));
        let m = &messages[i];
        let note = format!(
            "[Tool result set aside to fit the context window: {} call {}, about {} tokens. Full text: {at}]",
            m.get("toolName").and_then(Value::as_str).unwrap_or("?"),
            m.get("toolCallId").and_then(Value::as_str).unwrap_or("?"),
            k(before)
        );
        messages[i]["content"] = json!([{"type": "text", "text": note}]);
        total = total.saturating_sub(before) + estimate_tokens(&messages[i]);
    }
    if total <= window {
        return Guarded::Fits(messages);
    }
    // 2. Replace tool-call arguments and images with references to their stored data.
    for (i, message) in messages.iter_mut().enumerate() {
        let before = estimate_tokens(message);
        let original = reference(i);
        let Some(Value::Array(blocks)) = message.get_mut("content") else {
            continue;
        };
        for (j, block) in blocks.iter_mut().enumerate() {
            let stored = original
                .and_then(|m| m.get("content"))
                .and_then(|c| c.get(j));
            match block.get("type").and_then(Value::as_str) {
                Some("toolCall") => {
                    let at = stored_at(dir, stored.and_then(|b| b.get("arguments")))
                        .unwrap_or_else(|| in_context_file(dir, i));
                    block["arguments"] = json!({"omitted": at});
                }
                Some("image") => {
                    let at = stored_at(dir, stored).unwrap_or_else(|| in_context_file(dir, i));
                    *block = json!({"type": "text", "text": format!("[Image omitted: {at}]")});
                }
                _ => {}
            }
        }
        total = total.saturating_sub(before) + estimate_tokens(message);
    }
    let note = text_message(
        "user",
        &format!(
            "[core] This request was too large for the context window, so tool results, tool-call arguments and images were left out and replaced by references to their stored data. Your saved context is unchanged; to make it smaller, edit {}.",
            dir.join("context.jsonl").display()
        ),
    );
    total += estimate_tokens(&note);
    messages.push(note);
    if total <= window {
        Guarded::Fits(messages)
    } else {
        Guarded::TooLarge { tokens: total }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(t: &str) -> Value {
        text_message("user", t)
    }
    fn reply(t: &str, total: u64) -> Value {
        json!({"role": "assistant", "content": [{"type": "text", "text": t}], "usage": {"totalTokens": total}, "stopReason": "stop"})
    }
    fn result(id: &str, t: &str) -> Value {
        json!({"role": "toolResult", "toolCallId": id, "toolName": "bash", "content": [{"type": "text", "text": t}], "isError": false})
    }

    #[test]
    fn estimates_like_pi() {
        let msgs = vec![
            user(&"a".repeat(400)),
            reply("x", 84_000),
            result("c1", &"b".repeat(28_000)),
        ];
        assert_eq!(
            estimate(&msgs, 0),
            Estimate {
                total: 91_000,
                measured: Some(84_000),
                new: 7_000
            }
        );
        assert_eq!(
            usage_line(&estimate(&msgs, 0), Some(272_000)),
            "Context: about 91k of 272k tokens (84k measured + ~7k new)"
        );
        // A count from before the checkpoint is not trusted.
        let e = estimate(&msgs, 2);
        assert_eq!((e.measured, e.total), (None, 100 + 1 + 7_000));
        assert_eq!(usage_line(&e, None), "Context: about 7k tokens (estimated)");
    }

    #[test]
    fn the_system_usage_line_counts_toward_the_window() {
        let line = text_message("system", &"x".repeat(400));
        assert_eq!(estimate_tokens(&line), 100);
        // A request at the window's edge without the line is over it with the line.
        let msgs = vec![user(&"a".repeat(4_000)), line];
        assert_eq!(estimate(&msgs, 0).total, 1_100);
    }

    #[test]
    fn guard_sets_aside_oldest_results_first() {
        let dir = Path::new("/s");
        let msgs = vec![
            user("go"),
            result("c1", &"a".repeat(4000)),
            result("c2", &"b".repeat(4000)),
        ];
        let total: u64 = msgs.iter().map(estimate_tokens).sum();
        let refs = vec![
            user("go"),
            json!({"content": {"$ref": {"file": "log/000001.payload.jsonl", "key": "2.0"}}}),
            Value::Null,
        ];
        let Guarded::Fits(out) = guard(msgs, Some(&refs), total, 1500, dir) else {
            panic!("should fit")
        };
        let first = out[1]["content"][0]["text"].as_str().unwrap_or("");
        assert_eq!(
            first,
            "[Tool result set aside to fit the context window: bash call c1, about 1k tokens. Full text: /s/log/000001.payload.jsonl, key 2.0]"
        );
        assert_eq!(
            out[2]["content"][0]["text"].as_str().map(str::len),
            Some(4000),
            "the newest result stays"
        );
        let msgs = vec![user(&"u".repeat(40_000))];
        let total = estimate_tokens(&msgs[0]);
        assert!(matches!(
            guard(msgs, None, total, 1000, dir),
            Guarded::TooLarge { .. }
        ));
    }
}
