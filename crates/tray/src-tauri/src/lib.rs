mod config;
pub mod gql;
pub mod hermes;
mod logging;
mod notifier;

use std::path::PathBuf;
use std::sync::Mutex;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, State, WindowEvent};

use crate::config::{Channel, Config};
use crate::hermes::{ChannelEntry, Settings};
use crate::logging::log;

struct AppState {
    config_path: PathBuf,
    settings_tx: kanal::Sender<Settings>,
    status: Mutex<Option<hermes::StatusEvent>>,
}

#[tauri::command]
fn get_config(state: State<'_, AppState>) -> Config {
    Config::load(&state.config_path)
}

#[tauri::command]
fn get_status(state: State<'_, AppState>) -> Option<hermes::StatusEvent> {
    state.status.lock().expect("status lock poisoned").clone()
}

#[tauri::command(async)]
fn save_config(state: State<'_, AppState>, config: Config) -> Result<(), String> {
    let mut config = config;
    log(
        "config",
        format!(
            "saving config: {} channel(s) {:?}, title_changes={}, sound={}",
            config.channels.len(),
            config.channels.iter().map(Channel::login).collect::<Vec<_>>(),
            config.notify_title_changes,
            config.sound,
        ),
    );
    resolve_config(&mut config);
    config
        .save(&state.config_path)
        .map_err(|error| error.to_string())?;
    send_settings(&state.settings_tx, &config)
}

/// gql-resolves logins that are missing ids and refreshes stored display
/// names, so the config file carries everything needed to render the ui
/// without talking to twitch.
fn resolve_config(config: &mut Config) {
    let unknown: Vec<String> = config
        .channels
        .iter()
        .filter(|channel| channel.id().is_none())
        .map(|channel| channel.login().to_owned())
        .collect();
    let known_ids: Vec<u64> = config.channels.iter().filter_map(Channel::id).collect();
    if unknown.is_empty() && known_ids.is_empty() {
        return;
    }
    match gql::fetch_users(&known_ids, &unknown) {
        Ok(users) => {
            for channel in &mut config.channels {
                match channel.id() {
                    None => {
                        if let Some(user) = users.iter().find(|user| {
                            user.channel_name.eq_ignore_ascii_case(channel.login())
                        }) {
                            log(
                                "config",
                                format!(
                                    "resolved {} to id {} ({})",
                                    channel.login(),
                                    user.channel_id,
                                    user.channel_display_name
                                ),
                            );
                            channel.set_resolved(
                                user.channel_id,
                                Some(user.channel_display_name.clone()),
                            );
                        } else {
                            log(
                                "config",
                                format!(
                                    "gql returned no channel named {}, is it a typo?",
                                    channel.login()
                                ),
                            );
                        }
                    }
                    Some(id) => {
                        if let Some(user) =
                            users.iter().find(|user| user.channel_id == id)
                        {
                            channel.set_display_name(user.channel_display_name.clone());
                        }
                    }
                }
            }
        }
        Err(error) => log("config", format!("channel resolution failed: {error}")),
    }
}

fn send_settings(settings_tx: &kanal::Sender<Settings>, config: &Config) -> Result<(), String> {
    settings_tx
        .send(Settings {
            channels: config
                .channels
                .iter()
                .map(|channel| ChannelEntry {
                    login: channel.login().to_owned(),
                    id: channel.id(),
                    display_name: channel.display_name().map(String::from),
                })
                .collect(),
            notify_title_changes: config.notify_title_changes,
            sound: config.sound,
        })
        .map_err(|error| error.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            get_config,
            get_status,
            save_config
        ])
        .setup(|app| {
            let config_path = app.path().app_config_dir()?.join("config.json");
            let mut config = Config::load(&config_path);
            log(
                "config",
                format!(
                    "loaded config from {}: {} channel(s) {:?}, title_changes={}, sound={}",
                    config_path.display(),
                    config.channels.len(),
                    config.channels.iter().map(Channel::login).collect::<Vec<_>>(),
                    config.notify_title_changes,
                    config.sound,
                ),
            );
            // resolve legacy/unresolved entries once at startup and persist
            // the result so the config carries ids + display names
            resolve_config(&mut config);
            config
                .save(&config_path)
                .map_err(|error| error.to_string())?;

            let (settings_tx, settings_rx) = kanal::unbounded();
            app.manage(AppState {
                config_path: config_path.clone(),
                settings_tx: settings_tx.clone(),
                status: Mutex::new(None),
            });
            hermes::spawn(app.handle().clone(), settings_rx);
            send_settings(&settings_tx, &config)?;

            let open = MenuItem::with_id(app, "open", "Open", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;
            TrayIconBuilder::with_id("main")
                .icon(
                    app.default_window_icon()
                        .expect("default window icon")
                        .clone(),
                )
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_window(tray.app_handle());
                    }
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // closing the window keeps the app running in the tray
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn show_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}
