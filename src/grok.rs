//! The Grok CLI proxy (`grok-cli`, cli-chat-proxy.grok.com): its client headers, its model
//! list, and the request changes it needs, as the pi-grok-cli add-on makes them
//! (`provider/stream.ts`, `models/catalog.ts`, `payload/sanitize.ts`).

use serde_json::{Map, Value, json};

pub const PROVIDER: &str = "grok-cli";
pub const BASE: &str = "https://cli-chat-proxy.grok.com/v1";
/// The Grok Build release the add-on identifies as (the proxy rejects clients it deems too old).
const VERSION: &str = "1.0.70";
const ENCRYPTED_REASONING: &str = "reasoning.encrypted_content";
/// Models whose reasoning effort can be set (the add-on's EFFORT_CAPABLE_PREFIXES).
const EFFORT_PREFIXES: [&str; 6] = [
    "grok-3-mini",
    "grok-4.20-multi-agent",
    "grok-4.3",
    "grok-4.5",
    "grok-4.6",
    "grok-4.7",
];

/// The identification headers the proxy requires, for one model.
pub fn headers(model_id: &str) -> Value {
    json!({"User-Agent": format!("grok-shell/{VERSION} (macos; aarch64)"), "x-grok-client-identifier": "grok-shell",
        "x-grok-client-version": VERSION, "x-xai-token-auth": "xai-grok-cli", "x-grok-model-override": model_id})
}

