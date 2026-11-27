//! Chronological replay and the nightly self-tune (Python
//! `triage_evaluation.py`).

use std::collections::HashMap;

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::learner::{Learner, Prediction};
use crate::model::Model;
use crate::num::{py_round, py_sum};
use crate::text::{TicketText, features_for, tokenize};
use crate::tuning::{TUNE_AXES, Tuning, knob_value};

/// One labelled decision from `triage_history.jsonl`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Record {
    pub key: String,
    pub when: String,
    pub dest: String,
    pub text: TicketText,
    pub reporter: String,
    /// `requestType`, falling back to the legacy `reqType`.
    pub request_type: String,
    pub clone_projects: Vec<String>,
    pub feature_origin: String,
}

fn strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(items)) => items.iter().filter_map(|i| i.as_str().map(str::to_owned)).collect(),
        _ => Vec::new(),
    }
}

fn string(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or_default().to_owned()
}

impl Record {
    /// A raw history row, as `_replay_history` reads it (no sanitising).
    pub fn from_value(v: &Value) -> Option<Self> {
        let m = v.as_object()?;
        let request_type =
            if m.contains_key("requestType") { string(m.get("requestType")) } else { string(m.get("reqType")) };
        Some(Self {
            key: string(m.get("key")),
            when: string(m.get("when")),
            dest: m.get("dest")?.as_str()?.to_owned(),
            text: TicketText {
                summary: m.get("summary")?.as_str()?.to_owned(),
                desc: string(m.get("desc")),
                labels: strings(m.get("labels")),
                components: strings(m.get("components")),
                structured: strings(m.get("structured")),
            },
            reporter: string(m.get("reporter")),
            request_type,
            clone_projects: strings(m.get("cloneProjects")),
            feature_origin: m.get("featureOrigin").and_then(Value::as_str).unwrap_or("outcome").to_owned(),
        })
    }
}

/// Python `segment_name`.
pub fn segment_name(request_type: &str) -> String {
    if request_type.is_empty() { "Unknown request type".into() } else { request_type.to_owned() }
}

/// One predict-then-learn step, shared by replay and nightly refit.
pub fn fit_example(learner: &mut Learner, tuning: &Tuning, record: &Record) -> Prediction {
    let tokens = features_for(&record.text, &record.request_type, tuning.get("summaryWeight"));
    let title = tokenize(&record.text.summary);
    let prediction = learner.predict_ex(tuning, &tokens, &record.request_type, &record.reporter, Some(&title));
    let mistake = prediction.dest.as_ref().is_some_and(|d| *d != record.dest);
    let weight = if mistake { tuning.get("mistakeWeight").trunc() } else { 1.0 };
    learner.learn(
        tuning,
        &record.dest,
        &tokens,
        &record.request_type,
        &record.clone_projects,
        &record.reporter,
        weight,
        Some(&title),
        Some(&prediction),
    );
    prediction
}

pub type Counts = IndexMap<String, [u64; 2]>;

#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub actual: String,
    pub predicted: String,
    pub conf: i64,
    pub margin: i64,
    pub review: bool,
    pub correct: u64,
    pub key: String,
    pub summary: String,
}

/// Replay metrics (`_replay_history`'s dict).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Metrics {
    pub acc: f64,
    pub segments: Counts,
    pub feature_origins: IndexMap<String, u64>,
    pub samples: Vec<Sample>,
    pub firm_acc: f64,
    pub firm_cov: f64,
    pub n: u64,
    pub balanced: f64,
    pub worst: f64,
    pub per: IndexMap<String, f64>,
    pub per_counts: Counts,
    pub precision: IndexMap<String, f64>,
    pub precision_counts: Counts,
    pub macro_precision: f64,
    pub worst_precision: f64,
    pub f1: IndexMap<String, f64>,
    pub macro_f1: f64,
    pub worst_f1: f64,
    pub firm_balanced: f64,
    pub firm_worst: f64,
    pub firm_per: IndexMap<String, f64>,
    pub firm_per_counts: Counts,
    pub firm_precision: IndexMap<String, f64>,
    pub firm_precision_counts: Counts,
    pub firm_macro_precision: f64,
    pub firm_worst_precision: f64,
    pub sources: IndexMap<String, (u64, f64)>,
    pub model: Option<Model>,
}

fn rates(counts: &Counts) -> IndexMap<String, f64> {
    counts.iter().filter(|(_, v)| v[0] > 0).map(|(k, v)| (k.clone(), v[1] as f64 / v[0] as f64)).collect()
}

