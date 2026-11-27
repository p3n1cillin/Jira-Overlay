//! The worker loop (`run_forever`) and the process shell around it (`main`).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use jo_core::lifecycle::now_epoch;
use jo_core::stop::Stop;
use serde_json::{Map, Value, json};

use crate::watcher::{FAST_POLL_SECONDS, POLL_SECONDS, Watcher};

pub const OVERLAY_FOLLOW_SECONDS: f64 = 2.0;
const BACKFILL_RETRY_SECONDS: f64 = 300.0;

/// The stop signal as the loop sees it (tests record the waits).
pub trait Waiter {
    fn is_set(&self) -> bool;
    /// Sleep up to `seconds`; true as soon as stop is requested.
    fn wait(&self, seconds: f64) -> bool;
}

impl Waiter for Stop {
    fn is_set(&self) -> bool {
        Stop::is_set(self)
    }

    fn wait(&self, seconds: f64) -> bool {
        Stop::wait(self, Duration::from_secs_f64(seconds))
    }
}

/// The stages `run_forever` drives. `Err` is a Python exception.
pub trait Cycle {
    fn waiter(&self) -> &dyn Waiter;
    fn state(&mut self) -> &mut Map<String, Value>;
    fn log_error(&self, message: &str);
    fn ensure_workflow_known(&mut self) -> Result<bool, String>;
    fn backfill(&mut self) -> Result<(), String>;
    fn predict_intake(&mut self) -> Result<(), String>;
    fn resolve_and_learn(&mut self) -> Result<(), String>;
    fn maybe_daily_tune(&mut self) -> Result<(), String>;
    fn write_report(&mut self) -> Result<(), String>;
    /// Release network resources (Python closed the requests session).
    fn close(&mut self) {}
}

impl Cycle for Watcher {
    fn waiter(&self) -> &dyn Waiter {
        &self.stop
    }
    fn state(&mut self) -> &mut Map<String, Value> {
        &mut self.state
    }
    fn log_error(&self, message: &str) {
        Watcher::log_error(self, message);
    }
    fn ensure_workflow_known(&mut self) -> Result<bool, String> {
        Watcher::ensure_workflow_known(self)
    }
    fn backfill(&mut self) -> Result<(), String> {
        Watcher::backfill(self)
    }
    fn predict_intake(&mut self) -> Result<(), String> {
        Watcher::predict_intake(self)
    }
    fn resolve_and_learn(&mut self) -> Result<(), String> {
        Watcher::resolve_and_learn(self)
    }
    fn maybe_daily_tune(&mut self) -> Result<(), String> {
        Watcher::maybe_daily_tune(self)
    }
    fn write_report(&mut self) -> Result<(), String> {
        Watcher::write_report(self)
    }
}

/// Run a stage, turning a panic into an error like Python's broad guards.
fn stage<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    crate::watcher::guarded(f)
}

fn problem(c: &mut dyn Cycle, message: &str, key: &str) {
    c.state().insert(key.into(), json!(message));
    if let Err(e) = stage(|| c.write_report()) {
        c.log_error(&format!("Could not publish learner health:\n{e}"));
    }
}

/// Discovery (with backoff), backfill, then the fast/slow cycle until stop.
pub fn run_forever(c: &mut dyn Cycle) {
    run_inner(c);
    c.close();
}

fn run_inner(c: &mut dyn Cycle) {
    let mut delay = 30.0_f64;
    while !c.waiter().is_set() {
        match stage(|| c.ensure_workflow_known()) {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => c.log_error(&format!("workflow discovery failed:\n{e}")),
        }
        problem(
            c,
            "Waiting for Jira workflow discovery; retrying automatically. Check access or workflow configuration if this \
             persists.",
            "runtimeProblem",
        );
        if c.waiter().wait(delay) {
            return;
        }
        delay = (delay * 2.0).min(300.0);
    }
    if c.waiter().is_set() {
        return;
    }
    if let Err(e) = stage(|| c.backfill()) {
        c.log_error(&format!("backfill failed:\n{e}"));
    }
    let mut last_backfill = Instant::now();
    let mut last_slow = 0.0_f64;
    while !c.waiter().is_set() {
        let done = c.state().get("backfillDone").is_some_and(triage_engine::text::truthy);
        if !done && last_backfill.elapsed().as_secs_f64() >= BACKFILL_RETRY_SECONDS {
            if let Err(e) = stage(|| c.backfill()) {
                c.log_error(&format!("backfill retry failed:\n{e}"));
            }
            last_backfill = Instant::now();
        }
        // fast: keep suggestions fresh for tickets currently at intake
        let intake = stage(|| {
            c.predict_intake()?;
            if c.state().shift_remove("intakeProblem").is_some_and(|v| triage_engine::text::truthy(&v)) {
                c.write_report()?;
            }
            Ok(())
        });
        if let Err(e) = intake {
            c.log_error(&format!(
                "intake failed:
{e}"
            ));
            problem(
                c,
                "Intake prediction failed; retrying automatically. See the private learner log for details.",
                "intakeProblem",
            );
        }
        if c.waiter().is_set() {
            break;
        }
        // slow: score past suggestions against real routings and learn
        if now_epoch() - last_slow >= POLL_SECONDS {
            match stage(|| c.resolve_and_learn()) {
                Ok(()) => last_slow = now_epoch(),
                Err(e) => {
                    c.log_error(&format!("resolve/learn failed:\n{e}"));
                    problem(
                        c,
                        "Learning poll failed; retained checkpoint will be retried automatically.",
                        "runtimeProblem",
                    );
                }
            }
            if c.waiter().is_set() {
                break;
            }
            if let Err(e) = stage(|| c.maybe_daily_tune()) {
                c.log_error(&format!("daily tune failed:\n{e}"));
            }
        }
        c.waiter().wait(FAST_POLL_SECONDS);
    }
}

/// Start the writer thread. It always hands the watcher back, even after a
/// panic, so the final save still happens.
pub fn spawn_worker<W: Send + 'static>(
    mut worker: W,
    work: impl FnOnce(&mut W) + Send + 'static,
) -> std::io::Result<JoinHandle<W>> {
    thread::Builder::new().name("triage-learner".into()).spawn(move || {
        let _ = catch_unwind(AssertUnwindSafe(|| work(&mut worker)));
        worker
    })
}

/// No tray icon of its own: follow the overlay. Quitting the overlay records
/// the same deliberate quit; an update/setup stop request records "update".
pub fn follow_overlay(
    stop: &Stop,
    every: Duration,
    update_requested: &AtomicBool,
    overlay_wanted: impl Fn() -> bool,
    reason: impl Fn(&str, bool),
) {
    let mut update_recorded = false;
    loop {
        if update_requested.load(Ordering::SeqCst) && !update_recorded {
            reason("update", false);
            update_recorded = true;
        }
        if stop.wait(every) {
            break;
        }
        if !overlay_wanted() {
            reason("user-quit", true);
            stop.set();
        }
    }
    if update_requested.load(Ordering::SeqCst) && !update_recorded {
        reason("update", false);
    }
}
