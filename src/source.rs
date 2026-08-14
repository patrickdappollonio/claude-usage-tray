//! Usage snapshot model + statusline cache-file reader.
//!
//! See `docs/superpowers/specs/2026-08-13-claude-usage-tray-design.md` for the
//! cache contract. This module never panics on bad input: every failure mode
//! maps to a `SnapshotState` value.
//!
//! The public items here are not yet consumed outside tests: `icon.rs` and
//! `tray.rs`/`main.rs` (Tasks 2-3) wire them into the running binary. Until
//! then a plain (non-test) build sees them as unused, so this module is
//! exempted from `dead_code` at the module level, matching the convention
//! used by the other not-yet-wired placeholder modules.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Freshness classification for a [`UsageSnapshot`].
#[derive(Clone, Debug, PartialEq)]
pub enum SnapshotState {
    /// Cache file read and parsed; `written_at` is within the staleness window.
    Fresh,
    /// Cache file read and parsed, but `written_at` is older than the
    /// staleness threshold.
    Stale,
    /// Cache file missing, unreadable, or its JSON could not be parsed
    /// (including a missing/invalid `written_at`).
    Missing,
}

/// One rate-limit window (5-hour session or 7-day weekly).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Metric {
    pub percent: Option<f64>,
    pub resets_at: Option<jiff::Timestamp>,
}

/// A point-in-time read of the statusline usage cache.
#[derive(Clone, Debug)]
pub struct UsageSnapshot {
    pub session: Option<Metric>,
    pub weekly: Option<Metric>,
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

/// Cache entries older than this many seconds are considered stale.
const STALE_THRESHOLD_SECS: i64 = 600;

/// Default cache file location: `$CLAUDE_CONFIG_DIR/usage-tray-cache.json`,
/// falling back to `~/.claude/usage-tray-cache.json` when the env var is unset.
pub fn default_cache_path() -> PathBuf {
    let base = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".claude"),
    };
    base.join("usage-tray-cache.json")
}

/// Reads and parses the cache file at `path`. Never panics: any I/O error
/// (missing file, permissions, etc.) yields a `Missing` snapshot.
pub fn read_snapshot(path: &Path, now: jiff::Timestamp) -> UsageSnapshot {
    match std::fs::read_to_string(path) {
        Ok(body) => parse_cache_json(&body, now),
        Err(_) => UsageSnapshot::missing(),
    }
}

/// Pure parser for the cache JSON contract. Never panics: malformed or
/// partial JSON degrades gracefully rather than erroring loudly.
pub fn parse_cache_json(body: &str, now: jiff::Timestamp) -> UsageSnapshot {
    let value: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return UsageSnapshot::missing(),
    };

    let written_at_secs = match value.get("written_at").and_then(|v| v.as_i64()) {
        Some(secs) => secs,
        None => return UsageSnapshot::missing(),
    };
    let written_at = match jiff::Timestamp::from_second(written_at_secs) {
        Ok(ts) => ts,
        Err(_) => return UsageSnapshot::missing(),
    };

    let state = if now.as_second() - written_at.as_second() > STALE_THRESHOLD_SECS {
        SnapshotState::Stale
    } else {
        SnapshotState::Fresh
    };

    let rate_limits = value.get("rate_limits");
    let session = rate_limits
        .and_then(|rl| rl.get("five_hour"))
        .map(|m| parse_metric(m, now));
    let weekly = rate_limits
        .and_then(|rl| rl.get("seven_day"))
        .map(|m| parse_metric(m, now));

    UsageSnapshot {
        session,
        weekly,
        written_at: Some(written_at),
        state,
    }
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

    #[test]
    fn valid_full_cache_is_fresh_with_both_metrics() {
        let body = read_fixture("valid_full.json");
        let now = ts(1700000000 + 100); // 100s after written_at: well within threshold
        let snap = parse_cache_json(&body, now);

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
    fn written_at_older_than_600s_is_stale() {
        let body = read_fixture("valid_full.json");
        let now = ts(1700000000 + 601);
        let snap = parse_cache_json(&body, now);
        assert_eq!(snap.state, SnapshotState::Stale);
    }

    #[test]
    fn written_at_exactly_600s_old_is_still_fresh() {
        let body = read_fixture("valid_full.json");
        let now = ts(1700000000 + 600);
        let snap = parse_cache_json(&body, now);
        assert_eq!(snap.state, SnapshotState::Fresh);
    }

    #[test]
    fn missing_rate_limits_gives_none_metrics_but_state_from_age() {
        let body = read_fixture("missing_rate_limits.json");
        let now = ts(1700000000 + 5);
        let snap = parse_cache_json(&body, now);

        assert_eq!(snap.state, SnapshotState::Fresh);
        assert!(snap.session.is_none());
        assert!(snap.weekly.is_none());
    }

    #[test]
    fn float_used_percentage_is_parsed() {
        let body = read_fixture("float_percentage.json");
        let now = ts(1700000000 + 5);
        let snap = parse_cache_json(&body, now);
        let session = snap.session.expect("session metric present");
        assert_eq!(session.percent, Some(42.75));
    }

    #[test]
    fn missing_resets_at_gives_none_resets_but_keeps_percent() {
        let body = read_fixture("missing_resets_at.json");
        let now = ts(1700000000 + 5);
        let snap = parse_cache_json(&body, now);
        let session = snap.session.expect("session metric present");
        assert_eq!(session.percent, Some(42.0));
        assert_eq!(session.resets_at, None);
    }

    #[test]
    fn past_resets_at_with_nonzero_percent_forces_percent_to_zero() {
        let body = read_fixture("past_resets_at.json");
        // now is well after the resets_at in the fixture (1_600_000_000)
        let now = ts(1700000000 + 5);
        let snap = parse_cache_json(&body, now);
        let session = snap.session.expect("session metric present");
        assert_eq!(session.percent, Some(0.0));
    }

    #[test]
    fn garbage_json_is_missing() {
        let body = read_fixture("garbage.json");
        let now = ts(1700000000);
        let snap = parse_cache_json(&body, now);
        assert_eq!(snap.state, SnapshotState::Missing);
        assert!(snap.session.is_none());
        assert!(snap.weekly.is_none());
        assert!(snap.written_at.is_none());
    }

    #[test]
    fn empty_body_is_missing() {
        let now = ts(1700000000);
        let snap = parse_cache_json("", now);
        assert_eq!(snap.state, SnapshotState::Missing);
    }

    #[test]
    fn nonexistent_path_gives_missing_via_read_snapshot() {
        let now = ts(1700000000);
        let path = Path::new("/nonexistent/does/not/exist/usage-tray-cache.json");
        let snap = read_snapshot(path, now);
        assert_eq!(snap.state, SnapshotState::Missing);
        assert!(snap.session.is_none());
        assert!(snap.weekly.is_none());
        assert!(snap.written_at.is_none());
    }

    #[test]
    fn read_snapshot_reads_real_file_from_disk() {
        let now = ts(1700000000 + 5);
        let snap = read_snapshot(&fixture_path("valid_full.json"), now);
        assert_eq!(snap.state, SnapshotState::Fresh);
        assert!(snap.session.is_some());
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
            std::path::PathBuf::from("/tmp/custom-claude-dir/usage-tray-cache.json")
        );
    }

    #[test]
    fn default_cache_path_falls_back_to_home_dot_claude() {
        unsafe {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
        }
        let path = default_cache_path();
        assert!(path.ends_with("usage-tray-cache.json"));
        assert!(path.to_string_lossy().contains(".claude"));
    }
}
