//! 숨은 데스크톱 — 같은 로그온 세션 안에 사용자에게 보이지 않는 데스크톱을 하나 더 만들어 앱을 거기서 띄운다.
//!
//! - `CreateDesktopW` 로 만든 데스크톱은 입력 데스크톱(사용자가 보는 화면)이 아니므로, 그 위의 창·대화상자·메뉴·
//!   팝업은 사용자 화면에 절대 나타나지 않고, 사용자의 마우스·키보드 포커스를 가져가지도 못한다.
//! - 앱은 `STARTUPINFOW.lpDesktop = "WinSta0\\<이름>"` 으로 띄운다. 자식 프로세스도 같은 데스크톱을 물려받는다.
//! - 캡처는 `PrintWindow(PW_RENDERFULLCONTENT)` 만 쓴다. 측정(Win11 26200, AMD iGPU):
//!   - D3D11 flip 모델, D3D9Ex FLIPEX, Chrome/Edge(ANGLE D3D11 + DirectComposition, WebGL), GDI 앱은 실시간으로 찍힌다.
//!   - DXGI blt 모델(DISCARD), 레거시 D3D9, WPF 하드웨어 렌더링은 프레임이 버려진다 → 화면 밖 호스트로 다시 띄운다.
//!   - `PrintWindow(0)`·`GetDC(hwnd)+BitBlt` 는 숨은 데스크톱 창에 대해 **사용자의 실제 화면**을 돌려주므로 쓰면 안 된다.
//!   - Windows.Graphics.Capture 는 숨은 데스크톱 창에서 실패한다.
//! - `EnumWindows`·`FindWindow` 는 호출한 스레드의 데스크톱만 본다. 그래서 창을 다루는 작업은 [`DeskThread`]
//!   (SetThreadDesktop 으로 숨은 데스크톱에 붙은 전용 스레드)에서 돌린다. 메시지 보내기·캡처는 어느 스레드에서도 된다.
//! - `SwitchDesktop` 은 절대 부르지 않는다 (사용자 화면이 바뀐다). Job 의 `UILIMIT_DESKTOP` 이 앱도 못 하게 막는다.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::ThreadId;

use anyhow::{anyhow, bail, Result};
use parking_lot::Mutex;
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, EnumDesktopWindows, OpenDesktopW, SetThreadDesktop, DESKTOP_ENUMERATE,
    DESKTOP_READOBJECTS, HDESK,
};

use super::w32::Hwnd;
use crate::platform::win as pw;

const GENERIC_ALL: u32 = 0x1000_0000;

/// 우리가 만든 숨은 데스크톱 (버리면 CloseDesktop — 그 위의 앱이 모두 끝난 뒤에 버려야 한다)
pub struct HiddenDesktop {
    h: HDESK,
    name: String,
}

// 데스크톱 핸들은 프로세스 전역 커널 객체 핸들이라 스레드 사이에서 옮겨도 안전하다
unsafe impl Send for HiddenDesktop {}
unsafe impl Sync for HiddenDesktop {}

