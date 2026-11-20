//! Update notifications: looks up the latest GitHub release and announces it if it's newer than
//! the running version.
//!
//! "Update" downloads the release file for this OS and CPU (`asset_suffixes`), verifies it
//! against SHA256SUMS and swaps it in place of the running copy; "Restart" launches the new
//! version once the app has fully exited. Since the running copy itself is replaced, whatever the
//! user opens next time (the downloaded exe or the installed one) is the new version.
//! - Windows: `GhostDesk-<version>-windows-<x64|arm64>.exe`; the running exe is moved aside and
//!   the new one put in its place
//! - macOS: `GhostDesk-<version>-macos-<arm64|x64|universal>.zip` (containing GhostDesk.app);
//!   the running .app bundle is replaced as a whole
//!
//! If the release has no file for this OS and CPU, or on macOS we're running outside an .app
//! bundle, the release page is opened instead.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use eframe::egui;
use serde_json::Value;
use tokio::runtime::Handle;

/// Repository releases are published to.
const REPO: &str = "r2adkr/ghostdesk";
/// The "Release notes" link in Settings.
pub const RELEASES_PAGE: &str = "https://github.com/r2adkr/ghostdesk/releases";
/// Release file name endings to download on this OS and CPU, in order of preference (macOS falls
/// back to universal when there's no CPU-specific build).
///
/// Must match the file names in .github/workflows/release.yml.
fn asset_suffixes() -> &'static [&'static str] {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => &["-windows-x64.exe"],
        ("windows", "aarch64") => &["-windows-arm64.exe"],
        ("macos", "aarch64") => &["-macos-arm64.zip", "-macos-universal.zip"],
        ("macos", "x86_64") => &["-macos-x64.zip", "-macos-universal.zip"],
        _ => &[],
    }
}
/// The app tends to stay open for a long time, so check again every few hours.
const RECHECK: Duration = Duration::from_secs(6 * 60 * 60);

