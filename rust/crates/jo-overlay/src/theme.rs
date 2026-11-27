//! Design tokens, typography and icon glyphs.

use std::sync::Arc;

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId};

/// A deep, low-chroma base lets one accent carry the interface.
pub mod c {
    use eframe::egui::Color32;
    pub const BG: Color32 = Color32::from_rgb(0x11, 0x15, 0x1d);
    pub const BG_DARK: Color32 = Color32::from_rgb(0x0c, 0x0f, 0x15);
    pub const BORDER: Color32 = Color32::from_rgb(0x23, 0x2c, 0x39);
    pub const DIVIDER: Color32 = Color32::from_rgb(0x1b, 0x22, 0x2c);
    pub const INPUT: Color32 = Color32::from_rgb(0x1a, 0x21, 0x2b);
    pub const FG: Color32 = Color32::from_rgb(0x98, 0xa4, 0xb6);
    pub const FG_BRIGHT: Color32 = Color32::from_rgb(0xf2, 0xf5, 0xfa);
    pub const FG_DIM: Color32 = Color32::from_rgb(0x5c, 0x66, 0x75);
    pub const ACCENT: Color32 = Color32::from_rgb(0x4d, 0x9d, 0xff);
    pub const ACCENT_HI: Color32 = Color32::from_rgb(0x74, 0xb4, 0xff);
    pub const ALERT: Color32 = Color32::from_rgb(0xff, 0x63, 0x63);
    pub const WARN: Color32 = Color32::from_rgb(0xf7, 0xc0, 0x65);
    pub const OK: Color32 = Color32::from_rgb(0x41, 0xd3, 0x92);
    pub const INFO: Color32 = Color32::from_rgb(0x5f, 0xc8, 0xe8);
    pub const HOVER: Color32 = Color32::from_rgb(0x1e, 0x27, 0x33);
    pub const CHIP: Color32 = Color32::from_rgb(0x26, 0x31, 0x3f);
    pub const BADGE: Color32 = Color32::from_rgb(0x21, 0x2a, 0x36);
    pub const FLASH: Color32 = Color32::from_rgb(0x28, 0x40, 0x5e);
    pub const DONE_HOVER: Color32 = Color32::from_rgb(0x16, 0x28, 0x1f);
    pub const WHITE: Color32 = Color32::WHITE;
}

/// Linear-interpolate two opaque colours (t in 0..1), truncating like the
/// Python `_lerp_hex`.
pub fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |p: u8, q: u8| (f32::from(p) + (f32::from(q) - f32::from(p)) * t) as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

/// Tk point sizes on a 96-DPI display, in egui points.
pub fn pt(size: f32) -> f32 {
    size * 96.0 / 72.0
}

pub fn regular(size: f32) -> FontId {
    FontId::new(pt(size), FontFamily::Proportional)
}

pub fn bold(size: f32) -> FontId {
    FontId::new(pt(size), FontFamily::Name("bold".into()))
}

pub fn icon_font(size: f32) -> FontId {
    FontId::new(pt(size), FontFamily::Name("icons".into()))
}

/// Letter-space a short label with thin spaces (Python `_track`).
pub fn track(text: &str) -> String {
    text.chars().map(String::from).collect::<Vec<_>>().join("\u{2009}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Refresh,
    Snooze,
    Hide,
    Close,
    Settings,
}

impl Icon {
    /// Segoe Fluent Icons / MDL2 code point, else a plain-text stand-in.
    pub fn glyph(self, icon_font: bool) -> &'static str {
        match (self, icon_font) {
            (Icon::Refresh, true) => "\u{E72C}",
            (Icon::Snooze, true) => "\u{E708}",
            (Icon::Hide, true) => "\u{E921}",
            (Icon::Close, true) => "\u{E8BB}",
            (Icon::Settings, true) => "\u{E713}",
            (Icon::Refresh, false) => "↻",
            (Icon::Snooze, false) => "☾",
            (Icon::Hide, false) => "—",
            (Icon::Close, false) => "✕",
            (Icon::Settings, false) => "⚙",
        }
    }
}

