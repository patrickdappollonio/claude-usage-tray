//! Noticing that the program on disk is no longer the program that is running.
//!
//! A package upgrade replaces the binary underneath a running tray, and the
//! running tray goes on being the old version until somebody restarts it. This
//! module is how it finds out: it records the identity of its own executable at
//! startup and re-checks it on every poll tick.
//!
//! Identity is the *path string* plus `(dev, ino, mtime)` of what that path
//! points at. The path matters as much as the numbers: on Linux
//! `/proc/self/exe` still resolves after an upgrade, but to a now-unlinked
//! inode with ` (deleted)` appended, so following it would report that nothing
//! ever changes. Re-`stat`ing the recorded path instead sees the new file that
//! the package manager renamed into place, which is exactly the event worth
//! reporting.
//!
//! Everything here is a pure function of a path, so the tests replace a real
//! scratch file and watch the detection fire.

use std::path::{Path, PathBuf};

/// The path to record anywhere this program has to be found again later: the
/// statusline hook, the autostart entry, the upgrade watch.
///
/// Usually that is just `current_exe()`, but a Homebrew install needs more
/// care. On Linux `current_exe()` always resolves symlinks, so it reports
/// `<prefix>/Cellar/<name>/<version>/bin/<binary>`. That path stops existing
/// once `brew upgrade` cleans up the old version. See [`stable_path`].
pub fn current() -> std::io::Result<PathBuf> {
    std::env::current_exe().map(|exe| stable_path(&exe))
}

/// Maps a binary inside a versioned Homebrew directory to the link Homebrew
/// keeps pointing at the installed version, and returns anything else
/// unchanged.
///
/// The versioned layouts are `<prefix>/Cellar/<name>/<version>/…` (formulae)
/// and `<prefix>/Caskroom/<name>/<version>/…` (casks). The candidates are
/// `<prefix>/bin/<binary>`, the one on `PATH`, then for formulae
/// `<prefix>/opt/<name>/…`. A candidate is only used when it resolves to the
/// very file `exe` names, so a link that belongs to some other installation
/// or version is never picked.
pub fn stable_path(exe: &Path) -> PathBuf {
    let Some(file_name) = exe.file_name() else {
        return exe.to_path_buf();
    };
    let Ok(real) = std::fs::canonicalize(exe) else {
        return exe.to_path_buf();
    };
    let components: Vec<_> = exe.components().collect();
    // `…/<Cellar|Caskroom>/<name>/<version>/<rest…>`: the keg directory sits
    // three components before the rest.
    for (index, component) in components.iter().enumerate() {
        let keg_root = component.as_os_str();
        if keg_root != "Cellar" && keg_root != "Caskroom" {
            continue;
        }
        let (Some(name), Some(_version)) = (components.get(index + 1), components.get(index + 2))
        else {
            continue;
        };
        let prefix: PathBuf = components[..index].iter().collect();
        let rest: PathBuf = components[index + 3..].iter().collect();

        let mut candidates = vec![prefix.join("bin").join(file_name)];
        if keg_root == "Cellar" && !rest.as_os_str().is_empty() {
            candidates.push(prefix.join("opt").join(name).join(&rest));
        }
        if let Some(found) = candidates
            .into_iter()
            .find(|candidate| std::fs::canonicalize(candidate).is_ok_and(|c| c == real))
        {
            return found;
        }
    }
    exe.to_path_buf()
}

/// What `stat` says about the file a path points at, reduced to the fields
/// that change when a file is replaced or rewritten.
///
/// `(dev, ino)` catches the usual upgrade (a new file renamed over the old
/// one); `mtime` catches an in-place rewrite that happens to reuse the inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BinaryIdentity {
    dev: u64,
    ino: u64,
    mtime_secs: i64,
    mtime_nanos: i64,
}

/// Reads the identity of whatever `path` points at now, or `None` when it
/// cannot be stat'ed — which, mid-upgrade, is a perfectly ordinary momentary
/// state and not news.
pub fn identify(path: &Path) -> Option<BinaryIdentity> {
    use std::os::unix::fs::MetadataExt;

    // Deliberately following symlinks: `/usr/local/bin/tray` may be a link
    // into a versioned directory, and the interesting question is always
    // "which file would run if I started it again".
    let meta = std::fs::metadata(path).ok()?;
    Some(BinaryIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
        mtime_secs: meta.mtime(),
        mtime_nanos: meta.mtime_nsec(),
    })
}

