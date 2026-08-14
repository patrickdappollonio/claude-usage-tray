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
//! # What is deliberately not here
//!
//! * **No `NSApplication` delegate of our own.** `tao` already owns one, and
//!   handing `tray-icon` a running loop is all the status item needs.
//! * **No appearance watcher.** See [`watch_appearance`]: monochrome icons are
//!   AppKit template images, which the system re-tints for the menu bar by
//!   itself.

pub mod autostart;
mod tray;

use crate::platform::{BackendError, Channel, Toast};
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

/// Posts a Notification Center notification.
///
/// [`Toast::urgency`] and [`Toast::transient`] are dropped: they are
/// freedesktop concepts with no Notification Center equivalent, and
/// `notify-rust` only compiles those builders on Linux. Failures (an
/// unbundled binary whose notifications the user has denied, most likely) are
/// ignored on purpose: a missing notification must never take the tray with
/// it.
///
/// `channel` is accepted and ignored: every toast is fire-and-forget here, so
/// a new threshold alert stacks instead of replacing the previous one, same
/// as before this lane existed on Linux. `notify-rust`'s macOS backend hands
/// back a handle too, so replace-in-place is possible in principle, but
/// wiring it up is deferred to the bundled-app work — an unsigned,
/// unbundled binary's Notification Center entitlements are already shaky
/// (see the doc comment above), and that work is where this gets revisited
/// alongside a real app identity.
pub fn notify(toast: &Toast, _channel: Channel) {
    // Deliberately NOT notify-rust here: its macOS backend
    // (mac-notification-sys) masquerades under another app's bundle identity
    // via `get_bundle_identifier_or_default("use_default")`, and on current
    // macOS that lookup can fail into an "choose an application" picker for a
    // literal app called `use_default` (observed on a real Mac). osascript's
    // `display notification` is the unbundled-binary-safe path: no identity
    // games, no dialogs. Loses urgency/transient nuance, which is acceptable
    // until the bundled-app work gives us UNUserNotificationCenter.
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
