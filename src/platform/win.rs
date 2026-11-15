//! Windows 전용 도우미 (Win32, windows-sys).
//!
//! 사용자 SID, 사용자 기본 환경, 콘솔·표준 핸들 정리, 레지스트리·버전 정보 읽기,
//! 보이는 창 검사(숨김 실행한 앱이 사용자 화면에 나타났는지 감시) 등.

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

/// UTF-16, NUL 종료
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// NUL 종료 UTF-16 포인터 → String
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

/// 고정 길이 UTF-16 버퍼(NUL 까지) → String
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
    // TOKEN_USER 는 포인터를 담으므로 8바이트 정렬 버퍼를 쓴다
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

/// 현재 프로세스 사용자의 SID 문자열 (예: S-1-5-21-…-1001)
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

/// 다른 프로세스의 토큰을 TOKEN_QUERY 로 열어 `f` 에 넘긴다 (권한이 없으면 오류)
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

/// 다른 프로세스 사용자의 SID 문자열 (권한이 없으면 오류)
pub fn process_user_sid(pid: u32) -> std::io::Result<String> {
    with_process_token(pid, |t| unsafe { token_user_sid_string(t) })
}

/// 무결성 수준 RID (SECURITY_MANDATORY_*_RID)
pub const INTEGRITY_LOW: u32 = 0x1000;
pub const INTEGRITY_MEDIUM: u32 = 0x2000;
pub const INTEGRITY_HIGH: u32 = 0x3000;
pub const INTEGRITY_SYSTEM: u32 = 0x4000;

/// 사람이 읽을 무결성 수준 이름 (오류 메시지용)
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
    // TOKEN_MANDATORY_LABEL 은 포인터를 담으므로 8바이트 정렬 버퍼를 쓴다
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
    // 무결성 SID(S-1-16-<RID>)의 마지막 하위 권한이 RID
    Ok(*GetSidSubAuthority(sid, (*count - 1) as u32))
}

/// 이 프로세스의 무결성 수준 RID
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

/// 다른 프로세스의 무결성 수준 RID (권한이 없으면 오류: 보통 그 프로세스가 더 높은 권한이라는 뜻)
pub fn process_integrity_rid(pid: u32) -> std::io::Result<u32> {
    with_process_token(pid, |t| unsafe { token_integrity_rid(t) })
}

/// 이 사용자의 기본 환경변수 (MCP 호스트가 지운 환경이 아니라 로그인 때의 환경).
/// 실패하면 None (호출하는 쪽은 현재 환경을 그대로 쓴다).
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

/// 환경 블록의 항목 하나("이름=값", UTF-16) → (이름, 값).
/// UTF-16 그대로 나누므로 한글로 시작하는 이름이나 짝 없는 서로게이트가 든 값도 바뀌지 않는다.
/// "=C:=C:\..." 처럼 '=' 로 시작하는 숨은 항목(드라이브별 현재 폴더)과 '=' 가 없는 항목은 None.
fn split_env_entry(w: &[u16]) -> Option<(OsString, OsString)> {
    const EQ: u16 = b'=' as u16;
    if w.first() == Some(&EQ) {
        return None;
    }
    let eq = w.iter().position(|&c| c == EQ)?;
    Some((OsString::from_wide(&w[..eq]), OsString::from_wide(&w[eq + 1..])))
}

/// 표준 입출력 핸들을 자식에게 상속되지 않게 한다.
///
/// Rust std 의 `Command::spawn` 은 Windows 에서 bInheritHandles=TRUE 로 띄우므로, MCP 호스트가 넘긴
/// 파이프 핸들(상속 가능)이 GhostDesk 창 → 브라우저 → … 로 새어 나가, MCP 서버가 끝나도 호스트가
/// EOF 를 받지 못한다. std 의 Stdio::inherit/piped/null 은 자식마다 따로 상속 가능한 복사본을 만들므로
/// 원본의 상속 플래그를 꺼도 안전하다. 프로그램 시작 직후 한 번 호출한다.
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

/// GUI 로 실행되었는데 이 프로세스만을 위해 콘솔 창이 새로 생겼다면(탐색기에서 더블클릭 등) 그 창을 닫는다.
/// 터미널에서 실행했다면 그 터미널은 그대로 둔다.
pub fn detach_own_console() {
    if owns_console_alone() {
        unsafe { windows_sys::Win32::System::Console::FreeConsole() };
    }
}

