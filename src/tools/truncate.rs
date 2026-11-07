//! pi 1.5 `core/tools/truncate.js`: shared output limits.

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
pub const GREP_MAX_LINE_LENGTH: usize = 500;

#[derive(Debug, Clone, PartialEq)]
pub struct Truncation {
    pub content: String,
    pub truncated: bool,
    /// "lines" | "bytes"
    pub truncated_by: Option<&'static str>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
}

/// `Number.MAX_SAFE_INTEGER`: pi's "no line limit".
pub const NO_LINE_LIMIT: usize = 9_007_199_254_740_991;

impl Truncation {
    /// The truncation object pi keeps in `details`.
    pub fn to_json(&self, max_lines: usize, max_bytes: usize) -> serde_json::Value {
        serde_json::json!({
            "content": self.content,
            "truncated": self.truncated,
            "truncatedBy": self.truncated_by,
            "totalLines": self.total_lines,
            "totalBytes": self.total_bytes,
            "outputLines": self.output_lines,
            "outputBytes": self.output_bytes,
            "lastLinePartial": self.last_line_partial,
            "firstLineExceedsLimit": self.first_line_exceeds_limit,
            "maxLines": max_lines,
            "maxBytes": max_bytes,
        })
    }
}

fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// `formatSize`: 1023B, 1.5KB, 2.0MB (one decimal, JS toFixed rounding).
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{}KB", to_fixed1(bytes as f64 / 1024.0))
    } else {
        format!("{}MB", to_fixed1(bytes as f64 / (1024.0 * 1024.0)))
    }
}

fn to_fixed1(v: f64) -> String {
    format!("{v:.1}")
}

fn untouched(content: &str, lines: usize, bytes: usize) -> Truncation {
    Truncation {
        content: content.to_string(),
        truncated: false,
        truncated_by: None,
        total_lines: lines,
        total_bytes: bytes,
        output_lines: lines,
        output_bytes: bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
    }
}

/// Keep the first lines/bytes; never a partial line.
pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> Truncation {
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return untouched(content, total_lines, total_bytes);
    }
    if lines[0].len() > max_bytes {
        return Truncation {
            content: String::new(),
            truncated: true,
            truncated_by: Some("bytes"),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
        };
    }
    let mut out: Vec<&str> = Vec::new();
    let mut bytes = 0;
    let mut by = "lines";
    for (i, line) in lines.iter().enumerate().take(max_lines) {
        let line_bytes = line.len() + usize::from(i > 0);
        if bytes + line_bytes > max_bytes {
            by = "bytes";
            break;
        }
        out.push(line);
        bytes += line_bytes;
    }
    if out.len() >= max_lines && bytes <= max_bytes {
        by = "lines";
    }
    let content = out.join("\n");
    Truncation {
        output_bytes: content.len(),
        content,
        truncated: true,
        truncated_by: Some(by),
        total_lines,
        total_bytes,
        output_lines: out.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
    }
}

/// Keep the last lines/bytes; the last line may be partial if it alone is too big.
pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> Truncation {
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return untouched(content, total_lines, total_bytes);
    }
    let mut out: Vec<String> = Vec::new();
    let mut bytes = 0;
    let mut by = "lines";
    let mut partial = false;
    for line in lines.iter().rev() {
        if out.len() >= max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(!out.is_empty());
        if bytes + line_bytes > max_bytes {
            by = "bytes";
            if out.is_empty() {
                let tail = tail_bytes(line, max_bytes);
                bytes = tail.len();
                out.insert(0, tail.to_string());
                partial = true;
            }
            break;
        }
        out.insert(0, line.to_string());
        bytes += line_bytes;
    }
    if out.len() >= max_lines && bytes <= max_bytes {
        by = "lines";
    }
    let content = out.join("\n");
    Truncation {
        output_bytes: content.len(),
        content,
        truncated: true,
        truncated_by: Some(by),
        total_lines,
        total_bytes,
        output_lines: out.len(),
        last_line_partial: partial,
        first_line_exceeds_limit: false,
    }
}

/// The end of `s` within `max_bytes`, starting at a character boundary.
fn tail_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// `truncateLine`: at most `max` UTF-16 units, then "... [truncated]".
pub fn truncate_line(line: &str, max: usize) -> (String, bool) {
    if utf16_len(line) <= max {
        return (line.to_string(), false);
    }
    (format!("{}... [truncated]", utf16_prefix(line, max)), true)
}

pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// The longest prefix of `s` with at most `n` UTF-16 units.
pub fn utf16_prefix(s: &str, n: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        if units + c.len_utf16() > n {
            return &s[..i];
        }
        units += c.len_utf16();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_keeps_whole_lines_within_limits() {
        let t = truncate_head("a\nbb\nccc\n", 2, 100);
        assert_eq!(
            (
                t.content.as_str(),
                t.truncated_by,
                t.output_lines,
                t.total_lines
            ),
            ("a\nbb", Some("lines"), 2, 3)
        );
        let t = truncate_head("aaaa\nbbbb", 10, 6);
        assert_eq!(
            (t.content.as_str(), t.truncated_by),
            ("aaaa", Some("bytes"))
        );
        assert!(truncate_head("xxxxxxxx", 10, 4).first_line_exceeds_limit);
    }

    #[test]
    fn tail_keeps_last_lines_and_can_cut_one_long_line() {
        let t = truncate_tail("1\n2\n3", 2, 100);
        assert_eq!(t.content, "2\n3");
        let t = truncate_tail("abcdefgh", 10, 3);
        assert_eq!((t.content.as_str(), t.last_line_partial), ("fgh", true));
    }

    #[test]
    fn sizes_format_like_pi() {
        assert_eq!(format_size(1023), "1023B");
        assert_eq!(format_size(51200), "50.0KB");
        assert_eq!(format_size(1536), "1.5KB");
    }
}
