//! OpenAI Responses API wire format (Codex and Grok): context messages -> request input.
//! Mirrors the harness native provider's `replay` (src/native-provider.mjs), which
//! deliberately does not synthesize missing tool outputs.

use crate::store::Error;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

type Result<T> = std::result::Result<T, Error>;

/// The model a request is for; used to decide whether stored reasoning can be replayed.
pub struct Target<'a> {
    pub api: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
}

fn fail(what: &str) -> Error {
    Error::Format(what.to_string())
}

fn nonempty(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn input_content(content: &Value) -> Result<Vec<Value>> {
    match content {
        Value::String(text) => Ok(vec![json!({"type": "input_text", "text": text})]),
        Value::Null => Ok(Vec::new()),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => Ok(json!({"type": "input_text", "text": block.get("text").cloned().unwrap_or(Value::Null)})),
                Some("image") => {
                    let mime = block.get("mimeType").and_then(Value::as_str).unwrap_or("");
                    let data = block.get("data").and_then(Value::as_str).unwrap_or("");
                    Ok(json!({"type": "input_image", "image_url": format!("data:{mime};base64,{data}"), "detail": "auto"}))
                }
                _ => Err(fail("unsupported_input_block")),
            })
            .collect(),
        _ => Err(fail("unsupported_input_block")),
    }
}

/// [call id, optional item id] for a tool call block.
fn call_ids(call: &Value) -> Result<Vec<String>> {
    if let (Some(c), Some(i)) = (
        nonempty(call.get("providerCallId")),
        nonempty(call.get("providerItemId")),
    ) {
        return Ok(vec![c.to_string(), i.to_string()]);
    }
    let id = call.get("id").and_then(Value::as_str).unwrap_or("");
    let parts: Vec<String> = id.split('|').map(str::to_string).collect();
    if parts.len() > 2 {
        return Err(fail("ambiguous_replay_call_id"));
    }
    Ok(parts)
}

fn parse(text: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|_| fail("invalid_provider_json"))
}

/// Context messages -> Responses `input` items.
pub fn input(target: &Target, messages: &[Value]) -> Result<Vec<Value>> {
    let mut identities: HashMap<String, Vec<String>> = HashMap::new();
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        for block in msg
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                continue;
            }
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let ids = call_ids(block)?;
            if identities.get(&id).is_some_and(|known| *known != ids) {
                return Err(fail("ambiguous_replay_call_id"));
            }
            identities.insert(id, ids);
        }
    }

    let mut out = Vec::new();
    for msg in messages {
        match msg.get("role").and_then(Value::as_str) {
            Some("user") => out.push(json!({"role": "user", "content": input_content(msg.get("content").unwrap_or(&Value::Null))?})),
            Some("toolResult") => {
                let call = msg.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                let call_id = match identities.get(call) {
                    Some(ids) => ids[0].clone(),
                    None => call_ids(&json!({"id": call}))?[0].clone(),
                };
                let content = input_content(msg.get("content").unwrap_or(&Value::Null))?;
                let output = if content.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("input_image")) {
                    Value::Array(content)
                } else {
                    let texts: Vec<&str> = content.iter().map(|b| b.get("text").and_then(Value::as_str).unwrap_or("")).collect();
                    Value::String(texts.join("\n"))
                };
                out.push(json!({"type": "function_call_output", "call_id": call_id, "output": output}));
            }
            Some("assistant") => {
                let same = msg.get("provider").and_then(Value::as_str) == Some(target.provider)
                    && msg.get("api").and_then(Value::as_str) == Some(target.api)
                    && msg.get("model").and_then(Value::as_str) == Some(target.model);
                for block in msg.get("content").and_then(Value::as_array).into_iter().flatten() {
                    match block.get("type").and_then(Value::as_str) {
                        Some("thinking") => {
                            let signature = nonempty(block.get("thinkingSignature"));
                            let thinking = nonempty(block.get("thinking"));
                            let redacted = block.get("redacted").is_some_and(|r| r.as_bool() == Some(true));
                            if let (true, Some(sig)) = (same, signature) {
                                let item = parse(sig)?;
                                if item.get("type").and_then(Value::as_str) != Some("reasoning") {
                                    return Err(fail("invalid_reasoning_replay"));
                                }
                                out.push(item);
                            } else if let (Some(text), false) = (thinking, redacted) {
                                out.push(json!({"role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]}));
                            }
                        }
                        Some("text") => {
                            let mut item = Map::new();
                            item.insert("type".into(), json!("message"));
                            item.insert("role".into(), json!("assistant"));
                            if same && let Some(sig) = nonempty(block.get("textSignature")) {
                                let meta = if sig.starts_with('{') { parse(sig)? } else { json!({"id": sig}) };
                                if let Some(id) = nonempty(meta.get("id")) {
                                    item.insert("id".into(), json!(id));
                                }
                                if let Some(phase) = nonempty(meta.get("phase")) {
                                    item.insert("phase".into(), json!(phase));
                                }
                            }
                            item.insert("status".into(), json!("completed"));
                            item.insert(
                                "content".into(),
                                json!([{"type": "output_text", "text": block.get("text").cloned().unwrap_or(Value::Null), "annotations": []}]),
                            );
                            out.push(Value::Object(item));
                        }
                        Some("toolCall") => {
                            let ids = call_ids(block)?;
                            let mut item = Map::new();
                            item.insert("type".into(), json!("function_call"));
                            item.insert("call_id".into(), json!(ids[0]));
                            if let Some(item_id) = ids.get(1).filter(|s| !s.is_empty()) {
                                item.insert("id".into(), json!(item_id));
                            }
                            item.insert("name".into(), block.get("name").cloned().unwrap_or(Value::Null));
                            let args = block.get("arguments").cloned().unwrap_or(Value::Null);
                            item.insert("arguments".into(), json!(js_stringify(&args)));
                            if let Some(ns) = block.get("namespace") {
                                item.insert("namespace".into(), ns.clone());
                            }
                            if block.get("async") == Some(&Value::Bool(true)) {
                                item.insert("async".into(), json!(true));
                            }
                            out.push(Value::Object(item));
                        }
                        _ => return Err(fail("unsupported_assistant_block")),
                    }
                }
            }
            _ => return Err(fail("unsupported_message_role")),
        }
    }
    Ok(out)
}

/// JSON.stringify-compatible serialization (serde_json escapes the same way for valid text).
fn js_stringify(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".into())
}
