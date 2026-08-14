//! Claude usage tray: reads the statusline usage cache on a timer and renders
//! it as a StatusNotifierItem tray icon. No network, no credentials, no writes.
//!
//! Threading: `ksni::blocking::TrayMethods::spawn` runs the D-Bus service on
//! its own thread and hands back a `Handle`. The main thread then *is* the poll
//! loop, waiting on an mpsc channel with a timeout so that "Refresh now",
//! left-click, and "Quit" take effect immediately instead of on the next tick.

mod icon;
mod source;
mod tray;

use jiff::Timestamp;
use ksni::blocking::TrayMethods;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use tray::{UsageAlert, Wake};

/// Default seconds between cache re-reads; `CLAUDE_TRAY_POLL_SECS` overrides.
const DEFAULT_POLL_SECS: u64 = 5;

/// Reads the poll interval from the environment, falling back to the default
/// for anything unset, unparseable, or zero.
fn poll_interval() -> Duration {
    parse_poll_secs(std::env::var("CLAUDE_TRAY_POLL_SECS").ok().as_deref())
}

/// Pure half of [`poll_interval`], so the parse-or-default rule is testable
/// without mutating the process environment.
fn parse_poll_secs(raw: Option<&str>) -> Duration {
    let secs = raw
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|&secs| secs > 0)
        .unwrap_or(DEFAULT_POLL_SECS);
    Duration::from_secs(secs)
}

/// Emits a desktop notification. Failures (no notification daemon, D-Bus down)
/// are ignored: a missing notification must never take the tray with it.
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

fn main() {
    let cache_path = source::default_cache_path();
    let interval = poll_interval();

    let (wake_tx, wake_rx) = mpsc::channel::<Wake>();
    let mut snapshot = source::read_snapshot(&cache_path, Timestamp::now());

    let handle = match tray::UsageTray::new(snapshot.clone(), wake_tx).spawn() {
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

        match wake_rx.recv_timeout(interval) {
            // Woken by "Refresh now" or a left-click, or the timer expired:
            // either way, re-read below.
            Ok(Wake::Refresh) | Err(RecvTimeoutError::Timeout) => {}
            Ok(Wake::Quit) => break,
            // Every sender is gone, which can only mean the tray service died.
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if handle.is_closed() {
            break;
        }

        let next = source::read_snapshot(&cache_path, Timestamp::now());
        if tray::snapshot_changed(&snapshot, &next) {
            let pushed = next.clone();
            handle.update(move |tray| tray.snapshot = pushed);
        }
        snapshot = next;
    }

    handle.shutdown().wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_secs_parses_a_valid_override() {
        assert_eq!(parse_poll_secs(Some(" 30 ")), Duration::from_secs(30));
    }

    #[test]
    fn poll_secs_falls_back_on_unset_garbage_or_zero() {
        let default = Duration::from_secs(DEFAULT_POLL_SECS);
        assert_eq!(parse_poll_secs(None), default);
        assert_eq!(parse_poll_secs(Some("")), default);
        assert_eq!(parse_poll_secs(Some("soon")), default);
        assert_eq!(parse_poll_secs(Some("-1")), default);
        assert_eq!(parse_poll_secs(Some("0")), default);
    }
}
