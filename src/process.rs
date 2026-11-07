//! The core as a process: one session, JSON lines over stdin/stdout.
//!
//! Harness -> core: send, steer, abort, prepared, credentials, tool_result, context, checkpoint
//! Core -> harness: prepare_request, need_credentials, text_delta, reasoning_delta, tool_call,
//!   cancel, retry, cut_off, message_end, turn_end, context_file, error
//!
//! Tool hand-off (specification/CoreToolHandoff.tla): a call is saved as soon as it is
//! complete in the stream, then handed to the runner (`tool_call`); the reply keeps
//! streaming. Results (`tool_result`) are saved whenever they arrive. The next request
//! waits for every normal call; async calls do not block it. A dropped connection is
//! retried from scratch only when nothing of the reply was saved; otherwise the saved
//! part stays, a cut-off note is added and the conversation continues. Abort cancels
//! every unfinished call. On open, a saved call without a result gets an interrupted
//! result; nothing is re-run.
//!
//! Built-in tools (pi's read, bash, edit, write, grep, find, ls) run inside the core: their
//! `tool_call` event says `"builtin": true`, so the runner only shows it, and a `tool_result`
//! event follows once the result is saved. A runner tool with the same name replaces a
//! built-in; `--no-tool <name>` turns one off.

use crate::context_file;
use crate::provider::{self, Failure, Piece, Request};
use crate::responses::Target;
use crate::store::{Error, Session};
use crate::tools;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

type Result<T> = std::result::Result<T, Error>;

const MAX_ATTEMPTS: u32 = 3;
const CUT_OFF_NOTE: &str = "[core] Your previous reply was cut off by a connection loss. Its completed tool calls were run; their results follow.";

fn emit(event: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{event}");
    let _ = out.flush();
}

enum Msg {
    Op(Value),
    Piece(u64, Piece),
    /// A built-in tool finished: call id and result.
    Builtin(String, Value),
    Closed,
}

struct Call {
    name: String,
    is_async: bool,
    done: bool,
}

/// How a reply ended.
enum End {
    /// Finished or cut off; `calls` says whether it asked for tools.
    Reply {
        calls: bool,
    },
    Aborted,
    Failed,
}

struct Core {
    session: Session,
    session_id: String,
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
    calls: HashMap<String, Call>,
    steer: VecDeque<Value>,
    queued: VecDeque<Value>,
    requests: u64,
    attempts: u64,
    closed: bool,
    /// The folder the core was started in: built-in tools work there.
    cwd: PathBuf,
    /// Built-ins turned off with `--no-tool`.
    disabled: Vec<String>,
    /// Built-ins offered in the current request (not disabled, not replaced by a runner tool).
    active: HashSet<String>,
    /// Running built-in calls.
    running: HashMap<String, Arc<tools::Cancel>>,
    /// PI_* variables for bash, from the current request.
    tool_env: Vec<(String, String)>,
}

pub fn run(dir: &Path, disabled: Vec<String>) -> Result<()> {
    let session = if dir.join("meta.json").exists() {
        Session::open(dir)?
    } else {
        let id = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("session")
            .to_string();
        Session::create(dir, &id)?
    };
    session.recover()?;
    recover_calls(&session)?;
    let session_id = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session")
        .to_string();
    let (tx, rx) = channel();
    let input = tx.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(op) => {
                    if input.send(Msg::Op(op)).is_err() {
                        return;
                    }
                }
                Err(e) => emit(json!({"ev": "error", "error": format!("bad op: {e}")})),
            }
        }
        let _ = input.send(Msg::Closed);
    });
    let mut core = Core {
        session,
        session_id,
        rx,
        tx,
        calls: HashMap::new(),
        steer: VecDeque::new(),
        queued: VecDeque::new(),
        requests: 0,
        attempts: 0,
        closed: false,
        cwd: std::env::current_dir().map_err(|e| Error::Io(PathBuf::from("."), e))?,
        disabled,
        active: HashSet::new(),
        running: HashMap::new(),
        tool_env: Vec::new(),
    };
    while let Some(op) = core.next_idle_op() {
        if let Err(err) = core.idle(&op) {
            emit(json!({"ev": "error", "error": err.to_string()}));
        }
    }
    Ok(())
}

