//! Claude usage tray: reads the statusline usage cache on a timer and renders
//! it as a tray icon (a StatusNotifierItem on Linux, a menu bar `NSStatusItem`
//! on macOS). No credentials, no writes outside its
//! own config, the Claude config directory, and autostart files, and no
//! network beyond the optional once-daily GitHub release check in
//! [`update`] — nothing about the user's usage ever leaves the machine.
//!
//! The same binary is also the statusline command itself (`statusline`
//! subcommand) and its own installer (`hook install|uninstall|status`), so
//! there is no shell snippet anywhere in the design.
//!
//! Launching (tray mode): a bare `claude-usage-tray` re-executes itself with a
//! private flag, in its own process group with its standard streams on
//! `/dev/null`, and the parent returns the terminal — see [`detach`].
//! `--foreground` skips that. Either way exactly one tray may run at a time,
//! enforced by the `flock` in [`instance`]; `restart` is how an upgraded
//! binary replaces the copy already running.
//!
//! Threading (tray mode): the platform backend decides which thread the poll
//! loop gets ([`platform::run`] takes it as a closure precisely so that
//! neither side has to assume). On Linux the tray service runs on its own
//! thread and the main thread *is* the poll loop; on macOS it is the other way
//! round, because AppKit owns the main thread. Either way the loop waits on an
//! mpsc channel
//! with a timeout so that "Check for new data", left-click (a worded usage
//! summary), "Quit", "Install hook", and interval changes take effect
//! immediately instead of on the next tick.

mod appcache;
mod binary;
mod cli_refresh;
mod config;
mod hook;
mod icon;
mod instance;
mod menu;
mod platform;
mod source;
#[cfg(test)]
mod testutil;
mod ui;
mod update;

use jiff::{Timestamp, tz::TimeZone};
use platform::{Channel, Toast, Urgency};
use std::io::{Read, Write};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use ui::{ResetAlert, UsageAlert, Wake};

/// Builds the toast for a threshold crossing. Split out from [`notify`] so
/// the routing to [`Channel::ThresholdAlert`] — the whole point of this
/// function existing — is checkable without going through the backend's
/// D-Bus call; see the `notify_channel_routing` tests below.
fn threshold_toast(alert: &UsageAlert) -> (Toast, Channel) {
    (
        Toast {
            summary: alert.summary(),
            body: alert.body(),
            urgency: if alert.critical {
                Urgency::Critical
            } else {
                Urgency::Normal
            },
            transient: false,
        },
        Channel::ThresholdAlert,
    )
}

/// Builds the toast for the "your 5-hour window rolled over" notification.
/// Normal urgency: it is good news, not a warning. Ephemeral: a later
/// threshold alert must not replace it, and it must not replace one either.
fn reset_toast(alert: &ResetAlert) -> (Toast, Channel) {
    (
        Toast {
            summary: alert.summary(),
            body: alert.body(),
            urgency: Urgency::Normal,
            transient: false,
        },
        Channel::Ephemeral,
    )
}

/// Builds the toast that follows a *user-initiated* action ("Check for new
/// data", the `Install hook` item, or a left-click status readout): low
/// urgency and transient, so it acknowledges the click without piling up in
/// notification history the way the threshold alerts (deliberately) do, and
/// ephemeral so it never competes with a threshold alert for the same slot.
fn refresh_toast(body: &str) -> (Toast, Channel) {
    (
        Toast {
            summary: "Claude usage tray".to_string(),
            body: body.to_string(),
            urgency: Urgency::Low,
            transient: true,
        },
        Channel::Ephemeral,
    )
}

/// Builds the toast for "the binary underneath you was upgraded". Normal
/// urgency and not transient: it is worth finding again in notification
/// history, since the thing it asks for (a restart) is not something anyone
/// does mid-sentence. Ephemeral, so it neither replaces nor is replaced by a
/// threshold alert.
fn binary_swapped_toast() -> (Toast, Channel) {
    (
        Toast {
            summary: "Claude usage tray".to_string(),
            body: ui::RESTART_TO_UPDATE_TOAST.to_string(),
            urgency: Urgency::Normal,
            transient: false,
        },
        Channel::Ephemeral,
    )
}

