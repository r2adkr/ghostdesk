//! Sets up isolated screens: launches or connects whatever a `DeskTarget` describes and builds
//! the `Desk`.

use std::{path::{Path, PathBuf}, sync::Arc, time::Duration};

use anyhow::{anyhow, bail, Context, Result};

use super::{AgentShared, AgentStatus, DeskTarget, LogKind, ProfileSource, RemoteTarget};
use crate::{
    browser::{BrowserWorkspace, LaunchOptions},
    desk::Desk,
    profiles,
};

pub(crate) async fn prepare_workspace(shared: &Arc<AgentShared>, workspace: &str, target: &DeskTarget) -> Result<Desk> {
    prepare_workspace_with(shared, workspace, target, None).await
}

/// Like `prepare_workspace`, but the caller decides whether a remote target (kind=remote) may
/// end up connected to this machine itself (outside macOS only; ignored on macOS).
///
/// `None` follows the target's `dedicated_ack` (GUI, self-test). MCP requests (control.rs)
/// pass `Some(true)` only when a person saw the "this computer's own VNC server" warning in the
/// approval dialog, tying the approved target to the address actually connected.
pub(crate) async fn prepare_workspace_with(
    shared: &Arc<AgentShared>,
    workspace: &str,
    target: &DeskTarget,
    remote_local_ok: Option<bool>,
) -> Result<Desk> {
    match target {
        DeskTarget::Browser { source, headless, width, height, start_url } => {
            prepare_browser(shared, workspace, source, *headless, *width, *height, start_url).await
        }
        DeskTarget::Remote(t) => {
            let desk = prepare_remote(shared, t, remote_local_ok.unwrap_or(t.dedicated_ack)).await?;
            if let Some(options) = &t.ssh { *shared.ssh.lock() = Some(crate::ssh::SshConnection::new(options.clone())?); }
            Ok(desk)
        }
        DeskTarget::Ssh(options) => {
            let ssh = crate::ssh::SshConnection::new(options.clone())?;
            // Verify authentication/host key before reporting this workspace as connected.
            let result = ssh.execute("echo GhostDesk", 20, &shared.stop).await?;
            if result["success"] != true { bail!("SSH connection failed: {}", result["stderr"]); }
            *shared.ssh.lock() = Some(ssh.clone());
            Ok(Desk::Ssh(ssh))
        },
        DeskTarget::Electron { app, profile, width, height } => {
            prepare_electron(shared, workspace, app, profile, *width, *height).await
        }
        DeskTarget::Native(opts) => prepare_native(shared, opts).await,
        DeskTarget::VDisplay(o) => {
            shared.set_status(AgentStatus::Preparing("가상 디스플레이 준비 중".into()));
            let sh = shared.clone();
            let ws = crate::vdisplay::VirtualDisplayDesk::launch(o.clone(), Arc::new(move |b: &[u8]| sh.push_frame(b))).await?;
            for line in ws.report() {
                let kind = if line.contains("경고") { LogKind::Error } else { LogKind::Info };
                shared.log(kind, line);
            }
            Ok(Desk::VDisplay(ws))
        }
        DeskTarget::Host { code } => {
            shared.set_status(AgentStatus::Preparing("호스트 컴퓨터에 연결 중".into()));
            let bridge = crate::host::connect(code).await?;
            let sh = shared.clone();
            let d = crate::session::SessionDesk::connect_target(bridge.target.clone(), None, "호스트 컴퓨터", Arc::new(move |b: &[u8]| sh.push_frame(b)), Default::default()).await?;
            d.keep_alive(Box::new(bridge));
            let (w, h) = d.size();
            shared.log(LogKind::Info, format!("호스트 컴퓨터에 연결했습니다 ({w}x{h})"));
            Ok(Desk::Session(d))
        }
        DeskTarget::Session { account } => {
            // one agent at a time on this session screen: wait for the turn (turns.rs)
            let turn = take_session_turn(shared, account).await?;
            if cfg!(windows) {
                // the child session signs in as this account again; Windows may ask for the password
                shared.set_status(AgentStatus::Preparing("자식 세션에 로그인하는 중 · 'Windows 보안' 창이 뜨면 계정 암호를 넣으세요".into()));
                shared.log(
                    LogKind::Info,
                    "자식 세션은 지금 계정으로 한 번 더 로그인합니다. 'Windows 보안' 창이 뜨면 PIN 이 아니라 계정 암호를 넣고, '내 자격 증명 기억' 을 체크하면 다음부터 묻지 않습니다",
                );
            } else {
                shared.set_status(AgentStatus::Preparing(format!("'{account}' 세션에 연결 중")));
            }
            let sh = shared.clone();
            let d = crate::session::SessionDesk::connect(account, Arc::new(move |b: &[u8]| sh.push_frame(b))).await?;
            d.hold(turn);
            let (w, h) = d.size();
            shared.log(LogKind::Info, format!("전용 계정 '{account}' 의 별도 세션에 연결했습니다 ({w}x{h})"));
            Ok(Desk::Session(d))
        }
    }
}

