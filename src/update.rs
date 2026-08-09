//! Finding out that a newer release exists, and installing it.
//!
//! A third seam alongside `Platform` and `Shell`, and it is a sibling of
//! `Shell` rather than of `Platform` for the same reason: the answer arrives
//! on somebody else's schedule, from a thread of its own, and the UI has to be
//! woken when it does.
//!
//! Everything here is portable and covered by tests. What is not — the HTTP
//! request and the thread around it — lives in `github.rs` and is the only
//! part that cannot be exercised on macOS.
//!
//! The shape of the whole feature comes from one fact: the application is
//! running during a live broadcast. Nothing here is automatic. The check runs
//! once per launch and its only effect is to make an indicator appear;
//! downloading and restarting happen when the user clicks and never otherwise.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;

use crate::shell::Waker;

/// The file the release publishes, and the name this application runs under.
const ASSET: &str = "winsend.exe";

/// Where releases are read from.
///
/// The list rather than `/releases/latest`, which is not the same question:
/// that endpoint excludes pre-releases, and every release of this project so
/// far has been one, so it answers 404. Picking the highest version out of the
/// list is also the more honest question to ask, since what matters is the
/// number rather than which release GitHub considers current.
///
/// Only reached from `github.rs`, which is Windows-only, so on other targets
/// this and the three items marked the same way below read as dead code. They
/// are still compiled and still tested there, which is the entire reason they
/// live above the seam rather than inside the Windows adapter.
#[cfg_attr(not(windows), allow(dead_code))]
const RELEASES: &str = "https://api.github.com/repos/brueshi/WinSend/releases";

/// A three-number version, compared as numbers.
///
/// The derived ordering compares the fields in the order they are declared,
/// which is exactly right here. A textual comparison would put 0.1.9 above
/// 0.1.10, and with an updater that stops being untidy and becomes an
/// application that believes it is permanently out of date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u32,
    minor: u32,
    patch: u32,
}

impl Version {
    /// What this build is, taken from `Cargo.toml` at compile time.
    ///
    /// This is the number the running application compares against the tags,
    /// which is why `Cargo.toml` has to be bumped before the tag is created
    /// rather than after. See `tools/release.py`.
    pub fn current() -> Self {
        env!("CARGO_PKG_VERSION")
            .parse()
            .expect("the crate's own version must parse")
    }
}

impl FromStr for Version {
    type Err = ();

    /// Accepts `0.1.14` and `v0.1.14`, and nothing else.
    ///
    /// A suffix such as `-rc1` is rejected rather than ignored. Two versions
    /// that differ only by something this cannot see would compare equal, and
    /// offering an update on a comparison that is not real is worse than not
    /// offering one at all.
    fn from_str(text: &str) -> Result<Self, ()> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);

        let mut parts = text.split('.');
        let mut next = || parts.next().ok_or(())?.parse::<u32>().map_err(|_| ());
        let version = Self { major: next()?, minor: next()?, patch: next()? };

        if parts.next().is_some() {
            return Err(());
        }
        Ok(version)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A release that could be installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub url: String,
    /// The SHA-256 the download must hash to, lowercase hex without the
    /// `sha256:` the API prefixes it with.
    pub digest: String,
}

/// Something the updater found out, delivered the way `ShellEvent` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateEvent {
    /// A newer release exists. The only thing a check ever produces: one that
    /// found nothing, or could not run at all, says nothing.
    Available(Release),
    /// The new executable is in place. The application should relaunch itself
    /// and exit.
    Installed,
    /// The install failed and the executable on disk is still the old one.
    Failed(String),
}

pub trait Updater {
    /// Start the once-per-launch check. Returns immediately; the answer
    /// arrives through `poll`, or does not arrive.
    fn check(&self);

    /// Download the release, check it against its digest, and put it in place.
    /// The outcome arrives through `poll`.
    fn install(&self, release: Release);

    /// Everything that has happened since the last call.
    fn poll(&self) -> Vec<UpdateEvent>;

    /// Escape hatch for the mock-only debug controls, mirroring the one on
    /// `Platform` and `Shell`.
    #[cfg(not(windows))]
    fn as_mock(&self) -> Option<&crate::mock::MockUpdater> {
        None
    }
}

