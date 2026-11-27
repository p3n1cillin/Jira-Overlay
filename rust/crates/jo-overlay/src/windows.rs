//! Secondary windows: the alert popup, the snooze menu, the first-run
//! pickers, the team report and its people picker.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::Instant;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, Id, Rect, Role, Sense, Ui, Vec2, ViewportBuilder, ViewportCommand, ViewportId,
    pos2, vec2,
};
use jo_core::config::{self, Config};
use serde_json::{Value, json};

use crate::alerts::{self, PopupLine};
use crate::app::{OverlayApp, Services, UiEvent, Waker};
use crate::model::{Desk, Queue};
use crate::platform;
use crate::preview;
use crate::report::{self, Outcome, Person, RankRow, ReportGate};
use crate::stats;
use crate::theme::{self, c};
use crate::widgets::{self, Clickable};

pub const POPUP_TITLE: &str = "Jira Overlay — Alerts";
pub const PREVIEW_TITLE: &str = "Jira Overlay — Preview";
const ALERT_HEADER: &str = "  ALERT QUEUE — click to open  ";

/// First-run "Which queues should trigger alerts?".
pub struct AlertPicker {
    pub names: Vec<String>,
    pub ticks: Vec<bool>,
}

impl AlertPicker {
    pub fn new(queues: &[Queue]) -> Self {
        Self { names: queues.iter().map(|q| q.name.clone()).collect(), ticks: vec![false; queues.len()] }
    }

    pub fn chosen(&self) -> Vec<String> {
        self.names.iter().zip(&self.ticks).filter(|(_, t)| **t).map(|(n, _)| n.clone()).collect()
    }
}

/// "Which service desk to monitor?"; the poller waits for the answer.
pub struct DeskPicker {
    pub desks: Vec<Desk>,
    reply: Sender<Desk>,
}

impl DeskPicker {
    pub fn new(desks: Vec<Desk>, reply: Sender<Desk>) -> Self {
        Self { desks, reply }
    }

    /// Closing without choosing takes the first desk, never stranding the poll.
    pub fn answer(self, desk: Option<Desk>) {
        if let Some(desk) = desk.or_else(|| self.desks.first().cloned()) {
            let _ = self.reply.send(desk);
        }
    }
}

/// The team report window's state.
pub struct ReportWindow {
    pub range: String,
    pub basis: String,
    pub from: String,
    pub to: String,
    pub people: Vec<Person>,
    pub results: Vec<(String, Option<u64>)>,
    /// Per-person SLA % and median, parallel to `results`.
    pub stats: Vec<report::PersonStats>,
    pub rows: Vec<RankRow>,
    pub subtitle: String,
    pub status: (String, bool),
    pub gate: Arc<ReportGate>,
    pub picker: Option<PeoplePicker>,
    pub focus: bool,
}

/// "Tick everyone the report should cover".
pub struct PeoplePicker {
    pub current: Vec<Person>,
    pub rows: Vec<(Person, bool)>,
}

impl PeoplePicker {
    pub fn new(current: &[Person]) -> Self {
        let chosen = report::chosen_predicate(current);
        Self { current: current.to_vec(), rows: current.iter().map(|p| (p.clone(), chosen(p))).collect() }
    }

    /// Discovery landed: merge, keeping any ticks made meanwhile.
    pub fn discovered(&mut self, found: Vec<Person>) {
        let (people, _) = report::merge(&self.current, found);
        self.rows = report::repopulate(&self.rows, people, report::chosen_predicate(&self.current));
    }

    pub fn chosen(&self) -> Vec<Person> {
        self.rows.iter().filter(|(_, t)| *t).map(|(p, _)| p.clone()).collect()
    }
}

impl ReportWindow {
    pub fn new(cfg: &Config) -> Self {
        let today = stats::today();
        Self {
            range: report::initial_range(cfg),
            basis: config::get_str(cfg, "reportBasis", "resolved").to_owned(),
            from: (today - chrono::Days::new(29)).format("%Y-%m-%d").to_string(),
            to: today.format("%Y-%m-%d").to_string(),
            people: report::report_users(cfg),
            results: Vec::new(),
            stats: Vec::new(),
            rows: Vec::new(),
            subtitle: String::new(),
            status: (String::new(), false),
            gate: Arc::new(ReportGate::default()),
            picker: None,
            focus: false,
        }
    }

