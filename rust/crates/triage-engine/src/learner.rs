//! Naive-Bayes-style router over learned destinations, ported line for line
//! from the Python `Learner` so predictions are identical.
//!
//! Speed without changing results: every floating-point expression keeps the
//! Python operand order, and the caches below only memoise values that are
//! recomputed bit-for-bit identically (sums over unchanged tables, set sizes).
//! The caches assume the model is changed only through [`Learner::learn`].

use std::collections::HashMap;

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::model::{FastMap, FastSet, Model, Nested, RouteBalance, Table};
use crate::num::{first_max, py_round_int, py_sum};
use crate::text::{BIGRAM, counter, truthy};
use crate::tuning::Tuning;

pub const MIN_EXAMPLES: f64 = 8.0;
pub const HINT_MIN_EXAMPLES: f64 = 12.0;
pub const HINT_MIN_SHARE: f64 = 0.80;
const RENORM_FLOOR: f64 = 1e-9;
const EX_CAP: usize = 5000;
const EX_SLACK: usize = 1000;
const WHO_MIN_SEEN: f64 = 3.0;
const WHO_MIN_RATIO: f64 = 2.0;
/// Destination subsets whose vocabulary size is kept (the ready set and the
/// escalation set, plus a few superseded ready sets from early learning).
const UNION_CACHE: usize = 4;

/// One routing suggestion.
#[derive(Clone, Debug, PartialEq)]
pub struct Prediction {
    pub dest: Option<String>,
    pub conf: i64,
    pub second: Option<String>,
    pub second_conf: i64,
    pub margin: i64,
    pub review: bool,
    pub source: &'static str,
    pub note: String,
    pub why: Vec<String>,
}

impl Prediction {
    pub fn cold() -> Self {
        Self {
            dest: None,
            conf: 0,
            second: None,
            second_conf: 0,
            margin: 0,
            review: true,
            source: "cold",
            note: "still learning — not enough observed examples yet".into(),
            why: vec![],
        }
    }

    /// The Python dict layout (`dest, conf, second, secondConf, margin,
    /// review, source, note, why`, plus `why` last in the cold case order).
    pub fn to_value(&self) -> Value {
        if self.source == "cold" {
            return json!({"dest": null, "conf": 0, "second": null, "secondConf": 0, "margin": 0,
                          "review": true, "source": "cold", "why": [], "note": self.note});
        }
        json!({"dest": self.dest, "conf": self.conf, "second": self.second, "secondConf": self.second_conf,
               "margin": self.margin, "review": self.review, "source": self.source, "note": self.note,
               "why": self.why})
    }
}

/// Route-frequency correction factors (Python `route_balance_factors`).
pub fn route_balance_factors(balance: &RouteBalance, weight: f64) -> HashMap<String, f64> {
    balance_factors(balance, weight).into_iter().map(|(d, f)| (d.to_owned(), f)).collect()
}

/// [`route_balance_factors`] as `(dest, factor)` pairs; dests are unique.
fn balance_factors(balance: &RouteBalance, weight: f64) -> Vec<(&str, f64)> {
    let mut factors: Vec<(&str, f64)> = Vec::new();
    if weight == 0.0 || balance.observations < 50.0 {
        return factors;
    }
    let exponent = weight.clamp(0.0, 2.0);
    for dest in balance.actual.keys().chain(balance.predicted.keys()) {
        if factors.iter().any(|(d, _)| d == dest) {
            continue;
        }
        let observed = balance.actual.get(dest).copied().unwrap_or(0.0);
        let suggested = balance.predicted.get(dest).copied().unwrap_or(0.0);
        if observed.max(suggested) < 10.0 {
            continue;
        }
        let ratio = ((observed + 20.0) / (suggested + 20.0)).clamp(0.5, 2.0);
        factors.push((dest, ratio.powf(exponent)));
    }
    factors
}

fn factor_of(factors: &[(&str, f64)], dest: &str) -> Option<f64> {
    factors.iter().find(|(d, _)| *d == dest).map(|(_, f)| *f)
}

/// Inverted index over exemplar titles: token -> exemplar positions, plus each
/// exemplar's token count.
type ExemplarIndex = (FastMap<String, Vec<usize>>, Vec<usize>);

/// Values derived from the model, kept in step with it by `learn`.
#[derive(Default)]
struct Caches {
    /// `len(set().union(*tables))` for destination subsets the scorer used.
    unions: Vec<Union>,
    /// dest → unscaled `sum(reqTypes[dest].values())` and
    /// `sum(reporters[dest].values())`, dropped whenever those tables change.
    side_sums: FastMap<String, [Option<f64>; 2]>,
    /// Scratch for `nearest`: shared-token count per exemplar, and which
    /// exemplars are non-zero.
    overlap: Vec<u32>,
    touched: Vec<usize>,
}

