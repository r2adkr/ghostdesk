//! Windows workspace for regular apps: the Windows implementation of
//! [`crate::native::NativeAppWorkspace`].
//!
//! - Mode A: launched hidden (SW_HIDE) and suspended into a Job, with its windows kept off-screen
//!   and shown via SW_SHOWNOACTIVATE.
//! - Mode B: attaches with consent to a running user app, parks its windows off-screen while
//!   working and puts them back when done.
//! - Screen = the current window's client area (PrintWindow). Views = the app's top-level windows
//!   (new dialogs are switched to automatically).
//! - Input = window messages (the user's cursor, focus and clipboard are untouched). Semantic
//!   control = UI Automation (`app_control`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use parking_lot::Mutex;
use serde_json::Value;

use super::capture;
use super::guard::{self, Guard};
use super::hdesk::{self, DeskHost};
use super::input::{self, Geom, KeyPlan};
use super::launch::{self, Proc};
use super::menu::{self, Accel};
use super::uia;
use super::w32::{self, Hwnd};
use crate::browser::{FrameSink, MouseButton};
use crate::desk::ViewInfo;
use crate::native::{NativeHost, NativeMode, NativeOptions};
use crate::platform::win as pw;

#[derive(Clone, Debug)]
struct WinEntry {
    hwnd: Hwnd,
    title: String,
    class: String,
    owner: Hwnd,
    size: (i32, i32),
    enabled: bool,
}

struct Down {
    target: Hwnd,
    top: Hwnd,
    mk: usize,
    /// Where the button went down (screenshot coordinates / coordinates relative to `target`).
    start: (f64, f64),
    pt: (i32, i32),
}

#[derive(Default)]
struct State {
    windows: Vec<WinEntry>,
    current: Option<Hwnd>,
    geom: Option<Geom>,
    mouse: (f64, f64),
    down: Option<Down>,
    notes: Vec<String>,
    ui_ids: Vec<uia::Node>,
    ui_for: Option<Hwnd>,
    /// Internal clipboard (the user's system clipboard is never used).
    clipboard: String,
    /// Last clicked control (keyboard target candidate).
    last_target: Option<Hwnd>,
    reclaim_noted: bool,
}

struct Inner {
    pid: u32,
    mode: NativeMode,
    app_name: String,
    opts: NativeOptions,
    proc_: Proc,
    guard: Mutex<Option<Guard>>,
    state: Mutex<State>,
    size: Mutex<(u32, u32)>,
    closed: AtomicBool,
    shut: AtomicBool,
    sink: FrameSink,
    exposure: Mutex<Vec<String>>,
    /// The hidden desktop and its worker thread, if launched there (`None` = off-screen window on the
    /// user's desktop, or an attached app).
    desk: Option<Arc<DeskHost>>,
    /// Mode A: processes of the same app already running before launch (for single-instance
    /// detection).
    before_same: Mutex<Vec<u32>>,
}

/// Workspace for regular Windows apps.
pub struct NativeAppWorkspace {
    inner: Arc<Inner>,
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| anyhow!("작업 스레드 오류: {e}"))
}

/// Runs window work: on the dedicated thread for a hidden desktop, otherwise on a tokio worker
/// thread.
async fn exec<T: Send + 'static>(desk: &Option<Arc<DeskHost>>, f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    match desk {
        Some(d) => d.thread.run_async(f).await,
        None => blocking(f).await,
    }
}

/// Executables that don't paint on a hidden desktop and must use the off-screen host
/// (remembered for this GhostDesk run).
fn offscreen_verdicts() -> &'static Mutex<std::collections::HashMap<String, String>> {
    static V: std::sync::OnceLock<Mutex<std::collections::HashMap<String, String>>> = std::sync::OnceLock::new();
    V.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn verdict_key(app: &str) -> String {
    super::apps::resolve_app_query(app).map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_else(|| app.to_lowercase())
}

/// Hidden desktop launch failure. With `retry_offscreen`, relaunching on the off-screen host is
/// worth a try.
struct LaunchFail {
    err: anyhow::Error,
    retry_offscreen: bool,
}

impl From<anyhow::Error> for LaunchFail {
    fn from(err: anyhow::Error) -> Self {
        LaunchFail { err, retry_offscreen: false }
    }
}

const RIGHT_ERR: &str = "오른쪽 클릭(컨텍스트 메뉴)은 할 수 없습니다 — Windows 는 컨텍스트 메뉴를 실제 마우스 커서 위치, 즉 사용자 화면에 그립니다. \
    메뉴 막대 항목은 app_control menu, 요소 동작은 app_control press 를 쓰세요.";

impl NativeAppWorkspace {
    /// Launches the app on a hidden desktop (default) or as an off-screen window (mode A), or
    /// attaches to a running app with consent (mode B).
    pub async fn launch(opts: NativeOptions, sink: FrameSink) -> Result<Arc<Self>> {
        if opts.host == NativeHost::CurrentSession && (opts.attach_pid.is_some() || opts.attach_consent) {
            bail!("current_session 은 기존 로그인으로 새로 실행하는 모드입니다. attach 없이 쓰고, 실행 중인 앱은 먼저 직접 종료하세요.");
        }
        // window coordinates and captures in physical pixels (ignored in the GUI, where winit already set
        // it)
        pw::set_dpi_aware();
        let attaching = opts.attach_pid.is_some();
        let key = verdict_key(&opts.app);
        let cached = offscreen_verdicts().lock().get(&key).cloned();
        let want_hidden = !attaching
            && match opts.host {
                NativeHost::Offscreen => false,
                NativeHost::Hidden | NativeHost::CurrentSession => true,
                NativeHost::Auto => cached.is_none(),
            };
        let mut fallback_note = None;
        if want_hidden {
            match DeskHost::create() {
                Ok(host) => match Self::launch_on(opts.clone(), sink.clone(), Some(Arc::new(host))).await {
                    Ok(inner) => return Ok(Self::finish(inner)),
                    Err(f) if f.retry_offscreen && opts.host == NativeHost::Auto => {
                        eprintln!("[ghostdesk] 숨은 데스크톱 실행 실패 → 화면 밖 호스트로 다시 실행: {:#}", f.err);
                        offscreen_verdicts().lock().insert(key.clone(), format!("{:#}", f.err));
                        fallback_note = Some(format!(
                            "이 앱은 숨은 데스크톱에서 화면을 그리지 못해(GPU 'blt' 방식 출력 등) 사용자 데스크톱의 화면 밖 창으로 다시 띄웠습니다: {:#}",
                            f.err
                        ));
                    }
                    Err(f) => return Err(f.err),
                },
                Err(e) => {
                    if matches!(opts.host, NativeHost::Hidden | NativeHost::CurrentSession) {
                        return Err(e);
                    }
                    eprintln!("[ghostdesk] 숨은 데스크톱을 만들지 못해 화면 밖 호스트 사용: {e:#}");
                }
            }
        } else if let (Some(why), false) = (cached, attaching) {
            if opts.host == NativeHost::Auto {
                fallback_note = Some(format!("이 앱은 앞서 숨은 데스크톱에서 그려지지 않아 화면 밖 창으로 띄웠습니다: {why}"));
            }
        }
        let inner = Self::launch_on(opts, sink, None).await.map_err(|f| f.err)?;
        if let Some(n) = fallback_note {
            inner.note(n);
        }
        Ok(Self::finish(inner))
    }

    fn finish(inner: Arc<Inner>) -> Arc<Self> {
        let ws = Arc::new(NativeAppWorkspace { inner: inner.clone() });
        if inner.opts.live_fps > 0.0 {
            tokio::spawn(live_loop(Arc::downgrade(&inner), inner.opts.live_fps));
        }
        ws
    }

    async fn launch_on(opts: NativeOptions, sink: FrameSink, desk: Option<Arc<DeskHost>>) -> std::result::Result<Arc<Inner>, LaunchFail> {
        // launch on a regular worker thread (the attach decision has to look at user desktop windows).
        // If this future is cancelled, dropping the result (Arc<Inner>) runs Inner::drop → shutdown_sync,
        // so no launched process is left behind.
        let d2 = desk.clone();
        let inner = blocking(move || -> Result<Arc<Inner>> {
            let st = start_app(&opts, d2.as_deref())?;
            let desk = if st.mode == NativeMode::Launched { d2 } else { None };
            Ok(Arc::new(Inner {
                pid: st.proc_.pid,
                mode: st.mode,
                app_name: st.name,
                opts,
                proc_: st.proc_,
                guard: Mutex::new(Some(st.guard)),
                state: Mutex::new(State::default()),
                size: Mutex::new((1280, 800)),
                closed: AtomicBool::new(false),
                shut: AtomicBool::new(false),
                sink,
                exposure: Mutex::new(vec![]),
                desk,
                before_same: Mutex::new(st.before_same),
            }))
        })
        .await??;
        // wait for the first window and check rendering on the thread of the desktop that has the windows
        let i2 = inner.clone();
        let hidden = inner.desk.is_some();
        let r = exec(&inner.desk.clone(), move || -> std::result::Result<(), LaunchFail> {
            let before = i2.before_same.lock().clone();
            match i2.wait_first_window(before) {
                Ok(()) => {}
                // exited without a window on the hidden desktop: may be an app that can't create a device there,
                // like legacy D3D9
                Err(e) if hidden && i2.process_gone() => return Err(LaunchFail { err: e, retry_offscreen: true }),
                Err(e) => return Err(e.into()),
            }
            if hidden {
                if let Some(why) = i2.gpu_frames_lost() {
                    return Err(LaunchFail { err: anyhow!(why), retry_offscreen: true });
                }
                i2.check_escaped();
            }
            i2.guard_exposure();
            Ok(())
        })
        .await
        .map_err(LaunchFail::from)?;
        if let Err(f) = r {
            let i3 = inner.clone();
            let _ = exec(&inner.desk.clone(), move || i3.shutdown_sync()).await;
            return Err(f);
        }
        Ok(inner)
    }

    pub fn pid(&self) -> i32 {
        self.inner.pid as i32
    }

    pub fn mode(&self) -> NativeMode {
        self.inner.mode
    }

    pub fn app_name(&self) -> String {
        self.inner.app_name.clone()
    }

    /// Window exposure and focus events measured since launch (empty = no exposure).
    pub fn exposure_report(&self) -> Vec<String> {
        self.inner.collect_guard_events();
        self.inner.exposure.lock().clone()
    }

    /// Top-level windows of the app's processes (enumerated on the app's desktop), for self-tests
    /// and diagnostics.
    pub fn app_top_windows(&self) -> Vec<pw::TopWindow> {
        let pids = self.inner.pids();
        let f = move || pw::windows_of(&pids);
        match &self.inner.desk {
            Some(d) => d.thread.run(f).unwrap_or_default(),
            None => f(),
        }
    }

    /// Whether it runs on a hidden desktop.
    pub fn on_hidden_desktop(&self) -> bool {
        self.inner.desk.is_some()
    }