    pub fn period(&self) -> Option<(chrono::NaiveDate, chrono::NaiveDate)> {
        report::period(&self.range, &self.from, &self.to, stats::today())
    }

    /// Start a count (Python `refresh`): superseded counts never render.
    pub fn refresh(&mut self, cfg: &Config, svc: &Services, tx: &Sender<UiEvent>, waker: &Waker) {
        if self.gate.closed.load(Ordering::SeqCst) {
            return;
        }
        let Some((start, end)) = self.period() else {
            self.status = ("Dates must be yyyy-mm-dd".into(), true);
            return;
        };
        let desk = config::get_str(cfg, "serviceDeskName", "Service desk");
        self.subtitle = format!("{desk}  ·  {} to {}", start.format("%Y-%m-%d"), end.format("%Y-%m-%d"));
        if self.people.is_empty() {
            self.rows.clear();
            self.status = ("No people chosen yet — use People…".into(), false);
            return;
        }
        let seq = self.gate.seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.status = ("Counting…".into(), false);
        let (cfg, people, basis) = (cfg.clone(), self.people.clone(), self.basis.clone());
        let (make, gate, tx, waker) = (svc.make_transport.clone(), self.gate.clone(), tx.clone(), waker.clone());
        let _ = std::thread::Builder::new().name("team-report".into()).spawn(move || {
            let wanted = move || gate.wanted(seq);
            if let Some(outcome) = report::count_all(seq, &cfg, &people, (&basis, start, end), &make, &wanted) {
                let _ = tx.send(UiEvent::ReportDone(outcome));
                waker();
            }
        });
    }

    pub fn done(&mut self, outcome: Outcome) {
        if !self.gate.wanted(outcome.seq) {
            return;
        }
        self.rows = report::ranking(&outcome.results, &outcome.stats, outcome.total);
        self.status = report::coverage_status(&outcome);
        self.results = outcome.results;
        self.stats = outcome.stats;
    }

    pub fn export(&mut self, root: &Path, platform: &dyn platform::Platform) {
        if self.results.is_empty() {
            self.status = ("Nothing to export yet".into(), true);
            return;
        }
        let (start, end) = self.period().unwrap_or((stats::today(), stats::today()));
        let path = report::export_path(root, start, end);
        match std::fs::write(&path, report::csv_text(&self.results, &self.stats, start, end, &self.basis)) {
            Ok(()) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.status = (format!("Saved {name}"), false);
                platform.open_document(&path);
            }
            Err(e) => self.status = (format!("Could not save: {e}"), true),
        }
    }
}

#[derive(Default)]
struct ReportActions {
    refresh: bool,
    people: bool,
    export: bool,
    close: bool,
}

