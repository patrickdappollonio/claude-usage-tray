//! Claude usage tray: reads the statusline usage cache on a timer and renders
//! it as a StatusNotifierItem tray icon. No credentials, no writes outside its
//! own config, the Claude config directory, and autostart files, and no
//! network beyond the optional once-daily GitHub release check in
//! [`update`] — nothing about the user's usage ever leaves the machine.
//!
//! The same binary is also the statusline command itself (`statusline`
//! subcommand) and its own installer (`hook install|uninstall|status`), so
//! there is no shell snippet anywhere in the design.
//!
//! Threading (tray mode): the platform backend decides which thread the poll
//! loop gets ([`platform::run`] takes it as a closure precisely so that
//! neither side has to assume). On Linux the tray service runs on its own
//! thread and the main thread *is* the poll loop, waiting on an mpsc channel
//! with a timeout so that "Check for new data", left-click (a worded usage
//! summary), "Quit", "Install hook", and interval changes take effect
//! immediately instead of on the next tick.

// The macOS backend is still a stub (`src/platform/macos/`), so it never calls
// into the portable core and almost everything below `platform` looks unused
// when type-checking for Darwin. Expected until the real backend lands; the
// Linux build is unaffected.
#![cfg_attr(target_os = "macos", allow(dead_code))]

mod config;
mod hook;
mod icon;
mod menu;
mod platform;
mod source;
#[cfg(test)]
mod testutil;
mod ui;
mod update;

use jiff::{Timestamp, tz::TimeZone};
use platform::{Toast, Urgency};
use std::io::{Read, Write};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use ui::{ResetAlert, UsageAlert, Wake};

/// Emits a threshold notification. Failures (no notification daemon, D-Bus
/// down) are ignored by the backend: a missing notification must never take
/// the tray with it.
fn notify(alert: &UsageAlert) {
    platform::notify(&Toast {
        summary: alert.summary(),
        body: alert.body(),
        urgency: if alert.critical {
            Urgency::Critical
        } else {
            Urgency::Normal
        },
        transient: false,
    });
}

/// Emits the "your 5-hour window rolled over" notification. Normal urgency:
/// it is good news, not a warning.
fn notify_reset(alert: &ResetAlert) {
    platform::notify(&Toast {
        summary: alert.summary(),
        body: alert.body(),
        urgency: Urgency::Normal,
        transient: false,
    });
}

