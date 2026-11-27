//! Live coverage and calibration folded incrementally from the append-only
//! `triage_log.jsonl` (Python `_reset_metrics` / `_live_metrics`).

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{Duration, Local};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::pyfmt::{py_str, truthy};
use crate::support::{Counts, coverage_reason, segment_name};

/// Unresolved intake predictions kept for context.
pub const PENDING_CAP: usize = 4000;
/// Routed keys remembered to recognise repeat transitions.
pub const ROUTED_KEYS_CAP: usize = 8000;

/// An insertion-ordered map that only ever drops its oldest entry, with
/// Python dict semantics (re-assigning a key keeps its position). O(1).
#[derive(Clone, Debug, Default)]
pub struct Fifo<V> {
    entries: VecDeque<(String, V)>,
    index: HashMap<String, u64>,
    base: u64,
}

impl<V> Fifo<V> {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }

    pub fn get(&self, key: &str) -> Option<&V> {
        self.index.get(key).map(|pos| &self.entries[(pos - self.base) as usize].1)
    }

    pub fn insert(&mut self, key: String, value: V) {
        if let Some(pos) = self.index.get(&key) {
            self.entries[(pos - self.base) as usize].1 = value;
        } else {
            self.index.insert(key.clone(), self.base + self.entries.len() as u64);
            self.entries.push_back((key, value));
        }
    }

    pub fn pop_oldest(&mut self) {
        if let Some((key, _)) = self.entries.pop_front() {
            self.index.remove(&key);
            self.base += 1;
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }
}

fn count(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => n.as_u64().or_else(|| n.as_f64().map(|f| f.max(0.0) as u64)).unwrap_or(0),
        Some(Value::Bool(b)) => u64::from(*b),
        _ => 0,
    }
}

fn pair(v: Option<&Value>) -> [u64; 2] {
    match v.and_then(Value::as_array) {
        Some(a) => [count(a.first()), count(a.get(1))],
        None => [0, 0],
    }
}

fn counts(v: Option<&Value>) -> Counts {
    v.and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), pair(Some(v)))).collect())
        .unwrap_or_default()
}

fn counts_json(c: &Counts) -> Value {
    Value::Object(c.iter().map(|(k, v)| (k.clone(), json!(v))).collect())
}

fn bump(table: &mut Counts, key: String, hit: u64) {
    let row = table.entry(key).or_insert([0, 0]);
    row[0] += 1;
    row[1] += hit;
}

/// The cumulative metrics dict (`_metrics`).
#[derive(Clone, Debug, Default)]
pub struct LiveMetrics {
    pub routed: u64,
    pub predicted: u64,
    pub correct: u64,
    pub confident: [u64; 2],
    pub review: [u64; 2],
    pub unknown: Option<[u64; 2]>,
    pub by_actual: Counts,
    pub by_predicted: Counts,
    pub confident_by_actual: Counts,
    pub confident_by_predicted: Counts,
    pub recent_events: Vec<Map<String, Value>>,
    pub coverage_reasons: IndexMap<String, u64>,
    pub routed_keys: Fifo<String>,
    pub by_source: Counts,
    pub with_request_type: u64,
    pub segments: Option<Counts>,
    /// Keys this build does not know, kept when re-saved.
    pub extra: Map<String, Value>,
}

const KNOWN: [&str; 16] = [
    "routed",
    "predicted",
    "correct",
    "confident",
    "review",
    "unknown",
    "byActual",
    "byPredicted",
    "confidentByActual",
    "confidentByPredicted",
    "recentEvents",
    "coverageReasons",
    "routedKeys",
    "bySource",
    "withRequestType",
    "segments",
];

impl LiveMetrics {
    pub fn band(&self, name: &str) -> Option<[u64; 2]> {
        match name {
            "confident" => Some(self.confident),
            "review" => Some(self.review),
            "unknown" => self.unknown,
            _ => None,
        }
    }

    fn band_mut(&mut self, name: &str) -> &mut [u64; 2] {
        match name {
            "confident" => &mut self.confident,
            "review" => &mut self.review,
            _ => self.unknown.get_or_insert([0, 0]),
        }
    }

