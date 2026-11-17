//! Windows child session (PiP) backend: gives the agent the whole screen of a **separate
//! Windows session** on the same PC and account.
//!
//! An official Microsoft feature (Child Sessions): when the RDP ActiveX control in the parent
//! (user) session makes a loopback `ConnectToChildSession`, a child session of the same user logs
//! on. It has its own desktop and input queue, so SendInput and screen capture inside it never
//! touch the user's screen, cursor or keyboard.
//!
//! Layout (same helper RPC as the macOS separate-account session, see `session.rs`):
//! - Parent: [`ensure_connected`] hosts the RDP control (AtlAxWin) in an off-screen window on an
//!   STA thread, connects to the child session, and writes the child session id to a marker
//!   file. The helper launch is scheduled via HKCU RunOnce right before connecting.
//! - Child: [`run_helper`] (`ghostdesk --session-helper`) opens the capture/input server on the
//!   named pipe `GhostDesk-child-<SID>` only if the marker's id matches its own session id. In
//!   any other session (the user's own screen) it exits immediately.
//! - The user watches the child session through the live view in the GhostDesk window. The RDP
//!   window itself stays off-screen (not hidden or minimized, since minimized RDP can stop
//!   updating the screen).
//!
//! Limits: one connected child session system-wide, and it ends with the parent session.
//! Enabling the feature (`WTSEnableChildSessions`) needs admin rights, so the user approves it
//! via UAC; see [`request_enable`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use image::RgbImage;
use parking_lot::Mutex;
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSEnableChildSessions, WTSGetActiveConsoleSessionId, WTSGetChildSessionId,
    WTSIsChildSessionsEnabled,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::keys::{MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT};
use crate::platform::ipc::Endpoint;
use crate::platform::win::wide;
use crate::session::{fit_size, DisplayInfo, ServerConfig, SessionPlatform};

/// Child session screen size (RDP desktop size).
const DESK_W: i32 = 1280;
const DESK_H: i32 = 800;
/// How long to wait for the child session to log on and the helper to start.
pub const HELPER_WAIT: Duration = Duration::from_secs(90);
const CONNECT_WAIT: Duration = Duration::from_secs(60);
const RUNONCE_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\RunOnce";
const RUNONCE_VALUE: &str = "GhostDeskChildHelper";

pub const HELPER_MISSING: &str = "자식 세션 안의 GhostDesk 헬퍼에 연결하지 못했습니다. 자식 세션이 이미 로그온된 상태에서 헬퍼가 꺼졌다면 \
     자동으로 다시 뜨지 않습니다 — 자식 세션에서 로그아웃(또는 PC 다시 로그인)한 뒤 다시 시도하세요.";

pub const SETUP_GUIDE_KO: &str = r#"## 자식 세션(PiP) — 같은 PC 의 별도 Windows 화면

Windows 의 공식 기능인 '자식 세션'으로, 지금 계정 그대로 **별도의 Windows 화면**을 하나 더 만들어 에이전트에게 줍니다.
에이전트의 마우스·키보드는 그 화면 안에서만 움직이고 내 화면·커서·키보드에는 닿지 않습니다. 진행 상황은 GhostDesk 창에서 봅니다.

### 처음 한 번
1. 아래 '자식 세션 켜기' 를 누르고 관리자 승인(UAC)을 허락합니다. (`WTSEnableChildSessions` — 시스템 설정 하나를 켭니다)
2. 그 뒤 에이전트를 시작하면 자식 세션이 로그온되고(처음엔 수십 초) GhostDesk 헬퍼가 그 안에서 자동으로 실행됩니다.

### 알아 둘 점
- 같은 Windows 계정이라 설치된 앱·파일을 그대로 씁니다. 단, 이미 내 화면에서 쓰는 앱·브라우저 프로필은 동시에 열지 못할 수 있습니다.
- 자식 세션은 PC 전체에서 하나만 연결됩니다. 내가 로그아웃하면 자식 세션도 끝납니다.
- 스마트카드·Windows Hello 전용 로그인 등 일부 인증 방식에서는 자식 세션 자동 로그온이 되지 않을 수 있습니다.
- 끄려면 관리자 PowerShell 에서: `Set-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Terminal Server' fDenyChildConnections 1`"#;

// ---- session info ----

