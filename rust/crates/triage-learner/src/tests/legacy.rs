//! Existing users' files must load unchanged: legacy config keys, labels,
//! history rows and tuning state; plus the housekeeping caps.

use std::collections::BTreeSet;

use chrono::{Duration, Local, TimeZone};
use serde_json::json;

use super::*;

#[test]
fn a_tuned_config_is_upgraded_and_an_old_schema_retunes() {
    let dir = tempfile::tempdir().unwrap();
    let state = json!({"tunedConfig": {"mistakeWeight": 4, "customKnob": 1}, "tuningSchemaVersion": 7,
                       "lastTuneDate": "2026-11-01", "stats": {"predictions": 3, "correct": 2}});
    let w = watcher(dir.path(), json!({}), Some(state), &FakeJira::new());
    let tuned = w.state["tunedConfig"].as_object().unwrap();
    let keys: Vec<&str> = tuned.keys().map(String::as_str).collect();
    let mut want: Vec<&str> = triage_engine::tuning::BALANCED.iter().map(|(k, _)| *k).collect();
    want.push("customKnob");
    assert_eq!(keys, want, "BALANCED order, then extras");
    assert_eq!(tuned["mistakeWeight"], json!(4));
    assert_eq!(tuned["priorWeight"], json!(0.5));
    assert_eq!(w.tuning.get("mistakeWeight"), 4.0);
    assert_eq!(w.tuning.get("priorWeight"), 0.5);
    assert_eq!(w.tuning.get("reqSmooth"), 10.0, "knobs outside BALANCED keep their defaults");
    assert_eq!(w.state["tuningSchemaVersion"], json!(8));
    assert_eq!(w.state["lastTuneDate"], json!(""));
    let disk = read_json(&dir.path().join("triage_state.json"));
    assert_eq!(disk["lastTuneDate"], json!(""));
    assert_eq!(disk["stats"], json!({"predictions": 3, "correct": 2}), "unknown state keys survive");

    // Without a tuned config the live defaults stay (mistakeWeight 3).
    let dir = tempfile::tempdir().unwrap();
    let w =
        watcher(dir.path(), json!({}), Some(json!({"tuningSchemaVersion": 8, "lastTuneDate": "x"})), &FakeJira::new());
    assert_eq!(w.tuning.get("mistakeWeight"), 3.0);
    assert_eq!(w.state["lastTuneDate"], json!("x"));
}

#[test]
fn legacy_config_keys_and_labels() {
    let dir = tempfile::tempdir().unwrap();
    let w = watcher(
        dir.path(),
        json!({"resolvedStatus": "Closed", "resolvedLabel": "Fixed here", "movedLabels": {"PPS": "Proclaim bug"},
               "movedLabel": "Escalated to {project}", "moveTargetProjects": ["PPS", "bad key!", "DEV"],
               "triageStatuses": "With DEV"}),
        None,
        &FakeJira::new(),
    );
    assert_eq!(w.resolved, BTreeSet::from(["Closed".to_owned()]), "singular resolvedStatus");
    assert_eq!(w.dests, ["With DEV"], "a bare string is one status");
    assert_eq!(w.labels.move_targets, ["PPS", "DEV"]);
    assert_eq!(w.all_dests(), ["With DEV", "Fixed here", "Proclaim bug", "Escalated to DEV"]);
    assert_eq!(w.canonical_dest("Proclaim bug (moved to DEV)"), "Escalated to DEV");
    assert_eq!(w.canonical_dest("Proclaim bug (moved to PPS)"), "Proclaim bug");
    assert_eq!(w.learner.plan_for("Fixed here"), "resolve in HD — no handoff");
    assert_eq!(w.learner.plan_for("Escalated to DEV"), "move the ticket to the DEV project");
}

#[test]
fn legacy_rows_load_and_canonicalise() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = watcher(
        dir.path(),
        json!({"moveTargetProjects": ["PPS"], "triageStatuses": ["With DEV"]}),
        None,
        &FakeJira::new(),
    );
    // Old history rows: reqType, no featureOrigin, no key.
    let rows: Vec<Value> = (0..12)
        .map(|i| {
            json!({"when": format!("2026-09-{:02} 10:00", i + 1), "dest": "Proclaim bug (moved to PPS)",
                   "summary": format!("crash {i}"), "reqType": "Report a bug"})
        })
        .collect();
    w.rebuild_model(&rows);
    assert!(w.learner.model.req_types["Proclaim bug (moved to PPS)"].contains_key("Report a bug"));
    // Legacy labels in the log are reported under today's names.
    w.log(obj(json!({"event": "routed", "key": "HD-1", "dest": "Proclaim bug (moved to PPS)",
                     "predicted": "Proclaim bug (moved to PPS)", "predictionCorrect": true, "review": false})))
        .unwrap();
    w.state.insert("confusion".into(), json!({"Proclaim bug (moved to PPS) → Moved to PPS": 2}));
    w.write_report().unwrap();
    let report = std::fs::read_to_string(dir.path().join("TRIAGE_REPORT.md")).unwrap();
    assert!(report.contains("| Moved to PPS | 1/1 (100%) | 1/1 (100%) | 1/1 (100%) |"), "{report}");
    assert!(report.contains("- Moved to PPS: 100% (2/2)"), "{report}");
    assert!(
        report.contains("- `HD-1` **Moved to PPS** by ? (clones: none) — I said **Proclaim bug (moved to PPS)** ✅")
    );
}

