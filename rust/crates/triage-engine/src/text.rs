//! Turning Jira tickets into model features.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::Value;

use crate::model::FastState;

static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z][a-z0-9']{2,}").expect("valid regex"));
/// Case-reference codes ("hcw11593772keo", "tc25000123") collapse to one token.
static REFERENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z]{1,6}[0-9]{4,}[a-z0-9_]*$").expect("valid regex"));

pub const STOPWORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "from",
    "this",
    "that",
    "have",
    "has",
    "are",
    "was",
    "were",
    "been",
    "being",
    "will",
    "would",
    "should",
    "can",
    "could",
    "not",
    "but",
    "all",
    "any",
    "our",
    "your",
    "their",
    "there",
    "here",
    "when",
    "where",
    "what",
    "which",
    "who",
    "how",
    "please",
    "thanks",
    "thank",
    "hi",
    "hello",
    "regards",
    "kind",
    "need",
    "needs",
    "needed",
    "required",
    "request",
    "requested",
    "you",
    "its",
    "it's",
    "into",
    "onto",
    "per",
    "via",
    "due",
    "new",
];

/// Separator between the two words of a bigram feature (U+00B7).
pub const BIGRAM: char = '·';

/// Collapse an Atlassian Document Format tree to plain text.
pub fn flatten_adf(node: &Value) -> String {
    match node {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Object(map) => {
            let mut parts = Vec::new();
            if let Some(text) = map.get("text").filter(|t| truthy(t)) {
                parts.push(match text {
                    Value::String(s) => s.clone(),
                    other => py_str(other),
                });
            }
            if let Some(Value::Array(children)) = map.get("content") {
                parts.extend(children.iter().map(flatten_adf));
            }
            join_nonempty(parts)
        }
        Value::Array(items) => join_nonempty(items.iter().map(flatten_adf).collect()),
        _ => String::new(),
    }
}

fn join_nonempty(parts: Vec<String>) -> String {
    parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ")
}

/// Python truthiness of a JSON value.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `str()` of a JSON scalar.
pub fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        other => other.to_string(),
    }
}

/// Human-readable names from a Jira custom-field value.
pub fn field_names(value: &Value) -> Vec<String> {
    if !truthy(value) {
        return Vec::new();
    }
    match value {
        Value::Array(items) => items.iter().flat_map(field_names).collect(),
        Value::Object(map) => ["name", "value", "displayName"]
            .iter()
            .find_map(|k| map.get(*k).filter(|v| truthy(v)).map(py_str))
            .into_iter()
            .collect(),
        Value::String(s) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// The JSM request type's name from the request-type field value.
pub fn request_type_name(value: &Value) -> String {
    let Value::Object(map) = value else { return String::new() };
    let from_type = map.get("requestType").and_then(|rt| rt.get("name")).filter(|n| truthy(n));
    match from_type.or_else(|| map.get("name").filter(|n| truthy(n))) {
        Some(name) => py_str(name),
        None => String::new(),
    }
}

/// Lower-cased words of 3+ characters, stopwords dropped, reference codes
/// collapsed to `#ref`.
pub fn tokenize(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    WORD.find_iter(&lower)
        .map(|m| m.as_str())
        .filter(|w| !STOPWORDS.contains(w))
        .map(|w| if REFERENCE.is_match(w) { "#ref".to_owned() } else { w.to_owned() })
        .collect()
}

/// Text fields a feature vector is built from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TicketText {
    pub summary: String,
    pub desc: String,
    pub labels: Vec<String>,
    pub components: Vec<String>,
    pub structured: Vec<String>,
}

fn bigrams(tokens: &[String]) -> Vec<String> {
    tokens.windows(2).map(|w| format!("{}{BIGRAM}{}", w[0], w[1])).collect()
}

/// Unigrams + bigrams. Summary features repeat `summary_weight` times;
/// description features are deduplicated and sorted.
pub fn features_for(ticket: &TicketText, request_type: &str, summary_weight: f64) -> Vec<String> {
    let (summary, body) = feature_parts(ticket, request_type);
    let repeat = summary_repeat(summary_weight);
    let mut out = Vec::with_capacity(summary.len() * repeat + body.len());
    for _ in 0..repeat {
        out.extend(summary.iter().cloned());
    }
    out.extend(body);
    out
}

/// How many times summary features repeat for a `summaryWeight`.
pub fn summary_repeat(summary_weight: f64) -> usize {
    summary_weight.trunc().max(0.0) as usize
}

/// The two halves of [`features_for`]: summary unigrams + bigrams (repeated
/// `summaryWeight` times there), and the sorted, deduplicated body features.
pub fn feature_parts(ticket: &TicketText, request_type: &str) -> (Vec<String>, Vec<String>) {
    let summary_tokens = tokenize(&format!("{} {}", ticket.summary, request_type));
    let body = [ticket.desc.clone(), ticket.labels.join(" "), ticket.components.join(" "), ticket.structured.join(" ")]
        .join(" ");
    let body_tokens = tokenize(&body);
    let mut summary = summary_tokens.clone();
    summary.extend(bigrams(&summary_tokens));
    let body_set: BTreeSet<String> = body_tokens.iter().cloned().chain(bigrams(&body_tokens)).collect();
    (summary, body_set.into_iter().collect())
}

/// `collections.Counter(tokens)`: counts in first-occurrence order.
pub fn counter(tokens: &[String]) -> IndexMap<&str, u32, FastState> {
    let mut counts: IndexMap<&str, u32, FastState> = IndexMap::default();
    for t in tokens {
        *counts.entry(t.as_str()).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tokenize_rules() {
        assert_eq!(
            tokenize("Hi, PLEASE reset it's password for HCW11593772KEO and tc25000123 ab1234's"),
            ["reset", "password", "#ref", "#ref", "ab1234's"]
        );
        assert_eq!(tokenize("x1 ab a1b2c3 \u{212A}elvin"), ["a1b2c3", "kelvin"]);
    }

    #[test]
    fn features_layout() {
        let t = TicketText {
            summary: "Printer jammed again".into(),
            desc: "paper jammed in tray".into(),
            labels: vec!["hardware".into()],
            components: vec![],
            structured: vec!["customfield_1 Office".into()],
        };
        let f = features_for(&t, "", 2.0);
        assert_eq!(&f[..5], ["printer", "jammed", "again", "printer·jammed", "jammed·again"]);
        assert_eq!(&f[5..10], &f[..5]);
        let body = &f[10..];
        let mut sorted = body.to_vec();
        sorted.sort();
        assert_eq!(body, sorted.as_slice());
        assert!(body.contains(&"customfield".to_owned()) && body.contains(&"paper·jammed".to_owned()));
    }

    #[test]
    fn adf_and_field_names_are_null_tolerant() {
        let doc = json!({"type": "doc", "content": [{"type": "p", "content": [{"text": "Hello"}, {"text": ""},
                         {"text": "world", "content": null}]}, null, "raw"]});
        assert_eq!(flatten_adf(&doc), "Hello world raw");
        assert_eq!(flatten_adf(&json!(null)), "");
        assert_eq!(
            field_names(&json!([{"value": "A"}, {"name": "B"}, {"displayName": 3}, null, "C", 5])),
            ["A", "B", "3", "C"]
        );
        assert_eq!(request_type_name(&json!({"requestType": {"name": "Get IT help"}})), "Get IT help");
        assert_eq!(request_type_name(&json!({"requestType": null, "name": "Other"})), "Other");
        assert_eq!(request_type_name(&json!("x")), "");
    }
}