pub fn enabled() -> bool {
    let mut on: BOOL = 0;
    unsafe { WTSIsChildSessionsEnabled(&mut on) != 0 && on != 0 }
}

/// Child session id of this (parent) session.
pub fn child_session_id() -> Option<u32> {
    let mut id = 0u32;
    (unsafe { WTSGetChildSessionId(&mut id) } != 0 && id != u32::MAX && id != 0).then_some(id)
}

fn my_session_id() -> u32 {
    let mut id = u32::MAX;
    unsafe { ProcessIdToSessionId(std::process::id(), &mut id) };
    id
}

/// Helper pipe. Its name includes the user's SID, and ipc checks owner and integrity.
pub fn endpoint() -> Endpoint {
    Endpoint::custom("GhostDesk-child")
}

/// Marker file where the parent writes the id of the child session it just connected.
fn marker_path() -> PathBuf {
    crate::platform::paths::data_root().join("child-session-id")
}

/// Whether the helper may run in this session (pure function of the marker id, our session and
/// the console session).
fn helper_allowed(marker: Option<u32>, me: u32, console: u32) -> Result<(), String> {
    match marker {
        None => Err("부모 GhostDesk 가 자식 세션을 연결한 기록이 없습니다".into()),
        Some(_) if me == console => Err(format!("세션 {me} 은 지금 모니터에 표시 중인 사용자 화면입니다")),
        Some(m) if m != me => Err(format!("이 세션({me})은 GhostDesk 가 연결한 자식 세션({m})이 아닙니다")),
        Some(_) => Ok(()),
    }
}

// ---- enabling the feature (admin) ----

/// `ghostdesk --enable-child-sessions` (runs elevated).
pub fn enable_cli() -> Result<()> {
    if unsafe { WTSEnableChildSessions(1) } == 0 {
        bail!("WTSEnableChildSessions 실패: {}", std::io::Error::last_os_error());
    }
    println!("자식 세션을 켰습니다.");
    Ok(())
}

/// Shows the admin approval (UAC) prompt to enable child sessions; only enabled if the user
/// approves. Blocking, so don't call it from the GUI thread.
pub fn request_enable() -> Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    let exe = std::env::current_exe()?;
    let (verb, file, params) = (wide("runas"), wide(&exe), wide("--enable-child-sessions"));
    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOCLOSEPROCESS;
    sei.lpVerb = verb.as_ptr();
    sei.lpFile = file.as_ptr();
    sei.lpParameters = params.as_ptr();
    sei.nShow = 0; // SW_HIDE
    if unsafe { ShellExecuteExW(&mut sei) } == 0 {
        bail!("관리자 승인이 취소되었거나 실행하지 못했습니다 ({})", std::io::Error::last_os_error());
    }
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(sei.hProcess, INFINITE);
        GetExitCodeProcess(sei.hProcess, &mut code);
        CloseHandle(sei.hProcess);
    }
    if code != 0 || !enabled() {
        bail!("자식 세션을 켜지 못했습니다 (종료 코드 {code})");
    }
    Ok(())
}

// ---- capture and input (helper inside the child session) ----

/// Captures the child session's primary monitor with GDI and drives it with SendInput. Only
/// affects this process's own session.
struct ChildPlatform {
    session: u32,
}

fn virtual_screen() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        )
    }
}

fn send(inputs: &[INPUT]) -> Result<()> {
    let n = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    if n as usize != inputs.len() {
        bail!("SendInput 실패 ({}/{}): {}", n, inputs.len(), std::io::Error::last_os_error());
    }
    Ok(())
}

