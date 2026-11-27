//! Small Python-compatibility helpers shared by the UI-free core.

use std::path::Path;

use jo_core::config::Config;
use serde_json::Value;

/// `max(0, int(value or 0))`, with Python's `int()` semantics for the
/// shapes JSON can hold. Anything Python's `int()` would reject becomes 0.
pub fn count_value(value: Option<&Value>) -> u64 {
    let n = match value {
        None | Some(Value::Null) => 0,
        Some(Value::Bool(b)) => i64::from(*b),
        Some(Value::Number(n)) => {
            n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)).unwrap_or(0)
        }
        Some(Value::String(s)) => {
            let t = s.trim();
            let digits = t.trim_start_matches(['+', '-']);
            let well_formed = !digits.is_empty()
                && !digits.starts_with('_')
                && !digits.ends_with('_')
                && !digits.contains("__")
                && digits.chars().all(|c| c.is_ascii_digit() || c == '_');
            if well_formed { t.replace('_', "").parse().unwrap_or(0) } else { 0 }
        }
        Some(_) => 0,
    };
    n.max(0) as u64
}

/// Python `str(value)` for display (f-string interpolation).
pub fn py_str(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if n.is_f64() {
                let f = n.as_f64().unwrap_or(0.0);
                if f.fract() == 0.0 && f.abs() < 1e16 { format!("{f:.1}") } else { format!("{f}") }
            } else {
                n.to_string()
            }
        }
        other => other.to_string(),
    }
}

/// Python truthiness of a JSON value.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `cfg.get(key, default)` as an integer (normalised configs hold ints).
pub fn cfg_int(cfg: &Config, key: &str, default: i64) -> i64 {
    match cfg.get(key) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(default),
        Some(Value::Bool(b)) => i64::from(*b),
        _ => default,
    }
}

/// A saved coordinate as an integer (0 for anything unusable).
pub fn cfg_int_value(value: &Value) -> i32 {
    let n = match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f as i64)).unwrap_or(0),
        Value::Bool(b) => i64::from(*b),
        _ => 0,
    };
    n.clamp(i64::from(i32::MIN / 2), i64::from(i32::MAX / 2)) as i32
}

/// `cfg.get(key, default)` as a float.
pub fn cfg_float(cfg: &Config, key: &str, default: f64) -> f64 {
    match cfg.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(default),
        _ => default,
    }
}

/// The strings in a list-valued setting (non-strings are ignored).
pub fn cfg_strings(cfg: &Config, key: &str) -> Vec<String> {
    match cfg.get(key) {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
        _ => Vec::new(),
    }
}

/// Configured alert queue names (Python `_alert_queues`), de-duplicated in order.
pub fn alert_queues(cfg: &Config) -> Vec<String> {
    let mut names = cfg_strings(cfg, "alertQueues");
    if !truthy(cfg.get("alertQueues")) {
        names = match cfg.get("alertQueue").and_then(Value::as_str) {
            Some(single) if !single.is_empty() => vec![single.to_owned()],
            _ => Vec::new(),
        };
    }
    let mut out: Vec<String> = Vec::new();
    for name in names {
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// A JSON scalar id (`"12"` or `12`) as text.
pub fn id_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other @ Value::Bool(_)) => py_str(other),
        _ => String::new(),
    }
}

/// First `n` characters (Python `s[:n]`).
pub fn take_chars(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// Python `round(x)` on a float: half to even.
pub fn py_round(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// `os.path.basename` on Windows: the part after the last `\` or `/`.
pub fn basename(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or("")
}

/// Append one block to `jira_overlay_error.log` (5 MiB, one `.old` generation).
pub fn log_error(root: &Path, detail: &str) {
    jo_core::errlog::append_block(&root.join(ERROR_LOG), detail, ERROR_LOG_MAX);
}

pub const ERROR_LOG: &str = "jira_overlay_error.log";
pub const ERROR_LOG_MAX: u64 = 5 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn counts_follow_python_int() {
        for (raw, want) in [
            (json!(null), 0),
            (json!("12"), 12),
            (json!(3.7), 3),
            (json!(-5), 0),
            (json!("many"), 0),
            (json!(true), 1),
            (json!([1]), 0),
        ] {
            assert_eq!(count_value(Some(&raw)), want, "{raw}");
        }
        assert_eq!(count_value(None), 0);
    }

    #[test]
    fn rounding_is_bankers() {
        assert_eq!(py_round(92.5), 92);
        assert_eq!(py_round(93.5), 94);
        assert_eq!(py_round(80.0), 80);
    }

    #[test]
    fn basenames() {
        assert_eq!(basename(r"..\..\x.json"), "x.json");
        assert_eq!(basename("a/b.json"), "b.json");
        assert_eq!(basename("plain.json"), "plain.json");
    }

    #[test]
    fn alert_queue_names() {
        let cfg = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(alert_queues(&cfg(json!({"alertQueues": ["A", "", "A", "B"]}))), ["A", "B"]);
        assert_eq!(alert_queues(&cfg(json!({"alertQueues": [], "alertQueue": "Old"}))), ["Old"]);
        assert!(alert_queues(&cfg(json!({}))).is_empty());
    }
}
