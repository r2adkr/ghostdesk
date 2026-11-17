//! GhostDesk — 사용자의 화면·마우스·키보드·브라우저 창을 전혀 건드리지 않고,
//! 격리된 화면(로그인을 복사한 브라우저·숨긴 앱·별도 세션·원격 데스크톱) 위에서
//! 하나 또는 여러 computer-use 에이전트(Claude·GPT·Gemini 등)를 동시에 돌리는 데스크톱 앱.
//!
//! Windows 에서도 콘솔 서브시스템 실행 파일이다: MCP 호스트(stdio)와 `--connect`·`--selftest*` 같은
//! 명령줄 모드가 표준 입출력과 종료 코드를 그대로 쓰게 하기 위해서다. GUI 로 실행되면 이 프로세스만을 위해
//! 생긴 콘솔 창(탐색기에서 더블클릭한 경우)은 바로 떼어 낸다.

mod agent;
mod app;
mod browser;
mod cdp;
mod control;
mod mcp;
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
// macOS: 전용 계정 세션, Windows: 자식 세션(PiP) — 같은 헬퍼 RPC
#[cfg(any(unix, windows))]
mod session;
mod vdisplay;
mod keys;
mod llm;
mod profiles;
mod rfb;
mod selftest;
mod update;

fn main() -> anyhow::Result<()> {
    // DLL 심기 방지: 실행 중에 이름만으로 읽는 DLL(그래픽 스택의 opengl32·d3d12·dxgi·d3dcompiler_47 등)을
    // 실행 파일 폴더(내려받은 폴더일 수 있음)·현재 폴더·PATH 가 아니라 System32 에서만 찾는다.
    // 정적 가져오기는 build.rs 의 /DEPENDENTLOADFLAG 가 맡는다. 가장 먼저 해야 한다.
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::LibraryLoader::{SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32};
        SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32);
    }
    // MCP 호스트가 넘긴 표준 입출력 파이프가 GhostDesk 창·브라우저로 상속되어 새지 않게 (모든 모드)
    #[cfg(windows)]
    platform::win::harden_std_handles();
    let args: Vec<String> = std::env::args().collect();
    // 외부 에이전트가 GhostDesk 를 시험용으로 하나 더 띄웠다 (control.rs 의 app_target):
    // 빈 데이터 폴더를 쓰고(설정·키·제어 연결이 따로) 다른 AI 앱에 자동으로 연결·설치하지 않는다.
    // 다른 스레드가 생기기 전이라 환경변수를 바꿔도 안전하다.
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
        // 실제 가상 모니터를 만드는 테스트: --yes 가 있어야만 실행
        Some("--selftest-vdisplay") => return rt.block_on(vdisplay::selftest(&args[2..])),
        Some("--selftest-session") => return rt.block_on(session::selftest(&args[2..])),
        Some("--session-helper") => return rt.block_on(session::run_helper(&args[2..])),
        #[cfg(unix)]
        Some("--print-session-plist") => return session::print_session_plist(&args[2..]),
        // 관리자 권한(UAC)으로 실행됨: Windows 자식 세션 기능 켜기
        #[cfg(windows)]
        Some("--enable-child-sessions") => return winapp::child::enable_cli(),
        // 외부 에이전트(Claude 앱 / Claude Code / Codex)용 MCP 서버 (stdio)
        Some("--mcp") => return rt.block_on(mcp::run(&args[2..])),
        // 설치 + Claude Code · Claude 앱 · Codex 자동 연결 / 해제
        Some("--connect") => return install::cli(&args[2..], true),
        Some("--disconnect") => return install::cli(&args[2..], false),
        // 목록에 없는 에이전트용 연결 패키지(MCP 설정·스킬·플러그인)를 만든다
        Some("--export-kit") => return kit::cli(&args[2..]),
        // 릴리스용 macOS 앱 번들(zip) 만들기 (.github/workflows/release.yml)
        #[cfg(target_os = "macos")]
        Some("--package-macos") => return install::package_macos(&args[2..]),
        _ => {}
    }
    if args.get(1).map(String::as_str) == Some("--agent-test") {
        return rt.block_on(selftest::run_agent_cli(args.get(2).cloned().unwrap_or_default()));
    }

    // ── GUI ──
    // Windows: 이 프로세스만을 위해 생긴 콘솔(이전 Windows 에서 탐색기로 실행)이 있으면 콘솔 없이 다시 띄우고 끝낸다.
    #[cfg(windows)]
    if relaunch_without_console(&args) {
        return Ok(());
    }
    // Windows: 창은 사용자당 하나만. 이미 떠 있으면 그 창을 앞으로 가져오고 끝낸다
    // (macOS 는 LaunchServices 가 같은 일을 한다). `--background` 는 사용자가 직접 띄운 것이 아니라
    // (예: MCP 서버가 자동으로 띄움) 포커스를 빼앗지 않는다.
    #[cfg(windows)]
    let background = args.iter().skip(1).any(|a| a == "--background");
    // Windows: 새로 내려받은 exe 를 실행했으면 옛 GhostDesk 를 닫고 설치본을 이것으로 바꾼 뒤 설치본을 띄운다
    #[cfg(windows)]
    if !background && install::take_over_install() {
        return Ok(());
    }
    #[cfg(windows)]
    let _single = match gui_instance::acquire(!background) {
        gui_instance::Start::Proceed(guard) => guard,
        gui_instance::Start::Exit => return Ok(()),
    };
    // 탐색기 더블클릭 등으로 이 프로세스만을 위해 생긴 콘솔 창은 닫는다 (터미널에서 실행했으면 그대로)
    #[cfg(windows)]
    detach_console_for_gui();
    // 지난번 GhostDesk 가 비정상 종료되어 화면 밖에 남은 사용자 앱 창(내 앱 '이미 실행 중인 앱에 연결')을 되돌린다
    #[cfg(windows)]
    std::thread::spawn(native::recover_parked_windows);

    // macOS: 내려받은 새 버전을 압축 푼 자리에서 바로 열었으면, 옛 설치본을 이것으로 바꾸고 설치본으로 다시 연다
    #[cfg(target_os = "macos")]
    if install::take_over_install() {
        return Ok(());
    }

    let handle = rt.handle().clone();
    #[allow(unused_mut)]
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("GhostDesk")
        .with_inner_size(initial_window_size())
        .with_min_inner_size([900.0, 600.0]);
    #[cfg(windows)]
    if background {
        viewport = viewport.with_active(false);
    }
    // Windows: 기본 창 틀 대신 앱이 제목 표시줄을 그린다 (app/titlebar.rs). 제목은 작업 표시줄·Alt+Tab 용으로 남긴다.
    #[cfg(windows)]
    {
        viewport = viewport.with_decorations(false);
    }
    // macOS: Windows 와 같은 모양으로, 내용을 제목 표시줄 밑까지 넓히고 기본 제목 줄은 투명하게 숨긴다.
    // 창 단추(신호등)와 크기 바꾸기·전체 화면은 macOS 기본 동작을 그대로 쓴다.
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
    // drop(rt) 는 실행 중인 spawn_blocking(프로필 복제·설치 등)이 끝날 때까지 무기한 기다린다.
    // 창을 닫았으면 잠깐만 기다리고 프로세스를 끝낸다 (중단된 복제는 다음 실행 때 다시 만든다).
    rt.shutdown_timeout(std::time::Duration::from_secs(3));
    // 업데이트 후 '다시 시작': 단일 실행 뮤텍스를 먼저 놓아야 새 창이 바로 뜬다
    #[cfg(windows)]
    drop(_single);
    update::relaunch_if_requested();
    Ok(())
}

