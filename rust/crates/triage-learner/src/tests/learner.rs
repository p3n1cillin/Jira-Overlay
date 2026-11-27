//! `tests/test_learner.py`: the learner must work on anyone's service desk.

use std::collections::BTreeSet;
use std::fs;
use std::sync::{Arc, Mutex};

use chrono::{Duration, Local};
use serde_json::{Value, json};
use triage_engine::text::tokenize;
use triage_engine::{Learner, Model, Tuning};

use super::*;
use crate::support::Counts;
use crate::watcher::TUNE_WINDOW;

/// The foreign desk: 30 closed, 15 to Networks, 10 to Security, 5 waiting.
fn foreign_desk(fake: &FakeJira) {
    let mut issues = Vec::new();
    let mut add = |key: String, last: &str, path: &[&str]| {
        issues.push(json!({"key": key, "fields": {"status": {"name": last}, "summary": format!("ticket {key}")}}));
        let log: Vec<Value> = path.iter().enumerate().map(|(i, st)| status_change(&format!("c{i}"), st)).collect();
        fake.with(|j| j.changelogs.insert(key.clone(), log));
    };
    for i in 0..30 {
        add(format!("HD-{i}"), "Closed", &["Investigating", "Closed"]);
    }
    for i in 30..45 {
        add(format!("HD-{i}"), "Passed to Networks", &["Investigating", "Passed to Networks"]);
    }
    for i in 45..55 {
        add(format!("HD-{i}"), "Passed to Security", &["Passed to Security"]);
    }
    for i in 55..60 {
        add(format!("HD-{i}"), "Awaiting user", &["Investigating", "Awaiting user"]);
    }
    fake.on_search(move |jql| if jql.contains("updated >=") { issues.clone() } else { vec![] });
}

#[test]
fn configures_itself_from_a_foreign_desk() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    foreign_desk(&fake);
    let mut w = watcher(dir.path(), json!({}), None, &fake);
    assert_eq!(w.req_field, "customfield_99001", "Request Type found by its JSM schema");
    assert_eq!(
        w.context_fields.iter().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from(["customfield_99002".to_owned(), "customfield_99003".to_owned()])
    );
    let cfg = read_json(&dir.path().join("jira_config.json"));
    assert_eq!(cfg["requestTypeField"], json!("customfield_99001"), "field ids remembered");
    assert!(cfg.get("token").is_none(), "the token never lands on disk in plain text");

    assert!(w.ensure_workflow_known().unwrap());
    assert_eq!(w.resolved, BTreeSet::from(["Closed".to_owned()]), "the busiest settled status is 'resolved'");
    assert_eq!(
        w.dests.iter().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from(["Passed to Networks".to_owned(), "Passed to Security".to_owned()])
    );
    assert!(!w.dests.iter().any(|d| ["Investigating", "Awaiting user", "Triage"].contains(&d.as_str())));
    assert_eq!(w.labels.resolved_label, "Resolved in HD");
    assert_eq!(w.labels.moved_label("BUGS"), "Moved to BUGS");
    assert_eq!(w.state["workflowDiscovered"], json!(true));
    let cfg = read_json(&dir.path().join("jira_config.json"));
    assert_eq!(cfg["resolvedStatuses"], json!(["Closed"]));
    assert_eq!(cfg["triageStatuses"], json!(["Passed to Networks", "Passed to Security"]));
    let log = fs::read_to_string(dir.path().join("triage_learner.log")).unwrap();
    assert!(log.contains(
        "discovered outcomes for HD: resolved=['Closed'] handoffs=['Passed to Networks', 'Passed to Security']"
    ));
}

#[test]
fn colliding_labels_and_null_changelog_items() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    let collider = watcher(
        dir.path(),
        json!({"triageStatuses": ["Passed to Networks"], "resolvedLabel": "Passed to Networks"}),
        None,
        &fake,
    );
    assert_eq!(collider.all_dests(), ["Passed to Networks"]);

    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    foreign_desk(&fake);
    fake.with(|j| {
        let real = j.changelogs["HD-0"].clone();
        let mut with_null = vec![json!({"id": "cnull", "items": null})];
        with_null.extend(real);
        j.changelogs.insert("HD-0".into(), with_null);
    });
    let mut w = watcher(dir.path(), json!({}), None, &fake);
    assert!(w.ensure_workflow_known().unwrap(), "items: null doesn't stop discovery");
    assert_eq!(w.resolved, BTreeSet::from(["Closed".to_owned()]));
}

