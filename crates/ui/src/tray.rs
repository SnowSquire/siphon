//! System tray icon with an Open/Quit menu; left-click shows the window
//! without opening the menu.
//!
//! A raw `Shell_NotifyIconW` icon on a hidden message window: the icon,
//! tooltip, and Open/Quit menu are registered directly with the shell, so
//! tray clicks arrive as window messages on the GUI thread. What a click
//! does is carried out right there rather than queued for a frame, because a
//! window put away in the tray draws none: a click that brought it back
//! would otherwise wait for the window it was meant to bring back.
//! Notifications stay on WinRT toasts in `notifier`, never on the legacy
//! `NIF_INFO` balloon this same struct could show.
//! The `Tray` value must stay alive for the icon to remain: dropping it
//! removes the icon, and `Tray` always owns one — absence of a tray is
//! `Option<Tray>::None` at the call site, never a flag inside `Tray`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicIsize;
use std::sync::atomic::Ordering;

use eros::Context as _;
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
use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics;
use windows_sys::Win32::UI::WindowsAndMessaging::HICON;
use windows_sys::Win32::UI::WindowsAndMessaging::HMENU;
use windows_sys::Win32::UI::WindowsAndMessaging::HWND_MESSAGE;
use windows_sys::Win32::UI::WindowsAndMessaging::ICON_BIG;
use windows_sys::Win32::UI::WindowsAndMessaging::ICON_SMALL;
use windows_sys::Win32::UI::WindowsAndMessaging::LR_DEFAULTCOLOR;
use windows_sys::Win32::UI::WindowsAndMessaging::LookupIconIdFromDirectoryEx;
use windows_sys::Win32::UI::WindowsAndMessaging::MF_STRING;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows_sys::Win32::UI::WindowsAndMessaging::RegisterClassW;
use windows_sys::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW;
use windows_sys::Win32::UI::WindowsAndMessaging::SM_CXICON;
use windows_sys::Win32::UI::WindowsAndMessaging::SM_CXSMICON;
use windows_sys::Win32::UI::WindowsAndMessaging::SM_CYICON;
use windows_sys::Win32::UI::WindowsAndMessaging::SM_CYSMICON;
use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW;
use windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::TPM_BOTTOMALIGN;
use windows_sys::Win32::UI::WindowsAndMessaging::TPM_LEFTALIGN;
use windows_sys::Win32::UI::WindowsAndMessaging::TPM_RETURNCMD;
use windows_sys::Win32::UI::WindowsAndMessaging::TrackPopupMenuEx;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_APP;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_CONTEXTMENU;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_GETICON;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_LBUTTONDBLCLK;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_LBUTTONUP;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_RBUTTONDBLCLK;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_RBUTTONUP;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_SETICON;
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

/// Owns the tray icon and its message window. Held alive for the app
/// lifetime; dropping removes the icon from the tray.
pub struct Tray {
    hwnd: HWND,
    icon: HICON,
}

/// Builds the tray icon. Call on the GUI thread so the message window that
/// receives the shell callbacks pumps with the event loop.
pub fn build() -> eros::Result<Tray> {
    let hwnd = ensure_window()?;
    let icon = load_icon()?;
    add_icon(hwnd, icon)?;
    if let Ok(mut guard) = icon_state().lock() {
        *guard = Some(TrayIconState { hwnd, icon });
    }
    // Posted to from another thread by `show`, which may not touch a window.
    TRAY_HWND.store(hwnd as isize, Ordering::Release);
    Ok(Tray { hwnd, icon })
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
        unsafe { DestroyWindow(self.hwnd) };
    }
}

/// What a tray click does.
type Handler = Rc<dyn Fn(TrayAction)>;

thread_local! {
    /// What a tray click does, installed by the window that owns the UI. A
    /// window put away in the tray runs no frames, so a click has to be
    /// carried out by the GUI thread that receives it, not queued for one.
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
}

/// Installs what a tray click does. Called from the window's component, the
/// first place a window handle exists.
pub fn set_action_handler(handler: impl Fn(TrayAction) + 'static) {
    HANDLER.with(|slot| *slot.borrow_mut() = Some(Rc::new(handler)));
}

/// Runs the installed handler, if there is one. Cloned out first, so a handler
/// that tears anything down cannot pull the closure out from under itself.
fn dispatch(action: TrayAction) {
    let handler = HANDLER.with(|slot| slot.borrow().clone());
    if let Some(handler) = handler {
        handler(action);
    }
}

/// The tray window, for posting to it from another thread. Zero until the
/// tray exists.
static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);