impl HiddenDesktop {
    /// "GhostDesk-<pid>-<n>" 이름으로 새 데스크톱을 만든다 (해상도는 현재 디스플레이와 같다)
    pub fn create() -> Result<HiddenDesktop> {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let name = format!("GhostDesk-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::SeqCst));
        let w = pw::wide(&name);
        let h = unsafe { CreateDesktopW(w.as_ptr(), std::ptr::null(), std::ptr::null(), 0, GENERIC_ALL, std::ptr::null()) };
        if h.is_null() {
            bail!("숨은 데스크톱을 만들지 못했습니다: {}", std::io::Error::last_os_error());
        }
        Ok(HiddenDesktop { h, name })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// STARTUPINFOW.lpDesktop 에 넣을 "WinSta0\<이름>" (끝에 NUL)
    pub fn startup_path(&self) -> Vec<u16> {
        pw::wide(&format!("WinSta0\\{}", self.name))
    }

    /// 이 데스크톱의 최상위 창 (z 순서, 위가 먼저). 어느 스레드에서도 부를 수 있다.
    pub fn windows(&self) -> Vec<Hwnd> {
        enum_desktop(self.h)
    }
}

impl Drop for HiddenDesktop {
    fn drop(&mut self) {
        unsafe {
            CloseDesktop(self.h);
        }
    }
}

unsafe extern "system" fn enum_cb(h: HWND, lp: LPARAM) -> windows_sys::core::BOOL {
    let v = &mut *(lp as *mut Vec<Hwnd>);
    v.push(h as Hwnd);
    1
}

fn enum_desktop(h: HDESK) -> Vec<Hwnd> {
    let mut v: Vec<Hwnd> = Vec::new();
    unsafe {
        EnumDesktopWindows(h, Some(enum_cb), &mut v as *mut Vec<Hwnd> as LPARAM);
    }
    v
}

/// 사용자 데스크톱(Default)에 있는, 주어진 프로세스들의 창 — 숨은 데스크톱에서 띄운 앱이 사용자 화면으로
/// "새어 나왔는지" 확인하는 안전 검사용 (읽기 전용으로 연다).
pub fn windows_on_user_desktop(pids: &[u32]) -> Vec<Hwnd> {
    let name = pw::wide("Default");
    unsafe {
        let h = OpenDesktopW(name.as_ptr(), 0, 0, DESKTOP_ENUMERATE | DESKTOP_READOBJECTS);
        if h.is_null() {
            return vec![];
        }
        let v = enum_desktop(h).into_iter().filter(|&w| pids.contains(&pw::window_pid(w as HWND))).collect();
        CloseDesktop(h);
        v
    }
}

type Job = Box<dyn FnOnce() + Send + 'static>;

/// 숨은 데스크톱에 붙은 전용 작업 스레드. 창 열거·캡처·입력·UI Automation 을 모두 여기서 돌린다.
pub struct DeskThread {
    tx: Mutex<Option<mpsc::Sender<Job>>>,
    id: ThreadId,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl DeskThread {
    pub fn spawn(desk: &HiddenDesktop) -> Result<DeskThread> {
        let h = desk.h as usize;
        let (tx, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let handle = std::thread::Builder::new()
            .name(format!("ghostdesk-{}", desk.name))
            .spawn(move || {
                // SetThreadDesktop 은 창·훅이 없는 새 스레드에서만 성공한다
                let ok = unsafe { SetThreadDesktop(h as HDESK) } != 0;
                if !ok {
                    let _ = ready_tx.send(Err(format!("SetThreadDesktop 실패 (오류 {})", unsafe { GetLastError() })));
                    return;
                }
                set_thread_dpi_aware();
                let _ = ready_tx.send(Ok(()));
                while let Ok(job) = rx.recv() {
                    // 작업이 패닉해도 스레드는 살아 남는다 (호출한 쪽에는 결과 채널이 끊긴 것으로 알려진다)
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                }
            })
            .map_err(|e| anyhow!("작업 스레드를 만들지 못했습니다: {e}"))?;
        let id = handle.thread().id();
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = handle.join();
                bail!("숨은 데스크톱에 작업 스레드를 붙이지 못했습니다: {e}");
            }
            Err(_) => bail!("숨은 데스크톱 작업 스레드가 시작하자마자 끝났습니다"),
        }
        Ok(DeskThread { tx: Mutex::new(Some(tx)), id, handle: Mutex::new(Some(handle)) })
    }

    pub fn is_current(&self) -> bool {
        std::thread::current().id() == self.id
    }

    fn send(&self, job: Job) -> Result<()> {
        match self.tx.lock().as_ref() {
            Some(tx) => tx.send(job).map_err(|_| anyhow!("숨은 데스크톱 작업 스레드가 끝났습니다")),
            None => bail!("숨은 데스크톱 작업 스레드가 끝났습니다"),
        }
    }

    /// f 를 작업 스레드에서 돌리고 결과를 기다린다 (이미 그 스레드면 바로 돌린다)
    pub fn run<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
        if self.is_current() {
            return Ok(f());
        }
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Box::new(move || {
            let _ = tx.send(f());
        }))?;
        rx.recv().map_err(|_| anyhow!("숨은 데스크톱 작업이 실패했습니다 (패닉)"))
    }

    /// f 를 작업 스레드에서 돌리고 비동기로 기다린다
    pub async fn run_async<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
        if self.is_current() {
            return Ok(f());
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.send(Box::new(move || {
            let _ = tx.send(f());
        }))?;
        rx.await.map_err(|_| anyhow!("숨은 데스크톱 작업이 실패했습니다 (패닉)"))
    }

