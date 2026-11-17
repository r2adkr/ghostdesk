//! Local control server running inside the GhostDesk GUI.
//!
//! Lets external agents (the Claude app, Claude Code, Codex, …) create and drive isolated
//! screens through `ghostdesk --mcp`. Screens created from outside also show up in the GUI grid
//! with their virtual mouse.
//!
//! Protocol: JSON Lines over a local connection only the same user can open (`platform::ipc`).
//!   - macOS/Linux: Unix domain socket (mode 0600 + peer uid check)
//!   - Windows: named pipe `\\.\pipe\GhostDesk-control-<user SID>` (DACL allowing only this user
//!     and SYSTEM)
//!   Request   {"id": 1, "method": "open" | "list" | "act" | "close" | "hello", "params": {...}}
//!   Response  {"id": 1, "result": {...}} or {"id": 1, "error": "message"}
//!
//! Sensitive requests (cloning my login, using my running app, remote connections) only proceed
//! once the user allows them in the GUI.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use eframe::egui;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot, watch},
};

use crate::{
    agent::{self, AgentShared, AgentStatus, DeskTarget, LogKind, ProfileSource, RemoteTarget},
    desk::{Desk, DeskKind},
    llm::short,
    platform::{caps, ipc, text},
    profiles,
};

pub const PROTOCOL_VERSION: u32 = 1;

/// How long external actions wait at most while the user has taken over or paused.
const PAUSE_WAIT: Duration = Duration::from_secs(120);
/// How long to wait at most for an answer to the approval dialog.
const APPROVAL_WAIT: Duration = Duration::from_secs(180);
/// Marker for MCP workspaces created without login (empty profile). Folders with content but
/// without this marker are never opened as an empty profile.
const BLANK_MARKER: &str = "ghostdesk-mcp-blank";

/// Apps external agents can never open (shells, scripts, credentials, security settings).
///
/// GhostDesk itself isn't listed here: it's only opened as a separate test instance
/// (`is_ghostdesk`, `app_target`). Bundle ids are compared case-insensitively.
const EXTERNAL_DENY: &[&str] = &[
    // terminals and shells
    "com.apple.Terminal",
    "com.googlecode.iterm2",
    "dev.warp.Warp-Stable",
    "dev.warp.Warp",
    "net.kovidgoyal.kitty",
    "com.github.wez.wezterm",
    "io.alacritty",
    "org.alacritty",
    "com.mitchellh.ghostty",
    "co.zeit.hyper",
    "org.tabby",
    // scripting and automation
    "com.apple.ScriptEditor2",
    "com.apple.Automator",
    "com.apple.shortcuts",
    "com.latenightsw.ScriptDebugger8",
    // credentials
    "com.apple.keychainaccess",
    "com.apple.Passwords",
    "com.1password.1password",
    "com.agilebits.onepassword7",
    "com.bitwarden.desktop",
    "org.keepassxc.keepassxc",
    // system and security settings, file and process management
    "com.apple.systempreferences",
    "com.apple.finder",
    "com.apple.ActivityMonitor",
    "com.apple.Console",
    "com.apple.DiskUtility",
    "com.apple.DirectoryUtility",
    "com.apple.installer",
];

/// GhostDesk's own bundle id.
const GHOSTDESK_BUNDLE_ID: &str = "dev.ghostdesk.app";

/// Deny list by name (bundle file name, lowercase) for when the bundle id can't be read.
const EXTERNAL_DENY_NAMES: &[&str] = &[
    "terminal",
    "iterm",
    "iterm2",
    "warp",
    "kitty",
    "wezterm",
    "alacritty",
    "ghostty",
    "hyper",
    "tabby",
    "script editor",
    "automator",
    "shortcuts",
    "keychain access",
    "passwords",
    "system settings",
    "system preferences",
    "finder",
    "activity monitor",
    "console",
    "disk utility",
    "directory utility",
    "1password",
    "1password 7",
    "bitwarden",
    "keepassxc",
];

/// Events sent to the GUI.
pub enum ControlEvent {
    /// An external agent created a new screen (added to the grid).
    Register(Arc<AgentShared>),
    /// User approval is needed. If the requester gives up (timeout, disconnect), the receiving side
    /// of `reply` is dropped (the GUI clears items whose `reply.is_closed()`).
    Approval { title: String, detail: String, reply: oneshot::Sender<bool> },
}

/// This OS's word for regular apps in approval dialogs and errors ("macOS 앱" / "Windows 앱").
const NATIVE_APP_WORD: &str = if cfg!(target_os = "macos") { "macOS 앱" } else { text::LOCAL_APPS };
/// Label of the app identifier line in the approval dialog (macOS: bundle id, Windows: original
/// executable name and publisher).
const APP_ID_LABEL: &str = if cfg!(target_os = "macos") { "번들 id" } else { "앱 정보" };
/// Values an external agent may pass as app.
const APP_QUERY_HINT: &str = if cfg!(windows) { "앱 이름·실행 파일 이름·.exe/.lnk 경로" } else { "앱 이름·번들 id·.app 경로" };

/// The GhostDesk window's control endpoint (macOS:
/// ~/Library/Application Support/GhostDesk/control.sock, Windows:
/// \\.\pipe\GhostDesk-control-<user SID>).
pub fn endpoint() -> ipc::Endpoint {
    ipc::Endpoint::control()
}

/// One control connection (= one `ghostdesk --mcp` process).
#[derive(Clone)]
struct Conn {
    id: u64,
    /// true once the peer closes the connection.
    closed: watch::Receiver<bool>,
}

impl Conn {
    fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    async fn wait_closed(&self) {
        let mut rx = self.closed.clone();
        // a vanished sender (Err) also counts as the connection being over
        let _ = rx.wait_for(|c| *c).await;
    }
}

/// A screen opened by an external agent.
struct DeskEntry {
    shared: Arc<AgentShared>,
    /// id of the connection that created this screen. Other connections can't drive or close it
    /// unless `shared_access`.
    conn: u64,
    shared_access: bool,
    /// Workspace reservation key (released on close).
    reservation: String,
}

/// Workspace reservation. Released automatically on drop if opening fails.
struct Reservation<'a> {
    set: &'a Mutex<HashSet<String>>,
    key: String,
    name: String,
    keep: bool,
}

impl Reservation<'_> {
    /// The screen opened: hands the reservation over to the screen entry (released on close).
    fn keep(mut self) -> String {
        self.keep = true;
        std::mem::take(&mut self.key)
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if !self.keep {
            self.set.lock().remove(&self.key);
        }
    }
}

/// Workspace name → reservation key (lowercase, since APFS is case-insensitive).
fn reservation_key(workspace: &str) -> String {
    profiles::slug(workspace).to_lowercase()
}

pub struct ControlServer {
    desks: Mutex<HashMap<String, DeskEntry>>,
    /// Workspace names in use (including ones being prepared), so two programs never use the same
    /// folder at once.
    reserved: Mutex<HashSet<String>>,
    events: mpsc::UnboundedSender<ControlEvent>,
    ctx: egui::Context,
    next_id: AtomicU64,
    next_conn: AtomicU64,
    /// Tag that changes with every GUI run (so a desk_id from before a restart doesn't point at a new
    /// screen).
    instance: String,
}

impl ControlServer {
    pub fn new(events: mpsc::UnboundedSender<ControlEvent>, ctx: egui::Context) -> Arc<Self> {
        Arc::new(ControlServer {
            desks: Mutex::new(HashMap::new()),
            reserved: Mutex::new(HashSet::new()),
            events,
            ctx,
            next_id: AtomicU64::new(101),
            next_conn: AtomicU64::new(1),
            instance: instance_tag(),
        })
    }

    /// Opens the control endpoint (socket/named pipe) and accepts connections. Errors if another
    /// GhostDesk already has it.
    pub async fn serve(self: Arc<Self>) -> Result<()> {
        self.serve_on(&endpoint()).await
    }