/// Wait before the next try after `failures` failed checks in a row: 2, 5, 15, 30 minutes, then hourly.
fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs(match failures {
        0 | 1 => 120,
        2 => 300,
        3 => 900,
        4 => 1800,
        _ => 3600,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    /// "0.2.0" (leading v stripped).
    pub version: String,
    /// Release page (what's new, manual download).
    pub page: String,
    asset: Option<Asset>,
}

#[derive(Clone, Debug, PartialEq)]
struct Asset {
    name: String,
    url: String,
    size: u64,
    /// SHA256SUMS.txt of the same release (size check only if missing).
    sums_url: Option<String>,
}

impl Release {
    /// Whether it can be swapped in from inside the app (otherwise the release page opens).
    pub fn installable(&self) -> bool {
        self.asset.is_some()
    }
}

#[derive(Clone, Default, PartialEq)]
pub enum State {
    /// Not checked yet, already up to date, or the check failed (quietly, so being offline isn't a
    /// nuisance).
    #[default]
    None,
    Available(Release),
    /// Downloading (0..=1).
    Downloading(Release, f32),
    /// The new exe is in place; restart to apply.
    Ready(Release),
    Failed(Release, String),
}

pub type Shared = Arc<parking_lot::Mutex<State>>;

/// Checks for the latest release at startup and once a day after that. Also cleans up files left
/// by the previous update.
pub fn start(rt: &Handle, ctx: &egui::Context) -> Shared {
    let st = Shared::default();
    let (s, ctx) = (st.clone(), ctx.clone());
    rt.spawn_blocking(sweep_leftovers);
    rt.spawn(async move {
        // a failed check (offline, GitHub's request limit) is retried soon, not a day later
        let mut failures = 0u32;
        loop {
            let wait = match latest(env!("CARGO_PKG_VERSION")).await {
                Ok(found) => {
                    failures = 0;
                    if let Some(r) = found {
                        let mut g = s.lock();
                        // leave a download in progress or one already done alone
                        if !matches!(*g, State::Downloading(..) | State::Ready(_)) {
                            *g = State::Available(r);
                            ctx.request_repaint();
                        }
                    }
                    RECHECK
                }
                Err(_) => {
                    failures += 1;
                    retry_delay(failures)
                }
            };
            tokio::time::sleep(wait).await;
        }
    });
    st
}

/// Settings → "Check for updates": asks now instead of waiting for the daily check. The answer is
/// `Ok(true)` for a newer version (also put into `st` so the update toast appears), `Ok(false)` when
/// this is the latest, or why it could not be checked.
pub type CheckSlot = Arc<parking_lot::Mutex<Option<std::result::Result<bool, String>>>>;

pub fn check_now(rt: &Handle, st: Shared, ctx: egui::Context) -> CheckSlot {
    let slot = CheckSlot::default();
    let out = slot.clone();
    rt.spawn(async move {
        let res = match latest(env!("CARGO_PKG_VERSION")).await {
            Ok(Some(r)) => {
                let mut g = st.lock();
                if !matches!(*g, State::Downloading(..) | State::Ready(_)) {
                    *g = State::Available(r);
                }
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(e) => Err(format!("{e:#}")),
        };
        *out.lock() = Some(res);
        ctx.request_repaint();
    });
    slot
}

/// "Update": downloads the new exe and puts it where the running one is (progress in
/// `State::Downloading`).
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

/// "Restart": after the window closes, main launches the new exe via `relaunch_if_requested`.
static RELAUNCH: AtomicBool = AtomicBool::new(false);

pub fn request_relaunch(ctx: &egui::Context) {
    RELAUNCH.store(true, Ordering::SeqCst);
    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
}

/// Called by main after the GUI exits and the single-instance mutex is released (launching
/// earlier would make the new window see the old one and just exit).
pub fn relaunch_if_requested() {
    if !RELAUNCH.load(Ordering::SeqCst) {
        return;
    }
    // macOS: launch the swapped-in .app bundle through LaunchServices (-n: separate from the
    // exiting process)
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
        // it's a console program, so a plain launch would open a console window
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let _ = cmd.spawn();
}

fn client() -> Result<reqwest::Client> {
    // the GitHub API requires a User-Agent (only the app name and version are sent)
    Ok(reqwest::Client::builder()
        .user_agent(concat!("GhostDesk/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()?)
}

/// A stable release newer than `current`, if any.
async fn latest(current: &str) -> Result<Option<Release>> {
    // with nowhere to swap into (macOS, running outside a bundle) pick no file, so the release
    // page opens
    let suffixes = if update_target().is_ok() { asset_suffixes() } else { &[] };
    match latest_from_api(current, suffixes).await {
        Ok(r) => Ok(r),
        // the API allows few requests per address and hour (shared networks hit that often): the
        // release pages themselves are not limited
        Err(api_err) => latest_from_pages(current, suffixes).await.map_err(|e| e.context(format!("{api_err:#}"))),
    }
}

async fn latest_from_api(current: &str, suffixes: &[&str]) -> Result<Option<Release>> {
    let v: Value = client()?
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(30))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(parse_release(&v, current, suffixes))
}

/// Without the API: `/releases/latest` redirects to the newest stable release's tag, and its
/// SHA256SUMS.txt names the files.
async fn latest_from_pages(current: &str, suffixes: &[&str]) -> Result<Option<Release>> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("GhostDesk/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .build()?;
    let resp = http.get(format!("https://github.com/{REPO}/releases/latest")).send().await?;
    let location = resp.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()).context("최신 릴리스 주소를 알 수 없습니다")?;
    let tag = location.rsplit('/').next().unwrap_or_default().to_string();
    if tag.is_empty() || !newer(tag.trim_start_matches('v'), current) {
        return Ok(None);
    }
    let sums = http.get(format!("https://github.com/{REPO}/releases/download/{tag}/SHA256SUMS.txt")).send().await.and_then(|r| r.error_for_status());
    let sums = match sums {
        Ok(r) => r.text().await.ok(),
        Err(_) => None,
    };
    Ok(release_from_sums(&tag, sums.as_deref(), suffixes))
}

/// A release from its tag and SHA256SUMS.txt (lines of "<hash>  <file name>"): no sums means no file,
/// so the release page opens instead.
fn release_from_sums(tag: &str, sums: Option<&str>, suffixes: &[&str]) -> Option<Release> {
    let version = tag.trim_start_matches('v').to_string();
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    let names: Vec<&str> = sums.unwrap_or_default().lines().filter_map(|l| l.trim().split_once(char::is_whitespace).map(|(_, f)| f.trim().trim_start_matches('*'))).collect();
    let asset = suffixes.iter().find_map(|suf| {
        let name = names.iter().find(|n| n.ends_with(suf))?;
        Some(Asset { name: name.to_string(), url: format!("{base}/{name}"), size: 0, sums_url: Some(format!("{base}/SHA256SUMS.txt")) })
    });
    Some(Release { version, page: format!("https://github.com/{REPO}/releases/tag/{tag}"), asset })
}

/// What an update replaces: the running exe on Windows, the .app bundle containing it on macOS.
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

/// An app opened straight from a downloaded zip runs from a read-only temporary location
/// (AppTranslocation) on macOS.
///
/// That can't be replaced, so the new version goes to the install location
/// (~/Applications/GhostDesk.app) and is reopened from there.
fn mac_target(bundle: &Path) -> PathBuf {
    if bundle.to_string_lossy().contains("/AppTranslocation/") {
        crate::install::stable_app()
    } else {
        bundle.to_path_buf()
    }
}

/// GitHub release JSON → a stable release newer than `current`. Picks the first file matching
/// `suffixes`, in order.
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

/// Whether `candidate` is newer than `current` ("0.2.0" > "0.1.10", "0.2" == "0.2.0", suffixes
/// like -beta ignored).
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

/// The hash for `name` in SHA256SUMS.txt (lines of "<hash>  <file name>").
fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, file) = l.trim().split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
    })
}