    pub fn from_value(v: &Map<String, Value>) -> Self {
        let mut routed_keys = Fifo::default();
        if let Some(Value::Object(m)) = v.get("routedKeys") {
            for (k, stamp) in m {
                routed_keys.insert(k.clone(), py_str(stamp));
            }
        }
        Self {
            routed: count(v.get("routed")),
            predicted: count(v.get("predicted")),
            correct: count(v.get("correct")),
            confident: pair(v.get("confident")),
            review: pair(v.get("review")),
            unknown: v.get("unknown").map(|u| pair(Some(u))),
            by_actual: counts(v.get("byActual")),
            by_predicted: counts(v.get("byPredicted")),
            confident_by_actual: counts(v.get("confidentByActual")),
            confident_by_predicted: counts(v.get("confidentByPredicted")),
            recent_events: match v.get("recentEvents") {
                Some(Value::Array(items)) => items.iter().filter_map(Value::as_object).cloned().collect(),
                _ => Vec::new(),
            },
            coverage_reasons: v
                .get("coverageReasons")
                .and_then(Value::as_object)
                .map(|m| m.iter().map(|(k, n)| (k.clone(), count(Some(n)))).collect())
                .unwrap_or_default(),
            routed_keys,
            by_source: counts(v.get("bySource")),
            with_request_type: count(v.get("withRequestType")),
            segments: v.get("segments").map(|s| counts(Some(s))),
            extra: v
                .iter()
                .filter(|(k, _)| !KNOWN.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }

    pub fn to_value(&self) -> Value {
        let mut out = Map::new();
        out.insert("routed".into(), json!(self.routed));
        out.insert("predicted".into(), json!(self.predicted));
        out.insert("correct".into(), json!(self.correct));
        out.insert("confident".into(), json!(self.confident));
        out.insert("review".into(), json!(self.review));
        out.insert("byActual".into(), counts_json(&self.by_actual));
        out.insert("byPredicted".into(), counts_json(&self.by_predicted));
        out.insert("confidentByActual".into(), counts_json(&self.confident_by_actual));
        out.insert("confidentByPredicted".into(), counts_json(&self.confident_by_predicted));
        out.insert(
            "recentEvents".into(),
            Value::Array(self.recent_events.iter().cloned().map(Value::Object).collect()),
        );
        out.insert("coverageReasons".into(), json!(self.coverage_reasons));
        out.insert(
            "routedKeys".into(),
            Value::Object(self.routed_keys.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect()),
        );
        out.insert("bySource".into(), counts_json(&self.by_source));
        out.insert("withRequestType".into(), json!(self.with_request_type));
        if let Some(s) = &self.segments {
            out.insert("segments".into(), counts_json(s));
        }
        if let Some(u) = self.unknown {
            out.insert("unknown".into(), json!(u));
        }
        out.extend(self.extra.clone());
        Value::Object(out)
    }

    /// Fold complete log lines into the running totals.
    pub fn fold(
        &mut self,
        pending: &mut Fifo<Map<String, Value>>,
        text: &str,
        cutoff: &str,
        canonical: &dyn Fn(&str) -> String,
    ) {
        for raw in text.lines() {
            if raw.trim().is_empty() {
                continue;
            }
            let Ok(Value::Object(rec)) = serde_json::from_str::<Value>(raw) else { continue };
            let key = rec.get("key").map(py_str).unwrap_or_default();
            let event = rec.get("event").and_then(Value::as_str);
            if event == Some("would_have_done") {
                pending.insert(key, rec);
                while pending.len() > PENDING_CAP {
                    pending.pop_oldest();
                }
                continue;
            }
            if event != Some("routed") {
                continue;
            }
            self.routed += 1;
            let context: Map<String, Value> = if rec.get("review").is_some_and(|r| !r.is_null()) {
                rec.clone()
            } else {
                pending.get(&key).cloned().unwrap_or_default()
            };
            let reason = coverage_reason(&rec, pending.get(&key), self.routed_keys.contains(&key));
            *self.coverage_reasons.entry(reason.to_owned()).or_insert(0) += 1;
            let stamp = match rec.get("ts") {
                Some(v) if triage_engine::text::truthy(v) => py_str(v),
                _ => String::new(),
            };
            self.routed_keys.insert(key.clone(), stamp.clone());
            if self.routed_keys.len() > ROUTED_KEYS_CAP {
                self.routed_keys.pop_oldest();
            }
            if stamp.as_str() >= cutoff {
                let mut compact = Map::new();
                for k in ["ts", "predicted", "dest", "predictionCorrect", "requestType"] {
                    compact.insert(k.into(), rec.get(k).cloned().unwrap_or(Value::Null));
                }
                compact.insert("coverageReason".into(), json!(reason));
                compact.insert("ts".into(), json!(stamp));
                compact.insert("review".into(), context.get("review").cloned().unwrap_or(Value::Null));
                let dest = match compact.get("dest") {
                    Some(v) if triage_engine::text::truthy(v) => py_str(v),
                    _ => "unknown".into(),
                };
                compact.insert("dest".into(), json!(canonical(&dest)));
                if let Some(p) = compact.get("predicted").filter(|p| !p.is_null()) {
                    let p = canonical(&py_str(p));
                    compact.insert("predicted".into(), json!(p));
                }
                self.recent_events.push(compact);
            }
            if truthy(rec.get("requestType")) {
                self.with_request_type += 1;
            }
            let Some(predicted) = rec.get("predicted").filter(|p| !p.is_null()) else { continue };
            self.predicted += 1;
            let hit = u64::from(truthy(rec.get("predictionCorrect")));
            self.correct += hit;
            bump(self.segments.get_or_insert_with(Counts::new), segment_name(rec.get("requestType")), hit);
            let actual = canonical(&rec.get("dest").map(py_str).unwrap_or_else(|| "unknown".into()));
            let predicted = canonical(&py_str(predicted));
            bump(&mut self.by_actual, actual.clone(), hit);
            bump(&mut self.by_predicted, predicted.clone(), hit);
            let band = match context.get("review") {
                None | Some(Value::Null) => "unknown",
                Some(v) if triage_engine::text::truthy(v) => "review",
                Some(_) => "confident",
            };
            self.unknown.get_or_insert([0, 0]);
            let row = self.band_mut(band);
            row[0] += 1;
            row[1] += hit;
            if band == "confident" {
                bump(&mut self.confident_by_actual, actual, hit);
                bump(&mut self.confident_by_predicted, predicted, hit);
            }
            if let Some(source) = context.get("source").filter(|s| triage_engine::text::truthy(s)) {
                bump(&mut self.by_source, py_str(source), hit);
            }
        }
    }
}

/// The incremental reader: totals, the byte offset folded so far, and the
/// intake predictions still waiting for an outcome.
#[derive(Debug)]
pub struct MetricsCache {
    log: PathBuf,
    pub metrics: Option<LiveMetrics>,
    pub offset: u64,
    pub pending: Fifo<Map<String, Value>>,
}

pub fn checkpoint_path(log: &Path) -> PathBuf {
    let mut p = log.as_os_str().to_owned();
    p.push(".metrics.json");
    PathBuf::from(p)
}

impl MetricsCache {
    pub fn new(log: &Path) -> Self {
        Self { log: log.to_path_buf(), metrics: None, offset: 0, pending: Fifo::default() }
    }

    /// Start over, resuming from the rotation checkpoint when its prefix
    /// fingerprint still matches the live log.
    pub fn reset(&mut self) {
        self.metrics = Some(LiveMetrics::default());
        self.offset = 0;
        self.pending = Fifo::default();
        let Ok(text) = fs::read(checkpoint_path(&self.log)) else { return };
        let Ok(Value::Object(saved)) = serde_json::from_slice::<Value>(&text) else { return };
        let Some(Value::Object(metrics)) = saved.get("metrics") else { return };
        self.metrics = Some(LiveMetrics::from_value(metrics));
        if let Some(Value::Object(pending)) = saved.get("pending") {
            for (k, v) in pending {
                if let Value::Object(rec) = v {
                    self.pending.insert(k.clone(), rec.clone());
                }
            }
        }
        let (Some(size), Some(hash)) =
            (saved.get("prefixSize").and_then(Value::as_u64), saved.get("prefixHash").and_then(Value::as_str))
        else {
            return;
        };
        let Ok(mut file) = fs::File::open(&self.log) else { return };
        let mut prefix = Vec::new();
        if file.by_ref().take(size).read_to_end(&mut prefix).is_err() {
            return;
        }
        if prefix.len() as u64 == size && hex::encode(Sha256::digest(&prefix)) == hash {
            self.offset = size;
        }
    }

    /// Fold whatever is new since the last call.
    pub fn live(&mut self, canonical: &dyn Fn(&str) -> String) -> &LiveMetrics {
        if self.metrics.is_none() {
            self.reset();
        }
        let cutoff = (Local::now().naive_local() - Duration::days(30)).format("%Y-%m-%d %H:%M:%S").to_string();
        let expire =
            |m: &mut LiveMetrics| m.recent_events.retain(|r| r.get("ts").map(py_str).unwrap_or_default() >= cutoff);
        expire(self.metrics.as_mut().expect("reset above"));
        let Ok(size) = fs::metadata(&self.log).map(|m| m.len()) else { return self.metrics.as_ref().expect("set") };
        if size < self.offset {
            // Truncated or replaced (rotation): restart.
            self.reset();
            expire(self.metrics.as_mut().expect("reset above"));
        }
        if size == self.offset {
            return self.metrics.as_ref().expect("set");
        }
        let mut data = Vec::new();
        let read = fs::File::open(&self.log).and_then(|mut f| {
            f.seek(SeekFrom::Start(self.offset))?;
            f.read_to_end(&mut data)
        });
        if read.is_err() {
            return self.metrics.as_ref().expect("set");
        }
        // Ignore a half-written last line.
        let cut = data.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        if cut == 0 {
            return self.metrics.as_ref().expect("set");
        }
        self.offset += cut as u64;
        let text = String::from_utf8_lossy(&data[..cut]);
        let metrics = self.metrics.as_mut().expect("set");
        metrics.fold(&mut self.pending, &text, &cutoff, canonical);
        self.metrics.as_ref().expect("set")
    }

    pub fn pending_json(&self) -> Value {
        Value::Object(self.pending.iter().map(|(k, v)| (k.clone(), Value::Object(v.clone()))).collect())
    }

    pub fn log(&self) -> &Path {
        &self.log
    }
}
