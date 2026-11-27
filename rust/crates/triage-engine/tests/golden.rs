//! Differential tests against the shipping Python learner.
//!
//! Fixtures come from `rust/tools/golden.py`, which runs the Python model
//! over a seeded synthetic history. Predictions, the learned model and tuning
//! decisions must match exactly.

use std::fs;
use std::path::PathBuf;

use serde_json::{Map, Value, json};
use triage_engine::learner::Learner;
use triage_engine::model::Model;
use triage_engine::num::py_round;
use triage_engine::replay::{metrics_json, replay, self_tune, tune_score};
use triage_engine::text::{TicketText, features_for, tokenize};
use triage_engine::{Record, Tuning, fit_example};

fn golden(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name);
    serde_json::from_str(&fs::read_to_string(path).expect("fixture present")).expect("fixture is JSON")
}

fn records() -> Vec<Record> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/synthetic_history.jsonl");
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| Record::from_value(&serde_json::from_str(l).unwrap()).expect("well-formed record"))
        .collect()
}

const HANDOFFS: [&str; 3] = ["With DEV", "With BI", "With DATA"];

fn handoffs() -> Vec<String> {
    HANDOFFS.iter().map(|s| (*s).to_owned()).collect()
}

fn tuning_with(overrides: &Value) -> Tuning {
    let mut t = Tuning::default();
    t.update_from(overrides.as_object().expect("overrides object"));
    t
}

#[test]
fn text_pipeline_matches_python() {
    let g = golden("text.json");
    for case in g["tokenize"].as_array().unwrap() {
        let text = case[0].as_str().unwrap();
        let want: Vec<String> = serde_json::from_value(case[1].clone()).unwrap();
        assert_eq!(tokenize(text), want, "tokenize({text:?})");
    }
    let recs = records();
    for (i, case) in g["features"].as_array().unwrap().iter().enumerate() {
        let want: Vec<String> = serde_json::from_value(case[1].clone()).unwrap();
        let r = &recs[i];
        assert_eq!(features_for(&r.text, &r.request_type, 6.0), want, "features for record {i}");
    }
    for case in g["round"].as_array().unwrap() {
        let (x, n, want) = (case[0].as_f64().unwrap(), case[1].as_u64().unwrap() as usize, case[2].as_f64().unwrap());
        assert_eq!(py_round(x, n), want, "round({x}, {n})");
    }
}

fn check_sequential(name: &str) {
    let g = golden(&format!("sequential_{name}.json"));
    let tuning = tuning_with(&g["overrides"]);
    let mut learner = Learner::new(Model::default(), handoffs());
    learner.plans.insert("Resolved in SD".into(), "resolve in SD — no handoff".into());
    learner.plans.insert("Moved to PPS".into(), "move the ticket to the PPS project".into());
    let mut safety = Map::new();
    safety.insert("With BI".into(), json!({"forceReview": true, "reason": "only 4 confident predictions"}));
    learner.route_safety = safety;
    let want = g["predictions"].as_array().unwrap();
    for (i, r) in records().iter().enumerate() {
        let got = fit_example(&mut learner, &tuning, r).to_value();
        assert_eq!(got, want[i], "{name}: prediction {i} ({})", r.key);
    }
    assert_eq!(learner.model, Model::from_value(&g["model"]), "{name}: learned model");
    assert_eq!(learner.mutations, g["mutations"].as_u64().unwrap());
}

#[test]
fn sequential_default_matches_python() {
    check_sequential("default");
}

#[test]
fn sequential_aggressive_matches_python() {
    check_sequential("aggressive");
}

#[test]
fn sequential_plain_matches_python() {
    check_sequential("plain");
}

/// Exact except sums over Python sets, whose iteration order is randomised
/// per process (Python itself varies in the last bit there).
fn assert_metrics(got: &Value, want: &Value, label: &str) {
    const SET_ORDERED: [&str; 2] = ["macroPrecision", "macroF1"];
    for (key, w) in want.as_object().unwrap() {
        let g = &got[key];
        if key == "samples" {
            let n = w.as_array().unwrap().len();
            assert_eq!(&g.as_array().unwrap()[..n], w.as_array().unwrap().as_slice(), "{label}.samples");
        } else if SET_ORDERED.contains(&key.as_str()) {
            assert!((g.as_f64().unwrap() - w.as_f64().unwrap()).abs() < 1e-12, "{label}.{key}: {g} vs {w}");
        } else {
            assert_eq!(g, w, "{label}.{key}");
        }
    }
}