/// On open: a crash may have left a reply open and calls without results.
fn recover_calls(session: &Session) -> Result<()> {
    for reply in session.unfinished_replies()? {
        session.append_more(reply, &[], Some((&provider::zero_usage(), "error")))?;
    }
    let messages = session.context_refs()?;
    let mut results: Vec<String> = Vec::new();
    for m in &messages {
        if m.get("role").and_then(Value::as_str) == Some("toolResult")
            && let Some(id) = m.get("toolCallId").and_then(Value::as_str)
        {
            results.push(id.to_string());
        }
    }
    for m in &messages {
        if m.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        for block in m
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                continue;
            }
            let id = block.get("id").and_then(Value::as_str).unwrap_or("");
            if !id.is_empty() && !results.iter().any(|r| r == id) {
                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                session.append("tool", &tool_result_message(id, name, "Interrupted: the process stopped after this call was saved. It may or may not have run; its outcome is unknown, and it was not re-run.", true))?;
            }
        }
    }
    Ok(())
}

fn tool_result_message(call: &str, name: &str, text: &str, is_error: bool) -> Value {
    json!({"role": "toolResult", "toolCallId": call, "toolName": name,
           "content": [{"type": "text", "text": text}], "isError": is_error, "timestamp": provider::now_ms()})
}

impl Core {
    /// The next message. Built-in results are saved here, whatever the caller is waiting for,
    /// and returned (as saved) so the caller re-checks what it waits for; after stdin closes,
    /// running built-ins are still waited for.
    fn recv(&mut self) -> Result<Msg> {
        loop {
            if self.closed && self.running.is_empty() {
                return Ok(Msg::Closed);
            }
            match self.rx.recv() {
                Ok(Msg::Builtin(id, result)) => {
                    self.running.remove(&id);
                    self.record(&id, &result, true)?;
                    return Ok(Msg::Builtin(id, Value::Null));
                }
                Ok(Msg::Closed) | Err(_) => self.closed = true,
                Ok(msg) => return Ok(msg),
            }
        }
    }

    fn next_idle_op(&mut self) -> Option<Value> {
        if let Some(op) = self.queued.pop_front() {
            return Some(op);
        }
        loop {
            match self.recv() {
                Ok(Msg::Op(op)) => return Some(op),
                Ok(Msg::Closed) => return None,
                Ok(_) => continue, // a stale reply after an abort
                Err(err) => emit(json!({"ev": "error", "error": err.to_string()})),
            }
        }
    }

    fn idle(&mut self, op: &Value) -> Result<()> {
        match op.get("op").and_then(Value::as_str) {
            Some("send") => {
                let input = op
                    .get("input")
                    .ok_or_else(|| Error::Format("send needs input".into()))?;
                self.turn(user_message(input))
            }
            Some("steer") => {
                let input = op
                    .get("input")
                    .ok_or_else(|| Error::Format("steer needs input".into()))?;
                self.turn(user_message(input))
            }
            Some("tool_result") => self.save_result(op),
            Some("abort") => Ok(()),
            Some("context") => self
                .session
                .context()
                .map(|m| emit(json!({"ev": "context", "messages": m}))),
            Some("checkpoint") => match op.get("messages") {
                Some(Value::Array(m)) => self
                    .session
                    .checkpoint(m)
                    .map(|id| emit(json!({"ev": "checkpointed", "id": id}))),
                _ => Err(Error::Format("checkpoint needs messages".into())),
            },
            other => Err(Error::Format(format!("unknown op {other:?}"))),
        }
    }

    /// Handle an op that arrives mid-turn. Returns true on abort.
    fn during_turn(&mut self, op: Value) -> Result<bool> {
        match op.get("op").and_then(Value::as_str) {
            Some("tool_result") => self.save_result(&op).map(|_| false),
            Some("steer") => {
                if let Some(input) = op.get("input") {
                    self.steer.push_back(user_message(input));
                }
                Ok(false)
            }
            Some("abort") => Ok(true),
            _ => {
                self.queued.push_back(op);
                Ok(false)
            }
        }
    }

    /// Wait for the harness's answer to a request this core made.
    fn answer(&mut self, op: &str, id: &str) -> Result<Option<Value>> {
        loop {
            match self.recv()? {
                Msg::Op(value) => {
                    if value.get("op").and_then(Value::as_str) == Some(op)
                        && value.get("id").and_then(Value::as_str) == Some(id)
                    {
                        return Ok(Some(value));
                    }
                    if self.during_turn(value)? {
                        return Ok(None);
                    }
                }
                Msg::Piece(..) | Msg::Builtin(..) => {}
                Msg::Closed => {
                    return Err(Error::Format(format!(
                        "stdin closed while waiting for {op}"
                    )));
                }
            }
        }
    }

