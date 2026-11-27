//! The shadow watcher (Python `ShadowWatcher`): discovery, backfill, the
//! fast intake loop, the slow resolve-and-learn loop and the nightly tune.
//!
//! STRICTLY READ-ONLY towards Jira: GET requests and the search endpoint.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use chrono::{Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use indexmap::{IndexMap, IndexSet};
use jo_core::config;
use jo_core::jql::{path_segment, quote, safe_project_key};
use jo_core::lifecycle::now_epoch;
use jo_core::stop::Stop;
use jo_jira::{Jira, Transport};
use serde_json::{Map, Value, json};
use triage_engine::model::Model;
use triage_engine::num::py_round;
use triage_engine::replay::{Record, TuneResult, fit_example, knobs_json, metrics_json};
use triage_engine::text::{TicketText, features_for, tokenize};
use triage_engine::tuning::{BALANCED, knob_value};
use triage_engine::{Learner, Tuning};

use crate::discovery::{self, Labels, as_label_map, as_name_list, json_objects};
use crate::files::{Paths, append_jsonl, load_history, log_error, write_json};
use crate::issue::{Details, Move, Transition, project_moves, transitions};
use crate::metrics::{MetricsCache, checkpoint_path};
use crate::pyfmt::{dict_repr, list_repr, pct, prefix, py_str, truthy};
use crate::support::{Counts, wilson_lower};

pub const FAST_POLL_SECONDS: f64 = 12.0;
pub const POLL_SECONDS: f64 = 60.0;
pub const BACKFILL_DAYS: i64 = 90;
pub const BACKFILL_CAP: usize = 1000;
pub const OVERLAP_SECS: i64 = 600;
pub const TUNING_SCHEMA_VERSION: i64 = 8;
pub const SEEN_CAP: usize = 20000;
pub const TUNE_WINDOW: usize = 6000;
pub const MIN_DRIFT_SAMPLE: u64 = 30;
pub const MOVE_LOOKUPS_PER_CYCLE: usize = 20;
pub const LOG_ROTATE_BYTES: u64 = 20 * 1024 * 1024;
pub const ROUTE_ACTIVITY_DAYS: i64 = 21;
pub const MIN_ROUTE_RECENT_ACTUAL: u64 = 3;
pub const MIN_ROUTE_FIRM_PREDICTIONS: u64 = 10;
pub const MIN_ROUTE_FIRM_PRECISION: f64 = 0.95;
pub const IDLE_SAVE_SECS: f64 = 300.0;
pub const REVIEW_CASES_CAP: usize = 200;
pub const TUNE_HISTORY_CAP: usize = 60;
pub const RESOLVED_LABEL_FMT: &str = "Resolved in {project}";
pub const MOVED_LABEL_FMT: &str = "Moved to {project}";
const BASE_FIELDS: [&str; 7] = ["summary", "description", "labels", "components", "issuelinks", "reporter", "status"];

/// The nightly search (swappable so tests need not replay real history).
pub type Tuner = Box<dyn FnMut(&[Record], &Tuning, &[String]) -> Option<TuneResult> + Send>;

/// The Jira account's profile timezone (`_jira_tz`).
#[derive(Clone, Copy, Debug)]
pub enum JiraTz {
    Named(chrono_tz::Tz),
    Local,
}

impl JiraTz {
    pub fn now(self) -> NaiveDateTime {
        match self {
            Self::Named(tz) => Utc::now().with_timezone(&tz).naive_local(),
            Self::Local => Local::now().naive_local(),
        }
    }

    pub fn from_epoch(self, secs: f64) -> NaiveDateTime {
        let whole = secs.floor() as i64;
        let nanos = ((secs - secs.floor()) * 1e9) as u32;
        match self {
            Self::Named(tz) => Utc
                .timestamp_opt(whole, nanos)
                .single()
                .map(|t| t.with_timezone(&tz).naive_local())
                .unwrap_or_else(|| self.now()),
            Self::Local => {
                Local.timestamp_opt(whole, nanos).single().map(|t| t.naive_local()).unwrap_or_else(|| self.now())
            }
        }
    }
}

/// Run `f`, turning a panic into an error, as Python's per-item
/// `except Exception` guards do.
pub fn guarded<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(panic) => Err(panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_else(|| "panic".into())),
    }
}

fn obj_entry<'a>(
    map: &'a mut Map<String, Value>,
    key: &str,
    default: impl FnOnce() -> Value,
) -> &'a mut Map<String, Value> {
    if !map.get(key).is_some_and(Value::is_object) {
        map.insert(key.to_owned(), default());
    }
    map.get_mut(key).and_then(Value::as_object_mut).expect("object inserted above")
}

fn int_of(v: Option<&Value>) -> i64 {
    v.and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))).unwrap_or(0)
}

fn bump_int(map: &mut Map<String, Value>, key: &str, by: i64) {
    let n = int_of(map.get(key)) + by;
    map.insert(key.to_owned(), json!(n));
}

/// `str(value or "")`.
fn text(v: Option<&Value>) -> String {
    match v {
        Some(v) if triage_engine::text::truthy(v) => py_str(v),
        _ => String::new(),
    }
}

fn io_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub struct Watcher {
    pub paths: Paths,
    pub cfg: Map<String, Value>,
    pub project: String,
    pub jira: Jira,
    pub state: Map<String, Value>,
    pub fields_complete: bool,
    pub req_field: String,
    pub context_fields: Vec<String>,
    pub dests: Vec<String>,
    pub resolved: BTreeSet<String>,
    pub labels: Labels,
    pub issue_fields: Vec<String>,
    /// Python's module-global `TUNING`.
    pub tuning: Tuning,
    /// Owns the outcome plans and the route-safety policy (shared by
    /// reference with the watcher in Python).
    pub learner: Learner,
    pub(crate) saved_mutations: u64,
    pub(crate) last_state_save: f64,
    pub stop: Stop,
    pub metrics: MetricsCache,
    pub(crate) intake_keys: HashSet<String>,
    pub(crate) tz: Option<JiraTz>,
    pub log_rotate_bytes: u64,
    pub tuner: Tuner,
    /// `os.replace` for the log rotation (tests simulate a reader lock).
    pub replace_file: fn(&Path, &Path) -> io::Result<()>,
}

