//! Host mode: this computer lends its own screen to another GhostDesk over the internet, without VPN,
//! port forwarding or a VNC server.
//!
//! - The host runs `ghostdesk --host` in the background (started at login while the setting is on, see
//!   [`autostart`]). It listens on an iroh endpoint: connections are found by public key, go direct when NAT
//!   traversal works and over a relay otherwise, and are end-to-end encrypted either way.
//! - The pairing code (`ghostdesk:host:v1:…`) holds the endpoint id and a secret token. Whoever has the code can
//!   use the screen, so it is handled like a password and can be replaced at any time ([`regenerate_token`]).
//! - After the token check the stream speaks the same line protocol as the session helper (`session.rs`):
//!   screenshots, pointer, keys and text.
//! - The client side opens a local socket ([`Bridge`]) and connects the normal `SessionDesk` to it, so everything
//!   an agent can do in a session works on the host too.
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use iroh::{endpoint::presets, Endpoint, EndpointId, SecretKey};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;

const ALPN: &[u8] = b"ghostdesk/host/1";
const PREFIX: &str = "ghostdesk:host:v1:";
/// The token line is short; anything longer is not ours.
const MAX_TOKEN_LINE: usize = 128;

#[derive(Serialize, Deserialize)]
struct Stored {
    /// Hex of the 32 secret key bytes (the endpoint id stays the same across restarts).
    secret: String,
    token: String,
}

fn config_path() -> PathBuf {
    crate::platform::paths::data_root().join("host.json")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (s.len() % 2 == 0).then(|| ()).and_then(|_| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect())
}

/// A fresh random token: 32 bytes from the same OS-backed generator iroh uses for keys.
fn new_token() -> String {
    hex(&SecretKey::generate().to_bytes())
}

fn save(st: &Stored) -> Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(&serde_json::json!({"secret": st.secret, "token": st.token}))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
        f.write_all(text.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, text)?;
    Ok(())
}

fn load_or_create() -> Result<(SecretKey, String)> {
    let stored: Option<Stored> = std::fs::read_to_string(config_path()).ok().and_then(|t| serde_json::from_str(&t).ok());
    if let Some(st) = stored {
        if let Some(bytes) = unhex(&st.secret).and_then(|b| <[u8; 32]>::try_from(b).ok()) {
            if st.token.len() >= 32 {
                return Ok((SecretKey::from_bytes(&bytes), st.token));
            }
        }
    }
    let key = SecretKey::generate();
    let st = Stored { secret: hex(&key.to_bytes()), token: new_token() };
    save(&st)?;
    Ok((key, st.token))
}

/// Replaces the token: every code handed out before stops working (the running host picks it up on its next start).
pub fn regenerate_token() -> Result<()> {
    let (key, _) = load_or_create()?;
    save(&Stored { secret: hex(&key.to_bytes()), token: new_token() })
}

/// The code to give to the person who connects. Creates the host identity the first time.
pub fn pairing_code() -> Result<String> {
    let (key, token) = load_or_create()?;
    Ok(encode_code(&key.public().to_string(), &token))
}

fn encode_code(id: &str, token: &str) -> String {
    format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(serde_json::json!({"id": id, "token": token}).to_string()))
}

/// `(endpoint id, token)` of a pasted code.
pub fn parse_code(text: &str) -> Result<(EndpointId, String)> {
    let body = text.trim().strip_prefix(PREFIX).ok_or_else(|| anyhow!("호스트 접속 코드가 아닙니다 (ghostdesk:host:v1: 로 시작해야 해요)"))?;
    let bytes = URL_SAFE_NO_PAD.decode(body).map_err(|_| anyhow!("접속 코드를 읽을 수 없어요"))?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| anyhow!("접속 코드를 읽을 수 없어요"))?;
    let id: EndpointId = v["id"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow!("접속 코드의 주소가 올바르지 않아요"))?;
    let token = v["token"].as_str().filter(|t| (32..=MAX_TOKEN_LINE).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_hexdigit())).ok_or_else(|| anyhow!("접속 코드의 암호가 올바르지 않아요"))?;
    Ok((id, token.to_string()))
}

fn same_token(a: &[u8], b: &[u8]) -> bool {
    // compare everything, never stopping at the first difference
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Reads a short line (the token) byte by byte, so nothing past it is consumed.
async fn read_token_line(rd: &mut (impl tokio::io::AsyncRead + Unpin)) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut b = [0u8; 1];
        rd.read_exact(&mut b).await?;
        if b[0] == b'\n' {
            return Ok(out);
        }
        if out.len() >= MAX_TOKEN_LINE {
            bail!("too long");
        }
        out.push(b[0]);
    }
}

