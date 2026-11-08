//! One streamed request to an OpenAI Responses endpoint (Codex, Grok), over SSE.
//! The stream runs on its own thread and reports each piece as it arrives, so the
//! turn loop can save and start a tool call as soon as it is complete.

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
    pub reasoning: &'a crate::models::Reasoning,
    /// `service_tier`, e.g. "priority" for Fast mode.
    pub service_tier: Option<&'a str>,
    pub tools: &'a [Value],
    pub session_id: &'a str,
    pub token: &'a str,
    pub account_id: Option<&'a str>,
    /// Outside Codex the instructions are the first input message, as "developer" for a
    /// reasoning model that accepts that role and as "system" otherwise (as pi sends them).
    pub developer_role: bool,
}

/// pi's instruction role rule: "developer" when the model reasons and its
/// `compat.supportsDeveloperRole` is not false.
pub fn developer_role(model: &Value, reasoning: &crate::models::Reasoning) -> bool {
    reasoning.include && model.pointer("/compat/supportsDeveloperRole") != Some(&Value::Bool(false))
}

/// What the stream thread reports.
#[derive(Debug)]
pub enum Piece {
    Text(String),
    Reasoning(String),
    /// A finished output item (reasoning, text or a tool call) as a content block.
    Block(Value),
    Done {
        usage: Value,
        incomplete: bool,
    },
    Failed(Failure),
    /// The response's status and headers, as soon as they arrive (pi's after_provider_response).
    Response(Value),
}

#[derive(Debug)]
pub enum Failure {
    /// Connection trouble or a temporary server error: the request may be retried.
    Retryable(String),
    Fatal(String),
}

/// A request ready to send from another thread.
#[derive(Clone)]
pub struct Prepared {
    url: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Prepared {
    /// The request as it will be sent, for the runner to review (pi's before_provider_request
    /// and before_provider_headers).
    pub fn view(&self) -> Value {
        let headers: serde_json::Map<String, Value> = self
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let body: Value = serde_json::from_str(&self.body).unwrap_or(Value::Null);
        json!({"url": self.url, "headers": headers, "body": body})
    }

    /// Replace the body and/or headers with the runner's reviewed versions.
    pub fn replace(&mut self, body: Option<&Value>, headers: Option<&Value>) -> Result<()> {
        if let Some(body) = body {
            if body.get("stream") != Some(&Value::Bool(true))
                || !body.get("input").is_some_and(Value::is_array)
            {
                return Err(Error::Format(
                    "a reviewed request must keep stream: true and an input array".into(),
                ));
            }
            self.body = body.to_string();
        }
        if let Some(headers) = headers {
            let Value::Object(map) = headers else {
                return Err(Error::Format("reviewed headers must be an object".into()));
            };
            self.headers = map
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect();
        }
        Ok(())
    }
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
    let mut input = responses::input(&req.target, messages)?;
    if req.target.api != "openai-codex-responses" && !req.instructions.is_empty() {
        let role = if req.developer_role {
            "developer"
        } else {
            "system"
        };
        input.insert(0, json!({"role": role, "content": req.instructions}));
    }
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
    if let Some(tier) = req.service_tier {
        body["service_tier"] = json!(tier);
    }
    if let Some(effort) = &req.reasoning.effort {
        body["reasoning"] = if effort == "none" {
            json!({"effort": "none"})
        } else {
            json!({"effort": effort, "summary": "auto"})
        };
    }
    if req.reasoning.include {
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    Ok(body)
}

pub fn prepare(req: &Request, messages: &[Value]) -> Result<Prepared> {
    let mut headers: Vec<(String, String)> = [
        ("Authorization", format!("Bearer {}", req.token)),
        ("OpenAI-Beta", "responses=experimental".to_string()),
        ("accept", "text/event-stream".to_string()),
        ("content-type", "application/json".to_string()),
        ("originator", "pi".to_string()),
        ("session-id", req.session_id.to_string()),
        ("x-client-request-id", req.session_id.to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    if let Some(account) = req.account_id {
        headers.push(("chatgpt-account-id".to_string(), account.to_string()));
    }
    Ok(Prepared {
        url: endpoint(req.target.api, req.base_url),
        headers,
        body: body(req, messages)?.to_string(),
    })
}

/// Send the request on a new thread; every piece goes to `report`, ending with
/// exactly one `Done` or `Failed`.
pub fn spawn(prepared: Prepared, report: impl Fn(Piece) + Send + 'static) {
    std::thread::spawn(move || {
        if let Err(failure) = run(&prepared, &report) {
            report(Piece::Failed(failure));
        }
    });
}

fn run(p: &Prepared, report: &impl Fn(Piece)) -> std::result::Result<(), Failure> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut call = agent.post(&p.url);
    for (name, value) in &p.headers {
        call = call.header(name.as_str(), value);
    }
    let mut response = call
        .send(p.body.as_str())
        .map_err(|e| Failure::Retryable(format!("provider_request_failed: {e}")))?;
    let status = response.status().as_u16();
    let headers: serde_json::Map<String, Value> = response
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_string(), Value::String(v.to_string())))
        })
        .collect();
    report(Piece::Response(
        json!({"status": status, "headers": headers}),
    ));
    let reader = response.body_mut().as_reader();
    if status != 200 {
        let mut text = String::new();
        let _ = reader.take(4096).read_to_string(&mut text);
        let detail = format!(
            "provider_http_{status}: {}",
            text.chars().take(300).collect::<String>()
        );
        return Err(if status == 429 || status >= 500 {
            Failure::Retryable(detail)
        } else {
            Failure::Fatal(detail)
        });
    }
    let mut data = String::new();
    let mut seen = std::collections::HashSet::new();
    for line in BufReader::new(reader).lines() {
        let line = line.map_err(|e| Failure::Retryable(format!("provider_stream_read: {e}")))?;
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim_start());
            continue;
        }
        if !line.is_empty() || data.is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(&std::mem::take(&mut data))
            .map_err(|_| Failure::Fatal("invalid_provider_json".into()))?;
        if handle(&event, report, &mut seen)? {
            return Ok(());
        }
    }
    Err(Failure::Retryable("provider_stream_ended_early".into()))
}

