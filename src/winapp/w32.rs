//! Win32 helpers: window handles, text, styles and messages.
//!
//! Window handles are carried as `isize` ([`Hwnd`]) so they can move between threads, and only
//! turned into HWND at call time. **Synchronous** messages to other processes' windows always
//! have a timeout (SendMessageTimeoutW + SMTO_ABORTIFHUNG), so GhostDesk doesn't hang along with
//! a busy or hung app.

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND, POINT, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging as wm;

/// Window handle as an integer that can move between threads.
pub type Hwnd = isize;

#[inline]
pub fn hw(h: Hwnd) -> HWND {
    h as HWND
}

// ---- constants (defined here so they don't depend on windows-sys type aliases) ----

pub const WM_CLOSE: u32 = 0x0010;
pub const WM_SETTEXT: u32 = 0x000C;
pub const WM_GETTEXT: u32 = 0x000D;
pub const WM_GETTEXTLENGTH: u32 = 0x000E;
pub const WM_NULL: u32 = 0x0000;
pub const WM_NEXTDLGCTL: u32 = 0x0028;
pub const WM_KEYDOWN: u32 = 0x0100;
pub const WM_KEYUP: u32 = 0x0101;
pub const WM_CHAR: u32 = 0x0102;
pub const WM_SYSKEYDOWN: u32 = 0x0104;
pub const WM_SYSKEYUP: u32 = 0x0105;
pub const WM_COMMAND: u32 = 0x0111;
pub const WM_SYSCOMMAND: u32 = 0x0112;
pub const WM_INITMENU: u32 = 0x0116;
pub const WM_INITMENUPOPUP: u32 = 0x0117;
pub const WM_MOUSEMOVE: u32 = 0x0200;
pub const WM_LBUTTONDOWN: u32 = 0x0201;
pub const WM_LBUTTONUP: u32 = 0x0202;
pub const WM_LBUTTONDBLCLK: u32 = 0x0203;
pub const WM_MOUSEWHEEL: u32 = 0x020A;
pub const WM_MOUSEHWHEEL: u32 = 0x020E;
pub const WM_HSCROLL: u32 = 0x0114;
pub const WM_VSCROLL: u32 = 0x0115;
pub const WS_VSCROLL: u32 = 0x0020_0000;
pub const WS_HSCROLL: u32 = 0x0010_0000;

pub const EM_GETSEL: u32 = 0x00B0;
pub const EM_SETSEL: u32 = 0x00B1;
pub const EM_REPLACESEL: u32 = 0x00C2;
pub const EM_UNDO: u32 = 0x00C7;
pub const EM_REDO: u32 = 0x0400 + 84;
pub const CB_GETCOUNT: u32 = 0x0146;
pub const CB_GETCURSEL: u32 = 0x0147;
pub const CB_GETLBTEXT: u32 = 0x0148;
pub const CB_GETLBTEXTLEN: u32 = 0x0149;
pub const CB_SETCURSEL: u32 = 0x014E;
pub const CB_FINDSTRINGEXACT: u32 = 0x0158;
pub const CB_GETEXTENDEDUI: u32 = 0x0156;
pub const CB_SETEXTENDEDUI: u32 = 0x0155;
/// CB_ERR (-1) as the usize send_timeout returns.
const CB_ERR_USIZE: usize = usize::MAX;
pub const CBN_SELCHANGE: u32 = 1;
pub const BM_GETCHECK: u32 = 0x00F0;

pub const MK_LBUTTON: usize = 0x0001;
pub const MK_SHIFT: usize = 0x0004;
pub const MK_CONTROL: usize = 0x0008;

pub const SC_CLOSE: usize = 0xF060;

pub const WS_CHILD: u32 = 0x4000_0000;
pub const WS_VISIBLE: u32 = 0x1000_0000;
pub const WS_DISABLED: u32 = 0x0800_0000;
pub const WS_MINIMIZE: u32 = 0x2000_0000;
pub const WS_CAPTION: u32 = 0x00C0_0000;
pub const WS_POPUP: u32 = 0x8000_0000;
pub const WS_THICKFRAME: u32 = 0x0004_0000;
pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
pub const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
pub const WS_EX_APPWINDOW: u32 = 0x0004_0000;
pub const WS_EX_TOPMOST: u32 = 0x0000_0008;
pub const CS_DBLCLKS: usize = 0x0008;

