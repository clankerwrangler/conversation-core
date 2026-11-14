//! The `read` tool: text with offset/limit and continuation notices, or an image attachment.

use super::paths::{access, node_error, resolve_read_path};
use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size, truncate_head};
use super::{Context, Output, ToolResult, image, js_num, num_arg, str_arg};
use serde_json::{Map, Value, json};
use std::io::Read;

pub fn run(args: &Map<String, Value>, ctx: &Context) -> ToolResult {
    let path = str_arg(args, "path").unwrap_or("");
    let offset = num_arg(args, "offset");
    let limit = num_arg(args, "limit");
    let abs = resolve_read_path(path, &ctx.cwd);
    access(&abs, libc::R_OK).map_err(|e| node_error(&e, "access", Some(&abs)))?;
    if ctx.cancel.cancelled() {
        return Err("Operation aborted".into());
    }
    let mime = sniff(&abs)?;
    let bytes = std::fs::read(&abs).map_err(|e| read_error(&e, &abs))?;
    if let Some(mime) = mime {
        let content = match image::process(&bytes, mime) {
            Err(message) => vec![
                json!({"type": "text", "text": format!("Read image file [{mime}]\n{message}")}),
            ],
            Ok(img) => {
                let mut note = format!("Read image file [{}]", img.mime);
                if !img.hints.is_empty() {
                    note.push('\n');
                    note.push_str(&img.hints.join("\n"));
                }
                vec![
                    json!({"type": "text", "text": note}),
                    json!({"type": "image", "data": img.data, "mimeType": img.mime}),
                ]
            }
        };
        return Ok(Output {
            content,
            details: None,
            is_error: false,
        });
    }
    let text = String::from_utf8_lossy(&bytes);
    read_text(&text, path, offset, limit)
}

fn read_error(e: &std::io::Error, path: &std::path::Path) -> String {
    if e.raw_os_error() == Some(libc::EISDIR) {
        node_error(e, "read", None)
    } else {
        node_error(e, "open", Some(path))
    }
}

/// `detectSupportedImageMimeTypeFromFile`: sniff the first 4100 bytes.
fn sniff(path: &std::path::Path) -> Result<Option<&'static str>, String> {
    let mut file = std::fs::File::open(path).map_err(|e| node_error(&e, "open", Some(path)))?;
    let mut buf = Vec::with_capacity(4100);
    (&mut file)
        .take(4100)
        .read_to_end(&mut buf)
        .map_err(|e| read_error(&e, path))?;
    Ok(image::detect_mime(&buf))
}

fn read_text(text: &str, path: &str, offset: Option<f64>, limit: Option<f64>) -> ToolResult {
    let all: Vec<&str> = text.split('\n').collect();
    let total = all.len() as i64;
    // `offset ? Math.max(0, offset - 1) : 0`, indexes truncated toward zero like Array.slice.
    let start = match offset {
        Some(o) if o != 0.0 && !o.is_nan() => (o - 1.0).max(0.0).trunc() as i64,
        _ => 0,
    };
    let start_display = start + 1;
    if start >= total {
        let shown = offset.map_or_else(|| "undefined".to_string(), js_num);
        return Err(format!(
            "Offset {shown} is beyond end of file ({total} lines total)"
        ));
    }
    let (selected, user_limited) = match limit {
        Some(l) => {
            let end = (start as f64 + l).min(total as f64).trunc() as i64;
            let end_idx = end.clamp(start, total);
            (
                all[start as usize..end_idx as usize].join("\n"),
                Some(end - start),
            )
        }
        None => (all[start as usize..].join("\n"), None),
    };
    let t = truncate_head(&selected, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut details = None;
    let output = if t.first_line_exceeds_limit {
        let size = format_size(all[start as usize].len());
        details = Some(json!({"truncation": t.to_json(DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES)}));
        format!(
            "[Line {start_display} is {size}, exceeds {} limit. Use bash: sed -n '{start_display}p' {path} | head -c {DEFAULT_MAX_BYTES}]",
            format_size(DEFAULT_MAX_BYTES)
        )
    } else if t.truncated {
        let end_display = start_display + t.output_lines as i64 - 1;
        let next = end_display + 1;
        let notice = if t.truncated_by == Some("lines") {
            format!(
                "[Showing lines {start_display}-{end_display} of {total}. Use offset={next} to continue.]"
            )
        } else {
            format!(
                "[Showing lines {start_display}-{end_display} of {total} ({} limit). Use offset={next} to continue.]",
                format_size(DEFAULT_MAX_BYTES)
            )
        };
        details = Some(json!({"truncation": t.to_json(DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES)}));
        format!("{}\n\n{notice}", t.content)
    } else if let Some(n) = user_limited.filter(|n| start + n < total) {
        let remaining = total - (start + n);
        let next = start + n + 1;
        format!(
            "{}\n\n[{remaining} more lines in file. Use offset={next} to continue.]",
            t.content
        )
    } else {
        t.content
    };
    Ok(Output {
        content: vec![json!({"type": "text", "text": output})],
        details,
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(r: ToolResult) -> String {
        match r {
            Ok(o) => o.content[0]["text"].as_str().unwrap_or("").to_string(),
            Err(e) => format!("ERR {e}"),
        }
    }

    #[test]
    fn offsets_limits_and_notices() {
        let f = "a\nb\nc\nd";
        assert_eq!(text(read_text(f, "f", None, None)), "a\nb\nc\nd");
        assert_eq!(
            text(read_text(f, "f", Some(2.0), Some(2.0))),
            "b\nc\n\n[1 more lines in file. Use offset=4 to continue.]"
        );
        assert_eq!(
            text(read_text(f, "f", Some(9.0), None)),
            "ERR Offset 9 is beyond end of file (4 lines total)"
        );
        let big: String = (1..=2500).map(|i| format!("{i}\n")).collect();
        assert!(
            text(read_text(&big, "f", None, None))
                .ends_with("2000\n\n[Showing lines 1-2000 of 2501. Use offset=2001 to continue.]")
        );
        let long = "x".repeat(60_000);
        assert_eq!(
            text(read_text(&long, "f", None, None)),
            "[Line 1 is 58.6KB, exceeds 50.0KB limit. Use bash: sed -n '1p' f | head -c 51200]"
        );
    }
}
