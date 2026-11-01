//! The optional update check: one anonymous `GET` against the GitHub releases
//! API, at most once a day, switched off entirely by
//! `check_updates = false` in `config.toml`.
//!
//! This is the only network call the binary can ever make, and it is
//! deliberately the least interesting one possible: no credentials, no
//! telemetry, no query parameters, nothing sent but a `User-Agent` naming the
//! program and its version. Everything a caller needs from it is
//! an [`Outcome`]; a DNS failure, a rate limit, a proxy that swallows the
//! request, a redesigned JSON body — all of them are the same quiet
//! [`Outcome::Nothing`]. A background version check must never toast, never log, and
//! never be a reason the tray behaves differently.
//!
//! The two pieces that can be wrong in an interesting way — reading the
//! release JSON and deciding whether a tag is newer than what is running — are
//! pure functions, tested below without touching the network.

use std::time::Duration;

/// The releases endpoint. `latest` excludes drafts and prereleases on
/// GitHub's side, so the prerelease handling in [`is_newer`] is a
/// belt-and-braces measure for tags that are merely *named* like prereleases.
const RELEASES_URL: &str =
    "https://api.github.com/repos/patrickdappollonio/claude-usage-tray/releases/latest";

/// Whole-request budget. Generous, because it runs on its own thread and
/// nothing waits for it; bounded, because a hung connection must not keep a
/// thread (and a socket) alive for the lifetime of the session.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The version this binary was built as.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A release newer than the running binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    /// The tag with any leading `v` stripped, e.g. `0.2.0`.
    pub version: String,
    /// The release page to open in a browser.
    pub url: String,
}

impl Update {
    /// The menu row's label.
    pub fn label(&self) -> String {
        format!("⬆ Update available: v{}", self.version)
    }
}

/// What the releases API said about the latest release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// The tag with any leading `v` stripped, e.g. `0.2.0`.
    pub version: String,
    /// The release page to open in a browser.
    pub url: String,
    /// Names of the files fully uploaded to the release. An asset GitHub still
    /// reports as mid-upload is left out.
    pub assets: Vec<String>,
}

/// Pulls the tag, page and asset names out of a releases-API body, with the
/// tag's leading `v` stripped. `None` for anything that is not an object
/// carrying `tag_name` and `html_url` as non-empty strings — including HTML
/// error pages, GitHub's `{"message": "Not Found"}`, and truncated bodies. A
/// missing or malformed `assets` list reads as no assets.
pub fn parse_release_json(body: &str) -> Option<Release> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let tag = value.get("tag_name")?.as_str()?.trim();
    let url = value.get("html_url")?.as_str()?.trim();
    if tag.is_empty() || url.is_empty() {
        return None;
    }
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if version.is_empty() {
        return None;
    }
    let assets = value
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .map(|assets| assets.iter().filter_map(uploaded_asset_name).collect())
        .unwrap_or_default();
    Some(Release {
        version: version.to_string(),
        url: url.to_string(),
        assets,
    })
}

/// An asset's name, unless GitHub says it is not fully uploaded yet
/// (`"state": "open"`). An asset with no `state` at all is taken as uploaded.
fn uploaded_asset_name(asset: &serde_json::Value) -> Option<String> {
    let state = asset.get("state").and_then(serde_json::Value::as_str);
    if state.is_some_and(|state| state != "uploaded") {
        return None;
    }
    let name = asset.get("name")?.as_str()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// The OS and architecture words the release workflow puts in asset names,
/// for Rust's own [`std::env::consts::OS`] and [`std::env::consts::ARCH`].
/// `None` for a platform the project does not publish a build for.
fn release_platform(os: &str, arch: &str) -> Option<(&'static str, &'static str)> {
    let os = match os {
        "linux" => "linux",
        "macos" => "darwin",
        _ => return None,
    };
    let arch = match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => return None,
    };
    Some((os, arch))
}

/// The tarball `.github/workflows/release.yml` builds for this version on
/// this platform, e.g. `claude-usage-tray_0.2.0_linux_amd64.tar.gz`. The
/// other downloads for a platform (the `.deb`, the macOS `.app` zip) come out
/// of the same job, so the tarball stands for all of them.
fn asset_name(version: &str, os: &str, arch: &str) -> Option<String> {
    let (os, arch) = release_platform(os, arch)?;
    Some(format!("claude-usage-tray_{version}_{os}_{arch}.tar.gz"))
}

/// Whether `release` already carries the build for the given platform.
fn has_build_for(release: &Release, os: &str, arch: &str) -> bool {
    asset_name(&release.version, os, arch).is_some_and(|wanted| release.assets.contains(&wanted))
}

