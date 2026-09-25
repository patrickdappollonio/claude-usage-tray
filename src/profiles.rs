//! Finding every Claude Code config directory ("profile") on the machine.
//!
//! Claude Code picks its config directory from `$CLAUDE_CONFIG_DIR`, falling
//! back to `~/.claude`. The statusline hook runs inside Claude Code and
//! inherits that answer; the tray runs from the login session and may not.
//! When the two disagree, the hook writes its cache where the tray never
//! looks. So the tray never trusts its own environment alone.
//!
//! The candidates are:
//!
//! * the *primary* directory: `$CLAUDE_CONFIG_DIR` in this process's
//!   environment, else `~/.claude`. It always counts, so a fresh install still
//!   has somewhere to install the hook and to report "no data" for;
//! * `~/.claude`, when the primary is something else;
//! * sibling profiles named `~/.claude-*`;
//! * the directories listed in the known-directories file, which the hook adds
//!   to the first time it writes a cache somewhere other than `~/.claude`.
//!   That is what covers a directory with a name nothing else could guess.
//!
//! Every candidate but the primary counts only if it looks like a directory
//! Claude Code has actually used, with both `sessions/` and `projects/` in it.
//! That keeps an empty or unrelated `~/.claude-something` from turning a
//! single-profile setup into a multi-profile one.

use std::io;
use std::path::{Path, PathBuf};

use crate::source;

/// Name of the file, next to `config.toml`, that lists the non-default config
/// directories the hook has written to: one absolute path per line.
pub const KNOWN_DIRS_FILE_NAME: &str = "claude-config-dirs";

/// What every counted directory except the primary must contain.
const PROFILE_MARKERS: [&str; 2] = ["sessions", "projects"];

/// One Claude Code config directory and the two usage files the tray reads
/// from it.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    /// Short display name: `default` for `~/.claude`, `work` for
    /// `~/.claude-work`, the folder name otherwise.
    pub name: String,
    pub config_dir: PathBuf,
    /// Whether this is the directory this process's own environment points
    /// at.
    pub primary: bool,
    /// The statusline hook's cache inside `config_dir`.
    pub cache_path: PathBuf,
    /// Claude Code's own `.claude.json`.
    pub app_cache_path: PathBuf,
}

impl Profile {
    /// The paths Claude Code uses for `config_dir`. `.claude.json` sits inside
    /// the directory, except for the default `~/.claude`, whose `.claude.json`
    /// is in the home directory itself.
    pub fn new(config_dir: PathBuf, home: &Path, primary: bool) -> Self {
        let is_default = config_dir == home.join(".claude");
        let app_cache_path = if is_default {
            home.join(".claude.json")
        } else {
            config_dir.join(".claude.json")
        };
        Profile {
            name: base_name(&config_dir, is_default),
            cache_path: source::cache_path_in(&config_dir),
            app_cache_path,
            primary,
            config_dir,
        }
    }
}

/// `default`, `work` for `.claude-work`, or the folder name without a leading
/// dot.
fn base_name(dir: &Path, is_default: bool) -> String {
    if is_default {
        return "default".to_string();
    }
    let folder = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let folder = folder.strip_prefix(".claude-").unwrap_or(&folder);
    let folder = folder.strip_prefix('.').unwrap_or(folder);
    if folder.is_empty() {
        dir.display().to_string()
    } else {
        folder.to_string()
    }
}

/// A candidate that did not count, and why. Only the `profiles` command shows
/// these: they are what explains a profile the user expected but the tray
/// does not read.
#[derive(Clone, Debug, PartialEq)]
pub struct Skipped {
    pub config_dir: PathBuf,
    pub reason: &'static str,
}

/// The outcome of one discovery pass.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Discovery {
    /// The primary profile first, then the rest in discovery order.
    pub profiles: Vec<Profile>,
    pub skipped: Vec<Skipped>,
}

/// Default known-directories file:
/// `<tray config dir>/claude-config-dirs`.
pub fn known_dirs_path() -> PathBuf {
    crate::config::config_dir().join(KNOWN_DIRS_FILE_NAME)
}

/// Discovers the profiles on this machine from the real environment. Cheap
/// enough for the tray to call on every poll, so a profile created while it
/// runs shows up without a restart.
pub fn discover() -> Discovery {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let env_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from);
    discover_in(env_dir, &home, &read_known_dirs(&known_dirs_path()))
}

