//! Chronological replay and the nightly self-tune (Python
//! `triage_evaluation.py`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::learner::{Learner, Prediction};
use crate::model::Model;
use crate::num::{py_round, py_sum};
use crate::text::{TicketText, feature_parts, summary_repeat, tokenize};
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

/// A record's tokenised text, which no tuning knob affects: computed once and
/// reused by every replay of a tune.
pub struct Prepared {
    title: Vec<String>,
    /// `Counter(summary features)` in first-occurrence order, and whether
    /// each token is also a body feature.
    summary: Vec<(String, u32, bool)>,
    summary_len: usize,
    /// Sorted body features, and whether each is also a summary feature.
    body: Vec<(String, bool)>,
}

impl Prepared {
    pub fn new(record: &Record) -> Self {
        let (summary, body) = feature_parts(&record.text, &record.request_type);
        let in_body = |t: &String| body.binary_search(t).is_ok();
        let mut counted: Vec<(String, u32, bool)> = Vec::new();
        for t in &summary {
            match counted.iter_mut().find(|(u, _, _)| u == t) {
                Some(slot) => slot.1 += 1,
                None => counted.push((t.clone(), 1, in_body(t))),
            }
        }
        let body = body.iter().map(|t| (t.clone(), counted.iter().any(|(u, _, _)| u == t))).collect();
        Self { title: tokenize(&record.text.summary), summary: counted, summary_len: summary.len(), body }
    }

    /// `Counter(features_for(...))` and `len(features_for(...))` when the
    /// summary repeats `repeat` times: summary tokens first (in first-occurrence
    /// order), then body-only tokens in sorted order.
    fn counts(&self, repeat: usize) -> (Vec<(&str, u32)>, usize) {
        let mut counts = Vec::with_capacity(self.summary.len() + self.body.len());
        if repeat > 0 {
            let r = u32::try_from(repeat).expect("summary weight fits u32");
            counts.extend(self.summary.iter().map(|(t, c, b)| (t.as_str(), r * c + u32::from(*b))));
        }
        counts.extend(self.body.iter().filter(|(_, s)| repeat == 0 || !s).map(|(t, _)| (t.as_str(), 1)));
        (counts, self.summary_len * repeat + self.body.len())
    }
}

/// One predict-then-learn step, shared by replay and nightly refit.
pub fn fit_example(learner: &mut Learner, tuning: &Tuning, record: &Record) -> Prediction {
    fit_prepared(learner, tuning, record, &Prepared::new(record))
}

