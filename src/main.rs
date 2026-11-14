mod auth;
mod budget;
mod catalog;
mod completions;
mod context_file;
mod dirs;
mod entries;
mod grok;
mod import;
mod models;
mod process;
mod prompt;
mod provider;
mod responses;
mod store;
mod tools;
mod usage;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "core run <session-dir> [--no-tool <name>]... [--models <file>] [--agent-dir <dir>] [--skills <path>]... | core context <session-dir> | core input <session-dir> <api> <provider> <model> | core referenced <session-dir> | core reap <session-dir> <age-seconds> [--keep-details <a,b,...>] [--protocol 2] | core import <new-session-dir> <id> < records | core history <session-dir> <from> <to> | core tool <name> <arguments-json> | core usage <provider> [--auth <file>] | core prompt [--agent-dir <dir>] [--skills <path>]... [--tool <name>]...";

#[derive(Default)]
struct Flags {
    no_tool: Vec<String>,
    tools: Vec<String>,
    models: Option<PathBuf>,
    agent_dir: Option<PathBuf>,
    skills: Vec<PathBuf>,
    catalog: Option<PathBuf>,
    auth: Option<PathBuf>,
}

/// Flags after `run <dir>` (or `prompt`, which takes `--tool` instead of `--no-tool`).
/// None if anything else is there.
fn flags(rest: &[String], prompt: bool) -> Option<Flags> {
    let mut f = Flags::default();
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        let value = it.next()?;
        match flag.as_str() {
            "--no-tool" if !prompt && tools::NAMES.contains(&value.as_str()) => {
                f.no_tool.push(value.clone())
            }
            "--tool" if prompt => f.tools.push(value.clone()),
            "--models" if !prompt && f.models.is_none() => f.models = Some(value.into()),
            "--agent-dir" if f.agent_dir.is_none() => f.agent_dir = Some(value.into()),
            "--skills" => f.skills.push(value.into()),
            "--catalog" if !prompt && f.catalog.is_none() => f.catalog = Some(value.into()),
            "--auth" if !prompt && f.auth.is_none() => f.auth = Some(value.into()),
            _ => return None,
        }
    }
    Some(f)
}

