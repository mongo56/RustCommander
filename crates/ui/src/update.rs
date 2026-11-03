//! Update check (`cm_CheckForUpdates`): asks the GitHub releases API for the
//! latest release of this repository, compares its version with ours, and
//! downloads the installer asset (`RustCommander-<ver>-setup.exe`, made by
//! `scripts\package.ps1`) into a temp folder. Everything here blocks; the
//! app runs it on worker threads (`app/update.rs`).

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const API_LATEST: &str = "https://api.github.com/repos/mongo56/RustCommander/releases/latest";
/// Every asset of a release of this repository is downloaded from here
/// (GitHub then redirects to its storage host).
const DOWNLOAD_PREFIX: &str = "https://github.com/mongo56/RustCommander/releases/download/";
/// Hosts a download may end up on after redirects.
const DOWNLOAD_HOSTS: [&str; 3] = ["github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"];
pub const RELEASES_PAGE: &str = "https://github.com/mongo56/RustCommander/releases";
/// Command line for the downloaded installer: no "This will install"
/// prompt, progress only, and `/RESTARTAPP` makes the installer start the
/// new version when it's done (`[Run]` entry in `installer/rustcommander.iss`).
pub const INSTALLER_ARGS: &str = "/SP- /SILENT /NORESTART /RESTARTAPP";
/// Longest release-notes summary shown in the dialog.
const NOTES_LINES: usize = 12;
const NOTES_CHARS: usize = 700;

/// A release version: `major.minor.patch` (a missing part is 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    /// Parses `0.5.7`, `v0.5.7` or `0.5` (anything after `-` or `+`, like a
    /// pre-release tag, is ignored).
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
        let core = s.split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let mut next = |required: bool| match parts.next() {
            Some(p) => p.parse::<u64>().ok(),
            None if required => None,
            None => Some(0),
        };
        let v = Version(next(true)?, next(false)?, next(false)?);
        parts.next().is_none().then_some(v)
    }

    /// The version of this build. Debug builds take `RC_PRETEND_VERSION`
    /// instead when set, to try the update dialog against real releases.
    pub fn current() -> Version {
        #[cfg(debug_assertions)]
        if let Some(v) = std::env::var("RC_PRETEND_VERSION").ok().and_then(|v| Version::parse(&v)) {
            return v;
        }
        Version::parse(crate::VERSION).unwrap_or(Version(0, 0, 0))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// A downloadable file of a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// Lower-case hex SHA-256 when the API reports one (`digest`).
    pub sha256: Option<String>,
}

/// The latest release, as far as the update dialog needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    /// Release page on GitHub.
    pub page: String,
    /// Start of the release notes, as plain lines.
    pub notes: String,
    /// `RustCommander-<ver>-setup.exe`, if the release has it.
    pub setup: Option<Asset>,
}

/// Result of a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// The repository has no published release yet (HTTP 404).
    NoReleases,
    /// The latest release (whether newer or not: see `is_newer`).
    Latest(Release),
}

impl Release {
    pub fn is_newer(&self) -> bool {
        self.version > Version::current()
    }
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    size: u64,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

/// Reads a `releases/latest` response. Only an asset named exactly
/// `RustCommander-<version>-setup.exe`, uploaded to this release of this
/// repository, counts as the installer.
pub fn parse_release(json: &str) -> Result<Release, String> {
    let api: ApiRelease = serde_json::from_str(json).map_err(|e| format!("Unexpected answer from GitHub: {e}"))?;
    if api.draft {
        return Err("The latest release is a draft".into());
    }
    let version =
        Version::parse(&api.tag_name).ok_or_else(|| format!("The latest release has no version number ({})", api.tag_name))?;
    let shown = api.tag_name.trim().trim_start_matches(['v', 'V']);
    let wanted = format!("RustCommander-{shown}-setup.exe");
    let prefix = format!("{DOWNLOAD_PREFIX}{}/", api.tag_name);
    let setup = api
        .assets
        .into_iter()
        .filter(|a| a.state.as_deref().is_none_or(|s| s == "uploaded"))
        .find(|a| a.name.eq_ignore_ascii_case(&wanted))
        .filter(|a| a.size > 0 && a.browser_download_url == format!("{prefix}{}", a.name))
        .map(|a| Asset {
            sha256: a
                .digest
                .as_deref()
                .and_then(|d| d.strip_prefix("sha256:"))
                .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
                .map(str::to_ascii_lowercase),
            name: a.name,
            url: a.browser_download_url,
            size: a.size,
        });
    let page = if api.html_url.starts_with(RELEASES_PAGE) { api.html_url } else { RELEASES_PAGE.to_owned() };
    Ok(Release { version, tag: api.tag_name, page, notes: summary(api.body.as_deref().unwrap_or("")), setup })
}

/// The first lines of the release notes as plain text: Markdown headings,
/// bullets and emphasis markers dropped, blank runs collapsed, cut short.
pub fn summary(body: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        let line = line.trim_start_matches('#').trim();
        let line = match line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            Some(rest) => format!("\u{2022} {}", rest.trim()),
            None => line.to_owned(),
        };
        let line = line.replace("**", "").replace('`', "");
        if line.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let cut = lines.len() > NOTES_LINES;
    lines.truncate(NOTES_LINES);
    let mut text = lines.join("\n");
    if text.chars().count() > NOTES_CHARS {
        text = text.chars().take(NOTES_CHARS).collect::<String>().trim_end().to_owned();
        return format!("{text}\u{2026}");
    }
    if cut {
        text.push_str("\n\u{2026}");
    }
    text
}

