//! macOS 네이티브 앱 백엔드 (AppKit / Catalyst / 샌드박스 앱: 카카오톡, 메모, 텍스트 편집기 등).
//!
//! 에이전트는 **사용자 화면에 보이지 않는** 앱 창 하나를 조작한다.
//!
//! ## 동작 방식
//! - **Mode A (기본, 숨김 인스턴스 실행)**: 대상 앱을 숨김·비활성 상태로 새로 띄운다
//!   (`NSWorkspaceOpenConfiguration` activates=false, hides=true, 필요하면 createsNewApplicationInstance).
//!   일부 문서 기반 앱은 실행 직후 창을 아주 잠깐 보였다가 숨는다 — 노출 감시기가 즉시 다시 숨기고
//!   측정한 노출 시간을 [`NativeAppWorkspace::exposure_report`] 와 노트로 알려준다.
//! - **Mode B (사용자의 실행 중인 앱에 붙기)**: 호출자가 명시적인 동의 플래그(`attach_consent`)를 줄 때만.
//!   GhostDesk 가 그 앱을 숨기므로 **에이전트가 일하는 동안 사용자의 앱 창이 화면에서 사라진다.**
//!   원래 숨김 상태를 기록해 두었다가 종료 시 되돌린다 (원래 보이던 앱이면 unhide, 활성화는 안 함).
//!
//! ## 화면 모델
//! - 한 번에 창 하나. 뷰 = 앱의 창 목록 (AX + CGWindowID).
//! - 스크린샷 = ScreenCaptureKit 단일 창 캡처 (숨김/가려진 창도 됨), 긴 변 ≤ 1280px 로 축소.
//!   캡처 시점의 창 프레임(전역 포인트)을 기억해 에이전트 픽셀 → 창 포인트 → 전역 포인트로 바꾼다.
//!   창에 시트(저장 대화상자 등)가 붙어 있으면 시트도 합성한다.
//!
//! ## 입력
//! - 숨김 창에는 마우스 이벤트가 전달되지 않으므로 클릭은 **의미 기반 클릭**이다: 좌표 아래의 가장 깊은
//!   AX 요소를 찾아 역할에 맞는 동작(AXPress, 포커스+커서, 행 선택, 슬라이더 값)을 수행하고 노트로 알린다.
//!   드래그·픽셀 단위 조작·오른쪽 클릭(화면에 메뉴가 뜸)은 명확한 오류를 돌려준다.
//! - 글자 입력은 `CGEventPostToPid` 유니코드 이벤트 (한글 포함). 반영이 확인되지 않으면
//!   AXSelectedText 삽입으로 대체한다.
//! - 특수키는 mac 가상 키코드. Cmd/Ctrl 단축키: 전체선택/복사/잘라내기/붙여넣기는 AX 로 흉내낸다
//!   (복사/붙여넣기는 **내부 클립보드**를 써서 사용자의 시스템 클립보드를 건드리지 않음).
//!   그 밖의 단축키는 같은 단축키를 가진 메뉴 항목을 AXPress 하고, 없으면 SkyLight 키 창 레코드 +
//!   키 이벤트를 보낸다 (숨김 상태로 시작한 앱에서는 대부분 무시됨 — 노트로 알림).
//! - LLM 용 추가 도구 [`NativeAppWorkspace::app_control`] (ui_tree / press / set_value / menu ...).
//!
//! ## 사용자 세션 보호
//! 숨김 앱의 메뉴도 AXPress 로 실행되고, 앱의 AXMenuBar 에는 Apple 메뉴(잠금 화면 ctrl+cmd+Q, 로그아웃
//! shift+cmd+Q, 즉시 로그아웃, 재시동, 시스템 종료)가 활성 상태로 들어 있다(이 Mac 에서 확인). 그래서 메뉴 실행·
//! 단축키·키 이벤트 모두 사용자 세션 전체나 다른 앱에 작용하는 항목(Apple 메뉴, 서비스, 기타 가리기/모두 보기,
//! 도움말, 공유, 받아쓰기/이모지/글쓰기 도구, 열기 패널)을 차단하고, 링크·전체 화면·최소화 버튼은 누르지 않는다.
//! 클릭 좌표는 에이전트가 마지막으로 본 스크린샷의 창에만 적용한다 (창이 바뀌었으면 오류).
//!
//! ## 권한
//! 손쉬운 사용 + 화면 기록 권한이 필요하다 ([`crate::macos::preflight`]).
//! 새 바이너리가 처음 ScreenCaptureKit 을 쓰면 macOS 가 1회성 확인 알림을 띄울 수 있다.

#![allow(dead_code)]

use std::path::PathBuf;

/// 네이티브 앱 작업공간 설정
#[derive(Clone, Debug)]
pub struct NativeOptions {
    /// 대상 앱: `.app` 경로 또는 번들 id (예: "com.apple.TextEdit", "com.kakao.KakaoTalkMac")
    pub app: String,
    /// 시작할 때 열 파일들 (Mode A)
    pub open_files: Vec<PathBuf>,
    /// 이미 실행 중이어도 별도 인스턴스를 띄운다 (createsNewApplicationInstance). 단일 인스턴스 앱은 거부할 수 있음.
    pub new_instance: bool,
    /// Mode B 동의: 사용자가 실행 중인 인스턴스를 숨기고 그 위에서 일해도 된다는 명시적 동의
    pub attach_consent: bool,
    /// Mode B 에서 붙을 특정 pid (없으면 번들 id 의 첫 인스턴스)
    pub attach_pid: Option<i32>,
    /// 실행 인자 추가 (기본으로 상태 복원/저장을 끄는 인자가 들어간다)
    pub extra_args: Vec<String>,
    /// 스크린샷 긴 변 최대 픽셀
    pub max_long_edge: u32,
    /// 라이브 뷰 초당 프레임 (0 이면 끔)
    pub live_fps: f64,
    /// (Windows) 앱을 어디서 띄울지: 숨은 데스크톱 / 화면 밖 창 / 자동
    pub host: NativeHost,
}

/// (Windows) Mode A 앱을 띄울 곳
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum NativeHost {
    /// 숨은 데스크톱을 먼저 쓰고, 거기서 그려지지 않는 앱(GPU blt 출력 등)은 화면 밖 창으로 다시 띄운다
    #[default]
    Auto,
    /// 같은 세션의 보이지 않는 데스크톱 (CreateDesktop): 팝업·메뉴도 사용자 화면에 나타나지 않는다
    Hidden,
    /// 원본 로그인 데이터를 쓰는 같은 계정의 숨은 데스크톱. 실행 중인 앱과 attach 는 거절한다.
    CurrentSession,
    /// 사용자 데스크톱의 모든 모니터 바깥 (이전 방식)
    Offscreen,
}

impl NativeHost {
    pub fn parse(s: &str) -> Option<NativeHost> {
        match s.trim().to_lowercase().as_str() {
            "" | "auto" => Some(NativeHost::Auto),
            "hidden" | "hidden_desktop" | "virtual" => Some(NativeHost::Hidden),
            "current_session" => Some(NativeHost::CurrentSession),
            "offscreen" | "off_screen" => Some(NativeHost::Offscreen),
            _ => None,
        }
    }
}

#[cfg(test)]
mod host_tests {
    use super::NativeHost;

    #[test]
    fn current_session_is_an_explicit_host() {
        let host = NativeHost::parse("current_session").expect("현재 계정 모드를 선택할 수 있어야 함");
        assert_eq!(host, NativeHost::CurrentSession);
    }
}

