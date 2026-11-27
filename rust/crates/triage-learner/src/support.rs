//! Pure reporting and confidence helpers (Python `app/triage_support.py`).

use std::collections::BTreeSet;

use chrono::{Duration, NaiveDateTime};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use triage_engine::num::py_round;
use triage_engine::replay::Sample;

use crate::pyfmt::{prefix, py_str};

pub type Counts = IndexMap<String, [u64; 2]>;

/// `{"predictions", "correct", "pct"}` with `pct` rounded to one decimal.
pub fn accuracy_counts(total: u64, correct: u64) -> Value {
    json!({
        "predictions": total,
        "correct": correct,
        "pct": if total > 0 { Value::from(py_round(100.0 * correct as f64 / total as f64, 1)) } else { Value::Null },
    })
}

/// One-sided 95% lower bound; ten lucky predictions are not certainty.
pub fn wilson_lower(correct: u64, total: u64) -> f64 {
    wilson_lower_z(correct, total, 1.645)
}

pub fn wilson_lower_z(correct: u64, total: u64, z: f64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let n = total as f64;
    let p = correct as f64 / n;
    (p + z * z / (2.0 * n) - z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt()) / (1.0 + z * z / n)
}

/// `segment_name` for a JSON value (`str(request_type or "Unknown ...")`).
pub fn segment_name(request_type: Option<&Value>) -> String {
    match request_type {
        Some(v) if triage_engine::text::truthy(v) => py_str(v),
        _ => "Unknown request type".into(),
    }
}

/// Describe observed evidence, never assume why an intake ticket was missed.
pub fn coverage_reason(
    record: &Map<String, Value>,
    intake: Option<&Map<String, Value>>,
    prior_routing: bool,
) -> &'static str {
    if record.get("predicted").is_some_and(|p| !p.is_null()) {
        return "predicted";
    }
    if prior_routing {
        return "repeat_transition_without_prediction";
    }
    match intake {
        Some(i) if i.get("prediction").is_none_or(Value::is_null) => "cold_start",
        Some(_) => "prediction_context_not_retained",
        None => "no_intake_observation",
    }
}

/// `datetime.isoformat(sep=" ", timespec="seconds")`.
pub fn iso_seconds(t: NaiveDateTime) -> String {
    t.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn bump(table: &mut Counts, key: String, hit: u64) {
    let row = table.entry(key).or_insert([0, 0]);
    row[0] += 1;
    row[1] += hit;
}

fn counts_json(c: &Counts) -> Value {
    Value::Object(c.iter().map(|(k, v)| (k.clone(), json!(v))).collect())
}

/// `rolling_metrics`'s totals.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rolling {
    pub routed: u64,
    pub predicted: u64,
    pub correct: u64,
    pub by_actual: Counts,
    pub by_predicted: Counts,
    pub segments: Counts,
    pub bands: Counts,
    pub coverage_reasons: IndexMap<String, u64>,
    pub daily_coverage: Counts,
    pub coverage_pct: Option<f64>,
    pub window_days: i64,
}

impl Rolling {
    pub fn accuracy(&self) -> Value {
        accuracy_counts(self.predicted, self.correct)
    }

    pub fn to_value(&self) -> Value {
        json!({
            "routed": self.routed, "predicted": self.predicted, "correct": self.correct,
            "byActual": counts_json(&self.by_actual), "byPredicted": counts_json(&self.by_predicted),
            "segments": counts_json(&self.segments), "bands": counts_json(&self.bands),
            "coverageReasons": self.coverage_reasons, "dailyCoverage": counts_json(&self.daily_coverage),
            "accuracy": self.accuracy(), "coveragePct": self.coverage_pct, "windowDays": self.window_days,
        })
    }
}

fn ts_of(row: &Map<String, Value>) -> String {
    row.get("ts").map(py_str).unwrap_or_default()
}