#[test]
fn jql_values_are_quoted_and_keys_validated() {
    use jo_core::jql::{quote, safe_project_key};
    assert_eq!(quote("O\"Brien"), "\"O\\\"Brien\"");
    assert_eq!(quote("a\\\" OR \"1\"=\"1"), "\"a\\\\\\\" OR \\\"1\\\"=\\\"1\"");
    for key in ["HD\" OR project = \"SECRET", "HD OR 1=1", "", "1HD"] {
        assert!(!safe_project_key(key), "{key:?}");
    }
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    let mut evil = watcher(
        dir.path(),
        json!({"triageStatuses": ["Passed\" OR project = \"SECRET"], "resolvedStatuses": []}),
        None,
        &fake,
    );
    evil.backfill().unwrap();
    let queries = fake.queries().concat();
    assert!(!queries.contains("OR project = \"SECRET") && queries.contains("\\\""), "{queries}");

    // An invalid move-target key is never adopted (it would be persisted).
    evil.remember_move_target("BUGS\" OR project = \"SECRET").unwrap();
    assert!(evil.labels.move_targets.is_empty());
    evil.remember_move_target("BUGS").unwrap();
    assert_eq!(evil.labels.move_targets, ["BUGS"]);
    assert_eq!(read_json(&dir.path().join("jira_config.json"))["moveTargetProjects"], json!(["BUGS"]));
    assert_eq!(evil.learner.plan_for("Moved to BUGS"), "move the ticket to the BUGS project");
}

#[test]
fn resolve_in_place_desks_never_issue_an_empty_changed_to() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    let mut w2 = watcher(dir.path(), json!({"resolvedStatuses": ["Done"], "triageStatuses": []}), None, &fake);
    w2.resolve_and_learn().unwrap();
    let queries = fake.queries();
    assert!(!queries.iter().any(|q| q.contains("CHANGED TO ()")));
    assert!(queries.iter().any(|q| q.contains("CHANGED TO (\"Done\")")));

    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    let mut w3 = watcher(dir.path(), json!({"resolvedStatuses": ["Done"], "triageStatuses": []}), None, &fake);
    w3.backfill().unwrap();
    assert!(!fake.queries().iter().any(|q| q.contains("CHANGED TO ()")));
}

#[test]
fn drift_is_flagged_but_not_on_small_sample_noise() {
    let prior = obj(json!({"date": "2026-08-01", "balanced": 0.90, "firmCov": 0.85,
                           "per": {"With BI": 0.80, "With DEV": 0.90}}));
    let noisy = obj(json!({"date": "2026-08-02", "balanced": 0.895, "firmCov": 0.86,
                           "per": {"With BI": 0.30, "With DEV": 0.905},
                           "perCounts": {"With BI": [12, 4], "With DEV": [300, 271]}}));
    assert_eq!(Watcher::tune_drift_warning(&noisy, Some(&prior)), "");
    let real = obj(json!({"date": "2026-08-02", "balanced": 0.70, "firmCov": 0.85,
                          "per": {"With BI": 0.80, "With DEV": 0.50},
                          "perCounts": {"With BI": [12, 9], "With DEV": [300, 150]}}));
    let warning = Watcher::tune_drift_warning(&real, Some(&prior));
    assert!(
        warning.contains("balanced accuracy") && warning.contains("With DEV") && !warning.contains("With BI"),
        "{warning}"
    );
    assert_eq!(
        warning,
        "balanced accuracy dropped 20% (90% -> 70%); With DEV accuracy dropped 40% (90% -> 50%, n=300)"
    );
    assert_eq!(Watcher::tune_drift_warning(&real, None), "");
}