    pub fn size(&self) -> (u32, u32) {
        *self.inner.size.lock()
    }

    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        let i = self.inner.clone();
        let png = exec(&self.inner.desk, move || i.screenshot_png()).await??;
        (self.inner.sink)(&png);
        Ok(png)
    }

    pub async fn zoom(&self, region: [f64; 4]) -> Result<Vec<u8>> {
        let i = self.inner.clone();
        exec(&self.inner.desk, move || i.zoom_png(region)).await?
    }

    pub async fn mouse_move(&self, x: f64, y: f64, mods: i64) -> Result<()> {
        let dragging = {
            let mut st = self.inner.state.lock();
            st.mouse = (x, y);
            st.down.is_some()
        };
        if dragging {
            let i = self.inner.clone();
            exec(&self.inner.desk, move || i.drag_move(x, y, mods)).await??;
        }
        Ok(())
    }

    pub async fn mouse_down(&self, b: MouseButton, _count: i64, mods: i64) -> Result<()> {
        check_button(b)?;
        let i = self.inner.clone();
        exec(&self.inner.desk, move || i.button(true, mods)).await?
    }

    pub async fn mouse_up(&self, b: MouseButton, _count: i64, mods: i64) -> Result<()> {
        check_button(b)?;
        let i = self.inner.clone();
        exec(&self.inner.desk, move || i.button(false, mods)).await?
    }

    pub async fn click(&self, pos: Option<(f64, f64)>, b: MouseButton, count: u32, mods: i64) -> Result<()> {
        let pos = pos.unwrap_or_else(|| self.inner.state.lock().mouse);
        self.inner.state.lock().mouse = pos;
        check_button(b)?;
        let i = self.inner.clone();
        let note = exec(&self.inner.desk, move || i.click(pos.0, pos.1, count.max(1), mods)).await??;
        self.inner.note(note);
        Ok(())
    }

    pub async fn drag(&self, from: (f64, f64), to: (f64, f64), mods: i64) -> Result<()> {
        let i = self.inner.clone();
        let note = exec(&self.inner.desk, move || i.drag(from, to, mods)).await??;
        self.inner.note(note);
        Ok(())
    }

    pub async fn scroll(&self, pos: Option<(f64, f64)>, dx: f64, dy: f64, mods: i64) -> Result<()> {
        let pos = pos.unwrap_or_else(|| self.inner.state.lock().mouse);
        let i = self.inner.clone();
        let note = exec(&self.inner.desk, move || i.scroll(pos.0, pos.1, dx, dy, mods)).await??;
        self.inner.note(note);
        Ok(())
    }

    pub async fn key(&self, combo: &str, repeat: u32) -> Result<()> {
        let i = self.inner.clone();
        let combo = combo.to_string();
        let note = exec(&self.inner.desk, move || i.key_combo(&combo, repeat.clamp(1, 100), Duration::ZERO)).await??;
        if let Some(n) = note {
            self.inner.note(n);
        }
        Ok(())
    }

    pub async fn hold_key(&self, combo: &str, secs: f64) -> Result<()> {
        let i = self.inner.clone();
        let combo = combo.to_string();
        let hold = Duration::from_secs_f64(secs.clamp(0.0, 10.0));
        let note = exec(&self.inner.desk, move || i.key_combo(&combo, 1, hold)).await??;
        if let Some(n) = note {
            self.inner.note(n);
        }
        Ok(())
    }

    pub async fn type_text(&self, text: &str) -> Result<()> {
        let i = self.inner.clone();
        let text = text.to_string();
        let note = exec(&self.inner.desk, move || i.type_text(&text)).await??;
        if let Some(n) = note {
            self.inner.note(n);
        }
        Ok(())
    }

    pub fn mouse_pos(&self) -> (f64, f64) {
        self.inner.state.lock().mouse
    }

    pub fn views(&self) -> (Vec<ViewInfo>, Option<String>) {
        let st = self.inner.state.lock();
        let v = st
            .windows
            .iter()
            .map(|w| ViewInfo {
                id: w.hwnd.to_string(),
                title: if w.title.is_empty() { "(제목 없음)".into() } else { w.title.clone() },
                detail: format!(
                    "{} {}x{} (화면 밖 창){}",
                    w.class,
                    w.size.0,
                    w.size.1,
                    if w.enabled { "" } else { ", 대화상자가 열려 있어 비활성" }
                ),
            })
            .collect();
        (v, st.current.map(|c| c.to_string()))
    }

    pub async fn switch_view(&self, index: usize) -> Result<()> {
        let i = self.inner.clone();
        exec(&self.inner.desk, move || i.switch_window(index)).await??;
        Ok(())
    }

    pub fn take_notes(&self) -> Vec<String> {
        self.inner.collect_guard_events();
        std::mem::take(&mut self.inner.state.lock().notes)
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst) || self.inner.process_gone()
    }

    pub async fn shutdown(&self) {
        let i = self.inner.clone();
        let _ = exec(&self.inner.desk, move || i.shutdown_sync()).await;
    }

    /// Runs the LLM `app_control` tool and returns the result text.
    pub async fn app_control(&self, input: &Value) -> Result<String> {
        let i = self.inner.clone();
        let input = input.clone();
        exec(&self.inner.desk, move || i.app_control(&input)).await?
    }

    /// (Diagnostics) Sends only key messages (WM_KEYDOWN/UP), without emulation.
    pub async fn raw_key(&self, combo: &str) -> Result<String> {
        let i = self.inner.clone();
        let combo = combo.to_string();
        exec(&self.inner.desk, move || i.raw_key(&combo)).await?
    }
}

fn check_button(b: MouseButton) -> Result<()> {
    match b {
        MouseButton::Left => Ok(()),
        MouseButton::Right => bail!("{RIGHT_ERR}"),
        MouseButton::Middle => bail!("가운데 버튼 클릭은 지원하지 않습니다 (자동 스크롤 등은 실제 커서 위치를 씁니다)"),
    }
}

impl Drop for Inner {
    /// Processes we launched are always cleaned up, even if dropped without `shutdown` (mode B puts
    /// windows back).
    fn drop(&mut self) {
        self.shutdown_sync();
    }
}

// ---- launch / attach ----

struct Started {
    proc_: Proc,
    mode: NativeMode,
    name: String,
    guard: Guard,
    /// Mode A: processes of the same app already running before launch (for single-instance
    /// detection).
    before_same: Vec<u32>,
}

fn start_app(o: &NativeOptions, desk: Option<&DeskHost>) -> Result<Started> {
    // first put back any user windows (mode B) left off-screen by a previous crash
    super::recovery::recover();
    let exe = super::apps::resolve_app_query(&o.app).ok_or_else(|| {
        anyhow!(
            "앱을 찾지 못했습니다: '{}' — 실행 파일(.exe) 경로, 시작 메뉴 바로 가기(.lnk), 설치된 앱 이름 중 하나를 쓰세요",
            o.app
        )
    })?;
    launch::preflight(&exe)?;
    let name = launch::display_name(&exe);
    if let Some(p) = o.attach_pid {
        // refuse if attach_pid points at a process other than the requested app (another of the user's
        // apps)
        match pw::process_path(p as u32) {
            None => bail!("pid {p} 는 실행 중인 앱이 아닙니다"),
            Some(path) if !crate::platform::paths::same_path(&path, &exe) => {
                bail!("pid {p} 는 {name} 이(가) 아니라 {} 입니다 — 연결하지 않았습니다", path.display())
            }
            _ => {}
        }
    }
    // keep other requests from slipping in between current_session's check and launch (it uses the
    // original profile)
    static APP_LAUNCH: Mutex<()> = Mutex::new(());
    let _launch = APP_LAUNCH.lock();
    let running = match o.attach_pid {
        Some(p) => vec![p as u32],
        None => launch::running_instances(&exe),
    };
    if o.host == NativeHost::CurrentSession && !running.is_empty() {
        bail!("current_session: {name} 이(가) 이미 실행 중입니다. 원본 로그인 데이터 충돌과 기존 창 조작을 막기 위해 실행하지 않았습니다. 앱을 트레이까지 완전히 종료한 뒤 다시 시도하세요.");
    }
    if o.attach_pid.is_some() || (!running.is_empty() && !o.new_instance) {
        if !o.attach_consent {
            bail!(
                "{name} 이(가) 이미 실행 중입니다. 사용자의 실행 중인 앱에서 작업하려면 동의(attach)가 필요합니다 — \
                 동의하면 작업하는 동안 그 앱의 창이 화면 밖으로 옮겨져 보이지 않고, 끝나면 원래 자리로 돌아옵니다. \
                 또는 별도 인스턴스(new_instance)를 선택하세요."
            );
        }
        // prefer an instance with a visible window
        let pid = running
            .iter()
            .copied()
            .find(|p| pw::windows_of(&[*p]).iter().any(guard::is_view_window))
            .unwrap_or(running[0]);
        launch::check_attachable(pid)?;
        let process = launch::open_process(pid).ok_or_else(|| anyhow!("pid {pid} 프로세스를 열지 못했습니다"))?;
        // lock first so the app can't take the foreground while its minimized window is restored
        // off-screen
        let locked = w32::lock_foreground(true);
        let guard = Guard::start(NativeMode::Attached, vec![pid], None);
        guard.adopt_lock(locked);
        guard.lease(Duration::from_secs(3));
        eprintln!("[ghostdesk] Windows 앱 연결 (Mode B): {name} pid={pid}");
        return Ok(Started {
            proc_: Proc { pid, process, job: None },
            mode: NativeMode::Attached,
            name,
            guard,
            before_same: vec![],
        });
    }
    if let Some(d) = desk {
        // hidden desktop: the app starts as a normal window but never appears on the user's screen. The
        // guard runs on the user's desktop and moves off-screen any window the app does create there (if
        // it leaks out).
        let mut args = o.extra_args.clone();
        if hdesk::is_chromium_family(&exe) {
            for a in hdesk::CHROMIUM_HIDDEN_ARGS {
                if !args.iter().any(|x| x == a) {
                    args.push(a.to_string());
                }
            }
        }
        let (proc_, thread) = launch::spawn_suspended(&exe, &args, &o.open_files, Some(&d.desktop), launch::job_ui_limits_for(&exe))?;
        let guard = Guard::start(NativeMode::Launched, vec![proc_.pid], proc_.job.clone());
        std::thread::sleep(Duration::from_millis(40));
        launch::resume(&thread);
        drop(thread);
        eprintln!("[ghostdesk] Windows 앱 숨은 데스크톱 실행: {name} pid={} desktop={} ({})", proc_.pid, d.desktop.name(), exe.display());
        return Ok(Started { proc_, mode: NativeMode::Launched, name, guard, before_same: running });
    }
    let (proc_, thread) = launch::spawn_suspended_hidden(&exe, &o.extra_args, &o.open_files)?;
    let guard = Guard::start(NativeMode::Launched, vec![proc_.pid], proc_.job.clone());
    // launch phase: show the main window that started hidden, off-screen, and lock so the app can't
    // take the foreground
    guard.reveal_for(Duration::from_secs(20));
    guard.adopt_lock(w32::lock_foreground(true));
    guard.lease(Duration::from_secs(12));
    // give the guard time to install its hooks, then resume
    std::thread::sleep(Duration::from_millis(40));
    launch::resume(&thread);
    drop(thread);
    eprintln!("[ghostdesk] Windows 앱 숨김 실행 (Mode A): {name} pid={} ({})", proc_.pid, exe.display());
    Ok(Started { proc_, mode: NativeMode::Launched, name, guard, before_same: running })
}

// ---- internals ----

impl Inner {
    fn note(&self, s: impl Into<String>) {
        let s = s.into();
        if !s.is_empty() {
            self.state.lock().notes.push(s);
        }
    }

    fn pids(&self) -> Vec<u32> {
        let g = self.guard.lock();
        match g.as_ref() {
            Some(g) => g.pids(),
            None => self.proc_.pids(),
        }
    }

    fn process_gone(&self) -> bool {
        match self.mode {
            NativeMode::Launched => !self.proc_.alive() && self.proc_.pids().is_empty(),
            NativeMode::Attached => !self.proc_.alive(),
        }
    }

    /// Before sending input: briefly lock so the app can't take the foreground.
    fn lease(&self) {
        // apps on a hidden desktop can't take the user's foreground (it isn't the input desktop)
        if self.desk.is_some() {
            return;
        }
        if let Some(g) = self.guard.lock().as_ref() {
            if !g.reclaimed() {
                g.adopt_lock(w32::lock_foreground(true));
            }
            g.lease(Duration::from_millis(1500));
        }
    }

    /// Waits briefly until the app has processed the messages sent so far (and any messages it
    /// generated from them).
    fn flush_input(&self) {
        self.proc_.wait_idle(300);
        std::thread::sleep(Duration::from_millis(25));
    }

    /// Waits briefly for the app to process the input sent.
    fn settle(&self, ms: u32) {
        self.proc_.wait_idle(ms);
        std::thread::sleep(Duration::from_millis(30));
    }

    fn collect_guard_events(&self) {
        let (ev, reclaimed) = match self.guard.lock().as_ref() {
            Some(g) => (g.take_events(), g.reclaimed()),
            None => (vec![], false),
        };
        if !ev.is_empty() {
            self.exposure.lock().extend(ev.iter().cloned());
            self.note(format!("앱 창 감시: {}", ev.join(" / ")));
        }
        if reclaimed && self.mode == NativeMode::Attached {
            let mut st = self.state.lock();
            if !st.reclaim_noted {
                st.reclaim_noted = true;
                st.notes.push(
                    "사용자가 앱을 다시 화면에 표시했습니다. 사용자가 직접 쓰는 중일 수 있으니 작업을 멈추고 확인을 요청하세요.".into(),
                );
            }
        }
    }

