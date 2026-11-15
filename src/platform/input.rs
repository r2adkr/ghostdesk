//! 키 조합·수정키의 운영체제별 해석.
//!
//! 에이전트(특히 macOS 에 익숙한 모델)는 "cmd+c" 처럼 Command 키를 쓰는 경우가 많다.
//! macOS 가 아닌 곳에서 **이 컴퓨터 안의** 백엔드(헤드리스 브라우저·Electron·로컬 앱)를 조작할 때는
//! 같은 뜻의 Ctrl 로 바꿔 준다. 원격(VNC)·별도 세션은 상대 컴퓨터의 OS 를 따르므로 바꾸지 않는다.
//! `super`/`win`/`meta` 처럼 명시적으로 Windows 키를 고른 경우는 그대로 둔다.

use std::borrow::Cow;

use crate::keys::{MOD_CTRL, MOD_META};

/// 이 OS 에서 cmd → ctrl 치환이 필요한가
pub const REMAP_CMD: bool = !cfg!(target_os = "macos");

fn is_cmd_token(t: &str) -> bool {
    matches!(t.trim().to_ascii_lowercase().as_str(), "cmd" | "command" | "cmd_l" | "cmd_r")
}

fn is_ctrl_token(t: &str) -> bool {
    matches!(
        t.trim().to_ascii_lowercase().as_str(),
        "ctrl" | "control" | "control_l" | "control_r" | "ctrl_l" | "ctrl_r"
    )
}

/// 로컬 백엔드용 키 조합 현지화 ("cmd+shift+p" → "ctrl+shift+p", macOS 에서는 그대로).
pub fn localize_combo(combo: &str) -> Cow<'_, str> {
    localize_combo_for(combo, REMAP_CMD)
}

pub fn localize_combo_for(combo: &str, remap: bool) -> Cow<'_, str> {
    if !remap {
        return Cow::Borrowed(combo);
    }
    let trimmed = combo.trim();
    // "cmd++" 처럼 '+' 자체를 누르는 꼬리는 따로 떼어 둔다 (keys::parse_combo 와 같은 규칙)
    let (body, tail) = if trimmed.ends_with("++") {
        (&trimmed[..trimmed.len() - 2], "++")
    } else {
        (trimmed, "")
    };
    let parts: Vec<&str> = body.split('+').collect();
    if !parts.iter().any(|p| is_cmd_token(p)) {
        return Cow::Borrowed(combo);
    }
    let has_ctrl = parts.iter().any(|p| is_ctrl_token(p));
    let mut out: Vec<&str> = Vec::with_capacity(parts.len());
    for (i, p) in parts.iter().enumerate() {
        let last = i + 1 == parts.len() && tail.is_empty();
        if is_cmd_token(p) {
            // 마지막 토큰(= 누르는 키 자체)이 cmd 이면 Ctrl 키를 누르는 것으로
            if has_ctrl && !last {
                continue; // ctrl 이 이미 있으면 중복 제거
            }
            out.push("ctrl");
        } else {
            out.push(p);
        }
    }
    let mut s = out.join("+");
    if !tail.is_empty() {
        s.push_str(tail);
    }
    Cow::Owned(s)
}

/// 마우스 수정키 비트 현지화: macOS 가 아니면 Meta(cmd) → Ctrl.
pub fn localize_mods(mods: i64) -> i64 {
    localize_mods_for(mods, REMAP_CMD)
}

pub fn localize_mods_for(mods: i64, remap: bool) -> i64 {
    if remap && mods & MOD_META != 0 {
        (mods & !MOD_META) | MOD_CTRL
    } else {
        mods
    }
}

/// 직접 조작 중 egui 가 `Copy`/`Cut` 이벤트로 바꿔 버린 키를 격리 화면에 보낼 조합으로 되돌린다.
///
/// egui-winit 은 cmd/ctrl+C → Copy, cmd/ctrl+X → Cut 으로 바꾸고 원래 키는 알려 주지 않는다 (Key 이벤트도 없음).
/// Windows 에서는 옛 클립보드 키 ctrl+Insert → Copy, shift+Delete → Cut 도 같은 이벤트가 된다.
/// - macOS 등: 예전처럼 기본 수정키 + c/x
/// - Windows: 사람이 Insert/Delete 로 눌렀으면 그 조합 그대로 (원격 리눅스 터미널의 ctrl+Insert 가 ctrl+c(SIGINT)로,
///   탐색기의 shift+Delete(영구 삭제)가 ctrl+x 로 바뀌지 않게)
pub fn takeover_clipboard_combo(copy: bool, m: eframe::egui::Modifiers) -> String {
    #[cfg(windows)]
    if let Some(c) = legacy_clipboard_combo(copy, m, win_key_down) {
        return c;
    }
    #[cfg(not(windows))]
    let _ = m;
    format!("{}+{}", super::text::PRIMARY_MOD, if copy { "c" } else { "x" })
}

#[cfg(any(windows, test))]
const VK_INSERT: i32 = 0x2D;
#[cfg(any(windows, test))]
const VK_C: i32 = 0x43;

/// 이 스레드가 마지막으로 읽은 키 메시지 기준으로 키가 눌려 있는가 (GetKeyState: 메시지와 맞춰진 상태)
#[cfg(windows)]
fn win_key_down(vk: i32) -> bool {
    #[link(name = "user32")]
    extern "system" {
        fn GetKeyState(nvirtkey: i32) -> i16;
    }
    unsafe { GetKeyState(vk) < 0 }
}

