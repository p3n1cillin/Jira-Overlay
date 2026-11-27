//! egui_kittest coverage of the real views: every control is wired and
//! works (Python `test_wiring`), Settings round-trips the config, and the
//! secondary windows do what their Python counterparts did.

use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Role};
use egui_kittest::Harness;
use egui_kittest::kittest::{By, NodeT, Queryable};
use jo_core::config::{self, Config};
use serde_json::{Value, json};

use crate::app::{Options, OverlayApp, Services, UiEvent};
use crate::model::{Desk, Issue, Queue, Snapshot};
use crate::poll::PollEvent;
use crate::state::Tone;
use crate::testing::{FakeJira, FakePlatform, ok_json};

type Reasons = Arc<Mutex<Vec<(String, bool)>>>;

struct Fixture {
    harness: Harness<'static, OverlayApp<'static>>,
    platform: Arc<FakePlatform>,
    reasons: Reasons,
    dir: tempfile::TempDir,
}

fn cfg(v: Value) -> Config {
    v.as_object().unwrap().clone()
}

fn fixture_in(dir: tempfile::TempDir, config: Config, jira: FakeJira, latest: Option<&'static str>) -> Fixture {
    let platform = Arc::new(FakePlatform::default());
    let reasons: Reasons = Arc::default();
    let log = reasons.clone();
    let svc = Services {
        platform: platform.clone(),
        make_transport: jira.make(),
        latest_release: Arc::new(move || Ok(latest.map(str::to_owned))),
    };
    let app = OverlayApp::new(
        &egui::Context::default(),
        dir.path(),
        config,
        svc,
        Box::new(move |reason, quit| log.lock().unwrap().push((reason.to_owned(), quit))),
        Options { hwnd: None, tray: false, poller: false, hotkey: false },
    );
    let harness =
        Harness::builder().with_size([420.0, 1900.0]).with_accessibility_check(false).with_max_steps(8).build_ui_state(
            |ui, app: &mut OverlayApp<'static>| {
                app.logic(ui.ctx());
                app.ui(ui);
            },
            app,
        );
    Fixture { harness, platform, reasons, dir }
}

fn fixture(config: Value) -> Fixture {
    fixture_in(tempfile::tempdir().unwrap(), cfg(config), FakeJira::new(|_| ok_json(json!({}))), None)
}

fn wiring_config() -> Value {
    json!({"domain": "e.atlassian.net", "email": "a@b.c", "token": "t", "serviceDeskName": "Desk",
           "projectKey": "SD", "serviceDeskId": 1, "alertQueues": ["Open"], "alertQueuesAsked": true,
           "overlayWidth": 260, "alpha": 1.0, "alwaysVisible": true, "snapCorner": "bottom-right"})
}

fn snapshot(queues: &[(&str, u64)], issues: &[(&str, &str)]) -> Snapshot {
    Snapshot {
        queues: queues
            .iter()
            .enumerate()
            .map(|(i, (n, c))| Queue { name: (*n).into(), id: (i + 1).to_string(), issue_count: *c })
            .collect(),
        all_queue_names: queues.iter().map(|(n, _)| (*n).into()).collect(),
        new_issues: issues.iter().map(|(k, q)| Issue::new(k, Some(&format!("Printer {k}")), q)).collect(),
        alert_count: issues.len() as u64,
        completed_today: 5,
        sla_compliance: Some(98),
        ..Default::default()
    }
}

