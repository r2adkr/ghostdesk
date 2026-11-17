//! Private SkyLight (WindowServer) APIs, resolved with dlsym only so there is no link-time
//! dependency.
//!
//! Safety rules:
//! - `SLPSPostEventRecordTo` records are only ever sent to the PSN of a target process we
//!   launched (or the user consented to). We don't use cua-driver's approach of sending a
//!   defocus record to the user's front process.
//! - Never call `_SLPSSetFrontProcessWithOptions`, `SetFrontProcess` or
//!   `CGWarpMouseCursorPosition`.

use std::ffi::{c_void, CString};
use std::sync::OnceLock;

#[repr(C)]
#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub struct Psn {
    pub hi: u32,
    pub lo: u32,
}

const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

fn skylight_loaded() -> bool {
    static H: OnceLock<usize> = OnceLock::new();
    *H.get_or_init(|| {
        let p = CString::new("/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight").unwrap();
        unsafe { libc::dlopen(p.as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL) as usize }
    }) != 0
}

/// Address of a global symbol, or null.
pub fn sym(name: &str) -> *mut c_void {
    skylight_loaded();
    let c = CString::new(name).unwrap();
    unsafe { libc::dlsym(RTLD_DEFAULT, c.as_ptr()) }
}

/// pid → PSN (deprecated GetProcessForPID, ApplicationServices)
pub fn psn_for_pid(pid: i32) -> Option<Psn> {
    type F = unsafe extern "C" fn(i32, *mut Psn) -> i32;
    let p = sym("GetProcessForPID");
    if p.is_null() {
        return None;
    }
    let f: F = unsafe { std::mem::transmute(p) };
    let mut psn = Psn::default();
    (unsafe { f(pid, &mut psn) } == 0).then_some(psn)
}

/// Pid of WindowServer's front process (`_SLPSGetFrontProcess` + `GetProcessPID`), or -1.
pub fn front_pid() -> i32 {
    type GetFront = unsafe extern "C" fn(*mut Psn) -> i32;
    type ToPid = unsafe extern "C" fn(*const Psn, *mut i32) -> i32;
    let (gf, tp) = (sym("_SLPSGetFrontProcess"), sym("GetProcessPID"));
    if gf.is_null() || tp.is_null() {
        return -1;
    }
    unsafe {
        let gf: GetFront = std::mem::transmute(gf);
        let tp: ToPid = std::mem::transmute(tp);
        let mut psn = Psn::default();
        let mut pid = -1;
        if gf(&mut psn) == 0 {
            tp(&psn, &mut pid);
        }
        pid
    }
}

fn post_record(psn: &Psn, rec: &[u8; 0xf8]) -> i32 {
    type F = unsafe extern "C" fn(*const Psn, *const u8) -> i32;
    let p = sym("SLPSPostEventRecordTo");
    if p.is_null() {
        return -1;
    }
    let f: F = unsafe { std::mem::transmute(p) };
    // occasionally returns -600 (procNotFound and friends), so retry a few times
    let mut r = -1;
    for _ in 0..4 {
        r = unsafe { f(psn, rec.as_ptr()) };
        if r != -600 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    r
}

/// Posts SkyLight records that set AppKit's key-window state inside the target process, and
/// only to the target's PSN.
///
/// Sequence: defocus (0x0d, 0x8a=0x02) → focus (0x0d, 0x8a=0x01) → make-key (0x01, 0x02), each
/// a 248-byte record. WindowServer's front process and the user's focus are unaffected.
///
/// Caveat (observed on this machine): the records had no effect on apps that started hidden.
/// Even when they do take effect, verify by reading the result back whether Cmd shortcuts,
/// menus and clicks actually work.
///
/// Returns the result code of each record.
pub fn post_key_window_records(pid: i32, wid: u32) -> Result<Vec<i32>, String> {
    let psn = psn_for_pid(pid).ok_or_else(|| format!("pid {pid} 의 PSN 을 얻지 못했습니다"))?;
    let mut codes = Vec::new();
    let mut defocus = [0u8; 0xf8];
    defocus[0x04] = 0xf8;
    defocus[0x08] = 0x0d;
    defocus[0x8a] = 0x02;
    defocus[0x3c..0x40].copy_from_slice(&wid.to_le_bytes());
    codes.push(post_record(&psn, &defocus));
    let mut focus = [0u8; 0xf8];
    focus[0x04] = 0xf8;
    focus[0x08] = 0x0d;
    focus[0x8a] = 0x01;
    focus[0x3c..0x40].copy_from_slice(&wid.to_le_bytes());
    codes.push(post_record(&psn, &focus));
    std::thread::sleep(std::time::Duration::from_millis(60));
    for kind in [0x01u8, 0x02] {
        let mut b = [0u8; 0xf8];
        b[0x04] = 0xf8;
        b[0x08] = kind;
        b[0x3a] = 0x10;
        b[0x3c..0x40].copy_from_slice(&wid.to_le_bytes());
        for v in b.iter_mut().take(0x30).skip(0x20) {
            *v = 0xff;
        }
        codes.push(post_record(&psn, &b));
    }
    Ok(codes)
}