/// The real updater on Windows, the fake one everywhere else.
///
/// Mirrors `select_platform` and `shell::create`, including `WINSEND_MOCK=1`.
/// macOS always gets the mock: the release asset is a Windows executable, so
/// installing one here would replace a working binary with something that
/// cannot run.
pub fn create(waker: Waker) -> Box<dyn Updater> {
    let forced_mock = std::env::var("WINSEND_MOCK").is_ok_and(|v| v == "1");

    #[cfg(windows)]
    if !forced_mock {
        return Box::new(crate::github::GithubUpdater::new(waker));
    }

    let _ = forced_mock;
    Box::new(crate::mock::MockUpdater::new(waker))
}

/// What GitHub returns, cut down to the fields that matter.
#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    /// Added to the API relatively recently, so it is treated as optional and
    /// a release without one is passed over rather than trusted.
    #[serde(default)]
    digest: Option<String>,
}

/// The highest-numbered release that could actually be installed.
///
/// Draft releases are skipped; pre-releases are not, because every release of
/// this project has been one. A release whose tag will not parse, or which
/// publishes no usable asset, is passed over rather than guessed at — that
/// includes one with no digest, since a download that cannot be checked is not
/// one to offer.
pub fn newest_release(json: &str) -> Result<Option<Release>, String> {
    let releases: Vec<ApiRelease> =
        serde_json::from_str(json).map_err(|e| format!("could not read the release list: {e}"))?;

    Ok(releases
        .into_iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            let version = release.tag_name.parse().ok()?;
            let asset = release
                .assets
                .into_iter()
                .find(|asset| asset.name.eq_ignore_ascii_case(ASSET))?;
            let digest = asset.digest?;
            let digest = digest.strip_prefix("sha256:")?.to_ascii_lowercase();
            Some(Release { version, url: asset.browser_download_url, digest })
        })
        .max_by_key(|release| release.version))
}

/// The release worth telling the user about, if any.
pub fn newer_than_current(release: Option<Release>) -> Option<Release> {
    release.filter(|release| release.version > Version::current())
}

/// The URL the check reads. Windows-only in practice; see `RELEASES`.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn releases_url() -> &'static str {
    RELEASES
}

/// Whether the bytes downloaded are the bytes the release published.
///
/// This is trust on first use against GitHub over TLS, not code signing. It
/// catches a truncated or corrupted download and an asset that changed after
/// the release was published; it does not defend against GitHub itself, since
/// whatever could replace the asset could replace the digest beside it. Real
/// signing needs a certificate and is a separate decision with a cost.
///
/// Windows-only in practice; see `RELEASES`.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn digest_matches(bytes: &[u8], expected: &str) -> bool {
    use sha2::{Digest, Sha256};

    let actual = Sha256::digest(bytes);
    let actual = actual.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    // Constant time is not the point here — the digest is public and so is the
    // download — but a case or whitespace mismatch reading as tampering would
    // be, so both sides are normalised.
    actual == expected.trim().to_ascii_lowercase()
}

/// Where the running executable gets moved to so the new one can take its
/// name.
fn previous_executable(current: &Path) -> PathBuf {
    let stem = current.file_stem().map(|s| s.to_string_lossy().into_owned());
    let extension = current.extension().map(|e| e.to_string_lossy().into_owned());

    match (stem, extension) {
        (Some(stem), Some(extension)) => current.with_file_name(format!("{stem}-old.{extension}")),
        (Some(stem), None) => current.with_file_name(format!("{stem}-old")),
        // No file name at all is not a path an executable came from, but
        // answering with something harmless beats unwrapping on it.
        (None, _) => current.with_file_name("winsend-old"),
    }
}

/// Where the download is written before it is put in place.
///
/// Beside the current executable rather than in a temporary directory, so the
/// final step is a rename within one filesystem. A rename across filesystems
/// is a copy, and a copy is not the atomic swap this depends on.
pub fn download_path(current: &Path) -> PathBuf {
    previous_executable(current).with_extension("new")
}

