//! CGEvent 입력 합성.
//!
//! 두 부류가 있다:
//! 1. `pid_*` — `CGEventPostToPid` 로 **특정 프로세스에만** 이벤트를 보낸다. 전역 커서·포커스·
//!    다른 앱에는 영향이 없다. 우리가 띄운(또는 사용자가 동의한) 대상 pid 에만 쓸 것.
//!    숨겨진 비활성 앱에도 유니코드 글자(한글 포함)와 특수키(방향키·Return 등)는 전달된다.
//!    Cmd 단축키와 마우스 클릭은 앱 내부 키 창 상태가 없으면 조용히 무시된다.
//! 2. `global::*` — `CGEventPost(kCGHIDEventTap)` 로 **현재 로그인 세션 전체**에 이벤트를 보낸다.
//!    실제 마우스 커서를 움직이고 front 앱에 키를 보낸다.
//!    **전용 에이전트 로그인 세션(세션 헬퍼) 안에서만 쓸 것. 사용자의 세션에서는 절대 부르지 말 것.**

use std::time::Duration;

use anyhow::{anyhow, Result};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMouseButton, CGScrollEventUnit,
};

use crate::keys::{MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT};

// ───────────────────────── 키 코드 ─────────────────────────

pub const KC_RETURN: u16 = 36;
pub const KC_TAB: u16 = 48;
pub const KC_SPACE: u16 = 49;
pub const KC_DELETE: u16 = 51; // Backspace
pub const KC_ESCAPE: u16 = 53;
pub const KC_COMMAND: u16 = 55;
pub const KC_SHIFT: u16 = 56;
pub const KC_OPTION: u16 = 58;
pub const KC_CONTROL: u16 = 59;
pub const KC_FORWARD_DELETE: u16 = 117;

/// ANSI 배열 기준 글자 → 가상 키코드 (shift 필요 여부 포함)
pub fn char_keycode(c: char) -> Option<(u16, bool)> {
    let lower = c.to_ascii_lowercase();
    let shift_letter = c.is_ascii_uppercase();
    let base: Option<u16> = match lower {
        'a' => Some(0), 's' => Some(1), 'd' => Some(2), 'f' => Some(3), 'h' => Some(4), 'g' => Some(5),
        'z' => Some(6), 'x' => Some(7), 'c' => Some(8), 'v' => Some(9), 'b' => Some(11), 'q' => Some(12),
        'w' => Some(13), 'e' => Some(14), 'r' => Some(15), 'y' => Some(16), 't' => Some(17), 'o' => Some(31),
        'u' => Some(32), 'i' => Some(34), 'p' => Some(35), 'l' => Some(37), 'j' => Some(38), 'k' => Some(40),
        'n' => Some(45), 'm' => Some(46),
        _ => None,
    };
    if let Some(k) = base {
        return Some((k, shift_letter));
    }
    let r = match c {
        '1' => (18, false), '2' => (19, false), '3' => (20, false), '4' => (21, false), '6' => (22, false),
        '5' => (23, false), '=' => (24, false), '9' => (25, false), '7' => (26, false), '-' => (27, false),
        '8' => (28, false), '0' => (29, false), ']' => (30, false), '[' => (33, false), '\'' => (39, false),
        ';' => (41, false), '\\' => (42, false), ',' => (43, false), '/' => (44, false), '.' => (47, false),
        '`' => (50, false), ' ' => (49, false),
        '!' => (18, true), '@' => (19, true), '#' => (20, true), '$' => (21, true), '^' => (22, true),
        '%' => (23, true), '+' => (24, true), '(' => (25, true), '&' => (26, true), '_' => (27, true),
        '*' => (28, true), ')' => (29, true), '}' => (30, true), '{' => (33, true), '"' => (39, true),
        ':' => (41, true), '|' => (42, true), '<' => (43, true), '?' => (44, true), '>' => (47, true),
        '~' => (50, true),
        _ => return None,
    };
    Some(r)
}

