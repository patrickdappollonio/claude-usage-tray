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

use crate::platform::{BackendError, Channel, Toast, Urgency};
use crate::source::UsageSnapshot;
use crate::ui::TrayCore;
use ksni::blocking::TrayMethods;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Well-known D-Bus name of the StatusNotifierItem host the tray registers
/// with.
const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";

/// How long startup waits for [`WATCHER_NAME`] to appear before giving up.
/// Plasma owns the name before it runs autostart entries, but on GNOME it
/// comes from the AppIndicator shell extension, which loads alongside the
/// autostart apps: a tray launched at login can get there first.
const WATCHER_PATIENCE: Duration = Duration::from_secs(30);

/// How often the wait re-checks for [`WATCHER_NAME`].
const WATCHER_POLL: Duration = Duration::from_millis(250);

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
    wait_for_watcher(WATCHER_PATIENCE);
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

/// Blocks until [`WATCHER_NAME`] has an owner on the session bus, or until
/// `patience` runs out. Returns without complaint either way: the spawn that
/// follows is what reports a missing host, with the same message whether or
/// not this wait could reach the bus at all.
fn wait_for_watcher(patience: Duration) {
    let Ok(connection) = zbus::blocking::Connection::session() else {
        return;
    };
    let Ok(dbus) = zbus::blocking::fdo::DBusProxy::new(&connection) else {
        return;
    };
    let Ok(name) = zbus::names::BusName::try_from(WATCHER_NAME) else {
        return;
    };
    let deadline = Instant::now() + patience;
    loop {
        match dbus.name_has_owner(name.clone()) {
            Ok(true) | Err(_) => return,
            Ok(false) => {}
        }
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(WATCHER_POLL);
    }
}

/// The threshold-alert notification currently on screen, if any.
///
/// Kept so a later threshold alert can replace it in place instead of
/// stacking underneath it. `notify_rust::NotificationHandle` is `Send` here —
/// verified with a compile-time probe (`assert_send::<NotificationHandle>()`)
/// against this crate's actual dependency graph, where `notify-rust`'s
/// default `zbus` feature is the only backend enabled (the `dbus` feature is
/// off; see `Cargo.lock`), so the handle's only non-trivial field is a
/// `zbus::Connection`, itself `Arc`-backed and `Send + Sync` — which is what
/// makes storing it in a `Mutex` behind a `static` sound. Every other toast
/// (reset, refresh, status, install) stays fire-and-forget, matching
/// `Channel::Ephemeral`.
static THRESHOLD_HANDLE: Mutex<Option<notify_rust::NotificationHandle>> = Mutex::new(None);

/// Emits a freedesktop notification. Failures (no notification daemon, D-Bus
/// down) are ignored on purpose.
///
/// `Channel::ThresholdAlert` reuses the previous threshold notification's
/// D-Bus id by calling [`notify_rust::NotificationHandle::update`] on the
/// retained handle. Per that method's own documentation (`notify-rust`
/// 4.18.0, `src/xdg/mod.rs`): "Replace the original notification with an
/// updated version" — under the hood this re-sends the freedesktop `Notify`
/// call with the same id, which the spec defines as a request to replace the
/// existing notification rather than create a new one, and KDE Plasma (the
/// target desktop for this feature) honors that. If the update fails — the
/// notification was already dismissed, or the server dropped the id — a
/// fresh notification is shown and its handle replaces the stored one, same
/// as if none had been showing.
pub fn notify(toast: &Toast, channel: Channel) {
    let urgency = match toast.urgency {
        Urgency::Low => notify_rust::Urgency::Low,
        Urgency::Normal => notify_rust::Urgency::Normal,
        Urgency::Critical => notify_rust::Urgency::Critical,
    };

    if channel == Channel::ThresholdAlert {
        let mut slot = THRESHOLD_HANDLE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(handle) = slot.as_mut() {
            handle
                .appname("Claude usage tray")
                .summary(&toast.summary)
                .body(&toast.body)
                .urgency(urgency);
            if toast.transient {
                handle.hint(notify_rust::Hint::Transient(true));
            }
            if handle.update().is_ok() {
                return;
            }
            // Fall through: the old id is no longer replaceable, so show a
            // fresh notification and adopt its handle below.
        }

        let mut notification = notify_rust::Notification::new();
        notification
            .appname("Claude usage tray")
            .summary(&toast.summary)
            .body(&toast.body)
            .urgency(urgency);
        if toast.transient {
            notification.hint(notify_rust::Hint::Transient(true));
        }
        *slot = notification.show().ok();
        return;
    }

    let mut notification = notify_rust::Notification::new();
    notification
        .appname("Claude usage tray")
        .summary(&toast.summary)
        .body(&toast.body)
        .urgency(urgency);
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
