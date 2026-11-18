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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
/// Includes the time the user may need to type a password into the Windows Security prompt.
const CONNECT_WAIT: Duration = Duration::from_secs(180);
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

### 로그인 창이 뜨면 (Windows 보안)
자식 세션은 지금 계정으로 이 PC 에 **한 번 더 로그인**하는 방식이라, 연결할 때 'Windows 보안 — 자격 증명 입력' 창이 뜰 수 있습니다
(최신 Windows 11 은 이 연결에 네트워크 수준 인증을 씁니다). GhostDesk 는 암호를 대신 넣거나 저장하지 않으니 직접 입력하세요.
1. **사용자 이름**: 지금 계정이 채워져 있으면 그대로 둡니다. 비어 있거나 다르면 이 PC 에 로그인하는 계정을 넣습니다.
   Microsoft 계정이면 `MicrosoftAccount\메일주소`, 로컬 계정이면 계정 이름입니다.
2. **암호**: 계정의 **암호**를 넣습니다. PIN·지문·얼굴 인식(Windows Hello)은 이 창에서 쓸 수 없습니다.
   Microsoft 계정이면 Microsoft 계정 암호입니다.
3. **'내 자격 증명 기억'** 을 체크하면 다음부터는 묻지 않습니다 (Windows 자격 증명 관리자에 저장).
4. 3분 안에 입력하면 됩니다. 시간이 지나거나 창을 닫으면 에이전트를 다시 시작하세요.

### 암호를 모르거나 PIN 으로만 로그인해 왔다면
- 설정 → 계정 → 로그인 옵션의 **'보안 향상을 위해 이 장치에서 Microsoft 계정에 Windows Hello 로그인만 허용'** 이 켜져 있으면
  암호 로그인이 막혀 자식 세션 로그인이 실패할 수 있습니다. 이 옵션을 끈 뒤 다시 시도하세요.
- Microsoft 계정 암호를 잊었다면 account.microsoft.com 에서 다시 정한 뒤, PC 에 한 번 암호로 로그인해 두세요.
- 스마트카드로만 로그인하는 계정(회사 PC 등)은 자식 세션을 쓸 수 없습니다. 웹사이트·Windows 앱 모드나 원격 컴퓨터(VM)를 쓰세요.

### 연결이 안 될 때
- '자식 세션 켜기' 를 했는지(관리자 승인) 확인하세요.
- 로그인 창을 닫았거나 암호가 틀리면 "자식 세션에 연결하지 못했습니다" 가 뜹니다. 에이전트를 다시 시작하면 창이 다시 뜹니다.
- 저장한 암호가 바뀌었다면: 제어판 → 자격 증명 관리자 → Windows 자격 증명에서 localhost(TERMSRV) 항목을 지운 뒤 다시 시도하세요.

### 알아 둘 점
- 같은 Windows 계정이라 설치된 앱·파일을 그대로 씁니다. 단, 이미 내 화면에서 쓰는 앱·브라우저 프로필은 동시에 열지 못할 수 있습니다.
- 자식 세션은 PC 전체에서 하나만 연결됩니다. 내가 로그아웃하면 자식 세션도 끝납니다.
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