#[test]
fn the_window_is_written_in_the_jira_accounts_timezone() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    fake.with(|j| j.myself = json!({"timeZone": "Pacific/Auckland"}));
    let mut w = watcher(dir.path(), json!({}), None, &fake);
    w.tz = None;
    let last = jo_core::lifecycle::now_epoch() - 3600.0;
    w.state.insert("lastPollEpoch".into(), json!(last));
    let tz: chrono_tz::Tz = "Pacific/Auckland".parse().unwrap();
    let prev = chrono::Utc.timestamp_opt(last as i64, 0).unwrap().with_timezone(&tz).naive_local();
    let want = format!("\"{}\"", (prev - Duration::seconds(600)).format("%Y/%m/%d %H:%M"));
    assert_eq!(w.window_jql_time(), want);
    assert!(matches!(w.tz, Some(crate::watcher::JiraTz::Named(_))));
    let lookups = |fake: &FakeJira| fake.with(|j| j.gets.iter().filter(|g| g.ends_with("/myself")).count());
    assert_eq!(lookups(&fake), 1);
    w.window_jql_time();
    assert_eq!(lookups(&fake), 1, "resolved once per process");
    // Never polled: two hours back. Unknown zone: this machine's.
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    fake.with(|j| j.myself = json!({"timeZone": "Mars/Olympus"}));
    let mut w = watcher(dir.path(), json!({}), None, &fake);
    w.tz = None;
    let want = format!("\"{}\"", (Local::now().naive_local() - Duration::hours(2)).format("%Y/%m/%d %H:%M"));
    assert_eq!(w.window_jql_time(), want);
    assert!(matches!(w.tz, Some(crate::watcher::JiraTz::Local)));
}

#[test]
fn housekeeping_caps_and_scoring() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    let issue = json!({"key": "HD-1", "fields": {"summary": "printer jam"}});
    fake.with(|j| j.changelogs.insert("HD-1".into(), vec![status_change("5", "With DEV")]));
    fake.on_search(move |jql| if jql.contains("With DEV") { vec![issue.clone()] } else { vec![] });
    let old = (Local::now() - Duration::days(40)).format("%Y-%m-%d %H:%M").to_string();
    let recent = Local::now().format("%Y-%m-%d %H:%M").to_string();
    let seen: Vec<String> = (0..20005).map(|i| format!("X-{i}:1")).collect();
    let cases: Vec<Value> = (0..200).map(|i| json!({"key": format!("C-{i}")})).collect();
    let state = json!({"seenTransitions": seen, "reviewCases": cases, "predicted": {
        "HD-1": {"dest": "With DATA", "conf": 88, "second": "With DEV", "secondConf": 10, "review": false,
                 "source": "model", "at": recent},
        "HD-OLD": {"dest": null, "at": old}}});
    let mut w = watcher(
        dir.path(),
        json!({"triageStatuses": ["With DEV", "With DATA"], "resolvedStatuses": ["Done"]}),
        Some(state),
        &fake,
    );
    w.resolve_and_learn().unwrap();
    let seen = w.state["seenTransitions"].as_array().unwrap();
    assert_eq!(seen.len(), crate::watcher::SEEN_CAP);
    assert_eq!(seen.last().unwrap(), &json!("HD-1:5"), "the oldest are dropped, the newest kept");
    assert!(w.state["predicted"].get("HD-OLD").is_none(), "30-day predictions only");
    assert_eq!(w.state["predicted"]["HD-1"]["scored"], json!(true));
    let cases = w.state["reviewCases"].as_array().unwrap();
    assert_eq!(cases.len(), 200);
    assert_eq!(cases.last().unwrap()["key"], json!("HD-1"));
    assert_eq!(cases.last().unwrap()["predicted"], json!("With DATA"));
    assert_eq!(w.state["stats"], json!({"predictions": 1, "correct": 0}));
    assert_eq!(w.state["confusion"], json!({"With DATA → With DEV": 1}));
    let today = Local::now().format("%Y-%m-%d").to_string();
    assert_eq!(w.state["dailyStats"][&today], json!([1, 0]));
    assert_eq!(w.learner.model.seen.get("With DEV").copied(), Some(1.0));
    let routed = read_lines(&dir.path().join("triage_log.jsonl")).pop().unwrap();
    let keys: Vec<&String> = routed.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "event",
            "key",
            "summary",
            "dest",
            "by",
            "when",
            "requestType",
            "clones",
            "predicted",
            "confidence",
            "second",
            "secondConfidence",
            "review",
            "source",
            "predictionCorrect",
            "ts"
        ]
    );
    assert_eq!(routed["predictionCorrect"], json!(false));
}
