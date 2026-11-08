//! The tool hand-off end to end: the real `core` binary against a fake Responses
//! server that streams scripted replies and can drop the connection.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

static NEXT: AtomicU32 = AtomicU32::new(0);

enum Step {
    Event(Value),
    Pause(u64),
    Drop,
    /// As the only step: answer with this HTTP status and body instead of a stream.
    Status(u16, String),
}

/// One scripted reply per request, in order.
fn server(replies: Vec<Vec<Step>>) -> (String, std::thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut bodies = Vec::new();
        let mut writers = Vec::new();
        for reply in replies {
            let (mut stream, _) = listener.accept().unwrap();
            bodies.push(read_request(&mut stream));
            // Each reply streams on its own thread, so a stuck reply does not hold the next.
            writers.push(std::thread::spawn(move || {
                if let Some(Step::Status(code, body)) = reply.first() {
                    let _ = stream.write_all(
                        format!("HTTP/1.1 {code} Error\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes(),
                    );
                    return;
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                );
                for step in reply {
                    match step {
                        Step::Event(e) => {
                            let _ = stream.write_all(format!("data: {e}\n\n").as_bytes());
                            let _ = stream.flush();
                        }
                        Step::Pause(ms) => std::thread::sleep(Duration::from_millis(ms)),
                        Step::Drop | Step::Status(..) => break,
                    }
                }
            }));
        }
        for w in writers {
            let _ = w.join();
        }
        bodies
    });
    (base, handle)
}

fn read_request(stream: &mut TcpStream) -> Value {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().unwrap();
        }
    }
    if length == 0 {
        // A request without a body (a WebSocket upgrade the server does not accept).
        return Value::Null;
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// A fake Codex WebSocket server: each request frame is answered with the next reply on
/// the same socket; `Step::Drop` closes the socket, and the next request needs a new one.
/// Returns the request frames and the Authorization header of each socket.
type WsSeen = std::thread::JoinHandle<(Vec<Value>, Vec<String>)>;

// tungstenite's handshake callback returns its large error type; only the test sees it.
#[allow(clippy::result_large_err)]
fn ws_server(replies: Vec<Vec<Step>>) -> (String, WsSeen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut frames = Vec::new();
        let mut sockets = Vec::new();
        let mut replies = replies.into_iter();
        let mut pending = replies.next();
        while pending.is_some() {
            let (stream, _) = listener.accept().unwrap();
            let auth = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            let seen = auth.clone();
            let mut socket = tungstenite::accept_hdr(
                stream,
                move |req: &tungstenite::handshake::server::Request, res| {
                    *seen.lock().unwrap() = req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    Ok(res)
                },
            )
            .unwrap();
            sockets.push(auth.lock().unwrap().clone());
            'socket: while let Some(reply) = pending.take() {
                let Ok(tungstenite::Message::Text(frame)) = socket.read() else {
                    break;
                };
                frames.push(serde_json::from_str(frame.as_str()).unwrap());
                pending = replies.next();
                for step in reply {
                    match step {
                        Step::Event(e) => socket
                            .send(tungstenite::Message::text(e.to_string()))
                            .unwrap(),
                        Step::Pause(ms) => std::thread::sleep(Duration::from_millis(ms)),
                        Step::Drop | Step::Status(..) => {
                            let _ = socket.close(None);
                            let _ = socket.flush();
                            break 'socket;
                        }
                    }
                }
            }
        }
        (frames, sockets)
    });
    (base, handle)
}

fn item_done(item: Value) -> Step {
    Step::Event(json!({"type": "response.output_item.done", "item": item}))
}
fn call(id: &str, args: Value) -> Step {
    item_done(
        json!({"type": "function_call", "id": format!("fc_{id}"), "call_id": id, "name": "add", "arguments": args.to_string()}),
    )
}
fn async_call(id: &str) -> Step {
    item_done(
        json!({"type": "function_call", "id": format!("fc_{id}"), "call_id": id, "name": "add", "arguments": "{}", "async": true}),
    )
}
fn text(t: &str) -> Step {
    item_done(
        json!({"type": "message", "id": "msg_1", "content": [{"type": "output_text", "text": t}]}),
    )
}
/// A reply end with no usage: nothing measured, so the core estimates everything.
fn done_unmeasured() -> Step {
    Step::Event(json!({"type": "response.completed", "response": {}}))
}
fn done() -> Step {
    Step::Event(
        json!({"type": "response.completed", "response": {"usage": {"input_tokens": 10, "output_tokens": 2}}}),
    )
}

fn named_call(id: &str, name: &str, args: Value) -> Step {
    item_done(
        json!({"type": "function_call", "id": format!("fc_{id}"), "call_id": id, "name": name, "arguments": args.to_string()}),
    )
}

struct Core {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<Value>,
    base: String,
    dir: PathBuf,
    /// Runner tools sent with `prepared`.
    tools: Value,
    /// More fields for `prepared` (and the model object under "model").
    extra: Value,
    /// Return `provider_response` events instead of skipping them.
    show_responses: bool,
    /// The prepare_request events answered so far.
    prepares: Vec<Value>,
}

impl Core {
    fn start(dir: &Path, base: &str) -> Core {
        Core::start_with(dir, base, &[], json!([]))
    }

    fn start_with(dir: &Path, base: &str, flags: &[&str], tools: Value) -> Core {
        Core::start_in(dir, base, flags, tools, &std::env::current_dir().unwrap())
    }

