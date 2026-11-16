//! egui 화면: 에이전트 생성, 다중 에이전트 라이브 그리드, 상세 보기(로그/지시/직접 조작).

use std::sync::{atomic::Ordering, Arc};

use eframe::egui::{self, Color32, Margin, RichText, Stroke, StrokeKind, Vec2};
use tokio::{runtime::Handle, sync::mpsc};

use crate::{
    icon,
    agent::{self, AgentCmd, AgentConfig, AgentShared, AgentStatus, DeskTarget, LogKind, ProfileSource, RemoteTarget},
    browser::MouseButton,
    llm::{Llm, Provider, EFFORTS},
    platform::{caps, text},
    profiles::{self, InstalledBrowser},
};

mod connect;
mod detail;
mod fonts;
mod i18n;
mod labels;
mod pip;
mod prefs;
mod screens;
mod settings;
mod sidebar;
mod theme;
mod titlebar;
mod wizard;

use connect::ConnectState;
use detail::{spawn_input_queue, TakeoverInput};
use i18n::{tr, trf, Lang};

/// Windows 자식 세션 켜기(UAC) 요청 상태
#[derive(Default, Clone, PartialEq)]
enum ChildEnable {
    #[default]
    Idle,
    Waiting,
    Done(Result<(), String>),
}

/// 한 AI 회사의 '모델' 목록을 API 에서 불러오는 상태 (회사마다 하나씩, `Provider::ALL` 순서)
#[derive(Default, Clone)]
enum ModelFetch {
    #[default]
    Idle,
    Loading,
    Done(Result<Vec<String>, String>),
}

/// macOS 에서는 기존 문구를, 그 밖의 OS 에서는 그 OS 에 맞는 문구를 고른다.
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
    /// 이 에이전트의 화면을 늘 위에 뜨는 작은 창(PiP)으로 보고 있는가
    pip: bool,
    /// 내 확인 없이 할 수 있는 최대 단계 (0 = 무제한 · 외부 에이전트) — 진행 막대의 끝
    max_steps: u64,
}

impl AgentHandle {
    fn new(rt: &Handle, shared: Arc<AgentShared>, tx: mpsc::UnboundedSender<AgentCmd>, task: tokio::task::JoinHandle<()>) -> Self {
        let input_tx = spawn_input_queue(rt, shared.clone());
        AgentHandle {
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
            pip: false,
            max_steps: 0,
        }
    }
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
    /// 에이전트를 움직일 AI 회사. 키와 모델은 회사마다 따로 기억한다 (`Provider::ALL` 순서)
    provider: Provider,
    api_keys: Vec<String>,
    models: Vec<String>,
    /// '모델' 목록을 열 때 API 에서 불러온 실제 모델 목록 (회사마다)
    model_fetch: Arc<parking_lot::Mutex<Vec<ModelFetch>>>,
    /// API 키를 이 PC 에 (암호화해) 기억할지
    remember_keys: bool,
    /// 화면 언어
    lang: Lang,
    effort_idx: usize,
    /// 홈 화면 인사말 (켤 때마다 다르게)
    greeting: usize,
    browsers: Vec<InstalledBrowser>,
    /// 백그라운드에서 감지한 Electron 앱 목록 (None = 감지 중)
    electron_apps: Slot<Vec<crate::electron::ElectronApp>>,
    /// 로컬 앱 (이름, 경로). macOS: /Applications 의 .app (시작할 때 바로 읽음),
    /// Windows: 설치된 프로그램 (백그라운드에서 읽음, None = 확인 중)
    native_apps: Slot<Vec<(String, String)>>,
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
    /// 지난 프레임의 사이드바·상세 오른쪽 패널 너비 (제목 표시줄을 같은 열로 나눠 칠한다)
    title_cols: titlebar::Columns,
    /// 새 버전 알림 · 내려받기 상태 (백그라운드에서 갱신)
    update: crate::update::Shared,
}

