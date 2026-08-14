//! "Launch at login" on macOS — **a stub**, and the documented home of the
//! LaunchAgent implementation.
//!
//! The shape to fill in, mirroring `platform/linux/autostart.rs`:
//!
//! * The entry is a property list at
//!   `~/Library/LaunchAgents/io.github.patrickdappollonio.claude-usage-tray.plist`
//!   with `Label`, `ProgramArguments` (the absolute path of the running
//!   executable, from `std::env::current_exe`), and `RunAtLoad` = true.
//! * `is_enabled` is "does that file exist", exactly as on Linux.
//! * `enable` writes it atomically (temp file + rename) and, so the change
//!   takes effect without a logout, `launchctl bootstrap gui/$UID <path>`;
//!   `disable` runs `launchctl bootout` and removes the file, treating an
//!   already-absent entry as success.
//! * `is_available` is the same writability probe
//!   ([`crate::config::dir_is_writable`]) against the `LaunchAgents`
//!   directory.
//!
//! Everything here is deliberately pure-ish and directory-parameterised on
//! Linux so the plist body and the enable/disable state machine can be unit
//! tested against a temp directory; do the same here.
//!
//! Until then all three answers are "no", which the menu renders as a grayed,
//! unchecked `Launch at login` checkbox — the same thing it shows on a Linux
//! box with an unwritable autostart directory.

/// Whether the LaunchAgent could be written right now.
pub fn is_available() -> bool {
    false
}

/// Whether the LaunchAgent exists.
pub fn is_enabled() -> bool {
    false
}

/// Applies `enabled`, reporting whether the end state was reached. Always
/// `false` here, so the caller leaves the checkbox where it was rather than
/// lying about the state.
pub fn set_enabled(_enabled: bool) -> bool {
    false
}
