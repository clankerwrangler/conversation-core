//! One streamed request to an OpenAI Responses endpoint (Codex, Grok), over SSE.
//! The stream runs on its own thread and reports each piece as it arrives, so the
//! turn loop can save and start a tool call as soon as it is complete.

use crate::completions;
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
    /// Headers from the credentials, then from the model object (pi's `model.headers`, e.g.
    /// grok-cli's client identification), merged over the defaults; a null value removes one.
    pub extra_headers: Vec<&'a Value>,
    /// The runner's model object (pi's shape): compat, input kinds and limits for Chat
    /// Completions.
    pub model: &'a Value,
}

/// The ChatGPT account id inside a Codex access token (its JWT claims), as the harness reads it.
pub(crate) fn codex_account(token: &str) -> Option<String> {
    use base64::Engine;
    let claims = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(claims.trim_end_matches('='))
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    let id = claims
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")?
        .as_str()?;
    (!id.is_empty()).then(|| id.to_string())
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
        /// The service tier the provider reports it used, for pricing.
        service_tier: Option<String>,
    },
    Failed(Failure),
    /// The response's status and headers, as soon as they arrive (pi's after_provider_response).
    Response(Value),
    /// The provider's own error event, reduced to fixed tokens, just before the failure.
    ProviderError(Value),
}

/// Error codes the harness recognises, with their category (native-provider.mjs).
const PROVIDER_ERROR_CODES: &[(&str, &str)] = &[
    ("context_length_exceeded", "context"),
    ("invalid_request_error", "request"),
    ("invalid_value", "request"),
    ("missing_required_parameter", "request"),
    ("unsupported_parameter", "request"),
    ("unknown_parameter", "request"),
    ("invalid_api_key", "authentication"),
    ("authentication_error", "authentication"),
    ("permission_denied", "permission"),
    ("model_not_found", "not_found"),
    ("rate_limit_exceeded", "rate_limit"),
    ("insufficient_quota", "quota"),
    ("server_error", "server"),
    ("internal_error", "server"),
    ("service_unavailable", "server"),
    ("previous_response_not_found", "transport"),
    ("websocket_connection_limit_reached", "transport"),
    ("invalid_prompt", "request"),
    ("data_residency_mismatch", "permission"),
    ("bio_policy", "content_policy"),
    ("misalignment_policy_violation", "content_policy"),
    ("vector_store_timeout", "server"),
    ("invalid_image", "request"),
    ("invalid_image_format", "request"),
    ("invalid_base64_image", "request"),
    ("invalid_image_url", "request"),
    ("image_too_large", "request"),
    ("image_too_small", "request"),
    ("image_parse_error", "request"),
    ("image_content_policy_violation", "content_policy"),
    ("invalid_image_mode", "request"),
    ("image_file_too_large", "request"),
    ("unsupported_image_media_type", "request"),
    ("empty_image_file", "request"),
    ("failed_to_download_image", "request"),
    ("image_file_not_found", "request"),
];
const PROVIDER_ERROR_TYPES: &[(&str, &str)] = &[
    ("invalid_request_error", "request"),
    ("authentication_error", "authentication"),
    ("permission_error", "permission"),
    ("not_found_error", "not_found"),
    ("rate_limit_error", "rate_limit"),
    ("server_error", "server"),
    ("api_error", "server"),
];

fn lookup(table: &[(&'static str, &'static str)], key: Option<&Value>) -> Option<&'static str> {
    let key = key?.as_str()?;
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

fn state(value: Option<&Value>, known: bool) -> &'static str {
    match value {
        None | Some(Value::Null) => "absent",
        _ if known => "known",
        Some(Value::String(_)) => "unknown",
        _ => "invalid",
    }
}

fn shape(value: Option<&Value>) -> &'static str {
    match value {
        None => "missing",
        Some(Value::Null) => "null",
        Some(Value::String(_)) => "string",
        Some(Value::Object(_)) => "object",
        _ => "other",
    }
}

