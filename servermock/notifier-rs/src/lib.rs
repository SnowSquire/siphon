use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;

#[napi(object)]
pub struct NotifyOptions {
    pub subtitle: Option<String>,
    /// Windows only: AUMID of the app the toast is attributed to.
    pub app_id: Option<String>,
    pub sound: Option<bool>,
    pub timeout_ms: Option<u32>,
}

/// Show a native desktop notification (fire-and-forget; click events are
/// delivered on a background thread while the toast is alive).
#[napi]
pub fn notify(
    summary: String,
    body: String,
    options: Option<NotifyOptions>,
    on_click: Option<ThreadsafeFunction<()>>,
) -> napi::Result<()> {
    let options = options.unwrap_or(NotifyOptions {
        subtitle: None,
        app_id: None,
        sound: None,
        timeout_ms: None,
    });

    let mut notification = notify_rust::Notification::new();
    notification.summary(&summary).body(&body);

    if let Some(subtitle) = &options.subtitle {
        notification.subtitle(subtitle);
    }
    if let Some(sound) = options.sound {
        if !sound {
            notification.sound_name("ms-winsoundevent:Notification.Silent");
        }
    }
    if let Some(timeout_ms) = options.timeout_ms {
        notification.timeout(notify_rust::Timeout::Milliseconds(timeout_ms));
    }
    #[cfg(target_os = "windows")]
    if let Some(app_id) = &options.app_id {
        notification.app_id(app_id);
    }

    let handle = notification
        .show()
        .map_err(|error| napi::Error::from_reason(error.to_string()))?;

    if let Some(on_click) = on_click {
        // WinRT activation callbacks fire while this process is alive; watch
        // for them on a background thread so notify() returns immediately.
        std::thread::spawn(move || {
            let _ = handle.wait_for_response(|response: &notify_rust::NotificationResponse| {
                if matches!(response, notify_rust::NotificationResponse::Default) {
                    on_click.call(Ok(()), ThreadsafeFunctionCallMode::Blocking);
                }
            });
        });
    }

    Ok(())
}
