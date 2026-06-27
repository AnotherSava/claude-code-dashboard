//! Renders a usage-limit indicator on the tray icon and keeps the tray tooltip
//! showing both buckets. Two indicator styles (per the `TrayBadge` mode):
//!
//! - **Light**: recolor the app icon's three traffic lights by usage, stepping
//!   green → amber → red as the bucket fills (`render_light_badge`).
//! - **Number**: draw the percentage as a number, rasterized from a real font
//!   (Bebas Neue, bundled, OFL) directly at the OS tray pixel size — anti-aliased
//!   and shown ~1:1 rather than an upscaled bitmap or an OS-blurred oversized
//!   source. A maxed (>=100%) bucket shows the all-red light instead.
//!
//! `refresh` is the single entry point, called from the usage poll chokepoint
//! (`commands::emit_usage_limits_updated`), the tray submenu, and the config
//! watcher.

use std::sync::OnceLock;

use fontdue::Font;
use tauri::image::Image;
use tauri::{AppHandle, Manager};

use crate::config::{ConfigState, TrayBadge};
use crate::notifications::context_percent;
use crate::state::AppState;
use crate::usage_limits::{UsageLimits, UsageLimitsState, UsageStatus};

/// Red used for the context-over-threshold alert: the icon-mode border stroke
/// and the number-mode background fill.
const ALERT_BORDER_COLOR: [u8; 3] = [220, 38, 38];

/// Digit color drawn over the number-mode alert background — white for contrast
/// against the red fill.
const ALERT_TEXT_COLOR: [u8; 3] = [255, 255, 255];

/// App-icon body geometry as fractions of the rendered size, measured from the
/// 512px source icon (opaque content box x[64,447] y[10,501], corner r≈36): a
/// centered *portrait* rounded rect. The alert border is a rounded-rect stroke
/// matched to this outline, [`ALERT_OUTSET`] outside it.
const ICON_HALF_W: f32 = 0.374;
const ICON_HALF_H: f32 = 0.480;
const ICON_RADIUS: f32 = 0.070;
const ALERT_OUTSET: f32 = 0.020;

/// Bundled badge font — Bebas Neue (SIL Open Font License; see
/// `assets/fonts/OFL.txt`). Tall and condensed so two digits reach full height
/// without being shrunk to fit the icon width.
const FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/BebasNeue-Regular.ttf");

/// Every digit the badge can show — used to size the font by the *tallest*
/// glyph so the digit height is consistent across all numbers.
const HEIGHT_REFS: [char; 10] = ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9'];
/// Widest two-digit strings — used to size the font so the worst-case width
/// still fits, independent of the number actually shown.
const WIDTH_REFS: [&str; 2] = ["88", "00"];

fn font() -> &'static Font {
    static FONT: OnceLock<Font> = OnceLock::new();
    FONT.get_or_init(|| {
        Font::from_bytes(FONT_BYTES, fontdue::FontSettings::default())
            .expect("embedded Bebas Neue is a valid font")
    })
}

