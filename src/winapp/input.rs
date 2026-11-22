//! Input through window messages, without the real mouse cursor or keyboard.
//!
//! - Mouse: screenshot coordinates → client coordinates → deepest visible child window at that
//!   point → coordinates relative to that child, then PostMessage WM_MOUSEMOVE / WM_LBUTTONDOWN
//!   / WM_LBUTTONUP (/ WM_LBUTTONDBLCLK). The user's cursor never moves. Apps that read the real
//!   cursor with GetCursorPos while dragging may not follow.
//! - Wheel: WM_MOUSEWHEEL / WM_MOUSEHWHEEL (lParam is in screen coordinates, i.e. off-screen).
//! - Text: WM_CHAR, one per UTF-16 unit. It bypasses the IME, so Korean goes in as is.
//! - Named keys: WM_KEYDOWN / WM_KEYUP with an lParam carrying repeat 1, scan code, extended-key
//!   bit and transition bits. Posted messages don't change the app's GetKeyState, so Ctrl/Alt/
//!   Shift combos can't be faked with key messages (shortcuts are emulated through menu
//!   accelerators and edit commands instead; see `workspace`).

use anyhow::{bail, Result};

use super::menu::{self, Accel};
use super::w32::{self, Hwnd};
use crate::keys::{MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT};

/// Screenshot pixels ↔ client coordinates, as of the capture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geom {
    pub hwnd: Hwnd,
    pub client_w: u32,
    pub client_h: u32,
    pub shot_w: u32,
    pub shot_h: u32,
}

impl Geom {
    pub fn sx(&self) -> f64 {
        self.client_w as f64 / self.shot_w.max(1) as f64
    }
    pub fn sy(&self) -> f64 {
        self.client_h as f64 / self.shot_h.max(1) as f64
    }
    /// Screenshot coordinates → client coordinates (physical pixels).
    pub fn to_client(&self, x: f64, y: f64) -> (i32, i32) {
        ((x * self.sx()).floor() as i32, (y * self.sy()).floor() as i32)
    }
    pub fn contains_shot(&self, x: f64, y: f64) -> bool {
        x >= 0.0 && y >= 0.0 && x < self.shot_w as f64 && y < self.shot_h as f64
    }
    /// Screen rectangle `(l, t, r, b)` → screenshot `[x, y, w, h]`. `client_origin` is the client
    /// area's current origin in screen coordinates.
    pub fn rect_to_shot(&self, client_origin: (i32, i32), r: (i32, i32, i32, i32)) -> [i64; 4] {
        let (l, t, rr, b) = r;
        [
            ((l - client_origin.0) as f64 / self.sx()).round() as i64,
            ((t - client_origin.1) as f64 / self.sy()).round() as i64,
            ((rr - l) as f64 / self.sx()).round() as i64,
            ((b - t) as f64 / self.sy()).round() as i64,
        ]
    }
}

/// The deepest visible child window under a point (client coordinates of `top`), and the point
/// relative to it.
///
/// Transparent parts like group boxes fall through to the control behind
/// (RealChildWindowFromPoint).
pub fn hit(top: Hwnd, p: (i32, i32)) -> (Hwnd, (i32, i32)) {
    let mut cur = top;
    let mut pt = p;
    for _ in 0..32 {
        let ch = w32::real_child_at(cur, pt);
        if ch == 0 || ch == cur || !w32::is_visible(ch) {
            break;
        }
        pt = w32::map_point(cur, ch, pt);
        cur = ch;
    }
    (cur, pt)
}

pub fn mk_flags(mods: i64) -> usize {
    let mut f = 0;
    if mods & MOD_SHIFT != 0 {
        f |= w32::MK_SHIFT;
    }
    if mods & (MOD_CTRL | MOD_META) != 0 {
        f |= w32::MK_CONTROL;
    }
    f
}

/// Left click (`count` 2 = double click: WM_LBUTTONDBLCLK if the class has CS_DBLCLKS).
pub fn post_click(target: Hwnd, pt: (i32, i32), count: u32, mk: usize) {
    let l = w32::make_lparam(pt.0, pt.1);
    w32::post(target, w32::WM_MOUSEMOVE, mk, l);
    let dbl = w32::class_style(target) & w32::CS_DBLCLKS != 0;
    for i in 0..count.clamp(1, 3) {
        let down = if i == 1 && dbl { w32::WM_LBUTTONDBLCLK } else { w32::WM_LBUTTONDOWN };
        w32::post(target, down, mk | w32::MK_LBUTTON, l);
        w32::post(target, w32::WM_LBUTTONUP, mk, l);
    }
}