pub const SWP_NOSIZE: u32 = 0x0001;
pub const SWP_NOMOVE: u32 = 0x0002;
pub const SWP_NOZORDER: u32 = 0x0004;
pub const SWP_NOACTIVATE: u32 = 0x0010;
pub const SWP_FRAMECHANGED: u32 = 0x0020;
pub const SWP_NOOWNERZORDER: u32 = 0x0200;
pub const SWP_ASYNCWINDOWPOS: u32 = 0x4000;

pub const SW_HIDE: i32 = 0;
pub const SW_SHOWNORMAL: u32 = 1;
pub const SW_SHOWMINIMIZED: u32 = 2;
pub const SW_SHOWMAXIMIZED: u32 = 3;
pub const SW_SHOWNOACTIVATE: i32 = 4;
pub const SW_MINIMIZE: u32 = 6;
pub const SW_SHOWMINNOACTIVE: u32 = 7;
pub const WPF_RESTORETOMAXIMIZED: u32 = 0x0002;
pub const WPF_ASYNCWINDOWPLACEMENT: u32 = 0x0004;

const GWL_STYLE: i32 = -16;
const GWL_EXSTYLE: i32 = -20;
const GWLP_ID: i32 = -12;
const GCL_STYLE: i32 = -26;
const GA_PARENT: u32 = 1;
const GA_ROOT: u32 = 2;
const GW_OWNER: u32 = 4;
const SMTO_ABORTIFHUNG: u32 = 0x0002;
const SMTO_BLOCK: u32 = 0x0001;

// ---- functions missing from windows-sys's feature list (declared here to avoid touching Cargo.toml) ----

#[link(name = "user32", kind = "raw-dylib")]
extern "system" {
    fn PrintWindow(hwnd: HWND, hdc: *mut core::ffi::c_void, flags: u32) -> i32;
    fn MapVirtualKeyW(code: u32, map_type: u32) -> u32;
    fn SetWinEventHook(
        event_min: u32,
        event_max: u32,
        hmod: *mut core::ffi::c_void,
        proc_: Option<WinEventProc>,
        id_process: u32,
        id_thread: u32,
        flags: u32,
    ) -> isize;
    fn UnhookWinEvent(hook: isize) -> i32;
    fn GetClipboardSequenceNumber() -> u32;
    fn GetClipboardOwner() -> HWND;
    fn OpenClipboard(owner: HWND) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn EnumClipboardFormats(format: u32) -> u32;
    fn GetClipboardData(format: u32) -> HANDLE;
    fn SetClipboardData(format: u32, mem: HANDLE) -> HANDLE;
    fn IsWindowEnabled(hwnd: HWND) -> i32;
}

#[repr(C)]
struct LastInputInfo {
    cb_size: u32,
    dw_time: u32,
}

#[link(name = "user32", kind = "raw-dylib")]
extern "system" {
    fn GetLastInputInfo(plii: *mut LastInputInfo) -> i32;
}

#[link(name = "kernel32", kind = "raw-dylib")]
extern "system" {
    fn GetTickCount() -> u32;
    fn GlobalAlloc(flags: u32, bytes: usize) -> HANDLE;
    fn GlobalLock(mem: HANDLE) -> *mut core::ffi::c_void;
    fn GlobalUnlock(mem: HANDLE) -> i32;
    fn GlobalSize(mem: HANDLE) -> usize;
    fn GlobalFree(mem: HANDLE) -> HANDLE;
}

/// Milliseconds since the user's last real keyboard or mouse input (posted messages don't count).
pub fn ms_since_user_input() -> u32 {
    let mut l = LastInputInfo { cb_size: std::mem::size_of::<LastInputInfo>() as u32, dw_time: 0 };
    unsafe {
        if GetLastInputInfo(&mut l) == 0 {
            return u32::MAX;
        }
        GetTickCount().wrapping_sub(l.dw_time)
    }
}

pub type WinEventProc = unsafe extern "system" fn(hook: isize, event: u32, hwnd: HWND, id_object: i32, id_child: i32, thread: u32, time: u32);

pub fn print_window(h: Hwnd, hdc: *mut core::ffi::c_void, flags: u32) -> bool {
    unsafe { PrintWindow(hw(h), hdc, flags) != 0 }
}

/// Virtual key → scan code (MAPVK_VK_TO_VSC).
pub fn scan_code(vk: u32) -> u32 {
    unsafe { MapVirtualKeyW(vk, 0) }
}

pub fn set_win_event_hook(min: u32, max: u32, cb: WinEventProc, pid: u32, flags: u32) -> isize {
    unsafe { SetWinEventHook(min, max, std::ptr::null_mut(), Some(cb), pid, 0, flags) }
}

