//! The Settings panel's values and its save rules.

use jo_core::config::{self, Config};
use serde_json::{Value, json};

use crate::{hotkey, stats, theme, util};

/// Settings this build added after `jo_core::config::OVERLAY_KEYS` was
/// fixed. The shared save treats unknown keys as another tool's and keeps
/// the copy on disk, so `Shared::save` writes these explicitly.
pub const EXTRA_KEYS: [&str; 4] = ["slaAtRiskMinutes", "slaAtRiskToast", "hotkey", "theme"];

/// At-risk warning choices (minutes left; 0 = off).
pub const AT_RISK_CHOICES: [(i64, &str); 4] = [(0, "Off"), (15, "15 min"), (30, "30 min"), (60, "1 hour")];

/// Every control in the Settings panel.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsForm {
    pub refresh: i64,
    pub notifications: bool,
    pub sound: bool,
    pub alpha: f64,
    pub width: i64,
    pub always_visible: bool,
    pub skip_keywords: String,
    pub max_queue: String,
    /// Queue names listed (before the hidden filter), with "show" ticks.
    pub queue_names: Vec<String>,
    pub show: Vec<bool>,
    pub tray_badge: String,
    pub alerts: Vec<bool>,
    pub sla_enabled: bool,
    pub weekends_off: bool,
    pub sla_field: String,
    pub completed_filter: String,
    pub check_updates: bool,
    pub startup: bool,
    pub at_risk_minutes: i64,
    pub at_risk_toast: bool,
    pub hotkey: String,
    pub theme: String,
    was_at_risk_minutes: i64,
    was_at_risk_toast: bool,
    was_hotkey: String,
    was_theme: String,
    was_sla_enabled: bool,
    was_sla_field: String,
    previous_hidden: Vec<String>,
    previous_alerts: Vec<String>,
}

/// Refresh-interval choices offered as radio buttons.
pub const REFRESH_CHOICES: [(i64, &str); 4] = [(30, "30 s"), (60, "1 min"), (120, "2 min"), (300, "5 min")];

impl SettingsForm {
    pub fn from_config(cfg: &Config, queue_names: Vec<String>, startup: bool) -> Self {
        let hidden = util::cfg_strings(cfg, "hiddenQueues");
        // Python: cfg.get("alertQueues", [alertQueue] if alertQueue else [])
        let alerts = if cfg.contains_key("alertQueues") {
            util::cfg_strings(cfg, "alertQueues")
        } else {
            util::alert_queues(cfg)
        };
        let show = queue_names.iter().map(|n| !hidden.contains(n)).collect();
        let alert_ticks = queue_names.iter().map(|n| alerts.contains(n)).collect();
        Self {
            refresh: util::cfg_int(cfg, "refreshSeconds", 60),
            notifications: config::truthy(cfg, "notificationsEnabled", true),
            sound: config::truthy(cfg, "soundEnabled", true),
            alpha: util::cfg_float(cfg, "alpha", 0.93),
            width: util::cfg_int(cfg, "overlayWidth", 260),
            always_visible: config::truthy(cfg, "alwaysVisible", false),
            skip_keywords: util::cfg_strings(cfg, "skipKeywords").join(", "),
            max_queue: util::cfg_int(cfg, "maxQueueSize", 0).to_string(),
            show,
            tray_badge: config::get_str(cfg, "trayBadgeQueue", "").to_owned(),
            alerts: alert_ticks,
            sla_enabled: config::truthy(cfg, "slaEnabled", true),
            weekends_off: config::is_true(cfg, "weekendsOff"),
            sla_field: config::get_str(cfg, "slaField", "Time to resolution").to_owned(),
            completed_filter: config::get_str(cfg, "completedTodayFilter", "").to_owned(),
            check_updates: config::truthy(cfg, "checkForUpdates", true),
            startup,
            at_risk_minutes: stats::at_risk_minutes(cfg),
            at_risk_toast: config::truthy(cfg, "slaAtRiskToast", true),
            hotkey: hotkey::configured(cfg),
            theme: theme::theme_choice(cfg).to_owned(),
            was_at_risk_minutes: stats::at_risk_minutes(cfg),
            was_at_risk_toast: config::truthy(cfg, "slaAtRiskToast", true),
            was_hotkey: hotkey::configured(cfg),
            was_theme: theme::theme_choice(cfg).to_owned(),
            was_sla_enabled: config::truthy(cfg, "slaEnabled", true),
            was_sla_field: config::get_str(cfg, "slaField", "").trim().to_owned(),
            previous_hidden: hidden,
            previous_alerts: alerts,
            queue_names,
        }
    }