    /// Started in folder `cwd` (where its tools work and its AGENTS files are found).
    fn start_in(dir: &Path, base: &str, flags: &[&str], tools: Value, cwd: &Path) -> Core {
        let mut child = Command::new(env!("CARGO_BIN_EXE_core"))
            .args(["run", dir.to_str().unwrap()])
            .args(flags)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, events) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(serde_json::from_str(&line).unwrap());
            }
        });
        Core {
            child,
            stdin,
            events,
            base: base.to_string(),
            dir: dir.to_path_buf(),
            tools,
            extra: json!({}),
            show_responses: false,
            prepares: Vec::new(),
        }
    }

    fn op(&mut self, op: Value) {
        writeln!(self.stdin, "{op}").unwrap();
    }

    /// Next event, answering prepare_request and need_credentials automatically.
    fn next(&mut self) -> Value {
        loop {
            let ev = self
                .events
                .recv_timeout(Duration::from_secs(20))
                .expect("core went quiet");
            if std::env::var_os("CORE_TEST_TRACE").is_some() {
                eprintln!("event: {ev}");
            }
            match ev["ev"].as_str() {
                Some("prepare_request") => {
                    let mut model = json!({"api": "openai-responses", "provider": "test", "id": "m", "baseUrl": self.base});
                    let mut prepared =
                        json!({"op": "prepared", "id": ev["id"], "tools": self.tools});
                    for (k, v) in self.extra.as_object().unwrap() {
                        if k == "model" {
                            for (mk, mv) in v.as_object().unwrap() {
                                // null: leave the field out, for the core to fill in.
                                if mv.is_null() {
                                    model.as_object_mut().unwrap().remove(mk);
                                } else {
                                    model[mk] = mv.clone();
                                }
                            }
                        } else {
                            prepared[k] = v.clone();
                        }
                    }
                    prepared["model"] = model;
                    self.op(prepared);
                    self.prepares.push(ev);
                }
                Some("need_credentials") => self.op(
                    json!({"op": "credentials", "id": ev["id"], "token": "t", "accountId": "acct"}),
                ),
                Some("provider_response") if self.show_responses => return ev,
                Some("text_delta")
                | Some("reasoning_delta")
                | Some("window_unknown")
                | Some("provider_response") => {}
                _ => return ev,
            }
        }
    }

    fn until(&mut self, name: &str) -> Value {
        loop {
            let ev = self.next();
            if ev["ev"] == name {
                return ev;
            }
        }
    }

    fn result(&mut self, call: &str, value: &str) {
        self.op(json!({"op": "tool_result", "call": call, "result": {"content": [{"type": "text", "text": value}]}}));
    }

    fn context(&mut self) -> Vec<Value> {
        self.op(json!({"op": "context"}));
        self.until("context")["messages"]
            .as_array()
            .unwrap()
            .clone()
    }

    fn stop(mut self) {
        drop(self.stdin);
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "core-handoff-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ))
}