#[test]
fn suggestions_explain_themselves() {
    let tuning = Tuning::default();
    let mut lr = Learner::new(Model::default(), vec![]);
    for _ in 0..20 {
        lr.learn(
            &tuning,
            "With DEV",
            &tokenize("automation script deploy"),
            "",
            &[],
            "Sam",
            1.0,
            Some(&["automation".into()]),
            None,
        );
        lr.learn(
            &tuning,
            "With DATA",
            &tokenize("report totals export"),
            "",
            &[],
            "Sam",
            1.0,
            Some(&["report".into()]),
            None,
        );
    }
    let p =
        lr.predict_ex(&tuning, &tokenize("automation script needs deploying"), "", "Sam", Some(&["automation".into()]));
    assert_eq!(p.dest.as_deref(), Some("With DEV"));
    assert!(!p.why.is_empty() && p.why.iter().all(|w| "automation script needs deploying".contains(w.as_str())));

    let dir = tempfile::tempdir().unwrap();
    let w = watcher(dir.path(), json!({"resolvedLabel": "Resolved"}), None, &FakeJira::new());
    let label = w.short_label(&obj(p.to_value()));
    assert!(label.contains("because:"), "{label}");

    let mut cold = Learner::new(Model::default(), vec![]);
    let p_cold = cold.predict_ex(&tuning, &tokenize("anything at all"), "", "", None);
    assert!(p_cold.dest.is_none() && p_cold.why.is_empty());
    let mut shown = obj(p_cold.to_value());
    shown.insert("dest".into(), json!("Resolved"));
    assert_eq!(w.short_label(&shown), "resolve in HD · 0%");
}

#[test]
fn reporter_history_is_credited_only_when_it_points_the_same_way() {
    let tuning = Tuning::default();
    let mut lr = Learner::new(Model::default(), vec![]);
    let words = tokenize("please help with this");
    for _ in 0..20 {
        lr.learn(&tuning, "With DEV", &words, "", &[], "Sam", 1.0, Some(&["please".into()]), None);
        lr.learn(&tuning, "With DATA", &words, "", &[], "Alex", 1.0, Some(&["please".into()]), None);
    }
    let ex = Some(&["please".to_owned()][..]);
    let p = lr.predict_ex(&tuning, &words, "", "Alex", ex);
    assert!(p.dest.as_deref() == Some("With DATA") && p.why.contains(&"usually raised by Alex".to_owned()), "{p:?}");
    let p = lr.predict_ex(&tuning, &words, "", "Sam", ex);
    assert!(!p.why.iter().any(|w| w.contains("usually raised by Sam")), "{p:?}");
    let p = lr.predict_ex(&tuning, &words, "", "Someone New", ex);
    assert!(!p.why.iter().any(|w| w.contains("usually raised by")));
}

fn routed_desk(fake: &FakeJira, prefix: &str, n: usize) {
    let issues: Vec<Value> =
        (0..n).map(|i| json!({"key": format!("{prefix}-{i}"), "fields": {"status": {"name": "Done"}}})).collect();
    fake.with(|j| {
        for i in 0..n {
            j.changelogs.insert(
                format!("{prefix}-{i}"),
                vec![status_change(&format!("{}{i}", prefix.to_lowercase()), "Passed to Networks")],
            );
        }
    });
    fake.on_search(move |_| issues.clone());
}

#[test]
fn an_interrupted_backfill_keeps_what_it_learned() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    routed_desk(&fake, "BF", 20);
    let mut w =
        watcher(dir.path(), json!({"triageStatuses": ["Passed to Networks"], "resolvedStatuses": []}), None, &fake);
    fake.with(|j| j.stop_after = Some((8, w.stop.clone())));
    w.backfill().unwrap();
    let saved = read_lines(&dir.path().join("triage_history.jsonl"));
    assert!(!saved.is_empty() && saved.len() < 20, "{} of 20", saved.len());
    let seen = w.state["seenTransitions"].as_array().unwrap().len();
    assert!(seen > 0 && seen < 20);
    assert!(!crate::pyfmt::truthy(w.state.get("backfillDone")));
    assert_eq!(w.state["backfillCount"], json!(saved.len()));
    let on_disk = read_json(&dir.path().join("triage_state.json"));
    assert_eq!(on_disk["backfillCount"], json!(saved.len()), "checkpoint persisted");
    assert_eq!(saved[0]["featureOrigin"], json!("outcome"));
    assert_eq!(saved[0]["key"], json!("BF-0"));
}

