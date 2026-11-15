//! egui 화면: 에이전트 생성, 다중 에이전트 라이브 그리드, 상세 보기(로그/지시/직접 조작).

use std::sync::{atomic::Ordering, Arc};

use eframe::egui::{self, Color32, Margin, RichText, Stroke, StrokeKind, Vec2};
use tokio::{runtime::Handle, sync::mpsc};

use crate::{
    icon,
    agent::{self, AgentCmd, AgentConfig, AgentShared, AgentStatus, DeskTarget, LogKind, ProfileSource, RemoteTarget},
    browser::MouseButton,
    llm::{Llm, EFFORTS, MODELS},
    platform::{caps, text},
    profiles::{self, InstalledBrowser},
};

mod theme;

const SIZES: &[(u32, u32)] = &[(1280, 800), (1366, 768), (1440, 900), (1024, 768)];

/// '로컬 앱' 카드를 보여 줄지 (Electron 숨김 실행 또는 일반 앱 백엔드가 있는 OS)
const LOCAL_APP_CARD: bool = caps::ELECTRON || caps::NATIVE_APPS;

/// macOS 에서는 기존 문구를, 그 밖의 OS 에서는 그 OS 에 맞는 문구를 고른다.
/// Windows 자식 세션 켜기(UAC) 요청 상태
#[derive(Default, Clone, PartialEq)]
enum ChildEnable {
    #[default]
    Idle,
    Waiting,
    Done(Result<(), String>),
}

fn pick(mac: &'static str, other: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        mac
    } else {
        other
    }
}

struct AgentHandle {
    shared: Arc<AgentShared>,
    tx: mpsc::UnboundedSender<AgentCmd>,
    task: tokio::task::JoinHandle<()>,
    /// 직접 조작 입력 큐: 한 task 가 순서대로 적용한다 (이벤트마다 spawn 하면 순서가 뒤섞임)
    input_tx: mpsc::UnboundedSender<TakeoverInput>,
    texture: Option<egui::TextureHandle>,
    tex_seq: u64,
    /// 가상 마우스 표시 상태 (상세 보기 / 그리드 타일)
    cursor_view: crate::cursor::CursorView,
    tile_cursor_view: crate::cursor::CursorView,
    input: String,
    url_input: String,
    takeover: bool,
    /// 직접 조작 중 입력기(IME)가 조합하고 있는 글자 (Windows 한글 입력 표시용, 확정되면 비움)
    preedit: String,
}

/// 사용자의 직접 조작 입력 하나 (에이전트 좌표)
#[derive(Debug, Clone, PartialEq)]
enum TakeoverInput {
    /// 드래그 시작: 그 지점으로 이동한 뒤 버튼 누름
    DragStart(f64, f64),
    MouseMove(f64, f64),
    MouseUp,
    Click { pos: (f64, f64), button: MouseButton, count: u32 },
    Type(String),
    /// 사람이 누른 키 조합 (클립보드 조합도 그대로 전달)
    Key(String),
    Scroll { pos: Option<(f64, f64)>, dx: f64, dy: f64 },
}

impl TakeoverInput {
    async fn apply(self, w: &crate::desk::Desk) -> anyhow::Result<()> {
        match self {
            TakeoverInput::DragStart(x, y) => {
                w.mouse_move(x, y, 0).await?;
                w.mouse_down(MouseButton::Left, 1, 0).await
            }
            TakeoverInput::MouseMove(x, y) => w.mouse_move(x, y, 0).await,
            TakeoverInput::MouseUp => w.mouse_up(MouseButton::Left, 1, 0).await,
            TakeoverInput::Click { pos, button, count } => w.click(Some(pos), button, count, 0).await,
            TakeoverInput::Type(t) => w.type_text(&t).await,
            TakeoverInput::Key(k) => w.key_by_human(&k, 1).await,
            TakeoverInput::Scroll { pos, dx, dy } => w.scroll(pos, dx, dy, 0).await,
        }
    }
}

/// 직접 조작 입력 큐와 그것을 순서대로 적용하는 task 하나. 보내는 쪽(AgentHandle)이 사라지면 task 도 끝난다.
fn spawn_input_queue(rt: &Handle, shared: Arc<AgentShared>) -> mpsc::UnboundedSender<TakeoverInput> {
    let (tx, mut rx) = mpsc::unbounded_channel::<TakeoverInput>();
    rt.spawn(async move {
        while let Some(ev) = rx.recv().await {
            if shared.removed.load(Ordering::SeqCst) {
                break;
            }
            if let Some(w) = shared.workspace() {
                let _ = ev.apply(&w).await;
            }
        }
    });
    tx
}

#[derive(Default, PartialEq)]
enum View {
    #[default]
    Home,
    Grid,
    Detail,
    /// 화면 모아보기: 모든 에이전트의 화면만 창 가득 크게 (목록·패널 없음)
    Wall,
}

/// 새 에이전트가 쓸 격리 화면 종류
#[derive(Clone, Copy, PartialEq)]
enum FormKind {
    Browser,
    Electron,
    Native,
    Remote,
    Session,
}

/// 새 에이전트 창의 첫 선택
#[derive(Clone, Copy, PartialEq)]
enum Choice {
    Web,
    App,
    Session,
    Remote,
}

struct NewAgentForm {
    choice: Option<Choice>,
    app_search: String,
    /// 웹사이트: 내 브라우저 로그인 상태 사용 (어느 프로필인지는 profile_idx — 고급 설정에서 고름)
    web_use_login: bool,
    /// Mac 앱(Electron): 내 앱 데이터(로그인) 복제 사용
    app_use_login: bool,
    /// 별도 로그인 세션용 전용 계정 이름
    session_account: String,
    kind: FormKind,
    electron_idx: Option<usize>,
    electron_clone: bool,
    electron_clone_consent: bool,
    electron_refresh: bool,
    native_app: String,
    native_new_instance: bool,
    native_attach: bool,
    native_current_session: bool,
    /// 실험: 가상 디스플레이 위에서 실행 + 그 부작용에 대한 동의
    native_vdisplay: bool,
    native_vdisplay_consent: bool,
    remote_host: String,
    remote_port: u16,
    remote_user: String,
    remote_pass: String,
    remote_dedicated: bool,
    name: String,
    workspace: String,
    refresh: bool,
    browser_idx: usize,
    /// 복제할 프로필 (마법사에서는 web_use_login 이 켜졌을 때만 쓴다). None = 빈 프로필
    profile_idx: Option<usize>,
    start_url: String,
    task: String,
    headless: bool,
    size_idx: usize,
    max_steps: u64,
}

pub struct GhostApp {
    rt: Handle,
    api_key: String,
    model_idx: usize,
    effort_idx: usize,
    browsers: Vec<InstalledBrowser>,
    /// 백그라운드에서 감지한 Electron 앱 목록 (None = 감지 중)
    electron_apps: Arc<parking_lot::Mutex<Option<Vec<crate::electron::ElectronApp>>>>,
    /// 로컬 앱 (이름, 경로). macOS: /Applications 의 .app (시작할 때 바로 읽음),
    /// Windows: 설치된 프로그램 (백그라운드에서 읽음, None = 확인 중)
    native_apps: Arc<parking_lot::Mutex<Option<Vec<(String, String)>>>>,
    form: NewAgentForm,
    agents: Vec<AgentHandle>,
    selected: Option<usize>,
    view: View,
    next_id: u64,
    /// 에이전트 화면 위에 가상 마우스를 그릴지
    show_cursor: bool,
    /// 외부 에이전트(MCP) 제어 서버에서 오는 이벤트
    control_rx: mpsc::UnboundedReceiver<crate::control::ControlEvent>,
    /// 제어 서버 상태 (None = 정상 동작 중)
    control_error: Arc<parking_lot::Mutex<Option<String>>>,
    /// 사용자 승인 대기 중인 외부 요청
    approvals: Vec<Approval>,
    /// 승인 요청 때문에 Dock 주의 요청(바운스)을 보낸 상태인가
    attention_requested: bool,
    /// 외부 연결 안내 창 표시
    show_connect: bool,
    /// 자동 연결 결과/상태 (백그라운드에서 갱신)
    connect_state: Arc<parking_lot::Mutex<ConnectState>>,
    /// 새 에이전트 창 / 설정 창 표시
    wizard_open: bool,
    show_settings: bool,
    show_session_guide: bool,
    /// Windows 자식 세션: 기능이 켜져 있는가 (None = 아직 확인 안 함) / 켜기 요청 진행·결과
    child_enabled: Option<bool>,
    child_enable: Arc<parking_lot::Mutex<ChildEnable>>,
    /// 개발용: GHOSTDESK_DEMO=<n> 이면 빈 프로필 브라우저 에이전트 n 개를 자동 시작
    pending_demo: u32,
    toast: Option<(String, f64)>,
    /// 화면 모아보기에서 전체 화면으로 켰는가 (모아보기를 닫으면 되돌린다)
    wall_fullscreen: bool,
    /// API 키 없이 시작을 눌렀음 (새 에이전트 창의 키 칸 아래에 안내를 보인다)
    key_warn: bool,
    /// 새 에이전트 창에 API 키 칸을 보일지 (창을 열 때 키가 없었으면 — 입력하는 동안 칸이 사라지지 않게 고정)
    wizard_key_field: bool,
    /// 다음 프레임에 API 키 칸으로 입력 포커스를 옮긴다
    focus_key: bool,
    /// 화면 모아보기를 열 때 이미 전체 화면이었는가 (닫을 때 사용자의 전체 화면은 그대로 둔다)
    wall_fs_before: bool,
}

impl GhostApp {
    pub fn new(cc: &eframe::CreationContext<'_>, rt: Handle) -> Self {
        setup_fonts(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx);
        let browsers = profiles::detect();
        let profile_idx = browsers.first().and_then(|b| if b.profiles.is_empty() { None } else { Some(0) });
        let electron_apps = Arc::new(parking_lot::Mutex::new(None));
        {
            let slot = electron_apps.clone();
            let ctx = cc.egui_ctx.clone();
            rt.spawn_blocking(move || {
                let apps = crate::electron::detect_apps();
                *slot.lock() = Some(apps);
                ctx.request_repaint();
            });
        }
        let (ctl_tx, control_rx) = mpsc::unbounded_channel();
        let control_error = Arc::new(parking_lot::Mutex::new(None));
        {
            let server = crate::control::ControlServer::new(ctl_tx, cc.egui_ctx.clone());
            let err = control_error.clone();
            rt.spawn(async move {
                if let Err(e) = server.serve().await {
                    *err.lock() = Some(format!("{e:#}"));
                }
            });
        }
        let connect_state = Arc::new(parking_lot::Mutex::new(ConnectState {
            auto: crate::install::auto_connect_enabled(),
            install_dir: crate::install::stable_app().display().to_string(),
            ..Default::default()
        }));
        if crate::install::auto_connect_enabled() {
            spawn_connect(&rt, connect_state.clone(), cc.egui_ctx.clone());
        } else {
            refresh_connect_status(&rt, connect_state.clone(), cc.egui_ctx.clone());
        }
        let native_apps = Arc::new(parking_lot::Mutex::new(None));
        #[cfg(target_os = "macos")]
        {
            *native_apps.lock() = Some(list_mac_apps());
        }
        // Windows: 설치된 프로그램 목록(레지스트리·시작 메뉴 등)은 느릴 수 있어 백그라운드에서 읽는다
        #[cfg(not(target_os = "macos"))]
        if caps::NATIVE_APPS {
            let slot = native_apps.clone();
            let ctx = cc.egui_ctx.clone();
            rt.spawn_blocking(move || {
                let apps = crate::native::installed_apps();
                *slot.lock() = Some(apps);
                ctx.request_repaint();
            });
        } else {
            *native_apps.lock() = Some(Vec::new());
        }
        GhostApp {
            connect_state,
            control_rx,
            control_error,
            approvals: Vec::new(),
            attention_requested: false,
            show_connect: false,
            wizard_open: false,
            show_settings: false,
            show_session_guide: false,
            child_enabled: None,
            child_enable: Default::default(),
            electron_apps,
            native_apps,
            rt,
            api_key: std::env::var("ANTHROPIC_API_KEY").unwrap_or_default(),
            model_idx: 0,
            effort_idx: 1,
            browsers,
            form: NewAgentForm {
                choice: None,
                app_search: String::new(),
                web_use_login: false,
                app_use_login: false,
                session_account: "ghostdesk-agent".into(),
                kind: FormKind::Browser,
                electron_idx: None,
                electron_clone: false,
                electron_clone_consent: false,
                electron_refresh: false,
                native_app: String::new(),
                native_new_instance: false,
                native_attach: false,
                native_current_session: false,
                native_vdisplay: false,
                native_vdisplay_consent: false,
                remote_host: String::new(),
                remote_port: 5900,
                remote_user: String::new(),
                remote_pass: String::new(),
                remote_dedicated: false,
                name: "에이전트 1".into(),
                workspace: "ws-1".into(),
                refresh: false,
                browser_idx: 0,
                profile_idx,
                start_url: "https://www.google.com".into(),
                task: String::new(),
                headless: true,
                size_idx: 0,
                max_steps: 60,
            },
            agents: Vec::new(),
            selected: None,
            view: View::default(),
            next_id: 1,
            show_cursor: true,
            pending_demo: std::env::var("GHOSTDESK_DEMO").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
            toast: None,
            wall_fullscreen: false,
            key_warn: false,
            wizard_key_field: false,
            focus_key: false,
            wall_fs_before: false,
        }
    }

