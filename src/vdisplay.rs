//! (Experimental, opt-in) macOS backend that uses a virtual display as the agent's own screen.
//!
//! ## Idea
//! Hidden app windows (the native backend) get no mouse events. Input aimed at the target pid
//! (`CGEventPostToPid` + a SkyLight key window record sent only to the target PSN + window-local
//! coordinates via `CGEventSetWindowLocation`) only becomes real pixel clicks while the window is
//! **on screen (ordered in)**. So the private CoreGraphics class `CGVirtualDisplay` creates a
//! virtual monitor the user never sees, and the target app's windows are placed only on it. The
//! app stays inactive (not frontmost), so the user's cursor, focus and front app don't change.
//!
//! ## Limits and risks (the user must be told)
//! - The virtual display **joins** the user's display arrangement (there's no headless option).
//!   The user's cursor can cross the edge onto it, so it's attached at the outermost edge away
//!   from the Dock, bottom-aligned with the main display, to make that less likely. It's never
//!   made the main display or mirrored.
//! - Removing the display moves any windows left on it to another display, so shutdown ends the
//!   app first (or, in Mode B, puts windows back) before removing the display. User window
//!   positions are compared before creation, after creation and after removal, and reported.
//! - If the app opens a new window on a user display, the watchdog (50 ms) moves it to the
//!   virtual display, and hides the app if it can't.
//! - If the app activates itself (becomes frontmost) it takes the user's keyboard focus, so it's
//!   hidden immediately and reported in a note (Mode A).
//! - Open/save panels are windows of a separate system process and can't be clicked. Open panels
//!   (File > Open…, cmd+O) can appear on the user's screen, so they're blocked.
//!
//! ## Forbidden (on every path in this file)
//! Global `CGEventPost`, cursor warping, `SetFrontProcess`/`_SLPSSetFrontProcessWithOptions`,
//! `activateWithOptions` and `AXRaise` are never used. Input only goes to the pid we launched (or
//! attached to with consent).
//!
//! ## Screenshots
//! ScreenCaptureKit display filter + includingApplications(our pid) → captures the whole virtual
//! display but draws **only our app's windows** (the menu bar, wallpaper and any user windows
//! that wander onto the virtual display aren't captured). Agent coordinates (width×height
//! pixels) → global points = bounds.origin + p × (bounds.size / agent size).

#![allow(dead_code)]

use std::path::PathBuf;

/// Virtual display workspace options.
#[derive(Clone, Debug)]
pub struct VirtualDisplayOptions {
    /// Target app: `.app` path or bundle id.
    pub app: String,
    /// Agent screen (= virtual display points) width in pixels.
    pub width: u32,
    /// Agent screen height in pixels.
    pub height: u32,
    /// Mode B consent: explicit permission to move the user's running app's windows to the virtual
    /// display while working.
    pub attach_consent: bool,
    /// pid to attach to in Mode B (defaults to the bundle id's first instance).
    pub attach_pid: Option<i32>,
    /// Live view frames per second (0 = off).
    pub live_fps: f64,
    /// HiDPI virtual display (2x pixels). Default false = points and pixels are 1:1.
    pub hidpi: bool,
    /// Files to open at startup in Mode A.
    pub open_files: Vec<PathBuf>,
    /// Extra launch arguments.
    pub extra_args: Vec<String>,
}

impl Default for VirtualDisplayOptions {
    fn default() -> Self {
        VirtualDisplayOptions {
            app: String::new(),
            width: 1280,
            height: 800,
            attach_consent: false,
            attach_pid: None,
            live_fps: 2.0,
            hidpi: false,
            open_files: vec![],
            extra_args: vec![],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VdMode {
    /// An instance we launched (its process is ended on shutdown).
    Launched,
    /// Attached with consent to the user's running instance (window positions and hidden state are
    /// restored on shutdown).
    Attached,
}

/// Environment section of the agent system prompt (same format as agent.rs's ENV_*).
pub const ENV_VDISPLAY: &str = r#"Environment
- You operate a macOS app that runs on a separate virtual display reserved for you. The human does not see it on their own screen, and your input goes only to this app: it never moves the human's mouse pointer or keyboard focus.
- The screenshot is the whole virtual display but shows ONLY this app's windows on a black background (no menu bar, Dock or other apps). Coordinates are screenshot pixels (origin top-left).
- The mouse works like on a real screen: click, double/triple click, right click (context menus appear in the next screenshot), drag and scroll. Clicking a black area, or a spot that does not belong to the app, returns an error.
- The app's menu bar is not visible: use `app_control` menus to list it and menu {path: "File > New"} to run an item. Keyboard shortcuts (cmd+...) are delivered to the app.
- `app_control` ui_tree lists UI elements with ids and frames in screenshot pixels; press / set_value / focus / read work by id and are useful when a pixel click is ambiguous.
- Blocked for the human's safety: links that would open the human's browser, the minimize / full-screen window buttons, File > Open… (cmd+O), and system-wide shortcuts (lock screen, log out, force quit...). System open/save panels belong to another process and cannot be clicked; avoid them.
- New windows appear on your display automatically. If Events says the app was hidden, activated by the human, or the display disappeared, stop and ask the human via `ask_user`."#;

/// One-line description for control.rs (external MCP agents).
pub const CONTROL_HINT: &str = "보이지 않는 가상 디스플레이 위의 macOS 앱입니다. 실제 화면처럼 클릭·더블클릭·우클릭·드래그·스크롤이 됩니다 (입력은 그 앱에만 전달). 스크린샷에는 그 앱 창만 보이고 메뉴 막대는 없으니 메뉴는 desk_app {action:menus|menu}, 정확한 요소 조작은 ui_tree 후 press/set_value 를 쓰세요.";

pub const APP_CONTROL_DESCRIPTION: &str = "Semantic control of the macOS app running on your virtual display.\n\
Actions:\n\
- list_windows: list the app's windows (index, title).\n\
- switch_window {index}: make another window the keyboard target.\n\
- ui_tree {max_depth?}: numbered UI elements of the current window: id, role, title/description/value (truncated), frame [x,y,w,h] in SCREENSHOT pixels. Ids stay valid until the next ui_tree.\n\
- press {id}: AXPress a button/checkbox/menu item.\n\
- set_value {id, text}: replace the value of a text field/area (verified by re-reading).\n\
- focus {id}: give keyboard focus to an element.\n\
- read {id}: full value, selected text and visible range of an element.\n\
- menus: the app's menu bar (localized titles, shortcuts, disabled/blocked items).\n\
- menu {path}: run a menu bar item, e.g. \"File > Save\" or localized \"파일 > 저장\".\n\
Blocked: the Apple menu, Services / Hide Others / Show All, Help, Share, dictation / emoji / writing tools, File > Open…, links, minimize / full-screen buttons.";

/// The `app_control` tool definition exposed to the LLM (the virtual display subset).
pub fn app_control_tool() -> serde_json::Value {
    serde_json::json!({
        "name": "app_control",
        "description": APP_CONTROL_DESCRIPTION,
        "input_schema": {
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list_windows", "switch_window", "ui_tree", "press", "set_value", "focus", "read", "menus", "menu"]
                },
                "index": { "type": "integer", "description": "switch_window: window index from list_windows" },
                "id": { "type": "integer", "description": "element id from the latest ui_tree" },
                "text": { "type": "string", "description": "set_value: new value" },
                "path": { "type": "string", "description": "menu: e.g. \"File > Save\"" },
                "max_depth": { "type": "integer", "description": "ui_tree: max depth (default 30)" }
            },
            "required": ["action"]
        }
    })
}

// ---- pure geometry (all OSes) ----

pub mod geom {
    /// A rectangle in global point coordinates (top-left origin, y grows downward; the same space as
    /// CGDisplayBounds/CGWindowList).
    #[derive(Clone, Copy, Debug, PartialEq, Default)]
    pub struct Rect {
        pub x: f64,
        pub y: f64,
        pub w: f64,
        pub h: f64,
    }

    impl Rect {
        pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Rect {
            Rect { x, y, w, h }
        }
        pub fn max_x(&self) -> f64 {
            self.x + self.w
        }
        pub fn max_y(&self) -> f64 {
            self.y + self.h
        }
        pub fn area(&self) -> f64 {
            self.w.max(0.0) * self.h.max(0.0)
        }
        pub fn contains(&self, px: f64, py: f64) -> bool {
            px >= self.x && py >= self.y && px < self.max_x() && py < self.max_y()
        }
        pub fn intersection_area(&self, o: &Rect) -> f64 {
            let w = (self.max_x().min(o.max_x()) - self.x.max(o.x)).max(0.0);
            let h = (self.max_y().min(o.max_y()) - self.y.max(o.y)).max(0.0);
            w * h
        }
        /// Whether self fits entirely inside outer (with tol points of slack).
        pub fn inside(&self, outer: &Rect, tol: f64) -> bool {
            self.x >= outer.x - tol
                && self.y >= outer.y - tol
                && self.max_x() <= outer.max_x() + tol
                && self.max_y() <= outer.max_y() + tol
        }
        pub fn approx_eq(&self, o: &Rect, tol: f64) -> bool {
            (self.x - o.x).abs() <= tol
                && (self.y - o.y).abs() <= tol
                && (self.w - o.w).abs() <= tol
                && (self.h - o.h).abs() <= tol
        }
    }

    /// Which side to attach the virtual display on.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Side {
        Right,
        Left,
    }

    /// With the Dock on the right, it moves to the rightmost display, so attach on the left end then.
    pub fn side_for_dock(orientation: Option<&str>) -> Side {
        match orientation.map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if s == "right" => Side::Left,
            _ => Side::Right,
        }
    }

    fn v_overlap(y: f64, h: f64, d: &Rect) -> f64 {
        (y + h).min(d.max_y()) - y.max(d.y)
    }

    /// Origin for the new display (size, in points).
    ///
    /// - Right: attached outside the arrangement's rightmost edge (max x); Left: outside the leftmost
    ///   edge (min x). No other display lies beyond it, so nothing overlaps.
    /// - Vertical: bottom-aligned with the main display. If at that height it wouldn't touch the
    ///   reference (outermost) display vertically (macOS moves displays arbitrarily when the
    ///   arrangement has gaps), it's bottom-aligned with the reference display instead.
    pub fn arrangement_origin(displays: &[Rect], main: &Rect, size: (f64, f64), side: Side) -> (i32, i32) {
        let (w, h) = size;
        let y_main = main.max_y() - h;
        let better = |cand: &Rect, best: &Rect| -> bool {
            let (ce, be) = match side {
                Side::Right => (cand.max_x(), best.max_x()),
                Side::Left => (-cand.x, -best.x),
            };
            ce > be + 0.5 || ((ce - be).abs() <= 0.5 && v_overlap(y_main, h, cand) > v_overlap(y_main, h, best))
        };
        let mut anchor = *main;
        for d in displays {
            if better(d, &anchor) {
                anchor = *d;
            }
        }
        let x = match side {
            Side::Right => anchor.max_x(),
            Side::Left => anchor.x - w,
        };
        let mut y = y_main;
        if v_overlap(y, h, &anchor) < 1.0 {
            y = anchor.max_y() - h;
        }
        (x.round() as i32, y.round() as i32)
    }

    /// Agent pixels → global points (off-screen coordinates are clamped to the edge).
    pub fn agent_to_global(b: &Rect, agent: (u32, u32), x: f64, y: f64) -> (f64, f64) {
        let (aw, ah) = (agent.0.max(1) as f64, agent.1.max(1) as f64);
        let x = x.clamp(0.0, aw - 1.0);
        let y = y.clamp(0.0, ah - 1.0);
        (b.x + x * b.w / aw, b.y + y * b.h / ah)
    }

    /// Global points → agent pixels.
    pub fn global_to_agent(b: &Rect, agent: (u32, u32), gx: f64, gy: f64) -> (f64, f64) {
        let (aw, ah) = (agent.0.max(1) as f64, agent.1.max(1) as f64);
        ((gx - b.x) * aw / b.w.max(1.0), (gy - b.y) * ah / b.h.max(1.0))
    }

    /// Global rectangle → agent pixels [x, y, w, h].
    pub fn rect_to_agent(b: &Rect, agent: (u32, u32), r: &Rect) -> [i64; 4] {
        let (x0, y0) = global_to_agent(b, agent, r.x, r.y);
        let (x1, y1) = global_to_agent(b, agent, r.max_x(), r.max_y());
        [x0.round() as i64, y0.round() as i64, (x1 - x0).round() as i64, (y1 - y0).round() as i64]
    }

    /// Zoom region (agent pixels [x0,y0,x1,y1]) → (x, y, w, h) to crop from the native-resolution
    /// capture.
    pub fn region_to_pixels(region: [f64; 4], agent: (u32, u32), full: (u32, u32)) -> (u32, u32, u32, u32) {
        let kx = full.0 as f64 / agent.0.max(1) as f64;
        let ky = full.1 as f64 / agent.1.max(1) as f64;
        let (x0, y0) = (region[0].min(region[2]).max(0.0), region[1].min(region[3]).max(0.0));
        let (x1, y1) = (region[0].max(region[2]), region[1].max(region[3]));
        let cx = ((x0 * kx) as u32).min(full.0.saturating_sub(1));
        let cy = ((y0 * ky) as u32).min(full.1.saturating_sub(1));
        let cw = (((x1 - x0) * kx).max(1.0) as u32).min(full.0 - cx).max(1);
        let ch = (((y1 - y0) * ky).max(1.0) as u32).min(full.1 - cy).max(1);
        (cx, cy, cw, ch)
    }

    /// Frame that fits the window inside the display (keeping the top top_inset points free for the
    /// menu bar).
    ///
    /// Unchanged if it already fits, otherwise centered (and shrunk if needed).
    pub fn fit_window(win: &Rect, disp: &Rect, top_inset: f64) -> Rect {
        let avail = Rect::new(disp.x, disp.y + top_inset, disp.w, (disp.h - top_inset).max(1.0));
        if win.inside(&avail, 0.5) {
            return *win;
        }
        let w = win.w.min(avail.w).max(1.0);
        let h = win.h.min(avail.h).max(1.0);
        Rect::new(
            (avail.x + (avail.w - w) / 2.0).round(),
            (avail.y + (avail.h - h) / 2.0).round(),
            w.round(),
            h.round(),
        )
    }

    /// Whether the window overlaps a user display (others) at all (more than 1 pt²).
    pub fn is_stray(win: &Rect, others: &[Rect]) -> bool {
        others.iter().any(|d| win.intersection_area(d) > 1.0)
    }

    /// Straight path from → to (start excluded, end included).
    pub fn interp(from: (f64, f64), to: (f64, f64), steps: usize) -> Vec<(f64, f64)> {
        let n = steps.max(1);
        (1..=n)
            .map(|i| {
                let t = i as f64 / n as f64;
                (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
            })
            .collect()
    }

    /// Number of intermediate drag events.
    pub fn drag_steps(from: (f64, f64), to: (f64, f64)) -> usize {
        let d = ((to.0 - from.0).powi(2) + (to.1 - from.1).powi(2)).sqrt();
        ((d / 8.0) as usize).clamp(6, 60)
    }

    /// Splits a scroll amount (pixels) into pieces of at most max per event (keeping the sign).
    pub fn scroll_chunks(d: f64, max: f64) -> Vec<f64> {
        let max = max.abs().max(1.0);
        let mut out = vec![];
        let mut left = d;
        while left.abs() > 0.5 {
            let c = left.clamp(-max, max);
            out.push(c);
            left -= c;
        }
        out
    }

    /// Virtual display mode size (points). Half the pixels when hidpi.
    pub fn mode_for(width_px: u32, height_px: u32, hidpi: bool) -> (u32, u32) {
        if hidpi {
            ((width_px / 2).max(1), (height_px / 2).max(1))
        } else {
            (width_px, height_px)
        }
    }

    /// Physical size (mm) at 110 ppi; reporting something too large or small makes macOS pick an odd
    /// default scale.
    pub fn physical_size_mm(w_pt: f64, h_pt: f64) -> (f64, f64) {
        (w_pt / 110.0 * 25.4, h_pt / 110.0 * 25.4)
    }

    /// One line of a user window snapshot.
    #[derive(Clone, Debug, PartialEq)]
    pub struct WinSnap {
        pub wid: u32,
        pub pid: i32,
        pub rect: Rect,
        pub name: String,
    }

    /// Windows whose position/size changed between two snapshots (only windows present in both are
    /// compared; closed or newly opened windows may be user activity).
    pub fn diff_windows(before: &[WinSnap], after: &[WinSnap], tol: f64) -> Vec<String> {
        let mut out = vec![];
        for b in before {
            if let Some(a) = after.iter().find(|a| a.wid == b.wid) {
                if !a.rect.approx_eq(&b.rect, tol) {
                    out.push(format!(
                        "창 {} (pid {}, '{}'): ({:.0},{:.0} {:.0}x{:.0}) → ({:.0},{:.0} {:.0}x{:.0})",
                        b.wid, b.pid, b.name, b.rect.x, b.rect.y, b.rect.w, b.rect.h, a.rect.x, a.rect.y, a.rect.w, a.rect.h
                    ));
                }
            }
        }
        out
    }

    /// For deciding which window covers a point (excluding full-screen overlays): whether r covers 95%
    /// or more of the display area.
    pub fn covers_display(r: &Rect, disp: &Rect) -> bool {
        r.intersection_area(disp) >= disp.area() * 0.95
    }

    /// Strips offset digits from an Objective-C type encoding ("v20@0:8I16" → "v@:I").
    pub fn strip_encoding(enc: &str) -> String {
        enc.chars().filter(|c| !c.is_ascii_digit()).collect()
    }
}

