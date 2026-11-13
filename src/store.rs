//! Session folder reader: the context rule over chunked logs.
//!
//! Layout (one session):
//!   meta.json, state.json
//!   checkpoints/<id>.json            {"id", "through", "messages": [...]}
//!   log/NNNNNN.jsonl                 records {"n", "t", "k", ...}
//!   log/NNNNNN.payload.jsonl         {"key", "v"}: tool arguments, results, model-only images
//!   log/NNNNNN.reasoning.jsonl       {"key", "v"}: full reasoning, encrypted or plain
//!   log/NNNNNN.images/<sha256>       images the user sent or was shown
//!
//! A message in a record may contain `{"$ref": {"f": "payload"|"reasoning", "k": key}}`
//! in place of a bulky value, and image blocks `{"type":"image","mimeType":..,"$img":sha}`.
//! A tool result's images meant for the user stay in the record as `"keep"` beside its
//! reference, so they outlive a payload file that a cleanup deletes.

use base64::Engine;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum Error {
    Usage(&'static str),
    Io(PathBuf, std::io::Error),
    Json(String),
    Format(String),
    /// Another core process holds the session's writer lock.
    Locked(PathBuf),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(u) => write!(f, "usage: {u}"),
            Error::Io(p, e) => write!(f, "{}: {e}", p.display()),
            Error::Json(e) => write!(f, "json: {e}"),
            Error::Format(e) => write!(f, "format: {e}"),
            Error::Locked(p) => write!(f, "{}: another core is writing this session", p.display()),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e.to_string())
    }
}

type Result<T> = std::result::Result<T, Error>;

/// Record kinds that are sent to the model.
const MODEL_KINDS: [&str; 3] = ["user", "assistant", "tool"];

/// The session's writer lock (`writer.lock`, specification/CoreWriterLock.tla): one core
/// process at a time reads a session's tail and appends to it, so record numbers stay
/// unique. Taken without waiting: a second core on the same session fails at once. The
/// operating system releases it when the holder stops, even after a crash. Readers
/// (`history`, `context`) never take it.
pub struct WriterLock {
    _file: fs::File,
}

/// Takes the writer lock of the session in `dir` (created if missing).
pub fn lock_writer(dir: &Path) -> Result<WriterLock> {
    use std::os::fd::AsRawFd;
    fs::create_dir_all(dir).map_err(|e| Error::Io(dir.to_path_buf(), e))?;
    let path = dir.join("writer.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| Error::Io(path.clone(), e))?;
    loop {
        // SAFETY: flock on a descriptor this function owns; released when the lock is dropped.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(WriterLock { _file: file });
        }
        let e = std::io::Error::last_os_error();
        match e.kind() {
            std::io::ErrorKind::Interrupted => continue,
            std::io::ErrorKind::WouldBlock => return Err(Error::Locked(dir.to_path_buf())),
            _ => return Err(Error::Io(path, e)),
        }
    }
}

pub struct Session {
    dir: PathBuf,
}

struct Chunk {
    number: u32,
    records: Vec<Value>,
}

impl Session {
    /// A new, empty session folder with its first chunk.
    pub fn create(dir: &Path, id: &str) -> Result<Session> {
        for sub in ["log", "checkpoints"] {
            let path = dir.join(sub);
            fs::create_dir_all(&path).map_err(|e| Error::Io(path, e))?;
        }
        write_new(&dir.join("state.json"), "{}")?;
        let session = Session {
            dir: dir.to_path_buf(),
        };
        session.append_line(
            &session.log_path(1, None),
            &serde_json::json!({"n": 1, "k": "chunk", "chunk": 1, "prev": null}),
        )?;
        // meta.json last: a folder without it is not a session yet.
        write_new(
            &dir.join("meta.json"),
            &serde_json::json!({"format": 1, "id": id}).to_string(),
        )?;
        Ok(session)
    }

    /// Append a model-visible record, moving bulky parts to side files first.
    /// Returns the record's sequence number.
    pub fn append(&self, kind: &str, message: &Value) -> Result<u64> {
        self.append_record(kind, message, serde_json::json!({}))
    }

    /// Open a reply that is still streaming: its first tool call is complete, so the
    /// reply so far is saved before that call runs. Later parts follow with `append_more`.
    pub fn append_open_reply(&self, message: &Value) -> Result<u64> {
        self.append_record("assistant", message, serde_json::json!({"open": true}))
    }

    /// More blocks of an open reply. With `end` (usage, stop reason) it closes the reply.
    pub fn append_more(&self, reply: u64, blocks: &[Value], end: Option<&Value>) -> Result<u64> {
        let message = serde_json::json!({"role": "assistant", "content": blocks});
        let mut extra = serde_json::json!({"reply": reply});
        // The reply's closing fields: usage and stopReason, and for a failure errorMessage
        // and providerError.
        if let Some(end) = end {
            extra["end"] = end.clone();
        }
        self.append_record("assistant-more", &message, extra)
    }

    fn append_record(&self, kind: &str, message: &Value, extra: Value) -> Result<u64> {
        let (chunk, last) = self.tail()?;
        self.write_record(chunk, last + 1, &now_iso(), kind, message, extra)?;
        Ok(last + 1)
    }

