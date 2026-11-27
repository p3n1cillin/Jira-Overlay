//! The learner's files and how each one is written.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use jo_core::atomic::{WriteOptions, to_json_line, to_json_pretty, write_atomic};
use serde_json::Value;

/// Every file the learner reads or writes, next to the app.
#[derive(Clone, Debug)]
pub struct Paths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub log: PathBuf,
    pub report: PathBuf,
    pub status: PathBuf,
    pub error_log: PathBuf,
    pub suggest: PathBuf,
    pub history: PathBuf,
    pub dataset: PathBuf,
    pub reviews: PathBuf,
}

impl Paths {
    pub fn new(root: &Path) -> Self {
        let at = |name: &str| root.join(name);
        Self {
            root: root.to_path_buf(),
            config: at(jo_core::config::FILE_NAME),
            state: at("triage_state.json"),
            log: at("triage_log.jsonl"),
            report: at("TRIAGE_REPORT.md"),
            status: at("triage_status.json"),
            error_log: at("triage_learner.log"),
            suggest: at("triage_suggestions.json"),
            history: at("triage_history.jsonl"),
            dataset: at("triage_dataset.jsonl"),
            reviews: at("triage_reviews.json"),
        }
    }
}

/// `_log_error`: one timestamped line; never fails.
pub fn log_error(path: &Path, message: &str) {
    jo_core::errlog::append_line(path, message);
}

/// `_atomic_json_write`: `indent=1`, non-ASCII verbatim, replace retried
/// while a reader holds the file; optionally overwrite in place at the end.
pub fn write_json(path: &Path, value: &Value, fallback_in_place: bool) -> io::Result<()> {
    let data = to_json_pretty(value, 1).map_err(io::Error::other)?;
    write_atomic(path, &data, WriteOptions { retry_replace: true, fallback_in_place, ..WriteOptions::default() })
}

/// Append one JSONL row (`json.dumps(row, ensure_ascii=False) + "\n"`).
pub fn append_jsonl(path: &Path, row: &Value) -> io::Result<()> {
    let mut line = to_json_line(row).map_err(io::Error::other)?;
    line.push('\n');
    OpenOptions::new().create(true).append(true).open(path)?.write_all(line.as_bytes())
}

/// Labelled decisions sorted by `when`, or `None` without a history file.
/// Unparseable lines (a truncated final append) are counted, not fatal.
pub fn load_history(path: &Path, error_log: &Path) -> Option<Vec<Value>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            log_error(error_log, &format!("history: could not be read: {e}"));
            return Some(Vec::new());
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut recs = Vec::new();
    let mut bad = 0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(row @ Value::Object(_)) => recs.push(row),
            _ => bad += 1,
        }
    }
    if bad > 0 {
        log_error(error_log, &format!("history: skipped {bad} unparseable line(s)"));
    }
    recs.sort_by(|a, b| when_of(a).cmp(when_of(b)));
    Some(recs)
}

pub fn when_of(v: &Value) -> &str {
    v.get("when").and_then(Value::as_str).unwrap_or("")
}

/// A JSON object file, `{}` when missing or unreadable.
pub fn read_object(path: &Path) -> serde_json::Map<String, Value> {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}
