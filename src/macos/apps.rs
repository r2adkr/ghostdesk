//! 앱 실행/숨김/종료와 창 목록 (NSWorkspace, NSRunningApplication, CGWindowList).
//!
//! 금지: unhide / activate / AXRaise / 최소화 해제 — 창이 사용자 화면에 나타난다.
//! (유일한 예외: Mode B 종료 시 사용자의 앱을 원래 보이던 상태로 되돌리는 `restore_unhide`)

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSRunningApplication, NSWorkspace, NSWorkspaceOpenConfiguration};
use objc2_core_foundation::{CFArray, CFRetained, CGRect, CGPoint, CGSize};
use objc2_core_graphics::{CGWindowListCopyWindowInfo, CGWindowListOption};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSString, NSURL};

/// 대상 앱 식별 결과
#[derive(Clone, Debug)]
pub struct AppTarget {
    pub path: PathBuf,
    pub bundle_id: Option<String>,
    pub name: String,
}

/// 경로(.app) 또는 번들 id 로 앱을 찾는다.
pub fn resolve_app(app: &str) -> Result<AppTarget> {
    let ws = NSWorkspace::sharedWorkspace();
    let path = if Path::new(app).exists() {
        PathBuf::from(app)
    } else {
        let url = ws
            .URLForApplicationWithBundleIdentifier(&NSString::from_str(app))
            .ok_or_else(|| anyhow!("앱을 찾지 못했습니다: {app} (.app 경로 또는 번들 id 를 지정하세요)"))?;
        PathBuf::from(url.path().map(|p| p.to_string()).unwrap_or_default())
    };
    let bundle_id = bundle_identifier(&path);
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| app.to_string());
    Ok(AppTarget { path, bundle_id, name })
}

fn bundle_identifier(app: &Path) -> Option<String> {
    let b = objc2_foundation::NSBundle::bundleWithPath(&NSString::from_str(&app.to_string_lossy()))?;
    b.bundleIdentifier().map(|s| s.to_string())
}

/// 번들 id 로 실행 중인 인스턴스 pid 목록
pub fn running_pids(bundle_id: &str) -> Vec<i32> {
    NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(bundle_id))
        .iter()
        .filter(|a| !a.isTerminated())
        .map(|a| a.processIdentifier())
        .collect()
}

/// `app`(.app 경로)에서 실행 중인 다른 인스턴스(이 프로세스 제외)에 종료를 요청하고(메뉴의 '종료'와 같은 정상 종료),
/// 최대 `wait` 동안 끝나기를 기다린다. 아직 떠 있는 것이 있으면 false.
pub fn quit_instances_at(bundle_id: &str, app: &Path, wait: Duration) -> bool {
    let me = std::process::id() as i32;
    let want = app.canonicalize().unwrap_or_else(|_| app.to_path_buf());
    let at_app = |a: &NSRunningApplication| {
        a.processIdentifier() != me
            && !a.isTerminated()
            && a.bundleURL()
                .and_then(|u| u.path())
                .map(|p| {
                    let p = PathBuf::from(p.to_string());
                    p.canonicalize().unwrap_or(p) == want
                })
                .unwrap_or(false)
    };
    let list = || NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(bundle_id));
    let mut any = false;
    for a in list().iter().filter(|a| at_app(a)) {
        any = true;
        a.terminate();
    }
    if !any {
        return true;
    }
    let deadline = std::time::Instant::now() + wait;
    while std::time::Instant::now() < deadline {
        if !list().iter().any(|a| at_app(&a)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

/// 대상 앱의 실행 중인 인스턴스 pid — 번들 id 가 같거나, 번들 id 를 모르면 .app 경로가 같은 프로세스.
pub fn running_pids_for(t: &AppTarget) -> Vec<i32> {
    let want_path = std::fs::canonicalize(&t.path).unwrap_or_else(|_| t.path.clone());
    NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|a| !a.isTerminated())
        .filter(|a| {
            let same_id = matches!((&t.bundle_id, a.bundleIdentifier()), (Some(w), Some(g)) if *w == g.to_string());
            let same_path = a
                .bundleURL()
                .and_then(|u| u.path())
                .map(|p| std::fs::canonicalize(p.to_string()).map(|c| c == want_path).unwrap_or(false))
                .unwrap_or(false);
            same_id || same_path
        })
        .map(|a| a.processIdentifier())
        .collect()
}

/// Info.plist 의 LSMultipleInstancesProhibited — true 면 새 인스턴스 요청에도 기존 프로세스를 돌려준다
/// (이때 hides=true 설정 때문에 사용자의 앱이 숨겨질 수 있으므로 실행 전에 거부해야 한다).
pub fn multiple_instances_prohibited(app: &Path) -> bool {
    objc2_foundation::NSBundle::bundleWithPath(&NSString::from_str(&app.to_string_lossy()))
        .and_then(|b| b.objectForInfoDictionaryKey(&NSString::from_str("LSMultipleInstancesProhibited")))
        .and_then(|o| o.downcast::<NSNumber>().ok())
        .map(|n| n.boolValue())
        .unwrap_or(false)
}

/// 지금 실행 중인 모든 앱(NSRunningApplication)의 pid. 실행 직후 "새 프로세스인가"를 판별할 때 쓴다
/// (번들 id 를 모르는 앱도 기존 사용자 인스턴스를 우리 것으로 착각하지 않도록).
pub fn all_running_pids() -> std::collections::HashSet<i32> {
    NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|a| !a.isTerminated())
        .map(|a| a.processIdentifier())
        .collect()
}

