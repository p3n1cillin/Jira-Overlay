//! Read-only Jira Cloud client shared by the overlay and the learner.
//!
//! The only operations are GET requests and the two read-only search
//! endpoints (`POST /rest/api/3/search/jql` and
//! `POST /rest/api/3/search/approximate-count`). Any other method or POST
//! path is refused before a request is made, so nothing in Jira can ever be
//! created, changed or deleted through this crate.

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use jo_core::stop::Stop;
use serde_json::{Map, Value, json};

pub mod transport;

pub use transport::{Http, Request, Response, Transport, TransportError};

/// The only POST endpoints this client may call; both are read-only searches.
pub const SEARCH_PATHS: [&str; 2] = ["/rest/api/3/search/jql", "/rest/api/3/search/approximate-count"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// Retry policy for one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retry {
    /// A single attempt (the overlay polls again in 5 s anyway).
    Once,
    /// The learner's policy: 5 attempts; connection errors, timeouts, 429
    /// and 5xx back off 1 s doubling to 20 s, honouring `Retry-After`.
    Backoff,
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("{0}")]
    Transport(#[from] TransportError),
    #[error("stopped")]
    Stopped,
    #[error("gave up after repeated failures")]
    Exhausted,
    #[error("refusing a non-read-only request to {0}")]
    NotReadOnly(String),
}

/// `Retry-After` as seconds, or an HTTP date minus now; clamped to
/// [0, 86400]. Non-finite or unparseable values use `fallback`.
pub fn retry_delay(value: &str, fallback: f64, now: chrono::DateTime<chrono::Utc>) -> f64 {
    let value = value.trim();
    let delay = match value.parse::<f64>() {
        Ok(seconds) => seconds,
        Err(_) => match chrono::DateTime::parse_from_rfc2822(value) {
            Ok(stamp) => (stamp.with_timezone(&chrono::Utc) - now).num_milliseconds() as f64 / 1000.0,
            Err(_) => return fallback,
        },
    };
    if delay.is_finite() { delay.clamp(0.0, 86400.0) } else { fallback }
}

/// Items of a JSON array that are objects (Python `json_objects`).
pub fn json_objects(raw: &[Value]) -> Vec<Map<String, Value>> {
    raw.iter().filter_map(|v| v.as_object().cloned()).collect()
}

/// Read-only Jira client.
pub struct Jira {
    domain: String,
    auth: String,
    transport: Box<dyn Transport>,
    stop: Option<Stop>,
    refusals: Mutex<HashSet<u16>>,
    incomplete_reads: AtomicU64,
    last_success_epoch: Mutex<f64>,
    approx_count_ok: AtomicBool,
    log: Box<dyn Fn(&str) + Send + Sync>,
}

impl Jira {
    /// `domain` is the bare host (see `jo_core::jql::normalise_domain`).
    pub fn new(domain: &str, email: &str, token: &str, transport: Box<dyn Transport>) -> Self {
        let host = jo_core::jql::normalise_domain(domain);
        let credentials = base64::engine::general_purpose::STANDARD.encode(format!("{email}:{token}"));
        Self {
            domain: if host.is_empty() { domain.to_owned() } else { host },
            auth: format!("Basic {credentials}"),
            transport,
            stop: None,
            refusals: Mutex::default(),
            incomplete_reads: AtomicU64::new(0),
            last_success_epoch: Mutex::new(0.0),
            approx_count_ok: AtomicBool::new(true),
            log: Box::new(|_| {}),
        }
    }

    /// Interrupt long backoffs when `stop` is set (Quit or update shutdown).
    pub fn with_stop(mut self, stop: Stop) -> Self {
        self.stop = Some(stop);
        self
    }

    /// Where refusal messages (401/403/404) are written, once per code.
    pub fn with_log(mut self, log: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.log = Box::new(log);
        self
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }

    pub fn url(&self, path: &str) -> String {
        format!("https://{}{path}", self.domain)
    }

    pub fn incomplete_reads(&self) -> u64 {
        self.incomplete_reads.load(Ordering::SeqCst)
    }

    fn incomplete(&self) {
        self.incomplete_reads.fetch_add(1, Ordering::SeqCst);
    }

