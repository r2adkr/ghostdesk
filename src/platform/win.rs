//! Windows-only helpers (Win32 via windows-sys).
//!
//! User SID, the user's default environment, console and std handle cleanup, registry and
//! version info reads, visible window checks (watching whether hidden-launched apps appear on
//! the user's screen), and so on.

#![allow(clippy::missing_safety_doc)]

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL, HWND, LPARAM, POINT, RECT};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// UTF-16, NUL-terminated.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// NUL-terminated UTF-16 pointer → String.
pub unsafe fn from_wide_ptr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

/// Fixed-size UTF-16 buffer (up to the NUL) → String.
pub fn from_wide_buf(b: &[u16]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf16_lossy(&b[..n])
}

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

unsafe fn token_user_sid_string(token: HANDLE) -> std::io::Result<String> {
    let mut len = 0u32;
    GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
    if len == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // TOKEN_USER contains pointers, so use an 8-byte aligned buffer
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    if GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as *mut _, len, &mut len) == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let tu = &*(buf.as_ptr() as *const TOKEN_USER);
    let mut s: *mut u16 = std::ptr::null_mut();
    if ConvertSidToStringSidW(tu.User.Sid, &mut s) == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let out = from_wide_ptr(s);
    LocalFree(s as HLOCAL);
    Ok(out)
}

/// SID string of the current process's user (e.g. S-1-5-21-…-1001).
pub fn current_user_sid() -> std::io::Result<String> {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(s) = CACHE.get() {
        return Ok(s.clone());
    }
    let sid = unsafe {
        let mut t: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut t) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let t = OwnedHandle(t);
        token_user_sid_string(t.0)?
    };
    let _ = CACHE.set(sid.clone());
    Ok(sid)
}

/// Opens another process's token with TOKEN_QUERY and passes it to `f` (error without access).
fn with_process_token<T>(pid: u32, f: impl FnOnce(HANDLE) -> std::io::Result<T>) -> std::io::Result<T> {
    unsafe {
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if p.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let p = OwnedHandle(p);
        let mut t: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(p.0, TOKEN_QUERY, &mut t) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let t = OwnedHandle(t);
        f(t.0)
    }
}

/// SID string of another process's user (error without access).
pub fn process_user_sid(pid: u32) -> std::io::Result<String> {
    with_process_token(pid, |t| unsafe { token_user_sid_string(t) })
}

/// Integrity level RIDs (SECURITY_MANDATORY_*_RID).
pub const INTEGRITY_LOW: u32 = 0x1000;
pub const INTEGRITY_MEDIUM: u32 = 0x2000;
pub const INTEGRITY_HIGH: u32 = 0x3000;
pub const INTEGRITY_SYSTEM: u32 = 0x4000;

/// Human-readable integrity level name (for error messages).
pub fn integrity_name(rid: u32) -> &'static str {
    match rid {
        r if r < INTEGRITY_LOW => "신뢰 안 함",
        r if r < INTEGRITY_MEDIUM => "낮음",
        r if r < INTEGRITY_HIGH => "보통",
        r if r < INTEGRITY_SYSTEM => "높음(관리자)",
        _ => "시스템",
    }
}

unsafe fn token_integrity_rid(token: HANDLE) -> std::io::Result<u32> {
    use windows_sys::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, TokenIntegrityLevel, TOKEN_MANDATORY_LABEL,
    };
    let mut len = 0u32;
    GetTokenInformation(token, TokenIntegrityLevel, std::ptr::null_mut(), 0, &mut len);
    if len == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // TOKEN_MANDATORY_LABEL contains pointers, so use an 8-byte aligned buffer
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    if GetTokenInformation(token, TokenIntegrityLevel, buf.as_mut_ptr() as *mut _, len, &mut len) == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
    let sid = label.Label.Sid;
    let count = GetSidSubAuthorityCount(sid);
    if sid.is_null() || count.is_null() || *count == 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "무결성 수준 SID 를 읽지 못했습니다"));
    }
    // the RID is the last subauthority of the integrity SID (S-1-16-<RID>)
    Ok(*GetSidSubAuthority(sid, (*count - 1) as u32))
}

