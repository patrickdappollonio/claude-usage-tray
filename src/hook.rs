//! Hook lifecycle: the tray binary *is* the statusline command.
//!
//! There is no shell snippet to install any more. `hook install` is a
//! `settings.json` read-modify-write that points `statusLine.command` at
//! `<this binary> statusline`, optionally wrapping whatever command was there
//! before with `--exec '<original>'`. `hook uninstall` puts the original back
//! (or removes the key), `hook status` reports what is currently wired up.
//!
//! Everything here is parameterized by a config-directory path, so the tests
//! run entirely inside temp directories and never read or write the real
//! `~/.claude`. Nothing panics: failures are `io::Error`s the CLI turns into a
//! message and a nonzero exit.
//!
//! See `docs/superpowers/specs/2026-08-13-claude-usage-tray-design.md`,
//! "Hook lifecycle: binary-as-statusline".

use crate::source::{self, SnapshotState};
use std::io;
use std::path::{Path, PathBuf};

/// Claude Code's settings file inside the config directory.
pub const SETTINGS_FILE_NAME: &str = "settings.json";

/// Our one-time backup of it. "One-time" is the point: a second install must
/// not overwrite the pristine copy with an already-modified one.
pub const SETTINGS_BACKUP_FILE_NAME: &str = "settings.json.bak-usage-tray";

/// Suffix used when backing up a statusline *script* before stripping v1 hook
/// blocks out of it.
pub const SCRIPT_BACKUP_SUFFIX: &str = ".bak-usage-tray";

/// The subcommand token that identifies a `statusLine.command` as ours. Chosen
/// over matching the binary name so that a renamed or relocated binary is still
/// recognized (and refreshed) rather than wrapped a second time.
const MARKER_ARG: &str = "statusline";

// ---------------------------------------------------------------------------
// Command strings
// ---------------------------------------------------------------------------

/// A `statusLine.command` recognized as one of ours.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OurCommand {
    /// The binary path recorded in settings.json.
    pub exe: String,
    /// The user's own statusline command, if we are wrapping one.
    pub original: Option<String>,
}

/// Splits a command string the way `sh` would, for the small subset that can
/// appear here: whitespace separation, single quotes, double quotes and
/// backslash escapes. `None` means the string has an unterminated quote — in
/// which case we decline to interpret it at all rather than guess.
pub fn shell_split(command: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            c if c.is_whitespace() => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            '\'' => {
                has_token = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => current.push(c),
                        None => return None,
                    }
                }
            }
            '"' => {
                has_token = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            // Only these are special inside double quotes; a
                            // backslash before anything else is literal.
                            Some(c @ ('"' | '\\' | '$' | '`')) => current.push(c),
                            Some(c) => {
                                current.push('\\');
                                current.push(c);
                            }
                            None => return None,
                        },
                        Some(c) => current.push(c),
                        None => return None,
                    }
                }
            }
            '\\' => {
                has_token = true;
                match chars.next() {
                    Some(c) => current.push(c),
                    None => return None,
                }
            }
            c => {
                has_token = true;
                current.push(c);
            }
        }
    }
    if has_token {
        tokens.push(current);
    }
    Some(tokens)
}

/// Wraps `word` in single quotes so a shell passes it through verbatim.
pub fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// True when `word` is safe to hand to a shell unquoted.
fn is_plain(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/:=@+,%~".contains(c))
}

/// Quotes only what needs it, so the common case stays readable in
/// settings.json: `/home/me/bin/claude-usage-tray statusline`.
fn quote_if_needed(word: &str) -> String {
    if is_plain(word) {
        word.to_string()
    } else {
        shell_quote(word)
    }
}

/// Builds the `statusLine.command` value for this binary, optionally wrapping
/// the user's existing command.
pub fn build_command(exe: &str, original: Option<&str>) -> String {
    match original {
        Some(original) if !original.trim().is_empty() => format!(
            "{} {MARKER_ARG} --exec {}",
            quote_if_needed(exe),
            shell_quote(original)
        ),
        _ => format!("{} {MARKER_ARG}", quote_if_needed(exe)),
    }
}

/// Recognizes one of our own commands and recovers the wrapped original.
/// Recognition is by the `statusline` argument, not by the binary's name.
pub fn parse_our_command(command: &str) -> Option<OurCommand> {
    let tokens = shell_split(command)?;
    if tokens.len() < 2 || tokens[1] != MARKER_ARG {
        return None;
    }
    let original = match tokens.get(2).map(String::as_str) {
        Some("--exec") => tokens.get(3).cloned().filter(|o| !o.trim().is_empty()),
        _ => None,
    };
    Some(OurCommand {
        exe: tokens[0].clone(),
        original,
    })
}

// ---------------------------------------------------------------------------
// Legacy (v1) shell-block cleanup
// ---------------------------------------------------------------------------

