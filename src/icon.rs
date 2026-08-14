//! Renders a `UsageSnapshot` into the ARGB32 pixmaps `ksni` expects for the
//! tray icon: a dim background ring, a session-percent arc gauge swept
//! clockwise from the top in a color that bands with severity, and a small
//! inner dot showing the weekly percent in the same banding. See
//! `docs/superpowers/specs/2026-08-13-claude-usage-tray-design.md` for the
//! visual design ("Icon rendering").
//!
//! Two appearances are supported ([`IconAppearance`]): the banded color
//! gauge, and a monochrome one that draws ring, arc and dot in a single
//! foreground color — near-white on a dark UI, near-black on a light one —
//! where the arc sweep alone carries the usage signal.
//!
//! `SnapshotState::Missing` renders as a flat gray ring + center dot (no
//! arc, no color signal — there is no data to show). `SnapshotState::Stale`
//! draws the ring and the session arc at full strength, exactly as `Fresh`
//! does, and replaces only the center weekly dot with a small question-mark
//! glyph: the data stays readable, and the glyph says "this might not be
//! current" without hiding anything.

use crate::source::{SnapshotState, UsageSnapshot};
use tiny_skia::{LineCap, Paint, PathBuilder, Pixmap, Stroke, Transform};

/// Icon sizes `ksni` is given; StatusNotifierItem hosts pick whichever fits.
const SIZES: [u32; 3] = [22, 24, 48];

/// Dim neutral gray used for the background ring and the `Missing` state.
const GRAY: (u8, u8, u8) = (128, 128, 128);

/// Neutral mid-gray for the stale question mark in color mode. Deliberately
/// lighter than [`GRAY`] and unbanded: the glyph is a caveat about the data's
/// age, not a severity reading, so it must not compete with the arc's color.
const STALE_GLYPH: (u8, u8, u8) = (153, 153, 153);

/// Monochrome foreground for a dark UI: near-white, so the icon reads against
/// a dark panel.
const MONO_ON_DARK: (u8, u8, u8) = (238, 238, 238);

/// Monochrome foreground for a light UI: near-black.
const MONO_ON_LIGHT: (u8, u8, u8) = (51, 51, 51);

/// How the icon is painted. Resolved from the user's `icon_style` setting
/// (plus, for `mono-auto`, the desktop portal's color-scheme value) before
/// every render — see `crate::tray::resolve_appearance`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IconAppearance {
    /// Severity-banded colors (green/yellow/orange/red).
    #[default]
    Color,
    /// A single foreground color. `dark_ui` describes the *desktop*, not the
    /// icon: a dark UI gets the light foreground.
    Mono { dark_ui: bool },
}

impl IconAppearance {
    /// The single color everything is drawn in, or `None` in color mode.
    fn foreground(self) -> Option<(u8, u8, u8)> {
        match self {
            IconAppearance::Color => None,
            IconAppearance::Mono { dark_ui: true } => Some(MONO_ON_DARK),
            IconAppearance::Mono { dark_ui: false } => Some(MONO_ON_LIGHT),
        }
    }

    /// Color of the dim background ring.
    fn ring(self) -> (u8, u8, u8) {
        self.foreground().unwrap_or(GRAY)
    }

    /// Color of the session arc at `percent`. In monochrome mode the sweep
    /// alone carries the signal, so the color never changes with severity.
    fn arc(self, percent: f64) -> (u8, u8, u8) {
        self.foreground().unwrap_or_else(|| band_color(percent))
    }

    /// Color of the weekly dot at `percent`, same rule as the arc.
    fn dot(self, percent: f64) -> (u8, u8, u8) {
        self.arc(percent)
    }

    /// Color of the stale question mark: neutral gray in color mode, the plain
    /// foreground in monochrome (where a second gray would be invisible
    /// against the ring).
    fn glyph(self) -> (u8, u8, u8) {
        self.foreground().unwrap_or(STALE_GLYPH)
    }
}

/// Picks the severity color for a percentage: green below 50, yellow below
/// 75, orange below 90, red at 90 and above. The boundaries are deliberately
/// the same numbers as the notification thresholds in
/// [`crate::config::NOTIFY_THRESHOLDS`], so the icon changes color at exactly
/// the moments the tray also speaks up. Values outside 0..=100 are not
/// expected but are not rejected either — the bands are open-ended.
pub fn band_color(percent: f64) -> (u8, u8, u8) {
    if percent < 50.0 {
        (67, 160, 71) // green
    } else if percent < 75.0 {
        (255, 179, 0) // yellow
    } else if percent < 90.0 {
        (239, 108, 0) // orange
    } else {
        (211, 47, 47) // red
    }
}

