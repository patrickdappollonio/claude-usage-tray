//! Usage snapshot model + statusline cache-file reader (cache contract v2).
//!
//! The cache file is whatever Claude Code handed our `statusline` subcommand on
//! stdin, written back out **verbatim** — no reshaping, no wrapper object, no
//! `written_at` field. Two consequences follow:
//!
//! * freshness comes from the file's **mtime**, not from anything inside it;
//! * the parser has to pick `rate_limits` out of a document that also carries
//!   `model`, `workspace`, `cost` and whatever else Claude Code adds next, and
//!   ignore all of it.
//!
//! See `docs/superpowers/specs/2026-08-13-claude-usage-tray-design.md`. This
//! module never panics on bad input: every failure mode maps to a
//! `SnapshotState` value.

use std::io;
use std::path::{Path, PathBuf};

/// Freshness classification for a [`UsageSnapshot`].
#[derive(Clone, Debug, Default, PartialEq)]
pub enum SnapshotState {
    /// Cache file read and parsed; its mtime is within the staleness window.
    Fresh,
    /// Cache file read and parsed, but its mtime is older than the staleness
    /// threshold.
    Stale,
    /// Cache file missing, unreadable, or its JSON could not be parsed.
    #[default]
    Missing,
}

/// One rate-limit window (5-hour session or 7-day weekly).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Metric {
    pub percent: Option<f64>,
    pub resets_at: Option<jiff::Timestamp>,
}

/// The `rate_limits` half of a statusline document, once extracted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RateLimits {
    pub session: Option<Metric>,
    pub weekly: Option<Metric>,
}

/// A point-in-time read of the tray's usage sources. Historically this was
/// the statusline cache alone; it can now also be the merge of that cache
/// with Claude Code's own `cachedUsageUtilization` blob (see
/// [`merge_snapshot`] and `appcache.rs`).
#[derive(Clone, Debug, Default)]
pub struct UsageSnapshot {
    pub session: Option<Metric>,
    pub weekly: Option<Metric>,
    /// Per-model weekly buckets ("Fable at 71%"). Only the app cache carries
    /// these — a hook-only snapshot always has an empty list.
    pub scoped: Vec<crate::appcache::ScopedMetric>,
    /// When Claude Code fetched the app-cache blob the `scoped` rows came
    /// from. Carried separately from `written_at` because the scoped rows can
    /// be older than the winning session/weekly source, and the menu says so.
    pub scoped_fetched_at: Option<jiff::Timestamp>,
    /// When the winning source last reported — the statusline cache file's
    /// mtime, or the app cache's `fetchedAtMs`, whichever won the merge. The
    /// name is kept because that is still exactly what it means to every
    /// reader ("Updated 3 min ago", "Last updated 12 h ago").
    pub written_at: Option<jiff::Timestamp>,
    pub state: SnapshotState,
    /// Whether the statusline hook's cache file was missing or unparseable.
    /// Distinct from `state` now that app-cache data can make the snapshot
    /// `Fresh` while the hook is still not installed — the "Install hook"
    /// menu item keys off this, not off `state`.
    pub hook_missing: bool,
}

impl UsageSnapshot {
    fn missing() -> Self {
        UsageSnapshot {
            hook_missing: true,
            ..UsageSnapshot::default()
        }
    }
}

/// Cache files whose mtime is older than this many seconds are stale.
const STALE_THRESHOLD_SECS: i64 = 600;

/// Name of the contract-v2 cache file inside the Claude config directory.
pub const CACHE_FILE_NAME: &str = "usage-tray-statusline.json";

/// Name of the obsolete contract-v1 cache file, kept only so that `hook
/// install` can delete it.
pub const LEGACY_CACHE_FILE_NAME: &str = "usage-tray-cache.json";

/// The Claude Code config directory: `$CLAUDE_CONFIG_DIR` when set, else
/// `~/.claude`.
pub fn claude_config_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".claude"),
    }
}

/// Cache path inside an arbitrary config directory. Parameterized so tests
/// never touch the real `~/.claude`.
pub fn cache_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join(CACHE_FILE_NAME)
}

/// Default cache file location:
/// `${CLAUDE_CONFIG_DIR:-~/.claude}/usage-tray-statusline.json`.
pub fn default_cache_path() -> PathBuf {
    cache_path_in(&claude_config_dir())
}

/// Writes the raw statusline bytes to `path` atomically: temp file in the same
/// directory, then rename, so the tray never reads a half-written document.
/// Creates the parent directory if needed.
pub fn write_cache(path: &Path, body: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, body)?;
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// Whether a document (raw statusline bytes) has a non-null `rate_limits`
/// key. Malformed JSON and an explicit `null` both count as "lacks it".
fn has_rate_limits(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    matches!(value.get("rate_limits"), Some(v) if !v.is_null())
}

/// Decides whether the `statusline` subcommand should overwrite the cache
/// file with the incoming statusline document.
///
/// A freshly started interactive session paints its statusline before its
/// first turn, with `rate_limits` absent or null. Writing that payload
/// verbatim would clobber a previous cache that *did* have real usage data,
/// and the tray would show "no data" until the session's first real turn.
///
/// So: skip the write only when the incoming payload lacks `rate_limits` and
/// an existing cache is present and *does* have it — that's the one case
/// where writing loses information. Every other case writes: there's no
/// cache yet, the incoming payload carries real data, or the existing cache
/// was equally empty (an empty-but-present cache is still better than none
/// for first-run UX, and mtime freshness should still tick forward).
pub fn should_write_cache(incoming: &[u8], existing: Option<&[u8]>) -> bool {
    if has_rate_limits(incoming) {
        return true;
    }
    match existing {
        Some(existing) => !has_rate_limits(existing),
        None => true,
    }
}

/// The cache file's mtime, or `None` when it cannot be read.
pub fn cache_mtime(path: &Path) -> Option<jiff::Timestamp> {
    let modified = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()?;
    let secs = match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_secs()).ok()?,
        // A pre-1970 mtime is nonsense in practice, but it must not panic.
        Err(before) => -i64::try_from(before.duration().as_secs()).ok()?,
    };
    jiff::Timestamp::from_second(secs).ok()
}