    /// The body of `serve`. Tests run it on a temporary endpoint (`ipc::Endpoint::custom`).
    pub(crate) async fn serve_on(self: Arc<Self>, ep: &ipc::Endpoint) -> Result<()> {
        // errors if another window is already listening; cleans up a stale socket file and sets mode 0600
        // (Unix) / an owner-only DACL (Windows)
        let mut listener = ipc::Listener::bind(ep).await?;
        loop {
            // only same-user connections come back (the ipc layer drops other users)
            let (rd, wr) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    // transient errors like EMFILE: don't stop the server, pause briefly and keep accepting (avoids a
                    // busy loop)
                    eprintln!("[ghostdesk] 제어 소켓 accept 오류 (재시도): {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            };
            let me = self.clone();
            tokio::spawn(async move {
                let _ = me.handle_conn(rd, wr).await;
            });
        }
    }

    async fn handle_conn(self: Arc<Self>, rd: ipc::Reader, mut wr: ipc::Writer) -> Result<()> {
        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        // reading happens in a separate task so a dropped connection is noticed even while a request is
        // being handled, ending any approval or pause wait
        let (closed_tx, closed_rx) = watch::channel(false);
        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(rd).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                if line_tx.send(l).is_err() {
                    break;
                }
            }
            closed_tx.send_replace(true);
        });
        let conn = Conn { id: conn_id, closed: closed_rx };

        while let Some(line) = line_rx.recv().await {
            if line.trim().is_empty() {
                continue;
            }
            let resp = match serde_json::from_str::<Value>(&line) {
                Ok(req) => {
                    let id = req["id"].clone();
                    let method = req["method"].as_str().unwrap_or("").to_string();
                    match self.dispatch(&method, &req["params"], &conn).await {
                        Ok(r) => json!({ "id": id, "result": r }),
                        Err(e) => json!({ "id": id, "error": format!("{e:#}") }),
                    }
                }
                Err(e) => json!({ "id": null, "error": format!("JSON 오류: {e}") }),
            };
            if wr.write_all(format!("{resp}\n").as_bytes()).await.is_err() {
                break;
            }
        }
        reader.abort();
        // the connection is over (EOF, error, write failure): close every screen it created (so headless
        // browsers and hidden instances don't pile up, and user apps hidden by attach are restored)
        self.close_conn_desks(conn_id).await;
        Ok(())
    }

    async fn dispatch(&self, method: &str, p: &Value, conn: &Conn) -> Result<Value> {
        match method {
            "hello" => Ok(json!({ "protocol": PROTOCOL_VERSION, "app": "GhostDesk", "version": env!("CARGO_PKG_VERSION") })),
            "list" => Ok(self.list(conn)),
            "open" => self.open(p, conn).await,
            "act" => self.act(p, conn).await,
            "close" => self.close(p, conn).await,
            other => Err(anyhow!("알 수 없는 요청: {other}")),
        }
    }

    /// Drops screens removed in the GUI from the list (releasing reservations, ending leftover
    /// programs).
    fn prune_removed(&self) {
        let gone: Vec<DeskEntry> = {
            let mut desks = self.desks.lock();
            let ids: Vec<String> =
                desks.iter().filter(|(_, e)| e.shared.removed.load(Ordering::SeqCst)).map(|(k, _)| k.clone()).collect();
            ids.into_iter().filter_map(|id| desks.remove(&id)).collect()
        };
        for e in gone {
            self.discard(e);
        }
    }

    /// Discards an entry: releases its reservation and shuts down any remaining screen in the
    /// background.
    fn discard(&self, e: DeskEntry) {
        self.reserved.lock().remove(&e.reservation);
        let left = e.shared.workspace.lock().take();
        if let Some(d) = left {
            tokio::spawn(async move { d.shutdown().await });
        }
    }

    /// Closes an entry: shuts the screen down, then releases the reservation.
    async fn shutdown_entry(&self, e: DeskEntry, why: &str) {
        let desk = e.shared.workspace.lock().take();
        if let Some(d) = desk {
            d.shutdown().await;
        }
        self.reserved.lock().remove(&e.reservation);
        e.shared.log(LogKind::Info, why.to_string());
        e.shared.set_status(AgentStatus::Idle("닫힘".into()));
        self.ctx.request_repaint();
    }

    /// Closes every screen created by a dropped connection.
    async fn close_conn_desks(&self, conn: u64) {
        let mine: Vec<DeskEntry> = {
            let mut desks = self.desks.lock();
            let ids: Vec<String> = desks.iter().filter(|(_, e)| e.conn == conn).map(|(k, _)| k.clone()).collect();
            ids.into_iter().filter_map(|id| desks.remove(&id)).collect()
        };
        for e in mine {
            self.shutdown_entry(e, "외부 에이전트 연결이 끊겨 이 화면을 닫았습니다").await;
        }
    }

    fn get(&self, p: &Value, conn: &Conn) -> Result<(String, Arc<AgentShared>, Desk)> {
        let id = p["desk_id"].as_str().ok_or_else(|| anyhow!("desk_id 가 필요합니다"))?.to_string();
        self.prune_removed();
        let shared = {
            let desks = self.desks.lock();
            let e = desks.get(&id).ok_or_else(|| anyhow!("화면 {id} 를 찾을 수 없습니다 (desk_list 로 확인)"))?;
            if e.conn != conn.id && !e.shared_access {
                bail!("화면 {id} 는 다른 에이전트(연결)가 연 화면이라 조작할 수 없습니다. desk_open 으로 새 화면을 여세요");
            }
            e.shared.clone()
        };
        let desk = shared.workspace().ok_or_else(|| anyhow!("화면 {id} 는 이미 닫혔습니다 (GhostDesk 창에서 제거되었을 수 있음)"))?;
        if desk.is_closed() {
            bail!("화면 {id} 의 프로그램이 종료되었습니다");
        }
        Ok((id, shared, desk))
    }

    /// If the user removed the screen in the GhostDesk window, cleans up and returns an error.
    fn ensure_not_removed(&self, id: &str, shared: &Arc<AgentShared>) -> Result<()> {
        if shared.removed.load(Ordering::SeqCst) {
            let e = self.desks.lock().remove(id);
            if let Some(e) = e {
                self.discard(e);
            }
            bail!("사용자가 GhostDesk 창에서 화면 {id} 를 제거했습니다");
        }
        Ok(())
    }

    fn list(&self, conn: &Conn) -> Value {
        self.prune_removed();
        let desks = self.desks.lock();
        let mut out = Vec::new();
        for (id, e) in desks.iter() {
            let s = &e.shared;
            let Some(d) = s.workspace() else { continue };
            let (w, h) = d.size();
            let mine = e.conn == conn.id;
            out.push(json!({
                "desk_id": id,
                "name": s.name,
                "kind": kind_name(d.kind()),
                "size": [w, h],
                "closed": d.is_closed(),
                "mine": mine,
                "shared": e.shared_access,
                "controllable": mine || e.shared_access,
                "views": d.views_text(),
            }));
        }
        json!({ "desks": out })
    }

    /// Shows an approval dialog in the GUI and waits. On timeout or requester disconnect, drops the
    /// receiving side so the dialog goes away.
    async fn ask(&self, conn: &Conn, title: &str, detail: String) -> Result<()> {
        if conn.is_closed() {
            bail!("요청한 에이전트의 연결이 끊겨 승인 요청을 취소했습니다");
        }
        let (tx, rx) = oneshot::channel();
        self.events
            .send(ControlEvent::Approval { title: title.to_string(), detail, reply: tx })
            .map_err(|_| anyhow!("GhostDesk 창이 닫혀 있습니다"))?;
        self.ctx.request_repaint();
        let res = tokio::select! {
            r = tokio::time::timeout(APPROVAL_WAIT, rx) => match r {
                Ok(Ok(true)) => Ok(()),
                Ok(Ok(false)) => Err(anyhow!("사용자가 GhostDesk 창에서 요청을 거부했습니다")),
                _ => Err(anyhow!("사용자 승인 대기 시간이 지났습니다 (3분). GhostDesk 창을 확인해 달라고 사용자에게 요청하세요")),
            },
            _ = conn.wait_closed() => Err(anyhow!("요청한 에이전트의 연결이 끊겨 승인 요청을 취소했습니다")),
        };
        // rx was dropped, so have the GUI repaint and clear the leftover approval dialog
        self.ctx.request_repaint();
        res
    }

    /// Atomically reserves a workspace name, appending a number if it's already in use.
    fn reserve(&self, base: &str, n: u64) -> Reservation<'_> {
        let mut set = self.reserved.lock();
        let mut name = base.to_string();
        let mut i = 0u32;
        while set.contains(&reservation_key(&name)) {
            i += 1;
            name = if i == 1 { format!("{base}-{n}") } else { format!("{base}-{n}-{i}") };
        }
        let key = reservation_key(&name);
        set.insert(key.clone());
        Reservation { set: &self.reserved, key, name, keep: false }
    }

    async fn open(&self, p: &Value, conn: &Conn) -> Result<Value> {
        if conn.is_closed() {
            bail!("요청한 에이전트의 연결이 끊겼습니다");
        }
        let p = &apply_mode(p)?;
        self.prune_removed();
        let kind = p["kind"].as_str().unwrap_or("browser");
        let client = p["client"].as_str().unwrap_or("외부 에이전트").to_string();
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let label = p["name"].as_str().map(String::from).unwrap_or_else(|| format!("{kind}-{n}"));
        let desk_id = format!("d{}-{n}", self.instance);
        let use_login = p["use_my_login"].as_bool().unwrap_or(false);
        let shared_access = p["shared"].as_bool().unwrap_or(false);
        // screens with and without login cloning use separate workspace namespaces. Without login it's
        // "mcp_" (4th char '_'), with login "mcp-login-" (4th char '-'), so no workspace name (slug) can
        // make the two overlap (even on case-insensitive APFS). This stops a clone created with approval
        // from being reopened later by a "no login" request without approval.
        let ws_slug = profiles::slug(p["workspace"].as_str().unwrap_or(&label));
        let base = if use_login { format!("mcp-login-{ws_slug}") } else { format!("mcp_{ws_slug}") };
        // two programs using the same workspace at once would clash, so reserve atomically and number on
        // collision
        let reservation = self.reserve(&base, n);
        let workspace = reservation.name.clone();
        let ws_dir = profiles::workspaces_root().join(profiles::slug(&workspace));

        // kinds this OS can't use are refused before asking the user anything
        if let Some(why) = unsupported_kind(kind) {
            bail!("{why}");
        }

        // kind=remote (outside macOS): whether to connect even if the address actually connected to is
        // this computer itself. Only allowed once a person saw the "this computer's own VNC server"
        // warning in the approval dialog (None = follow the target's own check).
        #[cfg_attr(target_os = "macos", allow(unused_mut))]
        let mut remote_local_ok: Option<bool> = None;
        let target = match kind {
            "browser" => {
                let browsers = profiles::detect();
                let browser = browser_choice(&browsers, p["browser"].as_str().unwrap_or_default())?.clone();
                let source = if use_login {
                    let prof = profile_choice(&browser, p["browser_profile"].as_str())?;
                    self.ask(
                        conn,
                        "내 브라우저 로그인 사용 요청",
                        format!(
                            "{client} 이(가) {} 의 '{}' 프로필(로그인·쿠키)을 복제한 격리 브라우저를 열려고 합니다.\n복제본은 {} 의 GhostDesk 작업공간 '{workspace}' 에만 저장됩니다.",
                            browser.name,
                            prof.name,
                            text::THIS_COMPUTER
                        ),
                    )
                    .await?;
                    ProfileSource::Clone { browser: browser.clone(), profile_dir: prof.dir, refresh: p["refresh_login"].as_bool().unwrap_or(false) }
                } else {
                    ProfileSource::Empty { browser }
                };
                let (width, height) = desk_dims(p);
                DeskTarget::Browser {
                    source,
                    headless: true,
                    width,
                    height,
                    start_url: p["url"].as_str().unwrap_or("about:blank").to_string(),
                }
            }
            "app" => {
                let app = p["app"].as_str().ok_or_else(|| anyhow!("app ({APP_QUERY_HINT})이 필요합니다"))?;
                self.app_target(app, p, use_login, &client, &workspace, conn).await?
            }
            "session" => {
                let account = p["account"].as_str().unwrap_or("ghostdesk-agent").trim().to_string();
                if !valid_account_name(&account) {
                    bail!("account 는 계정 이름(영문 소문자·숫자·'.'·'_'·'-', 64자 이하)이어야 합니다 (받은 값: {account})");
                }
                // a Windows child session is the user's own account and reaches real data, so ask a person
                #[cfg(windows)]
                self.ask(
                    conn,
                    "자식 세션(PiP) 사용 요청",
                    format!(
                        "{client} 이(가) 이 PC 의 Windows 자식 세션(같은 계정의 별도 화면)을 열어 조작하려고 합니다.
                         내 마우스·키보드와는 분리되지만, 같은 Windows 계정이라 사용자의 파일·앱·로그인을 그대로 사용합니다."
                    ),
                )
                .await?;
                DeskTarget::Session { account }
            }
            "remote" => {
                let host = p["host"].as_str().ok_or_else(|| anyhow!("host 가 필요합니다"))?.to_string();
                let port = p["port"].as_u64().unwrap_or(5900) as u16;
                let dedicated = p["dedicated_machine"].as_bool().unwrap_or(false);
                let mut detail = format!("{client} 이(가) {host}:{port} 원격 데스크톱(VNC)에 접속하려고 합니다.");
                #[cfg(target_os = "macos")]
                if dedicated {
                    detail.push_str(
                        "\n\n\u{e193} 에이전트가 '이 대상은 에이전트 전용 Mac/VM 이다'라고 주장합니다. 허용하면 macOS 화면 공유 보호가 해제되어, \
                         대상이 다른 사람이 쓰는 Mac 이라면 그 Mac 의 화면이 다른 계정으로 전환될 수 있습니다. \
                         대상이 정말 에이전트 전용 머신일 때만 허용하세요.",
                    );
                }
                // outside macOS: this computer's own VNC server may be the user's real screen. Check before asking
                // a person: refuse without asking unless the agent claimed a dedicated machine, and if it did,
                // show that risk prominently in the approval dialog. Names aren't resolved before approval (so no
                // DNS query goes out for an agent-chosen name without approval): only addresses and localhost are
                // checked here, and the address actually connected to is checked at connect time
                // (agent::prepare_remote → rfb). Requests that didn't show the "this computer itself" warning here
                // are refused if the connected address turns out to be this computer, even with confirmation.
                #[cfg(not(target_os = "macos"))]
                {
                    let local = remote_is_this_computer(&host, port);
                    remote_local_ok = Some(local && dedicated);
                    if local && !dedicated {
                        bail!("{}", agent::local_vnc_refusal(&host, port));
                    }
                    if local {
                        detail.push_str(&format!(
                            "\n\n\u{e193} 주의: {host}:{port} 은(는) {pc} 자신의 VNC 서버입니다. 이 서버가 지금 보고 계신 화면을 내보내고 있다면, \
                             허용하는 순간 에이전트가 사용자의 실제 화면·마우스·키보드를 직접 조작합니다. \
                             WSL·Hyper-V 가상 머신처럼 {pc} 주소로 전달되는 별도 데스크톱이 확실할 때만 허용하세요.",
                            pc = text::THIS_COMPUTER
                        ));
                    }
                    if dedicated {
                        detail.push_str(&format!(
                            "\n\n\u{e193} 에이전트가 '이 대상은 에이전트 전용 머신/VM 이다'라고 주장합니다. 허용하면 '{pc} 자신의 VNC 서버' 보호와 \
                             macOS 화면 공유 보호가 해제됩니다. 대상이 Mac 이라면, 다른 사람이 쓰는 Mac 일 때 그 Mac 의 화면이 다른 계정으로 \
                             전환될 수 있습니다. 대상이 정말 에이전트 전용 머신일 때만 허용하세요.",
                            pc = text::THIS_COMPUTER
                        ));
                    }
                }
                self.ask(conn, "원격 데스크톱 접속 요청", detail).await?;
                DeskTarget::Remote(RemoteTarget {
                    host,
                    port,
                    username: p["username"].as_str().map(String::from),
                    password: p["password"].as_str().map(String::from),
                    dedicated_ack: dedicated,
                })
            }
            other => bail!("kind 는 browser / app / session / remote 중 하나여야 합니다 (받은 값: {other})"),
        };

        // opening with an empty profile, but the folder has earlier data (login clone leftovers, …):
        // refuse
        let blank = matches!(
            &target,
            DeskTarget::Browser { source: ProfileSource::Empty { .. }, .. }
                | DeskTarget::Electron { profile: crate::electron::ElectronProfile::Empty, .. }
        );
        if blank {
            check_blank_workspace(&ws_dir, &workspace)?;
        }
        if conn.is_closed() {
            bail!("요청한 에이전트의 연결이 끊겼습니다");
        }

        let shared = AgentShared::new(n, label.clone(), workspace.clone(), self.ctx.clone());
        *shared.external.lock() = Some(client.clone());
        shared.log(LogKind::Info, format!("{client} 이(가) 이 화면을 만들었습니다 (MCP)"));
        let _ = self.events.send(ControlEvent::Register(shared.clone()));
        self.ctx.request_repaint();

        let desk = match agent::prepare_workspace_with(&shared, &workspace, &target, remote_local_ok).await {
            Ok(d) => d,
            Err(e) => {
                shared.log(LogKind::Error, format!("{e:#}"));
                shared.set_status(AgentStatus::Error(short(&format!("{e}"), 80)));
                return Err(e);
            }
        };
        if blank {
            let _ = std::fs::create_dir_all(&ws_dir);
            let _ = std::fs::write(ws_dir.join(BLANK_MARKER), "GhostDesk MCP 로그인 미사용 작업공간\n");
        }
        // if the user removed it in the GUI during preparation, close it right away so no invisible screen
        // is left. (remove_agent sets removed first and takes under the same lock, so only one side ends
        // up owning the screen.)
        let stored = {
            let mut slot = shared.workspace.lock();
            if shared.removed.load(Ordering::SeqCst) {
                false
            } else {
                *slot = Some(desk.clone());
                true
            }
        };
        if !stored {
            desk.shutdown().await;
            bail!("사용자가 GhostDesk 창에서 이 화면을 제거했습니다");
        }
        // if the requester left during preparation (cancelled, exited), don't leave a screen nobody will
        // use
        if conn.is_closed() {
            let d = shared.workspace.lock().take();
            if let Some(d) = d {
                d.shutdown().await;
            }
            shared.log(LogKind::Info, "요청한 에이전트의 연결이 끊겨 화면을 닫았습니다");
            shared.set_status(AgentStatus::Idle("닫힘".into()));
            bail!("요청한 에이전트의 연결이 끊겨 화면을 닫았습니다");
        }
        shared.set_status(AgentStatus::Idle(format!("{client} 연결됨")));
        let reservation = reservation.keep();
        self.desks.lock().insert(desk_id.clone(), DeskEntry { shared: shared.clone(), conn: conn.id, shared_access, reservation });

        tokio::time::sleep(Duration::from_millis(300)).await;
        let png = desk.screenshot().await.ok();
        let (w, h) = desk.size();
        Ok(json!({
            "desk_id": desk_id,
            "name": label,
            "kind": kind_name(desk.kind()),
            "size": [w, h],
            "shared": shared_access,
            "notes": guide(desk.kind()),
            "views": desk.views_text(),
            "screenshot_png": png.map(|b| B64.encode(b)),
        }))
    }

    /// App name/bundle id/path → Electron (hidden clone) or macOS/Windows app (hidden window) target.
    async fn app_target(&self, app: &str, p: &Value, use_login: bool, client: &str, workspace: &str, conn: &Conn) -> Result<DeskTarget> {
        // auto already picks between hidden and offscreen, so those aren't exposed to external agents
        // (only the self-test uses them)
        let host = match p.get("app_host").filter(|v| !v.is_null()).map(|v| v.as_str()) {
            None | Some(Some("auto")) => crate::native::NativeHost::Auto,
            Some(Some("current_session")) => crate::native::NativeHost::CurrentSession,
            _ => bail!("app_host 는 auto / current_session 중 하나입니다"),
        };
        let current_session = host == crate::native::NativeHost::CurrentSession;
        let attach = p["attach_running"].as_bool().unwrap_or(false);
        if current_session && (!cfg!(windows) || attach) {
            bail!("current_session 은 Windows에서 기존 로그인으로 새로 실행하는 모드입니다. attach_running 없이 쓰세요.");
        }
        // modes this OS doesn't have are refused before looking up the app or asking for approval
        if p["virtual_display"].as_bool().unwrap_or(false) && !caps::VDISPLAY {
            bail!("{} kind=app 은 virtual_display 없이 쓰세요.", text::unsupported("virtual_display (가상 디스플레이)"));
        }
        let input = app.to_string();
        let r = tokio::task::spawn_blocking(move || resolve_external_app(&input))
            .await
            .map_err(|e| anyhow!("앱 확인 실패: {e}"))??;
        let bid = r.bundle_id.clone().unwrap_or_else(|| "알 수 없음".into());
        #[cfg(windows)]
        let bid = match &r.publisher {
            Some(p) => format!("{bid} — {p}"),
            None => bid,
        };

        // shells, scripts, credentials and security settings apps aren't opened for external agents
        if is_denied(&r) {
            bail!(
                "보안상 외부 에이전트는 '{}' ({bid}) 을(를) 열 수 없습니다. 터미널·스크립트·암호·시스템 설정 계열 앱은 차단됩니다.",
                r.name
            );
        }
        // GhostDesk itself: driving the current window could click its own approval dialogs or change
        // settings, keys and connections, so it's only opened as a freshly launched test instance with
        // an empty data folder (separate settings, keys, external connections and control endpoint)
        if is_ghostdesk(&r) {
            if cfg!(windows) || attach || current_session || use_login || p["virtual_display"].as_bool().unwrap_or(false) {
                bail!("GhostDesk 자신은 따로 떼어 낸 시험용 인스턴스(mode=app)로만 열 수 있습니다. 지금 쓰는 GhostDesk 창(app_running)이나 로그인 복제로는 열지 않습니다.");
            }
            let data = crate::platform::paths::data_root().join("nested").join(profiles::slug(workspace));
            self.ask(
                conn,
                "GhostDesk 시험용 인스턴스 실행 요청",
                format!(
                    "{client} 이(가) GhostDesk 를 하나 더, 보이지 않는 창으로 띄워 조작하려고 합니다.\n경로: {}\n새 GhostDesk 는 빈 데이터 폴더({})를 써서 이 창의 설정·API 키·외부 연결·승인과 섞이지 않고, 다른 AI 앱에 자동으로 연결하지도 않습니다.",
                    r.path.display(),
                    data.display()
                ),
            )
            .await?;
            return Ok(DeskTarget::Native(crate::native::NativeOptions {
                app: r.path.display().to_string(),
                new_instance: true,
                extra_args: vec![ISOLATED_DATA_ARG.into(), data.display().to_string()],
                host,
                ..Default::default()
            }));
        }
        // backends this OS can't use yet are refused before asking for approval. (Opening an Electron app
        // the regular-app way instead would run it on the user's real data, so it isn't handed off like
        // that.)
        let native = current_session || attach || r.electron.is_none();
        if !native && !caps::ELECTRON {
            bail!("{}", text::unsupported("Electron 앱 숨김 실행"));
        }
        if native && !caps::NATIVE_APPS {
            bail!("{}", text::unsupported(&format!("{} 숨김 실행", text::LOCAL_APPS)));
        }

        if let Some(e) = r.electron.filter(|_| !native) {
            if !e.hideable {
                // opening it natively would run it on the user's real login data, so don't hand it off that way
                let alternative = if cfg!(windows) {
                    "대안: mode=session 으로 별도 Windows 화면에서 실행하거나, 앱을 완전히 종료한 뒤 mode=app_current_session 을 선택하세요. 두 방식 모두 실제 계정·데이터를 사용하며 별도 승인이 필요합니다."
                } else {
                    "대안: mode=app_running 으로 실행 중인 앱을 연결하거나 mode=app_vdisplay 를 선택하세요. 실제 계정·데이터를 사용하며 별도 승인이 필요합니다."
                };
                bail!(
                    "'{}' 은(는) Electron 앱이지만 숨김 실행을 지원하지 않습니다: {}. 사용자의 실제 계정 데이터로 열리는 것을 막기 위해 {NATIVE_APP_WORD} 방식으로도 열지 않습니다. {alternative}",
                    e.name,
                    e.reason.clone().unwrap_or_else(|| "이유 미상".into())
                );
            }
            let profile = if use_login {
                let src = e.user_data_guess.clone().ok_or_else(|| anyhow!("{} 의 원본 데이터 폴더를 찾지 못했습니다", e.name))?;
                // show the .app bundle on macOS and the exe actually launched on Windows
                let shown = if cfg!(windows) { &e.exe } else { &e.bundle };
                self.ask(
                    conn,
                    "내 앱 로그인 사용 요청",
                    format!(
                        "{client} 이(가) {} 의 데이터({})를 복제한 숨김 인스턴스를 열려고 합니다.\n{APP_ID_LABEL}: {}\n경로: {}\n로그인 정보·쿠키가 작업공간 '{workspace}' 으로 복사됩니다.",
                        e.name,
                        src.display(),
                        e.bundle_id,
                        shown.display()
                    ),
                )
                .await?;
                crate::electron::ElectronProfile::Clone { source: src, refresh: p["refresh_login"].as_bool().unwrap_or(false) }
            } else {
                crate::electron::ElectronProfile::Empty
            };
            let (width, height) = desk_dims(p);
            return Ok(DeskTarget::Electron { app: e, profile, width, height });
        }

        // Windows: targets the local app backend always refuses at launch (Store apps, execution aliases,
        // Store launchers, console programs, …) are refused before asking a person (no approval dialog
        // for a request that would fail even if approved)
        #[cfg(windows)]
        {
            let exe = r.path.clone();
            tokio::task::spawn_blocking(move || crate::native::preflight_app(&exe))
                .await
                .map_err(|e| anyhow!("앱 확인 실패: {e}"))??;
        }
        // macOS apps aren't isolated and use the user's real data, so approval is always required
        let path = r.path.display().to_string();
        // experimental: run on a virtual display (real pixel clicks and drags work, at the cost of an
        // extra virtual monitor)
        if p["virtual_display"].as_bool().unwrap_or(false) {
            self.ask(
                conn,
                "가상 디스플레이 사용 요청 (실험)",
                format!(
                    "{client} 이(가) '{}' 을(를) 가상 모니터 위에서 실행해 조작하려고 합니다.\n번들 id: {bid}\n경로: {path}\n가상 모니터가 디스플레이 배치의 가장 바깥에 잠시 추가됩니다. 그동안 마우스 커서가 그쪽으로 넘어갈 수 있고, 설정에 따라 Dock·메뉴 막대가 그 화면에도 보일 수 있습니다. 작업이 끝나면 가상 모니터는 제거됩니다.\n이 앱은 격리되지 않으며 사용자의 실제 계정·데이터를 그대로 사용합니다.{}",
                    r.name,
                    if attach { "\n실행 중인 앱을 쓰므로 작업하는 동안 그 앱 창이 가상 모니터로 옮겨집니다." } else { "" }
                ),
            )
            .await?;
            let (width, height) = desk_dims(p);
            return Ok(DeskTarget::VDisplay(crate::vdisplay::VirtualDisplayOptions {
                app: path,
                width,
                height,
                attach_consent: attach,
                ..Default::default()
            }));
        }
        if attach {
            self.ask(
                conn,
                "실행 중인 내 앱 사용 요청",
                format!(
                    "{client} 이(가) 지금 실행 중인 '{}' 을(를) 조작하려고 합니다.\n{APP_ID_LABEL}: {bid}\n경로: {path}\n작업하는 동안 그 앱 창이 화면에서 숨겨지고, 끝나면 원래대로 돌아옵니다.",
                    r.name
                ),
            )
            .await?;
        } else {
            self.ask(
                conn,
                if current_session { "현재 계정 로그인 사용 요청" } else { "내 앱 실행 요청" },
                format!(
                    "{client} 이(가) {NATIVE_APP_WORD} '{}' 을(를) 보이지 않는 창으로 실행해 조작하려고 합니다.\n{APP_ID_LABEL}: {bid}\n경로: {path}\n이 앱은 격리되지 않으며 사용자의 실제 계정·데이터(로그인, 파일, 메시지 등)를 그대로 사용합니다.{}",
                    r.name,
                    if current_session { "\n현재 Windows 계정의 원본 프로필을 숨은 데스크톱에서 사용합니다. 복제본이 아니며 변경은 실제 계정에 반영됩니다. 같은 앱은 트레이까지 완전히 종료하세요. 숨은 실행에 실패하면 화면 밖 창으로 전환하지 않습니다." } else { "" }
                ),
            )
            .await?;
        }
        Ok(DeskTarget::Native(crate::native::NativeOptions {
            app: path,
            new_instance: !attach,
            attach_consent: attach,
            host,
            ..Default::default()
        }))
    }

    async fn act(&self, p: &Value, conn: &Conn) -> Result<Value> {
        let (id, shared, desk) = self.get(p, conn)?;
        self.ensure_not_removed(&id, &shared)?;
        // if the user pressed Stop, refuse this action (one refusal clears it)
        if shared.stop.swap(false, Ordering::SeqCst) {
            shared.log(LogKind::Info, "사용자가 '중지'를 눌러 외부 에이전트의 동작을 거부했습니다");
            bail!("사용자가 GhostDesk 창에서 '중지'를 눌렀습니다. 동작을 멈추고 사용자에게 다음 지시를 확인하세요.");
        }
        // while paused or taken over, wait until released (so user and agent input don't mix)
        if shared.paused.load(Ordering::SeqCst) {
            shared.log(LogKind::Info, "사용자가 직접 조작 중이라 외부 에이전트의 동작을 기다리게 합니다");
            let deadline = Instant::now() + PAUSE_WAIT;
            while shared.paused.load(Ordering::SeqCst) {
                if conn.is_closed() {
                    bail!("요청한 에이전트의 연결이 끊겼습니다");
                }
                self.ensure_not_removed(&id, &shared)?;
                if shared.stop.swap(false, Ordering::SeqCst) {
                    bail!("사용자가 GhostDesk 창에서 '중지'를 눌렀습니다. 동작을 멈추고 사용자에게 다음 지시를 확인하세요.");
                }
                if Instant::now() >= deadline {
                    bail!("사용자가 직접 조작 중입니다 (2분 동안 일시정지가 풀리지 않았습니다). 입력을 보내지 말고 잠시 후 다시 시도하세요.");
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            self.ensure_not_removed(&id, &shared)?;
            if desk.is_closed() {
                bail!("화면 {id} 의 프로그램이 종료되었습니다");
            }
        }
        let tool = p["tool"].as_str().unwrap_or("computer");
        let input = &p["input"];
        if tool == "computer" && input.get("actions").is_some() {
            return self.act_sequence(&shared, &desk, input, p["screenshot"].as_bool().unwrap_or(true)).await;
        }
        let name = match tool {
            "computer" => input["action"].as_str().ok_or_else(|| anyhow!("input.action 이 필요합니다"))?.to_string(),
            "browser" => "browser_control".to_string(),
            "app" => match desk.kind() {
                DeskKind::Electron => "app_menu".to_string(),
                _ => "app_control".to_string(),
            },
            other => bail!("tool 은 computer / browser / app 중 하나여야 합니다 (받은 값: {other})"),
        };
        let desc = agent::describe(&name, input);
        shared.log(LogKind::Action, desc.clone());
        shared.set_status(AgentStatus::Acting(short(&desc, 60)));

        let result = if tool == "computer" {
            let aim = |pt: (f64, f64)| {
                shared.cursor.lock().aim(pt, desk.mouse_pos());
                shared.ctx.request_repaint();
            };
            let r = agent::exec_computer(&desk, &name, input, &aim).await;
            shared.cursor.lock().record(&name, input, desk.mouse_pos(), false);
            r
        } else {
            agent::exec_desk_tool(&desk, &name, input).await
        };
        let client = shared.external.lock().clone().unwrap_or_default();
        shared.set_status(AgentStatus::Idle(format!("{client} 연결됨")));
        shared.ctx.request_repaint();
        let out = match result {
            Ok(o) => o,
            Err(e) => {
                shared.log(LogKind::Error, format!("{name} 실패: {e:#}"));
                return Err(e);
            }
        };
        let text = if out.text.is_empty() { "OK".to_string() } else { out.text };
        act_reply(&desk, text, out.image, p["screenshot"].as_bool().unwrap_or(true)).await
    }

    /// `input.actions`: runs several computer actions in order (a bundle like click → type → Enter in
    /// one call).
    ///
    /// Stops at the first failure or when the user stops/pauses, reporting how far it got. The screen
    /// is captured only once, at the end.
    async fn act_sequence(&self, shared: &Arc<AgentShared>, desk: &Desk, input: &Value, want_shot: bool) -> Result<Value> {
        let steps = input["actions"].as_array().ok_or_else(|| anyhow!("actions 는 동작 객체의 배열이어야 합니다"))?;
        anyhow::ensure!(!steps.is_empty(), "actions 가 비어 있습니다");
        anyhow::ensure!(steps.len() <= MAX_SEQUENCE, "actions 는 한 번에 {MAX_SEQUENCE}개까지입니다 (받은 수: {})", steps.len());
        let mut lines = Vec::new();
        let mut image = None;
        for (i, step) in steps.iter().enumerate() {
            let n = i + 1;
            let name = match step["action"].as_str() {
                Some(a) => a.to_string(),
                None => bail!("{}{n}번째 항목에 action 이 없습니다", done_prefix(&lines)),
            };
            if shared.stop.swap(false, Ordering::SeqCst) {
                shared.log(LogKind::Info, "사용자가 '중지'를 눌러 외부 에이전트의 동작을 멈췄습니다");
                bail!("{}사용자가 GhostDesk 창에서 '중지'를 눌러 {n}번째 동작부터 실행하지 않았습니다. 사용자에게 다음 지시를 확인하세요.", done_prefix(&lines));
            }
            if shared.paused.load(Ordering::SeqCst) {
                bail!("{}사용자가 직접 조작을 시작해 {n}번째 동작부터 실행하지 않았습니다. 잠시 후 화면을 확인하고 이어서 하세요.", done_prefix(&lines));
            }
            let desc = agent::describe(&name, step);
            shared.log(LogKind::Action, desc.clone());
            shared.set_status(AgentStatus::Acting(short(&format!("{n}/{} {desc}", steps.len()), 60)));
            let aim = |pt: (f64, f64)| {
                shared.cursor.lock().aim(pt, desk.mouse_pos());
                shared.ctx.request_repaint();
            };
            let r = agent::exec_computer(desk, &name, step, &aim).await;
            shared.cursor.lock().record(&name, step, desk.mouse_pos(), false);
            shared.ctx.request_repaint();
            match r {
                Ok(o) => {
                    lines.push(format!("{n}. {name}: {}", if o.text.is_empty() { "OK" } else { &o.text }));
                    if o.image.is_some() {
                        image = o.image;
                    }
                }
                Err(e) => {
                    shared.log(LogKind::Error, format!("{name} 실패: {e:#}"));
                    let client = shared.external.lock().clone().unwrap_or_default();
                    shared.set_status(AgentStatus::Idle(format!("{client} 연결됨")));
                    bail!("{}{n}번째 동작({name})이 실패해 거기서 멈췄습니다: {e:#}", done_prefix(&lines));
                }
            }
            // give the screen a moment to react to the previous action (about as fast as a person pressing in
            // a row)
            if n < steps.len() {
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
        }
        let client = shared.external.lock().clone().unwrap_or_default();
        shared.set_status(AgentStatus::Idle(format!("{client} 연결됨")));
        shared.ctx.request_repaint();
        // the screen at the end of the bundle matters, so capture a fresh one unless the last action was
        // a screenshot, even if there were screenshots midway
        let last_is_shot = steps.last().and_then(|s| s["action"].as_str()).is_some_and(|a| a == "screenshot" || a == "zoom");
        if !last_is_shot {
            image = None;
        }
        act_reply(desk, lines.join("\n"), image, want_shot).await
    }

    async fn close(&self, p: &Value, conn: &Conn) -> Result<Value> {
        let id = p["desk_id"].as_str().ok_or_else(|| anyhow!("desk_id 가 필요합니다"))?;
        let e = {
            let mut desks = self.desks.lock();
            let e = desks.get(id).ok_or_else(|| anyhow!("화면 {id} 를 찾을 수 없습니다"))?;
            if e.conn != conn.id && !e.shared_access {
                bail!("화면 {id} 는 다른 에이전트(연결)가 연 화면이라 닫을 수 없습니다");
            }
            desks.remove(id).expect("방금 확인한 항목")
        };
        self.shutdown_entry(e, "외부 에이전트가 화면을 닫았습니다").await;
        Ok(json!({ "closed": id }))
    }
}

/// Number of actions a single desk_action can run.
const MAX_SEQUENCE: usize = 50;

/// Preamble listing what already ran when a bundle stops midway.
fn done_prefix(lines: &[String]) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!("앞의 {}개는 실행했습니다 ({}). ", lines.len(), lines.join(" / "))
    }
}