impl OverlayApp<'_> {
    pub fn secondary_windows(&mut self, ctx: &egui::Context) {
        self.snooze_menu(ctx);
        self.alert_popup(ctx);
        self.preview_popup(ctx);
        self.desk_picker_window(ctx);
        self.alert_picker_window(ctx);
        self.report_window(ctx);
        self.people_picker_window(ctx);
    }

    fn snooze_menu(&mut self, ctx: &egui::Context) {
        if !self.snooze_popup {
            return;
        }
        let anchor = self.snooze_anchor;
        let font = theme::regular(9.0);
        let mut picked = None;
        let area = egui::Area::new(Id::unique("snooze-popup"))
            .order(egui::Order::Foreground)
            .pivot(Align2::LEFT_BOTTOM)
            .fixed_pos(pos2(anchor.left(), anchor.top() - 4.0))
            .constrain(true)
            .show(ctx, |ui| {
                egui::Frame::new().fill(c::input()).stroke(egui::Stroke::new(1.0, c::divider())).corner_radius(4).show(
                    ui,
                    |ui| {
                        ui.spacing_mut().item_spacing = Vec2::ZERO;
                        for (label, minutes) in [("Snooze 15 min", 15), ("Snooze 30 min", 30), ("Snooze 1 hour", 60)] {
                            let resp = widgets::flat_button(
                                ui,
                                label,
                                font.clone(),
                                (c::fg_bright(), c::input(), c::accent()),
                                Some(120.0),
                                true,
                            );
                            if resp.clicked() {
                                picked = Some(minutes);
                            }
                        }
                    },
                );
            });
        if let Some(minutes) = picked {
            self.snooze(minutes);
            return;
        }
        let outside = ctx.input(|i| {
            i.pointer.any_pressed()
                && i.pointer.interact_pos().is_some_and(|p| !area.response.rect.contains(p) && !anchor.contains(p))
        });
        if outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.snooze_popup = false;
        }
    }

    /// The ticket popup above the overlay; each row opens in the browser.
    fn alert_popup(&mut self, ctx: &egui::Context) {
        if !self.st.tooltip_open || self.st.snap.new_issues.is_empty() {
            self.st.tooltip_open = false;
            self.popup_size = None;
            self.popup_hwnd = None;
            return;
        }
        let size = self.popup_size.unwrap_or(vec2(360.0, 120.0));
        let builder = self.popup_builder(POPUP_TITLE, size);
        let lines = alerts::popup_lines(&self.st.snap.new_issues, &self.st.suggestions);
        let mut open = None;
        let mut dismiss = false;
        let (measured, _) =
            ctx.show_viewport_immediate(ViewportId::from_hash_of("alert-popup"), builder, |ui, _class| {
                ui.painter().rect_filled(ui.max_rect(), CornerRadius::ZERO, c::input());
                let inner = egui::Frame::new().fill(c::input()).show(ui, |ui| {
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    popup_contents(ui, ALERT_HEADER, &lines, &mut open, Some(&mut dismiss))
                });
                popup_measure(ui, &inner.response.rect)
            });
        if self.popup_hwnd.is_none() {
            self.popup_hwnd = platform::find_own_window(POPUP_TITLE);
            if let Some(h) = self.popup_hwnd {
                platform::round_corners(h, false);
            }
        }
        if self.popup_size.is_none_or(|s| (s - measured).length() > 1.0) {
            self.popup_size = Some(measured);
            ctx.request_repaint();
        }
        if let Some(key) = open {
            let url = self.ticket_url(&key);
            self.svc.platform.open_url(&url);
        }
        if dismiss {
            self.st.mark_all_seen(Instant::now());
            self.popup_hwnd = None;
        }
    }

    /// A borderless, non-activating popup just above the overlay.
    fn popup_builder(&self, title: &str, size: Vec2) -> ViewportBuilder {
        let ppp = self.ppp;
        let (sw, _) = self.svc.platform.screen_size();
        let (x, y) = (self.rect.x as f32 / ppp, self.rect.y as f32 / ppp);
        let pos = pos2(x.min(sw as f32 / ppp - size.x - 10.0).max(0.0), (y - size.y - 6.0).max(0.0));
        ViewportBuilder::default()
            .with_title(title)
            .with_decorations(false)
            .with_always_on_top()
            .with_taskbar(false)
            .with_active(false)
            .with_resizable(false)
            .with_inner_size(size)
            .with_position(pos)
    }

    /// A queue's newest tickets while its row (or this popup) is hovered.
    fn preview_popup(&mut self, ctx: &egui::Context) {
        let Some((queue_id, queue_name)) = self.preview_queue.clone() else {
            self.preview_size = None;
            self.preview_hwnd = None;
            return;
        };
        if !self.st.visible || self.st.in_settings {
            self.close_preview();
            return;
        }
        let lines: Vec<PopupLine> = match self.preview.get(&queue_id) {
            None => vec![PopupLine::Note("Loading…".into())],
            Some(None) => vec![PopupLine::Note("Could not load this queue".into())],
            Some(Some([])) => vec![PopupLine::Note("No tickets in this queue".into())],
            Some(Some(issues)) => preview::tickets(issues, &self.st.suggestions, chrono::Utc::now())
                .into_iter()
                .map(|t| PopupLine::Ticket {
                    detail: alerts::detail_text(t.age.as_deref(), t.assignee.as_deref()),
                    key: t.key,
                    summary: t.summary,
                    suggestion: t.suggestion,
                })
                .collect(),
        };
        let header = format!("  {} — newest  ", queue_name.to_uppercase());
        let size = self.preview_size.unwrap_or(vec2(360.0, 80.0));
        let builder = self.popup_builder(PREVIEW_TITLE, size);
        let mut open = None;
        let (measured, inside) =
            ctx.show_viewport_immediate(ViewportId::from_hash_of("queue-preview"), builder, |ui, _class| {
                ui.painter().rect_filled(ui.max_rect(), CornerRadius::ZERO, c::input());
                let inner = egui::Frame::new().fill(c::input()).show(ui, |ui| {
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    popup_contents(ui, &header, &lines, &mut open, None)
                });
                popup_measure(ui, &inner.response.rect)
            });
        if inside {
            self.preview_inside = Some(Instant::now());
        }
        if self.preview_hwnd.is_none() {
            self.preview_hwnd = platform::find_own_window(PREVIEW_TITLE);
            if let Some(h) = self.preview_hwnd {
                platform::round_corners(h, false);
            }
        }
        if self.preview_size.is_none_or(|s| (s - measured).length() > 1.0) {
            self.preview_size = Some(measured);
            ctx.request_repaint();
        }
        if let Some(key) = open {
            let url = self.ticket_url(&key);
            self.svc.platform.open_url(&url);
        }
    }

    fn desk_picker_window(&mut self, ctx: &egui::Context) {
        let Some(picker) = &self.desk_picker else { return };
        let desks = picker.desks.clone();
        let builder = ViewportBuilder::default()
            .with_title("Jira Overlay — Pick Desk")
            .with_inner_size([360.0, 90.0 + desks.len() as f32 * 44.0])
            .with_resizable(false);
        let mut answer: Option<Option<Desk>> = None;
        ctx.show_viewport_immediate(ViewportId::from_hash_of("desk-picker"), builder, |ui, _| {
            egui::Frame::new().fill(c::bg_dark()).inner_margin(egui::Margin::symmetric(24, 18)).show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    widgets::label(ui, "Which service desk to monitor?", theme::bold(11.0), c::fg_bright());
                });
                ui.add_space(10.0);
                let width = ui.available_width();
                for desk in &desks {
                    let resp = widgets::flat_button(
                        ui,
                        &desk.label(),
                        theme::regular(10.0),
                        (c::fg_bright(), c::input(), c::accent()),
                        Some(width),
                        true,
                    );
                    ui.add_space(8.0);
                    if resp.clicked() {
                        answer = Some(Some(desk.clone()));
                    }
                }
            });
            if ui.ctx().input(|i| i.viewport().close_requested()) {
                answer = Some(None);
            }
        });
        if let Some(desk) = answer {
            self.desk_picked(desk);
        }
    }

    fn alert_picker_window(&mut self, ctx: &egui::Context) {
        let Some(picker) = &mut self.alert_picker else { return };
        let (_, sh) = self.svc.platform.screen_size();
        let list_h = (picker.names.len() as f32 * 24.0 + 50.0).min(sh as f32 / self.ppp - 250.0).max(60.0);
        let builder = ViewportBuilder::default()
            .with_title("Jira Overlay — Choose Alert Queue")
            .with_inner_size([380.0, list_h + 130.0])
            .with_resizable(false)
            .with_always_on_top();
        let mut result: Option<Option<Vec<String>>> = None;
        ctx.show_viewport_immediate(ViewportId::from_hash_of("alert-picker"), builder, |ui, _| {
            egui::Frame::new().fill(c::bg_dark()).inner_margin(egui::Margin::symmetric(20, 16)).show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    widgets::label(ui, "Which queues should trigger alerts?", theme::bold(11.0), c::fg_bright());
                    widgets::label(
                        ui,
                        "The overlay will appear and notifications will fire\nwhen new tickets arrive in any of these queues.",
                        theme::regular(8.0),
                        c::fg_dim(),
                    );
                });
                ui.add_space(10.0);
                egui::ScrollArea::vertical().max_height(list_h).show(ui, |ui| {
                    for (i, name) in picker.names.iter().enumerate() {
                        ui.push_id(i, |ui| {
                            ui.checkbox(&mut picker.ticks[i], egui::RichText::new(name).font(theme::regular(10.0)));
                        });
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let half = (ui.available_width() - 4.0) / 2.0;
                        if widgets::flat_button(ui, "✓  Save", theme::bold(9.0), (c::white(), c::accent(), c::accent_hi()), Some(half), true)
                            .clicked()
                        {
                            result = Some(Some(picker.chosen()));
                        }
                        if widgets::flat_button(
                            ui,
                            "Don't alert me — just show the counts",
                            theme::regular(9.0),
                            (c::fg_dim(), c::input(), c::chip()),
                            Some(half),
                            true,
                        )
                        .clicked()
                        {
                            result = Some(None);
                        }
                    });
                });
            });
            if ui.ctx().input(|i| i.viewport().close_requested()) {
                result = Some(None);
            }
        });
        if let Some(chosen) = result {
            self.alert_picker_closed(chosen);
        }
    }

    fn report_window(&mut self, ctx: &egui::Context) {
        if self.report.is_none() {
            return;
        }
        let id = ViewportId::from_hash_of("team-report");
        let (sw, sh) = self.svc.platform.screen_size();
        let ppp = self.ppp;
        let builder = ViewportBuilder::default()
            .with_title("Jira Overlay — Team Report")
            .with_inner_size([520.0, 560.0])
            .with_min_inner_size([460.0, 380.0])
            .with_position(pos2(sw as f32 / ppp / 2.0 - 260.0, (sh as f32 / ppp / 2.0 - 300.0).max(20.0)));
        let modal = self.report.as_ref().is_some_and(|r| r.picker.is_some());
        let mut act = ReportActions::default();
        let report = self.report.as_mut().expect("checked");
        if std::mem::take(&mut report.focus) {
            ctx.send_viewport_cmd_to(id, ViewportCommand::Focus);
        }
        ctx.show_viewport_immediate(id, builder, |ui, _| {
            egui::Frame::new().fill(c::bg()).inner_margin(egui::Margin::symmetric(18, 16)).show(ui, |ui| {
                if modal {
                    ui.disable();
                }
                report_contents(ui, report, &mut act);
            });
            if ui.ctx().input(|i| i.viewport().close_requested()) {
                act.close = true;
            }
        });
        let cfg = self.config();
        if act.refresh {
            let (svc, tx, waker) = (self.svc.clone(), self.tx.clone(), self.waker.clone());
            if let Some(r) = &mut self.report {
                r.refresh(&cfg, &svc, &tx, &waker);
            }
        }
        if act.export {
            let platform = self.svc.platform.clone();
            if let Some(r) = &mut self.report {
                r.export(&self.root, platform.as_ref());
            }
        }
        if act.people {
            self.open_people_picker();
        }
        if act.close {
            self.close_report();
        }
    }

    fn open_people_picker(&mut self) {
        let Some(report) = &mut self.report else { return };
        report.picker = Some(PeoplePicker::new(&report.people));
        let (cfg, make, tx, waker) =
            (self.config(), self.svc.make_transport.clone(), self.tx.clone(), self.waker.clone());
        let _ = std::thread::Builder::new().name("people-discovery".into()).spawn(move || {
            let found = report::discover_people(&cfg, &make);
            let _ = tx.send(UiEvent::PeopleFound(found));
            waker();
        });
    }

    pub fn close_report(&mut self) {
        let Some(report) = self.report.take() else { return };
        report.gate.closed.store(true, Ordering::SeqCst);
        let (range, basis) = (report.range.clone(), report.basis.clone());
        self.shared().update(|c| {
            c.insert("reportRange".into(), json!(range));
            c.insert("reportBasis".into(), json!(basis));
        });
        self.shared().save();
    }

    fn people_picker_window(&mut self, ctx: &egui::Context) {
        let Some(picker) = self.report.as_mut().and_then(|r| r.picker.as_mut()) else { return };
        let builder =
            ViewportBuilder::default().with_title("Jira Overlay — Report People").with_inner_size([380.0, 460.0]);
        let mut result: Option<Option<Vec<Person>>> = None;
        ctx.show_viewport_immediate(ViewportId::from_hash_of("report-people"), builder, |ui, _| {
            egui::Frame::new().fill(c::bg()).inner_margin(egui::Margin::symmetric(16, 14)).show(ui, |ui| {
                widgets::label(ui, "Tick everyone the report should cover", theme::regular(9.0), c::fg());
                ui.add_space(8.0);
                let list_h = (ui.available_height() - 44.0).max(60.0);
                egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
                    if picker.rows.is_empty() {
                        widgets::label(ui, "Looking up assignable users…", theme::regular(9.0), c::fg_dim());
                    }
                    for (i, (person, ticked)) in picker.rows.iter_mut().enumerate() {
                        ui.push_id(i, |ui| ui.checkbox(ticked, person.name.as_str()));
                    }
                });
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if widgets::flat_button(
                        ui,
                        "Save",
                        theme::bold(9.0),
                        (c::white(), c::accent(), c::accent_hi()),
                        None,
                        true,
                    )
                    .clicked()
                    {
                        result = Some(Some(picker.chosen()));
                    }
                    ui.add_space(8.0);
                    if widgets::flat_button(
                        ui,
                        "Cancel",
                        theme::regular(9.0),
                        (c::fg(), c::input(), c::chip()),
                        None,
                        true,
                    )
                    .clicked()
                    {
                        result = Some(None);
                    }
                });
            });
            if ui.ctx().input(|i| i.viewport().close_requested()) {
                result = Some(None);
            }
        });
        let Some(result) = result else { return };
        let cfg = self.config();
        let (svc, tx, waker) = (self.svc.clone(), self.tx.clone(), self.waker.clone());
        let Some(report) = self.report.as_mut() else { return };
        report.picker = None;
        if let Some(people) = result {
            report.people = people.clone();
            let list: Vec<Value> = people.iter().map(Person::to_json).collect();
            self.st.shared.update(|c| c.insert("reportUsers".into(), Value::Array(list)));
            self.st.shared.save();
            report.refresh(&cfg, &svc, &tx, &waker);
        }
    }
}

