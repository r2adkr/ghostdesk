//! 고전 Win32 메뉴(HMENU): 읽기, 단축키(가속기) 글자 해석, WM_COMMAND 로 실행, 안전 규칙.
//!
//! 메뉴 막대는 화면 밖 창의 비작업 영역에 있으므로 열지 않는다(팝업 메뉴는 사용자 모니터 안으로 끌려 들어와 그려진다).
//! 대신 GetMenu/GetMenuItemInfoW 로 구조를 읽고(다른 프로세스의 메뉴도 읽힌다) 항목 id 를 WM_COMMAND 로 보낸다.
//! 백그라운드 창에 보낸 Ctrl 키 메시지는 앱의 GetKeyState 에 반영되지 않아 단축키가 먹지 않으므로,
//! 단축키는 같은 가속기 글자("Ctrl+S")를 가진 메뉴 항목을 찾아 실행하는 방식으로 흉내 낸다.

use windows_sys::Win32::UI::WindowsAndMessaging as wm;

use super::w32::{self, Hwnd};

const MIIM_STATE: u32 = 0x0001;
const MIIM_ID: u32 = 0x0002;
const MIIM_SUBMENU: u32 = 0x0004;
const MIIM_STRING: u32 = 0x0040;
const MIIM_FTYPE: u32 = 0x0100;
const MFT_SEPARATOR: u32 = 0x0800;
const MFT_OWNERDRAW: u32 = 0x0100;
const MFS_DISABLED: u32 = 0x0003;
const MFS_CHECKED: u32 = 0x0008;

pub const VK_BACK: u32 = 0x08;
pub const VK_TAB: u32 = 0x09;
pub const VK_RETURN: u32 = 0x0D;
pub const VK_PAUSE: u32 = 0x13;
pub const VK_ESCAPE: u32 = 0x1B;
pub const VK_SPACE: u32 = 0x20;
pub const VK_PRIOR: u32 = 0x21;
pub const VK_NEXT: u32 = 0x22;
pub const VK_END: u32 = 0x23;
pub const VK_HOME: u32 = 0x24;
pub const VK_LEFT: u32 = 0x25;
pub const VK_UP: u32 = 0x26;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_DOWN: u32 = 0x28;
pub const VK_INSERT: u32 = 0x2D;
pub const VK_DELETE: u32 = 0x2E;
pub const VK_ADD: u32 = 0x6B;
pub const VK_SUBTRACT: u32 = 0x6D;
pub const VK_F1: u32 = 0x70;
pub const VK_OEM_PLUS: u32 = 0xBB;
pub const VK_OEM_COMMA: u32 = 0xBC;
pub const VK_OEM_MINUS: u32 = 0xBD;
pub const VK_OEM_PERIOD: u32 = 0xBE;

/// 단축키 하나 (가상 키 + 수정키)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accel {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub vk: u32,
}

impl Accel {
    pub fn label(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("Ctrl+");
        }
        if self.alt {
            s.push_str("Alt+");
        }
        if self.shift {
            s.push_str("Shift+");
        }
        s.push_str(&vk_name(self.vk));
        s
    }
}

pub fn vk_name(vk: u32) -> String {
    match vk {
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).map(|c| c.to_string()).unwrap_or_default(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        VK_DELETE => "Del".into(),
        VK_INSERT => "Ins".into(),
        VK_RETURN => "Enter".into(),
        VK_ESCAPE => "Esc".into(),
        VK_TAB => "Tab".into(),
        VK_SPACE => "Space".into(),
        VK_BACK => "Backspace".into(),
        VK_HOME => "Home".into(),
        VK_END => "End".into(),
        VK_PRIOR => "PgUp".into(),
        VK_NEXT => "PgDn".into(),
        VK_LEFT => "Left".into(),
        VK_RIGHT => "Right".into(),
        VK_UP => "Up".into(),
        VK_DOWN => "Down".into(),
        VK_OEM_PLUS => "+".into(),
        VK_OEM_MINUS => "-".into(),
        VK_ADD => "Num +".into(),
        VK_SUBTRACT => "Num -".into(),
        other => format!("VK{other:#04x}"),
    }
}