/// Emits a threshold notification. Failures (no notification daemon, D-Bus
/// down) are ignored by the backend: a missing notification must never take
/// the tray with it.
fn notify(alert: &UsageAlert) {
    let (toast, channel) = threshold_toast(alert);
    platform::notify(&toast, channel);
}

/// Emits the "your 5-hour window rolled over" notification.
fn notify_reset(alert: &ResetAlert) {
    let (toast, channel) = reset_toast(alert);
    platform::notify(&toast, channel);
}

/// Emits the toast that follows a *user-initiated* action.
fn notify_refresh(body: &str) {
    let (toast, channel) = refresh_toast(body);
    platform::notify(&toast, channel);
}

/// Emits the "a new binary was installed under you" notification.
fn notify_binary_swapped() {
    let (toast, channel) = binary_swapped_toast();
    platform::notify(&toast, channel);
}


const USAGE: &str = "\
claude-usage-tray — Claude Code usage in the system tray

  claude-usage-tray                      run the tray in the background
  claude-usage-tray --foreground         run the tray in this terminal
  claude-usage-tray restart              stop the running tray and start again
  claude-usage-tray statusline [--exec CMD]
                                         Claude Code statusline command: caches
                                         the stdin JSON, optionally running CMD
                                         and passing its output through
  claude-usage-tray hook install         point statusLine.command at this binary
  claude-usage-tray hook uninstall       undo that
  claude-usage-tray hook status          report what is currently wired up
";

/// The subcommand that replaces a running instance. Named here because the
/// `Restart to update` menu row spawns the new binary with exactly this word,
/// rather than reimplementing what it does.
pub(crate) const RESTART_COMMAND: &str = "restart";

/// The documented flag that keeps the tray attached to the terminal.
const FOREGROUND_FLAG: &str = "--foreground";

/// The private flag the detaching parent passes to the copy of itself it
/// spawns. Behaves exactly like [`FOREGROUND_FLAG`] and is deliberately left
/// out of [`USAGE`]: it exists so a process listing distinguishes the
/// re-executed child from a user's own foreground run, not for anybody to
/// type.
const RUN_FOREGROUND_FLAG: &str = "__run-foreground";

/// Printed by the parent before it exits, so "nothing happened" is never the
/// visible outcome of launching the tray.
const BACKGROUNDED: &str =
    "claude-usage-tray: running in the background (use --foreground to keep it attached)";

/// Printed when the single-instance lock is already held. It names the way
/// out, because the common cause is a package upgrade: the old binary is still
/// running and the user has just tried to start the new one.
const ALREADY_RUNNING: &str =
    "claude-usage-tray is already running (run 'claude-usage-tray restart' to replace it)";

/// How long `restart` waits for the old instance to let go of the lock.
const RESTART_TIMEOUT: Duration = Duration::from_secs(10);

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// Bare invocation: re-exec into the background and return the terminal.
    Detach,
    /// Run the tray in this process (`--foreground`, or the private flag the
    /// detaching parent uses).
    Foreground,
    /// Replace whichever instance is running.
    Restart,
    Statusline,
    Hook,
    Usage,
}