/// Strips previously injected hook blocks out of a statusline script.
///
/// Recognizes the v1 markers (`# --- claude-usage-tray hook` …
/// `# --- end claude-usage-tray hook ---`) and the versioned form
/// (`# >>> claude-usage-tray hook v<N> >>>` … `# <<< claude-usage-tray hook <<<`).
///
/// `None` means "leave this file alone": either there was nothing to strip, or
/// a block was opened and never closed, in which case guessing where it ends
/// risks eating the user's own code.
pub fn strip_legacy_blocks(body: &str) -> Option<String> {
    #[derive(PartialEq)]
    enum Kind {
        V1,
        Versioned,
    }

    let mut out = String::with_capacity(body.len());
    let mut skipping: Option<Kind> = None;
    let mut removed_any = false;

    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();
        match &skipping {
            Some(kind) => {
                let ends = match kind {
                    Kind::V1 => trimmed.starts_with("# --- end claude-usage-tray hook"),
                    Kind::Versioned => trimmed.starts_with("# <<< claude-usage-tray hook"),
                };
                if ends {
                    skipping = None;
                }
            }
            None => {
                if trimmed.starts_with("# --- claude-usage-tray hook") {
                    skipping = Some(Kind::V1);
                    removed_any = true;
                } else if trimmed.starts_with("# >>> claude-usage-tray hook") {
                    skipping = Some(Kind::Versioned);
                    removed_any = true;
                } else {
                    out.push_str(line);
                }
            }
        }
    }

    if !removed_any || skipping.is_some() {
        return None;
    }
    Some(out)
}

/// Resolves the file a command's first word refers to, expanding a leading
/// `~/`. Returns `None` when the command does not name an existing file (it is
/// `jq`-through-a-pipe, a builtin, a one-liner, …) — there is then nothing to
/// clean.
fn script_path_of(command: &str) -> Option<PathBuf> {
    let first = shell_split(command)?.into_iter().next()?;
    let path = match first.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()?.join(rest),
        None => PathBuf::from(first),
    };
    path.is_file().then_some(path)
}

/// Strips v1 hook blocks out of the script a command points at, backing the
/// script up first. `Ok(None)` means nothing needed cleaning.
fn clean_legacy_script(command: &str) -> io::Result<Option<PathBuf>> {
    let Some(path) = script_path_of(command) else {
        return Ok(None);
    };
    let Ok(body) = std::fs::read_to_string(&path) else {
        // A binary or unreadable statusline program: not something we injected
        // into, so not something to clean.
        return Ok(None);
    };
    let Some(cleaned) = strip_legacy_blocks(&body) else {
        return Ok(None);
    };
    let backup = append_suffix(&path, SCRIPT_BACKUP_SUFFIX);
    backup_once(&path, &backup)?;
    write_atomic(&path, cleaned.as_bytes())?;
    Ok(Some(path))
}

/// `foo.sh` + `.bak-usage-tray` → `foo.sh.bak-usage-tray` (unlike
/// `Path::with_extension`, which would eat `.sh`).
fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

// ---------------------------------------------------------------------------
// settings.json I/O
// ---------------------------------------------------------------------------

/// Reads settings.json as a JSON object. A missing file is an empty object; a
/// file that is not valid JSON (or not an object) is an error, because
/// overwriting it would destroy settings we cannot even see.
fn read_settings(path: &Path) -> io::Result<serde_json::Value> {
    let body = match std::fs::read_to_string(path) {
        Ok(body) => body,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(serde_json::Value::Object(serde_json::Map::new()));
        }
        Err(err) => return Err(err),
    };
    if body.trim().is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(value) if value.is_object() => Ok(value),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is not a JSON object; refusing to modify it",
                path.display()
            ),
        )),
        Err(err) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not valid JSON ({err}); refusing to modify it", path.display()),
        )),
    }
}

/// Pretty-prints settings.json back out, atomically (temp file + rename in the
/// same directory) so Claude Code never reads a half-written file.
fn write_settings(path: &Path, settings: &serde_json::Value) -> io::Result<()> {
    let mut body = serde_json::to_string_pretty(settings)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    body.push('\n');
    write_atomic(path, body.as_bytes())
}

/// Temp file in the same directory, then rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let temp = append_suffix(path, ".usage-tray-tmp");
    std::fs::write(&temp, bytes)?;
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// Copies `path` to `backup`, but only if `backup` does not exist yet: the
/// first backup is the pristine one, and a second install must not clobber it
/// with an already-modified copy. Returns whether a backup was created.
fn backup_once(path: &Path, backup: &Path) -> io::Result<bool> {
    if backup.exists() || !path.exists() {
        return Ok(false);
    }
    std::fs::copy(path, backup)?;
    Ok(true)
}

/// Deletes `path` if it is there. Returns whether something was removed.
fn remove_if_present(path: &Path) -> io::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// Current `statusLine.command`, if it is a string.
fn command_of(settings: &serde_json::Value) -> Option<String> {
    settings
        .get("statusLine")?
        .get("command")?
        .as_str()
        .map(str::to_string)
}

/// Sets `statusLine.command`, preserving any other keys the user has under
/// `statusLine` (`padding`, for instance) and forcing `type: "command"`, which
/// is the only type this key supports.
fn set_command(settings: &mut serde_json::Value, command: &str) {
    let root = match settings.as_object_mut() {
        Some(root) => root,
        // read_settings guarantees an object; this is belt and braces.
        None => {
            *settings = serde_json::Value::Object(serde_json::Map::new());
            settings.as_object_mut().expect("just replaced with an object")
        }
    };
    let entry = root
        .entry("statusLine")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !entry.is_object() {
        *entry = serde_json::Value::Object(serde_json::Map::new());
    }
    let entry = entry.as_object_mut().expect("statusLine is an object");
    entry.insert("type".into(), serde_json::Value::String("command".into()));
    entry.insert(
        "command".into(),
        serde_json::Value::String(command.to_string()),
    );
}

