//! One-time greeting toasts: a welcome on the very first launch, and an
//! "updated" notice on the first launch after an upgrade.
//!
//! The welcome toast is not just a pleasantry. On macOS the system permission
//! prompt for notifications only appears the first time the app actually tries
//! to post one — without this, a user could run the tray for weeks and never
//! be asked, and then the first threshold alert would be silently dropped.
//!
//! What ran last is recorded in a plain one-line `last-version` file next to
//! the config, not inside `config.toml`: that file is a user-facing settings
//! contract, and machine state would force a rewrite of it on every upgrade.
//! The state is written *before* the toast, and a failed write suppresses the
//! toast entirely — on an unwritable config directory, greeting the user on
//! every single launch forever would be worse than never greeting them.

use std::io;
use std::path::{Path, PathBuf};

/// What, if anything, this launch is the first of.
#[derive(Debug, PartialEq)]
pub enum Greeting {
    /// No version on record: the app has never run (or never could write).
    FirstLaunch,
    /// A different version ran before; the payload is the current version.
    Updated(String),
    /// The recorded version is current: an ordinary launch.
    None,
}

/// Where the last-run version is recorded.
pub fn state_path() -> PathBuf {
    crate::config::config_dir().join("last-version")
}

/// Decides which greeting (if any) a launch deserves, given the recorded
/// version. Pure so the three-way split is testable without a filesystem.
pub fn classify(recorded: Option<&str>, current: &str) -> Greeting {
    match recorded.map(str::trim) {
        None | Some("") => Greeting::FirstLaunch,
        Some(v) if v == current => Greeting::None,
        Some(_) => Greeting::Updated(current.to_string()),
    }
}

/// Reads the recorded version. Absent, unreadable, and empty files all mean
/// the same thing here: nothing on record.
pub fn read_recorded(path: &Path) -> Option<String> {
    let body = std::fs::read_to_string(path).ok()?;
    let trimmed = body.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Records `version` atomically: a temp file in the same directory followed
/// by a rename, so a crash mid-write never leaves a truncated file behind.
pub fn record(path: &Path, version: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, version)?;
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            // Leaving the temp file behind would make the next record fail in
            // the same way for no extra information.
            let _ = std::fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// Classifies this launch and, if it deserves a greeting, records the current
/// version first. A failed record suppresses the greeting: the toast may only
/// fire when it is guaranteed not to fire again next launch.
pub fn check_and_record(path: &Path, current: &str) -> Greeting {
    let greeting = classify(read_recorded(path).as_deref(), current);
    if greeting == Greeting::None {
        return Greeting::None;
    }
    match record(path, current) {
        Ok(()) => greeting,
        Err(_) => Greeting::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn no_recorded_version_is_first_launch() {
        assert_eq!(classify(None, "0.1.0"), Greeting::FirstLaunch);
    }

    #[test]
    fn matching_version_is_silent() {
        assert_eq!(classify(Some("0.1.0"), "0.1.0"), Greeting::None);
    }

    #[test]
    fn different_version_is_updated_with_current_version() {
        assert_eq!(
            classify(Some("0.0.9"), "0.1.0"),
            Greeting::Updated("0.1.0".to_string())
        );
    }

    #[test]
    fn whitespace_only_recording_counts_as_first_launch() {
        assert_eq!(classify(Some("  \n"), "0.1.0"), Greeting::FirstLaunch);
    }

    #[test]
    fn record_round_trips_through_read_recorded() {
        let dir = TempDir::new("greeting-round-trip");
        let path = dir.path().join("last-version");
        record(&path, "0.1.0").expect("record");
        assert_eq!(read_recorded(&path), Some("0.1.0".to_string()));
    }

    #[test]
    fn read_recorded_treats_missing_and_empty_files_as_nothing() {
        let dir = TempDir::new("greeting-empty");
        let missing = dir.path().join("last-version");
        assert_eq!(read_recorded(&missing), None);
        std::fs::write(&missing, "\n").expect("write");
        assert_eq!(read_recorded(&missing), None);
    }

    #[test]
    fn check_and_record_is_silent_on_second_call() {
        let dir = TempDir::new("greeting-once");
        let path = dir.path().join("last-version");
        assert_eq!(check_and_record(&path, "0.1.0"), Greeting::FirstLaunch);
        assert_eq!(check_and_record(&path, "0.1.0"), Greeting::None);
        assert_eq!(
            check_and_record(&path, "0.2.0"),
            Greeting::Updated("0.2.0".to_string())
        );
        assert_eq!(check_and_record(&path, "0.2.0"), Greeting::None);
    }

    #[test]
    fn unwritable_destination_yields_no_greeting() {
        // The state path's parent is a regular file, so create_dir_all fails.
        // Deliberately not a permission-bit setup, which breaks as root.
        let dir = TempDir::new("greeting-unwritable");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"").expect("write blocker");
        let path = blocker.join("last-version");
        assert_eq!(check_and_record(&path, "0.1.0"), Greeting::None);
    }
}