impl Fixture {
    fn app(&mut self) -> &mut OverlayApp<'static> {
        self.harness.state_mut()
    }

    fn feed(&mut self, snap: Snapshot) {
        let tx = self.app().tx.clone();
        tx.send(UiEvent::Poll(PollEvent::Fetched(Box::new(snap)))).unwrap();
        self.steps(3);
    }

    fn steps(&mut self, n: usize) {
        self.harness.run_steps(n);
    }

    fn click(&mut self, label: &str) {
        self.harness.get_by_label(label).click();
        self.steps(3);
    }

    fn labels(&self, role: Role) -> Vec<String> {
        self.harness.query_all(By::new().role(role)).filter_map(|n| n.accesskit_node().label()).collect()
    }

    fn texts(&self) -> Vec<String> {
        self.harness
            .query_all(By::new())
            .flat_map(|n| {
                let node = n.accesskit_node();
                [node.label(), node.value()]
            })
            .flatten()
            .collect()
    }

    fn has_text(&self, needle: &str) -> bool {
        self.texts().iter().any(|t| t.contains(needle))
    }

    /// Click every control of these roles, re-querying after each click
    /// because a click can change the tree.
    fn sweep(&mut self, roles: &[Role], skip: &[&str]) -> usize {
        let mut used = 0;
        for &role in roles {
            let mut i = 0;
            loop {
                let nodes: Vec<String> = self.labels(role);
                let Some(label) = nodes.get(i).cloned() else { break };
                i += 1;
                if skip.iter().any(|s| label == *s) {
                    continue;
                }
                let node = self.harness.query_all(By::new().role(role)).nth(i - 1);
                if let Some(node) = node {
                    node.click();
                    used += 1;
                }
                self.steps(3);
                assert!(!self.app().closing, "{label:?} must not tear down the app");
            }
        }
        used
    }
}

#[test]
fn every_control_in_the_overlay_and_settings_works() {
    let mut f = fixture(wiring_config());
    f.steps(2);
    f.feed(snapshot(&[("Open", 3), ("In Progress", 6)], &[]));
    assert_eq!(f.app().st.status.1, Tone::Ok, "the Live status is green, not dim");

    f.click("Open service desk in Jira");
    assert!(
        f.platform.with(|r| r.urls.contains(&"https://e.atlassian.net/jira/servicedesk/projects/SD/queues".to_owned()))
    );

    let compact = f.sweep(&[Role::Button], &["Quit", "Settings"]);
    let urls = f.platform.with(|r| r.urls.clone());
    assert!(
        urls.contains(&"https://e.atlassian.net/jira/servicedesk/projects/SD/queues/custom/1".to_owned()),
        "{urls:?}"
    );
    assert!(urls.iter().any(|u| u.starts_with("https://e.atlassian.net/issues/?jql=project%20%3D%20SD")), "{urls:?}");

    f.click("Settings");
    assert!(f.app().st.in_settings);
    std::thread::sleep(Duration::from_millis(300));
    f.steps(2);
    let settings =
        f.sweep(&[Role::CheckBox, Role::RadioButton, Role::Button], &["Quit", "Settings", "✓  Save", "✕  Cancel"]);
    assert!(compact + settings >= 30, "a useful number of controls was covered ({compact} + {settings})");
    assert!(!f.app().closing, "the app is still running afterwards");
    assert!(f.reasons.lock().unwrap().is_empty());
}

fn round_trip() -> Value {
    json!({
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
    })
}

#[test]
fn saving_settings_unchanged_rewrites_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(config::FILE_NAME);
    fs::write(&path, round_trip().to_string()).unwrap();
    let loaded = config::load(&path).unwrap();
    let mut f = fixture_in(dir, loaded, FakeJira::new(|_| ok_json(json!({}))), None);
    f.steps(2);
    f.feed(snapshot(&[("Open", 1), ("Escalated", 1), ("Archive", 1)], &[]));
    f.click("Settings");
    f.click("✓  Save");
    assert!(!f.app().st.in_settings);
    let after: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let after = after.as_object().unwrap();
    let before = round_trip();
    let before = before.as_object().unwrap();
    let mut drift: Vec<&String> = before
        .keys()
        .chain(after.keys())
        .filter(|k| !["token", "tokenProtected"].contains(&k.as_str()))
        .filter(|k| before.get(*k) != after.get(*k))
        .collect();
    drift.dedup();
    assert!(drift.is_empty(), "saving Settings unchanged rewrites nothing: {drift:?}");
    assert_eq!(after["autoInstallUpdates"], false);
    assert_eq!(config::load(&path).unwrap()["token"], "SECRET", "the token still round-trips");
    assert_eq!(f.platform.with(|r| r.startup.clone()), [false], "the startup choice is applied on save");
}