/// Integrity level RID of this process.
pub fn current_integrity_rid() -> std::io::Result<u32> {
    unsafe {
        let mut t: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut t) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let t = OwnedHandle(t);
        token_integrity_rid(t.0)
    }
}

/// Integrity level RID of another process (an error without access usually means it runs with
/// higher privileges).
pub fn process_integrity_rid(pid: u32) -> std::io::Result<u32> {
    with_process_token(pid, |t| unsafe { token_integrity_rid(t) })
}

/// This user's default environment (the one from logon, not what the MCP host scrubbed).
///
/// `None` on failure (callers then use the current environment as is).
pub fn user_environment() -> Option<Vec<(OsString, OsString)>> {
    use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows_sys::Win32::Security::TOKEN_DUPLICATE;
    unsafe {
        let mut t: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &mut t) == 0 {
            return None;
        }
        let t = OwnedHandle(t);
        let mut block: *mut core::ffi::c_void = std::ptr::null_mut();
        if CreateEnvironmentBlock(&mut block, t.0, 0) == 0 || block.is_null() {
            return None;
        }
        let mut out = Vec::new();
        let mut p = block as *const u16;
        loop {
            let mut n = 0;
            while *p.add(n) != 0 {
                n += 1;
            }
            if n == 0 {
                break;
            }
            if let Some(kv) = split_env_entry(std::slice::from_raw_parts(p, n)) {
                out.push(kv);
            }
            p = p.add(n + 1);
        }
        DestroyEnvironmentBlock(block);
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }
}

/// One environment block entry ("name=value", UTF-16) → `(name, value)`.
///
/// Split while still UTF-16, so names starting with Hangul and values with unpaired surrogates
/// come through unchanged. Hidden entries starting with '=' like "=C:=C:\..." (per-drive current
/// directories) and entries without '=' give `None`.
fn split_env_entry(w: &[u16]) -> Option<(OsString, OsString)> {
    const EQ: u16 = b'=' as u16;
    if w.first() == Some(&EQ) {
        return None;
    }
    let eq = w.iter().position(|&c| c == EQ)?;
    Some((OsString::from_wide(&w[..eq]), OsString::from_wide(&w[eq + 1..])))
}

