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

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
