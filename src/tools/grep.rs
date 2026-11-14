//! The `grep` tool: what `rg --no-config --json --line-number --color=never --hidden
//! [--ignore-case] [--fixed-strings] [--glob G]` finds, built on ripgrep's own libraries,
//! formatted as tool output. Files are searched in name order (rg's parallel order is not
//! deterministic).

use super::paths::{relative, resolve_to_cwd};
use super::truncate::{
    DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, NO_LINE_LIMIT, format_size, truncate_head,
    truncate_line,
};
use super::{Context, Output, ToolResult, js_num, num_arg, path_arg, str_arg};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder, Sink, SinkMatch};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const DEFAULT_LIMIT: f64 = 100.0;

struct Match {
    file: PathBuf,
    line: u64,
    text: Option<String>,
}

struct Collect<'a> {
    file: &'a Path,
    matches: &'a mut Vec<Match>,
    limit: usize,
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _: &grep_searcher::Searcher,
        m: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        // One event per matching line, like rg's JSON `match` messages.
        for (line, bytes) in (m.line_number().unwrap_or(0)..).zip(m.lines()) {
            self.matches.push(Match {
                file: self.file.to_path_buf(),
                line,
                text: std::str::from_utf8(bytes).ok().map(str::to_string),
            });
            if self.matches.len() >= self.limit {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

pub fn run(args: &Map<String, Value>, ctx: &Context) -> ToolResult {
    let pattern = str_arg(args, "pattern").unwrap_or("");
    let search = resolve_to_cwd(path_arg(args, "path"), &ctx.cwd);
    let is_dir = match std::fs::metadata(&search) {
        Ok(m) => m.is_dir(),
        Err(_) => return Err(format!("Path not found: {}", search.display())),
    };
    let context = num_arg(args, "context")
        .filter(|c| *c > 0.0)
        .map_or(0, |c| c.trunc() as u64);
    let limit = num_arg(args, "limit").unwrap_or(DEFAULT_LIMIT).max(1.0);
    let max = limit.ceil() as usize;
    let ignore_case = args.get("ignoreCase") == Some(&Value::Bool(true));
    let literal = args.get("literal") == Some(&Value::Bool(true));
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(ignore_case)
        .fixed_strings(literal)
        .line_terminator(Some(b'\n'))
        .build(pattern)
        .map_err(|e| {
            // rg reports a syntax error against the pattern as written, and explains the line
            // terminator rule.
            let text = e.to_string();
            if let Some(lit) = text
                .strip_prefix("the literal ")
                .and_then(|t| t.strip_suffix(" is not allowed in a regex"))
            {
                return format!(
                    "the literal '{lit}' is not allowed in a regex\n\nConsider enabling multiline mode with the --multiline flag (or -U for short).\nWhen multiline mode is enabled, new line characters can be matched."
                );
            }
            if literal {
                return text;
            }
            match regex_syntax::ParserBuilder::new().case_insensitive(ignore_case).build().parse(pattern) {
                Err(syntax) => syntax.to_string(),
                Ok(_) => e.to_string(),
            }
        })?;
    let mut walker = ignore::WalkBuilder::new(&search);
    walker
        .hidden(false)
        .add_custom_ignore_filename(".rgignore")
        .sort_by_file_name(|a, b| a.cmp(b));
    if let Some(glob) = str_arg(args, "glob").filter(|g| !g.is_empty()) {
        let mut overrides = ignore::overrides::OverrideBuilder::new(&search);
        overrides.add(glob).map_err(|e| format!("rg: {e}"))?;
        walker.overrides(overrides.build().map_err(|e| format!("rg: {e}"))?);
    }
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();
    let mut matches: Vec<Match> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for entry in walker.build() {
        if ctx.cancel.cancelled() {
            return Err("Operation aborted".into());
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                errors.push(format!("rg: {e}"));
                continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let file = entry.path().to_path_buf();
        let sink = Collect {
            file: &file,
            matches: &mut matches,
            limit: max,
        };
        if let Err(e) = searcher.search_path(&matcher, &file, sink) {
            errors.push(format!("rg: {}: {e}", file.display()));
        }
        if matches.len() >= max {
            break;
        }
    }
    let limit_reached = matches.len() >= max;
    if !limit_reached && !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    if matches.is_empty() {
        return Ok(Output::text("No matches found"));
    }
    let format_path = |file: &Path| -> String {
        if is_dir {
            let rel = relative(&search, file);
            if !rel.is_empty() && !rel.starts_with("..") {
                return rel.replace('\\', "/");
            }
        }
        file.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let mut cache: HashMap<PathBuf, Vec<String>> = HashMap::new();
    let mut lines_truncated = false;
    let mut out: Vec<String> = Vec::new();
    for m in &matches {
        let shown = format_path(&m.file);
        if let (0, Some(text)) = (context, &m.text) {
            let clean = text.replace("\r\n", "\n").replace('\r', "");
            let clean = clean.strip_suffix('\n').unwrap_or(&clean);
            let (t, cut) = truncate_line(clean, GREP_MAX_LINE_LENGTH);
            lines_truncated |= cut;
            out.push(format!("{shown}:{}: {t}", m.line));
            continue;
        }
        let lines = cache
            .entry(m.file.clone())
            .or_insert_with(|| match std::fs::read(&m.file) {
                Ok(b) => String::from_utf8_lossy(&b)
                    .replace("\r\n", "\n")
                    .split('\n')
                    .map(str::to_string)
                    .collect(),
                Err(_) => Vec::new(),
            });
        if lines.is_empty() {
            out.push(format!("{shown}:{}: (unable to read file)", m.line));
            continue;
        }
        let (start, end) = if context > 0 {
            (
                m.line.saturating_sub(context).max(1),
                (m.line + context).min(lines.len() as u64),
            )
        } else {
            (m.line, m.line)
        };
        for current in start..=end {
            let text = lines
                .get(current as usize - 1)
                .map_or("", String::as_str)
                .replace('\r', "");
            let (t, cut) = truncate_line(&text, GREP_MAX_LINE_LENGTH);
            lines_truncated |= cut;
            if current == m.line {
                out.push(format!("{shown}:{current}: {t}"));
            } else {
                out.push(format!("{shown}-{current}- {t}"));
            }
        }
    }
    let t = truncate_head(&out.join("\n"), NO_LINE_LIMIT, DEFAULT_MAX_BYTES);
    let mut output = t.content.clone();
    let mut details = Map::new();
    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{} matches limit reached. Use limit={} for more, or refine pattern",
            js_num(limit),
            js_num(limit * 2.0)
        ));
        details.insert("matchLimitReached".into(), json!(limit));
    }
    if t.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.insert(
            "truncation".into(),
            t.to_json(NO_LINE_LIMIT, DEFAULT_MAX_BYTES),
        );
    }
    if lines_truncated {
        notices.push(format!(
            "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
        ));
        details.insert("linesTruncated".into(), json!(true));
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