/// Removes the whole `statusLine` key.
fn remove_statusline(settings: &mut serde_json::Value) {
    if let Some(root) = settings.as_object_mut() {
        root.remove("statusLine");
    }
}

// ---------------------------------------------------------------------------
// install / uninstall / status
// ---------------------------------------------------------------------------

/// What `hook install` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallReport {
    pub settings_path: PathBuf,
    /// The `statusLine.command` now recorded.
    pub command: String,
    /// The user's own command, if we are wrapping one.
    pub wrapped: Option<String>,
    /// Whether this run created the settings.json backup (false on re-install:
    /// the original backup is kept).
    pub created_backup: bool,
    /// A statusline script we stripped v1 hook blocks out of.
    pub cleaned_script: Option<PathBuf>,
    /// Whether an obsolete v1 cache file was deleted.
    pub removed_legacy_cache: bool,
    /// Whether the previous command was already one of ours (a refresh).
    pub refreshed: bool,
}

/// Points `statusLine.command` at `exe statusline`, wrapping whatever was
/// there before. Idempotent: re-running against an already-installed settings
/// file refreshes the recorded binary path instead of wrapping ourselves.
pub fn install_in(config_dir: &Path, exe: &Path) -> io::Result<InstallReport> {
    let settings_path = config_dir.join(SETTINGS_FILE_NAME);
    let mut settings = read_settings(&settings_path)?;

    let existing = command_of(&settings);
    let ours = existing.as_deref().and_then(parse_our_command);
    let refreshed = ours.is_some();
    let wrapped = match &ours {
        // Already installed: keep wrapping whatever it was already wrapping,
        // never the wrapper itself.
        Some(ours) => ours.original.clone(),
        None => existing.filter(|command| !command.trim().is_empty()),
    };

    let cleaned_script = match wrapped.as_deref() {
        Some(command) => clean_legacy_script(command)?,
        None => None,
    };

    let created_backup = backup_once(
        &settings_path,
        &config_dir.join(SETTINGS_BACKUP_FILE_NAME),
    )?;

    let command = build_command(&exe.to_string_lossy(), wrapped.as_deref());
    set_command(&mut settings, &command);
    write_settings(&settings_path, &settings)?;

    let removed_legacy_cache =
        remove_if_present(&config_dir.join(source::LEGACY_CACHE_FILE_NAME))?;

    Ok(InstallReport {
        settings_path,
        command,
        wrapped,
        created_backup,
        cleaned_script,
        removed_legacy_cache,
        refreshed,
    })
}

impl InstallReport {
    /// Human-readable summary for the CLI.
    pub fn render(&self) -> String {
        let mut lines = vec![format!(
            "{} statusline hook in {}",
            if self.refreshed { "Refreshed" } else { "Installed" },
            self.settings_path.display()
        )];
        lines.push(format!("  statusLine.command = {}", self.command));
        match &self.wrapped {
            Some(original) => lines.push(format!("  wrapping your command: {original}")),
            None => lines.push(
                "  no previous statusline was set, so nothing is printed to the statusline"
                    .to_string(),
            ),
        }
        if self.created_backup {
            lines.push(format!("  backup: {}", SETTINGS_BACKUP_FILE_NAME));
        }
        if let Some(script) = &self.cleaned_script {
            lines.push(format!(
                "  removed old shell hook blocks from {} (backup: {}{})",
                script.display(),
                script.display(),
                SCRIPT_BACKUP_SUFFIX
            ));
        }
        if self.removed_legacy_cache {
            lines.push(format!(
                "  deleted the obsolete {} cache",
                source::LEGACY_CACHE_FILE_NAME
            ));
        }
        lines.push("  data appears the next time Claude Code refreshes the statusline".into());
        lines.join("\n")
    }
}

/// What `hook uninstall` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UninstallReport {
    pub settings_path: PathBuf,
    /// Whether the settings file was pointing at us at all.
    pub was_installed: bool,
    /// The command we put back, if we had been wrapping one.
    pub restored: Option<String>,
    /// Whether the `statusLine` key was removed entirely.
    pub removed_statusline: bool,
    /// Whether a cache file was deleted.
    pub removed_cache: bool,
}

/// Restores the wrapped command (or removes `statusLine` entirely) and deletes
/// the cache file. Leaves a settings file that was never ours untouched.
pub fn uninstall_in(config_dir: &Path) -> io::Result<UninstallReport> {
    let settings_path = config_dir.join(SETTINGS_FILE_NAME);
    let mut settings = read_settings(&settings_path)?;

    let ours = command_of(&settings)
        .as_deref()
        .and_then(parse_our_command);
    let mut restored = None;
    let mut removed_statusline = false;

    if let Some(ours) = &ours {
        backup_once(&settings_path, &config_dir.join(SETTINGS_BACKUP_FILE_NAME))?;
        match &ours.original {
            Some(original) => {
                set_command(&mut settings, original);
                restored = Some(original.clone());
            }
            None => {
                remove_statusline(&mut settings);
                removed_statusline = true;
            }
        }
        write_settings(&settings_path, &settings)?;
    }

    let removed_cache = remove_if_present(&source::cache_path_in(config_dir))?;
    let _ = remove_if_present(&config_dir.join(source::LEGACY_CACHE_FILE_NAME))?;

    Ok(UninstallReport {
        settings_path,
        was_installed: ours.is_some(),
        restored,
        removed_statusline,
        removed_cache,
    })
}

