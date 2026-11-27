//! The overlay's UI-free state machine: what each poll result means for the
//! rows, status line, tray badge, notifications and visibility (Python
//! `_update_ui`, show/hide with fades, snooze and the row flash).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use jo_core::config::{self, Config};
use serde_json::{Map, Value};

use crate::alerts::{self, SeenSet};
use crate::learner::Suggestions;
use crate::links;
use crate::model::Snapshot;
use crate::placement;
use crate::platform::Platform;
use crate::poll::Shared;
use crate::stats;
use crate::util;

/// Delay before the overlay appears for a genuinely new ticket, so the
/// toast lands first.
pub const NEW_ALERT_SHOW_DELAY: Duration = Duration::from_millis(2500);
pub const FADE: Duration = Duration::from_millis(190);
/// The row-change highlight: 14 steps of 28 ms.
pub const FLASH: Duration = Duration::from_millis(14 * 28);
pub const RELATIVE_TIME_EVERY: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Dim,
    Ok,
    Warn,
    Alert,
}

/// One queue row as displayed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowView {
    pub name: String,
    pub id: String,
    pub count: u64,
    pub is_alert: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fade {
    start: f32,
    target: f32,
    began: Instant,
    unmap_when_done: bool,
}

/// Tray badge state: `(count, alert, queue name)`.
pub type TrayState = (u64, bool, String);

pub struct OverlayState {
    pub shared: Arc<Shared>,
    /// The latest poll result.
    pub snap: Snapshot,
    /// A hard error replacing the rows until the next good poll.
    pub body_error: Option<String>,
    pub rows: Vec<RowView>,
    pub empty_note: Option<&'static str>,
    pub status: (String, Tone),
    pub done_text: String,
    /// `"✓  5 done"`: the same figure when the row also carries at-risk.
    pub done_short: String,
    pub sla: Option<(String, Tone)>,
    /// `"5d 94% · 20d 95%"` beside today's SLA %, once fetched.
    pub sla_trend: Option<String>,
    /// `"⚠ 3 at risk"` while any open ticket's SLA is about to breach.
    pub at_risk: Option<String>,
    /// At-risk keys from the last reading (`None` before the first, which
    /// never toasts).
    at_risk_seen: Option<HashSet<String>>,
    /// Tickets that arrived while snoozed, in arrival order.
    pub snoozed_arrivals: Vec<String>,
    prev_counts: HashMap<String, u64>,
    pub flashes: HashMap<String, Instant>,
    pub seen: SeenSet,
    pub prev_newly: Option<u64>,
    pub dismissed: bool,
    pub manual_show: bool,
    pub snooze_until: Option<DateTime<Local>>,
    pub last_refresh: Option<Instant>,
    pub suggestions: Map<String, Value>,
    suggestions_file: Suggestions,
    tray_last: Option<TrayState>,
    /// A tray change for the UI thread to apply.
    pub tray_pending: Option<TrayState>,
    /// Logically shown (Python `_visible`).
    pub visible: bool,
    /// Pending delayed show (Python `_show_job`).
    pub show_at: Option<Instant>,
    /// Current window alpha and whether the window is mapped at all.
    pub alpha: f32,
    pub mapped: bool,
    pub fade: Option<Fade>,
    pub in_settings: bool,
    pub geom_animating: bool,
    /// Fit the window to its content and re-place it (Python `_resize`).
    pub resize_requested: bool,
    pub tooltip_open: bool,
}

