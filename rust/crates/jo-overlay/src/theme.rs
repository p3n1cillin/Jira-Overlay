//! Design tokens, typography and icon glyphs.

use std::sync::Arc;

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId};

/// Colour roles. The dark palette is a deep, low-chroma base that lets one
/// accent carry the interface; the light palette mirrors each role.
pub mod c {
    use std::cell::Cell;

    use eframe::egui::Color32;

    const fn rgb(hex: u32) -> Color32 {
        Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
    }

    /// Every colour role the overlay paints with.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Palette {
        pub bg: Color32,
        pub bg_dark: Color32,
        pub border: Color32,
        pub divider: Color32,
        pub input: Color32,
        pub fg: Color32,
        pub fg_bright: Color32,
        pub fg_dim: Color32,
        pub accent: Color32,
        pub accent_hi: Color32,
        pub alert: Color32,
        pub warn: Color32,
        pub ok: Color32,
        pub info: Color32,
        pub hover: Color32,
        pub chip: Color32,
        pub badge: Color32,
        pub flash: Color32,
        pub done_hover: Color32,
        /// Text on the accent fill (both palettes).
        pub white: Color32,
    }

    pub const DARK: Palette = Palette {
        bg: rgb(0x11151d),
        bg_dark: rgb(0x0c0f15),
        border: rgb(0x232c39),
        divider: rgb(0x1b222c),
        input: rgb(0x1a212b),
        fg: rgb(0x98a4b6),
        fg_bright: rgb(0xf2f5fa),
        fg_dim: rgb(0x5c6675),
        accent: rgb(0x4d9dff),
        accent_hi: rgb(0x74b4ff),
        alert: rgb(0xff6363),
        warn: rgb(0xf7c065),
        ok: rgb(0x41d392),
        info: rgb(0x5fc8e8),
        hover: rgb(0x1e2733),
        chip: rgb(0x26313f),
        badge: rgb(0x212a36),
        flash: rgb(0x28405e),
        done_hover: rgb(0x16281f),
        white: Color32::WHITE,
    };

    /// The same roles on a light base; status colours are darkened so they
    /// keep their contrast against white.
    pub const LIGHT: Palette = Palette {
        bg: rgb(0xf7f8fa),
        bg_dark: rgb(0xeef1f5),
        border: rgb(0xcfd6e0),
        divider: rgb(0xe1e6ec),
        input: rgb(0xebeef3),
        fg: rgb(0x46505f),
        fg_bright: rgb(0x111722),
        fg_dim: rgb(0x7d8796),
        accent: rgb(0x1f6fe5),
        accent_hi: rgb(0x3d86f0),
        alert: rgb(0xd23a3a),
        warn: rgb(0xa86a08),
        ok: rgb(0x16895a),
        info: rgb(0x0b7fa3),
        hover: rgb(0xe6ebf1),
        chip: rgb(0xdbe2eb),
        badge: rgb(0xe2e7ee),
        flash: rgb(0xcde0fa),
        done_hover: rgb(0xe0f2e9),
        white: Color32::WHITE,
    };

    thread_local! {
        // Per thread: the UI thread paints, and parallel tests never see
        // each other's choice.
        static LIGHT_ON: Cell<bool> = const { Cell::new(false) };
    }

    pub fn set_light(light: bool) {
        LIGHT_ON.with(|l| l.set(light));
    }

    pub fn is_light() -> bool {
        LIGHT_ON.with(Cell::get)
    }

    pub fn palette() -> &'static Palette {
        if is_light() { &LIGHT } else { &DARK }
    }

    pub fn bg() -> Color32 {
        palette().bg
    }
    pub fn bg_dark() -> Color32 {
        palette().bg_dark
    }
    pub fn border() -> Color32 {
        palette().border
    }
    pub fn divider() -> Color32 {
        palette().divider
    }
    pub fn input() -> Color32 {
        palette().input
    }
    pub fn fg() -> Color32 {
        palette().fg
    }
    pub fn fg_bright() -> Color32 {
        palette().fg_bright
    }
    pub fn fg_dim() -> Color32 {
        palette().fg_dim
    }
    pub fn accent() -> Color32 {
        palette().accent
    }
    pub fn accent_hi() -> Color32 {
        palette().accent_hi
    }
    pub fn alert() -> Color32 {
        palette().alert
    }
    pub fn warn() -> Color32 {
        palette().warn
    }
    pub fn ok() -> Color32 {
        palette().ok
    }
    pub fn info() -> Color32 {
        palette().info
    }
    pub fn hover() -> Color32 {
        palette().hover
    }
    pub fn chip() -> Color32 {
        palette().chip
    }
    pub fn badge() -> Color32 {
        palette().badge
    }
    pub fn flash() -> Color32 {
        palette().flash
    }
    pub fn done_hover() -> Color32 {
        palette().done_hover
    }
    pub fn white() -> Color32 {
        palette().white
    }
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