/// egui-winit 의 Windows 규칙: Cut = Cut 키 | ctrl 단독 + X | shift(+아무 수정키) + Delete,
/// Copy = Copy 키 | ctrl 단독 + C | ctrl(+아무 수정키) + Insert.
/// 수정키만으로 구별되면 그것으로 정하고, ctrl 단독(ctrl+C 와 ctrl+Insert)일 때만 키 상태를 본다.
/// 옛 클립보드 키가 아니면 None (기본 수정키 + c/x).
#[cfg(any(windows, test))]
fn legacy_clipboard_combo(copy: bool, m: eframe::egui::Modifiers, down: impl Fn(i32) -> bool) -> Option<String> {
    let (legacy, key) = if copy {
        // ctrl+C 는 ctrl 단독일 때만 Copy 가 되므로, shift·alt 가 함께면 ctrl+Insert
        (m.ctrl && (!m.command_only() || (down(VK_INSERT) && !down(VK_C))), "Insert")
    } else {
        // ctrl+X 는 ctrl 단독일 때만 Cut 이 되므로, shift 가 눌려 있으면 shift+Delete (ctrl+shift+Delete 포함)
        (m.shift, "Delete")
    };
    if !legacy {
        return None;
    }
    // app.rs 의 egui_key_combo 와 같은 순서·이름
    let mut s = String::new();
    if m.mac_cmd {
        s.push_str("cmd+");
    }
    if m.ctrl {
        s.push_str("ctrl+");
    }
    if m.alt {
        s.push_str("alt+");
    }
    if m.shift {
        s.push_str("shift+");
    }
    s.push_str(key);
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takeover_clipboard_keys() {
        use eframe::egui::Modifiers;
        let ctrl = Modifiers { ctrl: true, command: true, ..Default::default() };
        let shift = Modifiers { shift: true, ..Default::default() };
        let ctrl_shift = Modifiers { ctrl: true, command: true, shift: true, ..Default::default() };
        let ctrl_alt = Modifiers { ctrl: true, command: true, alt: true, ..Default::default() };
        let none = |_: i32| false;
        let insert_down = |vk: i32| vk == VK_INSERT;
        let c_down = |vk: i32| vk == VK_C;
        // ctrl+C / ctrl+X → 기존대로 (None = 기본 수정키 + c/x)
        assert_eq!(legacy_clipboard_combo(true, ctrl, c_down), None);
        assert_eq!(legacy_clipboard_combo(true, ctrl, none), None);
        assert_eq!(legacy_clipboard_combo(false, ctrl, none), None);
        // ctrl+Insert (키 상태로 구별), ctrl+shift+Insert·ctrl+alt+Insert (수정키로 구별)
        assert_eq!(legacy_clipboard_combo(true, ctrl, insert_down).as_deref(), Some("ctrl+Insert"));
        assert_eq!(legacy_clipboard_combo(true, ctrl_shift, none).as_deref(), Some("ctrl+shift+Insert"));
        assert_eq!(legacy_clipboard_combo(true, ctrl_alt, none).as_deref(), Some("ctrl+alt+Insert"));
        // shift+Delete, ctrl+shift+Delete
        assert_eq!(legacy_clipboard_combo(false, shift, none).as_deref(), Some("shift+Delete"));
        assert_eq!(legacy_clipboard_combo(false, ctrl_shift, none).as_deref(), Some("ctrl+shift+Delete"));
        // 수정키 없는 Copy/Cut 키 → 기존대로
        assert_eq!(legacy_clipboard_combo(true, Modifiers::default(), none), None);
        assert_eq!(legacy_clipboard_combo(false, Modifiers::default(), none), None);
        // 결과는 keys::parse_combo 로 해석 가능
        for c in ["ctrl+Insert", "ctrl+shift+Delete"] {
            assert!(crate::keys::parse_combo(c).is_ok(), "{c}");
        }
        // macOS 는 예전과 같은 cmd+c / cmd+x
        if cfg!(target_os = "macos") {
            let cmd = Modifiers { mac_cmd: true, command: true, ..Default::default() };
            assert_eq!(takeover_clipboard_combo(true, cmd), "cmd+c");
            assert_eq!(takeover_clipboard_combo(false, cmd), "cmd+x");
        }
    }

    #[test]
    fn combos() {
        let l = |c| localize_combo_for(c, true).into_owned();
        assert_eq!(l("cmd+c"), "ctrl+c");
        assert_eq!(l("Command+Shift+P"), "ctrl+Shift+P");
        assert_eq!(l("cmd+ctrl+f"), "ctrl+f");
        assert_eq!(l("ctrl+cmd+f"), "ctrl+f");
        assert_eq!(l("cmd++"), "ctrl++");
        assert_eq!(l("cmd"), "ctrl");
        assert_eq!(l("super+e"), "super+e");
        assert_eq!(l("win+r"), "win+r");
        assert_eq!(l("Return"), "Return");
        assert_eq!(l("ctrl+c"), "ctrl+c");
        assert_eq!(localize_combo_for("cmd+c", false), "cmd+c");
        // 결과는 keys::parse_combo 로 해석 가능해야 한다
        for c in ["cmd+c", "cmd+shift+p", "cmd++", "cmd+ctrl+f", "cmd"] {
            let out = l(c);
            let parsed = crate::keys::parse_combo(&out).unwrap();
            assert_eq!(parsed.mods & MOD_META, 0, "{c} → {out}");
        }
    }

    #[test]
    fn mods() {
        assert_eq!(localize_mods_for(MOD_META, true), MOD_CTRL);
        assert_eq!(localize_mods_for(MOD_META | MOD_CTRL, true), MOD_CTRL);
        assert_eq!(localize_mods_for(MOD_META | crate::keys::MOD_SHIFT, true), MOD_CTRL | crate::keys::MOD_SHIFT);
        assert_eq!(localize_mods_for(MOD_META, false), MOD_META);
    }
}