/// The harness's safeProviderError: what the provider said went wrong, as fixed tokens
/// only (never its text, ids or parameters).
pub fn provider_error(raw: &Value, code: Option<&Value>) -> Value {
    let kind = raw.get("type").and_then(Value::as_str).unwrap_or("");
    let error = if kind == "response.failed" {
        raw.pointer("/response/error")
    } else {
        raw.get("error")
    };
    let error_type = error.and_then(|e| e.get("type"));
    let message = if kind == "error" && raw.get("message").is_some() {
        raw.get("message")
    } else if error.is_some_and(Value::is_string) {
        error
    } else {
        error.and_then(|e| e.get("message"))
    };
    let known_code = lookup(PROVIDER_ERROR_CODES, code);
    let known_type = lookup(PROVIDER_ERROR_TYPES, error_type);
    json!({"version": 2,
        "code": if known_code.is_some() { code.and_then(Value::as_str).unwrap_or("unknown") } else { "unknown" },
        "category": known_code.or(known_type).unwrap_or("unknown"),
        "event": kind,
        "codeState": state(code, known_code.is_some()),
        "typeState": state(error_type, known_type.is_some()),
        "errorShape": shape(error),
        "messageShape": shape(message)})
}

#[derive(Debug)]
pub enum Failure {
    /// Connection trouble or a temporary server error: the request may be retried.
    Retryable(String),
    Fatal(String),
}

/// The provider refused to switch reasoning off for this model (OpenRouter's "Reasoning is
/// mandatory"): the core leaves the setting out, remembers it, and sends again.
pub const REASONING_MANDATORY: &str = "reasoning_mandatory";

/// The provider's error says reasoning cannot be switched off for this model: OpenRouter's
/// "Reasoning is mandatory", or OpenAI's unsupported `reasoning.effort` value 'none'.
fn refuses_reasoning_off(error: &str) -> bool {
    error.contains("Reasoning is mandatory")
        || (error.contains("reasoning.effort") && error.contains("'none' is not supported"))
}

/// How a request travels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Transport {
    /// One HTTP request per reply, the reply streamed as server-sent events.
    Sse,
    /// The harness's default for Codex: one WebSocket kept open between requests, with
    /// SSE when it cannot connect. Every request carries the whole context (no
    /// previous_response_id), so the context usage line stays once per request.
    Auto,
}

/// A request ready to send from another thread.
#[derive(Clone)]
pub struct Prepared {
    url: String,
    headers: Vec<(String, String)>,
    body: String,
    pub transport: Transport,
    /// A Chat Completions request: Some(whether the reply must end with a finish_reason).
    chat: Option<bool>,
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

    /// Leave one top-level field out of the body.
    pub fn without(&mut self, field: &str) {
        if let Ok(Value::Object(mut body)) = serde_json::from_str::<Value>(&self.body) {
            body.remove(field);
            self.body = Value::Object(body).to_string();
        }
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
    // The Grok CLI proxy takes the request only in the add-on's shape.
    if req.model.get("provider").and_then(Value::as_str) == Some(crate::grok::PROVIDER) {
        crate::grok::sanitize(&mut body, req.model, req.session_id);
    }
    Ok(body)
}

pub fn prepare(req: &Request, messages: &[Value]) -> Result<Prepared> {
    if req.target.api == completions::API {
        let mut headers = completions::headers(req.model, req.token, req.session_id);
        merge_headers(&mut headers, &req.extra_headers);
        let body = completions::body(
            req.model,
            req.instructions,
            messages,
            req.tools,
            req.session_id,
            req.reasoning,
        );
        return Ok(Prepared {
            url: completions::endpoint(req.base_url),
            headers,
            body: body.to_string(),
            transport: Transport::Sse,
            chat: Some(completions::compat(req.model).supports_finish_reason),
        });
    }
    // The harness's headers (native-provider.mjs), lower-case, later groups winning.
    let codex = req.target.api == "openai-codex-responses";
    let mut headers: Vec<(String, String)> = vec![
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "text/event-stream".into()),
    ];
    if !req.token.is_empty() {
        headers.push(("authorization".into(), format!("Bearer {}", req.token)));
    }
    if codex {
        let account = req
            .account_id
            .map(str::to_string)
            .or_else(|| codex_account(req.token));
        let Some(account) = account else {
            return Err(Error::Format("missing_codex_account_id".into()));
        };
        headers.push(("chatgpt-account-id".into(), account));
        headers.push(("originator".into(), "pi".into()));
    }
    headers.push((
        if codex { "session-id" } else { "session_id" }.into(),
        req.session_id.to_string(),
    ));
    merge_headers(&mut headers, &req.extra_headers);
    if req.model.get("provider").and_then(Value::as_str) == Some(crate::grok::PROVIDER) {
        // The add-on's before_provider_headers: the conversation the request belongs to.
        headers.retain(|(k, _)| k != "x-grok-conv-id");
        headers.push(("x-grok-conv-id".into(), req.session_id.to_string()));
    }
    Ok(Prepared {
        url: endpoint(req.target.api, req.base_url),
        headers,
        body: body(req, messages)?.to_string(),
        transport: if req.target.api == "openai-codex-responses" {
            Transport::Auto
        } else {
            Transport::Sse
        },
        chat: None,
    })
}