#[test]
fn off_grid_slider_values_survive_an_untouched_save() {
    let mut config = wiring_config();
    config["alpha"] = json!(0.93);
    config["overlayWidth"] = json!(255);
    config["refreshSeconds"] = json!(15);
    let mut f = fixture(config);
    f.steps(2);
    f.click("Settings");
    f.steps(3);
    f.click("✓  Save");
    let saved = config::load(&f.dir.path().join(config::FILE_NAME)).unwrap();
    assert_eq!(saved["alpha"], 0.93);
    assert_eq!(saved["overlayWidth"], 255);
    assert_eq!(saved["refreshSeconds"], 15, "a deliberate value below the radio choices is kept");
}

#[test]
fn a_refused_startup_change_saves_nothing() {
    let mut f = fixture(wiring_config());
    *f.platform.startup_ok.lock().unwrap() = false;
    f.steps(2);
    f.click("Settings");
    f.click("✓  Save");
    assert!(f.app().st.in_settings, "settings stay open");
    assert!(f.platform.with(|r| r.messages.iter().any(|(t, _)| t == "Windows startup")));
    assert!(!f.dir.path().join(config::FILE_NAME).exists(), "other settings have not been saved");
}

#[test]
fn learner_health_and_recovery_render_in_settings() {
    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Local::now().naive_local().format("%Y-%m-%dT%H:%M:%S").to_string();
    fs::write(
        dir.path().join("triage_status.json"),
        json!({"healthy": false, "refusals": [401, 403], "updatedAt": now,
               "provenance": {"trainedOnDecisions": 500, "lastTuneDate": "2026-08-01"},
               "accuracy": {"pct": 91.2},
               "drift": {"date": "2026-08-01", "detail": "balanced accuracy dropped 20% (90% -> 70%)"}})
        .to_string(),
    )
    .unwrap();
    fs::write(dir.path().join("unins000.exe"), b"").unwrap();
    fs::write(dir.path().join("JiraOverlay.exe"), b"").unwrap();
    let mut f = fixture_in(dir, cfg(wiring_config()), FakeJira::new(|_| ok_json(json!({}))), None);
    f.steps(2);
    f.click("Settings");
    assert!(f.has_text("Learner polling needs attention"), "an unhealthy learner renders");
    assert!(f.has_text("500 decisions learned · last tuned 2026-08-01 · 91% accurate"));
    assert!(f.has_text("balanced accuracy dropped 20%"), "the drift detail is shown");
    assert!(f.has_text("User-session recovery"), "native installs show recovery status");

    // Diagnostics: absent logs are not created just by viewing.
    f.click("Open local diagnostic logs");
    assert!(f.platform.with(|r| r.messages.last().unwrap().1.contains("No lifecycle logs")));
    assert!(!f.dir.path().join(".app-lifecycle").exists());
    let folder = jo_core::lifecycle::folder(f.dir.path(), true).unwrap();
    f.click("Open local diagnostic logs");
    assert_eq!(f.platform.with(|r| r.opened.clone()), [folder]);
    *f.platform.open_fails.lock().unwrap() = true;
    let before = f.platform.with(|r| r.messages.len());
    f.click("Open local diagnostic logs");
    assert_eq!(f.platform.with(|r| r.messages.len()), before + 1, "Explorer errors are visible");
}

#[test]
fn manual_update_check_and_download() {
    let mut f = fixture_in(
        tempfile::tempdir().unwrap(),
        cfg(wiring_config()),
        FakeJira::new(|_| ok_json(json!({}))),
        Some("2099.01.01"),
    );
    *f.platform.answer.lock().unwrap() = true;
    f.steps(2);
    f.click("Settings");
    assert!(f.harness.get_by_label("Open update download").accesskit_node().is_disabled(), "nothing to open yet");
    f.click("Check for updates");
    let until = Instant::now() + Duration::from_secs(5);
    while f.app().updates.busy && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
        f.steps(1);
    }
    assert!(f.has_text("Version 2099.01.01 is available"));
    assert_eq!(f.platform.with(|r| r.messages.last().cloned()).unwrap().0, "Application updates");
    f.click("Open update download");
    assert_eq!(
        f.platform.with(|r| r.urls.clone()),
        [format!("https://github.com/{}/releases/tag/v2099.01.01", jo_core::REPOSITORY)]
    );
    assert!(f.app().updates.status.starts_with("Release page opened"));
}

