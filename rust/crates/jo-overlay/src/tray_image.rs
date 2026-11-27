//! The tray badge: a 128×128 circle in Jira blue (alert red) with the count
//! in white Arial Bold, "99+" past 99.

use std::sync::OnceLock;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};

pub const SIZE: u32 = 128;
pub const BLUE: [u8; 3] = [0, 82, 204];
pub const ALERT_RED: [u8; 3] = [220, 80, 60];

/// What the badge says.
pub fn badge_text(count: u64) -> String {
    if count < 100 { count.to_string() } else { "99+".to_owned() }
}

fn font() -> Option<&'static FontVec> {
    static FONT: OnceLock<Option<FontVec>> = OnceLock::new();
    FONT.get_or_init(|| {
        let fonts = std::env::var_os("SystemRoot")
            .map_or_else(|| std::path::PathBuf::from(r"C:\Windows"), std::path::PathBuf::from)
            .join("Fonts");
        ["arialbd.ttf", "arial.ttf", "calibrib.ttf"]
            .iter()
            .find_map(|name| std::fs::read(fonts.join(name)).ok().and_then(|bytes| FontVec::try_from_vec(bytes).ok()))
    })
    .as_ref()
}

/// The badge as straight (non-premultiplied) RGBA, `SIZE`×`SIZE`.
pub fn render(count: u64, alert: bool) -> Vec<u8> {
    let mut pixmap = Pixmap::new(SIZE, SIZE).expect("non-zero size");
    let [r, g, b] = if alert { ALERT_RED } else { BLUE };
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, 255);
    paint.anti_alias = true;
    if let Some(circle) = PathBuilder::from_oval(
        tiny_skia::Rect::from_ltrb(2.0, 2.0, SIZE as f32 - 2.0, SIZE as f32 - 2.0).expect("rect"),
    ) {
        pixmap.fill_path(&circle, &paint, FillRule::Winding, Transform::identity(), None);
    }
    if let Some(font) = font() {
        draw_text(&mut pixmap, font, &badge_text(count));
    }
    pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect()
}

/// White text centred on its ink bounds, 64 px to the em like PIL's
/// `truetype(size=64)`.
fn draw_text(pixmap: &mut Pixmap, font: &FontVec, text: &str) {
    let em = font.units_per_em().unwrap_or(2048.0);
    let scale = PxScale::from(64.0 * font.height_unscaled() / em);
    let scaled = font.as_scaled(scale);
    let mut caret = 0.0_f32;
    let mut previous = None;
    let mut outlines = Vec::new();
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(prev) = previous {
            caret += scaled.kern(prev, id);
        }
        let glyph = id.with_scale_and_position(scale, point(caret, scaled.ascent()));
        caret += scaled.h_advance(id);
        previous = Some(id);
        if let Some(outlined) = font.outline_glyph(glyph) {
            outlines.push(outlined);
        }
    }
    let Some(first) = outlines.first() else { return };
    let mut bounds = first.px_bounds();
    for o in &outlines[1..] {
        let b = o.px_bounds();
        bounds.min.x = bounds.min.x.min(b.min.x);
        bounds.min.y = bounds.min.y.min(b.min.y);
        bounds.max.x = bounds.max.x.max(b.max.x);
        bounds.max.y = bounds.max.y.max(b.max.y);
    }
    let size = SIZE as i32;
    let (tw, th) = ((bounds.max.x - bounds.min.x) as i32, (bounds.max.y - bounds.min.y) as i32);
    let dx = (size - tw).div_euclid(2) - bounds.min.x as i32;
    let dy = (size - th).div_euclid(2) - bounds.min.y as i32;
    let width = SIZE as usize;
    let data = pixmap.data_mut();
    for o in &outlines {
        let b = o.px_bounds();
        o.draw(|x, y, coverage| {
            let px = b.min.x as i32 + x as i32 + dx;
            let py = b.min.y as i32 + y as i32 + dy;
            if !(0..size).contains(&px) || !(0..size).contains(&py) {
                return;
            }
            let i = (py as usize * width + px as usize) * 4;
            let c = coverage.clamp(0.0, 1.0);
            // Premultiplied white "over" whatever is there.
            for channel in &mut data[i..i + 4] {
                *channel = (255.0 * c + f32::from(*channel) * (1.0 - c)).round() as u8;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_renders_for_any_count() {
        for count in [0, 1, 9, 10, 99, 100, 999, 1000, 99999] {
            let rgba = render(count, count > 0);
            assert_eq!(rgba.len(), (SIZE * SIZE * 4) as usize, "{count}");
        }
        assert_eq!(badge_text(99), "99");
        assert_eq!(badge_text(100), "99+");
    }

    #[test]
    fn colours_and_text() {
        let px = |rgba: &[u8], x: u32, y: u32| {
            let i = ((y * SIZE + x) * 4) as usize;
            [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
        };
        let blue = render(0, false);
        assert_eq!(px(&blue, 0, 0)[3], 0, "transparent outside the circle");
        assert_eq!(px(&blue, 20, 64), [0, 82, 204, 255]);
        let red = render(5, true);
        assert_eq!(px(&red, 20, 64), [220, 80, 60, 255]);
        let white = red.chunks(4).filter(|p| p[0] > 250 && p[1] > 250 && p[2] > 250).count();
        assert!(white > 100, "the count is drawn in white ({white} px)");
    }
}