/// Emits the toast that follows a *user-initiated* action ("Check for new
/// data", the `Install hook` item, or a left-click status readout): low
/// urgency and transient, so it acknowledges the click without piling up in
/// notification history the way the threshold alerts (deliberately) do.
fn notify_refresh(body: &str) {
    platform::notify(&Toast {
        summary: "Claude usage tray".to_string(),
        body: body.to_string(),
        urgency: Urgency::Low,
        transient: true,
    });
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

/// How long after startup the first update check runs. Late enough that a
/// cold start never waits on DNS or TLS for anything the user can see; the
/// check happens on its own thread anyway, so this is only about not competing
/// with the tray's first paint.
const FIRST_UPDATE_CHECK_DELAY: Duration = Duration::from_secs(5);

/// Interval between update checks thereafter.
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Runs the update check on its own thread, forever: once shortly after
/// startup, then daily.
///
/// The `enabled` flag is re-read before every check rather than captured, so
/// unticking `Settings ▸ Check for updates` stops the next one without a
/// restart (and re-ticking it resumes on the following cycle). A check that
/// finds nothing — including one that failed outright, which is
/// indistinguishable here on purpose — leaves the shared slot alone and says
/// nothing.
///
/// The thread is deliberately never joined: it spends its whole life asleep,
/// and the process exits out from under it when the user quits.
fn spawn_update_checker(
    enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    found: ui::UpdateHandle,
    wake: mpsc::Sender<Wake>,
) {
    std::thread::spawn(move || {
        let mut wait = FIRST_UPDATE_CHECK_DELAY;
        loop {
            std::thread::sleep(wait);
            wait = UPDATE_CHECK_INTERVAL;
            if !enabled.load(Ordering::Relaxed) {
                continue;
            }
            if let Some(update) = update::check() {
                // Already showing this exact release: no need to make the tray
                // re-render for it.
                if found.get().as_ref() == Some(&update) {
                    continue;
                }
                found.set(Some(update));
                if wake.send(Wake::UpdateAvailable).is_err() {
                    // The poll loop is gone: the process is on its way out.
                    return;
                }
            }
        }
    });
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
    let kayfabe_path = source::default_kayfabe_path();
    let stored = config::load();
    let env_secs = config::env_override(std::env::var("CLAUDE_TRAY_POLL_SECS").ok().as_deref());
    let settings = ui::Settings::new(stored, env_secs);
    let interval = settings.interval_handle();
    let notify_prefs = settings.notify_handle();
    let appearance = settings.appearance_handle();
    let check_updates = settings.check_updates_handle();
    let updates = settings.update_handle();
    let tz = TimeZone::system();

    let (wake_tx, wake_rx) = mpsc::channel::<Wake>();

    // The only network activity in the whole program, and the only thing the
    // `check_updates` setting gates.
    spawn_update_checker(check_updates, updates, wake_tx.clone());

    // Watch the desktop's light/dark preference regardless of the current
    // style: it costs one thread and one D-Bus connection, and it means
    // switching to "Monochrome (auto)" is already correct instead of waiting
    // for the next theme change. The handle ignores the value while a
    // non-auto style is selected, so no repaint is triggered for it.
    {
        let appearance = appearance.clone();
        let wake_tx = wake_tx.clone();
        platform::watch_appearance(move |dark_ui| {
            if appearance.set_portal_dark(dark_ui) {
                let _ = wake_tx.send(Wake::AppearanceChanged);
            }
        });
    }

    let snapshot = source::read_snapshot_or_kayfabe(&cache_path, &kayfabe_path, Timestamp::now());

    let core = ui::TrayCore::new(snapshot.clone(), settings, wake_tx);
    // Blocks for the rest of the program: on Linux the closure below runs on
    // this thread and the tray service gets one of its own; another backend may
    // do the reverse. Nothing after this call may assume it came back early.
    let started = platform::run(core, move |handle| {
        poll_loop(
            handle,
            snapshot,
            &cache_path,
            &kayfabe_path,
            &wake_rx,
            &interval,
            &notify_prefs,
            &tz,
        );
    });
    if let Err(err) = started {
        eprintln!("claude-usage-tray: {err}");
        std::process::exit(1);
    }
}

/// The poll loop: re-reads the cache on a timer, on demand, and whenever a
/// pending quota reset comes due, pushing anything that changed to the tray and
/// emitting the notifications the pure state machines in [`ui`] ask for.
///
/// Returning ends the program — the backend shuts the tray down and
/// [`platform::run`] returns.
#[allow(clippy::too_many_arguments)]
fn poll_loop(
    handle: platform::TrayHandle,
    mut snapshot: source::UsageSnapshot,
    cache_path: &std::path::Path,
    kayfabe_path: &std::path::Path,
    wake_rx: &mpsc::Receiver<Wake>,
    interval: &std::sync::atomic::AtomicU64,
    notify_prefs: &ui::NotifyHandle,
    tz: &TimeZone,
) {
    // The first cycle's reading becomes the notifier's baseline rather than a
    // volley of alerts for crossings that happened before this process
    // existed; see `Notifier`.
    let mut notifier = ui::Notifier::new(&notify_prefs.get().thresholds);
    let mut reset_notifier = ui::ResetNotifier::new();

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
            let next =
                source::read_snapshot_or_kayfabe(cache_path, kayfabe_path, Timestamp::now());
            if ui::snapshot_changed(&snapshot, &next) {
                handle.set_snapshot(next.clone());
            }
            snapshot = next;
            continue;
        }

        // Re-read the interval every cycle so a settings change applies live,
        // and never sleep past a pending quota reset.
        let wait = ui::poll_wait(
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
            // reads, so a bare refresh is enough to make the backend re-render
            // and push the new pixmaps.
            Ok(Wake::AppearanceChanged) => {
                handle.refresh();
                continue;
            }
            // A newer release was found. The release itself is already in
            // shared state that `menu` reads, so a bare refresh is enough to
            // make the extra row appear. No toast: a version banner is not
            // worth interrupting anyone for.
            Ok(Wake::UpdateAvailable) => {
                handle.refresh();
                continue;
            }
            // The first-run menu item. The install itself runs here, off the
            // menu callback, then falls through to a re-read: the cache will
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

        let next = source::read_snapshot_or_kayfabe(cache_path, kayfabe_path, Timestamp::now());
        if ui::snapshot_changed(&snapshot, &next) {
            handle.set_snapshot(next.clone());
        }
        match post_read {
            PostRead::RefreshToast => {
                notify_refresh(&ui::refresh_message(
                    &snapshot,
                    &next,
                    Timestamp::now(),
                    tz,
                ));
            }
            PostRead::StatusToast => {
                notify_refresh(&ui::status_message(&next, Timestamp::now(), tz));
            }
            PostRead::Silent => {}
        }
        snapshot = next;
    }
}
