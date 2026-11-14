//! The `bash` tool:
//! `bash -c` in its own process group, combined output kept to the last 2000 lines or 50KB,
//! the full output in a temp file when that cuts anything.

use super::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, Truncation, format_size, truncate_tail,
};
use super::{Context, Output, ToolResult, js_num, kill_tree, num_arg, str_arg};
use serde_json::{Map, Value, json};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

const MAX_TIMEOUT_MS: f64 = 2_147_483_647.0;
/// After the shell exits, keep reading while descendants still write; stop after this much quiet.
const EXIT_STDIO_GRACE: Duration = Duration::from_millis(100);

fn shell() -> String {
    if Path::new("/bin/bash").exists() {
        return "/bin/bash".into();
    }
    if let Ok(out) = Command::new("which").arg("bash").output()
        && out.status.success()
        && let Some(first) = String::from_utf8_lossy(&out.stdout).trim().lines().next()
        && !first.is_empty()
    {
        return first.to_string();
    }
    "sh".into()
}

enum Ev {
    Data(Vec<u8>),
    End,
    Exit(std::process::ExitStatus),
    Failed(String),
}

enum Stop {
    Exited(i32),
    Aborted,
    TimedOut,
}

pub fn run(args: &Map<String, Value>, ctx: &Context) -> ToolResult {
    let command = str_arg(args, "command").unwrap_or("");
    let timeout = num_arg(args, "timeout");
    let mut acc = Accumulator::new();
    let stop = execute(command, timeout, ctx, &mut acc);
    acc.finish();
    let snapshot = acc.snapshot();
    match stop {
        Err(message) => Err(message),
        Ok(Stop::Aborted) => Err(append_status(
            &format_output(&snapshot, &acc, "").0,
            "Command aborted",
        )),
        Ok(Stop::TimedOut) => Err(append_status(
            &format_output(&snapshot, &acc, "").0,
            &format!(
                "Command timed out after {} seconds",
                js_num(timeout.unwrap_or(0.0))
            ),
        )),
        Ok(Stop::Exited(code)) => {
            let (text, details) = format_output(&snapshot, &acc, "(no output)");
            if code != 0 {
                return Ok(Output {
                    content: vec![
                        json!({"type": "text", "text": append_status(&text, &format!("Command exited with code {code}"))}),
                    ],
                    details,
                    is_error: true,
                });
            }
            Ok(Output {
                content: vec![json!({"type": "text", "text": text})],
                details,
                is_error: false,
            })
        }
    }
}

fn append_status(text: &str, status: &str) -> String {
    if text.is_empty() {
        status.to_string()
    } else {
        format!("{text}\n\n{status}")
    }
}

