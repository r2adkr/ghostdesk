//! Windows 일반 앱 백엔드 (native.rs 의 Windows 구현).
//!
//! 앱 창을 모든 모니터 바깥(화면 밖)에 두고, PrintWindow 로 캡처하고, 창 메시지·UI Automation 으로 조작한다.
//! 사용자의 마우스 커서·키보드 포커스·화면·시스템 클립보드는 건드리지 않는다. Mode A(숨김 실행)에서는 Job 의 UI 제한으로
//! 앱 자신도 시스템 클립보드를 읽거나 쓸 수 없고, Mode B(사용자 앱에 붙기)에서는 복사·붙여넣기 단추·메뉴를 이름으로 막는다.
//!
//! | 모듈 | 하는 일 |
//! |---|---|
//! | `launch` | 숨김(SW_HIDE)·일시정지로 CreateProcessW + Job(KILL_ON_JOB_CLOSE + 클립보드·시스템 설정 UI 제한), 오류 대화상자 끄기, 스토어 앱·실행 별칭·콘솔·관리자 앱·셸 거부 |
//! | `guard` | 노출 감시: WinEvent 훅 + 8ms 검사로 화면에 걸친 창을 즉시 화면 밖으로, 전경·클립보드 감시 |
//! | `capture` | PrintWindow(PW_RENDERFULLCONTENT) → 내용 영역 → 축소 → PNG/JPEG |
//! | `input` | 스크린샷 좌표 → 자식 창 좌표, WM_MOUSE*/WM_KEY*/WM_CHAR 보내기 |
//! | `menu` | HMENU 읽기, 가속기 글자("Ctrl+S") 해석, WM_COMMAND, 차단 규칙 |
//! | `uia` | UI Automation 요소 목록·Invoke/Toggle/Select/Value |
//! | `apps` | 시작 메뉴 .lnk·App Paths 로 설치된 앱 찾기 |
//! | `recovery` | Mode B 에서 옮긴 사용자 창 기록 (비정상 종료 뒤 되돌리기) |
//! | `workspace` | 위를 묶은 NativeAppWorkspace |
//! | `child` | 자식 세션(PiP): RDP 컨트롤로 같은 계정의 별도 세션 연결 + 그 안의 화면 캡처·SendInput 헬퍼 |
//! | `selftest` | `--selftest-native` (`testapp`: 시험용 Win32 창) |

#![allow(dead_code)]

mod apps;
mod capture;
pub mod child;
mod guard;
mod hdesk;
mod input;
mod launch;
mod menu;
mod recovery;
mod selftest;
mod testapp;
mod uia;
mod w32;
mod workspace;

pub use apps::{installed_apps, resolve_app_query};
pub use launch::preflight;
pub use recovery::recover as recover_parked_windows;
pub use selftest::selftest;
pub use workspace::NativeAppWorkspace;

/// 에이전트 시스템 프롬프트의 환경 설명 (agent.rs 의 ENV_NATIVE 대신)
pub const ENV_PROMPT: Option<&'static str> = Some(ENV_WINDOWS);

