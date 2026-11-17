//! 설치와 자동 연결.
//!
//! - GhostDesk 를 고정 위치에 설치한다 (관리자 권한 불필요).
//!   - macOS: ~/Applications/GhostDesk.app (앱 번들)
//!   - Windows: %LOCALAPPDATA%\Programs\GhostDesk\ghostdesk.exe (실행 파일 하나)
//! - Claude Code · Claude 앱 · Codex · Kimi Code · OpenCode · pi 에 GhostDesk MCP 서버를 자동으로 등록한다.
//!   클라이언트의 자체 CLI(`claude mcp add`)가 있으면 그것을 먼저 쓰고,
//!   없거나 실패하면 설정 파일을 서식·키 순서를 보존하며 직접 고친다 (처음 고치기 전의 원본을 백업).
//!   `ghostdesk` 항목 외의 설정은 건드리지 않는다.
//! - 설정 파일을 직접 고칠 때는 원래 권한(예: 0600)과 심볼릭 링크를 그대로 둔다
//!   (Windows 에는 권한 비트가 없어 새 파일은 폴더의 ACL 을 물려받는다). UTF-8 BOM 이 있던 파일은 BOM 도 그대로 둔다.
//!   읽을 수 없거나 해석할 수 없는 파일은 빈 설정으로 덮어쓰지 않고 오류로 멈춘다.
//! - JSON·TOML 은 문자열을 이어 붙이지 않고 serde_json·toml_edit 값으로 만든다
//!   (Windows 경로의 `\` 가 올바르게 이스케이프되도록).
//!
//! 이렇게 해 두면 사람이 따로 연결하지 않아도, 에이전트에게 "GhostDesk 로 …해줘" 라고만 하면 된다.

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    ffi::OsString,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::platform::fsx;

pub const SERVER_NAME: &str = "ghostdesk";
#[cfg(not(windows))]
pub(crate) const BUNDLE_ID: &str = "dev.ghostdesk.app";

/// Codex 의 MCP 도구 호출 시간 제한(초). 기본값 60초로는 desk_open 이 끝나기 전에 끊긴다
/// (사용자 승인 대기 최대 180초 + GUI 실행 대기 + 프로필·앱 데이터 복제).
const CODEX_TOOL_TIMEOUT_SEC: i64 = 600;
/// Codex 의 MCP 서버 시작(initialize) 시간 제한(초).
const CODEX_STARTUP_TIMEOUT_SEC: i64 = 30;
/// OpenCode 의 도구 목록 가져오기 시간 제한(밀리초). 기본 5초로는 `--mcp` 가 GUI 를 띄우는 동안 끊길 수 있다.
const OPENCODE_TIMEOUT_MS: i64 = CODEX_STARTUP_TIMEOUT_SEC * 1000;

// ───────────────────────── 사용자 폴더 ─────────────────────────

/// 설치 위치·설정 파일 위치를 정하는 사용자 폴더와 환경변수.
/// 실제 실행에서는 OS 에서 읽고, 테스트에서는 임시 폴더 아래의 가짜로 바꿔 끼운다.
/// (Windows 의 `dirs::home_dir()` 는 HOME·USERPROFILE 환경변수를 보지 않으므로 환경변수로는 돌릴 수 없다.
///  그래서 명시적으로 바꿔 끼우는 방식을 쓴다.)
#[derive(Clone, Debug)]
struct Bases {
    /// 홈 폴더 (~, %USERPROFILE%)
    home: PathBuf,
    /// %LOCALAPPDATA%
    #[cfg(windows)]
    local: PathBuf,
    /// %APPDATA%
    #[cfg(windows)]
    roaming: PathBuf,
    /// GhostDesk 자체 설정 파일 (자동 연결 등)
    settings: PathBuf,
    /// Some 이면 프로세스 환경변수 대신 이 목록만 본다 (테스트)
    env: Option<Vec<(String, OsString)>>,
}

impl Bases {
    #[cfg_attr(test, allow(dead_code))]
    fn real() -> Bases {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        Bases {
            // dirs 는 Windows 에서 SHGetKnownFolderPath 를 쓰므로 MCP 호스트가 환경변수를 지워도 같은 위치가 나온다
            #[cfg(windows)]
            local: dirs::data_local_dir().unwrap_or_else(|| home.join("AppData").join("Local")),
            #[cfg(windows)]
            roaming: dirs::config_dir().unwrap_or_else(|| home.join("AppData").join("Roaming")),
            settings: crate::platform::paths::settings_path(),
            home,
            env: None,
        }
    }

    fn var(&self, name: &str) -> Option<OsString> {
        match &self.env {
            Some(vars) => vars
                .iter()
                .rev()
                .find(|(k, _)| if cfg!(windows) { k.eq_ignore_ascii_case(name) } else { k == name })
                .map(|(_, v)| v.clone()),
            None => std::env::var_os(name),
        }
    }

    /// 비어 있지 않은 환경변수를 경로로
    fn var_path(&self, name: &str) -> Option<PathBuf> {
        self.var(name).filter(|v| !v.is_empty()).map(PathBuf::from)
    }
}

#[cfg(test)]
thread_local! {
    /// 테스트가 지정한 가짜 사용자 폴더 (스레드마다 따로)
    static TEST_BASES: std::cell::RefCell<Option<Bases>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
impl Bases {
    /// `root` 아래의 가짜 사용자 폴더 (환경변수도 비어 있음)
    fn under(root: &Path) -> Bases {
        Bases {
            home: root.join("home"),
            #[cfg(windows)]
            local: root.join("local"),
            #[cfg(windows)]
            roaming: root.join("roaming"),
            settings: root.join("settings").join("settings.json"),
            env: Some(Vec::new()),
        }
    }
}

fn bases() -> Bases {
    // 테스트에서는 실제 사용자 폴더·환경변수를 절대 보지 않는다: 가짜를 지정하지 않았으면 빈 임시 폴더
    #[cfg(test)]
    {
        TEST_BASES.with(|b| b.borrow().clone()).unwrap_or_else(|| {
            Bases::under(&std::env::temp_dir().join(format!("ghostdesk-install-sandbox-{}", std::process::id())))
        })
    }
    #[cfg(not(test))]
    {
        Bases::real()
    }
}

#[cfg(not(windows))]
fn home() -> PathBuf {
    bases().home
}

/// 테스트에서만: 임시 폴더 아래 경로인가
#[cfg(test)]
fn is_test_path(p: &Path) -> bool {
    let key = |x: &Path| {
        let s = crate::platform::paths::simplify(x).to_string_lossy().into_owned();
        if cfg!(windows) {
            s.replace('/', "\\").to_lowercase()
        } else {
            s
        }
    };
    let t = std::env::temp_dir();
    let roots = [t.clone(), crate::platform::paths::canonicalize(&t).unwrap_or(t)];
    let pk = key(p);
    roots.iter().any(|r| {
        let rk = key(r);
        let rk = rk.trim_end_matches(['\\', '/']);
        !rk.is_empty() && pk.len() > rk.len() && pk.starts_with(rk) && pk[rk.len()..].starts_with(['\\', '/'])
    })
}

/// 쓰기 전에 부른다. 테스트에서 임시 폴더 밖(실제 사용자 파일)을 고치려 하면 아무것도 쓰기 전에 멈춘다.
fn guard_write(p: &Path) {
    #[cfg(test)]
    assert!(is_test_path(p), "테스트가 임시 폴더 밖의 파일을 고치려 했습니다: {}", p.display());
    #[cfg(not(test))]
    let _ = p;
}

/// 사람에게 보여 줄 경로 (Unix: 홈 아래면 ~/…)
fn show_path(p: &Path) -> String {
    #[cfg(unix)]
    {
        if let Ok(rest) = p.strip_prefix(home()) {
            return format!("~/{}", rest.display());
        }
    }
    crate::platform::paths::simplify(p).display().to_string()
}

/// 설치 위치. macOS: ~/Applications/GhostDesk.app, Windows: %LOCALAPPDATA%\Programs\GhostDesk
pub fn stable_app() -> PathBuf {
    #[cfg(windows)]
    {
        bases().local.join("Programs").join("GhostDesk")
    }
    #[cfg(not(windows))]
    {
        home().join("Applications").join("GhostDesk.app")
    }
}

/// 설치본 실행 파일 (클라이언트 연결 설정에 쓰는 경로)
pub fn stable_exe() -> PathBuf {
    installed_exe(&stable_app())
}

/// 설치 폴더(번들) 안의 실행 파일
fn installed_exe(app: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        app.join(crate::platform::paths::EXE_NAME)
    }
    #[cfg(not(windows))]
    {
        app.join("Contents/MacOS/ghostdesk")
    }
}

fn settings_path() -> PathBuf {
    bases().settings
}

/// GhostDesk 설정 파일을 읽는다 (BOM 이 있어도 읽음). (값, BOM 이 있었는지)
fn read_settings(p: &Path) -> Option<(Value, bool)> {
    let s = std::fs::read_to_string(p).ok()?;
    let (body, bom) = fsx::strip_bom(&s);
    serde_json::from_str::<Value>(body).ok().map(|v| (v, bom))
}

/// 자동 연결 설정 (기본 켜짐). GHOSTDESK_NO_AUTOCONNECT=1 이면 이번 실행은 끔.
pub fn auto_connect_enabled() -> bool {
    if std::env::var("GHOSTDESK_NO_AUTOCONNECT").map(|v| v == "1").unwrap_or(false) {
        return false;
    }
    read_settings(&settings_path()).and_then(|(v, _)| v["auto_connect"].as_bool()).unwrap_or(true)
}

pub fn set_auto_connect(on: bool) -> Result<()> {
    let p = settings_path();
    guard_write(&p);
    let (mut v, bom) = read_settings(&p).unwrap_or((json!({}), false));
    if !v.is_object() {
        v = json!({});
    }
    v["auto_connect"] = json!(on);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut text = serde_json::to_string_pretty(&v)?;
    if bom {
        text.insert(0, '\u{FEFF}');
    }
    std::fs::write(&p, text)?;
    Ok(())
}

// ───────────────────────── 설치 ─────────────────────────

#[cfg(not(windows))]
fn current_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors().find(|p| p.extension().map(|e| e == "app").unwrap_or(false)).map(|p| p.to_path_buf())
}

/// 번들의 Info.plist (버전은 이 빌드의 Cargo.toml 버전)
#[cfg(not(windows))]
fn info_plist() -> String {
    let v = env!("CARGO_PKG_VERSION");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>GhostDesk</string>
<key>CFBundleDisplayName</key><string>GhostDesk</string>
<key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
<key>CFBundleExecutable</key><string>ghostdesk</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleVersion</key><string>{v}</string>
<key>CFBundleShortVersionString</key><string>{v}</string>
<key>LSMinimumSystemVersion</key><string>13.0</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
"#
    )
}

/// 번들을 마무리한다: 원본 지문을 적고(서명 봉인에 들어가야 하므로 서명 전에) ad-hoc 서명.
/// 지문을 못 구했으면 옛 지문이 남지 않게 지운다.
#[cfg(not(windows))]
fn seal_bundle(bundle: &Path, src_hash: Option<&str>) -> Result<()> {
    match src_hash {
        Some(h) => write_source_stamp(bundle, h)?,
        None => {
            let _ = std::fs::remove_file(source_stamp_path(bundle));
        }
    }
    let _ = run(
        Command::new("/usr/bin/codesign").args(["--force", "-s", "-", "--identifier", BUNDLE_ID]).arg(bundle),
        Duration::from_secs(120),
    );
    Ok(())
}

/// 실행 파일 `exe` 하나로 GhostDesk.app 번들을 `dest` 에 새로 만든다 (Info.plist · 실행 파일 · 원본 지문 · 서명).
/// 설치(번들 밖에서 실행했을 때)와 릴리스 패키징(`package_macos`)이 같이 쓴다.
#[cfg(not(windows))]
pub fn build_bundle(exe: &Path, dest: &Path) -> Result<()> {
    let _ = std::fs::remove_dir_all(dest);
    let macos = dest.join("Contents/MacOS");
    std::fs::create_dir_all(&macos)?;
    std::fs::write(dest.join("Contents/Info.plist"), info_plist())?;
    std::fs::copy(exe, macos.join("ghostdesk")).with_context(|| format!("{} 를 복사하지 못했습니다", exe.display()))?;
    seal_bundle(dest, sha256_file(exe).ok().as_deref())
}