fn fit_prepared(learner: &mut Learner, tuning: &Tuning, record: &Record, prep: &Prepared) -> Prediction {
    let (counts, n_tokens) = prep.counts(summary_repeat(tuning.get("summaryWeight")));
    let title = Some(prep.title.as_slice());
    let prediction = learner.predict_counted(tuning, &counts, n_tokens, &record.request_type, &record.reporter, title);
    let mistake = prediction.dest.as_ref().is_some_and(|d| *d != record.dest);
    let weight = if mistake { tuning.get("mistakeWeight").trunc() } else { 1.0 };
    learner.learn_counted(
        tuning,
        &record.dest,
        &counts,
        n_tokens,
        &record.request_type,
        &record.clone_projects,
        &record.reporter,
        weight,
        title,
        &prediction,
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
    if let Some(row) = counts.get_mut(key) {
        row[0] += 1;
        row[1] += hit;
    } else {
        counts.insert(key.to_owned(), [1, hit]);
    }
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
    let prepared: Vec<Prepared> = records.iter().map(Prepared::new).collect();
    let warm = (records.len() / 5).min(150);
    let window = (score_start.unwrap_or(warm), score_end.unwrap_or(records.len()));
    let (mut out, model) = replay_windows(records, &prepared, tuning, dests, &[window]);
    let mut metrics = out.pop().expect("one window");
    if include_model {
        metrics.model = Some(model);
    }
    metrics
}

/// One chronological replay scored over several `[start, end)` windows at
/// once. Learning never depends on scoring, so each window's metrics equal a
/// separate [`replay`] with that window.
pub fn replay_windows(
    records: &[Record],
    prepared: &[Prepared],
    tuning: &Tuning,
    dests: &[String],
    windows: &[(usize, usize)],
) -> (Vec<Metrics>, Model) {
    let mut learner = Learner::new(Model::default(), dests.to_vec());
    let mut scorers: Vec<Scorer> = windows.iter().map(|_| Scorer::default()).collect();
    for (i, (r, prep)) in records.iter().zip(prepared).enumerate() {
        let p = fit_prepared(&mut learner, tuning, r, prep);
        let Some(pred) = p.dest.as_ref() else { continue };
        for (scorer, (start, end)) in scorers.iter_mut().zip(windows) {
            if *start <= i && i < *end {
                scorer.observe(r, pred, &p);
            }
        }
    }
    (scorers.into_iter().map(Scorer::finish).collect(), learner.model)
}

/// `_replay_history`'s running tallies for one scoring window.
#[derive(Default)]
struct Scorer {
    n: u64,
    ok: u64,
    firm: u64,
    firm_ok: u64,
    out: Metrics,
    per: Counts,
    firm_per: Counts,
    predicted_per: Counts,
    firm_predicted_per: Counts,
    source_per: Counts,
}

impl Scorer {
    fn observe(&mut self, r: &Record, pred: &str, p: &Prediction) {
        let hit = u64::from(pred == r.dest);
        self.n += 1;
        self.ok += hit;
        let out = &mut self.out;
        bump(&mut out.segments, &segment_name(&r.request_type), hit);
        match out.feature_origins.get_mut(&r.feature_origin) {
            Some(n) => *n += 1,
            None => {
                out.feature_origins.insert(r.feature_origin.clone(), 1);
            }
        }
        out.samples.push(Sample {
            actual: r.dest.clone(),
            predicted: pred.to_owned(),
            conf: p.conf,
            margin: p.margin,
            review: p.review,
            correct: hit,
            key: r.key.clone(),
            summary: r.text.summary.clone(),
        });
        bump(&mut self.per, &r.dest, hit);
        bump(&mut self.predicted_per, pred, hit);
        bump(&mut self.source_per, p.source, hit);
        if !p.review {
            self.firm += 1;
            self.firm_ok += hit;
            bump(&mut self.firm_per, &r.dest, hit);
            bump(&mut self.firm_predicted_per, pred, hit);
        }
    }

    fn finish(self) -> Metrics {
        let Self { n, ok, firm, firm_ok, mut out, per, firm_per, predicted_per, firm_predicted_per, source_per } = self;
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
        out
    }
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

type Evaluated = Arc<(Score, Metrics)>;
type Cache = HashMap<Vec<u64>, Evaluated>;

/// Exact knob bits: the replay store's key (the search's memo key,
/// [`knobs_key`], folds -0.0 into 0.0 as Python's `1 == 1.0` key does).
fn exact_key(knobs: &Knobs) -> Vec<u64> {
    knobs.iter().map(|(_, v)| v.to_bits()).collect()
}

/// Replays keyed by exact knobs. A replay is a pure function of its knobs,
/// so results may be computed ahead of time (in parallel, speculatively) and
/// handed to the sequential search when it asks. The search's own memo cache
/// and budget are kept separately and exactly as in Python, so this changes
/// timing only.
/// One replay, computed at most once; waiters block until it is ready.
type Slot<T> = Arc<OnceLock<Arc<T>>>;

struct Store<'a, T, F> {
    compute: F,
    cells: Mutex<HashMap<Vec<u64>, Slot<T>>>,
    /// Replays running now, across all stores of one tune.
    active: &'a AtomicUsize,
}

impl<'a, T: Send + Sync, F: Fn(&Knobs) -> T + Sync> Store<'a, T, F> {
    fn new(compute: F, active: &'a AtomicUsize) -> Self {
        Self { compute, cells: Mutex::new(HashMap::new()), active }
    }

    /// The replay for `knobs`, computing it here unless it is done or under
    /// way on another thread (then this waits for it).
    fn get(&self, knobs: &Knobs) -> Arc<T> {
        let cell = self.cells.lock().expect("replay store").entry(exact_key(knobs)).or_default().clone();
        cell.get_or_init(|| {
            self.active.fetch_add(1, Ordering::Relaxed);
            let out = Arc::new((self.compute)(knobs));
            self.active.fetch_sub(1, Ordering::Relaxed);
            out
        })
        .clone()
    }

    /// Whether `knobs` is computed or being computed.
    fn started(&self, knobs: &Knobs) -> bool {
        self.cells.lock().expect("replay store").contains_key(&exact_key(knobs))
    }

    /// Compute `trials` concurrently (any already computed are skipped).
    fn prefetch(&self, trials: &[Knobs]) {
        if trials.len() < 2 {
            return;
        }
        std::thread::scope(|scope| {
            for knobs in trials {
                scope.spawn(move || self.get(knobs));
            }
        });
    }
}

/// Axes beyond the current one whose trials may be fetched speculatively,
/// while there are idle cores.
const LOOKAHEAD: usize = 3;
/// Extra budget for the second descent's run-ahead, whose view of the first
/// descent's evaluations is incomplete while both run.
const AHEAD_SLACK: u32 = 4;

/// Which copy of a descent this is.
#[derive(Clone, Copy, PartialEq)]
enum Role {
    /// The search itself: records each signature it pays for in `shared`.
    Real,
    /// A run-ahead of a later descent, only to warm the store: treats
    /// signatures in `shared` (the earlier descent's, so far) as free.
    Ahead,
}

/// The trials an axis will pay for from `cur` (values other than the current
/// one, not already known, within `budget`), and the budget left after them.
fn axis_trials(
    cur: &Knobs,
    axis: &str,
    values: &[f64],
    known: &impl Fn(&Vec<u64>) -> bool,
    budget: u32,
) -> (Vec<Knobs>, u32) {
    let start = cur.iter().find(|(k, _)| *k == axis).map(|(_, v)| *v);
    let mut left = budget;
    let mut trials = Vec::new();
    for value in values {
        let trial = set_axis(cur, axis, *value);
        if start == Some(*value) || known(&knobs_key(&trial)) {
            continue;
        }
        if left == 0 {
            break;
        }
        left -= 1;
        trials.push(trial);
    }
    (trials, left)
}

fn set_axis(knobs: &Knobs, axis: &str, value: f64) -> Knobs {
    let mut trial = knobs.clone();
    if let Some(slot) = trial.iter_mut().find(|(k, _)| *k == axis) {
        slot.1 = value;
    }
    trial
}

/// Python's coordinate descent (`_tune_descend`): `cache` and `spent` carry
/// over between the two descents, `budget` does not.
fn descend<F: Fn(&Knobs) -> (Score, Metrics) + Sync>(
    replays: &Store<(Score, Metrics), F>,
    shared: &Mutex<HashSet<Vec<u64>>>,
    role: Role,
    seed: Knobs,
    mut budget: u32,
    cache: &mut Cache,
    spent: &mut u32,
) -> Option<(Score, Evaluated, Knobs)> {
    let capacity = std::thread::available_parallelism().map_or(1, |n| n.get());
    let free = |sig: &Vec<u64>| role == Role::Ahead && shared.lock().expect("shared signatures").contains(sig);
    let mut evaluate = |knobs: &Knobs, budget: &mut u32, cache: &mut Cache| {
        let sig = knobs_key(knobs);
        if let Some(hit) = cache.get(&sig) {
            return Some(hit.clone());
        }
        if !free(&sig) {
            if *budget == 0 {
                return None;
            }
            *budget -= 1;
            *spent += 1;
        }
        let entry = replays.get(knobs);
        if role == Role::Real {
            shared.lock().expect("shared signatures").insert(sig.clone());
        }
        cache.insert(sig, entry.clone());
        Some(entry)
    };
    let mut cur = seed;
    let mut best = evaluate(&cur, &mut budget, cache)?;
    let mut best_score = best.0;
    for _ in 0..TUNE_PASSES {
        let mut improved = false;
        for (ai, &(axis, values)) in TUNE_AXES.iter().enumerate() {
            // Adopting a value only changes this axis, so the axis's trials
            // are known up front. When any is missing, fetch them together
            // with the next axes' trials as they will be if nothing here is
            // adopted.
            let known = |sig: &Vec<u64>| cache.contains_key(sig) || free(sig);
            let (trials, mut left) = axis_trials(&cur, axis, values, &known, budget);
            let mut batch: Vec<Knobs> = trials.into_iter().filter(|t| !replays.started(t)).collect();
            if !batch.is_empty() {
                let mut spare = capacity.saturating_sub(replays.active.load(Ordering::Relaxed) + batch.len());
                for (next, next_values) in TUNE_AXES[ai + 1..].iter().take(LOOKAHEAD) {
                    if spare == 0 {
                        break;
                    }
                    let (more, rest) = axis_trials(&cur, next, next_values, &known, left);
                    let more: Vec<Knobs> = more.into_iter().filter(|t| !replays.started(t)).take(spare).collect();
                    spare -= more.len();
                    batch.extend(more);
                    left = rest;
                }
                replays.prefetch(&batch);
            }
            for value in values {
                let current = cur.iter().find(|(k, _)| *k == axis).map(|(_, v)| *v);
                if current == Some(*value) {
                    continue;
                }
                let trial = set_axis(&cur, axis, *value);
                let Some(entry) = evaluate(&trial, &mut budget, cache) else { break };
                if greater(&entry.0, &best_score) {
                    best_score = entry.0;
                    best = entry;
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
    Some((best_score, best, cur))
}

/// [`Prepared`] for every record, in parallel.
fn prepare_all(records: &[Record]) -> Vec<Prepared> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let chunk = records.len().div_ceil(threads).max(64);
    std::thread::scope(|scope| {
        let parts: Vec<_> = records
            .chunks(chunk)
            .map(|part| scope.spawn(move || part.iter().map(Prepared::new).collect::<Vec<_>>()))
            .collect();
        parts.into_iter().flat_map(|p| p.join().expect("prepare records")).collect()
    })
}

/// Tune, gate against the incumbent, then report on an untouched tail.
pub fn self_tune(records: &[Record], live: &Tuning, dests: &[String]) -> Option<TuneResult> {
    let incumbent: Knobs = live.keep();
    // The watcher only tunes with 300+ decisions, so `split` is never clamped.
    let validation_n = (records.len() / 3).clamp(100, 1200);
    let split = records.len().saturating_sub(validation_n);
    let final_n = (validation_n / 3).clamp(100, 400);
    let final_start = records.len().saturating_sub(final_n);
    let gate_end = split.max(final_start);
    let prepared = prepare_all(records);
    let tuning_recs = &records[..split];
    let recent_start = (tuning_recs.len() / 5).min(150).max(tuning_recs.len().saturating_sub(1200));

    let active = AtomicUsize::new(0);
    let replays = Store::new(
        |knobs: &Knobs| {
            let window = [(recent_start, tuning_recs.len())];
            let (mut m, _) = replay_windows(tuning_recs, &prepared[..split], &live.with(knobs), dests, &window);
            let m = m.pop().expect("one window");
            (tune_score(&m), m)
        },
        &active,
    );
    // Gate, holdout and final-holdout windows of one full-history replay.
    let full_history = Store::new(
        |knobs: &Knobs| {
            let windows = [(split, gate_end), (split, records.len()), (final_start, records.len())];
            let (m, _) = replay_windows(records, &prepared, &live.with(knobs), dests, &windows);
            <[Metrics; 3]>::try_from(m).expect("three windows")
        },
        &active,
    );
    let half = (TUNE_BUDGET / 2).max(1);
    let balanced_seed: Knobs = live.with(&Tuning::balanced()).keep();
    let shared = Mutex::new(HashSet::new());

    let (runs, spent) = std::thread::scope(|scope| {
        // The incumbent's gate replay needs no search result.
        scope.spawn(|| full_history.get(&incumbent));
        // Run the second descent ahead so its replays overlap the first's,
        // then start the full replay its result would need.
        scope.spawn(|| {
            let ahead = descend(
                &replays,
                &shared,
                Role::Ahead,
                balanced_seed.clone(),
                TUNE_BUDGET - half + AHEAD_SLACK,
                &mut Cache::new(),
                &mut 0,
            );
            if let Some((_, _, knobs)) = ahead {
                full_history.get(&knobs);
            }
        });
        let mut cache = Cache::new();
        let mut spent = 0_u32;
        let first = descend(&replays, &shared, Role::Real, live.keep(), half, &mut cache, &mut spent);
        if let Some((_, _, knobs)) = &first {
            let (knobs, full_history) = (knobs.clone(), &full_history);
            scope.spawn(move || full_history.get(&knobs));
        }
        let second =
            descend(&replays, &shared, Role::Real, balanced_seed.clone(), TUNE_BUDGET - half, &mut cache, &mut spent);
        let runs: Vec<(Score, Evaluated, Knobs)> = [first.clone(), second].into_iter().flatten().collect();
        (runs, spent)
    });
    let mut runs = runs.into_iter();
    let mut best_run = runs.next()?;
    for run in runs {
        if greater(&run.0, &best_run.0) {
            best_run = run;
        }
    }
    let (best_score, best_m, cur) = best_run;
    let challenger = cur;

    let [challenger_gate, challenger_holdout, challenger_final] = (*full_history.get(&challenger)).clone();
    let [incumbent_gate, incumbent_holdout, incumbent_final] = (*full_history.get(&incumbent)).clone();
    let rejected = !accept_challenger(&incumbent_gate, &challenger_gate);
    let chosen = if rejected { incumbent.clone() } else { challenger.clone() };
    let (holdout, final_holdout) =
        if rejected { (incumbent_holdout, incumbent_final) } else { (challenger_holdout, challenger_final) };
    Some(TuneResult {
        score: best_score,
        cfg: chosen,
        train: best_m.1.clone(),
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
