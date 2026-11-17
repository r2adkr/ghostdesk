//! Windows backend for regular desktop apps (the Windows side of `native.rs`).
//!
//! App windows are parked outside every monitor, captured with PrintWindow and driven through
//! window messages and UI Automation. The user's cursor, keyboard focus, screen and system
//! clipboard are left alone. In mode A (hidden launch) the Job's UI restrictions keep the app
//! itself from reading or writing the system clipboard; in mode B (attaching to a user's app)
//! copy/paste buttons and menu items are blocked by name.
//!
//! | Module | Purpose |
//! |---|---|
//! | `launch` | Hidden (SW_HIDE), suspended CreateProcessW + Job (KILL_ON_JOB_CLOSE plus clipboard and system-settings UI limits); disables error dialogs; refuses Store apps, execution aliases, console, elevated and shell apps |
//! | `guard` | Exposure watch: a WinEvent hook plus an 8 ms check move any window touching the screen back off it; watches the foreground and clipboard |
//! | `capture` | PrintWindow(PW_RENDERFULLCONTENT) → client area → downscale → PNG/JPEG |
//! | `input` | Screenshot coordinates → child window coordinates; sends WM_MOUSE*/WM_KEY*/WM_CHAR |
//! | `menu` | Reads HMENUs, parses accelerator text ("Ctrl+S"), sends WM_COMMAND, block rules |
//! | `uia` | UI Automation element listing and Invoke/Toggle/Select/Value |
//! | `apps` | Finds installed apps via Start menu .lnk files and App Paths |
//! | `recovery` | Records user windows moved in mode B, to restore them after a crash |
//! | `workspace` | `NativeAppWorkspace`, tying the above together |
//! | `child` | Child session (PiP): connects a separate session of the same account through the RDP control, plus capture and SendInput helpers inside it |
//! | `selftest` | `--selftest-native` (`testapp`: a Win32 test window) |

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

/// Environment description for the agent's system prompt (replaces `ENV_NATIVE`).
pub const ENV_PROMPT: Option<&'static str> = Some(ENV_WINDOWS);

const ENV_WINDOWS: &str = r#"Environment
- You operate one window of a classic Windows desktop app that runs off-screen: its windows are placed outside every monitor, so nothing appears on the human's screen, and GhostDesk never uses the human's mouse cursor, keyboard focus or clipboard. The screenshot shows only the client area of the current window (no title bar, menu bar, taskbar or other apps); its size varies per window (long edge <= 1280 px). Coordinates always refer to the latest screenshot.
- Mouse: left clicks, double clicks and the scroll wheel are delivered to the control under the point as window messages (pixel-accurate). Each click reports the control it hit in Events. Drags work only on scroll bars, sliders and plain text boxes (other controls could start drag-and-drop, which Windows would complete at the human's real cursor); use scroll, set_value or select instead. Right-click / context menus, the middle button, opening combo-box drop-downs and date-picker calendars are refused because Windows draws those pop-ups on the human's screen.
- Typing: `type` sends characters directly (Korean works) to the focused control of the app and verifies standard text boxes automatically. Click a text box first to put the caret there. Named keys (Enter, Tab, Esc, arrows, Backspace, Delete, Home/End, PageUp/PageDown, F2-F12) work, except keys that would open a drop-down on the human's screen (F4 on combo boxes and date pickers, Up/Down on combo boxes that open their list with the arrow keys). Shift/Ctrl/Alt key states cannot be faked for a background window, so: ctrl+a/c/x/v/z/y in text boxes are emulated with an internal clipboard (the human's clipboard is never touched); other ctrl/alt shortcuts run the menu item that shows the same shortcut (e.g. ctrl+s -> File > Save); alt+F4 closes the window; alt+<letter> clicks the dialog button with that underlined letter. If none applies the key returns an error: use `app_control` instead.
- `app_control`: ui_tree lists UI Automation elements of the current window (id, type, name, value, state, frame in screenshot pixels); then press / set_value / select / choose / focus / read by id (ids are valid until the next ui_tree). menus shows the classic menu bar with shortcuts and disabled/blocked items; menu {path: "File > Save"} (localized names, e.g. "파일 > 저장") runs an item without opening the menu on screen. While a modal dialog (message box, Save As, ...) is open, the main window's menus and shortcuts are refused, just like with a real keyboard: finish or close the dialog first.
- Refused for the human's safety: Help (opens a browser or help window on the human's screen), links, the app's own Copy/Cut/Paste buttons and menu items (use ctrl+c/x/v, which use the internal clipboard), F1, F10, bare Alt, Alt+Space, Shift+F10, the Apps key, Windows-key shortcuts, minimize/maximize buttons, the Windows shell (Explorer, taskbar, desktop) and system launchers.
- Clipboard: when GhostDesk launched the app itself (the usual case), the app cannot read or write the human's clipboard at all, so any Copy/Paste feature of its own (toolbar icons and the like) silently does nothing; use ctrl+c/x/v. When attached to the human's already-running app, only the named Copy/Cut/Paste buttons and menu items are refused and the app can still write the clipboard on its own (e.g. Character Map's Select button); Events reports it, so tell the human when that happens.
- New windows and dialogs (message boxes, Save As) open off-screen too, are switched to automatically and reported in Events; take a screenshot before clicking in them. If Events says the human brought the app back to the screen, stop and ask the human via `ask_user`."#;

/// `app_control` tool description (Windows).
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
