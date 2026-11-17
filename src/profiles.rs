//! Finds installed Chromium-based browsers and their profiles, and clones a profile into an
//! isolated workspace.
//!
//! The original profile is only read. The clone runs with the same browser binary (with the same
//! Keychain item on macOS, and the same user's DPAPI / App-Bound key on Windows), so cookies and
//! login sessions decrypt as is.
//!
//! On Windows a running browser holds an exclusive lock on its cookie DB (`Network\Cookies`).
//! Skipping such a critical file would quietly produce a "logged out" clone, so the clone fails
//! instead (`CloneLocked`) and the user is told to quit the original browser completely.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub struct ProfileInfo {
    /// Folder name (e.g. "Default", "Profile 1").
    pub dir: String,
    /// Name shown in the browser.
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct InstalledBrowser {
    pub name: String,
    pub exe: PathBuf,
    pub data_dir: PathBuf,
    pub profiles: Vec<ProfileInfo>,
}

fn candidates() -> Vec<(&'static str, PathBuf, PathBuf)> {
    let home = dirs::home_dir().unwrap_or_default();
    let mut v = Vec::new();
    #[cfg(target_os = "macos")]
    {
        let sup = home.join("Library/Application Support");
        let app = |a: &str, b: &str| PathBuf::from(format!("/Applications/{a}.app/Contents/MacOS/{b}"));
        v.push(("Google Chrome", app("Google Chrome", "Google Chrome"), sup.join("Google/Chrome")));
        v.push(("Chrome Beta", app("Google Chrome Beta", "Google Chrome Beta"), sup.join("Google/Chrome Beta")));
        v.push(("Microsoft Edge", app("Microsoft Edge", "Microsoft Edge"), sup.join("Microsoft Edge")));
        v.push(("Brave", app("Brave Browser", "Brave Browser"), sup.join("BraveSoftware/Brave-Browser")));
        v.push(("Naver Whale", app("Naver Whale", "Naver Whale"), sup.join("Naver/Whale")));
        v.push(("Vivaldi", app("Vivaldi", "Vivaldi"), sup.join("Vivaldi")));
        v.push(("Chromium", app("Chromium", "Chromium"), sup.join("Chromium")));
    }
    #[cfg(windows)]
    {
        v.extend(win::candidates());
    }
    #[cfg(target_os = "linux")]
    {
        let cfg = home.join(".config");
        v.push(("Google Chrome", PathBuf::from("/usr/bin/google-chrome"), cfg.join("google-chrome")));
        v.push(("Chromium", PathBuf::from("/usr/bin/chromium"), cfg.join("chromium")));
        v.push(("Microsoft Edge", PathBuf::from("/usr/bin/microsoft-edge"), cfg.join("microsoft-edge")));
        v.push(("Brave", PathBuf::from("/usr/bin/brave-browser"), cfg.join("BraveSoftware/Brave-Browser")));
    }
    let _ = &home;
    v
}

/// Windows: finds browsers from candidate install locations (system-wide and per-user) and the
/// App Paths registry key.
///
/// Environment variables (ProgramFiles, …) aren't used, since an MCP host may launch us with a
/// scrubbed environment.
#[cfg(windows)]
mod win {
    use std::path::{Path, PathBuf};

    /// `(display name, install folder relative to Program Files or %LOCALAPPDATA%, executable, data
    /// folder relative to %LOCALAPPDATA%)`.
    ///
    /// The executable lives at `<install root>\<install folder>\Application\<executable>`.
    pub(super) const BROWSERS: &[(&str, &str, &str, &str)] = &[
        ("Google Chrome", r"Google\Chrome", "chrome.exe", r"Google\Chrome\User Data"),
        ("Chrome Beta", r"Google\Chrome Beta", "chrome.exe", r"Google\Chrome Beta\User Data"),
        ("Chrome Dev", r"Google\Chrome Dev", "chrome.exe", r"Google\Chrome Dev\User Data"),
        ("Chrome Canary", r"Google\Chrome SxS", "chrome.exe", r"Google\Chrome SxS\User Data"),
        ("Microsoft Edge", r"Microsoft\Edge", "msedge.exe", r"Microsoft\Edge\User Data"),
        ("Edge Beta", r"Microsoft\Edge Beta", "msedge.exe", r"Microsoft\Edge Beta\User Data"),
        ("Edge Dev", r"Microsoft\Edge Dev", "msedge.exe", r"Microsoft\Edge Dev\User Data"),
        ("Edge Canary", r"Microsoft\Edge SxS", "msedge.exe", r"Microsoft\Edge SxS\User Data"),
        ("Brave", r"BraveSoftware\Brave-Browser", "brave.exe", r"BraveSoftware\Brave-Browser\User Data"),
        ("Naver Whale", r"Naver\Naver Whale", "whale.exe", r"Naver\Naver Whale\User Data"),
        ("Vivaldi", "Vivaldi", "vivaldi.exe", r"Vivaldi\User Data"),
        ("Chromium", "Chromium", "chrome.exe", r"Chromium\User Data"),
    ];