pub fn unhook_win_event(h: isize) {
    if h != 0 {
        unsafe { UnhookWinEvent(h) };
    }
}

/// System clipboard sequence number (content is never read).
pub fn clipboard_sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

/// Window that last wrote to the clipboard.
pub fn clipboard_owner() -> Hwnd {
    unsafe { GetClipboardOwner() as Hwnd }
}

/// A copy of the clipboard kept in memory only, so it can be put back when a hidden app overwrites or empties it.
/// Never logged, saved or sent anywhere; dropped with the app's screen.
pub struct ClipSnapshot {
    formats: Vec<(u32, Vec<u8>)>,
}

/// Formats held as GDI or private handles rather than global memory: they can't be copied byte for byte. The ones
/// that matter have a memory twin Windows converts back from (CF_DIB for CF_BITMAP).
fn handle_format(f: u32) -> bool {
    matches!(f, 2 | 3 | 9 | 14 | 0x80 | 0x82 | 0x83 | 0x8E) || (0x200..=0x3FF).contains(&f)
}

/// Above this the snapshot isn't kept (a huge image); a write by the app is then only reported.
const CLIP_SNAPSHOT_MAX: usize = 16 << 20;

fn open_clipboard(owner: Hwnd) -> bool {
    // another program may hold it for a moment
    for _ in 0..10 {
        if unsafe { OpenClipboard(hw(owner)) } != 0 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    false
}

/// Copies every format on the clipboard. `None`: busy or too large (a restore is then not attempted).
pub fn clipboard_snapshot() -> Option<ClipSnapshot> {
    if !open_clipboard(0) {
        return None;
    }
    let mut formats = vec![];
    let mut total = 0usize;
    let mut ok = true;
    let mut f = 0;
    unsafe {
        loop {
            f = EnumClipboardFormats(f);
            if f == 0 {
                break;
            }
            if handle_format(f) {
                continue;
            }
            let h = GetClipboardData(f);
            if h.is_null() {
                continue;
            }
            let size = GlobalSize(h);
            total += size;
            if total > CLIP_SNAPSHOT_MAX {
                ok = false;
                break;
            }
            let p = GlobalLock(h) as *const u8;
            if p.is_null() {
                continue;
            }
            formats.push((f, std::slice::from_raw_parts(p, size).to_vec()));
            GlobalUnlock(h);
        }
        CloseClipboard();
    }
    ok.then_some(ClipSnapshot { formats })
}

/// Puts a snapshot back. `owner` must be a window of this process (with no owner Windows refuses the data).
pub fn clipboard_restore(snap: &ClipSnapshot, owner: Hwnd) -> bool {
    if owner == 0 || !open_clipboard(owner) {
        return false;
    }
    let mut all = true;
    unsafe {
        EmptyClipboard();
        for (f, bytes) in &snap.formats {
            const GMEM_MOVEABLE: u32 = 0x0002;
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
            if h.is_null() {
                all = false;
                continue;
            }
            let p = GlobalLock(h) as *mut u8;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
                GlobalUnlock(h);
            }
            // on success the clipboard owns the memory
            if SetClipboardData(*f, h).is_null() {
                GlobalFree(h);
                all = false;
            }
        }
        CloseClipboard();
    }
    all
}

/// A hidden message-only window owned by the calling thread (the owner of restored clipboard data).
pub fn message_window() -> Hwnd {
    let class: Vec<u16> = "STATIC".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        wm::CreateWindowExW(0, class.as_ptr(), std::ptr::null(), 0, 0, 0, 0, 0, -3isize as HWND, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null()) as Hwnd
    }
}

pub fn destroy_window(h: Hwnd) {
    if h != 0 {
        unsafe { wm::DestroyWindow(hw(h)) };
    }
}

// ---- kernel handles ----

/// An owned kernel handle, closed on drop.
pub struct Handle(pub HANDLE);
// kernel handles are safe to move between threads
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

// ───────────────────────── DPI ─────────────────────────

/// Temporarily switches this thread to the target window's DPI awareness (restored on drop).
///
/// DWM stretches DPI-unaware apps (most classic Win32 apps) at 125% or 150%. PrintWindow draws
/// such windows at their **logical size**, and the mouse coordinates the app receives are
/// logical too. Doing capture, child window lookup and coordinate conversion in the window's DPI
/// context therefore keeps sizes and coordinates in the app's own coordinate system (nothing
/// changes for DPI-aware apps).
pub struct DpiScope(Option<isize>);

