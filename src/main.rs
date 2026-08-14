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
use tray::{ResetAlert, UsageAlert, Wake};

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

/// Emits the "your 5-hour window rolled over" notification. Normal urgency:
/// it is good news, not a warning.
fn notify_reset(alert: &ResetAlert) {
    let mut notification = notify_rust::Notification::new();
    notification
        .appname("Claude usage")
        .summary(&alert.summary())
        .body(&alert.body())
        .urgency(notify_rust::Urgency::Normal);
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
    let notify_prefs = settings.notify_handle();
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

    let mut notifier = tray::Notifier::new(&notify_prefs.get().thresholds);
    let mut reset_notifier = tray::ResetNotifier::new();

    loop {
        // Re-read the preferences every cycle so a menu toggle applies live.
        let prefs = notify_prefs.get();
        notifier.set_enabled(&prefs.thresholds);
        if let Some(alert) = notifier.evaluate(snapshot.session.as_ref()) {
            notify(&alert);
        }

        let now = Timestamp::now();
        // Whether the window we were waiting on has now come due — recorded
        // before `evaluate` consumes it.
        let window_rolled_over = reset_notifier.deadline().is_some_and(|at| at <= now);
        let session_reset = snapshot
            .session
            .as_ref()
            .and_then(|session| session.resets_at);
        if let Some(alert) = reset_notifier.evaluate(session_reset, now, prefs.on_reset) {
            notify_reset(&alert);
        }
        if window_rolled_over {
            // The cached percentages describe the window that just ended; a
            // re-read reports the fresh one (the reader zeroes a percentage
            // whose `resets_at` has passed), so the icon follows the
            // notification instead of lagging a whole interval behind it.
            let next = source::read_snapshot(&cache_path, Timestamp::now());
            if tray::snapshot_changed(&snapshot, &next) {
                let pushed = next.clone();
                handle.update(move |tray| tray.snapshot = pushed);
            }
            snapshot = next;
            continue;
        }

        // Re-read the interval every cycle so a settings change applies live,
        // and never sleep past a pending quota reset.
        let wait = tray::poll_wait(
            interval.load(Ordering::Relaxed),
            reset_notifier.deadline(),
            Timestamp::now(),
        );
        let user_initiated = match wake_rx.recv_timeout(wait) {
            // "Check for new data" or a left-click: re-read *and* tell the
            // user what came of it.
            Ok(Wake::Refresh) => true,
            // The timer expired: re-read silently.
            Err(RecvTimeoutError::Timeout) => false,
            // The interval changed mid-wait: start a fresh wait with it.
            Ok(Wake::IntervalChanged) => continue,
            // A notification toggle changed: pick it up at the top of the loop.
            Ok(Wake::NotifyChanged) => continue,
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
