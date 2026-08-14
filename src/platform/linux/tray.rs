//! The `ksni::Tray` implementation: a thin adapter over [`TrayCore`].
//!
//! Nothing here decides anything. The core produces a [`MenuRow`] tree and a
//! list of [`IconImage`](crate::icon::IconImage)s; this module translates them
//! into `ksni` types and routes clicks back into the core by
//! [`MenuAction`](crate::menu::MenuAction). Keeping the translation this
//! mechanical is the point: the same core drives a different menu API on
//! another platform without any of the wording, gating or ordering being
//! restated there.

use crate::menu::{MenuRow, RadioGroup};
use crate::ui::TrayCore;

/// The tray object `ksni` owns on its service thread.
pub struct LinuxTray {
    pub core: TrayCore,
}

impl LinuxTray {
    pub fn new(core: TrayCore) -> Self {
        LinuxTray { core }
    }
}

/// Translates one portable row into a `ksni` menu item.
fn menu_item(row: MenuRow) -> ksni::MenuItem<LinuxTray> {
    match row {
        MenuRow::Separator => ksni::MenuItem::Separator,
        // A label, not a control: `ksni` has no dedicated kind, so a disabled
        // standard item is how an info row is spelled.
        MenuRow::Info { label } => ksni::MenuItem::Standard(ksni::menu::StandardItem {
            label,
            enabled: false,
            ..Default::default()
        }),
        MenuRow::Action { label, action } => ksni::menu::StandardItem {
            label,
            activate: Box::new(move |tray: &mut LinuxTray| tray.core.activate(&action)),
            ..Default::default()
        }
        .into(),
        MenuRow::Check {
            label,
            action,
            checked,
            enabled,
        } => ksni::menu::CheckmarkItem {
            label,
            enabled,
            checked,
            activate: Box::new(move |tray: &mut LinuxTray| tray.core.activate(&action)),
            ..Default::default()
        }
        .into(),
        MenuRow::Radio {
            group,
            selected,
            options,
        } => ksni::menu::RadioGroup {
            selected,
            select: Box::new(move |tray: &mut LinuxTray, index: usize| {
                tray.core.select(group, index)
            }),
            options: options
                .into_iter()
                .map(|option| ksni::menu::RadioItem {
                    label: option.label,
                    enabled: option.enabled,
                    ..Default::default()
                })
                .collect(),
        }
        .into(),
        MenuRow::SubMenu { label, rows } => ksni::menu::SubMenu {
            label,
            submenu: menu_items(rows),
            ..Default::default()
        }
        .into(),
    }
}

fn menu_items(rows: Vec<MenuRow>) -> Vec<ksni::MenuItem<LinuxTray>> {
    rows.into_iter().map(menu_item).collect()
}

/// `RadioGroup` is `Copy`, which is what lets one `select` closure serve the
/// whole group; asserted here so a future non-`Copy` variant fails loudly
/// rather than silently forcing a clone into the hot path.
const _: fn() = || {
    fn assert_copy<T: Copy + Send + 'static>() {}
    assert_copy::<RadioGroup>();
};

impl ksni::Tray for LinuxTray {
    fn id(&self) -> String {
        "claude-usage-tray".into()
    }

    fn title(&self) -> String {
        "Claude usage".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn status(&self) -> ksni::Status {
        ksni::Status::Active
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.core
            .icons()
            .into_iter()
            .map(|icon| ksni::Icon {
                width: icon.width,
                height: icon.height,
                data: icon.data,
            })
            .collect()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Claude usage".into(),
            description: self.core.tooltip(),
            ..Default::default()
        }
    }

    /// Left-click: show a worded summary of current usage. Unlike "Check for
    /// new data" this never reports whether the cache moved.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.core.clicked();
    }

    /// Overriding this (even as a no-op) opts out of ksni's `NO_ABOUT_TO_SHOW`
    /// default, which otherwise skips the update_properties/update_menu pass
    /// before the menu opens. Without this override, rows like "Updated N min
    /// ago" only refresh when the poll loop happens to push a changed
    /// snapshot, so the menu can show stale text while open.
    fn menu_about_to_show(&mut self) {}

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        menu_items(self.core.menu())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::MenuAction;

    /// The adapter must not drop or reorder rows: a missing row would take a
    /// feature with it, and the core's tests only cover the model.
    #[test]
    fn every_row_kind_maps_to_the_matching_ksni_item() {
        let rows = vec![
            MenuRow::info("label"),
            MenuRow::action("do it", MenuAction::Refresh),
            MenuRow::Check {
                label: "check".into(),
                action: MenuAction::ToggleNotifyOnReset,
                checked: true,
                enabled: false,
            },
            MenuRow::Radio {
                group: RadioGroup::IconStyle,
                selected: 1,
                options: vec![crate::menu::RadioOption {
                    label: "one".into(),
                    enabled: true,
                }],
            },
            MenuRow::Separator,
            MenuRow::SubMenu {
                label: "more".into(),
                rows: vec![MenuRow::Separator],
            },
        ];
        let items = menu_items(rows);
        assert_eq!(items.len(), 6);
        match &items[0] {
            ksni::MenuItem::Standard(item) => {
                assert_eq!(item.label, "label");
                assert!(!item.enabled, "info rows are grayed labels");
            }
            _ => panic!("expected a standard item"),
        }
        match &items[1] {
            ksni::MenuItem::Standard(item) => {
                assert_eq!(item.label, "do it");
                assert!(item.enabled, "action rows are clickable");
            }
            _ => panic!("expected a standard item"),
        }
        match &items[2] {
            ksni::MenuItem::Checkmark(item) => {
                assert_eq!(item.label, "check");
                assert!(item.checked);
                assert!(!item.enabled);
            }
            _ => panic!("expected a checkmark item"),
        }
        match &items[3] {
            ksni::MenuItem::RadioGroup(group) => {
                assert_eq!(group.selected, 1);
                assert_eq!(group.options.len(), 1);
                assert_eq!(group.options[0].label, "one");
            }
            _ => panic!("expected a radio group"),
        }
        assert!(matches!(items[4], ksni::MenuItem::Separator));
        match &items[5] {
            ksni::MenuItem::SubMenu(item) => {
                assert_eq!(item.label, "more");
                assert_eq!(item.submenu.len(), 1);
            }
            _ => panic!("expected a submenu"),
        }
    }
}
