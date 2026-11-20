//! OS notifications. The backend is the only process that raises them, so a
//! toast arrives with the UI closed; the UI renders the same entries from its
//! notification center.

use crate::gql;
use crate::http::Client;

/// `AppUserModelId` the toasts are shown under; registered per user on startup
/// so the banner shows this app instead of the PowerShell fallback.
#[cfg(windows)]
const AUMID: &str = "twitch-siphon";
#[cfg(windows)]
const DISPLAY_NAME: &str = "Siphon";

/// Bundled icon, written to disk for the toast registration.
#[cfg(windows)]
static ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

/// Registers the per-user AUMID so Windows attributes the toast to us.
#[cfg_attr(
    not(windows),
    expect(
        clippy::missing_const_for_fn,
        reason = "the Windows body writes a file, so both definitions share one signature"
    )
)]
pub fn register_aumid() {
    #[cfg(windows)]
    {
        let icon_path = std::env::temp_dir().join("twitch-siphon-icon.png");
        if let Err(error) = std::fs::write(&icon_path, ICON_PNG) {
            tracing::info!(target: "notifier", "failed to write notification icon: {error}");
            return;
        }
        let result = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
            .create_subkey(format!("Software\\Classes\\AppUserModelId\\{AUMID}"))
            .and_then(|(key, _)| {
                key.set_value("DisplayName", &DISPLAY_NAME)?;
                key.set_value("IconUri", &icon_path.as_os_str())
            });
        if let Err(error) = result {
            tracing::info!(target: "notifier", "failed to register aumid: {error}");
        }
    }
}

/// Shows one toast. Images are downloaded once and cached by file name.
pub async fn show(
    client: &Client,
    summary: &str,
    body: &str,
    sound: bool,
    image: Option<&str>,
    login: Option<&str>,
) {
    tracing::info!(target: "notifier", "showing notification: {summary} | {body}");
    let image = resolve_image(client, image).await;
    show_toast(
        summary,
        body,
        sound,
        image.as_deref(),
        login.map(str::to_owned),
    );
}

async fn resolve_image(client: &Client, image: Option<&str>) -> Option<String> {
    let url = image?;
    let file_name = url.rsplit('/').next().filter(|name| !name.is_empty())?;
    let dir = std::env::temp_dir().join("twitch-siphon-avatars");
    let path = dir.join(file_name);
    if compio::fs::metadata(&path).await.is_err()
        && let Err(error) = gql::fetch_file(client, url, &path).await
    {
        tracing::info!(target: "notifier", "failed to download avatar: {error}");
        return None;
    }
    path.into_os_string().into_string().ok()
}

fn show_toast(summary: &str, body: &str, sound: bool, image: Option<&str>, login: Option<String>) {
    #[cfg(windows)]
    {
        use tauri_winrt_notification::Duration;
        use tauri_winrt_notification::Sound;
        use tauri_winrt_notification::Toast;

        let mut toast = Toast::new(AUMID)
            .title(summary)
            .text2(body)
            .duration(Duration::Long)
            .sound(if sound { Some(Sound::Default) } else { None });
        if login.is_some() {
            toast = toast.add_button("Watch", "open");
        }
        if let Some(image) = image {
            toast = toast.image(std::path::Path::new(image), "");
        }
        if let Some(login) = login {
            toast = toast.on_activated(move |_| {
                open_url(&format!("https://www.twitch.tv/{login}"));
                Ok(())
            });
        }
        if let Err(error) = toast.show() {
            tracing::info!(target: "notifier", "failed to show notification: {error}");
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (summary, body, sound, image, login);
    }
}

#[cfg(windows)]
fn open_url(url: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
}