    /// The max-queue spinbox: junk typed into it means "no limit".
    fn max_queue_value(&self) -> i64 {
        self.max_queue.trim().parse::<i64>().map(|n| n.max(0)).unwrap_or(0)
    }

    /// Step the max-queue spinbox by its 1,000 increment within 0..=999,999.
    pub fn step_max_queue(&mut self, up: bool) {
        let n = self.max_queue_value();
        let next = if up { (n + 1000).min(999_999) } else { (n - 1000).max(0) };
        self.max_queue = next.to_string();
    }

    /// Ticked names, keeping configured names this panel doesn't list
    /// (filtered out by keyword/size, or missing in Jira) in their place.
    /// Deliberate change: the Python build dropped those on every save, which
    /// silently switched off an alert the moment its queue got busy.
    fn merged(&self, previous: &[String], ticked: impl Fn(usize) -> bool) -> Vec<String> {
        let listed = |name: &String| self.queue_names.iter().position(|n| n == name);
        let mut out: Vec<String> =
            previous.iter().filter(|n| listed(n).is_none_or(&ticked)).cloned().collect::<Vec<_>>();
        for (i, name) in self.queue_names.iter().enumerate() {
            if ticked(i) && !out.contains(name) {
                out.push(name.clone());
            }
        }
        out
    }

    pub fn hidden_queues(&self) -> Vec<String> {
        self.merged(&self.previous_hidden, |i| !self.show[i])
    }

    pub fn alert_queues(&self) -> Vec<String> {
        self.merged(&self.previous_alerts, |i| self.alerts[i])
    }