/// A popup's content size, and whether the pointer is over it.
fn popup_measure(ui: &Ui, content: &Rect) -> (Vec2, bool) {
    let inside = ui.ctx().input(|i| i.pointer.hover_pos()).is_some_and(|p| content.contains(p));
    (content.size(), inside)
}

fn popup_contents(
    ui: &mut Ui,
    header: &str,
    lines: &[PopupLine],
    open: &mut Option<String>,
    dismiss: Option<&mut bool>,
) {
    let width = popup_width(ui, header, lines, dismiss.is_some());
    let font7b = theme::bold(7.0);
    let h = widgets::text_size(ui, header, &font7b).y + 8.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, h), Sense::hover());
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, header, font7b.clone(), c::fg_dim());
    widgets::describe(&resp, Role::Label, header.trim());
    rule(ui, width, c::fg_dim(), 0.0);
    for line in lines {
        match line {
            PopupLine::Header(name) => {
                ui.add_space(4.0);
                widgets::line(ui, &format!("  {name}  "), font7b.clone(), c::info());
            }
            PopupLine::Ticket { key, summary, detail, suggestion } => {
                ui.add_space(2.0);
                let key_font = theme::bold(8.0);
                let text_font = theme::regular(8.0);
                let line_h = widgets::text_size(ui, key, &key_font).y;
                let rows = if suggestion.is_some() || detail.is_some() { 2.0 } else { 1.0 };
                let (rect, resp) = ui.allocate_exact_size(vec2(width, line_h * rows), Sense::click());
                ui.add_space(2.0);
                let row = rect.shrink2(vec2(8.0, 0.0));
                if resp.hovered() {
                    ui.painter().rect_filled(row, CornerRadius::ZERO, c::chip());
                }
                let top = row.top() + line_h / 2.0;
                ui.painter().text(pos2(row.left(), top), Align2::LEFT_CENTER, key, key_font, c::accent());
                ui.painter().text(
                    pos2(row.left() + 76.0, top),
                    Align2::LEFT_CENTER,
                    summary,
                    text_font.clone(),
                    c::fg(),
                );
                if let Some(detail) = detail {
                    let at = pos2(row.left() + 76.0, top + line_h);
                    ui.painter().text(at, Align2::LEFT_CENTER, detail, text_font.clone(), c::fg_dim());
                }
                if let Some((hint, review)) = suggestion {
                    // The hint carries its own "→" or (text-style) "⚠".
                    let colour = if *review { c::warn() } else { c::ok() };
                    let x = match detail {
                        Some(d) => row.left() + 76.0 + widgets::text_size(ui, d, &text_font).x + 12.0,
                        None => row.left() + 76.0,
                    };
                    let drawn = ui.painter().text(pos2(x, top + line_h), Align2::LEFT_CENTER, hint, text_font, colour);
                    // Readable by screen readers (hover-only: the row keeps the click).
                    let note = ui.interact(drawn, ui.scope_id().with(("hint", key)), Sense::hover());
                    widgets::describe(&note, Role::Label, hint);
                }
                widgets::describe(&resp, Role::Button, &format!("{key} {summary}"));
                if resp.clicked() {
                    *open = Some(key.clone());
                }
            }
            PopupLine::More(n) => {
                widgets::line(ui, &format!("  …and {n} more  "), theme::regular(7.0), c::fg_dim());
            }
            PopupLine::Note(text) => {
                ui.add_space(4.0);
                widgets::line(ui, &format!("  {text}  "), theme::regular(8.0), c::fg_dim());
                ui.add_space(4.0);
            }
        }
    }
    let Some(dismiss) = dismiss else {
        ui.add_space(4.0);
        return;
    };
    rule(ui, width, c::fg_dim(), 4.0);
    let resp = Clickable {
        text: "✓  Dismiss — hide until new tickets",
        font: theme::regular(8.0),
        color: c::ok(),
        hover_color: c::ok(),
        bg: None,
        hover_bg: Some(c::chip()),
        pad: vec2(8.0, 4.0),
        label: None,
    };
    let size = vec2(width - 16.0, resp.size(ui).y);
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        if resp.show_at(ui, rect, Id::unique("dismiss")).clicked() {
            *dismiss = true;
        }
    });
    ui.add_space(4.0);
}

