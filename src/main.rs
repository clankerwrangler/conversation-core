mod budget;
mod catalog;
mod completions;
mod context_file;
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
        let catalog = f.catalog.unwrap_or_else(catalog::default_dir);
        return match process::run(Path::new(dir), f.no_tool, models, sources, catalog) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
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
        [_, cmd, dir, from, to] if cmd == "history" => {
            match (from.parse::<u64>(), to.parse::<u64>()) {
                (Ok(from), Ok(to)) => store::Session::open(Path::new(dir))
                    .and_then(|session| session.history(from, to, true))
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
