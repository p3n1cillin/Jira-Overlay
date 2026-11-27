//! The Jira Overlay desktop UI: always-on-top queue overlay, tray icon,
//! alerts, settings, setup and the team report.
//!
//! Port of `jira_overlay.pyw`.

use std::path::Path;

use jo_core::lifecycle::Session;

/// How the overlay ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// Quit, setup cancelled or stopped for an update (reason already recorded).
    Quit,
    /// "Reconfigure credentials": the config was removed; the caller releases
    /// the single-instance mutex and starts a fresh overlay process.
    Restart,
}

/// Run the overlay until Quit, Reconfigure or an update stop request.
///
/// The caller has already checked the security hold, taken the
/// `JiraOverlaySingleInstance` mutex (a duplicate launch instead signals the
/// show event and exits) and opened `session`. This function creates and
/// waits on the show event, runs first-time setup when there is no config,
/// launches `<current exe> --learner` unless supervised, listens for the
/// per-root stop event and records quit reasons on `session`.
///
/// `Err(message)` is a fatal start-up error to show the user.
pub fn run(root: &Path, session: &Session) -> Result<Exit, String> {
    let _ = (root, session);
    Err("The Rust overlay is not implemented yet.".into())
}
