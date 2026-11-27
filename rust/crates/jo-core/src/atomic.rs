//! Crash-safe file writes and path confinement.
//!
//! Temporary files are created with `CREATE_NEW` (tempfile's O_EXCL), so a
//! file or link pre-planted at a predictable name is never written through.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("Refusing to use a linked or non-file application path")]
    Unsafe,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// `root/name`, refusing links, non-files and anything that resolves outside
/// `root` (the Python `_inside_file`).
pub fn inside_file(root: &Path, name: &str) -> Result<PathBuf, PathError> {
    let path = root.join(name);
    if let Ok(meta) = fs::symlink_metadata(&path)
        && (meta.file_type().is_symlink() || !meta.is_file())
    {
        return Err(PathError::Unsafe);
    }
    let parent = path.parent().ok_or(PathError::Unsafe)?;
    if canonical(parent)? != canonical(root)? {
        return Err(PathError::Unsafe);
    }
    Ok(path)
}

/// Canonical path without the `\\?\` verbatim prefix.
pub fn canonical(path: &Path) -> io::Result<PathBuf> {
    let full = fs::canonicalize(path)?;
    let text = full.to_string_lossy();
    Ok(match text.strip_prefix(r"\\?\UNC\") {
        Some(rest) => PathBuf::from(format!(r"\\{rest}")),
        None => match text.strip_prefix(r"\\?\") {
            Some(rest) => PathBuf::from(rest),
            None => full,
        },
    })
}

/// Options for [`write_atomic`].
#[derive(Clone, Copy, Debug, Default)]
pub struct WriteOptions<'a> {
    /// Temporary file prefix; defaults to `"<basename>."`.
    pub prefix: Option<&'a str>,
    /// fsync before the replace (lifecycle and update records do this).
    pub sync: bool,
    /// Retry the replace on sharing violations (another process reading the
    /// target, e.g. the overlay polling the learner's status file).
    pub retry_replace: bool,
    /// After retries are exhausted, overwrite the target in place.
    pub fallback_in_place: bool,
}

/// Write `data` to `path` via a fresh temporary file in the same folder and
/// an atomic replace.
pub fn write_atomic(path: &Path, data: &[u8], options: WriteOptions<'_>) -> io::Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let default_prefix = format!("{}.", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    let prefix = options.prefix.unwrap_or(&default_prefix);
    let mut temp = tempfile::Builder::new().prefix(prefix).suffix(".tmp").tempfile_in(dir)?;
    temp.write_all(data)?;
    temp.flush()?;
    if options.sync {
        temp.as_file().sync_all()?;
    }
    let attempts = if options.retry_replace { 6 } else { 1 };
    let mut delay = Duration::from_millis(50);
    let mut temp = Some(temp);
    for attempt in 0..attempts {
        let current = temp.take().expect("temporary file present");
        match current.persist(path) {
            Ok(_) => return Ok(()),
            Err(err) if err.error.kind() == io::ErrorKind::PermissionDenied && attempt + 1 < attempts => {
                temp = Some(err.file);
                thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_millis(800));
            }
            Err(err) => {
                if options.fallback_in_place && err.error.kind() == io::ErrorKind::PermissionDenied {
                    // The temp file is removed when `err.file` drops.
                    return fs::write(path, data);
                }
                return Err(err.error);
            }
        }
    }
    unreachable!("loop returns on the last attempt")
}

/// JSON in the layout Python's `json.dumps(indent=n, ensure_ascii=False)`
/// produces (key order preserved; floats in shortest round-trip form).
pub fn to_json_pretty<T: Serialize + ?Sized>(value: &T, indent: usize) -> serde_json::Result<Vec<u8>> {
    let spaces = vec![b' '; indent];
    let formatter = serde_json::ser::PrettyFormatter::with_indent(&spaces);
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, formatter);
    value.serialize(&mut ser)?;
    Ok(out)
}

/// One JSONL line in Python's default `json.dumps` layout: `", "` between
/// items and `": "` after keys, non-ASCII kept verbatim.
pub fn to_json_line<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<String> {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, PythonCompact);
    value.serialize(&mut ser)?;
    Ok(String::from_utf8(out).expect("serde_json emits UTF-8"))
}

/// serde_json formatter matching Python's default separators.
#[derive(Clone, Copy, Debug, Default)]
pub struct PythonCompact;

impl serde_json::ser::Formatter for PythonCompact {
    fn begin_array_value<W: ?Sized + Write>(&mut self, writer: &mut W, first: bool) -> io::Result<()> {
        if first { Ok(()) } else { writer.write_all(b", ") }
    }
    fn begin_object_key<W: ?Sized + Write>(&mut self, writer: &mut W, first: bool) -> io::Result<()> {
        if first { Ok(()) } else { writer.write_all(b", ") }
    }
    fn begin_object_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        writer.write_all(b": ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn python_layouts() {
        let v = json!({"a": [1, 2.5], "é": {"b": null}, "e": {}, "l": []});
        assert_eq!(to_json_line(&v).unwrap(), r#"{"a": [1, 2.5], "é": {"b": null}, "e": {}, "l": []}"#);
        let pretty = String::from_utf8(to_json_pretty(&v, 1).unwrap()).unwrap();
        assert_eq!(pretty, "{\n \"a\": [\n  1,\n  2.5\n ],\n \"é\": {\n  \"b\": null\n },\n \"e\": {},\n \"l\": []\n}");
    }

    #[test]
    fn atomic_write_never_follows_a_planted_temp_name() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("state.json");
        let planted = dir.path().join("state.json.tmp");
        fs::write(&planted, b"planted").unwrap();
        write_atomic(&target, b"{}", WriteOptions::default()).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"{}");
        assert_eq!(fs::read(&planted).unwrap(), b"planted");
    }

    #[test]
    fn inside_file_confines_to_root() {
        let dir = tempfile::tempdir().unwrap();
        assert!(inside_file(dir.path(), "x.json").is_ok());
        assert!(inside_file(dir.path(), r"..\x.json").is_err());
        fs::create_dir(dir.path().join("sub")).unwrap();
        assert!(inside_file(dir.path(), "sub").is_err());
    }
}
