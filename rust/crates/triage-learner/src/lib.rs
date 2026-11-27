//! The triage learner service: watches how the team routes tickets, learns
//! from it and publishes read-only suggestions for the overlay.
//!
//! Port of `triage_learner.pyw`, `app/triage_jira.py` (via `jo-jira`),
//! `app/triage_storage.py`, `app/triage_support.py`, `triage_eval.py` and
//! `triage_review.py`. File formats are unchanged.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use jo_core::lifecycle::{Session, wanted};

pub mod cli;
pub mod discovery;
pub mod files;
pub mod issue;
pub mod metrics;
pub mod pyfmt;
pub mod report;
pub mod runtime;
pub mod storage;
pub mod support;
pub mod watcher;

pub use watcher::Watcher;

/// Run the learner until Quit (the overlay was quit) or an update/setup stop
/// request, then save state. The caller has already checked the security
/// hold, taken the `JiraTriageLearner` mutex and opened `session`.
///
/// `Err(message)` is a start-up failure to show the user (missing config,
/// unreadable credentials).
pub fn run(root: &Path, session: &Session) -> Result<(), String> {
    let transport = jo_jira::Http::new().map_err(|e| format!("Failed to start:\n\n{e}"))?;
    let watcher = Watcher::open(root, Box::new(transport))?;
    let stop = watcher.stop.clone();
    let error_log = watcher.paths.error_log.clone();
    let worker =
        runtime::spawn_worker(watcher, |w| runtime::run_forever(w)).map_err(|e| format!("Failed to start:\n\n{e}"))?;

    let update = Arc::new(AtomicBool::new(false));
    let (flag, signal) = (update.clone(), stop.clone());
    if let Err(e) = jo_core::ipc::listen_for_stop(root, move || {
        flag.store(true, Ordering::SeqCst);
        signal.set();
    }) {
        files::log_error(&error_log, &format!("could not listen for stop requests: {e}"));
    }
    session.ready();
    runtime::follow_overlay(
        &stop,
        Duration::from_secs_f64(runtime::OVERLAY_FOLLOW_SECONDS),
        &update,
        || wanted(root, "overlay"),
        |reason, quit| session.reason(reason, quit),
    );
    // Do not exit while the writer is still learning or saving: the updater
    // waits for this process to end before replacing anything.
    match worker.join() {
        Ok(mut watcher) => {
            if let Err(e) = watcher.save_state(true) {
                watcher.log_error(&format!("final state save failed: {e}"));
            }
        }
        Err(_) => files::log_error(&error_log, "learner worker ended abnormally; final state save skipped"),
    }
    Ok(())
}

#[cfg(test)]
mod tests;
