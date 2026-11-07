mod context_file;
mod process;
mod provider;
mod responses;
mod store;
mod tools;

use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "core run <session-dir> [--no-tool <name>]... | core context <session-dir> | core input <session-dir> <api> <provider> <model> | core tool <name> <arguments-json>";

/// `--no-tool <name>` flags after `run <dir>`; None if anything else is there.
fn no_tool_flags(rest: &[String]) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        let name = it.next()?;
        if flag != "--no-tool" || !tools::NAMES.contains(&name.as_str()) {
            return None;
        }
        names.push(name.clone());
    }
    Some(names)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if let [_, cmd, dir, rest @ ..] = args.as_slice()
        && cmd == "run"
    {
        let Some(disabled) = no_tool_flags(rest) else {
            eprintln!("error: usage: {USAGE} (tools: {})", tools::NAMES.join(", "));
            return ExitCode::FAILURE;
        };
        return match process::run(Path::new(dir), disabled) {
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