fn auth_path(path: Option<PathBuf>, json_error: bool) -> Option<PathBuf> {
    match path.map(Ok).unwrap_or_else(auth::default_path) {
        Ok(path) => Some(path),
        Err(error) => {
            if json_error {
                println!("{}", serde_json::json!({"error": error}));
            } else {
                eprintln!("error: {error}");
            }
            None
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if let [_, cmd, dir, rest @ ..] = args.as_slice()
        && cmd == "run"
    {
        let Some(f) = flags(rest, false) else {
            eprintln!("error: usage: {USAGE} (tools: {})", tools::NAMES.join(", "));
            return ExitCode::FAILURE;
        };
        let models = match models::Models::load(f.models.as_deref()) {
            Ok(m) => m,
            Err(err) => {
                eprintln!("error: models file: {err}");
                return ExitCode::FAILURE;
            }
        };
        let sources = prompt::Sources::new(f.agent_dir, f.skills);
        let Some(auth) = auth_path(f.auth, false) else {
            return ExitCode::FAILURE;
        };
        let paths = process::Paths {
            catalog: f.catalog.unwrap_or_else(catalog::default_dir),
            auth,
        };
        return match process::run(Path::new(dir), f.no_tool, models, sources, paths) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        };
    }
    // A provider's remaining usage, read with the core's own sign-in, as one JSON object.
    if let [_, cmd, provider, rest @ ..] = args.as_slice()
        && cmd == "usage"
    {
        let Some(f) = flags(rest, false) else {
            eprintln!("error: usage: core usage <provider> [--auth <file>]");
            return ExitCode::FAILURE;
        };
        let Some(path) = auth_path(f.auth, false) else {
            return ExitCode::FAILURE;
        };
        println!("{}", usage::usage(&path, provider, provider::now_ms()));
        return ExitCode::SUCCESS;
    }
    // Sign-in from a terminal: `core login <provider> [device|browser|key] [--auth <file>] [--json]`.
    // A pasted address or API key is read from standard input, never taken as an argument.
    // With --json each step is one JSON line on standard output, for a program to follow:
    // {"step":"device_code","url","code","expiresIn"} | {"step":"open_and_paste","url"},
    // then {"done":<what was saved>} or {"error":<message>}.
    if let [_, cmd, provider, rest @ ..] = args.as_slice()
        && (cmd == "login" || cmd == "logout")
    {
        let json_steps = rest.iter().any(|a| a == "--json");
        let rest: Vec<String> = rest.iter().filter(|a| *a != "--json").cloned().collect();
        let rest = rest.as_slice();
        let (method, rest) = match rest {
            [m, rest @ ..] if !m.starts_with("--") => (Some(m.as_str()), rest),
            _ => (None, rest),
        };
        let Some(f) = flags(rest, false) else {
            eprintln!(
                "error: usage: core login <provider> [device|browser|key] [--auth <file>] | core logout <provider>"
            );
            return ExitCode::FAILURE;
        };
        let Some(path) = auth_path(f.auth, cmd == "login" && json_steps) else {
            return ExitCode::FAILURE;
        };
        if cmd == "logout" {
            return match auth::remove(&path, provider) {
                Ok(removed) => {
                    println!(
                        "{}",
                        if removed {
                            "signed out"
                        } else {
                            "was not signed in"
                        }
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        let method = method.unwrap_or(if provider == auth::OPENROUTER {
            "key"
        } else {
            "device"
        });
        let line = || {
            let mut s = String::new();
            std::io::stdin()
                .read_line(&mut s)
                .ok()
                .map(|_| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let key = if method == "key" {
            if !json_steps {
                eprintln!("Paste the API key, then press Enter:");
            }
            line()
        } else {
            None
        };
        if json_steps {
            let show = |step: auth::Step| {
                let line = match step {
                    auth::Step::DeviceCode {
                        url,
                        code,
                        expires_in,
                    } => {
                        serde_json::json!({"step": "device_code", "url": url, "code": code, "expiresIn": expires_in})
                    }
                    auth::Step::OpenAndPaste { url } => {
                        serde_json::json!({"step": "open_and_paste", "url": url})
                    }
                };
                println!("{line}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            };
            let (outcome, code) =
                match auth::login(&path, provider, method, key.as_deref(), &show, &line) {
                    Ok(done) => (serde_json::json!({"done": done}), ExitCode::SUCCESS),
                    Err(e) => (serde_json::json!({"error": e}), ExitCode::FAILURE),
                };
            println!("{outcome}");
            return code;
        }
        let show = |step: auth::Step| match step {
            auth::Step::DeviceCode {
                url,
                code,
                expires_in,
            } => {
                eprintln!(
                    "Open {url} and enter the code {code} (valid for {} minutes).",
                    expires_in / 60
                )
            }
            auth::Step::OpenAndPaste { url } => {
                eprintln!(
                    "Open this page and sign in:\n{url}\nThen paste the address the browser ends on and press Enter:"
                )
            }
        };
        return match auth::login(&path, provider, method, key.as_deref(), &show, &line) {
            Ok(done) => {
                println!("{done}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }
    // The core's part of the system prompt for the current folder, with the named tools.
    if let [_, cmd, rest @ ..] = args.as_slice()
        && cmd == "prompt"
    {
        let Some(f) = flags(rest, true) else {
            eprintln!("error: usage: {USAGE}");
            return ExitCode::FAILURE;
        };
        let cwd = match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        };
        let sources = prompt::Sources::new(f.agent_dir, f.skills);
        let files = prompt::context_files(&cwd, &sources.agent_dir);
        let skills = prompt::skills(&cwd, &sources);
        let tools: Vec<prompt::ToolPrompt> = f.tools.iter().map(|n| prompt::builtin(n)).collect();
        print!("{}", prompt::build("", &tools, &files, &skills, &cwd, &[]));
        return ExitCode::SUCCESS;
    }
    let result = match args.as_slice() {
        // One built-in call in the current folder, printed as the result the core would save.
        [_, cmd, name, arguments] if cmd == "tool" => serde_json::from_str(arguments)
            .map_err(store::Error::from)
            .and_then(|arguments| {
                let ctx = tools::Context {
                    cwd: std::env::current_dir().map_err(|e| store::Error::Io(".".into(), e))?,
                    env: Vec::new(),
                    cancel: Default::default(),
                };
                serde_json::to_string(&tools::run(name, &arguments, &ctx))
                    .map_err(store::Error::from)
            }),
        // Records numbered from..=to (with stored data read back), for a session not running.
        // The current chunk's entries.
        [_, cmd, dir] if cmd == "history" => store::Session::open(Path::new(dir))
            .and_then(|session| session.entries(session.current_start()?, u64::MAX))
            .and_then(|entries| serde_json::to_string(&entries).map_err(store::Error::from)),
        [_, cmd, dir, from, to] if cmd == "history" => {
            match (from.parse::<u64>(), to.parse::<u64>()) {
                (Ok(from), Ok(to)) => store::Session::open(Path::new(dir))
                    .and_then(|session| session.entries(from, to))
                    .and_then(|records| {
                        serde_json::to_string(&records).map_err(store::Error::from)
                    }),
                _ => Err(store::Error::Usage(USAGE)),
            }
        }
        // The Chat Completions request body for the session's context and a model object
        // in the Pi-compatible format, with no instructions or tools.
        [_, cmd, dir, model, level] if cmd == "chat-body" => std::fs::read_to_string(model)
            .map_err(|e| store::Error::Io(model.into(), e))
            .and_then(|text| {
                serde_json::from_str::<serde_json::Value>(&text).map_err(store::Error::from)
            })
            .and_then(|model| {
                let messages = store::Session::open(Path::new(dir))?.context()?;
                let level = (level != "off").then_some(level.as_str());
                let reasoning = models::reasoning(&model, None, level);
                let body = completions::body(&model, "", &messages, &[], "s", &reasoning);
                serde_json::to_string(&body).map_err(store::Error::from)
            }),
        // A new session from an older transcript: record lines on standard input (import.rs).
        [_, cmd, dir, id] if cmd == "import" => {
            import::import(Path::new(dir), id, std::io::stdin().lock())
                .map(|last| serde_json::json!({"records": last}).to_string())
        }
        [_, cmd, dir] if cmd == "referenced" => store::Session::open(Path::new(dir))
            .and_then(|session| session.referenced())
            .and_then(|files| serde_json::to_string(&files).map_err(store::Error::from)),
        // Delete tool output and reasoning older than the age, except what the context uses,
        // keeping the named top-level fields of tool results' details.
        // `--protocol 2` names this behaviour (every tool call's arguments kept, provably old
        // chunks only): a runner passes it so an older core, which rejects the flag before
        // touching anything, can never run its cleanup.
        [_, cmd, dir, age, rest @ ..] if cmd == "reap" => {
            let mut keep = Some(Vec::new());
            let mut flags = rest.iter();
            while let Some(flag) = flags.next() {
                match (flag.as_str(), flags.next()) {
                    ("--keep-details", Some(fields)) => {
                        if let Some(k) = keep.as_mut() {
                            k.extend(
                                fields
                                    .split(',')
                                    .filter(|f| !f.is_empty())
                                    .map(str::to_string),
                            );
                        }
                    }
                    ("--protocol", Some(version)) if version == "2" => {}
                    _ => keep = None,
                }
            }
            match (age.parse::<i64>(), keep) {
                (Ok(seconds), Some(keep)) if seconds > 0 => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    store::Session::open(Path::new(dir))
                        .and_then(|session| session.reap(now - seconds * 1000, &keep))
                        .map(|(files, bytes)| {
                            serde_json::json!({"deleted": files, "bytes": bytes}).to_string()
                        })
                }
                _ => Err(store::Error::Usage(
                    "core reap <session-dir> <age-seconds> [--keep-details <a,b,...>] [--protocol 2]",
                )),
            }
        }
        [_, cmd, dir] if cmd == "context" => store::Session::open(Path::new(dir))
            .and_then(|session| session.context())
            .and_then(|messages| serde_json::to_string(&messages).map_err(store::Error::from)),
        [_, cmd, dir, api, provider, model] if cmd == "input" => {
            store::Session::open(Path::new(dir))
                .and_then(|session| session.context())
                .and_then(|messages| {
                    let target = responses::Target {
                        api,
                        provider,
                        model,
                    };
                    responses::input(&target, &messages)
                })
                .and_then(|input| serde_json::to_string(&input).map_err(store::Error::from))
        }
        _ => Err(store::Error::Usage(USAGE)),
    };
    match result {
        Ok(out) => {
            println!("{out}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
