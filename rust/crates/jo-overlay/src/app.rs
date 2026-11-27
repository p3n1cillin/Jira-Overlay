//! The overlay application: event handling, timers, window management and
//! actions. Rendering lives in `view`, secondary windows in `windows`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui::{self, ViewportCommand};
use jo_core::config::{self, Config};
use serde_json::json;

use crate::Exit;
use crate::learner::{self, LearnerLines};
use crate::model::Desk;
use crate::placement::{self, Rect};
use crate::platform::{self, Platform};
use crate::poll::{self, MakeTransport, PollCmd, PollEvent, Poller, Shared};
use crate::report::{Outcome, Person};
use crate::settings::SettingsForm;
use crate::state::{self, OverlayState, RELATIVE_TIME_EVERY};
use crate::tray::Tray;
use crate::updates_ui::{self, LatestRelease, UpdateUi};
use crate::util;
use crate::windows::{AlertPicker, DeskPicker, ReportWindow};

/// Records why the process is ending (`Session::reason`).
pub type RecordReason<'a> = Box<dyn Fn(&str, bool) + 'a>;

/// Wakes the UI thread from any thread.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Everything outside the process the overlay talks to.
#[derive(Clone)]
pub struct Services {
    pub platform: Arc<dyn Platform>,
    pub make_transport: MakeTransport,
    pub latest_release: LatestRelease,
}

/// Messages to the UI thread.
pub enum UiEvent {
    Poll(PollEvent),
    UpdateChecked {
        release: Option<String>,
        error: Option<String>,
        manual: bool,
    },
    ReportDone(Outcome),
    PeopleFound(Vec<Person>),
    Menu(tray_icon::menu::MenuId),
    TrayHover,
    TrayClick,
    /// A second launch asked the running overlay to show itself.
    Show,
    /// Setup/the updater asked this installation's apps to stop.
    StopForUpdate,
}

/// The settings panel while it is open.
pub struct SettingsPanel {
    pub form: SettingsForm,
    pub learner: Option<LearnerLines>,
    /// `(healthy, message)` for native installs.
    pub recovery: Option<(bool, String)>,
}

/// An in-flight window-rect animation (settings expand/collapse).
pub struct GeomAnim {
    pub from: Rect,
    pub to: Rect,
    pub began: Instant,
    pub closing: bool,
}

pub const SETTINGS_MS: Duration = Duration::from_millis(260);

/// A press that may become a window drag.
#[derive(Clone, Copy)]
pub struct Drag {
    pub cursor0: (i32, i32),
    pub win0: (i32, i32),
    pub moved: bool,
}

pub struct OverlayApp<'a> {
    pub st: OverlayState,
    pub svc: Services,
    pub root: PathBuf,
    pub tx: Sender<UiEvent>,
    rx: Receiver<UiEvent>,
    pub waker: Waker,
    poll: Option<Sender<PollCmd>>,
    record_reason: RecordReason<'a>,
    pub exit: Exit,
    pub closing: bool,
    pub updates: UpdateUi,
    report_previous_at: Option<Instant>,
    next_relative: Instant,
    pub hwnd: Option<isize>,
    applied_mapped: bool,
    applied_alpha: f32,
    /// The overlay's rectangle in physical pixels (kept even without a real window).
    pub rect: Rect,
    pub has_icon_font: bool,
    /// Fonts and visuals are installed on the first frame.
    styled: bool,
    pub settings: Option<SettingsPanel>,
    pub geom: Option<GeomAnim>,
    queue_geom: Option<Rect>,
    pub drag: Option<Drag>,
    pub suppress_click: bool,
    pub snooze_popup: bool,
    pub alert_picker: Option<AlertPicker>,
    pub desk_picker: Option<DeskPicker>,
    pub report: Option<ReportWindow>,
    pub tray: Option<Tray>,
    /// Height the compact content needed last frame (points).
    pub content_height: f32,
    pub ppp: f32,
    /// Whether an alert queue row was hovered last frame.
    pub alert_row_hovered: bool,
    /// Where the snooze button was drawn (the snooze menu opens above it).
    pub snooze_anchor: egui::Rect,
    /// Measured size of the alert popup (points) and its window.
    pub popup_size: Option<egui::Vec2>,
    pub popup_hwnd: Option<isize>,
}

/// Construction options (tests skip the tray and the poller thread).
pub struct Options {
    pub hwnd: Option<isize>,
    pub tray: bool,
    pub poller: bool,
}