    fn save_result(&mut self, op: &Value) -> Result<()> {
        let id = op
            .get("call")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if self.running.contains_key(&id) {
            emit(
                json!({"ev": "error", "error": format!("tool_result for built-in call {id} ignored")}),
            );
            return Ok(());
        }
        let result = op.get("result").cloned().unwrap_or(Value::Null);
        self.record(&id, &result, false)
    }

    /// Save a call's result, unless it already has one (cancelled or answered).
    fn record(&mut self, id: &str, result: &Value, builtin: bool) -> Result<()> {
        let Some(call) = self.calls.get_mut(id) else {
            if !builtin {
                emit(
                    json!({"ev": "error", "error": format!("tool_result for unknown or finished call {id}")}),
                );
            }
            return Ok(());
        };
        if call.done {
            return Ok(()); // never record a second outcome
        }
        call.done = true;
        let mut message = json!({"role": "toolResult", "toolCallId": id, "toolName": call.name,
            "content": result.get("content").cloned().unwrap_or_else(|| json!([])),
            "isError": result.get("isError").cloned().unwrap_or(json!(false)), "timestamp": provider::now_ms()});
        if let Some(details) = result.get("details") {
            message["details"] = details.clone();
        }
        self.session.append("tool", &message)?;
        if builtin {
            emit(json!({"ev": "tool_result", "call": id, "result": result}));
        }
        Ok(())
    }

    /// Run a built-in call on its own thread; its result comes back as `Msg::Builtin`.
    fn start_builtin(&mut self, id: &str, name: &str, arguments: Value) {
        let cancel = Arc::new(tools::Cancel::default());
        self.running.insert(id.to_string(), cancel.clone());
        let ctx = tools::Context {
            cwd: self.cwd.clone(),
            env: self.tool_env.clone(),
            cancel,
        };
        let (tx, id, name) = (self.tx.clone(), id.to_string(), name.to_string());
        std::thread::spawn(move || {
            let result = tools::run(&name, &arguments, &ctx);
            let _ = tx.send(Msg::Builtin(id, result));
        });
    }

    fn turn(&mut self, user: Value) -> Result<()> {
        self.boundary()?;
        self.session.append("user", &user)?;
        loop {
            match self.request()? {
                End::Aborted => return Ok(()),
                End::Failed => {
                    emit(json!({"ev": "turn_end", "stopReason": "error"}));
                    return Ok(());
                }
                End::Reply { calls } => {
                    if !calls && self.steer.is_empty() {
                        emit(json!({"ev": "turn_end", "stopReason": "stop"}));
                        return Ok(());
                    }
                }
            }
            if !self.wait_for_normal_calls()? {
                return Ok(());
            }
            self.boundary()?;
            while let Some(steer) = self.steer.pop_front() {
                self.session.append("user", &steer)?;
            }
        }
    }

    /// Request boundary: an edited context file becomes the context.
    fn boundary(&mut self) -> Result<()> {
        match context_file::accept(&self.session)? {
            context_file::Outcome::Unchanged => {}
            context_file::Outcome::Accepted { messages } => {
                emit(json!({"ev": "context_file", "accepted": true, "messages": messages}))
            }
            context_file::Outcome::Rejected(reason) => {
                emit(json!({"ev": "context_file", "accepted": false, "error": reason}))
            }
        }
        Ok(())
    }

    /// Every normal call has a result. False if aborted.
    fn wait_for_normal_calls(&mut self) -> Result<bool> {
        while self.calls.values().any(|c| !c.done && !c.is_async) {
            match self.recv()? {
                Msg::Op(op) => {
                    if self.during_turn(op)? {
                        self.abort(None)?;
                        return Ok(false);
                    }
                }
                Msg::Piece(..) | Msg::Builtin(..) => {}
                Msg::Closed => {
                    return Err(Error::Format(
                        "stdin closed while tools were running".into(),
                    ));
                }
            }
        }
        Ok(true)
    }