/// One entry of the proxy's `/models` list as a model description in pi's shape. The
/// largest context window it offers is used, as the add-on does (500k). No prices are given.
pub fn model(entry: &Value, base_url: &str) -> Option<Value> {
    let id = entry.get("id")?.as_str()?;
    let efforts: Vec<&str> = entry
        .get("reasoning_efforts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| e.get("value").and_then(Value::as_str))
        .collect();
    let reasoning = entry
        .get("supports_reasoning_effort")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || !efforts.is_empty();
    let window = entry
        .get("context_windows")
        .and_then(Value::as_array)
        .and_then(|w| w.iter().filter_map(Value::as_u64).max())
        .or_else(|| entry.get("context_window").and_then(Value::as_u64));
    let mut model = json!({"id": id, "name": entry.get("name").cloned().unwrap_or(json!(id)), "api": "openai-responses",
        "provider": PROVIDER, "baseUrl": base_url, "reasoning": reasoning, "input": ["text", "image"], "headers": headers(id)});
    if let Some(w) = window {
        model["contextWindow"] = json!(w);
    }
    if !efforts.is_empty() {
        let mut map = Map::new();
        map.insert("off".into(), Value::Null);
        for level in ["minimal", "low", "medium", "high", "xhigh", "max"] {
            map.insert(
                level.into(),
                if efforts.contains(&level) {
                    json!(level)
                } else {
                    Value::Null
                },
            );
        }
        model["thinkingLevelMap"] = Value::Object(map);
    }
    Some(model)
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p {
                Value::String(s) => Some(s.clone()),
                Value::Object(o)
                    if matches!(
                        o.get("type").and_then(Value::as_str),
                        Some("text" | "input_text" | "output_text")
                    ) =>
                {
                    o.get("text").and_then(Value::as_str).map(str::to_string)
                }
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn reasoning_content(content: Option<&Value>) -> Option<Value> {
    let parts: Vec<Value> = match content? {
        Value::String(s) if !s.is_empty() => vec![json!({"type": "reasoning_text", "text": s})],
        Value::Array(items) => items
            .iter()
            .filter_map(|p| match p {
                Value::String(s) if !s.is_empty() => {
                    Some(json!({"type": "reasoning_text", "text": s}))
                }
                Value::Object(o)
                    if o.get("type").and_then(Value::as_str) == Some("reasoning_text")
                        && o.get("text").is_some_and(Value::is_string) =>
                {
                    Some(p.clone())
                }
                Value::Object(o)
                    if o.get("type").is_none() && o.get("text").is_some_and(Value::is_string) =>
                {
                    let mut p = p.clone();
                    p["type"] = json!("reasoning_text");
                    Some(p)
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    (!parts.is_empty()).then_some(Value::Array(parts))
}

/// An image part as the proxy takes it: `input_image` with a data or web address.
fn image_part(part: &Value) -> Value {
    let Some(o) = part.as_object() else {
        return part.clone();
    };
    let mut o = o.clone();
    if o.get("type").and_then(Value::as_str) == Some("image")
        && let (Some(data), Some(mime)) = (
            o.get("data").and_then(Value::as_str),
            o.get("mimeType").and_then(Value::as_str),
        )
    {
        return json!({"type": "input_image", "image_url": format!("data:{mime};base64,{data}"), "detail": "auto"});
    }
    if matches!(
        o.get("type").and_then(Value::as_str),
        Some("image_url" | "input_image")
    ) {
        if let Some(Value::Object(url)) = o.get("image_url").cloned() {
            o.insert(
                "image_url".into(),
                url.get("url").cloned().unwrap_or(Value::Null),
            );
            if let Some(detail) = url.get("detail") {
                o.insert("detail".into(), detail.clone());
            }
        }
        o.insert("type".into(), json!("input_image"));
        if !o
            .get("detail")
            .and_then(Value::as_str)
            .is_some_and(|d| !d.is_empty())
        {
            o.insert("detail".into(), json!("auto"));
        }
    }
    for key in ["content", "output"] {
        if let Some(Value::Array(items)) = o.get(key).cloned() {
            o.insert(
                key.into(),
                Value::Array(items.iter().map(image_part).collect()),
            );
        }
    }
    Value::Object(o)
}

fn effort_supported(model: &Value) -> bool {
    let id = model.get("id").and_then(Value::as_str).unwrap_or("");
    let name = id.rsplit('/').next().unwrap_or(id).to_lowercase();
    if !EFFORT_PREFIXES.iter().any(|p| name.starts_with(p))
        || model.get("reasoning") != Some(&Value::Bool(true))
    {
        return false;
    }
    match model.get("thinkingLevelMap").and_then(Value::as_object) {
        None => true,
        Some(map) => map
            .values()
            .any(|v| v.as_str().is_some_and(|v| v != "none")),
    }
}

/// The add-on's sanitizePayload: what the proxy accepts in a Responses request.
pub fn sanitize(body: &mut Value, model: &Value, session_id: &str) {
    let Some(b) = body.as_object_mut() else {
        return;
    };
    if let Some(Value::Array(items)) = b.get("input").cloned() {
        let mut input = Vec::new();
        let mut instructions = Vec::new();
        for mut item in items {
            if item.get("type").and_then(Value::as_str) == Some("reasoning")
                && let Some(o) = item.as_object_mut()
            {
                o.remove("status");
                match reasoning_content(o.get("content")) {
                    Some(c) => o.insert("content".into(), c),
                    None => o.remove("content"),
                };
            }
            if item.get("content").and_then(Value::as_str) == Some("") {
                continue;
            }
            // The proxy refuses system and developer messages: they join the instructions.
            if matches!(
                item.get("role").and_then(Value::as_str),
                Some("developer" | "system")
            ) {
                let text = text_of(item.get("content").unwrap_or(&Value::Null));
                if !text.trim().is_empty() {
                    instructions.push(text.trim().to_string());
                }
                continue;
            }
            input.push(image_part(&item));
        }
        if !instructions.is_empty() {
            let existing = b
                .get("instructions")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let merged: Vec<String> = existing.into_iter().chain(instructions).collect();
            b.insert("instructions".into(), json!(merged.join("\n\n")));
        }
        // A tool result with images: the text stays the result, the images follow as a message.
        let mut rewritten = Vec::new();
        for item in input {
            match (item.get("type").and_then(Value::as_str), item.get("output")) {
                (Some("function_call_output"), Some(Value::Array(parts))) => {
                    let images: Vec<Value> = parts
                        .iter()
                        .filter(|p| p.get("type").and_then(Value::as_str) == Some("input_image"))
                        .cloned()
                        .collect();
                    let text: Vec<String> = parts
                        .iter()
                        .filter(|p| p.get("type").and_then(Value::as_str) != Some("input_image"))
                        .filter_map(|p| {
                            p.as_str().map(str::to_string).or_else(|| {
                                p.get("text").and_then(Value::as_str).map(str::to_string)
                            })
                        })
                        .collect();
                    let text = if text.join("\n").is_empty() {
                        "(tool returned no text output)".to_string()
                    } else {
                        text.join("\n")
                    };
                    let mut result = item.clone();
                    result["output"] = json!(text);
                    rewritten.push(result);
                    if !images.is_empty() {
                        let call = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .map(|c| format!(" ({c})"))
                            .unwrap_or_default();
                        let (n, s) = (images.len(), if images.len() == 1 { "" } else { "s" });
                        let label = format!(
                            "The previous tool result{call} included {n} image{s}. Use the attached image{s} as the visual output from that tool."
                        );
                        let mut content = vec![json!({"type": "input_text", "text": label})];
                        content.extend(images);
                        rewritten.push(json!({"role": "user", "content": content}));
                    }
                }
                _ => rewritten.push(item),
            }
        }
        b.insert("input".into(), Value::Array(rewritten));
    }
    if let Some(format) = b.remove("response_format")
        && !b.contains_key("text")
    {
        b.insert("text".into(), json!({"format": format}));
    }
    let reasons = model
        .get("reasoning")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    match b.get("reasoning").and_then(Value::as_object).cloned() {
        Some(mut r) if reasons => {
            if effort_supported(model) {
                if r.get("effort").and_then(Value::as_str) == Some("minimal") {
                    r.insert("effort".into(), json!("low"));
                }
            } else {
                r.remove("effort");
            }
            if r.is_empty() {
                b.remove("reasoning");
            } else {
                b.insert("reasoning".into(), Value::Object(r));
            }
        }
        _ => {
            b.remove("reasoning");
        }
    }
    let has_reasoning = b.contains_key("reasoning");
    let mut include: Vec<Value> = b
        .get("include")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut kept = false;
    include.retain(|i| {
        if i != ENCRYPTED_REASONING {
            return true;
        }
        let keep = reasons && !kept;
        kept |= keep;
        keep
    });
    if has_reasoning && !kept {
        include.push(json!(ENCRYPTED_REASONING));
    }
    if include.is_empty() {
        b.remove("include");
    } else {
        b.insert("include".into(), Value::Array(include));
    }
    b.remove("prompt_cache_retention");
    if !b
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty())
    {
        b.remove("tool_choice");
    }
    if !session_id.is_empty() && !b.contains_key("prompt_cache_key") {
        b.insert("prompt_cache_key".into(), json!(session_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_reshaped_for_the_proxy() {
        let model = json!({"id": "grok-4.6", "reasoning": true});
        let mut body = json!({"model": "grok-4.6", "input": [
            {"role": "developer", "content": "Be brief."},
            {"type": "reasoning", "id": "r1", "status": "completed", "content": "thought"},
            {"role": "user", "content": ""},
            {"type": "function_call_output", "call_id": "c1", "output": [{"type": "input_text", "text": "shot"}, {"type": "input_image", "image_url": "data:image/png;base64,AA"}]}],
            "reasoning": {"effort": "minimal", "summary": "auto"}, "tools": [], "tool_choice": "auto"});
        sanitize(&mut body, &model, "s1");
        assert_eq!(body["instructions"], "Be brief.");
        assert_eq!(
            body["input"][0],
            json!({"type": "reasoning", "id": "r1", "content": [{"type": "reasoning_text", "text": "thought"}]})
        );
        assert_eq!(
            body["input"][1],
            json!({"type": "function_call_output", "call_id": "c1", "output": "shot"})
        );
        assert_eq!(
            body["input"][2]["content"][1],
            json!({"type": "input_image", "image_url": "data:image/png;base64,AA", "detail": "auto"})
        );
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert!(body.get("tool_choice").is_none());
        assert_eq!(body["prompt_cache_key"], "s1");
    }

    #[test]
    fn a_listed_model_becomes_a_model() {
        let entry = json!({"id": "grok-4.5", "name": "Grok 4.5", "context_window": 256000, "context_windows": [256000, 500000],
            "supports_reasoning_effort": true, "reasoning_efforts": [{"value": "high"}, {"value": "medium"}, {"value": "low"}]});
        let m = model(&entry, BASE).unwrap();
        assert_eq!(m["contextWindow"], 500000);
        assert_eq!(
            m["thinkingLevelMap"],
            json!({"off": null, "minimal": null, "low": "low", "medium": "medium", "high": "high", "xhigh": null, "max": null})
        );
        assert_eq!(m["headers"]["x-grok-model-override"], "grok-4.5");
    }
}