fn execute(
    command: &str,
    timeout: Option<f64>,
    ctx: &Context,
    acc: &mut Accumulator,
) -> Result<Stop, String> {
    let timeout_ms = match timeout {
        None => None,
        Some(t) if !t.is_finite() || t <= 0.0 => {
            return Err("Invalid timeout: must be a finite number of seconds".into());
        }
        Some(t) if t * 1000.0 > MAX_TIMEOUT_MS => {
            return Err(format!(
                "Invalid timeout: maximum is {} seconds",
                js_num(MAX_TIMEOUT_MS / 1000.0)
            ));
        }
        Some(t) => Some(t * 1000.0),
    };
    if ctx.cancel.cancelled() {
        return Ok(Stop::Aborted);
    }
    if !ctx.cwd.exists() {
        return Err(format!(
            "Working directory does not exist: {}\nCannot execute bash commands.",
            ctx.cwd.display()
        ));
    }
    let mut cmd = Command::new(shell());
    cmd.arg("-c")
        .arg(command)
        .current_dir(&ctx.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for key in [
        "PI_SESSION_ID",
        "PI_SESSION_FILE",
        "PI_PROVIDER",
        "PI_MODEL",
        "PI_REASONING_LEVEL",
    ] {
        cmd.env_remove(key);
    }
    for (k, v) in &ctx.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let pid = child.id() as i32;
    ctx.cancel.set_group(Some(pid));
    let (tx, rx) = channel();
    for pipe in [
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut pipe = pipe;
            let mut buf = vec![0u8; 65536];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(Ev::Data(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(Ev::End);
        });
    }
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(match child.wait() {
                Ok(status) => Ev::Exit(status),
                Err(e) => Ev::Failed(e.to_string()),
            });
        });
    }
    drop(tx);
    let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_secs_f64(ms / 1000.0));
    let mut timed_out = false;
    let mut ended = 0;
    let mut exit: Option<std::process::ExitStatus> = None;
    let mut idle_until: Option<Instant> = None;
    loop {
        if exit.is_some() && ended >= 2 {
            break;
        }
        let now = Instant::now();
        let mut wake = now + Duration::from_millis(50);
        if let Some(d) = deadline.filter(|_| !timed_out) {
            wake = wake.min(d);
        }
        if let Some(i) = idle_until {
            wake = wake.min(i);
        }
        match rx.recv_timeout(wake.saturating_duration_since(now)) {
            Ok(Ev::Data(d)) => {
                acc.append(&d);
                if exit.is_some() {
                    idle_until = Some(Instant::now() + EXIT_STDIO_GRACE);
                }
            }
            Ok(Ev::End) => ended += 1,
            Ok(Ev::Exit(status)) => {
                exit = Some(status);
                idle_until = Some(Instant::now() + EXIT_STDIO_GRACE);
            }
            Ok(Ev::Failed(e)) => {
                ctx.cancel.set_group(None);
                return Err(e);
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if ctx.cancel.cancelled() {
            kill_tree(pid);
        }
        if let Some(d) = deadline
            && !timed_out
            && Instant::now() >= d
            && exit.is_none()
        {
            timed_out = true;
            kill_tree(pid);
        }
        if exit.is_some() && idle_until.is_some_and(|i| Instant::now() >= i) {
            break;
        }
    }
    ctx.cancel.set_group(None);
    if ctx.cancel.cancelled() {
        return Ok(Stop::Aborted);
    }
    if timed_out {
        return Ok(Stop::TimedOut);
    }
    let status = exit.ok_or("Command terminated without an exit code")?;
    Ok(Stop::Exited(
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
    ))
}

fn format_output(snapshot: &Snapshot, acc: &Accumulator, empty: &str) -> (String, Option<Value>) {
    let t = &snapshot.truncation;
    let mut text = if t.content.is_empty() {
        empty.to_string()
    } else {
        t.content.clone()
    };
    if !snapshot.truncated {
        return (text, None);
    }
    let path = snapshot
        .full_output_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut truncation = t.to_json(DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    truncation["truncated"] = json!(true);
    truncation["truncatedBy"] = json!(snapshot.truncated_by);
    let details = json!({"truncation": truncation, "fullOutputPath": path});
    let start = t.total_lines - t.output_lines + 1;
    let end = t.total_lines;
    if t.last_line_partial {
        text.push_str(&format!(
            "\n\n[Showing last {} of line {end} (line is {}). Full output: {path}]",
            format_size(t.output_bytes),
            format_size(acc.current_line_bytes)
        ));
    } else if snapshot.truncated_by == "lines" {
        text.push_str(&format!(
            "\n\n[Showing lines {start}-{end} of {}. Full output: {path}]",
            t.total_lines
        ));
    } else {
        text.push_str(&format!(
            "\n\n[Showing lines {start}-{end} of {} ({} limit). Full output: {path}]",
            t.total_lines,
            format_size(DEFAULT_MAX_BYTES)
        ));
    }
    (text, Some(details))
}

struct Snapshot {
    truncation: Truncation,
    truncated: bool,
    truncated_by: &'static str,
    full_output_path: Option<PathBuf>,
}

/// Output accumulator: a decoded rolling tail, counts, and a temp file once output is big.
struct Accumulator {
    pending: Vec<u8>,
    raw_chunks: Vec<u8>,
    tail: String,
    tail_starts_at_line_boundary: bool,
    total_raw_bytes: usize,
    total_decoded_bytes: usize,
    completed_lines: usize,
    total_lines: usize,
    current_line_bytes: usize,
    has_open_line: bool,
    temp: Option<(PathBuf, std::fs::File)>,
}

const MAX_ROLLING_BYTES: usize = DEFAULT_MAX_BYTES * 2;

impl Accumulator {
    fn new() -> Accumulator {
        Accumulator {
            pending: Vec::new(),
            raw_chunks: Vec::new(),
            tail: String::new(),
            tail_starts_at_line_boundary: true,
            total_raw_bytes: 0,
            total_decoded_bytes: 0,
            completed_lines: 0,
            total_lines: 0,
            current_line_bytes: 0,
            has_open_line: false,
            temp: None,
        }
    }

    fn append(&mut self, data: &[u8]) {
        self.total_raw_bytes += data.len();
        let text = decode(&mut self.pending, data, false);
        self.append_decoded(&text);
        if self.temp.is_some() || self.should_use_temp_file() {
            self.ensure_temp_file();
            if let Some((_, f)) = &mut self.temp {
                let _ = f.write_all(data);
            }
        } else {
            self.raw_chunks.extend_from_slice(data);
        }
    }

    fn finish(&mut self) {
        let text = decode(&mut self.pending, &[], true);
        self.append_decoded(&text);
        if self.should_use_temp_file() {
            self.ensure_temp_file();
        }
    }

    fn snapshot(&mut self) -> Snapshot {
        let text = if self.tail_starts_at_line_boundary {
            self.tail.as_str()
        } else {
            self.tail
                .find('\n')
                .map_or(self.tail.as_str(), |i| &self.tail[i + 1..])
        };
        let mut t = truncate_tail(text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let truncated =
            self.total_lines > DEFAULT_MAX_LINES || self.total_decoded_bytes > DEFAULT_MAX_BYTES;
        let truncated_by =
            t.truncated_by
                .unwrap_or(if self.total_decoded_bytes > DEFAULT_MAX_BYTES {
                    "bytes"
                } else {
                    "lines"
                });
        t.total_lines = self.total_lines;
        t.total_bytes = self.total_decoded_bytes;
        if truncated {
            self.ensure_temp_file();
        }
        Snapshot {
            truncation: t,
            truncated,
            truncated_by,
            full_output_path: self.temp.as_ref().map(|(p, _)| p.clone()),
        }
    }

    fn append_decoded(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.total_decoded_bytes += text.len();
        self.tail.push_str(text);
        if self.tail.len() > MAX_ROLLING_BYTES * 2 {
            self.trim_tail();
        }
        let newlines = text.matches('\n').count();
        if newlines == 0 {
            self.current_line_bytes += text.len();
            self.has_open_line = true;
        } else {
            self.completed_lines += newlines;
            let rest = &text[text.rfind('\n').map_or(0, |i| i + 1)..];
            self.current_line_bytes = rest.len();
            self.has_open_line = !rest.is_empty();
        }
        self.total_lines = self.completed_lines + usize::from(self.has_open_line);
    }

    fn trim_tail(&mut self) {
        if self.tail.len() <= MAX_ROLLING_BYTES {
            return;
        }
        let mut start = self.tail.len() - MAX_ROLLING_BYTES;
        while !self.tail.is_char_boundary(start) {
            start += 1;
        }
        if start > 0 {
            self.tail_starts_at_line_boundary = self.tail.as_bytes()[start - 1] == b'\n';
        }
        self.tail = self.tail[start..].to_string();
    }

    fn should_use_temp_file(&self) -> bool {
        self.total_raw_bytes > DEFAULT_MAX_BYTES
            || self.total_decoded_bytes > DEFAULT_MAX_BYTES
            || self.total_lines > DEFAULT_MAX_LINES
    }

    fn ensure_temp_file(&mut self) {
        if self.temp.is_some() {
            return;
        }
        let mut random = [0u8; 8];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let _ = f.read_exact(&mut random);
        }
        let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("pi-bash-{hex}.log"));
        let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        else {
            return;
        };
        let _ = file.write_all(&self.raw_chunks);
        self.raw_chunks = Vec::new();
        self.temp = Some((path, file));
    }
}

/// Streaming UTF-8 decoding like `TextDecoder`: bad bytes become U+FFFD, an incomplete
/// sequence at the end waits for more input (or becomes U+FFFD at the end).
fn decode(pending: &mut Vec<u8>, data: &[u8], last: bool) -> String {
    pending.extend_from_slice(data);
    let mut out = String::new();
    let mut i = 0;
    while i < pending.len() {
        match std::str::from_utf8(&pending[i..]) {
            Ok(s) => {
                out.push_str(s);
                i = pending.len();
            }
            Err(e) => {
                let valid = e.valid_up_to();
                out.push_str(std::str::from_utf8(&pending[i..i + valid]).unwrap_or_default());
                i += valid;
                match e.error_len() {
                    Some(n) => {
                        out.push('\u{FFFD}');
                        i += n;
                    }
                    None => {
                        if last {
                            out.push('\u{FFFD}');
                            i = pending.len();
                        }
                        break;
                    }
                }
            }
        }
    }
    pending.drain(..i);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ctx() -> Context {
        Context {
            cwd: std::env::temp_dir(),
            env: vec![("PI_MODEL".into(), "m".into())],
            cancel: Arc::default(),
        }
    }

    fn run_cmd(args: Value) -> (String, bool) {
        let Value::Object(args) = args else {
            return (String::new(), true);
        };
        match run(&args, &ctx()) {
            Ok(o) => (
                o.content[0]["text"].as_str().unwrap_or("").to_string(),
                o.is_error,
            ),
            Err(e) => (e, true),
        }
    }

    #[test]
    fn output_exit_codes_timeouts() {
        // stdout and stderr are two pipes: both are kept in arrival order.
        let (both, failed) = run_cmd(json!({"command": "echo hi; echo err >&2"}));
        let mut lines: Vec<&str> = both.lines().collect();
        lines.sort_unstable();
        assert_eq!((lines, failed), (vec!["err", "hi"], false));
        assert_eq!(
            run_cmd(json!({"command": "true"})),
            ("(no output)".into(), false)
        );
        assert_eq!(
            run_cmd(json!({"command": "echo x; exit 3"})),
            ("x\n\n\nCommand exited with code 3".into(), true)
        );
        assert_eq!(
            run_cmd(json!({"command": "printf $PI_MODEL"})),
            ("m".into(), false)
        );
        assert_eq!(
            run_cmd(json!({"command": "sleep 5", "timeout": 0.2})),
            ("Command timed out after 0.2 seconds".into(), true)
        );
        let (text, _) = run_cmd(json!({"command": "seq 1 3000"}));
        assert!(text.starts_with("1001\n"), "{}", &text[..20]);
        assert!(
            text.contains("\n3000\n\n[Showing lines 1001-3000 of 3000. Full output: "),
            "{}",
            &text[text.len() - 120..]
        );
    }

    #[test]
    fn decodes_split_utf8() {
        let mut p = Vec::new();
        let bytes = "é".as_bytes();
        assert_eq!(decode(&mut p, &bytes[..1], false), "");
        assert_eq!(decode(&mut p, &bytes[1..], false), "é");
        assert_eq!(decode(&mut p, &[0xff, b'a'], true), "\u{FFFD}a");
    }
}