/// Watches one path for the moment its contents are swapped out.
///
/// The detection latches: [`check`](BinaryWatch::check) returns true exactly
/// once, on the first tick that sees a different file. Everything downstream
/// (the toast, the menu row) is a one-time announcement, and an upgrade that
/// lands in two stages must not produce two of them.
#[derive(Debug)]
pub struct BinaryWatch {
    path: PathBuf,
    /// The identity to compare against. `None` means no successful `stat` yet:
    /// the first one that succeeds becomes the baseline rather than being
    /// reported as a change, since there is nothing to have changed *from*.
    baseline: Option<BinaryIdentity>,
    swapped: bool,
}

impl BinaryWatch {
    /// Starts watching `path`, recording what is there right now.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let baseline = identify(&path);
        BinaryWatch {
            path,
            baseline,
            swapped: false,
        }
    }

    /// The path being watched: the executable a restart should run.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the swap has already been announced. The tray never asks — it
    /// acts on the one `true` that [`check`](BinaryWatch::check) returns — but
    /// the latch is worth pinning down in the tests.
    #[cfg(test)]
    pub fn swapped(&self) -> bool {
        self.swapped
    }

    /// Re-stats the path. True only on the first tick that finds a different
    /// file there.
    pub fn check(&mut self) -> bool {
        if self.swapped {
            return false;
        }
        let Some(current) = identify(&self.path) else {
            // Gone right now. An upgrade in progress looks like this for a
            // moment; the tick that finds the replacement is the one that
            // reports it.
            return false;
        };
        match self.baseline {
            None => {
                self.baseline = Some(current);
                false
            }
            Some(baseline) if baseline != current => {
                self.swapped = true;
                true
            }
            Some(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Writes `body` to `path` through a fresh file renamed into place, the
    /// way a package manager installs a binary: new inode, same path.
    fn install(path: &Path, body: &[u8]) {
        let temp = path.with_extension("incoming");
        std::fs::write(&temp, body).expect("write");
        std::fs::rename(&temp, path).expect("rename into place");
    }

    #[test]
    fn an_unchanged_file_is_never_reported() {
        let temp = TempDir::new("binary-unchanged");
        let exe = temp.path().join("tray");
        install(&exe, b"v1");

        let mut watch = BinaryWatch::new(&exe);
        for _ in 0..5 {
            assert!(!watch.check(), "nothing was installed");
        }
        assert!(!watch.swapped());
    }

    #[test]
    fn replacing_the_file_is_detected_once() {
        let temp = TempDir::new("binary-replaced");
        let exe = temp.path().join("tray");
        install(&exe, b"v1");

        let mut watch = BinaryWatch::new(&exe);
        assert!(!watch.check());

        install(&exe, b"v2-longer");
        assert!(watch.check(), "the replacement must be detected");
        assert!(watch.swapped());
        // Latched: the menu row and the toast are one-time announcements.
        assert!(!watch.check(), "the same swap must not be reported twice");

        install(&exe, b"v3");
        assert!(!watch.check(), "a second swap adds nothing to say");
    }

    /// The inode-reusing case: same file, rewritten in place. Only `mtime`
    /// moves, which is why it is part of the identity.
    #[test]
    fn rewriting_the_same_inode_is_detected_through_mtime() {
        let temp = TempDir::new("binary-rewritten");
        let exe = temp.path().join("tray");
        std::fs::write(&exe, b"v1").expect("write");
        let before = identify(&exe).expect("stat");

        let mut watch = BinaryWatch::new(&exe);
        // Move the timestamp explicitly rather than relying on the clock
        // ticking between two writes inside the same test.
        crate::testutil::set_mtime(&exe, 1_000_000_000);
        let after = identify(&exe).expect("stat");
        assert_eq!(
            (before.dev, before.ino),
            (after.dev, after.ino),
            "this test is only meaningful while the inode is reused"
        );

        assert!(watch.check(), "an in-place rewrite is still a new binary");
    }

    /// Mid-upgrade the path can be missing for an instant. That is not a swap:
    /// there is nothing yet to restart into.
    #[test]
    fn a_momentarily_missing_file_is_not_a_swap() {
        let temp = TempDir::new("binary-missing");
        let exe = temp.path().join("tray");
        install(&exe, b"v1");

        let mut watch = BinaryWatch::new(&exe);
        std::fs::remove_file(&exe).expect("remove");
        assert!(!watch.check(), "a gap is not an upgrade");
        assert!(!watch.check());

        install(&exe, b"v2");
        assert!(watch.check(), "the replacement is what gets reported");
    }

    /// Started from a path that could not be stat'ed at all: the first
    /// successful reading becomes the baseline instead of being announced as a
    /// change nobody made.
    #[test]
    fn a_path_that_only_appears_later_becomes_the_baseline() {
        let temp = TempDir::new("binary-late");
        let exe = temp.path().join("tray");

        let mut watch = BinaryWatch::new(&exe);
        assert!(!watch.check(), "still nothing there");

        install(&exe, b"v1");
        assert!(!watch.check(), "the first sighting is the baseline");
        install(&exe, b"v2");
        assert!(watch.check(), "the change after that is real");
    }

    /// The watched path is the one recorded at startup, and it is what a
    /// restart re-executes.
    #[test]
    fn the_watch_remembers_the_path_it_was_given() {
        let temp = TempDir::new("binary-path");
        let exe = temp.path().join("tray");
        let watch = BinaryWatch::new(&exe);
        assert_eq!(watch.path(), exe);
    }

    /// Lays out `<prefix>/<keg_root>/tray/<version>/<rest>` with a binary in
    /// it and returns the binary's path.
    fn keg(prefix: &Path, keg_root: &str, version: &str, rest: &str) -> PathBuf {
        let exe = prefix.join(keg_root).join("tray").join(version).join(rest);
        std::fs::create_dir_all(exe.parent().expect("parent")).expect("create keg");
        std::fs::write(&exe, version).expect("write binary");
        exe
    }

    fn link(target: &str, at: &Path) {
        std::fs::create_dir_all(at.parent().expect("parent")).expect("create link dir");
        std::os::unix::fs::symlink(target, at).expect("symlink");
    }

    #[test]
    fn a_formula_binary_maps_to_the_prefix_bin_link() {
        let temp = TempDir::new("stable-formula");
        let exe = keg(temp.path(), "Cellar", "1.0.3", "bin/tray");
        let bin = temp.path().join("bin/tray");
        link("../Cellar/tray/1.0.3/bin/tray", &bin);

        assert_eq!(stable_path(&exe), bin);
    }

    #[test]
    fn a_cask_binary_maps_to_the_prefix_bin_link() {
        let temp = TempDir::new("stable-cask");
        let exe = keg(temp.path(), "Caskroom", "1.0.3", "tray");
        let bin = temp.path().join("bin/tray");
        link("../Caskroom/tray/1.0.3/tray", &bin);

        assert_eq!(stable_path(&exe), bin);
    }

    /// An unlinked formula has no `bin/` entry, but `opt/` still exists.
    #[test]
    fn a_formula_without_a_bin_link_falls_back_to_opt() {
        let temp = TempDir::new("stable-opt");
        let exe = keg(temp.path(), "Cellar", "1.0.3", "bin/tray");
        link("../Cellar/tray/1.0.3", &temp.path().join("opt/tray"));

        assert_eq!(stable_path(&exe), temp.path().join("opt/tray/bin/tray"));
    }

    /// A newer version is linked but this one is still running: the link is
    /// not this binary, so it is not what this binary records.
    #[test]
    fn a_link_to_another_version_is_not_used() {
        let temp = TempDir::new("stable-other-version");
        let exe = keg(temp.path(), "Cellar", "1.0.3", "bin/tray");
        keg(temp.path(), "Cellar", "1.0.4", "bin/tray");
        link(
            "../Cellar/tray/1.0.4/bin/tray",
            &temp.path().join("bin/tray"),
        );

        assert_eq!(stable_path(&exe), exe);
    }

    #[test]
    fn a_binary_outside_homebrew_is_left_alone() {
        let temp = TempDir::new("stable-plain");
        let exe = temp.path().join("tray");
        install(&exe, b"v1");

        assert_eq!(stable_path(&exe), exe);
    }

    #[test]
    fn identify_reports_nothing_for_a_path_that_is_not_there() {
        let temp = TempDir::new("binary-identify-missing");
        assert!(identify(&temp.path().join("absent")).is_none());
    }
}
