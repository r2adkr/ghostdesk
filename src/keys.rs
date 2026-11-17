//! Converts xdotool-style key names ("ctrl+shift+a", "Return", "Page_Down") into
//! CDP `Input.dispatchKeyEvent` parameters.

use anyhow::{anyhow, Result};

pub const MOD_ALT: i64 = 1;
pub const MOD_CTRL: i64 = 2;
pub const MOD_META: i64 = 4;
pub const MOD_SHIFT: i64 = 8;

#[derive(Clone, Debug)]
pub struct KeyDef {
    pub key: String,
    pub code: String,
    pub key_code: i64,
    /// Text inserted when the key is pressed without modifiers.
    pub text: Option<String>,
    /// The modifier bit, if this key is itself a modifier.
    pub modifier_bit: i64,
    /// Whether typing this character needs shift (capitals, `!@#`, …).
    pub needs_shift: bool,
}

impl KeyDef {
    fn named(key: &str, code: &str, key_code: i64) -> Self {
        KeyDef {
            key: key.into(),
            code: code.into(),
            key_code,
            text: None,
            modifier_bit: 0,
            needs_shift: false,
        }
    }
    fn with_text(mut self, t: &str) -> Self {
        self.text = Some(t.into());
        self
    }
    fn modifier(mut self, bit: i64) -> Self {
        self.modifier_bit = bit;
        self
    }
}

pub struct Combo {
    pub modifiers: Vec<KeyDef>,
    pub main: KeyDef,
    pub mods: i64,
}

fn named_key(name: &str) -> Option<KeyDef> {
    let n = name.to_ascii_lowercase();
    let k = match n.as_str() {
        "return" | "enter" | "kp_enter" => KeyDef::named("Enter", "Enter", 13).with_text("\r"),
        "tab" => KeyDef::named("Tab", "Tab", 9),
        "escape" | "esc" => KeyDef::named("Escape", "Escape", 27),
        "backspace" => KeyDef::named("Backspace", "Backspace", 8),
        "delete" | "del" | "kp_delete" => KeyDef::named("Delete", "Delete", 46),
        "space" => KeyDef::named(" ", "Space", 32).with_text(" "),
        "up" | "arrowup" | "kp_up" => KeyDef::named("ArrowUp", "ArrowUp", 38),
        "down" | "arrowdown" | "kp_down" => KeyDef::named("ArrowDown", "ArrowDown", 40),
        "left" | "arrowleft" | "kp_left" => KeyDef::named("ArrowLeft", "ArrowLeft", 37),
        "right" | "arrowright" | "kp_right" => KeyDef::named("ArrowRight", "ArrowRight", 39),
        "home" | "kp_home" => KeyDef::named("Home", "Home", 36),
        "end" | "kp_end" => KeyDef::named("End", "End", 35),
        "page_up" | "pageup" | "prior" => KeyDef::named("PageUp", "PageUp", 33),
        "page_down" | "pagedown" | "next" => KeyDef::named("PageDown", "PageDown", 34),
        "insert" => KeyDef::named("Insert", "Insert", 45),
        "ctrl" | "control" | "control_l" | "control_r" | "ctrl_l" | "ctrl_r" => {
            KeyDef::named("Control", "ControlLeft", 17).modifier(MOD_CTRL)
        }
        "shift" | "shift_l" | "shift_r" => KeyDef::named("Shift", "ShiftLeft", 16).modifier(MOD_SHIFT),
        "alt" | "option" | "alt_l" | "alt_r" => KeyDef::named("Alt", "AltLeft", 18).modifier(MOD_ALT),
        "super" | "super_l" | "super_r" | "cmd" | "command" | "meta" | "meta_l" | "win" => {
            KeyDef::named("Meta", "MetaLeft", 91).modifier(MOD_META)
        }
        "plus" => return char_key('+'),
        "minus" => return char_key('-'),
        "comma" => return char_key(','),
        "period" => return char_key('.'),
        "slash" => return char_key('/'),
        _ => {
            if let Some(num) = n.strip_prefix('f').and_then(|s| s.parse::<i64>().ok()) {
                if (1..=12).contains(&num) {
                    return Some(KeyDef::named(&format!("F{num}"), &format!("F{num}"), 111 + num));
                }
            }
            return None;
        }
    };
    Some(k)
}