#[test]
fn one_unusable_issue_does_not_cost_the_pass() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    routed_desk(&fake, "PZ", 10);
    fake.with(|j| j.poison.push("PZ-4".into()));
    let mut w =
        watcher(dir.path(), json!({"triageStatuses": ["Passed to Networks"], "resolvedStatuses": []}), None, &fake);
    w.backfill().unwrap();
    assert_eq!(w.state["seenTransitions"].as_array().unwrap().len(), 9, "every other issue is learned");
    // Deviation: jo-jira counts a malformed changelog as an incomplete read,
    // so the pass is retried rather than certified complete.
    assert_eq!(w.state["backfillDone"], json!(false));
    assert_eq!(w.state["backfillCount"], json!(9));

    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    routed_desk(&fake, "PZ", 10);
    fake.with(|j| j.poison.push("PZ-7".into()));
    let mut w =
        watcher(dir.path(), json!({"triageStatuses": ["Passed to Networks"], "resolvedStatuses": []}), None, &fake);
    w.resolve_and_learn().unwrap();
    assert_eq!(w.state["seenTransitions"].as_array().unwrap().len(), 9);
    assert_eq!(w.state["pollIncomplete"], json!(true), "the bad read keeps the checkpoint");
}

#[test]
fn predict_intake_suggests_for_every_queued_ticket() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    let issues: Vec<Value> = (0..10)
        .map(|i| json!({"key": format!("PI-{i}"), "fields": {"status": {"name": "New"}, "summary": format!("ticket {i}")}}))
        .collect();
    fake.on_search(move |_| issues.clone());
    let mut w = watcher(dir.path(), json!({}), None, &fake);
    w.predict_intake().unwrap();
    let predicted = w.state["predicted"].as_object().unwrap();
    assert_eq!(predicted.len(), 10);
    let p = &predicted["PI-3"];
    assert_eq!(p["dest"], Value::Null, "cold start: no destination yet");
    assert_eq!(p["intakeFeatures"]["summary"], json!("ticket 3"));
    let keys: Vec<&String> = p.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "dest",
            "conf",
            "plan",
            "second",
            "secondConf",
            "review",
            "source",
            "why",
            "requestType",
            "intakeFeatures",
            "summary",
            "at"
        ]
    );
    let log = read_lines(&dir.path().join("triage_log.jsonl"));
    assert_eq!(log.len(), 10);
    assert_eq!(log[0]["event"], json!("would_have_done"));
    assert!(log[0].get("ts").is_some());
    // Cold-start predictions have no destination, so nothing is published.
    assert_eq!(read_json(&dir.path().join("triage_suggestions.json")), json!({}));
    assert_eq!(read_json(&dir.path().join("triage_state.json"))["predicted"].as_object().unwrap().len(), 10);
    // A second cycle does not re-predict.
    w.predict_intake().unwrap();
    assert_eq!(read_lines(&dir.path().join("triage_log.jsonl")).len(), 10);
}

#[test]
fn discovery_is_not_repeated_once_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    let mut w4 = watcher(
        dir.path(),
        json!({"resolvedStatuses": ["Done"], "triageStatuses": []}),
        Some(json!({"workflowDiscovered": true})),
        &fake,
    );
    let before = fake.calls();
    assert!(w4.ensure_workflow_known().unwrap());
    assert_eq!(fake.calls(), before);
}

fn rec(summary: &str, dest: &str) -> Value {
    json!({"summary": summary, "desc": "", "labels": [], "components": [], "structured": [], "reporter": "r",
           "dest": dest, "cloneProjects": [], "requestType": ""})
}

#[test]
fn rebuild_keeps_outcome_wording_and_survives_bad_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = watcher(
        dir.path(),
        json!({"resolvedStatuses": ["Done"], "triageStatuses": ["Passed to Networks"]}),
        None,
        &FakeJira::new(),
    );
    let label = w.labels.resolved_label.clone();
    w.rebuild_model(&[json!({"summary": "a", "desc": "b", "labels": [], "components": [], "structured": [],
                              "reporter": "r", "dest": label, "cloneProjects": [], "requestType": ""})]);
    assert_eq!(w.learner.plan_for(&label), "resolve in HD — no handoff");

    let good = json!({"summary": "printer broken", "desc": "it broke", "labels": ["hw"], "components": ["Printers"],
                      "structured": [], "reporter": "Sam", "dest": "With DEV", "cloneProjects": [], "requestType": "Get IT help"});
    let with = |k: &str, v: Value| {
        let mut r = good.clone();
        r[k] = v;
        r
    };
    let mixed = vec![
        good.clone(),
        json!({}),
        with("summary", Value::Null),
        with("labels", Value::Null),
        with("dest", Value::Null),
        good.clone(),
    ];
    w.rebuild_model(&mixed);
    assert!(w.state["model"]["priors"]["With DEV"].as_f64().unwrap() > 0.0);
    let log = fs::read_to_string(dir.path().join("triage_learner.log")).unwrap();
    assert!(log.contains("model rebuild skipped 2 unusable record(s) of 6"));
}

