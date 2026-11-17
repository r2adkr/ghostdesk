//! 새 버전 알림: GitHub 릴리스에서 최신판을 찾아 지금 버전보다 새로우면 화면에 알린다.
//!
//! '업데이트' 를 누르면 지금 OS·CPU 에 맞는 릴리스 파일(`asset_suffixes`)을 내려받아 SHA256SUMS 로 확인한 뒤
//! 지금 실행 중인 것 자리에 바꿔 넣고, '다시 시작' 을 누르면 앱이 완전히 끝난 뒤 새 버전을 띄운다.
//! 실행 중인 것 자체를 바꾸므로 사용자가 다음에 같은 것(내려받은 exe 든 설치본이든)을 열어도 새 버전이 뜬다.
//! - Windows: `GhostDesk-<버전>-windows-<x64|arm64>.exe` → 실행 중인 exe 를 옆으로 옮기고 새 exe 를 제자리에
//! - macOS: `GhostDesk-<버전>-macos-<arm64|x64|universal>.zip` (안에 GhostDesk.app) → 실행 중인 .app 번들을 통째로 바꿈
//!
//! 이 OS·CPU 용 파일이 릴리스에 없거나, macOS 에서 .app 번들 밖에서 실행 중이면 릴리스 페이지를 연다.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use eframe::egui;
use serde_json::Value;
use tokio::runtime::Handle;

/// 릴리스를 올리는 저장소
const REPO: &str = "r2adkr/ghostdesk";
/// 설정의 '릴리스 노트' 링크
pub const RELEASES_PAGE: &str = "https://github.com/r2adkr/ghostdesk/releases";
/// 이 OS·CPU 에서 받을 릴리스 파일의 이름 끝. 앞의 것부터 찾는다 (macOS 는 이 CPU 전용이 없으면 유니버설).
/// 릴리스 파일 이름은 .github/workflows/release.yml 과 같아야 한다.
fn asset_suffixes() -> &'static [&'static str] {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => &["-windows-x64.exe"],
        ("windows", "aarch64") => &["-windows-arm64.exe"],
        ("macos", "aarch64") => &["-macos-arm64.zip", "-macos-universal.zip"],
        ("macos", "x86_64") => &["-macos-x64.zip", "-macos-universal.zip"],
        _ => &[],
    }
}
/// 켜 둔 채 오래 쓰는 앱이라 하루에 한 번 다시 확인한다
const RECHECK: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    /// "0.2.0" (앞의 v 는 뺀다)
    pub version: String,
    /// 릴리스 페이지 (새 소식 · 직접 내려받기)
    pub page: String,
    asset: Option<Asset>,
}

#[derive(Clone, Debug, PartialEq)]
struct Asset {
    name: String,
    url: String,
    size: u64,
    /// 같은 릴리스의 SHA256SUMS.txt (없으면 크기만 확인)
    sums_url: Option<String>,
}

impl Release {
    /// 앱 안에서 바로 바꿔 넣을 수 있는가 (아니면 릴리스 페이지를 연다)
    pub fn installable(&self) -> bool {
        self.asset.is_some()
    }
}

#[derive(Clone, Default, PartialEq)]
pub enum State {
    /// 확인 전 · 이미 최신 · 확인 실패 (네트워크가 없을 때 귀찮게 하지 않도록 조용히 넘어간다)
    #[default]
    None,
    Available(Release),
    /// 내려받는 중 (0..=1)
    Downloading(Release, f32),
    /// 새 exe 를 제자리에 넣었다: 다시 시작하면 적용된다
    Ready(Release),
    Failed(Release, String),
}

pub type Shared = Arc<parking_lot::Mutex<State>>;

