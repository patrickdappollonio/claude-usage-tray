//! Persisted user settings for the tray: the poll interval and the
//! launch-at-login flag.
//!
//! The file lives at `~/.config/claude-usage-tray/config.toml` (respecting
//! `$XDG_CONFIG_HOME`) and holds four keys:
//!
//! ```toml
//! refresh_secs = 5
//! launch_at_login = false
//! notify_thresholds = [50, 75, 90, 99, 100]
//! notify_on_reset = true
//! ```
//!
//! Like every other read path in this crate, nothing here panics on bad input:
//! a missing, unreadable, or corrupt file loads as [`Config::default`]. Every
//! function that touches the filesystem is parameterized by a path so the tests
//! can run entirely inside a temp directory.
//!
//! See `docs/superpowers/specs/2026-08-13-claude-usage-tray-design.md`.

use std::io;
use std::path::{Path, PathBuf};

/// Poll interval used when no config file exists (and when the stored value is
/// unusable).
pub const DEFAULT_REFRESH_SECS: u64 = 5;

/// The intervals offered by the `Refresh interval` radio group, in seconds.
pub const REFRESH_CHOICES: [u64; 4] = [5, 15, 30, 60];

/// Every session-usage threshold the tray knows how to alert on, ascending.
/// Values outside this set are meaningless to the notifier, so they are
/// dropped on load rather than kept as dead weight in the config.
pub const NOTIFY_THRESHOLDS: [u8; 5] = [50, 75, 90, 99, 100];

/// Thresholds at and above this value notify at critical urgency; the lower
/// ones are informational.
pub const CRITICAL_FROM: u8 = 90;

/// True when a threshold should be delivered at critical urgency.
pub fn is_critical(threshold: u8) -> bool {
    threshold >= CRITICAL_FROM
}

/// User settings as stored in `config.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Seconds between cache re-reads.
    pub refresh_secs: u64,
    /// Whether an XDG autostart entry should exist for the tray.
    pub launch_at_login: bool,
    /// The enabled subset of [`NOTIFY_THRESHOLDS`], sorted and deduplicated.
    /// An empty list is a legitimate state: it means the user switched every
    /// threshold off.
    pub notify_thresholds: Vec<u8>,
    /// Whether the "session quota reset" notification fires.
    pub notify_on_reset: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            refresh_secs: DEFAULT_REFRESH_SECS,
            launch_at_login: false,
            notify_thresholds: NOTIFY_THRESHOLDS.to_vec(),
            notify_on_reset: true,
        }
    }
}

impl Config {
    /// Index of `refresh_secs` within [`REFRESH_CHOICES`], for the radio group.
    /// A stored value that isn't one of the offered options (hand-edited file)
    /// falls back to the default's index rather than leaving no option
    /// selected.
    pub fn refresh_choice(&self) -> usize {
        REFRESH_CHOICES
            .iter()
            .position(|&secs| secs == self.refresh_secs)
            .or_else(|| {
                REFRESH_CHOICES
                    .iter()
                    .position(|&secs| secs == DEFAULT_REFRESH_SECS)
            })
            .unwrap_or(0)
    }

    /// Whether alerts for `threshold` are switched on.
    pub fn notifies_at(&self, threshold: u8) -> bool {
        self.notify_thresholds.contains(&threshold)
    }

    /// Switches `threshold` on or off, keeping the list sorted and unique.
    /// Thresholds outside [`NOTIFY_THRESHOLDS`] are ignored.
    pub fn set_notifies_at(&mut self, threshold: u8, enabled: bool) {
        if !NOTIFY_THRESHOLDS.contains(&threshold) {
            return;
        }
        self.notify_thresholds.retain(|&t| t != threshold);
        if enabled {
            self.notify_thresholds.push(threshold);
            self.notify_thresholds.sort_unstable();
        }
    }
}

/// Keeps only known thresholds, deduplicated and sorted. Used both when
/// loading a hand-edited file and when accepting a list from anywhere else.
pub fn sanitize_thresholds(values: impl IntoIterator<Item = i64>) -> Vec<u8> {
    let mut kept: Vec<u8> = values
        .into_iter()
        .filter_map(|value| u8::try_from(value).ok())
        .filter(|threshold| NOTIFY_THRESHOLDS.contains(threshold))
        .collect();
    kept.sort_unstable();
    kept.dedup();
    kept
}

/// Default config file location: `$XDG_CONFIG_HOME/claude-usage-tray/config.toml`,
/// falling back to `~/.config/...` (this is what `dirs::config_dir` resolves).
pub fn default_config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Directory holding the config file.
fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("claude-usage-tray")
}

