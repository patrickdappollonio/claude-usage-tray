//! Optional periodic refresh of Claude Code's usage blob via the CLI itself.
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
//! `claude` binary is re-attempted at that cadence, not on every 5-second poll
//! tick. The cadence is ten minutes while Claude Code is in use on this
//! machine and an hour otherwise; see [`should_refresh`].
//!
//! Finding `claude` is its own small problem: see [`resolve`].

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// How old the blob may get, and the minimum spacing between attempts, while
/// nothing suggests it is behind. One hour: negligible load, and still enough
/// to pick up usage from the account's other machines.
pub const IDLE_REFRESH_SECS: i64 = 3600;

/// The same, while Claude Code is in use on this machine. Ten minutes keeps
/// the per-model rows close to the session and weekly ones, which the
/// statusline hook updates live.
pub const ACTIVE_REFRESH_SECS: i64 = 600;

/// Whether the poll loop should kick off a refresh now.
///
/// `fetched_at` is the blob's own `fetchedAtMs` (None: no blob at all);
/// `last_activity` is when the statusline hook last reported (None: never);
/// `last_attempt` is when this process last spawned the CLI (None: never).
///
/// The hook having reported *since* the blob was fetched is what counts as
/// "in use": usage has happened that the blob cannot contain. That also gives
/// a session's last few minutes one final fast refresh after it ends, before
/// the cadence drops back to hourly.
pub fn should_refresh(
    enabled: bool,
    fetched_at: Option<jiff::Timestamp>,
    last_activity: Option<jiff::Timestamp>,
    last_attempt: Option<jiff::Timestamp>,
    now: jiff::Timestamp,
) -> bool {
    if !enabled {
        return false;
    }
    let behind = match (last_activity, fetched_at) {
        (Some(activity), Some(fetched)) => activity > fetched,
        _ => false,
    };
    let wait = if behind {
        ACTIVE_REFRESH_SECS
    } else {
        IDLE_REFRESH_SECS
    };
    // A future-dated stamp (clock skew) simply fails the "older than" test,
    // which is the safe reading.
    let older_than_wait = |at: Option<jiff::Timestamp>| match at {
        Some(at) => now.as_second() - at.as_second() > wait,
        None => true,
    };
    older_than_wait(fetched_at) && older_than_wait(last_attempt)
}

/// Directories where a `claude` binary commonly lives, relative to the home
/// directory. A tray started by `launchd` or a desktop autostart entry gets
/// the system's bare `PATH`, not the one the user's shell profile builds, so
/// none of these would otherwise be searched.
const HOME_BIN_DIRS: &[&str] = &[
    ".local/bin",    // the native installer
    ".claude/local", // the older self-managed install
    ".npm-global/bin",
    ".bun/bin",
    ".volta/bin",
];

/// The same, for system-wide installs: Homebrew on Apple silicon, on Intel,
/// and on Linux.
const SYSTEM_BIN_DIRS: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/home/linuxbrew/.linuxbrew/bin",
];