const REQ_TYPES: usize = 0;
const REPORTERS: usize = 1;

/// CPython `sum(table.values())`, memoised per destination.
fn side_sum(sums: &mut FastMap<String, [Option<f64>; 2]>, which: usize, table: Option<&Table>, dest: &str) -> f64 {
    if let Some(v) = sums.get(dest).and_then(|s| s[which]) {
        return v;
    }
    let v = table.map_or(0.0, |t| py_sum(t.values().copied()));
    match sums.get_mut(dest) {
        Some(slot) => slot[which] = Some(v),
        None => {
            let mut slot = [None, None];
            slot[which] = Some(v);
            sums.insert(dest.to_owned(), slot);
        }
    }
    v
}

/// The vocabulary size of a destination subset.
struct Union {
    dests: Vec<String>,
    /// Each dest's position in `model.tokens` (`None`: no table yet).
    cols: Vec<Option<usize>>,
    len: usize,
}

/// Number of distinct tokens across `dests`' token tables, memoised.
fn union_len(unions: &mut Vec<Union>, tokens: &Nested, dests: &[&str]) -> usize {
    if let Some(u) =
        unions.iter().find(|u| u.dests.len() == dests.len() && u.dests.iter().zip(dests).all(|(a, b)| a == b))
    {
        return u.len;
    }
    let len = dests.iter().filter_map(|d| tokens.get(*d)).flat_map(|t| t.keys()).collect::<FastSet<&String>>().len();
    if unions.len() >= UNION_CACHE {
        unions.remove(0);
    }
    unions.push(Union {
        dests: dests.iter().map(|d| (*d).to_owned()).collect(),
        cols: dests.iter().map(|d| tokens.get_index_of(*d)).collect(),
        len,
    });
    len
}

/// Where a token sits in one destination's table, with a copy of its value.
#[derive(Clone, Copy)]
struct Cell {
    pos: u32,
    value: f64,
}

/// No entry: reads as `.get(t, 0)` does.
const ABSENT: Cell = Cell { pos: u32::MAX, value: 0.0 };

impl Cell {
    fn present(self) -> bool {
        self.pos != ABSENT.pos
    }
}

/// Token-major view of `model.tokens`: token → a [`Cell`] per destination,
/// by the table's position in `model.tokens` (a short row is absent). The
/// values are copies of the table entries, so reading them is the same as
/// looking them up; the keys are the whole vocabulary.
type TokenIndex = FastMap<String, Vec<Cell>>;

fn cell(row: &[Cell], col: usize) -> Cell {
    row.get(col).copied().unwrap_or(ABSENT)
}

fn ensure_index<'a>(index: &'a mut Option<TokenIndex>, model: &Model) -> &'a mut TokenIndex {
    index.get_or_insert_with(|| {
        let mut index = TokenIndex::default();
        for (col, table) in model.tokens.values().enumerate() {
            for (pos, (t, v)) in table.iter().enumerate() {
                let put = |row: &mut Vec<Cell>| {
                    row.resize(row.len().max(col + 1), ABSENT);
                    row[col] = Cell { pos: u32::try_from(pos).expect("table fits u32"), value: *v };
                };
                if let Some(row) = index.get_mut(t.as_str()) {
                    put(row);
                } else {
                    let mut row = Vec::new();
                    put(&mut row);
                    index.insert(t.clone(), row);
                }
            }
        }
        index
    })
}

/// `table[key] = table.get(key, 0) + x`, appending new keys.
fn add(table: &mut Table, key: &str, x: f64) {
    match table.get_mut(key) {
        Some(v) => *v += x,
        None => {
            table.insert(key.to_owned(), 0.0 + x);
        }
    }
}

fn nested_add(nested: &mut Nested, dest: &str, key: &str, x: f64) {
    match nested.get_mut(dest) {
        Some(table) => add(table, key, x),
        None => {
            let mut table = Table::default();
            add(&mut table, key, x);
            nested.insert(dest.to_owned(), table);
        }
    }
}

fn clone_project_for(model: &Model, dest: &str) -> String {
    model.clone_projects.get(dest).and_then(|cp| first_max(cp.iter()).cloned()).unwrap_or_default()
}

fn plan_for(plans: &IndexMap<String, String>, model: &Model, dest: &str) -> String {
    if let Some(plan) = plans.get(dest).filter(|p| !p.is_empty()) {
        return plan.clone();
    }
    let proj = clone_project_for(model, dest);
    if proj.is_empty() {
        format!("move to {dest}")
    } else {
        format!("raise a linked {proj} issue, then move to {dest}")
    }
}

