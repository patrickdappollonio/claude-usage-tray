//! Reader for Claude Code's own on-disk usage cache.
//!
//! Alongside the statusline hook (see `source.rs`), Claude Code keeps a
//! second, richer usage record: whenever the CLI fetches `/api/oauth/usage`
//! for its `/usage` screen it caches the whole structured response in
//! `.claude.json` under the `cachedUsageUtilization` key. That blob carries
//! everything the statusline `rate_limits` object does *plus* per-model
//! weekly buckets ("Current week (Fable)") the statusline never sees, and a
//! `fetchedAtMs` stamp saying when it was fetched.
//!
//! This module only ever *reads* that file — the same passive posture as the
//! statusline cache. It is an internal cache of Claude Code's, not a
//! contract, so every field is optional and any surprise degrades to "this
//! metric is absent", never to an error the tray would surface.

use std::path::{Path, PathBuf};

use crate::source::Metric;

/// One per-model weekly bucket from the `limits` array: a `weekly_scoped`
/// entry such as "Fable at 71%".
#[derive(Clone, Debug, PartialEq)]
pub struct ScopedMetric {
    /// The model's display name as Claude Code reports it ("Fable").
    pub name: String,
    pub metric: Metric,
}

/// The usable subset of `cachedUsageUtilization`, once extracted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AppUsage {
    pub session: Option<Metric>,
    pub weekly: Option<Metric>,
    /// Per-model weekly buckets, in the order Claude Code listed them.
    pub scoped: Vec<ScopedMetric>,
    /// When Claude Code fetched this from the API (`fetchedAtMs`).
    pub fetched_at: Option<jiff::Timestamp>,
}

/// Where `.claude.json` lives: `$CLAUDE_CONFIG_DIR/.claude.json` when the
/// override is set, else `~/.claude.json` (note: the home directory itself,
/// *not* inside `~/.claude/`).
pub fn app_cache_path() -> PathBuf {
    app_cache_path_from(
        std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from),
        dirs::home_dir(),
    )
}

/// Pure resolution behind [`app_cache_path`], parameterized so tests never
/// race other tests over the real environment.
fn app_cache_path_from(config_dir: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    match config_dir {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(".claude.json"),
        _ => home
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".claude.json"),
    }
}

/// Reads and parses `.claude.json` at `path`. Any failure — missing file,
/// unreadable, not JSON, no `cachedUsageUtilization` key — is `None`.
pub fn read_app_usage(path: &Path, now: jiff::Timestamp) -> Option<AppUsage> {
    let body = std::fs::read_to_string(path).ok()?;
    parse_app_cache(&body, now)
}

