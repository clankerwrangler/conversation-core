//! The `ls` tool: entries sorted case-insensitively, `/` after directories, dotfiles included.

use super::paths::{access, node_error, resolve_to_cwd};
use super::truncate::{DEFAULT_MAX_BYTES, NO_LINE_LIMIT, format_size, truncate_head};
use super::{Context, Output, ToolResult, js_num, num_arg, path_arg};
use serde_json::{Map, Value, json};
use std::cmp::Ordering;
use unicode_normalization::UnicodeNormalization;

const DEFAULT_LIMIT: f64 = 500.0;

pub fn run(args: &Map<String, Value>, ctx: &Context) -> ToolResult {
    let dir = resolve_to_cwd(path_arg(args, "path"), &ctx.cwd);
    let limit = num_arg(args, "limit").unwrap_or(DEFAULT_LIMIT);
    if access(&dir, libc::F_OK).is_err() {
        return Err(format!("Path not found: {}", dir.display()));
    }
    let meta = std::fs::metadata(&dir).map_err(|e| node_error(&e, "stat", Some(&dir)))?;
    if !meta.is_dir() {
        return Err(format!("Not a directory: {}", dir.display()));
    }
    let mut entries: Vec<String> = std::fs::read_dir(&dir)
        .map_err(|e| {
            format!(
                "Cannot read directory: {}",
                node_error(&e, "scandir", Some(&dir))
            )
        })?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    entries.sort_by(|a, b| locale_compare(&a.to_lowercase(), &b.to_lowercase()));
    let mut results = Vec::new();
    let mut limit_reached = false;
    for entry in &entries {
        if ctx.cancel.cancelled() {
            return Err("Operation aborted".into());
        }
        if results.len() as f64 >= limit {
            limit_reached = true;
            break;
        }
        let Ok(m) = std::fs::metadata(dir.join(entry)) else {
            continue;
        };
        results.push(if m.is_dir() {
            format!("{entry}/")
        } else {
            entry.clone()
        });
    }
    if results.is_empty() {
        return Ok(Output::text("(empty directory)"));
    }
    let t = truncate_head(&results.join("\n"), NO_LINE_LIMIT, DEFAULT_MAX_BYTES);
    let mut output = t.content.clone();
    let mut details = Map::new();
    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{} entries limit reached. Use limit={} for more",
            js_num(limit),
            js_num(limit * 2.0)
        ));
        details.insert("entryLimitReached".into(), json!(limit));
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

/// The CLDR root order of ASCII characters: whitespace, punctuation, symbols, digits, letters.
const ROOT_ORDER: &str =
    "\t\n\u{b}\u{c}\r _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0123456789abcdefghijklmnopqrstuvwxyz";

fn primary(c: char) -> u32 {
    match ROOT_ORDER.find(c) {
        Some(i) => i as u32 + 1,
        None => 1000 + c as u32,
    }
}

/// Primary weights (base characters) and secondary weights (accents) of a string.
fn weights(s: &str) -> (Vec<u32>, Vec<u32>) {
    let mut p = Vec::new();
    let mut sec = Vec::new();
    for c in s.nfd() {
        if unicode_normalization::char::is_combining_mark(c) {
            sec.push(c as u32);
        } else if c.is_control() && !c.is_whitespace() {
            continue;
        } else {
            p.push(primary(c));
            sec.push(0);
        }
    }
    (p, sec)
}

/// `String.prototype.localeCompare` with the root locale, for the names `ls` sorts.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    let (pa, sa) = weights(a);
    let (pb, sb) = weights(b);
    pa.cmp(&pb).then_with(|| sa.cmp(&sb)).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_like_locale_compare() {
        let mut v = vec!["b", "_x", "a10", "a2", ".git", "-y", "Z", "é"];
        v.sort_by(|a, b| locale_compare(&a.to_lowercase(), &b.to_lowercase()));
        assert_eq!(v, vec!["_x", "-y", ".git", "a10", "a2", "b", "é", "Z"]);
    }
}