impl OverlayState {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            snap: Snapshot::default(),
            body_error: None,
            rows: Vec::new(),
            empty_note: None,
            status: ("Connecting…".into(), Tone::Dim),
            done_text: "✓  Completed today — —".into(),
            done_short: "✓  — done".into(),
            sla: None,
            sla_trend: None,
            at_risk: None,
            at_risk_seen: None,
            snoozed_arrivals: Vec::new(),
            prev_counts: HashMap::new(),
            flashes: HashMap::new(),
            seen: SeenSet::default(),
            prev_newly: None,
            dismissed: false,
            manual_show: false,
            snooze_until: None,
            last_refresh: None,
            suggestions: Map::new(),
            suggestions_file: Suggestions::default(),
            tray_last: None,
            tray_pending: None,
            visible: false,
            show_at: None,
            alpha: 0.0,
            mapped: false,
            fade: None,
            in_settings: false,
            geom_animating: false,
            resize_requested: false,
            tooltip_open: false,
        }
    }

    pub fn config(&self) -> Config {
        self.shared.config()
    }

    pub fn is_snoozed(&mut self) -> bool {
        match self.snooze_until {
            Some(until) if Local::now() < until => true,
            _ => {
                self.snooze_until = None;
                false
            }
        }
    }

    /// Repaint from the latest poll (Python `_update_ui`).
    pub fn update_ui(&mut self, now: Instant, platform: &dyn Platform) {
        let cfg = self.config();
        if self.snap.conn_error {
            // Transient network loss: keep the rows, say so quietly.
            self.status = ("Reconnecting…".into(), Tone::Warn);
            return;
        }
        self.suggestions = self.suggestions_file.load(&self.shared.root, &cfg);

        if let Some(err) = self.snap.error_msg.clone() {
            self.rows.clear();
            self.flashes.clear();
            self.empty_note = None;
            self.body_error = Some(err);
            self.status = ("Tap ↻ to retry".into(), Tone::Alert);
            self.resize_requested = true;
            return;
        }
        self.body_error = None;

        let current: HashSet<String> = self.snap.queues.iter().map(|q| q.name.clone()).collect();
        self.flashes.retain(|name, _| current.contains(name));
        // An empty box is ambiguous: say whether the desk has no queues or
        // the user's filters hid them all.
        self.empty_note = self.snap.queues.is_empty().then_some(if self.snap.all_queue_names.is_empty() {
            "No queues found in this service desk"
        } else {
            "All queues are hidden — see Settings"
        });

        let alert_qs: HashSet<String> = util::alert_queues(&cfg).into_iter().collect();
        let mut rows = Vec::with_capacity(self.snap.queues.len());
        for q in &self.snap.queues {
            let is_alert = alert_qs.contains(&q.name);
            // Alert queues use the live reconciled count: JSM's cached
            // includeCount can be a stale non-zero for an empty queue.
            let count = if is_alert {
                self.snap.reconciled.get(&q.name).copied().unwrap_or(q.issue_count)
            } else {
                q.issue_count
            };
            if self.prev_counts.get(&q.name).is_some_and(|prev| *prev != count) {
                self.flashes.insert(q.name.clone(), now);
            }
            self.prev_counts.insert(q.name.clone(), count);
            rows.push(RowView { name: q.name.clone(), id: q.id.clone(), count, is_alert });
        }
        self.rows = rows;

        let newly_count = self.snap.alert_count;
        let truly_new = self.seen.update(&self.snap.new_issues);
        // A genuinely new ticket cancels any manual dismissal.
        if !truly_new.is_empty() {
            self.dismissed = false;
        }
        let snoozed = self.is_snoozed();
        // The first poll never alerts: everything in the queue is "new" then.
        let has_new_alert = !truly_new.is_empty() && self.prev_newly.is_some();
        let notify = config::truthy(&cfg, "notificationsEnabled", true);
        if has_new_alert && !snoozed {
            let (summaries, queues) = alerts::lookups(&self.snap.new_issues);
            let (title, body) = alerts::compose_alert(&truly_new, &summaries, &queues, &self.suggestions);
            if notify {
                let open = match truly_new.as_slice() {
                    [key] => links::ticket(&cfg, key),
                    _ => self.queue_link(&cfg, &truly_new),
                };
                platform.toast(&title, &body, Some(&open));
            }
            if config::truthy(&cfg, "soundEnabled", true) {
                platform.beep();
            }
        } else if has_new_alert {
            // Snoozed: no individual alerts, but remembered for the catch-up.
            for key in truly_new {
                if !self.snoozed_arrivals.contains(&key) {
                    self.snoozed_arrivals.push(key);
                }
            }
        }
        if !snoozed && !self.snoozed_arrivals.is_empty() {
            self.snooze_catch_up(&cfg, notify, platform);
        }
        self.prev_newly = Some(newly_count);
        self.last_refresh = Some(now);

        let day = stats::working_day_window(stats::today(), stats::weekends_off(&cfg)).1;
        self.done_text = format!("✓  Completed {day} — {}", self.snap.completed_today);
        let suffix = if day == "today" { String::new() } else { format!(" {day}") };
        self.done_short = format!("✓  {} done{suffix}", self.snap.completed_today);
        self.sla = self.snap.sla_compliance.map(|pct| {
            let tone = if pct >= 95 {
                Tone::Ok
            } else if pct >= 80 {
                Tone::Warn
            } else {
                Tone::Alert
            };
            (format!("SLA {pct}%"), tone)
        });
        self.sla_trend = self.sla.as_ref().and(self.snap.sla_trend.as_ref()).map(stats::trend_text);
        self.at_risk_update(&cfg, notify && !snoozed, platform);

        let tray_queue = tray_queue(&cfg);
        let wanted = tray_queue.to_lowercase();
        let tray_count = self
            .snap
            .queues
            .iter()
            .find(|q| q.name.to_lowercase().contains(&wanted))
            .map_or(newly_count, |q| self.snap.reconciled.get(&q.name).copied().unwrap_or(q.issue_count));
        self.set_tray((tray_count, newly_count > 0, tray_queue));

        self.status = if snoozed {
            let until = self.snooze_until.map(|u| u.format("%H:%M").to_string()).unwrap_or_default();
            (format!("Snoozed until {until}"), Tone::Warn)
        } else if !self.snap.alert_queues_missing.is_empty() {
            (format!("Alert queue not found: {}", self.snap.alert_queues_missing.join(", ")), Tone::Warn)
        } else {
            (format!("● Live · {}", Local::now().format("%H:%M:%S")), Tone::Ok)
        };

        // Settings and transitions manage the window themselves.
        if !self.in_settings && !self.geom_animating {
            let show = self.manual_show
                || (((newly_count > 0 && !self.dismissed) || config::truthy(&cfg, "alwaysVisible", false)) && !snoozed);
            if show {
                self.resize_requested = true;
                if !self.visible && self.show_at.is_none() {
                    self.show_at = Some(now + if has_new_alert { NEW_ALERT_SHOW_DELAY } else { Duration::ZERO });
                }
            } else {
                self.show_at = None;
                if self.visible {
                    self.do_hide(now);
                }
            }
        }
    }

    /// The Jira queue holding the first of `keys`, else the desk's queues.
    fn queue_link(&self, cfg: &Config, keys: &[String]) -> String {
        let queue = keys
            .iter()
            .find_map(|k| self.snap.new_issues.iter().find(|i| &i.key == k))
            .and_then(|i| self.snap.alert_queue_ids.get(&i.queue));
        match queue {
            Some(id) => links::queue(cfg, id),
            None => links::desk(cfg),
        }
    }

    /// The snooze ended: one summary toast for the tickets that arrived
    /// meanwhile and are still waiting (never individual alerts).
    fn snooze_catch_up(&mut self, cfg: &Config, notify: bool, platform: &dyn Platform) {
        let present: HashSet<&str> = self.snap.new_issues.iter().map(|i| i.key.as_str()).collect();
        let arrivals = std::mem::take(&mut self.snoozed_arrivals);
        let waiting: Vec<String> = arrivals.into_iter().filter(|k| present.contains(k.as_str())).collect();
        if waiting.is_empty() || !notify {
            return;
        }
        let (summaries, _) = alerts::lookups(&self.snap.new_issues);
        let (title, body) = alerts::compose_catch_up(&waiting, &summaries);
        platform.toast(&title, &body, Some(&self.queue_link(cfg, &waiting)));
    }

    /// The "⚠ n at risk" label, and a toast for tickets newly at risk.
    fn at_risk_update(&mut self, cfg: &Config, may_toast: bool, platform: &dyn Platform) {
        let Some(risk) = &self.snap.at_risk else {
            self.at_risk = None;
            self.at_risk_seen = None;
            return;
        };
        self.at_risk = (risk.count > 0).then(|| format!("⚠ {} at risk", risk.count));
        let keys: HashSet<String> = risk.tickets.iter().map(|(k, _)| k.clone()).collect();
        if let Some(previous) = &self.at_risk_seen {
            let newly: Vec<(String, Option<String>)> =
                risk.tickets.iter().filter(|(k, _)| !previous.contains(k)).cloned().collect();
            if !newly.is_empty() && may_toast && config::truthy(cfg, "slaAtRiskToast", true) {
                let (title, body) = alerts::compose_at_risk(&newly, stats::at_risk_minutes(cfg));
                let open = stats::sla_at_risk_jql(cfg).map(|jql| links::jql(cfg, &jql));
                platform.toast(&title, &body, open.as_deref());
            }
        }
        self.at_risk_seen = Some(keys);
    }

    fn set_tray(&mut self, state: TrayState) {
        if self.tray_last.as_ref() != Some(&state) {
            self.tray_last = Some(state.clone());
            self.tray_pending = Some(state);
        }
    }

    /// Show/hide timers and fades; returns true while something animates.
    pub fn tick(&mut self, now: Instant) -> bool {
        if self.show_at.is_some_and(|at| now >= at) {
            self.do_show(now);
        }
        if let Some(f) = self.fade {
            let t = now.saturating_duration_since(f.began).as_secs_f32() / FADE.as_secs_f32();
            if t >= 1.0 {
                self.alpha = f.target;
                self.fade = None;
                if f.unmap_when_done {
                    self.mapped = false;
                }
            } else {
                self.alpha = (f.start + (f.target - f.start) * placement::ease_out(t)).clamp(0.0, 1.0);
            }
        }
        self.flashes.retain(|_, began| now.saturating_duration_since(*began) < FLASH);
        self.fade.is_some() || !self.flashes.is_empty()
    }

    /// Row background during its flash: highlight easing back to the base.
    pub fn flash_t(&self, name: &str, now: Instant) -> Option<f32> {
        let began = self.flashes.get(name)?;
        let steps = (now.saturating_duration_since(*began).as_millis() / 28).min(14) as f32;
        Some(steps / 14.0)
    }

    fn target_alpha(&self) -> f32 {
        util::cfg_float(&self.config(), "alpha", 0.93) as f32
    }

    /// Python `_do_show`.
    pub fn do_show(&mut self, now: Instant) {
        self.show_at = None;
        if self.in_settings || self.geom_animating {
            return; // the settings panel manages its own visibility
        }
        if self.visible && self.fade.is_none() {
            return;
        }
        self.visible = true;
        self.mapped = true;
        self.resize_requested = true;
        let target = self.target_alpha();
        self.animate_alpha(target, false, now);
    }

    /// Python `_do_hide`.
    pub fn do_hide(&mut self, now: Instant) {
        self.show_at = None;
        if !self.visible && self.fade.is_none() {
            return;
        }
        self.tooltip_open = false;
        self.visible = false;
        self.animate_alpha(0.0, true, now);
    }

    pub fn animate_alpha(&mut self, target: f32, unmap_when_done: bool, now: Instant) {
        self.fade = None;
        if (target - self.alpha).abs() < 0.001 {
            self.alpha = target;
            if unmap_when_done {
                self.mapped = false;
            }
            return;
        }
        self.fade = Some(Fade { start: self.alpha, target, began: now, unmap_when_done });
    }

    /// Explicit show (tray): stays up until hidden by hand.
    pub fn user_show(&mut self, now: Instant) {
        self.manual_show = true;
        self.dismissed = false;
        self.do_show(now);
    }

    /// Explicit hide (footer, tray, Dismiss): stays hidden until a NEW ticket.
    pub fn user_hide(&mut self, now: Instant) {
        self.manual_show = false;
        self.dismissed = true;
        self.do_hide(now);
    }

    /// "Dismiss — hide until new tickets": displayed tickets are already in
    /// the seen-set, so nothing re-notifies.
    pub fn mark_all_seen(&mut self, now: Instant) {
        self.tooltip_open = false;
        self.user_hide(now);
    }

    pub fn snooze(&mut self, minutes: i64, now: Instant, platform: &dyn Platform) {
        self.snooze_until = Some(Local::now() + chrono::Duration::minutes(minutes));
        self.manual_show = false; // snoozing means "get out of my way"
        self.update_ui(now, platform);
    }

    pub fn cancel_snooze(&mut self, now: Instant, platform: &dyn Platform) {
        self.snooze_until = None;
        self.update_ui(now, platform);
    }

    /// Flag stale data when polls have stopped landing (every 30 s).
    pub fn relative_time(&mut self, now: Instant) {
        let Some(last) = self.last_refresh else { return };
        if self.snap.error_msg.is_some() || self.snap.conn_error || self.is_snoozed() {
            return;
        }
        let secs = now.saturating_duration_since(last).as_secs();
        if secs >= 90 {
            let rel = if secs < 3600 { format!("{}m", secs / 60) } else { format!("{}h", secs / 3600) };
            self.status = (format!("Updated {rel} ago"), Tone::Warn);
        }
    }
}

