//! One tray, one machine: an advisory lock that a second launch trips over.
//!
//! Two copies of the tray would draw two icons, emit every notification twice,
//! and fight over the same config file, so the second one refuses to start.
//!
//! The mechanism is `flock(2)` on a file in the runtime directory, held open
//! for the life of the process. That choice is deliberate: a lock taken this
//! way is owned by the *open file description*, so the kernel drops it when the
//! process exits, however it exits. A PID file would need stale-entry cleanup
//! after a crash or a kill -9; this needs none, and a leftover lock file on
//! disk means nothing on its own.
//!
//! Both supported platforms are Unix, so there is a single implementation. The
//! path is a parameter rather than a constant so the tests can lock inside a
//! temp directory instead of the developer's real runtime directory.

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
    // Never read: the point is the file staying open. `flock` is released by
    // the close that `File`'s `Drop` performs.
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

    /// Keeps the lock for the rest of the process, deliberately never closing
    /// the file. Used by the tray, which holds it until it exits.
    pub fn hold_forever(self) {
        std::mem::forget(self);
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

/// Starts `exe arg` as a detached child: standard streams on `/dev/null` and a
/// process group of its own, so it outlives the terminal (or the tray) that
/// started it.
///
/// The single place that spelling lives, because two callers need exactly the
/// same one: the parent that backgrounds the tray, and the `Restart to update`
/// menu row that starts the newly installed binary's `restart`.
pub fn spawn_detached(exe: &Path, arg: &str) -> io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;

    std::process::Command::new(exe)
        .arg(arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
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

/// Polls until the lock at `path` can be taken (releasing it again
/// immediately), or `timeout` elapses. True means the previous holder is gone.
pub fn wait_until_free(path: &Path, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match try_acquire(path) {
            // Taking it and dropping it straight away is the probe: something
            // has to actually be able to lock the file for it to be free.
            Ok(Some(lock)) => {
                drop(lock);
                return true;
            }
            // Unlockable for a reason that waiting will not fix.
            Err(_) => return false,
            Ok(None) => {}
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Where the lock file lives: the per-user runtime directory when the platform
/// has one (`$XDG_RUNTIME_DIR` on Linux — tmpfs, wiped between logins, exactly
/// what runtime state is for), otherwise the cache directory, otherwise the
/// system temp directory. macOS has no runtime directory, so it lands in
/// `~/Library/Caches`.
pub fn lock_path() -> PathBuf {
    dirs::runtime_dir()
        .or_else(dirs::cache_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join(LOCK_NAME)
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
    fn waiting_returns_immediately_when_the_lock_is_free() {
        let temp = TempDir::new("instance-wait-free");
        let path = temp.path().join("tray.lock");
        let started = std::time::Instant::now();
        assert!(wait_until_free(&path, std::time::Duration::from_secs(10)));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn waiting_gives_up_when_the_lock_stays_held() {
        let temp = TempDir::new("instance-wait-held");
        let path = temp.path().join("tray.lock");
        let held = try_acquire(&path).expect("acquire").expect("free");
        assert!(!wait_until_free(&path, std::time::Duration::from_millis(250)));
        drop(held);
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
}
