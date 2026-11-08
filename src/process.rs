//! The core as a process: one session, JSON lines over stdin/stdout.
//!
//! Harness -> core: send, steer, abort, prepared, credentials, tool_result, context, checkpoint
//! Core -> harness: prepare_request, need_credentials, text_delta, reasoning_delta, tool_call,
//!   cancel, retry, cut_off, message_end, turn_end, settled, context_file, error
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

use crate::auth;
use crate::budget;
use crate::catalog;
use crate::context_file;
use crate::models::{Models, Pricing};
use crate::prompt;
use crate::provider::{self, Failure, Piece, Request};
use crate::responses::Target;
use crate::store::{Error, Session};
use crate::tools;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

/// pi's harness gives up on a reply after this long without a sign of life
/// (`prepared.streamStallMs` changes it).
const STREAM_STALL_MS: u64 = 10 * 60_000;

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
    /// Context windows from the models file.
    models: Models,
    /// Models already reported as having no known window.
    warned: HashSet<String>,
    /// Injected messages that arrived mid-turn, added at the next safe point.
    injected: VecDeque<Value>,
    /// One-off completions waiting for their credentials, by id.
    completions: HashMap<String, Value>,
    /// How long a reply may be silent before the attempt fails.
    stall: Duration,
    /// Where the system prompt's AGENTS files and skills come from.
    sources: prompt::Sources,
    /// Where fetched model lists (OpenRouter's) are cached.
    catalog_dir: PathBuf,
    /// The core's own credentials file.
    auth_path: PathBuf,
    /// The id of the last op taken from the runner (answers to the core's own questions
    /// aside), reported by `settled` so the runner knows which of its ops are done.
    seen: Value,
    /// A turn-starting op was handled since the last `settled`.
    unsettled: bool,
}

/// Where the core keeps what it fetches and what it is signed in with.
pub struct Paths {
    pub catalog: PathBuf,
    pub auth: PathBuf,
}

pub fn run(
    dir: &Path,
    disabled: Vec<String>,
    models: Models,
    sources: prompt::Sources,
    paths: Paths,
) -> Result<()> {
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
        models,
        warned: HashSet::new(),
        injected: VecDeque::new(),
        completions: HashMap::new(),
        stall: Duration::from_millis(STREAM_STALL_MS),
        sources,
        catalog_dir: paths.catalog,
        auth_path: paths.auth,
        seen: Value::Null,
        unsettled: false,
    };
    while let Some(op) = core.next_idle_op() {
        if let Err(err) = core.idle(&op) {
            emit(
                json!({"ev": "error", "id": op.get("id"), "op": op.get("op"), "error": err.to_string()}),
            );
        }
        // pi's agent_settled: turns are over and nothing taken is waiting. `through` names
        // the last op taken, so a runner knows whether ops it sent since are still to come.
        if core.unsettled && core.queued.is_empty() {
            core.unsettled = false;
            emit(json!({"ev": "settled", "through": core.seen}));
        }
    }
    Ok(())
}

