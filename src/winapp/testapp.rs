//! 자체 시험용 Win32 창 (`ghostdesk --selftest-native --target`).
//!
//! 메뉴(가속기 글자 포함)·여러 줄 입력 칸·단추·체크 상자·콤보 상자·메시지 상자·"저장할까요?" 확인을 가진
//! 고전 Win32 앱. GhostDesk 가 숨김으로 띄워 메뉴 흉내·대화상자 처리·종료를 결정적으로 시험한다.
//! 이 창의 '복사' 메뉴는 시스템 클립보드를 쓰지 않는다 (차단이 실패해도 사용자 클립보드는 안전하게).

use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};

use anyhow::Result;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging as wm;

use crate::platform::win as pw;

#[link(name = "kernel32", kind = "raw-dylib")]
extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut core::ffi::c_void;
}

#[link(name = "user32", kind = "raw-dylib")]
extern "system" {
    fn IsDialogMessageW(hdlg: HWND, msg: *const wm::MSG) -> i32;
}

static EDIT: AtomicIsize = AtomicIsize::new(0);
static LABEL: AtomicIsize = AtomicIsize::new(0);
static COMBO: AtomicIsize = AtomicIsize::new(0);
static SAVED: AtomicU32 = AtomicU32::new(0);
static CLICKS: AtomicU32 = AtomicU32::new(0);

pub const TITLE: &str = "GhostDesk 시험 대상";
pub const QUERY_FOCUS: u32 = wm::WM_APP + 1;

const EM_SETSEL: u32 = 0x00B1;
const EM_GETMODIFY: u32 = 0x00B8;
const EM_SETMODIFY: u32 = 0x00B9;
const EM_UNDO: u32 = 0x00C7;
const ID_NEW: u32 = 101;
const ID_SAVE: u32 = 102;
const ID_EXIT: u32 = 103;
const ID_MSGBOX: u32 = 105;
const ID_ALTY: u32 = 106;
const ID_ASK: u32 = 107;
const ID_UNDO: u32 = 201;
const ID_COPY: u32 = 202;
const ID_SELALL: u32 = 203;
const ID_ABOUT: u32 = 301;
const ID_EDIT: i32 = 1001;
const ID_BUTTON: i32 = 1002;
const ID_LABEL: i32 = 1003;
const ID_CHECK: i32 = 1004;
const ID_COMBO: i32 = 1005;
const ID_ENV: i32 = 1006;

/// 이 프로세스가 놓인 환경 (자체 시험이 ui_tree 로 읽는다): 속한 Job 의 UI 제한, 물려받은 오류 모드
pub fn env_label() -> String {
    let ui = super::launch::job_ui_limits(std::ptr::null_mut()).map(|u| format!("{u:#x}")).unwrap_or_else(|| "none".into());
    let account = pw::current_user_sid().unwrap_or_default();
    let profile = std::env::var("USERPROFILE").unwrap_or_default();
    let persisted = std::env::var_os("GHOSTDESK_SELFTEST_LOGIN_STATE")
        .and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    format!("환경 job-ui={ui} errmode={:#x} account={account} profile={profile} persisted={persisted}", super::launch::process_error_mode())
}

fn set_text(h: isize, s: &str) {
    let w = pw::wide(s);
    unsafe { wm::SetWindowTextW(h as HWND, w.as_ptr()) };
}

