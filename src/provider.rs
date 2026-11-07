//! One streamed request to an OpenAI Responses endpoint (Codex, Grok), over SSE.

use crate::responses::{self, Target};
use crate::store::Error;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read};

type Result<T> = std::result::Result<T, Error>;

/// Everything the harness decides about a request (via `prepared`).
pub struct Request<'a> {
    pub target: Target<'a>,
    pub base_url: &'a str,
    pub instructions: &'a str,
    pub reasoning: Option<&'a str>,
    pub tools: &'a [Value],
    pub session_id: &'a str,
    pub token: &'a str,
    pub account_id: Option<&'a str>,
}

/// Streaming output, reported as it arrives.
pub enum Delta<'a> {
    Text(&'a str),
    Reasoning(&'a str),
}

fn endpoint(api: &str, base: &str) -> String {
    let base = base.trim_end_matches('/');
    if api == "openai-codex-responses" {
        if base.ends_with("/codex/responses") {
            base.to_string()
        } else if base.ends_with("/codex") {
            format!("{base}/responses")
        } else {
            format!("{base}/codex/responses")
        }
    } else if base.ends_with("/responses") {
        base.to_string()
    } else {
        format!("{base}/responses")
    }
}

pub fn body(req: &Request, messages: &[Value]) -> Result<Value> {
    let input = responses::input(&req.target, messages)?;
    let mut body = json!({
        "model": req.target.model, "stream": true, "store": false, "input": input, "tools": req.tools,
    });
    if req.target.api == "openai-codex-responses" {
        body["instructions"] = json!(if req.instructions.is_empty() {
            "You are a helpful assistant."
        } else {
            req.instructions
        });
        body["text"] = json!({"verbosity": "low"});
        body["parallel_tool_calls"] = json!(true);
        body["include"] = json!(["reasoning.encrypted_content"]);
        body["tool_choice"] = json!("auto");
    }
    body["prompt_cache_key"] = json!(req.session_id);
    if let Some(effort) = req.reasoning {
        body["reasoning"] = if effort == "none" {
            json!({"effort": "none"})
        } else {
            json!({"effort": effort, "summary": "auto"})
        };
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    Ok(body)
}

/// Send one request and assemble the assistant message (pi message shape).
pub fn stream(req: &Request, messages: &[Value], on_delta: &mut dyn FnMut(Delta)) -> Result<Value> {
    let body = body(req, messages)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut call = agent
        .post(endpoint(req.target.api, req.base_url))
        .header("Authorization", format!("Bearer {}", req.token))
        .header("OpenAI-Beta", "responses=experimental")
        .header("accept", "text/event-stream")
        .header("content-type", "application/json")
        .header("originator", "pi")
        .header("session-id", req.session_id)
        .header("x-client-request-id", req.session_id);
    if let Some(account) = req.account_id {
        call = call.header("chatgpt-account-id", account);
    }
    let mut response = call
        .send(body.to_string())
        .map_err(|e| Error::Format(format!("provider_request_failed: {e}")))?;
    let status = response.status().as_u16();
    let reader = response.body_mut().as_reader();
    if status != 200 {
        let mut text = String::new();
        let _ = reader.take(4096).read_to_string(&mut text);
        return Err(Error::Format(format!(
            "provider_http_{status}: {}",
            text.chars().take(300).collect::<String>()
        )));
    }

    let mut message = json!({
        "role": "assistant", "content": [], "api": req.target.api, "provider": req.target.provider,
        "model": req.target.model, "stopReason": "stop", "timestamp": now_ms(),
        "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                  "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
    });
    let mut data = String::new();
    let mut finished = false;
    for line in BufReader::new(reader).lines() {
        let line = line.map_err(|e| Error::Format(format!("provider_stream_read: {e}")))?;
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim_start());
            continue;
        }
        if !line.is_empty() || data.is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(&std::mem::take(&mut data))
            .map_err(|_| Error::Format("invalid_provider_json".into()))?;
        if handle(&event, &mut message, on_delta)? {
            finished = true;
            break;
        }
    }
    if !finished {
        return Err(Error::Format("provider_stream_ended_early".into()));
    }
    Ok(message)
}

/// Apply one stream event; true once the response is complete.
fn handle(event: &Value, message: &mut Value, on_delta: &mut dyn FnMut(Delta)) -> Result<bool> {
    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "response.output_text.delta" => on_delta(Delta::Text(
            event.get("delta").and_then(Value::as_str).unwrap_or(""),
        )),
        "response.reasoning_summary_text.delta" => on_delta(Delta::Reasoning(
            event.get("delta").and_then(Value::as_str).unwrap_or(""),
        )),
        "response.output_item.done" => {
            if let Some(block) = event.get("item").and_then(block_from_item) {
                if block.get("type").and_then(Value::as_str) == Some("toolCall") {
                    message["stopReason"] = json!("toolUse");
                }
                if let Some(Value::Array(content)) = message.get_mut("content") {
                    content.push(block);
                }
            }
        }
        "response.completed" | "response.done" | "response.incomplete" => {
            if let Some(usage) = event.pointer("/response/usage") {
                let input = usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let output = usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let cached = usage
                    .pointer("/input_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                message["usage"]["input"] = json!(input.saturating_sub(cached));
                message["usage"]["output"] = json!(output);
                message["usage"]["cacheRead"] = json!(cached);
                message["usage"]["totalTokens"] = json!(input + output);
            }
            if kind == "response.incomplete" {
                message["stopReason"] = json!("length");
            }
            return Ok(true);
        }
        "response.failed" | "error" => {
            let detail = event
                .pointer("/response/error/message")
                .or_else(|| event.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            return Err(Error::Format(format!("provider_error: {detail}")));
        }
        _ => {}
    }
    Ok(false)
}

/// A finished output item as a pi content block.
fn block_from_item(item: &Value) -> Option<Value> {
    match item.get("type").and_then(Value::as_str)? {
        "reasoning" => {
            let summary: Vec<&str> = item
                .get("summary")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect()
                })
                .unwrap_or_default();
            Some(
                json!({"type": "thinking", "thinking": summary.join("\n\n"), "thinkingSignature": item.to_string()}),
            )
        }
        "message" => {
            let text: String = item
                .get("content")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect()
                })
                .unwrap_or_default();
            let mut signature =
                json!({"v": 1, "id": item.get("id").cloned().unwrap_or(Value::Null)});
            if let Some(phase) = item.get("phase").filter(|p| !p.is_null()) {
                signature["phase"] = phase.clone();
            }
            Some(json!({"type": "text", "text": text, "textSignature": signature.to_string()}))
        }
        "function_call" => {
            let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
            let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
            let arguments: Value = item
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|a| serde_json::from_str(a).ok())
                .unwrap_or_else(|| json!({}));
            Some(
                json!({"type": "toolCall", "id": format!("{call_id}|{item_id}"), "name": item.get("name").cloned().unwrap_or(Value::Null), "arguments": arguments}),
            )
        }
        _ => None,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