/// Makes the std handles non-inheritable.
///
/// Rust std's `Command::spawn` uses bInheritHandles=TRUE on Windows, so the inheritable pipe
/// handles the MCP host passed us would leak into the GhostDesk window → browser → …, and the
/// host would never see EOF after the MCP server exits. std's Stdio::inherit/piped/null make a
/// separate inheritable copy per child, so clearing the flag on the originals is safe. Call once
/// right at startup.
pub fn harden_std_handles() {
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        unsafe {
            let h = GetStdHandle(id);
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// If we run as a GUI but a console window was created just for this process (double-click in
/// Explorer and the like), closes it. A terminal we were started from is left alone.
pub fn detach_own_console() {
    if owns_console_alone() {
        unsafe { windows_sys::Win32::System::Console::FreeConsole() };
    }
}

/// Whether the attached console is used by this process alone (one created for it, e.g. by a
/// double-click in Explorer). False when started from a terminal, which shares its console with
/// the shell.
pub fn owns_console_alone() -> bool {
    use windows_sys::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
    unsafe {
        if GetConsoleWindow().is_null() {
            return false;
        }
        let mut list = [0u32; 4];
        GetConsoleProcessList(list.as_mut_ptr(), list.len() as u32) == 1
    }
}

/// Makes this process per-monitor DPI aware (PerMonitorV2) so window coordinates and captures
/// are in physical pixels.
///
/// The GUI (winit) sets this itself; this is for windowless modes that handle Win32 windows.
pub fn set_dpi_aware() {
    use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

// ---- registry ----

pub use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

/// One string value (REG_SZ / REG_EXPAND_SZ, expanded). `None` for `value` means the default
/// value.
pub fn reg_string(root: HKEY, subkey: &str, value: Option<&str>) -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ};
    let sk = wide(subkey);
    let v = value.map(wide);
    let vp = v.as_ref().map(|v| v.as_ptr()).unwrap_or(std::ptr::null());
    unsafe {
        let mut len = 0u32;
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
        if RegGetValueW(root, sk.as_ptr(), vp, flags, std::ptr::null_mut(), std::ptr::null_mut(), &mut len) != 0 || len == 0 {
            return None;
        }
        let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
        let mut len2 = (buf.len() * 2) as u32;
        if RegGetValueW(root, sk.as_ptr(), vp, flags, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _, &mut len2) != 0 {
            return None;
        }
        let s = from_wide_buf(&buf);
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

/// Names of subkeys.
pub fn reg_subkeys(root: HKEY, subkey: &str) -> Vec<String> {
    use windows_sys::Win32::System::Registry::{RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, KEY_READ};
    let sk = wide(subkey);
    let mut out = Vec::new();
    unsafe {
        let mut h: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(root, sk.as_ptr(), 0, KEY_READ, &mut h) != 0 {
            return out;
        }
        let mut i = 0u32;
        loop {
            let mut name = [0u16; 512];
            let mut n = name.len() as u32;
            let r = RegEnumKeyExW(
                h,
                i,
                name.as_mut_ptr(),
                &mut n,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if r != 0 {
                break;
            }
            out.push(String::from_utf16_lossy(&name[..n as usize]));
            i += 1;
        }
        RegCloseKey(h);
    }
    out
}

/// Executable path registered under "App Paths" (e.g. "chrome.exe" →
/// C:\Program Files\Google\Chrome\Application\chrome.exe).
pub fn app_path(exe_name: &str) -> Option<PathBuf> {
    let sub = format!(r"Software\Microsoft\Windows\CurrentVersion\App Paths\{exe_name}");
    [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
        .into_iter()
        .filter_map(|root| reg_string(root, &sub, None))
        .map(|s| PathBuf::from(s.trim().trim_matches('"')))
        .find(|p| p.is_file())
}

/// Installed programs (Uninstall key): `(display name, install location, display icon path)`.
pub fn installed_programs() -> Vec<(String, Option<PathBuf>, Option<PathBuf>)> {
    let mut out = Vec::new();
    let roots = [
        (HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_LOCAL_MACHINE, r"Software\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_LOCAL_MACHINE, r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"),
    ];
    for (root, base) in roots {
        for k in reg_subkeys(root, base) {
            let sub = format!(r"{base}\{k}");
            let Some(name) = reg_string(root, &sub, Some("DisplayName")) else { continue };
            let loc = reg_string(root, &sub, Some("InstallLocation"))
                .map(|s| PathBuf::from(s.trim().trim_matches('"')))
                .filter(|p| p.is_dir());
            let icon = reg_string(root, &sub, Some("DisplayIcon")).map(|s| {
                // "C:\x\app.exe,0" → path only
                let s = s.trim().trim_matches('"');
                let s = s.rsplit_once(',').filter(|(_, n)| n.trim().parse::<i32>().is_ok()).map(|(p, _)| p).unwrap_or(s);
                PathBuf::from(s.trim().trim_matches('"'))
            });
            out.push((name, loc, icon));
        }
    }
    out
}

// ---- executable version info ----

#[derive(Clone, Debug, Default)]
pub struct VersionInfo {
    pub product_name: Option<String>,
    pub file_description: Option<String>,
    pub original_filename: Option<String>,
    pub company_name: Option<String>,
    pub product_version: Option<String>,
}

/// An executable's VERSIONINFO resource.
pub fn version_info(path: &Path) -> Option<VersionInfo> {
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    let p = wide(path);
    unsafe {
        let mut dummy = 0u32;
        let size = GetFileVersionInfoSizeW(p.as_ptr(), &mut dummy);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(p.as_ptr(), 0, size, data.as_mut_ptr() as *mut _) == 0 {
            return None;
        }
        // use the first language/code page in the list (English Unicode if none)
        let mut tr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut trlen = 0u32;
        let mut langs: Vec<(u16, u16)> = Vec::new();
        if VerQueryValueW(data.as_ptr() as *const _, wide(r"\VarFileInfo\Translation").as_ptr(), &mut tr, &mut trlen) != 0
            && !tr.is_null()
        {
            let n = trlen as usize / 4;
            let pairs = std::slice::from_raw_parts(tr as *const u16, n * 2);
            for i in 0..n {
                langs.push((pairs[i * 2], pairs[i * 2 + 1]));
            }
        }
        langs.push((0x0409, 0x04B0));
        langs.push((0x0409, 0x04E4));
        let get = |name: &str| -> Option<String> {
            for (l, c) in &langs {
                let q = wide(format!(r"\StringFileInfo\{l:04x}{c:04x}\{name}"));
                let mut v: *mut core::ffi::c_void = std::ptr::null_mut();
                let mut vlen = 0u32;
                if VerQueryValueW(data.as_ptr() as *const _, q.as_ptr(), &mut v, &mut vlen) != 0 && !v.is_null() && vlen > 0 {
                    let s = from_wide_ptr(v as *const u16);
                    let s = s.trim().to_string();
                    if !s.is_empty() {
                        return Some(s);
                    }
                }
            }
            None
        };
        Some(VersionInfo {
            product_name: get("ProductName"),
            file_description: get("FileDescription"),
            original_filename: get("OriginalFilename"),
            company_name: get("CompanyName"),
            product_version: get("ProductVersion"),
        })
    }
}

// ---- window checks ----

/// State of one top-level window.
#[derive(Clone, Debug)]
pub struct TopWindow {
    pub hwnd: isize,
    pub pid: u32,
    pub title: String,
    pub class: String,
    /// Screen coordinates (physical pixels, as seen by a DPI-aware process).
    pub rect: (i32, i32, i32, i32),
    pub visible: bool,
    pub minimized: bool,
    /// Cloaked by DWM (on another virtual desktop, suspended UWP, …).
    pub cloaked: bool,
    /// Opacity of a layered window (`None` if not layered or unknown).
    pub alpha: Option<u8>,
}

impl TopWindow {
    /// Whether the user could actually see this window: visible, not minimized, not cloaked, opaque
    /// and overlapping a monitor.
    pub fn shows_on_screen(&self) -> bool {
        let (l, t, r, b) = self.rect;
        self.visible
            && !self.minimized
            && !self.cloaked
            && self.alpha.map_or(true, |a| a > 8)
            && r - l > 1
            && b - t > 1
            && intersects_any_monitor(self.rect)
    }
}

unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> windows_sys::core::BOOL {
    let v = &mut *(lparam as *mut Vec<HWND>);
    v.push(hwnd);
    1
}

/// Every top-level HWND in z-order (topmost first).
pub fn top_level_hwnds() -> Vec<HWND> {
    use windows_sys::Win32::UI::WindowsAndMessaging::EnumWindows;
    let mut v: Vec<HWND> = Vec::new();
    unsafe {
        EnumWindows(Some(enum_cb), &mut v as *mut Vec<HWND> as LPARAM);
    }
    v
}

pub fn window_pid(hwnd: HWND) -> u32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    pid
}

pub fn window_info(hwnd: HWND) -> TopWindow {
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetLayeredWindowAttributes, GetWindowLongPtrW, GetWindowRect, GetWindowTextW, IsIconic,
        IsWindowVisible, GWL_EXSTYLE, LWA_ALPHA, WS_EX_LAYERED,
    };
    unsafe {
        let mut title = [0u16; 256];
        let tn = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
        let mut class = [0u16; 128];
        let cn = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
        let mut r: RECT = std::mem::zeroed();
        GetWindowRect(hwnd, &mut r);
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, &mut cloaked as *mut u32 as *mut _, 4);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let alpha = if ex & WS_EX_LAYERED != 0 {
            let mut a = 255u8;
            let mut flags = 0u32;
            if GetLayeredWindowAttributes(hwnd, std::ptr::null_mut(), &mut a, &mut flags) != 0 && flags & LWA_ALPHA != 0 {
                Some(a)
            } else {
                None // UpdateLayeredWindow style: unknown
            }
        } else {
            None
        };
        TopWindow {
            hwnd: hwnd as isize,
            pid: window_pid(hwnd),
            title: String::from_utf16_lossy(&title[..tn.max(0) as usize]),
            class: String::from_utf16_lossy(&class[..cn.max(0) as usize]),
            rect: (r.left, r.top, r.right, r.bottom),
            visible: IsWindowVisible(hwnd) != 0,
            minimized: IsIconic(hwnd) != 0,
            cloaked: cloaked != 0,
            alpha,
        }
    }
}

/// Top-level windows owned by the given processes.
pub fn windows_of(pids: &[u32]) -> Vec<TopWindow> {
    top_level_hwnds()
        .into_iter()
        .filter(|&h| pids.contains(&window_pid(h)))
        .map(window_info)
        .collect()
}

/// The virtual screen (all monitors combined): `(left, top, right, bottom)`.
pub fn virtual_screen() -> (i32, i32, i32, i32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        (x, y, x + GetSystemMetrics(SM_CXVIRTUALSCREEN), y + GetSystemMetrics(SM_CYVIRTUALSCREEN))
    }
}

/// Whether a rectangle overlaps any monitor.
pub fn intersects_any_monitor(rect: (i32, i32, i32, i32)) -> bool {
    use windows_sys::Win32::Graphics::Gdi::{MonitorFromRect, MONITOR_DEFAULTTONULL};
    let r = RECT { left: rect.0, top: rect.1, right: rect.2, bottom: rect.3 };
    unsafe { !MonitorFromRect(&r, MONITOR_DEFAULTTONULL).is_null() }
}

/// Off-screen origin (physical pixels) for parking hidden-launched windows.
///
/// Beyond the bottom-right of every monitor, with margin for the Win32 coordinate limit
/// (±32767) and DPI scaling.
pub fn offscreen_origin() -> (i32, i32) {
    let (_, _, r, b) = virtual_screen();
    ((r + 4000).min(28000), (b + 4000).min(28000))
}

/// Process of the foreground window (the one with keyboard focus).
pub fn foreground_pid() -> Option<u32> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let h = unsafe { GetForegroundWindow() };
    if h.is_null() {
        None
    } else {
        Some(window_pid(h))
    }
}

