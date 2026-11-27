//! Normalising Jira issues and changelogs into what the learner learns from.
//!
//! Every field is treated as possibly null, not merely absent: Jira sends an
//! explicit null for fields the account cannot see.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};
use triage_engine::text::{TicketText, field_names, flatten_adf, request_type_name, truthy};

use crate::discovery::json_objects;
use crate::pyfmt::{prefix, py_str};

/// A link from an intake ticket to an issue in another project.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkedIssue {
    pub key: String,
    pub link_type: String,
}

impl LinkedIssue {
    pub fn project(&self) -> String {
        self.key.split('-').next().unwrap_or("").to_owned()
    }
}

/// `_issue_details`'s dict.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Details {
    pub key: String,
    pub summary: String,
    pub desc: String,
    pub labels: Vec<String>,
    pub components: Vec<String>,
    pub request_type: String,
    pub structured: Vec<String>,
    pub clones: Vec<LinkedIssue>,
    pub reporter: String,
}

fn obj(v: Option<&Value>) -> Option<&Map<String, Value>> {
    v.and_then(Value::as_object)
}

/// A string field where Jira may send null: null is empty, other scalars
/// are their text.
fn str_field(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(v) => py_str(v),
    }
}

fn text_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(v) if truthy(v) => py_str(v),
        _ => String::new(),
    }
}

impl Details {
    pub fn parse(issue: &Map<String, Value>, project: &str, req_field: &str, context_fields: &[String]) -> Self {
        let empty = Map::new();
        let f = obj(issue.get("fields")).unwrap_or(&empty);
        let desc = flatten_adf(f.get("description").unwrap_or(&Value::Null));
        let components = json_objects(f.get("components")).into_iter().map(|c| str_field(c.get("name"))).collect();
        let labels = match f.get("labels") {
            Some(Value::Array(items)) => items.iter().map(py_str).collect(),
            _ => Vec::new(),
        };
        let request_type = if req_field.is_empty() {
            String::new()
        } else {
            request_type_name(f.get(req_field).unwrap_or(&Value::Null))
        };
        let mut structured = Vec::new();
        for fid in context_fields {
            for name in field_names(f.get(fid).unwrap_or(&Value::Null)) {
                structured.push(format!("{fid} {name}"));
            }
        }
        let mut clones = Vec::new();
        if let Some(Value::Array(links)) = f.get("issuelinks") {
            for link in links.iter().filter_map(Value::as_object) {
                let other = [link.get("outwardIssue"), link.get("inwardIssue")]
                    .into_iter()
                    .flatten()
                    .find(|v| truthy(v))
                    .and_then(Value::as_object);
                let key = str_field(other.and_then(|o| o.get("key")));
                if !key.is_empty() && !key.starts_with(&format!("{project}-")) {
                    let link_type = str_field(obj(link.get("type")).and_then(|t| t.get("name")));
                    clones.push(LinkedIssue { key, link_type });
                }
            }
        }
        let reporter = str_field(obj(f.get("reporter")).and_then(|r| r.get("displayName")));
        Self {
            key: text_or_empty(issue.get("key")),
            summary: text_or_empty(f.get("summary")),
            desc: prefix(&desc, 800),
            labels,
            components,
            request_type,
            structured,
            clones,
            reporter,
        }
    }

    pub fn text(&self) -> TicketText {
        TicketText {
            summary: self.summary.clone(),
            desc: self.desc.clone(),
            labels: self.labels.clone(),
            components: self.components.clone(),
            structured: self.structured.clone(),
        }
    }

    pub fn clone_projects(&self) -> Vec<String> {
        self.clones.iter().map(LinkedIssue::project).collect()
    }

    pub fn clone_keys(&self) -> Vec<String> {
        self.clones.iter().map(|c| c.key.clone()).collect()
    }

    /// The features `predict_intake` snapshots (`intakeFeatures`).
    pub fn intake_features(&self) -> Value {
        json!({"summary": self.summary, "desc": self.desc, "labels": self.labels, "components": self.components,
               "structured": self.structured, "reporter": self.reporter, "requestType": self.request_type})
    }