pub fn post_move(target: Hwnd, pt: (i32, i32), mk: usize) {
    w32::post(target, w32::WM_MOUSEMOVE, mk, w32::make_lparam(pt.0, pt.1));
}

pub fn post_button(target: Hwnd, pt: (i32, i32), down: bool, mk: usize) {
    let l = w32::make_lparam(pt.0, pt.1);
    if down {
        w32::post(target, w32::WM_LBUTTONDOWN, mk | w32::MK_LBUTTON, l);
    } else {
        w32::post(target, w32::WM_LBUTTONUP, mk, l);
    }
}

/// Wheel (`delta`: one notch = 120, positive = up/right). `screen_pt` is in screen coordinates.
pub fn post_wheel(target: Hwnd, screen_pt: (i32, i32), delta: i32, horizontal: bool, mk: usize) {
    let msg = if horizontal { w32::WM_MOUSEHWHEEL } else { w32::WM_MOUSEWHEEL };
    let w = w32::make_wparam(mk as u32, delta as i16 as u16 as u32);
    w32::post(target, msg, w, w32::make_lparam(screen_pt.0, screen_pt.1));
}

/// Scroll pixels (agent units: one step = 100 px) → wheel notches (sign kept, at least one).
pub fn wheel_notches(px: f64) -> i32 {
    if px.abs() < 1.0 {
        return 0;
    }
    let n = (px / 100.0).round() as i32;
    if n == 0 {
        px.signum() as i32
    } else {
        n.clamp(-50, 50)
    }
}

// ---- keyboard ----

pub fn is_extended_vk(vk: u32) -> bool {
    matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0x5B | 0x5C | 0x5D | 0x6F | 0x90 | 0xA3 | 0xA5)
}

/// lParam for WM_KEYDOWN/WM_KEYUP: repeat 1, scan code, extended-key bit (24), Alt context
/// (29), previous state (30) and transition (31).
pub fn key_lparam(scan: u32, extended: bool, up: bool, alt: bool) -> isize {
    let mut l: u32 = 1 | ((scan & 0xFF) << 16);
    if extended {
        l |= 1 << 24;
    }
    if alt {
        l |= 1 << 29;
    }
    if up {
        l |= (1 << 30) | (1 << 31);
    }
    l as i32 as isize
}

pub fn post_key(target: Hwnd, vk: u32, up: bool) {
    let l = key_lparam(w32::scan_code(vk), is_extended_vk(vk), up, false);
    w32::post(target, if up { w32::WM_KEYUP } else { w32::WM_KEYDOWN }, vk as usize, l);
}

/// Presses `vk` with Ctrl and/or Shift held, as a background window sees it. Apps read modifiers with
/// `GetKeyState`, which posted key messages don't change, so for the moment of the key this thread joins the
/// target thread's input state, sets the modifiers there and puts the old state back afterwards (the same thing
/// AutoHotkey's ControlSend does). The user's own keyboard state is a different input queue and is not touched.
/// `false` when the thread can't be joined (then nothing was sent).
pub fn post_with_mods(target: Hwnd, vk: u32, ctrl: bool, shift: bool) -> bool {
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardState, SetKeyboardState};
    let (tid, _) = w32::thread_pid(target);
    let me = unsafe { GetCurrentThreadId() };
    if tid == 0 || tid == me || unsafe { AttachThreadInput(me, tid, 1) } == 0 {
        return false;
    }
    let mut old = [0u8; 256];
    unsafe { GetKeyboardState(old.as_mut_ptr()) };
    let mut held = old;
    for (on, keys) in [(ctrl, [0x11, 0xA2]), (shift, [0x10, 0xA0])] {
        for k in keys {
            held[k] = if on { 0x80 } else { 0 };
        }
    }
    unsafe { SetKeyboardState(held.as_ptr()) };
    // A message sent to the thread is handled before its posted ones, so the second empty send only returns once
    // the posted key has been handled (while the modifiers are still down).
    let drain = || {
        for _ in 0..2 {
            w32::send_timeout(target, windows_sys::Win32::UI::WindowsAndMessaging::WM_NULL, 0, 0, 1000);
        }
    };
    post_key(target, vk, false);
    drain();
    post_key(target, vk, true);
    drain();
    unsafe {
        SetKeyboardState(old.as_ptr());
        AttachThreadInput(me, tid, 0);
    }
    true
}

