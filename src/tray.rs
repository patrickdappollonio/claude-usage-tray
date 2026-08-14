//! The StatusNotifierItem tray implementation and its pure label/notification
//! logic.
//!
//! Everything that produces user-visible text lives here as a free function
//! taking `now` and a `TimeZone` explicitly, so it can be unit-tested without a
//! desktop, a D-Bus session, or a dependency on the machine's clock and locale.
//! [`UsageTray`] is a thin shell that calls those helpers with the real clock.
//!
//! The notification logic is likewise a pure state machine ([`Notifier`]): it
//! decides *whether* an alert should fire and returns a description of it. The
//! actual notify-rust emission happens in `main.rs`, which keeps this module
//! free of side effects.
//!
//! See `docs/superpowers/specs/2026-08-13-claude-usage-tray-design.md`.

use crate::source::{Metric, SnapshotState, UsageSnapshot};
use jiff::{Timestamp, tz::TimeZone};
use std::sync::mpsc::Sender;

/// Session-usage percentage at which a normal-urgency alert fires.
const WARN_THRESHOLD: f64 = 80.0;
/// Session-usage percentage at which a critical-urgency alert fires.
const CRITICAL_THRESHOLD: f64 = 95.0;

/// Messages the tray sends to the poll loop in `main.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// Re-read the cache immediately (menu "Refresh now" or left-click).
    Refresh,
    /// Shut the tray down and exit the process (menu "Quit").
    Quit,
}

/// Furthest a reset time may be from `now` before `humanize_reset` stops
/// using the weekday form: past this many days the `%a` abbreviation reads
/// as "this week", which is misleading for something further out.
const WEEKDAY_FORM_MAX_DAYS: i32 = 6;

/// Formats a reset timestamp for display: `HH:MM` when it falls on the same
/// local day as `now`; `Day HH:MM` (e.g. `Mon 12:59`) when it's within the
/// next `WEEKDAY_FORM_MAX_DAYS` days, since a bare weekday reads as "this
/// week"; otherwise `Mon DD HH:MM` (e.g. `Aug 20 09:00`) so a target further
/// out doesn't get misread as being within the week.
pub fn humanize_reset(at: Timestamp, now: Timestamp, tz: &TimeZone) -> String {
    let at = at.to_zoned(tz.clone());
    let now = now.to_zoned(tz.clone());
    if at.date() == now.date() {
        at.strftime("%H:%M").to_string()
    } else if (at.date() - now.date()).get_days().abs() <= WEEKDAY_FORM_MAX_DAYS {
        at.strftime("%a %H:%M").to_string()
    } else {
        at.strftime("%b %d %H:%M").to_string()
    }
}

/// `Session: 42% · resets 18:00`
pub fn session_line(metric: Option<&Metric>, now: Timestamp, tz: &TimeZone) -> String {
    metric_line("Session", metric, now, tz)
}

/// `Weekly: 61% · resets Mon 12:59`
pub fn weekly_line(metric: Option<&Metric>, now: Timestamp, tz: &TimeZone) -> String {
    metric_line("Weekly", metric, now, tz)
}

fn metric_line(label: &str, metric: Option<&Metric>, now: Timestamp, tz: &TimeZone) -> String {
    let Some(metric) = metric else {
        return format!("{label}: no data");
    };
    let percent = match metric.percent {
        Some(p) => format!("{}%", p.round()),
        // Em dash: the window exists in the cache but carries no percentage.
        None => "—".to_string(),
    };
    match metric.resets_at {
        Some(at) => format!("{label}: {percent} · resets {}", humanize_reset(at, now, tz)),
        None => format!("{label}: {percent}"),
    }
}

/// The third menu row: freshness of the cache, or an explanation of why there
/// is nothing to show.
pub fn status_line(snapshot: &UsageSnapshot, now: Timestamp, tz: &TimeZone) -> String {
    match snapshot.state {
        SnapshotState::Missing => "⚠ No data — install statusline hook".to_string(),
        SnapshotState::Stale => match snapshot.written_at {
            Some(at) => format!("⚠ Stale since {}", humanize_reset(at, now, tz)),
            None => "⚠ Stale".to_string(),
        },
        SnapshotState::Fresh => match snapshot.written_at {
            Some(at) => {
                // Clamp at zero: a cache written "in the future" only means the
                // writer's clock ran slightly ahead, not that time moved back.
                let age_secs = (now.as_second() - at.as_second()).max(0);
                match age_secs / 60 {
                    0 => "Updated just now".to_string(),
                    1 => "Updated 1 min ago".to_string(),
                    mins if mins < 60 => format!("Updated {mins} min ago"),
                    mins => format!("Updated {} hr ago", mins / 60),
                }
            }
            None => "Updated recently".to_string(),
        },
    }
}

