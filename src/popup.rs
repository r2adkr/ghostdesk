//! Pop-up window logic for Windows app mode (platform-neutral so it is tested on every OS).
//!
//! Menus and drop-down panels are separate top-level windows. This module decides which windows
//! count as such pop-ups ([`classify`]) and, as a small state machine ([`Tracker`]), when the
//! agent's current window should switch to a newly opened pop-up and when it should return to the
//! window it came from. It does no Win32 calls: the caller fills [`Probe`] and applies [`Plan`].
#![cfg_attr(not(windows), allow(dead_code))]

const WS_POPUP: u32 = 0x8000_0000;
const WS_CHILD: u32 = 0x4000_0000;
const WS_CAPTION: u32 = 0x00C0_0000;
const WS_THICKFRAME: u32 = 0x0004_0000;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const WS_EX_NOACTIVATE: u32 = 0x0800_0000;

/// Largest size (px per edge) a structurally detected pop-up may have; anything bigger is a real
/// window.
const MAX_POPUP_EDGE: i32 = 2000;

/// What the caller knows about a top-level window.
#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub class: String,
    pub title_empty: bool,
    pub w: i32,
    pub h: i32,
    pub style: u32,
    pub ex_style: u32,
    pub owned: bool,
    /// The caller knows this window is an input-method, shell or host window that is never a
    /// pop-up the agent should see; it only blocks the structural rule below.
    pub helper: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Kind {
    /// An ordinary window (or not recognised: the conservative default).
    #[default]
    None,
    /// Classic Win32 menu (#32768).
    Menu,
    /// Menu or panel an app draws itself (Qt, Chromium, WPF, ...).
    Popup,
    /// Combo-box drop-down list (still refused, never shown).
    Combo,
    /// Tooltip.
    Tip,
    /// Menu shadow.
    Shadow,
}

impl Kind {
    /// Pop-ups the agent may switch to.
    pub fn is_popup(self) -> bool {
        matches!(self, Kind::Menu | Kind::Popup)
    }

    /// Any transient helper window (the guard does not restyle these).
    pub fn is_transient(self) -> bool {
        self != Kind::None
    }

    pub fn label_ko(self) -> &'static str {
        match self {
            Kind::Menu => "메뉴",
            Kind::Popup => "팝업",
            _ => "",
        }
    }
}

/// Why a refresh of the window list happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// After an agent action or a screenshot request.
    Agent,
    /// The live view loop (must never move the agent to another window).
    Background,
}

#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub hwnd: isize,
    pub kind: Kind,
}

/// Decides which kind of window this is. Unknown windows are `Kind::None`, which keeps today's
/// behaviour.
pub fn classify(p: &Probe) -> Kind {
    let c = p.class.as_str();
    if c == "SysShadow" {
        return Kind::Shadow;
    }
    // toolkits prefix or suffix the class (WindowsForms10.tooltips_class32.app.0.<hash>, Qt...ToolTip...)
    if c.to_ascii_lowercase().contains("tooltip") {
        return Kind::Tip;
    }
    if c == "#32768" {
        return Kind::Menu;
    }
    if c == "ComboLBox" {
        return Kind::Combo;
    }
    let has_caption = p.style & WS_CAPTION == WS_CAPTION;
    // Qt5 and Qt6 embed the version in the class name, so match the middle part only
    if c.contains("QWindowPopup") && !has_caption {
        return Kind::Popup;
    }
    if p.style & WS_POPUP != 0
        && !has_caption
        && p.style & WS_THICKFRAME == 0
        && p.style & WS_CHILD == 0
        && p.owned
        && !p.helper
        && p.ex_style & (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) != 0
        && p.title_empty
        && p.w <= MAX_POPUP_EDGE
        && p.h <= MAX_POPUP_EDGE
    {
        return Kind::Popup;
    }
    Kind::None
}

/// Smallest size (w, h) a window of this kind must have to be listed.
pub fn min_size(kind: Kind) -> (i32, i32) {
    if kind.is_popup() {
        (24, 16)
    } else {
        (40, 30)
    }
}

/// What the caller must do after a window-list refresh.
#[derive(Default, Debug, PartialEq)]
pub struct Plan {
    pub switch_to: Option<isize>,
    /// Pop-ups adopted (announced) by this update.
    pub opened: Vec<isize>,
    /// The current window was a pop-up and is gone.
    pub closed_current: bool,
    /// `switch_to` goes back to the base window or to another pop-up after the current one closed.
    pub restored: bool,
}

