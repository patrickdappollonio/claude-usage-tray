//! Optional hourly refresh of Claude Code's usage blob via the CLI itself.
//!
//! The `.claude.json` usage blob (see `appcache.rs`) only refreshes when
//! Claude Code decides to fetch `/api/oauth/usage` — typically when someone
//! opens `/usage`. Left alone it can sit for many hours, and the per-model
//! weekly rows go stale with it. Running `claude -p "/usage"` headlessly
//! refreshes it as a side effect: the official client, the user's normal
//! login, zero tokens consumed, a few seconds of wall clock.
//!
//! This module decides *when* that is worth doing (pure, tested) and does it
//! (a detached thread around a child process). The `cli_refresh` config flag
//! gates the whole thing, and the cadence is bounded both by the blob's own
//! `fetchedAtMs` and by when we last *tried* — so a broken or missing
//! `claude` binary is re-attempted hourly, not on every 5-second poll tick.

/// How old the blob may get before a refresh is warranted, and also the
/// minimum spacing between attempts. One hour, per the setting's design: long
/// enough to be negligible load, short enough that the per-model rows track
/// the day.
pub const REFRESH_AFTER_SECS: i64 = 3600;

/// Whether the poll loop should kick off a refresh now.
///
/// `fetched_at` is the blob's own `fetchedAtMs` (None: no blob at all);
/// `last_attempt` is when this process last spawned the CLI (None: never).
pub fn should_refresh(
    enabled: bool,
    fetched_at: Option<jiff::Timestamp>,
    last_attempt: Option<jiff::Timestamp>,
    now: jiff::Timestamp,
) -> bool {
    if !enabled {
        return false;
    }
    // `>` on timestamps directly: a future-dated stamp (clock skew) simply
    // fails the "older than an hour" test, which is the safe reading.
    let older_than_hour = |at: Option<jiff::Timestamp>| match at {
        Some(at) => now.as_second() - at.as_second() > REFRESH_AFTER_SECS,
        None => true,
    };
    older_than_hour(fetched_at) && older_than_hour(last_attempt)
}

/// Spawns `claude -p "/usage"` detached and returns immediately. The child's
/// output is discarded — the write it makes to `.claude.json` is the entire
/// point, and the next poll tick picks that up. Failures are silent by
/// design: the tray must keep working on machines where `claude` is not on
/// `PATH`, and [`should_refresh`]'s attempt-spacing already prevents retry
/// storms.
pub fn spawn_refresh() {
    std::thread::spawn(|| {
        let Ok(mut child) = std::process::Command::new("claude")
            .args(["-p", "/usage"])
            // A neutral working directory, so the run is not attributed to
            // whatever directory the tray happened to start in.
            .current_dir(dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/")))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return;
        };
        // Reap the child, but never wait forever on a hung CLI: poll for up
        // to two minutes, then kill it. One mostly-sleeping thread, not load.
        for _ in 0..120 {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(std::time::Duration::from_secs(1)),
                Err(_) => return,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> jiff::Timestamp {
        jiff::Timestamp::from_second(secs).expect("valid timestamp")
    }

    const NOW: i64 = 1_700_000_000;

    #[test]
    fn disabled_never_refreshes() {
        assert!(!should_refresh(false, None, None, ts(NOW)));
    }

    #[test]
    fn no_blob_and_never_attempted_refreshes() {
        assert!(should_refresh(true, None, None, ts(NOW)));
    }

    #[test]
    fn a_fresh_blob_does_not_refresh() {
        assert!(!should_refresh(true, Some(ts(NOW - 600)), None, ts(NOW)));
    }

    #[test]
    fn a_blob_older_than_an_hour_refreshes() {
        assert!(should_refresh(true, Some(ts(NOW - 3601)), None, ts(NOW)));
    }

    #[test]
    fn a_recent_attempt_blocks_retry_even_with_an_old_blob() {
        // The attempt failed (blob still old) — do not respawn every tick.
        assert!(!should_refresh(
            true,
            Some(ts(NOW - 7200)),
            Some(ts(NOW - 120)),
            ts(NOW)
        ));
    }

    #[test]
    fn an_old_attempt_allows_another_try() {
        assert!(should_refresh(
            true,
            Some(ts(NOW - 7200)),
            Some(ts(NOW - 3601)),
            ts(NOW)
        ));
    }

    #[test]
    fn a_future_dated_blob_counts_as_fresh_not_negative_age() {
        // Clock skew: a blob "from the future" means the writer's clock ran
        // ahead, not that a refresh is due.
        assert!(!should_refresh(true, Some(ts(NOW + 600)), None, ts(NOW)));
    }
}