fn key_token_vk(t: &str) -> Option<u32> {
    let l = t.trim().to_lowercase();
    if l.chars().count() == 1 {
        let c = l.chars().next()?;
        return match c {
            'a'..='z' => Some(c.to_ascii_uppercase() as u32),
            '0'..='9' => Some(c as u32),
            '+' | '=' => Some(VK_OEM_PLUS),
            '-' => Some(VK_OEM_MINUS),
            ',' => Some(VK_OEM_COMMA),
            '.' => Some(VK_OEM_PERIOD),
            '/' => Some(0xBF),
            ';' => Some(0xBA),
            '[' => Some(0xDB),
            ']' => Some(0xDD),
            '\\' => Some(0xDC),
            '\'' => Some(0xDE),
            '`' => Some(0xC0),
            _ => None,
        };
    }
    if let Some(n) = l.strip_prefix('f').and_then(|s| s.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(VK_F1 + n - 1);
        }
    }
    Some(match l.as_str() {
        "del" | "delete" | "entf" | "suppr" | "삭제" => VK_DELETE,
        "ins" | "insert" | "einfg" | "inser" => VK_INSERT,
        "enter" | "return" | "eingabe" | "entrée" => VK_RETURN,
        "esc" | "escape" | "echap" => VK_ESCAPE,
        "tab" => VK_TAB,
        "space" | "spacebar" | "leertaste" | "espace" => VK_SPACE,
        "backspace" | "bksp" | "back" | "rücktaste" => VK_BACK,
        "home" | "pos1" | "début" => VK_HOME,
        "end" | "ende" | "fin" => VK_END,
        "pgup" | "page up" | "pageup" | "bild auf" | "bild-auf" | "prior" => VK_PRIOR,
        "pgdn" | "page down" | "pagedown" | "bild ab" | "bild-ab" | "next" => VK_NEXT,
        "left" | "left arrow" | "links" => VK_LEFT,
        "right" | "right arrow" | "rechts" => VK_RIGHT,
        "up" | "up arrow" | "nach-oben" => VK_UP,
        "down" | "down arrow" | "nach-unten" => VK_DOWN,
        "plus" => VK_OEM_PLUS,
        "minus" => VK_OEM_MINUS,
        "num +" | "num+" | "numpad +" | "num plus" => VK_ADD,
        "num -" | "num-" | "numpad -" | "num minus" => VK_SUBTRACT,
        "break" | "pause" => VK_PAUSE,
        _ => return None,
    })
}

/// 메뉴 항목의 단축키 글자("Ctrl+Shift+S", "Strg+S", "Del", "F5")를 해석한다
pub fn parse_accel(text: &str) -> Option<Accel> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    // 끝의 '+' 는 키 자체: "Ctrl++" → ["Ctrl", "+"], "Ctrl+Num +" → ["Ctrl", "Num +"]
    let mut raw: Vec<String> = t.split('+').map(|s| s.to_string()).collect();
    if t.ends_with('+') {
        raw.pop();
        if t.ends_with("++") {
            raw.pop();
            raw.push("+".into());
        } else if let Some(last) = raw.last_mut() {
            last.push('+');
        }
    }
    let mut parts: Vec<&str> = raw.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    let key = parts.pop()?;
    let mut a = Accel { ctrl: false, alt: false, shift: false, vk: key_token_vk(key)? };
    for m in parts {
        match m.to_lowercase().as_str() {
            "ctrl" | "control" | "ctl" | "strg" | "컨트롤" => a.ctrl = true,
            "shift" | "shft" | "umschalt" | "maj" | "시프트" => a.shift = true,
            "alt" | "알트" => a.alt = true,
            _ => return None,
        }
    }
    Some(a)
}

/// 메뉴 항목 하나
#[derive(Clone, Debug)]
pub struct Item {
    pub id: u32,
    /// '&' 를 뺀 이름
    pub title: String,
    /// '\t' 뒤의 단축키 글자 (있으면)
    pub accel_text: Option<String>,
    pub disabled: bool,
    pub checked: bool,
    pub separator: bool,
    pub owner_drawn: bool,
    pub sub: Option<isize>,
}