/// Waits for this agent's turn on the session screen (the Windows child session, or a macOS
/// dedicated account's session), showing who it waits for. Agents inside GhostDesk wait until
/// stopped; external agents give up after 40 s (their MCP call would time out) with who has it.
async fn take_session_turn(shared: &Arc<AgentShared>, account: &str) -> Result<crate::turns::Turn> {
    use std::sync::atomic::Ordering;
    let (key, what) = if cfg!(windows) {
        (crate::turns::CHILD_SESSION.to_string(), "자식 세션".to_string())
    } else {
        (crate::turns::account_key(account), format!("'{account}' 세션"))
    };
    let external = shared.external.lock().clone();
    let who = match &external {
        Some(client) => format!("{client} ({})", shared.name),
        None => shared.name.clone(),
    };
    // an outside agent's call must end before its client gives up: ~55 s (Codex waits 60 s by default), or minutes when
    // the client keeps the call alive with progress notices (mcp.rs sends them and says `patient`)
    let patient = shared.queue_patient.load(Ordering::Relaxed);
    let max_wait = external.is_some().then(|| Duration::from_secs(if patient { 600 } else { 55 }));
    let sh = shared.clone();
    let on_wait = move |owner: &str, place: usize| {
        sh.set_status(AgentStatus::Preparing(format!("{what} 차례를 기다리는 중 · 지금 {owner} 사용 중 · 대기 {place}번째")));
        sh.log(LogKind::Info, format!("{what} 은(는) 한 번에 한 에이전트만 쓸 수 있어 {owner} 이(가) 끝나기를 기다립니다"));
    };
    let sh = shared.clone();
    let cancelled = move || sh.stop.load(Ordering::SeqCst) || sh.removed.load(Ordering::SeqCst);
    // an outside agent is one connection: its place in line outlives its (short) call
    let conn = external.is_some().then(|| shared.team_member_id.load(Ordering::Relaxed)).filter(|c| *c != 0);
    crate::turns::global().take_as(conn, &key, &who, max_wait, on_wait, cancelled).await
}

async fn prepare_electron(
    shared: &Arc<AgentShared>,
    workspace: &str,
    app: &crate::electron::ElectronApp,
    profile: &crate::electron::ElectronProfile,
    width: u32,
    height: u32,
) -> Result<Desk> {
    use crate::electron::{self, ElectronLaunch, ElectronProfile};
    if !app.hideable {
        return Err(anyhow!(
            "{} 은(는) 숨김 실행을 지원하지 않습니다: {}",
            app.name,
            app.reason.clone().unwrap_or_default()
        ));
    }
    let mut o = ElectronLaunch::new(app.clone(), workspace, width, height);
    o.profile = profile.clone();
    if let ElectronProfile::Clone { source, .. } = profile {
        shared.log(LogKind::Info, format!("{} 데이터({})를 작업공간으로 복제합니다", app.name, source.display()));
    }
    let sh = shared.clone();
    o.progress = Some(Arc::new(move |files, bytes| {
        sh.set_status(AgentStatus::Preparing(format!("앱 데이터 복제 중 ({files}개, {:.0} MB)", bytes as f64 / 1e6)));
    }));
    shared.set_status(AgentStatus::Preparing(format!("{} 숨김 실행 중", app.name)));
    let sh = shared.clone();
    let ws = electron::launch(o, Arc::new(move |b: &[u8]| sh.push_frame(b))).await?;
    shared.log(
        LogKind::Info,
        format!("{} 를 화면에 보이지 않게 실행했습니다 ({width}x{height}, pid {})", app.name, ws.main_pid),
    );
    Ok(Desk::Electron(ws))
}