/// Renders the 22/24/48 px ARGB32 (network byte order) icons for `snapshot`
/// in the given appearance.
pub fn render_icons(snapshot: &UsageSnapshot, appearance: IconAppearance) -> Vec<ksni::Icon> {
    SIZES
        .iter()
        .map(|&size| render_one(size, snapshot, appearance))
        .collect()
}

fn render_one(size: u32, snapshot: &UsageSnapshot, appearance: IconAppearance) -> ksni::Icon {
    // Fixed, known-valid dimensions (22/24/48, always > 0) — Pixmap::new only
    // returns None for zero-sized or overflowing dimensions, neither of which
    // can happen here.
    let mut pixmap = Pixmap::new(size, size).expect("fixed icon sizes are always valid");

    let center = size as f32 / 2.0;
    let stroke_width = (size as f32 * 0.16).max(1.5);
    let radius = center - stroke_width;

    match snapshot.state {
        SnapshotState::Missing => draw_missing(&mut pixmap, center, radius, stroke_width),
        SnapshotState::Fresh | SnapshotState::Stale => {
            let session_percent = extract_percent(snapshot.session.as_ref());
            // Stale swaps the weekly dot for the "?" glyph; the ring and arc
            // are byte-for-byte what `Fresh` draws.
            let center_mark = if snapshot.state == SnapshotState::Stale {
                CenterMark::QuestionMark
            } else {
                CenterMark::WeeklyDot(extract_percent(snapshot.weekly.as_ref()))
            };
            draw_gauge(
                &mut pixmap,
                size,
                center,
                radius,
                stroke_width,
                session_percent,
                center_mark,
                appearance,
            );
        }
    }

    ksni::Icon {
        width: size as i32,
        height: size as i32,
        data: premultiplied_rgba_to_argb_be(pixmap.data()),
    }
}

/// `None` means "no reading" — either the metric itself is absent or its
/// `percent` field is. Callers must not treat that the same as an actual 0%:
/// see `draw_gauge`, which renders it in neutral gray rather than the green
/// that `band_color(0.0)` would otherwise (wrongly) imply.
fn extract_percent(metric: Option<&crate::source::Metric>) -> Option<f64> {
    metric.and_then(|m| m.percent).map(|p| p.clamp(0.0, 100.0))
}

fn draw_missing(pixmap: &mut Pixmap, center: f32, radius: f32, stroke_width: f32) {
    stroke_circle(pixmap, center, center, radius, stroke_width, GRAY, 220);
    fill_dot(pixmap, center, center, stroke_width * 0.6, GRAY, 220);
}

/// What goes in the middle of the gauge.
#[derive(Clone, Copy, Debug)]
enum CenterMark {
    /// The normal weekly-percent dot; `None` means "no reading" (gray).
    WeeklyDot(Option<f64>),
    /// The stale marker. It replaces the dot rather than joining it: the
    /// center is the only spot free of the ring and the arc, and two marks
    /// there would be unreadable at 22 px.
    QuestionMark,
}

