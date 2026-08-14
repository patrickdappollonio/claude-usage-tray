//! The portable half of the tray: every decision, every piece of user-visible
//! text, and the menu model — with nothing platform-specific in it.
//!
//! Everything that produces user-visible text lives here as a free function
//! taking `now` and a `TimeZone` explicitly, so it can be unit-tested without a
//! desktop, a D-Bus session, or a dependency on the machine's clock and locale.
//! [`TrayCore`] is a thin shell that calls those helpers with the real clock,
//! and the platform backend in [`crate::platform`] is a thin shell around
//! *that*: it renders [`TrayCore::menu`] with its native menu API and hands
//! clicks back through [`TrayCore::activate`].
//!
//! The notification logic is likewise a pure state machine ([`Notifier`]): it
//! decides *whether* an alert should fire and returns a description of it. The
//! actual toast emission happens in `main.rs` via
//! [`crate::platform::notify`], which keeps this module free of side effects.
//!
//! See `docs/superpowers/specs/2026-08-13-claude-usage-tray-design.md`.

use crate::config::{self, Config, IconStyle, NOTIFY_THRESHOLDS, REFRESH_CHOICES};
use crate::icon::IconAppearance;
use crate::menu::{MenuAction, MenuRow, RadioGroup, RadioOption};
use crate::source::{Metric, SnapshotState, UsageSnapshot};
use crate::update::Update;
use jiff::{Timestamp, tz::TimeZone};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Messages the tray sends to the poll loop in `main.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// Re-read the cache immediately because the *user* asked via the menu's
    /// "Check for new data" item. This is the only wake reason that produces
    /// a refresh notification (which says whether the cache moved forward).
    Refresh,
    /// Left-click: re-read the cache (cheap, no "did it change" toast) and
    /// show a transient, worded summary of the current usage instead.
    ShowStatus,
    /// The refresh interval changed: re-arm the timer with the new value
    /// instead of finishing the current (possibly 60 s) wait. Not a refresh —
    /// it neither re-reads the cache nor notifies.
    IntervalChanged,
    /// A notification setting changed: cut the current wait short so the poll
    /// loop picks the new preferences up from the shared state immediately
    /// rather than up to a minute later. Like `IntervalChanged`, it neither
    /// re-reads the cache nor notifies.
    NotifyChanged,
    /// Install the statusline hook (menu "Install hook", offered only while
    /// there is no data). The install itself runs in the poll loop rather than
    /// in the D-Bus callback, so the menu never blocks on filesystem work.
    InstallHook,
    /// The resolved icon appearance changed — either the user picked a
    /// different `Icon style`, or (under `mono-auto`) the desktop switched
    /// between its light and dark themes. The poll loop pushes a property
    /// update so the new icon reaches the tray host immediately; like the
    /// other settings wakes it neither re-reads the cache nor notifies.
    AppearanceChanged,
    /// A newer release was found by the update checker thread. The release
    /// itself lives in shared state ([`UpdateHandle`]); this only asks the
    /// poll loop to push a property update so the extra menu row appears
    /// without waiting for the next tick. Like the other settings wakes it
    /// neither re-reads the cache nor notifies — an update is worth a menu
    /// row, not a toast.
    UpdateAvailable,
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

/// Age of a cache written at `at`, in seconds, as seen from `now`.
///
/// Clamped at zero: a cache written "in the future" only means the writer's
/// clock ran slightly ahead, not that time moved back.
fn age_secs(at: Timestamp, now: Timestamp) -> i64 {
    (now.as_second() - at.as_second()).max(0)
}

/// Turns an age in seconds into a short elapsed-time phrase: `45 min`, `12 h`,
/// `3 d`.
///
/// A wall-clock time is a poor way to describe a long gap — `Stale since
/// 21:13` reads as "an hour ago" whether the cache is one hour or three days
/// old, and the reader has to do the subtraction. The unit widens as the gap
/// grows so the number stays small and immediately meaningful: minutes below
/// an hour, whole hours (rounded to the nearest) up to two days, whole days
/// beyond that.
fn humanize_age(secs: i64) -> String {
    const HOUR: i64 = 3600;
    const DAY: i64 = 86_400;
    if secs < HOUR {
        return format!("{} min", secs / 60);
    }
    // Rounded, not truncated: 119 minutes is much better described as "2 h"
    // than as "1 h".
    let hours = (secs + HOUR / 2) / HOUR;
    if hours < 48 {
        format!("{hours} h")
    } else {
        format!("{} d", (secs + DAY / 2) / DAY)
    }
}