fn popup_width(ui: &Ui, header: &str, lines: &[PopupLine], dismiss: bool) -> f32 {
    let small = theme::regular(8.0);
    let mut width = widgets::text_size(ui, header, &theme::bold(7.0)).x;
    if dismiss {
        width = width.max(widgets::text_size(ui, "✓  Dismiss — hide until new tickets", &small).x + 32.0);
    }
    for line in lines {
        let w = match line {
            PopupLine::Header(name) => widgets::text_size(ui, &format!("  {name}  "), &theme::bold(7.0)).x,
            PopupLine::Ticket { summary, detail, suggestion, .. } => {
                let main = 16.0 + 76.0 + widgets::text_size(ui, summary, &small).x;
                let detail_w = detail.as_ref().map_or(0.0, |d| widgets::text_size(ui, d, &small).x + 12.0);
                let hint_w = suggestion.as_ref().map_or(0.0, |(l, _)| widgets::text_size(ui, l, &small).x);
                main.max(16.0 + 76.0 + detail_w + hint_w)
            }
            PopupLine::More(n) => widgets::text_size(ui, &format!("  …and {n} more  "), &theme::regular(7.0)).x,
            PopupLine::Note(text) => widgets::text_size(ui, &format!("  {text}  "), &small).x,
        };
        width = width.max(w);
    }
    width + 4.0
}

