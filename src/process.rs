//! The core as a process: one session, JSON lines over stdin/stdout.
//!
//! Harness -> core: {"op":"send","input":{...}}, {"op":"prepared",...}, {"op":"credentials",...}, {"op":"context"}
//! Core -> harness: prepare_request, need_credentials, text_delta, reasoning_delta, message_end, turn_end, error

use crate::context_file;
use crate::provider::{self, Delta, Request};
use crate::responses::Target;
use crate::store::{Error, Session};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::Path;

type Result<T> = std::result::Result<T, Error>;

fn emit(event: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{event}");
    let _ = out.flush();
}

struct Ops<I: Iterator<Item = std::io::Result<String>>> {
    lines: I,
}

impl<I: Iterator<Item = std::io::Result<String>>> Ops<I> {
    fn next(&mut self) -> Option<Result<Value>> {
        loop {
            let line = match self.lines.next()? {
                Ok(line) => line,
                Err(e) => return Some(Err(Error::Format(format!("stdin: {e}")))),
            };
            if line.trim().is_empty() {
                continue;
            }
            return Some(serde_json::from_str(&line).map_err(Error::from));
        }
    }

    /// Wait for the harness's answer to a request this core made.
    fn answer(&mut self, op: &str, id: &str) -> Result<Value> {
        while let Some(value) = self.next() {
            let value = value?;
            if value.get("op").and_then(Value::as_str) == Some(op)
                && value.get("id").and_then(Value::as_str) == Some(id)
            {
                return Ok(value);
            }
            emit(json!({"ev": "error", "error": format!("unexpected op while waiting for {op}")}));
        }
        Err(Error::Format(format!(
            "stdin closed while waiting for {op}"
        )))
    }
}

pub fn run(dir: &Path) -> Result<()> {
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
    let session_id = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session")
        .to_string();
    let stdin = std::io::stdin();
    let mut ops = Ops {
        lines: stdin.lock().lines(),
    };
    let mut request = 0u64;
    while let Some(op) = ops.next() {
        let op = op?;
        let result = match op.get("op").and_then(Value::as_str) {
            Some("send") => {
                request += 1;
                turn(&session, &session_id, &op, &mut ops, request)
            }
            Some("context") => session
                .context()
                .map(|messages| emit(json!({"ev": "context", "messages": messages}))),
            Some("checkpoint") => match op.get("messages") {
                Some(Value::Array(messages)) => session
                    .checkpoint(messages)
                    .map(|id| emit(json!({"ev": "checkpointed", "id": id}))),
                _ => Err(Error::Format("checkpoint needs messages".into())),
            },
            other => Err(Error::Format(format!("unknown op {other:?}"))),
        };
        if let Err(err) = result {
            emit(json!({"ev": "error", "error": err.to_string()}));
        }
    }
    Ok(())
}

fn turn<I: Iterator<Item = std::io::Result<String>>>(
    session: &Session,
    session_id: &str,
    op: &Value,
    ops: &mut Ops<I>,
    request: u64,
) -> Result<()> {
    let input = op
        .get("input")
        .ok_or_else(|| Error::Format("send needs input".into()))?;
    let user = json!({"role": "user", "content": input.get("content").cloned().unwrap_or_else(|| json!([{"type": "text", "text": input.get("text").cloned().unwrap_or(Value::Null)}])), "timestamp": now_ms()});
    // Request boundary: an edited context file becomes the context first.
    match context_file::accept(session)? {
        context_file::Outcome::Unchanged => {}
        context_file::Outcome::Accepted { messages } => {
            emit(json!({"ev": "context_file", "accepted": true, "messages": messages}))
        }
        context_file::Outcome::Rejected(reason) => {
            emit(json!({"ev": "context_file", "accepted": false, "error": reason}))
        }
    }
    session.append("user", &user)?;
    context_file::publish(session)?;

    let context = session.context()?;
    let id = format!("r{request}");
    emit(json!({"ev": "prepare_request", "id": id, "context": context}));
    let prepared = ops.answer("prepared", &id)?;
    let context = match prepared.get("context") {
        Some(Value::Array(edited)) => edited.clone(),
        _ => context,
    };
    let model = prepared
        .get("model")
        .ok_or_else(|| Error::Format("prepared needs model".into()))?;
    let field = |key: &str| model.get(key).and_then(Value::as_str).unwrap_or("");
    let (api, provider_name, model_id, base_url) = (
        field("api"),
        field("provider"),
        field("id"),
        field("baseUrl"),
    );

    emit(json!({"ev": "need_credentials", "id": id, "provider": provider_name}));
    let credentials = ops.answer("credentials", &id)?;
    let token = credentials
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Format("credentials need token".into()))?;

    let empty = Vec::new();
    let req = Request {
        target: Target {
            api,
            provider: provider_name,
            model: model_id,
        },
        base_url,
        instructions: prepared
            .get("instructions")
            .and_then(Value::as_str)
            .unwrap_or(""),
        reasoning: prepared.get("reasoning").and_then(Value::as_str),
        tools: prepared
            .get("tools")
            .and_then(Value::as_array)
            .unwrap_or(&empty),
        session_id,
        token,
        account_id: credentials.get("accountId").and_then(Value::as_str),
    };
    let message = provider::stream(&req, &context, &mut |delta| match delta {
        Delta::Text(text) => emit(json!({"ev": "text_delta", "delta": text})),
        Delta::Reasoning(text) => emit(json!({"ev": "reasoning_delta", "delta": text})),
    })?;
    let n = session.append("assistant", &message)?;
    emit(json!({"ev": "message_end", "n": n, "message": message}));
    emit(
        json!({"ev": "turn_end", "stopReason": message.get("stopReason").cloned().unwrap_or(Value::Null)}),
    );
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