/// `ghostdesk --package-macos <실행 파일> <출력.zip>` — 릴리스용 GhostDesk.app 을 만들어 zip 으로 묶는다
/// (zip 안에는 GhostDesk.app 하나. 앱 안 업데이트(`update.rs`)가 이 모양을 풀어 바꿔 넣는다).
#[cfg(not(windows))]
pub fn package_macos(args: &[String]) -> Result<()> {
    let [exe, out] = args else { anyhow::bail!("사용법: ghostdesk --package-macos <실행 파일> <출력.zip>") };
    let work = std::env::temp_dir().join(format!("ghostdesk-package-{}", std::process::id()));
    let app = work.join("GhostDesk.app");
    let res = (|| {
        build_bundle(Path::new(exe), &app)?;
        let _ = std::fs::remove_file(out);
        run(Command::new("/usr/bin/ditto").args(["-c", "-k", "--keepParent"]).arg(&app).arg(out), Duration::from_secs(300))?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    res
}

/// 설치본 안에 "어떤 원본 실행 파일에서 설치했는지" 를 적어 두는 파일.
/// macOS: codesign 이 설치본의 실행 파일을 다시 쓰므로 설치본과 원본을 바이트로 비교할 수는 없다.
/// 그래서 서명하기 전 원본의 SHA-256 을 여기에 적어 두고, 다음 실행 때 그것과 비교한다.
/// Windows: 서명은 하지 않지만, 시작할 때마다 설치본까지 다시 해시하지 않도록 같은 방식을 쓴다.
const SOURCE_STAMP: &str = if cfg!(windows) { "ghostdesk.src-sha256" } else { "Contents/Resources/ghostdesk.src-sha256" };

fn source_stamp_path(app: &Path) -> PathBuf {
    app.join(SOURCE_STAMP)
}

/// `app` 이 원본 해시 `src_hash` 로 설치된 것인가 (실행 파일이 있고 기록된 지문이 같음)
fn installed_from(app: &Path, src_hash: &str) -> bool {
    installed_exe(app).is_file()
        && std::fs::read_to_string(source_stamp_path(app)).map(|s| s.trim() == src_hash).unwrap_or(false)
}

/// 설치본(macOS 는 staging 번들) 안에 원본 지문을 적는다. macOS 는 codesign 전에 불러야 서명 봉인에 포함된다.
fn write_source_stamp(app: &Path, src_hash: &str) -> Result<()> {
    let p = source_stamp_path(app);
    guard_write(&p);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&p, format!("{src_hash}\n"))?;
    Ok(())
}

/// 지금 실행 중인 GhostDesk 를 고정 위치(`stable_app`)에 설치(또는 갱신)하고 그 실행 파일 경로를 돌려준다.
/// 이미 같은 원본에서 설치한 것이면 아무것도 하지 않는다. 실행 중인 설치본도 안전하게 갱신한다
/// (macOS: 새 번들 폴더로 바꿔치기, Windows: 실행 중인 exe 를 옆으로 옮긴 뒤 새 exe 를 제자리에).
pub fn install_app() -> Result<PathBuf> {
    let me = std::env::current_exe().context("실행 파일 경로를 알 수 없습니다")?;
    #[cfg(windows)]
    {
        install_exe_into(&me, &stable_app())
    }
    #[cfg(not(windows))]
    {
        let target_app = stable_app();
        let target_exe = stable_exe();
        if me.canonicalize().ok() == target_exe.canonicalize().ok() {
            return Ok(target_exe);
        }
        // 서명되지 않은 원본의 지문으로 비교 (설치본은 codesign 으로 바이트가 바뀌어 있음)
        let src_hash = sha256_file(&me).ok();
        if let Some(h) = &src_hash {
            if installed_from(&target_app, h) {
                return Ok(target_exe);
            }
        }
        let parent = target_app.parent().ok_or_else(|| anyhow!("설치 경로 오류"))?;
        guard_write(parent);
        std::fs::create_dir_all(parent)?;
        let staging = parent.join(".GhostDesk.app.installing");
        let _ = std::fs::remove_dir_all(&staging);
        match current_bundle() {
            Some(bundle) => {
                run(Command::new("/usr/bin/ditto").arg(&bundle).arg(&staging), Duration::from_secs(120))?;
                // 지문은 서명 전에 기록한다 (서명 뒤에 Resources 를 바꾸면 번들 봉인이 깨짐)
                seal_bundle(&staging, src_hash.as_deref())?;
            }
            None => build_bundle(&me, &staging)?,
        }
        // 바꿔치기: 기존 설치본을 옆으로 옮긴 뒤 새 것을 제자리에 (실행 중인 기존 프로세스는 옛 파일을 계속 씀)
        let old = parent.join(".GhostDesk.app.old");
        let _ = std::fs::remove_dir_all(&old);
        if target_app.exists() {
            std::fs::rename(&target_app, &old).context("기존 설치본을 옮기지 못했습니다")?;
        }
        std::fs::rename(&staging, &target_app).context("새 설치본을 옮기지 못했습니다")?;
        let _ = std::fs::remove_dir_all(&old);
        // LaunchServices 에 등록 (Spotlight·open 에서 찾을 수 있게)
        let _ = run(
            Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
                .arg("-f")
                .arg(&target_app),
            Duration::from_secs(30),
        );
        Ok(target_exe)
    }
}

/// Windows 설치: 실행 파일 `me` 를 `dir` 에 `ghostdesk.exe` 로 넣는다.
///
/// 설치본이 실행 중일 수 있다 (GhostDesk 창, 또는 Claude·Codex 가 띄운 `ghostdesk.exe --mcp`).
/// 실행 중인 exe 는 지우거나 덮어쓸 수 없지만 이름은 바꿀 수 있으므로, 옆으로 옮긴 뒤
/// (`ghostdesk.exe.old-<시각>`) 새 exe 를 제자리에 놓는다. 폴더 통째 바꿔치기는 하지 않는다
/// (실행 중인 파일이 든 폴더는 옮길 수 없어 연결 전체가 실패한다). 옮겨 둔 옛 exe 는 그 프로세스가
/// 끝난 뒤 다음 설치 때 지운다.
#[cfg(windows)]
fn install_exe_into(me: &Path, dir: &Path) -> Result<PathBuf> {
    let target = installed_exe(dir);
    if target.is_file() && (crate::platform::paths::same_path(me, &target) || is_displaced_copy(me, dir)) {
        // 설치본 자신이거나, 바꿔치기로 옆에 옮겨진 옛 설치본에서 실행 중 (새 설치본을 옛것으로 되돌리지 않음)
        return Ok(target);
    }
    let src_hash = sha256_file(me).ok();
    if let Some(h) = &src_hash {
        if installed_from(dir, h) && same_len(me, &target) {
            sweep_install_leftovers(dir);
            return Ok(target);
        }
    }
    guard_write(dir);
    std::fs::create_dir_all(dir).with_context(|| format!("설치 폴더 {} 을(를) 만들지 못했습니다", dir.display()))?;
    sweep_install_leftovers(dir);
    // 같은 폴더에 먼저 복사해 둔다 (바꿔치기는 이름 바꾸기만으로 끝나도록). 동시에 설치하는 다른 GhostDesk 와
    // 겹치지 않게 pid 를 붙인다.
    let staging = with_suffix(&target, &format!(".installing-{}", std::process::id()));
    let _ = fsx::remove_file_retry(&staging);
    if let Err(e) = std::fs::copy(me, &staging) {
        let _ = std::fs::remove_file(&staging);
        return Err(e).with_context(|| format!("{} 을(를) {} 로 복사하지 못했습니다", me.display(), staging.display()));
    }
    // 옛 지문을 먼저 지운다 (바꿔치기 도중 끊기면 다음 실행 때 다시 설치하도록)
    let _ = fsx::remove_file_retry(&source_stamp_path(dir));
    if let Err(e) = swap_in(&staging, &target) {
        let _ = std::fs::remove_file(&staging);
        return Err(e);
    }
    // 지문 기록은 실패해도 설치는 끝난 것 (다음 실행 때 한 번 더 복사할 뿐)
    if let Some(h) = &src_hash {
        let _ = write_source_stamp(dir, h);
    }
    sweep_install_leftovers(dir);
    Ok(target)
}

/// `staging` 을 `target` 자리에 놓는다 (Windows).
#[cfg(windows)]
pub(crate) fn swap_in(staging: &Path, target: &Path) -> Result<()> {
    // 실행 중이 아니면 한 번에 바꿔치기 (MoveFileExW + REPLACE_EXISTING, 원자적)
    let first = match std::fs::rename(staging, target) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    if std::fs::symlink_metadata(target).is_err() {
        // 대상이 없는데 실패: 백신 검사 등으로 새 파일이 잠깐 잠긴 경우 → 재시도
        return fsx::rename_replace(staging, target)
            .with_context(|| format!("새 설치본을 {} 로 옮기지 못했습니다 ({first})", target.display()));
    }
    // 실행 중인 설치본: 이름만 바꿔 옆으로 치운다 (실행 중인 exe 도 이름 바꾸기는 허용됨)
    let old = old_exe_name(target);
    fsx::rename_replace(target, &old)
        .with_context(|| format!("기존 설치본 {} 을(를) 옆으로 옮기지 못했습니다 ({first})", target.display()))?;
    if let Err(e) = fsx::rename_replace(staging, target) {
        // 되돌린다 (설치본이 없는 채로 두지 않음)
        let _ = std::fs::rename(&old, target);
        return Err(e).with_context(|| format!("새 설치본을 {} 로 옮기지 못했습니다", target.display()));
    }
    Ok(())
}

/// 옆으로 옮길 옛 설치본 이름: ghostdesk.exe.old-<밀리초>
#[cfg(windows)]
fn old_exe_name(target: &Path) -> PathBuf {
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    (0u32..)
        .map(|n| with_suffix(target, &if n == 0 { format!(".old-{ts}") } else { format!(".old-{ts}-{n}") }))
        .find(|p| std::fs::symlink_metadata(p).is_err())
        .unwrap_or_else(|| with_suffix(target, ".old"))
}

/// `me` 가 설치 폴더 안의 옆으로 옮겨진 옛 설치본(ghostdesk.exe.old-*)인가
#[cfg(windows)]
fn is_displaced_copy(me: &Path, dir: &Path) -> bool {
    let name = me.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    name.starts_with(&format!("{}.old-", crate::platform::paths::EXE_NAME))
        && me.parent().map(|p| crate::platform::paths::same_path(p, dir)).unwrap_or(false)
}

#[cfg(windows)]
fn same_len(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.len() == y.len(),
        _ => false,
    }
}

/// 지난 설치가 남긴 파일 정리 (Windows, 실패는 무시):
/// 옆으로 옮겨 둔 옛 실행 파일(아직 실행 중이면 지워지지 않으므로 다음에 다시)과 중단된 복사본.
#[cfg(windows)]
fn sweep_install_leftovers(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let exe = crate::platform::paths::EXE_NAME;
    let old_prefix = format!("{exe}.old");
    let staging_prefix = format!("{exe}.installing-");
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if name.starts_with(&old_prefix) {
            let _ = std::fs::remove_file(e.path());
        } else if let Some(pid) = name.strip_prefix(&staging_prefix) {
            // 다른 GhostDesk 가 지금 설치 중인 복사본은 건드리지 않는다
            let busy = pid.parse::<u32>().map(|p| p != std::process::id() && crate::platform::proc::pid_alive(p)).unwrap_or(false);
            if !busy {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

// ───────────────────────── SHA-256 ─────────────────────────
// 의존성을 늘리지 않으려고 직접 구현 (설치 여부 판단용 지문).

struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
    0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Sha256 {
    fn new() -> Self {
        Sha256 {
            h: [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19],
            buf: [0; 64],
            buf_len: 0,
            total: 0,
        }
    }

    fn block(h: &mut [u32; 8], b: &[u8]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut bb, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(SHA256_K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & bb) ^ (a & c) ^ (bb & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = bb;
            bb = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, bb, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let n = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + n].copy_from_slice(&data[..n]);
            self.buf_len += n;
            data = &data[n..];
            if self.buf_len < 64 {
                return;
            }
            let buf = self.buf;
            Self::block(&mut self.h, &buf);
            self.buf_len = 0;
        }
        let mut chunks = data.chunks_exact(64);
        for c in &mut chunks {
            Self::block(&mut self.h, c);
        }
        let rest = chunks.remainder();
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    fn finish_hex(mut self) -> String {
        let bits = self.total.wrapping_mul(8);
        let mut pad = vec![0x80u8];
        let used = (self.buf_len + 1) % 64;
        let zeros = if used <= 56 { 56 - used } else { 120 - used };
        pad.extend(std::iter::repeat(0u8).take(zeros));
        pad.extend_from_slice(&bits.to_be_bytes());
        let total = self.total;
        self.update(&pad);
        self.total = total;
        debug_assert_eq!(self.buf_len, 0);
        self.h.iter().map(|x| format!("{x:08x}")).collect()
    }
}

pub(crate) fn sha256_file(p: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finish_hex())
}

// ───────────────────────── 보조 ─────────────────────────

/// 명령을 시간 제한과 함께 실행하고 stdout 을 돌려준다.
#[cfg(not(windows))]
fn run(cmd: &mut Command, timeout: Duration) -> Result<String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let start = Instant::now();
    loop {
        if let Some(st) = child.try_wait()? {
            let out = child.wait_with_output()?;
            let so = String::from_utf8_lossy(&out.stdout).to_string();
            if st.success() {
                return Ok(so);
            }
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim().to_string() + &so);
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("시간 초과");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 명령을 시간 제한과 함께 실행하고 stdout 을 돌려준다 (Windows).
/// - 콘솔 창을 띄우지 않고(CREATE_NO_WINDOW) Job 객체에 넣어 띄운다: 시간 초과 때 cmd.exe 뿐 아니라
///   그 아래 node 같은 자손까지 함께 끝낸다. 정상 종료 뒤 남은 자손도 정리한다.
/// - 출력은 따로 읽는다 (출력이 많아 파이프가 차면 자식이 멈추므로).
/// - cmd.exe 의 오류 문구는 UTF-8 이 아니라 OEM 코드 페이지(한국어 Windows: CP949)이므로 맞게 풀어 쓴다.
#[cfg(windows)]
fn run(cmd: &mut Command, timeout: Duration) -> Result<String> {
    use crate::platform::proc;
    use std::sync::mpsc;
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = proc::spawn_group(cmd)?;
    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    if let Some(mut p) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = p.read_to_end(&mut b);
            let _ = tx.send((false, b));
        });
    }
    if let Some(mut p) = child.stderr.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = p.read_to_end(&mut b);
            let _ = tx.send((true, b));
        });
    }
    drop(tx);
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Ok(st),
            Ok(None) if start.elapsed() > timeout => break Err(anyhow!("시간 초과")),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => break Err(e.into()),
        }
    };
    // 끝났든 시간 초과든 Job 을 닫아 남은 자손까지 정리한다 (그래야 파이프도 닫힌다)
    proc::kill_group(&mut child);
    let (mut so, mut se) = (Vec::new(), Vec::new());
    // Job 을 벗어난 손자가 파이프를 쥐고 있어도 무한정 기다리지 않는다
    let deadline = Instant::now() + Duration::from_secs(3);
    for _ in 0..2 {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((true, b)) => se = b,
            Ok((false, b)) => so = b,
            Err(_) => break,
        }
    }
    let st = status?;
    let so = decode_output(&so);
    if st.success() {
        return Ok(so);
    }
    bail!("{}", decode_output(&se).trim().to_string() + &so)
}

/// 자식 프로세스 출력 → 문자열. UTF-8 이 아니면 OEM 코드 페이지(cmd.exe 의 오류 문구 등)로 풀어 본다.
#[cfg(windows)]
fn decode_output(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => oem_to_string(b).unwrap_or_else(|| String::from_utf8_lossy(b).into_owned()),
    }
}

#[cfg(windows)]
fn oem_to_string(b: &[u8]) -> Option<String> {
    // windows-sys 의 Win32_Globalization 기능을 따로 켜지 않으려고 이 함수 하나만 직접 선언한다
    #[link(name = "kernel32")]
    extern "system" {
        fn MultiByteToWideChar(codepage: u32, flags: u32, src: *const u8, src_len: i32, dst: *mut u16, dst_len: i32) -> i32;
    }
    const CP_OEMCP: u32 = 1;
    let len = i32::try_from(b.len()).ok().filter(|n| *n > 0)?;
    unsafe {
        let n = MultiByteToWideChar(CP_OEMCP, 0, b.as_ptr(), len, std::ptr::null_mut(), 0);
        if n <= 0 {
            return None;
        }
        let mut w = vec![0u16; n as usize];
        let m = MultiByteToWideChar(CP_OEMCP, 0, b.as_ptr(), len, w.as_mut_ptr(), n);
        if m <= 0 {
            return None;
        }
        w.truncate(m as usize);
        Some(String::from_utf16_lossy(&w))
    }
}

/// 클라이언트 CLI 를 실행할 때 쓸 PATH.
/// Finder·Dock·`open -g` 로 실행한 GUI 는 launchd 의 PATH(/usr/bin:/bin:/usr/sbin:/sbin)만 받는다.
/// 그러면 `#!/usr/bin/env node` 로 시작하는 claude CLI(npm 설치)가 node 를 찾지 못해 실패하므로,
/// CLI 자신의 폴더(nvm 등은 node 가 같은 폴더에 있음)와 흔한 설치 위치를 앞에 붙인다.
#[cfg(not(windows))]
fn cli_path_env(cli: &Path) -> OsString {
    let b = bases();
    let h = b.home.clone();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = cli.parent() {
        dirs.push(d.to_path_buf());
    }
    dirs.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        h.join(".local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/sbin"),
    ]);
    if let Some(cur) = b.var("PATH") {
        dirs.extend(std::env::split_paths(&cur));
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| !d.as_os_str().is_empty() && seen.insert(d.clone()));
    std::env::join_paths(dirs).unwrap_or_else(|_| OsString::from("/usr/bin:/bin:/usr/sbin:/sbin"))
}

