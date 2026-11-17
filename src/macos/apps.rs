//! Launching, hiding and quitting apps, plus window lists (NSWorkspace, NSRunningApplication,
//! CGWindowList).
//!
//! Never unhide, activate, AXRaise or deminiaturize: any of those puts a window on the user's
//! screen. (The one exception is `restore_unhide`, which returns a user's app to its original
//! visible state when mode B ends.)

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

/// A resolved target app.
#[derive(Clone, Debug)]
pub struct AppTarget {
    pub path: PathBuf,
    pub bundle_id: Option<String>,
    pub name: String,
}

/// Finds an app by path (.app) or bundle id.
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

/// Pids of running instances with this bundle id.
pub fn running_pids(bundle_id: &str) -> Vec<i32> {
    NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(bundle_id))
        .iter()
        .filter(|a| !a.isTerminated())
        .map(|a| a.processIdentifier())
        .collect()
}

/// Asks other running instances of `app` (.app path, excluding this process) to quit normally,
/// like the Quit menu item, and waits up to `wait` for them to exit. Returns false if any are
/// still running.
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

/// Pids of the target's running instances: same bundle id, or same .app path when the bundle id
/// is unknown.
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

/// Info.plist's `LSMultipleInstancesProhibited`. When true, asking for a new instance returns
/// the existing process instead, and `hides=true` could then hide the user's app, so this must
/// be refused before launching.
pub fn multiple_instances_prohibited(app: &Path) -> bool {
    objc2_foundation::NSBundle::bundleWithPath(&NSString::from_str(&app.to_string_lossy()))
        .and_then(|b| b.objectForInfoDictionaryKey(&NSString::from_str("LSMultipleInstancesProhibited")))
        .and_then(|o| o.downcast::<NSNumber>().ok())
        .map(|n| n.boolValue())
        .unwrap_or(false)
}

/// Pids of every running app (NSRunningApplication). Used right after launch to tell whether a
/// process is new, so an existing user instance is never mistaken for ours, even for apps
/// without a known bundle id.
pub fn all_running_pids() -> std::collections::HashSet<i32> {
    NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|a| !a.isTerminated())
        .map(|a| a.processIdentifier())
        .collect()
}

/// Bundle id of a pid (`None` if it's not a running app).
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

/// Hides the app (its windows leave the user's screen). Returns `isHidden` afterwards.
pub fn hide(pid: i32) -> bool {
    match running_app(pid) {
        Some(a) => {
            a.hide();
            a.isHidden()
        }
        None => false,
    }
}

/// Mode B shutdown only: makes the user's app visible again if it was before (without
/// activating it).
pub fn restore_unhide(pid: i32) -> bool {
    running_app(pid).map(|a| a.unhide()).unwrap_or(false)
}

/// Launches a **new instance** (or the first one) of an app, hidden and inactive.
///
/// - `activates=false`, `hides=true`, `addsToRecentItems=false`, `promptsUserIfNeeded=false`
/// - with `new_instance`, creates a separate process even if the app is already running
///   (`createsNewApplicationInstance`)
/// - `args` e.g. `["-ApplePersistenceIgnoreState", "YES"]` keeps the user's window restoration
///   state from being read or written
/// - `files` are opened at launch
///
/// Document-based apps may flash a window right after launch before hiding, so the caller must
/// run an exposure guard.
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

/// Kills a process (and its process group): SIGTERM, up to 3 s wait, then SIGKILL.
///
/// AppKit's graceful `terminate` can raise a "Save changes?" prompt, so it isn't used.
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

/// Window info from CGWindowList.
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
    /// Whether the window is actually visible on the user's screen (onscreen && alpha > 0).
    pub fn visible(&self) -> bool {
        self.onscreen && self.alpha > 0.0
    }
}

/// Window list. Empty `pids` means all; `onscreen_only` limits it to windows on screen.
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

/// Windows of the given pids that are visible on the user's screen.
pub fn visible_windows(pids: &[i32]) -> Vec<CgWin> {
    if pids.is_empty() {
        return vec![];
    }
    cg_windows(pids, true).into_iter().filter(|w| w.visible()).collect()
}