/// HMENU 의 항목들. owner 가 있으면 먼저 WM_INITMENUPOPUP 으로 활성 상태를 갱신하게 한다 (index = 상위 메뉴에서의 위치).
pub fn items(hmenu: isize, refresh: Option<(Hwnd, usize)>) -> Vec<Item> {
    if hmenu == 0 {
        return vec![];
    }
    if let Some((owner, idx)) = refresh {
        let _ = w32::send_timeout(owner, w32::WM_INITMENUPOPUP, hmenu as usize, idx as isize, 500);
    }
    let n = unsafe { wm::GetMenuItemCount(hmenu as _) };
    let mut out = Vec::new();
    for i in 0..n.clamp(0, 400) {
        let mut buf = vec![0u16; 512];
        let mut mii: wm::MENUITEMINFOW = unsafe { std::mem::zeroed() };
        mii.cbSize = std::mem::size_of::<wm::MENUITEMINFOW>() as u32;
        mii.fMask = MIIM_STATE | MIIM_ID | MIIM_SUBMENU | MIIM_STRING | MIIM_FTYPE;
        mii.dwTypeData = buf.as_mut_ptr();
        mii.cch = (buf.len() - 1) as u32;
        if unsafe { wm::GetMenuItemInfoW(hmenu as _, i as u32, 1, &mut mii) } == 0 {
            continue;
        }
        let raw = if mii.fType & (MFT_SEPARATOR | MFT_OWNERDRAW) == 0 {
            String::from_utf16_lossy(&buf[..(mii.cch as usize).min(buf.len())])
        } else {
            String::new()
        };
        let accel_text = raw.split_once('\t').map(|(_, a)| a.trim().to_string()).filter(|a| !a.is_empty());
        out.push(Item {
            id: mii.wID,
            title: w32::strip_mnemonic(&raw),
            accel_text,
            disabled: mii.fState & MFS_DISABLED != 0,
            checked: mii.fState & MFS_CHECKED != 0,
            separator: mii.fType & MFT_SEPARATOR != 0,
            owner_drawn: mii.fType & MFT_OWNERDRAW != 0,
            sub: (!mii.hSubMenu.is_null()).then_some(mii.hSubMenu as isize),
        });
    }
    out
}

/// 메뉴를 가진 창 (현재 창 → 소유자 순으로 처음 GetMenu 가 있는 창)
pub fn menu_owner(h: Hwnd) -> Option<(Hwnd, isize)> {
    let mut cur = h;
    for _ in 0..6 {
        if cur == 0 {
            break;
        }
        let m = w32::menu_of(cur);
        if m != 0 && w32::style(cur) & w32::WS_CHILD == 0 {
            return Some((cur, m));
        }
        cur = w32::owner(cur);
    }
    None
}

// ───────────── 안전 규칙 ─────────────

pub const R_HELP: &str = "도움말은 브라우저나 도움말 창을 사용자 화면 앞에 띄우므로 차단했습니다";
pub const R_CLIPBOARD: &str =
    "앱의 복사·잘라내기·붙여넣기는 사용자의 시스템 클립보드를 읽거나 바꾸므로 차단했습니다 — Ctrl+C/X/V 를 key 로 보내면 내부 클립보드로 흉내 냅니다";
pub const R_SYSTEM: &str = "이 항목은 창을 사용자 화면으로 옮기거나(최대화·이동) 세션 전체에 작용하므로 차단했습니다";
pub const R_MODAL: &str = "모달 대화상자가 열려 있어 본 창(메뉴를 가진 창)이 비활성이므로 실행하지 않았습니다 — 실제 키보드로도 대화상자가 \
    열린 동안에는 본 창의 메뉴·단축키가 동작하지 않습니다. 대화상자를 먼저 끝내거나 닫으세요 (app_control list_windows 로 확인)";

/// 메뉴 소유 창이 쓸 수 있는가: 모달 대화상자(DialogBox·MessageBox·공용 대화상자)가 열려 있으면 소유 창은 비활성이고,
/// 그 창에 WM_COMMAND 를 보내면 대화상자의 모달 루프 안에서 명령이 다시 실행된다 (저장·끝내기 중첩 → 자료 손상·충돌).
pub fn owner_usable(owner: Hwnd) -> bool {
    w32::is_enabled(owner)
}

