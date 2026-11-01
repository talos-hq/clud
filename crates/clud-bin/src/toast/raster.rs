//! Rasterize a toast into a semi-transparent PNG for the kitty tier (#1189).
//!
//! The image is a rounded translucent panel with a severity accent bar, the
//! toast text, and a close "X" in its right-hand cells. It is rendered at a
//! nominal cell size and placed with `c=`/`r=`, so the terminal scales it to
//! the real cell grid — clud never needs to know the terminal's pixel size.

use std::io::{self, Cursor};
use std::sync::OnceLock;

use super::Severity;

/// Bundled DejaVu Sans Mono (Bitstream Vera license, see
/// `assets/fonts/DejaVu-LICENSE.txt`). Bundled rather than looked up at
/// runtime so the wheel needs no system font library.
const FONT_BYTES: &[u8] = include_bytes!("../../assets/fonts/DejaVuSansMono.ttf");

/// Nominal pixels per cell. 2x a typical 10x20 cell so the terminal scales
/// down, which keeps text crisp on HiDPI displays.
pub const CELL_W_PX: u32 = 20;
pub const CELL_H_PX: u32 = 40;

/// Toast height in cells.
pub const TOAST_ROWS: u16 = 2;
/// Cells reserved on the right for the close button.
pub const CLOSE_COLS: u16 = 3;
/// Cells before the text: accent bar plus padding.
const LEAD_COLS: u16 = 2;
const MIN_COLS: u16 = 16;
const MAX_COLS: u16 = 72;

const PANEL: [u8; 4] = [22, 24, 30, 214];
const BORDER: [u8; 4] = [255, 255, 255, 40];
const TEXT: [u8; 3] = [236, 238, 242];
const CLOSE: [u8; 3] = [178, 184, 196];

fn font() -> Option<&'static fontdue::Font> {
    static FONT: OnceLock<Option<fontdue::Font>> = OnceLock::new();
    FONT.get_or_init(|| {
        fontdue::Font::from_bytes(FONT_BYTES, fontdue::FontSettings::default()).ok()
    })
    .as_ref()
}

fn accent(severity: Severity) -> [u8; 3] {
    match severity {
        Severity::Info => [96, 165, 250],
        Severity::Warn => [245, 182, 66],
        Severity::Alert => [248, 90, 90],
    }
}

/// Size in cells for `text` on a terminal `term_cols` wide, or `None` when the
/// terminal is too narrow to show a useful toast.
pub fn toast_cells(text: &str, term_cols: u16) -> Option<(u16, u16)> {
    let text_cols = u16::try_from(text.chars().count()).unwrap_or(u16::MAX);
    let wanted = text_cols
        .saturating_add(LEAD_COLS + CLOSE_COLS + 1)
        .clamp(MIN_COLS, MAX_COLS);
    let available = term_cols.saturating_sub(2);
    if available < MIN_COLS {
        return None;
    }
    Some((wanted.min(available), TOAST_ROWS))
}

/// Render the toast as PNG bytes sized `cols` x `rows` nominal cells.
pub fn render_png(text: &str, severity: Severity, cols: u16, rows: u16) -> io::Result<Vec<u8>> {
    let rgba = render_rgba(text, severity, cols, rows);
    let (w, h) = pixel_size(cols, rows);
    let image = image::RgbaImage::from_raw(w, h, rgba)
        .ok_or_else(|| io::Error::other("toast buffer size mismatch"))?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|err| io::Error::other(format!("failed to encode toast PNG: {err}")))?;
    Ok(png)
}

/// Width and height for the transient, non-dismissible CPU HUD.
pub fn cpu_cells(text: &str, term_cols: u16) -> Option<(u16, u16)> {
    const MIN_COLS: u16 = 20;
    const MAX_COLS: u16 = 76;
    let available = term_cols.saturating_sub(2);
    if available < MIN_COLS {
        return None;
    }
    let wanted = u16::try_from(text.chars().count())
        .unwrap_or(u16::MAX)
        .saturating_add(4)
        .clamp(MIN_COLS, MAX_COLS);
    Some((wanted.min(available), TOAST_ROWS))
}

/// Render a CPU panel without a close affordance. The CPU watcher owns its
/// lifetime; only the compositor's timing and hover state affect opacity.
pub fn render_cpu_png(
    text: &str,
    severity: Severity,
    cols: u16,
    rows: u16,
    opacity: u8,
) -> io::Result<Vec<u8>> {
    let rgba = render_cpu_rgba(text, severity, cols, rows, opacity);
    let (w, h) = pixel_size(cols, rows);
    let image = image::RgbaImage::from_raw(w, h, rgba)
        .ok_or_else(|| io::Error::other("cpu panel buffer size mismatch"))?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|err| io::Error::other(format!("failed to encode CPU panel PNG: {err}")))?;
    Ok(png)
}