/// Aggregate routing events by observation time, with known confidence only.
pub fn rolling_metrics(records: &[Map<String, Value>], now: NaiveDateTime, days: i64) -> Rolling {
    let cutoff = iso_seconds(now - Duration::days(days));
    let end = iso_seconds(now);
    let rows: Vec<&Map<String, Value>> = records
        .iter()
        .filter(|r| {
            let ts = ts_of(r);
            cutoff <= ts && ts <= end
        })
        .collect();
    let mut totals = Rolling { routed: rows.len() as u64, window_days: days, ..Rolling::default() };
    for row in &rows {
        let reason = row.get("coverageReason").map(py_str).unwrap_or_else(|| "unclassified".into());
        *totals.coverage_reasons.entry(reason).or_insert(0) += 1;
        let day = totals.daily_coverage.entry(prefix(&ts_of(row), 10)).or_insert([0, 0]);
        day[0] += 1;
        let Some(predicted) = row.get("predicted").filter(|p| !p.is_null()) else { continue };
        day[1] += 1;
        let hit = u64::from(crate::pyfmt::truthy(row.get("predictionCorrect")));
        totals.predicted += 1;
        totals.correct += hit;
        let band = match row.get("review") {
            None | Some(Value::Null) => "unknown",
            Some(v) if triage_engine::text::truthy(v) => "review",
            Some(_) => "confident",
        };
        let actual = row.get("dest").map(py_str).unwrap_or_else(|| "unknown".into());
        bump(&mut totals.by_actual, actual, hit);
        bump(&mut totals.by_predicted, py_str(predicted), hit);
        bump(&mut totals.segments, segment_name(row.get("requestType")), hit);
        bump(&mut totals.bands, band.into(), hit);
    }
    totals.coverage_pct = (!rows.is_empty()).then(|| py_round(100.0 * totals.predicted as f64 / rows.len() as f64, 1));
    totals
}

/// One baseline/recent comparison.
#[derive(Clone, Debug, PartialEq)]
pub struct Comparison {
    pub kind: &'static str,
    pub label: String,
    pub baseline: [u64; 2],
    pub recent: [u64; 2],
    pub drop_points: f64,
    pub warning: bool,
}

impl Comparison {
    fn to_value(&self) -> Value {
        json!({"kind": self.kind, "label": self.label,
               "baseline": accuracy_counts(self.baseline[0], self.baseline[1]),
               "recent": accuracy_counts(self.recent[0], self.recent[1]),
               "dropPoints": self.drop_points, "warning": self.warning})
    }
}

/// `performance_drift`'s result.
#[derive(Clone, Debug, PartialEq)]
pub struct Drift {
    pub recent: Rolling,
    pub baseline: Rolling,
    pub comparisons: Vec<Comparison>,
}

impl Drift {
    pub fn warnings(&self) -> impl Iterator<Item = &Comparison> {
        self.comparisons.iter().filter(|c| c.warning)
    }

    pub fn to_value(&self) -> Value {
        json!({"recentDays": 7, "baselineDays": 21,
               "recent": self.recent.accuracy(), "baseline": self.baseline.accuracy(),
               "comparisons": self.comparisons.iter().map(Comparison::to_value).collect::<Vec<_>>(),
               "warnings": self.warnings().map(Comparison::to_value).collect::<Vec<_>>()})
    }
}

/// Compare a recent week with the preceding, disjoint 21-day baseline.
pub fn performance_drift(records: &[Map<String, Value>], now: NaiveDateTime) -> Drift {
    let split = iso_seconds(now - Duration::days(7));
    let (recent_rows, baseline_rows): (Vec<_>, Vec<_>) = records.iter().cloned().partition(|r| ts_of(r) >= split);
    let current = rolling_metrics(&recent_rows, now, 7);
    let baseline = rolling_metrics(&baseline_rows, now - Duration::days(7), 21);
    let mut comparisons = Vec::new();
    let mut compare = |kind: &'static str, label: &str, previous: [u64; 2], latest: [u64; 2], minimum: u64| {
        let ([old_n, old_ok], [new_n, new_ok]) = (previous, latest);
        if old_n.min(new_n) < minimum {
            return;
        }
        let (old_rate, new_rate) = (old_ok as f64 / old_n as f64, new_ok as f64 / new_n as f64);
        let pooled = (old_ok + new_ok) as f64 / (old_n + new_n) as f64;
        let error = (pooled * (1.0 - pooled) * (1.0 / old_n as f64 + 1.0 / new_n as f64)).sqrt();
        let drop = old_rate - new_rate;
        let z = if error != 0.0 { drop / error } else { 0.0 };
        comparisons.push(Comparison {
            kind,
            label: label.to_owned(),
            baseline: previous,
            recent: latest,
            drop_points: py_round(100.0 * drop, 2),
            warning: drop >= 0.03 && z >= 3.0,
        });
    };
    compare(
        "overall",
        "Overall accuracy",
        [baseline.predicted, baseline.correct],
        [current.predicted, current.correct],
        50,
    );
    let band = |r: &Rolling| r.bands.get("confident").copied().unwrap_or([0, 0]);
    compare("band", "Confident suggestions", band(&baseline), band(&current), 50);
    for (kind, old, new) in [
        ("route_precision", &baseline.by_predicted, &current.by_predicted),
        ("request_type", &baseline.segments, &current.segments),
    ] {
        let labels: BTreeSet<&String> = old.keys().filter(|k| new.contains_key(*k)).collect();
        for label in labels {
            compare(kind, label, old[label], new[label], 30);
        }
    }
    Drift { recent: current, baseline, comparisons }
}