    /// Record `n` of `chunk` with time `t`. `append` finds the position itself; an import
    /// (`core import`) keeps it, since it writes many records in a row with their own times.
    pub fn write_record(
        &self,
        chunk: u32,
        n: u64,
        t: &str,
        kind: &str,
        message: &Value,
        extra: Value,
    ) -> Result<()> {
        let mut sides: Vec<(&'static str, Value)> = Vec::new();
        let stored = self.split(chunk, n, message, &mut sides)?;
        for (file, line) in &sides {
            self.append_line(&self.log_path(chunk, Some(file)), line)?;
        }
        let mut record = serde_json::json!({"n": n, "t": t, "k": kind, "m": stored});
        if let (Value::Object(r), Value::Object(e)) = (&mut record, extra) {
            for (key, value) in e {
                if !["n", "t", "k", "m"].contains(&key.as_str()) {
                    r.insert(key, value);
                }
            }
        }
        self.append_line(&self.log_path(chunk, None), &record)
    }

    /// A record the model never sees (`app` bookkeeping, `settings`): `{n, t, k, ...fields}`.
    pub fn append_entry(&self, kind: &str, fields: Value) -> Result<u64> {
        let (chunk, last) = self.tail()?;
        self.write_entry(chunk, last + 1, &now_iso(), kind, fields)?;
        Ok(last + 1)
    }

    /// `append_entry` at a given position and time (see `write_record`).
    pub fn write_entry(
        &self,
        chunk: u32,
        n: u64,
        t: &str,
        kind: &str,
        fields: Value,
    ) -> Result<()> {
        let mut record = serde_json::json!({"n": n, "t": t, "k": kind});
        if let (Value::Object(r), Value::Object(f)) = (&mut record, fields) {
            for (key, value) in f {
                if !["n", "t", "k"].contains(&key.as_str()) {
                    r.insert(key, value);
                }
            }
        }
        self.append_line(&self.log_path(chunk, None), &record)
    }

    /// The current chunk and the highest record number in it.
    pub fn position(&self) -> Result<(u32, u64)> {
        self.tail()
    }

    /// A message injected by the runner (pi's custom message): a `user` record whose
    /// `source` says who added it, with pi's `display` flag and `details`.
    pub fn append_injected(
        &self,
        message: &Value,
        source: &str,
        display: bool,
        text: bool,
        details: Option<&Value>,
    ) -> Result<u64> {
        let mut extra = serde_json::json!({"source": source, "display": display});
        if text {
            extra["text"] = Value::Bool(true);
        }
        if let Some(d) = details {
            extra["details"] = d.clone();
        }
        self.append_record("user", message, extra)
    }

    pub fn meta(&self) -> Result<Value> {
        Ok(serde_json::from_str(&read(&self.dir.join("meta.json"))?)?)
    }

    /// Record a model or reasoning change (pi's `model_change` / `thinking_level_change`)
    /// and keep the current choice in meta.json.
    pub fn select(
        &self,
        provider_model: Option<(&str, &str)>,
        reasoning: Option<&str>,
    ) -> Result<()> {
        let mut meta = self.meta()?;
        if let Some((provider, model)) = provider_model {
            self.append_entry(
                "settings",
                serde_json::json!({"type": "model_change", "provider": provider, "modelId": model}),
            )?;
            meta["model"] = serde_json::json!({"provider": provider, "id": model});
        }
        if let Some(level) = reasoning {
            self.append_entry(
                "settings",
                serde_json::json!({"type": "thinking_level_change", "thinkingLevel": level}),
            )?;
            meta["reasoning"] = Value::String(level.to_string());
        }
        self.write_durably(&self.dir.join("meta.json"), &serde_json::to_string(&meta)?)
    }

    /// Records numbered `from..=to` across chunks, references named by file. With `resolve`,
    /// bulky parts are read back; one whose stored data is gone stays a reference and the
    /// record gets `"expired": true`.
    pub fn history(&self, from: u64, to: u64, resolve: bool) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut resolver = Resolver {
            dir: self.dir.clone(),
            files: HashMap::new(),
            expired: false,
            lenient: true,
        };
        // Only the chunks that hold the range are read; a chunk's first line says where it starts.
        let numbers = self.chunk_numbers()?;
        let firsts = numbers
            .iter()
            .map(|&c| self.chunk_first(c))
            .collect::<Result<Vec<u64>>>()?;
        for (index, &number) in numbers.iter().enumerate() {
            let next = firsts.get(index + 1).copied();
            if next.is_some_and(|next| next <= from) {
                continue;
            }
            if firsts[index] > to {
                break;
            }
            let chunk = self.load_chunk(number)?;
            let last = chunk
                .records
                .last()
                .and_then(|r| r.get("n"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            for record in &chunk.records {
                let n = record.get("n").and_then(Value::as_u64).unwrap_or(0);
                if n < from || n > to {
                    continue;
                }
                let mut r = qualify(number, record);
                if resolve && let Some(m) = r.get("m").cloned() {
                    resolver.expired = false;
                    match resolver.resolve(&m) {
                        // Only the kept part (images meant for the user) is left.
                        Ok(full) if resolver.expired => {
                            r["m"] = full;
                            r["expired"] = Value::Bool(true);
                        }
                        Ok(full) => r["m"] = full,
                        Err(_) => r["expired"] = Value::Bool(true),
                    }
                }
                out.push(r);
            }
            if last >= to {
                break;
            }
        }
        Ok(out)
    }

    /// Where the current chunk begins: history starts here unless older records are asked for.
    pub fn current_start(&self) -> Result<u64> {
        let last = *self
            .chunk_numbers()?
            .last()
            .ok_or_else(|| Error::Format("no chunk".into()))?;
        self.chunk_first(last)
    }

    /// The number of a chunk's first record (its opening `chunk` record), from its first line.
    fn chunk_first(&self, number: u32) -> Result<u64> {
        use std::io::BufRead;
        let path = self.log_path(number, None);
        let file = fs::File::open(&path).map_err(|e| Error::Io(path.clone(), e))?;
        let mut line = String::new();
        std::io::BufReader::new(file)
            .read_line(&mut line)
            .map_err(|e| Error::Io(path.clone(), e))?;
        let first: Value = serde_json::from_str(&line)?;
        first
            .get("n")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::Format(format!("chunk {number} has no first record")))
    }

    /// History as handed out (see entries.rs): complete entries for records `from..=to`.
    pub fn entries(&self, from: u64, to: u64) -> Result<Vec<Value>> {
        let records = self.history(from, u64::MAX, true)?;
        Ok(crate::entries::entries(&records, to))
    }

    /// Project fields from matching message entries without loading their content payloads.
    /// The selection is generic: exact top-level message matches, top-level fields and
    /// selected `details` fields. Payload files are scanned once, one line at a time.
    pub fn projected_entries(&self, from: u64, to: u64, select: &Value) -> Result<Vec<Value>> {
        use std::io::BufRead;
        let criteria = select
            .get("where")
            .and_then(Value::as_object)
            .ok_or_else(|| Error::Format("history selection needs where".into()))?;
        if criteria.len() > 16 || criteria.values().any(|v| v.is_array() || v.is_object()) {
            return Err(Error::Format("invalid history selection where".into()));
        }
        let names = |key: &str| -> Result<Vec<String>> {
            let values = select
                .get(key)
                .and_then(Value::as_array)
                .ok_or_else(|| Error::Format(format!("history selection needs {key}")))?;
            if values.len() > 32 {
                return Err(Error::Format(format!("too many history {key}")));
            }
            values
                .iter()
                .map(|v| {
                    v.as_str()
                        .filter(|s| !s.is_empty() && s.len() <= 128)
                        .map(str::to_string)
                        .ok_or_else(|| Error::Format(format!("invalid history {key}")))
                })
                .collect()
        };
        let fields = names("fields")?;
        let detail_fields = names("details")?;
        let pick_details = |value: &Value| -> Value {
            let mut out = Map::new();
            for field in &detail_fields {
                if let Some((outer, inner)) = field.split_once('.') {
                    if inner.contains('.') || outer.is_empty() || inner.is_empty() {
                        continue;
                    }
                    if let Some(value) = value.get(outer).and_then(|nested| nested.get(inner)) {
                        let nested = out
                            .entry(outer.to_string())
                            .or_insert_with(|| Value::Object(Map::new()));
                        if let Some(nested) = nested.as_object_mut() {
                            nested.insert(inner.to_string(), value.clone());
                        }
                    }
                } else if let Some(value) = value.get(field) {
                    out.insert(field.clone(), value.clone());
                }
            }
            Value::Object(out)
        };
        let records = self.history(from, u64::MAX, false)?;
        let entries = crate::entries::entries(&records, to);
        let mut selected = Vec::new();
        // Each payload file is read once; only requested detail values are retained.
        let mut needed: HashMap<String, HashMap<String, Vec<usize>>> = HashMap::new();
        // Detail fields a reference keeps in the record (see Resolver::resolve), by entry.
        let mut kept: HashMap<usize, Map<String, Value>> = HashMap::new();
        for mut entry in entries {
            let Some(message) = entry.get("message").and_then(Value::as_object) else {
                continue;
            };
            if !criteria
                .iter()
                .all(|(key, value)| message.get(key) == Some(value))
            {
                continue;
            }
            let mut projected = Map::new();
            for field in &fields {
                if let Some(value) = message.get(field) {
                    projected.insert(field.clone(), value.clone());
                }
            }
            if !detail_fields.is_empty()
                && let Some(details) = message.get("details")
            {
                if let Some(reference) = details.get("$ref") {
                    let file = reference
                        .get("file")
                        .and_then(Value::as_str)
                        .filter(|f| side_file_path(f))
                        .ok_or_else(|| Error::Format("invalid history reference".into()))?;
                    let key = reference
                        .get("key")
                        .and_then(Value::as_str)
                        .ok_or_else(|| Error::Format("invalid history reference key".into()))?;
                    needed
                        .entry(file.to_string())
                        .or_default()
                        .entry(key.to_string())
                        .or_default()
                        .push(selected.len());
                    if let Some(Value::Object(keep)) = details.get("keep") {
                        kept.insert(selected.len(), keep.clone());
                    }
                } else {
                    projected.insert("details".into(), pick_details(details));
                }
            }
            entry["message"] = Value::Object(projected);
            selected.push(entry);
        }
        for (file, mut keys) in needed {
            let path = self.dir.join(&file);
            match fs::File::open(&path) {
                Ok(input) => {
                    let mut reader = std::io::BufReader::new(input);
                    let mut line = String::new();
                    while !keys.is_empty() {
                        line.clear();
                        if reader
                            .read_line(&mut line)
                            .map_err(|e| Error::Io(path.clone(), e))?
                            == 0
                        {
                            break;
                        }
                        if line.trim().is_empty() {
                            continue;
                        }
                        let value: Value = serde_json::from_str(&line)?;
                        let Some(key) = value.get("key").and_then(Value::as_str) else {
                            continue;
                        };
                        if let Some(indexes) = keys.remove(key) {
                            let stored = value.get("v").cloned().unwrap_or(Value::Null);
                            for index in indexes {
                                let mut details = stored.clone();
                                if let (Some(out), Some(keep)) =
                                    (details.as_object_mut(), kept.get(&index))
                                {
                                    out.extend(keep.clone());
                                }
                                selected[index]["message"]["details"] = pick_details(&details);
                            }
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // A cleanup deleted it, keeping named detail fields beside it: an entry
                    // whose requested fields were all kept reads as before, not expired.
                    let kept_file = match kept_file_for(&file) {
                        Some(name) => self.read_kept(&name)?,
                        None => None,
                    };
                    if let Some((kept_fields, values)) = kept_file {
                        let complete = detail_fields.iter().all(|field| {
                            let top = field
                                .split_once('.')
                                .map_or(field.as_str(), |(outer, _)| outer);
                            kept_fields.iter().any(|kept| kept == top)
                        });
                        keys.retain(|key, indexes| {
                            let Some(stored) = values.get(key) else {
                                return true;
                            };
                            for &index in indexes.iter() {
                                let mut details = stored.clone();
                                if let (Some(out), Some(keep)) =
                                    (details.as_object_mut(), kept.get(&index))
                                {
                                    out.extend(keep.clone());
                                }
                                selected[index]["message"]["details"] = pick_details(&details);
                            }
                            !complete
                        });
                    }
                }
                Err(error) => return Err(Error::Io(path, error)),
            }
            for indexes in keys.into_values() {
                for index in indexes {
                    selected[index]["expired"] = Value::Bool(true);
                    // What was kept (beside the reference, or by a cleanup) still shows.
                    if selected[index]["message"].get("details").is_none()
                        && let Some(keep) = kept.get(&index)
                    {
                        selected[index]["message"]["details"] =
                            pick_details(&Value::Object(keep.clone()));
                    }
                }
            }
        }
        Ok(selected)
    }

    /// Replies opened but never closed (a crash mid-reply), oldest first.
    pub fn unfinished_replies(&self) -> Result<Vec<u64>> {
        let mut open: Vec<u64> = Vec::new();
        for number in self.chunk_numbers()? {
            for record in self.load_chunk(number)?.records {
                let n = record.get("n").and_then(Value::as_u64).unwrap_or(0);
                match record.get("k").and_then(Value::as_str) {
                    Some("assistant") if record.get("open") == Some(&Value::Bool(true)) => {
                        open.push(n)
                    }
                    Some("assistant-more") if record.get("end").is_some() => {
                        let reply = record.get("reply").and_then(Value::as_u64);
                        open.retain(|o| Some(*o) != reply);
                    }
                    _ => {}
                }
            }
        }
        Ok(open)
    }

    /// The current chunk and the highest sequence number in it.
    fn tail(&self) -> Result<(u32, u64)> {
        let chunk = *self
            .chunk_numbers()?
            .last()
            .ok_or_else(|| Error::Format("no chunk".into()))?;
        let last = self
            .load_chunk(chunk)?
            .records
            .iter()
            .filter_map(|r| r.get("n").and_then(Value::as_u64))
            .max()
            .unwrap_or(0);
        Ok((chunk, last))
    }

    /// Write a checkpoint: the complete model context as of the last record.
    /// Order (specification/CoreCheckpoint.tla): content, marker, prune, seal.
    /// The caller writes checkpoints only when no tool call is pending.
    pub fn checkpoint(&self, messages: &[Value]) -> Result<String> {
        self.checkpoint_at(messages, &now_iso())
    }

    /// `checkpoint` with the marker's time given (an import keeps the original time).
    pub fn checkpoint_at(&self, messages: &[Value], t: &str) -> Result<String> {
        // A checkpoint is a context: like an edited context file, it may only refer to
        // stored data that exists (a cleanup may have deleted older data).
        if let Some(file) = stored_files(messages)
            .into_iter()
            .find(|file| !self.dir.join(file).exists())
        {
            return Err(Error::Format(format!(
                "checkpoint refers to stored data that is gone: {file}"
            )));
        }
        let (chunk, through) = self.tail()?;
        let id = format!("{chunk:06}-{}", through + 1);
        let content = serde_json::json!({"id": id, "through": through, "messages": messages});
        self.write_durably(
            &self.checkpoint_path(&id),
            &serde_json::to_string(&content)?,
        )?;
        let marker = serde_json::json!({"n": through + 1, "t": t, "k": "checkpoint", "id": id, "through": through});
        self.append_line(&self.log_path(chunk, None), &marker)?;
        self.prune()?;
        self.seal()?;
        Ok(id)
    }

    /// Finish whatever a crash interrupted: keep only the newest two checkpoint
    /// files (this also drops content no marker names), and start a new chunk if
    /// none was started since the newest marker. Run on open.
    pub fn recover(&self) -> Result<()> {
        self.prune()?;
        if let Some((chunk, _)) = self.newest_markers(1)?.first()
            && Some(chunk) == self.chunk_numbers()?.last()
        {
            self.seal()?;
        }
        Ok(())
    }

    /// The newest `count` checkpoint markers as (chunk, id), newest first.
    fn newest_markers(&self, count: usize) -> Result<Vec<(u32, String)>> {
        let mut found = Vec::new();
        for &number in self.chunk_numbers()?.iter().rev() {
            for record in self.load_chunk(number)?.records.iter().rev() {
                if let Some((id, _)) = checkpoint_marker(record) {
                    found.push((number, id));
                    if found.len() == count {
                        return Ok(found);
                    }
                }
            }
        }
        Ok(found)
    }

    fn prune(&self) -> Result<()> {
        let keep: Vec<String> = self
            .newest_markers(2)?
            .into_iter()
            .map(|(_, id)| id)
            .collect();
        let dir = self.dir.join("checkpoints");
        let entries = fs::read_dir(&dir).map_err(|e| Error::Io(dir.clone(), e))?;
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            let id = name.strip_suffix(".json").unwrap_or(&name);
            if !keep.iter().any(|k| k == id) {
                let path = entry.path();
                fs::remove_file(&path).map_err(|e| Error::Io(path, e))?;
            }
        }
        Ok(())
    }

    /// Start the next chunk. Creating it exclusively makes a repeated seal harmless.
    fn seal(&self) -> Result<()> {
        use std::io::Write;
        let (chunk, last) = self.tail()?;
        let path = self.log_path(chunk + 1, None);
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
            Err(e) => return Err(Error::Io(path, e)),
        };
        let first =
            serde_json::json!({"n": last + 1, "k": "chunk", "chunk": chunk + 1, "prev": chunk});
        file.write_all(format!("{first}\n").as_bytes())
            .and_then(|_| file.sync_data())
            .map_err(|e| Error::Io(path, e))
    }

    fn checkpoint_path(&self, id: &str) -> PathBuf {
        self.dir.join("checkpoints").join(format!("{id}.json"))
    }

    /// Write a whole file so it is either absent or complete: temporary file, sync, rename, sync the folder.
    fn write_durably(&self, path: &Path, body: &str) -> Result<()> {
        use std::io::Write;
        let tmp = path.with_extension("tmp");
        let mut file = fs::File::create(&tmp).map_err(|e| Error::Io(tmp.clone(), e))?;
        file.write_all(body.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|e| Error::Io(tmp.clone(), e))?;
        fs::rename(&tmp, path).map_err(|e| Error::Io(path.to_path_buf(), e))?;
        if let Some(parent) = path.parent() {
            fs::File::open(parent)
                .and_then(|d| d.sync_all())
                .map_err(|e| Error::Io(parent.to_path_buf(), e))?;
        }
        Ok(())
    }

    fn split(
        &self,
        chunk: u32,
        n: u64,
        message: &Value,
        sides: &mut Vec<(&'static str, Value)>,
    ) -> Result<Value> {
        let mut m = message.clone();
        let role = m
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut put = |file: &'static str, v: Value| {
            let key = format!("{n}.{}", sides.len());
            sides.push((file, serde_json::json!({"key": key, "v": v})));
            serde_json::json!({"$ref": {"f": file, "k": key}})
        };
        match role.as_str() {
            "assistant" => {
                if let Some(Value::Array(blocks)) = m.get_mut("content") {
                    for b in blocks {
                        let kind = b
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        if kind == "thinking"
                            && b.get("thinkingSignature").is_some_and(|v| !v.is_null())
                        {
                            let v = b["thinkingSignature"].take();
                            b["thinkingSignature"] = put("reasoning", v);
                        }
                        if kind == "toolCall" && b.get("arguments").is_some_and(|v| !v.is_null()) {
                            let v = b["arguments"].take();
                            b["arguments"] = put("payload", v);
                        }
                    }
                }
            }
            "toolResult" => {
                // Images meant for the user (`details.shownImages`, and the result's own
                // images when `details.imagesForUser`) outlive the payload: they go to the
                // session's images, and the record keeps them beside the reference.
                let for_user = m.pointer("/details/imagesForUser") == Some(&Value::Bool(true));
                let mut keep_details = Map::new();
                if let Some(Value::Object(details)) = m.get_mut("details") {
                    if let Some(Value::Array(images)) = details.remove("shownImages") {
                        let mut images = images;
                        for image in &mut images {
                            self.store_image_block(chunk, image)?;
                        }
                        keep_details.insert("shownImages".into(), Value::Array(images));
                    }
                    if for_user {
                        keep_details.insert("imagesForUser".into(), Value::Bool(true));
                    }
                }
                let mut keep_content = Vec::new();
                if for_user && let Some(Value::Array(blocks)) = m.get_mut("content") {
                    for block in blocks.iter_mut() {
                        if block.get("type").and_then(Value::as_str) == Some("image") {
                            self.store_image_block(chunk, block)?;
                            keep_content.push(block.clone());
                        }
                    }
                }
                for field in ["content", "details"] {
                    if m.get(field).is_some_and(|v| !v.is_null()) {
                        let v = m[field].take();
                        let mut reference = put("payload", v);
                        if field == "details" && !keep_details.is_empty() {
                            reference["keep"] = Value::Object(std::mem::take(&mut keep_details));
                        } else if field == "content" && !keep_content.is_empty() {
                            reference["keep"] = Value::Array(std::mem::take(&mut keep_content));
                        }
                        m[field] = reference;
                    }
                }
            }
            "user" => {
                if let Some(Value::Array(blocks)) = m.get_mut("content") {
                    for b in blocks {
                        if b.get("data").is_some() {
                            self.store_image_block(chunk, b)?;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(m)
    }

    /// An image block's data moved to the session's images, the block keeping `"$img"`.
    fn store_image_block(&self, chunk: u32, block: &mut Value) -> Result<()> {
        if block.get("type").and_then(Value::as_str) != Some("image") {
            return Ok(());
        }
        if let Some(data) = block
            .get("data")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            let sha = self.store_image(chunk, &data)?;
            if let Some(obj) = block.as_object_mut() {
                obj.remove("data");
                obj.insert("$img".into(), Value::String(sha));
            }
        }
        Ok(())
    }

    fn store_image(&self, chunk: u32, data: &str) -> Result<String> {
        use sha2::Digest;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| Error::Format("image is not base64".into()))?;
        let sha = format!("{:x}", sha2::Sha256::digest(&bytes));
        let dir = self.dir.join("log").join(format!("{chunk:06}.images"));
        fs::create_dir_all(&dir).map_err(|e| Error::Io(dir.clone(), e))?;
        let path = dir.join(&sha);
        if !path.exists() {
            fs::write(&path, &bytes).map_err(|e| Error::Io(path.clone(), e))?;
        }
        Ok(sha)
    }

    fn log_path(&self, chunk: u32, side: Option<&str>) -> PathBuf {
        let name = match side {
            Some(f) => format!("{chunk:06}.{f}.jsonl"),
            None => format!("{chunk:06}.jsonl"),
        };
        self.dir.join("log").join(name)
    }

    /// Append one JSON line and sync it to disk.
    fn append_line(&self, path: &Path, value: &Value) -> Result<()> {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| Error::Io(path.to_path_buf(), e))?;
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        file.write_all(line.as_bytes())
            .and_then(|_| file.sync_data())
            .map_err(|e| Error::Io(path.to_path_buf(), e))
    }

    pub fn open(dir: &Path) -> Result<Session> {
        let meta = dir.join("meta.json");
        let value: Value = serde_json::from_str(&read(&meta)?)?;
        match value.get("format").and_then(Value::as_u64) {
            Some(1) => Ok(Session {
                dir: dir.to_path_buf(),
            }),
            other => Err(Error::Format(format!("unsupported format {other:?}"))),
        }
    }

    /// The context exactly as the model receives it: `context_refs` with every
    /// reference replaced by the stored data it names.
    pub fn context(&self) -> Result<Vec<Value>> {
        self.resolve(&self.context_refs()?)
    }

    /// Replace references (`{"$ref":{"file","key"}}`, image `"$img": path`) with stored data.
    /// References may only name side files and images inside this session's log.
    pub fn resolve(&self, messages: &[Value]) -> Result<Vec<Value>> {
        let mut resolver = Resolver {
            dir: self.dir.clone(),
            files: HashMap::new(),
            expired: false,
            lenient: false,
        };
        messages.iter().map(|m| resolver.resolve(m)).collect()
    }

    /// The context rule: the latest checkpoint's messages, then every model record
    /// after it. Bulky parts stay references to their stored data.
    pub fn context_refs(&self) -> Result<Vec<Value>> {
        Ok(self.context_parts()?.0)
    }

    /// The stored files (session-relative) that the current context and the latest two
    /// checkpoints refer to: what a cleanup outside the core must not delete.
    pub fn referenced(&self) -> Result<Vec<String>> {
        let mut out = stored_files(&self.context_refs()?);
        for (_, id) in self.newest_markers(2)? {
            out.extend(stored_files(&self.checkpoint_messages(&id)?));
        }
        Ok(out.into_iter().collect())
    }

    /// Run `f` holding `references.lock` (specification/ReaperReferences.tla). A cleanup holds
    /// it from asking `core referenced` until it is done deleting; the core holds it while
    /// accepting a new context, so a new reference never names a file being deleted. The
    /// operating system releases it when its holder stops.
    pub fn with_references_lock<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        use std::os::fd::AsRawFd;
        let path = self.dir.join("references.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| Error::Io(path.clone(), e))?;
        // SAFETY: flock on a descriptor this function owns; released when `file` is dropped.
        while unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(Error::Io(path, e));
            }
        }
        let result = f();
        drop(file);
        result
    }

    /// The cleanup: delete the tool output and reasoning files (`log/NNNNNN.payload.jsonl`,
    /// `log/NNNNNN.reasoning.jsonl`) of every chunk all of whose records (besides its
    /// timeless opening record) are provably older than `cutoff_ms` (a record with a
    /// missing or unreadable time keeps its chunk), unless
    /// `referenced` names them. The current chunk is never touched, nor the messages and the
    /// images meant for the user, which live elsewhere.
    ///
    /// Before a payload file goes, the top-level `keep_fields` of each tool result's
    /// `details` are written to `log/NNNNNN.kept.jsonl` (first line `{"fields": [...]}`),
    /// durably, so the runner's own small facts outlive the bulky output: a history
    /// selection of only kept fields reads them as before.
    ///
    /// Everything happens under references.lock (specification/ReaperReferences.tla), so
    /// the core never accepts a reference to a file while it is being deleted. Returns
    /// the deleted files (session-relative) and their size in bytes.
    pub fn reap(&self, cutoff_ms: i64, keep_fields: &[String]) -> Result<(Vec<String>, u64)> {
        self.with_references_lock(|| {
            let referenced: std::collections::BTreeSet<String> =
                self.referenced()?.into_iter().collect();
            let numbers = self.chunk_numbers()?;
            let Some((_, older)) = numbers.split_last() else {
                return Ok((Vec::new(), 0));
            };
            let (mut deleted, mut bytes) = (Vec::new(), 0);
            for &number in older {
                let chunk = self.load_chunk(number)?;
                // The chunk's own opening record carries no time; every other record must
                // carry a readable one older than the cutoff.
                let mut timed = chunk
                    .records
                    .iter()
                    .filter(|r| r.get("k").and_then(Value::as_str) != Some("chunk"))
                    .peekable();
                let old = timed.peek().is_some()
                    && timed.all(|r| {
                        r.get("t")
                            .and_then(Value::as_str)
                            .and_then(ms_from_iso)
                            .is_some_and(|t| t < cutoff_ms)
                    });
                if !old {
                    continue;
                }
                for kind in ["payload", "reasoning"] {
                    let name = format!("log/{number:06}.{kind}.jsonl");
                    if referenced.contains(&name) {
                        continue;
                    }
                    let path = self.dir.join(&name);
                    let info = match fs::metadata(&path) {
                        Ok(info) => info,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(e) => return Err(Error::Io(path, e)),
                    };
                    if kind == "payload" {
                        self.keep_details(&chunk, &name, keep_fields)?;
                    }
                    fs::remove_file(&path).map_err(|e| Error::Io(path.clone(), e))?;
                    bytes += info.len();
                    deleted.push(name);
                }
            }
            Ok((deleted, bytes))
        })
    }

    /// Write to the chunk's kept file (replacing any earlier one), from its payload file
    /// `payload`: every tool call's arguments whole (`"whole": true`), and the
    /// `keep_fields` of every tool result's stored `details`, an empty object when it has
    /// none of them (so a selection of kept fields still reads it as complete).
    fn keep_details(&self, chunk: &Chunk, payload: &str, keep_fields: &[String]) -> Result<()> {
        use std::io::BufRead;
        fn references(value: &Value, out: &mut std::collections::HashSet<String>) {
            match value {
                Value::Object(map) => {
                    if let Some(key) = map
                        .get("$ref")
                        .and_then(|r| r.get("k"))
                        .and_then(Value::as_str)
                    {
                        out.insert(key.to_string());
                    }
                    map.values().for_each(|v| references(v, out));
                }
                Value::Array(items) => items.iter().for_each(|v| references(v, out)),
                _ => {}
            }
        }
        let keys: std::collections::HashSet<String> = chunk
            .records
            .iter()
            .filter_map(|r| r.get("m")?.get("details")?.get("$ref")?.get("k")?.as_str())
            .map(str::to_string)
            .collect();
        let mut arguments = std::collections::HashSet::new();
        for record in &chunk.records {
            if record.get("k").and_then(Value::as_str) == Some("assistant")
                && let Some(message) = record.get("m")
            {
                references(message, &mut arguments);
            }
        }
        let path = self.dir.join(payload);
        let input = fs::File::open(&path).map_err(|e| Error::Io(path.clone(), e))?;
        let mut body = serde_json::to_string(&serde_json::json!({"fields": keep_fields}))? + "\n";
        for line in std::io::BufReader::new(input).lines() {
            let line = line.map_err(|e| Error::Io(path.clone(), e))?;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line)?;
            let (Some(key), Some(stored)) =
                (value.get("key").and_then(Value::as_str), value.get("v"))
            else {
                continue;
            };
            let line = if arguments.contains(key) {
                serde_json::json!({"key": key, "v": stored, "whole": true})
            } else if keys.contains(key) {
                let kept: Map<String, Value> = keep_fields
                    .iter()
                    .filter_map(|f| stored.get(f).map(|v| (f.clone(), v.clone())))
                    .collect();
                serde_json::json!({"key": key, "v": kept})
            } else {
                continue;
            };
            body += &serde_json::to_string(&line)?;
            body.push('\n');
        }
        let kept =
            kept_file_for(payload).ok_or_else(|| Error::Format("not a payload file".into()))?;
        self.write_durably(&self.dir.join(kept), &body)
    }