/// pid 의 번들 id (실행 중인 앱이 아니면 None)
pub fn bundle_id_of(pid: i32) -> Option<String> {
    running_app(pid)?.bundleIdentifier().map(|s| s.to_string())
}

pub fn running_app(pid: i32) -> Option<Retained<NSRunningApplication>> {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
}

pub fn is_hidden(pid: i32) -> Option<bool> {
    running_app(pid).map(|a| a.isHidden())
}

pub fn is_running(pid: i32) -> bool {
    running_app(pid).map(|a| !a.isTerminated()).unwrap_or(false)
        || unsafe { libc::kill(pid, 0) } == 0
}

/// 앱을 숨긴다 (창이 사용자 화면에서 사라짐). 결과로 isHidden 을 돌려준다.
pub fn hide(pid: i32) -> bool {
    match running_app(pid) {
        Some(a) => {
            a.hide();
            a.isHidden()
        }
        None => false,
    }
}

/// Mode B 종료 시에만: 사용자의 앱이 원래 보이던 상태였다면 다시 보이게 한다 (활성화는 하지 않음).
pub fn restore_unhide(pid: i32) -> bool {
    running_app(pid).map(|a| a.unhide()).unwrap_or(false)
}

/// 숨김·비활성 상태로 앱의 **새 인스턴스**(또는 첫 인스턴스)를 띄운다.
///
/// - activates=false, hides=true, addsToRecentItems=false, promptsUserIfNeeded=false
/// - new_instance=true 면 이미 실행 중이어도 별도 프로세스를 만든다 (createsNewApplicationInstance)
/// - args 예: ["-ApplePersistenceIgnoreState", "YES"] — 사용자의 창 복원 상태를 읽지/쓰지 않게 한다
/// - files 가 있으면 그 파일들을 연 채로 시작한다
///
/// 주의: 문서 기반 앱은 실행 직후 창을 잠깐 보였다가 숨는 경우가 있다 → 호출자가 노출 감시를 할 것.
pub fn launch_hidden(app: &Path, files: &[PathBuf], args: &[String], new_instance: bool) -> Result<i32> {
    let ws = NSWorkspace::sharedWorkspace();
    let cfg = NSWorkspaceOpenConfiguration::new();
    cfg.setActivates(false);
    cfg.setHides(true);
    cfg.setAddsToRecentItems(false);
    cfg.setPromptsUserIfNeeded(false);
    cfg.setCreatesNewApplicationInstance(new_instance);
    let ns_args: Vec<Retained<NSString>> = args.iter().map(|a| NSString::from_str(a)).collect();
    cfg.setArguments(&NSArray::from_retained_slice(&ns_args));
    let app_url = NSURL::fileURLWithPath(&NSString::from_str(&app.to_string_lossy()));

    let (tx, rx) = mpsc::channel::<Result<i32, String>>();
    let block = RcBlock::new(move |a: *mut NSRunningApplication, e: *mut NSError| {
        let r = if !e.is_null() {
            let e = unsafe { &*e };
            Err(format!("{} (code={})", e.localizedDescription(), e.code()))
        } else if a.is_null() {
            Err("실행된 앱 정보가 없습니다".into())
        } else {
            Ok(unsafe { &*a }.processIdentifier())
        };
        let _ = tx.send(r);
    });
    if files.is_empty() {
        ws.openApplicationAtURL_configuration_completionHandler(&app_url, &cfg, Some(&block));
    } else {
        let urls: Vec<Retained<NSURL>> = files
            .iter()
            .map(|f| NSURL::fileURLWithPath(&NSString::from_str(&f.to_string_lossy())))
            .collect();
        ws.openURLs_withApplicationAtURL_configuration_completionHandler(
            &NSArray::from_retained_slice(&urls),
            &app_url,
            &cfg,
            Some(&block),
        );
    }
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(pid)) => Ok(pid),
        Ok(Err(e)) => bail!("앱 실행 실패: {e}"),
        Err(_) => bail!("앱 실행이 30초 안에 끝나지 않았습니다"),
    }
}