/// Select per-route review thresholds on gate only; evaluate on final.
pub fn threshold_experiment(gate: &[Sample], final_: &[Sample]) -> Value {
    let mut result = Map::new();
    let dests: BTreeSet<&String> = gate.iter().map(|p| &p.predicted).collect();
    let kept = |rows: &[Sample], dest: &str, conf: i64, margin: i64| -> (u64, u64) {
        let chosen: Vec<&Sample> =
            rows.iter().filter(|p| p.predicted == dest && p.conf >= conf && p.margin >= margin && !p.review).collect();
        (chosen.len() as u64, chosen.iter().map(|p| p.correct).sum())
    };
    for dest in dests {
        let mut best: Option<(u64, i64, i64)> = None;
        for conf in [80, 85, 90, 95, 99] {
            for margin in [20, 25, 30] {
                let (n, correct) = kept(gate, dest, conf, margin);
                if n >= 10 && correct as f64 / n as f64 >= 0.95 && wilson_lower(correct, n) >= 0.90 {
                    let option = (n, -conf, -margin);
                    if best.is_none_or(|b| option > b) {
                        best = Some(option);
                    }
                }
            }
        }
        let Some((n, neg_conf, neg_margin)) = best else {
            result.insert(dest.clone(), json!({"review": true, "reason": "Insufficient gate evidence"}));
            continue;
        };
        let (conf, margin) = (-neg_conf, -neg_margin);
        let (fn_, fc) = kept(final_, dest, conf, margin);
        result.insert(
            dest.clone(),
            json!({"reviewConf": conf, "reviewMargin": margin, "gateN": n, "final": accuracy_counts(fn_, fc)}),
        );
    }
    Value::Object(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(y: i32, m: u32, d: u32, h: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, 0, 0).unwrap()
    }

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn wilson_needs_real_evidence() {
        // test_integrity: ten perfect examples do not prove reliable precision.
        assert!(wilson_lower(10, 10) < 0.90 && 0.90 < wilson_lower(100, 100));
        assert_eq!(wilson_lower(0, 0), 0.0);
    }

    #[test]
    fn rolling_windows_and_coverage() {
        // test_learning_progress: recent windows / unknown bands / daily coverage.
        let now = at(2030, 5, 10, 12);
        let event = |days: i64, predicted: Value, correct: bool, review: Value, reason: &str| {
            obj(json!({"ts": iso_seconds(now - Duration::days(days)), "predicted": predicted, "dest": "IT",
                       "predictionCorrect": correct, "review": review, "requestType": "General enquiry",
                       "coverageReason": reason}))
        };
        let rows = vec![
            event(1, json!("Tier Z"), false, json!(true), "predicted"),
            event(8, json!("IT"), true, json!(false), "predicted"),
            event(31, json!("Tier Z"), false, json!(true), "predicted"),
            event(2, Value::Null, false, json!(true), "no_intake_observation"),
            event(3, json!("IT"), true, Value::Null, "predicted"),
        ];
        let week = rolling_metrics(&rows, now, 7);
        let month = rolling_metrics(&rows, now, 30);
        assert!(week.routed == 3 && week.predicted == 2 && month.routed == 4);
        assert_eq!(month.accuracy()["pct"], json!(66.7));
        assert!(week.bands.get("confident").is_none() && week.bands["unknown"] == [1, 1]);
        assert_eq!(week.coverage_reasons["no_intake_observation"], 1);
        assert_eq!(week.daily_coverage.values().map(|[n, s]| n - s).sum::<u64>(), 1);
        assert_eq!(rolling_metrics(&rows, now + Duration::days(40), 30).routed, 0);
        assert_eq!(week.segments, Counts::from([("General enquiry".to_owned(), [2, 1])]));
    }

    #[test]
    fn drift_needs_material_drop_and_evidence() {
        // test_adaptive_routes: gradual drift alerts.
        let now = at(2030, 5, 30, 12);
        let mut events = Vec::new();
        for (days, n, correct) in [(15, 1000, 970), (2, 1000, 880)] {
            for i in 0..n {
                events.push(obj(json!({"ts": iso_seconds(now - Duration::days(days)), "predicted": "Queue Ω",
                    "dest": if i < correct { "Queue Ω" } else { "Équipe α" }, "predictionCorrect": i < correct,
                    "review": false, "requestType": "Besoin général"})));
            }
        }
        let drift = performance_drift(&events, now);
        let kinds: BTreeSet<&str> = drift.warnings().map(|w| w.kind).collect();
        assert_eq!(kinds, BTreeSet::from(["overall", "band", "route_precision", "request_type"]));
        assert!(drift.baseline.predicted == 1000 && drift.recent.predicted == 1000);
        let mut tiny: Vec<_> = events[..5].to_vec();
        tiny.extend_from_slice(&events[events.len() - 5..]);
        assert_eq!(performance_drift(&tiny, now).warnings().count(), 0);
        let stable: Vec<_> = events
            .iter()
            .map(|r| {
                let mut r = r.clone();
                r.insert("predictionCorrect".into(), json!(true));
                r
            })
            .collect();
        assert_eq!(performance_drift(&stable, now).warnings().count(), 0);
        assert_eq!(performance_drift(&[], now).warnings().count(), 0);
        let boundary = obj(json!({"ts": iso_seconds(now - Duration::days(7)), "predicted": "Queue Ω",
                                  "dest": "Queue Ω", "predictionCorrect": true}));
        let edge = performance_drift(std::slice::from_ref(&boundary), now);
        assert!(edge.recent.predicted == 1 && edge.baseline.predicted == 0);
        let mut old = boundary.clone();
        old.insert("ts".into(), json!(iso_seconds(now - Duration::days(29))));
        let mut future = boundary;
        future.insert("ts".into(), json!(iso_seconds(now + Duration::days(1))));
        let edge = performance_drift(&[old, future], now);
        assert!(edge.recent.predicted == 0 && edge.baseline.predicted == 0);
    }

    #[test]
    fn thresholds_never_look_at_the_final_window() {
        let sample = |correct| Sample {
            actual: "D".into(),
            predicted: "D".into(),
            conf: 99,
            margin: 30,
            review: false,
            correct,
            key: String::new(),
            summary: String::new(),
        };
        let gate = vec![sample(1); 100];
        let final_ = vec![sample(0); 5];
        let out = threshold_experiment(&gate, &final_);
        assert_eq!(out["D"]["reviewConf"], json!(80));
        assert_eq!(out["D"]["final"]["pct"], json!(0.0));
    }

    #[test]
    fn coverage_reasons() {
        let routed = obj(json!({"predicted": null}));
        assert_eq!(coverage_reason(&obj(json!({"predicted": "X"})), None, true), "predicted");
        assert_eq!(coverage_reason(&routed, None, true), "repeat_transition_without_prediction");
        assert_eq!(coverage_reason(&routed, Some(&obj(json!({"prediction": null}))), false), "cold_start");
        assert_eq!(
            coverage_reason(&routed, Some(&obj(json!({"prediction": "X"}))), false),
            "prediction_context_not_retained"
        );
        assert_eq!(coverage_reason(&routed, None, false), "no_intake_observation");
    }
}