fn mean(values: &IndexMap<String, f64>) -> f64 {
    if values.is_empty() { 0.0 } else { py_sum(values.values().copied()) / values.len() as f64 }
}

fn minimum(values: impl IntoIterator<Item = f64>) -> f64 {
    values.into_iter().reduce(|a, b| if b < a { b } else { a }).unwrap_or(0.0)
}

fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 { 0.0 } else { a as f64 / b as f64 }
}

fn bump(counts: &mut Counts, key: &str, hit: u64) {
    let row = counts.entry(key.to_owned()).or_insert([0, 0]);
    row[0] += 1;
    row[1] += hit;
}

/// Chronologically predict-then-learn over `records` under `tuning`,
/// scoring predictions in `[start, end)` (default start: the warm-up).
pub fn replay(
    records: &[Record],
    tuning: &Tuning,
    dests: &[String],
    score_start: Option<usize>,
    score_end: Option<usize>,
    include_model: bool,
) -> Metrics {
    let mut learner = Learner::new(Model::default(), dests.to_vec());
    let warm = (records.len() / 5).min(150);
    let start = score_start.unwrap_or(warm);
    let end = score_end.unwrap_or(records.len());
    let (mut n, mut ok, mut firm, mut firm_ok) = (0_u64, 0_u64, 0_u64, 0_u64);
    let mut out = Metrics::default();
    let (mut per, mut firm_per, mut predicted_per, mut firm_predicted_per, mut source_per) =
        (Counts::new(), Counts::new(), Counts::new(), Counts::new(), Counts::new());
    for (i, r) in records.iter().enumerate() {
        let p = fit_example(&mut learner, tuning, r);
        let Some(pred) = p.dest.as_ref() else { continue };
        if !(start <= i && i < end) {
            continue;
        }
        let hit = u64::from(*pred == r.dest);
        n += 1;
        ok += hit;
        bump(&mut out.segments, &segment_name(&r.request_type), hit);
        *out.feature_origins.entry(r.feature_origin.clone()).or_insert(0) += 1;
        out.samples.push(Sample {
            actual: r.dest.clone(),
            predicted: pred.clone(),
            conf: p.conf,
            margin: p.margin,
            review: p.review,
            correct: hit,
            key: r.key.clone(),
            summary: r.text.summary.clone(),
        });
        bump(&mut per, &r.dest, hit);
        bump(&mut predicted_per, pred, hit);
        bump(&mut source_per, p.source, hit);
        if !p.review {
            firm += 1;
            firm_ok += hit;
            bump(&mut firm_per, &r.dest, hit);
            bump(&mut firm_predicted_per, pred, hit);
        }
    }
    let per_acc = rates(&per);
    let firm_per_acc = rates(&firm_per);
    let precision = rates(&predicted_per);
    let firm_precision = rates(&firm_predicted_per);
    let mut labels: Vec<String> = per.keys().cloned().collect();
    for k in predicted_per.keys() {
        if !labels.contains(k) {
            labels.push(k.clone());
        }
    }
    let f1: IndexMap<String, f64> = labels
        .iter()
        .map(|label| {
            let recall = per_acc.get(label).copied().unwrap_or(0.0);
            let prec = precision.get(label).copied().unwrap_or(0.0);
            let f = if prec + recall != 0.0 { 2.0 * prec * recall / (prec + recall) } else { 0.0 };
            (label.clone(), f)
        })
        .collect();
    out.acc = ratio(ok, n);
    out.firm_acc = ratio(firm_ok, firm);
    out.firm_cov = ratio(firm, n);
    out.n = n;
    out.balanced = mean(&per_acc);
    out.worst = minimum(per_acc.values().copied());
    out.macro_precision = if labels.is_empty() {
        0.0
    } else {
        py_sum(labels.iter().map(|k| precision.get(k).copied().unwrap_or(0.0))) / labels.len() as f64
    };
    out.worst_precision = minimum(labels.iter().map(|k| precision.get(k).copied().unwrap_or(0.0)));
    out.macro_f1 = mean(&f1);
    out.worst_f1 = minimum(f1.values().copied());
    out.firm_balanced = mean(&firm_per_acc);
    out.firm_worst = minimum(firm_per_acc.values().copied());
    out.firm_macro_precision = mean(&firm_precision);
    out.firm_worst_precision = minimum(firm_precision.values().copied());
    out.sources =
        source_per.iter().filter(|(_, v)| v[0] > 0).map(|(k, v)| (k.clone(), (v[0], ratio(v[1], v[0])))).collect();
    out.per = per_acc;
    out.per_counts = per;
    out.precision = precision;
    out.precision_counts = predicted_per;
    out.f1 = f1;
    out.firm_per = firm_per_acc;
    out.firm_per_counts = firm_per;
    out.firm_precision = firm_precision;
    out.firm_precision_counts = firm_predicted_per;
    if include_model {
        out.model = Some(learner.model);
    }
    out
}