/// On open: a crash may have left a reply open and calls without results.
fn recover_calls(session: &Session) -> Result<()> {
    for reply in session.unfinished_replies()? {
        let end = json!({"usage": provider::zero_usage(), "stopReason": "error", "errorMessage": "interrupted"});
        session.append_more(reply, &[], Some(&end))?;
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
        Ok(self.recv_until(None)?.unwrap_or(Msg::Closed))
    }

    /// `recv`, giving up (None) at `deadline`.
    fn recv_until(&mut self, deadline: Option<Instant>) -> Result<Option<Msg>> {
        loop {
            if self.closed && self.running.is_empty() {
                return Ok(Some(Msg::Closed));
            }
            let got = match deadline {
                None => self.rx.recv().ok(),
                Some(d) => match self
                    .rx
                    .recv_timeout(d.saturating_duration_since(Instant::now()))
                {
                    Ok(msg) => Some(msg),
                    Err(RecvTimeoutError::Timeout) => return Ok(None),
                    Err(RecvTimeoutError::Disconnected) => None,
                },
            };
            match got {
                Some(Msg::Builtin(id, result)) => {
                    self.running.remove(&id);
                    self.record(&id, &result, true)?;
                    return Ok(Some(Msg::Builtin(id, Value::Null)));
                }
                Some(Msg::Closed) | None => self.closed = true,
                Some(Msg::Op(op)) => {
                    let answer = matches!(
                        op.get("op").and_then(Value::as_str),
                        Some("prepared" | "credentials" | "review_request" | "login_input")
                    );
                    if !answer && let Some(id) = op.get("id") {
                        self.seen = id.clone();
                    }
                    return Ok(Some(Msg::Op(op)));
                }
                Some(msg) => return Ok(Some(msg)),
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

    /// Ops answered the same way whether or not a turn is running. True if `op` was one.
    fn anytime(&mut self, op: &Value) -> Result<bool> {
        let id = op.get("id").cloned().unwrap_or(Value::Null);
        let text = |key: &str| op.get(key).and_then(Value::as_str);
        match text("op") {
            // pi's custom entry: harness bookkeeping the model never sees.
            Some("record") => {
                let ns = text("ns").ok_or_else(|| Error::Format("record needs ns".into()))?;
                let data = op.get("data").cloned().unwrap_or(Value::Null);
                let n = self
                    .session
                    .append_entry("app", json!({"ns": ns, "data": data}))?;
                emit(json!({"ev": "recorded", "id": id, "n": n}));
            }
            Some("set_model") => {
                let model = match (text("provider"), text("model")) {
                    (Some(p), Some(m)) => Some((p, m)),
                    (None, None) => None,
                    _ => {
                        return Err(Error::Format(
                            "set_model needs provider and model together".into(),
                        ));
                    }
                };
                self.session.select(model, text("reasoning"))?;
                let meta = self.session.meta()?;
                emit(
                    json!({"ev": "model_set", "id": id, "model": meta.get("model"), "reasoning": meta.get("reasoning")}),
                );
            }
            Some("history") => {
                let num = |key: &str| op.get(key).and_then(Value::as_u64);
                let from = num("from").unwrap_or(1);
                let to = num("to").unwrap_or(u64::MAX);
                let entries = self.session.entries(from, to)?;
                emit(json!({"ev": "history", "id": id, "entries": entries}));
            }
            // The models the core knows: the models file's, and OpenRouter's own list.
            Some("models") => {
                let list = catalog::list(
                    &self.models,
                    &self.catalog_dir,
                    text("provider"),
                    text("token"),
                );
                emit(json!({"ev": "models", "id": id, "models": list}));
            }
            Some("complete") => {
                let key = text("id")
                    .ok_or_else(|| Error::Format("complete needs id".into()))?
                    .to_string();
                let provider = op
                    .pointer("/model/provider")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                match self.stored_credentials(&provider) {
                    Some(credentials) => self.complete_with(op.clone(), credentials),
                    None => {
                        self.completions.insert(key.clone(), op.clone());
                        emit(json!({"ev": "need_credentials", "id": key, "provider": provider}));
                    }
                }
            }
            Some("credentials") if text("id").is_some_and(|i| self.completions.contains_key(i)) => {
                let request = self
                    .completions
                    .remove(text("id").unwrap_or(""))
                    .unwrap_or(Value::Null);
                self.complete_with(request, op.clone());
            }
            // Sign-in, kept by the core (see auth.rs): device code, or a page to open whose
            // final address is pasted back with `login_input`, or an OpenRouter API key.
            Some("login") => {
                let provider = text("provider")
                    .ok_or_else(|| Error::Format("login needs provider".into()))?
                    .to_string();
                let method = text("method")
                    .unwrap_or(if provider == auth::OPENROUTER {
                        "key"
                    } else {
                        "device"
                    })
                    .to_string();
                start_login(
                    self.auth_path.clone(),
                    id,
                    provider,
                    method,
                    text("key").map(str::to_string),
                );
            }
            Some("login_input") => {
                let key = id
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| id.to_string());
                if let Some(tx) = LOGIN_INPUTS
                    .lock()
                    .ok()
                    .and_then(|mut m| m.as_mut().and_then(|m| m.remove(&key)))
                {
                    let _ = tx.send(text("input").unwrap_or("").to_string());
                }
            }
            Some("logout") => {
                let provider = text("provider")
                    .ok_or_else(|| Error::Format("logout needs provider".into()))?;
                let removed = auth::remove(&self.auth_path, provider).map_err(Error::Format)?;
                emit(
                    json!({"ev": "logged_out", "id": id, "provider": provider, "removed": removed}),
                );
            }
            Some("auth_status") => emit(
                json!({"ev": "auth_status", "id": id, "signedIn": auth::status(&self.auth_path)}),
            ),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// The core's stored sign-in for a provider, renewed if about to expire. None when it
    /// holds none (or renewal failed, reported as `sign_in_needed`): the runner is asked.
    fn stored_credentials(&self, provider: &str) -> Option<Value> {
        match auth::credentials(&self.auth_path, provider) {
            Ok(found) => found,
            Err(error) => {
                emit(json!({"ev": "sign_in_needed", "provider": provider, "error": error}));
                None
            }
        }
    }

    fn complete_with(&self, mut request: Value, credentials: Value) {
        let named = request.get("model").cloned().unwrap_or(Value::Null);
        let token = credentials.get("token").and_then(Value::as_str);
        let model = catalog::resolve(&self.models, &self.catalog_dir, &named, token);
        request["model"] = model.clone();
        let model_id = model.get("id").and_then(Value::as_str).unwrap_or("");
        let level = request.get("reasoning").and_then(Value::as_str);
        let reasoning = crate::models::reasoning(&model, self.models.get(model_id), level);
        let pricing = Pricing::new(&model, self.models.get(model_id), None);
        start_completion(
            request,
            credentials,
            self.session_id.clone(),
            reasoning,
            pricing,
        );
    }

    /// pi's custom message: a message the model sees, added by the runner (family message,
    /// background notice, reminder). Like pi's `triggerTurn`, it wakes the model unless
    /// `"wake": false` (internal notes). During a turn it is added at the next safe point and
    /// gets a reply in that turn (pi's steer), or with `"deliver": "follow_up"` after the turn.
    fn inject(&mut self, op: &Value) -> Result<()> {
        let m = op.get("message").cloned().unwrap_or(Value::Null);
        // Text given as text is handed back as text (pi keeps custom content as given).
        let text = matches!(m.get("content"), Some(Value::String(_)));
        let content = match m.get("content") {
            Some(Value::String(s)) => json!([{"type": "text", "text": s}]),
            Some(blocks @ Value::Array(_)) => blocks.clone(),
            _ => return Err(Error::Format("add_context needs message.content".into())),
        };
        let source = m
            .get("customType")
            .and_then(Value::as_str)
            .unwrap_or("runner");
        let display = m.get("display").and_then(Value::as_bool).unwrap_or(true);
        let message = json!({"role": "user", "content": content, "timestamp": provider::now_ms()});
        let n = self
            .session
            .append_injected(&message, source, display, text, m.get("details"))?;
        emit(json!({"ev": "added", "id": op.get("id"), "n": n}));
        Ok(())
    }

    /// Add the injected messages that arrived during the turn; follow-ups wait for the
    /// turn's end unless `all`.
    fn flush_injected(&mut self, all: bool) -> Result<()> {
        let pending: Vec<Value> = self.injected.drain(..).collect();
        for op in pending {
            if all || !follow_up(&op) {
                self.inject(&op)?;
            } else {
                self.injected.push_back(op);
            }
        }
        Ok(())
    }

    /// Run a turn, then another for as long as waking messages arrived during it.
    /// After an abort, messages are still added but wake nothing.
    fn run_turns(&mut self, mut start: Option<Value>) -> Result<()> {
        loop {
            let finished = self.turn(start.take())?;
            let wake = finished && self.injected.iter().any(wakes);
            self.flush_injected(true)?;
            if !wake {
                return Ok(());
            }
        }
    }

    fn idle(&mut self, op: &Value) -> Result<()> {
        if self.anytime(op)? {
            return Ok(());
        }
        let kind = op.get("op").and_then(Value::as_str);
        if matches!(kind, Some("send" | "steer" | "add_context")) {
            self.unsettled = true;
        }
        match kind {
            Some("send") | Some("steer") => {
                let input = op
                    .get("input")
                    .ok_or_else(|| Error::Format("send needs input".into()))?;
                self.run_turns(Some(user_message(input)))
            }
            Some("add_context") => {
                self.inject(op)?;
                if wakes(op) {
                    self.run_turns(None)?;
                }
                Ok(())
            }
            Some("tool_result") => self.save_result(op),
            Some("abort") => Ok(()),
            Some("context") => self
                .session
                .context()
                .map(|m| emit(json!({"ev": "context", "messages": m}))),
            Some("checkpoint") => match op.get("messages") {
                Some(Value::Array(m)) => {
                    let session = &self.session;
                    session
                        .with_references_lock(|| session.checkpoint(m))
                        .map(|id| emit(json!({"ev": "checkpointed", "id": id})))
                }
                _ => Err(Error::Format("checkpoint needs messages".into())),
            },
            other => Err(Error::Format(format!("unknown op {other:?}"))),
        }
    }

    /// Handle an op that arrives mid-turn. Returns true on abort.
    fn during_turn(&mut self, op: Value) -> Result<bool> {
        // A malformed op is reported to its sender; it does not end the turn.
        match self.anytime(&op) {
            Ok(true) => return Ok(false),
            Ok(false) => {}
            Err(err) => {
                emit(
                    json!({"ev": "error", "id": op.get("id"), "op": op.get("op"), "error": err.to_string()}),
                );
                return Ok(false);
            }
        }
        match op.get("op").and_then(Value::as_str) {
            Some("add_context") => {
                self.injected.push_back(op);
                Ok(false)
            }
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

    /// One turn, starting with `user` if given (else with what was just added). False if aborted.
    fn turn(&mut self, user: Option<Value>) -> Result<bool> {
        self.boundary()?;
        if let Some(user) = user {
            self.session.append("user", &user)?;
        }
        loop {
            match self.request()? {
                End::Aborted => return Ok(false),
                End::Failed => {
                    emit(json!({"ev": "turn_end", "stopReason": "error"}));
                    return Ok(true);
                }
                End::Reply { calls } => {
                    let waiting = self.injected.iter().any(|op| wakes(op) && !follow_up(op));
                    if !calls && self.steer.is_empty() && !waiting {
                        emit(json!({"ev": "turn_end", "stopReason": "stop"}));
                        return Ok(true);
                    }
                }
            }
            if !self.wait_for_normal_calls()? {
                return Ok(false);
            }
            self.boundary()?;
            self.flush_injected(false)?;
            while let Some(steer) = self.steer.pop_front() {
                self.session.append("user", &steer)?;
            }
        }
    }

    /// Request boundary: an edited context file becomes the context.
    fn boundary(&mut self) -> Result<()> {
        let session = &self.session;
        match session.with_references_lock(|| context_file::accept(session))? {
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
            let end = json!({"usage": provider::zero_usage(), "stopReason": "aborted", "errorMessage": "request_aborted"});
            self.session.append_more(reply, rest, Some(&end))?;
        }
        emit(json!({"ev": "turn_end", "stopReason": "aborted"}));
        Ok(())
    }

    /// The messages actually sent: the overflow guard applied, then the context usage line at
    /// the very end (so the cached prompt stays unchanged). Neither is saved. None when the
    /// request is too large to send (`context_too_large`).
    fn budget(
        &mut self,
        context: Vec<Value>,
        refs: Option<&[Value]>,
        trusted_from: usize,
        model: &str,
        window: Option<u64>,
    ) -> Option<Vec<Value>> {
        let mut estimate = budget::estimate(&context, trusted_from);
        let line = budget::text_message("user", &budget::usage_line(&estimate, window));
        estimate.total += budget::estimate_tokens(&line);
        let Some(window) = window else {
            if self.warned.insert(model.to_string()) {
                emit(json!({"ev": "window_unknown", "model": model}));
            }
            let mut sent = context;
            sent.push(line);
            return Some(sent);
        };
        match budget::guard(context, refs, estimate.total, window, self.session.dir()) {
            budget::Guarded::Fits(mut sent) => {
                sent.push(line);
                Some(sent)
            }
            budget::Guarded::TooLarge { tokens } => {
                emit(json!({"ev": "context_too_large", "tokens": tokens, "window": window}));
                None
            }
        }
    }

    /// One request: prepare, stream, save. Retries from scratch only while nothing was saved.
    fn request(&mut self) -> Result<End> {
        context_file::publish(&self.session)?;
        let (refs, from_checkpoint) = self.session.context_parts()?;
        let context = self.session.resolve(&refs)?;
        self.requests += 1;
        let id = format!("r{}", self.requests);
        // The model and reasoning last chosen with set_model, for the runner to use.
        let meta = self.session.meta()?;
        let selected = json!({"model": meta.get("model"), "reasoning": meta.get("reasoning")});
        // The AGENTS files the prompt will carry, read fresh for every request.
        let files = prompt::context_files(&self.cwd, &self.sources.agent_dir);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        emit(
            json!({"ev": "prepare_request", "id": id, "context": context, "selected": selected, "contextFiles": paths}),
        );
        let Some(prepared) = self.answer("prepared", &id)? else {
            self.abort(None)?;
            return Ok(End::Aborted);
        };
        // A context the runner replaced has no references and no trusted token count.
        let (context, refs, trusted_from) = match prepared.get("context") {
            Some(Value::Array(edited)) => (edited.clone(), None, edited.len()),
            _ => (context, Some(refs), from_checkpoint),
        };
        let named = prepared
            .get("model")
            .cloned()
            .ok_or_else(|| Error::Format("prepared needs model".into()))?;
        let provider_name = named
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // The core's own sign-in first (renewed when about to expire); the runner otherwise.
        let credentials = match self.stored_credentials(&provider_name) {
            Some(c) => c,
            None => {
                emit(json!({"ev": "need_credentials", "id": id, "provider": provider_name}));
                let Some(c) = self.answer("credentials", &id)? else {
                    self.abort(None)?;
                    return Ok(End::Aborted);
                };
                c
            }
        };
        let token = credentials
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Format("credentials need token".into()))?;
        // The runner names the model; its details come from the catalog (OpenRouter's own
        // list, or the models file), under whatever the runner set itself.
        let model = catalog::resolve(&self.models, &self.catalog_dir, &named, Some(token));
        let field = |key: &str| {
            model
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let (api, model_id, base_url) = (field("api"), field("id"), field("baseUrl"));
        // Built-ins first, then the runner's tools; a runner tool replaces a built-in of its name.
        let mut runner_tools = prepared
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // pi's promptSnippet / promptGuidelines go into the prompt, not to the provider.
        let runner_prompts: Vec<prompt::ToolPrompt> = runner_tools
            .iter_mut()
            .map(prompt::take_tool_prompt)
            .collect();
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
        let preamble = prepared
            .get("instructions")
            .and_then(Value::as_str)
            .unwrap_or("");
        // pi's forceSystemPrompt: `"basePrompt": false` sends the runner's instructions alone.
        let instructions = if prepared.get("basePrompt") == Some(&Value::Bool(false)) {
            preamble.to_string()
        } else {
            let mut sections = Vec::new();
            if let Some(Value::Object(extra)) = prepared.get("sections") {
                for (name, content) in extra {
                    if !prompt::valid_section_name(name) {
                        return Err(Error::Format(format!(
                            "invalid system prompt section name: {name}"
                        )));
                    }
                    sections.push((name.clone(), content.as_str().unwrap_or("").to_string()));
                }
            }
            let mut tool_prompts: Vec<prompt::ToolPrompt> = tools::NAMES
                .iter()
                .filter(|n| self.active.contains(**n))
                .map(|n| prompt::builtin(n))
                .collect();
            tool_prompts.extend(runner_prompts);
            let skills = prompt::skills(&self.cwd, &self.sources);
            prompt::build(
                preamble,
                &tool_prompts,
                &files,
                &skills,
                &self.cwd,
                &sections,
            )
        };
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
        // The level as the model accepts it (pi's clampThinkingLevel and thinkingLevelMap).
        let reasoning_setting =
            crate::models::reasoning(&model, self.models.get(&model_id), reasoning);
        let req = Request {
            target: Target {
                api: &api,
                provider: &provider_name,
                model: &model_id,
            },
            // The credentials may name another endpoint (an account on its own proxy).
            base_url: credentials
                .get("baseUrl")
                .and_then(Value::as_str)
                .unwrap_or(base_url.as_str()),
            instructions: &instructions,
            reasoning: &reasoning_setting,
            service_tier: prepared.get("serviceTier").and_then(Value::as_str),
            tools: &request_tools,
            session_id: &session_id,
            token,
            account_id: credentials.get("accountId").and_then(Value::as_str),
            developer_role: provider::developer_role(&model, &reasoning_setting),
            extra_headers: [credentials.get("headers"), model.get("headers")]
                .into_iter()
                .flatten()
                .collect(),
            model: &model,
        };
        self.stall = Duration::from_millis(
            prepared
                .get("streamStallMs")
                .and_then(Value::as_u64)
                .unwrap_or(STREAM_STALL_MS),
        );
        // The model's window (from the runner or the fetched list), else the models file's.
        let window = model
            .get("contextWindow")
            .and_then(Value::as_u64)
            .filter(|w| *w > 0)
            .or_else(|| self.models.window(&model_id));
        let Some(context) = self.budget(context, refs.as_deref(), trusted_from, &model_id, window)
        else {
            return Ok(End::Failed);
        };
        let mut ready = provider::prepare(&req, &context)?;
        // `"transport": "sse"` keeps a Codex request on plain HTTP.
        if prepared.get("transport").and_then(Value::as_str) == Some("sse") {
            ready.transport = provider::Transport::Sse;
        }
        // pi's before_provider_request / before_provider_headers: the runner may review and
        // change the exact request (body and headers) before it is sent.
        if prepared.get("reviewRequest") == Some(&Value::Bool(true)) {
            emit(json!({"ev": "review_request", "id": id, "request": ready.view()}));
            let Some(reviewed) = self.answer("review_request", &id)? else {
                self.abort(None)?;
                return Ok(End::Aborted);
            };
            ready.replace(reviewed.get("body"), reviewed.get("headers"))?;
        }
        let pricing = Pricing::new(
            &model,
            self.models.get(&model_id),
            prepared.get("serviceTier").and_then(Value::as_str),
        );
        for attempt in 1..=MAX_ATTEMPTS {
            self.attempts += 1;
            let tag = self.attempts;
            let tx = self.tx.clone();
            provider::spawn(ready.clone(), move |piece| {
                let _ = tx.send(Msg::Piece(tag, piece));
            });
            match self.stream(tag, &target, &pricing, window)? {
                Streamed::Ended(end) => return Ok(end),
                // The model's reasoning cannot be switched off: send again without the
                // setting (the model's own default), and remember it for this model.
                Streamed::Retry(reason)
                    if reason == provider::REASONING_MANDATORY && attempt < MAX_ATTEMPTS =>
                {
                    catalog::learn_reasoning_mandatory(
                        &self.catalog_dir,
                        &provider_name,
                        &model_id,
                    );
                    ready.without("reasoning");
                    emit(json!({"ev": "retry", "attempt": attempt + 1, "error": reason}));
                }
                Streamed::Retry(reason) if attempt < MAX_ATTEMPTS => {
                    emit(json!({"ev": "retry", "attempt": attempt + 1, "error": reason}));
                    std::thread::sleep(Duration::from_secs(1 << (attempt - 1)));
                }
                Streamed::Retry(reason) => {
                    let end = failure_end(&reason, None);
                    let n = self.save_failed_reply(&target, &end)?;
                    emit(error_event(&end, n));
                    return Ok(End::Failed);
                }
            }
        }
        Ok(End::Failed)
    }

    /// A reply that failed before anything was saved: saved empty with its error, as pi does
    /// (the context leaves failed replies out).
    fn save_failed_reply(&self, target: &Target, end: &Value) -> Result<u64> {
        let mut message = provider::empty_reply(target);
        message["content"] = json!([]);
        if let Value::Object(fields) = end {
            for (key, value) in fields {
                message[key.as_str()] = value.clone();
            }
        }
        self.session.append("assistant", &message)
    }

    /// Stream one attempt, saving and handing off each tool call as soon as it is complete.
    fn stream(
        &mut self,
        tag: u64,
        target: &Target,
        pricing: &Pricing,
        window: Option<u64>,
    ) -> Result<Streamed> {
        let mut message = provider::empty_reply(target);
        let mut unsaved: Vec<Value> = Vec::new();
        let mut open: Option<u64> = None;
        let mut has_calls = false;
        // The provider's own error, as safe fixed tokens (the harness's nativeProviderError).
        let mut provider_error: Option<Value> = None;
        // The provider's last sign of life: silence longer than `stall` fails the attempt.
        let mut heard = Instant::now();
        loop {
            let piece = match self.recv_until(Some(heard + self.stall))? {
                None => Piece::Failed(Failure::Retryable("provider_stream_stalled".into())),
                Some(Msg::Piece(t, piece)) if t == tag => {
                    heard = Instant::now();
                    piece
                }
                Some(Msg::Piece(..) | Msg::Builtin(..)) => continue,
                Some(Msg::Op(op)) => {
                    if self.during_turn(op)? {
                        self.abort(open.map(|r| (r, unsaved.as_slice())))?;
                        return Ok(Streamed::Ended(End::Aborted));
                    }
                    continue;
                }
                Some(Msg::Closed) => return Err(Error::Format("stdin closed mid-reply".into())),
            };
            match piece {
                Piece::Response(response) => {
                    let mut ev = json!({"ev": "provider_response"});
                    if let (Value::Object(e), Value::Object(r)) = (&mut ev, response) {
                        e.extend(r);
                    }
                    emit(ev);
                }
                Piece::ProviderError(e) => provider_error = Some(e),
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
                Piece::Done {
                    mut usage,
                    incomplete,
                    service_tier,
                } => {
                    pricing.apply(&mut usage, service_tier.as_deref());
                    let stop = if has_calls {
                        "toolUse"
                    } else if incomplete {
                        "length"
                    } else {
                        "stop"
                    };
                    let n = match open {
                        Some(reply) => {
                            let end = json!({"usage": usage, "stopReason": stop});
                            self.session.append_more(reply, &unsaved, Some(&end))?
                        }
                        None => {
                            message["content"] = Value::Array(unsaved);
                            message["usage"] = usage.clone();
                            message["stopReason"] = json!(stop);
                            self.session.append("assistant", &message)?
                        }
                    };
                    // Right after a reply the context is exactly what the provider counted.
                    let tokens = budget::context_tokens(&usage);
                    let context = json!({"tokens": tokens, "contextWindow": window,
                        "percent": window.zip(tokens).map(|(w, t)| t as f64 / w as f64 * 100.0)});
                    emit(
                        json!({"ev": "message_end", "n": n, "stopReason": stop, "usage": usage, "contextUsage": context}),
                    );
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
                    let end = failure_end(&reason, provider_error.as_ref());
                    let Some(reply) = open else {
                        // Nothing was saved: the failed reply is saved empty, as pi does.
                        let n = self.save_failed_reply(target, &end)?;
                        emit(error_event(&end, n));
                        return Ok(Streamed::Ended(End::Failed));
                    };
                    // Something was saved: keep it, let the started calls finish.
                    let n = self.session.append_more(reply, &unsaved, Some(&end))?;
                    if fatal {
                        emit(error_event(&end, n));
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

/// An injected message wakes the model unless it says `"wake": false`.
fn wakes(op: &Value) -> bool {
    op.get("wake") != Some(&Value::Bool(false))
}

/// Sign-ins waiting for a pasted address, by the login op's id.
static LOGIN_INPUTS: std::sync::Mutex<Option<HashMap<String, Sender<String>>>> =
    std::sync::Mutex::new(None);

/// Run one sign-in on its own thread: `login` events report each step and the end.
fn start_login(path: PathBuf, id: Value, provider: String, method: String, key: Option<String>) {
    std::thread::spawn(move || {
        let tag = id
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| id.to_string());
        let show = |step: auth::Step| match step {
            auth::Step::DeviceCode {
                url,
                code,
                expires_in,
            } => emit(json!({"ev": "login", "id": id, "provider": provider,
                "step": "device_code", "url": url, "code": code, "expiresIn": expires_in})),
            auth::Step::OpenAndPaste { url } => {
                emit(json!({"ev": "login", "id": id, "provider": provider,
                "step": "open_and_paste", "url": url}))
            }
        };
        let paste = || {
            let (tx, rx) = channel();
            if let Ok(mut inputs) = LOGIN_INPUTS.lock() {
                inputs
                    .get_or_insert_with(HashMap::new)
                    .insert(tag.clone(), tx);
            }
            rx.recv_timeout(Duration::from_secs(15 * 60)).ok()
        };
        match auth::login(&path, &provider, &method, key.as_deref(), &show, &paste) {
            Ok(done) => emit(
                json!({"ev": "login", "id": id, "provider": provider, "step": "done", "signedIn": done}),
            ),
            Err(error) => emit(
                json!({"ev": "login", "id": id, "provider": provider, "step": "failed", "error": error}),
            ),
        }
    });
}

/// The closing fields of a failed reply: pi's stopReason "error" and errorMessage (a fixed
/// code such as `provider_http_429` or `context_length_exceeded`, never provider text), and
/// the provider's own error when it sent one.
fn failure_end(code: &str, provider_error: Option<&Value>) -> Value {
    let mut end =
        json!({"usage": provider::zero_usage(), "stopReason": "error", "errorMessage": code});
    if let Some(e) = provider_error {
        end["providerError"] = e.clone();
    }
    end
}

/// The `error` event for a saved failed reply `n`.
fn error_event(end: &Value, n: u64) -> Value {
    let mut ev = json!({"ev": "error", "n": n, "error": end["errorMessage"]});
    if let Some(e) = end.get("providerError") {
        ev["providerError"] = e.clone();
    }
    ev
}

/// Delivered after the current turn rather than at its next safe point.
fn follow_up(op: &Value) -> bool {
    op.get("deliver").and_then(Value::as_str) == Some("follow_up")
}

/// pi's `completeSimple`: one request outside the conversation (titles), never saved. Runs on
/// its own thread and ends with one `completed` event carrying the reply or an error.
fn start_completion(
    request: Value,
    credentials: Value,
    session_id: String,
    reasoning: crate::models::Reasoning,
    pricing: Pricing,
) {
    std::thread::spawn(move || {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let field = |key: &str| {
            request
                .pointer(&format!("/model/{key}"))
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
        let target = Target {
            api: &api,
            provider: &provider_name,
            model: &model_id,
        };
        let Some(token) = credentials.get("token").and_then(Value::as_str) else {
            emit(json!({"ev": "completed", "id": id, "error": "credentials need token"}));
            return;
        };
        let messages = request
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let req = Request {
            target: Target {
                api: &api,
                provider: &provider_name,
                model: &model_id,
            },
            base_url: credentials
                .get("baseUrl")
                .and_then(Value::as_str)
                .unwrap_or(base_url.as_str()),
            instructions: request
                .get("instructions")
                .and_then(Value::as_str)
                .unwrap_or(""),
            reasoning: &reasoning,
            service_tier: None,
            tools: &[],
            session_id: &session_id,
            token,
            account_id: credentials.get("accountId").and_then(Value::as_str),
            developer_role: provider::developer_role(
                request.get("model").unwrap_or(&Value::Null),
                &reasoning,
            ),
            extra_headers: [
                credentials.get("headers"),
                request.pointer("/model/headers"),
            ]
            .into_iter()
            .flatten()
            .collect(),
            model: request.get("model").unwrap_or(&Value::Null),
        };
        let ready = match provider::prepare(&req, &messages) {
            Ok(r) => r,
            Err(e) => {
                emit(json!({"ev": "completed", "id": id, "error": e.to_string()}));
                return;
            }
        };
        let (tx, rx) = channel();
        provider::spawn(ready, move |piece| {
            let _ = tx.send(piece);
        });
        let mut message = provider::empty_reply(&target);
        let mut blocks = Vec::new();
        for piece in rx {
            match piece {
                Piece::Block(b) => blocks.push(b),
                Piece::Done {
                    mut usage,
                    incomplete,
                    service_tier,
                } => {
                    pricing.apply(&mut usage, service_tier.as_deref());
                    message["content"] = Value::Array(blocks);
                    message["usage"] = usage;
                    message["stopReason"] = json!(if incomplete { "length" } else { "stop" });
                    emit(json!({"ev": "completed", "id": id, "message": message}));
                    return;
                }
                Piece::Failed(Failure::Retryable(e) | Failure::Fatal(e)) => {
                    emit(json!({"ev": "completed", "id": id, "error": e}));
                    return;
                }
                Piece::Text(_)
                | Piece::Reasoning(_)
                | Piece::Response(_)
                | Piece::ProviderError(_) => {}
            }
        }
        emit(json!({"ev": "completed", "id": id, "error": "the stream ended without a reply"}));
    });
}

fn user_message(input: &Value) -> Value {
    let content = input.get("content").cloned().unwrap_or_else(
        || json!([{"type": "text", "text": input.get("text").cloned().unwrap_or(Value::Null)}]),
    );
    json!({"role": "user", "content": content, "timestamp": provider::now_ms()})
}
