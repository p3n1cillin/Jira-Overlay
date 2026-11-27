//! Naive-Bayes-style router over learned destinations, ported line for line
//! from the Python `Learner` so predictions are identical.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::model::{Model, Table};
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
pub fn route_balance_factors(balance: &crate::model::RouteBalance, weight: f64) -> HashMap<String, f64> {
    let mut factors = HashMap::new();
    if weight == 0.0 || balance.observations < 50.0 {
        return factors;
    }
    let exponent = weight.clamp(0.0, 2.0);
    for dest in balance.actual.keys().chain(balance.predicted.keys()) {
        if factors.contains_key(dest) {
            continue;
        }
        let observed = balance.actual.get(dest).copied().unwrap_or(0.0);
        let suggested = balance.predicted.get(dest).copied().unwrap_or(0.0);
        if observed.max(suggested) < 10.0 {
            continue;
        }
        let ratio = ((observed + 20.0) / (suggested + 20.0)).clamp(0.5, 2.0);
        factors.insert(dest.clone(), ratio.powf(exponent));
    }
    factors
}

/// Inverted index over exemplar titles: token -> exemplar positions, plus each
/// exemplar's token count.
type ExemplarIndex = (HashMap<String, Vec<usize>>, Vec<usize>);

pub struct Learner {
    pub model: Model,
    /// dest → plain-English plan, supplied by the watcher.
    pub plans: IndexMap<String, String>,
    pub escalation_dests: Vec<String>,
    /// dest → `{forceReview, reason, ...}` from nightly validation.
    pub route_safety: Map<String, Value>,
    vocab: Option<HashSet<String>>,
    ex_index: Option<ExemplarIndex>,
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
            vocab: None,
            ex_index: None,
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

    fn vocab_len(&mut self) -> usize {
        if self.vocab.is_none() {
            self.vocab = Some(self.model.tokens.values().flat_map(|t| t.keys().cloned()).collect());
        }
        self.vocab.as_ref().map_or(0, HashSet::len)
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
        let computed;
        let prediction = match prediction {
            Some(p) => p,
            None => {
                computed = self.predict_ex(tuning, tokens, req_type, reporter, exemplar);
                &computed
            }
        };
        if let Some(pred_dest) = &prediction.dest {
            let balance = &mut self.model.route_balance;
            for table in [&mut balance.actual, &mut balance.predicted] {
                for v in table.values_mut() {
                    *v *= 0.995;
                }
            }
            *balance.actual.entry(dest.to_owned()).or_insert(0.0) += 1.0;
            *balance.predicted.entry(pred_dest.clone()).or_insert(0.0) += 1.0;
            balance.observations += 1.0;
        }
        self.mutations += 1;
        let boost = self.advance_decay(tuning);
        let w = weight * boost;
        let m = &mut self.model;
        *m.priors.entry(dest.to_owned()).or_insert(0.0) += boost;
        *m.seen.entry(dest.to_owned()).or_insert(0.0) += 1.0;
        let tok = m.tokens.entry(dest.to_owned()).or_default();
        for (t, k) in counter(tokens) {
            if let Some(vocab) = &mut self.vocab
                && !tok.contains_key(t)
            {
                vocab.insert(t.to_owned());
            }
            *tok.entry(t.to_owned()).or_insert(0.0) += w * f64::from(k);
        }
        *m.totals.entry(dest.to_owned()).or_insert(0.0) += w * tokens.len() as f64;
        if !req_type.is_empty() {
            *m.req_types.entry(dest.to_owned()).or_default().entry(req_type.to_owned()).or_insert(0.0) += w;
        }
        if !reporter.is_empty() {
            *m.reporters.entry(dest.to_owned()).or_default().entry(reporter.to_owned()).or_insert(0.0) += w;
        }
        for proj in clone_projects {
            *m.clone_projects.entry(dest.to_owned()).or_default().entry(proj.clone()).or_insert(0.0) += 1.0;
        }
        if let Some(exemplar) = exemplar.filter(|e| !e.is_empty()) {
            let mut toks: Vec<String> = exemplar.iter().cloned().collect::<HashSet<_>>().into_iter().collect();
            toks.sort();
            m.exemplars.push((toks, dest.to_owned()));
            if m.exemplars.len() > EX_CAP + EX_SLACK {
                let excess = m.exemplars.len() - EX_CAP;
                m.exemplars.drain(..excess);
                self.ex_index = None;
            } else if let Some((index, lens)) = &mut self.ex_index {
                let i = m.exemplars.len() - 1;
                let toks = &m.exemplars[i].0;
                for t in toks {
                    index.entry(t.clone()).or_default().push(i);
                }
                lens.push(toks.len());
            }
        }
    }

