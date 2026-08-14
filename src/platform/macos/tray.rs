//! The status item and its menu: a thin adapter over [`TrayCore`], and the
//! macOS counterpart of `platform/linux/tray.rs`.
//!
//! Nothing here decides anything. The core produces a [`MenuRow`] tree and a
//! list of [`IconImage`](crate::icon::IconImage)s; this module translates them
//! into `tray-icon`/`muda` types and routes clicks back into the core by
//! [`MenuAction`](crate::menu::MenuAction). Every method must run on the main
//! thread — see [`super::run`] for how the poll loop gets there.
//!
//! # Two differences from the Linux adapter, both forced by the API
//!
//! * **`muda` has no radio group.** A [`MenuRow::Radio`] becomes a run of
//!   [`CheckMenuItem`]s whose exclusivity this module maintains by rebuilding
//!   the menu after every click. AppKit does the same thing internally, and it
//!   is what the platform's own preference menus look like.
//! * **`muda` has no "menu is about to open" hook**, which is what Linux uses
//!   to refresh the freshness line while the menu is open. The menu is instead
//!   rebuilt on every snapshot push and every refresh, so its text is at most
//!   one poll interval old — the same age as the icon.

use crate::icon::IconAppearance;
use crate::menu::{MenuAction, MenuRow, RadioGroup};
use crate::source::UsageSnapshot;
use crate::ui::TrayCore;
use std::collections::HashMap;
use tray_icon::menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// The `TrayIconId` the status item is built with. Only ever compared against
/// the id on incoming [`tray_icon::TrayIconEvent`]s, of which there is exactly
/// one source, but naming it keeps the events self-describing in a debugger.
const TRAY_ID: &str = "claude-usage-tray";

/// What a menu id means. Rebuilt with the menu, because the ids are.
#[derive(Clone, Debug)]
enum Dispatch {
    /// Hand this action to [`TrayCore::activate`].
    Activate(MenuAction),
    /// Hand this group and index to [`TrayCore::select`].
    Select(RadioGroup, usize),
}

/// Both [`Menu`] and [`Submenu`] grow by `append`, but `muda` gives them no
/// shared trait for it, so the recursive row walk gets one of its own.
trait Append {
    fn add(&self, item: &dyn IsMenuItem);
}

impl Append for Menu {
    fn add(&self, item: &dyn IsMenuItem) {
        // An append can only fail on Windows (where it can hit a native menu
        // limit); a dropped row is not worth taking the tray down for.
        let _ = self.append(item);
    }
}

impl Append for Submenu {
    fn add(&self, item: &dyn IsMenuItem) {
        let _ = self.append(item);
    }
}

/// The status item, the menu currently attached to it, and the core they both
/// render.
pub struct MacTray {
    pub core: TrayCore,
    /// `None` until the event loop is running and [`MacTray::create`] has been
    /// called; see [`super::run`].
    tray: Option<TrayIcon>,
    /// The live menu, kept alive rather than dropped after it is installed:
    /// `muda` items route their clicks through Objective-C targets owned by
    /// the Rust-side menu tree, so dropping it would leave the `NSMenuItem`s
    /// pointing at nothing.
    menu: Option<Menu>,
    /// Menu id to meaning, for the menu currently installed.
    actions: HashMap<String, Dispatch>,
    /// Source of the menu ids. Monotonic across rebuilds so that a click on a
    /// menu that was open while a rebuild happened cannot be misread as a
    /// click on whatever took its place.
    next_id: u64,
}

impl MacTray {
    pub fn new(core: TrayCore) -> Self {
        MacTray {
            core,
            tray: None,
            menu: None,
            actions: HashMap::new(),
            next_id: 0,
        }
    }

    /// Creates the status item. Must be called with the event loop already
    /// running (see [`super::run`]).
    pub fn create(&mut self) -> tray_icon::Result<()> {
        let menu = self.build_menu();
        let (icon, is_template) = self.icon_image();

        let mut builder = TrayIconBuilder::new()
            .with_id(TRAY_ID)
            .with_menu(Box::new(menu.clone()))
            .with_tooltip(self.core.tooltip())
            .with_icon_as_template(is_template)
            // Left click is a status readout and right click opens the menu,
            // which is what the Linux tray does (`Activate` versus
            // `ContextMenu` in StatusNotifierItem) and therefore what the
            // product is documented to do. Leaving the menu on left click as
            // well would fire both, so every menu opening would also raise a
            // notification.
            .with_menu_on_left_click(false)
            .with_menu_on_right_click(true);
        if let Some(icon) = icon {
            builder = builder.with_icon(icon);
        }

        self.tray = Some(builder.build()?);
        self.menu = Some(menu);
        Ok(())
    }

    /// Publishes a new snapshot: new icon, new tooltip, new menu labels.
    pub fn set_snapshot(&mut self, snapshot: UsageSnapshot) {
        self.core.snapshot = snapshot;
        self.refresh();
    }