async fn prepare_native(shared: &Arc<AgentShared>, opts: &crate::native::NativeOptions) -> Result<Desk> {
    shared.set_status(AgentStatus::Preparing(format!("{} 준비 중", opts.app)));
    let sh = shared.clone();
    let ws = crate::native::NativeAppWorkspace::launch(opts.clone(), Arc::new(move |b: &[u8]| sh.push_frame(b))).await?;
    let mode = match ws.mode() {
        crate::native::NativeMode::Launched => "숨김 인스턴스를 새로 실행",
        crate::native::NativeMode::Attached => "사용 중인 앱에 연결 (작업 중 앱 창이 숨겨짐)",
    };
    shared.log(LogKind::Info, format!("{}: {mode} (pid {})", ws.app_name(), ws.pid()));
    for e in ws.exposure_report() {
        shared.log(LogKind::Error, format!("화면 노출 감지: {e}"));
    }
    Ok(Desk::Native(ws))
}

/// Error for connecting to this machine's own VNC server without confirmation (outside macOS).
///
/// Such a server usually exports the real screen the user is looking at, so the agent would be
/// driving the user's screen, mouse and keyboard directly. Separate desktops forwarded to this
/// machine's address (WSL, Hyper-V) are allowed after confirmation. MCP requests are refused
/// with the same message by control.rs before asking anyone.
#[cfg(not(target_os = "macos"))]
pub(crate) fn local_vnc_refusal(host: &str, port: u16) -> String {
    use crate::platform::text::THIS_COMPUTER;
    format!(
        "{host}:{port} 은(는) {THIS_COMPUTER} 자신에서 실행 중인 VNC 서버입니다. 여기에 접속하면 에이전트가 지금 보고 계신 \
         실제 화면·마우스·키보드를 조작하게 될 수 있습니다. WSL·Hyper-V 가상 머신처럼 {THIS_COMPUTER} 주소로 전달되는 \
         별도 데스크톱이 확실하다면 '에이전트 전용 머신' 확인란을 켜고 다시 시작하세요."
    )
}

/// Error for a remote target that looked like another machine at approval time but actually
/// connected to this one (outside macOS).
///
/// The dedicated-machine confirmation was given, but nobody saw the "this computer itself"
/// warning: the DNS answer changed, or another name pointing here was used.
#[cfg(not(target_os = "macos"))]
fn local_vnc_mismatch(host: &str, port: u16) -> String {
    use crate::platform::text::THIS_COMPUTER;
    format!(
        "{host}:{port} 은(는) 승인할 때 {THIS_COMPUTER} 가 아닌 대상으로 보였지만, 실제로 접속한 주소는 {THIS_COMPUTER} 자신입니다. \
         지금 보고 계신 실제 화면일 수 있어 접속하지 않습니다. {THIS_COMPUTER} 의 VNC 서버가 맞다면 host 를 127.0.0.1 처럼 \
         주소로 직접 적어 다시 요청하세요."
    )
}

