//! The `edit` tool: exact replacement, then a fuzzy fallback that
//! normalizes trailing spaces, quotes, dashes and special spaces; line endings and a BOM are kept.

use super::paths::{access, error_code, node_error, resolve_to_cwd};
use super::{Context, Output, ToolResult, js_trim_end, str_arg, with_file_lock};
use serde_json::{Map, Value, json};
use unicode_normalization::UnicodeNormalization;

fn is_single_edit(v: &Value) -> bool {
    v.get("oldText").is_some_and(Value::is_string) && v.get("newText").is_some_and(Value::is_string)
}

/// `prepareEditArguments`: `edits` as a JSON string or a single object, and legacy top-level
/// `oldText`/`newText`.
pub fn prepare_arguments(mut args: Value) -> Value {
    let Value::Object(obj) = &mut args else {
        return args;
    };
    match obj.get("edits") {
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Array(a)) => {
                obj.insert("edits".into(), Value::Array(a));
            }
            Ok(single) if single.is_object() && is_single_edit(&single) => {
                obj.insert("edits".into(), json!([single]));
            }
            _ => {}
        },
        Some(single) if single.is_object() && is_single_edit(single) => {
            let single = single.clone();
            obj.insert("edits".into(), json!([single]));
        }
        _ => {}
    }
    let (Some(Value::String(old)), Some(Value::String(new))) =
        (obj.get("oldText"), obj.get("newText"))
    else {
        return args;
    };
    let legacy = json!({"oldText": old, "newText": new});
    let mut edits = match obj.get("edits") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    edits.push(legacy);
    obj.remove("oldText");
    obj.remove("newText");
    obj.insert("edits".into(), Value::Array(edits));
    args
}

pub fn run(args: &Map<String, Value>, ctx: &Context) -> ToolResult {
    let edits: Vec<(String, String)> = match args.get("edits") {
        Some(Value::Array(a)) if !a.is_empty() => a
            .iter()
            .map(|e| {
                let s = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                (s("oldText"), s("newText"))
            })
            .collect(),
        _ => {
            return Err(
                "Edit tool input is invalid. edits must contain at least one replacement.".into(),
            );
        }
    };
    let path = str_arg(args, "path").unwrap_or("");
    let abs = resolve_to_cwd(path, &ctx.cwd);
    with_file_lock(&abs, || {
        if ctx.cancel.cancelled() {
            return Err("Operation aborted".to_string());
        }
        if let Err(e) = access(&abs, libc::R_OK | libc::W_OK) {
            return Err(format!(
                "Could not edit file: {path}. Error code: {}.",
                error_code(&e)
            ));
        }
        let raw = std::fs::read(&abs).map_err(|e| node_error(&e, "open", Some(&abs)))?;
        let raw = String::from_utf8_lossy(&raw);
        let (bom, content) = match raw.strip_prefix('\u{FEFF}') {
            Some(rest) => ("\u{FEFF}", rest),
            None => ("", raw.as_ref()),
        };
        let ending = detect_line_ending(content);
        let normalized = normalize_to_lf(content);
        let new_content = apply_edits(&normalized, &edits, path)?;
        if ctx.cancel.cancelled() {
            return Err("Operation aborted".to_string());
        }
        let restored = if ending == "\r\n" {
            new_content.replace('\n', "\r\n")
        } else {
            new_content.clone()
        };
        std::fs::write(&abs, format!("{bom}{restored}"))
            .map_err(|e| node_error(&e, "open", Some(&abs)))?;
        let (diff, first_changed) = diff_string(&normalized, &new_content, 4);
        let patch = unified_patch(path, &normalized, &new_content);
        let mut details = json!({"diff": diff, "patch": patch});
        if let Some(line) = first_changed {
            details["firstChangedLine"] = json!(line);
        }
        Ok(Output {
            content: vec![
                json!({"type": "text", "text": format!("Successfully replaced {} block(s) in {path}.", edits.len())}),
            ],
            details: Some(details),
            is_error: false,
        })
    })
}

fn detect_line_ending(content: &str) -> &'static str {
    match (content.find("\r\n"), content.find('\n')) {
        (Some(crlf), Some(lf)) if crlf < lf => "\r\n",
        _ => "\n",
    }
}

fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// `normalizeForFuzzyMatch`.
fn fuzzy(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    let trimmed: Vec<&str> = nfkc.split('\n').map(js_trim_end).collect();
    trimmed
        .join("\n")
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

struct Found {
    index: usize,
    len: usize,
    fuzzy: bool,
}

/// `fuzzyFindText`: offsets are in `content`, or in its fuzzy form when `fuzzy`.
fn find(content: &str, old: &str) -> Option<Found> {
    if let Some(index) = content.find(old) {
        return Some(Found {
            index,
            len: old.len(),
            fuzzy: false,
        });
    }
    let fc = fuzzy(content);
    let fo = fuzzy(old);
    fc.find(&fo).map(|index| Found {
        index,
        len: fo.len(),
        fuzzy: true,
    })
}

fn count_occurrences(content: &str, old: &str) -> usize {
    let fo = fuzzy(old);
    if fo.is_empty() {
        // "x".split("") has one item per UTF-16 unit
        return fuzzy(content)
            .chars()
            .map(char::len_utf16)
            .sum::<usize>()
            .saturating_sub(1);
    }
    fuzzy(content).matches(fo.as_str()).count()
}

struct Matched {
    edit: usize,
    index: usize,
    len: usize,
    new: String,
}

/// `applyEditsToNormalizedContent`: returns the new content.
fn apply_edits(normalized: &str, edits: &[(String, String)], path: &str) -> Result<String, String> {
    let edits: Vec<(String, String)> = edits
        .iter()
        .map(|(o, n)| (normalize_to_lf(o), normalize_to_lf(n)))
        .collect();
    let total = edits.len();
    for (i, (old, _)) in edits.iter().enumerate() {
        if old.is_empty() {
            return Err(if total == 1 {
                format!("oldText must not be empty in {path}.")
            } else {
                format!("edits[{i}].oldText must not be empty in {path}.")
            });
        }
    }
    let used_fuzzy = edits
        .iter()
        .any(|(old, _)| find(normalized, old).is_some_and(|f| f.fuzzy));
    let base = if used_fuzzy {
        fuzzy(normalized)
    } else {
        normalized.to_string()
    };
    let mut matched = Vec::new();
    for (i, (old, new)) in edits.iter().enumerate() {
        let Some(found) = find(&base, old) else {
            return Err(if total == 1 {
                format!(
                    "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
                )
            } else {
                format!(
                    "Could not find edits[{i}] in {path}. The oldText must match exactly including all whitespace and newlines."
                )
            });
        };
        let n = count_occurrences(&base, old);
        if n > 1 {
            return Err(if total == 1 {
                format!(
                    "Found {n} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
                )
            } else {
                format!(
                    "Found {n} occurrences of edits[{i}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
                )
            });
        }
        matched.push(Matched {
            edit: i,
            index: found.index,
            len: found.len,
            new: new.clone(),
        });
    }
    matched.sort_by_key(|m| m.index);
    for w in matched.windows(2) {
        if w[0].index + w[0].len > w[1].index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                w[0].edit, w[1].edit
            ));
        }
    }
    let new_content = if used_fuzzy {
        apply_preserving_unchanged_lines(normalized, &base, &matched)?
    } else {
        apply_replacements(&base, &matched.iter().collect::<Vec<_>>(), 0)
    };
    if new_content == normalized {
        return Err(if total == 1 {
            format!(
                "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
            )
        } else {
            format!("No changes made to {path}. The replacements produced identical content.")
        });
    }
    Ok(new_content)
}

fn apply_replacements(content: &str, replacements: &[&Matched], offset: usize) -> String {
    let mut result = content.to_string();
    for r in replacements.iter().rev() {
        let at = r.index - offset;
        result.replace_range(at..at + r.len, &r.new);
    }
    result
}

/// `applyReplacementsPreservingUnchangedLines`: only touched lines come from the fuzzy base.
fn apply_preserving_unchanged_lines(
    original: &str,
    base: &str,
    replacements: &[Matched],
) -> Result<String, String> {
    let original_lines: Vec<&str> = original.split_inclusive('\n').collect();
    let mut spans = Vec::new();
    let mut at = 0;
    for line in base.split_inclusive('\n') {
        spans.push((at, at + line.len()));
        at += line.len();
    }
    if original_lines.len() != spans.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .into(),
        );
    }
    let mut groups: Vec<(usize, usize, Vec<&Matched>)> = Vec::new();
    for r in replacements {
        let start = spans
            .iter()
            .position(|(s, e)| r.index >= *s && r.index < *e)
            .ok_or("Replacement range is outside the base content.")?;
        let end_offset = r.index + r.len;
        let mut end = start;
        while end < spans.len() && spans[end].1 < end_offset {
            end += 1;
        }
        if end >= spans.len() {
            return Err("Replacement range is outside the base content.".into());
        }
        let end = end + 1;
        match groups.last_mut() {
            Some(g) if start < g.1 => {
                g.1 = g.1.max(end);
                g.2.push(r);
            }
            _ => groups.push((start, end, vec![r])),
        }
    }
    let mut result = String::new();
    let mut line = 0;
    for (start, end, group) in &groups {
        result.push_str(&original_lines[line..*start].concat());
        let (from, to) = (spans[*start].0, spans[end - 1].1);
        result.push_str(&apply_replacements(&base[from..to], group, from));
        line = *end;
    }
    result.push_str(&original_lines[line..].concat());
    Ok(result)
}