/// Remembers which window the agent was on before a pop-up opened.
#[derive(Default, Clone, Debug)]
pub struct Tracker {
    base: Option<isize>,
    /// Pop-ups seen but not yet adopted (a background refresh saw them first).
    unadopted: Vec<isize>,
}

impl Tracker {
    pub fn base(&self) -> Option<isize> {
        self.base
    }

    /// The agent chose a window itself: a pop-up left open is not switched back to by itself.
    pub fn manual_switch(&mut self) {
        self.base = None;
        self.unadopted.clear();
    }

    pub fn update(&mut self, prev: &[Entry], now: &[Entry], current: Option<isize>, cause: Cause, normal_switch: Option<isize>) -> Plan {
        let popup_now: Vec<isize> = now.iter().filter(|e| e.kind.is_popup()).map(|e| e.hwnd).collect();
        let alive = |h: isize| popup_now.contains(&h);
        self.unadopted.retain(|h| alive(*h));
        // the first listing is not an opening: pop-ups already there are never adopted later
        for h in popup_now.iter().filter(|_| !prev.is_empty()) {
            if !prev.iter().any(|e| e.hwnd == *h) && !self.unadopted.contains(h) {
                self.unadopted.push(*h);
            }
        }
        // a new ordinary window (dialog) wins over a pop-up
        if let Some(h) = normal_switch {
            self.base = None;
            self.unadopted.clear();
            return Plan { switch_to: Some(h), ..Plan::default() };
        }
        if cause == Cause::Agent {
            if let Some(p) = popup_now.iter().copied().find(|h| self.unadopted.contains(h)) {
                let opened: Vec<isize> = popup_now.iter().copied().filter(|h| self.unadopted.contains(h)).collect();
                if let Some(c) = current {
                    let c_ok = now.iter().any(|e| e.hwnd == c && !e.kind.is_popup());
                    if c_ok {
                        self.base = Some(c);
                    }
                }
                self.unadopted.clear();
                return Plan { switch_to: Some(p), opened, ..Plan::default() };
            }
        }
        // return path: the current window was a pop-up that is gone
        let Some(c) = current else { return Plan::default() };
        let was_popup = prev.iter().any(|e| e.hwnd == c && e.kind.is_popup());
        if !was_popup || alive(c) {
            return Plan::default();
        }
        let base_ok = self.base.filter(|b| now.iter().any(|e| e.hwnd == *b && !e.kind.is_popup()));
        // another open pop-up (the menu a submenu came from) only for the agent's own refreshes
        let other_popup = if cause == Cause::Agent { popup_now.first().copied() } else { None };
        let target = other_popup.or(base_ok);
        if target.is_none_or(|t| !popup_now.contains(&t)) {
            self.base = None;
        }
        Plan { switch_to: target, opened: Vec::new(), closed_current: true, restored: target.is_some() }
    }
}

/// The item under a screen point: the name of the smallest rectangle containing it. Rectangles
/// are (left, top, right, bottom) in screen coordinates; left and top are inclusive, right and
/// bottom exclusive.
pub fn item_at<'a>(items: &'a [(String, (i32, i32, i32, i32))], pt: (i32, i32)) -> Option<&'a str> {
    items
        .iter()
        .filter(|(name, (l, t, r, b))| !name.is_empty() && pt.0 >= *l && pt.0 < *r && pt.1 >= *t && pt.1 < *b)
        .min_by_key(|(_, (l, t, r, b))| (*r as i64 - *l as i64) * (*b as i64 - *t as i64))
        .map(|(name, _)| name.as_str())
}

/// Note to the agent when a pop-up became the current window.
pub fn note_open(kind: Kind, class: &str, w: i32, h: i32, base_title: &str) -> String {
    let mut s = format!(
        "팝업({}) {class} {w}x{h} 열림 — 현재 창이 팝업으로 바뀌었습니다. \
         항목은 스크린샷에서 클릭하거나 ui_tree 후 press / choose 로 고르고, 닫으려면 Escape 키를 보내세요.",
        kind.label_ko()
    );
    if !base_title.is_empty() {
        s.push_str(&format!(" 원래 창 '{base_title}' 는 switch_window 로 볼 수 있고 팝업은 열린 채로 유지됩니다"));
    }
    s
}

