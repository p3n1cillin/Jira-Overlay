//! Append-only diagnostic logs with one rotated generation.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use chrono::Local;

/// Append `"%Y-%m-%d %H:%M:%S\n{detail}\n\n"` to `path`, rotating to
/// `<path>.old` once it exceeds `max_bytes` (overlay error log: 5 MiB).
/// Failures are swallowed: logging must never take the app down.
pub fn append_block(path: &Path, detail: &str, max_bytes: u64) {
    rotate(path, max_bytes, "old");
    let stamp = Local::now().format("%Y-%m-%d %H:%M:%S");
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = write!(file, "{stamp}\n{detail}\n\n");
    }
}

/// Append `"%Y-%m-%d %H:%M:%S  {line}\n"` (two spaces), the learner log format.
pub fn append_line(path: &Path, line: &str) {
    let stamp = Local::now().format("%Y-%m-%d %H:%M:%S");
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{stamp}  {line}");
    }
}

/// Rename `path` to `<path>.<suffix>` when it is larger than `max_bytes`.
pub fn rotate(path: &Path, max_bytes: u64, suffix: &str) {
    if fs::metadata(path).is_ok_and(|m| m.len() > max_bytes) {
        let mut backup = path.as_os_str().to_owned();
        backup.push(".");
        backup.push(suffix);
        let _ = fs::rename(path, backup);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_one_generation() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("e.log");
        append_block(&log, "first", 10);
        append_block(&log, "second", 10);
        let old = fs::read_to_string(dir.path().join("e.log.old")).unwrap();
        let new = fs::read_to_string(&log).unwrap();
        assert!(old.contains("first") && new.contains("second") && !new.contains("first"));
    }
}