unsafe fn child(parent: HWND, class: &str, text: &str, style: u32, ex: u32, r: (i32, i32, i32, i32), id: i32) -> isize {
    let c = pw::wide(class);
    let t = pw::wide(text);
    wm::CreateWindowExW(
        ex,
        c.as_ptr(),
        t.as_ptr(),
        style | wm::WS_CHILD | wm::WS_VISIBLE,
        r.0,
        r.1,
        r.2,
        r.3,
        parent,
        id as isize as _,
        GetModuleHandleW(std::ptr::null()),
        std::ptr::null(),
    ) as isize
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        QUERY_FOCUS => super::w32::thread_focus(super::w32::thread_pid(h as isize).0).map(|(_, f)| f).unwrap_or(0),
        wm::WM_CREATE => {
            let e = child(
                h,
                "EDIT",
                "",
                (wm::ES_MULTILINE | wm::ES_WANTRETURN | wm::ES_AUTOVSCROLL) as u32 | wm::WS_VSCROLL | wm::WS_BORDER | wm::WS_TABSTOP,
                0,
                (10, 10, 400, 120),
                ID_EDIT,
            );
            EDIT.store(e, Ordering::SeqCst);
            child(h, "BUTTON", "확인(&O)", (wm::BS_PUSHBUTTON as u32) | wm::WS_TABSTOP, 0, (420, 10, 120, 30), ID_BUTTON);
            LABEL.store(child(h, "STATIC", "클릭 0", 0, 0, (420, 50, 180, 24), ID_LABEL), Ordering::SeqCst);
            child(h, "BUTTON", "옵션(&P)", (wm::BS_AUTOCHECKBOX as u32) | wm::WS_TABSTOP, 0, (420, 80, 180, 24), ID_CHECK);
            let c = child(h, "COMBOBOX", "", (wm::CBS_DROPDOWNLIST as u32) | wm::WS_TABSTOP | wm::WS_VSCROLL, 0, (10, 140, 200, 200), ID_COMBO);
            for it in ["사과", "바나나", "포도"] {
                let s = pw::wide(it);
                wm::SendMessageW(c as HWND, wm::CB_ADDSTRING, 0, s.as_ptr() as isize);
            }
            COMBO.store(c, Ordering::SeqCst);
            child(h, "STATIC", &env_label(), 0, 0, (220, 140, 380, 24), ID_ENV);
            0
        }
        wm::WM_COMMAND => {
            let id = (w & 0xFFFF) as u32;
            let code = ((w >> 16) & 0xFFFF) as u32;
            let edit = EDIT.load(Ordering::SeqCst) as HWND;
            match id {
                ID_NEW => {
                    set_text(edit as isize, "");
                    set_text(h as isize, "새 문서");
                }
                ID_SAVE => {
                    let n = SAVED.fetch_add(1, Ordering::SeqCst) + 1;
                    wm::SendMessageW(edit, EM_SETMODIFY, 0, 0);
                    set_text(h as isize, &format!("저장됨 {n}"));
                }
                ID_MSGBOX => {
                    let t = pw::wide("시험용 메시지 상자입니다");
                    let c = pw::wide("GhostDesk 시험");
                    wm::MessageBoxW(h, t.as_ptr(), c.as_ptr(), wm::MB_OK | wm::MB_ICONINFORMATION);
                }
                // 본 창 메뉴의 Alt+Y 단축키 — 질문 상자(예(&Y))가 열린 동안에는 실행되면 안 된다
                ID_ALTY => set_text(h as isize, "Alt+Y 메뉴 실행됨"),
                ID_ASK => {
                    let t = pw::wide("시험용 질문 상자입니다");
                    let c = pw::wide("GhostDesk 시험");
                    let r = wm::MessageBoxW(h, t.as_ptr(), c.as_ptr(), wm::MB_YESNO | wm::MB_ICONQUESTION);
                    set_text(h as isize, if r == wm::IDYES { "질문: 예" } else { "질문: 아니요" });
                }
                ID_EXIT => {
                    wm::DestroyWindow(h);
                }
                ID_UNDO => {
                    wm::SendMessageW(edit, EM_UNDO, 0, 0);
                }
                ID_COPY => set_text(h as isize, "복사 실행됨 (차단되어야 함)"),
                ID_SELALL => {
                    wm::SendMessageW(edit, EM_SETSEL, 0, -1);
                }
                ID_ABOUT => set_text(h as isize, "정보 열림 (차단되어야 함)"),
                x if x == ID_BUTTON as u32 && code == 0 => {
                    let n = CLICKS.fetch_add(1, Ordering::SeqCst) + 1;
                    set_text(LABEL.load(Ordering::SeqCst), &format!("클릭 {n}"));
                }
                x if x == ID_COMBO as u32 && code == 1 => {
                    let c = COMBO.load(Ordering::SeqCst) as HWND;
                    let i = wm::SendMessageW(c, wm::CB_GETCURSEL, 0, 0);
                    let mut buf = [0u16; 64];
                    wm::SendMessageW(c, wm::CB_GETLBTEXT, i as usize, buf.as_mut_ptr() as isize);
                    set_text(h as isize, &format!("선택: {}", pw::from_wide_buf(&buf)));
                }
                _ => {}
            }
            0
        }
        wm::WM_CLOSE => {
            let edit = EDIT.load(Ordering::SeqCst) as HWND;
            if wm::SendMessageW(edit, EM_GETMODIFY, 0, 0) != 0 {
                let t = pw::wide("변경 내용을 저장할까요?");
                let c = pw::wide("GhostDesk 시험");
                let r = wm::MessageBoxW(h, t.as_ptr(), c.as_ptr(), wm::MB_YESNOCANCEL | wm::MB_ICONQUESTION);
                if r == wm::IDCANCEL {
                    return 0;
                }
            }
            wm::DestroyWindow(h);
            0
        }
        wm::WM_DESTROY => {
            wm::PostQuitMessage(0);
            0
        }
        _ => wm::DefWindowProcW(h, msg, w, l),
    }
}