fn roles(ctx: &[Value]) -> Vec<String> {
    ctx.iter()
        .map(|m| m["role"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_call_is_saved_and_handed_off_before_the_reply_ends() {
    let (base, server) = server(vec![
        vec![
            call("c1", json!({"a": 1, "b": 2})),
            Step::Pause(500),
            text("calling"),
            done(),
        ],
        vec![text("3"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "add"}}));
    let first = core.next();
    assert_eq!(
        first["ev"], "tool_call",
        "the call is handed off while the reply is still streaming"
    );
    assert_eq!(first["arguments"], json!({"a": 1, "b": 2}));
    let log = std::fs::read_to_string(d.join("log/000001.jsonl")).unwrap();
    assert!(
        log.contains("\"open\":true"),
        "the call was saved before it was handed off"
    );
    core.result("c1|fc_c1", "3");
    core.until("turn_end");
    let ctx = core.context();
    assert_eq!(
        roles(&ctx),
        ["user", "assistant", "toolResult", "assistant"]
    );
    assert_eq!(
        ctx[1]["content"].as_array().unwrap().len(),
        2,
        "both parts of the reply form one message"
    );
    let bodies = server.join().unwrap();
    let second: Vec<&str> = bodies[1]["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["type"].as_str())
        .collect();
    assert_eq!(second, ["function_call", "message", "function_call_output"]);
    assert!(
        last_input_text(&bodies[1]).starts_with("Context: about"),
        "the usage line comes last"
    );
    core.stop();
}

#[test]
fn a_drop_before_anything_is_saved_is_retried() {
    let (base, server) = server(vec![vec![Step::Drop], vec![text("hello"), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "hi"}}));
    assert_eq!(core.next()["ev"], "retry");
    core.until("turn_end");
    assert_eq!(roles(&core.context()), ["user", "assistant"]);
    assert_eq!(server.join().unwrap().len(), 2);
    core.stop();
}

#[test]
fn a_drop_after_a_saved_call_keeps_it_and_continues() {
    let (base, server) = server(vec![
        vec![call("c1", json!({})), Step::Pause(200), Step::Drop],
        vec![text("done"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    assert_eq!(core.next()["ev"], "tool_call");
    assert_eq!(core.next()["ev"], "cut_off");
    core.result("c1|fc_c1", "ok");
    core.until("turn_end");
    let ctx = core.context();
    assert_eq!(
        roles(&ctx),
        ["user", "assistant", "user", "toolResult", "assistant"]
    );
    assert!(
        ctx[2]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cut off")
    );
    assert_eq!(
        server.join().unwrap().len(),
        2,
        "no retry from scratch: the call is not repeated"
    );
    core.stop();
}

#[test]
fn async_calls_do_not_hold_the_next_request() {
    let (base, server) = server(vec![
        vec![async_call("c1"), done()],
        vec![text("working on it"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    assert_eq!(core.next()["async"], true);
    core.until("turn_end");
    core.result("c1|fc_c1", "late");
    let ctx = core.context();
    assert_eq!(
        roles(&ctx),
        ["user", "assistant", "assistant", "toolResult"],
        "the late result is appended when it arrives"
    );
    server.join().unwrap();
    core.stop();
}

#[test]
fn abort_cancels_unfinished_calls_and_ignores_late_results() {
    let (base, _server) = server(vec![vec![call("c1", json!({})), Step::Pause(3000), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    assert_eq!(core.next()["ev"], "tool_call");
    core.op(json!({"op": "abort"}));
    assert_eq!(core.next()["calls"], json!(["c1|fc_c1"]));
    assert_eq!(core.next()["stopReason"], "aborted");
    core.result("c1|fc_c1", "too late");
    let ctx = core.context();
    assert_eq!(roles(&ctx), ["user", "assistant", "toolResult"]);
    assert_eq!(ctx[2]["content"][0]["text"], "Cancelled.");
    core.stop();
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn built_in_calls_run_inside_the_core() {
    let (base, server) = server(vec![
        vec![
            named_call("c1", "bash", json!({"command": "echo hi", "timeout": null})),
            done(),
        ],
        vec![text("ok"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "run it"}}));
    let call = core.next();
    assert_eq!(
        (&call["ev"], &call["builtin"]),
        (&json!("tool_call"), &json!(true))
    );
    let result = core.until("tool_result");
    assert_eq!(result["result"]["content"][0]["text"], "hi\n");
    core.until("turn_end");
    let ctx = core.context();
    assert_eq!(
        roles(&ctx),
        ["user", "assistant", "toolResult", "assistant"]
    );
    assert_eq!(ctx[2]["content"][0]["text"], "hi\n");
    let bodies = server.join().unwrap();
    assert_eq!(
        tool_names(&bodies[0]),
        ["read", "bash", "edit", "write", "grep", "find", "ls"]
    );
    assert_eq!(bodies[0]["tools"][1]["strict"], true);
    core.stop();
}

#[test]
fn a_runner_tool_replaces_a_built_in_and_no_tool_turns_one_off() {
    let (base, server) = server(vec![vec![text("hi"), done()]]);
    let d = dir();
    let mine = json!([{"type": "function", "name": "read", "description": "runner read", "parameters": {"type": "object"}}]);
    let mut core = Core::start_with(&d, &base, &["--no-tool", "bash", "--no-tool", "ls"], mine);
    core.op(json!({"op": "send", "input": {"text": "hi"}}));
    core.until("turn_end");
    let bodies = server.join().unwrap();
    assert_eq!(
        tool_names(&bodies[0]),
        ["edit", "write", "grep", "find", "read"]
    );
    assert_eq!(bodies[0]["tools"][4]["description"], "runner read");
    core.stop();
}

#[test]
fn abort_kills_a_running_built_in() {
    let d = dir();
    let marker = std::env::temp_dir().join(format!(
        "core-marker-{}",
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let command = format!("sleep 1.5; touch {}", marker.display());
    let (base, _server) = server(vec![vec![
        named_call("c1", "bash", json!({"command": command})),
        done(),
    ]]);
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    assert_eq!(core.next()["builtin"], true);
    core.op(json!({"op": "abort"}));
    assert_eq!(core.until("turn_end")["stopReason"], "aborted");
    std::thread::sleep(Duration::from_millis(2500));
    assert!(!marker.exists(), "the shell's process group was killed");
    let ctx = core.context();
    assert_eq!(ctx[2]["content"][0]["text"], "Cancelled.");
    core.stop();
}

/// A models file giving model "m" this window.
fn models_file(window: u64) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "core-models-{}-{}.json",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::write(
        &path,
        json!({"models": {"m": {"contextWindow": window}}}).to_string(),
    )
    .unwrap();
    path
}

fn last_input_text(body: &Value) -> String {
    let input = body["input"].as_array().unwrap();
    input.last().unwrap()["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[test]
fn every_request_ends_with_the_context_usage_line_which_is_never_saved() {
    let (base, server) = server(vec![vec![text("one"), done()], vec![text("two"), done()]]);
    let d = dir();
    let models = models_file(272_000);
    let mut core = Core::start_with(
        &d,
        &base,
        &["--models", models.to_str().unwrap()],
        json!([]),
    );
    core.op(json!({"op": "send", "input": {"text": "hi"}}));
    core.until("turn_end");
    core.op(json!({"op": "send", "input": {"text": "again"}}));
    core.until("turn_end");
    let bodies = server.join().unwrap();
    assert_eq!(
        last_input_text(&bodies[0]),
        "Context: about 0k of 272k tokens (estimated)"
    );
    // The fake server reports 12 tokens for the first reply.
    assert_eq!(
        last_input_text(&bodies[1]),
        "Context: about 0k of 272k tokens (0k measured + ~0k new)"
    );
    let ctx = core.context();
    assert!(
        ctx.iter()
            .all(|m| !m.to_string().contains("Context: about")),
        "the line is not saved"
    );
    let _ = std::fs::remove_file(models);
    core.stop();
}

#[test]
fn the_overflow_guard_sets_aside_old_tool_results() {
    let (base, server) = server(vec![
        vec![call("c1", json!({})), done_unmeasured()],
        vec![call("c2", json!({})), done_unmeasured()],
        vec![text("ok"), done()],
    ]);
    let d = dir();
    let models = models_file(1_500);
    let mut core = Core::start_with(
        &d,
        &base,
        &["--models", models.to_str().unwrap()],
        json!([]),
    );
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    core.until("tool_call");
    core.result("c1|fc_c1", &"a".repeat(4000));
    core.until("tool_call");
    core.result("c2|fc_c2", &"b".repeat(4000));
    core.until("turn_end");
    let bodies = server.join().unwrap();
    let outputs: Vec<String> = bodies[2]["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "function_call_output")
        .map(|i| i["output"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        outputs[0]
            .starts_with("[Tool result set aside to fit the context window: add call c1|fc_c1"),
        "{}",
        outputs[0]
    );
    assert!(
        outputs[0].contains("log/000001.payload.jsonl, key "),
        "{}",
        outputs[0]
    );
    assert_eq!(outputs[1].len(), 4000, "the newest result is still sent");
    let ctx = core.context();
    assert_eq!(
        ctx[2]["content"][0]["text"].as_str().map(str::len),
        Some(4000),
        "history is unchanged"
    );
    let _ = std::fs::remove_file(models);
    core.stop();
}

#[test]
fn a_request_that_cannot_fit_is_not_sent() {
    let (base, server) = server(vec![]);
    let d = dir();
    let models = models_file(100);
    let mut core = Core::start_with(
        &d,
        &base,
        &["--models", models.to_str().unwrap()],
        json!([]),
    );
    core.op(json!({"op": "send", "input": {"text": "x".repeat(4000)}}));
    let ev = core.until("context_too_large");
    assert_eq!(ev["window"], 100);
    assert_eq!(core.until("turn_end")["stopReason"], "error");
    assert!(server.join().unwrap().is_empty(), "nothing was sent");
    let _ = std::fs::remove_file(models);
    core.stop();
}

#[test]
fn referenced_lists_the_files_the_context_points_to() {
    let (base, server) = server(vec![
        vec![call("c1", json!({"a": 1})), done()],
        vec![text("ok"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    core.until("tool_call");
    core.result("c1|fc_c1", "3");
    core.until("turn_end");
    server.join().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_core"))
        .args(["referenced", d.to_str().unwrap()])
        .output()
        .unwrap();
    let files: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(files, json!(["log/000001.payload.jsonl"]));
    core.stop();
}

#[test]
fn a_new_context_waits_while_a_cleanup_holds_the_references_lock() {
    use std::os::fd::AsRawFd;
    let (base, server) = server(vec![vec![text("one"), done()], vec![text("two"), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "hi"}}));
    core.until("turn_end");
    // A cleanup takes the lock; the next turn's context check must wait for it.
    let lock = std::fs::File::open(d.join("references.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    core.op(json!({"op": "send", "input": {"text": "again"}}));
    assert!(
        core.events
            .recv_timeout(Duration::from_millis(700))
            .is_err(),
        "nothing happens while the lock is held"
    );
    drop(lock);
    core.until("turn_end");
    assert_eq!(server.join().unwrap().len(), 2);
    core.stop();
}

#[test]
fn injected_messages_and_entries_like_pi_custom_messages() {
    let (base, server) = server(vec![
        vec![call("c1", json!({})), done()],
        vec![text("ok"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    // Idle, not waking (an internal note): added at once, starts no turn.
    core.op(json!({"op": "add_context", "id": "a1", "wake": false, "message": {"customType": "family", "content": "hello from a child", "display": true}}));
    assert_eq!(core.until("added")["id"], "a1");
    core.op(json!({"op": "record", "id": "e1", "ns": "harness.usage", "data": {"x": 1}}));
    assert_eq!(core.until("recorded")["id"], "e1");
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    core.until("tool_call");
    // Mid-turn: held until the next safe point (after the call's result).
    core.op(json!({"op": "add_context", "id": "a2", "message": {"customType": "reminder", "content": "late note"}}));
    core.result("c1|fc_c1", "3");
    core.until("turn_end");
    let ctx = core.context();
    assert_eq!(
        roles(&ctx),
        [
            "user",
            "user",
            "assistant",
            "toolResult",
            "user",
            "assistant"
        ]
    );
    assert_eq!(ctx[0]["content"][0]["text"], "hello from a child");
    assert_eq!(ctx[4]["content"][0]["text"], "late note");
    let bodies = server.join().unwrap();
    assert!(
        bodies[1].to_string().contains("late note"),
        "the note reached the next request"
    );
    // History shows the bookkeeping entry and the injected message's source.
    core.op(json!({"op": "history", "id": "h1", "from": 1, "to": 4, "resolve": true}));
    let h = core.until("history");
    let kinds: Vec<&str> = h["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["k"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["chunk", "user", "app", "user"]);
    assert_eq!(h["records"][1]["source"], "family");
    assert_eq!(h["records"][2]["data"], json!({"x": 1}));
    core.stop();
}

#[test]
fn an_incoming_message_wakes_an_idle_session() {
    let (base, server) = server(vec![vec![text("hi child"), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "add_context", "message": {"customType": "family", "content": "message from a child"}}));
    core.until("turn_end");
    assert_eq!(roles(&core.context()), ["user", "assistant"]);
    assert_eq!(server.join().unwrap().len(), 1);
    core.stop();
}

#[test]
fn a_message_during_a_turn_gets_a_reply_in_that_turn() {
    let (base, server) = server(vec![
        vec![text("first"), Step::Pause(500), done()],
        vec![text("about the note"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    core.op(json!({"op": "add_context", "message": {"customType": "family", "content": "note"}}));
    core.until("turn_end");
    let ctx = core.context();
    assert_eq!(roles(&ctx), ["user", "assistant", "user", "assistant"]);
    assert_eq!(ctx[2]["content"][0]["text"], "note");
    assert_eq!(server.join().unwrap().len(), 2);
    core.stop();
}

#[test]
fn a_follow_up_waits_for_the_turn_to_end_then_starts_one() {
    let (base, server) = server(vec![
        vec![call("c1", json!({})), done()],
        vec![text("done"), done()],
        vec![text("about the follow-up"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    core.until("tool_call");
    core.op(json!({"op": "add_context", "deliver": "follow_up", "message": {"customType": "family", "content": "later"}}));
    core.result("c1|fc_c1", "3");
    core.until("turn_end");
    core.until("turn_end");
    let ctx = core.context();
    assert_eq!(
        roles(&ctx),
        [
            "user",
            "assistant",
            "toolResult",
            "assistant",
            "user",
            "assistant"
        ]
    );
    assert_eq!(ctx[4]["content"][0]["text"], "later");
    assert_eq!(server.join().unwrap().len(), 3);
    core.stop();
}

#[test]
fn set_model_is_recorded_and_offered_to_the_runner() {
    let (base, server) = server(vec![vec![text("hi"), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "set_model", "id": "s1", "provider": "openai-codex", "model": "gpt-6.1-sol", "reasoning": "high"}));
    let set = core.until("model_set");
    assert_eq!(
        (&set["model"]["id"], &set["reasoning"]),
        (&json!("gpt-6.1-sol"), &json!("high"))
    );
    core.op(json!({"op": "send", "input": {"text": "hi"}}));
    let ev = core.events.recv_timeout(Duration::from_secs(20)).unwrap();
    assert_eq!(ev["ev"], "prepare_request");
    assert_eq!(ev["selected"]["model"]["id"], "gpt-6.1-sol");
    let model =
        json!({"api": "openai-responses", "provider": "test", "id": "m", "baseUrl": core.base});
    core.op(json!({"op": "prepared", "id": ev["id"], "model": model}));
    core.until("turn_end");
    server.join().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_core"))
        .args(["history", d.to_str().unwrap(), "1", "3"])
        .output()
        .unwrap();
    let records: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(records[1]["type"], "model_change");
    assert_eq!(records[2]["thinkingLevel"], "high");
    core.stop();
}

#[test]
fn a_one_off_completion_is_answered_and_never_saved() {
    let (base, server) = server(vec![vec![text("A short title"), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    let model = json!({"api": "openai-responses", "provider": "test", "id": "m", "baseUrl": base});
    core.op(
        json!({"op": "complete", "id": "t1", "model": model, "instructions": "Title this.",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "a long chat"}]}]}),
    );
    // `until` answers the completion's need_credentials (id "t1") like any other.
    let done = core.until("completed");
    assert_eq!(done["id"], "t1");
    assert_eq!(done["message"]["content"][0]["text"], "A short title");
    assert!(core.context().is_empty(), "nothing was saved");
    let body = &server.join().unwrap()[0];
    assert_eq!(body["tools"], json!([]));
    core.stop();
}

#[test]
fn a_silent_reply_is_given_up_and_retried() {
    let (base, server) = server(vec![
        vec![text("partial"), Step::Pause(8000), done()],
        vec![text("second try"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.extra = json!({"streamStallMs": 2000});
    core.op(json!({"op": "send", "input": {"text": "hi"}}));
    let retry = core.until("retry");
    assert!(
        retry["error"]
            .as_str()
            .unwrap()
            .starts_with("provider_stream_stalled"),
        "{retry}"
    );
    core.until("turn_end");
    let ctx = core.context();
    assert_eq!(ctx[1]["content"][0]["text"], "second try");
    assert_eq!(server.join().unwrap().len(), 2);
    core.stop();
}

#[test]
fn the_runner_can_review_the_exact_request() {
    let (base, server) = server(vec![vec![text("ok"), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.extra = json!({"reviewRequest": true});
    core.show_responses = true;
    core.op(json!({"op": "send", "input": {"text": "hi"}}));
    let review = core.until("review_request");
    let mut body = review["request"]["body"].clone();
    assert_eq!(body["stream"], true);
    assert!(
        review["request"]["headers"]["authorization"]
            .as_str()
            .unwrap()
            .starts_with("Bearer ")
    );
    body["metadata"] = json!({"reviewed": "yes"});
    core.op(json!({"op": "review_request", "id": review["id"], "body": body}));
    let response = core.until("provider_response");
    assert_eq!(response["status"], 200);
    core.until("turn_end");
    assert_eq!(server.join().unwrap()[0]["metadata"]["reviewed"], "yes");
    core.stop();
}

#[test]
fn reasoning_levels_and_fast_mode_reach_the_request_as_pi_sends_them() {
    let (base, server) = server(vec![vec![text("a"), done()], vec![text("b"), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    let map = json!({"off": null, "minimal": "low", "low": "low", "medium": "medium", "high": "high", "xhigh": "xhigh", "max": "max"});
    core.extra =
        json!({"model": {"reasoning": true, "thinkingLevelMap": map}, "serviceTier": "priority"});
    core.op(json!({"op": "send", "input": {"text": "one"}}));
    core.until("turn_end");
    core.extra["reasoning"] = json!("minimal");
    core.op(json!({"op": "send", "input": {"text": "two"}}));
    core.until("turn_end");
    let bodies = server.join().unwrap();
    assert!(
        bodies[0].get("reasoning").is_none(),
        "off is not offered by this model, so nothing is sent"
    );
    assert_eq!(bodies[0]["service_tier"], "priority");
    assert_eq!(
        bodies[1]["reasoning"]["effort"], "low",
        "minimal maps to low"
    );
    core.stop();
}

#[test]
fn the_core_builds_the_changing_part_of_the_system_prompt() {
    let (base, server) = server(vec![vec![text("a"), done()], vec![text("b"), done()]]);
    let d = dir();
    let work = dir();
    let agent = work.join("agent");
    let cwd = work.join("project");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(agent.join("AGENTS.md"), "Global rules.").unwrap();
    std::fs::write(cwd.join("AGENTS.md"), "Project rules.").unwrap();
    let ipython = json!([{"type": "function", "name": "ipython", "description": "Run Python", "parameters": {"type": "object"},
        "promptSnippet": "Run calculations", "promptGuidelines": ["Use the tool as needed."]}]);
    let flags = [
        "--agent-dir",
        agent.to_str().unwrap(),
        "--no-tool",
        "read",
        "--no-tool",
        "bash",
        "--no-tool",
        "edit",
        "--no-tool",
        "write",
        "--no-tool",
        "grep",
        "--no-tool",
        "find",
        "--no-tool",
        "ls",
    ];
    let mut core = Core::start_in(&d, &base, &flags, ipython, &cwd);
    core.extra = json!({"instructions": "You are an assistant.", "sections": {"scratch": "Use /tmp."},
        "model": {"api": "openai-codex-responses"}, "transport": "sse"});
    core.op(json!({"op": "send", "input": {"text": "one"}}));
    core.until("turn_end");
    // A runner that wants only its own text turns the core's part off; outside Codex
    // the text is the first input message.
    core.extra =
        json!({"instructions": "Only this.", "basePrompt": false, "model": {"reasoning": true}});
    core.op(json!({"op": "send", "input": {"text": "two"}}));
    core.until("turn_end");
    let paths: Vec<String> = core.prepares[0]["contextFiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        paths,
        [
            agent.join("AGENTS.md").to_string_lossy(),
            cwd.join("AGENTS.md").to_string_lossy()
        ]
    );
    let bodies = server.join().unwrap();
    let expected = format!(
        "You are an assistant.\n\n<tools>\n- ipython: Run calculations\n</tools>\n\n<rules>\n- Use the tool as needed.\n</rules>\n\n<project_context>\nProject-specific instructions and guidelines:\n\n<project_instructions path=\"{}\">\nGlobal rules.\n</project_instructions>\n\n<project_instructions path=\"{}\">\nProject rules.\n</project_instructions>\n</project_context>\n\n<cwd>\n{}\n</cwd>\n\n<scratch>\nUse /tmp.\n</scratch>",
        agent.join("AGENTS.md").display(),
        cwd.join("AGENTS.md").display(),
        cwd.display()
    );
    assert_eq!(bodies[0]["instructions"], expected);
    assert_eq!(
        bodies[0]["tools"],
        json!([{"type": "function", "name": "ipython", "description": "Run Python", "parameters": {"type": "object"}}])
    );
    assert!(bodies[1].get("instructions").is_none());
    assert_eq!(
        bodies[1]["input"][0],
        json!({"role": "developer", "content": "Only this."})
    );
    core.stop();
}

#[test]
fn a_reply_reports_its_usage_cost_and_how_full_the_context_is() {
    let usage = json!({"input_tokens": 3000, "output_tokens": 400, "total_tokens": 3400,
        "input_tokens_details": {"cached_tokens": 2000, "cache_write_tokens": 500},
        "output_tokens_details": {"reasoning_tokens": 300}});
    let (base, server) = server(vec![vec![
        text("hi"),
        Step::Event(
            json!({"type": "response.completed", "response": {"output": [], "usage": usage, "service_tier": "default"}}),
        ),
    ]]);
    let d = dir();
    let models = d.with_extension("models.json");
    std::fs::write(&models, r#"{"models": {"m": {"contextWindow": 10000, "cost": {"input": 2, "output": 10, "cacheRead": 0.1, "cacheWrite": 2.5}}}}"#).unwrap();
    let mut core = Core::start_with(
        &d,
        &base,
        &["--models", models.to_str().unwrap()],
        json!([]),
    );
    core.extra = json!({"serviceTier": "priority", "model": {"api": "openai-codex-responses"}, "transport": "sse"});
    core.op(json!({"op": "send", "input": {"text": "hi"}}));
    let end = core.until("message_end");
    let u = &end["usage"];
    assert_eq!(
        (
            u["input"].clone(),
            u["cacheRead"].clone(),
            u["cacheWrite"].clone(),
            u["reasoning"].clone(),
            u["totalTokens"].clone()
        ),
        (json!(500), json!(2000), json!(500), json!(300), json!(3400))
    );
    // Codex said "default" for a priority request: priced double, as the harness does.
    let expected = ((2.0 / 1_000_000.0) * 500.0
        + (10.0 / 1_000_000.0) * 400.0
        + (0.1 / 1_000_000.0) * 2000.0
        + (2.5 * 500.0 + 2.0 * 2.0 * 0.0) / 1_000_000.0)
        * 2.0;
    assert_eq!(u["cost"]["total"], json!(expected));
    assert_eq!(
        end["contextUsage"],
        json!({"tokens": 3400, "contextWindow": 10000, "percent": 34.0})
    );
    core.until("turn_end");
    let ctx = core.context();
    assert_eq!(
        ctx[1]["usage"], end["usage"],
        "the saved reply carries the same usage"
    );
    drop(server);
    let _ = std::fs::remove_file(models);
    core.stop();
}

#[test]
fn a_provider_error_is_reported_and_saved_as_fixed_codes() {
    let (base, server) = server(vec![
        vec![Step::Event(
            json!({"type": "error", "code": "context_length_exceeded", "message": "secret request text here"}),
        )],
        vec![Step::Event(
            json!({"type": "response.failed", "response": {"status": "failed", "error": {"code": "weird_new_code", "type": "server_error", "message": "x"}}}),
        )],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "one"}}));
    let first = core.until("error");
    assert_eq!(first["error"], "context_length_exceeded");
    assert_eq!(
        first["providerError"],
        json!({"version": 2, "code": "context_length_exceeded", "category": "context", "event": "error",
        "codeState": "known", "typeState": "absent", "errorShape": "missing", "messageShape": "string"})
    );
    core.until("turn_end");
    core.op(json!({"op": "send", "input": {"text": "two"}}));
    let second = core.until("error");
    assert_eq!(second["error"], "provider_response_failed");
    assert_eq!(
        (
            second["providerError"]["code"].clone(),
            second["providerError"]["category"].clone(),
            second["providerError"]["codeState"].clone()
        ),
        (json!("unknown"), json!("server"), json!("unknown"))
    );
    core.until("turn_end");
    // The failed replies are saved, as pi saves them, but never sent to the model.
    assert_eq!(
        roles(&core.context()),
        ["user", "assistant", "user", "assistant"]
    );
    core.op(json!({"op": "history", "id": "h", "from": first["n"], "to": first["n"]}));
    let saved = &core.until("history")["records"][0]["m"];
    assert_eq!(
        (saved["stopReason"].clone(), saved["errorMessage"].clone()),
        (json!("error"), json!("context_length_exceeded"))
    );
    assert!(
        !saved.to_string().contains("secret"),
        "the provider's text is not kept"
    );
    let bodies = server.join().unwrap();
    assert!(
        bodies[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["role"] != "assistant")
    );
    core.stop();
}

#[test]
fn model_headers_are_merged_over_the_defaults_as_the_harness_does() {
    let (base, server) = server(vec![vec![text("a"), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    // grok-cli's model headers (pi's model.headers); a null removes a default.
    core.extra = json!({"reviewRequest": true, "model": {"provider": "grok-cli",
        "headers": {"User-Agent": "grok-shell/1", "x-grok-model-override": "m", "accept": null}}});
    core.op(json!({"op": "send", "input": {"text": "hi"}}));
    let review = core.until("review_request");
    let headers = review["request"]["headers"].clone();
    assert_eq!(
        headers,
        json!({"content-type": "application/json", "authorization": "Bearer t",
        "session_id": headers["session_id"], "user-agent": "grok-shell/1", "x-grok-model-override": "m"})
    );
    assert!(headers["session_id"].is_string());
    core.op(json!({"op": "review_request", "id": review["id"]}));
    core.until("turn_end");
    drop(server);
    core.stop();
}

#[test]
fn an_openrouter_turn_runs_a_tool_call_over_chat_completions() {
    let chunk = |c: Value| Step::Event(json!({"id": "gen-1", "choices": [c]}));
    let (base, server) = server(vec![
        vec![
            chunk(json!({"delta": {"reasoning": "let me add"}})),
            chunk(
                json!({"delta": {"tool_calls": [{"index": 0, "id": "t1", "function": {"name": "add", "arguments": "{\"a\":"}}]}}),
            ),
            chunk(
                json!({"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "2}"}}]}, "finish_reason": "tool_calls"}),
            ),
            Step::Event(
                json!({"id": "gen-1", "choices": [], "usage": {"prompt_tokens": 20, "completion_tokens": 5, "cost": 0.25, "is_byok": false}}),
            ),
        ],
        vec![chunk(
            json!({"delta": {"content": "It is 2."}, "finish_reason": "stop"}),
        )],
    ]);
    let d = dir();
    // OpenRouter's list as the core caches it: the runner names only provider and id.
    let catalog = d.with_extension("catalog");
    std::fs::create_dir_all(&catalog).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    std::fs::write(catalog.join("openrouter.json"), json!({"checkedAt": now, "models": [{"id": "z-ai/glm-5.3",
        "api": "openai-completions", "provider": "openrouter", "baseUrl": "https://unused.invalid", "reasoning": true,
        "input": ["text"], "contextWindow": 200000, "maxTokens": 1000}]}).to_string()).unwrap();
    let tools = json!([{"type": "function", "name": "add", "description": "Add", "parameters": {"type": "object"}}]);
    let flags = ["--catalog", catalog.to_str().unwrap()];
    let mut core = Core::start_in(&d, &base, &flags, tools, &std::env::current_dir().unwrap());
    core.extra = json!({"instructions": "Be brief.", "model": {"provider": "openrouter", "id": "z-ai/glm-5.3", "api": null}});
    core.op(json!({"op": "send", "input": {"text": "add 2"}}));
    let call = core.until("tool_call");
    assert_eq!(
        (call["name"].clone(), call["arguments"].clone()),
        (json!("add"), json!({"a": 2}))
    );
    let end = core.next();
    assert_eq!(end["ev"], "message_end");
    assert_eq!(
        end["usage"]["cost"]["total"], 0.25,
        "OpenRouter's billed cost"
    );
    core.result(call["call"].as_str().unwrap(), "2");
    let last = core.until("turn_end");
    assert_eq!(last["stopReason"], "stop");
    let bodies = server.join().unwrap();
    assert_eq!(bodies[0]["messages"][0]["role"], "system");
    assert!(
        bodies[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("Be brief.\n\n<tools>")
    );
    assert_eq!(bodies[0]["reasoning"], json!({"effort": "none"}));
    assert_eq!(
        bodies[0]["tools"][7]["function"]["name"], "add",
        "after the seven built-ins"
    );
    let second = &bodies[1]["messages"];
    assert_eq!(second[2]["tool_calls"][0]["id"], "t1");
    assert_eq!(
        second[3],
        json!({"role": "tool", "content": "2", "tool_call_id": "t1"})
    );
    let ctx = core.context();
    assert_eq!(ctx[1]["content"][0]["type"], "thinking");
    core.stop();
}

#[test]
fn a_model_that_cannot_switch_reasoning_off_is_learned() {
    let refused = r#"{"error":{"message":"Reasoning is mandatory for this endpoint and cannot be disabled.","code":400}}"#;
    let chunk = |t: &str| {
        Step::Event(json!({"choices": [{"delta": {"content": t}, "finish_reason": "stop"}]}))
    };
    let (base, server) = server(vec![
        vec![Step::Status(400, refused.into())],
        vec![chunk("a")],
        vec![chunk("b")],
    ]);
    let d = dir();
    let catalog = d.with_extension("catalog");
    std::fs::create_dir_all(&catalog).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    std::fs::write(catalog.join("openrouter.json"), json!({"checkedAt": now, "models": [{"id": "z/flash",
        "api": "openai-completions", "provider": "openrouter", "reasoning": true, "input": ["text"]}]}).to_string()).unwrap();
    let flags = ["--catalog", catalog.to_str().unwrap()];
    let mut core = Core::start_in(
        &d,
        &base,
        &flags,
        json!([]),
        &std::env::current_dir().unwrap(),
    );
    core.extra = json!({"model": {"provider": "openrouter", "id": "z/flash", "api": null}});
    core.op(json!({"op": "send", "input": {"text": "one"}}));
    assert_eq!(core.until("retry")["error"], "reasoning_mandatory");
    assert_eq!(core.until("turn_end")["stopReason"], "stop");
    core.op(json!({"op": "send", "input": {"text": "two"}}));
    assert_eq!(core.until("turn_end")["stopReason"], "stop");
    let bodies = server.join().unwrap();
    assert_eq!(bodies[0]["reasoning"], json!({"effort": "none"}));
    assert!(
        bodies[1].get("reasoning").is_none(),
        "sent again without it"
    );
    assert!(
        bodies[2].get("reasoning").is_none(),
        "and remembered for the next request"
    );
    core.stop();
}

fn codex(core: &mut Core) {
    core.extra = json!({"model": {"api": "openai-codex-responses"}});
}

#[test]
fn codex_requests_share_one_websocket_and_each_carries_the_whole_context() {
    let (base, server) = ws_server(vec![vec![text("a"), done()], vec![text("b"), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    codex(&mut core);
    core.op(json!({"op": "send", "input": {"text": "one"}}));
    let first = core.until("turn_end");
    assert_eq!(first["stopReason"], "stop", "{first}");
    core.op(json!({"op": "send", "input": {"text": "two"}}));
    core.until("turn_end");
    let (frames, sockets) = server.join().unwrap();
    assert_eq!(
        sockets,
        ["Bearer t"],
        "one socket for both requests, with the usual headers"
    );
    assert!(
        frames
            .iter()
            .all(|f| f["type"] == "response.create" && f.get("previous_response_id").is_none())
    );
    let second = frames[1]["input"].to_string();
    assert!(
        second.contains("one") && second.contains("two"),
        "the whole context is sent again"
    );
    assert_eq!(
        roles(&core.context()),
        ["user", "assistant", "user", "assistant"]
    );
    core.stop();
}

#[test]
fn a_rejected_or_closed_websocket_is_replaced_by_a_new_one() {
    let rejected = json!({"type": "error", "code": "websocket_connection_limit_reached"});
    let (base, server) = ws_server(vec![
        vec![Step::Event(rejected), Step::Drop],
        vec![text("a"), done(), Step::Drop],
        vec![text("b"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    codex(&mut core);
    core.op(json!({"op": "send", "input": {"text": "one"}}));
    assert_eq!(core.until("turn_end")["stopReason"], "stop");
    // The server closed the kept socket after the first reply: the next request finds it
    // closed and opens a new one, without a retry or an error.
    core.op(json!({"op": "send", "input": {"text": "two"}}));
    let end = core.until("turn_end");
    assert_eq!(end["stopReason"], "stop", "{end}");
    let (frames, sockets) = server.join().unwrap();
    assert_eq!((frames.len(), sockets.len()), (3, 3));
    core.stop();
}

#[test]
fn without_a_websocket_a_codex_request_goes_over_http() {
    // The first connection is the refused upgrade; the request then comes over HTTP.
    let (base, server) = server(vec![vec![], vec![text("a"), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    codex(&mut core);
    core.op(json!({"op": "send", "input": {"text": "one"}}));
    assert_eq!(core.until("turn_end")["stopReason"], "stop");
    let bodies = server.join().unwrap();
    assert_eq!(bodies[0], Value::Null);
    assert_eq!(bodies[1]["stream"], true);
    core.stop();
}

#[test]
fn an_item_listed_only_in_the_final_message_is_not_lost() {
    let late = json!({"type": "function_call", "id": "fc_c1", "call_id": "c1", "name": "add", "arguments": "{\"a\":1}", "status": "completed"});
    let (base, server) = server(vec![
        vec![Step::Event(
            json!({"type": "response.completed", "response": {"output": [late], "usage": {"input_tokens": 5, "output_tokens": 1}}}),
        )],
        vec![text("done"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    let call = core.until("tool_call");
    assert_eq!(call["arguments"], json!({"a": 1}));
    core.result("c1|fc_c1", "1");
    core.until("turn_end");
    assert_eq!(server.join().unwrap().len(), 2);
    core.stop();
}

#[test]
fn after_a_crash_a_saved_call_gets_an_interrupted_result_and_is_not_rerun() {
    let (base, _server) = server(vec![vec![call("c1", json!({})), Step::Pause(5000)]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    assert_eq!(core.next()["ev"], "tool_call");
    core.child.kill().unwrap();
    let _ = core.child.wait();
    let (base2, _s2) = server(vec![]);
    let mut again = Core::start(&d, &base2);
    let ctx = again.context();
    assert_eq!(roles(&ctx), ["user", "assistant", "toolResult"]);
    assert!(
        ctx[2]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("may or may not have run")
    );
    assert_eq!(ctx[1]["stopReason"], "error", "the open reply was closed");
    again.stop();
}