fn request_type_hint<'a>(model: &'a Model, req_type: &str) -> Option<&'a str> {
    if req_type.is_empty() {
        return None;
    }
    let scale = model.scale;
    let counts: Vec<(&String, f64)> =
        model.req_types.iter().map(|(d, table)| (d, table.get(req_type).copied().unwrap_or(0.0) * scale)).collect();
    let total = py_sum(counts.iter().map(|(_, c)| *c));
    if total < HINT_MIN_EXAMPLES {
        return None;
    }
    let best: &'a String = *first_max(counts.iter().map(|(d, c)| (d, c)))?;
    let best_count = counts.iter().find(|(d, _)| *d == best).map(|(_, c)| *c).unwrap_or(0.0);
    (best_count / total >= HINT_MIN_SHARE).then_some(best.as_str())
}

/// Most similar previous title (Jaccard), newest first on ties.
fn nearest(
    model: &Model,
    ex_index: &mut Option<ExemplarIndex>,
    caches: &mut Caches,
    title: &[String],
) -> (Option<String>, f64) {
    if model.exemplars.is_empty() || title.is_empty() {
        return (None, 0.0);
    }
    let (index, lens) = ex_index.get_or_insert_with(|| {
        let mut index: FastMap<String, Vec<usize>> = FastMap::default();
        let mut lens = Vec::with_capacity(model.exemplars.len());
        for (i, (toks, _)) in model.exemplars.iter().enumerate() {
            for t in toks {
                index.entry(t.clone()).or_default().push(i);
            }
            lens.push(toks.len());
        }
        (index, lens)
    });
    let mut query: Vec<&str> = title.iter().map(String::as_str).collect();
    query.sort_unstable();
    query.dedup();
    let Caches { overlap, touched, .. } = caches;
    if overlap.len() < lens.len() {
        overlap.resize(lens.len(), 0);
    }
    touched.clear();
    for t in &query {
        for &i in index.get(*t).map(Vec::as_slice).unwrap_or_default() {
            if overlap[i] == 0 {
                touched.push(i);
            }
            overlap[i] += 1;
        }
    }
    let nq = query.len();
    // Python scans newest first and keeps the first strict improvement: the
    // highest similarity, the newest exemplar among equals.
    let (mut best, mut best_j) = (None, 0.0_f64);
    for &i in touched.iter() {
        let shared = overlap[i] as usize;
        overlap[i] = 0;
        let j = shared as f64 / (nq + lens[i] - shared) as f64;
        if j > best_j || (j == best_j && best.is_some_and(|b| i > b)) {
            best_j = j;
            best = Some(i);
        }
    }
    (best.map(|i| model.exemplars[i].1.clone()), best_j)
}

/// Per-prediction inputs shared by the full scorer and the escalation rerank.
struct Scoring<'a> {
    tuning: &'a Tuning,
    counts: &'a [(&'a str, u32)],
    req_type: &'a str,
    reporter: &'a str,
    /// Ready destinations, in prior order.
    ready: &'a [&'a str],
    priors: &'a [f64],
    /// `tokens[ready[d]].get(counts[t], 0)` at `raw[t * ready.len() + d]`.
    raw: &'a [f64],
    /// Tokens no ready destination has a count for (every raw cell `+0.0`):
    /// their per-destination term is the same value, computed once.
    unseen: &'a [bool],
    hint: Option<&'a str>,
}

