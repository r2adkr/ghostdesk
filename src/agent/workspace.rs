//! 격리 화면 준비: 화면 종류(DeskTarget)별로 띄우거나 접속해 `Desk` 를 만든다.

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{anyhow, Result};

use super::{AgentShared, AgentStatus, DeskTarget, LogKind, ProfileSource, RemoteTarget};
use crate::{
    browser::{BrowserWorkspace, LaunchOptions},
    desk::Desk,
    profiles,
};

pub(crate) async fn prepare_workspace(shared: &Arc<AgentShared>, workspace: &str, target: &DeskTarget) -> Result<Desk> {
    prepare_workspace_with(shared, workspace, target, None).await
}

/// `prepare_workspace` 와 같되, 원격 대상(kind=remote)에 실제로 접속한 주소가 이 컴퓨터 자신일 때
/// 허용할지를 호출한 쪽이 정한다 (macOS 밖에서만 쓴다. macOS 에서는 무시).
/// `None` 이면 대상의 `dedicated_ack` 를 따른다 (GUI·자체 점검). MCP 요청(control.rs)은 사람이 승인 창에서
/// '이 컴퓨터 자신의 VNC 서버' 경고를 본 경우에만 `Some(true)` 를 넘겨, 승인한 대상과 실제로 접속한 주소를 묶는다.
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
        DeskTarget::Remote(t) => prepare_remote(shared, t, remote_local_ok.unwrap_or(t.dedicated_ack)).await,
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
        DeskTarget::Session { account } => {
            shared.set_status(AgentStatus::Preparing(format!("'{account}' 세션에 연결 중")));
            let sh = shared.clone();
            let d = crate::session::SessionDesk::connect(account, Arc::new(move |b: &[u8]| sh.push_frame(b))).await?;
            let (w, h) = d.size();
            shared.log(LogKind::Info, format!("전용 계정 '{account}' 의 별도 세션에 연결했습니다 ({w}x{h})"));
            Ok(Desk::Session(d))
        }
    }
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

/// 이 컴퓨터 자신의 VNC 서버에 확인 없이 접속하려 할 때의 오류 (macOS 밖).
/// 그런 서버는 대개 지금 사용자가 보고 있는 실제 화면을 내보내므로, 에이전트가 사용자의 화면·마우스·키보드를
/// 직접 조작하게 된다. WSL·Hyper-V 처럼 이 컴퓨터 주소로 전달되는 별도 데스크톱은 확인 후 허용한다.
/// (MCP 요청은 control.rs 가 사람에게 묻기 전에 같은 문구로 거절한다)
#[cfg(not(target_os = "macos"))]
pub(crate) fn local_vnc_refusal(host: &str, port: u16) -> String {
    use crate::platform::text::THIS_COMPUTER;
    format!(
        "{host}:{port} 은(는) {THIS_COMPUTER} 자신에서 실행 중인 VNC 서버입니다. 여기에 접속하면 에이전트가 지금 보고 계신 \
         실제 화면·마우스·키보드를 조작하게 될 수 있습니다. WSL·Hyper-V 가상 머신처럼 {THIS_COMPUTER} 주소로 전달되는 \
         별도 데스크톱이 확실하다면 '에이전트 전용 머신' 확인란을 켜고 다시 시작하세요."
    )
}

/// 원격 대상이 승인할 때는 이 컴퓨터가 아닌 것으로 보였는데 실제로 접속한 주소는 이 컴퓨터 자신일 때의 오류 (macOS 밖).
/// (에이전트 전용 머신 확인은 있었지만 사람이 '이 컴퓨터 자신' 경고를 보지 못했다: DNS 응답이 바뀌었거나
/// 이 컴퓨터를 가리키는 다른 이름을 썼다)
#[cfg(not(target_os = "macos"))]
fn local_vnc_mismatch(host: &str, port: u16) -> String {
    use crate::platform::text::THIS_COMPUTER;
    format!(
        "{host}:{port} 은(는) 승인할 때 {THIS_COMPUTER} 가 아닌 대상으로 보였지만, 실제로 접속한 주소는 {THIS_COMPUTER} 자신입니다. \
         지금 보고 계신 실제 화면일 수 있어 접속하지 않습니다. {THIS_COMPUTER} 의 VNC 서버가 맞다면 host 를 127.0.0.1 처럼 \
         주소로 직접 적어 다시 요청하세요."
    )
}