/// Key definition for a single printable ASCII character.
pub fn char_key(c: char) -> Option<KeyDef> {
    if !c.is_ascii() || c.is_ascii_control() {
        return None;
    }
    if c == ' ' {
        return named_key("space");
    }
    if c.is_ascii_alphabetic() {
        let up = c.to_ascii_uppercase();
        return Some(KeyDef {
            key: c.to_string(),
            code: format!("Key{up}"),
            key_code: up as i64,
            text: Some(c.to_string()),
            modifier_bit: 0,
            needs_shift: c.is_ascii_uppercase(),
        });
    }
    if c.is_ascii_digit() {
        return Some(KeyDef {
            key: c.to_string(),
            code: format!("Digit{c}"),
            key_code: c as i64,
            text: Some(c.to_string()),
            modifier_bit: 0,
            needs_shift: false,
        });
    }
    let shifted = [
        ('!', '1'), ('@', '2'), ('#', '3'), ('$', '4'), ('%', '5'), ('^', '6'), ('&', '7'),
        ('*', '8'), ('(', '9'), (')', '0'), ('_', '-'), ('+', '='), ('{', '['), ('}', ']'),
        ('|', '\\'), (':', ';'), ('"', '\''), ('<', ','), ('>', '.'), ('?', '/'), ('~', '`'),
    ];
    let (base, needs_shift) = match shifted.iter().find(|(s, _)| *s == c) {
        Some((_, b)) => (*b, true),
        None => (c, false),
    };
    let (code, kc) = match base {
        '1'..='9' | '0' => (format!("Digit{base}"), base as i64),
        '-' => ("Minus".into(), 189),
        '=' => ("Equal".into(), 187),
        '[' => ("BracketLeft".into(), 219),
        ']' => ("BracketRight".into(), 221),
        '\\' => ("Backslash".into(), 220),
        ';' => ("Semicolon".into(), 186),
        '\'' => ("Quote".into(), 222),
        ',' => ("Comma".into(), 188),
        '.' => ("Period".into(), 190),
        '/' => ("Slash".into(), 191),
        '`' => ("Backquote".into(), 192),
        _ => (String::new(), 0),
    };
    Some(KeyDef {
        key: c.to_string(),
        code,
        key_code: kc,
        text: Some(c.to_string()),
        modifier_bit: 0,
        needs_shift,
    })
}

pub fn parse_combo(combo: &str) -> Result<Combo> {
    let combo = combo.trim();
    // handle "ctrl++", where '+' itself is the key
    let parts: Vec<String> = if combo.ends_with("++") {
        let mut v: Vec<String> = combo[..combo.len() - 2]
            .split('+')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        v.push("+".into());
        v
    } else if combo == "+" {
        vec!["+".into()]
    } else {
        combo.split('+').map(|s| s.trim().to_string()).collect()
    };
    if parts.is_empty() {
        return Err(anyhow!("빈 키 조합"));
    }
    let mut defs = Vec::new();
    for p in &parts {
        let d = if p.chars().count() == 1 {
            char_key(p.chars().next().unwrap())
        } else {
            named_key(p)
        }
        .ok_or_else(|| anyhow!("알 수 없는 키 이름: {p}"))?;
        defs.push(d);
    }
    let main = defs.pop().unwrap();
    let mut mods = defs.iter().fold(0, |acc, d| acc | d.modifier_bit);
    if main.needs_shift {
        mods |= MOD_SHIFT;
    }
    // a single letter with shift is sent as the capital
    let mut main = main;
    if mods & MOD_SHIFT != 0 && main.key.len() == 1 {
        let c = main.key.chars().next().unwrap();
        if c.is_ascii_lowercase() {
            main.key = c.to_ascii_uppercase().to_string();
            main.text = Some(main.key.clone());
        }
    }
    Ok(Combo {
        modifiers: defs,
        main,
        mods,
    })
}

/// On macOS, Chrome handles Cmd shortcuts through NSResponder, so editing shortcuts sent over
/// CDP have to name the editing commands explicitly in `commands`.
pub fn mac_edit_commands(combo: &Combo) -> Vec<&'static str> {
    if !cfg!(target_os = "macos") || combo.mods & (MOD_CTRL | MOD_META) == 0 {
        return vec![];
    }
    match combo.main.key.to_ascii_lowercase().as_str() {
        "a" => vec!["selectAll"],
        "c" => vec!["copy"],
        "v" => vec!["paste"],
        "x" => vec!["cut"],
        "z" if combo.mods & MOD_SHIFT != 0 => vec!["redo"],
        "z" => vec!["undo"],
        "y" => vec!["redo"],
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn combos() {
        let c = parse_combo("ctrl+shift+a").unwrap();
        assert_eq!(c.mods, MOD_CTRL | MOD_SHIFT);
        assert_eq!(c.main.key, "A");
        let c = parse_combo("Return").unwrap();
        assert_eq!(c.main.key_code, 13);
        let c = parse_combo("ctrl++").unwrap();
        assert_eq!(c.main.key, "+");
        assert!(parse_combo("Page_Down").is_ok());
        assert!(parse_combo("F5").is_ok());
        assert!(parse_combo("super").is_ok());
    }
}
