//! Notify-only update checks: 15 s after start, then every 6 hours, plus
//! "Check for updates" on demand. Nothing is ever downloaded or run; a
//! confirmed update opens only the version-bound release page.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jo_core::config::{self, Config};
use jo_core::updates;
use serde_json::Value;

use crate::platform::Platform;
use crate::util;

pub const FIRST_CHECK: Duration = Duration::from_secs(15);
pub const RECHECK: Duration = Duration::from_secs(6 * 60 * 60);
pub const REPORT_PREVIOUS_AFTER: Duration = Duration::from_secs(2);

/// Looks up the newest published version newer than this build.
pub type LatestRelease = Arc<dyn Fn() -> Result<Option<String>, String> + Send + Sync>;

/// The real check: public GitHub releases, no Jira credentials involved.
pub fn github() -> LatestRelease {
    Arc::new(|| {
        let client = updates::GitHub::new().map_err(|e| e.0)?;
        updates::latest_release(&client, jo_core::REPOSITORY, jo_core::VERSION)
            .map(|found| found.map(|r| r.version))
            .map_err(|e| e.0)
    })
}

pub struct UpdateUi {
    pub release: Option<String>,
    pub status: String,
    pub busy: bool,
    notice: Option<String>,
    pub next_check: Option<Instant>,
}

impl UpdateUi {
    pub fn new(now: Instant) -> Self {
        Self {
            release: None,
            status: "Updates have not been checked yet".into(),
            busy: false,
            notice: None,
            next_check: Some(now + FIRST_CHECK),
        }
    }

    /// "Open update download" is enabled only with a release and no check running.
    pub fn can_open(&self) -> bool {
        self.release.is_some() && !self.busy
    }

    /// Start a check; true when the caller should run the lookup.
    pub fn begin(&mut self, manual: bool, cfg: &Config, now: Instant) -> bool {
        if self.busy {
            return false;
        }
        self.next_check = None;
        if !manual && !config::truthy(cfg, "checkForUpdates", true) {
            self.next_check = Some(now + RECHECK);
            return false;
        }
        self.busy = true;
        self.status = "Checking for updates…".into();
        true
    }

    pub fn finished(
        &mut self,
        release: Option<String>,
        error: Option<String>,
        manual: bool,
        cfg: &Config,
        now: Instant,
        platform: &dyn Platform,
    ) {
        self.busy = false;
        self.release = release.clone();
        let text = match (&error, &release) {
            (Some(e), _) => format!("Update check unavailable: {}", util::take_chars(e, 180)),
            (None, Some(v)) => format!("Version {v} is available"),
            (None, None) => format!("No newer published release (installed {})", jo_core::VERSION),
        };
        self.status = text.clone();
        self.next_check = Some(now + RECHECK);
        if manual {
            platform.info("Application updates", &text);
        } else if let Some(v) = release
            && self.notice.as_ref() != Some(&v)
        {
            self.notice = Some(v);
            if config::truthy(cfg, "notificationsEnabled", true) {
                platform.toast("Jira Overlay update", &format!("{text}. Open Settings to update."));
            }
        }
    }

    /// "Open update download". `automatic` never acts: background checks
    /// only ever announce.
    pub fn open_download(&mut self, root: &Path, automatic: bool, platform: &dyn Platform) {
        let Some(version) = self.release.clone() else { return };
        if self.busy || automatic {
            return;
        }
        if updates::source_checkout(root) {
            platform.info(
                "Git checkout",
                "Close both apps and use git pull --ff-only to update this checkout. Automatic updates never overwrite Git working files.",
            );
            return;
        }
        let page = match updates::release_page(&version) {
            Ok(page) => page,
            Err(e) => return self.failed(&e.0, false, root, platform),
        };
        if !platform.ask_yes_no(
            "Application update",
            "Open the release page? Download and run the reviewed installer manually under your IT policy. The app will not execute downloads or restart automatically.",
        ) {
            return;
        }
        if !platform.open_url(&page) {
            return self.failed("Windows could not open the release page", false, root, platform);
        }
        self.status = "Release page opened. Installation requires explicit user/IT approval.".into();
    }

    pub fn failed(&mut self, detail: &str, record: bool, root: &Path, platform: &dyn Platform) {
        let detail = if detail.contains("[JIRA-") { detail.to_owned() } else { format!("[JIRA-U100] {detail}") };
        self.busy = false;
        self.status = format!("Update failed: {detail}");
        if record && let Some(version) = &self.release {
            let _ = updates::record_update(root, version, "failed", &detail, true);
        }
        platform.error("Application update", &detail);
    }

