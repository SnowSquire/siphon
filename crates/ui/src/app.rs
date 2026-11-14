//! Slint foreground: applies RPC snapshots to the retained window and forwards
//! callbacks to the RPC worker as requests.

use std::rc::Rc;
use std::sync::Arc;

use kanal::Receiver;
use siphon_rpc::ChannelView;
use siphon_rpc::ConnHealth;
use siphon_rpc::Request;
use slint::ComponentHandle as _;
use slint::SharedString;
use slint::VecModel;

use crate::format::format_iso_ms;
use crate::rpc::GuiEvent;
use crate::rpc::IntentSender;
use crate::rpc::SharedSnapshot;
use crate::rpc::TrayAction;
use crate::rpc::UiCommand;
use crate::ui::AppWindow;
use crate::ui::ChannelRow;
use crate::ui::ConnState;
use crate::ui::SiphonTray;

pub struct ViewParams {
    pub intents: IntentSender,
    pub shared: Arc<SharedSnapshot>,
    pub gui_rx: Receiver<GuiEvent>,
    pub single: app_single_instance::PrimaryHandle,
}

/// Owns the tray icon and the primary-instance guard: dropping either removes
/// the icon or releases primary status, so both outlive `run`.
pub struct Guards {
    _tray: Option<SiphonTray>,
    _single: app_single_instance::PrimaryHandle,
}

pub fn build(params: ViewParams) -> (AppWindow, Guards) {
    let ui = AppWindow::new().expect("slint window");
    wire(&ui, &params.intents);
    {
        let snapshot = params.shared.read().clone();
        apply_snapshot(&ui, &snapshot);
    }
    let tray = build_tray(&ui);
    let has_tray = tray.is_some();
    arm_minimize_to_tray(&ui, has_tray);
    ui.window().on_close_requested(move || {
        if has_tray {
            trim_working_set();
        } else {
            // Without a tray a hidden window would strand invisible.
            let _ = slint::quit_event_loop();
        }
        slint::CloseRequestResponse::HideWindow
    });
    spawn_pump(&ui, Arc::clone(&params.shared), params.gui_rx);
    let guards = Guards {
        _tray: tray,
        _single: params.single,
    };
    (ui, guards)
}

fn build_tray(ui: &AppWindow) -> Option<SiphonTray> {
    let tray = match SiphonTray::new() {
        Ok(tray) => tray,
        Err(error) => {
            tracing::error!(target: "tray", "tray build failed: {error}");
            return None;
        }
    };
    let weak = ui.as_weak();
    tray.on_show_window(move || {
        if let Some(ui) = weak.upgrade() {
            tracing::info!(target: "tray", "tray Open, showing window");
            let _ = ui.show();
        }
    });
    tray.on_restart_service(|| {
        tracing::info!(target: "tray", "restart service requested");
        if let Err(error) = std::thread::Builder::new()
            .name("service restart".to_owned())
            .spawn(crate::service::restart)
        {
            tracing::error!(target: "service", "could not start the restart thread: {error}");
        }
    });
    tray.on_quit_app(|| {
        let _ = slint::quit_event_loop();
    });
    if let Err(error) = tray.show() {
        tracing::error!(target: "tray", "tray show failed: {error}");
        return None;
    }
    Some(tray)
}

const MINIMIZE_SUBCLASS_ID: usize = 1;
const MINIMIZE_HOOK_ATTEMPTS: u32 = 50;
const MINIMIZE_HOOK_RETRY_IN: std::time::Duration = std::time::Duration::from_millis(100);

/// Minimize hides to the tray instead of the taskbar. Slint exposes no
/// minimize event, so a comctl subclass swallows `SC_MINIMIZE`.
fn arm_minimize_to_tray(ui: &AppWindow, has_tray: bool) {
    if !has_tray {
        return;
    }
    let weak = ui.as_weak();
    slint::Timer::single_shot(MINIMIZE_HOOK_RETRY_IN, move || {
        install_minimize_hook(&weak, MINIMIZE_HOOK_ATTEMPTS);
    });
}

fn install_minimize_hook(weak: &slint::Weak<AppWindow>, attempts: u32) {
    let Some(ui) = weak.upgrade() else { return };
    let Some(hwnd) = slint_hwnd(&ui) else {
        if attempts > 0 {
            let weak = weak.clone();
            slint::Timer::single_shot(MINIMIZE_HOOK_RETRY_IN, move || {
                install_minimize_hook(&weak, attempts - 1);
            });
        } else {
            tracing::info!(target: "app", "minimize-to-tray hook found no window");
        }
        return;
    };
    // SAFETY: `hwnd` is our own live window on this GUI thread; the subclass
    // chains to winit's procedure and comctl drops it with the window.
    let installed = unsafe {
        windows_sys::Win32::UI::Shell::SetWindowSubclass(
            hwnd,
            Some(minimize_subclass),
            MINIMIZE_SUBCLASS_ID,
            0,
        )
    };
    if installed == 0 {
        tracing::info!(target: "app", "minimize-to-tray hook failed to install");
    }
}