impl<'a> OverlayApp<'a> {
    pub fn new(
        ctx: &egui::Context,
        root: &Path,
        cfg: Config,
        svc: Services,
        record_reason: RecordReason<'a>,
        options: Options,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let repaint = ctx.clone();
        let waker: Waker = Arc::new(move || repaint.request_repaint());
        let shared = Arc::new(Shared::new(root, cfg));
        let now = Instant::now();
        let mut app = Self {
            st: OverlayState::new(shared.clone()),
            svc,
            root: root.to_path_buf(),
            tx,
            rx,
            waker,
            poll: None,
            record_reason,
            exit: Exit::Quit,
            closing: false,
            updates: UpdateUi::new(now),
            report_previous_at: Some(now + updates_ui::REPORT_PREVIOUS_AFTER),
            next_relative: now + RELATIVE_TIME_EVERY,
            hwnd: options.hwnd,
            applied_mapped: false,
            applied_alpha: -1.0,
            rect: Rect { x: 100, y: 100, w: 260, h: 60 },
            has_icon_font: false,
            styled: false,
            settings: None,
            geom: None,
            queue_geom: None,
            drag: None,
            suppress_click: false,
            snooze_popup: false,
            alert_picker: None,
            desk_picker: None,
            report: None,
            tray: None,
            content_height: 0.0,
            ppp: ctx.pixels_per_point(),
            alert_row_hovered: false,
            snooze_anchor: egui::Rect::NOTHING,
            popup_size: None,
            popup_hwnd: None,
        };
        if let Some(h) = app.hwnd {
            platform::round_corners(h, false);
            if let Some((x, y, w, hgt)) = platform::window_rect(h) {
                app.rect = Rect { x, y, w, h: hgt };
            }
        }
        if options.tray {
            app.tray = Tray::new(&app.root, app.tx.clone(), app.waker.clone());
        }
        if options.poller {
            let (tx, waker) = (app.tx.clone(), app.waker.clone());
            let poller = Poller::new(
                shared,
                app.svc.make_transport.clone(),
                Box::new(move |e| {
                    let _ = tx.send(UiEvent::Poll(e));
                    waker();
                }),
            );
            app.poll = Some(poll::spawn(poller));
        }
        app
    }

    pub fn config(&self) -> Config {
        self.st.config()
    }

    /// Install fonts and visuals once, on the context actually rendering.
    pub fn ensure_style(&mut self, ctx: &egui::Context) {
        if !self.styled {
            self.has_icon_font = crate::theme::install_fonts(ctx);
            crate::theme::install_style(ctx);
            self.styled = true;
        }
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.st.shared
    }

    pub fn send_poll(&self, cmd: PollCmd) {
        if let Some(tx) = &self.poll {
            let _ = tx.send(cmd);
        }
    }

    // ── Per-frame logic (runs whether or not the overlay is shown) ─────────

    pub fn logic(&mut self, ctx: &egui::Context) {
        self.ppp = ctx.pixels_per_point();
        let now = Instant::now();
        while let Ok(event) = self.rx.try_recv() {
            self.handle(ctx, event, now);
        }
        if self.updates.next_check.is_some_and(|at| now >= at) {
            self.check_updates(false);
        }
        if self.report_previous_at.is_some_and(|at| now >= at) {
            self.report_previous_at = None;
            let platform = self.svc.platform.clone();
            self.updates.report_previous(&self.root, platform.as_ref());
        }
        if now >= self.next_relative {
            self.next_relative = now + RELATIVE_TIME_EVERY;
            self.st.relative_time(now);
        }
        let animating = self.st.tick(now) || self.geom.is_some() || self.drag.is_some();
        self.apply_window();
        if let Some(pending) = self.st.tray_pending.take()
            && let Some(tray) = &mut self.tray
        {
            tray.show_state(&pending);
        }
        if animating {
            ctx.request_repaint();
        } else {
            let deadlines =
                [self.st.show_at, self.updates.next_check, self.report_previous_at, Some(self.next_relative)];
            if let Some(next) = deadlines.into_iter().flatten().min() {
                ctx.request_repaint_after(next.saturating_duration_since(now));
            }
        }
    }