fn parse_mode(args: &[String]) -> Mode {
    match args.first().map(String::as_str) {
        None => Mode::Detach,
        Some(flag) if flag == FOREGROUND_FLAG && args.len() == 1 => Mode::Foreground,
        Some(flag) if flag == RUN_FOREGROUND_FLAG && args.len() == 1 => Mode::Foreground,
        Some(command) if command == RESTART_COMMAND && args.len() == 1 => Mode::Restart,
        Some("statusline") => Mode::Statusline,
        Some("hook") => Mode::Hook,
        _ => Mode::Usage,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match parse_mode(&args) {
        Mode::Detach => detach(),
        Mode::Foreground => run_tray_locked(),
        Mode::Restart => run_restart(),
        // Never detached, and never single-instanced: both are synchronous
        // commands the user (or Claude Code) is waiting on the exit of, and
        // `statusline` in particular runs concurrently with a live tray by
        // design.
        Mode::Statusline => run_statusline(&args[1..]),
        Mode::Hook => run_hook(&args[1..]),
        Mode::Usage => {
            eprint!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

/// Re-executes this binary in the background and returns the terminal.
///
/// A re-exec rather than a bare `fork`: the tray brings up AppKit on macOS,
/// and a forked child that has not `exec`ed may not safely touch it. Spawning
/// a fresh process sidesteps the whole question and costs one extra `exec`
/// once per launch.
///
/// The single-instance check happens *here*, before the spawn, as well as in
/// the child. The child's stderr is `/dev/null`, so a refusal printed there
/// would be invisible; doing it in the parent is what makes "already running"
/// something the user actually reads, with a nonzero exit to match.
fn detach() -> i32 {
    match instance::try_acquire(&instance::lock_path()) {
        Ok(None) => {
            eprintln!("{ALREADY_RUNNING}");
            return 1;
        }
        // Free: release it again immediately so the child can take it. The
        // gap is a race only against another launch in the same instant, and
        // the child's own check is what closes it.
        Ok(Some(lock)) => drop(lock),
        // The lock file is unusable (read-only runtime directory, say). Not a
        // reason to refuse to run the tray.
        Err(_) => {}
    }
    spawn_background()
}

/// Spawns the detached child and reports it. Split from [`detach`] so
/// `restart` can reuse it after clearing the way.
fn spawn_background() -> i32 {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("claude-usage-tray: cannot determine my own path: {err}");
            return 1;
        }
    };
    // Detached in its own process group, so closing the terminal (or a Ctrl-C
    // meant for whatever the user runs next) does not take the tray with it.
    match instance::spawn_detached(&exe, RUN_FOREGROUND_FLAG) {
        Ok(_) => {
            println!("{BACKGROUNDED}");
            0
        }
        Err(err) => {
            eprintln!("claude-usage-tray: could not start in the background: {err}");
            1
        }
    }
}

/// The `restart` subcommand: stop whatever instance is running, then start a
/// fresh one in the background.
///
/// This is what makes an upgrade a one-liner. The newly installed binary
/// cannot simply start — the old process still holds the lock — and asking
/// people to hunt for a PID is a poor answer, so the tool does the hunting: the
/// lock file carries the running instance's PID, and the lock itself is the
/// proof that the PID is live.
fn run_restart() -> i32 {
    let path = instance::lock_path();
    match instance::try_acquire(&path) {
        // Nothing running: "restart" is just "start".
        Ok(Some(lock)) => drop(lock),
        Err(_) => {}
        Ok(None) => {
            let Some(pid) = instance::read_pid(&path) else {
                eprintln!(
                    "claude-usage-tray: another instance is running but did not record its \
                     process id, so it cannot be stopped from here; quit it from its tray menu \
                     and try again"
                );
                return 1;
            };
            if !instance::terminate(pid) {
                eprintln!("claude-usage-tray: could not stop the running instance (pid {pid})");
                return 1;
            }
            if !instance::wait_until_free(&path, RESTART_TIMEOUT) {
                eprintln!(
                    "claude-usage-tray: the running instance (pid {pid}) did not exit; nothing \
                     was started"
                );
                return 1;
            }
            println!("claude-usage-tray: stopped the running instance (pid {pid})");
        }
    }
    spawn_background()
}

/// Takes the single-instance lock, then runs the tray for the life of the
/// process. The lock is held by leaking the open file: the kernel releases it
/// when this process ends, however it ends.
fn run_tray_locked() -> i32 {
    match instance::try_acquire(&instance::lock_path()) {
        Ok(Some(lock)) => {
            lock.record_pid();
            lock.hold_forever();
        }
        Ok(None) => {
            eprintln!("{ALREADY_RUNNING}");
            return 1;
        }
        Err(_) => {}
    }
    run_tray();
    0
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
    let app_cache_path = appcache::app_cache_path();
    let kayfabe_path = source::default_kayfabe_path();
    let stored = config::load();
    let env_secs = config::env_override(std::env::var("CLAUDE_TRAY_POLL_SECS").ok().as_deref());
    let settings = ui::Settings::new(stored, env_secs);
    let interval = settings.interval_handle();
    let notify_prefs = settings.notify_handle();
    let appearance = settings.appearance_handle();
    let check_updates = settings.check_updates_handle();
    let cli_refresh = settings.cli_refresh_handle();
    let updates = settings.update_handle();
    let restart = settings.restart_handle();
    let tz = TimeZone::system();

    // The path of the binary this process was started from, watched for the
    // moment a package upgrade replaces it. Recorded here, before anything
    // else can chdir or otherwise disturb the answer. If the path cannot be
    // determined at all there is nothing to watch and nothing to restart into,
    // so the feature simply stays quiet.
    let mut binary_watch = std::env::current_exe().ok().map(binary::BinaryWatch::new);

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

    let snapshot = source::read_merged_or_kayfabe(
        &cache_path,
        &app_cache_path,
        &kayfabe_path,
        Timestamp::now(),
    );

    let core = ui::TrayCore::new(snapshot.clone(), settings, wake_tx);
    // Blocks for the rest of the program: on Linux the closure below runs on
    // this thread and the tray service gets one of its own; another backend may
    // do the reverse. Nothing after this call may assume it came back early.
    let started = platform::run(core, move |handle| {
        poll_loop(
            handle,
            snapshot,
            &cache_path,
            &app_cache_path,
            &kayfabe_path,
            &wake_rx,
            &cli_refresh,
            &interval,
            &notify_prefs,
            &tz,
            binary_watch.as_mut(),
            &restart,
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
    app_cache_path: &std::path::Path,
    kayfabe_path: &std::path::Path,
    wake_rx: &mpsc::Receiver<Wake>,
    cli_refresh: &std::sync::atomic::AtomicBool,
    interval: &std::sync::atomic::AtomicU64,
    notify_prefs: &ui::NotifyHandle,
    tz: &TimeZone,
    mut binary_watch: Option<&mut binary::BinaryWatch>,
    restart: &ui::RestartHandle,
) {
    // The first cycle's reading becomes the notifier's baseline rather than a
    // volley of alerts for crossings that happened before this process
    // existed; see `Notifier`.
    let mut notifier = ui::Notifier::new(&notify_prefs.get().thresholds);
    let mut reset_notifier = ui::ResetNotifier::new();
    // When this process last spawned `claude -p "/usage"`. Process-local on
    // purpose: an hour-scale cadence does not need to survive restarts, and
    // the blob's own `fetchedAtMs` already carries the cross-process truth.
    let mut last_cli_refresh: Option<Timestamp> = None;

    loop {
        // Re-read the preferences every cycle so a menu toggle applies live.
        let prefs = notify_prefs.get();
        notifier.set_enabled(&prefs.thresholds);
        if let Some(alert) = notifier.evaluate(snapshot.session.as_ref()) {
            notify(&alert);
        }

        // Optionally keep Claude Code's usage blob (the per-model rows'
        // only source) from going stale: at most one headless CLI run per
        // hour, gated by the settings toggle. The spawn is fire-and-forget;
        // a later poll tick picks up whatever it wrote.
        if cli_refresh::should_refresh(
            cli_refresh.load(Ordering::Relaxed),
            snapshot.scoped_fetched_at,
            last_cli_refresh,
            Timestamp::now(),
        ) {
            last_cli_refresh = Some(Timestamp::now());
            cli_refresh::spawn_refresh();
        }

        // Has the program on disk been replaced since this process started? One
        // `stat` per cycle, and it fires exactly once: the toast says it, and
        // the menu row keeps offering it from then on.
        if let Some(watch) = binary_watch.as_deref_mut()
            && watch.check()
        {
            restart.set(Some(watch.path().to_path_buf()));
            notify_binary_swapped();
            // The row lives in shared state that `menu` reads, so a repaint is
            // all it takes to make it appear without waiting for a click.
            handle.refresh();
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
            let next = source::read_merged_or_kayfabe(
                cache_path,
                app_cache_path,
                kayfabe_path,
                Timestamp::now(),
            );
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

        let next = source::read_merged_or_kayfabe(
            cache_path,
            app_cache_path,
            kayfabe_path,
            Timestamp::now(),
        );
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

/// Pure logic worth testing directly, without going through the backend's
/// D-Bus call: which [`Channel`] each kind of toast is routed to. The
/// replace-in-place mechanism itself (retaining and updating a
/// `NotificationHandle`) lives in `platform::linux` and talks to a live
/// notification daemon, so it is not unit-testable here — that verification
/// is left to the orchestrator running this on a real desktop.
#[cfg(test)]
mod command_line {
    use super::*;

    fn mode(args: &[&str]) -> Mode {
        let owned: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        parse_mode(&owned)
    }

    #[test]
    fn a_bare_invocation_detaches() {
        assert_eq!(mode(&[]), Mode::Detach);
    }

    #[test]
    fn the_foreground_flag_and_the_private_flag_both_run_here() {
        assert_eq!(mode(&["--foreground"]), Mode::Foreground);
        assert_eq!(mode(&["__run-foreground"]), Mode::Foreground);
    }

    #[test]
    fn restart_is_its_own_mode() {
        assert_eq!(mode(&["restart"]), Mode::Restart);
    }

    /// The two synchronous commands must never be detached: something is
    /// waiting on their output and their exit code.
    #[test]
    fn the_synchronous_subcommands_are_untouched() {
        assert_eq!(mode(&["statusline"]), Mode::Statusline);
        assert_eq!(mode(&["statusline", "--exec", "prompt"]), Mode::Statusline);
        assert_eq!(mode(&["hook", "install"]), Mode::Hook);
        assert_eq!(mode(&["hook", "status"]), Mode::Hook);
    }

    #[test]
    fn anything_else_is_a_usage_error() {
        for args in [
            vec!["--background"],
            vec!["-f"],
            vec!["--foreground", "extra"],
            vec!["restart", "now"],
            vec![""],
        ] {
            assert_eq!(mode(&args), Mode::Usage, "unexpected mode for {args:?}");
        }
    }

    /// The private flag is a mechanism, not an interface: documenting it would
    /// invite people to use it instead of `--foreground`.
    #[test]
    fn the_usage_text_documents_the_public_flags_only() {
        assert!(USAGE.contains("--foreground"));
        assert!(USAGE.contains("claude-usage-tray restart"));
        assert!(!USAGE.contains(RUN_FOREGROUND_FLAG));
    }

    /// The refusal has to tell an upgrading user what to do next, or "already
    /// running" is a dead end with the old binary still in the tray.
    #[test]
    fn the_already_running_message_points_at_restart() {
        assert!(ALREADY_RUNNING.starts_with("claude-usage-tray is already running"));
        assert!(ALREADY_RUNNING.contains("claude-usage-tray restart"));
    }

    #[test]
    fn the_background_notice_mentions_how_to_stay_attached() {
        assert!(BACKGROUNDED.contains("running in the background"));
        assert!(BACKGROUNDED.contains(FOREGROUND_FLAG));
    }
}

#[cfg(test)]
mod notify_channel_routing {
    use super::*;

    #[test]
    fn normal_threshold_alert_routes_to_threshold_alert_channel() {
        let alert = UsageAlert {
            threshold: 75,
            percent: 75.0,
            critical: false,
        };
        let (toast, channel) = threshold_toast(&alert);
        assert_eq!(channel, Channel::ThresholdAlert);
        assert_eq!(toast.urgency, Urgency::Normal);
        assert!(!toast.transient);
    }

    #[test]
    fn critical_threshold_alert_also_routes_to_threshold_alert_channel() {
        // The whole point: a 90% critical alert must land on the same
        // replaceable channel as the 75% one it supersedes, not a separate
        // lane that would let both stack.
        let alert = UsageAlert {
            threshold: 90,
            percent: 90.0,
            critical: true,
        };
        let (toast, channel) = threshold_toast(&alert);
        assert_eq!(channel, Channel::ThresholdAlert);
        assert_eq!(toast.urgency, Urgency::Critical);
        assert!(!toast.transient);
    }

    #[test]
    fn reset_alert_routes_to_ephemeral_channel() {
        let alert = ResetAlert {
            at: Timestamp::now(),
        };
        let (_, channel) = reset_toast(&alert);
        assert_eq!(channel, Channel::Ephemeral);
    }

    /// The upgrade notice must not land on the threshold lane: it would
    /// replace (or be replaced by) a live usage warning, which is the one
    /// thing the user cannot afford to lose.
    #[test]
    fn the_binary_swap_toast_routes_to_the_ephemeral_channel_and_persists() {
        let (toast, channel) = binary_swapped_toast();
        assert_eq!(channel, Channel::Ephemeral);
        assert_eq!(toast.urgency, Urgency::Normal);
        assert!(
            !toast.transient,
            "a restart prompt is worth finding again in history"
        );
        assert_eq!(toast.body, "Update installed — restart to apply");
    }

    #[test]
    fn refresh_toast_routes_to_ephemeral_channel() {
        let (toast, channel) = refresh_toast("2 requests today");
        assert_eq!(channel, Channel::Ephemeral);
        assert!(toast.transient);
    }
}