    pub(super) fn candidates() -> Vec<(&'static str, PathBuf, PathBuf)> {
        let local = dirs::data_local_dir().unwrap_or_default();
        // system-wide installs (Program Files, Program Files (x86)) → per-user installs (%LOCALAPPDATA%)
        let mut roots = crate::platform::win::program_files_dirs();
        if !local.as_os_str().is_empty() {
            roots.push(local.clone());
        }
        let found = resolve(&roots, &|exe: &str| crate::platform::win::app_path(exe), &|p: &Path| {
            crate::platform::win::version_info(p).and_then(|v| v.product_name)
        });
        BROWSERS
            .iter()
            .zip(found)
            .filter_map(|(&(name, _, _, data), exe)| exe.map(|e| (name, e, local.join(data))))
            .collect()
    }

    fn lower(p: &Path) -> String {
        p.to_string_lossy().replace('/', "\\").to_lowercase()
    }

    /// Executable paths found, in `BROWSERS` order.
    ///
    /// 1) Checks the standard location under each install root. 2) Fills in the rest from App Paths
    /// (installs on other drives or in custom folders).
    pub(super) fn resolve(
        roots: &[PathBuf],
        app_path: &dyn Fn(&str) -> Option<PathBuf>,
        product_name: &dyn Fn(&Path) -> Option<String>,
    ) -> Vec<Option<PathBuf>> {
        let mut out: Vec<Option<PathBuf>> = BROWSERS
            .iter()
            .map(|&(_, dir, exe, _)| roots.iter().map(|r| r.join(dir).join("Application").join(exe)).find(|p| p.is_file()))
            .collect();
        let mut seen: Vec<&str> = Vec::new();
        for &(_, _, exe, _) in BROWSERS {
            if seen.contains(&exe) {
                continue;
            }
            seen.push(exe);
            let Some(p) = app_path(exe) else { continue };
            let pl = lower(&p);
            if out.iter().flatten().any(|q| lower(q) == pl) {
                continue;
            }
            // tell the channel apart by the path's tail (…\Google\Chrome Beta\Application\chrome.exe)
            let by_suffix = BROWSERS.iter().position(|&(_, dir, e, _)| {
                e == exe && pl.ends_with(&format!("\\{}", lower(&Path::new(dir).join("Application").join(e))))
            });
            // non-standard layout: same executable name with a matching product name, else the first
            // (stable channel)
            let idx = by_suffix.or_else(|| {
                let prod = product_name(&p).unwrap_or_default().to_lowercase();
                BROWSERS
                    .iter()
                    .position(|&(name, _, e, _)| e == exe && !prod.is_empty() && prod.contains(&name.to_lowercase()))
                    .or_else(|| BROWSERS.iter().position(|&(_, _, e, _)| e == exe))
            });
            if let Some(i) = idx {
                if out[i].is_none() {
                    out[i] = Some(p);
                }
            }
        }
        out
    }
}

pub fn detect() -> Vec<InstalledBrowser> {
    candidates()
        .into_iter()
        .filter(|(_, exe, _)| exe.exists())
        .map(|(name, exe, data_dir)| {
            let profiles = list_profiles(&data_dir);
            InstalledBrowser {
                name: name.to_string(),
                exe,
                data_dir,
                profiles,
            }
        })
        .collect()
}

fn list_profiles(data_dir: &Path) -> Vec<ProfileInfo> {
    let mut out = Vec::new();
    if let Ok(s) = fs::read_to_string(data_dir.join("Local State")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            if let Some(map) = v["profile"]["info_cache"].as_object() {
                for (dir, info) in map {
                    if data_dir.join(dir).is_dir() {
                        let name = info["name"].as_str().unwrap_or(dir).to_string();
                        let user = info["user_name"].as_str().unwrap_or("");
                        let label = if user.is_empty() { name } else { format!("{name} ({user})") };
                        out.push(ProfileInfo { dir: dir.clone(), name: label });
                    }
                }
            }
        }
    }
    if out.is_empty() && data_dir.join("Default").is_dir() {
        out.push(ProfileInfo { dir: "Default".into(), name: "Default".into() });
    }
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    out
}

/// Entries left out of the clone (big caches unrelated to login state).
const EXCLUDE: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "DawnCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "GrShaderCache",
    "ShaderCache",
    "GraphiteDawnCache",
    "Service Worker/CacheStorage",
    "Service Worker/ScriptCache",
    "optimization_guide_model_store",
    "optimization_guide_prediction_model_downloads",
    "Download Service",
    "SingletonLock",
    "SingletonSocket",
    "SingletonCookie",
    "RunningChromeVersion",
    "Crashpad",
];