/// 이름 붙은 특수키 → 키코드. xdotool/CDP(keys.rs) 와 같은 의미로: "BackSpace" = 51 (mac delete),
/// "Delete"/"KP_Delete"/"ForwardDelete" = 117 (앞으로 지우기).
pub fn named_keycode(name: &str) -> Option<u16> {
    let n = name.to_ascii_lowercase();
    let k = match n.as_str() {
        "return" | "enter" => 36,
        "kp_enter" => 76,
        "tab" => 48,
        "space" => 49,
        "backspace" => 51,
        "delete" | "del" | "forwarddelete" | "forward_delete" | "kp_delete" => 117,
        "escape" | "esc" => 53,
        "left" | "arrowleft" | "kp_left" => 123,
        "right" | "arrowright" | "kp_right" => 124,
        "down" | "arrowdown" | "kp_down" => 125,
        "up" | "arrowup" | "kp_up" => 126,
        "home" | "kp_home" => 115,
        "end" | "kp_end" => 119,
        "page_up" | "pageup" | "prior" => 116,
        "page_down" | "pagedown" | "next" => 121,
        "help" | "insert" => 114,
        "f1" => 122, "f2" => 120, "f3" => 99, "f4" => 118, "f5" => 96, "f6" => 97, "f7" => 98, "f8" => 100,
        "f9" => 101, "f10" => 109, "f11" => 103, "f12" => 111, "f13" => 105, "f14" => 107, "f15" => 113,
        "f16" => 106, "f17" => 64, "f18" => 79, "f19" => 80, "f20" => 90,
        "plus" => 24, "minus" => 27, "comma" => 43, "period" => 47, "slash" => 44, "equal" => 24,
        _ => return None,
    };
    Some(k)
}

fn modifier_bit(name: &str) -> Option<i64> {
    Some(match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "control_l" | "control_r" | "ctrl_l" | "ctrl_r" => MOD_CTRL,
        "shift" | "shift_l" | "shift_r" => MOD_SHIFT,
        "alt" | "option" | "alt_l" | "alt_r" | "opt" => MOD_ALT,
        "super" | "super_l" | "super_r" | "cmd" | "command" | "meta" | "meta_l" | "win" => MOD_META,
        _ => return None,
    })
}

pub fn mods_to_flags(mods: i64) -> CGEventFlags {
    let mut f = 0u64;
    if mods & MOD_SHIFT != 0 {
        f |= CGEventFlags::MaskShift.0;
    }
    if mods & MOD_CTRL != 0 {
        f |= CGEventFlags::MaskControl.0;
    }
    if mods & MOD_ALT != 0 {
        f |= CGEventFlags::MaskAlternate.0;
    }
    if mods & MOD_META != 0 {
        f |= CGEventFlags::MaskCommand.0;
    }
    CGEventFlags(f)
}

/// 파싱된 mac 키 조합
#[derive(Clone, Debug, PartialEq)]
pub struct MacCombo {
    /// 주 키코드 (수정키만 누르는 조합이면 해당 수정키 코드)
    pub keycode: u16,
    /// 주 키가 출력 가능한 글자라면 그 글자 (수정키 없는 상태 기준)
    pub ch: Option<char>,
    /// MOD_* 비트
    pub mods: i64,
}

impl MacCombo {
    pub fn flags(&self) -> CGEventFlags {
        mods_to_flags(self.mods)
    }
    /// Cmd/Ctrl/Alt 가 붙은 "단축키"인가
    pub fn is_shortcut(&self) -> bool {
        self.mods & (MOD_META | MOD_CTRL | MOD_ALT) != 0
    }
}

