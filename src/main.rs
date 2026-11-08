mod auth;
mod budget;
mod catalog;
mod completions;
mod context_file;
mod dirs;
mod entries;
mod grok;
mod models;
mod process;
mod prompt;
mod provider;
mod responses;
mod store;
mod tools;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "core run <session-dir> [--no-tool <name>]... [--models <file>] [--agent-dir <dir>] [--skills <path>]... | core context <session-dir> | core input <session-dir> <api> <provider> <model> | core referenced <session-dir> | core history <session-dir> <from> <to> | core tool <name> <arguments-json> | core prompt [--agent-dir <dir>] [--skills <path>]... [--tool <name>]...";

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
        let paths = process::Paths {
            catalog: f.catalog.unwrap_or_else(catalog::default_dir),
            auth: f.auth.unwrap_or_else(auth::default_path),
        };
        return match process::run(Path::new(dir), f.no_tool, models, sources, paths) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        };
    }
    // Sign-in from a terminal: `core login <provider> [device|browser|key] [--auth <file>]`.
    // A pasted address or API key is read from standard input, never taken as an argument.
    if let [_, cmd, provider, rest @ ..] = args.as_slice()
        && (cmd == "login" || cmd == "logout")
    {
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
        let path = f.auth.unwrap_or_else(auth::default_path);
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
            eprintln!("Paste the API key, then press Enter:");
            line()
        } else {
            None
        };
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
        // (pi's shape), with no instructions or tools: for comparing with pi.
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
        [_, cmd, dir] if cmd == "referenced" => store::Session::open(Path::new(dir))
            .and_then(|session| session.referenced())
            .and_then(|files| serde_json::to_string(&files).map_err(store::Error::from)),
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
