use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

use crate::logging::log;

pub fn notify(app: &AppHandle, summary: &str, body: &str, sound: bool) {
    let app = app.clone();
    let (summary, body) = (summary.to_owned(), body.to_owned());
    // showing a toast can block; keep that off the hermes thread
    std::thread::spawn(move || {
        log(
            "notifier",
            format!("showing notification: {summary} | {body}"),
        );
        let builder = app
            .notification()
            .builder()
            .title(summary)
            .body(body)
            .sound(if sound { "Default" } else { "Silent" });
        if let Err(error) = builder.show() {
            log("notifier", format!("failed to show notification: {error}"));
        }
    });
}
