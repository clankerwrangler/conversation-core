//! pi 1.5 `write`: create parent directories, then write the file.

use super::paths::{node_error, resolve_to_cwd};
use super::{Context, Output, ToolResult, str_arg, with_file_lock};
use serde_json::{Map, Value};

pub fn run(args: &Map<String, Value>, ctx: &Context) -> ToolResult {
    let path = str_arg(args, "path").unwrap_or("");
    let content = str_arg(args, "content").unwrap_or("");
    let abs = resolve_to_cwd(path, &ctx.cwd);
    with_file_lock(&abs, || {
        if ctx.cancel.cancelled() {
            return Err("Operation aborted".to_string());
        }
        if let Some(dir) = abs.parent() {
            std::fs::create_dir_all(dir).map_err(|e| node_error(&e, "mkdir", Some(dir)))?;
        }
        std::fs::write(&abs, content).map_err(|e| node_error(&e, "open", Some(&abs)))?;
        Ok(Output::text(format!("Successfully wrote to {path}")))
    })
}