    /// 새 작업을 받지 않고, 쌓인 작업을 마친 뒤 스레드를 끝낸다 (작업 스레드 자신에서 부르면 기다리지 않는다)
    pub fn stop(&self) {
        self.tx.lock().take();
        if let Some(h) = self.handle.lock().take() {
            if !self.is_current() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for DeskThread {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 이 스레드의 DPI 인식을 Per-Monitor v2 로 (창 좌표·캡처를 실제 픽셀로)
fn set_thread_dpi_aware() {
    use windows_sys::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// 숨은 데스크톱 하나와 그 작업 스레드
pub struct DeskHost {
    pub thread: DeskThread,
    // 스레드보다 나중에 버려지도록 뒤에 둔다
    pub desktop: HiddenDesktop,
}

impl DeskHost {
    pub fn create() -> Result<DeskHost> {
        let desktop = HiddenDesktop::create()?;
        let thread = DeskThread::spawn(&desktop)?;
        Ok(DeskHost { thread, desktop })
    }
}

// ───────────────────────── GPU 렌더링 판정 ─────────────────────────

/// 프로세스가 올린 GPU 관련 DLL (d3d9/d3d11/d3d12/dxgi/opengl32/wpfgfx_*)
pub fn gpu_modules(pid: u32) -> Vec<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32,
    };
    let mut out = vec![];
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid);
        if snap == INVALID_HANDLE_VALUE || snap.is_null() {
            return out;
        }
        let mut me: MODULEENTRY32W = std::mem::zeroed();
        me.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
        let mut ok = Module32FirstW(snap, &mut me) != 0;
        while ok {
            let n = me.szModule.iter().position(|&c| c == 0).unwrap_or(me.szModule.len());
            let name = String::from_utf16_lossy(&me.szModule[..n]).to_lowercase();
            if is_gpu_module(&name) {
                out.push(name);
            }
            ok = Module32NextW(snap, &mut me) != 0;
        }
        CloseHandle(snap);
    }
    out
}

pub fn is_gpu_module(name: &str) -> bool {
    matches!(name, "d3d9.dll" | "d3d11.dll" | "d3d12.dll" | "dxgi.dll" | "opengl32.dll" | "d3d10.dll" | "d3d10_1.dll")
        || name.starts_with("wpfgfx_")
}

/// 실행 파일이 Chromium 계열(Chrome·Edge·Brave·Whale·Electron 앱)인가 — 이 앱들은 Job UI 제한이 있으면
/// 렌더러가 뜨지 않는다 (측정: UILIMIT 비트가 하나라도 있으면 빈 창만 남는다).
pub fn is_chromium_family(exe: &std::path::Path) -> bool {
    let name = exe.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    if matches!(name.as_str(), "chrome.exe" | "msedge.exe" | "brave.exe" | "whale.exe" | "vivaldi.exe" | "opera.exe" | "electron.exe") {
        return true;
    }
    // Electron 앱: 실행 파일 옆에 resources\app.asar 또는 resources\app 폴더, 그리고 Chromium 리소스 파일
    let Some(dir) = exe.parent() else { return false };
    let res = dir.join("resources");
    (res.join("app.asar").is_file() || res.join("app").is_dir())
        && (dir.join("chrome_100_percent.pak").is_file() || dir.join("resources.pak").is_file())
}

/// Chromium 계열을 숨은 데스크톱에서 띄울 때 더하는 인자 (가려진 창 최적화를 꺼서 계속 그리게 한다).
/// `--disable-gpu`·`--use-angle=swiftshader`·`--disable-direct-composition` 은 쓰지 않는다 (측정: 빈 화면/멈춘 화면).
pub const CHROMIUM_HIDDEN_ARGS: &[&str] = &[
    "--disable-features=CalculateNativeWinOcclusion",
    "--disable-backgrounding-occluded-windows",
    "--disable-renderer-backgrounding",
    "--disable-background-timer-throttling",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_module_names() {
        assert!(is_gpu_module("d3d11.dll"));
        assert!(is_gpu_module("wpfgfx_cor3.dll"));
        assert!(!is_gpu_module("user32.dll"));
    }

    #[test]
    fn hidden_desktop_isolated_thread() {
        let host = DeskHost::create().expect("desktop");
        assert!(host.desktop.startup_path().len() > 10);
        // 작업 스레드는 숨은 데스크톱에 붙어 있다: 그 스레드의 EnumWindows 는 사용자 데스크톱 창을 보지 않는다
        let n_user = pw::top_level_hwnds().len();
        let n_hidden = host.thread.run(|| pw::top_level_hwnds().len()).unwrap();
        assert!(n_user > 0);
        assert!(n_hidden < n_user, "hidden={n_hidden} user={n_user}");
        assert_eq!(host.thread.run(|| 7).unwrap(), 7);
    }
}
