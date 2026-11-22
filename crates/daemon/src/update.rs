//! Update checking and installer hand-off.
//!
//! The backend owns the check and the installer run; the UI only renders the
//! offer and sends `ApplyUpdate`. The installer closes both processes and the
//! waiter opens the window again, which starts a daemon of its own.

use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use thiserror::Error;

use crate::constants::MAX_JSON;
use crate::constants::MAX_UPDATE;
use crate::constants::RELEASES_URL;
pub use crate::constants::UPDATE_CHECK_INTERVAL as CHECK_INTERVAL;
use crate::constants::UPDATE_TIMEOUT;
use crate::http::Client;
#[cfg(windows)]
const INSTALL_KEY: &str = "Software\\Ozeniken\\Siphon";

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("http: {0}")]
    Http(#[from] crate::http::Error),
    #[error("json: {0}")]
    Json(#[from] sonic_rs::Error),
    #[error("update check returned status {0}")]
    Status(crate::http::StatusCode),
    #[error("request timed out")]
    Timeout,
    #[error("{0}")]
    Invalid(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Only produced on platforms without the installer hand-off.
    #[allow(dead_code)]
    #[error("no installer for this build")]
    NoInstaller,
}

/// A published release worth offering: the version plus where to get it.
/// `installer_url` is `None` when the release carries no installer asset.
#[derive(Clone, Debug)]
pub struct Release {
    pub version: semver::Version,
    pub installer_url: Option<String>,
    pub page_url: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseJson {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<AssetJson>,
}

#[derive(Debug, Deserialize)]
struct AssetJson {
    name: String,
    browser_download_url: String,
}

/// Decodes one `releases/latest` body. Tags carry a `v` prefix (`v0.1.3`);
/// anything that is not semver after stripping it is refused.
///
/// The installer is the `-setup.exe` asset: a portable build or an archive can
/// sit in the same release without being mistaken for one.
pub fn parse_release(body: &[u8]) -> Result<Release, UpdateError> {
    let json: ReleaseJson = sonic_rs::from_slice(body)?;
    let tag = json.tag_name.strip_prefix('v').unwrap_or(&json.tag_name);
    let version = semver::Version::parse(tag).map_err(|error| {
        UpdateError::Invalid(format!("bad release tag {}: {error}", json.tag_name))
    })?;
    let installer_url = json
        .assets
        .into_iter()
        .find(|asset| asset.name.to_lowercase().ends_with("-setup.exe"))
        .map(|asset| asset.browser_download_url);
    Ok(Release {
        version,
        installer_url,
        page_url: json.html_url,
    })
}

/// True when the release is strictly newer than this build.
#[must_use]
pub fn newer_than_current(release: &Release) -> bool {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).is_ok_and(|current| release.version > current)
}

/// Fetches the latest published release. Draft releases are invisible here.
pub async fn fetch_latest(client: &Client) -> Result<Release, UpdateError> {
    let mut response = crate::http::timeout(
        UPDATE_TIMEOUT,
        client.get_with(
            RELEASES_URL,
            &[
                ("User-Agent", "siphon"),
                ("Accept", "application/vnd.github+json"),
            ],
        ),
    )
    .await
    .map_err(|_| UpdateError::Timeout)??;
    if !response.status.is_success() {
        return Err(UpdateError::Status(response.status));
    }
    let body = crate::http::timeout(UPDATE_TIMEOUT, response.body.read_to_end(MAX_JSON))
        .await
        .map_err(|_| UpdateError::Timeout)??;
    parse_release(&body)
}

/// How this copy was installed: by the per-user installer or standalone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InstallMode {
    /// Constructed only by the Windows registry check.
    #[allow(dead_code)]
    Installed,
    Portable,
}

/// An installed copy lives under the `InstallDir` the installer recorded;
/// anything else (portable exe, dev build) is standalone.
#[cfg(windows)]
#[must_use]
pub fn install_mode() -> InstallMode {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let install_dir = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(INSTALL_KEY)
        .and_then(|key| key.get_value::<String, _>("InstallDir"))
        .ok()
        .map(PathBuf::from);
    match (exe_dir, install_dir) {
        (Some(exe), Some(installed)) if exe.starts_with(&installed) => InstallMode::Installed,
        _ => InstallMode::Portable,
    }
}

#[cfg(not(windows))]
#[must_use]
pub const fn install_mode() -> InstallMode {
    InstallMode::Portable
}

/// Downloads the installer to the temp dir. The body is read before the file
/// is written, so a failed download leaves no partial `.exe` behind.
pub async fn download_installer(
    client: &Client,
    url: &str,
    version: &semver::Version,
) -> Result<PathBuf, UpdateError> {
    let mut response = crate::http::timeout(UPDATE_TIMEOUT, client.get(url))
        .await
        .map_err(|_| UpdateError::Timeout)??;
    if !response.status.is_success() {
        return Err(UpdateError::Status(response.status));
    }
    let body = crate::http::timeout(UPDATE_TIMEOUT, response.body.read_to_end(MAX_UPDATE))
        .await
        .map_err(|_| UpdateError::Timeout)??;
    let path = std::env::temp_dir().join(format!("siphon-update-{version}.exe"));
    compio::fs::write(&path, body).await.0?;
    Ok(path)
}

/// Builds the waiter script: installs silently, then opens the window.
/// Single quotes cover spaces in both paths; embedded quotes are doubled.
#[cfg(any(windows, test))]
fn relaunch_script(installer: &Path, relaunch: &Path) -> String {
    let quote = |path: &Path| path.display().to_string().replace('\'', "''");
    format!(
        "Start-Process -FilePath '{}' -ArgumentList '/S' -Wait; Start-Process -FilePath '{}'",
        quote(installer),
        quote(relaunch),
    )
}

/// Launches the installer and returns immediately. The daemon exits so no
/// files are locked; the script reopens the window once the install is done.
#[cfg(windows)]
pub fn install_and_relaunch(installer: &Path) -> Result<(), UpdateError> {
    use std::os::windows::process::CommandExt as _;

    let exe = std::env::current_exe()?;
    // The installer closes both processes. Reopening the window also brings
    // back the daemon it starts; a tree without one falls back to this exe.
    let window = exe.with_file_name("siphon.exe");
    let relaunch = if window.is_file() { &window } else { &exe };
    let script = relaunch_script(installer, relaunch);
    std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
pub const fn install_and_relaunch(_installer: &Path) -> Result<(), UpdateError> {
    Err(UpdateError::NoInstaller)
}

/// Opens the release page for builds with no installer hand-off.
#[cfg(windows)]
pub fn open_page(url: &str) {
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
}

/// Opens the release page for builds with no installer hand-off.
#[cfg(not(windows))]
pub const fn open_page(_url: &str) {}

/// `CREATE_NO_WINDOW`: no console flashes for the installer waiter.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &[u8] = br#"{
        "tag_name": "v0.2.0",
        "html_url": "https://github.com/SnowSquire/twitch-siphon/releases/tag/v0.2.0",
        "assets": [
            {"name": "siphon_0.2.0_x64-portable.exe",
             "browser_download_url": "https://example.com/siphon_0.2.0_x64-portable.exe"},
            {"name": "siphon-0.2.0-x64-setup.exe",
             "browser_download_url": "https://example.com/siphon-0.2.0-x64-setup.exe"}
        ]
    }"#;

    #[test]
    fn parse_picks_version_installer_and_page() {
        let release = parse_release(BODY).unwrap();
        assert_eq!(release.version, semver::Version::new(0, 2, 0));
        assert_eq!(
            release.installer_url.as_deref(),
            Some("https://example.com/siphon-0.2.0-x64-setup.exe")
        );
        assert_eq!(
            release.page_url,
            "https://github.com/SnowSquire/twitch-siphon/releases/tag/v0.2.0"
        );
    }

    #[test]
    fn parse_ignores_the_portable_exe() {
        let body = br#"{"tag_name": "v0.2.0", "html_url": "https://example.com/r",
            "assets": [{"name": "siphon_0.2.0_x64-portable.exe",
                        "browser_download_url": "https://example.com/siphon-portable.exe"}]}"#;
        let release = parse_release(body).unwrap();
        assert!(release.installer_url.is_none());
    }

    #[test]
    fn parse_without_installer_yields_none() {
        let body = br#"{"tag_name": "v0.2.0", "html_url": "https://example.com/r",
            "assets": [{"name": "notes.txt", "browser_download_url": "https://example.com/n"}]}"#;
        let release = parse_release(body).unwrap();
        assert!(release.installer_url.is_none());
    }

    #[test]
    fn parse_rejects_non_semver_tag() {
        let body = br#"{"tag_name": "nightly", "html_url": "https://example.com/r", "assets": []}"#;
        assert!(parse_release(body).is_err());
    }

    #[test]
    fn relaunch_script_installs_before_reopening_and_quotes_paths() {
        let script = relaunch_script(
            Path::new("C:\\My Dir\\siphon-update-0.2.0.exe"),
            Path::new("C:\\App Dir\\siphon.exe"),
        );
        let install = script.find("/S").expect("installs silently");
        let open = script
            .find("C:\\App Dir\\siphon.exe")
            .expect("reopens the window");
        assert!(install < open, "reopen must wait for the install");
        assert!(script.contains("-Wait"));
        assert!(script.contains("'C:\\My Dir\\siphon-update-0.2.0.exe'"));
        assert!(script.contains("'C:\\App Dir\\siphon.exe'"));
    }
}