/// Box/area downscale of an RGBA buffer to `dw`x`dh`, alpha-weighted so
/// transparent regions don't darken the result. Used to fit the source app
/// icon to the exact tray pixel size — doing the downscale ourselves yields a
/// cleaner result than handing the OS an oversized bitmap to blur down.
fn area_downscale(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let mut out = vec![0u8; dw * dh * 4];
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return out;
    }
    for dy in 0..dh {
        let sy0 = dy * sh / dh;
        let sy1 = (((dy + 1) * sh / dh).max(sy0 + 1)).min(sh);
        for dx in 0..dw {
            let sx0 = dx * sw / dw;
            let sx1 = (((dx + 1) * sw / dw).max(sx0 + 1)).min(sw);
            let (mut r, mut g, mut b, mut a, mut count) = (0f32, 0f32, 0f32, 0f32, 0f32);
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let p = (sy * sw + sx) * 4;
                    let af = src[p + 3] as f32 / 255.0;
                    r += src[p] as f32 * af;
                    g += src[p + 1] as f32 * af;
                    b += src[p + 2] as f32 * af;
                    a += src[p + 3] as f32;
                    count += 1.0;
                }
            }
            let dp = (dy * dw + dx) * 4;
            if a > 0.0 {
                let wsum = a / 255.0;
                out[dp] = (r / wsum).round().clamp(0.0, 255.0) as u8;
                out[dp + 1] = (g / wsum).round().clamp(0.0, 255.0) as u8;
                out[dp + 2] = (b / wsum).round().clamp(0.0, 255.0) as u8;
                out[dp + 3] = (a / count).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// Pixel size to render the tray bitmap at, matching what the OS displays so
/// it's shown ~1:1 (crisp) instead of being downscaled from an oversized image.
/// Windows: 16 logical px * DPI scale (16/24/32 at 100/150/200%). macOS: the
/// `tray-icon` crate always `setSize`s the menu-bar image to 18pt tall, so the
/// useful resolution is 18pt * backing scale (36px on a 2x Retina display).
fn target_icon_px(app: &AppHandle) -> usize {
    let scale = app
        .get_webview_window("main")
        .and_then(|w| w.scale_factor().ok())
        .unwrap_or(1.0);
    #[cfg(target_os = "macos")]
    let base = 18.0_f64;
    #[cfg(not(target_os = "macos"))]
    let base = 16.0_f64;
    ((base * scale).round() as usize).clamp(16, 64)
}

/// Pick a number color by urgency so the badge conveys severity at a glance,
/// matching the green→amber→red progression of the in-app bars.
fn urgency_color(pct: u32) -> [u8; 3] {
    if pct >= 85 {
        [255, 90, 90] // red
    } else if pct >= 60 {
        [240, 200, 70] // amber
    } else {
        [90, 210, 120] // green
    }
}

/// Baseline-relative bounding box (minx, miny, maxx, maxy) of `text` laid out
/// at `px`, with y growing downward and the baseline at y=0. Uses metrics only
/// (no rasterization), so it's cheap to call while searching for a fit.
fn text_bounds(font: &Font, text: &str, px: f32) -> (i32, i32, i32, i32) {
    let (mut minx, mut miny, mut maxx, mut maxy) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    let mut pen = 0.0f32;
    for ch in text.chars() {
        let m = font.metrics(ch, px);
        let gx = (pen + m.xmin as f32).round() as i32;
        let gy = -(m.ymin + m.height as i32);
        minx = minx.min(gx);
        maxx = maxx.max(gx + m.width as i32);
        miny = miny.min(gy);
        maxy = maxy.max(gy + m.height as i32);
        pen += m.advance_width;
    }
    if minx > maxx {
        (0, 0, 0, 0)
    } else {
        (minx, miny, maxx, maxy)
    }
}

/// Alpha-over composite of (`rgb`, coverage `a`) onto one RGBA pixel slice.
fn over(dst: &mut [u8], rgb: [u8; 3], a: u8) {
    if a == 0 {
        return;
    }
    let sa = a as f32 / 255.0;
    let da = dst[3] as f32 / 255.0;
    let out_a = sa + da * (1.0 - sa);
    if out_a <= 0.0 {
        return;
    }
    for k in 0..3 {
        let v = (rgb[k] as f32 * sa + dst[k] as f32 * da * (1.0 - sa)) / out_a;
        dst[k] = v.round().clamp(0.0, 255.0) as u8;
    }
    dst[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

/// Rasterize a coverage mask (0..255 per pixel, `size`x`size`) into a `color`ed
/// badge on a fully transparent background — only the digits show, the menu bar
/// fills everything else, and the anti-aliased edges fade to transparent
/// (alpha = coverage). Built directly at `size` (the OS tray pixel size) so it
/// displays ~1:1 instead of being resampled by the OS.
fn compose_badge(cov: &[u8], color: [u8; 3], size: usize) -> Image<'static> {
    let w = size;
    let h = size;

    // Transparent background — no plate. The colored mark is drawn straight onto
    // empty pixels, so only the digits carry alpha.
    let mut buf = vec![0u8; w * h * 4];
    for i in 0..w * h {
        let p = i * 4;
        over(&mut buf[p..p + 4], color, cov[i]);
    }

    Image::new_owned(buf, w as u32, h as u32)
}

/// Anti-aliased coverage mask of `text` (1-2 digits) rasterized from the
/// bundled condensed font, sized to fill a `size`x`size` icon.
fn text_coverage(text: &str, size: usize) -> Vec<u8> {
    let w = size;
    let h = size;
    let font = font();
    // Leave 1px around the text for the outline.
    let avail = (w.min(h).saturating_sub(2)).max(1) as i32;
    // Size from text-INDEPENDENT references so every number renders at the same
    // digit height: a narrow "15" must not grow taller than a wide "82" just
    // because it leaves more width to spare. Constrain by the tallest glyph and
    // the widest two-character string. (Bounds grow monotonically with px.)
    let mut px = 4.0f32;
    let mut s = 4.0f32;
    while s <= size as f32 * 4.0 {
        let tall = HEIGHT_REFS.iter().map(|&c| font.metrics(c, s).height as i32).max().unwrap_or(0);
        let wide = WIDTH_REFS
            .iter()
            .map(|t| { let (a, _, b, _) = text_bounds(font, t, s); b - a })
            .max()
            .unwrap_or(0);
        if tall <= avail && wide <= avail {
            px = s;
            s += 1.0;
        } else {
            break;
        }
    }

    // Fixed baseline from the reference figure height, so all numbers sit at the
    // same vertical position regardless of which digits they contain.
    let ref_h = HEIGHT_REFS.iter().map(|&c| font.metrics(c, px).height as i32).max().unwrap_or(0);
    let baseline = (h as i32 + ref_h) / 2;
    // Horizontal centering uses the actual text width.
    let (minx, _, maxx, _) = text_bounds(font, text, px);
    let off_x = (w as i32 - (maxx - minx)) / 2 - minx;

    let mut cov = vec![0u8; w * h];
    let mut pen = 0.0f32;
    for ch in text.chars() {
        let (m, bitmap) = font.rasterize(ch, px);
        let gx = (pen + m.xmin as f32).round() as i32 + off_x;
        let gy = baseline - m.ymin - m.height as i32;
        for row in 0..m.height {
            for col in 0..m.width {
                let c = bitmap[row * m.width + col];
                if c == 0 {
                    continue;
                }
                let x = gx + col as i32;
                let y = gy + row as i32;
                if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                    let idx = y as usize * w + x as usize;
                    if c > cov[idx] {
                        cov[idx] = c;
                    }
                }
            }
        }
        pen += m.advance_width;
    }
    cov
}

/// Render a 1-2 digit number badge on a black plate.
fn render_badge(text: &str, color: [u8; 3], size: usize) -> Image<'static> {
    compose_badge(&text_coverage(text, size), color, size)
}