#[test]
fn alert_popup_lists_tickets_and_dismisses() {
    let mut f = fixture(wiring_config());
    f.steps(2);
    f.feed(snapshot(&[("Open", 2)], &[("SD-1", "Open"), ("SD-2", "Open")]));
    f.harness.get_by_label("Open 2").hover();
    f.steps(4);
    assert!(f.app().st.tooltip_open);
    assert!(f.has_text("ALERT QUEUE — click to open"));
    f.click("SD-2 Printer SD-2");
    assert!(f.platform.with(|r| r.urls.contains(&"https://e.atlassian.net/browse/SD-2".to_owned())));
    f.click("✓  Dismiss — hide until new tickets");
    assert!(!f.app().st.tooltip_open && f.app().st.dismissed && !f.app().st.visible);
}

#[test]
fn snooze_menu_and_footer_quit() {
    let mut f = fixture(wiring_config());
    f.steps(2);
    f.feed(snapshot(&[("Open", 0)], &[]));
    f.click("Snooze alerts");
    assert!(f.app().snooze_popup);
    f.click("Snooze 30 min");
    assert!(f.app().st.status.0.starts_with("Snoozed until "));
    f.click("Snooze alerts");
    assert!(f.app().st.snooze_until.is_none(), "a second click cancels the snooze");
    f.click("Quit");
    assert!(f.app().closing);
    assert_eq!(*f.reasons.lock().unwrap(), [("user-quit".to_owned(), true)]);
}

#[test]
fn reconfigure_removes_the_config_and_restarts() {
    let mut f = fixture(wiring_config());
    let path = f.dir.path().join(config::FILE_NAME);
    fs::write(&path, "{}").unwrap();
    f.steps(2);
    f.click("Settings");
    f.click("Reconfigure credentials…");
    assert!(path.exists() && !f.app().closing, "declining changes nothing");
    *f.platform.answer.lock().unwrap() = true;
    f.click("Reconfigure credentials…");
    assert!(!path.exists());
    assert_eq!(f.app().exit, crate::Exit::Restart);
    assert!(f.app().closing);
}

#[test]
fn first_run_alert_queue_picker() {
    let mut f = fixture(json!({"domain": "e.atlassian.net"}));
    f.steps(2);
    f.feed(snapshot(&[("Open", 1), ("Closed", 2)], &[]));
    let tx = f.app().tx.clone();
    tx.send(UiEvent::Poll(PollEvent::OfferAlertPicker)).unwrap();
    f.steps(3);
    assert!(f.has_text("Which queues should trigger alerts?"));
    f.harness
        .query_all(By::new().role(Role::CheckBox))
        .find(|n| n.accesskit_node().label().as_deref() == Some("Open"))
        .unwrap()
        .click();
    f.steps(3);
    f.click("✓  Save");
    let saved = config::load(&f.dir.path().join(config::FILE_NAME)).unwrap();
    assert_eq!(saved["alertQueues"], json!(["Open"]));
    assert_eq!(saved["alertQueue"], "Open");
    assert_eq!(saved["alertQueuesAsked"], true);

    // Declining is remembered too.
    let mut f = fixture(json!({"domain": "e.atlassian.net"}));
    f.steps(2);
    f.feed(snapshot(&[("Open", 1)], &[]));
    let tx = f.app().tx.clone();
    tx.send(UiEvent::Poll(PollEvent::OfferAlertPicker)).unwrap();
    f.steps(3);
    f.click("Don't alert me — just show the counts");
    let saved = config::load(&f.dir.path().join(config::FILE_NAME)).unwrap();
    assert_eq!(saved["alertQueuesAsked"], true);
    assert!(!saved.contains_key("alertQueues"));
}

#[test]
fn desk_picker_answers_the_poller() {
    let mut f = fixture(json!({}));
    f.steps(2);
    let (reply, answer) = std::sync::mpsc::channel();
    let desks = vec![
        Desk { id: json!(5), project_name: "Help".into(), project_key: "HD".into() },
        Desk { id: json!(6), project_name: "Ops".into(), project_key: "OP".into() },
    ];
    let tx = f.app().tx.clone();
    tx.send(UiEvent::Poll(PollEvent::PickDesk { desks, reply })).unwrap();
    f.steps(3);
    f.click("Ops  (OP)");
    assert_eq!(answer.try_recv().unwrap().project_key, "OP");
    assert!(f.app().desk_picker.is_none());
}

