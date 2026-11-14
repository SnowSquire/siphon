//! System tray icon with an Open/Quit menu; left-click shows the window
//! without opening the menu.
//!
//! A raw `Shell_NotifyIconW` icon on a hidden message window: the icon,
//! tooltip, and Open/Quit menu are registered directly with the shell, so
//! tray clicks arrive as window messages on the GUI thread and forward
//! into a [`kanal`] channel with sends that never block; the work task and
//! the GUI pump await tray events instead of polling. The channel sender
//! rides in the message window's user data, which is how the global window
//! procedure reaches per-window state. Notifications stay on
//! WinRT toasts in `notifier`, never on the legacy `NIF_INFO` balloon this
//! same struct could show.
//! The `Tray` value must stay alive for the icon to remain: dropping it
//! removes the icon, and `Tray` always owns one — absence of a tray is
//! `Option<Tray>::None` at the call site, never a flag inside `Tray`.

use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Context as _;
use kanal::Sender;
use windows_sys::Win32::Foundation::ERROR_CLASS_ALREADY_EXISTS;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Foundation::LPARAM;
use windows_sys::Win32::Foundation::LRESULT;
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::Foundation::WPARAM;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::NIF_ICON;
use windows_sys::Win32::UI::Shell::NIF_MESSAGE;
use windows_sys::Win32::UI::Shell::NIF_SHOWTIP;
use windows_sys::Win32::UI::Shell::NIF_TIP;
use windows_sys::Win32::UI::Shell::NIM_ADD;
use windows_sys::Win32::UI::Shell::NIM_DELETE;
use windows_sys::Win32::UI::Shell::NIM_SETVERSION;
use windows_sys::Win32::UI::Shell::NIN_SELECT;
use windows_sys::Win32::UI::Shell::NOTIFYICON_VERSION_4;
use windows_sys::Win32::UI::Shell::NOTIFYICONDATAW;
use windows_sys::Win32::UI::Shell::Shell_NotifyIconW;
use windows_sys::Win32::UI::WindowsAndMessaging::AppendMenuW;
use windows_sys::Win32::UI::WindowsAndMessaging::CreateIconFromResourceEx;
use windows_sys::Win32::UI::WindowsAndMessaging::CreatePopupMenu;
use windows_sys::Win32::UI::WindowsAndMessaging::CreateWindowExW;
use windows_sys::Win32::UI::WindowsAndMessaging::DefWindowProcW;
use windows_sys::Win32::UI::WindowsAndMessaging::DestroyIcon;
use windows_sys::Win32::UI::WindowsAndMessaging::DestroyMenu;
use windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::GWLP_USERDATA;
use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW;
use windows_sys::Win32::UI::WindowsAndMessaging::HICON;
use windows_sys::Win32::UI::WindowsAndMessaging::HMENU;
use windows_sys::Win32::UI::WindowsAndMessaging::HWND_MESSAGE;
use windows_sys::Win32::UI::WindowsAndMessaging::LR_DEFAULTCOLOR;
use windows_sys::Win32::UI::WindowsAndMessaging::LookupIconIdFromDirectoryEx;
use windows_sys::Win32::UI::WindowsAndMessaging::MF_STRING;
use windows_sys::Win32::UI::WindowsAndMessaging::RegisterClassW;
use windows_sys::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW;
use windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW;
use windows_sys::Win32::UI::WindowsAndMessaging::TPM_BOTTOMALIGN;
use windows_sys::Win32::UI::WindowsAndMessaging::TPM_LEFTALIGN;
use windows_sys::Win32::UI::WindowsAndMessaging::TPM_RETURNCMD;
use windows_sys::Win32::UI::WindowsAndMessaging::TrackPopupMenuEx;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_APP;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_CONTEXTMENU;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_LBUTTONDBLCLK;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_LBUTTONUP;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_RBUTTONDBLCLK;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_RBUTTONUP;
use windows_sys::Win32::UI::WindowsAndMessaging::WNDCLASSW;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayAction {
    Show,
    Restart,
    Quit,
}

/// Raw bytes of the bundled icon, included exactly once. The toast
/// registration writes these to disk.
#[allow(dead_code)]
pub static ICON_PNG: &[u8] = include_bytes!("../icons/icon.png");

