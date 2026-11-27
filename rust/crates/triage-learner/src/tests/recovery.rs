//! `tests/test_runtime_recovery.py` (learner parts), `test_hardening.py`
//! (log rotation) and `test_updates_ui.py` (shutdown joins and saves).

use std::cell::RefCell;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::metrics::MetricsCache;
use crate::runtime::{Cycle, Waiter, follow_overlay, run_forever, spawn_worker};

#[test]
fn an_offline_field_catalogue_stays_retryable() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    fake.with(|j| j.fields = Value::Null);
    let mut w = watcher(
        dir.path(),
        json!({"resolvedStatuses": ["Completed by this desk"]}),
        Some(json!({"workflowDiscovered": true})),
        &fake,
    );
    let cfg = || read_json(&dir.path().join("jira_config.json"));
    assert!(!w.fields_complete);
    assert!(!w.ensure_workflow_known().unwrap());
    assert!(cfg().get("requestTypeField").is_none(), "no empty discovery defaults persisted");
    fake.with(|j| {
        j.fields = json!([{"id": "customfield_87654", "name": "Arbitrary portal label",
                           "schema": {"custom": crate::discovery::REQUEST_TYPE_SCHEMA}}])
    });
    assert!(w.ensure_workflow_known().unwrap());
    assert_eq!(w.req_field, "customfield_87654");
    assert!(w.issue_fields.contains(&"customfield_87654".to_owned()));
    assert_eq!(cfg()["requestTypeField"], json!("customfield_87654"));
    assert_eq!(cfg()["contextFieldIds"], json!([]));
}

/// The real loop with every stage replaced (Python's `runtime()` fixture).
struct Fake {
    state: Map<String, Value>,
    stop: FakeStop,
    events: RefCell<Vec<&'static str>>,
    attempts: Vec<bool>,
}

struct FakeStop {
    stopped: RefCell<bool>,
    cancel: bool,
    waits: RefCell<Vec<f64>>,
}

impl Waiter for FakeStop {
    fn is_set(&self) -> bool {
        *self.stopped.borrow()
    }
    fn wait(&self, seconds: f64) -> bool {
        self.waits.borrow_mut().push(seconds);
        if self.cancel {
            *self.stopped.borrow_mut() = true;
        }
        *self.stopped.borrow()
    }
}

impl Cycle for Fake {
    fn waiter(&self) -> &dyn Waiter {
        &self.stop
    }
    fn state(&mut self) -> &mut Map<String, Value> {
        &mut self.state
    }
    fn log_error(&self, _: &str) {}
    fn ensure_workflow_known(&mut self) -> Result<bool, String> {
        Ok(self.attempts.remove(0))
    }
    fn backfill(&mut self) -> Result<(), String> {
        self.state.insert("backfillDone".into(), json!(true));
        self.events.borrow_mut().push("backfill");
        Ok(())
    }
    fn predict_intake(&mut self) -> Result<(), String> {
        self.events.borrow_mut().push("predict");
        *self.stop.stopped.borrow_mut() = true;
        Ok(())
    }
    fn resolve_and_learn(&mut self) -> Result<(), String> {
        self.events.borrow_mut().push("UNEXPECTED poll after quit");
        Ok(())
    }
    fn maybe_daily_tune(&mut self) -> Result<(), String> {
        self.events.borrow_mut().push("UNEXPECTED tune after quit");
        Ok(())
    }
    fn write_report(&mut self) -> Result<(), String> {
        self.events.borrow_mut().push("health");
        Ok(())
    }
    fn close(&mut self) {
        self.events.borrow_mut().push("closed");
    }
}

fn runtime(cancel: bool) -> Fake {
    Fake {
        state: Map::new(),
        stop: FakeStop { stopped: RefCell::new(false), cancel, waits: RefCell::new(vec![]) },
        events: RefCell::new(vec![]),
        attempts: vec![false, true],
    }
}