fn slint_hwnd(ui: &AppWindow) -> Option<windows_sys::Win32::Foundation::HWND> {
    use raw_window_handle::HasWindowHandle as _;
    use raw_window_handle::RawWindowHandle;

    let provider = ui.window().window_handle();
    let raw = provider.window_handle().ok()?;
    match raw.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as _),
        _ => None,
    }
}

/// SAFETY: our own live window; hiding only toggles visibility.
unsafe extern "system" fn minimize_subclass(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
    _id: usize,
    _data: usize,
) -> windows_sys::Win32::Foundation::LRESULT {
    if is_minimize_command(msg, wparam) {
        // SAFETY: our own live window; hiding only toggles visibility.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
                hwnd,
                windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE,
            )
        };
        trim_working_set();
        tracing::info!(target: "tray", "minimize hid to tray");
        return 0;
    }
    // SAFETY: default subclass chaining for everything else.
    unsafe { windows_sys::Win32::UI::Shell::DefSubclassProc(hwnd, msg, wparam, lparam) }
}

const fn is_minimize_command(msg: u32, wparam: usize) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging as wam;

    msg == wam::WM_SYSCOMMAND && wparam & 0xFFF0 == wam::SC_MINIMIZE as usize
}

fn wire(ui: &AppWindow, intents: &IntentSender) {
    let send = {
        let intents = intents.clone();
        move |request| intents.send(UiCommand::Request(request))
    };

    ui.on_add_login({
        let send = send.clone();
        move |text| {
            let value = text.trim().to_owned();
            if !value.is_empty() {
                send(Request::AddLogin(value));
            }
        }
    });
    ui.on_remove_channel({
        let send = send.clone();
        move |id| {
            if let Ok(channel) = id.parse::<u64>() {
                send(Request::RemoveChannel(channel));
            } else {
                tracing::info!(target: "app", "bad channel id {id}");
            }
        }
    });
    ui.on_add_word({
        let send = send.clone();
        move |text| {
            let value = text.trim().to_owned();
            if !value.is_empty() {
                send(Request::AddFilteredWord(value));
            }
        }
    });
    ui.on_remove_word({
        let send = send.clone();
        move |index| {
            if index >= 0 {
                send(Request::RemoveFilteredWord(
                    u32::try_from(index).unwrap_or(u32::MAX),
                ));
            }
        }
    });
    ui.on_set_notify({
        let send = send.clone();
        move |checked| send(Request::SetNotifyTitleChanges(checked))
    });
    ui.on_set_sound({
        let send = send.clone();
        move |checked| send(Request::SetSound(checked))
    });
    ui.on_apply_update({
        let send = send.clone();
        move || send(Request::ApplyUpdate)
    });
    ui.on_clear_error(move || send(Request::ClearError));
}