impl Default for NativeOptions {
    fn default() -> Self {
        NativeOptions {
            app: String::new(),
            open_files: vec![],
            new_instance: false,
            attach_consent: false,
            attach_pid: None,
            extra_args: vec![],
            max_long_edge: 1280,
            live_fps: 2.0,
            host: NativeHost::Auto,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NativeMode {
    /// 우리가 숨김 상태로 띄운 인스턴스 (종료 시 프로세스를 끝낸다)
    Launched,
    /// 사용자의 실행 중인 인스턴스에 동의 하에 붙음 (종료 시 숨김 상태만 되돌린다)
    Attached,
}

/// 이 컴퓨터에 설치된 일반 앱 목록 (표시 이름, 실행 파일 경로) — 새 에이전트 창의 앱 고르기용.
/// macOS 는 app.rs 가 /Applications 를 직접 훑으므로 비어 있다. Windows 는 시작 메뉴·App Paths 에서 찾는다.
pub fn installed_apps() -> Vec<(String, String)> {
    #[cfg(windows)]
    {
        crate::winapp::installed_apps()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// 사용자가 적은 앱(경로·이름·실행 파일 이름)을 실행 파일 경로로 바꾼다. Windows 전용 (그 밖은 None).
pub fn resolve_app_query(query: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        crate::winapp::resolve_app_query(query)
    }
    #[cfg(not(windows))]
    {
        let _ = query;
        None
    }
}

/// 실행 전에 항상 거절할 실행 파일인지 확인한다 (Windows: Store 앱·실행 별칭·콘솔 프로그램·관리자 권한 앱·셸과 시스템 실행기 등).
/// 외부 요청은 사람에게 승인을 묻기 **전에** 이것으로 거른다. 다른 OS 는 항상 Ok.
pub fn preflight_app(exe: &std::path::Path) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        crate::winapp::preflight(exe)
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
        Ok(())
    }
}

/// GhostDesk 가 비정상 종료되어 화면 밖에 남은 사용자 앱 창(Windows Mode B)을 원래 자리로 되돌린다. 되돌린 창 수.
/// Windows 에서는 앱 작업공간을 열 때마다 자동으로 부르며, GUI 시작 때 불러도 된다. 다른 OS 는 할 일이 없다 (0).
pub fn recover_parked_windows() -> usize {
    #[cfg(windows)]
    {
        crate::winapp::recover_parked_windows()
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// 이 OS 의 로컬 앱 백엔드에 맞는 에이전트 환경 설명 (None 이면 agent.rs 의 macOS 설명을 쓴다)
pub fn env_prompt() -> Option<&'static str> {
    #[cfg(windows)]
    {
        crate::winapp::ENV_PROMPT
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// LLM 에 노출할 `app_control` 도구 정의 (Anthropic custom tool 형식)
pub fn app_control_tool() -> serde_json::Value {
    serde_json::json!({
        "name": "app_control",
        "description": APP_CONTROL_DESCRIPTION,
        "input_schema": {
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list_windows", "switch_window", "ui_tree", "press", "set_value", "focus", "select", "choose", "menus", "menu", "read"]
                },
                "index": { "type": "integer", "description": "switch_window: window index from list_windows" },
                "id": { "type": "integer", "description": "element id from the latest ui_tree" },
                "text": { "type": "string", "description": "set_value: new value" },
                "option": { "type": "string", "description": "choose: popup menu option title" },
                "path": { "type": "string", "description": "menu: e.g. \"File > Save\" (use the app's own, possibly localized, titles)" },
                "max_depth": { "type": "integer", "description": "ui_tree: max depth (default 30)" }
            },
            "required": ["action"]
        }
    })
}

/// Windows: 화면 밖 Win32 앱용 설명 (winapp 참고)
#[cfg(windows)]
pub const APP_CONTROL_DESCRIPTION: &str = crate::winapp::APP_CONTROL_DESCRIPTION;

#[cfg(not(windows))]
pub const APP_CONTROL_DESCRIPTION: &str = "Semantic control of the native macOS app window you are operating. The app runs hidden (off the user's screen), so mouse clicks are translated into accessibility actions; use this tool when a click does not do what you want, for menus, or to read text exactly.\n\
Actions:\n\
- list_windows: list the app's windows (index, title).\n\
- switch_window {index}: operate another window.\n\
- ui_tree {max_depth?}: numbered list of UI elements of the current window: id, role, title/description/value (truncated), frame [x,y,w,h] in SCREENSHOT pixels. Ids stay valid until the next ui_tree call.\n\
- press {id}: AXPress a button/checkbox/link/menu item.\n\
- set_value {id, text}: replace the value of a text field/area (verified by re-reading).\n\
- focus {id}: give keyboard focus to an element (then type with the computer tool).\n\
- select {id}: select a table/list/outline row or tab.\n\
- choose {id, option}: pick an option of a popup button without opening the menu on screen.\n\
- menus: list the app's menu bar (localized titles, shortcuts, disabled items).\n\
- menu {path}: invoke a menu bar item, e.g. \"File > Save\" or localized \"파일 > 저장\". Unreliable while hidden for items that act on the focused text (Edit menu); verify with a screenshot.\n\
- read {id}: full untruncated value of an element.\n\
Limits: drag, right-click (context menus), and pixel-precise mouse gestures are not possible while the app is hidden. Cmd+A/C/X/V are emulated with an internal clipboard (the user's system clipboard is never touched).\n\
Blocked for the user's safety (menu, shortcut and click alike): the Apple menu (lock screen, log out, restart...), Services / Hide Others / Show All, Help, Share, dictation / emoji / writing tools, File > Open… (the open panel would appear on the user's screen), links (they would open a browser on the user's screen), and the full-screen / minimize window buttons. After a new window opens, take a screenshot before clicking in it.";

#[cfg(target_os = "macos")]
pub use imp::*;

// Windows: 화면 밖 창 + PrintWindow + 창 메시지 + UI Automation (src/winapp)
#[cfg(windows)]
pub use crate::winapp::{selftest, NativeAppWorkspace};

#[cfg(not(any(target_os = "macos", windows)))]
pub use stub::*;

// ═════════════════════════════════════════ macOS ═════════════════════════════════════════

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::{anyhow, bail, Context, Result};
    use image::RgbImage;
    use objc2_core_foundation::{CFRange, CGPoint, CGRect, CGSize};
    use parking_lot::Mutex;
    use serde_json::Value;

    use super::{NativeHost, NativeMode, NativeOptions};
    use crate::browser::{FrameSink, MouseButton};
    use crate::desk::ViewInfo;
    use crate::keys::{MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT};
    use crate::macos::{
        apps,
        ax::{self, El},
        capture::{self, Filter},
        guard::ExposureGuard,
        input, skylight,
    };

    // ───────────────────────── 좌표 ─────────────────────────

    /// 스크린샷 픽셀 ↔ 전역 포인트 변환 정보 (캡처 시점 기준)
    #[derive(Clone, Copy, Debug)]
    struct Geom {
        wid: u32,
        /// 창 프레임 (전역 포인트, 좌상단 원점)
        frame: CGRect,
        shot_w: u32,
        shot_h: u32,
    }

    impl Geom {
        fn sx(&self) -> f64 {
            self.frame.size.width / self.shot_w.max(1) as f64
        }
        fn sy(&self) -> f64 {
            self.frame.size.height / self.shot_h.max(1) as f64
        }
        fn to_global(&self, x: f64, y: f64) -> CGPoint {
            CGPoint { x: self.frame.origin.x + x * self.sx(), y: self.frame.origin.y + y * self.sy() }
        }
        fn rect_to_shot(&self, r: CGRect) -> [i64; 4] {
            [
                ((r.origin.x - self.frame.origin.x) / self.sx()).round() as i64,
                ((r.origin.y - self.frame.origin.y) / self.sy()).round() as i64,
                (r.size.width / self.sx()).round() as i64,
                (r.size.height / self.sy()).round() as i64,
            ]
        }
    }

    fn contains(r: &CGRect, p: CGPoint) -> bool {
        p.x >= r.origin.x && p.y >= r.origin.y && p.x < r.origin.x + r.size.width && p.y < r.origin.y + r.size.height
    }

    fn intersects(a: &CGRect, b: &CGRect) -> bool {
        a.origin.x < b.origin.x + b.size.width
            && b.origin.x < a.origin.x + a.size.width
            && a.origin.y < b.origin.y + b.size.height
            && b.origin.y < a.origin.y + a.size.height
    }

    fn center(r: &CGRect) -> CGPoint {
        CGPoint { x: r.origin.x + r.size.width / 2.0, y: r.origin.y + r.size.height / 2.0 }
    }

    // ───────────────────────── AX 노드 정보 ─────────────────────────

    const NODE_ATTRS: [&str; 10] = [
        "AXRole", "AXSubrole", "AXPosition", "AXSize", "AXTitle", "AXDescription", "AXValue", "AXEnabled",
        "AXFocused", "AXPlaceholderValue",
    ];

    #[derive(Clone, Debug)]
    struct Node {
        el: El,
        role: String,
        subrole: String,
        frame: Option<CGRect>,
        title: String,
        desc: String,
        value: String,
        enabled: bool,
        focused: bool,
        placeholder: String,
    }

    impl Node {
        fn read(el: &El) -> Node {
            let v = el.multi(&NODE_ATTRS);
            let s = |i: usize| v[i].as_deref().and_then(ax::cf_to_string).unwrap_or_default();
            let b = |i: usize, d: bool| {
                v[i].as_deref().and_then(ax::cf_to_string).map(|x| x == "1").unwrap_or(d)
            };
            let pos = v[2].as_deref().and_then(ax::cf_point);
            let size = v[3].as_deref().and_then(ax::cf_size);
            Node {
                el: el.clone(),
                role: s(0),
                subrole: s(1),
                frame: match (pos, size) {
                    (Some(origin), Some(size)) => Some(CGRect { origin, size }),
                    _ => None,
                },
                title: s(4),
                desc: s(5),
                value: s(6),
                enabled: b(7, true),
                focused: b(8, false),
                placeholder: s(9),
            }
        }

        /// 사람이 읽을 이름 (title > description > value 순)
        fn label(&self) -> String {
            for s in [&self.title, &self.desc, &self.placeholder] {
                if !s.trim().is_empty() {
                    return s.trim().to_string();
                }
            }
            if !self.value.is_empty() && !is_text_role(&self.role) {
                return self.value.clone();
            }
            String::new()
        }

        fn describe(&self) -> String {
            let l = self.label();
            if l.is_empty() {
                self.role.clone()
            } else {
                format!("{} '{}'", self.role, short(&l, 40))
            }
        }
    }

    fn is_text_role(role: &str) -> bool {
        matches!(role, "AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField")
    }

    fn is_press_role(role: &str) -> bool {
        matches!(
            role,
            "AXButton" | "AXCheckBox" | "AXRadioButton" | "AXLink" | "AXDisclosureTriangle" | "AXMenuItem" | "AXColorWell"
                | "AXDockItem"
        )
    }

    fn is_popup_role(role: &str) -> bool {
        matches!(role, "AXPopUpButton" | "AXMenuButton")
    }

    fn short(s: &str, n: usize) -> String {
        let s = s.replace('\n', "⏎").replace('\r', "");
        if s.chars().count() <= n {
            s
        } else {
            format!("{}…", s.chars().take(n).collect::<String>())
        }
    }

    fn utf16_len(s: &str) -> isize {
        s.encode_utf16().count() as isize
    }

    /// 자식 목록. 큰 표/목록은 보이는 행만 쓴다.
    fn kids(n: &Node) -> Vec<El> {
        if matches!(n.role.as_str(), "AXTable" | "AXOutline" | "AXList" | "AXBrowser" | "AXGrid") {
            let vis = n.el.elements("AXVisibleRows");
            if !vis.is_empty() {
                let mut v: Vec<El> = n.el.children().into_iter().filter(|c| c.role() != "AXRow" && c.role() != "AXColumn").collect();
                v.extend(vis);
                return v;
            }
            let vis = n.el.elements("AXVisibleChildren");
            if !vis.is_empty() {
                return vis;
            }
        }
        n.el.children()
    }

    /// 루트에서 점 p 를 포함하는 가장 깊은 요소까지의 경로 (위에 그려지는 뒤쪽 자식 우선)
    fn hit_path(root: &El, p: CGPoint) -> Vec<Node> {
        let mut path = vec![Node::read(root)];
        let mut budget = 3000usize;
        for _depth in 0..60 {
            let cur = path.last().unwrap().clone();
            let children = kids(&cur);
            let mut next: Option<Node> = None;
            let mut column: Option<Node> = None;
            for k in children.iter().rev() {
                if budget == 0 {
                    break;
                }
                budget -= 1;
                let n = Node::read(k);
                let Some(f) = n.frame else { continue };
                if f.size.width <= 0.0 || f.size.height <= 0.0 || !contains(&f, p) {
                    continue;
                }
                if n.role == "AXColumn" {
                    column.get_or_insert(n);
                    continue;
                }
                next = Some(n);
                break;
            }
            match next.or(column) {
                Some(n) => path.push(n),
                None => break,
            }
        }
        path
    }

    // ───────────────────────── 상태 ─────────────────────────

    #[derive(Clone)]
    struct WinEntry {
        wid: u32,
        title: String,
        subrole: String,
        frame: CGRect,
        el: Option<El>,
    }

    #[derive(Default)]
    struct State {
        windows: Vec<WinEntry>,
        current: Option<u32>,
        geom: Option<Geom>,
        mouse: (f64, f64),
        down: Option<((f64, f64), MouseButton)>,
        notes: Vec<String>,
        ui_ids: Vec<El>,
        clipboard: String,
        last_windows_refresh: Option<Instant>,
        user_unhid: bool,
    }

    struct Inner {
        pid: i32,
        /// 대상 앱의 번들 id (종료 시 pid 재사용 확인용)
        bundle_id: Option<String>,
        mode: NativeMode,
        prior_hidden: bool,
        app_name: String,
        opts: NativeOptions,
        state: Mutex<State>,
        filter: Mutex<Option<Filter>>,
        size: Mutex<(u32, u32)>,
        closed: AtomicBool,
        shut: AtomicBool,
        sink: FrameSink,
        exposure: Mutex<Vec<String>>,
        guard: Mutex<Option<ExposureGuard>>,
    }

    /// macOS 네이티브 앱 작업공간
    pub struct NativeAppWorkspace {
        inner: Arc<Inner>,
    }

    async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
        tokio::task::spawn_blocking(f).await.map_err(|e| anyhow!("작업 스레드 오류: {e}"))
    }

    /// 상태 복원/저장을 끄는 기본 실행 인자 (사용자의 창 복원 상태를 읽거나 덮어쓰지 않게)
    /// 문서 기반 앱이 시작할 때 열기 패널(별도 시스템 프로세스 창이라 숨길 수 없음) 대신 빈 문서를 만들게 한다.
    const DEFAULT_ARGS: [&str; 6] = [
        "-ApplePersistenceIgnoreState",
        "YES",
        "-ApplePersistence",
        "NO",
        "-NSShowAppCentricOpenPanelInsteadOfUntitledFile",
        "NO",
    ];

    impl NativeAppWorkspace {
        /// 앱을 숨김 상태로 띄우거나(Mode A), 동의 하에 실행 중인 앱에 붙는다(Mode B).
        pub async fn launch(opts: NativeOptions, sink: FrameSink) -> Result<Arc<Self>> {
            anyhow::ensure!(opts.host != NativeHost::CurrentSession, "current_session 은 Windows 전용입니다");
            crate::macos::preflight()?;
            // 실행부터 Inner 생성·첫 창 대기까지 전부 작업 스레드 안에서 한다.
            // 이 future 가 중간에 취소되어도(에이전트 중지 등) 작업 결과인 Arc<Inner> 는 런타임이 버리면서
            // Inner::drop → shutdown_sync 가 돌아 우리가 띄운 프로세스가 남지 않는다.
            let inner = blocking(move || -> Result<Arc<Inner>> {
                let st = start_app(&opts)?;
                let inner = Arc::new(Inner {
                    pid: st.pid,
                    bundle_id: st.bundle_id,
                    mode: st.mode,
                    prior_hidden: st.prior_hidden,
                    app_name: st.name,
                    opts,
                    state: Mutex::new(State::default()),
                    filter: Mutex::new(None),
                    size: Mutex::new((1280, 800)),
                    closed: AtomicBool::new(false),
                    shut: AtomicBool::new(false),
                    sink,
                    exposure: Mutex::new(st.exposure.clone()),
                    guard: Mutex::new(st.guard),
                });
                if !st.exposure.is_empty() {
                    inner.note(format!("실행 중 창이 잠깐 화면에 노출되어 즉시 숨겼습니다: {}", st.exposure.join(", ")));
                }
                // 창이 생길 때까지 기다린다 (최대 10초)
                let t = Instant::now();
                while t.elapsed() < Duration::from_secs(10) {
                    inner.guard_exposure();
                    inner.refresh_windows(false);
                    if !inner.state.lock().windows.is_empty() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
                inner.ensure_current();
                Ok(inner)
            })
            .await??;
            let ws = Arc::new(NativeAppWorkspace { inner: inner.clone() });
            if inner.opts.live_fps > 0.0 {
                tokio::spawn(live_loop(Arc::downgrade(&inner), inner.opts.live_fps));
            }
            Ok(ws)
        }

        pub fn pid(&self) -> i32 {
            self.inner.pid
        }

        pub fn mode(&self) -> NativeMode {
            self.inner.mode
        }

        pub fn app_name(&self) -> String {
            self.inner.app_name.clone()
        }

        /// 실행 이후 측정된 창 노출 기록 (비어 있으면 노출 없음)
        pub fn exposure_report(&self) -> Vec<String> {
            self.inner.collect_guard_events();
            self.inner.exposure.lock().clone()
        }

        // ───────────── 계약 메서드 ─────────────

        pub fn size(&self) -> (u32, u32) {
            *self.inner.size.lock()
        }

        pub async fn screenshot(&self) -> Result<Vec<u8>> {
            let i = self.inner.clone();
            let png = blocking(move || i.screenshot_png()).await??;
            (self.inner.sink)(&png);
            Ok(png)
        }

        pub async fn zoom(&self, region: [f64; 4]) -> Result<Vec<u8>> {
            let i = self.inner.clone();
            blocking(move || i.zoom_png(region)).await?
        }

        pub async fn mouse_move(&self, x: f64, y: f64, _mods: i64) -> Result<()> {
            let mut st = self.inner.state.lock();
            st.mouse = (x, y);
            if st.down.is_some() {
                // 드래그 중 — mouse_up 에서 오류를 낸다
            }
            Ok(())
        }

        pub async fn mouse_down(&self, b: MouseButton, _count: i64, _mods: i64) -> Result<()> {
            let mut st = self.inner.state.lock();
            st.down = Some((st.mouse, b));
            Ok(())
        }

        pub async fn mouse_up(&self, b: MouseButton, count: i64, mods: i64) -> Result<()> {
            let (down, pos) = {
                let mut st = self.inner.state.lock();
                (st.down.take(), st.mouse)
            };
            let Some((dpos, _)) = down else {
                bail!("mouse_down 없이 mouse_up 이 왔습니다");
            };
            if (dpos.0 - pos.0).abs() > 4.0 || (dpos.1 - pos.1).abs() > 4.0 {
                bail!("{}", DRAG_ERR);
            }
            self.click(Some(pos), b, count.max(1) as u32, mods).await
        }

        pub async fn click(&self, pos: Option<(f64, f64)>, b: MouseButton, count: u32, mods: i64) -> Result<()> {
            let pos = pos.unwrap_or_else(|| self.inner.state.lock().mouse);
            self.inner.state.lock().mouse = pos;
            if b == MouseButton::Right || mods & MOD_CTRL != 0 {
                bail!(
                    "숨김 모드에서는 오른쪽 클릭(컨텍스트 메뉴)을 할 수 없습니다 — 메뉴가 사용자 화면에 그려지기 때문입니다. \
                     메뉴 막대 항목은 app_control menu, 요소 동작은 app_control press 를 쓰세요."
                );
            }
            if b == MouseButton::Middle {
                bail!("숨김 모드에서는 가운데 버튼 클릭을 지원하지 않습니다");
            }
            let i = self.inner.clone();
            let note = blocking(move || i.semantic_click(pos.0, pos.1, count.max(1), mods)).await??;
            self.inner.note(note);
            Ok(())
        }

        pub async fn drag(&self, _from: (f64, f64), _to: (f64, f64), _mods: i64) -> Result<()> {
            bail!("{}", DRAG_ERR)
        }

        pub async fn scroll(&self, pos: Option<(f64, f64)>, dx: f64, dy: f64, _mods: i64) -> Result<()> {
            let pos = pos.unwrap_or_else(|| self.inner.state.lock().mouse);
            let i = self.inner.clone();
            let note = blocking(move || i.semantic_scroll(pos.0, pos.1, dx, dy)).await??;
            self.inner.note(note);
            Ok(())
        }

        pub async fn key(&self, combo: &str, repeat: u32) -> Result<()> {
            let i = self.inner.clone();
            let combo = combo.to_string();
            let note = blocking(move || i.key_combo(&combo, repeat.max(1), Duration::from_millis(15))).await??;
            if let Some(n) = note {
                self.inner.note(n);
            }
            Ok(())
        }

        pub async fn hold_key(&self, combo: &str, secs: f64) -> Result<()> {
            let i = self.inner.clone();
            let combo = combo.to_string();
            let hold = Duration::from_secs_f64(secs.clamp(0.0, 10.0));
            let note = blocking(move || i.key_combo(&combo, 1, hold)).await??;
            if let Some(n) = note {
                self.inner.note(n);
            }
            Ok(())
        }

        pub async fn type_text(&self, text: &str) -> Result<()> {
            let i = self.inner.clone();
            let text = text.to_string();
            let note = blocking(move || i.type_text(&text)).await??;
            if let Some(n) = note {
                self.inner.note(n);
            }
            Ok(())
        }

        pub fn mouse_pos(&self) -> (f64, f64) {
            self.inner.state.lock().mouse
        }

        pub fn views(&self) -> (Vec<ViewInfo>, Option<String>) {
            let st = self.inner.state.lock();
            let v = st
                .windows
                .iter()
                .map(|w| ViewInfo {
                    id: w.wid.to_string(),
                    title: if w.title.is_empty() { "(제목 없음)".into() } else { w.title.clone() },
                    detail: format!(
                        "{} {}x{} (숨김 창)",
                        if w.subrole.is_empty() { "AXWindow" } else { &w.subrole },
                        w.frame.size.width as i64,
                        w.frame.size.height as i64
                    ),
                })
                .collect();
            (v, st.current.map(|c| c.to_string()))
        }

        pub async fn switch_view(&self, index: usize) -> Result<()> {
            let i = self.inner.clone();
            blocking(move || i.switch_window(index)).await??;
            Ok(())
        }

        pub fn take_notes(&self) -> Vec<String> {
            std::mem::take(&mut self.inner.state.lock().notes)
        }

        pub fn is_closed(&self) -> bool {
            self.inner.closed.load(Ordering::SeqCst) || !apps::is_running(self.inner.pid)
        }

        pub async fn shutdown(&self) {
            let i = self.inner.clone();
            let _ = blocking(move || i.shutdown_sync()).await;
        }

        /// LLM `app_control` 도구 실행. 결과 텍스트를 돌려준다.
        pub async fn app_control(&self, input: &Value) -> Result<String> {
            let i = self.inner.clone();
            let input = input.clone();
            blocking(move || i.app_control(&input)).await?
        }

        /// (진단용) 에뮬레이션 없이 SkyLight 키 창 레코드 + 키 이벤트만 보낸다.
        pub async fn raw_key(&self, combo: &str) -> Result<String> {
            let i = self.inner.clone();
            let combo = combo.to_string();
            blocking(move || i.raw_key(&combo)).await?
        }
    }

    impl Drop for Inner {
        /// shutdown 을 부르지 않고 버려져도 우리가 띄운 프로세스는 반드시 정리한다
        fn drop(&mut self) {
            self.shutdown_sync();
        }
    }

    const DRAG_ERR: &str = "숨김 모드에서는 드래그와 픽셀 단위 마우스 조작을 할 수 없습니다 (숨겨진 창에는 마우스 이벤트가 전달되지 않음). \
        스크롤은 scroll, 값 변경은 app_control set_value, 선택은 app_control select 를 쓰세요.";

    // ───────────────────────── 실행 / 연결 ─────────────────────────

    struct StartResult {
        pid: i32,
        bundle_id: Option<String>,
        mode: NativeMode,
        prior_hidden: bool,
        name: String,
        exposure: Vec<String>,
        guard: Option<ExposureGuard>,
    }

    fn start_app(o: &NativeOptions) -> Result<StartResult> {
        let target = apps::resolve_app(&o.app)?;
        if let Some(p) = o.attach_pid {
            // attach_pid 가 지정한 앱과 다른 프로세스(사용자의 다른 앱)를 가리키면 거부한다
            let actual = apps::bundle_id_of(p);
            match (&target.bundle_id, &actual) {
                (_, None) => bail!("pid {p} 는 실행 중인 앱이 아닙니다"),
                (Some(want), Some(got)) if want != got => {
                    bail!("pid {p} 는 {} 이(가) 아니라 {got} 입니다 — 연결하지 않았습니다", target.name)
                }
                _ => {}
            }
        }
        let running = match o.attach_pid {
            Some(p) => vec![p],
            None => apps::running_pids_for(&target),
        };
        if o.attach_pid.is_some() || (!running.is_empty() && !o.new_instance) {
            if !o.attach_consent {
                bail!(
                    "{} 이(가) 이미 실행 중입니다. 사용자의 실행 중인 앱에서 작업하려면 동의(attach)가 필요합니다 — \
                     동의하면 작업하는 동안 그 앱의 창이 화면에서 숨겨집니다. 또는 별도 인스턴스(new_instance)를 선택하세요.",
                    target.name
                );
            }
            let pid = running[0];
            let prior_hidden = apps::is_hidden(pid).ok_or_else(|| anyhow!("pid {pid} 앱을 찾지 못했습니다"))?;
            if !prior_hidden && !apps::hide(pid) {
                std::thread::sleep(Duration::from_millis(300));
                if apps::is_hidden(pid) != Some(true) {
                    bail!("{} 을(를) 숨기지 못했습니다", target.name);
                }
            }
            eprintln!("[ghostdesk] 네이티브 앱 연결 (Mode B): {} pid={pid} 원래 숨김={prior_hidden}", target.name);
            return Ok(StartResult {
                pid,
                bundle_id: target.bundle_id,
                mode: NativeMode::Attached,
                prior_hidden,
                name: target.name,
                exposure: vec![],
                guard: None,
            });
        }
        if !running.is_empty() && apps::multiple_instances_prohibited(&target.path) {
            // 새 인스턴스를 요청해도 기존(사용자) 프로세스가 돌아오고, hides=true 때문에 사용자의 앱이 숨겨진다
            bail!(
                "{} 은(는) 여러 인스턴스 실행을 허용하지 않는 앱이라 별도 인스턴스를 띄울 수 없습니다 (사용자의 앱이 숨겨질 수 있음). \
                 사용자의 실행 중인 앱에서 작업하려면 attach 동의가 필요합니다.",
                target.name
            );
        }
        // 만일 기존 프로세스가 돌아오면 숨김 상태를 되돌리기 위해 실행 전 상태를 기록한다
        let prior_visible: Vec<(i32, bool)> = running.iter().map(|p| (*p, apps::is_hidden(*p) == Some(false))).collect();
        let mut args: Vec<String> = DEFAULT_ARGS.iter().map(|s| s.to_string()).collect();
        args.extend(o.extra_args.iter().cloned());
        // 번들 id 를 몰라 running 이 비어 있어도 기존(사용자) 프로세스를 우리 것으로 착각하지 않도록
        // 실행 직전의 모든 앱 pid 를 기록해 둔다 (착각하면 종료 시 사용자의 앱을 kill 하게 된다)
        let mut before: HashSet<i32> = apps::all_running_pids();
        before.extend(running.iter().copied());
        let t0 = Instant::now();
        let pid = apps::launch_hidden(&target.path, &o.open_files, &args, o.new_instance || !running.is_empty())?;
        if before.contains(&pid) {
            // 우리 실행 요청(hides=true)이 사용자의 기존 앱을 숨겼다면 원래대로 되돌린다 (활성화는 하지 않음)
            if prior_visible.iter().any(|(p, vis)| *p == pid && *vis) && apps::is_hidden(pid) == Some(true) {
                apps::restore_unhide(pid);
                eprintln!("[ghostdesk] 실행 요청이 기존 앱(pid {pid})을 숨겨 원래대로 다시 표시함");
            }
            bail!(
                "{} 이(가) 새 인스턴스를 만들지 않고 기존 프로세스(pid {pid})를 돌려주었습니다 (단일 인스턴스 앱). \
                 사용자의 앱에서 작업하려면 attach 동의가 필요합니다.",
                target.name
            );
        }
        // 상시 감시기 (AXObserver + 30ms 폴링) 를 바로 켜고,
        // 실행 직후 4초 동안은 5ms 간격으로도 감시해 노출 시간을 정밀하게 잰다
        let guard = ExposureGuard::start(pid);
        let mut exposure = watch_exposure(pid, Duration::from_secs(4), t0);
        exposure.extend(guard.take_events());
        if apps::is_hidden(pid) == Some(false) {
            apps::hide(pid);
        }
        eprintln!(
            "[ghostdesk] 네이티브 앱 숨김 실행 (Mode A): {} pid={pid} 노출={}",
            target.name,
            if exposure.is_empty() { "없음".to_string() } else { exposure.join(", ") }
        );
        Ok(StartResult {
            pid,
            bundle_id: target.bundle_id.or_else(|| apps::bundle_id_of(pid)),
            mode: NativeMode::Launched,
            prior_hidden: true,
            name: target.name,
            exposure,
            guard: Some(guard),
        })
    }

    /// pid 의 창이 화면에 보이면 즉시 숨기고 노출 구간을 기록한다.
    fn watch_exposure(pid: i32, dur: Duration, t0: Instant) -> Vec<String> {
        let start = Instant::now();
        let mut out = Vec::new();
        let mut visible_since: Option<Instant> = None;
        while start.elapsed() < dur {
            let vis = apps::visible_windows(&[pid]);
            if !vis.is_empty() {
                if visible_since.is_none() {
                    visible_since = Some(Instant::now());
                }
                apps::hide(pid);
            } else if let Some(s) = visible_since.take() {
                out.push(format!(
                    "실행 {}ms 후 약 {}ms (5ms 샘플링)",
                    (s - t0).as_millis(),
                    s.elapsed().as_millis().max(5)
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if let Some(s) = visible_since {
            out.push(format!("실행 {}ms 후 {}ms 이상 (계속 보임)", (s - t0).as_millis(), s.elapsed().as_millis()));
        }
        out
    }

    // ───────────────────────── 내부 구현 ─────────────────────────

    impl Inner {
        fn note(&self, s: impl Into<String>) {
            let s = s.into();
            if !s.is_empty() {
                self.state.lock().notes.push(s);
            }
        }

        fn app(&self) -> El {
            El::app(self.pid)
        }

        /// 상시 감시기가 남긴 기록을 노출 목록과 노트로 옮긴다
        fn collect_guard_events(&self) {
            let ev = self.guard.lock().as_ref().map(|g| g.take_events()).unwrap_or_default();
            if !ev.is_empty() {
                for e in &ev {
                    eprintln!("[ghostdesk] 노출 감시: {e}");
                }
                self.exposure.lock().extend(ev.iter().cloned());
                self.note(format!("앱 창이 사용자 화면에 나타나려 해 즉시 숨겼습니다: {}", ev.join(" / ")));
            }
        }

        /// 우리 창이 사용자 화면에 보이면 즉시 숨긴다 (Mode A). Mode B 에서 사용자가 직접 다시 보이게 했다면 존중한다.
        fn guard_exposure(&self) {
            if self.shut.load(Ordering::SeqCst) {
                return;
            }
            self.collect_guard_events();
            let vis = apps::visible_windows(&[self.pid]);
            if vis.is_empty() {
                return;
            }
            if self.mode == NativeMode::Attached {
                let mut st = self.state.lock();
                if !st.user_unhid {
                    st.user_unhid = true;
                    st.notes.push(
                        "사용자가 앱을 다시 화면에 표시했습니다. 사용자가 직접 쓰는 중일 수 있으니 작업을 멈추고 확인을 요청하세요.".into(),
                    );
                }
                return;
            }
            let t = Instant::now();
            apps::hide(self.pid);
            while !apps::visible_windows(&[self.pid]).is_empty() && t.elapsed() < Duration::from_millis(500) {
                std::thread::sleep(Duration::from_millis(5));
            }
            let msg = format!(
                "창 {}개가 사용자 화면에 나타나 즉시 숨겼습니다 (약 {}ms)",
                vis.len(),
                t.elapsed().as_millis()
            );
            eprintln!("[ghostdesk] 경고: {msg}");
            self.exposure.lock().push(msg.clone());
            self.note(msg);
        }

        /// 창 목록 갱신. 새 창이 생기면 노트를 남기고 그 창으로 전환한다.
        fn refresh_windows(&self, announce: bool) {
            let app = self.app();
            let mut list = Vec::new();
            // AXWindows 가 정상적으로 빈 배열이면 "창 없음". 속성 자체를 못 읽을 때만 CGWindowList 로 대체한다
            // (숨은 앱에는 화면 밖 보조 창(저장 패널 캐시 등)이 남아 있어 CG 목록은 부정확하다).
            let ax_ok = app.get_raw("AXWindows").is_ok();
            for w in app.elements("AXWindows") {
                let Some(wid) = w.window_id() else { continue };
                let n = Node::read(&w);
                let Some(frame) = n.frame else { continue };
                if frame.size.width < 40.0 || frame.size.height < 30.0 {
                    continue;
                }
                list.push(WinEntry { wid, title: n.title.clone(), subrole: n.subrole.clone(), frame, el: Some(w) });
            }
            if list.is_empty() && !ax_ok {
                // AX 가 창을 주지 않는 앱: CGWindowList 로 대체 (의미 기반 조작은 제한됨)
                for c in apps::cg_windows(&[self.pid], false) {
                    if c.layer == 0 && c.bounds.size.width >= 100.0 && c.bounds.size.height >= 80.0 {
                        list.push(WinEntry { wid: c.wid, title: c.name, subrole: String::new(), frame: c.bounds, el: None });
                    }
                }
            }
            let mut st = self.state.lock();
            let old: HashSet<u32> = st.windows.iter().map(|w| w.wid).collect();
            let had_any = !st.windows.is_empty();
            let mut switch_to = None;
            if announce && had_any {
                for w in &list {
                    if !old.contains(&w.wid) {
                        st.notes.push(format!(
                            "새 창이 열려 그 창으로 전환했습니다: '{}' ({})",
                            w.title,
                            if w.subrole.is_empty() { "AXWindow" } else { &w.subrole }
                        ));
                        switch_to = Some(w.wid);
                    }
                }
                let closed: Vec<String> = st
                    .windows
                    .iter()
                    .filter(|w| !list.iter().any(|n| n.wid == w.wid))
                    .map(|w| format!("창이 닫혔습니다: '{}'", w.title))
                    .collect();
                st.notes.extend(closed);
            }
            st.windows = list;
            st.last_windows_refresh = Some(Instant::now());
            if let Some(w) = switch_to {
                st.current = Some(w);
                st.geom = None;
            }
            if let Some(c) = st.current {
                if !st.windows.iter().any(|w| w.wid == c) {
                    st.current = None;
                    st.geom = None;
                }
            }
        }

        /// 현재 창이 없으면 앱의 주 창(또는 첫 창)을 고른다.
        fn ensure_current(&self) -> Option<WinEntry> {
            {
                let st = self.state.lock();
                if let Some(c) = st.current {
                    if let Some(w) = st.windows.iter().find(|w| w.wid == c) {
                        return Some(w.clone());
                    }
                }
            }
            self.refresh_windows(true);
            let app = self.app();
            let pref = app
                .element("AXFocusedWindow")
                .or_else(|| app.element("AXMainWindow"))
                .and_then(|w| w.window_id());
            let mut st = self.state.lock();
            let pick = st
                .windows
                .iter()
                .find(|w| Some(w.wid) == pref)
                .or_else(|| st.windows.iter().find(|w| w.subrole == "AXStandardWindow"))
                .or_else(|| st.windows.first())
                .cloned();
            st.current = pick.as_ref().map(|w| w.wid);
            st.geom = None;
            pick
        }

        fn current_window(&self) -> Result<WinEntry> {
            self.ensure_current().ok_or_else(|| anyhow!("{} 에 열린 창이 없습니다", self.app_name))
        }

        fn current_el(&self) -> Result<(WinEntry, El)> {
            let w = self.current_window()?;
            let el = w.el.clone().ok_or_else(|| anyhow!("이 창은 접근성(AX) 정보를 제공하지 않아 의미 기반 조작을 할 수 없습니다"))?;
            Ok((w, el))
        }

        /// 현재 창을 캡처한다 (시트가 있으면 합성). (이미지, 좌표 변환)
        fn capture_current(&self, max_long_edge: u32) -> Result<(RgbImage, Geom)> {
            let w = self.current_window()?;
            // 캡처 시점의 실제 프레임 (AX), 없으면 CG
            let frame = w.el.as_ref().and_then(|e| e.frame()).unwrap_or(w.frame);
            let mut cache = self.filter.lock();
            let reuse = match cache.as_ref() {
                Some(f) => {
                    f.wid == Some(w.wid)
                        && f.created.elapsed() < Duration::from_secs(30)
                        && (f.content_rect.size.width - frame.size.width).abs() < 1.0
                        && (f.content_rect.size.height - frame.size.height).abs() < 1.0
                }
                None => false,
            };
            if !reuse {
                *cache = Some(capture::window_filter(w.wid)?);
            }
            let f = cache.as_ref().unwrap();
            let (tw, th) = capture::fit_size(frame.size.width, frame.size.height, f.scale, max_long_edge);
            let mut img = match f.capture(tw, th) {
                Ok(i) => i,
                Err(_) => {
                    // 필터가 낡았을 수 있다 → 새로 만들어 한 번 더
                    *cache = Some(capture::window_filter(w.wid)?);
                    cache.as_ref().unwrap().capture(tw, th)?
                }
            };
            drop(cache);
            let geom = Geom { wid: w.wid, frame, shot_w: img.width(), shot_h: img.height() };
            // 시트(저장 대화상자 등)는 별도 창이므로 겹쳐 그린다
            if let Some(el) = &w.el {
                for s in el.children().into_iter().filter(|c| c.role() == "AXSheet") {
                    let (Some(swid), Some(sf)) = (s.window_id(), s.frame()) else { continue };
                    if swid == w.wid {
                        continue;
                    }
                    if let Ok(sfilter) = capture::window_filter(swid) {
                        let r = geom.rect_to_shot(sf);
                        if r[2] > 0 && r[3] > 0 {
                            if let Ok(simg) = sfilter.capture_rgba(r[2] as u32, r[3] as u32) {
                                capture::blend_onto(&mut img, &simg, r[0], r[1]);
                            }
                        }
                    }
                }
            }
            Ok((img, geom))
        }

        fn screenshot_png(&self) -> Result<Vec<u8>> {
            self.guard_exposure();
            let (img, geom) = self.capture_current(self.opts.max_long_edge)?;
            *self.size.lock() = (geom.shot_w, geom.shot_h);
            self.state.lock().geom = Some(geom);
            capture::encode_png(&img)
        }

        fn geom(&self) -> Result<Geom> {
            let cur = self.current_window()?;
            if let Some(g) = self.state.lock().geom {
                if g.wid == cur.wid {
                    // 창이 움직였을 수 있으니 원점만 최신으로
                    let mut g = g;
                    if let Some(f) = cur.el.as_ref().and_then(|e| e.frame()) {
                        if (f.size.width - g.frame.size.width).abs() < 1.0 && (f.size.height - g.frame.size.height).abs() < 1.0 {
                            g.frame.origin = f.origin;
                        }
                    }
                    return Ok(g);
                }
            }
            // 아직 이 창을 찍지 않았다 → 한 장 찍어 좌표계를 만든다
            self.screenshot_png()?;
            self.state.lock().geom.ok_or_else(|| anyhow!("좌표계를 만들지 못했습니다"))
        }

        fn zoom_png(&self, region: [f64; 4]) -> Result<Vec<u8>> {
            let g = self.geom()?;
            let (full, fg) = self.capture_current(0)?;
            let k = fg.shot_w as f64 / g.shot_w.max(1) as f64;
            let (x0, y0) = (region[0].min(region[2]).max(0.0), region[1].min(region[3]).max(0.0));
            let (x1, y1) = (region[0].max(region[2]), region[1].max(region[3]));
            let cx = ((x0 * k) as u32).min(full.width().saturating_sub(1));
            let cy = ((y0 * k) as u32).min(full.height().saturating_sub(1));
            let cw = (((x1 - x0) * k).max(1.0) as u32).min(full.width() - cx);
            let ch = (((y1 - y0) * k).max(1.0) as u32).min(full.height() - cy);
            let crop = image::imageops::crop_imm(&full, cx, cy, cw, ch).to_image();
            let target = self.opts.max_long_edge.max(640) as f64;
            let scale = (target / cw.max(ch) as f64).clamp(1.0, 8.0);
            let out = image::imageops::resize(
                &crop,
                ((cw as f64 * scale) as u32).max(1),
                ((ch as f64 * scale) as u32).max(1),
                image::imageops::FilterType::CatmullRom,
            );
            capture::encode_png(&out)
        }

        fn switch_window(&self, index: usize) -> Result<String> {
            self.refresh_windows(false);
            let mut st = self.state.lock();
            let w = st
                .windows
                .get(index)
                .cloned()
                .ok_or_else(|| anyhow!("창 번호 {index} 가 없습니다 (창 {}개)", st.windows.len()))?;
            st.current = Some(w.wid);
            st.geom = None;
            Ok(format!("창 [{index}] '{}' 로 전환했습니다", w.title))
        }

        // ───────────── 의미 기반 클릭 ─────────────

        /// 입력(클릭·스크롤)용 좌표계. 에이전트가 마지막으로 본 스크린샷의 창이 지금 창과 같아야 한다.
        /// 창이 바뀐 뒤(새 창 자동 전환, switch_window) 아직 스크린샷이 없으면 예전 스크린샷 기준 좌표를
        /// 새 창에 잘못 적용하지 않도록(엉뚱한 버튼을 누르게 됨) 오류를 낸다.
        fn input_geom(&self) -> Result<(Geom, El)> {
            let cur = self.current_window()?;
            let g = self.state.lock().geom;
            let Some(mut g) = g.filter(|g| g.wid == cur.wid) else {
                bail!(
                    "현재 창 '{}' 의 스크린샷을 아직 보지 않았습니다 (새 창으로 자동 전환되었거나 창을 바꿈). \
                     screenshot 으로 화면을 다시 확인한 뒤 좌표를 지정하세요.",
                    cur.title
                );
            };
            let el = cur
                .el
                .clone()
                .ok_or_else(|| anyhow!("이 창은 접근성(AX) 정보를 제공하지 않아 의미 기반 조작을 할 수 없습니다"))?;
            // 창이 움직였을 수 있으니 크기가 같으면 원점만 최신으로
            if let Some(f) = el.frame() {
                if (f.size.width - g.frame.size.width).abs() < 1.0 && (f.size.height - g.frame.size.height).abs() < 1.0 {
                    g.frame.origin = f.origin;
                }
            }
            Ok((g, el))
        }

        fn semantic_click(&self, x: f64, y: f64, count: u32, mods: i64) -> Result<String> {
            let (g, win) = self.input_geom()?;
            let p = g.to_global(x, y);
            if !contains(&g.frame, p) {
                bail!("좌표 ({x:.0},{y:.0}) 가 창 밖입니다 (스크린샷 크기 {}x{})", g.shot_w, g.shot_h);
            }
            let path = hit_path(&win, p);
            let deepest = path.last().unwrap();
            let mut tried = Vec::new();
            // 가장 깊은 요소부터 최대 6단계 위까지 동작 가능한 요소를 찾는다
            for n in path.iter().rev().take(6) {
                let role = n.role.as_str();
                if role == "AXWindow" || role == "AXSheet" && n.el == win {
                    break;
                }
                if !n.enabled && (is_press_role(role) || is_text_role(role)) {
                    bail!("클릭한 {} 이(가) 비활성화되어 있습니다", n.describe());
                }
                if is_text_role(role) || n.subrole == "AXSearchField" {
                    return self.click_text(n, p, count);
                }
                if is_popup_role(role) {
                    let opts = popup_options(&n.el);
                    bail!(
                        "{} 는 팝업 메뉴입니다. 숨김 모드에서 메뉴를 열면 사용자 화면에 그려지므로 열지 않았습니다. {}",
                        n.describe(),
                        if opts.is_empty() {
                            "app_control ui_tree 로 id 를 확인한 뒤 app_control choose {id, option} 을 시도하세요.".to_string()
                        } else {
                            format!("선택지: [{}] — app_control ui_tree 로 id 를 확인한 뒤 app_control choose {{id, option}} 을 쓰세요.", opts.join(", "))
                        }
                    );
                }
                if is_press_role(role) {
                    return self.press_node(n, count);
                }
                if role == "AXRow" || role == "AXOutlineRow" {
                    return self.select_row(n, count);
                }
                if role == "AXSlider" {
                    return self.set_slider(n, p);
                }
                if role == "AXTabGroup" || role == "AXScrollArea" || role == "AXSplitGroup" || role == "AXTable" || role == "AXOutline" || role == "AXList" {
                    break;
                }
                let acts = n.el.actions();
                if acts.iter().any(|a| a == "AXPress") && !matches!(role, "AXGroup" | "AXCell" | "AXStaticText" | "AXImage" | "AXLayoutArea") {
                    return self.press_node(n, count);
                }
                tried.push(n.describe());
            }
            // 표 안의 셀/글자 → 행 선택
            if let Some(row) = path.iter().rev().find(|n| n.role == "AXRow" || n.role == "AXOutlineRow") {
                return self.select_row(row, count);
            }
            // 일반 요소라도 AXPress 를 지원하면 누른다
            for n in path.iter().rev().take(4) {
                if n.el.actions().iter().any(|a| a == "AXPress") && n.role != "AXWindow" {
                    return self.press_node(n, count);
                }
            }
            let _ = mods;
            bail!(
                "({x:.0},{y:.0}) 의 요소 {} 에는 수행할 수 있는 접근성 동작이 없습니다 (경로: {}). \
                 app_control ui_tree 로 누를 수 있는 요소를 확인하세요.",
                deepest.describe(),
                path.iter().map(|n| n.role.as_str()).collect::<Vec<_>>().join(" > ")
            )
        }

        fn press_node(&self, n: &Node, count: u32) -> Result<String> {
            if n.role == "AXLink" {
                bail!(
                    "{} 는 링크입니다. 누르면 브라우저 등 다른 앱이 사용자 화면 앞에 열리므로 누르지 않았습니다. \
                     주소·내용은 app_control read 로 확인하세요.",
                    n.describe()
                );
            }
            if matches!(n.subrole.as_str(), "AXFullScreenButton" | "AXMinimizeButton") {
                bail!(
                    "{} ({}) 는 창을 전체 화면(새 Space)으로 보내거나 Dock 으로 최소화해 사용자 화면에 영향을 주므로 누르지 않았습니다",
                    n.describe(),
                    n.subrole
                );
            }
            if n.role == "AXButton" && [&n.title, &n.desc].iter().any(|t| HELP_TITLES.contains(&norm(t).as_str())) {
                bail!("{} 는 도움말 버튼입니다. {R_HELP}", n.describe());
            }
            let acts = n.el.actions();
            if count >= 2 && acts.iter().any(|a| a == "AXOpen") {
                let e = n.el.perform("AXOpen");
                return Ok(format!("더블클릭 → {} 에 AXOpen ({})", n.describe(), ax::err_name(e)));
            }
            let before = n.el.string("AXValue");
            let mut codes = Vec::new();
            for _ in 0..count.min(3) {
                codes.push(ax::err_name(n.el.perform("AXPress")));
                std::thread::sleep(Duration::from_millis(60));
            }
            let after = n.el.string("AXValue");
            let changed = if n.role == "AXCheckBox" || n.role == "AXRadioButton" {
                format!(" (값 {} → {})", before.unwrap_or_default(), after.unwrap_or_default())
            } else {
                String::new()
            };
            Ok(format!("클릭 → {} 에 AXPress{}{} ", n.describe(), if count > 1 { format!(" x{}", codes.len()) } else { String::new() }, changed)
                + &format!("[{}]", codes.join(",")))
        }

        fn click_text(&self, n: &Node, p: CGPoint, count: u32) -> Result<String> {
            let el = &n.el;
            let fe = el.set_bool("AXFocused", true);
            std::thread::sleep(Duration::from_millis(40));
            let value = el.string("AXValue").unwrap_or_default();
            let len = utf16_len(&value);
            let idx = el.range_for_position(p).map(|r| r.location.clamp(0, len)).unwrap_or(len);
            let range = match count {
                2 => word_range(&value, idx),
                c if c >= 3 => line_range(&value, idx),
                _ => CFRange { location: idx, length: 0 },
            };
            let se = el.set_range("AXSelectedTextRange", range);
            std::thread::sleep(Duration::from_millis(30));
            let focused = el.bool("AXFocused").unwrap_or(false);
            let sel = el.range("AXSelectedTextRange");
            let ok_sel = sel.map(|s| s.location == range.location && s.length == range.length).unwrap_or(false);
            let what = match count {
                2 => "단어 선택",
                c if c >= 3 => "줄 선택",
                _ => "커서 배치",
            };
            Ok(format!(
                "클릭 → {} 에 포커스{} + {what} (위치 {}/{}{})",
                n.describe(),
                if focused { "" } else { "(포커스 확인 안 됨)" },
                range.location,
                len,
                if ok_sel { String::new() } else { format!(", 선택 반영 확인 안 됨: {} / {}", ax::err_name(fe), ax::err_name(se)) }
            ))
        }

        fn select_row(&self, n: &Node, count: u32) -> Result<String> {
            let e = n.el.set_bool("AXSelected", true);
            std::thread::sleep(Duration::from_millis(50));
            let mut ok = n.el.bool("AXSelected").unwrap_or(false);
            if !ok {
                // 표에 AXSelectedRows 로 지정
                if let Some(table) = n.el.parent() {
                    let arr = objc2_core_foundation::CFArray::from_objects(&[&*n.el.0]);
                    let _ = table.set("AXSelectedRows", &arr);
                    std::thread::sleep(Duration::from_millis(50));
                    ok = n.el.bool("AXSelected").unwrap_or(false);
                }
            }
            let mut msg = format!(
                "클릭 → 행 선택 {}{}",
                row_label(&n.el),
                if ok { String::new() } else { format!(" (선택 확인 안 됨: {})", ax::err_name(e)) }
            );
            if count >= 2 {
                if n.el.actions().iter().any(|a| a == "AXOpen") {
                    let e = n.el.perform("AXOpen");
                    msg.push_str(&format!(" + AXOpen ({})", ax::err_name(e)));
                } else {
                    msg.push_str(" — 더블클릭 동작은 접근성으로 할 수 없어 선택만 했습니다. 열려면 Return 키를 눌러 보세요.");
                }
            }
            Ok(msg)
        }

        fn set_slider(&self, n: &Node, p: CGPoint) -> Result<String> {
            let f = n.frame.ok_or_else(|| anyhow!("슬라이더 프레임 없음"))?;
            let min = n.el.number("AXMinValue").unwrap_or(0.0);
            let max = n.el.number("AXMaxValue").unwrap_or(1.0);
            let vertical = f.size.height > f.size.width;
            let frac = if vertical {
                1.0 - (p.y - f.origin.y) / f.size.height.max(1.0)
            } else {
                (p.x - f.origin.x) / f.size.width.max(1.0)
            };
            let v = min + (max - min) * frac.clamp(0.0, 1.0);
            let e = n.el.set_number("AXValue", v);
            std::thread::sleep(Duration::from_millis(40));
            Ok(format!(
                "클릭 → 슬라이더 {} 값을 {v:.2} 로 설정 (확인: {}, {})",
                n.describe(),
                n.el.string("AXValue").unwrap_or_default(),
                ax::err_name(e)
            ))
        }

        // ───────────── 스크롤 ─────────────

        fn semantic_scroll(&self, x: f64, y: f64, dx: f64, dy: f64) -> Result<String> {
            let (g, win) = self.input_geom()?;
            let p = g.to_global(x, y);
            let path = hit_path(&win, p);
            let area = path
                .iter()
                .rev()
                .find(|n| n.role == "AXScrollArea")
                .map(|n| n.el.clone())
                .or_else(|| find_role(&win, "AXScrollArea", 12))
                .ok_or_else(|| anyhow!("이 위치에 스크롤할 수 있는 영역(AXScrollArea)이 없습니다"))?;
            let area_frame = area.frame().ok_or_else(|| anyhow!("스크롤 영역 프레임 없음"))?;
            let doc = area.elements("AXContents").into_iter().next().or_else(|| area.children().into_iter().find(|c| {
                !matches!(c.role().as_str(), "AXScrollBar")
            }));
            let doc_size = doc.as_ref().and_then(|d| d.frame()).map(|f| f.size).unwrap_or(CGSize { width: 0.0, height: 0.0 });
            let mut out = Vec::new();
            for (delta_px, bar_attr, doc_len, vis_len, name) in [
                (dy * g.sy(), "AXVerticalScrollBar", doc_size.height, area_frame.size.height, "세로"),
                (dx * g.sx(), "AXHorizontalScrollBar", doc_size.width, area_frame.size.width, "가로"),
            ] {
                if delta_px.abs() < 0.5 {
                    continue;
                }
                let Some(bar) = area.element(bar_attr) else {
                    out.push(format!("{name} 스크롤 막대 없음 (더 스크롤할 내용이 없을 수 있음)"));
                    continue;
                };
                if bar.bool("AXEnabled") == Some(false) || doc_len <= vis_len + 1.0 {
                    out.push(format!("{name} 방향으로 더 스크롤할 내용이 없습니다"));
                    continue;
                }
                let old = bar.number("AXValue").unwrap_or(0.0);
                let range = (doc_len - vis_len).max(1.0);
                let new = (old + delta_px / range).clamp(0.0, 1.0);
                let e = bar.set_number("AXValue", new);
                std::thread::sleep(Duration::from_millis(50));
                let now = bar.number("AXValue").unwrap_or(old);
                if (now - old).abs() < 1e-4 && (new - old).abs() > 1e-4 {
                    // 값 설정이 안 먹으면 페이지 단위 동작
                    let act = if delta_px > 0.0 { "AXIncrement" } else { "AXDecrement" };
                    let e2 = bar.perform(act);
                    std::thread::sleep(Duration::from_millis(50));
                    let now2 = bar.number("AXValue").unwrap_or(now);
                    out.push(format!("{name} 스크롤: 값 설정 실패({}) → {act} ({}) 위치 {:.0}% → {:.0}%", ax::err_name(e), ax::err_name(e2), old * 100.0, now2 * 100.0));
                } else {
                    out.push(format!("{name} 스크롤 위치 {:.0}% → {:.0}%", old * 100.0, now * 100.0));
                }
            }
            if out.is_empty() {
                out.push("스크롤 양이 0 입니다".into());
            }
            Ok(format!("스크롤 → {}", out.join(", ")))
        }

        // ───────────── 키보드 ─────────────

        fn focused_text(&self) -> Option<El> {
            let f = self.app().element("AXFocusedUIElement")?;
            let r = f.role();
            (is_text_role(&r) || f.settable("AXSelectedText") || r == "AXWebArea").then_some(f)
        }

        fn type_text(&self, text: &str) -> Result<Option<String>> {
            if text.is_empty() {
                return Ok(None);
            }
            let focused = self.focused_text();
            let before = focused.as_ref().and_then(|f| f.string("AXValue")).unwrap_or_default();
            input::pid_type_text(self.pid, text)?;
            let Some(f) = focused else {
                return Ok(Some("글자를 보냈지만 포커스된 텍스트 요소가 없어 입력 반영을 확인하지 못했습니다".into()));
            };
            // 반영 확인 (최대 600ms)
            let want = text.replace('\r', "\n");
            let t = Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(60));
                let now = f.string("AXValue").unwrap_or_default();
                if now != before {
                    if now.contains(want.trim_end_matches('\n')) || utf16_len(&now) - utf16_len(&before) >= utf16_len(&want) {
                        return Ok(None);
                    }
                }
                if t.elapsed() > Duration::from_millis(600) {
                    break;
                }
            }
            let now = f.string("AXValue").unwrap_or_default();
            if now != before {
                return Ok(Some("입력은 반영되었지만 내용이 예상과 다를 수 있습니다 — 스크린샷으로 확인하세요".into()));
            }
            // 이벤트가 안 먹었다 → AXSelectedText 로 삽입
            if f.settable("AXSelectedText") {
                let e = f.set_string("AXSelectedText", &want);
                std::thread::sleep(Duration::from_millis(60));
                let after = f.string("AXValue").unwrap_or_default();
                if after != before {
                    return Ok(Some(
                        "키 이벤트가 반영되지 않아 접근성 삽입(AXSelectedText)으로 입력했습니다 (앱의 입력 이벤트 처리가 생략될 수 있음)".into(),
                    ));
                }
                bail!("글자 입력이 반영되지 않았습니다 (키 이벤트와 AXSelectedText({}) 모두 실패)", ax::err_name(e));
            }
            bail!("글자 입력이 반영되지 않았습니다 — 포커스된 요소가 입력을 받지 않습니다")
        }

        fn key_combo(&self, combo: &str, repeat: u32, hold: Duration) -> Result<Option<String>> {
            let c = input::parse_combo(combo)?;
            if !c.is_shortcut() {
                // 글자 키(수정키 없음/shift 만)는 유니코드로 보낸다 → 입력기 상태와 무관
                if let (Some(ch), true) = (c.ch, hold <= Duration::from_millis(15)) {
                    if c.keycode != 49 || c.mods == 0 {
                        let ch = if c.mods & MOD_SHIFT != 0 { shifted_char(ch) } else { ch };
                        let s: String = std::iter::repeat_n(ch, repeat as usize).collect();
                        input::pid_type_text(self.pid, &s)?;
                        return Ok(None);
                    }
                }
                for _ in 0..repeat {
                    input::pid_combo(self.pid, &c, hold)?;
                }
                return Ok(None);
            }
            // ── 단축키 ──
            if let Some(r) = system_combo_block(&c) {
                bail!("{combo}: {r}");
            }
            let primary = c.mods & (MOD_META | MOD_CTRL) != 0 && c.mods & MOD_ALT == 0;
            if primary {
                if let Some(ch) = c.ch {
                    if let Some(r) = self.emulate_edit(ch, c.mods & MOD_SHIFT != 0)? {
                        return Ok(Some(r));
                    }
                }
            }
            // 같은 단축키를 가진 메뉴 항목 (차단 항목과 겹치면 키 이벤트로도 보내지 않는다 — 앱이 키 창이면
            // 키 이벤트가 그 메뉴 항목을 실행하기 때문)
            if let Some(ch) = c.ch {
                if let Some((path, item, blocked)) = find_menu_by_shortcut(&self.app(), ch, c.mods) {
                    if let Some(r) = blocked {
                        bail!("{combo} 는 메뉴 '{path}' 의 단축키입니다. {r}");
                    }
                    if c.mods & MOD_META != 0 {
                        let e = item.perform("AXPress");
                        return Ok(Some(format!(
                            "{combo} → 메뉴 '{path}' 실행 ({}). 숨김 모드에서는 일부 메뉴가 조용히 무시될 수 있으니 결과를 확인하세요.",
                            ax::err_name(e)
                        )));
                    }
                }
            }
            let r = self.raw_key(combo)?;
            Ok(Some(format!(
                "{r}. 숨김 상태로 시작한 앱은 단축키를 무시하는 경우가 많습니다 — 효과가 없으면 app_control menu / press 를 쓰세요."
            )))
        }

        /// SkyLight 키 창 레코드(대상 PSN 전용) + 키 이벤트
        fn raw_key(&self, combo: &str) -> Result<String> {
            let c = input::parse_combo(combo)?;
            if c.is_shortcut() {
                if let Some(r) = system_combo_block(&c) {
                    bail!("{combo}: {r}");
                }
                if let Some(ch) = c.ch {
                    if let Some((path, _, Some(r))) = find_menu_by_shortcut(&self.app(), ch, c.mods) {
                        bail!("{combo} 는 메뉴 '{path}' 의 단축키입니다. {r}");
                    }
                }
            }
            let wid = self.state.lock().current;
            let rec = if self.mode == NativeMode::Attached {
                // 안전 규칙: SkyLight 레코드는 우리가 띄운 프로세스에만 보낸다 (Mode B 의 사용자 앱에는 보내지 않음)
                "생략 — Mode B".to_string()
            } else {
                match wid {
                    Some(w) => skylight::post_key_window_records(self.pid, w).map(|v| format!("{v:?}")).unwrap_or_else(|e| e),
                    None => "창 없음".into(),
                }
            };
            std::thread::sleep(Duration::from_millis(40));
            input::pid_combo(self.pid, &c, Duration::from_millis(20))?;
            Ok(format!("{combo} 키 이벤트를 보냈습니다 (키 창 레코드 {rec})"))
        }

        /// Cmd/Ctrl + A/C/X/V/Z 를 포커스된 텍스트 요소에 접근성으로 흉내낸다. 해당 없으면 None.
        fn emulate_edit(&self, ch: char, shift: bool) -> Result<Option<String>> {
            let ch = ch.to_ascii_lowercase();
            if !matches!(ch, 'a' | 'c' | 'x' | 'v') {
                return Ok(None);
            }
            let Some(f) = self.focused_text() else { return Ok(None) };
            let value = f.string("AXValue").unwrap_or_default();
            let len = utf16_len(&value);
            match (ch, shift) {
                ('a', false) => {
                    let e = f.set_range("AXSelectedTextRange", CFRange { location: 0, length: len });
                    std::thread::sleep(Duration::from_millis(30));
                    let ok = f.range("AXSelectedTextRange").map(|r| r.location == 0 && r.length == len).unwrap_or(false);
                    Ok(Some(format!(
                        "전체 선택 ({len}자){}",
                        if ok { String::new() } else { format!(" — 확인 안 됨 ({})", ax::err_name(e)) }
                    )))
                }
                ('c', false) | ('x', false) => {
                    let sel = f.string("AXSelectedText").unwrap_or_default();
                    if sel.is_empty() {
                        return Ok(Some("선택된 글자가 없어 복사하지 않았습니다".into()));
                    }
                    self.state.lock().clipboard = sel.clone();
                    if ch == 'x' {
                        let e = f.set_string("AXSelectedText", "");
                        std::thread::sleep(Duration::from_millis(40));
                        let after = f.string("AXValue").unwrap_or_default();
                        if after == value {
                            bail!("잘라내기가 반영되지 않았습니다 ({})", ax::err_name(e));
                        }
                        return Ok(Some(format!("{}자를 잘라내 내부 클립보드에 담았습니다 (시스템 클립보드는 건드리지 않음)", sel.chars().count())));
                    }
                    Ok(Some(format!("{}자를 내부 클립보드에 복사했습니다 (시스템 클립보드는 건드리지 않음)", sel.chars().count())))
                }
                ('v', false) => {
                    let clip = self.state.lock().clipboard.clone();
                    if clip.is_empty() {
                        bail!("내부 클립보드가 비어 있습니다. 사용자의 시스템 클립보드는 읽지 않습니다 — 넣을 글자는 type 으로 직접 입력하세요.");
                    }
                    let e = f.set_string("AXSelectedText", &clip);
                    std::thread::sleep(Duration::from_millis(40));
                    let after = f.string("AXValue").unwrap_or_default();
                    if after == value {
                        // 접근성 삽입이 안 되면 키 이벤트로
                        input::pid_type_text(self.pid, &clip)?;
                        return Ok(Some(format!("붙여넣기: 접근성 삽입 실패({}) → 글자 입력으로 대체", ax::err_name(e))));
                    }
                    Ok(Some(format!("내부 클립보드의 {}자를 붙여넣었습니다", clip.chars().count())))
                }
                _ => Ok(None),
            }
        }

        // ───────────── app_control ─────────────

        fn app_control(&self, input: &Value) -> Result<String> {
            let action = input["action"].as_str().unwrap_or_default();
            let id = || -> Result<El> {
                let id = input["id"].as_u64().ok_or_else(|| anyhow!("id 가 필요합니다"))? as usize;
                let st = self.state.lock();
                if st.ui_ids.is_empty() {
                    bail!("먼저 ui_tree 를 호출하세요");
                }
                st.ui_ids
                    .get(id.wrapping_sub(1))
                    .cloned()
                    .ok_or_else(|| anyhow!("id {id} 가 없습니다 (최근 ui_tree 기준 1..{})", st.ui_ids.len()))
            };
            match action {
                "list_windows" => {
                    self.refresh_windows(true);
                    self.ensure_current();
                    let st = self.state.lock();
                    let mut s = String::new();
                    for (i, w) in st.windows.iter().enumerate() {
                        s.push_str(&format!(
                            "{} [{i}] '{}' {} {}x{}\n",
                            if Some(w.wid) == st.current { "▶" } else { " " },
                            w.title,
                            w.subrole,
                            w.frame.size.width as i64,
                            w.frame.size.height as i64
                        ));
                    }
                    if s.is_empty() {
                        s = "열린 창이 없습니다".into();
                    }
                    Ok(s)
                }
                "switch_window" => {
                    let idx = input["index"].as_u64().ok_or_else(|| anyhow!("index 가 필요합니다"))? as usize;
                    self.switch_window(idx)
                }
                "ui_tree" => {
                    let depth = input["max_depth"].as_u64().unwrap_or(30).clamp(1, 80) as usize;
                    self.ui_tree(depth, 500)
                }
                "press" => {
                    let el = id()?;
                    let n = Node::read(&el);
                    if is_popup_role(&n.role) {
                        bail!("팝업 버튼을 누르면 메뉴가 사용자 화면에 그려지므로 누르지 않았습니다. choose {{id, option}} 을 쓰세요.");
                    }
                    self.press_node(&n, 1)
                }
                "set_value" => {
                    let el = id()?;
                    let text = input["text"].as_str().ok_or_else(|| anyhow!("text 가 필요합니다"))?;
                    let n = Node::read(&el);
                    if !el.settable("AXValue") {
                        bail!("{} 의 값은 바꿀 수 없습니다 (AXValue 설정 불가)", n.describe());
                    }
                    let e = el.set_string("AXValue", text);
                    std::thread::sleep(Duration::from_millis(60));
                    let now = el.string("AXValue").unwrap_or_default();
                    if now == text {
                        Ok(format!("{} 의 값을 설정했습니다 ({}자, 확인됨)", n.describe(), text.chars().count()))
                    } else {
                        bail!("값 설정이 반영되지 않았습니다 ({}): 현재 값 '{}'", ax::err_name(e), short(&now, 80))
                    }
                }
                "focus" => {
                    let el = id()?;
                    let e = el.set_bool("AXFocused", true);
                    std::thread::sleep(Duration::from_millis(50));
                    let ok = el.bool("AXFocused").unwrap_or(false);
                    Ok(format!(
                        "{} 에 포커스 {}",
                        Node::read(&el).describe(),
                        if ok { "(확인됨)".to_string() } else { format!("(확인 안 됨: {})", ax::err_name(e)) }
                    ))
                }
                "select" => {
                    let el = id()?;
                    let n = Node::read(&el);
                    if n.role == "AXRadioButton" || n.role == "AXTab" {
                        return self.press_node(&n, 1);
                    }
                    self.select_row(&n, 1)
                }
                "choose" => {
                    let el = id()?;
                    let opt = input["option"].as_str().ok_or_else(|| anyhow!("option 이 필요합니다"))?;
                    self.choose(&el, opt)
                }
                "menu" => {
                    let path = input["path"].as_str().ok_or_else(|| anyhow!("path 가 필요합니다 (예: \"File > Save\")"))?;
                    self.menu(path)
                }
                "menus" => self.menus(),
                "read" => {
                    let el = id()?;
                    let n = Node::read(&el);
                    let mut s = format!("{}{}\n", n.role, if n.subrole.is_empty() { String::new() } else { format!("/{}", n.subrole) });
                    for (k, v) in [("title", &n.title), ("description", &n.desc), ("placeholder", &n.placeholder)] {
                        if !v.is_empty() {
                            s.push_str(&format!("{k}: {v}\n"));
                        }
                    }
                    if let Some(sel) = el.string("AXSelectedText") {
                        s.push_str(&format!("selected text: {sel}\n"));
                    }
                    s.push_str(&format!("value:\n{}", n.value));
                    Ok(s)
                }
                "raw_key" => self.raw_key(input["text"].as_str().unwrap_or_default()),
                other => bail!("알 수 없는 app_control action: {other}"),
            }
        }

        fn ui_tree(&self, max_depth: usize, max_nodes: usize) -> Result<String> {
            let g = self.geom()?;
            let (w, win) = self.current_el()?;
            let mut ids: Vec<El> = Vec::new();
            let mut lines = Vec::new();
            let mut visited = 0usize;
            let mut stack: Vec<(El, usize)> = vec![(win.clone(), 0)];
            let wframe = g.frame;
            while let Some((el, d)) = stack.pop() {
                visited += 1;
                if visited > 4000 || ids.len() >= max_nodes {
                    lines.push(format!("… (요소가 많아 {}개에서 멈춤)", ids.len()));
                    break;
                }
                let n = Node::read(&el);
                let visible = n.frame.map(|f| f.size.width > 0.0 && f.size.height > 0.0 && intersects(&f, &wframe)).unwrap_or(false);
                let label = n.label();
                let boring = matches!(
                    n.role.as_str(),
                    "AXGroup" | "AXSplitGroup" | "AXScrollArea" | "AXLayoutArea" | "AXUnknown" | "AXSplitter" | "AXColumn" | "AXWindow" | ""
                ) && label.is_empty();
                if visible && !boring && d > 0 {
                    ids.push(el.clone());
                    let r = g.rect_to_shot(n.frame.unwrap());
                    let mut line = format!(
                        "{}{} {}{}",
                        "  ".repeat(d.saturating_sub(1).min(12)),
                        ids.len(),
                        n.role,
                        if n.subrole.is_empty() || n.subrole == "AXStandardWindow" { String::new() } else { format!("/{}", n.subrole) }
                    );
                    if !n.title.is_empty() {
                        line.push_str(&format!(" \"{}\"", short(&n.title, 50)));
                    }
                    if !n.desc.is_empty() && n.desc != n.title {
                        line.push_str(&format!(" desc=\"{}\"", short(&n.desc, 50)));
                    }
                    if !n.value.is_empty() && n.value != n.title {
                        line.push_str(&format!(" value=\"{}\"", short(&n.value, 60)));
                    } else if is_text_role(&n.role) && !n.placeholder.is_empty() {
                        line.push_str(&format!(" placeholder=\"{}\"", short(&n.placeholder, 40)));
                    }
                    line.push_str(&format!(" [{},{},{},{}]", r[0], r[1], r[2], r[3]));
                    if !n.enabled {
                        line.push_str(" disabled");
                    }
                    if n.focused {
                        line.push_str(" focused");
                    }
                    lines.push(line);
                }
                if d < max_depth && (visible || n.frame.is_none() || d == 0) {
                    let children = kids(&n);
                    for c in children.into_iter().rev() {
                        stack.push((c, d + 1));
                    }
                }
            }
            self.state.lock().ui_ids = ids;
            Ok(format!(
                "창 '{}' (스크린샷 {}x{} 픽셀 좌표):\n{}",
                w.title,
                g.shot_w,
                g.shot_h,
                lines.join("\n")
            ))
        }

        fn choose(&self, el: &El, option: &str) -> Result<String> {
            let n = Node::read(el);
            let items = popup_items(el);
            if items.is_empty() {
                // 닫힌 팝업은 메뉴 항목을 노출하지 않는 경우가 많다 → 값 직접 설정 시도
                if el.settable("AXValue") {
                    let e = el.set_string("AXValue", option);
                    std::thread::sleep(Duration::from_millis(120));
                    let now = el.string("AXValue").unwrap_or_default();
                    if norm(&now) == norm(option) {
                        return Ok(format!("{} 의 값을 '{option}' 로 설정했습니다 (확인됨)", n.describe()));
                    }
                    bail!("{} 의 값 설정이 반영되지 않았습니다 ({}). 이 팝업은 숨김 모드에서 바꿀 수 없습니다.", n.describe(), ax::err_name(e));
                }
                bail!(
                    "{} 의 선택지는 메뉴를 열어야만 보이는데, 숨김 모드에서 메뉴를 열면 사용자 화면에 그려지므로 열 수 없습니다. \
                     같은 기능의 메뉴 막대 항목(app_control menus)이나 키보드를 쓰세요.",
                    n.describe()
                );
            }
            let want = norm(option);
            let Some((title, item)) = items.iter().find(|(t, _)| norm(t) == want).or_else(|| items.iter().find(|(t, _)| norm(t).contains(&want))) else {
                bail!("'{option}' 선택지가 없습니다. 선택지: [{}]", items.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join(", "));
            };
            let before = el.string("AXValue").or_else(|| el.string("AXTitle")).unwrap_or_default();
            let e = item.perform("AXPress");
            std::thread::sleep(Duration::from_millis(120));
            let after = el.string("AXValue").or_else(|| el.string("AXTitle")).unwrap_or_default();
            Ok(format!(
                "{} 에서 '{title}' 선택 ({}; 값 '{}' → '{}')",
                n.describe(),
                ax::err_name(e),
                short(&before, 30),
                short(&after, 30)
            ))
        }

        fn menu(&self, path: &str) -> Result<String> {
            let parts: Vec<String> = path.split('>').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            if parts.is_empty() {
                bail!("메뉴 경로가 비었습니다");
            }
            let app = self.app();
            let bar = app.element("AXMenuBar").ok_or_else(|| anyhow!("앱의 메뉴 막대를 읽지 못했습니다"))?;
            let tops = menu_tops(&bar);
            let want = norm(&parts[0]);
            let top = tops
                .iter()
                .find(|t| !t.title.is_empty() && norm(&t.title) == want)
                .or_else(|| tops.iter().find(|t| !t.title.is_empty() && norm(&t.title).starts_with(&want)))
                .ok_or_else(|| {
                    anyhow!(
                        "메뉴 막대에서 '{}' 를 찾지 못했습니다. 있는 메뉴: [{}]",
                        parts[0],
                        tops.iter().skip(1).map(|t| t.title.as_str()).collect::<Vec<_>>().join(", ")
                    )
                })?;
            if let Some(r) = top_block_reason(top) {
                bail!("메뉴 '{}': {r}", top.title);
            }
            if parts.len() == 1 {
                // 최상위 메뉴 막대 항목을 누르면 메뉴가 화면에 열린다 → 하위 항목까지 지정하게 한다
                bail!("메뉴 '{}' 의 항목까지 지정하세요 (예: \"{} > …\"). app_control menus 로 항목을 확인할 수 있습니다.", top.title, top.title);
            }
            let mut walked = vec![top.title.clone()];
            let mut items = submenu_items(&top.el);
            for (k, part) in parts.iter().enumerate().skip(1) {
                let titles: Vec<String> = items.iter().map(|e| e.string("AXTitle").unwrap_or_default()).collect();
                let want = norm(part);
                let found = titles
                    .iter()
                    .position(|t| !t.is_empty() && norm(t) == want)
                    .or_else(|| titles.iter().position(|t| !t.is_empty() && norm(t).starts_with(&want)));
                let Some(i) = found else {
                    bail!(
                        "메뉴 '{}' 에서 '{part}' 를 찾지 못했습니다. 있는 항목: [{}]",
                        walked.join(" > "),
                        titles.iter().filter(|t| !t.is_empty()).map(|t| t.as_str()).take(40).collect::<Vec<_>>().join(", ")
                    );
                };
                walked.push(titles[i].clone());
                if let Some(r) = item_block_reason(top, &items, i, k - 1) {
                    bail!("메뉴 '{}': {r}", walked.join(" > "));
                }
                let el = items[i].clone();
                let sub = submenu_items(&el);
                if k == parts.len() - 1 {
                    if !sub.is_empty() {
                        bail!(
                            "'{}' 는 하위 메뉴입니다 (누르면 메뉴가 화면에 열림). 하위 항목까지 지정하세요: [{}]",
                            walked.join(" > "),
                            sub.iter().filter_map(|e| e.string("AXTitle")).take(30).collect::<Vec<_>>().join(", ")
                        );
                    }
                    if el.bool("AXEnabled") == Some(false) {
                        bail!("메뉴 '{}' 가 비활성화되어 있습니다", walked.join(" > "));
                    }
                    let e = el.perform("AXPress");
                    return Ok(format!(
                        "메뉴 '{}' 실행 ({}). 숨김 모드에서는 키 창이 없어 편집 메뉴처럼 포커스된 요소에 작용하는 항목은 조용히 무시될 수 있습니다 — 결과를 확인하세요.",
                        walked.join(" > "),
                        ax::err_name(e)
                    ));
                }
                items = sub;
                if items.is_empty() {
                    bail!("'{}' 의 하위 메뉴를 읽지 못했습니다 (메뉴가 열릴 때만 채워지는 앱일 수 있음)", walked.join(" > "));
                }
            }
            unreachable!()
        }

        /// 메뉴 막대 구조 (2단계, 단축키 포함) — 지역화된 메뉴 이름을 알려주기 위함. Apple 메뉴는 보여주지 않는다.
        fn menus(&self) -> Result<String> {
            let bar = self.app().element("AXMenuBar").ok_or_else(|| anyhow!("앱의 메뉴 막대를 읽지 못했습니다"))?;
            let mut out = String::new();
            for top in menu_tops(&bar).iter().skip(1) {
                if top_block_reason(top).is_some() {
                    out.push_str(&format!("{}: [차단 — 사용자 화면에 영향]\n", top.title));
                    continue;
                }
                let items = submenu_items(&top.el);
                let mut shown = Vec::new();
                for i in 0..items.len() {
                    let it = &items[i];
                    let Some(title) = it.string("AXTitle") else { continue };
                    let sc = it.string("AXMenuItemCmdChar").map(|c| {
                        let m = it.number("AXMenuItemCmdModifiers").unwrap_or(0.0) as i64;
                        format!(
                            " ({}{}{}{}{c})",
                            if m & 4 != 0 { "ctrl+" } else { "" },
                            if m & 2 != 0 { "alt+" } else { "" },
                            if m & 1 != 0 { "shift+" } else { "" },
                            if m & 8 == 0 { "cmd+" } else { "" }
                        )
                    });
                    let dis = if item_block_reason(top, &items, i, 0).is_some() {
                        " [차단]"
                    } else if it.bool("AXEnabled") == Some(false) {
                        " [비활성]"
                    } else {
                        ""
                    };
                    shown.push(format!("{title}{}{dis}", sc.unwrap_or_default()));
                }
                out.push_str(&format!("{}: {}\n", top.title, shown.join(" · ")));
            }
            Ok(out)
        }

        fn shutdown_sync(&self) {
            if self.shut.swap(true, Ordering::SeqCst) {
                return;
            }
            self.closed.store(true, Ordering::SeqCst);
            if let Some(mut g) = self.guard.lock().take() {
                g.stop();
            }
            match self.mode {
                NativeMode::Launched => {
                    // 앱이 이미 스스로 종료된 뒤 pid 가 다른 프로세스에 재사용되었을 수 있다 → 같은 앱일 때만 kill
                    let now = apps::bundle_id_of(self.pid);
                    let same = match (&self.bundle_id, &now) {
                        (_, None) => !apps::is_running(self.pid) || self.bundle_id.is_none(),
                        (Some(want), Some(got)) => want == got,
                        (None, Some(_)) => true,
                    };
                    if same {
                        apps::kill_process(self.pid);
                        eprintln!("[ghostdesk] 네이티브 앱 인스턴스 종료: pid={}", self.pid);
                    } else {
                        eprintln!("[ghostdesk] pid={} 가 다른 프로세스({now:?})로 바뀌어 종료하지 않음", self.pid);
                    }
                }
                NativeMode::Attached => {
                    let user_unhid = self.state.lock().user_unhid;
                    if !self.prior_hidden && !user_unhid {
                        apps::restore_unhide(self.pid);
                        eprintln!("[ghostdesk] 사용자 앱을 원래대로 다시 표시: pid={}", self.pid);
                    }
                }
            }
        }
    }

    // ───────────────────────── 보조 ─────────────────────────

    fn norm(s: &str) -> String {
        s.trim().trim_end_matches('…').trim_end_matches("...").trim().to_lowercase()
    }

    fn shifted_char(c: char) -> char {
        if c.is_ascii_lowercase() {
            return c.to_ascii_uppercase();
        }
        let pairs = [
            ('1', '!'), ('2', '@'), ('3', '#'), ('4', '$'), ('5', '%'), ('6', '^'), ('7', '&'), ('8', '*'), ('9', '('),
            ('0', ')'), ('-', '_'), ('=', '+'), ('[', '{'), (']', '}'), ('\\', '|'), (';', ':'), ('\'', '"'), (',', '<'),
            ('.', '>'), ('/', '?'), ('`', '~'),
        ];
        pairs.iter().find(|(a, _)| *a == c).map(|(_, b)| *b).unwrap_or(c)
    }

    fn row_label(row: &El) -> String {
        let mut texts = Vec::new();
        let mut stack = vec![(row.clone(), 0)];
        while let Some((e, d)) = stack.pop() {
            if texts.len() >= 3 || d > 4 {
                continue;
            }
            for c in e.children() {
                let r = c.role();
                if r == "AXStaticText" || r == "AXTextField" {
                    if let Some(v) = c.string("AXValue").or_else(|| c.string("AXTitle")) {
                        texts.push(short(&v, 30));
                    }
                } else {
                    stack.push((c, d + 1));
                }
            }
        }
        if texts.is_empty() {
            "(내용 없음)".into()
        } else {
            format!("'{}'", texts.join(" | "))
        }
    }

    fn find_role(root: &El, role: &str, max_depth: usize) -> Option<El> {
        let mut q = std::collections::VecDeque::from([(root.clone(), 0usize)]);
        let mut n = 0;
        while let Some((e, d)) = q.pop_front() {
            n += 1;
            if n > 2000 {
                break;
            }
            for c in e.children() {
                if c.role() == role {
                    return Some(c);
                }
                if d < max_depth {
                    q.push_back((c, d + 1));
                }
            }
        }
        None
    }

    fn popup_items(el: &El) -> Vec<(String, El)> {
        let mut out = Vec::new();
        for m in el.children().into_iter().filter(|c| c.role() == "AXMenu") {
            for it in m.children() {
                if let Some(t) = it.string("AXTitle") {
                    out.push((t, it));
                }
            }
        }
        out
    }

    fn popup_options(el: &El) -> Vec<String> {
        popup_items(el).into_iter().map(|(t, _)| t).take(30).collect()
    }

    fn word_range(value: &str, idx: isize) -> CFRange {
        let u: Vec<u16> = value.encode_utf16().collect();
        let is_word = |c: u16| char::from_u32(c as u32).map(|ch| ch.is_alphanumeric() || ch == '_').unwrap_or(true);
        let i = (idx.max(0) as usize).min(u.len());
        let mut s = i;
        while s > 0 && is_word(u[s - 1]) {
            s -= 1;
        }
        let mut e = i;
        while e < u.len() && is_word(u[e]) {
            e += 1;
        }
        CFRange { location: s as isize, length: (e - s) as isize }
    }

    fn line_range(value: &str, idx: isize) -> CFRange {
        let u: Vec<u16> = value.encode_utf16().collect();
        let i = (idx.max(0) as usize).min(u.len());
        let mut s = i;
        while s > 0 && u[s - 1] != b'\n' as u16 {
            s -= 1;
        }
        let mut e = i;
        while e < u.len() && u[e] != b'\n' as u16 {
            e += 1;
        }
        CFRange { location: s as isize, length: (e - s) as isize }
    }

    // ───────────── 메뉴 안전 ─────────────
    //
    // 숨김 앱의 메뉴 항목도 AXPress 로 실행되므로(이 Mac 에서 확인), 앱 밖 — 사용자 세션 전체 — 에 작용하는
    // 항목은 메뉴 실행·단축키 모두 차단한다. Apple 메뉴(잠금 화면 ctrl+cmd+Q, 로그아웃 shift+cmd+Q,
    // 즉시 로그아웃 alt+shift+cmd+Q, 재시동/시스템 종료)는 앱의 AXMenuBar 첫 항목으로 노출되고 활성 상태다.

    const R_APPLE: &str = "Apple 메뉴(잠금 화면·로그아웃·재시동·시스템 종료·강제 종료 등)는 사용자의 Mac 전체에 영향을 주므로 차단했습니다";
    const R_APP_MENU: &str = "앱 메뉴의 서비스·기타 가리기·모두 보기는 사용자의 다른 앱에 영향을 주므로 차단했습니다";
    const R_HELP: &str = "도움말은 별도 앱(도움말/팁)을 사용자 화면 앞에 띄우므로 차단했습니다";
    const R_SYSTEM_UI: &str =
        "이 항목은 받아쓰기·이모지·글쓰기 도구·자동 완성·창 배치·전체 화면 같은 시스템 UI 를 사용자 화면에 띄우므로 차단했습니다";
    const R_SHARE: &str = "공유는 다른 앱(메일·메시지 등)을 사용자 화면에 열 수 있어 차단했습니다";
    const R_OPEN: &str = "열기 패널은 별도 시스템 프로세스의 창이라 숨길 수 없어 사용자 화면에 나타나므로 차단했습니다 (열 파일은 작업공간 시작 시 지정)";

    const HELP_TITLES: [&str; 16] = [
        "help", "도움말", "ヘルプ", "帮助", "幫助", "hilfe", "aide", "ayuda", "aiuto", "ajuda", "справка", "hjälp", "hulp", "pomoc",
        "yardım", "hjelp",
    ];
    const SHARE_TITLES: [&str; 6] = ["share", "공유", "共享", "共有", "teilen", "partager"];
    /// 시스템 UI 를 띄우거나 창 배치·공간을 바꾸는 표준 항목의 AXIdentifier
    const BLOCKED_MENU_IDS: [&str; 11] = [
        "startDictation:",
        "orderFrontCharacterPalette:",
        "__NSTextViewContextSubmenuIdentifierWritingTools",
        "_NSMenuItemAutoFillIdentifier",
        "NSMenuItemImportFromDeviceIdentifier",
        "_zoomFill:",
        "_zoomCenter:",
        "toggleFullScreen:",
        "_removeWindowFromStageManagerSet:",
        "arrangeInFront:",
        "alternateArrangeInFront:",
    ];

    /// 메뉴 막대의 최상위 항목
    struct MenuTop {
        el: El,
        title: String,
        idx: usize,
        help: bool,
    }

    fn submenu_items(item: &El) -> Vec<El> {
        item.children().into_iter().find(|c| c.role() == "AXMenu").map(|m| m.children()).unwrap_or_default()
    }

    fn menu_tops(bar: &El) -> Vec<MenuTop> {
        let kids = bar.children();
        let n = kids.len();
        kids.into_iter()
            .enumerate()
            .map(|(idx, el)| {
                let title = el.string("AXTitle").unwrap_or_default();
                let help = idx > 1
                    && idx + 1 == n
                    && (HELP_TITLES.contains(&norm(&title).as_str())
                        || submenu_items(&el).iter().any(|it| it.string("AXMenuItemCmdChar").as_deref() == Some("?")));
                MenuTop { el, title, idx, help }
            })
            .collect()
    }

    fn top_block_reason(top: &MenuTop) -> Option<&'static str> {
        if top.idx == 0 || top.title == "Apple" {
            Some(R_APPLE)
        } else if top.help {
            Some(R_HELP)
        } else {
            None
        }
    }

    /// 메뉴 항목 siblings[i] 의 차단 이유. depth 0 = 최상위 메뉴의 직접 항목.
    fn item_block_reason(top: &MenuTop, siblings: &[El], i: usize, depth: usize) -> Option<&'static str> {
        if let Some(r) = top_block_reason(top) {
            return Some(r);
        }
        let it = &siblings[i];
        let id = it.string("AXIdentifier").unwrap_or_default();
        if BLOCKED_MENU_IDS.contains(&id.as_str()) {
            return Some(R_SYSTEM_UI);
        }
        let cmd = |e: &El| {
            (
                e.string("AXMenuItemCmdChar").unwrap_or_default().to_uppercase(),
                e.number("AXMenuItemCmdModifiers").unwrap_or(0.0) as i64,
            )
        };
        let (ch, m) = cmd(it);
        let has_sub = it.children().iter().any(|c| c.role() == "AXMenu");
        if top.idx == 1 && depth == 0 {
            // 앱 메뉴: 서비스(하위 메뉴), 기타 가리기(alt+cmd+H), 그 바로 다음의 모두 보기(단축키 없음)
            if has_sub || (ch == "H" && m == 2) {
                return Some(R_APP_MENU);
            }
            if ch.is_empty() && i > 0 && cmd(&siblings[i - 1]) == ("H".to_string(), 2) {
                return Some(R_APP_MENU);
            }
        }
        if has_sub && SHARE_TITLES.contains(&norm(&it.string("AXTitle").unwrap_or_default()).as_str()) {
            return Some(R_SHARE);
        }
        if ch == "O" && m == 0 {
            return Some(R_OPEN);
        }
        None
    }

    /// 메뉴와 무관하게 사용자 세션 전체에 작용하는 시스템 단축키 — 메뉴 실행도, 대상 앱에 키 이벤트로 보내지도 않는다.
    fn system_combo_block(c: &input::MacCombo) -> Option<&'static str> {
        let has = |b: i64| c.mods & b != 0;
        match c.keycode {
            12 if has(MOD_META) && (has(MOD_CTRL) || has(MOD_SHIFT)) => Some(R_APPLE), // 잠금 화면 / 로그아웃
            53 if has(MOD_META) && has(MOD_ALT) => Some(R_APPLE),                     // 강제 종료
            4 if has(MOD_META) && has(MOD_ALT) => Some(R_APP_MENU),                   // 기타 가리기
            49 if has(MOD_META) && has(MOD_CTRL) => Some(R_SYSTEM_UI),                // 이모지 및 기호
            _ => None,
        }
    }

    /// 메뉴 막대에서 단축키(ch + 수정키)를 가진 항목을 찾는다. (경로, 항목, 차단 이유)
    /// 차단 항목은 비활성이어도 찾아서 돌려준다 (호출자가 키 이벤트까지 막도록).
    fn find_menu_by_shortcut(app: &El, ch: char, mods: i64) -> Option<(String, El, Option<&'static str>)> {
        let bar = app.element("AXMenuBar")?;
        // AXMenuItemCmdModifiers: 0=Cmd 만, 1=Shift, 2=Option, 4=Control, 8=Cmd 없음
        let want_mods = (if mods & MOD_SHIFT != 0 { 1 } else { 0 })
            | (if mods & MOD_ALT != 0 { 2 } else { 0 })
            | (if mods & MOD_CTRL != 0 { 4 } else { 0 })
            | (if mods & MOD_META != 0 { 0 } else { 8 });
        let want_ch = ch.to_ascii_uppercase().to_string();
        #[allow(clippy::too_many_arguments)]
        fn walk(
            top: &MenuTop,
            items: &[El],
            path: &str,
            want_ch: &str,
            want_mods: i64,
            depth: usize,
            inherited: Option<&'static str>,
        ) -> Option<(String, El, Option<&'static str>)> {
            for (i, it) in items.iter().enumerate() {
                let title = it.string("AXTitle").unwrap_or_default();
                let p = if path.is_empty() { title.clone() } else { format!("{path} > {title}") };
                if let Some(c) = it.string("AXMenuItemCmdChar") {
                    let m = it.number("AXMenuItemCmdModifiers").unwrap_or(0.0) as i64;
                    if c.to_uppercase() == want_ch && m == want_mods {
                        let r = inherited.or_else(|| item_block_reason(top, items, i, depth));
                        if r.is_some() || it.bool("AXEnabled") != Some(false) {
                            return Some((p, it.clone(), r));
                        }
                    }
                }
                if depth < 3 {
                    let sub = submenu_items(it);
                    if !sub.is_empty() {
                        let r = inherited.or_else(|| item_block_reason(top, items, i, depth));
                        if let Some(x) = walk(top, &sub, &p, want_ch, want_mods, depth + 1, r) {
                            return Some(x);
                        }
                    }
                }
            }
            None
        }
        for top in menu_tops(&bar) {
            let items = submenu_items(&top.el);
            if let Some(x) = walk(&top, &items, &top.title, &want_ch, want_mods, 0, top_block_reason(&top)) {
                return Some(x);
            }
        }
        None
    }

    /// 라이브 뷰: 현재 창을 ~live_fps 로 캡처해 JPEG 로 싱크에 보낸다. 노출 감시와 창 목록 갱신도 겸한다.
    async fn live_loop(weak: std::sync::Weak<Inner>, fps: f64) {
        let period = Duration::from_secs_f64(1.0 / fps.clamp(0.2, 10.0));
        let mut tick = 0u64;
        loop {
            let Some(inner) = weak.upgrade() else { break };
            if inner.closed.load(Ordering::SeqCst) {
                let _ = tokio::task::spawn_blocking(move || drop(inner)).await;
                break;
            }
            tick += 1;
            // 강한 참조는 작업 스레드 안에서만 쥔다: 작업공간이 shutdown 없이 버려져 여기서 마지막 참조가
            // 풀리더라도 Inner::drop(프로세스 kill, 최대 3초 대기)이 tokio 워커가 아닌 작업 스레드에서 돈다.
            let _ = tokio::task::spawn_blocking(move || {
                let i = inner;
                if !apps::is_running(i.pid) {
                    i.closed.store(true, Ordering::SeqCst);
                    return;
                }
                i.guard_exposure();
                if tick % 4 == 1 {
                    i.refresh_windows(true);
                }
                if i.state.lock().current.is_none() {
                    i.ensure_current();
                }
                if let Ok((img, _g)) = i.capture_current(960) {
                    if let Ok(jpg) = capture::encode_jpeg(&img, 70) {
                        (i.sink)(&jpg);
                    }
                }
            })
            .await;
            tokio::time::sleep(period).await;
        }
    }

    // ───────────────────────── selftest ─────────────────────────

    struct TestLog {
        ok: Vec<String>,
        fail: Vec<String>,
    }

    impl TestLog {
        fn check(&mut self, name: &str, ok: bool, detail: impl AsRef<str>) {
            let line = format!("{name}: {}", detail.as_ref());
            println!("[{}] {line}", if ok { "PASS" } else { "FAIL" });
            if ok {
                self.ok.push(line);
            } else {
                self.fail.push(line);
            }
        }
    }

    fn arg_value(args: &[String], key: &str) -> Option<String> {
        args.iter().position(|a| a == key).and_then(|i| args.get(i + 1).cloned())
    }

    fn write_pidfile(path: &Option<String>, pid: i32) {
        if let Some(p) = path {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{pid}");
            }
        }
    }