    /// A kept file: its field list and its values by key; None when there is none.
    fn read_kept(&self, name: &str) -> Result<Option<Kept>> {
        let path = self.dir.join(name);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Io(path, e)),
        };
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let header: Value = serde_json::from_str(lines.next().unwrap_or("{}"))?;
        let fields = header
            .get("fields")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let mut values = HashMap::new();
        for line in lines {
            let value: Value = serde_json::from_str(line)?;
            if let (Some(key), Some(v)) = (value.get("key").and_then(Value::as_str), value.get("v"))
            {
                values.insert(key.to_string(), v.clone());
            }
        }
        Ok(Some((fields, values)))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `context_refs`, and how many of its messages come from the latest checkpoint.
    pub fn context_parts(&self) -> Result<(Vec<Value>, usize)> {
        let numbers = self.chunk_numbers()?;
        let mut tail: Vec<Chunk> = Vec::new();
        let mut marker: Option<(String, u64)> = None;
        for &number in numbers.iter().rev() {
            let chunk = self.load_chunk(number)?;
            marker = chunk.records.iter().rev().find_map(checkpoint_marker);
            tail.push(chunk);
            if marker.is_some() {
                break;
            }
        }
        tail.reverse();
        let (mut messages, after) = match &marker {
            Some((id, through)) => (self.checkpoint_messages(id)?, *through),
            None => (Vec::new(), 0),
        };
        let from_checkpoint = messages.len();
        messages.extend(self.model_messages(&tail, after)?);
        Ok((messages, from_checkpoint))
    }

    /// Model messages of records numbered above `after`, with references.
    pub fn model_messages_after(&self, after: u64) -> Result<Vec<Value>> {
        let mut tail: Vec<Chunk> = Vec::new();
        for &number in self.chunk_numbers()?.iter().rev() {
            let chunk = self.load_chunk(number)?;
            let first = chunk
                .records
                .first()
                .and_then(|r| r.get("n"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            tail.push(chunk);
            if first <= after {
                break;
            }
        }
        tail.reverse();
        self.model_messages(&tail, after)
    }

    fn model_messages(&self, chunks: &[Chunk], after: u64) -> Result<Vec<Value>> {
        let mut messages: Vec<Value> = Vec::new();
        // A reply saved in parts is one message, at the position of its first part.
        let mut replies: HashMap<u64, usize> = HashMap::new();
        for chunk in chunks {
            for record in &chunk.records {
                let n = record.get("n").and_then(Value::as_u64).unwrap_or(0);
                let kind = record.get("k").and_then(Value::as_str).unwrap_or("");
                if n <= after {
                    continue;
                }
                let message = || {
                    record
                        .get("m")
                        .map(|m| qualify(chunk.number, &without_unadmitted_calls(m)))
                        .ok_or_else(|| Error::Format(format!("record {n} has no message")))
                };
                if kind == "assistant-more" {
                    let target = record
                        .get("reply")
                        .and_then(Value::as_u64)
                        .and_then(|r| replies.get(&r).copied());
                    let Some(index) = target else { continue };
                    let more = message()?;
                    let blocks = more
                        .get("content")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    if let Some(Value::Array(content)) = messages[index].get_mut("content") {
                        content.extend(blocks);
                    }
                    if let Some(end) = record.get("end") {
                        messages[index]["usage"] = end.get("usage").cloned().unwrap_or(Value::Null);
                        messages[index]["stopReason"] =
                            end.get("stopReason").cloned().unwrap_or(Value::Null);
                        for key in ["errorMessage", "providerError"] {
                            if let Some(value) = end.get(key) {
                                messages[index][key] = value.clone();
                            }
                        }
                    }
                    continue;
                }
                if !MODEL_KINDS.contains(&kind) {
                    continue;
                }
                if kind == "assistant" && record.get("open") == Some(&Value::Bool(true)) {
                    replies.insert(n, messages.len());
                }
                messages.push(message()?);
            }
        }
        Ok(messages)
    }

    /// The highest record number so far.
    pub fn last_seq(&self) -> Result<u64> {
        Ok(self.tail()?.1)
    }

    pub fn read_state(&self) -> Result<Value> {
        match self.read_file("state.json")? {
            Some(text) if !text.trim().is_empty() => Ok(serde_json::from_str(&text)?),
            _ => Ok(serde_json::json!({})),
        }
    }

    pub fn write_state(&self, state: &Value) -> Result<()> {
        self.write_file("state.json", &serde_json::to_string(state)?)
    }

    /// A file in the session folder, or None if it does not exist.
    pub fn read_file(&self, name: &str) -> Result<Option<String>> {
        let path = self.dir.join(name);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Io(path, e)),
        }
    }

    /// Replace a file in the session folder completely, or not at all.
    pub fn write_file(&self, name: &str, text: &str) -> Result<()> {
        self.write_durably(&self.dir.join(name), text)
    }

    fn chunk_numbers(&self) -> Result<Vec<u32>> {
        let log = self.dir.join("log");
        let entries = fs::read_dir(&log).map_err(|e| Error::Io(log.clone(), e))?;
        let mut numbers: Vec<u32> = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                let stem = name.strip_suffix(".jsonl")?;
                if stem.len() == 6 {
                    stem.parse().ok()
                } else {
                    None
                }
            })
            .collect();
        numbers.sort_unstable();
        Ok(numbers)
    }

    fn load_chunk(&self, number: u32) -> Result<Chunk> {
        let path = self.dir.join("log").join(format!("{number:06}.jsonl"));
        let mut text = read(&path)?;
        // Another process (a history reader) may see a line still being written: every
        // complete record ends with a newline, so an unterminated tail is not read yet.
        if !text.ends_with('\n')
            && let Some(end) = text.rfind('\n')
        {
            text.truncate(end + 1);
        }
        let records = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<std::result::Result<Vec<Value>, _>>()?;
        Ok(Chunk { number, records })
    }

    fn checkpoint_messages(&self, id: &str) -> Result<Vec<Value>> {
        let path = self.dir.join("checkpoints").join(format!("{id}.json"));
        let value: Value = serde_json::from_str(&read(&path)?)?;
        match value.get("messages") {
            Some(Value::Array(messages)) => Ok(messages.clone()),
            _ => Err(Error::Format(format!("checkpoint {id} has no messages"))),
        }
    }
}