/// All three menu rows joined with newlines, for the tooltip body.
pub fn tooltip_text(snapshot: &UsageSnapshot, now: Timestamp, tz: &TimeZone) -> String {
    format!(
        "{}\n{}\n{}",
        session_line(snapshot.session.as_ref(), now, tz),
        weekly_line(snapshot.weekly.as_ref(), now, tz),
        status_line(snapshot, now, tz)
    )
}

/// True when two snapshots differ in anything the tray displays. Used to avoid
/// pushing (and re-rendering) an identical icon every poll tick.
pub fn snapshot_changed(a: &UsageSnapshot, b: &UsageSnapshot) -> bool {
    a.state != b.state
        || a.written_at != b.written_at
        || a.session != b.session
        || a.weekly != b.weekly
}

/// A threshold crossing the poll loop should turn into a desktop notification.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageAlert {
    /// The threshold that was crossed (80 or 95).
    pub threshold: u8,
    /// The session percentage that triggered it.
    pub percent: f64,
    /// Whether the notification should use critical urgency.
    pub critical: bool,
}

impl UsageAlert {
    /// Notification title.
    pub fn summary(&self) -> String {
        format!("Claude session usage {}%", self.percent.round())
    }

    /// Notification body.
    pub fn body(&self) -> String {
        if self.critical {
            format!(
                "The 5-hour window is above {}%. You are close to the limit.",
                self.threshold
            )
        } else {
            format!("The 5-hour window has passed {}% usage.", self.threshold)
        }
    }
}

/// Pure state machine deciding when a threshold notification should fire.
///
/// Each threshold fires once per crossing. A threshold re-arms when the
/// session percentage drops back below it, or when `resets_at` changes (a new
/// 5-hour window began). Readings without a percentage are ignored entirely:
/// they neither fire nor re-arm, so a temporarily unreadable cache cannot
/// cause a duplicate alert.
#[derive(Debug, Default)]
pub struct Notifier {
    fired_warn: bool,
    fired_critical: bool,
    window: Option<Timestamp>,
    seen_window: bool,
}

impl Notifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds the latest session metric in and returns an alert to emit, if any.
    pub fn evaluate(&mut self, session: Option<&Metric>) -> Option<UsageAlert> {
        let metric = session?;
        let percent = metric.percent?;

        if !self.seen_window || self.window != metric.resets_at {
            self.seen_window = true;
            self.window = metric.resets_at;
            self.fired_warn = false;
            self.fired_critical = false;
        }

        if percent < WARN_THRESHOLD {
            self.fired_warn = false;
        }
        if percent < CRITICAL_THRESHOLD {
            self.fired_critical = false;
        }

        if percent >= CRITICAL_THRESHOLD && !self.fired_critical {
            self.fired_critical = true;
            // Crossing straight past both thresholds must not produce two
            // notifications, so the warn level counts as already delivered.
            self.fired_warn = true;
            return Some(UsageAlert {
                threshold: 95,
                percent,
                critical: true,
            });
        }
        if percent >= WARN_THRESHOLD && !self.fired_warn {
            self.fired_warn = true;
            return Some(UsageAlert {
                threshold: 80,
                percent,
                critical: false,
            });
        }
        None
    }
}

/// The tray item itself: holds the latest snapshot and a channel back to the
/// poll loop for the menu actions.
pub struct UsageTray {
    pub snapshot: UsageSnapshot,
    tz: TimeZone,
    wake: Sender<Wake>,
}

impl UsageTray {
    pub fn new(snapshot: UsageSnapshot, wake: Sender<Wake>) -> Self {
        UsageTray {
            snapshot,
            tz: TimeZone::system(),
            wake,
        }
    }

