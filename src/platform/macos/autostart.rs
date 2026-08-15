//! "Launch at login" on macOS: a LaunchAgent property list.
//!
//! Enabling writes a `.plist` into `~/Library/LaunchAgents/`; disabling
//! removes it; the checkbox state is simply "does that file exist". That is
//! the same shape as the XDG autostart entry the Linux backend writes, and it
//! is deliberately the same shape: only the directory and the file format
//! differ.
//!
//! `launchctl` is not called. The plist is read by `launchd` when the user
//! logs in, which is precisely when "launch at login" is supposed to take
//! effect — and the only process the checkbox could otherwise start or stop is
//! this one, which is already running. Skipping it keeps the whole module
//! filesystem-only, which is what lets its tests run everywhere (see
//! `crate::platform`).
//!
//! Nothing here panics: I/O errors are returned to the caller, which logs them
//! and leaves the checkbox where it was.

use std::io;
use std::path::{Path, PathBuf};

/// Reverse-DNS label of the agent, and the file name it is stored under.
const LABEL: &str = "com.patrickdappollonio.claude-usage-tray";

/// File name of the LaunchAgent entry.
const ENTRY_NAME: &str = "com.patrickdappollonio.claude-usage-tray.plist";

/// Default LaunchAgent directory: `~/Library/LaunchAgents`.
pub fn default_autostart_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library")
        .join("LaunchAgents")
}

/// Path of the entry inside `dir`.
pub fn entry_path(dir: &Path) -> PathBuf {
    dir.join(ENTRY_NAME)
}

/// Builds the LaunchAgent plist for a given executable path.
///
/// Pure, so the exact keys are unit-tested without writing anything. The
/// executable path is XML-escaped: it comes from `std::env::current_exe`, so
/// it is whatever the user named the directory they put the binary in, and an
/// `&` in it must not produce a plist `launchd` refuses to parse.
///
/// `--foreground` is passed deliberately: a bare invocation re-executes itself
/// in the background and exits, and `launchd` would then be supervising a
/// process that is already gone while the real tray runs outside its
/// knowledge. Attached, the job `launchd` started is the tray itself.
pub fn launch_agent_plist(exec: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{exec}</string>\n\
         \t\t<string>--foreground</string>\n\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         </dict>\n\
         </plist>\n",
        exec = escape_xml(&exec.display().to_string()),
    )
}