/// xdotool 스타일 조합("ctrl+a", "cmd+shift+z", "Return", "F5", "ctrl++")을 mac 키코드로.
pub fn parse_combo(combo: &str) -> Result<MacCombo> {
    let combo = combo.trim();
    let parts: Vec<String> = if combo == "+" {
        vec!["+".into()]
    } else if let Some(stripped) = combo.strip_suffix("++") {
        let mut v: Vec<String> = stripped.split('+').filter(|s| !s.is_empty()).map(String::from).collect();
        v.push("+".into());
        v
    } else {
        combo.split('+').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    };
    if parts.is_empty() {
        return Err(anyhow!("빈 키 조합"));
    }
    let mut mods = 0;
    let (last, rest) = parts.split_last().unwrap();
    for m in rest {
        mods |= modifier_bit(m).ok_or_else(|| anyhow!("알 수 없는 수정키: {m}"))?;
    }
    // 수정키만 누르는 경우 ("shift", "cmd")
    if let Some(bit) = modifier_bit(last) {
        let kc = match bit {
            MOD_META => KC_COMMAND,
            MOD_SHIFT => KC_SHIFT,
            MOD_ALT => KC_OPTION,
            _ => KC_CONTROL,
        };
        return Ok(MacCombo { keycode: kc, ch: None, mods: mods | bit });
    }
    if last.chars().count() == 1 {
        let c = last.chars().next().unwrap();
        if let Some((kc, shift)) = char_keycode(c) {
            if shift {
                mods |= MOD_SHIFT;
            }
            let ch = if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c };
            return Ok(MacCombo { keycode: kc, ch: Some(ch), mods });
        }
        // ANSI 표에 없는 글자 (한글 등) → 키코드 0 + 유니코드
        return Ok(MacCombo { keycode: 0, ch: Some(c), mods });
    }
    if let Some(kc) = named_keycode(last) {
        let ch = match kc {
            49 => Some(' '),
            _ => None,
        };
        return Ok(MacCombo { keycode: kc, ch, mods });
    }
    Err(anyhow!("알 수 없는 키 이름: {last}"))
}

// ───────────────────────── 대상 pid 로 보내기 ─────────────────────────

fn source() -> Option<CFRetained<CGEventSource>> {
    CGEventSource::new(CGEventSourceStateID::Private)
}

fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

fn set_unicode(ev: &CGEvent, units: &[u16]) {
    unsafe { CGEvent::keyboard_set_unicode_string(Some(ev), units.len() as _, units.as_ptr()) };
}

/// 텍스트를 유니코드 키 이벤트로 대상 pid 에 보낸다 (이벤트당 최대 20 UTF-16 단위).
/// '\n' 은 Return, '\t' 는 Tab 키코드로 보낸다. 한글도 IME 를 거치지 않고 그대로 들어간다.
pub fn pid_type_text(pid: i32, text: &str) -> Result<()> {
    let src = source();
    let mut chunk: Vec<u16> = Vec::new();
    let flush = |chunk: &mut Vec<u16>| -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        for down in [true, false] {
            let ev = CGEvent::new_keyboard_event(src.as_deref(), 0, down).ok_or_else(|| anyhow!("키 이벤트 생성 실패"))?;
            set_unicode(&ev, chunk);
            CGEvent::post_to_pid(pid, Some(&ev));
        }
        chunk.clear();
        sleep_ms(12);
        Ok(())
    };
    for c in text.chars() {
        match c {
            '\n' | '\r' => {
                flush(&mut chunk)?;
                pid_key_tap(pid, KC_RETURN, CGEventFlags(0), Some('\r'))?;
            }
            '\t' => {
                flush(&mut chunk)?;
                pid_key_tap(pid, KC_TAB, CGEventFlags(0), Some('\t'))?;
            }
            _ => {
                let mut buf = [0u16; 2];
                let u = c.encode_utf16(&mut buf);
                if chunk.len() + u.len() > 20 {
                    flush(&mut chunk)?;
                }
                chunk.extend_from_slice(u);
            }
        }
    }
    flush(&mut chunk)
}

/// 키 하나 down 또는 up
pub fn pid_key(pid: i32, keycode: u16, flags: CGEventFlags, ch: Option<char>, down: bool) -> Result<()> {
    let ev = CGEvent::new_keyboard_event(source().as_deref(), keycode, down).ok_or_else(|| anyhow!("키 이벤트 생성 실패"))?;
    CGEvent::set_flags(Some(&ev), flags);
    if let Some(c) = ch {
        let mut buf = [0u16; 2];
        set_unicode(&ev, c.encode_utf16(&mut buf));
    }
    CGEvent::post_to_pid(pid, Some(&ev));
    Ok(())
}

pub fn pid_key_tap(pid: i32, keycode: u16, flags: CGEventFlags, ch: Option<char>) -> Result<()> {
    pid_key(pid, keycode, flags, ch, true)?;
    sleep_ms(15);
    pid_key(pid, keycode, flags, ch, false)?;
    sleep_ms(10);
    Ok(())
}

