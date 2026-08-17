//! One tray, one machine: an advisory lock that a second launch trips over.
//!
//! Two copies of the tray would draw two icons, emit every notification twice,
//! and fight over the same config file, so the second one refuses to start.
//!
//! The mechanism is `flock(2)` on a file in a per-user directory
//! ([`lock_path`]), held open for the life of the process. That choice is
//! deliberate: a lock taken this way is owned by the *open file description*,
//! so the kernel drops it when the process exits, however it exits. A PID file
//! would need stale-entry cleanup after a crash or a kill -9; this needs none,
//! and a leftover lock file on disk means nothing on its own.
//!
//! Both supported platforms are Unix, so there is a single implementation. The
//! path is a parameter rather than a constant so the tests can lock inside a
//! temp directory instead of the developer's real per-user directory.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// File name of the lock, inside whichever directory [`lock_path`] picks.
const LOCK_NAME: &str = "claude-usage-tray.lock";

/// A held lock. Releasing it is closing the file, so the lock lasts exactly as
/// long as this value — which, in the tray, is the whole process.
#[derive(Debug)]
pub struct InstanceLock {
    // Read only to compare identity with the on-disk file; the load-bearing
    // part is it staying open, since `flock` is released by the close that
    // `File`'s `Drop` performs.
    _file: File,
}

impl InstanceLock {
    /// Records this process's PID in the lock file, so a later
    /// `claude-usage-tray restart` knows who to ask to leave.
    ///
    /// The PID is advisory only: the lock is what proves an instance is alive,
    /// and the number is just how the replacement finds it. A failed write is
    /// therefore not worth failing the startup over — `restart` degrades to
    /// "could not identify the running instance" and refuses to guess.
    pub fn record_pid(&self) {
        let _ = write_pid(&self._file, std::process::id());
    }

    /// Whether this lock's open file is still the file at `path`. False after
    /// the lock file has been deleted or replaced under the holder — at which
    /// point the flock, though still held, protects a file nobody else can
    /// see, and a second instance would sail right past it.
    pub fn matches(&self, path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let Ok(held) = self._file.metadata() else { return false };
        let Ok(on_disk) = std::fs::metadata(path) else { return false };
        held.dev() == on_disk.dev() && held.ino() == on_disk.ino()
    }
}

/// Hands back a lock that is actually visible at `path`, re-acquiring if the
/// file was deleted or replaced under the current one. When somebody else
/// already took the fresh file, the original lock is kept — worthless as a
/// barrier now, but at that point *this* process is the one whose claim is
/// ambiguous, so it must not fight; the caller's next revalidation tries
/// again, and the loss is reported once on stderr.
pub fn revalidate(path: &Path, lock: InstanceLock) -> InstanceLock {
    if lock.matches(path) {
        return lock;
    }
    match try_acquire(path) {
        Ok(Some(fresh)) => {
            fresh.record_pid();
            fresh
        }
        _ => {
            // Not eprintln!: a panicking write on a piped stderr would take
            // the warden thread — and the lock — down with it.
            use std::io::Write as _;
            let _ = writeln!(
                io::stderr(),
                "claude-usage-tray: the lock file at {} was replaced under this instance \
                 and could not be re-taken; another instance may now be running",
                path.display()
            );
            lock
        }
    }
}

/// Truncates `file` and writes `pid` into it, followed by a newline so the
/// file reads sensibly with `cat`.
fn write_pid(mut file: &File, pid: u32) -> io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(format!("{pid}\n").as_bytes())?;
    file.flush()
}