/// Traffic-light colors. Green and red are sampled from the app icon's lights;
/// the amber is shifted toward gold (the icon's native amber is too orange and
/// reads too close to red at tray size). Indexed by level: 0=green, 1=amber,
/// 2=red.
const LEVEL_COLORS: [[u8; 3]; 3] = [[62, 182, 80], [245, 176, 48], [240, 79, 72]];

/// Per-state color level of each light, indexed `[state][band]` where band is
/// 0=top, 1=middle, 2=bottom. Reading bottom→top the 7 states progress
/// GGG → GGY → GYY → YYY → YYR → YRR → RRR as usage climbs: the top light
/// escalates first, then the middle, then the bottom.
const LIGHT_LEVELS: [[u8; 3]; 7] = [
    [0, 0, 0], // GGG
    [1, 0, 0], // GGY
    [1, 1, 0], // GYY
    [1, 1, 1], // YYY
    [2, 1, 1], // YYR
    [2, 2, 1], // YRR
    [2, 2, 2], // RRR
];

/// Map a percentage to a traffic-light state: 0..=99 split into 6 equal
/// intervals (states 0-5), and 100% to the all-red state 6.
fn light_state(pct: u32) -> usize {
    (pct * 6 / 100).min(6) as usize
}

/// Draw the red context-usage alert border onto a `size`x`size` RGBA buffer: a
/// rounded-rect stroke matched to the app icon's outline (see [`ICON_HALF_W`])
/// and outset slightly, with the top/bottom segments 1px thinner than the sides
/// (the icon is portrait, so an equal-thickness frame reads heavy top/bottom).
/// A cheap parametric SDF pass — no per-pixel neighborhood scan. Composited over
/// whatever the badge already drew.
fn draw_alert_border(buf: &mut [u8], size: usize) {
    let n = size as f32;
    let c = n / 2.0;
    let half_w = (ICON_HALF_W + ALERT_OUTSET) * n;
    let half_h = (ICON_HALF_H + ALERT_OUTSET) * n;
    let radius = ICON_RADIUS * n;
    let thick_lr = (0.09 * n).max(1.5);
    let thick_tb = (thick_lr - 1.0).max(0.5);
    let inner_w = half_w - thick_lr;
    let inner_h = half_h - thick_tb;
    let inner_r = (radius - thick_lr.min(thick_tb)).max(0.0);
    for y in 0..size {
        for x in 0..size {
            let px = x as f32 + 0.5 - c;
            let py = y as f32 + 0.5 - c;
            let d_out = rounded_rect_sdf(px, py, half_w, half_h, radius);
            let d_in = rounded_rect_sdf(px, py, inner_w, inner_h, inner_r);
            // Inside the outer outline AND outside the inner one = the ring,
            // with ~1px anti-aliasing on each boundary.
            let cov = (0.5 - d_out).clamp(0.0, 1.0).min((0.5 + d_in).clamp(0.0, 1.0));
            if cov > 0.0 {
                let p = (y * size + x) * 4;
                over(&mut buf[p..p + 4], ALERT_BORDER_COLOR, (cov * 255.0).round() as u8);
            }
        }
    }
}