/// Strips control characters that must not be typed (\x03, \x16 and \x18 are copy, paste and
/// cut in edit controls) and turns \r\n and \r into \n.
///
/// Returns `(cleaned text, number of characters removed)`.
pub fn sanitize_text(text: &str) -> (String, usize) {
    let t = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut removed = 0;
    let s: String = t
        .chars()
        .filter(|&c| {
            let ok = c == '\n' || c == '\t' || !(c.is_control());
            if !ok {
                removed += 1;
            }
            ok
        })
        .collect();
    (s, removed)
}

/// Types text: newlines and tabs as keys (the app's TranslateMessage makes the characters), the
/// rest as WM_CHAR. Returns the number of characters sent.
///
/// WM_CHAR from TranslateMessage goes to the **end** of the queue, so after Enter/Tab, `flush`
/// waits for the app to drain its queue before the rest is sent (otherwise the newline lands
/// late).
pub fn post_text(target: Hwnd, text: &str, flush: &dyn Fn()) -> usize {
    let mut n = 0;
    for c in text.chars() {
        match c {
            '\n' => {
                post_key(target, menu::VK_RETURN, false);
                post_key(target, menu::VK_RETURN, true);
                flush();
            }
            '\t' => {
                post_key(target, menu::VK_TAB, false);
                post_key(target, menu::VK_TAB, true);
                flush();
            }
            _ => {
                let mut b = [0u16; 2];
                for u in c.encode_utf16(&mut b) {
                    w32::post(target, w32::WM_CHAR, *u as usize, 1);
                }
            }
        }
        n += 1;
    }
    n
}

/// How to send a key combo.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyPlan {
    /// Characters (no modifiers, or shift only) → WM_CHAR.
    Text(String),
    /// Named key (no modifiers) → WM_KEYDOWN/UP. `shift` means the caller also asked for shift
    /// (which may not take effect).
    Key { vk: u32, shift: bool },
    /// Ctrl/Alt combo → emulated (edit commands, menu accelerators).
    Shortcut(Accel),
    /// A lone modifier (ctrl, shift) → no effect.
    BareModifier,
}