/// Escapes the five XML entities. Only ever applied to a filesystem path.
fn escape_xml(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// True when the LaunchAgent exists in `dir`.
pub fn is_enabled_in(dir: &Path) -> bool {
    entry_path(dir).is_file()
}

/// Writes the LaunchAgent into `dir`, creating the directory if needed. The
/// write is atomic (temp file + rename) so `launchd` never sees a half-written
/// plist.
pub fn enable_in(dir: &Path, exec: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = entry_path(dir);
    let temp = path.with_extension("plist.tmp");
    std::fs::write(&temp, launch_agent_plist(exec))?;
    match std::fs::rename(&temp, &path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// Removes the LaunchAgent from `dir`. Removing an entry that isn't there
/// succeeds: the requested end state ("not autostarting") already holds.
pub fn disable_in(dir: &Path) -> io::Result<()> {
    match std::fs::remove_file(entry_path(dir)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Whether the LaunchAgent in `dir` could actually be created or removed right
/// now. Shares the config module's create-then-write probe, since the question
/// ("can we put a file here?") is the same one.
pub fn is_available_in(dir: &Path) -> bool {
    crate::config::dir_is_writable(dir)
}

/// Whether the real LaunchAgents directory is usable. Probed each time the
/// menu is built so that fixing the permissions un-grays the checkbox without
/// a restart.
pub fn is_available() -> bool {
    is_available_in(&default_autostart_dir())
}

/// True when the entry exists in the real LaunchAgents directory.
pub fn is_enabled() -> bool {
    is_enabled_in(&default_autostart_dir())
}

/// Applies `enabled` to the real LaunchAgents directory, reporting whether the
/// end state was reached. Failures are logged and reported as `false` so the
/// caller can leave the checkbox unchanged rather than lying about the state.
pub fn set_enabled(enabled: bool) -> bool {
    let dir = default_autostart_dir();
    let result = if enabled {
        match std::env::current_exe() {
            Ok(exe) => enable_in(&dir, &exe),
            Err(err) => Err(err),
        }
    } else {
        disable_in(&dir)
    };
    match result {
        Ok(()) => true,
        Err(err) => {
            eprintln!(
                "claude-usage-tray: could not update {}: {err}",
                entry_path(&dir).display()
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn the_plist_has_the_required_keys() {
        let plist = launch_agent_plist(Path::new("/usr/local/bin/claude-usage-tray"));
        assert!(plist.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        for fragment in [
            "<key>Label</key>",
            "<string>com.patrickdappollonio.claude-usage-tray</string>",
            "<key>ProgramArguments</key>",
            "<string>/usr/local/bin/claude-usage-tray</string>",
            "<key>RunAtLoad</key>",
            "<true/>",
        ] {
            assert!(plist.contains(fragment), "missing {fragment:?}");
        }
        assert!(plist.trim_end().ends_with("</plist>"));
    }

    /// `launchd` starts the tray directly rather than having it re-execute
    /// itself into the background, so the job it supervises is the tray and not
    /// a process that exits at once.
    #[test]
    fn the_plist_runs_the_tray_in_the_foreground() {
        let plist = launch_agent_plist(Path::new("/usr/local/bin/claude-usage-tray"));
        let arguments = plist
            .split("<key>ProgramArguments</key>")
            .nth(1)
            .and_then(|rest| rest.split("</array>").next())
            .expect("a ProgramArguments array");
        let strings: Vec<&str> = arguments
            .split("<string>")
            .skip(1)
            .filter_map(|piece| piece.split("</string>").next())
            .collect();
        assert_eq!(
            strings,
            vec!["/usr/local/bin/claude-usage-tray", "--foreground"]
        );
    }

    /// The label is the file name without `.plist`; `launchctl` matches on it,
    /// so the two drifting apart would produce an agent that cannot be
    /// addressed by the file it lives in.
    #[test]
    fn the_label_matches_the_file_name() {
        assert_eq!(ENTRY_NAME, format!("{LABEL}.plist"));
    }

    #[test]
    fn the_plist_uses_the_absolute_exec_path_given() {
        let plist = launch_agent_plist(Path::new("/Users/someone/bin/tray"));
        assert!(plist.contains("<string>/Users/someone/bin/tray</string>"));
    }

    /// Launched from the app bundle, `current_exe` is the path *inside* the
    /// bundle, spaces and all. `ProgramArguments` is an array, so `launchd`
    /// never splits it on whitespace the way a shell would; the only thing that
    /// has to survive is the XML escaping. This pins that: one `<string>`
    /// holding the whole path, and the `--foreground` flag still separate.
    #[test]
    fn the_plist_keeps_a_bundled_exe_path_with_spaces_in_one_argument() {
        let exec = "/Applications/Claude Usage Tray.app/Contents/MacOS/claude-usage-tray";
        let plist = launch_agent_plist(Path::new(exec));
        let arguments = plist
            .split("<key>ProgramArguments</key>")
            .nth(1)
            .and_then(|rest| rest.split("</array>").next())
            .expect("a ProgramArguments array");
        let strings: Vec<&str> = arguments
            .split("<string>")
            .skip(1)
            .filter_map(|piece| piece.split("</string>").next())
            .collect();
        assert_eq!(strings, vec![exec, "--foreground"]);
    }

    /// A path is arbitrary user data as far as XML is concerned, and a raw `&`
    /// makes the whole plist unparseable — which would silently disable
    /// autostart for anyone with a `Rock & Roll` directory in their path.
    #[test]
    fn the_plist_escapes_xml_in_the_exec_path() {
        let plist = launch_agent_plist(Path::new("/Users/a&b/<c>/tray"));
        assert!(plist.contains("<string>/Users/a&amp;b/&lt;c&gt;/tray</string>"));
        assert!(!plist.contains("/Users/a&b/"));
    }

    #[test]
    fn enable_creates_the_entry_and_the_directory() {
        let temp = TempDir::new("launchagent-enable");
        let dir = temp.path().join("LaunchAgents");
        assert!(!is_enabled_in(&dir));

        enable_in(&dir, Path::new("/opt/tray")).expect("enable succeeds");
        assert!(is_enabled_in(&dir));
        let body = std::fs::read_to_string(entry_path(&dir)).expect("read entry");
        assert_eq!(body, launch_agent_plist(Path::new("/opt/tray")));
    }

    #[test]
    fn enable_is_idempotent_and_leaves_no_temp_file() {
        let temp = TempDir::new("launchagent-idempotent");
        let dir = temp.path().to_path_buf();
        enable_in(&dir, Path::new("/opt/tray")).expect("first enable");
        enable_in(&dir, Path::new("/opt/tray")).expect("second enable");

        let names: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(names.len(), 1, "unexpected files: {names:?}");
        assert_eq!(names[0], ENTRY_NAME);
    }

    #[test]
    fn disable_removes_the_entry() {
        let temp = TempDir::new("launchagent-disable");
        let dir = temp.path().to_path_buf();
        enable_in(&dir, Path::new("/opt/tray")).expect("enable");
        disable_in(&dir).expect("disable succeeds");
        assert!(!is_enabled_in(&dir));
    }

    #[test]
    fn disable_when_absent_succeeds() {
        let temp = TempDir::new("launchagent-disable-absent");
        disable_in(temp.path()).expect("disabling an absent entry is fine");
        disable_in(&temp.path().join("never-created")).expect("missing dir is fine too");
    }

    #[test]
    fn is_available_in_is_true_for_a_writable_directory() {
        let temp = TempDir::new("launchagent-available");
        // Also true for one that does not exist yet: it can be created.
        assert!(is_available_in(&temp.path().join("LaunchAgents")));
        assert!(is_available_in(temp.path()));
    }

    #[test]
    fn is_available_in_is_false_for_a_read_only_directory() {
        let temp = TempDir::new("launchagent-unavailable");
        let dir = temp.path().join("locked");
        std::fs::create_dir_all(&dir).expect("create dir");
        crate::testutil::set_mode(&dir, 0o555);

        let root_can_write_anyway = std::fs::write(dir.join("root-check"), b"").is_ok();
        if root_can_write_anyway {
            let _ = std::fs::remove_file(dir.join("root-check"));
        } else {
            assert!(!is_available_in(&dir));
        }

        crate::testutil::set_mode(&dir, 0o755);
    }

    #[test]
    fn is_enabled_ignores_a_directory_of_the_same_name() {
        let temp = TempDir::new("launchagent-dir-collision");
        std::fs::create_dir_all(entry_path(temp.path())).expect("create colliding dir");
        assert!(!is_enabled_in(temp.path()));
    }

    /// The app bundle and the LaunchAgent name the same application, and both
    /// spellings are written out by hand in two different languages (Rust here,
    /// `sh` there). Reading the script back is the only thing that keeps them
    /// from drifting apart into an app called one thing and a login item called
    /// another.
    #[test]
    fn the_bundle_identifier_matches_the_launch_agent_label() {
        let script = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/make-app-bundle.sh"),
        )
        .expect("read the bundle script");
        assert!(
            script.contains(&format!("BUNDLE_ID=\"{LABEL}\"")),
            "scripts/make-app-bundle.sh no longer uses {LABEL} as CFBundleIdentifier"
        );
    }

    /// The entry lands under the user's own `Library`, never the system-wide
    /// `/Library/LaunchAgents` (which needs root and would run for everybody).
    #[test]
    fn the_default_directory_is_the_users_launch_agents_folder() {
        let dir = default_autostart_dir();
        assert!(
            dir.ends_with("Library/LaunchAgents"),
            "unexpected directory: {}",
            dir.display()
        );
        assert!(!dir.starts_with("/Library"), "must not be system-wide");
    }
}