fn require_isolated_clipboard(redirected: Option<bool>) -> Result<()> {
    if redirected != Some(false) {
        bail!("자식 세션의 클립보드 격리를 확인하지 못했습니다. 사용자 클립보드를 보호하기 위해 연결하지 않았습니다.");
    }
    Ok(())
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

/// UI element ids belong to one helper connection and one observed foreground window.
#[derive(Default)]
pub(crate) struct AppControl {
    windows: Vec<super::w32::Hwnd>,
    nodes: Vec<super::uia::Node>,
    tree_for: Option<super::w32::Hwnd>,
}

fn process_in_session(pid: u32, session: u32) -> bool {
    let mut found = u32::MAX;
    unsafe { ProcessIdToSessionId(pid, &mut found) != 0 && found == session }
}

fn app_rect_to_frame(r: (i32, i32, i32, i32), screen: (i32, i32), frame: (u32, u32)) -> [i64; 4] {
    [r.0 as i64 * frame.0 as i64 / screen.0 as i64, r.1 as i64 * frame.1 as i64 / screen.1 as i64,
        (r.2 - r.0) as i64 * frame.0 as i64 / screen.0 as i64, (r.3 - r.1) as i64 * frame.1 as i64 / screen.1 as i64]
}

impl AppControl {
    fn run(&mut self, input: &serde_json::Value, session: u32, frame: (u32, u32), check: &dyn Fn() -> Result<()>) -> Result<String> {
        use super::{uia, w32};
        check()?;
        helper_allowed(read_marker(), my_session_id(), unsafe { WTSGetActiveConsoleSessionId() }).map_err(|e| anyhow!(e))?;
        if session != my_session_id() { bail!("앱 요소 조작 대상 세션이 헬퍼와 다릅니다"); }
        uia::com_init();
        let action = input["action"].as_str().unwrap_or("");
        if !matches!(action, "list_windows" | "switch_window" | "ui_tree" | "press" | "set_value" | "read" | "focus" | "select" | "choose") {
            bail!("지원하지 않는 app_control 동작입니다: {action}");
        }
        if action == "list_windows" {
            self.windows = crate::platform::win::top_level_hwnds().into_iter().filter_map(|h| {
                let w = crate::platform::win::window_info(h);
                (w.visible && !w.cloaked && process_in_session(w.pid, session)
                    && !super::guard::is_helper_class(&w.class) && !super::guard::is_shell_window(&w)).then_some(w.hwnd)
            }).collect();
            if self.windows.is_empty() { return Ok("열린 앱 창이 없습니다. 자식 세션에서 앱을 먼저 여세요.".into()); }
            let foreground = w32::root(w32::foreground());
            return Ok(self.windows.iter().enumerate().map(|(i, h)| format!("{} [{i}] '{}' ({}){}", if *h == foreground { "▶" } else { " " },
                w32::window_text(*h), w32::class_name(*h), if w32::is_enabled(*h) { "" } else { " (대화상자가 열려 비활성)" })).collect::<Vec<_>>().join("\n"));
        }
        if action == "switch_window" {
            let index = input["index"].as_u64().ok_or_else(|| anyhow!("index 가 필요합니다"))? as usize;
            let h = *self.windows.get(index).ok_or_else(|| anyhow!("먼저 list_windows 로 창 목록을 확인하세요"))?;
            if !w32::is_window(h) || !process_in_session(w32::thread_pid(h).1, session) || !w32::is_enabled(h) {
                bail!("이 창은 닫혔거나 대화상자가 열려 조작할 수 없습니다. list_windows 를 다시 확인하세요");
            }
            check()?;
            mark_pending_input();
            unsafe {
                if windows_sys::Win32::UI::WindowsAndMessaging::IsIconic(h as _) != 0 {
                    windows_sys::Win32::UI::WindowsAndMessaging::ShowWindowAsync(h as _, windows_sys::Win32::UI::WindowsAndMessaging::SW_RESTORE);
                }
                windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow(h as _);
            }
            self.nodes.clear();
            self.tree_for = None;
            if w32::root(w32::foreground()) != h { bail!("창 전환이 반영되지 않았습니다. 자식 세션 화면에서 창을 클릭하세요"); }
            return Ok(format!("창 '{}' 로 전환했습니다", w32::window_text(h)));
        }
        let top = w32::root(w32::foreground());
        if top == 0 || !process_in_session(w32::thread_pid(top).1, session) {
            bail!("자식 세션의 활성 창을 찾지 못했습니다. 먼저 앱 창을 여세요");
        }
        if action == "ui_tree" {
            let depth = input["max_depth"].as_u64().unwrap_or(30).clamp(1, 80) as usize;
            let nodes = uia::snapshot(top, depth, 500, |_| true)?;
            self.nodes = nodes.iter().map(|(_, n)| n.clone()).collect();
            self.tree_for = Some(top);
            let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN).max(1), GetSystemMetrics(SM_CYSCREEN).max(1)) };
            let (fw, fh) = if frame.0 == 0 || frame.1 == 0 { (sw as u32, sh as u32) } else { frame };
            let lines = nodes.iter().enumerate().map(|(i, (depth, n))| {
                let r = app_rect_to_frame(n.rect, (sw, sh), (fw, fh));
                format!("{}{} {} \"{}\" value=\"{}\" [{},{},{},{}]{}{}", "  ".repeat(depth.saturating_sub(1).min(12)), i + 1,
                    uia::type_name(n.ctype), w32::short(&n.name, 70), w32::short(n.value.as_deref().unwrap_or(""), 80),
                    r[0], r[1], r[2], r[3],
                    if n.enabled { "" } else { " disabled" }, if n.focused { " focused" } else { "" })
            }).collect::<Vec<_>>();
            return Ok(format!("창 '{}' (스크린샷 {fw}x{fh} 좌표):\n{}", w32::window_text(top), lines.join("\n")));
        }
        if self.tree_for != Some(top) { bail!("창이 바뀌었거나 UI 목록이 없습니다. ui_tree 를 다시 호출하세요"); }
        let id = input["id"].as_u64().and_then(|i| usize::try_from(i).ok()).ok_or_else(|| anyhow!("id 가 필요합니다"))?;
        let node = id.checked_sub(1).and_then(|i| self.nodes.get(i)).ok_or_else(|| anyhow!("id {id} 가 없습니다. ui_tree 를 다시 호출하세요"))?;
        if !process_in_session(node.el.process_id()?, session) { bail!("UI 요소가 확인된 자식 세션에 속하지 않습니다"); }
        if action == "read" {
            let value = node.el.value().or_else(|| node.el.document_text()).unwrap_or_default();
            return Ok(format!("{} '{}'\nvalue:\n{value}", uia::type_name(node.ctype), node.el.name()));
        }
        if !w32::is_enabled(top) || !node.el.current_enabled() { bail!("창 또는 요소가 비활성화되어 있습니다. 대화상자를 먼저 확인하세요"); }
        check()?;
        // UIA providers may return before their queued focus/selection changes are visible.
        mark_pending_input();
        match action {
            "press" => {
                if node.toggle.is_some() { node.el.toggle()?; }
                else if node.invoke { node.el.invoke()?; }
                else if node.select { node.el.select()?; }
                else if node.expand { node.el.expand_toggle()?; }
                else { bail!("이 요소는 버튼 동작을 제공하지 않습니다. 자식 세션 화면에서 좌표로 클릭하세요"); }
                Ok(format!("'{}' 요소를 눌렀습니다", node.name))
            }
            "focus" => { node.el.focus_in_child_session(session)?; Ok(format!("'{}' 요소에 포커스를 주었습니다", node.name)) }
            "select" => { node.el.select()?; Ok(format!("'{}' 요소를 선택했습니다", node.name)) }
            "set_value" => {
                let text = input["text"].as_str().ok_or_else(|| anyhow!("text 가 필요합니다"))?;
                let result = node.el.set_value(text);
                let mut actual = node.el.value().or_else(|| node.el.document_text());
                if actual.as_deref() != Some(text) && node.hwnd != 0 && process_in_session(w32::thread_pid(node.hwnd).1, session)
                    && super::workspace::is_edit_class(&w32::class_name(node.hwnd))
                    && w32::style(node.hwnd) & windows_sys::Win32::UI::WindowsAndMessaging::ES_READONLY as u32 == 0 {
                    check()?;
                    if w32::set_control_text(node.hwnd, text) { actual = w32::control_text(node.hwnd); }
                }
                if actual.as_ref().map(|s| s.replace("\r\n", "\n")) == Some(text.replace("\r\n", "\n")) {
                    Ok(format!("'{}' 값 설정을 확인했습니다 ({}자)", node.name, text.chars().count()))
                } else { bail!("값 설정을 확인하지 못했습니다{}", result.err().map(|e| format!(": {e}")).unwrap_or_default()); }
            }
            "choose" => {
                let option = input["option"].as_str().filter(|s| !s.trim().is_empty()).ok_or_else(|| anyhow!("option 이 필요합니다"))?;
                let selected = node.el.choose_in_child_session(option, session)?;
                Ok(format!("'{selected}' 선택을 요청했습니다"))
            }
            _ => bail!("지원하지 않는 app_control 동작입니다: {action}"),
        }
    }
}

