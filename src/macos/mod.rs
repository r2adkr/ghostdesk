//! macOS 플랫폼 기본 기능.
//!
//! - [`ax`]: Accessibility API 래퍼 (요소 트리, 속성 읽기/쓰기, 액션)
//! - [`capture`]: ScreenCaptureKit 단일 창 / 디스플레이 캡처
//! - [`input`]: CGEvent 입력 — `pid_*`(대상 프로세스 전용)와 `global::*`(전용 에이전트 세션 전용)
//! - [`skylight`]: SkyLight 비공개 API (front 프로세스 조회, 대상 PSN 전용 키 창 레코드)
//! - [`apps`]: 앱 실행/숨김/종료, CGWindowList 창 목록
//! - [`guard`]: 숨김 실행한 앱이 화면에 나타나면 즉시 다시 숨기는 노출 감시기
//!
//! 사용자 세션에서 지켜야 할 원칙: 사용자의 커서·front 앱·창을 절대 바꾸지 않는다.
//! 전역 이벤트 게시, 커서 워프, 앱 활성화/unhide/AXRaise 는 이 모듈의 사용자-세션 경로에서 쓰지 않는다.
#![allow(dead_code)]

pub mod apps;
pub mod ax;
pub mod capture;
pub mod guard;
pub mod input;
pub mod skylight;

use anyhow::{bail, Result};

/// TCC 권한 상태
#[derive(Clone, Copy, Debug)]
pub struct TccStatus {
    /// 손쉬운 사용 (AX, CGEventPostToPid)
    pub accessibility: bool,
    /// 화면 및 시스템 오디오 기록 (ScreenCaptureKit)
    pub screen_capture: bool,
    /// 이벤트 게시 권한 (CGPreflightPostEventAccess)
    pub post_event: bool,
}

/// 권한을 묻는 창을 띄우지 않고 현재 상태만 확인한다.
pub fn tcc_status() -> TccStatus {
    TccStatus {
        accessibility: ax::is_trusted(),
        screen_capture: objc2_core_graphics::CGPreflightScreenCaptureAccess(),
        post_event: objc2_core_graphics::CGPreflightPostEventAccess(),
    }
}

/// 네이티브 앱 백엔드에 필요한 권한이 없으면 사용자가 할 일을 알려주는 오류를 돌려준다.
/// (CGRequestScreenCaptureAccess 는 반복 호출하면 사용자를 귀찮게 하므로 부르지 않는다.)
pub fn preflight() -> Result<TccStatus> {
    let s = tcc_status();
    let mut missing = Vec::new();
    if !s.accessibility {
        missing.push(
            "• 손쉬운 사용: 시스템 설정 > 개인정보 보호 및 보안 > 손쉬운 사용 에서 GhostDesk(또는 GhostDesk 를 실행한 터미널 앱)를 켜세요.",
        );
    }
    if !s.screen_capture {
        missing.push(
            "• 화면 기록: 시스템 설정 > 개인정보 보호 및 보안 > 화면 및 시스템 오디오 기록 에서 GhostDesk(또는 실행한 터미널 앱)를 켜세요.",
        );
    }
    if !missing.is_empty() {
        bail!(
            "네이티브 앱 제어에 필요한 macOS 권한이 없습니다:\n{}\n권한을 켠 뒤 GhostDesk 를 다시 실행해야 적용됩니다.",
            missing.join("\n")
        );
    }
    Ok(s)
}