impl Watcher {
    /// Python `ShadowWatcher.__init__`. Reads the config and state, resolves
    /// custom fields (one Jira call unless pinned) and adopts a tuned config.
    pub fn open(root: &Path, transport: Box<dyn Transport>) -> Result<Self, String> {
        let paths = Paths::new(root);
        if !paths.config.exists() {
            return Err(format!(
                "No {} found next to this script.\n\nRun the overlay first so it can set up your Jira credentials and \
                 find your service desk.",
                config::FILE_NAME
            ));
        }
        let cfg = config::load(&paths.config).ok_or_else(|| {
            format!("Failed to start:\n\n{} could not be read; it has been set aside.", config::FILE_NAME)
        })?;
        let project = cfg.get("projectKey").and_then(Value::as_str).unwrap_or("").to_owned();
        if !safe_project_key(&project) {
            return Err("No usable projectKey in jira_config.json — run the overlay first so it can discover your \
                        service desk."
                .into());
        }
        let field = |k: &str| {
            cfg.get(k)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("Failed to start:\n\nKeyError: '{k}'"))
        };
        let stop = Stop::new();
        let error_log = paths.error_log.clone();
        let jira = Jira::new(&field("domain")?, &field("email")?, &field("token")?, transport)
            .with_stop(stop.clone())
            .with_log(move |m| log_error(&error_log, m));
        let mut state = match fs::read(&paths.state) {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(Value::Object(m)) => m,
                Ok(_) => return Err("Failed to start:\n\ntriage_state.json does not hold a JSON object".into()),
                Err(_) => Map::new(),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Map::new(),
            Err(e) => return Err(format!("Failed to start:\n\n{e}")),
        };
        let found = discovery::fields(&jira, &cfg);
        let resolved_label = text(cfg.get("resolvedLabel"));
        let labels = Labels {
            resolved_label: if resolved_label.is_empty() {
                RESOLVED_LABEL_FMT.replace("{project}", &project)
            } else {
                resolved_label
            },
            moved_label_fmt: match text(cfg.get("movedLabel")) {
                s if s.is_empty() => MOVED_LABEL_FMT.into(),
                s => s,
            },
            moved_label_overrides: as_label_map(cfg.get("movedLabels")),
            move_targets: as_name_list(cfg.get("moveTargetProjects"))
                .into_iter()
                .filter(|p| safe_project_key(p))
                .collect(),
            project: project.clone(),
        };
        let dests = as_name_list(cfg.get("triageStatuses"));
        let mut resolved: BTreeSet<String> = as_name_list(cfg.get("resolvedStatuses")).into_iter().collect();
        if resolved.is_empty() {
            resolved = as_name_list(cfg.get("resolvedStatus")).into_iter().collect();
        }
        let mut tuning = Tuning::default();
        let mut save = false;
        if let Some(Value::Object(tuned)) = state.get("tunedConfig") {
            let mut upgraded = Map::new();
            for (k, v) in BALANCED {
                upgraded.insert(k.to_owned(), tuned.get(k).cloned().unwrap_or_else(|| knob_value(k, v)));
            }
            for (k, v) in tuned {
                if !upgraded.contains_key(k) {
                    upgraded.insert(k.clone(), v.clone());
                }
            }
            save |= BALANCED.iter().any(|(k, _)| !tuned.contains_key(*k));
            tuning.update_from(&upgraded);
            state.insert("tunedConfig".into(), Value::Object(upgraded));
        }
        if state.get("tuningSchemaVersion").and_then(Value::as_f64).unwrap_or(0.0) < TUNING_SCHEMA_VERSION as f64 {
            state.insert("tuningSchemaVersion".into(), json!(TUNING_SCHEMA_VERSION));
            state.insert("lastTuneDate".into(), json!("")); // run the upgraded tuner once now
            save = true;
        }
        let mut plans = IndexMap::new();
        plans.insert(labels.resolved_label.clone(), format!("resolve in {project} — no handoff"));
        for target in &labels.move_targets {
            plans.insert(labels.moved_label(target), format!("move the ticket to the {target} project"));
        }
        if !state.get("routeSafety").is_some_and(Value::is_object) {
            state.insert("routeSafety".into(), json!({}));
        }
        let route_safety = state["routeSafety"].as_object().cloned().unwrap_or_default();
        if !state.get("model").is_some_and(Value::is_object) {
            state.insert("model".into(), json!({}));
        }
        let mut learner = Learner::new(Model::from_value(&state["model"]), dests.clone());
        learner.plans = plans;
        learner.route_safety = route_safety;
        let mut w = Self {
            metrics: MetricsCache::new(&paths.log),
            paths,
            cfg,
            project,
            jira,
            state,
            fields_complete: found.complete,
            req_field: found.request_type,
            context_fields: found.context,
            dests,
            resolved,
            labels,
            issue_fields: Vec::new(),
            tuning,
            learner,
            saved_mutations: 0,
            last_state_save: 0.0,
            stop,
            intake_keys: HashSet::new(),
            tz: None,
            log_rotate_bytes: LOG_ROTATE_BYTES,
            tuner: Box::new(triage_engine::replay::self_tune),
            replace_file: |from, to| fs::rename(from, to),
        };
        w.rebuild_issue_fields();
        // Remember the ids so the field catalogue isn't re-fetched every start.
        let ids = json!(w.context_fields);
        if w.fields_complete
            && (w.cfg.get("requestTypeField") != Some(&json!(w.req_field))
                || w.cfg.get("contextFieldIds") != Some(&ids))
        {
            let mut found = Map::new();
            found.insert("requestTypeField".into(), json!(w.req_field));
            found.insert("contextFieldIds".into(), ids);
            w.save_cfg(found).map_err(|e| format!("Failed to start:\n\n{e}"))?;
        }
        if save {
            w.save_state(true).map_err(|e| format!("Failed to start:\n\n{e}"))?;
        }
        w.saved_mutations = 0;
        w.last_state_save = 0.0;
        Ok(w)
    }

    pub fn log_error(&self, message: &str) {
        log_error(&self.paths.error_log, message);
    }

    fn rebuild_issue_fields(&mut self) {
        self.issue_fields = BASE_FIELDS.iter().map(|s| (*s).to_owned()).collect();
        if !self.req_field.is_empty() {
            self.issue_fields.push(self.req_field.clone());
        }
        self.issue_fields.extend(self.context_fields.iter().cloned());
    }

    fn fields(&self) -> Vec<&str> {
        self.issue_fields.iter().map(String::as_str).collect()
    }

    /// `_save_cfg`: persist inferred settings next to the user's own.
    pub fn save_cfg(&mut self, discovered: Map<String, Value>) -> Result<(), String> {
        for (k, v) in &discovered {
            self.cfg.insert(k.clone(), v.clone());
        }
        config::save_keys(&self.paths.config, &discovered).map_err(io_err)
    }

    pub fn all_dests(&self) -> Vec<String> {
        self.labels.all_dests(&self.dests)
    }

    pub fn canonical_dest(&self, dest: &str) -> String {
        self.labels.canonical_dest(dest)
    }

    // -- discovery -----------------------------------------------------------

    /// Fill in whatever the config didn't pin. True once at least the
    /// resolved statuses are known.
    pub fn ensure_workflow_known(&mut self) -> Result<bool, String> {
        if !self.fields_complete {
            let found = discovery::fields(&self.jira, &self.cfg);
            self.req_field = found.request_type;
            self.context_fields = found.context;
            self.fields_complete = found.complete;
            if !self.fields_complete {
                return Ok(false);
            }
            self.rebuild_issue_fields();
            let mut save = Map::new();
            save.insert("requestTypeField".into(), json!(self.req_field));
            save.insert("contextFieldIds".into(), json!(self.context_fields));
            self.save_cfg(save)?;
        }
        if !self.resolved.is_empty() && (!self.dests.is_empty() || truthy(self.state.get("workflowDiscovered"))) {
            return Ok(true);
        }
        let reads_before = self.jira.incomplete_reads();
        let (done, _open) = discovery::status_categories(&self.jira, &self.project);
        if done.is_empty() {
            self.log_error(&format!("could not read statuses for project {} — cannot start", self.project));
            return Ok(false);
        }
        let issues = self.jira.search(
            &format!("project = {} AND updated >= -{BACKFILL_DAYS}d ORDER BY updated DESC", self.project),
            &["summary", "status"],
            Some(250),
        );
        let mut moves: IndexMap<String, Vec<String>> = IndexMap::new();
        for issue in &issues {
            if self.stop.is_set() {
                return Ok(false);
            }
            let key = issue.get("key").map(py_str).unwrap_or_default();
            let moved_to: Vec<String> = self
                .jira
                .changelog(&key)
                .iter()
                .flat_map(|h| json_objects(h.get("items")).into_iter().cloned().collect::<Vec<_>>())
                .filter(|item| item.get("field").and_then(Value::as_str) == Some("status"))
                .map(|item| text(item.get("toString")))
                .collect();
            if !moved_to.is_empty() {
                moves.insert(key, moved_to);
            }
        }
        if self.jira.incomplete_reads() != reads_before {
            return Ok(false);
        }
        let (resolved, handoffs) = discovery::outcome_statuses(&issues, &moves, &done);
        let mut found = Map::new();
        if self.resolved.is_empty() {
            if resolved.is_empty() {
                self.log_error(&format!(
                    "no settled outcome status found in {} — set resolvedStatuses in jira_config.json",
                    self.project
                ));
                return Ok(false);
            }
            self.resolved = resolved.iter().cloned().collect();
            let label = text(self.cfg.get("resolvedLabel"));
            self.labels.resolved_label =
                if label.is_empty() { RESOLVED_LABEL_FMT.replace("{project}", &self.project) } else { label };
            found.insert("resolvedStatuses".into(), json!(resolved));
        }
        if self.dests.is_empty() {
            self.dests = handoffs.clone();
            found.insert("triageStatuses".into(), json!(handoffs));
        }
        if !found.is_empty() {
            self.save_cfg(found)?;
            let resolved: Vec<&String> = self.resolved.iter().collect();
            let handoffs = if self.dests.is_empty() { "none".to_owned() } else { list_repr(&self.dests) };
            self.log_error(&format!(
                "discovered outcomes for {}: resolved={} handoffs={handoffs}",
                self.project,
                list_repr(&resolved)
            ));
        }
        if !self.resolved.is_empty() {
            self.state.insert("workflowDiscovered".into(), json!(true));
            self.save_state(true)?;
        }
        Ok(!self.resolved.is_empty())
    }

    /// A project we've now seen a ticket moved into.
    pub fn remember_move_target(&mut self, project_key: &str) -> Result<(), String> {
        if !project_key.is_empty()
            && safe_project_key(project_key)
            && !self.labels.move_targets.iter().any(|p| p == project_key)
        {
            self.labels.move_targets.push(project_key.to_owned());
            self.learner
                .plans
                .insert(self.labels.moved_label(project_key), format!("move the ticket to the {project_key} project"));
            let mut save = Map::new();
            save.insert("moveTargetProjects".into(), json!(self.labels.move_targets));
            self.save_cfg(save)?;
            self.log_error(&format!("discovered move-target project: {project_key}"));
        }
        Ok(())
    }

    // -- state ---------------------------------------------------------------

    /// Persist state. `force=false` skips the write when nothing was learned
    /// and the last write is recent (the poll clock alone is not worth
    /// rewriting a multi-megabyte model).
    pub fn save_state(&mut self, force: bool) -> Result<(), String> {
        if !force {
            let unchanged = self.learner.mutations == self.saved_mutations;
            if unchanged && now_epoch() - self.last_state_save < IDLE_SAVE_SECS {
                return Ok(());
            }
        }
        self.state.insert("model".into(), self.learner.model.to_value());
        self.state.insert("routeSafety".into(), Value::Object(self.learner.route_safety.clone()));
        write_json(&self.paths.state, &Value::Object(self.state.clone()), false).map_err(io_err)?;
        if let Err(e) = crate::storage::daily_backup(&self.paths.state, &self.paths.history, &self.paths.log) {
            self.log_error(&format!("Recovery backup failed: {e}"));
        }
        self.saved_mutations = self.learner.mutations;
        self.last_state_save = now_epoch();
        Ok(())
    }

    /// Append an event to `triage_log.jsonl`, rotating past the size cap
    /// after checkpointing the folded metrics prefix.
    pub fn log(&mut self, mut record: Map<String, Value>) -> Result<(), String> {
        record.insert("ts".into(), json!(Local::now().format("%Y-%m-%d %H:%M:%S").to_string()));
        let log = self.paths.log.clone();
        if fs::metadata(&log).is_ok_and(|m| m.len() > self.log_rotate_bytes) {
            let _ = self.rotate_log();
        }
        append_jsonl(&log, &Value::Object(record)).map_err(io_err)
    }

    fn rotate_log(&mut self) -> io::Result<()> {
        let labels = self.labels.clone();
        let metrics = self.metrics.live(&|d| labels.canonical_dest(d)).to_value();
        let prefix_size = self.metrics.offset;
        let log = self.paths.log.clone();
        let mut prefix = Vec::new();
        {
            use std::io::Read;
            fs::File::open(&log)?.take(prefix_size).read_to_end(&mut prefix)?;
        }
        let digest = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&prefix));
        let checkpoint = json!({"metrics": metrics, "pending": self.metrics.pending_json(),
                                "prefixSize": prefix_size, "prefixHash": digest});
        write_json(&checkpoint_path(&log), &checkpoint, false)?;
        let mut old = log.as_os_str().to_owned();
        old.push(".old");
        if (self.replace_file)(&log, Path::new(&old)).is_ok() {
            self.metrics.metrics = None;
            self.metrics.offset = 0;
        }
        Ok(())
    }

    fn history_record(det: &Details, dest: &str, when: &str) -> Map<String, Value> {
        let mut rec = Map::new();
        rec.insert("key".into(), json!(det.key));
        rec.insert("when".into(), json!(when));
        rec.insert("dest".into(), json!(dest));
        rec.insert("summary".into(), json!(det.summary));
        rec.insert("desc".into(), json!(det.desc));
        rec.insert("labels".into(), json!(det.labels));
        rec.insert("components".into(), json!(det.components));
        rec.insert("reporter".into(), json!(det.reporter));
        rec.insert("requestType".into(), json!(det.request_type));
        rec.insert("structured".into(), json!(det.structured));
        rec.insert("cloneProjects".into(), json!(det.clone_projects()));
        rec
    }

    fn snapshot(&self, key: &str) -> Option<&Map<String, Value>> {
        self.state.get("predicted")?.get(key)?.get("intakeFeatures")?.as_object()
    }

    /// A self-contained labelled example for `triage_history.jsonl`, using
    /// the intake snapshot when one was taken.
    pub fn append_history(&self, det: &Details, dest: &str, when: &str) -> Result<(), String> {
        let mut rec = Self::history_record(det, dest, when);
        let origin = match self.snapshot(&det.key) {
            Some(snapshot) => {
                for (k, v) in snapshot {
                    rec.insert(k.clone(), v.clone());
                }
                "intake"
            }
            None => "outcome",
        };
        rec.insert("featureOrigin".into(), json!(origin));
        append_jsonl(&self.paths.history, &Value::Object(rec)).map_err(io_err)
    }

    pub fn issue_details(&self, issue: &Map<String, Value>) -> Details {
        Details::parse(issue, &self.project, &self.req_field, &self.context_fields)
    }

    /// The ticket as it was at intake, when a snapshot exists.
    pub fn learning_details(&self, det: &Details) -> Result<Details, String> {
        match self.snapshot(&det.key) {
            Some(snapshot) => det.with_snapshot(snapshot),
            None => Ok(det.clone()),
        }
    }

    fn features(&self, text: &TicketText, req_type: &str) -> Vec<String> {
        features_for(text, req_type, self.tuning.get("summaryWeight"))
    }

    fn transitions(&self, key: &str) -> (Vec<Transition>, Vec<Transition>) {
        transitions(key, &self.jira.changelog(key), &self.dests, &self.resolved)
    }

    fn project_moves(&self, key: &str) -> (Vec<Move>, String) {
        project_moves(key, &self.jira.changelog(key), &self.project)
    }

    /// Resolve an intake key that vanished: Jira keeps the old key as an
    /// alias after a move, revealing the destination project.
    fn find_moved_issue(&self, old_key: &str) -> Option<Map<String, Value>> {
        let data = self
            .jira
            .get(&format!("/rest/api/3/issue/{}", path_segment(old_key)), &[("fields", self.issue_fields.join(","))])?;
        let Value::Object(data) = data else { return None };
        let new_key = data.get("key").and_then(Value::as_str).unwrap_or("");
        if new_key.is_empty() || new_key.starts_with(&format!("{}-", self.project)) {
            return None; // still in the intake project — not a move
        }
        Some(data)
    }

    fn learn(&mut self, dest: &str, det: &Details, clone_projects: &[String], weight: f64) {
        let toks = self.features(&det.text(), &det.request_type);
        let title = tokenize(&det.summary);
        let tuning = self.tuning.clone();
        self.learner.learn(
            &tuning,
            dest,
            &toks,
            &det.request_type,
            clone_projects,
            &det.reporter,
            weight,
            Some(&title),
            None,
        );
    }

    fn mistake_weight(&self, correct: Option<bool>) -> f64 {
        if correct == Some(false) { self.tuning.get("mistakeWeight").trunc() } else { 1.0 }
    }

    // -- backfill --------------------------------------------------------------

    fn seen(&mut self) -> IndexSet<String> {
        if !self.state.get("seenTransitions").is_some_and(Value::is_array) {
            self.state.insert("seenTransitions".into(), json!([]));
        }
        self.state["seenTransitions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    }

    fn backfill_checkpoint(&mut self, seen: &IndexSet<String>, count: i64) -> Result<(), String> {
        self.state.insert("seenTransitions".into(), json!(seen.iter().collect::<Vec<_>>()));
        self.state.insert("backfillCount".into(), json!(count));
        self.save_state(true)
    }

    /// Seed the model from the last 90 days of decisions: handoffs, then
    /// resolutions in place, then moves into other projects.
    pub fn backfill(&mut self) -> Result<(), String> {
        if truthy(self.state.get("backfillDone")) {
            return Ok(());
        }
        let reads_before = self.jira.incomplete_reads();
        let base_count = int_of(self.state.get("backfillCount"));
        let mut cutoff = (self.jira_now() - Duration::days(BACKFILL_DAYS)).format("%Y-%m-%d").to_string();
        let dest_list = self.dests.iter().map(|d| quote(d)).collect::<Vec<_>>().join(", ");
        let jql = format!(
            "project = {} AND status CHANGED TO ({dest_list}) AFTER -{BACKFILL_DAYS}d ORDER BY updated DESC",
            self.project
        );
        let issues =
            if self.dests.is_empty() { Vec::new() } else { self.jira.search(&jql, &self.fields(), Some(BACKFILL_CAP)) };
        let mut seen = self.seen();
        let (mut learned, mut skipped) = (0_i64, 0_u64);
        let mut first_routing = "9999".to_owned();
        for issue in &issues {
            if self.stop.is_set() {
                return self.backfill_checkpoint(&seen, base_count + learned);
            }
            let outcome = guarded(|| {
                let det = self.issue_details(issue);
                let (routed, _) = self.transitions(&det.key);
                let det = self.learning_details(&det)?;
                for tr in routed {
                    if seen.contains(&tr.id) || tr.when < cutoff {
                        continue;
                    }
                    if tr.when < first_routing {
                        first_routing = tr.when.clone();
                    }
                    seen.insert(tr.id.clone());
                    let projs = det.clone_projects();
                    self.learn(&tr.dest, &det, &projs, 1.0);
                    learned += 1;
                    self.append_history(&det, &tr.dest, &tr.when)?;
                    self.log(Self::backfill_event(
                        &det.key,
                        &det.summary,
                        &tr.dest,
                        &tr.by,
                        &tr.when,
                        &det.request_type,
                        det.clone_keys(),
                    ))?;
                }
                Ok(())
            });
            if outcome.is_err() {
                skipped += 1;
            }
        }
        // Anchor the window to the first observed handoff, so resolutions
        // from before the process existed can't teach "resolve everything".
        if first_routing != "9999" {
            let start = prefix(&first_routing, 10);
            if start > cutoff {
                cutoff = start;
            }
            self.state.insert("processStart".into(), json!(cutoff));
        }
        // Phase 2: resolved inside the intake project, never handed off.
        let resolved_list = self.resolved.iter().map(|s| quote(s)).collect::<Vec<_>>().join(", ");
        let jql2 = format!(
            "project = {} AND status CHANGED TO ({resolved_list}) AFTER -{BACKFILL_DAYS}d ORDER BY updated DESC",
            self.project
        );
        let label = self.labels.resolved_label.clone();
        let issues = self.jira.search(&jql2, &self.fields(), Some(BACKFILL_CAP));
        for issue in &issues {
            if self.stop.is_set() {
                return self.backfill_checkpoint(&seen, base_count + learned);
            }
            let outcome = guarded(|| {
                let det = self.issue_details(issue);
                let (routed, resolutions) = self.transitions(&det.key);
                let det = self.learning_details(&det)?;
                if !routed.is_empty() {
                    return Ok(());
                }
                for tr in resolutions {
                    if seen.contains(&tr.id) || tr.when < cutoff {
                        continue;
                    }
                    seen.insert(tr.id.clone());
                    self.learn(&label, &det, &[], 1.0);
                    learned += 1;
                    self.append_history(&det, &label, &tr.when)?;
                    self.log(Self::backfill_event(
                        &det.key,
                        &det.summary,
                        &label,
                        &tr.by,
                        &tr.when,
                        &det.request_type,
                        vec![],
                    ))?;
                }
                Ok(())
            });
            if outcome.is_err() {
                skipped += 1;
            }
        }
        // Phase 3: moved out into another project entirely.
        for target in self.labels.move_targets.clone() {
            let jql3 = format!("project = {target} AND updated >= -{BACKFILL_DAYS}d ORDER BY created DESC");
            let label = self.labels.moved_label(&target);
            let issues = self.jira.search(&jql3, &self.fields(), Some(BACKFILL_CAP));
            for issue in &issues {
                if self.stop.is_set() {
                    return self.backfill_checkpoint(&seen, base_count + learned);
                }
                let outcome = guarded(|| {
                    let mut det = self.issue_details(issue);
                    let (moves, orig_summary) = self.project_moves(&det.key);
                    if moves.is_empty() {
                        return Ok(());
                    }
                    if !orig_summary.is_empty() {
                        det.summary = format!("{orig_summary} — {}", det.summary);
                    }
                    for mv in moves {
                        if seen.contains(&mv.id) || mv.when < cutoff {
                            continue;
                        }
                        seen.insert(mv.id.clone());
                        self.learn(&label, &det, &[], 1.0);
                        learned += 1;
                        self.append_history(&det, &label, &mv.when)?;
                        self.log(Self::backfill_event(
                            &mv.old_key,
                            &det.summary,
                            &label,
                            &mv.by,
                            &mv.when,
                            &det.request_type,
                            vec![det.key.clone()],
                        ))?;
                    }
                    Ok(())
                });
                if outcome.is_err() {
                    skipped += 1;
                }
            }
        }
        if skipped > 0 {
            self.log_error(&format!(
                "backfill skipped {skipped} unusable issue(s) — {learned} decision(s) learned from the rest"
            ));
        }
        self.state.insert("seenTransitions".into(), json!(seen.iter().collect::<Vec<_>>()));
        let complete = self.jira.incomplete_reads() == reads_before && !self.stop.is_set();
        self.state.insert("backfillDone".into(), json!(complete));
        self.state.insert("backfillCount".into(), json!(base_count + learned));
        if !complete {
            self.log_error("Historical reads were incomplete; retained progress will be retried");
        }
        self.save_state(true)?;
        self.write_report()
    }

    fn backfill_event(
        key: &str,
        summary: &str,
        dest: &str,
        by: &str,
        when: &str,
        req_type: &str,
        clones: Vec<String>,
    ) -> Map<String, Value> {
        let v = json!({"event": "backfill", "key": key, "summary": summary, "dest": dest, "by": by, "when": when,
                       "requestType": req_type, "clones": clones});
        v.as_object().cloned().expect("object literal")
    }

    // -- live cycle --------------------------------------------------------------

    /// The Jira account's timezone, resolved once per process.
    pub fn jira_tz(&mut self) -> JiraTz {
        if let Some(tz) = self.tz {
            return tz;
        }
        let name = match self.jira.get("/rest/api/3/myself", &[]) {
            Some(Value::Object(me)) => text(me.get("timeZone")),
            _ => String::new(),
        };
        let tz = name.parse::<chrono_tz::Tz>().map(JiraTz::Named).unwrap_or(JiraTz::Local);
        self.tz = Some(tz);
        tz
    }

    /// Wall-clock in the Jira account's timezone, naive like Jira's own
    /// timestamps.
    pub fn jira_now(&mut self) -> NaiveDateTime {
        self.jira_tz().now()
    }

    /// The JQL datetime literal the slow poll searches from.
    pub fn window_jql_time(&mut self) -> String {
        let now = self.jira_now();
        let last = self.state.get("lastPollEpoch").and_then(Value::as_f64).unwrap_or(0.0);
        let start = if last != 0.0 {
            let prev = self.jira_tz().from_epoch(last);
            (now - Duration::days(7)).max(prev - Duration::seconds(OVERLAP_SECS))
        } else {
            now - Duration::hours(2)
        };
        format!("\"{}\"", start.format("%Y/%m/%d %H:%M"))
    }

    /// FAST loop: a suggestion for every ticket currently at intake.
    pub fn predict_intake(&mut self) -> Result<(), String> {
        obj_entry(&mut self.state, "predicted", || json!({}));
        let mut changed = false;
        let mut skipped = 0_u64;
        let mut intake_keys = HashSet::new();
        let issues = self.jira.search(
            &format!("project = {} AND resolution = Unresolved AND statusCategory = New", self.project),
            &self.fields(),
            Some(500),
        );
        for issue in &issues {
            let key = issue.get("key").map(py_str).unwrap_or_default();
            if !key.is_empty() {
                intake_keys.insert(key.clone());
            }
            if key.is_empty() || self.state["predicted"].get(&key).is_some() {
                continue;
            }
            let at = self.jira_now().format("%Y-%m-%d %H:%M").to_string();
            let outcome = guarded(|| {
                let det = self.issue_details(issue);
                let toks = self.features(&det.text(), &det.request_type);
                let title = tokenize(&det.summary);
                let tuning = self.tuning.clone();
                let p = self.learner.predict_ex(&tuning, &toks, &det.request_type, &det.reporter, Some(&title));
                let entry = json!({"dest": p.dest, "conf": p.conf, "plan": p.note, "second": p.second,
                    "secondConf": p.second_conf, "review": p.review, "source": p.source, "why": p.why,
                    "requestType": det.request_type, "intakeFeatures": det.intake_features(),
                    "summary": prefix(&det.summary, 120), "at": at});
                obj_entry(&mut self.state, "predicted", || json!({})).insert(key.clone(), entry);
                changed = true;
                let event = json!({"event": "would_have_done", "key": key, "summary": det.summary,
                    "prediction": p.dest, "confidence": p.conf, "second": p.second, "review": p.review,
                    "source": p.source, "requestType": det.request_type, "plan": p.note});
                self.log(event.as_object().cloned().expect("object literal"))
            });
            if outcome.is_err() {
                skipped += 1;
            }
        }
        if skipped > 0 {
            self.log_error(&format!("predict_intake skipped {skipped} unusable issue(s) this cycle"));
        }
        // Tickets still queued at intake can't have been moved.
        self.intake_keys = intake_keys;
        let predicted = self.state["predicted"].as_object().cloned().unwrap_or_default();
        self.write_suggestions(&predicted)?;
        if changed {
            self.save_state(true)?;
        }
        Ok(())
    }

    /// SLOW loop: score earlier suggestions against the team's decisions and
    /// train on them.
    pub fn resolve_and_learn(&mut self) -> Result<(), String> {
        let since = self.window_jql_time();
        let poll_started = now_epoch();
        let reads_before = self.jira.incomplete_reads();
        obj_entry(&mut self.state, "predicted", || json!({}));
        let mut seen = self.seen();
        obj_entry(&mut self.state, "stats", || json!({"predictions": 0, "correct": 0}));
        let dest_list = self.dests.iter().map(|d| quote(d)).collect::<Vec<_>>().join(", ");
        let mut skipped = 0_u64;
        let issues = if self.dests.is_empty() {
            Vec::new()
        } else {
            self.jira.search(
                &format!("project = {} AND status CHANGED TO ({dest_list}) AFTER {since}", self.project),
                &self.fields(),
                None,
            )
        };
        for issue in &issues {
            let outcome = guarded(|| {
                let det = self.issue_details(issue);
                let (routed, _) = self.transitions(&det.key);
                let det = self.learning_details(&det)?;
                for tr in routed {
                    if seen.contains(&tr.id) {
                        continue;
                    }
                    seen.insert(tr.id.clone());
                    let correct = self.resolve_prediction(&det, &tr.dest, &tr.by, &tr.when, det.clone_keys())?;
                    let weight = self.mistake_weight(correct);
                    let projs = det.clone_projects();
                    self.learn(&tr.dest, &det, &projs, weight);
                    self.append_history(&det, &tr.dest, &tr.when)?;
                }
                Ok(())
            });
            if outcome.is_err() {
                skipped += 1;
            }
        }
        let resolved_list = self.resolved.iter().map(|s| quote(s)).collect::<Vec<_>>().join(", ");
        let label = self.labels.resolved_label.clone();
        let issues = self.jira.search(
            &format!("project = {} AND status CHANGED TO ({resolved_list}) AFTER {since}", self.project),
            &self.fields(),
            None,
        );
        for issue in &issues {
            let outcome = guarded(|| {
                let det = self.issue_details(issue);
                let (routed, resolutions) = self.transitions(&det.key);
                let det = self.learning_details(&det)?;
                if !routed.is_empty() {
                    return Ok(());
                }
                for tr in resolutions {
                    if seen.contains(&tr.id) {
                        continue;
                    }
                    seen.insert(tr.id.clone());
                    let correct = self.resolve_prediction(&det, &label, &tr.by, &tr.when, vec![])?;
                    let weight = self.mistake_weight(correct);
                    self.learn(&label, &det, &[], weight);
                    self.append_history(&det, &label, &tr.when)?;
                }
                Ok(())
            });
            if outcome.is_err() {
                skipped += 1;
            }
        }
        skipped += self.learn_moved_out(&mut seen);
        if skipped > 0 {
            self.log_error(&format!("resolve/learn skipped {skipped} unusable issue(s) this cycle"));
        }
        // housekeeping
        let keep = seen.len().saturating_sub(SEEN_CAP);
        self.state.insert("seenTransitions".into(), json!(seen.iter().skip(keep).collect::<Vec<_>>()));
        let cutoff = (self.jira_now() - Duration::days(30)).format("%Y-%m-%d").to_string();
        let predicted: Map<String, Value> = self.state["predicted"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, v)| v.get("at").and_then(Value::as_str).unwrap_or("9999") >= cutoff.as_str())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        self.state.insert("predicted".into(), Value::Object(predicted.clone()));
        let complete = self.jira.incomplete_reads() == reads_before;
        self.state.insert("pollIncomplete".into(), json!(!complete));
        if complete {
            self.state.shift_remove("runtimeProblem");
            self.state.insert("lastPollEpoch".into(), json!(poll_started));
            self.state.insert("lastSuccessfulPoll".into(), json!(poll_started));
        }
        self.save_state(false)?;
        self.write_suggestions(&predicted)?;
        self.write_report()
    }

    /// Catch tickets that left intake by being MOVED: re-fetch keys we
    /// predicted on but haven't scored. Returns the number skipped.
    fn learn_moved_out(&mut self, seen: &mut IndexSet<String>) -> u64 {
        let mut unscored: Vec<(String, String)> = self.state["predicted"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(k, p)| {
                p.get("dest").is_some_and(|d| !d.is_null())
                    && !truthy(p.get("scored"))
                    && !self.intake_keys.contains(*k)
            })
            .map(|(k, p)| (k.clone(), p.get("at").map(py_str).unwrap_or_default()))
            .collect();
        unscored.sort_by(|a, b| a.1.cmp(&b.1));
        let mut skipped = 0;
        for (old_key, _) in unscored.into_iter().take(MOVE_LOOKUPS_PER_CYCLE) {
            if self.stop.is_set() {
                return skipped;
            }
            let outcome = guarded(|| {
                let Some(moved) = self.find_moved_issue(&old_key) else { return Ok(()) };
                let mut det = self.issue_details(&moved);
                let moved_key = moved.get("key").map(py_str).unwrap_or_default();
                let new_project = moved_key.split('-').next().unwrap_or("").to_owned();
                let (moves, orig_summary) = self.project_moves(&moved_key);
                let move_id = moves.first().map_or_else(|| format!("{old_key}:moved"), |m| m.id.clone());
                if seen.contains(&move_id) {
                    return Ok(());
                }
                seen.insert(move_id);
                self.remember_move_target(&new_project)?;
                let label = self.labels.moved_label(&new_project);
                if !orig_summary.is_empty() {
                    det.summary = format!("{orig_summary} — {}", det.summary);
                }
                det.key = old_key.clone();
                let det = self.learning_details(&det)?;
                let when = match moves.first() {
                    Some(m) => m.when.clone(),
                    None => self.jira_now().format("%Y-%m-%d %H:%M").to_string(),
                };
                let by = moves.first().map(|m| m.by.clone()).unwrap_or_default();
                let correct = self.resolve_prediction(&det, &label, &by, &when, vec![moved_key.clone()])?;
                let weight = self.mistake_weight(correct);
                self.learn(&label, &det, &[], weight);
                self.append_history(&det, &label, &when)
            });
            if outcome.is_err() {
                skipped += 1;
            }
        }
        skipped
    }

    /// Score our earlier guess against the human decision and log it.
    /// `None` when we never made one.
    pub fn resolve_prediction(
        &mut self,
        det: &Details,
        dest: &str,
        by: &str,
        when: &str,
        clones: Vec<String>,
    ) -> Result<Option<bool>, String> {
        let predicted = obj_entry(&mut self.state, "predicted", || json!({}));
        let pred = match predicted.get_mut(&det.key) {
            Some(Value::Object(p)) => {
                if !p.is_empty() {
                    p.insert("scored".into(), json!(true)); // stop re-probing for a move
                }
                p.clone()
            }
            _ => Map::new(),
        };
        let p_dest = pred.get("dest").filter(|d| !d.is_null()).cloned();
        let mut correct = None;
        if let Some(p_dest) = &p_dest {
            let p_text = py_str(p_dest);
            let is_correct = p_text == dest && p_dest.is_string();
            correct = Some(is_correct);
            if !is_correct {
                let case = json!({"key": det.key, "summary": det.summary, "predicted": p_dest, "actual": dest,
                                  "confidence": pred.get("conf").cloned().unwrap_or(Value::Null), "when": when});
                if !self.state.get("reviewCases").is_some_and(Value::is_array) {
                    self.state.insert("reviewCases".into(), json!([]));
                }
                let cases = self.state["reviewCases"].as_array_mut().expect("array inserted above");
                cases.push(case);
                let excess = cases.len().saturating_sub(REVIEW_CASES_CAP);
                cases.drain(..excess);
            }
            let stats = obj_entry(&mut self.state, "stats", || json!({"predictions": 0, "correct": 0}));
            bump_int(stats, "predictions", 1);
            bump_int(stats, "correct", i64::from(is_correct));
            let confusion = obj_entry(&mut self.state, "confusion", || json!({}));
            bump_int(confusion, &format!("{p_text} → {dest}"), 1);
            let today = Local::now().format("%Y-%m-%d").to_string();
            let daily = obj_entry(&mut self.state, "dailyStats", || json!({}));
            let day = match daily.get(&today) {
                Some(Value::Array(a)) => [int_of(a.first()), int_of(a.get(1))],
                _ => [0, 0],
            };
            daily.insert(today, json!([day[0] + 1, day[1] + i64::from(is_correct)]));
        }
        let field = |k: &str| pred.get(k).cloned().unwrap_or(Value::Null);
        let event = json!({"event": "routed", "key": det.key, "summary": det.summary, "dest": dest, "by": by,
            "when": when, "requestType": det.request_type, "clones": clones,
            "predicted": p_dest.clone().unwrap_or(Value::Null), "confidence": field("conf"), "second": field("second"),
            "secondConfidence": field("secondConf"), "review": field("review"), "source": field("source"),
            "predictionCorrect": correct});
        self.log(event.as_object().cloned().expect("object literal"))?;
        Ok(correct)
    }

    // -- daily self-improvement ------------------------------------------------

    pub fn load_history(&self) -> Option<Vec<Value>> {
        load_history(&self.paths.history, &self.paths.error_log)
    }

    /// Re-fit a clean model over the whole history with the current tuning.
    /// Rows are sanitised; unusable ones are skipped, not fatal.
    pub fn rebuild_model(&mut self, recs: &[Value]) {
        let mut lr = Learner::new(Model::default(), self.dests.clone());
        lr.plans = self.learner.plans.clone();
        lr.route_safety = self.learner.route_safety.clone();
        let mut skipped = 0;
        for r in recs {
            let Some(m) = r.as_object() else {
                skipped += 1;
                continue;
            };
            let dest = text(m.get("dest"));
            if dest.is_empty() {
                skipped += 1;
                continue;
            }
            let req_type = match text(m.get("requestType")) {
                s if s.is_empty() => text(m.get("reqType")),
                s => s,
            };
            let record = Record {
                dest,
                text: TicketText {
                    summary: text(m.get("summary")),
                    desc: text(m.get("desc")),
                    labels: as_name_list(m.get("labels")),
                    components: as_name_list(m.get("components")),
                    structured: as_name_list(m.get("structured")),
                },
                reporter: text(m.get("reporter")),
                request_type: req_type,
                clone_projects: as_name_list(m.get("cloneProjects")),
                ..Record::default()
            };
            fit_example(&mut lr, &self.tuning, &record);
        }
        if skipped > 0 {
            self.log_error(&format!("model rebuild skipped {skipped} unusable record(s) of {}", recs.len()));
        }
        self.state.insert("model".into(), lr.model.to_value());
        self.learner = lr;
    }

    /// Which outcomes have earned a confident live label, judged by recent
    /// activity and chronological confident precision.
    pub fn derive_route_safety(&self, recs: &[Value], firm_counts: &Counts) -> Map<String, Value> {
        let mut dated: Vec<(NaiveDateTime, Option<String>)> = Vec::new();
        for r in recs {
            let raw = prefix(&r.get("when").map(py_str).unwrap_or_default(), 16);
            let Some(when) = parse_iso_minutes(&raw) else { continue };
            dated.push((when, Some(text(r.get("dest"))).filter(|d| !d.is_empty())));
        }
        let latest = dated.iter().map(|(w, _)| *w).max().unwrap_or_else(|| Local::now().naive_local());
        let cutoff = latest - Duration::days(ROUTE_ACTIVITY_DAYS);
        let mut recent: IndexMap<String, u64> = IndexMap::new();
        for (when, dest) in &dated {
            if let Some(d) = dest.as_ref().filter(|_| *when >= cutoff) {
                *recent.entry(d.clone()).or_insert(0) += 1;
            }
        }
        let mut policy = Map::new();
        for dest in self.all_dests() {
            let mut reasons = Vec::new();
            let actual_n = recent.get(&dest).copied().unwrap_or(0);
            let [pred_n, right] = firm_counts.get(&dest).copied().unwrap_or([0, 0]);
            let precision = (pred_n > 0).then(|| right as f64 / pred_n as f64);
            if actual_n < MIN_ROUTE_RECENT_ACTUAL {
                reasons.push(format!(
                    "{dest} is dormant ({actual_n} actual decisions in the last {ROUTE_ACTIVITY_DAYS} days)"
                ));
            }
            if pred_n < MIN_ROUTE_FIRM_PREDICTIONS {
                reasons.push(format!("only {pred_n} confident validation predictions exist"));
            } else if let Some(p) = precision.filter(|p| *p < MIN_ROUTE_FIRM_PRECISION) {
                reasons.push(format!("recent confident precision is {} ({right}/{pred_n})", pct(p, 0)));
            }
            let lower = wilson_lower(right, pred_n);
            if pred_n >= MIN_ROUTE_FIRM_PREDICTIONS && lower < 0.90 {
                reasons.push(format!("precision lower bound is {}; more evidence needed", pct(lower, 0)));
            }
            policy.insert(
                dest,
                json!({"forceReview": !reasons.is_empty(), "reason": reasons.join("; "), "recentActual": actual_n,
                       "firmPredictions": pred_n, "firmCorrect": right,
                       "firmPrecision": precision.map(|p| py_round(p, 4)),
                       "precisionLowerBound": py_round(lower, 4)}),
            );
        }
        policy
    }

    /// Flag a genuine night-to-night swing, never small-sample noise.
    pub fn tune_drift_warning(today: &Map<String, Value>, prior: Option<&Map<String, Value>>) -> String {
        let Some(prior) = prior.filter(|p| !p.is_empty()) else { return String::new() };
        let num = |m: &Map<String, Value>, k: &str| m.get(k).and_then(Value::as_f64);
        let mut lines = Vec::new();
        for (label, key, threshold) in [
            ("balanced accuracy", "balanced", 0.05),
            ("macro F1", "macroF1", 0.05),
            ("macro precision", "macroPrecision", 0.05),
            ("confident-band coverage", "firmCov", 0.10),
        ] {
            let now_v = num(today, key).unwrap_or(0.0);
            let then_v = num(prior, key).unwrap_or(now_v);
            let delta = now_v - then_v;
            if delta.abs() >= threshold {
                let verb = if delta < 0.0 { "dropped" } else { "rose" };
                lines.push(format!("{label} {verb} {} ({} -> {})", pct(delta.abs(), 0), pct(then_v, 0), pct(now_v, 0)));
            }
        }
        let empty = Map::new();
        let counts = today.get("perCounts").and_then(Value::as_object).unwrap_or(&empty);
        let prior_per = prior.get("per").and_then(Value::as_object).unwrap_or(&empty);
        for (dest, rate) in today.get("per").and_then(Value::as_object).unwrap_or(&empty) {
            let n = counts.get(dest).and_then(|c| c.get(0)).and_then(Value::as_u64).unwrap_or(0);
            let (Some(rate), Some(then)) = (rate.as_f64(), prior_per.get(dest).and_then(Value::as_f64)) else {
                continue;
            };
            if n < MIN_DRIFT_SAMPLE {
                continue;
            }
            let delta = rate - then;
            if delta.abs() >= 0.15 {
                let verb = if delta < 0.0 { "dropped" } else { "rose" };
                lines.push(format!(
                    "{dest} accuracy {verb} {} ({} -> {}, n={n})",
                    pct(delta.abs(), 0),
                    pct(then, 0),
                    pct(rate, 0)
                ));
            }
        }
        lines.join("; ")
    }

    /// Once a local day: replay the history under candidate configs, adopt
    /// the best and re-fit. The date is stamped before the risky work, so a
    /// failing search retries tomorrow, not every poll.
    pub fn maybe_daily_tune(&mut self) -> Result<(), String> {
        let today = Local::now().format("%Y-%m-%d").to_string();
        if self.state.get("lastTuneDate").and_then(Value::as_str) == Some(today.as_str()) {
            return Ok(());
        }
        let Some(recs) = self.load_history() else { return Ok(()) };
        self.state.insert("lastTuneDate".into(), json!(today));
        if recs.len() < 300 {
            return self.save_state(true);
        }
        if let Err(e) = guarded(|| self.tune_on(&recs, &today)) {
            self.log_error(&format!("daily self-tune {today} failed:\n{e}"));
        }
        self.save_state(true)?;
        self.write_report()
    }

    fn tune_on(&mut self, recs: &[Value], today: &str) -> Result<(), String> {
        let search = &recs[recs.len().saturating_sub(TUNE_WINDOW)..];
        if search.len() < recs.len() {
            self.log_error(&format!(
                "self-tune searching the most recent {} of {} decisions",
                search.len(),
                recs.len()
            ));
        }
        let records = search
            .iter()
            .map(|r| {
                Record::from_value(r).ok_or_else(|| "KeyError: a history record lacks 'summary' or 'dest'".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let dests = self.dests.clone();
        let Some(best) = (self.tuner)(&records, &self.tuning, &dests) else { return Ok(()) };
        let cfg = best.cfg_json();
        self.tuning.update_from(&cfg);
        self.state.insert("tunedConfig".into(), Value::Object(cfg.clone()));
        let summary = tune_summary(&best);
        let safety = self.derive_route_safety(recs, &best.holdout.firm_precision_counts);
        self.learner.route_safety = safety.clone();
        self.state.insert("routeSafety".into(), Value::Object(safety));
        self.rebuild_model(recs);
        if !self.state.get("tuneHistory").is_some_and(Value::is_array) {
            self.state.insert("tuneHistory".into(), json!([]));
        }
        let prior = self.state["tuneHistory"].as_array().and_then(|h| h.last()).and_then(Value::as_object).cloned();
        let drift = Self::tune_drift_warning(&summary, prior.as_ref());
        self.state.insert(
            "tuneDrift".into(),
            if drift.is_empty() { Value::Null } else { json!({"date": today, "detail": drift}) },
        );
        if !drift.is_empty() {
            self.log_error(&format!("self-tune drift {today}: {drift}"));
        }
        let entry = history_entry(today, &best, &summary);
        let hist = self.state["tuneHistory"].as_array_mut().expect("array inserted above");
        hist.push(entry);
        let excess = hist.len().saturating_sub(TUNE_HISTORY_CAP);
        hist.drain(..excess);
        let f = |k: &str| summary.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        self.log_error(&format!(
            "daily self-tune {today}: holdoutFirm={} balanced={} macroF1={} worst={} holdoutAcc={} n={} finalAcc={} \
             challengerRejected={} cfg={}",
            pct(f("firmAcc"), 1),
            pct(f("balanced"), 1),
            pct(f("macroF1"), 1),
            pct(f("worst"), 1),
            pct(f("acc"), 1),
            best.holdout.n,
            pct(best.final_holdout.acc, 1),
            if best.challenger_rejected { "True" } else { "False" },
            dict_repr(&cfg),
        ));
        Ok(())
    }

    // -- suggestions ---------------------------------------------------------

    fn dest_phrase(&self, dest: &str) -> String {
        if dest == self.labels.resolved_label { format!("resolve in {}", self.project) } else { dest.to_owned() }
    }

    /// One-line suggestion: both candidates on a close call; a confident
    /// call names the words that drove it.
    pub fn short_label(&self, p: &Map<String, Value>) -> String {
        let field = |k: &str| p.get(k).map(py_str).unwrap_or_else(|| "None".into());
        let best = self.dest_phrase(&field("dest"));
        if truthy(p.get("review")) && truthy(p.get("second")) {
            let second = self.dest_phrase(&field("second"));
            let second_conf = p.get("secondConf").map(py_str).unwrap_or_else(|| "0".into());
            return format!("review — likely {best} ({}%), or {second} ({second_conf}%)", field("conf"));
        }
        let mut label = format!("{best} · {}%", field("conf"));
        if let Some(Value::Array(why)) = p.get("why")
            && !why.is_empty()
        {
            label.push_str(" — because: ");
            label.push_str(&why.iter().map(py_str).collect::<Vec<_>>().join(", "));
        }
        label
    }

    /// Publish current suggestions for the overlay (never touches Jira).
    /// A route the nightly tune has since flagged is shown as review.
    pub fn write_suggestions(&mut self, predicted: &Map<String, Value>) -> Result<(), String> {
        let cutoff = (self.jira_now() - Duration::days(7)).format("%Y-%m-%d").to_string();
        let mut out = Map::new();
        for (key, p) in predicted {
            let Some(p) = p.as_object() else { continue };
            let at = p.get("at").and_then(Value::as_str).unwrap_or("");
            if !truthy(p.get("dest")) || at < cutoff.as_str() {
                continue;
            }
            let dest = p.get("dest").map(py_str).unwrap_or_default();
            let force = self.learner.route_safety.get(&dest).is_some_and(|s| truthy(s.get("forceReview")));
            let mut shown = p.clone();
            if force && !truthy(p.get("review")) {
                shown.insert("review".into(), json!(true));
            }
            out.insert(
                key.clone(),
                json!({"label": self.short_label(&shown), "review": truthy(shown.get("review")),
                       "plan": p.get("plan").cloned().unwrap_or(json!("")), "at": p.get("at").cloned().unwrap_or(Value::Null)}),
            );
        }
        write_json(&self.paths.suggest, &Value::Object(out), true).map_err(io_err)
    }
}

/// `datetime.fromisoformat` for the `YYYY-MM-DD[ HH:MM]` shapes history uses.
pub fn parse_iso_minutes(raw: &str) -> Option<NaiveDateTime> {
    for fmt in ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H", "%Y-%m-%dT%H"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(raw, fmt) {
            return Some(t);
        }
    }
    NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0))
}

