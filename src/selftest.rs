//! `ghostdesk --selftest [출력.png | --browser=<이름|실행 파일>]`
//!
//! 빈 임시 프로필로 headless 브라우저를 띄워 클릭/타이핑(영문+한글)/단축키/스크롤/
//! 탭 전환을 CDP 로 수행하고 결과를 검증한다. 사용자 프로필은 건드리지 않는다.
//!
//! 브라우저 고르기: `--browser=edge` 또는 환경변수 `GHOSTDESK_SELFTEST_BROWSER` (이름 일부, 또는 실행 파일 경로).
//! 실행 파일 경로를 주면 설치된 브라우저 목록(프로필 정보)도 읽지 않는다. 없으면 감지된 첫 브라우저.

use std::{
    path::PathBuf,
    sync::{atomic::{AtomicUsize, Ordering}, Arc},
    time::Duration,
};

use anyhow::{ensure, Result};

use crate::{browser::{BrowserWorkspace, LaunchOptions, MouseButton}, profiles};

/// 셀프 테스트에 쓸 브라우저 (표시 이름, 실행 파일)
fn pick_browser(choice: Option<String>) -> Result<(String, PathBuf)> {
    if let Some(c) = &choice {
        let p = PathBuf::from(c.trim().trim_matches('"'));
        if p.is_file() {
            let name = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            println!("브라우저: {name} ({})", p.display());
            return Ok((name, p));
        }
    }
    let browsers = profiles::detect();
    ensure!(!browsers.is_empty(), "Chromium 계열 브라우저를 찾지 못했습니다");
    let b = match &choice {
        Some(c) => {
            let want = c.trim().to_lowercase();
            browsers.iter().find(|b| b.name.to_lowercase().contains(&want)).ok_or_else(|| {
                anyhow::anyhow!(
                    "'{c}' 브라우저를 찾지 못했습니다 (감지됨: {})",
                    browsers.iter().map(|b| b.name.as_str()).collect::<Vec<_>>().join(", ")
                )
            })?
        }
        None => &browsers[0],
    };
    println!("브라우저: {} ({})", b.name, b.exe.display());
    println!("감지된 프로필: {:?}", b.profiles.iter().map(|p| &p.name).collect::<Vec<_>>());
    Ok((b.name.clone(), b.exe.clone()))
}

const PAGE: &str = r#"data:text/html;charset=utf-8,<html><body style='font:20px sans-serif;height:3000px'>
<input id=a style='position:absolute;left:100px;top:100px;width:400px;height:40px'>
<button id=b style='position:absolute;left:100px;top:200px;width:200px;height:50px' onclick="this.textContent='clicked'">click me</button>
<a id=l href='about:blank' target=_blank style='position:absolute;left:100px;top:300px'>new tab</a>
</body></html>"#;