/// 켤 때 한 번, 그 뒤로 하루에 한 번 최신 릴리스를 확인한다. 지난 업데이트가 남긴 파일도 치운다.
pub fn start(rt: &Handle, ctx: &egui::Context) -> Shared {
    let st = Shared::default();
    let (s, ctx) = (st.clone(), ctx.clone());
    rt.spawn_blocking(sweep_leftovers);
    rt.spawn(async move {
        loop {
            if let Ok(Some(r)) = latest(env!("CARGO_PKG_VERSION")).await {
                let mut g = s.lock();
                // 내려받는 중이거나 이미 받아 둔 것은 그대로 둔다
                if !matches!(*g, State::Downloading(..) | State::Ready(_)) {
                    *g = State::Available(r);
                    ctx.request_repaint();
                }
            }
            tokio::time::sleep(RECHECK).await;
        }
    });
    st
}

/// '업데이트': 새 exe 를 내려받아 실행 중인 exe 자리에 넣는다 (진행률은 `State::Downloading`)
pub fn download(rt: &Handle, st: Shared, ctx: egui::Context) {
    let r = match &*st.lock() {
        State::Available(r) | State::Failed(r, _) => r.clone(),
        _ => return,
    };
    let Some(asset) = r.asset.clone() else { return };
    *st.lock() = State::Downloading(r.clone(), 0.0);
    rt.spawn(async move {
        let progress = |f: f32| {
            if let State::Downloading(_, p) = &mut *st.lock() {
                *p = f;
            }
            ctx.request_repaint();
        };
        let res = match update_target() {
            Ok(target) => install(&asset, &target, progress).await,
            Err(e) => Err(e),
        };
        *st.lock() = match res {
            Ok(()) => State::Ready(r),
            Err(e) => State::Failed(r, format!("{e:#}")),
        };
        ctx.request_repaint();
    });
}

/// '다시 시작': 창을 닫은 뒤 main 이 `relaunch_if_requested` 로 새 exe 를 띄운다
static RELAUNCH: AtomicBool = AtomicBool::new(false);

pub fn request_relaunch(ctx: &egui::Context) {
    RELAUNCH.store(true, Ordering::SeqCst);
    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
}

/// main 이 GUI 를 끝내고 단일 실행 뮤텍스를 놓은 뒤 부른다 (먼저 띄우면 새 창이 옛 창을 보고 그냥 끝난다)
pub fn relaunch_if_requested() {
    if !RELAUNCH.load(Ordering::SeqCst) {
        return;
    }
    // macOS: 바꿔 넣은 .app 번들을 LaunchServices 로 새로 띄운다 (-n: 끝나 가는 지금 프로세스와 따로)
    #[cfg(target_os = "macos")]
    let mut cmd = match update_target() {
        Ok(bundle) => {
            let mut c = std::process::Command::new("/usr/bin/open");
            c.arg("-n").arg(bundle);
            c
        }
        Err(_) => return,
    };
    #[cfg(not(target_os = "macos"))]
    let mut cmd = match std::env::current_exe() {
        Ok(exe) => std::process::Command::new(exe),
        Err(_) => return,
    };
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        // 콘솔 프로그램이라 그냥 띄우면 콘솔 창이 생긴다
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let _ = cmd.spawn();
}