/// Action result: text, events, window/tab list, screen (captured fresh when `want_shot` and the
/// action returned no screen).
async fn act_reply(desk: &Desk, text: String, image: Option<Vec<u8>>, want_shot: bool) -> Result<Value> {
    let mut image = image;
    if image.is_none() && want_shot {
        tokio::time::sleep(Duration::from_millis(400)).await;
        image = desk.screenshot().await.ok();
    }
    let notes = desk.take_notes();
    let (w, h) = desk.size();
    Ok(json!({
        "text": text,
        "notes": notes,
        "views": desk.views_text(),
        "size": [w, h],
        "screenshot_png": image.map(|b| B64.encode(b)),
    }))
}

/// Short tag that changes with every GUI run (pid + start time).
fn instance_tag() -> String {
    let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    format!("{:x}{:04x}", std::process::id(), millis & 0xffff)
}

/// Clamps a screen size received over MCP to what the model can see without downscaling.
fn desk_dims(p: &Value) -> (u32, u32) {
    let w = p["width"].as_u64().unwrap_or(1280).clamp(640, 1920) as u32;
    let h = p["height"].as_u64().unwrap_or(800).clamp(400, 1200) as u32;
    (w, h)
}

/// Session accounts in external requests must be plain account names (nothing that could be read
/// as a path or option).
fn valid_account_name(a: &str) -> bool {
    !a.is_empty()
        && a.len() <= 64
        && !a.starts_with('-')
        && !a.starts_with('.')
        && a.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// Checks a workspace folder to be opened with an empty profile: it must be empty or created
/// earlier without login.
fn check_blank_workspace(dir: &Path, workspace: &str) -> Result<()> {
    let has_content = dir.read_dir().map(|mut d| d.next().is_some()).unwrap_or(false);
    if has_content && !dir.join(BLANK_MARKER).is_file() {
        bail!(
            "작업공간 '{workspace}' 폴더에 로그인 복제 등 다른 방식으로 만든 데이터가 있어 로그인 미사용 화면으로 열 수 없습니다. 다른 workspace 이름을 쓰세요."
        );
    }
    Ok(())
}

/// An external request's app resolved to a real bundle.
struct ResolvedApp {
    /// Normalized .app bundle path (Windows: .exe path).
    path: PathBuf,
    bundle_id: Option<String>,
    name: String,
    /// Inspection result if it's an Electron app (the bundle itself is inspected, wherever it lives).
    electron: Option<crate::electron::ElectronApp>,
    /// Windows: original file name from the executable's VERSIONINFO (to recognize renamed copies).
    #[cfg(windows)]
    original_filename: Option<String>,
    /// Windows: publisher (VERSIONINFO CompanyName), shown in the approval dialog.
    #[cfg(windows)]
    publisher: Option<String>,
}

/// Normalization for comparison: whitespace removed, lowercase, trailing '/' and ".app" stripped.
fn norm_app_query(s: &str) -> String {
    let l = s.trim().trim_end_matches('/').to_lowercase();
    l.strip_suffix(".app").map(str::to_string).unwrap_or(l)
}

/// Outermost .app bundle in a path (e.g. X.app/Contents/MacOS/x → X.app).
fn outer_bundle(p: &Path) -> Option<PathBuf> {
    p.ancestors()
        .filter(|a| a.extension().map(|x| x.eq_ignore_ascii_case("app")).unwrap_or(false))
        .last()
        .map(Path::to_path_buf)
}

#[cfg(not(windows))]
fn bundle_id_of(path: &Path) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        crate::macos::apps::resolve_app(&path.to_string_lossy()).ok().and_then(|t| t.bundle_id)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        None
    }
}