fn modifier_keys(mods: i64) -> Vec<(u16, i64)> {
    let mut v = vec![];
    for (bit, kc) in [(MOD_META, KC_COMMAND), (MOD_CTRL, KC_CONTROL), (MOD_ALT, KC_OPTION), (MOD_SHIFT, KC_SHIFT)] {
        if mods & bit != 0 {
            v.push((kc, bit));
        }
    }
    v
}

fn pid_flags_changed(pid: i32, keycode: u16, mods_after: i64) -> Result<()> {
    let ev = CGEvent::new_keyboard_event(source().as_deref(), keycode, true).ok_or_else(|| anyhow!("키 이벤트 생성 실패"))?;
    CGEvent::set_type(Some(&ev), CGEventType::FlagsChanged);
    CGEvent::set_flags(Some(&ev), mods_to_flags(mods_after));
    CGEvent::post_to_pid(pid, Some(&ev));
    Ok(())
}

/// 조합 누르기: 수정키 down → 주 키 down → (hold) → 주 키 up → 수정키 up
pub fn pid_combo(pid: i32, c: &MacCombo, hold: Duration) -> Result<()> {
    let mod_keys = modifier_keys(c.mods);
    let main_is_modifier = matches!(c.keycode, KC_COMMAND | KC_SHIFT | KC_OPTION | KC_CONTROL);
    let mut cur = 0;
    for (kc, bit) in &mod_keys {
        if main_is_modifier && *kc == c.keycode {
            continue;
        }
        cur |= bit;
        pid_flags_changed(pid, *kc, cur)?;
        sleep_ms(8);
    }
    if main_is_modifier {
        pid_flags_changed(pid, c.keycode, c.mods)?;
        std::thread::sleep(hold.max(Duration::from_millis(15)));
        pid_flags_changed(pid, c.keycode, cur)?;
    } else {
        // 글자 키는 단축키가 아니면 유니코드 문자열도 실어 보내 입력기(한글 자판 등)와 무관하게 만든다
        let ch = c.ch.map(|ch| if c.mods & MOD_SHIFT != 0 { shifted(ch) } else { ch });
        pid_key(pid, c.keycode, c.flags(), ch, true)?;
        std::thread::sleep(hold.max(Duration::from_millis(15)));
        pid_key(pid, c.keycode, c.flags(), ch, false)?;
    }
    for (kc, bit) in mod_keys.iter().rev() {
        if main_is_modifier && *kc == c.keycode {
            continue;
        }
        cur &= !bit;
        sleep_ms(8);
        pid_flags_changed(pid, *kc, cur)?;
    }
    sleep_ms(10);
    Ok(())
}

fn shifted(c: char) -> char {
    if c.is_ascii_lowercase() {
        return c.to_ascii_uppercase();
    }
    let pairs = [
        ('1', '!'), ('2', '@'), ('3', '#'), ('4', '$'), ('5', '%'), ('6', '^'), ('7', '&'), ('8', '*'),
        ('9', '('), ('0', ')'), ('-', '_'), ('=', '+'), ('[', '{'), (']', '}'), ('\\', '|'), (';', ':'),
        ('\'', '"'), (',', '<'), ('.', '>'), ('/', '?'), ('`', '~'),
    ];
    pairs.iter().find(|(a, _)| *a == c).map(|(_, b)| *b).unwrap_or(c)
}

