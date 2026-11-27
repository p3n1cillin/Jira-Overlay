//! Work out the shape of THIS Jira rather than assuming ours (Python
//! `Discovery` and the hand-edited-config coercions).

use std::collections::BTreeSet;

use indexmap::IndexMap;
use jo_jira::Jira;
use serde_json::{Map, Value};

use crate::pyfmt::py_str;

/// JSM's Request Type field is identified by its stable schema type.
pub const REQUEST_TYPE_SCHEMA: &str = "com.atlassian.servicedesk:vp-origin";
pub const DEFAULT_CONTEXT_FIELDS: [&str; 3] = ["Affected services", "Organizations", "Organisations"];
/// Share of arrivals still sitting in a status at scan time.
pub const REST_SHARE: f64 = 0.30;
/// Arrivals before a status can qualify as an outcome at all.
pub const MIN_DEST_ARRIVALS: u64 = 5;

/// A config setting that must be a list of names, whatever it holds. A bare
/// string is one name, never one per character.
pub fn as_name_list(value: Option<&Value>) -> Vec<String> {
    let keep = |s: String| (!s.trim().is_empty()).then_some(s);
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) => keep(s.clone()).into_iter().collect(),
        Some(Value::Array(items)) => items.iter().filter(|v| !v.is_null()).filter_map(|v| keep(py_str(v))).collect(),
        // Iterating a dict yields its keys.
        Some(Value::Object(map)) => map.keys().filter_map(|k| keep(k.clone())).collect(),
        Some(_) => Vec::new(),
    }
}

/// A config setting that must map a project key to a label.
pub fn as_label_map(value: Option<&Value>) -> Map<String, Value> {
    match value {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

/// Entries of a JSON array that are objects (Python `json_objects`).
pub fn json_objects(value: Option<&Value>) -> Vec<&Map<String, Value>> {
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_object).collect(),
        _ => Vec::new(),
    }
}

fn get_str<'a>(map: &'a Map<String, Value>, key: &str) -> &'a str {
    map.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Discovered (request-type field id, context field ids, catalogue read).
#[derive(Clone, Debug, PartialEq)]
pub struct Fields {
    pub request_type: String,
    pub context: Vec<String>,
    /// False when the field catalogue could not be read; retried later.
    pub complete: bool,
}

/// Match the Request Type schema (stable across JSM) and context fields by
/// their human names; pinned ids in the config skip the lookup.
pub fn fields(jira: &Jira, cfg: &Map<String, Value>) -> Fields {
    let pinned_rt = cfg.get("requestTypeField").and_then(Value::as_str).unwrap_or("").to_owned();
    let want: Vec<String> = match cfg.get("contextFields") {
        Some(v) if !v.is_null() => as_name_list(Some(v)),
        _ => DEFAULT_CONTEXT_FIELDS.iter().map(|s| (*s).to_owned()).collect(),
    }
    .into_iter()
    .map(|n| n.to_lowercase())
    .collect();
    let pinned_ctx = cfg.get("contextFieldIds").filter(|v| !v.is_null());
    if !pinned_rt.is_empty() && pinned_ctx.is_some() {
        return Fields { request_type: pinned_rt, context: as_name_list(pinned_ctx), complete: true };
    }
    let Some(Value::Array(raw)) = jira.get("/rest/api/3/field", &[]) else {
        return Fields { request_type: pinned_rt, context: as_name_list(pinned_ctx), complete: false };
    };
    let catalogue: Vec<&Map<String, Value>> = raw.iter().filter_map(Value::as_object).collect();
    let mut req_field = pinned_rt;
    let mut context: Vec<String> = Vec::new();
    for f in &catalogue {
        let id = get_str(f, "id");
        let name = get_str(f, "name").to_lowercase();
        let schema = f.get("schema").and_then(Value::as_object).map(|s| get_str(s, "custom")).unwrap_or("");
        if req_field.is_empty() && schema == REQUEST_TYPE_SCHEMA {
            req_field = id.to_owned();
        }
        if want.contains(&name) && !context.iter().any(|c| c == id) {
            context.push(id.to_owned());
        }
    }
    if req_field.is_empty() {
        // Site without the JSM origin field: fall back to the name.
        if let Some(f) = catalogue.iter().find(|f| get_str(f, "name").to_lowercase() == "request type") {
            req_field = get_str(f, "id").to_owned();
        }
    }
    Fields { request_type: req_field, context, complete: true }
}

/// (done statuses, open statuses) across every issue type in the project.
pub fn status_categories(jira: &Jira, project: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut done = BTreeSet::new();
    let mut open = BTreeSet::new();
    let data = jira.get(&format!("/rest/api/3/project/{project}/statuses"), &[]);
    for issue_type in json_objects(data.as_ref()) {
        for st in json_objects(issue_type.get("statuses")) {
            let name = get_str(st, "name");
            if name.is_empty() {
                continue;
            }
            let key = st.get("statusCategory").and_then(Value::as_object).map(|c| get_str(c, "key")).unwrap_or("");
            if key.to_lowercase() == "done" { &mut done } else { &mut open }.insert(name.to_owned());
        }
    }
    (done, open)
}

