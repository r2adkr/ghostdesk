//! macOS platform primitives.
//!
//! - [`ax`]: Accessibility API wrapper (element tree, attribute reads/writes, actions)
//! - [`capture`]: ScreenCaptureKit single-window / display capture
//! - [`input`]: CGEvent input, `pid_*` (target process only) and `global::*` (dedicated agent
//!   session only)
//! - [`skylight`]: private SkyLight APIs (front process lookup, key-window records sent only to
//!   the target PSN)
//! - [`apps`]: launching, hiding and quitting apps, CGWindowList window lists
//! - [`guard`]: exposure guard that re-hides hidden-launched apps that show up on screen
//!
//! Ground rule in the user's session: never change the user's cursor, front app or windows.
//! Global event posting, cursor warps and app activate/unhide/AXRaise are never used on the
//! user-session paths of this module.
#![allow(dead_code)]

pub mod apps;
pub mod ax;
pub mod capture;
pub mod guard;
pub mod input;
pub mod skylight;

use anyhow::{bail, Result};

/// TCC permission state.
#[derive(Clone, Copy, Debug)]
pub struct TccStatus {
    /// Accessibility (AX, CGEventPostToPid).
    pub accessibility: bool,
    /// Screen & System Audio Recording (ScreenCaptureKit).
    pub screen_capture: bool,
    /// Event posting (CGPreflightPostEventAccess).
    pub post_event: bool,
}

/// Checks the current state without showing any permission prompt.
pub fn tcc_status() -> TccStatus {
    TccStatus {
        accessibility: ax::is_trusted(),
        screen_capture: objc2_core_graphics::CGPreflightScreenCaptureAccess(),
        post_event: objc2_core_graphics::CGPreflightPostEventAccess(),
    }
}

/// Returns an error telling the user what to do if the native app backend lacks a permission.
///
/// Permission prompts are only shown when the user clicks Allow (`request_accessibility`,
/// `request_screen_capture`).
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

/// Whether screen recording was already requested in this run.
///
/// The screen recording state (CGPreflightScreenCaptureAccess) only changes after a restart, so
/// even if the user turned it on after the request it still reads as off in this run. Resetting
/// it again then would wipe out what they just turned on.
static SCREEN_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether screen recording was requested in this run (and so needs a restart to take effect).
pub fn screen_capture_needs_restart() -> bool {
    SCREEN_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) && !tcc_status().screen_capture
}

/// Resets GhostDesk's permission entries (this bundle id only, no admin rights needed).
///
/// GhostDesk is self-signed without a developer ID, so macOS ties permissions to the build's
/// fingerprint. After an update the fingerprint changes, and the switch in System Settings looks
/// on but doesn't apply to the current build. The stale entry has to go before the current build
/// can ask again.
fn reset_permission(service: &str) {
    let _ = std::process::Command::new("/usr/bin/tccutil").args(["reset", service, crate::install::BUNDLE_ID]).output();
}

/// Accessibility "Allow": resets the stale entry and shows only macOS's prompt (whose "Open
/// System Settings" leads to the switch).
///
/// System Settings isn't opened directly, to keep it to one window. No-op if already granted.
/// Accessibility takes effect immediately, so if it's off now, any switch that looks on belongs
/// to an old build.
pub fn request_accessibility() {
    use objc2_application_services::{kAXTrustedCheckOptionPrompt, AXIsProcessTrustedWithOptions};
    use objc2_core_foundation::{CFBoolean, CFDictionary};
    if tcc_status().accessibility {
        return;
    }
    reset_permission("Accessibility");
    // asking with the prompt option makes macOS add GhostDesk to the Accessibility list (off) and
    // show its prompt
    let key = unsafe { kAXTrustedCheckOptionPrompt };
    let opts = CFDictionary::from_slices(&[key], &[CFBoolean::new(true)]);
    unsafe { AXIsProcessTrustedWithOptions(Some(opts.as_opaque())) };
}

/// Screen recording "Allow": on the first request in this run, resets the stale entry and shows
/// only macOS's prompt.
///
/// Screen recording needs a restart to take effect, so later requests don't reset (the user may
/// have just turned it on).
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
