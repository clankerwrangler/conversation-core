//! Path handling on Unix.

use std::path::{Component, Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

fn is_unicode_space(c: char) -> bool {
    matches!(
        c,
        '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
}

/// `normalizePath` with unicode-space normalization and `@` stripping.
fn normalize_input(input: &str) -> String {
    let mut s: String = input
        .chars()
        .map(|c| if is_unicode_space(c) { ' ' } else { c })
        .collect();
    if let Some(rest) = s.strip_prefix('@') {
        s = rest.to_string();
    }
    if s == "~" {
        return home();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return Path::new(&home()).join(rest).to_string_lossy().into_owned();
    }
    if let Some(rest) = s.strip_prefix("file://") {
        return percent_decode(rest);
    }
    s
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Node's `path.resolve`: lexical, no filesystem access, no trailing slash.
fn lexical_resolve(base: &Path, p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    let mut out = PathBuf::from("/");
    for comp in joined.components() {
        match comp {
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
        }
    }
    out
}

/// `resolveToCwd`.
pub fn resolve_to_cwd(path: &str, cwd: &Path) -> PathBuf {
    lexical_resolve(cwd, Path::new(&normalize_input(path)))
}

/// `resolveReadPathAsync`: also tries macOS screenshot name variants.
pub fn resolve_read_path(path: &str, cwd: &Path) -> PathBuf {
    let resolved = resolve_to_cwd(path, cwd);
    if resolved.exists() {
        return resolved;
    }
    let s = resolved.to_string_lossy().into_owned();
    let am_pm = am_pm_variant(&s);
    if am_pm != s && Path::new(&am_pm).exists() {
        return PathBuf::from(am_pm);
    }
    let nfd: String = s.nfd().collect();
    if nfd != s && Path::new(&nfd).exists() {
        return PathBuf::from(nfd);
    }
    let curly = s.replace('\'', "\u{2019}");
    if curly != s && Path::new(&curly).exists() {
        return PathBuf::from(curly);
    }
    let nfd_curly = nfd.replace('\'', "\u{2019}");
    if nfd_curly != s && Path::new(&nfd_curly).exists() {
        return PathBuf::from(nfd_curly);
    }
    resolved
}

/// `/ (AM|PM)\./gi` -> narrow no-break space before AM/PM.
fn am_pm_variant(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let b = s.as_bytes();
    while i < b.len() {
        if b[i] == b' ' && i + 3 < b.len() && b[i + 1].is_ascii() && b[i + 2].is_ascii() {
            let word = &s[i + 1..i + 3];
            if (word.eq_ignore_ascii_case("am") || word.eq_ignore_ascii_case("pm"))
                && b[i + 3] == b'.'
            {
                out.push('\u{202F}');
                out.push_str(word);
                out.push('.');
                i += 4;
                continue;
            }
        }
        let c = s[i..].chars().next().unwrap_or(' ');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

const ERRNO: &[(i32, &str, &str)] = &[
    (1, "EPERM", "operation not permitted"),
    (2, "ENOENT", "no such file or directory"),
    (5, "EIO", "i/o error"),
    (13, "EACCES", "permission denied"),
    (17, "EEXIST", "file already exists"),
    (20, "ENOTDIR", "not a directory"),
    (21, "EISDIR", "illegal operation on a directory"),
    (24, "EMFILE", "too many open files"),
    (28, "ENOSPC", "no space left on device"),
    (30, "EROFS", "read-only file system"),
    (36, "ENAMETOOLONG", "name too long"),
    (40, "ELOOP", "too many symbolic links encountered"),
];

/// A Node-style error message for a failed filesystem call, e.g.
/// `ENOENT: no such file or directory, access '/x'`. Calls on an open file
/// (`read`) name no path.
pub fn node_error(e: &std::io::Error, syscall: &str, path: Option<&Path>) -> String {
    let Some((_, code, text)) = ERRNO.iter().find(|(n, _, _)| Some(*n) == e.raw_os_error()) else {
        return match path {
            Some(p) => format!("{e}, {syscall} '{}'", p.display()),
            None => format!("{e}, {syscall}"),
        };
    };
    match path {
        Some(p) => format!("{code}: {text}, {syscall} '{}'", p.display()),
        None => format!("{code}: {text}, {syscall}"),
    }
}

pub fn error_code(e: &std::io::Error) -> &'static str {
    ERRNO
        .iter()
        .find(|(n, _, _)| Some(*n) == e.raw_os_error())
        .map_or("UNKNOWN", |(_, code, _)| code)
}

/// `access(path, mode)` as Node does it (follows links, checks permissions).
pub fn access(path: &Path, mode: libc::c_int) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from_raw_os_error(libc::ENOENT))?;
    // SAFETY: `c` is a valid NUL-terminated string for the duration of the call.
    if unsafe { libc::access(c.as_ptr(), mode) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Node's `path.relative(from, to)` for absolute, normalized paths.
pub fn relative(from: &Path, to: &Path) -> String {
    let a: Vec<_> = from.components().collect();
    let b: Vec<_> = to.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<String> = vec!["..".to_string(); a.len() - common];
    parts.extend(
        b[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_like_node() {
        let cwd = Path::new("/w/project");
        assert_eq!(
            resolve_to_cwd("a/../b.txt", cwd),
            PathBuf::from("/w/project/b.txt")
        );
        assert_eq!(
            resolve_to_cwd("@src/x", cwd),
            PathBuf::from("/w/project/src/x")
        );
        assert_eq!(
            resolve_to_cwd("/etc/./hosts", cwd),
            PathBuf::from("/etc/hosts")
        );
        assert_eq!(
            resolve_to_cwd("file:///tmp/a%20b", cwd),
            PathBuf::from("/tmp/a b")
        );
        assert_eq!(am_pm_variant("Shot 1.02 PM.png"), "Shot 1.02\u{202F}PM.png");
    }
}