/// Real mouse cursor position (screen coordinates).
pub fn cursor_pos() -> Option<(i32, i32)> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut p = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut p) } != 0 {
        Some((p.x, p.y))
    } else {
        None
    }
}

/// Running processes: `(pid, parent pid, executable name)`.
pub fn processes() -> Vec<(u32, u32, String)> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let snap = OwnedHandle(snap);
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap.0, &mut e) != 0 {
            loop {
                out.push((e.th32ProcessID, e.th32ParentProcessID, from_wide_buf(&e.szExeFile)));
                if Process32NextW(snap.0, &mut e) == 0 {
                    break;
                }
            }
        }
    }
    out
}

/// Full executable path of a process.
pub fn process_path(pid: u32) -> Option<PathBuf> {
    use windows_sys::Win32::System::Threading::{QueryFullProcessImageNameW, PROCESS_NAME_WIN32};
    unsafe {
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if p.is_null() {
            return None;
        }
        let p = OwnedHandle(p);
        let mut buf = vec![0u16; 1024];
        let mut n = buf.len() as u32;
        if QueryFullProcessImageNameW(p.0, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut n) == 0 {
            return None;
        }
        Some(PathBuf::from(OsString::from_wide(&buf[..n as usize])))
    }
}

/// Security descriptor for named pipes, Jobs and the like (from an SDDL string). Freed on drop.
pub struct SecurityDescriptor {
    pub psd: *mut core::ffi::c_void,
}