#[allow(clippy::too_many_arguments)]
fn draw_gauge(
    pixmap: &mut Pixmap,
    size: u32,
    center: f32,
    radius: f32,
    stroke_width: f32,
    session_percent: Option<f64>,
    center_mark: CenterMark,
    appearance: IconAppearance,
) {
    // Dim background ring, always full circle.
    stroke_circle(
        pixmap,
        center,
        center,
        radius,
        stroke_width,
        appearance.ring(),
        70,
    );

    match session_percent {
        Some(session_percent) => {
            // Foreground arc: sweeps clockwise from the top, proportional to
            // session%.
            let sweep_deg = (session_percent / 100.0 * 360.0) as f32;
            if sweep_deg > 0.0
                && let Some(path) = arc_path(center, center, radius, -90.0, sweep_deg)
            {
                let color = appearance.arc(session_percent);
                let mut paint = Paint::default();
                paint.set_color_rgba8(color.0, color.1, color.2, 255);
                paint.anti_alias = true;
                let stroke = Stroke {
                    width: stroke_width,
                    line_cap: LineCap::Round,
                    ..Default::default()
                };
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
        // No reading: draw a full gray ring rather than staying silent (which
        // would be indistinguishable from an actual 0%) or drawing a
        // confident colored arc for data we don't have.
        None => {
            if let Some(path) = arc_path(center, center, radius, -90.0, 359.999) {
                let mut paint = Paint::default();
                paint.set_color_rgba8(GRAY.0, GRAY.1, GRAY.2, 255);
                paint.anti_alias = true;
                let stroke = Stroke {
                    width: stroke_width,
                    line_cap: LineCap::Round,
                    ..Default::default()
                };
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
    }

    match center_mark {
        // Small inner weekly dot, banded the same way — gray when there is no
        // reading, so "no data" never reads as a confident green 0%.
        // Unknown stays neutral gray in both appearances: "we have no reading"
        // must not look like a confident one, and gray is unsaturated either
        // way.
        CenterMark::WeeklyDot(weekly_percent) => {
            let weekly_color = weekly_percent
                .map(|percent| appearance.dot(percent))
                .unwrap_or(GRAY);
            fill_dot(pixmap, center, center, stroke_width * 0.55, weekly_color, 255);
        }
        CenterMark::QuestionMark => {
            draw_question_mark(pixmap, center, center, size as f32 * GLYPH_HEIGHT_RATIO, appearance.glyph());
        }
    }
}

/// Question-mark height as a fraction of the icon's edge. Sized to fill the
/// hole inside the ring without touching it at any of the three sizes.
const GLYPH_HEIGHT_RATIO: f32 = 0.44;

/// Draws a "?" of total height `height` centered on `(cx, cy)`, entirely from
/// stroked/filled tiny-skia paths — no font, no glyph atlas, nothing to
/// depend on at runtime.
///
/// Three pieces, laid out in fractions of `height` measured down from the
/// glyph's top edge:
///
/// * the **bowl**, a stroked arc of a circle centered at `0.29 h` with radius
///   `0.21 h`, running from 8 o'clock the long way round (up the left, over
///   the top, down the right) to about 4 o'clock — a 275° sweep, which leaves
///   the open notch at the bottom left that makes a "?" a "?" rather than an
///   "o". The radius stays comfortably wider than the pen so the bowl keeps a
///   visible hole instead of filling in;
/// * the **stem**, a straight stroke from where the bowl ends down and inward
///   to the vertical center line at `0.66 h`;
/// * the **dot**, a filled disc at `0.91 h`, separated from the stem's round
///   cap by a deliberate gap.
///
/// Every dimension scales with `height`, so the shape is identical at 22, 24
/// and 48 px — it just loses detail as the pixels run out.
fn draw_question_mark(pixmap: &mut Pixmap, cx: f32, cy: f32, height: f32, color: (u8, u8, u8)) {
    if height <= 0.0 {
        return;
    }
    // Never below one pixel, or the glyph anti-aliases itself into a smudge
    // at 22 px.
    let weight = (height * 0.15).max(1.0);
    let top = cy - height / 2.0;

    let bowl_cy = top + height * 0.29;
    let bowl_r = height * 0.21;
    // 120° = 8 o'clock in screen coordinates (y down); +300° walks up the
    // left side, over the top and down the right to 60° = 5 o'clock.
    let bowl_start = 130.0_f32;
    let bowl_sweep = 275.0_f32;

    let mut paint = Paint::default();
    paint.set_color_rgba8(color.0, color.1, color.2, 255);
    paint.anti_alias = true;
    let stroke = Stroke {
        width: weight,
        line_cap: LineCap::Round,
        ..Default::default()
    };

    if let Some(path) = arc_path(cx, bowl_cy, bowl_r, bowl_start, bowl_sweep) {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    // Stem: continues from the bowl's end point rather than starting at an
    // arbitrary spot, so the two read as one unbroken pen stroke.
    let end_rad = (bowl_start + bowl_sweep).to_radians();
    let stem_top_x = cx + bowl_r * end_rad.cos();
    let stem_top_y = bowl_cy + bowl_r * end_rad.sin();
    let mut pb = PathBuilder::new();
    pb.move_to(stem_top_x, stem_top_y);
    pb.line_to(cx, top + height * 0.66);
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    fill_dot(pixmap, cx, top + height * 0.91, weight * 0.60, color, 255);
}

fn stroke_circle(
    pixmap: &mut Pixmap,
    cx: f32,
    cy: f32,
    radius: f32,
    stroke_width: f32,
    color: (u8, u8, u8),
    alpha: u8,
) {
    if radius <= 0.0 {
        return;
    }
    let Some(path) = PathBuilder::from_circle(cx, cy, radius) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.0, color.1, color.2, alpha);
    paint.anti_alias = true;
    let stroke = Stroke {
        width: stroke_width,
        ..Default::default()
    };
    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

fn fill_dot(pixmap: &mut Pixmap, cx: f32, cy: f32, radius: f32, color: (u8, u8, u8), alpha: u8) {
    if radius <= 0.0 {
        return;
    }
    let Some(path) = PathBuilder::from_circle(cx, cy, radius) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.0, color.1, color.2, alpha);
    paint.anti_alias = true;
    pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
}

/// Builds an open polyline approximating a circular arc, stroked afterwards.
/// `start_deg`/`sweep_deg` are in degrees, 0 = pointing right (+x), positive
/// = clockwise in screen (y-down) coordinates, so `start_deg = -90` starts at
/// the top of the circle.
fn arc_path(cx: f32, cy: f32, radius: f32, start_deg: f32, sweep_deg: f32) -> Option<tiny_skia::Path> {
    if radius <= 0.0 || sweep_deg <= 0.0 {
        return None;
    }
    let segments = ((sweep_deg / 360.0) * 64.0).ceil().max(2.0) as usize;
    let mut pb = PathBuilder::new();
    for i in 0..=segments {
        let t = i as f32 / segments as f32;
        let deg = start_deg + sweep_deg * t;
        let rad = deg.to_radians();
        let x = cx + radius * rad.cos();
        let y = cy + radius * rad.sin();
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    pb.finish()
}

/// Converts tiny-skia's premultiplied RGBA8 buffer into the straight-alpha
/// ARGB32-big-endian byte layout `ksni::Icon` requires (network byte order,
/// i.e. alpha byte first).
fn premultiplied_rgba_to_argb_be(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for px in data.chunks_exact(4) {
        let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
        let (r, g, b) = if a == 0 {
            (0, 0, 0)
        } else {
            let unpremultiply = |c: u8| -> u8 {
                ((c as u32 * 255 + (a as u32 / 2)) / a as u32).min(255) as u8
            };
            (unpremultiply(r), unpremultiply(g), unpremultiply(b))
        };
        out.push(a);
        out.push(r);
        out.push(g);
        out.push(b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Metric;

    fn snapshot(state: SnapshotState, session_pct: Option<f64>, weekly_pct: Option<f64>) -> UsageSnapshot {
        UsageSnapshot {
            session: Some(Metric {
                percent: session_pct,
                resets_at: None,
            }),
            weekly: Some(Metric {
                percent: weekly_pct,
                resets_at: None,
            }),
            written_at: None,
            state,
        }
    }

    /// The band edges are the notification thresholds (50/75/90), so the icon
    /// and the toasts change at the same instants.
    #[test]
    fn band_color_boundaries() {
        assert_eq!(band_color(0.0), (67, 160, 71)); // green
        assert_eq!(band_color(49.0), (67, 160, 71)); // green
        assert_eq!(band_color(50.0), (255, 179, 0)); // yellow
        assert_eq!(band_color(74.0), (255, 179, 0)); // yellow
        assert_eq!(band_color(75.0), (239, 108, 0)); // orange
        assert_eq!(band_color(89.0), (239, 108, 0)); // orange
        assert_eq!(band_color(90.0), (211, 47, 47)); // red
        assert_eq!(band_color(96.0), (211, 47, 47)); // red
        assert_eq!(band_color(100.0), (211, 47, 47)); // red
    }

    #[test]
    fn renders_three_pixmaps_with_expected_sizes_and_content() {
        let snap = snapshot(SnapshotState::Fresh, Some(70.0), Some(30.0));
        let icons = render_icons(&snap, IconAppearance::Color);
        assert_eq!(icons.len(), 3);

        let expected_sizes = [22i32, 24, 48];
        for (icon, expected) in icons.iter().zip(expected_sizes) {
            assert_eq!(icon.width, expected);
            assert_eq!(icon.height, expected);
            let expected_len = (expected as usize) * (expected as usize) * 4;
            assert_eq!(icon.data.len(), expected_len);
            assert!(
                icon.data.chunks_exact(4).any(|px| px[0] != 0),
                "expected at least one non-transparent (alpha != 0) pixel"
            );
        }
    }

    #[test]
    fn missing_state_renders_dominant_gray() {
        let snap = UsageSnapshot {
            session: None,
            weekly: None,
            written_at: None,
            state: SnapshotState::Missing,
        };
        let icons = render_icons(&snap, IconAppearance::Color);
        let icon = &icons[2]; // 48px: largest, most stable sampling

        let mut gray_like = 0usize;
        let mut visible = 0usize;
        for px in icon.data.chunks_exact(4) {
            let (a, r, g, b) = (px[0], px[1], px[2], px[3]);
            if a == 0 {
                continue;
            }
            visible += 1;
            // "gray-like": r, g, b close to each other (within a few levels).
            let max = r.max(g).max(b);
            let min = r.min(g).min(b);
            if max - min <= 4 {
                gray_like += 1;
            }
        }
        assert!(visible > 0, "missing icon should draw something");
        assert!(
            gray_like as f64 / visible as f64 > 0.9,
            "expected the missing-state icon to be overwhelmingly gray, got {gray_like}/{visible}"
        );
    }

    #[test]
    fn zero_percent_session_draws_no_arc_but_still_renders_ring_and_dot() {
        let snap = snapshot(SnapshotState::Fresh, Some(0.0), Some(0.0));
        let icons = render_icons(&snap, IconAppearance::Color);
        assert!(icons[2].data.chunks_exact(4).any(|px| px[0] != 0));
    }

    /// A `None` percent must render as neutral gray, never as the green that
    /// `band_color(0.0)` would produce for an actual 0% reading — otherwise
    /// "we don't know" is indistinguishable from "definitely zero usage".
    #[test]
    fn unknown_percent_renders_gray_not_green() {
        let snap = snapshot(SnapshotState::Fresh, None, None);
        let icon = &render_icons(&snap, IconAppearance::Color)[2]; // 48px: largest, most stable sampling

        let green = (67u8, 160u8, 71u8);
        let mut green_like = 0usize;
        let mut visible = 0usize;
        for px in icon.data.chunks_exact(4) {
            let (a, r, g, b) = (px[0], px[1], px[2], px[3]);
            if a == 0 {
                continue;
            }
            visible += 1;
            let dist = (r as i32 - green.0 as i32).abs()
                + (g as i32 - green.1 as i32).abs()
                + (b as i32 - green.2 as i32).abs();
            if dist < 30 {
                green_like += 1;
            }
        }
        assert!(visible > 0, "unknown-percent icon should still draw something");
        assert_eq!(
            green_like, 0,
            "expected no green pixels when percent is unknown, got {green_like}/{visible}"
        );
    }

    /// The weekly dot specifically must not be a confident green when its
    /// percent is unknown, even if the session percent (drawn as the outer
    /// arc, far from the center) is known and happens to band green.
    #[test]
    fn unknown_weekly_percent_dot_is_not_green() {
        let size = 48u32;
        let snap = snapshot(SnapshotState::Fresh, Some(10.0), None);
        let icon = &render_icons(&snap, IconAppearance::Color)[2];

        // Sample only the small central region the dot occupies, well clear
        // of the outer arc/ring.
        let center = size as i64 / 2;
        let sample_radius: i64 = 3;
        let green = (67u8, 160u8, 71u8);
        let mut saw_center_pixel = false;
        for y in (center - sample_radius)..=(center + sample_radius) {
            for x in (center - sample_radius)..=(center + sample_radius) {
                let idx = (y as usize * size as usize + x as usize) * 4;
                let px = &icon.data[idx..idx + 4];
                let (a, r, g, b) = (px[0], px[1], px[2], px[3]);
                if a == 0 {
                    continue;
                }
                saw_center_pixel = true;
                assert_ne!(
                    (r, g, b),
                    green,
                    "weekly dot pixel at ({x},{y}) should not render band_color(0.0) \
                     (green) when its percent is unknown"
                );
            }
        }
        assert!(saw_center_pixel, "expected the weekly dot to draw something at the center");
    }

    /// Visible (alpha != 0) pixels of an icon as `(r, g, b)` triples, with the
    /// premultiplication already undone by the ARGB conversion.
    fn visible_pixels(icon: &ksni::Icon) -> Vec<(u8, u8, u8)> {
        icon.data
            .chunks_exact(4)
            .filter(|px| px[0] != 0)
            .map(|px| (px[1], px[2], px[3]))
            .collect()
    }

    /// Mean of a channel over the visible pixels; the icons are anti-aliased,
    /// so single-pixel assertions would be brittle.
    fn mean_luma(pixels: &[(u8, u8, u8)]) -> f64 {
        let sum: f64 = pixels
            .iter()
            .map(|&(r, g, b)| (r as f64 + g as f64 + b as f64) / 3.0)
            .sum();
        sum / pixels.len() as f64
    }

    #[test]
    fn monochrome_icons_contain_no_saturated_pixels() {
        // Percentages spanning every color band, including the red one that is
        // the most obviously non-gray in color mode.
        for percent in [5.0, 59.0, 70.0, 85.0, 96.0, 100.0] {
            for dark_ui in [true, false] {
                for state in [SnapshotState::Fresh, SnapshotState::Stale] {
                    let snap = snapshot(state.clone(), Some(percent), Some(percent));
                    for icon in render_icons(&snap, IconAppearance::Mono { dark_ui }) {
                        for (r, g, b) in visible_pixels(&icon) {
                            let max = r.max(g).max(b);
                            let min = r.min(g).min(b);
                            assert!(
                                max - min <= 4,
                                "saturated pixel ({r},{g},{b}) at {percent}% \
                                 dark_ui={dark_ui} state={state:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn monochrome_follows_the_ui_scheme_light_on_dark_and_dark_on_light() {
        let snap = snapshot(SnapshotState::Fresh, Some(70.0), Some(30.0));
        let on_dark = &render_icons(&snap, IconAppearance::Mono { dark_ui: true })[2];
        let on_light = &render_icons(&snap, IconAppearance::Mono { dark_ui: false })[2];

        let dark_ui_luma = mean_luma(&visible_pixels(on_dark));
        let light_ui_luma = mean_luma(&visible_pixels(on_light));

        assert!(
            dark_ui_luma > 200.0,
            "a dark UI must get a near-white icon, mean luma was {dark_ui_luma}"
        );
        assert!(
            light_ui_luma < 80.0,
            "a light UI must get a dark icon, mean luma was {light_ui_luma}"
        );
    }

    #[test]
    fn color_mode_still_bands_and_monochrome_does_not() {
        // 96% is red in color mode; the same reading in monochrome must be the
        // plain foreground, with the sweep left to carry the signal.
        let snap = snapshot(SnapshotState::Fresh, Some(96.0), Some(96.0));
        let red = (211u8, 47u8, 47u8);
        let color = &render_icons(&snap, IconAppearance::Color)[2];
        assert!(
            visible_pixels(color).contains(&red),
            "color mode should still paint the red band at 96%"
        );
        for dark_ui in [true, false] {
            let mono = &render_icons(&snap, IconAppearance::Mono { dark_ui })[2];
            assert!(
                !visible_pixels(mono).contains(&red),
                "monochrome must not paint the red band"
            );
        }
    }

    /// The arc sweep is the only signal in monochrome mode, so it had better
    /// actually differ with the percentage.
    #[test]
    fn monochrome_sweep_grows_with_session_percent() {
        let count_visible = |percent: f64| {
            let snap = snapshot(SnapshotState::Fresh, Some(percent), Some(0.0));
            visible_pixels(&render_icons(&snap, IconAppearance::Mono { dark_ui: true })[2]).len()
        };
        assert!(
            count_visible(90.0) > count_visible(20.0),
            "a fuller window must draw a longer arc"
        );
    }

    #[test]
    fn monochrome_missing_state_is_identical_to_color_mode() {
        // The gray "no data" icon carries no severity, so there is nothing for
        // the monochrome mode to change.
        let snap = UsageSnapshot {
            session: None,
            weekly: None,
            written_at: None,
            state: SnapshotState::Missing,
        };
        let color = render_icons(&snap, IconAppearance::Color);
        for dark_ui in [true, false] {
            let mono = render_icons(&snap, IconAppearance::Mono { dark_ui });
            for (a, b) in color.iter().zip(mono.iter()) {
                assert_eq!(a.data, b.data, "missing-state icon must not vary by style");
            }
        }
    }


    /// Pixels of `icon` at `distance >= min_r` from the center, i.e. the ring
    /// and arc annulus, with the small central area the weekly dot / question
    /// mark occupies excluded.
    fn outside_center(icon: &ksni::Icon, min_r: f64) -> Vec<[u8; 4]> {
        let size = icon.width as usize;
        let c = size as f64 / 2.0;
        let mut out = Vec::new();
        for y in 0..size {
            for x in 0..size {
                let dx = x as f64 + 0.5 - c;
                let dy = y as f64 + 0.5 - c;
                if (dx * dx + dy * dy).sqrt() < min_r {
                    continue;
                }
                let i = (y * size + x) * 4;
                out.push([
                    icon.data[i],
                    icon.data[i + 1],
                    icon.data[i + 2],
                    icon.data[i + 3],
                ]);
            }
        }
        out
    }

    /// Visible pixels strictly inside `max_r` of the center — the region the
    /// center mark has to itself.
    fn center_pixels(icon: &ksni::Icon, max_r: f64) -> Vec<(u8, u8, u8)> {
        let size = icon.width as usize;
        let c = size as f64 / 2.0;
        let mut out = Vec::new();
        for y in 0..size {
            for x in 0..size {
                let dx = x as f64 + 0.5 - c;
                let dy = y as f64 + 0.5 - c;
                if (dx * dx + dy * dy).sqrt() >= max_r {
                    continue;
                }
                let i = (y * size + x) * 4;
                if icon.data[i] == 0 {
                    continue;
                }
                out.push((icon.data[i + 1], icon.data[i + 2], icon.data[i + 3]));
            }
        }
        out
    }

    /// Radius (in px, at 48) that separates the ring/arc annulus from the
    /// center mark: the ring's inner edge sits at 12.5, the question mark
    /// reaches out to about 10.6.
    const SPLIT_R: f64 = 11.5;

    /// The whole point of the redesign: going stale must not cost the user any
    /// of the reading. Ring and arc have to come out byte-for-byte identical
    /// to the fresh icon — no dimming, no color change, no shortened sweep.
    #[test]
    fn stale_draws_the_same_ring_and_arc_as_fresh() {
        for appearance in [
            IconAppearance::Color,
            IconAppearance::Mono { dark_ui: true },
            IconAppearance::Mono { dark_ui: false },
        ] {
            let fresh = snapshot(SnapshotState::Fresh, Some(70.0), Some(30.0));
            let stale = snapshot(SnapshotState::Stale, Some(70.0), Some(30.0));
            let fresh_icon = &render_icons(&fresh, appearance)[2];
            let stale_icon = &render_icons(&stale, appearance)[2];

            let fresh_ring = outside_center(fresh_icon, SPLIT_R);
            let stale_ring = outside_center(stale_icon, SPLIT_R);
            assert!(
                fresh_ring.iter().filter(|px| px[0] != 0).count() > 100,
                "the sampled annulus should actually contain the ring and arc"
            );
            assert_eq!(
                fresh_ring, stale_ring,
                "stale must draw the same ring and arc as fresh ({appearance:?})"
            );
        }
    }

    /// The former behaviour scaled every pixel to 50% alpha; nothing may do
    /// that any more, at any size or in any style.
    #[test]
    fn stale_is_no_longer_dimmed() {
        for appearance in [
            IconAppearance::Color,
            IconAppearance::Mono { dark_ui: true },
            IconAppearance::Mono { dark_ui: false },
        ] {
            let fresh = snapshot(SnapshotState::Fresh, Some(70.0), Some(30.0));
            let stale = snapshot(SnapshotState::Stale, Some(70.0), Some(30.0));
            let max_alpha =
                |icon: &ksni::Icon| icon.data.chunks_exact(4).map(|px| px[0]).max().unwrap_or(0);
            for (f, s) in render_icons(&fresh, appearance)
                .iter()
                .zip(render_icons(&stale, appearance).iter())
            {
                assert_eq!(
                    max_alpha(f),
                    max_alpha(s),
                    "stale must be drawn at full strength ({appearance:?}, {}px)",
                    f.width
                );
            }
        }
    }

    /// Stale still has to be *distinguishable* from fresh — the difference has
    /// simply moved from the whole icon to the center mark.
    #[test]
    fn stale_center_differs_from_fresh_center() {
        for appearance in [
            IconAppearance::Color,
            IconAppearance::Mono { dark_ui: true },
        ] {
            let fresh = snapshot(SnapshotState::Fresh, Some(70.0), Some(30.0));
            let stale = snapshot(SnapshotState::Stale, Some(70.0), Some(30.0));
            let fresh_icon = &render_icons(&fresh, appearance)[2];
            let stale_icon = &render_icons(&stale, appearance)[2];
            assert_ne!(
                fresh_icon.data, stale_icon.data,
                "stale and fresh must not render identically ({appearance:?})"
            );
            assert_ne!(
                center_pixels(fresh_icon, SPLIT_R).len(),
                center_pixels(stale_icon, SPLIT_R).len(),
                "the center mark is where they differ ({appearance:?})"
            );
        }
    }

    /// A structural check that the glyph is a "?" rather than a fatter dot: a
    /// question mark is a tall, mostly-hollow shape, so it must (a) cover a
    /// bounding box far taller than the dot's diameter, and (b) leave the
    /// middle of that box empty — the bowl's hole and the gap above the tittle
    /// both fall on the center column. A disc of any radius fails (b).
    #[test]
    fn stale_center_is_a_hollow_tall_glyph_not_a_bigger_dot() {
        for appearance in [
            IconAppearance::Color,
            IconAppearance::Mono { dark_ui: true },
            IconAppearance::Mono { dark_ui: false },
        ] {
            let stale = snapshot(SnapshotState::Stale, Some(70.0), Some(30.0));
            let fresh = snapshot(SnapshotState::Fresh, Some(70.0), Some(30.0));
            let stale_icon = &render_icons(&stale, appearance)[2];
            let fresh_icon = &render_icons(&fresh, appearance)[2];
            let size = 48usize;
            let c = size as f64 / 2.0;

            let mut min_y = size;
            let mut max_y = 0usize;
            let mut painted = 0usize;
            let mut column_runs = 0usize;
            let mut column_prev_painted = false;
            for y in 0..size {
                let mut row_painted = false;
                for x in 0..size {
                    let dx = x as f64 + 0.5 - c;
                    let dy = y as f64 + 0.5 - c;
                    if (dx * dx + dy * dy).sqrt() >= SPLIT_R {
                        continue;
                    }
                    if stale_icon.data[(y * size + x) * 4] == 0 {
                        continue;
                    }
                    painted += 1;
                    row_painted = true;
                }
                if row_painted {
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                }
                // Walk the glyph's vertical center column and count how many
                // separate painted runs it crosses.
                let x = size / 2;
                let now = stale_icon.data[(y * size + x) * 4] != 0;
                if now && !column_prev_painted {
                    column_runs += 1;
                }
                column_prev_painted = now;
            }

            // The plain weekly dot at 48 px has radius stroke_width * 0.55 =
            // 4.2, so ~9 px of height and ~56 px of area.
            let dot_height = 2.0 * (48.0 * 0.16) * 0.55;
            assert!(
                (max_y - min_y + 1) as f64 > dot_height * 1.5,
                "the glyph must be far taller than the dot it replaces                  ({appearance:?}): {} vs {dot_height}",
                max_y - min_y + 1
            );
            assert!(
                painted > center_pixels(fresh_icon, SPLIT_R).len(),
                "the glyph must cover more than the plain dot ({appearance:?})"
            );
            // Bowl stroke, then stem, then tittle: a filled disc would give 1.
            assert!(
                column_runs >= 3,
                "the center column must cross bowl, stem and tittle separately                  ({appearance:?}), got {column_runs} run(s)"
            );
        }
    }

    /// In color mode the "?" is a neutral caveat, not a severity reading: it
    /// must never pick up a band color, or it would look like data.
    #[test]
    fn stale_glyph_is_neutral_gray_in_color_mode() {
        let stale = snapshot(SnapshotState::Stale, Some(96.0), Some(96.0));
        let icon = &render_icons(&stale, IconAppearance::Color)[2];
        let pixels = center_pixels(icon, SPLIT_R);
        assert!(!pixels.is_empty(), "the glyph should draw something");
        for (r, g, b) in pixels {
            assert!(
                r.max(g).max(b) - r.min(g).min(b) <= 4,
                "stale glyph pixel ({r},{g},{b}) is not neutral"
            );
        }
    }

    /// At 22 px the "?" loses detail, but it must still be visibly more than
    /// the dot it replaced — otherwise stale and fresh become the same icon on
    /// a normal-DPI panel.
    #[test]
    fn stale_glyph_still_beats_a_dot_at_22px() {
        for appearance in [
            IconAppearance::Color,
            IconAppearance::Mono { dark_ui: true },
        ] {
            let stale = snapshot(SnapshotState::Stale, Some(70.0), Some(30.0));
            let fresh = snapshot(SnapshotState::Fresh, Some(70.0), Some(30.0));
            let stale_icon = &render_icons(&stale, appearance)[0];
            let fresh_icon = &render_icons(&fresh, appearance)[0];
            assert_eq!(stale_icon.width, 22);
            // 22 px: ring inner edge is at 5.7, the glyph reaches ~4.8.
            let split = 5.2;
            assert!(
                center_pixels(stale_icon, split).len()
                    > center_pixels(fresh_icon, split).len() + 4,
                "the 22 px glyph must not collapse into a dot ({appearance:?})"
            );
        }
    }
}

