//! Where the overlay sits: corners of the current monitor's work area,
//! saved positions, snapping and the settings panel's rectangle. All in
//! physical pixels; margins scale with the display.

use jo_core::config::Config;
use serde_json::{Value, json};

use crate::platform::WorkArea;
use crate::util;

pub const CORNERS: [&str; 4] = ["top-left", "top-right", "bottom-left", "bottom-right"];
/// Distance from a corner within which a drag snaps to it.
pub const SNAP_MARGIN: f32 = 60.0;
/// Inset of a snapped window from the work-area edge.
pub const CORNER_INSET: f32 = 10.0;
pub const SETTINGS_W: f32 = 420.0;
pub const SETTINGS_MIN_H: f32 = 320.0;

/// A window rectangle `(x, y, w, h)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// The fallback when no monitor can be found: primary screen minus a taskbar.
pub fn fallback_area(screen: (i32, i32)) -> WorkArea {
    WorkArea { left: 0, top: 0, right: screen.0, bottom: screen.1 - 50 }
}

/// Corner coordinates for a `w`×`h` window in `area`.
pub fn corners(area: WorkArea, w: i32, h: i32, scale: f32) -> [(&'static str, (i32, i32)); 4] {
    let m = (CORNER_INSET * scale).round() as i32;
    [
        ("top-left", (area.left + m, area.top + m)),
        ("top-right", (area.right - w - m, area.top + m)),
        ("bottom-left", (area.left + m, area.bottom - h - m)),
        ("bottom-right", (area.right - w - m, area.bottom - h - m)),
    ]
}

fn corner(corners: &[(&'static str, (i32, i32)); 4], name: &str) -> Option<(i32, i32)> {
    corners.iter().find(|(n, _)| *n == name).map(|(_, p)| *p)
}

/// After a drag: the corner within the snap margin, if any.
pub fn snap_target(corners: &[(&'static str, (i32, i32)); 4], x: i32, y: i32, scale: f32) -> Option<&'static str> {
    let margin = SNAP_MARGIN * scale;
    corners
        .iter()
        .find(|(_, (cx, cy))| ((x - cx).abs() as f32) < margin && ((y - cy).abs() as f32) < margin)
        .map(|(n, _)| *n)
}

/// Double-click: the nearest corner.
pub fn nearest(corners: &[(&'static str, (i32, i32)); 4], x: i32, y: i32) -> (&'static str, (i32, i32)) {
    *corners
        .iter()
        .min_by_key(|(_, (cx, cy))| {
            let (dx, dy) = (i64::from(x - cx), i64::from(y - cy));
            dx * dx + dy * dy
        })
        .expect("four corners")
}

/// Record a drag release in the config: snapped corner, or the position.
pub fn remember_drop(cfg: &mut Config, snapped: Option<&str>, x: i32, y: i32) {
    cfg.shift_remove("lastX");
    cfg.shift_remove("lastY");
    cfg.shift_remove("snapCorner");
    match snapped {
        Some(name) => {
            cfg.insert("snapCorner".into(), json!(name));
        }
        None => {
            cfg.insert("lastX".into(), json!(x));
            cfg.insert("lastY".into(), json!(y));
        }
    }
}

/// Python `_reposition`: saved corner, else saved position, else
/// bottom-right; then clamped onto the monitor it lands on. With no monitor
/// at all it falls back to bottom-right and forgets the saved position.
pub fn reposition(
    cfg: &mut Config,
    current: (i32, i32),
    w: i32,
    h: i32,
    scale: f32,
    screen: (i32, i32),
    work_area: &dyn Fn(i32, i32) -> Option<WorkArea>,
) -> (i32, i32) {
    let here = work_area(current.0 + w / 2, current.1 + h / 2).unwrap_or_else(|| fallback_area(screen));
    let cs = corners(here, w, h, scale);
    let saved_corner = cfg.get("snapCorner").and_then(Value::as_str).and_then(|n| corner(&cs, n));
    let saved_xy = match (cfg.get("lastX"), cfg.get("lastY")) {
        (Some(x), Some(y)) => Some((util::cfg_int_value(x), util::cfg_int_value(y))),
        _ => None,
    };
    let bottom_right = corner(&cs, "bottom-right").expect("corner");
    let (x, y) = saved_corner.or(saved_xy).unwrap_or(bottom_right);
    match work_area(x + w / 2, y + h / 2) {
        Some(m) => (x.min(m.right - w).max(m.left), y.min(m.bottom - h).max(m.top)),
        None => {
            cfg.shift_remove("lastX");
            cfg.shift_remove("lastY");
            cfg.insert("snapCorner".into(), json!("bottom-right"));
            bottom_right
        }
    }
}

/// The settings panel's rectangle: chosen height first, then placed so its
/// right and bottom edges line up with the compact overlay where possible,
/// always within the work area.
pub fn settings_target(compact: Rect, area: WorkArea, scale: f32) -> Rect {
    let tw = (SETTINGS_W * scale).round() as i32;
    let (top_limit, bot_limit) = (area.top + (20.0 * scale) as i32, area.bottom - (10.0 * scale) as i32);
    let th = (((area.bottom - area.top) as f32 * 0.9) as i32).min((bot_limit - top_limit).max(1));
    let th = th.max((SETTINGS_MIN_H * scale).round() as i32);
    let ty = (compact.y + compact.h).min(bot_limit) - th;
    let ty = ty.min(bot_limit - th).max(top_limit);
    let inset = (10.0 * scale) as i32;
    let tx = (compact.x + compact.w - tw).min(area.right - tw - inset).max(area.left + inset);
    Rect { x: tx, y: ty, w: tw, h: th }
}

/// Cubic ease-in-out for the settings expand/collapse.
pub fn ease_in_out(t: f32) -> f32 {
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

/// Cubic ease-out for alpha fades.
pub fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

pub fn lerp_rect(a: Rect, b: Rect, k: f32) -> Rect {
    let mix = |p: i32, q: i32| (p as f32 + (q - p) as f32 * k).round() as i32;
    Rect { x: mix(a.x, b.x), y: mix(a.y, b.y), w: mix(a.w, b.w), h: mix(a.h, b.h) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITOR: WorkArea = WorkArea { left: 0, top: 0, right: 1920, bottom: 1032 };
    const W: i32 = 260;
    const H: i32 = 300;

    fn cfg(v: Value) -> Config {
        v.as_object().unwrap().clone()
    }

    fn place(v: Value, monitor: Option<WorkArea>) -> (i32, i32, Config) {
        let mut c = cfg(v);
        let (x, y) = reposition(&mut c, (100, 100), W, H, 1.0, (1920, 1080), &|_, _| monitor);
        (x, y, c)
    }

    #[test]
    fn the_overlay_always_lands_on_screen() {
        for (label, saved) in [
            ("a saved corner", json!({"snapCorner": "top-left"})),
            ("an unknown corner name", json!({"snapCorner": "nowhere"})),
            ("a saved position", json!({"lastX": 300, "lastY": 400})),
            ("a position far off the right", json!({"lastX": 99999, "lastY": 99999})),
            ("a negative position", json!({"lastX": -5000, "lastY": -5000})),
            ("nothing saved at all", json!({})),
        ] {
            let (x, y, _) = place(saved, Some(MONITOR));
            assert!(x >= 0 && y >= 0 && x + W <= 1920 && y + H <= 1032, "{label}: ({x}, {y})");
        }
        assert_eq!(place(json!({"snapCorner": "top-left"}), Some(MONITOR)).0, 10);
        assert_eq!(place(json!({}), Some(MONITOR)), (1920 - W - 10, 1032 - H - 10, cfg(json!({}))));
    }

    #[test]
    fn an_unplugged_monitor_falls_back_to_a_corner() {
        let (_, _, after) = place(json!({"lastX": 300, "lastY": 400}), None);
        assert_eq!(after.get("snapCorner"), Some(&json!("bottom-right")));
        assert!(!after.contains_key("lastX"));
    }

    #[test]
    fn snapping_and_drops() {
        let cs = corners(MONITOR, W, H, 1.0);
        assert_eq!(snap_target(&cs, 40, 50, 1.0), Some("top-left"));
        assert_eq!(snap_target(&cs, 75, 10, 1.0), None, "beyond 60 px");
        assert_eq!(snap_target(&cs, 90, 15, 1.5), Some("top-left"), "the margin scales with the display");
        assert_eq!(nearest(&cs, 1700, 700).0, "bottom-right");
        let mut c = cfg(json!({"lastX": 1, "lastY": 2}));
        remember_drop(&mut c, Some("top-right"), 0, 0);
        assert_eq!(c, cfg(json!({"snapCorner": "top-right"})));
        remember_drop(&mut c, None, 300, 400);
        assert_eq!(c, cfg(json!({"lastX": 300, "lastY": 400})));
    }

    #[test]
    fn settings_panel_fits_the_work_area() {
        let compact = Rect { x: 1650, y: 900, w: 260, h: 120 };
        let t = settings_target(compact, MONITOR, 1.0);
        assert_eq!((t.w, t.x + t.w), (420, 1910), "right edges aligned, inset from the edge");
        assert!(t.y >= 20 && t.y + t.h <= 1022);
        // Dragged above the top of the screen: never a negative height.
        let t = settings_target(Rect { x: 100, y: -400, w: 260, h: 120 }, MONITOR, 1.0);
        assert!(t.h >= 320 && t.y >= 20 && t.x >= 10);
        let tiny = WorkArea { left: 0, top: 0, right: 800, bottom: 200 };
        assert_eq!(settings_target(compact, tiny, 1.0).h, 320, "never too short to use");
    }

    #[test]
    fn easing() {
        assert_eq!(ease_in_out(0.0), 0.0);
        assert_eq!(ease_in_out(1.0), 1.0);
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-6);
        assert_eq!(ease_out(1.0), 1.0);
        let a = Rect { x: 0, y: 0, w: 100, h: 100 };
        let b = Rect { x: 100, y: 50, w: 300, h: 500 };
        assert_eq!(lerp_rect(a, b, 0.5), Rect { x: 50, y: 25, w: 200, h: 300 });
    }
}
