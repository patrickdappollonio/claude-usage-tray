//! The platform boundary: everything that talks to a desktop lives behind this
//! module, and everything above it ([`crate::ui`], [`crate::menu`],
//! [`crate::icon`], and the poll loop in `main.rs`) is portable.
//!
//! # What a backend owes the core
//!
//! A backend implements four things:
//!
//! * [`run`] — start the tray and drive it until the poll loop returns.
//! * [`TrayHandle`] — the poll loop's remote control over the running tray:
//!   push a new snapshot, force a repaint/menu rebuild, ask whether the tray
//!   died under it.
//! * [`autostart`] — "launch at login" for this desktop.
//! * [`notify`] and [`watch_appearance`] — desktop toasts and the light/dark
//!   preference.
//!
//! Everything else — which rows the menu has, what they say, when a
//! notification fires, what the icon looks like — is decided portably and
//! handed to the backend as data ([`crate::menu::MenuRow`],
//! [`crate::icon::IconImage`], [`Toast`]).
//!
//! # Why [`run`] takes the poll loop instead of returning
//!
//! The two platforms disagree about who owns the main thread, so the contract
//! is written so that neither side assumes it blocks:
//!
//! * **Linux (today).** `ksni` runs the StatusNotifierItem service on its own
//!   thread, so the backend calls `poll` on the thread it was given — the main
//!   thread *is* the poll loop, exactly as before this split — and shuts the
//!   service down when `poll` returns.
//! * **macOS (later).** `NSApplication` insists on the main thread and never
//!   returns until the app quits, so that backend will spawn `poll` on a worker
//!   thread and then run the event loop on the caller's thread, returning when
//!   the event loop stops.
//!
//! Hence the signature: `run` is given the loop as an `FnOnce(TrayHandle)` that
//! is `Send + 'static`, so a backend may run it here or over there, and `run`
//! itself is the thing that blocks in both cases. `main.rs` only knows that it
//! must not do anything after calling it.

use crate::ui::TrayCore;
use std::fmt;

#[cfg(target_os = "linux")]
#[path = "linux/mod.rs"]
mod imp;

#[cfg(target_os = "macos")]
#[path = "macos/mod.rs"]
mod imp;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!(
    "no tray backend for this platform: add one under src/platform/ and wire it up in \
     src/platform/mod.rs"
);

/// "Launch at login" for the current desktop.
///
/// Every implementation exposes the same three functions:
/// `is_available() -> bool` (can the entry be written at all?),
/// `is_enabled() -> bool` (is it there now?) and
/// `set_enabled(bool) -> bool` (apply it; `false` means the end state was *not*
/// reached, so the caller leaves the checkbox where it was).
pub use imp::autostart;

/// The poll loop's remote control over the running tray. Platform-specific
/// type, uniform API — see the module docs.
pub use imp::TrayHandle;

/// Why the tray could not be started.
///
/// The message is the whole human-readable explanation *including* any
/// platform-specific hint, because only the backend knows what to suggest
/// ("is a StatusNotifierItem host running?" means nothing on macOS). `main.rs`
/// prints it verbatim behind the program name.
#[derive(Debug)]
pub struct BackendError(String);

impl BackendError {
    pub fn new(message: impl Into<String>) -> Self {
        BackendError(message.into())
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How loudly a toast should be delivered. Portable spelling of the three
/// urgencies the tray actually uses; a backend maps them onto whatever its
/// notification system has (or ignores them, on a platform that has no such
/// concept).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Urgency {
    /// An acknowledgement of something the user just clicked.
    Low,
    /// A threshold crossing, or the quota-reset notice.
    Normal,
    /// A threshold crossing close to the limit.
    Critical,
}

/// A desktop notification to show. Built portably (see `main.rs`), emitted by
/// the backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    pub summary: String,
    pub body: String,
    pub urgency: Urgency,
    /// Whether the toast should be dropped from notification history once it
    /// disappears. True for the click acknowledgements, false for the alerts
    /// (which are deliberately worth scrolling back to).
    pub transient: bool,
}

/// Starts the tray and drives it until the poll loop finishes.
///
/// Blocks for the whole life of the program on every platform; see the module
/// docs for which thread the poll loop actually ends up on.
pub fn run<F>(core: TrayCore, poll: F) -> Result<(), BackendError>
where
    F: FnOnce(TrayHandle) + Send + 'static,
{
    imp::run(core, poll)
}

/// Shows a desktop notification. Failures are swallowed: a missing notification
/// must never take the tray with it.
pub fn notify(toast: &Toast) {
    imp::notify(toast);
}

/// Starts watching the desktop's light/dark preference, calling `on_change`
/// with "is the user's UI dark?" once at startup and again on every change.
///
/// `on_change` runs on the watcher's own thread, so it must be cheap and must
/// not panic. A platform with no way to answer simply never calls it, and the
/// dark-assuming default stands.
pub fn watch_appearance<F>(on_change: F)
where
    F: Fn(bool) + Send + 'static,
{
    imp::watch_appearance(on_change);
}