unsafe impl Send for SecurityDescriptor {}
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    pub fn from_sddl(sddl: &str) -> std::io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        let w = wide(sddl);
        let mut psd: *mut core::ffi::c_void = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(w.as_ptr(), SDDL_REVISION_1, &mut psd, std::ptr::null_mut())
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(SecurityDescriptor { psd })
    }

    /// Accessible only by this user (owner) and SYSTEM.
    pub fn owner_only() -> std::io::Result<Self> {
        let sid = current_user_sid()?;
        Self::from_sddl(&format!("O:{sid}D:P(A;;GA;;;{sid})(A;;GA;;;SY)"))
    }

    pub fn attributes(&self) -> windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.psd,
            bInheritHandle: 0,
        }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.psd.is_null() {
            unsafe { LocalFree(self.psd as HLOCAL) };
        }
    }
}

/// SID string of a kernel object's (named pipe, …) owner.
pub fn object_owner_sid(handle: HANDLE) -> std::io::Result<String> {
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
    unsafe {
        let mut owner: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut psd: *mut core::ffi::c_void = std::ptr::null_mut();
        let r = GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut psd,
        );
        if r != 0 {
            return Err(std::io::Error::from_raw_os_error(r as i32));
        }
        let mut s: *mut u16 = std::ptr::null_mut();
        let ok = ConvertSidToStringSidW(owner, &mut s);
        let out = if ok != 0 { Ok(from_wide_ptr(s)) } else { Err(std::io::Error::last_os_error()) };
        if !s.is_null() {
            LocalFree(s as HLOCAL);
        }
        LocalFree(psd as HLOCAL);
        out
    }
}

