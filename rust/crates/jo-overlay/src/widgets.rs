//! Small hand-drawn widgets matching the Tk originals (flat labels that act
//! as buttons, hover chips, section headers).

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Rect, Response, Role, Sense, Stroke, Ui, Vec2, WidgetInfo, pos2, vec2,
};

use crate::theme::{self, c};

pub fn text_size(ui: &Ui, text: &str, font: &FontId) -> Vec2 {
    ui.painter().layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE).size()
}

/// Mark a hand-drawn widget for screen readers (and tests).
pub fn describe(resp: &Response, kind: Role, label: &str) {
    let enabled = resp.enabled();
    let label = label.to_owned();
    resp.widget_info(move || WidgetInfo::labeled(kind, enabled, label.clone()));
}

/// A text label that behaves like a button: colour change and optional
/// chip background on hover.
pub struct Clickable<'a> {
    pub text: &'a str,
    pub font: FontId,
    pub color: Color32,
    pub hover_color: Color32,
    pub bg: Option<Color32>,
    pub hover_bg: Option<Color32>,
    pub pad: Vec2,
    /// Accessible name (defaults to the text).
    pub label: Option<&'a str>,
}

impl<'a> Clickable<'a> {
    pub fn new(text: &'a str, font: FontId, color: Color32) -> Self {
        Self { text, font, color, hover_color: color, bg: None, hover_bg: None, pad: Vec2::ZERO, label: None }
    }

    pub fn size(&self, ui: &Ui) -> Vec2 {
        text_size(ui, self.text, &self.font) + self.pad * 2.0
    }

    /// Draw at `rect` (already allocated by the caller's layout).
    pub fn show_at(self, ui: &mut Ui, rect: Rect, id: egui::Id) -> Response {
        let resp = ui.interact(rect, id, Sense::click());
        self.paint(ui, rect, &resp);
        resp
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let size = self.size(ui);
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        self.paint(ui, rect, &resp);
        resp
    }

    fn paint(self, ui: &Ui, rect: Rect, resp: &Response) {
        let hovered = resp.hovered();
        if let Some(bg) = if hovered { self.hover_bg.or(self.bg) } else { self.bg } {
            ui.painter().rect_filled(rect, CornerRadius::same(3), bg);
        }
        let color = if hovered { self.hover_color } else { self.color };
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, self.text, self.font.clone(), color);
        describe(resp, Role::Button, self.label.unwrap_or(self.text));
    }
}

/// A full-width hairline with space above and below.
pub fn divider(ui: &mut Ui, above: f32, below: f32) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, above + 1.0 + below), Sense::hover());
    let y = rect.top() + above + 0.5;
    ui.painter().hline(rect.x_range(), y, Stroke::new(1.0, c::DIVIDER));
}

/// Settings section title: small accent caps over a divider.
pub fn section(ui: &mut Ui, title: &str) {
    ui.add_space(theme::pt(9.0));
    let font = theme::bold(7.0);
    let size = text_size(ui, &title.to_uppercase(), &font);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), size.y), Sense::hover());
    ui.painter().text(rect.left_center(), Align2::LEFT_CENTER, title.to_uppercase(), font, c::ACCENT);
    divider(ui, 3.0, 2.0);
}

/// A flat filled button (Save, Cancel, Check for updates…).
pub fn flat_button(
    ui: &mut Ui,
    text: &str,
    font: FontId,
    (fg, bg, hover_bg): (Color32, Color32, Color32),
    width: Option<f32>,
    enabled: bool,
) -> Response {
    let natural = text_size(ui, text, &font) + vec2(20.0, 12.0);
    let size = vec2(width.map_or(natural.x, |w| w.max(natural.x)), natural.y);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    let fill = if enabled && resp.hovered() { hover_bg } else { bg };
    ui.painter().rect_filled(rect, CornerRadius::same(3), fill);
    let color = if enabled { fg } else { c::FG_DIM };
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, font, color);
    let resp = if enabled { resp } else { resp.on_hover_cursor(egui::CursorIcon::Default) };
    let label = text.to_owned();
    resp.widget_info(move || WidgetInfo::labeled(Role::Button, enabled, label.clone()));
    resp
}

/// A plain text label with a font and colour.
pub fn label(ui: &mut Ui, text: &str, font: FontId, color: Color32) -> Response {
    ui.add(egui::Label::new(egui::RichText::new(text).font(font).color(color)).wrap())
}

/// Left-aligned single line painted into an allocated rect.
pub fn line(ui: &mut Ui, text: &str, font: FontId, color: Color32) -> Rect {
    let size = text_size(ui, text, &font);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width().max(size.x), size.y), Sense::hover());
    ui.painter().text(pos2(rect.left(), rect.center().y), Align2::LEFT_CENTER, text, font, color);
    describe(&resp, Role::Label, text.trim());
    rect
}