/// Asks the window to show itself from another thread.
///
/// A second launch of the program arrives on a listener thread, and posting a
/// message is the only way such a thread may reach a window. It raises the
/// same message a tray click does, so there is one path to being shown.
pub fn show() {
    let hwnd = TRAY_HWND.load(Ordering::Acquire) as HWND;
    if hwnd.is_null() {
        return;
    }
    // SAFETY: the posted message carries no pointers. The window may already
    // be gone, in which case the post fails and nothing happens.
    unsafe { PostMessageW(hwnd, WM_TRAY, 0, NIN_SELECT as LPARAM) };
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
pub fn load_icon() -> eros::Result<HICON> {
    if !valid_ico(ICON_ICO) {
        eros::bail!("bundled icon.ico failed validation");
    }
    // SAFETY: `ICON_ICO` passed validation as a well-formed `.ico`
    // directory, so the lookup only reads its header.
    let offset =
        unsafe { LookupIconIdFromDirectoryEx(ICON_ICO.as_ptr(), 1, 0, 0, LR_DEFAULTCOLOR) };
    if offset <= 0 {
        eros::bail!("bundled icon.ico lookup failed");
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
        eros::bail!("CreateIconFromResourceEx failed");
    }
    Ok(icon)
}

/// Sets the window's title-bar and taskbar icon from the bundled `.ico`.
///
/// The `.ico` rather than the `.png`: the window manager takes an `HICON`, and
/// the icon file already carries the images Windows asks for at each size,
/// while the `.png` would need a decoder to become the straight rows
/// `Surface::set_icon` takes. Renderer setup calls this again on every
/// swapchain rebuild, so the check for an icon already set is what keeps a
/// resize from leaking another pair of handles. An icon given to a window this
/// way lives as long as the window, which is why neither is destroyed here.
pub fn set_window_icon(hwnd: HWND) {
    // SAFETY: `hwnd` is this process's window, and the message reads no pointers.
    if unsafe { SendMessageW(hwnd, WM_GETICON, ICON_BIG as WPARAM, 0) } != 0 {
        return;
    }
    // SAFETY: plain system metric queries.
    let (big_w, big_h) = unsafe { (GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON)) };
    // SAFETY: plain system metric queries.
    let (small_w, small_h) =
        unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) };
    let big = icon_at(big_w, big_h);
    if big.is_null() {
        return;
    }
    let small = icon_at(small_w, small_h);
    let small = if small.is_null() { big } else { small };
    // SAFETY: both handles were created just above and belong to the window
    // from here on; it is destroyed with the window.
    unsafe {
        SendMessageW(hwnd, WM_SETICON, ICON_BIG as WPARAM, big as LPARAM);
        SendMessageW(hwnd, WM_SETICON, ICON_SMALL as WPARAM, small as LPARAM);
    }
}

/// Creates the bundled `.ico` image closest to `cx` by `cy`.
fn icon_at(cx: i32, cy: i32) -> HICON {
    if !valid_ico(ICON_ICO) {
        return std::ptr::null_mut();
    }
    // SAFETY: `ICON_ICO` passed validation as a well-formed `.ico` directory,
    // so the lookup only reads its header.
    let offset =
        unsafe { LookupIconIdFromDirectoryEx(ICON_ICO.as_ptr(), 1, cx, cy, LR_DEFAULTCOLOR) };
    if offset <= 0 {
        return std::ptr::null_mut();
    }
    let Some(rest) = ICON_ICO.get(offset as usize..) else {
        return std::ptr::null_mut();
    };
    // SAFETY: `rest` starts at the validated image offset and spans to the end
    // of the bundle, which is exactly the resource slice the call reads.
    unsafe {
        CreateIconFromResourceEx(
            rest.as_ptr(),
            rest.len() as u32,
            1,
            0x0003_0000,
            cx,
            cy,
            LR_DEFAULTCOLOR,
        )
    }
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
fn add_icon(hwnd: HWND, icon: HICON) -> eros::Result<()> {
    let nid = base_nid(hwnd, icon);
    // SAFETY: `nid` is fully populated with our live window, icon, tooltip,
    // and callback message; the shell only reads it for this call.
    let added = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
    if added == 0 {
        eros::bail!("Shell_NotifyIconW NIM_ADD failed");
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
        dispatch(TrayAction::Show);
    } else if picked == MENU_RESTART {
        dispatch(TrayAction::Restart);
    } else if picked == MENU_QUIT {
        dispatch(TrayAction::Quit);
    }
}

static CLASS_READY: AtomicBool = AtomicBool::new(false);

/// Registers the message-window class once and creates the hidden window
/// the shell posts tray input to.
fn ensure_window() -> eros::Result<HWND> {
    // SAFETY: null name queries the current module handle.
    let instance = unsafe { GetModuleHandleW(core::ptr::null()) };
    if instance.is_null() {
        eros::bail!("GetModuleHandleW failed");
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
                eros::bail!("RegisterClassW failed ({error})");
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
        eros::bail!("CreateWindowExW failed");
    }
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
                dispatch(TrayAction::Show);
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