static QUEUED_INPUT: AtomicU64 = AtomicU64::new(0);
static OBSERVED_INPUT: AtomicU64 = AtomicU64::new(0);

fn mark_pending_input() { QUEUED_INPUT.fetch_add(1, Ordering::SeqCst); }

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
    let changes_focus_or_selection = inputs.iter().any(|input| {
        input.r#type == INPUT_KEYBOARD || (input.r#type == INPUT_MOUSE && unsafe {
            input.Anonymous.mi.dwFlags & !(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK) != 0
        })
    });
    if changes_focus_or_selection { mark_pending_input(); }
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
    fn text_target(&self, check: &dyn Fn() -> Result<()>) -> Result<super::w32::Hwnd> {
        check()?;
        let target = super::w32::thread_focus(0).map(|(_, h)| h).filter(|h| *h != 0)
            .ok_or_else(|| anyhow!("입력할 텍스트 칸에 포커스가 없습니다. 입력 칸을 먼저 클릭하세요."))?;
        let (_, pid) = super::w32::thread_pid(target);
        let mut target_session = u32::MAX;
        if unsafe { ProcessIdToSessionId(pid, &mut target_session) } == 0 || target_session != self.session {
            bail!("입력 대상이 자식 세션에 속하지 않아 입력을 거부합니다");
        }
        Ok(target)
    }

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
    fn supports_app_control(&self) -> bool { true }
    fn app_control(&self, input: &serde_json::Value, state: &mut crate::session::AppControlState, frame: (u32, u32), check: &dyn Fn() -> Result<()>) -> Result<String> {
        state.windows.run(input, self.session, frame, check)
    }
    fn input_epoch(&self) -> u64 { QUEUED_INPUT.load(Ordering::SeqCst) }
    fn observe_input(&self, epoch: u64) { OBSERVED_INPUT.store(epoch, Ordering::SeqCst); }
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
    fn type_text(&self, text: &str, check: &dyn Fn() -> Result<()>) -> Result<()> {
        let (text, removed) = super::input::sanitize_text(text);
        if removed != 0 {
            bail!("줄바꿈과 탭 이외의 제어 문자는 입력할 수 없습니다");
        }
        if text.is_empty() { return Ok(()); }
        // A synchronous edit must not overtake an earlier queued click or shortcut. Live-view
        // polling does not count as the agent observing the resulting focus and selection.
        if QUEUED_INPUT.load(Ordering::SeqCst) != OBSERVED_INPUT.load(Ordering::SeqCst) {
            return crate::session::type_text_segmented(self, &text, check);
        }
        let target = self.text_target(check)?;
        if super::workspace::is_edit_class(&super::w32::class_name(target)) {
            // Modern Notepad ignores WM_CHAR line breaks; replace the selection as one edit.
            check()?;
            if super::w32::send_str(target, super::w32::EM_REPLACESEL, 1, &text.replace('\n', "\r\n"), 3000).is_none() {
                bail!("앱이 문자 입력에 응답하지 않습니다. 일부만 입력되었을 수 있으니 내용을 확인하세요.");
            }
            return Ok(());
        }
        crate::session::type_text_segmented(self, &text, check)
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
        CoInitializeEx, IDispatch, COINIT_APARTMENTTHREADED, EXCEPINFO, DISPATCH_FLAGS, DISPATCH_METHOD, DISPATCH_PROPERTYGET,
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
        let mut ex = EXCEPINFO::default();
        let r = unsafe { d.Invoke(id, &GUID::zeroed(), LOCALE_USER_DEFAULT, flags, &params, Some(&mut out), Some(&mut ex), None) };
        for a in &mut args {
            unsafe {
                let _ = VariantClear(a);
            }
        }
        // DISP_E_EXCEPTION alone says nothing: the control's own error is in EXCEPINFO
        let (code, desc) = unsafe {
            if let Some(fill) = ex.pfnDeferredFillIn {
                let _ = fill(&mut ex);
            }
            let desc = ex.bstrDescription.to_string();
            for b in [&mut ex.bstrSource, &mut ex.bstrDescription, &mut ex.bstrHelpFile] {
                std::mem::ManuallyDrop::drop(b);
            }
            (ex.scode, desc)
        };
        r.with_context(|| match (code, desc.trim()) {
            (0, "") => format!("RDP 컨트롤 '{name}' 호출 실패"),
            (c, d) => format!("RDP 컨트롤 '{name}' 호출 실패: {d} (0x{c:08X})"),
        })?;
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
            // without CredSSP, recent Windows 11 builds (MSTSCAX 10.0.26100) reject Connect with E_INVALIDARG
            let adv = invoke(&disp, "AdvancedSettings", DISPATCH_PROPERTYGET, None)?;
            let adv = unsafe { (*adv.Anonymous.Anonymous.Anonymous.pdispVal).clone() }.context("RDP 컨트롤 AdvancedSettings 가 없습니다")?;
            invoke(&adv, "EnableCredSspSupport", DISPATCH_PROPERTYPUT, Some(v_bool(true)))?;
            invoke(&adv, "RedirectClipboard", DISPATCH_PROPERTYPUT, Some(v_bool(false))).context("클립보드 격리 설정 실패")?;
            let mut clipboard = invoke(&adv, "RedirectClipboard", DISPATCH_PROPERTYGET, None).context("클립보드 격리 확인 실패")?;
            let redirected = unsafe {
                let value = &clipboard.Anonymous.Anonymous;
                (value.vt == VT_BOOL).then(|| value.Anonymous.boolVal.0 != 0)
            };
            unsafe { let _ = VariantClear(&mut clipboard); }
            require_isolated_clipboard(redirected)?;
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
                        bail!(
                            "자식 세션에 연결하지 못했습니다 (상태 {c}). 'Windows 보안' 로그인 창을 닫았거나 암호가 틀렸다면 다시 시도하세요 — \
                             PIN 이 아니라 계정 암호를 넣어야 합니다. 새 에이전트 → 자식 세션 → '처음 설정 방법 보기' 의 '로그인 창이 뜨면' 을 참고하세요."
                        );
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
    fn semantic_element_rectangles_are_xywh_in_screenshot_pixels() {
        assert_eq!(app_rect_to_frame((100, 50, 300, 150), (1000, 500), (500, 250)), [50, 25, 100, 50]);
        assert_eq!(app_rect_to_frame((-10, -20, 90, 80), (1000, 500), (500, 250)), [-5, -10, 50, 50]);
    }

    #[test]
    fn clipboard_redirection_must_be_confirmed_disabled() {
        assert!(require_isolated_clipboard(Some(false)).is_ok());
        assert!(require_isolated_clipboard(Some(true)).is_err());
        assert!(require_isolated_clipboard(None).is_err());
    }

    #[test]
    fn semantic_app_control_refuses_unverified_sessions_before_enumerating_windows() {
        let mut state = AppControl::default();
        let result = state.run(&serde_json::json!({"action":"list_windows"}), u32::MAX, (1280, 800), &|| Ok(()));
        assert!(result.is_err());
        assert!(state.windows.is_empty());
    }

    #[test]
    #[ignore = "requires the marked child session; creates only a generated test window there"]
    fn semantic_controls_work_inside_child_session() {
        use super::super::{uia, w32};
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        let session = my_session_id();
        helper_allowed(read_marker(), session, unsafe { WTSGetActiveConsoleSessionId() }).unwrap();
        let ui = std::thread::spawn(|| super::super::testapp::run(false, false).unwrap());
        let end = Instant::now() + Duration::from_secs(5);
        let h = loop {
            let h = w32::find_window_class("GhostDeskWinAppTestTarget");
            if h != 0 && w32::thread_pid(h).1 == std::process::id() { break h; }
            assert!(Instant::now() < end, "test window did not start");
            std::thread::sleep(Duration::from_millis(10));
        };
        struct TestWindow(u32, Option<std::thread::JoinHandle<()>>);
        impl Drop for TestWindow {
            fn drop(&mut self) {
                unsafe { PostThreadMessageW(self.0, WM_QUIT, 0, 0); }
                self.1.take().unwrap().join().unwrap();
            }
        }
        let _window = TestWindow(w32::thread_pid(h).0, Some(ui));
        unsafe { ShowWindow(h as _, SW_SHOW); SetForegroundWindow(h as _); }
        let end = Instant::now() + Duration::from_secs(3);
        while w32::root(w32::foreground()) != h && Instant::now() < end {
            unsafe { SetForegroundWindow(h as _); }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(w32::root(w32::foreground()), h);
        let mut state = AppControl::default();
        let request = |action: &str, id: usize| serde_json::json!({"action":action, "id":id});
        let tree = state.run(&serde_json::json!({"action":"ui_tree"}), session, (1280, 800), &|| Ok(())).unwrap();
        println!("generated semantic fixture:\n{tree}");
        let edit = state.nodes.iter().position(|n| matches!(n.ctype, uia::CT_EDIT | uia::CT_DOCUMENT))
            .unwrap_or_else(|| panic!("edit/document element missing: {tree}")) + 1;
        let checkbox = state.nodes.iter().position(|n| n.ctype == uia::CT_CHECKBOX)
            .unwrap_or_else(|| panic!("checkbox element missing: {tree}")) + 1;
        let combo = state.nodes.iter().position(|n| n.ctype == uia::CT_COMBOBOX)
            .unwrap_or_else(|| panic!("combo element missing: {tree}")) + 1;
        let value = "자식 세션 UIA\n둘째 줄 👋";
        state.run(&serde_json::json!({"action":"set_value", "id":edit, "text":value}), session, (1280, 800), &|| Ok(())).unwrap();
        let read = state.run(&request("read", edit), session, (1280, 800), &|| Ok(())).unwrap();
        assert!(read.replace("\r\n", "\n").contains(value), "{read}");
        let platform = ChildPlatform { session };
        let observed = platform.input_epoch();
        platform.observe_input(observed);
        state.run(&request("read", edit), session, (1280, 800), &|| Ok(())).unwrap();
        assert_eq!(platform.input_epoch(), observed, "reading must keep the observed field usable");
        state.run(&request("focus", edit), session, (1280, 800), &|| Ok(())).unwrap();
        assert!(platform.input_epoch() > observed, "semantic focus must invalidate the synchronous edit observation");
        assert_eq!(w32::thread_focus(w32::thread_pid(h).0).unwrap().1, state.nodes[edit - 1].hwnd);
        state.run(&request("press", checkbox), session, (1280, 800), &|| Ok(())).unwrap();
        let checked = || w32::send_timeout(state.nodes[checkbox - 1].hwnd, w32::BM_GETCHECK, 0, 0, 500);
        let end = Instant::now() + Duration::from_secs(2);
        while checked() != Some(1) && Instant::now() < end { std::thread::sleep(Duration::from_millis(10)); }
        assert_eq!(checked(), Some(1), "application must apply the requested checkbox toggle");
        state.run(&serde_json::json!({"action":"choose", "id":combo, "option":"포도"}), session, (1280, 800), &|| Ok(())).unwrap();
        let end = Instant::now() + Duration::from_secs(2);
        while w32::window_text(h) != "선택: 포도" && Instant::now() < end { std::thread::sleep(Duration::from_millis(10)); }
        assert_eq!(w32::window_text(h), "선택: 포도", "application must receive its selection notification");
        unsafe { ShowWindow(h as _, SW_MAXIMIZE); }
        assert!(unsafe { IsZoomed(h as _) } != 0);
        let before_list = platform.input_epoch();
        state.run(&serde_json::json!({"action":"list_windows"}), session, (1280, 800), &|| Ok(())).unwrap();
        assert_eq!(platform.input_epoch(), before_list, "listing must not invalidate observed input");
        let index = state.windows.iter().position(|w| *w == h).unwrap();
        state.run(&serde_json::json!({"action":"switch_window", "index":index}), session, (1280, 800), &|| Ok(())).unwrap();
        assert!(unsafe { IsZoomed(h as _) } != 0, "switching windows must preserve maximized placement");
        assert!(state.run(&request("read", edit), session, (1280, 800), &|| Ok(())).unwrap_err().to_string().contains("ui_tree"));
    }

    /// Run only inside the marked child session: a deliberately slow real EDIT control catches
    /// replies that acknowledge queued input before the application has actually inserted it.
    #[tokio::test]
    #[ignore = "requires the marked Windows child session; never drives the parent desktop"]
    async fn multiline_text_is_complete_when_rpc_returns() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, SetFocus};
        use windows_sys::Win32::UI::WindowsAndMessaging::*;

        static RECEIVED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        static INSERTED: AtomicBool = AtomicBool::new(false);
        static KEYUPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        thread_local! { static ORIGINAL: std::cell::Cell<isize> = const { std::cell::Cell::new(0) }; }
        unsafe extern "system" fn delayed_edit(h: windows_sys::Win32::Foundation::HWND, msg: u32, w: usize, l: isize) -> isize {
            if msg == WM_CHAR {
                std::thread::sleep(Duration::from_millis(2));
                RECEIVED.fetch_add(1, Ordering::SeqCst);
            }
            if msg == WM_KEYDOWN {
                std::thread::sleep(Duration::from_millis(100));
                if w == 0x41 && GetKeyState(0x11) < 0 {
                    SendMessageW(h, super::super::w32::EM_SETSEL, 0, -1);
                    return 0;
                }
            }
            if msg == WM_KEYUP { KEYUPS.fetch_add(1, Ordering::SeqCst); }
            if msg == super::super::w32::EM_REPLACESEL {
                std::thread::sleep(Duration::from_millis(100));
                INSERTED.store(true, Ordering::SeqCst);
            }
            let proc: WNDPROC = std::mem::transmute(ORIGINAL.with(|p| p.get()));
            CallWindowProcW(proc, h, msg, w, l)
        }

        let me = my_session_id();
        helper_allowed(read_marker(), me, unsafe { WTSGetActiveConsoleSessionId() }).unwrap();
        let (ready, window) = std::sync::mpsc::channel();
        let ui = std::thread::spawn(move || unsafe {
            let h = CreateWindowExW(
                0, wide("EDIT").as_ptr(), wide("").as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE | WS_VSCROLL | ES_MULTILINE as u32 | ES_WANTRETURN as u32 | ES_AUTOVSCROLL as u32,
                30, 30, 900, 600, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(),
            );
            assert!(!h.is_null());
            ORIGINAL.with(|p| p.set(SetWindowLongPtrW(h, GWLP_WNDPROC, delayed_edit as *const () as isize)));
            SetForegroundWindow(h);
            SetFocus(h);
            ready.send(h as usize).unwrap();
            let mut msg: MSG = std::mem::zeroed();
            while IsWindow(h) != 0 && GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
        let h = window.recv_timeout(Duration::from_secs(5)).unwrap();
        struct Window(usize, Option<std::thread::JoinHandle<()>>);
        impl Drop for Window {
            fn drop(&mut self) {
                unsafe { PostMessageW(self.0 as _, WM_CLOSE, 0, 0) };
                if let Some(t) = self.1.take() { t.join().unwrap(); }
            }
        }
        let _window = Window(h, Some(ui));
        assert_eq!(unsafe { GetForegroundWindow() } as usize, h, "test window did not receive foreground focus");

        let ep = Endpoint::custom(&format!("ghostdesk-child-input-test-{}", std::process::id()));
        let cfg = ServerConfig { allow_uids: vec![], allow_on_console: false, label: "input-test".into(), log: Arc::new(|_| {}) };
        let srv = crate::session::spawn_server(
            crate::session::HelperListener::bind(&ep).await.unwrap(), Arc::new(ChildPlatform { session: me }), cfg,
        );
        let (rd, mut wr) = crate::platform::ipc::connect(&ep).await.unwrap();
        let mut rd = BufReader::new(rd);
        let text = format!("{}끝👋", "재용머니 파산 👋\r\n둘째\t줄\n\n".repeat(130));
        let request = serde_json::json!({"id":1,"op":"type","text":text});
        wr.write_all(format!("{request}\n").as_bytes()).await.unwrap();
        let mut response = String::new();
        rd.read_line(&mut response).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["ok"], true, "{response}");
        let actual = super::super::w32::control_text(h as _).unwrap();
        // Keep focus on this test window until old queued input is drained, even on a red run.
        let expected = text.replace("\r\n", "\n");
        let end = Instant::now() + Duration::from_secs(30);
        while !INSERTED.load(Ordering::SeqCst) && RECEIVED.load(Ordering::SeqCst) < expected.encode_utf16().count() && Instant::now() < end {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let actual = actual.replace("\r\n", "\n");
        assert!(actual == expected, "text was incomplete or reordered at RPC completion: actual {} chars, expected {}", actual.chars().count(), expected.chars().count());

        for request in [serde_json::json!({"id":2,"op":"key","combo":"ctrl+a"}), serde_json::json!({"id":3,"op":"type","text":"새 글"})] {
            wr.write_all(format!("{request}\n").as_bytes()).await.unwrap();
            let mut line = String::new();
            rd.read_line(&mut line).await.unwrap();
            let response: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(response["ok"], true, "{response}");
        }
        let mut replaced = super::super::w32::control_text(h as _).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while (KEYUPS.load(Ordering::SeqCst) < 2 || replaced != "새 글") && Instant::now() < end {
            tokio::time::sleep(Duration::from_millis(10)).await;
            replaced = super::super::w32::control_text(h as _).unwrap();
        }
        assert_eq!(replaced, "새 글", "queued Ctrl+A must be applied before typing replacement text");
        for request in [
            serde_json::json!({"id":4,"op":"screenshot","max_long_edge":8,"format":"png","observe_input":true}),
            serde_json::json!({"id":5,"op":"pointer","action":"move","x":30,"y":30,"fw":1280,"fh":800}),
            serde_json::json!({"id":6,"op":"type","text":" 확인"}),
        ] {
            wr.write_all(format!("{request}\n").as_bytes()).await.unwrap();
            let mut line = String::new();
            rd.read_line(&mut line).await.unwrap();
            let response: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(response["ok"], true, "{response}");
        }
        let after_observation = super::super::w32::control_text(h as _).unwrap();
        wr.shutdown().await.unwrap();
        srv.stop().await;
        assert_eq!(after_observation, "새 글 확인", "observed typing must stay synchronous after a canary mouse move");
    }

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