/// Tolerant TOML parser for the config contract. Unknown keys are ignored,
/// wrong-typed or absent keys fall back to the default, and a zero interval is
/// rejected (it would spin the poll loop).
pub fn parse_config(body: &str) -> Config {
    let defaults = Config::default();
    let Ok(table) = body.parse::<toml::Table>() else {
        return defaults;
    };
    let refresh_secs = table
        .get("refresh_secs")
        .and_then(|value| value.as_integer())
        .and_then(|secs| u64::try_from(secs).ok())
        .filter(|&secs| secs > 0)
        .unwrap_or(defaults.refresh_secs);
    let launch_at_login = table
        .get("launch_at_login")
        .and_then(|value| value.as_bool())
        .unwrap_or(defaults.launch_at_login);
    // A present-but-not-a-list value (or a missing key) means "we have no idea
    // what the user wanted" — defaults. A real list, on the other hand, is
    // taken at face value after filtering, including the empty one: that is
    // how "every threshold switched off" is stored.
    let notify_thresholds = match table
        .get("notify_thresholds")
        .and_then(|value| value.as_array())
    {
        Some(items) => sanitize_thresholds(items.iter().filter_map(|item| item.as_integer())),
        None => defaults.notify_thresholds.clone(),
    };
    let notify_on_reset = table
        .get("notify_on_reset")
        .and_then(|value| value.as_bool())
        .unwrap_or(defaults.notify_on_reset);
    Config {
        refresh_secs,
        launch_at_login,
        notify_thresholds,
        notify_on_reset,
    }
}

/// Renders a config back to TOML. Written by hand rather than through a
/// serializer: the schema is three scalars and a small integer list, and this
/// keeps the `toml` dependency to its parser half.
pub fn render_config(config: &Config) -> String {
    let thresholds = config
        .notify_thresholds
        .iter()
        .map(|threshold| threshold.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "refresh_secs = {}\nlaunch_at_login = {}\nnotify_thresholds = [{}]\nnotify_on_reset = {}\n",
        config.refresh_secs, config.launch_at_login, thresholds, config.notify_on_reset
    )
}

/// Loads the config at `path`. Any failure — file absent, unreadable, or not
/// valid TOML — yields the defaults, so a corrupt file degrades to "works with
/// stock settings" instead of failing to start.
pub fn load_from(path: &Path) -> Config {
    match std::fs::read_to_string(path) {
        Ok(body) => parse_config(&body),
        Err(_) => Config::default(),
    }
}

/// Writes the config to `path` atomically: a temp file in the same directory
/// followed by a rename, so a reader (or a crash mid-write) never observes a
/// truncated file. Creates the parent directory if needed.
pub fn save_to(path: &Path, config: &Config) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("toml.tmp");
    std::fs::write(&temp, render_config(config))?;
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            // Leaving the temp file behind would make the next save fail in
            // the same way for no extra information.
            let _ = std::fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// Cheap capability probe: can we create `dir` (if absent) and write a file
/// inside it? Used to gray out the menu entries whose only effect would be a
/// failed save.
///
/// Deliberately a real create + write rather than a permission-bit reading:
/// read-only mounts, full disks, immutable attributes and SELinux denials all
/// show up here and none of them show up in the mode bits. The probe file is
/// removed again; if even the removal fails, the directory is still writable,
/// which is the question being asked.
pub fn dir_is_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Whether the real config directory can be written to right now. Called on
/// every menu open, hence the "cheap" requirement above.
pub fn is_writable() -> bool {
    dir_is_writable(&config_dir())
}

/// Interprets the `CLAUDE_TRAY_POLL_SECS` value. `None` (unset, unparseable,
/// or zero) means "no override — use the configured interval".
///
/// The env var deliberately wins over the config file: it is the escape hatch
/// for running the tray with a different cadence without touching the user's
/// saved settings.
pub fn env_override(raw: Option<&str>) -> Option<u64> {
    raw.and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|&secs| secs > 0)
}

/// Loads the config from [`default_config_path`].
pub fn load() -> Config {
    load_from(&default_config_path())
}