#[test]
fn discovery_retries_and_quit_interrupts_its_backoff() {
    let mut w = runtime(false);
    run_forever(&mut w);
    assert_eq!(*w.stop.waits.borrow(), [30.0]);
    assert_eq!(*w.events.borrow(), ["health", "backfill", "predict", "closed"]);

    let mut w = runtime(true);
    run_forever(&mut w);
    assert_eq!(*w.events.borrow(), ["health", "closed"]);
    assert!(w.state.get("runtimeProblem").is_some_and(|p| p.as_str().is_some_and(|s| !s.is_empty())));
}

#[test]
fn partial_historical_reads_stay_resumable() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    fake.with(|j| j.fail_search = true);
    let mut w = watcher(
        dir.path(),
        json!({"projectKey": "GENERIC", "resolvedStatuses": ["Finished by service desk"], "triageStatuses": []}),
        Some(json!({"backfillCount": 12})),
        &fake,
    );
    w.backfill().unwrap();
    assert_eq!(w.state["backfillDone"], json!(false));
    assert_eq!(w.state["backfillCount"], json!(12));
    fake.with(|j| j.fail_search = false);
    w.backfill().unwrap();
    assert_eq!(w.state["backfillDone"], json!(true));
    assert_eq!(w.state["backfillCount"], json!(12));
}

#[test]
fn the_activity_log_rotates_and_metrics_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = watcher(dir.path(), json!({}), None, &FakeJira::new());
    w.log_rotate_bytes = 200;
    let big = obj(json!({"event": "routed", "key": "SD-1", "summary": "x".repeat(80)}));
    for _ in 0..5 {
        w.log(big.clone()).unwrap();
    }
    let log = dir.path().join("triage_log.jsonl");
    assert!(fs::metadata(&log).unwrap().len() <= 400);
    assert!(dir.path().join("triage_log.jsonl.old").exists());
    let mut cache = MetricsCache::new(&log);
    cache.metrics = Some(Default::default());
    cache.offset = 1_000_000_000; // pretend we had read far more than exists
    cache.live(&|d| d.to_owned());
    assert!(cache.offset <= fs::metadata(&log).unwrap().len());
}

#[test]
fn shutdown_joins_and_saves_the_writer() {
    for update in [true, false] {
        let sequence = Arc::new(Mutex::new(Vec::<&str>::new()));
        let stop = jo_core::stop::Stop::new();
        let worker_stop = stop.clone();
        let log = sequence.clone();
        let worker = spawn_worker(log.clone(), move |seq| {
            worker_stop.wait(Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(30));
            seq.lock().unwrap().push("writer-finished");
        })
        .unwrap();
        let requested = Arc::new(AtomicBool::new(false));
        if update {
            let (flag, signal, seq) = (requested.clone(), stop.clone(), sequence.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                seq.lock().unwrap().push("update-requested");
                flag.store(true, Ordering::SeqCst);
                signal.set();
            });
        }
        let reasons = Mutex::new(Vec::new());
        follow_overlay(
            &stop,
            Duration::from_millis(10),
            &requested,
            || update,
            |r, quit| {
                reasons.lock().unwrap().push((r.to_owned(), quit));
            },
        );
        let seq = worker.join().unwrap();
        seq.lock().unwrap().push("saved");
        let got = sequence.lock().unwrap().clone();
        if update {
            assert_eq!(got, ["update-requested", "writer-finished", "saved"]);
            assert_eq!(*reasons.lock().unwrap(), [("update".to_owned(), false)]);
        } else {
            assert_eq!(got, ["writer-finished", "saved"]);
            assert_eq!(*reasons.lock().unwrap(), [("user-quit".to_owned(), true)]);
        }
    }
}

#[test]
fn a_panicking_writer_still_hands_back_its_state() {
    let worker = spawn_worker(41, |n| {
        *n += 1;
        panic!("writer bug");
    })
    .unwrap();
    assert_eq!(worker.join().unwrap(), 42);
}