/// Reads and parses the cache file at `path`. Never panics: any I/O error
/// (missing file, permissions, etc.) yields a `Missing` snapshot.
pub fn read_snapshot(path: &Path, now: jiff::Timestamp) -> UsageSnapshot {
    match std::fs::read_to_string(path) {
        Ok(body) => snapshot_from(&body, cache_mtime(path), now),
        Err(_) => UsageSnapshot::missing(),
    }
}

/// Name of the "kayfabe" fixture file: wrestling jargon for a staged event
/// presented as real, which is exactly the joke here. When this file exists
/// next to `config.toml`, the tray presents its contents as the real usage
/// snapshot — handy for demoing every visual state (fresh/stale/missing,
/// arbitrary percentages, upcoming resets) without waiting on real usage data
/// or hand-editing the real cache. Delete it and reality resumes on the next
/// poll tick. Deliberately unadvertised: not in `--help`, not in the README,
/// source-visible only.
pub const KAYFABE_FILE_NAME: &str = "kayfabe.json";

/// Default kayfabe fixture path: `<config dir>/claude-usage-tray/kayfabe.json`
/// — the same directory `config.toml` lives in, so it inherits the same
/// `$XDG_CONFIG_HOME` resolution.
pub fn default_kayfabe_path() -> PathBuf {
    crate::config::config_dir().join(KAYFABE_FILE_NAME)
}

/// Builds a snapshot from a `kayfabe.json` fixture body. Pure and
/// unit-testable independent of any filesystem probing.
///
/// * `session`/`weekly` (0-100, omitted → `None`) become each metric's
///   `percent` directly, with no other validation.
/// * `age_minutes` (default 0) sets `written_at = now - age_minutes` minutes;
///   staleness is classified by the same 600 s rule as real cache reads, so
///   720 renders as "Stale" ("12 h ago").
/// * `session_resets_in_minutes` (default 180) and `weekly_resets_in_days`
///   (default 4) set each metric's `resets_at` relative to the fixture file's
///   **mtime**. Both accept negative values (a reset already in the past).
///
/// The two anchors are different on purpose. `written_at` follows `now`, so a
/// staged `age_minutes` keeps meaning what it says on every poll instead of
/// drifting into staleness while the demo is being looked at. `resets_at`
/// follows the file's mtime, so it is *stable* for an unedited file: anchoring
/// it to `now` moved every reset time forward a few seconds per tick, and the
/// threshold notifier reads a changed `resets_at` as "a new 5-hour window
/// began" — which re-armed every threshold and re-fired the alert on every
/// single poll. An mtime-anchored reset only moves when the fixture is
/// actually edited, which is exactly when a re-arm is wanted. A file whose
/// mtime cannot be read falls back to `now`, which is no worse than the old
/// behaviour.
///
/// Unlike `parse_metric`, this deliberately does **not** zero a percent whose
/// `resets_at` has already passed: the fake file is authoritative for every
/// field it sets, including a percent paired with a past reset, so that
/// combination can itself be used to demo the real zeroing rule's *absence*
/// or exercised end to end by simply setting `session_resets_in_minutes`
/// negative alongside `session: 0`.
///
/// Missing/unreadable is handled by the caller ([`read_merged_or_kayfabe`]);
/// this function only has to decide `Missing` for unparseable JSON.
pub fn fake_snapshot(
    body: &str,
    mtime: Option<jiff::Timestamp>,
    now: jiff::Timestamp,
) -> UsageSnapshot {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return UsageSnapshot::missing();
    };
    // Plain `serde_json::Value` lookups rather than a derived struct: `serde`
    // isn't a direct dependency (only `serde_json`/`toml` pull it in), and
    // this fixture format is small enough not to earn one.
    let field_f64 = |key: &str| value.get(key).and_then(|v| v.as_f64());
    let field_i64 = |key: &str| value.get(key).and_then(|v| v.as_i64());

    let age_minutes = field_i64("age_minutes").unwrap_or(0);
    let written_at = jiff::Timestamp::from_second(now.as_second() - age_minutes * 60).ok();

    let session_resets_in_minutes = field_i64("session_resets_in_minutes").unwrap_or(180);
    let weekly_resets_in_days = field_i64("weekly_resets_in_days").unwrap_or(4);
    // The stable anchor: see the note above on why this is not `now`.
    let anchor = mtime.unwrap_or(now).as_second();
    let session_resets_at =
        jiff::Timestamp::from_second(anchor + session_resets_in_minutes * 60).ok();
    let weekly_resets_at =
        jiff::Timestamp::from_second(anchor + weekly_resets_in_days * 86400).ok();

    UsageSnapshot {
        session: Some(Metric {
            percent: field_f64("session"),
            resets_at: session_resets_at,
        }),
        weekly: Some(Metric {
            percent: field_f64("weekly"),
            resets_at: weekly_resets_at,
        }),
        // An optional staged per-model row: `"fable": 71` puts "Fable" at
        // 71%, sharing the weekly reset. Anchoring its fetched-at to
        // `written_at` keeps the "as of" age suffix obeying `age_minutes`
        // instead of growing while the demo is on screen.
        scoped: field_f64("fable")
            .map(|percent| {
                vec![crate::appcache::ScopedMetric {
                    name: "Fable".to_string(),
                    metric: Metric {
                        percent: Some(percent),
                        resets_at: weekly_resets_at,
                    },
                }]
            })
            .unwrap_or_default(),
        scoped_fetched_at: written_at,
        written_at,
        state: classify(written_at, now),
        ..UsageSnapshot::default()
    }
}