fn agent(timeout: Option<Duration>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(format!("RustCommander/{} (update check)", crate::VERSION))
        .https_only(true)
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        // Downloads may take long, but must not stall.
        .timeout_per_read(Some(Duration::from_secs(60)))
        .timeout_global(timeout)
        .build()
        .new_agent()
}

/// Asks GitHub for the latest release.
pub fn check() -> Result<Check, String> {
    let response = agent(Some(Duration::from_secs(45)))
        .get(API_LATEST)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| format!("Can't reach GitHub: {e}"))?;
    let status = response.status().as_u16();
    if status == 404 {
        return Ok(Check::NoReleases);
    }
    let mut body = response.into_body();
    if status != 200 {
        let hint = if status == 403 || status == 429 { " (rate limit; try again later)" } else { "" };
        return Err(format!("GitHub answered HTTP {status}{hint}"));
    }
    let json = body.read_to_string().map_err(|e| format!("Can't read GitHub's answer: {e}"))?;
    parse_release(&json).map(Check::Latest)
}

/// Folder the installer is downloaded into.
pub fn download_dir() -> PathBuf {
    std::env::temp_dir().join("RustCommander-update")
}

/// Downloads `asset` into `dir` (emptied first), calling `progress` with
/// the bytes received so far. The file must come from this repository's
/// release download URL (redirects only to GitHub's hosts), have exactly
/// the size the API reported, and match its SHA-256 when one was given.
/// Returns the finished file; `Ok(None)` when cancelled.
pub fn download(
    asset: &Asset,
    dir: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<Option<PathBuf>, String> {
    if !asset.url.starts_with(DOWNLOAD_PREFIX) || asset.name.contains(['/', '\\', ':']) || asset.name.starts_with('.') {
        return Err("The download isn't a release file of RustCommander".into());
    }
    if dir.exists() {
        let _ = std::fs::remove_dir_all(dir);
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("Can't create {}: {e}", dir.display()))?;
    let part = dir.join(format!("{}.part", asset.name));
    let result = fetch(asset, &part, cancel, &mut progress);
    match result {
        Ok(true) => {
            let target = dir.join(&asset.name);
            std::fs::rename(&part, &target).map_err(|e| format!("Can't finish the download: {e}"))?;
            Ok(Some(target))
        }
        Ok(false) => {
            let _ = std::fs::remove_file(&part);
            Ok(None)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

/// Streams the asset into `part`; false when cancelled.
fn fetch(asset: &Asset, part: &Path, cancel: &AtomicBool, progress: &mut impl FnMut(u64)) -> Result<bool, String> {
    use ureq::ResponseExt;
    let response = agent(None)
        .get(&asset.url)
        .header("Accept", "application/octet-stream")
        .call()
        .map_err(|e| format!("Download failed: {e}"))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("Download failed: HTTP {status}"));
    }
    let host = response.get_uri().host().unwrap_or("").to_ascii_lowercase();
    if !DOWNLOAD_HOSTS.contains(&host.as_str()) {
        return Err(format!("Download was redirected to an unexpected host ({host})"));
    }
    let body = response.into_body();
    if body.content_length().is_some_and(|n| n != asset.size) {
        return Err("Download has the wrong size".into());
    }
    let mut reader = body.into_reader();
    let mut file = std::fs::File::create(part).map_err(|e| format!("Can't write {}: {e}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut received = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let n = reader.read(&mut buf).map_err(|e| format!("Download failed: {e}"))?;
        if n == 0 {
            break;
        }
        received += n as u64;
        if received > asset.size {
            return Err("Download is larger than announced".into());
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| format!("Can't write {}: {e}", part.display()))?;
        progress(received);
    }
    file.sync_all().map_err(|e| format!("Can't write {}: {e}", part.display()))?;
    if received != asset.size {
        return Err(format!("Download incomplete ({received} of {} bytes)", asset.size));
    }
    if let Some(expected) = &asset.sha256 {
        let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if &got != expected {
            return Err("Download is damaged (checksum mismatch)".into());
        }
    }
    Ok(true)
}

/// True when this exe was installed by the setup program (its uninstaller
/// sits next to it); false for the portable zip and development builds.
pub fn is_installed() -> bool {
    let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_owned)) else { return false };
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            name.starts_with("unins") && name.ends_with(".exe")
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r###"{
      "url": "https://api.github.com/repos/mongo56/RustCommander/releases/1",
      "html_url": "https://github.com/mongo56/RustCommander/releases/tag/v0.9.1",
      "tag_name": "v0.9.1",
      "name": "RustCommander 0.9.1",
      "draft": false,
      "prerelease": false,
      "body": "## What's new\r\n\r\n- **Update** command\r\n- Faster `zip` writes\r\n\r\n\r\nThanks!",
      "assets": [
        {
          "name": "RustCommander-0.9.1-win64.zip",
          "size": 4000000,
          "state": "uploaded",
          "browser_download_url": "https://github.com/mongo56/RustCommander/releases/download/v0.9.1/RustCommander-0.9.1-win64.zip"
        },
        {
          "name": "RustCommander-0.9.1-setup.exe",
          "size": 5123456,
          "state": "uploaded",
          "digest": "sha256:ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
          "browser_download_url": "https://github.com/mongo56/RustCommander/releases/download/v0.9.1/RustCommander-0.9.1-setup.exe"
        }
      ]
    }"###;

    #[test]
    fn versions_parse_and_compare() {
        assert_eq!(Version::parse("v0.5.7"), Some(Version(0, 5, 7)));
        assert_eq!(Version::parse("0.5.7"), Some(Version(0, 5, 7)));
        assert_eq!(Version::parse(" V1.2 "), Some(Version(1, 2, 0)));
        assert_eq!(Version::parse("2"), Some(Version(2, 0, 0)));
        assert_eq!(Version::parse("1.0.0-beta.2"), Some(Version(1, 0, 0)));
        assert_eq!(Version::parse("v1.2.3.4"), None);
        assert_eq!(Version::parse("latest"), None);
        assert_eq!(Version::parse(""), None);
        assert!(Version(0, 5, 10) > Version(0, 5, 9));
        assert!(Version(0, 10, 0) > Version(0, 9, 99));
        assert!(Version(1, 0, 0) > Version(0, 99, 99));
        assert_eq!(Version(0, 5, 7).to_string(), "0.5.7");
        assert_eq!(Version::current().to_string(), crate::VERSION);
    }

    #[test]
    fn release_json_parses() {
        let r = parse_release(SAMPLE).unwrap();
        assert_eq!(r.version, Version(0, 9, 1));
        assert_eq!(r.tag, "v0.9.1");
        assert_eq!(r.page, "https://github.com/mongo56/RustCommander/releases/tag/v0.9.1");
        let setup = r.setup.unwrap();
        assert_eq!(setup.name, "RustCommander-0.9.1-setup.exe");
        assert_eq!(setup.size, 5123456);
        assert_eq!(setup.sha256.as_deref(), Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"));
        assert_eq!(r.notes, "What's new\n\n\u{2022} Update command\n\u{2022} Faster zip writes\n\nThanks!");
    }

    #[test]
    fn tag_without_v_and_newer_check() {
        let json = SAMPLE.replace("v0.9.1", "0.9.1");
        let r = parse_release(&json).unwrap();
        assert!(r.setup.is_some());
        let old = Release { version: Version(0, 0, 1), ..r.clone() };
        assert!(!old.is_newer());
        let new = Release { version: Version(999, 0, 0), ..r };
        assert!(new.is_newer());
    }

    #[test]
    fn foreign_or_mismatched_assets_are_ignored() {
        // Asset of another repository.
        let json = SAMPLE.replace(
            "https://github.com/mongo56/RustCommander/releases/download/v0.9.1/RustCommander-0.9.1-setup.exe",
            "https://evil.example/RustCommander-0.9.1-setup.exe",
        );
        assert_eq!(parse_release(&json).unwrap().setup, None);
        // Installer of another version.
        let json = SAMPLE.replace("RustCommander-0.9.1-setup.exe", "RustCommander-0.9.0-setup.exe");
        assert_eq!(parse_release(&json).unwrap().setup, None);
        // Upload not finished.
        let json = SAMPLE.replace(r#""size": 5123456,
          "state": "uploaded""#, r#""size": 5123456,
          "state": "starter""#);
        assert_eq!(parse_release(&json).unwrap().setup, None);
        // No digest: still fine, just unchecked.
        let json = SAMPLE.replace(r#""digest": "sha256:ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789","#, "");
        assert_eq!(parse_release(&json).unwrap().setup.unwrap().sha256, None);
        // Odd page URL falls back to the releases page.
        let json = SAMPLE.replace("https://github.com/mongo56/RustCommander/releases/tag/v0.9.1", "https://evil.example/");
        assert_eq!(parse_release(&json).unwrap().page, RELEASES_PAGE);
    }

    #[test]
    fn bad_answers_are_errors() {
        assert!(parse_release("not json").is_err());
        assert!(parse_release(r#"{"tag_name": "nightly", "assets": []}"#).is_err());
        assert!(parse_release(r#"{"tag_name": "v1.0.0", "draft": true}"#).is_err());
        let minimal = parse_release(r#"{"tag_name": "v1.0.0"}"#).unwrap();
        assert_eq!(minimal.setup, None);
        assert_eq!(minimal.notes, "");
        assert_eq!(minimal.page, RELEASES_PAGE);
    }

    #[test]
    fn long_notes_are_cut() {
        let body: String = (1..=30).map(|i| format!("- item {i}\n")).collect();
        let notes = summary(&body);
        assert_eq!(notes.lines().count(), NOTES_LINES + 1);
        assert!(notes.ends_with('\u{2026}'));
        let long = "x".repeat(2000);
        let notes = summary(&long);
        assert_eq!(notes.chars().count(), NOTES_CHARS + 1);
    }

    #[test]
    fn download_refuses_foreign_urls() {
        let asset = Asset { name: "x.exe".into(), url: "https://example.com/x.exe".into(), size: 1, sha256: None };
        let dir = std::env::temp_dir().join(format!("rc-update-test-{}", std::process::id()));
        let cancel = AtomicBool::new(false);
        assert!(download(&asset, &dir, &cancel, |_| {}).is_err());
        let sneaky = Asset { name: "..\\x.exe".into(), url: format!("{DOWNLOAD_PREFIX}v1/x.exe"), size: 1, sha256: None };
        assert!(download(&sneaky, &dir, &cancel, |_| {}).is_err());
        assert!(!dir.exists());
    }

    /// The real API, read-only: `cargo test -p rustcommander live_update_check -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_update_check() {
        let check = check();
        println!("{check:?}");
        assert!(check.is_ok());
    }

    /// Downloads the latest installer into a test folder (never runs it)
    /// and removes it again:
    /// `cargo test -p rustcommander live_update_download -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_update_download() {
        let Ok(Check::Latest(release)) = check() else { panic!("no release") };
        let asset = release.setup.expect("installer asset");
        let dir = std::env::temp_dir().join(format!("rc-update-download-{}", std::process::id()));
        let cancel = AtomicBool::new(false);
        let mut calls = 0;
        let file = download(&asset, &dir, &cancel, |_| calls += 1).unwrap().unwrap();
        println!("{} ({} bytes, sha256 {:?}, {calls} progress calls)", file.display(), asset.size, asset.sha256);
        assert_eq!(std::fs::metadata(&file).unwrap().len(), asset.size);
        std::fs::remove_dir_all(&dir).unwrap();
        // A wrong checksum is refused and leaves nothing behind.
        let bad = Asset { sha256: Some("0".repeat(64)), ..asset };
        assert!(download(&bad, &dir, &cancel, |_| {}).is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