/// Header groups over the defaults, lower-case, later winning; a null removes one.
fn merge_headers(headers: &mut Vec<(String, String)>, groups: &[&Value]) {
    for group in groups {
        let Value::Object(group) = group else {
            continue;
        };
        for (key, value) in group {
            let key = key.to_ascii_lowercase();
            headers.retain(|(k, _)| *k != key);
            if let Some(value) = value.as_str() {
                headers.push((key, value.to_string()));
            }
        }
    }
}

/// Send the request on a new thread; every piece goes to `report`, ending with
/// exactly one `Done` or `Failed`.
pub fn spawn(prepared: Prepared, report: impl Fn(Piece) + Send + 'static) {
    std::thread::spawn(move || {
        let result = match prepared.transport {
            Transport::Sse => run(&prepared, &report),
            Transport::Auto => websocket::run(&prepared, &report),
        };
        if let Err(failure) = result {
            report(Piece::Failed(failure));
        }
    });
}

/// The WebSocket transport, as the harness's native-provider.mjs runs it: one socket per
/// session kept open between requests (closed after 5 idle minutes or 55 minutes of age,
/// or when anything goes wrong), the same headers as HTTP, each request sent as one
/// `response.create` text frame, and the reply's events arriving as text frames.
mod websocket {
    use super::{Failure, Piece, Prepared, handle};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::net::{TcpStream, ToSocketAddrs};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use tungstenite::client::IntoClientRequest;
    use tungstenite::protocol::WebSocketConfig;
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::{Message, WebSocket};

    const IDLE: Duration = Duration::from_secs(300);
    const MAX_AGE: Duration = Duration::from_secs(3300);
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
    const MAX_FRAME: usize = 16 * 1024 * 1024;
    /// Codes that mean "this socket cannot take the request": try once more on a new one.
    const REJECTED: [&str; 2] = [
        "previous_response_not_found",
        "websocket_connection_limit_reached",
    ];

    type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

    struct Open {
        socket: Socket,
        identity: String,
        created: Instant,
        last_used: Instant,
    }

    /// The session's socket between requests. A request takes it and puts it back only
    /// after a complete reply, so a socket is never shared or reused after trouble.
    static IDLE_SOCKET: Mutex<Option<Open>> = Mutex::new(None);

    /// Same URL and headers (including the token): the same socket may be reused.
    fn identity(p: &Prepared) -> String {
        let mut hash = Sha256::new();
        hash.update(p.url.as_bytes());
        for (k, v) in &p.headers {
            hash.update([0]);
            hash.update(k.as_bytes());
            hash.update([1]);
            hash.update(v.as_bytes());
        }
        format!("{:x}", hash.finalize())
    }