fn mouse(dx: i32, dy: i32, data: i32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn key_input(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn vk_event(vk: u16, up: bool) -> INPUT {
    let mut f = if up { KEYEVENTF_KEYUP } else { 0 };
    if super::input::is_extended_vk(vk as u32) {
        f |= KEYEVENTF_EXTENDEDKEY;
    }
    key_input(vk, 0, f)
}

/// GhostDesk modifier bits → virtual keys.
fn mod_vks(mods: i64) -> Vec<u16> {
    [(MOD_CTRL, 0x11), (MOD_SHIFT, 0x10), (MOD_ALT, 0x12), (MOD_META, 0x5B)]
        .into_iter()
        .filter(|(b, _)| mods & b != 0)
        .map(|(_, vk)| vk)
        .collect()
}

/// Sends `events` with the modifiers held down.
fn with_mods(mods: i64, events: Vec<INPUT>) -> Result<()> {
    let vks = mod_vks(mods);
    let mut all: Vec<INPUT> = vks.iter().map(|&v| vk_event(v, false)).collect();
    all.extend(events);
    all.extend(vks.iter().rev().map(|&v| vk_event(v, true)));
    send(&all)
}

impl ChildPlatform {
    /// Global pixel coordinates → SendInput absolute coordinates (virtual desktop 0..65535).
    fn abs(&self, x: f64, y: f64) -> (i32, i32) {
        let (vx, vy, vw, vh) = virtual_screen();
        let nx = ((x - vx as f64) * 65535.0 / (vw - 1).max(1) as f64).round() as i32;
        let ny = ((y - vy as f64) * 65535.0 / (vh - 1).max(1) as f64).round() as i32;
        (nx, ny)
    }
    fn move_ev(&self, x: f64, y: f64) -> INPUT {
        let (nx, ny) = self.abs(x, y);
        mouse(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)
    }
}

impl SessionPlatform for ChildPlatform {
    fn name(&self) -> String {
        format!("Windows 자식 세션 {}", self.session)
    }
    fn display(&self) -> Result<DisplayInfo> {
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        if w <= 0 || h <= 0 {
            bail!("화면 크기를 알 수 없습니다");
        }
        Ok(DisplayInfo { origin_x: 0.0, origin_y: 0.0, width_pt: w as f64, height_pt: h as f64, scale: 1.0 })
    }
    fn capture(&self, max_long_edge: u32) -> Result<RgbImage> {
        use windows_sys::Win32::Graphics::Gdi::{
            BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC, ReleaseDC,
            SelectObject, BITMAPINFO, BITMAPINFOHEADER, CAPTUREBLT, DIB_RGB_COLORS, SRCCOPY,
        };
        let d = self.display()?;
        let (w, h) = (d.width_pt as i32, d.height_pt as i32);
        let buf = unsafe {
            let screen = GetDC(std::ptr::null_mut());
            if screen.is_null() {
                bail!("화면 DC 를 얻지 못했습니다");
            }
            let mem = CreateCompatibleDC(screen);
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            let ok = !dib.is_null() && !bits.is_null() && {
                let old = SelectObject(mem, dib as _);
                let r = BitBlt(mem, 0, 0, w, h, screen, 0, 0, SRCCOPY | CAPTUREBLT) != 0;
                GdiFlush();
                SelectObject(mem, old);
                r
            };
            let buf = ok.then(|| std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize).to_vec());
            if !dib.is_null() {
                DeleteObject(dib as _);
            }
            DeleteDC(mem);
            ReleaseDC(std::ptr::null_mut(), screen);
            buf.ok_or_else(|| anyhow!("화면 캡처(BitBlt) 실패 — 자식 세션 연결이 끊겼을 수 있습니다"))?
        };
        let full = super::capture::crop_bgra(&buf, w as usize, 0, 0, w as usize, h as usize);
        let (tw, th) = fit_size(w as f64, h as f64, 1.0, max_long_edge);
        Ok(if (tw, th) == (w as u32, h as u32) {
            full
        } else {
            image::imageops::resize(&full, tw, th, image::imageops::FilterType::Triangle)
        })
    }
    fn mouse_move(&self, x: f64, y: f64, _pressed: Option<u8>, _mods: i64) -> Result<()> {
        // a pressed button stays down, so plain moves turn into a drag
        send(&[self.move_ev(x, y)])
    }
    fn mouse_button(&self, x: f64, y: f64, button: u8, down: bool, _count: i64, mods: i64) -> Result<()> {
        // Windows decides double clicks from the interval between clicks (the server sends down/up
        // `count` times)
        let flag = match (button, down) {
            (0, true) => MOUSEEVENTF_LEFTDOWN,
            (0, false) => MOUSEEVENTF_LEFTUP,
            (1, true) => MOUSEEVENTF_RIGHTDOWN,
            (1, false) => MOUSEEVENTF_RIGHTUP,
            (_, true) => MOUSEEVENTF_MIDDLEDOWN,
            (_, false) => MOUSEEVENTF_MIDDLEUP,
        };
        with_mods(mods, vec![self.move_ev(x, y), mouse(0, 0, 0, flag)])
    }
    fn scroll(&self, dx: f64, dy: f64, mods: i64) -> Result<()> {
        let mut ev = Vec::new();
        let v = super::input::wheel_notches(dy);
        if v != 0 {
            ev.push(mouse(0, 0, -v * 120, MOUSEEVENTF_WHEEL)); // dy > 0 = down = negative wheel
        }
        let hz = super::input::wheel_notches(dx);
        if hz != 0 {
            ev.push(mouse(0, 0, hz * 120, MOUSEEVENTF_HWHEEL));
        }
        if ev.is_empty() {
            return Ok(());
        }
        with_mods(mods, ev)
    }
    fn key(&self, combo: &str, hold: Duration, check: &dyn Fn() -> Result<()>) -> Result<()> {
        let c = crate::keys::parse_combo(combo)?;
        let vk = c.main.key_code as u16;
        if vk == 0 {
            bail!("보낼 수 없는 키입니다: {combo}");
        }
        let mods = mod_vks(c.mods);
        let mut down: Vec<INPUT> = mods.iter().map(|&v| vk_event(v, false)).collect();
        down.push(vk_event(vk, false));
        check()?;
        send(&down)?;
        let held = crate::session::hold_checked(hold, check);
        let mut up = vec![vk_event(vk, true)];
        up.extend(mods.iter().rev().map(|&v| vk_event(v, true)));
        send(&up)?;
        held
    }
    fn type_unicode(&self, text: &str, check: &dyn Fn() -> Result<()>) -> Result<()> {
        let ev: Vec<INPUT> = text
            .encode_utf16()
            .flat_map(|u| [key_input(0, u, KEYEVENTF_UNICODE), key_input(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)])
            .collect();
        if ev.is_empty() {
            return Ok(());
        }
        check()?;
        send(&ev)
    }
    fn cursor(&self) -> Result<(f64, f64)> {
        let mut p = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut p) } == 0 {
            bail!("커서 위치를 읽지 못했습니다");
        }
        Ok((p.x as f64, p.y as f64))
    }
    fn on_console(&self) -> bool {
        unsafe { WTSGetActiveConsoleSessionId() == self.session }
    }
}