    fn guard_exposure(&self) {
        if !self.shut.load(Ordering::SeqCst) {
            self.collect_guard_events();
        }
    }

    fn wait_first_window(&self, before_same: Vec<u32>) -> Result<()> {
        let t = Instant::now();
        let limit = Duration::from_secs(if self.mode == NativeMode::Attached { 3 } else { 15 });
        while t.elapsed() < limit {
            self.refresh_windows(false);
            if !self.state.lock().windows.is_empty() {
                break;
            }
            if self.mode == NativeMode::Launched && self.process_gone() {
                let code = self.proc_.exit_code();
                // a single-instance app that handed off to a copy of itself already running before launch
                let others: Vec<u32> = before_same.iter().copied().filter(|p| crate::platform::proc::pid_alive(*p)).collect();
                return Err(launch::err_exit_message(&self.app_name, code, &others));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if self.state.lock().windows.is_empty() {
            match self.mode {
                NativeMode::Launched => bail!(
                    "{} 이(가) {}초 안에 창을 열지 않았습니다 (알림 영역 전용 앱이거나 시작이 느린 앱일 수 있음)",
                    self.app_name,
                    limit.as_secs()
                ),
                NativeMode::Attached => bail!(
                    "{} 에 열린 창이 없습니다 — 알림 영역(트레이)에만 있는 앱이면 먼저 창을 열어 두세요",
                    self.app_name
                ),
            }
        }
        // give the off-screen main window a moment to finish painting
        self.settle(2000);
        self.ensure_current();
        Ok(())
    }

    /// Whether GPU frames are being dropped on the hidden desktop (blt-model DXGI, WPF hardware
    /// rendering, …).
    ///
    /// Only assumed when the current window's client area has been a single color for over 3 s, the
    /// app is responsive, and it has GPU DLLs loaded.
    fn gpu_frames_lost(&self) -> Option<String> {
        let t = Instant::now();
        let mut blank_since: Option<Instant> = None;
        while t.elapsed() < Duration::from_secs(6) {
            let cur = self.ensure_current()?;
            if w32::is_hung(cur.hwnd) {
                return None;
            }
            match capture::capture_client(cur.hwnd, 480) {
                Ok(shot) if !capture::is_blank(&shot.img) => return None,
                Ok(_) => {
                    let since = *blank_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= Duration::from_secs(3) {
                        let mut mods: Vec<String> = self.pids().into_iter().flat_map(hdesk::gpu_modules).collect();
                        if mods.is_empty() {
                            return None;
                        }
                        mods.sort();
                        mods.dedup();
                        return Some(format!(
                            "창 내용이 {:.0}초 동안 비어 있음 (GPU 모듈: {})",
                            since.elapsed().as_secs_f32(),
                            mods.join(", ")
                        ));
                    }
                }
                Err(_) => return None,
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        None
    }

    /// Whether an app launched on the hidden desktop created windows on the user's desktop (the
    /// guard moves them off-screen, but it's still logged).
    fn check_escaped(&self) {
        if self.desk.is_none() {
            return;
        }
        let n = hdesk::windows_on_user_desktop(&self.pids()).into_iter().filter(|&h| w32::is_visible(h)).count();
        if n > 0 {
            let msg = format!("앱이 사용자 데스크톱에 보이는 창 {n}개를 만들었습니다 (감시기가 화면 밖으로 옮김)");
            self.exposure.lock().push(msg.clone());
            self.note(msg);
        }
    }

    /// Refreshes the window list. When a new window appears, leaves a note and switches to it (modal
    /// dialogs first).
    fn refresh_windows(&self, announce: bool) {
        let pids = self.pids();
        let list: Vec<WinEntry> = pw::windows_of(&pids)
            .into_iter()
            .filter(guard::is_view_window)
            .map(|w| WinEntry {
                hwnd: w.hwnd,
                title: w.title.clone(),
                class: w.class.clone(),
                owner: w32::owner(w.hwnd),
                size: (w.rect.2 - w.rect.0, w.rect.3 - w.rect.1),
                enabled: w32::is_enabled(w.hwnd),
            })
            .collect();
        let mut st = self.state.lock();
        let had_any = !st.windows.is_empty();
        let mut switch_to = None;
        if announce && had_any {
            for w in &list {
                if !st.windows.iter().any(|o| o.hwnd == w.hwnd) {
                    st.notes.push(format!("새 창이 열려 그 창으로 전환했습니다: '{}' ({})", w.title, w.class));
                    if w.enabled && (switch_to.is_none() || w.owner != 0) {
                        switch_to = Some(w.hwnd);
                    }
                }
            }
            let closed: Vec<String> = st
                .windows
                .iter()
                .filter(|w| !list.iter().any(|n| n.hwnd == w.hwnd))
                .map(|w| format!("창이 닫혔습니다: '{}'", w.title))
                .collect();
            st.notes.extend(closed);
        }
        st.windows = list;
        if let Some(w) = switch_to {
            st.current = Some(w);
            st.geom = None;
        }
        if let Some(c) = st.current {
            if !st.windows.iter().any(|w| w.hwnd == c) {
                st.current = None;
                st.geom = None;
            }
        }
    }

    /// With no current window: the app thread's active window (modal dialog, …) → an activated
    /// unowned window → the first window.
    fn ensure_current(&self) -> Option<WinEntry> {
        {
            let st = self.state.lock();
            if let Some(c) = st.current {
                if let Some(w) = st.windows.iter().find(|w| w.hwnd == c) {
                    return Some(w.clone());
                }
            }
        }
        self.refresh_windows(true);
        let mut st = self.state.lock();
        let active = st.windows.first().and_then(|w| w32::thread_focus(w32::thread_pid(w.hwnd).0)).map(|(a, _)| a);
        let pick = st
            .windows
            .iter()
            .find(|w| Some(w.hwnd) == active && w.enabled)
            .or_else(|| st.windows.iter().find(|w| w.enabled && w.owner != 0))
            .or_else(|| st.windows.iter().find(|w| w.enabled && w.owner == 0))
            .or_else(|| st.windows.first())
            .cloned();
        st.current = pick.as_ref().map(|w| w.hwnd);
        st.geom = None;
        pick
    }

    fn current_window(&self) -> Result<WinEntry> {
        self.ensure_current().ok_or_else(|| anyhow!("{} 에 열린 창이 없습니다", self.app_name))
    }

    fn capture_current(&self, max_long_edge: u32) -> Result<(image::RgbImage, Geom)> {
        let w = self.current_window()?;
        let _dpi = w32::DpiScope::for_window(w.hwnd);
        let shot = match capture::capture_client(w.hwnd, max_long_edge) {
            Ok(s) => s,
            Err(e) => {
                // the window may have just closed, so refresh the list and try once more
                self.refresh_windows(true);
                let w2 = self.current_window()?;
                if w2.hwnd == w.hwnd {
                    return Err(e);
                }
                capture::capture_client(w2.hwnd, max_long_edge)?
            }
        };
        let hwnd = self.state.lock().current.unwrap_or(w.hwnd);
        let g = Geom { hwnd, client_w: shot.client_w, client_h: shot.client_h, shot_w: shot.img.width(), shot_h: shot.img.height() };
        Ok((shot.img, g))
    }

    fn screenshot_png(&self) -> Result<Vec<u8>> {
        self.guard_exposure();
        let (img, geom) = self.capture_current(self.opts.max_long_edge)?;
        if capture::is_blank(&img) {
            self.note("스크린샷이 한 가지 색으로만 보입니다 — 앱이 아직 그리는 중이거나 화면 밖에서 그리기를 멈추는 창일 수 있습니다");
        }
        *self.size.lock() = (geom.shot_w, geom.shot_h);
        self.state.lock().geom = Some(geom);
        capture::encode_png(&img)
    }

    fn geom(&self) -> Result<Geom> {
        let cur = self.current_window()?;
        if let Some(g) = self.state.lock().geom {
            if g.hwnd == cur.hwnd {
                return Ok(g);
            }
        }
        self.screenshot_png()?;
        self.state.lock().geom.ok_or_else(|| anyhow!("좌표계를 만들지 못했습니다"))
    }

    fn zoom_png(&self, region: [f64; 4]) -> Result<Vec<u8>> {
        let g = self.geom()?;
        let (full, fg) = self.capture_current(0)?;
        let k = fg.shot_w as f64 / g.shot_w.max(1) as f64;
        let (x0, y0) = (region[0].min(region[2]).max(0.0), region[1].min(region[3]).max(0.0));
        let (x1, y1) = (region[0].max(region[2]), region[1].max(region[3]));
        let cx = ((x0 * k) as u32).min(full.width().saturating_sub(1));
        let cy = ((y0 * k) as u32).min(full.height().saturating_sub(1));
        let cw = (((x1 - x0) * k).max(1.0) as u32).min(full.width() - cx);
        let ch = (((y1 - y0) * k).max(1.0) as u32).min(full.height() - cy);
        let crop = image::imageops::crop_imm(&full, cx, cy, cw, ch).to_image();
        let target = self.opts.max_long_edge.max(640) as f64;
        let scale = (target / cw.max(ch) as f64).clamp(1.0, 8.0);
        let out = image::imageops::resize(
            &crop,
            ((cw as f64 * scale) as u32).max(1),
            ((ch as f64 * scale) as u32).max(1),
            image::imageops::FilterType::CatmullRom,
        );
        capture::encode_png(&out)
    }

    fn switch_window(&self, index: usize) -> Result<String> {
        self.refresh_windows(false);
        let mut st = self.state.lock();
        let w = st.windows.get(index).cloned().ok_or_else(|| anyhow!("창 번호 {index} 가 없습니다 (창 {}개)", st.windows.len()))?;
        st.current = Some(w.hwnd);
        st.geom = None;
        Ok(format!("창 [{index}] '{}' 로 전환했습니다", w.title))
    }

    // ---- mouse ----

    /// Coordinate system for input. The window in the agent's last screenshot must be the current
    /// window.
    fn input_geom(&self) -> Result<Geom> {
        if self.guard.lock().as_ref().map(|g| g.reclaimed()).unwrap_or(false) {
            bail!("사용자가 앱을 다시 화면에 가져와 쓰는 중이라 입력을 보내지 않았습니다. 작업을 멈추고 사용자에게 확인하세요.");
        }
        let cur = self.current_window()?;
        let g = self.state.lock().geom;
        let Some(g) = g.filter(|g| g.hwnd == cur.hwnd) else {
            bail!(
                "현재 창 '{}' 의 스크린샷을 아직 보지 않았습니다 (새 창으로 자동 전환되었거나 창을 바꿈). \
                 screenshot 으로 화면을 다시 확인한 뒤 좌표를 지정하세요.",
                cur.title
            );
        };
        if !cur.enabled {
            bail!(
                "창 '{}' 은(는) 지금 비활성입니다 (모달 대화상자가 열려 있음). 뷰 목록에서 대화상자로 전환(switch_view)한 뒤 다시 하세요.",
                cur.title
            );
        }
        Ok(g)
    }

    /// Reason to refuse if clicking the control would show something on the user's screen or use the
    /// system clipboard.
    fn click_block_reason(&self, target: Hwnd) -> Option<String> {
        let class = w32::class_name(target);
        let lc = class.to_lowercase();
        let text = w32::window_text(target);
        if lc == "syslink" {
            return Some("링크는 브라우저를 사용자 화면 앞에 열기 때문에 누르지 않았습니다. 주소는 app_control read 로 확인하세요.".into());
        }
        if lc == "button" {
            if menu::is_clipboard_title(&text) {
                return Some(format!("'{}' 단추는 {}", w32::strip_mnemonic(&text), menu::R_CLIPBOARD));
            }
            if menu::is_help_title(&text) {
                return Some(format!("'{}' 단추는 — {}", w32::strip_mnemonic(&text), menu::R_HELP));
            }
        }
        // drop-down lists and calendars get pulled onto the user's screen. Matched by substring:
        // ComboBoxEx32, WinForms (WindowsForms10.COMBOBOX…), Delphi (TComboBox, TDateTimePicker), …
        if dropdown_kind(&class) == Some(Dropdown::Combo) {
            let items = w32::combo_items(target);
            return Some(format!(
                "콤보 상자를 클릭하면 드롭다운 목록이 사용자 화면에 열리므로 열지 않았습니다. app_control ui_tree 로 id 를 확인한 뒤 \
                 app_control choose {{id, option}} 을 쓰세요.{}",
                if items.is_empty() { String::new() } else { format!(" 선택지: [{}]", items.iter().take(30).cloned().collect::<Vec<_>>().join(", ")) }
            ));
        }
        if dropdown_kind(&class) == Some(Dropdown::Date) {
            return Some("날짜 선택 컨트롤의 달력은 사용자 화면에 열리므로 열지 않았습니다. 날짜는 컨트롤을 클릭한 위치 대신 키보드(숫자·화살표)로 바꾸세요.".into());
        }
        None
    }

    fn click(&self, x: f64, y: f64, count: u32, mods: i64) -> Result<String> {
        let mods = crate::platform::input::localize_mods(mods);
        let g = self.input_geom()?;
        let _dpi = w32::DpiScope::for_window(g.hwnd);
        if !g.contains_shot(x, y) {
            bail!("좌표 ({x:.0},{y:.0}) 가 창 밖입니다 (스크린샷 크기 {}x{})", g.shot_w, g.shot_h);
        }
        let p = g.to_client(x, y);
        let (target, pt) = input::hit(g.hwnd, p);
        if !w32::is_enabled(target) {
            bail!("클릭한 컨트롤 '{}'({}) 이(가) 비활성화되어 있습니다", w32::strip_mnemonic(&w32::window_text(target)), w32::class_name(target));
        }
        if let Some(r) = self.click_block_reason(target) {
            bail!("{r}");
        }
        self.lease();
        input::post_click(target, pt, count, input::mk_flags(mods));
        self.state.lock().last_target = Some(target);
        self.settle(800);
        self.after_action();
        let text = w32::strip_mnemonic(&w32::window_text(target));
        Ok(format!(
            "{} → {}{} ({x:.0},{y:.0})",
            match count {
                1 => "클릭",
                2 => "더블클릭",
                _ => "세 번 클릭",
            },
            w32::class_name(target),
            if text.is_empty() || target == g.hwnd { String::new() } else { format!(" '{}'", w32::short(&text, 30)) }
        ))
    }

    /// Checks whether a new window (dialog) appeared after an action.
    fn after_action(&self) {
        self.guard_exposure();
        self.refresh_windows(true);
    }

    fn button(&self, down: bool, mods: i64) -> Result<()> {
        let mods = crate::platform::input::localize_mods(mods);
        let mk = input::mk_flags(mods);
        if down {
            let g = self.input_geom()?;
            let _dpi = w32::DpiScope::for_window(g.hwnd);
            let (x, y) = self.state.lock().mouse;
            if !g.contains_shot(x, y) {
                bail!("좌표 ({x:.0},{y:.0}) 가 창 밖입니다");
            }
            let (target, pt) = input::hit(g.hwnd, g.to_client(x, y));
            if let Some(r) = self.click_block_reason(target) {
                bail!("{r}");
            }
            self.lease();
            input::post_move(target, pt, mk);
            input::post_button(target, pt, true, mk);
            let mut st = self.state.lock();
            st.down = Some(Down { target, top: g.hwnd, mk, start: (x, y), pt });
            st.last_target = Some(target);
            Ok(())
        } else {
            let (d, (x, y)) = {
                let mut st = self.state.lock();
                (st.down.take(), st.mouse)
            };
            let Some(d) = d else { bail!("mouse_down 없이 mouse_up 이 왔습니다") };
            let _dpi = w32::DpiScope::for_window(d.top);
            let g = self.state.lock().geom.filter(|g| g.hwnd == d.top).ok_or_else(|| anyhow!("드래그하던 창이 바뀌었습니다"))?;
            let moved = (x - d.start.0).abs() > 4.0 || (y - d.start.1).abs() > 4.0;
            self.lease();
            if moved && !drag_safe(&w32::class_name(d.target)) {
                // release at the press point to clear the pressed state, and report that no drag happened
                input::post_button(d.target, d.pt, false, d.mk);
                self.settle(500);
                bail!("{}", drag_refusal(&w32::class_name(d.target)));
            }
            let pt = w32::map_point(d.top, d.target, g.to_client(x, y));
            input::post_button(d.target, pt, false, d.mk);
            self.settle(500);
            self.after_action();
            Ok(())
        }
    }

    fn drag_move(&self, x: f64, y: f64, _mods: i64) -> Result<()> {
        let (target, top, mk) = match self.state.lock().down.as_ref() {
            Some(d) => (d.target, d.top, d.mk),
            None => return Ok(()),
        };
        if !drag_safe(&w32::class_name(target)) {
            // controls that can start an OLE drag and drop don't get moves with the button held (mouse_up
            // explains)
            return Ok(());
        }
        let _dpi = w32::DpiScope::for_window(top);
        let g = self.state.lock().geom.filter(|g| g.hwnd == top).ok_or_else(|| anyhow!("드래그하던 창이 바뀌었습니다"))?;
        let pt = w32::map_point(top, target, g.to_client(x, y));
        input::post_move(target, pt, mk | w32::MK_LBUTTON);
        Ok(())
    }

    fn drag(&self, from: (f64, f64), to: (f64, f64), mods: i64) -> Result<String> {
        let mods = crate::platform::input::localize_mods(mods);
        let g = self.input_geom()?;
        let _dpi = w32::DpiScope::for_window(g.hwnd);
        if !g.contains_shot(from.0, from.1) || !g.contains_shot(to.0, to.1) {
            bail!("드래그 시작·끝 좌표가 창 밖입니다 (스크린샷 크기 {}x{})", g.shot_w, g.shot_h);
        }
        let mk = input::mk_flags(mods);
        let (target, pt) = input::hit(g.hwnd, g.to_client(from.0, from.1));
        if let Some(r) = self.click_block_reason(target) {
            bail!("{r}");
        }
        if !drag_safe(&w32::class_name(target)) {
            bail!("{}", drag_refusal(&w32::class_name(target)));
        }
        self.lease();
        input::post_move(target, pt, mk);
        input::post_button(target, pt, true, mk);
        let steps = 12;
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let p = g.to_client(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
            input::post_move(target, w32::map_point(g.hwnd, target, p), mk | w32::MK_LBUTTON);
            std::thread::sleep(Duration::from_millis(12));
        }
        let end = w32::map_point(g.hwnd, target, g.to_client(to.0, to.1));
        input::post_button(target, end, false, mk);
        self.state.lock().mouse = to;
        self.settle(800);
        self.after_action();
        Ok(format!(
            "드래그 → {} ({:.0},{:.0})→({:.0},{:.0}). 실제 커서 위치(GetCursorPos)로 끌기를 처리하는 앱은 따라오지 않을 수 있으니 스크린샷으로 확인하세요.",
            w32::class_name(target),
            from.0,
            from.1,
            to.0,
            to.1
        ))
    }

    fn scroll(&self, x: f64, y: f64, dx: f64, dy: f64, mods: i64) -> Result<String> {
        let mods = crate::platform::input::localize_mods(mods);
        let g = self.input_geom()?;
        let _dpi = w32::DpiScope::for_window(g.hwnd);
        let (x, y) = (x.clamp(0.0, g.shot_w.saturating_sub(1) as f64), y.clamp(0.0, g.shot_h.saturating_sub(1) as f64));
        let p = g.to_client(x, y);
        let (target, _) = input::hit(g.hwnd, p);
        let origin = w32::client_origin(g.hwnd);
        let screen = (origin.0 + p.0, origin.1 + p.1);
        let mk = input::mk_flags(mods);
        let (nv, nh) = (input::wheel_notches(dy), input::wheel_notches(dx));
        if nv == 0 && nh == 0 {
            return Ok("스크롤 양이 0 입니다".into());
        }
        let bars = [(nv, find_scrollbar(target, g.hwnd, true)), (nh, find_scrollbar(target, g.hwnd, false))];
        let before: Vec<Option<i32>> = bars.iter().map(|(_, b)| b.map(|(_, _, pw, bar)| w32::scroll_pos(pw, bar))).collect();
        self.lease();
        // down (dy > 0) = negative wheel, right (dx > 0) = positive horizontal wheel
        for _ in 0..nv.abs() {
            input::post_wheel(target, screen, if nv > 0 { -120 } else { 120 }, false, mk);
        }
        for _ in 0..nh.abs() {
            input::post_wheel(target, screen, if nh > 0 { 120 } else { -120 }, true, mk);
        }
        self.settle(500);
        let mut out = vec![format!("스크롤 → {} (세로 {}칸, 가로 {}칸)", w32::class_name(target), -nv, nh)];
        // controls that don't handle the wheel (e.g. with a sibling scroll bar) get line scrolls sent to
        // the scroll bar
        for (i, (n, bar)) in bars.iter().enumerate() {
            let (Some((recv, ctl, posw, kind)), Some(b0)) = (*bar, before[i]) else { continue };
            if *n == 0 || w32::scroll_pos(posw, kind) != b0 {
                continue;
            }
            let msg = if i == 0 { w32::WM_VSCROLL } else { w32::WM_HSCROLL };
            let code = if *n > 0 { 1 } else { 0 }; // SB_LINEDOWN/RIGHT : SB_LINEUP/LEFT
            for _ in 0..(n.abs() * 3) {
                w32::post(recv, msg, code, ctl);
            }
            w32::post(recv, msg, 8, ctl); // SB_ENDSCROLL
            self.settle(500);
            out.push(format!(
                "휠이 반영되지 않아 {} 스크롤 막대로 {}줄 스크롤 (위치 {b0} → {})",
                if i == 0 { "세로" } else { "가로" },
                n.abs() * 3,
                w32::scroll_pos(posw, kind)
            ));
        }
        Ok(out.join(" / "))
    }

    // ---- keyboard ----

    /// Keyboard target: the app thread's focus window (inside the current window) → the last clicked
    /// control → the current window.
    fn focus_target(&self, top: Hwnd) -> Hwnd {
        let tid = w32::thread_pid(top).0;
        if let Some((_, f)) = w32::thread_focus(tid) {
            if f != 0 && w32::root(f) == top {
                return f;
            }
        }
        if let Some(t) = self.state.lock().last_target {
            if w32::is_window(t) && w32::root(t) == top {
                return t;
            }
        }
        top
    }

    fn key_top(&self) -> Result<Hwnd> {
        if self.guard.lock().as_ref().map(|g| g.reclaimed()).unwrap_or(false) {
            bail!("사용자가 앱을 다시 화면에 가져와 쓰는 중이라 입력을 보내지 않았습니다. 작업을 멈추고 사용자에게 확인하세요.");
        }
        let w = self.current_window()?;
        // the list's `enabled` is from the last refresh, so check the current state too
        if !w.enabled || !w32::is_enabled(w.hwnd) {
            bail!("창 '{}' 은(는) 지금 비활성입니다 (모달 대화상자가 열려 있음). 대화상자로 전환(switch_view)한 뒤 다시 하세요.", w.title);
        }
        Ok(w.hwnd)
    }

    fn type_text(&self, text: &str) -> Result<Option<String>> {
        let (t, removed) = input::sanitize_text(text);
        if t.is_empty() {
            return Ok((removed > 0).then(|| format!("제어 문자 {removed}개는 보내지 않았습니다")));
        }
        let top = self.key_top()?;
        let target = self.focus_target(top);
        let editable = is_edit_class(&w32::class_name(target));
        // compare against the text after earlier input has been processed
        self.settle(500);
        let before = if editable { w32::control_text(target) } else { None };
        self.lease();
        input::post_text(target, &t, &|| self.flush_input());
        self.settle(1500);
        let mut notes = Vec::new();
        if removed > 0 {
            notes.push(format!("제어 문자 {removed}개는 보내지 않았습니다"));
        }
        if let Some(before) = before {
            let want_len = t.encode_utf16().count() as isize;
            let ok = |now: &str| now != before && (now.contains(t.trim_end_matches('\n')) || now.encode_utf16().count() as isize - before.encode_utf16().count() as isize >= want_len);
            let deadline = Instant::now() + Duration::from_millis(800);
            let mut now = w32::control_text(target).unwrap_or_default();
            while !ok(&now) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(60));
                now = w32::control_text(target).unwrap_or_default();
            }
            if !ok(&now) {
                if now != before {
                    notes.push("입력은 반영되었지만 내용이 예상과 다를 수 있습니다 — 스크린샷으로 확인하세요".into());
                } else {
                    // character messages didn't take, so replace the selection instead (EM_REPLACESEL; the system
                    // marshals the string)
                    let _ = w32::send_str(target, w32::EM_REPLACESEL, 1, &t.replace('\n', "\r\n"), 2000);
                    std::thread::sleep(Duration::from_millis(80));
                    let after = w32::control_text(target).unwrap_or_default();
                    if after == before {
                        bail!("글자 입력이 반영되지 않았습니다 ({} 에 WM_CHAR·EM_REPLACESEL 모두 실패). 이 앱은 창 메시지 입력을 받지 않는 것 같습니다 — desk_close 후 desk_open mode=session (같은 PC 의 별도 Windows 세션, 실제 키보드·마우스 입력)으로 다시 여세요", w32::class_name(target));
                    }
                    notes.push("글자 메시지가 반영되지 않아 EM_REPLACESEL 로 넣었습니다".into());
                }
            }
        } else if target == top {
            notes.push("포커스된 입력 칸을 찾지 못해 창에 글자를 보냈습니다 — 먼저 입력 칸을 클릭하고 스크린샷으로 확인하세요".into());
        } else {
            notes.push(format!("{} 에 글자를 보냈습니다 (표준 입력 칸이 아니라 반영을 자동 확인하지 못함 — 스크린샷으로 확인)", w32::class_name(target)));
        }
        self.after_action();
        Ok((!notes.is_empty()).then(|| notes.join(" / ")))
    }

    fn key_combo(&self, combo: &str, repeat: u32, hold: Duration) -> Result<Option<String>> {
        let plan = input::plan_combo(combo)?;
        let top = self.key_top()?;
        let target = self.focus_target(top);
        match plan {
            KeyPlan::BareModifier => Ok(Some(format!("{combo}: 수정키만 눌러서는 효과가 없어 보내지 않았습니다"))),
            KeyPlan::Text(t) => {
                self.lease();
                input::post_text(target, &t.repeat(repeat as usize), &|| self.flush_input());
                self.settle(800);
                self.after_action();
                Ok(None)
            }
            KeyPlan::Key { vk, shift } => {
                if let Some(r) = dropdown_key_reason(target, vk, combo) {
                    bail!("{r}");
                }
                self.lease();
                if shift && vk == menu::VK_TAB && w32::class_name(top) == "#32770" {
                    // to the dialog's previous control (Shift state can't be faked with messages, hence
                    // WM_NEXTDLGCTL)
                    for _ in 0..repeat {
                        w32::post(top, w32::WM_NEXTDLGCTL, 1, 0);
                    }
                    self.settle(500);
                    return Ok(Some(format!("{combo} → 대화상자의 이전 컨트롤로 이동")));
                }
                for _ in 0..repeat {
                    input::post_key(target, vk, false);
                    if !hold.is_zero() {
                        std::thread::sleep(hold);
                    }
                    input::post_key(target, vk, true);
                }
                self.settle(800);
                self.after_action();
                Ok(shift.then(|| format!("{combo}: Shift 상태는 백그라운드 창에 전달되지 않아 Shift 없이 눌렀습니다")))
            }
            KeyPlan::Shortcut(a) => {
                let mut out = None;
                for _ in 0..repeat {
                    out = Some(self.shortcut(combo, &a, top, target)?);
                }
                self.after_action();
                Ok(out)
            }
        }
    }

    /// Ctrl/Alt shortcut emulation: edit commands (internal clipboard) → Alt+F4 → menu accelerators →
    /// dialog mnemonics.
    fn shortcut(&self, combo: &str, a: &Accel, top: Hwnd, target: Hwnd) -> Result<String> {
        if let Some(r) = self.emulate_edit(a, target)? {
            return Ok(r);
        }
        if menu::is_clipboard_accel(a) {
            bail!("{combo}: 입력 칸에 포커스가 없어 실행하지 않았습니다 — {}", menu::R_CLIPBOARD);
        }
        if a.alt && !a.ctrl && a.vk == 0x73 {
            self.lease();
            w32::post(top, w32::WM_SYSCOMMAND, w32::SC_CLOSE, 0);
            self.settle(800);
            return Ok(format!("{combo} → 창 '{}' 닫기 (WM_SYSCOMMAND SC_CLOSE)", w32::window_text(top)));
        }
        let alt_letter = a.alt && !a.ctrl && (0x41..=0x5A).contains(&a.vk);
        if let Some((path, owner, item, blocked)) = menu::find_by_accel(top, a) {
            if !menu::owner_usable(owner) {
                // with a modal dialog open, a real keyboard's Alt+letter goes to the dialog's mnemonic (never
                // reaching the main window's menu)
                if alt_letter {
                    let ch = (a.vk as u8 as char).to_ascii_lowercase();
                    if let Some(r) = self.dialog_mnemonic(top, ch)? {
                        return Ok(format!("{combo} → {r}"));
                    }
                }
                bail!("{combo} 는 메뉴 '{path}' 의 단축키인데, {}", menu::R_MODAL);
            }
            if let Some(r) = blocked {
                bail!("{combo} 는 메뉴 '{path}' 의 단축키입니다. {r}");
            }
            if item.disabled {
                bail!("{combo} 는 메뉴 '{path}' 의 단축키인데 지금 비활성화되어 있습니다");
            }
            self.lease();
            menu::invoke(owner, item.id);
            self.settle(1000);
            return Ok(format!("{combo} → 메뉴 '{path}' 실행 (WM_COMMAND {})", item.id));
        }
        if alt_letter {
            let ch = (a.vk as u8 as char).to_ascii_lowercase();
            if let Some(r) = self.dialog_mnemonic(top, ch)? {
                return Ok(format!("{combo} → {r}"));
            }
            bail!(
                "{combo}: 이 창에서 '{ch}' 니모닉을 가진 단추를 찾지 못했습니다. Alt+글자를 키 메시지로 보내면 메뉴가 사용자 화면에 열릴 수 있어 보내지 않았습니다 — \
                 메뉴는 app_control menus / menu, 단추는 app_control press 를 쓰세요."
            );
        }
        bail!(
            "{combo}: 백그라운드 창에 보낸 Ctrl/Alt 키 메시지는 앱이 인식하지 못합니다 (키 상태가 바뀌지 않음). 이 단축키를 가진 메뉴 항목도 없습니다 — \
             같은 기능을 app_control menus 로 찾아 menu {{path}} 로 실행하거나, 단추를 press 하세요."
        )
    }

    /// Presses a dialog's '&letter' mnemonic button (emulating Alt+letter).
    fn dialog_mnemonic(&self, top: Hwnd, ch: char) -> Result<Option<String>> {
        let _dpi = w32::DpiScope::for_window(top);
        for c in w32::child_windows(top) {
            if !w32::is_visible(c) || !w32::is_enabled(c) || w32::class_name(c).to_lowercase() != "button" {
                continue;
            }
            let text = w32::window_text(c);
            if w32::mnemonic_of(&text) == Some(ch) {
                if let Some(r) = self.click_block_reason(c) {
                    bail!("{r}");
                }
                let (cw, chh) = w32::client_size(c);
                self.lease();
                input::post_click(c, (cw / 2, chh / 2), 1, 0);
                self.state.lock().last_target = Some(c);
                self.settle(800);
                return Ok(Some(format!("단추 '{}' 클릭", w32::strip_mnemonic(&text))));
            }
        }
        Ok(None)
    }

    /// The selected text (UIA TextPattern → EM_GETSEL + WM_GETTEXT).
    fn selected_text(&self, target: Hwnd) -> Option<String> {
        if let Some(s) = uia::element_for(target).ok().and_then(|e| e.selection_text()) {
            return Some(s);
        }
        let r = w32::send_timeout(target, w32::EM_GETSEL, 0, 0, 1000)?;
        let (s, e) = ((r & 0xFFFF) as usize, ((r >> 16) & 0xFFFF) as usize);
        let text = w32::control_text(target)?;
        let u: Vec<u16> = text.encode_utf16().collect();
        (s <= e && e <= u.len()).then(|| String::from_utf16_lossy(&u[s..e]))
    }

    /// Emulates Ctrl+A/C/X/V/Z/Y in edit controls with the internal clipboard (the system clipboard
    /// is never touched). `None` if not applicable.
    fn emulate_edit(&self, a: &Accel, target: Hwnd) -> Result<Option<String>> {
        if a.alt || !is_edit_class(&w32::class_name(target)) {
            return Ok(None);
        }
        let op = match (a.ctrl, a.shift, a.vk) {
            (true, false, 0x41) => "all",
            (true, false, 0x43) | (true, false, menu::VK_INSERT) => "copy",
            (true, false, 0x58) | (false, true, menu::VK_DELETE) => "cut",
            (true, false, 0x56) | (false, true, menu::VK_INSERT) => "paste",
            (true, false, 0x5A) => "undo",
            (true, false, 0x59) | (true, true, 0x5A) => "redo",
            _ => return Ok(None),
        };
        // look at the selection after earlier input has been processed
        self.settle(500);
        self.lease();
        let class = w32::class_name(target);
        match op {
            "all" => {
                w32::send_timeout(target, w32::EM_SETSEL, 0, -1, 1000);
                let len = w32::control_text(target).map(|t| t.chars().count()).unwrap_or(0);
                Ok(Some(format!("전체 선택 ({len}자, EM_SETSEL)")))
            }
            "copy" | "cut" => {
                let sel = self.selected_text(target).unwrap_or_default();
                if sel.is_empty() {
                    return Ok(Some("선택된 글자가 없어 복사하지 않았습니다".into()));
                }
                self.state.lock().clipboard = sel.clone();
                if op == "cut" {
                    let before = w32::control_text(target).unwrap_or_default();
                    // a Backspace char on a selection deletes it (no clipboard involved)
                    w32::post(target, w32::WM_CHAR, 0x08, 1);
                    self.settle(500);
                    if w32::control_text(target).unwrap_or_default() == before {
                        bail!("잘라내기가 반영되지 않았습니다 (선택 삭제 실패)");
                    }
                    return Ok(Some(format!("{}자를 잘라내 내부 클립보드에 담았습니다 (시스템 클립보드는 건드리지 않음)", sel.chars().count())));
                }
                Ok(Some(format!("{}자를 내부 클립보드에 복사했습니다 (시스템 클립보드는 건드리지 않음)", sel.chars().count())))
            }
            "paste" => {
                let clip = self.state.lock().clipboard.clone();
                if clip.is_empty() {
                    bail!("내부 클립보드가 비어 있습니다. 사용자의 시스템 클립보드는 읽지 않습니다 — 넣을 글자는 type 으로 직접 입력하세요.");
                }
                let (t, _) = input::sanitize_text(&clip);
                input::post_text(target, &t, &|| self.flush_input());
                self.settle(1000);
                Ok(Some(format!("내부 클립보드의 {}자를 붙여넣었습니다 (글자 입력)", clip.chars().count())))
            }
            "undo" => {
                w32::send_timeout(target, w32::EM_UNDO, 0, 0, 1000);
                Ok(Some("실행 취소 (EM_UNDO)".into()))
            }
            _ => {
                if class.to_lowercase().starts_with("richedit") {
                    w32::send_timeout(target, w32::EM_REDO, 0, 0, 1000);
                    Ok(Some("다시 실행 (EM_REDO)".into()))
                } else {
                    w32::send_timeout(target, w32::EM_UNDO, 0, 0, 1000);
                    Ok(Some("다시 실행 (일반 편집 칸은 실행 취소가 한 단계라 EM_UNDO 로 되돌림)".into()))
                }
            }
        }
    }

    /// (Diagnostics) Sends key messages only. Combos that would open menus on the user's screen or
    /// use the clipboard are still blocked.
    fn raw_key(&self, combo: &str) -> Result<String> {
        let plan = input::plan_combo(combo)?;
        let top = self.key_top()?;
        let target = self.focus_target(top);
        let vk = match plan {
            KeyPlan::Shortcut(a) => {
                if menu::is_clipboard_accel(&a) {
                    bail!("{combo}: {}", menu::R_CLIPBOARD);
                }
                if a.alt {
                    bail!("{combo}: Alt 조합 키 메시지는 메뉴를 사용자 화면에 열 수 있어 보내지 않습니다");
                }
                a.vk
            }
            KeyPlan::Key { vk, .. } => vk,
            KeyPlan::Text(t) => {
                self.lease();
                input::post_text(target, &t, &|| self.flush_input());
                return Ok(format!("{combo} → WM_CHAR {:?}", t));
            }
            KeyPlan::BareModifier => return Ok("수정키만은 보내지 않았습니다".into()),
        };
        // Ctrl/Shift state isn't delivered, so ctrl+F4 is just F4 to the app; check regardless of
        // modifiers
        if let Some(r) = dropdown_key_reason(target, vk, combo) {
            bail!("{r}");
        }
        self.lease();
        if vk >= 0x41 && vk <= 0x5A {
            // character keys sent as WM_KEYDOWN can be turned into other characters by the app's IME (Korean
            // mode)
            bail!("{combo}: 글자 키는 키 메시지 대신 type 으로 보내세요 (입력기 상태에 따라 다른 글자가 될 수 있음)");
        }
        input::post_key(target, vk, false);
        input::post_key(target, vk, true);
        self.settle(500);
        Ok(format!("{combo} 키 메시지(WM_KEYDOWN/UP vk={vk:#04x})를 {} 에 보냈습니다 (Ctrl/Shift 상태는 전달되지 않음)", w32::class_name(target)))
    }

    // ───────────── app_control ─────────────

    fn ui_node(&self, input: &Value) -> Result<uia::Node> {
        let id = input["id"].as_u64().ok_or_else(|| anyhow!("id 가 필요합니다"))? as usize;
        let st = self.state.lock();
        if st.ui_ids.is_empty() {
            bail!("먼저 ui_tree 를 호출하세요");
        }
        if st.ui_for != st.current {
            bail!("창이 바뀌었습니다 — ui_tree 를 다시 호출하세요");
        }
        st.ui_ids
            .get(id.wrapping_sub(1))
            .cloned()
            .ok_or_else(|| anyhow!("id {id} 가 없습니다 (최근 ui_tree 기준 1..{})", st.ui_ids.len()))
    }

    fn app_control(&self, input: &Value) -> Result<String> {
        uia::com_init();
        let action = input["action"].as_str().unwrap_or_default();
        if !matches!(action, "list_windows" | "ui_tree" | "ui" | "menus" | "read") {
            if self.guard.lock().as_ref().map(|g| g.reclaimed()).unwrap_or(false) {
                bail!("사용자가 앱을 다시 화면에 가져와 쓰는 중이라 동작하지 않았습니다. 작업을 멈추고 사용자에게 확인하세요.");
            }
        }
        // actions that change elements get the same checks as key and mouse input: sent to buttons or
        // combo boxes of a window disabled by a modal dialog, the app's command handling would re-enter
        // inside the dialog's modal loop (something a real mouse can't do)
        if matches!(action, "press" | "set_value" | "focus" | "select" | "choose") {
            self.key_top()?;
        }
        match action {
            "list_windows" => {
                self.refresh_windows(true);
                self.ensure_current();
                let st = self.state.lock();
                let mut s = String::new();
                for (i, w) in st.windows.iter().enumerate() {
                    s.push_str(&format!(
                        "{} [{i}] '{}' {} {}x{}{}\n",
                        if Some(w.hwnd) == st.current { "▶" } else { " " },
                        w.title,
                        w.class,
                        w.size.0,
                        w.size.1,
                        if w.enabled { "" } else { " (비활성: 대화상자가 열려 있음)" }
                    ));
                }
                if s.is_empty() {
                    s = "열린 창이 없습니다".into();
                }
                Ok(s)
            }
            "switch_window" => {
                let idx = input["index"].as_u64().ok_or_else(|| anyhow!("index 가 필요합니다"))? as usize;
                self.switch_window(idx)
            }
            "ui_tree" | "ui" => {
                let depth = input["max_depth"].as_u64().unwrap_or(30).clamp(1, 80) as usize;
                self.ui_tree(depth, 500)
            }
            "press" => {
                let n = self.ui_node(input)?;
                self.press_node(&n)
            }
            "set_value" => {
                let n = self.ui_node(input)?;
                let text = input["text"].as_str().or_else(|| input["value"].as_str()).ok_or_else(|| anyhow!("text 가 필요합니다"))?;
                self.set_value(&n, text)
            }
            "focus" => {
                let n = self.ui_node(input)?;
                if !(matches!(n.ctype, uia::CT_EDIT | uia::CT_DOCUMENT) || is_edit_class(&n.class)) {
                    bail!(
                        "{} 에는 포커스만 따로 줄 수 없습니다 — UI Automation 의 SetFocus 는 앱 창을 사용자 화면 앞으로 가져오므로 쓰지 않습니다. \
                         입력 칸은 focus 또는 클릭으로 커서를 둘 수 있습니다.",
                        describe(&n)
                    );
                }
                let r = self.click_node_center(&n, 1)?;
                Ok(format!("{} 에 커서를 두었습니다 ({r})", describe(&n)))
            }
            "select" => {
                let n = self.ui_node(input)?;
                self.lease();
                n.el.select().map_err(|e| anyhow!("{} 을(를) 선택하지 못했습니다: {e}", describe(&n)))?;
                std::thread::sleep(Duration::from_millis(80));
                self.after_action();
                Ok(format!(
                    "{} 선택{}",
                    describe(&n),
                    if n.el.is_selected() == Some(true) { " (확인됨)" } else { " (확인 안 됨)" }
                ))
            }
            "choose" => {
                let n = self.ui_node(input)?;
                let opt = input["option"].as_str().ok_or_else(|| anyhow!("option 이 필요합니다"))?;
                self.choose(&n, opt)
            }
            "menus" => self.menus(),
            "menu" => {
                let path = input["path"].as_str().ok_or_else(|| anyhow!("path 가 필요합니다 (예: \"파일 > 저장\")"))?;
                self.menu(path)
            }
            "read" => {
                let n = self.ui_node(input)?;
                Ok(self.read_node(&n))
            }
            "raw_key" => self.raw_key(input["text"].as_str().unwrap_or_default()),
            other => bail!("알 수 없는 app_control action: {other}"),
        }
    }

    fn ui_tree(&self, max_depth: usize, max_nodes: usize) -> Result<String> {
        let g = self.geom()?;
        let w = self.current_window()?;
        let _dpi = w32::DpiScope::for_window(w.hwnd);
        let origin = w32::client_origin(w.hwnd);
        let (cw, ch) = w32::client_size(w.hwnd);
        let client = (origin.0, origin.1, origin.0 + cw, origin.1 + ch);
        let inter = |r: (i32, i32, i32, i32)| r.0 < client.2 && client.0 < r.2 && r.1 < client.3 && client.1 < r.3;
        let nodes = uia::snapshot(w.hwnd, max_depth, 4000, |n| {
            let r = n.rect;
            r.2 <= r.0 || r.3 <= r.1 || inter(r)
        })?;
        let mut ids = Vec::new();
        let mut lines = Vec::new();
        for (d, n) in nodes {
            let r = n.rect;
            let visible = r.2 > r.0 && r.3 > r.1 && inter(r);
            let boring = n.ctype == uia::CT_TITLEBAR || (matches!(n.ctype, 50025 | 50026 | 50033 | 0) && n.name.trim().is_empty() && n.value.as_deref().unwrap_or("").is_empty());
            if !visible || boring {
                continue;
            }
            if ids.len() >= max_nodes {
                lines.push(format!("… (요소가 많아 {}개에서 멈춤)", ids.len()));
                break;
            }
            ids.push(n.clone());
            let s = g.rect_to_shot(origin, r);
            let mut line = format!("{}{} {}", "  ".repeat(d.saturating_sub(1).min(12)), ids.len(), uia::type_name(n.ctype));
            if !n.name.trim().is_empty() {
                line.push_str(&format!(" \"{}\"", w32::short(n.name.trim(), 50)));
            }
            if let Some(v) = n.value.as_deref().filter(|v| !v.is_empty() && *v != n.name) {
                line.push_str(&format!(" value=\"{}\"", w32::short(v, 60)));
            }
            line.push_str(&format!(" [{},{},{},{}]", s[0], s[1], s[2], s[3]));
            match n.toggle {
                Some(1) => line.push_str(" checked"),
                Some(0) => line.push_str(" unchecked"),
                Some(2) => line.push_str(" mixed"),
                _ => {}
            }
            if n.selected == Some(true) {
                line.push_str(" selected");
            }
            if !n.enabled {
                line.push_str(" disabled");
            }
            if n.focused {
                line.push_str(" focused");
            }
            lines.push(line);
        }
        {
            let mut st = self.state.lock();
            st.ui_ids = ids;
            st.ui_for = Some(w.hwnd);
        }
        Ok(format!("창 '{}' (스크린샷 {}x{} 픽셀 좌표):\n{}", w.title, g.shot_w, g.shot_h, lines.join("\n")))
    }

    /// Clicks the center of an element (through window messages).
    fn click_node_center(&self, n: &uia::Node, count: u32) -> Result<String> {
        let top = self.current_window()?.hwnd;
        let _dpi = w32::DpiScope::for_window(top);
        let r = n.el.current_rect().unwrap_or(n.rect);
        if r.2 <= r.0 || r.3 <= r.1 {
            bail!("{} 의 위치를 알 수 없어 누르지 못했습니다", describe(n));
        }
        let (cx, cy) = ((r.0 + r.2) / 2, (r.1 + r.3) / 2);
        let (target, pt) = if n.hwnd != 0 && w32::is_window(n.hwnd) {
            let o = w32::client_origin(n.hwnd);
            input::hit(n.hwnd, (cx - o.0, cy - o.1))
        } else {
            let top = self.current_window()?.hwnd;
            let o = w32::client_origin(top);
            input::hit(top, (cx - o.0, cy - o.1))
        };
        if let Some(reason) = self.click_block_reason(target) {
            bail!("{reason}");
        }
        let class = w32::class_name(target);
        self.lease();
        input::post_click(target, pt, count, 0);
        self.state.lock().last_target = Some(target);
        self.settle(800);
        Ok(format!("{class} 가운데 클릭"))
    }

    fn press_node(&self, n: &uia::Node) -> Result<String> {
        if !n.enabled || !n.el.current_enabled() {
            bail!("{} 이(가) 비활성화되어 있습니다", describe(n));
        }
        let nm = n.name.trim();
        if n.ctype == uia::CT_HYPERLINK {
            bail!("{} 는 링크입니다. 누르면 브라우저가 사용자 화면 앞에 열리므로 누르지 않았습니다. 주소·내용은 read 로 확인하세요.", describe(n));
        }
        if matches!(n.ctype, uia::CT_BUTTON | uia::CT_MENUITEM | 50031) && menu::is_help_title(nm) {
            bail!("{} — {}", describe(n), menu::R_HELP);
        }
        if menu::is_clipboard_title(nm) && matches!(n.ctype, uia::CT_BUTTON | uia::CT_MENUITEM | 50031) {
            bail!("{} — {}", describe(n), menu::R_CLIPBOARD);
        }
        if n.ctype == uia::CT_COMBOBOX {
            bail!("{} 는 콤보 상자입니다. 펼치면 목록이 사용자 화면에 그려지므로 누르지 않았습니다 — choose {{id, option}} 을 쓰세요.", describe(n));
        }
        if n.ctype == uia::CT_MENUITEM {
            bail!("메뉴 항목은 app_control menu {{path}} 로 실행하세요 (펼쳐진 메뉴는 사용자 화면에 그려집니다)");
        }
        let lname = nm.to_lowercase();
        if n.aid.starts_with("Minimize") || n.aid.starts_with("Maximize") || ["최소화", "최대화", "minimize", "maximize"].contains(&lname.as_str()) {
            bail!("{} 는 창을 최소화·최대화(사용자 화면 크기로) 하므로 누르지 않았습니다", describe(n));
        }
        // Win32 buttons (check boxes and radios included) get real click messages, so the app receives
        // BN_CLICKED
        if n.hwnd != 0 && w32::class_name(n.hwnd).eq_ignore_ascii_case("button") {
            let before = w32::send_timeout(n.hwnd, w32::BM_GETCHECK, 0, 0, 500);
            let r = self.click_node_center(n, 1)?;
            let after = w32::send_timeout(n.hwnd, w32::BM_GETCHECK, 0, 0, 500);
            self.after_action();
            let chk = if matches!(n.ctype, uia::CT_CHECKBOX | uia::CT_RADIO) {
                format!(" (선택 상태 {} → {})", before.unwrap_or(0), after.unwrap_or(0))
            } else {
                String::new()
            };
            return Ok(format!("{} 누름 → {r}{chk}", describe(n)));
        }
        self.lease();
        let how = if n.toggle.is_some() {
            let b = n.el.toggle_state();
            n.el.toggle()?;
            std::thread::sleep(Duration::from_millis(60));
            format!("Toggle ({:?} → {:?})", b, n.el.toggle_state())
        } else if n.invoke {
            n.el.invoke()?;
            "Invoke".to_string()
        } else if n.select {
            n.el.select()?;
            "SelectionItem.Select".to_string()
        } else if n.expand {
            format!("ExpandCollapse ({})", n.el.expand_toggle()?)
        } else {
            self.click_node_center(n, 1)?
        };
        self.settle(800);
        self.after_action();
        Ok(format!("{} 누름 → {how}", describe(n)))
    }

    fn set_value(&self, n: &uia::Node, text: &str) -> Result<String> {
        self.lease();
        let r = n.el.set_value(text);
        std::thread::sleep(Duration::from_millis(80));
        let mut now = n.el.value().unwrap_or_default();
        if now != text && n.hwnd != 0 && is_edit_class(&w32::class_name(n.hwnd)) {
            // if UIA is blocked, WM_SETTEXT (the system marshals the string)
            w32::set_control_text(n.hwnd, text);
            std::thread::sleep(Duration::from_millis(60));
            now = w32::control_text(n.hwnd).unwrap_or_default();
        }
        self.after_action();
        if now.replace("\r\n", "\n") == text.replace("\r\n", "\n") {
            Ok(format!("{} 의 값을 설정했습니다 ({}자, 확인됨)", describe(n), text.chars().count()))
        } else {
            bail!(
                "값 설정이 반영되지 않았습니다{}: 현재 값 '{}'",
                r.err().map(|e| format!(" ({e})")).unwrap_or_default(),
                w32::short(&now, 80)
            )
        }
    }

    fn choose(&self, n: &uia::Node, option: &str) -> Result<String> {
        let want = menu::norm(option);
        let cls = if n.hwnd != 0 { w32::class_name(n.hwnd).to_lowercase() } else { String::new() };
        if n.hwnd != 0 && cls.contains("combobox") {
            let items = w32::combo_items(n.hwnd);
            let idx = items
                .iter()
                .position(|t| menu::norm(t) == want)
                .or_else(|| items.iter().position(|t| menu::norm(t).contains(&want)))
                .ok_or_else(|| anyhow!("'{option}' 선택지가 없습니다. 선택지: [{}]", items.iter().take(40).cloned().collect::<Vec<_>>().join(", ")))?;
            self.lease();
            w32::send_timeout(n.hwnd, w32::CB_SETCURSEL, idx, 0, 1000);
            // send CBN_SELCHANGE to the parent so the app learns of the change (CB_SETCURSEL doesn't notify)
            let parent = w32::parent(n.hwnd);
            w32::post(parent, w32::WM_COMMAND, w32::make_wparam(w32::ctrl_id(n.hwnd) as u32, w32::CBN_SELCHANGE), n.hwnd);
            self.settle(800);
            let cur = w32::send_timeout(n.hwnd, w32::CB_GETCURSEL, 0, 0, 1000).map(|c| c as isize);
            self.after_action();
            return Ok(format!(
                "{} 에서 '{}' 선택 ({})",
                describe(n),
                items[idx],
                if cur == Some(idx as isize) { "확인됨" } else { "확인 안 됨" }
            ));
        }
        if let Some(item) = n.el.find_descendant_named(option) {
            self.lease();
            if item.select().is_ok() || item.invoke().is_ok() {
                self.settle(800);
                self.after_action();
                return Ok(format!("{} 에서 '{}' 선택 (UI Automation)", describe(n), item.name()));
            }
        }
        if !n.readonly {
            return self.set_value(n, option);
        }
        bail!(
            "{} 의 선택지는 목록을 펼쳐야만 보이는데, 펼치면 사용자 화면에 그려지므로 열 수 없습니다. 같은 기능의 메뉴(app_control menus)나 키보드를 쓰세요.",
            describe(n)
        )
    }

    fn read_node(&self, n: &uia::Node) -> String {
        let mut s = format!("{}{}\n", uia::type_name(n.ctype), if n.class.is_empty() { String::new() } else { format!(" ({})", n.class) });
        if !n.name.is_empty() {
            s.push_str(&format!("name: {}\n", n.name));
        }
        if !n.aid.is_empty() {
            s.push_str(&format!("automation id: {}\n", n.aid));
        }
        if let Some(d) = n.el.legacy_description() {
            s.push_str(&format!("description: {d}\n"));
        }
        if let Some(t) = n.el.toggle_state() {
            s.push_str(&format!("toggle: {}\n", ["off", "on", "mixed"].get(t as usize).unwrap_or(&"?")));
        }
        if let Some(sel) = n.el.selection_text().filter(|s| !s.is_empty()) {
            s.push_str(&format!("selected text: {sel}\n"));
        }
        let full = if n.hwnd != 0 && is_edit_class(&w32::class_name(n.hwnd)) {
            w32::control_text(n.hwnd)
        } else {
            None
        }
        .or_else(|| n.el.value())
        .or_else(|| n.el.document_text())
        .unwrap_or_default();
        s.push_str(&format!("value:\n{full}"));
        s
    }

    fn menus(&self) -> Result<String> {
        // read only, so disabled windows (a main window with a dialog open) can be inspected too
        let top = self.current_window()?.hwnd;
        let Some((owner, bar)) = menu::menu_owner(top) else {
            let items = uia::menubar_items(top);
            if items.is_empty() {
                return Ok("이 창에는 메뉴 막대가 없습니다 (단추·입력 칸은 app_control ui_tree 로 찾으세요)".into());
            }
            return Ok(format!(
                "메뉴 막대(UI Automation): {} — 이 앱의 메뉴는 펼쳐야만 하위 항목이 보이는데, 펼치면 사용자 화면에 그려지므로 하위 항목은 실행할 수 없습니다.",
                items.iter().map(|(n, e)| if *e { n.clone() } else { format!("{n} [비활성]") }).collect::<Vec<_>>().join(" · ")
            ));
        };
        // an owner disabled by a modal dialog doesn't even get menu refresh messages (WM_INITMENU,
        // WM_INITMENUPOPUP)
        let usable = menu::owner_usable(owner);
        if usable {
            let _ = w32::send_timeout(owner, w32::WM_INITMENU, bar as usize, 0, 500);
        }
        let refresh = |i: usize| usable.then_some((owner, i));
        let mut out = String::new();
        if !usable {
            out.push_str(
                "(모달 대화상자가 열려 있어 본 창이 비활성입니다 — 대화상자를 끝내거나 닫기 전에는 메뉴·단축키를 실행할 수 없고, \
                 항목의 활성 상태는 최신이 아닐 수 있습니다)\n",
            );
        }
        for (i, top_it) in menu::items(bar, None).into_iter().enumerate() {
            if top_it.separator {
                continue;
            }
            if menu::is_help_title(&top_it.title) {
                out.push_str(&format!("{}: [차단 — {}]\n", top_it.title, menu::R_HELP));
                continue;
            }
            let Some(sub) = top_it.sub else {
                out.push_str(&format!("{} (바로 실행되는 항목){}\n", top_it.title, if top_it.disabled { " [비활성]" } else { "" }));
                continue;
            };
            let mut shown = Vec::new();
            for it in menu::items(sub, refresh(i)) {
                if it.separator {
                    continue;
                }
                let mut s = if it.owner_drawn && it.title.is_empty() { "(그림 항목)".to_string() } else { it.title.clone() };
                if let Some(a) = &it.accel_text {
                    s.push_str(&format!(" ({a})"));
                }
                if it.sub.is_some() {
                    let names: Vec<String> = menu::items(it.sub.unwrap_or(0), refresh(0))
                        .into_iter()
                        .filter(|x| !x.separator)
                        .map(|x| x.title)
                        .take(15)
                        .collect();
                    s.push_str(&format!(" ▸ [{}]", names.join(", ")));
                }
                if it.checked {
                    s.push_str(" ✓");
                }
                if menu::block_reason(&top_it.title, &it).is_some() {
                    s.push_str(" [차단]");
                } else if it.disabled {
                    s.push_str(" [비활성]");
                }
                shown.push(s);
            }
            out.push_str(&format!("{}: {}\n", top_it.title, shown.join(" · ")));
        }
        if out.is_empty() {
            out = "메뉴 막대가 비어 있습니다".into();
        }
        Ok(out)
    }

    fn menu(&self, path: &str) -> Result<String> {
        let parts: Vec<String> = path.split('>').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            bail!("메뉴 경로가 비었습니다");
        }
        // same checks as other input (reclaimed by the user, current window disabled)
        let top = self.key_top()?;
        let Some((owner, bar)) = menu::menu_owner(top) else {
            bail!("이 창에는 (Win32) 메뉴 막대가 없습니다. app_control menus 로 확인하세요.");
        };
        if !menu::owner_usable(owner) {
            bail!("메뉴 '{path}': {}", menu::R_MODAL);
        }
        let _ = w32::send_timeout(owner, w32::WM_INITMENU, bar as usize, 0, 500);
        let tops = menu::items(bar, None);
        let find = |items: &[menu::Item], part: &str| -> Option<usize> {
            let want = menu::norm(part);
            items
                .iter()
                .position(|t| !t.separator && menu::norm(&t.title) == want)
                .or_else(|| items.iter().position(|t| !t.separator && !t.title.is_empty() && menu::norm(&t.title).starts_with(&want)))
        };
        let ti = find(&tops, &parts[0]).ok_or_else(|| {
            anyhow!(
                "메뉴 막대에서 '{}' 를 찾지 못했습니다. 있는 메뉴: [{}]",
                parts[0],
                tops.iter().filter(|t| !t.separator).map(|t| t.title.as_str()).collect::<Vec<_>>().join(", ")
            )
        })?;
        let top_it = &tops[ti];
        if menu::is_help_title(&top_it.title) {
            bail!("메뉴 '{}': {}", top_it.title, menu::R_HELP);
        }
        let mut walked = vec![top_it.title.clone()];
        let mut item = top_it.clone();
        let mut items = match top_it.sub {
            Some(s) => menu::items(s, Some((owner, ti))),
            None => vec![],
        };
        for part in parts.iter().skip(1) {
            if items.is_empty() {
                bail!("'{}' 에는 하위 메뉴가 없습니다", walked.join(" > "));
            }
            let i = find(&items, part).ok_or_else(|| {
                anyhow!(
                    "메뉴 '{}' 에서 '{part}' 를 찾지 못했습니다. 있는 항목: [{}]",
                    walked.join(" > "),
                    items.iter().filter(|t| !t.separator).map(|t| t.title.as_str()).take(40).collect::<Vec<_>>().join(", ")
                )
            })?;
            item = items[i].clone();
            walked.push(item.title.clone());
            if let Some(r) = menu::block_reason(&top_it.title, &item) {
                bail!("메뉴 '{}': {r}", walked.join(" > "));
            }
            items = match item.sub {
                Some(s) => menu::items(s, Some((owner, i))),
                None => vec![],
            };
        }
        if item.sub.is_some() {
            bail!(
                "'{}' 는 하위 메뉴입니다. 하위 항목까지 지정하세요: [{}]",
                walked.join(" > "),
                items.iter().filter(|t| !t.separator).map(|t| t.title.as_str()).take(30).collect::<Vec<_>>().join(", ")
            );
        }
        if item.disabled {
            bail!("메뉴 '{}' 가 비활성화되어 있습니다", walked.join(" > "));
        }
        self.lease();
        menu::invoke(owner, item.id);
        self.settle(1000);
        self.after_action();
        Ok(format!("메뉴 '{}' 실행 (WM_COMMAND {}) — 결과는 스크린샷으로 확인하세요", walked.join(" > "), item.id))
    }

    fn shutdown_sync(&self) {
        if self.shut.swap(true, Ordering::SeqCst) {
            return;
        }
        self.closed.store(true, Ordering::SeqCst);
        match self.mode {
            NativeMode::Launched => {
                if self.proc_.alive() || !self.proc_.pids().is_empty() {
                    // if a dialog like "Save changes?" shows up, the guard moves it off-screen and the whole Job is
                    // ended after 2 s
                    self.lease();
                    let pids = self.pids();
                    let list = move || -> Vec<Hwnd> {
                        pw::windows_of(&pids).into_iter().filter(guard::is_view_window).map(|w| w.hwnd).collect()
                    };
                    let wins: Vec<Hwnd> = match &self.desk {
                        Some(d) => d.thread.run(list).unwrap_or_default(),
                        None => list(),
                    };
                    for h in wins {
                        w32::post(h, w32::WM_CLOSE, 0, 0);
                    }
                    if !self.proc_.wait_exit(2000) {
                        eprintln!("[ghostdesk] Windows 앱이 2초 안에 닫히지 않아 강제 종료: pid={}", self.pid);
                    }
                    self.proc_.terminate();
                    self.proc_.wait_exit(3000);
                }
                if let Some(mut g) = self.guard.lock().take() {
                    // keep logging during shutdown too (e.g. exposure of the save prompt)
                    std::thread::sleep(Duration::from_millis(50));
                    g.stop();
                    self.exposure.lock().extend(g.take_events());
                }
                eprintln!("[ghostdesk] Windows 앱 인스턴스 종료: pid={}", self.pid);
            }
            NativeMode::Attached => {
                if let Some(mut g) = self.guard.lock().take() {
                    let n = g.restore_all();
                    g.stop();
                    self.exposure.lock().extend(g.take_events());
                    eprintln!("[ghostdesk] 사용자 앱 창 {n}개를 원래 자리로 되돌림: pid={}", self.pid);
                }
            }
        }
    }
}

// ---- helpers ----

/// Whether it's safe to send drags to a control through messages.
///
/// If a posted WM_MOUSEMOVE (MK_LBUTTON) makes the app start an OLE drag and drop (DoDragDrop),
/// OLE uses the **real** mouse button state (released) and **real** cursor position to pick the
/// drop target, so the content could land in whatever user app sits under the cursor. Only
/// standard controls that never start OLE drags (scroll bars, sliders, plain edit boxes,
/// spinners) are allowed.
pub fn drag_safe(class: &str) -> bool {
    let l = class.to_lowercase();
    let base = l.strip_prefix("windowsforms10.").map(|r| r.split('.').next().unwrap_or("")).unwrap_or(&l);
    matches!(base, "scrollbar" | "msctls_trackbar32" | "edit" | "msctls_updown32")
}

fn drag_refusal(class: &str) -> String {
    format!(
        "{class} 에는 끌기를 보내지 않았습니다 — 끌어서 놓기를 지원하는 컨트롤은 실제 마우스 커서 아래(사용자 화면)에 있는 사용자의 앱에 \
         내용을 놓아 버릴 수 있습니다. 끌기는 스크롤 막대·슬라이더·일반 입력 칸에서만 합니다. 스크롤은 scroll, 값은 app_control set_value, \
         선택은 app_control select 를 쓰세요."
    )
}

/// Scroll bar to use instead of the wheel: `(window receiving WM_xSCROLL, bar control passed as
/// lParam (0 = the window's own bar), window to read the position from, bar kind)`.
///
/// Walks up from the control under the point, looking for the window's own scroll bar
/// (WS_VSCROLL/WS_HSCROLL) or a sibling scroll bar control right next to it.
fn find_scrollbar(target: Hwnd, top: Hwnd, vertical: bool) -> Option<(Hwnd, isize, Hwnd, i32)> {
    let bit = if vertical { w32::WS_VSCROLL } else { w32::WS_HSCROLL };
    let mut w = target;
    for _ in 0..16 {
        if w == 0 {
            break;
        }
        if w32::style(w) & bit != 0 {
            return Some((w, 0, w, if vertical { 1 } else { 0 }));
        }
        if w == top {
            break;
        }
        let parent = w32::parent(w);
        let (l, t, r, b) = w32::window_rect(w);
        for c in w32::child_windows(parent) {
            if w32::parent(c) != parent || !w32::is_visible(c) || !w32::class_name(c).eq_ignore_ascii_case("scrollbar") {
                continue;
            }
            let sbs_vert = w32::style(c) & 1 != 0;
            if sbs_vert != vertical {
                continue;
            }
            let (cl, ct, cr, cb) = w32::window_rect(c);
            let near = if vertical {
                cl >= r - 4 && cl <= r + 40 && ct < b && t < cb
            } else {
                ct >= b - 4 && ct <= b + 40 && cl < r && l < cr
            };
            if near {
                return Some((parent, c, c, 2));
            }
        }
        w = parent;
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Dropdown {
    /// Combo box (drop-down list).
    Combo,
    /// Date picker (calendar).
    Date,
}

/// Kinds of controls that open a popup (list, calendar) on the user's screen when clicked or
/// keyed. Matched by substring of the class name: ComboBox, ComboBoxEx32,
/// WindowsForms10.COMBOBOX…, Delphi TComboBox, SysDateTimePick32, Delphi TDateTimePicker, ….
fn dropdown_kind(class: &str) -> Option<Dropdown> {
    let l = class.to_lowercase();
    if l.contains("combobox") {
        Some(Dropdown::Combo)
    } else if l.contains("datetimepick") {
        Some(Dropdown::Date)
    } else {
        None
    }
}

const VK_UP: u32 = 0x26;
const VK_DOWN: u32 = 0x28;
const VK_F4: u32 = 0x73;

/// `(control, kind)` if the key target (or its parent, e.g. the edit inside a combo box) is a
/// drop-down control.
fn dropdown_of(target: Hwnd) -> Option<(Hwnd, Dropdown)> {
    if let Some(k) = dropdown_kind(&w32::class_name(target)) {
        return Some((target, k));
    }
    if w32::style(target) & w32::WS_CHILD != 0 {
        let p = w32::parent(target);
        if p != 0 {
            if let Some(k) = dropdown_kind(&w32::class_name(p)) {
                return Some((p, k));
            }
        }
    }
    None
}

/// Reason to refuse a key that would open a drop-down list or calendar on the user's screen: F4
/// (combo boxes, date pickers), Up/Down (extended-UI combo boxes, CB_SETEXTENDEDUI).
fn dropdown_key_reason(target: Hwnd, vk: u32, combo: &str) -> Option<String> {
    if !matches!(vk, VK_F4 | VK_UP | VK_DOWN) {
        return None;
    }
    let (ctl, kind) = dropdown_of(target)?;
    match kind {
        Dropdown::Combo if vk == VK_F4 || w32::combo_extended_ui(ctl) => Some(format!(
            "{combo}: 이 콤보 상자({})에 보내면 드롭다운 목록이 사용자 화면에 열리므로 보내지 않았습니다{}. \
             app_control ui_tree 로 id 를 확인한 뒤 app_control choose {{id, option}} 을 쓰세요.",
            w32::class_name(ctl),
            if vk == VK_F4 { "" } else { " (위/아래 화살표로 목록이 열리는 확장 UI 콤보 상자)" }
        )),
        Dropdown::Date if vk == VK_F4 => Some(format!(
            "{combo}: 날짜 선택 컨트롤에 보내면 달력이 사용자 화면에 열리므로 보내지 않았습니다. 날짜는 숫자·화살표 키로 바꾸세요."
        )),
        _ => None,
    }
}

/// Whether this is an edit control (Edit, RichEdit, WinForms TextBox, Delphi TEdit/TMemo, …).
pub fn is_edit_class(class: &str) -> bool {
    let l = class.to_lowercase();
    (l.contains("edit") || l.contains("memo")) && !l.contains("combobox")
}

fn describe(n: &uia::Node) -> String {
    let name = n.name.trim();
    if name.is_empty() {
        uia::type_name(n.ctype).to_string()
    } else {
        format!("{} '{}'", uia::type_name(n.ctype), w32::short(name, 40))
    }
}

/// Live view: captures the current window at ~`live_fps` and sends JPEGs to the sink. Also logs
/// exposure and refreshes the window list.
async fn live_loop(weak: std::sync::Weak<Inner>, fps: f64) {
    let period = Duration::from_secs_f64(1.0 / fps.clamp(0.2, 10.0));
    let mut tick = 0u64;
    loop {
        let Some(inner) = weak.upgrade() else { break };
        if inner.closed.load(Ordering::SeqCst) {
            let _ = tokio::task::spawn_blocking(move || drop(inner)).await;
            break;
        }
        tick += 1;
        let desk = inner.desk.clone();
        // only hold a strong reference inside the worker thread (if the last reference drops here,
        // shutdown doesn't block a tokio worker)
        let _ = exec(&desk, move || {
            let i = inner;
            if i.process_gone() {
                i.closed.store(true, Ordering::SeqCst);
                return;
            }
            i.guard_exposure();
            if tick % 4 == 1 {
                i.refresh_windows(true);
            }
            if tick % 10 == 3 {
                i.check_escaped();
            }
            if i.state.lock().current.is_none() {
                i.ensure_current();
            }
            if let Ok((img, _)) = i.capture_current(960) {
                if let Ok(jpg) = capture::encode_jpeg(&img, 70) {
                    (i.sink)(&jpg);
                }
            }
        })
        .await;
        tokio::time::sleep(period).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn current_session_rejects_attach_before_resolving_app() {
        for (attach_pid, attach_consent) in [(Some(123), false), (None, true)] {
            let opts = NativeOptions {
                app: "ghostdesk-no-such-current-session-app".into(),
                host: NativeHost::CurrentSession,
                attach_pid,
                attach_consent,
                ..Default::default()
            };
            let error = NativeAppWorkspace::launch(opts, Arc::new(|_| {})).await.err().unwrap().to_string();
            assert!(error.contains("current_session"), "{error}");
        }
    }

    #[test]
    fn edit_classes() {
        for c in ["Edit", "RICHEDIT50W", "RichEdit20W", "WindowsForms10.EDIT.app.0.141b42a_r9_ad1", "TMemo", "TEdit"] {
            assert!(is_edit_class(c), "{c}");
        }
        for c in ["Button", "ComboBox", "Static", "WindowsForms10.COMBOBOX.app.0", "#32770"] {
            assert!(!is_edit_class(c), "{c}");
        }
    }

    #[test]
    fn dropdown_classes() {
        for c in ["ComboBox", "ComboBoxEx32", "WindowsForms10.COMBOBOX.app.0.141b42a_r9_ad1", "TComboBox", "TDBLookupComboBox"] {
            assert_eq!(dropdown_kind(c), Some(Dropdown::Combo), "{c}");
        }
        for c in ["SysDateTimePick32", "TDateTimePicker", "WindowsForms10.SysDateTimePick32.app.0"] {
            assert_eq!(dropdown_kind(c), Some(Dropdown::Date), "{c}");
        }
        for c in ["Edit", "TEdit", "ComboLBox", "Button", "SysListView32"] {
            assert_eq!(dropdown_kind(c), None, "{c}");
        }
        // keys and windows unrelated to drop-downs pass through
        assert_eq!(dropdown_key_reason(0, 0x0D, "Return"), None);
        assert_eq!(dropdown_key_reason(0, VK_F4, "F4"), None);
    }
}