/// Put `downloaded` where `current` is, reversibly.
///
/// Windows will not let a running executable be overwritten, but it will let
/// it be renamed. So the running one is moved aside and the download takes its
/// name.
///
/// The rename back is the whole point of doing it this way. If the second step
/// fails after the first has succeeded, there would otherwise be no executable
/// at the path the user launches, and an interrupted update would have cost
/// them the application.
///
/// Windows-only in practice; see `RELEASES`.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn swap_in_place(current: &Path, downloaded: &Path) -> Result<(), String> {
    let previous = previous_executable(current);

    // A leftover from an earlier update that could not be deleted because it
    // was still running. Removing it explicitly means a failure shows up here
    // rather than as a confusing one at the rename below.
    let _ = std::fs::remove_file(&previous);

    std::fs::rename(current, &previous)
        .map_err(|e| format!("could not move the current version aside: {e}"))?;

    if let Err(why) = std::fs::rename(downloaded, current) {
        if let Err(rollback) = std::fs::rename(&previous, current) {
            return Err(format!(
                "the update failed ({why}) and could not be undone ({rollback}). \
                 The previous version is still at {}",
                previous.display()
            ));
        }
        return Err(format!("could not put the new version in place: {why}"));
    }

    Ok(())
}

/// Delete the executable the last update moved aside, if it is still there.
///
/// On the next start rather than at the end of the install, because a running
/// executable cannot be deleted — which is the same fact the whole rename
/// dance exists for. Failure is ignored: a stale file beside the application
/// is untidy, and nothing the user can act on.
pub fn clean_up_previous_install() {
    let Ok(current) = std::env::current_exe() else {
        return;
    };
    let _ = std::fs::remove_file(previous_executable(&current));
    let _ = std::fs::remove_file(download_path(&current));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        text.parse().expect("should parse")
    }

    #[test]
    fn versions_compare_as_numbers_rather_than_text() {
        assert!(version("0.1.10") > version("0.1.9"), "the trap this exists for");
        assert!(version("0.10.0") > version("0.9.9"));
        assert!(version("1.0.0") > version("0.99.99"));
        assert_eq!(version("v0.1.15"), version("0.1.15"), "the tag's v is not part of it");
    }

    #[test]
    fn a_version_that_is_not_three_numbers_is_refused() {
        for text in ["0.1", "0.1.2.3", "0.1.x", "", "v", "0.1.2-rc1", "latest", "-1.0.0"] {
            assert!(text.parse::<Version>().is_err(), "{text:?} must not parse");
        }
    }

    /// The version the running application compares against the tags. If this
    /// stops parsing, the updater has no idea what it is.
    #[test]
    fn this_build_knows_its_own_version() {
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }

    /// Trimmed from a real response, including the pre-release flag every
    /// release of this project carries.
    const RELEASES_JSON: &str = r#"[
        {
            "tag_name": "v0.1.15",
            "draft": false,
            "prerelease": true,
            "assets": [{
                "name": "winsend.exe",
                "browser_download_url": "https://example.invalid/v0.1.15/winsend.exe",
                "digest": "sha256:B5FE7EA9616F14148866D8101E55A69A3D9A55948758089B8618411D1C62B17B",
                "size": 4945408
            }]
        },
        {
            "tag_name": "v0.1.9",
            "draft": false,
            "prerelease": true,
            "assets": [{
                "name": "winsend.exe",
                "browser_download_url": "https://example.invalid/v0.1.9/winsend.exe",
                "digest": "sha256:aaaa",
                "size": 100
            }]
        }
    ]"#;

    #[test]
    fn the_highest_version_wins_rather_than_the_first_listed() {
        let release = newest_release(RELEASES_JSON).unwrap().expect("one is usable");
        assert_eq!(release.version, version("0.1.15"));
        assert_eq!(
            release.digest, "b5fe7ea9616f14148866d8101e55a69a3d9a55948758089b8618411d1c62b17b",
            "the sha256: prefix is stripped and the hex normalised"
        );
    }

    /// A pre-release is still a release here. Every one of this project's has
    /// been one, and skipping them would mean never offering anything.
    #[test]
    fn a_pre_release_is_still_offered() {
        assert!(newest_release(RELEASES_JSON).unwrap().is_some());
    }

    #[test]
    fn drafts_and_unusable_releases_are_passed_over() {
        let json = r#"[
            {"tag_name": "v9.9.9", "draft": true, "assets": [
                {"name": "winsend.exe", "browser_download_url": "u", "digest": "sha256:ab"}]},
            {"tag_name": "not-a-version", "draft": false, "assets": [
                {"name": "winsend.exe", "browser_download_url": "u", "digest": "sha256:ab"}]},
            {"tag_name": "v8.0.0", "draft": false, "assets": [
                {"name": "winsend.exe", "browser_download_url": "u"}]},
            {"tag_name": "v7.0.0", "draft": false, "assets": [
                {"name": "notes.txt", "browser_download_url": "u", "digest": "sha256:ab"}]},
            {"tag_name": "v1.0.0", "draft": false, "assets": [
                {"name": "winsend.exe", "browser_download_url": "u", "digest": "sha256:ab"}]}
        ]"#;

        let release = newest_release(json).unwrap().expect("the last one is usable");
        assert_eq!(
            release.version,
            version("1.0.0"),
            "a draft, an unparseable tag, a missing digest and a missing asset are all skipped"
        );
    }

    #[test]
    fn an_empty_release_list_is_not_an_error() {
        assert_eq!(newest_release("[]").unwrap(), None);
    }

    #[test]
    fn a_response_that_is_not_a_release_list_is_an_error() {
        assert!(newest_release(r#"{"message": "Not Found"}"#).is_err());
    }

    #[test]
    fn only_a_higher_version_is_offered() {
        let at = |text: &str| {
            Some(Release {
                version: version(text),
                url: "u".into(),
                digest: "d".into(),
            })
        };
        let current = Version::current().to_string();

        assert_eq!(newer_than_current(at(&current)), None, "the one we are running");
        assert_eq!(newer_than_current(at("0.0.1")), None, "an older one is never offered");
        assert!(newer_than_current(at("999.0.0")).is_some());
        assert_eq!(newer_than_current(None), None);
    }

    #[test]
    fn a_download_is_checked_against_its_digest() {
        // Known-answer: the SHA-256 of the empty input.
        assert!(digest_matches(
            b"",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        ));
        assert!(
            digest_matches(b"", "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855"),
            "case must not read as tampering"
        );
        assert!(!digest_matches(b"winsend", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"));
        assert!(!digest_matches(b"", ""), "an absent digest is not a match");
    }

    #[test]
    fn the_previous_executable_sits_beside_the_current_one() {
        assert_eq!(
            previous_executable(Path::new(r"C:\Apps\winsend.exe")),
            PathBuf::from(r"C:\Apps\winsend-old.exe")
        );
        assert_eq!(
            previous_executable(Path::new("/usr/local/bin/winsend")),
            PathBuf::from("/usr/local/bin/winsend-old")
        );
        assert_eq!(
            download_path(Path::new(r"C:\Apps\winsend.exe")),
            PathBuf::from(r"C:\Apps\winsend-old.new"),
            "and so does the download, so the swap is a rename within one filesystem"
        );
    }

    /// A scratch directory that cleans itself up, so the swap can be exercised
    /// against real files rather than against a description of them.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("winsend-test-{name}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("scratch directory");
            Self(path)
        }

        fn file(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, contents).expect("write");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_swap_puts_the_download_in_place_and_keeps_the_old_one() {
        let dir = TempDir::new("swap");
        let current = dir.file("winsend.exe", "old");
        let downloaded = dir.file("winsend-old.new", "new");

        swap_in_place(&current, &downloaded).expect("the swap must succeed");

        assert_eq!(std::fs::read_to_string(&current).unwrap(), "new");
        assert_eq!(
            std::fs::read_to_string(previous_executable(&current)).unwrap(),
            "old",
            "the running executable is still on disk, to be deleted next start"
        );
    }

    /// The failure that must never cost the user their application: the move
    /// aside succeeded, putting the new one in place did not, and the path
    /// they launch has to still work.
    #[test]
    fn a_swap_that_fails_half_way_puts_the_original_back() {
        let dir = TempDir::new("rollback");
        let current = dir.file("winsend.exe", "old");
        let missing = dir.0.join("never-downloaded.new");

        let failure = swap_in_place(&current, &missing).expect_err("there is nothing to move in");

        assert!(current.exists(), "the executable must still be where it was: {failure}");
        assert_eq!(std::fs::read_to_string(&current).unwrap(), "old");
        assert!(
            !previous_executable(&current).exists(),
            "and nothing is left lying beside it"
        );
    }

    /// An earlier update that could not clean up leaves this behind. Renaming
    /// onto it has to work rather than fail the whole update.
    #[test]
    fn a_leftover_from_a_previous_update_does_not_block_the_next_one() {
        let dir = TempDir::new("leftover");
        let current = dir.file("winsend.exe", "old");
        dir.file("winsend-old.exe", "older still");
        let downloaded = dir.file("winsend-old.new", "new");

        swap_in_place(&current, &downloaded).expect("the leftover must not get in the way");

        assert_eq!(std::fs::read_to_string(&current).unwrap(), "new");
    }
}
