//! The macOS backend — **a stub**. It exists so the crate type-checks for
//! Darwin (`cargo check --target aarch64-apple-darwin`) while the real
//! implementation is written; running the tray on macOS today fails cleanly
//! with "macOS backend not implemented yet" rather than doing something worse.
//!
//! # What the real implementation has to do
//!
//! * **[`run`]** — the inversion the contract in [`crate::platform`] exists
//!   for. `NSApplication` owns the main thread and does not return until the
//!   app quits, so this backend must build the status item, spawn `poll` on a
//!   worker thread with a [`TrayHandle`] that can reach the status item, and
//!   *then* run the event loop on the thread `run` was called on. The Linux
//!   backend does the opposite (the poll loop keeps the calling thread) and
//!   nothing above [`crate::platform`] can tell the difference.
//! * **[`TrayHandle`]** — every method is called from the poll-loop thread, so
//!   each one has to hop to the main thread before touching AppKit
//!   (`dispatch_async` onto the main queue, or the event loop's own proxy).
//!   `set_snapshot` re-renders the icon from
//!   [`TrayCore::icons`](crate::ui::TrayCore::icons) — the pixmaps are ARGB32
//!   big-endian, so they need swizzling into an `NSImage` — plus a menu rebuild
//!   from [`TrayCore::menu`](crate::ui::TrayCore::menu); `refresh` is the same
//!   without the snapshot; `is_closed` reports whether the app is terminating.
//! * **The menu** — map [`crate::menu::MenuRow`] onto `NSMenu`/`muda` exactly
//!   as `platform/linux/tray.rs` maps it onto `ksni`, and route clicks back
//!   through [`TrayCore::activate`](crate::ui::TrayCore::activate) and
//!   [`TrayCore::select`](crate::ui::TrayCore::select). No wording, ordering or
//!   enabled-state logic belongs here: it is all in [`crate::ui`] already.
//! * **[`notify`]** — `notify-rust` supports macOS, but its ObjC dependency
//!   cannot be built from Linux, so the dependency is currently gated to the
//!   Linux target in `Cargo.toml`. Re-add it under
//!   `[target.'cfg(target_os = "macos")'.dependencies]` (or use
//!   `UNUserNotificationCenter` directly) when building on a Mac. Note that
//!   `notify-rust`'s `urgency`/`hint` builders are Linux-only, which is exactly
//!   why [`Toast`] is a portable struct rather than a notify-rust type.
//! * **[`watch_appearance`]** — observe `NSApp.effectiveAppearance` (KVO on
//!   `AppleInterfaceStyle`) and report `true` for the dark aqua appearance. Not
//!   calling `on_change` at all is a valid implementation: the dark-assuming
//!   default then stands.
//! * **[`autostart`]** — a `~/Library/LaunchAgents` LaunchAgent; see that
//!   module.

pub mod autostart;

use crate::platform::{BackendError, Toast};
use crate::source::UsageSnapshot;
use crate::ui::TrayCore;

/// Placeholder for the handle the real backend will hand the poll loop. Never
/// constructed today: [`run`] fails before a tray exists.
pub struct TrayHandle;

impl TrayHandle {
    /// Publishes a new snapshot (icon, tooltip and menu).
    pub fn set_snapshot(&self, _snapshot: UsageSnapshot) {}

    /// Re-publishes from shared state without a new snapshot.
    pub fn refresh(&self) {}

    /// Whether the tray is gone and the poll loop should stop.
    pub fn is_closed(&self) -> bool {
        true
    }
}

/// Fails: there is no macOS tray yet.
pub fn run<F>(_core: TrayCore, _poll: F) -> Result<(), BackendError>
where
    F: FnOnce(TrayHandle) + Send + 'static,
{
    Err(BackendError::new("macOS backend not implemented yet"))
}

/// No-op until a notification backend is wired up; see the module docs.
pub fn notify(_toast: &Toast) {}

/// No-op: the dark-assuming default in [`crate::ui::AppearanceHandle`] stands
/// until `NSApp.effectiveAppearance` is observed here.
pub fn watch_appearance<F>(_on_change: F)
where
    F: Fn(bool) + Send + 'static,
{
}