/// GUI 를 콘솔 없이 다시 띄웠으면 true (이 프로세스는 바로 끝낸다).
///
/// Windows 11 24H2 이상은 실행 파일 매니페스트(consoleAllocationPolicy=detached) 덕분에 탐색기에서 실행해도
/// 콘솔이 생기지 않아 여기 걸리지 않는다. 그 전 버전에서 이 프로세스 혼자 콘솔을 쓰고 있으면, FreeConsole 만으로는
/// 기본 터미널이 Windows Terminal 일 때 빈 탭이 GUI 가 끝날 때까지 남으므로, 콘솔 없이(DETACHED_PROCESS)
/// 같은 인자로 자신을 다시 띄우고 이 프로세스는 끝낸다 → 콘솔(탭)도 곧바로 닫힌다.
#[cfg(windows)]
fn relaunch_without_console(args: &[String]) -> bool {
    use std::os::windows::process::CommandExt;
    const GUARD: &str = "GHOSTDESK_GUI_RELAUNCHED";
    if std::env::var_os(GUARD).is_some() {
        // 다시 띄운 쪽: 표시만 지우고 계속 (그래도 콘솔이 있으면 아래 detach_console_for_gui 가 처리)
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

/// GUI 로 실행될 때 이 프로세스만을 위해 생긴 콘솔 창을 떼어 낸다 (`platform::win::detach_own_console`).
/// (보통은 `relaunch_without_console` 가 먼저 처리한다. 다시 띄우기에 실패했을 때의 대비책.)
///
/// 콘솔을 떼면 콘솔을 가리키던 표준 핸들 값이 닫힌 채로 남는다. 그대로 두면
/// - 기본(inherit) 표준 입출력으로 자식을 띄우는 `Command::spawn` 이 ERROR_INVALID_HANDLE 로 실패하고,
/// - 나중에 다른 커널 객체(파일·파이프)가 같은 핸들 값을 받으면 `println!`/`eprintln!` 이 그곳에 쓰인다.
/// 그래서 콘솔이었던 표준 핸들만 NULL 로 바꿔 둔다. Rust std 는 NULL 표준 핸들을 '없음'으로 다룬다
/// (출력은 버리고, 자식에게는 표준 핸들을 넘기지 않는다). 파이프·파일로 넘겨받은 핸들은 그대로 둔다.
#[cfg(windows)]
fn detach_console_for_gui() {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetConsoleWindow, GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    let ids = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    unsafe {
        if GetConsoleWindow().is_null() {
            return; // 콘솔 창이 없음 (MCP 등이 DETACHED_PROCESS·CREATE_NO_WINDOW 로 띄움)
        }
        let was_console = ids.map(|id| {
            let h = GetStdHandle(id);
            let mut mode = 0u32;
            !h.is_null() && GetConsoleMode(h, &mut mode) != 0
        });
        platform::win::detach_own_console();
        if !GetConsoleWindow().is_null() {
            return; // 터미널에서 실행해 콘솔을 함께 쓰는 중: 그대로 둔다
        }
        for (id, console) in ids.into_iter().zip(was_console) {
            if console {
                SetStdHandle(id, std::ptr::null_mut());
            }
        }
    }
}

/// 처음 창 크기 (논리 좌표). 기본 1500x920.
/// Windows 는 노트북에서 150% 배율이 흔해(1920x1080 → 논리 1280x720) 기본 크기가 화면보다 커질 수 있으므로
/// 주 모니터의 작업 영역(작업 표시줄 제외) 안으로 줄인다.
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

/// `want` 를 작업 영역 `work`(논리 좌표) 안에 들어가게 줄인다. 창 테두리·제목 표시줄 여유를 두고,
/// 최소 크기(900x600)보다 작게는 줄이지 않는다.
#[cfg_attr(not(windows), allow(dead_code))]
fn fit_window(want: [f32; 2], work: [f32; 2]) -> [f32; 2] {
    let w = want[0].min(work[0] - 40.0).max(900.0);
    let h = want[1].min(work[1] - 80.0).max(600.0);
    [w, h]
}

/// 주 모니터 작업 영역의 크기 (논리 좌표 = 실제 픽셀 / 배율).
#[cfg(windows)]
fn windows_work_area_points() -> Option<[f32; 2]> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETWORKAREA};
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut r as *mut RECT as *mut _, 0) } != 0;
    // DPI 를 인식하지 않는 프로세스면 GetDpiForSystem 은 96 이고 작업 영역도 이미 배율이 적용된 값이다.
    let dpi = unsafe { GetDpiForSystem() }.max(96) as f32;
    let (w, h) = ((r.right - r.left) as f32, (r.bottom - r.top) as f32);
    (ok && w > 0.0 && h > 0.0).then(|| [w * 96.0 / dpi, h * 96.0 / dpi])
}