/// `local_ok`: whether to connect even if the address actually reached is this machine
/// (outside macOS only). Usually equal to `t.dedicated_ack`; MCP requests set it only when a
/// person saw the "this computer itself" warning.
async fn prepare_remote(shared: &Arc<AgentShared>, t: &RemoteTarget, local_ok: bool) -> Result<Desk> {
    #[cfg(target_os = "macos")]
    let _ = local_ok;
    use crate::rfb::{self, RfbDesk, RfbOptions};
    // `vnc://host:5901` and `host:5901` typed into the address field mean what they say
    let mut cleaned = t.clone();
    (cleaned.host, cleaned.port) = rfb::normalize_endpoint(&t.host, t.port);
    let t = &cleaned;
    shared.set_status(AgentStatus::Preparing("원격 데스크톱 확인 중".into()));
    let g = rfb::probe(&t.host, t.port, rfb::ReplyVersion::Auto, Duration::from_secs(6)).await?;
    let types: Vec<&str> = g.security_types.iter().map(|x| rfb::security_type_name(*x)).collect();
    shared.log(LogKind::Info, format!("{}:{} — {} / 보안 방식: {}", t.host, t.port, g.server_version, types.join(", ")));
    let apple = g.server_version.contains("003.889");
    // this Mac's own Screen Sharing (Apple server) is always refused, confirmation or not. That's a
    // macOS-only rule: on other OSes an Apple server on loopback is just a tunnel to another Mac.
    #[cfg(target_os = "macos")]
    if apple && rfb::is_local_host(&t.host, t.port).await {
        return Err(anyhow!(
            "이 Mac 자신의 화면 공유 서버에는 연결하지 않습니다. GhostDesk 는 Apple 의 세션 선택 프로토콜을 구현하지 않아, \
             다른 계정으로 인증하면 지금 쓰고 있는 화면이 그 계정으로 전환될 위험이 있습니다. \
             같은 Mac 의 별도 계정을 쓰려면 'RDP식 별도 계정 세션' 방식을 사용하세요."
        ));
    }
    if apple && !t.dedicated_ack {
        return Err(anyhow!(
            "대상이 macOS 화면 공유 서버입니다. 그 Mac 의 콘솔 사용자와 다른 계정으로 접속하면 그 Mac 의 화면이 전환될 수 있습니다. \
             대상이 에이전트 전용 Mac/VM 이라면 '에이전트 전용 머신' 확인란을 켜고 다시 시작하세요."
        ));
    }
    // outside macOS: this machine's own VNC server (TightVNC, UltraVNC, RealVNC, …) may be the
    // user's real screen, so only connect with the dedicated-machine confirmation.
    // decide on the address actually reached (the probe connection's peer), not just a fresh name
    // lookup, and check again on the real connection's socket so a changing DNS answer can't slip
    // through. (Refused despite confirmation means an MCP request whose approver never saw the
    // "this computer itself" warning.)
    #[cfg(not(target_os = "macos"))]
    let refuse_local = (!local_ok).then(|| {
        if t.dedicated_ack {
            local_vnc_mismatch(&t.host, t.port)
        } else {
            local_vnc_refusal(&t.host, t.port)
        }
    });
    #[cfg(not(target_os = "macos"))]
    if let Some(why) = &refuse_local {
        if g.peer.is_some_and(rfb::is_local_addr) || rfb::is_local_host(&t.host, t.port).await {
            return Err(anyhow!("{why}"));
        }
    }
    let mut o = RfbOptions::new(&t.host, t.port);
    o.username = t.username.clone().filter(|s| !s.is_empty());
    o.password = t.password.clone().filter(|s| !s.is_empty());
    o.max_agent_size = Some((1280, 800));
    #[cfg(not(target_os = "macos"))]
    {
        o.refuse_local = refuse_local;
    }
    shared.set_status(AgentStatus::Preparing("원격 데스크톱 접속 중".into()));
    let ctx = shared.ctx.clone();
    let desk = RfbDesk::connect_with(o, Arc::new(move || ctx.request_repaint())).await?;
    let (fw, fh) = desk.fb_size();
    shared.log(
        LogKind::Info,
        format!("원격 데스크톱 접속됨: '{}' ({fw}x{fh}, 에이전트 좌표 {:?})", desk.desktop_name(), desk.size()),
    );
    // live screen: event-driven, pushed to the UI as soon as the framebuffer changes (coalesced to the user's fps limit)
    let (sh, weak) = (shared.clone(), Arc::downgrade(&desk));
    let mut rx = desk.subscribe_frames();
    tokio::spawn(async move {
        loop {
            if rx.changed().await.is_err() {
                break;
            }
            let Some(d) = weak.upgrade() else { break };
            if d.is_closed() {
                break;
            }
            d.with_frame(|w, h, rgba| sh.push_rgba(w, h, rgba));
            drop(d);
            tokio::time::sleep(Duration::from_millis(1000 / rfb::live_fps_limit().max(1) as u64)).await;
        }
    });
    Ok(Desk::Remote(desk))
}