impl DpiScope {
    pub fn for_window(h: Hwnd) -> DpiScope {
        use windows_sys::Win32::UI::HiDpi::{GetWindowDpiAwarenessContext, SetThreadDpiAwarenessContext};
        unsafe {
            let ctx = GetWindowDpiAwarenessContext(hw(h));
            if ctx.is_null() {
                return DpiScope(None);
            }
            let old = SetThreadDpiAwarenessContext(ctx);
            DpiScope((!old.is_null()).then_some(old as isize))
        }
    }
}

impl Drop for DpiScope {
    fn drop(&mut self) {
        if let Some(old) = self.0 {
            unsafe { windows_sys::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(old as _) };
        }
    }
}

/// A window's DPI awareness: 0 = unaware (stretched), 1 = system, 2 = per-monitor.
pub fn dpi_awareness(h: Hwnd) -> i32 {
    use windows_sys::Win32::UI::HiDpi::{GetAwarenessFromDpiAwarenessContext, GetWindowDpiAwarenessContext};
    unsafe { GetAwarenessFromDpiAwarenessContext(GetWindowDpiAwarenessContext(hw(h))) }
}

// ---- window info ----

pub fn is_window(h: Hwnd) -> bool {
    h != 0 && unsafe { wm::IsWindow(hw(h)) != 0 }
}

pub fn is_visible(h: Hwnd) -> bool {
    unsafe { wm::IsWindowVisible(hw(h)) != 0 }
}

pub fn is_enabled(h: Hwnd) -> bool {
    unsafe { IsWindowEnabled(hw(h)) != 0 }
}

pub fn is_iconic(h: Hwnd) -> bool {
    unsafe { wm::IsIconic(hw(h)) != 0 }
}

pub fn is_zoomed(h: Hwnd) -> bool {
    unsafe { wm::IsZoomed(hw(h)) != 0 }
}

pub fn is_hung(h: Hwnd) -> bool {
    unsafe { wm::IsHungAppWindow(hw(h)) != 0 }
}

pub fn class_name(h: Hwnd) -> String {
    let mut b = [0u16; 256];
    let n = unsafe { wm::GetClassNameW(hw(h), b.as_mut_ptr(), b.len() as i32) };
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}

/// Window title (for other processes' windows, reads the text the system holds instead of
/// sending a message).
pub fn window_text(h: Hwnd) -> String {
    let mut b = vec![0u16; 512];
    let n = unsafe { wm::GetWindowTextW(hw(h), b.as_mut_ptr(), b.len() as i32) };
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}

pub fn style(h: Hwnd) -> u32 {
    unsafe { wm::GetWindowLongPtrW(hw(h), GWL_STYLE) as u32 }
}

pub fn ex_style(h: Hwnd) -> u32 {
    unsafe { wm::GetWindowLongPtrW(hw(h), GWL_EXSTYLE) as u32 }
}

/// Changes the extended style. **Blocks until the owning thread handles messages if the window
/// belongs to another thread**, so only call it from a worker thread.
pub fn set_ex_style_blocking(h: Hwnd, ex: u32) {
    unsafe {
        wm::SetWindowLongPtrW(hw(h), GWL_EXSTYLE, ex as i32 as isize);
    }
}

pub fn ctrl_id(h: Hwnd) -> i32 {
    unsafe { wm::GetWindowLongPtrW(hw(h), GWLP_ID) as i32 }
}

pub fn class_style(h: Hwnd) -> usize {
    unsafe { wm::GetClassLongPtrW(hw(h), GCL_STYLE) }
}

pub fn owner(h: Hwnd) -> Hwnd {
    unsafe { wm::GetWindow(hw(h), GW_OWNER) as Hwnd }
}

pub fn parent(h: Hwnd) -> Hwnd {
    unsafe { wm::GetAncestor(hw(h), GA_PARENT) as Hwnd }
}

pub fn root(h: Hwnd) -> Hwnd {
    unsafe { wm::GetAncestor(hw(h), GA_ROOT) as Hwnd }
}

/// Whether this is a top-level window (direct child of the desktop, not message-only).
pub fn is_toplevel(h: Hwnd) -> bool {
    unsafe { style(h) & WS_CHILD == 0 && parent(h) == wm::GetDesktopWindow() as Hwnd }
}

pub fn window_rect(h: Hwnd) -> (i32, i32, i32, i32) {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { wm::GetWindowRect(hw(h), &mut r) };
    (r.left, r.top, r.right, r.bottom)
}

/// Client area size `(w, h)`.
pub fn client_size(h: Hwnd) -> (i32, i32) {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { wm::GetClientRect(hw(h), &mut r) };
    (r.right - r.left, r.bottom - r.top)
}