    /// Write the form into `cfg` with the Python save rules. Returns true
    /// when an SLA setting changed (drop the "no such field" latch).
    pub fn apply(&self, cfg: &mut Config) -> bool {
        let alert_list = self.alert_queues();
        let values = [
            ("refreshSeconds", json!(self.refresh)),
            ("notificationsEnabled", json!(self.notifications)),
            ("soundEnabled", json!(self.sound)),
            ("hiddenQueues", json!(self.hidden_queues())),
            ("trayBadgeQueue", json!(self.tray_badge)),
            ("alpha", json!((self.alpha * 100.0).round() / 100.0)),
            ("overlayWidth", json!(self.width)),
            ("alwaysVisible", json!(self.always_visible)),
            ("alertQueue", json!(alert_list.first().cloned().unwrap_or_default())),
            ("alertQueues", json!(alert_list)),
            ("slaEnabled", json!(self.sla_enabled)),
            // No fallback to the default: that made it impossible to turn off.
            ("slaField", json!(self.sla_field.trim())),
            ("completedTodayFilter", json!(self.completed_filter.trim())),
            (
                "skipKeywords",
                json!(self.skip_keywords.split(',').map(str::trim).filter(|k| !k.is_empty()).collect::<Vec<_>>()),
            ),
            ("maxQueueSize", json!(self.max_queue_value())),
        ];
        for (key, value) in values {
            cfg.insert(key.into(), value);
        }
        // Opt-ins/outs are only written once someone changes them.
        if cfg.contains_key("weekendsOff") || self.weekends_off {
            cfg.insert("weekendsOff".into(), Value::Bool(self.weekends_off));
        }
        if cfg.contains_key("checkForUpdates") || !self.check_updates {
            cfg.insert("checkForUpdates".into(), Value::Bool(self.check_updates));
        }
        if cfg.contains_key("autoInstallUpdates") {
            cfg.insert("autoInstallUpdates".into(), Value::Bool(false));
        }
        // Newer settings are written only once changed, so an untouched
        // save leaves a hand-edited or absent value exactly as it was.
        if self.at_risk_minutes != self.was_at_risk_minutes {
            cfg.insert("slaAtRiskMinutes".into(), json!(self.at_risk_minutes.max(0)));
        }
        if self.at_risk_toast != self.was_at_risk_toast {
            cfg.insert("slaAtRiskToast".into(), Value::Bool(self.at_risk_toast));
        }
        if self.hotkey != self.was_hotkey {
            cfg.insert("hotkey".into(), json!(self.hotkey));
        }
        if self.theme != self.was_theme {
            cfg.insert("theme".into(), json!(self.theme));
        }
        self.sla_enabled != self.was_sla_enabled
            || self.sla_field.trim() != self.was_sla_field
            || self.at_risk_minutes != self.was_at_risk_minutes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(v: Value) -> Config {
        v.as_object().unwrap().clone()
    }

    fn round_trip() -> Config {
        cfg(json!({
            "domain": "acme.atlassian.net", "email": "a@b.c", "token": "SECRET",
            "serviceDeskId": 11, "serviceDeskName": "Desk", "projectKey": "SD",
            "alertQueues": ["Open", "Escalated"], "alertQueue": "Open",
            "trayBadgeQueue": "Open", "hiddenQueues": ["Archive"],
            "skipKeywords": ["test", "spam"], "maxQueueSize": 500,
            "refreshSeconds": 15, "notificationsEnabled": true, "soundEnabled": false,
            "autoInstallUpdates": false,
            "alpha": 0.95, "overlayWidth": 300, "alwaysVisible": false,
            "slaField": "Time to resolution", "slaEnabled": true,
            "completedTodayFilter": "assignee in (\"Sam Carter\")",
            "lastX": 100, "lastY": 200,
            "reportUsers": [{"name": "Sam Carter", "accountId": "a1"}],
            "reportRange": "7 days", "reportBasis": "updated",
            "triageStatuses": ["With DEV"], "movedLabels": {"BUGS": "Product bug"}
        }))
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn saving_unchanged_rewrites_nothing() {
        let original = round_trip();
        let form = SettingsForm::from_config(&original, names(&["Open", "Escalated", "Archive"]), false);
        let mut saved = original.clone();
        assert!(!form.apply(&mut saved), "no SLA change");
        assert_eq!(saved, original, "saving Settings unchanged rewrites nothing");
        assert_eq!(saved["autoInstallUpdates"], false);
    }

    #[test]
    fn conditional_writes() {
        let mut c = cfg(json!({}));
        let form = SettingsForm::from_config(&c, vec![], false);
        form.apply(&mut c);
        assert!(
            !c.contains_key("weekendsOff")
                && !c.contains_key("checkForUpdates")
                && !c.contains_key("autoInstallUpdates")
        );
        assert_eq!(c["trayBadgeQueue"], "", "an absent badge queue stays empty; the tray falls back at use");

        let mut form = SettingsForm::from_config(&c, vec![], false);
        form.weekends_off = true;
        form.check_updates = false;
        form.apply(&mut c);
        assert_eq!(c["weekendsOff"], true);
        assert_eq!(c["checkForUpdates"], false);

        let mut form = SettingsForm::from_config(&c, vec![], false);
        form.weekends_off = false;
        form.check_updates = true;
        form.apply(&mut c);
        assert_eq!(c["weekendsOff"], false, "written once present");
        assert_eq!(c["checkForUpdates"], true);

        let mut legacy = cfg(json!({"autoInstallUpdates": true}));
        SettingsForm::from_config(&legacy, vec![], false).apply(&mut legacy);
        assert_eq!(legacy["autoInstallUpdates"], false, "an old opt-in is always forced off");
    }

    #[test]
    fn filtered_or_missing_queues_survive_a_save() {
        // Deliberate change 1.
        let mut c = cfg(json!({"alertQueues": ["Busy", "Open", "Gone"], "alertQueue": "Busy",
                               "hiddenQueues": ["Spam", "Archive"]}));
        // "Busy" is over maxQueueSize, "Gone" was renamed in Jira, "Spam" is
        // keyword-filtered: none of them is listed in the panel.
        let mut form = SettingsForm::from_config(&c, names(&["Open", "Archive", "New"]), false);
        assert_eq!(form.alerts, [true, false, false]);
        assert_eq!(form.show, [true, false, true]);
        form.alerts[2] = true; // tick "New"
        form.show[1] = true; // show "Archive" again
        form.apply(&mut c);
        assert_eq!(c["alertQueues"], json!(["Busy", "Open", "Gone", "New"]));
        assert_eq!(c["alertQueue"], "Busy");
        assert_eq!(c["hiddenQueues"], json!(["Spam"]));

        let mut form = SettingsForm::from_config(&c, names(&["Open", "Archive", "New"]), false);
        form.alerts[0] = false; // untick "Open"
        form.apply(&mut c);
        assert_eq!(c["alertQueues"], json!(["Busy", "Gone", "New"]), "a listed queue can still be removed");
    }

    #[test]
    fn newer_settings_are_written_only_when_changed() {
        let original = round_trip();
        let mut saved = original.clone();
        let form = SettingsForm::from_config(&original, vec![], false);
        assert_eq!(
            (form.at_risk_minutes, form.at_risk_toast, form.hotkey.as_str(), form.theme.as_str()),
            (30, true, "Ctrl+Alt+J", "System"),
            "the defaults"
        );
        form.apply(&mut saved);
        assert!(EXTRA_KEYS.iter().all(|k| !saved.contains_key(*k)), "an untouched save writes nothing new");

        let mut form = SettingsForm::from_config(&saved, vec![], false);
        form.at_risk_minutes = 60;
        form.at_risk_toast = false;
        form.hotkey = "Off".into();
        form.theme = "Light".into();
        assert!(form.apply(&mut saved), "the at-risk threshold is an SLA setting (drops the latch)");
        assert_eq!(saved["slaAtRiskMinutes"], 60);
        assert_eq!(saved["slaAtRiskToast"], false);
        assert_eq!(saved["hotkey"], "Off");
        assert_eq!(saved["theme"], "Light");

        // Hand-edited values the form can't show survive an untouched save.
        let mut odd = cfg(json!({"slaAtRiskMinutes": 7, "hotkey": "Ctrl+Alt+Nope", "theme": "neon",
                                 "slaField": "Time to resolution"}));
        let before = odd.clone();
        let form = SettingsForm::from_config(&odd, vec![], false);
        assert_eq!((form.at_risk_minutes, form.hotkey.as_str(), form.theme.as_str()), (7, "Off", "System"));
        assert!(!form.apply(&mut odd));
        for key in EXTRA_KEYS {
            assert_eq!(odd.get(key), before.get(key), "{key}");
        }
    }

    #[test]
    fn values_and_sla_latch() {
        let mut c = cfg(json!({"slaField": "Time to resolution"}));
        let mut form = SettingsForm::from_config(&c, vec![], true);
        assert!(form.startup);
        form.skip_keywords = " test , , spam ".into();
        form.max_queue = "junk".into();
        form.alpha = 0.4499999;
        form.sla_field = "  ".into();
        assert!(form.apply(&mut c), "clearing the SLA field resets the latch");
        assert_eq!(c["skipKeywords"], json!(["test", "spam"]));
        assert_eq!(c["maxQueueSize"], 0);
        assert_eq!(c["alpha"], 0.45);
        assert_eq!(c["slaField"], "", "cleared on purpose, not forced back to the default");

        let mut form = SettingsForm::from_config(&c, vec![], false);
        form.max_queue = "-5".into();
        form.apply(&mut c);
        assert_eq!(c["maxQueueSize"], 0);
        form.max_queue = "999000".into();
        form.step_max_queue(true);
        assert_eq!(form.max_queue, "999999");
        form.max_queue = "500".into();
        form.step_max_queue(false);
        assert_eq!(form.max_queue, "0");
    }
}
