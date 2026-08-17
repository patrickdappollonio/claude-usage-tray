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
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use std::time::Instant;
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

/// How long `restart` waits for the old instance to let go of the lock: the
/// overall deadline for clearing the way.
const RESTART_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `restart` waits to see the replacement actually holding the lock
/// before declaring the restart failed. Generous: the child spends up to
/// [`CHILD_LOCK_TIMEOUT`] just waiting out its own parent.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Exit code of `restart` meaning "this failed, and the failure was already
/// shown to the user as a notification from this process". The watching tray
/// (if one survives) must not toast the same failure again; code 1 means the
/// opposite — nobody has reported it, so the watcher should. Deliberately
/// not 2: that is this binary's usage-error code, and an older binary that
/// does not know the `restart` subcommand exits 2 after printing usage — a
/// failure that must be reported, not mistaken for "already reported".
pub(crate) const EXIT_RESTART_REPORTED: i32 = 3;

/// How long a direct launch (`claude-usage-tray`, `--foreground`) tries for
/// the lock before concluding another instance runs. Not a single attempt:
/// the create-nothing probes other processes run take LOCK_EX momentarily,
/// and one collision must not misread "held" — or worse, act on a stale PID
/// left behind in a free lock file.
const DIRECT_LAUNCH_PATIENCE: Duration = Duration::from_millis(300);

/// How long a freshly spawned tray waits for its parent — which holds the
/// lock across the spawn — to exit and release it.
const CHILD_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// The least time a just-SIGTERM'd tray is given to exit, even when the
/// shared restart deadline is already spent. Killing a tray and then waiting
/// zero milliseconds for it would turn a slow-but-clean exit into a reported
/// failure.
const POST_KILL_FLOOR: Duration = Duration::from_secs(2);

/// Writes a diagnostic line to stderr, swallowing write failures. On the
/// restart path stderr is a pipe into the old tray; once that tray is gone
/// the pipe has no reader and a bare `eprintln!` would abort this process on
/// the very line meant to explain the failure.
fn say(message: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "claude-usage-tray: {message}");
}

/// The outcome of trying to become *the* instance.
enum Claim {
    /// This process now holds the lock (PID recorded). Keep it alive for as
    /// long as being the instance matters.
    Held(instance::InstanceLock),
    /// Another instance is (or may be) running — here or at a pre-move
    /// legacy path.
    Busy,
    /// The lock file is unusable (read-only directory, say). Not a reason to
    /// keep the user from running the tray.
    Unlockable,
}

/// Takes the primary lock and checks the legacy paths, in that order.
///
/// The PID is recorded the instant the lock is taken: a held lock whose file
/// still names a previous holder would invite `restart` to signal a stale —
/// possibly recycled — PID. (A hole the width of the acquire-to-record gap
/// remains; it was previously the width of the whole spawn.) A legacy path
/// that cannot be checked but names a PID counts as running: the only reason
/// to look there is that an old tray might be holding it.
fn claim_instance(primary: &Path, legacy: &[PathBuf], patience: Duration) -> Claim {
    let lock = match instance::acquire_with_retry(primary, patience) {
        Ok(Some(lock)) => {
            lock.record_pid();
            Some(lock)
        }
        Ok(None) => return Claim::Busy,
        Err(_) => None,
    };
    for path in legacy {
        match instance::probe_held(path) {
            instance::Probe::Held => return Claim::Busy,
            instance::Probe::Free => {}
            instance::Probe::Unknown => {
                if instance::read_pid(path).is_some() {
                    return Claim::Busy;
                }
                say(&format!("could not check the old lock at {}", path.display()));
            }
        }
    }
    match lock {
        Some(lock) => Claim::Held(lock),
        None => Claim::Unlockable,
    }
}

/// A failure to clear a lock path, split by whether a SIGTERM had already
/// been issued: before the kill the old tray is alive to report the failure
/// (it reads this process's stderr); after the kill only this process can.
#[derive(Debug, PartialEq, Eq)]
enum ClearFailure {
    BeforeKill(String),
    AfterKill(String),
}