fn rounded(m: &IndexMap<String, f64>) -> Value {
    Value::Object(m.iter().map(|(k, v)| (k.clone(), json!(py_round(*v, 4)))).collect())
}

/// The fields of Python's `best` dict that downstream code reads.
pub fn tune_summary(best: &TuneResult) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("cfg".into(), Value::Object(best.cfg_json()));
    out.insert("trainN".into(), json!(best.train.n));
    out.insert("split".into(), json!(best.split));
    out.insert("evals".into(), json!(best.evals));
    out.insert("validationWindows".into(), json!(best.validation_windows));
    out.insert("challengerRejected".into(), json!(best.challenger_rejected));
    out.insert("gateReason".into(), json!(best.gate_reason));
    out.insert("incumbentGateScore".into(), json!(best.incumbent_gate_score));
    out.insert("challengerGateScore".into(), json!(best.challenger_gate_score));
    out.insert(
        "challengerCfg".into(),
        best.challenger_cfg.as_ref().map_or(Value::Null, |c| Value::Object(knobs_json(c))),
    );
    out.insert("gateN".into(), json!(best.gate.n));
    out.insert("holdoutN".into(), json!(best.holdout.n));
    out.insert("finalHoldout".into(), metrics_json(&best.final_holdout));
    out.insert("finalHoldoutN".into(), json!(best.final_holdout.n));
    if let Value::Object(holdout) = metrics_json(&best.holdout) {
        for (k, v) in holdout {
            if k != "samples" {
                out.insert(k, v);
            }
        }
    }
    out
}