impl Scoring<'_> {
    fn raw(&self, token: usize, dest: usize) -> f64 {
        self.raw[token * self.ready.len() + dest]
    }

    /// Request-type and reporter terms, in the Python order.
    fn side_terms(&self, m: &Model, caches: &mut Caches, d: &str, mut score: f64) -> f64 {
        let s = m.scale;
        if !self.req_type.is_empty() {
            let rt = m.req_types.get(d);
            let n_rt = side_sum(&mut caches.side_sums, REQ_TYPES, rt, d) * s;
            score += ((rt.and_then(|t| t.get(self.req_type)).copied().unwrap_or(0.0) * s + 1.0)
                / (n_rt + self.tuning.get("reqSmooth")))
            .ln();
            if self.hint == Some(d) {
                score += self.tuning.get("requestTypeHintWeight");
            }
        }
        if !self.reporter.is_empty() {
            let rp = m.reporters.get(d);
            let n_rp = side_sum(&mut caches.side_sums, REPORTERS, rp, d) * s;
            score += self.tuning.get("reporterWeight")
                * ((rp.and_then(|t| t.get(self.reporter)).copied().unwrap_or(0.0) * s + 1.0)
                    / (n_rp + self.tuning.get("repSmooth")))
                .ln();
        }
        score
    }

    /// Score a closed candidate set against itself (escalation rerank).
    /// `candidates` index `ready`.
    fn subset_scores(&self, m: &Model, caches: &mut Caches, candidates: &[usize], prior_weight: f64) -> Vec<f64> {
        let s = m.scale;
        let total_examples = py_sum(candidates.iter().map(|&i| self.priors[i]));
        let names: Vec<&str> = candidates.iter().map(|&i| self.ready[i]).collect();
        let vocab = union_len(&mut caches.unions, &m.tokens, &names).max(1) as f64;
        let tot_all = py_sum(names.iter().map(|d| m.totals.get(*d).copied().unwrap_or(0.0))) * s;
        let all_cnt: Vec<f64> =
            (0..self.counts.len()).map(|t| py_sum(candidates.iter().map(|&i| self.raw(t, i))) * s).collect();
        let mut scores = Vec::with_capacity(candidates.len());
        for &i in candidates {
            let d = self.ready[i];
            let mut score = prior_weight * (self.priors[i] / total_examples).ln();
            let total = m.totals.get(d).copied().unwrap_or(0.0) * s;
            let den_in = total + vocab;
            let den_out = (tot_all - total) + vocab;
            let term = |raw: f64, all: f64| {
                let inside = raw * s;
                let p_in = (inside + 1.0) / den_in;
                let p_out = (all - inside + 1.0) / den_out;
                (p_in / p_out).ln()
            };
            // An unseen token's all_cnt is py_sum(+0.0, ...) * s == 0.0 * s.
            let unseen_term = term(0.0, 0.0 * s);
            for (t, (_, reps)) in self.counts.iter().enumerate() {
                let x = if self.unseen[t] { unseen_term } else { term(self.raw(t, i), all_cnt[t]) };
                score += f64::from(*reps) * x;
            }
            scores.push(self.side_terms(m, caches, d, score));
        }
        scores
    }
}

