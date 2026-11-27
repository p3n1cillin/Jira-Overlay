//! Behaviour tests ported from the Python suite (`tests/test_learner.py`,
//! `test_integrity.py`, `test_learning_progress.py`,
//! `test_adaptive_routes.py`, `test_runtime_recovery.py`,
//! `test_hardening.py`, `test_updates_ui.py`). Everything runs in temp dirs
//! against a fake `jo_jira::Transport`; no network.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use jo_core::stop::Stop;
use jo_jira::{Method, Request, Response, Transport, TransportError};
use serde_json::{Map, Value, json};

use crate::watcher::{JiraTz, Watcher};

mod adaptive;
mod integrity;
mod learner;
mod legacy;
mod progress;
mod recovery;
mod review;

type SearchFn = Box<dyn Fn(&str) -> Vec<Value> + Send>;

/// A read-only Jira that answers the endpoints the learner uses and
/// records what it was asked.
#[derive(Default)]
pub struct Inner {
    pub fields: Value,
    pub statuses: Value,
    pub myself: Value,
    pub search: Option<SearchFn>,
    pub changelogs: HashMap<String, Vec<Value>>,
    pub issues: HashMap<String, Value>,
    pub queries: Vec<String>,
    pub gets: Vec<String>,
    pub changelog_calls: usize,
    pub stop_after: Option<(usize, Stop)>,
    /// Keys whose changelog comes back malformed.
    pub poison: Vec<String>,
    /// Path -> HTTP status to answer with instead.
    pub status_for: HashMap<String, u16>,
    pub fail_search: bool,
}

#[derive(Clone, Default)]
pub struct FakeJira(pub Arc<Mutex<Inner>>);

impl FakeJira {
    pub fn new() -> Self {
        let fake = Self::default();
        fake.with(|j| {
            j.fields = fields_catalogue();
            j.statuses = statuses();
            j.myself = json!({});
        });
        fake
    }

    pub fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        f(&mut self.0.lock().unwrap())
    }

    pub fn on_search(&self, f: impl Fn(&str) -> Vec<Value> + Send + 'static) {
        self.with(|j| j.search = Some(Box::new(f)));
    }

    pub fn queries(&self) -> Vec<String> {
        self.with(|j| j.queries.clone())
    }

    pub fn calls(&self) -> usize {
        self.with(|j| j.queries.len() + j.gets.len())
    }
}

fn reply(status: u16, body: &Value) -> Result<Response, TransportError> {
    Ok(Response { status, retry_after: None, body: body.to_string().into_bytes() })
}

impl Transport for FakeJira {
    fn send(&self, request: &Request) -> Result<Response, TransportError> {
        let path = request.url.splitn(4, '/').nth(3).map(|p| format!("/{p}")).unwrap_or_default();
        let mut j = self.0.lock().unwrap();
        if let Some(code) = j.status_for.get(&path) {
            return reply(*code, &json!({}));
        }
        if request.method == Method::Post {
            let jql = request.body.as_ref().and_then(|b| b.get("jql")).and_then(Value::as_str).unwrap_or("").to_owned();
            j.queries.push(jql.clone());
            if j.fail_search {
                return reply(400, &json!({}));
            }
            let issues = j.search.as_ref().map(|f| f(&jql)).unwrap_or_default();
            return reply(200, &json!({"issues": issues, "isLast": true}));
        }
        j.gets.push(path.clone());
        if let Some(rest) = path.strip_prefix("/rest/api/3/issue/") {
            if let Some(key) = rest.strip_suffix("/changelog") {
                j.changelog_calls += 1;
                if let Some((after, stop)) = &j.stop_after
                    && j.changelog_calls > *after
                {
                    stop.set();
                }
                if j.poison.iter().any(|p| p == key) {
                    return reply(200, &json!({"values": null}));
                }
                let values = j.changelogs.get(key).cloned().unwrap_or_default();
                return reply(200, &json!({"values": values, "isLast": true}));
            }
            return match j.issues.get(rest) {
                Some(issue) => reply(200, issue),
                None => reply(404, &json!({})),
            };
        }
        match path.as_str() {
            "/rest/api/3/field" => reply(200, &j.fields),
            "/rest/api/3/myself" => reply(200, &j.myself),
            p if p.ends_with("/statuses") => reply(200, &j.statuses),
            _ => reply(404, &json!({})),
        }
    }
}

pub fn statuses() -> Value {
    json!([{"statuses": [
        {"name": "Triage", "statusCategory": {"key": "new"}},
        {"name": "Investigating", "statusCategory": {"key": "indeterminate"}},
        {"name": "Awaiting user", "statusCategory": {"key": "indeterminate"}},
        {"name": "Closed", "statusCategory": {"key": "done"}},
        {"name": "Passed to Networks", "statusCategory": {"key": "done"}},
        {"name": "Passed to Security", "statusCategory": {"key": "done"}},
    ]}])
}

pub fn fields_catalogue() -> Value {
    json!([
        {"id": "customfield_99001", "name": "Request Type", "schema": {"custom": "com.atlassian.servicedesk:vp-origin"}},
        {"id": "customfield_99002", "name": "Affected services", "schema": {}},
        {"id": "customfield_99003", "name": "Organizations", "schema": {}},
        {"id": "customfield_99999", "name": "Story Points", "schema": {}},
    ])
}

/// A changelog timestamp inside every learning window (yesterday).
pub fn fixture_created() -> String {
    (chrono::Local::now() - chrono::Duration::days(1)).format("%Y-%m-%dT10:00:00.000+0000").to_string()
}

pub fn status_change(id: &str, to: &str) -> Value {
    json!({"id": id, "created": fixture_created(), "author": {"displayName": "Agent"},
           "items": [{"field": "status", "toString": to}]})
}

/// Write `jira_config.json` (+ optional state) and open a watcher on it.
pub fn watcher(dir: &Path, cfg_extra: Value, state: Option<Value>, fake: &FakeJira) -> Watcher {
    let mut cfg =
        json!({"domain": "example.atlassian.net", "email": "e@example.com", "token": "t", "projectKey": "HD"});
    for (k, v) in cfg_extra.as_object().cloned().unwrap_or_default() {
        cfg[k] = v;
    }
    fs::write(dir.join("jira_config.json"), cfg.to_string()).unwrap();
    if let Some(state) = state {
        fs::write(dir.join("triage_state.json"), state.to_string()).unwrap();
    }
    let mut w = Watcher::open(dir, Box::new(fake.clone())).expect("watcher opens");
    w.tz = Some(JiraTz::Local);
    w
}

pub fn obj(v: Value) -> Map<String, Value> {
    v.as_object().cloned().expect("object")
}

pub fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

pub fn read_lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