    /// Cancel every unfinished call (async ones too) and close an open reply.
    fn abort(&mut self, open: Option<(u64, &[Value])>) -> Result<()> {
        let unfinished: Vec<String> = self
            .calls
            .iter()
            .filter(|(_, c)| !c.done)
            .map(|(id, _)| id.clone())
            .collect();
        if !unfinished.is_empty() {
            emit(json!({"ev": "cancel", "calls": unfinished}));
        }
        for id in &unfinished {
            if let Some(cancel) = self.running.remove(id) {
                cancel.cancel();
            }
            if let Some(call) = self.calls.get_mut(id) {
                call.done = true;
                let message = tool_result_message(id, &call.name, "Cancelled.", true);
                self.session.append("tool", &message)?;
            }
        }
        if let Some((reply, rest)) = open {
            let usage = provider::zero_usage();
            self.session
                .append_more(reply, rest, Some((&usage, "aborted")))?;
        }
        emit(json!({"ev": "turn_end", "stopReason": "aborted"}));
        Ok(())
    }

    /// One request: prepare, stream, save. Retries from scratch only while nothing was saved.
    fn request(&mut self) -> Result<End> {
        context_file::publish(&self.session)?;
        let context = self.session.context()?;
        self.requests += 1;
        let id = format!("r{}", self.requests);
        emit(json!({"ev": "prepare_request", "id": id, "context": context}));
        let Some(prepared) = self.answer("prepared", &id)? else {
            self.abort(None)?;
            return Ok(End::Aborted);
        };
        let context = match prepared.get("context") {
            Some(Value::Array(edited)) => edited.clone(),
            _ => context,
        };
        let model = prepared
            .get("model")
            .cloned()
            .ok_or_else(|| Error::Format("prepared needs model".into()))?;
        let field = |key: &str| {
            model
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let (api, provider_name, model_id, base_url) = (
            field("api"),
            field("provider"),
            field("id"),
            field("baseUrl"),
        );
        emit(json!({"ev": "need_credentials", "id": id, "provider": provider_name}));
        let Some(credentials) = self.answer("credentials", &id)? else {
            self.abort(None)?;
            return Ok(End::Aborted);
        };
        let token = credentials
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Format("credentials need token".into()))?;
        // Built-ins first, then the runner's tools; a runner tool replaces a built-in of its name.
        let runner_tools = prepared
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let runner_names: HashSet<&str> = runner_tools
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        self.active = tools::NAMES
            .iter()
            .filter(|n| !runner_names.contains(**n) && !self.disabled.iter().any(|d| d == **n))
            .map(|n| n.to_string())
            .collect();
        let mut request_tools: Vec<Value> = tools::NAMES
            .iter()
            .filter(|n| self.active.contains(**n))
            .filter_map(|n| tools::definition(n).cloned())
            .collect();
        request_tools.extend(runner_tools.iter().cloned());
        let reasoning = prepared.get("reasoning").and_then(Value::as_str);
        self.tool_env = vec![
            ("PI_SESSION_ID".into(), self.session_id.clone()),
            ("PI_PROVIDER".into(), provider_name.clone()),
            ("PI_MODEL".into(), model_id.clone()),
        ];
        if let Some(level) = reasoning {
            self.tool_env
                .push(("PI_REASONING_LEVEL".into(), level.to_string()));
        }
        let session_id = self.session_id.clone();
        let target = Target {
            api: &api,
            provider: &provider_name,
            model: &model_id,
        };
        let req = Request {
            target: Target {
                api: &api,
                provider: &provider_name,
                model: &model_id,
            },
            base_url: &base_url,
            instructions: prepared
                .get("instructions")
                .and_then(Value::as_str)
                .unwrap_or(""),
            reasoning,
            tools: &request_tools,
            session_id: &session_id,
            token,
            account_id: credentials.get("accountId").and_then(Value::as_str),
        };
        for attempt in 1..=MAX_ATTEMPTS {
            let ready = provider::prepare(&req, &context)?;
            self.attempts += 1;
            let tag = self.attempts;
            let tx = self.tx.clone();
            provider::spawn(ready, move |piece| {
                let _ = tx.send(Msg::Piece(tag, piece));
            });
            match self.stream(tag, &target)? {
                Streamed::Ended(end) => return Ok(end),
                Streamed::Retry(reason) if attempt < MAX_ATTEMPTS => {
                    emit(json!({"ev": "retry", "attempt": attempt + 1, "error": reason}));
                    std::thread::sleep(Duration::from_secs(1 << (attempt - 1)));
                }
                Streamed::Retry(reason) => {
                    emit(json!({"ev": "error", "error": reason}));
                    return Ok(End::Failed);
                }
            }
        }
        Ok(End::Failed)
    }

