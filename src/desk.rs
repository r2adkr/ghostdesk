//! Common interface for the isolated screens agents operate.
//!
//! Each backend implements inherent methods with the same names, and this module just dispatches
//! over an enum (traits with async fns are awkward to use as `dyn`).

use std::borrow::Cow;
use std::sync::Arc;

use anyhow::Result;

use crate::browser::{BrowserWorkspace, MouseButton};
use crate::electron::ElectronWorkspace;
use crate::native::NativeAppWorkspace;
use crate::rfb::RfbDesk;
use crate::session::SessionDesk;
use crate::vdisplay::VirtualDisplayDesk;

#[derive(Clone, Debug)]
pub struct ViewInfo {
    pub id: String,
    pub title: String,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DeskKind {
    /// Headless Chromium on a cloned profile (tab = view).
    Browser,
    /// A separate desktop session over RFB (VNC): a VM, another Mac, Linux Xvnc, …
    Remote,
    /// A hidden clone instance of an Electron app (CDP).
    Electron,
    /// Hidden macOS app windows (ScreenCaptureKit + Accessibility).
    Native,
    /// A separate login session of a dedicated agent account (RDP-style, session helper).
    Session,
    /// Experimental: macOS apps on a virtual display (real pixel input).
    VDisplay,
}

#[derive(Clone)]
pub enum Desk {
    Browser(Arc<BrowserWorkspace>),
    Remote(Arc<RfbDesk>),
    Electron(Arc<ElectronWorkspace>),
    Native(Arc<NativeAppWorkspace>),
    Session(Arc<SessionDesk>),
    VDisplay(Arc<VirtualDisplayDesk>),
}

macro_rules! each {
    ($self:ident, $w:ident => $e:expr) => {
        match $self {
            Desk::Browser($w) => $e,
            Desk::Remote($w) => $e,
            Desk::Electron($w) => $e,
            Desk::Native($w) => $e,
            Desk::Session($w) => $e,
            Desk::VDisplay($w) => $e,
        }
    };
}

/// Whether a key combo touches the system clipboard (copy, cut, paste), even with extra
/// modifiers like shift.
///
/// - cmd/ctrl + c, x, v (including "paste without formatting" like ctrl+shift+v)
/// - outside macOS, the legacy Windows/Chromium clipboard keys: ctrl+Insert (copy), shift+Insert
///   (paste), shift+Delete (cut). Insert is blocked even with meta. macOS has no such bindings,
///   so they pass through as before.
pub fn is_clipboard_combo(combo: &str) -> bool {
    use crate::keys::{parse_combo, MOD_CTRL, MOD_META, MOD_SHIFT};
    let Ok(c) = parse_combo(combo) else { return false };
    match c.main.key.to_ascii_lowercase().as_str() {
        "c" | "x" | "v" => c.mods & (MOD_CTRL | MOD_META) != 0,
        "insert" if !cfg!(target_os = "macos") => c.mods & (MOD_CTRL | MOD_SHIFT | MOD_META) != 0,
        // ctrl+Delete deletes the next word, let it through
        "delete" if !cfg!(target_os = "macos") => c.mods & MOD_SHIFT != 0 && c.mods & MOD_CTRL == 0,
        _ => false,
    }
}

/// Whether the backend runs on this machine (headless browser, Electron, local apps).
///
/// On those screens cmd is sent as ctrl outside macOS (`platform::input`). Remote (VNC),
/// separate-session and virtual-display screens follow the other screen's OS and are sent as is.
fn is_local_kind(kind: DeskKind) -> bool {
    matches!(kind, DeskKind::Browser | DeskKind::Electron | DeskKind::Native)
}

/// Localizes a key combo for the screen kind (always unchanged on macOS).
fn localize_combo_for(kind: DeskKind, combo: &str) -> Cow<'_, str> {
    if is_local_kind(kind) {
        crate::platform::input::localize_combo(combo)
    } else {
        Cow::Borrowed(combo)
    }
}

/// Localizes mouse modifier bits for the screen kind (always unchanged on macOS).
fn localize_mods_for(kind: DeskKind, mods: i64) -> i64 {
    if is_local_kind(kind) {
        crate::platform::input::localize_mods(mods)
    } else {
        mods
    }
}

impl Desk {
    pub fn kind(&self) -> DeskKind {
        match self {
            Desk::Browser(_) => DeskKind::Browser,
            Desk::Remote(_) => DeskKind::Remote,
            Desk::Electron(_) => DeskKind::Electron,
            Desk::Native(_) => DeskKind::Native,
            Desk::Session(_) => DeskKind::Session,
            Desk::VDisplay(_) => DeskKind::VDisplay,
        }
    }