    /// Re-publishes without changing the snapshot, for the things the tray
    /// reads out of shared state on every render: the resolved icon appearance
    /// and the update-available row.
    pub fn refresh(&mut self) {
        let (icon, is_template) = self.icon_image();
        let tooltip = self.core.tooltip();
        if let Some(tray) = &self.tray {
            // `set_icon` on its own clears the template flag, so the two are
            // always set together.
            let _ = tray.set_icon_with_as_template(icon, is_template);
            let _ = tray.set_tooltip(Some(tooltip));
        }
        self.rebuild_menu();
    }

    /// Routes a menu click back into the core, then rebuilds: a click changes
    /// what the menu should say next time (a checkbox that flipped, a radio
    /// group whose selection moved, an autostart toggle the filesystem
    /// refused), and `muda` has already drawn its own guess of the new state.
    pub fn on_menu_event(&mut self, id: &str) {
        let Some(dispatch) = self.actions.get(id).cloned() else {
            // An id from a menu that has since been replaced. The core state it
            // referred to is gone; doing nothing is the only safe reading.
            return;
        };
        match dispatch {
            Dispatch::Activate(action) => self.core.activate(&action),
            Dispatch::Select(group, index) => self.core.select(group, index),
        }
        self.rebuild_menu();
    }

    /// Left click: show a worded summary of current usage.
    pub fn on_left_click(&self) {
        self.core.clicked();
    }

    /// Builds a fresh menu and hands it to the status item, dropping the old
    /// one only once the new one is installed.
    fn rebuild_menu(&mut self) {
        let menu = self.build_menu();
        if let Some(tray) = &self.tray {
            tray.set_menu(Some(Box::new(menu.clone())));
        }
        self.menu = Some(menu);
    }

    /// Translates the core's rows into a `muda` menu, recording what each
    /// generated id means.
    fn build_menu(&mut self) -> Menu {
        self.actions.clear();
        let rows = self.core.menu();
        let menu = Menu::new();
        self.append_rows(&menu, rows);
        menu
    }

    fn append_rows(&mut self, into: &dyn Append, rows: Vec<MenuRow>) {
        for row in rows {
            match row {
                MenuRow::Separator => into.add(&PredefinedMenuItem::separator()),
                // A label, not a control: `muda` has no dedicated kind, so a
                // disabled item is how an info row is spelled — the same
                // spelling the Linux adapter uses.
                MenuRow::Info { label } => into.add(&MenuItem::new(label, false, None)),
                MenuRow::Action { label, action } => {
                    let id = self.register(Dispatch::Activate(action));
                    into.add(&MenuItem::with_id(id, label, true, None));
                }
                MenuRow::Check {
                    label,
                    action,
                    checked,
                    enabled,
                } => {
                    let id = self.register(Dispatch::Activate(action));
                    into.add(&CheckMenuItem::with_id(id, label, enabled, checked, None));
                }
                MenuRow::Radio {
                    group,
                    selected,
                    options,
                } => {
                    for (index, option) in options.into_iter().enumerate() {
                        let id = self.register(Dispatch::Select(group, index));
                        into.add(&CheckMenuItem::with_id(
                            id,
                            option.label,
                            option.enabled,
                            index == selected,
                            None,
                        ));
                    }
                }
                MenuRow::SubMenu { label, rows } => {
                    let submenu = Submenu::new(label, true);
                    self.append_rows(&submenu, rows);
                    into.add(&submenu);
                }
            }
        }
    }

    /// Allocates the next menu id for `dispatch`.
    fn register(&mut self, dispatch: Dispatch) -> String {
        self.next_id += 1;
        let id = format!("cut-{}", self.next_id);
        self.actions.insert(id.clone(), dispatch);
        id
    }

    /// The icon to show, and whether it is an AppKit *template* image.
    ///
    /// Template images are drawn by the system from their alpha channel alone,
    /// which is exactly right for a monochrome icon (macOS then tints it for
    /// the light or dark menu bar, live, with no theme watching of our own —
    /// see [`super::watch_appearance`]) and exactly wrong for the color gauge,
    /// whose whole signal *is* the color. So the user's own icon-style choice
    /// decides: monochrome becomes a template drawn in the dark foreground,
    /// color stays a plain colored image.
    ///
    /// The 48 px render is the one handed over, of the three the core produces:
    /// `tray-icon` scales whatever it is given to 18 pt, so the largest is the
    /// one that survives a Retina menu bar.
    fn icon_image(&self) -> (Option<Icon>, bool) {
        let is_template = matches!(self.core.appearance(), IconAppearance::Mono { .. });
        let rendered = if is_template {
            crate::icon::render_icons(&self.core.snapshot, IconAppearance::Mono { dark_ui: false })
        } else {
            self.core.icons()
        };
        let icon = rendered
            .into_iter()
            .max_by_key(|image| image.width)
            .and_then(|image| {
                Icon::from_rgba(image.to_rgba(), image.width as u32, image.height as u32).ok()
            });
        (icon, is_template)
    }
}

/// `RadioGroup` is `Copy`, which is what lets one dispatch entry serve a whole
/// group; asserted here so a future non-`Copy` variant fails loudly rather than
/// silently forcing a clone into the hot path. Mirrors the same assertion in
/// the Linux adapter.
const _: fn() = || {
    fn assert_copy<T: Copy + Send + 'static>() {}
    assert_copy::<RadioGroup>();
};