/// Agent key combo → how to send it. Combos that would open menus or windows on the user's
/// screen are refused with a reason.
pub fn plan_combo(combo: &str) -> Result<KeyPlan> {
    let localized = crate::platform::input::localize_combo(combo);
    let c = crate::keys::parse_combo(&localized)?;
    let lower = localized.to_lowercase();
    let explicit_shift = lower.split('+').any(|t| matches!(t.trim(), "shift" | "shift_l" | "shift_r"));
    let vk = c.main.key_code as u32;
    let ctrl = c.mods & MOD_CTRL != 0;
    let alt = c.mods & MOD_ALT != 0;
    // 0x5B/0x5C = left/right Windows key
    if c.mods & MOD_META != 0 || vk == 0x5B || vk == 0x5C {
        bail!("Windows 키 조합은 앱에 보낼 수 없습니다 (시작 메뉴·시스템 단축키는 사용자 세션 전체에 작용)");
    }
    // menu mode, the system menu and context menus draw popups on the user's screen
    if vk == 18 || (alt && !ctrl && vk == 0x20) {
        bail!("Alt 키(메뉴 모드)와 Alt+Space(창 메뉴)는 메뉴를 사용자 화면에 띄우므로 보낼 수 없습니다 — 메뉴는 app_control menus / menu 를 쓰세요");
    }
    if vk == 0x79 && !ctrl && !alt {
        bail!("F10(메뉴 모드)은 메뉴를 사용자 화면에 띄우므로 보낼 수 없습니다 — 메뉴는 app_control menus / menu 를 쓰세요");
    }
    if vk == 0x5D || (vk == 0x79 && explicit_shift) {
        bail!("컨텍스트 메뉴 키(Apps, Shift+F10)는 메뉴를 실제 마우스 커서 위치(사용자 화면)에 띄우므로 보낼 수 없습니다");
    }
    if vk == menu::VK_F1 && !ctrl && !alt {
        bail!("F1(도움말)은 도움말·브라우저를 사용자 화면 앞에 띄우므로 보내지 않았습니다");
    }
    if vk == 17 || vk == 16 {
        return Ok(KeyPlan::BareModifier);
    }
    // Shift+Insert / Shift+Delete are paste/cut in edit controls → emulate as shortcuts
    if ctrl || alt || (explicit_shift && matches!(vk, 0x2D | 0x2E)) {
        return Ok(KeyPlan::Shortcut(Accel { ctrl, alt, shift: explicit_shift, vk }));
    }
    // character keys (including with shift only) → text
    if let Some(t) = c.main.text.as_ref() {
        if vk != 13 && !t.is_empty() {
            return Ok(KeyPlan::Text(t.clone()));
        }
    }
    if vk == 0 {
        bail!("보낼 수 없는 키입니다: {combo}");
    }
    Ok(KeyPlan::Key { vk, shift: c.mods & MOD_SHIFT != 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geom_mapping() {
        let g = Geom { hwnd: 1, client_w: 2000, client_h: 1000, shot_w: 1000, shot_h: 500 };
        assert_eq!(g.to_client(10.0, 20.0), (20, 40));
        assert_eq!(g.to_client(999.9, 499.9), (1999, 999));
        assert!(g.contains_shot(0.0, 0.0));
        assert!(!g.contains_shot(1000.0, 10.0));
        // client origin (5000, 3000), screen rect (5100,3050)-(5300,3150) → screenshot [50,25,100,50]
        assert_eq!(g.rect_to_shot((5000, 3000), (5100, 3050, 5300, 3150)), [50, 25, 100, 50]);
        assert_eq!(wheel_notches(300.0), 3);
        assert_eq!(wheel_notches(-100.0), -1);
        assert_eq!(wheel_notches(30.0), 1);
        assert_eq!(wheel_notches(0.2), 0);
    }

    #[test]
    fn key_lparams() {
        // 'A' down: repeat 1, scan 0x1E
        assert_eq!(key_lparam(0x1E, false, false, false), 0x001E_0001);
        // Delete up: extended + previous state + transition
        assert_eq!(key_lparam(0x53, true, true, false) as u32, 0xC153_0001);
        // Alt context
        assert_eq!(key_lparam(0x3E, false, false, true) as u32, 0x203E_0001);
        assert!(is_extended_vk(0x2E));
        assert!(is_extended_vk(0x25));
        assert!(!is_extended_vk(0x41));
        assert!(!is_extended_vk(0x0D));
    }

    #[test]
    fn text_sanitizing() {
        assert_eq!(sanitize_text("a\r\nb\rc"), ("a\nb\nc".to_string(), 0));
        let (s, n) = sanitize_text("x\u{3}y\u{16}z\u{18}\t");
        assert_eq!(s, "xyz\t");
        assert_eq!(n, 3);
        assert_eq!(sanitize_text("한글 ABC"), ("한글 ABC".to_string(), 0));
    }

    #[test]
    fn combo_plans() {
        assert_eq!(plan_combo("a").unwrap(), KeyPlan::Text("a".into()));
        assert_eq!(plan_combo("shift+a").unwrap(), KeyPlan::Text("A".into()));
        assert_eq!(plan_combo("Return").unwrap(), KeyPlan::Key { vk: 13, shift: false });
        assert_eq!(plan_combo("Delete").unwrap(), KeyPlan::Key { vk: 46, shift: false });
        assert_eq!(plan_combo("ctrl+s").unwrap(), KeyPlan::Shortcut(Accel { ctrl: true, alt: false, shift: false, vk: 0x53 }));
        // macOS-style cmd becomes ctrl
        assert_eq!(plan_combo("cmd+shift+s").unwrap(), KeyPlan::Shortcut(Accel { ctrl: true, alt: false, shift: true, vk: 0x53 }));
        assert_eq!(plan_combo("alt+F4").unwrap(), KeyPlan::Shortcut(Accel { ctrl: false, alt: true, shift: false, vk: 0x73 }));
        assert_eq!(plan_combo("ctrl").unwrap(), KeyPlan::BareModifier);
        for bad in ["alt", "F10", "shift+F10", "alt+space", "super+e", "win+r", "F1"] {
            assert!(plan_combo(bad).is_err(), "{bad}");
        }
    }
}
