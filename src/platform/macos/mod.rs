//! The macOS backend: an `NSStatusItem` in the menu bar (`tray-icon`), its
//! `NSMenu` (`muda`, which `tray-icon` re-exports as `tray_icon::menu`), a
//! LaunchAgent for autostart, and Notification Center toasts (`notify-rust`).
//!
//! # Which thread everything runs on
//!
//! This is the inversion the contract in [`crate::platform`] exists for.
//! AppKit owns the main thread: `tray-icon` documents that "an event loop must
//! be running on the main thread so you also need to create the tray icon on
//! the main thread", and that the icon must be created once the loop is
//! *already running* rather than merely built. So [`run`] keeps the thread it
//! was called on for the event loop, creates the status item on the first
//! `StartCause::Init`, and spawns the poll loop onto a worker thread from
//! there. The Linux backend does the opposite, and nothing above
//! [`crate::platform`] can tell the difference.
//!
//! Everything that touches the status item therefore has to get back to the
//! main thread first. [`TrayHandle`] does that by posting a [`UserEvent`]
//! through the event loop's proxy — the same route `tray-icon`'s own
//! `tao`/`winit` examples use for tray and menu events — and the loop applies
//! it. Nothing but the proxy crosses the thread boundary, which is what makes
//! the `muda` menu (which is `Rc`-based and main-thread-only) safe to keep.
//!
//! The structure follows `tray-icon`'s `examples/tao.rs` closely, including
//! the `CFRunLoop::wake_up` after creating the icon, which their example needs
//! to make the icon appear at all.
//!
//! # Two kinds of process
//!
//! The same binary runs either as a bare executable or as the executable
//! inside `Claude Usage Tray.app` (see `scripts/make-app-bundle.sh`), and
//! [`notify`] is the one place that can tell the difference and has to. Only a
//! bundle has a `CFBundleIdentifier`, and `UNUserNotificationCenter` refuses
//! to work — crashes, in fact — without one. Everything else here behaves
//! identically in both, including the LaunchAgent, which simply records
//! whichever path this copy was launched from.
//!
//! # What is deliberately not here
//!
//! * **No `NSApplication` delegate of our own.** `tao` already owns one, and
//!   handing `tray-icon` a running loop is all the status item needs.
//! * **No appearance watcher.** See [`watch_appearance`]: monochrome icons are
//!   AppKit template images, which the system re-tints for the menu bar by
//!   itself.
//! * **No `SMAppService` registration.** "Launch at login" stays a LaunchAgent
//!   on both shapes of install, so there is one implementation and one
//!   checkbox rather than a bundled path and an unbundled one that behave
//!   differently. The cost is that macOS lists it under "Allow in the
//!   Background" instead of "Open at Login".

pub mod autostart;
mod tray;

use crate::platform::{BackendError, Channel, Toast, Urgency};
use crate::source::UsageSnapshot;
use crate::ui::TrayCore;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::menu::MenuEvent;
use tray_icon::TrayIconEvent;

/// Everything the event loop can be asked to do, from wherever it is asked.
///
/// `tray-icon` and `muda` deliver their events through global handlers that
/// may fire on any thread, and the poll loop lives on a worker thread, so both
/// arrive the same way: as one of these, handled on the main thread.
enum UserEvent {
    /// A click on the status item.
    Tray(TrayIconEvent),
    /// A click on a menu row.
    Menu(MenuEvent),
    /// The poll loop read a new snapshot.
    Snapshot(Box<UsageSnapshot>),
    /// The poll loop wants a repaint from shared state.
    Refresh,
    /// The poll loop returned, so the program is over.
    PollFinished,
}

/// The poll loop's remote control over the running tray: a proxy back to the
/// main thread's event loop. Every method is a message, never a direct AppKit
/// call, because this type only ever exists on the poll-loop thread.
pub struct TrayHandle {
    proxy: EventLoopProxy<UserEvent>,
    /// Set once the event loop has stopped accepting events, which is the only
    /// way this side finds out that the tray is gone.
    closed: Arc<AtomicBool>,
}

impl TrayHandle {
    fn send(&self, event: UserEvent) {
        if self.proxy.send_event(event).is_err() {
            self.closed.store(true, Ordering::Relaxed);
        }
    }

    /// Publishes a new snapshot: new icon, new tooltip, new menu labels.
    pub fn set_snapshot(&self, snapshot: UsageSnapshot) {
        self.send(UserEvent::Snapshot(Box::new(snapshot)));
    }

    /// Re-publishes without changing the snapshot, for the things the tray
    /// reads out of shared state on every render: the resolved icon appearance
    /// and the update-available row.
    pub fn refresh(&self) {
        self.send(UserEvent::Refresh);
    }

