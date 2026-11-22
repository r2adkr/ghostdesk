//! Isolated browser workspace.
//!
//! Launches a separate Chromium process (headless by default) on a cloned profile and drives all
//! input and screen capture through CDP alone. The OS mouse cursor, keyboard focus and the user's
//! own browser windows are never touched.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::{
    cdp::{Cdp, CdpEvent},
    keys::{self, MOD_ALT, MOD_CTRL, MOD_META},
};

pub type FrameSink = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Features to disable. (`--disable-features` only honors the last occurrence, so they're passed
/// together.)
///
/// Windows: turns off occlusion tracking so that in windowed mode, painting doesn't stop while
/// other windows cover it.
#[cfg(not(windows))]
const DISABLE_FEATURES: &str = "--disable-features=Translate,MediaRouter,DialMediaRouteProvider,OptimizationHints,AutofillServerCommunication,CertificateTransparencyComponentUpdater";
#[cfg(windows)]
const DISABLE_FEATURES: &str =
    "--disable-features=Translate,MediaRouter,DialMediaRouteProvider,OptimizationHints,AutofillServerCommunication,CertificateTransparencyComponentUpdater,CalculateNativeWinOcclusion";

#[derive(Clone, Debug)]
pub struct LaunchOptions {
    pub exe: PathBuf,
    pub user_data_dir: PathBuf,
    pub profile_dir: Option<String>,
    pub width: u32,
    pub height: u32,
    pub headless: bool,
    pub start_url: String,
}

#[derive(Clone, Debug)]
pub struct TabInfo {
    pub target_id: String,
    pub title: String,
    pub url: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    fn name(self) -> &'static str {
        match self {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
        }
    }
    fn bit(self) -> i64 {
        match self {
            MouseButton::Left => 1,
            MouseButton::Right => 2,
            MouseButton::Middle => 4,
        }
    }
}

#[derive(Default)]
struct State {
    tabs: Vec<TabInfo>,
    active: Option<String>,
    sessions: HashMap<String, String>,
    mouse: (f64, f64),
    buttons: i64,
    /// Events to tell the agent about (new tab opened, …).
    notes: Vec<String>,
    /// (Drag interception) The intercepted HTML5 drag, finished inside the page on button release.
    drag: Option<DragState>,
    /// (Drag interception) Whether the mouse moved with the left button down (a drag may have started).
    moved_pressed: bool,
    /// (Drag interception) Sessions where Input.setInterceptDrags couldn't be enabled (old Electron, …).
    no_intercept: HashSet<String>,
    dialogs: HashMap<String, Value>,
    next_dialog_id: u64,
    /// "session/frame" of every frame that is loading right now (Page.frameStartedLoading … frameStoppedLoading).
    loading: HashSet<String>,
    /// Counts navigations (a frame starting to load, or the address changing within the page), so a caller can
    /// tell whether one began after a given moment.
    nav_seq: u64,
}

/// An intercepted HTML5 drag (Input.dragIntercepted).
#[derive(Clone, Debug)]
struct DragState {
    session: String,
    /// Input.DragData (passed to Input.dispatchDragEvent as is).
    data: Value,
    /// Whether dragEnter was already sent.
    entered: bool,
}

pub struct BrowserWorkspace {
    cdp: Arc<Cdp>,
    child: Mutex<Option<Child>>,
    pub width: u32,
    pub height: u32,
    user_agent: Option<String>,
    /// Electron app mode (see ConnectOptions::app_mode).
    app_mode: bool,
    /// Intercepts HTML5 drags and finishes them inside the page instead of handing them to the OS
    /// (see ConnectOptions::intercept_drags).
    intercept_drags: bool,
    state: Mutex<State>,
    ready: AtomicBool,
    closed: AtomicBool,
    sink: FrameSink,
    dialog_changed: tokio::sync::Notify,
}

/// Options for attaching to the CDP endpoint of an already running Chromium/Electron.
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    pub width: u32,
    pub height: u32,
    /// Electron app mode: no new tabs (no Target.createTarget), no Target.activateTarget (it brings
    /// windows to the front), and devtools/extension pages are left out of the views.
    pub app_mode: bool,
    /// Whether to send the headless UA with "HeadlessChrome" replaced by "Chrome" (headless browser
    /// only).
    pub fix_headless_ua: bool,
    /// URL to navigate to right after connecting (empty = don't navigate).
    pub start_url: String,
    /// How long to wait for a page target (window) to appear (only meaningful in app_mode).
    pub page_wait: Duration,
    /// HTML5 drag interception: enables Input.setInterceptDrags on every attached page session and
    /// finishes drags started by the agent inside the page with Input.dispatchDragEvent (dragEnter,
    /// dragOver, drop).
    ///
    /// Without it, a windowed (headful) app makes Chromium start an OS drag and drop (Windows: OLE
    /// DoDragDrop), and the drop lands on whatever window is under the user's real cursor. (Windows
    /// Electron workspaces only.)
    pub intercept_drags: bool,
}

/// Whether the target counts as a view (tab/window). devtools, extensions, service workers, …
/// are excluded.
pub fn is_view_target(info: &Value, app_mode: bool) -> bool {
    if info["type"] != "page" {
        return false;
    }
    if !app_mode {
        return true;
    }
    let url = info["url"].as_str().unwrap_or_default();
    !(url.starts_with("devtools://")
        || url.starts_with("chrome-extension://")
        || url.starts_with("chrome://")
        || url.starts_with("chrome-error://"))
}

impl BrowserWorkspace {
    pub async fn launch(opts: LaunchOptions, sink: FrameSink) -> Result<Arc<Self>> {
        // check the start URL with the same rules as navigate (before launching the browser)
        #[cfg(windows)]
        if !opts.start_url.trim().is_empty() {
            if let Some(why) = remote_file_refusal(&opts.start_url, &normalize_url(&opts.start_url)) {
                bail!("{why}");
            }
        }
        let (child, ws_url) = Self::spawn(&opts).await?;
        Self::connect(
            &ws_url,
            Some(child),
            ConnectOptions {
                width: opts.width,
                height: opts.height,
                app_mode: false,
                fix_headless_ua: opts.headless,
                start_url: opts.start_url.clone(),
                page_wait: Duration::from_secs(0),
                intercept_drags: false,
            },
            sink,
        )
        .await
    }