    fn connect(p: &Prepared) -> Result<(Socket, Value), String> {
        let url = p
            .url
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| "websocket_bad_url")?;
        for (k, v) in &p.headers {
            let (Ok(name), Ok(value)) = (
                tungstenite::http::HeaderName::from_bytes(k.as_bytes()),
                tungstenite::http::HeaderValue::from_str(v),
            ) else {
                continue;
            };
            request.headers_mut().insert(name, value);
        }
        let uri = request.uri();
        let host = uri.host().ok_or("websocket_bad_url")?.to_string();
        let port = uri
            .port_u16()
            .unwrap_or(if uri.scheme_str() == Some("wss") {
                443
            } else {
                80
            });
        let address = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|_| "websocket_connect_failed")?
            .next()
            .ok_or("websocket_connect_failed")?;
        let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
            .map_err(|_| "websocket_connect_failed")?;
        let _ = stream.set_read_timeout(Some(CONNECT_TIMEOUT));
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME))
            .max_frame_size(Some(MAX_FRAME));
        let (socket, response) =
            tungstenite::client_tls_with_config(request, stream, Some(config), None)
                .map_err(|_| "websocket_connect_failed")?;
        if let MaybeTlsStream::Plain(s) = socket.get_ref() {
            let _ = s.set_read_timeout(None);
        }
        if let MaybeTlsStream::Rustls(s) = socket.get_ref() {
            let _ = s.get_ref().set_read_timeout(None);
        }
        let headers: serde_json::Map<String, Value> = response
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|v| (k.as_str().to_string(), Value::String(v.to_string())))
            })
            .collect();
        Ok((
            socket,
            json!({"status": response.status().as_u16(), "headers": headers, "transport": "websocket"}),
        ))
    }

    /// A kept socket that may be used for this request, or None.
    fn take_idle(identity: &str) -> Option<Open> {
        let open = IDLE_SOCKET.lock().ok()?.take()?;
        let fresh = open.identity == identity
            && open.last_used.elapsed() < IDLE
            && open.created.elapsed() < MAX_AGE;
        if fresh { Some(open) } else { None }
    }

    enum Outcome {
        Done,
        /// Nothing arrived from this socket before it failed or rejected the request.
        Unused(Failure),
        Failed(Failure),
    }

    fn exchange(socket: &mut Socket, frame: &str, report: &impl Fn(Piece)) -> Outcome {
        if socket.send(Message::text(frame)).is_err() {
            return Outcome::Unused(Failure::Retryable("websocket_closed".into()));
        }
        let mut seen = std::collections::HashSet::new();
        let mut any = false;
        loop {
            let message = match socket.read() {
                Ok(m) => m,
                Err(tungstenite::Error::Capacity(_)) => {
                    return Outcome::Failed(Failure::Fatal("provider_frame_limit".into()));
                }
                Err(_) if !any => {
                    return Outcome::Unused(Failure::Retryable("websocket_closed".into()));
                }
                Err(_) => return Outcome::Failed(Failure::Retryable("websocket_closed".into())),
            };
            let text = match message {
                Message::Text(t) => t,
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                Message::Close(_) if !any => {
                    return Outcome::Unused(Failure::Retryable("websocket_closed".into()));
                }
                Message::Close(_) => {
                    return Outcome::Failed(Failure::Retryable("websocket_closed".into()));
                }
                Message::Binary(_) => {
                    return Outcome::Failed(Failure::Fatal("unsupported_websocket_frame".into()));
                }
            };
            let Ok(event) = serde_json::from_str::<Value>(text.as_str()) else {
                return Outcome::Failed(Failure::Fatal("invalid_provider_json".into()));
            };
            if !any && event.get("type").and_then(Value::as_str) == Some("error") {
                let code = event
                    .get("code")
                    .or_else(|| event.pointer("/error/code"))
                    .and_then(Value::as_str);
                if code.is_some_and(|c| REJECTED.contains(&c)) {
                    return Outcome::Unused(Failure::Retryable(
                        code.unwrap_or_default().to_string(),
                    ));
                }
            }
            any = true;
            match handle(&event, report, &mut seen) {
                Ok(true) => return Outcome::Done,
                Ok(false) => {}
                Err(failure) => return Outcome::Failed(failure),
            }
        }
    }

    pub(super) fn run(p: &Prepared, report: &impl Fn(Piece)) -> Result<(), Failure> {
        let identity = identity(p);
        let frame = match p.body.strip_prefix('{') {
            Some(rest) if rest.trim_start() != "}" => {
                format!("{{\"type\":\"response.create\",{rest}")
            }
            _ => return Err(Failure::Fatal("invalid_stream_payload".into())),
        };
        let mut reused = take_idle(&identity);
        // A kept socket may have gone stale: one fresh try after it, as the harness does
        // for a rejected request.
        for _ in 0..2 {
            let (mut open, was_kept) = match reused.take() {
                Some(open) => (open, true),
                None => match connect(p) {
                    Ok((socket, response)) => {
                        report(Piece::Response(response));
                        (
                            Open {
                                socket,
                                identity: identity.clone(),
                                created: Instant::now(),
                                last_used: Instant::now(),
                            },
                            false,
                        )
                    }
                    // Cannot open a socket: the request goes over plain HTTP instead.
                    Err(_) => return super::run(p, report),
                },
            };
            match exchange(&mut open.socket, &frame, report) {
                Outcome::Done => {
                    open.last_used = Instant::now();
                    if let Ok(mut slot) = IDLE_SOCKET.lock() {
                        *slot = Some(open);
                    }
                    return Ok(());
                }
                Outcome::Unused(failure) => {
                    let _ = open.socket.close(None);
                    if !was_kept
                        && !REJECTED
                            .iter()
                            .any(|c| matches!(&failure, Failure::Retryable(r) if r == c))
                    {
                        return Err(failure);
                    }
                }
                Outcome::Failed(failure) => {
                    let _ = open.socket.close(None);
                    return Err(failure);
                }
            }
        }
        Err(Failure::Retryable("websocket_closed".into()))
    }
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
        .map_err(|_| Failure::Retryable("provider_request_failed".into()))?;
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
        // Only the status is reported: an error body can echo the request. It is read only
        // to recognise a refusal the core can correct by itself.
        let mut body = String::new();
        let _ = reader.take(64 * 1024).read_to_string(&mut body);
        if status == 400 && refuses_reasoning_off(&body) {
            return Err(Failure::Retryable(REASONING_MANDATORY.into()));
        }
        let detail = format!("provider_http_{status}");
        return Err(if status == 429 || status >= 500 {
            Failure::Retryable(detail)
        } else {
            Failure::Fatal(detail)
        });
    }
    let mut data = String::new();
    let mut seen = std::collections::HashSet::new();
    let mut chat = p.chat.map(|finish| (completions::Reply::default(), finish));
    for line in BufReader::new(reader).lines() {
        let line = line.map_err(|_| Failure::Retryable("provider_request_failed".into()))?;
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim_start());
            continue;
        }
        if !line.is_empty() || data.is_empty() {
            continue;
        }
        let text = std::mem::take(&mut data);
        if let Some((reply, _)) = &mut chat {
            // Chat Completions: chunks until "[DONE]" (or the end of the stream).
            if text == "[DONE]" {
                break;
            }
            let chunk: Value = serde_json::from_str(&text)
                .map_err(|_| Failure::Fatal("invalid_provider_json".into()))?;
            reply.chunk(&chunk, report)?;
            continue;
        }
        let event: Value = serde_json::from_str(&text)
            .map_err(|_| Failure::Fatal("invalid_provider_json".into()))?;
        if handle(&event, report, &mut seen)? {
            return Ok(());
        }
    }
    if let Some((reply, finish)) = chat {
        return reply.finish(report, finish);
    }
    Err(Failure::Retryable("provider_stream_incomplete".into()))
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
                service_tier: event
                    .pointer("/response/service_tier")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
            return Ok(true);
        }
        // Fixed codes as the harness gives them; the provider's text is never passed on.
        "error" if refuses_reasoning_off(&event.to_string()) => {
            return Err(Failure::Retryable(REASONING_MANDATORY.into()));
        }
        "error" => {
            let code = event.get("code").or_else(|| event.pointer("/error/code"));
            report(Piece::ProviderError(provider_error(event, code)));
            return Err(Failure::Fatal(
                if code.and_then(Value::as_str) == Some("context_length_exceeded") {
                    "context_length_exceeded".into()
                } else {
                    "provider_error".into()
                },
            ));
        }
        "response.failed" => {
            let code = event.pointer("/response/error/code");
            report(Piece::ProviderError(provider_error(event, code)));
            return Err(Failure::Fatal(
                if code.and_then(Value::as_str) == Some("context_length_exceeded") {
                    "context_length_exceeded".into()
                } else {
                    "provider_response_failed".into()
                },
            ));
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
    // As pi reads it: input_tokens includes cached and cache-write tokens.
    let (input, cached, written) = (
        get("/input_tokens"),
        get("/input_tokens_details/cached_tokens"),
        get("/input_tokens_details/cache_write_tokens"),
    );
    json!({"input": input.saturating_sub(cached).saturating_sub(written), "output": get("/output_tokens"),
           "cacheRead": cached, "cacheWrite": written, "reasoning": get("/output_tokens_details/reasoning_tokens"),
           "totalTokens": get("/total_tokens"), "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}})
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