    /// ui_tree 출력에서 조건에 맞는 첫 줄의 (id, 중심 좌표)
    fn find_in_tree(tree: &str, pred: impl Fn(&str) -> bool) -> Option<(usize, (f64, f64))> {
        for line in tree.lines() {
            let t = line.trim_start();
            if !pred(t) {
                continue;
            }
            let id: usize = t.split_whitespace().next()?.parse().ok()?;
            let lb = t.rfind('[')?;
            let rb = t[lb..].find(']')? + lb;
            let nums: Vec<f64> = t[lb + 1..rb].split(',').filter_map(|s| s.trim().parse().ok()).collect();
            if nums.len() == 4 {
                return Some((id, (nums[0] + nums[2] / 2.0, nums[1] + nums[3] / 2.0)));
            }
        }
        None
    }

    /// `--selftest-native [--pidfile P] [--out DIR] [--skip-calc]`
    ///
    /// 자체 TextEdit 인스턴스(숨김, 임시 .txt)로 스크린샷·ui_tree·입력·의미 기반 클릭·set_value·뷰 목록·
    /// Mode B 연결을 검사하고, 사용자가 Calculator 를 쓰고 있지 않으면 숨김 Calculator 로 1+2= 을 눌러 본다.
    pub async fn selftest(args: &[String]) -> Result<()> {
        let pidfile = arg_value(args, "--pidfile");
        let out_dir = PathBuf::from(arg_value(args, "--out").unwrap_or_else(|| std::env::temp_dir().to_string_lossy().to_string()));
        let skip_calc = args.iter().any(|a| a == "--skip-calc");
        // 실행 노출 측정 전용: --launch-only [--no-file]
        let launch_only = args.iter().any(|a| a == "--launch-only");
        // 기본은 빈 문서로 시작 (문서를 열면 TextEdit 이 스스로 unhide 해서 수 ms 노출됨 — 이 Mac 에서 측정)
        let no_file = !args.iter().any(|a| a == "--with-file");
        let tcc = crate::macos::preflight()?;
        println!("TCC: {tcc:?}");
        let tmp = std::env::temp_dir().join(format!("ghostdesk-native-selftest-{}", std::process::id()));
        std::fs::create_dir_all(&tmp)?;
        std::fs::create_dir_all(&out_dir)?;
        let txt = tmp.join("ghostdesk-selftest.txt");
        std::fs::write(&txt, "GhostDesk native selftest\n")?;
        let mut log = TestLog { ok: vec![], fail: vec![] };
        let frames = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fr = frames.clone();
        let sink: FrameSink = Arc::new(move |_b: &[u8]| {
            fr.fetch_add(1, Ordering::SeqCst);
        });

        let user_textedit = apps::running_pids("com.apple.TextEdit");
        println!("사용자 TextEdit pid: {user_textedit:?} (건드리지 않음)");
        let t0 = Instant::now();
        let ws = NativeAppWorkspace::launch(
            NativeOptions {
                app: "com.apple.TextEdit".into(),
                open_files: if no_file { vec![] } else { vec![txt.clone()] },
                new_instance: true,
                live_fps: 2.0,
                ..Default::default()
            },
            sink.clone(),
        )
        .await
        .context("TextEdit 숨김 실행")?;
        let pid = ws.pid();
        write_pidfile(&pidfile, pid);
        log.check("launch", !user_textedit.contains(&pid), format!("pid={pid} mode={:?} {}ms", ws.mode(), t0.elapsed().as_millis()));
        log.check("launch-exposure", ws.exposure_report().is_empty(), format!("{:?}", ws.exposure_report()));
        if launch_only {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let (views, _) = ws.views();
            println!("views: {:?}", views.iter().map(|v| v.title.clone()).collect::<Vec<_>>());
            println!("exposure after 2s: {:?}", ws.exposure_report());
            ws.shutdown().await;
            let _ = std::fs::remove_dir_all(&tmp);
            return Ok(());
        }

        let result = async {
            // 스크린샷
            let t = Instant::now();
            let png = ws.screenshot().await?;
            let shot = out_dir.join("native-textedit.png");
            std::fs::write(&shot, &png)?;
            let (w, h) = ws.size();
            log.check("screenshot", w > 100 && h > 100 && w.max(h) <= 1280, format!("{w}x{h} {}B {}ms → {}", png.len(), t.elapsed().as_millis(), shot.display()));
            let (views, active) = ws.views();
            log.check("views", !views.is_empty(), format!("{:?} active={active:?}", views.iter().map(|v| format!("{}:{}", v.id, v.title)).collect::<Vec<_>>()));

            // ui_tree
            let tree = ws.app_control(&serde_json::json!({"action": "ui_tree"})).await?;
            println!("{tree}");
            let ta = find_in_tree(&tree, |l| l.split_whitespace().nth(1) == Some("AXTextArea"));
            log.check("ui_tree", ta.is_some(), format!("AXTextArea={ta:?}"));
            let (ta_id, ta_c) = ta.ok_or_else(|| anyhow!("텍스트 영역 없음"))?;
            if no_file {
                let r = ws.app_control(&serde_json::json!({"action": "set_value", "id": ta_id, "text": "GhostDesk native selftest\n"})).await;
                log.check("set_value(initial)", r.as_ref().map(|s| s.contains("확인됨")).unwrap_or(false), format!("{r:?}"));
            }

            // 의미 기반 클릭: 체크박스(볼드) → AXPress, 값 변화 확인
            if let Some((_, c)) = find_in_tree(&tree, |l| l.contains("AXCheckBox") && (l.contains("볼드") || l.to_lowercase().contains("bold"))) {
                ws.click(Some(c), MouseButton::Left, 1, 0).await?;
                let n = ws.take_notes();
                log.check("semantic-click-checkbox", n.iter().any(|x| x.contains("AXCheckBox") && x.contains("0 → 1")), format!("{n:?}"));
                ws.click(Some(c), MouseButton::Left, 1, 0).await?;
                let _ = ws.take_notes();
            }
            // 팝업 버튼: 클릭은 거부(메뉴가 화면에 뜨므로), choose 로 메뉴를 열지 않고 선택
            if let Some((pid_, c)) = find_in_tree(&tree, |l| l.split_whitespace().nth(1) == Some("AXPopUpButton") && l.contains("줄 간격")) {
                let r = ws.click(Some(c), MouseButton::Left, 1, 0).await;
                log.check("popup-click-refused", r.is_err(), format!("{:?}", r.err().map(|e| short(&e.to_string(), 160))));
                let list = ws.app_control(&serde_json::json!({"action": "choose", "id": pid_, "option": "__없음__"})).await;
                let opts: Vec<String> = list
                    .as_ref()
                    .err()
                    .and_then(|e| {
                        let m = e.to_string();
                        let a = m.find('[')?;
                        let b = m.rfind(']')?;
                        Some(m[a + 1..b].split(", ").map(String::from).collect())
                    })
                    .unwrap_or_default();
                println!("popup options: {opts:?}");
                if let Some(o) = opts.iter().find(|o| o.as_str() != "1.0" && o.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)) {
                    let r = ws.app_control(&serde_json::json!({"action": "choose", "id": pid_, "option": o})).await;
                    log.check("popup-choose(no menu shown)", r.as_ref().map(|s| s.contains(&format!("→ '{o}'"))).unwrap_or(false), format!("{r:?}"));
                } else {
                    // 닫힌 NSPopUpButton 은 메뉴 항목을 노출하지 않는다 — 알려진 한계 (정보성)
                    log.check("popup-choose(info)", true, format!("선택지를 읽지 못함(알려진 한계): {list:?}"));
                }
            }

            // 의미 기반 클릭 (텍스트 영역 → 포커스 + 커서)
            ws.click(Some(ta_c), MouseButton::Left, 1, 0).await?;
            let notes = ws.take_notes();
            log.check("semantic-click-text", notes.iter().any(|n| n.contains("AXTextArea") && n.contains("포커스")), format!("{notes:?}"));

            // 커서를 끝으로 (End 대신 cmd+a 흉내 후 오른쪽 화살표는 앱에 따라 다르므로 set_value 로 상태를 분명히)
            let r = ws.app_control(&serde_json::json!({"action": "read", "id": ta_id})).await?;
            println!("read before typing: {}", short(&r, 120));
            ws.key("End", 1).await.ok();
            // 글자 입력 (한글 포함)
            ws.type_text("hello 안녕").await?;
            tokio::time::sleep(Duration::from_millis(200)).await;
            let v = ws.app_control(&serde_json::json!({"action": "read", "id": ta_id})).await?;
            let notes = ws.take_notes();
            log.check("type_text", v.contains("hello 안녕"), format!("value={:?} notes={notes:?}", short(v.split("value:\n").nth(1).unwrap_or(""), 80)));

            // Return
            ws.key("Return", 1).await?;
            ws.type_text("둘째 줄").await?;
            tokio::time::sleep(Duration::from_millis(200)).await;
            let v = ws.app_control(&serde_json::json!({"action": "read", "id": ta_id})).await?;
            log.check("key-Return", v.contains("안녕\n둘째 줄"), format!("{:?}", short(v.split("value:\n").nth(1).unwrap_or(""), 80)));

            // BackSpace x2 → "둘째 줄" 에서 " 줄" 이 지워진다
            ws.key("BackSpace", 2).await?;
            tokio::time::sleep(Duration::from_millis(150)).await;
            let v = ws.app_control(&serde_json::json!({"action": "read", "id": ta_id})).await?;
            log.check("key-BackSpace", v.contains("안녕\n둘째GhostDesk"), format!("{:?}", short(v.split("value:\n").nth(1).unwrap_or(""), 80)));
            // Delete = 앞으로 지우기 (xdotool/keys.rs 와 같은 의미): 커서를 맨 앞에 두고 Delete → 첫 글자가 지워진다
            {
                let i = ws.inner.clone();
                blocking(move || {
                    if let Some(f) = i.focused_text() {
                        f.set_range("AXSelectedTextRange", CFRange { location: 0, length: 0 });
                    }
                })
                .await?;
                ws.key("Delete", 1).await?;
                tokio::time::sleep(Duration::from_millis(150)).await;
                let v2 = ws.app_control(&serde_json::json!({"action": "read", "id": ta_id})).await?;
                let body = v2.split("value:\n").nth(1).unwrap_or("");
                log.check("key-Delete(forward)", body.starts_with("ello 안녕"), format!("{:?}", short(body, 40)));
            }
            let menus = ws.app_control(&serde_json::json!({"action": "menus"})).await.unwrap_or_else(|e| e.to_string());
            println!("{menus}");
            // 메뉴 안전: Apple/도움말/서비스 등은 차단 (실제로 누르지 않는 조회·존재하지 않는 항목으로만 검사)
            log.check("menus-hide-apple+mark-blocked", !menus.contains("잠금") && !menus.contains("로그아웃") && menus.contains("[차단]"), short(&menus, 200));
            {
                let i = ws.inner.clone();
                let (lock, open, new) = blocking(move || {
                    let app = i.app();
                    (
                        find_menu_by_shortcut(&app, 'q', MOD_CTRL | MOD_META).map(|(p, _, r)| (p, r.is_some())),
                        find_menu_by_shortcut(&app, 'o', MOD_META).map(|(p, _, r)| (p, r.is_some())),
                        find_menu_by_shortcut(&app, 'n', MOD_META).map(|(p, _, r)| (p, r.is_some())),
                    )
                })
                .await?;
                log.check(
                    "menu-shortcut-block",
                    lock.as_ref().map(|x| x.1).unwrap_or(true) && open.as_ref().map(|x| x.1).unwrap_or(false) && new.as_ref().map(|x| !x.1).unwrap_or(false),
                    format!("ctrl+cmd+Q={lock:?} cmd+O={open:?} cmd+N={new:?}"),
                );
            }
            let r1 = ws.key("ctrl+cmd+q", 1).await;
            let r2 = ws.key("alt+shift+cmd+q", 1).await;
            let r3 = ws.app_control(&serde_json::json!({"action": "menu", "path": "Apple > __없음__"})).await;
            let r4 = ws.app_control(&serde_json::json!({"action": "menu", "path": "도움말 > __없음__"})).await;
            log.check(
                "dangerous-combos/menus-refused",
                r1.is_err() && r2.is_err() && r3.as_ref().err().map(|e| e.to_string().contains("Apple 메뉴")).unwrap_or(false)
                    && r4.as_ref().err().map(|e| e.to_string().contains("도움말")).unwrap_or(false),
                format!("{:?} / {:?} / {:?} / {:?}", r1.err().map(|e| short(&e.to_string(), 50)), r2.err().map(|e| short(&e.to_string(), 30)), r3.err().map(|e| short(&e.to_string(), 30)), r4.err().map(|e| short(&e.to_string(), 30))),
            );

            // 단축키: 전체 선택(에뮬레이션)
            ws.key("cmd+a", 1).await?;
            let notes = ws.take_notes();
            log.check("cmd+a(emulated)", notes.iter().any(|n| n.contains("전체 선택") && !n.contains("확인 안 됨")), format!("{notes:?}"));
            ws.key("cmd+c", 1).await?;
            println!("cmd+c: {:?}", ws.take_notes());

            // 메뉴로 전체 선택: 먼저 커서를 0 으로 (클릭 좌표를 텍스트 시작 부근으로)
            ws.click(Some(((ta_c.0 * 0.05).max(5.0), ta_c.1)), MouseButton::Left, 1, 0).await.ok();
            let _ = ws.take_notes();
            if let Some((path, _)) = {
                let i = ws.inner.clone();
                blocking(move || find_menu_by_shortcut(&i.app(), 'a', MOD_META).map(|(p, e, _)| (p, e))).await?
            } {
                let r = ws.app_control(&serde_json::json!({"action": "menu", "path": path})).await?;
                tokio::time::sleep(Duration::from_millis(250)).await;
                let i = ws.inner.clone();
                let sel = blocking(move || i.focused_text().and_then(|f| f.range("AXSelectedTextRange").map(|r| (r.location, r.length)))).await?;
                let total = utf16_len(v.split("value:\n").nth(1).unwrap_or(""));
                println!("menu '{path}': {r} → selection={sel:?} (전체 {total})");
                log.check("menu-select-all(info)", true, format!("selection after AX menu press = {sel:?}, full={total} (효과 {})", if sel.map(|s| s.1 == total).unwrap_or(false) { "있음" } else { "없음 — 숨김 모드 한계" }));
            }
            // SkyLight 레코드 + 실제 cmd 키 이벤트 (에뮬레이션 없이)
            ws.click(Some(((ta_c.0 * 0.05).max(5.0), ta_c.1)), MouseButton::Left, 1, 0).await.ok();
            let _ = ws.take_notes();
            let r = ws.raw_key("cmd+a").await?;
            tokio::time::sleep(Duration::from_millis(250)).await;
            let i = ws.inner.clone();
            let sel = blocking(move || i.focused_text().and_then(|f| f.range("AXSelectedTextRange").map(|r| (r.location, r.length)))).await?;
            log.check("raw-cmd+a(info)", true, format!("{r} → selection={sel:?} (길이>0 이면 효과 있음)"));

            // set_value
            let r = ws.app_control(&serde_json::json!({"action": "set_value", "id": ta_id, "text": "set_value 로 바꾼 내용\n두 번째"})).await;
            log.check("set_value", r.as_ref().map(|s| s.contains("확인됨")).unwrap_or(false), format!("{r:?}"));

            // 의미 기반 클릭 후 cmd+v (내부 클립보드)
            ws.click(Some(ta_c), MouseButton::Left, 1, 0).await?;
            let _ = ws.take_notes();
            ws.key("cmd+v", 1).await?;
            let notes = ws.take_notes();
            let v = ws.app_control(&serde_json::json!({"action": "read", "id": ta_id})).await?;
            log.check("cmd+v(internal clipboard)", notes.iter().any(|n| n.contains("붙여넣")), format!("{notes:?} value={:?}", short(v.split("value:\n").nth(1).unwrap_or(""), 80)));

            // 앱 수준 메뉴 단축키: cmd+n → 같은 단축키의 메뉴 항목(파일 > 신규)을 AXPress
            let (v0, orig) = ws.views();
            let before_n = v0.len();
            ws.key("cmd+n", 1).await?;
            let n1 = ws.take_notes();
            tokio::time::sleep(Duration::from_millis(1200)).await;
            let _ = ws.app_control(&serde_json::json!({"action": "list_windows"})).await?;
            let after_n = ws.views().0.len();
            let n2 = ws.take_notes();
            log.check("cmd+n(menu press, info)", true, format!("창 {before_n} → {after_n}; {n1:?} {n2:?}"));
            if after_n > before_n {
                // 새 창으로 자동 전환된 뒤 스크린샷 없이 예전 좌표로 클릭하면 거부해야 한다
                let r = ws.click(Some(ta_c), MouseButton::Left, 1, 0).await;
                log.check("stale-coords-refused", r.as_ref().err().map(|e| e.to_string().contains("스크린샷")).unwrap_or(false), format!("{:?}", r.err().map(|e| short(&e.to_string(), 60))));
                // 새 창으로 자동 전환되었을 것 → 원래 창으로 돌아간다
                let (views, _) = ws.views();
                if let Some(i) = views.iter().position(|v| Some(&v.id) == orig.as_ref()) {
                    ws.switch_view(i).await.ok();
                }
                let _ = ws.app_control(&serde_json::json!({"action": "ui_tree"})).await?;
            }

            // 스크롤 (짧은 문서라 막대가 없을 수 있음)
            let r = ws.scroll(Some(ta_c), 0.0, 300.0, 0).await;
            log.check("scroll(info)", true, format!("{r:?} {:?}", ws.take_notes()));

            // 드래그/오른쪽 클릭은 명확한 오류
            let d = ws.drag((10.0, 10.0), (50.0, 50.0), 0).await;
            let rc = ws.click(Some(ta_c), MouseButton::Right, 1, 0).await;
            log.check("drag/right-click refused", d.is_err() && rc.is_err(), format!("{:?} / {:?}", d.err().map(|e| short(&e.to_string(), 40)), rc.err().map(|e| short(&e.to_string(), 40))));

            // 줌
            let z = ws.zoom([0.0, 0.0, 200.0, 120.0]).await?;
            let zp = out_dir.join("native-textedit-zoom.png");
            std::fs::write(&zp, &z)?;
            log.check("zoom", z.len() > 1000, format!("{}B → {}", z.len(), zp.display()));

            // 최종 스크린샷 저장 (닫기 전에)
            let png = ws.screenshot().await?;
            let shot2 = out_dir.join("native-textedit-after.png");
            std::fs::write(&shot2, &png)?;
            println!("after screenshot → {}", shot2.display());

            // 라이브 뷰
            let f0 = frames.load(Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(2600)).await;
            let f1 = frames.load(Ordering::SeqCst);
            log.check("live-view", f1 - f0 >= 3, format!("2.6초 동안 {}프레임", f1 - f0));

            // Mode B: 우리가 띄운 인스턴스에 동의 하에 붙기 (원래 숨김 → 종료 후에도 숨김 유지)
            let b = NativeAppWorkspace::launch(
                NativeOptions {
                    app: "com.apple.TextEdit".into(),
                    attach_consent: true,
                    attach_pid: Some(pid),
                    live_fps: 0.0,
                    ..Default::default()
                },
                sink.clone(),
            )
            .await?;
            let png = b.screenshot().await?;
            log.check("modeB-attach", b.mode() == NativeMode::Attached && png.len() > 1000, format!("mode={:?} {}B", b.mode(), png.len()));
            let no_consent = NativeAppWorkspace::launch(
                NativeOptions { app: "com.apple.TextEdit".into(), attach_pid: Some(pid), live_fps: 0.0, ..Default::default() },
                sink.clone(),
            )
            .await;
            log.check("modeB-needs-consent", no_consent.is_err(), format!("{:?}", no_consent.err().map(|e| short(&e.to_string(), 60))));
            let wrong = NativeAppWorkspace::launch(
                NativeOptions { app: "com.apple.calculator".into(), attach_pid: Some(pid), attach_consent: true, live_fps: 0.0, ..Default::default() },
                sink.clone(),
            )
            .await;
            log.check("modeB-wrong-app-pid-refused", wrong.as_ref().err().map(|e| e.to_string().contains("아니라")).unwrap_or(false), format!("{:?}", wrong.err().map(|e| short(&e.to_string(), 80))));
            b.shutdown().await;
            drop(b);
            log.check("modeB-restore", apps::is_hidden(pid) == Some(true) && apps::is_running(pid), "원래 숨김 상태 유지, 프로세스 유지");
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = &result {
            log.check("textedit-flow", false, format!("{e:#}"));
        }
        // 정리: 문서 창을 닫고 "삭제"를 눌러 자동 저장본을 남기지 않는다 (TextEdit 컨테이너는 사용자와 공유됨)
        let mut sheet_shot = false;
        for _round in 0..6 {
            let (views, _) = ws.views();
            if views.is_empty() {
                break;
            }
            ws.switch_view(0).await.ok();
            let Ok(tree) = ws.app_control(&serde_json::json!({"action": "ui_tree"})).await else { break };
            let Some((close_id, _)) = find_in_tree(&tree, |l| l.contains("AXButton/AXCloseButton")) else {
                println!("닫기 버튼 없음:\n{tree}");
                break;
            };
            let r = ws.app_control(&serde_json::json!({"action": "press", "id": close_id})).await;
            println!("close window '{}': {r:?}", views[0].title);
            tokio::time::sleep(Duration::from_millis(700)).await;
            let Ok(tree) = ws.app_control(&serde_json::json!({"action": "ui_tree"})).await else { continue };
            if let Some((del_id, _)) = find_in_tree(&tree, |l| {
                l.split_whitespace().nth(1) == Some("AXButton") && ["\"삭제\"", "\"Delete\"", "\"저장 안 함\"", "\"Don't Save\""].iter().any(|t| l.contains(t))
            }) {
                if !sheet_shot {
                    if let Ok(png) = ws.screenshot().await {
                        let p = out_dir.join("native-textedit-sheet.png");
                        let _ = std::fs::write(&p, &png);
                        println!("sheet screenshot → {}", p.display());
                        sheet_shot = true;
                    }
                }
                let r = ws.app_control(&serde_json::json!({"action": "press", "id": del_id})).await;
                println!("  discard: {r:?}");
                tokio::time::sleep(Duration::from_millis(700)).await;
            }
            let lw = ws.app_control(&serde_json::json!({"action": "list_windows"})).await;
            println!("  windows now: {lw:?}");
        }
        let (rem, _) = ws.views();
        for i in 0..rem.len() {
            ws.switch_view(i).await.ok();
            match ws.screenshot().await {
                Ok(png) => {
                    let p = out_dir.join(format!("native-textedit-remaining-{i}.png"));
                    let _ = std::fs::write(&p, &png);
                    println!("remaining window [{i}] '{}' → {}", rem[i].title, p.display());
                }
                Err(e) => println!("remaining window [{i}] '{}' 캡처 실패: {e:#}", rem[i].title),
            }
            match ws.app_control(&serde_json::json!({"action": "ui_tree", "max_depth": 8})).await {
                Ok(t) => println!("{t}"),
                Err(e) => println!("ui_tree 실패: {e:#}"),
            }
        }
        let left = ws.views().0.len();
        log.check("cleanup-close-discard", left == 0, format!("남은 창 {left}개 (시트 합성 스크린샷: {sheet_shot})"));
        let exp = ws.exposure_report();
        log.check("no-exposure(TextEdit)", exp.is_empty(), format!("{exp:?}"));
        ws.shutdown().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        log.check("shutdown-TextEdit", !apps::is_running(pid), format!("pid {pid} 종료"));
        drop(ws);

        // ── 취소된 launch 가 프로세스를 남기지 않는지 (에이전트 중지 등으로 launch future 가 버려진 경우) ──
        if !args.iter().any(|a| a == "--skip-cancel") {
            let before: HashSet<i32> = apps::running_pids("com.apple.TextEdit").into_iter().collect();
            let r = tokio::time::timeout(
                Duration::from_millis(400),
                NativeAppWorkspace::launch(
                    NativeOptions { app: "com.apple.TextEdit".into(), new_instance: true, live_fps: 0.0, ..Default::default() },
                    sink.clone(),
                ),
            )
            .await;
            let timed_out = r.is_err();
            if let Ok(Ok(w)) = r {
                w.shutdown().await;
            }
            // 실행(최대 30초) + 노출 측정(4초) + 창 대기 뒤 버려진 Inner 가 정리될 때까지 기다린다
            let t = Instant::now();
            let mut seen_new = HashSet::new();
            let mut left: Vec<i32> = vec![];
            while t.elapsed() < Duration::from_secs(25) {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let now: Vec<i32> = apps::running_pids("com.apple.TextEdit").into_iter().filter(|p| !before.contains(p)).collect();
                for p in &now {
                    if seen_new.insert(*p) {
                        write_pidfile(&pidfile, *p);
                    }
                }
                left = now;
                if !seen_new.is_empty() && left.is_empty() {
                    break;
                }
            }
            log.check(
                "cancelled-launch-cleaned",
                timed_out && !seen_new.is_empty() && left.is_empty(),
                format!("timeout={timed_out} 새 pid {seen_new:?} → 남은 pid {left:?} ({}ms)", t.elapsed().as_millis()),
            );
            for p in left {
                apps::kill_process(p);
            }
        }

        // ── Calculator ──
        let calc_running = apps::running_pids("com.apple.calculator");
        if skip_calc {
            println!("Calculator 검사 건너뜀 (--skip-calc)");
        } else if !calc_running.is_empty() {
            println!("사용자가 Calculator 를 실행 중이므로 검사하지 않습니다 (pid {calc_running:?})");
        } else {
            let r = calc_test(&mut log, &out_dir, &pidfile, sink.clone()).await;
            if let Err(e) = r {
                log.check("calculator", false, format!("{e:#}"));
            }
        }
        // 세션 헬퍼용 디스플레이 캡처: 크기만 확인하고 내용은 저장하지 않는다 (사용자 화면이므로)
        if args.iter().any(|a| a == "--display-check") {
            let r = tokio::task::spawn_blocking(|| crate::macos::capture::capture_display_png(0, 640)).await?;
            match r {
                Ok((png, w, h)) => {
                    log.check("capture_display_png(dims only)", w > 0 && h > 0 && w.max(h) <= 640, format!("{w}x{h} {}B (저장하지 않음)", png.len()))
                }
                Err(e) => log.check("capture_display_png(dims only)", false, format!("{e:#}")),
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
        println!("\n==== native selftest: PASS {} / FAIL {} ====", log.ok.len(), log.fail.len());
        for f in &log.fail {
            println!("  FAIL {f}");
        }
        if log.fail.is_empty() {
            Ok(())
        } else {
            bail!("{}개 실패", log.fail.len())
        }
    }

    async fn calc_test(log: &mut TestLog, out_dir: &std::path::Path, pidfile: &Option<String>, sink: FrameSink) -> Result<()> {
        let ws = NativeAppWorkspace::launch(
            NativeOptions { app: "com.apple.calculator".into(), live_fps: 0.0, ..Default::default() },
            sink,
        )
        .await?;
        let pid = ws.pid();
        write_pidfile(pidfile, pid);
        let r = async {
            let png = ws.screenshot().await?;
            let shot = out_dir.join("native-calculator.png");
            std::fs::write(&shot, &png)?;
            let (w, h) = ws.size();
            log.check("calc-screenshot", png.len() > 1000, format!("{w}x{h} → {}", shot.display()));
            let tree = ws.app_control(&serde_json::json!({"action": "ui_tree"})).await?;
            println!("{tree}");
            let btn = |names: &[&str]| {
                let names: Vec<String> = names.iter().map(|s| s.to_lowercase()).collect();
                find_in_tree(&tree, move |l| {
                    if l.split_whitespace().nth(1) != Some("AXButton") {
                        return false;
                    }
                    let low = l.to_lowercase();
                    names.iter().any(|n| low.contains(&format!("desc=\"{n}\"")) || low.contains(&format!(" \"{n}\"")))
                })
            };
            // 먼저 초기화 (AC/C)
            if let Some((_, c)) = btn(&["all clear", "clear", "지우기", "모두 지우기", "ac", "c"]) {
                ws.click(Some(c), MouseButton::Left, 1, 0).await.ok();
            }
            let seq: [(&str, &[&str]); 4] = [
                ("1", &["1"]),
                ("+", &["add", "더하기", "+", "plus"]),
                ("2", &["2"]),
                ("=", &["equals", "같음", "등호", "=", "결과"]),
            ];
            for (label, names) in seq {
                let (_, c) = btn(names).ok_or_else(|| anyhow!("'{label}' 버튼을 ui_tree 에서 찾지 못했습니다"))?;
                ws.click(Some(c), MouseButton::Left, 1, 0).await?;
                println!("  {label} @ ({:.0},{:.0}): {:?}", c.0, c.1, ws.take_notes());
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            let tree2 = ws.app_control(&serde_json::json!({"action": "ui_tree"})).await?;
            let display: Vec<&str> = tree2
                .lines()
                .filter(|l| {
                    let t = l.trim_start();
                    let r = t.split_whitespace().nth(1).unwrap_or("");
                    (r == "AXStaticText" || r == "AXScrollArea" || r == "AXGroup") && t.contains("value=")
                })
                .collect();
            println!("display candidates: {display:?}");
            let ok = display.iter().any(|l| l.contains("value=\"3\"") || l.contains("value=\"‎3\""));
            log.check("calc-1+2=3", ok, format!("{display:?}"));
            let png = ws.screenshot().await?;
            std::fs::write(out_dir.join("native-calculator-after.png"), &png)?;
            anyhow::Ok(())
        }
        .await;
        let exp = ws.exposure_report();
        log.check("no-exposure(Calculator)", exp.is_empty(), format!("{exp:?}"));
        ws.shutdown().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        log.check("shutdown-Calculator", !apps::is_running(pid), format!("pid {pid}"));
        r
    }
}

// ═════════════════════════════════════════ 다른 OS ═════════════════════════════════════════

#[cfg(not(any(target_os = "macos", windows)))]
mod stub {
    use std::sync::Arc;

    use anyhow::{bail, Result};
    use serde_json::Value;

    use super::{NativeMode, NativeOptions};
    use crate::browser::{FrameSink, MouseButton};
    use crate::desk::ViewInfo;

    const MSG: &str = "네이티브 앱 백엔드는 macOS·Windows 에서만 지원됩니다";

    pub struct NativeAppWorkspace;

    impl NativeAppWorkspace {
        pub async fn launch(_opts: NativeOptions, _sink: FrameSink) -> Result<Arc<Self>> {
            bail!(MSG)
        }
        pub fn pid(&self) -> i32 {
            -1
        }
        pub fn mode(&self) -> NativeMode {
            NativeMode::Launched
        }
        pub fn app_name(&self) -> String {
            String::new()
        }
        pub fn exposure_report(&self) -> Vec<String> {
            vec![]
        }
        pub fn size(&self) -> (u32, u32) {
            (0, 0)
        }
        pub async fn screenshot(&self) -> Result<Vec<u8>> {
            bail!(MSG)
        }
        pub async fn zoom(&self, _region: [f64; 4]) -> Result<Vec<u8>> {
            bail!(MSG)
        }
        pub async fn mouse_move(&self, _x: f64, _y: f64, _mods: i64) -> Result<()> {
            bail!(MSG)
        }
        pub async fn mouse_down(&self, _b: MouseButton, _count: i64, _mods: i64) -> Result<()> {
            bail!(MSG)
        }
        pub async fn mouse_up(&self, _b: MouseButton, _count: i64, _mods: i64) -> Result<()> {
            bail!(MSG)
        }
        pub async fn click(&self, _pos: Option<(f64, f64)>, _b: MouseButton, _count: u32, _mods: i64) -> Result<()> {
            bail!(MSG)
        }
        pub async fn drag(&self, _from: (f64, f64), _to: (f64, f64), _mods: i64) -> Result<()> {
            bail!(MSG)
        }
        pub async fn scroll(&self, _pos: Option<(f64, f64)>, _dx: f64, _dy: f64, _mods: i64) -> Result<()> {
            bail!(MSG)
        }
        pub async fn key(&self, _combo: &str, _repeat: u32) -> Result<()> {
            bail!(MSG)
        }
        pub async fn hold_key(&self, _combo: &str, _secs: f64) -> Result<()> {
            bail!(MSG)
        }
        pub async fn type_text(&self, _text: &str) -> Result<()> {
            bail!(MSG)
        }
        pub fn mouse_pos(&self) -> (f64, f64) {
            (0.0, 0.0)
        }
        pub fn views(&self) -> (Vec<ViewInfo>, Option<String>) {
            (vec![], None)
        }
        pub async fn switch_view(&self, _index: usize) -> Result<()> {
            bail!(MSG)
        }
        pub fn take_notes(&self) -> Vec<String> {
            vec![]
        }
        pub fn is_closed(&self) -> bool {
            true
        }
        pub async fn shutdown(&self) {}
        pub async fn app_control(&self, _input: &Value) -> Result<String> {
            bail!(MSG)
        }
        pub async fn raw_key(&self, _combo: &str) -> Result<String> {
            bail!(MSG)
        }
    }

    pub async fn selftest(_args: &[String]) -> Result<()> {
        bail!(MSG)
    }
}
