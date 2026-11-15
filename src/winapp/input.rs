//! 실제 마우스 커서·키보드 없이 창 메시지로 입력하기.
//!
//! - 마우스: 스크린샷 좌표 → 창 내용 좌표 → 그 점의 가장 깊은 (보이는) 자식 창 → 그 자식 기준 좌표로
//!   WM_MOUSEMOVE / WM_LBUTTONDOWN / WM_LBUTTONUP (/ WM_LBUTTONDBLCLK) 를 PostMessage 한다.
//!   사용자의 커서는 움직이지 않는다. 드래그 중 GetCursorPos 로 실제 커서를 읽는 앱은 따라오지 않을 수 있다.
//! - 휠: WM_MOUSEWHEEL / WM_MOUSEHWHEEL (lParam 은 화면 좌표 = 화면 밖 좌표).
//! - 글자: WM_CHAR (UTF-16 단위마다 하나, 입력기를 거치지 않으므로 한글도 그대로 들어간다).
//! - 이름 있는 키: WM_KEYDOWN / WM_KEYUP (반복 1, 스캔 코드, 확장 키 비트, 전이 비트를 맞춘 lParam).
//!   보낸 메시지는 앱의 GetKeyState 를 바꾸지 않으므로 Ctrl/Alt/Shift 조합은 키 메시지로 흉내 낼 수 없다
//!   (단축키는 메뉴 가속기·편집 명령 흉내로 처리한다 — workspace 참고).

use anyhow::{bail, Result};

use super::menu::{self, Accel};
use super::w32::{self, Hwnd};
use crate::keys::{MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT};

/// 스크린샷 픽셀 ↔ 창 내용 좌표 (캡처 시점 기준)
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
    /// 스크린샷 좌표 → 창 내용 좌표 (실제 픽셀)
    pub fn to_client(&self, x: f64, y: f64) -> (i32, i32) {
        ((x * self.sx()).floor() as i32, (y * self.sy()).floor() as i32)
    }
    pub fn contains_shot(&self, x: f64, y: f64) -> bool {
        x >= 0.0 && y >= 0.0 && x < self.shot_w as f64 && y < self.shot_h as f64
    }
    /// 화면 좌표 사각형(l,t,r,b) → 스크린샷 좌표 [x, y, w, h]. client_origin = 지금 내용 영역 원점의 화면 좌표.
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

/// 점(top 의 내용 좌표) 아래의 가장 깊은 보이는 자식 창과 그 창 기준 좌표.
/// 그룹 상자처럼 투명한 부분은 뒤의 컨트롤로 넘어간다 (RealChildWindowFromPoint).
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

/// 왼쪽 버튼 클릭 (count 2 = 더블클릭: 창 클래스가 CS_DBLCLKS 면 WM_LBUTTONDBLCLK)
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

/// 휠 (delta: 한 칸 = 120, 양수 = 위/오른쪽). screen_pt 는 화면 좌표.
pub fn post_wheel(target: Hwnd, screen_pt: (i32, i32), delta: i32, horizontal: bool, mk: usize) {
    let msg = if horizontal { w32::WM_MOUSEHWHEEL } else { w32::WM_MOUSEWHEEL };
    let w = w32::make_wparam(mk as u32, delta as i16 as u16 as u32);
    w32::post(target, msg, w, w32::make_lparam(screen_pt.0, screen_pt.1));
}

/// 스크롤 픽셀(에이전트 단위: 한 번 = 100px) → 휠 칸 수 (부호 유지, 최소 1칸)
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

// ───────────────────────── 키보드 ─────────────────────────

pub fn is_extended_vk(vk: u32) -> bool {
    matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0x5B | 0x5C | 0x5D | 0x6F | 0x90 | 0xA3 | 0xA5)
}

/// WM_KEYDOWN/WM_KEYUP 의 lParam: 반복 1, 스캔 코드, 확장 키 비트(24), Alt 문맥(29), 이전 상태(30)·전이(31)
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

/// 입력에 넣으면 안 되는 제어 문자를 뺀다 (\x03·\x16·\x18 은 편집 컨트롤에서 복사·붙여넣기·잘라내기다).
/// \r\n·\r 은 \n 으로. (정리한 글자, 뺀 글자 수)
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