/// Render the traffic-light badge: recolor the app icon's three lights to the
/// state for `pct`, keeping the housing. The `*Number` modes reuse this for the
/// all-red 100% state. When `alert` is set, the icon's white outer border is
/// recolored red as the context-usage warning. Drawn at the OS tray pixel size.
fn render_light_badge(base: &Image, pct: u32, size: usize, alert: bool) -> Image<'static> {
    let levels = LIGHT_LEVELS[light_state(pct)];
    let bw = base.width() as usize;
    let bh = base.height() as usize;
    let src = base.rgba();
    let mut buf = src.to_vec();

    // Recolor every "lit" pixel (bright + saturated — i.e. one of the colored
    // bulbs, not the dark housing) to its band's state color.
    for y in 0..bh {
        let band = (y * 3 / bh.max(1)).min(2);
        let color = LEVEL_COLORS[levels[band] as usize];
        for x in 0..bw {
            let p = (y * bw + x) * 4;
            if src[p + 3] == 0 {
                continue;
            }
            let (r, g, b) = (src[p] as i32, src[p + 1] as i32, src[p + 2] as i32);
            let mx = r.max(g).max(b);
            let mn = r.min(g).min(b);
            let lit = mx > 110 && (mx - mn) * 100 > 30 * mx; // brightness + saturation > 0.30
            if lit {
                buf[p] = color[0];
                buf[p + 1] = color[1];
                buf[p + 2] = color[2];
            }
        }
    }

    // Draw the alert border after downscaling so it lands at the tray pixel
    // size (its stroke matches the icon outline at that size).
    let mut small = area_downscale(&buf, bw, bh, size, size);
    if alert {
        draw_alert_border(&mut small, size);
    }
    Image::new_owned(small, size as u32, size as u32)
}