pub async fn run(out: Option<String>) -> Result<()> {
    // 두 번째 인자로 `--browser=…` 를 줄 수도 있다 (그때는 스크린샷을 저장하지 않는다)
    let (out, choice) = match out {
        Some(o) if o.starts_with("--browser=") => (None, Some(o["--browser=".len()..].to_string())),
        o => (o, std::env::var("GHOSTDESK_SELFTEST_BROWSER").ok().filter(|s| !s.trim().is_empty())),
    };
    let (_name, exe) = pick_browser(choice)?;

    let dir = std::env::temp_dir().join(format!("ghostdesk-selftest-{}", std::process::id()));
    let frames = Arc::new(AtomicUsize::new(0));
    let f2 = frames.clone();
    let ws = BrowserWorkspace::launch(
        LaunchOptions {
            exe,
            user_data_dir: dir.clone(),
            profile_dir: None,
            width: 1280,
            height: 800,
            headless: true,
            start_url: PAGE.into(),
        },
        Arc::new(move |_b: &[u8]| {
            f2.fetch_add(1, Ordering::Relaxed);
        }),
    )
    .await?;
    println!("✔ 실행 및 CDP 연결");

    let ua = ws.eval("navigator.userAgent").await?;
    println!("  UA: {ua}");
    ensure!(!ua.as_str().unwrap_or("").contains("Headless"), "UA 에 Headless 가 남아 있음");
    let size = ws.eval("[innerWidth, innerHeight, devicePixelRatio]").await?;
    println!("  viewport: {size}");

    ws.click(Some((300.0, 120.0)), MouseButton::Left, 1, 0).await?;
    ws.type_text("Hello GhostDesk! 안녕하세요").await?;
    let v = ws.eval("document.getElementById('a').value").await?;
    println!("  입력값: {v}");
    ensure!(v == "Hello GhostDesk! 안녕하세요", "타이핑 결과 불일치");
    println!("✔ 클릭 + 영문/한글 타이핑");

    ws.key("ctrl+a", 1).await?;
    ws.key("BackSpace", 1).await?;
    let v = ws.eval("document.getElementById('a').value").await?;
    ensure!(v == "", "전체선택+삭제 실패: {v}");
    println!("✔ ctrl+a / BackSpace");

    ws.click(Some((200.0, 225.0)), MouseButton::Left, 1, 0).await?;
    let v = ws.eval("document.getElementById('b').textContent").await?;
    ensure!(v == "clicked", "버튼 클릭 실패");
    println!("✔ 버튼 클릭");

    ws.scroll(Some((640.0, 400.0)), 0.0, 500.0, 0).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let y = ws.eval("scrollY").await?;
    ensure!(y.as_f64().unwrap_or(0.0) > 100.0, "스크롤 실패: {y}");
    println!("✔ 스크롤 (scrollY={y})");
    ws.scroll(None, 0.0, -2000.0, 0).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;

    let png = ws.screenshot().await?;
    let img = image::load_from_memory(&png)?;
    ensure!(img.width() == 1280 && img.height() == 800, "스크린샷 크기 {}x{}", img.width(), img.height());
    println!("✔ 스크린샷 {}x{} ({} KB)", img.width(), img.height(), png.len() / 1024);
    if let Some(p) = &out {
        std::fs::write(p, &png)?;
        println!("  저장: {p}");
    }

    ws.click(Some((130.0, 310.0)), MouseButton::Left, 1, 0).await?;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let (tabs, _) = ws.tabs();
    println!("  탭 목록:\n{}", ws.tabs_text());
    ensure!(tabs.len() >= 2, "target=_blank 새 탭 감지 실패");
    println!("✔ 새 탭 자동 전환 / 메모: {:?}", ws.take_notes());
    ws.switch_tab(0).await?;
    ws.new_tab("example.com").await?;
    println!("✔ 새 탭에서 이동: {:?}", ws.active_tab().map(|t| t.url));

    let z = ws.zoom([100.0, 100.0, 500.0, 300.0]).await?;
    println!("✔ zoom ({} KB)", z.len() / 1024);
    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("✔ 라이브 프레임 수신: {}개", frames.load(Ordering::Relaxed));
    ensure!(frames.load(Ordering::Relaxed) > 0, "screencast 프레임 없음");

    // Windows: 브라우저와 헬퍼 프로세스가 모두 Job 객체 안에 있고, 종료 뒤 하나도 남지 않아야 한다
    #[cfg(windows)]
    let job_pids = {
        let pid = ws.browser_pid().ok_or_else(|| anyhow::anyhow!("브라우저 pid 없음"))?;
        // (런처형 브라우저면 본 pid 는 이미 끝났고 실제 브라우저만 Job 에 있다)
        let pids = crate::platform::proc::group_pids(pid).unwrap_or_default();
        ensure!(!pids.is_empty(), "브라우저가 Job 객체에 들어 있지 않음 (pid {pid})");
        println!("✔ Job 객체: 프로세스 {}개", pids.len());
        pids
    };

    ws.shutdown().await;
    #[cfg(windows)]
    {
        let mut left = job_pids.clone();
        for _ in 0..60 {
            left.retain(|&p| crate::platform::proc::pid_alive(p));
            if left.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        ensure!(left.is_empty(), "종료 뒤에도 남은 브라우저 프로세스: {left:?}");
        println!("✔ 종료 후 남은 프로세스 없음");
    }
    match crate::platform::fsx::remove_dir_all_retry(&dir) {
        Ok(()) => {}
        Err(e) => println!("  (임시 폴더를 지우지 못했습니다: {} — {e})", dir.display()),
    }
    println!("\n모든 셀프 테스트 통과");
    Ok(())
}

/// `ghostdesk --agent-test "<작업>"` : GUI 없이 에이전트 루프 하나를 빈 프로필로 실행한다.
/// (ANTHROPIC_BASE_URL 로 가짜 서버를 지정해 루프를 검증할 때 사용)
pub async fn run_agent_cli(task: String) -> Result<()> {
    use crate::{agent::{self, AgentConfig, AgentShared, DeskTarget, ProfileSource}, llm::Llm};
    let browser = profiles::detect().into_iter().next().ok_or_else(|| anyhow::anyhow!("브라우저 없음"))?;
    let ctx = eframe::egui::Context::default();
    let ws_name = format!("agent-test-{}", std::process::id());
    let shared = AgentShared::new(1, "cli".into(), ws_name.clone(), ctx);
    let cfg = AgentConfig {
        name: "cli".into(),
        workspace: ws_name.clone(),
        task,
        // GHOSTDESK_TEST_REMOTE=host:port[:dedicated] 이면 원격 데스크톱(VNC) 백엔드로 실행
        target: if let Ok(name) = std::env::var("GHOSTDESK_TEST_ELECTRON") {
            let app = crate::electron::detect_apps()
                .into_iter()
                .find(|a| a.name.contains(&name) || a.bundle_id == name)
                .ok_or_else(|| anyhow::anyhow!("Electron 앱 {name} 없음"))?;
            DeskTarget::Electron { app, profile: crate::electron::ElectronProfile::Empty, width: 1280, height: 800 }
        } else if let Ok(app) = std::env::var("GHOSTDESK_TEST_NATIVE") {
            DeskTarget::Native(crate::native::NativeOptions { app, new_instance: true, ..Default::default() })
        } else { match std::env::var("GHOSTDESK_TEST_REMOTE").ok() {
            Some(spec) => {
                let parts: Vec<&str> = spec.split(':').collect();
                DeskTarget::Remote(crate::agent::RemoteTarget {
                    host: parts.first().copied().unwrap_or("127.0.0.1").to_string(),
                    port: parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(5900),
                    username: None,
                    password: None,
                    dedicated_ack: parts.get(2) == Some(&"dedicated"),
                })
            }
            None => DeskTarget::Browser {
                source: ProfileSource::Empty { browser },
                headless: true,
                width: 1280,
                height: 800,
                start_url: "about:blank".into(),
            },
        } },
        max_steps: 10,
        llm: Llm::new(
            std::env::var("ANTHROPIC_API_KEY").unwrap_or_else(|_| "test".into()),
            "claude-opus-5-5".into(),
            "medium".into(),
        ),
    };
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let h = tokio::spawn(agent::run_agent(shared.clone(), cfg, rx));
    let mut printed = 0;
    loop {
        tokio::time::sleep(Duration::from_millis(300)).await;
        {
            let log = shared.log.lock();
            for l in &log[printed..] {
                println!("[{:>5.1}s] {:?}: {}", l.secs, l.kind, l.text);
            }
            printed = log.len();
        }
        match shared.status() {
            crate::agent::AgentStatus::Idle(r) => { println!("상태: 대기 — {r}"); break; }
            crate::agent::AgentStatus::Error(e) => { println!("상태: 오류 — {e}"); break; }
            _ => {}
        }
    }
    if let Some(ws) = shared.workspace() {
        if let Some(Ok(v)) = match ws.as_browser() { Some(b) => Some(b.eval("document.getElementById('q') && document.getElementById('q').value").await), None => None } {
            println!("페이지 입력값 확인: {v}");
        }
        ws.shutdown().await;
    }
    h.abort();
    let _ = crate::platform::fsx::remove_dir_all_retry(&profiles::workspaces_root().join(profiles::slug(&ws_name)));
    Ok(())
}
