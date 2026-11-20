//! GhostDesk: a desktop app that runs one or more computer-use agents (Claude, GPT, Gemini, …)
//! side by side on isolated screens (browsers with copied logins, hidden apps, separate sessions,
//! remote desktops) without ever touching the user's screen, mouse, keyboard or browser windows.
//!
//! It's a console-subsystem executable on Windows too, so the MCP host (stdio) and command-line
//! modes like `--connect` and `--selftest*` get standard I/O and exit codes as is. When run as a
//! GUI, a console window created just for this process (double-click in Explorer) is detached
//! right away.

mod agent;
mod agent_account;
mod app;
mod browser;
mod browser_dom;
mod cdp;
mod cleanup;
mod control;
mod mcp;
mod toolbox;
mod cursor;
mod icon;
mod install;
mod kit;
mod desk;
mod electron;
#[cfg(target_os = "macos")]
mod macos;
mod native;
mod platform;
#[cfg(windows)]
mod winapp;
// macOS: dedicated account session; Windows: child session (PiP). Same helper RPC.
#[cfg(any(unix, windows))]
mod host;
mod session;
mod vdisplay;
mod keys;
mod licenses;
mod llm;
mod profiles;
mod rfb;
mod ssh;
mod cli;
mod pi;
mod selftest;
mod team;
mod turns;
mod update;
mod usage;