    /// 외부 에이전트의 민감한 요청을 사용자에게 묻는 창 (모달: 답할 때까지 다른 조작을 막는다)
    fn approval_window(&mut self, ctx: &egui::Context) {
        // 요청한 쪽이 시간 초과·연결 끊김으로 응답을 더 받지 않으면(수신자 drop) 창을 치운다
        prune_closed_approvals(&mut self.approvals);
        let Some((title, detail, _)) = self.approvals.first() else {
            if self.attention_requested {
                self.attention_requested = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Reset));
            }
            return;
        };
        // 입력이 없어도 시간 초과된 요청이 정리되도록 주기적으로 다시 그린다
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
        let (title, detail) = (title.clone(), detail.clone());
        let mut answer = None;
        // 바깥 클릭·Esc 로는 닫지 않는다: 반드시 허용/거부 중 하나를 고른다
        egui::Modal::new(egui::Id::unique("approval")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::SHIELD_ALERT).size(20.0).color(theme::WARN));
                ui.label(RichText::new(&title).size(18.0).strong().color(theme::TEXT));
            });
            ui.add_space(6.0);
            ui.label(RichText::new(&detail).color(theme::BODY));
            ui.add_space(16.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::primary(ui, "   허용   ").clicked() {
                    answer = Some(true);
                }
                if theme::secondary(ui, "   거부   ").clicked() {
                    answer = Some(false);
                }
            });
        });
        if let Some(a) = answer {
            let (_, _, reply) = self.approvals.remove(0);
            if let Some(r) = reply {
                let _ = r.send(a);
            }
        }
    }

    fn notify(&mut self, ctx: &egui::Context, msg: impl Into<String>) {
        self.toast = Some((msg.into(), ctx.input(|i| i.time) + 4.0));
    }

    fn spawn_agent(&mut self, ctx: &egui::Context) {
        if self.api_key.trim().is_empty() {
            self.key_warn = true;
            if self.wizard_open {
                // 새 에이전트 창 안에서는 키 칸 아래 안내 + 키 칸으로 포커스 (아래쪽 알림은 겹치므로 생략)
                self.wizard_key_field = true;
                self.focus_key = true;
                return;
            }
            return self.notify(ctx, "Anthropic API 키를 입력하세요 (또는 ANTHROPIC_API_KEY 환경변수).");
        }
        self.key_warn = false;
        let ws_name = self.form.workspace.trim().to_string();
        if ws_name.is_empty() {
            return self.notify(ctx, "작업공간 이름을 입력하세요.");
        }
        if self.agents.iter().any(|a| profiles::slug(&a.shared.workspace_name) == profiles::slug(&ws_name)) {
            return self.notify(ctx, "같은 작업공간을 쓰는 에이전트가 이미 있습니다. 다른 이름을 쓰세요.");
        }
        // Windows 는 CON·NUL·COM1 같은 장치 이름을 폴더 이름으로 쓸 수 없다
        if cfg!(windows) && crate::platform::paths::is_reserved_name(&profiles::slug(&ws_name)) {
            return self.notify(ctx, format!("'{ws_name}' 은(는) Windows 에서 폴더 이름으로 쓸 수 없습니다. 다른 작업공간 이름을 쓰세요."));
        }
        let target = match self.form.kind {
            FormKind::Browser => {
                let Some(browser) = self.browsers.get(self.form.browser_idx).cloned() else {
                    return self.notify(ctx, "Chromium 계열 브라우저(Chrome/Edge/Brave/Whale 등)를 찾지 못했습니다.");
                };
                let source = match self.form.profile_idx.and_then(|i| browser.profiles.get(i)) {
                    Some(p) => ProfileSource::Clone {
                        profile_dir: p.dir.clone(),
                        browser: browser.clone(),
                        refresh: self.form.refresh,
                    },
                    None => ProfileSource::Empty { browser: browser.clone() },
                };
                let (w, h) = SIZES[self.form.size_idx];
                DeskTarget::Browser {
                    source,
                    headless: self.form.headless,
                    width: w,
                    height: h,
                    start_url: self.form.start_url.clone(),
                }
            }
            FormKind::Electron => {
                if !caps::ELECTRON {
                    return self.notify(ctx, text::unsupported("Electron 앱 숨김 실행"));
                }
                let apps = self.electron_apps.lock().clone().unwrap_or_default();
                let Some(app) = self.form.electron_idx.and_then(|i| apps.get(i)).cloned() else {
                    return self.notify(ctx, "Electron 앱을 선택하세요.");
                };
                if !app.hideable {
                    return self.notify(ctx, format!("{} 은(는) 지원하지 않습니다: {}", app.name, app.reason.clone().unwrap_or_default()));
                }
                let profile = if self.form.electron_clone {
                    let Some(src) = app.user_data_guess.clone() else {
                        return self.notify(ctx, "이 앱의 원본 데이터 폴더를 찾지 못했습니다. 빈 프로필을 쓰세요.");
                    };
                    if !self.form.electron_clone_consent {
                        return self.notify(ctx, "앱 데이터(로그인 정보·쿠키) 복제에 동의해야 합니다.");
                    }
                    crate::electron::ElectronProfile::Clone { source: src, refresh: self.form.electron_refresh }
                } else {
                    crate::electron::ElectronProfile::Empty
                };
                let (w, h) = SIZES[self.form.size_idx];
                DeskTarget::Electron { app, profile, width: w, height: h }
            }
            FormKind::Native => {
                if !caps::NATIVE_APPS {
                    return self.notify(ctx, text::unsupported(text::LOCAL_APPS));
                }
                let app = self.form.native_app.trim().to_string();
                if app.is_empty() {
                    return self.notify(
                        ctx,
                        pick("앱(.app 경로 또는 번들 id)을 입력하거나 고르세요.", "앱(실행 파일 .exe 경로 또는 이름)을 입력하거나 고르세요."),
                    );
                }
                #[cfg(target_os = "macos")]
                if let Err(e) = crate::macos::preflight() {
                    return self.notify(ctx, format!("{e:#}"));
                }
                // 가상 디스플레이는 이 OS 에서 지원할 때만 (Windows 에서는 체크박스도 보이지 않는다)
                if self.form.native_vdisplay && caps::VDISPLAY {
                    if !self.form.native_vdisplay_consent {
                        return self.notify(ctx, "가상 디스플레이 사용에 대한 안내를 확인하고 동의 체크를 켜세요.");
                    }
                    let (w, h) = SIZES[self.form.size_idx];
                    DeskTarget::VDisplay(crate::vdisplay::VirtualDisplayOptions {
                        app,
                        width: w,
                        height: h,
                        attach_consent: self.form.native_attach,
                        ..Default::default()
                    })
                } else {
                    DeskTarget::Native(crate::native::NativeOptions {
                        app,
                        new_instance: self.form.native_new_instance,
                        attach_consent: self.form.native_attach,
                        host: if cfg!(windows) && self.form.native_current_session {
                            crate::native::NativeHost::CurrentSession
                        } else {
                            crate::native::NativeHost::Auto
                        },
                        ..Default::default()
                    })
                }
            }
            FormKind::Session => {
                if !caps::SESSION {
                    return self.notify(ctx, text::unsupported("별도 계정 세션"));
                }
                // Windows 자식 세션은 같은 계정이라 계정 이름이 필요 없다
                if cfg!(target_os = "macos") && self.form.session_account.trim().is_empty() {
                    return self.notify(ctx, "전용 에이전트 계정 이름을 입력하세요.");
                }
                DeskTarget::Session { account: self.form.session_account.trim().to_string() }
            }
            FormKind::Remote => {
                if self.form.remote_host.trim().is_empty() {
                    return self.notify(ctx, "원격 데스크톱 주소를 입력하세요.");
                }
                DeskTarget::Remote(RemoteTarget {
                    host: self.form.remote_host.trim().to_string(),
                    port: self.form.remote_port,
                    username: Some(self.form.remote_user.trim().to_string()),
                    password: Some(self.form.remote_pass.clone()),
                    dedicated_ack: self.form.remote_dedicated,
                })
            }
        };
        let cfg = AgentConfig {
            name: self.form.name.clone(),
            workspace: ws_name.clone(),
            task: self.form.task.clone(),
            target,
            max_steps: self.form.max_steps,
            llm: Llm::new(
                self.api_key.trim().to_string(),
                MODELS[self.model_idx].to_string(),
                EFFORTS[self.effort_idx].to_string(),
            ),
        };
        let id = self.next_id;
        self.next_id += 1;
        let shared = AgentShared::new(id, cfg.name.clone(), ws_name, ctx.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        let task = self.rt.spawn(agent::run_agent(shared.clone(), cfg, rx));
        let input_tx = spawn_input_queue(&self.rt, shared.clone());
        self.agents.push(AgentHandle {
            shared,
            tx,
            task,
            input_tx,
            texture: None,
            tex_seq: 0,
            cursor_view: Default::default(),
            tile_cursor_view: Default::default(),
            input: String::new(),
            url_input: String::new(),
            takeover: false,
            preedit: String::new(),
        });
        self.form.name = format!("에이전트 {}", self.next_id);
        self.form.workspace = format!("ws-{}", self.next_id);
        self.form.task.clear();
    }

    fn remove_agent(&mut self, idx: usize) {
        let h = self.agents.remove(idx);
        // 아직 준비 중인 화면(내부·외부 모두)은 준비가 끝나는 즉시 닫히도록 표시한다
        h.shared.removed.store(true, Ordering::SeqCst);
        h.task.abort();
        let ws = h.shared.workspace.lock().take();
        if let Some(ws) = ws {
            self.rt.spawn(async move { ws.shutdown().await });
        }
        self.selected = match self.selected {
            Some(s) if s == idx => None,
            Some(s) if s > idx => Some(s - 1),
            s => s,
        };
        if self.view == View::Detail && self.selected.is_none() {
            self.view = View::Grid;
        }
    }

    fn update_textures(&mut self, ctx: &egui::Context) {
        for a in &mut self.agents {
            let seq = a.shared.frame_seq.load(Ordering::Relaxed);
            if seq != a.tex_seq {
                if let Some(img) = a.shared.frame.lock().take() {
                    match &mut a.texture {
                        Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                        None => {
                            a.texture = Some(ctx.load_texture(
                                format!("agent-{}", a.shared.id),
                                img,
                                egui::TextureOptions::LINEAR,
                            ))
                        }
                    }
                }
                a.tex_seq = seq;
            }
        }
    }

    // ───────────────────────── 상단 바 ─────────────────────────

    /// 왼쪽 사이드바: 이름 · 새 에이전트 · 보기 · 에이전트 목록, 아래쪽에 외부 연결 · 설정.
    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(RichText::new(icon::GHOST).size(18.0).color(theme::ACCENT_TEXT));
            ui.label(RichText::new("GhostDesk").size(16.0).strong().color(theme::TEXT));
        });
        ui.add_space(16.0);
        if theme::nav_item(ui, false, icon::PLUS, "새 에이전트", None)
            .on_hover_text(format!("에이전트가 일할 화면(웹사이트·{}·원격 컴퓨터)을 고르고 할 일을 맡깁니다", text::LOCAL_APPS))
            .clicked()
        {
            self.open_wizard();
        }
        if theme::nav_item(ui, self.view == View::Home, icon::GHOST, "홈", None).clicked() {
            self.view = View::Home;
        }
        if theme::nav_item(ui, self.view == View::Grid, icon::LAYOUT_GRID, "모든 에이전트", None).clicked() {
            self.view = View::Grid;
        }
        if !self.agents.is_empty()
            && theme::nav_item(ui, false, icon::LAYOUT_DASHBOARD, "화면 모아보기", None)
                .on_hover_text(pick(
                    "모든 에이전트의 화면만 한곳에 크게 모아 봅니다 (⌃⌘F 전체 화면, Esc 닫기)",
                    "모든 에이전트의 화면만 한곳에 크게 모아 봅니다 (F11 전체 화면, Esc 닫기)",
                ))
                .clicked()
        {
            self.enter_wall(ui.ctx());
        }
        // 아래쪽 고정 줄을 먼저 놓고, 남은 높이에 에이전트 목록
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            if theme::nav_item(ui, false, icon::SETTINGS, "설정", None)
                .on_hover_text("Claude API 키 · 모델 · 가상 마우스")
                .clicked()
            {
                self.show_settings = true;
            }
            // 외부 연결: 연결돼 있으면 초록 점, 연결 서버 오류면 빨간 점
            if theme::nav_item(ui, false, icon::PLUG, "외부 연결", self.connect_badge())
                .on_hover_text("외부 에이전트 연결 — Claude 앱 · Claude Code · Codex 에서 GhostDesk 쓰기 (API 키 불필요)")
                .clicked()
            {
                self.open_connect(ui.ctx());
            }
            ui.add_space(4.0);
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                if !self.agents.is_empty() {
                    ui.add_space(20.0);
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.agent_list(ui));
                }
            });
        });
    }

    /// '외부 연결' 단추의 상태 점: 연결 서버 오류면 빨강, 클라이언트가 하나라도 연결돼 있으면 초록, 아니면 없음.
    /// (연결 상태는 백그라운드에서 미리 계산해 둔다: 이 함수는 매 프레임 불린다)
    fn connect_badge(&self) -> Option<Color32> {
        if self.control_error.lock().is_some() {
            return Some(theme::DANGER);
        }
        self.connect_state.lock().connected_any().then_some(theme::SUCCESS)
    }

    fn open_wizard(&mut self) {
        self.wizard_open = true;
        self.form.choice = None;
        self.key_warn = false;
        self.wizard_key_field = self.api_key.trim().is_empty();
    }

    /// 외부 연결 창을 연다. 다른 곳(터미널의 --connect 등)에서 바뀐 연결 상태도 보이게 새로 읽는다.
    fn open_connect(&mut self, ctx: &egui::Context) {
        self.show_connect = true;
        if !self.connect_state.lock().busy {
            refresh_connect_status(&self.rt, self.connect_state.clone(), ctx.clone());
        }
    }

    /// 화면 모아보기를 연다
    fn enter_wall(&mut self, ctx: &egui::Context) {
        self.wall_fs_before = ctx.input(|i| i.viewport().fullscreen).unwrap_or(false);
        self.view = View::Wall;
    }

    /// ⚙ 설정: API 키, 모델, 가상 마우스
    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut close = false;
        let (ti, to) = self.agents.iter().fold((0, 0), |acc, a| {
            let t = *a.shared.tokens.lock();
            (acc.0 + t.0, acc.1 + t.1)
        });
        let m = egui::Modal::new(egui::Id::unique("settings")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(440.0);
            close |= theme::modal_header(ui, "설정");
            theme::section(ui, "Claude API");
            const FIELD: f32 = 340.0;
            egui::Grid::new("settings").num_columns(2).min_col_width(72.0).spacing([14.0, 10.0]).show(ui, |ui| {
                ui.label("API 키");
                ui.add(
                    egui::TextEdit::singleline(&mut self.api_key)
                        .password(true)
                        .desired_width(FIELD)
                        .margin(Margin::symmetric(10, 6))
                        .hint_text("sk-ant-…"),
                );
                ui.end_row();
                ui.label("모델");
                egui::ComboBox::from_id_salt("model").width(FIELD).selected_text(MODELS[self.model_idx]).show_ui(ui, |ui| {
                    for (i, m) in MODELS.iter().enumerate() {
                        ui.selectable_value(&mut self.model_idx, i, *m);
                    }
                });
                ui.end_row();
                ui.label("생각 깊이");
                egui::ComboBox::from_id_salt("effort").width(FIELD).selected_text(effort_label(EFFORTS[self.effort_idx])).show_ui(ui, |ui| {
                    for (i, e) in EFFORTS.iter().enumerate() {
                        ui.selectable_value(&mut self.effort_idx, i, effort_label(e));
                    }
                });
                ui.end_row();
            });
            ui.label(theme::hint("키는 저장하지 않습니다 (ANTHROPIC_API_KEY 환경변수가 있으면 자동으로 채움). 외부 에이전트 연결에는 필요 없습니다."));
            ui.add_space(10.0);
            theme::section(ui, "화면");
            ui.add(theme::switch(&mut self.show_cursor, "가상 마우스 표시"))
                .on_hover_text("에이전트 화면 위에 에이전트의 마우스 위치·궤적·클릭·입력을 그립니다 (직접 조작은 주황색)");
            if ti + to > 0 {
                ui.label(theme::hint(format!("이번 실행의 토큰: 입력 {ti} · 출력 {to}")));
            }
        });
        if close || m.should_close() {
            self.show_settings = false;
        }
    }

    /// Windows 자식 세션 기능 상태 + 켜기(관리자 승인) 버튼
    #[cfg(windows)]
    fn child_session_status(&mut self, ui: &mut egui::Ui) {
        let req = self.child_enable.lock().clone();
        if matches!(req, ChildEnable::Done(_)) && self.child_enabled != Some(true) {
            self.child_enabled = None; // 요청이 끝났으면 다시 확인
        }
        let on = *self.child_enabled.get_or_insert_with(crate::winapp::child::enabled);
        if on {
            ui.label(RichText::new(format!("{}  자식 세션 기능이 켜져 있습니다.", icon::CIRCLE_CHECK)).color(theme::BODY));
            return;
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("자식 세션 기능이 꺼져 있습니다.").color(theme::DANGER));
            let waiting = req == ChildEnable::Waiting;
            if ui.add_enabled(!waiting, egui::Button::new(format!("{}  자식 세션 켜기 (관리자 승인)", icon::LOCK_OPEN))).clicked() {
                *self.child_enable.lock() = ChildEnable::Waiting;
                let st = self.child_enable.clone();
                let ctx = ui.ctx().clone();
                std::thread::spawn(move || {
                    let r = crate::winapp::child::request_enable().map_err(|e| format!("{e:#}"));
                    *st.lock() = ChildEnable::Done(r);
                    ctx.request_repaint();
                });
            }
            if waiting {
                ui.spinner();
            }
        });
        if let ChildEnable::Done(Err(e)) = &req {
            ui.label(RichText::new(e).color(theme::DANGER));
        }
    }

    /// 별도 계정 세션(RDP 방식) 처음 설정 안내 창
    fn session_guide_window(&mut self, ctx: &egui::Context) {
        if !self.show_session_guide {
            return;
        }
        let exe = "/Users/Shared/GhostDesk/bin/ghostdesk";
        #[cfg(unix)]
        let uid = unsafe { libc::getuid() };
        #[cfg(not(unix))]
        let uid = 0u32;
        let max_h = (ctx.content_rect().height() - 160.0).max(240.0);
        let mut close = false;
        let m = egui::Modal::new(egui::Id::unique("session_guide")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(620.0);
            close |= theme::modal_header(ui, pick("별도 계정 세션 설정", "자식 세션(PiP) 설정"));
            theme::modal_scroll(ui, max_h, |ui| {
                ui.label(crate::session::SETUP_GUIDE_KO);
                #[cfg(unix)]
                ui.add_space(6.0);
                #[cfg(unix)]
                ui.horizontal(|ui| {
                    ui.strong("LaunchAgent plist (에이전트 계정의 ~/Library/LaunchAgents 에 저장)");
                });
                #[cfg(unix)]
                {
                    let plist = crate::session::launch_agent_plist(exe, uid);
                    if ui.small_button(format!("{}  복사", icon::COPY)).clicked() {
                        ui.ctx().copy_text(plist.clone());
                    }
                    ui.add(egui::TextEdit::multiline(&mut plist.as_str()).code_editor().desired_width(f32::INFINITY));
                }
                #[cfg(not(unix))]
                let _ = (exe, uid);
            });
        });
        if close || m.should_close() {
            self.show_session_guide = false;
        }
    }

    /// 외부 에이전트 연결 (Claude 앱 · Claude Code · Codex)
    fn connect_window(&mut self, ctx: &egui::Context) {
        if !self.show_connect {
            return;
        }
        let err = self.control_error.lock().clone();
        let st = self.connect_state.lock().clone();
        let max_h = (ctx.content_rect().height() - 220.0).max(220.0);
        let mut close = false;
        let mut auto_err = None;
        let m = egui::Modal::new(egui::Id::unique("connect")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(560.0);
            close |= theme::modal_header(ui, "외부 에이전트 연결");
            theme::modal_scroll(ui, max_h, |ui| {
                ui.label(RichText::new("연결하면 그 에이전트에게 \"GhostDesk 로 …해줘\" 라고 말하기만 하면 됩니다. API 키는 필요 없습니다.").color(theme::BODY));
                if let Some(e) = &err {
                    theme::tinted(theme::DANGER).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(format!("연결 서버 오류: {e}")).color(theme::DANGER));
                    });
                }
                ui.add_space(4.0);
                for (i, c) in st.clients.iter().enumerate() {
                    let ok = st.ok.get(i).copied().unwrap_or(false);
                    theme::card().inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(c.client.name()).strong().color(theme::TEXT));
                            let (txt, col) = match (&c.registered, c.installed) {
                                (Some(_), _) if ok => ("연결됨", theme::SUCCESS),
                                (Some(_), _) => ("다른 경로로 연결됨", theme::WARN),
                                (None, true) => ("연결 안 됨", theme::MUTED),
                                (None, false) => ("설치 안 됨", Color32::from_gray(95)),
                            };
                            theme::chip(ui, col, txt);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if c.registered.is_some() && ui.add_enabled_ui(!st.busy, |ui| theme::secondary(ui, "해제")).inner.clicked() {
                                    run_client_action(&self.rt, self.connect_state.clone(), ui.ctx().clone(), Some(c.client), false);
                                }
                                if c.installed && !ok && ui.add_enabled_ui(!st.busy, |ui| theme::secondary(ui, "연결")).inner.clicked() {
                                    run_client_action(&self.rt, self.connect_state.clone(), ui.ctx().clone(), Some(c.client), true);
                                }
                            });
                        });
                    });
                }
                if st.clients.is_empty() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(theme::hint("연결 상태를 확인하는 중…"));
                    });
                }
                if st.busy {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("연결하는 중…");
                    });
                }
                for m in &st.messages {
                    ui.label(theme::hint(m.clone()));
                }
                ui.add_space(2.0);
                egui::CollapsingHeader::new("직접 설정하기 (고급)").default_open(false).show(ui, |ui| {
                    ui.label(theme::hint(format!("설치 위치: {} (연결은 이 경로를 씁니다)", st.install_dir)));
                    let exe = crate::install::stable_exe().display().to_string();
                    for (title, snippet) in crate::mcp::setup_snippets(&exe) {
                        egui::Frame::new().fill(theme::BG).corner_radius(theme::CARD_RADIUS).inner_margin(Margin::same(10)).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(title).strong().color(theme::TEXT));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.small_button(format!("{}  복사", icon::COPY)).clicked() {
                                        ui.ctx().copy_text(snippet.clone());
                                    }
                                });
                            });
                            ui.add(egui::TextEdit::multiline(&mut snippet.as_str()).code_editor().desired_rows(1).desired_width(f32::INFINITY));
                        });
                    }
                    ui.label(theme::hint("로그인 복제 · 실행 중인 앱 사용 · 원격 접속은 GhostDesk 창에서 허용해야 진행됩니다. GhostDesk 창이 꺼져 있으면 자동으로 켜집니다."));
                });
            });
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                let mut auto = st.auto;
                if ui.add(theme::switch(&mut auto, "켤 때 자동으로 연결")).changed() {
                    match crate::install::set_auto_connect(auto) {
                        Ok(()) => self.connect_state.lock().auto = auto,
                        Err(e) => auto_err = Some(format!("자동 연결 설정을 저장하지 못했습니다: {e:#}")),
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled_ui(!st.busy, |ui| theme::primary(ui, "  모두 연결  ")).inner.clicked() {
                        spawn_connect(&self.rt, self.connect_state.clone(), ui.ctx().clone());
                    }
                });
            });
        });
        if close || m.should_close() {
            self.show_connect = false;
        }
        if let Some(e) = auto_err {
            self.notify(ctx, e);
        }
    }

    // ───────────────────────── 새 에이전트 ─────────────────────────

    /// ＋ 새 에이전트: 무엇을 조작할지 고르고, 할 일을 적는다. 나머지는 고급 설정.
    fn new_agent_window(&mut self, ctx: &egui::Context) {
        if !self.wizard_open {
            return;
        }
        const W: f32 = 600.0;
        let max_h = (ctx.content_rect().height() - 200.0).max(260.0);
        let mut close = false;
        let mut start = false;
        let mut go_connect = false;
        // 이 OS 에서 지원하지 않는 화면 종류는 보이지 않는다 (platform::caps)
        let mut kinds: Vec<(Choice, &str, &str, &str)> = vec![(Choice::Web, icon::GLOBE, "웹사이트", "숨겨진 브라우저")];
        if LOCAL_APP_CARD {
            kinds.push((Choice::App, text::APP_ICON, text::LOCAL_APPS, "보이지 않게 실행"));
        }
        if caps::SESSION {
            kinds.push((
                Choice::Session,
                icon::USER,
                pick("별도 계정 세션", "자식 세션"),
                pick("RDP 처럼 통째로", "같은 PC 의 별도 화면"),
            ));
        }
        kinds.push((Choice::Remote, icon::MONITOR, "원격 컴퓨터", "VNC 로 접속"));
        let m = egui::Modal::new(egui::Id::unique("new_agent")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(W);
            close |= theme::modal_header(ui, "새 에이전트");
            theme::modal_scroll(ui, max_h, |ui| {
                ui.set_width(W);
                theme::section(ui, "무엇을 조작할까요");
                let gap = 10.0;
                let n = kinds.len() as f32;
                let tw = ((W - gap * (n - 1.0)) / n).floor();
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for (c, icon, title, sub) in &kinds {
                        if theme::tile(ui, self.form.choice == Some(*c), icon, title, sub, Vec2::new(tw, 96.0)).clicked() {
                            self.form.choice = Some(*c);
                        }
                    }
                });
                let Some(choice) = self.form.choice else {
                    return;
                };
                ui.add_space(14.0);
                match choice {
                    Choice::Web => self.simple_web(ui),
                    Choice::App => self.simple_app(ui),
                    Choice::Remote => self.form_remote(ui),
                    Choice::Session => {
                        if cfg!(target_os = "macos") {
                            ui.horizontal(|ui| {
                                ui.label("전용 계정");
                                ui.text_edit_singleline(&mut self.form.session_account);
                            });
                            ui.label(theme::hint("이 Mac 의 별도 사용자 계정 화면 전체를 에이전트가 씁니다. 내 화면과 완전히 분리되지만, 그 계정은 내 로그인을 공유하지 않습니다. 처음 한 번 설정이 필요합니다."));
                        } else {
                            ui.label(theme::hint("Windows 자식 세션: 지금 계정 그대로 별도의 Windows 화면을 하나 더 만들어 에이전트가 씁니다. 내 마우스·키보드와 분리되고, 진행 화면은 이 창에서 봅니다."));
                            #[cfg(windows)]
                            self.child_session_status(ui);
                        }
                        if ui.button(format!("{}  처음 설정 방법 보기", icon::BOOK_OPEN)).clicked() {
                            self.show_session_guide = true;
                        }
                    }
                }
                ui.add_space(12.0);
                theme::section(ui, "할 일");
                ui.add(
                    egui::TextEdit::multiline(&mut self.form.task)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(10, 8))
                        .hint_text("예) 네이버에서 오늘 날씨를 찾아 요약해줘  (비워 두면 나중에 지시)"),
                );
                ui.add_space(4.0);
                egui::CollapsingHeader::new("고급 설정").default_open(false).show(ui, |ui| {
                    egui::Grid::new("adv").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                        ui.label("이름");
                        ui.text_edit_singleline(&mut self.form.name);
                        ui.end_row();
                        ui.label("작업공간");
                        ui.text_edit_singleline(&mut self.form.workspace)
                            .on_hover_text("에이전트마다 고유해야 합니다. 같은 이름이면 이전 복제본(로그인 포함)을 재사용합니다.");
                        ui.end_row();
                        ui.label("최대 단계");
                        ui.horizontal(|ui| {
                            let mut unlimited = self.form.max_steps == 0;
                            if ui.add(theme::switch(&mut unlimited, "무제한 (계속 실행)")).changed() {
                                self.form.max_steps = if unlimited { 0 } else { 60 };
                            }
                            if !unlimited {
                                ui.add(egui::DragValue::new(&mut self.form.max_steps).range(1..=1000));
                            }
                        });
                        ui.end_row();
                    });
                    match choice {
                        Choice::Web => self.form_browser(ui),
                        Choice::App => {
                            ui.label(theme::hint(pick(
                                "앱을 직접 지정하려면 번들 id 나 .app 경로를 입력하세요.",
                                "앱을 직접 지정하려면 실행 파일(.exe) 경로나 프로그램 이름을 입력하세요.",
                            )));
                            ui.add(egui::TextEdit::singleline(&mut self.form.native_app).desired_width(f32::INFINITY));
                        }
                        Choice::Remote | Choice::Session => {}
                    }
                });
                if self.wizard_key_field {
                    ui.add_space(12.0);
                    theme::section(ui, "Claude API 키");
                    ui.horizontal(|ui| {
                        let link_w = 96.0;
                        let warn = self.key_warn && self.api_key.trim().is_empty();
                        if warn {
                            ui.visuals_mut().widgets.inactive.bg_stroke = Stroke::new(1.0, theme::WARN);
                        }
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.api_key)
                                .id(egui::Id::unique("wizard_api_key"))
                                .password(true)
                                .hint_text("sk-ant-…")
                                .margin(Margin::symmetric(10, 6))
                                .desired_width((ui.available_width() - link_w).max(120.0)),
                        );
                        if std::mem::take(&mut self.focus_key) {
                            r.request_focus();
                        }
                        if r.changed() {
                            self.key_warn = false;
                        }
                        if ui
                            .link("키 없이 쓰기")
                            .on_hover_text("Claude 앱 · Claude Code · Codex 에 연결해 쓰면 API 키가 필요 없습니다")
                            .clicked()
                        {
                            go_connect = true;
                        }
                    });
                    if self.key_warn && self.api_key.trim().is_empty() {
                        ui.label(RichText::new("시작하려면 API 키를 넣으세요").size(12.0).color(theme::WARN));
                    }
                }
            });
            ui.add_space(16.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ready = self.form.choice.is_some();
                if ui.add_enabled_ui(ready, |ui| theme::primary(ui, "   시작   ")).inner.clicked() {
                    start = true;
                }
                if theme::secondary(ui, "취소").clicked() {
                    close = true;
                }
            });
        });
        // 바깥 클릭으로는 닫지 않는다 (적던 내용을 실수로 잃지 않게). Esc 와 ✕·취소로 닫는다.
        let esc = m.is_top_modal && !m.any_popup_open && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        if start {
            self.start_from_wizard(ctx);
        }
        if close || esc {
            self.wizard_open = false;
        }
        if go_connect {
            self.wizard_open = false;
            self.open_connect(ctx);
        }
    }

    fn simple_web(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("simple_web").num_columns(2).min_col_width(72.0).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("시작 주소");
            ui.add(egui::TextEdit::singleline(&mut self.form.start_url).desired_width(f32::INFINITY).margin(Margin::symmetric(10, 6)));
            ui.end_row();
        });
        let bname = self.browsers.get(self.form.browser_idx).map(|b| b.name.clone()).unwrap_or_default();
        let profiles = self.browsers.get(self.form.browser_idx).map(|b| b.profiles.clone()).unwrap_or_default();
        // 복제할 프로필이 없으면 체크박스는 꺼진 채 비활성 (비활성 체크박스에 true 가 남지 않게)
        if profiles.is_empty() {
            self.form.web_use_login = false;
        }
        ui.horizontal(|ui| {
            let r = ui
                .add_enabled(!profiles.is_empty(), theme::switch(&mut self.form.web_use_login, format!("내 {bname} 로그인 사용")))
                .on_hover_text("내 브라우저의 쿠키·로그인을 복사한(프로필을 복제한) 격리 브라우저를 씁니다. 원본 브라우저에는 아무 영향이 없습니다. 복제할 프로필은 고급 설정에서 고를 수 있습니다.");
            if r.changed() && self.form.web_use_login && self.form.profile_idx.is_none_or(|i| i >= profiles.len()) {
                self.form.profile_idx = Some(0);
            }
            // 프로필이 여럿이면 어느 것을 복제하는지 이름만 (계정 주소는 보이지 않게)
            if self.form.web_use_login && profiles.len() > 1 {
                if let Some(p) = self.form.profile_idx.and_then(|i| profiles.get(i)) {
                    ui.label(theme::hint(profile_short(&p.name)));
                }
            }
        });
        if self.browsers.is_empty() {
            ui.colored_label(theme::DANGER, "Chrome/Edge/Brave/Whale 같은 Chromium 계열 브라우저를 찾지 못했습니다.");
        }
    }

    fn simple_app(&mut self, ui: &mut egui::Ui) {
        let electrons = self.electron_apps.lock().clone().unwrap_or_default();
        if cfg!(windows) {
            ui.add(theme::switch(&mut self.form.native_current_session, "현재 계정 로그인 사용 · 숨은 화면"));
            if self.form.native_current_session {
                self.form.native_attach = false;
                self.form.app_use_login = false;
                self.form.native_app = current_session_app_path(&electrons, &self.form.native_app);
                theme::tinted(theme::WARN).show(ui, |ui| {
                    ui.label("앱의 원본 로그인과 데이터를 그대로 사용합니다. 작업 결과는 실제 계정에 반영됩니다. 같은 앱은 트레이까지 종료한 뒤 시작하세요. 지원하지 않는 앱은 오류를 알리고 멈춥니다.");
                });
            }
        }
        ui.add(
            egui::TextEdit::singleline(&mut self.form.app_search)
                .hint_text(pick("앱 검색 (예: Code, 텍스트 편집기, 카카오톡)", "앱 검색 (예: Code, 카카오톡, Character Map)"))
                .desired_width(f32::INFINITY)
                .margin(Margin::symmetric(10, 6)),
        );
        let q = self.form.app_search.to_lowercase();
        let natives = self.native_apps.lock().clone();
        let detecting = self.electron_apps.lock().is_none() || natives.is_none();
        let items = app_list_items(&electrons, &natives.unwrap_or_default(), self.form.native_current_session);
        // Electron 감지가 늦게 끝났으면, 이미 고른 앱이 Electron 인지 다시 맞춘다 (네이티브 경로로 새지 않게)
        if self.form.electron_idx.is_none() {
            self.form.electron_idx = electrons.iter().position(|e| electron_matches(e, &self.form.native_app));
        }
        egui::Frame::new().fill(theme::BG).stroke(Stroke::new(1.0, theme::BORDER)).corner_radius(theme::CARD_RADIUS).inner_margin(Margin::same(6)).show(ui, |ui| {
        egui::ScrollArea::vertical().max_height(180.0).auto_shrink([false, true]).show(ui, |ui| {
            ui.set_width(ui.available_width());
            // 한 줄짜리 목록: 왼쪽 정렬, 행마다 폭 전체를 누를 수 있게
            ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for it in items.iter().filter(|it| q.is_empty() || it.label.to_lowercase().contains(&q) || it.path.to_lowercase().contains(&q)) {
                let sel = self.form.native_app == it.path;
                match &it.disabled {
                    // 숨김 실행이 안 되는 Electron 앱: 목록에는 보이되 고를 수 없고, 이유를 알려 준다
                    Some(reason) => {
                        ui.add_enabled(false, egui::Button::selectable(false, it.label.as_str()))
                            .on_disabled_hover_text(reason.as_str());
                    }
                    None => {
                        if ui.add(egui::Button::selectable(sel, it.label.as_str()).min_size(Vec2::new(0.0, 28.0))).clicked() {
                            self.form.native_app = it.path.clone();
                            self.form.electron_idx = it.electron;
                        }
                    }
                }
            }
            });
        });
        });
        if detecting {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(theme::hint(pick("Electron 앱을 확인하는 중…", "설치된 앱을 확인하는 중…")));
            });
        }
        if self.form.native_app.is_empty() && items.iter().any(|it| it.electron.is_some()) {
            ui.label(theme::hint(format!(
                "{} {}",
                icon::STAR,
                pick("완전히 숨겨 실행되는 앱 (나머지는 창을 숨긴 채 조작)", "완전히 숨겨 실행되는 앱 (나머지는 창을 화면 밖에 두고 조작)"),
            )))
            .on_hover_text(format!("{} 표시 앱(VS Code 등 Electron)은 별도 인스턴스를 띄워 완전히 숨겨 실행합니다.", icon::STAR));
        }
        if self.form.native_app.is_empty() {
            return;
        }
        if self.form.native_current_session {
            self.form.electron_idx = None;
            ui.label(theme::hint("현재 Windows 계정으로 숨은 데스크톱에 새로 실행합니다. 창을 화면 밖으로 옮기는 방식으로 자동 전환하지 않습니다. Store 앱·관리자 권한 앱·일부 GPU 앱은 지원하지 않습니다."));
            return;
        }
        let electron = self.form.electron_idx.and_then(|i| electrons.get(i)).filter(|e| electron_matches(e, &self.form.native_app));
        match electron {
            Some(e) if !e.hideable => {
                ui.colored_label(
                    theme::DANGER,
                    format!("{} 은(는) 숨김 실행을 지원하지 않습니다: {}", e.name, e.reason.clone().unwrap_or_default()),
                );
            }
            Some(e) => {
                // 원본 데이터 폴더가 없으면 체크박스는 꺼진 채 비활성 (비활성 체크박스에 true 가 남지 않게)
                if e.user_data_guess.is_none() {
                    self.form.app_use_login = false;
                }
                ui.add_enabled_ui(e.user_data_guess.is_some(), |ui| {
                    ui.add(theme::switch(&mut self.form.app_use_login, format!("내 {} 로그인 상태 사용 (앱 데이터를 복제해 씀)", e.name)));
                });
            }
            None => {
                self.form.electron_idx = None;
                ui.add(theme::switch(&mut self.form.native_attach, "실행 중인 내 앱을 사용 (작업하는 동안 그 앱 창이 숨겨짐)"))
                    .on_hover_text("카카오톡처럼 한 번에 하나만 로그인되는 앱용입니다. 끄면 별도의 숨긴 인스턴스를 새로 띄웁니다.");
                // 가상 디스플레이(CGVirtualDisplay)는 macOS 전용: 다른 OS 에서는 체크박스를 보이지 않고 꺼 둔다
                if caps::VDISPLAY {
                    ui.add(theme::switch(&mut self.form.native_vdisplay, format!("{}  가상 디스플레이에서 실행 (실험 — 드래그·우클릭 가능)", icon::FLASK)))
                        .on_hover_text("앱 창을 숨기는 대신 보이지 않는 가상 모니터 위에 띄워 실제 마우스 동작을 그대로 씁니다.");
                } else {
                    self.form.native_vdisplay = false;
                }
                if self.form.native_vdisplay {
                    theme::tinted(theme::WARN).show(ui, |ui| {
                        ui.label(RichText::new("가상 모니터가 디스플레이 배치의 가장 바깥에 잠시 추가됩니다. 그동안 마우스 커서가 그쪽으로 넘어갈 수 있고, 설정에 따라 Dock·메뉴 막대가 그 화면에도 보일 수 있습니다. 작업이 끝나면 가상 모니터는 제거됩니다.").small());
                        ui.add(theme::switch(&mut self.form.native_vdisplay_consent, "이해했고 가상 모니터 추가에 동의합니다"));
                    });
                } else {
                    self.form.native_vdisplay_consent = false;
                }
                // macOS 권한(손쉬운 사용·화면 기록) 안내는 macOS 에서만. Windows 는 이런 권한 동의가 없다.
                #[cfg(target_os = "macos")]
                {
                    let t = crate::macos::tcc_status();
                    if !(t.accessibility && t.screen_capture) {
                        ui.colored_label(theme::WARN, "이 방식은 시스템 설정 > 개인정보 보호 및 보안 에서 '손쉬운 사용'과 '화면 및 시스템 오디오 기록' 권한이 필요합니다.");
                    }
                }
                ui.label(theme::hint(pick(
                    "클릭은 버튼 누르기·입력칸 선택 같은 동작으로 바뀝니다(드래그·우클릭 불가). 캡처 중 메뉴 막대에 화면 기록 표시가 나타납니다.",
                    "앱 창을 모든 모니터 바깥에 두고 화면을 캡처해 조작합니다. 내 마우스·키보드·화면은 그대로입니다. 일부 앱(게임·하드웨어 가속 화면 등)은 화면이 비어 보이거나 입력이 듣지 않을 수 있습니다.",
                )));
            }
        }
    }

    fn start_from_wizard(&mut self, ctx: &egui::Context) {
        match self.form.choice {
            Some(Choice::Web) => {
                self.form.kind = FormKind::Browser;
                let n = self.browsers.get(self.form.browser_idx).map(|b| b.profiles.len()).unwrap_or(0);
                // 고급 설정에서 고른 프로필을 존중한다 (범위를 벗어났을 때만 첫 프로필)
                self.form.profile_idx = match (self.form.web_use_login && n > 0, self.form.profile_idx) {
                    (false, _) => None,
                    (true, Some(i)) if i < n => Some(i),
                    (true, _) => Some(0),
                };
            }
            Some(Choice::App) => {
                if self.form.native_app.trim().is_empty() {
                    return self.notify(ctx, "조작할 앱을 고르세요.");
                }
                if cfg!(windows) && self.form.native_current_session {
                    let electrons = self.electron_apps.lock().clone().unwrap_or_default();
                    self.form.native_app = current_session_app_path(&electrons, self.form.native_app.trim());
                    self.form.kind = FormKind::Native;
                    self.form.native_new_instance = true;
                    self.form.native_attach = false;
                    self.form.electron_idx = None;
                } else {
                    // Electron 앱은 감지 결과로만 판단한다: 감지 전이거나 숨김 실행이 안 되는 앱을
                    // 조용히 네이티브(창 숨김) 방식으로 돌리지 않는다.
                    let Some(electrons) = self.electron_apps.lock().clone() else {
                        return self.notify(ctx, "앱 목록을 확인하는 중입니다. 잠시 후 다시 시작하세요.");
                    };
                    let found = electrons.iter().position(|e| electron_matches(e, self.form.native_app.trim()));
                    if let Some(i) = found {
                        let e = &electrons[i];
                        if !e.hideable {
                            return self.notify(ctx, format!("{} 은(는) 숨김 실행을 지원하지 않습니다: {}", e.name, e.reason.clone().unwrap_or_default()));
                        }
                        self.form.electron_idx = Some(i);
                    } else {
                        self.form.electron_idx = None;
                    }
                    if let Some(e) = self.form.electron_idx.and_then(|i| electrons.get(i)) {
                        self.form.kind = FormKind::Electron;
                        let clone = self.form.app_use_login && e.user_data_guess.is_some();
                        self.form.electron_clone = clone;
                        // 체크박스 문구가 '앱 데이터를 복제'임을 명시하므로 체크 자체를 동의로 본다
                        self.form.electron_clone_consent = clone;
                    } else {
                        self.form.kind = FormKind::Native;
                        self.form.native_new_instance = !self.form.native_attach;
                    }
                }
            }
            Some(Choice::Remote) => self.form.kind = FormKind::Remote,
            Some(Choice::Session) => self.form.kind = FormKind::Session,
            None => return self.notify(ctx, "무엇을 조작할지 먼저 고르세요."),
        }
        let before = self.agents.len();
        self.spawn_agent(ctx);
        if self.agents.len() > before {
            self.wizard_open = false;
            self.form.web_use_login = false;
            self.form.app_use_login = false;
            self.form.native_current_session = false;
            self.selected = Some(self.agents.len() - 1);
        }
    }

    fn form_remote(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("form_remote").num_columns(2).min_col_width(72.0).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("주소");
            ui.add(
                egui::TextEdit::singleline(&mut self.form.remote_host)
                    .hint_text("예) 192.168.0.20 또는 vm.local")
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(10, 6)),
            );
            ui.end_row();
            ui.label("포트");
            ui.add(egui::DragValue::new(&mut self.form.remote_port).range(1..=65535));
            ui.end_row();
            ui.label("사용자");
            ui.add(
                egui::TextEdit::singleline(&mut self.form.remote_user)
                    .hint_text("macOS 화면 공유는 계정 이름")
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(10, 6)),
            );
            ui.end_row();
            ui.label("비밀번호");
            ui.add(
                egui::TextEdit::singleline(&mut self.form.remote_pass)
                    .password(true)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(10, 6)),
            );
            ui.end_row();
        });
        ui.add(theme::switch(&mut self.form.remote_dedicated, "에이전트 전용 머신입니다")).on_hover_text(pick(
            "macOS 화면 공유에 접속할 때 필요합니다. \
             GhostDesk 는 Apple 의 세션 선택 단계를 구현하지 않아, 대상 Mac 의 콘솔 사용자와 다른 계정으로 접속하면 그 Mac 의 화면이 전환될 수 있습니다. 이 Mac 자신에는 항상 접속을 거부합니다.",
            "macOS 화면 공유나 이 PC 의 VNC 서버에 접속할 때 필요합니다. \
             GhostDesk 는 Apple 의 세션 선택 단계를 구현하지 않아, 대상 Mac 의 콘솔 사용자와 다른 계정으로 접속하면 그 Mac 의 화면이 전환될 수 있습니다. \
             또 이 PC 자신에서 실행 중인 VNC 서버는 지금 보고 계신 실제 화면일 수 있어, 확인 없이는 접속하지 않습니다. \
             WSL·Hyper-V 가상 머신처럼 이 PC 주소로 전달되는 별도 데스크톱일 때만 켜세요.",
        ));
    }

    fn form_browser(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("form").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("브라우저");
            let bname = self.browsers.get(self.form.browser_idx).map(|b| b.name.clone()).unwrap_or("(없음)".into());
            egui::ComboBox::from_id_salt("browser").selected_text(bname).show_ui(ui, |ui| {
                for (i, b) in self.browsers.iter().enumerate() {
                    if ui.selectable_label(self.form.browser_idx == i, &b.name).clicked() {
                        self.form.browser_idx = i;
                        self.form.profile_idx = if b.profiles.is_empty() { None } else { Some(0) };
                    }
                }
            });
            ui.end_row();

            ui.label("프로필");
            let profiles = self.browsers.get(self.form.browser_idx).map(|b| b.profiles.clone()).unwrap_or_default();
            // 위의 '로그인 상태 사용' 체크박스와 같은 설정을 보여 준다: 프로필을 고르면 체크가 켜지고,
            // '빈 프로필' 을 고르면 꺼진다 (어느 쪽도 다른 쪽에 덮이지 않음)
            let picked = if self.form.web_use_login { self.form.profile_idx.and_then(|i| profiles.get(i)) } else { None };
            let ptext = match picked {
                Some(p) => format!("복제: {}", p.name),
                None => "빈 프로필 (로그인 없음)".into(),
            };
            egui::ComboBox::from_id_salt("profile").selected_text(ptext).width(200.0).show_ui(ui, |ui| {
                for (i, p) in profiles.iter().enumerate() {
                    let sel = self.form.web_use_login && self.form.profile_idx == Some(i);
                    if ui.selectable_label(sel, format!("복제: {}", p.name)).clicked() {
                        self.form.profile_idx = Some(i);
                        self.form.web_use_login = true;
                    }
                }
                if ui.selectable_label(!self.form.web_use_login, "빈 프로필 (로그인 없음)").clicked() {
                    self.form.web_use_login = false;
                }
            });
            ui.end_row();

            ui.label("");
            ui.add(theme::switch(&mut self.form.refresh, "원본에서 다시 복제 (로그인 최신화)"));
            ui.end_row();

            ui.label("시작 URL");
            ui.text_edit_singleline(&mut self.form.start_url);
            ui.end_row();

            ui.label("화면 크기");
            let (w, h) = SIZES[self.form.size_idx];
            egui::ComboBox::from_id_salt("size").selected_text(format!("{w}x{h}")).show_ui(ui, |ui| {
                for (i, (w, h)) in SIZES.iter().enumerate() {
                    ui.selectable_value(&mut self.form.size_idx, i, format!("{w}x{h}"));
                }
            });
            ui.end_row();
        });
        ui.add(theme::switch(&mut self.form.headless, "headless (화면에 아무 창도 띄우지 않음)"))
            .on_hover_text("끄면 디버그용으로 실제 브라우저 창이 보입니다. 이 경우에도 입력은 CDP 로만 전달되어 마우스/키보드를 뺏지 않습니다.");
        if self.browsers.is_empty() {
            ui.colored_label(theme::DANGER, "Chromium 계열 브라우저를 찾지 못했습니다.");
        }
    }

    // ───────────────────────── 왼쪽: 에이전트 목록 (상세 보기에서 다른 에이전트로 옮겨 갈 때) ─────────────────────────

    fn agent_list(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            theme::section(ui, "에이전트");
            egui::Frame::new().fill(theme::CARD).corner_radius(255).inner_margin(Margin::symmetric(7, 1)).show(ui, |ui| {
                ui.label(RichText::new(self.agents.len().to_string()).size(11.0).color(theme::MUTED));
            });
        });
        ui.add_space(2.0);
        let mut clicked = None;
        for (i, a) in self.agents.iter().enumerate() {
            let st = a.shared.status();
            let sel = self.view == View::Detail && self.selected == Some(i);
            let fill = if sel { theme::CARD_HOVER } else { Color32::TRANSPARENT };
            let external = a.shared.external.lock().is_some();
            let q = a.shared.queue.lock().len();
            let r = egui::Frame::new()
                .fill(fill)
                .corner_radius(theme::CARD_RADIUS)
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.horizontal(|ui| {
                        theme::dot(ui, crate::cursor::agent_color(a.shared.id), 4.5);
                        // 오른쪽 끝(상태 점·외부 표시)을 먼저 놓고, 이름은 남은 폭에서 줄인다
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            match (&st, attention(&st, a.takeover)) {
                                (AgentStatus::WaitingUser(_), _) => {
                                    theme::chip(ui, theme::WARN, "질문").on_hover_text(status_text(&st));
                                }
                                (_, Some(c)) => {
                                    theme::dot(ui, c, 3.5).on_hover_text(if a.takeover { "직접 조작 중".to_string() } else { status_text(&st) });
                                }
                                _ => {}
                            }
                            if external {
                                ui.label(theme::hint("외부"));
                            }
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.add(egui::Label::new(RichText::new(&a.shared.name).color(theme::TEXT)).truncate().selectable(false));
                            });
                        });
                    });
                    if q > 0 {
                        ui.horizontal(|ui| {
                            ui.add_space(14.0);
                            ui.label(theme::hint(format!("대기 중인 작업 {q}개")));
                        });
                    }
                })
                .response
                .interact(egui::Sense::click());
            r.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, sel, format!("{} 열기", a.shared.name)));
            if r.hovered() && !sel {
                ui.painter().rect_stroke(r.rect, theme::CARD_RADIUS, Stroke::new(1.0, theme::BORDER), StrokeKind::Inside);
            }
            if r.clicked() {
                clicked = Some(i);
            }
        }
        if let Some(i) = clicked {
            self.selected = Some(i);
            self.view = View::Detail;
        }
    }

    // ───────────────────────── 처음 화면 ─────────────────────────

    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            let w = ui.available_width().min(680.0);
            ui.add_space((ui.available_height() * 0.28).max(24.0));
            ui.label(RichText::new("오늘은 어떤 일을 대신 맡아 드릴까요?").size(28.0).color(theme::TEXT));
            ui.add_space(28.0);
            // 맡길 일 입력 상자: Enter(또는 ↵ 버튼)로 새 에이전트 창을 열고, 적은 일을 그대로 넘긴다
            let id = egui::Id::unique("welcome_composer");
            let focused = ui.memory(|m| m.has_focus(id));
            let mut submit = false;
            ui.allocate_ui(Vec2::new(w, 0.0), |ui| {
                theme::composer_frame(focused).show(ui, |ui| {
                    ui.set_width(w - 32.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), |ui| {
                            submit |= theme::enter_button(ui, "시작하기 (Enter)").clicked();
                            let edit = egui::TextEdit::multiline(&mut self.form.task)
                                .id(id)
                                .hint_text("에이전트에게 맡길 일을 적어 보세요")
                                .frame(egui::Frame::NONE)
                                .desired_rows(2)
                                .desired_width(ui.available_width())
                                .return_key(Some(egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter)));
                            let r = ui.add(edit);
                            if r.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift) {
                                submit = true;
                            }
                        });
                    });
                });
            });
            if submit {
                self.open_wizard();
            }
        });
    }

    // ───────────────────────── 그리드 ─────────────────────────

    fn grid(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("모든 에이전트").size(20.0).strong().color(theme::TEXT));
            ui.label(RichText::new(self.agents.len().to_string()).size(14.0).color(theme::MUTED));
        });
        ui.add_space(16.0);
        if self.agents.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.3).max(24.0));
                ui.label(RichText::new("아직 에이전트가 없습니다").size(20.0).color(theme::TEXT));
                ui.add_space(8.0);
                ui.label(theme::hint("홈에서 할 일을 맡기거나 새 에이전트를 추가하세요."));
                ui.add_space(20.0);
                if theme::primary(ui, "새 에이전트").clicked() {
                    self.open_wizard();
                }
            });
            return;
        }
        self.screens(ui, false);
    }

    // ───────────────────────── 화면 모아보기 ─────────────────────────

    /// 모아보기를 닫고 다른 보기로 (전체 화면이었으면 창 모드로 되돌린다)
    fn leave_wall(&mut self, ctx: &egui::Context, to: View) {
        // 모아보기에서 켠 전체 화면만 끈다 (열기 전부터 전체 화면이었으면 그대로)
        if self.wall_fullscreen && !self.wall_fs_before {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
        self.wall_fullscreen = false;
        self.view = to;
    }

    /// 모든 에이전트의 화면만 창 가득 배치한다. 화면을 누르면 그 에이전트의 상세 보기로.
    fn screens(&mut self, ui: &mut egui::Ui, immersive: bool) {
        let ctx = ui.ctx().clone();
        // 창 관리자가 전체 화면을 바꿨을 수도 있으니 실제 상태를 따른다
        if let Some(fs) = ctx.input(|i| i.viewport().fullscreen).filter(|_| immersive) {
            self.wall_fullscreen = fs;
        }
        let mut exit = false;
        // 전체 화면: F11 (macOS 는 ⌃⌘F)
        let mut toggle_fs = immersive && ctx.input(|i| {
            i.key_pressed(egui::Key::F11) || (cfg!(target_os = "macos") && i.modifiers.mac_cmd && i.modifiers.ctrl && i.key_pressed(egui::Key::F))
        });

        // 화면만: 창 전체를 화면 배치에 쓰고, 단추는 오른쪽 위에 겹쳐 둔다
        let area = ui.available_rect_before_wrap();
        ui.allocate_rect(area, egui::Sense::hover());
        let n = self.agents.len();
        let aspect_of = |a: &AgentHandle| {
            a.shared.workspace().map(|w| {
                let (sw, sh) = w.size();
                sh as f32 / sw.max(1) as f32
            })
        };
        // 칸 비율은 가장 세로로 긴 화면에 맞춘다 (모든 화면이 칸 안에 비율 그대로 들어가게)
        let aspect = self.agents.iter().filter_map(aspect_of).fold(0.0f32, f32::max);
        let aspect = if aspect > 0.0 { aspect } else { 0.625 };
        let gap = 10.0;
        let (cols, cell) = wall_layout(n, area.size(), aspect, gap);
        let rows = n.div_ceil(cols.max(1));
        let total_h = rows as f32 * cell.y + gap * (rows as f32 - 1.0);
        let top = area.center().y - total_h / 2.0;
        let show_cursor = self.show_cursor;
        let mut open = None;
        let mut repaint = false;
        for (i, a) in self.agents.iter_mut().enumerate() {
            let (r, c) = (i / cols, i % cols);
            // 마지막 줄은 가운데 정렬
            let in_row = if r + 1 == rows { n - r * cols } else { cols };
            let row_w = in_row as f32 * cell.x + gap * (in_row as f32 - 1.0);
            let left = area.center().x - row_w / 2.0;
            let cell_rect = egui::Rect::from_min_size(egui::pos2(left + c as f32 * (cell.x + gap), top + r as f32 * (cell.y + gap)), cell);
            let ws = a.shared.workspace();
            let asp = ws
                .as_ref()
                .map(|w| {
                    let (sw, sh) = w.size();
                    sh as f32 / sw.max(1) as f32
                })
                .unwrap_or(aspect);
            let fit = if asp >= cell.y / cell.x { Vec2::new(cell.y / asp, cell.y) } else { Vec2::new(cell.x, cell.x * asp) };
            let rect = egui::Rect::from_center_size(cell_rect.center(), fit);
            let resp = match &a.texture {
                Some(t) => ui.put(rect, egui::Image::new((t.id(), rect.size())).corner_radius(theme::CARD_RADIUS).sense(egui::Sense::click())),
                None => {
                    let r = ui.allocate_rect(rect, egui::Sense::click());
                    ui.painter().rect_filled(rect, theme::CARD_RADIUS, theme::CARD);
                    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "준비 중…", egui::FontId::proportional(14.0), theme::MUTED);
                    r
                }
            };
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("{} 크게 보기", a.shared.name)));
            let st = a.shared.status();
            let p = ui.painter();
            if ws.is_none() && a.texture.is_some() && !matches!(st, AgentStatus::Preparing(_)) {
                p.rect_filled(rect, theme::CARD_RADIUS, Color32::from_black_alpha(160));
                p.text(rect.center(), egui::Align2::CENTER_CENTER, "닫힘", egui::FontId::proportional(18.0), Color32::from_gray(200));
            }
            if let (true, Some(w), Some(_)) = (show_cursor, ws.as_ref(), a.texture.as_ref()) {
                let (sw, _) = w.size();
                let scale = rect.width() / sw.max(1) as f32;
                let mut cur = a.shared.cursor.lock();
                repaint |= crate::cursor::paint(
                    p, rect, scale, &mut cur, &mut a.tile_cursor_view,
                    w.mouse_pos(), crate::cursor::agent_color(a.shared.id), None, true,
                );
            }
            // 왼쪽 위 이름표: ● 이름 · 상태 (사람이 볼 일이 있으면 그 색으로 테두리와 상태 글)
            let attn = attention(&st, a.takeover);
            let mut job = egui::text::LayoutJob::default();
            let fmt = |color: Color32| egui::TextFormat { font_id: egui::FontId::proportional(12.0), color, ..Default::default() };
            job.append(&a.shared.name, 0.0, fmt(Color32::WHITE));
            let status = status_text(&st);
            let status = if is_error(&st) { humanize_error(&status) } else { status };
            job.append(&format!("  ·  {}", crate::llm::short(&status, 28)), 0.0, fmt(attn.unwrap_or(Color32::from_gray(205))));
            let g = p.layout_job(job);
            let at = rect.left_top() + Vec2::new(10.0, 9.0);
            let pill = egui::Rect::from_min_size(at, Vec2::new(g.size().x + 16.0, g.size().y)).expand2(Vec2::new(8.0, 4.0));
            p.rect_filled(pill, 255, Color32::from_black_alpha(170));
            p.circle_filled(egui::pos2(at.x + 4.0, at.y + g.size().y / 2.0), 4.0, crate::cursor::agent_color(a.shared.id));
            p.galley(at + Vec2::new(14.0, 0.0), g, Color32::WHITE);
            if let Some(c) = attn {
                p.rect_stroke(rect, theme::CARD_RADIUS, Stroke::new(3.0, c), StrokeKind::Outside);
            } else if resp.hovered() {
                p.rect_stroke(rect, theme::CARD_RADIUS, Stroke::new(1.5, theme::BORDER_HOVER), StrokeKind::Outside);
            }
            if resp.clicked() {
                open = Some(i);
            }
        }
        // 오른쪽 위 단추 (전체 화면에서는 마우스를 위쪽 가장자리로 가져갈 때만)
        let near_top = ctx.input(|i| i.pointer.hover_pos().is_some_and(|p| p.y < area.top() + 90.0));
        if immersive && (!self.wall_fullscreen || near_top) {
            egui::Area::new(egui::Id::unique("wall_bar"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::RIGHT_TOP, [-18.0, 18.0])
                .show(&ctx, |ui| {
                    egui::Frame::new().fill(Color32::from_black_alpha(190)).corner_radius(theme::CARD_RADIUS).inner_margin(Margin::same(6)).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.add_space(4.0);
                            ui.label(theme::hint(format!("화면 {n}개")));
                            if theme::icon_toggle(ui, self.show_cursor, icon::MOUSE_POINTER, "가상 마우스 표시").clicked() {
                                self.show_cursor = !self.show_cursor;
                            }
                            if theme::toggle(ui, self.wall_fullscreen, "전체 화면", theme::ACCENT).on_hover_text(pick("⌃⌘F", "F11")).clicked() {
                                toggle_fs = true;
                            }
                            if theme::secondary(ui, "닫기").on_hover_text("Esc").clicked() {
                                exit = true;
                            }
                        });
                    });
                });
        }
        if toggle_fs {
            self.wall_fullscreen = !self.wall_fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.wall_fullscreen));
        }
        if repaint {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
        if let Some(i) = open {
            self.selected = Some(i);
            if immersive {
                self.leave_wall(&ctx, View::Detail);
            } else {
                self.view = View::Detail;
            }
        } else if exit {
            self.leave_wall(&ctx, View::Grid);
        }
    }

    // ───────────────────────── 상세 ─────────────────────────

    /// 상세 보기 오른쪽 패널: 이름·상태 → 제어 → (질문/외부 안내) → 기록 → 아래쪽 지시 입력
    fn detail_side(&mut self, ui: &mut egui::Ui, idx: usize) -> bool {
        let a = &mut self.agents[idx];
        let st = a.shared.status();
        let mut remove = false;
        // 1줄: ● 이름 ……… ⋯ (제거는 메뉴 안에: 실수로 누르지 않게)
        ui.horizontal(|ui| {
            theme::dot(ui, crate::cursor::agent_color(a.shared.id), 5.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button(RichText::new(icon::ELLIPSIS).size(18.0).color(theme::BODY), |ui| {
                    if ui
                        .button(RichText::new("에이전트 제거").color(theme::DANGER))
                        .on_hover_text("격리 화면을 종료합니다 (작업공간 폴더는 남음)")
                        .clicked()
                    {
                        remove = true;
                        ui.close();
                    }
                })
                .response
                .on_hover_text("더 보기");
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(&a.shared.name).size(18.0).strong().color(theme::TEXT)).truncate());
                });
            });
        });
        let t = *a.shared.tokens.lock();
        theme::chip(ui, status_color(&st), &status_text(&st)).on_hover_text(format!(
            "단계 {} · 토큰 {} / {} · 작업공간 {}",
            a.shared.steps.load(Ordering::Relaxed),
            t.0,
            t.1,
            a.shared.workspace_name
        ));
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let paused = a.shared.paused.load(Ordering::Relaxed);
            if theme::secondary(ui, if paused { "재개" } else { "일시정지" }).clicked() {
                a.shared.paused.store(!paused, Ordering::Relaxed);
            }
            if theme::secondary(ui, "중지").on_hover_text("현재 동작 후 멈추고 다음 지시를 기다립니다 (대화 맥락 유지)").clicked() {
                a.shared.stop.store(true, Ordering::SeqCst);
                a.shared.paused.store(false, Ordering::Relaxed);
            }
            let before = a.takeover;
            if theme::toggle(ui, a.takeover, "직접 조작", theme::TAKEOVER)
                .on_hover_text("켜면 에이전트가 멈추고, 왼쪽 화면을 클릭·스크롤·타이핑해 직접 조작할 수 있습니다 (로그인·2단계 인증 등)")
                .clicked()
            {
                a.takeover = !a.takeover;
            }
            if a.takeover != before {
                a.shared.paused.store(a.takeover, Ordering::Relaxed);
            }
        });

        if let AgentStatus::WaitingUser(q) = &st {
            ui.add_space(2.0);
            theme::tinted(theme::WARN).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("에이전트의 질문").strong().color(theme::WARN));
                // 긴 질문도 아래 답변 칸을 밀어내지 않게 높이를 제한한다
                egui::ScrollArea::vertical().id_salt("question").max_height(160.0).show(ui, |ui| {
                    ui.label(q);
                });
            });
        }
        let external = a.shared.external.lock().clone();
        if let Some(client) = &external {
            ui.add_space(2.0);
            theme::tinted(theme::ACCENT).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(format!("{client} 이(가) 조작하는 화면")).strong().color(theme::ACCENT_TEXT));
                ui.label(theme::hint("지시는 그 에이전트(Claude 앱·Claude Code·Codex 등)에게 하세요. 여기서는 지켜보거나 '직접 조작'으로 도울 수 있습니다."));
            });
        }
        ui.add_space(6.0);

        // 가운데: 기록, 아래: 지시 입력 (GhostDesk 안의 에이전트만).
        // 입력칸 높이는 지난 프레임에 잰 값으로 기록 영역을 남겨 두고, 높이가 바뀌면 한 번 더 그려 맞춘다.
        // 자리가 모자라면 기록을 줄인다 (입력칸과 보내기 단추는 늘 보이게).
        let has_composer = external.is_none();
        let composer_id = egui::Id::unique(("composer_h", a.shared.id));
        let composer_h: f32 = ui.data(|d| d.get_temp(composer_id)).unwrap_or(150.0);
        let log_h = (ui.available_height() - if has_composer { composer_h + 10.0 } else { 0.0 }).max(0.0);
        if log_h >= 60.0 {
            egui::Frame::new().fill(theme::BG).corner_radius(theme::CARD_RADIUS).inner_margin(Margin::same(10)).show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("log")
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .max_height(log_h - 20.0)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 7.0;
                        for l in a.shared.log.lock().iter() {
                            log_row(ui, l);
                        }
                    });
            });
        }

        if has_composer {
            ui.add_space(10.0);
            let waiting = matches!(st, AgentStatus::WaitingUser(_));
            let composer = ui.scope(|ui| {
                let mut remove_q = None;
                {
                    let q = a.shared.queue.lock();
                    if !q.is_empty() {
                        egui::ScrollArea::vertical().id_salt("queue").max_height(84.0).show(ui, |ui| {
                            for (i, t) in q.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    ui.label(theme::hint(format!("다음 {}. {}", i + 1, crate::llm::short(t, 40))));
                                    if ui.small_button(icon::X).on_hover_text("대기열에서 빼기").clicked() {
                                        remove_q = Some(i);
                                    }
                                });
                            }
                        });
                    }
                }
                if let Some(i) = remove_q {
                    a.shared.queue.lock().remove(i);
                }
                let hint = if waiting {
                    pick("답변 입력 (Cmd+Enter 전송)", "답변 입력 (Ctrl+Enter 전송)")
                } else {
                    pick("지시 입력 (Cmd+Enter 전송)", "지시 입력 (Ctrl+Enter 전송)")
                };
                let te = ui.add(
                    egui::TextEdit::multiline(&mut a.input)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(10, 8))
                        .hint_text(hint),
                );
                let send_key = te.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.command);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if (theme::primary(ui, if waiting { "답변 보내기" } else { "보내기" }).on_hover_text("지금 바로 지시합니다").clicked() || send_key)
                        && !a.input.trim().is_empty()
                    {
                        let text = a.input.trim().to_string();
                        let cmd = if waiting { AgentCmd::Answer(text) } else { AgentCmd::Instruction(text) };
                        let _ = a.tx.send(cmd);
                        a.input.clear();
                    }
                    if !waiting
                        && theme::secondary(ui, "다음 작업으로")
                            .on_hover_text("지금 작업이 정상적으로 끝나면 멈추지 않고 이어서 실행합니다. 중지·최대 단계·오류로 멈춘 상태에서는 자동으로 시작하지 않으니, '보내기'로 지시해 재개하면 그 일이 끝난 뒤 대기열이 이어집니다.")
                            .clicked()
                        && !a.input.trim().is_empty()
                    {
                        a.shared.queue.lock().push_back(a.input.trim().to_string());
                        a.input.clear();
                        let _ = a.tx.send(AgentCmd::Wake);
                    }
                });
            });
            let h = composer.response.rect.height();
            if (h - composer_h).abs() > 0.5 {
                ui.data_mut(|d| d.insert_temp(composer_id, h));
                ui.ctx().request_repaint();
            }
        }
        remove
    }

    fn detail_view(&mut self, ui: &mut egui::Ui, idx: usize) {
        let rt = self.rt.clone();
        let ws_opt = self.agents[idx].shared.workspace();
        let browser = ws_opt.as_ref().and_then(|w| w.as_browser().cloned());
        let (views, active) = ws_opt.as_ref().map(|w| w.views()).unwrap_or_default();
        // 1줄: ‹ 모두 보기 · (브라우저: ◀ ▶ ⟳ + 주소 / 앱: 지금 창 제목) · 오른쪽 끝 가상 마우스
        ui.horizontal(|ui| {
            if ui
                .add(egui::Button::new(RichText::new("‹  모두 보기").color(theme::TEXT)).frame_when_inactive(false).min_size(Vec2::new(0.0, 32.0)))
                .clicked()
            {
                self.view = View::Grid;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::icon_toggle(ui, self.show_cursor, icon::MOUSE_POINTER, "가상 마우스 표시").clicked() {
                    self.show_cursor = !self.show_cursor;
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if let Some(b) = &browser {
                        let w = b.clone();
                        if theme::icon_button(ui, icon::ARROW_LEFT, "뒤로").clicked() {
                            rt.spawn(async move { let _ = w.history_go(-1).await; });
                        }
                        let w = b.clone();
                        if theme::icon_button(ui, icon::ARROW_RIGHT, "앞으로").clicked() {
                            rt.spawn(async move { let _ = w.history_go(1).await; });
                        }
                        let w = b.clone();
                        if theme::icon_button(ui, icon::ROTATE_CW, "새로 고침").clicked() {
                            rt.spawn(async move { let _ = w.reload().await; });
                        }
                        let w = b.clone();
                        if theme::icon_button(ui, "＋", "새 탭").clicked() {
                            rt.spawn(async move { let _ = w.new_tab("").await; });
                        }
                        let a = &mut self.agents[idx];
                        let cur = b.active_tab().map(|t| t.url).unwrap_or_default();
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut a.url_input)
                                .desired_width(f32::INFINITY)
                                .margin(Margin::symmetric(10, 6))
                                .hint_text(cur),
                        );
                        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !a.url_input.trim().is_empty() {
                            let w = b.clone();
                            let u = std::mem::take(&mut a.url_input);
                            rt.spawn(async move { let _ = w.navigate(&u).await; });
                        }
                    } else if let Some(t) = views.iter().find(|t| Some(&t.id) == active.as_ref()) {
                        let title = if t.title.is_empty() { t.detail.clone() } else { t.title.clone() };
                        ui.add(egui::Label::new(theme::hint(title)).truncate());
                    }
                });
            });
        });
        // 2줄: 탭/창이 둘 이상일 때만 (넘치면 다음 줄로)
        if views.len() > 1 {
            ui.horizontal_wrapped(|ui| {
                for (i, t) in views.iter().enumerate() {
                    let title = if t.title.is_empty() { t.detail.clone() } else { t.title.clone() };
                    let sel = Some(&t.id) == active.as_ref();
                    if ui.add(egui::Button::selectable(sel, crate::llm::short(&title, 24))).on_hover_text(&t.detail).clicked() && !sel {
                        if let Some(ws) = &ws_opt {
                            let w = ws.clone();
                            rt.spawn(async move { let _ = w.switch_view(i).await; });
                        }
                    }
                }
            });
        }
        let a = &mut self.agents[idx];
        let Some(ws) = ws_opt else {
            ui.centered_and_justified(|ui| ui.label(RichText::new(status_text(&a.shared.status())).color(theme::MUTED)));
            return;
        };
        ui.add_space(6.0);

        // 라이브 화면 (가운데 정렬, 비율 유지)
        let avail = ui.available_size();
        let (sw, sh) = ws.size();
        let scale = (avail.x / sw.max(1) as f32).min(avail.y / sh.max(1) as f32).max(0.05);
        let size = Vec2::new(sw as f32 * scale, sh as f32 * scale);
        let sense = if a.takeover { egui::Sense::click_and_drag() } else { egui::Sense::hover() };
        let pad = ((avail.x - size.x) / 2.0).max(0.0);
        let resp = ui
            .horizontal(|ui| {
                ui.add_space(pad);
                match &a.texture {
                    Some(t) => ui.add(egui::Image::new((t.id(), size)).corner_radius(theme::CARD_RADIUS).sense(sense)),
                    None => {
                        let r = ui.allocate_response(size, sense);
                        ui.painter().rect_filled(r.rect, 10, theme::CARD);
                        ui.painter().text(r.rect.center(), egui::Align2::CENTER_CENTER, "화면 준비 중…", egui::FontId::proportional(14.0), theme::MUTED);
                        r
                    }
                }
            })
            .inner;
        let rect = resp.rect;
        if !a.takeover {
            // 검은 페이지도 바탕과 구분되게 얇은 테두리
            ui.painter().rect_stroke(rect, 10, Stroke::new(1.0, theme::BORDER), StrokeKind::Outside);
        }
        // 가상 마우스: 격리 화면에는 실제 커서가 없으므로 에이전트 동작을 기록해 그린다.
        if self.show_cursor && a.texture.is_some() {
            let mut st = a.shared.cursor.lock();
            let again = crate::cursor::paint(
                ui.painter(), rect, scale, &mut st, &mut a.cursor_view, ws.mouse_pos(),
                crate::cursor::agent_color(a.shared.id), Some(&a.shared.name), false,
            );
            if again {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(16));
            }
        }
        let shared = a.shared.clone();
        let rec = move |name: &str, input: serde_json::Value, pos: (f64, f64)| {
            shared.cursor.lock().record(name, &input, pos, true);
        };
        if a.takeover {
            // 직접 조작 중: 주황 테두리 + 왼쪽 위 표시
            let p = ui.painter();
            p.rect_stroke(rect, 10, egui::Stroke::new(2.0, theme::TAKEOVER), egui::StrokeKind::Outside);
            let at = rect.left_top() + Vec2::new(12.0, 10.0);
            let g = p.layout_no_wrap("직접 조작 중".to_string(), egui::FontId::proportional(12.0), Color32::BLACK);
            p.rect_filled(egui::Rect::from_min_size(at, g.size()).expand2(Vec2::new(8.0, 3.0)), 255, theme::TAKEOVER);
            p.galley(at, g, Color32::BLACK);
        }

        if !a.takeover {
            return;
        }
        let to_page = |p: egui::Pos2| (((p.x - rect.min.x) / scale) as f64, ((p.y - rect.min.y) / scale) as f64);
        let hovered = resp.hovered();
        if hovered || resp.has_focus() {
            resp.request_focus();
        }
        // 모든 입력은 에이전트별 큐 하나로 보내 순서대로 적용한다
        let input = a.input_tx.clone();
        let send = move |ev: TakeoverInput| {
            let _ = input.send(ev);
        };
        if resp.drag_started() {
            if let Some(p) = resp.interact_pointer_pos() {
                let (x, y) = to_page(p);
                rec("left_mouse_down", serde_json::json!({ "coordinate": [x, y] }), (x, y));
                send(TakeoverInput::DragStart(x, y));
            }
        } else if resp.dragged() {
            if let Some(p) = resp.interact_pointer_pos() {
                let (x, y) = to_page(p);
                rec("mouse_move", serde_json::json!({ "coordinate": [x, y] }), (x, y));
                send(TakeoverInput::MouseMove(x, y));
            }
        }
        if resp.drag_stopped() {
            send(TakeoverInput::MouseUp);
        } else if resp.clicked() || resp.secondary_clicked() || resp.double_clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                let (x, y) = to_page(p);
                let (btn, count) = if resp.secondary_clicked() {
                    (MouseButton::Right, 1)
                } else if resp.double_clicked() {
                    (MouseButton::Left, 2)
                } else {
                    (MouseButton::Left, 1)
                };
                let name = match (btn, count) {
                    (MouseButton::Right, _) => "right_click",
                    (_, 2) => "double_click",
                    _ => "left_click",
                };
                rec(name, serde_json::json!({ "coordinate": [x, y] }), (x, y));
                send(TakeoverInput::Click { pos: (x, y), button: btn, count });
            }
        }
        if !hovered {
            // 화면 밖으로 나가면 입력기가 꺼지므로 조합 중이던 글자 표시도 지운다
            a.preedit.clear();
            return;
        }
        // Windows: 직접 조작 중에는 입력기(IME)를 켜 둔다. egui 는 글 입력칸에 포커스가 없으면 IME 를 꺼 버려
        // (ImmAssociateContextEx) 한글 자판이 영문 글자로 들어온다. 조합이 확정되면(Commit) Type 으로 보낸다.
        if cfg!(windows) {
            let at = resp.hover_pos().unwrap_or(rect.left_top());
            ui.ctx().output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    purpose: egui::IMEPurpose::Normal,
                    rect,
                    cursor_rect: egui::Rect::from_min_size(at, Vec2::new(1.0, 18.0)),
                    should_interrupt_composition: false,
                });
            });
        }
        let events = ui.input(|i| i.events.clone());
        for ev in events {
            match ev {
                // 붙여넣기: 사람이 직접 한 조작이므로 사용자의 클립보드 글자를 그대로 입력한다
                egui::Event::Text(t) | egui::Event::Paste(t) => {
                    rec("type", serde_json::json!({ "text": t }), ws.mouse_pos());
                    send(TakeoverInput::Type(t));
                }
                // egui 는 cmd+C / cmd+X (Windows: ctrl+C·ctrl+Insert / ctrl+X·shift+Delete) 를 Key 대신
                // Copy / Cut 으로 바꾸므로 격리 화면에 그 OS 의 키 조합으로 전달한다. Windows 의 Insert/Delete 는
                // 누른 조합 그대로 (그 화면 안에서 복사·잘라내기가 일어난다. Electron 의 클립보드 차단은 에이전트에게만 적용)
                egui::Event::Copy | egui::Event::Cut => {
                    let combo = crate::platform::input::takeover_clipboard_combo(matches!(ev, egui::Event::Copy), ui.input(|i| i.modifiers));
                    rec("key", serde_json::json!({ "text": combo }), ws.mouse_pos());
                    send(TakeoverInput::Key(combo));
                }
                // 조합 중인 글자는 화면에만 보여 준다 (Windows 에서만 IME 를 켜므로 그때만 온다)
                egui::Event::Ime(egui::ImeEvent::Preedit { text: t, .. }) if cfg!(windows) => {
                    a.preedit = t;
                }
                egui::Event::Ime(egui::ImeEvent::Commit(t)) => {
                    a.preedit.clear();
                    send(TakeoverInput::Type(t));
                }
                egui::Event::MouseWheel { delta, unit, .. } => {
                    let mul = match unit {
                        egui::MouseWheelUnit::Point => 1.0,
                        egui::MouseWheelUnit::Line => 40.0,
                        egui::MouseWheelUnit::Page => 600.0,
                    };
                    let (dx, dy) = (-(delta.x * mul) as f64, -(delta.y * mul) as f64);
                    let pos = resp.hover_pos().map(to_page);
                    if let Some(p) = pos {
                        let dir = if dy.abs() >= dx.abs() { if dy < 0.0 { "up" } else { "down" } } else if dx < 0.0 { "left" } else { "right" };
                        rec("scroll", serde_json::json!({ "coordinate": [p.0, p.1], "scroll_direction": dir }), p);
                    }
                    send(TakeoverInput::Scroll { pos, dx, dy });
                }
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    if let Some(combo) = egui_key_combo(key, modifiers) {
                        rec("key", serde_json::json!({ "text": combo }), ws.mouse_pos());
                        send(TakeoverInput::Key(combo));
                    }
                }
                _ => {}
            }
        }
        if !a.preedit.is_empty() {
            let painter = ui.painter();
            let at = resp.hover_pos().unwrap_or(rect.left_bottom()) + Vec2::new(14.0, 14.0);
            let galley = painter.layout_no_wrap(a.preedit.clone(), egui::FontId::proportional(16.0), Color32::WHITE);
            let bg = egui::Rect::from_min_size(at, galley.size()).expand(4.0);
            painter.rect_filled(bg, 4.0, theme::RAISED);
            painter.rect_stroke(bg, 4.0, egui::Stroke::new(1.0, theme::TAKEOVER), egui::StrokeKind::Outside);
            painter.galley(at, galley, Color32::WHITE);
        }
    }
}

