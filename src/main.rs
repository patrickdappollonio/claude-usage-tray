//! Claude usage tray: reads the statusline usage cache on a timer and renders
//! it as a StatusNotifierItem tray icon. No network, no credentials, no writes
//! outside its own config, the Claude config directory, and autostart files.
//!
//! The same binary is also the statusline command itself (`statusline`
//! subcommand) and its own installer (`hook install|uninstall|status`), so
//! there is no shell snippet anywhere in the design.
//!
//! Threading (tray mode): `ksni::blocking::TrayMethods::spawn` runs the D-Bus
//! service on its own thread and hands back a `Handle`. The main thread then
//! *is* the poll loop, waiting on an mpsc channel with a timeout so that
//! "Check for new data", left-click (a worded usage summary), "Quit",
//! "Install hook", and interval changes take effect immediately instead of on
//! the next tick.

mod autostart;
mod config;
mod hook;
mod icon;
mod portal;
mod source;
#[cfg(test)]
mod testutil;
mod tray;

use jiff::{Timestamp, tz::TimeZone};
use ksni::blocking::TrayMethods;
use std::io::{Read, Write};
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

/// Emits the toast that follows a *user-initiated* action ("Check for new
/// data", the `Install hook` item, or a left-click status readout): low
/// urgency and transient, so it acknowledges the click without piling up in
/// notification history the way the threshold alerts (deliberately) do.
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

const USAGE: &str = "\
claude-usage-tray — Claude Code usage in the system tray

  claude-usage-tray                      run the tray
  claude-usage-tray statusline [--exec CMD]
                                         Claude Code statusline command: caches
                                         the stdin JSON, optionally running CMD
                                         and passing its output through
  claude-usage-tray hook install         point statusLine.command at this binary
  claude-usage-tray hook uninstall       undo that
  claude-usage-tray hook status          report what is currently wired up
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None => {
            run_tray();
            0
        }
        Some("statusline") => run_statusline(&args[1..]),
        Some("hook") => run_hook(&args[1..]),
        _ => {
            eprint!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

/// The `statusline` subcommand: pure transport. Reads Claude Code's statusline
/// JSON from stdin, writes it verbatim to the cache, and — with `--exec` —
/// hands the same bytes to the user's own statusline command and lets its
/// stdout through untouched.
///
/// It exits 0 whatever happens short of a usage error: a broken cache write or
/// a failing child must never make somebody's statusline worse, and this
/// command never prints anything of its own.
fn run_statusline(args: &[String]) -> i32 {
    let exec = match args {
        [] => None,
        [flag, command] if flag == "--exec" => Some(command.clone()),
        _ => {
            eprint!("{USAGE}");
            return 2;
        }
    };

    let mut input = Vec::new();
    // A read error leaves whatever arrived before it; there is nothing better
    // to do with it than carry on.
    let _ = std::io::stdin().read_to_end(&mut input);
    let cache_path = source::default_cache_path();
    let existing = std::fs::read(&cache_path).ok();
    if source::should_write_cache(&input, existing.as_deref()) {
        let _ = source::write_cache(&cache_path, &input);
    }

    if let Some(command) = exec {
        // stdout is inherited rather than piped and copied, so the child's
        // bytes reach Claude Code exactly as written — no added newline, no
        // buffering surprises.
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn();
        if let Ok(mut child) = child {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(&input);
                // Dropping closes the pipe, so a child reading to EOF finishes.
                drop(stdin);
            }
            let _ = child.wait();
        }
    }
    0
}

/// The `hook` subcommand family. Prints a human-readable report; a nonzero
/// exit means the settings file could not be read or written.
fn run_hook(args: &[String]) -> i32 {
    let config_dir = source::claude_config_dir();
    match args.first().map(String::as_str) {
        Some("install") if args.len() == 1 => {
            let exe = match std::env::current_exe() {
                Ok(exe) => exe,
                Err(err) => {
                    eprintln!("claude-usage-tray: cannot determine my own path: {err}");
                    return 1;
                }
            };
            match hook::install_in(&config_dir, &exe) {
                Ok(report) => {
                    println!("{}", report.render());
                    0
                }
                Err(err) => {
                    eprintln!("claude-usage-tray: hook install failed: {err}");
                    1
                }
            }
        }
        Some("uninstall") if args.len() == 1 => match hook::uninstall_in(&config_dir) {
            Ok(report) => {
                println!("{}", report.render());
                0
            }
            Err(err) => {
                eprintln!("claude-usage-tray: hook uninstall failed: {err}");
                1
            }
        },
        Some("status") if args.len() == 1 => {
            let report = hook::status_in(&config_dir, Timestamp::now());
            let exe = std::env::current_exe().ok();
            println!("{}", report.render(exe.as_deref(), Timestamp::now()));
            0
        }
        _ => {
            eprint!("{USAGE}");
            2
        }
    }
}