/// File names essential for carrying over login state. If any of them is **locked** and can't be
/// copied, the clone fails (matched by name at any depth, e.g. Electron's Partitions/*/Cookies).
const CRITICAL_NAMES: &[&str] = &["Cookies", "Login Data", "Local State", "Preferences", "Secure Preferences", "Web Data"];

/// Locations of critical files to check for locks before copying starts (relative to the clone
/// root).
#[cfg(windows)]
const CRITICAL_REL: &[&str] = &[
    "Network/Cookies",
    "Cookies",
    "Login Data",
    "Local State",
    "Preferences",
    "Secure Preferences",
    "Web Data",
];

pub struct CloneStats {
    pub files: u64,
    pub bytes: u64,
    /// Files that couldn't be copied (including `locked` below).
    pub skipped: u64,
    /// Of those, files locked by another program (Windows sharing or lock violation).
    ///
    /// Locked critical files aren't counted here; the clone itself fails with `CloneLocked`.
    pub locked: u64,
    /// Relative paths of skipped locked files (only the first few).
    pub locked_paths: Vec<String>,
}

impl CloneStats {
    fn new() -> Self {
        CloneStats { files: 0, bytes: 0, skipped: 0, locked: 0, locked_paths: Vec::new() }
    }

    /// Logs any skipped (non-critical) locked files for diagnostics.
    fn report_locked(&self) {
        if self.locked > 0 {
            eprintln!(
                "[ghostdesk] 다른 프로그램이 사용 중이라 복사하지 못한 파일 {}개 (로그인과 무관): {}",
                self.locked,
                self.locked_paths.join(", ")
            );
        }
    }
}

/// Maximum number of entries kept in the locked file list.
const MAX_LOCKED_PATHS: usize = 20;

/// Error when critical login files are locked because the original browser (app) is running, so
/// the profile can't be cloned.
///
/// Detect it with `anyhow::Error::downcast_ref::<CloneLocked>()`.
#[derive(Debug)]
pub struct CloneLocked {
    /// The locked critical files (relative to the clone source).
    pub files: Vec<String>,
    /// Programs holding the files ("Google Chrome (pid 1234)"). Empty if unknown.
    pub holders: Vec<String>,
    /// `(path, reason)` if the partially copied folder couldn't be removed. That folder is an
    /// incomplete clone missing login data.
    pub leftover: Option<(PathBuf, String)>,
}

impl std::fmt::Display for CloneLocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let files: Vec<String> = self
            .files
            .iter()
            .map(|r| if cfg!(windows) { r.replace('/', "\\") } else { r.clone() })
            .collect();
        writeln!(
            f,
            "원본 브라우저(앱)가 실행 중이라 로그인 정보 파일을 복사할 수 없습니다 (잠긴 파일: {}).",
            files.join(", ")
        )?;
        if !self.holders.is_empty() {
            writeln!(f, "사용 중인 프로그램: {}", self.holders.join(", "))?;
        }
        if let Some((p, e)) = &self.leftover {
            writeln!(
                f,
                "복사하다 만 폴더를 지우지 못했습니다: {} ({e}). 로그인 정보가 빠진 불완전한 복제본이니 쓰지 말고, \
                 다시 시도하기 전에 이 폴더를 지우세요.",
                p.display()
            )?;
        }
        write!(
            f,
            "그 프로그램을 완전히 종료한 뒤 다시 시도하세요. 창을 모두 닫아도 백그라운드에서 계속 실행될 수 있으니 \
             작업 표시줄 알림 영역(트레이) 아이콘에서 '종료'를 누르거나, 브라우저 설정에서 \
             '브라우저를 닫아도 백그라운드 앱 계속 실행'과 Edge 의 '시작 부스트'를 끄세요. \
             (로그인 없이 쓰려면 빈 프로필을 선택하세요)"
        )
    }
}

impl std::error::Error for CloneLocked {}

/// Windows sharing violation (32) or lock violation (33): another program has the file open
/// exclusively.
///
/// Only checked on Windows; on Unix errno 32 and 33 are EPIPE and EDOM.
fn is_lock_error(e: &std::io::Error) -> bool {
    cfg!(windows) && matches!(e.raw_os_error(), Some(32) | Some(33))
}

fn is_critical(rel: &str) -> bool {
    rel.rsplit('/').next().map(|n| CRITICAL_NAMES.contains(&n)).unwrap_or(false)
}

/// Root-relative path ("a/b") → real path.
#[cfg(windows)]
fn join_rel(root: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(root.to_path_buf(), |p, c| p.join(c))
}