    /// Stream one attempt, saving and handing off each tool call as soon as it is complete.
    fn stream(&mut self, tag: u64, target: &Target) -> Result<Streamed> {
        let mut message = provider::empty_reply(target);
        let mut unsaved: Vec<Value> = Vec::new();
        let mut open: Option<u64> = None;
        let mut has_calls = false;
        loop {
            let piece = match self.recv()? {
                Msg::Piece(t, piece) if t == tag => piece,
                Msg::Piece(..) | Msg::Builtin(..) => continue,
                Msg::Op(op) => {
                    if self.during_turn(op)? {
                        self.abort(open.map(|r| (r, unsaved.as_slice())))?;
                        return Ok(Streamed::Ended(End::Aborted));
                    }
                    continue;
                }
                Msg::Closed => return Err(Error::Format("stdin closed mid-reply".into())),
            };
            match piece {
                Piece::Text(delta) => emit(json!({"ev": "text_delta", "delta": delta})),
                Piece::Reasoning(delta) => emit(json!({"ev": "reasoning_delta", "delta": delta})),
                Piece::Block(block) => {
                    let is_call = block.get("type").and_then(Value::as_str) == Some("toolCall");
                    unsaved.push(block.clone());
                    if is_call {
                        has_calls = true;
                        // Save the reply so far with this call, then hand the call off.
                        match open {
                            None => {
                                message["content"] = Value::Array(std::mem::take(&mut unsaved));
                                message["stopReason"] = json!("toolUse");
                                open = Some(self.session.append_open_reply(&message)?);
                            }
                            Some(reply) => {
                                self.session.append_more(
                                    reply,
                                    &std::mem::take(&mut unsaved),
                                    None,
                                )?;
                            }
                        }
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let is_async = block.get("async") == Some(&Value::Bool(true));
                        self.calls.insert(
                            id.clone(),
                            Call {
                                name: name.clone(),
                                is_async,
                                done: false,
                            },
                        );
                        let builtin = self.active.contains(&name);
                        emit(
                            json!({"ev": "tool_call", "call": id, "name": name, "arguments": block.get("arguments"), "async": is_async, "builtin": builtin}),
                        );
                        if builtin {
                            let arguments =
                                block.get("arguments").cloned().unwrap_or_else(|| json!({}));
                            self.start_builtin(&id, &name, arguments);
                        }
                    }
                }
                Piece::Done { usage, incomplete } => {
                    let stop = if has_calls {
                        "toolUse"
                    } else if incomplete {
                        "length"
                    } else {
                        "stop"
                    };
                    let n = match open {
                        Some(reply) => {
                            self.session
                                .append_more(reply, &unsaved, Some((&usage, stop)))?
                        }
                        None => {
                            message["content"] = Value::Array(unsaved);
                            message["usage"] = usage;
                            message["stopReason"] = json!(stop);
                            self.session.append("assistant", &message)?
                        }
                    };
                    emit(json!({"ev": "message_end", "n": n, "stopReason": stop}));
                    return Ok(Streamed::Ended(End::Reply { calls: has_calls }));
                }
                Piece::Failed(Failure::Retryable(reason)) if open.is_none() => {
                    return Ok(Streamed::Retry(reason));
                }
                Piece::Failed(failure) => {
                    let (reason, fatal) = match failure {
                        Failure::Retryable(r) => (r, false),
                        Failure::Fatal(r) => (r, true),
                    };
                    let Some(reply) = open else {
                        emit(json!({"ev": "error", "error": reason}));
                        return Ok(Streamed::Ended(End::Failed));
                    };
                    // Something was saved: keep it, let the started calls finish.
                    let usage = provider::zero_usage();
                    self.session
                        .append_more(reply, &unsaved, Some((&usage, "error")))?;
                    if fatal {
                        emit(json!({"ev": "error", "error": reason}));
                        if !self.wait_for_normal_calls()? {
                            return Ok(Streamed::Ended(End::Aborted));
                        }
                        return Ok(Streamed::Ended(End::Failed));
                    }
                    self.session.append("user", &json!({"role": "user", "content": [{"type": "text", "text": CUT_OFF_NOTE}], "timestamp": provider::now_ms()}))?;
                    emit(json!({"ev": "cut_off", "error": reason}));
                    return Ok(Streamed::Ended(End::Reply { calls: true }));
                }
            }
        }
    }
}

enum Streamed {
    Ended(End),
    Retry(String),
}

fn user_message(input: &Value) -> Value {
    let content = input.get("content").cloned().unwrap_or_else(
        || json!([{"type": "text", "text": input.get("text").cloned().unwrap_or(Value::Null)}]),
    );
    json!({"role": "user", "content": content, "timestamp": provider::now_ms()})
}