fn why(
    m: &Model,
    vocab_len: usize,
    caches: &mut Caches,
    scoring: &Scoring<'_>,
    best: &str,
    second: &str,
) -> Vec<String> {
    let (tuning, reporter) = (scoring.tuning, scoring.reporter);
    let vocab = vocab_len.max(1) as f64;
    let s = m.scale;
    let tok_best = m.tokens.get(best);
    let tok_second = m.tokens.get(second);
    let get = |t: Option<&Table>, k: &str| t.and_then(|t| t.get(k)).copied().unwrap_or(0.0);
    let den_best = m.totals.get(best).copied().unwrap_or(0.0) * s + vocab;
    let den_second = m.totals.get(second).copied().unwrap_or(0.0) * s + vocab;
    let ratio_ln = |b: f64, c: f64| {
        let rate_best = (b * s + 1.0) / den_best;
        let rate_second = (c * s + 1.0) / den_second;
        (rate_best / rate_second).ln()
    };
    let unseen_ln = ratio_ln(0.0, 0.0);
    // Counts for ready destinations are already in the score matrix.
    let col_best = scoring.ready.iter().position(|d| *d == best);
    let col_second = scoring.ready.iter().position(|d| *d == second);
    let mut weighed: Vec<(f64, &str)> = Vec::new();
    for (i, (t, k)) in scoring.counts.iter().enumerate() {
        let b = col_best.map_or_else(|| get(tok_best, t), |c| scoring.raw(i, c));
        let c = col_second.map_or_else(|| get(tok_second, t), |c| scoring.raw(i, c));
        let x = if b.to_bits() == 0 && c.to_bits() == 0 { unseen_ln } else { ratio_ln(b, c) };
        let weight = f64::from(*k) * x;
        if weight > 0.0 {
            weighed.push((weight, t));
        }
    }
    weighed.sort_by(|a, b| (-a.0).partial_cmp(&-b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut reasons: Vec<String> = weighed.iter().take(4).map(|(_, t)| t.replace(BIGRAM, " ")).collect();
    if !reporter.is_empty() {
        let rep_best = m.reporters.get(best);
        let rep_second = m.reporters.get(second);
        let seen = get(rep_best, reporter) + get(rep_second, reporter);
        if seen * s >= WHO_MIN_SEEN {
            let rep_smooth = tuning.get("repSmooth");
            let n_best = side_sum(&mut caches.side_sums, REPORTERS, rep_best, best) * s;
            let n_second = side_sum(&mut caches.side_sums, REPORTERS, rep_second, second) * s;
            let rate_best = (get(rep_best, reporter) * s + 1.0) / (n_best + rep_smooth);
            let rate_second = (get(rep_second, reporter) * s + 1.0) / (n_second + rep_smooth);
            if rate_best >= rate_second * WHO_MIN_RATIO {
                reasons.push(format!("usually raised by {reporter}"));
            }
        }
    }
    reasons
}

pub struct Learner {
    pub model: Model,
    /// dest → plain-English plan, supplied by the watcher.
    pub plans: IndexMap<String, String>,
    pub escalation_dests: Vec<String>,
    /// dest → `{forceReview, reason, ...}` from nightly validation.
    pub route_safety: Map<String, Value>,
    index: Option<TokenIndex>,
    ex_index: Option<ExemplarIndex>,
    caches: Caches,
    /// Bumped by `learn`, so the watcher can skip rewriting unchanged state.
    pub mutations: u64,
}

impl Learner {
    pub fn new(model: Model, escalation_dests: Vec<String>) -> Self {
        Self {
            model,
            plans: IndexMap::new(),
            escalation_dests,
            route_safety: Map::new(),
            index: None,
            ex_index: None,
            caches: Caches::default(),
            mutations: 0,
        }
    }

    fn renormalise(&mut self) {
        let m = &mut self.model;
        let s = m.scale;
        let dests: Vec<String> = m.priors.keys().cloned().collect();
        for d in &dests {
            *m.priors.get_mut(d).expect("dest from priors") *= s;
            let total = m.totals.get(d).copied().unwrap_or(0.0) * s;
            m.totals.insert(d.clone(), total);
            for bucket in [&mut m.tokens, &mut m.req_types, &mut m.reporters] {
                if let Some(tbl) = bucket.get_mut(d) {
                    for v in tbl.values_mut() {
                        *v *= s;
                    }
                }
            }
        }
        m.scale = 1.0;
        self.caches.side_sums.clear();
        if let Some(index) = &mut self.index {
            let tables: Vec<&Table> = self.model.tokens.values().collect();
            for row in index.values_mut() {
                for (col, c) in row.iter_mut().enumerate() {
                    if c.present() {
                        c.value = tables[col][c.pos as usize];
                    }
                }
            }
        }
    }

    fn advance_decay(&mut self, tuning: &Tuning) -> f64 {
        let f = tuning.get("decayFactor");
        if f < 1.0 {
            self.model.scale *= f;
            if self.model.scale < RENORM_FLOOR {
                self.renormalise();
            }
        }
        1.0 / self.model.scale
    }

    #[allow(clippy::too_many_arguments)]
    pub fn learn(
        &mut self,
        tuning: &Tuning,
        dest: &str,
        tokens: &[String],
        req_type: &str,
        clone_projects: &[String],
        reporter: &str,
        weight: f64,
        exemplar: Option<&[String]>,
        prediction: Option<&Prediction>,
    ) {
        let counts: Vec<(&str, u32)> = counter(tokens).into_iter().collect();
        let computed;
        let prediction = match prediction {
            Some(p) => p,
            None => {
                computed = self.predict_counted(tuning, &counts, tokens.len(), req_type, reporter, exemplar);
                &computed
            }
        };
        self.learn_counted(
            tuning,
            dest,
            &counts,
            tokens.len(),
            req_type,
            clone_projects,
            reporter,
            weight,
            exemplar,
            prediction,
        );
    }

    /// `learn` from `Counter(tokens)` (first-occurrence order) and `len(tokens)`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn learn_counted(
        &mut self,
        tuning: &Tuning,
        dest: &str,
        counts: &[(&str, u32)],
        n_tokens: usize,
        req_type: &str,
        clone_projects: &[String],
        reporter: &str,
        weight: f64,
        exemplar: Option<&[String]>,
        prediction: &Prediction,
    ) {
        if let Some(pred_dest) = &prediction.dest {
            let balance = &mut self.model.route_balance;
            for table in [&mut balance.actual, &mut balance.predicted] {
                for v in table.values_mut() {
                    *v *= 0.995;
                }
            }
            add(&mut balance.actual, dest, 1.0);
            add(&mut balance.predicted, pred_dest, 1.0);
            balance.observations += 1.0;
        }
        self.mutations += 1;
        let boost = self.advance_decay(tuning);
        let w = weight * boost;
        let Self { model: m, index, ex_index, caches, .. } = self;
        add(&mut m.priors, dest, boost);
        add(&mut m.seen, dest, 1.0);
        let col = match m.tokens.get_index_of(dest) {
            Some(col) => col,
            None => {
                let col = m.tokens.insert_full(dest.to_owned(), Table::default()).0;
                for u in &mut caches.unions {
                    for (name, c) in u.dests.iter().zip(&mut u.cols) {
                        if name == dest {
                            *c = Some(col);
                        }
                    }
                }
                col
            }
        };
        // One index lookup per token finds its table entry, if any.
        let index = ensure_index(index, m);
        let tok = &mut m.tokens[col];
        let unions = &mut caches.unions;
        for (t, k) in counts {
            let x = w * f64::from(*k);
            let mut update = |row: &mut Vec<Cell>| {
                if row.len() <= col {
                    row.resize(col + 1, ABSENT);
                }
                let c = row[col];
                if c.present() {
                    let v = &mut tok[c.pos as usize];
                    *v += x;
                    row[col].value = *v;
                } else {
                    // A token new to this destination grows a subset's
                    // vocabulary unless another member already has it.
                    for u in unions.iter_mut() {
                        if u.cols.contains(&Some(col))
                            && u.cols.iter().all(|e| *e == Some(col) || !e.is_some_and(|e| cell(row, e).present()))
                        {
                            u.len += 1;
                        }
                    }
                    let v = 0.0 + x;
                    let (pos, old) = tok.insert_full((*t).to_owned(), v);
                    debug_assert!(old.is_none(), "token index out of step with the table");
                    row[col] = Cell { pos: u32::try_from(pos).expect("table fits u32"), value: v };
                }
            };
            if let Some(row) = index.get_mut(*t) {
                update(row);
            } else {
                let mut row = Vec::new();
                update(&mut row);
                index.insert((*t).to_owned(), row);
            }
        }
        add(&mut m.totals, dest, w * n_tokens as f64);
        if !req_type.is_empty() {
            nested_add(&mut m.req_types, dest, req_type, w);
        }
        if !reporter.is_empty() {
            nested_add(&mut m.reporters, dest, reporter, w);
        }
        if !req_type.is_empty() || !reporter.is_empty() {
            caches.side_sums.remove(dest);
        }
        for proj in clone_projects {
            nested_add(&mut m.clone_projects, dest, proj, 1.0);
        }
        if let Some(exemplar) = exemplar.filter(|e| !e.is_empty()) {
            let mut toks: Vec<String> = exemplar.to_vec();
            toks.sort();
            toks.dedup();
            m.exemplars.push((toks, dest.to_owned()));
            if m.exemplars.len() > EX_CAP + EX_SLACK {
                let excess = m.exemplars.len() - EX_CAP;
                m.exemplars.drain(..excess);
                *ex_index = None;
            } else if let Some((index, lens)) = ex_index {
                let i = m.exemplars.len() - 1;
                let toks = &m.exemplars[i].0;
                for t in toks {
                    match index.get_mut(t.as_str()) {
                        Some(list) => list.push(i),
                        None => {
                            index.insert(t.clone(), vec![i]);
                        }
                    }
                }
                lens.push(toks.len());
            }
        }
    }

    pub fn clone_project_for(&self, dest: &str) -> String {
        clone_project_for(&self.model, dest)
    }

    pub fn plan_for(&self, dest: &str) -> String {
        plan_for(&self.plans, &self.model, dest)
    }

    pub fn request_type_hint(&self, req_type: &str) -> Option<String> {
        request_type_hint(&self.model, req_type).map(str::to_owned)
    }

    /// Full prediction: best destination, confidence, runner-up, margin and
    /// whether a human should double-check.
    pub fn predict_ex(
        &mut self,
        tuning: &Tuning,
        tokens: &[String],
        req_type: &str,
        reporter: &str,
        exemplar: Option<&[String]>,
    ) -> Prediction {
        let counts: Vec<(&str, u32)> = counter(tokens).into_iter().collect();
        self.predict_counted(tuning, &counts, tokens.len(), req_type, reporter, exemplar)
    }

    /// `predict_ex` from `Counter(tokens)` (first-occurrence order) and `len(tokens)`.
    pub(crate) fn predict_counted(
        &mut self,
        tuning: &Tuning,
        counts: &[(&str, u32)],
        n_tokens: usize,
        req_type: &str,
        reporter: &str,
        exemplar: Option<&[String]>,
    ) -> Prediction {
        let Self { model: m, plans, escalation_dests, route_safety, index, ex_index, caches, .. } = self;
        let (ready, priors): (Vec<&str>, Vec<f64>) = m
            .priors
            .iter()
            .filter(|(d, prior)| m.seen.get(*d).copied().unwrap_or(**prior) >= MIN_EXAMPLES)
            .map(|(d, prior)| (d.as_str(), *prior))
            .unzip();
        if ready.len() < 2 {
            return Prediction::cold();
        }
        let index: &TokenIndex = ensure_index(index, m);
        let vocab_len = index.len();
        let nr = ready.len();
        let s = m.scale;
        let total_examples = py_sum(priors.iter().copied());
        let vocab = if nr == m.tokens.len() {
            vocab_len.max(1) as f64
        } else {
            union_len(&mut caches.unions, &m.tokens, &ready).max(1) as f64
        };

        // One lookup per token for every ready destination's count, reused
        // by every term below.
        let cols: Vec<Option<usize>> = ready.iter().map(|d| m.tokens.get_index_of(*d)).collect();
        let mut raw = vec![0.0_f64; counts.len() * nr];
        let mut unseen = vec![true; counts.len()];
        for ((cells, (t, _)), unseen) in raw.chunks_exact_mut(nr).zip(counts).zip(&mut unseen) {
            if let Some(row) = index.get(*t) {
                for (value, col) in cells.iter_mut().zip(&cols) {
                    *value = col.map_or(0.0, |c| cell(row, c).value);
                }
                *unseen = cells.iter().all(|v| v.to_bits() == 0);
            }
        }
        let log_odds = tuning.get("logOdds") != 0.0;
        let mut tot_all = 0.0;
        let mut all_cnt: Vec<f64> = Vec::new();
        if log_odds {
            tot_all = py_sum(ready.iter().map(|d| m.totals.get(*d).copied().unwrap_or(0.0))) * s;
            all_cnt = raw
                .chunks_exact(nr)
                .map(|row| {
                    let mut c = 0.0_f64;
                    for v in row {
                        c += v;
                    }
                    c * s
                })
                .collect();
        }
        let prior_w = tuning.get("priorWeight");
        let hint_for_type = request_type_hint(m, req_type);
        let scoring = Scoring {
            tuning,
            counts,
            req_type,
            reporter,
            ready: &ready,
            priors: &priors,
            raw: &raw,
            unseen: &unseen,
            hint: hint_for_type,
        };
        let mut scores: Vec<f64> = Vec::with_capacity(nr);
        for (di, d) in ready.iter().enumerate() {
            let mut score = prior_w * (priors[di] / total_examples).ln();
            let tot = m.totals.get(*d).copied().unwrap_or(0.0) * s;
            let den_in = tot + vocab;
            if log_odds {
                let den_out = (tot_all - tot) + vocab;
                let term = |raw: f64, all: f64| {
                    let c_in = raw * s;
                    let p_in = (c_in + 1.0) / den_in;
                    let p_out = (all - c_in + 1.0) / den_out;
                    (p_in / p_out).ln()
                };
                // An unseen token's all_cnt is (0.0 + 0.0 + ...) * s == 0.0 * s.
                let unseen_term = term(0.0, 0.0 * s);
                for (ti, (_, k)) in counts.iter().enumerate() {
                    let x = if unseen[ti] { unseen_term } else { term(scoring.raw(ti, di), all_cnt[ti]) };
                    score += f64::from(*k) * x;
                }
            } else {
                let term = |raw: f64| ((raw * s + 1.0) / den_in).ln();
                let unseen_term = term(0.0);
                for (ti, (_, k)) in counts.iter().enumerate() {
                    let x = if unseen[ti] { unseen_term } else { term(scoring.raw(ti, di)) };
                    score += f64::from(*k) * x;
                }
            }
            scores.push(scoring.side_terms(m, caches, d, score));
        }

        // Temperature-scaled softmax.
        let global_best = stable_argmax(&scores);
        let mx = scores[global_best];
        let temp = (n_tokens as f64 / tuning.get("confTemp")).max(1.0);
        let exps: Vec<f64> = scores.iter().map(|s| ((s - mx) / temp).exp()).collect();
        let tot_e = py_sum(exps.iter().copied());
        let mut probs: Vec<f64> = exps.iter().map(|e| e / tot_e).collect();
        let mut source = "model";

        let escalation: Vec<usize> =
            escalation_dests.iter().filter_map(|d| ready.iter().position(|r| r == d)).collect();
        if tuning.get("escalationSpecialist") != 0.0 && escalation.contains(&global_best) && escalation.len() >= 2 {
            let handoff_mass = py_sum(escalation.iter().map(|&i| probs[i]));
            let esc_scores = scoring.subset_scores(m, caches, &escalation, tuning.get("escalationPriorWeight"));
            let esc_mx = esc_scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let esc_exps: Vec<f64> = esc_scores.iter().map(|s| ((s - esc_mx) / temp).exp()).collect();
            let esc_total_e = py_sum(esc_exps.iter().copied());
            for &i in &escalation {
                let first = escalation.iter().position(|&x| x == i).expect("member");
                probs[i] = handoff_mass * esc_exps[first] / esc_total_e;
            }
            source = "escalation";
        }

        let factors = balance_factors(&m.route_balance, tuning.get("routeBalanceWeight"));
        if !factors.is_empty() {
            let adjusted: Vec<f64> =
                probs.iter().zip(&ready).map(|(p, d)| p * factor_of(&factors, d).unwrap_or(1.0)).collect();
            let mass = py_sum(adjusted.iter().copied());
            probs = adjusted.into_iter().map(|p| p / mass).collect();
        }
        let mut order: Vec<usize> = (0..probs.len()).collect();
        order.sort_by(|a, b| probs[*b].partial_cmp(&probs[*a]).unwrap_or(std::cmp::Ordering::Equal));
        let mut best = ready[order[0]].to_owned();
        let mut second = ready[order[1]].to_owned();
        let mut conf = py_round_int(100.0 * probs[order[0]]);
        let mut conf2 = py_round_int(100.0 * probs[order[1]]);
        let mut note = plan_for(plans, m, &best);
        let hint_dest = hint_for_type;
        let hint_conf = match hint_dest {
            Some(h) => py_round_int(100.0 * ready.iter().position(|d| *d == h).map_or(0.0, |i| probs[i])),
            None => 0,
        };

        let thresh = tuning.get("knnThresh");
        if let Some(exemplar) = exemplar.filter(|e| !e.is_empty())
            && thresh != 0.0
        {
            let (k_dest, sim) = nearest(m, ex_index, caches, exemplar);
            if let Some(k_dest) = k_dest.filter(|k| !k.is_empty())
                && sim >= thresh
            {
                let k_conf = py_round_int(50.0 + 50.0 * sim);
                if k_dest != best || k_conf > conf {
                    let same = k_dest == best;
                    if !same {
                        second = best.clone();
                        conf2 = conf;
                    }
                    conf = k_conf.max(if same { conf } else { 0 });
                    best = k_dest;
                    source = "memory";
                    note = format!("{} (matches a previous ticket)", plan_for(plans, m, &best));
                }
            }
        }

        if source != "memory" && (factor_of(&factors, &best).unwrap_or(1.0) - 1.0).abs() > 1e-6 {
            note.push_str("; ranking adjusted using earlier routing outcomes");
        }
        let hint_conflict = hint_dest.is_some_and(|h| ready.contains(&h) && best != h);
        if hint_conflict {
            let h = hint_dest.expect("conflict implies a hint");
            if second != h {
                second = h.to_owned();
                conf2 = hint_conf;
            } else {
                conf2 = conf2.max(hint_conf);
            }
            note.push_str(&format!("; review because Request Type points to {h}"));
        }
        let margin = conf - conf2;
        let mut review =
            (conf as f64) < tuning.get("reviewConf") || (margin as f64) < tuning.get("reviewMargin") || hint_conflict;
        if let Some(Value::Object(safety)) = route_safety.get(&best)
            && safety.get("forceReview").is_some_and(truthy)
        {
            review = true;
            let reason = safety
                .get("reason")
                .filter(|r| truthy(r))
                .map(crate::text::py_str)
                .unwrap_or_else(|| "route confidence needs more evidence".into());
            note.push_str(&format!("; review because {reason}"));
        }
        let why = if second.is_empty() { vec![] } else { why(m, vocab_len, caches, &scoring, &best, &second) };
        Prediction {
            dest: Some(best),
            conf,
            second: Some(second),
            second_conf: conf2,
            margin,
            review,
            source,
            note,
            why,
        }
    }

    /// The strongest tokens for a destination (report).
    pub fn top_keywords(&self, dest: &str, n: usize) -> Vec<String> {
        let Some(tok) = self.model.tokens.get(dest) else { return vec![] };
        let mut items: Vec<(&String, f64)> = tok.iter().map(|(k, v)| (k, *v)).collect();
        items.sort_by(|a, b| (-a.1).partial_cmp(&-b.1).unwrap_or(std::cmp::Ordering::Equal));
        items.into_iter().take(n).map(|(t, _)| t.replace(BIGRAM, " ")).collect()
    }
}

/// Index of the first maximum (Python's stable `sorted(..., reverse=True)[0]`).
fn stable_argmax(scores: &[f64]) -> usize {
    let mut best = 0;
    for (i, s) in scores.iter().enumerate() {
        if *s > scores[best] {
            best = i;
        }
    }
    best
}
