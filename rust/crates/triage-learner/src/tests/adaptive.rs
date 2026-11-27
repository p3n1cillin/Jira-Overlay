//! `tests/test_adaptive_routes.py`: portable route ranking and the champion
//! gate's accuracy safeguards.

use std::collections::HashMap;

use serde_json::json;
use triage_engine::learner::route_balance_factors;
use triage_engine::model::RouteBalance;
use triage_engine::replay::{Metrics, accept_challenger};
use triage_engine::{Learner, Model};

use super::*;

fn balance(observations: f64, actual: &[(&str, f64)], predicted: &[(&str, f64)]) -> RouteBalance {
    let table = |pairs: &[(&str, f64)]| pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect();
    RouteBalance { actual: table(actual), predicted: table(predicted), observations }
}

#[test]
fn over_and_under_predicted_routes_are_corrected_within_bounds() {
    let b = balance(100.0, &[("Équipe α", 20.0), ("Queue Ω", 80.0)], &[("Équipe α", 80.0), ("Queue Ω", 20.0)]);
    let factors = route_balance_factors(&b, 1.0);
    assert_eq!(factors, HashMap::from([("Équipe α".to_owned(), 0.5), ("Queue Ω".to_owned(), 2.0)]));
    assert!(route_balance_factors(&RouteBalance { observations: 10.0, ..b.clone() }, 1.0).is_empty());
    assert!(route_balance_factors(&b, 0.0).is_empty());
    assert!(route_balance_factors(&balance(100.0, &[("rare", 2.0)], &[("rare", 8.0)]), 1.0).is_empty());
    let tuning = triage_engine::Tuning::default();
    for malformed in [
        json!(null),
        json!({"observations": null}),
        json!({"actual": null, "predicted": "bad"}),
        json!({"actual": {"route": "bad"}, "observations": -1}),
    ] {
        let mut lr = Learner::new(Model::from_value(&json!({"routeBalance": malformed})), vec![]);
        lr.learn(&tuning, "Only observed route", &["word".into()], "Portal type", &[], "", 1.0, None, None);
        assert!(route_balance_factors(&lr.model.route_balance, 1.0).is_empty());
    }
}

#[test]
fn adaptive_ranking_keeps_rebuild_replay_equivalence() {
    let rows: Vec<Value> = (0..120)
        .map(|i| {
            json!({"summary": format!("query {}", i % 4), "desc": "support", "labels": [], "components": [],
                   "structured": [], "reporter": "person", "requestType": "Besoin général",
                   "dest": if i % 3 != 0 { "Queue Ω" } else { "Équipe α" }, "cloneProjects": []})
        })
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let mut w = watcher(dir.path(), json!({"triageStatuses": ["Queue Ω"]}), None, &FakeJira::new());
    w.tuning.set("routeBalanceWeight", 1.0);
    w.tuning.set("mistakeWeight", 4.0);
    let recs: Vec<_> = rows.iter().map(|r| triage_engine::Record::from_value(r).unwrap()).collect();
    let replay = triage_engine::replay(&recs, &w.tuning, &w.dests, Some(0), None, true);
    w.rebuild_model(&rows);
    assert_eq!(&w.learner.model, replay.model.as_ref().unwrap());
    assert!(w.learner.model.route_balance.observations > 50.0);

    let rename = |d: &str| if d == "Queue Ω" { "Custom department 7" } else { "Front office" }.to_owned();
    let renamed: Vec<_> = recs.iter().map(|r| triage_engine::Record { dest: rename(&r.dest), ..r.clone() }).collect();
    let again = triage_engine::replay(&renamed, &w.tuning, &[rename("Queue Ω")], Some(0), None, false);
    let mapped: Vec<String> = replay.samples.iter().map(|s| rename(&s.predicted)).collect();
    let got: Vec<String> = again.samples.iter().map(|s| s.predicted.clone()).collect();
    assert_eq!(mapped, got, "renaming departments does not change decisions");
    assert_eq!(replay.acc, again.acc);

    let before = w.learner.model.route_balance.clone();
    let tuning = w.tuning.clone();
    w.learner.predict_ex(&tuning, &["query".into()], "Besoin général", "", None);
    assert_eq!(w.learner.model.route_balance, before, "predicting never mutates balance counts");
}

#[test]
fn a_macro_win_cannot_buy_an_accuracy_regression() {
    let metrics = |rate: f64, macro_: f64, firm: f64| Metrics {
        acc: rate,
        macro_f1: macro_,
        balanced: macro_,
        worst: macro_,
        macro_precision: macro_,
        worst_f1: macro_,
        firm_balanced: firm,
        firm_worst: firm,
        firm_acc: firm,
        firm_cov: 0.7,
        firm_macro_precision: firm,
        firm_worst_precision: firm,
        ..Metrics::default()
    };
    let baseline = metrics(0.9, 0.9, 0.98);
    assert!(!accept_challenger(&baseline, &metrics(0.88, 0.92, 0.98)));
    assert!(!accept_challenger(&baseline, &metrics(0.9, 0.92, 0.95)));
    assert!(accept_challenger(&baseline, &metrics(0.91, 0.92, 0.98)));
}