/// Runs the hook installer from the tray's menu item and returns the toast to
/// show. Deliberately called from the poll loop rather than from the D-Bus
/// callback, so the filesystem work never blocks the menu.
fn install_hook_now() -> String {
    let result = std::env::current_exe()
        .and_then(|exe| hook::install_in(&source::claude_config_dir(), &exe));
    hook::install_toast(&result)
}

/// What the poll loop should do with a fresh read once a wake reason has been
/// handled.
enum PostRead {
    /// No toast: a timer tick, or an action that already emitted its own.
    Silent,
    /// "Check for new data": report whether the cache moved forward.
    RefreshToast,
    /// A left-click: report a worded summary of current usage.
    StatusToast,
}

fn run_tray() {
    let cache_path = source::default_cache_path();
    let stored = config::load();
    let env_secs = config::env_override(std::env::var("CLAUDE_TRAY_POLL_SECS").ok().as_deref());
    let settings = tray::Settings::new(stored, env_secs);
    let interval = settings.interval_handle();
    let notify_prefs = settings.notify_handle();
    let appearance = settings.appearance_handle();
    let tz = TimeZone::system();

    let (wake_tx, wake_rx) = mpsc::channel::<Wake>();

    // Watch the desktop's light/dark preference regardless of the current
    // style: it costs one thread and one D-Bus connection, and it means
    // switching to "Monochrome (auto)" is already correct instead of waiting
    // for the next theme change. The handle ignores the value while a
    // non-auto style is selected, so no repaint is triggered for it.
    {
        let appearance = appearance.clone();
        let wake_tx = wake_tx.clone();
        portal::spawn_watcher(move |dark_ui| {
            if appearance.set_portal_dark(dark_ui) {
                let _ = wake_tx.send(Wake::AppearanceChanged);
            }
        });
    }

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
        let post_read = match wake_rx.recv_timeout(wait) {
            // "Check for new data": re-read *and* tell the user whether the
            // cache moved forward.
            Ok(Wake::Refresh) => PostRead::RefreshToast,
            // Left-click: re-read (cheap) and show a worded summary of
            // current usage — never a "did it change" report.
            Ok(Wake::ShowStatus) => PostRead::StatusToast,
            // The timer expired: re-read silently.
            Err(RecvTimeoutError::Timeout) => PostRead::Silent,
            // The interval changed mid-wait: start a fresh wait with it.
            Ok(Wake::IntervalChanged) => continue,
            // A notification toggle changed: pick it up at the top of the loop.
            Ok(Wake::NotifyChanged) => continue,
            // The icon style changed, or the desktop switched theme under
            // `mono-auto`. The appearance is shared state that `icon_pixmap`
            // reads, so an empty update is enough to make ksni re-render and
            // push the new pixmaps.
            Ok(Wake::AppearanceChanged) => {
                handle.update(|_tray| {});
                continue;
            }
            // The first-run menu item. The install itself runs here, off the
            // D-Bus callback, then falls through to a re-read: the cache will
            // still be missing until Claude Code next refreshes, which is
            // exactly what the toast says.
            Ok(Wake::InstallHook) => {
                notify_refresh(&install_hook_now());
                PostRead::Silent
            }
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
        match post_read {
            PostRead::RefreshToast => {
                notify_refresh(&tray::refresh_message(
                    &snapshot,
                    &next,
                    Timestamp::now(),
                    &tz,
                ));
            }
            PostRead::StatusToast => {
                notify_refresh(&tray::status_message(&next, Timestamp::now(), &tz));
            }
            PostRead::Silent => {}
        }
        snapshot = next;
    }

    handle.shutdown().wait();
}
