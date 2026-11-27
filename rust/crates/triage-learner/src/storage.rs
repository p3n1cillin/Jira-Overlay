//! Local recovery archives (Python `app/triage_storage.py`). Call while the
//! learner's writer is idle.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// One verified zip per local day in `<state dir>/triage_backups`, holding
/// the state, history, log and its metrics checkpoint plus `manifest.json`.
pub fn daily_backup(state_path: &Path, history_path: &Path, log_path: &Path) -> Result<PathBuf, String> {
    let root = state_path.parent().unwrap_or(Path::new(".")).join("triage_backups");
    match fs::create_dir(&root) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && root.is_dir() => {}
        Err(e) => return Err(e.to_string()),
    }
    let target = root.join(format!("{}.zip", chrono::Local::now().format("%Y-%m-%d")));
    if target.exists() {
        return Ok(target);
    }
    let mut metrics = log_path.as_os_str().to_owned();
    metrics.push(".metrics.json");
    let paths = [state_path.to_path_buf(), history_path.to_path_buf(), log_path.to_path_buf(), PathBuf::from(metrics)];
    let temp = tempfile::Builder::new().prefix("tmp").suffix(".tmp").tempfile_in(&root).map_err(|e| e.to_string())?;
    {
        let mut archive = zip::ZipWriter::new(temp.as_file());
        let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let mut manifest = Map::new();
        for path in &paths {
            if !path.exists() {
                continue;
            }
            let data = fs::read(path).map_err(|e| e.to_string())?;
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            archive.start_file(name.as_str(), options).map_err(|e| e.to_string())?;
            archive.write_all(&data).map_err(|e| e.to_string())?;
            manifest.insert(name, Value::String(sha256_hex(&data)));
        }
        let text = jo_core::atomic::to_json_pretty(&manifest, 2).map_err(|e| e.to_string())?;
        archive.start_file("manifest.json", options).map_err(|e| e.to_string())?;
        archive.write_all(&text).map_err(|e| e.to_string())?;
        archive.finish().map_err(|e| e.to_string())?;
    }
    verify_backup(temp.path())?;
    temp.persist(&target).map_err(|e| e.error.to_string())?;
    Ok(target)
}

/// Check every manifest entry's checksum; returns the manifest.
pub fn verify_backup(path: &Path) -> Result<Map<String, Value>, String> {
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let read = |archive: &mut zip::ZipArchive<fs::File>, name: &str| -> Result<Vec<u8>, String> {
        let mut entry = archive.by_name(name).map_err(|e| format!("{e}: {name}"))?;
        let mut data = Vec::new();
        entry.read_to_end(&mut data).map_err(|e| e.to_string())?;
        Ok(data)
    };
    let manifest: Map<String, Value> =
        serde_json::from_slice(&read(&mut archive, "manifest.json")?).map_err(|e| e.to_string())?;
    for (name, digest) in &manifest {
        let base = Path::new(name).file_name().map(|n| n.to_string_lossy().into_owned());
        if base.as_deref() != Some(name.as_str()) || name.contains('\\') {
            return Err("Unsafe archive member".into());
        }
        if Some(sha256_hex(&read(&mut archive, name)?).as_str()) != digest.as_str() {
            return Err(format!("Backup checksum mismatch: {name}"));
        }
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_verifies_and_excludes_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("triage_state.json");
        fs::write(&state, b"{\"model\": {}}").unwrap();
        fs::write(dir.path().join("jira_config.json"), b"{\"token\": \"secret\"}").unwrap();
        let backup =
            daily_backup(&state, &dir.path().join("triage_history.jsonl"), &dir.path().join("triage_log.jsonl"))
                .unwrap();
        let manifest = verify_backup(&backup).unwrap();
        assert!(manifest.contains_key("triage_state.json") && !manifest.contains_key("jira_config.json"));
        // Once a day: a second call keeps the first archive.
        fs::write(&state, b"{}").unwrap();
        let again =
            daily_backup(&state, &dir.path().join("triage_history.jsonl"), &dir.path().join("triage_log.jsonl"))
                .unwrap();
        assert_eq!(again, backup);
        assert!(verify_backup(&again).is_ok());
        let leftovers: Vec<_> = fs::read_dir(backup.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
