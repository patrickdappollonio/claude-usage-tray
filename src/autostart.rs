//! "Launch at login" via the XDG autostart spec.
//!
//! Enabling writes a `.desktop` entry to `~/.config/autostart/`; disabling
//! removes it; the checkbox state is simply "does that file exist". Every
//! desktop that implements the XDG autostart spec — KDE, GNOME, XFCE, LXQt,
//! Cinnamon, MATE — honours the same directory, so there is nothing
//! desktop-specific here.
//!
//! Nothing in this module panics: I/O errors are returned to the caller, which
//! logs them and leaves the checkbox where it was.
//!
//! See `docs/superpowers/specs/2026-08-13-claude-usage-tray-design.md`.

use std::io;
use std::path::{Path, PathBuf};

/// File name of the autostart entry.
const ENTRY_NAME: &str = "claude-usage-tray.desktop";

/// Default autostart directory: `$XDG_CONFIG_HOME/autostart`, falling back to
/// `~/.config/autostart`.
pub fn default_autostart_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("autostart")
}

/// Path of the entry inside `dir`.
pub fn entry_path(dir: &Path) -> PathBuf {
    dir.join(ENTRY_NAME)
}

/// Builds the `.desktop` entry body for a given executable path.
///
/// Pure, so the exact keys are unit-tested without writing anything.
pub fn desktop_entry(exec: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Claude Usage Tray\n\
         Comment=Claude Code subscription usage in the system tray\n\
         Exec={}\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n",
        exec.display()
    )
}

/// True when the autostart entry exists in `dir`.
pub fn is_enabled_in(dir: &Path) -> bool {
    entry_path(dir).is_file()
}

/// Writes the autostart entry into `dir`, creating the directory if needed.
/// The write is atomic (temp file + rename) so a half-written entry is never
/// visible to the session manager.
pub fn enable_in(dir: &Path, exec: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = entry_path(dir);
    let temp = path.with_extension("desktop.tmp");
    std::fs::write(&temp, desktop_entry(exec))?;
    match std::fs::rename(&temp, &path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// Removes the autostart entry from `dir`. Removing an entry that isn't there
/// succeeds: the requested end state ("not autostarting") already holds.
pub fn disable_in(dir: &Path) -> io::Result<()> {
    match std::fs::remove_file(entry_path(dir)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// True when the entry exists in the real autostart directory.
pub fn is_enabled() -> bool {
    is_enabled_in(&default_autostart_dir())
}

/// Applies `enabled` to the real autostart directory, reporting whether the
/// end state was reached. Failures are logged and reported as `false` so the
/// caller can leave the checkbox unchanged rather than lying about the state.
pub fn set_enabled(enabled: bool) -> bool {
    let dir = default_autostart_dir();
    let result = if enabled {
        match std::env::current_exe() {
            Ok(exe) => enable_in(&dir, &exe),
            Err(err) => Err(err),
        }
    } else {
        disable_in(&dir)
    };
    match result {
        Ok(()) => true,
        Err(err) => {
            eprintln!(
                "claude-usage-tray: could not update {}: {err}",
                entry_path(&dir).display()
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn desktop_entry_has_the_required_keys() {
        let entry = desktop_entry(Path::new("/usr/local/bin/claude-usage-tray"));
        assert!(entry.starts_with("[Desktop Entry]\n"));
        for line in [
            "Type=Application",
            "Name=Claude Usage Tray",
            "Exec=/usr/local/bin/claude-usage-tray",
            "X-GNOME-Autostart-enabled=true",
        ] {
            assert!(entry.lines().any(|l| l == line), "missing {line:?}");
        }
    }

    #[test]
    fn desktop_entry_uses_the_absolute_exec_path_given() {
        let entry = desktop_entry(Path::new("/home/someone/bin/tray"));
        assert!(entry.contains("Exec=/home/someone/bin/tray\n"));
    }

    #[test]
    fn enable_creates_the_entry_and_the_directory() {
        let temp = TempDir::new("autostart-enable");
        let dir = temp.path().join("autostart");
        assert!(!is_enabled_in(&dir));

        enable_in(&dir, Path::new("/opt/tray")).expect("enable succeeds");
        assert!(is_enabled_in(&dir));
        let body = std::fs::read_to_string(entry_path(&dir)).expect("read entry");
        assert_eq!(body, desktop_entry(Path::new("/opt/tray")));
    }

    #[test]
    fn enable_is_idempotent_and_leaves_no_temp_file() {
        let temp = TempDir::new("autostart-idempotent");
        let dir = temp.path().to_path_buf();
        enable_in(&dir, Path::new("/opt/tray")).expect("first enable");
        enable_in(&dir, Path::new("/opt/tray")).expect("second enable");

        let names: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(names.len(), 1, "unexpected files: {names:?}");
        assert_eq!(names[0], ENTRY_NAME);
    }

    #[test]
    fn disable_removes_the_entry() {
        let temp = TempDir::new("autostart-disable");
        let dir = temp.path().to_path_buf();
        enable_in(&dir, Path::new("/opt/tray")).expect("enable");
        disable_in(&dir).expect("disable succeeds");
        assert!(!is_enabled_in(&dir));
    }

    #[test]
    fn disable_when_absent_succeeds() {
        let temp = TempDir::new("autostart-disable-absent");
        disable_in(temp.path()).expect("disabling an absent entry is fine");
        disable_in(&temp.path().join("never-created")).expect("missing dir is fine too");
    }

    #[test]
    fn is_enabled_ignores_a_directory_of_the_same_name() {
        let temp = TempDir::new("autostart-dir-collision");
        std::fs::create_dir_all(entry_path(temp.path())).expect("create colliding dir");
        assert!(!is_enabled_in(temp.path()));
    }
}