#[test]
fn damaged_model_sections_still_load_and_learn() {
    let tuning = Tuning::default();
    for broken in [
        json!(null),
        json!("corrupt"),
        json!({"priors": null}),
        json!({"tokens": null}),
        json!({"exemplars": null}),
        json!({"scale": null}),
        json!({"scale": 0}),
        json!({"scale": -1}),
    ] {
        let mut lr = Learner::new(Model::from_value(&broken), vec![]);
        lr.learn(&tuning, "D", &["a".into(), "b".into(), "c".into()], "rt", &[], "Sam", 1.0, Some(&["a".into()]), None);
        lr.predict_ex(&tuning, &["a".into()], "rt", "Sam", Some(&["a".into()]));
        assert!(lr.model.scale > 0.0);
    }
    // And through the watcher's own start-up path.
    let dir = tempfile::tempdir().unwrap();
    let w = watcher(dir.path(), json!({}), Some(json!({"model": "corrupt", "routeSafety": [1]})), &FakeJira::new());
    assert!(w.state["model"].is_object() && w.state["routeSafety"].is_object());
}

fn write_history(dir: &std::path::Path, n: usize) {
    let mut body = String::new();
    for i in 0..n {
        body.push_str(&rec(&format!("t{i}"), "D").to_string());
        body.push('\n');
    }
    fs::write(dir.join("triage_history.jsonl"), body).unwrap();
}

#[test]
fn the_nightly_search_is_windowed() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = watcher(dir.path(), json!({}), None, &FakeJira::new());
    write_history(dir.path(), TUNE_WINDOW + 2500);
    let searched = Arc::new(Mutex::new(0));
    let seen = searched.clone();
    w.tuner = Box::new(move |recs, _, _| {
        *seen.lock().unwrap() = recs.len();
        None
    });
    w.maybe_daily_tune().unwrap();
    assert_eq!(*searched.lock().unwrap(), TUNE_WINDOW);
    assert_eq!(w.state["lastTuneDate"], json!(Local::now().format("%Y-%m-%d").to_string()));
}

#[test]
fn a_failed_tune_is_left_to_the_next_day() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = watcher(dir.path(), json!({}), None, &FakeJira::new());
    write_history(dir.path(), 400);
    let attempts = Arc::new(Mutex::new(0));
    let counter = attempts.clone();
    w.tuner = Box::new(move |_, _, _| {
        *counter.lock().unwrap() += 1;
        panic!("the search itself is broken");
    });
    w.maybe_daily_tune().unwrap();
    assert_eq!(w.state["lastTuneDate"], json!(Local::now().format("%Y-%m-%d").to_string()));
    w.maybe_daily_tune().unwrap();
    assert_eq!(*attempts.lock().unwrap(), 1, "not retried the same day");
    let log = fs::read_to_string(dir.path().join("triage_learner.log")).unwrap();
    assert!(log.contains("failed:\nthe search itself is broken"));
    // Fewer than 300 decisions: stamped, saved, nothing tuned.
    let dir = tempfile::tempdir().unwrap();
    let mut w = watcher(dir.path(), json!({}), None, &FakeJira::new());
    write_history(dir.path(), 10);
    w.tuner = Box::new(|_, _, _| panic!("must not run"));
    w.maybe_daily_tune().unwrap();
    assert!(read_json(&dir.path().join("triage_state.json"))["lastTuneDate"].as_str().is_some_and(|d| !d.is_empty()));
}