/// The one spelling of "run `exe arg` detached": own process group, stdio on
/// `/dev/null`. [`spawn_detached`] and [`spawn_watched`] both build on it.
fn detached_command(exe: &Path, arg: &str) -> std::process::Command {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(exe);
    command
        .arg(arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    command
}

/// Starts `exe arg` fully detached, all standard streams on `/dev/null`.
/// Used by the parent that backgrounds the tray; the `Restart to update`
/// menu row uses [`spawn_watched`] instead, keeping the child's stderr.
pub fn spawn_detached(exe: &Path, arg: &str) -> io::Result<std::process::Child> {
    detached_command(exe, arg).spawn()
}

/// Like [`spawn_detached`], but with the child's stderr piped back so the
/// caller can learn *why* a restart failed. The caller owns reaping the
/// child (`wait_with_output`) — a dropped `Child` here would be a zombie per
/// click of the restart row.
pub fn spawn_watched(exe: &Path, arg: &str) -> io::Result<std::process::Child> {
    let mut command = detached_command(exe, arg);
    command.stderr(std::process::Stdio::piped());
    command.spawn()
}

/// Parses the contents of a lock file into a PID.
///
/// Anything that is not a plain positive integer is rejected rather than
/// guessed at: the number is about to be handed to `kill`, and the cost of
/// being wrong is signalling an unrelated process.
pub fn parse_pid(contents: &str) -> Option<i32> {
    let pid: i32 = contents.trim().parse().ok()?;
    (pid > 1).then_some(pid)
}

/// Reads the PID recorded in the lock file at `path`, if there is a usable one.
pub fn read_pid(path: &Path) -> Option<i32> {
    parse_pid(&std::fs::read_to_string(path).ok()?)
}

/// Asks the process to exit with `SIGTERM`. Returns false only when the signal
/// could not be delivered for a reason other than "it is already gone".
pub fn terminate(pid: i32) -> bool {
    // SAFETY: `kill` reads no memory; it takes a PID and a signal number.
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// What a create-nothing look at a lock path found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// Somebody holds the flock right now.
    Held,
    /// Nothing holds it — including "the file does not even exist".
    Free,
    /// The file exists but could not be opened or locked, so there is no
    /// answer. Callers decide how much benefit of the doubt that gets.
    Unknown,
}

/// Asks whether the lock at `path` is held, creating nothing on disk.
///
/// Unlike [`try_acquire`] this never creates the file or its directory —
/// essential for the legacy-path checks, which would otherwise re-create the
/// very file the lock moved away from, forever. It is *not* contention-free:
/// distinguishing Free from Held means taking `LOCK_EX` for a moment, which
/// is why single-shot claimants were retired (see the launch patience
/// constants in main.rs).
pub fn probe_held(path: &Path) -> Probe {
    let file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Probe::Free,
        Err(_) => return Probe::Unknown,
    };
    // SAFETY: `flock` takes a file descriptor and a flag word and touches
    // nothing else; the descriptor is live for the whole call. (LOCK_EX does
    // not require a writable descriptor.)
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        // Taking it proved it free; dropping `file` releases it again.
        return Probe::Free;
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::EWOULDBLOCK) => Probe::Held,
        _ => Probe::Unknown,
    }
}

