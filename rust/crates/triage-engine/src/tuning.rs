//! Learning knobs (Python's module-level `TUNING`), passed explicitly.

use indexmap::IndexMap;
use serde_json::{Map, Value};

/// Every knob the learner reads. Values are `f64`; Python's int/float
/// distinction only matters when writing them back out (see [`Tuning::to_json`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Tuning {
    values: IndexMap<&'static str, f64>,
}

/// (name, default, written as integer)
const KNOBS: [(&str, f64, bool); 16] = [
    ("routeBalanceWeight", 0.0, false),
    ("summaryWeight", 6.0, true),
    ("mistakeWeight", 3.0, true),
    ("confTemp", 4.0, false),
    ("reqSmooth", 10.0, true),
    ("repSmooth", 20.0, true),
    ("reporterWeight", 1.0, false),
    ("escalationSpecialist", 0.0, true),
    ("escalationPriorWeight", 0.25, false),
    ("logOdds", 1.0, true),
    ("priorWeight", 1.0, false),
    ("knnThresh", 0.7, false),
    ("reviewConf", 75.0, true),
    ("reviewMargin", 20.0, true),
    ("requestTypeHintWeight", 3.0, false),
    ("decayFactor", 0.997, false),
];

/// The values the nightly tune may explore, in search order.
pub const TUNE_AXES: [(&str, &[f64]); 13] = [
    ("routeBalanceWeight", &[0.0, 0.5, 1.0, 2.0]),
    ("summaryWeight", &[3.0, 4.0, 5.0, 6.0, 8.0, 10.0]),
    ("mistakeWeight", &[1.0, 2.0, 3.0, 4.0]),
    ("decayFactor", &[1.0, 0.999, 0.997, 0.995, 0.99]),
    ("confTemp", &[2.0, 3.0, 4.0, 6.0, 8.0]),
    ("priorWeight", &[0.0, 0.25, 0.5, 0.75, 1.0]),
    ("knnThresh", &[0.0, 0.6, 0.7, 0.8, 0.9]),
    ("requestTypeHintWeight", &[0.0, 1.5, 3.0, 5.0]),
    ("reporterWeight", &[0.0, 1.0, 2.0, 3.0, 5.0]),
    ("escalationSpecialist", &[0.0, 1.0]),
    ("escalationPriorWeight", &[0.0, 0.25, 0.5, 0.75, 1.0]),
    ("reviewConf", &[65.0, 70.0, 75.0, 80.0, 85.0]),
    ("reviewMargin", &[10.0, 15.0, 20.0, 25.0]),
];

/// `BALANCED_TUNED_CONFIG`: the offline-validated baseline.
pub const BALANCED: [(&str, f64); 13] = [
    ("routeBalanceWeight", 0.0),
    ("summaryWeight", 6.0),
    ("mistakeWeight", 2.0),
    ("decayFactor", 0.997),
    ("confTemp", 4.0),
    ("reviewConf", 75.0),
    ("reviewMargin", 20.0),
    ("knnThresh", 0.7),
    ("priorWeight", 0.5),
    ("requestTypeHintWeight", 3.0),
    ("reporterWeight", 1.0),
    ("escalationSpecialist", 0.0),
    ("escalationPriorWeight", 0.25),
];

impl Default for Tuning {
    fn default() -> Self {
        Self { values: KNOBS.iter().map(|(k, v, _)| (*k, *v)).collect() }
    }
}

impl Tuning {
    pub fn get(&self, name: &str) -> f64 {
        self.values.get(name).copied().unwrap_or_else(|| panic!("unknown tuning knob {name}"))
    }

    pub fn set(&mut self, name: &str, value: f64) {
        let key = KNOBS.iter().find(|(k, _, _)| *k == name).map(|(k, _, _)| *k);
        if let Some(key) = key {
            self.values.insert(key, value);
        }
    }

    /// `TUNING.update(cfg)` for the knobs this build knows; numbers only.
    pub fn update_from(&mut self, cfg: &Map<String, Value>) {
        for (k, v) in cfg {
            let number = match v {
                Value::Number(n) => n.as_f64(),
                Value::Bool(b) => Some(f64::from(u8::from(*b))),
                _ => None,
            };
            if let Some(n) = number {
                self.set(k, n);
            }
        }
    }

    /// A copy with `overrides` applied (replay's `TUNING.update(cfg)`).
    pub fn with(&self, overrides: &[(&str, f64)]) -> Self {
        let mut out = self.clone();
        for (k, v) in overrides {
            out.set(k, *v);
        }
        out
    }

    /// The searched knobs, in axis order (`_TUNE_KEEP`).
    pub fn keep(&self) -> Vec<(&'static str, f64)> {
        TUNE_AXES.iter().map(|(k, _)| (*k, self.get(k))).collect()
    }

    /// Searched knobs as JSON, integer-typed knobs written as integers.
    pub fn keep_json(&self) -> Map<String, Value> {
        self.keep().into_iter().map(|(k, v)| (k.to_owned(), knob_value(k, v))).collect()
    }

    pub fn balanced() -> Vec<(&'static str, f64)> {
        BALANCED.to_vec()
    }
}

/// JSON number for a knob, matching the Python types (ints stay ints).
pub fn knob_value(name: &str, value: f64) -> Value {
    let integer = KNOBS.iter().any(|(k, _, int)| *k == name && *int);
    if integer && value.fract() == 0.0 && value.abs() < 9.0e15 { Value::from(value as i64) } else { Value::from(value) }
}