/// (resolved statuses, handoff statuses) read off observed behaviour: done
/// statuses tickets actually come to rest in; the busiest is ordinary
/// resolution, every other a handoff.
pub fn outcome_statuses(
    issues: &[Map<String, Value>],
    transitions: &IndexMap<String, Vec<String>>,
    done: &BTreeSet<String>,
) -> (Vec<String>, Vec<String>) {
    let current: IndexMap<String, String> = issues
        .iter()
        .map(|i| {
            let status = i
                .get("fields")
                .and_then(Value::as_object)
                .and_then(|f| f.get("status"))
                .and_then(Value::as_object)
                .map(|s| get_str(s, "name"))
                .unwrap_or("");
            (get_str(i, "key").to_owned(), status.to_owned())
        })
        .collect();
    let mut arrivals: IndexMap<String, u64> = IndexMap::new();
    let mut resting: IndexMap<String, u64> = IndexMap::new();
    for (key, moved_to) in transitions {
        let mut unique: Vec<&String> = Vec::new();
        for s in moved_to {
            if !unique.contains(&s) {
                unique.push(s);
            }
        }
        for status in unique {
            if !done.contains(status) {
                continue;
            }
            *arrivals.entry(status.clone()).or_insert(0) += 1;
            if current.get(key) == Some(status) {
                *resting.entry(status.clone()).or_insert(0) += 1;
            }
        }
    }
    let mut picked: Vec<String> = arrivals
        .iter()
        .filter(|(s, n)| {
            **n >= MIN_DEST_ARRIVALS && resting.get(*s).copied().unwrap_or(0) as f64 / **n as f64 >= REST_SHARE
        })
        .map(|(s, _)| s.clone())
        .collect();
    picked.sort_by_key(|s| std::cmp::Reverse(arrivals[s]));
    if picked.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let rest = picked.split_off(1);
    (picked, rest)
}

/// Outcome labels and the legacy-label mapping.
#[derive(Clone, Debug, Default)]
pub struct Labels {
    pub project: String,
    pub resolved_label: String,
    pub moved_label_fmt: String,
    pub moved_label_overrides: Map<String, Value>,
    pub move_targets: Vec<String>,
}

impl Labels {
    /// Outcome label for a ticket moved into `project_key`.
    pub fn moved_label(&self, project_key: &str) -> String {
        match self.moved_label_overrides.get(project_key) {
            Some(v) if triage_engine::text::truthy(v) => py_str(v),
            _ => self.moved_label_fmt.replace("{project}", project_key),
        }
    }

    /// Map labels written by older versions to today's outcome names.
    pub fn canonical_dest(&self, dest: &str) -> String {
        for project_key in &self.move_targets {
            let legacy = format!("Proclaim bug (moved to {project_key})");
            let current = self.moved_label(project_key);
            if dest == legacy && current != legacy {
                return current;
            }
        }
        dest.to_owned()
    }

    /// Every outcome the report accounts for, deduplicated.
    pub fn all_dests(&self, dests: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let moved = self.move_targets.iter().map(|p| self.moved_label(p));
        for d in dests.iter().cloned().chain(std::iter::once(self.resolved_label.clone())).chain(moved) {
            if !out.contains(&d) {
                out.push(d);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn name_lists_survive_hand_edits() {
        assert_eq!(as_name_list(Some(&json!("With DEV"))), ["With DEV"]);
        assert!(as_name_list(Some(&Value::Null)).is_empty() && as_name_list(None).is_empty());
        assert_eq!(as_name_list(Some(&json!(["With BI", "With DEV"]))), ["With BI", "With DEV"]);
        assert_eq!(as_name_list(Some(&json!(["With DEV", null, ""]))), ["With DEV"]);
        assert_eq!(as_name_list(Some(&json!([42]))), ["42"]);
        assert!(as_name_list(Some(&json!(7))).is_empty());
        assert!(as_name_list(Some(&json!("   "))).is_empty());
        assert_eq!(
            as_label_map(Some(&json!({"PPS": "Proclaim bug"}))),
            json!({"PPS": "Proclaim bug"}).as_object().unwrap().clone()
        );
        assert!(as_label_map(Some(&json!(["PPS"]))).is_empty() && as_label_map(Some(&Value::Null)).is_empty());
        assert!(json_objects(Some(&Value::Null)).is_empty() && json_objects(Some(&json!("junk"))).is_empty());
        assert_eq!(json_objects(Some(&json!([{"a": 1}, "junk", null, 5]))).len(), 1);
    }

    #[test]
    fn labels() {
        let labels = Labels {
            project: "HD".into(),
            resolved_label: "Passed to Networks".into(),
            moved_label_fmt: "Moved to {project}".into(),
            moved_label_overrides: json!({"PPS": "Proclaim bug"}).as_object().unwrap().clone(),
            move_targets: vec!["BUGS".into()],
        };
        assert_eq!(labels.moved_label("BUGS"), "Moved to BUGS");
        assert_eq!(labels.moved_label("PPS"), "Proclaim bug");
        assert_eq!(labels.canonical_dest("Proclaim bug (moved to BUGS)"), "Moved to BUGS");
        assert_eq!(labels.canonical_dest("Other"), "Other");
        // A colliding resolved label doesn't duplicate the outcome list.
        let collider = Labels { move_targets: vec![], ..labels };
        assert_eq!(collider.all_dests(&["Passed to Networks".into()]), ["Passed to Networks"]);
    }
}
