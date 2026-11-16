//! 이 운영체제에서 쓸 수 있는 "보이지 않는 화면" 종류.
//!
//! UI(새 에이전트 창), 제어 서버(외부 에이전트의 desk_open — 사용자 승인을 묻기 **전에** 확인),
//! MCP 도구 설명, 에이전트 시스템 프롬프트가 모두 이 값을 본다.

/// 🌐 헤드리스 브라우저 (CDP) — Chromium 계열이 설치된 모든 OS
pub const BROWSER: bool = true;

/// 🖥 원격 컴퓨터 (VNC/RFB) — 순수 TCP
pub const REMOTE: bool = true;

/// ⭐ Electron 앱 숨김 복제 인스턴스 (CDP + Node 인스펙터)
pub const ELECTRON: bool = cfg!(any(target_os = "macos", windows));

/// 그 밖의 로컬 앱 (macOS: ScreenCaptureKit + 접근성, Windows: 화면 밖 창 + PrintWindow + 창 메시지)
pub const NATIVE_APPS: bool = cfg!(any(target_os = "macos", windows));

/// 🧪 가상 디스플레이 (CGVirtualDisplay) — macOS 전용
pub const VDISPLAY: bool = cfg!(target_os = "macos");

/// 🧑‍💻 별도 세션 — macOS: 전용 계정(세션 헬퍼 + LaunchAgent), Windows: 같은 계정의 자식 세션(PiP)
pub const SESSION: bool = cfg!(any(target_os = "macos", windows));