    /// A failed update recorded by an earlier run is reported exactly once.
    pub fn report_previous(&mut self, root: &Path, platform: &dyn Platform) {
        let previous = updates::update_state(root);
        let failed = previous.get("phase").and_then(Value::as_str) == Some("failed");
        if !failed || util::truthy(previous.get("notified")) {
            return;
        }
        let detail = previous.get("detail").and_then(Value::as_str).unwrap_or("Update failed").to_owned();
        if let Some(version) = previous.get("version").and_then(Value::as_str) {
            let _ = updates::record_update(root, version, "failed", &detail, true);
        }
        self.failed(&detail, false, root, platform);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakePlatform;
    use serde_json::json;

    fn cfg(v: Value) -> Config {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn schedule_and_manual_check() {
        let now = Instant::now();
        let p = FakePlatform::default();
        let mut u = UpdateUi::new(now);
        assert_eq!(u.next_check, Some(now + FIRST_CHECK), "first check 15 s after start");
        assert!(!u.can_open());
        assert!(u.begin(true, &cfg(json!({})), now));
        assert_eq!(u.status, "Checking for updates…");
        assert!(!u.begin(true, &cfg(json!({})), now), "one check at a time");
        u.finished(Some("2099.01.01".into()), None, true, &cfg(json!({})), now, &p);
        assert!(u.status.contains("2099.01.01") && u.can_open());
        assert_eq!(u.next_check, Some(now + RECHECK), "then every 6 hours");
        assert_eq!(
            p.with(|r| r.messages.clone()),
            [("Application updates".into(), "Version 2099.01.01 is available".into())]
        );

        let off = cfg(json!({"checkForUpdates": false}));
        assert!(!u.begin(false, &off, now), "background checks honour the opt-out");
        assert_eq!(u.next_check, Some(now + RECHECK));
        assert!(u.begin(true, &off, now), "a manual check still runs");
        u.finished(None, Some("offline".into()), false, &off, now, &p);
        assert_eq!(u.status, "Update check unavailable: offline");
        u.finished(None, None, false, &off, now, &p);
        assert_eq!(u.status, format!("No newer published release (installed {})", jo_core::VERSION));
    }

    #[test]
    fn background_notice_once_per_version() {
        let now = Instant::now();
        let p = FakePlatform::default();
        let mut u = UpdateUi::new(now);
        for _ in 0..2 {
            u.finished(Some("2099.01.01".into()), None, false, &cfg(json!({})), now, &p);
        }
        assert_eq!(
            p.with(|r| r.toasts.clone()),
            [("Jira Overlay update".into(), "Version 2099.01.01 is available. Open Settings to update.".into())]
        );
        let quiet = FakePlatform::default();
        let mut u = UpdateUi::new(now);
        u.finished(Some("2099.01.02".into()), None, false, &cfg(json!({"notificationsEnabled": false})), now, &quiet);
        assert!(quiet.with(|r| r.toasts.is_empty()));
    }

    #[test]
    fn opening_the_download_is_confirmed_and_bound_to_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let p = FakePlatform::default();
        *p.answer.lock().unwrap() = true;
        let mut u = UpdateUi::new(Instant::now());
        u.release = Some("2099.01.01".into());
        u.open_download(dir.path(), true, &p);
        assert!(p.with(|r| r.urls.is_empty() && r.questions.is_empty()), "background checks never open anything");
        u.open_download(dir.path(), false, &p);
        assert_eq!(
            p.with(|r| r.urls.clone()),
            [format!("https://github.com/{}/releases/tag/v2099.01.01", jo_core::REPOSITORY)]
        );
        assert!(u.status.starts_with("Release page opened"));
        *p.answer.lock().unwrap() = false;
        u.open_download(dir.path(), false, &p);
        assert_eq!(p.with(|r| r.urls.len()), 1, "declining causes no action");
        *p.answer.lock().unwrap() = true;
        u.release = Some("2099.01.02".into());
        u.open_download(dir.path(), false, &p);
        assert!(p.with(|r| r.urls.last().unwrap().ends_with("/v2099.01.02")));

        u.release = Some("../../evil".into());
        u.open_download(dir.path(), false, &p);
        assert!(u.status.starts_with("Update failed: [JIRA-U100]"), "{}", u.status);

        std::fs::create_dir(dir.path().join(".git")).unwrap();
        u.release = Some("2099.01.03".into());
        u.open_download(dir.path(), false, &p);
        assert!(p.with(|r| r.messages.iter().any(|(_, m)| m.contains("git pull --ff-only"))));
        assert_eq!(p.with(|r| r.urls.len()), 2, "source checkouts get instructions, not a browser");
    }

    #[test]
    fn previous_failure_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        updates::record_update(dir.path(), "2099.01.01", "failed", "disk full", false).unwrap();
        let p = FakePlatform::default();
        let mut u = UpdateUi::new(Instant::now());
        u.report_previous(dir.path(), &p);
        assert_eq!(u.status, "Update failed: [JIRA-U100] disk full");
        assert_eq!(p.with(|r| r.messages.len()), 1);
        u.report_previous(dir.path(), &p);
        assert_eq!(p.with(|r| r.messages.len()), 1, "reported once");
        assert_eq!(updates::update_state(dir.path())["notified"], true);
    }
}