/// (실험적) 창-로컬 좌표를 실은 마우스 클릭을 대상 pid 에 보낸다.
///
/// macOS 27 에서는 mouseMoved 프라이머 + 필드(1,3,7,40,51,58,91,92) + 비공개
/// `CGEventSetWindowLocation`(창 기준 포인트 좌표)이 있어야 하고, 앱 내부 키 창 상태가
/// SkyLight 레코드로 설정되어 있어야 동작한다. **숨겨진(ordered-out) 창에는 동작하지 않는다.**
/// global = 전역 포인트 좌표, local = 창 좌상단 기준 포인트 좌표.
pub fn pid_click_window(pid: i32, wid: u32, global: CGPoint, local: CGPoint, right: bool, count: i64) -> Result<()> {
    type SetWinLoc = unsafe extern "C" fn(*const CGEvent, CGPoint);
    let setloc = super::skylight::sym("CGEventSetWindowLocation");
    let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState);
    let (down_t, up_t, btn) = if right {
        (CGEventType::RightMouseDown, CGEventType::RightMouseUp, CGMouseButton::Right)
    } else {
        (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp, CGMouseButton::Left)
    };
    let mk = |t: CGEventType, cs: i64| -> Result<CFRetained<CGEvent>> {
        let ev = CGEvent::new_mouse_event(src.as_deref(), t, global, btn).ok_or_else(|| anyhow!("마우스 이벤트 생성 실패"))?;
        for (f, v) in [(1u32, cs), (3, 0), (7, 3), (51, wid as i64), (58, 424242), (91, wid as i64), (92, wid as i64), (40, pid as i64)] {
            CGEvent::set_integer_value_field(Some(&ev), CGEventField(f), v);
        }
        if !setloc.is_null() {
            let f: SetWinLoc = unsafe { std::mem::transmute(setloc) };
            unsafe { f(&*ev, local) };
        }
        Ok(ev)
    };
    CGEvent::post_to_pid(pid, Some(&*mk(CGEventType::MouseMoved, 0)?));
    sleep_ms(30);
    for i in 1..=count.max(1) {
        CGEvent::post_to_pid(pid, Some(&*mk(down_t, i)?));
        sleep_ms(28);
        CGEvent::post_to_pid(pid, Some(&*mk(up_t, i)?));
        sleep_ms(60);
    }
    Ok(())
}

// ───────────────────────── 세션 전역 (헬퍼 전용) ─────────────────────────

/// **전용 에이전트 로그인 세션(세션 헬퍼) 안에서만 쓸 것. 사용자의 세션에서 부르면
/// 사용자의 실제 커서를 움직이고 사용자가 보고 있는 앱에 입력이 들어간다.**
pub mod global {
    use super::*;

