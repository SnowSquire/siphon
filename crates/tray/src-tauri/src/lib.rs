mod config;
mod gql;
mod hermes;
mod logging;
mod notifier;

use std::path::PathBuf;
use std::sync::Mutex;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, State, WindowEvent};

use crate::config::{Channel, Config};
use crate::hermes::ChannelEntry;
use crate::logging::log;

struct AppState {
    config_path: PathBuf,
    // the in-memory config is the source of truth; every mutation command
    // updates it, persists it to disk and forwards a hermes command
    config: Mutex<Config>,
    hermes_tx: kanal::Sender<hermes::Command>,
    status: Mutex<Option<hermes::StatusEvent>>,
}

#[tauri::command]
async fn get_config(state: State<'_, AppState>) -> Result<Config, ()> {
    Ok(state.config.lock().expect("config lock poisoned").clone())
}

#[tauri::command]
async fn get_status(state: State<'_, AppState>) -> Result<Option<hermes::StatusEvent>, ()> {
    Ok(state.status.lock().expect("status lock poisoned").clone())
}

#[tauri::command]
async fn add_channel(state: State<'_, AppState>, login: String) -> Result<(), String> {
    let login = login.trim().to_lowercase();
    let mut config = state.config.lock().expect("config lock poisoned").clone();
    if config
        .channels
        .iter()
        .any(|channel| channel.login().eq_ignore_ascii_case(&login))
    {
        return Ok(());
    }
    config.channels.push(Channel::Unresolved(login.clone()));
    resolve_config(&mut config).await;
    state.persist(&config)?;
    let added = config.channels.last().expect("channel just pushed");
    state
        .hermes_tx
        .send(hermes::Command::AddChannel(ChannelEntry {
            login: added.login().to_owned(),
            id: added.id(),
            display_name: added.display_name().map(String::from),
        }))
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn remove_channel(state: State<'_, AppState>, login: String) -> Result<(), String> {
    let mut config = state.config.lock().expect("config lock poisoned").clone();
    config
        .channels
        .retain(|channel| !channel.login().eq_ignore_ascii_case(&login));
    state.persist(&config)?;
    state
        .hermes_tx
        .send(hermes::Command::RemoveChannel(login))
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn set_notify_title_changes(state: State<'_, AppState>, value: bool) -> Result<(), String> {
    let mut config = state.config.lock().expect("config lock poisoned").clone();
    config.notify_title_changes = value;
    state.persist(&config)?;
    state
        .hermes_tx
        .send(hermes::Command::SetNotifyTitleChanges(value))
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn set_sound(state: State<'_, AppState>, value: bool) -> Result<(), String> {
    let mut config = state.config.lock().expect("config lock poisoned").clone();
    config.sound = value;
    state.persist(&config)?;
    state
        .hermes_tx
        .send(hermes::Command::SetSound(value))
        .map_err(|error| error.to_string())
}

impl AppState {
    fn persist(&self, config: &Config) -> Result<(), String> {
        *self.config.lock().expect("config lock poisoned") = config.clone();
        config
            .save(&self.config_path)
            .map_err(|error| error.to_string())
    }
}

/// gql-resolves logins that are missing ids and refreshes stored display
/// names, so the config file carries everything needed to render the ui
/// without talking to twitch.
async fn resolve_config(config: &mut Config) {
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
    match gql::fetch_users(&known_ids, &unknown).await {
        Ok(users) => {
            for channel in &mut config.channels {
                let known = channel.id();
                let user = users.iter().find(|user| {
                    known.is_some_and(|id| user.channel_id == id)
                        || user.channel_name.eq_ignore_ascii_case(channel.login())
                });
                match (user, known) {
                    (Some(user), None) => {
                        log(
                            "config",
                            format!(
                                "resolved {} to id {} ({})",
                                channel.login(),
                                user.channel_id,
                                user.channel_display_name
                            ),
                        );
                        channel
                            .set_resolved(user.channel_id, Some(user.channel_display_name.clone()));
                    }
                    // refresh login/display name in case the channel was renamed
                    (Some(user), Some(_)) => {
                        channel.set_login(user.channel_name.clone());
                        channel.set_display_name(user.channel_display_name.clone());
                    }
                    (None, None) => log(
                        "config",
                        format!(
                            "gql returned no channel named {}, is it a typo?",
                            channel.login()
                        ),
                    ),
                    (None, Some(_)) => {}
                }
            }
        }
        Err(error) => log("config", format!("channel resolution failed: {error}")),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // a second instance was launched: focus the existing window instead
            show_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            get_config,
            get_status,
            add_channel,
            remove_channel,
            set_notify_title_changes,
            set_sound
        ])
        .setup(|app| {
            // the aumid must exist before the first toast goes out
            #[cfg(target_os = "windows")]
            notifier::register_aumid();
            let config_path = app.path().app_config_dir()?.join("config.json");
            let mut config = Config::load(&config_path);
            log(
                "config",
                format!(
                    "loaded config from {}: {} channel(s) {:?}, title_changes={}, sound={}",
                    config_path.display(),
                    config.channels.len(),
                    config
                        .channels
                        .iter()
                        .map(Channel::login)
                        .collect::<Vec<_>>(),
                    config.notify_title_changes,
                    config.sound,
                ),
            );
            // resolve legacy/unresolved entries once at startup and persist
            // the result so the config carries ids + display names
            tauri::async_runtime::block_on(resolve_config(&mut config));
            config
                .save(&config_path)
                .map_err(|error| error.to_string())?;

            let (hermes_tx, command_rx) = kanal::unbounded();
            app.manage(AppState {
                config_path,
                config: Mutex::new(config.clone()),
                hermes_tx: hermes_tx.clone(),
                status: Mutex::new(None),
            });
            hermes::spawn(app.handle().clone(), command_rx);
            for channel in &config.channels {
                hermes_tx
                    .send(hermes::Command::AddChannel(ChannelEntry {
                        login: channel.login().to_owned(),
                        id: channel.id(),
                        display_name: channel.display_name().map(String::from),
                    }))
                    .map_err(|error| error.to_string())?;
            }
            hermes_tx
                .send(hermes::Command::SetNotifyTitleChanges(
                    config.notify_title_changes,
                ))
                .map_err(|error| error.to_string())?;
            hermes_tx
                .send(hermes::Command::SetSound(config.sound))
                .map_err(|error| error.to_string())?;

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