/// Download → verify size and hash → swap with `target` (the running exe or .app bundle). The
/// partial download is deleted on failure.
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
    // download into the same folder so a rename is enough to swap it in
    let staging = sibling(&me, &format!(".update-{}", std::process::id()));
    if let Some(dir) = me.parent() {
        tokio::fs::create_dir_all(dir).await.with_context(|| format!("{} 폴더를 만들 수 없습니다", dir.display()))?;
    }
    let res: Result<()> = async {
        let mut resp = http.get(&asset.url).send().await?.error_for_status()?;
        // the size is unknown when the release came from its page instead of the API
        let total = if asset.size > 0 { asset.size } else { resp.content_length().unwrap_or(0) };
        let mut f = tokio::fs::File::create(&staging)
            .await
            .with_context(|| format!("{} 에 새 버전을 쓸 수 없습니다", staging.display()))?;
        let mut got = 0u64;
        while let Some(chunk) = resp.chunk().await? {
            f.write_all(&chunk).await?;
            got += chunk.len() as u64;
            progress(got as f32 / total.max(1) as f32);
        }
        f.flush().await?;
        drop(f);
        anyhow::ensure!(total == 0 || got == total, "내려받은 크기가 다릅니다 ({got} / {total} 바이트)");
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
    // a running exe can only be renamed: move it aside (<name>.old-<time>) and put the new file in
    // its place
    crate::install::swap_in(staging, me)
}

/// macOS: unpacks the zip into a sibling folder, checks the GhostDesk.app inside, moves the
/// running bundle aside (`<name>.old-<time>`) and puts the new one in its place.
///
/// The running process keeps using the moved old files. Rolls back if putting it in place fails.
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
        // no app at the install location yet (opened from the download location without moving it):
        // just put the new one there
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

/// `<file name of p><suffix>` in the same folder as `p`.
fn sibling(p: &Path, suffix: &str) -> PathBuf {
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    p.with_file_name(name)
}

/// Leftovers from the previous update (old exe or bundle moved aside, partial downloads). An old
/// exe that's still running can't be deleted and is retried next time.
///
/// Only removes items named by the updater that start with the update target's (exe or .app)
/// name.
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
        // OS without a downloadable file: notify only (opens the page)
        assert!(!parse_release(&v, "0.2.0", &[]).unwrap().installable());
        // same version, prereleases and drafts aren't announced
        assert_eq!(parse_release(&v, "0.3.0", &[]), None);
        let mut pre = v.clone();
        pre["prerelease"] = json!(true);
        assert_eq!(parse_release(&pre, "0.2.0", &[]), None);
    }

    /// macOS: the CPU-specific file if present, otherwise universal. Never picks the Windows file.
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
        // the list for the current platform isn't empty (supported OS and CPU)
        if cfg!(any(windows, target_os = "macos")) {
            assert!(!asset_suffixes().is_empty());
        }
    }

    #[test]
    fn a_release_is_found_from_its_sums_file_without_the_api() {
        let sums = "aa11  GhostDesk-0.12.0-windows-x64.exe\nbb22 *GhostDesk-0.12.0-macos-universal.zip\n";
        let r = release_from_sums("v0.12.0", Some(sums), &["-windows-x64.exe"]).unwrap();
        assert_eq!(r.version, "0.12.0");
        assert!(r.installable());
        assert!(r.asset.as_ref().unwrap().url.ends_with("/v0.12.0/GhostDesk-0.12.0-windows-x64.exe"));
        // nothing for this OS, or no sums: the release page opens instead
        assert!(!release_from_sums("v0.12.0", Some(sums), &["-linux.tar"]).unwrap().installable());
        assert!(!release_from_sums("v0.12.0", None, &["-windows-x64.exe"]).unwrap().installable());
    }

    #[test]
    fn failed_checks_are_retried_soon_then_hourly() {
        assert!(retry_delay(1) < Duration::from_secs(600));
        assert!(retry_delay(3) < retry_delay(6));
        assert_eq!(retry_delay(99), Duration::from_secs(3600));
    }

    #[test]
    fn reads_sha256sums() {
        let sums = "ABCDEF  GhostDesk-0.3.0-windows-x64.exe\r\n0123 *other.exe\n";
        assert_eq!(sum_for(sums, "GhostDesk-0.3.0-windows-x64.exe").as_deref(), Some("abcdef"));
        assert_eq!(sum_for(sums, "other.exe").as_deref(), Some("0123"));
        assert_eq!(sum_for(sums, "missing.exe"), None);
    }

    /// Downloads the real latest GitHub release and swaps it in for a fake exe (needs network,
    /// about 30 MB):
    /// `cargo test --bin ghostdesk live_update_roundtrip -- --ignored`
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

    /// macOS: unpacks the zip (with GhostDesk.app inside) into the running bundle's place, moves the
    /// old bundle aside and cleans up the download.
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

        // nothing is replaced if the zip has no GhostDesk.app
        let bad = dir.join("bad.zip");
        std::fs::write(dir.join("x.txt"), "x").unwrap();
        std::process::Command::new("/usr/bin/ditto").args(["-c", "-k"]).arg(dir.join("x.txt")).arg(&bad).status().unwrap();
        assert!(swap(&bad, &target).is_err());
        assert_eq!(std::fs::read_to_string(target.join("Contents/MacOS/ghostdesk")).unwrap(), "new");

        // installs the new app even with nothing at the install location yet (updating an app opened
        // straight from the download location)
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