/// Screen coordinates of the client area origin.
pub fn client_origin(h: Hwnd) -> (i32, i32) {
    let mut p = POINT { x: 0, y: 0 };
    unsafe { windows_sys::Win32::Graphics::Gdi::ClientToScreen(hw(h), &mut p) };
    (p.x, p.y)
}

/// Converts client coordinates of one window into another's.
pub fn map_point(from: Hwnd, to: Hwnd, p: (i32, i32)) -> (i32, i32) {
    let mut pt = POINT { x: p.0, y: p.1 };
    unsafe { windows_sys::Win32::Graphics::Gdi::MapWindowPoints(hw(from), hw(to), &mut pt, 1) };
    (pt.x, pt.y)
}

/// `(thread id, process id)`.
pub fn thread_pid(h: Hwnd) -> (u32, u32) {
    let mut pid = 0u32;
    let tid = unsafe { wm::GetWindowThreadProcessId(hw(h), &mut pid) };
    (tid, pid)
}

/// That thread's `(active window, focus window)`. This is **the thread's own** state, not the
/// user's input state.
pub fn thread_focus(tid: u32) -> Option<(Hwnd, Hwnd)> {
    let mut g = wm::GUITHREADINFO { cbSize: std::mem::size_of::<wm::GUITHREADINFO>() as u32, ..Default::default() };
    if unsafe { wm::GetGUIThreadInfo(tid, &mut g) } == 0 {
        return None;
    }
    Some((g.hwndActive as Hwnd, g.hwndFocus as Hwnd))
}

/// Child windows (all depths).
pub fn child_windows(parent: Hwnd) -> Vec<Hwnd> {
    unsafe extern "system" fn cb(h: HWND, l: isize) -> windows_sys::core::BOOL {
        let v = &mut *(l as *mut Vec<Hwnd>);
        v.push(h as Hwnd);
        1
    }
    let mut v: Vec<Hwnd> = Vec::new();
    unsafe { wm::EnumChildWindows(hw(parent), Some(cb), &mut v as *mut Vec<Hwnd> as isize) };
    v
}

/// The immediate (visible) child at a point in the parent's client coordinates. Transparent
/// controls like group boxes are skipped.
pub fn real_child_at(parent: Hwnd, p: (i32, i32)) -> Hwnd {
    unsafe { wm::RealChildWindowFromPoint(hw(parent), POINT { x: p.0, y: p.1 }) as Hwnd }
}

/// Scroll position (`bar`: 0 = horizontal, 1 = vertical, 2 = the scroll bar control itself).
pub fn scroll_pos(h: Hwnd, bar: i32) -> i32 {
    unsafe { wm::GetScrollPos(hw(h), bar) }
}

pub fn menu_of(h: Hwnd) -> isize {
    unsafe { wm::GetMenu(hw(h)) as isize }
}

/// The (first) top-level window of a class, or 0.
pub fn find_window_class(class: &str) -> Hwnd {
    let c: Vec<u16> = class.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { wm::FindWindowW(c.as_ptr(), std::ptr::null()) as Hwnd }
}

/// The shell's desktop window (Progman), or 0.
pub fn shell_window() -> Hwnd {
    unsafe { wm::GetShellWindow() as Hwnd }
}

// ---- messages ----

pub fn post(h: Hwnd, msg: u32, w: usize, l: isize) -> bool {
    unsafe { wm::PostMessageW(hw(h), msg, w, l) != 0 }
}

/// Synchronous message with a timeout. `None` on timeout or failure.
pub fn send_timeout(h: Hwnd, msg: u32, w: usize, l: isize, ms: u32) -> Option<usize> {
    let mut out = 0usize;
    let r = unsafe { wm::SendMessageTimeoutW(hw(h), msg, w, l, SMTO_ABORTIFHUNG | SMTO_BLOCK, ms, &mut out) };
    if r == 0 {
        None
    } else {
        Some(out)
    }
}

/// A control's full text via WM_GETTEXT (the system marshals the buffer across processes).
pub fn control_text(h: Hwnd) -> Option<String> {
    let len = send_timeout(h, WM_GETTEXTLENGTH, 0, 0, 1000)?;
    let cap = len.min(4_000_000) + 2;
    let mut buf = vec![0u16; cap];
    let n = send_timeout(h, WM_GETTEXT, cap, buf.as_mut_ptr() as isize, 2000)?;
    Some(String::from_utf16_lossy(&buf[..n.min(cap)]))
}

