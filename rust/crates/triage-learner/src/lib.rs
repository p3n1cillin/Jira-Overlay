//! The triage learner service: watches how the team routes tickets, learns
//! from it and publishes read-only suggestions for the overlay.
//!
//! Port of `triage_learner.pyw`, `app/triage_jira.py` (via `jo-jira`),
//! `app/triage_storage.py`, `app/triage_support.py`, `triage_eval.py` and
//! `triage_review.py`. File formats are unchanged.

use std::path::Path;

use jo_core::lifecycle::Session;

/// Run the learner until Quit (the overlay was quit) or an update/setup stop
/// request, then save state. The caller has already checked the security
/// hold, taken the `JiraTriageLearner` mutex and opened `session`.
///
/// `Err(message)` is a start-up failure to show the user (missing config,
/// unreadable credentials).
pub fn run(root: &Path, session: &Session) -> Result<(), String> {
    let _ = (root, session);
    Err("The Rust triage learner is not implemented yet.".into())
}

/// Offline command-line tools. Return the process exit code.
pub mod cli {
    use std::path::Path;

    /// `triage_eval.py` equivalent.
    pub fn eval(root: &Path, args: &[String]) -> i32 {
        let _ = (root, args);
        eprintln!("triage eval is not implemented yet");
        2
    }

    /// `triage_review.py` equivalent.
    pub fn review(root: &Path, args: &[String]) -> i32 {
        let _ = (root, args);
        eprintln!("triage review is not implemented yet");
        2
    }
}