/// Opens for reading with every share mode. A sharing violation even then means another program
/// locked it exclusively.
///
/// CopyFile (`fs::copy`) also opens the source with read/write/delete sharing, so files opened
/// with sharing like SQLite databases copy fine and only files opened without sharing fail; that's
/// why copying sticks with plain `fs::copy`.
#[cfg(windows)]
fn open_shared(src: &Path) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(src)
}

/// Checks whether critical files are locked before copying starts (so it doesn't fail after
/// copying several GB).
///
/// Returns `(source path, relative path for display)`.
#[cfg(windows)]
fn preflight(root: &Path, prefix: &str, skip: &dyn Fn(&str) -> bool) -> Vec<(PathBuf, String)> {
    CRITICAL_REL
        .iter()
        .filter(|rel| !skip(rel))
        .filter_map(|rel| {
            let p = join_rel(root, rel);
            if !p.is_file() {
                return None;
            }
            match open_shared(&p) {
                Err(e) if is_lock_error(&e) => Some((p, format!("{prefix}{rel}"))),
                _ => None,
            }
        })
        .collect()
}

/// Locked critical files → error (on Windows also finds the holding programs via Restart
/// Manager).
fn locked_error(locked: Vec<(PathBuf, String)>) -> anyhow::Error {
    let paths: Vec<PathBuf> = locked.iter().map(|(p, _)| p.clone()).collect();
    anyhow::Error::new(CloneLocked {
        files: locked.into_iter().map(|(_, r)| r).collect(),
        holders: lock_holders(&paths),
        leftover: None,
    })
}

/// When the clone failed on locked critical files: removes the partially copied `dirs` and builds
/// the `CloneLocked` error.
///
/// A folder that can't be removed isn't passed over silently but recorded in the error
/// (`leftover`), so the user knows and a half-clone is never used as if it were complete.
/// (`downcast_ref::<CloneLocked>()` still works.)
fn locked_error_cleanup(locked: Vec<(PathBuf, String)>, dirs: &[&Path]) -> anyhow::Error {
    let mut err = locked_error(locked);
    for d in dirs {
        if let Err(e) = crate::platform::fsx::remove_dir_all_retry(d) {
            eprintln!("[ghostdesk] 복사하다 만 폴더를 지우지 못했습니다: {} — {e}", d.display());
            if let Some(cl) = err.downcast_mut::<CloneLocked>() {
                cl.leftover.get_or_insert_with(|| (d.to_path_buf(), e.to_string()));
            }
        }
    }
    err
}

/// Processes holding a file open (Restart Manager: only reads names and pids, never terminates
/// anything).
#[cfg(windows)]
fn lock_holders(paths: &[PathBuf]) -> Vec<String> {
    use crate::platform::win::{from_wide_buf, wide};
    use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
    use windows_sys::Win32::System::RestartManager::{
        RmEndSession, RmGetList, RmRegisterResources, RmStartSession, CCH_RM_SESSION_KEY, RM_PROCESS_INFO,
    };
    let mut out: Vec<String> = Vec::new();
    if paths.is_empty() {
        return out;
    }
    unsafe {
        let mut session = 0u32;
        let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
        if RmStartSession(&mut session, 0, key.as_mut_ptr()) != 0 {
            return out;
        }
        let names: Vec<Vec<u16>> = paths.iter().map(wide).collect();
        let ptrs: Vec<*const u16> = names.iter().map(|w| w.as_ptr()).collect();
        if RmRegisterResources(session, ptrs.len() as u32, ptrs.as_ptr(), 0, std::ptr::null(), 0, std::ptr::null()) == 0 {
            let mut infos: Vec<RM_PROCESS_INFO> = vec![RM_PROCESS_INFO::default(); 8];
            for _ in 0..3 {
                let mut needed = 0u32;
                let mut n = infos.len() as u32;
                let mut reasons = 0u32;
                let r = RmGetList(session, &mut needed, &mut n, infos.as_mut_ptr(), &mut reasons);
                if r == ERROR_MORE_DATA {
                    infos = vec![RM_PROCESS_INFO::default(); needed as usize + 4];
                    continue;
                }
                if r == 0 {
                    for i in infos.iter().take(n as usize) {
                        let name = from_wide_buf(&i.strAppName);
                        let pid = i.Process.dwProcessId;
                        let s = if name.trim().is_empty() { format!("pid {pid}") } else { format!("{} (pid {pid})", name.trim()) };
                        if !out.contains(&s) {
                            out.push(s);
                        }
                    }
                }
                break;
            }
        }
        RmEndSession(session);
    }
    out
}

#[cfg(not(windows))]
fn lock_holders(_paths: &[PathBuf]) -> Vec<String> {
    Vec::new()
}