    fn send(&self, wake: Wake) {
        // A closed channel means the poll loop is already gone; there is
        // nothing useful to do about it and panicking would kill the tray.
        let _ = self.wake.send(wake);
    }
}

impl ksni::Tray for UsageTray {
    fn id(&self) -> String {
        "claude-usage-tray".into()
    }

    fn title(&self) -> String {
        "Claude usage".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn status(&self) -> ksni::Status {
        ksni::Status::Active
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        crate::icon::render_icons(&self.snapshot)
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Claude usage".into(),
            description: tooltip_text(&self.snapshot, Timestamp::now(), &self.tz),
            ..Default::default()
        }
    }

    /// Left-click: re-read the cache immediately.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Wake::Refresh);
    }

    /// Overriding this (even as a no-op) opts out of ksni's `NO_ABOUT_TO_SHOW`
    /// default, which otherwise skips the update_properties/update_menu pass
    /// before the menu opens. Without this override, rows like "Updated N min
    /// ago" only refresh when the poll loop happens to push a changed
    /// snapshot, so the menu can show stale text while open.
    fn menu_about_to_show(&mut self) {}

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let now = Timestamp::now();
        let info = |label: String| {
            ksni::MenuItem::Standard(ksni::menu::StandardItem {
                label,
                enabled: false,
                ..Default::default()
            })
        };
        vec![
            info(session_line(self.snapshot.session.as_ref(), now, &self.tz)),
            info(weekly_line(self.snapshot.weekly.as_ref(), now, &self.tz)),
            info(status_line(&self.snapshot, now, &self.tz)),
            ksni::MenuItem::Separator,
            ksni::menu::StandardItem {
                label: "Refresh now".into(),
                activate: Box::new(|tray: &mut Self| tray.send(Wake::Refresh)),
                ..Default::default()
            }
            .into(),
            ksni::menu::StandardItem {
                label: "Quit".into(),
                activate: Box::new(|tray: &mut Self| tray.send(Wake::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SnapshotState;

    fn ts(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).expect("valid timestamp")
    }

    // 2023-11-14 22:13:20 UTC (a Tuesday)
    const BASE: i64 = 1_700_000_000;

    fn utc() -> TimeZone {
        TimeZone::UTC
    }

    fn metric(percent: Option<f64>, resets_at: Option<i64>) -> Metric {
        Metric {
            percent,
            resets_at: resets_at.map(ts),
        }
    }

    fn snapshot(state: SnapshotState, written_at: Option<i64>) -> UsageSnapshot {
        UsageSnapshot {
            session: None,
            weekly: None,
            written_at: written_at.map(ts),
            state,
        }
    }

    #[test]
    fn humanize_reset_same_day_is_time_only() {
        // BASE is 22:13:20 UTC; +1000s -> 22:30 same day
        assert_eq!(humanize_reset(ts(BASE + 1000), ts(BASE), &utc()), "22:30");
    }

    #[test]
    fn humanize_reset_other_day_includes_weekday() {
        // BASE + 12h crosses midnight into Wednesday 10:13
        assert_eq!(
            humanize_reset(ts(BASE + 12 * 3600), ts(BASE), &utc()),
            "Wed 10:13"
        );
    }

    #[test]
    fn humanize_reset_six_days_out_still_uses_weekday_form() {
        // BASE is Tue 2023-11-14; +6 days is Mon 2023-11-20 — still "this
        // week enough" to read unambiguously as a weekday.
        assert_eq!(
            humanize_reset(ts(BASE + 6 * 86_400), ts(BASE), &utc()),
            "Mon 22:13"
        );
    }

    #[test]
    fn humanize_reset_more_than_six_days_out_uses_month_day_form() {
        // BASE is Tue 2023-11-14; +7 days is Tue 2023-11-21 — a bare "Tue"
        // would misleadingly read as "this week", so fall back to Mon DD.
        assert_eq!(
            humanize_reset(ts(BASE + 7 * 86_400), ts(BASE), &utc()),
            "Nov 21 22:13"
        );
    }

    #[test]
    fn humanize_reset_far_future_uses_month_day_form() {
        // BASE + 8 days -> Wed 2023-11-22.
        assert_eq!(
            humanize_reset(ts(BASE + 8 * 86_400), ts(BASE), &utc()),
            "Nov 22 22:13"
        );
    }

    #[test]
    fn session_line_with_percent_and_reset() {
        let m = metric(Some(42.0), Some(BASE + 1000));
        assert_eq!(
            session_line(Some(&m), ts(BASE), &utc()),
            "Session: 42% · resets 22:30"
        );
    }

    #[test]
    fn session_line_rounds_fractional_percent() {
        let m = metric(Some(61.5), Some(BASE + 1000));
        assert_eq!(
            session_line(Some(&m), ts(BASE), &utc()),
            "Session: 62% · resets 22:30"
        );
    }

    #[test]
    fn session_line_without_reset_omits_reset_clause() {
        let m = metric(Some(42.0), None);
        assert_eq!(session_line(Some(&m), ts(BASE), &utc()), "Session: 42%");
    }

    #[test]
    fn session_line_without_percent_uses_dash() {
        let m = metric(None, Some(BASE + 1000));
        assert_eq!(
            session_line(Some(&m), ts(BASE), &utc()),
            "Session: — · resets 22:30"
        );
    }

    #[test]
    fn session_line_absent_metric_is_no_data() {
        assert_eq!(session_line(None, ts(BASE), &utc()), "Session: no data");
    }

    #[test]
    fn weekly_line_uses_weekly_label() {
        let m = metric(Some(61.0), Some(BASE + 12 * 3600));
        assert_eq!(
            weekly_line(Some(&m), ts(BASE), &utc()),
            "Weekly: 61% · resets Wed 10:13"
        );
    }

    #[test]
    fn status_line_missing_tells_user_to_install_hook() {
        let s = snapshot(SnapshotState::Missing, None);
        assert_eq!(
            status_line(&s, ts(BASE), &utc()),
            "⚠ No data — install statusline hook"
        );
    }

    #[test]
    fn status_line_stale_shows_since_time() {
        let s = snapshot(SnapshotState::Stale, Some(BASE - 3600));
        assert_eq!(status_line(&s, ts(BASE), &utc()), "⚠ Stale since 21:13");
    }

    #[test]
    fn status_line_fresh_under_a_minute_is_just_now() {
        let s = snapshot(SnapshotState::Fresh, Some(BASE - 30));
        assert_eq!(status_line(&s, ts(BASE), &utc()), "Updated just now");
    }

    #[test]
    fn status_line_fresh_one_minute_is_singular() {
        let s = snapshot(SnapshotState::Fresh, Some(BASE - 60));
        assert_eq!(status_line(&s, ts(BASE), &utc()), "Updated 1 min ago");
    }

    #[test]
    fn status_line_fresh_minutes_ago() {
        let s = snapshot(SnapshotState::Fresh, Some(BASE - 185));
        assert_eq!(status_line(&s, ts(BASE), &utc()), "Updated 3 min ago");
    }

    #[test]
    fn status_line_fresh_with_clock_skew_is_just_now() {
        let s = snapshot(SnapshotState::Fresh, Some(BASE + 30));
        assert_eq!(status_line(&s, ts(BASE), &utc()), "Updated just now");
    }

    #[test]
    fn tooltip_text_has_all_three_lines() {
        let mut s = snapshot(SnapshotState::Fresh, Some(BASE - 30));
        s.session = Some(metric(Some(42.0), Some(BASE + 1000)));
        s.weekly = Some(metric(Some(61.0), Some(BASE + 1000)));
        assert_eq!(
            tooltip_text(&s, ts(BASE), &utc()),
            "Session: 42% · resets 22:30\nWeekly: 61% · resets 22:30\nUpdated just now"
        );
    }

    #[test]
    fn snapshot_changed_detects_percent_and_state_changes() {
        let mut a = snapshot(SnapshotState::Fresh, Some(BASE));
        a.session = Some(metric(Some(42.0), Some(BASE + 100)));
        let mut b = a.clone();
        assert!(!snapshot_changed(&a, &b));

        b.session = Some(metric(Some(43.0), Some(BASE + 100)));
        assert!(snapshot_changed(&a, &b));

        let mut c = a.clone();
        c.state = SnapshotState::Stale;
        assert!(snapshot_changed(&a, &c));

        let mut d = a.clone();
        d.written_at = Some(ts(BASE + 5));
        assert!(snapshot_changed(&a, &d));

        let mut e = a.clone();
        e.weekly = Some(metric(Some(1.0), None));
        assert!(snapshot_changed(&a, &e));
    }

    #[test]
    fn notifier_fires_once_at_eighty() {
        let mut n = Notifier::new();
        assert_eq!(n.evaluate(Some(&metric(Some(50.0), Some(BASE)))), None);
        let alert = n
            .evaluate(Some(&metric(Some(81.0), Some(BASE))))
            .expect("crossing 80 fires");
        assert_eq!(alert.threshold, 80);
        assert!(!alert.critical);
        // Still above 80 but not 95: no repeat.
        assert_eq!(n.evaluate(Some(&metric(Some(85.0), Some(BASE)))), None);
    }

    #[test]
    fn notifier_fires_critical_at_ninety_five() {
        let mut n = Notifier::new();
        n.evaluate(Some(&metric(Some(81.0), Some(BASE))));
        let alert = n
            .evaluate(Some(&metric(Some(96.0), Some(BASE))))
            .expect("crossing 95 fires");
        assert_eq!(alert.threshold, 95);
        assert!(alert.critical);
        assert_eq!(n.evaluate(Some(&metric(Some(99.0), Some(BASE)))), None);
    }

    #[test]
    fn notifier_jumping_past_both_thresholds_fires_only_critical() {
        let mut n = Notifier::new();
        n.evaluate(Some(&metric(Some(10.0), Some(BASE))));
        let alert = n
            .evaluate(Some(&metric(Some(97.0), Some(BASE))))
            .expect("fires");
        assert_eq!(alert.threshold, 95);
        // 80 must not fire afterwards while still high.
        assert_eq!(n.evaluate(Some(&metric(Some(90.0), Some(BASE)))), None);
    }

    #[test]
    fn notifier_rearms_when_percent_drops_below_threshold() {
        let mut n = Notifier::new();
        n.evaluate(Some(&metric(Some(10.0), Some(BASE))));
        assert!(n.evaluate(Some(&metric(Some(81.0), Some(BASE)))).is_some());
        assert_eq!(n.evaluate(Some(&metric(Some(70.0), Some(BASE)))), None);
        assert!(n.evaluate(Some(&metric(Some(82.0), Some(BASE)))).is_some());
    }

    #[test]
    fn notifier_rearms_when_window_resets_at_changes() {
        let mut n = Notifier::new();
        n.evaluate(Some(&metric(Some(10.0), Some(BASE))));
        assert!(n.evaluate(Some(&metric(Some(81.0), Some(BASE)))).is_some());
        // New window, still high -> fires again.
        let alert = n
            .evaluate(Some(&metric(Some(81.0), Some(BASE + 18_000))))
            .expect("new window re-arms");
        assert_eq!(alert.threshold, 80);
    }

    #[test]
    fn notifier_first_ever_reading_above_threshold_fires() {
        let mut n = Notifier::new();
        let alert = n
            .evaluate(Some(&metric(Some(85.0), Some(BASE))))
            .expect("first reading above 80 fires");
        assert_eq!(alert.threshold, 80);
    }

    #[test]
    fn notifier_ignores_missing_data_without_rearming() {
        let mut n = Notifier::new();
        assert!(n.evaluate(Some(&metric(Some(81.0), Some(BASE)))).is_some());
        assert_eq!(n.evaluate(None), None);
        assert_eq!(n.evaluate(Some(&metric(None, Some(BASE)))), None);
        // Data comes back unchanged: must not re-fire.
        assert_eq!(n.evaluate(Some(&metric(Some(81.0), Some(BASE)))), None);
    }

    #[test]
    fn alert_text_differs_by_urgency() {
        let warn = UsageAlert {
            threshold: 80,
            percent: 81.0,
            critical: false,
        };
        assert_eq!(warn.summary(), "Claude session usage 81%");
        assert!(warn.body().contains("80%"));

        let crit = UsageAlert {
            threshold: 95,
            percent: 96.0,
            critical: true,
        };
        assert_eq!(crit.summary(), "Claude session usage 96%");
        assert!(crit.body().contains("95%"));
    }
}