fn report_jira() -> FakeJira {
    FakeJira::new(|req| {
        if req.url.ends_with("/user/assignable/search") {
            return ok_json(
                json!([{"displayName": "Ada", "accountId": "acc-ada"}, {"displayName": "Sam", "accountId": "acc-sam"}]),
            );
        }
        if req.url.ends_with("/search/jql") {
            let issue = |h: u32| {
                json!({"key": format!("SD-{h}"), "fields": {
                "created": "2026-11-02T09:00:00.000+0000",
                "resolutiondate": format!("2026-11-02T{:02}:00:00.000+0000", 9 + h)}})
            };
            return ok_json(json!({"issues": [issue(2), issue(4), issue(6)], "isLast": true}));
        }
        let jql = req.body.as_ref().and_then(|b| b["jql"].as_str()).unwrap_or_default().to_owned();
        let n = if jql.contains("everBreached") {
            1
        } else if jql.contains("acc-sam") || jql.contains("\"Sam\"") {
            4
        } else if jql.contains("IS EMPTY") {
            1
        } else if jql.contains("assignee =") {
            2
        } else {
            10
        };
        ok_json(json!({ "count": n }))
    })
}

fn wait_for(f: &mut Fixture, what: impl Fn(&mut Fixture) -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !what(f) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
        f.steps(1);
    }
    f.steps(3);
}

#[test]
fn team_report_counts_exports_and_picks_people() {
    let config = cfg(json!({"domain": "e.atlassian.net", "projectKey": "SD", "serviceDeskName": "Desk",
                            "reportUsers": ["Sam"], "reportRange": "Fortnight"}));
    let mut f = fixture_in(tempfile::tempdir().unwrap(), config, report_jira(), None);
    f.steps(2);
    let ctx = f.harness.ctx.clone();
    f.app().open_report(&ctx);
    assert_eq!(f.app().report.as_ref().unwrap().range, "30 days", "an unknown range falls back");
    wait_for(&mut f, |f| f.app().report.as_ref().is_some_and(|r| !r.rows.is_empty()));
    assert!(f.has_text("4 of 10 completed (40%) · 1 had no assignee"));
    assert!(f.has_text("Sam · 4 · SLA 75% · 4.0h"), "per-person SLA and median resolution time");

    f.click("Export CSV");
    let report = f.app().report.as_ref().unwrap();
    let (start, end) = report.period().unwrap();
    let csv = crate::report::export_path(f.dir.path(), start, end);
    let text = fs::read_to_string(&csv).unwrap();
    assert!(text.starts_with("Person,Completed,SLA %,Median hours to resolution,"), "{text}");
    assert!(text.contains("Sam,4,75,4.0,"), "{text}");
    assert_eq!(f.platform.with(|r| r.opened.clone()), [csv]);

    f.click("People…");
    wait_for(&mut f, |f| f.app().report.as_ref().and_then(|r| r.picker.as_ref()).is_some_and(|p| p.rows.len() == 2));
    let ticks: HashMap<String, bool> = f
        .app()
        .report
        .as_ref()
        .unwrap()
        .picker
        .as_ref()
        .unwrap()
        .rows
        .iter()
        .map(|(p, t)| (p.name.clone(), *t))
        .collect();
    assert_eq!(ticks, HashMap::from([("Ada".into(), false), ("Sam".into(), true)]), "Sam is matched, not listed twice");
    f.harness
        .query_all(By::new().role(Role::CheckBox))
        .find(|n| n.accesskit_node().label().as_deref() == Some("Ada"))
        .unwrap()
        .click();
    f.steps(3);
    f.click("Save");
    let saved = config::load(&f.dir.path().join(config::FILE_NAME)).unwrap();
    assert_eq!(
        saved["reportUsers"],
        json!([{"name": "Ada", "accountId": "acc-ada"}, {"name": "Sam", "accountId": "acc-sam"}])
    );

    f.app().close_report();
    let saved = config::load(&f.dir.path().join(config::FILE_NAME)).unwrap();
    assert_eq!(saved["reportRange"], "30 days");
    assert_eq!(saved["reportBasis"], "resolved");
}