/// The tray badge's queue: `trayBadgeQueue`, or "Newly Opened" when absent
/// or empty (deliberate change: an empty name matched every queue).
pub fn tray_queue(cfg: &Config) -> String {
    match config::get_str(cfg, "trayBadgeQueue", "") {
        "" => "Newly Opened".to_owned(),
        name => name.to_owned(),
    }
}

/// Tray tooltip text.
pub fn tray_tooltip(state: &TrayState) -> String {
    let (count, _, name) = state;
    if *count == 0 {
        format!("Jira — {name}: 0")
    } else {
        format!("Jira — {name}: {count} {}", if *count == 1 { "ticket" } else { "tickets" })
    }
}

/// Header label sized to the space available: letter-spaced if it fits,
/// else plain caps, else cut with an ellipsis.
pub fn fit_title(desk: &str, avail: f32, measure: impl Fn(&str) -> f32) -> String {
    let mut name: Vec<char> =
        if desk.is_empty() { "JIRA".chars().collect() } else { desk.to_uppercase().chars().collect() };
    let plain: String = name.iter().collect();
    for candidate in [format!("● {}", crate::theme::track(&plain)), format!("● {plain}")] {
        if measure(&candidate) <= avail {
            return candidate;
        }
    }
    while !name.is_empty() && measure(&format!("● {}…", name.iter().collect::<String>())) > avail {
        name.pop();
    }
    format!("● {}…", name.iter().collect::<String>())
}

