//! The OpenAI Chat Completions API (`openai-completions`) for OpenRouter and other
//! OpenAI-compatible endpoints.
//!
//! Covered: compatibility detection and `model.compat` overrides, message conversion
//! (including replay of another model's history: its thinking is dropped and long tool
//! call ids are shortened), tools, the OpenRouter and OpenAI reasoning settings, Anthropic
//! cache markers for OpenRouter's anthropic/ models, OpenRouter session affinity and billed
//! cost, and the streamed reply. Not covered (unused here): grammar tools and the
//! zai/qwen/chat-template/baseten/together/deepseek reasoning formats, which send no
//! reasoning setting.

use crate::provider::{Failure, Piece, provider_error};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

pub const API: &str = "openai-completions";

/// Resolved compatibility settings: detected from the provider and
/// endpoint, then overridden by `model.compat`.
#[derive(Debug, Clone)]
pub struct Compat {
    pub supports_store: bool,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_usage_in_streaming: bool,
    pub supports_finish_reason: bool,
    pub max_tokens_field: String,
    pub requires_tool_result_name: bool,
    pub requires_assistant_after_tool_result: bool,
    pub requires_thinking_as_text: bool,
    pub requires_reasoning_content: bool,
    pub thinking_format: String,
    pub supports_strict_mode: bool,
    pub anthropic_cache_control: bool,
    pub send_session_affinity: bool,
    pub affinity_format: String,
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

pub fn compat(model: &Value) -> Compat {
    let provider = text(model, "provider");
    let base = text(model, "baseUrl");
    let id = text(model, "id");
    let zai = provider == "zai"
        || provider == "zai-coding-cn"
        || base.contains("api.z.ai")
        || base.contains("open.bigmodel.cn");
    let together = provider == "together"
        || base.contains("api.together.ai")
        || base.contains("api.together.xyz");
    let moonshot =
        provider == "moonshotai" || provider == "moonshotai-cn" || base.contains("api.moonshot.");
    let openrouter = provider == "openrouter" || base.contains("openrouter.ai");
    let cf_workers = provider == "cloudflare-workers-ai" || base.contains("api.cloudflare.com");
    let cf_gateway =
        provider == "cloudflare-ai-gateway" || base.contains("gateway.ai.cloudflare.com");
    let nvidia = provider == "nvidia" || base.contains("integrate.api.nvidia.com");
    let ant_ling = provider == "ant-ling" || base.contains("api.ant-ling.com");
    let cerebras = provider == "cerebras" || base.contains("cerebras.ai");
    let deepseek = provider == "deepseek" || base.to_lowercase().contains("deepseek.com");
    let grok = provider == "xai" || base.contains("api.x.ai");
    let non_standard = nvidia
        || cerebras
        || grok
        || together
        || base.contains("chutes.ai")
        || deepseek
        || zai
        || moonshot
        || provider == "opencode"
        || base.contains("opencode.ai")
        || cf_workers
        || cf_gateway
        || ant_ling;
    let max_tokens = base.contains("chutes.ai")
        || deepseek
        || moonshot
        || cf_gateway
        || together
        || nvidia
        || ant_ling
        || zai;
    let developer_model = openrouter && (id.starts_with("anthropic/") || id.starts_with("openai/"));
    let mut c = Compat {
        supports_store: !non_standard,
        supports_developer_role: developer_model || (!non_standard && !openrouter),
        supports_reasoning_effort: !grok
            && !zai
            && !moonshot
            && !together
            && !cf_gateway
            && !nvidia
            && !ant_ling,
        supports_usage_in_streaming: true,
        supports_finish_reason: true,
        max_tokens_field: if max_tokens {
            "max_tokens"
        } else {
            "max_completion_tokens"
        }
        .into(),
        requires_tool_result_name: false,
        requires_assistant_after_tool_result: false,
        requires_thinking_as_text: false,
        requires_reasoning_content: deepseek,
        thinking_format: if deepseek {
            "deepseek"
        } else if zai {
            "zai"
        } else if together {
            "together"
        } else if ant_ling {
            "ant-ling"
        } else if openrouter {
            "openrouter"
        } else {
            "openai"
        }
        .into(),
        supports_strict_mode: false,
        anthropic_cache_control: provider == "openrouter" && id.starts_with("anthropic/"),
        send_session_affinity: openrouter,
        affinity_format: if openrouter { "openrouter" } else { "openai" }.into(),
    };
    let Some(o) = model.get("compat").filter(|c| c.is_object()) else {
        return c;
    };
    let flag = |key: &str, slot: &mut bool| {
        if let Some(b) = o.get(key).and_then(Value::as_bool) {
            *slot = b;
        }
    };
    flag("supportsStore", &mut c.supports_store);
    flag("supportsDeveloperRole", &mut c.supports_developer_role);
    flag("supportsReasoningEffort", &mut c.supports_reasoning_effort);
    flag(
        "supportsUsageInStreaming",
        &mut c.supports_usage_in_streaming,
    );
    flag("supportsFinishReason", &mut c.supports_finish_reason);
    flag("requiresToolResultName", &mut c.requires_tool_result_name);
    flag(
        "requiresAssistantAfterToolResult",
        &mut c.requires_assistant_after_tool_result,
    );
    flag("requiresThinkingAsText", &mut c.requires_thinking_as_text);
    flag(
        "requiresReasoningContentOnAssistantMessages",
        &mut c.requires_reasoning_content,
    );
    flag("supportsStrictMode", &mut c.supports_strict_mode);
    flag("sendSessionAffinityHeaders", &mut c.send_session_affinity);
    let word = |key: &str, slot: &mut String| {
        if let Some(s) = o.get(key).and_then(Value::as_str) {
            *slot = s.to_string();
        }
    };
    word("maxTokensField", &mut c.max_tokens_field);
    word("thinkingFormat", &mut c.thinking_format);
    word("sessionAffinityFormat", &mut c.affinity_format);
    if let Some(format) = o.get("cacheControlFormat").and_then(Value::as_str) {
        c.anthropic_cache_control = format == "anthropic";
    }
    c
}

fn same_model(message: &Value, model: &Value) -> bool {
    ["provider", "api"]
        .iter()
        .all(|k| message.get(*k) == model.get(*k))
        && message.get("model") == model.get("id")
}

fn accepts_images(model: &Value) -> bool {
    match model.get("input").and_then(Value::as_array) {
        Some(kinds) => kinds.iter().any(|k| k == "image"),
        None => true,
    }
}

fn replace_images(content: &[Value], placeholder: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut previous = false;
    for block in content {
        if block.get("type").and_then(Value::as_str) == Some("image") {
            if !previous {
                out.push(json!({"type": "text", "text": placeholder}));
            }
            previous = true;
            continue;
        }
        previous = block.get("text").and_then(Value::as_str) == Some(placeholder);
        out.push(block.clone());
    }
    out
}

/// A Pi-compatible 53-bit cyrb53-style hash in base 36, for shortening tool call ids.
fn short_hash(s: &str) -> String {
    let (mut h1, mut h2): (u32, u32) = (0xdeadbeef, 0x41c6ce57);
    for ch in s.encode_utf16() {
        h1 = (h1 ^ ch as u32).wrapping_mul(2654435761);
        h2 = (h2 ^ ch as u32).wrapping_mul(1597334677);
    }
    let a = (h1 ^ (h1 >> 16)).wrapping_mul(2246822507) ^ (h2 ^ (h2 >> 13)).wrapping_mul(3266489909);
    h1 = a;
    let b = (h2 ^ (h2 >> 16)).wrapping_mul(2246822507) ^ (h1 ^ (h1 >> 13)).wrapping_mul(3266489909);
    h2 = b;
    format!("{}{}", base36(h2), base36(h1))
}

fn base36(mut n: u32) -> String {
    if n == 0 {
        return "0".into();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while n > 0 {
        out.push(digits[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

fn clean(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Normalize Chat Completions tool call ids: Responses ids `call|item` become
/// `call_item`, shortened with a hash past 40 characters.
fn normalize_id(id: &str, model: &Value) -> String {
    if let Some((call, item)) = id.split_once('|') {
        let (call, item) = (clean(call), clean(item));
        let combined = if item.is_empty() {
            call.clone()
        } else {
            format!("{call}_{item}")
        };
        if combined.encode_utf16().count() <= 40 {
            return combined;
        }
        let hash: String = short_hash(id).chars().take(8).collect();
        let keep = 40usize.saturating_sub(hash.len() + 1).max(1);
        let prefix: String = call.chars().take(keep).collect();
        return format!("{prefix}_{hash}");
    }
    if model.get("provider").and_then(Value::as_str) == Some("openai") && id.chars().count() > 40 {
        return id.chars().take(40).collect();
    }
    id.to_string()
}

/// Transform messages: images a model cannot take become a note; another model's
/// thinking is dropped and its tool call ids normalized; failed replies are left out; a call
/// without a result gets a placeholder. A late result becomes a user message at
/// its actual position: Chat Completions requires tool messages next to their call.
pub fn transform(messages: &[Value], model: &Value) -> Vec<Value> {
    let images = accepts_images(model);
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut changed = Vec::new();
    for message in messages {
        let mut m = message.clone();
        if m.get("content").is_none_or(Value::is_null) {
            m["content"] = json!([]);
        }
        let role = text(&m, "role");
        if !images
            && (role == "user" || role == "toolResult")
            && let Some(content) = m["content"].as_array().cloned()
        {
            let note = if role == "user" {
                "(image omitted: model does not support images)"
            } else {
                "(tool image omitted: model does not support images)"
            };
            m["content"] = Value::Array(replace_images(&content, note));
        }
        if role == "toolResult" {
            if let Some(new) = ids.get(&text(&m, "toolCallId")) {
                m["toolCallId"] = json!(new);
            }
        } else if role == "assistant" {
            let same = same_model(&m, model);
            let mut content = Vec::new();
            for block in m["content"].as_array().cloned().unwrap_or_default() {
                match block.get("type").and_then(Value::as_str) {
                    Some("thinking") => {
                        if !same {
                            continue;
                        }
                        let signed = block.get("redacted") == Some(&Value::Bool(true))
                            || block
                                .get("thinkingSignature")
                                .and_then(Value::as_str)
                                .is_some_and(|s| !s.is_empty());
                        if signed || !text(&block, "thinking").trim().is_empty() {
                            content.push(block);
                        }
                    }
                    Some("text") if !same => {
                        content.push(json!({"type": "text", "text": block.get("text")}))
                    }
                    Some("toolCall") if !same => {
                        let mut call = block.clone();
                        if let Some(o) = call.as_object_mut() {
                            o.remove("thoughtSignature");
                        }
                        let id = text(&block, "id");
                        let new = normalize_id(&id, model);
                        if new != id {
                            ids.insert(id, new.clone());
                            call["id"] = json!(new);
                        }
                        content.push(call);
                    }
                    _ => content.push(block),
                }
            }
            m["content"] = Value::Array(content);
        }
        changed.push(m);
    }
    let mut out = Vec::new();
    let mut pending: Vec<Value> = Vec::new();
    let mut answered: HashSet<String> = HashSet::new();
    let mut held: Vec<Value> = Vec::new();
    let close = |out: &mut Vec<Value>,
                 pending: &mut Vec<Value>,
                 answered: &mut HashSet<String>,
                 held: &mut Vec<Value>| {
        for call in pending.drain(..) {
            if !answered.contains(&text(&call, "id")) {
                out.push(json!({"role": "toolResult", "toolCallId": call.get("id"), "toolName": call.get("name"),
                    "content": [{"type": "text", "text": if call.get("async") == Some(&Value::Bool(true)) { "Tool result is pending." } else { "No result provided" }}], "isError": true, "timestamp": crate::provider::now_ms()}));
            }
        }
        answered.clear();
        out.append(held);
    };
    for m in changed {
        match text(&m, "role").as_str() {
            "assistant" => {
                close(&mut out, &mut pending, &mut answered, &mut held);
                if matches!(text(&m, "stopReason").as_str(), "error" | "aborted") {
                    continue;
                }
                let calls: Vec<Value> = m["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("toolCall"))
                    .cloned()
                    .collect();
                if !calls.is_empty() {
                    pending = calls;
                    answered.clear();
                }
                out.push(m);
            }
            "toolResult" => {
                let id = text(&m, "toolCallId");
                if pending.iter().any(|call| text(call, "id") == id) && !answered.contains(&id) {
                    answered.insert(id);
                    out.push(m);
                } else {
                    // The call was already closed before this result arrived. Chat
                    // cannot accept an orphan tool message; retain chronological order.
                    close(&mut out, &mut pending, &mut answered, &mut held);
                    let mut content = vec![json!({"type": "text", "text": format!(
                        "[Core-delivered late tool result{} for {} (call {}); not user-authored]:", if m.get("isError") == Some(&Value::Bool(true)) { " (error)" } else { "" }, text(&m, "toolName"), id)})];
                    match m.get("content") {
                        Some(Value::Array(blocks)) => content.extend(blocks.clone()),
                        Some(Value::String(value)) => {
                            content.push(json!({"type": "text", "text": value}))
                        }
                        _ => {}
                    }
                    out.push(json!({"role": "user", "content": content, "timestamp": m.get("timestamp")}));
                }
            }
            "system" if !pending.is_empty() => held.push(m),
            "user" => {
                close(&mut out, &mut pending, &mut answered, &mut held);
                out.push(m);
            }
            _ => out.push(m),
        }
    }
    close(&mut out, &mut pending, &mut answered, &mut held);
    out
}

fn reasoning_detail(d: &Value) -> bool {
    let Some(o) = d.as_object() else { return false };
    let common = o.get("id").is_none_or(|v| v.is_null() || v.is_string())
        && o.get("format").is_none_or(Value::is_string)
        && o.get("index").is_none_or(Value::is_number);
    common
        && match o.get("type").and_then(Value::as_str) {
            Some("reasoning.summary") => o.get("summary").is_some_and(Value::is_string),
            Some("reasoning.encrypted") => o.get("data").is_some_and(Value::is_string),
            Some("reasoning.text") => {
                o.get("text").is_some_and(Value::is_string)
                    && o.get("signature")
                        .is_none_or(|v| v.is_null() || v.is_string())
            }
            _ => false,
        }
}

fn parse_details(signature: Option<&str>) -> Option<Vec<Value>> {
    let parsed: Value = serde_json::from_str(signature?).ok()?;
    let list = parsed.as_array()?;
    (!list.is_empty() && list.iter().all(reasoning_detail)).then(|| list.clone())
}

fn legacy_detail(signature: Option<&str>) -> Option<Value> {
    let parsed: Value = serde_json::from_str(signature?).ok()?;
    let ok = reasoning_detail(&parsed)
        && parsed["type"] == "reasoning.encrypted"
        && parsed
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        && parsed
            .get("data")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
    ok.then_some(parsed)
}

fn image_part(block: &Value) -> Value {
    json!({"type": "image_url", "image_url": {"url": format!("data:{};base64,{}", text(block, "mimeType"), text(block, "data"))}})
}

/// Convert messages: the system prompt first, then the conversation.
pub fn messages(model: &Value, c: &Compat, instructions: &str, context: &[Value]) -> Vec<Value> {
    let reasoning = model.get("reasoning") == Some(&Value::Bool(true));
    let role = if reasoning && c.supports_developer_role {
        "developer"
    } else {
        "system"
    };
    let mut out = Vec::new();
    if !instructions.is_empty() {
        out.push(json!({"role": role, "content": instructions}));
    }
    let messages = transform(context, model);
    let mut last_role = String::new();
    let mut i = 0;
    while i < messages.len() {
        let m = &messages[i];
        let kind = text(m, "role");
        if c.requires_assistant_after_tool_result && last_role == "toolResult" && kind == "user" {
            out.push(json!({"role": "assistant", "content": "I have processed the tool results."}));
        }
        match kind.as_str() {
            "user" => {
                if let Some(s) = m["content"].as_str() {
                    out.push(json!({"role": "user", "content": s}));
                } else {
                    let parts: Vec<Value> = m["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|b| b["type"] != "text" || !text(b, "text").is_empty())
                        .map(|b| {
                            if b["type"] == "text" {
                                json!({"type": "text", "text": b["text"]})
                            } else {
                                image_part(b)
                            }
                        })
                        .collect();
                    if parts.is_empty() {
                        i += 1;
                        continue;
                    }
                    out.push(json!({"role": "user", "content": parts}));
                }
            }
            "assistant" => {
                let blocks = m["content"].as_array().cloned().unwrap_or_default();
                let of = |t: &str| {
                    blocks
                        .iter()
                        .filter(|b| b["type"] == t)
                        .cloned()
                        .collect::<Vec<_>>()
                };
                let texts: Vec<String> = of("text")
                    .iter()
                    .map(|b| text(b, "text"))
                    .filter(|t| !t.trim().is_empty())
                    .collect();
                let joined = texts.concat();
                let thinking = of("thinking");
                let calls = of("toolCall");
                let mut msg = Map::new();
                msg.insert("role".into(), json!("assistant"));
                msg.insert(
                    "content".into(),
                    if c.requires_assistant_after_tool_result {
                        json!("")
                    } else {
                        Value::Null
                    },
                );
                let signed = thinking.iter().find_map(|b| {
                    parse_details(b.get("thinkingSignature").and_then(Value::as_str))
                });
                let legacy: Vec<Value> = calls
                    .iter()
                    .filter_map(|b| {
                        legacy_detail(b.get("thoughtSignature").and_then(Value::as_str))
                    })
                    .collect();
                let details = signed.or(if legacy.is_empty() {
                    None
                } else {
                    Some(legacy)
                });
                let written: Vec<&Value> = thinking
                    .iter()
                    .filter(|b| !text(b, "thinking").trim().is_empty())
                    .collect();
                if !written.is_empty() {
                    if c.requires_thinking_as_text {
                        let mut parts = vec![
                            json!({"type": "text", "text": written.iter().map(|b| text(b, "thinking")).collect::<Vec<_>>().join("\n\n")}),
                        ];
                        parts.extend(texts.iter().map(|t| json!({"type": "text", "text": t})));
                        msg.insert("content".into(), Value::Array(parts));
                    } else {
                        if !joined.is_empty() {
                            msg.insert("content".into(), json!(joined));
                        }
                        if details.is_none() {
                            let signature = text(written[0], "thinkingSignature");
                            if ["reasoning", "reasoning_content", "reasoning_text"]
                                .contains(&signature.as_str())
                            {
                                let all: Vec<String> =
                                    written.iter().map(|b| text(b, "thinking")).collect();
                                msg.insert(signature, json!(all.join("\n")));
                            }
                        }
                    }
                } else if !joined.is_empty() {
                    msg.insert("content".into(), json!(joined));
                }
                if !calls.is_empty() {
                    let list: Vec<Value> = calls
                        .iter()
                        .map(|b| json!({"id": b["id"], "type": "function", "function": {"name": b["name"],
                            "arguments": crate::responses::js_stringify(b.get("arguments").unwrap_or(&json!({})))}}))
                        .collect();
                    msg.insert("tool_calls".into(), Value::Array(list));
                }
                if let Some(details) = details {
                    msg.insert("reasoning_details".into(), Value::Array(details));
                }
                if c.requires_reasoning_content
                    && reasoning
                    && !msg.contains_key("reasoning_content")
                {
                    msg.insert("reasoning_content".into(), json!(""));
                }
                let has_content = match msg.get("content") {
                    Some(Value::String(s)) => !s.is_empty(),
                    Some(Value::Array(a)) => !a.is_empty(),
                    _ => false,
                };
                if has_content || msg.contains_key("tool_calls") {
                    out.push(Value::Object(msg));
                }
            }
            "toolResult" => {
                let mut images = Vec::new();
                let mut j = i;
                while j < messages.len() && messages[j]["role"] == "toolResult" {
                    let r = &messages[j];
                    let content = r["content"].as_array().cloned().unwrap_or_default();
                    let body: Vec<String> = content
                        .iter()
                        .filter(|b| b["type"] == "text")
                        .map(|b| text(b, "text"))
                        .collect();
                    let body = body.join("\n");
                    let has_images = content.iter().any(|b| b["type"] == "image");
                    let shown = if !body.trim().is_empty() {
                        body
                    } else if has_images {
                        "(see attached image)".into()
                    } else {
                        "(no tool output)".into()
                    };
                    let mut msg =
                        json!({"role": "tool", "content": shown, "tool_call_id": r["toolCallId"]});
                    if c.requires_tool_result_name
                        && r.get("toolName").is_some_and(|n| n.is_string())
                    {
                        msg["name"] = r["toolName"].clone();
                    }
                    out.push(msg);
                    if has_images && accepts_images(model) {
                        images.extend(
                            content
                                .iter()
                                .filter(|b| b["type"] == "image")
                                .map(image_part),
                        );
                    }
                    j += 1;
                }
                i = j;
                if images.is_empty() {
                    last_role = "toolResult".into();
                } else {
                    if c.requires_assistant_after_tool_result {
                        out.push(json!({"role": "assistant", "content": "I have processed the tool results."}));
                    }
                    let mut parts = vec![
                        json!({"type": "text", "text": "Attached image(s) from tool result:"}),
                    ];
                    parts.extend(images);
                    out.push(json!({"role": "user", "content": parts}));
                    last_role = "user".into();
                }
                continue;
            }
            _ => {}
        }
        last_role = kind;
        i += 1;
    }
    out
}

/// A Responses-shaped tool definition as a Chat Completions tool.
fn tool(definition: &Value, c: &Compat) -> Value {
    let mut function = json!({"name": definition["name"], "description": definition["description"], "parameters": definition["parameters"]});
    if c.supports_strict_mode {
        function["strict"] = json!(
            definition
                .get("strict")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        );
    }
    json!({"type": "function", "function": function})
}

/// Estimate context tokens at 3.5 characters per token for the output limit.
fn estimate(messages: &[Value]) -> u64 {
    let len = |s: &str| s.encode_utf16().count();
    let content_chars = |content: &Value| -> usize {
        match content {
            Value::String(s) => len(s),
            Value::Array(blocks) => blocks
                .iter()
                .map(|b| {
                    if b["type"] == "text" {
                        len(b["text"].as_str().unwrap_or(""))
                    } else {
                        4800
                    }
                })
                .sum(),
            _ => 0,
        }
    };
    let tokens_of = |m: &Value| -> u64 {
        let chars = match m["role"].as_str() {
            Some("user") | Some("toolResult") => content_chars(&m["content"]),
            _ => m["content"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|b| match b["type"].as_str() {
                    Some("text") => len(b["text"].as_str().unwrap_or("")),
                    Some("thinking") => len(b["thinking"].as_str().unwrap_or("")),
                    _ => {
                        len(b["name"].as_str().unwrap_or(""))
                            + len(&crate::responses::js_stringify(&b["arguments"]))
                    }
                })
                .sum(),
        };
        (chars as f64 / 3.5).ceil() as u64
    };
    let mut latest = f64::NEG_INFINITY;
    let mut last: Option<(u64, usize)> = None;
    for (i, m) in messages.iter().enumerate() {
        let time = m.get("timestamp").and_then(Value::as_f64).unwrap_or(0.0);
        if m["role"] == "assistant"
            && time >= latest
            && !matches!(m["stopReason"].as_str(), Some("aborted" | "error"))
            && let Some(t) = m.get("usage").and_then(crate::budget::context_tokens)
        {
            last = Some((t, i));
        }
        latest = latest.max(time);
    }
    match last {
        Some((t, i)) => t + messages[i + 1..].iter().map(tokens_of).sum::<u64>(),
        None => messages.iter().map(tokens_of).sum(),
    }
}

/// Build parameters for a streamed request.
pub fn body(
    model: &Value,
    instructions: &str,
    context: &[Value],
    tools: &[Value],
    session_id: &str,
    reasoning: &crate::models::Reasoning,
) -> Value {
    let c = compat(model);
    let mut messages = messages(model, &c, instructions, context);
    let mut body = Map::new();
    body.insert("model".into(), model["id"].clone());
    body.insert("stream".into(), json!(true));
    if text(model, "baseUrl").contains("api.openai.com") {
        let key: String = session_id.chars().take(64).collect();
        body.insert("prompt_cache_key".into(), json!(key));
    }
    if c.supports_usage_in_streaming {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if c.supports_store {
        body.insert("store".into(), json!(false));
    }
    if let Some(max) = model.get("maxTokens").and_then(Value::as_u64) {
        let window = model
            .get("contextWindow")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let limit = if window <= 0 {
            max.max(1)
        } else {
            let available = window - estimate(context) as i64 - 4096;
            max.min(available.max(1) as u64)
        };
        if limit > 0 {
            body.insert(c.max_tokens_field.clone(), json!(limit));
        }
    }
    let had_tools = context.iter().any(|m| {
        m["role"] == "toolResult"
            || (m["role"] == "assistant"
                && m["content"]
                    .as_array()
                    .is_some_and(|b| b.iter().any(|b| b["type"] == "toolCall")))
    });
    let mut converted: Vec<Value> = tools.iter().map(|t| tool(t, &c)).collect();
    if c.anthropic_cache_control {
        mark_cache(&mut messages, &mut converted);
    }
    body.insert("messages".into(), Value::Array(messages));
    if !converted.is_empty() {
        body.insert("tools".into(), Value::Array(converted));
    } else if had_tools {
        body.insert("tools".into(), json!([]));
    }
    let model_reasoning = model.get("reasoning") == Some(&Value::Bool(true));
    let off = reasoning.level == "off";
    let map = model.get("thinkingLevelMap");
    match c.thinking_format.as_str() {
        "openrouter" if model_reasoning => {
            if let Some(effort) = &reasoning.effort {
                body.insert("reasoning".into(), json!({"effort": effort}));
            }
        }
        "openai" if model_reasoning && c.supports_reasoning_effort => {
            if !off {
                body.insert(
                    "reasoning_effort".into(),
                    json!(reasoning.effort.clone().unwrap_or(reasoning.level.clone())),
                );
            } else if let Some(Value::String(v)) = map.and_then(|m| m.get("off")) {
                body.insert("reasoning_effort".into(), json!(v));
            }
        }
        _ => {}
    }
    if let Some(routing) = model
        .pointer("/compat/openRouterRouting")
        .filter(|r| r.is_object())
    {
        body.insert("provider".into(), routing.clone());
    }
    if let Some(Value::Object(extra)) = model.get("samplingParams") {
        for (k, v) in extra {
            body.insert(k.clone(), v.clone());
        }
    }
    // Keep `messages` second to preserve request field order.
    let mut ordered = Map::new();
    for key in ["model", "messages"] {
        if let Some(v) = body.remove(key) {
            ordered.insert(key.into(), v);
        }
    }
    ordered.extend(body);
    Value::Object(ordered)
}

/// Anthropic cache markers on the system prompt, the last tool and the last message's text.
fn mark_cache(messages: &mut [Value], tools: &mut [Value]) {
    let marker = json!({"type": "ephemeral"});
    let mark = |m: &mut Value| -> bool {
        match m.get_mut("content") {
            Some(Value::String(s)) if !s.is_empty() => {
                let t = s.clone();
                m["content"] = json!([{"type": "text", "text": t, "cache_control": marker}]);
                true
            }
            Some(Value::Array(parts)) => match parts.iter_mut().rev().find(|p| p["type"] == "text")
            {
                Some(p) => {
                    p["cache_control"] = marker.clone();
                    true
                }
                None => false,
            },
            _ => false,
        }
    };
    if let Some(m) = messages
        .iter_mut()
        .find(|m| m["role"] == "system" || m["role"] == "developer")
    {
        mark(m);
    }
    if let Some(t) = tools.last_mut() {
        t["cache_control"] = marker.clone();
    }
    for m in messages.iter_mut().rev() {
        if matches!(m["role"].as_str(), Some("user" | "assistant" | "tool")) && mark(m) {
            break;
        }
    }
}

/// Request headers: authorization, then session affinity, then the
/// credentials' and model's headers.
pub fn headers(model: &Value, token: &str, session_id: &str) -> Vec<(String, String)> {
    let c = compat(model);
    let mut h = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".into(), "application/json".into()),
    ];
    if !token.is_empty() {
        h.push(("authorization".into(), format!("Bearer {token}")));
    }
    if c.send_session_affinity && !session_id.is_empty() {
        if c.affinity_format == "openrouter" {
            h.push(("x-session-id".into(), session_id.into()));
        } else {
            if c.affinity_format == "openai" {
                h.push(("session_id".into(), session_id.into()));
            }
            h.push(("x-client-request-id".into(), session_id.into()));
            h.push(("x-session-affinity".into(), session_id.into()));
        }
    }
    h
}

pub fn endpoint(base: &str) -> String {
    format!("{}/chat/completions", base.trim_end_matches('/'))
}

enum Block {
    Text(String),
    Thinking {
        text: String,
        signature: String,
    },
    Call {
        id: String,
        name: String,
        arguments: String,
    },
}

/// One streamed reply, chunk by chunk.
#[derive(Default)]
pub struct Reply {
    blocks: Vec<Block>,
    text: Option<usize>,
    thinking: Option<usize>,
    by_index: HashMap<i64, usize>,
    by_id: HashMap<String, usize>,
    details: Option<Vec<Value>>,
    usage: Option<Value>,
    finish: Option<String>,
}

fn usage_from(raw: &Value) -> Value {
    let n = |p: &str| raw.pointer(p).and_then(Value::as_u64);
    let prompt = n("/prompt_tokens").unwrap_or(0);
    let read = n("/prompt_tokens_details/cached_tokens")
        .or(n("/prompt_cache_hit_tokens"))
        .or(n("/cached_tokens"))
        .unwrap_or(0);
    let write = n("/prompt_tokens_details/cache_write_tokens").unwrap_or(0);
    let input = prompt.saturating_sub(read).saturating_sub(write);
    let output = n("/completion_tokens").unwrap_or(0);
    let mut usage = json!({"input": input, "output": output, "cacheRead": read, "cacheWrite": write,
        "reasoning": n("/completion_tokens_details/reasoning_tokens").unwrap_or(0),
        "totalTokens": input + output + read + write,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}});
    // OpenRouter's billed cost, applied after the rates (see models::Pricing).
    let reported: Map<String, Value> = ["cost", "is_byok", "cost_details"]
        .iter()
        .filter_map(|k| raw.get(*k).map(|v| (k.to_string(), v.clone())))
        .collect();
    if !reported.is_empty() {
        usage["_openrouter"] = Value::Object(reported);
    }
    usage
}

impl Reply {
    fn thinking_block(&mut self, signature: &str) -> usize {
        if let Some(i) = self.thinking {
            return i;
        }
        self.blocks.push(Block::Thinking {
            text: String::new(),
            signature: signature.into(),
        });
        self.thinking = Some(self.blocks.len() - 1);
        self.blocks.len() - 1
    }

    /// One chunk. An `error` in the chunk is the provider failing mid-reply.
    pub fn chunk(&mut self, chunk: &Value, report: &impl Fn(Piece)) -> Result<(), Failure> {
        if let Some(error) = chunk.get("error") {
            let raw = json!({"type": "error", "error": error});
            report(Piece::ProviderError(provider_error(
                &raw,
                error.get("code"),
            )));
            return Err(Failure::Fatal("provider_error".into()));
        }
        if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(usage_from(u));
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return Ok(());
        };
        if chunk.get("usage").is_none_or(Value::is_null)
            && let Some(u) = choice.get("usage").filter(|u| u.is_object())
        {
            self.usage = Some(usage_from(u));
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(reason.to_string());
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(());
        };
        if let Some((field, piece)) = ["reasoning_content", "reasoning", "reasoning_text"]
            .iter()
            .find_map(|f| {
                delta
                    .get(*f)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(|s| (*f, s.to_string()))
            })
        {
            let i = self.thinking_block(field);
            if let Block::Thinking { text, .. } = &mut self.blocks[i] {
                text.push_str(&piece);
            }
            report(Piece::Reasoning(piece));
        }
        for detail in delta
            .get("reasoning_details")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if !reasoning_detail(detail) {
                continue;
            }
            self.thinking_block("");
            let details = self.details.get_or_insert_with(Vec::new);
            append_detail(details, detail);
        }
        if let Some(piece) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let i = match self.text {
                Some(i) => i,
                None => {
                    self.blocks.push(Block::Text(String::new()));
                    self.text = Some(self.blocks.len() - 1);
                    self.blocks.len() - 1
                }
            };
            if let Block::Text(t) = &mut self.blocks[i] {
                t.push_str(piece);
            }
            report(Piece::Text(piece.to_string(), None));
        }
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let index = call.get("index").and_then(Value::as_i64);
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let found = index
                .and_then(|i| self.by_index.get(&i).copied())
                .or_else(|| id.and_then(|id| self.by_id.get(id).copied()));
            let at = match found {
                Some(at) => at,
                None => {
                    self.blocks.push(Block::Call {
                        id: id.unwrap_or("").into(),
                        name: name.into(),
                        arguments: String::new(),
                    });
                    self.blocks.len() - 1
                }
            };
            if let Some(i) = index {
                self.by_index.entry(i).or_insert(at);
            }
            if let Some(id) = id {
                self.by_id.insert(id.into(), at);
            }
            if let Block::Call {
                id: have,
                name: named,
                arguments,
            } = &mut self.blocks[at]
            {
                if have.is_empty()
                    && let Some(id) = id
                {
                    *have = id.into();
                }
                if named.is_empty() && !name.is_empty() {
                    *named = name.into();
                }
                if let Some(piece) = call.pointer("/function/arguments").and_then(Value::as_str) {
                    arguments.push_str(piece);
                }
            }
        }
        Ok(())
    }

    /// The end of the stream: the finished blocks, then the reply's end.
    pub fn finish(
        self,
        report: &impl Fn(Piece),
        supports_finish_reason: bool,
    ) -> Result<(), Failure> {
        let details = self
            .details
            .map(|d| crate::responses::js_stringify(&Value::Array(d)));
        for block in self.blocks {
            report(Piece::Block(match block {
                Block::Text(t) => json!({"type": "text", "text": t}),
                Block::Thinking { text, signature } => {
                    json!({"type": "thinking", "thinking": text, "thinkingSignature": details.clone().unwrap_or(signature)})
                }
                Block::Call {
                    id,
                    name,
                    arguments,
                } => {
                    let mut call =
                        json!({"type": "toolCall", "id": id, "name": name, "arguments": {}});
                    if !arguments.trim().is_empty() {
                        match serde_json::from_str::<Value>(&arguments) {
                            Ok(v) if v.is_object() => call["arguments"] = v,
                            Ok(_) => {
                                call["argumentsError"] = json!(format!(
                                    "Tool arguments must be a JSON object\n\nReceived arguments:\n{arguments}"
                                ))
                            }
                            Err(e) => {
                                call["argumentsError"] =
                                    json!(format!("{e}\n\nReceived arguments:\n{arguments}"))
                            }
                        }
                    }
                    call
                }
            }));
        }
        let incomplete = match self.finish.as_deref() {
            Some("stop" | "end" | "tool_calls" | "function_call") => false,
            Some("length") => true,
            Some(other) => {
                let code: String = other
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .take(40)
                    .collect();
                return Err(Failure::Fatal(format!("provider_finish_{code}")));
            }
            None if supports_finish_reason => {
                return Err(Failure::Retryable("provider_stream_incomplete".into()));
            }
            None => false,
        };
        let usage = self.usage.unwrap_or_else(crate::provider::zero_usage);
        report(Piece::Done {
            usage,
            incomplete,
            service_tier: None,
        });
        Ok(())
    }
}

fn append_detail(details: &mut Vec<Value>, detail: &Value) {
    let fill = |last: &mut Value| {
        if last.get("id").is_none_or(Value::is_null) && detail.get("id").is_some() {
            last["id"] = detail["id"].clone();
        }
        if last
            .get("format")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
            && detail.get("format").is_some()
        {
            last["format"] = detail["format"].clone();
        }
        if last.get("index").is_none_or(Value::is_null) && detail.get("index").is_some() {
            last["index"] = detail["index"].clone();
        }
    };
    if let Some(last) = details.last_mut() {
        if detail["type"] == "reasoning.text" && last["type"] == "reasoning.text" {
            let joined = format!("{}{}", text(last, "text"), text(detail, "text"));
            last["text"] = json!(joined);
            if last
                .get("signature")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
                && detail.get("signature").is_some()
            {
                last["signature"] = detail["signature"].clone();
            }
            fill(last);
            return;
        }
        if detail["type"] == "reasoning.summary" && last["type"] == "reasoning.summary" {
            let joined = format!("{}{}", text(last, "summary"), text(detail, "summary"));
            last["summary"] = json!(joined);
            fill(last);
            return;
        }
    }
    details.push(detail.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_hash_matches_pi() {
        // Pi-compatible hash examples.
        assert_eq!(
            normalize_id("call_abc|fc_123", &json!({})),
            "call_abc_fc_123"
        );
        let long = format!("call_{}|fc_{}", "x".repeat(30), "y".repeat(60));
        let id = normalize_id(&long, &json!({}));
        assert_eq!(id.len(), 40);
        assert!(id.starts_with("call_xxx"));
    }

    #[test]
    fn another_models_history_is_replayed_without_its_thinking() {
        let model = json!({"provider": "openrouter", "api": API, "id": "z", "input": ["text"]});
        let codex = json!({"role": "assistant", "provider": "openai-codex", "api": "openai-codex-responses", "model": "gpt",
            "stopReason": "toolUse", "content": [{"type": "thinking", "thinking": "hmm", "thinkingSignature": "{}"},
            {"type": "text", "text": "ok", "textSignature": "x"}, {"type": "toolCall", "id": "c1|fc_1", "name": "ls", "arguments": {"a": 1}}]});
        let ctx = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "image", "mimeType": "image/png", "data": "AA"}]}),
            codex,
        ];
        let c = compat(&model);
        let out = messages(&model, &c, "sys", &ctx);
        assert_eq!(
            out,
            vec![
                json!({"role": "system", "content": "sys"}),
                json!({"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "text", "text": "(image omitted: model does not support images)"}]}),
                json!({"role": "assistant", "content": "ok", "tool_calls": [{"id": "c1_fc_1", "type": "function", "function": {"name": "ls", "arguments": "{\"a\":1}"}}]}),
                json!({"role": "tool", "content": "No result provided", "tool_call_id": "c1_fc_1"}),
            ]
        );
    }

    #[test]
    fn a_streamed_reply_becomes_blocks() {
        let pieces = std::cell::RefCell::new(Vec::new());
        let report = |p: Piece| pieces.borrow_mut().push(format!("{p:?}"));
        let mut r = Reply::default();
        for chunk in [
            json!({"choices": [{"delta": {"reasoning": "think"}}]}),
            json!({"choices": [{"delta": {"content": "Hi"}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "t1", "function": {"name": "ls", "arguments": "{\"pa"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "th\":1}"}}]}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 3, "prompt_tokens_details": {"cached_tokens": 4}, "cost": 0.5, "is_byok": false}}),
        ] {
            r.chunk(&chunk, &report).unwrap();
        }
        r.finish(&report, true).unwrap();
        let all = pieces.borrow().join("\n");
        assert!(all.contains(r#""thinking": String("think")"#) || all.contains("think"));
        assert!(
            all.contains("\"path\": Number(1)") || all.contains("path"),
            "{all}"
        );
        assert!(all.contains("_openrouter"));
    }
}
