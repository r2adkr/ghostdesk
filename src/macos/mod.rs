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
/// (권한을 묻는 창은 사용자가 '허용' 을 누를 때만 띄운다: `request_accessibility` · `request_screen_capture`)
pub fn preflight() -> Result<TccStatus> {
    let s = tcc_status();
    let mut missing = Vec::new();
    if !s.accessibility {
        missing.push("손쉬운 사용");
    }
    if !s.screen_capture {
        missing.push("화면 기록");
    }
    if !missing.is_empty() {
        bail!(
            "Mac 앱을 조작하려면 GhostDesk 에 macOS 권한({})이 필요합니다. 사용자에게 GhostDesk 창의 설정 > 권한 에서 권한마다 \
             '허용' 을 누르고, 뜨는 macOS 창의 '시스템 설정 열기' 에서 GhostDesk 스위치를 켜 달라고 하세요. 화면 기록은 켠 뒤 GhostDesk 를 다시 시작해야 반영됩니다.",
            missing.join(" · ")
        );
    }
    Ok(s)
}

/// 이번 실행에서 화면 기록을 이미 요청했는가. 화면 기록 상태(CGPreflightScreenCaptureAccess)는 다시 시작해야 바뀌므로,
/// 요청한 뒤 사용자가 켰더라도 이번 실행에서는 여전히 꺼짐으로 보인다 → 그때 다시 지우면 방금 켠 것을 없애게 된다.
static SCREEN_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 이번 실행에서 화면 기록을 요청했는가 (그러면 화면 기록은 다시 시작해야 반영된다)
pub fn screen_capture_needs_restart() -> bool {
    SCREEN_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) && !tcc_status().screen_capture
}

/// GhostDesk 의 권한 항목을 지운다 (이 번들 id 의 항목만, 관리자 권한 없이).
/// GhostDesk 는 개발자 서명이 없는 자체 서명이라 macOS 가 권한을 '그 빌드의 지문' 에 묶는다. 업데이트하면 지문이 바뀌어
/// 시스템 설정의 스위치는 켜져 보여도 지금 빌드에는 적용되지 않는다. 그 낡은 항목을 지워야 지금 빌드로 새로 요청할 수 있다.
fn reset_permission(service: &str) {
    let _ = std::process::Command::new("/usr/bin/tccutil").args(["reset", service, crate::install::BUNDLE_ID]).output();
}

/// 손쉬운 사용 '허용': 낡은 항목을 지우고 macOS 의 요청 창만 띄운다 (그 창의 '시스템 설정 열기' 로 스위치를 켠다).
/// 창을 하나만 띄우려고 시스템 설정은 직접 열지 않는다. 이미 켜져 있으면 아무것도 하지 않는다.
/// 손쉬운 사용은 바로 반영되므로, 지금 꺼져 있으면 켜 둔 스위치가 있어도 옛 빌드 것이다.
pub fn request_accessibility() {
    use objc2_application_services::{kAXTrustedCheckOptionPrompt, AXIsProcessTrustedWithOptions};
    use objc2_core_foundation::{CFBoolean, CFDictionary};
    if tcc_status().accessibility {
        return;
    }
    reset_permission("Accessibility");
    // 프롬프트를 켜고 물으면 macOS 가 GhostDesk 를 손쉬운 사용 목록에 (꺼진 채로) 넣고 안내 창을 띄운다
    let key = unsafe { kAXTrustedCheckOptionPrompt };
    let opts = CFDictionary::from_slices(&[key], &[CFBoolean::new(true)]);
    unsafe { AXIsProcessTrustedWithOptions(Some(opts.as_opaque())) };
}

/// 화면 기록 '허용': (이번 실행에서 처음이면) 낡은 항목을 지우고 macOS 의 요청 창만 띄운다.
/// 화면 기록은 다시 시작해야 반영되므로 두 번째부터는 지우지 않는다 (사용자가 방금 켰을 수 있다).
pub fn request_screen_capture() {
    use std::sync::atomic::Ordering;
    if tcc_status().screen_capture {
        return;
    }
    if !SCREEN_REQUESTED.swap(true, Ordering::SeqCst) {
        reset_permission("ScreenCapture");
    }
    objc2_core_graphics::CGRequestScreenCaptureAccess();
}