fn client() -> Result<reqwest::Client> {
    // GitHub API 는 User-Agent 가 꼭 있어야 한다 (앱 이름과 버전만 보낸다)
    Ok(reqwest::Client::builder()
        .user_agent(concat!("GhostDesk/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()?)
}

/// `current` 보다 새 정식 릴리스 (없으면 None)
async fn latest(current: &str) -> Result<Option<Release>> {
    let v: Value = client()?
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(30))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    // 바꿔 넣을 곳이 없으면(macOS 에서 번들 밖 실행) 파일을 고르지 않는다 → 릴리스 페이지를 연다
    let suffixes = if update_target().is_ok() { asset_suffixes() } else { &[] };
    Ok(parse_release(&v, current, suffixes))
}

/// 업데이트로 바꿔 넣을 것: Windows 는 실행 중인 exe, macOS 는 그것을 담은 .app 번들
fn update_target() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("실행 파일 경로를 알 수 없습니다")?;
    if cfg!(target_os = "macos") {
        let bundle = exe
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .map(Path::to_path_buf)
            .context("GhostDesk.app 번들 안에서 실행 중이 아니라 앱 안에서 업데이트할 수 없습니다")?;
        Ok(mac_target(&bundle))
    } else {
        Ok(exe)
    }
}

/// 내려받은 zip 에서 바로 연 앱은 macOS 가 읽기 전용 임시 위치(AppTranslocation)에서 실행한다.
/// 그 자리는 바꿔 넣을 수 없으므로 설치 위치(~/Applications/GhostDesk.app)에 새 버전을 넣고 거기서 다시 연다.
fn mac_target(bundle: &Path) -> PathBuf {
    if bundle.to_string_lossy().contains("/AppTranslocation/") {
        crate::install::stable_app()
    } else {
        bundle.to_path_buf()
    }
}

/// GitHub 릴리스 JSON → 지금(`current`)보다 새 정식 릴리스. `suffixes` 의 앞에서부터 처음 맞는 파일을 고른다.
fn parse_release(v: &Value, current: &str, suffixes: &[&str]) -> Option<Release> {
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        return None;
    }
    let version = v["tag_name"].as_str()?.trim_start_matches('v').to_string();
    if !newer(&version, current) {
        return None;
    }
    let assets = v["assets"].as_array().map(Vec::as_slice).unwrap_or_default();
    let url_of = |a: &Value| a["browser_download_url"].as_str().map(str::to_string);
    let sums_url = assets.iter().find(|a| a["name"] == "SHA256SUMS.txt").and_then(url_of);
    let asset = suffixes.iter().find_map(|suf| {
        let a = assets.iter().find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(suf)))?;
        Some(Asset { name: a["name"].as_str()?.to_string(), url: url_of(a)?, size: a["size"].as_u64()?, sums_url: sums_url.clone() })
    });
    let page = v["html_url"].as_str().map(str::to_string).unwrap_or_else(|| format!("https://github.com/{REPO}/releases/latest"));
    Some(Release { version, page, asset })
}

/// `candidate` 가 `current` 보다 새 버전인가 ("0.2.0" > "0.1.10", "0.2" == "0.2.0", 꼬리표 -beta 등은 무시)
pub(crate) fn newer(candidate: &str, current: &str) -> bool {
    let parts = |s: &str| -> Vec<u64> {
        s.split(['-', '+']).next().unwrap_or("").split('.').map(|p| p.trim().parse().unwrap_or(0)).collect()
    };
    let (mut a, mut b) = (parts(candidate), parts(current));
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    a > b
}

/// SHA256SUMS.txt ("<해시>  <파일 이름>" 줄들) 에서 `name` 의 해시
fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, file) = l.trim().split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
    })
}

/// 내려받기 → 크기·해시 확인 → `target`(실행 중인 exe 또는 .app 번들) 과 바꿔치기. 실패하면 받던 파일은 지운다.
async fn install(asset: &Asset, target: &Path, progress: impl Fn(f32)) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let me = target.to_path_buf();
    let http = client()?;
    let expected = match &asset.sums_url {
        Some(u) => {
            let sums = http.get(u).send().await?.error_for_status()?.text().await?;
            Some(sum_for(&sums, &asset.name).context("SHA256SUMS.txt 에 이 파일의 해시가 없습니다")?)
        }
        None => None,
    };
    // 같은 폴더에 받아야 이름 바꾸기만으로 바꿔 넣을 수 있다
    let staging = sibling(&me, &format!(".update-{}", std::process::id()));
    if let Some(dir) = me.parent() {
        tokio::fs::create_dir_all(dir).await.with_context(|| format!("{} 폴더를 만들 수 없습니다", dir.display()))?;
    }
    let res: Result<()> = async {
        let mut resp = http.get(&asset.url).send().await?.error_for_status()?;
        let mut f = tokio::fs::File::create(&staging)
            .await
            .with_context(|| format!("{} 에 새 버전을 쓸 수 없습니다", staging.display()))?;
        let mut got = 0u64;
        while let Some(chunk) = resp.chunk().await? {
            f.write_all(&chunk).await?;
            got += chunk.len() as u64;
            progress(got as f32 / asset.size.max(1) as f32);
        }
        f.flush().await?;
        drop(f);
        anyhow::ensure!(got == asset.size, "내려받은 크기가 다릅니다 ({got} / {} 바이트)", asset.size);
        let (staging, me) = (staging.clone(), me.clone());
        tokio::task::spawn_blocking(move || -> Result<()> {
            if let Some(want) = expected {
                let got = crate::install::sha256_file(&staging)?;
                anyhow::ensure!(got == want, "내려받은 파일의 해시가 릴리스와 다릅니다");
            }
            swap(&staging, &me)
        })
        .await?
    }
    .await;
    if res.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    res
}