#[test]
fn replay_metrics_match_python() {
    let g = golden("replay.json");
    let recs = records();
    let base = Tuning::default();
    let full = replay(&recs, &base, &handoffs(), None, None, false);
    assert_metrics(&metrics_json(&full), &g["full"], "full");
    let window = replay(&recs, &base.with(&[("routeBalanceWeight", 0.5)]), &handoffs(), Some(600), Some(1000), false);
    assert_metrics(&metrics_json(&window), &g["window"], "window");
    let scores = g["scores"].as_array().unwrap();
    assert_eq!(json!(tune_score(&full).to_vec()), scores[0]);
    assert_eq!(json!(tune_score(&window).to_vec()), scores[1]);
}

#[test]
fn self_tune_matches_python() {
    let g = golden("self_tune.json");
    let best = self_tune(&records(), &Tuning::default(), &handoffs()).expect("tune runs");
    let num = |v: &Value| v.as_f64().unwrap();
    let cfg = best.cfg_json();
    for (k, v) in g["cfg"].as_object().unwrap() {
        assert_eq!(num(&cfg[k]), num(v), "cfg.{k}");
    }
    assert_eq!(json!(best.score.to_vec()), g["score"]);
    assert_eq!(best.split as u64, g["split"].as_u64().unwrap());
    assert_eq!(u64::from(best.evals), g["evals"].as_u64().unwrap());
    assert_eq!(best.validation_windows as u64, g["validationWindows"].as_u64().unwrap());
    assert_eq!(best.challenger_rejected, g["challengerRejected"].as_bool().unwrap());
    assert_eq!(best.gate_reason, g["gateReason"].as_str().unwrap());
    assert_eq!(json!(best.incumbent_gate_score.to_vec()), g["incumbentGateScore"]);
    assert_eq!(json!(best.challenger_gate_score.to_vec()), g["challengerGateScore"]);
    assert_eq!(best.gate.n, g["gateN"].as_u64().unwrap());
    assert_eq!(best.train.n, g["trainN"].as_u64().unwrap());
    assert_eq!(best.holdout.n, g["holdoutN"].as_u64().unwrap());
    assert_eq!(best.final_holdout.n, g["finalHoldoutN"].as_u64().unwrap());
    assert_eq!(best.holdout.acc, num(&g["acc"]));
    assert_eq!(best.holdout.balanced, num(&g["balanced"]));
    assert!((best.holdout.macro_f1 - num(&g["macroF1"])).abs() < 1e-12);
    assert_eq!(best.final_holdout.acc, num(&g["finalAcc"]));
    assert_eq!(best.gate.acc, num(&g["gateAcc"]));
}

#[test]
fn feature_text_struct_matches_record() {
    let r = &records()[0];
    assert!(matches!(r.text, TicketText { .. }));
}

/// Local-only: compare against a copy of a real data folder replayed by
/// `golden.py --real` (never committed). Run with `cargo test -- --ignored`.
#[test]
#[ignore]
fn real_history_matches_python() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/golden-local");
    let g: Value = serde_json::from_str(&fs::read_to_string(dir.join("real.json")).unwrap()).unwrap();
    let folder = std::env::var("JO_REAL_DATA").expect("JO_REAL_DATA points at the copied data folder");
    let folder = PathBuf::from(folder);
    let mut recs: Vec<Record> = fs::read_to_string(folder.join("triage_history.jsonl"))
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| Record::from_value(&v))
        .collect();
    recs.sort_by(|a, b| a.when.cmp(&b.when));
    let dests: Vec<String> = serde_json::from_value(g["dests"].clone()).unwrap();
    let tuning = tuning_with(&g["tuning"]);
    let mut learner = Learner::new(Model::default(), dests.clone());
    let want = g["predictions"].as_array().unwrap();
    assert_eq!(recs.len(), want.len());
    let mut mismatches = 0;
    for (i, r) in recs.iter().enumerate() {
        let got = fit_example(&mut learner, &tuning, r).to_value();
        if got != want[i] {
            mismatches += 1;
            if mismatches <= 5 {
                eprintln!("record {i}: rust {got}\n           python {}", want[i]);
            }
        }
    }
    let state: Value = serde_json::from_str(&fs::read_to_string(folder.join("triage_state.json")).unwrap()).unwrap();
    let mut live = Learner::new(Model::from_value(&state["model"]), dests);
    if let Some(Value::Object(s)) = state.get("routeSafety") {
        live.route_safety = s.clone();
    }
    let live_want = g["livePredictions"].as_array().unwrap();
    for (i, r) in recs[recs.len() - live_want.len()..].iter().enumerate() {
        let tokens = features_for(&r.text, &r.request_type, tuning.get("summaryWeight"));
        let got = live.predict_ex(&tuning, &tokens, &r.request_type, &r.reporter, Some(&tokenize(&r.text.summary)));
        if got.to_value() != live_want[i] {
            mismatches += 1;
            if mismatches <= 10 {
                eprintln!("live {i}: rust {}\n        python {}", got.to_value(), live_want[i]);
            }
        }
    }
    assert_eq!(mismatches, 0, "{} replayed + {} live predictions", want.len(), live_want.len());
}