/// The resolution behind [`discover`], parameterized so tests never touch the
/// real home directory.
pub fn discover_in(env_dir: Option<PathBuf>, home: &Path, known: &[PathBuf]) -> Discovery {
    let default_dir = home.join(".claude");
    let primary = env_dir.unwrap_or_else(|| default_dir.clone());

    let mut candidates = vec![default_dir];
    candidates.extend(sibling_profiles(home));
    candidates.extend(known.iter().cloned());

    let mut discovery = Discovery {
        profiles: vec![Profile::new(primary.clone(), home, true)],
        skipped: Vec::new(),
    };
    let mut seen = vec![primary];
    for dir in candidates {
        if seen.contains(&dir) {
            continue;
        }
        seen.push(dir.clone());
        if !dir.is_dir() {
            // A known directory that has since been deleted is worth
            // mentioning; a `~/.claude` that never existed is not.
            if known.contains(&dir) {
                discovery.skipped.push(Skipped {
                    config_dir: dir,
                    reason: "no longer exists",
                });
            }
            continue;
        }
        if !looks_like_profile(&dir) {
            discovery.skipped.push(Skipped {
                config_dir: dir,
                reason: "no sessions/ and projects/ folders, so Claude Code has not used it",
            });
            continue;
        }
        discovery.profiles.push(Profile::new(dir, home, false));
    }
    dedupe_names(&mut discovery.profiles);
    discovery
}

/// Whether Claude Code has used `dir` as its config directory.
fn looks_like_profile(dir: &Path) -> bool {
    PROFILE_MARKERS
        .iter()
        .all(|marker| dir.join(marker).is_dir())
}

/// Two profiles can share a folder name (`/a/claude` and `/b/claude`). The
/// first keeps the short name; later ones fall back to their full path, which
/// is unique by construction.
fn dedupe_names(profiles: &mut [Profile]) {
    for index in 1..profiles.len() {
        let (earlier, rest) = profiles.split_at_mut(index);
        let current = &mut rest[0];
        if earlier.iter().any(|profile| profile.name == current.name) {
            current.name = current.config_dir.display().to_string();
        }
    }
}

/// `~/.claude-*` directories, sorted so discovery order is stable.
fn sibling_profiles(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".claude-"))
        })
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    found.sort();
    found
}

