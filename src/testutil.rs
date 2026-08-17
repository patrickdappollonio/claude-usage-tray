//! Test-only helpers. Compiled out of the real binary.

use std::path::{Path, PathBuf};

/// A unique, self-cleaning temp directory.
///
/// Every test that touches the filesystem uses one of these, so no test ever
/// reads or writes the developer's real `~/.config` or `~/.claude`.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "claude-usage-tray-test-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// Sets the permission bits of `path`. Used by the capability-probe tests to
/// build a directory that cannot be written to.
pub fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("set mode");
}

/// Spawns a just-written script fixture, absorbing the two host-shaped ways
/// that spawn can fail.
///
/// `ETXTBSY` is the interesting one: another test's `fork` can inherit the
/// write handle this thread's `fs::write` already closed, and for the width of
/// that child's fork→exec window the kernel sees the script as open for
/// writing and refuses to exec it. Nothing in the fixture is wrong, so retry
/// briefly instead of failing the suite. `PermissionDenied` means a `noexec`
/// temp mount that can never run the fixture, so the caller skips.
pub fn spawn_script(
    mut spawn: impl FnMut() -> std::io::Result<std::process::Child>,
) -> Option<std::process::Child> {
    for _ in 0..20 {
        match spawn() {
            Ok(child) => return Some(child),
            Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return None,
            Err(err) => panic!("spawn failed: {err}"),
        }
    }
    panic!("spawn stayed ETXTBSY after 20 attempts");
}

/// Sets the modification time of `path` to `secs` since the epoch. Used by the
/// binary-swap tests to move an mtime deliberately instead of hoping the clock
/// ticks between two writes.
pub fn set_mtime(path: &Path, secs: i64) {
    use std::os::unix::ffi::OsStrExt;

    let raw = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path without NUL");
    let times = [
        libc::timeval {
            tv_sec: secs as libc::time_t,
            tv_usec: 0,
        },
        libc::timeval {
            tv_sec: secs as libc::time_t,
            tv_usec: 0,
        },
    ];
    // SAFETY: a NUL-terminated path and a two-element `timeval` array, which is
    // exactly what `utimes` reads.
    let rc = unsafe { libc::utimes(raw.as_ptr(), times.as_ptr()) };
    assert_eq!(rc, 0, "utimes failed: {}", std::io::Error::last_os_error());
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