/// 클라이언트 CLI 를 실행할 때 쓸 PATH (Windows).
/// CLI 자신의 폴더(npm 의 claude.cmd 는 같은 폴더의 node.exe 를 먼저 찾음) + 물려받은 PATH
/// + 이 사용자의 현재 PATH(레지스트리 기준: GhostDesk 를 켠 뒤 설치한 node 도 찾도록, 그리고 MCP 호스트가
/// 줄인 환경으로 GhostDesk 가 떴어도 되도록). Unix 의 고정 폴더(/usr/bin 등)는 Windows 에서 의미가 없다.
#[cfg(windows)]
fn cli_path_env(cli: &Path) -> OsString {
    let b = bases();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = cli.parent() {
        dirs.push(d.to_path_buf());
    }
    if let Some(cur) = b.var("PATH") {
        dirs.extend(std::env::split_paths(&cur));
    }
    // 테스트(가짜 환경)에서는 실제 사용자 환경을 섞지 않는다
    if b.env.is_none() {
        if let Some(env) = crate::platform::win::user_environment() {
            if let Some((_, p)) = env.iter().find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case("PATH")) {
                dirs.extend(std::env::split_paths(p));
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| {
        let key = d.to_string_lossy().to_lowercase().trim_end_matches(['\\', '/']).to_string();
        !key.is_empty() && seen.insert(key)
    });
    std::env::join_paths(&dirs).unwrap_or_else(|_| b.var("PATH").unwrap_or_default())
}

/// CLI 실행 명령 (PATH 를 명시적으로 지정).
/// Windows: npm 이 설치한 `claude.cmd` 같은 배치 파일은 Rust std 가 `cmd.exe /e:ON /v:OFF /d /c "…"` 로 실행하며
/// 인자를 cmd 규칙에 맞게 따옴표·`%` 이스케이프한다 (Rust 1.77+, CVE-2024-24576 대응. 줄바꿈 등 안전하게
/// 넘길 수 없는 인자는 실행하지 않고 오류). cmd.exe 는 PATH 가 아니라 시스템 폴더의 것을 쓴다. 콘솔 창은 띄우지 않는다.
fn cli_command(cli: &Path) -> Command {
    let mut c = Command::new(cli);
    c.env("PATH", cli_path_env(cli));
    #[cfg(windows)]
    {
        // cmd.exe 는 이름만 적힌 명령(npm 의 claude.cmd 가 부르는 "node" 등)을 PATH 보다 현재 폴더에서 먼저 찾는다.
        // GhostDesk 가 물려받은 현재 폴더(처음 실행한 Downloads, --connect 를 실행한 폴더 등)에 심어 둔
        // node.exe / node.cmd 가 실행되지 않게, CLI 자신의 폴더에서 실행하고 현재 폴더 검색도 끈다.
        // (CLI 폴더는 cli_path_env 가 PATH 맨 앞에 넣으므로 그 폴더의 프로그램은 그대로 찾는다)
        let dir = cli.parent().filter(|d| d.is_dir()).map(Path::to_path_buf).or_else(|| Some(bases().home).filter(|h| h.is_dir()));
        if let Some(dir) = dir {
            c.current_dir(dir);
        }
        c.env("NoDefaultCurrentDirectoryInExePath", "1");
        crate::platform::proc::hide_console(&mut c);
    }
    c
}

#[cfg(not(windows))]
fn find_cli(name: &str) -> Option<PathBuf> {
    let b = bases();
    let h = b.home.clone();
    let mut cands = vec![
        h.join(".local/bin").join(name),
        h.join(format!(".{name}/local/{name}")),
        h.join(format!(".{name}/bin/{name}")), // opencode 설치 스크립트 (~/.opencode/bin)
        PathBuf::from("/opt/homebrew/bin").join(name),
        PathBuf::from("/usr/local/bin").join(name),
    ];
    // 터미널에서 실행했다면 PATH 에도 있을 수 있음 (nvm, ~/.npm-global, bun 등)
    if let Some(cur) = b.var("PATH") {
        cands.extend(std::env::split_paths(&cur).filter(|d| d.is_absolute()).map(|d| d.join(name)));
    }
    // 테스트에서는 실제로 설치된 CLI 를 절대 실행하지 않는다 (실제 설정을 고치게 되므로)
    #[cfg(test)]
    cands.retain(|p| is_test_path(p));
    cands.into_iter().find(|p| p.is_file())
}

/// 실행할 수 있는 확장자, 고르는 순서. 이 중 PATHEXT 에 있는 것만 본다.
/// 확장자 없는 파일(npm 이 함께 까는 sh 스크립트 `claude`)과 .ps1 은 CreateProcess 로 실행할 수 없으므로 고르지 않는다.
#[cfg(windows)]
const CLI_EXTS: [&str; 4] = [".exe", ".cmd", ".bat", ".com"];

/// Windows: 흔한 설치 위치와 PATH 에서 `name.exe` → `name.cmd` → `name.bat` 순으로 찾는다.
#[cfg(windows)]
fn find_cli(name: &str) -> Option<PathBuf> {
    let b = bases();
    let pathext = b
        .var("PATHEXT")
        .map(|v| v.to_string_lossy().to_lowercase())
        .filter(|v| !v.trim().is_empty())
        // MCP 호스트가 환경을 줄여 PATHEXT 가 없을 수 있다 → cmd.exe 의 기본값
        .unwrap_or_else(|| ".com;.exe;.bat;.cmd".into());
    let exts: Vec<&str> = CLI_EXTS.iter().copied().filter(|e| pathext.split(';').any(|x| x.trim() == *e)).collect();
    let mut dirs = vec![
        b.home.join(".local").join("bin"),                       // Claude Code 기본 설치 (claude.exe)
        b.roaming.join("npm"),                                   // npm -g (claude.cmd, codex.cmd)
        b.local.join("Microsoft").join("WinGet").join("Links"),  // winget
        b.home.join("scoop").join("shims"),                      // scoop
        b.local.join("pnpm"),                                    // pnpm -g
        b.home.join(".bun").join("bin"),                         // bun
        b.local.join("Volta").join("bin"),                       // volta
        b.home.join(format!(".{name}")).join("local"),           // claude 의 예전 로컬 설치
        b.home.join(format!(".{name}")).join("bin"),             // opencode 설치 스크립트 (~/.opencode/bin)
    ];
    if let Some(cur) = b.var("PATH") {
        dirs.extend(std::env::split_paths(&cur).filter(|d| d.is_absolute()));
    }
    for d in dirs {
        for e in &exts {
            let p = d.join(format!("{name}{e}"));
            // 테스트에서는 실제로 설치된 CLI 를 절대 실행하지 않는다 (실제 설정을 고치게 되므로)
            #[cfg(test)]
            if !is_test_path(&p) {
                continue;
            }
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// `p` 의 경로 끝에 접미사를 붙인 형제 경로 (확장자를 바꾸지 않음)
fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// 심볼릭 링크를 끝까지 따라간 실제 파일 경로 (dotfiles 링크 보존용). 파일이 없으면 링크가 가리키는 경로.
/// (Windows 에서도 `\\?\` 접두사 없는 보통 경로를 돌려준다)
fn resolve_target(p: &Path) -> PathBuf {
    if let Ok(r) = crate::platform::paths::canonicalize(p) {
        return r;
    }
    // 대상이 아직 없는 (끊어진) 링크: 직접 따라간다
    let mut cur = p.to_path_buf();
    for _ in 0..32 {
        match std::fs::read_link(&cur) {
            Ok(t) => {
                cur = if t.is_absolute() { t } else { cur.parent().unwrap_or(Path::new("/")).join(t) };
            }
            Err(_) => break,
        }
    }
    cur
}

/// 원래 파일의 권한 (없으면 0600: 비밀이 들어 있을 수 있는 설정 파일이므로 좁게).
/// Windows 에는 권한 비트가 없다 (새 파일은 폴더의 ACL — 본인·SYSTEM·Administrators — 을 물려받음).
fn file_mode(real: &Path) -> u32 {
    #[cfg(unix)]
    {
        std::fs::metadata(real).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o600)
    }
    #[cfg(not(unix))]
    {
        let _ = real;
        0o600
    }
}

/// 새 파일을 만든다 (Unix: 주어진 권한으로, Windows: 폴더의 ACL 을 물려받음)
fn create_new_with_mode(path: &Path, mode: u32) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    o.mode(mode);
    #[cfg(not(unix))]
    let _ = mode;
    o.open(path)
}

/// umask 영향 제거 (원래 권한 그대로) — Unix 만
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// 파일 내용을 읽는다. 없으면 None. 그 밖의 오류(권한, 디렉터리 등)는 오류로 돌려준다.
fn read_bytes_opt(p: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(p) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("{} 을(를) 읽지 못했습니다", p.display())),
    }
}

fn bytes_to_str<'a>(p: &Path, b: &'a [u8]) -> Result<&'a str> {
    std::str::from_utf8(b).with_context(|| format!("{} 이(가) 올바른 UTF-8 텍스트가 아닙니다 (고치지 않음)", p.display()))
}

/// 설정 파일을 처음 고치기 전의 원본을 같은 폴더에 `.ghostdesk-backup` 으로 남긴다.
/// 이미 백업이 있으면 덮어쓰지 않는다 (처음 원본을 보존). 백업도 원본과 같은 권한으로 만든다.
fn backup(p: &Path) -> Result<()> {
    let b = with_suffix(p, ".ghostdesk-backup");
    if std::fs::symlink_metadata(&b).is_ok() {
        return Ok(());
    }
    let Some(bytes) = read_bytes_opt(p)? else {
        return Ok(());
    };
    guard_write(&b);
    let mode = file_mode(&resolve_target(p));
    let mut f = match create_new_with_mode(&b, mode) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("백업 {} 을(를) 만들지 못했습니다", b.display())),
    };
    f.write_all(&bytes)?;
    f.sync_all()?;
    set_mode(&b, mode)?;
    Ok(())
}

/// `p` 를 `contents` 로 원자적으로 바꾼다.
/// - 심볼릭 링크면 링크를 그대로 두고 실제 대상 파일을 바꾼다.
/// - 원래 권한을 그대로 쓴다 (새 파일은 0600). 넓히지 않는다.
/// - `expect` 가 주어지면, 바꿔치기 직전에 파일을 다시 읽어 그 내용과 다르면(그 사이 다른 프로그램이 고침)
///   쓰지 않고 false 를 돌려준다.
/// - Windows: 다른 프로그램(백신, 실행 중인 클라이언트)이 파일을 잠깐 쥐고 있으면 몇 번 다시 시도한다.
fn write_atomic(p: &Path, contents: &[u8], expect: Option<&Option<Vec<u8>>>) -> Result<bool> {
    let real = resolve_target(p);
    guard_write(p);
    guard_write(&real);
    let mode = file_mode(&real);
    let tmp = with_suffix(&real, ".ghostdesk-tmp");
    let _ = std::fs::remove_file(&tmp);
    let res = (|| -> Result<bool> {
        {
            let mut f = create_new_with_mode(&tmp, mode)
                .with_context(|| format!("{} 을(를) 만들지 못했습니다", tmp.display()))?;
            f.write_all(contents)?;
            f.sync_all()?;
        }
        // umask 영향 제거 (원래 권한 그대로)
        set_mode(&tmp, mode)?;
        if let Some(exp) = expect {
            if read_bytes_opt(p)? != *exp {
                return Ok(false);
            }
        }
        fsx::rename_replace(&tmp, &real).with_context(|| format!("{} 을(를) 바꾸지 못했습니다", real.display()))?;
        Ok(true)
    })();
    if !matches!(res, Ok(true)) {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// 설정 파일을 읽고 → `edit` 으로 고친 뒤 → 바로 다시 읽어 그 사이 바뀌지 않았을 때만 쓴다.
/// (실행 중인 Claude Code 등이 같은 파일을 수시로 다시 쓰므로, 바뀌었으면 새 내용으로 처음부터 다시 함)
/// `edit` 은 원래 텍스트(파일이 없으면 None, BOM 은 뗀 것)를 받아 새 텍스트를 돌려준다. None 을 돌려주면 고칠 것이 없음.
/// 원래 파일에 UTF-8 BOM 이 있었으면 새 내용에도 붙인다 (그 파일을 쓰는 프로그램이 같은 인코딩을 보도록).
fn update_file(p: &Path, edit: &dyn Fn(Option<&str>) -> Result<Option<String>>) -> Result<()> {
    for _ in 0..5 {
        let orig = read_bytes_opt(p)?;
        let (text, bom) = match &orig {
            Some(b) => {
                let (t, bom) = fsx::strip_bom(bytes_to_str(p, b)?);
                (Some(t), bom)
            }
            None => (None, false),
        };
        let Some(mut new) = edit(text)? else {
            return Ok(());
        };
        if bom {
            new.insert(0, '\u{FEFF}');
        }
        backup(p)?;
        if write_atomic(p, new.as_bytes(), Some(&orig))? {
            return Ok(());
        }
    }
    bail!("{} 이(가) 다른 프로그램에 의해 계속 바뀌고 있어 고치지 못했습니다. 잠시 뒤 다시 시도하세요", p.display())
}

/// JSON 텍스트 해석. 비어 있으면 {}. 해석할 수 없으면 오류 (빈 설정으로 덮어쓰지 않도록).
/// 앞의 UTF-8 BOM 은 무시한다 (Windows PowerShell 5.1 의 `Set-Content -Encoding UTF8` 등이 붙임).
fn parse_json(p: &Path, text: Option<&str>) -> Result<Value> {
    match text.map(|s| fsx::strip_bom(s).0) {
        None => Ok(json!({})),
        Some(s) if s.trim().is_empty() => Ok(json!({})),
        Some(s) => {
            let v: Value = serde_json::from_str(s).with_context(|| format!("{} 이(가) 올바른 JSON 이 아닙니다 (고치지 않음)", p.display()))?;
            if !v.is_object() {
                bail!("{} 의 형식이 예상과 다릅니다 (최상위가 객체가 아님, 고치지 않음)", p.display());
            }
            Ok(v)
        }
    }
}

fn read_json(p: &Path) -> Result<Value> {
    let b = read_bytes_opt(p)?;
    let s = match &b {
        Some(b) => Some(bytes_to_str(p, b)?),
        None => None,
    };
    parse_json(p, s)
}

/// JSON 설정의 `<key>.ghostdesk` 를 `entry` 로 바꾼다 (None 이면 지움). 다른 키·순서는 그대로.
/// `key` 는 서버 목록 객체의 이름 (대부분 "mcpServers", OpenCode 는 "mcp").
fn json_set_server(p: &Path, key: &str, entry: Option<Value>) -> Result<()> {
    update_file(p, &|text| {
        let mut v = parse_json(p, text)?;
        let obj = v.as_object_mut().ok_or_else(|| anyhow!("{} 형식 오류", p.display()))?;
        match &entry {
            Some(e) => {
                let servers = obj.entry(key).or_insert_with(|| json!({}));
                if servers.is_null() {
                    *servers = json!({});
                }
                let m = servers
                    .as_object_mut()
                    .ok_or_else(|| anyhow!("{} 의 {key} 형식이 예상과 다릅니다 (고치지 않음)", p.display()))?;
                if m.get(SERVER_NAME) == Some(e) {
                    return Ok(None);
                }
                m.insert(SERVER_NAME.to_string(), e.clone());
            }
            None => match obj.get_mut(key).and_then(|s| s.as_object_mut()) {
                Some(m) if m.contains_key(SERVER_NAME) => {
                    m.shift_remove(SERVER_NAME);
                }
                _ => return Ok(None),
            },
        }
        Ok(Some(serde_json::to_string_pretty(&v)? + "\n"))
    })
}

/// Codex config.toml 해석 (서식·주석 보존). 해석할 수 없으면 오류.
fn parse_toml(p: &Path, text: Option<&str>) -> Result<toml_edit::DocumentMut> {
    fsx::strip_bom(text.unwrap_or(""))
        .0
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("{} 이(가) 올바른 TOML 이 아닙니다 (고치지 않음)", p.display()))
}

/// Codex config.toml 을 읽기만 한다 (BOM 무시). 없거나 해석할 수 없으면 None.
fn read_toml(p: &Path) -> Option<toml_edit::DocumentMut> {
    let b = read_bytes_opt(p).ok()??;
    fsx::strip_bom(&String::from_utf8_lossy(&b)).0.parse::<toml_edit::DocumentMut>().ok()
}

/// Codex 용 [mcp_servers.ghostdesk] 테이블
fn codex_table(exe: &str, client_name: &str) -> toml_edit::Table {
    let mut t = toml_edit::Table::new();
    t["command"] = toml_edit::value(exe);
    let mut args = toml_edit::Array::new();
    args.push("--mcp");
    t["args"] = toml_edit::value(args);
    let mut env = toml_edit::InlineTable::new();
    env.insert("GHOSTDESK_CLIENT_NAME", client_name.into());
    t["env"] = toml_edit::value(env);
    // desk_open 은 승인 대기·GUI 실행·프로필 복제로 Codex 기본 시간 제한(60초)을 넘길 수 있다
    t["startup_timeout_sec"] = toml_edit::value(CODEX_STARTUP_TIMEOUT_SEC);
    t["tool_timeout_sec"] = toml_edit::value(CODEX_TOOL_TIMEOUT_SEC);
    t
}

/// 등록된 Codex 항목에 시간 제한이 들어 있는가 (예전 버전이 등록한 항목은 없음 → 다시 씀)
fn codex_has_timeouts(p: &Path) -> bool {
    let Some(doc) = read_toml(p) else { return false };
    let Some(t) = doc.get("mcp_servers").and_then(|m| m.get(SERVER_NAME)) else { return false };
    t.get("tool_timeout_sec").and_then(|v| v.as_integer()).map(|n| n >= CODEX_TOOL_TIMEOUT_SEC).unwrap_or(false)
        && t.get("startup_timeout_sec").and_then(|v| v.as_integer()).is_some()
}

fn codex_set_server(p: &Path, entry: Option<toml_edit::Table>) -> Result<()> {
    update_file(p, &|text| {
        let mut doc = parse_toml(p, text)?;
        match &entry {
            Some(t) => {
                if doc.get("mcp_servers").is_none() {
                    let mut m = toml_edit::Table::new();
                    m.set_implicit(true);
                    doc["mcp_servers"] = toml_edit::Item::Table(m);
                }
                let m = doc["mcp_servers"]
                    .as_table_like_mut()
                    .ok_or_else(|| anyhow!("{} 의 mcp_servers 형식이 예상과 다릅니다 (고치지 않음)", p.display()))?;
                m.insert(SERVER_NAME, toml_edit::Item::Table(t.clone()));
            }
            None => match doc.get_mut("mcp_servers").and_then(|i| i.as_table_like_mut()) {
                Some(m) if m.contains_key(SERVER_NAME) => {
                    m.remove(SERVER_NAME);
                }
                _ => return Ok(None),
            },
        }
        Ok(Some(doc.to_string()))
    })
}

// ───────────────────────── 클라이언트 ─────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Client {
    ClaudeCode,
    ClaudeApp,
    Codex,
    KimiCode,
    OpenCode,
    /// pi 는 MCP 를 기본 지원하지 않는다. MCP 확장(pi-mcp-adapter 등)이 읽는 ~/.pi/agent/mcp.json 에 등록한다.
    Pi,
}