/// 오류로 멈춘 상태인가 (Claude API 오류 뒤에는 '오류 — …' 로 대기 상태가 된다)
fn is_error(st: &AgentStatus) -> bool {
    match st {
        AgentStatus::Error(_) => true,
        AgentStatus::Idle(s) => s.starts_with("오류"),
        _ => false,
    }
}

/// 사람이 볼 일이 있으면 그 표시 색: 직접 조작 중 = 주황, 질문 = 노랑, 오류 = 빨강
fn attention(st: &AgentStatus, takeover: bool) -> Option<Color32> {
    if takeover {
        Some(theme::TAKEOVER)
    } else if matches!(st, AgentStatus::WaitingUser(_)) {
        Some(theme::WARN)
    } else if is_error(st) {
        Some(theme::DANGER)
    } else {
        None
    }
}

/// 상태 표시 색 (테마 색): 일하는 중 = 초록, 사람 응답 대기 = 노랑, 오류 = 빨강, 쉬는 중 = 회색
fn status_color(st: &AgentStatus) -> Color32 {
    match st {
        _ if is_error(st) => theme::DANGER,
        AgentStatus::Preparing(_) => theme::MUTED,
        AgentStatus::Thinking | AgentStatus::Acting(_) => theme::SUCCESS,
        AgentStatus::WaitingUser(_) => theme::WARN,
        AgentStatus::Idle(_) => theme::MUTED,
        AgentStatus::Error(_) => theme::DANGER,
    }
}