const HELP_TITLES: [&str; 12] = ["help", "도움말", "?", "ヘルプ", "帮助", "幫助", "hilfe", "aide", "ayuda", "aiuto", "ajuda", "справка"];
const CLIP_WORDS: [&str; 12] = ["copy", "cut", "paste", "복사", "잘라내기", "붙여넣기", "붙여 넣기", "コピー", "切り取り", "貼り付け", "复制", "粘贴"];

pub fn norm(s: &str) -> String {
    let s = w32::strip_mnemonic(s);
    // "열기(O)..." → "열기", "Open..." → "open"
    let s = s.trim().trim_end_matches('…').trim_end_matches("...").trim();
    let s = match (s.rfind('('), s.ends_with(')')) {
        (Some(i), true) if s.len() - i <= 5 && i > 0 => &s[..i],
        _ => s,
    };
    s.trim().to_lowercase()
}

pub fn is_help_title(t: &str) -> bool {
    let n = norm(t);
    HELP_TITLES.contains(&n.as_str())
}

/// 이름이 시스템 클립보드를 쓰는 동작(복사·잘라내기·붙여넣기)인가
pub fn is_clipboard_title(t: &str) -> bool {
    let n = norm(t);
    CLIP_WORDS.iter().any(|w| n == *w || n.starts_with(&format!("{w} ")) || n.ends_with(&format!(" {w}")))
}

pub fn is_clipboard_accel(a: &Accel) -> bool {
    let c = (a.ctrl && !a.alt && matches!(a.vk, 0x43 | 0x58 | 0x56)) // Ctrl+C/X/V
        || (a.ctrl && a.vk == VK_INSERT) // Ctrl+Ins
        || (a.shift && !a.ctrl && a.vk == VK_INSERT) // Shift+Ins
        || (a.shift && !a.ctrl && a.vk == VK_DELETE); // Shift+Del
    c
}

/// 항목 차단 이유 (top_title = 최상위 메뉴 이름)
pub fn block_reason(top_title: &str, item: &Item) -> Option<&'static str> {
    if is_help_title(top_title) || is_help_title(&item.title) {
        return Some(R_HELP);
    }
    if is_clipboard_title(&item.title) || item.accel_text.as_deref().and_then(parse_accel).map(|a| is_clipboard_accel(&a)).unwrap_or(false) {
        return Some(R_CLIPBOARD);
    }
    None
}