fn main() -> anyhow::Result<()> {
    // DLL planting defense: DLLs loaded by name at runtime (graphics stack: opengl32, d3d12, dxgi,
    // d3dcompiler_47, …) are only searched for in System32, not the executable's folder (possibly
    // Downloads), the current folder or PATH. Static imports are covered by /DEPENDENTLOADFLAG in
    // build.rs. Must come first.
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::LibraryLoader::{SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32};
        SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32);
    }
    // keep the stdio pipes the MCP host gave us from leaking into the GhostDesk window and browsers
    // (all modes)
    #[cfg(windows)]
    platform::win::harden_std_handles();
    let args: Vec<String> = std::env::args().collect();
    // an external agent launched an extra GhostDesk for testing (control.rs app_target): use an
    // empty data folder (separate settings, keys and control endpoint) and don't auto-connect to or
    // install into other AI apps. No other threads exist yet, so changing env vars is safe.
    if let Some(dir) = args.iter().position(|a| a == control::ISOLATED_DATA_ARG).and_then(|i| args.get(i + 1)) {
        std::env::set_var("GHOSTDESK_DATA_DIR", dir);
        std::env::set_var("GHOSTDESK_NO_AUTOCONNECT", "1");
    }
    let rt = tokio::runtime::Runtime::new()?;
    if args.get(1).map(String::as_str) == Some("--selftest") {
        return rt.block_on(selftest::run(args.get(2).cloned()));
    }
    match args.get(1).map(String::as_str) {
        Some("--selftest-electron") => return rt.block_on(electron::selftest(&args[2..])),
        Some("--selftest-native") => return rt.block_on(native::selftest(&args[2..])),
        // test that creates a real virtual monitor: only runs with --yes
        Some("--selftest-vdisplay") => return rt.block_on(vdisplay::selftest(&args[2..])),
        Some("--selftest-session") => return rt.block_on(session::selftest(&args[2..])),
        Some("--host") => return rt.block_on(host::run()),
        Some("--session-helper") => return rt.block_on(session::run_helper(&args[2..])),
        #[cfg(unix)]
        Some("--print-session-plist") => return session::print_session_plist(&args[2..]),
        // running elevated (UAC): enable the Windows child sessions feature
        #[cfg(windows)]
        Some("--enable-child-sessions") => return winapp::child::enable_cli(),
        // MCP server for external agents (Claude app / Claude Code / Codex), over stdio
        Some("--mcp") => return rt.block_on(mcp::run(&args[2..])),
        // install + auto-connect / disconnect Claude Code, the Claude app and Codex
        Some("--connect") => return install::cli(&args[2..], true),
        Some("--disconnect") => return install::cli(&args[2..], false),
        // build a connection kit (MCP config, skill, plugin) for agents not in the list
        Some("--export-kit") => return kit::cli(&args[2..]),
        // build the macOS app bundle (zip) for releases (.github/workflows/release.yml)
        #[cfg(target_os = "macos")]
        Some("--package-macos") => return install::package_macos(&args[2..]),
        _ => {}
    }
    if args.get(1).map(String::as_str) == Some("--agent-test") {
        return rt.block_on(selftest::run_agent_cli(args.get(2).cloned().unwrap_or_default()));
    }

    // ---- GUI ----
    // Windows: if a console was created just for this process (launched from Explorer on older
    // Windows), relaunch without a console and exit.
    #[cfg(windows)]
    if relaunch_without_console(&args) {
        return Ok(());
    }
    // Windows: one window per user. If one is already up, bring it to the front and exit (on macOS
    // LaunchServices does this). With `--background` (not started by the user, e.g. auto-launched by
    // the MCP server) focus isn't taken.
    #[cfg(windows)]
    let background = args.iter().skip(1).any(|a| a == "--background");
    // Windows: when a freshly downloaded exe is run, close the old GhostDesk, replace the installed
    // copy with this one, and launch the installed copy
    // Windows: the setup exe (or `--uninstall`) opens the installer window instead of the app
    #[cfg(windows)]
    if let Some(mode) = app::setup::mode(&args) {
        return app::setup::run(mode);
    }
    #[cfg(windows)]
    app::setup::refresh_registration();
    #[cfg(windows)]
    if !background && install::take_over_install() {
        return Ok(());
    }
    #[cfg(windows)]
    let _single = match gui_instance::acquire(!background) {
        gui_instance::Start::Proceed(guard) => guard,
        gui_instance::Start::Exit => return Ok(()),
    };
    // close a console window created just for this process (double-click in Explorer, …); leave it if
    // started from a terminal
    #[cfg(windows)]
    detach_console_for_gui();
    // put back user app windows left off-screen by a crashed GhostDesk ("attach to a running app")
    #[cfg(windows)]
    std::thread::spawn(native::recover_parked_windows);
    // a child-session helper start-up script left behind by a crash would run at the next logon
    #[cfg(windows)]
    winapp::child::forget_helper_start();

    // macOS: if the new version was opened right where it was unpacked, replace the old installed
    // copy with it and reopen from the install location
    #[cfg(target_os = "macos")]
    if install::take_over_install() {
        return Ok(());
    }

    let handle = rt.handle().clone();
    #[allow(unused_mut)]
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("GhostDesk")
        .with_icon(app::window_icon())
        .with_inner_size(initial_window_size())
        .with_min_inner_size([900.0, 600.0]);
    #[cfg(windows)]
    if background {
        viewport = viewport.with_active(false);
    }
    // Windows: the app draws its own title bar instead of the native frame (app/titlebar.rs). The
    // title stays for the taskbar and Alt+Tab.
    #[cfg(windows)]
    {
        viewport = viewport.with_decorations(false);
    }
    // macOS: same look as Windows, with the content extended under the title bar and the native
    // title bar hidden transparently. The traffic lights, resizing and full screen stay native.
    #[cfg(target_os = "macos")]
    {
        viewport = viewport.with_fullsize_content_view(true).with_titlebar_shown(false).with_title_shown(false);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "GhostDesk",
        options,
        Box::new(move |cc| Ok(Box::new(app::GhostApp::new(cc, handle)))),
    )
    .map_err(|e| anyhow::anyhow!("GUI 실행 실패: {e}"))?;
    // drop(rt) waits forever for running spawn_blocking tasks (profile cloning, installs, …). Once
    // the window is closed, wait briefly and exit (an interrupted clone is rebuilt next run).
    rt.shutdown_timeout(std::time::Duration::from_secs(3));
    // "Restart" after an update: release the single-instance mutex first so the new window shows up
    // right away
    #[cfg(windows)]
    drop(_single);
    update::relaunch_if_requested();
    Ok(())
}

