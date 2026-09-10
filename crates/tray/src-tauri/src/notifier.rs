use notify_rust::Notification;

use crate::gql;
use crate::logging::log;

/// aumid the toasts are shown under; registered per-user on startup so the
/// notification banner shows us instead of the powershell fallback
#[cfg(target_os = "windows")]
const AUMID: &str = "twitch-siphon";
#[cfg(target_os = "windows")]
const DISPLAY_NAME: &str = "Siphon";

/// registers `HKCU\Software\Classes\AppUserModelId\{AUMID}` with our display
/// name and an icon unpacked to the temp dir; re-running every startup is
/// cheap and self-heals a deleted icon
#[cfg(target_os = "windows")]
pub fn register_aumid() {
    let icon_path = std::env::temp_dir().join("twitch-siphon-icon.png");
    if let Err(error) = std::fs::write(&icon_path, include_bytes!("../icons/icon.png")) {
        log(
            "notifier",
            format!("failed to write notification icon: {error}"),
        );
        return;
    }
    let result = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .create_subkey(format!("Software\\Classes\\AppUserModelId\\{AUMID}"))
        .and_then(|(key, _)| {
            key.set_value("DisplayName", &DISPLAY_NAME)?;
            key.set_value("IconUri", &icon_path.as_os_str())
        });
    if let Err(error) = result {
        log("notifier", format!("failed to register aumid: {error}"));
    }
}

/// downloads the avatar into a per-url temp cache file and returns the local
/// path; the cdn url embeds the avatar hash, so a changed picture lands in a
/// new file and stale ones are simply abandoned
async fn resolve_image(image: Option<&str>) -> Option<String> {
    let url = image?;
    let file_name = url
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())?;
    let dir = std::env::temp_dir().join("twitch-siphon-avatars");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(file_name);
    if !path.exists() {
        if let Err(error) = gql::fetch_file(url, &path).await {
            log("notifier", format!("failed to download avatar: {error}"));
            return None;
        }
    }
    // notify-rust takes paths as &str; bail out of impossibly-named temp dirs
    path.into_os_string().into_string().ok()
}

/// "Silent" intentionally fails `Sound::from_str(..)`; notify-rust maps the
/// failed parse to no sound at all, which is exactly the silent toast.
pub async fn notify(summary: &str, body: &str, sound: bool, image: Option<&str>) {
    log(
        "notifier",
        format!("showing notification: {summary} | {body}"),
    );
    // showing a toast can block; keep that off the async runtime
    #[cfg(target_os = "windows")]
    {
        let (summary, body) = (summary.to_owned(), body.to_owned());
        let image = resolve_image(image).await;
        let result = tauri::async_runtime::spawn_blocking(move || {
            let mut notification = Notification::new();
            let builder = notification
                .summary(&summary)
                .body(&body)
                .app_id(AUMID)
                .sound_name(if sound { "Default" } else { "Silent" });
            if let Some(image) = &image {
                builder.image_path(image);
            }
            builder.show()
        })
        .await;
        if let Err(error) = result {
            log("notifier", format!("failed to show notification: {error}"));
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let image = resolve_image(image).await;
        let mut notification = Notification::new();
        let builder = notification
            .summary(summary)
            .body(body)
            .sound_name(if sound { "Default" } else { "Silent" });
        if let Some(image) = &image {
            builder.image_path(image);
        }
        let result = builder.show_async().await;
        if let Err(error) = result {
            log("notifier", format!("failed to show notification: {error}"));
        }
    }
}