#[test]
fn a_real_tune_adopts_and_records_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let prior: Vec<Value> = (0..60).map(|i| json!({"date": format!("old-{i}")})).collect();
    let mut w = watcher(
        dir.path(),
        json!({"triageStatuses": ["With DEV"], "resolvedStatuses": ["Done"]}),
        Some(json!({"tuneHistory": prior})),
        &FakeJira::new(),
    );
    let mut body = String::new();
    for i in 0..320 {
        let (summary, dest) =
            if i % 3 == 0 { ("printer jam tray", "With DEV") } else { ("password reset locked", "Resolved in HD") };
        let when = (Local::now() - Duration::minutes(320 - i)).format("%Y-%m-%d %H:%M").to_string();
        let mut r = rec(&format!("{summary} {}", i % 7), dest);
        r["when"] = json!(when);
        r["key"] = json!(format!("HD-{i}"));
        body.push_str(&r.to_string());
        body.push('\n');
    }
    fs::write(dir.path().join("triage_history.jsonl"), body).unwrap();
    w.maybe_daily_tune().unwrap();
    let state = read_json(&dir.path().join("triage_state.json"));
    let hist = state["tuneHistory"].as_array().unwrap();
    assert_eq!(hist.len(), 60, "capped at 60");
    assert_eq!(hist[0]["date"], json!("old-1"));
    let keys: Vec<&String> = hist[59].as_object().unwrap().keys().collect();
    assert_eq!(keys[..4], ["date", "acc", "balanced", "worst"]);
    assert!(keys.contains(&&"incumbentGateScore".to_owned()) && keys.last() == Some(&&"cfg".to_owned()));
    assert!(state["tunedConfig"].is_object() && state["routeSafety"]["With DEV"].is_object());
    assert!(state["model"]["priors"]["With DEV"].as_f64().unwrap() > 0.0, "re-fitted on the whole history");
    let log = fs::read_to_string(dir.path().join("triage_learner.log")).unwrap();
    assert!(log.contains("daily self-tune") && log.contains("cfg={'routeBalanceWeight': "), "{log}");
    let report = fs::read_to_string(dir.path().join("TRIAGE_REPORT.md")).unwrap();
    assert!(report.contains("## Self-tuning (runs daily on accumulated history)"));
}

#[test]
fn a_truncated_log_line_does_not_blank_the_report() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = watcher(dir.path(), json!({"resolvedLabel": "Resolved"}), None, &FakeJira::new());
    fs::write(
        dir.path().join("triage_log.jsonl"),
        format!(
            "{}\n{{\"event\": \"routed\", \"key\": \"SD-2\", \"dest\": \"D\", \"by\": \"Sam\"",
            json!({"event": "routed", "key": "SD-1", "dest": "D", "by": "Sam", "when": "2026-01-01 00:00", "clones": [], "predicted": null})
        ),
    )
    .unwrap();
    w.write_report().unwrap();
    let report = fs::read_to_string(dir.path().join("TRIAGE_REPORT.md")).unwrap();
    assert!(report.contains("- `SD-1` **D** by Sam (clones: none)  \n  "), "{report}");
}

#[test]
fn refusals_surface_in_report_and_status() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    let mut w = watcher(dir.path(), json!({}), None, &fake);
    fake.with(|j| j.status_for.insert("/rest/api/3/myself".into(), 401));
    assert!(w.jira.get("/rest/api/3/myself", &[]).is_none());
    w.write_report().unwrap();
    let report = fs::read_to_string(dir.path().join("TRIAGE_REPORT.md")).unwrap();
    assert!(report.contains("## ⚠ Jira has been refusing requests (401) on recent requests"), "{report}");
    let status = read_json(&dir.path().join("triage_status.json"));
    assert_eq!(status["healthy"], json!(false));
    assert_eq!(status["refusals"], json!([401]));
    assert_eq!(status["healthMessage"], json!("Jira is refusing requests"));
    for key in ["trainedOnDecisions", "backfillComplete", "lastTuneDate", "outcomes"] {
        assert!(status["provenance"].get(key).is_some(), "{key}");
    }
    // A successful search clears the refusal.
    w.jira.search("project = HD", &["summary"], Some(1));
    w.write_report().unwrap();
    assert!(!fs::read_to_string(dir.path().join("TRIAGE_REPORT.md")).unwrap().contains("refusing requests"));
    assert_eq!(read_json(&dir.path().join("triage_status.json"))["healthy"], json!(true));

    // A populated confusion table doesn't clobber the prediction total.
    w.state.insert("stats".into(), json!({"predictions": 1313, "correct": 1178}));
    w.state.insert("confusion".into(), json!({"With DEV → With BI": 3, "With BI → With DEV": 5}));
    w.write_report().unwrap();
    let status = read_json(&dir.path().join("triage_status.json"));
    assert_eq!(status["accuracy"], json!({"predictions": 1313, "correct": 1178, "pct": 89.7}));
    let report = fs::read_to_string(dir.path().join("TRIAGE_REPORT.md")).unwrap();
    assert!(report.contains("## Prediction accuracy: 89% (1178/1313)"));
    assert!(report.contains(
        "## Mistakes learned from (said → team chose)\n\n- With BI → With DEV — 5×\n- With DEV → With BI — 3×"
    ));
}