/// Copies `src_data_dir/<profile_dir>` and `Local State` into `dest` (a new user-data-dir).
///
/// On Windows, if the original browser is running and critical files like cookies are locked,
/// fails with `CloneLocked` and removes the partially copied profile folder (so a logged-out
/// clone doesn't linger looking complete).
pub fn clone_profile(
    src_data_dir: &Path,
    profile_dir: &str,
    dest: &Path,
    progress: &dyn Fn(u64, u64),
) -> Result<CloneStats> {
    let skip = |rel: &str| EXCLUDE.iter().any(|e| rel == *e);
    #[cfg(windows)]
    {
        let mut locked = preflight(src_data_dir, "", &|rel| rel != "Local State");
        locked.extend(preflight(&src_data_dir.join(profile_dir), &format!("{profile_dir}/"), &|rel| {
            rel == "Local State" || skip(rel)
        }));
        if !locked.is_empty() {
            return Err(locked_error(locked));
        }
    }
    fs::create_dir_all(dest).with_context(|| format!("{} 생성 실패", dest.display()))?;
    let mut stats = CloneStats::new();
    let mut critical: Vec<(PathBuf, String)> = Vec::new();
    let ls = src_data_dir.join("Local State");
    if ls.exists() {
        match fs::copy(&ls, dest.join("Local State")) {
            Ok(_) => {}
            Err(e) if is_lock_error(&e) => critical.push((ls.clone(), "Local State".into())),
            Err(e) => return Err(anyhow::Error::new(e).context("Local State 복사 실패")),
        }
    }
    let src = src_data_dir.join(profile_dir);
    anyhow::ensure!(src.is_dir(), "프로필 폴더가 없습니다: {}", src.display());
    if critical.is_empty() {
        copy_tree(&src, &dest.join(profile_dir), &src, &skip, &mut stats, &mut critical, progress)?;
    }
    if !critical.is_empty() {
        let _ = crate::platform::fsx::remove_file_retry(&dest.join("Local State"));
        let locked = critical
            .into_iter()
            .map(|(p, rel)| if rel == "Local State" { (p, rel) } else { (p, format!("{profile_dir}/{rel}")) })
            .collect();
        return Err(locked_error_cleanup(locked, &[&dest.join(profile_dir)]));
    }
    stats.report_locked();
    Ok(stats)
}

/// Copies a whole folder (the source is only read). `exclude_rel` holds root-relative paths
/// ("a/b"); `exclude_names` are names excluded at any depth. Symlinks and sockets are skipped.
///
/// Same locking policy as `clone_profile`: if critical files (cookies, …) are locked, fails with
/// `CloneLocked` and removes `dest`.
pub fn clone_dir(
    src: &Path,
    dest: &Path,
    exclude_rel: &[&str],
    exclude_names: &[&str],
    progress: &dyn Fn(u64, u64),
) -> Result<CloneStats> {
    anyhow::ensure!(src.is_dir(), "원본 폴더가 없습니다: {}", src.display());
    let skip = |rel: &str| {
        exclude_rel.iter().any(|e| rel == *e)
            || rel.rsplit('/').next().map(|n| exclude_names.iter().any(|e| n == *e)).unwrap_or(false)
    };
    #[cfg(windows)]
    {
        let locked = preflight(src, "", &skip);
        if !locked.is_empty() {
            return Err(locked_error(locked));
        }
    }
    fs::create_dir_all(dest).with_context(|| format!("{} 생성 실패", dest.display()))?;
    let mut stats = CloneStats::new();
    let mut critical: Vec<(PathBuf, String)> = Vec::new();
    copy_tree(src, dest, src, &skip, &mut stats, &mut critical, progress)?;
    if !critical.is_empty() {
        return Err(locked_error_cleanup(critical, &[dest]));
    }
    stats.report_locked();
    Ok(stats)
}

/// `critical` collects critical files that couldn't be copied because they're locked. Once there
/// is one the clone will fail anyway, so copying stops early.
fn copy_tree(
    src: &Path,
    dst: &Path,
    root: &Path,
    skip: &dyn Fn(&str) -> bool,
    stats: &mut CloneStats,
    critical: &mut Vec<(PathBuf, String)>,
    progress: &dyn Fn(u64, u64),
) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        if !critical.is_empty() {
            break;
        }
        let Ok(entry) = entry else {
            stats.skipped += 1;
            continue;
        };
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        if skip(&rel) {
            continue;
        }
        let Ok(ft) = entry.file_type() else {
            stats.skipped += 1;
            continue;
        };
        let target = dst.join(entry.file_name());
        if ft.is_symlink() {
            continue;
        } else if ft.is_dir() {
            copy_tree(&path, &target, root, skip, stats, critical, progress)?;
        } else if ft.is_file() {
            match fs::copy(&path, &target) {
                Ok(n) => {
                    stats.files += 1;
                    stats.bytes += n;
                    if stats.files % 200 == 0 {
                        progress(stats.files, stats.bytes);
                    }
                }
                Err(e) if is_lock_error(&e) => {
                    stats.skipped += 1;
                    if is_critical(&rel) {
                        critical.push((path.clone(), rel));
                    } else {
                        stats.locked += 1;
                        if stats.locked_paths.len() < MAX_LOCKED_PATHS {
                            stats.locked_paths.push(rel);
                        }
                    }
                }
                Err(_) => stats.skipped += 1,
            }
        }
    }
    Ok(())
}

