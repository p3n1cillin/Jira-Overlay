//! `jira_config.json`: load, normalise and save, compatible with the Python
//! build. The token is only ever stored DPAPI-protected as `tokenProtected`.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;

use serde_json::{Map, Value};

use crate::atomic::{WriteOptions, to_json_pretty, write_atomic};
use crate::{dpapi, jql};

pub type Config = Map<String, Value>;

pub const FILE_NAME: &str = "jira_config.json";

/// Settings this app owns. Anything else belongs to another tool (the
/// learner) and is preserved on save.
pub const OVERLAY_KEYS: &[&str] = &[
    "domain",
    "email",
    "token",
    "serviceDeskId",
    "serviceDeskName",
    "projectKey",
    "alertQueue",
    "alertQueues",
    "trayBadgeQueue",
    "hiddenQueues",
    "skipKeywords",
    "maxQueueSize",
    "refreshSeconds",
    "alertQueuesAsked",
    "notificationsEnabled",
    "soundEnabled",
    "checkForUpdates",
    "autoInstallUpdates",
    "alpha",
    "overlayWidth",
    "alwaysVisible",
    "slaField",
    "slaEnabled",
    "weekendsOff",
    "reportUsers",
    "reportRange",
    "reportBasis",
    "completedTodayFilter",
    "suggestionsFile",
    "lastX",
    "lastY",
    "snapCorner",
];

/// Settings that identify the install: a save may replace them, never drop them.
pub const IDENTITY_KEYS: &[&str] = &["domain", "email", "token", "serviceDeskId", "serviceDeskName", "projectKey"];

enum Cast {
    Int,
    Float,
}

/// key, caster, default, low, high — physical limits, not the Settings choices.
const NUMERIC: &[(&str, Cast, f64, f64, f64)] = &[
    ("overlayWidth", Cast::Int, 260.0, 120.0, 4000.0),
    ("alpha", Cast::Float, 0.93, 0.1, 1.0),
    ("refreshSeconds", Cast::Int, 60.0, 5.0, 86400.0),
    ("maxQueueSize", Cast::Int, 0.0, 0.0, 10_000_000.0),
];
const LISTS: &[&str] = &["alertQueues", "hiddenQueues", "skipKeywords", "reportUsers"];
const STRINGS: &[&str] = &[
    "slaField",
    "completedTodayFilter",
    "trayBadgeQueue",
    "alertQueue",
    "serviceDeskName",
    "projectKey",
    "snapCorner",
    "reportRange",
    "reportBasis",
];

static SAVE_LOCK: Mutex<()> = Mutex::new(());

/// Python `int(x)` for the shapes JSON can hold; `None` where Python raises.
fn py_int(value: &Value) -> Option<i64> {
    match value {
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::String(s) => {
            let t = s.trim();
            let digits = t.trim_start_matches(['+', '-']);
            let well_formed = !digits.is_empty()
                && !digits.starts_with('_')
                && !digits.ends_with('_')
                && !digits.contains("__")
                && digits.chars().all(|c| c.is_ascii_digit() || c == '_');
            if well_formed { t.replace('_', "").parse().ok() } else { None }
        }
        _ => None,
    }
}