// ---- host process ----

/// `ghostdesk --host`: serves this computer's screen until stopped. Used by the login item.
pub async fn run() -> Result<()> {
    let log = crate::session::helper_logger();
    let platform = crate::session::host_platform().map_err(|e| {
        log(&format!("호스트 시작 실패: {e}"));
        e
    })?;
    let (key, token) = load_or_create()?;
    let endpoint = Endpoint::builder(presets::N0).secret_key(key).alpns(vec![ALPN.to_vec()]).bind().await.context("연결 준비 실패")?;
    log(&format!("호스트 시작: {}", endpoint.id()));
    let server = Arc::new(crate::session::HostServer::new(platform, tr_label(), log.clone()));
    let stop = async {
        #[cfg(unix)]
        {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending().await } } => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    };
    serve_loop(&endpoint, &token, &server, &log, stop).await;
    server.stop_all();
    endpoint.close().await;
    log("호스트 종료");
    Ok(())
}

/// Accepts connections until `stop` finishes.
async fn serve_loop(endpoint: &Endpoint, token: &str, server: &Arc<crate::session::HostServer>, log: &crate::session::LogFn, stop: impl std::future::Future<Output = ()>) {
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let (server, token, log) = (server.clone(), token.to_string(), log.clone());
                tokio::spawn(async move {
                    if let Err(e) = handle(incoming, &token, &server, &log).await {
                        log(&format!("연결 종료: {e:#}"));
                    }
                });
            }
        }
    }
}

fn tr_label() -> String {
    "호스트".to_string()
}

async fn handle(incoming: iroh::endpoint::Incoming, token: &str, server: &Arc<crate::session::HostServer>, log: &crate::session::LogFn) -> Result<()> {
    let conn = incoming.await.context("연결 수락 실패")?;
    // one connection carries a stream per `SessionDesk` connection (it reconnects on its own)
    loop {
        let Ok((send, recv)) = conn.accept_bi().await else { return Ok(()) };
        let (server, token, log) = (server.clone(), token.to_string(), log.clone());
        tokio::spawn(async move {
            if let Err(e) = handle_stream(send, recv, &token, &server, &log).await {
                log(&format!("연결 종료: {e:#}"));
            }
        });
    }
}

async fn handle_stream(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    token: &str,
    server: &crate::session::HostServer,
    log: &crate::session::LogFn,
) -> Result<()> {
    let got = tokio::time::timeout(Duration::from_secs(10), read_token_line(&mut recv)).await.context("암호를 받지 못했습니다")??;
    if !same_token(&got, token.as_bytes()) {
        let _ = send.write_all(b"DENIED\n").await;
        let _ = send.finish();
        bail!("접속 코드의 암호가 맞지 않아 거절했습니다");
    }
    send.write_all(b"OK\n").await?;
    log("클라이언트 접속");
    server.serve(Box::new(recv), Box::new(send)).await;
    log("클라이언트 연결 끝");
    Ok(())
}

// ---- login item ----

/// Turning host mode on or off for real: on = start now and at every login, off = stop and forget.
pub mod autostart {
    use super::*;

    #[cfg(target_os = "macos")]
    const LABEL: &str = "com.ghostdesk.host";

    #[cfg(target_os = "macos")]
    fn plist_path() -> Result<PathBuf> {
        Ok(dirs::home_dir().ok_or_else(|| anyhow!("홈 폴더를 알 수 없어요"))?.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
    }

    /// LaunchAgent that keeps `ghostdesk --host` running in this user's session.
    #[cfg(any(target_os = "macos", test))]
    pub fn launch_agent_plist(exe: &str, log: &str) -> String {
        let esc = |t: &str| t.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\"><dict>\n\
             <key>Label</key><string>com.ghostdesk.host</string>\n\
             <key>ProgramArguments</key><array><string>{}</string><string>--host</string></array>\n\
             <key>RunAtLoad</key><true/>\n\
             <key>KeepAlive</key><true/>\n\
             <key>ThrottleInterval</key><integer>30</integer>\n\
             <key>ProcessType</key><string>Interactive</string>\n\
             <key>StandardErrorPath</key><string>{}</string>\n\
             </dict></plist>\n",
            esc(exe),
            esc(log)
        )
    }

    /// Whether the login item is installed.
    pub fn enabled() -> bool {
        #[cfg(target_os = "macos")]
        {
            plist_path().map(|p| p.is_file()).unwrap_or(false)
        }
        #[cfg(windows)]
        {
            win_run_value().is_some()
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            false
        }
    }

    #[cfg(windows)]
    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    #[cfg(windows)]
    const RUN_NAME: &str = "GhostDeskHost";