/// Tool calls from a failed reply that never reached the provider stay in history,
/// marked `unadmitted`, and are never sent to the model.
fn without_unadmitted_calls(message: &Value) -> Value {
    let mut message = message.clone();
    if let Some(Value::Array(blocks)) = message.get_mut("content") {
        blocks.retain(|block| block.get("unadmitted") != Some(&Value::Bool(true)));
    }
    message
}

fn checkpoint_marker(record: &Value) -> Option<(String, u64)> {
    if record.get("k")?.as_str()? != "checkpoint" {
        return None;
    }
    Some((
        record.get("id")?.as_str()?.to_string(),
        record.get("n")?.as_u64()?,
    ))
}

fn write_new(path: &Path, body: &str) -> Result<()> {
    fs::write(path, body).map_err(|e| Error::Io(path.to_path_buf(), e))
}

/// Current UTC time as RFC 3339 with milliseconds, without a date library.
pub fn now_iso() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0) as i64;
    iso_from_ms(ms)
}

/// An RFC 3339 time (`YYYY-MM-DDTHH:MM:SS[.fff](Z|±HH:MM)`) as milliseconds since 1970
/// (UTC), the inverse of `iso_from_ms`; None for anything else.
pub fn ms_from_iso(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if !text.is_ascii()
        || b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| {
        let part = text.get(range)?;
        part.bytes()
            .all(|d| d.is_ascii_digit())
            .then(|| part.parse::<i64>().ok())?
    };
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    let (hour, minute, second) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
    // The zone: Z, or an offset of hours and minutes east of UTC.
    let (rest, offset_minutes) = if let Some(rest) = text.strip_suffix(['Z', 'z']) {
        (&rest[19..], 0)
    } else {
        let zone = text.get(text.len().checked_sub(6)?..)?;
        let zb = zone.as_bytes();
        let sign = match zb[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        if zb[3] != b':' {
            return None;
        }
        let at = text.len() - 6;
        let (oh, om) = (digits(at + 1..at + 3)?, digits(at + 4..at + 6)?);
        if oh > 23 || om > 59 || at < 19 {
            return None;
        }
        (&text[19..at], sign * (oh * 60 + om))
    };
    let millis = match rest.strip_prefix('.') {
        None if rest.is_empty() => 0,
        Some(fraction) if !fraction.is_empty() && fraction.bytes().all(|d| d.is_ascii_digit()) => {
            format!("{fraction:0<3}")[..3].parse().ok()?
        }
        _ => return None,
    };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Days-from-civil (Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(
        ((days * 86_400 + hour * 3600 + minute * 60 + second - offset_minutes * 60) * 1000)
            + millis,
    )
}

/// A time in milliseconds since 1970 as RFC 3339 UTC with milliseconds.
pub fn iso_from_ms(ms: i64) -> String {
    let (secs, millis) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|e| Error::Io(path.to_path_buf(), e))
}

/// A record's short references (`{"f","k"}`, image sha) as references that name the
/// actual file in the session folder, so they can be read and resolved anywhere.
fn qualify(chunk: u32, value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref") {
                let file = reference.get("f").and_then(Value::as_str).unwrap_or("");
                let key = reference.get("k").cloned().unwrap_or(Value::Null);
                let mut out = serde_json::json!({"$ref": {"file": format!("log/{chunk:06}.{file}.jsonl"), "key": key}});
                if let Some(keep) = map.get("keep") {
                    out["keep"] = qualify(chunk, keep);
                }
                return out;
            }
            let mut out = Map::new();
            for (key, inner) in map {
                match (key.as_str(), inner) {
                    ("$img", Value::String(sha)) if !sha.contains('/') => {
                        out.insert(
                            key.clone(),
                            Value::String(format!("log/{chunk:06}.images/{sha}")),
                        );
                    }
                    _ => {
                        out.insert(key.clone(), qualify(chunk, inner));
                    }
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| qualify(chunk, v)).collect()),
        other => other.clone(),
    }
}

