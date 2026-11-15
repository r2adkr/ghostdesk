//! 노출 감시기: 우리가 숨김 상태로 띄운 앱이 스스로 unhide 하거나 창을 화면에 올리면 즉시 다시 숨긴다.
//!
//! 문서 기반 앱(TextEdit 등)은 문서 창을 표시할 때 스스로 unhide 하는 경우가 있다 (이 Mac 에서 확인:
//! 실행 직후 약 20ms 노출). 두 가지 경로로 감시한다.
//! 1. AXObserver: `AXApplicationShown` / `AXWindowCreated` / `AXWindowDeminiaturized` 알림이 오면 즉시 hide.
//! 2. 30ms 간격 CGWindowList 폴링: 화면에 보이는(onscreen, alpha>0) 창이 있으면 hide 하고 노출 시간을 기록.
//!
//! **우리가 띄운 프로세스(Mode A)에만** 쓴다. 사용자의 앱(Mode B)에 쓰면 사용자가 앱을 다시 보려 할 때 방해가 된다.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::apps;

type AXObserverRef = *mut c_void;
type ObsCallback = extern "C" fn(AXObserverRef, *const c_void, *const c_void, *mut c_void);

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXObserverCreate(pid: i32, cb: ObsCallback, out: *mut AXObserverRef) -> i32;
    fn AXObserverAddNotification(obs: AXObserverRef, el: *const c_void, notif: *const c_void, refcon: *mut c_void) -> i32;
    fn AXObserverGetRunLoopSource(obs: AXObserverRef) -> *mut c_void;
    fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(rl: *mut c_void, src: *mut c_void, mode: *const c_void);
    fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source_handled: u8) -> i32;
    fn CFRelease(p: *const c_void);
    static kCFRunLoopDefaultMode: *const c_void;
}

struct Shared {
    pid: i32,
    stop: AtomicBool,
    events: Mutex<Vec<String>>,
    started: Instant,
}

/// 감시기 핸들. drop 하면 멈춘다.
pub struct ExposureGuard {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

extern "C" fn on_notification(_obs: AXObserverRef, _el: *const c_void, notif: *const c_void, refcon: *mut c_void) {
    if refcon.is_null() {
        return;
    }
    let shared = unsafe { &*(refcon as *const Shared) };
    if shared.stop.load(Ordering::SeqCst) {
        return;
    }
    let name = unsafe { (&*(notif as *const objc2_core_foundation::CFString)).to_string() };
    if name == "AXApplicationShown" || name == "AXWindowDeminiaturized" {
        apps::hide(shared.pid);
        shared
            .events
            .lock()
            .push(format!("{}ms: 앱이 스스로 표시({name}) → 즉시 숨김", shared.started.elapsed().as_millis()));
    } else if !apps::visible_windows(&[shared.pid]).is_empty() {
        apps::hide(shared.pid);
        shared
            .events
            .lock()
            .push(format!("{}ms: 새 창이 보이는 상태로 생성({name}) → 즉시 숨김", shared.started.elapsed().as_millis()));
    }
}

impl ExposureGuard {
    pub fn start(pid: i32) -> ExposureGuard {
        let shared = Arc::new(Shared { pid, stop: AtomicBool::new(false), events: Mutex::new(vec![]), started: Instant::now() });
        let sh = shared.clone();
        let thread = std::thread::Builder::new()
            .name(format!("ghostdesk-guard-{pid}"))
            .spawn(move || run(sh))
            .ok();
        ExposureGuard { shared, thread }
    }

    /// 지금까지의 노출/대응 기록을 가져가고 비운다
    pub fn take_events(&self) -> Vec<String> {
        std::mem::take(&mut self.shared.events.lock())
    }

    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ExposureGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(sh: Arc<Shared>) {
    let refcon = Arc::as_ptr(&sh) as *mut c_void;
    let mut obs: AXObserverRef = std::ptr::null_mut();
    let app = unsafe { AXUIElementCreateApplication(sh.pid) };
    let ok = unsafe { AXObserverCreate(sh.pid, on_notification, &mut obs) } == 0 && !obs.is_null();
    // 앱이 막 실행된 직후에는 알림 등록이 실패할 수 있어 몇 초 동안 재시도한다
    let mut pending: Vec<&str> = vec!["AXApplicationShown", "AXWindowCreated", "AXWindowDeminiaturized"];
    let try_register = |pending: &mut Vec<&str>| {
        pending.retain(|n| {
            let s = objc2_core_foundation::CFString::from_str(n);
            let p = objc2_core_foundation::CFRetained::as_ptr(&s).as_ptr() as *const c_void;
            let r = unsafe { AXObserverAddNotification(obs, app, p, refcon) };
            // 0 = 성공, -25209 = 이미 등록됨
            !(r == 0 || r == -25209)
        });
    };
    if ok {
        try_register(&mut pending);
        unsafe { CFRunLoopAddSource(CFRunLoopGetCurrent(), AXObserverGetRunLoopSource(obs), kCFRunLoopDefaultMode) };
    }
    let mut visible_since: Option<Instant> = None;
    while !sh.stop.load(Ordering::SeqCst) {
        if ok && !pending.is_empty() && sh.started.elapsed() < Duration::from_secs(10) {
            try_register(&mut pending);
        }
        if ok {
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.03, 0) };
        } else {
            std::thread::sleep(Duration::from_millis(30));
        }
        if sh.stop.load(Ordering::SeqCst) {
            break;
        }
        let vis = apps::visible_windows(&[sh.pid]);
        if !vis.is_empty() {
            if visible_since.is_none() {
                visible_since = Some(Instant::now());
            }
            apps::hide(sh.pid);
        } else if let Some(s) = visible_since.take() {
            sh.events.lock().push(format!(
                "{}ms: 창이 화면에 약 {}ms 보여 숨김 (30ms 폴링)",
                (s - sh.started).as_millis(),
                s.elapsed().as_millis().max(30)
            ));
        }
        if !apps::is_running(sh.pid) {
            break;
        }
    }
    unsafe {
        if !obs.is_null() {
            CFRelease(obs);
        }
        if !app.is_null() {
            CFRelease(app);
        }
    }
}