/// Saves the config to [`default_config_path`], logging rather than
/// propagating I/O errors: a read-only home directory must not take the tray
/// down, and the in-memory setting still applies for this run.
pub fn save(config: &Config) {
    let path = default_config_path();
    if let Err(err) = save_to(&path, config) {
        eprintln!(
            "claude-usage-tray: could not write {}: {err}",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn default_is_five_seconds_no_autostart_and_all_notifications_on() {
        assert_eq!(
            Config::default(),
            Config {
                refresh_secs: 5,
                launch_at_login: false,
                notify_thresholds: vec![50, 75, 90, 99, 100],
                notify_on_reset: true,
            }
        );
    }

    #[test]
    fn parses_a_full_config() {
        let config = parse_config(
            "refresh_secs = 30\nlaunch_at_login = true\n\
             notify_thresholds = [75, 100]\nnotify_on_reset = false\n",
        );
        assert_eq!(
            config,
            Config {
                refresh_secs: 30,
                launch_at_login: true,
                notify_thresholds: vec![75, 100],
                notify_on_reset: false,
            }
        );
    }

    #[test]
    fn parses_a_partial_config_with_defaults_for_the_rest() {
        assert_eq!(
            parse_config("launch_at_login = true\n"),
            Config {
                launch_at_login: true,
                ..Config::default()
            }
        );
        assert_eq!(
            parse_config("refresh_secs = 60\n"),
            Config {
                refresh_secs: 60,
                ..Config::default()
            }
        );
    }

    #[test]
    fn ignores_unknown_keys() {
        assert_eq!(
            parse_config("colour = \"blue\"\nrefresh_secs = 15\n"),
            Config {
                refresh_secs: 15,
                ..Config::default()
            }
        );
    }

    #[test]
    fn parses_an_empty_threshold_list_as_everything_off() {
        // Not a fallback case: this is how "no threshold alerts" is stored.
        assert_eq!(
            parse_config("notify_thresholds = []\n").notify_thresholds,
            Vec::<u8>::new()
        );
    }

    #[test]
    fn threshold_list_drops_unknown_and_out_of_range_values() {
        assert_eq!(
            parse_config("notify_thresholds = [50, 80, 101, -3, 999999, 100]\n")
                .notify_thresholds,
            vec![50, 100]
        );
    }

    #[test]
    fn threshold_list_is_deduplicated_and_sorted() {
        assert_eq!(
            parse_config("notify_thresholds = [100, 50, 100, 75, 50]\n").notify_thresholds,
            vec![50, 75, 100]
        );
    }

    #[test]
    fn a_non_list_threshold_value_falls_back_to_the_default_set() {
        for body in [
            "notify_thresholds = 90\n",
            "notify_thresholds = \"all\"\n",
            "notify_thresholds = true\n",
            "notify_thresholds = { at = 90 }\n",
        ] {
            assert_eq!(
                parse_config(body).notify_thresholds,
                NOTIFY_THRESHOLDS.to_vec(),
                "body: {body}"
            );
        }
        // A list of the wrong element type keeps list semantics: nothing
        // survives the filter, so nothing is enabled.
        assert_eq!(
            parse_config("notify_thresholds = [\"90\", true]\n").notify_thresholds,
            Vec::<u8>::new()
        );
    }

    #[test]
    fn notify_on_reset_falls_back_when_wrong_typed() {
        assert!(!parse_config("notify_on_reset = false\n").notify_on_reset);
        assert!(parse_config("notify_on_reset = 1\n").notify_on_reset);
    }

    #[test]
    fn set_notifies_at_toggles_and_keeps_the_list_sorted_and_unique() {
        let mut config = Config {
            notify_thresholds: vec![],
            ..Config::default()
        };
        config.set_notifies_at(100, true);
        config.set_notifies_at(50, true);
        config.set_notifies_at(50, true);
        assert_eq!(config.notify_thresholds, vec![50, 100]);
        assert!(config.notifies_at(50));

        config.set_notifies_at(50, false);
        assert_eq!(config.notify_thresholds, vec![100]);
        assert!(!config.notifies_at(50));

        // Unknown thresholds are not storable.
        config.set_notifies_at(80, true);
        assert_eq!(config.notify_thresholds, vec![100]);
    }

    #[test]
    fn critical_urgency_starts_at_ninety() {
        assert!(!is_critical(50));
        assert!(!is_critical(75));
        assert!(is_critical(90));
        assert!(is_critical(99));
        assert!(is_critical(100));
    }

    #[test]
    fn corrupt_or_wrong_typed_values_fall_back_to_defaults() {
        assert_eq!(parse_config("this is not toml {{{"), Config::default());
        assert_eq!(parse_config(""), Config::default());
        assert_eq!(
            parse_config("refresh_secs = \"soon\"\nlaunch_at_login = 7\n"),
            Config::default()
        );
        assert_eq!(parse_config("refresh_secs = 0\n"), Config::default());
        assert_eq!(parse_config("refresh_secs = -5\n"), Config::default());
    }

    #[test]
    fn render_round_trips_through_parse() {
        let config = Config {
            refresh_secs: 60,
            launch_at_login: true,
            notify_thresholds: vec![75, 99],
            notify_on_reset: false,
        };
        assert_eq!(parse_config(&render_config(&config)), config);

        // The all-off list must survive the round trip as itself, not as the
        // default set.
        let none = Config {
            notify_thresholds: vec![],
            ..Config::default()
        };
        assert_eq!(parse_config(&render_config(&none)), none);
    }

    #[test]
    fn load_from_missing_file_is_defaults() {
        let dir = TempDir::new("load-missing");
        assert_eq!(
            load_from(&dir.path().join("nope/config.toml")),
            Config::default()
        );
    }

    #[test]
    fn save_then_load_round_trips_and_creates_the_directory() {
        let dir = TempDir::new("save-load");
        let path = dir.path().join("claude-usage-tray/config.toml");
        let config = Config {
            refresh_secs: 15,
            launch_at_login: true,
            notify_thresholds: vec![50, 100],
            notify_on_reset: false,
        };
        save_to(&path, &config).expect("save succeeds");
        assert!(path.exists());
        assert_eq!(load_from(&path), config);
    }

    #[test]
    fn save_overwrites_and_leaves_no_temp_file_behind() {
        let dir = TempDir::new("save-overwrite");
        let path = dir.path().join("config.toml");
        save_to(&path, &Config::default()).expect("first save");
        let updated = Config {
            refresh_secs: 30,
            launch_at_login: true,
            ..Config::default()
        };
        save_to(&path, &updated).expect("second save");
        assert_eq!(load_from(&path), updated);

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read temp dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .filter(|name| name != "config.toml")
            .collect();
        assert!(leftovers.is_empty(), "unexpected files: {leftovers:?}");
    }

    #[test]
    fn env_override_accepts_a_valid_value() {
        assert_eq!(env_override(Some(" 30 ")), Some(30));
    }

    #[test]
    fn env_override_rejects_unset_garbage_zero_and_negative() {
        assert_eq!(env_override(None), None);
        assert_eq!(env_override(Some("")), None);
        assert_eq!(env_override(Some("soon")), None);
        assert_eq!(env_override(Some("-1")), None);
        assert_eq!(env_override(Some("0")), None);
    }

    #[test]
    fn env_override_wins_over_the_configured_interval() {
        let config = Config {
            refresh_secs: 60,
            ..Config::default()
        };
        assert_eq!(env_override(Some("2")).unwrap_or(config.refresh_secs), 2);
        assert_eq!(env_override(None).unwrap_or(config.refresh_secs), 60);
    }

    #[test]
    fn refresh_choice_maps_to_the_radio_index() {
        for (index, &secs) in REFRESH_CHOICES.iter().enumerate() {
            let config = Config {
                refresh_secs: secs,
                ..Config::default()
            };
            assert_eq!(config.refresh_choice(), index);
        }
    }

    #[test]
    fn dir_is_writable_creates_a_missing_directory_and_leaves_nothing_behind() {
        let dir = TempDir::new("writable-probe");
        let target = dir.path().join("claude-usage-tray");
        assert!(dir_is_writable(&target));
        assert!(target.is_dir());
        let leftovers: Vec<_> = std::fs::read_dir(&target)
            .expect("read probed dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        assert!(leftovers.is_empty(), "probe left files: {leftovers:?}");
    }

    #[test]
    fn dir_is_writable_is_false_for_a_read_only_directory() {
        let dir = TempDir::new("writable-readonly");
        let target = dir.path().join("locked");
        std::fs::create_dir_all(&target).expect("create dir");
        crate::testutil::set_mode(&target, 0o555);

        // Root ignores the mode bits, so the assertion below would be a lie
        // there. Find out by trying it directly rather than guessing at the
        // uid, then skip.
        let root_can_write_anyway = std::fs::write(target.join("root-check"), b"").is_ok();
        if root_can_write_anyway {
            let _ = std::fs::remove_file(target.join("root-check"));
        } else {
            assert!(!dir_is_writable(&target));
        }

        // Restore so the TempDir can clean itself up.
        crate::testutil::set_mode(&target, 0o755);
    }

    #[test]
    fn dir_is_writable_is_false_when_the_path_is_a_file() {
        let dir = TempDir::new("writable-file");
        let path = dir.path().join("not-a-dir");
        std::fs::write(&path, b"x").expect("write file");
        assert!(!dir_is_writable(&path));
    }

    #[test]
    fn refresh_choice_for_an_unlisted_interval_selects_the_default() {
        let config = Config {
            refresh_secs: 7,
            ..Config::default()
        };
        assert_eq!(
            REFRESH_CHOICES[config.refresh_choice()],
            DEFAULT_REFRESH_SECS
        );
    }
}