impl Client {
    pub const ALL: [Client; 6] = [Client::ClaudeCode, Client::ClaudeApp, Client::Codex, Client::KimiCode, Client::OpenCode, Client::Pi];
    pub fn name(self) -> &'static str {
        match self {
            Client::ClaudeCode => "Claude Code",
            Client::ClaudeApp => "Claude 앱",
            Client::Codex => "Codex",
            Client::KimiCode => "Kimi Code",
            Client::OpenCode => "OpenCode",
            Client::Pi => "pi",
        }
    }
    /// 운영체제와 상관없이 홈 기준인 클라이언트의 설정 파일 (나머지는 None)
    /// - Kimi Code: %KIMI_CODE_HOME% 또는 ~/.kimi-code 의 mcp.json
    /// - OpenCode: %XDG_CONFIG_HOME% 또는 ~/.config 의 opencode/opencode.json (없고 opencode.jsonc 만 있으면 그것)
    /// - pi: %PI_CODING_AGENT_DIR% 또는 ~/.pi/agent 의 mcp.json
    fn home_config(self, b: &Bases) -> Option<PathBuf> {
        match self {
            Client::KimiCode => Some(kimi_home(b).join("mcp.json")),
            Client::OpenCode => {
                let d = opencode_dir(b);
                let (json, jsonc) = (d.join("opencode.json"), d.join("opencode.jsonc"));
                Some(if !json.exists() && jsonc.is_file() { jsonc } else { json })
            }
            Client::Pi => Some(pi_agent_dir(b).join("mcp.json")),
            _ => None,
        }
    }
    /// 이 클라이언트가 읽는 설정 파일 (등록할 때 실제로 고치는 경로).
    /// - macOS: ~/.claude.json, ~/Library/Application Support/Claude/claude_desktop_config.json, ~/.codex/config.toml
    /// - Windows: %CLAUDE_CONFIG_DIR% 또는 %USERPROFILE% 의 .claude.json, Claude 앱 데이터 폴더(`claude_app_config`),
    ///   %CODEX_HOME% 또는 %USERPROFILE%\.codex 의 config.toml
    pub fn config(self) -> PathBuf {
        let b = bases();
        if let Some(p) = self.home_config(&b) {
            return p;
        }
        #[cfg(windows)]
        {
            match self {
                Client::ClaudeCode => b.var_path("CLAUDE_CONFIG_DIR").unwrap_or_else(|| b.home.clone()).join(".claude.json"),
                Client::ClaudeApp => claude_app_config(&b),
                _ => codex_home(&b).join("config.toml"),
            }
        }
        #[cfg(not(windows))]
        {
            let h = b.home;
            match self {
                Client::ClaudeCode => h.join(".claude.json"),
                Client::ClaudeApp => h.join("Library/Application Support/Claude/claude_desktop_config.json"),
                _ => h.join(".codex/config.toml"),
            }
        }
    }
    /// 이 컴퓨터에 설치되어 있는가
    pub fn installed(self) -> bool {
        let b = bases();
        match self {
            Client::KimiCode => return find_cli("kimi").is_some() || kimi_home(&b).exists(),
            Client::OpenCode => return find_cli("opencode").is_some() || opencode_dir(&b).exists(),
            Client::Pi => return find_cli("pi").is_some() || pi_agent_dir(&b).exists(),
            _ => {}
        }
        #[cfg(windows)]
        {
            match self {
                Client::ClaudeCode => find_cli("claude").is_some() || self.config().exists(),
                Client::ClaudeApp => claude_app_installed(&b),
                // Codex 데스크톱 앱(MSIX)도 CLI 와 같은 ~/.codex 를 쓴다
                _ => find_cli("codex").is_some() || codex_home(&b).exists() || !package_ids(&b, "OpenAI.Codex").is_empty(),
            }
        }
        #[cfg(not(windows))]
        {
            match self {
                Client::ClaudeCode => find_cli("claude").is_some() || self.config().exists(),
                Client::ClaudeApp => Path::new("/Applications/Claude.app").exists() || self.config().parent().map(|d| d.exists()).unwrap_or(false),
                _ => find_cli("codex").is_some() || b.home.join(".codex").exists(),
            }
        }
    }
    /// 현재 등록된 ghostdesk 명령 경로 (없으면 None)
    pub fn registered(self) -> Option<String> {
        registered_in(self, &self.config())
    }
}

/// Codex 설정 폴더 (Windows): %CODEX_HOME% 또는 %USERPROFILE%\.codex
#[cfg(windows)]
fn codex_home(b: &Bases) -> PathBuf {
    b.var_path("CODEX_HOME").unwrap_or_else(|| b.home.join(".codex"))
}

/// Kimi Code 설정 폴더: %KIMI_CODE_HOME% 또는 ~/.kimi-code
fn kimi_home(b: &Bases) -> PathBuf {
    b.var_path("KIMI_CODE_HOME").unwrap_or_else(|| b.home.join(".kimi-code"))
}

/// OpenCode 전역 설정 폴더: %XDG_CONFIG_HOME%/opencode 또는 ~/.config/opencode (Windows 도 같은 규칙)
fn opencode_dir(b: &Bases) -> PathBuf {
    b.var_path("XDG_CONFIG_HOME").unwrap_or_else(|| b.home.join(".config")).join("opencode")
}

/// pi 에이전트 폴더: %PI_CODING_AGENT_DIR% 또는 ~/.pi/agent
fn pi_agent_dir(b: &Bases) -> PathBuf {
    b.var_path("PI_CODING_AGENT_DIR").unwrap_or_else(|| b.home.join(".pi").join("agent"))
}

/// JSON 설정에서 MCP 서버 목록이 들어 있는 키
fn servers_key(c: Client) -> &'static str {
    if c == Client::OpenCode {
        "mcp"
    } else {
        "mcpServers"
    }
}

/// 설치된 MSIX 패키지 `<이름>_<게시자 ID>` 의 게시자 ID 들 (%LOCALAPPDATA%\Packages 기준, 소문자, 정렬)
#[cfg(windows)]
fn package_ids(b: &Bases, name: &str) -> Vec<String> {
    let prefix = format!("{name}_").to_lowercase();
    let mut ids: Vec<String> = std::fs::read_dir(b.local.join("Packages"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            let id = n.strip_prefix(&prefix)?;
            (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())).then(|| id.to_string())
        })
        .collect();
    ids.sort();
    ids
}

/// Claude 앱(MSIX) 데이터 폴더들: %LOCALAPPDATA%\Claude-Data\claude_<게시자 ID> → (ID 소문자, 폴더). 이름순.
#[cfg(windows)]
fn claude_data_dirs(b: &Bases) -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(b.local.join("Claude-Data"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            let id = n.strip_prefix("claude_")?.to_string();
            (!id.is_empty()).then(|| (id, e.path()))
        })
        .collect();
    v.sort();
    v
}

/// 예전 Claude 데이터 폴더(%APPDATA%\Claude)에 "데이터를 옮겨 갔다" 는 표식(.user-data-left.*)이 있는가
#[cfg(windows)]
fn has_left_marker(legacy: &Path) -> bool {
    std::fs::read_dir(legacy)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().to_lowercase().starts_with(".user-data-left"))
}

/// Claude 앱의 claude_desktop_config.json 위치 (Windows).
/// 1. 지금의 Claude 앱(MSIX)은 %LOCALAPPDATA%\Claude-Data\claude_<게시자 ID>\ 를 쓴다. 설치된 패키지
///    (%LOCALAPPDATA%\Packages\Claude_<ID>)와 ID 가 같은 폴더 → 설정 파일이 이미 있는 폴더 → 첫 폴더 순으로 고른다.
/// 2. %APPDATA%\Claude 는 예전 설치본의 폴더다. MSIX 앱이 데이터를 옮겨 간 뒤에는 표식(.user-data-left.*)만
///    남고 더는 읽지 않으므로, 표식이 없을 때만 쓴다 (예전 설치본이거나, 아직 옮기기 전이면 앱이 처음 켜질 때
///    이 파일도 함께 옮겨 간다).
/// 3. 데이터 폴더가 아직 없고 패키지만 있으면(설치 후 한 번도 켜지 않음) 앱이 쓸 폴더를 고른다.
/// 4. 아무것도 없으면 %APPDATA%\Claude (공식 문서의 기본 위치).
#[cfg(windows)]
fn claude_app_config(b: &Bases) -> PathBuf {
    const FILE: &str = "claude_desktop_config.json";
    let pkgs = package_ids(b, "Claude");
    let data = claude_data_dirs(b);
    let pick = data
        .iter()
        .find(|(id, _)| pkgs.contains(id))
        .or_else(|| data.iter().find(|(_, d)| d.join(FILE).is_file()))
        .or_else(|| data.first());
    if let Some((_, d)) = pick {
        return d.join(FILE);
    }
    let legacy = b.roaming.join("Claude");
    if legacy.is_dir() && !has_left_marker(&legacy) {
        return legacy.join(FILE);
    }
    if let Some(id) = pkgs.first() {
        return b.local.join("Claude-Data").join(format!("claude_{id}")).join(FILE);
    }
    legacy.join(FILE)
}

/// Claude 앱이 설치되어 있는가 (Windows): MSIX 데이터 폴더·패키지, 예전 설치 프로그램(Squirrel)의 폴더,
/// 또는 옮겨 가지 않은 예전 데이터 폴더
#[cfg(windows)]
fn claude_app_installed(b: &Bases) -> bool {
    let legacy = b.roaming.join("Claude");
    b.local.join("Claude-Data").is_dir()
        || !package_ids(b, "Claude").is_empty()
        || b.local.join("AnthropicClaude").is_dir()
        || (legacy.is_dir() && !has_left_marker(&legacy))
}

fn registered_in(c: Client, p: &Path) -> Option<String> {
    match c {
        Client::OpenCode => {
            // OpenCode 는 command 가 [실행 파일, 인자…] 배열
            let v = read_json(p).ok()?;
            v["mcp"][SERVER_NAME]["command"][0].as_str().map(String::from)
        }
        Client::ClaudeCode | Client::ClaudeApp | Client::KimiCode | Client::Pi => {
            let v = read_json(p).ok()?;
            v["mcpServers"][SERVER_NAME]["command"].as_str().map(String::from)
        }
        Client::Codex => {
            let doc = read_toml(p)?;
            doc.get("mcp_servers")?.get(SERVER_NAME)?.get("command")?.as_str().map(String::from)
        }
    }
}

#[derive(Clone, Debug)]
pub struct ClientStatus {
    pub client: Client,
    pub installed: bool,
    pub registered: Option<String>,
}

