//! SkyLight(WindowServer) 비공개 API — dlsym 으로만 접근한다 (링크 의존성 없음).
//!
//! 안전 규칙:
//! - `SLPSPostEventRecordTo` 레코드는 **우리가 띄운(또는 사용자가 동의한) 대상 프로세스의 PSN 에만** 보낸다.
//!   사용자의 front 프로세스에 defocus 레코드를 보내는 cua-driver 원본 방식은 쓰지 않는다.
//! - `_SLPSSetFrontProcessWithOptions` / `SetFrontProcess` / `CGWarpMouseCursorPosition` 은 절대 부르지 않는다.

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

/// 전역 심볼 주소 (없으면 null)
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

/// WindowServer 기준 front 프로세스의 pid (`_SLPSGetFrontProcess` + `GetProcessPID`). 실패 시 -1.
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
    // 가끔 -600(procNotFound 류)을 돌려주므로 몇 번 재시도
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

/// 대상 프로세스 내부의 AppKit "키 창" 상태를 설정하는 SkyLight 레코드를 **대상 PSN 에만** 보낸다.
///
/// 순서: defocus(0x0d, 0x8a=0x02) → focus(0x0d, 0x8a=0x01) → make-key(0x01, 0x02). 248바이트 레코드.
/// WindowServer 의 front 프로세스나 사용자의 포커스는 바뀌지 않는다.
///
/// 한계 (이 Mac 에서 확인): 앱이 처음부터 숨겨진 상태라면 이 레코드는 효과가 없었다.
/// 효과가 있는 경우에도 Cmd 단축키·메뉴·마우스 클릭이 동작하는지는 반드시 결과를 다시 읽어 검증할 것.
/// 반환: 각 레코드의 결과 코드.
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