    pub fn as_browser(&self) -> Option<&Arc<BrowserWorkspace>> {
        match self {
            Desk::Browser(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_electron(&self) -> Option<&Arc<ElectronWorkspace>> {
        match self {
            Desk::Electron(e) => Some(e),
            _ => None,
        }
    }

    pub fn as_vdisplay(&self) -> Option<&Arc<VirtualDisplayDesk>> {
        match self {
            Desk::VDisplay(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_native(&self) -> Option<&Arc<NativeAppWorkspace>> {
        match self {
            Desk::Native(n) => Some(n),
            _ => None,
        }
    }

    /// Screenshot size in pixels (the agent's coordinate space).
    pub fn size(&self) -> (u32, u32) {
        each!(self, w => w.size())
    }

    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        each!(self, w => w.screenshot().await)
    }

    pub async fn zoom(&self, region: [f64; 4]) -> Result<Vec<u8>> {
        each!(self, w => w.zoom(region).await)
    }

    /// Mouse modifier bits adjusted for this screen (local backend outside macOS: cmd → ctrl).
    fn mods(&self, mods: i64) -> i64 {
        localize_mods_for(self.kind(), mods)
    }

    /// Key combo adjusted for this screen (local backend outside macOS: cmd → ctrl).
    fn combo<'a>(&self, combo: &'a str) -> Cow<'a, str> {
        localize_combo_for(self.kind(), combo)
    }

    pub async fn mouse_move(&self, x: f64, y: f64, mods: i64) -> Result<()> {
        let mods = self.mods(mods);
        each!(self, w => w.mouse_move(x, y, mods).await)
    }

    /// Glides smoothly from the current position to `(x, y)` like a person (a stream of mouseMoved
    /// events along an ease-in-out path).
    ///
    /// Hidden local apps (Native) have no pointer (clicks are semantic or window messages), so the
    /// move is only recorded.
    pub async fn glide_to(&self, x: f64, y: f64, mods: i64) -> Result<()> {
        if self.kind() == DeskKind::Native {
            return Ok(());
        }
        let (fx, fy) = self.mouse_pos();
        let dist = ((x - fx).powi(2) + (y - fy).powi(2)).sqrt();
        if dist < 3.0 {
            return self.mouse_move(x, y, mods).await;
        }
        let dur = crate::cursor::glide_duration(dist);
        let steps = crate::cursor::frames(dur);
        let dt = dur / steps;
        for i in 1..=steps {
            let t = crate::cursor::ease(i as f32 / steps as f32) as f64;
            self.mouse_move(fx + (x - fx) * t, fy + (y - fy) * t, mods).await?;
            if i < steps {
                tokio::time::sleep(dt).await;
            }
        }
        Ok(())
    }

    pub async fn mouse_down(&self, b: MouseButton, count: i64, mods: i64) -> Result<()> {
        let mods = self.mods(mods);
        each!(self, w => w.mouse_down(b, count, mods).await)
    }

    pub async fn mouse_up(&self, b: MouseButton, count: i64, mods: i64) -> Result<()> {
        let mods = self.mods(mods);
        each!(self, w => w.mouse_up(b, count, mods).await)
    }

    pub async fn click(&self, pos: Option<(f64, f64)>, b: MouseButton, count: u32, mods: i64) -> Result<()> {
        let mods = self.mods(mods);
        each!(self, w => w.click(pos, b, count, mods).await)
    }

    pub async fn drag(&self, from: (f64, f64), to: (f64, f64), mods: i64) -> Result<()> {
        let mods = self.mods(mods);
        each!(self, w => w.drag(from, to, mods).await)
    }

    pub async fn scroll(&self, pos: Option<(f64, f64)>, dx: f64, dy: f64, mods: i64) -> Result<()> {
        let mods = self.mods(mods);
        each!(self, w => w.scroll(pos, dx, dy, mods).await)
    }

    /// Key input from an agent (internal or external).
    ///
    /// Electron screens share the user's real system clipboard, so copy/cut/paste combos
    /// (`is_clipboard_combo`) are refused. The user's own takeover input uses `key_by_human`.
    pub async fn key(&self, combo: &str, repeat: u32) -> Result<()> {
        let combo = self.combo(combo);
        self.check_clipboard_combo(&combo)?;
        each!(self, w => w.key(&combo, repeat).await)
    }

    /// A key the user pressed during takeover. It's the person's own input, so clipboard combos pass
    /// through too.
    pub async fn key_by_human(&self, combo: &str, repeat: u32) -> Result<()> {
        let combo = self.combo(combo);
        each!(self, w => w.key(&combo, repeat).await)
    }

    pub async fn hold_key(&self, combo: &str, secs: f64) -> Result<()> {
        let combo = self.combo(combo);
        self.check_clipboard_combo(&combo)?;
        each!(self, w => w.hold_key(&combo, secs).await)
    }

    fn check_clipboard_combo(&self, combo: &str) -> Result<()> {
        if self.kind() == DeskKind::Electron && is_clipboard_combo(combo) {
            let keys = if cfg!(target_os = "macos") {
                "cmd/ctrl + c·x·v"
            } else {
                "ctrl + c·x·v, ctrl/shift + Insert, shift + Delete"
            };
            anyhow::bail!(
                "Electron 앱 화면에서는 '{combo}' 를 쓸 수 없습니다: 이 앱은 사용자의 실제 시스템 클립보드를 공유하므로 \
                 복사·잘라내기·붙여넣기 단축키({keys})는 막혀 있습니다. 텍스트를 넣으려면 `type` 동작을 쓰세요. \
                 (Clipboard shortcuts are blocked on Electron desks because the clipboard is the human's; use the `type` action instead.)"
            );
        }
        Ok(())
    }

    pub async fn type_text(&self, text: &str) -> Result<()> {
        each!(self, w => w.type_text(text).await)
    }

    pub fn mouse_pos(&self) -> (f64, f64) {
        each!(self, w => w.mouse_pos())
    }

    /// Tabs (browser) or windows (app), plus the current view id.
    pub fn views(&self) -> (Vec<ViewInfo>, Option<String>) {
        each!(self, w => w.views())
    }

    pub async fn switch_view(&self, index: usize) -> Result<()> {
        each!(self, w => w.switch_view(index).await)
    }

    pub fn views_label(&self) -> &'static str {
        match self.kind() {
            DeskKind::Browser => "Tabs",
            DeskKind::Remote => "Desktop",
            DeskKind::Electron | DeskKind::Native | DeskKind::VDisplay => "Windows",
            DeskKind::Session => "Session",
        }
    }

    pub fn views_text(&self) -> String {
        let (views, active) = self.views();
        let mut out = String::new();
        for (i, v) in views.iter().enumerate() {
            let mark = if Some(&v.id) == active.as_ref() { "▶" } else { " " };
            out.push_str(&format!(
                "{mark} [{i}] {} — {}\n",
                crate::llm::short(&v.title, 80),
                crate::llm::short(&v.detail, 160)
            ));
        }
        out
    }

    pub fn take_notes(&self) -> Vec<String> {
        each!(self, w => w.take_notes())
    }

    pub fn is_closed(&self) -> bool {
        each!(self, w => w.is_closed())
    }

    pub async fn shutdown(&self) {
        each!(self, w => w.shutdown().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT};

    #[test]
    fn clipboard_combos() {
        for c in [
            "cmd+c", "cmd+v", "cmd+x", "ctrl+c", "ctrl+V", "super+v", "cmd+shift+v", "command+x", "meta+c",
            // with extra modifiers, e.g. paste without formatting
            "ctrl+shift+v", "ctrl+shift+V", "ctrl+alt+v",
        ] {
            assert!(is_clipboard_combo(c), "{c}");
        }
        // Windows/Chromium Insert/Delete clipboard keys: blocked outside macOS only (macOS sends them
        // as before)
        for c in [
            "ctrl+Insert", "shift+Insert", "Shift+insert", "ctrl+shift+Insert", "meta+Insert", "cmd+Insert",
            "shift+Delete", "shift+del", "shift+KP_Delete", "shift+alt+Delete",
        ] {
            assert_eq!(is_clipboard_combo(c), !cfg!(target_os = "macos"), "{c}");
        }
        for c in [
            "cmd+a", "cmd+z", "c", "v", "shift+v", "alt+c", "Return", "cmd+shift+p", "not a key",
            "Insert", "alt+Insert", "Delete", "del", "ctrl+Delete", "ctrl+shift+Delete", "alt+Delete",
            "BackSpace", "shift+BackSpace", "ctrl+i", "shift+d",
        ] {
            assert!(!is_clipboard_combo(c), "{c}");
        }
    }

    #[test]
    fn local_backends_get_localized_shortcuts() {
        let remap = !cfg!(target_os = "macos");
        for k in [DeskKind::Browser, DeskKind::Electron, DeskKind::Native] {
            assert!(is_local_kind(k));
            let want = if remap { "ctrl+shift+p" } else { "cmd+shift+p" };
            assert_eq!(localize_combo_for(k, "cmd+shift+p"), want, "{k:?}");
            assert_eq!(localize_combo_for(k, "super+e"), "super+e", "{k:?}: 명시한 Windows 키는 그대로");
            assert_eq!(localize_combo_for(k, "ctrl+c"), "ctrl+c");
            let m = localize_mods_for(k, MOD_META | MOD_SHIFT);
            assert_eq!(m, if remap { MOD_CTRL | MOD_SHIFT } else { MOD_META | MOD_SHIFT }, "{k:?}");
            assert_eq!(localize_mods_for(k, MOD_ALT), MOD_ALT);
        }
        // remote, separate-session and virtual-display screens follow the other side's OS, so unchanged
        for k in [DeskKind::Remote, DeskKind::Session, DeskKind::VDisplay] {
            assert!(!is_local_kind(k));
            assert_eq!(localize_combo_for(k, "cmd+c"), "cmd+c", "{k:?}");
            assert_eq!(localize_mods_for(k, MOD_META), MOD_META, "{k:?}");
        }
        // localized combos still hit the clipboard check
        assert!(is_clipboard_combo(&localize_combo_for(DeskKind::Electron, "cmd+v")));
        assert_eq!(is_clipboard_combo(&localize_combo_for(DeskKind::Electron, "cmd+Insert")), !cfg!(target_os = "macos"));
    }
}