/// Bundled `.ico`; the shell picks its preferred image through the lookup
/// call in `load_icon`.
static ICON_ICO: &[u8] = include_bytes!("../icons/icon.ico");

const TRAY_ID: u32 = 1;
/// Callback message the shell posts to the message window on tray input.
const WM_TRAY: u32 = WM_APP + 1;
/// Keyboard activation of the icon; `windows-sys` omits this `shellapi.h`
/// `NIN_*` value (`WM_USER + 1`), so it is spelled out here.
const NIN_KEYSELECT: u32 = 1025;
const MENU_OPEN: i32 = 1001;
const MENU_RESTART: i32 = 1002;
const MENU_QUIT: i32 = 1003;

#[cfg(debug_assertions)]
const TOOLTIP: &str = "Siphon Dev";
#[cfg(not(debug_assertions))]
const TOOLTIP: &str = "Siphon";

/// Owns the tray icon, its message window, and the channel sender tray
/// clicks forward into. Held alive for the app lifetime; dropping removes
/// the icon from the tray.
pub struct Tray {
    hwnd: HWND,
    icon: HICON,
    /// The sender allocation behind the window's user data pointer. The
    /// window is destroyed first in `Drop`, so the pointer never outlives
    /// what it points to.
    tx: *mut Sender<TrayAction>,
}

/// Builds the tray icon with clicks forwarding into `tx`. Call on the GUI
/// thread so the message window that receives the shell callbacks pumps
/// with the event loop.
pub fn build(tx: Sender<TrayAction>) -> anyhow::Result<Tray> {
    let tx = Box::new(tx);
    let hwnd = ensure_window(&tx)?;
    let icon = load_icon()?;
    add_icon(hwnd, icon)?;
    if let Ok(mut guard) = icon_state().lock() {
        *guard = Some(TrayIconState { hwnd, icon });
    }
    Ok(Tray {
        hwnd,
        icon,
        tx: Box::into_raw(tx),
    })
}

impl Drop for Tray {
    fn drop(&mut self) {
        let nid: NOTIFYICONDATAW = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ID,
            ..Default::default()
        };
        // SAFETY: populated with the id from `add_icon`; the shell stops
        // tracking the icon.
        unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
        // SAFETY: icon created by `load_icon` and owned by this value.
        unsafe { DestroyIcon(self.icon) };
        // SAFETY: message window created by `ensure_window` for this value.
        // Destroying it first retires the window procedure, so nothing can
        // still read the sender pointer freed below; only this thread ever
        // touched either.
        unsafe { DestroyWindow(self.hwnd) };
        // SAFETY: `tx` came from `Box::into_raw` in `build`; the sole
        // borrower is gone.
        unsafe { drop(Box::from_raw(self.tx)) };
    }
}

/// Reads the sender back out of the message window's user data. Set once
/// by [`build`] for the window's whole life; the owning `Tray` outlives
/// the window, and only the GUI thread that owns both ever reads it.
fn sender_for(hwnd: HWND) -> Option<&'static Sender<TrayAction>> {
    // SAFETY: user data holds a `*const Sender` installed at window
    // creation; the pointer stays valid until `Drop` destroys the window.
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Sender<TrayAction>;
        ptr.as_ref()
    }
}

fn send_action(hwnd: HWND, action: TrayAction) {
    if let Some(tx) = sender_for(hwnd) {
        let _ = tx.send(action);
    }
}

struct TrayIconState {
    hwnd: HWND,
    icon: HICON,
}

// SAFETY: the handles are only ever touched on the GUI thread that owns the
// tray; the mutex never moves them across threads for use.
unsafe impl Send for TrayIconState {}

/// Live icon for re-adding after Explorer restarts.
static ICON_STATE: OnceLock<Mutex<Option<TrayIconState>>> = OnceLock::new();

fn icon_state() -> &'static Mutex<Option<TrayIconState>> {
    ICON_STATE.get_or_init(|| Mutex::new(None))
}

/// `TaskbarCreated` broadcast id; the icon is re-added when it arrives.
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