    /// Most similar previous title (Jaccard), newest first on ties.
    fn nearest(&mut self, title: &[String]) -> (Option<String>, f64) {
        if self.model.exemplars.is_empty() || title.is_empty() {
            return (None, 0.0);
        }
        if self.ex_index.is_none() {
            let mut index: HashMap<String, Vec<usize>> = HashMap::new();
            let mut lens = Vec::with_capacity(self.model.exemplars.len());
            for (i, (toks, _)) in self.model.exemplars.iter().enumerate() {
                for t in toks {
                    index.entry(t.clone()).or_default().push(i);
                }
                lens.push(toks.len());
            }
            self.ex_index = Some((index, lens));
        }
        let (index, lens) = self.ex_index.as_ref().expect("built above");
        let query: HashSet<&String> = title.iter().collect();
        let mut overlap: HashMap<usize, usize> = HashMap::new();
        for t in &query {
            for &i in index.get(*t).map(Vec::as_slice).unwrap_or_default() {
                *overlap.entry(i).or_insert(0) += 1;
            }
        }
        let nq = query.len();
        let mut candidates: Vec<usize> = overlap.keys().copied().collect();
        candidates.sort_unstable_by(|a, b| b.cmp(a));
        let (mut best_dest, mut best_j) = (None, 0.0_f64);
        for i in candidates {
            let shared = overlap[&i];
            let j = shared as f64 / (nq + lens[i] - shared) as f64;
            if j > best_j {
                best_j = j;
                best_dest = Some(self.model.exemplars[i].1.clone());
            }
        }
        (best_dest, best_j)
    }

    pub fn clone_project_for(&self, dest: &str) -> String {
        self.model.clone_projects.get(dest).and_then(|cp| first_max(cp.iter()).cloned()).unwrap_or_default()
    }

    pub fn plan_for(&self, dest: &str) -> String {
        if let Some(plan) = self.plans.get(dest).filter(|p| !p.is_empty()) {
            return plan.clone();
        }
        let proj = self.clone_project_for(dest);
        if proj.is_empty() {
            format!("move to {dest}")
        } else {
            format!("raise a linked {proj} issue, then move to {dest}")
        }
    }

    pub fn request_type_hint(&self, req_type: &str) -> Option<String> {
        if req_type.is_empty() {
            return None;
        }
        let scale = self.model.scale;
        let counts: Vec<(&String, f64)> = self
            .model
            .req_types
            .iter()
            .map(|(d, table)| (d, table.get(req_type).copied().unwrap_or(0.0) * scale))
            .collect();
        let total = py_sum(counts.iter().map(|(_, c)| *c));
        if total < HINT_MIN_EXAMPLES {
            return None;
        }
        let best = first_max(counts.iter().map(|(d, c)| (*d, c)))?;
        let best_count = counts.iter().find(|(d, _)| *d == best).map(|(_, c)| *c).unwrap_or(0.0);
        (best_count / total >= HINT_MIN_SHARE).then(|| best.clone())
    }