    fn handle(&mut self, ctx: &egui::Context, event: UiEvent, now: Instant) {
        match event {
            UiEvent::Poll(PollEvent::Fetched(snapshot)) => {
                self.st.snap = *snapshot;
                self.update_ui_logged(now);
            }
            UiEvent::Poll(PollEvent::OfferAlertPicker) => {
                if self.alert_picker.is_none() {
                    self.alert_picker = Some(AlertPicker::new(&self.st.snap.queues));
                }
            }
            UiEvent::Poll(PollEvent::PickDesk { desks, reply }) => {
                self.desk_picker = Some(DeskPicker::new(desks, reply))
            }
            UiEvent::UpdateChecked { release, error, manual } => {
                let (cfg, platform) = (self.config(), self.svc.platform.clone());
                self.updates.finished(release, error, manual, &cfg, now, platform.as_ref());
            }
            UiEvent::ReportDone(outcome) => {
                if let Some(report) = &mut self.report {
                    report.done(outcome);
                }
            }
            UiEvent::PeopleFound(people) => {
                if let Some(picker) = self.report.as_mut().and_then(|r| r.picker.as_mut()) {
                    picker.discovered(people);
                }
            }
            UiEvent::Menu(id) => self.menu(ctx, &id, now),
            UiEvent::TrayHover => {
                if let Some(tray) = &mut self.tray {
                    tray.refresh_learner_item(&self.root);
                }
            }
            UiEvent::TrayClick => self.tray_toggle(now),
            UiEvent::Show => self.st.user_show(now),
            UiEvent::StopForUpdate => {
                (self.record_reason)("update", false);
                self.close(ctx);
            }
        }
    }