#[cfg(target_os = "macos")]
fn path_for_bundle_id(id: &str) -> Option<PathBuf> {
    crate::macos::apps::resolve_app(id).ok().map(|t| t.path)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn path_for_bundle_id(_id: &str) -> Option<PathBuf> {
    None
}

/// App name, bundle id or path (trailing '/', doubled '/', with or without ".app", any case) → the
/// real bundle.
#[cfg(not(windows))]
fn resolve_external_app(input: &str) -> Result<ResolvedApp> {
    let raw = input.trim();
    if raw.is_empty() {
        bail!("app 이 비어 있습니다");
    }
    let electrons = crate::electron::detect_apps();
    let q = norm_app_query(raw);
    let found: Option<PathBuf> = if raw.contains('/') {
        let expanded = match raw.strip_prefix("~/") {
            Some(rest) => dirs::home_dir().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(raw)),
            None => PathBuf::from(raw),
        };
        let canon = std::fs::canonicalize(&expanded).map_err(|_| anyhow!("앱을 찾지 못했습니다: {raw}"))?;
        Some(outer_bundle(&canon).ok_or_else(|| anyhow!("{raw} 는 .app 번들이 아닙니다"))?)
    } else {
        electrons
            .iter()
            .find(|a| {
                a.bundle_id.to_lowercase() == raw.to_lowercase()
                    || norm_app_query(&a.name) == q
                    || a.bundle.file_stem().map(|s| s.to_string_lossy().to_lowercase() == q).unwrap_or(false)
            })
            .map(|a| a.bundle.clone())
            .or_else(|| resolve_app_name(raw).map(PathBuf::from))
            .or_else(|| path_for_bundle_id(raw))
    };
    let path = found.ok_or_else(|| anyhow!("앱을 찾지 못했습니다: {raw} (앱 이름·번들 id·.app 경로를 지정하세요)"))?;
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    if !path.is_dir() {
        bail!("앱을 찾지 못했습니다: {}", path.display());
    }
    // use the list entry if there is one (including whether it's running), otherwise inspect the
    // bundle itself wherever it lives
    let electron = electrons
        .into_iter()
        .find(|a| std::fs::canonicalize(&a.bundle).map(|b| b == path).unwrap_or(false))
        .or_else(|| crate::electron::inspect_app(&path));
    let bundle_id = bundle_id_of(&path).or_else(|| electron.as_ref().map(|e| e.bundle_id.clone()).filter(|b| !b.is_empty()));
    let name = electron
        .as_ref()
        .map(|e| e.name.clone())
        .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| raw.to_string());
    Ok(ResolvedApp { path, bundle_id, name, electron })
}

/// Windows: app name, executable name, .exe/.lnk path or Electron install folder → the real
/// executable (.exe only).
///
/// Order: (block by name first) → a path is used as is / a folder as an Electron install folder /
/// anything else (.lnk, …) is resolved by the local app backend; a name goes to the Electron app
/// list (name, executable name) → the local app backend (Start menu, App Paths, …).
#[cfg(windows)]
fn resolve_external_app(input: &str) -> Result<ResolvedApp> {
    use crate::platform::paths;
    let raw = input.trim().trim_matches('"').trim();
    if raw.is_empty() {
        bail!("app 이 비어 있습니다");
    }
    // network shares (\\host\…, //host/…, \\?\UNC\…) and device paths (\\.\…, \\?\GLOBALROOT\…) are
    // refused before touching the file system, since merely opening one makes SMB/WebDAV authenticate
    // to that host with the user's Windows credentials (NTLM). (The %VAR%-expanded form is checked
    // too; local \\?\C:\… is fine.)
    if win_policy::network_or_device(raw) || win_policy::network_or_device(&win_policy::expand_path(raw).to_string_lossy()) {
        bail!("보안상 외부 에이전트는 네트워크 공유·장치 경로의 앱을 열 수 없습니다: {raw}");
    }
    // shells and system tools are refused by name alone, before resolving a path (App Execution
    // Aliases like wt.exe can't be opened as real files, so they'd never reach the path check)
    if win_policy::denied_query(raw) {
        bail!("보안상 외부 에이전트는 '{raw}' 을(를) 열 수 없습니다. 터미널·스크립트·암호·시스템 설정 계열 앱과 GhostDesk 자신은 차단됩니다.");
    }
    let electrons = crate::electron::detect_apps();
    let q = win_policy::norm_query(raw);
    let looks_like_path = raw.contains(['\\', '/']) || raw.as_bytes().get(1) == Some(&b':');
    let picked: PathBuf = if looks_like_path {
        let p = win_policy::expand_path(raw);
        if win_policy::has_ext(&p, "exe") && p.is_file() {
            p
        } else if p.is_dir() {
            // an Electron app's install folder → that app's executable
            crate::electron::inspect_app(&p)
                .map(|e| e.exe)
                .or_else(|| crate::native::resolve_app_query(raw))
                .ok_or_else(|| anyhow!("{raw} 는 실행 파일(.exe)이 아닙니다"))?
        } else {
            // shortcuts (.lnk) and the like are turned into the real executable by the local app backend
            crate::native::resolve_app_query(raw).ok_or_else(|| anyhow!("앱을 찾지 못했습니다: {raw}"))?
        }
    } else {
        electrons
            .iter()
            .find(|a| win_policy::electron_matches(a, raw, &q))
            .map(|a| a.exe.clone())
            .or_else(|| crate::native::resolve_app_query(raw))
            .ok_or_else(|| anyhow!("앱을 찾지 못했습니다: {raw} ({APP_QUERY_HINT} 를 지정하세요)"))?
    };
    // if a shortcut (.lnk), App Paths entry, … points at a network share or device, refuse before
    // canonicalizing (which opens the file)
    if win_policy::network_or_device(&picked.to_string_lossy()) {
        bail!("보안상 외부 에이전트는 네트워크 공유·장치 경로의 앱을 열 수 없습니다: {}", picked.display());
    }
    // canonical path without the \\?\ prefix (shown as is in the approval dialog)
    let exe = paths::canonicalize(&picked).unwrap_or_else(|_| paths::simplify(&picked));
    // if a local path leads to a network share (a junction or shortcut pointing at one, …), stop
    // reading that file
    if win_policy::network_or_device(&exe.to_string_lossy()) {
        bail!("보안상 외부 에이전트는 네트워크 공유·장치 경로의 앱을 열 수 없습니다: {}", exe.display());
    }
    // the local app backend expands %VAR% in the path once more at launch, so for a path with % in it
    // the file checked and approved here could differ from the one actually run: refuse
    if exe.to_string_lossy().contains('%') {
        bail!("보안상 경로에 '%' 가 든 실행 파일은 열 수 없습니다: {}", exe.display());
    }
    // for security only executables (.exe) are opened (no scripts or config files like
    // .bat/.cmd/.ps1/.vbs/.msc/.cpl/.msi)
    if !win_policy::has_ext(&exe, "exe") {
        bail!("{} 는 실행 파일(.exe)이 아닙니다. 보안상 외부 에이전트는 .exe 앱만 열 수 있습니다", exe.display());
    }
    if !exe.is_file() {
        bail!("앱을 찾지 못했습니다: {}", exe.display());
    }
    // use the list entry if there is one (including whether it's running), otherwise inspect the
    // executable and install folder directly. Electron apps are only opened as hidden clone instances
    // (never on the user's real data, as the regular-app path would).
    let electron = electrons
        .into_iter()
        .find(|a| paths::same_path(&a.exe, &exe))
        .or_else(|| crate::electron::inspect_app(&exe))
        .or_else(|| exe.parent().and_then(crate::electron::inspect_app));
    let vi = crate::platform::win::version_info(&exe).unwrap_or_default();
    let bundle_id = electron
        .as_ref()
        .map(|e| e.bundle_id.clone())
        .filter(|b| !b.is_empty())
        .or_else(|| vi.original_filename.clone());
    let name = electron
        .as_ref()
        .map(|e| e.name.clone())
        .or_else(|| vi.file_description.clone())
        .or_else(|| vi.product_name.clone())
        .or_else(|| exe.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| raw.to_string());
    Ok(ResolvedApp { path: exe, bundle_id, name, electron, original_filename: vi.original_filename, publisher: vi.company_name })
}

/// GhostDesk's own .app bundle (installed or dev build).
fn own_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    outer_bundle(&exe)
}

/// Argument passed when launching GhostDesk as a separate instance (main.rs reads it to switch the
/// data folder and turn off auto-connect).
pub const ISOLATED_DATA_ARG: &str = "--isolated-data";

/// Whether the app being opened is GhostDesk itself (bundle id, this executable's bundle, bundle
/// name).
fn is_ghostdesk(r: &ResolvedApp) -> bool {
    r.bundle_id.as_deref().is_some_and(|b| b.eq_ignore_ascii_case(GHOSTDESK_BUNDLE_ID))
        || own_bundle().is_some_and(|o| o == r.path)
        || r.path.file_stem().is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case("ghostdesk"))
}

fn is_denied(r: &ResolvedApp) -> bool {
    if let Some(b) = &r.bundle_id {
        if EXTERNAL_DENY.iter().any(|d| d.eq_ignore_ascii_case(b)) {
            return true;
        }
    }
    let stem = r.path.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    if EXTERNAL_DENY_NAMES.contains(&stem.as_str()) {
        return true;
    }
    // Windows: executable name, original file name, display name, location (system folders,
    // packages), GhostDesk itself
    #[cfg(windows)]
    if win_policy::denied(r) {
        return true;
    }
    false
}

/// Outside macOS: whether a kind=remote target is this computer itself (loopback, unspecified, one
/// of this computer's addresses, localhost), checked before asking a person for approval.
///
/// No name resolution (DNS, LLMNR, …) is done: no network query goes out for an agent-chosen name
/// before approval, and a name with changing answers can't fool this check. Other names are
/// assumed not to be local, and the address actually connected to is checked at connect time (rfb's
/// refuse_local).
#[cfg(not(target_os = "macos"))]
fn remote_is_this_computer(host: &str, port: u16) -> bool {
    let h = host.trim();
    let h = h.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(h);
    if let Ok(ip) = h.parse::<std::net::IpAddr>() {
        return crate::rfb::is_local_addr(std::net::SocketAddr::new(ip, port));
    }
    let name = h.trim_end_matches('.').to_ascii_lowercase();
    name == "localhost" || name.ends_with(".localhost")
}