#[test]
fn at_risk_shows_on_the_completed_row_and_opens_its_jql() {
    use crate::model::{AtRisk, SlaTrend};
    let mut f = fixture(wiring_config());
    f.steps(2);
    let mut snap = snapshot(&[("Open", 0)], &[]);
    snap.at_risk = Some(AtRisk { count: 2, tickets: vec![("SD-1".into(), None), ("SD-2".into(), None)] });
    snap.sla_trend = Some(SlaTrend { five: Some(94), twenty: Some(95) });
    f.feed(snap);
    assert!(f.has_text("⚠ 2 at risk"));
    assert_eq!(f.app().st.sla_trend.as_deref(), Some("5d 94% · 20d 95%"));
    f.click("⚠ 2 at risk");
    let urls = f.platform.with(|r| r.urls.clone());
    assert_eq!(urls.len(), 1, "the at-risk label takes the click, not the completed row: {urls:?}");
    assert!(urls[0].contains("running%28%29") && urls[0].contains("remaining%28%2230m%22%29"), "{urls:?}");
    f.click("✓  Completed today — 5");
    assert!(f.platform.with(|r| r.urls[1].contains("statusCategory%20%3D%20Done")), "the row still opens completed");
}

#[test]
fn hovering_a_queue_previews_its_newest_tickets() {
    let jira = FakeJira::new(|req| {
        if req.url.ends_with("/queue/2/issue") {
            let values: Vec<Value> = (1..=10)
                .map(|i| {
                    json!({"key": format!("SD-{i}"), "fields": {
                        "summary": format!("Laptop {i}"),
                        "created": format!("2026-11-27T{:02}:00:00.000+0000", i),
                        "assignee": {"displayName": "Sam Carter"}}})
                })
                .collect();
            return ok_json(json!({ "values": values }));
        }
        ok_json(json!({}))
    });
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("triage_suggestions.json"),
        json!({"SD-10": {"label": "With DEV · 92% — because: vpn", "review": false}}).to_string(),
    )
    .unwrap();
    let mut f = fixture_in(dir, cfg(wiring_config()), jira.clone(), None);
    f.steps(2);
    f.feed(snapshot(&[("Open", 0), ("In Progress", 10)], &[]));
    let fetches = |jira: &FakeJira| jira.requests().iter().filter(|r| r.url.ends_with("/queue/2/issue")).count();

    f.harness.get_by_label("In Progress 10").hover();
    f.steps(2);
    assert_eq!(fetches(&jira), 0, "a passing pointer fetches nothing");
    std::thread::sleep(crate::app::PREVIEW_DWELL + Duration::from_millis(50));
    f.steps(2);
    wait_for(&mut f, |f| f.app().preview.get("2").is_some());
    assert!(f.has_text("IN PROGRESS — newest"));
    assert!(f.has_text("SD-10 Laptop 10"), "newest first");
    assert!(!f.has_text("SD-2 Laptop 2"), "at most eight");
    assert!(f.has_text("→ With DEV · 92%"), "the learner's suggestion, inline");
    let request = jira.requests().into_iter().find(|r| r.url.ends_with("/queue/2/issue")).unwrap();
    assert_eq!(request.method, jo_jira::Method::Get, "read-only");
    f.click("SD-9 Laptop 9");
    assert!(f.platform.with(|r| r.urls.contains(&"https://e.atlassian.net/browse/SD-9".to_owned())));

    // Hovering again within a minute reuses what was fetched.
    f.app().close_preview();
    f.harness.get_by_label("In Progress 10").hover();
    // (Embedded in the test harness the old popup covered the row for one frame.)
    f.steps(2);
    std::thread::sleep(crate::app::PREVIEW_DWELL + Duration::from_millis(50));
    f.steps(3);
    assert!(f.app().preview_queue.is_some());
    assert_eq!(fetches(&jira), 1, "cached per queue for 60 s");
}