/// WM_SETTEXT (the system marshals the string).
pub fn set_control_text(h: Hwnd, s: &str) -> bool {
    let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    send_timeout(h, WM_SETTEXT, 0, w.as_ptr() as isize, 2000).map(|r| r != 0).unwrap_or(false)
}

/// Only for standard messages taking a string pointer in lParam (ones the system marshals across
/// processes).
pub fn send_str(h: Hwnd, msg: u32, w: usize, s: &str, ms: u32) -> Option<usize> {
    let wz: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    send_timeout(h, msg, w, wz.as_ptr() as isize, ms)
}

/// Longest combo box item accepted (UTF-16 units).
const COMBO_ITEM_MAX: usize = 4096;

/// Combo box item texts (CB_GETLBTEXT is marshaled by the system). Indices are passed straight to
/// CB_SETCURSEL, so unreadable items keep their slot as an empty string.
///
/// CB_GETLBTEXT takes no buffer size: for another process's combo box the system writes the
/// item's length **at send time** into our buffer. If the app makes the item longer in between,
/// a buffer sized by asking first could overflow. So a fixed buffer more than twice the accepted
/// length is used, and the length is asked again afterwards; if it changed (or on error, or when
/// too long) the item is dropped.
pub fn combo_items(h: Hwnd) -> Vec<String> {
    let n = send_timeout(h, CB_GETCOUNT, 0, 0, 1000).unwrap_or(0) as isize;
    let mut out = Vec::new();
    let mut buf = vec![0u16; COMBO_ITEM_MAX * 2 + 2];
    for i in 0..n.clamp(0, 500) {
        let i = i as usize;
        let len = send_timeout(h, CB_GETLBTEXTLEN, i, 0, 500);
        let Some(len) = len.filter(|l| *l != CB_ERR_USIZE && *l <= COMBO_ITEM_MAX) else {
            out.push(String::new());
            continue;
        };
        let got = send_timeout(h, CB_GETLBTEXT, i, buf.as_mut_ptr() as isize, 500);
        let again = send_timeout(h, CB_GETLBTEXTLEN, i, 0, 500);
        match got {
            Some(g) if g != CB_ERR_USIZE && g <= COMBO_ITEM_MAX && again == Some(len) => {
                out.push(String::from_utf16_lossy(&buf[..g]));
            }
            // item changed while reading, or an error: drop it
            _ => out.push(String::new()),
        }
    }
    out
}

/// Whether the combo box uses the extended UI (arrow keys open the list). False if unknown.
pub fn combo_extended_ui(h: Hwnd) -> bool {
    send_timeout(h, CB_GETEXTENDEDUI, 0, 0, 500).map(|r| r != 0).unwrap_or(false)
}

/// MAKELPARAM (two signed 16-bit coordinates).
pub fn make_lparam(x: i32, y: i32) -> isize {
    (((y as i16 as u16 as u32) << 16) | (x as i16 as u16 as u32)) as i32 as isize
}

/// MAKEWPARAM
pub fn make_wparam(lo: u32, hi: u32) -> usize {
    ((hi & 0xFFFF) << 16 | (lo & 0xFFFF)) as usize
}

// ---- placement (moving off-screen) ----

#[derive(Clone, Copy, Debug)]
pub struct Placement {
    pub show_cmd: u32,
    pub normal: (i32, i32, i32, i32),
    pub min_pos: (i32, i32),
    pub max_pos: (i32, i32),
    /// WINDOWPLACEMENT.flags (WPF_RESTORETOMAXIMIZED: a window minimized from maximized).
    pub flags: u32,
}

pub fn get_placement(h: Hwnd) -> Option<Placement> {
    let mut p = wm::WINDOWPLACEMENT { length: std::mem::size_of::<wm::WINDOWPLACEMENT>() as u32, ..unsafe { std::mem::zeroed() } };
    if unsafe { wm::GetWindowPlacement(hw(h), &mut p) } == 0 {
        return None;
    }
    let r = p.rcNormalPosition;
    Some(Placement {
        show_cmd: p.showCmd,
        normal: (r.left, r.top, r.right, r.bottom),
        min_pos: (p.ptMinPosition.x, p.ptMinPosition.y),
        max_pos: (p.ptMaxPosition.x, p.ptMaxPosition.y),
        flags: p.flags,
    })
}

/// Flags for SetWindowPlacement: async plus "restore to maximized" (WPF_RESTORETOMAXIMIZED).
///
/// That flag is only passed with a minimizing command; with a normal or show command the window
/// may maximize (on the user's monitor).
pub fn placement_flags(pl: &Placement) -> u32 {
    let minimizing = matches!(pl.show_cmd, SW_SHOWMINIMIZED | SW_MINIMIZE | SW_SHOWMINNOACTIVE);
    if minimizing {
        (pl.flags & WPF_RESTORETOMAXIMIZED) | WPF_ASYNCWINDOWPLACEMENT
    } else {
        WPF_ASYNCWINDOWPLACEMENT
    }
}