    /// Whether the event loop is gone and the poll loop should stop.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

/// Runs the event loop on this thread, with the poll loop on a worker thread.
///
/// Never returns: `tao`'s `run` diverges, exiting the process when the loop
/// stops. That is also why a status item that fails to build reports itself
/// here rather than through the [`BackendError`] in the signature — by the
/// time it can be built, `run`'s caller is no longer reachable. The message is
/// worded and prefixed exactly as `main.rs` would have printed it.
pub fn run<F>(core: TrayCore, poll: F) -> Result<(), BackendError>
where
    F: FnOnce(TrayHandle) + Send + 'static,
{
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    // A menu bar app, not an application: no Dock icon, no app menu, and no
    // stealing focus from whatever the user is doing when the tray starts.
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    event_loop.set_dock_visibility(false);
    event_loop.set_activate_ignoring_other_apps(false);

    // Forward the two global event sources into the loop, so that everything
    // is handled in one place on the main thread.
    let proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = proxy.send_event(UserEvent::Tray(event));
    }));
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = proxy.send_event(UserEvent::Menu(event));
    }));

    let proxy = event_loop.create_proxy();
    let closed = Arc::new(AtomicBool::new(false));
    let mut tray = tray::MacTray::new(core);
    // Taken on the first `Init`, which is the only place it can be started:
    // the poll loop must not run before there is a status item to push to.
    let mut poll = Some(poll);

    event_loop.run(move |event, _target, control_flow| {
        // Nothing here polls: every wake is an event.
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {
                // Created here, with the loop already running, rather than
                // before it: creating it earlier breaks over fullscreen apps
                // (tauri-apps/tray-icon#90), which is why the crate's own
                // examples do exactly this.
                if let Err(err) = tray.create() {
                    eprintln!("claude-usage-tray: could not create the menu bar icon: {err}");
                    std::process::exit(1);
                }
                // The icon does not actually appear until the run loop turns
                // over once more; `tray-icon`'s tao example does the same.
                if let Some(run_loop) = objc2_core_foundation::CFRunLoop::main() {
                    objc2_core_foundation::CFRunLoop::wake_up(&run_loop);
                }

                if let Some(poll) = poll.take() {
                    let handle = TrayHandle {
                        proxy: proxy.clone(),
                        closed: Arc::clone(&closed),
                    };
                    let done = proxy.clone();
                    std::thread::spawn(move || {
                        poll(handle);
                        // The poll loop only returns when the program is over
                        // (the `Quit` row, or a dead channel), so this is what
                        // stops the event loop.
                        let _ = done.send_event(UserEvent::PollFinished);
                    });
                }
            }

            // Clicks open the menu, handled entirely by AppKit via
            // `with_menu_on_left_click(true)`; nothing to do here. The worded
            // usage summary that Linux toasts on left click is redundant on
            // macOS because the opened menu leads with the same info rows.
            Event::UserEvent(UserEvent::Tray(TrayIconEvent::Click { .. })) => {}

            Event::UserEvent(UserEvent::Menu(event)) => tray.on_menu_event(event.id.as_ref()),
            Event::UserEvent(UserEvent::Snapshot(snapshot)) => tray.set_snapshot(*snapshot),
            Event::UserEvent(UserEvent::Refresh) => tray.refresh(),
            Event::UserEvent(UserEvent::PollFinished) => *control_flow = ControlFlow::Exit,

            _ => {}
        }
    })
}

/// The `UNNotificationRequest` identifier every threshold alert reuses.
///
/// Re-posting a request under an identifier that is already delivered replaces
/// it in place, which is exactly what [`Channel::ThresholdAlert`] asks for: the
/// 90% banner reads where the 75% one was instead of stacking under it.
/// Ephemeral toasts pass no identifier at all and get a fresh system-assigned
/// one each time, so they stack.
const THRESHOLD_ALERT_ID: &str = "com.patrickdappollonio.claude-usage-tray.threshold-alert";

/// Whether this process has an app bundle identity, worked out once.
///
/// `notify_rust::check_bundle` is `NSBundle::mainBundle().bundleIdentifier()`
/// and nothing else, which is the reliable form of the question:
/// `mainBundle` itself is *not* a bundle test (Apple documents that it "may
/// return a valid bundle object even for unbundled apps", rooted at whatever
/// directory the executable sits in), but only a real `Info.plist` supplies a
/// `CFBundleIdentifier`. Matching on `".app/Contents/MacOS/"` in the
/// executable path would have guessed at the same thing from the outside.
///
/// This is also a hard gate, not a preference: `UNUserNotificationCenter`
/// *crashes* a process with no bundle identifier, so the check has to come
/// before anything touches the framework.
fn is_bundled() -> bool {
    static BUNDLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *BUNDLED.get_or_init(|| notify_rust::check_bundle().is_ok())
}

/// Asks for notification permission once per process, returning whether it was
/// granted.
///
/// `mac-usernotifications` never requests authorization on its own — every
/// send path only calls `check_bundle` — so this is ours to do. The first call
/// puts up the system's "Claude Usage Tray would like to send you
/// notifications" prompt; every later one reads the answer cached here. It
/// blocks, but on the notification framework's own dispatch queue rather than
/// the main run loop, and it is called from the poll-loop thread.
///
/// A denial is honored rather than worked around: no fallback, no second
/// route. The user said no.
fn notifications_authorized() -> bool {
    static AUTHORIZED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AUTHORIZED.get_or_init(|| notify_rust::request_auth_blocking().unwrap_or(false))
}

