//! Renders a `UsageSnapshot` into the ARGB32 pixmaps `ksni` expects for the
//! tray icon: a dim background ring, a session-percent arc gauge swept
//! clockwise from the top in a color that bands with severity, and a small
//! inner dot showing the weekly percent in the same banding. See
//! `docs/superpowers/specs/2026-08-13-claude-usage-tray-design.md` for the
//! visual design ("Icon rendering").
//!
//! `SnapshotState::Missing` renders as a flat gray ring + center dot (no
//! arc, no color signal — there is no data to show). `SnapshotState::Stale`
//! renders identically to `Fresh` but with every pixel's alpha scaled down,
//! so the icon visibly dims while data ages.

// This module's public API (`render_icons`, `band_color`) is wired into the
// running tray by `tray.rs`/`main.rs` in Task 3, which don't exist yet — so
// clippy currently sees everything here as dead code from the bin target's
// point of view. Tests already exercise it. Drop this once Task 3 lands.
#![allow(dead_code)]

use crate::source::{SnapshotState, UsageSnapshot};
use tiny_skia::{LineCap, Paint, PathBuilder, Pixmap, Stroke, Transform};

/// Icon sizes `ksni` is given; StatusNotifierItem hosts pick whichever fits.
const SIZES: [u32; 3] = [22, 24, 48];

/// Dim neutral gray used for the background ring and the `Missing` state.
const GRAY: (u8, u8, u8) = (128, 128, 128);

/// Alpha multiplier applied to the whole pixmap when the snapshot is stale.
const STALE_ALPHA_SCALE: f32 = 0.5;

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

/// Renders the 22/24/48 px ARGB32 (network byte order) icons for `snapshot`.
pub fn render_icons(snapshot: &UsageSnapshot) -> Vec<ksni::Icon> {
    SIZES.iter().map(|&size| render_one(size, snapshot)).collect()
}

fn render_one(size: u32, snapshot: &UsageSnapshot) -> ksni::Icon {
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
            draw_gauge(&mut pixmap, center, radius, stroke_width, session_percent, weekly_percent);
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

fn extract_percent(metric: Option<&crate::source::Metric>) -> f64 {
    metric
        .and_then(|m| m.percent)
        .unwrap_or(0.0)
        .clamp(0.0, 100.0)
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
    session_percent: f64,
    weekly_percent: f64,
) {
    // Dim background ring, always full circle.
    stroke_circle(pixmap, center, center, radius, stroke_width, GRAY, 70);

    // Foreground arc: sweeps clockwise from the top, proportional to session%.
    let sweep_deg = (session_percent / 100.0 * 360.0) as f32;
    if sweep_deg > 0.0
        && let Some(path) = arc_path(center, center, radius, -90.0, sweep_deg)
    {
        let color = band_color(session_percent);
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

    // Small inner weekly dot, banded the same way.
    let weekly_color = band_color(weekly_percent);
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
        let icons = render_icons(&snap);
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
        let icons = render_icons(&snap);
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

        let fresh_icon = &render_icons(&fresh)[2];
        let stale_icon = &render_icons(&stale)[2];

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
        let icons = render_icons(&snap);
        assert!(icons[2].data.chunks_exact(4).any(|px| px[0] != 0));
    }
}
