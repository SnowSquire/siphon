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

/// Bundled `.ico`; the directory inside it names every image and its size.
static ICON_ICO: &[u8] = include_bytes!("../icons/icon.ico");

/// Header of an `.ico`: reserved, type, image count.
const ICONDIR_SIZE: usize = 6;
/// One `.ico` directory entry. It is two bytes longer than the entry of a
/// *resource group* directory, which names images by resource id instead.
const ICONDIRENTRY_SIZE: usize = 16;
/// The size the notification area asks for on a normal display; the shell
/// scales it down for the tray.
const TRAY_ICON: i32 = 32;

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

/// The bundled `.ico` image closest to `cx` by `cy`, as `(offset, size)`.
///
/// The file's own directory answers this: each entry carries the image's width
/// and height as one byte (zero meaning 256), its length, and its offset into
/// the file. `LookupIconIdFromDirectoryEx` is not the answer even though it is
/// the call the shell offers: it parses a resource *group* directory, whose
/// entries are two bytes shorter and hold a resource id where a file entry
/// holds an offset, so what it returns is an id and not a position in these
/// bytes.
fn best_image(data: &[u8], cx: u32, cy: u32) -> Option<(usize, usize)> {
    if data.get(0..2)? != [0, 0] || data.get(2..4)? != [1, 0] {
        return None;
    }
    let count = usize::from(u16::from_le_bytes([*data.get(4)?, *data.get(5)?]));
    let directory = ICONDIR_SIZE.checked_add(count.checked_mul(ICONDIRENTRY_SIZE)?)?;
    if count == 0 || directory > data.len() {
        return None;
    }
    let mut best: Option<(u64, bool, usize, usize)> = None;
    for index in 0..count {
        let start = ICONDIR_SIZE + index * ICONDIRENTRY_SIZE;
        let entry = data.get(start..start + ICONDIRENTRY_SIZE)?;
        // A zero dimension is how the format writes 256.
        let extent = |value: u8| if value == 0 { 256 } else { u32::from(value) };
        let width = extent(entry[0]);
        let height = extent(entry[1]);
        let size = u32::from_le_bytes(entry[8..12].try_into().ok()?) as usize;
        let offset = u32::from_le_bytes(entry[12..16].try_into().ok()?) as usize;
        if size == 0
            || offset < directory
            || offset.checked_add(size).is_none_or(|end| end > data.len())
        {
            return None;
        }
        let upscale = width < cx || height < cy;
        let distance = u64::from(width.abs_diff(cx)) + u64::from(height.abs_diff(cy));
        let better = best.is_none_or(|(nearest, upscaled, ..)| {
            // Closest first; at the same distance the larger image, so the slot
            // is filled by scaling a picture down, never up.
            distance < nearest || (distance == nearest && upscaled && !upscale)
        });
        if better {
            best = Some((distance, upscale, offset, size));
        }
    }
    best.map(|(_, _, offset, size)| (offset, size))
}

/// Loads the bundled `.ico` at the size the notification area shows.
pub fn load_icon() -> eros::Result<HICON> {
    let icon = icon_at(TRAY_ICON, TRAY_ICON);
    eros::ensure!(!icon.is_null(), "bundled icon.ico could not be loaded");
    Ok(icon)
}

/// Sets the window's title-bar and task-bar icon from the bundled `.ico`.
///
/// The `.ico` rather than the `.png`: a window takes an `HICON`, and the icon
/// file already carries the images Windows asks for at each size, while the PNG
/// would need a decoder to become the straight rows `Surface::set_icon` takes.
/// This covers the running window; the executable carries the same icon as a
/// resource (`build.rs`), which is what Explorer and a pinned button read.
///
/// Renderer setup calls this again on every swapchain rebuild, so the check for
/// an icon already set is what keeps a resize from leaking another pair of
/// handles. An icon given to a window this way lives as long as the window,
/// which is why neither is destroyed here.
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

/// Creates an `HICON` from the bundled `.ico`, at the size closest to `cx` by
/// `cy`.
///
/// The image is created at its own size (`0, 0`): the directory already picked
/// the one that matches, and the call's own scaling then has nothing to do.
fn icon_at(cx: i32, cy: i32) -> HICON {
    let request = |value: i32| u32::try_from(value).unwrap_or(0);
    let Some((offset, size)) = best_image(ICON_ICO, request(cx), request(cy)) else {
        return std::ptr::null_mut();
    };
    let Some(bytes) = ICON_ICO.get(offset..offset + size) else {
        return std::ptr::null_mut();
    };
    // The call wants the bits DWORD-aligned, and an `.ico` image begins wherever
    // its directory says: a heap buffer is aligned where the embedded bytes are
    // not. The copy happens once per icon, which is setup.
    let mut bits = Vec::with_capacity(size);
    bits.extend_from_slice(bytes);
    // SAFETY: `bits` is exactly one image the validated directory names, which
    // is the resource slice the call reads, and the call copies whatever it
    // needs before it returns.
    unsafe {
        CreateIconFromResourceEx(
            bits.as_ptr(),
            u32::try_from(bits.len()).unwrap_or(0),
            1,
            0x0003_0000,
            0,
            0,
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
    use super::best_image;

    #[test]
    fn rejects_truncated_directory() {
        assert!(best_image(&ICON_ICO[..10], 32, 32).is_none());
    }

    /// The chosen bounds are one whole image, and it is the image that calls
    /// itself that size. The bundle stores PNG images; an icon that is
    /// regenerated with uncompressed entries stacks the colour rows over the AND
    /// mask, so the DIB's height is twice the icon's.
    #[test]
    fn picks_the_image_nearest_the_requested_size() {
        for request in [16, 24, 32, 48, 64, 256] {
            let (offset, size) = best_image(ICON_ICO, request, request).expect("an image");
            assert_eq!(
                dimensions(&ICON_ICO[offset..offset + size]),
                (request, request)
            );
        }
    }

    fn dimensions(image: &[u8]) -> (u32, u32) {
        if image.starts_with(b"\x89PNG\r\n\x1a\n") {
            let width = u32::from_be_bytes(image[16..20].try_into().expect("width"));
            let height = u32::from_be_bytes(image[20..24].try_into().expect("height"));
            return (width, height);
        }
        let width = u32::from_le_bytes(image[4..8].try_into().expect("width"));
        let height = u32::from_le_bytes(image[8..12].try_into().expect("height"));
        (width, height / 2)
    }
}