/// Queue row label: names longer than the width allows are cut.
pub fn row_label(name: &str, overlay_width: i64) -> String {
    let max_chars = 12.max((overlay_width - 100).div_euclid(7)) as usize;
    if name.chars().count() <= max_chars {
        name.to_owned()
    } else {
        format!("{}…", util::take_chars(name, max_chars - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue, Queue};
    use crate::testing::FakePlatform;
    use serde_json::json;

    fn state(cfg: Value) -> (OverlayState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(Shared::new(dir.path(), cfg.as_object().unwrap().clone()));
        (OverlayState::new(shared), dir)
    }

    fn snap(queues: &[(&str, u64)], issues: &[&str], alert_count: u64) -> Snapshot {
        Snapshot {
            queues: queues
                .iter()
                .enumerate()
                .map(|(i, (n, c))| Queue { name: (*n).into(), id: i.to_string(), issue_count: *c })
                .collect(),
            all_queue_names: queues.iter().map(|(n, _)| (*n).into()).collect(),
            new_issues: issues.iter().map(|k| Issue::new(k, Some(&format!("about {k}")), "Open")).collect(),
            alert_count,
            completed_today: 5,
            sla_compliance: Some(98),
            ..Default::default()
        }
    }

    #[test]
    fn first_poll_never_alerts_and_new_tickets_do() {
        let (mut s, _d) = state(json!({"alertQueues": ["Open"]}));
        let p = FakePlatform::default();
        let now = Instant::now();
        s.snap = snap(&[("Open", 2)], &["SD-1", "SD-2"], 2);
        s.update_ui(now, &p);
        assert!(p.with(|r| r.toasts.is_empty() && r.beeps == 0), "first poll never alerts");
        assert_eq!(s.show_at, Some(now), "shown at once for tickets already waiting");
        s.tick(now);
        assert!(s.visible && s.mapped);

        s.snap = snap(&[("Open", 3)], &["SD-1", "SD-2", "SD-3"], 3);
        s.update_ui(now, &p);
        assert_eq!(p.with(|r| r.toasts.clone()), [("Jira — SD-3".into(), "about SD-3\nIn Open".into())]);
        assert_eq!(p.with(|r| r.beeps), 1);
        assert!(s.flashes.contains_key("Open"), "a changed count flashes its row");
        assert_eq!(s.status.1, Tone::Ok);
        assert!(s.status.0.starts_with("● Live · "));
        assert_eq!(s.done_text, "✓  Completed today — 5");
        assert_eq!(s.sla, Some(("SLA 98%".into(), Tone::Ok)));
    }

    #[test]
    fn new_alert_shows_after_delay_and_dismiss_holds_until_new() {
        let (mut s, _d) = state(json!({"alertQueues": ["Open"], "soundEnabled": false, "notificationsEnabled": false}));
        let p = FakePlatform::default();
        let now = Instant::now();
        s.snap = snap(&[("Open", 0)], &[], 0);
        s.update_ui(now, &p);
        assert!(s.show_at.is_none(), "nothing to show");
        s.snap = snap(&[("Open", 1)], &["SD-7"], 1);
        s.update_ui(now, &p);
        assert_eq!(s.show_at, Some(now + NEW_ALERT_SHOW_DELAY), "2.5 s so the toast lands first");
        assert!(p.with(|r| r.toasts.is_empty() && r.beeps == 0), "both switched off");
        s.tick(now + NEW_ALERT_SHOW_DELAY);
        assert!(s.visible);

        s.mark_all_seen(now);
        assert!(!s.visible && s.dismissed);
        s.update_ui(now, &p);
        assert!(s.show_at.is_none(), "dismissed stays hidden while the same tickets wait");
        s.snap = snap(&[("Open", 2)], &["SD-7", "SD-8"], 2);
        s.update_ui(now, &p);
        assert!(!s.dismissed && s.show_at.is_some(), "a genuinely new ticket cancels the dismissal");
    }

    #[test]
    fn snooze_hides_and_suppresses() {
        let (mut s, _d) = state(json!({"alertQueues": ["Open"], "alwaysVisible": true}));
        let p = FakePlatform::default();
        let now = Instant::now();
        s.snap = snap(&[("Open", 1)], &["SD-1"], 1);
        s.update_ui(now, &p);
        s.tick(now);
        assert!(s.visible);
        s.snooze(15, now, &p);
        assert!(!s.visible, "snooze hides even dashboard mode");
        assert!(s.status.0.starts_with("Snoozed until ") && s.status.1 == Tone::Warn);
        s.snap = snap(&[("Open", 2)], &["SD-1", "SD-2"], 2);
        s.update_ui(now, &p);
        assert!(p.with(|r| r.toasts.is_empty() && r.beeps == 0), "no toast or sound while snoozed");
        assert_eq!(s.snoozed_arrivals, ["SD-2"], "but the arrival is remembered");
        s.cancel_snooze(now, &p);
        assert_eq!(
            p.with(|r| r.toasts.clone()),
            [("Jira — 1 ticket arrived while snoozed".into(), "SD-2: about SD-2".into())],
            "one catch-up summary, never the individual alert"
        );
        assert_eq!(p.with(|r| r.beeps), 0);
        assert!(s.snoozed_arrivals.is_empty());
        s.update_ui(now, &p);
        assert_eq!(p.with(|r| r.toasts.len()), 1, "the catch-up fires once");
        assert!(s.show_at.is_some());
    }

    #[test]
    fn snooze_expiry_catches_up_on_tickets_still_waiting() {
        let (mut s, _d) = state(json!({"alertQueues": ["Open"], "domain": "e.atlassian.net", "projectKey": "SD"}));
        let p = FakePlatform::default();
        let now = Instant::now();
        s.snap = snap(&[("Open", 0)], &[], 0);
        s.snap.alert_queue_ids.insert("Open".into(), "7".into());
        s.update_ui(now, &p);
        s.snooze(15, now, &p);
        let ids = s.snap.alert_queue_ids.clone();
        for keys in [&["SD-1"][..], &["SD-1", "SD-2"], &["SD-2", "SD-3", "SD-4", "SD-5"]] {
            s.snap = snap(&[("Open", keys.len() as u64)], keys, keys.len() as u64);
            s.snap.alert_queue_ids = ids.clone();
            s.update_ui(now, &p);
        }
        assert!(p.with(|r| r.toasts.is_empty()));
        // The snooze runs out on its own; SD-1 was handled meanwhile.
        s.snooze_until = Some(Local::now() - chrono::Duration::seconds(1));
        s.update_ui(now, &p);
        let toasts = p.with(|r| r.toasts.clone());
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].0, "Jira — 4 tickets arrived while snoozed");
        assert_eq!(toasts[0].1, "SD-2: about SD-2\nSD-3: about SD-3\nSD-4: about SD-4\n…and 1 more");
        assert_eq!(
            p.with(|r| r.toast_urls.clone()),
            [Some("https://e.atlassian.net/jira/servicedesk/projects/SD/queues/custom/7".into())],
            "clicking it opens the alert queue"
        );

        // Nothing still waiting: no catch-up at all.
        let p = FakePlatform::default();
        s.snooze(15, now, &p);
        s.snap = snap(&[("Open", 5)], &["SD-2", "SD-3", "SD-4", "SD-5", "SD-9"], 5);
        s.update_ui(now, &p);
        s.snap = snap(&[("Open", 0)], &[], 0);
        s.cancel_snooze(now, &p);
        assert!(p.with(|r| r.toasts.is_empty()));
    }

    #[test]
    fn toasts_open_the_ticket_or_the_queue() {
        let (mut s, _d) = state(json!({"alertQueues": ["Open"], "domain": "e.atlassian.net", "projectKey": "SD"}));
        let p = FakePlatform::default();
        let now = Instant::now();
        s.snap = snap(&[("Open", 0)], &[], 0);
        s.update_ui(now, &p);
        s.snap = snap(&[("Open", 1)], &["SD-1"], 1);
        s.update_ui(now, &p);
        s.snap = snap(&[("Open", 3)], &["SD-1", "SD-2", "SD-3"], 3);
        s.snap.alert_queue_ids.insert("Open".into(), "12".into());
        s.update_ui(now, &p);
        assert_eq!(
            p.with(|r| r.toast_urls.clone()),
            [
                Some("https://e.atlassian.net/browse/SD-1".into()),
                Some("https://e.atlassian.net/jira/servicedesk/projects/SD/queues/custom/12".into())
            ]
        );
    }

    #[test]
    fn at_risk_label_and_toasts() {
        use crate::model::AtRisk;
        let risk = |keys: &[&str]| AtRisk {
            count: keys.len() as u64,
            tickets: keys.iter().map(|k| ((*k).to_owned(), Some(format!("about {k}")))).collect(),
        };
        let (mut s, _d) = state(json!({"domain": "e.atlassian.net", "projectKey": "SD"}));
        let p = FakePlatform::default();
        let now = Instant::now();
        s.snap.at_risk = Some(risk(&["SD-1"]));
        s.update_ui(now, &p);
        assert_eq!(s.at_risk.as_deref(), Some("⚠ 1 at risk"));
        assert!(p.with(|r| r.toasts.is_empty()), "already at risk at start-up: no toast");
        s.snap.at_risk = Some(risk(&["SD-1", "SD-2"]));
        s.update_ui(now, &p);
        assert_eq!(s.at_risk.as_deref(), Some("⚠ 2 at risk"));
        assert_eq!(
            p.with(|r| r.toasts.clone()),
            [("SLA at risk — SD-2".into(), "about SD-2\nUnder 30 min of SLA left".into())]
        );
        let url = p.with(|r| r.toast_urls[0].clone()).unwrap();
        assert!(
            url.starts_with("https://e.atlassian.net/issues/?jql=project%20%3D%20SD%20AND%20statusCategory"),
            "{url}"
        );
        assert!(url.contains("remaining%28%2230m%22%29"), "{url}");
        s.update_ui(now, &p);
        assert_eq!(p.with(|r| r.toasts.len()), 1, "the same tickets don't toast again");
        s.snap.at_risk = Some(risk(&[]));
        s.update_ui(now, &p);
        assert_eq!(s.at_risk, None, "nothing at risk, no label");
        s.snap.at_risk = None;
        s.update_ui(now, &p);
        assert_eq!(s.at_risk, None);

        // The toast can be switched off on its own, and snoozing silences it.
        for extra in [json!({"slaAtRiskToast": false}), json!({"notificationsEnabled": false})] {
            let mut c = json!({"projectKey": "SD"});
            c.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            let (mut s, _d) = state(c);
            let p = FakePlatform::default();
            s.snap.at_risk = Some(risk(&[]));
            s.update_ui(now, &p);
            s.snap.at_risk = Some(risk(&["SD-5"]));
            s.update_ui(now, &p);
            assert!(p.with(|r| r.toasts.is_empty()) && s.at_risk.is_some());
        }
        let (mut s, _d) = state(json!({"projectKey": "SD"}));
        let p = FakePlatform::default();
        s.snap.at_risk = Some(risk(&[]));
        s.update_ui(now, &p);
        s.snooze(15, now, &p);
        s.snap.at_risk = Some(risk(&["SD-5"]));
        s.update_ui(now, &p);
        s.cancel_snooze(now, &p);
        assert!(p.with(|r| r.toasts.is_empty()), "at risk while snoozed: no toast, then or later");
    }

    #[test]
    fn sla_trend_text_follows_the_sla_figure() {
        use crate::model::SlaTrend;
        let (mut s, _d) = state(json!({}));
        let p = FakePlatform::default();
        s.snap.sla_compliance = Some(97);
        s.snap.sla_trend = Some(SlaTrend { five: Some(94), twenty: Some(95) });
        s.update_ui(Instant::now(), &p);
        assert_eq!(s.sla_trend.as_deref(), Some("5d 94% · 20d 95%"));
        s.snap.sla_compliance = None;
        s.update_ui(Instant::now(), &p);
        assert_eq!(s.sla_trend, None, "no SLA figure, no trend");
        s.snap.completed_today = 12;
        s.update_ui(Instant::now(), &p);
        assert_eq!(s.done_short, "✓  12 done");
    }

    #[test]
    fn errors_and_reconnecting() {
        let (mut s, _d) = state(json!({}));
        let p = FakePlatform::default();
        let now = Instant::now();
        s.snap = snap(&[("Q", 4)], &[], 0);
        s.update_ui(now, &p);
        assert_eq!(s.rows.len(), 1);
        s.snap.conn_error = true;
        s.update_ui(now, &p);
        assert_eq!(s.status, ("Reconnecting…".into(), Tone::Warn));
        assert_eq!(s.rows.len(), 1, "rows stay during a reconnect");
        s.snap.conn_error = false;
        s.snap.error_msg = Some("HTTP 401 — check credentials".into());
        s.update_ui(now, &p);
        assert!(s.rows.is_empty() && s.body_error.is_some());
        assert_eq!(s.status, ("Tap ↻ to retry".into(), Tone::Alert));
        s.snap = Snapshot { all_queue_names: vec!["Hidden".into()], ..Default::default() };
        s.update_ui(now, &p);
        assert_eq!(s.empty_note, Some("All queues are hidden — see Settings"));
        s.snap = Snapshot::default();
        s.update_ui(now, &p);
        assert_eq!(s.empty_note, Some("No queues found in this service desk"));
        s.snap.alert_queues_missing = vec!["Gone".into()];
        s.update_ui(now, &p);
        assert_eq!(s.status, ("Alert queue not found: Gone".into(), Tone::Warn));
    }

    #[test]
    fn sla_colours_and_weekend_label() {
        let (mut s, _d) = state(json!({}));
        let p = FakePlatform::default();
        for (pct, tone) in [(95, Tone::Ok), (94, Tone::Warn), (80, Tone::Warn), (79, Tone::Alert)] {
            s.snap.sla_compliance = Some(pct);
            s.update_ui(Instant::now(), &p);
            assert_eq!(s.sla.clone().unwrap().1, tone, "{pct}");
        }
        s.snap.sla_compliance = None;
        s.update_ui(Instant::now(), &p);
        assert!(s.sla.is_none());
    }

    #[test]
    fn tray_badge() {
        let (mut s, _d) = state(json!({"alertQueues": ["Newly Opened"]}));
        let p = FakePlatform::default();
        s.snap = snap(&[("Newly Opened", 9), ("Other", 4)], &[], 3);
        s.snap.reconciled.insert("Newly Opened".into(), 3);
        s.update_ui(Instant::now(), &p);
        let badge = s.tray_pending.take().unwrap();
        assert_eq!(badge, (3, true, "Newly Opened".into()), "the reconciled count, like the row");
        assert_eq!(tray_tooltip(&badge), "Jira — Newly Opened: 3 tickets");
        s.update_ui(Instant::now(), &p);
        assert!(s.tray_pending.is_none(), "unchanged state is not redrawn");
        assert_eq!(tray_tooltip(&(1, true, "Q".into())), "Jira — Q: 1 ticket");
        assert_eq!(tray_tooltip(&(0, false, "Q".into())), "Jira — Q: 0");

        // Deliberate change 6: an empty badge queue means "Newly Opened".
        let (mut s, _d) = state(json!({"trayBadgeQueue": ""}));
        s.snap = snap(&[("Alpha", 7), ("Newly Opened", 2)], &[], 0);
        s.update_ui(Instant::now(), &p);
        assert_eq!(s.tray_pending.take().unwrap(), (2, false, "Newly Opened".into()));
        let (mut s, _d) = state(json!({"trayBadgeQueue": "alp"}));
        s.snap = snap(&[("Alpha", 7)], &[], 0);
        s.update_ui(Instant::now(), &p);
        assert_eq!(s.tray_pending.take().unwrap().0, 7, "case-insensitive substring match");
    }

    #[test]
    fn fades_and_relative_time() {
        let (mut s, _d) = state(json!({"alpha": 0.8}));
        let now = Instant::now();
        s.user_show(now);
        assert!(s.manual_show && s.mapped);
        assert!(s.tick(now + FADE / 2));
        assert!(s.alpha > 0.0 && s.alpha < 0.8);
        s.tick(now + FADE);
        assert_eq!(s.alpha, 0.8);
        s.user_hide(now + FADE);
        s.tick(now + FADE * 3);
        assert!(!s.mapped && s.alpha == 0.0, "unmapped once faded out");

        let p = FakePlatform::default();
        s.update_ui(now, &p);
        s.relative_time(now + Duration::from_secs(89));
        assert!(s.status.0.starts_with("● Live"));
        s.relative_time(now + Duration::from_secs(150));
        assert_eq!(s.status, ("Updated 2m ago".into(), Tone::Warn));
        s.relative_time(now + Duration::from_secs(7300));
        assert_eq!(s.status.0, "Updated 2h ago");
    }

    #[test]
    fn settings_block_show() {
        let (mut s, _d) = state(json!({"alwaysVisible": true}));
        s.in_settings = true;
        s.update_ui(Instant::now(), &FakePlatform::default());
        assert!(s.show_at.is_none());
        s.do_show(Instant::now());
        assert!(!s.visible);
    }

    #[test]
    fn title_and_row_labels() {
        let measure = |s: &str| s.chars().count() as f32 * 7.0;
        assert_eq!(fit_title("Desk", 200.0, measure), "● D\u{2009}E\u{2009}S\u{2009}K");
        assert_eq!(fit_title("", 200.0, measure), "● J\u{2009}I\u{2009}R\u{2009}A");
        assert_eq!(fit_title("Service Desk", 120.0, measure), "● SERVICE DESK");
        let cut = fit_title("A very long service desk name", 100.0, measure);
        assert!(cut.ends_with('…') && measure(&cut) <= 100.0, "{cut}");
        assert_eq!(row_label("Short", 260), "Short");
        let long = "A queue name that is far too long";
        assert_eq!(row_label(long, 260), format!("{}…", &long[..21]));
        assert_eq!(row_label(long, 120).chars().count(), 12);
    }
}