/// The first of `names` found in the Windows font folder, mapped read-only.
pub fn system_font(names: &[&str]) -> Option<&'static [u8]> {
    let dir = std::env::var_os("SystemRoot")
        .map_or_else(|| std::path::PathBuf::from(r"C:\Windows"), std::path::PathBuf::from)
        .join("Fonts");
    names.iter().find_map(|n| mapped_file(&dir.join(n)))
}

/// A file's bytes for the life of the process, memory-mapped read-only
/// rather than copied: font files are megabytes each, and mapped pages are
/// shared with every other process using the font instead of counting as
/// this process's private memory. Each path is mapped once. Falls back to
/// reading (and leaking) a copy if mapping fails.
pub fn mapped_file(path: &std::path::Path) -> Option<&'static [u8]> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static MAPPED: Mutex<Option<HashMap<std::path::PathBuf, &'static [u8]>>> = Mutex::new(None);
    let mut cache = MAPPED.lock().unwrap_or_else(|p| p.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some(bytes) = cache.get(path) {
        return Some(bytes);
    }
    let bytes = map_read_only(path).or_else(|| std::fs::read(path).ok().map(|v| &*Box::leak(v.into_boxed_slice())))?;
    cache.insert(path.to_path_buf(), bytes);
    Some(bytes)
}

fn map_read_only(path: &std::path::Path) -> Option<&'static [u8]> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_READ, MapViewOfFile, PAGE_READONLY};
    let file = std::fs::File::open(path).ok()?;
    let len = usize::try_from(file.metadata().ok()?.len()).ok().filter(|n| *n > 0)?;
    // SAFETY: the mapping and view are created from a file we hold open;
    // the view is never unmapped, so the slice stays valid for the life of
    // the process. Windows refuses to truncate a file while a view of it
    // exists, and system fonts are not writable by this user.
    unsafe {
        let mapping = CreateFileMappingW(HANDLE(file.as_raw_handle()), None, PAGE_READONLY, 0, 0, None).ok()?;
        let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
        let _ = CloseHandle(mapping); // the view keeps the section alive
        if view.Value.is_null() {
            return None;
        }
        Some(std::slice::from_raw_parts(view.Value.cast::<u8>(), len))
    }
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
                fonts.font_data.insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
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

/// Visuals matching the active palette, applied to every viewport.
pub fn install_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        *v = if c::is_light() { egui::Visuals::light() } else { egui::Visuals::dark() };
        v.panel_fill = c::bg();
        v.window_fill = c::bg();
        v.extreme_bg_color = c::input();
        v.faint_bg_color = c::input();
        v.override_text_color = None;
        v.selection.bg_fill = c::accent();
        v.hyperlink_color = c::accent();
        for w in [&mut v.widgets.inactive, &mut v.widgets.noninteractive] {
            w.bg_fill = c::input();
            w.weak_bg_fill = c::input();
            w.fg_stroke.color = c::fg();
            w.bg_stroke = egui::Stroke::NONE;
        }
        v.widgets.noninteractive.bg_fill = c::bg();
        v.widgets.noninteractive.weak_bg_fill = c::bg();
        v.widgets.hovered.bg_fill = c::chip();
        v.widgets.hovered.weak_bg_fill = c::chip();
        v.widgets.hovered.fg_stroke.color = c::fg_bright();
        v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
        v.widgets.active.bg_fill = c::accent();
        v.widgets.active.weak_bg_fill = c::accent();
        v.widgets.active.fg_stroke.color = c::white();
        v.widgets.open.bg_fill = c::chip();
        style.spacing.item_spacing = egui::vec2(6.0, 2.0);
        style.interaction.tooltip_delay = 0.6;
        style.text_styles.insert(egui::TextStyle::Body, regular(9.0));
        style.text_styles.insert(egui::TextStyle::Button, regular(9.0));
        style.text_styles.insert(egui::TextStyle::Small, regular(8.0));
    });
}

/// Theme setting choices: `theme` in the config (absent means "System").
pub const THEME_CHOICES: [&str; 3] = ["System", "Dark", "Light"];