/// `local_ok`: 실제로 접속한 주소가 이 컴퓨터 자신이어도 접속할지 (macOS 밖에서만 쓴다).
/// 보통은 `t.dedicated_ack` 와 같고, MCP 요청은 사람이 '이 컴퓨터 자신' 경고를 본 경우에만 true.
async fn prepare_remote(shared: &Arc<AgentShared>, t: &RemoteTarget, local_ok: bool) -> Result<Desk> {
    #[cfg(target_os = "macos")]
    let _ = local_ok;
    use crate::rfb::{self, RfbDesk, RfbOptions};
    shared.set_status(AgentStatus::Preparing("원격 데스크톱 확인 중".into()));
    let g = rfb::probe(&t.host, t.port, rfb::ReplyVersion::Auto, Duration::from_secs(6)).await?;
    let types: Vec<&str> = g.security_types.iter().map(|x| rfb::security_type_name(*x)).collect();
    shared.log(LogKind::Info, format!("{}:{} — {} / 보안 방식: {}", t.host, t.port, g.server_version, types.join(", ")));
    let apple = g.server_version.contains("003.889");
    // 이 Mac 자신의 화면 공유(Apple 서버)는 확인과 상관없이 항상 거부한다 (macOS 전용 규칙:
    // 다른 OS 에서 루프백의 Apple 서버는 다른 Mac 으로 가는 터널일 뿐이다).
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
    // macOS 밖: 이 컴퓨터 자신의 VNC 서버(TightVNC·UltraVNC·RealVNC 등)는 사용자의 실제 화면일 수 있으므로
    // '에이전트 전용 머신' 확인이 있어야만 접속한다.
    // 판단은 이름을 다시 푼 결과뿐 아니라 실제로 접속한 주소(탐지 연결의 상대 주소)로도 하고,
    // 본 접속에서도 접속한 소켓으로 다시 확인한다 (DNS 응답이 바뀌는 사이에 빠져나가지 못하게).
    // (확인이 있었는데도 허용하지 않는 경우는 승인 창에서 '이 컴퓨터 자신' 경고를 보지 못한 MCP 요청이다)
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
    // 라이브 화면: 프레임버퍼가 바뀔 때마다(최대 10fps) UI 로 올린다.
    let (sh, weak) = (shared.clone(), Arc::downgrade(&desk));
    tokio::spawn(async move {
        let mut last = u64::MAX;
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let Some(d) = weak.upgrade() else { break };
            if d.is_closed() {
                break;
            }
            let s = d.frame_seq();
            if s != last {
                last = s;
                d.with_frame(|w, h, rgba| sh.push_rgba(w, h, rgba));
            }
        }
    });
    Ok(Desk::Remote(desk))
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
            // 복제 도중 종료(앱 종료 시 런타임 shutdown_timeout 등)되면 표시 파일이 남는다: 그 복제본은 다시 만든다
            let cloning_mark = dir.join(".ghostdesk-cloning");
            let has = dir.join(profile_dir).is_dir() && !cloning_mark.exists();
            if *refresh || !has {
                if dir.exists() {
                    // Windows 는 파일 잠금이 강제라, 방금 끝난 브라우저·백신이 쥔 파일 때문에 지우기가 잠깐 실패할 수 있다:
                    // 몇 번 다시 시도하고, 그래도 안 되면 반쯤 남은 폴더 위에 복제하지 않고 알린다.
                    let r = crate::platform::fsx::remove_dir_all_retry(&dir);
                    if cfg!(windows) {
                        if let Err(e) = r {
                            return Err(anyhow!(
                                "작업공간 폴더를 비우지 못했습니다 (다른 프로그램이 사용 중일 수 있습니다 — 그 작업공간을 쓰던 브라우저가 \
                                 아직 닫히는 중이면 잠시 후 다시 시도하세요): {} — {e}",
                                dir.display()
                            ));
                        }
                    }
                }
                std::fs::create_dir_all(&dir).ok();
                std::fs::write(&cloning_mark, b"").ok();
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
                std::fs::remove_file(&cloning_mark).ok();
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
        format!(
            "격리 브라우저 실행됨 ({}x{}, {})",
            width,
            height,
            if headless { "headless — 화면에 표시되지 않음" } else { "창 표시 모드" }
        ),
    );
    Ok(Desk::Browser(ws))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui;

    /// 이 컴퓨터의 VNC 서버: macOS 밖에서는 '에이전트 전용 머신' 확인이 있어야 하고, Apple 서버 자신은 macOS 에서만 항상 거부
    #[tokio::test]
    async fn local_vnc_server_needs_ack() {
        // 버전과 보안 방식(None)만 알려 주고 끊는 가짜 RFB 서버 (접속 단계는 항상 실패)
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
        let target = |port, ack| RemoteTarget { host: "127.0.0.1".into(), port, username: None, password: None, dedicated_ack: ack };
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
            // 다른 OS 에서 루프백의 Apple 서버는 다른 Mac 으로 가는 터널이다: 확인했으면 거부하지 않음
            assert!(!e.contains("이 Mac 자신의") && !e.contains("자신에서 실행 중인 VNC 서버"), "{e}");
        }
        let e = err(prepare_remote(&shared, &target(port, false), false).await);
        if cfg!(target_os = "macos") {
            assert!(e.contains("이 Mac 자신의 화면 공유 서버"), "{e}");
        } else {
            assert!(e.contains("대상이 macOS 화면 공유 서버입니다"), "{e}");
        }

        // macOS 밖: 확인은 있었지만 사람이 승인할 때 '이 컴퓨터 자신' 경고를 보지 못한 MCP 요청(local_ok=false)은
        // 실제로 접속한 주소가 이 컴퓨터면 거절한다 (Apple 서버든 아니든)
        #[cfg(not(target_os = "macos"))]
        for v in [&b"RFB 003.008\n"[..], &b"RFB 003.889\n"[..]] {
            let port = fake(v).await;
            let e = err(prepare_remote(&shared, &target(port, true), false).await);
            assert_eq!(e, local_vnc_mismatch("127.0.0.1", port), "{}", String::from_utf8_lossy(v));
            let e = err(prepare_workspace_with(&shared, "t", &DeskTarget::Remote(target(port, true)), Some(false)).await);
            assert_eq!(e, local_vnc_mismatch("127.0.0.1", port));
            // 넘겨주지 않으면 확인(dedicated_ack)을 따른다
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