/// RID of the integrity label on a kernel object (named pipe, …). No label counts as medium
/// (INTEGRITY_MEDIUM).
///
/// Objects created by a below-medium process are labeled with the creator's level, and that
/// process can't apply a higher label or remove it (that takes SeRelabelPrivilege). Unlike
/// process and token DACLs, the creator can't hide it, so it's a reliable way to tell who
/// created the object. Readable with just READ_CONTROL on the handle.
pub fn object_integrity_rid(handle: HANDLE) -> std::io::Result<u32> {
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{
        GetAce, GetSidSubAuthority, GetSidSubAuthorityCount, ACE_HEADER, ACL, INHERIT_ONLY_ACE, LABEL_SECURITY_INFORMATION,
        SYSTEM_MANDATORY_LABEL_ACE,
    };
    const SYSTEM_MANDATORY_LABEL_ACE_TYPE: u8 = 0x11;
    unsafe {
        let mut sacl: *mut ACL = std::ptr::null_mut();
        let mut psd: *mut core::ffi::c_void = std::ptr::null_mut();
        let r = GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            LABEL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sacl,
            &mut psd,
        );
        if r != 0 {
            return Err(std::io::Error::from_raw_os_error(r as i32));
        }
        let _free = LocalBox(psd);
        if sacl.is_null() {
            return Ok(INTEGRITY_MEDIUM);
        }
        // with several label ACEs (normally there's one), use the lowest
        let mut found: Option<u32> = None;
        for i in 0..(*sacl).AceCount as u32 {
            let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
            if GetAce(sacl, i, &mut ace) == 0 || ace.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let header = &*(ace as *const ACE_HEADER);
            if header.AceType != SYSTEM_MANDATORY_LABEL_ACE_TYPE || header.AceFlags as u32 & INHERIT_ONLY_ACE != 0 {
                continue;
            }
            let sid = std::ptr::addr_of_mut!((*(ace as *mut SYSTEM_MANDATORY_LABEL_ACE)).SidStart) as *mut core::ffi::c_void;
            let count = GetSidSubAuthorityCount(sid);
            if count.is_null() || *count == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "무결성 레이블 SID 를 읽지 못했습니다"));
            }
            let rid = *GetSidSubAuthority(sid, (*count - 1) as u32);
            found = Some(found.map_or(rid, |f: u32| f.min(rid)));
        }
        Ok(found.unwrap_or(INTEGRITY_MEDIUM))
    }
}

/// Frees a pointer that needs LocalFree when it goes out of scope.
struct LocalBox(*mut core::ffi::c_void);

impl Drop for LocalBox {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0 as HLOCAL) };
        }
    }
}