/// The third menu row: freshness of the cache, or an explanation of why there
/// is nothing to show.
///
/// `_tz` is unused now that every branch reports an elapsed duration rather
/// than a wall-clock time, but it stays in the signature: it is a public
/// helper alongside `session_line`/`weekly_line`, and dropping the parameter
/// would break their symmetry for no gain.
pub fn status_line(snapshot: &UsageSnapshot, now: Timestamp, _tz: &TimeZone) -> String {
    match snapshot.state {
        // First-run wording: the actionable `Install hook` item sits directly
        // under this row, so the row states the diagnosis and the item is the
        // instruction.
        SnapshotState::Missing => "⚠ Hook not installed — no data".to_string(),
        SnapshotState::Stale => match snapshot.written_at {
            Some(at) => format!("⚠ Last updated {} ago", humanize_age(age_secs(at, now))),
            None => "⚠ Stale".to_string(),
        },
        SnapshotState::Fresh => match snapshot.written_at {
            Some(at) => match age_secs(at, now) / 60 {
                0 => "Updated just now".to_string(),
                1 => "Updated 1 min ago".to_string(),
                mins if mins < 60 => format!("Updated {mins} min ago"),
                mins => format!("Updated {} hr ago", mins / 60),
            },
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

/// One metric's contribution to [`status_message`]: `32% of your 5-hour
/// session (resets at 03:50)`. `None` when the percentage itself is unknown —
/// a window that exists in the cache but carries no percentage is treated the
/// same as no window at all, matching [`short_metric`].
fn status_clause(noun: &str, metric: Option<&Metric>, now: Timestamp, tz: &TimeZone) -> Option<String> {
    let percent = metric?.percent?;
    let base = format!("{}% of your {noun}", percent.round());
    match metric?.resets_at {
        Some(at) => Some(format!("{base} ({})", status_reset_clause(at, now, tz))),
        None => Some(base),
    }
}

/// `resets at 03:50` for a same-day reset, `resets Tue 09:00` for a further
/// one — `humanize_reset` already drops the weekday for same-day resets, so
/// "at" only reads naturally in that first form; prefixing it onto "Tue
/// 09:00" would misparse as "at Tuesday".
fn status_reset_clause(at: Timestamp, now: Timestamp, tz: &TimeZone) -> String {
    let text = humanize_reset(at, now, tz);
    let same_day = at.to_zoned(tz.clone()).date() == now.to_zoned(tz.clone()).date();
    if same_day {
        format!("resets at {text}")
    } else {
        format!("resets {text}")
    }
}

/// Body of the notification shown after a left-click: a worded readout of the
/// current usage rather than a "did the cache change" report.
///
/// Pure so every combination of known/unknown/stale is unit-testable. Unlike
/// [`refresh_message`], this never compares against a previous snapshot — a
/// left-click re-reads the cache but does not care whether it moved.
pub fn status_message(snapshot: &UsageSnapshot, now: Timestamp, tz: &TimeZone) -> String {
    if snapshot.state == SnapshotState::Missing {
        return "No usage data — install the statusline hook.".to_string();
    }

    let session = status_clause("5-hour session", snapshot.session.as_ref(), now, tz);
    let weekly = status_clause("weekly limit", snapshot.weekly.as_ref(), now, tz);

    let mut sentence = match (session, weekly) {
        (Some(session), Some(weekly)) => format!("You've used {session} and {weekly}."),
        (Some(session), None) => format!("You've used {session}; weekly usage is unknown."),
        (None, Some(weekly)) => format!("You've used {weekly}; session usage is unknown."),
        (None, None) => "No usage data reported.".to_string(),
    };

    if snapshot.state == SnapshotState::Stale
        && let Some(at) = snapshot.written_at
    {
        sentence.push_str(&format!(
            " Last updated {} ago.",
            humanize_age(age_secs(at, now))
        ));
    }

    sentence
}

/// Whether the menu should offer the `Install hook` item. Only while there is
/// no data at all: once the cache exists (even a stale one) the hook is
/// demonstrably installed and the item would be noise.
pub fn shows_install_item(state: &SnapshotState) -> bool {
    *state == SnapshotState::Missing
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

/// How far a window's `resets_at` may move between two readings and still
/// count as *the same* window.
///
/// Reported reset times are not perfectly stable — they can shift by a few
/// seconds between reports — and the notifier treats a new window as a reason
/// to re-arm every threshold. Without a tolerance, a source whose `resets_at`
/// creeps forward re-fires the same alert on every poll (which is exactly what
/// a staged `kayfabe.json` used to do, several times a minute). A real
/// rollover moves the reset time by the whole window length — hours — so a
/// minute of slack cannot hide one.
const WINDOW_JITTER_TOLERANCE_SECS: i64 = 60;

/// Whether two `resets_at` readings describe the same window, within
/// [`WINDOW_JITTER_TOLERANCE_SECS`]. "No reset time" is only the same window
/// as "no reset time".
fn same_window(a: Option<Timestamp>, b: Option<Timestamp>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a.as_second() - b.as_second()).abs() <= WINDOW_JITTER_TOLERANCE_SECS,
        (None, None) => true,
        _ => false,
    }
}

/// Pure state machine deciding when a threshold notification should fire.
///
/// Each threshold fires once per crossing. A threshold re-arms when the
/// session percentage drops back below it, or when `resets_at` moves by more
/// than [`WINDOW_JITTER_TOLERANCE_SECS`] (a new 5-hour window began; smaller
/// movements are jitter, not a rollover). Readings without a percentage are ignored entirely:
/// they neither fire nor re-arm, so a temporarily unreadable cache cannot
/// cause a duplicate alert.
///
/// The **first reading that carries a percentage is a baseline, not a
/// crossing**: every threshold at or below it is recorded as delivered and
/// nothing is emitted. Without that, restarting the tray at 82% would
/// re-announce the 75% crossing that happened before it started, every time.
/// "First" means the first *real* percentage, so a tray started before any
/// usage data exists baselines off the first genuine reading rather than off
/// the emptiness that preceded it. This is also why no notification state has
/// to survive a restart: the first reading reconstructs it.
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
    /// Whether a reading with a real percentage has been seen yet. The first
    /// one is the baseline (see [`Notifier::evaluate`]) and never alerts.
    baselined: bool,
}

impl Notifier {
    /// Builds a notifier for the given enabled thresholds.
    pub fn new(enabled: &[u8]) -> Self {
        Notifier {
            enabled: enabled.to_vec(),
            fired: Vec::new(),
            window: None,
            seen_window: false,
            baselined: false,
        }
    }

    /// Records every threshold at or below `percent` as already delivered,
    /// without producing an alert. This is what the first real reading does:
    /// see [`Notifier::evaluate`].
    fn baseline(&mut self, percent: f64) {
        self.baselined = true;
        for threshold in NOTIFY_THRESHOLDS {
            if percent >= f64::from(threshold) && !self.fired.contains(&threshold) {
                self.fired.push(threshold);
            }
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

        if !self.seen_window || !same_window(self.window, metric.resets_at) {
            self.seen_window = true;
            self.fired.clear();
        }
        // Recorded on every reading, not only on a re-arm: the comparison is
        // against the *previous* reading, so a reset time that creeps forward
        // by a second at a time never accumulates its way into a false
        // rollover.
        self.window = metric.resets_at;

        // The startup baseline. Usage that was already past a threshold when
        // the tray started is not a crossing the tray witnessed, and
        // announcing it would mean every restart above 50% re-plays an old
        // alert (the reason nothing here needs to persist notification state
        // across runs: the first reading reconstructs it).
        if !self.baselined {
            self.baseline(percent);
            return None;
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
        "Claude usage tray".to_string()
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

/// Shared slot holding the newest release the update checker has found, if
/// any: written by the checker thread, read by the menu on every build.
///
/// `None` is both "no check has succeeded" and "you are up to date" — the menu
/// draws nothing for either, and nothing else in the tray behaves differently
/// because of it. Poison-tolerant for the same reason as [`NotifyHandle`]: a
/// version banner is not worth taking the tray down for.
#[derive(Clone, Default)]
pub struct UpdateHandle(Arc<Mutex<Option<Update>>>);

impl UpdateHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// The release to advertise right now, if any.
    pub fn get(&self) -> Option<Update> {
        match self.0.lock() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Records a found release.
    pub fn set(&self, update: Option<Update>) {
        match self.0.lock() {
            Ok(mut slot) => *slot = update,
            Err(poisoned) => *poisoned.into_inner() = update,
        }
    }
}

/// Resolves the configured style plus the desktop's reported scheme into the
/// appearance the renderer takes.
///
/// The pinned monochrome styles are named for the *user's UI*: `mono-dark`
/// means "my desktop is dark", which is drawn with the light foreground.
/// `portal_dark` is only consulted for `mono-auto`, and is itself already the
/// dark-assuming fallback when the portal said nothing (see
/// the portal watcher’s `dark_ui_from_scheme`).
pub fn resolve_appearance(style: IconStyle, portal_dark: bool) -> IconAppearance {
    match style {
        IconStyle::Color => IconAppearance::Color,
        IconStyle::MonoAuto => IconAppearance::Mono {
            dark_ui: portal_dark,
        },
        IconStyle::MonoDark => IconAppearance::Mono { dark_ui: true },
        IconStyle::MonoLight => IconAppearance::Mono { dark_ui: false },
    }
}

/// The two inputs the icon appearance is resolved from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AppearanceState {
    style: IconStyle,
    /// Last value the portal watcher reported; the dark-assuming default holds
    /// until (and unless) it reports anything.
    portal_dark: bool,
}

/// Shared handle to the icon appearance: written by the menu (style changes)
/// and by the portal watcher thread (desktop theme changes), read by the tray
/// on every icon render. A mutex for the same reason as [`NotifyHandle`] — it
/// is held only for a copy or a store, never across I/O.
#[derive(Clone)]
pub struct AppearanceHandle(Arc<Mutex<AppearanceState>>);

impl AppearanceHandle {
    fn new(style: IconStyle) -> Self {
        AppearanceHandle(Arc::new(Mutex::new(AppearanceState {
            style,
            portal_dark: true,
        })))
    }

    fn with<R>(&self, f: impl FnOnce(&mut AppearanceState) -> R) -> R {
        match self.0.lock() {
            Ok(mut state) => f(&mut state),
            // A poisoned mutex must not take the tray down over an icon color.
            Err(poisoned) => f(&mut poisoned.into_inner()),
        }
    }

    /// The appearance to render with right now.
    pub fn resolved(&self) -> IconAppearance {
        self.with(|state| resolve_appearance(state.style, state.portal_dark))
    }

    /// Records the user's new choice; returns whether the rendered appearance
    /// actually changed (switching between two styles that resolve the same
    /// way needs no repaint).
    pub fn set_style(&self, style: IconStyle) -> bool {
        self.with(|state| {
            let before = resolve_appearance(state.style, state.portal_dark);
            state.style = style;
            resolve_appearance(style, state.portal_dark) != before
        })
    }

    /// Records what the portal reported; returns whether that changed the
    /// rendered appearance (it does not while a non-auto style is selected).
    pub fn set_portal_dark(&self, dark: bool) -> bool {
        self.with(|state| {
            let before = resolve_appearance(state.style, state.portal_dark);
            state.portal_dark = dark;
            resolve_appearance(state.style, dark) != before
        })
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
    /// Icon appearance, shared with the portal watcher thread too.
    appearance: AppearanceHandle,
    /// Whether the update checker may run, shared with its thread so that
    /// switching the setting off stops the *next* check without a restart.
    check_updates: Arc<AtomicBool>,
    /// The release the update checker found, if any.
    update: UpdateHandle,
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
        let appearance = AppearanceHandle::new(config.icon_style);
        let check_updates = Arc::new(AtomicBool::new(config.check_updates));
        Settings {
            config,
            interval: Arc::new(AtomicU64::new(effective)),
            notify,
            appearance,
            check_updates,
            update: UpdateHandle::new(),
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

    /// Handle for the portal watcher thread (and for the poll loop, which
    /// needs nothing from it beyond keeping it alive).
    pub fn appearance_handle(&self) -> AppearanceHandle {
        self.appearance.clone()
    }

    /// Handle for the update-checker thread; `load` it before every check so
    /// switching the setting off takes effect without a restart.
    pub fn check_updates_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.check_updates)
    }

    /// Handle for the update-checker thread (which writes it) — the menu reads
    /// the same slot through the tray's own copy.
    pub fn update_handle(&self) -> UpdateHandle {
        self.update.clone()
    }
}

/// The host capabilities the menu is drawn from.
///
/// Probed fresh on every menu build (all three checks are cheap: a
/// `create_dir_all` plus one tiny file, and two `stat`s), so fixing a
/// permissions problem un-grays the entries without restarting the tray. Taken
/// as a parameter by [`TrayCore::menu_with`] rather than read inside it, which
/// is what lets the row model be unit-tested without touching the real home
/// directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuEnv {
    /// Whether the config file can actually be written. Everything that
    /// persists a setting renders grayed when it cannot.
    pub can_persist: bool,
    /// Whether the platform's autostart mechanism is usable.
    pub autostart_available: bool,
    /// Whether autostart is currently on, read from the platform rather than
    /// from the config mirror: the entry is the thing the session manager acts
    /// on, and the user may have removed it behind our back.
    pub autostart_enabled: bool,
}

impl MenuEnv {
    /// Asks the config file and the platform's autostart backend.
    pub fn probe() -> Self {
        MenuEnv {
            can_persist: config::is_writable(),
            autostart_available: crate::platform::autostart::is_available(),
            autostart_enabled: crate::platform::autostart::is_enabled(),
        }
    }
}

/// The portable half of the tray: the latest snapshot, the settings, and a
/// channel back to the poll loop.
///
/// Platform backends own one of these and do three things with it: render its
/// [`icons`](TrayCore::icons) and [`tooltip`](TrayCore::tooltip), map its
/// [`menu`](TrayCore::menu) onto the native menu API, and hand user input back
/// through [`activate`](TrayCore::activate), [`select`](TrayCore::select) and
/// [`clicked`](TrayCore::clicked). No decision lives in a backend.
pub struct TrayCore {
    pub snapshot: UsageSnapshot,
    settings: Settings,
    tz: TimeZone,
    wake: Sender<Wake>,
}

impl TrayCore {
    pub fn new(snapshot: UsageSnapshot, settings: Settings, wake: Sender<Wake>) -> Self {
        TrayCore {
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

    /// The icon pixmaps for the current snapshot and appearance.
    pub fn icons(&self) -> Vec<crate::icon::IconImage> {
        crate::icon::render_icons(&self.snapshot, self.settings.appearance.resolved())
    }

    /// The appearance [`icons`](TrayCore::icons) would render with right now.
    ///
    /// Backends do not choose an appearance — this is the resolved user
    /// setting — but a backend may need to *know* which one it is: the macOS
    /// one marks the status item as an AppKit template image (so the system
    /// tints it for the menu bar) only when the user asked for monochrome,
    /// because a template image throws the severity colors away.
    #[cfg(any(target_os = "macos", test))]
    pub fn appearance(&self) -> crate::icon::IconAppearance {
        self.settings.appearance.resolved()
    }

    /// The tooltip body: the same three lines the menu opens with.
    pub fn tooltip(&self) -> String {
        tooltip_text(&self.snapshot, Timestamp::now(), &self.tz)
    }

    /// Left-click: show a worded summary of current usage. Unlike "Check for
    /// new data" this never reports whether the cache moved.
    ///
    /// Linux-only in practice: on macOS every click opens the menu (the menu
    /// bar convention), whose info rows already carry this summary.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn clicked(&self) {
        self.send(Wake::ShowStatus);
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

    /// Radio-group handler: persist the chosen icon style and repaint. The
    /// appearance is shared state, so pushing a property update (via the wake)
    /// is all the poll loop has to do.
    fn select_icon_style(&mut self, index: usize) {
        let Some(&style) = IconStyle::ALL.get(index) else {
            // Out-of-range index from the menu host: nothing to crash over.
            return;
        };
        if self.settings.config.icon_style == style {
            return;
        }
        self.settings.config.icon_style = style;
        config::save(&self.settings.config);
        if self.settings.appearance.set_style(style) {
            self.send(Wake::AppearanceChanged);
        }
    }

    /// Checkbox handler: flip the platform's autostart entry, then mirror the
    /// new state into the config file. If writing the entry failed, nothing is
    /// recorded and the checkbox stays where it was.
    fn toggle_launch_at_login(&mut self) {
        let wanted = !crate::platform::autostart::is_enabled();
        if !crate::platform::autostart::set_enabled(wanted) {
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

    /// Checkbox handler for the update check. Switching it off stops the next
    /// scheduled check (the checker thread re-reads the shared flag before
    /// every request) but deliberately leaves an already-found update on the
    /// menu: hiding a result the user has already been shown would look like a
    /// bug, and the row is one click away from being acted on.
    fn toggle_check_updates(&mut self) {
        let enabled = !self.settings.config.check_updates;
        self.settings.config.check_updates = enabled;
        config::save(&self.settings.config);
        self.settings.check_updates.store(enabled, Ordering::Relaxed);
    }

    /// Runs the action behind a menu row. Called by the backend from whatever
    /// thread its menu callbacks arrive on, so everything here is either
    /// in-memory or a small config write — the two slow things (the hook
    /// install and the cache re-read) are deferred to the poll loop through a
    /// [`Wake`].
    pub fn activate(&mut self, action: &MenuAction) {
        match action {
            MenuAction::InstallHook => self.send(Wake::InstallHook),
            // `xdg-open` (or its platform equivalent) is spawned and never
            // waited on, so a slow browser cannot block the callback thread.
            MenuAction::OpenUrl(url) => crate::update::open_url(url),
            MenuAction::Refresh => self.send(Wake::Refresh),
            MenuAction::Quit => self.send(Wake::Quit),
            MenuAction::ToggleLaunchAtLogin => self.toggle_launch_at_login(),
            MenuAction::ToggleThreshold(threshold) => self.toggle_threshold(*threshold),
            MenuAction::ToggleNotifyOnReset => self.toggle_notify_on_reset(),
            MenuAction::ToggleCheckUpdates => self.toggle_check_updates(),
        }
    }

    /// Runs a radio-group selection.
    pub fn select(&mut self, group: RadioGroup, index: usize) {
        match group {
            RadioGroup::RefreshInterval => self.select_refresh(index),
            RadioGroup::IconStyle => self.select_icon_style(index),
        }
    }

    /// The menu as the user should see it right now.
    pub fn menu(&self) -> Vec<MenuRow> {
        self.menu_with(Timestamp::now(), MenuEnv::probe())
    }

    /// The menu for an explicit clock and host capability set, so every row can
    /// be pinned by a test.
    pub fn menu_with(&self, now: Timestamp, env: MenuEnv) -> Vec<MenuRow> {
        let mut rows = vec![
            MenuRow::info(session_line(self.snapshot.session.as_ref(), now, &self.tz)),
            MenuRow::info(weekly_line(self.snapshot.weekly.as_ref(), now, &self.tz)),
            MenuRow::info(status_line(&self.snapshot, now, &self.tz)),
        ];
        if shows_install_item(&self.snapshot.state) {
            // The one enabled row in the no-data state: everything else here
            // is a label, and a first-run user needs exactly one thing to do.
            rows.push(MenuRow::action("Install hook", MenuAction::InstallHook));
        }
        rows.push(MenuRow::Separator);
        if let Some(update) = self.settings.update.get() {
            // Enabled, unlike the three info rows above it: this one does
            // something.
            rows.push(MenuRow::action(
                update.label(),
                MenuAction::OpenUrl(update.url.clone()),
            ));
        }
        rows.extend([
            self.settings_menu(env),
            MenuRow::action("Check for new data", MenuAction::Refresh),
            MenuRow::action("Quit", MenuAction::Quit),
        ]);
        rows
    }

    /// The `Notifications` sub-submenu. `enabled` is the capability probe
    /// result: with an unwritable config directory every toggle here would
    /// silently fail to persist, so they render grayed instead.
    fn notifications_menu(&self, enabled: bool) -> MenuRow {
        let mut rows: Vec<MenuRow> = NOTIFY_THRESHOLDS
            .iter()
            .map(|&threshold| MenuRow::Check {
                label: format!("At {threshold}%"),
                action: MenuAction::ToggleThreshold(threshold),
                checked: self.settings.config.notifies_at(threshold),
                enabled,
            })
            .collect();
        rows.push(MenuRow::Separator);
        rows.push(MenuRow::Check {
            label: "When quota resets".into(),
            action: MenuAction::ToggleNotifyOnReset,
            checked: self.settings.config.notify_on_reset,
            enabled,
        });
        MenuRow::SubMenu {
            label: "Notifications".into(),
            rows,
        }
    }

    /// The `Settings` submenu.
    fn settings_menu(&self, env: MenuEnv) -> MenuRow {
        let can_persist = env.can_persist;
        let mut rows: Vec<MenuRow> = vec![
            MenuRow::Check {
                label: "Launch at login".into(),
                action: MenuAction::ToggleLaunchAtLogin,
                checked: env.autostart_enabled,
                enabled: env.autostart_available,
            },
            MenuRow::Separator,
            self.notifications_menu(can_persist),
            MenuRow::Separator,
            MenuRow::info("Refresh interval"),
            MenuRow::Radio {
                group: RadioGroup::RefreshInterval,
                selected: self.settings.config.refresh_choice(),
                options: REFRESH_CHOICES
                    .iter()
                    .map(|secs| RadioOption {
                        label: format!("{secs} s"),
                        enabled: can_persist,
                    })
                    .collect(),
            },
            MenuRow::Separator,
            MenuRow::info("Icon style"),
            MenuRow::Radio {
                group: RadioGroup::IconStyle,
                selected: self.settings.config.icon_style.choice(),
                options: IconStyle::ALL
                    .iter()
                    .map(|style| RadioOption {
                        label: style.label().to_string(),
                        enabled: can_persist,
                    })
                    .collect(),
            },
        ];
        if self.settings.env_locked {
            // Without this the radio group would look broken: the choice is
            // saved, but the tray keeps polling at the environment's cadence.
            rows.push(MenuRow::info(format!(
                "(CLAUDE_TRAY_POLL_SECS={} is in effect)",
                self.settings.interval.load(Ordering::Relaxed)
            )));
        }
        rows.push(MenuRow::Separator);
        rows.push(MenuRow::Check {
            label: "Check for updates".into(),
            action: MenuAction::ToggleCheckUpdates,
            // Grayed with the rest of the persisted settings: a toggle that
            // cannot be written would silently revert on restart.
            checked: self.settings.config.check_updates,
            enabled: can_persist,
        });
        MenuRow::SubMenu {
            label: "Settings".into(),
            rows,
        }
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
    fn status_line_missing_says_the_hook_is_not_installed() {
        let s = snapshot(SnapshotState::Missing, None);
        assert_eq!(
            status_line(&s, ts(BASE), &utc()),
            "⚠ Hook not installed — no data"
        );
    }

    #[test]
    fn the_install_item_appears_only_while_there_is_no_data() {
        assert!(shows_install_item(&SnapshotState::Missing));
        assert!(!shows_install_item(&SnapshotState::Stale));
        assert!(!shows_install_item(&SnapshotState::Fresh));
    }

    #[test]
    fn status_line_stale_under_an_hour_counts_minutes() {
        // Stale starts at 10 minutes, so this is the shortest gap the branch
        // ever has to describe.
        let s = snapshot(SnapshotState::Stale, Some(BASE - 11 * 60));
        assert_eq!(
            status_line(&s, ts(BASE), &utc()),
            "⚠ Last updated 11 min ago"
        );
        let s = snapshot(SnapshotState::Stale, Some(BASE - 59 * 60));
        assert_eq!(
            status_line(&s, ts(BASE), &utc()),
            "⚠ Last updated 59 min ago"
        );
    }

    #[test]
    fn status_line_stale_switches_to_hours_at_one_hour() {
        let s = snapshot(SnapshotState::Stale, Some(BASE - 3600));
        assert_eq!(status_line(&s, ts(BASE), &utc()), "⚠ Last updated 1 h ago");
        let s = snapshot(SnapshotState::Stale, Some(BASE - 12 * 3600));
        assert_eq!(status_line(&s, ts(BASE), &utc()), "⚠ Last updated 12 h ago");
    }

    #[test]
    fn status_line_stale_rounds_hours_to_the_nearest() {
        // 1 h 45 min is "2 h", not "1 h".
        let s = snapshot(SnapshotState::Stale, Some(BASE - (3600 + 45 * 60)));
        assert_eq!(status_line(&s, ts(BASE), &utc()), "⚠ Last updated 2 h ago");
        // ...and 1 h 10 min still rounds down.
        let s = snapshot(SnapshotState::Stale, Some(BASE - (3600 + 10 * 60)));
        assert_eq!(status_line(&s, ts(BASE), &utc()), "⚠ Last updated 1 h ago");
    }

    #[test]
    fn status_line_stale_switches_to_days_at_forty_eight_hours() {
        // 47 h stays in hours; 48 h is the first "2 d".
        let s = snapshot(SnapshotState::Stale, Some(BASE - 47 * 3600));
        assert_eq!(status_line(&s, ts(BASE), &utc()), "⚠ Last updated 47 h ago");
        let s = snapshot(SnapshotState::Stale, Some(BASE - 48 * 3600));
        assert_eq!(status_line(&s, ts(BASE), &utc()), "⚠ Last updated 2 d ago");
        let s = snapshot(SnapshotState::Stale, Some(BASE - 9 * 86_400));
        assert_eq!(status_line(&s, ts(BASE), &utc()), "⚠ Last updated 9 d ago");
    }

    #[test]
    fn status_line_stale_without_a_timestamp_just_says_stale() {
        let s = snapshot(SnapshotState::Stale, None);
        assert_eq!(status_line(&s, ts(BASE), &utc()), "⚠ Stale");
    }

    #[test]
    fn status_line_stale_with_clock_skew_does_not_go_negative() {
        // A cache "written in the future" is a skewed clock, not time travel.
        let s = snapshot(SnapshotState::Stale, Some(BASE + 300));
        assert_eq!(status_line(&s, ts(BASE), &utc()), "⚠ Last updated 0 min ago");
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
    fn status_message_missing_points_at_the_hook() {
        let s = snapshot(SnapshotState::Missing, None);
        assert_eq!(
            status_message(&s, ts(BASE), &utc()),
            "No usage data — install the statusline hook."
        );
    }

    #[test]
    fn status_message_fresh_with_both_percents() {
        // BASE is 22:13:20 UTC (Tue); +1000s -> 22:30 same day (session);
        // +12h -> Wed 10:13 (weekly), well past `WEEKDAY_FORM_MAX_DAYS`... no,
        // within it, so it takes the weekday form.
        let mut s = snapshot(SnapshotState::Fresh, Some(BASE - 30));
        s.session = Some(metric(Some(32.0), Some(BASE + 1000)));
        s.weekly = Some(metric(Some(33.0), Some(BASE + 12 * 3600)));
        assert_eq!(
            status_message(&s, ts(BASE), &utc()),
            "You've used 32% of your 5-hour session (resets at 22:30) and 33% \
             of your weekly limit (resets Wed 10:13)."
        );
    }

    #[test]
    fn status_message_rounds_percents_like_the_menu_lines() {
        let mut s = snapshot(SnapshotState::Fresh, Some(BASE - 30));
        s.session = Some(metric(Some(32.4), Some(BASE + 1000)));
        s.weekly = Some(metric(Some(32.5), Some(BASE + 1000)));
        assert_eq!(
            status_message(&s, ts(BASE), &utc()),
            "You've used 32% of your 5-hour session (resets at 22:30) and 33% \
             of your weekly limit (resets at 22:30)."
        );
    }

    #[test]
    fn status_message_stale_appends_the_age() {
        let mut s = snapshot(SnapshotState::Stale, Some(BASE - 12 * 3600));
        s.session = Some(metric(Some(32.0), Some(BASE + 1000)));
        s.weekly = Some(metric(Some(33.0), Some(BASE + 12 * 3600)));
        assert_eq!(
            status_message(&s, ts(BASE), &utc()),
            "You've used 32% of your 5-hour session (resets at 22:30) and 33% \
             of your weekly limit (resets Wed 10:13). Last updated 12 h ago."
        );
    }

    #[test]
    fn status_message_stale_without_a_written_at_omits_the_age_clause() {
        let mut s = snapshot(SnapshotState::Stale, None);
        s.session = Some(metric(Some(32.0), None));
        assert_eq!(
            status_message(&s, ts(BASE), &utc()),
            "You've used 32% of your 5-hour session; weekly usage is unknown."
        );
    }

    #[test]
    fn status_message_weekly_missing_says_so() {
        let mut s = snapshot(SnapshotState::Fresh, Some(BASE - 30));
        s.session = Some(metric(Some(32.0), Some(BASE + 1000)));
        assert_eq!(
            status_message(&s, ts(BASE), &utc()),
            "You've used 32% of your 5-hour session (resets at 22:30); weekly \
             usage is unknown."
        );
    }

    #[test]
    fn status_message_session_missing_says_so() {
        let mut s = snapshot(SnapshotState::Fresh, Some(BASE - 30));
        s.weekly = Some(metric(Some(33.0), Some(BASE + 12 * 3600)));
        assert_eq!(
            status_message(&s, ts(BASE), &utc()),
            "You've used 33% of your weekly limit (resets Wed 10:13); session \
             usage is unknown."
        );
    }

    #[test]
    fn status_message_session_percent_absent_counts_as_unknown() {
        // A window present in the cache but with no percentage (an em dash in
        // the menu) is worded the same as no window at all.
        let mut s = snapshot(SnapshotState::Fresh, Some(BASE - 30));
        s.session = Some(metric(None, Some(BASE + 1000)));
        s.weekly = Some(metric(Some(33.0), Some(BASE + 12 * 3600)));
        assert_eq!(
            status_message(&s, ts(BASE), &utc()),
            "You've used 33% of your weekly limit (resets Wed 10:13); session \
             usage is unknown."
        );
    }

    #[test]
    fn status_message_no_metrics_at_all_but_not_missing() {
        // Valid JSON with no `rate_limits` at all (API-key billing): neither
        // window is known, but the hook is demonstrably installed and working.
        let s = snapshot(SnapshotState::Fresh, Some(BASE - 30));
        assert_eq!(status_message(&s, ts(BASE), &utc()), "No usage data reported.");
    }

    #[test]
    fn status_message_reset_without_a_resets_at_omits_the_parenthetical() {
        let mut s = snapshot(SnapshotState::Fresh, Some(BASE - 30));
        s.session = Some(metric(Some(32.0), None));
        s.weekly = Some(metric(Some(33.0), None));
        assert_eq!(
            status_message(&s, ts(BASE), &utc()),
            "You've used 32% of your 5-hour session and 33% of your weekly limit."
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
        n.evaluate(Some(&metric(Some(10.0), Some(BASE)))); // baseline
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
    fn notifier_treats_small_resets_at_drift_as_the_same_window() {
        // The live bug: a source whose reset time crept forward a few seconds
        // per poll re-fired the same alert every tick.
        let mut n = all_on();
        n.evaluate(Some(&metric(Some(10.0), Some(BASE)))); // baseline
        assert!(n.evaluate(Some(&metric(Some(82.0), Some(BASE)))).is_some());
        for tick in 1..=12 {
            assert_eq!(
                n.evaluate(Some(&metric(Some(82.0), Some(BASE + tick * 5)))),
                None,
                "re-fired after {}s of drift",
                tick * 5
            );
        }
    }

    #[test]
    fn notifier_window_tolerance_is_exactly_sixty_seconds() {
        let mut n = all_on();
        n.evaluate(Some(&metric(Some(10.0), Some(BASE)))); // baseline
        assert!(n.evaluate(Some(&metric(Some(82.0), Some(BASE)))).is_some());
        // Sixty seconds either way is still the same window.
        assert_eq!(n.evaluate(Some(&metric(Some(82.0), Some(BASE + 60)))), None);
        assert_eq!(n.evaluate(Some(&metric(Some(82.0), Some(BASE)))), None);
        assert_eq!(n.evaluate(Some(&metric(Some(82.0), Some(BASE - 60)))), None);
        // A second more is a different window.
        let alert = n
            .evaluate(Some(&metric(Some(82.0), Some(BASE + 61))))
            .expect("past the tolerance, this is a rollover");
        assert_eq!(alert.threshold, 75);
    }

    #[test]
    fn notifier_treats_appearing_and_disappearing_reset_times_as_new_windows() {
        let mut n = all_on();
        n.evaluate(Some(&metric(Some(10.0), Some(BASE)))); // baseline
        assert!(n.evaluate(Some(&metric(Some(82.0), Some(BASE)))).is_some());
        // A reading that lost its reset time is not "the same window".
        assert!(n.evaluate(Some(&metric(Some(82.0), None))).is_some());
        assert_eq!(n.evaluate(Some(&metric(Some(82.0), None))), None);
        assert!(n.evaluate(Some(&metric(Some(82.0), Some(BASE)))).is_some());
    }

    #[test]
    fn notifier_baselines_on_the_first_reading_instead_of_firing() {
        // Restarting the tray at 82% must not re-announce the 75% crossing
        // that happened before it started.
        let mut n = all_on();
        assert_eq!(n.evaluate(Some(&metric(Some(82.0), Some(BASE)))), None);
        // Nor on any later reading that stays inside the same band.
        assert_eq!(n.evaluate(Some(&metric(Some(82.0), Some(BASE)))), None);
        assert_eq!(n.evaluate(Some(&metric(Some(89.9), Some(BASE)))), None);

        // A crossing the tray actually witnesses still fires, and only the
        // one that was crossed.
        let alert = n
            .evaluate(Some(&metric(Some(91.0), Some(BASE))))
            .expect("90 was crossed while watching");
        assert_eq!(alert.threshold, 90);
    }

    #[test]
    fn notifier_baseline_still_re_arms_on_a_drop_and_re_rise() {
        let mut n = all_on();
        assert_eq!(n.evaluate(Some(&metric(Some(82.0), Some(BASE)))), None);
        // Below 75 again: that threshold is armed even though its "crossing"
        // was never announced.
        assert_eq!(n.evaluate(Some(&metric(Some(70.0), Some(BASE)))), None);
        let alert = n
            .evaluate(Some(&metric(Some(76.0), Some(BASE))))
            .expect("a real crossing after the baseline");
        assert_eq!(alert.threshold, 75);
    }

    #[test]
    fn notifier_baselines_off_the_first_real_percentage_not_off_the_absence() {
        // A tray started before Claude Code has ever reported: the readings
        // without a percentage must not consume the baseline, or the first
        // real one (already at 82%) would fire.
        let mut n = all_on();
        assert_eq!(n.evaluate(None), None);
        assert_eq!(n.evaluate(Some(&metric(None, None))), None);
        assert_eq!(n.evaluate(Some(&metric(None, Some(BASE)))), None);
        assert_eq!(n.evaluate(Some(&metric(Some(82.0), Some(BASE)))), None);
        // ...and from there it behaves normally.
        let alert = n
            .evaluate(Some(&metric(Some(99.0), Some(BASE))))
            .expect("a witnessed crossing");
        assert_eq!(alert.threshold, 99);
    }

    #[test]
    fn notifier_ignores_missing_data_without_rearming() {
        let mut n = all_on();
        n.evaluate(Some(&metric(Some(10.0), Some(BASE)))); // baseline
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
        n.evaluate(Some(&metric(Some(10.0), Some(BASE)))); // baseline
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
        // The startup baseline, which is silent whatever it reads.
        assert_eq!(n.evaluate(Some(&metric(Some(95.0), Some(BASE)))), None);
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
    fn pinned_styles_are_named_for_the_ui_not_for_the_icon() {
        // "mono-dark" = my UI is dark = light icon, and vice versa. The portal
        // value is irrelevant for both, whichever way it points.
        for portal_dark in [true, false] {
            assert_eq!(
                resolve_appearance(IconStyle::MonoDark, portal_dark),
                IconAppearance::Mono { dark_ui: true }
            );
            assert_eq!(
                resolve_appearance(IconStyle::MonoLight, portal_dark),
                IconAppearance::Mono { dark_ui: false }
            );
            assert_eq!(
                resolve_appearance(IconStyle::Color, portal_dark),
                IconAppearance::Color
            );
        }
    }

    #[test]
    fn auto_follows_the_portal() {
        assert_eq!(
            resolve_appearance(IconStyle::MonoAuto, true),
            IconAppearance::Mono { dark_ui: true }
        );
        assert_eq!(
            resolve_appearance(IconStyle::MonoAuto, false),
            IconAppearance::Mono { dark_ui: false }
        );
    }

    #[test]
    fn the_appearance_handle_defaults_to_a_dark_ui_until_the_portal_speaks() {
        let handle = AppearanceHandle::new(IconStyle::MonoAuto);
        assert_eq!(handle.resolved(), IconAppearance::Mono { dark_ui: true });
    }

    #[test]
    fn appearance_changes_report_whether_a_repaint_is_needed() {
        let handle = AppearanceHandle::new(IconStyle::Color);
        assert!(!handle.set_style(IconStyle::Color), "no-op selection");
        assert!(handle.set_style(IconStyle::MonoAuto));
        assert_eq!(handle.resolved(), IconAppearance::Mono { dark_ui: true });

        // Under auto, a portal change repaints.
        assert!(handle.set_portal_dark(false));
        assert_eq!(handle.resolved(), IconAppearance::Mono { dark_ui: false });
        assert!(!handle.set_portal_dark(false), "same value again");

        // Two styles that resolve identically need no repaint: auto currently
        // says "light UI", which is exactly what mono-light pins.
        assert!(!handle.set_style(IconStyle::MonoLight));
        // ...and once pinned, the portal no longer moves the icon.
        assert!(!handle.set_portal_dark(true));
        assert_eq!(handle.resolved(), IconAppearance::Mono { dark_ui: false });
    }

    #[test]
    fn the_tray_renders_with_the_configured_style() {
        let settings = Settings::new(
            Config {
                icon_style: IconStyle::MonoLight,
                ..Config::default()
            },
            None,
        );
        assert_eq!(
            settings.appearance_handle().resolved(),
            IconAppearance::Mono { dark_ui: false }
        );
    }

    /// The macOS backend asks the core which appearance it is about to render
    /// in, and turns the answer into "is this an AppKit template image". If
    /// this ever stopped tracking the setting, the menu bar icon would either
    /// lose its colors or stop following the system theme.
    #[test]
    fn the_core_reports_the_appearance_it_renders_with() {
        for (style, expected) in [
            (IconStyle::Color, IconAppearance::Color),
            (IconStyle::MonoDark, IconAppearance::Mono { dark_ui: true }),
            (IconStyle::MonoLight, IconAppearance::Mono { dark_ui: false }),
        ] {
            let (core, _rx) = core_for(
                timeless(SnapshotState::Fresh),
                Config {
                    icon_style: style,
                    ..Config::default()
                },
                None,
            );
            assert_eq!(core.appearance(), expected, "{style:?}");
        }
    }

    #[test]
    fn no_update_is_known_until_the_checker_reports_one() {
        // The menu row's presence is exactly "is there something in the slot",
        // so that is what this pins.
        let settings = Settings::new(Config::default(), None);
        let handle = settings.update_handle();
        assert_eq!(handle.get(), None);

        let found = Update {
            version: "0.2.0".into(),
            url: "https://example.test/releases/tag/v0.2.0".into(),
        };
        handle.set(Some(found.clone()));
        assert_eq!(settings.update.get(), Some(found.clone()));
        assert_eq!(found.label(), "⬆ Update available: v0.2.0");

        // A later check that finds nothing clears the row again.
        handle.set(None);
        assert_eq!(settings.update.get(), None);
    }

    #[test]
    fn the_update_check_flag_starts_from_the_config() {
        for enabled in [true, false] {
            let settings = Settings::new(
                Config {
                    check_updates: enabled,
                    ..Config::default()
                },
                None,
            );
            assert_eq!(
                settings.check_updates_handle().load(Ordering::Relaxed),
                enabled
            );
        }
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

    // ---- the menu model -------------------------------------------------
    //
    // The rows are what the user actually sees, and every backend renders
    // them verbatim, so these pin the model rather than any one platform's
    // menu API.

    /// A core with a channel kept alive (dropping the receiver would make
    /// every `send` a silent no-op, which these tests do not exercise).
    fn core_for(
        snapshot: UsageSnapshot,
        config: Config,
        env_secs: Option<u64>,
    ) -> (TrayCore, std::sync::mpsc::Receiver<Wake>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            TrayCore::new(snapshot, Settings::new(config, env_secs), tx),
            rx,
        )
    }

    /// Everything available: the state a healthy desktop reports.
    fn all_available() -> MenuEnv {
        MenuEnv {
            can_persist: true,
            autostart_available: true,
            autostart_enabled: false,
        }
    }

    /// One row, flattened to something a test can assert on. Separators and
    /// radio groups have no label of their own.
    fn label_of(row: &MenuRow) -> String {
        match row {
            MenuRow::Info { label }
            | MenuRow::Action { label, .. }
            | MenuRow::Check { label, .. }
            | MenuRow::SubMenu { label, .. } => label.clone(),
            MenuRow::Radio { .. } => "<radio>".to_string(),
            MenuRow::Separator => "---".to_string(),
        }
    }

    fn labels(rows: &[MenuRow]) -> Vec<String> {
        rows.iter().map(label_of).collect()
    }

    fn submenu<'a>(rows: &'a [MenuRow], label: &str) -> &'a [MenuRow] {
        rows.iter()
            .find_map(|row| match row {
                MenuRow::SubMenu { label: l, rows } if l == label => Some(rows.as_slice()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {label:?} submenu"))
    }

    /// A snapshot with percentages but no reset times, so the labels do not
    /// depend on the machine's time zone.
    fn timeless(state: SnapshotState) -> UsageSnapshot {
        UsageSnapshot {
            session: Some(metric(Some(42.0), None)),
            weekly: Some(metric(Some(61.0), None)),
            written_at: Some(ts(BASE - 60)),
            state,
        }
    }

    #[test]
    fn the_menu_opens_with_the_three_info_rows_and_ends_with_the_actions() {
        let (core, _rx) = core_for(timeless(SnapshotState::Fresh), Config::default(), None);
        let rows = core.menu_with(ts(BASE), all_available());
        assert_eq!(
            labels(&rows),
            vec![
                "Session: 42%",
                "Weekly: 61%",
                "Updated 1 min ago",
                "---",
                "Settings",
                "Check for new data",
                "Quit",
            ]
        );
        // The first three are labels, not controls.
        assert!(matches!(rows[0], MenuRow::Info { .. }));
        assert!(matches!(rows[1], MenuRow::Info { .. }));
        assert!(matches!(rows[2], MenuRow::Info { .. }));
        assert_eq!(
            rows[5],
            MenuRow::action("Check for new data", MenuAction::Refresh)
        );
        assert_eq!(rows[6], MenuRow::action("Quit", MenuAction::Quit));
    }

    #[test]
    fn the_install_row_appears_only_while_there_is_no_data() {
        for state in [SnapshotState::Fresh, SnapshotState::Stale] {
            let (core, _rx) = core_for(timeless(state.clone()), Config::default(), None);
            let rows = core.menu_with(ts(BASE), all_available());
            assert!(
                !labels(&rows).contains(&"Install hook".to_string()),
                "{state:?} must not offer the install item"
            );
        }

        let (core, _rx) = core_for(
            snapshot(SnapshotState::Missing, None),
            Config::default(),
            None,
        );
        let rows = core.menu_with(ts(BASE), all_available());
        // Directly under the diagnosis row it explains.
        assert_eq!(
            rows[3],
            MenuRow::action("Install hook", MenuAction::InstallHook)
        );
    }

    #[test]
    fn the_update_row_appears_once_a_release_is_found() {
        let (core, _rx) = core_for(timeless(SnapshotState::Fresh), Config::default(), None);
        assert!(
            !labels(&core.menu_with(ts(BASE), all_available()))
                .iter()
                .any(|label| label.contains("Update available")),
            "nothing to advertise before the checker runs"
        );

        let update = Update {
            version: "0.2.0".into(),
            url: "https://example.test/releases/tag/v0.2.0".into(),
        };
        core.settings.update.set(Some(update.clone()));
        let rows = core.menu_with(ts(BASE), all_available());
        // Between the separator and the `Settings` submenu, and clickable.
        assert_eq!(
            rows[4],
            MenuRow::action(update.label(), MenuAction::OpenUrl(update.url.clone()))
        );
    }

    #[test]
    fn the_settings_submenu_lists_every_control_in_order() {
        let (core, _rx) = core_for(timeless(SnapshotState::Fresh), Config::default(), None);
        let rows = core.menu_with(ts(BASE), all_available());
        assert_eq!(
            labels(submenu(&rows, "Settings")),
            vec![
                "Launch at login",
                "---",
                "Notifications",
                "---",
                "Refresh interval",
                "<radio>",
                "---",
                "Icon style",
                "<radio>",
                "---",
                "Check for updates",
            ]
        );
    }

    #[test]
    fn the_radio_groups_offer_every_choice_and_mark_the_configured_one() {
        let config = Config {
            refresh_secs: REFRESH_CHOICES[1],
            icon_style: IconStyle::MonoLight,
            ..Config::default()
        };
        let (core, _rx) = core_for(timeless(SnapshotState::Fresh), config, None);
        let rows = core.menu_with(ts(BASE), all_available());
        let settings = submenu(&rows, "Settings");

        match &settings[5] {
            MenuRow::Radio {
                group,
                selected,
                options,
            } => {
                assert_eq!(*group, RadioGroup::RefreshInterval);
                assert_eq!(*selected, 1);
                assert_eq!(options.len(), REFRESH_CHOICES.len());
                assert_eq!(options[0].label, format!("{} s", REFRESH_CHOICES[0]));
            }
            other => panic!("expected the interval radio group, got {other:?}"),
        }
        match &settings[8] {
            MenuRow::Radio {
                group,
                selected,
                options,
            } => {
                assert_eq!(*group, RadioGroup::IconStyle);
                assert_eq!(*selected, IconStyle::MonoLight.choice());
                assert_eq!(options.len(), IconStyle::ALL.len());
            }
            other => panic!("expected the icon-style radio group, got {other:?}"),
        }
    }

    #[test]
    fn the_notifications_submenu_mirrors_the_configured_thresholds() {
        let mut config = Config::default();
        config.set_notifies_at(NOTIFY_THRESHOLDS[0], false);
        config.notify_on_reset = false;
        let (core, _rx) = core_for(timeless(SnapshotState::Fresh), config, None);
        let rows = core.menu_with(ts(BASE), all_available());
        let notifications = submenu(submenu(&rows, "Settings"), "Notifications");

        assert_eq!(notifications.len(), NOTIFY_THRESHOLDS.len() + 2);
        assert_eq!(
            notifications[0],
            MenuRow::Check {
                label: format!("At {}%", NOTIFY_THRESHOLDS[0]),
                action: MenuAction::ToggleThreshold(NOTIFY_THRESHOLDS[0]),
                checked: false,
                enabled: true,
            }
        );
        assert_eq!(
            notifications[1],
            MenuRow::Check {
                label: format!("At {}%", NOTIFY_THRESHOLDS[1]),
                action: MenuAction::ToggleThreshold(NOTIFY_THRESHOLDS[1]),
                checked: true,
                enabled: true,
            }
        );
        assert!(matches!(
            notifications[NOTIFY_THRESHOLDS.len()],
            MenuRow::Separator
        ));
        assert_eq!(
            notifications[NOTIFY_THRESHOLDS.len() + 1],
            MenuRow::Check {
                label: "When quota resets".into(),
                action: MenuAction::ToggleNotifyOnReset,
                checked: false,
                enabled: true,
            }
        );
    }

    #[test]
    fn an_unwritable_config_grays_every_persisted_control() {
        let (core, _rx) = core_for(timeless(SnapshotState::Fresh), Config::default(), None);
        let env = MenuEnv {
            can_persist: false,
            autostart_available: true,
            autostart_enabled: true,
        };
        let rows = core.menu_with(ts(BASE), env);
        let settings = submenu(&rows, "Settings");

        // Autostart is a separate capability: it stays usable, and reports the
        // state read from the platform rather than from the config mirror.
        assert_eq!(
            settings[0],
            MenuRow::Check {
                label: "Launch at login".into(),
                action: MenuAction::ToggleLaunchAtLogin,
                checked: true,
                enabled: true,
            }
        );
        for row in [&settings[5], &settings[8]] {
            match row {
                MenuRow::Radio { options, .. } => {
                    assert!(options.iter().all(|option| !option.enabled));
                }
                other => panic!("expected a radio group, got {other:?}"),
            }
        }
        for row in submenu(settings, "Notifications") {
            if let MenuRow::Check { enabled, label, .. } = row {
                assert!(!enabled, "{label} should be grayed");
            }
        }
        match &settings[10] {
            MenuRow::Check { label, enabled, .. } => {
                assert_eq!(label, "Check for updates");
                assert!(!enabled);
            }
            other => panic!("expected the update checkbox, got {other:?}"),
        }
    }

    #[test]
    fn an_unavailable_autostart_directory_grays_only_that_checkbox() {
        let (core, _rx) = core_for(timeless(SnapshotState::Fresh), Config::default(), None);
        let env = MenuEnv {
            can_persist: true,
            autostart_available: false,
            autostart_enabled: false,
        };
        let settings = submenu(&core.menu_with(ts(BASE), env), "Settings").to_vec();
        match &settings[0] {
            MenuRow::Check { enabled, .. } => assert!(!enabled),
            other => panic!("expected the autostart checkbox, got {other:?}"),
        }
        match &settings[10] {
            MenuRow::Check { enabled, .. } => assert!(enabled, "the rest stays usable"),
            other => panic!("expected the update checkbox, got {other:?}"),
        }
    }

    #[test]
    fn the_env_override_note_appears_only_while_the_environment_wins() {
        let (core, _rx) = core_for(timeless(SnapshotState::Fresh), Config::default(), None);
        assert!(
            !labels(submenu(&core.menu_with(ts(BASE), all_available()), "Settings"))
                .iter()
                .any(|label| label.contains("CLAUDE_TRAY_POLL_SECS"))
        );

        let (core, _rx) = core_for(timeless(SnapshotState::Fresh), Config::default(), Some(7));
        let settings = submenu(&core.menu_with(ts(BASE), all_available()), "Settings").to_vec();
        // Right after the icon-style group, before the final separator.
        assert_eq!(
            settings[9],
            MenuRow::info("(CLAUDE_TRAY_POLL_SECS=7 is in effect)")
        );
    }
}
