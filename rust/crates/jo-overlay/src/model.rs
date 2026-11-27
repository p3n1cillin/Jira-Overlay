//! Data the poller hands to the UI.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::util;

/// One service-desk queue, with its count already normalised to an integer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Queue {
    pub name: String,
    pub id: String,
    pub issue_count: u64,
}

/// One queue ticket (alert-queue pull or hover preview).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Issue {
    /// `issueKey`, else `key`; may be empty for a malformed entry.
    pub key: String,
    /// `fields.summary` when it is text.
    pub summary: Option<String>,
    /// The alert queue it was pulled from (Python `_queue`).
    pub queue: String,
    /// `fields.created` as Jira sent it, when the queue returns it.
    pub created: Option<String>,
    /// `fields.assignee.displayName`, when assigned and returned.
    pub assignee: Option<String>,
}

impl Issue {
    pub fn new(key: &str, summary: Option<&str>, queue: &str) -> Self {
        Self { key: key.into(), summary: summary.map(str::to_owned), queue: queue.into(), ..Self::default() }
    }

    pub fn from_json(value: &Value, queue: &str) -> Self {
        let text = |v: Option<&Value>| v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
        let key = text(value.get("issueKey")).or_else(|| text(value.get("key"))).unwrap_or_default();
        let fields = value.get("fields");
        let summary = fields.and_then(|f| f.get("summary")).and_then(Value::as_str).map(str::to_owned);
        let created = text(fields.and_then(|f| f.get("created")));
        let assignee = text(fields.and_then(|f| f.get("assignee")).and_then(|a| a.get("displayName")));
        Self { key, summary, queue: queue.to_owned(), created, assignee }
    }
}

/// Open tickets whose SLA is running with little time left (slow path).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AtRisk {
    pub count: u64,
    /// `(key, summary)` of the tickets returned (at most one page).
    pub tickets: Vec<(String, Option<String>)>,
}

/// SLA % over the last 5 and 20 working days (slow path, cached).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlaTrend {
    pub five: Option<i64>,
    pub twenty: Option<i64>,
}

/// A service desk returned by discovery.
#[derive(Clone, Debug, PartialEq)]
pub struct Desk {
    pub id: Value,
    pub project_name: String,
    pub project_key: String,
}

impl Desk {
    pub fn from_json(value: &Map<String, Value>) -> Option<Self> {
        let id = value.get("id")?.clone();
        Some(Self {
            id,
            project_name: value.get("projectName").map(util::py_str).unwrap_or_default(),
            project_key: value.get("projectKey").and_then(Value::as_str).unwrap_or_default().to_owned(),
        })
    }

    /// Picker button text: `"{projectName}  ({projectKey})"`.
    pub fn label(&self) -> String {
        format!("{}  ({})", self.project_name, self.project_key)
    }
}

/// Everything one fetch produced (the Python instance attributes the fetch
/// thread wrote), copied to the UI after every poll.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// Transient failure: keep showing the last data, say "Reconnecting…".
    pub conn_error: bool,
    /// Hard error requiring user action.
    pub error_msg: Option<String>,
    /// Queues to display (after every filter).
    pub queues: Vec<Queue>,
    /// Queue names before the hidden-queue filter (for Settings).
    pub all_queue_names: Vec<String>,
    /// Tickets currently in the alert queues.
    pub new_issues: Vec<Issue>,
    /// Alert queue name -> live (reconciled) count.
    pub reconciled: HashMap<String, u64>,
    /// Total across every alert queue, from the unfiltered list.
    pub alert_count: u64,
    /// Alert queues configured but not in Jira.
    pub alert_queues_missing: Vec<String>,
    /// Alert queue name -> queue id (for links to the queue).
    pub alert_queue_ids: HashMap<String, String>,
    pub completed_today: u64,
    pub sla_compliance: Option<i64>,
    /// `None`: switched off, not supported by this Jira, or not fetched yet.
    pub at_risk: Option<AtRisk>,
    pub sla_trend: Option<SlaTrend>,
}