    #[cfg(windows)]
    fn win_run_value() -> Option<String> {
        let out = hidden("reg", &["query", RUN_KEY, "/v", RUN_NAME]).ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
    }

    #[cfg(windows)]
    fn hidden(program: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
        use std::os::windows::process::CommandExt;
        std::process::Command::new(program).args(args).creation_flags(0x0800_0000).output()
    }

    #[cfg(windows)]
    fn pid_file() -> PathBuf {
        crate::platform::paths::data_root().join("host.pid")
    }

    pub fn set(on: bool) -> Result<()> {
        if on {
            // make the identity and code now, so the first run does not race the screen
            let _ = pairing_code()?;
        }
        set_impl(on)
    }

    #[cfg(target_os = "macos")]
    fn set_impl(on: bool) -> Result<()> {
        let path = plist_path()?;
        let uid = unsafe { libc::getuid() };
        let domain = format!("gui/{uid}");
        let run = |args: &[&str]| std::process::Command::new("/bin/launchctl").args(args).output();
        // leave nothing half loaded whichever way we are going
        let _ = run(&["bootout", &format!("{domain}/{LABEL}")]);
        if on {
            let exe = std::env::current_exe()?;
            let log = crate::platform::paths::logs_dir().join("host.log");
            if let Some(d) = path.parent() {
                std::fs::create_dir_all(d)?;
            }
            if let Some(d) = log.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&path, launch_agent_plist(&exe.to_string_lossy(), &log.to_string_lossy()))?;
            let out = run(&["bootstrap", &domain, &path.to_string_lossy()])?;
            if !out.status.success() {
                bail!("호스트를 시작하지 못했어요: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
        } else if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    #[cfg(windows)]
    fn set_impl(on: bool) -> Result<()> {
        use std::os::windows::process::CommandExt;
        // stop a running host first, either way
        if let Some(pid) = std::fs::read_to_string(pid_file()).ok().and_then(|t| t.trim().parse::<u32>().ok()) {
            let _ = hidden("taskkill", &["/PID", &pid.to_string(), "/T", "/F"]);
            let _ = std::fs::remove_file(pid_file());
        }
        if on {
            let exe = std::env::current_exe()?;
            let cmd = format!("\"{}\" --host", exe.display());
            let out = hidden("reg", &["add", RUN_KEY, "/v", RUN_NAME, "/t", "REG_SZ", "/d", &cmd, "/f"])?;
            if !out.status.success() {
                bail!("시작 프로그램에 등록하지 못했어요");
            }
            let child = std::process::Command::new(&exe).arg("--host").creation_flags(0x0800_0000 | 0x0000_0008).spawn().context("호스트를 시작하지 못했어요")?;
            let _ = std::fs::write(pid_file(), child.id().to_string());
        } else {
            let _ = hidden("reg", &["delete", RUN_KEY, "/v", RUN_NAME, "/f"]);
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    fn set_impl(_on: bool) -> Result<()> {
        bail!("호스트 모드는 macOS 와 Windows 에서만 쓸 수 있습니다")
    }
}

// ---- client side ----

/// A local socket that carries each connection of a `SessionDesk` to the host over iroh. Dropping it closes
/// the socket and the iroh endpoint.
pub struct Bridge {
    /// What `SessionDesk::connect_target` takes (a socket path on Unix, a pipe on Windows).
    pub target: crate::session::Target,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Connects to the host in `code` and returns the local socket that reaches it. Fails now (not later) if the host
/// does not answer or the code's token is refused.
pub async fn connect(code: &str) -> Result<Bridge> {
    let (id, token) = parse_code(code)?;
    let endpoint = Endpoint::bind(presets::N0).await.context("연결 준비 실패")?;
    let conn = tokio::time::timeout(Duration::from_secs(30), endpoint.connect(id, ALPN))
        .await
        .map_err(|_| anyhow!("호스트가 응답하지 않아요. 호스트 컴퓨터가 켜져 있고 GhostDesk 호스트가 켜져 있는지 확인하세요"))?
        .map_err(|e| anyhow!("호스트에 연결하지 못했어요: {e}"))?;
    // prove the token once up front so a wrong code fails here
    open_stream(&conn, &token).await?.0;
    let local = crate::platform::ipc::Endpoint::custom(&format!("ghostdesk-host-{}", hex(&SecretKey::generate().to_bytes()[..8])));
    let mut listener = crate::platform::ipc::Listener::bind(&local).await?;
    let task = tokio::spawn(async move {
        let _keep = endpoint; // closes with the bridge
        while let Ok((mut lrd, mut lwr)) = listener.accept().await {
            let conn = conn.clone();
            let token = token.clone();
            tokio::spawn(async move {
                let Ok((mut send, mut recv)) = open_stream(&conn, &token).await else { return };
                let up = tokio::io::copy(&mut lrd, &mut send);
                let down = tokio::io::copy(&mut recv, &mut lwr);
                tokio::select! { _ = up => {}, _ = down => {} }
                let _ = send.finish();
            });
        }
    });
    #[cfg(unix)]
    let target = local.path().to_path_buf();
    #[cfg(windows)]
    let target = local;
    Ok(Bridge { target, task })
}

async fn open_stream(conn: &iroh::endpoint::Connection, token: &str) -> Result<(iroh::endpoint::SendStream, iroh::endpoint::RecvStream)> {
    let (mut send, mut recv) = conn.open_bi().await.context("스트림을 열지 못했어요")?;
    send.write_all(token.as_bytes()).await?;
    send.write_all(b"\n").await?;
    let reply = tokio::time::timeout(Duration::from_secs(10), read_token_line(&mut recv)).await.context("호스트가 응답하지 않아요")??;
    match reply.as_slice() {
        b"OK" => Ok((send, recv)),
        b"DENIED" => bail!("접속 코드가 더 이상 유효하지 않아요. 호스트에서 새 코드를 받으세요"),
        _ => bail!("호스트의 응답을 이해하지 못했어요"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs internet (iroh's public relay): host and client in one process, a mock screen, the real bridge.
    /// `cargo test live_host_round_trip -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "Explicit opt-in: uses iroh's public relay servers"]
    async fn live_host_round_trip() {
        let key = SecretKey::generate();
        let token = new_token();
        let endpoint = Endpoint::builder(presets::N0).secret_key(key).alpns(vec![ALPN.to_vec()]).bind().await.unwrap();
        let code = encode_code(&endpoint.id().to_string(), &token);
        let log: crate::session::LogFn = Arc::new(|m| eprintln!("[host] {m}"));
        let server = Arc::new(crate::session::HostServer::new(Arc::new(crate::session::MockPlatform::new()), "t".into(), log.clone()));
        let (ep2, tok2, srv2, log2) = (endpoint.clone(), token.clone(), server.clone(), log.clone());
        let host = tokio::spawn(async move { serve_loop(&ep2, &tok2, &srv2, &log2, std::future::pending()).await });

        // a wrong token is refused up front
        let bad = encode_code(&endpoint.id().to_string(), &new_token());
        assert!(connect(&bad).await.is_err());

        let bridge = connect(&code).await.expect("connect");
        let sink: crate::browser::FrameSink = Arc::new(|_: &[u8]| {});
        let desk = crate::session::SessionDesk::connect_target(bridge.target.clone(), None, "host", sink, Default::default()).await.expect("session over bridge");
        let png = desk.screenshot().await.expect("screenshot");
        assert!(png.len() > 100);
        desk.key("Return", 1).await.expect("key");
        desk.shutdown().await;
        drop(bridge);
        host.abort();
    }

    #[test]
    fn codes_round_trip_and_reject_junk() {
        let key = SecretKey::generate();
        let token = new_token();
        let code = encode_code(&key.public().to_string(), &token);
        let (id, t) = parse_code(&format!("  {code}\n")).unwrap();
        assert_eq!(id, key.public());
        assert_eq!(t, token);
        assert!(parse_code("ghostdesk:connect:v1:abc").is_err());
        assert!(parse_code(&format!("{PREFIX}%%%")).is_err());
        let short = encode_code(&key.public().to_string(), "abcd");
        assert!(parse_code(&short).is_err(), "a short token is refused");
    }

    #[test]
    fn login_item_plist_runs_the_host_and_escapes_paths() {
        let p = autostart::launch_agent_plist("/Applications/Ghost & Desk.app/Contents/MacOS/ghostdesk", "/tmp/h.log");
        assert!(p.contains("<string>--host</string>") && p.contains("<key>RunAtLoad</key><true/>") && p.contains("<key>KeepAlive</key><true/>"));
        assert!(p.contains("Ghost &amp; Desk") && !p.contains("Ghost & Desk"));
    }

    #[test]
    fn token_compare_and_hex() {
        assert!(same_token(b"abc", b"abc"));
        assert!(!same_token(b"abc", b"abd") && !same_token(b"abc", b"ab"));
        assert_eq!(unhex(&hex(&[0, 1, 254, 255])), Some(vec![0, 1, 254, 255]));
        assert!(unhex("abc").is_none() && unhex("zz").is_none());
        assert_ne!(new_token(), new_token());
    }
}