fn taskbar_created() -> u32 {
    *TASKBAR_CREATED.get_or_init(|| {
        let name = wide("TaskbarCreated");
        // SAFETY: registering a well-formed message name; the pointer is
        // only read for the duration of the call.
        unsafe { RegisterWindowMessageW(name.as_ptr()) }
    })
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn write_tip(tip: &mut [u16; 128], text: &str) {
    let encoded: Vec<u16> = text.encode_utf16().collect();
    let len = encoded.len().min(tip.len() - 1);
    tip[..len].copy_from_slice(&encoded[..len]);
}

/// Validates the `.ico` directory so offsets read below stay in bounds.
fn valid_ico(data: &[u8]) -> bool {
    const ICONDIR_SIZE: usize = 6;
    const ICONDIRENTRY_SIZE: usize = 16;
    if data.len() < ICONDIR_SIZE || data[0..2] != [0, 0] || data[2..4] != [1, 0] {
        return false;
    }
    let count = u16::from_le_bytes([data[4], data[5]]) as usize;
    let directory = ICONDIR_SIZE + count.saturating_mul(ICONDIRENTRY_SIZE);
    if count == 0 || directory > data.len() {
        return false;
    }
    (0..count).all(|index| {
        let start = ICONDIR_SIZE + index * ICONDIRENTRY_SIZE;
        let entry = &data[start..start + ICONDIRENTRY_SIZE];
        let size = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
        let offset = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
        size != 0
            && offset >= directory
            && offset
                .checked_add(size)
                .is_some_and(|end| end <= data.len())
    })
}

/// Picks the shell-preferred image out of the bundled `.ico`.
pub fn load_icon() -> anyhow::Result<HICON> {
    if !valid_ico(ICON_ICO) {
        anyhow::bail!("bundled icon.ico failed validation");
    }
    // SAFETY: `ICON_ICO` passed validation as a well-formed `.ico`
    // directory, so the lookup only reads its header.
    let offset =
        unsafe { LookupIconIdFromDirectoryEx(ICON_ICO.as_ptr(), 1, 0, 0, LR_DEFAULTCOLOR) };
    if offset <= 0 {
        anyhow::bail!("bundled icon.ico lookup failed");
    }
    let rest = ICON_ICO
        .get(offset as usize..)
        .context("bundled icon.ico entry out of bounds")?;
    // SAFETY: `rest` starts at the validated image offset and spans to the
    // end of the bundle, which is exactly the resource slice the call reads.
    let icon = unsafe {
        CreateIconFromResourceEx(
            rest.as_ptr(),
            rest.len() as u32,
            1,
            0x0003_0000,
            0,
            0,
            LR_DEFAULTCOLOR,
        )
    };
    if icon.is_null() {
        anyhow::bail!("CreateIconFromResourceEx failed");
    }
    Ok(icon)
}

fn base_nid(hwnd: HWND, icon: HICON) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    write_tip(&mut nid.szTip, TOOLTIP);
    nid
}

/// Adds the icon and opts into version 4 event behavior.
fn add_icon(hwnd: HWND, icon: HICON) -> anyhow::Result<()> {
    let nid = base_nid(hwnd, icon);
    // SAFETY: `nid` is fully populated with our live window, icon, tooltip,
    // and callback message; the shell only reads it for this call.
    let added = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
    if added == 0 {
        anyhow::bail!("Shell_NotifyIconW NIM_ADD failed");
    }
    let mut versioned = nid;
    versioned.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    // SAFETY: same populated struct; the call only reads the id and the
    // version member.
    unsafe { Shell_NotifyIconW(NIM_SETVERSION, &versioned) };
    Ok(())
}

#[allow(clippy::significant_drop_tightening)]
fn re_add() {
    let guard = icon_state().lock();
    let Ok(guard) = guard else { return };
    if let Some(state) = guard.as_ref() {
        let _ = add_icon(state.hwnd, state.icon);
    }
}