/// desk_open mode names (must match mcp.rs `modes`).
pub(crate) const MODES: [&str; 9] =
    ["web", "web_login", "app", "app_login", "app_current_session", "app_running", "app_vdisplay", "session", "remote"];

fn browser_choice<'a>(browsers: &'a [profiles::InstalledBrowser], query: &str) -> Result<&'a profiles::InstalledBrowser> {
    let want = query.trim().to_lowercase();
    if want.is_empty() {
        return browsers.first().ok_or_else(|| anyhow!("Chromium 계열 브라우저(Chrome/Edge/Brave/Whale 등)를 찾지 못했습니다"));
    }
    if let Some(browser) = browsers.iter().find(|b| b.name.to_lowercase() == want) {
        return Ok(browser);
    }
    let matches: Vec<_> = browsers.iter().filter(|b| b.name.to_lowercase().contains(&want)).collect();
    if let [browser] = matches.as_slice() {
        return Ok(*browser);
    }
    bail!("브라우저 '{query}' 를 하나로 선택할 수 없습니다. 감지된 이름을 지정하세요: {}", browsers.iter().map(|b| b.name.as_str()).collect::<Vec<_>>().join(", "))
}

fn profile_choice(browser: &profiles::InstalledBrowser, query: Option<&str>) -> Result<profiles::ProfileInfo> {
    let Some(query) = query.filter(|s| !s.trim().is_empty()).map(str::trim) else {
        return browser.profiles.first().cloned().ok_or_else(|| anyhow!("{} 프로필이 없습니다", browser.name));
    };
    // Resolve only detected IDs/names; never turn a caller's profile string into a file path.
    if let Some(p) = browser.profiles.iter().find(|p| p.dir.eq_ignore_ascii_case(query)) {
        return Ok(p.clone());
    }
    let matches: Vec<_> = browser.profiles.iter().filter(|p| p.name.eq_ignore_ascii_case(query)).collect();
    if let [profile] = matches.as_slice() {
        return Ok((*profile).clone());
    }
    bail!("프로필 '{query}' 를 하나로 선택할 수 없습니다. 감지된 프로필 ID 를 지정하세요: {}", browser.profiles.iter().map(|p| format!("{} ({})", p.dir, p.name)).collect::<Vec<_>>().join(", "))
}

/// Map the public mode while preserving workspace and login-source options.
fn apply_mode(p: &Value) -> Result<Value> {
    for key in ["browser", "browser_profile", "workspace"] {
        if p.get(key).is_some_and(|v| !v.is_null() && !v.is_string()) {
            bail!("{key} 는 문자열이어야 합니다");
        }
    }
    if p.get("refresh_login").is_some_and(|v| !v.is_null() && !v.is_boolean()) {
        bail!("refresh_login 은 true 또는 false 여야 합니다");
    }
    let Some(mode) = p.get("mode").filter(|v| !v.is_null()) else { return Ok(p.clone()) };
    let (kind, opt): (&str, Option<(&str, Value)>) = match mode.as_str().unwrap_or("") {
        "web" => ("browser", None),
        "web_login" => ("browser", Some(("use_my_login", json!(true)))),
        "app" => ("app", None),
        "app_login" => ("app", Some(("use_my_login", json!(true)))),
        "app_current_session" => ("app", Some(("app_host", json!("current_session")))),
        "app_running" => ("app", Some(("attach_running", json!(true)))),
        "app_vdisplay" => ("app", Some(("virtual_display", json!(true)))),
        "session" => ("session", None),
        "remote" => ("remote", None),
        _ => bail!("mode 는 {} 중 하나입니다 (받은 값: {mode})", MODES.join(" / ")),
    };
    let mut q = p.as_object().cloned().unwrap_or_default();
    for k in ["kind", "use_my_login", "app_host", "attach_running", "virtual_display"] {
        q.remove(k);
    }
    q.insert("kind".into(), json!(kind));
    if let Some((k, v)) = opt {
        q.insert(k.into(), v);
    }
    Ok(Value::Object(q))
}

/// Reason a screen kind can't be used on this OS (to refuse before asking the user for approval).
fn unsupported_kind(kind: &str) -> Option<String> {
    match kind {
        "browser" if !caps::BROWSER => Some(text::unsupported("kind=browser")),
        "remote" if !caps::REMOTE => Some(text::unsupported("kind=remote (원격 데스크톱)")),
        "app" if !(caps::ELECTRON || caps::NATIVE_APPS) => Some(text::unsupported("kind=app (로컬 앱)")),
        "session" if !caps::SESSION => Some(format!(
            "{} 별도 계정이 필요하면 에이전트 전용 VM 에 VNC 서버를 띄우고 kind=remote 로 연결하세요.",
            text::unsupported("kind=session (별도 계정 세션)")
        )),
        _ => None,
    }
}

fn kind_name(k: DeskKind) -> &'static str {
    match k {
        DeskKind::Browser => "browser",
        DeskKind::Remote => "remote",
        DeskKind::Electron => "app(electron)",
        DeskKind::Native => {
            if cfg!(target_os = "macos") {
                "app(macos)"
            } else if cfg!(windows) {
                "app(windows)"
            } else {
                "app(native)"
            }
        }
        DeskKind::VDisplay => "app(vdisplay)",
        DeskKind::Session => "session",
    }
}

/// How to use Windows regular app screens (off-screen window + UI Automation).
const NATIVE_GUIDE_WINDOWS: &str = "사용자 화면 밖에 띄운 Windows 앱 창입니다 (사용자에게는 보이지 않습니다). 좌표는 스크린샷 픽셀입니다. 정확한 조작은 desk_app {action:ui_tree} 후 press/set_value/focus/menu 를 쓰세요. 단축키는 ctrl 기준입니다 (cmd 는 ctrl 로 바뀝니다).";

/// Per-screen usage notes for external agents.
pub fn guide(k: DeskKind) -> &'static str {
    match k {
        DeskKind::Browser => "격리된 headless 브라우저입니다. 스크린샷은 페이지 영역만 보이며 주소창이 없으니 desk_browser(navigate/back/new_tab...)로 이동하세요. 확인·입력 대화상자가 뜨면 desk_browser {action:dialog} 로 읽고 respond_dialog 로 현재 id 에 명시적으로 응답하세요. 좌표는 스크린샷 픽셀입니다.",
        DeskKind::Remote => "별도 원격 데스크톱(VNC)입니다. 좌표는 스크린샷 픽셀입니다. 한글 입력은 서버에 따라 안 될 수 있습니다.",
        DeskKind::Electron => "보이지 않게 띄운 Electron 앱 창입니다. 네이티브 메뉴는 desk_app {action:list|click, path:\"File > ...\"} 로 쓰세요. 확인·입력 대화상자는 desk_app {action:dialog} 로 읽고 respond_dialog 에 현재 source 와 id 를 지정해 응답하세요. 파일 열기·저장 대화상자는 notes 안내를 따르세요. 이 앱은 사용자의 실제 시스템 클립보드를 쓰므로 cmd/ctrl+c·x·v 는 거부됩니다. 글자 입력은 type 을 쓰세요.",
        DeskKind::Session if cfg!(windows) => "같은 Windows 계정의 자식 세션 화면 전체입니다. 시작 메뉴·작업 표시줄·alt+tab 을 사용할 수 있습니다. 정확한 클릭·입력은 desk_app {action:list_windows|switch_window|ui_tree|press|set_value|read|focus|select|choose} 를 쓰세요. id 는 다음 ui_tree 전까지 유효합니다. menus/menu 는 지원하지 않으므로 메뉴는 스크린샷과 키보드 단축키로 조작하세요. helper 재시작 안내가 나오면 업데이트된 GhostDesk 로 자식 세션을 다시 연결하세요. 사용자의 파일·로그인은 실제 계정의 것입니다.",
        DeskKind::Session => "전용 에이전트 계정의 별도 로그인 세션 전체 화면입니다. 사용자 화면이 아니므로 Dock·메뉴 막대·cmd+tab 을 자유롭게 써도 됩니다. 사용자의 로그인은 공유되지 않습니다.",
        DeskKind::VDisplay => crate::vdisplay::CONTROL_HINT,
        DeskKind::Native => {
            if cfg!(windows) {
                NATIVE_GUIDE_WINDOWS
            } else {
                "숨겨진 macOS 앱 창입니다. 클릭은 접근성 동작(버튼 누르기·입력칸 포커스)으로 바뀌며 드래그·우클릭은 안 됩니다. 정확한 조작은 desk_app {action:ui_tree} 후 press/set_value/focus/menu 를 쓰세요."
            }
        }
    }
}

#[cfg(not(windows))]
fn resolve_app_name(name: &str) -> Option<String> {
    let name = name.trim();
    if name.contains('/') {
        return None; // already a path
    }
    let want = norm_app_query(name);
    let mut dirs_ = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/Applications/Utilities"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Applications/Utilities"),
    ];
    if let Some(h) = dirs::home_dir() {
        dirs_.insert(1, h.join("Applications"));
    }
    for dir in dirs_ {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|x| x.eq_ignore_ascii_case("app")).unwrap_or(false)
                    && p.file_stem().map(|s| s.to_string_lossy().to_lowercase() == want).unwrap_or(false)
                {
                    return Some(p.display().to_string());
                }
            }
        }
    }
    None
}

/// Decides which apps external agents can't open on Windows.
///
/// The counterpart of macOS's bundle id list, checked in several layers:
/// 1. Executable name (lowercase, no extension): blocked on an exact or prefix match.
/// 2. VERSIONINFO OriginalFilename: also catches renamed copies (cmd.exe → tool.exe).
/// 3. Display name (FileDescription/Start menu name): "명령 프롬프트", "Registry Editor", ….
/// 4. Location: anything under %SystemRoot% is blocked by default (a few harmless ones allowed),
///    network paths are blocked, and executables inside blocked packages
///    (WindowsApps\<package>) are blocked.
/// 5. GhostDesk itself (the running file, the installed copy, its install folder).
#[cfg(windows)]
mod win_policy {
    use std::path::{Path, PathBuf};

    use super::ResolvedApp;
    use crate::platform::{paths, win};

    /// Executable names blocked on an exact match (lowercase, no extension).
    const DENY_EXE: &[&str] = &[
        // shells, terminals, remote shells
        "cmd", "wt", "wtd", "openconsole", "conhost", "bash", "sh", "zsh", "fish", "nu", "git-bash", "git-cmd", "mintty",
        "cmder", "alacritty", "hyper", "tabby", "warp", "kitty", "putty", "plink", "ssh", "telnet", "termius",
        "fluentterminal", "sudo", "gsudo",
        // script hosts, interpreters, abusable system binaries (LOLBins)
        "wscript", "cscript", "mshta", "rundll32", "regsvr32", "msbuild", "installutil", "cmstp", "forfiles", "schtasks",
        "at", "sc", "bitsadmin", "certutil", "wmic", "psexec", "psexec64", "runas", "hh", "msdt", "pcalua",
        "scriptrunner", "mavinject", "msxsl", "odbcconf", "regasm", "regsvcs", "ieexec", "presentationhost",
        "syncappvpublishingserver", "appvlp", "ftp", "tftp", "py", "pyw", "node", "deno", "bun", "perl", "ruby", "php",
        "php-cgi", "php-win", "java", "javaw", "lua", "tclsh", "wish", "code-tunnel", "code-insiders-tunnel",
        // automation
        "pad.console.host", "pad.designer", "pad.robot",
        // credentials
        "keeper", "keeperpasswordmanager", "credwiz", "proton pass",
        // system and security settings (the counterparts of System Settings, Disk Utility, Installer)
        "systemsettings", "control", "mmc", "regedit", "reg", "msconfig", "useraccountcontrolsettings", "netplwiz",
        "optionalfeatures", "eventvwr", "perfmon", "resmon", "diskpart", "dfrgui", "bcdedit", "dism", "rstrui",
        "sechealthui", "securityhealthsystray", "msiexec", "winget", "appinstaller", "gpedit", "secpol", "certmgr",
        "compmgmt", "services", "lusrmgr", "taskschd", "devmgmt", "wf", "fodhelper", "computerdefaults", "sdclt",
        // accessibility tools that interfere with the user's real screen and input
        "osk", "magnify", "narrator", "sethc", "utilman",
        // process management (the counterpart of Activity Monitor)
        "taskmgr", "tcpview", "tcpview64",
        // file managers (the counterpart of Finder)
        "explorer", "totalcmd", "totalcmd64", "dopus", "xyplorer", "freecommander", "onecommander", "files",
        // remote control and screen sharing
        "mstsc", "msra", "quickassist", "teamviewer", "anydesk", "rustdesk",
    ];

    /// Blocked when starting with these prefixes (variants like powershell_ise, pwsh-preview,
    /// ubuntu2204, keepassxc, regedt32, …).
    const DENY_EXE_PREFIX: &[&str] = &[
        "powershell", "pwsh", "windowsterminal", "wezterm", "mobaxterm", "conemu", "wsl", "ubuntu", "debian", "kali",
        "opensuse", "sles", "oraclelinux", "archlinux", "python", "autohotkey", "autoit3", "keepass", "1password",
        "bitwarden", "lastpass", "dashlane", "enpass", "roboform", "nordpass", "regedt", "systemproperties", "procexp",
        "procmon", "autoruns", "processhacker", "systeminformer", "ghostdesk",
    ];

    /// Blocked by display name (lowercase): Start menu name, FileDescription.
    const DENY_DISPLAY: &[&str] = &[
        "command prompt", "명령 프롬프트", "windows command processor", "windows terminal", "터미널", "terminal",
        "windows powershell", "windows powershell ise", "powershell 7", "git bash", "git cmd",
        "windows subsystem for linux", "registry editor", "레지스트리 편집기", "registry console tool",
        "task manager", "작업 관리자", "file explorer", "파일 탐색기", "windows explorer", "control panel", "제어판",
        "windows control panel", "settings", "설정", "windows settings", "windows security", "windows 보안",
        "services", "서비스", "computer management", "컴퓨터 관리", "event viewer", "이벤트 뷰어", "resource monitor",
        "리소스 모니터", "performance monitor", "성능 모니터", "system configuration", "시스템 구성",
        "microsoft management console", "credential manager", "자격 증명 관리자", "remote desktop connection",
        "원격 데스크톱 연결", "quick assist", "빠른 지원", "on-screen keyboard", "화상 키보드", "narrator", "내레이터",
        "magnifier", "돋보기", "task scheduler", "작업 스케줄러", "windows script host", "microsoft (r) html application host",
    ];

