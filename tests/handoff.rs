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
        let mut writers = Vec::new();
        for reply in replies {
            let (mut stream, _) = listener.accept().unwrap();
            bodies.push(read_request(&mut stream));
            // Each reply streams on its own thread, so a stuck reply does not hold the next.
            writers.push(std::thread::spawn(move || {
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
                        Step::Drop => break,
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
}

impl Core {
    fn start(dir: &Path, base: &str) -> Core {
        Core::start_with(dir, base, &[], json!([]))
    }

    fn start_with(dir: &Path, base: &str, flags: &[&str], tools: Value) -> Core {
        let mut child = Command::new(env!("CARGO_BIN_EXE_core"))
            .args(["run", dir.to_str().unwrap()])
            .args(flags)
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
                                model[mk] = mv.clone();
                            }
                        } else {
                            prepared[k] = v.clone();
                        }
                    }
                    prepared["model"] = model;
                    self.op(prepared);
                }
                Some("need_credentials") => {
                    self.op(json!({"op": "credentials", "id": ev["id"], "token": "t"}))
                }
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
        review["request"]["headers"]["Authorization"]
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