#[test]
fn new_settings_save_and_take_effect() {
    let mut f = fixture(wiring_config());
    f.steps(2);
    f.click("Settings");
    std::thread::sleep(Duration::from_millis(300));
    f.steps(2);
    f.click("Light");
    f.click("1 hour");
    f.app().settings.as_mut().unwrap().form.hotkey = "Off".into();
    f.click("✓  Save");
    let saved = config::load(&f.dir.path().join(config::FILE_NAME)).unwrap();
    assert_eq!(saved["theme"], "Light");
    assert_eq!(saved["slaAtRiskMinutes"], 60);
    assert_eq!(saved["hotkey"], "Off");
    assert!(!saved.contains_key("slaAtRiskToast"), "untouched settings stay unwritten");
    f.steps(2);
    assert!(crate::theme::c::is_light(), "the light palette is applied right away");
    assert_eq!(f.harness.ctx.global_style().visuals.panel_fill, crate::theme::c::LIGHT.bg);

    // Dark again.
    f.click("Settings");
    std::thread::sleep(Duration::from_millis(300));
    f.steps(2);
    f.click("Dark");
    f.click("✓  Save");
    f.steps(2);
    assert!(!crate::theme::c::is_light());
    assert_eq!(config::load(&f.dir.path().join(config::FILE_NAME)).unwrap()["theme"], "Dark");
}

#[test]
fn learner_accuracy_table_renders_in_settings() {
    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Local::now().naive_local().format("%Y-%m-%dT%H:%M:%S").to_string();
    fs::write(
        dir.path().join("triage_status.json"),
        json!({"healthy": true, "updatedAt": now,
               "recentPerformance": {
                   "7": {"accuracy": {"predictions": 40, "correct": 36, "pct": 90.0}, "byPredicted": {"IT": [30, 27]}},
                   "30": {"accuracy": {"predictions": 200, "correct": 184, "pct": 92.0}, "byPredicted": {"IT": [150, 139]}}},
               "performanceDrift": {"warnings": [{"label": "Overall accuracy", "dropPoints": 5.5}]},
               "routeSafety": {"With DEV": {"forceReview": true, "reason": "dormant"}}})
        .to_string(),
    )
    .unwrap();
    let mut f = fixture_in(dir, cfg(wiring_config()), FakeJira::new(|_| ok_json(json!({}))), None);
    f.steps(2);
    f.click("Settings");
    for text in [
        "Overall",
        "90% (40)",
        "92% (200)",
        "93% (150)",
        "Overall accuracy dropped 5.5 points",
        "With DEV: always suggested for review — dormant",
    ] {
        assert!(f.has_text(text), "{text}");
    }
}

#[test]
fn toast_clicks_and_the_hotkey() {
    let mut f = fixture(wiring_config());
    f.steps(2);
    let tx = f.app().tx.clone();
    for url in [
        "https://e.atlassian.net/browse/SD-1",
        "https://evil.example/browse/SD-1",
        "https://e.atlassian.net.evil.example/x",
    ] {
        tx.send(UiEvent::OpenUrl(url.into())).unwrap();
    }
    f.steps(2);
    assert_eq!(
        f.platform.with(|r| r.urls.clone()),
        ["https://e.atlassian.net/browse/SD-1"],
        "only the Jira site opens"
    );

    let visible = f.app().st.visible;
    tx.send(UiEvent::Hotkey).unwrap();
    f.steps(2);
    assert_ne!(f.app().st.visible, visible, "the hotkey toggles the overlay");
    tx.send(UiEvent::Hotkey).unwrap();
    f.steps(2);
    assert_eq!(f.app().st.visible, visible);
}

#[test]
fn a_display_scale_change_refits_the_overlay() {
    let mut f = fixture(wiring_config());
    f.steps(2);
    f.feed(snapshot(&[("Open", 1), ("In Progress", 3)], &[]));
    f.steps(2);
    let before = f.app().rect;
    assert_eq!(before.w, 260);
    f.harness.set_pixels_per_point(1.5);
    f.steps(4);
    let after = f.app().rect;
    assert_eq!(after.w, 390, "width follows the new scale");
    assert!((after.h as f32 - before.h as f32 * 1.5).abs() <= 3.0, "{before:?} -> {after:?}");
}