    /// Executables inside these packages (WindowsApps\<name>_<version>_…) are blocked (lowercase
    /// prefixes).
    const DENY_PACKAGES: &[&str] = &[
        "microsoft.windowsterminal",
        "microsoft.powershell",
        "microsoftcorporationii.windowssubsystemforlinux",
        "canonicalgrouplimited.",
        "thedebianproject.",
        "kalilinux.",
        "46932suse.",
        "whitewaterfoundryltd.",
        "windows.immersivecontrolpanel",
        "microsoft.sechealthui",
        "microsoft.desktopappinstaller",
        "microsoftcorporationii.quickassist",
        "microsoft.powerautomatedesktop",
        "8bitsolutionsllc.bitwardendesktop",
        "agilebits.",
    ];

    /// Harmless built-in tools allowed as exceptions directly under %SystemRoot%, System32 and
    /// SysWOW64.
    const SYSTEM_ALLOW: &[&str] = &["notepad", "mspaint", "calc", "charmap", "write"];

    /// Normalization for comparison: strips quotes and trailing separators, keeps the last path
    /// segment, lowercases, and removes ".exe"/".lnk"/".app".
    pub(super) fn norm_query(s: &str) -> String {
        let t = s.trim().trim_matches('"').trim_end_matches(['/', '\\']);
        let last = t.rsplit(['/', '\\']).next().unwrap_or(t);
        let l = super::norm_app_query(last);
        for ext in [".exe", ".lnk"] {
            if let Some(x) = l.strip_suffix(ext) {
                return x.trim_end().to_string();
            }
        }
        l
    }