pub fn pixel_size(cols: u16, rows: u16) -> (u32, u32) {
    (u32::from(cols) * CELL_W_PX, u32::from(rows) * CELL_H_PX)
}

/// Straight-alpha RGBA pixels.
pub fn render_rgba(text: &str, severity: Severity, cols: u16, rows: u16) -> Vec<u8> {
    let (w, h) = pixel_size(cols.max(1), rows.max(1));
    let mut canvas = Canvas::new(w, h);
    let inset = 2.0;
    let radius = (h as f32 * 0.28).min(18.0);
    canvas.rounded_rect(
        inset,
        inset,
        w as f32 - inset,
        h as f32 - inset,
        radius,
        PANEL,
    );
    canvas.rounded_rect_outline(
        inset,
        inset,
        w as f32 - inset,
        h as f32 - inset,
        radius,
        BORDER,
    );

    let bar = accent(severity);
    canvas.rounded_rect(
        inset + 8.0,
        h as f32 * 0.22,
        inset + 14.0,
        h as f32 * 0.78,
        3.0,
        [bar[0], bar[1], bar[2], 255],
    );

    let text_left = (LEAD_COLS as u32 * CELL_W_PX) as f32;
    let close_left = w as f32 - (CLOSE_COLS as u32 * CELL_W_PX) as f32;
    if let Some(font) = font() {
        let px = h as f32 * 0.40;
        let fitted = fit_text(font, text, px, close_left - text_left - 8.0);
        canvas.text(font, &fitted, px, text_left, h as f32 / 2.0, TEXT);
    }

    // Close button: an X centred in the reserved cells.
    let cx = close_left + (CLOSE_COLS as u32 * CELL_W_PX) as f32 / 2.0;
    let cy = h as f32 / 2.0;
    let arm = (h as f32 * 0.16).min(9.0);
    let stroke = 2.4;
    canvas.line(cx - arm, cy - arm, cx + arm, cy + arm, stroke, CLOSE);
    canvas.line(cx - arm, cy + arm, cx + arm, cy - arm, stroke, CLOSE);

    canvas.pixels
}

/// Straight-alpha RGBA pixels for a non-dismissible CPU panel.
pub fn render_cpu_rgba(
    text: &str,
    severity: Severity,
    cols: u16,
    rows: u16,
    opacity: u8,
) -> Vec<u8> {
    let (w, h) = pixel_size(cols.max(1), rows.max(1));
    let mut canvas = Canvas::new(w, h);
    let inset = 2.0;
    let radius = (h as f32 * 0.28).min(18.0);
    canvas.rounded_rect(
        inset,
        inset,
        w as f32 - inset,
        h as f32 - inset,
        radius,
        PANEL,
    );
    canvas.rounded_rect_outline(
        inset,
        inset,
        w as f32 - inset,
        h as f32 - inset,
        radius,
        BORDER,
    );
    let bar = accent(severity);
    canvas.rounded_rect(
        inset + 8.0,
        h as f32 * 0.22,
        inset + 14.0,
        h as f32 * 0.78,
        3.0,
        [bar[0], bar[1], bar[2], 255],
    );
    if let Some(font) = font() {
        let px = h as f32 * 0.40;
        let left = (LEAD_COLS as u32 * CELL_W_PX) as f32;
        let fitted = fit_text(font, text, px, w as f32 - left - CELL_W_PX as f32);
        canvas.text(font, &fitted, px, left, h as f32 / 2.0, TEXT);
    }
    for pixel in canvas.pixels.as_chunks_mut::<4>().0 {
        pixel[3] = ((u16::from(pixel[3]) * u16::from(opacity.min(100)) + 50) / 100) as u8;
    }
    canvas.pixels
}

/// Truncate `text` with an ellipsis so it fits `max_width` pixels.
fn fit_text(font: &fontdue::Font, text: &str, px: f32, max_width: f32) -> String {
    let width = |s: &str| -> f32 { s.chars().map(|c| font.metrics(c, px).advance_width).sum() };
    if width(text) <= max_width {
        return text.to_string();
    }
    let ellipsis = '…';
    let budget = max_width - font.metrics(ellipsis, px).advance_width;
    let mut out = String::new();
    let mut used = 0.0;
    for c in text.chars() {
        let advance = font.metrics(c, px).advance_width;
        if used + advance > budget {
            break;
        }
        used += advance;
        out.push(c);
    }
    out.push(ellipsis);
    out
}

