//! The poller: queue counts every 5 s, alert-queue pulls, reconciliation,
//! completed/SLA stats and the backoff schedule. Runs on its own thread and
//! hands a [`Snapshot`] to the UI after every poll.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jo_core::config::{self, Config};
use jo_core::jql;
use jo_jira::{Jira, Method, Retry, Transport};
use serde_json::{Value, json};

use crate::model::{Desk, Issue, Queue, Snapshot};
use crate::stats;
use crate::util;

/// Queue-count poll interval: always 5 s regardless of settings.
pub const LIVE_POLL: Duration = Duration::from_secs(5);
const ISSUE_LIMIT: usize = 50;
const ISSUE_PAGES: usize = 4;
/// Alert-queue pulls run on this many dedicated clients.
pub const PULL_WORKERS: usize = 4;

/// Builds a fresh HTTP transport (one connection pool per client).
pub type MakeTransport = Arc<dyn Fn() -> Box<dyn Transport> + Send + Sync>;

/// The real transport factory.
pub fn http_transports() -> MakeTransport {
    Arc::new(|| match jo_jira::Http::new() {
        Ok(http) => Box::new(http) as Box<dyn Transport>,
        Err(err) => Box::new(Broken(err.0)),
    })
}

/// A transport that could not be built: every request fails as a
/// connection error, which the UI reports as "Reconnecting…".
struct Broken(String);

impl Transport for Broken {
    fn send(&self, _request: &jo_jira::Request) -> Result<jo_jira::Response, jo_jira::TransportError> {
        Err(jo_jira::TransportError(self.0.clone()))
    }
}

/// The configuration both threads read and write, and where it lives.
pub struct Shared {
    pub root: PathBuf,
    config: Mutex<Config>,
}

impl Shared {
    pub fn new(root: &Path, cfg: Config) -> Self {
        Self { root: root.to_path_buf(), config: Mutex::new(cfg) }
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join(config::FILE_NAME)
    }

