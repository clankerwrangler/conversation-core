//! Pi-compatible built-in tools: `read`, `bash`, `edit`, `write`, `grep`, `find`, and `ls`.
//! Their names, parameters, output text, limits, and edit rules are compatible.
//!
//! A call runs on its own thread after the core has saved it. Arguments are prepared, coerced,
//! and validated; a thrown error becomes an error result containing only its message.

mod bash;
mod edit;
mod find;
mod grep;
mod image;
mod ls;
mod paths;
mod read;
mod truncate;
mod write;

use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub const NAMES: [&str; 7] = ["read", "bash", "edit", "write", "grep", "find", "ls"];

/// Responses-format definitions for the built-in tools.
pub fn definitions() -> &'static [Value] {
    static DEFS: OnceLock<Vec<Value>> = OnceLock::new();
    DEFS.get_or_init(|| serde_json::from_str(include_str!("pi-tools.json")).expect("pi-tools.json"))
}

pub fn definition(name: &str) -> Option<&'static Value> {
    definitions()
        .iter()
        .find(|d| d.get("name").and_then(Value::as_str) == Some(name))
}

/// A tool result: `{content, details}` plus whether it is an error.
pub struct Output {
    pub content: Vec<Value>,
    pub details: Option<Value>,
    pub is_error: bool,
}

impl Output {
    pub fn text(text: impl Into<String>) -> Output {
        Output {
            content: vec![json!({"type": "text", "text": text.into()})],
            details: None,
            is_error: false,
        }
    }
}

type ToolResult = Result<Output, String>;

/// Stops a running call: sets the flag tools check, and kills a running shell's process group.
#[derive(Default)]
pub struct Cancel {
    flag: AtomicBool,
    group: Mutex<Option<i32>>,
}

impl Cancel {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        if let Ok(group) = self.group.lock()
            && let Some(pid) = *group
        {
            kill_tree(pid);
        }
    }

    pub fn cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    fn set_group(&self, pid: Option<i32>) {
        if let Ok(mut group) = self.group.lock() {
            *group = pid;
        }
        if pid.is_some() && self.cancelled() {
            self.cancel();
        }
    }
}