type Found = (String, Item, Option<&'static str>);

/// 단축키를 가진 메뉴 항목을 찾는다: (경로, 메뉴 소유 창, 항목, 차단 이유).
/// 활성 항목·차단 항목을 먼저 돌려주고, 같은 단축키가 비활성 항목에만 있으면 그 항목을 돌려준다 (호출자가 '비활성'이라고 알림).
/// 소유 창이 비활성(모달 대화상자가 열림)이면 WM_INITMENU/WM_INITMENUPOPUP 도 보내지 않는다 — 호출자는 소유 창을
/// [`owner_usable`] 로 확인하고 실행하지 않아야 한다.
pub fn find_by_accel(h: Hwnd, want: &Accel) -> Option<(String, Hwnd, Item, Option<&'static str>)> {
    let (owner, bar) = menu_owner(h)?;
    let refresh = owner_usable(owner);
    fn walk(
        owner: Hwnd,
        refresh: bool,
        hm: isize,
        idx: usize,
        top: &str,
        path: &str,
        want: &Accel,
        depth: usize,
        disabled: &mut Option<Found>,
    ) -> Option<Found> {
        for (i, it) in items(hm, refresh.then_some((owner, idx))).into_iter().enumerate() {
            let p = if path.is_empty() { it.title.clone() } else { format!("{path} > {}", it.title) };
            if let Some(a) = it.accel_text.as_deref().and_then(parse_accel) {
                if a == *want {
                    let r = block_reason(top, &it);
                    if r.is_some() || !it.disabled {
                        return Some((p, it, r));
                    }
                    if disabled.is_none() {
                        *disabled = Some((p.clone(), it.clone(), None));
                    }
                }
            }
            if depth < 3 {
                if let Some(sub) = it.sub {
                    if let Some(x) = walk(owner, refresh, sub, i, top, &p, want, depth + 1, disabled) {
                        return Some(x);
                    }
                }
            }
        }
        None
    }
    if refresh {
        let _ = w32::send_timeout(owner, w32::WM_INITMENU, bar as usize, 0, 500);
    }
    let mut disabled: Option<Found> = None;
    for (i, top) in items(bar, None).into_iter().enumerate() {
        let Some(sub) = top.sub else { continue };
        let help = if is_help_title(&top.title) { Some(R_HELP) } else { None };
        if let Some((p, it, r)) = walk(owner, refresh, sub, i, &top.title, &top.title, want, 0, &mut disabled) {
            return Some((p, owner, it, r.or(help)));
        }
    }
    disabled.map(|(p, it, r)| (p, owner, it, r))
}

/// 메뉴 항목 실행 (WM_COMMAND)
pub fn invoke(owner: Hwnd, id: u32) -> bool {
    w32::post(owner, w32::WM_COMMAND, w32::make_wparam(id, 0), 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accel_parsing() {
        assert_eq!(parse_accel("Ctrl+S"), Some(Accel { ctrl: true, alt: false, shift: false, vk: 0x53 }));
        assert_eq!(parse_accel("Ctrl+Shift+S"), Some(Accel { ctrl: true, alt: false, shift: true, vk: 0x53 }));
        assert_eq!(parse_accel("Strg+N"), Some(Accel { ctrl: true, alt: false, shift: false, vk: 0x4E }));
        assert_eq!(parse_accel("Del"), Some(Accel { ctrl: false, alt: false, shift: false, vk: VK_DELETE }));
        assert_eq!(parse_accel("F5"), Some(Accel { ctrl: false, alt: false, shift: false, vk: 0x74 }));
        assert_eq!(parse_accel("Alt+F4"), Some(Accel { ctrl: false, alt: true, shift: false, vk: 0x73 }));
        assert_eq!(parse_accel("Ctrl++"), Some(Accel { ctrl: true, alt: false, shift: false, vk: VK_OEM_PLUS }));
        assert_eq!(parse_accel("Ctrl+Num +"), Some(Accel { ctrl: true, alt: false, shift: false, vk: VK_ADD }));
        assert_eq!(parse_accel("Ctrl+PgDn"), Some(Accel { ctrl: true, alt: false, shift: false, vk: VK_NEXT }));
        assert_eq!(parse_accel("Ctrl+1"), Some(Accel { ctrl: true, alt: false, shift: false, vk: 0x31 }));
        assert_eq!(parse_accel(""), None);
        assert_eq!(parse_accel("Hyper+Q"), None);
        assert_eq!(parse_accel("Ctrl+S").unwrap().label(), "Ctrl+S");
        assert_eq!(parse_accel("ctrl+shift+f12").unwrap().label(), "Ctrl+Shift+F12");
    }

    #[test]
    fn safety_rules() {
        assert!(is_help_title("도움말(&H)"));
        assert!(is_help_title("&Help"));
        assert!(is_help_title("?"));
        assert!(!is_help_title("파일(&F)"));
        assert!(is_clipboard_title("복사(&C)"));
        assert!(is_clipboard_title("&Paste\tCtrl+V"));
        assert!(is_clipboard_title("선택하여 붙여넣기"));
        assert!(!is_clipboard_title("저장(&S)"));
        assert!(!is_clipboard_title("복사본 저장"));
        assert!(is_clipboard_accel(&parse_accel("Ctrl+C").unwrap()));
        assert!(is_clipboard_accel(&parse_accel("Shift+Del").unwrap()));
        assert!(is_clipboard_accel(&parse_accel("Shift+Ins").unwrap()));
        assert!(!is_clipboard_accel(&parse_accel("Ctrl+S").unwrap()));
        assert!(!is_clipboard_accel(&parse_accel("Del").unwrap()));
        let it = Item {
            id: 1,
            title: "모두 선택".into(),
            accel_text: Some("Ctrl+A".into()),
            disabled: false,
            checked: false,
            separator: false,
            owner_drawn: false,
            sub: None,
        };
        assert_eq!(block_reason("편집", &it), None);
        assert_eq!(block_reason("도움말", &it), Some(R_HELP));
        let cp = Item { title: "복사".into(), accel_text: Some("Ctrl+C".into()), ..it.clone() };
        assert_eq!(block_reason("편집", &cp), Some(R_CLIPBOARD));
        assert_eq!(norm("열기(&O)..."), "열기");
        assert_eq!(norm("Save &As…"), "save as");
    }
}
