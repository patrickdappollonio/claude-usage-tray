//! Persisted user settings for the tray: the poll interval and the
//! launch-at-login flag.
//!
//! The file lives at `~/.config/claude-usage-tray/config.toml` (respecting
//! `$XDG_CONFIG_HOME`) and holds exactly two keys:
//!
//! ```toml
//! refresh_secs = 5
//! launch_at_login = false
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

/// User settings as stored in `config.toml`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Seconds between cache re-reads.
    pub refresh_secs: u64,
    /// Whether an XDG autostart entry should exist for the tray.
    pub launch_at_login: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            refresh_secs: DEFAULT_REFRESH_SECS,
            launch_at_login: false,
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
    Config {
        refresh_secs,
        launch_at_login,
    }
}

/// Renders a config back to TOML. Written by hand rather than through a
/// serializer: the schema is two scalars, and this keeps the `toml` dependency
/// to its parser half.
pub fn render_config(config: &Config) -> String {
    format!(
        "refresh_secs = {}\nlaunch_at_login = {}\n",
        config.refresh_secs, config.launch_at_login
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
    fn default_is_five_seconds_and_no_autostart() {
        assert_eq!(
            Config::default(),
            Config {
                refresh_secs: 5,
                launch_at_login: false
            }
        );
    }

    #[test]
    fn parses_a_full_config() {
        let config = parse_config("refresh_secs = 30\nlaunch_at_login = true\n");
        assert_eq!(
            config,
            Config {
                refresh_secs: 30,
                launch_at_login: true
            }
        );
    }

    #[test]
    fn parses_a_partial_config_with_defaults_for_the_rest() {
        assert_eq!(
            parse_config("launch_at_login = true\n"),
            Config {
                refresh_secs: 5,
                launch_at_login: true
            }
        );
        assert_eq!(
            parse_config("refresh_secs = 60\n"),
            Config {
                refresh_secs: 60,
                launch_at_login: false
            }
        );
    }

    #[test]
    fn ignores_unknown_keys() {
        assert_eq!(
            parse_config("colour = \"blue\"\nrefresh_secs = 15\n"),
            Config {
                refresh_secs: 15,
                launch_at_login: false
            }
        );
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
        };
        assert_eq!(parse_config(&render_config(&config)), config);
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
            launch_at_login: false,
        };
        assert_eq!(env_override(Some("2")).unwrap_or(config.refresh_secs), 2);
        assert_eq!(env_override(None).unwrap_or(config.refresh_secs), 60);
    }

    #[test]
    fn refresh_choice_maps_to_the_radio_index() {
        for (index, &secs) in REFRESH_CHOICES.iter().enumerate() {
            let config = Config {
                refresh_secs: secs,
                launch_at_login: false,
            };
            assert_eq!(config.refresh_choice(), index);
        }
    }

    #[test]
    fn refresh_choice_for_an_unlisted_interval_selects_the_default() {
        let config = Config {
            refresh_secs: 7,
            launch_at_login: false,
        };
        assert_eq!(
            REFRESH_CHOICES[config.refresh_choice()],
            DEFAULT_REFRESH_SECS
        );
    }
}