/// Note to the agent when the pop-up it was on closed.
pub fn note_closed(base_title: &str, restored: bool) -> String {
    if restored {
        let t = if base_title.is_empty() { "(제목 없음)" } else { base_title };
        format!("팝업이 닫혔습니다 — '{t}' 창으로 돌아왔습니다")
    } else {
        "팝업이 닫혔습니다".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OVERLAPPED: u32 = 0x00CF_0000; // WS_OVERLAPPEDWINDOW

    fn probe(class: &str) -> Probe {
        Probe { class: class.into(), title_empty: true, w: 200, h: 300, ..Probe::default() }
    }

    fn e(hwnd: isize, kind: Kind) -> Entry {
        Entry { hwnd, kind }
    }

    #[test]
    fn classify_by_class() {
        assert_eq!(classify(&probe("#32768")), Kind::Menu);
        assert_eq!(classify(&probe("ComboLBox")), Kind::Combo);
        assert_eq!(classify(&probe("SysShadow")), Kind::Shadow);
        for c in [
            "tooltips_class32",
            "Qt5QWindowToolTipSaveBits",
            "Qt6QWindowToolTipDropShadow",
            "WindowsForms10.tooltips_class32.app.0.141b42a_r6_ad1",
            "ATL:tooltips_class32",
        ] {
            assert_eq!(classify(&probe(c)), Kind::Tip, "{c}");
        }
        for c in ["Qt5QWindowPopupDropShadowSaveBits", "Qt5152QWindowPopup", "Qt6QWindowPopup"] {
            assert_eq!(classify(&probe(c)), Kind::Popup, "{c}");
        }
        let mut p = probe("Qt5QWindowPopupDropShadowSaveBits");
        p.style = OVERLAPPED;
        assert_eq!(classify(&p), Kind::None);
        assert_eq!(classify(&probe("Qt5QWindowIcon")), Kind::None);
        let mut p = probe("Qt5QWindowToolSaveBits");
        p.style = WS_POPUP | WS_CAPTION;
        p.title_empty = false;
        p.owned = true;
        p.ex_style = WS_EX_TOOLWINDOW;
        assert_eq!(classify(&p), Kind::None);
    }

    #[test]
    fn classify_structural() {
        let mut p = probe("Chrome_WidgetWin_1");
        p.style = WS_POPUP;
        p.owned = true;
        p.ex_style = WS_EX_TOOLWINDOW;
        assert_eq!(classify(&p), Kind::Popup);
        p.ex_style = WS_EX_NOACTIVATE;
        assert_eq!(classify(&p), Kind::Popup);
        // caption
        let mut q = p.clone();
        q.style = OVERLAPPED;
        assert_eq!(classify(&q), Kind::None);
        // frameless but unowned, titled and big
        let mut q = probe("Chrome_WidgetWin_1");
        q.style = WS_POPUP;
        q.w = 1280;
        q.h = 800;
        q.title_empty = false;
        assert_eq!(classify(&q), Kind::None);
        // owned tool window with a title
        let mut q = p.clone();
        q.title_empty = false;
        assert_eq!(classify(&q), Kind::None);
        // huge
        let mut q = p.clone();
        q.w = 3000;
        q.h = 3000;
        assert_eq!(classify(&q), Kind::None);
        // child
        let mut q = p.clone();
        q.style = WS_POPUP | WS_CHILD;
        assert_eq!(classify(&q), Kind::None);
        // resizable
        let mut q = p.clone();
        q.style = WS_POPUP | WS_THICKFRAME;
        assert_eq!(classify(&q), Kind::None);
        // not a tool window at all
        let mut q = p.clone();
        q.ex_style = 0;
        assert_eq!(classify(&q), Kind::None);
        assert_eq!(classify(&probe("#32770")), Kind::None);
        assert_eq!(classify(&probe("Notepad")), Kind::None);
        // input-method / shell / host windows with popup-like style bits stay ordinary
        let mut q = p.clone();
        q.class = "MSCTFIME Composition".into();
        assert_eq!(classify(&q), Kind::Popup);
        q.helper = true;
        assert_eq!(classify(&q), Kind::None);
    }

    #[test]
    fn first_listing_is_not_an_opening() {
        let mut t = Tracker::default();
        let main = e(1, Kind::None);
        let now = [e(9, Kind::Popup), main];
        // initial listing (empty prev), then agent refreshes: the pop-up was already there
        assert_eq!(t.update(&[], &now, Some(1), Cause::Background, None), Plan::default());
        assert_eq!(t.update(&now, &now, Some(1), Cause::Agent, None), Plan::default());
        // a pop-up that opens afterwards is still adopted
        let later = [e(9, Kind::Popup), e(10, Kind::Menu), main];
        let p = t.update(&now, &later, Some(1), Cause::Agent, None);
        assert_eq!(p.switch_to, Some(10));
    }

    #[test]
    fn sizes_and_labels() {
        assert_eq!(min_size(Kind::Menu), (24, 16));
        assert_eq!(min_size(Kind::Popup), (24, 16));
        assert_eq!(min_size(Kind::None), (40, 30));
        assert!(Kind::Menu.is_popup() && Kind::Popup.is_popup());
        assert!(!Kind::Tip.is_popup() && !Kind::Combo.is_popup() && !Kind::Shadow.is_popup() && !Kind::None.is_popup());
        assert!(Kind::Tip.is_transient() && !Kind::None.is_transient());
        assert_eq!(Kind::Menu.label_ko(), "메뉴");
        assert_eq!(Kind::Popup.label_ko(), "팝업");
        assert_eq!(Kind::default(), Kind::None);
    }

    #[test]
    fn open_adopts_on_agent_refresh() {
        let mut t = Tracker::default();
        let prev = [e(1, Kind::None)];
        let now = [e(9, Kind::Popup), e(1, Kind::None)];
        let p = t.update(&prev, &now, Some(1), Cause::Agent, None);
        assert_eq!(p.switch_to, Some(9));
        assert_eq!(p.opened, vec![9]);
        assert!(!p.closed_current && !p.restored);
        assert_eq!(t.base(), Some(1));
    }

    #[test]
    fn background_never_switches_but_agent_adopts_later() {
        let mut t = Tracker::default();
        let prev = [e(1, Kind::None)];
        let now = [e(9, Kind::Menu), e(1, Kind::None)];
        let p = t.update(&prev, &now, Some(1), Cause::Background, None);
        assert_eq!(p, Plan::default());
        assert_eq!(t.base(), None);
        let p = t.update(&now, &now, Some(1), Cause::Agent, None);
        assert_eq!(p.switch_to, Some(9));
        assert_eq!(p.opened, vec![9]);
        assert_eq!(t.base(), Some(1));
        // adopted once only
        let p = t.update(&now, &now, Some(9), Cause::Agent, None);
        assert_eq!(p, Plan::default());
    }

    #[test]
    fn submenu_stack_returns_step_by_step() {
        let mut t = Tracker::default();
        let main = e(1, Kind::None);
        t.update(&[main], &[e(10, Kind::Menu), main], Some(1), Cause::Agent, None);
        assert_eq!(t.base(), Some(1));
        // submenu opens on top
        let prev = [e(10, Kind::Menu), main];
        let now = [e(11, Kind::Menu), e(10, Kind::Menu), main];
        let p = t.update(&prev, &now, Some(10), Cause::Agent, None);
        assert_eq!(p.switch_to, Some(11));
        assert_eq!(t.base(), Some(1));
        // submenu closes: back to the first menu
        let p = t.update(&now, &prev, Some(11), Cause::Agent, None);
        assert_eq!(p.switch_to, Some(10));
        assert!(p.closed_current && p.restored);
        assert_eq!(t.base(), Some(1));
        // first menu closes: back to the main window
        let p = t.update(&prev, &[main], Some(10), Cause::Agent, None);
        assert_eq!(p.switch_to, Some(1));
        assert!(p.closed_current && p.restored);
        assert_eq!(t.base(), None);
    }

    #[test]
    fn dialog_wins_over_popup() {
        let mut t = Tracker::default();
        let prev = [e(1, Kind::None)];
        let now = [e(9, Kind::Popup), e(5, Kind::None), e(1, Kind::None)];
        let p = t.update(&prev, &now, Some(1), Cause::Agent, Some(5));
        assert_eq!(p.switch_to, Some(5));
        assert!(p.opened.is_empty());
        assert_eq!(t.base(), None);
        // the popup is not adopted afterwards either
        let p = t.update(&now, &now, Some(5), Cause::Agent, None);
        assert_eq!(p, Plan::default());
    }

    #[test]
    fn base_died() {
        let mut t = Tracker::default();
        let main = e(1, Kind::None);
        t.update(&[main], &[e(9, Kind::Popup), main], Some(1), Cause::Agent, None);
        let prev = [e(9, Kind::Popup), main];
        let p = t.update(&prev, &[], Some(9), Cause::Agent, None);
        assert_eq!(p.switch_to, None);
        assert!(p.closed_current && !p.restored);
        assert_eq!(t.base(), None);
    }

    #[test]
    fn background_close_returns_to_base_only() {
        let mut t = Tracker::default();
        let main = e(1, Kind::None);
        t.update(&[main], &[e(9, Kind::Popup), main], Some(1), Cause::Agent, None);
        let prev = [e(9, Kind::Popup), e(10, Kind::Popup), main];
        let now = [e(10, Kind::Popup), main];
        let p = t.update(&prev, &now, Some(9), Cause::Background, None);
        assert_eq!(p.switch_to, Some(1));
        assert!(p.closed_current && p.restored);
    }

    #[test]
    fn manual_switch_stops_readoption() {
        let mut t = Tracker::default();
        let main = e(1, Kind::None);
        let now = [e(9, Kind::Popup), main];
        t.update(&[main], &now, Some(1), Cause::Agent, None);
        t.manual_switch();
        assert_eq!(t.base(), None);
        // still open and the agent is on the main window: nothing happens
        assert_eq!(t.update(&now, &now, Some(1), Cause::Agent, None), Plan::default());
        // a new one is adopted
        let now2 = [e(12, Kind::Menu), e(9, Kind::Popup), main];
        let p = t.update(&now, &now2, Some(1), Cause::Agent, None);
        assert_eq!(p.switch_to, Some(12));
        assert_eq!(p.opened, vec![12]);
    }

    #[test]
    fn flapping_popup_is_ignored_and_counts_again() {
        let mut t = Tracker::default();
        let main = e(1, Kind::None);
        // opened and closed between two refreshes: never in prev and now at once
        let p = t.update(&[main], &[main], Some(1), Cause::Agent, None);
        assert_eq!(p, Plan::default());
        let p = t.update(&[main], &[e(9, Kind::Popup), main], Some(1), Cause::Agent, None);
        assert_eq!(p.switch_to, Some(9));
    }

    #[test]
    fn non_popups_never_switch() {
        let mut t = Tracker::default();
        let main = e(1, Kind::None);
        let now = [e(2, Kind::Tip), e(3, Kind::Shadow), e(4, Kind::Combo), main];
        assert_eq!(t.update(&[main], &now, Some(1), Cause::Agent, None), Plan::default());
    }

    #[test]
    fn robust_with_odd_input() {
        let mut t = Tracker::default();
        assert_eq!(t.update(&[], &[], None, Cause::Agent, None), Plan::default());
        assert_eq!(t.update(&[], &[], Some(77), Cause::Background, None), Plan::default());
        assert_eq!(t.update(&[e(1, Kind::None)], &[e(1, Kind::None)], Some(77), Cause::Agent, None), Plan::default());
    }

    #[test]
    fn item_hit() {
        let items = vec![
            ("메뉴".to_string(), (0, 0, 100, 200)),
            ("복사".to_string(), (0, 20, 100, 40)),
            (String::new(), (0, 20, 100, 40)),
            ("작은".to_string(), (10, 22, 20, 30)),
        ];
        assert_eq!(item_at(&items, (50, 30)), Some("복사"));
        assert_eq!(item_at(&items, (15, 25)), Some("작은"));
        assert_eq!(item_at(&items, (50, 100)), Some("메뉴"));
        assert_eq!(item_at(&items, (150, 100)), None);
        // left/top inclusive, right/bottom exclusive
        assert_eq!(item_at(&items, (0, 20)), Some("복사"));
        assert_eq!(item_at(&items, (100, 30)), None);
        assert_eq!(item_at(&items, (50, 40)), Some("메뉴"));
        assert_eq!(item_at(&[], (1, 1)), None);
    }

    #[test]
    fn notes() {
        let n = note_open(Kind::Menu, "#32768", 180, 240, "Studio");
        assert!(n.contains("메뉴") && n.contains("180x240") && n.contains("'Studio'") && n.contains("#32768"));
        let n = note_open(Kind::Popup, "Qt5QWindowPopup", 10, 20, "");
        assert!(n.contains("10x20") && !n.contains("원래 창"));
        assert!(note_closed("Studio", true).contains("'Studio'"));
        assert_eq!(note_closed("", false), "팝업이 닫혔습니다");
    }
}