/// Python `float(x)`; `None` where Python raises.
fn py_float(value: &Value) -> Option<f64> {
    match value {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Python `str(x)` for non-string JSON values.
fn py_str(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Make a config safe to use, whatever is in the file. A valid config comes
/// back unchanged.
pub fn normalise(cfg: &mut Config) {
    for key in ["checkForUpdates", "autoInstallUpdates"] {
        if cfg.get(key).is_some_and(|v| !v.is_boolean()) {
            cfg.insert(key.into(), Value::Bool(false)); // malformed opt-outs never enable installation
        }
    }
    for (key, cast, default, low, high) in NUMERIC {
        let Some(value) = cfg.get(*key) else { continue };
        let fixed = match cast {
            Cast::Int => {
                let n = py_int(value).unwrap_or(*default as i64);
                Value::from(n.clamp(*low as i64, *high as i64))
            }
            Cast::Float => {
                let f = py_float(value).unwrap_or(*default);
                let f = if f.is_nan() { *low } else { f.clamp(*low, *high) };
                Value::from(f)
            }
        };
        if &fixed != value {
            cfg.insert((*key).into(), fixed);
        }
    }
    for key in LISTS {
        match cfg.get(*key) {
            Some(Value::Array(_)) | None => {}
            Some(Value::String(s)) if !s.is_empty() => {
                let item = Value::String(s.clone());
                cfg.insert((*key).into(), Value::Array(vec![item]));
            }
            Some(_) => {
                cfg.insert((*key).into(), Value::Array(vec![]));
            }
        }
    }
    for key in STRINGS {
        if let Some(value) = cfg.get(*key).filter(|v| !v.is_string()) {
            let text = py_str(value);
            cfg.insert((*key).into(), Value::String(text));
        }
    }
    if cfg.get("projectKey").and_then(Value::as_str).is_some_and(|k| !jql::safe_project_key(k)) {
        cfg.insert("projectKey".into(), Value::String(String::new()));
    }
}

fn move_aside(path: &Path) {
    let mut bad = path.as_os_str().to_owned();
    bad.push(".bad");
    let _ = fs::rename(path, bad);
}

/// Load and normalise the config. `None` means "run setup": the file is
/// missing, or it was unreadable/corrupt/not an object and has been moved
/// aside to `.bad`. The token is decrypted into `token` in memory only; a
/// plain-text token found on disk is encrypted in place immediately.
pub fn load(path: &Path) -> Option<Config> {
    let text = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(_) => {
            move_aside(path);
            return None;
        }
    };
    let mut cfg = match serde_json::from_slice::<Value>(&text) {
        Ok(Value::Object(map)) => map,
        _ => {
            move_aside(path);
            return None;
        }
    };
    let needs_migration = cfg.contains_key("token") && !cfg.contains_key("tokenProtected");
    if let Some(blob) = cfg.shift_remove("tokenProtected") {
        let token = blob.as_str().and_then(dpapi::unprotect).unwrap_or_default();
        cfg.insert("token".into(), Value::String(token));
    }
    normalise(&mut cfg);
    if needs_migration && cfg.get("token").and_then(Value::as_str).is_some_and(|t| !t.is_empty()) {
        let _ = save(path, &cfg);
    }
    Some(cfg)
}

fn read_object(path: &Path) -> Config {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

/// The overlay's save: a read-merge-write of the file on disk.
///
/// * Overlay-owned keys come from `cfg`; ones absent from `cfg` are removed
///   (that is how `lastX`/`lastY` disappear when snapping to a corner),
///   except identity keys, which a partial `cfg` can never drop.
/// * Keys owned by other tools keep the value on disk. (The Python build
///   wrote its stale start-up copy over the learner's newer values.) A
///   foreign key only in `cfg` is added.
/// * `token` is never written in plain text.
pub fn save(path: &Path, cfg: &Config) -> io::Result<()> {
    let _guard = SAVE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut merged = read_object(path);
    for key in OVERLAY_KEYS.iter().filter(|k| !IDENTITY_KEYS.contains(k)) {
        merged.shift_remove(*key);
    }
    for (key, value) in cfg {
        if OVERLAY_KEYS.contains(&key.as_str()) || !merged.contains_key(key) {
            merged.insert(key.clone(), value.clone());
        }
    }
    finish_token(&mut merged, cfg.contains_key("token").then(|| cfg.get("token")))?;
    write_config(path, &merged)
}

/// The learner's save: merge only `updates` into the file on disk.
pub fn save_keys(path: &Path, updates: &Config) -> io::Result<()> {
    let _guard = SAVE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut merged = read_object(path);
    for (key, value) in updates {
        merged.insert(key.clone(), value.clone());
    }
    finish_token(&mut merged, updates.contains_key("token").then(|| updates.get("token")))?;
    write_config(path, &merged)
}

/// `supplied` is `Some(value)` when the caller passed `token` explicitly.
fn finish_token(merged: &mut Config, supplied: Option<Option<&Value>>) -> io::Result<()> {
    let to_protect = match supplied {
        Some(value) => {
            merged.shift_remove("token");
            let token = value.and_then(Value::as_str).unwrap_or_default().to_owned();
            if token.is_empty() {
                merged.shift_remove("tokenProtected");
                None
            } else {
                Some(token)
            }
        }
        None => merged.shift_remove("token").and_then(|v| v.as_str().map(str::to_owned)).filter(|t| !t.is_empty()),
    };
    if let Some(token) = to_protect {
        let blob = dpapi::protect(&token).map_err(io::Error::other)?;
        merged.insert("tokenProtected".into(), Value::String(blob));
    }
    Ok(())
}

fn write_config(path: &Path, merged: &Config) -> io::Result<()> {
    if path.exists() {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        let _ = fs::copy(path, bak);
    }
    let data = to_json_pretty(merged, 2).map_err(io::Error::other)?;
    write_atomic(path, &data, WriteOptions::default())
}

/// Read-only view used by the guardian: are credentials present?
pub fn configured(root: &Path) -> bool {
    let cfg = read_object(&root.join(FILE_NAME));
    let truthy = |k: &str| match cfg.get(k) {
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Null) | None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => true,
    };
    truthy("domain") && truthy("email") && (truthy("tokenProtected") || truthy("token"))
}

/// `cfg[key]` as a string, or `default` when absent/not a string.
pub fn get_str<'a>(cfg: &'a Config, key: &str, default: &'a str) -> &'a str {
    cfg.get(key).and_then(Value::as_str).unwrap_or(default)
}

/// `cfg[key] is True` (Python identity check: only a JSON `true` counts).
pub fn is_true(cfg: &Config, key: &str) -> bool {
    cfg.get(key) == Some(&Value::Bool(true))
}

/// Python truthiness of `cfg.get(key, default)` for booleans stored loosely.
pub fn truthy(cfg: &Config, key: &str, default: bool) -> bool {
    match cfg.get(key) {
        None => default,
        Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Config {
        match v {
            Value::Object(m) => m,
            _ => unreachable!(),
        }
    }

    fn setup() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        (dir, path)
    }

    fn disk(path: &Path) -> Config {
        read_object(path)
    }

    #[test]
    fn foreign_keys_survive_and_overlay_keys_are_removed() {
        let (_d, path) = setup();
        fs::write(&path, r#"{"domain":"a.b","resolvedLabel":"X","lastX":5,"lastY":6}"#).unwrap();
        save(&path, &obj(json!({"domain":"a.b","snapCorner":"top-left"}))).unwrap();
        let on_disk = disk(&path);
        assert_eq!(on_disk["resolvedLabel"], "X");
        assert!(!on_disk.contains_key("lastX") && !on_disk.contains_key("lastY"));
        assert_eq!(on_disk["snapCorner"], "top-left");
    }

    #[test]
    fn stale_learner_keys_in_memory_do_not_overwrite_newer_disk_values() {
        let (_d, path) = setup();
        fs::write(&path, r#"{"domain":"a.b","triageStatuses":["New A","New B"]}"#).unwrap();
        save(&path, &obj(json!({"domain":"a.b","triageStatuses":["Old"],"alpha":0.9}))).unwrap();
        assert_eq!(disk(&path)["triageStatuses"], json!(["New A", "New B"]));
        assert_eq!(disk(&path)["alpha"], 0.9);
    }

    #[test]
    fn partial_save_keeps_identity_and_encrypts_token() {
        let (_d, path) = setup();
        save(&path, &obj(json!({"domain":"a.b","email":"e@x","token":"t0k","serviceDeskId":11}))).unwrap();
        save(&path, &obj(json!({"alpha":0.5}))).unwrap();
        let on_disk = disk(&path);
        assert_eq!(on_disk["domain"], "a.b");
        assert_eq!(on_disk["serviceDeskId"], 11);
        assert!(!on_disk.contains_key("token"));
        assert_eq!(dpapi::unprotect(on_disk["tokenProtected"].as_str().unwrap()).as_deref(), Some("t0k"));
        let loaded = load(&path).unwrap();
        assert_eq!(loaded["token"], "t0k");
        assert!(!loaded.contains_key("tokenProtected"));
    }

    #[test]
    fn identity_keys_can_be_replaced_and_empty_token_clears() {
        let (_d, path) = setup();
        save(&path, &obj(json!({"domain":"a.b","token":"x"}))).unwrap();
        save(&path, &obj(json!({"domain":"c.d","token":""}))).unwrap();
        let on_disk = disk(&path);
        assert_eq!(on_disk["domain"], "c.d");
        assert!(!on_disk.contains_key("token") && !on_disk.contains_key("tokenProtected"));
    }

    #[test]
    fn backup_only_after_first_save() {
        let (d, path) = setup();
        save(&path, &obj(json!({"domain":"a.b"}))).unwrap();
        assert!(!d.path().join("jira_config.json.bak").exists());
        save(&path, &obj(json!({"domain":"c.d"}))).unwrap();
        assert!(fs::read_to_string(d.path().join("jira_config.json.bak")).unwrap().contains("a.b"));
    }

    #[test]
    fn plain_token_migrates_on_load() {
        let (_d, path) = setup();
        fs::write(&path, r#"{"domain":"a.b","token":"plain"}"#).unwrap();
        let cfg = load(&path).unwrap();
        assert_eq!(cfg["token"], "plain");
        let on_disk = disk(&path);
        assert!(!on_disk.contains_key("token"));
        assert!(on_disk.contains_key("tokenProtected"));
    }

    #[test]
    fn undecryptable_blob_gives_empty_token() {
        let (_d, path) = setup();
        fs::write(&path, r#"{"domain":"a.b","tokenProtected":"AAAA"}"#).unwrap();
        assert_eq!(load(&path).unwrap()["token"], "");
    }

    #[test]
    fn corrupt_and_non_object_files_move_aside() {
        for body in ["{not json", "[1,2]"] {
            let (d, path) = setup();
            fs::write(&path, body).unwrap();
            assert!(load(&path).is_none());
            assert!(d.path().join("jira_config.json.bad").exists(), "{body}");
        }
        let (_d, path) = setup();
        assert!(load(&path).is_none());
    }

    #[test]
    fn normalise_repairs_types_and_leaves_valid_alone() {
        let mut cfg = obj(json!({
            "overlayWidth": "300", "alpha": "lots", "refreshSeconds": 1, "maxQueueSize": -5,
            "alertQueues": "Open", "hiddenQueues": 7, "skipKeywords": "",
            "slaField": null, "trayBadgeQueue": 12, "projectKey": "SD OR 1=1",
            "checkForUpdates": "yes", "autoInstallUpdates": 1
        }));
        normalise(&mut cfg);
        assert_eq!(cfg["overlayWidth"], 300);
        assert_eq!(cfg["alpha"], 0.93);
        assert_eq!(cfg["refreshSeconds"], 5);
        assert_eq!(cfg["maxQueueSize"], 0);
        assert_eq!(cfg["alertQueues"], json!(["Open"]));
        assert_eq!(cfg["hiddenQueues"], json!([]));
        assert_eq!(cfg["skipKeywords"], json!([]));
        assert_eq!(cfg["slaField"], "");
        assert_eq!(cfg["trayBadgeQueue"], "12");
        assert_eq!(cfg["projectKey"], "");
        assert_eq!(cfg["checkForUpdates"], false);
        assert_eq!(cfg["autoInstallUpdates"], false);

        let valid = obj(json!({"overlayWidth": 260, "alpha": 0.95, "refreshSeconds": 15,
            "alertQueues": ["A"], "projectKey": "SD", "slaField": "Time to resolution"}));
        let mut copy = valid.clone();
        normalise(&mut copy);
        assert_eq!(copy, valid);
    }

    #[test]
    fn learner_save_merges_only_given_keys() {
        let (_d, path) = setup();
        save(&path, &obj(json!({"domain":"a.b","token":"t","alpha":0.4}))).unwrap();
        save_keys(&path, &obj(json!({"triageStatuses":["X"]}))).unwrap();
        let on_disk = disk(&path);
        assert_eq!(on_disk["alpha"], 0.4);
        assert_eq!(on_disk["triageStatuses"], json!(["X"]));
        assert!(on_disk.contains_key("tokenProtected"));
        assert!(configured(path.parent().unwrap()) == on_disk.contains_key("email"));
    }
}
