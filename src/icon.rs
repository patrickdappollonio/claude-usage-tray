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
//! renders identically to `Fresh` but with every pixel's alpha scaled down,
//! so the icon visibly dims while data ages.

use crate::source::{SnapshotState, UsageSnapshot};
use tiny_skia::{LineCap, Paint, PathBuilder, Pixmap, Stroke, Transform};

/// Icon sizes `ksni` is given; StatusNotifierItem hosts pick whichever fits.
const SIZES: [u32; 3] = [22, 24, 48];

/// Dim neutral gray used for the background ring and the `Missing` state.
const GRAY: (u8, u8, u8) = (128, 128, 128);

/// Alpha multiplier applied to the whole pixmap when the snapshot is stale.
const STALE_ALPHA_SCALE: f32 = 0.5;

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
    /// Severity-banded colors (green/amber/orange/red).
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
}

/// Picks the severity color for a percentage: green below 60, amber below
/// 80, orange below 95, red at 95 and above. Values outside 0..=100 are not
/// expected but are not rejected either — the bands are open-ended.
pub fn band_color(percent: f64) -> (u8, u8, u8) {
    if percent < 60.0 {
        (67, 160, 71) // green
    } else if percent < 80.0 {
        (255, 179, 0) // amber
    } else if percent < 95.0 {
        (251, 140, 0) // orange
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
            let weekly_percent = extract_percent(snapshot.weekly.as_ref());
            draw_gauge(
                &mut pixmap,
                center,
                radius,
                stroke_width,
                session_percent,
                weekly_percent,
                appearance,
            );
        }
    }

    if snapshot.state == SnapshotState::Stale {
        scale_alpha(pixmap.data_mut(), STALE_ALPHA_SCALE);
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

fn draw_gauge(
    pixmap: &mut Pixmap,
    center: f32,
    radius: f32,
    stroke_width: f32,
    session_percent: Option<f64>,
    weekly_percent: Option<f64>,
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

    // Small inner weekly dot, banded the same way — gray when there is no
    // reading, so "no data" never reads as a confident green 0%.
    // Unknown stays neutral gray in both appearances: "we have no reading"
    // must not look like a confident one, and gray is unsaturated either way.
    let weekly_color = weekly_percent
        .map(|percent| appearance.dot(percent))
        .unwrap_or(GRAY);
    fill_dot(pixmap, center, center, stroke_width * 0.55, weekly_color, 255);
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

/// Scales every channel of a premultiplied RGBA8 buffer by `factor`
/// (clamped to `[0, 1]`), which dims the pixel while preserving the
/// premultiplied invariant `rgb <= a`.
fn scale_alpha(data: &mut [u8], factor: f32) {
    let factor = factor.clamp(0.0, 1.0);
    for channel in data.iter_mut() {
        *channel = (*channel as f32 * factor).round() as u8;
    }
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

    #[test]
    fn band_color_boundaries() {
        assert_eq!(band_color(59.0), (67, 160, 71)); // green
        assert_eq!(band_color(60.0), (255, 179, 0)); // amber
        assert_eq!(band_color(79.0), (255, 179, 0)); // amber
        assert_eq!(band_color(80.0), (251, 140, 0)); // orange
        assert_eq!(band_color(94.0), (251, 140, 0)); // orange
        assert_eq!(band_color(95.0), (211, 47, 47)); // red
        assert_eq!(band_color(96.0), (211, 47, 47)); // red
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
    fn stale_state_has_lower_max_alpha_than_fresh() {
        let fresh = snapshot(SnapshotState::Fresh, Some(70.0), Some(30.0));
        let stale = snapshot(SnapshotState::Stale, Some(70.0), Some(30.0));

        let fresh_icon = &render_icons(&fresh, IconAppearance::Color)[2];
        let stale_icon = &render_icons(&stale, IconAppearance::Color)[2];

        let max_alpha = |data: &[u8]| data.chunks_exact(4).map(|px| px[0]).max().unwrap_or(0);

        let fresh_max = max_alpha(&fresh_icon.data);
        let stale_max = max_alpha(&stale_icon.data);

        assert!(
            stale_max < fresh_max,
            "stale max alpha {stale_max} should be below fresh max alpha {fresh_max}"
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

    #[test]
    fn monochrome_stale_dims_exactly_like_color_mode() {
        for dark_ui in [true, false] {
            let appearance = IconAppearance::Mono { dark_ui };
            let fresh = snapshot(SnapshotState::Fresh, Some(70.0), Some(30.0));
            let stale = snapshot(SnapshotState::Stale, Some(70.0), Some(30.0));
            let max_alpha = |icon: &ksni::Icon| {
                icon.data
                    .chunks_exact(4)
                    .map(|px| px[0])
                    .max()
                    .unwrap_or(0)
            };
            let fresh_max = max_alpha(&render_icons(&fresh, appearance)[2]);
            let stale_max = max_alpha(&render_icons(&stale, appearance)[2]);
            assert!(
                stale_max < fresh_max,
                "stale ({stale_max}) should be dimmer than fresh ({fresh_max})"
            );
            // Same 0.5 scale as color mode, within rounding.
            let expected = (f32::from(fresh_max) * STALE_ALPHA_SCALE).round() as u8;
            assert!(
                stale_max.abs_diff(expected) <= 1,
                "stale alpha {stale_max} should be about {expected}"
            );
        }
    }
}
