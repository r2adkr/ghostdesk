//! 설치된 Chromium 계열 브라우저와 프로필을 찾고, 프로필을 격리 작업공간으로 복제한다.
//!
//! 원본 프로필은 읽기만 한다. 복제본은 같은 브라우저 바이너리로 실행되므로
//! (macOS 에서는 같은 Keychain 항목으로, Windows 에서는 같은 사용자의 DPAPI·App-Bound 키로)
//! 쿠키/로그인 세션이 그대로 복호화된다.
//!
//! Windows 에서는 브라우저가 실행 중이면 쿠키 DB(`Network\Cookies`)를 배타적으로 잠가 둔다.
//! 이런 핵심 파일을 복사하지 못한 채 넘어가면 '로그아웃된' 복제본이 조용히 만들어지므로,
//! 복제를 실패시키고(`CloneLocked`) 원본 브라우저를 완전히 종료하라고 알린다.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub struct ProfileInfo {
    /// 폴더 이름 (예: "Default", "Profile 1")
    pub dir: String,
    /// 브라우저에 표시되는 이름
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

/// Windows: 설치 위치 후보(시스템 전체·사용자별 설치)와 App Paths 레지스트리로 브라우저를 찾는다.
/// 환경변수(ProgramFiles 등)는 MCP 호스트가 지운 채 실행할 수 있으므로 쓰지 않는다.
#[cfg(windows)]
mod win {
    use std::path::{Path, PathBuf};

    /// (표시 이름, 설치 폴더 — Program Files 나 %LOCALAPPDATA% 기준, 실행 파일, 데이터 폴더 — %LOCALAPPDATA% 기준)
    /// 실행 파일은 `<설치 루트>\<설치 폴더>\Application\<실행 파일>` 에 있다.
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
        // 시스템 전체 설치(Program Files, Program Files (x86)) → 사용자별 설치(%LOCALAPPDATA%)
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

    /// `BROWSERS` 순서대로 찾은 실행 파일 경로.
    /// 1) 설치 루트마다 표준 위치를 본다. 2) 못 찾은 것은 App Paths(다른 드라이브·사용자 지정 폴더 설치)로 채운다.
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
            // 경로 끝(…\Google\Chrome Beta\Application\chrome.exe)으로 채널을 가린다
            let by_suffix = BROWSERS.iter().position(|&(_, dir, e, _)| {
                e == exe && pl.ends_with(&format!("\\{}", lower(&Path::new(dir).join("Application").join(e))))
            });
            // 표준 폴더 구조가 아니면: 같은 실행 파일 이름 중 제품 이름이 맞는 것, 없으면 첫 번째(정식 채널)
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

/// 복제에서 제외할 항목 (용량만 크고 로그인 상태와 무관한 캐시들)
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

/// 로그인 상태를 옮기는 데 꼭 필요한 파일 이름. 이 중 하나라도 **잠겨서** 복사하지 못하면
/// 복제를 실패로 본다 (어느 깊이에 있든 이름으로 판단: Electron 의 Partitions/*/Cookies 등).
const CRITICAL_NAMES: &[&str] = &["Cookies", "Login Data", "Local State", "Preferences", "Secure Preferences", "Web Data"];

/// 복사를 시작하기 전에 잠김을 미리 확인할 핵심 파일 위치 (복제 루트 기준)
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
    /// 복사하지 못한 파일 수 (아래 `locked` 포함)
    pub skipped: u64,
    /// 그중 다른 프로그램이 잠그고 있어서(Windows 공유 위반·잠금 위반) 복사하지 못한 파일 수.
    /// 핵심 파일이 잠긴 경우는 여기 들어가지 않고 복제 자체가 `CloneLocked` 로 실패한다.
    pub locked: u64,
    /// 잠겨서 건너뛴 파일의 상대 경로 (앞의 몇 개만)
    pub locked_paths: Vec<String>,
}

impl CloneStats {
    fn new() -> Self {
        CloneStats { files: 0, bytes: 0, skipped: 0, locked: 0, locked_paths: Vec::new() }
    }