/// Known folders (Program Files and such). Accurate even when launched with a scrubbed
/// environment.
pub fn program_files_dirs() -> Vec<PathBuf> {
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_ProgramFilesX64, FOLDERID_ProgramFilesX86, SHGetKnownFolderPath, KF_FLAG_DEFAULT,
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for id in [&FOLDERID_ProgramFilesX64, &FOLDERID_ProgramFilesX86] {
        unsafe {
            let mut p: *mut u16 = std::ptr::null_mut();
            if SHGetKnownFolderPath(id, KF_FLAG_DEFAULT as _, std::ptr::null_mut(), &mut p) == 0 && !p.is_null() {
                let s = from_wide_ptr(p);
                windows_sys::Win32::System::Com::CoTaskMemFree(p as *const _);
                let pb = PathBuf::from(s);
                if !out.contains(&pb) {
                    out.push(pb);
                }
            }
        }
    }
    if out.is_empty() {
        for v in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
            if let Some(p) = std::env::var_os(v).map(PathBuf::from) {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sid_and_env() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "{sid}");
        assert_eq!(process_user_sid(std::process::id()).unwrap(), sid);
        let env = user_environment().expect("user env");
        assert!(env.iter().any(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case("PATH")));
        assert!(env.iter().any(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case("USERPROFILE")));
        assert!(env.iter().all(|(k, _)| !k.is_empty() && !k.to_string_lossy().starts_with('=')));
    }

    #[test]
    fn env_entry_split() {
        let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        let kv = |k: &str, v: &str| Some((OsString::from(k), OsString::from(v)));
        assert_eq!(split_env_entry(&w(r"PATH=C:\x;C:\y")), kv("PATH", r"C:\x;C:\y"));
        // names starting with Hangul come through intact (no UTF-8 byte boundary issues)
        assert_eq!(split_env_entry(&w(r"한글_HOME=D:\자료")), kv("한글_HOME", r"D:\자료"));
        // '=' inside the value is part of it, and empty values are allowed
        assert_eq!(split_env_entry(&w("A=b=c")), kv("A", "b=c"));
        assert_eq!(split_env_entry(&w("EMPTY=")), kv("EMPTY", ""));
        // hidden entries like per-drive current directories and entries without '=' are skipped
        assert_eq!(split_env_entry(&w(r"=C:=C:\Windows")), None);
        assert_eq!(split_env_entry(&w("=ExitCode=00000000")), None);
        assert_eq!(split_env_entry(&w("NOEQ")), None);
        // values with unpaired surrogates survive losslessly (not replaced with U+FFFD)
        let mut raw = w("P=C:\\");
        raw.push(0xD800);
        raw.extend(w("x"));
        let (k, v) = split_env_entry(&raw).unwrap();
        assert_eq!(k, OsString::from("P"));
        assert_eq!(v.encode_wide().collect::<Vec<u16>>(), raw[2..].to_vec());
    }

    #[test]
    fn integrity_levels() {
        let me = current_integrity_rid().unwrap();
        // tests run at medium (or elevated)
        assert!(me >= INTEGRITY_MEDIUM, "{me:#x}");
        assert_eq!(process_integrity_rid(std::process::id()).unwrap(), me);
        assert_eq!(integrity_name(INTEGRITY_LOW), "낮음");
        assert_eq!(integrity_name(INTEGRITY_MEDIUM), "보통");
        assert_eq!(integrity_name(0x2100), "보통");
        assert_eq!(integrity_name(INTEGRITY_HIGH), "높음(관리자)");
        assert_eq!(integrity_name(0), "신뢰 안 함");
        // a nonexistent pid is an error
        assert!(process_integrity_rid(u32::MAX - 2).is_err());
    }

    #[test]
    fn registry_and_version() {
        // keys and files present on every Windows
        assert!(reg_string(HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", Some("ProductName")).is_some());
        assert!(!reg_subkeys(HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft").is_empty());
        let sys = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let vi = version_info(&PathBuf::from(sys).join(r"System32\notepad.exe")).expect("notepad version");
        assert!(vi.original_filename.unwrap_or_default().to_lowercase().starts_with("notepad"));
        assert!(!program_files_dirs().is_empty());
    }

    #[test]
    fn windows_and_processes() {
        let ps = processes();
        assert!(ps.iter().any(|(p, _, _)| *p == std::process::id()));
        assert!(process_path(std::process::id()).is_some());
        let _ = top_level_hwnds();
        let (l, t, r, b) = virtual_screen();
        assert!(r > l && b > t);
        let (x, y) = offscreen_origin();
        assert!(!intersects_any_monitor((x, y, x + 100, y + 100)));
    }

    #[test]
    fn sddl() {
        let sd = SecurityDescriptor::owner_only().unwrap();
        assert!(!sd.psd.is_null());
    }
}