/// 상태 표시 글 (화면용으로 짧게)
fn status_text(st: &AgentStatus) -> String {
    match st {
        AgentStatus::Thinking => "생각 중".into(),
        AgentStatus::Acting(s) => format!("실행 중: {s}"),
        AgentStatus::Idle(s) if s.starts_with("오류") => "오류 — 지시하면 다시 시도".into(),
        s => s.label(),
    }
}

/// 오류 기록을 알아보기 쉬운 짧은 말로 (전체 글은 마우스를 올리면 보인다)
fn humanize_error(s: &str) -> String {
    let l = s.to_lowercase();
    if l.contains("401") || l.contains("invalid x-api-key") || l.contains("authentication") {
        "API 키를 확인하세요".into()
    } else if l.contains("429") || l.contains("rate limit") || l.contains("rate_limit") {
        "요청 한도 초과 — 잠시 후 다시".into()
    } else if l.contains("529") || l.contains("overloaded") {
        "Claude 가 혼잡함 — 잠시 후 다시".into()
    } else if l.contains("error sending request") || l.contains("연결 실패") || l.contains("dns") || l.contains("timed out") {
        "Claude API 에 연결할 수 없음".into()
    } else {
        crate::llm::short(s, 60)
    }
}

/// 생각 깊이 표시 (API 값은 그대로 괄호 안에)
fn effort_label(e: &str) -> String {
    let ko = match e {
        "low" => "낮음",
        "medium" => "보통",
        "high" => "높음",
        "xhigh" => "매우 높음",
        "max" => "최대",
        _ => return e.to_string(),
    };
    format!("{ko} ({e})")
}