const BROWSER_SOURCE_MARKER: &str = ".ghostdesk-browser-source.json";

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct BrowserCloneSource {
    data_dir: PathBuf,
    profile_dir: String,
}

fn browser_clone_reusable(dir: &Path, source: &BrowserCloneSource, refresh: bool) -> Result<bool> {
    if refresh || dir.join(".ghostdesk-cloning").exists() || !dir.exists() {
        return Ok(false);
    }
    if std::fs::read_dir(dir)?.next().transpose()?.is_none() {
        return Ok(false);
    }
    let previous = std::fs::read_to_string(dir.join(BROWSER_SOURCE_MARKER)).ok()
        .and_then(|text| serde_json::from_str::<BrowserCloneSource>(&text).ok());
    if previous.as_ref() != Some(source) || !dir.join(&source.profile_dir).is_dir() {
        bail!("기존 작업공간 '{}' 의 브라우저·프로필 출처가 선택한 로그인 원본과 다르거나 확인되지 않습니다. 기존 내용은 유지했습니다. 다른 작업공간 이름을 쓰거나, 기존 복제본을 교체하려면 refresh_login=true 로 새로 복제하세요.", dir.display());
    }
    Ok(true)
}

async fn prepare_browser(
    shared: &Arc<AgentShared>,
    workspace: &str,
    source: &ProfileSource,
    headless: bool,
    width: u32,
    height: u32,
    start_url: &str,
) -> Result<Desk> {
    let dir: PathBuf = profiles::workspaces_root().join(profiles::slug(workspace));
    let (exe, profile_dir) = match source {
        ProfileSource::Clone { browser, profile_dir, refresh } => {
            // a marker file is left behind if we exit mid-clone (runtime shutdown_timeout on quit, etc.);
            // that clone gets rebuilt
            let cloning_mark = dir.join(".ghostdesk-cloning");
            let clone_source = BrowserCloneSource {
                data_dir: crate::platform::paths::canonicalize(&browser.data_dir).unwrap_or_else(|_| browser.data_dir.clone()),
                profile_dir: profile_dir.clone(),
            };
            if !browser_clone_reusable(&dir, &clone_source, *refresh)? {
                if dir.exists() {
                    crate::platform::fsx::remove_file_retry(&dir.join(BROWSER_SOURCE_MARKER))
                        .context("기존 복제본의 완료 표시를 지우지 못했습니다")?;
                    // Windows file locks are mandatory, so deletion can briefly fail on files held by a browser or
                    // antivirus that just exited. Retry a few times, and if it still fails, report it rather than
                    // cloning over a half-deleted folder.
                    let r = crate::platform::fsx::remove_dir_all_retry(&dir);
                    if let Err(e) = r {
                        return Err(anyhow!(
                                "작업공간 폴더를 비우지 못했습니다 (다른 프로그램이 사용 중일 수 있습니다 — 그 작업공간을 쓰던 브라우저가 \
                                 아직 닫히는 중이면 잠시 후 다시 시도하세요): {} — {e}",
                                dir.display()
                        ));
                    }
                }
                std::fs::create_dir_all(&dir).with_context(|| format!("작업공간 생성 실패: {}", dir.display()))?;
                std::fs::write(&cloning_mark, b"").context("프로필 복제 진행 표시를 기록하지 못했습니다")?;
                shared.set_status(AgentStatus::Preparing("프로필 복제 중".into()));
                shared.log(
                    LogKind::Info,
                    format!("{} / {} 프로필을 작업공간 '{}' 으로 복제합니다", browser.name, profile_dir, workspace),
                );
                let (src, pd, dst, sh) = (browser.data_dir.clone(), profile_dir.clone(), dir.clone(), shared.clone());
                let st = tokio::task::spawn_blocking(move || {
                    profiles::clone_profile(&src, &pd, &dst, &|files, bytes| {
                        sh.set_status(AgentStatus::Preparing(format!(
                            "프로필 복제 중 ({files}개, {:.0} MB)",
                            bytes as f64 / 1e6
                        )));
                    })
                })
                .await??;
                std::fs::write(dir.join(BROWSER_SOURCE_MARKER), serde_json::to_vec(&clone_source)?)
                    .context("프로필 복제 원본을 기록하지 못했습니다")?;
                crate::platform::fsx::remove_file_retry(&cloning_mark).context("프로필 복제 완료 표시를 갱신하지 못했습니다")?;
                shared.log(
                    LogKind::Info,
                    format!(
                        "복제 완료: 파일 {}개, {:.1} MB (잠김 등으로 건너뜀 {}개)",
                        st.files,
                        st.bytes as f64 / 1e6,
                        st.skipped
                    ),
                );
            } else {
                shared.log(LogKind::Info, format!("기존 작업공간 '{}' 재사용", workspace));
            }
            (browser.exe.clone(), Some(profile_dir.clone()))
        }
        ProfileSource::Empty { browser } => (browser.exe.clone(), None),
    };

    shared.set_status(AgentStatus::Preparing("브라우저 실행 중".into()));
    let sh = shared.clone();
    let ws = BrowserWorkspace::launch(
        LaunchOptions {
            exe,
            user_data_dir: dir,
            profile_dir,
            width,
            height,
            headless,
            start_url: start_url.to_string(),
        },
        Arc::new(move |b: &[u8]| sh.push_frame(b)),
    )
    .await?;
    shared.log(
        LogKind::Info,
        crate::app::i18n::trf(
            "격리 브라우저 실행됨 ({}x{}, {})",
            &[&width, &height, &crate::app::i18n::tr(if headless { "headless — 화면에 표시되지 않음" } else { "창 표시 모드" })],
        ),
    );
    Ok(Desk::Browser(ws))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui;

    fn browser_clone_fixture(tag: &str) -> (PathBuf, PathBuf, BrowserCloneSource) {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("ghostdesk-browser-affinity-{tag}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let clone = root.join("clone");
        std::fs::create_dir_all(clone.join("Default")).unwrap();
        std::fs::write(clone.join("Default/keep.txt"), "existing login workspace").unwrap();
        let source = BrowserCloneSource { data_dir: root.join("Chrome"), profile_dir: "Default".into() };
        (root, clone, source)
    }

    fn remove_clone_fixture(root: &Path) {
        let root = std::fs::canonicalize(root).unwrap();
        assert!(root.starts_with(std::fs::canonicalize(std::env::temp_dir()).unwrap()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn browser_clone_source_mismatch_preserves_existing_workspace() {
        let (root, clone, source) = browser_clone_fixture("mismatch");
        let other = BrowserCloneSource { data_dir: root.join("Edge"), profile_dir: "Default".into() };
        std::fs::write(clone.join(BROWSER_SOURCE_MARKER), serde_json::to_vec(&other).unwrap()).unwrap();
        assert!(browser_clone_reusable(&clone, &source, false).is_err());
        assert_eq!(std::fs::read_to_string(clone.join("Default/keep.txt")).unwrap(), "existing login workspace");
        remove_clone_fixture(&root);
    }

    #[test]
    fn browser_clone_legacy_and_missing_profile_require_explicit_refresh() {
        let (root, clone, source) = browser_clone_fixture("legacy");
        let error = browser_clone_reusable(&clone, &source, false).unwrap_err().to_string();
        assert!(error.contains("refresh_login") && error.contains("다른 작업공간"), "{error}");
        std::fs::write(clone.join(BROWSER_SOURCE_MARKER), serde_json::to_vec(&source).unwrap()).unwrap();
        let other_profile = BrowserCloneSource { data_dir: source.data_dir, profile_dir: "Profile 1".into() };
        assert!(browser_clone_reusable(&clone, &other_profile, false).is_err(), "a missing requested profile must not delete the previous profile");
        assert!(clone.join("Default/keep.txt").is_file());
        assert!(!browser_clone_reusable(&clone, &other_profile, true).unwrap());
        remove_clone_fixture(&root);
    }

    #[test]
    fn browser_clone_matching_source_reuses_and_interrupted_clone_rebuilds() {
        let (root, clone, source) = browser_clone_fixture("matching");
        std::fs::write(clone.join(BROWSER_SOURCE_MARKER), serde_json::to_vec(&source).unwrap()).unwrap();
        assert!(browser_clone_reusable(&clone, &source, false).unwrap());
        assert!(!browser_clone_reusable(&clone, &source, true).unwrap());
        std::fs::write(clone.join(".ghostdesk-cloning"), "").unwrap();
        assert!(!browser_clone_reusable(&clone, &source, false).unwrap());
        remove_clone_fixture(&root);
    }

    /// This machine's VNC server needs the dedicated-machine confirmation outside macOS; the Apple
    /// server itself is always refused, but only on macOS.
    #[tokio::test]
    async fn local_vnc_server_needs_ack() {
        // fake RFB server that sends a version and security type (None), then hangs up (connecting
        // always fails)
        async fn fake(version: &'static [u8]) -> u16 {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                while let Ok((mut s, _)) = l.accept().await {
                    let _ = s.write_all(version).await;
                    let mut v = [0u8; 12];
                    if s.read_exact(&mut v).await.is_ok() {
                        let _ = s.write_all(&[1, 1]).await;
                    }
                }
            });
            port
        }
        let shared = AgentShared::new(1, "t".into(), "t".into(), egui::Context::default());
        let target = |port, ack| RemoteTarget { host: "127.0.0.1".into(), port, username: None, password: None, dedicated_ack: ack, ssh: None };
        let err = |r: Result<Desk>| r.err().map(|e| format!("{e:#}")).unwrap_or_default();

        let port = fake(b"RFB 003.008\n").await;
        let e = err(prepare_remote(&shared, &target(port, false), false).await);
        if cfg!(target_os = "macos") {
            assert!(!e.contains("VNC 서버입니다"), "macOS 는 기존대로 Apple 서버만 확인: {e}");
        } else {
            assert!(e.contains("자신에서 실행 중인 VNC 서버"), "{e}");
        }
        let e = err(prepare_remote(&shared, &target(port, true), true).await);
        assert!(!e.contains("자신에서 실행 중인 VNC 서버"), "확인하면 거부하지 않음: {e}");

        let port = fake(b"RFB 003.889\n").await;
        let e = err(prepare_remote(&shared, &target(port, true), true).await);
        if cfg!(target_os = "macos") {
            assert!(e.contains("이 Mac 자신의 화면 공유 서버"), "{e}");
        } else {
            // on other OSes an Apple server on loopback is a tunnel to another Mac: not refused once
            // confirmed
            assert!(!e.contains("이 Mac 자신의") && !e.contains("자신에서 실행 중인 VNC 서버"), "{e}");
        }
        let e = err(prepare_remote(&shared, &target(port, false), false).await);
        if cfg!(target_os = "macos") {
            assert!(e.contains("이 Mac 자신의 화면 공유 서버"), "{e}");
        } else {
            assert!(e.contains("대상이 macOS 화면 공유 서버입니다"), "{e}");
        }

        // outside macOS: an MCP request that was confirmed but whose approver never saw the "this
        // computer itself" warning (local_ok=false) is refused if it actually reached this machine,
        // Apple server or not
        #[cfg(not(target_os = "macos"))]
        for v in [&b"RFB 003.008\n"[..], &b"RFB 003.889\n"[..]] {
            let port = fake(v).await;
            let e = err(prepare_remote(&shared, &target(port, true), false).await);
            assert_eq!(e, local_vnc_mismatch("127.0.0.1", port), "{}", String::from_utf8_lossy(v));
            let e = err(prepare_workspace_with(&shared, "t", &DeskTarget::Remote(target(port, true)), Some(false)).await);
            assert_eq!(e, local_vnc_mismatch("127.0.0.1", port));
            // when not passed, the confirmation (dedicated_ack) decides
            let e = err(prepare_workspace(&shared, "t", &DeskTarget::Remote(target(port, true))).await);
            assert!(!e.contains("자신에서 실행 중인") && !e.contains("실제로 접속한 주소는"), "{e}");
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn local_vnc_refusal_mentions_this_pc() {
        let m = local_vnc_refusal("localhost", 5900);
        assert!(m.contains("localhost:5900"));
        assert!(m.contains(crate::platform::text::THIS_COMPUTER));
        assert!(m.contains("에이전트 전용 머신"), "앱의 확인란 이름과 같아야 함: {m}");
    }
}
