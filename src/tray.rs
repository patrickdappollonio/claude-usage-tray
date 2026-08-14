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

use crate::config::{self, Config, NOTIFY_THRESHOLDS, REFRESH_CHOICES};
use crate::source::{Metric, SnapshotState, UsageSnapshot};
use jiff::{Timestamp, tz::TimeZone};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Messages the tray sends to the poll loop in `main.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// Re-read the cache immediately because the *user* asked (menu
    /// "Check for new data" or left-click). This is the only wake reason that
    /// produces a refresh notification.
    Refresh,
    /// The refresh interval changed: re-arm the timer with the new value
    /// instead of finishing the current (possibly 60 s) wait. Not a refresh —
    /// it neither re-reads the cache nor notifies.
    IntervalChanged,
    /// A notification setting changed: cut the current wait short so the poll
    /// loop picks the new preferences up from the shared state immediately
    /// rather than up to a minute later. Like `IntervalChanged`, it neither
    /// re-reads the cache nor notifies.
    NotifyChanged,
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

/// `Session 7%` / `Session —` — the compact form used in the refresh toast.
fn short_metric(label: &str, metric: Option<&Metric>) -> String {
    match metric.and_then(|metric| metric.percent) {
        Some(percent) => format!("{label} {}%", percent.round()),
        None => format!("{label} —"),
    }
}

/// Body of the notification shown after a *user-initiated* refresh.
///
/// Pure so the three outcomes are unit-testable: the cache moved forward, it
/// didn't, or there is no cache at all. Timer-driven polls never call this —
/// see the poll loop in `main.rs`.
pub fn refresh_message(
    previous: &UsageSnapshot,
    current: &UsageSnapshot,
    now: Timestamp,
    tz: &TimeZone,
) -> String {
    if current.state == SnapshotState::Missing {
        return "No data — install the statusline hook".to_string();
    }
    // `written_at` is the hook's own timestamp, so a change in it is the only
    // reliable evidence that Claude Code reported something new; percentages
    // can legitimately stay identical between two real reports.
    if current.written_at.is_some() && current.written_at != previous.written_at {
        return format!(
            "Updated — {}, {}",
            short_metric("Session", current.session.as_ref()),
            short_metric("Weekly", current.weekly.as_ref())
        );
    }
    match current.written_at {
        Some(at) => format!(
            "No new data — Claude Code last reported at {}",
            humanize_reset(at, now, tz)
        ),
        None => "No new data — Claude Code has not reported yet".to_string(),
    }
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
    /// The threshold that was crossed — one of [`NOTIFY_THRESHOLDS`].
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
        if self.threshold >= 100 {
            "The 5-hour window is fully used. Further requests will wait for the reset."
                .to_string()
        } else if self.critical {
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
///
/// Fired state is tracked for *every* threshold in [`NOTIFY_THRESHOLDS`],
/// including the ones currently switched off. That is what makes toggling
/// safe: a threshold the user re-enables while usage is already past it is
/// recorded as delivered, so it stays quiet until the next real crossing,
/// exactly as if it had been on the whole time. It is also what makes a jump
/// past several thresholds fire only the highest one.
#[derive(Debug)]
pub struct Notifier {
    /// The enabled subset of [`NOTIFY_THRESHOLDS`].
    enabled: Vec<u8>,
    /// Thresholds already delivered (or passed) for the current window.
    fired: Vec<u8>,
    window: Option<Timestamp>,
    seen_window: bool,
}

impl Notifier {
    /// Builds a notifier for the given enabled thresholds.
    pub fn new(enabled: &[u8]) -> Self {
        Notifier {
            enabled: enabled.to_vec(),
            fired: Vec::new(),
            window: None,
            seen_window: false,
        }
    }

    /// Replaces the enabled set (a menu toggle). No fired state is discarded,
    /// so re-enabling a threshold that current usage is already past does not
    /// produce a spurious alert.
    pub fn set_enabled(&mut self, enabled: &[u8]) {
        if self.enabled != enabled {
            self.enabled = enabled.to_vec();
        }
    }

    /// Feeds the latest session metric in and returns an alert to emit, if any.
    pub fn evaluate(&mut self, session: Option<&Metric>) -> Option<UsageAlert> {
        let metric = session?;
        let percent = metric.percent?;

        if !self.seen_window || self.window != metric.resets_at {
            self.seen_window = true;
            self.window = metric.resets_at;
            self.fired.clear();
        }

        // Anything usage has fallen back below is armed again.
        self.fired
            .retain(|&threshold| percent >= f64::from(threshold));

        let alert = self
            .enabled
            .iter()
            .copied()
            .filter(|&threshold| percent >= f64::from(threshold) && !self.fired.contains(&threshold))
            .max()
            .map(|threshold| UsageAlert {
                threshold,
                percent,
                critical: config::is_critical(threshold),
            });

        // Every threshold usage has passed counts as delivered — the lower
        // ones because the highest alert already covers them, the disabled
        // ones so that switching them on later stays quiet.
        for threshold in NOTIFY_THRESHOLDS {
            if percent >= f64::from(threshold) && !self.fired.contains(&threshold) {
                self.fired.push(threshold);
            }
        }

        alert
    }
}

/// The "your 5-hour window rolled over" notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetAlert {
    /// The `resets_at` that came due.
    pub at: Timestamp,
}

impl ResetAlert {
    pub fn summary(&self) -> String {
        "Claude usage".to_string()
    }

    pub fn body(&self) -> String {
        "Session quota reset — fresh 5-hour window".to_string()
    }
}

/// Pure state machine for the quota-reset notification.
///
/// It runs off the tray's own wall clock rather than off cache contents, so it
/// fires on time even when Claude Code is idle and nothing is refreshing the
/// cache — `resets_at` is an absolute timestamp, so no new data is needed to
/// know the window rolled over.
///
/// A reset only fires if the tray saw that `resets_at` while it was still in
/// the future. A window that had already expired when the tray first read the
/// cache (a stale cache from yesterday, say) is recorded as handled without
/// notifying: announcing "fresh window" for something that expired hours ago
/// would be noise at every startup.
#[derive(Debug, Default)]
pub struct ResetNotifier {
    /// A future reset we are waiting for.
    pending: Option<Timestamp>,
    /// The last `resets_at` already dealt with, fired or suppressed. Fires
    /// happen at most once per distinct value.
    handled: Option<Timestamp>,
}

impl ResetNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds in the session window's `resets_at` and the current time.
    /// `enabled` is the user's `notify_on_reset` setting: when off, the
    /// crossing is still consumed (so switching it back on later cannot
    /// resurrect an old one), it just produces no alert.
    pub fn evaluate(
        &mut self,
        resets_at: Option<Timestamp>,
        now: Timestamp,
        enabled: bool,
    ) -> Option<ResetAlert> {
        // A watched window comes due on the tray's own clock, whether or not
        // the cache still reports it. Consuming it here and not under
        // `resets_at` is what keeps a cache that went missing mid-window from
        // leaving a permanently-due deadline behind — which the poll loop
        // would busy-wait on.
        let fired = match self.pending {
            Some(at) if at <= now => {
                self.pending = None;
                self.handled = Some(at);
                enabled.then_some(ResetAlert { at })
            }
            _ => None,
        };
        // Arm the next window. A `resets_at` already in the past that we never
        // watched (a stale cache at startup) arms nothing and fires nothing.
        if let Some(at) = resets_at
            && at > now
            && self.handled != Some(at)
        {
            self.pending = Some(at);
        }
        fired
    }

    /// The moment the poll loop must be awake by, if any, so the reset alert
    /// is not delayed by a long refresh interval.
    pub fn deadline(&self) -> Option<Timestamp> {
        self.pending
    }
}

/// How long the poll loop should wait: the configured interval, cut short so
/// that a pending quota reset is handled the moment it comes due rather than
/// up to a full interval later.
///
/// A deadline that has already passed yields a zero wait; the loop then
/// immediately runs its next cycle, which consumes the crossing — so this
/// cannot spin, because the deadline is gone by the following iteration.
pub fn poll_wait(interval_secs: u64, deadline: Option<Timestamp>, now: Timestamp) -> Duration {
    let interval = Duration::from_secs(interval_secs);
    match deadline {
        Some(at) => {
            let secs = at.as_second() - now.as_second();
            let until = Duration::from_secs(u64::try_from(secs).unwrap_or(0));
            interval.min(until)
        }
        None => interval,
    }
}

/// The notification preferences the poll loop needs, shared with the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotifyPrefs {
    /// Enabled session-usage thresholds.
    pub thresholds: Vec<u8>,
    /// Whether the quota-reset alert fires.
    pub on_reset: bool,
}

impl NotifyPrefs {
    fn from_config(config: &Config) -> Self {
        NotifyPrefs {
            thresholds: config.notify_thresholds.clone(),
            on_reset: config.notify_on_reset,
        }
    }
}

/// Shared handle to [`NotifyPrefs`]. A mutex rather than atomics because the
/// value is a list; it is held only for a clone or a store, never across I/O.
#[derive(Clone)]
pub struct NotifyHandle(Arc<Mutex<NotifyPrefs>>);

impl NotifyHandle {
    fn new(prefs: NotifyPrefs) -> Self {
        NotifyHandle(Arc::new(Mutex::new(prefs)))
    }

    /// Current preferences. A poisoned mutex (a panic while holding it) still
    /// yields usable data here — recovering beats taking the tray down over a
    /// notification setting.
    pub fn get(&self) -> NotifyPrefs {
        match self.0.lock() {
            Ok(prefs) => prefs.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn set(&self, prefs: NotifyPrefs) {
        match self.0.lock() {
            Ok(mut slot) => *slot = prefs,
            Err(poisoned) => *poisoned.into_inner() = prefs,
        }
    }
}

/// Settings shared between the menu (which changes them) and the poll loop
/// (which reads the interval and the notification preferences every cycle, so
/// changes apply live).
pub struct Settings {
    /// Last-loaded/last-saved config file contents.
    config: Config,
    /// The interval the poll loop actually waits, in seconds. Shared so a
    /// radio-group change takes effect without restarting anything.
    interval: Arc<AtomicU64>,
    /// Notification preferences, shared the same way.
    notify: NotifyHandle,
    /// True when `CLAUDE_TRAY_POLL_SECS` is set to a usable value. The radio
    /// group then still persists the user's choice, but the effective interval
    /// stays the environment's.
    env_locked: bool,
}

impl Settings {
    /// Builds the shared settings from the loaded config and the environment
    /// override, returning the handle the poll loop reads.
    pub fn new(config: Config, env_secs: Option<u64>) -> Self {
        let effective = env_secs.unwrap_or(config.refresh_secs);
        let notify = NotifyHandle::new(NotifyPrefs::from_config(&config));
        Settings {
            config,
            interval: Arc::new(AtomicU64::new(effective)),
            notify,
            env_locked: env_secs.is_some(),
        }
    }

    /// Handle for the poll loop; `load` it once per cycle.
    pub fn interval_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.interval)
    }

    /// Handle for the poll loop; `get` it once per cycle.
    pub fn notify_handle(&self) -> NotifyHandle {
        self.notify.clone()
    }
}

/// The tray item itself: holds the latest snapshot, the settings, and a
/// channel back to the poll loop for the menu actions.
pub struct UsageTray {
    pub snapshot: UsageSnapshot,
    settings: Settings,
    tz: TimeZone,
    wake: Sender<Wake>,
}

impl UsageTray {
    pub fn new(snapshot: UsageSnapshot, settings: Settings, wake: Sender<Wake>) -> Self {
        UsageTray {
            snapshot,
            settings,
            tz: TimeZone::system(),
            wake,
        }
    }

    fn send(&self, wake: Wake) {
        // A closed channel means the poll loop is already gone; there is
        // nothing useful to do about it and panicking would kill the tray.
        let _ = self.wake.send(wake);
    }

    /// Radio-group handler: persist the chosen interval and, unless the
    /// environment overrides it, apply it to the running poll loop.
    fn select_refresh(&mut self, index: usize) {
        let Some(&secs) = REFRESH_CHOICES.get(index) else {
            // The index comes from the menu host; an out-of-range one is not
            // worth crashing over.
            return;
        };
        if self.settings.config.refresh_secs == secs {
            return;
        }
        self.settings.config.refresh_secs = secs;
        config::save(&self.settings.config);
        if !self.settings.env_locked {
            self.settings.interval.store(secs, Ordering::Relaxed);
            self.send(Wake::IntervalChanged);
        }
    }

    /// Checkbox handler: flip the XDG autostart entry, then mirror the new
    /// state into the config file. If writing the entry failed, nothing is
    /// recorded and the checkbox stays where it was.
    fn toggle_launch_at_login(&mut self) {
        let wanted = !crate::autostart::is_enabled();
        if !crate::autostart::set_enabled(wanted) {
            return;
        }
        self.settings.config.launch_at_login = wanted;
        config::save(&self.settings.config);
    }

    /// Persists a changed config and republishes the notification preferences
    /// to the poll loop, waking it so the change is live immediately.
    fn apply_notify_change(&mut self) {
        config::save(&self.settings.config);
        self.settings
            .notify
            .set(NotifyPrefs::from_config(&self.settings.config));
        self.send(Wake::NotifyChanged);
    }

    /// Checkbox handler for one usage threshold.
    fn toggle_threshold(&mut self, threshold: u8) {
        let enabled = !self.settings.config.notifies_at(threshold);
        self.settings.config.set_notifies_at(threshold, enabled);
        self.apply_notify_change();
    }

    /// Checkbox handler for the quota-reset alert.
    fn toggle_notify_on_reset(&mut self) {
        self.settings.config.notify_on_reset = !self.settings.config.notify_on_reset;
        self.apply_notify_change();
    }

    /// The `Notifications` sub-submenu. `enabled` is the capability probe
    /// result: with an unwritable config directory every toggle here would
    /// silently fail to persist, so they render grayed instead.
    fn notifications_menu(&self, enabled: bool) -> ksni::MenuItem<Self> {
        let mut submenu: Vec<ksni::MenuItem<Self>> = NOTIFY_THRESHOLDS
            .iter()
            .map(|&threshold| {
                ksni::menu::CheckmarkItem {
                    label: format!("At {threshold}%"),
                    enabled,
                    checked: self.settings.config.notifies_at(threshold),
                    activate: Box::new(move |tray: &mut Self| tray.toggle_threshold(threshold)),
                    ..Default::default()
                }
                .into()
            })
            .collect();
        submenu.push(ksni::MenuItem::Separator);
        submenu.push(
            ksni::menu::CheckmarkItem {
                label: "When quota resets".into(),
                enabled,
                checked: self.settings.config.notify_on_reset,
                activate: Box::new(|tray: &mut Self| tray.toggle_notify_on_reset()),
                ..Default::default()
            }
            .into(),
        );
        ksni::menu::SubMenu {
            label: "Notifications".into(),
            submenu,
            ..Default::default()
        }
        .into()
    }

    /// The `Settings` submenu.
    fn settings_menu(&self) -> ksni::MenuItem<Self> {
        // Probed on every menu build (cheap: a create_dir_all plus one tiny
        // file), so fixing a permissions problem un-grays the entries without
        // restarting the tray.
        let can_persist = config::is_writable();
        let can_autostart = crate::autostart::is_available();
        let mut submenu: Vec<ksni::MenuItem<Self>> = vec![
            ksni::menu::CheckmarkItem {
                label: "Launch at login".into(),
                enabled: can_autostart,
                // Read from disk, not from the config mirror: the file is the
                // thing the session manager actually acts on, and the user may
                // have removed it behind our back.
                checked: crate::autostart::is_enabled(),
                activate: Box::new(|tray: &mut Self| tray.toggle_launch_at_login()),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            self.notifications_menu(can_persist),
            ksni::MenuItem::Separator,
            ksni::MenuItem::Standard(ksni::menu::StandardItem {
                label: "Refresh interval".into(),
                enabled: false,
                ..Default::default()
            }),
            ksni::menu::RadioGroup {
                selected: self.settings.config.refresh_choice(),
                select: Box::new(|tray: &mut Self, index: usize| tray.select_refresh(index)),
                options: REFRESH_CHOICES
                    .iter()
                    .map(|secs| ksni::menu::RadioItem {
                        label: format!("{secs} s"),
                        enabled: can_persist,
                        ..Default::default()
                    })
                    .collect(),
            }
            .into(),
        ];
        if self.settings.env_locked {
            // Without this the radio group would look broken: the choice is
            // saved, but the tray keeps polling at the environment's cadence.
            submenu.push(ksni::MenuItem::Standard(ksni::menu::StandardItem {
                label: format!(
                    "(CLAUDE_TRAY_POLL_SECS={} is in effect)",
                    self.settings.interval.load(Ordering::Relaxed)
                ),
                enabled: false,
                ..Default::default()
            }));
        }
        ksni::menu::SubMenu {
            label: "Settings".into(),
            submenu,
            ..Default::default()
        }
        .into()
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
            self.settings_menu(),
            ksni::menu::StandardItem {
                label: "Check for new data".into(),
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

    /// Builds a snapshot with both metrics populated.
    fn full(state: SnapshotState, written_at: i64, session: f64, weekly: f64) -> UsageSnapshot {
        let mut s = snapshot(state, Some(written_at));
        s.session = Some(metric(Some(session), Some(BASE + 1000)));
        s.weekly = Some(metric(Some(weekly), Some(BASE + 1000)));
        s
    }

    #[test]
    fn refresh_message_reports_updated_when_written_at_moved() {
        let before = full(SnapshotState::Fresh, BASE - 300, 5.0, 27.0);
        let after = full(SnapshotState::Fresh, BASE - 10, 7.0, 28.0);
        assert_eq!(
            refresh_message(&before, &after, ts(BASE), &utc()),
            "Updated — Session 7%, Weekly 28%"
        );
    }

    #[test]
    fn refresh_message_reports_updated_even_when_percentages_are_unchanged() {
        // A new report with identical numbers is still new data.
        let before = full(SnapshotState::Fresh, BASE - 300, 7.0, 28.0);
        let after = full(SnapshotState::Fresh, BASE - 10, 7.0, 28.0);
        assert_eq!(
            refresh_message(&before, &after, ts(BASE), &utc()),
            "Updated — Session 7%, Weekly 28%"
        );
    }

    #[test]
    fn refresh_message_without_percentages_uses_dashes() {
        let before = snapshot(SnapshotState::Fresh, Some(BASE - 300));
        let after = snapshot(SnapshotState::Fresh, Some(BASE - 10));
        assert_eq!(
            refresh_message(&before, &after, ts(BASE), &utc()),
            "Updated — Session —, Weekly —"
        );
    }

    #[test]
    fn refresh_message_reports_no_new_data_with_the_last_report_time() {
        let before = full(SnapshotState::Fresh, BASE - 3600, 7.0, 28.0);
        let after = before.clone();
        assert_eq!(
            refresh_message(&before, &after, ts(BASE), &utc()),
            "No new data — Claude Code last reported at 21:13"
        );
    }

    #[test]
    fn refresh_message_no_new_data_applies_to_a_stale_cache_too() {
        let before = full(SnapshotState::Stale, BASE - 3600, 7.0, 28.0);
        let mut after = before.clone();
        after.state = SnapshotState::Stale;
        assert!(refresh_message(&before, &after, ts(BASE), &utc()).starts_with("No new data — "));
    }

    #[test]
    fn refresh_message_for_a_missing_cache_points_at_the_hook() {
        let before = snapshot(SnapshotState::Missing, None);
        let after = snapshot(SnapshotState::Missing, None);
        assert_eq!(
            refresh_message(&before, &after, ts(BASE), &utc()),
            "No data — install the statusline hook"
        );
    }

    #[test]
    fn refresh_message_missing_wins_even_if_written_at_changed() {
        // A cache that became unreadable must not be announced as an update.
        let before = full(SnapshotState::Fresh, BASE - 300, 7.0, 28.0);
        let after = snapshot(SnapshotState::Missing, None);
        assert_eq!(
            refresh_message(&before, &after, ts(BASE), &utc()),
            "No data — install the statusline hook"
        );
    }

    #[test]
    fn settings_use_the_config_interval_when_the_env_is_unset() {
        let settings = Settings::new(
            Config {
                refresh_secs: 30,
                ..Config::default()
            },
            None,
        );
        assert!(!settings.env_locked);
        assert_eq!(settings.interval_handle().load(Ordering::Relaxed), 30);
    }

    #[test]
    fn settings_let_the_env_override_win_and_lock_the_interval() {
        let settings = Settings::new(
            Config {
                refresh_secs: 30,
                ..Config::default()
            },
            Some(2),
        );
        assert!(settings.env_locked);
        assert_eq!(settings.interval_handle().load(Ordering::Relaxed), 2);
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

    /// All thresholds on, the shipped default.
    fn all_on() -> Notifier {
        Notifier::new(&NOTIFY_THRESHOLDS)
    }

    #[test]
    fn notifier_fires_once_per_enabled_threshold() {
        let mut n = all_on();
        assert_eq!(n.evaluate(Some(&metric(Some(10.0), Some(BASE)))), None);

        let alert = n
            .evaluate(Some(&metric(Some(51.0), Some(BASE))))
            .expect("crossing 50 fires");
        assert_eq!(alert.threshold, 50);
        assert!(!alert.critical);
        // Still above 50, below 75: no repeat.
        assert_eq!(n.evaluate(Some(&metric(Some(60.0), Some(BASE)))), None);

        let alert = n
            .evaluate(Some(&metric(Some(76.0), Some(BASE))))
            .expect("crossing 75 fires");
        assert_eq!(alert.threshold, 75);
        assert!(!alert.critical);
    }

    #[test]
    fn notifier_upper_thresholds_are_critical() {
        let mut n = all_on();
        n.evaluate(Some(&metric(Some(10.0), Some(BASE))));
        for (percent, threshold) in [(91.0, 90), (99.0, 99), (100.0, 100)] {
            let alert = n
                .evaluate(Some(&metric(Some(percent), Some(BASE))))
                .unwrap_or_else(|| panic!("crossing {threshold} fires"));
            assert_eq!(alert.threshold, threshold);
            assert!(alert.critical, "{threshold} must be critical");
        }
    }

    #[test]
    fn notifier_fires_at_exactly_the_threshold() {
        let mut n = all_on();
        let alert = n
            .evaluate(Some(&metric(Some(50.0), Some(BASE))))
            .expect("50.0 counts as reaching 50");
        assert_eq!(alert.threshold, 50);
    }

    #[test]
    fn notifier_jumping_past_several_thresholds_fires_only_the_highest() {
        let mut n = all_on();
        n.evaluate(Some(&metric(Some(10.0), Some(BASE))));
        let alert = n
            .evaluate(Some(&metric(Some(99.5), Some(BASE))))
            .expect("fires");
        assert_eq!(alert.threshold, 99);
        // The ones it flew past must stay quiet while usage stays up there.
        assert_eq!(n.evaluate(Some(&metric(Some(99.6), Some(BASE)))), None);
        assert_eq!(n.evaluate(Some(&metric(Some(91.0), Some(BASE)))), None);
        assert_eq!(n.evaluate(Some(&metric(Some(80.0), Some(BASE)))), None);
    }

    #[test]
    fn notifier_rearms_when_percent_drops_below_threshold() {
        let mut n = all_on();
        n.evaluate(Some(&metric(Some(10.0), Some(BASE))));
        assert!(n.evaluate(Some(&metric(Some(76.0), Some(BASE)))).is_some());
        assert_eq!(n.evaluate(Some(&metric(Some(60.0), Some(BASE)))), None);
        let alert = n
            .evaluate(Some(&metric(Some(77.0), Some(BASE))))
            .expect("re-armed by the drop");
        assert_eq!(alert.threshold, 75);
    }

    #[test]
    fn notifier_rearms_when_window_resets_at_changes() {
        let mut n = all_on();
        n.evaluate(Some(&metric(Some(10.0), Some(BASE))));
        assert!(n.evaluate(Some(&metric(Some(76.0), Some(BASE)))).is_some());
        // New window, still high -> fires again.
        let alert = n
            .evaluate(Some(&metric(Some(76.0), Some(BASE + 18_000))))
            .expect("new window re-arms");
        assert_eq!(alert.threshold, 75);
    }

    #[test]
    fn notifier_first_ever_reading_above_threshold_fires() {
        let mut n = all_on();
        let alert = n
            .evaluate(Some(&metric(Some(85.0), Some(BASE))))
            .expect("first reading above 75 fires");
        assert_eq!(alert.threshold, 75);
    }

    #[test]
    fn notifier_ignores_missing_data_without_rearming() {
        let mut n = all_on();
        assert!(n.evaluate(Some(&metric(Some(76.0), Some(BASE)))).is_some());
        assert_eq!(n.evaluate(None), None);
        assert_eq!(n.evaluate(Some(&metric(None, Some(BASE)))), None);
        // Data comes back unchanged: must not re-fire.
        assert_eq!(n.evaluate(Some(&metric(Some(76.0), Some(BASE)))), None);
    }

    #[test]
    fn notifier_with_no_thresholds_never_fires() {
        let mut n = Notifier::new(&[]);
        assert_eq!(n.evaluate(Some(&metric(Some(100.0), Some(BASE)))), None);
    }

    #[test]
    fn notifier_skips_disabled_thresholds_and_fires_the_next_enabled_one() {
        // Only 50 and 100 on: passing 76 must stay silent, 100 must not.
        let mut n = Notifier::new(&[50, 100]);
        let alert = n
            .evaluate(Some(&metric(Some(51.0), Some(BASE))))
            .expect("50 is on");
        assert_eq!(alert.threshold, 50);
        assert_eq!(n.evaluate(Some(&metric(Some(91.0), Some(BASE)))), None);
        let alert = n
            .evaluate(Some(&metric(Some(100.0), Some(BASE))))
            .expect("100 is on");
        assert_eq!(alert.threshold, 100);
    }

    #[test]
    fn notifier_reenabling_a_threshold_already_passed_does_not_fire() {
        // The reconfigure edge: 90 is off while usage climbs past it, then the
        // user switches it back on. Nothing happened at 90 that the user asked
        // to hear about, so it must stay silent until a real crossing.
        let mut n = Notifier::new(&[50, 75]);
        n.evaluate(Some(&metric(Some(10.0), Some(BASE))));
        assert!(n.evaluate(Some(&metric(Some(95.0), Some(BASE)))).is_some());

        n.set_enabled(&NOTIFY_THRESHOLDS);
        assert_eq!(n.evaluate(Some(&metric(Some(95.0), Some(BASE)))), None);
        assert_eq!(n.evaluate(Some(&metric(Some(96.0), Some(BASE)))), None);

        // A genuine later crossing of 99 still fires.
        let alert = n
            .evaluate(Some(&metric(Some(99.0), Some(BASE))))
            .expect("99 is a fresh crossing");
        assert_eq!(alert.threshold, 99);
    }

    #[test]
    fn notifier_reenabling_after_a_drop_fires_on_the_next_crossing() {
        let mut n = Notifier::new(&[50, 75]);
        n.evaluate(Some(&metric(Some(95.0), Some(BASE))));
        n.set_enabled(&NOTIFY_THRESHOLDS);
        // Usage falls back below 90, then climbs again: now it is a crossing
        // the user has asked to hear about.
        assert_eq!(n.evaluate(Some(&metric(Some(80.0), Some(BASE)))), None);
        let alert = n
            .evaluate(Some(&metric(Some(92.0), Some(BASE))))
            .expect("re-armed by the drop");
        assert_eq!(alert.threshold, 90);
    }

    #[test]
    fn notifier_disabling_a_threshold_silences_it_immediately() {
        let mut n = all_on();
        n.evaluate(Some(&metric(Some(10.0), Some(BASE))));
        n.set_enabled(&[100]);
        assert_eq!(n.evaluate(Some(&metric(Some(99.0), Some(BASE)))), None);
    }

    #[test]
    fn alert_text_differs_by_urgency() {
        let warn = UsageAlert {
            threshold: 50,
            percent: 51.0,
            critical: false,
        };
        assert_eq!(warn.summary(), "Claude session usage 51%");
        assert!(warn.body().contains("50%"));

        let crit = UsageAlert {
            threshold: 90,
            percent: 91.0,
            critical: true,
        };
        assert_eq!(crit.summary(), "Claude session usage 91%");
        assert!(crit.body().contains("90%"));

        // 100% is not "close to the limit" — it is the limit.
        let full = UsageAlert {
            threshold: 100,
            percent: 100.0,
            critical: true,
        };
        assert_eq!(full.summary(), "Claude session usage 100%");
        assert!(full.body().contains("fully used"));
    }

    #[test]
    fn reset_notifier_fires_once_when_the_window_it_watched_comes_due() {
        let mut r = ResetNotifier::new();
        let at = ts(BASE + 600);
        assert_eq!(r.evaluate(Some(at), ts(BASE), true), None);
        assert_eq!(r.deadline(), Some(at));

        let alert = r
            .evaluate(Some(at), ts(BASE + 600), true)
            .expect("due at exactly resets_at");
        assert_eq!(alert.at, at);
        assert_eq!(alert.body(), "Session quota reset — fresh 5-hour window");
        assert_eq!(r.deadline(), None);

        // The cache still reports the same (now past) resets_at for as long as
        // Claude Code stays idle: never fire twice for it.
        assert_eq!(r.evaluate(Some(at), ts(BASE + 900), true), None);
        assert_eq!(r.evaluate(Some(at), ts(BASE + 20_000), true), None);
    }

    #[test]
    fn reset_notifier_fires_again_for_the_next_window() {
        let mut r = ResetNotifier::new();
        let first = ts(BASE + 600);
        r.evaluate(Some(first), ts(BASE), true);
        assert!(r.evaluate(Some(first), ts(BASE + 600), true).is_some());

        let second = ts(BASE + 18_600);
        assert_eq!(r.evaluate(Some(second), ts(BASE + 700), true), None);
        let alert = r
            .evaluate(Some(second), ts(BASE + 18_600), true)
            .expect("the next window fires too");
        assert_eq!(alert.at, second);
    }

    #[test]
    fn reset_notifier_stays_silent_without_a_resets_at() {
        let mut r = ResetNotifier::new();
        assert_eq!(r.evaluate(None, ts(BASE), true), None);
        assert_eq!(r.evaluate(None, ts(BASE + 100_000), true), None);
        assert_eq!(r.deadline(), None);
    }

    #[test]
    fn reset_notifier_ignores_a_window_that_expired_before_the_tray_saw_it() {
        // Startup against a stale cache: announcing a reset that happened
        // hours ago would be noise.
        let mut r = ResetNotifier::new();
        assert_eq!(r.evaluate(Some(ts(BASE - 3600)), ts(BASE), true), None);
        assert_eq!(r.deadline(), None);
    }

    #[test]
    fn reset_notifier_consumes_the_crossing_while_disabled() {
        let mut r = ResetNotifier::new();
        let at = ts(BASE + 600);
        r.evaluate(Some(at), ts(BASE), false);
        assert_eq!(r.evaluate(Some(at), ts(BASE + 600), false), None);
        // Switching the setting back on must not resurrect the old crossing.
        assert_eq!(r.evaluate(Some(at), ts(BASE + 601), true), None);
    }

    #[test]
    fn reset_notifier_still_fires_if_the_cache_disappears_mid_window() {
        // Claude Code idle plus a deleted cache file: the reset is still a
        // fact about the clock, and the pending deadline must be consumed
        // either way — a deadline that stayed due would spin the poll loop.
        let mut r = ResetNotifier::new();
        let at = ts(BASE + 600);
        r.evaluate(Some(at), ts(BASE), true);
        let alert = r
            .evaluate(None, ts(BASE + 600), true)
            .expect("fires from the clock alone");
        assert_eq!(alert.at, at);
        assert_eq!(r.deadline(), None);
        assert_eq!(r.evaluate(None, ts(BASE + 601), true), None);
        assert_eq!(r.deadline(), None);
    }

    #[test]
    fn reset_notifier_consumes_the_deadline_even_when_disabled() {
        let mut r = ResetNotifier::new();
        let at = ts(BASE + 600);
        r.evaluate(Some(at), ts(BASE), false);
        assert_eq!(r.evaluate(None, ts(BASE + 600), false), None);
        assert_eq!(r.deadline(), None);
    }

    #[test]
    fn poll_wait_is_the_interval_when_nothing_is_pending() {
        assert_eq!(poll_wait(60, None, ts(BASE)), Duration::from_secs(60));
    }

    #[test]
    fn poll_wait_is_clamped_to_a_nearer_reset() {
        assert_eq!(
            poll_wait(60, Some(ts(BASE + 10)), ts(BASE)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn poll_wait_keeps_the_interval_when_the_reset_is_further_out() {
        assert_eq!(
            poll_wait(5, Some(ts(BASE + 4000)), ts(BASE)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn poll_wait_for_a_due_or_past_reset_is_zero() {
        assert_eq!(poll_wait(60, Some(ts(BASE)), ts(BASE)), Duration::ZERO);
        assert_eq!(poll_wait(60, Some(ts(BASE - 90)), ts(BASE)), Duration::ZERO);
    }

    /// Walks the poll loop's own sequence — evaluate, then wait for
    /// `poll_wait` — over a simulated hour with a 60 s interval, checking that
    /// the reset lands on time and that the loop never sits in a zero-wait
    /// spin.
    #[test]
    fn simulated_poll_loop_fires_the_reset_on_time_without_spinning() {
        let mut r = ResetNotifier::new();
        let reset_at = BASE + 1_000;
        let mut now = BASE;
        let mut fired_at: Option<i64> = None;
        let mut zero_waits = 0;

        for _ in 0..200 {
            if now > BASE + 3_600 {
                break;
            }
            if let Some(alert) = r.evaluate(Some(ts(reset_at)), ts(now), true) {
                assert_eq!(alert.at, ts(reset_at));
                assert!(fired_at.is_none(), "fired twice");
                fired_at = Some(now);
            }
            let wait = poll_wait(60, r.deadline(), ts(now));
            if wait.is_zero() {
                zero_waits += 1;
            }
            // A zero wait is only legitimate as the single cycle that
            // consumes a due deadline.
            assert!(zero_waits <= 1, "poll loop spun at {now}");
            now += i64::try_from(wait.as_secs()).unwrap_or(i64::MAX);
            if wait.is_zero() {
                // The real loop still does a cache read on this pass; time
                // moves on regardless.
                now += 1;
            }
        }

        assert_eq!(fired_at, Some(reset_at), "reset fired late or not at all");
    }

    #[test]
    fn notify_prefs_are_shared_live_between_the_menu_and_the_poll_loop() {
        let settings = Settings::new(Config::default(), None);
        let handle = settings.notify_handle();
        assert_eq!(
            handle.get(),
            NotifyPrefs {
                thresholds: NOTIFY_THRESHOLDS.to_vec(),
                on_reset: true,
            }
        );

        let changed = NotifyPrefs {
            thresholds: vec![100],
            on_reset: false,
        };
        settings.notify.set(changed.clone());
        assert_eq!(handle.get(), changed);
    }
}