/// Fit the plain app icon to the tray size, optionally drawing the red
/// context-usage alert border around it. Used when a badge mode is active but
/// there's no usable usage reading to draw a light or number for.
fn render_plain_icon(base: &Image, size: usize, alert: bool) -> Image<'static> {
    let mut small = area_downscale(base.rgba(), base.width() as usize, base.height() as usize, size, size);
    if alert {
        draw_alert_border(&mut small, size);
    }
    Image::new_owned(small, size as u32, size as u32)
}

/// Signed distance from point (`px`, `py`) to a rounded rectangle centered at
/// the origin with half-extents (`hx`, `hy`) and corner radius `r`. Negative
/// inside, positive outside, zero on the edge.
fn rounded_rect_sdf(px: f32, py: f32, hx: f32, hy: f32, r: f32) -> f32 {
    let qx = px.abs() - (hx - r);
    let qy = py.abs() - (hy - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let inside = qx.max(qy).min(0.0);
    outside + inside - r
}

/// Fill a slightly-rounded red plate over a `size`x`size` RGBA buffer — the
/// context-usage-over-threshold alert background for the *number* modes, which
/// have no icon outline to trace (the light/icon modes recolor the icon's own
/// border via [`draw_alert_border`] instead). The digits are drawn on top.
fn fill_alert_background(buf: &mut [u8], size: usize) {
    let n = size as f32;
    let center = n / 2.0;
    let half = center - 0.5; // outer edge sits ~0.5px from the image border
    let radius = (n * 0.18).round().max(2.0);
    for y in 0..size {
        for x in 0..size {
            let px = x as f32 + 0.5 - center;
            let py = y as f32 + 0.5 - center;
            let d = rounded_rect_sdf(px, py, half, half, radius);
            // Inside the rounded rect, with ~1px anti-aliasing on the edge.
            let cov = (0.5 - d).clamp(0.0, 1.0);
            if cov > 0.0 {
                let p = (y * size + x) * 4;
                over(&mut buf[p..p + 4], ALERT_BORDER_COLOR, (cov * 255.0).round() as u8);
            }
        }
    }
}

/// Render the number badge over the red alert background: a filled rounded-rect
/// in the alert red with the digits drawn on top in white. The number-mode
/// context-usage-over-threshold alert — a filled plate reads more clearly at
/// tray size than a thin ring around the digits.
fn render_badge_alert(text: &str, size: usize) -> Image<'static> {
    let mut buf = vec![0u8; size * size * 4];
    fill_alert_background(&mut buf, size);
    let cov = text_coverage(text, size);
    for i in 0..size * size {
        let p = i * 4;
        over(&mut buf[p..p + 4], ALERT_TEXT_COLOR, cov[i]);
    }
    Image::new_owned(buf, size as u32, size as u32)
}

/// Whether the context-usage alert should be drawn: the feature is enabled, a
/// badge style is active, and at least one local session has reached `threshold`
/// percent of its model's context window. Disabled, a `None`/`0` threshold, or a
/// `None` badge → never.
fn context_alert_active(app: &AppHandle, badge: TrayBadge, enabled: bool, threshold: Option<f32>, window_tokens: &std::collections::HashMap<String, u64>) -> bool {
    if !enabled {
        return false;
    }
    let Some(threshold) = threshold.filter(|t| *t > 0.0) else { return false };
    if badge == TrayBadge::None {
        return false;
    }
    app.try_state::<AppState>().is_some_and(|st| {
        st.snapshot().iter().any(|s| context_percent(s, window_tokens).is_some_and(|p| p >= threshold))
    })
}

/// Badge text for a sub-100% percentage: always two digits, zero-padded ("09"),
/// so the digit size and position stay stable across every value. A maxed
/// (>=100%) bucket draws the all-red traffic light instead — see `refresh`.
fn badge_text(pct: u32) -> String {
    format!("{pct:02}")
}