    fn src() -> Option<CFRetained<CGEventSource>> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
    }

    fn post(ev: &CGEvent) {
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(ev));
    }

    /// 마우스 버튼 종류: 0=왼쪽, 1=오른쪽, 2=가운데
    fn button_types(button: u8, dragging: bool) -> (CGEventType, CGEventType, CGEventType, CGMouseButton) {
        match button {
            1 => (CGEventType::RightMouseDown, CGEventType::RightMouseUp, CGEventType::RightMouseDragged, CGMouseButton::Right),
            2 => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, CGEventType::OtherMouseDragged, CGMouseButton::Center),
            _ => {
                let _ = dragging;
                (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp, CGEventType::LeftMouseDragged, CGMouseButton::Left)
            }
        }
    }

    /// 커서 이동 (전역 포인트 좌표). pressed = 누르고 있는 버튼 (드래그)
    pub fn post_mouse_move(x: f64, y: f64, pressed: Option<u8>, mods: i64) -> Result<()> {
        let p = CGPoint { x, y };
        let (t, btn) = match pressed {
            Some(b) => {
                let (_, _, drag, btn) = button_types(b, true);
                (drag, btn)
            }
            None => (CGEventType::MouseMoved, CGMouseButton::Left),
        };
        let ev = CGEvent::new_mouse_event(src().as_deref(), t, p, btn).ok_or_else(|| anyhow!("마우스 이벤트 생성 실패"))?;
        CGEvent::set_flags(Some(&ev), mods_to_flags(mods));
        post(&ev);
        Ok(())
    }

    /// 버튼 down/up (click_count = 1,2,3)
    pub fn post_mouse_button(x: f64, y: f64, button: u8, down: bool, click_count: i64, mods: i64) -> Result<()> {
        let p = CGPoint { x, y };
        let (dt, ut, _, btn) = button_types(button, false);
        let ev = CGEvent::new_mouse_event(src().as_deref(), if down { dt } else { ut }, p, btn)
            .ok_or_else(|| anyhow!("마우스 이벤트 생성 실패"))?;
        CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventClickState, click_count.max(1));
        CGEvent::set_flags(Some(&ev), mods_to_flags(mods));
        post(&ev);
        Ok(())
    }

    /// 클릭 (count 회)
    pub fn post_click(x: f64, y: f64, button: u8, count: i64, mods: i64) -> Result<()> {
        post_mouse_move(x, y, None, mods)?;
        sleep_ms(20);
        for i in 1..=count.max(1) {
            post_mouse_button(x, y, button, true, i, mods)?;
            sleep_ms(30);
            post_mouse_button(x, y, button, false, i, mods)?;
            sleep_ms(50);
        }
        Ok(())
    }

    /// 스크롤 (픽셀 단위, dy>0 = 아래로)
    pub fn post_scroll(dx: f64, dy: f64) -> Result<()> {
        let ev = CGEvent::new_scroll_wheel_event2(src().as_deref(), CGScrollEventUnit::Pixel, 2, (-dy) as i32, (-dx) as i32, 0)
            .ok_or_else(|| anyhow!("스크롤 이벤트 생성 실패"))?;
        post(&ev);
        Ok(())
    }

    /// 키 하나 down/up
    pub fn post_key(keycode: u16, down: bool, mods: i64) -> Result<()> {
        let ev = CGEvent::new_keyboard_event(src().as_deref(), keycode, down).ok_or_else(|| anyhow!("키 이벤트 생성 실패"))?;
        CGEvent::set_flags(Some(&ev), mods_to_flags(mods));
        post(&ev);
        Ok(())
    }

    /// 조합 키 ("cmd+shift+z")
    pub fn post_combo(combo: &str, hold: Duration, check: &dyn Fn() -> Result<()>) -> Result<()> {
        let c = parse_combo(combo)?;
        let mod_keys = modifier_keys(c.mods);
        let mut cur = 0;
        let mut pressed = Vec::new();
        let mut main_down = false;
        let mut result = (|| {
            for (kc, bit) in &mod_keys {
                if *kc == c.keycode {
                    continue;
                }
                check()?;
                cur |= bit;
                post_key(*kc, true, cur)?;
                pressed.push((*kc, *bit));
                sleep_ms(8);
            }
            check()?;
            post_key(c.keycode, true, c.mods)?;
            main_down = true;
            crate::session::hold_checked(hold.max(Duration::from_millis(15)), check)
        })();
        if main_down {
            result = result.and(post_key(c.keycode, false, c.mods));
        }
        for (kc, bit) in pressed.iter().rev() {
            cur &= !bit;
            sleep_ms(8);
            result = result.and(post_key(*kc, false, cur));
        }
        result
    }

    /// 유니코드 텍스트 (이벤트당 최대 20 UTF-16 단위, 한글 포함)
    pub fn post_unicode(text: &str, check: &dyn Fn() -> Result<()>) -> Result<()> {
        let s = src();
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut i = 0;
        while i < units.len() {
            check()?;
            let mut end = (i + 20).min(units.len());
            // 서로게이트 쌍을 자르지 않는다
            if end < units.len() && (0xD800..0xDC00).contains(&units[end - 1]) {
                end -= 1;
            }
            for down in [true, false] {
                let ev = CGEvent::new_keyboard_event(s.as_deref(), 0, down).ok_or_else(|| anyhow!("키 이벤트 생성 실패"))?;
                set_unicode(&ev, &units[i..end]);
                post(&ev);
            }
            sleep_ms(12);
            i = end;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn combos() {
        let c = parse_combo("cmd+shift+z").unwrap();
        assert_eq!(c.keycode, 6);
        assert_eq!(c.mods, MOD_META | MOD_SHIFT);
        assert_eq!(parse_combo("Return").unwrap().keycode, 36);
        assert_eq!(parse_combo("BackSpace").unwrap().keycode, 51);
        assert_eq!(parse_combo("Delete").unwrap().keycode, 117);
        assert_eq!(parse_combo("ctrl++").unwrap().keycode, 24);
        assert_eq!(parse_combo("A").unwrap().mods, MOD_SHIFT);
        assert_eq!(parse_combo("F5").unwrap().keycode, 96);
        assert!(parse_combo("cmd").unwrap().mods & MOD_META != 0);
        assert!(parse_combo("ctrl+a").unwrap().is_shortcut());
    }
}