/// Reads the known-directories file. A missing or unreadable file is an empty
/// list; blank lines are ignored.
pub fn read_known_dirs(path: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(path)
        .map(|body| {
            body.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Adds `config_dir` to the known-directories file at `path` unless it is
/// already listed or is the default `~/.claude`, which is always a candidate.
/// The whole file is rewritten through a rename, so a reader never sees half
/// of it. Two first-time writers racing can drop one entry; `hook install`
/// adds it back.
pub fn record_known_dir(path: &Path, config_dir: &Path, home: &Path) -> io::Result<()> {
    let config_dir = std::path::absolute(config_dir)?;
    if config_dir == home.join(".claude") {
        return Ok(());
    }
    let mut known = read_known_dirs(path);
    if known.contains(&config_dir) {
        return Ok(());
    }
    known.push(config_dir);

    let mut body = String::new();
    for dir in &known {
        body.push_str(&dir.to_string_lossy());
        body.push('\n');
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, body)?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

/// [`record_known_dir`] against the real known-directories file. Failures are
/// ignored: the note only helps the tray find the directory, and nothing
/// that calls this should fail because of it.
pub fn remember(config_dir: &Path) {
    if let Some(home) = dirs::home_dir() {
        let _ = record_known_dir(&known_dirs_path(), config_dir, &home);
    }
}

/// The `profiles` command's report: one block per profile, then the skipped
/// folders. Everything the tray's "no data" could be explained by is in here:
/// a missing profile, a hook not installed, a hook writing somewhere else, or
/// a cache nobody has written yet.
pub fn render(discovery: &Discovery, now: jiff::Timestamp) -> String {
    let mut blocks = Vec::new();
    for profile in &discovery.profiles {
        let status = crate::hook::status_in(&profile.config_dir, now);
        let mut lines = vec![format!(
            "{}  {}{}",
            profile.name,
            profile.config_dir.display(),
            if profile.primary {
                "  (this shell's profile)"
            } else {
                ""
            }
        )];
        lines.push(format!("  hook: {}", hook_state(profile, &status)));
        lines.push(format!("  data: {}", status.cache_freshness(now)));
        blocks.push(lines.join("\n"));
    }
    if discovery.profiles.len() == 1 {
        blocks.push("Only one profile, so the tray shows it the way it always has.".to_string());
    } else {
        blocks.push(format!(
            "{} profiles, so the tray shows a section for each.",
            discovery.profiles.len()
        ));
    }
    if !discovery.skipped.is_empty() {
        let mut lines = vec!["Skipped:".to_string()];
        for skipped in &discovery.skipped {
            lines.push(format!(
                "  {}: {}",
                skipped.config_dir.display(),
                skipped.reason
            ));
        }
        blocks.push(lines.join("\n"));
    }
    blocks.join("\n\n")
}

/// One line on whether `profile`'s hook is installed and writing to the right
/// place.
fn hook_state(profile: &Profile, status: &crate::hook::StatusReport) -> String {
    if !status.installed {
        return "not installed (run `claude-usage-tray hook install`)".to_string();
    }
    let here = std::path::absolute(&profile.config_dir).unwrap_or(profile.config_dir.clone());
    match status.recorded_config_dir.as_deref().map(PathBuf::from) {
        Some(dir) if dir == here => "installed".to_string(),
        Some(dir) => format!(
            "installed, but writes its data to {} (run `claude-usage-tray hook install` to fix)",
            dir.display()
        ),
        None => "installed (the tray names this folder in it on its next start)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn dirs_of(discovery: &Discovery) -> Vec<PathBuf> {
        discovery
            .profiles
            .iter()
            .map(|p| p.config_dir.clone())
            .collect()
    }

    fn names_of(discovery: &Discovery) -> Vec<String> {
        discovery.profiles.iter().map(|p| p.name.clone()).collect()
    }

    /// A directory Claude Code has used: both marker folders present.
    fn make_profile(dir: &Path) {
        for marker in PROFILE_MARKERS {
            std::fs::create_dir_all(dir.join(marker)).expect("create marker");
        }
    }

    #[test]
    fn default_profile_reads_claude_json_from_home() {
        let profile = Profile::new(
            PathBuf::from("/home/me/.claude"),
            Path::new("/home/me"),
            true,
        );
        assert_eq!(profile.name, "default");
        assert_eq!(
            profile.cache_path,
            PathBuf::from("/home/me/.claude/usage-tray-statusline.json")
        );
        assert_eq!(
            profile.app_cache_path,
            PathBuf::from("/home/me/.claude.json")
        );
    }

    #[test]
    fn other_profiles_read_claude_json_from_inside_and_get_short_names() {
        let home = Path::new("/home/me");
        let work = Profile::new(PathBuf::from("/home/me/.claude-work"), home, false);
        assert_eq!(work.name, "work");
        assert_eq!(
            work.app_cache_path,
            PathBuf::from("/home/me/.claude-work/.claude.json")
        );
        let custom = Profile::new(PathBuf::from("/srv/some/dir"), home, false);
        assert_eq!(custom.name, "dir");
        let hidden = Profile::new(PathBuf::from("/srv/.agent"), home, false);
        assert_eq!(hidden.name, "agent");
    }

    #[test]
    fn with_nothing_on_disk_the_primary_still_counts() {
        let home = TempDir::new("profiles-bare");
        let found = discover_in(None, home.path(), &[]);
        assert_eq!(dirs_of(&found), vec![home.path().join(".claude")]);
        assert!(found.profiles[0].primary);
        assert!(found.skipped.is_empty());
    }

    #[test]
    fn env_dir_is_primary_and_a_used_default_follows() {
        let home = TempDir::new("profiles-env");
        make_profile(&home.path().join(".claude"));
        let custom = home.path().join("elsewhere");
        let found = discover_in(Some(custom.clone()), home.path(), &[]);
        assert_eq!(dirs_of(&found), vec![custom, home.path().join(".claude")]);
        assert!(found.profiles[0].primary);
        assert!(!found.profiles[1].primary);
    }

    #[test]
    fn siblings_count_only_when_claude_code_has_used_them() {
        let home = TempDir::new("profiles-siblings");
        make_profile(&home.path().join(".claude"));
        make_profile(&home.path().join(".claude-work"));
        make_profile(&home.path().join(".claude-alt"));
        std::fs::create_dir_all(home.path().join(".claude-empty")).unwrap();
        std::fs::create_dir_all(home.path().join(".claudeish")).unwrap();
        // A file with a matching name is not a candidate at all.
        std::fs::write(home.path().join(".claude-notes"), "").unwrap();

        let found = discover_in(None, home.path(), &[]);
        assert_eq!(names_of(&found), vec!["default", "alt", "work"]);
        assert_eq!(
            found.skipped,
            vec![Skipped {
                config_dir: home.path().join(".claude-empty"),
                reason: "no sessions/ and projects/ folders, so Claude Code has not used it",
            }]
        );
    }

    #[test]
    fn known_dirs_count_once_and_report_when_gone() {
        let home = TempDir::new("profiles-known");
        let custom = home.path().join("custom");
        make_profile(&custom);
        let gone = home.path().join("gone");
        let found = discover_in(
            None,
            home.path(),
            &[custom.clone(), custom.clone(), gone.clone()],
        );
        assert_eq!(dirs_of(&found), vec![home.path().join(".claude"), custom]);
        assert_eq!(
            found.skipped,
            vec![Skipped {
                config_dir: gone,
                reason: "no longer exists",
            }]
        );
    }

    #[test]
    fn a_repeated_name_falls_back_to_the_full_path() {
        let home = TempDir::new("profiles-dupes");
        let first = home.path().join("a").join("claude");
        let second = home.path().join("b").join("claude");
        make_profile(&first);
        make_profile(&second);
        let found = discover_in(Some(first), home.path(), std::slice::from_ref(&second));
        assert_eq!(
            names_of(&found),
            vec!["claude".to_string(), second.display().to_string()]
        );
    }

    #[test]
    fn render_marks_the_primary_and_reports_hooks_data_and_skips() {
        let home = TempDir::new("profiles-render");
        let default_dir = home.path().join(".claude");
        let work = home.path().join(".claude-work");
        make_profile(&default_dir);
        make_profile(&work);
        std::fs::create_dir_all(home.path().join(".claude-empty")).unwrap();
        crate::hook::install_in(&work, Path::new("/opt/tray")).expect("install");
        std::fs::write(
            default_dir.join(crate::hook::SETTINGS_FILE_NAME),
            r#"{"statusLine":{"command":"/opt/tray statusline --config-dir /elsewhere"}}"#,
        )
        .unwrap();

        let text = render(&discover_in(None, home.path(), &[]), jiff::Timestamp::now());
        let expected_head = format!(
            "default  {}  (this shell's profile)\n  hook: installed, but writes its data to /elsewhere",
            default_dir.display()
        );
        assert!(text.starts_with(&expected_head), "{text}");
        assert!(text.contains(&format!(
            "work  {}\n  hook: installed\n  data: no cache file yet",
            work.display()
        )));
        assert!(text.contains("2 profiles, so the tray shows a section for each."));
        assert!(text.contains(&format!(
            "Skipped:\n  {}: no sessions/",
            home.path().join(".claude-empty").display()
        )));
    }

    #[test]
    fn render_of_a_single_profile_says_nothing_changes() {
        let home = TempDir::new("profiles-render-single");
        let text = render(&discover_in(None, home.path(), &[]), jiff::Timestamp::now());
        assert!(text.contains("hook: not installed"));
        assert!(text.contains("Only one profile"));
        assert!(!text.contains("Skipped"));
    }

    #[test]
    fn record_known_dir_appends_new_dirs_once() {
        let temp = TempDir::new("profiles-record");
        let file = temp.path().join("state").join(KNOWN_DIRS_FILE_NAME);
        let home = temp.path().join("home");
        record_known_dir(&file, Path::new("/srv/a"), &home).unwrap();
        record_known_dir(&file, Path::new("/srv/b"), &home).unwrap();
        record_known_dir(&file, Path::new("/srv/a"), &home).unwrap();
        assert_eq!(
            read_known_dirs(&file),
            vec![PathBuf::from("/srv/a"), PathBuf::from("/srv/b")]
        );
    }

    #[test]
    fn record_known_dir_skips_the_default_dir() {
        let temp = TempDir::new("profiles-record-default");
        let file = temp.path().join(KNOWN_DIRS_FILE_NAME);
        let home = temp.path().join("home");
        record_known_dir(&file, &home.join(".claude"), &home).unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn record_known_dir_stores_relative_dirs_as_absolute() {
        let temp = TempDir::new("profiles-record-relative");
        let file = temp.path().join(KNOWN_DIRS_FILE_NAME);
        record_known_dir(&file, Path::new("rel/dir"), Path::new("/nowhere")).unwrap();
        let known = read_known_dirs(&file);
        assert_eq!(known.len(), 1);
        assert!(known[0].is_absolute());
        assert!(known[0].ends_with("rel/dir"));
    }

    #[test]
    fn read_known_dirs_ignores_blank_lines_and_a_missing_file() {
        let temp = TempDir::new("profiles-read");
        let file = temp.path().join(KNOWN_DIRS_FILE_NAME);
        assert!(read_known_dirs(&file).is_empty());
        std::fs::write(&file, "/a\n\n  \n/b\n").unwrap();
        assert_eq!(
            read_known_dirs(&file),
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
    }
}
