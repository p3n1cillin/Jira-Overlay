//! Shared, UI-free building blocks for Jira Overlay: configuration, DPAPI,
//! atomic files, JQL helpers, Windows IPC, lifecycle records, startup
//! registration and the notify-only update check.
//!
//! Everything here mirrors a contract the Python build already established
//! (file formats, kernel object names, registry values), so a Rust build and a
//! Python build interoperate on the same installation folder.

pub mod atomic;
pub mod config;
pub mod dpapi;
pub mod errlog;
pub mod guardian;
pub mod ipc;
pub mod jql;
pub mod lifecycle;
pub mod startup;
pub mod stop;
pub mod updates;

/// Application version, date-based like the Python releases.
pub const VERSION: &str = "2026.11.24";
/// GitHub repository the notify-only update check consults.
pub const REPOSITORY: &str = "p3n1cillin/Jira-Overlay";
/// File whose presence keeps every component closed pending IT approval.
pub const SECURITY_HOLD: &str = ".jira-overlay-security-hold";

/// True when the installation folder carries the incident security hold.
pub fn security_hold(root: &std::path::Path) -> bool {
    root.join(SECURITY_HOLD).exists()
}