/// Stops whatever holds a pre-move lock at `path`, creating nothing.
/// `Ok(signalled)` reports whether a SIGTERM was actually sent.
fn clear_legacy(path: &Path, deadline: Instant) -> Result<bool, ClearFailure> {
    match instance::probe_held(path) {
        instance::Probe::Free => return Ok(false),
        instance::Probe::Unknown => {
            // The one probe answer that must not mean "go ahead": the only
            // reason to look here is that an old tray might be holding it.
            return match instance::read_pid(path) {
                Some(_) => Err(ClearFailure::BeforeKill(format!(
                    "an older instance may still be running (lock: {}), but it cannot \
                     be checked from here; quit it from its tray menu and try again",
                    path.display()
                ))),
                None => {
                    say(&format!("could not check the old lock at {}", path.display()));
                    Ok(false)
                }
            };
        }
        instance::Probe::Held => {}
    }
    let Some(pid) = instance::read_pid(path) else {
        return Err(ClearFailure::BeforeKill(
            "another instance is running but did not record its process id, so it \
             cannot be stopped from here; quit it from its tray menu and try again"
                .to_string(),
        ));
    };
    if !instance::terminate(pid) {
        return Err(ClearFailure::BeforeKill(format!(
            "could not stop the running instance (pid {pid})"
        )));
    }
    // From here on the failure mode changes owner: a SIGTERM is out, so the
    // tray it went to may no longer be around to report anything. The floor
    // keeps an exhausted deadline from turning a clean-but-slow exit into a
    // reported failure (D9).
    let deadline = deadline.max(Instant::now() + POST_KILL_FLOOR);
    loop {
        match instance::probe_held(path) {
            instance::Probe::Free => break,
            // Unknown is not "gone": a transiently unreadable lock must not
            // let a still-running tray be declared stopped.
            instance::Probe::Held | instance::Probe::Unknown => {}
        }
        if Instant::now() >= deadline {
            return Err(ClearFailure::AfterKill(format!(
                "the running instance (pid {pid}) did not exit; nothing was started"
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    say(&format!("stopped the running instance (pid {pid})"));
    Ok(true)
}

/// Clears the primary lock path and ends *holding* it (PID recorded), so the
/// gap between "the old tray is gone" and "the replacement is spawned" is
/// never an unlocked one. `Ok((None, _))` means the lock file is unusable.
fn clear_and_hold_primary(
    path: &Path,
    deadline: Instant,
) -> Result<(Option<instance::InstanceLock>, bool), ClearFailure> {
    match instance::acquire_with_retry(path, DIRECT_LAUNCH_PATIENCE) {
        Ok(Some(lock)) => {
            lock.record_pid();
            return Ok((Some(lock), false));
        }
        Err(_) => return Ok((None, false)),
        Ok(None) => {}
    }
    let Some(pid) = instance::read_pid(path) else {
        return Err(ClearFailure::BeforeKill(
            "another instance is running but did not record its process id, so it \
             cannot be stopped from here; quit it from its tray menu and try again"
                .to_string(),
        ));
    };
    if !instance::terminate(pid) {
        return Err(ClearFailure::BeforeKill(format!(
            "could not stop the running instance (pid {pid})"
        )));
    }
    // The floor keeps an exhausted deadline from meaning "kill, then wait
    // zero milliseconds" (D9).
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(POST_KILL_FLOOR);
    match instance::acquire_with_retry(path, remaining) {
        Ok(Some(lock)) => {
            lock.record_pid();
            say(&format!("stopped the running instance (pid {pid})"));
            Ok((Some(lock), true))
        }
        Ok(None) => Err(ClearFailure::AfterKill(format!(
            "the running instance (pid {pid}) did not exit; nothing was started"
        ))),
        Err(_) => Ok((None, true)),
    }
}

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// Bare invocation: re-exec into the background and return the terminal.
    Detach,
    /// Run the tray in this process (`--foreground`, or the private flag the
    /// detaching parent uses). `spawned` distinguishes the two: a child that
    /// was just spawned waits out its parent's hold on the lock, while a
    /// user's own `--foreground` beside a running tray is answered promptly.
    Foreground { spawned: bool },
    /// Replace whichever instance is running.
    Restart,
    Statusline,
    Hook,
    Usage,
}

fn parse_mode(args: &[String]) -> Mode {
    match args.first().map(String::as_str) {
        None => Mode::Detach,
        Some(flag) if flag == FOREGROUND_FLAG && args.len() == 1 => {
            Mode::Foreground { spawned: false }
        }
        Some(flag) if flag == RUN_FOREGROUND_FLAG && args.len() == 1 => {
            Mode::Foreground { spawned: true }
        }
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
        Mode::Foreground { spawned } => run_tray_locked(spawned),
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
    // The claim is handed to spawn_background, which holds it across the
    // spawn. The child retries the lock until this process releases it, so
    // concurrent launches serialize on the lock file instead of slipping
    // through a probe-then-spawn gap.
    let held = match claim_instance(
        &instance::lock_path(),
        &instance::legacy_lock_paths(),
        DIRECT_LAUNCH_PATIENCE,
    ) {
        Claim::Busy => {
            eprintln!("{ALREADY_RUNNING}");
            return 1;
        }
        Claim::Held(lock) => Some(lock),
        Claim::Unlockable => None,
    };
    match spawn_background(held) {
        Ok(()) => 0,
        Err(message) => {
            say(&message);
            1
        }
    }
}

/// Spawns the detached child and reports it. `held` is the instance lock the
/// caller kept across the clearing of the way; it is released the moment the
/// spawn has happened, deliberately *before* the stdout write — a stalled
/// stdout reader must not keep the lock from the child, which only retries
/// for a few seconds. Failures come back as text: the caller knows whether a
/// human, a pipe, or a toast is listening.
fn spawn_background(held: Option<instance::InstanceLock>) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("cannot determine my own path: {err}"))?;
    // Detached in its own process group, so closing the terminal (or a Ctrl-C
    // meant for whatever the user runs next) does not take the tray with it.
    match instance::spawn_detached(&exe, RUN_FOREGROUND_FLAG) {
        Ok(_) => {
            drop(held);
            println!("{BACKGROUNDED}");
            Ok(())
        }
        Err(err) => Err(format!("could not start in the background: {err}")),
    }
}

/// The `restart` subcommand: stop whatever instance is running — at the
/// current lock path or a pre-move legacy one — then start a fresh tray and
/// stay alive long enough to confirm it is really up.
///
/// Reporting is split by who is alive (D1). While no SIGTERM has been sent,
/// the tray that spawned this process is alive by construction, reads this
/// process's stderr, and raises the toast on exit code 1. Once *any* SIGTERM
/// is out — or an after-kill timeout says the target may or may not die at
/// any moment — the toast comes from here, before the stderr write (D6: the
/// pipe may be dead), and the exit code becomes [`EXIT_RESTART_REPORTED`] so
/// a surviving watcher stays silent instead of duplicating it.
fn run_restart() -> i32 {
    let deadline = Instant::now() + RESTART_TIMEOUT;
    let mut killed = false;
    for legacy in instance::legacy_lock_paths() {
        match clear_legacy(&legacy, deadline) {
            Ok(signalled) => killed |= signalled,
            Err(failure) => return report_restart_failure(failure, killed),
        }
    }
    let primary = instance::lock_path();
    let held = match clear_and_hold_primary(&primary, deadline) {
        Ok((held, signalled)) => {
            killed |= signalled;
            held
        }
        Err(failure) => return report_restart_failure(failure, killed),
    };
    let verifiable = held.is_some();
    if let Err(message) = spawn_background(held) {
        return report_failure_text(&message, killed);
    }
    // An unusable lock file means nothing can ever hold it: verification
    // would time out against a replacement that is running fine.
    if verifiable && !verify_replacement(&primary, VERIFY_TIMEOUT) {
        return report_failure_text(
            "the new tray did not come up; check that the new binary runs",
            killed,
        );
    }
    0
}

/// Routes a clearing failure to whoever can still see it. `killed` is
/// whether any SIGTERM was issued *earlier* in this run — an `AfterKill`
/// failure implies one was issued inside the failing call itself.
fn report_restart_failure(failure: ClearFailure, killed: bool) -> i32 {
    match failure {
        ClearFailure::BeforeKill(message) => report_failure_text(&message, killed),
        ClearFailure::AfterKill(message) => report_failure_text(&message, true),
    }
}

/// Reports a restart failure and returns the exit code that tells a
/// surviving watcher whether it has been reported already. Toast before
/// stderr: once a kill happened, the stderr pipe may have no reader (D6).
fn report_failure_text(message: &str, reported_here: bool) -> i32 {
    if reported_here {
        notify_restart_failure(message);
        say(message);
        EXIT_RESTART_REPORTED
    } else {
        say(message);
        1
    }
}

/// Polls until *somebody* holds the lock at `path` — the replacement tray
/// announcing itself — or gives up. Uses [`instance::probe_held`] because it
/// creates nothing: `try_acquire` would manufacture the very file it is
/// checking for. ("Somebody" rather than "my grandchild" is deliberate: if a
/// concurrent restart's tray won instead, the user still has exactly one
/// tray, which is the outcome being verified.)
fn verify_replacement(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match instance::probe_held(path) {
            instance::Probe::Held => return true,
            instance::Probe::Free | instance::Probe::Unknown => {}
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Raises the "Could not restart to update" toast. Time-bounded: on macOS a
/// notification attempt from a short-lived process with no run loop could
/// otherwise wedge this process open forever — the bound turns "maybe no
/// toast" into the worst case instead of "a stuck process". When the sender
/// thread is still busy at the deadline, returning lets the process exit,
/// which reaps it.
pub(crate) fn notify_restart_failure(body: &str) {
    let toast = Toast {
        summary: "Could not restart to update".to_string(),
        body: body.to_string(),
        urgency: Urgency::Normal,
        // Worth scrolling back to, exactly like the threshold alerts.
        transient: false,
    };
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        platform::notify(&toast, Channel::Ephemeral);
        let _ = done_tx.send(());
    });
    let _ = done_rx.recv_timeout(Duration::from_secs(5));
}

/// Takes the single-instance lock, then runs the tray for the life of the
/// process. The lock is held by leaking the open file: the kernel releases it
/// when this process ends, however it ends.
fn run_tray_locked(spawned: bool) -> i32 {
    let patience = if spawned { CHILD_LOCK_TIMEOUT } else { DIRECT_LAUNCH_PATIENCE };
    match claim_instance(&instance::lock_path(), &instance::legacy_lock_paths(), patience) {
        Claim::Held(lock) => lock.hold_forever(),
        Claim::Busy => {
            eprintln!("{ALREADY_RUNNING}");
            return 1;
        }
        Claim::Unlockable => {}
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

    /// Both run the tray here; they differ only in `spawned`, which is what
    /// decides how long the lock is waited for — a spawned child waits out
    /// its parent, a user's own `--foreground` does not.
    #[test]
    fn the_foreground_flag_and_the_private_flag_both_run_here() {
        assert_eq!(mode(&["--foreground"]), Mode::Foreground { spawned: false });
        assert_eq!(
            mode(&["__run-foreground"]),
            Mode::Foreground { spawned: true }
        );
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
mod instance_claims {
    use super::*;

    #[test]
    fn claiming_records_this_process_pid_the_moment_the_lock_is_taken() {
        let temp = crate::testutil::TempDir::new("claim-pid");
        let primary = temp.path().join("tray.lock");
        let claim = claim_instance(&primary, &[], Duration::ZERO);
        let Claim::Held(lock) = claim else { panic!("expected Held") };
        assert_eq!(crate::instance::read_pid(&primary), Some(std::process::id() as i32));
        drop(lock);
    }

    #[test]
    fn claiming_refuses_when_the_primary_lock_is_held() {
        let temp = crate::testutil::TempDir::new("claim-primary-busy");
        let primary = temp.path().join("tray.lock");
        let held = crate::instance::try_acquire(&primary).expect("acquire").expect("free");
        assert!(matches!(claim_instance(&primary, &[], Duration::ZERO), Claim::Busy));
        drop(held);
    }

    #[test]
    fn claiming_refuses_when_a_legacy_lock_is_held_and_releases_the_primary() {
        let temp = crate::testutil::TempDir::new("claim-legacy-busy");
        let primary = temp.path().join("tray.lock");
        let legacy = temp.path().join("legacy.lock");
        let old_tray = crate::instance::try_acquire(&legacy).expect("acquire").expect("free");
        assert!(matches!(
            claim_instance(&primary, std::slice::from_ref(&legacy), Duration::ZERO),
            Claim::Busy
        ));
        // The primary taken during the refused claim must have been released.
        assert!(crate::instance::try_acquire(&primary).expect("probe").is_some());
        drop(old_tray);
    }

    #[test]
    fn claiming_ignores_a_legacy_path_that_does_not_exist_and_creates_nothing() {
        let temp = crate::testutil::TempDir::new("claim-legacy-absent");
        let primary = temp.path().join("tray.lock");
        let legacy = temp.path().join("caches").join("legacy.lock");
        let claim = claim_instance(&primary, std::slice::from_ref(&legacy), Duration::ZERO);
        assert!(matches!(claim, Claim::Held(_)));
        assert!(!legacy.exists(), "probing must not resurrect the legacy file");
    }

    #[test]
    fn clearing_a_free_or_absent_legacy_path_is_a_no_op() {
        let temp = crate::testutil::TempDir::new("clear-legacy-free");
        let legacy = temp.path().join("legacy.lock");
        let deadline = Instant::now() + Duration::from_millis(100);
        assert_eq!(clear_legacy(&legacy, deadline), Ok(false));
        assert!(!legacy.exists());
    }

    #[test]
    fn clearing_a_legacy_holder_that_ignores_the_signal_times_out_after_the_kill() {
        let temp = crate::testutil::TempDir::new("clear-legacy-deaf");
        let legacy = temp.path().join("legacy.lock");
        let held = crate::instance::try_acquire(&legacy).expect("acquire").expect("free");
        // A PID that does not exist: terminate() reports success (ESRCH), but the
        // lock stays held by this test, so waiting must time out. Writing the file
        // does not release the flock — the lock lives on `held`'s descriptor.
        std::fs::write(&legacy, b"2147483632\n").expect("write pid");
        let deadline = Instant::now() + Duration::from_millis(200);
        assert!(matches!(clear_legacy(&legacy, deadline), Err(ClearFailure::AfterKill(_))));
        drop(held);
    }

    #[test]
    fn clearing_a_legacy_holder_with_no_recorded_pid_fails_before_any_kill() {
        let temp = crate::testutil::TempDir::new("clear-legacy-no-pid");
        let legacy = temp.path().join("legacy.lock");
        let held = crate::instance::try_acquire(&legacy).expect("acquire").expect("free");
        let deadline = Instant::now() + Duration::from_millis(100);
        assert!(matches!(clear_legacy(&legacy, deadline), Err(ClearFailure::BeforeKill(_))));
        drop(held);
    }

    #[test]
    fn holding_the_primary_when_it_is_free_records_the_pid_and_keeps_the_lock() {
        let temp = crate::testutil::TempDir::new("hold-primary-free");
        let primary = temp.path().join("tray.lock");
        let deadline = Instant::now() + Duration::from_millis(400);
        let (held, signalled) = clear_and_hold_primary(&primary, deadline).expect("no error");
        assert!(!signalled);
        let held = held.expect("a lock");
        assert_eq!(crate::instance::read_pid(&primary), Some(std::process::id() as i32));
        assert!(crate::instance::try_acquire(&primary).expect("probe").is_none(), "still held");
        drop(held);
    }

    #[test]
    fn holding_the_primary_against_a_deaf_holder_times_out_after_the_kill() {
        let temp = crate::testutil::TempDir::new("hold-primary-deaf");
        let primary = temp.path().join("tray.lock");
        let held = crate::instance::try_acquire(&primary).expect("acquire").expect("free");
        std::fs::write(&primary, b"2147483632\n").expect("write pid");
        let deadline = Instant::now() + Duration::from_millis(600);
        assert!(matches!(
            clear_and_hold_primary(&primary, deadline),
            Err(ClearFailure::AfterKill(_))
        ));
        drop(held);
    }

    #[test]
    fn holding_the_primary_against_a_holder_with_no_pid_fails_before_any_kill() {
        let temp = crate::testutil::TempDir::new("hold-primary-no-pid");
        let primary = temp.path().join("tray.lock");
        let held = crate::instance::try_acquire(&primary).expect("acquire").expect("free");
        let deadline = Instant::now() + Duration::from_millis(600);
        assert!(matches!(
            clear_and_hold_primary(&primary, deadline),
            Err(ClearFailure::BeforeKill(_))
        ));
        drop(held);
    }

    #[test]
    fn verification_sees_a_replacement_that_takes_the_lock() {
        let temp = crate::testutil::TempDir::new("verify-up");
        let path = temp.path().join("tray.lock");
        let handle = {
            let path = path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                // Retry rather than a single shot: verify_replacement's own probe
                // holds LOCK_EX for an instant, and colliding with it must not
                // fail the test.
                crate::instance::acquire_with_retry(&path, Duration::from_secs(5))
                    .expect("io ok")
                    .expect("the lock must be winnable")
            })
        };
        assert!(verify_replacement(&path, Duration::from_secs(10)));
        drop(handle.join().expect("join"));
    }

    #[test]
    fn verification_gives_up_when_nothing_ever_takes_the_lock() {
        let temp = crate::testutil::TempDir::new("verify-down");
        let path = temp.path().join("tray.lock");
        assert!(!verify_replacement(&path, Duration::from_millis(300)));
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
