//! `tests/test_learning_progress.py`: refit/replay equivalence, live log
//! folding, report/status consistency, review provenance.

use std::collections::BTreeSet;
use std::fs;

use indexmap::IndexMap;
use serde_json::json;

use super::*;
use crate::metrics::MetricsCache;
use crate::support::rolling_metrics;

fn records() -> Vec<Value> {
    (0..90)
        .map(|i| {
            json!({"key": i.to_string(), "summary": format!("case query {}", i % 3), "desc": "request support",
                   "labels": [], "components": [], "reporter": "r", "requestType": "General enquiry", "structured": [],
                   "cloneProjects": [], "dest": if i % 3 != 0 { "Tier Z" } else { "Front desk" }})
        })
        .collect()
}

#[test]
fn nightly_rebuild_reproduces_the_weighted_replay_model() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = watcher(dir.path(), json!({"triageStatuses": ["Tier Z"]}), None, &FakeJira::new());
    w.learner.route_safety = obj(json!({"Tier Z": {"forceReview": true}}));
    w.tuning.set("mistakeWeight", 4.0);
    let rows = records();
    let recs: Vec<_> = rows.iter().map(|r| triage_engine::Record::from_value(r).unwrap()).collect();
    let replay = triage_engine::replay(&recs, &w.tuning, &w.dests, Some(0), None, true);
    w.rebuild_model(&rows);
    assert_eq!(&w.learner.model, replay.model.as_ref().unwrap(), "rebuild == weighted replay");
    assert!(replay.samples.iter().any(|s| s.correct == 0), "the fixture includes mistakes");
    assert_eq!(w.learner.route_safety, obj(json!({"Tier Z": {"forceReview": true}})), "route policy kept");
}

#[test]
fn coverage_reasons_follow_observable_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let mut w =
        watcher(dir.path(), json!({"triageStatuses": ["Tier Z"], "resolvedLabel": "IT"}), None, &FakeJira::new());
    for row in [
        json!({"event": "would_have_done", "key": "A", "prediction": null, "review": true}),
        json!({"event": "routed", "key": "A", "dest": "IT", "predicted": null}),
        json!({"event": "routed", "key": "A", "dest": "IT", "predicted": null}),
        json!({"event": "routed", "key": "B", "dest": "IT", "predicted": null}),
        json!({"event": "would_have_done", "key": "C", "prediction": "IT", "review": false}),
        json!({"event": "routed", "key": "C", "dest": "IT", "predicted": null}),
    ] {
        w.log(obj(row)).unwrap();
    }
    let canon = |d: &str| d.to_owned();
    let metrics = w.metrics.live(&canon).clone();
    let reasons: IndexMap<String, u64> = metrics.coverage_reasons.clone();
    let want: BTreeSet<(String, u64)> = [
        ("cold_start", 1),
        ("repeat_transition_without_prediction", 1),
        ("no_intake_observation", 1),
        ("prediction_context_not_retained", 1),
    ]
    .into_iter()
    .map(|(k, n)| (k.to_owned(), n))
    .collect();
    assert_eq!(reasons.into_iter().collect::<BTreeSet<_>>(), want);
    let now = chrono::Local::now().naive_local();
    assert_eq!(rolling_metrics(&metrics.recent_events, now, 30).routed, 4);
    assert_eq!(metrics.routed, 4);

    // Report and status expose the same recent counts and coverage.
    w.state.insert("stats".into(), json!({"predictions": 4, "correct": 3}));
    w.write_report().unwrap();
    let status = read_json(&dir.path().join("triage_status.json"));
    let report = fs::read_to_string(dir.path().join("TRIAGE_REPORT.md")).unwrap();
    assert_eq!(status["recentPerformance"]["30"]["routed"], json!(4));
    assert_eq!(status["accuracy"]["predictions"], json!(4));
    assert!(report.contains("repeat transition without prediction: 1"), "{report}");
    assert!(report.contains("intake prediction coverage 0.0% (0/4)"), "{report}");

    // Recent events and provenance survive rotation and restart.
    w.log_rotate_bytes = 1;
    w.log(obj(json!({"event": "routed", "key": "D", "dest": "IT", "predicted": null}))).unwrap();
    let after = MetricsCache::new(&dir.path().join("triage_log.jsonl")).live(&canon).clone();
    assert_eq!(rolling_metrics(&after.recent_events, now + chrono::Duration::seconds(5), 30).routed, 5);
    assert_eq!(after.routed, 5);
    assert_eq!(after.coverage_reasons["no_intake_observation"], 2);
}

#[test]
fn automated_review_is_pending_human_confirmation() {
    let case = obj(json!({"actual": "Équipe α", "predicted": "Tier Z", "summary": "printer access", "confidence": 55}));
    let mut cases = IndexMap::new();
    cases.insert("case".to_owned(), case.clone());
    let evidence = crate::cli::analysis_evidence(
        &[
            json!({"requestType": "Besoin général", "dest": "Équipe α"}),
            json!({"requestType": "Besoin général", "dest": "Tier Z"}),
        ],
        &cases,
    );
    let analysis = crate::cli::analyse_case(
        &case,
        &obj(json!({"requestType": "Besoin général", "featureOrigin": "intake"})),
        Some(&evidence),
    );
    assert_eq!(analysis["source"], json!("automated_log_analysis"));
    assert_eq!(analysis["requiresHumanReview"], json!(true));
    assert_eq!(analysis["observedFacts"]["actual"], json!("Équipe α"));
    assert_eq!(analysis["observedFacts"]["requestTypeOutcomes"], json!({"Équipe α": 1, "Tier Z": 1}));
    assert_eq!(analysis["observedFacts"]["matchingMistakes"], json!(1));
    assert_eq!(
        analysis["note"],
        json!(
            "The recorded suggestion was Tier Z; the observed destination was Équipe α. This request type has 2 \
               historical routing observations across 2 outcomes. This suggestion/destination pair occurs in 1 mistakes \
               in the selected review window. Compare the original intake features with both outcomes; these \
               retrospective counts do not establish the cause or a routing rule."
        )
    );
}

#[test]
fn offline_roles_come_from_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let paths = crate::files::Paths::new(dir.path());
    fs::write(&paths.config, r#"{"triageStatuses": ["Équipe α", "Tier Z"]}"#).unwrap();
    let want: BTreeSet<String> = ["Équipe α".to_owned(), "Tier Z".to_owned()].into();
    assert_eq!(crate::cli::handoff_dests(&paths, &records()), want);
    fs::write(&paths.config, r#"{"triageStatuses": []}"#).unwrap();
    assert!(crate::cli::handoff_dests(&paths, &[json!({"dest": "With Anything"})]).is_empty());
    fs::remove_file(&paths.config).unwrap();
    let evidence =
        [json!({"dest": "A", "cloneProjects": ["DEV"]}), json!({"dest": "B", "isHandoff": true}), json!({"dest": "C"})];
    assert_eq!(crate::cli::handoff_dests(&paths, &evidence), BTreeSet::from(["A".to_owned(), "B".to_owned()]));
}