    /// 잠겨서 건너뛴 (핵심이 아닌) 파일이 있으면 진단용으로 남긴다
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

/// 잠긴 파일 목록에 남길 최대 개수
const MAX_LOCKED_PATHS: usize = 20;

/// 원본 브라우저(앱)가 실행 중이라 로그인에 꼭 필요한 파일이 잠겨 있어 복제할 수 없을 때의 오류.
/// `anyhow::Error::downcast_ref::<CloneLocked>()` 로 구분할 수 있다.
#[derive(Debug)]
pub struct CloneLocked {
    /// 잠긴 핵심 파일 (복제 원본 기준 상대 경로)
    pub files: Vec<String>,
    /// 파일을 쥐고 있는 프로그램 ("Google Chrome (pid 1234)"). 알아내지 못하면 빈 목록.
    pub holders: Vec<String>,
    /// 복사하다 만 폴더를 지우지 못했으면 (그 경로, 이유). 그 폴더는 로그인 정보가 빠진 불완전한 복제본이다.
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

/// Windows 공유 위반(32)·잠금 위반(33): 다른 프로그램이 파일을 배타적으로 열고 있다.
/// (Unix 의 errno 32·33 은 EPIPE·EDOM 으로 뜻이 다르므로 Windows 에서만 본다)
fn is_lock_error(e: &std::io::Error) -> bool {
    cfg!(windows) && matches!(e.raw_os_error(), Some(32) | Some(33))
}

fn is_critical(rel: &str) -> bool {
    rel.rsplit('/').next().map(|n| CRITICAL_NAMES.contains(&n)).unwrap_or(false)
}

/// 루트 기준 상대 경로("a/b")를 실제 경로로
#[cfg(windows)]
fn join_rel(root: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(root.to_path_buf(), |p, c| p.join(c))
}

/// 공유 모드를 모두 열고 읽기로 연다. 이것도 공유 위반이면 다른 프로그램이 배타적으로 잠근 것이다.
/// (CopyFile(`fs::copy`)도 읽기·쓰기·삭제 공유로 원본을 열기 때문에, SQLite 처럼 공유로 열어 둔 파일은
/// 그대로 복사되고 공유 없이 연 파일만 실패한다 — 그래서 복사는 `fs::copy` 그대로 쓴다)
#[cfg(windows)]
fn open_shared(src: &Path) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(src)
}

/// 복사를 시작하기 전에 핵심 파일이 잠겨 있는지 확인한다 (수 GB 를 복사한 뒤에야 실패하지 않게).
/// 반환: (원본 경로, 표시용 상대 경로)
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

/// 잠긴 핵심 파일 목록 → 오류 (Windows 에서는 Restart Manager 로 쥐고 있는 프로그램 이름도 찾는다)
fn locked_error(locked: Vec<(PathBuf, String)>) -> anyhow::Error {
    let paths: Vec<PathBuf> = locked.iter().map(|(p, _)| p.clone()).collect();
    anyhow::Error::new(CloneLocked {
        files: locked.into_iter().map(|(_, r)| r).collect(),
        holders: lock_holders(&paths),
        leftover: None,
    })
}

/// 핵심 파일이 잠겨 복제가 실패했을 때: 복사하다 만 `dirs` 를 지우고 `CloneLocked` 오류를 만든다.
/// 지우지 못한 폴더가 있으면 조용히 넘어가지 않고 오류(`leftover`)에 적는다 — 반쪽 복제본이 나중에
/// 완성된 복제본처럼 쓰이지 않게 사용자에게 알린다. (`downcast_ref::<CloneLocked>()` 는 그대로 된다)
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

/// 파일을 열고 있는 프로세스 (Restart Manager: 이름과 pid 만 읽는다. 종료시키지 않는다)
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

/// `src_data_dir/<profile_dir>` 와 `Local State` 를 `dest` (새 user-data-dir) 로 복사한다.
///
/// Windows 에서 원본 브라우저가 실행 중이라 쿠키 등 핵심 파일이 잠겨 있으면 `CloneLocked` 오류로
/// 실패하고, 복사하던 프로필 폴더는 지운다 (로그아웃된 복제본이 완성된 것처럼 남지 않게).
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

/// 폴더 전체를 복사한다 (원본은 읽기만 함). `exclude_rel` 은 루트 기준 상대 경로("a/b"),
/// `exclude_names` 는 어느 깊이에 있든 이름이 같으면 제외할 항목이다. 심볼릭 링크·소켓은 건너뛴다.
///
/// 잠김 정책은 `clone_profile` 과 같다: 핵심 파일(쿠키 등)이 잠겨 있으면 `CloneLocked` 로 실패하고
/// `dest` 를 지운다.
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

/// `critical` 에는 잠겨서 복사하지 못한 핵심 파일을 모은다. 하나라도 생기면 복제가 어차피 실패하므로
/// 더 복사하지 않고 빠져나온다.
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

/// 앱 데이터 폴더 아래의 작업공간 루트
/// (macOS: ~/Library/Application Support/GhostDesk/workspaces, Windows: %LOCALAPPDATA%\GhostDesk\workspaces)
pub fn workspaces_root() -> PathBuf {
    crate::platform::paths::workspaces_root()
}

/// 작업공간 이름 → 폴더 이름. 글자·숫자·-·_ 만 남긴다.
/// Windows 에서는 예약 장치 이름(CON, NUL, COM1 …)을 피하고, Chromium 내부 경로가 260자를 넘지 않게
/// 길이를 줄인다 (줄였으면 원래 이름의 해시를 붙여 서로 다른 이름이 겹치지 않게).
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

/// Windows 작업공간 폴더 이름의 최대 글자 수
#[cfg(windows)]
const SLUG_MAX: usize = 64;

#[cfg(windows)]
fn windows_safe_slug(name: &str, mut s: String) -> String {
    if s.chars().count() > SLUG_MAX {
        // FNV-1a (원래 이름 전체)
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
            // 공백·한글이 들어간 경로에서도 동작해야 한다
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

        /// 다른 프로그램이 공유 없이(share mode 0) 연 파일
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
            // Restart Manager 가 쥐고 있는 프로그램(= 이 테스트 프로세스)을 찾아야 한다
            let me = format!("pid {})", std::process::id());
            assert!(cl.holders.iter().any(|h| h.ends_with(&me)), "holders: {:?}", cl.holders);
            let msg = err.to_string();
            assert!(msg.contains("Default\\Network\\Cookies") && msg.contains("완전히 종료"), "{msg}");
            // 완성된 것처럼 보이는 복제본을 남기지 않는다
            assert!(!dst.join("Default").exists());
            assert!(!dst.join("Local State").exists());
            drop(_h);
            crate::platform::fsx::remove_dir_all_retry(&t).unwrap();
        }

        /// 복사 도중(사전 확인 이후)에 핵심 파일이 잠겨 있는 경우: 예) Electron 의 Partitions/*/Cookies
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
            // 잠금이 풀리면 정상 복제
            let st = clone_dir(&src, &dst, &[], &[], &|_, _| {}).unwrap();
            assert_eq!(st.files, 3);
            assert!(dst.join("Partitions/main/Network/Cookies").exists());
            crate::platform::fsx::remove_dir_all_retry(&t).unwrap();
        }

