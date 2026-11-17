//! 에이전트가 조작하는 "격리 화면"의 공통 인터페이스.
//!
//! 백엔드마다 같은 이름의 고유 메서드를 구현하고, 여기서는 enum 으로 분기만 한다.
//! (async fn 을 가진 trait 은 dyn 으로 쓰기 불편하므로 enum dispatch 를 쓴다.)

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
    /// 복제 프로필의 headless Chromium (탭 = 뷰)
    Browser,
    /// RFB(VNC)로 접속한 별도 데스크톱 세션 (VM, 다른 Mac, Linux Xvnc 등)
    Remote,
    /// 숨김 실행한 Electron 앱 복제 인스턴스 (CDP)
    Electron,
    /// 숨겨진 macOS 앱 창 (ScreenCaptureKit + 접근성)
    Native,
    /// 전용 에이전트 계정의 별도 로그인 세션 (RDP 방식, 세션 헬퍼)
    Session,
    /// 실험: 가상 디스플레이 위의 macOS 앱 (실제 픽셀 입력)
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

/// 시스템 클립보드에 닿는 키 조합인가 (복사·잘라내기·붙여넣기). shift 등 다른 수정키가 붙어도 해당한다.
/// - cmd/ctrl + c·x·v (ctrl+shift+v 같은 '서식 없이 붙여넣기' 포함)
/// - (macOS 제외) Windows·Chromium 의 옛 클립보드 키: ctrl+Insert(복사), shift+Insert(붙여넣기),
///   shift+Delete(잘라내기). Insert 는 meta 가 붙어도 막는다. macOS 에는 이 바인딩이 없으므로 예전처럼 그대로 보낸다.
pub fn is_clipboard_combo(combo: &str) -> bool {
    use crate::keys::{parse_combo, MOD_CTRL, MOD_META, MOD_SHIFT};
    let Ok(c) = parse_combo(combo) else { return false };
    match c.main.key.to_ascii_lowercase().as_str() {
        "c" | "x" | "v" => c.mods & (MOD_CTRL | MOD_META) != 0,
        "insert" if !cfg!(target_os = "macos") => c.mods & (MOD_CTRL | MOD_SHIFT | MOD_META) != 0,
        // ctrl+Delete 는 '다음 단어 지우기' 라 그대로 둔다
        "delete" if !cfg!(target_os = "macos") => c.mods & MOD_SHIFT != 0 && c.mods & MOD_CTRL == 0,
        _ => false,
    }
}

/// 이 컴퓨터 안에서 도는 백엔드(헤드리스 브라우저·Electron·로컬 앱)인가.
/// 이런 화면에서는 macOS 가 아니면 cmd → ctrl 로 바꿔 보낸다 (`platform::input`).
/// 원격(VNC)·별도 세션·가상 디스플레이는 상대 화면의 OS 규칙을 따르므로 그대로 보낸다.
fn is_local_kind(kind: DeskKind) -> bool {
    matches!(kind, DeskKind::Browser | DeskKind::Electron | DeskKind::Native)
}

/// 화면 종류에 맞게 키 조합을 현지화한다 (macOS 에서는 항상 그대로).
fn localize_combo_for(kind: DeskKind, combo: &str) -> Cow<'_, str> {
    if is_local_kind(kind) {
        crate::platform::input::localize_combo(combo)
    } else {
        Cow::Borrowed(combo)
    }
}

/// 화면 종류에 맞게 마우스 수정키 비트를 현지화한다 (macOS 에서는 항상 그대로).
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

    /// 스크린샷 픽셀 크기 (에이전트 좌표계)
    pub fn size(&self) -> (u32, u32) {
        each!(self, w => w.size())
    }

    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        each!(self, w => w.screenshot().await)
    }

    pub async fn zoom(&self, region: [f64; 4]) -> Result<Vec<u8>> {
        each!(self, w => w.zoom(region).await)
    }

    /// 마우스 수정키 비트를 이 화면에 맞게 (로컬 백엔드 + macOS 아님: cmd → ctrl)
    fn mods(&self, mods: i64) -> i64 {
        localize_mods_for(self.kind(), mods)
    }

    /// 키 조합을 이 화면에 맞게 (로컬 백엔드 + macOS 아님: cmd → ctrl)
    fn combo<'a>(&self, combo: &'a str) -> Cow<'a, str> {
        localize_combo_for(self.kind(), combo)
    }

    pub async fn mouse_move(&self, x: f64, y: f64, mods: i64) -> Result<()> {
        let mods = self.mods(mods);
        each!(self, w => w.mouse_move(x, y, mods).await)
    }

    /// 현재 위치에서 (x, y) 까지 사람처럼 부드럽게 이동한다 (ease-in-out 경로의 mouseMoved 연속 전송).
    /// 숨긴 로컬 앱(Native)은 포인터 개념이 없어(의미 기반 클릭·창 메시지) 이동만 기록한다.
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

    /// 에이전트(내부·외부)의 키 입력. Electron 화면은 사용자의 실제 시스템 클립보드를 공유하므로
    /// 복사·잘라내기·붙여넣기 조합(`is_clipboard_combo`)은 거부한다 (사용자의 직접 조작은 `key_by_human` 을 쓴다).
    pub async fn key(&self, combo: &str, repeat: u32) -> Result<()> {
        let combo = self.combo(combo);
        self.check_clipboard_combo(&combo)?;
        each!(self, w => w.key(&combo, repeat).await)
    }

    /// 사용자가 '직접 조작'으로 누른 키. 사람이 직접 한 입력이므로 클립보드 조합도 그대로 전달한다.
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

    /// 탭(브라우저) 또는 창(앱) 목록과 현재 뷰 id
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
            // 서식 없이 붙여넣기 등 수정키가 더 붙은 경우
            "ctrl+shift+v", "ctrl+shift+V", "ctrl+alt+v",
        ] {
            assert!(is_clipboard_combo(c), "{c}");
        }
        // Windows·Chromium 의 Insert/Delete 클립보드 키: macOS 밖에서만 막는다 (macOS 는 예전처럼 그대로 보냄)
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
        // 원격·별도 세션·가상 디스플레이는 상대 화면의 OS 를 따르므로 그대로
        for k in [DeskKind::Remote, DeskKind::Session, DeskKind::VDisplay] {
            assert!(!is_local_kind(k));
            assert_eq!(localize_combo_for(k, "cmd+c"), "cmd+c", "{k:?}");
            assert_eq!(localize_mods_for(k, MOD_META), MOD_META, "{k:?}");
        }
        // 현지화한 조합도 클립보드 검사에 그대로 걸린다
        assert!(is_clipboard_combo(&localize_combo_for(DeskKind::Electron, "cmd+v")));
        assert_eq!(is_clipboard_combo(&localize_combo_for(DeskKind::Electron, "cmd+Insert")), !cfg!(target_os = "macos"));
    }
}