struct Canvas {
    w: u32,
    h: u32,
    pixels: Vec<u8>,
}

impl Canvas {
    fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            pixels: vec![0; (w as usize) * (h as usize) * 4],
        }
    }

    /// Source-over blend of `color` with `coverage` in [0, 1].
    fn blend(&mut self, x: u32, y: u32, color: [u8; 4], coverage: f32) {
        if x >= self.w || y >= self.h || coverage <= 0.0 {
            return;
        }
        let i = ((y * self.w + x) * 4) as usize;
        let sa = (f32::from(color[3]) / 255.0) * coverage.min(1.0);
        let da = f32::from(self.pixels[i + 3]) / 255.0;
        let out_a = sa + da * (1.0 - sa);
        if out_a <= 0.0 {
            return;
        }
        for (channel, &source) in color[..3].iter().enumerate() {
            let sc = f32::from(source) / 255.0;
            let dc = f32::from(self.pixels[i + channel]) / 255.0;
            let oc = (sc * sa + dc * da * (1.0 - sa)) / out_a;
            self.pixels[i + channel] = (oc * 255.0).round() as u8;
        }
        self.pixels[i + 3] = (out_a * 255.0).round() as u8;
    }

    /// Signed distance to a rounded rectangle (negative inside).
    fn rounded_rect_sdf(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> f32 {
        let cx = (x0 + x1) / 2.0;
        let cy = (y0 + y1) / 2.0;
        let hx = (x1 - x0) / 2.0 - r;
        let hy = (y1 - y0) / 2.0 - r;
        let dx = (px - cx).abs() - hx;
        let dy = (py - cy).abs() - hy;
        let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
        outside + dx.max(dy).min(0.0) - r
    }

    fn rounded_rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32, color: [u8; 4]) {
        for y in 0..self.h {
            for x in 0..self.w {
                let d = Self::rounded_rect_sdf(x as f32 + 0.5, y as f32 + 0.5, x0, y0, x1, y1, r);
                let coverage = (0.5 - d).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    self.blend(x, y, color, coverage);
                }
            }
        }
    }

    fn rounded_rect_outline(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32, color: [u8; 4]) {
        for y in 0..self.h {
            for x in 0..self.w {
                let d = Self::rounded_rect_sdf(x as f32 + 0.5, y as f32 + 0.5, x0, y0, x1, y1, r);
                let coverage = (1.0 - d.abs()).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    self.blend(x, y, color, coverage);
                }
            }
        }
    }

    fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: [u8; 3]) {
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len2 = (dx * dx + dy * dy).max(f32::EPSILON);
        let pad = width + 1.0;
        let (min_x, max_x) = (x0.min(x1) - pad, x0.max(x1) + pad);
        let (min_y, max_y) = (y0.min(y1) - pad, y0.max(y1) + pad);
        for y in min_y.max(0.0) as u32..(max_y.max(0.0) as u32).min(self.h) {
            for x in min_x.max(0.0) as u32..(max_x.max(0.0) as u32).min(self.w) {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let t = (((px - x0) * dx + (py - y0) * dy) / len2).clamp(0.0, 1.0);
                let (qx, qy) = (x0 + t * dx, y0 + t * dy);
                let d = ((px - qx).powi(2) + (py - qy).powi(2)).sqrt() - width / 2.0;
                let coverage = (0.5 - d).clamp(0.0, 1.0);
                self.blend(x, y, [color[0], color[1], color[2], 255], coverage);
            }
        }
    }

    fn text(
        &mut self,
        font: &fontdue::Font,
        text: &str,
        px: f32,
        left: f32,
        mid_y: f32,
        color: [u8; 3],
    ) {
        let Some(line) = font.horizontal_line_metrics(px) else {
            return;
        };
        let baseline = mid_y + (line.ascent + line.descent) / 2.0;
        let mut pen = left;
        for c in text.chars() {
            let (metrics, coverage) = font.rasterize(c, px);
            let gx = pen + metrics.xmin as f32;
            let gy = baseline - metrics.height as f32 - metrics.ymin as f32;
            for row in 0..metrics.height {
                for col in 0..metrics.width {
                    let a = coverage[row * metrics.width + col];
                    if a == 0 {
                        continue;
                    }
                    let x = gx + col as f32;
                    let y = gy + row as f32;
                    if x < 0.0 || y < 0.0 {
                        continue;
                    }
                    self.blend(
                        x as u32,
                        y as u32,
                        [color[0], color[1], color[2], 255],
                        f32::from(a) / 255.0,
                    );
                }
            }
            pen += metrics.advance_width;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_font_loads() {
        assert!(font().is_some());
    }

    #[test]
    fn png_decodes_to_the_nominal_cell_size() {
        let png = render_png("cpu 287 % · 2.9 / 12 cores", Severity::Warn, 30, 2).unwrap();
        let image = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), pixel_size(30, 2));
    }

    #[test]
    fn the_panel_is_semi_transparent_and_the_corners_are_clear() {
        let (w, h) = pixel_size(24, 2);
        let rgba = render_rgba("hello", Severity::Info, 24, 2);
        let alpha = |x: u32, y: u32| rgba[((y * w + x) * 4 + 3) as usize];
        assert_eq!(alpha(0, 0), 0, "rounded corner must be transparent");
        let centre = alpha(w / 2, h - 6);
        assert!(
            centre > 120 && centre < 255,
            "panel must be translucent, not opaque: alpha={centre}"
        );
    }

    #[test]
    fn text_and_close_button_draw_bright_pixels() {
        let cols = 24;
        let (w, h) = pixel_size(cols, 2);
        let rgba = render_rgba("MMMM", Severity::Info, cols, 2);
        let bright = |x0: u32, x1: u32| {
            (0..h).any(|y| {
                (x0..x1).any(|x| {
                    let i = ((y * w + x) * 4) as usize;
                    rgba[i] > 150 && rgba[i + 3] > 200
                })
            })
        };
        let text_left = LEAD_COLS as u32 * CELL_W_PX;
        assert!(
            bright(text_left, text_left + 4 * CELL_W_PX),
            "text glyphs missing"
        );
        let close_left = w - CLOSE_COLS as u32 * CELL_W_PX;
        assert!(bright(close_left, w), "close button missing");
    }

    #[test]
    fn severity_changes_the_accent_colour() {
        let info = render_rgba("x", Severity::Info, 16, 2);
        let alert = render_rgba("x", Severity::Alert, 16, 2);
        assert_ne!(info, alert);
    }

    #[test]
    fn long_text_is_truncated_with_an_ellipsis() {
        let font = font().unwrap();
        let fitted = fit_text(font, &"x".repeat(200), 16.0, 120.0);
        assert!(fitted.ends_with('…'));
        assert!(fitted.chars().count() < 200);
        assert_eq!(fit_text(font, "short", 16.0, 500.0), "short");
    }

    #[test]
    fn geometry_is_clamped_and_refuses_narrow_terminals() {
        assert_eq!(toast_cells("hi", 120), Some((MIN_COLS, TOAST_ROWS)));
        let (cols, _) = toast_cells(&"y".repeat(300), 200).unwrap();
        assert_eq!(cols, MAX_COLS);
        let (cols, _) = toast_cells(&"y".repeat(300), 40).unwrap();
        assert_eq!(cols, 38);
        assert_eq!(toast_cells("hi", 10), None);
    }

    #[test]
    fn cpu_hud_keeps_all_metrics_legible_at_eighty_columns() {
        let line = "cpu 287 % · 2.9 / 12 cores · rss 1.42 GiB · 24 procs · 7 m";
        let (cols, rows) = cpu_cells(line, 80).unwrap();
        assert_eq!(rows, TOAST_ROWS);
        let font = font().unwrap();
        let width = f32::from(cols) * CELL_W_PX as f32
            - (LEAD_COLS as u32 * CELL_W_PX) as f32
            - CELL_W_PX as f32;
        assert_eq!(
            fit_text(font, line, rows as f32 * CELL_H_PX as f32 * 0.40, width),
            line
        );
    }

    #[test]
    fn cpu_hud_opacity_scales_the_whole_image_without_a_tiny_text_mode() {
        let at_ninety = render_cpu_rgba("cpu 287 %", Severity::Warn, 30, 2, 90);
        let at_fifty = render_cpu_rgba("cpu 287 %", Severity::Warn, 30, 2, 50);
        assert_eq!(at_ninety.len(), at_fifty.len());
        let max_alpha = |rgba: &[u8]| {
            rgba.as_chunks::<4>()
                .0
                .iter()
                .map(|pixel| pixel[3])
                .max()
                .unwrap()
        };
        assert_eq!(max_alpha(&at_ninety), 230);
        assert_eq!(max_alpha(&at_fifty), 128);
        assert!(at_ninety
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] > 0));
    }
}
