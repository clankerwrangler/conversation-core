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
}

/// One scripted reply per request, in order.
fn server(replies: Vec<Vec<Step>>) -> (String, std::thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut bodies = Vec::new();
        for reply in replies {
            let (mut stream, _) = listener.accept().unwrap();
            bodies.push(read_request(&mut stream));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n")
                .unwrap();
            for step in reply {
                match step {
                    Step::Event(e) => {
                        let _ = stream.write_all(format!("data: {e}\n\n").as_bytes());
                        let _ = stream.flush();
                    }
                    Step::Pause(ms) => std::thread::sleep(Duration::from_millis(ms)),
                    Step::Drop => break,
                }
            }
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
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
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
fn done() -> Step {
    Step::Event(
        json!({"type": "response.completed", "response": {"usage": {"input_tokens": 10, "output_tokens": 2}}}),
    )
}

struct Core {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<Value>,
    base: String,
    dir: PathBuf,
}

impl Core {
    fn start(dir: &Path, base: &str) -> Core {
        let mut child = Command::new(env!("CARGO_BIN_EXE_core"))
            .args(["run", dir.to_str().unwrap()])
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
            match ev["ev"].as_str() {
                Some("prepare_request") => {
                    let model = json!({"api": "openai-responses", "provider": "test", "id": "m", "baseUrl": self.base});
                    self.op(json!({"op": "prepared", "id": ev["id"], "model": model}));
                }
                Some("need_credentials") => {
                    self.op(json!({"op": "credentials", "id": ev["id"], "token": "t"}))
                }
                Some("text_delta") | Some("reasoning_delta") => {}
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
            .starts_with("Interrupted")
    );
    assert_eq!(ctx[1]["stopReason"], "error", "the open reply was closed");
    again.stop();
}