/// Returns true if the GUI was relaunched without a console (this process exits right away).
///
/// On Windows 11 24H2 and later the executable manifest (consoleAllocationPolicy=detached) means
/// no console is created when launched from Explorer, so this never triggers. On earlier
/// versions, if this process has a console to itself, FreeConsole alone leaves an empty tab until
/// the GUI exits when Windows Terminal is the default terminal. So we relaunch ourselves with the
/// same args without a console (DETACHED_PROCESS) and exit, which closes the console (tab) too.
#[cfg(windows)]
fn relaunch_without_console(args: &[String]) -> bool {
    use std::os::windows::process::CommandExt;
    const GUARD: &str = "GHOSTDESK_GUI_RELAUNCHED";
    if std::env::var_os(GUARD).is_some() {
        // the relaunched side: just clear the marker and continue (detach_console_for_gui below handles
        // any remaining console)
        std::env::remove_var(GUARD);
        return false;
    }
    if !platform::win::owns_console_alone() {
        return false;
    }
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let Ok(exe) = std::env::current_exe() else { return false };
    std::process::Command::new(exe)
        .args(args.iter().skip(1))
        .env(GUARD, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .is_ok()
}

/// Detaches a console window created just for this process when running as a GUI
/// (`platform::win::detach_own_console`). Usually `relaunch_without_console` handles it first;
/// this is the fallback when relaunching fails.
///
/// Detaching leaves the std handles that pointed at the console as closed handle values. Left
/// alone,
/// - `Command::spawn` with default (inherited) stdio fails with ERROR_INVALID_HANDLE, and
/// - if another kernel object (file, pipe) later gets the same handle value, `println!` /
///   `eprintln!` would write into it.
///
/// So the std handles that were the console are set to NULL. Rust std treats NULL std handles as
/// absent (output is discarded, children get none). Handles passed in as pipes or files are left
/// alone.
#[cfg(windows)]
fn detach_console_for_gui() {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetConsoleWindow, GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    let ids = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    unsafe {
        if GetConsoleWindow().is_null() {
            return; // no console window (launched by MCP and others with DETACHED_PROCESS / CREATE_NO_WINDOW)
        }
        let was_console = ids.map(|id| {
            let h = GetStdHandle(id);
            let mut mode = 0u32;
            !h.is_null() && GetConsoleMode(h, &mut mode) != 0
        });
        platform::win::detach_own_console();
        if !GetConsoleWindow().is_null() {
            return; // started from a terminal and sharing its console: leave it
        }
        for (id, console) in ids.into_iter().zip(was_console) {
            if console {
                SetStdHandle(id, std::ptr::null_mut());
            }
        }
    }
}

/// Initial window size in logical units, 1500x920 by default.
///
/// On Windows 150% scaling is common on laptops (1920x1080 → 1280x720 logical), so the default
/// can exceed the screen; it's shrunk to fit the primary monitor's work area (excluding the
/// taskbar).
fn initial_window_size() -> [f32; 2] {
    const DEFAULT: [f32; 2] = [1500.0, 920.0];
    #[cfg(windows)]
    {
        if let Some(work) = windows_work_area_points() {
            return fit_window(DEFAULT, work);
        }
    }
    DEFAULT
}

/// Shrinks `want` to fit the work area `work` (logical units), leaving room for the window frame
/// and title bar, but never below the minimum size (900x600).
#[cfg_attr(not(windows), allow(dead_code))]
fn fit_window(want: [f32; 2], work: [f32; 2]) -> [f32; 2] {
    let w = want[0].min(work[0] - 40.0).max(900.0);
    let h = want[1].min(work[1] - 80.0).max(600.0);
    [w, h]
}

/// Size of the primary monitor's work area (logical = physical pixels / scale).
#[cfg(windows)]
fn windows_work_area_points() -> Option<[f32; 2]> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETWORKAREA};
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut r as *mut RECT as *mut _, 0) } != 0;
    // for a DPI-unaware process GetDpiForSystem is 96 and the work area is already scaled
    let dpi = unsafe { GetDpiForSystem() }.max(96) as f32;
    let (w, h) = ((r.right - r.left) as f32, (r.bottom - r.top) as f32);
    (ok && w > 0.0 && h > 0.0).then(|| [w * 96.0 / dpi, h * 96.0 / dpi])
}