/// The configured theme, normalised to one of [`THEME_CHOICES`].
pub fn theme_choice(cfg: &jo_core::config::Config) -> &'static str {
    let raw = jo_core::config::get_str(cfg, "theme", "System");
    THEME_CHOICES.iter().find(|t| t.eq_ignore_ascii_case(raw.trim())).copied().unwrap_or("System")
}

/// Windows' app mode: `Some(true)` for light, `None` when unreadable.
pub fn system_prefers_light() -> Option<bool> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
        .ok()
        .map(|v| v != 0)
}

/// Whether to paint with the light palette. `system` is only asked for
/// "System"; an unreadable registry means dark, the original look.
pub fn wants_light(choice: &str, system: impl FnOnce() -> Option<bool>) -> bool {
    match choice {
        "Light" => true,
        "Dark" => false,
        _ => system().unwrap_or(false),
    }
}

/// Switch palette (this thread) and restyle `ctx`; true when it changed.
pub fn apply_palette(ctx: &egui::Context, light: bool) -> bool {
    if c::is_light() == light {
        return false;
    }
    c::set_light(light);
    install_style(ctx);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lerp_and_track() {
        assert_eq!(lerp(c::flash(), c::bg(), 0.0), c::flash());
        assert_eq!(lerp(c::flash(), c::bg(), 1.0), c::bg());
        assert_eq!(track("AB"), "A\u{2009}B");
        assert_eq!(Icon::Settings.glyph(false), "⚙");
    }

    fn luminance(c: Color32) -> f64 {
        let lin = |v: u8| {
            let s = f64::from(v) / 255.0;
            if s <= 0.03928 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn both_palettes_keep_text_readable() {
        for (name, p) in [("dark", c::DARK), ("light", c::LIGHT)] {
            assert!(contrast(p.fg_bright, p.bg) >= 7.0, "{name}: headline text");
            assert!(contrast(p.fg, p.bg) >= 4.5, "{name}: body text");
            assert!(contrast(p.fg_dim, p.bg) >= 3.0, "{name}: dim text");
            for (role, colour) in [("accent", p.accent), ("ok", p.ok), ("warn", p.warn), ("alert", p.alert)] {
                assert!(contrast(colour, p.bg) >= 3.0, "{name}: {role} against the background");
            }
            // The original dark accent carries white bold labels at ~2.7:1.
            assert!(contrast(p.white, p.accent) >= 2.5, "{name}: text on the accent");
        }
        assert!(luminance(c::LIGHT.bg) > luminance(c::LIGHT.fg) && luminance(c::DARK.bg) < luminance(c::DARK.fg));
    }

    #[test]
    fn system_fonts_are_mapped_once() {
        let a = system_font(&["nope-not-a-font.ttf", "segoeui.ttf", "arial.ttf"]).expect("a Windows UI font");
        let b = system_font(&["segoeui.ttf", "arial.ttf"]).unwrap();
        assert_eq!(a.as_ptr(), b.as_ptr(), "the same mapping is reused");
        assert!(a.len() > 1000 && (&a[..4] == b"\0\x01\0\0" || &a[..4] == b"OTTO" || &a[..4] == b"true"));
        let dir = tempfile::tempdir().unwrap();
        assert!(mapped_file(&dir.path().join("missing.ttf")).is_none());
        let empty = dir.path().join("empty.ttf");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(mapped_file(&empty), Some(&[][..]), "an empty file falls back to a (zero-length) copy");
    }

    #[test]
    fn theme_choice_and_system_mode() {
        let cfg = |v: serde_json::Value| v.as_object().unwrap().clone();
        assert_eq!(theme_choice(&cfg(serde_json::json!({}))), "System");
        assert_eq!(theme_choice(&cfg(serde_json::json!({"theme": "light"}))), "Light");
        assert_eq!(theme_choice(&cfg(serde_json::json!({"theme": "Neon"}))), "System");
        assert_eq!(theme_choice(&cfg(serde_json::json!({"theme": 3}))), "System");
        assert!(wants_light("Light", || panic!("not asked")));
        assert!(!wants_light("Dark", || panic!("not asked")));
        assert!(wants_light("System", || Some(true)));
        assert!(!wants_light("System", || None), "unreadable registry keeps the dark look");
        // The real registry read never panics, whatever the machine has.
        let _ = system_prefers_light();

        let ctx = egui::Context::default();
        assert!(apply_palette(&ctx, true) && c::is_light() && c::bg() == c::LIGHT.bg);
        assert!(!apply_palette(&ctx, true), "no change, no restyle");
        assert!(apply_palette(&ctx, false) && c::bg() == c::DARK.bg);
    }
}