/// 브라우저 프로필 표시 이름에서 계정 주소 부분 '(…@…)' 을 뗀다
fn profile_short(name: &str) -> String {
    match name.rfind(" (") {
        Some(i) if name.ends_with(')') && name[i..].contains('@') => name[..i].to_string(),
        _ => name.to_string(),
    }
}

/// 기록 한 줄: 종류 색 점 + 글 (줄바꿈하면 점 오른쪽으로 들여 씀). 시각은 마우스를 올리면.
fn log_row(ui: &mut egui::Ui, l: &crate::agent::LogEntry) {
    let (dot, color) = match l.kind {
        LogKind::User => (theme::ACCENT_TEXT, theme::TEXT),
        LogKind::Assistant => (theme::MUTED, theme::TEXT),
        LogKind::Thinking => (theme::MUTED, theme::MUTED),
        LogKind::Action => (theme::SUCCESS, theme::BODY),
        LogKind::Info => (Color32::from_gray(90), theme::MUTED),
        LogKind::Error => (theme::DANGER, theme::DANGER),
    };
    let row = ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let (r, _) = ui.allocate_exact_size(Vec2::new(8.0, 18.0), egui::Sense::hover());
        ui.painter().circle_filled(egui::pos2(r.center().x, r.top() + 9.0), 3.0, dot);
        let mut t = RichText::new(&l.text).color(color);
        match l.kind {
            LogKind::User => t = t.strong(),
            LogKind::Thinking => t = t.italics(),
            LogKind::Action | LogKind::Info => t = t.size(13.0),
            _ => {}
        }
        ui.add(egui::Label::new(t).wrap());
    });
    row.response.on_hover_text(format!("{:.1}초", l.secs));
}

