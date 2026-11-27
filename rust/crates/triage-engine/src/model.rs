//! The learned model as stored in `triage_state.json["model"]`.
//!
//! Decayed counts are stored pre-divided by `scale` (true value = stored *
//! scale), exactly as the Python learner writes them. Loading is defensive:
//! an unusable section is rebuilt empty rather than taking the learner down.

use indexmap::IndexMap;
use serde_json::{Map, Value};

pub type Table = IndexMap<String, f64>;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RouteBalance {
    pub actual: Table,
    pub predicted: Table,
    pub observations: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub priors: Table,
    pub tokens: IndexMap<String, Table>,
    pub totals: Table,
    pub req_types: IndexMap<String, Table>,
    pub reporters: IndexMap<String, Table>,
    pub clone_projects: IndexMap<String, Table>,
    pub seen: Table,
    /// `[sorted unique title tokens, dest]`, oldest first.
    pub exemplars: Vec<(Vec<String>, String)>,
    pub route_balance: RouteBalance,
    pub scale: f64,
    /// Keys this build does not know, preserved on save.
    pub extra: Map<String, Value>,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            priors: Table::new(),
            tokens: IndexMap::new(),
            totals: Table::new(),
            req_types: IndexMap::new(),
            reporters: IndexMap::new(),
            clone_projects: IndexMap::new(),
            seen: Table::new(),
            exemplars: Vec::new(),
            route_balance: RouteBalance::default(),
            scale: 1.0,
            extra: Map::new(),
        }
    }
}

const KNOWN: [&str; 10] = [
    "priors",
    "tokens",
    "totals",
    "reqTypes",
    "reporters",
    "cloneProjects",
    "seen",
    "exemplars",
    "routeBalance",
    "scale",
];

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    }
}

fn table(v: Option<&Value>) -> Table {
    match v {
        Some(Value::Object(map)) => map.iter().filter_map(|(k, v)| number(v).map(|n| (k.clone(), n))).collect(),
        _ => Table::new(),
    }
}

fn nested(v: Option<&Value>) -> IndexMap<String, Table> {
    match v {
        Some(Value::Object(map)) => map.iter().map(|(k, v)| (k.clone(), table(Some(v)))).collect(),
        _ => IndexMap::new(),
    }
}

fn balance_table(v: Option<&Value>) -> Table {
    table(v).into_iter().filter(|(_, n)| n.is_finite() && *n >= 0.0).collect()
}

impl Model {
    pub fn from_value(value: &Value) -> Self {
        let Value::Object(m) = value else { return Self::default() };
        let exemplars = match m.get("exemplars") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|item| {
                    let pair = item.as_array()?;
                    let tokens = pair
                        .first()?
                        .as_array()?
                        .iter()
                        .map(|t| t.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()?;
                    let dest = pair.get(1)?.as_str()?.to_owned();
                    Some((tokens, dest))
                })
                .collect(),
            _ => Vec::new(),
        };
        let balance = match m.get("routeBalance") {
            Some(Value::Object(b)) => {
                let obs = b.get("observations").and_then(number).filter(|n| n.is_finite() && *n >= 0.0).unwrap_or(0.0);
                RouteBalance {
                    actual: balance_table(b.get("actual")),
                    predicted: balance_table(b.get("predicted")),
                    observations: obs,
                }
            }
            _ => RouteBalance::default(),
        };
        let scale = m.get("scale").and_then(number).filter(|s| *s > 0.0).unwrap_or(1.0);
        Self {
            priors: table(m.get("priors")),
            tokens: nested(m.get("tokens")),
            totals: table(m.get("totals")),
            req_types: nested(m.get("reqTypes")),
            reporters: nested(m.get("reporters")),
            clone_projects: nested(m.get("cloneProjects")),
            seen: table(m.get("seen")),
            exemplars,
            route_balance: balance,
            scale,
            extra: m
                .iter()
                .filter(|(k, _)| !KNOWN.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }

    pub fn to_value(&self) -> Value {
        let floats = |t: &Table| Value::Object(t.iter().map(|(k, v)| (k.clone(), Value::from(*v))).collect());
        let ints = |t: &Table| Value::Object(t.iter().map(|(k, v)| (k.clone(), int_or_float(*v))).collect());
        let nested = |n: &IndexMap<String, Table>, f: &dyn Fn(&Table) -> Value| {
            Value::Object(n.iter().map(|(k, t)| (k.clone(), f(t))).collect())
        };
        let mut out = Map::new();
        out.insert("priors".into(), floats(&self.priors));
        out.insert("tokens".into(), nested(&self.tokens, &floats));
        out.insert("totals".into(), floats(&self.totals));
        out.insert("reqTypes".into(), nested(&self.req_types, &floats));
        out.insert("reporters".into(), nested(&self.reporters, &floats));
        out.insert("cloneProjects".into(), nested(&self.clone_projects, &ints));
        out.insert("seen".into(), ints(&self.seen));
        out.insert(
            "exemplars".into(),
            Value::Array(
                self.exemplars
                    .iter()
                    .map(|(t, d)| Value::Array(vec![Value::from(t.clone()), Value::from(d.clone())]))
                    .collect(),
            ),
        );
        let mut balance = Map::new();
        // Python stores a route's first count as the int 1 until it decays.
        balance.insert("actual".into(), ints(&self.route_balance.actual));
        balance.insert("predicted".into(), ints(&self.route_balance.predicted));
        balance.insert("observations".into(), int_or_float(self.route_balance.observations));
        out.insert("routeBalance".into(), Value::Object(balance));
        out.insert("scale".into(), Value::from(self.scale));
        out.extend(self.extra.clone());
        Value::Object(out)
    }
}

/// Integral values as JSON integers (Python's `int` counters).
pub fn int_or_float(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.0e15 { Value::from(v as i64) } else { Value::from(v) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn damaged_sections_load_empty_and_round_trip() {
        let m = Model::from_value(&json!({
            "priors": {"A": 10.5, "B": "bad"}, "tokens": null, "totals": [], "seen": {"A": 9},
            "exemplars": [[["a", "b"], "A"], "junk", [[1], "B"]],
            "routeBalance": {"actual": {"A": 3, "B": -1, "C": "inf"}, "predicted": 5, "observations": -2},
            "scale": 0, "custom": [1]
        }));
        assert_eq!(m.priors.len(), 1);
        assert!(m.tokens.is_empty() && m.totals.is_empty());
        assert_eq!(m.exemplars, vec![(vec!["a".to_owned(), "b".to_owned()], "A".to_owned())]);
        assert_eq!(m.route_balance.actual.len(), 1);
        assert_eq!(m.route_balance.observations, 0.0);
        assert_eq!(m.scale, 1.0);
        let again = Model::from_value(&m.to_value());
        assert_eq!(again, m);
        assert_eq!(m.to_value()["seen"]["A"], json!(9));
        assert_eq!(m.to_value()["custom"], json!([1]));
    }
}