/// Splits a version into its numeric triple, tolerating missing components
/// (`"1"` is `1.0.0`) and ignoring anything from the first `-` onwards.
///
/// `None` when any present component is not a plain number: `1.2.x` and
/// `latest` are not versions this can reason about, and guessing at them is
/// how a downgrade gets advertised as an upgrade.
fn numeric_triple(version: &str) -> Option<[u64; 3]> {
    let core = version.split('-').next().unwrap_or("");
    let core = core.strip_prefix('v').unwrap_or(core).trim();
    if core.is_empty() {
        return None;
    }
    let mut triple = [0u64; 3];
    let mut parts = core.split('.');
    for slot in &mut triple {
        match parts.next() {
            // A missing component is zero: "1.2" means 1.2.0.
            None => break,
            Some(part) => *slot = part.parse::<u64>().ok()?,
        }
    }
    // Anything past the third component (a build number, say) does not change
    // the ordering decision, but it must still be numeric to be trusted.
    for part in parts {
        part.parse::<u64>().ok()?;
    }
    Some(triple)
}

/// Whether `latest` is a newer release than `current`.
///
/// The comparison is the numeric triple alone, so the rule for a tag with a
/// dash in it (`0.2.0-rc.1`) is simply: **the prerelease marker is ignored,
/// and only the numbers decide**. `0.2.0-rc.1` therefore counts as newer than
/// `0.1.0` (its numbers are), and `0.1.0-rc.1` does *not* count as newer than
/// `0.1.0` (identical numbers are not newer). This errs towards announcing an
/// upcoming release rather than towards nagging a user who is already running
/// the prerelease.
///
/// Anything non-numeric on either side is `false`: a version this code cannot
/// order is not evidence of an update.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (numeric_triple(latest), numeric_triple(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// Performs the request and returns the body. Split out from [`check`] purely
/// so the parsing above stays testable without it; nothing here is unit-tested,
/// because everything here is network I/O.
fn fetch() -> Option<String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .build()
        .new_agent();
    let mut response = agent
        .get(RELEASES_URL)
        .header(
            "User-Agent",
            concat!("claude-usage-tray/", env!("CARGO_PKG_VERSION")),
        )
        .header("Accept", "application/vnd.github+json")
        .call()
        .ok()?;
    response.body_mut().read_to_string().ok()
}

/// What one update check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A newer release with a build for this platform.
    Available(Update),
    /// A newer release whose build for this platform is not uploaded yet. Not
    /// worth showing, but worth checking again soon rather than tomorrow.
    Pending,
    /// No update, or no answer. Callers must not distinguish the two.
    Nothing,
}

/// Decides what a release means for a binary at version `current` running on
/// `os`/`arch` (spelled as in [`std::env::consts`]).
fn judge(release: Release, current: &str, os: &str, arch: &str) -> Outcome {
    if !is_newer(&release.version, current) {
        return Outcome::Nothing;
    }
    if !has_build_for(&release, os, arch) {
        return Outcome::Pending;
    }
    Outcome::Available(Update {
        version: release.version,
        url: release.url,
    })
}