/// 프로세스(와 그 프로세스 그룹)를 종료한다. SIGTERM → 최대 3초 대기 → SIGKILL.
/// AppKit 의 정상 종료(terminate)는 "저장하시겠습니까" 창을 띄울 수 있으므로 쓰지 않는다.
pub fn kill_process(pid: i32) {
    if pid <= 1 {
        return;
    }
    unsafe {
        let pgid = libc::getpgid(pid);
        if pgid == pid {
            libc::kill(-pid, libc::SIGTERM);
        }
        libc::kill(pid, libc::SIGTERM);
    }
    for _ in 0..30 {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe {
        let pgid = libc::getpgid(pid);
        if pgid == pid {
            libc::kill(-pid, libc::SIGKILL);
        }
        libc::kill(pid, libc::SIGKILL);
    }
}

/// CGWindowList 의 창 정보
#[derive(Clone, Debug)]
pub struct CgWin {
    pub wid: u32,
    pub pid: i32,
    pub layer: i64,
    pub onscreen: bool,
    pub alpha: f64,
    pub bounds: CGRect,
    pub name: String,
}

impl CgWin {
    /// 사용자 화면에 실제로 보이는가 (onscreen && alpha>0)
    pub fn visible(&self) -> bool {
        self.onscreen && self.alpha > 0.0
    }
}

/// 창 목록. pids 가 비어 있으면 전체. onscreen_only=true 면 화면에 올라간 창만.
pub fn cg_windows(pids: &[i32], onscreen_only: bool) -> Vec<CgWin> {
    let opt = if onscreen_only { CGWindowListOption::OptionOnScreenOnly } else { CGWindowListOption::OptionAll };
    let Some(arr) = CGWindowListCopyWindowInfo(opt, 0) else { return vec![] };
    let arr: CFRetained<CFArray> = arr;
    let ns: &NSArray<NSDictionary<NSString, AnyObject>> = unsafe { &*(CFRetained::as_ptr(&arr).as_ptr() as *const _) };
    let num = |d: &NSDictionary<NSString, AnyObject>, k: &str| -> Option<f64> {
        d.objectForKey(&NSString::from_str(k))?.downcast::<NSNumber>().ok().map(|n| n.doubleValue())
    };
    let mut out = vec![];
    for d in ns.iter() {
        let pid = num(&d, "kCGWindowOwnerPID").unwrap_or(-1.0) as i32;
        if !pids.is_empty() && !pids.contains(&pid) {
            continue;
        }
        let mut b = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: 0.0, height: 0.0 } };
        if let Some(bo) = d.objectForKey(&NSString::from_str("kCGWindowBounds")) {
            if let Ok(bd) = bo.downcast::<NSDictionary>() {
                let bd: &NSDictionary<NSString, AnyObject> = unsafe { &*(Retained::as_ptr(&bd) as *const _) };
                b.origin.x = num(bd, "X").unwrap_or(0.0);
                b.origin.y = num(bd, "Y").unwrap_or(0.0);
                b.size.width = num(bd, "Width").unwrap_or(0.0);
                b.size.height = num(bd, "Height").unwrap_or(0.0);
            }
        }
        out.push(CgWin {
            wid: num(&d, "kCGWindowNumber").unwrap_or(0.0) as u32,
            pid,
            layer: num(&d, "kCGWindowLayer").unwrap_or(0.0) as i64,
            onscreen: num(&d, "kCGWindowIsOnscreen").unwrap_or(0.0) != 0.0,
            alpha: num(&d, "kCGWindowAlpha").unwrap_or(0.0),
            bounds: b,
            name: d
                .objectForKey(&NSString::from_str("kCGWindowName"))
                .and_then(|o| o.downcast::<NSString>().ok())
                .map(|s| s.to_string())
                .unwrap_or_default(),
        });
    }
    out
}

/// 대상 pid 들 중 사용자 화면에 보이는 창
pub fn visible_windows(pids: &[i32]) -> Vec<CgWin> {
    if pids.is_empty() {
        return vec![];
    }
    cg_windows(pids, true).into_iter().filter(|w| w.visible()).collect()
}