/// 글자 입력: 줄바꿈·탭은 키(앱의 TranslateMessage 가 글자를 만든다), 나머지는 WM_CHAR. 보낸 글자 수.
///
/// TranslateMessage 가 만든 WM_CHAR 는 대기열 **끝**에 붙으므로, Enter/Tab 뒤에는 `flush` 로 앱이 대기열을
/// 비울 때까지 기다린 다음 나머지 글자를 보낸다 (안 그러면 줄바꿈이 뒤로 밀린다).
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

/// 키 조합을 어떻게 보낼지
#[derive(Clone, Debug, PartialEq)]
pub enum KeyPlan {
    /// 글자 (수정키 없음 / shift 만) → WM_CHAR
    Text(String),
    /// 이름 있는 키 (수정키 없음) → WM_KEYDOWN/UP. shift = 사용자가 shift 를 함께 지정함 (반영되지 않을 수 있음)
    Key { vk: u32, shift: bool },
    /// Ctrl/Alt 조합 → 흉내 (편집 명령·메뉴 가속기)
    Shortcut(Accel),
    /// 수정키 하나만 (ctrl·shift) → 아무 효과 없음
    BareModifier,
}

/// 에이전트의 키 조합 → 보내는 방법. 사용자 화면에 메뉴·창을 띄우는 조합은 이유와 함께 거부한다.
pub fn plan_combo(combo: &str) -> Result<KeyPlan> {
    let localized = crate::platform::input::localize_combo(combo);
    let c = crate::keys::parse_combo(&localized)?;
    let lower = localized.to_lowercase();
    let explicit_shift = lower.split('+').any(|t| matches!(t.trim(), "shift" | "shift_l" | "shift_r"));
    let vk = c.main.key_code as u32;
    let ctrl = c.mods & MOD_CTRL != 0;
    let alt = c.mods & MOD_ALT != 0;
    if c.mods & MOD_META != 0 || vk == 0x5B || vk == 91 {
        bail!("Windows 키 조합은 앱에 보낼 수 없습니다 (시작 메뉴·시스템 단축키는 사용자 세션 전체에 작용)");
    }
    // 메뉴 모드·시스템 메뉴·컨텍스트 메뉴는 사용자 화면에 팝업을 그린다
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
    // Shift+Insert / Shift+Delete 는 편집 컨트롤의 붙여넣기·잘라내기다 → 단축키 흉내로
    if ctrl || alt || (explicit_shift && matches!(vk, 0x2D | 0x2E)) {
        return Ok(KeyPlan::Shortcut(Accel { ctrl, alt, shift: explicit_shift, vk }));
    }
    // 글자 키 (shift 만 붙은 경우 포함) → 글자로
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
        // 내용 원점 (5000, 3000) 에서 화면 사각형 (5100,3050)-(5300,3150) → 스크린샷 [50,25,100,50]
        assert_eq!(g.rect_to_shot((5000, 3000), (5100, 3050, 5300, 3150)), [50, 25, 100, 50]);
        assert_eq!(wheel_notches(300.0), 3);
        assert_eq!(wheel_notches(-100.0), -1);
        assert_eq!(wheel_notches(30.0), 1);
        assert_eq!(wheel_notches(0.2), 0);
    }

    #[test]
    fn key_lparams() {
        // 'A' 누름: 반복 1, 스캔 0x1E
        assert_eq!(key_lparam(0x1E, false, false, false), 0x001E_0001);
        // Delete 뗌: 확장 + 이전 상태 + 전이
        assert_eq!(key_lparam(0x53, true, true, false) as u32, 0xC153_0001);
        // Alt 문맥
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
        // macOS 식 cmd 는 ctrl 로
        assert_eq!(plan_combo("cmd+shift+s").unwrap(), KeyPlan::Shortcut(Accel { ctrl: true, alt: false, shift: true, vk: 0x53 }));
        assert_eq!(plan_combo("alt+F4").unwrap(), KeyPlan::Shortcut(Accel { ctrl: false, alt: true, shift: false, vk: 0x73 }));
        assert_eq!(plan_combo("ctrl").unwrap(), KeyPlan::BareModifier);
        for bad in ["alt", "F10", "shift+F10", "alt+space", "super+e", "win+r", "F1"] {
            assert!(plan_combo(bad).is_err(), "{bad}");
        }
    }
}