/// Whole-percent value (0..=100) for the bucket the badge tracks, or `None`
/// when the badge is off or there's no fresh, usable reading.
fn badge_percent(badge: TrayBadge, usage: &UsageLimits) -> Option<u32> {
    if usage.status != UsageStatus::Ok {
        return None;
    }
    let bucket = match badge {
        TrayBadge::None => return None,
        TrayBadge::FiveHourLight | TrayBadge::FiveHourNumber => usage.five_hour.as_ref(),
        TrayBadge::SevenDayLight | TrayBadge::SevenDayNumber => usage.seven_day.as_ref(),
    }?;
    Some((bucket.utilization * 100.0).round().clamp(0.0, 100.0) as u32)
}

/// One-line tooltip with whatever buckets are available, falling back to the
/// bare app name when usage is unknown.
fn tooltip(usage: &UsageLimits) -> String {
    let base = "Claude Code Dashboard";
    if usage.status != UsageStatus::Ok {
        return base.to_string();
    }
    let mut parts = Vec::new();
    if let Some(b) = &usage.five_hour {
        parts.push(format!("5h {}%", (b.utilization * 100.0).round() as u32));
    }
    if let Some(b) = &usage.seven_day {
        parts.push(format!("7d {}%", (b.utilization * 100.0).round() as u32));
    }
    if parts.is_empty() {
        base.to_string()
    } else {
        format!("{base} — {}", parts.join(" · "))
    }
}