/// Polls [`try_acquire`] until the lock is won, an I/O error says waiting
/// will not help, or `timeout` passes (`Ok(None)`). A zero timeout is a
/// single immediate attempt.
///
/// This is how a freshly spawned tray tolerates its own parent: `detach` and
/// `restart` hold the lock *through* the spawn precisely so concurrent
/// launches serialize on the file, which means the child's first attempts may
/// find its parent still holding on for a few more milliseconds.
pub fn acquire_with_retry(
    path: &Path,
    timeout: std::time::Duration,
) -> io::Result<Option<InstanceLock>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(lock) = try_acquire(path)? {
            return Ok(Some(lock));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Picks the directory the lock file lives in, given what the platform
/// offers. Pure so both platform branches are testable from either OS.
///
/// Linux: the per-user runtime directory (`$XDG_RUNTIME_DIR` — tmpfs, wiped
/// between logins, exactly what runtime state is for), then cache, then temp.
/// macOS: `~/Library/Application Support`, then temp — deliberately *not*
/// `~/Library/Caches`, which macOS may purge under disk pressure. A purged
/// lock file would let a second tray start while the first still runs.
fn choose_lock_dir(
    macos: bool,
    runtime: Option<PathBuf>,
    data_local: Option<PathBuf>,
    cache: Option<PathBuf>,
    temp: PathBuf,
) -> PathBuf {
    if macos {
        data_local.unwrap_or(temp)
    } else {
        runtime.or(cache).unwrap_or(temp)
    }
}

/// Directories earlier releases kept the lock in. A freshly upgraded binary
/// must still notice — and be able to stop — a tray from before the move, or
/// "Restart to update" would start a second instance during the one upgrade
/// that crosses the move.
fn choose_legacy_lock_dirs(macos: bool, cache: Option<PathBuf>) -> Vec<PathBuf> {
    if macos { cache.into_iter().collect() } else { Vec::new() }
}

/// Where the lock file lives. See [`choose_lock_dir`] for the reasoning.
pub fn lock_path() -> PathBuf {
    choose_lock_dir(
        cfg!(target_os = "macos"),
        dirs::runtime_dir(),
        dirs::data_local_dir(),
        dirs::cache_dir(),
        std::env::temp_dir(),
    )
    .join(LOCK_NAME)
}

/// Lock files earlier releases may still be holding. Empty on Linux.
pub fn legacy_lock_paths() -> Vec<PathBuf> {
    choose_legacy_lock_dirs(cfg!(target_os = "macos"), dirs::cache_dir())
        .into_iter()
        .map(|dir| dir.join(LOCK_NAME))
        .collect()
}

/// Tries to take the lock at `path`.
///
/// `Ok(Some(lock))` means this process now owns it; `Ok(None)` means another
/// process already does, so the caller should refuse to start. An `Err` means
/// the lock file itself could not be created or opened — a read-only runtime
/// directory, say — which is not a reason to keep the user from running the
/// tray, so callers treat it as "go ahead, unlocked".
pub fn try_acquire(path: &Path) -> io::Result<Option<InstanceLock>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;

    // SAFETY: `flock` takes a file descriptor and a flag word and touches
    // nothing else; the descriptor is live for the whole call because `file`
    // outlives it.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(InstanceLock { _file: file }));
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // Somebody else holds it. This is the one error that is not a failure:
        // it is the answer. (`EWOULDBLOCK` and `EAGAIN` are the same number on
        // Linux and macOS alike, so naming one covers both.)
        Some(libc::EWOULDBLOCK) => Ok(None),
        _ => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// The whole contract, and the reason a second launch can detect a first
    /// one: `flock` locks are held by the open file description, not by the
    /// process, so a second `open` of the same path conflicts even from inside
    /// the same process. (`flock(2)`: "If a process uses open(2) ... to obtain
    /// more than one file descriptor for the same file, these file descriptors
    /// are treated independently by flock(). An attempt to lock the file using
    /// one of these file descriptors may be denied by a lock that the calling
    /// process has already placed via another file descriptor.")
    #[test]
    fn a_second_acquire_is_refused_while_the_first_is_held() {
        let temp = TempDir::new("instance-lock");
        let path = temp.path().join("tray.lock");

        let first = try_acquire(&path).expect("first acquire").expect("free");
        assert!(
            try_acquire(&path).expect("second acquire").is_none(),
            "a second instance must be refused"
        );
        drop(first);
    }

    #[test]
    fn the_lock_is_available_again_once_it_is_released() {
        let temp = TempDir::new("instance-release");
        let path = temp.path().join("tray.lock");

        let first = try_acquire(&path).expect("acquire").expect("free");
        drop(first);

        let second = try_acquire(&path).expect("re-acquire");
        assert!(
            second.is_some(),
            "closing the file must release the lock, leftover file or not"
        );
        assert!(path.exists(), "the lock file itself is expected to linger");
    }

    #[test]
    fn acquiring_creates_the_directory_and_the_file() {
        let temp = TempDir::new("instance-create");
        let path = temp.path().join("nested").join("tray.lock");

        let lock = try_acquire(&path).expect("acquire").expect("free");
        assert!(path.is_file());
        drop(lock);
    }

    /// An existing lock file left behind by a process that is gone is not a
    /// lock: nothing has to clean it up.
    #[test]
    fn a_stale_lock_file_does_not_block_anything() {
        let temp = TempDir::new("instance-stale");
        let path = temp.path().join("tray.lock");
        std::fs::write(&path, b"leftover").expect("write stale file");

        let lock = try_acquire(&path).expect("acquire").expect("stale file is free");
        drop(lock);
    }

    #[test]
    fn the_holder_records_its_own_pid() {
        let temp = TempDir::new("instance-pid");
        let path = temp.path().join("tray.lock");

        let lock = try_acquire(&path).expect("acquire").expect("free");
        lock.record_pid();
        assert_eq!(read_pid(&path), Some(std::process::id() as i32));
        drop(lock);
    }

    /// Re-recording must replace the number, not append to it: a lock file
    /// reading "1234\n5678" would either fail to parse or name the wrong
    /// process.
    #[test]
    fn recording_a_pid_replaces_the_previous_one() {
        let temp = TempDir::new("instance-pid-replace");
        let path = temp.path().join("tray.lock");
        std::fs::write(&path, b"999999\n").expect("write old pid");

        let lock = try_acquire(&path).expect("acquire").expect("free");
        lock.record_pid();
        drop(lock);

        let body = std::fs::read_to_string(&path).expect("read");
        assert_eq!(body.trim(), std::process::id().to_string());
    }

    #[test]
    fn pid_parsing_accepts_a_plain_number_with_surrounding_whitespace() {
        assert_eq!(parse_pid("4321"), Some(4321));
        assert_eq!(parse_pid("  4321\n"), Some(4321));
    }

    /// Everything else is refused, because the next thing that happens to the
    /// number is a signal being sent to it.
    #[test]
    fn pid_parsing_refuses_anything_that_is_not_a_real_pid() {
        for garbage in ["", "   ", "abc", "12x", "-1", "0", "1", "12 34", "3.5"] {
            assert_eq!(parse_pid(garbage), None, "accepted {garbage:?}");
        }
    }

    #[test]
    fn reading_a_pid_from_a_missing_or_empty_file_gives_nothing() {
        let temp = TempDir::new("instance-pid-missing");
        assert_eq!(read_pid(&temp.path().join("absent.lock")), None);
        let empty = temp.path().join("empty.lock");
        std::fs::write(&empty, b"").expect("write empty");
        assert_eq!(read_pid(&empty), None);
    }

    #[test]
    fn probing_reports_held_free_and_missing_without_creating_anything() {
        let temp = TempDir::new("instance-probe");
        let path = temp.path().join("nested").join("tray.lock");
        assert_eq!(probe_held(&path), Probe::Free);
        assert!(!path.parent().unwrap().exists(), "a probe must not create directories");

        let held = try_acquire(&path).expect("acquire").expect("free");
        assert_eq!(probe_held(&path), Probe::Held);
        drop(held);
        assert_eq!(probe_held(&path), Probe::Free);
    }

    #[test]
    fn a_retrying_acquire_gets_the_lock_when_the_holder_leaves() {
        let temp = TempDir::new("instance-retry-succeeds");
        let path = temp.path().join("tray.lock");
        let held = try_acquire(&path).expect("acquire").expect("free");
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let handle = {
            let path = path.clone();
            std::thread::spawn(move || {
                // Handshake: the holder must still be holding when the retry
                // starts, or this degrades into a plain try_acquire test.
                ready_tx.send(()).expect("send");
                acquire_with_retry(&path, std::time::Duration::from_secs(10))
            })
        };
        ready_rx.recv().expect("recv");
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(held);
        let lock = handle.join().expect("join").expect("io ok");
        assert!(lock.is_some(), "the retry must win once the holder is gone");
    }

    #[test]
    fn a_retrying_acquire_gives_up_when_the_holder_stays() {
        let temp = TempDir::new("instance-retry-gives-up");
        let path = temp.path().join("tray.lock");
        let held = try_acquire(&path).expect("acquire").expect("free");
        let lock = acquire_with_retry(&path, std::time::Duration::from_millis(250)).expect("io ok");
        assert!(lock.is_none());
        drop(held);
    }

    #[test]
    fn a_zero_timeout_acquire_is_a_single_immediate_attempt() {
        let temp = TempDir::new("instance-retry-zero");
        let path = temp.path().join("tray.lock");
        let held = try_acquire(&path).expect("acquire").expect("free");
        let started = std::time::Instant::now();
        let lock = acquire_with_retry(&path, std::time::Duration::ZERO).expect("io ok");
        assert!(lock.is_none());
        // Discriminates "no sleep" from "one 100ms sleep" with room for a loaded
        // machine; the filesystem work itself is microseconds on a local disk.
        assert!(started.elapsed() < std::time::Duration::from_millis(90), "must not sleep");
        drop(held);
    }

    #[test]
    fn a_held_lock_matches_its_path_until_the_file_is_replaced() {
        let temp = TempDir::new("instance-matches");
        let path = temp.path().join("tray.lock");
        let held = try_acquire(&path).expect("acquire").expect("free");
        assert!(held.matches(&path));
        std::fs::remove_file(&path).expect("delete out from under the holder");
        assert!(!held.matches(&path));
        drop(held);
    }

    #[test]
    fn revalidating_a_deleted_lock_takes_a_fresh_one_and_records_the_pid() {
        let temp = TempDir::new("instance-revalidate");
        let path = temp.path().join("tray.lock");
        let held = try_acquire(&path).expect("acquire").expect("free");
        std::fs::remove_file(&path).expect("delete out from under the holder");

        let renewed = revalidate(&path, held);
        assert!(renewed.matches(&path), "the returned lock must be on the current file");
        assert_eq!(read_pid(&path), Some(std::process::id() as i32));
        // The fresh lock must actually be held.
        assert!(try_acquire(&path).expect("probe").is_none());
        drop(renewed);
    }

    #[test]
    fn revalidating_an_intact_lock_changes_nothing() {
        let temp = TempDir::new("instance-revalidate-noop");
        let path = temp.path().join("tray.lock");
        let held = try_acquire(&path).expect("acquire").expect("free");
        let same = revalidate(&path, held);
        assert!(same.matches(&path));
        drop(same);
    }

    #[test]
    fn a_watched_spawn_pipes_stderr_and_can_be_reaped() {
        let temp = TempDir::new("instance-watched");
        let script = temp.path().join("fail.sh");
        std::fs::write(&script, "#!/bin/sh\necho boom >&2\nexit 4\n").expect("write script");
        crate::testutil::set_mode(&script, 0o755);

        let child = match spawn_watched(&script, "unused") {
            Ok(child) => child,
            // A noexec temp mount cannot run the fixture; that is the host's
            // shape, not a defect in spawn_watched.
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(err) => panic!("spawn failed: {err}"),
        };
        let output = child.wait_with_output().expect("wait");
        assert_eq!(output.status.code(), Some(4));
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "boom");
    }

    /// Signalling a PID that no longer exists is success: the end state the
    /// caller wanted ("that process is not running") already holds.
    #[test]
    fn terminating_a_process_that_is_already_gone_counts_as_done() {
        // A PID that cannot be running: `kill` on an unused high PID gives
        // ESRCH. 0x7FFF_FFF0 is above every default `pid_max`.
        assert!(terminate(0x7FFF_FFF0));
    }

    /// The default path is a file named for the program, in a directory that
    /// exists per user rather than per machine.
    #[test]
    fn the_default_lock_path_is_named_after_the_program() {
        let path = lock_path();
        assert_eq!(path.file_name().unwrap(), LOCK_NAME);
        assert!(path.parent().is_some());
    }

    #[test]
    fn the_lock_dir_on_macos_prefers_application_support_over_caches() {
        // ~/Library/Caches is purgeable on macOS: a purged lock file would let a
        // second tray start. Application Support is not.
        let dir = choose_lock_dir(
            true,
            None, // macOS has no runtime dir
            Some(PathBuf::from("/u/Library/Application Support")),
            Some(PathBuf::from("/u/Library/Caches")),
            PathBuf::from("/tmp"),
        );
        assert_eq!(dir, PathBuf::from("/u/Library/Application Support"));
    }

    #[test]
    fn the_lock_dir_on_macos_falls_back_to_temp_when_data_local_is_unknown() {
        let dir = choose_lock_dir(
            true,
            None,
            None,
            Some(PathBuf::from("/u/Library/Caches")),
            PathBuf::from("/tmp"),
        );
        assert_eq!(dir, PathBuf::from("/tmp"));
    }

    #[test]
    fn the_lock_dir_on_linux_is_unchanged_runtime_then_cache_then_temp() {
        let runtime = Some(PathBuf::from("/run/user/1000"));
        let cache = Some(PathBuf::from("/u/.cache"));
        let data = Some(PathBuf::from("/u/.local/share"));
        assert_eq!(
            choose_lock_dir(false, runtime, data.clone(), cache.clone(), PathBuf::from("/tmp")),
            PathBuf::from("/run/user/1000")
        );
        assert_eq!(
            choose_lock_dir(false, None, data, cache, PathBuf::from("/tmp")),
            PathBuf::from("/u/.cache")
        );
        assert_eq!(
            choose_lock_dir(false, None, None, None, PathBuf::from("/tmp")),
            PathBuf::from("/tmp")
        );
    }

    #[test]
    fn legacy_lock_dirs_exist_only_on_macos_and_point_at_caches() {
        assert_eq!(
            choose_legacy_lock_dirs(true, Some(PathBuf::from("/u/Library/Caches"))),
            vec![PathBuf::from("/u/Library/Caches")]
        );
        assert!(choose_legacy_lock_dirs(true, None).is_empty());
        assert!(choose_legacy_lock_dirs(false, Some(PathBuf::from("/u/.cache"))).is_empty());
    }

    #[test]
    fn legacy_lock_paths_join_the_lock_name_onto_each_legacy_dir() {
        // Exercised through the pure chooser so the assertion runs on Linux too,
        // where legacy_lock_paths() itself is empty.
        let paths: Vec<PathBuf> = choose_legacy_lock_dirs(true, Some(PathBuf::from("/u/Library/Caches")))
            .into_iter()
            .map(|dir| dir.join(LOCK_NAME))
            .collect();
        assert_eq!(paths, vec![PathBuf::from("/u/Library/Caches").join(LOCK_NAME)]);
        assert!(legacy_lock_paths().iter().all(|p| p.file_name().unwrap() == LOCK_NAME));
    }
}
