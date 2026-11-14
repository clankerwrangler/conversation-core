//! The `find` tool: what `fd --glob --color=never --hidden [--no-require-git] --max-results N
//! [--full-path]` prints, built on the libraries fd uses (`ignore`, `globset`). Results are in
//! walk order sorted by name (fd's own order is not deterministic).

use super::paths::{relative, resolve_to_cwd};
use super::truncate::{DEFAULT_MAX_BYTES, NO_LINE_LIMIT, format_size, truncate_head};
use super::{Context, Output, ToolResult, js_num, num_arg, path_arg, str_arg};
use serde_json::{Map, Value, json};
use std::path::Path;

const DEFAULT_LIMIT: f64 = 1000.0;

pub fn inside_git_repo(start: &Path) -> bool {
    start.ancestors().any(|dir| dir.join(".git").exists())
}

pub fn run(args: &Map<String, Value>, ctx: &Context) -> ToolResult {
    let pattern = str_arg(args, "pattern").unwrap_or("");
    let search = resolve_to_cwd(path_arg(args, "path"), &ctx.cwd);
    let limit = num_arg(args, "limit").unwrap_or(DEFAULT_LIMIT);
    let max = limit.trunc().max(1.0) as usize;
    if !search.is_dir() {
        return Err(format!(
            "[fd error]: Search path \"{}\" is not a directory.\n[fd error]: No valid search paths given.",
            search.display()
        ));
    }
    let full_path = pattern.contains('/');
    let mut effective = pattern.to_string();
    if full_path && !pattern.starts_with('/') && !pattern.starts_with("**/") && pattern != "**" {
        effective = format!("**/{pattern}");
    }
    // fd's smart case: an uppercase letter makes the pattern case-sensitive.
    let case_insensitive = !pattern.chars().any(char::is_uppercase);
    let glob = globset::GlobBuilder::new(&effective)
        .literal_separator(full_path)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|e| format!("[fd error]: {e}"))?
        .compile_matcher();
    let mut walker = ignore::WalkBuilder::new(&search);
    walker
        .hidden(false)
        .require_git(inside_git_repo(&search))
        .add_custom_ignore_filename(".fdignore")
        .sort_by_file_name(|a, b| a.cmp(b));
    let mut results: Vec<String> = Vec::new();
    for entry in walker.build() {
        if ctx.cancel.cancelled() {
            return Err("Operation aborted".into());
        }
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();
        let matched = if full_path {
            glob.is_match(path)
        } else {
            glob.is_match(Path::new(entry.file_name()))
        };
        if !matched {
            continue;
        }
        let mut rel = relative(&search, path);
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            rel.push('/');
        }
        results.push(rel);
        if results.len() >= max {
            break;
        }
    }
    if results.is_empty() {
        return Ok(Output::text("No files found matching pattern"));
    }
    let limit_reached = results.len() as f64 >= limit;
    let t = truncate_head(&results.join("\n"), NO_LINE_LIMIT, DEFAULT_MAX_BYTES);
    let mut output = t.content.clone();
    let mut details = Map::new();
    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{} results limit reached. Use limit={} for more, or refine pattern",
            js_num(limit),
            js_num(limit * 2.0)
        ));
        details.insert("resultLimitReached".into(), json!(limit));
    }
    if t.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.insert(
            "truncation".into(),
            t.to_json(NO_LINE_LIMIT, DEFAULT_MAX_BYTES),
        );
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok(Output {
        content: vec![json!({"type": "text", "text": output})],
        details: (!details.is_empty()).then_some(Value::Object(details)),
        is_error: false,
    })
}
