//! The overlay window's contents: header, queue rows, completed/SLA line,
//! footer and the inline Settings panel.

use std::time::Instant;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, Id, Layout, Rect, Role, Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2, pos2,
    vec2,
};

use crate::app::OverlayApp;
use crate::settings::REFRESH_CHOICES;
use crate::state::{self, Tone};
use crate::theme::{self, Icon, c};
use crate::util;
use crate::widgets::{self, Clickable};

pub fn tone_color(tone: Tone) -> Color32 {
    match tone {
        Tone::Dim => c::FG_DIM,
        Tone::Ok => c::OK,
        Tone::Warn => c::WARN,
        Tone::Alert => c::ALERT,
    }
}

/// Things a frame asked for, applied once drawing is done.
#[derive(Default)]
pub struct Actions {
    pub open_url: Option<String>,
    pub open_desk: bool,
    pub open_completed: bool,
    pub settings: bool,
    pub refresh: bool,
    pub snooze: bool,
    pub hide: bool,
    pub quit: bool,
    pub snap_nearest: bool,
    pub check_updates: bool,
    pub open_download: bool,
    pub diagnostics: bool,
    pub reconfigure: bool,
    pub save: bool,
    pub cancel: bool,
    /// The press landed somewhere that drags the window.
    pub drag_press: bool,
}