// ---- helper (--session-helper, inside the child session) ----

fn read_marker() -> Option<u32> {
    std::fs::read_to_string(marker_path()).ok()?.trim().parse().ok()
}

/// Started via RunOnce when the child session logs on. Does nothing and exits outside a child
/// session.
pub async fn run_helper(_args: &[String]) -> Result<()> {
    let me = my_session_id();
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    // logon may finish before the parent writes the marker, so wait a bit
    let deadline = Instant::now() + Duration::from_secs(60);
    let verdict = loop {
        let v = helper_allowed(read_marker(), me, console);
        if v.is_ok() || Instant::now() >= deadline {
            break v;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    if let Err(why) = verdict {
        // e.g. the user's own session: exit without calling any screen or input API
        eprintln!("[ghostdesk-session-helper] 실행 거부: {why}");
        bail!("세션 헬퍼 실행 거부: {why}");
    }
    crate::platform::win::set_dpi_aware();
    let log = crate::session::helper_logger();
    let ep = endpoint();
    let listener = crate::session::HelperListener::bind(&ep)
        .await
        .with_context(|| format!("헬퍼 파이프 {} 를 열지 못했습니다 (이미 실행 중?)", ep.display()))?;
    log(&format!("자식 세션 {me} 에서 대기 중: {}", ep.display()));
    let cfg = ServerConfig { allow_uids: vec![], allow_on_console: false, label: format!("자식 세션 {me}"), log: log.clone() };
    let handle = crate::session::spawn_server(listener, Arc::new(ChildPlatform { session: me }), cfg);
    // this process ends when the child session logs off
    let _ = tokio::signal::ctrl_c().await;
    handle.stop().await;
    log("종료됨");
    Ok(())
}

/// `ghostdesk --selftest-session`: child session state (read only) plus a mock helper round
/// trip. Changes no system settings.
pub async fn selftest(_args: &[String]) -> Result<()> {
    println!(
        "자식 세션 기능: {} / 연결된 자식 세션: {:?} / 내 세션 {} (콘솔 {})",
        if enabled() { "켜짐" } else { "꺼짐" },
        child_session_id(),
        my_session_id(),
        unsafe { WTSGetActiveConsoleSessionId() }
    );
    crate::session::pipe_roundtrip().await?;
    println!("mock 헬퍼 ↔ SessionDesk 명명 파이프 왕복: 통과");
    Ok(())
}

// ---- parent: connecting the child session through the RDP control ----

/// STA thread holding the connection. It ends with GhostDesk; the child session then stays
/// logged on but disconnected.
struct Connector {
    alive: Arc<AtomicBool>,
}

static CONNECTOR: Mutex<Option<Connector>> = Mutex::new(None);

/// Makes sure the child session is connected (returns right away if it already is).
pub async fn ensure_connected() -> Result<()> {
    if !enabled() {
        bail!("Windows 자식 세션 기능이 꺼져 있습니다. 새 에이전트 창의 '자식 세션' 에서 '자식 세션 켜기' 를 눌러 관리자 승인 후 다시 시도하세요.");
    }
    {
        let g = CONNECTOR.lock();
        if g.as_ref().is_some_and(|c| c.alive.load(Ordering::SeqCst)) && child_session_id().is_some() {
            return Ok(());
        }
    }
    let exe = std::env::current_exe()?;
    // schedule the helper for the next logon (the child session about to start); Windows removes
    // the value once it runs
    set_run_once(&format!("\"{}\" --session-helper", exe.display()))?;
    let alive = Arc::new(AtomicBool::new(true));
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<()>>();
    let a = alive.clone();
    std::thread::Builder::new()
        .name("ghostdesk-child-session".into())
        .spawn(move || {
            let mut tx = Some(tx);
            let r = rdp::run(&a, &mut tx);
            a.store(false, Ordering::SeqCst);
            let _ = std::fs::remove_file(marker_path());
            if let (Some(tx), Err(e)) = (tx.take(), r) {
                let _ = tx.send(Err(e));
            }
        })?;
    *CONNECTOR.lock() = Some(Connector { alive });
    rx.await.map_err(|_| anyhow!("자식 세션 연결 스레드가 끝났습니다"))?
}

fn set_run_once(cmd: &str) -> Result<()> {
    use windows_sys::Win32::System::Registry::{RegSetKeyValueW, REG_SZ};
    let data = wide(cmd);
    let r = unsafe {
        RegSetKeyValueW(
            crate::platform::win::HKEY_CURRENT_USER,
            wide(RUNONCE_KEY).as_ptr(),
            wide(RUNONCE_VALUE).as_ptr(),
            REG_SZ,
            data.as_ptr() as _,
            (data.len() * 2) as u32,
        )
    };
    if r != 0 {
        bail!("헬퍼 실행 예약(RunOnce) 실패: 오류 {r}");
    }
    Ok(())
}

mod rdp {
    //! Hosts the RDP ActiveX control (MsTscAx) in an ATL AtlAxWin window and drives it through
    //! IDispatch. State is polled; there's no event sink.
    use super::*;
    use windows::core::{Interface, BSTR, GUID, HRESULT, IUnknown, PCWSTR};
    use windows::Win32::System::Com::{
        CoInitializeEx, IDispatch, COINIT_APARTMENTTHREADED, DISPATCH_FLAGS, DISPATCH_METHOD, DISPATCH_PROPERTYGET,
        DISPATCH_PROPERTYPUT, DISPPARAMS,
    };
    use windows::Win32::System::Variant::{VariantClear, VARIANT, VARIANT_0_0, VT_BOOL, VT_BSTR, VT_I2, VT_I4};
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, ShowWindow, TranslateMessage, MSG, PM_REMOVE,
        SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
    };

    /// MSTSCAX's IMsRdpExtendedSettings (the only place `ConnectToChildSession` lives). vtable:
    /// put_Property comes after the three IUnknown slots.
    const IID_EXTENDED_SETTINGS: GUID = GUID::from_u128(0x302D8188_0052_4807_806A_362B628F9AC5);
    type PutProperty = unsafe extern "system" fn(*mut core::ffi::c_void, *const u16, *const VARIANT) -> HRESULT;

    /// IMsRdpExtendedSettings::put_Property(name, value)
    fn put_extended(unk: &IUnknown, name: &str, value: &VARIANT) -> Result<()> {
        let mut raw = std::ptr::null_mut();
        unsafe { unk.query(&IID_EXTENDED_SETTINGS, &mut raw) }
            .ok()
            .context("IMsRdpExtendedSettings 가 없습니다 (Windows 버전 확인)")?;
        let owner = unsafe { IUnknown::from_raw(raw) }; // Release on drop
        let name = BSTR::from(name);
        let hr = unsafe {
            let vtbl = *(raw as *const *const usize);
            let put: PutProperty = std::mem::transmute(*vtbl.add(3));
            put(raw, name.as_ptr(), value)
        };
        drop(owner);
        hr.ok().map_err(Into::into)
    }

    const LOCALE_USER_DEFAULT: u32 = 0x0400;
    const DISPID_PROPERTYPUT: i32 = -3;

    fn var(vt: windows::Win32::System::Variant::VARENUM, set: impl FnOnce(&mut VARIANT_0_0)) -> VARIANT {
        let mut v = VARIANT::default();
        unsafe {
            let inner = &mut *v.Anonymous.Anonymous;
            inner.vt = vt;
            set(inner);
        }
        v
    }
    fn v_bool(b: bool) -> VARIANT {
        var(VT_BOOL, |i| i.Anonymous.boolVal = windows::Win32::Foundation::VARIANT_BOOL(if b { -1 } else { 0 }))
    }
    fn v_i4(n: i32) -> VARIANT {
        var(VT_I4, |i| i.Anonymous.lVal = n)
    }
    fn v_str(s: &str) -> VARIANT {
        var(VT_BSTR, |i| i.Anonymous.bstrVal = std::mem::ManuallyDrop::new(BSTR::from(s)))
    }

    fn invoke(d: &IDispatch, name: &str, flags: DISPATCH_FLAGS, arg: Option<VARIANT>) -> Result<VARIANT> {
        let w = wide(name);
        let names = [PCWSTR(w.as_ptr())];
        let mut id = 0i32;
        unsafe { d.GetIDsOfNames(&GUID::zeroed(), names.as_ptr(), 1, LOCALE_USER_DEFAULT, &mut id) }
            .with_context(|| format!("RDP 컨트롤에 '{name}' 이 없습니다"))?;
        let mut args: Vec<VARIANT> = arg.into_iter().collect();
        let mut named = DISPID_PROPERTYPUT;
        let put = flags == DISPATCH_PROPERTYPUT;
        let params = DISPPARAMS {
            rgvarg: if args.is_empty() { std::ptr::null_mut() } else { args.as_mut_ptr() },
            rgdispidNamedArgs: if put { &mut named } else { std::ptr::null_mut() },
            cArgs: args.len() as u32,
            cNamedArgs: put as u32,
        };
        let mut out = VARIANT::default();
        let r = unsafe { d.Invoke(id, &GUID::zeroed(), LOCALE_USER_DEFAULT, flags, &params, Some(&mut out), None, None) };
        for a in &mut args {
            unsafe {
                let _ = VariantClear(a);
            }
        }
        r.with_context(|| format!("RDP 컨트롤 '{name}' 호출 실패"))?;
        Ok(out)
    }

    /// The Connected property: 0 = disconnected, 1 = connected, 2 = connecting.
    fn connected(d: &IDispatch) -> i32 {
        let Ok(mut v) = invoke(d, "Connected", DISPATCH_PROPERTYGET, None) else { return 0 };
        let n = unsafe {
            let i = &v.Anonymous.Anonymous;
            if i.vt == VT_I2 { i.Anonymous.iVal as i32 } else if i.vt == VT_I4 { i.Anonymous.lVal } else { 0 }
        };
        unsafe {
            let _ = VariantClear(&mut v);
        }
        n
    }

    fn pump() {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    type AtlAxWinInit = unsafe extern "system" fn() -> i32;
    type AtlAxGetControl = unsafe extern "system" fn(isize, *mut *mut core::ffi::c_void) -> HRESULT;

    /// Connects and pumps messages while `alive` holds. Reports the connection result once via
    /// `tx`.
    pub(super) fn run(alive: &AtomicBool, tx: &mut Option<tokio::sync::oneshot::Sender<Result<()>>>) -> Result<()> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok().context("COM 초기화 실패")?;
        let atl = unsafe { LoadLibraryW(wide("atl.dll").as_ptr()) };
        if atl.is_null() {
            bail!("atl.dll 을 불러오지 못했습니다");
        }
        let (init, get) = unsafe {
            let i = GetProcAddress(atl, c"AtlAxWinInit".as_ptr() as _).ok_or_else(|| anyhow!("AtlAxWinInit 없음"))?;
            let g = GetProcAddress(atl, c"AtlAxGetControl".as_ptr() as _).ok_or_else(|| anyhow!("AtlAxGetControl 없음"))?;
            (std::mem::transmute::<_, AtlAxWinInit>(i), std::mem::transmute::<_, AtlAxGetControl>(g))
        };
        if unsafe { init() } == 0 {
            bail!("AtlAxWinInit 실패");
        }
        // left of every monitor: invisible to the user, but not minimized, so RDP keeps updating the
        // screen
        let (vx, vy, _, _) = virtual_screen();
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                wide("AtlAxWin").as_ptr(),
                wide("MsTscAx.MsTscAx").as_ptr(),
                WS_POPUP,
                vx - DESK_W - 200,
                vy,
                DESK_W,
                DESK_H,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            bail!("RDP 컨트롤 창을 만들지 못했습니다 ({})", std::io::Error::last_os_error());
        }
        let result = (|| -> Result<()> {
            unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
            let mut raw = std::ptr::null_mut();
            unsafe { get(hwnd as isize, &mut raw) }.ok().context("RDP 컨트롤을 얻지 못했습니다")?;
            let unk = unsafe { IUnknown::from_raw(raw) };
            let disp: IDispatch = unk.cast().context("RDP 컨트롤 IDispatch")?;
            invoke(&disp, "Server", DISPATCH_PROPERTYPUT, Some(v_str("localhost")))?;
            invoke(&disp, "DesktopWidth", DISPATCH_PROPERTYPUT, Some(v_i4(DESK_W)))?;
            invoke(&disp, "DesktopHeight", DISPATCH_PROPERTYPUT, Some(v_i4(DESK_H)))?;
            put_extended(&unk, "ConnectToChildSession", &v_bool(true)).context("ConnectToChildSession 설정 실패")?;
            invoke(&disp, "Connect", DISPATCH_METHOD, None)?;

            let start = Instant::now();
            let mut was_up = false;
            let mut last_poll = Instant::now() - Duration::from_secs(1);
            while alive.load(Ordering::SeqCst) {
                pump();
                if last_poll.elapsed() >= Duration::from_millis(500) {
                    last_poll = Instant::now();
                    let c = connected(&disp);
                    let child = child_session_id();
                    if c == 1 && !was_up {
                        if let Some(id) = child {
                            let _ = std::fs::create_dir_all(crate::platform::paths::data_root());
                            std::fs::write(marker_path(), id.to_string()).context("자식 세션 표시 파일 쓰기 실패")?;
                            was_up = true;
                            if let Some(tx) = tx.take() {
                                let _ = tx.send(Ok(()));
                            }
                        }
                    }
                    if was_up && c == 0 {
                        bail!("자식 세션 연결이 끊겼습니다");
                    }
                    if !was_up && (c == 0 && start.elapsed() > Duration::from_secs(5) || start.elapsed() > CONNECT_WAIT) {
                        bail!("자식 세션에 연결하지 못했습니다 (상태 {c}). 자식 세션 기능이 켜져 있는지, 이 계정의 로그인 방식이 자식 세션 자동 로그온을 지원하는지 확인하세요.");
                    }
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            let _ = invoke(&disp, "Disconnect", DISPATCH_METHOD, None);
            Ok(())
        })();
        unsafe { DestroyWindow(hwnd) };
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_runs_only_in_marked_child_session() {
        assert!(helper_allowed(None, 3, 1).is_err());
        assert!(helper_allowed(Some(3), 1, 1).is_err(), "콘솔(사용자 화면)에서는 거부");
        assert!(helper_allowed(Some(3), 3, 3).is_err(), "표시 id 가 맞아도 콘솔이면 거부");
        assert!(helper_allowed(Some(4), 3, 1).is_err(), "다른 세션 거부");
        assert!(helper_allowed(Some(3), 3, 1).is_ok());
    }

    #[test]
    fn mods_and_endpoint() {
        assert_eq!(mod_vks(MOD_CTRL | MOD_SHIFT), vec![0x11, 0x10]);
        assert!(mod_vks(0).is_empty());
        assert!(endpoint().display().starts_with(r"\\.\pipe\GhostDesk-child-"));
    }
}
