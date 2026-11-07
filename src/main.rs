mod budget;
mod context_file;
mod models;
mod process;
mod provider;
mod responses;
mod store;
mod tools;

use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "core run <session-dir> [--no-tool <name>]... [--models <file>] | core context <session-dir> | core input <session-dir> <api> <provider> <model> | core referenced <session-dir> | core tool <name> <arguments-json>";

/// `--no-tool <name>` and `--models <file>` after `run <dir>`; None if anything else is there.
fn run_flags(rest: &[String]) -> Option<(Vec<String>, Option<std::path::PathBuf>)> {
    let mut names = Vec::new();
    let mut models = None;
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        let value = it.next()?;
        match flag.as_str() {
            "--no-tool" if tools::NAMES.contains(&value.as_str()) => names.push(value.clone()),
            "--models" if models.is_none() => models = Some(value.into()),
            _ => return None,
        }
    }
    Some((names, models))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if let [_, cmd, dir, rest @ ..] = args.as_slice()
        && cmd == "run"
    {
        let Some((disabled, models)) = run_flags(rest) else {
            eprintln!("error: usage: {USAGE} (tools: {})", tools::NAMES.join(", "));
            return ExitCode::FAILURE;
        };
        let models = match models::Models::load(models.as_deref()) {
            Ok(m) => m,
            Err(err) => {
                eprintln!("error: models file: {err}");
                return ExitCode::FAILURE;
            }
        };
        return match process::run(Path::new(dir), disabled, models) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        };
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
