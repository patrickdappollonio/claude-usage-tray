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

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
