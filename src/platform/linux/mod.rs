//! The Linux backend: a StatusNotifierItem tray over D-Bus (`ksni`), XDG
//! autostart, XDG portal appearance, and freedesktop notifications.
//!
//! The interesting part is [`run`]: `ksni`'s blocking `spawn` puts the D-Bus
//! service on a thread of its own and hands back a handle, so the poll loop
//! keeps the thread it was called on — the main thread — exactly as it did
//! before the platform split. See [`crate::platform`] for why the contract is
//! shaped to let the other backend do the opposite.

pub mod autostart;
mod portal;
mod tray;

use crate::platform::{BackendError, Toast, Urgency};
use crate::source::UsageSnapshot;
use crate::ui::TrayCore;
use ksni::blocking::TrayMethods;

/// The poll loop's remote control over the running tray, wrapping the `ksni`
/// handle. Every method is a property push: `ksni` re-reads the tray's
/// `icon_pixmap`/`tool_tip`/`menu` after the closure runs and sends the tray
/// host whatever changed.
pub struct TrayHandle(ksni::blocking::Handle<tray::LinuxTray>);

impl TrayHandle {
    /// Publishes a new snapshot: new icon, new tooltip, new menu labels.
    pub fn set_snapshot(&self, snapshot: UsageSnapshot) {
        self.0.update(move |tray| tray.core.snapshot = snapshot);
    }

    /// Re-publishes without changing the snapshot, for the things the tray
    /// reads out of shared state on every render: the resolved icon appearance
    /// and the update-available row.
    pub fn refresh(&self) {
        self.0.update(|_tray| {});
    }

    /// Whether the tray service is gone (host disappeared, D-Bus died).
    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
}

/// Starts the tray service and then runs `poll` on this thread.
pub fn run<F>(core: TrayCore, poll: F) -> Result<(), BackendError>
where
    F: FnOnce(TrayHandle) + Send + 'static,
{
    let handle = tray::LinuxTray::new(core).spawn().map_err(|err| {
        BackendError::new(format!(
            "could not start the tray service: {err}\n\
             Is a StatusNotifierItem host (KDE Plasma, or GNOME with the \
             AppIndicator extension) running?"
        ))
    })?;
    poll(TrayHandle(handle.clone()));
    handle.shutdown().wait();
    Ok(())
}

/// Emits a freedesktop notification. Failures (no notification daemon, D-Bus
/// down) are ignored on purpose.
pub fn notify(toast: &Toast) {
    let mut notification = notify_rust::Notification::new();
    notification
        .appname("Claude usage tray")
        .summary(&toast.summary)
        .body(&toast.body)
        .urgency(match toast.urgency {
            Urgency::Low => notify_rust::Urgency::Low,
            Urgency::Normal => notify_rust::Urgency::Normal,
            Urgency::Critical => notify_rust::Urgency::Critical,
        });
    if toast.transient {
        notification.hint(notify_rust::Hint::Transient(true));
    }
    let _ = notification.show();
}

/// Watches `org.freedesktop.appearance color-scheme` through the XDG portal.
pub fn watch_appearance<F>(on_change: F)
where
    F: Fn(bool) + Send + 'static,
{
    portal::spawn_watcher(on_change);
}