pub fn status_all() -> Vec<ClientStatus> {
    Client::ALL
        .iter()
        .map(|c| ClientStatus { client: *c, installed: c.installed(), registered: c.registered() })
        .collect()
}

/// JSON 설정을 쓰는 클라이언트의 ghostdesk 항목.
/// 참고: Claude Code 에는 서버별 도구 시간 제한 필드가 없다 (전역 환경 변수 MCP_TOOL_TIMEOUT 으로만 조절).
fn json_entry(c: Client, exe: &str) -> Value {
    let env = json!({ "GHOSTDESK_CLIENT_NAME": c.name() });
    match c {
        Client::ClaudeCode => json!({ "type": "stdio", "command": exe, "args": ["--mcp"], "env": env }),
        // desk_open 은 승인 대기·GUI 실행으로 오래 걸릴 수 있어 Codex 와 같은 시간 제한을 준다
        Client::KimiCode => json!({
            "command": exe, "args": ["--mcp"], "env": env,
            "startupTimeoutMs": CODEX_STARTUP_TIMEOUT_SEC * 1000,
            "toolTimeoutMs": CODEX_TOOL_TIMEOUT_SEC * 1000,
        }),
        Client::OpenCode => json!({
            "type": "local", "command": [exe, "--mcp"], "environment": env, "enabled": true, "timeout": OPENCODE_TIMEOUT_MS,
        }),
        // pi-mcp-adapter 는 쉬는 서버를 10분 뒤 끊는데, 연결이 끊기면 GhostDesk 가 그 연결의 화면을 닫는다 → 계속 유지
        Client::Pi => json!({ "command": exe, "args": ["--mcp"], "env": env, "lifecycle": "lazy-keep-alive" }),
        _ => json!({ "command": exe, "args": ["--mcp"], "env": env }),
    }
}

/// pi 등록 뒤의 안내 (pi 자체에는 MCP 가 없다)
const PI_NOTE: &str = "pi 에는 MCP 확장이 필요: pi install npm:pi-mcp-adapter";

/// Claude 앱 설정을 바꾼 뒤의 안내 (Windows 의 Claude 앱은 창을 닫아도 트레이에 남아 있다)
const CLAUDE_APP_RESTART: &str = if cfg!(windows) {
    "Claude 앱을 트레이 아이콘에서 완전히 종료한 뒤 다시 실행하면 적용"
} else {
    "Claude 앱 재시작 후 적용"
};

/// 한 클라이언트에 ghostdesk 를 등록한다 (이미 같은 경로면 그대로).
pub fn register(c: Client, exe: &Path) -> Result<String> {
    let exe_s = exe.display().to_string();
    let up_to_date = match c {
        Client::Codex => codex_has_timeouts(&c.config()),
        // 시간 제한 등 항목 전체가 같아야 그대로 둔다
        Client::KimiCode | Client::OpenCode | Client::Pi => {
            read_json(&c.config()).map(|v| v[servers_key(c)][SERVER_NAME] == json_entry(c, &exe_s)).unwrap_or(false)
        }
        _ => true,
    };
    if c.registered().as_deref() == Some(exe_s.as_str()) && up_to_date {
        return Ok("이미 연결됨".into());
    }
    let label = format!("GHOSTDESK_CLIENT_NAME={}", c.name());
    match c {
        Client::ClaudeCode => {
            // claude CLI 를 먼저 쓴다 (Claude Code 가 실행 중이어도 자기 설정을 안전하게 고침)
            let mut cli_err = None;
            if let Some(cli) = find_cli("claude") {
                if c.registered().is_some() {
                    let _ = run(cli_command(&cli).args(["mcp", "remove", "--scope", "user", SERVER_NAME]), Duration::from_secs(30));
                }
                match run(
                    cli_command(&cli).args(["mcp", "add", "--scope", "user", SERVER_NAME, "-e", &label, "--"]).arg(&exe_s).arg("--mcp"),
                    Duration::from_secs(40),
                ) {
                    Ok(_) => return Ok("claude mcp add 로 연결".into()),
                    // CLI 실행 실패(예: node 를 못 찾음) → 아래 JSON 직접 수정으로 대체
                    Err(e) => cli_err = Some(e),
                }
            }
            let p = c.config();
            // CLAUDE_CONFIG_DIR 로 지정한 폴더가 아직 없을 수 있다
            if let Some(d) = p.parent().filter(|d| !d.exists()) {
                guard_write(d);
                std::fs::create_dir_all(d)?;
            }
            json_set_server(&p, "mcpServers", Some(json_entry(c, &exe_s)))?;
            let shown = show_path(&p);
            Ok(match cli_err {
                Some(e) => format!("{shown} 에 추가 (claude CLI 실패로 직접 수정: {})", first_line(&e.to_string())),
                None => format!("{shown} 에 추가"),
            })
        }
        Client::ClaudeApp => {
            let p = c.config();
            if let Some(d) = p.parent() {
                guard_write(d);
                std::fs::create_dir_all(d)?;
            }
            json_set_server(&p, "mcpServers", Some(json_entry(c, &exe_s)))?;
            // macOS 는 위치가 하나뿐이라 파일 이름만, Windows 는 앱 버전마다 위치가 달라 실제 경로를 보여 준다
            let shown = if cfg!(windows) { show_path(&p) } else { "claude_desktop_config.json".to_string() };
            Ok(format!("{shown} 에 추가 ({CLAUDE_APP_RESTART})"))
        }
        Client::Codex => {
            // codex CLI 는 config.toml 을 다시 쓰며 주석을 지우고 시간 제한도 못 넣으므로, 서식을 보존하는 직접 수정만 쓴다
            let _ = &label;
            let p = c.config();
            if let Some(d) = p.parent() {
                guard_write(d);
                std::fs::create_dir_all(d)?;
            }
            codex_set_server(&p, Some(codex_table(&exe_s, c.name())))?;
            Ok(format!("{} 에 추가", show_path(&p)))
        }
        // 세 클라이언트 모두 CLI 없이 JSON 파일을 직접 고친다 (kimi/opencode CLI 는 대화형이거나 시간 제한을 못 넣음)
        Client::KimiCode | Client::OpenCode | Client::Pi => {
            let p = c.config();
            if let Some(d) = p.parent() {
                guard_write(d);
                std::fs::create_dir_all(d)?;
            }
            json_set_server(&p, servers_key(c), Some(json_entry(c, &exe_s)))?;
            Ok(match c {
                Client::Pi => format!("{} 에 추가 ({PI_NOTE})", show_path(&p)),
                _ => format!("{} 에 추가", show_path(&p)),
            })
        }
    }
}

fn first_line(s: &str) -> String {
    let l = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    l.chars().take(120).collect()
}

/// 해제 결과 문구. Windows 는 고친 설정 파일 경로도 보여 준다 (macOS 는 기존 문구 그대로).
fn removed_msg(p: &Path, note: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if cfg!(windows) {
        parts.push(show_path(p));
    }
    if let Some(n) = note {
        parts.push(n.to_string());
    }
    if parts.is_empty() {
        "해제됨".into()
    } else {
        format!("해제됨 ({})", parts.join(", "))
    }
}

/// 한 클라이언트에서 ghostdesk 등록을 지운다.
pub fn unregister(c: Client) -> Result<String> {
    if c.registered().is_none() {
        return Ok("연결되어 있지 않음".into());
    }
    let p = c.config();
    match c {
        Client::ClaudeCode => {
            if let Some(cli) = find_cli("claude") {
                if run(cli_command(&cli).args(["mcp", "remove", "--scope", "user", SERVER_NAME]), Duration::from_secs(30)).is_ok() {
                    return Ok("해제됨".into());
                }
            }
            json_set_server(&p, "mcpServers", None)?;
            Ok(removed_msg(&p, None))
        }
        Client::ClaudeApp => {
            json_set_server(&p, "mcpServers", None)?;
            Ok(removed_msg(&p, Some(CLAUDE_APP_RESTART)))
        }
        Client::Codex => {
            codex_set_server(&p, None)?;
            Ok(removed_msg(&p, None))
        }
        Client::KimiCode | Client::OpenCode | Client::Pi => {
            json_set_server(&p, servers_key(c), None)?;
            Ok(removed_msg(&p, None))
        }
    }
}

/// 설치 + 설치된 모든 클라이언트에 등록. 결과를 (클라이언트, 메시지) 로 돌려준다.
pub fn connect_all() -> Result<(PathBuf, Vec<(Client, Result<String>)>)> {
    let exe = install_app()?;
    let res = Client::ALL
        .iter()
        .filter(|c| c.installed())
        .map(|c| (*c, register(*c, &exe)))
        .collect();
    Ok((exe, res))
}

pub fn disconnect_all() -> Vec<(Client, Result<String>)> {
    Client::ALL.iter().map(|c| (*c, unregister(*c))).collect()
}