/// Dedicated pump thread: blocks on the worker channel and replays each event
/// onto the UI thread through the weak handle.
fn spawn_pump(ui: &AppWindow, shared: Arc<SharedSnapshot>, gui_rx: Receiver<GuiEvent>) {
    let weak = ui.as_weak();
    std::thread::Builder::new()
        .name("slint pump".to_owned())
        .spawn(move || {
            while let Ok(event) = gui_rx.recv() {
                match event {
                    GuiEvent::Snapshot => {
                        let snapshot = shared.read().clone();
                        if weak
                            .upgrade_in_event_loop(move |ui| apply_snapshot(&ui, &snapshot))
                            .is_err()
                        {
                            break;
                        }
                    }
                    GuiEvent::Tray(action) => {
                        let snapshot = shared.read().clone();
                        if weak
                            .upgrade_in_event_loop(move |ui| {
                                apply_snapshot(&ui, &snapshot);
                                match action {
                                    TrayAction::Show => {
                                        let _ = ui.window().show();
                                    }
                                }
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        })
        .expect("slint pump thread");
}

fn apply_snapshot(ui: &AppWindow, snapshot: &siphon_rpc::Snapshot) {
    ui.set_conn_state(match snapshot.conn {
        ConnHealth::Connected => ConnState::Connected,
        ConnHealth::Connecting => ConnState::Connecting,
        ConnHealth::NoChannels => ConnState::NoChannels,
    });
    ui.set_error_text(SharedString::from(
        snapshot.error.as_deref().unwrap_or_default(),
    ));
    if let Some(offer) = &snapshot.update {
        ui.set_has_update(true);
        ui.set_update_text(SharedString::from(
            format!("Update to {}", offer.version).as_str(),
        ));
    } else {
        ui.set_has_update(false);
        ui.set_update_text(SharedString::new());
    }
    ui.set_notify_titles(snapshot.config.notify_title_changes);
    ui.set_sound(snapshot.config.sound);

    let rows: Vec<ChannelRow> = snapshot
        .config
        .channels
        .iter()
        .map(|channel| {
            let resolved = snapshot.channels.iter().find(|view| view.id == channel.id);
            let name = resolved
                .map(|view| view.display_name.as_str())
                .or(channel.display_name.as_deref())
                .unwrap_or(channel.login.as_str());
            let title = resolved
                .and_then(|view| view.title.clone())
                .unwrap_or_default();
            let viewers = resolved.map_or_else(|| "—".to_owned(), live_viewers);
            let detail =
                resolved.map_or_else(String::new, |view| detail_line(&channel.login, view));
            ChannelRow {
                id: SharedString::from(channel.id.to_string().as_str()),
                name: SharedString::from(name),
                title: SharedString::from(title.as_str()),
                viewers: SharedString::from(viewers.as_str()),
                detail: SharedString::from(detail.as_str()),
                pending: false,
            }
        })
        .chain(snapshot.pending.iter().map(|login| ChannelRow {
            id: SharedString::new(),
            name: SharedString::from(login.as_str()),
            title: SharedString::from("…"),
            viewers: SharedString::from("…"),
            detail: SharedString::new(),
            pending: true,
        }))
        .collect();
    ui.set_channels(Rc::new(VecModel::from(rows)).into());

    let words: Vec<SharedString> = snapshot
        .config
        .filtered_words
        .iter()
        .map(|word| SharedString::from(word.as_str()))
        .collect();
    ui.set_words(Rc::new(VecModel::from(words)).into());
}

fn detail_line(login: &str, view: &ChannelView) -> String {
    let game = view.game.as_deref().unwrap_or("no game");
    let start = view
        .stream_start_ms
        .map_or_else(|| "—".to_owned(), format_iso_ms);
    let counts = format!(
        "{}/{}",
        viewers_text(view.viewers),
        viewers_text(view.collaboration_viewers)
    );
    format!("{login} • {game} • {start} • {counts}")
}

fn live_viewers(view: &ChannelView) -> String {
    if !view.live {
        return "offline".to_owned();
    }
    view.viewers
        .map_or_else(|| "—".to_owned(), |viewers| viewers.to_string())
}

fn viewers_text(viewers: Option<u32>) -> String {
    viewers.map_or_else(|| "—".to_owned(), |viewers| viewers.to_string())
}

/// A hidden tray app needs no resident pages: page everything out and let
/// faults bring back only what the threads touch.
fn trim_working_set() {
    use windows_sys::Win32::System::ProcessStatus::GetProcessMemoryInfo;
    use windows_sys::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    use windows_sys::Win32::System::Threading::SetProcessWorkingSetSize;

    // SAFETY: the process handle is ours and the size arguments are sentinels
    // that ask Windows to trim the working set.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: the process handle is ours.
    unsafe { SetProcessWorkingSetSize(process, usize::MAX, usize::MAX) };
    // SAFETY: `PROCESS_MEMORY_COUNTERS` is a plain C struct; all zeroes is its
    // valid initial state.
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    // SAFETY: `counters` is a live, correctly sized struct; the handle is ours.
    let read = unsafe {
        GetProcessMemoryInfo(
            process,
            &raw mut counters,
            u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS>()).unwrap_or(u32::MAX),
        )
    };
    if read != 0 {
        tracing::info!(
            target: "app",
            "trimmed working set to {} MB",
            counters.WorkingSetSize / 1024 / 1024,
        );
    }
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::UI::WindowsAndMessaging as wam;

    use super::is_minimize_command;

    #[test]
    fn minimize_command_masks_sys_bits() {
        assert!(is_minimize_command(
            wam::WM_SYSCOMMAND,
            wam::SC_MINIMIZE as usize
        ));
        assert!(is_minimize_command(
            wam::WM_SYSCOMMAND,
            wam::SC_MINIMIZE as usize | 0x000F
        ));
        assert!(!is_minimize_command(
            wam::WM_SYSCOMMAND,
            wam::SC_CLOSE as usize
        ));
        assert!(!is_minimize_command(
            wam::WM_CLOSE,
            wam::SC_MINIMIZE as usize
        ));
    }
}