/// Changes the placement asynchronously (doesn't wait on the target thread).
pub fn set_placement_async(h: Hwnd, pl: &Placement) -> bool {
    let p = wm::WINDOWPLACEMENT {
        length: std::mem::size_of::<wm::WINDOWPLACEMENT>() as u32,
        flags: placement_flags(pl),
        showCmd: pl.show_cmd,
        ptMinPosition: POINT { x: pl.min_pos.0, y: pl.min_pos.1 },
        ptMaxPosition: POINT { x: pl.max_pos.0, y: pl.max_pos.1 },
        rcNormalPosition: RECT { left: pl.normal.0, top: pl.normal.1, right: pl.normal.2, bottom: pl.normal.3 },
    };
    unsafe { wm::SetWindowPlacement(hw(h), &p) != 0 }
}

/// A showCmd that keeps the current visibility when restoring a placement (without activating).
pub fn keep_visibility_cmd(h: Hwnd) -> u32 {
    if !is_visible(h) {
        SW_HIDE as u32
    } else if is_iconic(h) {
        SW_SHOWMINNOACTIVE
    } else {
        SW_SHOWNOACTIVATE as u32
    }
}

/// WINDOWPLACEMENT's normal position (rcNormalPosition) → screen coordinates.
///
/// Placement coordinates of non-tool top-level windows are workspace coordinates (relative to
/// the monitor's area minus the taskbar and such).
pub fn normal_to_screen(h: Hwnd, normal: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTOPRIMARY};
    if ex_style(h) & WS_EX_TOOLWINDOW != 0 {
        return normal;
    }
    let r = RECT { left: normal.0, top: normal.1, right: normal.2, bottom: normal.3 };
    unsafe {
        let mon = MonitorFromRect(&r, MONITOR_DEFAULTTOPRIMARY);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if mon.is_null() || GetMonitorInfoW(mon, &mut mi) == 0 {
            return normal;
        }
        let (dx, dy) = (mi.rcWork.left - mi.rcMonitor.left, mi.rcWork.top - mi.rcMonitor.top);
        (normal.0 + dx, normal.1 + dy, normal.2 + dx, normal.3 + dy)
    }
}

/// Whether a normally shown window's actual rectangle differs from its placement's normal
/// position (screen coordinates), i.e. it was arranged by Aero Snap or snap layouts.
///
/// SetWindowPlacement alone would put such a window back to its pre-snap size and position, so
/// the actual rectangle is remembered separately and restored.
pub fn arranged_rect_differs(normal_screen: (i32, i32, i32, i32), actual: (i32, i32, i32, i32)) -> bool {
    let d = |a: i32, b: i32| (a - b).abs() > 2;
    d(normal_screen.0, actual.0) || d(normal_screen.1, actual.1) || d(normal_screen.2, actual.2) || d(normal_screen.3, actual.3)
}

/// Moves a window to the screen rectangle `(l, t, r, b)` without activating, changing z-order
/// or waiting on the target thread.
pub fn set_rect_async(h: Hwnd, r: (i32, i32, i32, i32)) -> bool {
    unsafe {
        wm::SetWindowPos(
            hw(h),
            std::ptr::null_mut(),
            r.0,
            r.1,
            r.2 - r.0,
            r.3 - r.1,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS,
        ) != 0
    }
}

/// Moves a window to `(x, y)` without activating, changing z-order or waiting on the target
/// thread.
pub fn move_async(h: Hwnd, x: i32, y: i32) -> bool {
    unsafe {
        wm::SetWindowPos(
            hw(h),
            std::ptr::null_mut(),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS,
        ) != 0
    }
}

/// Applies style changes (recomputes the frame), asynchronously.
pub fn frame_changed_async(h: Hwnd) {
    unsafe {
        wm::SetWindowPos(
            hw(h),
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOSIZE | SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_FRAMECHANGED | SWP_ASYNCWINDOWPOS,
        );
    }
}

pub fn show_async(h: Hwnd, cmd: i32) -> bool {
    unsafe { wm::ShowWindowAsync(hw(h), cmd) != 0 }
}

pub fn foreground() -> Hwnd {
    unsafe { wm::GetForegroundWindow() as Hwnd }
}

