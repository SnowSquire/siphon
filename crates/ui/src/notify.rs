//! OS notifications.
//!
//! The window raises them now, not the daemon: the daemon publishes stream
//! transitions and this decides whether one is worth a toast. The image is a
//! local file the daemon already cached next to the avatar, so nothing is
//! downloaded here.

/// `AppUserModelId` the toasts are shown under; registered per user on startup
/// so the banner shows this app instead of the PowerShell fallback.
#[cfg(windows)]
const AUMID: &str = "twitch-siphon";
#[cfg(windows)]
const DISPLAY_NAME: &str = "Siphon";

/// The tray's icon, written to disk for the toast registration.
#[cfg(windows)]
static ICON_PNG: &[u8] = include_bytes!("../icons/icon.png");

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
            tracing::info!(target: "notify", "failed to write notification icon: {error}");
            return;
        }
        let result = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
            .create_subkey(format!("Software\\Classes\\AppUserModelId\\{AUMID}"))
            .and_then(|(key, _)| {
                key.set_value("DisplayName", &DISPLAY_NAME)?;
                key.set_value("IconUri", &icon_path.as_os_str())
            });
        if let Err(error) = result {
            tracing::info!(target: "notify", "failed to register aumid: {error}");
        }
    }
}

/// Shows one toast. `image` is a local path, `login` opens the channel.
pub fn show(summary: &str, body: &str, sound: bool, image: Option<&str>, login: Option<&str>) {
    tracing::info!(target: "notify", "showing notification: {summary} | {body}");
    show_toast(summary, body, sound, image, login.map(str::to_owned));
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
            tracing::info!(target: "notify", "failed to show notification: {error}");
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (summary, body, sound, image, login);
    }
}

#[cfg(windows)]
fn open_url(url: &str) {
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
}