/// The read the tray actually polls. Probes `kayfabe_path` first (a cheap
/// stat + read on every poll tick, so no state to go stale across the process
/// lifetime):
///
/// * kayfabe file absent → the real path: the statusline hook cache
///   ([`read_snapshot`]) merged with Claude Code's `.claude.json` usage blob
///   via [`merge_snapshot`].
/// * kayfabe file present and readable → its contents go through
///   [`fake_snapshot`] and *become* the snapshot, both real sources ignored.
/// * kayfabe file present but unreadable (permissions, race, etc.) →
///   `Missing`, same as a missing real cache would be.
pub fn read_merged_or_kayfabe(
    cache_path: &Path,
    app_cache_path: &Path,
    kayfabe_path: &Path,
    now: jiff::Timestamp,
) -> UsageSnapshot {
    match std::fs::read_to_string(kayfabe_path) {
        Ok(body) => fake_snapshot(&body, cache_mtime(kayfabe_path), now),
        Err(err) if err.kind() == io::ErrorKind::NotFound => merge_snapshot(
            read_snapshot(cache_path, now),
            crate::appcache::read_app_usage(app_cache_path, now),
            now,
        ),
        Err(_) => UsageSnapshot::missing(),
    }
}

/// Merges the statusline-hook snapshot with Claude Code's own app-cache
/// usage blob into the one snapshot the tray displays.
///
/// The rule, agreed with the user: freshest source wins, per metric.
///
/// * `session`/`weekly`: taken from whichever source reported more recently
///   (hook mtime vs `fetchedAtMs`); a metric the winner lacks falls back to
///   the other source rather than disappearing.
/// * `scoped` (the per-model "Fable" buckets): always from the app cache —
///   the hook never carries them, so "newest wins" would silently drop them
///   whenever the hook is fresher, which is nearly always.
/// * `written_at`/`state`: the winning source's timestamp, classified by the
///   same staleness rule as before.
/// * "No data" only when neither source yields anything.
pub fn merge_snapshot(
    hook: UsageSnapshot,
    app: Option<crate::appcache::AppUsage>,
    now: jiff::Timestamp,
) -> UsageSnapshot {
    let Some(app) = app else {
        return hook;
    };
    let hook_missing = hook.state == SnapshotState::Missing;

    // An undated app cache can't win a freshness contest, but a dated one
    // beats a hook with no known mtime: data of known age over data of none.
    let app_newer = match (app.fetched_at, hook.written_at) {
        (Some(app_at), Some(hook_at)) => app_at > hook_at,
        (Some(_), None) => true,
        (None, _) => false,
    };
    let (session, weekly, written_at) = if app_newer {
        (
            app.session.or(hook.session),
            app.weekly.or(hook.weekly),
            app.fetched_at,
        )
    } else {
        (
            hook.session.or(app.session),
            hook.weekly.or(app.weekly),
            hook.written_at.or(app.fetched_at),
        )
    };

    let no_data_at_all =
        hook_missing && session.is_none() && weekly.is_none() && app.scoped.is_empty();
    let state = if no_data_at_all {
        SnapshotState::Missing
    } else {
        classify(written_at, now)
    };

    UsageSnapshot {
        session,
        weekly,
        scoped: app.scoped,
        scoped_fetched_at: app.fetched_at,
        written_at,
        state,
        hook_missing,
    }
}

/// Classifies freshness from the cache file's mtime. An unknown mtime (an
/// exotic filesystem, a race with a rename) is treated as fresh rather than as
/// no-data: the content was read successfully, and claiming "no hook
/// installed" because of a stat failure would be a worse lie than showing
/// data of unknown age.
pub fn classify(written_at: Option<jiff::Timestamp>, now: jiff::Timestamp) -> SnapshotState {
    match written_at {
        Some(at) if now.as_second() - at.as_second() > STALE_THRESHOLD_SECS => SnapshotState::Stale,
        _ => SnapshotState::Fresh,
    }
}

/// Builds a snapshot from a raw statusline document plus the file's mtime.
/// Unparseable JSON is `Missing`; valid JSON without `rate_limits` is a
/// perfectly good (if empty) reading, because that is what Claude Code sends
/// on API-key billing.
pub fn snapshot_from(
    body: &str,
    written_at: Option<jiff::Timestamp>,
    now: jiff::Timestamp,
) -> UsageSnapshot {
    let Some(limits) = parse_statusline_json(body, now) else {
        return UsageSnapshot::missing();
    };
    UsageSnapshot {
        session: limits.session,
        weekly: limits.weekly,
        written_at,
        state: classify(written_at, now),
        ..UsageSnapshot::default()
    }
}

/// Pure parser for the raw statusline JSON: pulls `rate_limits.five_hour` and
/// `rate_limits.seven_day` out and ignores every other key in the document.
/// `None` means "this is not JSON at all"; everything softer than that
/// degrades to absent fields.
pub fn parse_statusline_json(body: &str, now: jiff::Timestamp) -> Option<RateLimits> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let rate_limits = value.get("rate_limits");
    Some(RateLimits {
        session: rate_limits
            .and_then(|rl| rl.get("five_hour"))
            .map(|m| parse_metric(m, now)),
        weekly: rate_limits
            .and_then(|rl| rl.get("seven_day"))
            .map(|m| parse_metric(m, now)),
    })
}