fn history_entry(today: &str, best: &TuneResult, summary: &Map<String, Value>) -> Value {
    let h = &best.holdout;
    let fin = &best.final_holdout;
    let r = |x: f64| py_round(x, 4);
    json!({
        "date": today, "acc": r(h.acc), "balanced": r(h.balanced), "worst": r(h.worst), "firmAcc": r(h.firm_acc),
        "firmBalanced": r(h.firm_balanced), "firmWorst": r(h.firm_worst), "firmCov": r(h.firm_cov),
        "n": h.n, "trainN": best.train.n, "holdoutN": h.n, "gateN": best.gate.n, "finalHoldoutN": fin.n,
        "validationWindows": best.validation_windows, "challengerRejected": best.challenger_rejected,
        "gateReason": best.gate_reason, "incumbentGateScore": best.incumbent_gate_score,
        "challengerGateScore": best.challenger_gate_score,
        "segments": summary.get("segments").cloned().unwrap_or(json!({})),
        "featureOrigins": summary.get("featureOrigins").cloned().unwrap_or(json!({})),
        "finalAcc": r(fin.acc), "finalFirmAcc": r(fin.firm_acc), "finalBalanced": r(fin.balanced),
        "macroF1": r(h.macro_f1), "macroPrecision": r(h.macro_precision),
        "per": rounded(&h.per), "precision": rounded(&h.precision), "firmPer": rounded(&h.firm_per),
        "firmPrecision": rounded(&h.firm_precision), "cfg": summary.get("cfg").cloned().unwrap_or(json!({})),
    })
}