/// Windows GUI single instance: a named mutex per user (SID).
#[cfg(windows)]
mod gui_instance {
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, HANDLE, HWND};

    /// Mutex handle. Must be held until the process exits so other launches know one is running.
    pub struct Guard(HANDLE);

    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    pub enum Start {
        /// First window: keep running (`None` if the mutex couldn't be created; run anyway).
        Proceed(Option<Guard>),
        /// A window is already up: this process exits.
        Exit,
    }

    /// A per-user name within the session (Local\ = the current logon session).
    pub fn mutex_name(sid: Option<&str>) -> String {
        match sid {
            Some(s) if !s.is_empty() => format!("Local\\GhostDesk-GUI-{s}"),
            _ => "Local\\GhostDesk-GUI".to_string(),
        }
    }

    /// Creates the mutex: `Ok((handle, already existed))`, or `Err(Win32 error code)`.
    pub fn create(name: &str) -> Result<(Guard, bool), u32> {
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let w = crate::platform::win::wide(name);
        unsafe {
            let h = CreateMutexW(std::ptr::null(), 0, w.as_ptr());
            let err = GetLastError();
            if h.is_null() {
                Err(err)
            } else {
                Ok((Guard(h), err == ERROR_ALREADY_EXISTS))
            }
        }
    }

    /// The "GhostDesk" top-level window of a process with this executable's name (excluding
    /// ourselves).
    fn existing_window() -> Option<crate::platform::win::TopWindow> {
        use crate::platform::win;
        let me = std::process::id();
        let own_name = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| crate::platform::paths::EXE_NAME.to_string());
        win::top_level_hwnds()
            .into_iter()
            .map(win::window_info)
            .filter(|w| w.pid != me && w.title == "GhostDesk")
            .find(|w| {
                // leave other windows with the same title alone (e.g. Explorer showing a 'GhostDesk' folder)
                win::process_path(w.pid)
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .is_some_and(|n| {
                        n.eq_ignore_ascii_case(&own_name) || n.eq_ignore_ascii_case(crate::platform::paths::EXE_NAME)
                    })
            })
    }

    /// Only called when the user launched it again themselves: restores it if minimized and brings
    /// it to the front.
    fn bring_to_front(hwnd: HWND) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW};
        unsafe {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            } else {
                ShowWindow(hwnd, SW_SHOW);
            }
            SetForegroundWindow(hwnd);
        }
    }

    /// `Proceed` for the first window. If one already exists, brings it to the front (with
    /// `activate`) and returns `Exit`.
    ///
    /// A previous launch may still be opening or closing its window, so wait a little (up to 10 s):
    /// if the previous process exits and the mutex goes away, this one becomes the first.
    pub fn acquire(activate: bool) -> Start {
        let sid = crate::platform::win::current_user_sid().ok();
        let scope = crate::platform::ipc::instance_suffix(std::env::var_os("GHOSTDESK_DATA_DIR").as_deref());
        let name = format!("{}{scope}", mutex_name(sid.as_deref()));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match create(&name) {
                Ok((g, false)) => return Start::Proceed(Some(g)),
                // already exists: close our handle so the mutex disappears when the earlier process exits
                Ok((g, true)) => drop(g),
                // a mutex created by an elevated GhostDesk can't be opened (access denied), but it still exists
                Err(ERROR_ACCESS_DENIED) => {}
                // any other failure: run without the single-instance check (the control server still prevents
                // duplicate external connections)
                Err(_) => return Start::Proceed(None),
            }
            if let Some(w) = existing_window() {
                // a window not yet visible (before its first frame) shows up on its own shortly
                if w.visible {
                    if activate {
                        bring_to_front(w.hwnd as HWND);
                    }
                    return Start::Exit;
                }
            }
            if Instant::now() >= deadline {
                return Start::Exit;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_fits_small_screens() {
        // 1920x1080 @150% → logical 1280x(720 - taskbar)
        assert_eq!(fit_window([1500.0, 920.0], [1280.0, 680.0]), [1240.0, 600.0]);
        // unchanged on a large screen
        assert_eq!(fit_window([1500.0, 920.0], [2560.0, 1400.0]), [1500.0, 920.0]);
        // never below the minimum size, even on a tiny screen
        assert_eq!(fit_window([1500.0, 920.0], [800.0, 500.0]), [900.0, 600.0]);
        let s = initial_window_size();
        assert!(s[0] >= 900.0 && s[0] <= 1500.0 && s[1] >= 600.0 && s[1] <= 920.0, "{s:?}");
        if !cfg!(windows) {
            assert_eq!(s, [1500.0, 920.0], "macOS 는 기존 크기 그대로");
        }
    }

    #[cfg(windows)]
    #[test]
    fn gui_mutex_detects_second_instance() {
        assert_eq!(gui_instance::mutex_name(Some("S-1-5-21-1-2-3-1001")), "Local\\GhostDesk-GUI-S-1-5-21-1-2-3-1001");
        assert_eq!(gui_instance::mutex_name(None), "Local\\GhostDesk-GUI");
        // a test-only name that can't clash with the real GUI
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("Local\\GhostDesk-test-{}-{nanos}", std::process::id());
        let (first, already) = gui_instance::create(&name).unwrap();
        assert!(!already);
        let (second, already) = gui_instance::create(&name).unwrap();
        assert!(already, "두 번째 실행은 이미 있음을 알아야 함");
        drop(second);
        drop(first);
        // once all are closed, the next one is first again
        let (_again, already) = gui_instance::create(&name).unwrap();
        assert!(!already);
    }
}