#[test]
fn idle_cycles_do_not_rewrite_the_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = watcher(dir.path(), json!({}), None, &FakeJira::new());
    let state = dir.path().join("triage_state.json");
    let written = |w: &mut Watcher, force: bool| {
        let _ = fs::remove_file(&state);
        w.save_state(force).unwrap();
        state.exists()
    };
    assert!(written(&mut w, false), "never saved before");
    for _ in 0..5 {
        assert!(!written(&mut w, false), "idle");
    }
    let tuning = w.tuning.clone();
    w.learner.learn(
        &tuning,
        "D",
        &["a".into(), "b".into(), "c".into()],
        "rt",
        &[],
        "Sam",
        1.0,
        Some(&["a".into()]),
        None,
    );
    assert!(written(&mut w, false), "learning is persisted straight away");
    assert!(written(&mut w, true), "a forced save always writes");
    w.last_state_save = jo_core::lifecycle::now_epoch() - crate::watcher::IDLE_SAVE_SECS - 1.0;
    assert!(written(&mut w, false), "bookkeeping flushed once the idle interval passes");
    assert!(dir.path().join("triage_backups").join(format!("{}.zip", Local::now().format("%Y-%m-%d"))).exists());
}

#[test]
fn route_safety_forces_review_without_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let mut rs =
        watcher(dir.path(), json!({"triageStatuses": ["With DEV", "With BI", "With DATA"]}), None, &FakeJira::new());
    let now = Local::now().naive_local();
    let recent = |days: i64| (now - Duration::days(days)).format("%Y-%m-%d %H:%M").to_string();
    let mut recs = Vec::new();
    for (days, dest) in [(1, "With DEV"), (200, "With BI"), (1, "With DATA")] {
        for _ in 0..5 {
            recs.push(json!({"when": recent(days), "dest": dest}));
        }
    }
    let firm: Counts = [("With DEV", [100, 100]), ("With BI", [20, 20]), ("With DATA", [20, 10])]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
    let policy = rs.derive_route_safety(&recs, &firm);
    assert_eq!(policy["With DEV"]["forceReview"], json!(false), "{}", policy["With DEV"]);
    assert_eq!(policy["With BI"]["forceReview"], json!(true));
    assert!(policy["With BI"]["reason"].as_str().unwrap().contains("dormant"));
    assert_eq!(policy["With DATA"]["forceReview"], json!(true));
    assert!(policy["With DATA"]["reason"].as_str().unwrap().contains("recent confident precision is 50% (10/20)"));
    assert_eq!(policy["Resolved in HD"]["forceReview"], json!(true));
    assert_eq!(policy["Resolved in HD"]["firmPrecision"], Value::Null);
    let keys: Vec<&String> = policy["With DEV"].as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "forceReview",
            "reason",
            "recentActual",
            "firmPredictions",
            "firmCorrect",
            "firmPrecision",
            "precisionLowerBound"
        ]
    );

    // A tightened policy applies to suggestions already on screen.
    rs.learner.route_safety = obj(json!({"With DATA": policy["With DATA"]}));
    let shown = obj(json!({"K-1": {"dest": "With DATA", "conf": 91, "second": null, "secondConf": 0, "review": false,
                                   "source": "model", "note": "move to With DATA", "at": recent(0)}}));
    rs.write_suggestions(&shown).unwrap();
    let published = read_json(&dir.path().join("triage_suggestions.json"));
    assert_eq!(published["K-1"]["review"], json!(true));
    assert_eq!(shown["K-1"]["review"], json!(false), "stored prediction untouched");
    assert_eq!(published["K-1"]["label"], json!("With DATA · 91%"));
}