const ENV_WINDOWS: &str = r#"Environment
- You operate one window of a classic Windows desktop app that runs off-screen: its windows are placed outside every monitor, so nothing appears on the human's screen, and GhostDesk never uses the human's mouse cursor, keyboard focus or clipboard. The screenshot shows only the client area of the current window (no title bar, menu bar, taskbar or other apps); its size varies per window (long edge <= 1280 px). Coordinates always refer to the latest screenshot.
- Mouse: left clicks, double clicks and the scroll wheel are delivered to the control under the point as window messages (pixel-accurate). Each click reports the control it hit in Events. Drags work only on scroll bars, sliders and plain text boxes (other controls could start drag-and-drop, which Windows would complete at the human's real cursor); use scroll, set_value or select instead. Right-click / context menus, the middle button, opening combo-box drop-downs and date-picker calendars are refused because Windows draws those pop-ups on the human's screen.
- Typing: `type` sends characters directly (Korean works) to the focused control of the app and verifies standard text boxes automatically. Click a text box first to put the caret there. Named keys (Enter, Tab, Esc, arrows, Backspace, Delete, Home/End, PageUp/PageDown, F2-F12) work, except keys that would open a drop-down on the human's screen (F4 on combo boxes and date pickers, Up/Down on combo boxes that open their list with the arrow keys). Shift/Ctrl/Alt key states cannot be faked for a background window, so: ctrl+a/c/x/v/z/y in text boxes are emulated with an internal clipboard (the human's clipboard is never touched); other ctrl/alt shortcuts run the menu item that shows the same shortcut (e.g. ctrl+s -> File > Save); alt+F4 closes the window; alt+<letter> clicks the dialog button with that underlined letter. If none applies the key returns an error: use `app_control` instead.
- `app_control`: ui_tree lists UI Automation elements of the current window (id, type, name, value, state, frame in screenshot pixels); then press / set_value / select / choose / focus / read by id (ids are valid until the next ui_tree). menus shows the classic menu bar with shortcuts and disabled/blocked items; menu {path: "File > Save"} (localized names, e.g. "파일 > 저장") runs an item without opening the menu on screen. While a modal dialog (message box, Save As, ...) is open, the main window's menus and shortcuts are refused, just like with a real keyboard: finish or close the dialog first.
- Refused for the human's safety: Help (opens a browser or help window on the human's screen), links, the app's own Copy/Cut/Paste buttons and menu items (use ctrl+c/x/v, which use the internal clipboard), F1, F10, bare Alt, Alt+Space, Shift+F10, the Apps key, Windows-key shortcuts, minimize/maximize buttons, the Windows shell (Explorer, taskbar, desktop) and system launchers.
- Clipboard: when GhostDesk launched the app itself (the usual case), the app cannot read or write the human's clipboard at all, so any Copy/Paste feature of its own (toolbar icons and the like) silently does nothing; use ctrl+c/x/v. When attached to the human's already-running app, only the named Copy/Cut/Paste buttons and menu items are refused and the app can still write the clipboard on its own (e.g. Character Map's Select button); Events reports it, so tell the human when that happens.
- New windows and dialogs (message boxes, Save As) open off-screen too, are switched to automatically and reported in Events; take a screenshot before clicking in them. If Events says the human brought the app back to the screen, stop and ask the human via `ask_user`."#;

/// app_control 도구 설명 (Windows)
pub const APP_CONTROL_DESCRIPTION: &str = "Semantic control of the Windows desktop app window you are operating. The app runs off-screen (outside every monitor), so clicks and keys are window messages; use this tool for menus, for elements a click cannot reach, to set or read text exactly, and to pick combo-box options without opening the drop-down.\n\
Actions:\n\
- list_windows: list the app's windows (index, title, disabled while a dialog is open).\n\
- switch_window {index}: operate another window (dialogs are switched to automatically).\n\
- ui_tree {max_depth?}: numbered list of UI Automation elements of the current window: id, type (Button, Edit, CheckBox, ComboBox, ListItem, ...), name, value (truncated), frame [x,y,w,h] in SCREENSHOT pixels, checked/selected/disabled/focused. Ids stay valid until the next ui_tree call.\n\
- press {id}: click a button / check box / radio button (window message), or Invoke / Toggle / Select / Expand an element.\n\
- set_value {id, text}: replace the text of an edit box (verified by re-reading).\n\
- focus {id}: put the caret in a text box (clicks it; keyboard focus of other element types cannot be set without bringing the app to the human's screen).\n\
- select {id}: select a list / tree / tab item.\n\
- choose {id, option}: pick a combo-box option without opening the drop-down on screen.\n\
- menus: list the classic menu bar (localized titles, shortcuts, disabled and blocked items).\n\
- menu {path}: run a menu item, e.g. \"File > Save\" or localized \"파일 > 저장\" (WM_COMMAND; the menu is never opened on screen).\n\
- read {id}: full untruncated name / value / text of an element.\n\
Shortcuts: ctrl+a/c/x/v/z/y in text boxes are emulated with an internal clipboard (the user's system clipboard is never touched); other ctrl/alt shortcuts run the menu item with the same shortcut (alt+letter presses the dialog button with that underlined letter). While a modal dialog is open, the main window's menus and shortcuts, and press / set_value / focus / select / choose on the disabled main window, are refused (finish or close the dialog first; ui_tree, read and menus still work there).\n\
Blocked for the user's safety: Help, links, the app's own Copy / Cut / Paste (system clipboard), right-click and context menus, combo-box drop-downs (including F4), drags outside scroll bars / sliders / plain text boxes (drag-and-drop would land at the user's real cursor), minimize / maximize. After a new window opens, take a screenshot before clicking in it.";