unsafe fn build_menu() -> wm::HMENU {
    let bar = wm::CreateMenu();
    let add = |m: wm::HMENU, flags: u32, id: usize, text: &str| {
        let t = pw::wide(text);
        wm::AppendMenuW(m, flags, id, t.as_ptr());
    };
    let file = wm::CreatePopupMenu();
    add(file, wm::MF_STRING, ID_NEW as usize, "새로 만들기(&N)\tCtrl+N");
    add(file, wm::MF_STRING, ID_SAVE as usize, "저장(&S)\tCtrl+S");
    add(file, wm::MF_STRING, ID_MSGBOX as usize, "메시지 상자(&M)\tCtrl+M");
    add(file, wm::MF_STRING, ID_ASK as usize, "질문 상자(&Q)\tCtrl+K");
    add(file, wm::MF_STRING, ID_ALTY as usize, "다른 동작(&Y)\tAlt+Y");
    add(file, wm::MF_SEPARATOR, 0, "");
    add(file, wm::MF_STRING, ID_EXIT as usize, "끝내기(&X)");
    let edit = wm::CreatePopupMenu();
    add(edit, wm::MF_STRING, ID_UNDO as usize, "실행 취소(&U)\tCtrl+Z");
    add(edit, wm::MF_STRING, ID_COPY as usize, "복사(&C)\tCtrl+C");
    add(edit, wm::MF_STRING, ID_SELALL as usize, "모두 선택(&A)\tCtrl+A");
    add(edit, wm::MF_STRING | wm::MF_GRAYED, 299, "비활성 항목(&D)\tCtrl+D");
    let help = wm::CreatePopupMenu();
    add(help, wm::MF_STRING, ID_ABOUT as usize, "정보(&A)");
    add(bar, wm::MF_POPUP, file as usize, "파일(&F)");
    add(bar, wm::MF_POPUP, edit as usize, "편집(&E)");
    add(bar, wm::MF_POPUP, help as usize, "도움말(&H)");
    bar
}

/// 시험 대상 창을 띄우고 메시지 루프를 돈다. offscreen_visible 이면 처음부터 화면 밖에 도구 창으로 보이게
/// (Mode B — "사용자가 이미 띄워 둔 앱" 흉내, 사용자 화면에는 나타나지 않음). minimized 면 그 상태로 최소화 (도구 창의 최소화 위치는 화면 밖).
pub fn run(offscreen_visible: bool, minimized: bool) -> Result<()> {
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = pw::wide("GhostDeskWinAppTestTarget");
        let wc = wm::WNDCLASSEXW {
            cbSize: std::mem::size_of::<wm::WNDCLASSEXW>() as u32,
            style: wm::CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hCursor: wm::LoadCursorW(std::ptr::null_mut(), wm::IDC_ARROW),
            hbrBackground: 16usize as _, // COLOR_BTNFACE + 1
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        wm::RegisterClassExW(&wc);
        let (x, y, ex) = if offscreen_visible {
            let (ox, oy) = pw::offscreen_origin();
            (ox, oy, wm::WS_EX_TOOLWINDOW)
        } else {
            (wm::CW_USEDEFAULT, wm::CW_USEDEFAULT, 0)
        };
        let title = pw::wide(TITLE);
        let hwnd = wm::CreateWindowExW(
            ex,
            class.as_ptr(),
            title.as_ptr(),
            wm::WS_OVERLAPPEDWINDOW,
            x,
            y,
            640,
            420,
            std::ptr::null_mut(),
            build_menu(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            anyhow::bail!("시험 창을 만들지 못했습니다");
        }
        // 일반 앱처럼 시작 정보(STARTUPINFO)의 표시 방법을 따른다 (GhostDesk 가 SW_HIDE 로 띄움)
        wm::ShowWindow(
            hwnd,
            if minimized {
                wm::SW_SHOWMINNOACTIVE
            } else if offscreen_visible {
                wm::SW_SHOWNOACTIVATE
            } else {
                wm::SW_SHOWDEFAULT
            },
        );
        if minimized {
            // '최대화 상태에서 최소화된 창' 흉내 (복원하면 최대화): 최대화 자체는 사용자 모니터에 붙으므로 하지 않고 플래그만 둔다
            let mut wp: wm::WINDOWPLACEMENT = std::mem::zeroed();
            wp.length = std::mem::size_of::<wm::WINDOWPLACEMENT>() as u32;
            if wm::GetWindowPlacement(hwnd, &mut wp) != 0 {
                wp.flags = wm::WPF_RESTORETOMAXIMIZED;
                wp.showCmd = wm::SW_SHOWMINNOACTIVE as u32;
                wm::SetWindowPlacement(hwnd, &wp);
            }
        }
        let accels = [
            wm::ACCEL { fVirt: (wm::FCONTROL | wm::FVIRTKEY) as u8, key: b'N' as u16, cmd: ID_NEW as u16 },
            wm::ACCEL { fVirt: (wm::FCONTROL | wm::FVIRTKEY) as u8, key: b'S' as u16, cmd: ID_SAVE as u16 },
            wm::ACCEL { fVirt: (wm::FCONTROL | wm::FVIRTKEY) as u8, key: b'M' as u16, cmd: ID_MSGBOX as u16 },
        ];
        let haccel = wm::CreateAcceleratorTableW(accels.as_ptr(), accels.len() as i32);
        let mut msg: wm::MSG = std::mem::zeroed();
        while wm::GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            if wm::TranslateAcceleratorW(hwnd, haccel, &msg) != 0 || IsDialogMessageW(hwnd, &msg) != 0 {
                continue;
            }
            wm::TranslateMessage(&msg);
            wm::DispatchMessageW(&msg);
        }
    }
    Ok(())
}
