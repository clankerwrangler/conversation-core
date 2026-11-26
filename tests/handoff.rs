//! The tool hand-off end to end: the real `core` binary against a fake Responses
//! server that streams scripted replies and can drop the connection.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
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
    /// Return `reasoning_delta` events instead of skipping them.
    show_reasoning: bool,
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
            show_reasoning: false,
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
                Some("reasoning_delta") if self.show_reasoning => return ev,
                Some("text_delta")
                | Some("reasoning_delta")
                | Some("window_unknown")
                | Some("provider_response")
                | Some("turn_start") => {}
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
fn a_session_has_one_writing_core_at_a_time() {
    let d = dir();
    let mut first = Core::start(&d, "http://127.0.0.1:9");
    first.op(json!({"op": "context"}));
    first.until("context");
    let second = Command::new(env!("CARGO_BIN_EXE_core"))
        .args(["run", d.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        !second.status.success(),
        "a second core on the same session stops at once"
    );
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("another core is writing this session")
    );
    // A core that ends without closing (a crash) leaves the session free.
    first.child.kill().unwrap();
    first.child.wait().unwrap();
    let mut third = Core::start(&d, "http://127.0.0.1:9");
    third.op(json!({"op": "context"}));
    third.until("context");
    third.stop();
}
#[test]
fn reasoning_summary_parts_stream_with_a_break_after_each_as_pi_streams_them() {
    let summary = |index: u64, delta: &str| {
        Step::Event(
            json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "summary_index": index, "delta": delta}),
        )
    };
    let part_done = |index: u64, text: &str| {
        Step::Event(
            json!({"type": "response.reasoning_summary_part.done", "item_id": "rs_1", "summary_index": index, "part": {"type": "summary_text", "text": text}}),
        )
    };
    let (base, server) = server(vec![vec![
        summary(0, "**Checking"),
        summary(0, " files**"),
        part_done(0, "**Checking files**"),
        summary(1, "**Saving**"),
        part_done(1, "**Saving**"),
        item_done(
            json!({"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "**Checking files**"}, {"type": "summary_text", "text": "**Saving**"}]}),
        ),
        text("done"),
        done(),
    ]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.show_reasoning = true;
    core.op(json!({"op": "send", "input": {"text": "hi"}}));
    let mut streamed = String::new();
    loop {
        let ev = core.next();
        match ev["ev"].as_str() {
            Some("reasoning_delta") => streamed.push_str(ev["delta"].as_str().unwrap()),
            Some("turn_end") => break,
            _ => {}
        }
    }
    assert_eq!(
        streamed, "**Checking files**\n\n**Saving**\n\n",
        "a reader of the live summary sees where each heading ends"
    );
    let ctx = core.context();
    let thinking = ctx[1]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "thinking")
        .expect("the summary is saved");
    assert_eq!(
        thinking["thinking"], "**Checking files**\n\n**Saving**",
        "the saved summary is unchanged"
    );
    server.join().unwrap();
    core.stop();
}
#[test]
fn reply_text_is_saved_with_when_its_first_words_arrived() {
    let (base, server) = server(vec![vec![
        Step::Pause(400),
        Step::Event(
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "Hel"}),
        ),
        Step::Pause(400),
        text("Hello"),
        done(),
    ]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "hi"}}));
    core.until("turn_end");
    let ctx = core.context();
    let reply = &ctx[1];
    let began = reply["timestamp"].as_u64().unwrap();
    let started = reply["content"][0]["startedAt"]
        .as_u64()
        .expect("the text keeps its start");
    assert!(
        started >= began + 300,
        "not when the request began ({began}), but at the first words ({started})"
    );
    assert!(
        started + 300 <= began + 800,
        "not when the text finished ({started})"
    );
    server.join().unwrap();
    core.stop();
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
fn repeated_completed_call_is_dispatched_and_saved_once() {
    let marker = std::env::temp_dir().join(format!(
        "core-dedup-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let args = json!({"command": format!("printf 'x\\n' >> {}", marker.display())});
    let (base, server) = server(vec![
        vec![
            named_call("c1", "bash", args.clone()),
            Step::Pause(400),
            named_call("c1", "bash", args.clone()),
            named_call("c2", "bash", args),
            done(),
        ],
        vec![text("done"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    let mut calls = Vec::new();
    loop {
        let event = core.next();
        if event["ev"] == "tool_call" {
            calls.push(event["call"].as_str().unwrap().to_string());
        }
        if event["ev"] == "settled" {
            break;
        }
    }
    assert_eq!(calls, ["c1|fc_c1", "c2|fc_c2"]);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap().lines().count(),
        2,
        "one side effect per distinct call, including when the duplicate follows completion"
    );
    let ctx = core.context();
    let blocks: Vec<_> = ctx
        .iter()
        .filter(|m| m["role"] == "assistant")
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .filter(|b| b["type"] == "toolCall")
        .collect();
    assert_eq!(
        blocks.len(),
        2,
        "the duplicate is not persisted as a second call"
    );
    let bodies = server.join().unwrap();
    assert_eq!(
        bodies[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|x| x["type"] == "function_call")
            .count(),
        2
    );
    let _ = std::fs::remove_file(marker);
    core.stop();
}

#[test]
fn repeated_runner_call_in_later_reply_is_not_handed_off_again() {
    let (base, server) = server(vec![
        vec![call("c1", json!({"a": 1})), done()],
        vec![call("c1", json!({"a": 1})), text("continued"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    assert_eq!(core.until("tool_call")["call"], "c1|fc_c1");
    core.result("c1|fc_c1", "done-once");
    let mut dispatched = 0;
    loop {
        let event = core.next();
        if event["ev"] == "tool_call" {
            dispatched += 1;
        }
        if event["ev"] == "settled" {
            break;
        }
    }
    assert_eq!(dispatched, 0);
    let ctx = core.context();
    assert_eq!(ctx.iter().filter(|m| m["role"] == "toolResult").count(), 1);
    assert_eq!(server.join().unwrap().len(), 2);
    core.stop();
}

#[test]
fn repeated_call_does_not_replace_the_running_cancellation_handle() {
    let marker = std::env::temp_dir().join(format!(
        "core-dedup-cancel-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let args = json!({"command": format!("sleep 1.5; touch {}", marker.display())});
    let (base, _server) = server(vec![vec![
        named_call("c1", "bash", args.clone()),
        named_call("c1", "bash", args),
        Step::Event(json!({"type": "response.output_text.delta", "delta": "barrier"})),
        Step::Pause(3000),
        done(),
    ]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    core.until("tool_call");
    loop {
        let event = core
            .events
            .recv_timeout(Duration::from_secs(5))
            .expect("provider barrier");
        if event["ev"] == "text_delta" && event["delta"] == "barrier" {
            break;
        }
    }
    core.op(json!({"op": "abort"}));
    assert_eq!(core.until("cancel")["calls"], json!(["c1|fc_c1"]));
    assert_eq!(core.until("turn_end")["stopReason"], "aborted");
    std::thread::sleep(Duration::from_millis(2200));
    assert!(
        !marker.exists(),
        "abort must cancel the original running action"
    );
    let ctx = core.context();
    assert_eq!(ctx.iter().filter(|m| m["role"] == "toolResult").count(), 1);
    let _ = std::fs::remove_file(marker);
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
fn async_result_after_settle_wakes_one_follow_up() {
    let (base, server) = server(vec![
        vec![async_call("c1"), done()],
        vec![text("working on it"), done()],
        vec![text("late result received"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    assert_eq!(core.next()["async"], true);
    core.until("settled");
    core.result("c1|fc_c1", "late-output");
    core.until("settled");
    // A duplicate result must not wake another turn or change the saved outcome.
    core.result("c1|fc_c1", "duplicate");
    let ctx = core.context();
    assert_eq!(
        roles(&ctx),
        ["user", "assistant", "assistant", "toolResult", "assistant"]
    );
    assert_eq!(ctx[3]["content"][0]["text"], "late-output");
    let bodies = server.join().unwrap();
    let input = bodies[2]["input"].as_array().unwrap();
    let call = input
        .iter()
        .position(|x| x["type"] == "function_call")
        .unwrap();
    let interim = input
        .iter()
        .position(|x| x["role"] == "assistant" && x.to_string().contains("working on it"))
        .unwrap();
    let results: Vec<_> = input
        .iter()
        .enumerate()
        .filter(|(_, x)| x["type"] == "function_call_output")
        .collect();
    assert_eq!(results.len(), 1);
    assert!(call < interim && interim < results[0].0);
    assert_eq!(results[0].1["output"], "late-output");
    core.stop();
}

#[test]
fn async_result_during_a_stream_wakes_follow_up_if_snapshot_missed_it() {
    let (base, server) = server(vec![
        vec![async_call("c1"), done()],
        vec![Step::Pause(500), text("working on it"), done()],
        vec![text("late result received"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.show_responses = true;
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    core.until("tool_call");
    core.until("provider_response"); // second request has already captured its context
    core.result("c1|fc_c1", "busy-output");
    core.until("settled");
    let bodies = server.join().unwrap();
    assert!(!bodies[1].to_string().contains("busy-output"));
    assert_eq!(
        bodies[2]["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|x| x["type"] == "function_call_output" && x["output"] == "busy-output")
            .count(),
        1
    );
    assert_eq!(
        roles(&core.context()),
        ["user", "assistant", "toolResult", "assistant", "assistant"]
    );
    core.stop();
}

#[test]
fn async_result_captured_by_next_request_does_not_wake_an_extra_turn() {
    let (base, server) = server(vec![
        vec![async_call("c1"), Step::Pause(500), done()],
        vec![text("got it"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    core.until("tool_call");
    core.result("c1|fc_c1", "early-output");
    core.until("settled");
    let bodies = server.join().unwrap();
    assert_eq!(
        bodies[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|x| x["type"] == "function_call_output" && x["output"] == "early-output")
            .count(),
        1
    );
    assert_eq!(
        roles(&core.context()),
        ["user", "assistant", "toolResult", "assistant"]
    );
    core.stop();
}

#[test]
fn late_async_builtin_result_wakes_follow_up() {
    let builtin = item_done(
        json!({"type": "function_call", "id": "fc_c1", "call_id": "c1", "name": "bash",
        "arguments": json!({"command": "sleep 1; printf builtin-late"}).to_string(), "async": true}),
    );
    let (base, server) = server(vec![
        vec![builtin, done()],
        vec![text("working"), done()],
        vec![text("received"), done()],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    assert_eq!(core.until("tool_call")["builtin"], true);
    core.until("settled");
    assert_eq!(
        core.until("tool_result")["result"]["content"][0]["text"],
        "builtin-late"
    );
    core.until("settled");
    let bodies = server.join().unwrap();
    assert_eq!(
        bodies[2]["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|x| x["type"] == "function_call_output" && x["output"] == "builtin-late")
            .count(),
        1
    );
    core.stop();
}

#[test]
fn aborted_async_call_ignores_late_result_without_waking() {
    let (base, _server) = server(vec![vec![async_call("c1"), Step::Pause(500), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    core.until("tool_call");
    core.op(json!({"op": "abort"}));
    assert_eq!(core.until("cancel")["calls"], json!(["c1|fc_c1"]));
    assert_eq!(core.until("turn_end")["stopReason"], "aborted");
    core.until("settled");
    core.result("c1|fc_c1", "must-not-appear");
    let ctx = core.context();
    assert_eq!(roles(&ctx), ["user", "assistant", "toolResult"]);
    assert_eq!(ctx[2]["content"][0]["text"], "Cancelled.");
    core.stop();
}

#[test]
fn abort_suppresses_wake_for_an_already_saved_async_result() {
    let (base, _server) = server(vec![vec![async_call("c1"), Step::Pause(500), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    core.until("tool_call");
    core.result("c1|fc_c1", "completed-before-abort");
    core.op(json!({"op": "record", "id": "barrier", "ns": "test", "data": {}}));
    core.until("recorded");
    core.op(json!({"op": "abort"}));
    assert_eq!(core.until("turn_end")["stopReason"], "aborted");
    core.until("settled");
    let ctx = core.context();
    assert_eq!(roles(&ctx), ["user", "assistant", "toolResult"]);
    assert_eq!(ctx[2]["content"][0]["text"], "completed-before-abort");
    core.stop();
}

#[test]
fn late_async_result_after_switch_to_chat_has_valid_order() {
    let (base, server) = server(vec![
        vec![async_call("c1"), done()],
        vec![text("working on it"), done()],
        vec![Step::Event(
            json!({"choices": [{"delta": {"content": "received"}, "finish_reason": "stop"}]}),
        )],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    core.until("tool_call");
    core.until("settled");
    core.extra = json!({"model": {"api": "openai-completions"}});
    core.result("c1|fc_c1", "chat-late-output");
    core.until("settled");
    let bodies = server.join().unwrap();
    let messages = bodies[2]["messages"].as_array().unwrap();
    let call = messages
        .iter()
        .position(|m| m["role"] == "assistant" && m.get("tool_calls").is_some())
        .unwrap();
    assert_eq!(messages[call + 1]["role"], "tool");
    let interim = messages
        .iter()
        .position(|m| m["role"] == "assistant" && m.to_string().contains("working on it"))
        .unwrap();
    let actual: Vec<_> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.to_string().contains("chat-late-output"))
        .collect();
    assert_eq!(actual.len(), 1);
    assert!(call + 1 < interim && interim < actual[0].0);
    assert_eq!(actual[0].1["role"], "user");
    assert!(actual[0].1.to_string().contains("not user-authored"));
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
    // It is a core note in the instruction role, so the model never takes it for the user.
    for body in &bodies {
        let last = body["input"].as_array().unwrap().last().unwrap();
        assert_ne!(last["role"], "user", "the usage line is not a user message");
        assert!(matches!(
            last["role"].as_str(),
            Some("developer" | "system")
        ));
    }
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
fn reap_accepts_only_the_protocol_it_implements_and_refuses_before_deleting() {
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
    let listing = || {
        let mut names: Vec<String> = std::fs::read_dir(d.join("log"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    };
    let before = listing();
    let reap = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_core"))
            .args(["reap", d.to_str().unwrap(), "1"])
            .args(extra)
            .output()
            .unwrap()
    };
    for refused in [
        &["--protocol", "1"][..],
        &["--protocol", "3"],
        &["--protocol"],
        &["--unknown", "x"],
    ] {
        assert!(!reap(refused).status.success(), "{refused:?}");
    }
    assert!(
        before.iter().any(|n| n.ends_with(".payload.jsonl")),
        "stored tool data exists: {before:?}"
    );
    assert_eq!(listing(), before, "a refused cleanup deletes nothing");
    let out = reap(&["--keep-details", "ok", "--protocol", "2"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        result["deleted"],
        json!([]),
        "the current chunk is never touched"
    );
    core.stop();
}

#[test]
fn a_new_context_waits_while_a_cleanup_holds_the_references_lock() {
    use std::os::fd::AsRawFd;
    let (base, server) = server(vec![vec![text("one"), done()], vec![text("two"), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "hi"}}));
    core.until("settled");
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
fn history_is_the_current_chunk_unless_older_is_asked_for() {
    let (base, server) = server(vec![vec![text("one"), done()], vec![text("two"), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.op(json!({"op": "send", "input": {"text": "first"}}));
    core.until("settled");
    let messages = core.context();
    core.op(json!({"op": "checkpoint", "messages": messages}));
    core.until("checkpointed");
    core.op(json!({"op": "send", "input": {"text": "second"}}));
    core.until("settled");
    server.join().unwrap();
    let texts = |entries: &Value| -> Vec<String> {
        entries
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e.pointer("/message/content/0/text").and_then(Value::as_str))
            .map(str::to_string)
            .collect()
    };
    core.op(json!({"op": "history", "id": "h1"}));
    assert_eq!(
        texts(&core.until("history")["entries"]),
        ["second", "two"],
        "current chunk only"
    );
    core.op(json!({"op": "history", "id": "h2", "from": 1}));
    assert_eq!(
        texts(&core.until("history")["entries"]),
        ["first", "one", "second", "two"],
        "older when asked for"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_core"))
        .args(["history", d.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(
        texts(&serde_json::from_slice(&out.stdout).unwrap()),
        ["second", "two"]
    );
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
    core.op(json!({"op": "history", "id": "h1", "from": 1, "to": 4}));
    let h = core.until("history");
    let kinds: Vec<&str> = h["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["custom_message", "custom", "message"]);
    assert_eq!(h["entries"][0]["customType"], "family");
    assert_eq!(
        h["entries"][0]["content"], "hello from a child",
        "text given as text comes back as text"
    );
    assert_eq!(h["entries"][0]["id"], "2");
    assert_eq!(h["entries"][1]["data"], json!({"x": 1}));
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
    assert_eq!(
        core.events.recv_timeout(Duration::from_secs(20)).unwrap()["ev"],
        "turn_start"
    );
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
    assert_eq!(records[0]["type"], "model_change");
    assert_eq!(records[1]["thinkingLevel"], "high");
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
fn body_only_review_never_sends_auth_headers_to_the_runner() {
    let (base, server) = server(vec![vec![text("ok"), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.extra = json!({"reviewRequest": "body"});
    core.op(json!({"op": "send", "input": {"text": "hi"}}));
    let review = core.until("review_request");
    assert!(review["request"].get("headers").is_none());
    assert!(review["request"].get("url").is_none());
    assert_eq!(review["request"]["body"]["stream"], true);
    core.op(json!({"op": "review_request", "id": review["id"]}));
    core.until("turn_end");
    assert_eq!(server.join().unwrap().len(), 1);
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
        "You are an assistant.\n\n<tools>\n- ipython: Run calculations\n</tools>\n\n<rules>\n- Use the tool as needed.\n</rules>\n\n<project_context>\nProject-specific instructions and guidelines:\n\n<project_instructions path=\"{}\">\nGlobal rules.\n</project_instructions>\n\n<project_instructions path=\"{}\">\nProject rules.\n</project_instructions>\n</project_context>\n\n<cwd>\n{}\n</cwd>\n\n<scratch>\nUse /tmp.\n</scratch>\n\nYour context is mirrored at {}. Edit the file to change it; edits take effect at your next request.",
        agent.join("AGENTS.md").display(),
        cwd.join("AGENTS.md").display(),
        cwd.display(),
        d.join("context.jsonl").display()
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
    // Failed replies are saved but never sent to the model.
    assert_eq!(
        roles(&core.context()),
        ["user", "assistant", "user", "assistant"]
    );
    core.op(json!({"op": "history", "id": "h", "from": first["n"], "to": first["n"]}));
    let saved = &core.until("history")["entries"][0]["message"];
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
    // Grok CLI model headers: a null removes a default.
    core.extra = json!({"reviewRequest": true, "model": {"provider": "grok-cli",
        "headers": {"User-Agent": "grok-shell/1", "x-grok-model-override": "m", "accept": null}}});
    core.op(json!({"op": "send", "input": {"text": "hi"}}));
    let review = core.until("review_request");
    let headers = review["request"]["headers"].clone();
    assert_eq!(
        headers,
        json!({"content-type": "application/json", "authorization": "Bearer t",
        "session_id": headers["session_id"], "user-agent": "grok-shell/1", "x-grok-model-override": "m",
        "x-grok-conv-id": headers["session_id"]}),
        "and grok-cli's conversation header"
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

#[test]
fn a_stored_sign_in_is_used_without_asking_the_runner() {
    let (base, server) = server(vec![vec![text("a"), done()]]);
    let d = dir();
    let auth = d.with_extension("auth.json");
    std::fs::write(
        &auth,
        r#"{"test": {"type": "api_key", "key": "stored-key"}}"#,
    )
    .unwrap();
    let mut core = Core::start_with(&d, &base, &["--auth", auth.to_str().unwrap()], json!([]));
    core.extra = json!({"reviewRequest": true});
    core.op(json!({"op": "send", "input": {"text": "hi"}}));
    let review = core.until("review_request");
    assert_eq!(
        review["request"]["headers"]["authorization"],
        "Bearer stored-key"
    );
    core.op(json!({"op": "review_request", "id": review["id"]}));
    core.until("turn_end");
    core.op(json!({"op": "auth_status", "id": "s"}));
    let status = core.until("auth_status");
    assert_eq!(status["signedIn"][0]["provider"], "test");
    assert!(
        !status.to_string().contains("stored-key"),
        "the secret is never reported"
    );
    drop(server);
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

/// A counted fake Responses server that accepts requests until `stop` is set, counting every
/// incoming provider request (even unexpected retries past the scripted replies).
struct CountedServer {
    base: String,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Vec<Value>>>,
}

impl CountedServer {
    fn start(replies: Vec<Vec<Step>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let reqs = Arc::clone(&requests);
        let stop_flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut replies = replies.into_iter();
            let mut bodies = Vec::new();
            let mut writers = Vec::new();
            while !stop_flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        reqs.fetch_add(1, Ordering::SeqCst);
                        bodies.push(read_request(&mut stream));
                        let reply = replies
                            .next()
                            .unwrap_or_else(|| vec![text("unexpected-retry"), done()]);
                        writers.push(std::thread::spawn(move || {
                            if let Some(Step::Status(code, body)) = reply.first() {
                                let _ = stream.write_all(
                                    format!(
                                        "HTTP/1.1 {code} Error\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                        body.len()
                                    )
                                    .as_bytes(),
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
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("accept failed: {e}"),
                }
            }
            for w in writers {
                let _ = w.join();
            }
            bodies
        });
        Self {
            base,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> (usize, Vec<Value>) {
        self.stop.store(true, Ordering::SeqCst);
        let bodies = self.handle.take().unwrap().join().unwrap();
        (self.requests.load(Ordering::SeqCst), bodies)
    }
}

#[test]
fn eof_during_retry_backoff_with_running_builtin_saves_result_without_retrying() {
    let builtin = item_done(json!({
        "type": "function_call",
        "id": "fc_c1",
        "call_id": "c1",
        "name": "bash",
        "arguments": json!({"command": "sleep 1.4; printf builtin-saved"}).to_string(),
        "async": true
    }));
    let srv = CountedServer::start(vec![vec![builtin, done()], vec![Step::Drop]]);
    let d = dir();
    let mut core = Core::start(&d, &srv.base);
    core.op(json!({"op": "send", "input": {"text": "go"}}));
    assert_eq!(core.until("tool_call")["builtin"], true);
    assert_eq!(core.until("retry")["attempt"], 2);
    // Close stdin while the 1.4s async built-in is still running (longer than the 1s backoff).
    let Core {
        mut child, stdin, ..
    } = core;
    drop(stdin);
    let _ = child.wait().unwrap();
    let (count, _bodies) = srv.finish();
    assert_eq!(
        count, 2,
        "must not start a retry after stdin EOF while waiting for a running built-in"
    );
    // Verify the outstanding built-in result was preserved in the session before exiting.
    let mut inspect = Core::start(&d, "http://127.0.0.1:9");
    let ctx = inspect.context();
    assert!(
        ctx.iter().any(|m| {
            m["role"] == "toolResult"
                && m["content"][0]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("builtin-saved"))
        }),
        "running built-in result must be saved before exit: {ctx:?}"
    );
    inspect.stop();
}

#[test]
fn abort_during_retry_backoff_cancels_retry_and_runs_queued_send() {
    let srv = CountedServer::start(vec![vec![Step::Drop], vec![text("after-abort"), done()]]);
    let d = dir();
    let mut core = Core::start(&d, &srv.base);
    core.op(json!({"op": "send", "input": {"text": "first"}}));
    assert_eq!(core.until("retry")["attempt"], 2);
    let started = std::time::Instant::now();
    // Abort during the 1s backoff and queue a separate send while keeping stdin open.
    core.op(json!({"op": "abort"}));
    core.op(json!({"op": "send", "input": {"text": "second"}}));
    let aborted = core.until("turn_end");
    assert_eq!(aborted["stopReason"], "aborted");
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "abort should cancel retry backoff promptly, took {:?}",
        started.elapsed()
    );
    let next = core.until("turn_end");
    assert_ne!(next["stopReason"], "aborted");
    let ctx = core.context();
    assert!(
        ctx.iter().any(|m| {
            m["role"] == "assistant"
                && m["content"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|p| p["text"] == "after-abort"))
        }),
        "queued send should complete after abort: {ctx:?}"
    );
    core.stop();
    let (count, bodies) = srv.finish();
    assert_eq!(
        count, 2,
        "aborted request must not retry; only the queued send runs a second request"
    );
    assert!(
        serde_json::to_string(&bodies[1])
            .unwrap()
            .contains("second"),
        "second request should carry the queued send"
    );
}

fn done_tokens(input: u64, output: u64) -> Step {
    Step::Event(json!({
        "type": "response.completed",
        "response": {
            "usage": {
                "input_tokens": input,
                "output_tokens": output,
                "total_tokens": input + output
            }
        }
    }))
}

#[test]
fn shortened_request_usage_is_not_reused_for_restored_history() {
    let blob = "x".repeat(16_000);
    let (base, server) = server(vec![
        // 1: tool call under tight 1500-token window
        vec![call("c1", json!({})), done_tokens(40, 5)],
        // 2: guard shortens the 16k-byte result; provider returns small 415-token total
        vec![text("saw shortened"), done_tokens(410, 5)],
        // 3: next request under same 1500-token window must count restored result and shorten again
        vec![text("still shortened"), done_tokens(420, 5)],
        // 4: roomier 10,000-token window sends full restored result and proves a new anchor
        vec![text("saw full"), done_tokens(4_150, 50)],
        // 5: subsequent request under same 10,000-token window reuses the proved 4,200 count
        vec![text("anchored"), done_tokens(4_220, 10)],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.extra = json!({"model": {"contextWindow": 1_500}});

    core.op(json!({"op": "send", "input": {"text": "run tool"}}));
    core.until("tool_call");
    core.result("c1|fc_c1", &blob);
    assert_eq!(core.until("turn_end")["stopReason"], "stop");

    core.op(json!({"op": "send", "input": {"text": "continue tight"}}));
    assert_eq!(core.until("turn_end")["stopReason"], "stop");

    core.extra = json!({"model": {"contextWindow": 10_000}});
    core.op(json!({"op": "send", "input": {"text": "continue roomy"}}));
    assert_eq!(core.until("turn_end")["stopReason"], "stop");

    core.op(json!({"op": "send", "input": {"text": "after full"}}));
    assert_eq!(core.until("turn_end")["stopReason"], "stop");

    // Full saved result must remain intact in canonical history.
    let ctx = core.context();
    assert_eq!(
        ctx[2]["content"][0]["text"].as_str(),
        Some(blob.as_str()),
        "saved history must keep the full tool result"
    );
    core.stop();

    let bodies = server.join().unwrap();
    assert_eq!(bodies.len(), 5);
    let output_of = |body: &Value| -> String {
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .and_then(|i| i["output"].as_str())
            .unwrap_or("")
            .to_string()
    };

    // Request 2 shortened the tool result.
    assert!(
        output_of(&bodies[1]).starts_with("[Tool result set aside to fit the context window:"),
        "request 2 should shorten: {}",
        output_of(&bodies[1])
    );
    // Request 3 must NOT trust the 415-token shortened count; it retains request 1's proved
    // 0k anchor, counts the restored ~4k result, and shortens again.
    assert_eq!(
        last_input_text(&bodies[2]),
        "Context: about 4k of 2k tokens (0k measured + ~4k new)"
    );
    assert!(
        output_of(&bodies[2]).starts_with("[Tool result set aside to fit the context window:"),
        "request 3 must shorten the restored result again: {}",
        output_of(&bodies[2])
    );
    // Request 4 with a roomier window retains request 1's proved 0k anchor, counts the
    // restored ~4k result, and sends the full 16k-byte result.
    assert_eq!(
        last_input_text(&bodies[3]),
        "Context: about 4k of 10k tokens (0k measured + ~4k new)"
    );
    assert_eq!(
        output_of(&bodies[3]),
        blob,
        "roomier window should send the full restored result"
    );
    // Request 5 reuses the proved unguarded 4,200-token count from request 4.
    assert_eq!(
        last_input_text(&bodies[4]),
        "Context: about 4k of 10k tokens (4k measured + ~0k new)"
    );
}

#[test]
fn restored_history_declines_to_send_when_it_cannot_fit() {
    let blob = "x".repeat(16_000);
    let (base, server) = server(vec![
        vec![call("c1", json!({})), done_tokens(40, 5)],
        vec![text("shortened"), done_tokens(410, 5)],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.extra = json!({"model": {"contextWindow": 1_500}});

    core.op(json!({"op": "send", "input": {"text": "run tool"}}));
    core.until("tool_call");
    core.result("c1|fc_c1", &blob);
    assert_eq!(core.until("turn_end")["stopReason"], "stop");

    // A user message larger than the 1,500-token window cannot be shortened away.
    // Request 3 must NOT skip the guard based on the prior 415-token shortened count.
    core.op(json!({"op": "send", "input": {"text": "u".repeat(8_000)}}));
    let ev = core.until("context_too_large");
    assert_eq!(ev["window"], 1_500);
    assert_eq!(core.until("turn_end")["stopReason"], "error");
    core.stop();
    assert_eq!(
        server.join().unwrap().len(),
        2,
        "request 3 must be rejected locally without sending"
    );
}

#[test]
fn budget_anchor_survives_bookkeeping_but_clears_on_review_target_checkpoint_and_reopen() {
    let (base, server) = server(vec![
        // 1: unguarded reply proves anchor (8000 tokens); mid-stream app record does not break it
        vec![Step::Pause(80), text("one"), done_tokens(7_990, 10)],
        // 2: reuses 8k anchor; review_request replaces body so its 9k reply cannot anchor
        vec![text("two"), done_tokens(8_990, 10)],
        // 3: unanchored because request 2's body was replaced; proves 6k anchor
        vec![text("three"), done_tokens(5_990, 10)],
        // 4: different model id invalidates the 6k anchor; proves 7k anchor on m2
        vec![text("four"), done_tokens(6_990, 10)],
        // 5: after checkpoint, anchor is cleared; proves 5k anchor
        vec![text("five"), done_tokens(4_990, 10)],
        // 6: after editing context.jsonl, anchor is cleared; proves 5k anchor
        vec![text("six"), done_tokens(4_990, 10)],
        // 7: after reopening the session in a new Core process, starts unanchored
        vec![text("seven"), done_tokens(5_010, 10)],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.extra = json!({"model": {"contextWindow": 20_000}});

    // Request 1 with a mid-stream bookkeeping record.
    core.op(json!({"op": "send", "input": {"text": "first"}}));
    core.op(
        json!({"op": "record", "id": "rec1", "ns": "progress", "data": {"summary": "working"}}),
    );
    core.until("recorded");
    assert_eq!(core.until("turn_end")["stopReason"], "stop");

    // Request 2 with review_request replacing body.
    core.extra = json!({"model": {"contextWindow": 20_000}, "reviewRequest": true});
    core.op(json!({"op": "send", "input": {"text": "second"}}));
    let review = core.until("review_request");
    let body = review["request"]["body"].clone();
    core.op(json!({"op": "review_request", "id": review["id"], "body": body}));
    assert_eq!(core.until("turn_end")["stopReason"], "stop");

    // Request 3 without review_request.
    core.extra = json!({"model": {"contextWindow": 20_000}});
    core.op(json!({"op": "send", "input": {"text": "third"}}));
    assert_eq!(core.until("turn_end")["stopReason"], "stop");

    // Request 4 switches model id.
    core.extra = json!({"model": {"id": "m2", "contextWindow": 20_000}});
    core.op(json!({"op": "send", "input": {"text": "fourth"}}));
    assert_eq!(core.until("turn_end")["stopReason"], "stop");

    // Request 5 after explicit checkpoint.
    let ctx = core.context();
    core.op(json!({"op": "checkpoint", "messages": ctx}));
    core.until("checkpointed");
    core.op(json!({"op": "send", "input": {"text": "fifth"}}));
    assert_eq!(core.until("turn_end")["stopReason"], "stop");

    // Request 6 after editing context.jsonl (accepted at turn boundary).
    let edited = json!({"role": "user", "content": [{"type": "text", "text": "edited summary"}]});
    std::fs::write(d.join("context.jsonl"), format!("{edited}\n")).unwrap();
    core.op(json!({"op": "send", "input": {"text": "sixth"}}));
    assert_eq!(core.until("context_file")["accepted"], true);
    assert_eq!(core.until("turn_end")["stopReason"], "stop");

    // Close Core without deleting session dir, then reopen in a fresh Core process.
    drop(core.stdin);
    let _ = core.child.wait();

    let mut reopened = Core::start(&d, &base);
    reopened.extra = json!({"model": {"id": "m2", "contextWindow": 20_000}});
    reopened.op(json!({"op": "send", "input": {"text": "seventh"}}));
    assert_eq!(reopened.until("turn_end")["stopReason"], "stop");
    reopened.stop();

    let bodies = server.join().unwrap();
    assert_eq!(bodies.len(), 7);
    assert_eq!(
        last_input_text(&bodies[0]),
        "Context: about 0k of 20k tokens (estimated)"
    );
    assert_eq!(
        last_input_text(&bodies[1]),
        "Context: about 8k of 20k tokens (8k measured + ~0k new)",
        "mid-stream bookkeeping record must not break the proved anchor"
    );
    assert_eq!(
        last_input_text(&bodies[2]),
        "Context: about 0k of 20k tokens (estimated)",
        "review_request body replacement must not produce a trusted anchor"
    );
    assert_eq!(
        last_input_text(&bodies[3]),
        "Context: about 0k of 20k tokens (estimated)",
        "model change must invalidate the previous target's anchor"
    );
    assert_eq!(
        last_input_text(&bodies[4]),
        "Context: about 0k of 20k tokens (estimated)",
        "checkpoint must invalidate the anchor"
    );
    assert_eq!(
        last_input_text(&bodies[5]),
        "Context: about 0k of 20k tokens (estimated)",
        "edited context.jsonl must invalidate the anchor"
    );
    assert_eq!(
        last_input_text(&bodies[6]),
        "Context: about 0k of 20k tokens (estimated)",
        "reopening a session must start unanchored"
    );
}

#[test]
fn late_async_tool_result_before_reply_prevents_anchoring_over_it() {
    let blob = "z".repeat(16_000);
    let (base, server) = server(vec![
        // 1: start an async tool call (turn continues immediately to request 2)
        vec![async_call("c1"), done_tokens(40, 5)],
        // 2: paused at review_request while async result arrives before reply 2 is saved
        vec![text("working"), done_tokens(195, 5)],
        // 3: woken by async_result_pending; must count and shorten the 16k-byte result
        vec![text("saw async"), done_tokens(300, 5)],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.extra = json!({"model": {"contextWindow": 1_500}});

    core.op(json!({"op": "send", "input": {"text": "start bg"}}));
    let tc = core.until("tool_call");
    assert_eq!(tc["async"], true);
    // Pause request 2 right after snapshot capture so the async result lands before reply 2.
    core.extra = json!({"model": {"contextWindow": 1_500}, "reviewRequest": true});
    let review = core.until("review_request");
    core.result("c1|fc_c1", &blob);
    // FIFO barrier: once 'recorded' arrives, the preceding tool_result is durably saved.
    core.op(json!({"op": "record", "id": "sync", "ns": "sync", "data": 1}));
    core.until("recorded");
    // Approve review_request WITHOUT replacing body so only model_writes contiguity blocks anchoring.
    core.extra = json!({"model": {"contextWindow": 1_500}});
    core.op(json!({"op": "review_request", "id": review["id"]}));
    core.until("settled");
    core.stop();

    let bodies = server.join().unwrap();
    assert_eq!(bodies.len(), 3);
    assert_eq!(
        last_input_text(&bodies[2]),
        "Context: about 4k of 2k tokens (estimated)",
        "reply 2 must not anchor over an async tool result saved before it"
    );
    let out = bodies[2]["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .and_then(|i| i["output"].as_str())
        .unwrap_or("");
    assert!(
        out.starts_with("[Tool result set aside to fit the context window:"),
        "request 3 must shorten the unseen async result: {out}"
    );
}

#[test]
fn over_budget_proved_anchor_survives_consecutive_shortened_requests() {
    // Small enough that chars/4 (~700 tokens) fits the 2,000-token window on its own,
    // so losing the proved 1,700-token anchor would sawtooth back to an unguarded request.
    let tool_blob = "r".repeat(2_800);
    let (base, server) = server(vec![
        // 1: unguarded reply proves 1,700 measured tokens on a 2,000-token window
        vec![call("c1", json!({})), done_tokens(1_600, 100)],
        // 2: 1,700 + ~700 > 2,000 -> shortened; provider returns small 320-token count
        vec![text("shortened once"), done_tokens(300, 20)],
        // 3: must retain request 1's proved anchor and keep shortening (no sawtooth)
        vec![text("shortened twice"), done_tokens(310, 20)],
        // 4: must still retain request 1's proved anchor and keep shortening
        vec![text("shortened thrice"), done_tokens(320, 20)],
        // 5: after editing context.jsonl, retained anchor is cleared and request fits unguarded
        vec![text("after compact"), done_tokens(100, 10)],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);
    core.extra = json!({"model": {"contextWindow": 2_000}});

    core.op(json!({"op": "send", "input": {"text": "run tool"}}));
    core.until("tool_call");
    core.result("c1|fc_c1", &tool_blob);
    assert_eq!(core.until("turn_end")["stopReason"], "stop");

    core.op(json!({"op": "send", "input": {"text": "follow up 1"}}));
    assert_eq!(core.until("turn_end")["stopReason"], "stop");

    core.op(json!({"op": "send", "input": {"text": "follow up 2"}}));
    assert_eq!(core.until("turn_end")["stopReason"], "stop");

    // Compacting via context.jsonl must clear the retained anchor.
    let edited = json!({"role": "user", "content": [{"type": "text", "text": "compact summary"}]});
    std::fs::write(d.join("context.jsonl"), format!("{edited}\n")).unwrap();
    core.op(json!({"op": "send", "input": {"text": "after compact"}}));
    assert_eq!(core.until("context_file")["accepted"], true);
    assert_eq!(core.until("turn_end")["stopReason"], "stop");
    core.stop();

    let bodies = server.join().unwrap();
    assert_eq!(bodies.len(), 5);
    let output_of = |body: &Value| -> String {
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .and_then(|i| i["output"].as_str())
            .unwrap_or("")
            .to_string()
    };

    for (idx, body) in bodies[1..=3].iter().enumerate() {
        let req_num = idx + 2;
        assert!(
            output_of(body).starts_with("[Tool result set aside to fit the context window:"),
            "request {req_num} must stay shortened without sawtoothing: {}",
            output_of(body)
        );
        let note = last_input_text(body);
        assert!(
            note.contains("(2k measured + ~"),
            "request {req_num} must keep request 1's proved anchor, got: {note}"
        );
    }
    assert_eq!(
        last_input_text(&bodies[4]),
        "Context: about 0k of 2k tokens (estimated)",
        "accepted context.jsonl edit must clear the retained anchor"
    );
}

#[test]
fn async_tool_context_file_edit_is_not_overwritten_by_next_request() {
    let (base, server) = server(vec![
        // 1: dispatch async tool call (turn immediately continues to request 2 while c1 runs)
        vec![async_call("c1"), done_tokens(40, 5)],
        // 2: runs while c1 is still active; calls a normal wait tool w1
        vec![call("w1", json!({"wait": "c1"})), done_tokens(50, 5)],
        // 3: runs once both c1 and w1 have finished; must accept c1's edit
        vec![text("after compact"), done_tokens(60, 5)],
        // 4: steady follow-up must not accept the same edit twice
        vec![text("steady"), done_tokens(70, 5)],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);

    core.op(json!({"op": "send", "input": {"text": "run compact"}}));
    let tc1 = core.until("tool_call");
    assert_eq!(tc1["async"], true);

    // Pause request 2 at review_request while c1 is still running.
    core.extra = json!({"reviewRequest": true});
    let review = core.until("review_request");

    // Request 2 must freeze context.jsonl while c1 is running: only request 1's single
    // user message should be in context.jsonl (not the assistant's c1 tool call).
    let frozen_lines: Vec<Value> = std::fs::read_to_string(d.join("context.jsonl"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        frozen_lines.len(),
        1,
        "request 2 must not republish context.jsonl while async tool c1 is running"
    );
    assert_eq!(frozen_lines[0]["content"][0]["text"], "run compact");

    // Simulate async tool c1 atomically replacing context.jsonl while request 2 is in flight.
    let edited =
        json!({"role": "user", "content": [{"type": "text", "text": "compacted summary"}]});
    let edited_text = format!("{edited}\n");
    let tmp = d.join("context.jsonl.tmp");
    std::fs::write(&tmp, &edited_text).unwrap();
    std::fs::rename(&tmp, d.join("context.jsonl")).unwrap();

    // Release request 2; it calls normal tool w1 and blocks in wait_for_normal_calls.
    core.extra = json!({});
    core.op(json!({"op": "review_request", "id": review["id"]}));
    let tc2 = core.until("tool_call");
    assert_eq!(tc2["call"], "w1|fc_w1");
    assert_eq!(
        std::fs::read_to_string(d.join("context.jsonl")).unwrap(),
        edited_text,
        "in-flight request 2 must leave the async tool's edit untouched on disk"
    );

    // Finish both c1 and w1 so core becomes quiescent before request 3.
    core.result("c1|fc_c1", "compacted ok");
    core.result("w1|fc_w1", "c1 done");

    let cf_ev = core.until("context_file");
    assert_eq!(cf_ev["accepted"], true);
    assert_eq!(core.until("turn_end")["stopReason"], "stop");

    // Request 4 must NOT double-accept the already published edit.
    core.op(json!({"op": "send", "input": {"text": "steady check"}}));
    let mut cf_events = 0usize;
    loop {
        let ev = core.next();
        if ev["ev"] == "context_file" {
            cf_events += 1;
        }
        if ev["ev"] == "turn_end" {
            assert_eq!(ev["stopReason"], "stop");
            break;
        }
    }
    assert_eq!(
        cf_events, 0,
        "republished context must not be accepted twice"
    );

    // Disk mirror now contains the merged context (edited summary + post-through tool results).
    let disk = std::fs::read_to_string(d.join("context.jsonl")).unwrap();
    assert!(disk.contains("compacted summary"));
    assert!(disk.contains("steady check"));
    core.stop();

    let bodies = server.join().unwrap();
    assert_eq!(bodies.len(), 4);
    let req3 = bodies[2]["input"].to_string();
    assert!(
        req3.contains("compacted summary"),
        "req3 missing edit: {req3}"
    );
    assert!(
        req3.contains("compacted ok"),
        "req3 missing c1 result: {req3}"
    );
    assert!(req3.contains("c1 done"), "req3 missing w1 result: {req3}");
    assert!(
        !req3.contains("run compact"),
        "req3 should have compacted away the original prompt: {req3}"
    );
}

#[test]
fn context_file_edit_is_taken_in_while_an_unrelated_tool_runs() {
    let (base, server) = server(vec![
        // 1: dispatch async tool call c1, which keeps running
        vec![async_call("c1"), done_tokens(40, 5)],
        // 2: ends the turn while c1 is still running
        vec![text("bg running"), done_tokens(50, 5)],
        // 3: new user input while c1 still runs; must already use the edit
        vec![text("answered from summary"), done_tokens(60, 5)],
        // 4: woken by c1's result; must not take the edit in again
        vec![text("saw bg result"), done_tokens(70, 5)],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);

    core.op(json!({"op": "send", "input": {"text": "start bg"}}));
    core.until("settled");

    // The runner edits context.jsonl (replacing it in one step) while c1 still runs.
    let edited = json!({"role": "user", "content": [{"type": "text", "text": "short summary"}]});
    let edited_text = format!("{edited}\n");
    let tmp = d.join("context.jsonl.tmp");
    std::fs::write(&tmp, &edited_text).unwrap();
    std::fs::rename(&tmp, d.join("context.jsonl")).unwrap();

    core.op(json!({"op": "send", "input": {"text": "next question"}}));
    let cf = core.until("context_file");
    assert_eq!(cf["accepted"], true, "the edit is taken in although c1 runs");
    core.until("settled");
    assert_eq!(
        std::fs::read_to_string(d.join("context.jsonl")).unwrap(),
        edited_text,
        "the file is not rewritten while c1 runs"
    );

    core.result("c1|fc_c1", "bg done");
    let mut accepted_again = 0usize;
    loop {
        let ev = core.next();
        if ev["ev"] == "context_file" {
            accepted_again += 1;
        }
        if ev["ev"] == "settled" {
            break;
        }
    }
    assert_eq!(accepted_again, 0, "the edit must not be taken in twice");
    let disk = std::fs::read_to_string(d.join("context.jsonl")).unwrap();
    assert!(
        disk.contains("short summary") && disk.contains("saw bg result"),
        "once nothing runs the file is rewritten with the merged context: {disk}"
    );
    core.stop();

    let bodies = server.join().unwrap();
    assert_eq!(bodies.len(), 4);
    let req3 = bodies[2]["input"].to_string();
    assert!(req3.contains("short summary"), "req3 missing edit: {req3}");
    assert!(req3.contains("next question"), "req3 missing new input: {req3}");
    assert!(
        !req3.contains("start bg"),
        "req3 should have compacted away the original prompt: {req3}"
    );
    let req4 = bodies[3]["input"].to_string();
    assert!(req4.contains("short summary") && req4.contains("bg done"));
    assert!(!req4.contains("start bg"), "req4: {req4}");
}

#[test]
fn async_tool_context_file_edit_accepted_after_late_completion() {
    let (base, server) = server(vec![
        // 1: dispatch async tool call c1
        vec![async_call("c1"), done_tokens(40, 5)],
        // 2: ends turn while c1 is still running
        vec![text("continuing while bg runs"), done_tokens(50, 5)],
        // 3: woken when c1 finishes; now quiescent and accepts c1's edit
        vec![text("saw late compact"), done_tokens(60, 5)],
    ]);
    let d = dir();
    let mut core = Core::start(&d, &base);

    core.op(json!({"op": "send", "input": {"text": "start bg"}}));
    core.until("settled");

    // Request 2 must have frozen context.jsonl while c1 was running.
    let frozen_lines: Vec<Value> = std::fs::read_to_string(d.join("context.jsonl"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(frozen_lines.len(), 1);
    assert_eq!(frozen_lines[0]["content"][0]["text"], "start bg");

    // Async tool c1 edits context.jsonl and reports completion after turn 1 settled.
    let edited =
        json!({"role": "user", "content": [{"type": "text", "text": "late compacted summary"}]});
    std::fs::write(d.join("context.jsonl"), format!("{edited}\n")).unwrap();
    core.result("c1|fc_c1", "bg done");

    assert_eq!(core.until("context_file")["accepted"], true);
    core.until("settled");
    core.stop();

    let bodies = server.join().unwrap();
    assert_eq!(bodies.len(), 3);
    let req3 = bodies[2]["input"].to_string();
    assert!(
        req3.contains("late compacted summary"),
        "req3 missing edit: {req3}"
    );
    assert!(
        req3.contains("continuing while bg runs"),
        "req3 missing intervening reply: {req3}"
    );
    assert!(
        req3.contains("bg done"),
        "req3 missing async result: {req3}"
    );
    assert!(
        !req3.contains("start bg"),
        "req3 should have compacted away the original prompt: {req3}"
    );
}