#[test]
fn hard_errors_replace_the_rows() {
    let mut f = fixture(wiring_config());
    f.steps(2);
    f.feed(Snapshot { error_msg: Some("HTTP 401 — check credentials".into()), ..Default::default() });
    assert!(f.has_text("HTTP 401 — check credentials"));
    assert!(f.has_text("Tap ↻ to retry"));
}

#[test]
fn only_the_latest_of_rapid_report_refreshes_renders() {
    use crate::report::Person;
    use crate::windows::ReportWindow;
    let jira = FakeJira::new(|req| {
        let jql = req.body.as_ref().and_then(|b| b["jql"].as_str()).unwrap_or_default().to_owned();
        let jitter = jql.bytes().fold(0_u64, |a, b| a.wrapping_mul(31).wrapping_add(u64::from(b))) % 40;
        std::thread::sleep(Duration::from_millis(jitter));
        ok_json(json!({"count": 1}))
    });
    for _trial in 0..3 {
        let (tx, rx) = std::sync::mpsc::channel();
        let svc = Services {
            platform: Arc::new(FakePlatform::default()),
            make_transport: jira.make(),
            latest_release: Arc::new(|| Ok(None)),
        };
        let config = cfg(json!({"projectKey": "SD"}));
        let mut window = ReportWindow::new(&config);
        let waker: crate::app::Waker = Arc::new(|| {});
        for click in 0..5 {
            window.people = vec![Person::new(&format!("P{click}"), "")];
            window.refresh(&config, &svc, &tx, &waker);
        }
        let until = Instant::now() + Duration::from_secs(5);
        let mut rendered = Vec::new();
        while Instant::now() < until {
            if let Ok(UiEvent::ReportDone(outcome)) = rx.recv_timeout(Duration::from_millis(300)) {
                window.done(outcome);
                rendered.push(window.rows.first().map(|r| r.name.clone()).unwrap_or_default());
            } else if !rendered.is_empty() {
                break;
            }
        }
        assert_eq!(rendered, ["P4"], "only the last click's result ever renders");
    }
}

#[test]
fn setup_checks_then_saves_and_closes() {
    use crate::setup::SetupApp;
    use std::cell::RefCell;
    use std::rc::Rc;
    let dir = tempfile::tempdir().unwrap();
    let platform = Arc::new(FakePlatform::default());
    let result = Rc::new(RefCell::new(None));
    let jira = FakeJira::new(|req| {
        if req.authorization.is_empty() {
            return crate::testing::status(401);
        }
        ok_json(json!({"displayName": "Sam Carter"}))
    });
    let app = SetupApp::new(&egui::Context::default(), dir.path(), platform.clone(), jira.make(), result.clone());
    let mut harness = Harness::builder()
        .with_size([430.0, 320.0])
        .with_accessibility_check(false)
        .build_ui_state(|ui, app: &mut SetupApp| app.ui(ui), app);
    harness.run_steps(2);
    harness.get_by_label("Save & Launch").click();
    harness.run_steps(2);
    assert_eq!(platform.with(|r| r.messages.clone()), [("Missing".into(), "Jira Domain is required.".into())]);
    harness.state_mut().values = ["https://Acme.atlassian.net/browse/SD-1".into(), "a@b.c".into(), "tok".into()];
    harness.get_by_label("Save & Launch").click();
    let until = Instant::now() + Duration::from_secs(5);
    while !harness.state().done && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
        harness.run_steps(1);
    }
    assert!(harness.state().done);
    assert_eq!(
        platform.with(|r| r.messages.last().cloned()).unwrap(),
        ("Signed in".into(), "Connected to Jira as Sam Carter.".into())
    );
    let saved = result.borrow().clone().unwrap();
    assert_eq!(saved["domain"], "acme.atlassian.net");
    assert_eq!(saved["token"], "tok");
    let on_disk: Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join(config::FILE_NAME)).unwrap()).unwrap();
    assert!(on_disk.get("token").is_none() && on_disk.get("tokenProtected").is_some(), "never written in plain text");
    assert_eq!(jira.requests()[0].url, "https://acme.atlassian.net/rest/api/3/myself");
}