    pub fn last_success_epoch(&self) -> f64 {
        *self.last_success_epoch.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// HTTP status codes currently refusing access (401/403/404).
    pub fn refusals(&self) -> Vec<u16> {
        let mut codes: Vec<u16> = self.refusals.lock().unwrap_or_else(|p| p.into_inner()).iter().copied().collect();
        codes.sort_unstable();
        codes
    }

    fn stopped(&self) -> bool {
        self.stop.as_ref().is_some_and(Stop::is_set)
    }

    fn pause(&self, seconds: f64) -> bool {
        let wait = Duration::from_secs_f64(seconds.max(0.0));
        match &self.stop {
            Some(stop) => stop.wait(wait),
            None => {
                std::thread::sleep(wait);
                false
            }
        }
    }

    /// Send one request. POST is allowed only to [`SEARCH_PATHS`].
    pub fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
        timeout: Duration,
        retry: Retry,
    ) -> Result<Response, SendError> {
        if method == Method::Post && !SEARCH_PATHS.contains(&path) {
            return Err(SendError::NotReadOnly(path.to_owned()));
        }
        let request = Request {
            method,
            url: self.url(path),
            query: query.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect(),
            body: body.cloned(),
            authorization: self.auth.clone(),
            timeout,
        };
        let attempts = if retry == Retry::Backoff { 5 } else { 1 };
        let mut delay = 1.0_f64;
        for attempt in 0..attempts {
            if self.stopped() {
                return Err(SendError::Stopped);
            }
            let last = attempt + 1 == attempts;
            match self.transport.send(&request) {
                Err(err) => {
                    if last {
                        return Err(err.into());
                    }
                    if self.pause(delay) {
                        return Err(SendError::Stopped);
                    }
                    delay = (delay * 2.0).min(20.0);
                }
                Ok(response) if retry == Retry::Backoff && (response.status == 429 || response.status >= 500) => {
                    if last {
                        return Err(SendError::Exhausted);
                    }
                    let wait = retry_delay(response.retry_after.as_deref().unwrap_or(""), delay, chrono::Utc::now());
                    if self.pause(wait) {
                        return Err(SendError::Stopped);
                    }
                    delay = (delay * 2.0).min(20.0);
                }
                Ok(response) => {
                    self.note_refusal(method, path, response.status);
                    return Ok(response);
                }
            }
        }
        Err(SendError::Exhausted)
    }

    /// Record the first auth/permission refusal of each kind, so a wrong
    /// token is reported instead of silently producing empty results.
    fn note_refusal(&self, method: Method, path: &str, status: u16) {
        let mut refusals = self.refusals.lock().unwrap_or_else(|p| p.into_inner());
        if status < 400 {
            *self.last_success_epoch.lock().unwrap_or_else(|p| p.into_inner()) = jo_core::lifecycle::now_epoch();
            if path.contains("/search") {
                refusals.clear();
            }
            return;
        }
        if (status == 404 && path.contains("/issue/")) || ![401, 403, 404].contains(&status) || !refusals.insert(status)
        {
            return;
        }
        let reason = match status {
            401 => "credentials rejected — check the API token in jira_config.json",
            403 => "permission denied — this account cannot read that project",
            _ => "not found — check projectKey in jira_config.json",
        };
        let verb = if method == Method::Get { "GET" } else { "POST" };
        (self.log)(&format!("Jira returned {status} for {verb} {path}: {reason}"));
    }

    /// Learner GET: retries, `None` on failure; a non-JSON 2xx counts as an
    /// incomplete read.
    pub fn get(&self, path: &str, query: &[(&str, String)]) -> Option<Value> {
        let response = self.send(Method::Get, path, query, None, Duration::from_secs(15), Retry::Backoff).ok()?;
        if !response.ok() {
            return None;
        }
        match serde_json::from_slice(&response.body) {
            Ok(value) => Some(value),
            Err(_) => {
                self.incomplete();
                None
            }
        }
    }