impl OverlayApp<'_> {
    /// The root viewport's whole frame.
    pub fn ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let now = Instant::now();
        self.ensure_style(&ctx);
        self.ppp = ctx.pixels_per_point();
        if ctx.input(|i| i.viewport().close_requested()) && !self.closing {
            // Alt+F4 on the overlay is a Quit, as WM_DELETE_WINDOW was.
            self.quit(&ctx);
        }
        self.tick_geometry();

        // Window drag: tracked in screen pixels, because the window moves
        // under the pointer and egui's own positions are window-relative.
        self.suppress_click = false;
        let (pressed, released, down) =
            ctx.input(|i| (i.pointer.primary_pressed(), i.pointer.primary_released(), i.pointer.primary_down()));
        if self.drag.is_some() && down {
            self.drag_motion();
        }
        if released {
            self.end_drag();
        }

        let full = ui.max_rect();
        let bg = ui.interact(full, Id::unique("overlay-background"), Sense::click());
        ui.painter().rect_filled(full, CornerRadius::ZERO, c::BG);
        ui.painter().rect_stroke(full, CornerRadius::ZERO, Stroke::new(1.0, c::BORDER), StrokeKind::Inside);
        let inner = Rect::from_min_max(full.min + vec2(15.0, 11.0), full.max - vec2(15.0, 11.0));
        let mut actions = Actions { drag_press: bg.is_pointer_button_down_on(), ..Default::default() };
        if bg.double_clicked() {
            actions.snap_nearest = true;
        }

        let mut body = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::top_down(egui::Align::Min)));
        body.spacing_mut().item_spacing = vec2(0.0, 0.0);
        self.header(&mut body, &mut actions);
        widgets::divider(&mut body, 8.0, 6.0);
        let in_settings = self.st.in_settings && self.settings.is_some();
        if in_settings {
            self.settings_view(&mut body, &mut actions);
        } else {
            let top = body.cursor().top();
            self.compact(&mut body, &mut actions, now);
            let used = body.cursor().top() - top;
            // header + divider above, frame insets around.
            let header_h = top - inner.top();
            self.after_compact_frame(used + header_h + 22.0);
        }

        if pressed && actions.drag_press && (!in_settings || self.drag.is_none()) {
            self.begin_drag();
        }
        self.apply_actions(&ctx, actions);
        self.secondary_windows(&ctx);
    }

    fn apply_actions(&mut self, ctx: &egui::Context, a: Actions) {
        let now = Instant::now();
        let click_ok = !self.suppress_click;
        if let Some(url) = a.open_url.filter(|_| click_ok) {
            self.svc.platform.open_url(&url);
        }
        if a.open_desk && click_ok {
            self.open_service_desk();
        }
        if a.open_completed && click_ok {
            self.open_completed_today();
        }
        if a.snap_nearest {
            self.snap_to_nearest();
        }
        if a.settings {
            self.toggle_settings();
        }
        if a.refresh {
            self.refresh();
        }
        if a.snooze {
            self.snooze_cycle();
        }
        if a.hide {
            self.st.user_hide(now);
        }
        if a.check_updates {
            self.check_updates(true);
        }
        if a.open_download {
            self.open_update_download();
        }
        if a.diagnostics {
            self.open_diagnostics();
        }
        if a.reconfigure {
            self.reconfigure(ctx);
        }
        if a.save {
            self.save_settings();
        }
        if a.cancel {
            self.close_settings();
        }
        if a.quit {
            self.quit(ctx);
        }
    }

    fn header(&mut self, ui: &mut Ui, a: &mut Actions) {
        let width = ui.available_width();
        let title_font = theme::bold(8.0);
        let gear_font = if self.has_icon_font { theme::icon_font(11.0) } else { theme::regular(11.0) };
        let icon = if self.st.in_settings { Icon::Close } else { Icon::Settings };
        let gear = Clickable {
            text: icon.glyph(self.has_icon_font),
            font: gear_font,
            color: c::FG_DIM,
            hover_color: c::FG_BRIGHT,
            bg: None,
            hover_bg: Some(c::CHIP),
            pad: vec2(6.0, 2.0),
            label: Some("Settings"),
        };
        let gear_size = gear.size(ui);
        let title = self.title_text(ui);
        let title_size = widgets::text_size(ui, &title, &title_font);
        let height = gear_size.y.max(title_size.y);
        let (row, header) = ui.allocate_exact_size(vec2(width, height), Sense::click());
        if header.is_pointer_button_down_on() {
            a.drag_press = true;
        }
        if header.double_clicked() {
            a.snap_nearest = true;
        }
        let gear_rect = Rect::from_min_size(pos2(row.right() - gear_size.x, row.top()), gear_size);
        let gear_resp = gear.show_at(ui, gear_rect, Id::unique("gear")).on_hover_text("Settings");
        if gear_resp.clicked() {
            a.settings = true;
        }
        let title_rect = Rect::from_min_size(pos2(row.left(), row.center().y - title_size.y / 2.0), title_size);
        let title_resp = ui.interact(title_rect, Id::unique("title"), Sense::click());
        let color = if title_resp.hovered() { c::ACCENT_HI } else { c::ACCENT };
        ui.painter().text(title_rect.left_center(), Align2::LEFT_CENTER, &title, title_font, color);
        widgets::describe(&title_resp, Role::Button, "Open service desk in Jira");
        let title_resp = title_resp.on_hover_text("Open service desk in Jira");
        if title_resp.is_pointer_button_down_on() {
            a.drag_press = true;
        }
        if title_resp.double_clicked() {
            a.snap_nearest = true;
        } else if title_resp.clicked() {
            a.open_desk = true;
        }
    }

    fn compact(&mut self, ui: &mut Ui, a: &mut Actions, now: Instant) {
        let width = ui.available_width();
        let cfg = self.config();
        let overlay_width = util::cfg_int(&cfg, "overlayWidth", 260);
        if let Some(err) = self.st.body_error.clone() {
            let rect = centered_text(ui, &err, theme::regular(8.0), c::ALERT, 0.0);
            let resp = ui.interact(rect, Id::unique("error-label"), Sense::click());
            widgets::describe(&resp, Role::Label, &err);
            if resp.is_pointer_button_down_on() {
                a.drag_press = true;
            }
        } else {
            if let Some(note) = self.st.empty_note {
                let rect = centered_text(ui, note, theme::regular(8.0), c::FG_DIM, 6.0);
                let resp = ui.interact(rect, Id::unique("empty-note"), Sense::click());
                widgets::describe(&resp, Role::Label, note);
                if resp.is_pointer_button_down_on() {
                    a.drag_press = true;
                }
            }
            let mut alert_hovered = false;
            let rows = self.st.rows.clone();
            for row in &rows {
                ui.add_space(1.0);
                let name_font = theme::regular(9.0);
                let chip_font = theme::bold(8.0);
                let label = state::row_label(&row.name, overlay_width);
                let text_h = widgets::text_size(ui, &label, &name_font).y;
                let (rect, resp) = ui.allocate_exact_size(vec2(width, text_h + 6.0), Sense::click());
                ui.add_space(1.0);
                let hovered = resp.hovered();
                let fill = match self.st.flash_t(&row.name, now) {
                    Some(t) => Some(theme::lerp(c::FLASH, c::BG, t)),
                    None => hovered.then_some(c::HOVER),
                };
                if let Some(fill) = fill {
                    ui.painter().rect_filled(rect, CornerRadius::ZERO, fill);
                }
                let name_color = if row.is_alert { c::INFO } else { c::FG };
                ui.painter().text(
                    pos2(rect.left() + 4.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    &label,
                    name_font,
                    name_color,
                );
                let (chip_fg, chip_bg, dot) = match (row.is_alert, row.count > 0) {
                    (true, true) => (c::WHITE, c::ACCENT, true),
                    (true, false) => (c::FG_DIM, c::BADGE, false),
                    (false, true) => (c::FG_BRIGHT, c::BADGE, false),
                    (false, false) => (c::FG_DIM, c::BADGE, false),
                };
                let count = row.count.to_string();
                let chip_size = widgets::text_size(ui, &count, &chip_font) + vec2(16.0, 2.0);
                let chip = Rect::from_min_size(
                    pos2(rect.right() - 4.0 - chip_size.x, rect.center().y - chip_size.y / 2.0),
                    chip_size,
                );
                ui.painter().rect_filled(chip, CornerRadius::same(3), chip_bg);
                ui.painter().text(chip.center(), Align2::CENTER_CENTER, &count, chip_font, chip_fg);
                if dot {
                    ui.painter().text(
                        pos2(chip.left() - 5.0, rect.center().y),
                        Align2::RIGHT_CENTER,
                        "●",
                        theme::regular(8.0),
                        c::INFO,
                    );
                }
                widgets::describe(&resp, Role::Button, &format!("{} {count}", row.name));
                if resp.is_pointer_button_down_on() {
                    a.drag_press = true;
                }
                if resp.clicked() {
                    a.open_url = Some(self.row_url(&row.id));
                }
                if row.is_alert && hovered {
                    alert_hovered = true;
                }
            }
            self.alert_hover(ui, alert_hovered);
        }

        widgets::divider(ui, 8.0, 4.0);
        // Completed today / SLA, opening the same JQL in Jira.
        let done_font = theme::regular(9.0);
        let sla_font = theme::bold(8.0);
        let done_h = widgets::text_size(ui, &self.st.done_text, &done_font).y + 4.0;
        let (rect, resp) = ui.allocate_exact_size(vec2(width, done_h), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(rect, CornerRadius::ZERO, c::DONE_HOVER);
        }
        ui.painter().text(
            pos2(rect.left() + 2.0, rect.center().y),
            Align2::LEFT_CENTER,
            &self.st.done_text,
            done_font,
            c::OK,
        );
        if let Some((text, tone)) = &self.st.sla {
            ui.painter().text(
                pos2(rect.right() - 2.0, rect.center().y),
                Align2::RIGHT_CENTER,
                text,
                sla_font,
                tone_color(*tone),
            );
        }
        widgets::describe(&resp, Role::Button, &self.st.done_text);
        let resp = resp.on_hover_text("Open completed tickets in Jira");
        if resp.is_pointer_button_down_on() {
            a.drag_press = true;
        }
        if resp.clicked() {
            a.open_completed = true;
        }
        widgets::divider(ui, 6.0, 4.0);
        self.footer(ui, a);
    }

    /// Hovering an alert queue row opens the ticket popup; leaving it for
    /// somewhere outside the overlay closes it.
    fn alert_hover(&mut self, ui: &Ui, hovered: bool) {
        if hovered && !self.alert_row_hovered && !self.st.tooltip_open && !self.st.snap.new_issues.is_empty() {
            self.st.tooltip_open = true;
        }
        if !hovered && self.alert_row_hovered && ui.ctx().input(|i| i.pointer.hover_pos()).is_none() {
            self.st.tooltip_open = false;
        }
        self.alert_row_hovered = hovered;
    }

    fn footer(&mut self, ui: &mut Ui, a: &mut Actions) {
        let width = ui.available_width();
        let font = if self.has_icon_font { theme::icon_font(11.0) } else { theme::regular(11.0) };
        let snoozed = self.now_snoozed();
        let buttons = [
            (Icon::Refresh, "Refresh now", c::FG_DIM, c::FG_BRIGHT),
            (Icon::Snooze, "Snooze alerts", if snoozed { c::WARN } else { c::FG_DIM }, c::FG_BRIGHT),
            (Icon::Hide, "Hide until new tickets arrive", c::FG_DIM, c::FG_BRIGHT),
            (Icon::Close, "Quit", c::FG_DIM, c::ALERT),
        ];
        let sizes: Vec<Vec2> = buttons
            .iter()
            .map(|(icon, ..)| {
                Clickable::new(icon.glyph(self.has_icon_font), font.clone(), c::FG_DIM).size(ui) + vec2(14.0, 6.0)
            })
            .collect();
        let height = sizes.iter().map(|s| s.y).fold(0.0, f32::max);
        let (row, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
        let mut x = row.left();
        for ((icon, tip, color, hover), size) in buttons.into_iter().zip(sizes) {
            let rect = Rect::from_min_size(pos2(x, row.top()), size);
            x += size.x + 2.0;
            let resp = Clickable {
                text: icon.glyph(self.has_icon_font),
                font: font.clone(),
                color,
                hover_color: hover,
                bg: None,
                hover_bg: Some(c::CHIP),
                pad: vec2(7.0, 3.0),
                label: Some(tip),
            }
            .show_at(ui, rect, Id::unique(("footer", tip)))
            .on_hover_text(tip);
            if icon == Icon::Snooze {
                self.snooze_anchor = rect;
            }
            if resp.clicked() {
                match icon {
                    Icon::Refresh => a.refresh = true,
                    Icon::Snooze => a.snooze = true,
                    Icon::Hide => a.hide = true,
                    Icon::Close => a.quit = true,
                    Icon::Settings => {}
                }
            }
        }
        let (text, tone) = &self.st.status;
        let status = ui.painter().text(
            pos2(row.right(), row.center().y),
            Align2::RIGHT_CENTER,
            text,
            theme::regular(7.0),
            tone_color(*tone),
        );
        let resp = ui.interact(status, Id::unique("status-line"), Sense::hover());
        widgets::describe(&resp, Role::Label, text);
    }

    fn settings_view(&mut self, ui: &mut Ui, a: &mut Actions) {
        let footer_h = 40.0;
        let avail = ui.available_height();
        let can_open = self.updates.can_open();
        let update_status = self.updates.status.clone();
        let Some(panel) = self.settings.as_mut() else { return };
        egui::ScrollArea::vertical()
            .id_salt("settings-scroll")
            .max_height((avail - footer_h).max(40.0))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 3.0);
                let f = &mut panel.form;
                let dim8 = |ui: &mut Ui, text: &str| widgets::label(ui, text, theme::regular(8.0), c::FG_DIM);
                let fg8 = |ui: &mut Ui, text: &str| widgets::label(ui, text, theme::regular(8.0), c::FG);

                widgets::section(ui, "Stats refresh interval");
                dim8(ui, "Queue counts update live every 5 s.  This controls how often\nSLA % and Completed Today refresh (heavier queries).");
                ui.horizontal(|ui| {
                    for (secs, label) in REFRESH_CHOICES {
                        ui.radio_value(&mut f.refresh, secs, label);
                    }
                });

                widgets::section(ui, "Alerts");
                ui.checkbox(&mut f.notifications, "Desktop notification");
                ui.checkbox(&mut f.sound, "Sound alert");

                widgets::section(ui, "Appearance");
                egui::Grid::new("appearance").num_columns(2).spacing(vec2(8.0, 6.0)).show(ui, |ui| {
                    // Edited through copies: a value off the slider's grid (a
                    // hand-edited 0.93) must survive an untouched save.
                    ui.label("Transparency");
                    let mut alpha = f.alpha;
                    let slider = egui::Slider::new(&mut alpha, 0.3..=1.0)
                        .step_by(0.05)
                        .fixed_decimals(2)
                        .clamping(egui::SliderClamping::Never);
                    if ui.add(slider).changed() {
                        f.alpha = alpha;
                    }
                    ui.end_row();
                    ui.label("Width");
                    let mut width = f.width;
                    let slider = egui::Slider::new(&mut width, 200..=420).step_by(10.0).clamping(egui::SliderClamping::Never);
                    if ui.add(slider).changed() {
                        f.width = width;
                    }
                    ui.end_row();
                });
                ui.checkbox(&mut f.always_visible, "Always show (dashboard mode)");

                widgets::section(ui, "Queue filtering");
                fg8(ui, "Hide queues whose name contains:");
                ui.add(egui::TextEdit::singleline(&mut f.skip_keywords).desired_width(f32::INFINITY));
                fg8(ui, "Hide queues with more than N tickets (0 = no limit):");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.max_queue).desired_width(80.0));
                    if ui.small_button("▲").clicked() {
                        f.step_max_queue(true);
                    }
                    if ui.small_button("▼").clicked() {
                        f.step_max_queue(false);
                    }
                });

                if !f.queue_names.is_empty() {
                    widgets::section(ui, "Queues to show");
                    for i in 0..f.queue_names.len() {
                        let name = f.queue_names[i].clone();
                        ui.push_id(("show", i), |ui| ui.checkbox(&mut f.show[i], name));
                    }
                }
                widgets::section(ui, "Tray badge shows");
                for i in 0..f.queue_names.len() {
                    let name = f.queue_names[i].clone();
                    ui.push_id(("tray", i), |ui| ui.radio_value(&mut f.tray_badge, name.clone(), name));
                }
                widgets::section(ui, "Alert queues (trigger overlay + notifications)");
                for i in 0..f.queue_names.len() {
                    let name = f.queue_names[i].clone();
                    ui.push_id(("alert", i), |ui| ui.checkbox(&mut f.alerts[i], name));
                }

                widgets::section(ui, "SLA & completed today");
                ui.checkbox(&mut f.sla_enabled, "Show SLA compliance");
                ui.checkbox(&mut f.weekends_off, "We don't work weekends (Sat/Sun show Friday)");
                widgets::label(ui, "SLA field names (comma-separated; any breach counts):", theme::regular(9.0), c::FG);
                ui.add_enabled(f.sla_enabled, egui::TextEdit::singleline(&mut f.sla_field).desired_width(f32::INFINITY));
                dim8(ui, "Plenty of teams have SLAs and don't track them.\nTurn this off and the overlay stops asking Jira about them entirely.");
                fg8(ui, "Extra JQL filter (e.g. \"assignee is not EMPTY\"):");
                ui.add(egui::TextEdit::singleline(&mut f.completed_filter).desired_width(f32::INFINITY));

                widgets::section(ui, "System");
                ui.checkbox(&mut f.check_updates, "Check for application updates");
                dim8(ui, "Updates notify only. Installation requires user/IT approval.");
                dim8(ui, &update_status);
                if ui.button("Check for updates").clicked() {
                    a.check_updates = true;
                }
                if ui.add_enabled(can_open, egui::Button::new("Open update download")).clicked() {
                    a.open_download = true;
                }
                ui.checkbox(&mut f.startup, "Run on Windows startup");
                dim8(ui, &format!("Jira Overlay {}", jo_core::VERSION));
                if let Some((healthy, message)) = &panel.recovery {
                    widgets::label(ui, message, theme::regular(8.0), if *healthy { c::FG_DIM } else { c::WARN });
                    if ui.button("Open local diagnostic logs").clicked() {
                        a.diagnostics = true;
                    }
                }
                ui.add_space(4.0);
                if ui.button("Reconfigure credentials…").clicked() {
                    a.reconfigure = true;
                }

                if let Some(lines) = &panel.learner {
                    widgets::section(ui, "Triage learner");
                    ui.horizontal(|ui| {
                        let (dot, head) = if lines.healthy { (c::OK, c::FG_BRIGHT) } else { (c::ALERT, c::ALERT) };
                        widgets::label(ui, "●", theme::regular(10.0), dot);
                        widgets::label(ui, &lines.headline, theme::bold(9.0), head);
                    });
                    if let Some(details) = &lines.details {
                        dim8(ui, details);
                    }
                    if let Some(drift) = &lines.drift {
                        widgets::label(ui, drift, theme::regular(8.0), c::WARN);
                    }
                }
                ui.add_space(6.0);
            });

        ui.add_space(6.0);
        let half = (ui.available_width() - 4.0) / 2.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 0.0);
            let save = widgets::flat_button(
                ui,
                "✓  Save",
                theme::bold(9.0),
                (c::WHITE, c::ACCENT, c::ACCENT_HI),
                Some(half),
                true,
            );
            if save.clicked() {
                a.save = true;
            }
            let cancel = widgets::flat_button(
                ui,
                "✕  Cancel",
                theme::regular(9.0),
                (c::FG, c::INPUT, c::CHIP),
                Some(half),
                true,
            );
            if cancel.clicked() {
                a.cancel = true;
            }
        });
    }
}

/// A centred one-line label; returns its rect.
fn centered_text(ui: &mut Ui, text: &str, font: egui::FontId, color: Color32, pad_y: f32) -> Rect {
    let size = widgets::text_size(ui, text, &font);
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), size.y + pad_y * 2.0), Sense::hover());
    ui.painter().text(row.center(), Align2::CENTER_CENTER, text, font, color);
    Rect::from_center_size(row.center(), size)
}