    /// `dict(det, **snapshot)`; `Err` where Python's later use of a
    /// wrongly-typed snapshot value would have raised.
    pub fn with_snapshot(&self, snapshot: &Map<String, Value>) -> Result<Self, String> {
        let mut out = self.clone();
        let string = |v: &Value| v.as_str().map(str::to_owned).ok_or_else(|| "unusable intake snapshot".to_owned());
        let list = |v: &Value| -> Result<Vec<String>, String> {
            match v {
                Value::Array(items) => items.iter().map(string).collect(),
                _ => Err("unusable intake snapshot".into()),
            }
        };
        for (k, v) in snapshot {
            match k.as_str() {
                "summary" => out.summary = string(v)?,
                "desc" => out.desc = string(v)?,
                "labels" => out.labels = list(v)?,
                "components" => out.components = list(v)?,
                "structured" => out.structured = list(v)?,
                "reporter" => out.reporter = string(v)?,
                "requestType" => out.request_type = string(v)?,
                _ => {}
            }
        }
        Ok(out)
    }
}

/// One status change into an outcome (`_transitions`).
#[derive(Clone, Debug, PartialEq)]
pub struct Transition {
    pub id: String,
    pub dest: String,
    pub by: String,
    pub when: String,
}

/// One move of an intake ticket into another project (`_project_moves`).
#[derive(Clone, Debug, PartialEq)]
pub struct Move {
    pub id: String,
    pub old_key: String,
    pub by: String,
    pub when: String,
}

fn author(h: &Map<String, Value>) -> String {
    str_field(obj(h.get("author")).and_then(|a| a.get("displayName")))
}

fn when(h: &Map<String, Value>) -> String {
    prefix(&text_or_empty(h.get("created")), 16).replace('T', " ")
}

/// (handoffs into a destination status, transitions into a resolved status).
pub fn transitions(
    key: &str,
    changelog: &[Map<String, Value>],
    dests: &[String],
    resolved: &BTreeSet<String>,
) -> (Vec<Transition>, Vec<Transition>) {
    let mut routed = Vec::new();
    let mut done = Vec::new();
    for h in changelog {
        for item in json_objects(h.get("items")) {
            if item.get("field").and_then(Value::as_str) != Some("status") {
                continue;
            }
            let to = str_field(item.get("toString"));
            let rec = Transition {
                id: format!("{key}:{}", h.get("id").map(py_str).unwrap_or_default()),
                dest: to.clone(),
                by: author(h),
                when: when(h),
            };
            if dests.contains(&to) {
                routed.push(rec);
            } else if resolved.contains(&to) {
                done.push(rec);
            }
        }
    }
    (routed, done)
}