    /// `_update_ui`, with a failure logged instead of stopping the app.
    pub fn update_ui_logged(&mut self, now: Instant) {
        let platform = self.svc.platform.clone();
        let st = &mut self.st;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| st.update_ui(now, platform.as_ref())));
        if let Err(panic) = outcome {
            let detail = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()));
            util::log_error(&self.root, &format!("repaint failed:\n{}", detail.unwrap_or_default()));
        }
    }

    fn menu(&mut self, ctx: &egui::Context, id: &tray_icon::menu::MenuId, now: Instant) {
        let Some(item) = self.tray.as_ref().and_then(|t| t.item(id)) else { return };
        match item {
            crate::tray::Item::ShowHide => self.tray_toggle(now),
            crate::tray::Item::TeamReport => self.open_report(ctx),
            crate::tray::Item::LearnerReport => self.open_learner_report(),
            crate::tray::Item::CheckUpdates => self.check_updates(true),
            crate::tray::Item::Quit => self.quit(ctx),
        }
    }

    fn tray_toggle(&mut self, now: Instant) {
        if self.st.visible { self.st.user_hide(now) } else { self.st.user_show(now) }
    }

    /// Push alpha/mapping to the real window (alpha first, so a show never
    /// flashes at full opacity).
    fn apply_window(&mut self) {
        let Some(h) = self.hwnd else { return };
        if (self.applied_alpha - self.st.alpha).abs() > 0.001 {
            platform::set_alpha(h, self.st.alpha);
            self.applied_alpha = self.st.alpha;
        }
        if self.applied_mapped != self.st.mapped {
            platform::set_mapped(h, self.st.mapped);
            self.applied_mapped = self.st.mapped;
        }
    }

    pub fn set_rect(&mut self, rect: Rect) {
        self.rect = rect;
        if let Some(h) = self.hwnd {
            platform::set_window_rect(h, rect.x, rect.y, rect.w, rect.h);
        }
    }

    fn work_area_at(&self, x: i32, y: i32) -> platform::WorkArea {
        self.svc.platform.work_area(x, y).unwrap_or_else(|| placement::fallback_area(self.svc.platform.screen_size()))
    }

    pub fn corners(&self) -> [(&'static str, (i32, i32)); 4] {
        let r = self.rect;
        placement::corners(self.work_area_at(r.x + r.w / 2, r.y + r.h / 2), r.w, r.h, self.ppp)
    }

    /// Python `_resize` + `_reposition`: width from the config, height from
    /// the content, then placed by the saved corner/position.
    pub fn fit_and_place(&mut self) {
        if self.st.in_settings || self.geom.is_some() || self.drag.is_some_and(|d| d.moved) {
            return;
        }
        let cfg = self.config();
        let w = (util::cfg_int(&cfg, "overlayWidth", 260) as f32 * self.ppp).round() as i32;
        let h = ((self.content_height + 2.0) * self.ppp).round() as i32;
        let (current, screen, ppp) = ((self.rect.x, self.rect.y), self.svc.platform.screen_size(), self.ppp);
        let platform = self.svc.platform.clone();
        let (x, y) = self
            .shared()
            .update(|c| placement::reposition(c, current, w, h, ppp, screen, &|x, y| platform.work_area(x, y)));
        let target = Rect { x, y, w, h };
        if target != self.rect {
            self.set_rect(target);
        }
    }

    // ── Drag and snap ──────────────────────────────────────────────────────

    pub fn begin_drag(&mut self) {
        if self.hwnd.is_none() {
            return; // headless (tests): there is no real window to move
        }
        if let Some(cursor0) = platform::cursor_pos() {
            self.drag = Some(Drag { cursor0, win0: (self.rect.x, self.rect.y), moved: false });
        }
    }

    pub fn drag_motion(&mut self) {
        let Some(mut drag) = self.drag else { return };
        let Some((cx, cy)) = platform::cursor_pos() else { return };
        let (dx, dy) = (cx - drag.cursor0.0, cy - drag.cursor0.1);
        let threshold = 4.0 * self.ppp;
        if dx.abs() as f32 > threshold || dy.abs() as f32 > threshold {
            drag.moved = true;
        }
        if drag.moved {
            let r = self.rect;
            self.set_rect(Rect { x: drag.win0.0 + dx, y: drag.win0.1 + dy, ..r });
        }
        self.drag = Some(drag);
    }

    /// Release: snap to a corner within 60 px, else remember the position.
    pub fn end_drag(&mut self) {
        let Some(drag) = self.drag.take() else { return };
        self.suppress_click = drag.moved;
        if !drag.moved || self.st.in_settings {
            return;
        }
        let corners = self.corners();
        let snapped = placement::snap_target(&corners, self.rect.x, self.rect.y, self.ppp);
        let (x, y) = (self.rect.x, self.rect.y);
        self.shared().update(|c| placement::remember_drop(c, snapped, x, y));
        if let Some(name) = snapped
            && let Some((_, (cx, cy))) = corners.iter().find(|(n, _)| *n == name)
        {
            let r = self.rect;
            self.set_rect(Rect { x: *cx, y: *cy, ..r });
        }
        self.shared().save();
    }

    /// Double-click on the header: snap to the nearest corner.
    pub fn snap_to_nearest(&mut self) {
        let (name, (x, y)) = placement::nearest(&self.corners(), self.rect.x, self.rect.y);
        let r = self.rect;
        self.set_rect(Rect { x, y, ..r });
        self.shared().update(|c| {
            c.shift_remove("lastX");
            c.shift_remove("lastY");
            c.insert("snapCorner".into(), json!(name));
        });
        self.shared().save();
    }

    // ── Actions ────────────────────────────────────────────────────────────

    fn domain_url(&self, path: &str) -> String {
        format!("https://{}{path}", config::get_str(&self.config(), "domain", ""))
    }

    pub fn row_url(&self, queue_id: &str) -> String {
        let cfg = self.config();
        match config::get_str(&cfg, "projectKey", "") {
            "" => self.domain_url("/jira/servicedesk"),
            pk => self.domain_url(&format!(
                "/jira/servicedesk/projects/{}/queues/custom/{}",
                jo_core::jql::path_segment(pk),
                jo_core::jql::path_segment(queue_id)
            )),
        }
    }

    pub fn open_service_desk(&self) {
        let cfg = self.config();
        let url = match config::get_str(&cfg, "projectKey", "") {
            "" => self.domain_url("/jira/servicedesk"),
            pk => self.domain_url(&format!("/jira/servicedesk/projects/{}/queues", jo_core::jql::path_segment(pk))),
        };
        self.svc.platform.open_url(&url);
    }

    pub fn open_completed_today(&self) {
        let jql = crate::stats::completed_today_jql(&self.config(), crate::stats::today());
        self.svc.platform.open_url(&self.domain_url(&format!("/issues/?jql={}", quote_all(&jql))));
    }

    pub fn ticket_url(&self, key: &str) -> String {
        self.domain_url(&format!("/browse/{}", jo_core::jql::path_segment(key)))
    }

    pub fn refresh(&self) {
        self.send_poll(PollCmd::Refresh);
    }

    pub fn snooze_cycle(&mut self) {
        if self.st.is_snoozed() {
            let platform = self.svc.platform.clone();
            self.st.cancel_snooze(Instant::now(), platform.as_ref());
        } else {
            self.snooze_popup = true;
        }
    }

    pub fn snooze(&mut self, minutes: i64) {
        self.snooze_popup = false;
        let platform = self.svc.platform.clone();
        self.st.snooze(minutes, Instant::now(), platform.as_ref());
    }

    pub fn close(&mut self, ctx: &egui::Context) {
        self.closing = true;
        self.send_poll(PollCmd::Quit);
        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, ViewportCommand::Close);
    }

    /// Footer ✕ and tray Quit: a deliberate quit the guardian won't undo.
    pub fn quit(&mut self, ctx: &egui::Context) {
        (self.record_reason)("user-quit", true);
        self.close(ctx);
    }

    pub fn check_updates(&mut self, manual: bool) {
        let cfg = self.config();
        if !self.updates.begin(manual, &cfg, Instant::now()) {
            return;
        }
        let (lookup, tx, waker) = (self.svc.latest_release.clone(), self.tx.clone(), self.waker.clone());
        let _ = std::thread::Builder::new().name("update-check".into()).spawn(move || {
            let (release, error) = match lookup() {
                Ok(release) => (release, None),
                Err(e) => (None, Some(e)),
            };
            let _ = tx.send(UiEvent::UpdateChecked { release, error, manual });
            waker();
        });
    }

    pub fn open_update_download(&mut self) {
        let platform = self.svc.platform.clone();
        self.updates.open_download(&self.root, false, platform.as_ref());
    }

    pub fn learner_report_path(&self) -> PathBuf {
        self.root.join("TRIAGE_REPORT.md")
    }

    pub fn open_learner_report(&self) {
        let path = self.learner_report_path();
        if path.is_file() {
            self.svc.platform.open_document(&path);
        }
    }

    pub fn open_report(&mut self, ctx: &egui::Context) {
        if let Some(report) = &mut self.report {
            report.focus = true;
            return;
        }
        let mut report = ReportWindow::new(&self.config());
        report.refresh(&self.config(), &self.svc, &self.tx, &self.waker);
        self.report = Some(report);
        ctx.request_repaint();
    }

    pub fn open_diagnostics(&self) {
        let p = &self.svc.platform;
        match jo_core::lifecycle::folder(&self.root, false) {
            Ok(path) if !path.is_dir() => p.info("Diagnostic logs", "No lifecycle logs have been recorded yet."),
            Ok(path) => {
                if p.open_folder(&path).is_err() {
                    p.error("Diagnostic logs", "Windows could not open this installation's local diagnostic folder.");
                }
            }
            Err(_) => p.error("Diagnostic logs", "Windows could not open this installation's local diagnostic folder."),
        }
    }

    /// "Reconfigure credentials…": confirm, remove the config and restart.
    pub fn reconfigure(&mut self, ctx: &egui::Context) {
        if !self.svc.platform.ask_yes_no("Reconfigure", "Clear saved credentials and restart setup?") {
            return;
        }
        let _ = std::fs::remove_file(self.shared().config_path());
        self.exit = Exit::Restart;
        self.close(ctx);
    }

    // ── Inline settings ────────────────────────────────────────────────────

    pub fn toggle_settings(&mut self) {
        if self.st.in_settings {
            self.close_settings();
            return;
        }
        let now = Instant::now();
        self.st.in_settings = true;
        self.st.show_at = None;
        self.st.tooltip_open = false;
        self.snooze_popup = false;
        let was_hidden = !self.st.visible;
        if was_hidden {
            self.st.visible = true;
            self.st.alpha = 0.0;
            self.st.mapped = true;
        }
        // Never record an intermediate frame as "the compact overlay".
        if self.queue_geom.is_none() || self.geom.is_none() {
            self.queue_geom = Some(self.rect);
        }
        self.settings = Some(self.build_settings());
        let compact = self.queue_geom.unwrap_or(self.rect);
        let area = self.work_area_at(compact.x + compact.w / 2, compact.y + compact.h / 2);
        let target = placement::settings_target(compact, area, self.ppp);
        if was_hidden {
            self.geom = None;
            self.set_rect(target);
            let alpha = util::cfg_float(&self.config(), "alpha", 0.93) as f32;
            self.st.animate_alpha(alpha, false, now);
        } else {
            self.geom = Some(GeomAnim { from: self.rect, to: target, began: now, closing: false });
            self.st.geom_animating = true;
        }
    }

    fn build_settings(&self) -> SettingsPanel {
        let cfg = self.config();
        let names = if self.st.snap.all_queue_names.is_empty() {
            self.st.snap.queues.iter().map(|q| q.name.clone()).collect()
        } else {
            self.st.snap.all_queue_names.clone()
        };
        let recovery = jo_core::updates::native_install(&self.root).then(|| {
            jo_core::lifecycle::recovery_status(&self.root, |name| {
                jo_core::ipc::component_running(name).unwrap_or(false)
            })
        });
        SettingsPanel {
            form: SettingsForm::from_config(&cfg, names, self.svc.platform.startup_enabled()),
            learner: learner::load_status(&self.root.join("triage_status.json")).map(|s| learner::learner_lines(&s)),
            recovery,
        }
    }

    pub fn close_settings(&mut self) {
        // Intent, not animation state: a click mid-collapse is not another close.
        self.st.in_settings = false;
        let Some(compact) = self.queue_geom else { return };
        self.geom = Some(GeomAnim { from: self.rect, to: compact, began: Instant::now(), closing: true });
        self.st.geom_animating = true;
    }

    /// Advance the expand/collapse; the panel swap happens at the start.
    pub fn tick_geometry(&mut self) {
        let Some(anim) = &self.geom else { return };
        let t = anim.began.elapsed().as_secs_f32() / SETTINGS_MS.as_secs_f32();
        if t >= 1.0 {
            let (to, closing) = (anim.to, anim.closing);
            self.geom = None;
            self.st.geom_animating = false;
            self.set_rect(to);
            if closing {
                self.settings = None;
                self.fit_and_place(); // a queue update may have arrived meanwhile
            }
        } else {
            let rect = placement::lerp_rect(anim.from, anim.to, placement::ease_in_out(t));
            self.set_rect(rect);
        }
    }

    pub fn save_settings(&mut self) {
        let Some(panel) = &self.settings else { return };
        let p = self.svc.platform.clone();
        if !p.set_startup(panel.form.startup) {
            p.error(
                "Windows startup",
                "Windows did not allow the startup setting to be changed. Other settings have not been saved; check your Windows permissions and try again.",
            );
            return;
        }
        let form = panel.form.clone();
        let reset_sla = self.shared().update(|c| form.apply(c));
        self.shared().save();
        self.st.rows.clear();
        self.st.flashes.clear();
        self.send_poll(PollCmd::SettingsSaved { reset_sla });
        self.close_settings();
    }

    // ── Secondary windows' results ─────────────────────────────────────────

    /// The first-run picker closed (any way): asked and answered.
    pub fn alert_picker_closed(&mut self, chosen: Option<Vec<String>>) {
        self.alert_picker = None;
        self.shared().update(|c| {
            if let Some(chosen) = &chosen {
                c.insert("alertQueue".into(), json!(chosen.first().cloned().unwrap_or_default()));
                c.insert("alertQueues".into(), json!(chosen));
            }
            c.insert("alertQueuesAsked".into(), json!(true));
        });
        self.shared().save();
        if !config::get_str(&self.config(), "alertQueue", "").is_empty() {
            self.refresh();
        }
    }

    pub fn desk_picked(&mut self, desk: Option<Desk>) {
        if let Some(picker) = self.desk_picker.take() {
            picker.answer(desk);
        }
    }

    /// Fit the window after a compact frame (Python `root.after(10, _resize)`).
    pub fn after_compact_frame(&mut self, content_height: f32) {
        let changed = (content_height - self.content_height).abs() > 0.5;
        self.content_height = content_height;
        if (self.st.resize_requested || (changed && self.st.mapped)) && self.drag.is_none() {
            self.st.resize_requested = false;
            self.fit_and_place();
        }
    }

    pub fn now_snoozed(&mut self) -> bool {
        self.st.is_snoozed()
    }

    pub fn title_text(&self, ui: &egui::Ui) -> String {
        let cfg = self.config();
        let desk = config::get_str(&cfg, "serviceDeskName", "JIRA").to_owned();
        let avail = (util::cfg_int(&cfg, "overlayWidth", 260) - 2 - 28 - 30) as f32;
        let font = crate::theme::bold(8.0);
        state::fit_title(&desk, avail, |t| crate::widgets::text_size(ui, t, &font).x)
    }
}

/// `urllib.parse.quote(text, safe="")`.
pub fn quote_all(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"_.-~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jql_links_are_fully_encoded() {
        assert_eq!(quote_all("a = \"b\" & c"), "a%20%3D%20%22b%22%20%26%20c");
        assert_eq!(quote_all("SD-1_x.y~"), "SD-1_x.y~");
    }
}
