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
#[derive(Clone, Debug, PartialEq)]
pub enum SnapshotState {
    /// Cache file read and parsed; its mtime is within the staleness window.
    Fresh,
    /// Cache file read and parsed, but its mtime is older than the staleness
    /// threshold.
    Stale,
    /// Cache file missing, unreadable, or its JSON could not be parsed.
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

/// A point-in-time read of the statusline cache.
#[derive(Clone, Debug)]
pub struct UsageSnapshot {
    pub session: Option<Metric>,
    pub weekly: Option<Metric>,
    /// When Claude Code last wrote the cache — under contract v2 this is the
    /// cache file's mtime rather than a field inside the file. The name is
    /// kept because that is still exactly what it means to every reader
    /// ("Updated 3 min ago", "Stale since 14:02").
    pub written_at: Option<jiff::Timestamp>,
    pub state: SnapshotState,
}

impl UsageSnapshot {
    fn missing() -> Self {
        UsageSnapshot {
            session: None,
            weekly: None,
            written_at: None,
            state: SnapshotState::Missing,
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
    let modified = std::fs::metadata(path).and_then(|meta| meta.modified()).ok()?;
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
}