    /// Learner search (`POST /rest/api/3/search/jql`). `cap: None` reads
    /// every page and requires a terminal page to count as complete.
    pub fn search(&self, jql: &str, fields: &[&str], cap: Option<usize>) -> Vec<Map<String, Value>> {
        let mut issues = Vec::new();
        let mut token: Option<String> = None;
        let mut tokens_seen = HashSet::new();
        let mut keys_seen = HashSet::new();
        let (mut complete, mut valid) = (false, true);
        while cap.is_none_or(|c| issues.len() < c) {
            if self.stopped() {
                break;
            }
            let max = cap.map_or(100, |c| (c - issues.len()).min(100));
            let mut body = json!({"jql": jql, "fields": fields, "maxResults": max});
            if let Some(t) = &token {
                body["nextPageToken"] = json!(t);
            }
            let Ok(response) =
                self.send(Method::Post, SEARCH_PATHS[0], &[], Some(&body), Duration::from_secs(20), Retry::Backoff)
            else {
                break;
            };
            if !response.ok() {
                break;
            }
            let Ok(Value::Object(data)) = serde_json::from_slice::<Value>(&response.body) else { break };
            let Some(Value::Array(raw)) = data.get("issues") else { break };
            let page = json_objects(raw);
            if page.len() != raw.len() {
                valid = false;
            }
            let mut added = 0;
            for issue in page {
                match issue.get("key").and_then(Value::as_str) {
                    Some(key) if !key.trim().is_empty() && keys_seen.insert(key.to_owned()) => {
                        issues.push(issue);
                        added += 1;
                    }
                    _ => valid = false,
                }
            }
            let next = match data.get("nextPageToken") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()).filter(|s| !s.is_empty()),
                Some(_) => {
                    valid = false;
                    break;
                }
            };
            let is_last = data.get("isLast");
            if is_last.is_some_and(|v| !v.is_boolean()) {
                valid = false;
                break;
            }
            if next.is_none() || is_last == Some(&Value::Bool(true)) {
                complete = is_last != Some(&Value::Bool(false));
                break;
            }
            let next = next.expect("checked above");
            if added == 0 || !tokens_seen.insert(next.clone()) {
                break;
            }
            token = Some(next);
        }
        if let Some(c) = cap
            && issues.len() >= c
        {
            issues.truncate(c);
            complete = true;
        }
        if !complete || !valid {
            self.incomplete();
        }
        issues
    }

    /// All changelog histories for an issue (paginated GET).
    pub fn changelog(&self, key: &str) -> Vec<Map<String, Value>> {
        let mut out = Vec::new();
        let mut start = 0_i64;
        let mut ids_seen = HashSet::new();
        let path = format!("/rest/api/3/issue/{}/changelog", jo_core::jql::path_segment(key));
        loop {
            if self.stopped() {
                self.incomplete();
                break;
            }
            let Some(Value::Object(data)) =
                self.get(&path, &[("startAt", start.to_string()), ("maxResults", "100".into())])
            else {
                self.incomplete();
                break;
            };
            if let Some(at) = data.get("startAt")
                && (at.is_boolean() || at.as_i64() != Some(start))
            {
                self.incomplete();
                break;
            }
            let Some(Value::Array(raw)) = data.get("values") else {
                self.incomplete();
                break;
            };
            let page = json_objects(raw);
            let mut valid = page.len() == raw.len();
            let mut added = 0;
            for entry in page {
                match entry.get("id").and_then(Value::as_str) {
                    Some(id) if !id.is_empty() && ids_seen.insert(id.to_owned()) => {
                        out.push(entry);
                        added += 1;
                    }
                    _ => valid = false,
                }
            }
            if !valid {
                self.incomplete();
            }
            let last = data.get("isLast");
            if last.is_some_and(|v| !v.is_null() && !v.is_boolean()) {
                self.incomplete();
                break;
            }
            let last = last.and_then(Value::as_bool);
            if last == Some(true) {
                break;
            }
            let total = data.get("total").filter(|t| !t.is_boolean()).and_then(Value::as_i64);
            if last.is_none() {
                if let Some(total) = total {
                    if start + raw.len() as i64 >= total {
                        break;
                    }
                } else if data.get("total").is_none_or(Value::is_null) {
                    break; // legacy single-page response without paging metadata
                }
            }
            if added == 0 {
                self.incomplete();
                break;
            }
            start += raw.len() as i64;
        }
        out
    }

    /// The overlay's count: `(count, jql_rejected)`. Prefers
    /// `/search/approximate-count`; on 404 (older Jira) switches to paging
    /// `/search/jql` for the rest of the process. `jql_rejected` is true only
    /// for HTTP 400, so a transient failure never latches a setting off.
    pub fn count(&self, jql: &str) -> (Option<u64>, bool) {
        let timeout = Duration::from_secs(10);
        if self.approx_count_ok.load(Ordering::SeqCst) {
            let body = json!({"jql": jql});
            let Ok(r) = self.send(Method::Post, SEARCH_PATHS[1], &[], Some(&body), timeout, Retry::Once) else {
                return (None, false);
            };
            if r.ok() {
                let count = serde_json::from_slice::<Value>(&r.body).ok().and_then(|v| v.get("count")?.as_u64());
                return (count, false);
            }
            match r.status {
                400 => return (None, true),
                404 => self.approx_count_ok.store(false, Ordering::SeqCst),
                _ => return (None, false),
            }
        }
        let mut total = 0_u64;
        let mut token: Option<String> = None;
        for _ in 0..100 {
            let mut body = json!({"jql": jql, "maxResults": 200, "fields": ["status"]});
            if let Some(t) = &token {
                body["nextPageToken"] = json!(t);
            }
            let Ok(r) = self.send(Method::Post, SEARCH_PATHS[0], &[], Some(&body), timeout, Retry::Once) else {
                return (None, false);
            };
            if !r.ok() {
                return (None, r.status == 400);
            }
            let Ok(Value::Object(data)) = serde_json::from_slice::<Value>(&r.body) else { return (None, false) };
            let page_len = data.get("issues").and_then(Value::as_array).map_or(0, Vec::len);
            total += page_len as u64;
            token = data.get("nextPageToken").and_then(Value::as_str).filter(|t| !t.is_empty()).map(str::to_owned);
            if token.is_none() || data.get("isLast") == Some(&Value::Bool(true)) || page_len == 0 {
                break;
            }
        }
        (Some(total), false)
    }
}

#[cfg(test)]
mod tests;