    /// A copy of the current configuration.
    pub fn config(&self) -> Config {
        self.config.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn update<R>(&self, f: impl FnOnce(&mut Config) -> R) -> R {
        f(&mut self.config.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// Write the overlay's keys (read-merge-write, see `jo_core::config::save`).
    pub fn save(&self) {
        let cfg = self.config();
        if let Err(err) = config::save(&self.config_path(), &cfg) {
            util::log_error(&self.root, &format!("Could not save the configuration: {err}"));
        }
    }
}

/// A read-only Jira client for the given configuration.
pub fn client(cfg: &Config, make: &MakeTransport) -> Jira {
    Jira::new(
        config::get_str(cfg, "domain", ""),
        config::get_str(cfg, "email", ""),
        config::get_str(cfg, "token", ""),
        make(),
    )
}

/// What the poller tells the UI.
pub enum PollEvent {
    /// One poll finished (Python `_fetch_finished`).
    Fetched(Box<Snapshot>),
    /// First run: offer the alert-queue picker.
    OfferAlertPicker,
    /// Several desks are accessible: ask which one (reply within 120 s).
    PickDesk { desks: Vec<Desk>, reply: Sender<Desk> },
}

/// What the UI tells the poller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollCmd {
    /// "Refresh now": poll immediately unless a poll is already running.
    Refresh,
    /// Settings were saved. During a poll this forces an immediate re-poll
    /// with fresh stats; `reset_sla` drops the "no such SLA field" latch.
    SettingsSaved {
        reset_sla: bool,
    },
    Quit,
}

#[derive(Debug)]
enum FetchError {
    /// Connection failure or timeout: keep the last data.
    Transient,
    Http {
        status: u16,
        retry_after: Option<String>,
    },
    /// Anything else: `(Python exception type, detail)`.
    Other(&'static str, String),
    /// Discovery found no service desk this account can read.
    NoDesk,
}

/// Poller state (the Python attributes the fetch thread owned).
pub struct Poller {
    shared: Arc<Shared>,
    make: MakeTransport,
    jira: Jira,
    pool: Vec<Jira>,
    pub data: Snapshot,
    pub error_count: u32,
    pub retry_after: Option<Duration>,
    /// When completed/SLA stats were last fetched (`None`: never).
    pub last_full_fetch: Option<Instant>,
    pub sla_unavailable: bool,
    emit: Box<dyn Fn(PollEvent) + Send>,
}

impl Poller {
    pub fn new(shared: Arc<Shared>, make: MakeTransport, emit: Box<dyn Fn(PollEvent) + Send>) -> Self {
        let jira = client(&shared.config(), &make);
        Self {
            shared,
            make,
            jira,
            pool: Vec::new(),
            data: Snapshot::default(),
            error_count: 0,
            retry_after: None,
            last_full_fetch: None,
            sla_unavailable: false,
            emit,
        }
    }

    fn get(&self, client: &Jira, path: &str, query: &[(&str, String)], timeout: u64) -> Option<jo_jira::Response> {
        client.send(Method::Get, path, query, None, Duration::from_secs(timeout), Retry::Once).ok()
    }

    /// One poll with Python `_fetch_worker`'s error classification.
    pub fn fetch_cycle(&mut self) {
        self.retry_after = None;
        match self.fetch_data() {
            Ok(()) => {}
            Err(FetchError::Transient) => self.data.conn_error = true,
            Err(FetchError::Http { status, retry_after }) if status == 429 || status >= 500 => {
                // Rate-limited or a server hiccup: transient, keep last data.
                self.data.conn_error = true;
                let ra = retry_after.unwrap_or_default();
                if !ra.is_empty() && ra.chars().all(|c| c.is_ascii_digit()) {
                    let ms = ra.parse::<u64>().unwrap_or(u64::MAX / 1000).saturating_mul(1000);
                    self.retry_after = Some(Duration::from_millis(ms).max(LIVE_POLL));
                }
            }
            Err(FetchError::Http { status, .. }) => {
                let hint = if status == 401 || status == 403 { "check credentials" } else { "request failed" };
                self.hard_error(format!("HTTP {status} — {hint}"));
            }
            Err(FetchError::Other(kind, detail)) => {
                self.hard_error(format!("{kind}: {}", util::take_chars(&detail, 40)));
            }
            Err(FetchError::NoDesk) => {
                // Deliberate change: this is an error like any other and backs
                // off, instead of re-running discovery every 5 seconds.
                self.error_count += 1;
                self.data.error_msg = Some("No accessible service desk found".into());
                self.data.conn_error = false;
            }
        }
    }

    fn hard_error(&mut self, message: String) {
        self.error_count += 1;
        self.data.error_msg = Some(message);
        self.data.conn_error = false;
        self.data.queues.clear();
    }

    /// Delay before the next poll (Python `_fetch_finished`), given a jitter
    /// factor in [0.85, 1.15] for the error backoff.
    pub fn delay(&self, jitter: f64) -> Duration {
        if let Some(wait) = self.retry_after {
            wait
        } else if self.data.conn_error {
            LIVE_POLL
        } else if self.error_count > 0 {
            let ms = (5_000_u64 << self.error_count.min(6)).min(300_000);
            Duration::from_millis((ms as f64 * jitter) as u64)
        } else {
            LIVE_POLL
        }
    }

    fn fetch_data(&mut self) -> Result<(), FetchError> {
        let mut cfg = self.shared.config();

        // Discover the service desk on first run.
        if !util::truthy(cfg.get("serviceDeskId")) {
            let desk = self.discover_service_desk().ok_or(FetchError::NoDesk)?;
            self.shared.update(|c| {
                c.insert("serviceDeskId".into(), desk.id.clone());
                c.insert("serviceDeskName".into(), json!(desk.project_name));
                c.insert("projectKey".into(), json!(desk.project_key));
            });
            self.shared.save();
            cfg = self.shared.config();
        }
        let sd = jql::path_segment(&util::id_text(cfg.get("serviceDeskId")));

        // Backfill projectKey if absent (e.g. an older save stripped it).
        if config::get_str(&cfg, "projectKey", "").is_empty()
            && let Some(r) = self.get(&self.jira, &format!("/rest/servicedeskapi/servicedesk/{sd}"), &[], 10)
            && r.ok()
        {
            let pk = r.json().and_then(|v| v.get("projectKey")?.as_str().map(str::to_owned)).unwrap_or_default();
            // An identifier interpolated into JQL: only a real key is kept.
            let pk = if jql::safe_project_key(&pk) { pk } else { String::new() };
            self.shared.update(|c| c.insert("projectKey".into(), json!(pk)));
            self.shared.save();
            cfg = self.shared.config();
        }

        // Fast path: queue counts.
        let r = self
            .jira
            .send(
                Method::Get,
                &format!("/rest/servicedeskapi/servicedesk/{sd}/queue"),
                &[("includeCount", "true".into())],
                None,
                Duration::from_secs(10),
                Retry::Once,
            )
            .map_err(|_| FetchError::Transient)?;
        if !r.ok() {
            return Err(FetchError::Http { status: r.status, retry_after: r.retry_after.clone() });
        }
        let body: Value =
            serde_json::from_slice(&r.body).map_err(|e| FetchError::Other("JSONDecodeError", e.to_string()))?;
        let every_queue = parse_queues(&body)?;

        let skip: Vec<String> = util::cfg_strings(&cfg, "skipKeywords")
            .iter()
            .map(|k| k.trim().to_uppercase())
            .filter(|k| !k.is_empty())
            .collect();
        let max_count = util::cfg_int(&cfg, "maxQueueSize", 0).max(0) as u64;
        let hidden: HashSet<String> = util::cfg_strings(&cfg, "hiddenQueues").into_iter().collect();

        // Display filters decide what is worth looking at; none of them may
        // switch off an alert, so alerting reads `every_queue`.
        let all_queues: Vec<&Queue> = every_queue
            .iter()
            .filter(|q| {
                let upper = q.name.to_uppercase();
                !skip.iter().any(|kw| upper.contains(kw.as_str())) && (max_count == 0 || q.issue_count <= max_count)
            })
            .collect();
        self.data.all_queue_names = all_queues.iter().map(|q| q.name.clone()).collect();
        self.data.queues = all_queues.into_iter().filter(|q| !hidden.contains(&q.name)).cloned().collect();

        // Offer the alert-queue picker once, on first run.
        let alert_qs: HashSet<String> = util::alert_queues(&cfg).into_iter().collect();
        if alert_qs.is_empty() && !self.data.queues.is_empty() && !util::truthy(cfg.get("alertQueuesAsked")) {
            (self.emit)(PollEvent::OfferAlertPicker);
        }

        let alert_objs: Vec<&Queue> = every_queue.iter().filter(|q| alert_qs.contains(&q.name)).collect();
        // Missing means Jira really doesn't have it — never the user's filters.
        self.data.alert_queues_missing = if every_queue.is_empty() {
            Vec::new()
        } else {
            let names: HashSet<&str> = every_queue.iter().map(|q| q.name.as_str()).collect();
            let mut missing: Vec<String> = alert_qs.iter().filter(|n| !names.contains(n.as_str())).cloned().collect();
            missing.sort();
            missing
        };

        // Pull every busy alert queue's issues: includeCount is cached by JSM,
        // so the live list is authoritative once fully enumerated.
        let mut reconciled: HashMap<String, u64> = HashMap::new();
        for q in alert_objs.iter().filter(|q| q.issue_count == 0) {
            reconciled.insert(q.name.clone(), 0);
        }
        let busy: Vec<&Queue> = alert_objs.iter().copied().filter(|q| q.issue_count > 0).collect();
        let mut issues = Vec::new();
        let mut complete = true;
        if !busy.is_empty() {
            let results = self.pull_all(&sd, &busy);
            for (q, result) in busy.iter().zip(results) {
                match result {
                    Some((vals, enumerated)) => {
                        let count = if enumerated { vals.len() as u64 } else { q.issue_count };
                        issues.extend(vals.iter().map(|v| Issue::from_json(v, &q.name)));
                        reconciled.insert(q.name.clone(), count);
                    }
                    None => {
                        complete = false;
                        reconciled.insert(q.name.clone(), q.issue_count);
                    }
                }
            }
        }
        // On partial failure keep the previous snapshot — replacing it would
        // wipe the seen-set and re-notify every ticket once the fetch recovers.
        if complete {
            self.data.new_issues = issues;
        }
        self.data.alert_count =
            alert_objs.iter().map(|q| reconciled.get(&q.name).copied().unwrap_or(q.issue_count)).sum();
        self.data.reconciled = reconciled;

        // Slow path: SLA & completed today, every refreshSeconds.
        let interval = util::cfg_int(&cfg, "refreshSeconds", 60).max(0) as f64;
        if self.last_full_fetch.is_none_or(|t| t.elapsed().as_secs_f64() >= interval) {
            let jira = &self.jira;
            let out = stats::completion_stats(&cfg, stats::today(), &mut self.sla_unavailable, &|q| jira.count(q));
            self.data.completed_today = out.completed_today;
            self.data.sla_compliance = out.sla_compliance;
            self.last_full_fetch = Some(Instant::now());
        }

        self.data.error_msg = None;
        self.data.conn_error = false;
        self.error_count = 0;
        Ok(())
    }

    /// Pull each busy queue on a fixed pool of dedicated clients, at most
    /// four at once. `None` for a queue whose pull failed.
    fn pull_all(&mut self, sd: &str, busy: &[&Queue]) -> Vec<Pulled> {
        let workers = busy.len().min(PULL_WORKERS);
        let cfg = self.shared.config();
        while self.pool.len() < workers {
            self.pool.push(client(&cfg, &self.make));
        }
        let next = AtomicUsize::new(0);
        let results: Mutex<Vec<Pulled>> = Mutex::new(vec![None; busy.len()]);
        std::thread::scope(|scope| {
            for jira in &self.pool[..workers] {
                let (next, results) = (&next, &results);
                scope.spawn(move || {
                    loop {
                        let i = next.fetch_add(1, Ordering::SeqCst);
                        let Some(q) = busy.get(i) else { break };
                        let pulled = pull(jira, sd, &q.id);
                        results.lock().unwrap_or_else(|p| p.into_inner())[i] = pulled;
                    }
                });
            }
        });
        results.into_inner().unwrap_or_else(|p| p.into_inner())
    }

    /// Python `_discover_service_desk`: probe every desk, ask if several.
    fn discover_service_desk(&self) -> Option<Desk> {
        let r = self.get(&self.jira, "/rest/servicedeskapi/servicedesk", &[], 10)?;
        if !r.ok() {
            return None;
        }
        let desks: Vec<Desk> = match r.json()?.get("values") {
            Some(Value::Array(items)) => {
                items.iter().filter_map(Value::as_object).filter_map(Desk::from_json).collect()
            }
            _ => Vec::new(),
        };
        // Probe each desk independently — one unreachable desk must not abort
        // discovery for the rest.
        let accessible: Vec<Desk> = desks
            .into_iter()
            .filter(|d| {
                let id = jql::path_segment(&util::id_text(Some(&d.id)));
                self.get(
                    &self.jira,
                    &format!("/rest/servicedeskapi/servicedesk/{id}/queue"),
                    &[("limit", "1".into())],
                    5,
                )
                .is_some_and(|probe| probe.ok())
            })
            .collect();
        match accessible.len() {
            0 => None,
            1 => accessible.into_iter().next(),
            _ => {
                let (reply, answer) = mpsc::channel();
                (self.emit)(PollEvent::PickDesk { desks: accessible, reply });
                answer.recv_timeout(Duration::from_secs(120)).ok()
            }
        }
    }

    /// Run until [`PollCmd::Quit`] or the UI goes away.
    pub fn run(mut self, commands: Receiver<PollCmd>) {
        loop {
            self.fetch_cycle();
            // Commands that arrived while the poll ran: a Refresh is a no-op
            // (a poll was already running); a settings save re-polls at once.
            // Drained before the result is handed over, so anything the UI
            // sends after seeing it starts a fresh poll.
            let mut settings_changed = false;
            loop {
                match commands.try_recv() {
                    Ok(PollCmd::Refresh) => {}
                    Ok(PollCmd::SettingsSaved { reset_sla }) => {
                        self.sla_unavailable &= !reset_sla;
                        settings_changed = true;
                    }
                    Ok(PollCmd::Quit) | Err(TryRecvError::Disconnected) => return,
                    Err(TryRecvError::Empty) => break,
                }
            }
            (self.emit)(PollEvent::Fetched(Box::new(self.data.clone())));
            let delay = if settings_changed {
                self.last_full_fetch = None; // force a stats refresh
                Duration::ZERO
            } else {
                self.delay(jitter())
            };
            match commands.recv_timeout(delay) {
                Ok(PollCmd::Refresh) | Err(RecvTimeoutError::Timeout) => {}
                Ok(PollCmd::SettingsSaved { reset_sla }) => self.sla_unavailable &= !reset_sla,
                Ok(PollCmd::Quit) | Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// The queue list, with each count normalised once (Jira sends nulls).
fn parse_queues(body: &Value) -> Result<Vec<Queue>, FetchError> {
    let Value::Object(map) = body else {
        return Err(FetchError::Other("AttributeError", format!("'{}' object has no attribute 'get'", kind(body))));
    };
    let items = match map.get("values") {
        Some(Value::Array(items)) => items.as_slice(),
        Some(other) if util::truthy(Some(other)) => {
            return Err(FetchError::Other("TypeError", format!("queue list is a {}", kind(other))));
        }
        _ => &[],
    };
    items
        .iter()
        .map(|q| {
            let Value::Object(q) = q else {
                return Err(FetchError::Other(
                    "AttributeError",
                    format!("'{}' object has no attribute 'get'", kind(q)),
                ));
            };
            let name = q.get("name").ok_or_else(|| FetchError::Other("KeyError", "'name'".into()))?;
            Ok(Queue {
                name: util::py_str(name),
                id: util::id_text(q.get("id")),
                issue_count: util::count_value(q.get("issueCount")),
            })
        })
        .collect()
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// One queue's pages: `Some((issues, fully_enumerated))`, `None` on failure.
type Pulled = Option<(Vec<Value>, bool)>;

fn pull(jira: &Jira, sd: &str, queue_id: &str) -> Pulled {
    let path = format!("/rest/servicedeskapi/servicedesk/{sd}/queue/{}/issue", jql::path_segment(queue_id));
    let mut vals = Vec::new();
    for page_no in 0..ISSUE_PAGES {
        let start = page_no * ISSUE_LIMIT;
        let query = [("start", start.to_string()), ("limit", ISSUE_LIMIT.to_string())];
        let r = jira.send(Method::Get, &path, &query, None, Duration::from_secs(10), Retry::Once).ok()?;
        if !r.ok() {
            return None;
        }
        let page = match serde_json::from_slice::<Value>(&r.body).ok()?.get("values") {
            Some(Value::Array(items)) => items.clone(),
            Some(other) if util::truthy(Some(other)) => return None,
            _ => Vec::new(),
        };
        let short = page.len() < ISSUE_LIMIT;
        vals.extend(page);
        if short {
            return Some((vals, true)); // short page: whole queue seen
        }
    }
    Some((vals, false)) // ended on the page cap
}

/// ±15% jitter so a fleet of overlays doesn't retry in lockstep.
pub fn jitter() -> f64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(Instant::now().elapsed().as_nanos());
    0.85 + (h.finish() % 30_001) as f64 / 100_000.0
}

/// Start the poller thread. Returns the command channel.
pub fn spawn(poller: Poller) -> Sender<PollCmd> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new().name("jira-poller".into()).spawn(move || poller.run(rx));
    tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeJira, ok_json};
    use jo_jira::Response;
    use std::sync::Barrier;

    fn base() -> Config {
        json!({"domain": "x.atlassian.net", "email": "e", "token": "t", "projectKey": "SD",
               "serviceDeskId": 1, "serviceDeskName": "D", "refreshSeconds": 86400})
        .as_object()
        .unwrap()
        .clone()
    }

    struct Rig {
        poller: Poller,
        events: Arc<Mutex<Vec<String>>>,
        _dir: tempfile::TempDir,
    }

    fn rig(cfg: Config, fake: &FakeJira) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(Shared::new(dir.path(), cfg));
        let events = Arc::new(Mutex::new(Vec::new()));
        let log = events.clone();
        let mut poller = Poller::new(
            shared,
            fake.make(),
            Box::new(move |e| {
                let name = match e {
                    PollEvent::Fetched(_) => "fetched",
                    PollEvent::OfferAlertPicker => "picker",
                    PollEvent::PickDesk { .. } => "pick-desk",
                };
                log.lock().unwrap().push(name.to_owned());
            }),
        );
        poller.last_full_fetch = Some(Instant::now()); // stats are tested separately
        Rig { poller, events, _dir: dir }
    }

    fn queues_then_full_pages(queues: Value) -> FakeJira {
        FakeJira::new(move |req| {
            if req.url.contains("/issue") {
                let page: Vec<Value> = (0..50).map(|i| json!({"issueKey": format!("SD-{i}")})).collect();
                ok_json(json!({ "values": page }))
            } else {
                ok_json(json!({ "values": queues.clone() }))
            }
        })
    }

    fn open_closed() -> Value {
        json!([{"name": "Open", "id": 1, "issueCount": 4}, {"name": "Closed", "id": 2, "issueCount": 9}])
    }

    #[test]
    fn alert_queue_picker_is_offered_once() {
        let fake = queues_then_full_pages(open_closed());
        let mut r = rig(base(), &fake);
        r.poller.fetch_cycle();
        assert!(r.events.lock().unwrap().contains(&"picker".to_owned()), "offered on first run");

        let mut cfg = base();
        cfg.insert("alertQueuesAsked".into(), json!(true));
        let mut r = rig(cfg.clone(), &fake);
        r.poller.fetch_cycle();
        assert!(r.events.lock().unwrap().is_empty(), "closing it without choosing is remembered");
        assert_eq!(r.poller.data.queues.len(), 2, "queue counts still populate");

        cfg.insert("alertQueues".into(), json!([]));
        let mut r = rig(cfg, &fake);
        r.poller.fetch_cycle();
        assert!(r.events.lock().unwrap().is_empty() && r.poller.data.queues.len() == 2, "no alert queue is allowed");
    }

    #[test]
    fn display_filters_never_silence_alerts() {
        let busy = json!([{"name": "Newly Opened", "id": 1, "issueCount": 150},
                          {"name": "Archive Escalations", "id": 2, "issueCount": 5},
                          {"name": "Open tasks", "id": 3, "issueCount": 12}]);
        let fake = queues_then_full_pages(busy);
        let mut alert_base = base();
        alert_base.insert("alertQueues".into(), json!(["Newly Opened"]));
        alert_base.insert("alertQueuesAsked".into(), json!(true));
        for (label, extra, rows) in [
            ("with no filters set", json!({}), 3),
            ("when capped by maxQueueSize", json!({"maxQueueSize": 100}), 2),
            ("when excluded by skipKeywords", json!({"skipKeywords": ["NEWLY"]}), 2),
            ("when hidden from display", json!({"hiddenQueues": ["Newly Opened"]}), 2),
        ] {
            let mut cfg = alert_base.clone();
            cfg.extend(extra.as_object().unwrap().clone());
            let mut r = rig(cfg, &fake);
            r.poller.fetch_cycle();
            let d = &r.poller.data;
            assert!(
                d.alert_count == 150 && d.alert_queues_missing.is_empty(),
                "a busy alert queue still alerts {label}"
            );
            assert_eq!(d.queues.len(), rows, "the display is still filtered {label}");
        }
        let mut cfg = alert_base.clone();
        cfg.insert("alertQueues".into(), json!(["Nope"]));
        let mut r = rig(cfg, &fake);
        r.poller.fetch_cycle();
        assert_eq!(r.poller.data.alert_queues_missing, ["Nope"], "a queue Jira really doesn't have is reported");
    }

    #[test]
    fn malformed_counts_become_integers() {
        for raw in [json!(null), json!("12"), json!(3.7), json!(-5), json!("many"), json!(true)] {
            let queues = json!([{"name": "Q", "id": 1, "issueCount": raw}]);
            let fake = FakeJira::new(move |req| {
                if req.url.contains("/queue/1/issue") {
                    ok_json(json!({"values": [{"key": "SD-1", "fields": {"summary": "t"}}]}))
                } else {
                    ok_json(json!({ "values": queues.clone() }))
                }
            });
            let mut cfg = base();
            cfg.insert("alertQueues".into(), json!(["Q"]));
            cfg.insert("alertQueuesAsked".into(), json!(true));
            let mut r = rig(cfg, &fake);
            r.poller.fetch_cycle();
            assert!(r.poller.data.error_msg.is_none(), "{raw}");
            assert_eq!(r.poller.data.queues.len(), 1);
        }
        let fake = FakeJira::new(|_| ok_json(json!({"values": [{"name": "Q", "id": 1}]})));
        let mut r = rig(base(), &fake);
        r.poller.fetch_cycle();
        assert_eq!(r.poller.data.queues[0].issue_count, 0, "an absent count is 0");
    }

    #[test]
    fn null_values_never_crash() {
        let fake = FakeJira::new(|_| ok_json(json!({"values": null})));
        let mut cfg = base();
        cfg.insert("alertQueues".into(), json!(["Q"]));
        let mut r = rig(cfg.clone(), &fake);
        r.poller.data.queues = vec![Queue { name: "stale".into(), id: "9".into(), issue_count: 1 }];
        r.poller.fetch_cycle();
        assert!(r.poller.data.error_msg.is_none());
        assert!(r.poller.data.queues.is_empty(), "an empty queue list rather than stale data");

        // A per-queue issue page of "values": null.
        let fake = FakeJira::new(|req| {
            if req.url.contains("/queue/1/issue") {
                ok_json(json!({"values": null}))
            } else {
                ok_json(json!({"values": [{"name": "Q", "id": 1, "issueCount": 5}]}))
            }
        });
        let mut r = rig(cfg, &fake);
        r.poller.fetch_cycle();
        assert!(r.poller.data.error_msg.is_none());
        assert_eq!(r.poller.data.reconciled["Q"], 0, "a short (empty) page fully enumerates the queue");

        // Discovery with "values": null finds nothing.
        let fake = FakeJira::new(|_| ok_json(json!({"values": null})));
        let mut cfg = base();
        cfg.remove("serviceDeskId");
        let mut r = rig(cfg, &fake);
        r.poller.fetch_cycle();
        assert_eq!(r.poller.data.error_msg.as_deref(), Some("No accessible service desk found"));
    }

    #[test]
    fn issues_are_tagged_with_their_queue() {
        let fake = FakeJira::new(|req| {
            if req.url.contains("/issue") {
                ok_json(json!({"values": [{"issueKey": "SD-9", "fields": {}}, {"issueKey": "SD-10"}]}))
            } else {
                ok_json(json!({"values": [{"name": "Escalations", "id": 7, "issueCount": 2}]}))
            }
        });
        let mut cfg = base();
        cfg.insert("alertQueues".into(), json!(["Escalations"]));
        cfg.insert("alertQueuesAsked".into(), json!(true));
        let mut r = rig(cfg, &fake);
        r.poller.fetch_cycle();
        let issues = &r.poller.data.new_issues;
        assert_eq!(issues.len(), 2);
        assert!(issues.iter().all(|i| i.queue == "Escalations"));
    }

    #[test]
    fn alert_queues_pull_concurrently_on_dedicated_clients() {
        let barrier = Arc::new(Barrier::new(2));
        let b = barrier.clone();
        let fake = FakeJira::new(move |req| {
            if req.url.contains("/issue") {
                b.wait();
                let qid = req.url.trim_end_matches("/issue").rsplit('/').next().unwrap().to_owned();
                ok_json(json!({"values": [{"issueKey": format!("Q{qid}-1")}]}))
            } else {
                ok_json(json!({"values": [{"name": "Qa", "id": "a", "issueCount": 3},
                                          {"name": "Qb", "id": "b", "issueCount": 3}]}))
            }
        });
        let mut cfg = base();
        cfg.insert("alertQueues".into(), json!(["Qa", "Qb"]));
        cfg.insert("alertQueuesAsked".into(), json!(true));
        let mut r = rig(cfg, &fake);
        r.poller.fetch_cycle();
        let queues: HashSet<&str> = r.poller.data.new_issues.iter().map(|i| i.queue.as_str()).collect();
        assert_eq!(queues, HashSet::from(["Qa", "Qb"]));
        assert_eq!(r.poller.pool.len(), 2, "one dedicated client per concurrent pull");
        // The pool never grows past four, however many queues are busy.
        let many: Vec<Value> = (0..7).map(|i| json!({"name": format!("Q{i}"), "id": i, "issueCount": 1})).collect();
        let fake = FakeJira::new(move |req| {
            if req.url.contains("/issue") {
                ok_json(json!({"values": []}))
            } else {
                ok_json(json!({"values": many.clone()}))
            }
        });
        let mut cfg = base();
        cfg.insert("alertQueues".into(), json!((0..7).map(|i| format!("Q{i}")).collect::<Vec<_>>()));
        let mut r = rig(cfg, &fake);
        r.poller.fetch_cycle();
        assert_eq!(r.poller.pool.len(), PULL_WORKERS);
        assert_eq!(fake.clients(), 1 + PULL_WORKERS, "the poller's own client plus the pull pool");
    }

    #[test]
    fn page_cap_trusts_cached_count_and_short_page_is_authoritative() {
        let fake = queues_then_full_pages(json!([{"name": "Busy", "id": 1, "issueCount": 999}]));
        let mut cfg = base();
        cfg.insert("alertQueues".into(), json!(["Busy"]));
        let mut r = rig(cfg.clone(), &fake);
        r.poller.fetch_cycle();
        assert_eq!(r.poller.data.reconciled["Busy"], 999);
        assert_eq!(r.poller.data.new_issues.len(), 200, "four pages of fifty");
        let pages = fake.requests().iter().filter(|q| q.url.contains("/issue")).count();
        assert_eq!(pages, 4);

        // A cached non-zero for a queue that is really empty reconciles to 0.
        let fake = FakeJira::new(|req| {
            if req.url.contains("/issue") {
                ok_json(json!({"values": []}))
            } else {
                ok_json(json!({"values": [{"name": "Busy", "id": 1, "issueCount": 3}]}))
            }
        });
        let mut r = rig(cfg, &fake);
        r.poller.fetch_cycle();
        assert_eq!(r.poller.data.alert_count, 0);
    }

    #[test]
    fn a_failed_pull_keeps_the_previous_tickets() {
        let fake = FakeJira::new(|req| {
            if req.url.contains("/issue") {
                Ok(Response { status: 500, ..Default::default() })
            } else {
                ok_json(json!({"values": [{"name": "Q", "id": 1, "issueCount": 3}]}))
            }
        });
        let mut cfg = base();
        cfg.insert("alertQueues".into(), json!(["Q"]));
        let mut r = rig(cfg, &fake);
        let kept = vec![Issue { key: "SD-1".into(), summary: None, queue: "Q".into() }];
        r.poller.data.new_issues = kept.clone();
        r.poller.fetch_cycle();
        assert_eq!(r.poller.data.new_issues, kept);
        assert_eq!(r.poller.data.alert_count, 3, "falls back to the cached count");
    }

    #[test]
    fn errors_backoff_and_retry_after() {
        let status = Arc::new(Mutex::new((200_u16, None::<String>)));
        let s = status.clone();
        let fake = FakeJira::new(move |_| {
            let (code, ra) = s.lock().unwrap().clone();
            if code == 0 {
                return Err(jo_jira::TransportError("connection refused".into()));
            }
            Ok(Response { status: code, retry_after: ra, body: br#"{"values": []}"#.to_vec() })
        });
        let mut r = rig(base(), &fake);
        let set = |code: u16, ra: Option<&str>| *status.lock().unwrap() = (code, ra.map(str::to_owned));

        r.poller.fetch_cycle();
        assert_eq!(r.poller.delay(1.0), LIVE_POLL, "healthy polling every 5 s");

        set(0, None);
        r.poller.data.queues = vec![Queue { name: "Kept".into(), id: "1".into(), issue_count: 2 }];
        r.poller.fetch_cycle();
        assert!(r.poller.data.conn_error && r.poller.data.error_msg.is_none());
        assert_eq!(r.poller.data.queues.len(), 1, "a network drop keeps the last data");
        assert_eq!(r.poller.delay(1.0), LIVE_POLL, "reconnect attempts at the live rate");

        set(429, Some("30"));
        r.poller.fetch_cycle();
        assert!(r.poller.data.conn_error);
        assert_eq!(r.poller.delay(1.0), Duration::from_secs(30), "Retry-After is honoured");
        set(503, Some("1"));
        r.poller.fetch_cycle();
        assert_eq!(r.poller.delay(1.0), LIVE_POLL, "but never faster than the live poll");
        set(503, Some("Wed, 21 Oct 2015 07:28:00 GMT"));
        r.poller.fetch_cycle();
        assert_eq!(r.poller.delay(1.0), LIVE_POLL, "a date Retry-After is ignored");

        set(401, None);
        r.poller.fetch_cycle();
        assert_eq!(r.poller.data.error_msg.as_deref(), Some("HTTP 401 — check credentials"));
        assert!(!r.poller.data.conn_error && r.poller.data.queues.is_empty());
        let mut seen = vec![r.poller.delay(1.0)];
        set(404, None);
        for _ in 0..6 {
            r.poller.fetch_cycle();
            seen.push(r.poller.delay(1.0));
        }
        assert_eq!(r.poller.data.error_msg.as_deref(), Some("HTTP 404 — request failed"));
        let secs: Vec<u64> = seen.iter().map(Duration::as_secs).collect();
        assert_eq!(secs, [10, 20, 40, 80, 160, 300, 300], "exponential backoff capped at 5 minutes");
        let jittered = r.poller.delay(jitter());
        assert!((255..=345).contains(&jittered.as_secs()), "{jittered:?}");

        set(200, None);
        r.poller.fetch_cycle();
        assert!(r.poller.data.error_msg.is_none() && r.poller.error_count == 0);
        assert_eq!(r.poller.delay(1.0), LIVE_POLL);
    }

    #[test]
    fn unreadable_queue_list_is_a_hard_error() {
        let fake = FakeJira::new(|_| Ok(Response { status: 200, retry_after: None, body: b"<html>".to_vec() }));
        let mut r = rig(base(), &fake);
        r.poller.fetch_cycle();
        let msg = r.poller.data.error_msg.clone().unwrap();
        assert!(msg.starts_with("JSONDecodeError: ") && msg.len() <= "JSONDecodeError: ".len() + 40, "{msg}");
        assert_eq!(r.poller.error_count, 1);
    }

    #[test]
    fn no_service_desk_backs_off() {
        // Deliberate change 2: the error backoff applies.
        let fake = FakeJira::new(|_| ok_json(json!({"values": []})));
        let mut cfg = base();
        cfg.remove("serviceDeskId");
        let mut r = rig(cfg, &fake);
        r.poller.fetch_cycle();
        assert_eq!(r.poller.data.error_msg.as_deref(), Some("No accessible service desk found"));
        assert_eq!(r.poller.delay(1.0), Duration::from_secs(10));
        r.poller.fetch_cycle();
        assert_eq!(r.poller.delay(1.0), Duration::from_secs(20));
    }

    #[test]
    fn discovery_saves_the_single_accessible_desk() {
        let fake = FakeJira::new(|req| {
            let path = req.url.trim_start_matches("https://x.atlassian.net");
            match path {
                "/rest/servicedeskapi/servicedesk" => ok_json(json!({"values": [
                    {"id": "5", "projectName": "Help", "projectKey": "HD"},
                    {"id": "6", "projectName": "Locked", "projectKey": "LK"}]})),
                "/rest/servicedeskapi/servicedesk/6/queue" if req.query.iter().any(|(k, _)| k == "limit") => {
                    Ok(Response { status: 403, ..Default::default() })
                }
                _ => ok_json(json!({"values": [{"name": "Q", "id": 1, "issueCount": 0}]})),
            }
        });
        let mut cfg = base();
        cfg.remove("serviceDeskId");
        cfg.remove("projectKey");
        let mut r = rig(cfg, &fake);
        r.poller.fetch_cycle();
        let saved = config::load(&r.poller.shared.config_path()).unwrap();
        assert_eq!(saved["serviceDeskId"], "5");
        assert_eq!(saved["serviceDeskName"], "Help");
        assert_eq!(saved["projectKey"], "HD");
        assert!(r.poller.data.error_msg.is_none());
        assert_eq!(r.poller.data.queues.len(), 1);
    }

    #[test]
    fn several_desks_ask_the_user() {
        let fake = FakeJira::new(|req| {
            if req.url.ends_with("/servicedesk") {
                ok_json(json!({"values": [{"id": 5, "projectName": "A", "projectKey": "AA"},
                                          {"id": 6, "projectName": "B", "projectKey": "BB"}]}))
            } else {
                ok_json(json!({"values": []}))
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = base();
        cfg.remove("serviceDeskId");
        let shared = Arc::new(Shared::new(dir.path(), cfg));
        let mut poller = Poller::new(
            shared.clone(),
            fake.make(),
            Box::new(|e| {
                if let PollEvent::PickDesk { desks, reply } = e {
                    assert_eq!(desks[1].label(), "B  (BB)");
                    reply.send(desks[1].clone()).unwrap();
                }
            }),
        );
        poller.last_full_fetch = Some(Instant::now());
        poller.fetch_cycle();
        assert_eq!(shared.config()["serviceDeskId"], 6);
        assert_eq!(shared.config()["projectKey"], "BB");
    }

    #[test]
    fn project_key_backfill_is_validated() {
        let fake = FakeJira::new(|req| {
            if req.url.ends_with("/servicedesk/1") {
                ok_json(json!({"projectKey": "SD OR 1=1"}))
            } else {
                ok_json(json!({"values": []}))
            }
        });
        let mut cfg = base();
        cfg.insert("projectKey".into(), json!(""));
        let mut r = rig(cfg, &fake);
        r.poller.fetch_cycle();
        assert_eq!(r.poller.shared.config()["projectKey"], "");
    }

    #[test]
    fn slow_path_runs_on_its_interval() {
        let fake = FakeJira::new(|req| {
            if req.url.ends_with("approximate-count") {
                ok_json(json!({"count": 4}))
            } else {
                ok_json(json!({"values": []}))
            }
        });
        let mut cfg = base();
        cfg.insert("slaEnabled".into(), json!(false));
        let mut r = rig(cfg, &fake);
        r.poller.last_full_fetch = None;
        r.poller.fetch_cycle();
        assert_eq!(r.poller.data.completed_today, 4);
        let counted = || fake.requests().iter().filter(|q| q.url.ends_with("approximate-count")).count();
        assert_eq!(counted(), 1);
        r.poller.fetch_cycle();
        assert_eq!(counted(), 1, "stats wait for refreshSeconds");
    }

    #[test]
    fn run_loop_polls_and_quits() {
        let fake = FakeJira::new(|_| ok_json(json!({"values": []})));
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(Shared::new(dir.path(), base()));
        let (tx, rx) = mpsc::channel();
        let poller = Poller::new(
            shared,
            fake.make(),
            Box::new(move |e| {
                if matches!(e, PollEvent::Fetched(_)) {
                    let _ = tx.send(());
                }
            }),
        );
        let commands = spawn(poller);
        rx.recv_timeout(Duration::from_secs(5)).expect("first poll is immediate");
        commands.send(PollCmd::Refresh).unwrap();
        rx.recv_timeout(Duration::from_secs(5)).expect("refresh polls at once");
        commands.send(PollCmd::Quit).unwrap();
    }
}