impl GhostApp {
    pub fn new(cc: &eframe::CreationContext<'_>, rt: Handle) -> Self {
        theme::apply(&cc.egui_ctx);
        let browsers = profiles::detect();
        let profile_idx = browsers.first().and_then(|b| if b.profiles.is_empty() { None } else { Some(0) });
        let electron_apps = wizard::load_in_background(&rt, &cc.egui_ctx, crate::electron::detect_apps);
        let native_apps = wizard::native_app_list(&rt, &cc.egui_ctx);
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
        let connect_state = connect::start(&rt, &cc.egui_ctx);
        let update = crate::update::start(&rt, &cc.egui_ctx);
        let mut app = GhostApp {
            update,
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
            // 환경변수에 키가 있는 첫 회사를 고른다 (없으면 Claude)
            provider: Provider::ALL.into_iter().find(|p| p.needs_key() && !p.env_key().is_empty()).unwrap_or(Provider::Anthropic),
            api_keys: Provider::ALL.iter().map(|p| p.env_key()).collect(),
            models: Provider::ALL.iter().map(|p| p.default_models()[0].to_string()).collect(),
            model_fetch: Arc::new(parking_lot::Mutex::new(vec![ModelFetch::Idle; Provider::ALL.len()])),
            remember_keys: prefs::CAN_REMEMBER_KEYS,
            lang: Lang::Ko,
            effort_idx: 1,
            greeting: screens::random_greeting(),
            browsers,
            form: NewAgentForm::new(profile_idx),
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
            title_cols: titlebar::Columns { left: 248.0, right: 0.0 },
        };
        // 저장해 둔 언어·AI·키를 적용하고, 그 언어에 맞는 글꼴을 올린다
        app.load_prefs();
        i18n::set(app.lang);
        fonts::setup_fonts(&cc.egui_ctx, app.lang);
        app
    }

    /// 화면 언어를 바꾼다: 글은 다음 프레임부터, 글꼴은 그 언어의 글자가 있는 것으로
    fn set_lang(&mut self, ctx: &egui::Context, lang: Lang) {
        if self.lang == lang {
            return;
        }
        self.lang = lang;
        i18n::set(lang);
        fonts::setup_fonts(ctx, lang);
        self.save_prefs();
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
                ui.label(RichText::new(icon::SHIELD_ALERT).size(20.0).color(theme::warn()));
                ui.label(RichText::new(&title).size(18.0).strong().color(theme::text()));
            });
            ui.add_space(6.0);
            ui.label(RichText::new(&detail).color(theme::body()));
            ui.add_space(16.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::primary(ui, tr("   허용   ")).clicked() {
                    answer = Some(true);
                }
                if theme::secondary(ui, tr("   거부   ")).clicked() {
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
        self.toast = Some((msg.into(), ctx.input(|i| i.time) + TOAST_SECS));
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
}

impl eframe::App for GhostApp {
    /// 창 바탕: 기본값(반투명 검정)이면 패널 사이 1px 틈과 구분선이 밝은 테마에서 검게 비친다
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        theme::sync(&ctx);
        // 외부 에이전트 이벤트 처리
        while let Ok(ev) = self.control_rx.try_recv() {
            match ev {
                crate::control::ControlEvent::Register(shared) => {
                    // 외부 에이전트가 조작하는 화면: 대화 루프가 없으므로 지시 채널·task 는 빈 것
                    let (tx, _rx) = mpsc::unbounded_channel();
                    let task = self.rt.spawn(async {});
                    self.agents.push(AgentHandle::new(&self.rt, shared, tx, task));
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
        self.spawn_demos(&ctx);
        self.update_textures(&ctx);

        // 화면 모아보기: 다른 패널 없이 화면만 (에이전트가 모두 사라지면 처음 화면으로)
        if self.view == View::Wall
            && (self.agents.is_empty()
                || (self.approvals.is_empty() && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))))
        {
            self.leave_wall(&ctx, View::Grid);
        }
        // 제목 표시줄은 옆 패널보다 먼저 놓아야 창 너비 전체를 차지한다
        if titlebar::visible(&ctx) {
            let cols = if self.view == View::Wall { titlebar::Columns::default() } else { self.title_cols };
            egui::Panel::top("titlebar")
                .exact_size(titlebar::HEIGHT)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::NONE)
                .show(ui, |ui| titlebar::bar(ui, cols));
        }
        if self.view == View::Wall {
            let fade = theme::appear(&ctx, egui::Id::unique("view_in"), self.view_key(), 0.2);
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(theme::bg()).inner_margin(Margin::same(12)))
                .show(ui, |ui| {
                    ui.multiply_opacity(fade);
                    self.screens(ui, true)
                });
        } else {
            self.main_layout(ui, &ctx);
        }
        self.toast_area(&ctx);
        self.pip_windows(&ctx);
        titlebar::resize_edges(&ctx);
    }
}