/// 화면 모아보기 배치: n 개 화면을 area 안에 가장 크게 놓는 (열 수, 칸 크기). aspect = 높이/너비.
fn wall_layout(n: usize, area: Vec2, aspect: f32, gap: f32) -> (usize, Vec2) {
    let n = n.max(1);
    let mut best = (1, Vec2::ZERO);
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        let by_width = (area.x - gap * (cols as f32 - 1.0)) / cols as f32;
        let by_height = ((area.y - gap * (rows as f32 - 1.0)) / rows as f32) / aspect;
        let w = by_width.min(by_height).max(1.0);
        if w > best.1.x + 0.01 {
            best = (cols, Vec2::new(w, w * aspect));
        }
    }
    best
}

/// egui 키 이벤트 → xdotool 스타일 조합. 일반 글자는 Text 이벤트로 오므로 수정키가 있을 때만 처리.
fn egui_key_combo(key: egui::Key, m: egui::Modifiers) -> Option<String> {
    use egui::Key::*;
    let base = match key {
        Enter => "Return".to_string(),
        Tab => "Tab".into(),
        Backspace => "BackSpace".into(),
        Delete => "Delete".into(),
        Escape => "Escape".into(),
        ArrowUp => "Up".into(),
        ArrowDown => "Down".into(),
        ArrowLeft => "Left".into(),
        ArrowRight => "Right".into(),
        Home => "Home".into(),
        End => "End".into(),
        PageUp => "Page_Up".into(),
        PageDown => "Page_Down".into(),
        F1 => "F1".into(), F2 => "F2".into(), F3 => "F3".into(), F4 => "F4".into(),
        F5 => "F5".into(), F6 => "F6".into(), F7 => "F7".into(), F8 => "F8".into(),
        F9 => "F9".into(), F10 => "F10".into(), F11 => "F11".into(), F12 => "F12".into(),
        k => {
            let name = k.name();
            if name.chars().count() == 1 && (m.ctrl || m.mac_cmd || m.alt) {
                name.to_ascii_lowercase()
            } else {
                return None;
            }
        }
    };
    let mut s = String::new();
    if m.mac_cmd { s.push_str("cmd+"); }
    if m.ctrl { s.push_str("ctrl+"); }
    if m.alt { s.push_str("alt+"); }
    if m.shift { s.push_str("shift+"); }
    s.push_str(&base);
    Some(s)
}

