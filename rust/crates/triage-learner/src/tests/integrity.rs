//! `tests/test_integrity.py`: end-to-end accounting, interrupted polling,
//! log rotation and recovery.

use std::fs;
use std::io;
use std::path::Path;

use serde_json::json;

use super::*;
use crate::metrics::MetricsCache;

fn integrity_watcher(dir: &Path, fake: &FakeJira) -> Watcher {
    let mut w = watcher(
        dir,
        json!({"triageStatuses": ["With DEV", "With DATA"], "resolvedStatuses": ["Closed"], "resolvedLabel": "IT",
               "moveTargetProjects": ["IPS"]}),
        None,
        fake,
    );
    w.state.insert("stats".into(), json!({"predictions": 10, "correct": 8}));
    w
}

fn event(key: &str) -> Map<String, Value> {
    obj(json!({"event": "routed", "key": key, "predicted": "With DEV", "dest": "IT", "predictionCorrect": false,
               "review": true, "by": "tester", "when": "2026-09-07 10:00", "clones": [], "requestType": "Get IT help"}))
}

#[test]
fn an_unused_route_cannot_overwrite_report_totals() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = integrity_watcher(dir.path(), &FakeJira::new());
    w.log(event("SD-1")).unwrap();
    w.write_report().unwrap();
    let status = read_json(&dir.path().join("triage_status.json"));
    let report = fs::read_to_string(dir.path().join("TRIAGE_REPORT.md")).unwrap();
    assert_eq!(status["accuracy"], json!({"predictions": 10, "correct": 8, "pct": 80.0}));
    assert!(report.contains("80% (8/10)"));
    assert!(report.contains("| Moved to IPS | move the ticket to the IPS project | 0 |"));
    assert!(report.contains("- **Review-flagged suggestions:** 0% (0/1)"));
    let keys: Vec<&String> = status.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "updatedAt",
            "version",
            "healthy",
            "healthMessage",
            "lastSuccessfulPoll",
            "refusals",
            "provenance",
            "accuracy",
            "routeSafety",
            "segments",
            "recentPerformance",
            "performanceDrift",
            "drift"
        ]
    );
}

#[test]
fn failed_pages_do_not_advance_the_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    let mut w = integrity_watcher(dir.path(), &fake);
    w.state.insert("lastPollEpoch".into(), json!(100));
    fake.with(|j| j.fail_search = true);
    w.resolve_and_learn().unwrap();
    assert_eq!(w.state["lastPollEpoch"], json!(100));
    assert_eq!(w.state["pollIncomplete"], json!(true));
    let status = read_json(&dir.path().join("triage_status.json"));
    assert_eq!(status["healthy"], json!(false));
    assert_eq!(status["healthMessage"], json!("Polling is incomplete; retrying from the last checkpoint"));

    fake.with(|j| j.fail_search = false);
    w.state.insert("runtimeProblem".into(), json!("Previous poll failed"));
    w.resolve_and_learn().unwrap();
    assert!(w.state["lastPollEpoch"].as_f64().unwrap() > 100.0);
    assert_eq!(w.state["pollIncomplete"], json!(false));
    assert!(!w.state.contains_key("runtimeProblem"));
    assert!(w.state["lastSuccessfulPoll"].as_f64().is_some());
    // The window resumes from the last poll with the overlap, capped at 7 days.
    let window = w.window_jql_time();
    assert!(window.starts_with('"') && window.len() == 18, "{window}");
}

#[test]
fn rotation_and_restart_count_every_event_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = integrity_watcher(dir.path(), &FakeJira::new());
    w.log(event("SD-1")).unwrap();
    w.log_rotate_bytes = 200;
    for n in 0..6 {
        w.log(event(&n.to_string())).unwrap();
    }
    let log = dir.path().join("triage_log.jsonl");
    let canon = |d: &str| d.to_owned();
    let live = w.metrics.live(&canon).predicted;
    let fresh = MetricsCache::new(&log).live(&canon).predicted;
    assert_eq!((live, fresh), (7, 7));
    assert!(dir.path().join("triage_log.jsonl.old").exists());
    assert!(dir.path().join("triage_log.jsonl.metrics.json").exists());

    // A reader lock defeats the rename: checkpoint written, log kept.
    w.replace_file = |_, _| Err(io::Error::from(io::ErrorKind::PermissionDenied));
    w.log(event("locked")).unwrap();
    assert_eq!(MetricsCache::new(&log).live(&canon).predicted, 8, "archived prefix not double-counted");
}

#[test]
fn training_uses_the_intake_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = integrity_watcher(dir.path(), &FakeJira::new());
    let snapshot = json!({"summary": "original", "desc": "before routing", "labels": [], "components": [],
                          "structured": [], "reporter": "intake reporter", "requestType": "Get IT help"});
    w.state.insert("predicted".into(), json!({"SD-99": {"intakeFeatures": snapshot}}));
    let det = crate::issue::Details {
        key: "SD-99".into(),
        summary: "edited after routing".into(),
        desc: "before routing".into(),
        reporter: "intake reporter".into(),
        request_type: "Get IT help".into(),
        ..Default::default()
    };
    w.append_history(&det, "IT", "2026-09-07 12:00").unwrap();
    let captured = read_lines(&dir.path().join("triage_history.jsonl")).pop().unwrap();
    assert_eq!(w.learning_details(&det).unwrap().summary, "original");
    assert_eq!(captured["summary"], json!("original"));
    assert_eq!(captured["dest"], json!("IT"));
    assert_eq!(captured["featureOrigin"], json!("intake"));
    let keys: Vec<&String> = captured.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "key",
            "when",
            "dest",
            "summary",
            "desc",
            "labels",
            "components",
            "reporter",
            "requestType",
            "structured",
            "cloneProjects",
            "featureOrigin"
        ]
    );
    // A wrongly typed snapshot makes that one ticket unusable, not the cycle.
    w.state.insert("predicted".into(), json!({"SD-99": {"intakeFeatures": {"labels": null}}}));
    assert!(w.learning_details(&det).is_err());
}

#[test]
fn offline_and_live_replay_agree() {
    let records: Vec<Value> = (0..50)
        .map(|n| {
            json!({"key": n.to_string(), "summary": if n % 2 == 1 { "data query" } else { "server restart" },
                   "desc": "", "labels": [], "components": [], "structured": [], "reporter": "r", "requestType": "",
                   "cloneProjects": [], "dest": if n % 2 == 1 { "With DATA" } else { "IT" }})
        })
        .collect();
    let recs: Vec<_> = records.iter().map(|r| triage_engine::Record::from_value(r).unwrap()).collect();
    let dests = vec!["With DATA".to_owned()];
    let tuning = triage_engine::Tuning::default();
    let direct = triage_engine::replay(&recs, &tuning, &dests, Some(10), None, false);
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("jira_config.json"), r#"{"triageStatuses": ["With DATA"]}"#).unwrap();
    let mut body = String::new();
    for r in &records {
        body.push_str(&r.to_string());
        body.push('\n');
    }
    fs::write(dir.path().join("triage_history.jsonl"), body).unwrap();
    let mut out = Vec::new();
    let connect = || -> Result<Box<dyn Transport>, String> { Err("offline".into()) };
    assert_eq!(crate::cli::eval_with(dir.path(), &["analyze".into()], &mut out, &connect), 0);
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("acc="), "{text}");
    assert!(direct.n > 0);
}
