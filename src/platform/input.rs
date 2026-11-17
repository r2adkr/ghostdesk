//! Per-OS interpretation of key combos and modifiers.
//!
//! Agents (especially models used to macOS) often send Command combos like "cmd+c". When driving
//! a backend running **on this machine** (headless browser, Electron, local apps) outside macOS,
//! those become the equivalent Ctrl combos. Remote (VNC) and separate-session screens follow the
//! other machine's OS and are left alone. Explicit Windows-key choices (`super`, `win`, `meta`)
//! are kept as is.

use std::borrow::Cow;

use crate::keys::{MOD_CTRL, MOD_META};

/// Whether this OS needs cmd → ctrl remapping.
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

/// Localizes a key combo for local backends ("cmd+shift+p" → "ctrl+shift+p"; unchanged on
/// macOS).
pub fn localize_combo(combo: &str) -> Cow<'_, str> {
    localize_combo_for(combo, REMAP_CMD)
}

pub fn localize_combo_for(combo: &str, remap: bool) -> Cow<'_, str> {
    if !remap {
        return Cow::Borrowed(combo);
    }
    let trimmed = combo.trim();
    // split off a trailing '+' key as in "cmd++" (same rule as keys::parse_combo)
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
            // if the last token (the key itself) is cmd, press Ctrl
            if has_ctrl && !last {
                continue; // drop a duplicate ctrl
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

/// Localizes mouse modifier bits: Meta (cmd) → Ctrl outside macOS.
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

/// Turns keys that egui converted into `Copy`/`Cut` events during takeover back into a combo to
/// send to the isolated screen.
///
/// egui-winit turns cmd/ctrl+C into Copy and cmd/ctrl+X into Cut without reporting the original
/// key (no Key event either). On Windows the legacy clipboard keys ctrl+Insert → Copy and
/// shift+Delete → Cut become the same events.
/// - macOS and others: the primary modifier + c/x, as before
/// - Windows: if the person pressed Insert/Delete, keep that combo, so ctrl+Insert in a remote
///   Linux terminal doesn't become ctrl+c (SIGINT) and shift+Delete (permanent delete) in
///   Explorer doesn't become ctrl+x
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

/// Whether a key is down as of the last key message this thread read (GetKeyState, which is in
/// sync with the message queue).
#[cfg(windows)]
fn win_key_down(vk: i32) -> bool {
    #[link(name = "user32")]
    extern "system" {
        fn GetKeyState(nvirtkey: i32) -> i16;
    }
    unsafe { GetKeyState(vk) < 0 }
}

/// egui-winit's Windows rules: Cut = Cut key | ctrl alone + X | shift (+ any modifier) + Delete;
/// Copy = Copy key | ctrl alone + C | ctrl (+ any modifier) + Insert.
///
/// Decides by modifiers when they are enough, and only checks key state for ctrl alone (ctrl+C
/// vs ctrl+Insert). Returns `None` for anything but the legacy clipboard keys (primary modifier
/// + c/x).
#[cfg(any(windows, test))]
fn legacy_clipboard_combo(copy: bool, m: eframe::egui::Modifiers, down: impl Fn(i32) -> bool) -> Option<String> {
    let (legacy, key) = if copy {
        // ctrl+C is only Copy with ctrl alone, so ctrl with shift or alt means ctrl+Insert
        (m.ctrl && (!m.command_only() || (down(VK_INSERT) && !down(VK_C))), "Insert")
    } else {
        // ctrl+X is only Cut with ctrl alone, so with shift held it's shift+Delete (incl. ctrl+shift+Delete)
        (m.shift, "Delete")
    };
    if !legacy {
        return None;
    }
    // same order and names as egui_key_combo in app.rs
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
        // ctrl+C / ctrl+X → unchanged (None = primary modifier + c/x)
        assert_eq!(legacy_clipboard_combo(true, ctrl, c_down), None);
        assert_eq!(legacy_clipboard_combo(true, ctrl, none), None);
        assert_eq!(legacy_clipboard_combo(false, ctrl, none), None);
        // ctrl+Insert (told apart by key state), ctrl+shift+Insert and ctrl+alt+Insert (by modifiers)
        assert_eq!(legacy_clipboard_combo(true, ctrl, insert_down).as_deref(), Some("ctrl+Insert"));
        assert_eq!(legacy_clipboard_combo(true, ctrl_shift, none).as_deref(), Some("ctrl+shift+Insert"));
        assert_eq!(legacy_clipboard_combo(true, ctrl_alt, none).as_deref(), Some("ctrl+alt+Insert"));
        // shift+Delete, ctrl+shift+Delete
        assert_eq!(legacy_clipboard_combo(false, shift, none).as_deref(), Some("shift+Delete"));
        assert_eq!(legacy_clipboard_combo(false, ctrl_shift, none).as_deref(), Some("ctrl+shift+Delete"));
        // Copy/Cut keys without modifiers → unchanged
        assert_eq!(legacy_clipboard_combo(true, Modifiers::default(), none), None);
        assert_eq!(legacy_clipboard_combo(false, Modifiers::default(), none), None);
        // results parse with keys::parse_combo
        for c in ["ctrl+Insert", "ctrl+shift+Delete"] {
            assert!(crate::keys::parse_combo(c).is_ok(), "{c}");
        }
        // macOS keeps cmd+c / cmd+x
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
        // results must parse with keys::parse_combo
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