/// The stored files (session-relative) that qualified messages refer to: `$ref` files and
/// `$img` paths.
fn stored_files(messages: &[Value]) -> std::collections::BTreeSet<String> {
    fn collect(value: &Value, out: &mut std::collections::BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                if let Some(file) = map
                    .get("$ref")
                    .and_then(|r| r.get("file"))
                    .and_then(Value::as_str)
                {
                    out.insert(file.to_string());
                }
                if let Some(Value::String(path)) = map.get("$img")
                    && path.contains('/')
                {
                    out.insert(path.clone());
                }
                map.values().for_each(|v| collect(v, out));
            }
            Value::Array(items) => items.iter().for_each(|v| collect(v, out)),
            _ => {}
        }
    }
    let mut out = std::collections::BTreeSet::new();
    messages.iter().for_each(|m| collect(m, &mut out));
    out
}

/// What a cleanup kept of a payload file: the kept field names, and the values by key.
type Kept = (Vec<String>, HashMap<String, Value>);

/// `log/NNNNNN.kept.jsonl` beside a payload file: what a cleanup kept of it.
fn kept_file_for(payload: &str) -> Option<String> {
    payload
        .strip_suffix(".payload.jsonl")
        .map(|stem| format!("{stem}.kept.jsonl"))
}

/// `log/NNNNNN.payload.jsonl`, `log/NNNNNN.reasoning.jsonl`: the only side files a reference may name.
fn side_file_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("log/") else {
        return false;
    };
    let Some((number, file)) = rest.split_once('.') else {
        return false;
    };
    number.len() == 6
        && number.bytes().all(|b| b.is_ascii_digit())
        && (file == "payload.jsonl" || file == "reasoning.jsonl")
}

/// `log/NNNNNN.images/<sha256>`: the only image paths a reference may name.
fn image_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("log/") else {
        return false;
    };
    let Some((number, sha)) = rest.split_once(".images/") else {
        return false;
    };
    number.len() == 6
        && number.bytes().all(|b| b.is_ascii_digit())
        && sha.len() == 64
        && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Resolves references against side files, loading each file once.
struct Resolver {
    dir: PathBuf,
    files: HashMap<String, HashMap<String, Value>>,
    /// Set when a reference's stored data was gone (its kept part, if any, stands in).
    expired: bool,
    /// History reads: a reference whose data is gone stays a reference instead of failing
    /// the whole message, so the rest (kept images included) is still read back.
    lenient: bool,
}

/// The chunk number in `log/NNNNNN.<side>.jsonl`.
fn side_file_chunk(file: &str) -> Option<u32> {
    file.strip_prefix("log/")?.get(..6)?.parse().ok()
}

impl Resolver {
    fn lookup(&mut self, file: &str, key: &str) -> Result<Value> {
        if !side_file_path(file) {
            return Err(Error::Format(format!(
                "reference to {file}: not a side file of this session"
            )));
        }
        if !self.files.contains_key(file) {
            let path = self.dir.join(file);
            let mut map = HashMap::new();
            if path.exists() {
                for line in read(&path)?.lines().filter(|l| !l.trim().is_empty()) {
                    let entry: Value = serde_json::from_str(line)?;
                    if let (Some(k), Some(v)) =
                        (entry.get("key").and_then(Value::as_str), entry.get("v"))
                    {
                        map.insert(k.to_string(), v.clone());
                    }
                }
            } else if let Some(kept) = kept_file_for(file) {
                // A cleanup deleted the payload file: what it kept whole (tool-call
                // arguments) reads as before.
                let kept = self.dir.join(kept);
                if kept.exists() {
                    for line in read(&kept)?
                        .lines()
                        .skip(1)
                        .filter(|l| !l.trim().is_empty())
                    {
                        let entry: Value = serde_json::from_str(line)?;
                        if entry.get("whole") == Some(&Value::Bool(true))
                            && let (Some(k), Some(v)) =
                                (entry.get("key").and_then(Value::as_str), entry.get("v"))
                        {
                            map.insert(k.to_string(), v.clone());
                        }
                    }
                }
            }
            self.files.insert(file.to_string(), map);
        }
        self.files[file]
            .get(key)
            .cloned()
            .ok_or_else(|| Error::Format(format!("{file} has no {key}")))
    }