/// The whole check: fetch, parse, compare, and make sure the release has a
/// build for this platform before offering it.
pub fn check() -> Outcome {
    let Some(release) = fetch().as_deref().and_then(parse_release_json) else {
        return Outcome::Nothing;
    };
    judge(
        release,
        current_version(),
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

/// The platform's own "hand this URL to the default browser" command:
/// `open` on macOS, `xdg-open` everywhere else.
#[cfg(target_os = "macos")]
const OPENER: &str = "open";
#[cfg(not(target_os = "macos"))]
const OPENER: &str = "xdg-open";

/// Opens a release page in the user's browser via [`OPENER`], detached.
///
/// Spawned and deliberately never waited on: this is called from the poll
/// loop, and a browser that takes seconds to start (or an `xdg-open` that is
/// not installed at all) must not stall it. A failure to spawn is silent for
/// the same reason every other optional integration here is.
pub fn open_url(url: &str) {
    let _ = std::process::Command::new(OPENER)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_release_body_and_strips_the_v() {
        let body = r#"{
            "tag_name": "v0.2.0",
            "html_url": "https://github.com/o/r/releases/tag/v0.2.0",
            "name": "0.2.0",
            "assets": []
        }"#;
        assert_eq!(
            parse_release_json(body),
            Some(Release {
                version: "0.2.0".to_string(),
                url: "https://github.com/o/r/releases/tag/v0.2.0".to_string(),
                assets: vec![],
            })
        );
    }

    #[test]
    fn parses_a_tag_without_the_v_prefix() {
        let body = r#"{"tag_name": "1.4.2", "html_url": "https://example.test/r"}"#;
        assert_eq!(
            parse_release_json(body),
            Some(Release {
                version: "1.4.2".to_string(),
                url: "https://example.test/r".to_string(),
                assets: vec![],
            })
        );
    }

    #[test]
    fn missing_fields_are_none() {
        assert_eq!(
            parse_release_json(r#"{"html_url": "https://x.test"}"#),
            None
        );
        assert_eq!(parse_release_json(r#"{"tag_name": "v1.0.0"}"#), None);
        assert_eq!(parse_release_json("{}"), None);
        // GitHub's own error shape for a repo with no releases yet.
        assert_eq!(parse_release_json(r#"{"message": "Not Found"}"#), None);
    }

    #[test]
    fn wrong_typed_or_empty_fields_are_none() {
        assert_eq!(
            parse_release_json(r#"{"tag_name": 2, "html_url": "https://x.test"}"#),
            None
        );
        assert_eq!(
            parse_release_json(r#"{"tag_name": "v1.0.0", "html_url": null}"#),
            None
        );
        assert_eq!(
            parse_release_json(r#"{"tag_name": "", "html_url": "https://x.test"}"#),
            None
        );
        // A tag that is *only* the prefix leaves nothing to compare.
        assert_eq!(
            parse_release_json(r#"{"tag_name": "v", "html_url": "https://x.test"}"#),
            None
        );
    }

    #[test]
    fn garbage_bodies_are_none() {
        for body in [
            "",
            "not json at all",
            "<html><body>502 Bad Gateway</body></html>",
            "[]",
            "null",
            r#"{"tag_name": "v1.0.0", "html_url": "https://x.test""#, // truncated
        ] {
            assert_eq!(parse_release_json(body), None, "body: {body}");
        }
    }

    #[test]
    fn equal_versions_are_not_newer() {
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("1.2.3", "1.2.3"));
    }

    #[test]
    fn newer_patch_minor_and_major_are_newer() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
    }

    #[test]
    fn older_patch_minor_and_major_are_not_newer() {
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("0.99.99", "1.0.0"));
    }

    #[test]
    fn components_compare_numerically_not_lexically() {
        // The classic: "10" sorts before "9" as text.
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
        assert!(!is_newer("2.0.0", "10.0.0"));
    }

    #[test]
    fn a_v_prefix_on_either_side_is_tolerated() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.2.0", "v0.1.0"));
        assert!(!is_newer("v0.1.0", "v0.1.0"));
    }

    #[test]
    fn missing_components_count_as_zero() {
        assert!(!is_newer("1", "1.0.0"));
        assert!(!is_newer("1.0", "1.0.0"));
        assert!(is_newer("1.1", "1.0.9"));
        assert!(is_newer("2", "1.9.9"));
    }

    #[test]
    fn a_prerelease_tag_is_judged_on_its_numbers_alone() {
        // Numerically ahead: an upcoming release worth pointing at.
        assert!(is_newer("0.2.0-rc.1", "0.1.0"));
        assert!(is_newer("v1.0.0-beta", "0.9.4"));
        // Numerically equal: not newer, so somebody running the prerelease is
        // not nagged to "update" to the same numbers.
        assert!(!is_newer("0.1.0-rc.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0-rc.1"));
        // Numerically behind stays behind.
        assert!(!is_newer("0.1.0-rc.1", "0.2.0"));
    }

    #[test]
    fn non_numeric_versions_never_count_as_newer() {
        for (latest, current) in [
            ("latest", "0.1.0"),
            ("nightly", "0.1.0"),
            ("0.x.0", "0.1.0"),
            ("1.2.3.beta", "0.1.0"),
            ("", "0.1.0"),
            ("v", "0.1.0"),
            ("-rc.1", "0.1.0"),
            ("0.2.0", "garbage"),
            ("9999.0.0", ""),
        ] {
            assert!(!is_newer(latest, current), "{latest} vs {current}");
        }
    }

    #[test]
    fn a_fourth_component_is_accepted_but_does_not_reorder() {
        assert!(!is_newer("1.2.3.4", "1.2.3"));
        assert!(is_newer("1.2.4.0", "1.2.3.9"));
    }

    #[test]
    fn the_current_version_is_something_this_code_can_compare() {
        // A build whose own version cannot be parsed would silently disable
        // the whole feature, so pin the invariant here rather than finding out
        // from a user who never sees an update.
        assert!(numeric_triple(current_version()).is_some());
    }

    #[test]
    fn the_url_opener_is_the_platform_native_command() {
        // `xdg-open` does not exist on macOS, so a click on the update row
        // there must reach for `open` instead — this silently did nothing on
        // macOS for as long as the command was hardcoded.
        #[cfg(target_os = "macos")]
        assert_eq!(OPENER, "open");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(OPENER, "xdg-open");
    }

    fn release(version: &str, assets: &[&str]) -> Release {
        Release {
            version: version.to_string(),
            url: "https://example.test/r".to_string(),
            assets: assets.iter().map(|name| name.to_string()).collect(),
        }
    }

    #[test]
    fn asset_names_are_read_and_unfinished_uploads_skipped() {
        let body = r#"{
            "tag_name": "v0.2.0",
            "html_url": "https://example.test/r",
            "assets": [
                {"name": "claude-usage-tray_0.2.0_linux_amd64.tar.gz", "state": "uploaded"},
                {"name": "claude-usage-tray_0.2.0_darwin_arm64.tar.gz", "state": "open"},
                {"name": "claude-usage-tray_0.2.0_linux_arm64.tar.gz"},
                {"state": "uploaded"},
                {"name": "", "state": "uploaded"}
            ]
        }"#;
        assert_eq!(
            parse_release_json(body).map(|release| release.assets),
            Some(vec![
                "claude-usage-tray_0.2.0_linux_amd64.tar.gz".to_string(),
                "claude-usage-tray_0.2.0_linux_arm64.tar.gz".to_string(),
            ])
        );
    }

    #[test]
    fn a_malformed_assets_list_reads_as_no_assets() {
        let body = r#"{"tag_name": "v0.2.0", "html_url": "https://x.test", "assets": "nope"}"#;
        assert_eq!(
            parse_release_json(body).map(|release| release.assets),
            Some(vec![])
        );
    }

    #[test]
    fn asset_names_match_the_release_workflow() {
        for (os, arch, want) in [
            (
                "linux",
                "x86_64",
                "claude-usage-tray_1.2.3_linux_amd64.tar.gz",
            ),
            (
                "linux",
                "aarch64",
                "claude-usage-tray_1.2.3_linux_arm64.tar.gz",
            ),
            (
                "macos",
                "x86_64",
                "claude-usage-tray_1.2.3_darwin_amd64.tar.gz",
            ),
            (
                "macos",
                "aarch64",
                "claude-usage-tray_1.2.3_darwin_arm64.tar.gz",
            ),
        ] {
            assert_eq!(
                asset_name("1.2.3", os, arch).as_deref(),
                Some(want),
                "{os}/{arch}"
            );
        }
        assert_eq!(asset_name("1.2.3", "windows", "x86_64"), None);
        assert_eq!(asset_name("1.2.3", "linux", "riscv64"), None);
    }

    #[test]
    fn this_build_has_a_release_asset_name() {
        // Without one, no release could ever be offered on this platform.
        assert!(release_platform(std::env::consts::OS, std::env::consts::ARCH).is_some());
    }

    #[test]
    fn a_newer_release_with_this_platforms_build_is_offered() {
        let release = release(
            "0.2.0",
            &[
                "claude-usage-tray_0.2.0_darwin_arm64_app.zip",
                "claude-usage-tray_0.2.0_darwin_arm64.tar.gz",
            ],
        );
        assert_eq!(
            judge(release, "0.1.0", "macos", "aarch64"),
            Outcome::Available(Update {
                version: "0.2.0".to_string(),
                url: "https://example.test/r".to_string(),
            })
        );
    }

    #[test]
    fn a_newer_release_without_this_platforms_build_is_pending() {
        // Every build but Apple Silicon's: a release whose last job is still running.
        let release = release(
            "0.2.0",
            &[
                "claude-usage-tray_0.2.0_darwin_amd64.tar.gz",
                "claude-usage-tray_0.2.0_linux_amd64.tar.gz",
                "claude-usage-tray_0.2.0_linux_arm64.tar.gz",
            ],
        );
        assert_eq!(
            judge(release.clone(), "0.1.0", "macos", "aarch64"),
            Outcome::Pending
        );
        assert!(matches!(
            judge(release, "0.1.0", "linux", "x86_64"),
            Outcome::Available(_)
        ));
    }

    #[test]
    fn another_versions_build_does_not_count() {
        let release = release("0.2.0", &["claude-usage-tray_0.1.0_linux_amd64.tar.gz"]);
        assert_eq!(judge(release, "0.1.0", "linux", "x86_64"), Outcome::Pending);
    }

    #[test]
    fn a_release_that_is_not_newer_is_nothing_even_without_a_build() {
        assert_eq!(
            judge(release("0.1.0", &[]), "0.1.0", "linux", "x86_64"),
            Outcome::Nothing
        );
    }

    #[test]
    fn the_menu_label_reads_as_a_version() {
        let update = Update {
            version: "0.2.0".into(),
            url: "https://example.test".into(),
        };
        assert_eq!(update.label(), "⬆ Update available: v0.2.0");
    }
}