/// (moves of an intake ticket out to another project, its original summary).
pub fn project_moves(key: &str, changelog: &[Map<String, Value>], project: &str) -> (Vec<Move>, String) {
    let mut moves = Vec::new();
    let mut first_summary = String::new();
    let intake = format!("{project}-");
    for h in changelog {
        for item in json_objects(h.get("items")) {
            let field = item.get("field").and_then(Value::as_str).unwrap_or("");
            let from = text_or_empty(item.get("fromString"));
            if field == "Key" && from.starts_with(&intake) {
                moves.push(Move {
                    id: format!("{key}:{}", h.get("id").map(py_str).unwrap_or_default()),
                    old_key: from,
                    by: author(h),
                    when: when(h),
                });
            } else if field == "summary" && first_summary.is_empty() {
                first_summary = from;
            }
        }
    }
    (moves, first_summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: Value) -> Details {
        Details::parse(v.as_object().unwrap(), "SD", "customfield_10010", &["customfield_10332".into()])
    }

    #[test]
    fn null_shapes_are_parsed() {
        for payload in [
            json!({"key": "SD-1"}),
            json!({"key": "SD-1", "fields": null}),
            json!({"key": "SD-1", "fields": {"summary": null}}),
            json!({"key": "SD-1", "fields": {"summary": 42}}),
            json!({"key": "SD-1", "fields": {"summary": "x", "description": null}}),
            json!({"key": "SD-1", "fields": {"summary": "x", "labels": null}}),
            json!({"key": "SD-1", "fields": {"summary": "x", "components": null}}),
            json!({"key": "SD-1", "fields": {"summary": "x", "reporter": null}}),
            json!({"key": "SD-1", "fields": {"summary": "x", "issuelinks": null}}),
            json!({"key": "SD-1", "fields": {"summary": "x", "components": [{}]}}),
            json!({"key": "SD-1", "fields": {"summary": "x", "issuelinks": [{}]}}),
            json!({"key": "SD-1", "fields": {"summary": "x", "issuelinks": [{"type": null, "outwardIssue": {"key": "X-1"}}]}}),
            json!({"key": null, "fields": {"summary": "x"}}),
        ] {
            let det = parse(payload);
            let tuning = triage_engine::Tuning::default();
            let feats = triage_engine::text::features_for(&det.text(), &det.request_type, 6.0);
            let mut lr = triage_engine::Learner::new(triage_engine::Model::default(), vec![]);
            let title = triage_engine::text::tokenize(&det.summary);
            lr.learn(&tuning, "Somewhere", &feats, &det.request_type, &[], &det.reporter, 1.0, Some(&title), None);
        }
        assert_eq!(parse(json!({"fields": {"summary": 42}})).summary, "42");
    }

    #[test]
    fn well_formed_ticket_is_unchanged() {
        let good = parse(json!({"key": "SD-9", "fields": {
            "summary": "Printer offline in Legal",
            "description": {"content": [{"content": [{"text": "stopped working"}]}]},
            "labels": ["hardware", "urgent"], "components": [{"name": "Printers"}],
            "reporter": {"displayName": "Sam Carter"},
            "issuelinks": [{"type": {"name": "Escalation"}, "outwardIssue": {"key": "DEV-9"}},
                           {"type": {"name": "Relates"}, "inwardIssue": {"key": "SD-3"}}],
            "customfield_10010": {"requestType": {"name": "Get IT help"}},
            "customfield_10332": [{"value": "Printing"}]}}));
        assert_eq!(good.summary, "Printer offline in Legal");
        assert!(good.desc.contains("stopped working"));
        assert_eq!(good.labels, ["hardware", "urgent"]);
        assert_eq!(good.components, ["Printers"]);
        assert_eq!(good.reporter, "Sam Carter");
        assert_eq!(good.request_type, "Get IT help");
        assert_eq!(good.clone_keys(), ["DEV-9"]);
        assert_eq!(good.structured, ["customfield_10332 Printing"]);
    }

    #[test]
    fn changelog_parsing() {
        let log: Vec<Map<String, Value>> = serde_json::from_value(json!([
            {"id": "1", "created": "2026-09-07T10:00:00.000+0100", "author": {"displayName": "Sam"},
             "items": [{"field": "status", "toString": "With DEV"}, {"field": "status", "toString": "Done"}]},
            {"id": "2", "items": null},
            {"id": "3", "created": "2026-09-08T11:30:00.000+0100", "author": null,
             "items": [{"field": "Key", "fromString": "SD-5"}, {"field": "summary", "fromString": "orig"},
                       {"field": "summary", "fromString": "later"}]}
        ]))
        .unwrap();
        let (routed, done) = transitions("SD-5", &log, &["With DEV".into()], &BTreeSet::from(["Done".to_owned()]));
        assert_eq!(
            routed[0],
            Transition {
                id: "SD-5:1".into(),
                dest: "With DEV".into(),
                by: "Sam".into(),
                when: "2026-09-07 10:00".into()
            }
        );
        assert_eq!(done.len(), 1);
        let (moves, summary) = project_moves("BUG-1", &log, "SD");
        assert_eq!(moves.len(), 1);
        assert_eq!((moves[0].old_key.as_str(), moves[0].when.as_str()), ("SD-5", "2026-09-08 11:30"));
        assert_eq!(summary, "orig");
    }
}