        /// 잠긴 핵심 파일로 실패한 뒤 복사하다 만 폴더를 지우지 못하면, 조용히 넘어가지 않고 오류에 적는다
        /// (CloneLocked 로 구분되는 것은 그대로)
        #[test]
        fn failed_cleanup_is_reported() {
            let t = tmp("cleanup");
            let src = t.join("src");
            fs::create_dir_all(src.join("Partitions/main/Network")).unwrap();
            fs::write(src.join("Preferences"), "{}").unwrap();
            fs::write(src.join("Partitions/main/Network/Cookies"), "c").unwrap();
            let _h = hold_exclusive(&src.join("Partitions/main/Network/Cookies"));
            // 다른 프로그램(백신 등)이 복제본 안의 파일을 지울 수 없게 쥐고 있는 상황
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
            // 지울 수 있으면 leftover 가 없다 (기존 동작)
            drop(_d);
            let err = clone_dir(&src, &dst, &[], &[], &|_, _| {}).err().expect("실패해야 함");
            assert!(err.downcast_ref::<CloneLocked>().unwrap().leftover.is_none());
            assert!(!dst.exists());
            drop(_h);
            crate::platform::fsx::remove_dir_all_retry(&t).unwrap();
        }

        /// 핵심이 아닌 파일이 잠겨 있으면 건너뛰고 따로 센다
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

        /// SQLite 처럼 읽기·쓰기 공유로 열어 둔 파일(History·Web Data·Login Data 등)은 잠김이 아니다: 그대로 복사된다
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
            // 이미 줄인 이름을 다시 넣어도 그대로 (control.rs 처럼 slug 를 두 번 거치는 경우)
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
            // 사용자 지정 위치 (App Paths 로만 찾을 수 있음)
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

        /// 실제 설치 탐색: 실행 파일 경로만 확인한다 (사용자 프로필·Local State 는 읽지 않음)
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