/// Report one stream event; true once the response is complete. `seen` holds the ids of
/// items already reported, so items listed only in the final message are reported too.
fn handle(
    event: &Value,
    report: &impl Fn(Piece),
    seen: &mut std::collections::HashSet<String>,
) -> std::result::Result<bool, Failure> {
    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
    let text = |key: &str| {
        event
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match kind {
        "response.output_text.delta" => report(Piece::Text(text("delta"))),
        "response.reasoning_summary_text.delta" => report(Piece::Reasoning(text("delta"))),
        "response.output_item.done" => {
            if let Some(item) = event.get("item") {
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    seen.insert(id.to_string());
                }
                if let Some(block) = block_from_item(item) {
                    report(Piece::Block(block));
                }
            }
        }
        "response.completed" | "response.done" | "response.incomplete" => {
            // An item the stream never finished separately is taken from the final list,
            // in order, as the harness's native provider does.
            for item in event
                .pointer("/response/output")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                if id.is_empty()
                    || seen.contains(id)
                    || !["function_call", "reasoning", "message"].contains(&kind)
                {
                    continue;
                }
                if item.get("status").is_some_and(|s| s != "completed") {
                    return Err(Failure::Fatal("incomplete_terminal_item".into()));
                }
                seen.insert(id.to_string());
                if let Some(block) = block_from_item(item) {
                    report(Piece::Block(block));
                }
            }
            report(Piece::Done {
                usage: usage_from(event.pointer("/response/usage")),
                incomplete: kind == "response.incomplete",
            });
            return Ok(true);
        }
        "response.failed" | "error" => {
            let detail = event
                .pointer("/response/error/message")
                .or_else(|| event.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            return Err(Failure::Fatal(format!("provider_error: {detail}")));
        }
        _ => {}
    }
    Ok(false)
}

fn usage_from(usage: Option<&Value>) -> Value {
    let get = |path: &str| {
        usage
            .and_then(|u| u.pointer(path))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let (input, output, cached) = (
        get("/input_tokens"),
        get("/output_tokens"),
        get("/input_tokens_details/cached_tokens"),
    );
    json!({"input": input.saturating_sub(cached), "output": output, "cacheRead": cached, "cacheWrite": 0,
           "totalTokens": input + output, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}})
}

/// Usage for a reply the provider never finished.
pub fn zero_usage() -> Value {
    usage_from(None)
}

/// A new assistant message in pi's shape, before any content arrives.
pub fn empty_reply(target: &Target) -> Value {
    json!({
        "role": "assistant", "content": [], "api": target.api, "provider": target.provider,
        "model": target.model, "stopReason": "stop", "timestamp": now_ms(),
        "usage": usage_from(None),
    })
}

/// A finished output item as a pi content block. Async calls keep their marker.
pub fn block_from_item(item: &Value) -> Option<Value> {
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
            let mut block = json!({"type": "toolCall", "id": format!("{call_id}|{item_id}"),
                "name": item.get("name").cloned().unwrap_or(Value::Null), "arguments": arguments});
            if item.get("async") == Some(&Value::Bool(true)) {
                block["async"] = json!(true);
            }
            Some(block)
        }
        _ => None,
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