fn system_font(names: &[&str]) -> Option<Vec<u8>> {
    let dir = std::env::var_os("SystemRoot")
        .map_or_else(|| std::path::PathBuf::from(r"C:\Windows"), std::path::PathBuf::from)
        .join("Fonts");
    names.iter().find_map(|n| std::fs::read(dir.join(n)).ok())
}

/// Install Segoe UI (regular/bold), Segoe UI Symbol and the Windows icon
/// font, keeping egui's bundled fonts as fallbacks. Returns whether an icon
/// font was found.
pub fn install_fonts(ctx: &egui::Context) -> bool {
    let mut fonts = FontDefinitions::default();
    let defaults = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut add = |name: &str, files: &[&str]| -> bool {
        match system_font(files) {
            Some(bytes) => {
                fonts.font_data.insert(name.to_owned(), Arc::new(FontData::from_owned(bytes)));
                true
            }
            None => false,
        }
    };
    let regular = add("segoe", &["segoeui.ttf"]);
    let bold = add("segoe-bold", &["segoeuib.ttf", "segoeuisb.ttf"]);
    let symbol = add("segoe-symbol", &["seguisym.ttf"]);
    let icons = add("icons", &["SegoeIcons.ttf", "segmdl2.ttf"]);
    let chain = |first: Option<&str>| {
        let mut list: Vec<String> = first.map(str::to_owned).into_iter().collect();
        if symbol {
            list.push("segoe-symbol".into());
        }
        list.extend(defaults.iter().cloned());
        list
    };
    let body = chain(regular.then_some("segoe"));
    let heavy = chain(if bold {
        Some("segoe-bold")
    } else if regular {
        Some("segoe")
    } else {
        None
    });
    let mut icon_chain: Vec<String> = icons.then(|| "icons".to_owned()).into_iter().collect();
    icon_chain.extend(body.iter().cloned());
    fonts.families.insert(FontFamily::Proportional, body);
    fonts.families.insert(FontFamily::Name("bold".into()), heavy);
    fonts.families.insert(FontFamily::Name("icons".into()), icon_chain);
    ctx.set_fonts(fonts);
    icons
}

/// Dark visuals matching the palette, applied to every viewport.
pub fn install_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = c::BG;
        v.window_fill = c::BG;
        v.extreme_bg_color = c::INPUT;
        v.faint_bg_color = c::INPUT;
        v.override_text_color = None;
        v.selection.bg_fill = c::ACCENT;
        v.hyperlink_color = c::ACCENT;
        for w in [&mut v.widgets.inactive, &mut v.widgets.noninteractive] {
            w.bg_fill = c::INPUT;
            w.weak_bg_fill = c::INPUT;
            w.fg_stroke.color = c::FG;
            w.bg_stroke = egui::Stroke::NONE;
        }
        v.widgets.noninteractive.bg_fill = c::BG;
        v.widgets.noninteractive.weak_bg_fill = c::BG;
        v.widgets.hovered.bg_fill = c::CHIP;
        v.widgets.hovered.weak_bg_fill = c::CHIP;
        v.widgets.hovered.fg_stroke.color = c::FG_BRIGHT;
        v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
        v.widgets.active.bg_fill = c::ACCENT;
        v.widgets.active.weak_bg_fill = c::ACCENT;
        v.widgets.active.fg_stroke.color = c::WHITE;
        v.widgets.open.bg_fill = c::CHIP;
        style.spacing.item_spacing = egui::vec2(6.0, 2.0);
        style.interaction.tooltip_delay = 0.6;
        style.text_styles.insert(egui::TextStyle::Body, regular(9.0));
        style.text_styles.insert(egui::TextStyle::Button, regular(9.0));
        style.text_styles.insert(egui::TextStyle::Small, regular(8.0));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lerp_and_track() {
        assert_eq!(lerp(c::FLASH, c::BG, 0.0), c::FLASH);
        assert_eq!(lerp(c::FLASH, c::BG, 1.0), c::BG);
        assert_eq!(track("AB"), "A\u{2009}B");
        assert_eq!(Icon::Settings.glyph(false), "⚙");
    }
}