    fn image(&self, path: &str) -> Result<String> {
        if !image_path(path) {
            return Err(Error::Format(format!(
                "image reference {path}: not an image of this session"
            )));
        }
        let full = self.dir.join(path);
        let bytes = fs::read(&full).map_err(|e| Error::Io(full.clone(), e))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    fn resolve(&mut self, value: &Value) -> Result<Value> {
        match value {
            Value::Object(map) => {
                if let Some(reference) = map.get("$ref") {
                    let file = reference.get("file").and_then(Value::as_str);
                    let key = reference.get("key").and_then(Value::as_str);
                    let (Some(file), Some(key)) = (file, key) else {
                        return Err(Error::Format("malformed $ref".into()));
                    };
                    let stored = self.lookup(file, key);
                    // A reference may keep part of its value in the record (images meant
                    // for the user). The stored value then holds the session's own image
                    // references too; kept fields complete it, or stand in when it is gone.
                    return match (stored, map.get("keep")) {
                        (Ok(value), None) => Ok(value),
                        (Ok(value), Some(keep)) => {
                            let chunk = side_file_chunk(file)
                                .ok_or_else(|| Error::Format("malformed $ref file".into()))?;
                            let mut value = self.resolve(&qualify(chunk, &value))?;
                            if let (Some(out), Value::Object(kept)) = (value.as_object_mut(), keep)
                            {
                                for (name, inner) in kept {
                                    out.insert(name.clone(), self.resolve(inner)?);
                                }
                            }
                            Ok(value)
                        }
                        (Err(_), Some(keep)) => {
                            self.expired = true;
                            self.resolve(keep)
                        }
                        (Err(_), None) if self.lenient => {
                            self.expired = true;
                            Ok(Value::Object(map.clone()))
                        }
                        (Err(error), None) => Err(error),
                    };
                }
                let mut out = Map::new();
                for (key, inner) in map {
                    if key == "$img" {
                        let path = inner
                            .as_str()
                            .ok_or_else(|| Error::Format("malformed $img".into()))?;
                        out.insert("data".into(), Value::String(self.image(path)?));
                    } else {
                        out.insert(key.clone(), self.resolve(inner)?);
                    }
                }
                Ok(Value::Object(out))
            }
            Value::Array(items) => Ok(Value::Array(
                items
                    .iter()
                    .map(|v| self.resolve(v))
                    .collect::<Result<_>>()?,
            )),
            other => Ok(other.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT: AtomicU32 = AtomicU32::new(0);
    const SHA: &str = "8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4";

    /// A throwaway session folder built from (file, lines) pairs.
    fn session(files: &[(&str, Vec<Value>)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "core-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(dir.join("log")).unwrap();
        fs::create_dir_all(dir.join("checkpoints")).unwrap();
        fs::write(dir.join("meta.json"), r#"{"format":1}"#).unwrap();
        for (name, lines) in files {
            let path = dir.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
            fs::write(path, body).unwrap();
        }
        dir
    }

    fn user(n: u64, text: &str) -> Value {
        json!({"n": n, "k": "user", "m": {"role": "user", "content": [{"type": "text", "text": text}]}})
    }

    fn context(dir: &Path) -> Vec<Value> {
        Session::open(dir).unwrap().context().unwrap()
    }

    #[test]
    fn projected_history_returns_only_requested_details_and_marks_missing_payloads() {
        let dir = session(&[
            (
                "log/000001.jsonl",
                vec![
                    json!({"n":1,"k":"chunk"}),
                    json!({"n":2,"k":"tool","m":{"role":"toolResult","toolCallId":"a", "toolName":"example",
                    "content":{"$ref":{"f":"payload","k":"2.0"}}, "details":{"$ref":{"f":"payload","k":"2.1"}},"isError":false}}),
                    json!({"n":3,"k":"tool","m":{"role":"toolResult","toolCallId":"b","toolName":"other",
                    "details":{"$ref":{"f":"payload","k":"3.1"}}}}),
                ],
            ),
            (
                "log/000001.payload.jsonl",
                vec![
                    json!({"key":"2.0","v":[{"type":"text","text":"large output"}]}),
                    json!({"key":"2.1","v":{"identity":{"value":"saved", "opaque":"private body"}, "large":"private body"}}),
                    json!({"key":"3.1","v":{"identity":"other"}}),
                ],
            ),
            (
                "log/000002.jsonl",
                vec![
                    json!({"n":4,"k":"chunk"}),
                    json!({"n":5,"k":"tool","m":{"role":"toolResult","toolCallId":"c","toolName":"example",
                    "details":{"$ref":{"f":"payload","k":"5.1"}},"isError":false}}),
                ],
            ),
            (
                "log/000002.payload.jsonl",
                vec![json!({"key":"5.1","v":{"identity":{"value":"new"}}})],
            ),
        ]);
        let session = Session::open(&dir).unwrap();
        let select = json!({"where":{"role":"toolResult","toolName":"example"},
            "fields":["role","toolCallId","toolName","isError"], "details":["identity.value"]});
        let got = session.projected_entries(1, u64::MAX, &select).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1]["message"]["details"]["identity"]["value"], "new");
        assert_eq!(
            got[0]["message"],
            json!({"role":"toolResult","toolCallId":"a","toolName":"example",
            "isError":false,"details":{"identity":{"value":"saved"}}})
        );
        assert!(
            !serde_json::to_string(&got)
                .unwrap()
                .contains("large output")
        );
        assert!(
            !serde_json::to_string(&got)
                .unwrap()
                .contains("private body")
        );
        fs::remove_file(dir.join("log/000001.payload.jsonl")).unwrap();
        let expired = session.projected_entries(1, u64::MAX, &select).unwrap();
        assert_eq!(expired[0]["expired"], true);
        assert!(expired[0]["message"].get("details").is_none());
        assert_eq!(expired[1]["message"]["details"]["identity"]["value"], "new");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn times_read_back_as_written() {
        for ms in [0, 1_794_557_090_262, 951_782_400_000, -86_400_000] {
            assert_eq!(ms_from_iso(&iso_from_ms(ms)), Some(ms));
        }
        assert_eq!(ms_from_iso("2026-11-13T09:11:58Z"), Some(1_794_561_118_000));
        assert_eq!(
            ms_from_iso("2026-11-13T09:11:58.5Z"),
            Some(1_794_561_118_500)
        );
        // Offsets name the same instant: 10:11:58+01:00 is 09:11:58Z.
        assert_eq!(
            ms_from_iso("2026-11-13T10:11:58+01:00"),
            Some(1_794_561_118_000)
        );
        assert_eq!(
            ms_from_iso("2026-11-13T04:41:58.250-04:30"),
            Some(1_794_561_118_250)
        );
        assert_eq!(ms_from_iso("2026-11-13t09:11:58z"), Some(1_794_561_118_000));
        for bad in [
            "",
            "2026-11-13",
            "2026-13-01T00:00:00Z",
            "2026-11-13T09:11:58.Z",
            "2026-11-13T09:11:58+0100",
            "2026-11-13T09:11:58+24:00",
            "2026-11-13T09:11:58",
            "Fri, 13 Nov 2026 09:11:58 GMT",
            "2026-11-13T09:11:5x+01:00",
        ] {
            assert_eq!(ms_from_iso(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_chunk_is_old_only_when_every_record_is_provably_old() {
        let old = "2026-01-01T00:00:00.000Z";
        let now = ms_from_iso("2026-03-01T00:00:00.000Z").unwrap();
        let chunk = |second: Value| {
            session(&[
                (
                    "log/000001.jsonl",
                    vec![
                        json!({"n": 1, "t": old, "k": "app", "ns": "x", "data": {}}),
                        second,
                    ],
                ),
                (
                    "log/000001.payload.jsonl",
                    vec![json!({"key": "2.0", "v": []})],
                ),
                (
                    "log/000002.jsonl",
                    vec![json!({"n": 3, "t": old, "k": "chunk"})],
                ),
            ])
        };
        for (label, second, deleted) in [
            (
                "a recent time with an offset",
                json!({"n": 2, "t": "2026-02-28T23:00:00+01:00", "k": "app", "ns": "x", "data": {}}),
                false,
            ),
            (
                "an unreadable time",
                json!({"n": 2, "t": "yesterday", "k": "app", "ns": "x", "data": {}}),
                false,
            ),
            (
                "no time",
                json!({"n": 2, "k": "app", "ns": "x", "data": {}}),
                false,
            ),
            (
                "an old time with an offset",
                json!({"n": 2, "t": "2026-01-01T05:00:00+05:00", "k": "app", "ns": "x", "data": {}}),
                true,
            ),
            // The core's own opening record of a chunk has no time and does not count.
            (
                "a timeless opening record",
                json!({"n": 2, "k": "chunk"}),
                true,
            ),
        ] {
            let dir = chunk(second);
            let (files, _) = Session::open(&dir)
                .unwrap()
                .reap(now - 14 * 86_400_000, &[])
                .unwrap();
            assert_eq!(!files.is_empty(), deleted, "{label}");
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn the_cleanup_keeps_named_detail_fields_and_projections_of_them_read_as_before() {
        let old = "2026-01-01T00:00:00.000Z";
        let dir = session(&[
            (
                "log/000001.jsonl",
                vec![
                    json!({"n": 1, "t": old, "k": "chunk"}),
                    json!({"n": 2, "t": old, "k": "assistant", "m": {"role": "assistant", "content": [
                    {"type": "toolCall", "id": "a", "name": "py", "arguments": {"$ref": {"f": "payload", "k": "2.a"}}}]}}),
                    json!({"n": 3, "t": old, "k": "tool", "m": {"role": "toolResult", "toolCallId": "a", "toolName": "py",
                    "content": {"$ref": {"f": "payload", "k": "2.0"}}, "details": {"$ref": {"f": "payload", "k": "2.1"}}}}),
                    json!({"n": 4, "t": old, "k": "tool", "m": {"role": "toolResult", "toolCallId": "b", "toolName": "py",
                    "details": {"$ref": {"f": "payload", "k": "4.1"}}}}),
                ],
            ),
            (
                "log/000001.payload.jsonl",
                vec![
                    json!({"key": "2.a", "v": {"code": "print('the code that ran')"}}),
                    json!({"key": "2.0", "v": [{"type": "text", "text": "big output"}]}),
                    json!({"key": "2.1", "v": {"executionId": "e1", "checkpoint": {"id": 7}, "big": "bulky body"}}),
                    json!({"key": "4.1", "v": {"unrelated": "only this"}}),
                ],
            ),
            (
                "log/000002.jsonl",
                vec![
                    json!({"n": 5, "t": old, "k": "chunk"}),
                    json!({"n": 6, "t": old, "k": "checkpoint", "id": "c1"}),
                ],
            ),
            (
                "log/000003.jsonl",
                vec![json!({"n": 7, "t": old, "k": "chunk"})],
            ),
            (
                "checkpoints/c1.json",
                vec![
                    json!({"id": "c1", "through": 6, "messages": [{"role": "user", "content": "summary"}]}),
                ],
            ),
        ]);
        let session = Session::open(&dir).unwrap();
        let now = ms_from_iso("2026-03-01T00:00:00.000Z").unwrap();
        let keep = ["executionId".to_string(), "checkpoint".to_string()];
        let (files, _) = session.reap(now - 14 * 86_400_000, &keep).unwrap();
        assert_eq!(files, ["log/000001.payload.jsonl"]);
        let kept = fs::read_to_string(dir.join("log/000001.kept.jsonl")).unwrap();
        assert!(!kept.contains("bulky body") && !kept.contains("big output"));
        // Only kept fields asked for: read as before, not expired.
        let facts = json!({"where": {"role": "toolResult"}, "fields": ["toolCallId"], "details": ["executionId", "checkpoint.id"]});
        let got = session.projected_entries(1, u64::MAX, &facts).unwrap();
        assert_eq!(
            got[0]["message"]["details"],
            json!({"executionId": "e1", "checkpoint": {"id": 7}})
        );
        assert!(got[0].get("expired").is_none());
        // A result whose details had none of the kept fields reads as complete and empty.
        assert_eq!(got[1]["message"]["details"], json!({}));
        assert!(got[1].get("expired").is_none());
        // The tool call's input is kept whole and reads back as before; the output is gone.
        let history = session.history(1, u64::MAX, true).unwrap();
        let call = history.iter().find(|e| e["n"] == 2).unwrap();
        assert_eq!(
            call["m"]["content"][0]["arguments"],
            json!({"code": "print('the code that ran')"})
        );
        assert!(call.get("expired").is_none());
        assert_eq!(
            history.iter().find(|e| e["n"] == 3).unwrap()["expired"],
            true
        );
        // A field that was not kept: expired, with what was kept still shown.
        let more = json!({"where": {"role": "toolResult"}, "fields": ["toolCallId"], "details": ["executionId", "big"]});
        let got = session.projected_entries(1, u64::MAX, &more).unwrap();
        assert_eq!(got[0]["expired"], true);
        assert_eq!(got[0]["message"]["details"], json!({"executionId": "e1"}));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_checkpoint_may_not_refer_to_stored_data_a_cleanup_deleted() {
        let old = "2026-01-01T00:00:00.000Z";
        let dir = session(&[
            (
                "log/000001.jsonl",
                vec![
                    json!({"n": 1, "t": old, "k": "chunk"}),
                    json!({"n": 2, "t": old, "k": "tool", "m": {"role": "toolResult", "toolCallId": "a", "content": {"$ref": {"f": "payload", "k": "2.0"}}}}),
                    json!({"n": 3, "t": old, "k": "checkpoint", "id": "c1"}),
                ],
            ),
            (
                "log/000001.payload.jsonl",
                vec![json!({"key": "2.0", "v": [{"type": "text", "text": "old"}]})],
            ),
            (
                "log/000002.jsonl",
                vec![json!({"n": 4, "t": old, "k": "chunk"})],
            ),
            (
                "checkpoints/c1.json",
                vec![
                    json!({"id": "c1", "through": 3, "messages": [{"role": "user", "content": "summary"}]}),
                ],
            ),
        ]);
        let session = Session::open(&dir).unwrap();
        let now = ms_from_iso("2026-03-01T00:00:00.000Z").unwrap();
        assert_eq!(
            session.reap(now - 14 * 86_400_000, &[]).unwrap().0,
            ["log/000001.payload.jsonl"]
        );
        // A runner holding an older copy of the context tries to checkpoint it.
        let stale = json!({"role": "toolResult", "toolCallId": "a", "content": {"$ref": {"file": "log/000001.payload.jsonl", "key": "2.0"}}});
        let error = session.checkpoint(&[stale]).unwrap_err().to_string();
        assert!(error.contains("stored data that is gone"), "{error}");
        assert!(session.context().is_ok(), "the context stays readable");
        // Without the stale reference it is accepted.
        assert!(
            session
                .checkpoint(&[json!({"role": "user", "content": "fresh"})])
                .is_ok()
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_cleanup_deletes_only_old_unreferenced_tool_output_and_reasoning() {
        let old = "2026-01-01T00:00:00.000Z";
        let tool = |n: u64, k: &str| {
            json!({"n": n, "t": old, "k": "tool", "m": {"role": "toolResult", "toolCallId": format!("c{n}"),
                "content": {"$ref": {"f": "payload", "k": k}}}})
        };
        let dir = session(&[
            // Old and before the checkpoint: its tool output and reasoning go.
            (
                "log/000001.jsonl",
                vec![json!({"n": 1, "t": old, "k": "chunk"}), tool(2, "2.0")],
            ),
            (
                "log/000001.payload.jsonl",
                vec![json!({"key": "2.0", "v": [{"type": "text", "text": "old output"}]})],
            ),
            (
                "log/000001.reasoning.jsonl",
                vec![json!({"key": "2.r", "v": "old thinking"})],
            ),
            // Old, but the context after the checkpoint still points into it: kept.
            (
                "log/000002.jsonl",
                vec![
                    json!({"n": 3, "t": old, "k": "chunk"}),
                    json!({"n": 4, "t": old, "k": "checkpoint", "id": "c1"}),
                    tool(5, "5.0"),
                ],
            ),
            (
                "log/000002.payload.jsonl",
                vec![json!({"key": "5.0", "v": [{"type": "text", "text": "still sent"}]})],
            ),
            (
                "log/000002.reasoning.jsonl",
                vec![json!({"key": "5.r", "v": "unreferenced thinking"})],
            ),
            // An old chunk without readable times is kept.
            ("log/000003.jsonl", vec![json!({"n": 6, "k": "chunk"})]),
            (
                "log/000003.payload.jsonl",
                vec![json!({"key": "6.0", "v": []})],
            ),
            // The current chunk is never touched, however old.
            (
                "log/000004.jsonl",
                vec![json!({"n": 7, "t": old, "k": "chunk"}), tool(8, "8.0")],
            ),
            (
                "log/000004.payload.jsonl",
                vec![json!({"key": "8.0", "v": [{"type": "text", "text": "current"}]})],
            ),
            ("log/000001.images/x", vec![]),
            (
                "checkpoints/c1.json",
                vec![json!({"id": "c1", "through": 4, "messages": [
                {"role": "user", "content": [{"type": "text", "text": "summary"}]}]})],
            ),
        ]);
        let session = Session::open(&dir).unwrap();
        let before = session.context().unwrap();
        let now = ms_from_iso("2026-03-01T00:00:00.000Z").unwrap();
        let (deleted, bytes) = session.reap(now - 14 * 86_400_000, &[]).unwrap();
        assert_eq!(
            deleted,
            [
                "log/000001.payload.jsonl",
                "log/000001.reasoning.jsonl",
                "log/000002.reasoning.jsonl"
            ]
        );
        assert!(bytes > 0);
        for kept in [
            "log/000002.payload.jsonl",
            "log/000003.payload.jsonl",
            "log/000004.payload.jsonl",
            "log/000001.images/x",
            "log/000001.jsonl",
        ] {
            assert!(dir.join(kept).exists(), "{kept}");
        }
        // The model sees exactly what it saw before; history shows the old output expired.
        assert_eq!(session.context().unwrap(), before);
        let history = session.history(1, u64::MAX, true).unwrap();
        let old_result = history.iter().find(|e| e["n"] == 2).unwrap();
        assert_eq!(old_result["expired"], true);
        // Nothing is newer than a cutoff a year back: a second run deletes nothing.
        assert_eq!(
            session.reap(now - 365 * 86_400_000, &[]).unwrap().0,
            Vec::<String>::new()
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn without_checkpoint_every_model_record_is_sent_in_order() {
        let dir = session(&[(
            "log/000001.jsonl",
            vec![
                json!({"n": 1, "k": "chunk"}),
                user(2, "a"),
                json!({"n": 3, "k": "app", "ns": "harness.usage", "data": {}}),
                user(4, "b"),
            ],
        )]);
        let texts: Vec<_> = context(&dir)
            .iter()
            .map(|m| m["content"][0]["text"].clone())
            .collect();
        assert_eq!(texts, vec![json!("a"), json!("b")]);
    }

    #[test]
    fn latest_checkpoint_replaces_everything_before_it_across_chunks() {
        let dir = session(&[
            (
                "log/000001.jsonl",
                vec![
                    user(1, "old"),
                    json!({"n": 2, "k": "checkpoint", "id": "c1", "through": 1}),
                ],
            ),
            (
                "log/000002.jsonl",
                vec![
                    user(3, "mid"),
                    json!({"n": 4, "k": "checkpoint", "id": "c2", "through": 3}),
                ],
            ),
            (
                "log/000003.jsonl",
                vec![json!({"n": 5, "k": "chunk"}), user(6, "new")],
            ),
            (
                "checkpoints/c2.json",
                vec![json!({"id": "c2", "messages": [{"role": "user", "content": "summary"}]})],
            ),
        ]);
        let got = context(&dir);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["content"], json!("summary"));
        assert_eq!(got[1]["content"][0]["text"], json!("new"));
    }

    #[test]
    fn side_file_references_and_images_are_restored() {
        let dir = session(&[
            (
                "log/000001.jsonl",
                vec![
                    json!({"n": 1, "k": "assistant", "m": {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "s", "thinkingSignature": {"$ref": {"f": "reasoning", "k": "1.0"}}},
                        {"type": "toolCall", "id": "t", "arguments": {"$ref": {"f": "payload", "k": "1.1"}}}]}}),
                    json!({"n": 2, "k": "user", "m": {"role": "user", "content": [
                        {"type": "image", "mimeType": "image/png", "$img": SHA}]}}),
                ],
            ),
            (
                "log/000001.payload.jsonl",
                vec![json!({"key": "1.1", "v": {"code": "1+1"}})],
            ),
            (
                "log/000001.reasoning.jsonl",
                vec![json!({"key": "1.0", "v": "enc"})],
            ),
        ]);
        fs::create_dir_all(dir.join("log/000001.images")).unwrap();
        fs::write(dir.join("log/000001.images").join(SHA), b"hi").unwrap();
        let got = context(&dir);
        assert_eq!(got[0]["content"][0]["thinkingSignature"], json!("enc"));
        assert_eq!(got[0]["content"][1]["arguments"], json!({"code": "1+1"}));
        assert_eq!(got[1]["content"][0]["data"], json!("aGk="));
    }

    #[test]
    fn unadmitted_calls_stay_in_history_but_are_never_sent() {
        let dir = session(&[(
            "log/000001.jsonl",
            vec![
                json!({"n": 1, "k": "assistant", "m": {"role": "assistant", "content": [
                {"type": "text", "text": "x"},
                {"type": "toolCall", "id": "t", "unadmitted": true}]}}),
            ],
        )]);
        assert_eq!(context(&dir)[0]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_missing_side_file_value_is_an_error_not_a_silent_gap() {
        let dir = session(&[(
            "log/000001.jsonl",
            vec![
                json!({"n": 1, "k": "tool", "m": {"role": "toolResult", "content": {"$ref": {"f": "payload", "k": "1.0"}}}}),
            ],
        )]);
        assert!(Session::open(&dir).unwrap().context().is_err());
    }

    #[test]
    fn appended_messages_come_back_unchanged_with_bulk_in_side_files() {
        let dir = std::env::temp_dir().join(format!(
            "core-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let session = Session::create(&dir, "t").unwrap();
        let messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "image", "mimeType": "image/png", "data": "aGk="}]}),
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "sum", "thinkingSignature": "{\"type\":\"reasoning\"}"},
                {"type": "toolCall", "id": "c|fc", "name": "ipython", "arguments": {"code": "1"}}]}),
            json!({"role": "toolResult", "toolCallId": "c|fc", "content": [{"type": "text", "text": "1"}]}),
        ];
        for (kind, m) in ["user", "assistant", "tool"].iter().zip(&messages) {
            session.append(kind, m).unwrap();
        }
        assert_eq!(session.context().unwrap(), messages);
        let log = fs::read_to_string(dir.join("log/000001.jsonl")).unwrap();
        assert!(
            !log.contains("aGk=")
                && !log.contains("\\\"type\\\":\\\"reasoning")
                && !log.contains("\"code\"")
        );
        assert!(
            log.contains("\"thinking\":\"sum\""),
            "readable summary stays in the main log"
        );
    }

    #[test]
    fn images_meant_for_the_user_outlive_the_payload() {
        let (dir, session) = fresh();
        let shown = json!({"role": "toolResult", "toolCallId": "s", "toolName": "ipython",
            "content": [{"type": "text", "text": "[Image shown to the user: a.png]"}],
            "details": {"ok": true, "shownImages": [{"type": "image", "data": "U0hPV04=", "mimeType": "image/png", "name": "a.png"}]}});
        let attached = json!({"role": "toolResult", "toolCallId": "f", "toolName": "ipython",
            "content": [{"type": "text", "text": "chart"}, {"type": "image", "data": "Rk9S", "mimeType": "image/png"}],
            "details": {"ok": true, "imagesForUser": true}});
        let working = json!({"role": "toolResult", "toolCallId": "w", "toolName": "ipython",
            "content": [{"type": "image", "data": "V09SSw==", "mimeType": "image/png"}], "details": {"ok": true}});
        for m in [&shown, &attached, &working] {
            session.append("tool", m).unwrap();
        }
        // Nothing changes while the payload is there: the model and the history see the same.
        assert_eq!(
            session.context().unwrap(),
            vec![shown.clone(), attached.clone(), working.clone()]
        );
        let full = session.history(1, u64::MAX, true).unwrap();
        let message =
            |n: usize| full.iter().filter(|r| r.get("m").is_some()).nth(n).unwrap()["m"].clone();
        assert_eq!((message(0), message(1)), (shown.clone(), attached.clone()));
        let images: Vec<_> = fs::read_dir(dir.join("log/000001.images"))
            .unwrap()
            .collect();
        assert_eq!(
            images.len(),
            2,
            "only the images meant for the user are kept as images"
        );
        // A cleanup deletes the payload: those images stay, the rest is gone and marked expired.
        fs::remove_file(dir.join("log/000001.payload.jsonl")).unwrap();
        let left = session.history(1, u64::MAX, true).unwrap();
        let left: Vec<_> = left.iter().filter(|r| r.get("m").is_some()).collect();
        assert!(left.iter().all(|r| r["expired"] == true));
        assert_eq!(
            left[0]["m"]["details"],
            json!({"shownImages": shown["details"]["shownImages"]})
        );
        assert_eq!(
            left[1]["m"]["content"],
            json!([{"type": "image", "data": "Rk9S", "mimeType": "image/png"}])
        );
        assert_eq!(left[1]["m"]["details"], json!({"imagesForUser": true}));
        assert!(
            left[2]["m"]["content"].get("$ref").is_some(),
            "working images are not kept"
        );
        let select = json!({"where": {"role": "toolResult"}, "fields": ["toolCallId"], "details": ["imagesForUser", "ok"]});
        let picked = session.projected_entries(1, u64::MAX, &select).unwrap();
        assert_eq!(
            picked[1]["message"]["details"],
            json!({"imagesForUser": true})
        );
        assert_eq!(picked[1]["expired"], true);
        let _ = fs::remove_dir_all(dir);
    }

    fn fresh() -> (PathBuf, Session) {
        let dir = std::env::temp_dir().join(format!(
            "core-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let session = Session::create(&dir, "t").unwrap();
        (dir, session)
    }

    fn text(t: &str) -> Value {
        json!({"role": "user", "content": [{"type": "text", "text": t}]})
    }

    fn checkpoint_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir.join("checkpoints"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn checkpoint_replaces_context_and_seals_the_chunk() {
        let (_dir, s) = fresh();
        s.append("user", &text("a")).unwrap();
        s.checkpoint(&[text("summary")]).unwrap();
        s.append("user", &text("b")).unwrap();
        assert_eq!(s.context().unwrap(), vec![text("summary"), text("b")]);
        assert_eq!(s.chunk_numbers().unwrap(), vec![1, 2]);
    }

    #[test]
    fn only_the_newest_two_checkpoints_are_kept() {
        let (dir, s) = fresh();
        for i in 0..3 {
            s.append("user", &text(&i.to_string())).unwrap();
            s.checkpoint(&[text(&format!("cp{i}"))]).unwrap();
        }
        assert_eq!(checkpoint_files(&dir).len(), 2);
        assert_eq!(s.context().unwrap(), vec![text("cp2")]);
    }

    #[test]
    fn crash_after_content_before_marker_leaves_context_unchanged() {
        let (dir, s) = fresh();
        s.append("user", &text("a")).unwrap();
        // Crash after step 1: content written, no marker.
        s.write_durably(&s.checkpoint_path("000001-3"), r#"{"messages":[]}"#)
            .unwrap();
        assert_eq!(s.context().unwrap(), vec![text("a")]);
        s.recover().unwrap();
        assert!(
            checkpoint_files(&dir).is_empty(),
            "orphan content is dropped"
        );
        assert_eq!(s.context().unwrap(), vec![text("a")]);
    }

    #[test]
    fn crash_after_marker_with_a_late_result_is_finished_on_open() {
        let (dir, s) = fresh();
        s.append("user", &text("a")).unwrap();
        s.checkpoint(&[text("cp1")]).unwrap();
        s.append("user", &text("b")).unwrap();
        // Second checkpoint: content and marker, then a late message, then a crash
        // before pruning and sealing.
        let (chunk, through) = s.tail().unwrap();
        let id = format!("{chunk:06}-{}", through + 1);
        s.write_durably(
            &s.checkpoint_path(&id),
            &json!({"messages": [text("cp2")]}).to_string(),
        )
        .unwrap();
        s.append_line(
            &s.log_path(chunk, None),
            &json!({"n": through + 1, "k": "checkpoint", "id": id, "through": through}),
        )
        .unwrap();
        s.append("tool", &json!({"role": "toolResult", "toolCallId": "x", "content": [{"type": "text", "text": "late"}]})).unwrap();
        // A stray third file stands in for a prune that never ran.
        s.write_durably(&s.checkpoint_path("000000-0"), "{}")
            .unwrap();
        s.recover().unwrap();
        assert_eq!(checkpoint_files(&dir).len(), 2);
        assert_eq!(s.chunk_numbers().unwrap(), vec![1, 2, 3]);
        let ctx = s.context().unwrap();
        assert_eq!(ctx[0], text("cp2"));
        assert_eq!(ctx[1]["content"][0]["text"], json!("late"));
        s.recover().unwrap();
        assert_eq!(
            s.chunk_numbers().unwrap(),
            vec![1, 2, 3],
            "recovery is idempotent"
        );
    }

    #[test]
    fn unknown_format_is_rejected() {
        let dir = session(&[]);
        fs::write(dir.join("meta.json"), r#"{"format":2}"#).unwrap();
        assert!(Session::open(&dir).is_err());
    }
}