/// `killProcessTree` on Linux: SIGKILL the process group, else the process.
fn kill_tree(pid: i32) {
    // SAFETY: plain signal sends; failure only means the process is already gone.
    unsafe {
        if libc::kill(-pid, libc::SIGKILL) != 0 {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

pub struct Context {
    pub cwd: PathBuf,
    /// PI_SESSION_ID, PI_PROVIDER, PI_MODEL, PI_REASONING_LEVEL for bash.
    pub env: Vec<(String, String)>,
    pub cancel: Arc<Cancel>,
}

/// Run one built-in call. Returns the tool result as the core saves it: `{content, details?, isError}`.
pub fn run(name: &str, arguments: &Value, ctx: &Context) -> Value {
    let output = prepare(name, arguments).and_then(|args| execute(name, &args, ctx));
    match output {
        Ok(out) => {
            let mut result = json!({"content": out.content, "isError": out.is_error});
            if let Some(details) = out.details {
                result["details"] = details;
            }
            result
        }
        Err(message) => {
            json!({"content": [{"type": "text", "text": message}], "details": {}, "isError": true})
        }
    }
}

fn execute(name: &str, args: &Map<String, Value>, ctx: &Context) -> ToolResult {
    match name {
        "read" => read::run(args, ctx),
        "bash" => bash::run(args, ctx),
        "edit" => edit::run(args, ctx),
        "write" => write::run(args, ctx),
        "grep" => grep::run(args, ctx),
        "find" => find::run(args, ctx),
        "ls" => ls::run(args, ctx),
        other => Err(format!("Tool {other} not found")),
    }
}

// ---------------------------------------------------------------------------------------------
// Arguments: preparation, coercion, and validation against the
// tool's TypeBox schema (not the strict wire schema).
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Ty {
    Str,
    Num,
    Bool,
    /// `edit`'s `edits`: an array of `{oldText: string, newText: string}`.
    Edits,
}

struct Prop {
    name: &'static str,
    ty: Ty,
    required: bool,
}

const fn req(name: &'static str, ty: Ty) -> Prop {
    Prop {
        name,
        ty,
        required: true,
    }
}

const fn opt(name: &'static str, ty: Ty) -> Prop {
    Prop {
        name,
        ty,
        required: false,
    }
}

fn schema(name: &str) -> &'static [Prop] {
    use Ty::*;
    const READ: &[Prop] = &[req("path", Str), opt("offset", Num), opt("limit", Num)];
    const BASH: &[Prop] = &[req("command", Str), opt("timeout", Num)];
    const EDIT: &[Prop] = &[req("path", Str), req("edits", Edits)];
    const WRITE: &[Prop] = &[req("path", Str), req("content", Str)];
    const GREP: &[Prop] = &[
        req("pattern", Str),
        opt("path", Str),
        opt("glob", Str),
        opt("ignoreCase", Bool),
        opt("literal", Bool),
        opt("context", Num),
        opt("limit", Num),
    ];
    const FIND: &[Prop] = &[req("pattern", Str), opt("path", Str), opt("limit", Num)];
    const LS: &[Prop] = &[opt("path", Str), opt("limit", Num)];
    match name {
        "read" => READ,
        "bash" => BASH,
        "edit" => EDIT,
        "write" => WRITE,
        "grep" => GREP,
        "find" => FIND,
        _ => LS,
    }
}

fn prepare(name: &str, arguments: &Value) -> Result<Map<String, Value>, String> {
    let mut args = arguments.clone();
    if name == "edit" {
        args = edit::prepare_arguments(args);
    }
    let Value::Object(obj) = &args else {
        return Err(validation_error(
            name,
            &[(
                "root".into(),
                format!("expected object, got {}", type_name(&args)),
            )],
            &args,
        ));
    };
    let props = schema(name);
    let coerced = coerce(obj, props);
    let errors = validate(&coerced, props);
    if errors.is_empty() {
        Ok(coerced)
    } else {
        Err(validation_error(name, &errors, &args))
    }
}

fn validation_error(name: &str, errors: &[(String, String)], args: &Value) -> String {
    let lines: Vec<String> = errors
        .iter()
        .map(|(p, m)| format!("  - {p}: {m}"))
        .collect();
    format!(
        "Validation failed for tool \"{name}\":\n{}\n\nReceived arguments:\n{}",
        lines.join("\n"),
        serde_json::to_string_pretty(args).unwrap_or_default()
    )
}

fn coerce(obj: &Map<String, Value>, props: &[Prop]) -> Map<String, Value> {
    let mut out = obj.clone();
    // Pass 1: an optional null is dropped, as if omitted.
    for p in props {
        if !p.required && out.get(p.name) == Some(&Value::Null) {
            out.remove(p.name);
        }
    }
    // Pass 2 (TypeBox's conversion rules); pass 3 changes nothing further for these schemas.
    for p in props {
        if let Some(v) = out.get_mut(p.name) {
            *v = convert(p.ty, v);
        }
    }
    out
}

fn convert(ty: Ty, v: &Value) -> Value {
    match ty {
        Ty::Str => try_string(v).unwrap_or_else(|| v.clone()),
        Ty::Num => try_number(v).map_or_else(|| v.clone(), number),
        Ty::Bool => try_boolean(v).map_or_else(|| v.clone(), Value::Bool),
        Ty::Edits => match v {
            Value::Array(items) => Value::Array(items.iter().map(convert_edit).collect()),
            other => Value::Array(vec![convert_edit(other)]),
        },
    }
}

fn convert_edit(v: &Value) -> Value {
    let Value::Object(o) = v else {
        return v.clone();
    };
    let mut o = o.clone();
    for key in ["oldText", "newText"] {
        if let Some(x) = o.get_mut(key) {
            *x = convert(Ty::Str, x);
        }
    }
    Value::Object(o)
}

fn number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15 {
        json!(n as i64)
    } else {
        json!(n)
    }
}

fn try_string(v: &Value) -> Option<Value> {
    match v {
        Value::Bool(b) => Some(json!(b.to_string())),
        Value::Number(n) => n
            .as_f64()
            .filter(|f| f.is_finite())
            .map(|f| json!(js_num(f))),
        Value::Null => Some(json!("null")),
        Value::String(_) => Some(v.clone()),
        _ => None,
    }
}

fn try_number(v: &Value) -> Option<f64> {
    match v {
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::Null => Some(0.0),
        Value::String(s) => {
            if let Some(n) = js_to_number(s).filter(|f| f.is_finite()) {
                return Some(n);
            }
            match s.to_lowercase().as_str() {
                "false" => Some(0.0),
                "true" => Some(1.0),
                _ => big_int_of_string(s),
            }
        }
        _ => None,
    }
}

/// TypeBox's TryBigInt of a string: `12n`, `12.5` (truncated) or `12`, within safe integers.
fn big_int_of_string(s: &str) -> Option<f64> {
    let digits = |t: &str| {
        let t = t.strip_prefix('-').unwrap_or(t);
        !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) && (t == "0" || !t.starts_with('0'))
    };
    let int = if let Some(t) = s.strip_suffix('n') {
        digits(t).then_some(t)
    } else if let Some((a, b)) = s.split_once('.') {
        (digits(a) && !b.is_empty() && b.bytes().all(|c| c.is_ascii_digit())).then_some(a)
    } else {
        digits(s).then_some(s)
    }?;
    let n: f64 = int.parse().ok()?;
    (n.abs() <= 9_007_199_254_740_991.0).then_some(n)
}