/// Foreground switch lock (LSFW_LOCK / LSFW_UNLOCK). Only succeeds with foreground rights.
pub fn lock_foreground(lock: bool) -> bool {
    unsafe { wm::LockSetForegroundWindow(if lock { 1 } else { 2 }) != 0 }
}

/// Hands the foreground (taken by the app) back to the original window. Only call it when our
/// app grabbed the foreground.
pub fn restore_foreground(h: Hwnd) -> bool {
    is_window(h) && unsafe { wm::SetForegroundWindow(hw(h)) != 0 }
}

// ---- text ----

/// Strips mnemonic markers ('&') from menu and control text: "&File" → "File", "&&" → "&". The
/// shortcut part (after '\t') is dropped.
pub fn strip_mnemonic(s: &str) -> String {
    let s = s.split('\t').next().unwrap_or("");
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '&' {
            if it.peek() == Some(&'&') {
                out.push('&');
                it.next();
            }
            continue;
        }
        out.push(c);
    }
    // a leftover marker like "열기(&O)" → "열기(O)" is kept (that's the name the app shows)
    out.trim().to_string()
}

/// The mnemonic character ("&Save" → 's').
pub fn mnemonic_of(s: &str) -> Option<char> {
    let s = s.split('\t').next().unwrap_or("");
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '&' {
            match it.next() {
                Some('&') => continue,
                Some(m) => return Some(m.to_ascii_lowercase()),
                None => return None,
            }
        }
    }
    None
}

pub fn short(s: &str, n: usize) -> String {
    let s = s.replace("\r\n", "⏎").replace(['\n', '\r'], "⏎");
    if s.chars().count() <= n {
        s
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lparam_and_text() {
        assert_eq!(make_lparam(10, 20), (20 << 16 | 10) as isize);
        // negative coordinates as signed 16-bit
        let l = make_lparam(-5, 7) as u32;
        assert_eq!(l & 0xFFFF, 0xFFFB);
        assert_eq!(l >> 16, 7);
        assert_eq!(make_wparam(0x1234, 1), 0x0001_1234);
        assert_eq!(strip_mnemonic("&File"), "File");
        assert_eq!(strip_mnemonic("Save &As...\tCtrl+Shift+S"), "Save As...");
        assert_eq!(strip_mnemonic("Tom && Jerry"), "Tom & Jerry");
        assert_eq!(strip_mnemonic("열기(&O)..."), "열기(O)...");
        assert_eq!(mnemonic_of("열기(&O)"), Some('o'));
        assert_eq!(mnemonic_of("A && B"), None);
        assert_eq!(short("a\r\nb", 10), "a⏎b");
    }

    #[test]
    fn placement_flags_keep_restore_to_maximized() {
        let mut pl = Placement { show_cmd: SW_SHOWMINIMIZED, normal: (0, 0, 10, 10), min_pos: (0, 0), max_pos: (0, 0), flags: 0 };
        assert_eq!(placement_flags(&pl), WPF_ASYNCWINDOWPLACEMENT);
        pl.flags = WPF_RESTORETOMAXIMIZED | 0x1; // WPF_SETMINPOSITION isn't passed
        assert_eq!(placement_flags(&pl), WPF_RESTORETOMAXIMIZED | WPF_ASYNCWINDOWPLACEMENT);
        pl.show_cmd = SW_SHOWMINNOACTIVE;
        assert_eq!(placement_flags(&pl), WPF_RESTORETOMAXIMIZED | WPF_ASYNCWINDOWPLACEMENT);
        // not passed with show/normal commands (maximizing while restoring off-screen would snap to the
        // user's monitor)
        for c in [SW_SHOWNORMAL, SW_SHOWNOACTIVATE as u32, SW_SHOWMAXIMIZED, SW_HIDE as u32] {
            pl.show_cmd = c;
            assert_eq!(placement_flags(&pl), WPF_ASYNCWINDOWPLACEMENT, "{c}");
        }
    }

    #[test]
    fn arranged_rect() {
        // same rectangle (±2 px) = not snapped
        assert!(!arranged_rect_differs((100, 100, 900, 700), (100, 100, 900, 700)));
        assert!(!arranged_rect_differs((100, 100, 900, 700), (101, 99, 902, 700)));
        // snapped to the left half: size and position differ
        assert!(arranged_rect_differs((100, 100, 900, 700), (-7, 0, 967, 1047)));
    }

    #[test]
    fn combo_items_on_missing_window() {
        // a missing window gives an empty list (failed messages don't become items)
        assert!(combo_items(0).is_empty());
        assert!(!combo_extended_ui(0));
    }
}