fn rule(ui: &mut Ui, width: f32, color: Color32, above: f32) {
    ui.add_space(above);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 1.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, color);
}

fn report_contents(ui: &mut Ui, r: &mut ReportWindow, act: &mut ReportActions) {
    ui.spacing_mut().item_spacing = vec2(8.0, 4.0);
    widgets::label(ui, &theme::track("TEAM REPORT"), theme::bold(9.0), c::accent());
    widgets::label(ui, &r.subtitle, theme::regular(9.0), c::fg_dim());
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        for (label, _) in report::REPORT_RANGES {
            if ui.radio_value(&mut r.range, label.to_owned(), label).clicked() {
                act.refresh = true;
            }
        }
    });
    if r.range == "Custom" {
        ui.horizontal(|ui| {
            for (text, value) in [("From", &mut r.from), ("To", &mut r.to)] {
                widgets::label(ui, text, theme::regular(8.0), c::fg_dim());
                let resp = ui.add(egui::TextEdit::singleline(value).desired_width(90.0));
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    act.refresh = true;
                }
            }
            widgets::label(ui, "yyyy-mm-dd", theme::regular(8.0), c::fg_dim());
        });
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        widgets::label(ui, "Count by", theme::regular(8.0), c::fg_dim());
        for (label, value) in report::REPORT_BASES {
            if ui.radio_value(&mut r.basis, value.to_owned(), label).clicked() {
                act.refresh = true;
            }
        }
    });
    widgets::divider(ui, 8.0, 8.0);
    let list_h = (ui.available_height() - 60.0).max(80.0);
    egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
        if r.rows.is_empty() {
            widgets::label(ui, "Nothing to show yet.", theme::regular(9.0), c::fg_dim());
        }
        for row in &r.rows {
            rank_row(ui, row);
        }
    });
    widgets::divider(ui, 12.0, 8.0);
    ui.horizontal(|ui| {
        for (text, flag) in
            [("People…", &mut act.people), ("Export CSV", &mut act.export), ("Refresh", &mut act.refresh)]
        {
            let resp = Clickable {
                text,
                font: theme::regular(9.0),
                color: c::fg(),
                hover_color: c::fg_bright(),
                bg: Some(c::input()),
                hover_bg: Some(c::chip()),
                pad: vec2(12.0, 5.0),
                label: None,
            }
            .show(ui);
            if resp.clicked() {
                *flag = true;
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (text, warn) = &r.status;
            widgets::label(ui, text, theme::regular(8.0), if *warn { c::warn() } else { c::fg_dim() });
        });
    });
}