#[cfg(target_os = "macos")]
#[allow(unused_imports)]
pub use imp::*;

#[cfg(not(target_os = "macos"))]
#[allow(unused_imports)]
pub use stub::*;

// ═════════════════════════════════════════ macOS ═════════════════════════════════════════

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::HashSet;
    use std::ffi::{c_char, c_void, CStr};
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};

    use anyhow::{anyhow, bail, Result};
    use block2::RcBlock;
    use image::RgbImage;
    use objc2::rc::{Allocated, Retained};
    use objc2::runtime::{AnyClass, AnyObject, Sel};
    use objc2::{msg_send, sel, AllocAnyThread};
    use objc2_application_services::{AXValue, AXValueType};
    use objc2_core_foundation::{CFRetained, CFString, CFType, CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{
        CGBeginDisplayConfiguration, CGCancelDisplayConfiguration, CGCompleteDisplayConfiguration,
        CGConfigureDisplayFadeEffect, CGConfigureDisplayOrigin, CGConfigureOption, CGDisplayBounds, CGDisplayConfigRef,
        CGDisplayCopyDisplayMode, CGDisplayIsInMirrorSet, CGDisplayMirrorsDisplay, CGDisplayMode, CGDisplayPixelsHigh,
        CGDisplayPixelsWide, CGError, CGEvent, CGEventField,
        CGEventSource, CGEventSourceStateID, CGEventType, CGGetActiveDisplayList, CGImage, CGMainDisplayID,
        CGMouseButton, CGScrollEventUnit,
    };
    use objc2_foundation::{NSArray, NSError, NSString};
    use objc2_screen_capture_kit::{
        SCContentFilter, SCDisplay, SCRunningApplication, SCScreenshotManager, SCStreamConfiguration, SCWindow,
    };
    use parking_lot::Mutex;
    use serde_json::Value;

    use super::geom::{self, Rect, WinSnap};
    use super::{VdMode, VirtualDisplayOptions};
    use crate::browser::{FrameSink, MouseButton};
    use crate::desk::ViewInfo;
    use crate::keys::{MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT};
    use crate::macos::{
        apps,
        ax::{self, El},
        capture,
        guard::ExposureGuard,
        input, skylight,
    };

    // the private classes (CGVirtualDisplay*) live in CoreGraphics, which must be loaded before the
    // class lookup
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {}

    extern "C" {
        /// libdispatch (libSystem). attr = NULL → serial queue. Returns a +1 retained OS_dispatch_queue
        /// (an ObjC object).
        fn dispatch_queue_create(label: *const c_char, attr: *mut c_void) -> *mut AnyObject;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFPreferencesCopyAppValue(key: *const c_void, app: *const c_void) -> *const c_void;
    }

    /// Space kept free at the top of the window for the menu bar (with "Displays have separate
    /// Spaces", the menu bar is drawn on the virtual display too).
    const TOP_INSET: f64 = 30.0;
    const NO_WINDOW: &str = "에이전트 앱 창이 없는 곳입니다";

    /// Runs on a worker thread. Tokio worker threads are reused and long-lived, so each call is
    /// wrapped in an autorelease pool to keep objects autoreleased by AppKit/SCK from piling up until
    /// the thread exits.
    async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
        tokio::task::spawn_blocking(move || objc2::rc::autoreleasepool(|_| f()))
            .await
            .map_err(|e| anyhow!("작업 스레드 오류: {e}"))
    }

    fn sleep_ms(ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }

    fn r(c: CGRect) -> Rect {
        Rect::new(c.origin.x, c.origin.y, c.size.width, c.size.height)
    }

    // ---- display queries ----

    fn active_displays() -> Vec<u32> {
        let mut ids = [0u32; 32];
        let mut n = 0u32;
        let e = unsafe { CGGetActiveDisplayList(32, ids.as_mut_ptr(), &mut n) };
        if e != CGError::Success {
            return vec![];
        }
        ids[..(n as usize).min(32)].to_vec()
    }

    fn display_rect(id: u32) -> Rect {
        r(CGDisplayBounds(id))
    }

    /// Rects of the active displays other than id.
    fn other_display_rects(id: u32) -> Vec<Rect> {
        active_displays().into_iter().filter(|d| *d != id).map(display_rect).collect()
    }

    /// Dock position (com.apple.dock orientation: bottom/left/right). Read only.
    fn dock_orientation() -> Option<String> {
        let key = CFString::from_str("orientation");
        let app = CFString::from_str("com.apple.dock");
        let p = unsafe {
            CFPreferencesCopyAppValue(
                CFRetained::as_ptr(&key).as_ptr() as *const c_void,
                CFRetained::as_ptr(&app).as_ptr() as *const c_void,
            )
        };
        let p = NonNull::new(p as *mut CFType)?;
        let v: CFRetained<CFType> = unsafe { CFRetained::from_raw(p) };
        v.downcast_ref::<CFString>().map(|s| s.to_string())
    }

    // ---- private class checks (read only) ----

    /// (class, selector, expected encoding with digits stripped), as found in this Mac's runtime
    /// (macOS 27.0.1).
    const REQUIRED: &[(&str, &str, &str)] = &[
        ("CGVirtualDisplayDescriptor", "init", "@@:"),
        ("CGVirtualDisplayDescriptor", "setName:", "v@:@"),
        ("CGVirtualDisplayDescriptor", "setMaxPixelsWide:", "v@:I"),
        ("CGVirtualDisplayDescriptor", "setMaxPixelsHigh:", "v@:I"),
        ("CGVirtualDisplayDescriptor", "setSizeInMillimeters:", "v@:{CGSize=dd}"),
        ("CGVirtualDisplayDescriptor", "setVendorID:", "v@:I"),
        ("CGVirtualDisplayDescriptor", "setProductID:", "v@:I"),
        ("CGVirtualDisplayDescriptor", "setSerialNum:", "v@:I"),
        ("CGVirtualDisplayDescriptor", "setTerminationHandler:", "v@:@?"),
        ("CGVirtualDisplay", "initWithDescriptor:", "@@:@"),
        ("CGVirtualDisplay", "displayID", "I@:"),
        ("CGVirtualDisplay", "applySettings:", "B@:@"),
        ("CGVirtualDisplaySettings", "init", "@@:"),
        ("CGVirtualDisplaySettings", "setHiDPI:", "v@:I"),
        ("CGVirtualDisplaySettings", "setModes:", "v@:@"),
        ("CGVirtualDisplayMode", "initWithWidth:height:refreshRate:", "@@:IId"),
    ];

    fn class(name: &str) -> Result<&'static AnyClass> {
        let c = std::ffi::CString::new(name).unwrap();
        AnyClass::get(&c).ok_or_else(|| anyhow!("비공개 클래스 {name} 가 없습니다 (이 macOS 에서는 가상 디스플레이를 쓸 수 없음)"))
    }

    fn method_encoding(cls: &AnyClass, sel: Sel) -> Option<String> {
        let m = cls.instance_method(sel)?;
        let p = unsafe { objc2::ffi::method_getTypeEncoding(m as *const _ as *const _) };
        if p.is_null() {
            return Some(String::new());
        }
        Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().to_string())
    }

    /// The descriptor's queue setter selector (setQueue: preferred, setDispatchQueue: otherwise).
    fn queue_selector(cls: &AnyClass) -> Option<Sel> {
        [sel!(setQueue:), sel!(setDispatchQueue:)]
            .into_iter()
            .find(|s| method_encoding(cls, *s).map(|e| geom::strip_encoding(&e) == "v@:@").unwrap_or(false))
    }

    /// Read-only check results (one line per item). Only queries class objects; no instances are
    /// created.
    pub fn available_report() -> Result<Vec<String>> {
        let mut lines = vec![];
        let mut missing = vec![];
        for cname in ["CGVirtualDisplay", "CGVirtualDisplayDescriptor", "CGVirtualDisplaySettings", "CGVirtualDisplayMode"] {
            match class(cname) {
                Ok(_) => lines.push(format!("클래스 {cname}: 있음")),
                Err(_) => missing.push(format!("클래스 {cname} 없음")),
            }
        }
        if !missing.is_empty() {
            bail!("가상 디스플레이 API 를 쓸 수 없습니다: {}", missing.join(", "));
        }
        for (cname, s, want) in REQUIRED {
            let cls = class(cname)?;
            let sel = Sel::register(&std::ffi::CString::new(*s).unwrap());
            match method_encoding(cls, sel) {
                Some(enc) if geom::strip_encoding(&enc) == *want => lines.push(format!("-[{cname} {s}] {enc}")),
                Some(enc) => missing.push(format!("-[{cname} {s}] 인코딩 불일치: {enc} (기대 {want})")),
                None => missing.push(format!("-[{cname} {s}] 없음")),
            }
        }
        match queue_selector(class("CGVirtualDisplayDescriptor")?) {
            Some(s) => lines.push(format!("큐 셀렉터: {}", s.name().to_string_lossy())),
            None => missing.push("디스크립터 큐 셀렉터(setQueue:/setDispatchQueue:) 없음".into()),
        }
        for sym in ["CGEventSetWindowLocation", "SLPSPostEventRecordTo", "GetProcessForPID", "_SLPSGetFrontProcess"] {
            if skylight::sym(sym).is_null() {
                missing.push(format!("심볼 {sym} 없음"));
            } else {
                lines.push(format!("심볼 {sym}: 있음"));
            }
        }
        if !missing.is_empty() {
            bail!("가상 디스플레이 백엔드에 필요한 API 가 없거나 달라졌습니다:\n{}", missing.join("\n"));
        }
        Ok(lines)
    }

    /// Checks read-only that the private classes, selectors and private input symbols exist (no
    /// display is created).
    pub fn available() -> Result<()> {
        available_report().map(|_| ())
    }

    // ───────────────────────── VirtualDisplay (RAII) ─────────────────────────

    static SERIAL: AtomicU32 = AtomicU32::new(0);

    type TermBlock = RcBlock<dyn Fn(*mut AnyObject, *mut AnyObject)>;

    /// A single CGVirtualDisplay. Dropping it removes the display.
    pub struct VirtualDisplay {
        display: Option<Retained<AnyObject>>,
        queue: Option<Retained<AnyObject>>,
        term: Option<TermBlock>,
        terminated: Arc<AtomicBool>,
        id: u32,
        hidpi: bool,
        name: String,
        /// Notes left during creation and placement.
        pub notes: Vec<String>,
    }
    // CGVirtualDisplay is treated as a thread-safe proxy object (only created and released, no other
    // messages sent)
    unsafe impl Send for VirtualDisplay {}
    unsafe impl Sync for VirtualDisplay {}

    unsafe fn new_obj(cname: &str) -> Result<Retained<AnyObject>> {
        let cls = class(cname)?;
        let a: Allocated<AnyObject> = msg_send![cls, alloc];
        let o: Option<Retained<AnyObject>> = msg_send![a, init];
        o.ok_or_else(|| anyhow!("{cname} init 이 nil 을 돌려주었습니다"))
    }

    impl VirtualDisplay {
        /// Creates a virtual display and attaches it at the arrangement's outermost edge (right by
        /// default, left when the Dock is on the right), bottom-aligned with the main display. If it
        /// becomes the main display or gets mirrored, it's removed immediately with an error.
        ///
        /// width_px/height_px = framebuffer pixels. With hidpi the point size is half that.
        pub fn create(width_px: u32, height_px: u32, hidpi: bool, name: &str) -> Result<VirtualDisplay> {
            if !(320..=7680).contains(&width_px) || !(240..=4320).contains(&height_px) {
                bail!("가상 디스플레이 크기가 범위를 벗어났습니다: {width_px}x{height_px}");
            }
            available()?;
            let _ = CGMainDisplayID(); // initializes the CGS connection from the CLI
            let main_before = CGMainDisplayID();
            let before_ids: HashSet<u32> = active_displays().into_iter().collect();
            let before_rects: Vec<(u32, Rect)> = before_ids.iter().map(|d| (*d, display_rect(*d))).collect();
            let (mode_w, mode_h) = geom::mode_for(width_px, height_px, hidpi);
            let (mm_w, mm_h) = geom::physical_size_mm(mode_w as f64, mode_h as f64);
            let serial = 0x4744_0001u32.wrapping_add(SERIAL.fetch_add(1, Ordering::SeqCst));
            let terminated = Arc::new(AtomicBool::new(false));

            let mut vd = unsafe {
                let q = dispatch_queue_create(c"com.ghostdesk.virtual-display".as_ptr(), std::ptr::null_mut());
                let queue = Retained::from_raw(q).ok_or_else(|| anyhow!("dispatch 큐를 만들지 못했습니다"))?;
                let dcls = class("CGVirtualDisplayDescriptor")?;
                let desc = new_obj("CGVirtualDisplayDescriptor")?;
                let ns_name = NSString::from_str(name);
                let _: () = msg_send![&*desc, setName: &*ns_name];
                let _: () = msg_send![&*desc, setMaxPixelsWide: width_px];
                let _: () = msg_send![&*desc, setMaxPixelsHigh: height_px];
                let _: () = msg_send![&*desc, setSizeInMillimeters: CGSize { width: mm_w, height: mm_h }];
                let _: () = msg_send![&*desc, setVendorID: 0x4744u32];
                let _: () = msg_send![&*desc, setProductID: 0x5644u32];
                let _: () = msg_send![&*desc, setSerialNum: serial];
                match queue_selector(dcls) {
                    Some(s) if s == sel!(setQueue:) => {
                        let _: () = msg_send![&*desc, setQueue: &*queue];
                    }
                    Some(_) => {
                        let _: () = msg_send![&*desc, setDispatchQueue: &*queue];
                    }
                    None => bail!("디스크립터 큐 셀렉터가 없습니다"),
                }
                let flag = terminated.clone();
                let term: TermBlock = RcBlock::new(move |_a: *mut AnyObject, _b: *mut AnyObject| {
                    flag.store(true, Ordering::SeqCst);
                });
                let _: () = msg_send![&*desc, setTerminationHandler: &*term];

                let cls = class("CGVirtualDisplay")?;
                let a: Allocated<AnyObject> = msg_send![cls, alloc];
                let display: Option<Retained<AnyObject>> = msg_send![a, initWithDescriptor: &*desc];
                let display = display.ok_or_else(|| anyhow!("CGVirtualDisplay 를 만들지 못했습니다 (initWithDescriptor 가 nil)"))?;
                let id: u32 = msg_send![&*display, displayID];
                VirtualDisplay {
                    display: Some(display),
                    queue: Some(queue),
                    term: Some(term),
                    terminated,
                    id,
                    hidpi,
                    name: name.to_string(),
                    notes: vec![],
                }
            };
            // from here on an error drops vd, which removes the display
            if vd.id == 0 {
                bail!("가상 디스플레이 id 가 0 입니다");
            }
            unsafe {
                let settings = new_obj("CGVirtualDisplaySettings")?;
                let _: () = msg_send![&*settings, setHiDPI: if hidpi { 1u32 } else { 0u32 }];
                let mcls = class("CGVirtualDisplayMode")?;
                let a: Allocated<AnyObject> = msg_send![mcls, alloc];
                let mode: Option<Retained<AnyObject>> =
                    msg_send![a, initWithWidth: mode_w, height: mode_h, refreshRate: 60.0f64];
                let mode = mode.ok_or_else(|| anyhow!("CGVirtualDisplayMode 를 만들지 못했습니다"))?;
                let modes = NSArray::from_retained_slice(&[mode]);
                let _: () = msg_send![&*settings, setModes: &*modes];
                let ok: bool = msg_send![vd.display.as_deref().unwrap(), applySettings: &*settings];
                if !ok {
                    bail!("가상 디스플레이 설정(applySettings:)이 거부되었습니다");
                }
            }
            // wait until it shows up in the active display list (up to 3 s)
            let t = Instant::now();
            while !active_displays().contains(&vd.id) {
                if t.elapsed() > Duration::from_secs(3) {
                    bail!("가상 디스플레이 {} 가 3초 안에 활성 목록에 나타나지 않았습니다", vd.id);
                }
                sleep_ms(20);
            }
            vd.check_safe(main_before)?;
            match vd.place() {
                Ok((x, y)) => vd.notes.push(format!("가상 디스플레이 {} 를 ({x}, {y}) 에 배치", vd.id)),
                Err(e) => vd.notes.push(format!("가상 디스플레이 배치 실패 (macOS 기본 위치 사용): {e}")),
            }
            vd.check_safe(main_before)?;
            // did any user display move?
            for (d, before) in &before_rects {
                let now = display_rect(*d);
                if !now.approx_eq(before, 0.5) {
                    vd.notes.push(format!(
                        "경고: 사용자 디스플레이 {d} 의 위치가 바뀌었습니다 ({:.0},{:.0}) → ({:.0},{:.0})",
                        before.x, before.y, now.x, now.y
                    ));
                }
            }
            let b = vd.bounds();
            let px = vd.pixel_size();
            vd.notes.push(format!(
                "가상 디스플레이 {}: {:.0}x{:.0} pt, {}x{} px, 원점 ({:.0},{:.0})",
                vd.id, b.w, b.h, px.0, px.1, b.x, b.y
            ));
            for n in &vd.notes {
                eprintln!("[ghostdesk] {n}");
            }
            Ok(vd)
        }

        /// Errors if the main display changed or got mirrored (the caller drops it → display removed).
        fn check_safe(&self, main_before: u32) -> Result<()> {
            let main_now = CGMainDisplayID();
            if main_now != main_before || main_now == self.id {
                bail!("가상 디스플레이를 만들자 주 디스플레이가 바뀌었습니다 ({main_before} → {main_now}) — 즉시 제거합니다");
            }
            if CGDisplayIsInMirrorSet(self.id) || CGDisplayMirrorsDisplay(self.id) != 0 {
                bail!("가상 디스플레이가 미러링 상태가 되었습니다 — 즉시 제거합니다");
            }
            Ok(())
        }

        /// Attaches it at the arrangement's outermost edge (kCGConfigureForSession; forgotten on log out).
        fn place(&self) -> Result<(i32, i32)> {
            let others = other_display_rects(self.id);
            let main = display_rect(CGMainDisplayID());
            let me = self.bounds();
            let side = geom::side_for_dock(dock_orientation().as_deref());
            let (x, y) = geom::arrangement_origin(&others, &main, (me.w, me.h), side);
            if (me.x - x as f64).abs() < 0.5 && (me.y - y as f64).abs() < 0.5 {
                return Ok((x, y));
            }
            unsafe {
                let mut cfg: CGDisplayConfigRef = std::ptr::null_mut();
                let e = CGBeginDisplayConfiguration(&mut cfg);
                if e != CGError::Success || cfg.is_null() {
                    bail!("CGBeginDisplayConfiguration 실패 ({e:?})");
                }
                // turn off the default reconfiguration fade (which briefly dims the user's screen)
                let _ = CGConfigureDisplayFadeEffect(cfg, 0.0, 0.0, 0.0, 0.0, 0.0);
                let e = CGConfigureDisplayOrigin(cfg, self.id, x, y);
                if e != CGError::Success {
                    CGCancelDisplayConfiguration(cfg);
                    bail!("CGConfigureDisplayOrigin 실패 ({e:?})");
                }
                let e = CGCompleteDisplayConfiguration(cfg, CGConfigureOption::ForSession);
                if e != CGError::Success {
                    bail!("CGCompleteDisplayConfiguration 실패 ({e:?})");
                }
            }
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(1) {
                let b = self.bounds();
                if (b.x - x as f64).abs() < 0.5 && (b.y - y as f64).abs() < 0.5 {
                    break;
                }
                sleep_ms(20);
            }
            let b = self.bounds();
            Ok((b.x.round() as i32, b.y.round() as i32))
        }

        pub fn display_id(&self) -> u32 {
            self.id
        }

        /// Region in global points (CGDisplayBounds).
        pub fn bounds(&self) -> Rect {
            display_rect(self.id)
        }

        /// Framebuffer pixel size. CGDisplayPixelsWide returns the point size in HiDPI modes, so the
        /// current mode's actual pixel size (CGDisplayModeGetPixelWidth) is used first.
        pub fn pixel_size(&self) -> (u32, u32) {
            if let Some(m) = CGDisplayCopyDisplayMode(self.id) {
                let (w, h) = (CGDisplayMode::pixel_width(Some(&m)), CGDisplayMode::pixel_height(Some(&m)));
                if w > 0 && h > 0 {
                    return (w as u32, h as u32);
                }
            }
            (CGDisplayPixelsWide(self.id) as u32, CGDisplayPixelsHigh(self.id) as u32)
        }

        pub fn is_active(&self) -> bool {
            active_displays().contains(&self.id)
        }

        /// Whether the system terminated the display (terminationHandler).
        pub fn terminated(&self) -> bool {
            self.terminated.load(Ordering::SeqCst)
        }

        /// Removes the display. Right before, records other people's windows on it (not our_pids and not
        /// GhostDesk itself), which macOS will move to another display.
        pub fn destroy(mut self, our_pids: &[i32]) -> Vec<String> {
            let mut notes = vec![];
            let b = self.bounds();
            let me = std::process::id() as i32;
            for w in apps::cg_windows(&[], true) {
                if w.layer != 0 || !w.visible() || w.pid == me || our_pids.contains(&w.pid) {
                    continue;
                }
                if r(w.bounds).intersection_area(&b) > 1.0 {
                    notes.push(format!(
                        "가상 디스플레이를 없앨 때 그 위에 다른 프로그램 창이 있었습니다: '{}' (pid {}, 창 {}) — macOS 가 다른 디스플레이로 옮깁니다",
                        w.name, w.pid, w.wid
                    ));
                }
            }
            let id = self.id;
            self.release();
            if active_displays().contains(&id) {
                notes.push(format!("가상 디스플레이 {id} 가 3초 안에 사라지지 않았습니다"));
            } else {
                notes.push(format!("가상 디스플레이 {id} 제거됨"));
            }
            notes
        }

        fn release(&mut self) {
            if let Some(d) = self.display.take() {
                drop(d);
                let t = Instant::now();
                while active_displays().contains(&self.id) && t.elapsed() < Duration::from_secs(3) {
                    sleep_ms(20);
                }
            }
            self.term.take();
            self.queue.take();
        }
    }

    impl Drop for VirtualDisplay {
        fn drop(&mut self) {
            self.release();
        }
    }

    // ---- user window snapshot ----

    /// Regular (layer 0) windows visible on the user's screen, excluding the given pids.
    fn snapshot_user_windows(exclude: &[i32]) -> Vec<WinSnap> {
        apps::cg_windows(&[], true)
            .into_iter()
            .filter(|w| w.layer == 0 && w.visible() && !exclude.contains(&w.pid))
            .map(|w| WinSnap { wid: w.wid, pid: w.pid, rect: r(w.bounds), name: w.name })
            .collect()
    }

    // ---- AX helpers ----

    fn size_value(s: CGSize) -> Option<CFRetained<AXValue>> {
        unsafe { AXValue::new(AXValueType::CGSize, NonNull::from(&s).cast()) }
    }

    /// Sets a window frame via AX (position → size → position). Returns the frame read back.
    fn set_frame(el: &El, f: Rect) -> Option<Rect> {
        let p = CGPoint { x: f.x, y: f.y };
        let s = CGSize { width: f.w, height: f.h };
        if let Some(v) = ax::point_value(p) {
            el.set("AXPosition", &v);
        }
        if let Some(v) = size_value(s) {
            el.set("AXSize", &v);
        }
        if let Some(v) = ax::point_value(p) {
            el.set("AXPosition", &v);
        }
        sleep_ms(20);
        el.frame().map(r)
    }

    const NODE_ATTRS: [&str; 10] = [
        "AXRole", "AXSubrole", "AXPosition", "AXSize", "AXTitle", "AXDescription", "AXValue", "AXEnabled", "AXFocused",
        "AXPlaceholderValue",
    ];

    #[derive(Clone, Debug)]
    struct Node {
        el: El,
        role: String,
        subrole: String,
        frame: Option<Rect>,
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
            let b = |i: usize, d: bool| v[i].as_deref().and_then(ax::cf_to_string).map(|x| x == "1").unwrap_or(d);
            let pos = v[2].as_deref().and_then(ax::cf_point);
            let size = v[3].as_deref().and_then(ax::cf_size);
            Node {
                el: el.clone(),
                role: s(0),
                subrole: s(1),
                frame: match (pos, size) {
                    (Some(o), Some(sz)) => Some(Rect::new(o.x, o.y, sz.width, sz.height)),
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

    fn short(s: &str, n: usize) -> String {
        let s = s.replace('\n', "⏎").replace('\r', "");
        if s.chars().count() <= n {
            s
        } else {
            format!("{}…", s.chars().take(n).collect::<String>())
        }
    }

    fn kids(n: &Node) -> Vec<El> {
        if matches!(n.role.as_str(), "AXTable" | "AXOutline" | "AXList" | "AXBrowser" | "AXGrid") {
            let vis = n.el.elements("AXVisibleRows");
            if !vis.is_empty() {
                let mut v: Vec<El> =
                    n.el.children().into_iter().filter(|c| c.role() != "AXRow" && c.role() != "AXColumn").collect();
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

    /// Whether pressing the element would affect the user's screen (links → the user's browser,
    /// minimize → the Dock, full screen → a Space switch).
    fn element_block_reason(role: &str, subrole: &str) -> Option<&'static str> {
        if role == "AXLink" {
            return Some("링크는 사용자의 브라우저를 사용자 화면에 열 수 있어 누르지 않았습니다. 링크 주소가 필요하면 app_control read 로 읽고 사용자에게 알리세요.");
        }
        match subrole {
            "AXMinimizeButton" => Some("최소화 버튼은 창을 사용자 화면의 Dock 으로 보내므로 누르지 않았습니다"),
            "AXFullScreenButton" | "AXZoomButton" => Some("전체 화면/확대 버튼은 디스플레이 공간을 바꾸므로 누르지 않았습니다"),
            _ => None,
        }
    }

    // ---- menu safety (trimmed copy of native.rs's rules) ----

    const R_APPLE: &str = "Apple 메뉴(잠금 화면·로그아웃·재시동·시스템 종료·강제 종료 등)는 사용자의 Mac 전체에 영향을 주므로 차단했습니다";
    const R_APP_MENU: &str = "앱 메뉴의 서비스·기타 가리기·모두 보기는 사용자의 다른 앱에 영향을 주므로 차단했습니다";
    const R_HELP: &str = "도움말은 별도 앱을 사용자 화면 앞에 띄우므로 차단했습니다";
    const R_SYSTEM_UI: &str = "받아쓰기·이모지·글쓰기 도구·자동 완성·창 배치·전체 화면 같은 시스템 UI 는 사용자 화면에 뜨므로 차단했습니다";
    const R_SHARE: &str = "공유는 다른 앱(메일·메시지 등)을 사용자 화면에 열 수 있어 차단했습니다";
    const R_OPEN: &str = "열기 패널은 별도 시스템 프로세스의 창이라 사용자 화면에 나타날 수 있어 차단했습니다";
    const HELP_TITLES: [&str; 16] = [
        "help", "도움말", "ヘルプ", "帮助", "幫助", "hilfe", "aide", "ayuda", "aiuto", "ajuda", "справка", "hjälp", "hulp", "pomoc",
        "yardım", "hjelp",
    ];
    const SHARE_TITLES: [&str; 6] = ["share", "공유", "共享", "共有", "teilen", "partager"];
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

    fn norm(s: &str) -> String {
        s.trim().trim_end_matches('…').trim_end_matches("...").trim().to_lowercase()
    }

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

    /// System shortcuts acting on the whole user session.
    fn system_combo_block(c: &input::MacCombo) -> Option<&'static str> {
        let has = |b: i64| c.mods & b != 0;
        match c.keycode {
            12 if has(MOD_META) && (has(MOD_CTRL) || has(MOD_SHIFT)) => Some(R_APPLE),
            53 if has(MOD_META) && has(MOD_ALT) => Some(R_APPLE),
            4 if has(MOD_META) && has(MOD_ALT) => Some(R_APP_MENU),
            49 if has(MOD_META) && has(MOD_CTRL) => Some(R_SYSTEM_UI),
            31 if has(MOD_META) && !has(MOD_SHIFT) && !has(MOD_CTRL) && !has(MOD_ALT) => Some(R_OPEN), // cmd+O
            _ => None,
        }
    }

    /// Finds the menu bar item with a shortcut (ch + modifiers): (path, block reason).
    fn find_menu_by_shortcut(app: &El, ch: char, mods: i64) -> Option<(String, Option<&'static str>)> {
        let bar = app.element("AXMenuBar")?;
        let want_mods = (if mods & MOD_SHIFT != 0 { 1 } else { 0 })
            | (if mods & MOD_ALT != 0 { 2 } else { 0 })
            | (if mods & MOD_CTRL != 0 { 4 } else { 0 })
            | (if mods & MOD_META != 0 { 0 } else { 8 });
        let want_ch = ch.to_ascii_uppercase().to_string();
        fn walk(
            top: &MenuTop,
            items: &[El],
            path: &str,
            want_ch: &str,
            want_mods: i64,
            depth: usize,
            inherited: Option<&'static str>,
        ) -> Option<(String, Option<&'static str>)> {
            for (i, it) in items.iter().enumerate() {
                let title = it.string("AXTitle").unwrap_or_default();
                let p = if path.is_empty() { title.clone() } else { format!("{path} > {title}") };
                if let Some(c) = it.string("AXMenuItemCmdChar") {
                    let m = it.number("AXMenuItemCmdModifiers").unwrap_or(0.0) as i64;
                    if c.to_uppercase() == want_ch && m == want_mods {
                        return Some((p, inherited.or_else(|| item_block_reason(top, items, i, depth))));
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

    // ---- input events ----

    /// The window receiving the event.
    #[derive(Clone, Copy, Debug)]
    struct Target {
        pid: i32,
        wid: u32,
        /// Window frame (global points).
        win: Rect,
    }

    fn button_info(b: MouseButton) -> (CGEventType, CGEventType, CGEventType, CGMouseButton, i64) {
        match b {
            MouseButton::Left => {
                (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp, CGEventType::LeftMouseDragged, CGMouseButton::Left, 0)
            }
            MouseButton::Right => (
                CGEventType::RightMouseDown,
                CGEventType::RightMouseUp,
                CGEventType::RightMouseDragged,
                CGMouseButton::Right,
                1,
            ),
            MouseButton::Middle => (
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                CGEventType::OtherMouseDragged,
                CGMouseButton::Center,
                2,
            ),
        }
    }

    /// Common fields for events carrying window-local coordinates (the combination verified on
    /// macOS 27: 1,3,7,40,51,58,91,92 + CGEventSetWindowLocation).
    fn stamp(ev: &CGEvent, t: &Target, g: CGPoint, click_state: i64, button_num: i64, mods: i64) {
        type SetWinLoc = unsafe extern "C" fn(*const CGEvent, CGPoint);
        for (f, v) in [
            (1u32, click_state),
            (3, button_num),
            (7, 3),
            (51, t.wid as i64),
            (58, 424242),
            (91, t.wid as i64),
            (92, t.wid as i64),
            (40, t.pid as i64),
        ] {
            CGEvent::set_integer_value_field(Some(ev), CGEventField(f), v);
        }
        // a HIDSystemState source can mix in modifiers the user is holding, so always overwrite them
        // explicitly
        CGEvent::set_flags(Some(ev), input::mods_to_flags(mods));
        let setloc = skylight::sym("CGEventSetWindowLocation");
        if !setloc.is_null() {
            let f: SetWinLoc = unsafe { std::mem::transmute(setloc) };
            let local = CGPoint { x: g.x - t.win.x, y: g.y - t.win.y };
            unsafe { f(ev, local) };
        }
    }

    fn post_mouse(t: &Target, typ: CGEventType, g: CGPoint, b: MouseButton, click_state: i64, mods: i64) -> Result<()> {
        let (_, _, _, btn, num) = button_info(b);
        let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState);
        let ev = CGEvent::new_mouse_event(src.as_deref(), typ, g, btn).ok_or_else(|| anyhow!("마우스 이벤트 생성 실패"))?;
        stamp(&ev, t, g, click_state, num, mods);
        CGEvent::post_to_pid(t.pid, Some(&ev));
        Ok(())
    }

    fn post_scroll(t: &Target, g: CGPoint, dx: f64, dy: f64, mods: i64) -> Result<()> {
        let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState);
        let ev = CGEvent::new_scroll_wheel_event2(src.as_deref(), CGScrollEventUnit::Pixel, 2, (-dy) as i32, (-dx) as i32, 0)
            .ok_or_else(|| anyhow!("스크롤 이벤트 생성 실패"))?;
        CGEvent::set_location(Some(&ev), g);
        stamp(&ev, t, g, 0, 0, mods);
        CGEvent::post_to_pid(t.pid, Some(&ev));
        Ok(())
    }

    // ---- capture ----

    struct ScFilter {
        filter: Retained<SCContentFilter>,
        at: Instant,
    }
    unsafe impl Send for ScFilter {}
    unsafe impl Sync for ScFilter {}

    fn ns_err(e: *mut NSError) -> String {
        if e.is_null() {
            return "알 수 없는 오류".into();
        }
        let e = unsafe { &*e };
        format!("{} (domain={} code={})", e.localizedDescription(), e.domain(), e.code())
    }

    fn sck_capture(filter: &SCContentFilter, w: u32, h: u32) -> Result<RgbImage> {
        let cfg = unsafe { SCStreamConfiguration::new() };
        unsafe {
            cfg.setWidth(w as usize);
            cfg.setHeight(h as usize);
            cfg.setShowsCursor(false);
        }
        let (tx, rx) = mpsc::channel::<Result<CFRetained<CGImage>, String>>();
        let block = RcBlock::new(move |img: *mut CGImage, e: *mut NSError| {
            let r = if !e.is_null() {
                Err(ns_err(e))
            } else {
                NonNull::new(img).map(|p| unsafe { CFRetained::retain(p) }).ok_or_else(|| "빈 이미지".to_string())
            };
            let _ = tx.send(r);
        });
        unsafe { SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(filter, &cfg, Some(&block)) };
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(img)) => capture::cgimage_to_rgb(&img),
            Ok(Err(e)) => bail!("가상 디스플레이 캡처 실패: {e}"),
            Err(_) => bail!("가상 디스플레이 캡처가 10초 안에 끝나지 않았습니다"),
        }
    }

    // ---- workspace ----

    #[derive(Clone)]
    struct WinEntry {
        wid: u32,
        title: String,
        subrole: String,
        frame: Rect,
        el: El,
    }

    struct Down {
        t: Target,
        button: MouseButton,
        count: i64,
    }

    #[derive(Default)]
    struct State {
        /// Last pointer position in agent coordinates.
        mouse: (f64, f64),
        down: Option<Down>,
        /// Key input target (pid, wid).
        key_target: Option<(i32, u32)>,
        last_records: Option<((i32, u32), Instant)>,
        windows: Vec<WinEntry>,
        notes: Vec<String>,
        ui_ids: Vec<El>,
        /// Why the app was hidden (until recovered).
        hidden_reason: Option<String>,
        recoveries: u32,
        focus_steals: u32,
        /// Mode B: the user activated the app themselves, so input stops.
        user_active: bool,
    }

    struct Inner {
        pid: i32,
        bundle_id: Option<String>,
        app_name: String,
        mode: VdMode,
        prior_hidden: bool,
        opts: VirtualDisplayOptions,
        agent: (u32, u32),
        vd: Mutex<Option<VirtualDisplay>>,
        vd_id: u32,
        vd_bounds: Mutex<Rect>,
        user_before: Vec<WinSnap>,
        original_frames: Mutex<Vec<(El, Rect)>>,
        state: Mutex<State>,
        filter: Mutex<Option<ScFilter>>,
        guard: Mutex<Option<ExposureGuard>>,
        closed: AtomicBool,
        shut: AtomicBool,
        sink: FrameSink,
        report: Mutex<Vec<String>>,
        watchdog_stop: Arc<AtomicBool>,
        watchdog: Mutex<Option<std::thread::JoinHandle<()>>>,
        /// Runs input sequences (clicks, drags, keys) one at a time.
        input_lock: Mutex<()>,
    }

    /// macOS app workspace on a virtual display.
    pub struct VirtualDisplayDesk {
        inner: Arc<Inner>,
    }

    /// Default arguments that turn off state restoration/saving and make apps start with an empty
    /// document instead of an open panel (same as native.rs).
    const DEFAULT_ARGS: [&str; 6] = [
        "-ApplePersistenceIgnoreState",
        "YES",
        "-ApplePersistence",
        "NO",
        "-NSShowAppCentricOpenPanelInsteadOfUntitledFile",
        "NO",
    ];

    struct StartResult {
        pid: i32,
        bundle_id: Option<String>,
        mode: VdMode,
        prior_hidden: bool,
        name: String,
        exposure: Vec<String>,
        guard: Option<ExposureGuard>,
    }

    /// Mode A: launches a new hidden, inactive instance. Mode B: attaches to a running app with
    /// consent and hides it.
    fn start_app(o: &VirtualDisplayOptions, target: &apps::AppTarget) -> Result<StartResult> {
        if let Some(p) = o.attach_pid {
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
            None => apps::running_pids_for(target),
        };
        if o.attach_pid.is_some() || (o.attach_consent && !running.is_empty()) {
            if !o.attach_consent {
                bail!("사용자의 실행 중인 앱에 붙으려면 동의(attach_consent)가 필요합니다");
            }
            let pid = running[0];
            let prior_hidden = apps::is_hidden(pid).ok_or_else(|| anyhow!("pid {pid} 앱을 찾지 못했습니다"))?;
            if !prior_hidden && !apps::hide(pid) {
                sleep_ms(300);
                if apps::is_hidden(pid) != Some(true) {
                    bail!("{} 을(를) 숨기지 못했습니다", target.name);
                }
            }
            eprintln!("[ghostdesk] 가상 디스플레이: 사용자 앱에 연결 (Mode B) {} pid={pid} 원래 숨김={prior_hidden}", target.name);
            return Ok(StartResult {
                pid,
                bundle_id: target.bundle_id.clone(),
                mode: VdMode::Attached,
                prior_hidden,
                name: target.name.clone(),
                exposure: vec![],
                guard: None,
            });
        }
        if !running.is_empty() && apps::multiple_instances_prohibited(&target.path) {
            bail!(
                "{} 은(는) 여러 인스턴스 실행을 허용하지 않는 앱이라 별도 인스턴스를 띄울 수 없습니다. \
                 사용자의 실행 중인 앱에서 작업하려면 attach 동의가 필요합니다.",
                target.name
            );
        }
        let prior_visible: Vec<(i32, bool)> = running.iter().map(|p| (*p, apps::is_hidden(*p) == Some(false))).collect();
        let mut args: Vec<String> = DEFAULT_ARGS.iter().map(|s| s.to_string()).collect();
        args.extend(o.extra_args.iter().cloned());
        let mut before: HashSet<i32> = apps::all_running_pids();
        before.extend(running.iter().copied());
        let t0 = Instant::now();
        let pid = apps::launch_hidden(&target.path, &o.open_files, &args, !running.is_empty())?;
        if before.contains(&pid) {
            if prior_visible.iter().any(|(p, vis)| *p == pid && *vis) && apps::is_hidden(pid) == Some(true) {
                apps::restore_unhide(pid);
            }
            bail!(
                "{} 이(가) 새 인스턴스를 만들지 않고 기존 프로세스(pid {pid})를 돌려주었습니다. \
                 사용자의 앱에서 작업하려면 attach 동의가 필요합니다.",
                target.name
            );
        }
        // keep it hidden until its windows are moved to the virtual display
        let guard = ExposureGuard::start(pid);
        let mut exposure = watch_exposure(pid, Duration::from_millis(2500), t0);
        exposure.extend(guard.take_events());
        if apps::is_hidden(pid) == Some(false) {
            apps::hide(pid);
        }
        eprintln!(
            "[ghostdesk] 가상 디스플레이: 숨김 실행 (Mode A) {} pid={pid} 노출={}",
            target.name,
            if exposure.is_empty() { "없음".to_string() } else { exposure.join(", ") }
        );
        Ok(StartResult {
            pid,
            bundle_id: target.bundle_id.clone().or_else(|| apps::bundle_id_of(pid)),
            mode: VdMode::Launched,
            prior_hidden: true,
            name: target.name.clone(),
            exposure,
            guard: Some(guard),
        })
    }

    fn watch_exposure(pid: i32, dur: Duration, t0: Instant) -> Vec<String> {
        let start = Instant::now();
        let mut out = Vec::new();
        let mut since: Option<Instant> = None;
        while start.elapsed() < dur {
            if !apps::visible_windows(&[pid]).is_empty() {
                since.get_or_insert_with(Instant::now);
                apps::hide(pid);
            } else if let Some(s) = since.take() {
                out.push(format!("실행 {}ms 후 약 {}ms", (s - t0).as_millis(), s.elapsed().as_millis().max(5)));
            }
            sleep_ms(5);
        }
        if let Some(s) = since {
            out.push(format!("실행 {}ms 후 {}ms 이상 (계속 보임)", (s - t0).as_millis(), s.elapsed().as_millis()));
        }
        out
    }

    fn start(opts: VirtualDisplayOptions, sink: FrameSink) -> Result<Arc<Inner>> {
        available()?;
        let agent = (opts.width.clamp(640, 3840), opts.height.clamp(480, 2160));
        let target = apps::resolve_app(&opts.app)?;
        let me = std::process::id() as i32;
        let user_before = snapshot_user_windows(&[me]);
        let (cw, ch) = if opts.hidpi { (agent.0 * 2, agent.1 * 2) } else { agent };
        let vd = VirtualDisplay::create(cw, ch, opts.hidpi, &format!("GhostDesk Agent ({})", target.name))?;
        let mut first_notes = vec![];
        let moved = geom::diff_windows(&user_before, &snapshot_user_windows(&[me]), 1.0);
        if !moved.is_empty() {
            let m = format!("가상 디스플레이를 만든 뒤 사용자 창 위치가 바뀌었습니다: {}", moved.join(" / "));
            eprintln!("[ghostdesk] 경고: {m}");
            first_notes.push(m);
        }
        let st = start_app(&opts, &target)?; // on failure vd is dropped → display removed
        let vd_id = vd.display_id();
        let bounds = vd.bounds();
        let mut report: Vec<String> = vd.notes.clone();
        report.extend(first_notes.iter().cloned());
        let inner = Arc::new(Inner {
            pid: st.pid,
            bundle_id: st.bundle_id,
            app_name: st.name,
            mode: st.mode,
            prior_hidden: st.prior_hidden,
            opts,
            agent,
            vd: Mutex::new(Some(vd)),
            vd_id,
            vd_bounds: Mutex::new(bounds),
            user_before,
            original_frames: Mutex::new(vec![]),
            state: Mutex::new(State { mouse: (agent.0 as f64 / 2.0, agent.1 as f64 / 2.0), ..Default::default() }),
            filter: Mutex::new(None),
            guard: Mutex::new(st.guard),
            closed: AtomicBool::new(false),
            shut: AtomicBool::new(false),
            sink,
            report: Mutex::new(report),
            watchdog_stop: Arc::new(AtomicBool::new(false)),
            watchdog: Mutex::new(None),
            input_lock: Mutex::new(()),
        });
        // later errors are cleaned up by inner drop → shutdown_sync (process ended/restored + display
        // removed)
        for n in first_notes {
            inner.note(n);
        }
        if !st.exposure.is_empty() {
            inner.note(format!("실행 중 창이 잠깐 사용자 화면에 노출되어 즉시 숨겼습니다: {}", st.exposure.join(", ")));
        }
        inner.prepare_windows(true)?;
        inner.show()?;
        inner.refresh_windows(false);
        let weak = Arc::downgrade(&inner);
        let stop = inner.watchdog_stop.clone();
        let h = std::thread::Builder::new()
            .name(format!("ghostdesk-vdisplay-watch-{}", inner.pid))
            .spawn(move || watchdog(weak, stop))
            .map_err(|e| anyhow!("감시 스레드를 만들지 못했습니다: {e}"))?;
        *inner.watchdog.lock() = Some(h);
        eprintln!(
            "[ghostdesk] 가상 디스플레이 작업공간 준비: {} pid={} 디스플레이={} 영역=({:.0},{:.0} {:.0}x{:.0})",
            inner.app_name, inner.pid, vd_id, bounds.x, bounds.y, bounds.w, bounds.h
        );
        Ok(inner)
    }

    /// Watchdog: every 50 ms brings back windows that went onto user displays, every 250 ms checks
    /// state (display, front app, process).
    fn watchdog(weak: std::sync::Weak<Inner>, stop: Arc<AtomicBool>) {
        let mut tick = 0u64;
        while !stop.load(Ordering::SeqCst) {
            sleep_ms(50);
            tick += 1;
            let Some(i) = weak.upgrade() else { break };
            if i.shut.load(Ordering::SeqCst) {
                break;
            }
            // this thread lives as long as the workspace, so drain the autorelease pool every tick
            objc2::rc::autoreleasepool(|_| {
                i.fix_strays("감시기");
                if tick.is_multiple_of(5) {
                    i.check_health();
                }
                if tick.is_multiple_of(20) {
                    i.refresh_windows(true);
                }
            });
            drop(i);
        }
    }

    impl VirtualDisplayDesk {
        /// TCC check → create the virtual display → launch the app hidden (or attach with consent) → move
        /// its windows to the virtual display → show without activating → start the watchdog.
        pub async fn launch(opts: VirtualDisplayOptions, sink: FrameSink) -> Result<Arc<VirtualDisplayDesk>> {
            crate::macos::preflight()?;
            let fps = opts.live_fps;
            let inner = blocking(move || start(opts, sink)).await??;
            if fps > 0.0 {
                tokio::spawn(live_loop(Arc::downgrade(&inner), fps));
            }
            Ok(Arc::new(VirtualDisplayDesk { inner }))
        }

        pub fn pid(&self) -> i32 {
            self.inner.pid
        }
        pub fn mode(&self) -> VdMode {
            self.inner.mode
        }
        pub fn app_name(&self) -> String {
            self.inner.app_name.clone()
        }
        pub fn display_id(&self) -> u32 {
            self.inner.vd_id
        }
        /// Virtual display region (global points).
        pub fn display_bounds(&self) -> geom::Rect {
            *self.inner.vd_bounds.lock()
        }
        /// Records from creation and shutdown (including the user window comparison).
        pub fn report(&self) -> Vec<String> {
            self.inner.report.lock().clone()
        }

        pub fn size(&self) -> (u32, u32) {
            self.inner.agent
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

        pub async fn mouse_move(&self, x: f64, y: f64, mods: i64) -> Result<()> {
            let i = self.inner.clone();
            blocking(move || i.mouse_move(x, y, mods)).await?
        }

        pub async fn mouse_down(&self, b: MouseButton, count: i64, mods: i64) -> Result<()> {
            let i = self.inner.clone();
            blocking(move || i.mouse_down(b, count, mods)).await?
        }

        pub async fn mouse_up(&self, b: MouseButton, count: i64, mods: i64) -> Result<()> {
            let i = self.inner.clone();
            blocking(move || i.mouse_up(b, count, mods)).await?
        }

        pub async fn click(&self, pos: Option<(f64, f64)>, b: MouseButton, count: u32, mods: i64) -> Result<()> {
            let i = self.inner.clone();
            blocking(move || i.click(pos, b, count.max(1), mods)).await?
        }

        pub async fn drag(&self, from: (f64, f64), to: (f64, f64), mods: i64) -> Result<()> {
            let i = self.inner.clone();
            blocking(move || i.drag(from, to, mods)).await?
        }

        pub async fn scroll(&self, pos: Option<(f64, f64)>, dx: f64, dy: f64, mods: i64) -> Result<()> {
            let i = self.inner.clone();
            blocking(move || i.scroll(pos, dx, dy, mods)).await?
        }

        /// In the mode that uses the user's running app (Attached), blocks app quit and window close
        /// shortcuts.
        fn guard_quit(&self, combo: &str) -> Result<()> {
            if self.mode() == VdMode::Attached {
                let c = combo.to_ascii_lowercase().replace(' ', "");
                let has_cmd = c.split('+').any(|p| matches!(p, "cmd" | "command" | "super" | "meta"));
                let last = c.rsplit('+').next().unwrap_or("");
                if has_cmd && (last == "q" || last == "w") {
                    bail!("사용자의 실행 중인 앱이라 종료(cmd+Q)·창 닫기(cmd+W)는 막혀 있습니다");
                }
            }
            Ok(())
        }

        pub async fn key(&self, combo: &str, repeat: u32) -> Result<()> {
            self.guard_quit(combo)?;
            let i = self.inner.clone();
            let combo = combo.to_string();
            blocking(move || i.key(&combo, repeat.max(1), Duration::from_millis(15))).await?
        }

        pub async fn hold_key(&self, combo: &str, secs: f64) -> Result<()> {
            self.guard_quit(combo)?;
            let i = self.inner.clone();
            let combo = combo.to_string();
            let hold = Duration::from_secs_f64(secs.clamp(0.0, 10.0));
            blocking(move || i.key(&combo, 1, hold)).await?
        }

        pub async fn type_text(&self, text: &str) -> Result<()> {
            let i = self.inner.clone();
            let text = text.to_string();
            blocking(move || i.type_text(&text)).await?
        }

        pub fn mouse_pos(&self) -> (f64, f64) {
            self.inner.state.lock().mouse
        }

        /// Window list (refreshed every second by the watchdog; no IPC here).
        pub fn views(&self) -> (Vec<ViewInfo>, Option<String>) {
            let st = self.inner.state.lock();
            let b = *self.inner.vd_bounds.lock();
            let v = st
                .windows
                .iter()
                .map(|w| {
                    let a = geom::rect_to_agent(&b, self.inner.agent, &w.frame);
                    ViewInfo {
                        id: w.wid.to_string(),
                        title: if w.title.is_empty() { "(제목 없음)".into() } else { w.title.clone() },
                        detail: format!(
                            "{} [{},{},{},{}] (가상 디스플레이)",
                            if w.subrole.is_empty() { "AXWindow" } else { &w.subrole },
                            a[0],
                            a[1],
                            a[2],
                            a[3]
                        ),
                    }
                })
                .collect();
            (v, st.key_target.map(|(_, w)| w.to_string()))
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

        /// LLM `app_control` tool (list_windows / switch_window / ui_tree / press / set_value / focus /
        /// read / menus / menu).
        pub async fn app_control(&self, input: &Value) -> Result<String> {
            let i = self.inner.clone();
            let input = input.clone();
            blocking(move || i.app_control(&input)).await?
        }
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            self.shutdown_sync();
        }
    }

    /// If dropped without shutdown() (which can happen in an async context), cleanup runs on a
    /// separate thread, so waiting for the app to exit and the display to go away (several seconds)
    /// doesn't block a tokio worker.
    impl Drop for VirtualDisplayDesk {
        fn drop(&mut self) {
            if self.inner.shut.load(Ordering::SeqCst) {
                return;
            }
            let i = self.inner.clone();
            let _ = std::thread::Builder::new()
                .name("ghostdesk-vdisplay-cleanup".into())
                .spawn(move || objc2::rc::autoreleasepool(|_| i.shutdown_sync()));
        }
    }

    impl Inner {
        fn note(&self, s: impl Into<String>) {
            let s = s.into();
            if !s.is_empty() {
                self.state.lock().notes.push(s);
            }
        }

        fn log(&self, s: impl Into<String>) {
            let s = s.into();
            eprintln!("[ghostdesk] 가상 디스플레이: {s}");
            self.report.lock().push(s.clone());
            self.note(s);
        }

        fn app(&self) -> El {
            El::app(self.pid)
        }

        fn bounds(&self) -> Rect {
            *self.vd_bounds.lock()
        }

        /// Refreshes the AX window list (announce = leave notes for new/closed windows).
        fn refresh_windows(&self, announce: bool) {
            let mut list = vec![];
            for w in self.app().elements("AXWindows") {
                let Some(wid) = w.window_id() else { continue };
                let n = Node::read(&w);
                let Some(frame) = n.frame else { continue };
                if frame.w < 40.0 || frame.h < 30.0 {
                    continue;
                }
                list.push(WinEntry { wid, title: n.title.clone(), subrole: n.subrole.clone(), frame, el: w });
            }
            let mut st = self.state.lock();
            if announce && !st.windows.is_empty() {
                let old: HashSet<u32> = st.windows.iter().map(|w| w.wid).collect();
                let mut new_notes = vec![];
                for w in &list {
                    if !old.contains(&w.wid) {
                        new_notes.push(format!("새 창이 열렸습니다: '{}'", w.title));
                    }
                }
                for w in &st.windows {
                    if !list.iter().any(|n| n.wid == w.wid) {
                        new_notes.push(format!("창이 닫혔습니다: '{}'", w.title));
                    }
                }
                st.notes.extend(new_notes);
            }
            st.windows = list;
        }

        /// Moves all of the app's AX windows onto the virtual display (while hidden). first = initial
        /// preparation (waits for windows to appear).
        fn prepare_windows(&self, first: bool) -> Result<()> {
            let t = Instant::now();
            loop {
                self.refresh_windows(false);
                if !self.state.lock().windows.is_empty() || !first || t.elapsed() > Duration::from_secs(10) {
                    break;
                }
                sleep_ms(150);
            }
            let wins = self.state.lock().windows.clone();
            if wins.is_empty() {
                bail!("{} 의 창을 10초 안에 찾지 못했습니다", self.app_name);
            }
            let vdb = self.bounds();
            if first && self.mode == VdMode::Attached {
                *self.original_frames.lock() = wins.iter().map(|w| (w.el.clone(), w.frame)).collect();
            }
            let mut fails = vec![];
            for w in &wins {
                let want = geom::fit_window(&w.frame, &vdb, TOP_INSET);
                let got = if want.approx_eq(&w.frame, 0.5) { Some(w.frame) } else { set_frame(&w.el, want) };
                match got {
                    Some(f) if f.inside(&vdb, 2.0) => {}
                    other => fails.push(format!("'{}' → {:?}", w.title, other)),
                }
            }
            if !fails.is_empty() {
                bail!(
                    "창을 가상 디스플레이로 옮기지 못했습니다 (앱은 숨긴 상태로 둡니다): {}",
                    fails.join(", ")
                );
            }
            self.refresh_windows(false);
            Ok(())
        }

        /// Our currently visible windows that overlap a user display.
        fn strays(&self) -> Vec<apps::CgWin> {
            let others = other_display_rects(self.vd_id);
            apps::visible_windows(&[self.pid]).into_iter().filter(|w| geom::is_stray(&r(w.bounds), &others)).collect()
        }

        /// Shows the app without activating it and checks every window is only on the virtual display.
        fn show(&self) -> Result<()> {
            if let Some(mut g) = self.guard.lock().take() {
                g.stop();
                let ev = g.take_events();
                if !ev.is_empty() {
                    self.log(format!("숨김 상태 감시 중 노출: {}", ev.join(" / ")));
                }
            }
            let front_before = skylight::front_pid();
            if apps::is_hidden(self.pid) == Some(true) {
                let e = self.app().set_bool("AXHidden", false);
                let t = Instant::now();
                while apps::is_hidden(self.pid) == Some(true) && t.elapsed() < Duration::from_millis(1000) {
                    sleep_ms(10);
                }
                if apps::is_hidden(self.pid) == Some(true) {
                    eprintln!("[ghostdesk] AXHidden=false 가 반영되지 않음 ({}) → NSRunningApplication.unhide", ax::err_name(e));
                    apps::restore_unhide(self.pid);
                    let t = Instant::now();
                    while apps::is_hidden(self.pid) == Some(true) && t.elapsed() < Duration::from_millis(1000) {
                        sleep_ms(10);
                    }
                }
                if apps::is_hidden(self.pid) == Some(true) {
                    bail!("{} 을(를) 표시하지 못했습니다", self.app_name);
                }
            }
            sleep_ms(60);
            let f = skylight::front_pid();
            if f == self.pid && front_before != self.pid {
                apps::hide(self.pid);
                bail!(
                    "{} 을(를) 표시하자 앱이 활성화되어(사용자 키보드 포커스 이동) 즉시 다시 숨겼습니다 — 이 앱은 가상 디스플레이 모드를 쓸 수 없습니다",
                    self.app_name
                );
            }
            // wait for the windows to come on screen and check none went onto a user display
            for attempt in 0..2 {
                let t = Instant::now();
                while apps::visible_windows(&[self.pid]).is_empty() && t.elapsed() < Duration::from_millis(1500) {
                    sleep_ms(15);
                }
                let strays = self.strays();
                if strays.is_empty() {
                    let n = apps::visible_windows(&[self.pid]).len();
                    if n == 0 {
                        bail!("{} 을(를) 표시했지만 화면에 올라온 창이 없습니다", self.app_name);
                    }
                    self.state.lock().hidden_reason = None;
                    return Ok(());
                }
                apps::hide(self.pid);
                self.log(format!(
                    "표시 직후 창 {}개가 사용자 디스플레이에 나타나 즉시 숨겼습니다 (시도 {})",
                    strays.len(),
                    attempt + 1
                ));
                sleep_ms(100);
                self.prepare_windows(false)?;
                let _ = self.app().set_bool("AXHidden", false);
                sleep_ms(100);
            }
            apps::hide(self.pid);
            bail!("{} 의 창을 가상 디스플레이에만 두지 못해 숨겼습니다", self.app_name)
        }

        /// Moves our windows overlapping a user display onto the virtual display, hiding the app if that
        /// fails. Returns whether there was no problem or it was resolved.
        fn fix_strays(&self, ctx: &str) -> bool {
            if self.shut.load(Ordering::SeqCst) {
                return true;
            }
            // in Mode B, once the user starts using the app themselves, don't take back windows they moved or
            // hide the app
            if self.mode == VdMode::Attached && self.state.lock().user_active {
                return true;
            }
            let strays = self.strays();
            if strays.is_empty() {
                return true;
            }
            let t = Instant::now();
            let vdb = self.bounds();
            let mut moved = 0;
            for w in self.app().elements("AXWindows") {
                let Some(wid) = w.window_id() else { continue };
                if let Some(s) = strays.iter().find(|s| s.wid == wid) {
                    let want = geom::fit_window(&r(s.bounds), &vdb, TOP_INSET);
                    set_frame(&w, want);
                    moved += 1;
                }
            }
            sleep_ms(30);
            let remain = self.strays();
            if remain.is_empty() {
                self.log(format!(
                    "{ctx}: 앱 창 {}개가 사용자 디스플레이에 나타나 가상 디스플레이로 옮겼습니다 (약 {}ms)",
                    moved.max(strays.len()),
                    t.elapsed().as_millis()
                ));
                return true;
            }
            apps::hide(self.pid);
            let t2 = Instant::now();
            while !apps::visible_windows(&[self.pid]).is_empty() && t2.elapsed() < Duration::from_millis(300) {
                sleep_ms(5);
            }
            let reason = format!(
                "{ctx}: 옮길 수 없는 창 {}개(메뉴·팝업 등)가 사용자 디스플레이에 나타나 앱을 숨겼습니다 (약 {}ms)",
                remain.len(),
                t.elapsed().as_millis()
            );
            self.state.lock().hidden_reason = Some(reason.clone());
            self.log(reason);
            false
        }

        /// Checks display, front app and process state.
        fn check_health(&self) {
            if self.shut.load(Ordering::SeqCst) {
                return;
            }
            if !apps::is_running(self.pid) {
                self.closed.store(true, Ordering::SeqCst);
                return;
            }
            if !active_displays().contains(&self.vd_id) {
                apps::hide(self.pid);
                if !self.closed.swap(true, Ordering::SeqCst) {
                    self.log("가상 디스플레이가 사라져 앱을 숨기고 작업을 멈췄습니다");
                }
                return;
            }
            let now = display_rect(self.vd_id);
            let changed = {
                let mut b = self.vd_bounds.lock();
                let c = !now.approx_eq(&b, 0.5);
                *b = now;
                c
            };
            if changed {
                *self.filter.lock() = None;
                self.log(format!("가상 디스플레이 위치가 바뀌었습니다 → ({:.0},{:.0})", now.x, now.y));
            }
            if CGMainDisplayID() == self.vd_id {
                self.log("경고: 가상 디스플레이가 주 디스플레이가 되었습니다 (사용자가 바꿨을 수 있음). 작업을 멈추고 확인하세요");
                self.closed.store(true, Ordering::SeqCst);
            }
            // if our app becomes frontmost (active), the user's keyboard input goes to it
            if skylight::front_pid() == self.pid {
                match self.mode {
                    VdMode::Launched => {
                        apps::hide(self.pid);
                        let n = {
                            let mut st = self.state.lock();
                            st.focus_steals += 1;
                            st.hidden_reason = Some("앱이 활성화(front)되어 숨겼습니다".into());
                            st.focus_steals
                        };
                        self.log(format!(
                            "앱이 활성화(front)되어 사용자 키보드 포커스를 가져가지 않도록 즉시 숨겼습니다 ({n}회째). \
                             사용자가 가상 디스플레이의 창을 클릭했거나 앱이 스스로 활성화했을 수 있습니다"
                        ));
                    }
                    VdMode::Attached => {
                        let mut st = self.state.lock();
                        if !st.user_active {
                            st.user_active = true;
                            st.notes.push(
                                "사용자가 앱을 직접 활성화했습니다. 사용자가 쓰는 중일 수 있으니 입력을 멈췄습니다 — ask_user 로 확인하세요."
                                    .into(),
                            );
                        }
                    }
                }
            }
        }

        /// Only checks whether input is possible, without recovery (for hover, window switching and
        /// state-changing app_control actions).
        fn check_open(&self) -> Result<()> {
            if self.shut.load(Ordering::SeqCst) || self.closed.load(Ordering::SeqCst) {
                bail!("작업공간이 닫혔습니다");
            }
            if self.state.lock().user_active {
                bail!("사용자가 앱을 직접 사용 중이라 입력을 보내지 않았습니다 — ask_user 로 확인하세요");
            }
            Ok(())
        }

        /// Check before pixel input: if hidden, shows it on the virtual display again (up to 3 times).
        fn ensure_input(&self) -> Result<()> {
            if self.shut.load(Ordering::SeqCst) || self.closed.load(Ordering::SeqCst) {
                bail!("작업공간이 닫혔습니다");
            }
            if self.state.lock().user_active {
                bail!("사용자가 앱을 직접 사용 중이라 입력을 보내지 않았습니다 — ask_user 로 확인하세요");
            }
            if apps::is_hidden(self.pid) == Some(true) {
                let reason = {
                    let mut st = self.state.lock();
                    if st.recoveries >= 3 || st.focus_steals >= 3 {
                        bail!(
                            "앱이 숨겨진 상태입니다 ({}). 반복되어 다시 표시하지 않습니다 — ask_user 로 사용자에게 알리세요",
                            st.hidden_reason.clone().unwrap_or_default()
                        );
                    }
                    st.recoveries += 1;
                    st.hidden_reason.clone().unwrap_or_default()
                };
                self.prepare_windows(false)?;
                self.show()?;
                self.note(format!("숨겨졌던 앱을 다시 가상 디스플레이에 표시했습니다 (이전 사유: {reason})"));
            }
            Ok(())
        }

        /// Watches closely right after input for new windows appearing on a user display.
        fn settle(&self, dur: Duration) {
            let t = Instant::now();
            while t.elapsed() < dur {
                if !self.fix_strays("입력 직후") {
                    break;
                }
                sleep_ms(10);
            }
        }

        /// Agent coordinates → our window that receives the event (CGWindowList front to back). Errors if
        /// someone else's window covers it.
        fn hit(&self, x: f64, y: f64) -> Result<(Target, CGPoint)> {
            let b = self.bounds();
            let (gx, gy) = geom::agent_to_global(&b, self.agent, x, y);
            let mut blocker: Option<apps::CgWin> = None;
            for w in apps::cg_windows(&[], true) {
                if !w.visible() || w.layer < 0 {
                    continue;
                }
                let wr = r(w.bounds);
                if !wr.contains(gx, gy) {
                    continue;
                }
                if w.pid == self.pid {
                    if let Some(bw) = blocker {
                        bail!(
                            "{NO_WINDOW} — 다른 프로그램의 창('{}', pid {}, layer {})이 이 지점을 덮고 있습니다",
                            bw.name,
                            bw.pid,
                            bw.layer
                        );
                    }
                    return Ok((Target { pid: w.pid, wid: w.wid, win: wr }, CGPoint { x: gx, y: gy }));
                }
                // ignore system overlays covering the whole display; record any other foreign window as a blocker
                if blocker.is_none() && !geom::covers_display(&wr, &b) {
                    blocker = Some(w);
                }
            }
            match blocker {
                Some(bw) => bail!("{NO_WINDOW} — 이 지점은 다른 프로그램의 창('{}', pid {})입니다", bw.name, bw.pid),
                None => bail!("{NO_WINDOW} (가상 디스플레이의 빈 영역 ({x:.0}, {y:.0}))"),
            }
        }

        /// Errors if the element under the point affects the user's screen (links, minimize, full
        /// screen).
        fn check_element(&self, g: CGPoint) -> Result<()> {
            let Some(el) = self.app().element_at(g.x as f32, g.y as f32) else { return Ok(()) };
            let mut cur = Some(el);
            for _ in 0..4 {
                let Some(e) = cur else { break };
                let role = e.role();
                let sub = e.string("AXSubrole").unwrap_or_default();
                if let Some(reason) = element_block_reason(&role, &sub) {
                    bail!("{reason}");
                }
                if matches!(role.as_str(), "AXWindow" | "AXApplication") {
                    break;
                }
                cur = e.parent();
            }
            Ok(())
        }

        /// Key window record for the target PSN only (skipped if sent to the same window within 1.5 s).
        fn key_records(&self, t: &Target, force: bool) {
            {
                let st = self.state.lock();
                if let Some((k, at)) = st.last_records {
                    if !force && k == (t.pid, t.wid) && at.elapsed() < Duration::from_millis(1500) {
                        return;
                    }
                }
            }
            if let Err(e) = skylight::post_key_window_records(t.pid, t.wid) {
                eprintln!("[ghostdesk] 키 창 레코드 실패: {e}");
            }
            let mut st = self.state.lock();
            st.last_records = Some(((t.pid, t.wid), Instant::now()));
            st.key_target = Some((t.pid, t.wid));
        }

        /// Key input target window: the last clicked/switched window (if still visible), else our
        /// frontmost window.
        fn key_target(&self) -> Result<Target> {
            let wins: Vec<apps::CgWin> = apps::visible_windows(&[self.pid]).into_iter().filter(|w| w.layer == 0).collect();
            let want = self.state.lock().key_target;
            if let Some((_, wid)) = want {
                if let Some(w) = wins.iter().find(|w| w.wid == wid) {
                    return Ok(Target { pid: self.pid, wid, win: r(w.bounds) });
                }
            }
            let w = wins.first().ok_or_else(|| anyhow!("{NO_WINDOW} — 화면에 올라온 앱 창이 없습니다"))?;
            self.state.lock().key_target = Some((self.pid, w.wid));
            Ok(Target { pid: self.pid, wid: w.wid, win: r(w.bounds) })
        }

        fn primer(&self, t: &Target, g: CGPoint, mods: i64) -> Result<()> {
            post_mouse(t, CGEventType::MouseMoved, g, MouseButton::Left, 0, mods)?;
            sleep_ms(30);
            Ok(())
        }

        fn click(&self, pos: Option<(f64, f64)>, b: MouseButton, count: u32, mods: i64) -> Result<()> {
            let _g = self.input_lock.lock();
            self.ensure_input()?;
            let pos = pos.unwrap_or_else(|| self.state.lock().mouse);
            self.state.lock().mouse = pos;
            let (t, g) = self.hit(pos.0, pos.1)?;
            self.check_element(g)?;
            self.key_records(&t, true);
            self.primer(&t, g, mods)?;
            let (down, up, _, _, _) = button_info(b);
            for i in 1..=count.clamp(1, 3) as i64 {
                post_mouse(&t, down, g, b, i, mods)?;
                sleep_ms(28);
                post_mouse(&t, up, g, b, i, mods)?;
                sleep_ms(if i < count as i64 { 40 } else { 60 });
            }
            self.settle(Duration::from_millis(400));
            Ok(())
        }

        fn drag(&self, from: (f64, f64), to: (f64, f64), mods: i64) -> Result<()> {
            let _g = self.input_lock.lock();
            self.ensure_input()?;
            let (t, g0) = self.hit(from.0, from.1)?;
            self.check_element(g0)?;
            let b = self.bounds();
            let (gx1, gy1) = geom::agent_to_global(&b, self.agent, to.0, to.1);
            self.key_records(&t, true);
            self.primer(&t, g0, mods)?;
            post_mouse(&t, CGEventType::LeftMouseDown, g0, MouseButton::Left, 1, mods)?;
            sleep_ms(40);
            let steps = geom::drag_steps((g0.x, g0.y), (gx1, gy1));
            for (x, y) in geom::interp((g0.x, g0.y), (gx1, gy1), steps) {
                post_mouse(&t, CGEventType::LeftMouseDragged, CGPoint { x, y }, MouseButton::Left, 1, mods)?;
                sleep_ms(12);
            }
            sleep_ms(40);
            post_mouse(&t, CGEventType::LeftMouseUp, CGPoint { x: gx1, y: gy1 }, MouseButton::Left, 1, mods)?;
            self.state.lock().mouse = to;
            self.settle(Duration::from_millis(400));
            Ok(())
        }

        fn mouse_move(&self, x: f64, y: f64, mods: i64) -> Result<()> {
            let _g = self.input_lock.lock();
            self.check_open()?;
            let down_target = {
                let mut st = self.state.lock();
                st.mouse = (x, y);
                st.down.as_ref().map(|d| (d.t, d.button))
            };
            let b = self.bounds();
            let (gx, gy) = geom::agent_to_global(&b, self.agent, x, y);
            let g = CGPoint { x: gx, y: gy };
            match down_target {
                Some((t, button)) => {
                    let (_, _, dragged, _, _) = button_info(button);
                    post_mouse(&t, dragged, g, button, 1, mods)
                }
                None => {
                    // hover: send move events only over our windows (otherwise just record the position)
                    if let Ok((t, g)) = self.hit(x, y) {
                        post_mouse(&t, CGEventType::MouseMoved, g, MouseButton::Left, 0, mods)?;
                    }
                    Ok(())
                }
            }
        }

        fn mouse_down(&self, b: MouseButton, count: i64, mods: i64) -> Result<()> {
            let _g = self.input_lock.lock();
            self.ensure_input()?;
            let pos = self.state.lock().mouse;
            let (t, g) = self.hit(pos.0, pos.1)?;
            self.check_element(g)?;
            self.key_records(&t, true);
            self.primer(&t, g, mods)?;
            let (down, _, _, _, _) = button_info(b);
            post_mouse(&t, down, g, b, count.max(1), mods)?;
            self.state.lock().down = Some(Down { t, button: b, count: count.max(1) });
            Ok(())
        }

        fn mouse_up(&self, b: MouseButton, count: i64, mods: i64) -> Result<()> {
            let _g = self.input_lock.lock();
            let (down, pos) = {
                let mut st = self.state.lock();
                (st.down.take(), st.mouse)
            };
            let Some(d) = down else { bail!("mouse_down 없이 mouse_up 이 왔습니다") };
            let bnd = self.bounds();
            let (gx, gy) = geom::agent_to_global(&bnd, self.agent, pos.0, pos.1);
            let (_, up, _, _, _) = button_info(b);
            post_mouse(&d.t, up, CGPoint { x: gx, y: gy }, b, count.max(d.count).max(1), mods)?;
            self.settle(Duration::from_millis(300));
            Ok(())
        }

        fn scroll(&self, pos: Option<(f64, f64)>, dx: f64, dy: f64, mods: i64) -> Result<()> {
            let _g = self.input_lock.lock();
            self.ensure_input()?;
            let pos = pos.unwrap_or_else(|| self.state.lock().mouse);
            self.state.lock().mouse = pos;
            let (t, g) = self.hit(pos.0, pos.1)?;
            self.primer(&t, g, mods)?;
            let ys = geom::scroll_chunks(dy, 120.0);
            let xs = geom::scroll_chunks(dx, 120.0);
            for k in 0..ys.len().max(xs.len()) {
                post_scroll(&t, g, xs.get(k).copied().unwrap_or(0.0), ys.get(k).copied().unwrap_or(0.0), mods)?;
                sleep_ms(16);
            }
            Ok(())
        }

        fn key(&self, combo: &str, repeat: u32, hold: Duration) -> Result<()> {
            let _g = self.input_lock.lock();
            self.ensure_input()?;
            let c = input::parse_combo(combo)?;
            if let Some(reason) = system_combo_block(&c) {
                bail!("{combo}: {reason}");
            }
            if c.is_shortcut() {
                if let Some(ch) = c.ch {
                    if let Some((path, Some(reason))) = find_menu_by_shortcut(&self.app(), ch, c.mods) {
                        bail!("{combo} 는 메뉴 '{path}' 의 단축키입니다: {reason}");
                    }
                }
            }
            let t = self.key_target()?;
            self.key_records(&t, c.is_shortcut());
            for _ in 0..repeat.clamp(1, 100) {
                input::pid_combo(t.pid, &c, hold)?;
            }
            self.settle(Duration::from_millis(if c.is_shortcut() { 600 } else { 200 }));
            Ok(())
        }

        fn type_text(&self, text: &str) -> Result<()> {
            let _g = self.input_lock.lock();
            self.ensure_input()?;
            let t = self.key_target()?;
            self.key_records(&t, false);
            input::pid_type_text(t.pid, text)?;
            self.settle(Duration::from_millis(200));
            Ok(())
        }

        fn switch_window(&self, index: usize) -> Result<String> {
            self.check_open()?;
            self.refresh_windows(false);
            let w = {
                let st = self.state.lock();
                st.windows.get(index).cloned().ok_or_else(|| anyhow!("창 {index} 가 없습니다 (0..{})", st.windows.len()))?
            };
            let t = Target { pid: self.pid, wid: w.wid, win: w.frame };
            self.key_records(&t, true);
            Ok(format!("키 입력 대상 창을 '{}' 로 바꿨습니다 (창 순서는 바꾸지 않음)", w.title))
        }

        // ---- capture ----

        fn sc_filter(&self) -> Result<Retained<SCContentFilter>> {
            if let Some(f) = self.filter.lock().as_ref() {
                if f.at.elapsed() < Duration::from_secs(3) {
                    return Ok(f.filter.clone());
                }
            }
            let content = capture::shareable_content()?;
            let displays = unsafe { content.displays() };
            let d: Retained<SCDisplay> = displays
                .iter()
                .find(|d| unsafe { d.displayID() } == self.vd_id)
                .ok_or_else(|| anyhow!("가상 디스플레이 {} 를 ScreenCaptureKit 목록에서 찾지 못했습니다", self.vd_id))?;
            let apps_list = unsafe { content.applications() };
            let ours: Vec<Retained<SCRunningApplication>> =
                apps_list.iter().filter(|a| unsafe { a.processID() } == self.pid).collect();
            if ours.is_empty() {
                bail!("{} 이(가) 아직 ScreenCaptureKit 앱 목록에 없습니다", self.app_name);
            }
            let empty: Retained<NSArray<SCWindow>> = NSArray::new();
            let filter = unsafe {
                SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                    SCContentFilter::alloc(),
                    &d,
                    &NSArray::from_retained_slice(&ours),
                    &empty,
                )
            };
            *self.filter.lock() = Some(ScFilter { filter: filter.clone(), at: Instant::now() });
            Ok(filter)
        }

        fn capture(&self, w: u32, h: u32) -> Result<RgbImage> {
            let f = self.sc_filter()?;
            sck_capture(&f, w, h)
        }

        fn screenshot_png(&self) -> Result<Vec<u8>> {
            self.fix_strays("스크린샷 전");
            let img = self.capture(self.agent.0, self.agent.1)?;
            capture::encode_png(&img)
        }

        fn zoom_png(&self, region: [f64; 4]) -> Result<Vec<u8>> {
            let px = {
                let vd = self.vd.lock();
                vd.as_ref().map(|v| v.pixel_size()).ok_or_else(|| anyhow!("가상 디스플레이가 없습니다"))?
            };
            let full = self.capture(px.0.max(1), px.1.max(1))?;
            let (cx, cy, cw, ch) = geom::region_to_pixels(region, self.agent, full.dimensions());
            let crop = image::imageops::crop_imm(&full, cx, cy, cw, ch).to_image();
            let target = self.agent.0.max(640) as f64;
            let scale = (target / cw.max(ch) as f64).clamp(1.0, 8.0);
            let out = image::imageops::resize(
                &crop,
                ((cw as f64 * scale) as u32).max(1),
                ((ch as f64 * scale) as u32).max(1),
                image::imageops::FilterType::CatmullRom,
            );
            capture::encode_png(&out)
        }

        // ───────── app_control ─────────

        fn current_window(&self) -> Result<WinEntry> {
            self.refresh_windows(false);
            let t = self.key_target()?;
            let st = self.state.lock();
            st.windows
                .iter()
                .find(|w| w.wid == t.wid)
                .or_else(|| st.windows.first())
                .cloned()
                .ok_or_else(|| anyhow!("앱 창이 없습니다"))
        }

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
            if matches!(action, "press" | "set_value" | "focus" | "menu") {
                self.check_open()?;
            }
            match action {
                "list_windows" => {
                    self.refresh_windows(true);
                    let cur = self.state.lock().key_target.map(|k| k.1);
                    let b = self.bounds();
                    let st = self.state.lock();
                    let mut s = String::new();
                    for (i, w) in st.windows.iter().enumerate() {
                        let a = geom::rect_to_agent(&b, self.agent, &w.frame);
                        s.push_str(&format!(
                            "{} [{i}] '{}' {} [{},{},{},{}]\n",
                            if Some(w.wid) == cur { "▶" } else { " " },
                            w.title,
                            w.subrole,
                            a[0],
                            a[1],
                            a[2],
                            a[3]
                        ));
                    }
                    Ok(if s.is_empty() { "열린 창이 없습니다".into() } else { s })
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
                    if let Some(reason) = element_block_reason(&n.role, &n.subrole) {
                        bail!("{reason}");
                    }
                    if n.role == "AXMenuItem" {
                        bail!("메뉴 항목은 app_control menu {{path}} 로 실행하세요 (차단 규칙 적용)");
                    }
                    let e = el.perform("AXPress");
                    self.settle(Duration::from_millis(400));
                    Ok(format!("{} 누름 ({})", n.describe(), ax::err_name(e)))
                }
                "set_value" => {
                    let el = id()?;
                    let text = input["text"].as_str().ok_or_else(|| anyhow!("text 가 필요합니다"))?;
                    let n = Node::read(&el);
                    if !el.settable("AXValue") {
                        bail!("{} 의 값은 바꿀 수 없습니다", n.describe());
                    }
                    let e = el.set_string("AXValue", text);
                    sleep_ms(60);
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
                    sleep_ms(50);
                    let ok = el.bool("AXFocused").unwrap_or(false);
                    Ok(format!(
                        "{} 에 포커스 {}",
                        Node::read(&el).describe(),
                        if ok { "(확인됨)".to_string() } else { format!("(확인 안 됨: {})", ax::err_name(e)) }
                    ))
                }
                "read" => {
                    let el = id()?;
                    let n = Node::read(&el);
                    let mut s = format!(
                        "{}{}\n",
                        n.role,
                        if n.subrole.is_empty() { String::new() } else { format!("/{}", n.subrole) }
                    );
                    for (k, v) in [("title", &n.title), ("description", &n.desc), ("placeholder", &n.placeholder)] {
                        if !v.is_empty() {
                            s.push_str(&format!("{k}: {v}\n"));
                        }
                    }
                    if let Some(sel) = el.string("AXSelectedText") {
                        s.push_str(&format!("selected text: {sel}\n"));
                    }
                    if let Some(rg) = el.range("AXSelectedTextRange") {
                        s.push_str(&format!("selected range: {},{}\n", rg.location, rg.length));
                    }
                    if let Some(rg) = el.range("AXVisibleCharacterRange") {
                        s.push_str(&format!("visible range: {},{}\n", rg.location, rg.length));
                    }
                    s.push_str(&format!("value:\n{}", n.value));
                    Ok(s)
                }
                "menus" => self.menus(),
                "menu" => {
                    let path = input["path"].as_str().ok_or_else(|| anyhow!("path 가 필요합니다 (예: \"File > Save\")"))?;
                    let r = self.menu(path)?;
                    self.settle(Duration::from_millis(600));
                    Ok(r)
                }
                other => bail!("알 수 없는 app_control action: {other}"),
            }
        }

        fn ui_tree(&self, max_depth: usize, max_nodes: usize) -> Result<String> {
            let w = self.current_window()?;
            let b = self.bounds();
            let mut ids: Vec<El> = vec![];
            let mut lines = vec![];
            let mut visited = 0usize;
            let mut stack: Vec<(El, usize)> = vec![(w.el.clone(), 0)];
            while let Some((el, d)) = stack.pop() {
                visited += 1;
                if visited > 4000 || ids.len() >= max_nodes {
                    lines.push(format!("… (요소가 많아 {}개에서 멈춤)", ids.len()));
                    break;
                }
                let n = Node::read(&el);
                let visible = n.frame.map(|f| f.w > 0.0 && f.h > 0.0 && f.intersection_area(&w.frame) > 0.0).unwrap_or(false);
                let label = n.label();
                let boring = matches!(
                    n.role.as_str(),
                    "AXGroup" | "AXSplitGroup" | "AXScrollArea" | "AXLayoutArea" | "AXUnknown" | "AXSplitter" | "AXColumn" | "AXWindow" | ""
                ) && label.is_empty();
                if visible && !boring && d > 0 {
                    ids.push(el.clone());
                    let a = geom::rect_to_agent(&b, self.agent, &n.frame.unwrap());
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
                    line.push_str(&format!(" [{},{},{},{}]", a[0], a[1], a[2], a[3]));
                    if !n.enabled {
                        line.push_str(" disabled");
                    }
                    if n.focused {
                        line.push_str(" focused");
                    }
                    lines.push(line);
                }
                if d < max_depth && (visible || n.frame.is_none() || d == 0) {
                    for c in kids(&n).into_iter().rev() {
                        stack.push((c, d + 1));
                    }
                }
            }
            self.state.lock().ui_ids = ids;
            Ok(format!(
                "창 '{}' (스크린샷 {}x{} 픽셀 좌표):\n{}",
                w.title,
                self.agent.0,
                self.agent.1,
                lines.join("\n")
            ))
        }

        fn menu(&self, path: &str) -> Result<String> {
            let parts: Vec<String> = path.split('>').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            if parts.len() < 2 {
                bail!("메뉴 경로는 \"메뉴 > 항목\" 형식이어야 합니다 (app_control menus 로 확인)");
            }
            let bar = self.app().element("AXMenuBar").ok_or_else(|| anyhow!("앱의 메뉴 막대를 읽지 못했습니다"))?;
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
                            "'{}' 는 하위 메뉴입니다. 하위 항목까지 지정하세요: [{}]",
                            walked.join(" > "),
                            sub.iter().filter_map(|e| e.string("AXTitle")).take(30).collect::<Vec<_>>().join(", ")
                        );
                    }
                    if el.bool("AXEnabled") == Some(false) {
                        bail!("메뉴 '{}' 가 비활성화되어 있습니다", walked.join(" > "));
                    }
                    let e = el.perform("AXPress");
                    return Ok(format!("메뉴 '{}' 실행 ({}) — 결과를 스크린샷으로 확인하세요", walked.join(" > "), ax::err_name(e)));
                }
                items = sub;
                if items.is_empty() {
                    bail!("'{}' 의 하위 메뉴를 읽지 못했습니다", walked.join(" > "));
                }
            }
            unreachable!()
        }

        fn menus(&self) -> Result<String> {
            let bar = self.app().element("AXMenuBar").ok_or_else(|| anyhow!("앱의 메뉴 막대를 읽지 못했습니다"))?;
            let mut out = String::new();
            for top in menu_tops(&bar).iter().skip(1) {
                if top_block_reason(top).is_some() {
                    out.push_str(&format!("{}: [차단 — 사용자 화면에 영향]\n", top.title));
                    continue;
                }
                let items = submenu_items(&top.el);
                let mut shown = vec![];
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

        // ---- shutdown ----

        fn shutdown_sync(&self) {
            if self.shut.swap(true, Ordering::SeqCst) {
                return;
            }
            self.closed.store(true, Ordering::SeqCst);
            self.watchdog_stop.store(true, Ordering::SeqCst);
            if let Some(h) = self.watchdog.lock().take() {
                if h.thread().id() != std::thread::current().id() {
                    let _ = h.join();
                }
            }
            if let Some(mut g) = self.guard.lock().take() {
                g.stop();
            }
            let mut rep = vec![];
            match self.mode {
                VdMode::Launched => {
                    let now = apps::bundle_id_of(self.pid);
                    let same = match (&self.bundle_id, &now) {
                        (_, None) => !apps::is_running(self.pid) || self.bundle_id.is_none(),
                        (Some(want), Some(got)) => want == got,
                        (None, Some(_)) => true,
                    };
                    if same {
                        // hide first so windows don't jump onto a user display, then quit
                        apps::hide(self.pid);
                        apps::kill_process(self.pid);
                        rep.push(format!("앱 인스턴스 종료: pid={}", self.pid));
                    } else {
                        rep.push(format!("pid={} 가 다른 프로세스({now:?})로 바뀌어 종료하지 않음", self.pid));
                    }
                    let t = Instant::now();
                    while apps::is_running(self.pid) && t.elapsed() < Duration::from_secs(2) {
                        sleep_ms(20);
                    }
                }
                VdMode::Attached => {
                    apps::hide(self.pid);
                    sleep_ms(100);
                    let frames = std::mem::take(&mut *self.original_frames.lock());
                    let mut bad = 0;
                    for (el, f) in &frames {
                        match set_frame(el, *f) {
                            Some(now) if now.approx_eq(f, 2.0) => {}
                            _ => bad += 1,
                        }
                    }
                    rep.push(format!("사용자 앱 창 {}개 원래 위치로 복원 (실패 {bad})", frames.len()));
                    let user_active = self.state.lock().user_active;
                    if !self.prior_hidden && !user_active {
                        apps::restore_unhide(self.pid);
                        rep.push("사용자 앱을 원래대로 다시 표시 (활성화하지 않음)".into());
                    }
                }
            }
            if let Some(vd) = self.vd.lock().take() {
                rep.extend(vd.destroy(&[self.pid]));
            }
            let me = std::process::id() as i32;
            let exclude: Vec<i32> = match self.mode {
                VdMode::Launched => vec![me, self.pid],
                VdMode::Attached => vec![me],
            };
            let after = snapshot_user_windows(&exclude);
            let moved = geom::diff_windows(&self.user_before, &after, 1.0);
            if moved.is_empty() {
                rep.push("사용자 창 위치 변화 없음 (시작 전과 비교)".into());
            } else {
                rep.push(format!(
                    "시작 전과 비교해 위치/크기가 바뀐 사용자 창 {}개 (사용자가 직접 옮겼을 수도 있음): {}",
                    moved.len(),
                    moved.join(" / ")
                ));
            }
            for l in &rep {
                eprintln!("[ghostdesk] 가상 디스플레이 종료: {l}");
            }
            self.report.lock().extend(rep);
        }
    }

    /// Live view: captures the virtual display (containing only our app's windows) at ~fps and sends
    /// JPEGs to the sink.
    async fn live_loop(weak: std::sync::Weak<Inner>, fps: f64) {
        let period = Duration::from_secs_f64(1.0 / fps.clamp(0.2, 10.0));
        loop {
            let Some(inner) = weak.upgrade() else { break };
            if inner.closed.load(Ordering::SeqCst) {
                let _ = tokio::task::spawn_blocking(move || drop(inner)).await;
                break;
            }
            let _ = tokio::task::spawn_blocking(move || objc2::rc::autoreleasepool(|_| {
                let i = inner;
                let (aw, ah) = i.agent;
                let k = (960.0 / aw.max(ah) as f64).min(1.0);
                let (w, h) = (((aw as f64 * k) as u32).max(1), ((ah as f64 * k) as u32).max(1));
                if let Ok(img) = i.capture(w, h) {
                    if let Ok(jpg) = capture::encode_jpeg(&img, 70) {
                        (i.sink)(&jpg);
                    }
                }
            }))
            .await;
            tokio::time::sleep(period).await;
        }
    }

    // ---- selftest (creates a real display; only with the user's consent) ----

    struct TestLog {
        ok: Vec<String>,
        fail: Vec<String>,
    }

    impl TestLog {
        fn check(&mut self, name: &str, ok: bool, detail: impl AsRef<str>) {
            let line = format!("{name}: {}", detail.as_ref());
            println!("[{}] {line}", if ok { "통과" } else { "실패" });
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

    /// (id, rect [x,y,w,h]) of the first ui_tree line matching the predicate.
    fn find_in_tree(tree: &str, pred: impl Fn(&str) -> bool) -> Option<(usize, [f64; 4])> {
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
                return Some((id, [nums[0], nums[1], nums[2], nums[3]]));
            }
        }
        None
    }

    fn read_field(read: &str, key: &str) -> Option<String> {
        read.lines().find_map(|l| l.strip_prefix(key).map(|v| v.trim().to_string()))
    }

    /// (elapsed ms, cursor position, front pid, main display id)
    type Sample = (u128, CGPoint, i32, u32);

    /// Sampler recording the user's cursor, front app and main display every 10 ms.
    struct Sampler {
        stop: Arc<AtomicBool>,
        h: Option<std::thread::JoinHandle<Vec<Sample>>>,
    }

    impl Sampler {
        fn start() -> Sampler {
            let stop = Arc::new(AtomicBool::new(false));
            let s = stop.clone();
            let h = std::thread::spawn(move || {
                let t0 = Instant::now();
                let mut out = vec![];
                while !s.load(Ordering::SeqCst) {
                    let p = CGEvent::new(None).map(|e| CGEvent::location(Some(&e))).unwrap_or(CGPoint { x: f64::NAN, y: f64::NAN });
                    out.push((t0.elapsed().as_millis(), p, skylight::front_pid(), CGMainDisplayID()));
                    sleep_ms(10);
                }
                out
            });
            Sampler { stop, h: Some(h) }
        }
        fn finish(mut self) -> Vec<Sample> {
            self.stop.store(true, Ordering::SeqCst);
            self.h.take().and_then(|h| h.join().ok()).unwrap_or_default()
        }
    }

    /// `--selftest-vdisplay --yes [--app com.apple.TextEdit] [--out DIR] [--width 1280 --height 800]`
    ///
    /// **Actually creates a virtual display and launches an app.** Only run with the user's consent
    /// (refused without `--yes`). Checks clicks, typing, shortcuts, double clicks, drags, right clicks
    /// and scrolling on our own TextEdit instance, while continuously recording the user's cursor,
    /// front app and main display to report that nothing was affected.
    pub async fn selftest(args: &[String]) -> Result<()> {
        if !args.iter().any(|a| a == "--yes") {
            bail!(
                "가상 디스플레이 셀프테스트는 실제로 가상 모니터를 하나 만들고(디스플레이 배치에 잠시 추가됨) TextEdit 을 띄웁니다.\n\
                 사용자가 동의한 경우에만 --yes 를 붙여 다시 실행하세요."
            );
        }
        let mut log = TestLog { ok: vec![], fail: vec![] };
        println!("== 가상 디스플레이 셀프테스트 ==");
        match available_report() {
            Ok(lines) => log.check("비공개 API 확인", true, format!("{}개 항목", lines.len())),
            Err(e) => {
                log.check("비공개 API 확인", false, format!("{e}"));
                bail!("가상 디스플레이 API 를 쓸 수 없습니다");
            }
        }
        let out_dir = std::path::PathBuf::from(arg_value(args, "--out").unwrap_or_else(|| {
            std::env::temp_dir().join("ghostdesk-vdisplay-selftest").to_string_lossy().to_string()
        }));
        let _ = std::fs::create_dir_all(&out_dir);
        let app = arg_value(args, "--app").unwrap_or_else(|| "com.apple.TextEdit".into());
        let width: u32 = arg_value(args, "--width").and_then(|v| v.parse().ok()).unwrap_or(1280);
        let height: u32 = arg_value(args, "--height").and_then(|v| v.parse().ok()).unwrap_or(800);
        let doc = out_dir.join("vdisplay-test.txt");
        std::fs::write(&doc, "GhostDesk virtual display test\nsecond line here\n")?;

        let main_before = CGMainDisplayID();
        let displays_before = active_displays();
        let me = std::process::id() as i32;
        let user_before = snapshot_user_windows(&[me]);
        let sampler = Sampler::start();
        let t0 = Instant::now();

        let opts = VirtualDisplayOptions {
            app: app.clone(),
            width,
            height,
            live_fps: 0.0,
            open_files: vec![doc.clone()],
            ..Default::default()
        };
        let desk = match VirtualDisplayDesk::launch(opts, Arc::new(|_b: &[u8]| {})).await {
            Ok(d) => d,
            Err(e) => {
                let samples = sampler.finish();
                log.check("작업공간 시작", false, format!("{e:#}"));
                println!("(샘플 {}개 기록)", samples.len());
                bail!("시작 실패");
            }
        };
        let pid = desk.pid();
        let vb = desk.display_bounds();
        log.check(
            "작업공간 시작",
            true,
            format!(
                "{} pid={pid}, 가상 디스플레이 {} ({:.0},{:.0} {:.0}x{:.0}), {}ms",
                desk.app_name(),
                desk.display_id(),
                vb.x,
                vb.y,
                vb.w,
                vb.h,
                t0.elapsed().as_millis()
            ),
        );
        for n in desk.report() {
            println!("   · {n}");
        }
        let mut posted_points: Vec<(f64, f64)> = vec![];
        let to_g = |x: f64, y: f64| geom::agent_to_global(&vb, (width, height), x, y);

        let result: Result<()> = async {
            // 1. screenshot
            let png = desk.screenshot().await?;
            let _ = std::fs::write(out_dir.join("01-start.png"), &png);
            let img = image::load_from_memory(&png)?.to_rgb8();
            let nonblack = img.pixels().filter(|p| p[0] as u32 + p[1] as u32 + p[2] as u32 > 30).count();
            log.check(
                "스크린샷",
                img.dimensions() == (width, height) && nonblack > (width * height / 50) as usize,
                format!("{}x{}, 검지 않은 픽셀 {}", img.width(), img.height(), nonblack),
            );

            // 2. find the text area + pixel click
            let tree = desk.app_control(&serde_json::json!({"action": "ui_tree"})).await?;
            let Some((tid, tr)) = find_in_tree(&tree, |l| l.contains("AXTextArea")) else {
                log.check("텍스트 영역 찾기", false, short(&tree, 300));
                bail!("텍스트 영역 없음");
            };
            let (cx, cy) = (tr[0] + tr[2] * 0.6, tr[1] + tr[3] * 0.6);
            posted_points.push(to_g(cx, cy));
            desk.click(Some((cx, cy)), MouseButton::Left, 1, 0).await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let tree2 = desk.app_control(&serde_json::json!({"action": "ui_tree"})).await?;
            let focused = tree2.lines().any(|l| l.contains("AXTextArea") && l.contains("focused"));
            log.check("픽셀 클릭으로 포커스", focused, format!("텍스트 영역 [{:.0},{:.0},{:.0},{:.0}]", tr[0], tr[1], tr[2], tr[3]));
            // ui_tree was called again, so refresh the id
            let tid = find_in_tree(&tree2, |l| l.contains("AXTextArea")).map(|x| x.0).unwrap_or(tid);
            let read_now = |d: Arc<VirtualDisplayDesk>| async move {
                d.app_control(&serde_json::json!({"action": "read", "id": tid})).await.unwrap_or_default()
            };

            // 3. typing (after moving to the end)
            desk.key("cmd+Down", 1).await?;
            let marker = " 가상디스플레이ABC123";
            desk.type_text(marker).await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let r1 = read_now(desk.clone()).await;
            let value = r1.split_once("value:\n").map(|x| x.1.to_string()).unwrap_or_default();
            log.check("유니코드 입력", value.contains(marker.trim()), short(&value, 80));

            // 4. cmd+A (a shortcut; needs the key window record)
            desk.key("cmd+a", 1).await?;
            tokio::time::sleep(Duration::from_millis(250)).await;
            let r2 = read_now(desk.clone()).await;
            let sel_len: i64 = read_field(&r2, "selected range:")
                .and_then(|s| s.split(',').nth(1).and_then(|v| v.trim().parse().ok()))
                .unwrap_or(0);
            log.check("cmd+A 전체 선택", sel_len > 40, format!("선택 길이 {sel_len}"));

            // 5. double click (first word of the first line)
            let (wx, wy) = (tr[0] + 30.0, tr[1] + 12.0);
            posted_points.push(to_g(wx, wy));
            desk.click(Some((wx, wy)), MouseButton::Left, 2, 0).await?;
            tokio::time::sleep(Duration::from_millis(250)).await;
            let r3 = read_now(desk.clone()).await;
            let sel = read_field(&r3, "selected text:").unwrap_or_default();
            log.check("더블클릭 단어 선택", !sel.is_empty() && !sel.contains('\n') && sel.len() < 40, format!("선택 '{sel}'"));

            // 6. drag selection (rightward within the first line)
            let (dx0, dy0, dx1) = (tr[0] + 8.0, tr[1] + 12.0, tr[0] + 160.0);
            posted_points.push(to_g(dx0, dy0));
            posted_points.push(to_g(dx1, dy0));
            desk.drag((dx0, dy0), (dx1, dy0), 0).await?;
            tokio::time::sleep(Duration::from_millis(250)).await;
            let r4 = read_now(desk.clone()).await;
            let dsel = read_field(&r4, "selected text:").unwrap_or_default();
            log.check("드래그 선택", dsel.chars().count() >= 5, format!("선택 '{}'", short(&dsel, 40)));

            // 7. right click → does the context menu appear as our window on the virtual display → Escape
            posted_points.push(to_g(cx, cy));
            desk.click(Some((cx, cy)), MouseButton::Right, 1, 0).await?;
            tokio::time::sleep(Duration::from_millis(400)).await;
            let menus: Vec<apps::CgWin> = apps::visible_windows(&[pid]).into_iter().filter(|w| w.layer >= 100).collect();
            let on_vd = menus.iter().all(|w| r(w.bounds).inside(&vb, 2.0));
            let png = desk.screenshot().await?;
            let _ = std::fs::write(out_dir.join("02-context-menu.png"), &png);
            log.check("우클릭 컨텍스트 메뉴", !menus.is_empty() && on_vd, format!("메뉴 창 {}개, 가상 디스플레이 안: {on_vd}", menus.len()));
            desk.key("Escape", 1).await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let left = apps::visible_windows(&[pid]).into_iter().filter(|w| w.layer >= 100).count();
            log.check("Escape 로 메뉴 닫기", left == 0, format!("남은 메뉴 창 {left}"));

            // 8. scrolling: insert a long text and scroll up
            desk.key("cmd+Down", 1).await?;
            let long: String = (1..=120).map(|i| format!("\nline {i}")).collect();
            desk.type_text(&long).await?;
            tokio::time::sleep(Duration::from_millis(400)).await;
            let before_vis = read_field(&read_now(desk.clone()).await, "visible range:").unwrap_or_default();
            posted_points.push(to_g(cx, cy));
            desk.scroll(Some((cx, cy)), 0.0, -3000.0, 0).await?;
            tokio::time::sleep(Duration::from_millis(400)).await;
            let after_vis = read_field(&read_now(desk.clone()).await, "visible range:").unwrap_or_default();
            log.check("스크롤", !before_vis.is_empty() && before_vis != after_vis, format!("보이는 범위 {before_vis} → {after_vis}"));

            // 9. menu action (app_control): Edit > Select All (localized names checked via menus)
            let menus_txt = desk.app_control(&serde_json::json!({"action": "menus"})).await?;
            log.check("메뉴 목록", menus_txt.lines().count() >= 3, format!("{}줄", menus_txt.lines().count()));

            // 10. clicking an empty area must error (bottom-right corner of the virtual display)
            let e = desk.click(Some((width as f64 - 3.0, height as f64 - 3.0)), MouseButton::Left, 1, 0).await;
            log.check("빈 영역 클릭 거부", e.as_ref().is_err_and(|e| e.to_string().contains(NO_WINDOW)), format!("{e:?}"));

            let png = desk.screenshot().await?;
            let _ = std::fs::write(out_dir.join("03-end.png"), &png);
            for n in desk.take_notes() {
                println!("   · 노트: {n}");
            }
            Ok(())
        }
        .await;
        if let Err(e) = &result {
            log.check("테스트 진행", false, format!("{e:#}"));
        }

        desk.shutdown().await;
        let samples = sampler.finish();
        for n in desk.report() {
            println!("   · {n}");
        }
        let gone = !active_displays().contains(&desk.display_id());
        log.check("종료 후 가상 디스플레이 제거", gone, format!("활성 디스플레이 {:?}", active_displays()));
        log.check(
            "디스플레이 구성 복원",
            active_displays().len() == displays_before.len() && CGMainDisplayID() == main_before,
            format!("전 {:?} / 후 {:?}, 주 디스플레이 {main_before} → {}", displays_before, active_displays(), CGMainDisplayID()),
        );
        let moved = geom::diff_windows(&user_before, &snapshot_user_windows(&[me]), 1.0);
        log.check("사용자 창 위치 유지", moved.is_empty(), if moved.is_empty() { "변화 없음".into() } else { moved.join(" / ") });

        // analyze the sampler
        let front_ours = samples.iter().filter(|s| s.2 == pid).count();
        log.check("front 앱 유지 (우리 앱이 활성화되지 않음)", front_ours == 0, format!("샘플 {}개 중 우리 앱 front {front_ours}", samples.len()));
        let main_changed = samples.iter().filter(|s| s.3 != main_before).count();
        log.check("주 디스플레이 유지", main_changed == 0, format!("바뀐 샘플 {main_changed}"));
        let c0 = samples.first().map(|s| s.1).unwrap_or(CGPoint { x: 0.0, y: 0.0 });
        let max_move = samples.iter().map(|s| ((s.1.x - c0.x).powi(2) + (s.1.y - c0.y).powi(2)).sqrt()).fold(0.0, f64::max);
        let on_points = samples
            .iter()
            .filter(|s| posted_points.iter().any(|p| (s.1.x - p.0).abs() < 1.0 && (s.1.y - p.1).abs() < 1.0))
            .count();
        let on_vd = samples.iter().filter(|s| vb.contains(s.1.x, s.1.y)).count();
        log.check(
            "사용자 커서 비간섭",
            on_points == 0,
            format!(
                "에이전트 입력 지점과 같은 커서 위치 샘플 {on_points}, 커서가 가상 디스플레이 위에 있던 샘플 {on_vd}, 시작 위치에서 최대 이동 {max_move:.0}pt (사용자가 직접 움직였다면 정상)"
            ),
        );

        println!("\n== 결과: 통과 {} / 실패 {} ==", log.ok.len(), log.fail.len());
        for f in &log.fail {
            println!("  실패: {f}");
        }
        println!("스크린샷: {}", out_dir.display());
        if log.fail.is_empty() {
            Ok(())
        } else {
            bail!("가상 디스플레이 셀프테스트 실패 {}건", log.fail.len())
        }
    }

}

// ---- other OSes ----

#[cfg(not(target_os = "macos"))]
mod stub {
    use std::sync::Arc;

    use anyhow::{bail, Result};
    use serde_json::Value;

    use super::geom::Rect;
    use super::{VdMode, VirtualDisplayOptions};
    use crate::browser::{FrameSink, MouseButton};
    use crate::desk::ViewInfo;

    /// Common notice, pointing to this OS's regular app backend (off-screen windows) if it has one.
    ///
    /// Windows has no public API equivalent to CGVirtualDisplay (it needs a signed IddCx driver).
    fn msg() -> String {
        use crate::platform::text::{unsupported, APP_ICON, LOCAL_APPS};
        let mut m = unsupported("가상 디스플레이");
        if crate::platform::caps::NATIVE_APPS {
            m.push_str(&format!(". 가상 디스플레이 옵션(virtual_display)을 끄고 일반 '{APP_ICON} {LOCAL_APPS}' 모드로 실행하세요"));
        }
        m
    }

    pub fn available() -> Result<()> {
        bail!(msg())
    }

    pub fn available_report() -> Result<Vec<String>> {
        bail!(msg())
    }

    pub struct VirtualDisplay;

    impl VirtualDisplay {
        pub fn create(_width_px: u32, _height_px: u32, _hidpi: bool, _name: &str) -> Result<VirtualDisplay> {
            bail!(msg())
        }
        pub fn display_id(&self) -> u32 {
            0
        }
        pub fn bounds(&self) -> Rect {
            Rect::default()
        }
        pub fn pixel_size(&self) -> (u32, u32) {
            (0, 0)
        }
        pub fn is_active(&self) -> bool {
            false
        }
        pub fn terminated(&self) -> bool {
            true
        }
        pub fn destroy(self, _our_pids: &[i32]) -> Vec<String> {
            vec![]
        }
    }

    pub struct VirtualDisplayDesk;

    impl VirtualDisplayDesk {
        pub async fn launch(_opts: VirtualDisplayOptions, _sink: FrameSink) -> Result<Arc<VirtualDisplayDesk>> {
            bail!(msg())
        }
        pub fn pid(&self) -> i32 {
            -1
        }
        pub fn mode(&self) -> VdMode {
            VdMode::Launched
        }
        pub fn app_name(&self) -> String {
            String::new()
        }
        pub fn display_id(&self) -> u32 {
            0
        }
        pub fn display_bounds(&self) -> Rect {
            Rect::default()
        }
        pub fn report(&self) -> Vec<String> {
            vec![]
        }
        pub fn size(&self) -> (u32, u32) {
            (0, 0)
        }
        pub async fn screenshot(&self) -> Result<Vec<u8>> {
            bail!(msg())
        }
        pub async fn zoom(&self, _region: [f64; 4]) -> Result<Vec<u8>> {
            bail!(msg())
        }
        pub async fn mouse_move(&self, _x: f64, _y: f64, _mods: i64) -> Result<()> {
            bail!(msg())
        }
        pub async fn mouse_down(&self, _b: MouseButton, _count: i64, _mods: i64) -> Result<()> {
            bail!(msg())
        }
        pub async fn mouse_up(&self, _b: MouseButton, _count: i64, _mods: i64) -> Result<()> {
            bail!(msg())
        }
        pub async fn click(&self, _pos: Option<(f64, f64)>, _b: MouseButton, _count: u32, _mods: i64) -> Result<()> {
            bail!(msg())
        }
        pub async fn drag(&self, _from: (f64, f64), _to: (f64, f64), _mods: i64) -> Result<()> {
            bail!(msg())
        }
        pub async fn scroll(&self, _pos: Option<(f64, f64)>, _dx: f64, _dy: f64, _mods: i64) -> Result<()> {
            bail!(msg())
        }
        pub async fn key(&self, _combo: &str, _repeat: u32) -> Result<()> {
            bail!(msg())
        }
        pub async fn hold_key(&self, _combo: &str, _secs: f64) -> Result<()> {
            bail!(msg())
        }
        pub async fn type_text(&self, _text: &str) -> Result<()> {
            bail!(msg())
        }
        pub fn mouse_pos(&self) -> (f64, f64) {
            (0.0, 0.0)
        }
        pub fn views(&self) -> (Vec<ViewInfo>, Option<String>) {
            (vec![], None)
        }
        pub async fn switch_view(&self, _index: usize) -> Result<()> {
            bail!(msg())
        }
        pub fn take_notes(&self) -> Vec<String> {
            vec![]
        }
        pub fn is_closed(&self) -> bool {
            true
        }
        pub async fn shutdown(&self) {}
        pub async fn app_control(&self, _input: &Value) -> Result<String> {
            bail!(msg())
        }
    }

    pub async fn selftest(_args: &[String]) -> Result<()> {
        bail!(msg())
    }
}

// ---- tests ----

#[cfg(test)]
mod tests {
    use super::geom::*;

    fn close(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6
    }

    #[test]
    fn mapping_roundtrip_and_scale() {
        let b = Rect::new(1728.0, 317.0, 1280.0, 800.0);
        assert!(close(agent_to_global(&b, (1280, 800), 0.0, 0.0), (1728.0, 317.0)));
        assert!(close(agent_to_global(&b, (1280, 800), 100.0, 50.0), (1828.0, 367.0)));
        // off-screen goes to the edge
        assert!(close(agent_to_global(&b, (1280, 800), -5.0, 9999.0), (1728.0, 317.0 + 799.0)));
        let g = agent_to_global(&b, (1280, 800), 640.5, 400.25);
        assert!(close(global_to_agent(&b, (1280, 800), g.0, g.1), (640.5, 400.25)));
        // a 640x400 point display seen as 1280x800 agent pixels (scale 0.5)
        let half = Rect::new(-640.0, 0.0, 640.0, 400.0);
        assert!(close(agent_to_global(&half, (1280, 800), 200.0, 100.0), (-540.0, 50.0)));
        assert_eq!(rect_to_agent(&half, (1280, 800), &Rect::new(-600.0, 10.0, 100.0, 50.0)), [80, 20, 200, 100]);
    }

    #[test]
    fn window_bounds_checks() {
        let vd = Rect::new(1728.0, 317.0, 1280.0, 800.0);
        let main = Rect::new(0.0, 0.0, 1728.0, 1117.0);
        assert!(Rect::new(1800.0, 400.0, 600.0, 400.0).inside(&vd, 0.0));
        assert!(!Rect::new(1700.0, 400.0, 600.0, 400.0).inside(&vd, 0.0));
        assert!(Rect::new(1727.0, 400.0, 600.0, 400.0).inside(&vd, 2.0));
        // a window overlapping a user display = stray
        assert!(is_stray(&Rect::new(1700.0, 400.0, 600.0, 400.0), &[main]));
        assert!(!is_stray(&Rect::new(1728.0, 400.0, 600.0, 400.0), &[main]));
        // outside every display (on none of them) isn't stray
        assert!(!is_stray(&Rect::new(3100.0, 400.0, 600.0, 400.0), &[main]));
        // contact of 1 pt² or less is ignored
        assert!(!is_stray(&Rect::new(1727.5, 400.0, 600.0, 1.0), &[main]));
    }

    #[test]
    fn fit_window_moves_and_shrinks() {
        let vd = Rect::new(1728.0, 317.0, 1280.0, 800.0);
        // already inside: unchanged
        let inside = Rect::new(1800.0, 400.0, 600.0, 400.0);
        assert_eq!(fit_window(&inside, &vd, 30.0), inside);
        // a window that was on the main display → centered
        let f = fit_window(&Rect::new(100.0, 100.0, 600.0, 400.0), &vd, 30.0);
        assert!(f.inside(&vd, 0.0));
        assert_eq!((f.w, f.h), (600.0, 400.0));
        assert!(f.y >= vd.y + 30.0);
        // too large: shrunk
        let f = fit_window(&Rect::new(0.0, 0.0, 2000.0, 1500.0), &vd, 30.0);
        assert!(f.inside(&vd, 0.0));
        assert_eq!((f.w, f.h), (1280.0, 770.0));
    }

    #[test]
    fn arrangement_far_right_bottom_aligned() {
        let main = Rect::new(0.0, 0.0, 1728.0, 1117.0);
        // a single display
        assert_eq!(arrangement_origin(&[main], &main, (1280.0, 800.0), Side::Right), (1728, 317));
        assert_eq!(arrangement_origin(&[main], &main, (1280.0, 800.0), Side::Left), (-1280, 317));
        // external monitor on the right (shifted up): rightmost edge, bottom-aligned with the main display
        let ext = Rect::new(1728.0, -500.0, 2560.0, 1440.0);
        assert_eq!(arrangement_origin(&[main, ext], &main, (1280.0, 800.0), Side::Right), (4288, 317));
        // if the external monitor doesn't overlap the main display's height vertically, bottom-align with
        // the external monitor
        let high = Rect::new(1728.0, -1440.0, 2560.0, 1440.0);
        assert_eq!(arrangement_origin(&[main, high], &main, (1280.0, 800.0), Side::Right), (4288, -800));
        // a monitor on the left doesn't affect right-side placement
        let left = Rect::new(-1920.0, 0.0, 1920.0, 1080.0);
        assert_eq!(arrangement_origin(&[left, main], &main, (1280.0, 800.0), Side::Right), (1728, 317));
        assert_eq!(arrangement_origin(&[left, main], &main, (1280.0, 800.0), Side::Left), (-3200, 317));
        // a virtual display taller than the main one: bottom-aligned even if y goes negative
        assert_eq!(arrangement_origin(&[main], &main, (1600.0, 1200.0), Side::Right), (1728, -83));
        // two displays with the same right edge (stacked vertically): use the one overlapping more of the
        // main display's height
        let top = Rect::new(1728.0, -1080.0, 1920.0, 1080.0);
        let bottom = Rect::new(1728.0, 0.0, 1920.0, 1080.0);
        assert_eq!(arrangement_origin(&[main, top, bottom], &main, (1280.0, 800.0), Side::Right), (3648, 317));
        // the result overlaps no display
        for ds in [vec![main], vec![main, ext], vec![main, high], vec![left, main], vec![main, top, bottom]] {
            for side in [Side::Right, Side::Left] {
                let (x, y) = arrangement_origin(&ds, &main, (1280.0, 800.0), side);
                let me = Rect::new(x as f64, y as f64, 1280.0, 800.0);
                assert!(ds.iter().all(|d| d.intersection_area(&me) == 0.0), "{ds:?} {side:?}");
                // it shares an edge with some display (no gap)
                assert!(ds.iter().any(|d| {
                    let touch_x = (d.max_x() - me.x).abs() < 0.5 || (me.max_x() - d.x).abs() < 0.5;
                    let ov = (me.max_y().min(d.max_y()) - me.y.max(d.y)) > 0.0;
                    touch_x && ov
                }));
            }
        }
    }

    #[test]
    fn dock_side() {
        assert_eq!(side_for_dock(None), Side::Right);
        assert_eq!(side_for_dock(Some("bottom")), Side::Right);
        assert_eq!(side_for_dock(Some("left")), Side::Right);
        assert_eq!(side_for_dock(Some("right")), Side::Left);
    }

    #[test]
    fn helpers() {
        let p = interp((0.0, 0.0), (10.0, 20.0), 4);
        assert_eq!(p.len(), 4);
        assert_eq!(p[3], (10.0, 20.0));
        assert_eq!(drag_steps((0.0, 0.0), (0.0, 0.0)), 6);
        assert_eq!(drag_steps((0.0, 0.0), (1000.0, 0.0)), 60);
        assert_eq!(scroll_chunks(-300.0, 120.0), vec![-120.0, -120.0, -60.0]);
        assert!(scroll_chunks(0.0, 120.0).is_empty());
        assert_eq!(mode_for(2560, 1600, true), (1280, 800));
        assert_eq!(mode_for(1280, 800, false), (1280, 800));
        assert_eq!(region_to_pixels([10.0, 10.0, 110.0, 60.0], (1280, 800), (2560, 1600)), (20, 20, 200, 100));
        assert_eq!(region_to_pixels([1270.0, 790.0, 2000.0, 2000.0], (1280, 800), (1280, 800)), (1270, 790, 10, 10));
        assert_eq!(strip_encoding("v20@0:8I16"), "v@:I");
        assert_eq!(strip_encoding("v32@0:8{CGSize=dd}16"), "v@:{CGSize=dd}");
        let (w, _) = physical_size_mm(1280.0, 800.0);
        assert!((w - 295.56).abs() < 0.1);
        assert!(covers_display(&Rect::new(0.0, 0.0, 100.0, 100.0), &Rect::new(0.0, 0.0, 100.0, 100.0)));
        assert!(!covers_display(&Rect::new(0.0, 0.0, 50.0, 100.0), &Rect::new(0.0, 0.0, 100.0, 100.0)));
    }

    #[test]
    fn window_diff() {
        let a = WinSnap { wid: 1, pid: 10, rect: Rect::new(0.0, 0.0, 100.0, 100.0), name: "a".into() };
        let b = WinSnap { wid: 2, pid: 11, rect: Rect::new(50.0, 50.0, 100.0, 100.0), name: "b".into() };
        let mut b2 = b.clone();
        b2.rect.x += 10.0;
        let c = WinSnap { wid: 3, pid: 12, rect: Rect::new(0.0, 0.0, 10.0, 10.0), name: "c".into() };
        assert!(diff_windows(&[a.clone(), b.clone()], &[a.clone(), b.clone(), c], 1.0).is_empty());
        let d = diff_windows(&[a.clone(), b], &[a, b2], 1.0);
        assert_eq!(d.len(), 1);
        assert!(d[0].contains("창 2"));
    }

    /// Read only: checks the private classes/selectors/symbols exist (no display is created).
    #[cfg(target_os = "macos")]
    #[test]
    fn private_api_available_readonly() {
        match super::available_report() {
            Ok(lines) => {
                for l in &lines {
                    println!("{l}");
                }
                assert!(lines.iter().any(|l| l.contains("CGVirtualDisplay")));
            }
            Err(e) => panic!("{e}"),
        }
    }

    /// Other OSes: every entry point returns the common notice error (no panic).
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn stub_reports_unsupported() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let sink: crate::browser::FrameSink = std::sync::Arc::new(|_: &[u8]| {});
        let errs = [
            super::available().unwrap_err().to_string(),
            super::available_report().unwrap_err().to_string(),
            super::VirtualDisplay::create(1280, 800, false, "t").err().unwrap().to_string(),
            rt.block_on(super::VirtualDisplayDesk::launch(super::VirtualDisplayOptions::default(), sink)).err().unwrap().to_string(),
            rt.block_on(super::selftest(&["--yes".to_string()])).unwrap_err().to_string(),
        ];
        for e in &errs {
            assert_eq!(e, &errs[0]);
            assert!(e.starts_with("가상 디스플레이 은(는)"), "{e}");
            assert!(e.contains("macOS 전용"), "{e}");
        }
        if crate::platform::caps::NATIVE_APPS {
            assert!(errs[0].contains(crate::platform::text::LOCAL_APPS), "{}", errs[0]);
        }
    }
}