impl UninstallReport {
    pub fn render(&self) -> String {
        let mut lines = Vec::new();
        if self.was_installed {
            lines.push(format!("Removed the statusline hook from {}", self.settings_path.display()));
            match &self.restored {
                Some(original) => lines.push(format!("  restored your command: {original}")),
                None if self.removed_statusline => {
                    lines.push("  removed the statusLine key (there was nothing to restore)".into())
                }
                None => {}
            }
        } else {
            lines.push(format!(
                "No claude-usage-tray statusline hook found in {}",
                self.settings_path.display()
            ));
        }
        if self.removed_cache {
            lines.push("  deleted the cache file".into());
        }
        lines.join("\n")
    }
}

/// What `hook status` found.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusReport {
    pub settings_path: PathBuf,
    /// The `statusLine.command` currently configured, whatever it is.
    pub command: Option<String>,
    /// Whether that command is one of ours.
    pub installed: bool,
    /// The binary path recorded in settings.json (ours only).
    pub recorded_exe: Option<String>,
    /// The wrapped original (ours only).
    pub wrapped: Option<String>,
    pub cache_path: PathBuf,
    pub cache_state: SnapshotState,
    pub cache_written_at: Option<jiff::Timestamp>,
}

/// Inspects a config directory: what settings.json says, and how fresh the
/// cache file is.
pub fn status_in(config_dir: &Path, now: jiff::Timestamp) -> StatusReport {
    let settings_path = config_dir.join(SETTINGS_FILE_NAME);
    let command = read_settings(&settings_path).ok().and_then(|s| command_of(&s));
    let ours = command.as_deref().and_then(parse_our_command);
    let cache_path = source::cache_path_in(config_dir);
    let snapshot = source::read_snapshot(&cache_path, now);
    StatusReport {
        settings_path,
        command,
        installed: ours.is_some(),
        recorded_exe: ours.as_ref().map(|ours| ours.exe.clone()),
        wrapped: ours.and_then(|ours| ours.original),
        cache_path,
        cache_state: snapshot.state,
        cache_written_at: snapshot.written_at,
    }
}

impl StatusReport {
    /// Human-readable summary. `running_exe` is `current_exe()`, so the report
    /// can point out an install that records a different (moved or renamed)
    /// binary.
    pub fn render(&self, running_exe: Option<&Path>, now: jiff::Timestamp) -> String {
        let mut lines = vec![format!("settings: {}", self.settings_path.display())];
        match (&self.command, self.installed) {
            (Some(command), true) => {
                lines.push("hook: installed".into());
                lines.push(format!("  statusLine.command = {command}"));
                match &self.wrapped {
                    Some(original) => lines.push(format!("  wrapping: {original}")),
                    None => lines.push("  wrapping: nothing (statusline prints nothing)".into()),
                }
                if let (Some(recorded), Some(running)) = (&self.recorded_exe, running_exe) {
                    let running = running.to_string_lossy();
                    if recorded.as_str() != running.as_ref() {
                        lines.push(format!(
                            "  recorded binary {recorded} differs from the running {running} \
                             — re-run `hook install` to point it here"
                        ));
                    }
                }
            }
            (Some(command), false) => {
                lines.push("hook: NOT installed".into());
                lines.push(format!("  statusLine.command = {command}"));
            }
            (None, _) => {
                lines.push("hook: NOT installed (no statusLine.command configured)".into());
            }
        }
        lines.push(format!("cache: {}", self.cache_path.display()));
        lines.push(format!("  {}", self.cache_freshness(now)));
        lines.join("\n")
    }

    /// One line describing the cache file's age.
    pub fn cache_freshness(&self, now: jiff::Timestamp) -> String {
        match (&self.cache_state, self.cache_written_at) {
            (SnapshotState::Missing, _) => "no cache file yet".to_string(),
            (state, Some(at)) => {
                let age = (now.as_second() - at.as_second()).max(0);
                let label = if *state == SnapshotState::Stale {
                    "stale"
                } else {
                    "fresh"
                };
                format!("{label}, last written {age} s ago")
            }
            (_, None) => "present, age unknown".to_string(),
        }
    }
}