fn show_menu(hwnd: HWND) {
    // SAFETY: creating an empty popup menu; the handle is only null-checked.
    let menu: HMENU = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return;
    }
    let open = wide("Open");
    let restart = wide("Restart service");
    let quit = wide("Quit");
    // SAFETY: appending a string item to a live menu with a nul-terminated
    // label that outlives the call.
    unsafe { AppendMenuW(menu, MF_STRING, MENU_OPEN as usize, open.as_ptr()) };
    // SAFETY: appending a string item to a live menu with a nul-terminated
    // label that outlives the call.
    unsafe { AppendMenuW(menu, MF_STRING, MENU_RESTART as usize, restart.as_ptr()) };
    // SAFETY: appending a string item to a live menu with a nul-terminated
    // label that outlives the call.
    unsafe { AppendMenuW(menu, MF_STRING, MENU_QUIT as usize, quit.as_ptr()) };
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: reading the cursor position into a struct we own.
    unsafe { GetCursorPos(&mut point) };
    // SAFETY: our own message window; foreground status lets the menu take
    // input and dismiss.
    unsafe { SetForegroundWindow(hwnd) };
    // SAFETY: modal menu tracking on our window with no clip rect; returns
    // the picked command id.
    let picked = unsafe {
        TrackPopupMenuEx(
            menu,
            TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RETURNCMD,
            point.x,
            point.y,
            hwnd,
            core::ptr::null(),
        )
    };
    // SAFETY: destroying the menu created above, exactly once.
    unsafe { DestroyMenu(menu) };
    if picked == MENU_OPEN {
        send_action(hwnd, TrayAction::Show);
    } else if picked == MENU_RESTART {
        send_action(hwnd, TrayAction::Restart);
    } else if picked == MENU_QUIT {
        send_action(hwnd, TrayAction::Quit);
    }
}

static CLASS_READY: AtomicBool = AtomicBool::new(false);

/// Registers the message-window class once and creates the hidden window
/// the shell posts tray input to. Stashes `tx` in the window's user data
/// so the window procedure can forward clicks into it.
fn ensure_window(tx: &Sender<TrayAction>) -> anyhow::Result<HWND> {
    // SAFETY: null name queries the current module handle.
    let instance = unsafe { GetModuleHandleW(core::ptr::null()) };
    if instance.is_null() {
        anyhow::bail!("GetModuleHandleW failed");
    }
    if !CLASS_READY.load(Ordering::Acquire) {
        let class = wide("SiphonTray");
        // SAFETY: zero is valid for every class field left unset; the
        // procedure and name below are filled in before registering.
        let mut wc: WNDCLASSW = unsafe { core::mem::zeroed() };
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = instance;
        wc.lpszClassName = class.as_ptr();
        // SAFETY: registering the class described above; the name is copied
        // by the call, so the local stays valid.
        let atom = unsafe { RegisterClassW(&wc) };
        if atom == 0 {
            // SAFETY: reading the calling thread's last error after a failed
            // registration.
            let error = unsafe { GetLastError() };
            if error != ERROR_CLASS_ALREADY_EXISTS {
                anyhow::bail!("RegisterClassW failed ({error})");
            }
        }
        CLASS_READY.store(true, Ordering::Release);
    }
    let class = wide("SiphonTray");
    // SAFETY: class "SiphonTray" is registered above; a message-only window
    // needs no styles, geometry, or menu.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            core::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            core::ptr::null_mut(),
            instance,
            core::ptr::null(),
        )
    };
    if hwnd.is_null() {
        anyhow::bail!("CreateWindowExW failed");
    }
    // SAFETY: `hwnd` is our own live window; user data is a pointer-sized
    // slot the procedure reads back in `sender_for`, and the `Tray` owner
    // outlives the window.
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, core::ptr::from_ref(tx) as isize) };
    Ok(hwnd)
}

/// Routes shell callbacks to tray actions; Explorer restarts re-add the
/// icon. Runs on the GUI thread that pumps the message window.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_TRAY {
        // Version 4 carries the icon id in the high word of `lparam`,
        // so only the low word names the event.
        match (lparam as u32) & 0xFFFF {
            NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                send_action(hwnd, TrayAction::Show);
            }
            WM_RBUTTONUP | WM_RBUTTONDBLCLK | WM_CONTEXTMENU => show_menu(hwnd),
            _ => {}
        }
        return 0;
    }
    if msg == taskbar_created() {
        re_add();
        return 0;
    }
    // SAFETY: default handling for every message not processed above.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::ICON_ICO;
    use super::valid_ico;

    #[test]
    fn rejects_truncated_directory() {
        assert!(!valid_ico(&ICON_ICO[..10]));
    }
}