    pub(super) fn has_ext(p: &Path, ext: &str) -> bool {
        p.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext))
    }

    /// Whether the executable name (lowercase, no extension) is on the deny list.
    pub(super) fn denied_stem(stem: &str) -> bool {
        let s = stem.trim();
        !s.is_empty() && (DENY_EXE.contains(&s) || DENY_EXE_PREFIX.iter().any(|p| s.starts_with(p)))
    }

    fn denied_name(name: &str) -> bool {
        let n = name.trim().to_lowercase();
        !n.is_empty() && (DENY_DISPLAY.contains(&n.as_str()) || denied_stem(&n))
    }

    /// Whether to refuse based on the request string alone, before finding the app (name, executable
    /// name, file name of a path).
    pub(super) fn denied_query(raw: &str) -> bool {
        denied_name(&norm_query(raw))
    }

    /// Whether an Electron app list entry matches the request (id, name, executable name, install
    /// folder name).
    pub(super) fn electron_matches(a: &crate::electron::ElectronApp, raw: &str, q: &str) -> bool {
        let lower = |p: &Path| p.file_name().map(|s| s.to_string_lossy().to_lowercase());
        let exe_stem = a.exe.file_stem().map(|s| s.to_string_lossy().to_lowercase());
        (!a.bundle_id.is_empty() && a.bundle_id.to_lowercase() == raw.to_lowercase())
            || norm_query(&a.name) == q
            || exe_stem.as_deref() == Some(q)
            || lower(&a.bundle).as_deref() == Some(q)
    }

    /// Expands `%VAR%` and a leading `~\`.
    pub(super) fn expand_path(raw: &str) -> PathBuf {
        let mut s = String::new();
        let mut rest = raw;
        while let Some(i) = rest.find('%') {
            s.push_str(&rest[..i]);
            let after = &rest[i + 1..];
            match after.find('%') {
                Some(j) if j > 0 => {
                    let name = &after[..j];
                    match std::env::var_os(name) {
                        Some(v) => s.push_str(&v.to_string_lossy()),
                        None => {
                            s.push('%');
                            s.push_str(name);
                            s.push('%');
                        }
                    }
                    rest = &after[j + 1..];
                }
                _ => {
                    s.push('%');
                    rest = after;
                }
            }
        }
        s.push_str(rest);
        if let Some(r) = s.strip_prefix("~\\").or_else(|| s.strip_prefix("~/")) {
            if let Some(h) = dirs::home_dir() {
                return h.join(r);
            }
        }
        PathBuf::from(s)
    }

    /// Whether it's a network share or device path (only looks at the string, never touches the file
    /// system).
    ///
    /// Covers `\\host\…`, `//host/…`, `\\?\UNC\…`, `\\?\GLOBALROOT\…`, `\\.\…` and NT paths `\??\…`.
    /// A local drive's `\\?\C:\…` isn't one.
    pub(super) fn network_or_device(s: &str) -> bool {
        let s = s.trim().trim_matches('"').trim().replace('/', "\\");
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            let b = rest.as_bytes();
            return !(b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\');
        }
        s.starts_with(r"\\") || s.starts_with(r"\??\")
    }

    /// Local path string for comparison (lowercase, '\\' separators, \\?\ prefix removed). None for
    /// network and device paths.
    fn local_form(p: &Path) -> Option<String> {
        let s = p.to_string_lossy().replace('/', "\\").to_lowercase();
        let s = match s.strip_prefix(r"\\?\") {
            Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_string(),
            Some(_) => return None,
            None if s.starts_with(r"\\") => return None,
            None => s,
        };
        Some(s.trim_end_matches('\\').to_string())
    }

    /// The Windows folder (usually C:\Windows), from the known-folder API so it's right even when run
    /// with the environment cleared.
    fn system_root() -> String {
        static ROOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        ROOT.get_or_init(|| {
            use windows_sys::Win32::UI::Shell::{FOLDERID_Windows, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
            let mut found = None;
            unsafe {
                let mut p: *mut u16 = std::ptr::null_mut();
                if SHGetKnownFolderPath(&FOLDERID_Windows, KF_FLAG_DEFAULT as _, std::ptr::null_mut(), &mut p) == 0 && !p.is_null() {
                    found = Some(win::from_wide_ptr(p));
                }
                if !p.is_null() {
                    windows_sys::Win32::System::Com::CoTaskMemFree(p as *const _);
                }
            }
            let root = found
                .filter(|s| !s.is_empty())
                .or_else(|| std::env::var("SystemRoot").ok())
                .or_else(|| std::env::var("windir").ok())
                .unwrap_or_else(|| r"C:\Windows".into());
            local_form(Path::new(&root)).unwrap_or_else(|| r"c:\windows".into())
        })
        .clone()
    }

    /// Whether it's under %SystemRoot% and not on the allowlist (system folders hold hundreds of
    /// shells, admin tools and LOLBins, so they're blocked by default).
    fn system_dir_denied(s: &str) -> bool {
        let root = system_root();
        let Some(rest) = s.strip_prefix(root.as_str()).and_then(|r| r.strip_prefix('\\')) else { return false };
        let (dir, file) = rest.rsplit_once('\\').unwrap_or(("", rest));
        let stem = file.strip_suffix(".exe").unwrap_or(file);
        !(matches!(dir, "" | "system32" | "syswow64") && SYSTEM_ALLOW.contains(&stem))
    }

    /// Whether it's an executable inside a blocked package (Windows Terminal, PowerShell, WSL,
    /// Settings, Windows Security, …).
    fn denied_package(s: &str) -> bool {
        let parts: Vec<&str> = s.split('\\').collect();
        parts.windows(2).any(|w| {
            w[0] == "windowsapps" && {
                let name = w[1].split('_').next().unwrap_or("");
                DENY_PACKAGES.iter().any(|d| name.starts_with(d))
            }
        })
    }

    /// GhostDesk itself (the running file, the installed copy, other executables in its install
    /// folder).
    fn is_own(p: &Path, s: &str) -> bool {
        if std::env::current_exe().is_ok_and(|me| paths::same_path(p, &me)) {
            return true;
        }
        let installed = crate::install::stable_exe();
        if paths::same_path(p, &installed) {
            return true;
        }
        installed
            .parent()
            .filter(|d| d.file_name().is_some_and(|n| n.eq_ignore_ascii_case("GhostDesk")))
            .and_then(local_form)
            .is_some_and(|d| s.starts_with(&format!("{d}\\")))
    }

    /// Verdict for a single executable path (name, location, GhostDesk itself).
    fn denied_exe(p: &Path) -> bool {
        let stem = p.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        if denied_stem(&stem) {
            return true;
        }
        // network shares (\\server\…, \\localhost\C$\Windows\…) and device paths can't be located, so
        // they're blocked
        let Some(s) = local_form(p) else { return true };
        system_dir_denied(&s) || denied_package(&s) || is_own(p, &s)
    }

    fn denied_original_name(o: Option<&str>) -> bool {
        o.is_some_and(|o| {
            let n = o.trim().to_lowercase();
            // strip every stacked extension, like pwsh.dll or PowerShell.EXE.MUI
            let stem = n.split('.').next().unwrap_or("");
            denied_stem(stem) || denied_stem(&norm_query(&n))
        })
    }

    pub(super) fn denied(r: &ResolvedApp) -> bool {
        if denied_exe(&r.path) || denied_original_name(r.original_filename.as_deref()) || denied_name(&r.name) {
            return true;
        }
        if let Some(e) = &r.electron {
            if denied_exe(&e.exe) || denied_name(&e.name) {
                return true;
            }
            if denied_original_name(win::version_info(&e.exe).and_then(|v| v.original_filename).as_deref()) {
                return true;
            }
        }
        false
    }

    #[cfg(test)]
    pub(super) fn system_root_for_test() -> String {
        system_root()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_browser_and_profile_do_not_silently_use_another_login() {
        let browser = profiles::InstalledBrowser {
            name: "Microsoft Edge".into(), exe: "msedge.exe".into(), data_dir: "browser-data".into(),
            profiles: vec![
                profiles::ProfileInfo { dir: "Default".into(), name: "Personal".into() },
                profiles::ProfileInfo { dir: "Profile 1".into(), name: "Work".into() },
            ],
        };
        let browsers = vec![browser];
        assert_eq!(browser_choice(&browsers, "").unwrap().name, "Microsoft Edge");
        assert_eq!(browser_choice(&browsers, "edge").unwrap().name, "Microsoft Edge");
        assert!(browser_choice(&browsers, "Chrome").is_err(), "an explicit browser must not fall back to another account source");
        let b = &browsers[0];
        assert_eq!(profile_choice(b, None).unwrap().dir, "Default");
        assert_eq!(profile_choice(b, Some("Work")).unwrap().dir, "Profile 1");
        assert_eq!(profile_choice(b, Some("profile 1")).unwrap().dir, "Profile 1");
        assert!(profile_choice(b, Some("../../other-user")).is_err());
        let mut duplicate = b.clone();
        duplicate.profiles.push(profiles::ProfileInfo { dir: "Profile 2".into(), name: "Work".into() });
        assert!(profile_choice(&duplicate, Some("Work")).is_err(), "ambiguous profile names require the detected directory ID");
        let mut beta = b.clone();
        beta.name = "Microsoft Edge Beta".into();
        assert!(browser_choice(&[b.clone(), beta], "edge").is_err(), "partial browser names must not choose an arbitrary login source");
    }

    #[test]
    fn namespaces_never_collide() {
        // no slug put into a no-login name can overlap the login clone namespace
        for w in ["login-foo", "login", "-login-foo", "LOGIN-foo", "foo", "x_y-z"] {
            let plain = format!("mcp_{}", profiles::slug(w)).to_lowercase();
            assert!(!plain.starts_with("mcp-login-"), "{plain}");
            let slugged = profiles::slug(&plain);
            assert!(!slugged.to_lowercase().starts_with("mcp-login-"));
        }
    }

    #[test]
    fn dims_are_clamped() {
        assert_eq!(desk_dims(&json!({})), (1280, 800));
        assert_eq!(desk_dims(&json!({"width": 10000, "height": 10000})), (1920, 1200));
        assert_eq!(desk_dims(&json!({"width": 1, "height": 1})), (640, 400));
    }

    #[test]
    fn account_names() {
        assert!(valid_account_name("ghostdesk-agent"));
        assert!(valid_account_name("agent.2_x"));
        for bad in ["", "/tmp/x.sock", "../x", "-x", ".x", "Agent", "a b", "a/b"] {
            assert!(!valid_account_name(bad), "{bad}");
        }
    }

    #[test]
    fn app_query_normalization() {
        assert_eq!(norm_app_query("Slack.app"), "slack");
        assert_eq!(norm_app_query(" SLACK.APP/ "), "slack");
        assert_eq!(norm_app_query("Visual Studio Code"), "visual studio code");
        assert_eq!(
            outer_bundle(Path::new("/Applications/GhostDesk.app/Contents/MacOS/ghostdesk")),
            Some(PathBuf::from("/Applications/GhostDesk.app"))
        );
        assert_eq!(
            outer_bundle(Path::new("/Applications/Slack.app/Contents/Frameworks/Slack Helper.app")),
            Some(PathBuf::from("/Applications/Slack.app"))
        );
        assert_eq!(outer_bundle(Path::new("/usr/bin/true")), None);
    }

    #[test]
    fn deny_list_by_bundle_id_and_name() {
        let mk = |p: &str, b: Option<&str>| ResolvedApp {
            path: PathBuf::from(p),
            bundle_id: b.map(String::from),
            name: String::new(),
            electron: None,
            #[cfg(windows)]
            original_filename: None,
            #[cfg(windows)]
            publisher: None,
        };
        assert!(is_denied(&mk("/System/Applications/Utilities/Terminal.app", Some("com.apple.Terminal"))));
        assert!(is_denied(&mk("/x/Renamed.app", Some("COM.APPLE.SCRIPTEDITOR2"))));
        assert!(is_denied(&mk("/x/Script Editor.app", None)));
        assert!(!is_denied(&mk("/System/Applications/TextEdit.app", Some("com.apple.TextEdit"))));
        // GhostDesk itself isn't on the deny list; it's only opened as a test instance (app_target)
        if !cfg!(windows) {
            assert!(!is_denied(&mk("/x/Foo.app", Some("dev.ghostdesk.app"))));
        }
        assert!(is_ghostdesk(&mk("/x/Foo.app", Some("DEV.GHOSTDESK.APP"))));
        assert!(is_ghostdesk(&mk("/Applications/GhostDesk.app", None)));
        assert!(!is_ghostdesk(&mk("/System/Applications/TextEdit.app", Some("com.apple.TextEdit"))));
    }

    #[test]
    fn blank_workspace_check() {
        let tmp = std::env::temp_dir().join(format!("ghostdesk-ctl-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // missing and empty folders are allowed
        assert!(check_blank_workspace(&tmp, "w").is_ok());
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(check_blank_workspace(&tmp, "w").is_ok());
        // data (clone leftovers) without the marker is refused
        std::fs::write(tmp.join("Local State"), "{}").unwrap();
        assert!(check_blank_workspace(&tmp, "w").is_err());
        // a folder created without login is allowed
        std::fs::write(tmp.join(BLANK_MARKER), "").unwrap();
        assert!(check_blank_workspace(&tmp, "w").is_ok());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reservation_is_atomic_and_released() {
        let set = Mutex::new(HashSet::new());
        let reserve = |base: &str, n: u64| {
            let mut s = set.lock();
            let mut name = base.to_string();
            let mut i = 0;
            while s.contains(&reservation_key(&name)) {
                i += 1;
                name = if i == 1 { format!("{base}-{n}") } else { format!("{base}-{n}-{i}") };
            }
            let key = reservation_key(&name);
            s.insert(key.clone());
            Reservation { set: &set, key, name, keep: false }
        };
        let a = reserve("mcp_foo", 1);
        let b = reserve("mcp_FOO", 2); // names differing only in case are the same folder
        assert_eq!(a.name, "mcp_foo");
        assert_eq!(b.name, "mcp_FOO-2");
        drop(b); // failure → released
        assert!(!set.lock().contains("mcp_foo-2"));
        let key = a.keep();
        assert!(set.lock().contains(&key));
    }

    fn conn(id: u64) -> (watch::Sender<bool>, Conn) {
        let (tx, rx) = watch::channel(false);
        (tx, Conn { id, closed: rx })
    }

    #[tokio::test]
    async fn approval_cancelled_when_requester_disconnects() {
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let srv = ControlServer::new(ev_tx, egui::Context::default());
        let (closed_tx, c) = conn(1);
        let srv2 = srv.clone();
        let c2 = c.clone();
        let task = tokio::spawn(async move {
            srv2.open(&json!({ "kind": "remote", "host": "h.invalid", "dedicated_machine": true }), &c2).await
        });
        let Some(ControlEvent::Approval { detail, reply, .. }) = ev_rx.recv().await else { panic!("승인 요청이 없음") };
        assert!(detail.contains("에이전트 전용"), "dedicated 주장이 승인 문구에 보여야 함");
        assert!(!reply.is_closed());
        closed_tx.send_replace(true);
        let r = tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        assert!(r.is_err());
        assert!(reply.is_closed(), "요청자가 떠나면 GUI 가 승인 창을 치울 수 있어야 함");
        assert!(srv.reserved.lock().is_empty(), "실패한 open 은 작업공간 예약을 풀어야 함");
    }

    #[tokio::test]
    async fn desks_bound_to_connection() {
        let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
        let srv = ControlServer::new(ev_tx, egui::Context::default());
        let (_t1, a) = conn(1);
        let (_t2, b) = conn(2);
        for (id, owner, shared) in [("d-1", 1, false), ("d-2", 1, true), ("d-3", 2, false)] {
            let sh = AgentShared::new(1, id.into(), format!("mcp_{id}"), srv.ctx.clone());
            srv.reserved.lock().insert(reservation_key(&format!("mcp_{id}")));
            srv.desks.lock().insert(
                id.into(),
                DeskEntry { shared: sh, conn: owner, shared_access: shared, reservation: reservation_key(&format!("mcp_{id}")) },
            );
        }
        // other connections can't drive or close it, shared ones can
        assert!(srv.get(&json!({ "desk_id": "d-1" }), &b).err().unwrap().to_string().contains("다른 에이전트"));
        assert!(srv.close(&json!({ "desk_id": "d-1" }), &b).await.is_err());
        assert!(srv.close(&json!({ "desk_id": "d-2" }), &b).await.is_ok());
        assert!(!srv.reserved.lock().contains("mcp_d-2"));
        // screens removed in the GUI are cleaned up
        srv.desks.lock()["d-3"].shared.removed.store(true, Ordering::SeqCst);
        srv.prune_removed();
        assert!(!srv.desks.lock().contains_key("d-3"));
        assert!(!srv.reserved.lock().contains("mcp_d-3"));
        // when a connection drops, every screen it created is closed
        srv.close_conn_desks(a.id).await;
        assert!(srv.desks.lock().is_empty());
        assert!(srv.reserved.lock().is_empty());
    }

    #[test]
    fn mode_maps_to_one_set_of_options() {
        let q = apply_mode(&json!({"mode": "app_current_session", "app": "x", "attach_running": true, "kind": "browser"})).unwrap();
        assert_eq!(q, json!({"mode": "app_current_session", "app": "x", "kind": "app", "app_host": "current_session"}));
        assert_eq!(apply_mode(&json!({"mode": "web_login"})).unwrap()["use_my_login"], json!(true));
        assert_eq!(apply_mode(&json!({"mode": "session"})).unwrap()["kind"], json!("session"));
        assert!(apply_mode(&json!({"mode": "hidden"})).unwrap_err().to_string().contains("web_login"));
        // without a mode, old arguments pass through as is
        assert_eq!(apply_mode(&json!({"kind": "remote"})).unwrap(), json!({"kind": "remote"}));
        let q = apply_mode(&json!({"mode":"web_login", "browser":"Edge", "browser_profile":"Profile 1", "workspace":"travel", "refresh_login":true})).unwrap();
        assert_eq!(q["browser_profile"], "Profile 1");
        assert_eq!(q["workspace"], "travel");
        assert_eq!(q["refresh_login"], true);
        for invalid in [json!({"browser":4}), json!({"browser_profile":["Default"]}), json!({"workspace":false}), json!({"refresh_login":"true"})] {
            assert!(apply_mode(&invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn kinds_gated_by_platform() {
        // macOS accepts every kind (unchanged behavior)
        for k in ["browser", "remote", "app", "session", "nope"] {
            if caps::SESSION || k != "session" {
                assert!(unsupported_kind(k).is_none(), "{k}");
            }
        }
        if cfg!(windows) {
            assert!(guide(DeskKind::Session).contains("자식 세션"));
            assert_eq!(kind_name(DeskKind::Native), "app(windows)");
            assert!(guide(DeskKind::Native).contains("Windows"));
        }
        if cfg!(target_os = "macos") {
            assert_eq!(kind_name(DeskKind::Native), "app(macos)");
            assert_eq!(NATIVE_APP_WORD, "macOS 앱");
            assert_eq!(APP_ID_LABEL, "번들 id");
        }
    }

    #[cfg(windows)]
    fn win_app(p: &str, orig: Option<&str>, name: &str) -> ResolvedApp {
        ResolvedApp {
            path: PathBuf::from(p),
            bundle_id: orig.map(String::from),
            name: name.into(),
            electron: None,
            original_filename: orig.map(String::from),
            publisher: None,
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_deny_policy() {
        let sys = win_policy::system_root_for_test();
        let s = |rel: &str| format!(r"{sys}\{rel}");
        // shells, terminals, interpreters, password managers, GhostDesk: by executable name
        for p in [
            r"C:\Program Files\Git\git-bash.exe",
            r"C:\Program Files\PowerShell\7\pwsh.exe",
            r"C:\Tools\ubuntu2204.exe",
            r"C:\Users\a\AppData\Local\Programs\KeePassXC\KeePassXC.exe",
            r"D:\x\ProcExp64.exe",
            r"C:\Program Files\Python312\python.exe",
            r"C:\x\GhostDesk-old.exe",
            r"C:\Program Files\Microsoft VS Code\bin\code-tunnel.exe",
        ] {
            assert!(is_denied(&win_app(p, None, "")), "{p}");
        }
        // renamed copies: by original file name
        for o in ["Cmd.Exe", "pwsh.dll", "PowerShell.EXE.MUI", "regedit.exe", "PAD.Console.Host.exe"] {
            assert!(is_denied(&win_app(r"C:\Users\a\Desktop\tool.exe", Some(o), "")), "{o}");
        }
        // by display name
        for n in ["Registry Editor", "명령 프롬프트", "Windows PowerShell", "Task Manager", "Bitwarden"] {
            assert!(is_denied(&win_app(r"C:\Users\a\Desktop\tool.exe", None, n)), "{n}");
        }
        // system folders are blocked by default, with a few harmless tools directly under them allowed
        assert!(is_denied(&win_app(&s(r"System32\dxdiag.exe"), None, "")));
        assert!(is_denied(&win_app(&s(r"System32\WindowsPowerShell\v1.0\x.exe"), None, "")));
        assert!(is_denied(&win_app(&s(r"System32\sub\notepad.exe"), None, "")));
        assert!(is_denied(&win_app(&s(r"SystemApps\x\y.exe"), None, "")));
        assert!(!is_denied(&win_app(&s(r"System32\notepad.exe"), Some("NOTEPAD.EXE"), "Notepad")));
        assert!(!is_denied(&win_app(&s("notepad.exe"), None, "Notepad")));
        assert!(!is_denied(&win_app(&s(r"SysWOW64\charmap.exe"), None, "")));
        assert!(!is_denied(&win_app(&s(r"System32\calc.exe").to_uppercase(), None, "")));
        // network share and device paths
        assert!(is_denied(&win_app(r"\\localhost\C$\Windows\System32\dxdiag.exe", None, "")));
        assert!(is_denied(&win_app(r"\\?\UNC\srv\share\app.exe", None, "")));
        assert!(is_denied(&win_app(r"\\?\GLOBALROOT\Device\x.exe", None, "")));
        // executables inside blocked packages
        assert!(is_denied(&win_app(
            r"C:\Program Files\WindowsApps\Microsoft.WindowsTerminal_1.21.10351.0_x64__8wekyb3d8bbwe\Other.exe",
            None,
            ""
        )));
        assert!(is_denied(&win_app(r"C:\Users\a\AppData\Local\Microsoft\WindowsApps\Microsoft.PowerShell_8wekyb3d8bbwe\x.exe", None, "")));
        // GhostDesk itself (the running file)
        let me = std::env::current_exe().unwrap();
        assert!(is_denied(&win_app(&me.to_string_lossy(), None, "")));
        // regular apps pass
        for (p, n) in [
            (r"C:\Program Files\Microsoft VS Code\Code.exe", "Visual Studio Code"),
            (r"C:\Program Files\Google\Chrome\Application\chrome.exe", "Google Chrome"),
            (r"C:\Program Files\WindowsApps\Microsoft.WindowsNotepad_11.2410.21.0_x64__8wekyb3d8bbwe\Notepad\Notepad.exe", "Notepad"),
            (r"C:\Users\a\AppData\Local\Kakao\KakaoTalk\KakaoTalk.exe", "KakaoTalk"),
            (r"\\?\C:\Program Files\Slack\slack.exe", "Slack"),
        ] {
            assert!(!is_denied(&win_app(p, Some("electron.exe"), n)), "{p}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_query_normalization_and_early_deny() {
        assert_eq!(win_policy::norm_query(r#""C:\Windows\System32\CMD.EXE""#), "cmd");
        assert_eq!(win_policy::norm_query("Visual Studio Code"), "visual studio code");
        assert_eq!(win_policy::norm_query(r"C:\Users\a\Desktop\Slack.lnk"), "slack");
        assert_eq!(win_policy::norm_query(r"C:\Program Files\Slack\"), "slack");
        for q in [
            "cmd",
            "CMD.exe",
            "powershell",
            "PowerShell_ISE",
            "pwsh",
            "wt",
            "Windows Terminal",
            "명령 프롬프트",
            "regedit",
            "taskmgr",
            "explorer.exe",
            r"C:\Windows\System32\cmd.exe",
            r"%LOCALAPPDATA%\Microsoft\WindowsApps\wt.exe",
            "wsl",
            "ubuntu",
            "KeePassXC",
            "ghostdesk",
        ] {
            assert!(win_policy::denied_query(q), "{q}");
            let e = resolve_external_app(q).err().unwrap_or_else(|| panic!("{q} 가 통과함")).to_string();
            assert!(e.contains("보안상"), "{q}: {e}");
        }
        for q in ["notepad", "Visual Studio Code", "chrome", "KakaoTalk", "Slack", "Microsoft Edge"] {
            assert!(!win_policy::denied_query(q), "{q}");
        }
        let root = std::env::var("SystemRoot").unwrap();
        assert_eq!(win_policy::expand_path(r"%SystemRoot%\x.exe"), PathBuf::from(format!(r"{root}\x.exe")));
        assert_eq!(win_policy::expand_path(r"%GD_NO_SUCH_VAR_X%\a"), PathBuf::from(r"%GD_NO_SUCH_VAR_X%\a"));
        assert_eq!(win_policy::expand_path("100%"), PathBuf::from("100%"));
        assert_eq!(win_policy::expand_path(r"~\a.exe"), dirs::home_dir().unwrap().join("a.exe"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_resolves_exe_paths() {
        let sys = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()));
        let tmp = std::env::temp_dir().join(format!("ghostdesk-ctl-win-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // a renamed copy of cmd.exe: the path resolves, but it's blocked by its original file name
        // (VERSIONINFO)
        let copy = tmp.join("harmless-tool.exe");
        std::fs::copy(sys.join(r"System32\cmd.exe"), &copy).unwrap();
        let r = resolve_external_app(&copy.to_string_lossy()).unwrap();
        assert_eq!(r.path, crate::platform::paths::canonicalize(&copy).unwrap());
        assert!(!r.path.to_string_lossy().starts_with(r"\\?\"), "{:?}", r.path);
        assert!(r.original_filename.as_deref().unwrap_or("").to_lowercase().starts_with("cmd"), "{:?}", r.original_filename);
        assert!(r.electron.is_none());
        assert!(is_denied(&r), "이름을 바꾼 cmd.exe 는 차단되어야 함");
        // non-.exe files, missing files and empty values aren't opened
        let bat = tmp.join("x.bat");
        std::fs::write(&bat, "@echo off\r\n").unwrap();
        assert!(resolve_external_app(&bat.to_string_lossy()).is_err());
        assert!(resolve_external_app(&tmp.join("missing.exe").to_string_lossy()).is_err());
        assert!(resolve_external_app("  ").is_err());
        assert!(resolve_external_app("ghostdesk-no-such-app-xyz").is_err());
        // allowed tools directly under the system folder (only if present)
        let np = sys.join("notepad.exe");
        if np.is_file() {
            for q in [np.to_string_lossy().into_owned(), r"%SystemRoot%\notepad.exe".to_string(), format!("\"{}\"", np.display())] {
                let r = resolve_external_app(&q).unwrap();
                assert!(!is_denied(&r), "{q}: {:?}", r.path);
                // the name is the VERSIONINFO description ("Notepad" / "메모장" depending on the Windows display
                // language)
                assert!(!r.name.is_empty(), "{q}");
                assert!(r.original_filename.as_deref().unwrap_or("").to_lowercase().starts_with("notepad"), "{q}");
            }
        }
        // other tools in the system folder are blocked
        let dx = sys.join(r"System32\dxdiag.exe");
        if dx.is_file() {
            assert!(is_denied(&resolve_external_app(&dx.to_string_lossy()).unwrap()));
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn running_electron_target_requires_attach_approval() {
        let tmp = std::env::temp_dir().join(format!("ghostdesk-ctl-electron-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("resources/app")).unwrap();
        let exe = tmp.join("example.exe");
        std::fs::copy(PathBuf::from(std::env::var("SystemRoot").unwrap()).join(r"System32\charmap.exe"), &exe).unwrap();
        std::fs::write(tmp.join("icudtl.dat"), b"fixture").unwrap();
        std::fs::write(tmp.join("resources/app/package.json"), br#"{"name":"test-electron","main":"main.js"}"#).unwrap();
        assert!(resolve_external_app(&exe.to_string_lossy()).unwrap().electron.is_some());
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let srv = ControlServer::new(ev_tx, egui::Context::default());
        let (_t, c) = conn(1);
        let task = tokio::spawn(async move {
            srv.app_target(&exe.to_string_lossy(), &json!({"attach_running":true}), false, "test", "test", &c).await
        });
        let event = tokio::time::timeout(Duration::from_secs(10), ev_rx.recv()).await.unwrap();
        let Some(ControlEvent::Approval { title, reply, .. }) = event else { panic!("실행 중인 앱 승인 요청 없음") };
        assert_eq!(title, "실행 중인 내 앱 사용 요청");
        reply.send(true).unwrap();
        let target = task.await.unwrap().unwrap();
        let DeskTarget::Native(options) = target else { panic!("실행 중인 앱에 연결해야 함") };
        assert!(options.attach_consent && !options.new_instance);
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn current_session_target_requires_original_data_approval() {
        let exe = PathBuf::from(std::env::var("SystemRoot").unwrap()).join(r"System32\charmap.exe");
        if !exe.is_file() { return; }
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let srv = ControlServer::new(ev_tx, egui::Context::default());
        let (_t, c) = conn(1);
        let app = exe.display().to_string();
        let task = tokio::spawn(async move {
            srv.app_target(&app, &json!({"app_host":"current_session"}), false, "test", "test", &c).await
        });
        let event = tokio::time::timeout(Duration::from_secs(10), ev_rx.recv()).await.unwrap();
        let Some(ControlEvent::Approval { title, detail, reply }) = event else { panic!("승인 요청 없음") };
        assert_eq!(title, "현재 계정 로그인 사용 요청");
        assert!(detail.contains("원본 프로필") && detail.contains("실제 계정에 반영"), "{detail}");
        reply.send(true).unwrap();
        let target = task.await.unwrap().unwrap();
        let DeskTarget::Native(options) = target else { panic!("복제 백엔드가 아닌 네이티브 대상이어야 함") };
        assert_eq!(options.host, crate::native::NativeHost::CurrentSession);
        assert!(options.new_instance && !options.attach_consent);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn current_session_invalid_options_fail_before_approval() {
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let srv = ControlServer::new(ev_tx, egui::Context::default());
        let (_t, c) = conn(1);
        for p in [
            json!({"kind":"app", "app":"ghostdesk-no-such-app", "app_host":"typo"}),
            json!({"kind":"app", "app":"ghostdesk-no-such-app", "app_host":"current_session", "attach_running":true}),
        ] {
            let error = srv.open(&p, &c).await.err().unwrap().to_string();
            assert!(error.contains("app_host") || error.contains("current_session"), "{error}");
            assert!(ev_rx.try_recv().is_err());
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_gates_before_approval() {
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let srv = ControlServer::new(ev_tx, egui::Context::default());
        let (_t, c) = conn(1);
        for (p, want) in [
            (json!({ "kind": "app", "app": "notepad", "virtual_display": true }), "virtual_display"),
            (json!({ "kind": "app", "app": "cmd" }), "보안상"),
            (json!({ "kind": "app", "app": "powershell.exe", "use_my_login": true }), "보안상"),
            (json!({ "kind": "app", "app": r"C:\Windows\System32\cmd.exe", "attach_running": true }), "보안상"),
            (json!({ "kind": "app", "app": r"C:\Windows\System32\dxdiag.exe" }), "보안상"),
        ] {
            let e = srv.open(&p, &c).await.unwrap_err().to_string();
            assert!(e.contains(want), "{p}: {e}");
            assert!(ev_rx.try_recv().is_err(), "승인 창을 띄우기 전에 거절해야 함: {p}");
        }
        assert!(srv.reserved.lock().is_empty());
        let sys = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()));
        // targets the local app backend refuses at launch (Win11's Store launcher calc, the notepad
        // execution alias, console programs) are refused with that reason before asking for approval
        let console = std::env::temp_dir().join(format!("ghostdesk-ctl-console-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&console);
        std::fs::create_dir_all(&console).unwrap();
        let console_exe = console.join("plain-console-tool.exe");
        std::fs::copy(sys.join(r"System32\where.exe"), &console_exe).unwrap();
        for exe in [sys.join(r"System32\calc.exe"), sys.join("notepad.exe"), sys.join(r"System32\notepad.exe"), console_exe.clone()] {
            if !exe.is_file() {
                continue;
            }
            let canonical = crate::platform::paths::canonicalize(&exe).unwrap();
            let Err(why) = crate::native::preflight_app(&canonical) else { continue };
            let p = json!({ "kind": "app", "app": exe.to_string_lossy() });
            let e = srv.open(&p, &c).await.unwrap_err().to_string();
            // compare with the same canonical path as the control server (TEMP may be an 8.3 short path)
            assert_eq!(e.to_lowercase(), why.to_string().to_lowercase(), "{p}");
            assert!(ev_rx.try_recv().is_err(), "승인 창을 띄우기 전에 거절해야 함: {p}");
            let p = json!({ "kind": "app", "app": exe.to_string_lossy(), "attach_running": true });
            assert!(srv.open(&p, &c).await.is_err());
            assert!(ev_rx.try_recv().is_err(), "승인 창을 띄우기 전에 거절해야 함: {p}");
        }
        assert!(crate::native::preflight_app(&console_exe).unwrap_err().to_string().contains("콘솔"));
        let _ = std::fs::remove_dir_all(&console);
        assert!(srv.reserved.lock().is_empty());
        // allowed, runnable apps ask for approval (a refusal runs nothing). Execution aliases like Win11's
        // notepad were already refused above without approval, so only apps that run as is are checked.
        let runnable = [sys.join("notepad.exe"), sys.join(r"System32\charmap.exe")]
            .into_iter()
            .find(|p| p.is_file() && crate::native::preflight_app(p).is_ok());
        if let Some(exe) = runnable {
            let stem = exe.file_name().unwrap().to_string_lossy().to_lowercase();
            let (srv2, c2) = (srv.clone(), c.clone());
            let p = json!({ "kind": "app", "app": exe.to_string_lossy() });
            let task = tokio::spawn(async move { srv2.open(&p, &c2).await });
            let Some(ControlEvent::Approval { title, detail, reply }) = ev_rx.recv().await else { panic!("승인 요청이 없음") };
            assert_eq!(title, "내 앱 실행 요청");
            assert!(detail.contains("Windows 앱") && detail.to_lowercase().contains(&format!(r"\{stem}")), "{detail}");
            assert!(!detail.contains("macOS") && !detail.contains("번들"), "{detail}");
            let _ = reply.send(false);
            let e = task.await.unwrap().unwrap_err().to_string();
            assert!(e.contains("거부"), "{e}");
            assert!(srv.reserved.lock().is_empty());
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_refuses_network_and_device_paths_before_touching_them() {
        for s in [
            r"\\ghostdesk-test.invalid\share\tool.exe",
            "//ghostdesk-test.invalid/share/tool.exe",
            r"\/ghostdesk-test.invalid\share\tool.exe",
            r"\\?\UNC\ghostdesk-test.invalid\share\tool.exe",
            r"\\?\GLOBALROOT\Device\Mup\ghostdesk-test.invalid\share\tool.exe",
            r"\\.\pipe\x",
            r"\??\UNC\ghostdesk-test.invalid\share\tool.exe",
            r#""\\ghostdesk-test.invalid\share\tool.exe""#,
            r"\\?\Volume{00000000-0000-0000-0000-000000000000}\x.exe",
        ] {
            assert!(win_policy::network_or_device(s), "{s}");
            let e = resolve_external_app(s).err().unwrap_or_else(|| panic!("{s} 가 통과함")).to_string();
            assert!(e.contains("네트워크 공유"), "{s}: {e}");
        }
        for s in [r"C:\Program Files\x.exe", r"\\?\C:\Program Files\x.exe", "notepad", r"%SystemRoot%\notepad.exe", r"\Windows\x.exe", "C:x.exe"] {
            assert!(!win_policy::network_or_device(s), "{s}");
        }
        // refuse when the %VAR%-expanded result is a network path (even for a name that doesn't look like
        // a path)
        std::env::set_var("GD_CTL_TEST_UNC_TARGET", r"\\ghostdesk-test.invalid\share\tool.exe");
        let e = resolve_external_app("%GD_CTL_TEST_UNC_TARGET%").err().unwrap().to_string();
        assert!(e.contains("네트워크 공유"), "{e}");
        std::env::set_var("GD_CTL_TEST_UNC_ROOT", "//ghostdesk-test.invalid/share");
        let e = resolve_external_app(r"%GD_CTL_TEST_UNC_ROOT%\tool.exe").err().unwrap().to_string();
        assert!(e.contains("네트워크 공유"), "{e}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_refuses_percent_in_resolved_path() {
        // the local app backend expands %VAR% again at launch, so paths where the checked file and the
        // launched file could differ are refused
        let sys = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()));
        let tmp = std::env::temp_dir().join(format!("ghostdesk-ctl-pct-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        for name in ["%USERNAME%", "%GD_CTL_NO_SUCH_VAR%"] {
            let dir = tmp.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::copy(sys.join(r"System32\where.exe"), dir.join("tool.exe")).unwrap();
        }
        // an undefined variable name stays as is, leaving a % in the canonical path
        let e = resolve_external_app(&format!(r"{}\%GD_CTL_NO_SUCH_VAR%\tool.exe", tmp.display())).err().unwrap().to_string();
        assert!(e.contains("'%'"), "{e}");
        // one expansion still leaves '%USERNAME%' (8.3 short names and shortcut targets can cause this
        // too): the file checked here is ...\%USERNAME%\tool.exe, but at launch it becomes
        // ...\<user name>\tool.exe
        std::env::set_var("GD_CTL_PCT_DIR", "%USERNAME%");
        let e = resolve_external_app(&format!(r"{}\%GD_CTL_PCT_DIR%\tool.exe", tmp.display())).err().unwrap().to_string();
        assert!(e.contains("'%'"), "{e}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn remote_to_this_computer_is_checked_before_approval() {
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let srv = ControlServer::new(ev_tx, egui::Context::default());
        let (_t, c) = conn(1);
        // without a dedicated machine claim, refuse with the same message without asking
        for host in ["127.0.0.1", "localhost", "::1"] {
            let e = srv.open(&json!({ "kind": "remote", "host": host, "port": 5999 }), &c).await.unwrap_err().to_string();
            assert_eq!(e, agent::local_vnc_refusal(host, 5999));
            assert!(ev_rx.try_recv().is_err(), "승인 창을 띄우기 전에 거절해야 함: {host}");
        }
        // with a dedicated machine claim, ask with a prominent "this computer itself" warning
        let (srv2, c2) = (srv.clone(), c.clone());
        let task = tokio::spawn(async move {
            srv2.open(&json!({ "kind": "remote", "host": "127.0.0.1", "port": 5999, "dedicated_machine": true }), &c2).await
        });
        let Some(ControlEvent::Approval { detail, reply, .. }) = ev_rx.recv().await else { panic!("승인 요청이 없음") };
        assert!(detail.contains(&format!("127.0.0.1:5999 은(는) {} 자신의 VNC 서버", text::THIS_COMPUTER)), "{detail}");
        assert!(detail.contains("실제 화면·마우스·키보드"), "{detail}");
        assert!(detail.contains("Mac 이라면"), "Mac 화면 공유 경고는 대상이 Mac 일 때의 이야기로만: {detail}");
        assert!(!detail.contains("에이전트 전용 Mac/VM"), "{detail}");
        let _ = reply.send(false);
        assert!(task.await.unwrap().unwrap_err().to_string().contains("거부"));
        // targets other than this computer are asked about without the "this computer itself" warning
        let (srv2, c2) = (srv.clone(), c.clone());
        let task = tokio::spawn(async move {
            srv2.open(&json!({ "kind": "remote", "host": "192.0.2.50", "port": 5999, "dedicated_machine": true }), &c2).await
        });
        let Some(ControlEvent::Approval { detail, reply, .. }) = ev_rx.recv().await else { panic!("승인 요청이 없음") };
        assert!(!detail.contains("자신의 VNC 서버입니다"), "{detail}");
        assert!(detail.contains("에이전트 전용"), "{detail}");
        let _ = reply.send(false);
        assert!(task.await.unwrap().is_err());
        // names aren't resolved before approval: only addresses and localhost count as this computer;
        // other names are checked against the address actually connected to at connect time (and refused
        // then if the approval dialog didn't show the "this computer itself" warning)
        for h in ["127.0.0.2", "0.0.0.0", "[::1]", " LocalHost. ", "agent.localhost"] {
            assert!(remote_is_this_computer(h, 5900), "{h}");
        }
        for h in ["192.0.2.50", "ghostdesk-test.invalid", "localhost.ghostdesk-test.invalid", ""] {
            assert!(!remote_is_this_computer(h, 5900), "{h}");
        }
        // once a person approved after seeing the "this computer itself" warning, connecting to this
        // computer's VNC server is attempted (the fake server fails the handshake, but it isn't refused
        // for being this computer)
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut s, _)) = l.accept().await {
                let _ = s.write_all(b"RFB 003.008\n").await;
                let mut v = [0u8; 12];
                if s.read_exact(&mut v).await.is_ok() {
                    let _ = s.write_all(&[1, 1]).await;
                }
            }
        });
        let (srv2, c2) = (srv.clone(), c.clone());
        let task = tokio::spawn(async move {
            srv2.open(&json!({ "kind": "remote", "host": "127.0.0.1", "port": port, "dedicated_machine": true }), &c2).await
        });
        let Some(ControlEvent::Approval { detail, reply, .. }) = ev_rx.recv().await else { panic!("승인 요청이 없음") };
        assert!(detail.contains("자신의 VNC 서버입니다"), "{detail}");
        let _ = reply.send(true);
        let e = task.await.unwrap().unwrap_err().to_string();
        assert!(!e.contains("자신에서 실행 중인") && !e.contains("실제로 접속한 주소는") && !e.contains("거부"), "{e}");
        assert!(srv.reserved.lock().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn resolves_and_denies_system_apps() {
        if Path::new("/System/Applications/TextEdit.app").is_dir() {
            for q in ["TextEdit", "textedit.app", "/System/Applications/TextEdit.app/", "/System//Applications/TextEdit.app"] {
                let r = resolve_external_app(q).unwrap();
                assert_eq!(r.path, std::fs::canonicalize("/System/Applications/TextEdit.app").unwrap(), "{q}");
                assert!(r.electron.is_none());
                assert!(!is_denied(&r));
            }
        }
        if Path::new("/System/Applications/Utilities/Terminal.app").is_dir() {
            for q in ["Terminal", "terminal.APP", "com.apple.Terminal"] {
                let r = resolve_external_app(q).unwrap();
                assert!(is_denied(&r), "{q}");
            }
        }
    }

}