/// The toast shown after the tray menu's `Install hook` item runs.
///
/// A re-install says so rather than claiming a fresh one: somebody who clicks
/// the item because the tray shows no data needs to know that the entry was
/// *already* there — the wording that promises data "next time Claude Code
/// refreshes" would send them off waiting for something that is not coming.
/// This mirrors the CLI's own `Installed` / `Refreshed` distinction.
pub fn install_toast(result: &io::Result<InstallReport>) -> String {
    match result {
        Ok(report) if report.refreshed => "Hook already installed — entry refreshed".to_string(),
        Ok(_) => "Hook installed — data appears next time Claude Code refreshes".to_string(),
        Err(err) => format!("Hook install failed: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read file")
    }

    fn settings_json(dir: &Path) -> serde_json::Value {
        serde_json::from_str(&read(&dir.join(SETTINGS_FILE_NAME))).expect("valid json")
    }

    fn write_settings_file(dir: &Path, body: &str) {
        std::fs::create_dir_all(dir).expect("create dir");
        std::fs::write(dir.join(SETTINGS_FILE_NAME), body).expect("write settings");
    }

    fn exe() -> PathBuf {
        PathBuf::from("/home/me/bin/claude-usage-tray")
    }

    // -- command strings ---------------------------------------------------

    #[test]
    fn build_command_without_an_original_is_just_the_subcommand() {
        assert_eq!(
            build_command("/home/me/bin/claude-usage-tray", None),
            "/home/me/bin/claude-usage-tray statusline"
        );
        assert_eq!(
            build_command("/home/me/bin/claude-usage-tray", Some("   ")),
            "/home/me/bin/claude-usage-tray statusline"
        );
    }

    #[test]
    fn build_command_wraps_the_original_in_single_quotes() {
        assert_eq!(
            build_command("/opt/tray", Some("~/.claude/statusline.sh")),
            "/opt/tray statusline --exec '~/.claude/statusline.sh'"
        );
    }

    #[test]
    fn build_command_quotes_an_exe_path_with_spaces() {
        assert_eq!(
            build_command("/home/my name/tray", None),
            "'/home/my name/tray' statusline"
        );
    }

    #[test]
    fn build_command_escapes_single_quotes_in_the_original() {
        let command = build_command("/opt/tray", Some("echo 'hi there'"));
        assert_eq!(command, r"/opt/tray statusline --exec 'echo '\''hi there'\'''");
        // And it round-trips through the parser.
        let parsed = parse_our_command(&command).expect("ours");
        assert_eq!(parsed.original.as_deref(), Some("echo 'hi there'"));
    }

    #[test]
    fn parse_our_command_recognizes_the_statusline_argument() {
        let parsed = parse_our_command("/anywhere/renamed-binary statusline").expect("ours");
        assert_eq!(parsed.exe, "/anywhere/renamed-binary");
        assert_eq!(parsed.original, None);
    }

    #[test]
    fn parse_our_command_extracts_a_wrapped_original() {
        let parsed =
            parse_our_command("/opt/tray statusline --exec '/home/me/.claude/line.sh --color'")
                .expect("ours");
        assert_eq!(
            parsed.original.as_deref(),
            Some("/home/me/.claude/line.sh --color")
        );
    }

    #[test]
    fn parse_our_command_rejects_someone_elses_command() {
        assert_eq!(parse_our_command("~/.claude/statusline-command.sh"), None);
        assert_eq!(parse_our_command("bash -c 'echo hi'"), None);
        assert_eq!(parse_our_command(""), None);
        // A script that merely has "statusline" in its *name* is not ours: the
        // marker has to be the second word.
        assert_eq!(parse_our_command("/usr/bin/statusline-thing --fancy"), None);
    }

    #[test]
    fn parse_our_command_declines_an_unterminated_quote() {
        assert_eq!(parse_our_command("/opt/tray statusline --exec 'oops"), None);
    }

    #[test]
    fn shell_split_handles_quotes_and_escapes() {
        assert_eq!(
            shell_split(r#"a "b c" 'd e' f\ g"#).expect("splits"),
            vec!["a", "b c", "d e", "f g"]
        );
        assert_eq!(shell_split("   ").expect("splits"), Vec::<String>::new());
        assert_eq!(shell_split("'"), None);
    }

    // -- legacy block stripping -------------------------------------------

    const V1_SCRIPT: &str = "#!/bin/bash\n\
        input=$(cat)\n\
        # --- claude-usage-tray hook: tees rate_limits to the tray's cache file ---\n\
        if command -v jq >/dev/null 2>&1; then\n\
        :\n\
        fi\n\
        # --- end claude-usage-tray hook ---\n\
        echo \"my statusline\"\n";

    #[test]
    fn strip_legacy_blocks_removes_the_v1_block() {
        let cleaned = strip_legacy_blocks(V1_SCRIPT).expect("something was stripped");
        assert_eq!(cleaned, "#!/bin/bash\ninput=$(cat)\necho \"my statusline\"\n");
    }

    #[test]
    fn strip_legacy_blocks_removes_versioned_blocks() {
        let body = "before\n\
            # >>> claude-usage-tray hook v2 >>>\n\
            junk\n\
            # <<< claude-usage-tray hook <<<\n\
            after\n";
        assert_eq!(
            strip_legacy_blocks(body).expect("stripped"),
            "before\nafter\n"
        );
    }

    #[test]
    fn strip_legacy_blocks_removes_several_blocks() {
        let body = "a\n# >>> claude-usage-tray hook v1 >>>\nx\n# <<< claude-usage-tray hook <<<\n\
            b\n# >>> claude-usage-tray hook v2 >>>\ny\n# <<< claude-usage-tray hook <<<\nc\n";
        assert_eq!(strip_legacy_blocks(body).expect("stripped"), "a\nb\nc\n");
    }

    #[test]
    fn strip_legacy_blocks_leaves_untouched_scripts_alone() {
        assert_eq!(strip_legacy_blocks("#!/bin/sh\necho hi\n"), None);
    }

    #[test]
    fn strip_legacy_blocks_refuses_an_unterminated_block() {
        // Half-edited script: guessing where the block ends could delete the
        // user's own code, so we do nothing.
        let body = "a\n# --- claude-usage-tray hook ---\nb\nc\n";
        assert_eq!(strip_legacy_blocks(body), None);
    }

    // -- install -----------------------------------------------------------

    #[test]
    fn install_into_a_config_dir_without_settings_creates_the_key() {
        let temp = TempDir::new("hook-install-fresh");
        let report = install_in(temp.path(), &exe()).expect("install succeeds");

        assert_eq!(report.command, "/home/me/bin/claude-usage-tray statusline");
        assert_eq!(report.wrapped, None);
        assert!(!report.refreshed);
        assert!(!report.created_backup, "nothing existed to back up");

        let settings = settings_json(temp.path());
        assert_eq!(settings["statusLine"]["type"], "command");
        assert_eq!(
            settings["statusLine"]["command"],
            "/home/me/bin/claude-usage-tray statusline"
        );
    }

    #[test]
    fn install_preserves_every_other_settings_key() {
        let temp = TempDir::new("hook-install-preserve");
        write_settings_file(
            temp.path(),
            r#"{"model":"opus","env":{"FOO":"bar"},"permissions":{"allow":["Bash(ls:*)"]}}"#,
        );
        install_in(temp.path(), &exe()).expect("install succeeds");

        let settings = settings_json(temp.path());
        assert_eq!(settings["model"], "opus");
        assert_eq!(settings["env"]["FOO"], "bar");
        assert_eq!(settings["permissions"]["allow"][0], "Bash(ls:*)");
        assert!(settings["statusLine"]["command"].is_string());
    }

    #[test]
    fn install_wraps_an_existing_statusline_command() {
        let temp = TempDir::new("hook-install-wrap");
        write_settings_file(
            temp.path(),
            r#"{"statusLine":{"type":"command","command":"~/.claude/statusline-command.sh","padding":0}}"#,
        );
        let report = install_in(temp.path(), &exe()).expect("install succeeds");

        assert_eq!(
            report.wrapped.as_deref(),
            Some("~/.claude/statusline-command.sh")
        );
        assert_eq!(
            report.command,
            "/home/me/bin/claude-usage-tray statusline --exec '~/.claude/statusline-command.sh'"
        );
        let settings = settings_json(temp.path());
        assert_eq!(settings["statusLine"]["command"], report.command);
        // Sibling keys under statusLine survive.
        assert_eq!(settings["statusLine"]["padding"], 0);
    }

    #[test]
    fn install_is_idempotent_and_never_double_wraps() {
        let temp = TempDir::new("hook-install-idempotent");
        write_settings_file(
            temp.path(),
            r#"{"statusLine":{"type":"command","command":"~/.claude/line.sh"}}"#,
        );
        let first = install_in(temp.path(), &exe()).expect("first install");
        let second = install_in(temp.path(), &exe()).expect("second install");

        assert_eq!(first.command, second.command);
        assert!(!first.refreshed);
        assert!(second.refreshed);
        assert_eq!(second.wrapped.as_deref(), Some("~/.claude/line.sh"));
        assert_eq!(second.command.matches("statusline").count(), 1);
    }

    #[test]
    fn install_refreshes_a_moved_binary_path() {
        let temp = TempDir::new("hook-install-moved");
        write_settings_file(
            temp.path(),
            r#"{"statusLine":{"type":"command","command":"/old/path/tray statusline --exec '~/.claude/line.sh'"}}"#,
        );
        let report = install_in(temp.path(), &PathBuf::from("/new/path/tray")).expect("install");
        assert!(report.refreshed);
        assert_eq!(
            report.command,
            "/new/path/tray statusline --exec '~/.claude/line.sh'"
        );
    }

    #[test]
    fn install_backs_settings_up_once_and_keeps_the_original_backup() {
        let temp = TempDir::new("hook-install-backup");
        let pristine = r#"{"statusLine":{"type":"command","command":"~/.claude/line.sh"}}"#;
        write_settings_file(temp.path(), pristine);

        let first = install_in(temp.path(), &exe()).expect("first install");
        assert!(first.created_backup);
        let backup = temp.path().join(SETTINGS_BACKUP_FILE_NAME);
        assert_eq!(read(&backup), pristine);

        let second = install_in(temp.path(), &exe()).expect("second install");
        assert!(!second.created_backup, "must not re-back-up");
        assert_eq!(read(&backup), pristine, "the pristine backup is preserved");
    }

    #[test]
    fn install_refuses_to_touch_invalid_settings_json() {
        let temp = TempDir::new("hook-install-invalid");
        write_settings_file(temp.path(), "{ not json");
        let err = install_in(temp.path(), &exe()).expect_err("must refuse");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // And it left the file exactly as it found it.
        assert_eq!(read(&temp.path().join(SETTINGS_FILE_NAME)), "{ not json");
    }

    #[test]
    fn install_strips_v1_hook_blocks_from_the_wrapped_script() {
        let temp = TempDir::new("hook-install-legacy-script");
        let script = temp.path().join("statusline-command.sh");
        std::fs::write(&script, V1_SCRIPT).expect("write script");
        write_settings_file(
            temp.path(),
            &format!(
                r#"{{"statusLine":{{"type":"command","command":"{}"}}}}"#,
                script.display()
            ),
        );

        let report = install_in(temp.path(), &exe()).expect("install");
        assert_eq!(report.cleaned_script.as_deref(), Some(script.as_path()));
        let cleaned = read(&script);
        assert!(!cleaned.contains("claude-usage-tray hook"));
        assert!(cleaned.contains("echo \"my statusline\""));
        // The pre-cleanup script is recoverable.
        let backup = append_suffix(&script, SCRIPT_BACKUP_SUFFIX);
        assert_eq!(read(&backup), V1_SCRIPT);
    }

    #[test]
    fn install_leaves_a_clean_script_and_its_backup_alone() {
        let temp = TempDir::new("hook-install-clean-script");
        let script = temp.path().join("line.sh");
        std::fs::write(&script, "#!/bin/sh\necho hi\n").expect("write script");
        write_settings_file(
            temp.path(),
            &format!(
                r#"{{"statusLine":{{"type":"command","command":"{}"}}}}"#,
                script.display()
            ),
        );
        let report = install_in(temp.path(), &exe()).expect("install");
        assert_eq!(report.cleaned_script, None);
        assert!(!append_suffix(&script, SCRIPT_BACKUP_SUFFIX).exists());
    }

    #[test]
    fn install_deletes_the_obsolete_v1_cache() {
        let temp = TempDir::new("hook-install-legacy-cache");
        let legacy = temp.path().join(source::LEGACY_CACHE_FILE_NAME);
        std::fs::write(&legacy, "{}").expect("write legacy cache");

        let report = install_in(temp.path(), &exe()).expect("install");
        assert!(report.removed_legacy_cache);
        assert!(!legacy.exists());

        // Second run: nothing left to remove, and that is not an error.
        let report = install_in(temp.path(), &exe()).expect("install");
        assert!(!report.removed_legacy_cache);
    }

    #[test]
    fn install_report_renders_the_essentials() {
        let temp = TempDir::new("hook-install-render");
        write_settings_file(
            temp.path(),
            r#"{"statusLine":{"type":"command","command":"~/.claude/line.sh"}}"#,
        );
        let rendered = install_in(temp.path(), &exe()).expect("install").render();
        assert!(rendered.contains("Installed statusline hook"));
        assert!(rendered.contains("--exec '~/.claude/line.sh'"));
        assert!(rendered.contains("wrapping your command: ~/.claude/line.sh"));
    }

    // -- uninstall ---------------------------------------------------------

    #[test]
    fn uninstall_restores_the_wrapped_command() {
        let temp = TempDir::new("hook-uninstall-restore");
        write_settings_file(
            temp.path(),
            r#"{"model":"opus","statusLine":{"type":"command","command":"~/.claude/line.sh"}}"#,
        );
        install_in(temp.path(), &exe()).expect("install");
        let report = uninstall_in(temp.path()).expect("uninstall");

        assert!(report.was_installed);
        assert_eq!(report.restored.as_deref(), Some("~/.claude/line.sh"));
        let settings = settings_json(temp.path());
        assert_eq!(settings["statusLine"]["command"], "~/.claude/line.sh");
        assert_eq!(settings["model"], "opus", "other keys survive");
    }

    #[test]
    fn uninstall_removes_the_statusline_key_when_there_was_nothing_to_restore() {
        let temp = TempDir::new("hook-uninstall-remove");
        write_settings_file(temp.path(), r#"{"model":"opus"}"#);
        install_in(temp.path(), &exe()).expect("install");
        let report = uninstall_in(temp.path()).expect("uninstall");

        assert!(report.removed_statusline);
        let settings = settings_json(temp.path());
        assert!(settings.get("statusLine").is_none());
        assert_eq!(settings["model"], "opus");
    }

    #[test]
    fn uninstall_deletes_the_cache_file() {
        let temp = TempDir::new("hook-uninstall-cache");
        install_in(temp.path(), &exe()).expect("install");
        let cache = source::cache_path_in(temp.path());
        source::write_cache(&cache, b"{}").expect("write cache");

        let report = uninstall_in(temp.path()).expect("uninstall");
        assert!(report.removed_cache);
        assert!(!cache.exists());
    }

    #[test]
    fn uninstall_leaves_someone_elses_statusline_alone() {
        let temp = TempDir::new("hook-uninstall-foreign");
        write_settings_file(
            temp.path(),
            "{\n  \"statusLine\": {\n    \"command\": \"~/.claude/line.sh\"\n  }\n}",
        );
        let before = read(&temp.path().join(SETTINGS_FILE_NAME));
        let report = uninstall_in(temp.path()).expect("uninstall");

        assert!(!report.was_installed);
        assert_eq!(
            read(&temp.path().join(SETTINGS_FILE_NAME)),
            before,
            "an untouched settings file must not even be reformatted"
        );
    }

    #[test]
    fn uninstall_without_a_settings_file_does_not_create_one() {
        let temp = TempDir::new("hook-uninstall-absent");
        let report = uninstall_in(temp.path()).expect("uninstall");
        assert!(!report.was_installed);
        assert!(!temp.path().join(SETTINGS_FILE_NAME).exists());
        assert!(report.render().contains("No claude-usage-tray"));
    }

    #[test]
    fn install_then_uninstall_round_trips_the_settings_file() {
        let temp = TempDir::new("hook-roundtrip");
        write_settings_file(
            temp.path(),
            "{\n  \"statusLine\": {\n    \"type\": \"command\",\n    \"command\": \"~/.claude/line.sh\"\n  }\n}\n",
        );
        let before: serde_json::Value = settings_json(temp.path());
        install_in(temp.path(), &exe()).expect("install");
        uninstall_in(temp.path()).expect("uninstall");
        assert_eq!(settings_json(temp.path()), before);
    }

    // -- status ------------------------------------------------------------

    fn ts(secs: i64) -> jiff::Timestamp {
        jiff::Timestamp::from_second(secs).expect("valid timestamp")
    }

    #[test]
    fn status_reports_an_installed_hook_with_its_wrapped_command() {
        let temp = TempDir::new("hook-status-installed");
        write_settings_file(
            temp.path(),
            r#"{"statusLine":{"type":"command","command":"~/.claude/line.sh"}}"#,
        );
        install_in(temp.path(), &exe()).expect("install");

        let report = status_in(temp.path(), jiff::Timestamp::now());
        assert!(report.installed);
        assert_eq!(report.recorded_exe.as_deref(), Some(exe().to_str().unwrap()));
        assert_eq!(report.wrapped.as_deref(), Some("~/.claude/line.sh"));
        assert_eq!(report.cache_state, SnapshotState::Missing);

        let rendered = report.render(Some(&exe()), jiff::Timestamp::now());
        assert!(rendered.contains("hook: installed"));
        assert!(rendered.contains("wrapping: ~/.claude/line.sh"));
        assert!(rendered.contains("no cache file yet"));
        assert!(!rendered.contains("differs from the running"));
    }

    #[test]
    fn status_flags_a_recorded_binary_that_is_not_the_running_one() {
        let temp = TempDir::new("hook-status-moved");
        install_in(temp.path(), &exe()).expect("install");
        let report = status_in(temp.path(), jiff::Timestamp::now());
        let rendered = report.render(Some(Path::new("/somewhere/else/tray")), jiff::Timestamp::now());
        assert!(rendered.contains("differs from the running"));
    }

    #[test]
    fn status_reports_a_foreign_statusline_as_not_installed() {
        let temp = TempDir::new("hook-status-foreign");
        write_settings_file(
            temp.path(),
            r#"{"statusLine":{"type":"command","command":"~/.claude/line.sh"}}"#,
        );
        let report = status_in(temp.path(), jiff::Timestamp::now());
        assert!(!report.installed);
        let rendered = report.render(None, jiff::Timestamp::now());
        assert!(rendered.contains("hook: NOT installed"));
        assert!(rendered.contains("~/.claude/line.sh"));
    }

    #[test]
    fn status_on_an_empty_config_dir_says_nothing_is_configured() {
        let temp = TempDir::new("hook-status-empty");
        let report = status_in(temp.path(), jiff::Timestamp::now());
        assert!(!report.installed);
        assert_eq!(report.command, None);
        assert!(
            report
                .render(None, jiff::Timestamp::now())
                .contains("no statusLine.command configured")
        );
    }

    #[test]
    fn status_reports_cache_freshness_from_its_age() {
        let temp = TempDir::new("hook-status-cache");
        source::write_cache(&source::cache_path_in(temp.path()), b"{}").expect("write cache");
        let report = status_in(temp.path(), jiff::Timestamp::now());
        assert_eq!(report.cache_state, SnapshotState::Fresh);
        let at = report.cache_written_at.expect("mtime known");
        assert!(report.cache_freshness(at).starts_with("fresh, last written"));

        let mut stale = report.clone();
        stale.cache_state = SnapshotState::Stale;
        stale.cache_written_at = Some(ts(1_700_000_000));
        assert_eq!(
            stale.cache_freshness(ts(1_700_000_700)),
            "stale, last written 700 s ago"
        );
    }

    #[test]
    fn install_toast_text_matches_the_spec() {
        let temp = TempDir::new("hook-toast");
        let ok = install_in(temp.path(), &exe());
        assert_eq!(
            install_toast(&ok),
            "Hook installed — data appears next time Claude Code refreshes"
        );

        let err: io::Result<InstallReport> =
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "nope"));
        assert_eq!(install_toast(&err), "Hook install failed: nope");
    }

    #[test]
    fn install_toast_says_so_when_the_hook_was_already_installed() {
        let temp = TempDir::new("hook-toast-refresh");
        let first = install_in(temp.path(), &exe()).expect("first install");
        assert!(!first.refreshed);

        let again = install_in(temp.path(), &exe());
        assert!(again.as_ref().expect("second install").refreshed);
        assert_eq!(
            install_toast(&again),
            "Hook already installed — entry refreshed"
        );
    }
}
