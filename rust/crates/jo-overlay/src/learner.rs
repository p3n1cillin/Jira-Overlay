//! Files the (optional) triage learner writes next to the app: routing
//! suggestions for the alert popup and its health for Settings. Another
//! process writes both, so nothing in them is taken on trust.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use jo_core::config::{self, Config};
use serde_json::{Map, Value};

use crate::util;

/// Suggestions older than this are from a learner that has stopped.
const SUGGESTIONS_MAX_AGE: f64 = 900.0;

/// Display-only routing suggestions, re-parsed only when the file changes.
#[derive(Default)]
pub struct Suggestions {
    cached: Map<String, Value>,
    mtime: Option<SystemTime>,
}

impl Suggestions {
    /// The suggestions file: always next to the app (basename only, so a
    /// hand-edited absolute path or `..\` traversal can't point elsewhere).
    pub fn path(root: &Path, cfg: &Config) -> PathBuf {
        let configured = config::get_str(cfg, "suggestionsFile", "");
        let name = util::basename(if configured.is_empty() { "triage_suggestions.json" } else { configured });
        root.join(name)
    }

    pub fn load(&mut self, root: &Path, cfg: &Config) -> Map<String, Value> {
        let path = Self::path(root, cfg);
        let Ok(mtime) = fs::metadata(&path).and_then(|m| m.modified()) else { return Map::new() };
        let age = SystemTime::now().duration_since(mtime).map_or(0.0, |d| d.as_secs_f64());
        if age > SUGGESTIONS_MAX_AGE {
            return Map::new();
        }
        if self.mtime == Some(mtime) {
            return self.cached.clone(); // unchanged since last read
        }
        let Ok(text) = fs::read(&path) else { return Map::new() };
        let Ok(parsed) = serde_json::from_slice::<Value>(&text) else { return Map::new() };
        self.cached = match parsed {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        self.mtime = Some(mtime);
        self.cached.clone()
    }
}

/// Python `datetime.fromisoformat` for the shapes the learner writes.
/// Naive stamps are local time.
fn parse_iso(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
    if let Ok(stamp) = DateTime::parse_from_rfc3339(text) {
        return Some(stamp.with_timezone(&Utc));
    }
    if let Ok(stamp) = DateTime::parse_from_rfc3339(&text.replacen(' ', "T", 1)) {
        return Some(stamp.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M%:z"] {
        if let Ok(stamp) = DateTime::parse_from_str(text, format) {
            return Some(stamp.with_timezone(&Utc));
        }
    }
    let naive = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(text, f).ok())
        .or_else(|| NaiveDate::parse_from_str(text, "%Y-%m-%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0)))?;
    Local.from_local_datetime(&naive).earliest().map(|t| t.with_timezone(&Utc))
}

/// Health derived from both heartbeat age and poll completion (Python
/// `triage_support.status_view`): stale beyond 300 s, or more than 60 s in
/// the future, reads as unhealthy.
pub fn status_view(data: &Map<String, Value>, now: DateTime<Utc>) -> Map<String, Value> {
    let mut result = data.clone();
    let stamp = data.get("updatedAt").map(|v| match v {
        Value::String(s) => s.clone(),
        other => util::py_str(other),
    });
    let stale = match stamp.as_deref().and_then(parse_iso) {
        Some(at) => {
            let age = (now - at).num_milliseconds() as f64 / 1000.0;
            !(-60.0..=300.0).contains(&age)
        }
        None => true,
    };
    if stale {
        result.insert("healthy".into(), Value::Bool(false));
        result
            .insert("healthMessage".into(), Value::String("Learner status is stale; check that it is running".into()));
    } else if !data.get("healthy").is_none_or(|v| util::truthy(Some(v))) {
        result.entry("healthMessage").or_insert_with(|| Value::String("Learner polling needs attention".into()));
    }
    result
}

/// `triage_status.json` through [`status_view`]; `None` when the learner
/// isn't installed, hasn't reported yet, or wrote something unreadable.
pub fn load_status(path: &Path) -> Option<Map<String, Value>> {
    let data: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    Some(status_view(data.as_object()?, Utc::now()))
}

/// What the Settings panel's "Triage learner" block shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LearnerLines {
    pub healthy: bool,
    pub headline: String,
    /// `"500 decisions learned · last tuned … · 91% accurate · v…"`, if any.
    pub details: Option<String>,
    /// `"⚠︎ Drift since …: …"`, if any.
    pub drift: Option<String>,
}

/// Build the block from whatever shape the status file had.
pub fn learner_lines(status: &Map<String, Value>) -> LearnerLines {
    let healthy = status.get("healthy").is_none_or(|v| util::truthy(Some(v)));
    let empty = Map::new();
    let prov = status.get("provenance").and_then(Value::as_object).unwrap_or(&empty);
    let acc = status.get("accuracy").and_then(Value::as_object).unwrap_or(&empty);
    let headline = if healthy {
        "Healthy".to_owned()
    } else {
        let codes = match status.get("refusals") {
            Some(Value::Array(items)) => items.iter().map(util::py_str).collect::<Vec<_>>().join(", "),
            _ => String::new(),
        };
        match status.get("healthMessage").filter(|v| util::truthy(Some(v))) {
            Some(message) => util::py_str(message),
            None => format!("Jira is refusing requests ({})", if codes.is_empty() { "?" } else { &codes }),
        }
    };
    let mut details = Vec::new();
    if let Some(trained) = prov.get("trainedOnDecisions").filter(|v| util::truthy(Some(v))) {
        details.push(format!("{} decisions learned", util::py_str(trained)));
    }
    if let Some(date) = prov.get("lastTuneDate").filter(|v| util::truthy(Some(v))) {
        details.push(format!("last tuned {}", util::py_str(date)));
    }
    match acc.get("pct") {
        Some(Value::Number(n)) => details.push(format!("{:.0}% accurate", n.as_f64().unwrap_or(0.0))),
        Some(Value::Bool(b)) => details.push(format!("{}% accurate", u8::from(*b))),
        _ => {}
    }
    if let Some(version) = status.get("version").filter(|v| util::truthy(Some(v))) {
        details.push(format!("v{}", util::py_str(version)));
    }
    let drift = status.get("drift").and_then(Value::as_object).and_then(|d| {
        let detail = d.get("detail").filter(|v| util::truthy(Some(v)))?;
        let date = d.get("date").map_or_else(|| "last tune".to_owned(), util::py_str);
        Some(format!("⚠\u{FE0E} Drift since {date}: {}", util::py_str(detail)))
    });
    LearnerLines { healthy, headline, details: (!details.is_empty()).then(|| details.join(" · ")), drift }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs::File;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn suggestions_from_untrusted_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("suggestions.json");
        let cfg = map(json!({"suggestionsFile": path.to_string_lossy()}));
        for (content, valid) in [
            (r#"{"SD-1": {"label": "With DEV"}}"#, true),
            (r#"["nope"]"#, false),
            ("null", false),
            (r#"{"SD-1": {"label": "With"#, false),
            ("{}", false),
        ] {
            fs::write(&path, content).unwrap();
            let loaded = Suggestions::default().load(dir.path(), &cfg);
            assert_eq!(loaded.contains_key("SD-1"), valid, "{content}");
        }
        fs::write(&path, r#"{"SD-1": {"label": "With DEV"}}"#).unwrap();
        let mut reader = Suggestions::default();
        let first = reader.load(dir.path(), &cfg);
        let second = reader.load(dir.path(), &cfg);
        assert_eq!(first, second, "a second read returns the same suggestions");
        let old = SystemTime::now() - std::time::Duration::from_secs(3600);
        File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
        assert!(Suggestions::default().load(dir.path(), &cfg).is_empty(), "stale suggestions are dropped");
    }

    #[test]
    fn suggestions_path_is_confined() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("zz_probe.json"), r#"{"KEY-1": {"label": "from project dir"}}"#).unwrap();
        let traversal = map(json!({"suggestionsFile": r"..\..\..\..\nowhere\secret.json"}));
        assert!(Suggestions::default().load(dir.path(), &traversal).is_empty());
        assert_eq!(Suggestions::path(dir.path(), &traversal), dir.path().join("secret.json"));
        let reachable = map(json!({"suggestionsFile": r"..\..\zz_probe.json"}));
        assert_eq!(Suggestions::default().load(dir.path(), &reachable)["KEY-1"]["label"], "from project dir");
        assert_eq!(Suggestions::path(dir.path(), &map(json!({}))), dir.path().join("triage_suggestions.json"));
    }

    #[test]
    fn status_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.json");
        let now = Local::now().naive_local().format("%Y-%m-%dT%H:%M:%S%.6f").to_string();
        fs::write(
            &path,
            json!({"healthy": true, "updatedAt": now, "refusals": [],
            "provenance": {"trainedOnDecisions": 3278, "lastTuneDate": "2026-08-18"},
            "accuracy": {"pct": 89.7}})
            .to_string(),
        )
        .unwrap();
        assert_eq!(load_status(&path).unwrap()["healthy"], true, "a healthy status file loads");

        fs::write(&path, r#"{"healthy": false, "refusals": [401]}"#).unwrap();
        let loaded = load_status(&path).unwrap();
        assert!(loaded["healthy"] == false && loaded["refusals"] == json!([401]));

        assert!(load_status(&dir.path().join("nope.json")).is_none(), "no learner installed is not an error");
        fs::write(&path, "{not json").unwrap();
        assert!(load_status(&path).is_none());
        fs::write(&path, "[1, 2, 3]").unwrap();
        assert!(load_status(&path).is_none());
    }

    #[test]
    fn staleness_window() {
        let now = Utc::now();
        let at = |secs: i64| {
            map(json!({"healthy": true, "updatedAt": (now - chrono::Duration::seconds(secs)).to_rfc3339()}))
        };
        assert_eq!(status_view(&at(10), now)["healthy"], true);
        assert_eq!(status_view(&at(301), now)["healthy"], false, "older than 300 s");
        assert_eq!(status_view(&at(-61), now)["healthy"], false, "more than 60 s in the future");
        assert_eq!(status_view(&at(-30), now)["healthy"], true);
        let unhealthy = map(json!({"healthy": false, "updatedAt": now.to_rfc3339()}));
        assert_eq!(status_view(&unhealthy, now)["healthMessage"], "Learner polling needs attention");
        let z = map(json!({"updatedAt": now.format("%Y-%m-%dT%H:%M:%SZ").to_string()}));
        assert!(!status_view(&z, now).contains_key("healthMessage"));
    }

    #[test]
    fn learner_block_survives_any_shape() {
        let lines = learner_lines(&map(json!({
            "healthy": false, "refusals": [401, 403],
            "provenance": {"trainedOnDecisions": 500, "lastTuneDate": "2026-08-01"},
            "accuracy": {"pct": 91.2},
            "drift": {"date": "2026-08-01", "detail": "balanced accuracy dropped 20% (90% -> 70%)"}
        })));
        assert!(lines.headline.contains("refusing requests (401, 403)"));
        assert_eq!(lines.details.as_deref(), Some("500 decisions learned · last tuned 2026-08-01 · 91% accurate"));
        assert!(lines.drift.unwrap().contains("balanced accuracy dropped 20%"));
        for data in [
            json!({}),
            json!({"healthy": "yes"}),
            json!({"healthy": true, "provenance": "nope"}),
            json!({"healthy": true, "provenance": [1, 2, 3]}),
            json!({"healthy": true, "accuracy": "nope"}),
            json!({"healthy": true, "accuracy": {"pct": "high"}}),
            json!({"healthy": false, "refusals": "oops"}),
            json!({"healthy": true, "drift": "weird"}),
        ] {
            let lines = learner_lines(&map(data.clone()));
            assert!(!lines.headline.is_empty(), "{data}");
        }
        assert_eq!(
            learner_lines(&map(json!({"healthy": false, "refusals": "oops"}))).headline,
            "Jira is refusing requests (?)"
        );
        let stale = learner_lines(&map(
            json!({"healthy": false, "healthMessage": "Learner status is stale; check that it is running"}),
        ));
        assert_eq!(stale.headline, "Learner status is stale; check that it is running");
        assert_eq!(learner_lines(&map(json!({"version": "2026.11.24"}))).details.as_deref(), Some("v2026.11.24"));
    }
}