impl GhostApp {
    /// 보통 화면: 왼쪽 사이드바 · (상세면 오른쪽 패널) · 가운데 · 모달 창
    fn main_layout(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let ctx = ctx.clone();
        // 보기(또는 고른 에이전트)가 바뀌면 가운데와 오른쪽 패널이 0.2초 동안 서서히 나타난다
        let fade = theme::appear(&ctx, egui::Id::unique("view_in"), self.view_key(), 0.2);
        // 제목 표시줄이 있으면 그 아래로 바로 이어지게 위 여백을 줄인다
        let top = if titlebar::visible(&ctx) { 4 } else { 14 };
        let side = egui::Panel::left("left")
            .resizable(true)
            .default_size(248.0)
            .min_size(200.0)
            .max_size(380.0)
            .frame(egui::Frame::new().fill(theme::surface()).inner_margin(Margin { left: 10, right: 10, top, bottom: 14 }))
            .show(ui, |ui| self.sidebar(ui));
        self.title_cols = titlebar::Columns { left: side.response.rect.width(), right: 0.0 };
        // 상세 보기: 오른쪽 패널(상태·제어·기록·지시) + 가운데 라이브 화면
        if self.view == View::Detail && self.selected.is_none_or(|i| i >= self.agents.len()) {
            self.view = View::Grid;
        }
        if self.view == View::Detail {
            if let Some(idx) = self.selected {
                let mut remove = false;
                let detail = egui::Panel::right("detail_side")
                    .resizable(true)
                    .default_size(400.0)
                    .min_size(320.0)
                    .frame(egui::Frame::new().fill(theme::surface()).inner_margin(Margin::same(16)))
                    .show(ui, |ui| {
                        ui.multiply_opacity(fade);
                        remove = self.detail_side(ui, idx);
                    });
                self.title_cols.right = detail.response.rect.width();
                if remove {
                    self.remove_agent(idx);
                }
            }
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::bg()).inner_margin(Margin::same(24)))
            .show(ui, |ui| {
                ui.multiply_opacity(fade);
                match self.view {
                    View::Home => self.welcome(ui),
                    View::Detail => self.detail_view(ui, self.selected.unwrap()),
                    _ => self.grid(ui),
                }
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

    /// 지금 보기를 나타내는 값 (바뀌면 화면 전환 효과를 다시 시작한다)
    fn view_key(&self) -> u64 {
        let view = match self.view {
            View::Home => 0,
            View::Grid => 1,
            View::Detail => 2,
            View::Wall => 3,
        };
        let agent = self.selected.and_then(|i| self.agents.get(i)).map_or(0, |a| a.shared.id);
        view << 48 | agent
    }

    /// 잠깐 보이는 알림 (모든 창 위). 아래에서 살짝 떠오르며 나타나고, 끝날 때 옅어지며 사라진다.
    fn toast_area(&mut self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        let now = ctx.input(|i| i.time);
        if let Some((msg, until)) = &self.toast {
            if now < *until {
                let left = until - now;
                let t = theme::ease_out(((TOAST_SECS - left) / 0.2) as f32).min(theme::ease_out((left / 0.3) as f32));
                egui::Area::new(egui::Id::unique("toast"))
                    .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -28.0 + (1.0 - t) * 10.0])
                    .order(egui::Order::Tooltip)
                    .fade_in(false)
                    .show(&ctx, |ui| {
                        ui.multiply_opacity(t);
                        egui::Frame::new()
                            .fill(theme::card_hover())
                            .stroke(Stroke::new(1.0, theme::border_hover()))
                            .corner_radius(theme::CARD_RADIUS)
                            .inner_margin(Margin::symmetric(16, 10))
                            .shadow(ui.style().visuals.popup_shadow)
                            .show(ui, |ui| ui.label(RichText::new(msg).color(theme::text())));
                    });
                // 나타나는 중·사라지는 중에는 매 프레임, 그 사이에는 사라지기 시작할 때 다시 그린다
                if t < 1.0 {
                    ctx.request_repaint();
                } else {
                    ctx.request_repaint_after(std::time::Duration::from_secs_f64((left - 0.3).max(0.0)));
                }
            } else {
                self.toast = None;
            }
        }
    }
}

impl Drop for GhostApp {
    fn drop(&mut self) {
        self.save_prefs();
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

type Approval = (String, String, Option<tokio::sync::oneshot::Sender<bool>>);

/// 알림이 보이는 시간 (초)
const TOAST_SECS: f64 = 4.0;

/// 백그라운드에서 채우는 값 (None = 아직 확인 중)
type Slot<T> = Arc<parking_lot::Mutex<Option<T>>>;

/// 응답을 받을 쪽이 사라진(시간 초과·연결 끊김) 승인 요청을 버린다.
fn prune_closed_approvals(list: &mut Vec<Approval>) {
    list.retain(|(_, _, r)| r.as_ref().is_some_and(|r| !r.is_closed()));
}

#[cfg(test)]
mod tests {
    use super::*;

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