impl eframe::App for GhostApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // 외부 에이전트 이벤트 처리
        while let Ok(ev) = self.control_rx.try_recv() {
            match ev {
                crate::control::ControlEvent::Register(shared) => {
                    let (tx, _rx) = mpsc::unbounded_channel();
                    let input_tx = spawn_input_queue(&self.rt, shared.clone());
                    self.agents.push(AgentHandle {
                        shared,
                        tx,
                        task: self.rt.spawn(async {}),
                        input_tx,
                        texture: None,
                        tex_seq: 0,
                        cursor_view: Default::default(),
                        tile_cursor_view: Default::default(),
                        input: String::new(),
                        url_input: String::new(),
                        takeover: false,
                        preedit: String::new(),
                    });
                }
                crate::control::ControlEvent::Approval { title, detail, reply } => {
                    self.approvals.push((title, detail, Some(reply)));
                    // 백그라운드로 뜬 창이라도 알 수 있게 Dock 아이콘(Windows: 작업 표시줄 단추 깜빡임)으로 알린다
                    // (포커스는 빼앗지 않음)
                    ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                        egui::UserAttentionType::Informational,
                    ));
                    self.attention_requested = true;
                }
            }
        }
        self.approval_window(&ctx);
        while self.pending_demo > 0 {
            self.pending_demo -= 1;
            // 데모는 사용자 데이터를 쓰지 않는다: 빈 프로필, 빈 페이지
            self.form.kind = FormKind::Browser;
            self.form.profile_idx = None;
            self.form.start_url = "about:blank".into();
            self.form.task = "데모 작업".into();
            self.spawn_agent(&ctx);
        }
        self.update_textures(&ctx);

        // 화면 모아보기: 다른 패널 없이 화면만 (에이전트가 모두 사라지면 처음 화면으로)
        if self.view == View::Wall
            && (self.agents.is_empty()
                || (self.approvals.is_empty() && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))))
        {
            self.leave_wall(&ctx, View::Grid);
        }
        if self.view == View::Wall {
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(Color32::from_rgb(10, 9, 8)).inner_margin(Margin::same(12)))
                .show(ui, |ui| self.screens(ui, true));
        } else {
            self.main_layout(ui, &ctx);
        }
        self.toast_area(&ctx);
    }
}

impl GhostApp {
    /// 보통 화면: 왼쪽 사이드바 · (상세면 오른쪽 패널) · 가운데 · 모달 창
    fn main_layout(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let ctx = ctx.clone();
        egui::Panel::left("left")
            .resizable(true)
            .default_size(248.0)
            .min_size(200.0)
            .max_size(380.0)
            .frame(egui::Frame::new().fill(theme::SURFACE).inner_margin(Margin::symmetric(10, 14)))
            .show(ui, |ui| self.sidebar(ui));
        // 상세 보기: 오른쪽 패널(상태·제어·기록·지시) + 가운데 라이브 화면
        if self.view == View::Detail && !self.selected.is_some_and(|i| i < self.agents.len()) {
            self.view = View::Grid;
        }
        if self.view == View::Detail {
            if let Some(idx) = self.selected {
                let mut remove = false;
                egui::Panel::right("detail_side")
                    .resizable(true)
                    .default_size(400.0)
                    .min_size(320.0)
                    .frame(egui::Frame::new().fill(theme::SURFACE).inner_margin(Margin::same(16)))
                    .show(ui, |ui| remove = self.detail_side(ui, idx));
                if remove {
                    self.remove_agent(idx);
                }
            }
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(Margin::same(24)))
            .show(ui, |ui| match self.view {
                View::Home => self.welcome(ui),
                View::Detail => self.detail_view(ui, self.selected.unwrap()),
                _ => self.grid(ui),
            });

        // 모달 창: 승인 요청이 있으면 그것만 보인다 (다른 창은 잠시 숨김 — 적던 내용은 유지)
        if self.approvals.is_empty() {
            self.settings_window(&ctx);
            self.connect_window(&ctx);
            self.new_agent_window(&ctx);
            // 별도 세션 안내 (macOS: LaunchAgent, Windows: 자식 세션 켜기)
            if caps::SESSION {
                self.session_guide_window(&ctx);
            }
        }

    }

    /// 잠깐 보이는 알림 (모든 창 위)
    fn toast_area(&mut self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        let now = ctx.input(|i| i.time);
        if let Some((msg, until)) = &self.toast {
            if now < *until {
                egui::Area::new(egui::Id::unique("toast"))
                    .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -28.0])
                    .order(egui::Order::Tooltip)
                    .show(&ctx, |ui| {
                        egui::Frame::new()
                            .fill(theme::CARD_HOVER)
                            .stroke(Stroke::new(1.0, theme::BORDER_HOVER))
                            .corner_radius(theme::CARD_RADIUS)
                            .inner_margin(Margin::symmetric(16, 10))
                            .shadow(ui.style().visuals.popup_shadow)
                            .show(ui, |ui| ui.label(RichText::new(msg).color(theme::TEXT)));
                    });
                ctx.request_repaint_after(std::time::Duration::from_millis(250));
            } else {
                self.toast = None;
            }
        }
    }
}

impl Drop for GhostApp {
    fn drop(&mut self) {
        // 종료 시 모든 격리 화면을 정리한다. 락에서 먼저 꺼내(락을 쥔 채 block_on 하지 않음)
        // 모두 동시에 닫고, 전체에 시간 제한 하나만 둔다.
        let mut desks = Vec::new();
        for a in self.agents.drain(..) {
            a.shared.removed.store(true, Ordering::SeqCst);
            a.task.abort();
            let ws = a.shared.workspace.lock().take();
            if let Some(ws) = ws {
                desks.push(ws);
            }
        }
        if desks.is_empty() {
            return;
        }
        // timeout(Sleep) 은 런타임 안에서 만들어야 한다: block_on 밖(메인 스레드)에서 만들면
        // "there is no reactor running" 으로 패닉해 화면을 정리하지 못한 채 끝난다.
        let _ = self.rt.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                futures_util::future::join_all(desks.iter().map(|ws| ws.shutdown())),
            )
            .await
        });
    }
}

/// 외부 에이전트 자동 연결 상태 (화면은 매 프레임 그리므로 파일·경로 조회가 필요한 값은 백그라운드에서 채워 둔다)
#[derive(Clone, Default)]
struct ConnectState {
    busy: bool,
    clients: Vec<crate::install::ClientStatus>,
    /// clients 와 같은 순서: 지금 설치본으로 연결돼 있는가
    ok: Vec<bool>,
    messages: Vec<String>,
    /// '켤 때 자동으로 연결' 설정
    auto: bool,
    /// 설치 위치 (표시용)
    install_dir: String,
}

impl ConnectState {
    fn connected_any(&self) -> bool {
        self.ok.iter().any(|&b| b)
    }
}

/// 클라이언트 상태와 표시용 값을 읽는다 (파일 조회 — UI 스레드에서 부르지 않는다)
fn load_connect_status() -> ConnectState {
    let clients = crate::install::status_all();
    let exe = crate::install::stable_exe();
    ConnectState {
        ok: clients.iter().map(|c| c.registered.as_deref().is_some_and(|p| registered_is(p, &exe))).collect(),
        clients,
        install_dir: crate::install::stable_app().display().to_string(),
        ..Default::default()
    }
}

/// 읽어 온 상태를 반영한다 (진행 중 표시·메시지·자동 연결 설정은 그대로 — 자동 연결은 이 창의 스위치로만 바뀐다)
fn apply_connect_status(dst: &mut ConnectState, src: ConnectState) {
    dst.clients = src.clients;
    dst.ok = src.ok;
    dst.install_dir = src.install_dir;
}

fn refresh_connect_status(rt: &Handle, st: Arc<parking_lot::Mutex<ConnectState>>, ctx: egui::Context) {
    rt.spawn_blocking(move || {
        let fresh = load_connect_status();
        apply_connect_status(&mut st.lock(), fresh);
        ctx.request_repaint();
    });
}

/// 설치 + 모든 클라이언트 연결 (백그라운드)
fn spawn_connect(rt: &Handle, st: Arc<parking_lot::Mutex<ConnectState>>, ctx: egui::Context) {
    st.lock().busy = true;
    rt.spawn_blocking(move || {
        let mut msgs = Vec::new();
        match crate::install::connect_all() {
            Ok((_, res)) => {
                for (c, r) in res {
                    msgs.push(match r {
                        Ok(m) => format!("{}: {m}", c.name()),
                        Err(e) => format!("{}: 실패 — {e:#}", c.name()),
                    });
                }
            }
            Err(e) => msgs.push(format!("설치 실패: {e:#}")),
        }
        let fresh = load_connect_status();
        let mut s = st.lock();
        s.busy = false;
        apply_connect_status(&mut s, fresh);
        s.messages = msgs;
        ctx.request_repaint();
    });
}

/// 한 클라이언트 연결/해제 (백그라운드)
fn run_client_action(rt: &Handle, st: Arc<parking_lot::Mutex<ConnectState>>, ctx: egui::Context, c: Option<crate::install::Client>, connect: bool) {
    st.lock().busy = true;
    rt.spawn_blocking(move || {
        let mut msgs = Vec::new();
        if let Some(c) = c {
            let r = if connect {
                crate::install::install_app().and_then(|exe| crate::install::register(c, &exe))
            } else {
                crate::install::unregister(c)
            };
            msgs.push(match r {
                Ok(m) => format!("{}: {m}", c.name()),
                Err(e) => format!("{}: 실패 — {e:#}", c.name()),
            });
        }
        let fresh = load_connect_status();
        let mut s = st.lock();
        s.busy = false;
        apply_connect_status(&mut s, fresh);
        s.messages = msgs;
        ctx.request_repaint();
    });
}

/// 클라이언트 설정에 등록된 실행 파일 경로가 지금 설치된 GhostDesk 인가.
/// macOS 는 기존대로 문자열 비교. Windows 는 대소문자·구분자·`\\?\` 접두사 차이를 무시하고,
/// 그래도 다르면 실제 파일이 같은지 본다 (드라이브 문자 대소문자 등으로 '다른 경로로 연결됨' 이 잘못 뜨지 않게).
fn registered_is(registered: &str, exe: &std::path::Path) -> bool {
    if !cfg!(windows) {
        return registered == exe.display().to_string();
    }
    use crate::platform::paths;
    let norm = |p: &std::path::Path| paths::simplify(p).to_string_lossy().replace('/', "\\").to_lowercase();
    let reg = std::path::Path::new(registered.trim());
    norm(reg) == norm(exe) || paths::same_path(reg, exe)
}

/// 앱 경로 비교. Windows 는 대소문자·구분자 차이를 무시한다 (macOS 는 기존대로 그대로 비교).
fn same_app_path(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        let norm = |s: &str| s.trim().replace('/', "\\").trim_end_matches('\\').to_lowercase();
        norm(a) == norm(b)
    } else {
        a == b
    }
}

/// 고른 경로가 이 Electron 앱인가 (macOS: 앱 번들 경로. Windows: 설치 폴더 또는 실행 파일 경로)
fn electron_matches(e: &crate::electron::ElectronApp, path: &str) -> bool {
    same_app_path(&e.bundle.display().to_string(), path) || (cfg!(windows) && same_app_path(&e.exe.display().to_string(), path))
}

type Approval = (String, String, Option<tokio::sync::oneshot::Sender<bool>>);

/// 응답을 받을 쪽이 사라진(시간 초과·연결 끊김) 승인 요청을 버린다.
fn current_session_app_path(electrons: &[crate::electron::ElectronApp], selected: &str) -> String {
    electrons.iter().find(|e| electron_matches(e, selected))
        .map(|e| e.exe.display().to_string()).unwrap_or_else(|| selected.to_string())
}

fn prune_closed_approvals(list: &mut Vec<Approval>) {
    list.retain(|(_, _, r)| r.as_ref().is_some_and(|r| !r.is_closed()));
}

/// '로컬 앱'(Mac 앱 / Windows 앱) 목록의 한 항목
#[derive(Debug, Clone, PartialEq)]
struct AppListItem {
    label: String,
    path: String,
    /// 숨김 실행 가능한 Electron 앱이면 그 인덱스
    electron: Option<usize>,
    /// 고를 수 없는 항목이면 그 이유 (숨김 실행이 안 되는 Electron 앱)
    disabled: Option<String>,
}

/// Electron 앱(숨김 실행 가능 별표 / 불가 — 비활성 + 이유)과 나머지 로컬 앱을 한 목록으로 만든다.
/// 감지된 Electron 앱은 네이티브 항목으로 다시 넣지 않는다 (네이티브 경로로 조용히 새지 않게).
/// Windows 에서는 설치 목록의 실행 파일 경로가 Electron 앱의 실행 파일과 같아도 같은 앱으로 본다.
fn app_list_items(electrons: &[crate::electron::ElectronApp], native_apps: &[(String, String)], current_session: bool) -> Vec<AppListItem> {
    let mut items: Vec<AppListItem> = Vec::new();
    for (i, e) in electrons.iter().enumerate() {
        let path = if cfg!(windows) && current_session { &e.exe } else { &e.bundle }.display().to_string();
        if items.iter().any(|it| same_app_path(&it.path, &path)) {
            continue;
        }
        if cfg!(windows) && current_session {
            items.push(AppListItem { label: e.name.clone(), path, electron: None, disabled: None });
            continue;
        }
        if e.hideable {
            items.push(AppListItem { label: format!("{} {}", icon::STAR, e.name), path, electron: Some(i), disabled: None });
        } else {
            let reason = e.reason.clone().filter(|r| !r.trim().is_empty()).unwrap_or_else(|| "숨김 실행을 지원하지 않습니다".into());
            items.push(AppListItem {
                label: format!("{} — 숨김 실행 불가", e.name),
                path,
                electron: None,
                disabled: Some(format!("{} 은(는) 숨김 실행을 지원하지 않습니다: {reason}", e.name)),
            });
        }
    }
    for (name, path) in native_apps {
        let dup = items.iter().any(|it| same_app_path(&it.path, path))
            || (cfg!(windows) && electrons.iter().any(|e| electron_matches(e, path)));
        if !dup {
            items.push(AppListItem { label: name.clone(), path: path.clone(), electron: None, disabled: None });
        }
    }
    // 고를 수 있는 앱을 먼저, 숨김 실행이 안 되는 앱은 맨 뒤에 (순서는 그대로)
    items.sort_by_key(|it| it.disabled.is_some());
    items
}