/// Pure parser for a `.claude.json` document body. Same posture as
/// `source::parse_statusline_json`: plain `serde_json::Value` lookups, every
/// missing or mistyped field degrades to absence.
pub fn parse_app_cache(body: &str, now: jiff::Timestamp) -> Option<AppUsage> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let cached = value.get("cachedUsageUtilization")?;
    if !cached.is_object() {
        return None;
    }

    let fetched_at = cached
        .get("fetchedAtMs")
        .and_then(|v| v.as_i64())
        .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok());
    let utilization = cached.get("utilization");

    let window = |key: &str| {
        utilization
            .and_then(|u| u.get(key))
            .filter(|w| w.is_object())
            .map(|w| parse_window(w, "utilization", now))
    };

    let scoped = utilization
        .and_then(|u| u.get("limits"))
        .and_then(|l| l.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter(|entry| {
                    entry.get("kind").and_then(|k| k.as_str()) == Some("weekly_scoped")
                })
                .filter_map(|entry| {
                    let name = entry
                        .get("scope")?
                        .get("model")?
                        .get("display_name")?
                        .as_str()?
                        .to_string();
                    Some(ScopedMetric {
                        name,
                        metric: parse_window(entry, "percent", now),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Some(AppUsage {
        session: window("five_hour"),
        weekly: window("seven_day"),
        scoped,
        fetched_at,
    })
}

/// Parses one rate-limit window object. `percent_key` differs between the
/// named buckets (`utilization`) and the `limits` entries (`percent`);
/// `resets_at` is an ISO-8601 string in both. The past-reset zeroing rule is
/// the same as `source::parse_metric`'s: a window whose reset has passed
/// rolled over while nothing was running, so its recorded percent describes a
/// window that no longer exists.
fn parse_window(value: &serde_json::Value, percent_key: &str, now: jiff::Timestamp) -> Metric {
    let percent = value.get(percent_key).and_then(|v| v.as_f64());
    let resets_at = value
        .get("resets_at")
        .and_then(|v| v.as_str())
        .and_then(|text| text.parse::<jiff::Timestamp>().ok());

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

    fn read_fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/appcache")
            .join(name);
        std::fs::read_to_string(path).expect("fixture must exist")
    }

    fn ts(secs: i64) -> jiff::Timestamp {
        jiff::Timestamp::from_second(secs).expect("valid timestamp")
    }

    /// A `now` earlier than every `resets_at` in the fixture, so the
    /// past-reset zeroing rule stays out of the way.
    const NOW: i64 = 1786841760; // 2026-08-15, between fetchedAtMs and the resets

    #[test]
    fn parses_the_full_fixture() {
        let usage = parse_app_cache(&read_fixture("valid_full.json"), ts(NOW)).expect("parses");

        let session = usage.session.expect("session present");
        assert_eq!(session.percent, Some(11.0));
        assert_eq!(
            session.resets_at.map(|at| at.as_second()),
            Some(1786856400) // 2026-08-16T05:00:00Z, fractional seconds dropped
        );

        let weekly = usage.weekly.expect("weekly present");
        assert_eq!(weekly.percent, Some(47.0));

        assert_eq!(usage.scoped.len(), 1);
        assert_eq!(usage.scoped[0].name, "Fable");
        assert_eq!(usage.scoped[0].metric.percent, Some(71.0));
        assert!(usage.scoped[0].metric.resets_at.is_some());

        assert_eq!(
            usage.fetched_at.map(|at| at.as_millisecond()),
            Some(1786841753450)
        );
    }

    #[test]
    fn session_and_weekly_limits_entries_do_not_become_scoped_rows() {
        let usage = parse_app_cache(&read_fixture("valid_full.json"), ts(NOW)).expect("parses");
        assert!(
            usage.scoped.iter().all(|scoped| scoped.name == "Fable"),
            "only weekly_scoped entries with a model name belong here: {:?}",
            usage.scoped
        );
    }

    #[test]
    fn missing_cached_usage_key_is_none() {
        assert_eq!(parse_app_cache(r#"{"numStartups": 3}"#, ts(NOW)), None);
    }

    #[test]
    fn null_cached_usage_key_is_none() {
        assert_eq!(
            parse_app_cache(r#"{"cachedUsageUtilization": null}"#, ts(NOW)),
            None
        );
    }

    #[test]
    fn garbage_is_none() {
        assert_eq!(parse_app_cache("not json at all", ts(NOW)), None);
    }

    #[test]
    fn empty_utilization_is_a_valid_but_empty_reading() {
        let body = r#"{"cachedUsageUtilization": {"fetchedAtMs": 1786841753450, "utilization": {}}}"#;
        let usage = parse_app_cache(body, ts(NOW)).expect("the key exists, so this parses");
        assert_eq!(usage.session, None);
        assert_eq!(usage.weekly, None);
        assert!(usage.scoped.is_empty());
        assert!(usage.fetched_at.is_some());
    }

    #[test]
    fn a_past_reset_zeroes_the_percent_like_the_statusline_parser() {
        // Same rule as `source::parse_metric`: a window whose reset has
        // passed rolled over while nothing was running, so its recorded
        // percent describes a window that no longer exists.
        let body = r#"{"cachedUsageUtilization": {"utilization": {
            "five_hour": {"utilization": 80, "resets_at": "2026-08-15T00:00:00+00:00"}
        }}}"#;
        let usage = parse_app_cache(body, ts(NOW)).expect("parses");
        assert_eq!(usage.session.expect("session").percent, Some(0.0));
    }

    #[test]
    fn scoped_entries_without_a_model_name_are_skipped() {
        let body = r#"{"cachedUsageUtilization": {"utilization": {"limits": [
            {"kind": "weekly_scoped", "percent": 12, "scope": {"model": null}},
            {"kind": "weekly_scoped", "percent": 34, "scope": null},
            {"kind": "weekly_scoped", "percent": 56,
             "scope": {"model": {"display_name": "Fable"}}}
        ]}}}"#;
        let usage = parse_app_cache(body, ts(NOW)).expect("parses");
        assert_eq!(usage.scoped.len(), 1);
        assert_eq!(usage.scoped[0].name, "Fable");
        assert_eq!(usage.scoped[0].metric.percent, Some(56.0));
    }

    #[test]
    fn wrong_types_degrade_to_absent_fields_not_errors() {
        let body = r#"{"cachedUsageUtilization": {"fetchedAtMs": "soon", "utilization": {
            "five_hour": {"utilization": "lots", "resets_at": 12345},
            "limits": "nope"
        }}}"#;
        let usage = parse_app_cache(body, ts(NOW)).expect("parses");
        assert_eq!(usage.fetched_at, None);
        let session = usage.session.expect("the window object exists");
        assert_eq!(session.percent, None);
        assert_eq!(session.resets_at, None);
        assert!(usage.scoped.is_empty());
    }

    #[test]
    fn read_app_usage_missing_file_is_none() {
        let temp = TempDir::new("appcache-missing");
        assert_eq!(
            read_app_usage(&temp.path().join(".claude.json"), ts(NOW)),
            None
        );
    }

    #[test]
    fn read_app_usage_reads_a_real_file() {
        let temp = TempDir::new("appcache-real");
        let path = temp.path().join(".claude.json");
        std::fs::write(&path, read_fixture("valid_full.json")).expect("write");
        let usage = read_app_usage(&path, ts(NOW)).expect("parses");
        assert_eq!(usage.scoped[0].name, "Fable");
    }

    #[test]
    fn app_cache_path_respects_claude_config_dir() {
        assert_eq!(
            app_cache_path_from(Some(PathBuf::from("/tmp/custom-claude-dir")), None),
            PathBuf::from("/tmp/custom-claude-dir/.claude.json")
        );
    }

    #[test]
    fn app_cache_path_defaults_to_home_not_dot_claude() {
        let path = app_cache_path_from(None, Some(PathBuf::from("/home/someone")));
        assert_eq!(path, PathBuf::from("/home/someone/.claude.json"));
        assert!(
            !path.starts_with("/home/someone/.claude/"),
            "must be ~/.claude.json, not inside ~/.claude/"
        );
    }
}