/// 이 프로세스만 쓰는 콘솔이 붙어 있는가 (탐색기 더블클릭 등으로 이 프로세스를 위해 새로 생긴 콘솔).
/// 터미널에서 실행했으면 셸과 콘솔을 함께 쓰므로 false.
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

/// 이 프로세스를 모니터별 DPI 인식(PerMonitorV2)으로 (창 좌표·캡처가 실제 픽셀 단위가 되게).
/// GUI(winit)는 스스로 설정하므로, 창 없이 Win32 창을 다루는 모드에서 쓴다.
pub fn set_dpi_aware() {
    use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

// ───────────────────────── 레지스트리 ─────────────────────────

pub use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

/// 문자열 값 하나 (REG_SZ / REG_EXPAND_SZ — 확장한 값). `value` 가 None 이면 기본값.
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

/// 하위 키 이름 목록
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

/// "App Paths" 에 등록된 실행 파일 경로 (예: "chrome.exe" → C:\Program Files\Google\Chrome\Application\chrome.exe)
pub fn app_path(exe_name: &str) -> Option<PathBuf> {
    let sub = format!(r"Software\Microsoft\Windows\CurrentVersion\App Paths\{exe_name}");
    [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
        .into_iter()
        .filter_map(|root| reg_string(root, &sub, None))
        .map(|s| PathBuf::from(s.trim().trim_matches('"')))
        .find(|p| p.is_file())
}

/// 설치된 프로그램 목록 (Uninstall 키): (표시 이름, 설치 위치, 표시 아이콘 경로)
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
                // "C:\x\app.exe,0" → 경로만
                let s = s.trim().trim_matches('"');
                let s = s.rsplit_once(',').filter(|(_, n)| n.trim().parse::<i32>().is_ok()).map(|(p, _)| p).unwrap_or(s);
                PathBuf::from(s.trim().trim_matches('"'))
            });
            out.push((name, loc, icon));
        }
    }
    out
}

// ───────────────────────── 실행 파일 버전 정보 ─────────────────────────

#[derive(Clone, Debug, Default)]
pub struct VersionInfo {
    pub product_name: Option<String>,
    pub file_description: Option<String>,
    pub original_filename: Option<String>,
    pub company_name: Option<String>,
    pub product_version: Option<String>,
}

/// 실행 파일의 VERSIONINFO 리소스
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
        // 언어·코드페이지 목록에서 첫 번째를 쓴다 (없으면 영어 유니코드)
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

// ───────────────────────── 창 검사 ─────────────────────────

/// 최상위 창 하나의 상태
#[derive(Clone, Debug)]
pub struct TopWindow {
    pub hwnd: isize,
    pub pid: u32,
    pub title: String,
    pub class: String,
    /// 화면 좌표 (실제 픽셀, DPI 인식 프로세스 기준)
    pub rect: (i32, i32, i32, i32),
    pub visible: bool,
    pub minimized: bool,
    /// DWM 이 감춘 창 (가상 데스크톱의 다른 창, UWP 일시정지 등)
    pub cloaked: bool,
    /// 레이어드 창의 투명도 (None 이면 레이어드 아님 / 알 수 없음)
    pub alpha: Option<u8>,
    pub tool_window: bool,
}

impl TopWindow {
    /// 사용자 눈에 실제로 보일 수 있는 창인가: 보임 + 최소화 아님 + 숨김 아님 + 불투명 + 모니터와 겹침
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

/// 모든 최상위 창 HWND (z 순서, 위가 먼저)
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
        IsWindowVisible, GWL_EXSTYLE, LWA_ALPHA, WS_EX_LAYERED, WS_EX_TOOLWINDOW,
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
                None // UpdateLayeredWindow 방식: 알 수 없음
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
            tool_window: ex & WS_EX_TOOLWINDOW != 0,
        }
    }
}

/// 주어진 프로세스들이 가진 최상위 창
pub fn windows_of(pids: &[u32]) -> Vec<TopWindow> {
    top_level_hwnds()
        .into_iter()
        .filter(|&h| pids.contains(&window_pid(h)))
        .map(window_info)
        .collect()
}

/// 가상 화면(모든 모니터를 합친 사각형): (left, top, right, bottom)
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

/// 사각형이 어느 모니터와든 겹치는가
pub fn intersects_any_monitor(rect: (i32, i32, i32, i32)) -> bool {
    use windows_sys::Win32::Graphics::Gdi::{MonitorFromRect, MONITOR_DEFAULTTONULL};
    let r = RECT { left: rect.0, top: rect.1, right: rect.2, bottom: rect.3 };
    unsafe { !MonitorFromRect(&r, MONITOR_DEFAULTTONULL).is_null() }
}

