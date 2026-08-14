//! Claude usage tray: reads the statusline usage cache on a timer and renders
//! it as a StatusNotifierItem tray icon. No network, no credentials, no writes
//! outside its own config and autostart files.
//!
//! Threading: `ksni::blocking::TrayMethods::spawn` runs the D-Bus service on
//! its own thread and hands back a `Handle`. The main thread then *is* the poll
//! loop, waiting on an mpsc channel with a timeout so that "Check for new
//! data", left-click, "Quit", and interval changes take effect immediately
//! instead of on the next tick.

mod autostart;
mod config;
mod icon;
mod source;
#[cfg(test)]
mod testutil;
mod tray;

use jiff::{Timestamp, tz::TimeZone};
use ksni::blocking::TrayMethods;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use tray::{UsageAlert, Wake};

/// Emits a threshold notification. Failures (no notification daemon, D-Bus
/// down) are ignored: a missing notification must never take the tray with it.
fn notify(alert: &UsageAlert) {
    let mut notification = notify_rust::Notification::new();
    notification
        .appname("Claude usage")
        .summary(&alert.summary())
        .body(&alert.body())
        .urgency(if alert.critical {
            notify_rust::Urgency::Critical
        } else {
            notify_rust::Urgency::Normal
        });
    let _ = notification.show();
}

/// Emits the toast that follows a *user-initiated* refresh: low urgency and
/// transient, so it acknowledges the click without piling up in notification
/// history the way the threshold alerts (deliberately) do.
fn notify_refresh(body: &str) {
    let mut notification = notify_rust::Notification::new();
    notification
        .appname("Claude usage")
        .summary("Claude usage")
        .body(body)
        .urgency(notify_rust::Urgency::Low)
        .hint(notify_rust::Hint::Transient(true));
    let _ = notification.show();
}

fn main() {
    let cache_path = source::default_cache_path();
    let stored = config::load();
    let env_secs = config::env_override(std::env::var("CLAUDE_TRAY_POLL_SECS").ok().as_deref());
    let settings = tray::Settings::new(stored, env_secs);
    let interval = settings.interval_handle();
    let tz = TimeZone::system();

    let (wake_tx, wake_rx) = mpsc::channel::<Wake>();
    let mut snapshot = source::read_snapshot(&cache_path, Timestamp::now());

    let handle = match tray::UsageTray::new(snapshot.clone(), settings, wake_tx).spawn() {
        Ok(handle) => handle,
        Err(err) => {
            eprintln!(
                "claude-usage-tray: could not start the tray service: {err}\n\
                 Is a StatusNotifierItem host (KDE Plasma, or GNOME with the \
                 AppIndicator extension) running?"
            );
            std::process::exit(1);
        }
    };

    let mut notifier = tray::Notifier::new();

    loop {
        if let Some(alert) = notifier.evaluate(snapshot.session.as_ref()) {
            notify(&alert);
        }

        // Re-read the interval every cycle so a settings change applies live.
        let wait = Duration::from_secs(interval.load(Ordering::Relaxed));
        let user_initiated = match wake_rx.recv_timeout(wait) {
            // "Check for new data" or a left-click: re-read *and* tell the
            // user what came of it.
            Ok(Wake::Refresh) => true,
            // The timer expired: re-read silently.
            Err(RecvTimeoutError::Timeout) => false,
            // The interval changed mid-wait: start a fresh wait with it.
            Ok(Wake::IntervalChanged) => continue,
            Ok(Wake::Quit) => break,
            // Every sender is gone, which can only mean the tray service died.
            Err(RecvTimeoutError::Disconnected) => break,
        };

        if handle.is_closed() {
            break;
        }

        let next = source::read_snapshot(&cache_path, Timestamp::now());
        if tray::snapshot_changed(&snapshot, &next) {
            let pushed = next.clone();
            handle.update(move |tray| tray.snapshot = pushed);
        }
        if user_initiated {
            notify_refresh(&tray::refresh_message(
                &snapshot,
                &next,
                Timestamp::now(),
                &tz,
            ));
        }
        snapshot = next;
    }

    handle.shutdown().wait();
}
