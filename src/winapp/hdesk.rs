//! Hidden desktop: a second desktop in the same logon session, invisible to the user, where
//! apps are launched.
//!
//! - A desktop made with `CreateDesktopW` isn't the input desktop (the one the user sees), so
//!   its windows, dialogs, menus and popups never appear on the user's screen and can't take the
//!   user's mouse or keyboard focus.
//! - Apps are launched with `STARTUPINFOW.lpDesktop = "WinSta0\\<name>"`. Child processes
//!   inherit the same desktop.
//! - Capture uses `PrintWindow(PW_RENDERFULLCONTENT)` only. Measured (Win11 26200, AMD iGPU):
//!   - D3D11 flip model, D3D9Ex FLIPEX, Chrome/Edge (ANGLE D3D11 + DirectComposition, WebGL)
//!     and GDI apps capture live.
//!   - DXGI blt model (DISCARD), legacy D3D9 and WPF hardware rendering drop frames, so those
//!     are relaunched on the off-screen host.
//!   - `PrintWindow(0)` and `GetDC(hwnd)+BitBlt` return **the user's real screen** for windows
//!     on a hidden desktop. Never use them.
//!   - Windows.Graphics.Capture fails on hidden desktop windows.
//! - `EnumWindows` and `FindWindow` only see the calling thread's desktop, so window work runs
//!   on a [`DeskThread`] (a dedicated thread attached to the hidden desktop with
//!   SetThreadDesktop). Sending messages and capturing work from any thread.
//! - Never call `SwitchDesktop` (it changes the user's screen). The Job's `UILIMIT_DESKTOP`
//!   stops apps from doing it too.

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

/// A hidden desktop we created. Dropping it calls CloseDesktop, so drop it only after every app
/// on it has exited.
pub struct HiddenDesktop {
    h: HDESK,
    name: String,
}

// desktop handles are process-wide kernel object handles, safe to move between threads
unsafe impl Send for HiddenDesktop {}
unsafe impl Sync for HiddenDesktop {}

impl HiddenDesktop {
    /// Creates a new desktop named "GhostDesk-<pid>-<n>" (same resolution as the current display).
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

    /// "WinSta0\<name>" for `STARTUPINFOW.lpDesktop` (NUL-terminated).
    pub fn startup_path(&self) -> Vec<u16> {
        pw::wide(&format!("WinSta0\\{}", self.name))
    }

    /// Top-level windows on this desktop, in z-order (topmost first). Callable from any thread.
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

/// Windows of the given processes on the user's (Default) desktop.
///
/// A safety check for apps launched on the hidden desktop "leaking" onto the user's screen.
/// Opens the desktop read-only.
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

/// A dedicated worker thread attached to the hidden desktop. Window enumeration, capture, input
/// and UI Automation all run here.
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
                // SetThreadDesktop only succeeds on a fresh thread with no windows or hooks
                let ok = unsafe { SetThreadDesktop(h as HDESK) } != 0;
                if !ok {
                    let _ = ready_tx.send(Err(format!("SetThreadDesktop 실패 (오류 {})", unsafe { GetLastError() })));
                    return;
                }
                set_thread_dpi_aware();
                let _ = ready_tx.send(Ok(()));
                while let Ok(job) = rx.recv() {
                    // the thread survives a panicking job (the caller sees its result channel close)
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

    /// Runs `f` on the worker thread and waits for the result (runs it directly if already on that
    /// thread).
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

    /// Runs `f` on the worker thread and awaits the result.
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

    /// Stops accepting jobs, finishes queued ones and ends the thread (doesn't wait when called from
    /// the worker thread itself).
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

/// Makes this thread Per-Monitor v2 DPI aware (window coordinates and capture in real pixels).
fn set_thread_dpi_aware() {
    use windows_sys::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// One hidden desktop and its worker thread.
pub struct DeskHost {
    pub thread: DeskThread,
    // declared after the thread so it's dropped later
    pub desktop: HiddenDesktop,
}

impl DeskHost {
    pub fn create() -> Result<DeskHost> {
        let desktop = HiddenDesktop::create()?;
        let thread = DeskThread::spawn(&desktop)?;
        Ok(DeskHost { thread, desktop })
    }
}

// ---- GPU rendering detection ----

/// GPU-related DLLs loaded by a process (d3d9/d3d11/d3d12/dxgi/opengl32/wpfgfx_*).
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

/// Whether the executable is Chromium-based (Chrome, Edge, Brave, Whale, Electron apps).
///
/// Their renderers don't start under Job UI limits (measured: any UILIMIT bit leaves an empty
/// window).
pub fn is_chromium_family(exe: &std::path::Path) -> bool {
    let name = exe.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    if matches!(name.as_str(), "chrome.exe" | "msedge.exe" | "brave.exe" | "whale.exe" | "vivaldi.exe" | "opera.exe" | "electron.exe") {
        return true;
    }
    // Electron apps: resources\app.asar or a resources\app folder next to the exe, plus Chromium
    // resource files
    let Some(dir) = exe.parent() else { return false };
    let res = dir.join("resources");
    (res.join("app.asar").is_file() || res.join("app").is_dir())
        && (dir.join("chrome_100_percent.pak").is_file() || dir.join("resources.pak").is_file())
}

/// Extra args for Chromium-based apps on the hidden desktop (disables occluded-window
/// optimizations so they keep painting).
///
/// `--disable-gpu`, `--use-angle=swiftshader` and `--disable-direct-composition` are not used
/// (measured: blank or frozen screen).
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
        // the worker thread is attached to the hidden desktop, so its EnumWindows doesn't see user
        // desktop windows
        let n_user = pw::top_level_hwnds().len();
        let n_hidden = host.thread.run(|| pw::top_level_hwnds().len()).unwrap();
        assert!(n_user > 0);
        assert!(n_hidden < n_user, "hidden={n_hidden} user={n_user}");
        assert_eq!(host.thread.run(|| 7).unwrap(), 7);
    }
}