#[cfg(windows)]
fn swap(staging: &Path, me: &Path) -> Result<()> {
    // 실행 중인 exe 는 이름만 바꿀 수 있다: 옆(<이름>.old-<시각>)으로 옮기고 새 파일을 제자리에
    crate::install::swap_in(staging, me)
}

/// macOS: 받은 zip 을 옆 폴더에 풀어 그 안의 GhostDesk.app 을 확인한 뒤, 실행 중인 번들을 옆(`<이름>.old-<시각>`)으로
/// 옮기고 새 번들을 제자리에 넣는다 (실행 중인 프로세스는 옮겨진 옛 파일을 계속 쓴다). 넣다가 실패하면 되돌린다.
#[cfg(target_os = "macos")]
fn swap(zip: &Path, bundle: &Path) -> Result<()> {
    let unpacked = sibling(bundle, &format!(".update-{}.d", std::process::id()));
    let res = (|| -> Result<()> {
        let _ = std::fs::remove_dir_all(&unpacked);
        let out = std::process::Command::new("/usr/bin/ditto").args(["-x", "-k"]).arg(zip).arg(&unpacked).output()?;
        anyhow::ensure!(out.status.success(), "받은 파일을 풀지 못했습니다: {}", String::from_utf8_lossy(&out.stderr).trim());
        let new_app = unpacked.join("GhostDesk.app");
        anyhow::ensure!(new_app.join("Contents/MacOS/ghostdesk").is_file(), "받은 파일 안에 GhostDesk.app 이 없습니다");
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        // 설치 위치에 아직 앱이 없으면(옮겨 둔 적 없이 내려받은 곳에서 연 경우) 새 앱만 넣는다
        let old = sibling(bundle, &format!(".old-{secs}"));
        let had = bundle.exists();
        if had {
            std::fs::rename(bundle, &old).context("지금 앱을 옮기지 못했습니다")?;
        }
        if let Err(e) = std::fs::rename(&new_app, bundle) {
            if had {
                let _ = std::fs::rename(&old, bundle);
            }
            return Err(anyhow::Error::from(e).context("새 앱을 제자리에 넣지 못했습니다"));
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&unpacked);
    let _ = std::fs::remove_file(zip);
    res
}

#[cfg(not(any(windows, target_os = "macos")))]
fn swap(_staging: &Path, _me: &Path) -> Result<()> {
    anyhow::bail!("이 OS 에서는 앱 안에서 업데이트할 수 없습니다")
}

/// `p` 와 같은 폴더의 `<p 이름><suffix>`
fn sibling(p: &Path, suffix: &str) -> PathBuf {
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    p.with_file_name(name)
}

/// 지난 업데이트가 남긴 것 (옆으로 옮긴 옛 exe·번들 · 받다 만 파일·폴더). 아직 실행 중인 옛 exe 는 지워지지 않아 다음에 다시.
/// 바꿔 넣는 대상(exe 또는 .app) 이름으로 시작하는, 업데이트가 만든 이름의 것만 지운다.
fn sweep_leftovers() {
    let Ok(me) = update_target() else { return };
    let (Some(dir), Some(name)) = (me.parent(), me.file_name()) else { return };
    let name = name.to_string_lossy().to_lowercase();
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_lowercase();
        if let Some(rest) = n.strip_prefix(&name) {
            if rest.starts_with(".old-") || rest.starts_with(".update-") {
                let p = e.path();
                let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compares_versions() {
        assert!(newer("0.2.0", "0.1.1"));
        assert!(newer("0.1.10", "0.1.9"));
        assert!(newer("1.0", "0.9.9"));
        assert!(!newer("0.2", "0.2.0"));
        assert!(!newer("0.1.1", "0.2.0"));
        assert!(!newer("0.2.0-beta", "0.2.0"));
    }

    #[test]
    fn picks_this_os_asset_and_checksum() {
        let v = json!({
            "tag_name": "v0.3.0", "html_url": "https://github.com/r2adkr/ghostdesk/releases/tag/v0.3.0",
            "draft": false, "prerelease": false,
            "assets": [
                {"name": "SHA256SUMS.txt", "size": 98, "browser_download_url": "https://x/SHA256SUMS.txt"},
                {"name": "GhostDesk-0.3.0-windows-x64.exe", "size": 123, "browser_download_url": "https://x/a.exe"}
            ]
        });
        let r = parse_release(&v, "0.2.0", &["-windows-x64.exe"]).unwrap();
        assert_eq!(r.version, "0.3.0");
        assert!(r.installable());
        let a = r.asset.unwrap();
        assert_eq!((a.size, a.url.as_str(), a.sums_url.as_deref()), (123, "https://x/a.exe", Some("https://x/SHA256SUMS.txt")));
        // 내려받을 파일이 없는 OS: 알리기만 (페이지 열기)
        assert!(!parse_release(&v, "0.2.0", &[]).unwrap().installable());
        // 같은 버전 · 미리보기 · 초안은 알리지 않는다
        assert_eq!(parse_release(&v, "0.3.0", &[]), None);
        let mut pre = v.clone();
        pre["prerelease"] = json!(true);
        assert_eq!(parse_release(&pre, "0.2.0", &[]), None);
    }

    /// macOS: 이 CPU 전용 파일이 있으면 그것, 없으면 유니버설. Windows 파일은 고르지 않는다
    #[test]
    fn picks_the_mac_build_for_this_cpu() {
        let rel = |names: &[&str]| {
            json!({
                "tag_name": "v0.3.0", "draft": false, "prerelease": false,
                "assets": names.iter().map(|n| json!({"name": n, "size": 1, "browser_download_url": format!("https://x/{n}")})).collect::<Vec<_>>()
            })
        };
        let arm = ["-macos-arm64.zip", "-macos-universal.zip"];
        let both = rel(&["GhostDesk-0.3.0-windows-x64.exe", "GhostDesk-0.3.0-macos-universal.zip", "GhostDesk-0.3.0-macos-arm64.zip"]);
        assert_eq!(parse_release(&both, "0.2.0", &arm).unwrap().asset.unwrap().name, "GhostDesk-0.3.0-macos-arm64.zip");
        let universal = rel(&["GhostDesk-0.3.0-windows-x64.exe", "GhostDesk-0.3.0-macos-universal.zip"]);
        assert_eq!(parse_release(&universal, "0.2.0", &arm).unwrap().asset.unwrap().name, "GhostDesk-0.3.0-macos-universal.zip");
        let win_only = rel(&["GhostDesk-0.3.0-windows-x64.exe"]);
        assert!(!parse_release(&win_only, "0.2.0", &arm).unwrap().installable(), "macOS 파일이 없으면 페이지만 연다");
        // 지금 실행 환경의 목록이 비어 있지 않다 (지원하는 OS·CPU)
        if cfg!(any(windows, target_os = "macos")) {
            assert!(!asset_suffixes().is_empty());
        }
    }

    #[test]
    fn reads_sha256sums() {
        let sums = "ABCDEF  GhostDesk-0.3.0-windows-x64.exe\r\n0123 *other.exe\n";
        assert_eq!(sum_for(sums, "GhostDesk-0.3.0-windows-x64.exe").as_deref(), Some("abcdef"));
        assert_eq!(sum_for(sums, "other.exe").as_deref(), Some("0123"));
        assert_eq!(sum_for(sums, "missing.exe"), None);
    }

    /// 실제 GitHub 최신 릴리스를 받아 가짜 exe 자리에 바꿔 넣는다 (네트워크 필요, 약 30MB):
    /// cargo test --bin ghostdesk live_update_roundtrip -- --ignored
    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    #[ignore]
    fn live_update_roundtrip() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt.block_on(latest("0.0.1")).unwrap().expect("최신 릴리스가 있어야 함");
        let asset = r.asset.clone().expect("Windows 실행 파일 자산이 있어야 함");
        let dir = std::env::temp_dir().join(format!("ghostdesk-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("GhostDesk-old.exe");
        std::fs::write(&target, b"old").unwrap();
        let last = std::sync::Mutex::new(0.0f32);
        rt.block_on(install(&asset, &target, |f| *last.lock().unwrap() = f)).unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().len(), asset.size);
        assert!((*last.lock().unwrap() - 1.0).abs() < 1e-6, "진행률이 끝까지 가야 함");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 1, "받던 파일이 남으면 안 됨: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// macOS: 받은 zip(안에 GhostDesk.app)을 풀어 실행 중인 번들 자리에 넣고, 옛 번들은 옆으로 옮기고, 받은 것들은 치운다
    #[cfg(target_os = "macos")]
    #[test]
    fn mac_swap_replaces_the_bundle() {
        let dir = std::env::temp_dir().join(format!("ghostdesk-swap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let write = |app: &Path, body: &str| {
            std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
            std::fs::write(app.join("Contents/MacOS/ghostdesk"), body).unwrap();
        };
        let target = dir.join("GhostDesk.app");
        write(&target, "old");
        let new_app = dir.join("new").join("GhostDesk.app");
        write(&new_app, "new");
        let zip = dir.join("GhostDesk.app.update-1");
        let ok = std::process::Command::new("/usr/bin/ditto").args(["-c", "-k", "--keepParent"]).arg(&new_app).arg(&zip).status().unwrap();
        assert!(ok.success());

        swap(&zip, &target).unwrap();
        assert_eq!(std::fs::read_to_string(target.join("Contents/MacOS/ghostdesk")).unwrap(), "new");
        let left: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(left.iter().any(|n| n.starts_with("GhostDesk.app.old-")), "{left:?}");
        assert!(!left.iter().any(|n| n.contains(".update-")), "받은 zip·푼 폴더가 남으면 안 됨: {left:?}");

        // zip 안에 GhostDesk.app 이 없으면 아무것도 바꾸지 않는다
        let bad = dir.join("bad.zip");
        std::fs::write(dir.join("x.txt"), "x").unwrap();
        std::process::Command::new("/usr/bin/ditto").args(["-c", "-k"]).arg(dir.join("x.txt")).arg(&bad).status().unwrap();
        assert!(swap(&bad, &target).is_err());
        assert_eq!(std::fs::read_to_string(target.join("Contents/MacOS/ghostdesk")).unwrap(), "new");

        // 설치 위치에 아직 앱이 없어도 (내려받은 곳에서 바로 연 앱의 업데이트) 새 앱을 넣는다
        let fresh = dir.join("fresh").join("GhostDesk.app");
        std::fs::create_dir_all(fresh.parent().unwrap()).unwrap();
        let zip2 = dir.join("again.zip");
        std::process::Command::new("/usr/bin/ditto").args(["-c", "-k", "--keepParent"]).arg(&new_app).arg(&zip2).status().unwrap();
        swap(&zip2, &fresh).unwrap();
        assert_eq!(std::fs::read_to_string(fresh.join("Contents/MacOS/ghostdesk")).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn translocated_app_updates_the_installed_copy() {
        let t = Path::new("/private/var/folders/x/T/AppTranslocation/158384EE/d/GhostDesk.app");
        assert_eq!(mac_target(t), crate::install::stable_app());
        let normal = Path::new("/Applications/GhostDesk.app");
        assert_eq!(mac_target(normal), normal);
    }

    #[test]
    fn sibling_keeps_folder() {
        let p = Path::new("dir").join("GhostDesk-0.1.1-windows-x64.exe");
        assert_eq!(sibling(&p, ".update-7"), Path::new("dir").join("GhostDesk-0.1.1-windows-x64.exe.update-7"));
    }
}