/// Workspace root under the app data folder (macOS:
/// ~/Library/Application Support/GhostDesk/workspaces, Windows:
/// %LOCALAPPDATA%\GhostDesk\workspaces).
pub fn workspaces_root() -> PathBuf {
    crate::platform::paths::workspaces_root()
}

/// Workspace name → folder name, keeping only letters, digits, '-' and '_'.
///
/// On Windows it avoids reserved device names (CON, NUL, COM1, …) and shortens the name so
/// Chromium's internal paths stay under 260 characters (a shortened name gets a hash of the
/// original appended so different names don't collide).
pub fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let s = if s.is_empty() { "workspace".into() } else { s };
    #[cfg(windows)]
    let s = windows_safe_slug(name, s);
    s
}

/// Maximum length of a Windows workspace folder name.
#[cfg(windows)]
const SLUG_MAX: usize = 64;

#[cfg(windows)]
fn windows_safe_slug(name: &str, mut s: String) -> String {
    if s.chars().count() > SLUG_MAX {
        // FNV-1a over the whole original name
        let mut h: u32 = 0x811c_9dc5;
        for b in name.bytes() {
            h ^= b as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
        let head: String = s.chars().take(SLUG_MAX - 9).collect();
        s = format!("{head}-{h:08x}");
    }
    if crate::platform::paths::is_reserved_name(&s) {
        s.insert(0, '_');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clone_skips_caches() {
        let tmp = std::env::temp_dir().join(format!("ghostdesk-test-{}", std::process::id()));
        let src = tmp.join("src");
        fs::create_dir_all(src.join("Default/Cache")).unwrap();
        fs::create_dir_all(src.join("Default/Local Storage")).unwrap();
        fs::write(src.join("Local State"), "{}").unwrap();
        fs::write(src.join("Default/Cookies"), "c").unwrap();
        fs::write(src.join("Default/Cache/x"), "big").unwrap();
        fs::write(src.join("Default/Local Storage/y"), "ls").unwrap();
        let dst = tmp.join("dst");
        let st = clone_profile(&src, "Default", &dst, &|_, _| {}).unwrap();
        assert_eq!(st.files, 2);
        assert_eq!(st.locked, 0);
        assert!(dst.join("Default/Cookies").exists());
        assert!(dst.join("Local State").exists());
        assert!(!dst.join("Default/Cache").exists());
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn slug_basic() {
        assert_eq!(slug("작업 1"), "작업_1");
        assert_eq!(slug("a/b\\c"), "a_b_c");
        assert_eq!(slug(""), "workspace");
        assert_eq!(slug("mcp-login-x_y"), "mcp-login-x_y");
    }

    #[cfg(windows)]
    mod windows {
        use super::super::*;
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

        fn tmp(tag: &str) -> PathBuf {
            // must work with paths containing spaces and Hangul
            let d = std::env::temp_dir().join(format!("ghostdesk 복제 {tag}-{}", std::process::id()));
            let _ = crate::platform::fsx::remove_dir_all_retry(&d);
            d
        }

        fn fixture(src: &Path) {
            fs::create_dir_all(src.join("Default/Network")).unwrap();
            fs::create_dir_all(src.join("Default/Local Storage/leveldb")).unwrap();
            fs::write(src.join("Local State"), "{}").unwrap();
            fs::write(src.join("Default/Network/Cookies"), "cookies").unwrap();
            fs::write(src.join("Default/Preferences"), "{}").unwrap();
            fs::write(src.join("Default/Local Storage/leveldb/LOCK"), "").unwrap();
            fs::write(src.join("Default/Local Storage/leveldb/000003.log"), "ls").unwrap();
        }

        /// A file opened by "another program" without sharing (share mode 0).
        fn hold_exclusive(p: &Path) -> fs::File {
            fs::OpenOptions::new().read(true).write(true).share_mode(0).open(p).unwrap()
        }

        #[test]
        fn locked_cookies_fail_clone_profile() {
            let t = tmp("cookies");
            let src = t.join("src");
            fixture(&src);
            let _h = hold_exclusive(&src.join("Default/Network/Cookies"));
            let dst = t.join("dst");
            let err = clone_profile(&src, "Default", &dst, &|_, _| {}).err().expect("잠긴 쿠키면 실패해야 함");
            let cl = err.downcast_ref::<CloneLocked>().expect("CloneLocked 오류여야 함");
            assert_eq!(cl.files, vec!["Default/Network/Cookies".to_string()]);
            // Restart Manager must find the holder (= this test process)
            let me = format!("pid {})", std::process::id());
            assert!(cl.holders.iter().any(|h| h.ends_with(&me)), "holders: {:?}", cl.holders);
            let msg = err.to_string();
            assert!(msg.contains("Default\\Network\\Cookies") && msg.contains("완전히 종료"), "{msg}");
            // no clone that looks complete is left behind
            assert!(!dst.join("Default").exists());
            assert!(!dst.join("Local State").exists());
            drop(_h);
            crate::platform::fsx::remove_dir_all_retry(&t).unwrap();
        }

        /// A critical file locked during the copy (after the upfront check), e.g. Electron's
        /// Partitions/*/Cookies.
        #[test]
        fn locked_cookies_mid_copy_fail_clone_dir() {
            let t = tmp("dir");
            let src = t.join("src");
            fs::create_dir_all(src.join("Partitions/main/Network")).unwrap();
            fs::create_dir_all(src.join("blob_storage")).unwrap();
            fs::write(src.join("Preferences"), "{}").unwrap();
            fs::write(src.join("blob_storage/x"), "x").unwrap();
            fs::write(src.join("Partitions/main/Network/Cookies"), "c").unwrap();
            let _h = hold_exclusive(&src.join("Partitions/main/Network/Cookies"));
            let dst = t.join("dst");
            let err = clone_dir(&src, &dst, &[], &[], &|_, _| {}).err().expect("실패해야 함");
            let cl = err.downcast_ref::<CloneLocked>().expect("CloneLocked");
            assert_eq!(cl.files, vec!["Partitions/main/Network/Cookies".to_string()]);
            assert!(!dst.exists(), "실패한 복제본은 지워야 함");
            drop(_h);
            // once the lock is released the clone succeeds
            let st = clone_dir(&src, &dst, &[], &[], &|_, _| {}).unwrap();
            assert_eq!(st.files, 3);
            assert!(dst.join("Partitions/main/Network/Cookies").exists());
            crate::platform::fsx::remove_dir_all_retry(&t).unwrap();
        }

        /// If the partial folder can't be removed after failing on a locked critical file, it's recorded
        /// in the error instead of passed over (still detectable as CloneLocked).
        #[test]
        fn failed_cleanup_is_reported() {
            let t = tmp("cleanup");
            let src = t.join("src");
            fs::create_dir_all(src.join("Partitions/main/Network")).unwrap();
            fs::write(src.join("Preferences"), "{}").unwrap();
            fs::write(src.join("Partitions/main/Network/Cookies"), "c").unwrap();
            let _h = hold_exclusive(&src.join("Partitions/main/Network/Cookies"));
            // another program (antivirus, …) holds a file inside the clone so it can't be deleted
            let dst = t.join("dst");
            fs::create_dir_all(&dst).unwrap();
            fs::write(dst.join("held"), "x").unwrap();
            let _d = hold_exclusive(&dst.join("held"));
            let err = clone_dir(&src, &dst, &[], &[], &|_, _| {}).err().expect("실패해야 함");
            let cl = err.downcast_ref::<CloneLocked>().expect("CloneLocked");
            assert_eq!(cl.files, vec!["Partitions/main/Network/Cookies".to_string()]);
            assert_eq!(cl.leftover.as_ref().map(|(p, _)| p.as_path()), Some(dst.as_path()));
            let msg = err.to_string();
            assert!(msg.contains("지우지 못했습니다") && msg.contains("불완전한 복제본"), "{msg}");
            // when it can be deleted there's no leftover (existing behavior)
            drop(_d);
            let err = clone_dir(&src, &dst, &[], &[], &|_, _| {}).err().expect("실패해야 함");
            assert!(err.downcast_ref::<CloneLocked>().unwrap().leftover.is_none());
            assert!(!dst.exists());
            drop(_h);
            crate::platform::fsx::remove_dir_all_retry(&t).unwrap();
        }

        /// Locked non-critical files are skipped and counted separately.
        #[test]
        fn locked_noncritical_is_counted() {
            let t = tmp("lock");
            let src = t.join("src");
            fixture(&src);
            let _h = hold_exclusive(&src.join("Default/Local Storage/leveldb/LOCK"));
            let dst = t.join("dst");
            let st = clone_profile(&src, "Default", &dst, &|_, _| {}).unwrap();
            assert_eq!(st.locked, 1);
            assert_eq!(st.skipped, 1);
            assert_eq!(st.locked_paths, vec!["Local Storage/leveldb/LOCK".to_string()]);
            assert_eq!(st.files, 3);
            assert!(dst.join("Default/Network/Cookies").exists());
            drop(_h);
            crate::platform::fsx::remove_dir_all_retry(&t).unwrap();
        }

        /// Files opened with read/write sharing like SQLite (History, Web Data, Login Data, …) aren't
        /// locked; they're copied as is.
        #[test]
        fn shared_open_file_is_copied() {
            let t = tmp("shared");
            let src = t.join("src");
            fixture(&src);
            fs::write(src.join("Default/Web Data"), "webdata").unwrap();
            let _h = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(src.join("Default/Web Data"))
                .unwrap();
            let dst = t.join("dst");
            let st = clone_profile(&src, "Default", &dst, &|_, _| {}).unwrap();
            assert_eq!(st.locked, 0);
            assert_eq!(fs::read_to_string(dst.join("Default/Web Data")).unwrap(), "webdata");
            drop(_h);
            crate::platform::fsx::remove_dir_all_retry(&t).unwrap();
        }

        #[test]
        fn slug_windows_safe() {
            assert_eq!(slug("con"), "_con");
            assert_eq!(slug("NUL"), "_NUL");
            assert_eq!(slug("com1"), "_com1");
            assert_eq!(slug("con.txt"), "con_txt");
            assert_eq!(slug("console"), "console");
            let long_a = "가".repeat(100);
            let long_b = format!("{}나", "가".repeat(99));
            let (a, b) = (slug(&long_a), slug(&long_b));
            assert_eq!(a.chars().count(), SLUG_MAX);
            assert_ne!(a, b, "앞부분이 같아도 서로 다른 이름은 다른 폴더");
            assert_eq!(slug(&long_a), a, "같은 이름은 항상 같은 폴더");
            // an already shortened name passes through unchanged (slug applied twice, as in control.rs)
            assert_eq!(slug(&a), a);
            let short = "a".repeat(SLUG_MAX);
            assert_eq!(slug(&short), short);
        }

        #[test]
        fn resolve_finds_channels() {
            let t = tmp("resolve");
            let pf = t.join("Program Files");
            let local = t.join("Local");
            let other = t.join("D drive");
            let mk = |p: PathBuf| {
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(&p, "").unwrap();
                p
            };
            let chrome = mk(pf.join(r"Google\Chrome\Application\chrome.exe"));
            let canary = mk(local.join(r"Google\Chrome SxS\Application\chrome.exe"));
            let edge = mk(pf.join(r"Microsoft\Edge\Application\msedge.exe"));
            // custom location (only findable through App Paths)
            let brave = mk(other.join(r"Apps\Brave\brave.exe"));
            let beta = mk(other.join(r"Google\Chrome Beta\Application\chrome.exe"));
            let roots = vec![pf.clone(), local.clone()];
            let (brave2, beta2) = (brave.clone(), beta.clone());
            let app_path = move |exe: &str| match exe {
                "brave.exe" => Some(brave2.clone()),
                "chrome.exe" => Some(beta2.clone()),
                _ => None,
            };
            let found = win::resolve(&roots, &app_path, &|_| None);
            let get = |name: &str| {
                let i = win::BROWSERS.iter().position(|b| b.0 == name).unwrap();
                found[i].clone()
            };
            assert_eq!(get("Google Chrome"), Some(chrome));
            assert_eq!(get("Chrome Canary"), Some(canary));
            assert_eq!(get("Microsoft Edge"), Some(edge));
            assert_eq!(get("Brave"), Some(brave));
            assert_eq!(get("Chrome Beta"), Some(beta), "App Paths 경로 끝으로 채널을 가려야 함");
            assert_eq!(get("Chromium"), None);
            assert_eq!(get("Naver Whale"), None);
            crate::platform::fsx::remove_dir_all_retry(&t).unwrap();
        }

        /// Real install discovery: only checks executable paths (never reads user profiles or Local
        /// State).
        #[test]
        fn real_candidates_are_sane() {
            let c = candidates();
            let mut names: Vec<&str> = c.iter().map(|x| x.0).collect();
            names.sort();
            names.dedup();
            assert_eq!(names.len(), c.len(), "브라우저 이름이 겹침: {c:?}");
            for (name, exe, data) in &c {
                assert!(exe.is_file(), "{name}: {}", exe.display());
                assert!(exe.is_absolute() && data.is_absolute());
                assert!(!exe.to_string_lossy().contains('/'), "구분자가 섞임: {}", exe.display());
            }
            let std_chrome = crate::platform::win::program_files_dirs()
                .into_iter()
                .map(|p| p.join(r"Google\Chrome\Application\chrome.exe"))
                .find(|p| p.is_file());
            if let Some(p) = std_chrome {
                assert!(c.iter().any(|x| x.0 == "Google Chrome" && x.1 == p), "{c:?}");
            }
        }
    }
}
