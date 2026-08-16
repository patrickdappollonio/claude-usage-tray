//! A platform-neutral description of the tray menu.
//!
//! The menu used to be built directly out of `ksni` types, which tied every row
//! — including the ones that are pure product decisions, like "the `Install
//! hook` item only appears while there is no data" — to the StatusNotifierItem
//! backend. Here it is plain data instead: [`crate::ui::TrayCore::menu_with`]
//! builds a `Vec<MenuRow>` from the snapshot and the settings, each platform
//! backend maps those rows onto its own menu API, and the mapping stays thin
//! enough to read in one screen.
//!
//! Rows carry *semantic* action IDs rather than closures. A backend hands the
//! ID back to [`crate::ui::TrayCore::activate`] (or, for radio groups, to
//! [`crate::ui::TrayCore::select`]) and the core does the work — so the
//! behaviour behind a menu entry is written once, portably, and is unit-testable
//! without a desktop.

/// One thing a menu entry can ask the core to do.
///
/// Deliberately an enum of intents, not of implementations: `ToggleThreshold`
/// says what the user asked for, and the core decides what that means for the
/// config file, the shared notification preferences, and the poll loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuAction {
    /// Install the statusline hook (offered only while there is no data).
    InstallHook,
    /// Open the release page of the update the checker found.
    OpenUrl(String),
    /// Restart into the new binary that has appeared on disk (offered instead
    /// of [`MenuAction::OpenUrl`] when the upgrade is already installed).
    RestartToUpdate,
    /// "Check for new data".
    Refresh,
    /// Quit the tray.
    Quit,
    /// Flip the autostart entry.
    ToggleLaunchAtLogin,
    /// Flip one session-usage notification threshold.
    ToggleThreshold(u8),
    /// Flip the quota-reset notification.
    ToggleNotifyOnReset,
    /// Flip the daily update check.
    ToggleCheckUpdates,
    ToggleCliRefresh,
}

/// Which radio group a selection belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RadioGroup {
    /// The poll interval, indexing [`crate::config::REFRESH_CHOICES`].
    RefreshInterval,
    /// The icon style, indexing [`crate::config::IconStyle::ALL`].
    IconStyle,
}

/// One option inside a [`MenuRow::Radio`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RadioOption {
    pub label: String,
    pub enabled: bool,
}

/// A row of the menu.
///
/// `enabled: false` is used for two different things on purpose, exactly as the
/// old ksni menu did: a grayed *label* (an [`MenuRow::Info`] row, which has no
/// action at all) and a genuinely unavailable control (a checkbox whose config
/// file cannot be written).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuRow {
    /// A read-only label: the usage lines, the freshness line, the section
    /// headings inside `Settings`.
    Info { label: String },
    /// A clickable item.
    Action { label: String, action: MenuAction },
    /// A checkbox.
    Check {
        label: String,
        action: MenuAction,
        checked: bool,
        enabled: bool,
    },
    /// A radio group: exactly one of `options` is selected.
    Radio {
        group: RadioGroup,
        selected: usize,
        options: Vec<RadioOption>,
    },
    /// A nested menu.
    SubMenu { label: String, rows: Vec<MenuRow> },
    /// A divider.
    Separator,
}

impl MenuRow {
    /// Convenience constructor for the grayed label rows.
    pub fn info(label: impl Into<String>) -> Self {
        MenuRow::Info {
            label: label.into(),
        }
    }

    /// Convenience constructor for a plain clickable item.
    pub fn action(label: impl Into<String>, action: MenuAction) -> Self {
        MenuRow::Action {
            label: label.into(),
            action,
        }
    }
}