/// Posts a Notification Center notification.
///
/// Which route it takes depends on what this process *is*:
///
/// * **Inside the app bundle** (the `.app` that `scripts/make-app-bundle.sh`
///   assembles) it goes through `UNUserNotificationCenter`, via notify-rust's
///   `preview-macos-un` backend. That is the modern framework, and the one
///   that gives the banner the app's own name, a permission prompt the user
///   can answer once, an entry in System Settings > Notifications, and
///   scroll-back in Notification Center.
/// * **As a bare binary** (Homebrew, npm, the tarball) there is no app
///   identity to post under, so it falls back to `osascript`'s `display
///   notification`. That is best effort in the honest sense: it costs one
///   short-lived process and on current macOS it frequently shows nothing at
///   all (measured on a real Mac, from a terminal, silently). It stays because
///   it is free and it still works on some setups; it is not something to
///   promise anybody.
///
/// [`Toast::transient`] is dropped either way: Notification Center has no
/// "don't keep this in history" concept. Failures are ignored on purpose — a
/// missing notification must never take the tray with it.
pub fn notify(toast: &Toast, channel: Channel) {
    if is_bundled() {
        notify_bundled(toast, channel);
    } else {
        notify_unbundled(toast);
    }
}

/// How many times a bundled notification is attempted, and how long to wait
/// between attempts. See [`notify_bundled`] for what is being waited for.
const SEND_ATTEMPTS: u32 = 5;
const SEND_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// The `UNUserNotificationCenter` path. Only ever called once [`is_bundled`]
/// has said yes.
///
/// # Why this retries
///
/// `mac-usernotifications` blocks on its send future through
/// `block_on_current`, which refuses to block a non-main thread unless the
/// main run loop is *waiting* (`CFRunLoop::main().is_waiting()`), and returns
/// `MainThreadNotRunning` without sending anything if it is not. Every toast
/// here comes off the poll-loop thread, and several of them are emitted
/// immediately after that same thread pushed a snapshot through the event loop
/// proxy — which is precisely what wakes the main thread up. So the one moment
/// a toast is most likely to be sent is also the one moment the probe is most
/// likely to say "busy".
///
/// The window is a few milliseconds of icon rendering, so a handful of spaced
/// attempts covers it, and a genuinely refused notification (authorization
/// revoked, say) simply fails five cheap times instead of one. Nothing is
/// double-posted by retrying: `block_on_current` bails out before the send
/// future is ever polled, and a threshold alert re-sent under the same
/// identifier replaces itself in any case.
fn notify_bundled(toast: &Toast, channel: Channel) {
    if !notifications_authorized() {
        return;
    }
    for attempt in 0..SEND_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(SEND_RETRY_DELAY);
        }
        let mut notification = notify_rust::Notification::new();
        notification
            .summary(&toast.summary)
            .body(&toast.body)
            .interruption_level(interruption_level(toast.urgency));
        if channel == Channel::ThresholdAlert {
            notification.id(THRESHOLD_ALERT_ID);
        }
        if notification.show().is_ok() {
            return;
        }
    }
}

/// Maps the portable urgency onto an interruption level.
///
/// `Critical` deliberately stops at `Active` rather than `TimeSensitive`, even
/// though notify-rust's own `Urgency` conversion goes there: Apple gates the
/// time-sensitive level behind the Time Sensitive Notifications capability
/// (enabled in Xcode, backed by a provisioning profile), and this bundle is
/// signed ad hoc with no entitlements at all. Asking for a level we are not
/// entitled to risks the *most* important alert being the one that does not
/// arrive, which is a bad trade for a banner that breaks through Focus.
fn interruption_level(urgency: Urgency) -> notify_rust::InterruptionLevel {
    match urgency {
        Urgency::Low => notify_rust::InterruptionLevel::Passive,
        Urgency::Normal | Urgency::Critical => notify_rust::InterruptionLevel::Active,
    }
}

/// The unbundled fallback: `osascript`'s `display notification`.
///
/// Spawned and never waited on, with all three standard streams on
/// `/dev/null`, so a missing or refusing `osascript` costs nothing.
fn notify_unbundled(toast: &Toast) {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        applescript_escape(&toast.body),
        applescript_escape(&toast.summary),
    );
    let _ = std::process::Command::new("osascript")
        .args(["-e", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Escapes a string for inclusion inside a double-quoted AppleScript literal.
/// AppleScript's only escapes in double-quoted strings are `\"` and `\\`;
/// newlines are legal inside the quotes as-is.
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Does nothing, on purpose.
///
/// On Linux this watches the desktop portal so a monochrome icon can be
/// re-rendered when the user switches theme. macOS needs no such thing: a
/// monochrome icon is published as an AppKit *template* image (see
/// `tray::MacTray::icon_image`), which the system tints for the menu bar
/// itself, in both appearances, without the icon being re-rendered at all.
/// Never calling `on_change` leaves the dark-assuming default standing, and
/// nothing reads it.
pub fn watch_appearance<F>(_on_change: F)
where
    F: Fn(bool) + Send + 'static,
{
}