/// Lexicographic tuning score (`_tune_score`).
pub type Score = [f64; 12];

pub fn tune_score(m: &Metrics) -> Score {
    [
        py_round(m.macro_f1, 3),
        py_round(m.balanced, 3),
        py_round(m.macro_precision, 3),
        py_round(m.worst_f1, 3),
        py_round(m.worst, 3),
        py_round(m.firm_macro_precision, 3),
        py_round(m.firm_worst_precision, 3),
        py_round(m.firm_balanced, 3),
        py_round(m.firm_worst, 3),
        py_round(m.firm_acc, 3),
        m.firm_cov,
        py_round(m.acc, 3),
    ]
}

fn greater(a: &Score, b: &Score) -> bool {
    for (x, y) in a.iter().zip(b) {
        if x != y {
            return x > y;
        }
    }
    false
}

/// A macro-score win cannot purchase a material accuracy regression.
pub fn accept_challenger(incumbent: &Metrics, challenger: &Metrics) -> bool {
    greater(&tune_score(challenger), &tune_score(incumbent))
        && challenger.acc >= incumbent.acc - 0.005
        && challenger.firm_acc >= incumbent.firm_acc - 0.005
}

pub const TUNE_BUDGET: u32 = 120;
const TUNE_PASSES: u32 = 3;