fn try_boolean(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => match n.as_f64() {
            Some(0.0) => Some(false),
            Some(1.0) => Some(true),
            _ => None,
        },
        Value::Null => Some(false),
        Value::String(s) => {
            let lower = s.to_lowercase();
            if lower == "false" || s == "0" {
                Some(false)
            } else if lower == "true" || s == "1" {
                Some(true)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn validate(obj: &Map<String, Value>, props: &[Prop]) -> Vec<(String, String)> {
    let mut errors = Vec::new();
    for p in props.iter().filter(|p| p.required) {
        if !obj.contains_key(p.name) {
            errors.push((p.name.to_string(), "missing".to_string()));
        }
    }
    for p in props {
        let Some(v) = obj.get(p.name) else { continue };
        let expected = match p.ty {
            Ty::Str => "string",
            Ty::Num => "number",
            Ty::Bool => "boolean",
            Ty::Edits => "array",
        };
        let ok = match p.ty {
            Ty::Str => v.is_string(),
            Ty::Num => v.is_number(),
            Ty::Bool => v.is_boolean(),
            Ty::Edits => v.is_array(),
        };
        if !ok {
            errors.push((
                p.name.to_string(),
                format!("expected {expected}, got {}", type_name(v)),
            ));
            continue;
        }
        if let (Ty::Edits, Value::Array(items)) = (p.ty, v) {
            for (i, item) in items.iter().enumerate() {
                let at = format!("{}.{i}", p.name);
                let Value::Object(o) = item else {
                    errors.push((at, format!("expected object, got {}", type_name(item))));
                    continue;
                };
                for key in ["oldText", "newText"] {
                    if !o.contains_key(key) {
                        errors.push((format!("{at}.{key}"), "missing".to_string()));
                    }
                }
                for key in ["oldText", "newText"] {
                    if let Some(x) = o.get(key).filter(|x| !x.is_string()) {
                        errors.push((
                            format!("{at}.{key}"),
                            format!("expected string, got {}", type_name(x)),
                        ));
                    }
                }
            }
        }
    }
    errors
}

/// A schema's type name.
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        Value::String(_) => "string",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0) {
                "integer"
            } else {
                "number"
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Small JavaScript semantics the ports need.
// ---------------------------------------------------------------------------------------------

/// JavaScript whitespace and line terminators (what `trim` and `Number()` strip).
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

fn js_trim_end(s: &str) -> &str {
    s.trim_end_matches(is_js_space)
}

/// `Number(string)`; None for NaN.
fn js_to_number(s: &str) -> Option<f64> {
    let t = s.trim_matches(is_js_space);
    if t.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(rest) = t.strip_prefix(prefix) {
            if rest.is_empty() || !rest.chars().all(|c| c.is_digit(radix)) {
                return None;
            }
            return Some(rest.chars().fold(0.0, |acc, c| {
                acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
            }));
        }
    }
    let unsigned = t.strip_prefix(['+', '-']).unwrap_or(t);
    if unsigned == "Infinity" {
        return Some(if t.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    let valid = unsigned
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
        && unsigned
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || c == '.');
    if !valid {
        return None;
    }
    t.parse::<f64>().ok()
}

/// `String(number)` for the numbers tools print.
fn js_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n}");
        if n.abs() >= 1e21 {
            format!("{n:e}").replace('e', "e+")
        } else {
            s
        }
    }
}

/// A numeric argument after coercion.
fn num_arg(args: &Map<String, Value>, key: &str) -> Option<f64> {
    args.get(key).and_then(Value::as_f64)
}

fn str_arg<'a>(args: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// Treat an empty string like an omitted path (`path || "."`).
fn path_arg<'a>(args: &'a Map<String, Value>, key: &str) -> &'a str {
    str_arg(args, key).filter(|s| !s.is_empty()).unwrap_or(".")
}

/// Allow one write or edit at a time per real file.
fn with_file_lock<T>(path: &std::path::Path, f: impl FnOnce() -> T) -> T {
    static LOCKS: OnceLock<Mutex<std::collections::HashMap<PathBuf, Arc<Mutex<()>>>>> =
        OnceLock::new();
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let lock = {
        let mut map = LOCKS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.entry(key).or_default().clone()
    };
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coerces_and_validates_like_pi_schema() {
        let args = prepare("read", &json!({"path": "a", "offset": "5", "limit": null}))
            .unwrap_or_default();
        assert_eq!(Value::Object(args), json!({"path": "a", "offset": 5}));
        let err = prepare("write", &json!({"path": 3}))
            .err()
            .unwrap_or_default();
        assert!(err.starts_with("Validation failed for tool \"write\":\n  - content: missing\n\nReceived arguments:\n{\n  \"path\": 3\n}"), "{err}");
        let err = prepare("grep", &json!({"pattern": "x", "limit": {}}))
            .err()
            .unwrap_or_default();
        assert!(
            err.contains("  - limit: expected number, got object"),
            "{err}"
        );
        let err = prepare("edit", &json!({"path": "p", "edits": [{"oldText": "a"}]}))
            .err()
            .unwrap_or_default();
        assert!(err.contains("  - edits.0.newText: missing"), "{err}");
    }

    #[test]
    fn js_numbers() {
        assert_eq!(js_to_number(" 42 "), Some(42.0));
        assert_eq!(js_to_number("0x10"), Some(16.0));
        assert_eq!(js_to_number("abc"), None);
        assert_eq!(js_to_number("inf"), None);
        assert_eq!(js_num(2.5), "2.5");
        assert_eq!(js_num(10.0), "10");
    }
}