/// Parses a single rate-limit window object (`{"used_percentage": .., "resets_at": ..}`).
/// A `resets_at` in the past forces `percent` to `0.0` (the window rolled over
/// while no session was running).
fn parse_metric(value: &serde_json::Value, now: jiff::Timestamp) -> Metric {
    let percent = value.get("used_percentage").and_then(|v| v.as_f64());
    let resets_at = value
        .get("resets_at")
        .and_then(|v| v.as_i64())
        .and_then(|secs| jiff::Timestamp::from_second(secs).ok());

    let percent = match (percent, resets_at) {
        (Some(p), Some(r)) if r < now && p != 0.0 => Some(0.0),
        (p, _) => p,
    };

    Metric { percent, resets_at }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::path::Path;

    fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/cache")
            .join(name)
    }

    fn read_fixture(name: &str) -> String {
        std::fs::read_to_string(fixture_path(name)).expect("fixture must exist")
    }

    fn ts(secs: i64) -> jiff::Timestamp {
        jiff::Timestamp::from_second(secs).expect("valid timestamp")
    }

    /// Writes `body` to a file inside `dir` and stamps its mtime, so the
    /// mtime-based staleness rules can be tested without sleeping.
    fn write_with_mtime(dir: &Path, name: &str, body: &str, mtime_secs: u64) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write cache");
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open for set_times");
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime_secs);
        file.set_times(std::fs::FileTimes::new().set_modified(when))
            .expect("set mtime");
        path
    }

    #[test]
    fn valid_full_statusline_json_yields_both_metrics() {
        let body = read_fixture("valid_full.json");
        let now = ts(1700000000 + 100);
        let snap = snapshot_from(&body, Some(ts(1700000000)), now);

        assert_eq!(snap.state, SnapshotState::Fresh);
        assert_eq!(snap.written_at, Some(ts(1700000000)));

        let session = snap.session.expect("session metric present");
        assert_eq!(session.percent, Some(42.0));
        assert_eq!(session.resets_at, Some(ts(1700018000)));

        let weekly = snap.weekly.expect("weekly metric present");
        assert_eq!(weekly.percent, Some(61.5));
        assert_eq!(weekly.resets_at, Some(ts(1700600000)));
    }

    #[test]
    fn the_other_statusline_keys_are_ignored() {
        // The fixture is a real statusline document: model, workspace, cost,
        // output_style. None of them may leak into the snapshot or upset the
        // parse.
        let body = read_fixture("valid_full.json");
        assert!(body.contains("\"model\""), "fixture must be realistic");
        assert!(body.contains("\"workspace\""), "fixture must be realistic");
        let snap = snapshot_from(&body, Some(ts(1700000000)), ts(1700000000));
        assert_eq!(snap.state, SnapshotState::Fresh);
        assert!(snap.session.is_some());
    }

    #[test]
    fn mtime_older_than_600s_is_stale() {
        let body = read_fixture("valid_full.json");
        let snap = snapshot_from(&body, Some(ts(1700000000)), ts(1700000000 + 601));
        assert_eq!(snap.state, SnapshotState::Stale);
    }

    #[test]
    fn mtime_exactly_600s_old_is_still_fresh() {
        let body = read_fixture("valid_full.json");
        let snap = snapshot_from(&body, Some(ts(1700000000)), ts(1700000000 + 600));
        assert_eq!(snap.state, SnapshotState::Fresh);
    }

    #[test]
    fn an_unknown_mtime_is_treated_as_fresh_rather_than_missing() {
        let body = read_fixture("valid_full.json");
        let snap = snapshot_from(&body, None, ts(1700000000));
        assert_eq!(snap.state, SnapshotState::Fresh);
        assert_eq!(snap.written_at, None);
        assert!(snap.session.is_some());
    }

    fn hook_snapshot(written_at: i64, session: f64, weekly: f64) -> UsageSnapshot {
        UsageSnapshot {
            session: Some(Metric {
                percent: Some(session),
                resets_at: None,
            }),
            weekly: Some(Metric {
                percent: Some(weekly),
                resets_at: None,
            }),
            written_at: Some(ts(written_at)),
            state: SnapshotState::Fresh,
            ..UsageSnapshot::default()
        }
    }

    fn app_usage(fetched_at: i64, session: f64, weekly: f64) -> crate::appcache::AppUsage {
        crate::appcache::AppUsage {
            session: Some(Metric {
                percent: Some(session),
                resets_at: None,
            }),
            weekly: Some(Metric {
                percent: Some(weekly),
                resets_at: None,
            }),
            scoped: vec![crate::appcache::ScopedMetric {
                name: "Fable".to_string(),
                metric: Metric {
                    percent: Some(71.0),
                    resets_at: None,
                },
            }],
            fetched_at: Some(ts(fetched_at)),
        }
    }

    const MERGE_NOW: i64 = 1_700_000_000;

    #[test]
    fn merge_prefers_the_app_cache_when_it_is_newer() {
        let hook = hook_snapshot(MERGE_NOW - 300, 10.0, 40.0);
        let app = app_usage(MERGE_NOW - 60, 11.0, 47.0);
        let merged = merge_snapshot(hook, Some(app), ts(MERGE_NOW));
        assert_eq!(merged.session.as_ref().and_then(|m| m.percent), Some(11.0));
        assert_eq!(merged.weekly.as_ref().and_then(|m| m.percent), Some(47.0));
        assert_eq!(merged.written_at, Some(ts(MERGE_NOW - 60)));
        assert_eq!(merged.state, SnapshotState::Fresh);
        assert!(!merged.hook_missing);
    }

    #[test]
    fn merge_prefers_the_hook_when_it_is_newer() {
        let hook = hook_snapshot(MERGE_NOW - 60, 10.0, 40.0);
        let app = app_usage(MERGE_NOW - 300, 11.0, 47.0);
        let merged = merge_snapshot(hook, Some(app), ts(MERGE_NOW));
        assert_eq!(merged.session.as_ref().and_then(|m| m.percent), Some(10.0));
        assert_eq!(merged.weekly.as_ref().and_then(|m| m.percent), Some(40.0));
        assert_eq!(merged.written_at, Some(ts(MERGE_NOW - 60)));
    }

    #[test]
    fn merge_keeps_the_scoped_rows_even_when_the_hook_wins() {
        let hook = hook_snapshot(MERGE_NOW - 60, 10.0, 40.0);
        let app = app_usage(MERGE_NOW - 300, 11.0, 47.0);
        let merged = merge_snapshot(hook, Some(app), ts(MERGE_NOW));
        assert_eq!(merged.scoped.len(), 1);
        assert_eq!(merged.scoped[0].name, "Fable");
        assert_eq!(merged.scoped_fetched_at, Some(ts(MERGE_NOW - 300)));
    }

    #[test]
    fn merge_falls_back_per_metric_when_the_winner_lacks_one() {
        // The winning (newer) hook has no weekly window at all; the app
        // cache's weekly must survive rather than vanish.
        let mut hook = hook_snapshot(MERGE_NOW - 60, 10.0, 0.0);
        hook.weekly = None;
        let app = app_usage(MERGE_NOW - 300, 11.0, 47.0);
        let merged = merge_snapshot(hook, Some(app), ts(MERGE_NOW));
        assert_eq!(merged.session.as_ref().and_then(|m| m.percent), Some(10.0));
        assert_eq!(merged.weekly.as_ref().and_then(|m| m.percent), Some(47.0));
    }

    #[test]
    fn merge_with_a_missing_hook_uses_the_app_cache_and_flags_the_hook() {
        let hook = UsageSnapshot::missing();
        let app = app_usage(MERGE_NOW - 60, 11.0, 47.0);
        let merged = merge_snapshot(hook, Some(app), ts(MERGE_NOW));
        assert_eq!(merged.session.as_ref().and_then(|m| m.percent), Some(11.0));
        assert_eq!(merged.state, SnapshotState::Fresh);
        assert!(merged.hook_missing);
    }

    #[test]
    fn merge_with_no_app_cache_is_the_hook_snapshot() {
        let hook = hook_snapshot(MERGE_NOW - 60, 10.0, 40.0);
        let merged = merge_snapshot(hook, None, ts(MERGE_NOW));
        assert_eq!(merged.session.as_ref().and_then(|m| m.percent), Some(10.0));
        assert_eq!(merged.weekly.as_ref().and_then(|m| m.percent), Some(40.0));
        assert!(merged.scoped.is_empty());
        assert!(!merged.hook_missing);
    }

    #[test]
    fn merge_with_neither_source_is_missing() {
        let merged = merge_snapshot(UsageSnapshot::missing(), None, ts(MERGE_NOW));
        assert_eq!(merged.state, SnapshotState::Missing);
        assert!(merged.hook_missing);
    }

    #[test]
    fn merge_treats_an_undated_app_cache_as_older_than_any_hook() {
        let hook = hook_snapshot(MERGE_NOW - 60, 10.0, 40.0);
        let mut app = app_usage(MERGE_NOW - 300, 11.0, 47.0);
        app.fetched_at = None;
        let merged = merge_snapshot(hook, Some(app), ts(MERGE_NOW));
        assert_eq!(merged.session.as_ref().and_then(|m| m.percent), Some(10.0));
        assert_eq!(merged.written_at, Some(ts(MERGE_NOW - 60)));
        // The scoped rows still come along; they have no other source.
        assert_eq!(merged.scoped.len(), 1);
    }

    #[test]
    fn merge_classifies_staleness_from_the_winning_timestamp() {
        // Both sources are old; the fresher of the two is still past the
        // staleness threshold, so the merged snapshot is Stale, not Fresh.
        let hook = hook_snapshot(MERGE_NOW - 5000, 10.0, 40.0);
        let mut hook = hook;
        hook.state = SnapshotState::Stale;
        let app = app_usage(MERGE_NOW - 3000, 11.0, 47.0);
        let merged = merge_snapshot(hook, Some(app), ts(MERGE_NOW));
        assert_eq!(merged.written_at, Some(ts(MERGE_NOW - 3000)));
        assert_eq!(merged.state, SnapshotState::Stale);
    }

    #[test]
    fn kayfabe_can_stage_a_fable_row() {
        let body = r#"{"session": 10, "weekly": 20, "fable": 71}"#;
        let snap = fake_snapshot(body, Some(ts(MERGE_NOW)), ts(MERGE_NOW));
        assert_eq!(snap.scoped.len(), 1);
        assert_eq!(snap.scoped[0].name, "Fable");
        assert_eq!(snap.scoped[0].metric.percent, Some(71.0));
        // Anchored to `written_at` so a staged snapshot never grows the
        // "as of" age suffix while the demo is being looked at.
        assert_eq!(snap.scoped_fetched_at, snap.written_at);
    }

    #[test]
    fn kayfabe_without_a_fable_key_stages_no_scoped_rows() {
        let snap = fake_snapshot(r#"{"session": 10}"#, Some(ts(MERGE_NOW)), ts(MERGE_NOW));
        assert!(snap.scoped.is_empty());
    }

    #[test]
    fn read_merged_or_kayfabe_merges_the_two_real_files() {
        let temp = TempDir::new("merged-read");
        let hook_path = cache_path_in(temp.path());
        write_cache(
            &hook_path,
            br#"{"rate_limits":{"five_hour":{"used_percentage":10}}}"#,
        )
        .expect("write hook cache");
        let app_path = temp.path().join(".claude.json");
        std::fs::write(
            &app_path,
            r#"{"cachedUsageUtilization": {"fetchedAtMs": 1, "utilization": {"limits": [
                {"kind": "weekly_scoped", "percent": 71,
                 "scope": {"model": {"display_name": "Fable"}}}
            ]}}}"#,
        )
        .expect("write app cache");
        let snap = read_merged_or_kayfabe(
            &hook_path,
            &app_path,
            &temp.path().join("kayfabe.json"),
            jiff::Timestamp::now(),
        );
        assert_eq!(snap.session.as_ref().and_then(|m| m.percent), Some(10.0));
        assert_eq!(snap.scoped.len(), 1);
        assert!(!snap.hook_missing);
    }

    #[test]
    fn read_merged_or_kayfabe_still_prefers_the_kayfabe_file() {
        let temp = TempDir::new("merged-kayfabe");
        let kayfabe = temp.path().join("kayfabe.json");
        std::fs::write(&kayfabe, r#"{"session": 55, "fable": 71}"#).expect("write kayfabe");
        let snap = read_merged_or_kayfabe(
            &cache_path_in(temp.path()),
            &temp.path().join(".claude.json"),
            &kayfabe,
            jiff::Timestamp::now(),
        );
        assert_eq!(snap.session.as_ref().and_then(|m| m.percent), Some(55.0));
        assert_eq!(snap.scoped.len(), 1);
    }

    #[test]
    fn missing_rate_limits_gives_none_metrics_but_a_real_state() {
        let body = read_fixture("missing_rate_limits.json");
        let snap = snapshot_from(&body, Some(ts(1700000000)), ts(1700000000 + 5));

        assert_eq!(snap.state, SnapshotState::Fresh);
        assert!(snap.session.is_none());
        assert!(snap.weekly.is_none());
    }

    #[test]
    fn float_used_percentage_is_parsed() {
        let body = read_fixture("float_percentage.json");
        let snap = snapshot_from(&body, Some(ts(1700000000)), ts(1700000000 + 5));
        let session = snap.session.expect("session metric present");
        assert_eq!(session.percent, Some(42.75));
    }

    #[test]
    fn missing_resets_at_gives_none_resets_but_keeps_percent() {
        let body = read_fixture("missing_resets_at.json");
        let snap = snapshot_from(&body, Some(ts(1700000000)), ts(1700000000 + 5));
        let session = snap.session.expect("session metric present");
        assert_eq!(session.percent, Some(42.0));
        assert_eq!(session.resets_at, None);
    }

    #[test]
    fn past_resets_at_with_nonzero_percent_forces_percent_to_zero() {
        let body = read_fixture("past_resets_at.json");
        let snap = snapshot_from(&body, Some(ts(1700000000)), ts(1700000000 + 5));
        let session = snap.session.expect("session metric present");
        assert_eq!(session.percent, Some(0.0));
    }

    #[test]
    fn garbage_json_is_missing() {
        let body = read_fixture("garbage.json");
        let snap = snapshot_from(&body, Some(ts(1700000000)), ts(1700000000));
        assert_eq!(snap.state, SnapshotState::Missing);
        assert!(snap.session.is_none());
        assert!(snap.weekly.is_none());
        assert!(snap.written_at.is_none());
    }

    #[test]
    fn empty_body_is_missing() {
        let snap = snapshot_from("", Some(ts(1700000000)), ts(1700000000));
        assert_eq!(snap.state, SnapshotState::Missing);
    }

    #[test]
    fn parse_statusline_json_reports_non_json_as_none() {
        assert!(parse_statusline_json("not json", ts(1700000000)).is_none());
        assert_eq!(
            parse_statusline_json("{}", ts(1700000000)),
            Some(RateLimits::default())
        );
    }

    #[test]
    fn nonexistent_path_gives_missing_via_read_snapshot() {
        let now = ts(1700000000);
        let path = Path::new("/nonexistent/does/not/exist/usage-tray-statusline.json");
        let snap = read_snapshot(path, now);
        assert_eq!(snap.state, SnapshotState::Missing);
        assert!(snap.session.is_none());
        assert!(snap.weekly.is_none());
        assert!(snap.written_at.is_none());
    }

    #[test]
    fn read_snapshot_takes_its_freshness_from_the_file_mtime() {
        let temp = TempDir::new("source-mtime");
        let body = read_fixture("valid_full.json");
        let path = write_with_mtime(temp.path(), CACHE_FILE_NAME, &body, 1_700_000_000);

        let fresh = read_snapshot(&path, ts(1_700_000_000 + 60));
        assert_eq!(fresh.state, SnapshotState::Fresh);
        assert_eq!(fresh.written_at, Some(ts(1_700_000_000)));
        assert_eq!(fresh.session.expect("session").percent, Some(42.0));

        let stale = read_snapshot(&path, ts(1_700_000_000 + 3_600));
        assert_eq!(stale.state, SnapshotState::Stale);
        assert_eq!(stale.written_at, Some(ts(1_700_000_000)));
    }

    #[test]
    fn write_cache_writes_bytes_verbatim_and_leaves_no_temp_file() {
        let temp = TempDir::new("source-write");
        let dir = temp.path().join("claude");
        let path = cache_path_in(&dir);
        let body = br#"{"rate_limits":{"five_hour":{"used_percentage":3}}}"#;

        write_cache(&path, body).expect("write succeeds");
        assert_eq!(std::fs::read(&path).expect("read back"), body);

        // A second write overwrites in place, still leaving only the one file.
        write_cache(&path, b"{}").expect("second write succeeds");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(names.len(), 1, "unexpected files: {names:?}");
        assert_eq!(names[0], CACHE_FILE_NAME);
    }

    #[test]
    fn write_cache_then_read_snapshot_round_trips() {
        let temp = TempDir::new("source-roundtrip");
        let path = cache_path_in(temp.path());
        write_cache(&path, read_fixture("valid_full.json").as_bytes()).expect("write");
        // A just-written file is fresh against the real clock; `now` is pinned
        // only so the fixture's `resets_at` still counts as being in the
        // future (an expired window legitimately zeroes the percentage).
        let snap = read_snapshot(&path, ts(1_700_000_000));
        assert_eq!(snap.state, SnapshotState::Fresh);
        assert_eq!(snap.session.expect("session").percent, Some(42.0));
        assert!(
            read_snapshot(&path, jiff::Timestamp::now()).state == SnapshotState::Fresh,
            "a file written a moment ago must not be stale"
        );
    }

    #[test]
    fn default_cache_path_respects_claude_config_dir_env_var() {
        // SAFETY: test-only env mutation, single-threaded within this process
        // for this variable's usage (no other test reads/writes it).
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", "/tmp/custom-claude-dir");
        }
        let path = default_cache_path();
        unsafe {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
        }
        assert_eq!(
            path,
            std::path::PathBuf::from("/tmp/custom-claude-dir/usage-tray-statusline.json")
        );
    }

    #[test]
    fn should_write_cache_writes_when_no_existing_cache() {
        assert!(should_write_cache(b"{}", None));
    }

    #[test]
    fn should_write_cache_writes_when_incoming_has_rate_limits() {
        let incoming = br#"{"rate_limits":{"five_hour":{"used_percentage":3}}}"#;
        let existing = br#"{"rate_limits":{"five_hour":{"used_percentage":1}}}"#;
        assert!(should_write_cache(incoming, Some(existing)));
    }

    #[test]
    fn should_write_cache_skips_when_incoming_lacks_it_but_existing_has_it() {
        let incoming = br#"{"model":"opus"}"#;
        let existing = br#"{"rate_limits":{"five_hour":{"used_percentage":1}}}"#;
        assert!(!should_write_cache(incoming, Some(existing)));
    }

    #[test]
    fn should_write_cache_writes_when_both_lack_it() {
        let incoming = br#"{"model":"opus"}"#;
        let existing = br#"{"model":"sonnet"}"#;
        assert!(should_write_cache(incoming, Some(existing)));
    }

    #[test]
    fn should_write_cache_skips_when_incoming_malformed_and_existing_good() {
        let incoming = b"not json";
        let existing = br#"{"rate_limits":{"five_hour":{"used_percentage":1}}}"#;
        assert!(!should_write_cache(incoming, Some(existing)));
    }

    #[test]
    fn should_write_cache_writes_when_existing_malformed_and_incoming_good() {
        let incoming = br#"{"rate_limits":{"five_hour":{"used_percentage":3}}}"#;
        let existing = b"not json";
        assert!(should_write_cache(incoming, Some(existing)));
    }

    #[test]
    fn should_write_cache_treats_null_rate_limits_as_absent() {
        let incoming = br#"{"rate_limits":null}"#;
        let existing = br#"{"rate_limits":{"five_hour":{"used_percentage":1}}}"#;
        assert!(!should_write_cache(incoming, Some(existing)));
        assert!(should_write_cache(incoming, None));
    }

    #[test]
    fn default_cache_path_falls_back_to_home_dot_claude() {
        unsafe {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
        }
        let path = default_cache_path();
        assert!(path.ends_with(CACHE_FILE_NAME));
        assert!(path.to_string_lossy().contains(".claude"));
    }

    // -- kayfabe -----------------------------------------------------------

    #[test]
    fn fake_snapshot_full_file_sets_every_field() {
        let now = ts(1_700_000_000);
        let body = r#"{"session": 82, "weekly": 45, "age_minutes": 10,
                        "session_resets_in_minutes": 30, "weekly_resets_in_days": 2}"#;
        let snap = fake_snapshot(body, Some(now), now);

        assert_eq!(snap.state, SnapshotState::Fresh);
        assert_eq!(snap.written_at, Some(ts(1_700_000_000 - 600)));

        let session = snap.session.expect("session present");
        assert_eq!(session.percent, Some(82.0));
        assert_eq!(session.resets_at, Some(ts(1_700_000_000 + 30 * 60)));

        let weekly = snap.weekly.expect("weekly present");
        assert_eq!(weekly.percent, Some(45.0));
        assert_eq!(weekly.resets_at, Some(ts(1_700_000_000 + 2 * 86_400)));
    }

    #[test]
    fn fake_snapshot_empty_object_uses_every_default() {
        let now = ts(1_700_000_000);
        let snap = fake_snapshot("{}", Some(now), now);

        assert_eq!(snap.state, SnapshotState::Fresh);
        assert_eq!(snap.written_at, Some(now)); // age_minutes defaults to 0

        let session = snap.session.expect("session present, percent unknown");
        assert_eq!(session.percent, None);
        assert_eq!(session.resets_at, Some(ts(1_700_000_000 + 180 * 60)));

        let weekly = snap.weekly.expect("weekly present, percent unknown");
        assert_eq!(weekly.percent, None);
        assert_eq!(weekly.resets_at, Some(ts(1_700_000_000 + 4 * 86_400)));
    }

    #[test]
    fn fake_snapshot_session_omitted_is_none_percent_but_weekly_still_set() {
        let now = ts(1_700_000_000);
        let snap = fake_snapshot(r#"{"weekly": 45}"#, Some(now), now);
        assert_eq!(snap.session.expect("session present").percent, None);
        assert_eq!(snap.weekly.expect("weekly present").percent, Some(45.0));
    }

    #[test]
    fn fake_snapshot_weekly_omitted_is_none_percent_but_session_still_set() {
        let now = ts(1_700_000_000);
        let snap = fake_snapshot(r#"{"session": 82}"#, Some(now), now);
        assert_eq!(snap.session.expect("session present").percent, Some(82.0));
        assert_eq!(snap.weekly.expect("weekly present").percent, None);
    }

    #[test]
    fn fake_snapshot_age_crossing_stale_boundary() {
        let now = ts(1_700_000_000);
        let fresh = fake_snapshot(r#"{"age_minutes": 9}"#, Some(now), now); // 540s < 600s
        assert_eq!(fresh.state, SnapshotState::Fresh);

        let stale = fake_snapshot(r#"{"age_minutes": 12}"#, Some(now), now); // 720s > 600s
        assert_eq!(stale.state, SnapshotState::Stale);
        assert_eq!(stale.written_at, Some(ts(1_700_000_000 - 12 * 60)));
    }

    #[test]
    fn fake_snapshot_negative_resets_in_are_kept_as_past_timestamps() {
        let now = ts(1_700_000_000);
        let snap = fake_snapshot(
            r#"{"session": 0, "session_resets_in_minutes": -5, "weekly_resets_in_days": -1}"#,
            Some(now),
            now,
        );
        // The fake file is authoritative: percent is NOT re-zeroed by the
        // real cache's past-resets-at rule, it's simply whatever was set.
        let session = snap.session.expect("session present");
        assert_eq!(session.percent, Some(0.0));
        assert_eq!(session.resets_at, Some(ts(1_700_000_000 - 5 * 60)));
        let weekly = snap.weekly.expect("weekly present");
        assert_eq!(weekly.resets_at, Some(ts(1_700_000_000 - 86_400)));
    }

    #[test]
    fn fake_snapshot_resets_are_anchored_to_the_mtime_not_to_now() {
        let mtime = ts(1_700_000_000);
        let now = ts(1_700_000_000 + 3_600); // an hour of polling later
        let snap = fake_snapshot(
            r#"{"session": 82, "session_resets_in_minutes": 30, "weekly_resets_in_days": 2}"#,
            Some(mtime),
            now,
        );
        assert_eq!(
            snap.session.expect("session").resets_at,
            Some(ts(1_700_000_000 + 30 * 60))
        );
        assert_eq!(
            snap.weekly.expect("weekly").resets_at,
            Some(ts(1_700_000_000 + 2 * 86_400))
        );
    }

    #[test]
    fn fake_snapshot_resets_do_not_move_between_polls_of_an_unedited_file() {
        // The bug this pins: a `now`-anchored reset time drifted forward on
        // every poll, which the threshold notifier read as a brand-new 5-hour
        // window and used to re-fire the same alert every few seconds.
        let mtime = ts(1_700_000_000);
        let body = r#"{"session": 82}"#;
        let first = fake_snapshot(body, Some(mtime), ts(1_700_000_100));
        for tick in [1, 5, 60, 3_600] {
            let later = fake_snapshot(body, Some(mtime), ts(1_700_000_100 + tick));
            assert_eq!(
                first.session.as_ref().expect("session").resets_at,
                later.session.as_ref().expect("session").resets_at,
                "session reset moved after {tick}s"
            );
            assert_eq!(
                first.weekly.as_ref().expect("weekly").resets_at,
                later.weekly.as_ref().expect("weekly").resets_at,
                "weekly reset moved after {tick}s"
            );
        }
    }

    #[test]
    fn fake_snapshot_without_an_mtime_falls_back_to_now() {
        let now = ts(1_700_000_000);
        let snap = fake_snapshot(r#"{"session_resets_in_minutes": 30}"#, None, now);
        assert_eq!(
            snap.session.expect("session").resets_at,
            Some(ts(1_700_000_000 + 30 * 60))
        );
    }

    #[test]
    fn read_merged_or_kayfabe_takes_the_reset_anchor_from_the_file_on_disk() {
        // End to end through the real stat, not just the pure helper.
        let temp = TempDir::new("kayfabe-anchor");
        let cache_path = temp.path().join(CACHE_FILE_NAME); // never created
        let kayfabe_path = write_with_mtime(
            temp.path(),
            "kayfabe.json",
            r#"{"session": 82, "session_resets_in_minutes": 30}"#,
            1_700_000_000,
        );

        let first = read_merged_or_kayfabe(&cache_path, &cache_path.with_extension("no-app-cache"), &kayfabe_path, ts(1_700_000_005));
        let later = read_merged_or_kayfabe(&cache_path, &cache_path.with_extension("no-app-cache"), &kayfabe_path, ts(1_700_000_305));
        assert_eq!(
            first.session.expect("session").resets_at,
            Some(ts(1_700_000_000 + 30 * 60))
        );
        assert_eq!(
            later.session.expect("session").resets_at,
            Some(ts(1_700_000_000 + 30 * 60))
        );
    }

    #[test]
    fn fake_snapshot_garbage_json_is_missing() {
        let now = ts(1_700_000_000);
        let snap = fake_snapshot("not json", Some(now), now);
        assert_eq!(snap.state, SnapshotState::Missing);
        assert!(snap.session.is_none());
        assert!(snap.weekly.is_none());
        assert!(snap.written_at.is_none());
    }

    #[test]
    fn read_merged_or_kayfabe_falls_through_to_real_cache_when_kayfabe_absent() {
        let temp = TempDir::new("kayfabe-absent");
        let body = read_fixture("valid_full.json");
        let cache_path = write_with_mtime(temp.path(), CACHE_FILE_NAME, &body, 1_700_000_000);
        let kayfabe_path = temp.path().join("kayfabe.json"); // never created

        let snap = read_merged_or_kayfabe(&cache_path, &cache_path.with_extension("no-app-cache"), &kayfabe_path, ts(1_700_000_000 + 5));
        assert_eq!(snap.state, SnapshotState::Fresh);
        assert_eq!(snap.session.expect("session").percent, Some(42.0));
    }

    #[test]
    fn read_merged_or_kayfabe_uses_kayfabe_when_present() {
        let temp = TempDir::new("kayfabe-present");
        let body = read_fixture("valid_full.json");
        let cache_path = write_with_mtime(temp.path(), CACHE_FILE_NAME, &body, 1_700_000_000);
        let kayfabe_path = temp.path().join("kayfabe.json");
        std::fs::write(&kayfabe_path, r#"{"session": 82}"#).expect("write kayfabe");

        let now = ts(1_700_000_000 + 5);
        let snap = read_merged_or_kayfabe(&cache_path, &cache_path.with_extension("no-app-cache"), &kayfabe_path, now);
        // The real cache says 42%; the kayfabe file wins.
        assert_eq!(snap.session.expect("session").percent, Some(82.0));
    }

    #[test]
    fn read_merged_or_kayfabe_missing_file_content_yields_missing_state() {
        let temp = TempDir::new("kayfabe-garbage");
        let body = read_fixture("valid_full.json");
        let cache_path = write_with_mtime(temp.path(), CACHE_FILE_NAME, &body, 1_700_000_000);
        let kayfabe_path = temp.path().join("kayfabe.json");
        std::fs::write(&kayfabe_path, "not json").expect("write kayfabe");

        let snap = read_merged_or_kayfabe(&cache_path, &cache_path.with_extension("no-app-cache"), &kayfabe_path, ts(1_700_000_000 + 5));
        assert_eq!(snap.state, SnapshotState::Missing);
    }

    #[test]
    fn default_kayfabe_path_sits_next_to_config_toml() {
        let path = default_kayfabe_path();
        assert!(path.ends_with("claude-usage-tray/kayfabe.json"));
    }
}