/// 숨김 실행한 창을 놓아 둘 화면 밖 좌표 (실제 픽셀). 모든 모니터의 오른쪽 아래 바깥이며,
/// Win32 좌표 한계(±32767)와 DPI 배율을 감안해 여유를 둔다.
pub fn offscreen_origin() -> (i32, i32) {
    let (_, _, r, b) = virtual_screen();
    ((r + 4000).min(28000), (b + 4000).min(28000))
}

/// 지금 키보드 포커스를 가진(전경) 창의 프로세스
pub fn foreground_pid() -> Option<u32> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let h = unsafe { GetForegroundWindow() };
    if h.is_null() {
        None
    } else {
        Some(window_pid(h))
    }
}

/// 실제 마우스 커서 위치 (화면 좌표)
pub fn cursor_pos() -> Option<(i32, i32)> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut p = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut p) } != 0 {
        Some((p.x, p.y))
    } else {
        None
    }
}

/// 실행 중인 프로세스 목록: (pid, 부모 pid, 실행 파일 이름)
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

/// 프로세스의 실행 파일 전체 경로
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

/// 명명 파이프·Job 등에 줄 보안 설명자 (SDDL 문자열에서). 해제는 Drop 이 한다.
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

    /// 이 사용자(소유자)와 SYSTEM 만 접근 가능
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

/// 커널 객체(명명 파이프 등) 소유자의 SID 문자열
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

/// 커널 객체(명명 파이프 등)에 붙은 무결성 레이블의 RID. 레이블이 없으면 보통(INTEGRITY_MEDIUM)으로 본다.
/// 보통보다 낮은 무결성 프로세스가 만든 객체에는 만든 프로세스의 수준이 레이블로 붙고, 그 프로세스는
/// 자기보다 높은 레이블을 붙이거나 레이블을 뗄 수 없다 (SeRelabelPrivilege 필요). 프로세스·토큰의 DACL 과 달리
/// 만든 쪽이 가릴 수 없으므로 "누가 만들었나"를 확인하는 믿을 만한 근거다. 핸들에 READ_CONTROL 만 있으면 읽을 수 있다.
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
        // 레이블 ACE 가 여럿이면(정상적으로는 하나) 가장 낮은 것을 쓴다
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

/// LocalFree 로 풀어야 하는 포인터를 범위가 끝날 때 푼다
struct LocalBox(*mut core::ffi::c_void);

impl Drop for LocalBox {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0 as HLOCAL) };
        }
    }
}

/// 알려진 폴더 (Program Files 등). 환경변수가 지워진 채 실행되어도 정확하다.
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
        // 한글로 시작하는 이름도 그대로 (UTF-8 바이트 경계와 무관)
        assert_eq!(split_env_entry(&w(r"한글_HOME=D:\자료")), kv("한글_HOME", r"D:\자료"));
        // 값 안의 '=' 는 값의 일부, 빈 값도 허용
        assert_eq!(split_env_entry(&w("A=b=c")), kv("A", "b=c"));
        assert_eq!(split_env_entry(&w("EMPTY=")), kv("EMPTY", ""));
        // 드라이브별 현재 폴더 같은 숨은 항목과 '=' 없는 항목은 건너뛴다
        assert_eq!(split_env_entry(&w(r"=C:=C:\Windows")), None);
        assert_eq!(split_env_entry(&w("=ExitCode=00000000")), None);
        assert_eq!(split_env_entry(&w("NOEQ")), None);
        // 짝 없는 서로게이트가 든 값도 손실 없이 (U+FFFD 로 바뀌지 않음)
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
        // 테스트는 보통(또는 관리자) 권한으로 돈다
        assert!(me >= INTEGRITY_MEDIUM, "{me:#x}");
        assert_eq!(process_integrity_rid(std::process::id()).unwrap(), me);
        assert_eq!(integrity_name(INTEGRITY_LOW), "낮음");
        assert_eq!(integrity_name(INTEGRITY_MEDIUM), "보통");
        assert_eq!(integrity_name(0x2100), "보통");
        assert_eq!(integrity_name(INTEGRITY_HIGH), "높음(관리자)");
        assert_eq!(integrity_name(0), "신뢰 안 함");
        // 없는 pid 는 오류
        assert!(process_integrity_rid(u32::MAX - 2).is_err());
    }

    #[test]
    fn registry_and_version() {
        // 모든 Windows 에 있는 키·파일
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