    fn why(
        &mut self,
        tuning: &Tuning,
        counts: &IndexMap<&str, u32>,
        best: &str,
        second: &str,
        reporter: &str,
    ) -> Vec<String> {
        let vocab = self.vocab_len().max(1) as f64;
        let m = &self.model;
        let s = m.scale;
        let empty = Table::new();
        let tok_best = m.tokens.get(best).unwrap_or(&empty);
        let tok_second = m.tokens.get(second).unwrap_or(&empty);
        let den_best = m.totals.get(best).copied().unwrap_or(0.0) * s + vocab;
        let den_second = m.totals.get(second).copied().unwrap_or(0.0) * s + vocab;
        let mut weighed: Vec<(f64, &str)> = Vec::new();
        for (t, k) in counts {
            let rate_best = (tok_best.get(*t).copied().unwrap_or(0.0) * s + 1.0) / den_best;
            let rate_second = (tok_second.get(*t).copied().unwrap_or(0.0) * s + 1.0) / den_second;
            let weight = f64::from(*k) * (rate_best / rate_second).ln();
            if weight > 0.0 {
                weighed.push((weight, t));
            }
        }
        weighed.sort_by(|a, b| (-a.0).partial_cmp(&-b.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut reasons: Vec<String> = weighed.iter().take(4).map(|(_, t)| t.replace(BIGRAM, " ")).collect();
        if !reporter.is_empty() {
            let rep_best = m.reporters.get(best).unwrap_or(&empty);
            let rep_second = m.reporters.get(second).unwrap_or(&empty);
            let seen =
                rep_best.get(reporter).copied().unwrap_or(0.0) + rep_second.get(reporter).copied().unwrap_or(0.0);
            if seen * s >= WHO_MIN_SEEN {
                let rep_smooth = tuning.get("repSmooth");
                let n_best = py_sum(rep_best.values().copied()) * s;
                let n_second = py_sum(rep_second.values().copied()) * s;
                let rate_best = (rep_best.get(reporter).copied().unwrap_or(0.0) * s + 1.0) / (n_best + rep_smooth);
                let rate_second =
                    (rep_second.get(reporter).copied().unwrap_or(0.0) * s + 1.0) / (n_second + rep_smooth);
                if rate_best >= rate_second * WHO_MIN_RATIO {
                    reasons.push(format!("usually raised by {reporter}"));
                }
            }
        }
        reasons
    }

    /// Score a closed candidate set against itself (escalation rerank).
    fn subset_scores(
        &self,
        tuning: &Tuning,
        counts: &IndexMap<&str, u32>,
        req_type: &str,
        reporter: &str,
        candidates: &[String],
        prior_weight: f64,
    ) -> Vec<(String, f64)> {
        let m = &self.model;
        let s = m.scale;
        let empty = Table::new();
        let total_examples = py_sum(candidates.iter().map(|d| m.priors[d]));
        let tables: Vec<&Table> = candidates.iter().map(|d| m.tokens.get(d).unwrap_or(&empty)).collect();
        let vocab = tables.iter().flat_map(|t| t.keys()).collect::<HashSet<_>>().len().max(1) as f64;
        let tot_all = py_sum(candidates.iter().map(|d| m.totals.get(d).copied().unwrap_or(0.0))) * s;
        let all_cnt: HashMap<&str, f64> = counts
            .keys()
            .map(|t| (*t, py_sum(tables.iter().map(|tbl| tbl.get(*t).copied().unwrap_or(0.0))) * s))
            .collect();
        let hint = self.request_type_hint(req_type);
        let rep_w = tuning.get("reporterWeight");
        let mut scores = Vec::with_capacity(candidates.len());
        for (d, table) in candidates.iter().zip(&tables) {
            let mut score = prior_weight * (m.priors[d] / total_examples).ln();
            let total = m.totals.get(d).copied().unwrap_or(0.0) * s;
            let den_in = total + vocab;
            let den_out = (tot_all - total) + vocab;
            for (token, reps) in counts {
                let inside = table.get(*token).copied().unwrap_or(0.0) * s;
                let p_in = (inside + 1.0) / den_in;
                let p_out = (all_cnt[token] - inside + 1.0) / den_out;
                score += f64::from(*reps) * (p_in / p_out).ln();
            }
            if !req_type.is_empty() {
                let rc = m.req_types.get(d).unwrap_or(&empty);
                let request_total = py_sum(rc.values().copied()) * s;
                score += ((rc.get(req_type).copied().unwrap_or(0.0) * s + 1.0)
                    / (request_total + tuning.get("reqSmooth")))
                .ln();
                if hint.as_deref() == Some(d.as_str()) {
                    score += tuning.get("requestTypeHintWeight");
                }
            }
            if !reporter.is_empty() {
                let rc = m.reporters.get(d).unwrap_or(&empty);
                let reporter_total = py_sum(rc.values().copied()) * s;
                score += rep_w
                    * ((rc.get(reporter).copied().unwrap_or(0.0) * s + 1.0)
                        / (reporter_total + tuning.get("repSmooth")))
                    .ln();
            }
            scores.push((d.clone(), score));
        }
        scores
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
        let ready: Vec<String> = self
            .model
            .priors
            .iter()
            .filter(|(d, prior)| self.model.seen.get(*d).copied().unwrap_or(**prior) >= MIN_EXAMPLES)
            .map(|(d, _)| d.clone())
            .collect();
        if ready.len() < 2 {
            return Prediction::cold();
        }
        let vocab_all =
            if ready.len() == self.model.tokens.len() { Some(self.vocab_len().max(1) as f64) } else { None };
        let m = &self.model;
        let s = m.scale;
        let empty = Table::new();
        let total_examples = py_sum(ready.iter().map(|d| m.priors[d]));
        let tok_tables: Vec<&Table> = ready.iter().map(|d| m.tokens.get(d).unwrap_or(&empty)).collect();
        let vocab = vocab_all
            .unwrap_or_else(|| tok_tables.iter().flat_map(|t| t.keys()).collect::<HashSet<_>>().len().max(1) as f64);

        let counts = counter(tokens);
        let log_odds = tuning.get("logOdds") != 0.0;
        let mut tot_all = 0.0;
        let mut all_cnt: HashMap<&str, f64> = HashMap::new();
        if log_odds {
            tot_all = py_sum(ready.iter().map(|d| m.totals.get(d).copied().unwrap_or(0.0))) * s;
            for t in counts.keys() {
                let mut c = 0.0_f64;
                for tbl in &tok_tables {
                    c += tbl.get(*t).copied().unwrap_or(0.0);
                }
                all_cnt.insert(t, c * s);
            }
        }
        let prior_w = tuning.get("priorWeight");
        let rep_w = tuning.get("reporterWeight");
        let hint_for_type = self.request_type_hint(req_type);
        let mut scores: Vec<(String, f64)> = Vec::with_capacity(ready.len());
        for (d, tok) in ready.iter().zip(&tok_tables) {
            let mut score = prior_w * (m.priors[d] / total_examples).ln();
            let tot = m.totals.get(d).copied().unwrap_or(0.0) * s;
            let den_in = tot + vocab;
            if log_odds {
                let den_out = (tot_all - tot) + vocab;
                for (t, k) in &counts {
                    let c_in = tok.get(*t).copied().unwrap_or(0.0) * s;
                    let p_in = (c_in + 1.0) / den_in;
                    let p_out = (all_cnt[t] - c_in + 1.0) / den_out;
                    score += f64::from(*k) * (p_in / p_out).ln();
                }
            } else {
                for (t, k) in &counts {
                    score += f64::from(*k) * ((tok.get(*t).copied().unwrap_or(0.0) * s + 1.0) / den_in).ln();
                }
            }
            if !req_type.is_empty() {
                let rt = m.req_types.get(d).unwrap_or(&empty);
                let n_rt = py_sum(rt.values().copied()) * s;
                score += ((rt.get(req_type).copied().unwrap_or(0.0) * s + 1.0) / (n_rt + tuning.get("reqSmooth"))).ln();
                if hint_for_type.as_deref() == Some(d.as_str()) {
                    score += tuning.get("requestTypeHintWeight");
                }
            }
            if !reporter.is_empty() {
                let rp = m.reporters.get(d).unwrap_or(&empty);
                let n_rp = py_sum(rp.values().copied()) * s;
                score += rep_w
                    * ((rp.get(reporter).copied().unwrap_or(0.0) * s + 1.0) / (n_rp + tuning.get("repSmooth"))).ln();
            }
            scores.push((d.clone(), score));
        }

        // Temperature-scaled softmax.
        let global_best = stable_argmax(&scores);
        let mx = scores[global_best].1;
        let temp = (tokens.len() as f64 / tuning.get("confTemp")).max(1.0);
        let exps: Vec<f64> = scores.iter().map(|(_, s)| ((s - mx) / temp).exp()).collect();
        let tot_e = py_sum(exps.iter().copied());
        let mut probs: Vec<(String, f64)> =
            scores.iter().zip(&exps).map(|((d, _), e)| (d.clone(), e / tot_e)).collect();
        let mut source = "model";

        let escalation: Vec<String> =
            self.escalation_dests.iter().filter(|d| scores.iter().any(|(s, _)| s == *d)).cloned().collect();
        if tuning.get("escalationSpecialist") != 0.0
            && escalation.contains(&scores[global_best].0)
            && escalation.len() >= 2
        {
            let prob_of =
                |probs: &[(String, f64)], d: &str| probs.iter().find(|(x, _)| x == d).map_or(0.0, |(_, p)| *p);
            let handoff_mass = py_sum(escalation.iter().map(|d| prob_of(&probs, d)));
            let esc_scores = self.subset_scores(
                tuning,
                &counts,
                req_type,
                reporter,
                &escalation,
                tuning.get("escalationPriorWeight"),
            );
            let esc_mx = esc_scores.iter().map(|(_, s)| *s).fold(f64::NEG_INFINITY, f64::max);
            let esc_exps: Vec<(String, f64)> =
                esc_scores.iter().map(|(d, s)| (d.clone(), ((s - esc_mx) / temp).exp())).collect();
            let esc_total_e = py_sum(esc_exps.iter().map(|(_, e)| *e));
            for d in &escalation {
                let e = esc_exps.iter().find(|(x, _)| x == d).map_or(0.0, |(_, e)| *e);
                if let Some(slot) = probs.iter_mut().find(|(x, _)| x == d) {
                    slot.1 = handoff_mass * e / esc_total_e;
                }
            }
            source = "escalation";
        }

        let factors = route_balance_factors(&self.model.route_balance, tuning.get("routeBalanceWeight"));
        if !factors.is_empty() {
            let adjusted: Vec<(String, f64)> =
                probs.iter().map(|(d, p)| (d.clone(), p * factors.get(d).copied().unwrap_or(1.0))).collect();
            let mass = py_sum(adjusted.iter().map(|(_, p)| *p));
            probs = adjusted.into_iter().map(|(d, p)| (d, p / mass)).collect();
        }
        let mut order: Vec<usize> = (0..probs.len()).collect();
        order.sort_by(|a, b| probs[*b].1.partial_cmp(&probs[*a].1).unwrap_or(std::cmp::Ordering::Equal));
        let mut best = probs[order[0]].0.clone();
        let mut second = probs[order[1]].0.clone();
        let mut conf = py_round_int(100.0 * probs[order[0]].1);
        let mut conf2 = py_round_int(100.0 * probs[order[1]].1);
        let mut note = self.plan_for(&best);
        let hint_dest = hint_for_type;
        let hint_conf = match &hint_dest {
            Some(h) => py_round_int(100.0 * probs.iter().find(|(d, _)| d == h).map_or(0.0, |(_, p)| *p)),
            None => 0,
        };

        let thresh = tuning.get("knnThresh");
        if let Some(exemplar) = exemplar.filter(|e| !e.is_empty())
            && thresh != 0.0
        {
            let (k_dest, sim) = self.nearest(exemplar);
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
                    note = format!("{} (matches a previous ticket)", self.plan_for(&best));
                }
            }
        }

        if source != "memory" && (factors.get(&best).copied().unwrap_or(1.0) - 1.0).abs() > 1e-6 {
            note.push_str("; ranking adjusted using earlier routing outcomes");
        }
        let hint_conflict = hint_dest.as_ref().is_some_and(|h| ready.contains(h) && &best != h);
        if hint_conflict {
            let h = hint_dest.clone().expect("conflict implies a hint");
            if second != h {
                second = h.clone();
                conf2 = hint_conf;
            } else {
                conf2 = conf2.max(hint_conf);
            }
            note.push_str(&format!("; review because Request Type points to {h}"));
        }
        let margin = conf - conf2;
        let mut review =
            (conf as f64) < tuning.get("reviewConf") || (margin as f64) < tuning.get("reviewMargin") || hint_conflict;
        if let Some(Value::Object(safety)) = self.route_safety.get(&best)
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
        let why = if second.is_empty() { vec![] } else { self.why(tuning, &counts, &best, &second, reporter) };
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
fn stable_argmax(scores: &[(String, f64)]) -> usize {
    let mut best = 0;
    for (i, (_, s)) in scores.iter().enumerate() {
        if *s > scores[best].1 {
            best = i;
        }
    }
    best
}