    /// Launches Chromium and reads the browser websocket URL from DevToolsActivePort.
    pub async fn spawn(opts: &LaunchOptions) -> Result<(Child, String)> {
        std::fs::create_dir_all(&opts.user_data_dir)?;
        let port_file = opts.user_data_dir.join("DevToolsActivePort");
        let _ = std::fs::remove_file(&port_file);
        let log_path = opts.user_data_dir.join("ghostdesk-browser.log");
        let log = std::fs::File::create(&log_path).ok();

        let mut cmd = Command::new(&opts.exe);
        cmd.arg(format!("--user-data-dir={}", opts.user_data_dir.display()))
            .arg("--remote-debugging-port=0")
            // no --remote-allow-origins: the CDP client sends no Origin header, and allowing every origin would let
            // web pages in the user's own browser attach to this one (and its cloned logins)
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg(DISABLE_FEATURES)
            .arg("--disable-background-timer-throttling")
            .arg("--disable-renderer-backgrounding")
            .arg("--disable-backgrounding-occluded-windows")
            .arg("--mute-audio")
            // Background services an agent never uses (sync, component updates, crash reports,
            // phishing/ping/metrics uploads): about a fifth less memory per browser, measured on
            // an idle headless Chrome.
            .args([
                "--disable-sync",
                "--disable-component-update",
                "--disable-default-apps",
                "--no-pings",
                "--disable-breakpad",
                "--disable-client-side-phishing-detection",
                "--disable-domain-reliability",
                "--metrics-recording-only",
            ])
            .arg(format!("--window-size={},{}", opts.width, opts.height));
        if let Some(p) = &opts.profile_dir {
            cmd.arg(format!("--profile-directory={p}"));
        }
        if opts.headless {
            cmd.arg("--headless=new");
        }
        // Windows: even if the cloned profile has "continue running background apps when closed" on,
        // don't create a tray icon or linger after Browser.close
        #[cfg(windows)]
        cmd.arg("--disable-background-mode");
        // CI runners have no GPU: a software-only browser starts faster and can't hang in GPU setup
        #[cfg(test)]
        cmd.arg("--disable-gpu");
        cmd.arg("about:blank");
        cmd.stdin(Stdio::null()).stdout(Stdio::null());
        match log {
            Some(f) => cmd.stderr(f),
            None => cmd.stderr(Stdio::null()),
        };
        // launch in its own process group (Unix: pgid, Windows: Job object) so that cancelling during
        // startup also cleans up renderer and GPU helpers. The Windows Job is KILL_ON_JOB_CLOSE, so no
        // invisible Chromium is left behind even if GhostDesk crashes.
        // The guard holds it until startup finishes and the workspace takes ownership: if this future is
        // aborted (agent removed) or errors out, Drop kills Chromium.
        // Tests launch real browsers from many threads at once; on a small CI runner that made startup
        // exceed the 40 s limit, so only one starts at a time (held until its port file appears).
        #[cfg(test)]
        let _startup_gate = tests_support::STARTUP_GATE.lock().await;
        let mut guard = ChildGuard::new(
            crate::platform::proc::spawn_group(&mut cmd)
                .with_context(|| format!("브라우저 실행 실패: {}", opts.exe.display()))?,
        );
        #[cfg(test)]
        tests_support::record_spawn(&opts.user_data_dir, guard.get().id());
        let child = guard.get();

        // wait for the DevToolsActivePort file to appear
        let start = Instant::now();
        let ws_url = loop {
            if let Some(u) = read_devtools_port_file(&port_file) {
                break u;
            }
            if let Ok(Some(status)) = child.try_wait() {
                // Windows: some browsers (Naver Whale, …) ship a launcher executable that starts the real browser
                // as a child and exits right away. The child stays in the same Job, so keep waiting while the Job
                // has live processes.
                #[cfg(windows)]
                let handed_off = launcher_handed_off(child);
                #[cfg(not(windows))]
                let handed_off = false;
                if !handed_off {
                    let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
                    let tail: String = tail.lines().rev().take(8).collect::<Vec<_>>().join("\n");
                    bail!(
                        "브라우저가 바로 종료되었습니다 ({status}). 같은 작업공간을 다른 곳에서 쓰고 있지 않은지 확인하세요.\n{tail}"
                    );
                }
            }
            if start.elapsed() > Duration::from_secs(40) {
                // the guard kills and reaps the process group on drop; the browser's own last words say why
                let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
                let mut lines: Vec<&str> = tail.lines().rev().take(8).collect();
                lines.reverse();
                let tail = lines.join("\n");
                #[cfg(windows)]
                bail!(
                    "브라우저 디버깅 포트를 40초 안에 찾지 못했습니다. 회사·학교 관리 정책(RemoteDebuggingAllowed, \
                     DeveloperToolsAvailability)으로 원격 디버깅이 막혀 있으면 이렇게 될 수 있습니다.\n{tail}"
                );
                #[cfg(not(windows))]
                bail!("브라우저 디버깅 포트를 40초 안에 찾지 못했습니다\n{tail}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        Ok((guard.release(), ws_url))
    }

    /// Attaches to the browser websocket (`ws://…/devtools/browser/…`) of a running Chromium/Electron.
    ///
    /// If `child` is given it's cleaned up on shutdown (None when the process is managed elsewhere,
    /// as with Electron).
    pub async fn connect(ws_url: &str, child: Option<Child>, opts: ConnectOptions, sink: FrameSink) -> Result<Arc<Self>> {
        let cdp = Cdp::connect(ws_url).await?;
        let user_agent = if opts.fix_headless_ua && !opts.app_mode {
            cdp.call("Browser.getVersion", json!({}), None)
                .await
                .ok()
                .and_then(|v| v["userAgent"].as_str().map(|s| s.replace("HeadlessChrome", "Chrome")))
        } else {
            None
        };

        let ws = Arc::new(BrowserWorkspace {
            cdp: cdp.clone(),
            child: Mutex::new(child),
            width: opts.width,
            height: opts.height,
            user_agent,
            app_mode: opts.app_mode,
            intercept_drags: opts.intercept_drags,
            state: Mutex::new(State {
                mouse: (opts.width as f64 / 2.0, opts.height as f64 / 2.0),
                ..Default::default()
            }),
            ready: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            sink,
            dialog_changed: tokio::sync::Notify::new(),
        });

        // if we bail out (cancelled or errored) before ready, event_loop still holds the Arc and Drop
        // won't run, so kill the Chromium we launched ourselves (no-op without a child)
        let mut pre_ready = CloseOnDrop(Some(ws.clone()));
        let rx = cdp.subscribe();
        tokio::spawn(ws.clone().event_loop(rx));

        cdp.call("Target.setDiscoverTargets", json!({ "discover": true }), None)
            .await?;
        let start = Instant::now();
        let first = loop {
            let targets = cdp.call("Target.getTargets", json!({}), None).await?;
            let mut first: Option<String> = None;
            let mut first_nonblank: Option<String> = None;
            if let Some(arr) = targets["targetInfos"].as_array() {
                let mut st = ws.state.lock();
                for t in arr.iter().filter(|t| is_view_target(t, opts.app_mode)) {
                    let id = t["targetId"].as_str().unwrap_or_default().to_string();
                    let url = t["url"].as_str().unwrap_or_default();
                    if !st.tabs.iter().any(|x| x.target_id == id) {
                        st.tabs.push(TabInfo {
                            target_id: id.clone(),
                            title: t["title"].as_str().unwrap_or_default().into(),
                            url: url.into(),
                        });
                    }
                    if url != "about:blank" && !url.is_empty() {
                        first_nonblank.get_or_insert(id.clone());
                    }
                    first.get_or_insert(id);
                }
            }
            // in app mode, prefer a window with real content over the about:blank splash
            let pick = if opts.app_mode { first_nonblank.or(first) } else { first };
            if pick.is_some() || !opts.app_mode || start.elapsed() > opts.page_wait {
                break pick;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        let first = match first {
            Some(f) => f,
            None if opts.app_mode => bail!("앱 창(페이지 타깃)이 {}초 안에 나타나지 않았습니다", opts.page_wait.as_secs()),
            None => {
                let r = cdp
                    .call("Target.createTarget", json!({ "url": "about:blank" }), None)
                    .await?;
                r["targetId"].as_str().unwrap_or_default().to_string()
            }
        };
        ws.activate(&first).await?;
        ws.ready.store(true, Ordering::SeqCst);
        if !opts.start_url.trim().is_empty() {
            ws.navigate(&opts.start_url).await?;
        }
        pre_ready.0 = None;
        Ok(ws)
    }

    // ---- events ----

    async fn event_loop(self: Arc<Self>, mut rx: tokio::sync::broadcast::Receiver<CdpEvent>) {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if ev.method == "__closed" {
                        self.closed.store(true, Ordering::SeqCst);
                        break;
                    }
                    if let Err(e) = self.handle_event(ev).await {
                        if self.closed.load(Ordering::SeqCst) {
                            continue;
                        }
                        eprintln!("[ghostdesk] 이벤트 처리 오류: {e:#}");
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    }

    async fn handle_event(&self, ev: CdpEvent) -> Result<()> {
        match ev.method.as_str() {
            "Page.screencastFrame" => {
                let sid = ev.session_id.clone();
                if let Some(ack) = ev.params["sessionId"].as_i64() {
                    // Keep consuming events while Chromium is blocked on a modal dialog.
                    self.cdp.send_untracked("Page.screencastFrameAck", json!({ "sessionId": ack }), sid.as_deref())?;
                }
                if sid.is_some() && sid == self.active_session() {
                    if let Some(data) = ev.params["data"].as_str() {
                        if let Ok(bytes) = B64.decode(data) {
                            (self.sink)(&bytes);
                        }
                    }
                }
            }
            "Target.targetCreated" => {
                let info = &ev.params["targetInfo"];
                if !is_view_target(info, self.app_mode) {
                    return Ok(());
                }
                let id = info["targetId"].as_str().unwrap_or_default().to_string();
                let has_opener = info["openerId"].as_str().is_some();
                let is_new = {
                    let mut st = self.state.lock();
                    if st.tabs.iter().any(|t| t.target_id == id) {
                        false
                    } else {
                        st.tabs.push(TabInfo {
                            target_id: id.clone(),
                            title: info["title"].as_str().unwrap_or_default().into(),
                            url: info["url"].as_str().unwrap_or_default().into(),
                        });
                        true
                    }
                };
                // popups and new tabs opened by the page are switched to automatically
                if is_new && has_opener && self.ready.load(Ordering::SeqCst) && !self.app_mode {
                    self.state
                        .lock()
                        .notes
                        .push("페이지가 새 탭을 열어 그 탭으로 전환했습니다.".into());
                    self.activate(&id).await?;
                }
                // app mode: switch to a new window when it opens (kept hidden)
                if is_new && self.app_mode && self.ready.load(Ordering::SeqCst) {
                    self.state
                        .lock()
                        .notes
                        .push("앱이 새 창을 열어 그 창으로 전환했습니다 (창 목록에서 다시 바꿀 수 있습니다).".into());
                    self.activate(&id).await?;
                }
            }
            "Target.targetInfoChanged" => {
                let info = &ev.params["targetInfo"];
                let id = info["targetId"].as_str().unwrap_or_default();
                let mut st = self.state.lock();
                if let Some(t) = st.tabs.iter_mut().find(|t| t.target_id == id) {
                    t.title = info["title"].as_str().unwrap_or_default().into();
                    t.url = info["url"].as_str().unwrap_or_default().into();
                }
            }
            "Target.targetDestroyed" => {
                let id = ev.params["targetId"].as_str().unwrap_or_default().to_string();
                let next = {
                    let mut st = self.state.lock();
                    st.tabs.retain(|t| t.target_id != id);
                    if let Some(session) = st.sessions.remove(&id) { st.dialogs.remove(&session); }
                    if st.active.as_deref() == Some(id.as_str()) {
                        st.active = None;
                        Some(st.tabs.last().map(|t| t.target_id.clone()))
                    } else {
                        None
                    }
                };
                if let Some(next) = next {
                    let target = match next {
                        Some(t) => t,
                        None if self.app_mode => {
                            self.state
                                .lock()
                                .notes
                                .push("앱의 마지막 창이 닫혔습니다. 새 창이 열리면 자동으로 전환합니다.".into());
                            return Ok(());
                        }
                        None => {
                            let r = self
                                .cdp
                                .call("Target.createTarget", json!({ "url": "about:blank" }), None)
                                .await?;
                            r["targetId"].as_str().unwrap_or_default().to_string()
                        }
                    };
                    self.state.lock().notes.push("현재 탭이 닫혀 다른 탭으로 전환했습니다.".into());
                    self.activate(&target).await?;
                }
            }
            "Target.detachedFromTarget" => {
                if let Some(sid) = ev.params["sessionId"].as_str() {
                    let mut st = self.state.lock();
                    st.sessions.retain(|_, s| s != sid);
                    st.dialogs.remove(sid);
                    let prefix = format!("{sid}/");
                    st.loading.retain(|k| !k.starts_with(&prefix));
                    if st.drag.as_ref().is_some_and(|d| d.session == sid) {
                        st.drag = None;
                    }
                }
            }
            "Page.javascriptDialogOpening" => {
                let Some(session) = ev.session_id else { return Ok(()) };
                let msg = ev.params["message"].as_str().unwrap_or_default().to_string();
                let kind = ev.params["type"].as_str().unwrap_or("dialog").to_string();
                let mut state = self.state.lock();
                state.next_dialog_id += 1;
                let mut dialog = ev.params;
                dialog["id"] = json!(state.next_dialog_id);
                dialog["source"] = json!("javascript");
                state.dialogs.insert(session, dialog);
                state.notes.push(format!("페이지 대화상자({kind})가 응답을 기다립니다. dialog 조회 후 respond_dialog 로 응답하세요: {msg}"));
                drop(state);
                self.dialog_changed.notify_waiters();
            }
            "Page.frameStartedLoading" => {
                if let (Some(session), Some(frame)) = (ev.session_id.as_deref(), ev.params["frameId"].as_str()) {
                    let mut st = self.state.lock();
                    st.loading.insert(format!("{session}/{frame}"));
                    st.nav_seq += 1;
                }
            }
            "Page.frameStoppedLoading" | "Page.frameDetached" => {
                if let (Some(session), Some(frame)) = (ev.session_id.as_deref(), ev.params["frameId"].as_str()) {
                    self.state.lock().loading.remove(&format!("{session}/{frame}"));
                }
            }
            "Page.navigatedWithinDocument" => self.state.lock().nav_seq += 1,
            "Page.javascriptDialogClosed" => {
                if let Some(session) = ev.session_id { self.state.lock().dialogs.remove(&session); }
            }
            "Input.dragIntercepted" if self.intercept_drags => {
                // the page started an HTML5 drag (it doesn't reach the OS). If it's the active session and the
                // agent is holding the left button, remember it and finish it inside the page on move/release;
                // otherwise (e.g. it arrived after the button was already released) cancel it right away so the
                // page isn't left mid-drag.
                let Some(sid) = ev.session_id.clone() else { return Ok(()) };
                let data = ev.params["data"].clone();
                let keep = {
                    let mut st = self.state.lock();
                    let active = st.active.as_ref().and_then(|a| st.sessions.get(a)).cloned();
                    if st.buttons & MouseButton::Left.bit() != 0 && active.as_deref() == Some(sid.as_str()) {
                        st.drag = Some(DragState { session: sid.clone(), data: data.clone(), entered: false });
                        true
                    } else {
                        false
                    }
                };
                if !keep {
                    let (x, y) = self.mouse_pos();
                    let _ = self.drag_event(&sid, "dragCancel", x, y, &data, 0).await;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn active_session(&self) -> Option<String> {
        let st = self.state.lock();
        st.active.as_ref().and_then(|a| st.sessions.get(a).cloned())
    }

    fn session(&self) -> Result<String> {
        self.active_session().ok_or_else(|| anyhow!("활성 탭이 없습니다"))
    }

    pub fn dialog(&self) -> Option<Value> {
        let session = self.active_session()?;
        self.state.lock().dialogs.get(&session).cloned()
    }

    pub async fn respond_dialog(&self, id: u64, accept: bool, prompt_text: Option<&str>) -> Result<()> {
        let session = self.session()?;
        {
            let mut state = self.state.lock();
            let dialog = state.dialogs.get_mut(&session).ok_or_else(|| anyhow!("응답할 페이지 대화상자가 없습니다"))?;
            if dialog["id"] != id || dialog["responding"] == true { bail!("대화상자가 바뀌었거나 이미 응답 중입니다. dialog 를 다시 조회하세요."); }
            dialog["responding"] = json!(true);
        }
        let mut params = json!({"accept":accept});
        if let Some(text) = prompt_text { params["promptText"] = json!(text); }
        let result = self.cdp.call("Page.handleJavaScriptDialog", params, Some(&session)).await;
        let mut state = self.state.lock();
        if state.dialogs.get(&session).is_some_and(|dialog| dialog["id"] == id) {
            if result.is_ok() { state.dialogs.remove(&session); }
            else if let Some(dialog) = state.dialogs.get_mut(&session) { dialog.as_object_mut().unwrap().remove("responding"); }
        }
        result.map(|_| ())
    }

    async fn input_call(&self, method: &str, params: Value, session: &str) -> Result<Value> {
        let changed = self.dialog_changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if self.state.lock().dialogs.contains_key(session) { bail!("페이지 대화상자가 응답을 기다립니다. dialog 조회 후 respond_dialog 로 응답하세요."); }
        let call = self.cdp.call(method, params, Some(session));
        tokio::pin!(call);
        loop {
            tokio::select! {
                result = &mut call => return result,
                _ = &mut changed => {
                    if self.state.lock().dialogs.contains_key(session) {
                        bail!("입력은 전달됐으며 페이지 대화상자가 응답을 기다립니다. 입력을 반복하지 말고 dialog 조회 후 respond_dialog 로 응답하세요.");
                    }
                    changed.set(self.dialog_changed.notified());
                    changed.as_mut().enable();
                }
            }
        }
    }

    async fn activate(&self, target_id: &str) -> Result<()> {
        if self.intercept_drags {
            // cancel any unfinished drag inside its original page before switching views
            self.cancel_drag().await;
        }
        if let Some(prev) = self.active_session() {
            let _ = self.cdp.call("Page.stopScreencast", json!({}), Some(&prev)).await;
        }
        let existing = self.state.lock().sessions.get(target_id).cloned();
        let session = match existing {
            Some(s) => s,
            None => {
                let r = self
                    .cdp
                    .call(
                        "Target.attachToTarget",
                        json!({ "targetId": target_id, "flatten": true }),
                        None,
                    )
                    .await?;
                let s = r["sessionId"]
                    .as_str()
                    .ok_or_else(|| anyhow!("sessionId 없음"))?
                    .to_string();
                self.init_session(&s).await?;
                self.state.lock().sessions.insert(target_id.to_string(), s.clone());
                s
            }
        };
        if !self.app_mode {
            // in Electron, activateTarget can bring the real window to the front, so it isn't sent
            let _ = self
                .cdp
                .call("Target.activateTarget", json!({ "targetId": target_id }), None)
                .await;
        }
        self.state.lock().active = Some(target_id.to_string());
        self.cdp
            .call(
                "Page.startScreencast",
                json!({
                    "format": "jpeg", "quality": 70,
                    "maxWidth": self.width, "maxHeight": self.height,
                    "everyNthFrame": 1
                }),
                Some(&session),
            )
            .await?;
        Ok(())
    }

    async fn init_session(&self, s: &str) -> Result<()> {
        let c = &self.cdp;
        c.call("Page.enable", json!({}), Some(s)).await?;
        c.call(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": self.width, "height": self.height,
                "deviceScaleFactor": 1, "mobile": false
            }),
            Some(s),
        )
        .await?;
        // so input focus/blur works in the background and headless too
        let _ = c
            .call("Emulation.setFocusEmulationEnabled", json!({ "enabled": true }), Some(s))
            .await;
        if let Some(ua) = &self.user_agent {
            let _ = c
                .call("Network.setUserAgentOverride", json!({ "userAgent": ua }), Some(s))
                .await;
        }
        if self.intercept_drags {
            // has to be enabled per session (= page target) and survives navigation. If it can't be (old
            // Chromium), this session falls back to sending mouse events that never start a drag (see
            // mouse_move_intercepting).
            if let Err(e) = c.call("Input.setInterceptDrags", json!({ "enabled": true }), Some(s)).await {
                eprintln!("[ghostdesk] 드래그 가로채기를 켜지 못했습니다: {e:#}");
                let mut st = self.state.lock();
                st.no_intercept.insert(s.to_string());
                st.notes.push(
                    "이 창에서는 드래그 가로채기를 켤 수 없어 끌어 놓기(drag and drop)가 동작하지 않습니다 \
                     (사용자 화면으로 새어 나가지 않게 드래그를 시작하지 않음). 메뉴·명령 팔레트를 쓰세요."
                        .into(),
                );
            }
        }
        Ok(())
    }

    /// Rereads view titles and URLs with Target.getTargets (adding new view targets too).
    pub async fn refresh_targets(&self) -> Result<()> {
        let targets = self.cdp.call("Target.getTargets", json!({}), None).await?;
        let mut st = self.state.lock();
        if let Some(arr) = targets["targetInfos"].as_array() {
            for t in arr.iter().filter(|t| is_view_target(t, self.app_mode)) {
                let id = t["targetId"].as_str().unwrap_or_default();
                let title = t["title"].as_str().unwrap_or_default().to_string();
                let url = t["url"].as_str().unwrap_or_default().to_string();
                match st.tabs.iter_mut().find(|x| x.target_id == id) {
                    Some(x) => {
                        x.title = title;
                        x.url = url;
                    }
                    None => st.tabs.push(TabInfo { target_id: id.to_string(), title, url }),
                }
            }
        }
        Ok(())
    }

    // ---- queries ----

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn tabs(&self) -> (Vec<TabInfo>, Option<String>) {
        let st = self.state.lock();
        (st.tabs.clone(), st.active.clone())
    }

    pub fn active_tab(&self) -> Option<TabInfo> {
        let st = self.state.lock();
        let a = st.active.as_ref()?;
        st.tabs.iter().find(|t| &t.target_id == a).cloned()
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn views(&self) -> (Vec<crate::desk::ViewInfo>, Option<String>) {
        let (tabs, active) = self.tabs();
        let v = tabs
            .into_iter()
            .map(|t| crate::desk::ViewInfo { id: t.target_id, title: t.title, detail: t.url })
            .collect();
        (v, active)
    }

    pub async fn switch_view(&self, index: usize) -> Result<()> {
        self.switch_tab(index).await
    }

    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut self.state.lock().notes)
    }

    pub fn mouse_pos(&self) -> (f64, f64) {
        self.state.lock().mouse
    }

    // ---- screen ----

    /// PNG screenshot of the current viewport (exactly width x height).
    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        if self.dialog().is_some() { bail!("페이지 대화상자가 응답을 기다립니다. 화면 대신 dialog 로 내용을 확인하세요."); }
        let s = self.session()?;
        let r = self
            .cdp
            .call(
                "Page.captureScreenshot",
                json!({ "format": "png", "fromSurface": true, "captureBeyondViewport": false }),
                Some(&s),
            )
            .await?;
        let bytes = B64.decode(r["data"].as_str().unwrap_or_default())?;
        // check the size from the PNG header (IHDR) and only decode to resize when it differs
        let bytes = if png_size(&bytes) == Some((self.width, self.height)) {
            bytes
        } else {
            let img = image::load_from_memory(&bytes)?;
            let resized = img.resize_exact(self.width, self.height, image::imageops::FilterType::Triangle);
            encode_png(&resized)?
        };
        (self.sink)(&bytes);
        Ok(bytes)
    }

    /// Zoomed-in PNG of a screen region.
    pub async fn zoom(&self, region: [f64; 4]) -> Result<Vec<u8>> {
        let s = self.session()?;
        let (x0, y0) = (region[0].min(region[2]), region[1].min(region[3]));
        let (x1, y1) = (region[0].max(region[2]), region[1].max(region[3]));
        let (w, h) = ((x1 - x0).max(1.0), (y1 - y0).max(1.0));
        let scale = (self.width as f64 / w).min(self.height as f64 / h).clamp(1.0, 8.0);
        let r = self
            .cdp
            .call(
                "Page.captureScreenshot",
                json!({
                    "format": "png", "fromSurface": true,
                    "clip": { "x": x0, "y": y0, "width": w, "height": h, "scale": scale }
                }),
                Some(&s),
            )
            .await?;
        Ok(B64.decode(r["data"].as_str().unwrap_or_default())?)
    }

    // ---- mouse ----

    async fn mouse_event(
        &self,
        typ: &str,
        x: f64,
        y: f64,
        button: &str,
        click_count: i64,
        mods: i64,
        extra: Value,
    ) -> Result<()> {
        let s = self.session()?;
        let buttons = self.state.lock().buttons;
        let mut p = json!({
            "type": typ, "x": x, "y": y, "button": button, "buttons": buttons,
            "clickCount": click_count, "modifiers": mods
        });
        if let (Some(obj), Some(ex)) = (p.as_object_mut(), extra.as_object()) {
            for (k, v) in ex {
                obj.insert(k.clone(), v.clone());
            }
        }
        self.input_call("Input.dispatchMouseEvent", p, &s).await?;
        Ok(())
    }

    pub async fn mouse_move(&self, x: f64, y: f64, mods: i64) -> Result<()> {
        let (x, y) = self.clamp(x, y);
        self.state.lock().mouse = (x, y);
        if self.intercept_drags {
            return self.mouse_move_intercepting(x, y, mods).await;
        }
        self.mouse_event("mouseMoved", x, y, "none", 0, mods, json!({})).await
    }

    pub async fn mouse_down(&self, button: MouseButton, count: i64, mods: i64) -> Result<()> {
        let (x, y) = self.mouse_pos();
        if self.intercept_drags && button == MouseButton::Left {
            // a leftover drag (shouldn't happen) is cancelled inside the page before starting over
            self.cancel_drag().await;
            self.state.lock().moved_pressed = false;
        }
        self.state.lock().buttons |= button.bit();
        self.mouse_event("mousePressed", x, y, button.name(), count, mods, json!({})).await
    }

    pub async fn mouse_up(&self, button: MouseButton, count: i64, mods: i64) -> Result<()> {
        let (x, y) = self.mouse_pos();
        if self.intercept_drags && button == MouseButton::Left {
            // moving with the button held may have started a drag in the page. Input.dragIntercepted can
            // arrive a little late, so wait briefly. If a drag is in progress, drop inside the page instead
            // of sending mouseReleased (in a real OS drag and drop, the release doesn't reach the page
            // either).
            let moved = std::mem::take(&mut self.state.lock().moved_pressed);
            if moved {
                self.wait_drag(Duration::from_millis(250)).await;
            }
            let drag = {
                let mut st = self.state.lock();
                st.buttons &= !button.bit();
                st.drag.take()
            };
            if let Some(d) = drag {
                return self.finish_drag(d, x, y, mods).await;
            }
            return self.mouse_event("mouseReleased", x, y, button.name(), count, mods, json!({})).await;
        }
        self.state.lock().buttons &= !button.bit();
        self.mouse_event("mouseReleased", x, y, button.name(), count, mods, json!({})).await
    }

    // ---- HTML5 drag interception (ConnectOptions::intercept_drags) ----

    /// Mouse move in drag interception mode.
    ///
    /// Blink only starts an HTML5 drag (dragstart) on a mouseMoved whose `button` is left (with
    /// "none", as on the default path, in-page drag and drop never works). Only sessions with
    /// interception enabled get left, so in-app drag and drop works (VS Code explorer, editor tabs,
    /// …), and once a drag starts, dragOver is sent instead of mouseMoved. Sessions where
    /// interception couldn't be enabled keep sending "none" so no drag ever starts (and nothing can
    /// leak to the OS).
    async fn mouse_move_intercepting(&self, x: f64, y: f64, mods: i64) -> Result<()> {
        let (pressed, guarded) = {
            let st = self.state.lock();
            let active = st.active.as_ref().and_then(|a| st.sessions.get(a));
            (
                st.buttons & MouseButton::Left.bit() != 0,
                active.is_some_and(|s| !st.no_intercept.contains(s)),
            )
        };
        if self.drag_over(x, y, mods).await? {
            return Ok(());
        }
        let button = if pressed && guarded { "left" } else { "none" };
        self.mouse_event("mouseMoved", x, y, button, 0, mods, json!({})).await?;
        if pressed && guarded {
            self.state.lock().moved_pressed = true;
            // if this move started a drag (and the event already arrived), drag it in to this position now
            self.drag_over(x, y, mods).await?;
        }
        Ok(())
    }

    /// If there's an intercepted drag, sends dragEnter (first time only) and dragOver at (x, y) and
    /// returns true.
    async fn drag_over(&self, x: f64, y: f64, mods: i64) -> Result<bool> {
        let Some(d) = self.state.lock().drag.clone() else { return Ok(false) };
        let mut r = Ok(());
        if !d.entered {
            r = self.drag_event(&d.session, "dragEnter", x, y, &d.data, mods).await;
            if r.is_ok() {
                if let Some(cur) = self.state.lock().drag.as_mut() {
                    cur.entered = true;
                }
            }
        }
        if r.is_ok() {
            r = self.drag_event(&d.session, "dragOver", x, y, &d.data, mods).await;
        }
        if let Err(e) = r {
            self.cancel_drag().await;
            return Err(e.context("드래그 이동 실패 (페이지 안에서 드래그를 취소했습니다)"));
        }
        Ok(true)
    }

    /// Drops the intercepted drag where the button was released: dragEnter (if not yet sent) →
    /// dragOver → drop.
    async fn finish_drag(&self, d: DragState, x: f64, y: f64, mods: i64) -> Result<()> {
        let mut r = Ok(());
        if !d.entered {
            r = self.drag_event(&d.session, "dragEnter", x, y, &d.data, mods).await;
        }
        if r.is_ok() {
            r = self.drag_event(&d.session, "dragOver", x, y, &d.data, mods).await;
        }
        if r.is_ok() {
            r = self.drag_event(&d.session, "drop", x, y, &d.data, mods).await;
        }
        if let Err(e) = r {
            let _ = self.drag_event(&d.session, "dragCancel", x, y, &d.data, mods).await;
            return Err(e.context("드래그 놓기 실패 (페이지 안에서 드래그를 취소했습니다)"));
        }
        Ok(())
    }

    /// Cancels an unfinished intercepted drag inside the page (no-op if there is none).
    async fn cancel_drag(&self) {
        let d = self.state.lock().drag.take();
        if let Some(d) = d {
            let (x, y) = self.mouse_pos();
            let _ = self.drag_event(&d.session, "dragCancel", x, y, &d.data, 0).await;
        }
    }

    /// Waits up to `max` for Input.dragIntercepted to arrive.
    async fn wait_drag(&self, max: Duration) {
        let start = Instant::now();
        while start.elapsed() < max {
            if self.state.lock().drag.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn drag_event(&self, session: &str, typ: &str, x: f64, y: f64, data: &Value, mods: i64) -> Result<()> {
        self.cdp
            .call_timeout(
                "Input.dispatchDragEvent",
                json!({ "type": typ, "x": x, "y": y, "data": data, "modifiers": mods }),
                Some(session),
                Duration::from_secs(5),
            )
            .await?;
        Ok(())
    }

    pub async fn click(&self, pos: Option<(f64, f64)>, button: MouseButton, count: u32, mods: i64) -> Result<()> {
        if let Some((x, y)) = pos {
            self.mouse_move(x, y, mods).await?;
        }
        // press, then release a moment later like a person would (some pages react to the pressed state,
        // and it matches the on-screen click indicator)
        for i in 1..=count.max(1) as i64 {
            self.mouse_down(button, i, mods).await?;
            tokio::time::sleep(Duration::from_millis(crate::cursor::PRESS_MS)).await;
            self.mouse_up(button, i, mods).await?;
            if i < count as i64 {
                tokio::time::sleep(Duration::from_millis(crate::cursor::MULTI_CLICK_GAP_MS)).await;
            }
        }
        Ok(())
    }

    pub async fn drag(&self, from: (f64, f64), to: (f64, f64), mods: i64) -> Result<()> {
        self.mouse_move(from.0, from.1, mods).await?;
        self.mouse_down(MouseButton::Left, 1, mods).await?;
        tokio::time::sleep(Duration::from_millis(crate::cursor::PRESS_MS)).await;
        // drag along an ease-in/ease-out path over a time matched to the distance (the same curve as the
        // on-screen glide)
        let dist = ((to.0 - from.0).powi(2) + (to.1 - from.1).powi(2)).sqrt();
        let steps = crate::cursor::frames(crate::cursor::glide_duration(dist));
        for i in 1..=steps {
            let t = crate::cursor::ease(i as f32 / steps as f32) as f64;
            self.mouse_move(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t, mods).await?;
            tokio::time::sleep(Duration::from_millis(crate::cursor::FRAME_MS)).await;
        }
        self.mouse_up(MouseButton::Left, 1, mods).await
    }

    pub async fn scroll(&self, pos: Option<(f64, f64)>, dx: f64, dy: f64, mods: i64) -> Result<()> {
        let (x, y) = match pos {
            Some((x, y)) => {
                self.mouse_move(x, y, mods).await?;
                self.mouse_pos()
            }
            None => self.mouse_pos(),
        };
        // split into several small wheel events instead of one big jump, starting fast and easing out
        let parts = crate::cursor::scroll_parts(dx.abs().max(dy.abs()));
        let n = parts.len();
        for (i, f) in parts.into_iter().enumerate() {
            self.mouse_event("mouseWheel", x, y, "none", 0, mods, json!({ "deltaX": dx * f, "deltaY": dy * f })).await?;
            if i + 1 < n {
                tokio::time::sleep(Duration::from_millis(crate::cursor::FRAME_MS)).await;
            }
        }
        Ok(())
    }

    fn clamp(&self, x: f64, y: f64) -> (f64, f64) {
        (
            x.clamp(0.0, self.width as f64 - 1.0),
            y.clamp(0.0, self.height as f64 - 1.0),
        )
    }

    // ---- keyboard ----

    async fn key_event(&self, typ: &str, k: &keys::KeyDef, mods: i64, with_text: bool, commands: &[&str]) -> Result<()> {
        let s = self.session()?;
        let mut p = json!({
            "type": typ, "key": k.key, "code": k.code,
            "windowsVirtualKeyCode": k.key_code, "nativeVirtualKeyCode": k.key_code,
            "modifiers": mods
        });
        if with_text {
            if let Some(t) = &k.text {
                p["text"] = json!(t);
                p["unmodifiedText"] = json!(t);
            }
        }
        if !commands.is_empty() {
            p["commands"] = json!(commands);
        }
        self.input_call("Input.dispatchKeyEvent", p, &s).await?;
        Ok(())
    }

    async fn combo_down(&self, c: &keys::Combo) -> Result<()> {
        let mut acc = 0;
        for m in &c.modifiers {
            acc |= m.modifier_bit;
            self.key_event("rawKeyDown", m, acc, false, &[]).await?;
        }
        let printable = c.main.text.is_some() && c.mods & (MOD_CTRL | MOD_META | MOD_ALT) == 0;
        let cmds = keys::mac_edit_commands(c);
        let typ = if printable { "keyDown" } else { "rawKeyDown" };
        self.key_event(typ, &c.main, c.mods, printable, &cmds).await
    }

    async fn combo_up(&self, c: &keys::Combo) -> Result<()> {
        self.key_event("keyUp", &c.main, c.mods, false, &[]).await?;
        let mut acc = c.mods;
        for m in c.modifiers.iter().rev() {
            acc &= !m.modifier_bit;
            self.key_event("keyUp", m, acc, false, &[]).await?;
        }
        Ok(())
    }

    pub async fn key(&self, combo: &str, repeat: u32) -> Result<()> {
        let c = keys::parse_combo(combo)?;
        if let Some(cmd) = browser_shortcut(&c) {
            for _ in 0..repeat.clamp(1, 100) {
                self.run_shortcut(cmd).await?;
            }
            return Ok(());
        }
        for _ in 0..repeat.clamp(1, 100) {
            self.combo_down(&c).await?;
            self.combo_up(&c).await?;
        }
        Ok(())
    }

    /// Does what the browser itself would do for a shortcut that never reaches the page.
    async fn run_shortcut(&self, cmd: Shortcut) -> Result<()> {
        let (tabs, active) = self.tabs();
        let n = tabs.len().max(1);
        let cur = tabs.iter().position(|t| Some(&t.target_id) == active.as_ref()).unwrap_or(0);
        match cmd {
            Shortcut::NewTab => self.new_tab("").await,
            Shortcut::CloseTab if tabs.len() <= 1 => bail!("마지막 탭은 닫지 않았습니다 (작업 화면이 닫힙니다)"),
            Shortcut::CloseTab => self.close_tab(cur).await,
            Shortcut::NextTab => self.switch_tab((cur + 1) % n).await,
            Shortcut::PrevTab => self.switch_tab((cur + n - 1) % n).await,
            Shortcut::Tab(i) => self.switch_tab(i.min(n - 1)).await,
            Shortcut::LastTab => self.switch_tab(n - 1).await,
            Shortcut::Back => self.history_go(-1).await,
            Shortcut::Forward => self.history_go(1).await,
            Shortcut::Reload => self.reload().await,
        }
    }

    pub async fn hold_key(&self, combo: &str, secs: f64) -> Result<()> {
        let c = keys::parse_combo(combo)?;
        self.combo_down(&c).await?;
        tokio::time::sleep(Duration::from_secs_f64(secs.clamp(0.0, 300.0))).await;
        self.combo_up(&c).await
    }

    /// Types text: ASCII as real key events, Korean and other text via insertText.
    pub async fn type_text(&self, text: &str) -> Result<()> {
        let s = self.session()?;
        let mut buf = String::new();
        let text = text.replace("\r\n", "\n");
        for ch in text.chars() {
            let special = match ch {
                '\n' | '\r' => Some("Return"),
                '\t' => Some("Tab"),
                _ => None,
            };
            if special.is_some() || (ch.is_ascii() && !ch.is_ascii_control()) {
                if !buf.is_empty() {
                    self
                        .input_call("Input.insertText", json!({ "text": std::mem::take(&mut buf) }), &s)
                        .await?;
                }
                match special {
                    Some(k) => self.key(k, 1).await?,
                    None => {
                        let k = keys::char_key(ch).ok_or_else(|| anyhow!("입력 불가 문자"))?;
                        let mods = if k.needs_shift { keys::MOD_SHIFT } else { 0 };
                        self.key_event("keyDown", &k, mods, true, &[]).await?;
                        self.key_event("keyUp", &k, mods, false, &[]).await?;
                    }
                }
            } else if !ch.is_control() {
                buf.push(ch);
            }
        }
        if !buf.is_empty() {
            self.input_call("Input.insertText", json!({ "text": buf }), &s).await?;
        }
        Ok(())
    }

    // ---- navigation / tabs ----

    pub async fn navigate(&self, raw: &str) -> Result<()> {
        let url = normalize_url(raw);
        // Windows: file://server/share URLs make SMB send the user's credentials (NTLM) to that server
        #[cfg(windows)]
        if let Some(why) = remote_file_refusal(raw, &url) {
            bail!("{why}");
        }
        let s = self.session()?;
        let r = self.cdp.call("Page.navigate", json!({ "url": url }), Some(&s)).await?;
        if let Some(err) = r["errorText"].as_str() {
            if !err.is_empty() {
                bail!("이동 실패: {err}");
            }
        }
        self.wait_ready(Duration::from_secs(15)).await;
        Ok(())
    }

    pub async fn wait_ready(&self, timeout: Duration) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Ok(s) = self.session() {
                if let Ok(r) = self
                    .cdp
                    .call_timeout(
                        "Runtime.evaluate",
                        json!({ "expression": "document.readyState", "returnByValue": true }),
                        Some(&s),
                        Duration::from_secs(3),
                    )
                    .await
                {
                    if r["result"]["value"] == "complete" {
                        return;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// How many navigations have started so far (see `settle_after`).
    pub fn nav_mark(&self) -> u64 {
        self.state.lock().nav_seq
    }

    /// Whether the active tab has a frame that is still loading.
    fn is_loading(&self) -> bool {
        let st = self.state.lock();
        let Some(prefix) = st.active.as_ref().and_then(|a| st.sessions.get(a)).map(|s| format!("{s}/")) else { return false };
        st.loading.iter().any(|k| k.starts_with(&prefix))
    }

    /// Waits while the active tab is loading a page (at most `max`); returns at once when it is not.
    pub async fn wait_not_loading(&self, max: Duration) {
        let start = Instant::now();
        while self.is_loading() && start.elapsed() < max {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// After an action that may leave the page (a click, Enter in a form): if a navigation began since `since`
    /// (from `nav_mark`) — or begins within a short grace period — waits until the new page has loaded, at most
    /// `max`. Returns what happened for the agent to read: `None` when the page did not change.
    pub async fn settle_after(&self, since: u64, grace: Duration, max: Duration) -> Option<String> {
        let start = Instant::now();
        let mut started = false;
        loop {
            let (seq, loading) = (self.nav_mark(), self.is_loading());
            started |= seq > since;
            if started && !loading {
                break;
            }
            if !started && start.elapsed() >= grace {
                return None;
            }
            if start.elapsed() >= max {
                return Some(format!("페이지가 {}초 안에 다 열리지 않았어요 (아직 불러오는 중이면 wait_for 로 기다리세요)", max.as_secs_f64()));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // loaded; the document may still be settling (readyState), and scripts get a moment to draw
        self.wait_ready(max.saturating_sub(start.elapsed()).max(Duration::from_millis(500))).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let page = self.eval("document.title + ' — ' + location.href").await.ok().and_then(|v| v.as_str().map(String::from));
        Some(format!("페이지가 바뀌었어요{}", page.map(|p| format!(": {p}")).unwrap_or_default()))
    }

    pub async fn history_go(&self, delta: i64) -> Result<()> {
        let s = self.session()?;
        let h = self.cdp.call("Page.getNavigationHistory", json!({}), Some(&s)).await?;
        let idx = h["currentIndex"].as_i64().unwrap_or(0) + delta;
        let entries = h["entries"].as_array().cloned().unwrap_or_default();
        let entry = entries
            .get(idx.max(0) as usize)
            .filter(|_| idx >= 0)
            .ok_or_else(|| anyhow!("더 이상 이동할 기록이 없습니다"))?;
        self.cdp
            .call("Page.navigateToHistoryEntry", json!({ "entryId": entry["id"] }), Some(&s))
            .await?;
        self.wait_ready(Duration::from_secs(10)).await;
        Ok(())
    }

    pub async fn reload(&self) -> Result<()> {
        let s = self.session()?;
        self.cdp.call("Page.reload", json!({}), Some(&s)).await?;
        self.wait_ready(Duration::from_secs(15)).await;
        Ok(())
    }

    pub async fn new_tab(&self, url: &str) -> Result<()> {
        let u = if url.trim().is_empty() { "about:blank".to_string() } else { normalize_url(url) };
        #[cfg(windows)]
        if let Some(why) = remote_file_refusal(url, &u) {
            bail!("{why}");
        }
        let r = self.cdp.call("Target.createTarget", json!({ "url": u }), None).await?;
        let id = r["targetId"].as_str().unwrap_or_default().to_string();
        {
            let mut st = self.state.lock();
            if !st.tabs.iter().any(|t| t.target_id == id) {
                st.tabs.push(TabInfo { target_id: id.clone(), title: String::new(), url: u.clone() });
            }
        }
        self.activate(&id).await?;
        self.wait_ready(Duration::from_secs(15)).await;
        Ok(())
    }

    pub async fn switch_tab(&self, index: usize) -> Result<()> {
        let id = self
            .state
            .lock()
            .tabs
            .get(index)
            .map(|t| t.target_id.clone())
            .ok_or_else(|| anyhow!("탭 번호 {index} 가 없습니다"))?;
        self.activate(&id).await
    }

    pub async fn close_tab(&self, index: usize) -> Result<()> {
        let id = self
            .state
            .lock()
            .tabs
            .get(index)
            .map(|t| t.target_id.clone())
            .ok_or_else(|| anyhow!("탭 번호 {index} 가 없습니다"))?;
        self.cdp.call("Target.closeTarget", json!({ "targetId": id }), None).await?;
        Ok(())
    }

    pub fn tabs_text(&self) -> String {
        let (tabs, active) = self.tabs();
        let mut out = String::new();
        for (i, t) in tabs.iter().enumerate() {
            let mark = if Some(&t.target_id) == active.as_ref() { "▶" } else { " " };
            out.push_str(&format!(
                "{mark} [{i}] {} — {}\n",
                crate::llm::short(&t.title, 80),
                crate::llm::short(&t.url, 160)
            ));
        }
        out
    }

    /// Evaluates JS in the page (for self-tests).
    pub async fn eval(&self, expr: &str) -> Result<Value> {
        let s = self.session()?;
        let r = self
            .input_call("Runtime.evaluate", json!({ "expression": expr, "returnByValue": true }), &s)
            .await?;
        Ok(r["result"]["value"].clone())
    }

    // ---- shutdown ----

    pub async fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if self.app_mode && self.child.lock().is_none() {
            // Electron apps are shut down gracefully from outside (electron.rs) through the inspector
            return;
        }
        let _ = self
            .cdp
            .call_timeout("Browser.close", json!({}), None, Duration::from_secs(3))
            .await;
        let child = self.child.lock().take();
        if let Some(mut c) = child {
            for _ in 0..30 {
                #[cfg(not(windows))]
                if let Ok(Some(_)) = c.try_wait() {
                    return;
                }
                // Windows: it only counts as a clean exit once the browser inside the Job has ended too, not
                // just the main process (which may be a launcher). Clean up leftover helpers (crashpad, …) and
                // close the Job handle.
                #[cfg(windows)]
                if let Ok(Some(_)) = c.try_wait() {
                    if !launcher_handed_off(&c) {
                        crate::platform::proc::kill_group(&mut c);
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            kill_child(c);
        }
    }

    /// pid of the browser main process this workspace launched (None for externally managed
    /// Electron).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn browser_pid(&self) -> Option<u32> {
        self.child.lock().as_ref().map(|c| c.id())
    }
}

/// Holds the launched Chromium until the workspace takes ownership; if dropped before that
/// (future aborted, error), kills and reaps the whole process group.
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(c: Child) -> Self {
        ChildGuard(Some(c))
    }
    fn get(&mut self) -> &mut Child {
        self.0.as_mut().expect("ChildGuard 는 release 전까지 Child 를 가진다")
    }
    /// Hands over ownership (Drop does nothing afterwards).
    fn release(mut self) -> Child {
        self.0.take().expect("ChildGuard 는 release 전까지 Child 를 가진다")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(c) = self.0.take() {
            kill_child_group(c);
        }
    }
}

/// Kills and reaps the whole process group, only for children not yet reaped (so the pid/pgid
/// can't have been reused).
///
/// Launched with spawn_group, so on Unix it's SIGKILL to pgid == pid, and on Windows the whole Job
/// object is terminated.
fn kill_child_group(mut c: Child) {
    crate::platform::proc::kill_group(&mut c);
}

/// (Windows) Whether the main process has exited but live processes, like the real browser,
/// remain in the same Job.
///
/// This happens when a launcher executable (Naver Whale's Application\whale.exe, …) hands off to
/// the real browser. (If a browser using the same user-data-dir was already running and only got
/// the command line, the Job is empty.)
#[cfg(windows)]
fn launcher_handed_off(c: &Child) -> bool {
    crate::platform::proc::group_pids(c.id()).is_some_and(|p| !p.is_empty())
}

/// Cleans up the remaining browser on the normal exit path (shutdown grace period expired, Drop).
///
/// Unix: kills only the main process, as before (helpers exit on their own).
/// Windows: tears down the whole Job so GPU, renderer and crashpad helpers don't linger holding
/// files in the workspace folder.
fn kill_child(mut c: Child) {
    #[cfg(windows)]
    crate::platform::proc::kill_group(&mut c);
    #[cfg(not(windows))]
    {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// If connect() ends before ready (cancelled, error), kills the Chromium the workspace holds and
/// marks it closed.
struct CloseOnDrop(Option<Arc<BrowserWorkspace>>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        if let Some(ws) = self.0.take() {
            ws.closed.store(true, Ordering::SeqCst);
            let child = ws.child.lock().take();
            if let Some(c) = child {
                kill_child_group(c);
            }
        }
    }
}

impl Drop for BrowserWorkspace {
    fn drop(&mut self) {
        if let Some(c) = self.child.lock().take() {
            kill_child(c);
        }
    }
}

/// Turns a `DevToolsActivePort` file (port on the first line, path on the second) into the
/// browser websocket URL.
pub fn read_devtools_port_file(path: &std::path::Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let mut lines = s.lines();
    let (port, p) = (lines.next()?, lines.next()?);
    Some(format!("ws://127.0.0.1:{}{}", port.trim(), p.trim()))
}

pub fn encode_png(img: &image::DynamicImage) -> Result<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

/// Reads (width, height) from a PNG's IHDR (no decoding).
pub fn png_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 24 || &b[..8] != b"\x89PNG\r\n\x1a\n" || &b[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
    let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
    Some((w, h))
}

pub fn normalize_url(raw: &str) -> String {
    let t = raw.trim();
    if t.contains("://") || t.starts_with("about:") || t.starts_with("data:") || t.starts_with("chrome:") {
        return t.to_string();
    }
    if !t.contains(' ') && (t.contains('.') || t.starts_with("localhost")) {
        return format!("https://{t}");
    }
    let q: String = t
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("https://www.google.com/search?q={q}")
}

/// (Windows) Reason to refuse a file: URL or UNC path pointing at another computer.
///
/// Chromium on Windows opens `file://server/share/x` as `\\server\share\x`, and the SMB (WebDAV)
/// redirector then authenticates to that server with the user's Windows credentials (NTLM).
/// `raw` is what the agent gave, `url` is the normalize_url result. (Not used on macOS, where
/// file://host isn't SMB access.)
#[cfg_attr(not(windows), allow(dead_code))]
pub fn remote_file_refusal(raw: &str, url: &str) -> Option<String> {
    let t = raw.trim();
    let unc = ["\\\\", "//", "\\/", "/\\"].iter().any(|p| t.starts_with(p));
    // check the original input as well as the URL actually sent (refuse even if normalize_url turned
    // input like `file:\\server` into https)
    let host = if unc { None } else { file_url_remote_host(url).or_else(|| file_url_remote_host(t)) };
    if !unc && host.is_none() {
        return None;
    }
    let target = match host {
        Some(h) => format!("호스트 '{h}'"),
        None => "UNC 경로".into(),
    };
    Some(format!(
        "다른 컴퓨터(네트워크 공유)를 가리키는 주소는 열 수 없습니다 ({target}): '{}'. Windows 에서는 이런 file:// 주소나 \
         \\\\서버\\공유 경로를 열면 그 서버에 사용자의 Windows 로그인 자격 증명(NTLM)이 자동으로 전송됩니다. \
         이 PC 의 파일은 file:///C:/폴더/파일 처럼 호스트 없이 쓰세요. \
         (file:// URLs with a remote host and UNC paths are blocked on Windows because they send the user's NTLM credentials.)",
        crate::llm::short(t, 200)
    ))
}

/// The remote host a file: URL points at (None for local files), following Windows Chromium's
/// parsing.
///
/// Backslashes count as slashes; three slashes (`file:///`) or a drive letter (`file:C:/`,
/// `file:////C:/`) are local, and any other slash count is UNC (the first path segment is the
/// host). Even with an empty host or localhost, a path starting with `//`
/// (`file://localhost//server/share`) can turn into UNC, so it counts as remote.
fn file_url_remote_host(url: &str) -> Option<String> {
    // like Chromium, strip tabs and newlines and ignore surrounding whitespace and control characters
    let s: String = url.chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
    let mut s = s.trim_matches(|c: char| c <= ' ');
    // view-source: opens the inner URL
    while let Some(p) = s.get(..12) {
        if !p.eq_ignore_ascii_case("view-source:") {
            break;
        }
        s = s[12..].trim_start_matches(|c: char| c <= ' ');
    }
    let rest = match s.get(..5) {
        Some(p) if p.eq_ignore_ascii_case("file:") => s[5..].replace('\\', "/"),
        _ => return None,
    };
    let slashes = rest.chars().take_while(|&c| c == '/').count();
    let after = &rest[slashes..];
    let is_drive = |p: &str| {
        let b = p.as_bytes();
        b.len() >= 2 && b[0].is_ascii_alphabetic() && matches!(b[1], b':' | b'|') && (b.len() == 2 || matches!(b[2], b'/' | b'?' | b'#'))
    };
    if is_drive(after) || slashes == 3 {
        return None;
    }
    let end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let host = percent_decode(&after[..end]).trim().to_ascii_lowercase();
    if !host.is_empty() && host != "localhost" {
        return Some(host);
    }
    // no host or localhost: a path starting with // again can become a UNC path
    let path = &after[end..];
    let path_slashes = path.chars().take_while(|&c| c == '/').count();
    if path_slashes >= 2 && !is_drive(&path[path_slashes..]) {
        let p = &path[path_slashes..];
        let h = &p[..p.find(['/', '?', '#']).unwrap_or(p.len())];
        return Some(if h.is_empty() { "(빈 호스트)".into() } else { percent_decode(h).to_ascii_lowercase() });
    }
    None
}

/// Decodes `%XX` back to bytes (invalid ones stay as is; non-UTF-8 is converted lossily).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() {
            let hex = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
            out.push(hex(b[i + 1]) * 16 + hex(b[i + 2]));
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Record for tests to find the main process pid launched by spawn() (user-data-dir → pid).

/// A browser-window shortcut. Key events sent over CDP only reach the page, so these do nothing
/// unless they are carried out here.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Shortcut {
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    Tab(usize),
    LastTab,
    Back,
    Forward,
    Reload,
}

fn browser_shortcut(c: &keys::Combo) -> Option<Shortcut> {
    use keys::MOD_SHIFT;
    // the command key on macOS, Ctrl elsewhere
    let primary = if cfg!(target_os = "macos") { MOD_META } else { MOD_CTRL };
    let m = c.mods;
    let only = |want: i64| m == want;
    Some(match c.main.key_code {
        0x54 if only(primary) => Shortcut::NewTab,                                  // T
        0x57 | 0x73 if only(primary) => Shortcut::CloseTab,                         // W, F4
        0x09 if only(MOD_CTRL) => Shortcut::NextTab,                                // Tab
        0x09 if only(MOD_CTRL | MOD_SHIFT) => Shortcut::PrevTab,
        0x22 if only(MOD_CTRL) => Shortcut::NextTab,                                // PageDown
        0x21 if only(MOD_CTRL) => Shortcut::PrevTab,                                // PageUp
        0x31..=0x38 if only(primary) => Shortcut::Tab((c.main.key_code - 0x31) as usize), // 1-8
        0x39 if only(primary) => Shortcut::LastTab,                                 // 9
        0x25 if only(MOD_ALT) || only(primary) && cfg!(target_os = "macos") => Shortcut::Back, // Left
        0x27 if only(MOD_ALT) || only(primary) && cfg!(target_os = "macos") => Shortcut::Forward,
        0xDB if only(primary) && cfg!(target_os = "macos") => Shortcut::Back,      // [
        0xDD if only(primary) && cfg!(target_os = "macos") => Shortcut::Forward,   // ]
        0x74 if m == 0 => Shortcut::Reload,                                         // F5
        0x52 if only(primary) => Shortcut::Reload,                                  // R
        _ => return None,
    })
}

#[cfg(test)]
mod tests_support {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static SPAWNED: Mutex<Vec<(PathBuf, u32)>> = Mutex::new(Vec::new());

    /// One browser start at a time across all tests.
    pub static STARTUP_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    pub fn record_spawn(dir: &Path, pid: u32) {
        SPAWNED.lock().unwrap_or_else(|e| e.into_inner()).push((dir.to_path_buf(), pid));
    }

    #[allow(dead_code)]
    pub fn spawned_pid(dir: &Path) -> Option<u32> {
        let v = SPAWNED.lock().unwrap_or_else(|e| e.into_inner());
        v.iter().rev().find(|(d, _)| d == dir).map(|(_, p)| *p)
    }
}

#[cfg(all(test, windows))]
mod win_tests {
    use super::*;
    use crate::platform::fsx::remove_dir_all_retry;
    use crate::platform::proc::{group_pids, pid_alive};
    use std::sync::atomic::AtomicUsize;

    /// Upper bound for "this call must not hang" checks. It only has to tell a hang from a slow call, so it
    /// is generous: a loaded CI runner takes seconds over what a developer machine takes milliseconds for.
    const HANG_GUARD: Duration = Duration::from_secs(20);

    async fn all_gone(pids: &[u32]) -> bool {
        for _ in 0..60 {
            if pids.iter().all(|&p| !pid_alive(p)) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// If the agent is removed (aborted) during startup (waiting for DevToolsActivePort), the browser
    /// and its descendants must not be left behind.
    ///
    /// Same scenario as the Unix abort_during_spawn_kills_browser_group: checks the Job object cleans
    /// up.
    #[tokio::test]
    async fn abort_during_spawn_kills_browser_job() {
        let dir = std::env::temp_dir().join(format!("ghostdesk-spawn-abort-{}", std::process::id()));
        let _ = remove_dir_all_retry(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // fake browser: starts a grandchild (ping) and waits a long time, never writing the port file
        let exe = dir.join("fake-browser.cmd");
        std::fs::write(&exe, "@echo off\r\nping -n 120 127.0.0.1 >NUL\r\n").unwrap();
        let data = dir.join("data");
        let opts = LaunchOptions {
            exe,
            user_data_dir: data.clone(),
            profile_dir: None,
            width: 100,
            height: 100,
            headless: true,
            start_url: String::new(),
        };
        let task = tokio::spawn(async move { BrowserWorkspace::spawn(&opts).await.map(|_| ()) });
        let mut main = None;
        let mut pids: Vec<u32> = Vec::new();
        for _ in 0..200 {
            if main.is_none() {
                main = tests_support::spawned_pid(&data);
            }
            if let Some(p) = main {
                pids = group_pids(p).unwrap_or_default();
                if pids.len() >= 2 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!task.is_finished(), "가짜 브라우저 실행 실패: {:?}", task.await);
        assert!(pids.len() >= 2, "가짜 브라우저와 손자 프로세스가 Job 에 있어야 함: {pids:?}");
        assert!(pids.iter().all(|&p| pid_alive(p)));
        task.abort();
        let _ = task.await;
        assert!(all_gone(&pids).await, "abort 뒤에도 브라우저 프로세스가 남아 있음: {pids:?}");
        assert!(group_pids(main.unwrap()).is_none(), "Job 핸들을 닫아야 함");
        remove_dir_all_retry(&dir).unwrap();
    }

    fn fake_opts(dir: &std::path::Path, script: &str) -> LaunchOptions {
        let _ = remove_dir_all_retry(dir);
        std::fs::create_dir_all(dir).unwrap();
        let exe = dir.join("fake-browser.cmd");
        std::fs::write(&exe, script).unwrap();
        LaunchOptions {
            exe,
            user_data_dir: dir.join("data"),
            profile_dir: None,
            width: 100,
            height: 100,
            headless: true,
            start_url: String::new(),
        }
    }

    #[tokio::test]
    async fn real_browser_dialog_can_be_cancelled_and_prompt_answered_without_stalling() {
        let Some(exe) = ["chrome.exe", "msedge.exe"].iter().find_map(|e| crate::platform::win::app_path(e)) else { return };
        let dir = std::env::temp_dir().join(format!("ghostdesk-dialog-test-{}", std::process::id()));
        let _ = remove_dir_all_retry(&dir);
        let ws = BrowserWorkspace::launch(LaunchOptions {
            exe, user_data_dir: dir.clone(), profile_dir: None, width: 640, height: 400, headless: true,
            start_url: r#"data:text/html,<button style='position:absolute;left:20px;top:20px;width:200px;height:40px' onclick='window.deleted=confirm("Delete?")'>Delete</button><button style='position:absolute;left:20px;top:80px;width:200px;height:40px' onclick='window.answer=prompt("Name?","seed")'>Name</button>"#.into(),
        }, Arc::new(|_| {})).await.unwrap();
        async fn pending(ws: &BrowserWorkspace) -> Value {
            tokio::time::timeout(HANG_GUARD, async {
                loop {
                    if let Some(dialog) = ws.dialog() { return dialog; }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.expect("JavaScript dialog event did not arrive")
        }
        let desk = crate::desk::Desk::Browser(ws.clone());
        for (y, kind) in [(40.0, "confirm"), (100.0, "prompt")] {
            let click = tokio::time::timeout(HANG_GUARD, ws.click(Some((100.0, y)), MouseButton::Left, 1, 0))
                .await.expect("click stalled behind a pending JavaScript dialog");
            if let Err(error) = click { assert!(error.to_string().contains("대화상자"), "{error:#}"); }
            let dialog = pending(&ws).await;
            assert_eq!(dialog["type"], kind);
            let id = dialog["id"].as_u64().unwrap();
            let queried = crate::agent::exec_desk_tool(&desk, "browser_control", &json!({"action":"dialog"})).await.unwrap();
            assert_eq!(serde_json::from_str::<Value>(&queried.text).unwrap()["id"], id);
            for invalid in [json!({"action":"respond_dialog", "id":id}), json!({"action":"respond_dialog", "id":id, "accept":"false"})] {
                assert!(crate::agent::exec_desk_tool(&desk, "browser_control", &invalid).await.is_err());
                assert_eq!(ws.dialog().unwrap()["id"], id, "missing/invalid accept must not answer the dialog");
            }
            assert!(ws.respond_dialog(id + 1, true, None).await.is_err());
            assert!(tokio::time::timeout(HANG_GUARD, ws.screenshot()).await.unwrap().is_err());
            let mut reply = json!({"action":"respond_dialog", "id":id, "accept":kind == "prompt"});
            if kind == "prompt" { reply["prompt_text"] = json!("재용👋"); }
            crate::agent::exec_desk_tool(&desk, "browser_control", &reply).await.unwrap();
        }
        assert_eq!(ws.eval("window.deleted").await.unwrap(), false);
        assert_eq!(ws.eval("window.answer").await.unwrap(), "재용👋");
        let evaluation = tokio::time::timeout(HANG_GUARD, ws.eval("window.third = confirm('From eval')"))
            .await.expect("evaluation stalled behind its JavaScript dialog");
        assert!(evaluation.is_err());
        let dialog = pending(&ws).await;
        ws.respond_dialog(dialog["id"].as_u64().unwrap(), false, None).await.unwrap();
        assert_eq!(ws.eval("window.third").await.unwrap(), false);
        ws.shutdown().await;
        remove_dir_all_retry(&dir).unwrap();
    }

    /// A search box that a page swaps for an identical new node once typed into (as Wikipedia's does) keeps
    /// the number the agent read it under.
    #[tokio::test]
    async fn real_browser_ref_survives_a_replaced_search_box() {
        let Some(exe) = ["chrome.exe", "msedge.exe"].iter().find_map(|e| crate::platform::win::app_path(e)) else { return };
        let dir = std::env::temp_dir().join(format!("ghostdesk-ref-test-{}", std::process::id()));
        let _ = remove_dir_all_retry(&dir);
        let page = r#"data:text/html,<a href='x'>Home</a><form onsubmit='window.sent=this.q.value;return false'><input name=q placeholder=Search oninput='if(!window.swapped){window.swapped=1;const n=this.cloneNode();n.value=this.value;n.oninput=null;n.removeAttribute("data-gdref");this.replaceWith(n)}'></form><input name=other placeholder=Other>"#;
        let ws = BrowserWorkspace::launch(LaunchOptions {
            exe, user_data_dir: dir.clone(), profile_dir: None, width: 640, height: 400, headless: true, start_url: page.into(),
        }, Arc::new(|_| {})).await.unwrap();
        let desk = crate::desk::Desk::Browser(ws.clone());
        let run = |v: Value| { let desk = desk.clone(); async move { crate::agent::exec_desk_tool(&desk, "browser_control", &v).await } };
        run(json!({"action":"wait_for", "text": "Home"})).await.unwrap();
        let read = run(json!({"action":"read"})).await.unwrap().text;
        let line = read.lines().find(|l| l.trim_start().starts_with('[') && l.contains("Search")).unwrap_or_else(|| panic!("{read}"));
        let n: u64 = line.trim_start().trim_start_matches('[').split(']').next().unwrap().parse().unwrap();
        run(json!({"action":"fill", "ref": n, "value": "한글"})).await.unwrap();
        assert_eq!(ws.eval("window.swapped").await.unwrap(), 1, "the page did not replace its box");
        run(json!({"action":"fill", "ref": n, "value": "한글 검색", "submit": true})).await.unwrap();
        assert_eq!(ws.eval("window.sent").await.unwrap(), "한글 검색");
        ws.shutdown().await;
        remove_dir_all_retry(&dir).unwrap();
    }

    /// If the executable exits right away leaving nothing behind (e.g. it only handed the command
    /// line to a browser using the same workspace), report it immediately instead of waiting 40 s.
    #[tokio::test]
    async fn immediate_exit_is_reported() {
        let dir = std::env::temp_dir().join(format!("ghostdesk-spawn-exit-{}", std::process::id()));
        let opts = fake_opts(&dir, "@echo off\r\nexit /b 0\r\n");
        let t = Instant::now();
        let err = BrowserWorkspace::spawn(&opts).await.err().expect("실패해야 함");
        assert!(err.to_string().contains("바로 종료"), "{err:#}");
        assert!(t.elapsed() < Duration::from_secs(10));
        remove_dir_all_retry(&dir).unwrap();
    }

    /// A launcher executable (main process exits right away, the real browser stays as a child) keeps
    /// the wait going, and cancelling also cleans up the remaining child.
    #[tokio::test]
    async fn launcher_handoff_keeps_waiting_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("ghostdesk-spawn-launcher-{}", std::process::id()));
        // start /b: launch ping without waiting, then cmd exits
        let opts = fake_opts(&dir, "@echo off\r\nstart \"\" /b ping -n 120 127.0.0.1 >NUL\r\nexit /b 0\r\n");
        let data = opts.user_data_dir.clone();
        let task = tokio::spawn(async move { BrowserWorkspace::spawn(&opts).await.map(|_| ()) });
        let mut main = None;
        let mut pids: Vec<u32> = Vec::new();
        for _ in 0..200 {
            if main.is_none() {
                main = tests_support::spawned_pid(&data);
            }
            if let Some(m) = main {
                pids = group_pids(m).unwrap_or_default();
                // the launcher (cmd) has exited and only the child is left in the Job
                if !pids.is_empty() && !pids.contains(&m) {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let m = main.expect("pid");
        assert!(!pids.is_empty() && !pids.contains(&m), "런처가 끝나고 자식만 남아야 함: {pids:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!task.is_finished(), "런처가 끝났다고 바로 실패하면 안 됨: {:?}", task.await);
        task.abort();
        let _ = task.await;
        assert!(all_gone(&pids).await, "취소 뒤에도 남은 프로세스: {pids:?}");
        assert!(group_pids(m).is_none());
        remove_dir_all_retry(&dir).unwrap();
    }

    /// Real Chrome/Edge (empty temp profile, headless): launches and connects from paths with spaces
    /// and Korean characters, and after shutdown every helper process has exited so the workspace
    /// folder can be deleted.
    #[tokio::test]
    async fn real_browser_shutdown_leaves_nothing() {
        // only look up the executable location (the user's profile isn't read)
        let Some(exe) = ["chrome.exe", "msedge.exe"].iter().find_map(|e| crate::platform::win::app_path(e)) else {
            eprintln!("Chrome/Edge 가 없어 건너뜀");
            return;
        };
        let dir = std::env::temp_dir().join(format!("ghostdesk 브라우저 테스트-{}", std::process::id()));
        let _ = remove_dir_all_retry(&dir);
        let frames = Arc::new(AtomicUsize::new(0));
        let f2 = frames.clone();
        let ws = BrowserWorkspace::launch(
            LaunchOptions {
                exe: exe.clone(),
                user_data_dir: dir.clone(),
                profile_dir: None,
                width: 640,
                height: 400,
                headless: true,
                start_url: "data:text/html;charset=utf-8,<input id=a autofocus><p id=p>안녕</p>".into(),
            },
            Arc::new(move |_b: &[u8]| {
                f2.fetch_add(1, Ordering::Relaxed);
            }),
        )
        .await
        .unwrap_or_else(|e| panic!("{} 실행 실패: {e:#}", exe.display()));
        let pid = ws.browser_pid().expect("pid");
        let pids = group_pids(pid).expect("Job 이 있어야 함");
        assert!(pids.contains(&pid) && pids.len() >= 2, "헬퍼까지 Job 에 있어야 함: {pids:?}");
        assert_eq!(ws.eval("document.getElementById('p').textContent").await.unwrap(), "안녕");
        // internal shortcuts must use ctrl on Windows
        ws.click(Some((20.0, 10.0)), MouseButton::Left, 1, 0).await.unwrap();
        ws.type_text("abc 가나").await.unwrap();
        ws.key("ctrl+a", 1).await.unwrap();
        ws.key("BackSpace", 1).await.unwrap();
        assert_eq!(ws.eval("document.getElementById('a').value").await.unwrap(), "");
        ws.shutdown().await;
        assert!(group_pids(pid).is_none(), "shutdown 뒤에는 Job 을 닫아야 함");
        assert!(all_gone(&pids).await, "shutdown 뒤에도 남은 프로세스: {pids:?}");
        remove_dir_all_retry(&dir).expect("작업공간 폴더를 지울 수 있어야 함");
        assert!(!dir.exists());
    }

    /// Drag interception mode (what Windows Electron workspaces use) on real Chrome (empty temp
    /// profile, headless): HTML5 drags are caught by Input.dragIntercepted and dropped inside the page
    /// with the right data, and later input and screenshots don't hang. Remote file:// URLs must be
    /// refused.
    #[tokio::test]
    async fn real_browser_intercepted_drag_drops_in_page() {
        let Some(exe) = ["chrome.exe", "msedge.exe"].iter().find_map(|e| crate::platform::win::app_path(e)) else {
            eprintln!("Chrome/Edge 가 없어 건너뜀");
            return;
        };
        let t0 = Instant::now();
        let dir = std::env::temp_dir().join(format!("ghostdesk-drag-test-{}", std::process::id()));
        let _ = remove_dir_all_retry(&dir);
        let page = "data:text/html;charset=utf-8,<style>body{margin:0}div{position:absolute}</style>\
            <div id=src draggable=true style='left:20px;top:20px;width:100px;height:50px;background:red'>drag</div>\
            <div id=dst style='left:300px;top:20px;width:150px;height:150px;background:blue'></div>\
            <div id=btn style='left:20px;top:250px;width:100px;height:40px;background:green'></div>\
            <script>window.log=[];window.clicks=0;window.ups=0;\
            src.addEventListener('dragstart',e=>{e.dataTransfer.setData('text/plain','hello-drag');e.dataTransfer.setData('application/x-gd','custom');log.push('dragstart')});\
            src.addEventListener('dragend',e=>log.push('dragend:'+e.dataTransfer.dropEffect));\
            dst.addEventListener('dragenter',e=>{e.preventDefault();log.push('dragenter')});\
            dst.addEventListener('dragover',e=>e.preventDefault());\
            dst.addEventListener('drop',e=>{e.preventDefault();log.push('drop:'+e.dataTransfer.getData('text/plain')+':'+e.dataTransfer.getData('application/x-gd'))});\
            btn.addEventListener('click',()=>clicks++);document.addEventListener('mouseup',()=>ups++);</script>";
        let opts = LaunchOptions {
            exe: exe.clone(),
            user_data_dir: dir.clone(),
            profile_dir: None,
            width: 640,
            height: 400,
            headless: true,
            start_url: String::new(),
        };
        let (child, ws_url) = BrowserWorkspace::spawn(&opts).await.expect("브라우저 실행");
        let ws = BrowserWorkspace::connect(
            &ws_url,
            Some(child),
            ConnectOptions {
                width: 640,
                height: 400,
                app_mode: false,
                fix_headless_ua: true,
                start_url: page.into(),
                page_wait: Duration::from_secs(0),
                intercept_drags: true,
            },
            Arc::new(|_b: &[u8]| {}),
        )
        .await
        .expect("연결");
        fn to<F: std::future::Future>(f: F) -> tokio::time::Timeout<F> {
            tokio::time::timeout(Duration::from_secs(10), f)
        }
        let log = || async { ws.eval("JSON.stringify(log)").await.unwrap().as_str().unwrap_or_default().to_string() };

        // 1) one-shot drag: dropped at the target with the data intact
        to(ws.drag((70.0, 45.0), (375.0, 95.0), 0)).await.expect("drag 가 멈춤").unwrap();
        let l = log().await;
        assert!(l.contains("\"dragstart\"") && l.contains("\"dragenter\"") && l.contains("drop:hello-drag:custom"), "{l}");
        // clicks still work after the drag (mouse state isn't stuck)
        to(ws.click(Some((70.0, 270.0)), MouseButton::Left, 1, 0)).await.unwrap().unwrap();
        assert_eq!(ws.eval("clicks").await.unwrap(), json!(1));

        // 2) step by step: press → move → (mid-drag) screenshot → release
        ws.eval("log.length = 0").await.unwrap();
        to(ws.mouse_move(70.0, 45.0, 0)).await.unwrap().unwrap();
        to(ws.mouse_down(MouseButton::Left, 1, 0)).await.unwrap().unwrap();
        for (x, y) in [(120.0, 50.0), (250.0, 70.0), (380.0, 100.0)] {
            to(ws.mouse_move(x, y, 0)).await.expect("move 가 멈춤").unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let png = to(ws.screenshot()).await.expect("드래그 중 스크린샷이 멈춤").unwrap();
        assert_eq!(png_size(&png), Some((640, 400)));
        to(ws.mouse_up(MouseButton::Left, 1, 0)).await.expect("mouse_up 이 멈춤").unwrap();
        let l = log().await;
        assert!(l.contains("drop:hello-drag:custom"), "{l}");

        // 3) releasing over a non-drop target ends without a drop (dragend: none) and the next click works
        ws.eval("log.length = 0").await.unwrap();
        to(ws.drag((70.0, 45.0), (560.0, 340.0), 0)).await.unwrap().unwrap();
        let l = log().await;
        assert!(l.contains("dragend:none") && !l.contains("drop:"), "{l}");
        to(ws.click(Some((70.0, 270.0)), MouseButton::Left, 1, 0)).await.unwrap().unwrap();
        assert_eq!(ws.eval("clicks").await.unwrap(), json!(2));

        // 4) a press-and-move that isn't a drag (empty area) delivers mouseReleased as usual
        let ups = ws.eval("ups").await.unwrap().as_i64().unwrap();
        to(ws.drag((560.0, 300.0), (600.0, 360.0), 0)).await.unwrap().unwrap();
        assert_eq!(ws.eval("ups").await.unwrap().as_i64().unwrap(), ups + 1);

        // 5) remote file:// URLs and UNC paths aren't opened
        for u in ["file://example.invalid/share/x.png", r"\\example.invalid\share\x.png"] {
            let e = ws.navigate(u).await.expect_err(u);
            assert!(e.to_string().contains("NTLM"), "{e:#}");
        }
        ws.shutdown().await;
        let _ = remove_dir_all_retry(&dir);
        eprintln!("드래그 가로채기 테스트: {:?}", t0.elapsed());
        // (4-7 s on its own. Each action has a 10 s limit to catch hangs, so this only leaves slack for
        // running alongside other tests)
        assert!(t0.elapsed() < Duration::from_secs(60), "너무 오래 걸림: {:?}", t0.elapsed());
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn remote_file_urls_are_refused() {
        let bad = [
            "file://attacker.example/x/y.png",
            "FILE://Attacker.Example/x",
            "file:////attacker.example/x",
            "file://///attacker.example/x",
            r"file:\\attacker.example\x",
            "file:/attacker.example/x",
            "file:attacker.example/x",
            "file://127.0.0.1/c$/x",
            "file://%61ttacker/x",
            "file://localhost//attacker.example/x",
            " \tfile://attacker/x",
            "view-source:file://attacker/x",
            "fi\tle://attacker/x",
        ];
        for u in bad {
            let n = normalize_url(u);
            assert!(remote_file_refusal(u, &n).is_some(), "{u} → {n} 는 거부해야 함");
        }
        for u in [r"\\attacker.example\share\x", "//attacker.example/share", r"\\?\UNC\attacker\share"] {
            let r = remote_file_refusal(u, &normalize_url(u)).unwrap_or_else(|| panic!("{u} 는 거부해야 함"));
            assert!(r.contains("UNC") && r.contains("NTLM"), "{r}");
        }
        let ok = [
            "file:///C:/Users/a/x.html",
            "file:///c|/x",
            "file://localhost/C:/x",
            "file://LOCALHOST/c:/x",
            "file:C:/x",
            "file:////C:/x",
            "file:///tmp/x",
            "file:///",
            "https://example.com/file://x",
            "example.com",
            "about:blank",
            "data:text/html,<a href=file://x/y>",
            "검색어 file://x",
            "http://localhost:8080/",
        ];
        for u in ok {
            let n = normalize_url(u);
            assert!(remote_file_refusal(u, &n).is_none(), "{u} → {n} 는 허용해야 함");
        }
        assert_eq!(percent_decode("a%41%zz%4"), "aA%zz%4");
        assert_eq!(percent_decode("%ED%95%9C"), "한");
    }
}

#[cfg(test)]
mod text_tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};

    async fn mock_workspace() -> (BrowserWorkspace, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let events = Arc::new(Mutex::new(Vec::new()));
        let received = events.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = socket.next().await {
                let event: Value = serde_json::from_str(&text).unwrap();
                received.lock().push(event.clone());
                if event["method"] == "Page.screencastFrameAck" {
                    // Chromium may defer this reply until a JavaScript dialog is dismissed.
                    continue;
                }
                socket.send(tokio_tungstenite::tungstenite::Message::Text(json!({"id":event["id"],"result":{}}).to_string().into())).await.unwrap();
            }
        });
        let mut state = State::default();
        state.active = Some("page".into());
        state.sessions.insert("page".into(), "session".into());
        let ws = BrowserWorkspace {
            cdp: Cdp::connect(&url).await.unwrap(), child: Mutex::new(None), width: 640, height: 400,
            user_agent: None, app_mode: false, intercept_drags: false, state: Mutex::new(state),
            ready: AtomicBool::new(true), closed: AtomicBool::new(false), sink: Arc::new(|_| {}), dialog_changed: tokio::sync::Notify::new(),
        };
        (ws, events, server)
    }

    #[tokio::test]
    async fn a_delayed_frame_ack_does_not_block_dialog_events() {
        let (ws, events, server) = mock_workspace().await;
        tokio::time::timeout(Duration::from_secs(2), async {
            ws.handle_event(CdpEvent {
                method: "Page.screencastFrame".into(), params: json!({"sessionId":7}),
                session_id: Some("session".into()),
            }).await.unwrap();
            ws.handle_event(CdpEvent {
                method: "Page.javascriptDialogOpening".into(), params: json!({"type":"confirm"}),
                session_id: Some("session".into()),
            }).await.unwrap();
            assert_eq!(ws.dialog().unwrap()["type"], "confirm");
            while events.lock().is_empty() { tokio::task::yield_now().await; }
            assert_eq!(events.lock()[0]["params"]["sessionId"], 7);
        }).await.expect("frame acknowledgement blocked the dialog");
        server.abort();
    }

    #[tokio::test]
    async fn javascript_dialog_waits_for_explicit_response() {
        let (ws, events, server) = mock_workspace().await;
        ws.handle_event(CdpEvent {
            method: "Page.javascriptDialogOpening".into(),
            params: json!({"type":"confirm", "message":"Delete this?", "url":"https://example.test"}),
            session_id: Some("session".into()),
        }).await.unwrap();
        assert!(events.lock().is_empty(), "a confirmation must not be accepted automatically");
        let first = ws.dialog().unwrap();
        let id = first["id"].as_u64().unwrap();
        assert_eq!(first["message"], "Delete this?");
        assert!(ws.respond_dialog(id + 1, true, None).await.is_err());
        assert!(events.lock().is_empty(), "a stale response must not reach CDP");
        ws.respond_dialog(id, false, None).await.unwrap();
        assert!(ws.dialog().is_none());
        assert_eq!(events.lock()[0]["params"], json!({"accept":false}));
        ws.handle_event(CdpEvent {
            method: "Page.javascriptDialogOpening".into(),
            params: json!({"type":"prompt", "message":"Name?", "defaultPrompt":"seed"}),
            session_id: Some("session".into()),
        }).await.unwrap();
        let second = ws.dialog().unwrap();
        assert_eq!(second["defaultPrompt"], "seed");
        assert_ne!(first["id"], second["id"]);
        assert!(ws.respond_dialog(id, true, None).await.is_err());
        ws.respond_dialog(second["id"].as_u64().unwrap(), true, Some("재용👋")).await.unwrap();
        assert_eq!(events.lock()[1]["params"], json!({"accept":true,"promptText":"재용👋"}));
        server.abort();
    }

    #[tokio::test]
    async fn crlf_types_one_return_and_preserves_other_text() {
        let (ws, events, server) = mock_workspace().await;
        ws.type_text("a\r\nb\rc\n가\t👋").await.unwrap();
        let trace: Vec<String> = events.lock().iter().map(|event| {
            let p = &event["params"];
            match event["method"].as_str().unwrap() {
                "Input.insertText" => format!("text:{}", p["text"].as_str().unwrap()),
                "Input.dispatchKeyEvent" => format!("{}:{}", if p["type"] == "keyUp" { "up" } else { "down" }, p["key"].as_str().unwrap()),
                other => panic!("unexpected command: {other}"),
            }
        }).collect();
        server.abort();
        assert_eq!(trace, [
            "down:a", "up:a", "down:Enter", "up:Enter", "down:b", "up:b", "down:Enter", "up:Enter",
            "down:c", "up:c", "down:Enter", "up:Enter", "text:가", "down:Tab", "up:Tab", "text:👋",
        ]);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// If the agent is removed (aborted) during startup (waiting for DevToolsActivePort), Chromium and
    /// its children must not be left behind.
    #[tokio::test]
    async fn abort_during_spawn_kills_browser_group() {
        let dir = std::env::temp_dir().join(format!("ghostdesk-spawn-abort-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // fake browser: starts one child, writes both pids, then waits forever without writing the port
        // file
        let exe = dir.join("fake-browser.sh");
        let pids = dir.join("pids");
        std::fs::write(
            &exe,
            format!("#!/bin/sh\nsleep 60 &\necho $$ $! > '{}'\nwait\n", pids.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let opts = LaunchOptions {
            exe,
            user_data_dir: dir.join("data"),
            profile_dir: None,
            width: 100,
            height: 100,
            headless: true,
            start_url: String::new(),
        };
        let task = tokio::spawn(async move { BrowserWorkspace::spawn(&opts).await.map(|_| ()) });
        let mut ids: Vec<i32> = Vec::new();
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&pids) {
                ids = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                if ids.len() == 2 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(ids.len(), 2, "가짜 브라우저가 시작되지 않음");
        assert!(ids.iter().all(|&p| alive(p)));
        task.abort();
        let _ = task.await;
        let mut gone = false;
        for _ in 0..60 {
            if ids.iter().all(|&p| !alive(p)) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(gone, "abort 뒤에도 브라우저 프로세스가 남아 있음: {ids:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod shortcut_tests {
    use super::*;

    #[test]
    fn browser_window_shortcuts_are_recognized() {
        let sc = |k: &str| browser_shortcut(&keys::parse_combo(k).unwrap());
        let p = if cfg!(target_os = "macos") { "cmd" } else { "ctrl" };
        assert_eq!(sc(&format!("{p}+t")), Some(Shortcut::NewTab));
        assert_eq!(sc(&format!("{p}+w")), Some(Shortcut::CloseTab));
        assert_eq!(sc("ctrl+tab"), Some(Shortcut::NextTab));
        assert_eq!(sc("ctrl+shift+tab"), Some(Shortcut::PrevTab));
        assert_eq!(sc(&format!("{p}+3")), Some(Shortcut::Tab(2)));
        assert_eq!(sc(&format!("{p}+9")), Some(Shortcut::LastTab));
        assert_eq!(sc("alt+left"), Some(Shortcut::Back));
        assert_eq!(sc("F5"), Some(Shortcut::Reload));
        // page shortcuts still go to the page
        for k in [format!("{p}+c"), format!("{p}+a"), "tab".into(), "shift+tab".into(), "ctrl+shift+t".into(), "left".into()] {
            assert_eq!(sc(&k), None, "{k}");
        }
    }
}

/// Reading and driving a real page by element numbers across re-draws and page loads. Needs an installed Chrome
/// (it is skipped where there is none).
#[cfg(test)]
mod element_tests {
    use super::*;

    fn chrome() -> Option<PathBuf> {
        #[cfg(windows)]
        return ["chrome.exe", "msedge.exe"].iter().find_map(|e| crate::platform::win::app_path(e));
        #[cfg(target_os = "macos")]
        return ["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists());
        #[cfg(not(any(windows, target_os = "macos")))]
        return None;
    }

    /// A page with a search form whose box the page swaps for another node while it is used (as Wikipedia's does,
    /// with a new label and role), and a results page that takes a moment to arrive.
    async fn serve() -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let body = if path.starts_with("/search") {
                        tokio::time::sleep(Duration::from_millis(900)).await;
                        format!("<title>Results</title><h1>Results page</h1><p>{path}</p><a href='/'>Back home</a>")
                    } else {
                        "<title>Home</title><a href='/x'>Home link</a>\
                         <form action='/search'><input name=q placeholder='Search Wikipedia' \
                           oninput='if(!window.swapped){window.swapped=1;const n=this.cloneNode();n.value=this.value;n.oninput=null;n.setAttribute(\"role\",\"combobox\");n.placeholder=\"Search\";this.replaceWith(n)}'>\
                         <button>Go</button></form><input name=other placeholder=Other>"
                            .to_string()
                    };
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        port
    }

    fn number_of(read: &str, name: &str) -> u64 {
        let line = read.lines().find(|l| l.trim_start().starts_with('[') && l.contains(name)).unwrap_or_else(|| panic!("no '{name}' in:\n{read}"));
        line.trim_start().trim_start_matches('[').split(']').next().unwrap().parse().unwrap()
    }

    #[tokio::test]
    async fn element_numbers_hold_and_submits_wait_for_the_page_that_opens() {
        let Some(exe) = chrome() else { return };
        let port = serve().await;
        let dir = std::env::temp_dir().join(format!("ghostdesk-nav-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = BrowserWorkspace::launch(
            LaunchOptions { exe, user_data_dir: dir.clone(), profile_dir: None, width: 800, height: 500, headless: true, start_url: format!("http://127.0.0.1:{port}/") },
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
        let desk = crate::desk::Desk::Browser(ws.clone());
        let run = |v: Value| { let desk = desk.clone(); async move { crate::agent::exec_desk_tool(&desk, "browser_control", &v).await } };
        run(json!({"action":"wait_for", "text": "Home link"})).await.unwrap();

        let read = run(json!({"action":"read"})).await.unwrap().text;
        let (search, other) = (number_of(&read, "Search Wikipedia"), number_of(&read, "Other"));

        // typing makes the page swap the box (new label, new role): the number still reaches it
        run(json!({"action":"fill", "ref": search, "value": "한글"})).await.unwrap();
        assert_eq!(ws.eval("window.swapped").await.unwrap(), 1, "the page did not replace its box");
        // reading again keeps every number; the swapped box is the same element under the same number
        let again = run(json!({"action":"read"})).await.unwrap().text;
        assert_eq!(number_of(&again, "Search"), search, "{again}");
        assert_eq!(number_of(&again, "Other"), other, "{again}");
        // a number just read stays good when the page swaps the box once more behind the read
        ws.eval("(()=>{const o=document.querySelector('[name=q]');const n=o.cloneNode();n.value=o.value;n.placeholder='Search again';o.replaceWith(n)})()").await.unwrap();
        run(json!({"action":"fill", "ref": search, "value": "한글 검색"})).await.unwrap();
        assert_eq!(ws.eval("document.querySelector('[name=q]').value").await.unwrap(), "한글 검색");

        // Enter submits to a page that takes a moment: the same call waits for it, and says so
        let submitted = run(json!({"action":"fill", "ref": search, "value": "한글 검색", "submit": true})).await.unwrap().text;
        assert!(submitted.contains("페이지가 바뀌었어요") && submitted.contains("Results"), "{submitted}");
        // so the next read in the same bundle is about the new page
        let text = run(json!({"action":"text"})).await.unwrap().text;
        assert!(text.contains("Results page"), "{text}");

        // wait_nav = 0 does not wait: the old page is still there right after Enter
        run(json!({"action":"navigate", "url": format!("http://127.0.0.1:{port}/")})).await.unwrap();
        let read = run(json!({"action":"read"})).await.unwrap().text;
        let go = number_of(&read, "Go");
        let quick = run(json!({"action":"click", "ref": go, "wait_nav": 0})).await.unwrap().text;
        assert!(!quick.contains("페이지가 바뀌었어요"), "{quick}");
        // but any later action on the page waits out the load instead of failing halfway
        let text = run(json!({"action":"text"})).await.unwrap().text;
        assert!(text.contains("Results page"), "{text}");
        // a click that opens a page waits as well
        let back = run(json!({"action":"click", "label": "Back home"})).await.unwrap().text;
        assert!(back.contains("페이지가 바뀌었어요") && back.contains("Home"), "{back}");

        assert!(run(json!({"action":"click", "ref": 1, "wait_nav": "soon"})).await.is_err());
        ws.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