/// Re-render the tray icon and tooltip from current config + usage state.
/// Safe to call from any thread that has the `AppHandle`; a no-op until the
/// tray exists.
pub fn refresh(app: &AppHandle) {
    let Some(tray) = app.tray_by_id("main-tray") else {
        return;
    };
    let Some(base) = app.default_window_icon().cloned() else {
        return;
    };
    let config = app
        .try_state::<ConfigState>()
        .map(|c| c.snapshot())
        .unwrap_or_default();
    let badge = config.tray_badge;
    let usage = app
        .try_state::<UsageLimitsState>()
        .map(|s| s.snapshot())
        .unwrap_or_else(UsageLimits::empty);

    let _ = tray.set_tooltip(Some(tooltip(&usage)));
    let size = target_icon_px(app);
    // A red alert is drawn when an agent's context usage is over the threshold:
    // the icon-bearing styles (light, >=100% fallback, plain-icon fallback)
    // recolor the icon's own white border red; the number style draws the
    // digits over a red background plate.
    let alert = context_alert_active(app, badge, config.tray_context_alert_enabled, config.tray_context_alert_percent, &config.context_window_tokens);
    let img = match badge_percent(badge, &usage) {
        // Light modes recolor the traffic light. Number modes draw the
        // percentage, but a maxed (>=100%) bucket shows the all-red light
        // instead of a number.
        Some(pct) if badge.is_light() || pct >= 100 => render_light_badge(&base, pct, size, alert),
        Some(pct) if alert => render_badge_alert(&badge_text(pct), size),
        Some(pct) => render_badge(&badge_text(pct), urgency_color(pct), size),
        None => render_plain_icon(&base, size, alert),
    };
    let _ = tray.set_icon(Some(img));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage_limits::LimitBucket;

    fn usage_ok(five: Option<f32>, seven: Option<f32>) -> UsageLimits {
        UsageLimits {
            five_hour: five.map(|u| LimitBucket { utilization: u, resets_at: None }),
            seven_day: seven.map(|u| LimitBucket { utilization: u, resets_at: None }),
            status: UsageStatus::Ok,
            updated: 1,
        }
    }

    #[test]
    fn badge_percent_picks_the_selected_bucket() {
        let u = usage_ok(Some(0.42), Some(0.18));
        assert_eq!(badge_percent(TrayBadge::FiveHourLight, &u), Some(42));
        assert_eq!(badge_percent(TrayBadge::FiveHourNumber, &u), Some(42));
        assert_eq!(badge_percent(TrayBadge::SevenDayLight, &u), Some(18));
        assert_eq!(badge_percent(TrayBadge::SevenDayNumber, &u), Some(18));
        assert_eq!(badge_percent(TrayBadge::None, &u), None);
    }

    #[test]
    fn badge_percent_rounds_and_clamps() {
        let u = usage_ok(Some(0.846), Some(1.5));
        assert_eq!(badge_percent(TrayBadge::FiveHourNumber, &u), Some(85));
        assert_eq!(badge_percent(TrayBadge::SevenDayNumber, &u), Some(100));
    }

    #[test]
    fn badge_percent_none_when_not_ok_or_missing() {
        let mut u = usage_ok(Some(0.5), None);
        u.status = UsageStatus::NetworkError;
        assert_eq!(badge_percent(TrayBadge::FiveHourNumber, &u), None);

        let ok_missing = usage_ok(None, Some(0.3));
        assert_eq!(badge_percent(TrayBadge::FiveHourNumber, &ok_missing), None);
    }

    #[test]
    fn urgency_color_thresholds() {
        assert_eq!(urgency_color(0), [90, 210, 120]);
        assert_eq!(urgency_color(59), [90, 210, 120]);
        assert_eq!(urgency_color(60), [240, 200, 70]);
        assert_eq!(urgency_color(84), [240, 200, 70]);
        assert_eq!(urgency_color(85), [255, 90, 90]);
        assert_eq!(urgency_color(100), [255, 90, 90]);
    }

    #[test]
    fn tooltip_lists_available_buckets() {
        assert_eq!(tooltip(&usage_ok(Some(0.42), Some(0.18))), "Claude Code Dashboard — 5h 42% · 7d 18%");
        assert_eq!(tooltip(&usage_ok(Some(0.42), None)), "Claude Code Dashboard — 5h 42%");
        let mut down = usage_ok(Some(0.42), Some(0.18));
        down.status = UsageStatus::AuthExpired;
        assert_eq!(tooltip(&down), "Claude Code Dashboard");
    }

    #[test]
    fn badge_text_is_always_two_digits() {
        assert_eq!(badge_text(0), "00");
        assert_eq!(badge_text(7), "07");
        assert_eq!(badge_text(85), "85");
        assert_eq!(badge_text(99), "99");
    }

    #[test]
    fn render_badge_renders_at_requested_size_and_draws_pixels() {
        // Renders at the requested tray size (24px = 150% DPI here), with the
        // urgency color on a transparent background.
        let out = render_badge("85", urgency_color(85), 24);
        assert_eq!((out.width(), out.height()), (24, 24));
        let rgba = out.rgba();
        let has_red = rgba
            .chunks_exact(4)
            .any(|p| p[0] == 255 && p[1] == 90 && p[2] == 90);
        assert!(has_red, "the number color should appear in the output");
        let has_transparent = rgba.chunks_exact(4).any(|p| p[3] == 0);
        assert!(has_transparent, "the background should be transparent");
    }

    #[test]
    fn embedded_font_loads_and_rasterizes_digits() {
        let f = font();
        let (m, bmp) = f.rasterize('8', 24.0);
        assert!(m.width > 0 && m.height > 0, "digit should rasterize to a bitmap");
        assert_eq!(bmp.len(), m.width * m.height);
        assert!(bmp.iter().any(|&c| c > 0), "bitmap should have inked pixels");
    }

    #[test]
    fn light_state_splits_into_six_intervals_plus_max() {
        assert_eq!(light_state(0), 0);
        assert_eq!(light_state(16), 0);
        assert_eq!(light_state(17), 1);
        assert_eq!(light_state(50), 3);
        assert_eq!(light_state(83), 4);
        assert_eq!(light_state(84), 5);
        assert_eq!(light_state(99), 5);
        assert_eq!(light_state(100), 6);
    }

    #[test]
    fn render_light_badge_recolors_lights_per_state() {
        // 1x3 base of three saturated "lit" pixels (top, middle, bottom bands).
        let lit = [50u8, 200, 50, 255]; // green-ish, saturated, bright
        let base = Image::new_owned([lit, lit, lit].concat(), 1, 3);

        // State 0 (pct 0): all green.
        let g = render_light_badge(&base, 0, 3, false);
        for row in g.rgba().chunks_exact(4).take(3) {
            assert_eq!(&row[0..3], &LEVEL_COLORS[0]);
        }
        // State 6 (pct 100): all red.
        let r = render_light_badge(&base, 100, 3, false);
        for row in r.rgba().chunks_exact(4).take(3) {
            assert_eq!(&row[0..3], &LEVEL_COLORS[2]);
        }
        // State 1 (pct 20): top amber, middle + bottom green. Output is 3x3, so
        // rows start at byte 0 (top), 12 (middle), 24 (bottom).
        let s1 = render_light_badge(&base, 20, 3, false);
        let px = s1.rgba();
        assert_eq!(&px[0..3], &LEVEL_COLORS[1], "top -> amber");
        assert_eq!(&px[12..15], &LEVEL_COLORS[0], "middle -> green");
        assert_eq!(&px[24..27], &LEVEL_COLORS[0], "bottom -> green");
    }

    #[test]
    fn alert_border_traces_icon_outline_not_the_image_frame() {
        // The stroke matches the icon's portrait outline, outset slightly — so
        // it sits inboard of the image edge, leaves the core clear, and the
        // image corners (outside the outline) stay transparent.
        let size = 32;
        let mut buf = vec![0u8; size * size * 4];
        draw_alert_border(&mut buf, size);
        let at = |x: usize, y: usize| { let p = (y * size + x) * 4; [buf[p], buf[p + 1], buf[p + 2], buf[p + 3]] };
        assert_eq!(at(16, 16)[3], 0, "core inside the ring stays clear");
        assert_eq!(&at(5, 16)[..3], &ALERT_BORDER_COLOR, "left stroke is the alert red");
        assert!(at(5, 16)[3] > 0, "left stroke is opaque");
        assert_eq!(at(0, 0)[3], 0, "image corner (outside the outset outline) stays clear");
    }

    #[test]
    fn render_light_badge_alert_draws_border_keeps_core() {
        // A uniform dark base (no lit bulbs): alert adds the red border, the
        // core is untouched, and without alert there's no red.
        let base = Image::new_owned([20u8, 20, 24, 255].repeat(32 * 32), 32, 32);
        let plain = render_light_badge(&base, 0, 32, false);
        let alert = render_light_badge(&base, 0, 32, true);
        let at = |img: &Image, x: usize, y: usize| { let i = (y * 32 + x) * 4; let r = img.rgba(); [r[i], r[i + 1], r[i + 2]] };
        assert_ne!(at(&plain, 5, 16), ALERT_BORDER_COLOR, "no border without alert");
        assert_eq!(at(&alert, 5, 16), ALERT_BORDER_COLOR, "alert draws the red border on the left stroke");
        assert_eq!(at(&alert, 16, 16), at(&plain, 16, 16), "core unchanged by the border");
    }

    #[test]
    fn alert_background_fills_red_plate_behind_white_digits() {
        let size = 24;
        let img = render_badge_alert("85", size);
        let rgba = img.rgba();
        // The plate fills the rounded rect in opaque alert red (the margins
        // around the digits are plate-only).
        let has_red_plate = rgba.chunks_exact(4).any(|p| p[..3] == ALERT_BORDER_COLOR && p[3] == 255);
        assert!(has_red_plate, "background is an opaque alert-red plate");
        // The digits are drawn in white over the plate.
        let has_white = rgba.chunks_exact(4).any(|p| p[0] == 255 && p[1] == 255 && p[2] == 255 && p[3] == 255);
        assert!(has_white, "digits are drawn in white over the red plate");
        // The image corner sits outside the rounded rect and stays transparent.
        assert_eq!(rgba[3], 0, "image corner outside the plate stays transparent");
    }

    #[test]
    fn area_downscale_halves_a_solid_block() {
        // 2x2 opaque red -> 1x1 stays opaque red (alpha-weighted average).
        let src = vec![255u8, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255];
        let out = area_downscale(&src, 2, 2, 1, 1);
        assert_eq!(out, vec![255, 0, 0, 255]);
    }
}