fn rank_row(ui: &mut Ui, row: &RankRow) {
    let first = row.place == 1;
    let font = theme::regular(9.0);
    let h = widgets::text_size(ui, &row.name, &font).y + 6.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    let label = [row.name.as_str(), row.count.as_str(), row.sla.as_str(), row.median.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    widgets::describe(&resp, Role::Label, &label);
    let p = ui.painter();
    let cy = rect.center().y;
    p.text(pos2(rect.left(), cy), Align2::LEFT_CENTER, format!("{}.", row.place), font.clone(), c::fg_dim());
    let name_rect = Rect::from_min_max(pos2(rect.left() + 26.0, rect.top()), pos2(rect.left() + 176.0, rect.bottom()));
    p.with_clip_rect(name_rect).text(
        pos2(name_rect.left(), cy),
        Align2::LEFT_CENTER,
        &row.name,
        font.clone(),
        if first { c::fg_bright() } else { c::fg() },
    );
    let chip = Rect::from_min_max(pos2(rect.right() - 52.0, rect.top() + 1.0), pos2(rect.right(), rect.bottom() - 1.0));
    p.rect_filled(chip, CornerRadius::same(3), c::badge());
    p.text(
        chip.center(),
        Align2::CENTER_CENTER,
        &row.count,
        theme::bold(9.0),
        if first { c::accent() } else { c::fg() },
    );
    p.text(pos2(chip.left() - 8.0, cy), Align2::RIGHT_CENTER, &row.share, theme::regular(8.0), c::fg_dim());
    // Per-person SLA % and median time to resolution, before the share.
    let stats_text = [row.sla.as_str(), row.median.as_str()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>();
    let stats_right = chip.left() - 48.0;
    let mut track_right = stats_right;
    if !stats_text.is_empty() {
        let text = stats_text.join(" · ");
        let drawn = p.text(pos2(stats_right, cy), Align2::RIGHT_CENTER, &text, theme::regular(8.0), c::fg());
        track_right = drawn.left() - 8.0;
    }
    let track = Rect::from_min_max(pos2(name_rect.right() + 8.0, cy - 3.0), pos2(track_right, cy + 3.0));
    if track.width() > 0.0 {
        p.rect_filled(track, CornerRadius::ZERO, c::input());
        if row.bar > 0.0 {
            let bar = Rect::from_min_size(track.min, vec2(track.width() * row.bar, track.height()));
            p.rect_filled(bar, CornerRadius::ZERO, if first { c::accent() } else { c::info() });
        }
    }
}