/// Windows GUI 단일 실행: 사용자(SID)별 이름 있는 뮤텍스.
#[cfg(windows)]
mod gui_instance {
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, HANDLE, HWND};

    /// 뮤텍스 핸들. 프로세스가 끝날 때까지 들고 있어야 다른 실행이 "이미 떠 있음" 을 안다.
    pub struct Guard(HANDLE);

    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    pub enum Start {
        /// 첫 번째 창: 계속 실행 (None 이면 뮤텍스를 만들지 못했지만 그래도 실행)
        Proceed(Option<Guard>),
        /// 이미 떠 있는 창이 있음: 이 프로세스는 끝낸다
        Exit,
    }

    /// 세션 안에서 사용자마다 다른 이름 (Local\ = 현재 로그인 세션)
    pub fn mutex_name(sid: Option<&str>) -> String {
        match sid {
            Some(s) if !s.is_empty() => format!("Local\\GhostDesk-GUI-{s}"),
            _ => "Local\\GhostDesk-GUI".to_string(),
        }
    }

    /// 뮤텍스를 만든다: Ok((핸들, 이미 있었는가)), 실패하면 Err(Win32 오류 코드).
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

    /// 이 실행 파일과 같은 이름의 프로세스가 가진 "GhostDesk" 최상위 창 (자기 자신 제외)
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
                // 같은 제목의 다른 창(예: 'GhostDesk' 폴더를 연 탐색기)은 건드리지 않는다
                win::process_path(w.pid)
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .is_some_and(|n| {
                        n.eq_ignore_ascii_case(&own_name) || n.eq_ignore_ascii_case(crate::platform::paths::EXE_NAME)
                    })
            })
    }

    /// 사용자가 직접 다시 실행한 경우에만 부른다: 최소화돼 있으면 복원하고 앞으로 가져온다.
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

    /// 첫 번째 창이면 Proceed. 이미 창이 있으면 (activate 일 때) 그 창을 앞으로 가져오고 Exit.
    /// 앞선 실행이 아직 창을 띄우는 중이거나 닫히는 중일 수 있으므로 잠시(최대 10초) 기다려 본다:
    /// 앞선 프로세스가 끝나 뮤텍스가 사라지면 이쪽이 첫 번째가 된다.
    pub fn acquire(activate: bool) -> Start {
        let sid = crate::platform::win::current_user_sid().ok();
        let name = mutex_name(sid.as_deref());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match create(&name) {
                Ok((g, false)) => return Start::Proceed(Some(g)),
                // 이미 있음: 내 핸들을 닫아야 앞선 프로세스가 끝났을 때 뮤텍스가 사라진다
                Ok((g, true)) => drop(g),
                // 관리자 권한으로 뜬 GhostDesk 가 만든 뮤텍스는 열 수 없다(접근 거부): 그래도 '이미 있음'
                Err(ERROR_ACCESS_DENIED) => {}
                // 그 밖의 실패: 단일 실행 확인 없이 실행한다 (제어 서버가 외부 연결 중복을 한 번 더 막는다)
                Err(_) => return Start::Proceed(None),
            }
            if let Some(w) = existing_window() {
                // 아직 보이지 않는 창(첫 화면을 그리기 전)은 곧 스스로 나타난다
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
        // 1920x1080 @150% → 논리 1280x(720-작업 표시줄)
        assert_eq!(fit_window([1500.0, 920.0], [1280.0, 680.0]), [1240.0, 600.0]);
        // 큰 화면에서는 그대로
        assert_eq!(fit_window([1500.0, 920.0], [2560.0, 1400.0]), [1500.0, 920.0]);
        // 아주 작은 화면이어도 최소 크기 아래로는 줄이지 않음
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
        // 실제 GUI 와 겹치지 않는 시험용 이름
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("Local\\GhostDesk-test-{}-{nanos}", std::process::id());
        let (first, already) = gui_instance::create(&name).unwrap();
        assert!(!already);
        let (second, already) = gui_instance::create(&name).unwrap();
        assert!(already, "두 번째 실행은 이미 있음을 알아야 함");
        drop(second);
        drop(first);
        // 모두 닫히면 다시 첫 번째가 된다
        let (_again, already) = gui_instance::create(&name).unwrap();
        assert!(!already);
    }
}