/// `generateDiffString`: numbered `+`/`-` lines with up to `context` lines around changes.
fn diff_string(old: &str, new: &str, context: usize) -> (String, Option<usize>) {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    // Runs of the same tag, like jsdiff's parts.
    let mut parts: Vec<(ChangeTag, Vec<String>)> = Vec::new();
    for change in diff.iter_all_changes() {
        let text = change
            .value()
            .strip_suffix('\n')
            .unwrap_or(change.value())
            .to_string();
        match parts.last_mut() {
            Some((tag, lines)) if *tag == change.tag() => lines.push(text),
            _ => parts.push((change.tag(), vec![text])),
        }
    }
    let width = old
        .split('\n')
        .count()
        .max(new.split('\n').count())
        .to_string()
        .len();
    let num = |n: usize| format!("{n:>width$}");
    let blank = " ".repeat(width);
    let (mut old_n, mut new_n) = (1usize, 1usize);
    let mut out: Vec<String> = Vec::new();
    let mut last_was_change = false;
    let mut first = None;
    for (i, (tag, raw)) in parts.iter().enumerate() {
        if *tag != ChangeTag::Equal {
            first.get_or_insert(new_n);
            for line in raw {
                if *tag == ChangeTag::Insert {
                    out.push(format!("+{} {line}", num(new_n)));
                    new_n += 1;
                } else {
                    out.push(format!("-{} {line}", num(old_n)));
                    old_n += 1;
                }
            }
            last_was_change = true;
            continue;
        }
        let next_is_change = parts
            .get(i + 1)
            .is_some_and(|(t, _)| *t != ChangeTag::Equal);
        let show =
            |lines: &[String], out: &mut Vec<String>, old_n: &mut usize, new_n: &mut usize| {
                for line in lines {
                    out.push(format!(" {} {line}", num(*old_n)));
                    *old_n += 1;
                    *new_n += 1;
                }
            };
        if last_was_change && next_is_change {
            if raw.len() <= context * 2 {
                show(raw, &mut out, &mut old_n, &mut new_n);
            } else {
                show(&raw[..context], &mut out, &mut old_n, &mut new_n);
                out.push(format!(" {blank} ..."));
                let skipped = raw.len() - 2 * context;
                old_n += skipped;
                new_n += skipped;
                show(
                    &raw[raw.len() - context..],
                    &mut out,
                    &mut old_n,
                    &mut new_n,
                );
            }
        } else if last_was_change {
            let shown = raw.len().min(context);
            show(&raw[..shown], &mut out, &mut old_n, &mut new_n);
            if raw.len() > shown {
                out.push(format!(" {blank} ..."));
                old_n += raw.len() - shown;
                new_n += raw.len() - shown;
            }
        } else if next_is_change {
            let skipped = raw.len().saturating_sub(context);
            if skipped > 0 {
                out.push(format!(" {blank} ..."));
                old_n += skipped;
                new_n += skipped;
            }
            show(&raw[skipped..], &mut out, &mut old_n, &mut new_n);
        } else {
            old_n += raw.len();
            new_n += raw.len();
        }
        last_was_change = false;
    }
    (out.join("\n"), first)
}

/// `generateUnifiedPatch`: a unified diff with file headers only and 4 lines of context.
fn unified_patch(path: &str, old: &str, new: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(4)
        .header(path, path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edits(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn exact_fuzzy_and_errors() {
        assert_eq!(
            apply_edits("a\nb\nc\n", &edits(&[("b", "B")]), "f").ok(),
            Some("a\nB\nc\n".into())
        );
        // Fuzzy: curly quotes and trailing spaces; untouched lines keep their bytes.
        let src = "x = \u{201C}hi\u{201D}  \nkeep\u{00A0}me  \n";
        assert_eq!(
            apply_edits(src, &edits(&[("x = \"hi\"", "x = 1")]), "f").ok(),
            Some("x = 1\nkeep\u{00A0}me  \n".into())
        );
        assert_eq!(
            apply_edits("a a", &edits(&[("a", "b")]), "f").err(),
            Some("Found 2 occurrences of the text in f. The text must be unique. Please provide more context to make it unique.".into())
        );
        assert_eq!(
            apply_edits("abc", &edits(&[("ab", "x"), ("bc", "y")]), "f").err(),
            Some("edits[0] and edits[1] overlap in f. Merge them into one edit or target disjoint regions.".into())
        );
        assert_eq!(
            apply_edits("abc", &edits(&[("zz", "y")]), "f").err(),
            Some("Could not find the exact text in f. The old text must match exactly including all whitespace and newlines.".into())
        );
    }

    #[test]
    fn prepares_legacy_and_string_forms() {
        let a = prepare_arguments(
            json!({"path": "p", "edits": "[{\"oldText\":\"a\",\"newText\":\"b\"}]"}),
        );
        assert_eq!(
            a,
            json!({"path": "p", "edits": [{"oldText": "a", "newText": "b"}]})
        );
        let a = prepare_arguments(json!({"path": "p", "oldText": "a", "newText": "b"}));
        assert_eq!(
            a,
            json!({"path": "p", "edits": [{"oldText": "a", "newText": "b"}]})
        );
    }

    #[test]
    fn diff_numbers_lines() {
        let (d, first) = diff_string("a\nb\nc\n", "a\nB\nc\n", 4);
        assert_eq!(d, " 1 a\n-2 b\n+2 B\n 3 c");
        assert_eq!(first, Some(2));
    }
}