/// /Applications, /System/Applications 의 앱 목록 (이름, 경로)
#[cfg(target_os = "macos")]
fn list_mac_apps() -> Vec<(String, String)> {
    let mut v = Vec::new();
    for dir in ["/Applications", "/System/Applications", "/System/Applications/Utilities"] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "app").unwrap_or(false) {
                    let name = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    v.push((name, p.display().to_string()));
                }
            }
        }
    }
    v.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    v
}

/// 한글 글꼴 후보 (앞에서부터 처음 읽히는 하나를 기본 글꼴 뒤에 대체 글꼴로 붙인다).
fn font_candidates() -> Vec<std::path::PathBuf> {
    #[cfg(windows)]
    {
        windows_font_candidates(&windows_fonts_dir(), dirs::data_local_dir().as_deref())
    }
    #[cfg(not(windows))]
    {
        [
            "/System/Library/Fonts/AppleSDGothicNeo.ttc",
            "/System/Library/Fonts/Supplemental/AppleGothic.ttf",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/nanum/NanumGothic.ttf",
        ]
        .iter()
        .map(std::path::PathBuf::from)
        .collect()
    }
}

/// Windows 한글 글꼴 후보: 맑은 고딕(Windows 기본 한글 글꼴) → 굴림·바탕(선택 설치 글꼴) →
/// 사용자가 설치한 나눔고딕·Noto Sans KR (모든 사용자용 `fonts` 폴더, 그다음 사용자별 글꼴 폴더).
/// `local_appdata` 는 사용자별 글꼴 폴더(%LOCALAPPDATA%\Microsoft\Windows\Fonts)의 기준.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_font_candidates(fonts: &std::path::Path, local_appdata: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    const SYSTEM: &[&str] = &["malgun.ttf", "malgunsl.ttf", "gulim.ttc", "batang.ttc"];
    const ADDED: &[&str] = &["NanumGothic.ttf", "NotoSansKR-Regular.otf", "NotoSansKR-Regular.ttf", "NotoSansCJKkr-Regular.otf"];
    let user = local_appdata.map(|d| d.join("Microsoft").join("Windows").join("Fonts"));
    let mut v: Vec<std::path::PathBuf> = SYSTEM.iter().map(|f| fonts.join(f)).collect();
    for f in ADDED {
        v.push(fonts.join(f));
        if let Some(u) = &user {
            v.push(u.join(f));
        }
    }
    v
}

/// 시스템 글꼴 폴더: 알려진 폴더(FOLDERID_Fonts) → %SystemRoot%\Fonts → C:\Windows\Fonts.
/// (MCP 호스트가 환경변수를 지운 채 GUI 를 띄워도 찾을 수 있게 알려진 폴더 API 를 먼저 쓴다.)
#[cfg(windows)]
fn windows_fonts_dir() -> std::path::PathBuf {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Fonts, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    let known = unsafe {
        let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(&FOLDERID_Fonts, KF_FLAG_DEFAULT as u32, std::ptr::null_mut(), &mut p);
        let out = (hr >= 0 && !p.is_null()).then(|| std::path::PathBuf::from(crate::platform::win::from_wide_ptr(p)));
        if !p.is_null() {
            CoTaskMemFree(p as *const core::ffi::c_void);
        }
        out
    };
    known
        .filter(|d| !d.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("SystemRoot")
                .or_else(|| std::env::var_os("windir"))
                .map(|w| std::path::PathBuf::from(w).join("Fonts"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows\Fonts"))
}

fn setup_fonts(ctx: &egui::Context) {
    let candidates = font_candidates();
    let mut fonts = egui::FontDefinitions::default();
    for path in candidates {
        if let Ok(bytes) = std::fs::read(&path) {
            fonts.font_data.insert("korean".into(), Arc::new(egui::FontData::from_owned(bytes)));
            // 본문 글꼴은 시스템 한글 글꼴(맑은 고딕·Apple SD 산돌고딕 Neo)을 맨 앞에 두어 영문·한글 모양을 맞춘다.
            // 고정폭 글꼴은 기존 글꼴을 쓰고 한글만 이 글꼴로 채운다.
            fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "korean".into());
            fonts.families.entry(egui::FontFamily::Monospace).or_default().push("korean".into());
            break;
        }
    }
    // Lucide 아이콘 글꼴은 사용자 영역(U+E000~) 글자만 있으므로 맨 앞에 두어도 다른 글자 모양을 바꾸지 않는다
    fonts.font_data.insert("lucide".into(), Arc::new(egui::FontData::from_static(crate::icon::FONT)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().insert(0, "lucide".into());
    }
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn electron(name: &str, hideable: bool, reason: Option<&str>) -> crate::electron::ElectronApp {
        crate::electron::ElectronApp {
            name: name.into(),
            bundle_id: format!("test.{name}"),
            bundle: PathBuf::from(format!("/Applications/{name}.app")),
            exe: PathBuf::from(format!("/Applications/{name}.app/Contents/MacOS/{name}")),
            app_version: None,
            electron_version: None,
            fuses: None,
            hideable,
            reason: reason.map(String::from),
            family: crate::electron::AppFamily::Generic,
            user_data_guess: None,
            main_script: None,
            running: false,
            launch_profile: String::new(),
        }
    }

    #[cfg(windows)]
    #[test]
    fn current_session_preserves_an_existing_electron_selection() {
        let app = electron("Code", true, None);
        let apps = vec![app.clone()];
        let previous = app_list_items(&apps, &[], false);
        assert_eq!(current_session_app_path(&apps, &previous[0].path), app.exe.display().to_string());
        assert_eq!(current_session_app_path(&apps, "charmap"), "charmap");
    }

    #[cfg(windows)]
    #[test]
    fn current_session_allows_native_electron_apps() {
        let app = electron("Discord", false, Some("인스펙터 퓨즈 꺼짐"));
        let items = app_list_items(&[app.clone()], &[], true);
        assert!(items[0].disabled.is_none(), "명시한 현재 계정 모드는 CDP 퓨즈를 요구하지 않음");
        assert_eq!(items[0].electron, None);
        assert_eq!(items[0].path, app.exe.display().to_string());
    }

    #[test]
    fn non_hideable_electron_apps_are_listed_disabled() {
        let electrons = vec![electron("Code", true, None), electron("Discord", false, Some("퓨즈가 꺼져 있음"))];
        let natives = vec![
            ("Code".to_string(), "/Applications/Code.app".to_string()),
            ("Discord".to_string(), "/Applications/Discord.app".to_string()),
            ("TextEdit".to_string(), "/System/Applications/TextEdit.app".to_string()),
        ];
        let items = app_list_items(&electrons, &natives, false);
        assert_eq!(items.len(), 3, "{items:?}");
        let code = items.iter().find(|i| i.path.ends_with("Code.app")).unwrap();
        assert_eq!(code.electron, Some(0));
        assert!(code.disabled.is_none());
        // 숨김 실행이 안 되는 Electron 앱은 사라지지 않고 비활성 + 이유로 보이며, 네이티브 항목으로 다시 들어가지 않는다
        let discord: Vec<_> = items.iter().filter(|i| i.path.ends_with("Discord.app")).collect();
        assert_eq!(discord.len(), 1);
        assert!(discord[0].disabled.as_deref().unwrap().contains("퓨즈가 꺼져 있음"));
        assert_eq!(discord[0].electron, None);
        let te = items.iter().find(|i| i.path.ends_with("TextEdit.app")).unwrap();
        assert!(te.disabled.is_none() && te.electron.is_none());
    }

    #[test]
    fn windows_font_candidate_order() {
        use std::path::Path;
        let v = windows_font_candidates(Path::new(r"D:\Win\Fonts"), Some(Path::new(r"C:\Users\u\AppData\Local")));
        // 맑은 고딕이 가장 먼저, 시스템 글꼴 폴더 기준
        assert_eq!(v[0], Path::new(r"D:\Win\Fonts").join("malgun.ttf"));
        let pos = |p: &std::path::Path| v.iter().position(|x| x == p).unwrap_or_else(|| panic!("{p:?} 없음: {v:?}"));
        let gulim = pos(&Path::new(r"D:\Win\Fonts").join("gulim.ttc"));
        let nanum_sys = pos(&Path::new(r"D:\Win\Fonts").join("NanumGothic.ttf"));
        let nanum_user = pos(&Path::new(r"C:\Users\u\AppData\Local").join("Microsoft").join("Windows").join("Fonts").join("NanumGothic.ttf"));
        assert!(0 < gulim && gulim < nanum_sys && nanum_sys < nanum_user, "{v:?}");
        // 사용자별 글꼴 폴더를 모르면 시스템 폴더만
        let only_sys = windows_font_candidates(Path::new(r"C:\Windows\Fonts"), None);
        assert!(only_sys.iter().all(|p| p.starts_with(r"C:\Windows\Fonts")), "{only_sys:?}");
    }

    #[test]
    fn korean_font_is_loaded() {
        // 실제 글꼴 파일을 찾아 egui 에 넣었을 때 한글 글리프가 생기는가 (기본 egui 글꼴에는 한글이 없다)
        let ctx = egui::Context::default();
        setup_fonts(&ctx);
        ctx.run_ui(Default::default(), |_ui| {}).drop_without_applying_deltas();
        let ok = ctx.fonts_mut(|f| f.has_glyphs(&egui::FontId::proportional(14.0), "한글 에이전트 설정"));
        if cfg!(any(windows, target_os = "macos")) {
            assert!(ok, "한글 글꼴을 찾지 못함: {:?}", font_candidates());
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_finds_malgun_gothic() {
        let dir = windows_fonts_dir();
        assert!(dir.is_dir(), "{dir:?}");
        assert!(dir.ends_with("Fonts"), "{dir:?}");
        // Windows 10/11 에는 맑은 고딕이 항상 설치되어 있다
        let first = font_candidates().into_iter().find(|p| p.is_file()).expect("한글 글꼴 없음");
        assert!(first.file_name().unwrap().to_string_lossy().eq_ignore_ascii_case("malgun.ttf"), "{first:?}");
    }

    #[test]
    fn registered_path_comparison() {
        use std::path::Path;
        let exe = if cfg!(windows) { Path::new(r"C:\Users\u\AppData\Local\Programs\GhostDesk\ghostdesk.exe") } else { Path::new("/Applications/GhostDesk.app/Contents/MacOS/ghostdesk") };
        assert!(registered_is(&exe.display().to_string(), exe));
        assert!(!registered_is("/somewhere/else/ghostdesk", exe));
        if cfg!(windows) {
            // 드라이브 문자 대소문자·구분자·verbatim 접두사 차이는 같은 경로
            assert!(registered_is(r"c:\users\u\appdata\local\programs\ghostdesk\GHOSTDESK.EXE", exe));
            assert!(registered_is("C:/Users/u/AppData/Local/Programs/GhostDesk/ghostdesk.exe", exe));
            assert!(registered_is(r"\\?\C:\Users\u\AppData\Local\Programs\GhostDesk\ghostdesk.exe", exe));
            assert!(!registered_is(r"C:\Users\u\Downloads\ghostdesk.exe", exe));
        } else {
            // macOS 는 기존대로 정확히 같은 문자열만
            assert!(!registered_is(&exe.display().to_string().to_uppercase(), exe));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_app_list_merges_electron_exe() {
        let mut code = electron("Code", true, None);
        code.bundle = PathBuf::from(r"C:\Users\u\AppData\Local\Programs\Microsoft VS Code");
        code.exe = PathBuf::from(r"C:\Users\u\AppData\Local\Programs\Microsoft VS Code\Code.exe");
        let mut slack = electron("Slack", false, Some("퓨즈가 꺼져 있음"));
        slack.bundle = PathBuf::from(r"C:\Users\u\AppData\Local\slack");
        slack.exe = PathBuf::from(r"C:\Users\u\AppData\Local\slack\slack.exe");
        let electrons = vec![code, slack];
        let natives = vec![
            ("Visual Studio Code".to_string(), r"c:\users\u\appdata\local\programs\microsoft vs code\code.exe".to_string()),
            ("Slack".to_string(), r"C:\Users\u\AppData\Local\slack\slack.exe".to_string()),
            ("메모장".to_string(), r"C:\Windows\notepad.exe".to_string()),
        ];
        let items = app_list_items(&electrons, &natives, false);
        assert_eq!(items.len(), 3, "{items:?}");
        assert_eq!(items[0].electron, Some(0));
        assert_eq!(items[1].label, "메모장");
        assert!(items[2].disabled.is_some(), "숨김 실행이 안 되는 Electron 앱은 네이티브로 새지 않고 맨 뒤에");
        // 실행 파일 경로를 직접 적어도 Electron 앱으로 인식
        assert!(electron_matches(&electrons[0], r"C:\Users\u\AppData\Local\Programs\Microsoft VS Code\Code.exe"));
        assert!(electron_matches(&electrons[0], r"c:/users/u/appdata/local/programs/microsoft vs code/"));
        assert!(!electron_matches(&electrons[0], r"C:\Windows\notepad.exe"));
    }

    #[test]
    fn unsupported_cards_follow_caps() {
        // macOS 는 모든 카드, Windows 는 가상 디스플레이 없음 (별도 세션 = 자식 세션)
        if cfg!(target_os = "macos") {
            assert!(LOCAL_APP_CARD && caps::SESSION && caps::VDISPLAY);
            assert_eq!(text::PRIMARY_MOD, "cmd");
        }
        if cfg!(windows) {
            assert!(LOCAL_APP_CARD && caps::SESSION && !caps::VDISPLAY);
            assert_eq!(text::LOCAL_APPS, "Windows 앱");
            // 직접 조작의 복사/잘라내기는 Windows 에서 ctrl
            assert_eq!(format!("{}+c", text::PRIMARY_MOD), "ctrl+c");
        }
        assert_eq!(pick("mac", "other"), if cfg!(target_os = "macos") { "mac" } else { "other" });
    }

    #[test]
    fn wall_layout_fills_area() {
        let wide = Vec2::new(1600.0, 900.0);
        // 하나면 화면 높이에 맞춰 크게
        let (c, cell) = wall_layout(1, wide, 0.625, 10.0);
        assert_eq!(c, 1);
        assert!((cell.y - 900.0).abs() < 0.5 && cell.x <= 1600.0, "{cell:?}");
        // 넷이면 2x2
        assert_eq!(wall_layout(4, wide, 0.625, 10.0).0, 2);
        // 둘이면 넓은 화면에서는 나란히
        assert_eq!(wall_layout(2, wide, 0.625, 10.0).0, 2);
        // 세로로 긴 영역에서는 위아래로
        assert_eq!(wall_layout(2, Vec2::new(700.0, 1200.0), 0.625, 10.0).0, 1);
        // 어떤 경우에도 영역 밖으로 넘치지 않는다
        for n in 1..=12 {
            for area in [wide, Vec2::new(800.0, 600.0), Vec2::new(500.0, 1400.0)] {
                let (c, cell) = wall_layout(n, area, 0.625, 10.0);
                let rows = n.div_ceil(c);
                assert!(c as f32 * cell.x + (c - 1) as f32 * 10.0 <= area.x + 0.5, "n={n} {area:?}");
                assert!(rows as f32 * cell.y + (rows - 1) as f32 * 10.0 <= area.y + 0.5, "n={n} {area:?}");
            }
        }
    }

    #[test]
    fn attention_and_names() {
        assert_eq!(attention(&AgentStatus::Thinking, false), None);
        assert_eq!(attention(&AgentStatus::Idle("오류 — 지시를 입력하면 다시 시도".into()), false), Some(theme::DANGER));
        assert_eq!(attention(&AgentStatus::WaitingUser("?".into()), false), Some(theme::WARN));
        assert_eq!(attention(&AgentStatus::Idle("완료".into()), true), Some(theme::TAKEOVER));
        assert_eq!(profile_short("재완 (me@x.com)"), "재완");
        assert_eq!(profile_short("업무 (회사) (me@x.com)"), "업무 (회사)");
        assert_eq!(profile_short("업무 (회사)"), "업무 (회사)");
        assert_eq!(humanize_error("Claude API 연결 실패: error sending request for url"), "Claude API 에 연결할 수 없음");
        assert_eq!(humanize_error("HTTP 401: invalid x-api-key"), "API 키를 확인하세요");
    }

    #[test]
    fn stale_approvals_are_dropped() {
        let (tx1, rx1) = tokio::sync::oneshot::channel::<bool>();
        let (tx2, rx2) = tokio::sync::oneshot::channel::<bool>();
        let mut list: Vec<Approval> = vec![("a".into(), "".into(), Some(tx1)), ("b".into(), "".into(), Some(tx2)), ("c".into(), "".into(), None)];
        drop(rx1); // 요청한 쪽이 시간 초과로 포기
        prune_closed_approvals(&mut list);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "b");
        drop(rx2);
    }
}