/// The directories to search for `claude`, and to hand the child as its
/// `PATH`: everything already on `path_env`, in order, followed by the
/// well-known install directories that are not on it yet.
///
/// The inherited entries stay first so a user who arranged their `PATH` on
/// purpose keeps the `claude` they chose. The child gets the whole list, not
/// just the resolved binary, because an npm-installed `claude` is a script
/// that needs to find `node` next to it.
pub fn search_dirs(path_env: Option<&OsStr>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path_env
        .map(|path| std::env::split_paths(path).collect())
        .unwrap_or_default();
    let home_dirs = home
        .into_iter()
        .flat_map(|home| HOME_BIN_DIRS.iter().map(move |dir| home.join(dir)));
    let system_dirs = SYSTEM_BIN_DIRS.iter().map(PathBuf::from);
    for dir in home_dirs.chain(system_dirs) {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// Whether `path` is a file this process could run. `metadata` follows
/// symlinks: the native installer's `~/.local/bin/claude` is a link to a
/// versioned binary.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// The first executable `claude` in `dirs`, if any.
pub fn find_claude(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| dir.join("claude"))
        .find(|candidate| is_executable(candidate))
}

/// The binary to run and the `PATH` to run it with.
///
/// `recorded` is where the statusline hook last saw `claude` (see
/// `source::with_claude_path`). It comes first when it still exists, because
/// the hook ran inside Claude Code and saw the user's real `PATH`; the search
/// through `dirs` is the fallback for a hook that has not recorded one yet.
/// The binary's own directory leads the child's `PATH`, since that is where
/// an npm install keeps the `node` its script needs.
pub fn resolve(
    recorded: Option<PathBuf>,
    mut dirs: Vec<PathBuf>,
) -> Option<(PathBuf, Vec<PathBuf>)> {
    let claude = recorded
        .filter(|path| is_executable(path))
        .or_else(|| find_claude(&dirs))?;
    if let Some(parent) = claude.parent() {
        dirs.retain(|dir| dir != parent);
        dirs.insert(0, parent.to_path_buf());
    }
    Some((claude, dirs))
}

/// Spawns `claude -p "/usage"` detached and returns immediately. The child's
/// output is discarded — the write it makes to `.claude.json` is the entire
/// point, and the next poll tick picks that up. Failures are silent by
/// design: the tray must keep working on machines where `claude` is not
/// installed, and [`should_refresh`]'s attempt-spacing already prevents retry
/// storms.
pub fn spawn_refresh(recorded: Option<PathBuf>) {
    std::thread::spawn(move || {
        let home = dirs::home_dir();
        let dirs = search_dirs(std::env::var_os("PATH").as_deref(), home.as_deref());
        let Some((claude, dirs)) = resolve(recorded, dirs) else {
            return;
        };
        let mut command = std::process::Command::new(claude);
        if let Ok(path) = std::env::join_paths(&dirs) {
            command.env("PATH", path);
        }
        let Ok(mut child) = command
            .args(["-p", "/usage"])
            // A neutral working directory, so the run is not attributed to
            // whatever directory the tray happened to start in.
            .current_dir(home.unwrap_or_else(|| PathBuf::from("/")))
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
        assert!(!should_refresh(false, None, None, None, ts(NOW)));
    }

    #[test]
    fn no_blob_and_never_attempted_refreshes() {
        assert!(should_refresh(true, None, None, None, ts(NOW)));
    }

    #[test]
    fn a_fresh_blob_does_not_refresh() {
        assert!(!should_refresh(
            true,
            Some(ts(NOW - 600)),
            None,
            None,
            ts(NOW)
        ));
    }

    #[test]
    fn a_blob_older_than_an_hour_refreshes() {
        assert!(should_refresh(
            true,
            Some(ts(NOW - 3601)),
            None,
            None,
            ts(NOW)
        ));
    }

    #[test]
    fn a_recent_attempt_blocks_retry_even_with_an_old_blob() {
        // The attempt failed (blob still old) — do not respawn every tick.
        assert!(!should_refresh(
            true,
            Some(ts(NOW - 7200)),
            None,
            Some(ts(NOW - 120)),
            ts(NOW)
        ));
    }

    #[test]
    fn an_old_attempt_allows_another_try() {
        assert!(should_refresh(
            true,
            Some(ts(NOW - 7200)),
            None,
            Some(ts(NOW - 3601)),
            ts(NOW)
        ));
    }

    #[test]
    fn a_future_dated_blob_counts_as_fresh_not_negative_age() {
        // Clock skew: a blob "from the future" means the writer's clock ran
        // ahead, not that a refresh is due.
        assert!(!should_refresh(
            true,
            Some(ts(NOW + 600)),
            None,
            None,
            ts(NOW)
        ));
    }

    #[test]
    fn activity_since_the_fetch_refreshes_after_ten_minutes() {
        let fetched = Some(ts(NOW - 601));
        let activity = Some(ts(NOW - 5));
        assert!(should_refresh(true, fetched, activity, None, ts(NOW)));
    }

    #[test]
    fn activity_since_the_fetch_still_waits_out_the_ten_minutes() {
        let fetched = Some(ts(NOW - 300));
        let activity = Some(ts(NOW - 5));
        assert!(!should_refresh(true, fetched, activity, None, ts(NOW)));
    }

    #[test]
    fn activity_older_than_the_fetch_keeps_the_hourly_cadence() {
        // The blob already covers everything the hook reported: idle.
        let fetched = Some(ts(NOW - 1800));
        let activity = Some(ts(NOW - 2400));
        assert!(!should_refresh(true, fetched, activity, None, ts(NOW)));
    }

    #[test]
    fn a_failed_attempt_while_active_is_retried_after_ten_minutes_not_sooner() {
        let fetched = Some(ts(NOW - 7200));
        let activity = Some(ts(NOW - 5));
        let recent = Some(ts(NOW - 120));
        let earlier = Some(ts(NOW - 601));
        assert!(!should_refresh(true, fetched, activity, recent, ts(NOW)));
        assert!(should_refresh(true, fetched, activity, earlier, ts(NOW)));
    }

    fn fake_claude(dir: &Path, mode: u32) {
        std::fs::create_dir_all(dir).expect("create bin dir");
        let path = dir.join("claude");
        std::fs::write(&path, "#!/bin/sh\n").expect("write claude");
        crate::testutil::set_mode(&path, mode);
    }

    #[test]
    fn search_dirs_adds_the_install_dirs_a_launchd_path_lacks() {
        // The `PATH` a LaunchAgent really gets on macOS.
        let dirs = search_dirs(
            Some(OsStr::new("/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin")),
            Some(Path::new("/Users/someone")),
        );
        assert_eq!(dirs[0], Path::new("/usr/bin"), "inherited entries first");
        assert!(dirs.contains(&PathBuf::from("/Users/someone/.local/bin")));
        assert!(dirs.contains(&PathBuf::from("/opt/homebrew/bin")));
        assert_eq!(
            dirs.iter()
                .filter(|dir| *dir == Path::new("/usr/local/bin"))
                .count(),
            1,
            "a directory already on PATH is not repeated"
        );
    }

    #[test]
    fn search_dirs_without_a_path_or_home_still_has_the_system_dirs() {
        let dirs = search_dirs(None, None);
        assert!(dirs.contains(&PathBuf::from("/opt/homebrew/bin")));
    }

    #[test]
    fn find_claude_sees_a_binary_outside_the_inherited_path() {
        let temp = crate::testutil::TempDir::new("cli-refresh-find");
        let home = temp.path().join("home");
        fake_claude(&home.join(".local/bin"), 0o755);

        let dirs = search_dirs(Some(OsStr::new("/usr/bin:/bin")), Some(&home));
        // Skip on a host that has a real `claude` in /usr/bin or /bin.
        if dirs[..2].iter().any(|dir| dir.join("claude").exists()) {
            return;
        }
        assert_eq!(find_claude(&dirs), Some(home.join(".local/bin/claude")));
    }

    #[test]
    fn find_claude_prefers_the_inherited_path() {
        let temp = crate::testutil::TempDir::new("cli-refresh-prefer");
        let chosen = temp.path().join("chosen");
        let home = temp.path().join("home");
        fake_claude(&chosen, 0o755);
        fake_claude(&home.join(".local/bin"), 0o755);

        let dirs = search_dirs(Some(chosen.as_os_str()), Some(&home));
        assert_eq!(find_claude(&dirs), Some(chosen.join("claude")));
    }

    #[test]
    fn find_claude_skips_a_file_that_is_not_executable() {
        let temp = crate::testutil::TempDir::new("cli-refresh-noexec");
        fake_claude(temp.path(), 0o644);
        assert_eq!(find_claude(&[temp.path().to_path_buf()]), None);
    }

    #[test]
    fn find_claude_with_nothing_installed_is_none() {
        let temp = crate::testutil::TempDir::new("cli-refresh-none");
        assert_eq!(find_claude(&[temp.path().to_path_buf()]), None);
    }

    #[test]
    fn resolve_prefers_the_path_the_hook_recorded() {
        let temp = crate::testutil::TempDir::new("cli-refresh-recorded");
        let odd = temp.path().join("nvm/versions/node/bin");
        let usual = temp.path().join("usual");
        fake_claude(&odd, 0o755);
        fake_claude(&usual, 0o755);

        let (claude, dirs) =
            resolve(Some(odd.join("claude")), vec![usual.clone()]).expect("resolves");
        assert_eq!(claude, odd.join("claude"));
        assert_eq!(dirs, vec![odd, usual], "the binary's directory leads PATH");
    }

    #[test]
    fn resolve_falls_back_to_the_search_when_the_recorded_path_is_gone() {
        let temp = crate::testutil::TempDir::new("cli-refresh-gone");
        let usual = temp.path().join("usual");
        fake_claude(&usual, 0o755);

        let (claude, dirs) = resolve(
            Some(temp.path().join("uninstalled/claude")),
            vec![PathBuf::from("/usr/bin"), usual.clone()],
        )
        .expect("resolves");
        assert_eq!(claude, usual.join("claude"));
        assert_eq!(dirs, vec![usual, PathBuf::from("/usr/bin")]);
    }

    #[test]
    fn resolve_with_nothing_recorded_and_nothing_installed_is_none() {
        let temp = crate::testutil::TempDir::new("cli-refresh-nothing");
        assert_eq!(resolve(None, vec![temp.path().to_path_buf()]), None);
    }
}