/// Knob values for the searched axes, in axis order.
pub type Knobs = Vec<(&'static str, f64)>;

fn knobs_key(knobs: &Knobs) -> Vec<u64> {
    // 1 == 1.0 in Python's memo key; normalising -0.0 keeps that true here.
    knobs.iter().map(|(_, v)| (v + 0.0).to_bits()).collect()
}

/// The nightly tune's result (`_self_tune`'s dict).
#[derive(Clone, Debug)]
pub struct TuneResult {
    pub score: Score,
    pub cfg: Knobs,
    pub train: Metrics,
    pub split: usize,
    pub evals: u32,
    pub validation_windows: usize,
    pub challenger_rejected: bool,
    pub gate_reason: &'static str,
    pub incumbent_gate_score: Score,
    pub challenger_gate_score: Score,
    pub challenger_cfg: Option<Knobs>,
    pub gate: Metrics,
    pub holdout: Metrics,
    pub final_holdout: Metrics,
}

impl TuneResult {
    pub fn cfg_json(&self) -> Map<String, Value> {
        knobs_json(&self.cfg)
    }
}

pub fn knobs_json(knobs: &Knobs) -> Map<String, Value> {
    knobs.iter().map(|(k, v)| ((*k).to_owned(), knob_value(k, *v))).collect()
}

/// Tune, gate against the incumbent, then report on an untouched tail.
pub fn self_tune(records: &[Record], live: &Tuning, dests: &[String]) -> Option<TuneResult> {
    let incumbent: Knobs = live.keep();
    // The watcher only tunes with 300+ decisions, so `split` is never clamped.
    let validation_n = (records.len() / 3).clamp(100, 1200);
    let split = records.len().saturating_sub(validation_n);
    let tuning_recs = &records[..split];

    let mut cache: HashMap<Vec<u64>, (Score, Metrics)> = HashMap::new();
    let mut spent = 0_u32;
    let recent_start = (tuning_recs.len() / 5).min(150).max(tuning_recs.len().saturating_sub(1200));

    let mut evaluate = |knobs: &Knobs, budget: &mut u32, cache: &mut HashMap<Vec<u64>, (Score, Metrics)>| {
        let sig = knobs_key(knobs);
        if let Some(hit) = cache.get(&sig) {
            return Some(hit.clone());
        }
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        spent += 1;
        let m = replay(tuning_recs, &live.with(knobs), dests, Some(recent_start), None, false);
        let entry = (tune_score(&m), m);
        cache.insert(sig, entry.clone());
        Some(entry)
    };

    let mut descend = |seed: Knobs, mut budget: u32, cache: &mut HashMap<Vec<u64>, (Score, Metrics)>| {
        let mut cur = seed;
        let (mut best_score, mut best_m) = evaluate(&cur, &mut budget, cache)?;
        for _ in 0..TUNE_PASSES {
            let mut improved = false;
            for (axis, values) in TUNE_AXES {
                for value in values {
                    let current = cur.iter().find(|(k, _)| *k == axis).map(|(_, v)| *v);
                    if current == Some(*value) {
                        continue;
                    }
                    let mut trial = cur.clone();
                    if let Some(slot) = trial.iter_mut().find(|(k, _)| *k == axis) {
                        slot.1 = *value;
                    }
                    let Some((score, m)) = evaluate(&trial, &mut budget, cache) else { break };
                    if greater(&score, &best_score) {
                        best_score = score;
                        best_m = m;
                        cur = trial;
                        improved = true;
                    }
                }
                if budget == 0 {
                    break;
                }
            }
            if !improved || budget == 0 {
                break;
            }
        }
        Some((best_score, best_m, cur))
    };

    let half = (TUNE_BUDGET / 2).max(1);
    let balanced_seed: Knobs = {
        let base = live.with(&Tuning::balanced());
        base.keep()
    };
    let runs: Vec<(Score, Metrics, Knobs)> =
        [descend(live.keep(), half, &mut cache), descend(balanced_seed, TUNE_BUDGET - half, &mut cache)]
            .into_iter()
            .flatten()
            .collect();
    let mut best_run = runs.first()?.clone();
    for run in runs.into_iter().skip(1) {
        if greater(&run.0, &best_run.0) {
            best_run = run;
        }
    }
    let (best_score, best_m, cur) = best_run;
    let challenger = cur;

    let final_n = (validation_n / 3).clamp(100, 400);
    let final_start = records.len().saturating_sub(final_n);
    let gate_end = split.max(final_start);
    let challenger_gate = replay(records, &live.with(&challenger), dests, Some(split), Some(gate_end), false);
    let incumbent_gate = replay(records, &live.with(&incumbent), dests, Some(split), Some(gate_end), false);
    let rejected = !accept_challenger(&incumbent_gate, &challenger_gate);
    let chosen = if rejected { incumbent.clone() } else { challenger.clone() };
    let chosen_tuning = live.with(&chosen);
    let holdout = replay(records, &chosen_tuning, dests, Some(split), None, false);
    let final_holdout = replay(records, &chosen_tuning, dests, Some(final_start), None, false);
    Some(TuneResult {
        score: best_score,
        cfg: chosen,
        train: best_m,
        split,
        evals: spent,
        validation_windows: validation_n.div_ceil(400).max(1),
        challenger_rejected: rejected,
        gate_reason: if rejected {
            "Challenger did not improve the chronological gate within accuracy safeguards"
        } else {
            "Challenger beat incumbent on chronological gate"
        },
        incumbent_gate_score: tune_score(&incumbent_gate),
        challenger_gate_score: tune_score(&challenger_gate),
        challenger_cfg: rejected.then_some(challenger),
        gate: if rejected { incumbent_gate } else { challenger_gate },
        holdout,
        final_holdout,
    })
}

/// Metrics as the Python dict (for golden comparison and state files).
pub fn metrics_json(m: &Metrics) -> Value {
    let counts = |c: &Counts| Value::Object(c.iter().map(|(k, v)| (k.clone(), json!(v))).collect());
    let floats = |f: &IndexMap<String, f64>| Value::Object(f.iter().map(|(k, v)| (k.clone(), json!(v))).collect());
    json!({
        "acc": m.acc, "segments": counts(&m.segments), "featureOrigins": m.feature_origins,
        "samples": m.samples.iter().map(|s| json!({"actual": s.actual, "predicted": s.predicted, "conf": s.conf,
            "margin": s.margin, "review": s.review, "correct": s.correct, "key": s.key, "summary": s.summary}))
            .collect::<Vec<_>>(),
        "firmAcc": m.firm_acc, "firmCov": m.firm_cov, "n": m.n, "balanced": m.balanced, "worst": m.worst,
        "per": floats(&m.per), "perCounts": counts(&m.per_counts), "precision": floats(&m.precision),
        "precisionCounts": counts(&m.precision_counts), "macroPrecision": m.macro_precision,
        "worstPrecision": m.worst_precision, "f1": floats(&m.f1), "macroF1": m.macro_f1, "worstF1": m.worst_f1,
        "firmBalanced": m.firm_balanced, "firmWorst": m.firm_worst, "firmPer": floats(&m.firm_per),
        "firmPerCounts": counts(&m.firm_per_counts), "firmPrecision": floats(&m.firm_precision),
        "firmPrecisionCounts": counts(&m.firm_precision_counts), "firmMacroPrecision": m.firm_macro_precision,
        "firmWorstPrecision": m.firm_worst_precision,
        "sources": m.sources.iter().map(|(k, (n, acc))| (k.clone(), json!({"n": n, "acc": acc}))).collect::<Map<_, _>>(),
    })
}