/// `ghostdesk --connect` / `--disconnect` (터미널용)
pub fn cli(args: &[String], connect: bool) -> Result<()> {
    let _ = args;
    if connect {
        let (exe, res) = connect_all()?;
        println!("GhostDesk 설치 위치: {}", exe.display());
        for (c, r) in res {
            match r {
                Ok(m) => println!("✔ {}: {m}", c.name()),
                Err(e) => println!("✘ {}: {e:#}", c.name()),
            }
        }
        println!("이제 에이전트에게 \"GhostDesk 로 …해줘\" 라고 말하면 됩니다.");
    } else {
        for (c, r) in disconnect_all() {
            match r {
                Ok(m) => println!("✔ {}: {m}", c.name()),
                Err(e) => println!("✘ {}: {e:#}", c.name()),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    /// 테스트용 가짜 사용자 폴더. 살아 있는 동안 이 스레드의 `bases()` 가 여기를 가리킨다
    /// (실제 홈·%APPDATA%·%LOCALAPPDATA%·환경변수는 보지도 고치지도 않는다).
    struct FakeHome(PathBuf);
    impl FakeHome {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("ghostdesk-install-test-{}-{tag}", std::process::id()));
            let _ = fsx::remove_dir_all_retry(&d);
            std::fs::create_dir_all(&d).unwrap();
            TEST_BASES.with(|b| *b.borrow_mut() = Some(Bases::under(&d)));
            FakeHome(d)
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
        /// 가짜 환경변수 (나중에 넣은 값이 이김)
        #[cfg_attr(not(windows), allow(dead_code))]
        fn set_env(&self, k: &str, v: impl Into<OsString>) {
            let v = v.into();
            TEST_BASES.with(|b| b.borrow_mut().as_mut().unwrap().env.as_mut().unwrap().push((k.to_string(), v)));
        }
    }
    impl Drop for FakeHome {
        fn drop(&mut self) {
            TEST_BASES.with(|b| *b.borrow_mut() = None);
            let _ = fsx::remove_dir_all_retry(&self.0);
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn mkdir(p: impl AsRef<Path>) {
        std::fs::create_dir_all(p).unwrap();
    }

    #[cfg(unix)]
    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

    #[test]
    fn toml_edit_preserves_other_servers() {
        let src = "model = \"x\"\n\n[mcp_servers.other]\ncommand = \"a\"\n# 주석\n";
        let mut doc = src.parse::<toml_edit::DocumentMut>().unwrap();
        let mut t = toml_edit::Table::new();
        t["command"] = toml_edit::value("/x/ghostdesk");
        doc["mcp_servers"]["ghostdesk"] = toml_edit::Item::Table(t);
        let out = doc.to_string();
        assert!(out.contains("[mcp_servers.other]") && out.contains("# 주석") && out.contains("[mcp_servers.ghostdesk]"));
        let doc2 = out.parse::<toml_edit::DocumentMut>().unwrap();
        doc2["mcp_servers"]["other"]["command"].as_str().unwrap();
    }

    #[test]
    fn json_keeps_key_order() {
        let mut v: Value = serde_json::from_str(r#"{"z":1,"a":{"b":2},"mcpServers":{"x":{}}}"#).unwrap();
        v["mcpServers"]["ghostdesk"] = json!({"command": "c"});
        let s = serde_json::to_string(&v).unwrap();
        assert!(s.find("\"z\"").unwrap() < s.find("\"a\"").unwrap(), "키 순서 보존");
        assert!(s.contains("\"x\":{}"));
    }

    #[test]
    fn sha256_known_vectors() {
        let hx = |d: &[u8]| {
            let mut h = Sha256::new();
            h.update(d);
            h.finish_hex()
        };
        assert_eq!(hx(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hx(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            hx(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(hx(&vec![b'a'; 1_000_000]), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
        // 조각으로 나눠 넣어도 같은 결과
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 7 + 3) as u8).collect();
        for step in [1usize, 3, 63, 64, 65, 1000] {
            let mut h = Sha256::new();
            for c in data.chunks(step) {
                h.update(c);
            }
            assert_eq!(h.finish_hex(), hx(&data), "step {step}");
        }
    }

    #[test]
    fn source_stamp_detects_installed_build() {
        let h = FakeHome::new("stamp");
        let src = h.path("build/ghostdesk");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, b"unsigned binary v1").unwrap();
        let app = h.path("Applications/GhostDesk.app");
        let exe = installed_exe(&app);
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        // (macOS) 설치본 실행 파일은 codesign 으로 바뀐 바이트 (원본과 다름)
        std::fs::write(&exe, b"signed binary v1 (different bytes)").unwrap();
        let hash = sha256_file(&src).unwrap();
        assert!(!installed_from(&app, &hash), "지문이 없으면 다시 설치");
        write_source_stamp(&app, &hash).unwrap();
        assert!(installed_from(&app, &hash), "같은 원본이면 다시 설치하지 않음");
        std::fs::write(&src, b"unsigned binary v2").unwrap();
        assert!(!installed_from(&app, &sha256_file(&src).unwrap()), "원본이 바뀌면 다시 설치");
        std::fs::remove_file(&exe).unwrap();
        assert!(!installed_from(&app, &hash), "실행 파일이 없으면 다시 설치");
    }

    #[cfg(unix)]
    #[test]
    fn write_preserves_mode_and_symlink() {
        let h = FakeHome::new("mode");
        // 일반 파일 0600 유지
        let p = h.path(".claude.json");
        std::fs::write(&p, "{\"a\":1}").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        json_set_server(&p, "mcpServers", Some(json!({"command": "/x/ghostdesk"}))).unwrap();
        assert_eq!(mode_of(&p), 0o600, "권한이 넓어지면 안 됨");
        assert_eq!(read_json(&p).unwrap()["mcpServers"][SERVER_NAME]["command"], "/x/ghostdesk");
        assert_eq!(mode_of(&with_suffix(&p, ".ghostdesk-backup")), 0o600, "백업도 같은 권한");
        assert!(!with_suffix(&p, ".ghostdesk-tmp").exists());

        // 0640 같은 다른 권한도 그대로 (좁히지도 넓히지도 않음)
        let q = h.path("other.json");
        std::fs::write(&q, "{}").unwrap();
        std::fs::set_permissions(&q, std::fs::Permissions::from_mode(0o640)).unwrap();
        json_set_server(&q, "mcpServers", Some(json!({"command": "c"}))).unwrap();
        assert_eq!(mode_of(&q), 0o640);

        // 새 파일은 0600
        let n = h.path("new/claude_desktop_config.json");
        std::fs::create_dir_all(n.parent().unwrap()).unwrap();
        json_set_server(&n, "mcpServers", Some(json!({"command": "c"}))).unwrap();
        assert_eq!(mode_of(&n), 0o600);

        // 심볼릭 링크: 링크는 그대로, 실제 대상 파일이 바뀜
        let dot = h.path("dotfiles/claude.json");
        std::fs::create_dir_all(dot.parent().unwrap()).unwrap();
        std::fs::write(&dot, "{\"keep\":true}").unwrap();
        std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = h.path("link.json");
        symlink("dotfiles/claude.json", &link).unwrap();
        json_set_server(&link, "mcpServers", Some(json!({"command": "/x/ghostdesk"}))).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "링크가 일반 파일로 바뀌면 안 됨");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&dot).unwrap()).unwrap();
        assert_eq!(v["keep"], true);
        assert_eq!(v["mcpServers"][SERVER_NAME]["command"], "/x/ghostdesk");
        assert_eq!(mode_of(&dot), 0o600);

        // Codex config.toml 도 같은 방식
        let c = h.path(".codex/config.toml");
        std::fs::create_dir_all(c.parent().unwrap()).unwrap();
        std::fs::write(&c, "model = \"x\"\n").unwrap();
        std::fs::set_permissions(&c, std::fs::Permissions::from_mode(0o600)).unwrap();
        codex_set_server(&c, Some(codex_table("/x/ghostdesk", "Codex"))).unwrap();
        assert_eq!(mode_of(&c), 0o600);
    }

    #[test]
    fn refuses_unreadable_or_unparsable_config() {
        let h = FakeHome::new("bad");
        let p = h.path(".claude.json");
        let broken = "{\"oauthAccount\": {\"x\": 1}, broken";
        std::fs::write(&p, broken).unwrap();
        assert!(json_set_server(&p, "mcpServers", Some(json!({"command": "c"}))).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), broken, "해석 못 하는 파일은 덮어쓰지 않음");

        // 잘못된 UTF-8
        std::fs::write(&p, b"{\"a\": \"\xff\xfe\"}").unwrap();
        assert!(json_set_server(&p, "mcpServers", Some(json!({"command": "c"}))).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"{\"a\": \"\xff\xfe\"}");

        // 최상위가 객체가 아님 / mcpServers 가 객체가 아님 → 패닉 없이 오류
        std::fs::write(&p, "[1,2]").unwrap();
        assert!(json_set_server(&p, "mcpServers", Some(json!({"command": "c"}))).is_err());
        std::fs::write(&p, "{\"mcpServers\": \"x\"}").unwrap();
        assert!(json_set_server(&p, "mcpServers", Some(json!({"command": "c"}))).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"mcpServers\": \"x\"}");

        // 읽을 수 없는 경로 (디렉터리)
        let d = h.path("dir.json");
        std::fs::create_dir_all(&d).unwrap();
        assert!(json_set_server(&d, "mcpServers", Some(json!({"command": "c"}))).is_err());

        // Codex: 해석 못 하는 TOML 은 그대로
        let c = h.path("config.toml");
        std::fs::write(&c, "[[[ not toml").unwrap();
        assert!(codex_set_server(&c, Some(codex_table("/x", "Codex"))).is_err());
        assert!(codex_set_server(&c, None).is_err());
        assert_eq!(std::fs::read_to_string(&c).unwrap(), "[[[ not toml");

        // 없는 파일은 빈 설정으로 시작
        let missing = h.path("missing.json");
        json_set_server(&missing, "mcpServers", Some(json!({"command": "c"}))).unwrap();
        assert_eq!(read_json(&missing).unwrap()["mcpServers"][SERVER_NAME]["command"], "c");
    }

    #[test]
    fn backup_keeps_first_original() {
        let h = FakeHome::new("backup");
        let p = h.path(".claude.json");
        let original = "{\"original\": true}";
        std::fs::write(&p, original).unwrap();
        let b = with_suffix(&p, ".ghostdesk-backup");
        json_set_server(&p, "mcpServers", Some(json!({"command": "/a"}))).unwrap();
        assert_eq!(std::fs::read_to_string(&b).unwrap(), original);
        json_set_server(&p, "mcpServers", None).unwrap();
        json_set_server(&p, "mcpServers", Some(json!({"command": "/b"}))).unwrap();
        assert_eq!(std::fs::read_to_string(&b).unwrap(), original, "백업은 처음 원본 그대로");
        // 바꿀 것이 없으면 쓰지 않음
        let before = std::fs::read_to_string(&p).unwrap();
        json_set_server(&p, "mcpServers", Some(json!({"command": "/b"}))).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
        // 해제하면 다른 키는 남고 ghostdesk 만 빠짐
        json_set_server(&p, "mcpServers", None).unwrap();
        let v = read_json(&p).unwrap();
        assert_eq!(v["original"], true);
        assert!(v["mcpServers"].get(SERVER_NAME).is_none());
    }

    #[test]
    fn write_atomic_detects_concurrent_change() {
        let h = FakeHome::new("race");
        let p = h.path("f.json");
        std::fs::write(&p, "A").unwrap();
        let stale = Some(b"OLD".to_vec());
        assert!(!write_atomic(&p, b"B", Some(&stale)).unwrap(), "그 사이 바뀌었으면 쓰지 않음");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "A");
        assert!(!with_suffix(&p, ".ghostdesk-tmp").exists());
        let cur = Some(b"A".to_vec());
        assert!(write_atomic(&p, b"B", Some(&cur)).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "B");
    }

    #[test]
    fn codex_entry_has_timeouts() {
        let h = FakeHome::new("codex");
        let c = h.path(".codex/config.toml");
        std::fs::create_dir_all(c.parent().unwrap()).unwrap();
        let src = "# 내 설정\nmodel = \"x\"\n\n[mcp_servers.other]\ncommand = \"a\"\n\n[mcp_servers.ghostdesk]\ncommand = \"/old/ghostdesk\"\n";
        std::fs::write(&c, src).unwrap();
        assert!(!codex_has_timeouts(&c), "예전 항목은 시간 제한이 없음 → 다시 써야 함");
        codex_set_server(&c, Some(codex_table("/x/ghostdesk", "Codex"))).unwrap();
        let out = std::fs::read_to_string(&c).unwrap();
        assert!(out.contains("# 내 설정") && out.contains("[mcp_servers.other]"));
        let doc = out.parse::<toml_edit::DocumentMut>().unwrap();
        let g = &doc["mcp_servers"]["ghostdesk"];
        assert_eq!(g["command"].as_str(), Some("/x/ghostdesk"));
        assert_eq!(g["tool_timeout_sec"].as_integer(), Some(600));
        assert_eq!(g["startup_timeout_sec"].as_integer(), Some(30));
        assert_eq!(g["env"]["GHOSTDESK_CLIENT_NAME"].as_str(), Some("Codex"));
        assert!(codex_has_timeouts(&c));
        assert_eq!(registered_in(Client::Codex, &c).as_deref(), Some("/x/ghostdesk"));
        assert_eq!(std::fs::read_to_string(with_suffix(&c, ".ghostdesk-backup")).unwrap(), src);
        codex_set_server(&c, None).unwrap();
        assert!(registered_in(Client::Codex, &c).is_none());
        assert!(std::fs::read_to_string(&c).unwrap().contains("[mcp_servers.other]"));
    }

    #[cfg(unix)]
    #[test]
    fn cli_runs_with_explicit_path() {
        // `#!/usr/bin/env fakenode` 스크립트: CLI 와 같은 폴더의 fakenode 를 PATH 로 찾아야 실행됨
        let h = FakeHome::new("path");
        let bin = h.path("nvm/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let node = bin.join("ghostdesk-fakenode");
        std::fs::write(&node, "#!/bin/sh\necho ran-with-node\n").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cli = bin.join("claude");
        std::fs::write(&cli, "#!/usr/bin/env ghostdesk-fakenode\n").unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = cli_path_env(&cli);
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(dirs[0], bin, "CLI 자신의 폴더가 맨 앞");
        for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
            assert!(dirs.contains(&PathBuf::from(d)), "{d} 포함");
        }
        assert!(dirs.contains(&home().join(".local/bin")));
        let out = run(&mut cli_command(&cli), Duration::from_secs(10)).unwrap();
        assert_eq!(out.trim(), "ran-with-node");
        // PATH 를 지정하지 않으면 (GUI 의 launchd PATH) 실패하는 상황을 재현
        let fail = run(Command::new(&cli).env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin"), Duration::from_secs(10));
        assert!(fail.is_err());
    }

    // ───────── BOM · 이스케이프 · 전체 흐름 (모든 OS) ─────────

    #[test]
    fn bom_is_tolerated_and_preserved() {
        let h = FakeHome::new("bom");
        let exe = r"C:\g\ghostdesk.exe";
        // Windows PowerShell 5.1 의 Set-Content -Encoding UTF8 처럼 BOM 이 붙은 .claude.json
        let p = h.path(".claude.json");
        std::fs::write(&p, "\u{FEFF}{\n  \"numStartups\": 3,\n  \"mcpServers\": {\"other\": {\"command\": \"x\"}}\n}").unwrap();
        assert!(read_json(&p).is_ok(), "BOM 이 있어도 읽음");
        json_set_server(&p, "mcpServers", Some(json_entry(Client::ClaudeCode, exe))).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        assert!(bytes.starts_with(&BOM), "BOM 유지");
        assert!(!bytes[3..].starts_with(&BOM), "BOM 이 두 번 붙으면 안 됨");
        let v = read_json(&p).unwrap();
        assert_eq!(v["numStartups"], 3);
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["mcpServers"][SERVER_NAME]["type"], "stdio");
        assert_eq!(registered_in(Client::ClaudeCode, &p).as_deref(), Some(exe));
        assert!(std::fs::read(with_suffix(&p, ".ghostdesk-backup")).unwrap().starts_with(&BOM), "백업은 원본 그대로");
        json_set_server(&p, "mcpServers", None).unwrap();
        assert!(std::fs::read(&p).unwrap().starts_with(&BOM));
        assert!(registered_in(Client::ClaudeCode, &p).is_none());
        assert_eq!(read_json(&p).unwrap()["mcpServers"]["other"]["command"], "x");

        // BOM 없는 파일에는 붙이지 않음
        let q = h.path("plain.json");
        std::fs::write(&q, "{}").unwrap();
        json_set_server(&q, "mcpServers", Some(json!({"command": "c"}))).unwrap();
        assert!(!std::fs::read(&q).unwrap().starts_with(&BOM));

        // Codex config.toml
        let c = h.path("config.toml");
        std::fs::write(&c, "\u{FEFF}# 내 설정\nmodel = \"x\"\n").unwrap();
        codex_set_server(&c, Some(codex_table(exe, "Codex"))).unwrap();
        let out = std::fs::read(&c).unwrap();
        assert!(out.starts_with(&BOM) && !out[3..].starts_with(&BOM));
        assert!(String::from_utf8_lossy(&out).contains("# 내 설정"));
        assert!(codex_has_timeouts(&c));
        assert_eq!(registered_in(Client::Codex, &c).as_deref(), Some(exe));
        codex_set_server(&c, None).unwrap();
        assert!(std::fs::read(&c).unwrap().starts_with(&BOM));
        assert!(registered_in(Client::Codex, &c).is_none());
    }

    #[test]
    fn settings_tolerate_bom() {
        let h = FakeHome::new("settings");
        let p = settings_path();
        assert!(p.starts_with(&h.0));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "\u{FEFF}{\"auto_connect\": false, \"other\": 1}").unwrap();
        assert!(!auto_connect_enabled(), "BOM 이 있어도 '꺼짐' 을 읽음");
        set_auto_connect(true).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.starts_with('\u{FEFF}') && !s[3..].starts_with('\u{FEFF}'), "BOM 유지");
        let v: Value = serde_json::from_str(fsx::strip_bom(&s).0).unwrap();
        assert_eq!(v["auto_connect"], true);
        assert_eq!(v["other"], 1, "다른 설정은 그대로");
        if std::env::var("GHOSTDESK_NO_AUTOCONNECT").as_deref() != Ok("1") {
            assert!(auto_connect_enabled());
        }
        // 객체가 아닌 설정 파일이어도 패닉 없이 새로 쓴다
        std::fs::write(&p, "[1, 2]").unwrap();
        set_auto_connect(false).unwrap();
        assert!(!auto_connect_enabled());
    }

    #[test]
    fn windows_paths_are_escaped() {
        let h = FakeHome::new("escape");
        for (i, exe) in [
            r"C:\Users\홍 길동\AppData\Local\Programs\GhostDesk\ghostdesk.exe",
            r"C:\Users\O'Brien & Co (x)\AppData\Local\Programs\GhostDesk\ghostdesk.exe",
            // \u0041 · \n · \t 를 이스케이프로 잘못 해석하면 다른 경로가 된다
            r"\\server\share\u0041\new\tools\ghostdesk.exe",
        ]
        .into_iter()
        .enumerate()
        {
            let p = h.path(&format!("claude_desktop_config-{i}.json"));
            json_set_server(&p, "mcpServers", Some(json_entry(Client::ClaudeApp, exe))).unwrap();
            let raw = std::fs::read_to_string(&p).unwrap();
            let v: Value = serde_json::from_str(&raw).expect("올바른 JSON");
            assert_eq!(v["mcpServers"][SERVER_NAME]["command"].as_str(), Some(exe));
            assert!(raw.contains(r"\\"), "역슬래시가 이스케이프되어야 함: {raw}");
            assert_eq!(registered_in(Client::ClaudeApp, &p).as_deref(), Some(exe));

            let c = h.path(&format!("config-{i}.toml"));
            codex_set_server(&c, Some(codex_table(exe, "Codex"))).unwrap();
            let raw = std::fs::read_to_string(&c).unwrap();
            let doc = raw.parse::<toml_edit::DocumentMut>().expect("올바른 TOML");
            assert_eq!(doc["mcp_servers"][SERVER_NAME]["command"].as_str(), Some(exe), "{raw}");
            assert_eq!(registered_in(Client::Codex, &c).as_deref(), Some(exe));
        }
    }

    /// 가짜 사용자 폴더 안에서 등록 → 확인 → 해제 전체 흐름 (설치된 CLI 가 없으면 설정 파일을 직접 고친다)
    #[test]
    fn register_end_to_end_in_sandbox() {
        let h = FakeHome::new("e2e");
        let exe = stable_exe();
        assert!(exe.starts_with(&h.0));
        for c in Client::ALL {
            let p = c.config();
            assert!(p.starts_with(&h.0), "{p:?}");
            assert!(c.registered().is_none());
            let msg = register(c, &exe).unwrap();
            assert_eq!(c.registered().as_deref(), Some(exe.display().to_string().as_str()), "{c:?}");
            // 메시지에 실제로 고친 파일이 보인다
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            assert!(msg.contains(&name), "{msg}");
            #[cfg(windows)]
            assert!(msg.contains(&p.display().to_string()), "Windows 는 전체 경로: {msg}");
            assert_eq!(register(c, &exe).unwrap(), "이미 연결됨");
            let m = unregister(c).unwrap();
            assert!(m.starts_with("해제됨"), "{m}");
            #[cfg(windows)]
            assert!(m.contains(&p.display().to_string()), "{m}");
            assert!(c.registered().is_none());
            assert_eq!(unregister(c).unwrap(), "연결되어 있지 않음");
        }
        // Claude Code 항목은 type: stdio, Codex 는 시간 제한 포함
        register(Client::ClaudeCode, &exe).unwrap();
        assert_eq!(read_json(&Client::ClaudeCode.config()).unwrap()["mcpServers"][SERVER_NAME]["type"], "stdio");
        register(Client::Codex, &exe).unwrap();
        assert!(codex_has_timeouts(&Client::Codex.config()));
        let st = status_all();
        assert!(st.iter().any(|s| s.client == Client::Codex && s.installed && s.registered.is_some()));
    }

    /// Kimi Code · OpenCode · pi: 위치, 항목 모양, 다른 설정 보존, 환경변수
    #[test]
    fn kimi_opencode_pi_entries() {
        let h = FakeHome::new("harness");
        let exe = Path::new("/x/ghostdesk");
        let home = bases().home;
        assert_eq!(Client::KimiCode.config(), home.join(".kimi-code").join("mcp.json"));
        assert_eq!(Client::OpenCode.config(), home.join(".config").join("opencode").join("opencode.json"));
        assert_eq!(Client::Pi.config(), home.join(".pi").join("agent").join("mcp.json"));
        assert!(!Client::KimiCode.installed() && !Client::OpenCode.installed() && !Client::Pi.installed());

        // OpenCode: opencode.jsonc 만 있으면 그 파일을 고치고, 다른 설정은 그대로
        let oc = home.join(".config").join("opencode");
        mkdir(&oc);
        std::fs::write(oc.join("opencode.jsonc"), r#"{"$schema":"https://opencode.ai/config.json","provider":{"p":{}}}"#).unwrap();
        assert!(Client::OpenCode.installed());
        assert_eq!(Client::OpenCode.config(), oc.join("opencode.jsonc"));
        register(Client::OpenCode, exe).unwrap();
        let v = read_json(&oc.join("opencode.jsonc")).unwrap();
        assert_eq!(v["provider"], json!({"p":{}}));
        let g = &v["mcp"][SERVER_NAME];
        assert_eq!((g["type"].as_str(), g["command"].clone()), (Some("local"), json!(["/x/ghostdesk", "--mcp"])));
        assert_eq!(g["environment"]["GHOSTDESK_CLIENT_NAME"], "OpenCode");
        assert_eq!(Client::OpenCode.registered().as_deref(), Some("/x/ghostdesk"));
        // 주석이 있는 JSONC 는 해석하지 못하므로 덮어쓰지 않는다
        std::fs::write(oc.join("opencode.jsonc"), "{ // 주석\n}").unwrap();
        assert!(register(Client::OpenCode, Path::new("/y")).is_err());
        assert_eq!(std::fs::read_to_string(oc.join("opencode.jsonc")).unwrap(), "{ // 주석\n}");

        // Kimi: 시간 제한, 예전 항목(시간 제한 없음)은 다시 쓴다
        let kp = Client::KimiCode.config();
        mkdir(kp.parent().unwrap());
        std::fs::write(&kp, r#"{"mcpServers":{"other":{"command":"a"},"ghostdesk":{"command":"/x/ghostdesk"}}}"#).unwrap();
        assert_ne!(register(Client::KimiCode, exe).unwrap(), "이미 연결됨");
        let v = read_json(&kp).unwrap();
        assert_eq!(v["mcpServers"]["other"]["command"], "a");
        assert_eq!(v["mcpServers"][SERVER_NAME]["toolTimeoutMs"], CODEX_TOOL_TIMEOUT_SEC * 1000);
        assert_eq!(register(Client::KimiCode, exe).unwrap(), "이미 연결됨");

        // pi: 연결 유지 + 확장 안내
        assert!(register(Client::Pi, exe).unwrap().contains("pi-mcp-adapter"));
        assert_eq!(read_json(&Client::Pi.config()).unwrap()["mcpServers"][SERVER_NAME]["lifecycle"], "lazy-keep-alive");

        // 환경변수로 바꾼 위치
        h.set_env("KIMI_CODE_HOME", h.path("k"));
        h.set_env("XDG_CONFIG_HOME", h.path("xdg"));
        h.set_env("PI_CODING_AGENT_DIR", h.path("pi"));
        assert_eq!(Client::KimiCode.config(), h.path("k").join("mcp.json"));
        assert_eq!(Client::OpenCode.config(), h.path("xdg").join("opencode").join("opencode.json"));
        assert_eq!(Client::Pi.config(), h.path("pi").join("mcp.json"));
    }

    /// 테스트는 실제 사용자 파일을 절대 건드리지 않는다
    #[test]
    fn tests_never_touch_real_user_files() {
        // 1) 가짜 폴더를 지정하지 않은 스레드도 실제 폴더·환경변수가 아니라 빈 임시 폴더를 본다
        std::thread::spawn(|| {
            let mut paths = vec![stable_app(), stable_exe(), settings_path()];
            paths.extend(Client::ALL.iter().map(|c| c.config()));
            for p in &paths {
                assert!(is_test_path(p), "{p:?}");
            }
            if let Some(real) = dirs::home_dir() {
                for p in [real.join(".claude.json"), real.join(".codex").join("config.toml")] {
                    assert!(!paths.contains(&p));
                }
            }
            assert!(bases().var("PATH").is_none(), "실제 환경변수도 보지 않음");
            assert!(find_cli("claude").is_none() && find_cli("codex").is_none(), "실제로 설치된 CLI 를 실행하지 않음");
        })
        .join()
        .unwrap();

        // 2) 임시 폴더 밖에 쓰려 하면 아무것도 쓰기 전에 멈춘다
        //    (부모 폴더가 없는 경로라, 혹시 막지 못했어도 실제로 파일이 생기지는 않는다)
        let outside = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")).join("ghostdesk-test-must-not-exist").join("x.json");
        assert!(!is_test_path(&outside));
        let o = outside.clone();
        assert!(std::thread::spawn(move || write_atomic(&o, b"{}", None)).join().is_err(), "write_atomic 은 막혀야 함");
        let o = outside.clone();
        assert!(std::thread::spawn(move || json_set_server(&o, "mcpServers", Some(json!({})))).join().is_err());
        let o = outside.clone();
        assert!(std::thread::spawn(move || codex_set_server(&o, Some(codex_table("x", "Codex")))).join().is_err());
        let o = outside.clone();
        assert!(std::thread::spawn(move || {
            let mut b = Bases::under(&std::env::temp_dir().join("ghostdesk-unused"));
            b.settings = o;
            TEST_BASES.with(|t| *t.borrow_mut() = Some(b));
            set_auto_connect(true)
        })
        .join()
        .is_err());
        assert!(!outside.parent().unwrap().exists());
    }

    // ───────── Windows ─────────

    #[cfg(windows)]
    fn system32() -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into())).join("System32")
    }

    #[cfg(windows)]
    #[test]
    fn windows_install_location() {
        let h = FakeHome::new("loc");
        let b = bases();
        assert_eq!(stable_app(), b.local.join("Programs").join("GhostDesk"));
        assert_eq!(stable_exe(), stable_app().join("ghostdesk.exe"));
        assert!(stable_exe().starts_with(&h.0));
        assert_eq!(source_stamp_path(&stable_app()), stable_app().join("ghostdesk.src-sha256"));
    }

    #[cfg(windows)]
    #[test]
    fn claude_app_config_resolution() {
        const F: &str = "claude_desktop_config.json";
        // 아무것도 없음 → 설치 안 됨, 기본 위치는 %APPDATA%\Claude
        {
            let _h = FakeHome::new("capp0");
            let b = bases();
            assert!(!Client::ClaudeApp.installed());
            assert_eq!(Client::ClaudeApp.config(), b.roaming.join("Claude").join(F));
        }
        // 지금의 MSIX 앱: 패키지 ID 와 같은 Claude-Data 폴더 (설정 파일이 다른 폴더에 있어도)
        {
            let _h = FakeHome::new("capp1");
            let b = bases();
            let data = b.local.join("Claude-Data");
            mkdir(b.local.join("Packages").join("Claude_pzs8sxrjxfjjc"));
            mkdir(b.local.join("Packages").join("ClaudeHelper_aaaaaaaaaaaaa"));
            mkdir(data.join("claude_aaaaaaaaaaaaa"));
            std::fs::write(data.join("claude_aaaaaaaaaaaaa").join(F), "{}").unwrap();
            mkdir(data.join("claude_pzs8sxrjxfjjc"));
            std::fs::write(data.join("README.txt"), "x").unwrap();
            let legacy = b.roaming.join("Claude");
            mkdir(&legacy);
            std::fs::write(legacy.join(".user-data-left.claude_pzs8sxrjxfjjc"), "x").unwrap();
            assert!(Client::ClaudeApp.installed());
            let want = data.join("claude_pzs8sxrjxfjjc").join(F);
            assert_eq!(Client::ClaudeApp.config(), want);
            // 등록하면 그 파일만 생기고, 예전 폴더·다른 데이터 폴더는 건드리지 않는다
            let exe = stable_exe();
            let msg = register(Client::ClaudeApp, &exe).unwrap();
            assert!(msg.contains(&want.display().to_string()) && msg.contains("트레이"), "{msg}");
            assert_eq!(Client::ClaudeApp.registered().as_deref(), Some(exe.display().to_string().as_str()));
            assert!(!legacy.join(F).exists());
            assert_eq!(std::fs::read_to_string(data.join("claude_aaaaaaaaaaaaa").join(F)).unwrap(), "{}");
        }
        // 패키지 정보 없이 데이터 폴더만: 설정 파일이 있는 폴더 → 없으면 이름순 첫 폴더
        {
            let _h = FakeHome::new("capp2");
            let b = bases();
            let data = b.local.join("Claude-Data");
            mkdir(data.join("claude_aaa"));
            mkdir(data.join("claude_bbb"));
            mkdir(data.join("other_ccc"));
            assert!(Client::ClaudeApp.installed());
            assert_eq!(Client::ClaudeApp.config(), data.join("claude_aaa").join(F));
            std::fs::write(data.join("claude_bbb").join(F), "{}").unwrap();
            assert_eq!(Client::ClaudeApp.config(), data.join("claude_bbb").join(F));
        }
        // 예전 설치 프로그램(Squirrel): %APPDATA%\Claude (표식 없음)
        {
            let _h = FakeHome::new("capp3");
            let b = bases();
            mkdir(b.local.join("AnthropicClaude"));
            assert!(Client::ClaudeApp.installed());
            mkdir(b.roaming.join("Claude"));
            assert_eq!(Client::ClaudeApp.config(), b.roaming.join("Claude").join(F));
        }
        // MSIX 를 설치했지만 아직 데이터를 옮기기 전 (예전 폴더에 표식 없음) → 예전 폴더 (처음 켤 때 함께 옮겨 감)
        {
            let _h = FakeHome::new("capp4");
            let b = bases();
            mkdir(b.local.join("Packages").join("Claude_xyz0123456789"));
            mkdir(b.roaming.join("Claude"));
            assert_eq!(Client::ClaudeApp.config(), b.roaming.join("Claude").join(F));
        }
        // 설치 후 한 번도 켜지 않음 (데이터 폴더 없음, 예전 폴더에는 표식만) → 앱이 만들 Claude-Data 폴더
        {
            let _h = FakeHome::new("capp5");
            let b = bases();
            mkdir(b.local.join("Packages").join("Claude_xyz0123456789"));
            let legacy = b.roaming.join("Claude");
            mkdir(&legacy);
            std::fs::write(legacy.join(".user-data-left.claude_xyz0123456789"), "x").unwrap();
            assert!(Client::ClaudeApp.installed());
            assert_eq!(Client::ClaudeApp.config(), b.local.join("Claude-Data").join("claude_xyz0123456789").join(F));
        }
        // 표식만 남은 예전 폴더는 설치된 것으로 보지 않는다
        {
            let _h = FakeHome::new("capp6");
            let b = bases();
            let legacy = b.roaming.join("Claude");
            mkdir(&legacy);
            std::fs::write(legacy.join(".user-data-left.claude_x"), "x").unwrap();
            assert!(!Client::ClaudeApp.installed());
        }
    }

    #[cfg(windows)]
    #[test]
    fn env_overrides_for_config_dirs() {
        {
            let h = FakeHome::new("envdirs");
            let b = bases();
            assert_eq!(Client::ClaudeCode.config(), b.home.join(".claude.json"));
            assert_eq!(Client::Codex.config(), b.home.join(".codex").join("config.toml"));
            assert!(!Client::Codex.installed());
            // 빈 값은 무시
            h.set_env("CLAUDE_CONFIG_DIR", "");
            h.set_env("CODEX_HOME", "");
            assert_eq!(Client::ClaudeCode.config(), b.home.join(".claude.json"));
            assert_eq!(Client::Codex.config(), b.home.join(".codex").join("config.toml"));
            let cc = h.path("claude cfg");
            let cx = h.path("codex home");
            h.set_env("claude_config_dir", cc.as_os_str()); // Windows 환경변수 이름은 대소문자 무시
            h.set_env("CODEX_HOME", cx.as_os_str());
            assert_eq!(Client::ClaudeCode.config(), cc.join(".claude.json"));
            assert_eq!(Client::Codex.config(), cx.join("config.toml"));
            // 아직 없는 폴더도 만들어 등록
            let exe = stable_exe();
            register(Client::ClaudeCode, &exe).unwrap();
            register(Client::Codex, &exe).unwrap();
            assert!(cc.join(".claude.json").is_file() && cx.join("config.toml").is_file());
            assert!(Client::Codex.installed() && Client::ClaudeCode.installed());
            assert!(!b.home.join(".claude.json").exists() && !b.home.join(".codex").exists());
        }
        // Codex 데스크톱 앱(MSIX)만 있어도 설치된 것으로 본다
        {
            let _h = FakeHome::new("envdirs-msix");
            assert!(!Client::Codex.installed());
            mkdir(bases().local.join("Packages").join("OpenAI.Codex_2p2nqsd0c76g0"));
            assert!(Client::Codex.installed());
        }
    }

    #[cfg(windows)]
    #[test]
    fn find_cli_is_pathext_aware() {
        let h = FakeHome::new("pathext");
        let b = bases();
        let npm = b.roaming.join("npm");
        mkdir(&npm);
        // npm 이 함께 까는 확장자 없는 sh 스크립트와 PowerShell 스크립트는 실행할 수 없다
        std::fs::write(npm.join("claude"), "#!/bin/sh\n").unwrap();
        std::fs::write(npm.join("claude.ps1"), "").unwrap();
        assert_eq!(find_cli("claude"), None);
        std::fs::write(npm.join("claude.cmd"), "@echo off\r\n").unwrap();
        assert_eq!(find_cli("claude"), Some(npm.join("claude.cmd")));
        assert!(Client::ClaudeCode.installed());
        // 같은 폴더면 .exe 가 먼저, .cmd 가 .bat 보다 먼저
        std::fs::write(npm.join("claude.bat"), "").unwrap();
        assert_eq!(find_cli("claude"), Some(npm.join("claude.cmd")));
        std::fs::write(npm.join("claude.exe"), "").unwrap();
        assert_eq!(find_cli("claude"), Some(npm.join("claude.exe")));
        std::fs::remove_file(npm.join("claude.exe")).unwrap();
        // 기본 설치(~/.local/bin)가 npm 보다 먼저
        let local_bin = b.home.join(".local").join("bin");
        mkdir(&local_bin);
        std::fs::write(local_bin.join("claude.exe"), "").unwrap();
        assert_eq!(find_cli("claude"), Some(local_bin.join("claude.exe")));
        std::fs::remove_file(local_bin.join("claude.exe")).unwrap();
        // PATHEXT 에 없는 확장자는 고르지 않음
        h.set_env("PATHEXT", ".COM;.EXE");
        assert_eq!(find_cli("claude"), None);
        h.set_env("PATHEXT", ".COM;.EXE;.BAT;.CMD;.VBS;.JS");
        assert_eq!(find_cli("claude"), Some(npm.join("claude.cmd")));
        // PATH 에만 있는 것 (따옴표로 감싼 항목, 공백 포함 폴더)
        let custom = h.path("my tools");
        mkdir(&custom);
        std::fs::write(custom.join("codex.bat"), "@echo off\r\n").unwrap();
        mkdir(custom.join("codex.exe")); // 폴더는 실행 파일이 아님
        assert_eq!(find_cli("codex"), None);
        h.set_env("Path", format!("relative\\dir;\"{}\"", custom.display()));
        assert_eq!(find_cli("codex"), Some(custom.join("codex.bat")));
        assert!(Client::Codex.installed());
    }

    #[cfg(windows)]
    #[test]
    fn cli_path_env_windows() {
        let h = FakeHome::new("winpath");
        let cli = h.path("npm").join("claude.cmd");
        h.set_env("Path", r"C:\A;C:\B\;c:\a;;C:\npm-dup");
        let dirs: Vec<PathBuf> = std::env::split_paths(&cli_path_env(&cli)).collect();
        assert_eq!(dirs[0], h.path("npm"), "CLI 자신의 폴더가 맨 앞");
        // 물려받은 PATH (대소문자 무시하고 중복 제거). Unix 폴더(/usr/bin 등)는 넣지 않는다
        assert_eq!(dirs[1..], [PathBuf::from(r"C:\A"), PathBuf::from(r"C:\B\"), PathBuf::from(r"C:\npm-dup")]);
    }

    /// npm 의 claude.cmd 같은 배치 파일에 인자가 그대로 전달된다 (공백·&·괄호·% 가 든 경로, 한글 폴더)
    #[cfg(windows)]
    #[test]
    fn cmd_shim_receives_args_intact() {
        let h = FakeHome::new("cmdq");
        let dir = h.path("npm 폴더 & (x)");
        mkdir(&dir);
        let cli = dir.join("claude.cmd");
        // 받은 인자 줄(%*)을 그대로 파일에 적는다 (npm 의 claude.cmd 는 이 %* 를 node 에 넘긴다)
        std::fs::write(&cli, "@echo off\r\n>\"%~dp0args.txt\" echo(%*\r\necho ran-ok\r\n").unwrap();
        assert_eq!(find_cli("claude"), None, "가짜 PATH 에 없으면 못 찾음");
        h.set_env("PATH", dir.as_os_str());
        assert_eq!(find_cli("claude"), Some(cli.clone()));
        let exe = r"C:\Users\A&B (x) 100%\AppData\Local\Programs\GhostDesk\ghostdesk.exe";
        let label = "GHOSTDESK_CLIENT_NAME=Claude Code";
        let out = run(
            cli_command(&cli).args(["mcp", "add", "--scope", "user", SERVER_NAME, "-e", label, "--"]).arg(exe).arg("--mcp").arg("%PATH%"),
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(out.trim(), "ran-ok");
        let got = decode_output(&std::fs::read(dir.join("args.txt")).unwrap());
        assert_eq!(
            got.trim_end(),
            format!("mcp add --scope user ghostdesk -e \"{label}\" -- \"{exe}\" --mcp \"%PATH%\""),
            "환경변수가 펼쳐지거나 따옴표가 깨지면 안 됨"
        );
    }

    /// 시간 초과면 CLI 와 그 자손(cmd → node 등)을 모두 끝내고, 출력 파이프를 기다리며 멈추지 않는다
    #[cfg(windows)]
    #[test]
    fn cli_timeout_kills_whole_tree() {
        let h = FakeHome::new("cmdtimeout");
        h.set_env("PATH", system32().as_os_str());
        let dir = h.path("bin");
        mkdir(&dir);
        let cli = dir.join("claude.cmd");
        // 손자를 따로 띄워 두고 오래 기다리는 CLI 흉내. 손자가 살아남으면 3초 뒤 leak.txt 를 남긴다.
        std::fs::write(
            &cli,
            "@echo off\r\ncd /d \"%~dp0\"\r\nstart \"\" /b cmd /d /c \"ping -n 4 127.0.0.1 >NUL & echo leaked>leak.txt\"\r\nping -n 30 127.0.0.1\r\necho finished>finished.txt\r\n",
        )
        .unwrap();
        let t = Instant::now();
        let e = run(&mut cli_command(&cli), Duration::from_millis(1500)).unwrap_err();
        assert!(e.to_string().contains("시간 초과"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(8), "파이프를 기다리며 멈춤: {:?}", t.elapsed());
        std::thread::sleep(Duration::from_secs(5));
        assert!(!dir.join("leak.txt").exists(), "손자 프로세스가 남아 있음");
        assert!(!dir.join("finished.txt").exists());
    }

    /// CLI 는 자기 폴더에서 실행되고, cmd.exe 가 이름만 적힌 명령을 현재 폴더에서 찾지 않는다
    /// (GhostDesk 가 물려받은 현재 폴더에 심어 둔 node.cmd 등이 실행되지 않게)
    #[cfg(windows)]
    #[test]
    fn cli_runs_in_own_folder_without_cwd_lookup() {
        let h = FakeHome::new("cmdcwd");
        h.set_env("PATH", system32().as_os_str());
        let dir = h.path("bin");
        mkdir(&dir);
        let other = h.path("downloads");
        mkdir(&other);
        std::fs::write(other.join("ghostdesk-planted-node.cmd"), "@echo planted-ran\r\n").unwrap();
        std::fs::write(dir.join("ghostdesk-own-tool.cmd"), "@echo own-ran\r\n").unwrap();
        let cli = dir.join("claude.cmd");
        std::fs::write(
            &cli,
            format!(
                "@echo off\r\necho cwd=%CD%\r\ncall ghostdesk-own-tool\r\ncd /d \"{}\"\r\ncall ghostdesk-planted-node\r\nexit /b 0\r\n",
                other.display()
            ),
        )
        .unwrap();
        let c = cli_command(&cli);
        assert_eq!(c.get_current_dir(), Some(dir.as_path()));
        assert!(c
            .get_envs()
            .any(|(k, v)| k == "NoDefaultCurrentDirectoryInExePath" && v == Some(std::ffi::OsStr::new("1"))));
        let out = run(&mut cli_command(&cli), Duration::from_secs(30)).unwrap();
        assert!(out.to_lowercase().contains(&format!("cwd={}", dir.display()).to_lowercase()), "{out}");
        // CLI 자신의 폴더(PATH 맨 앞)의 프로그램은 그대로 찾는다
        assert!(out.contains("own-ran"), "{out}");
        // 현재 폴더에만 있는 프로그램은 실행되지 않는다
        assert!(!out.contains("planted-ran"), "{out}");
    }

    /// 실패한 CLI 의 오류 문구 (cmd.exe 는 OEM 코드 페이지로 쓴다)
    #[cfg(windows)]
    #[test]
    fn cli_errors_are_decoded() {
        let h = FakeHome::new("cmderr");
        h.set_env("PATH", system32().as_os_str());
        let dir = h.path("bin");
        mkdir(&dir);
        let cli = dir.join("claude.cmd");
        std::fs::write(&cli, "@echo off\r\nghostdesk-no-such-command-xyz\r\nexit /b 9\r\n").unwrap();
        let e = run(&mut cli_command(&cli), Duration::from_secs(30)).unwrap_err().to_string();
        assert!(e.contains("ghostdesk-no-such-command-xyz"), "{e}");
        assert!(!e.contains('\u{FFFD}'), "깨진 글자: {e}");
        assert_eq!(decode_output("한글 ok".as_bytes()), "한글 ok");
        #[link(name = "kernel32")]
        extern "system" {
            fn GetOEMCP() -> u32;
        }
        if unsafe { GetOEMCP() } == 949 {
            assert_eq!(decode_output(&[0xC7, 0xD1, 0xB1, 0xDB]), "한글");
        }
    }

    /// 설정 파일을 다른 프로그램이 열어 둔 상태에서 고치기
    #[cfg(windows)]
    #[test]
    fn windows_replace_while_file_is_open() {
        use std::os::windows::fs::OpenOptionsExt;
        let h = FakeHome::new("winopen");
        let p = h.path(".claude.json");
        std::fs::write(&p, "{\"a\":1}").unwrap();
        // 보통처럼 열어 둔 상태 (삭제 공유 허용: Node·Rust 의 기본값)
        {
            let _reader = std::fs::File::open(&p).unwrap();
            json_set_server(&p, "mcpServers", Some(json!({"command": "c"}))).unwrap();
        }
        assert_eq!(read_json(&p).unwrap()["mcpServers"][SERVER_NAME]["command"], "c");
        assert_eq!(read_json(&p).unwrap()["a"], 1);
        // 삭제 공유 없이 잠가 둔 상태: 잠시 재시도한 뒤 원본을 그대로 두고 오류
        {
            let _lock = std::fs::OpenOptions::new().read(true).share_mode(1 /* FILE_SHARE_READ */).open(&p).unwrap();
            let before = std::fs::read(&p).unwrap();
            let e = json_set_server(&p, "mcpServers", Some(json!({"command": "d"}))).unwrap_err();
            assert!(format!("{e:#}").contains("바꾸지 못했습니다"), "{e:#}");
            assert_eq!(std::fs::read(&p).unwrap(), before);
            assert!(!with_suffix(&p, ".ghostdesk-tmp").exists());
        }
        json_set_server(&p, "mcpServers", Some(json!({"command": "d"}))).unwrap();
    }

    /// 이 컴퓨터의 실제 위치를 보여 주기만 한다 (읽기 전용: 폴더 목록·존재 여부만 보고 파일 내용은 읽지 않는다).
    /// `cargo test install::tests::show_real_locations -- --ignored --nocapture`
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn show_real_locations() {
        let b = Bases::real();
        println!("설치 위치: {}", b.local.join("Programs").join("GhostDesk").join(crate::platform::paths::EXE_NAME).display());
        println!("GhostDesk 설정: {}", b.settings.display());
        println!("Claude Code 설정: {}", b.var_path("CLAUDE_CONFIG_DIR").unwrap_or_else(|| b.home.clone()).join(".claude.json").display());
        println!("Claude 앱 설정: {} (설치됨: {})", claude_app_config(&b).display(), claude_app_installed(&b));
        println!("Codex 설정: {} (MSIX: {:?})", codex_home(&b).join("config.toml").display(), package_ids(&b, "OpenAI.Codex"));
    }

    /// 설치본이 실행 중이어도 (Claude 가 띄운 ghostdesk.exe --mcp 등) 새 버전으로 바꿀 수 있다
    #[cfg(windows)]
    #[test]
    fn windows_install_replaces_running_exe() {
        use crate::platform::proc;
        let h = FakeHome::new("winstall");
        let dir = stable_app();
        let target = stable_exe();
        mkdir(&dir);
        let names = || -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
            v.sort();
            v
        };
        // 실행 중인 설치본 흉내: cmd.exe 를 설치본 이름으로 복사해 창 없이 실행해 둔다
        std::fs::copy(system32().join("cmd.exe"), &target).unwrap();
        let mut running = proc::spawn_group(
            Command::new(&target).args(["/d", "/c", "ping -n 10 127.0.0.1 >NUL"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(proc::peek_exit(&running).is_none(), "설치본이 실행 중이어야 함");
        assert!(std::fs::write(&target, b"x").is_err(), "실행 중인 exe 는 덮어쓸 수 없음 (그래서 옆으로 옮긴다)");
        // 중단된 다른 설치의 복사본: 끝난 프로세스 것은 지우고, 살아 있는 프로세스 것은 둔다
        let dead = with_suffix(&target, ".installing-4000000000");
        let busy = with_suffix(&target, &format!(".installing-{}", running.id()));
        std::fs::write(&dead, b"partial").unwrap();
        std::fs::write(&busy, b"partial").unwrap();

        let src = h.path("build").join("ghostdesk.exe");
        mkdir(src.parent().unwrap());
        std::fs::write(&src, b"new build v2").unwrap();
        assert_eq!(install_exe_into(&src, &dir).unwrap(), target);
        assert_eq!(std::fs::read(&target).unwrap(), b"new build v2");
        assert!(proc::peek_exit(&running).is_none(), "실행 중이던 옛 설치본은 그대로 계속 실행");
        let olds: Vec<String> = names().into_iter().filter(|n| n.starts_with("ghostdesk.exe.old-")).collect();
        assert_eq!(olds.len(), 1, "옛 설치본은 옆으로 옮겨져 있음: {:?}", names());
        assert!(!dead.exists() && busy.exists(), "{:?}", names());
        assert_eq!(std::fs::read_to_string(source_stamp_path(&dir)).unwrap().trim(), sha256_file(&src).unwrap());

        // 같은 원본이면 다시 복사하지 않음
        let mtime = std::fs::metadata(&target).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        install_exe_into(&src, &dir).unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().modified().unwrap(), mtime);
        // 설치본 자신, 또는 옆으로 옮겨진 옛 설치본에서 실행 중이면 아무것도 하지 않음 (새 설치본을 되돌리지 않음)
        install_exe_into(&target, &dir).unwrap();
        install_exe_into(&dir.join(&olds[0]), &dir).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new build v2");

        // 옛 프로세스가 끝나면 다음 설치 때 옛 파일을 지운다. 실행 중이 아니면 한 번에 바꿔치기 (옛 파일 안 남김)
        proc::kill_group(&mut running);
        std::thread::sleep(Duration::from_millis(500));
        std::fs::write(&src, b"new build v3 (longer)").unwrap();
        install_exe_into(&src, &dir).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new build v3 (longer)");
        assert_eq!(names(), vec!["ghostdesk.exe".to_string(), "ghostdesk.src-sha256".to_string()]);
        // 지문은 맞는데 설치본이 바뀌었으면(크기 다름) 다시 설치
        std::fs::write(&target, b"tampered").unwrap();
        install_exe_into(&src, &dir).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new build v3 (longer)");
    }
}
