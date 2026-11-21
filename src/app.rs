//! The egui UI: creating agents, the live multi-agent grid, and the detail view (log,
//! instructions, takeover).

use std::collections::HashMap;
use std::sync::{atomic::Ordering, Arc};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Margin, RichText, Stroke, StrokeKind, Vec2};
use tokio::{runtime::Handle, sync::mpsc};

use crate::{
    icon,
    agent::{self, AgentCmd, AgentConfig, AgentShared, AgentStatus, DeskTarget, LogKind, ProfileSource, RemoteTarget},
    browser::MouseButton,
    cleanup,
    llm::{Provider, EFFORTS},
    platform::{caps, text},
    profiles::{self, InstalledBrowser},
};

mod account;
mod agent_panel;
mod connect;
mod detail;
mod fonts;
mod host_settings;
mod ai;
pub(crate) mod i18n;
mod labels;
mod palette;
mod pi_settings;
mod toolbox_ui;
mod pip;
mod prefs;
mod route;
#[cfg(windows)]
pub mod setup;
mod screens;
mod settings;
mod remote_info;
mod saved_remotes;
mod ssh_profiles;
mod teams;
mod usage;
mod sidebar;
mod theme;
mod titlebar;
mod tour;
mod wizard;

/// GhostDesk's icon for its windows (Windows title bar and taskbar, macOS Dock while running).
/// Without it eframe shows its own default icon. Drawn by packaging/macos/make-art.swift.
pub fn window_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/app-icon.png")).unwrap_or_default()
}

use connect::ConnectState;
use detail::{spawn_input_queue, TakeoverInput};
use i18n::{tr, trf, Lang};

/// State of the request to enable Windows child sessions (UAC).
#[derive(Default, Clone, PartialEq)]
enum ChildEnable {
    #[default]
    Idle,
    Waiting,
    Done(Result<(), String>),
}

/// State of fetching an AI provider's model list from the API (one per provider, in
/// `Provider::ALL` order).
#[derive(Default, Clone)]
enum ModelFetch {
    #[default]
    Idle,
    Loading(u64),
    Done(Result<Vec<String>, String>),
}

/// Picks the original text on macOS and the OS-specific text elsewhere.
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
    /// Takeover input queue, applied in order by a single task (spawning per event would reorder
    /// them).
    input_tx: mpsc::UnboundedSender<TakeoverInput>,
    texture: Option<egui::TextureHandle>,
    tex_seq: u64,
    /// Virtual mouse display state (detail view / grid tile).
    cursor_view: crate::cursor::CursorView,
    tile_cursor_view: crate::cursor::CursorView,
    input: String,
    url_input: String,
    takeover: bool,
    /// Text the IME is composing during takeover (shows Korean input on Windows; cleared on commit).
    preedit: String,
    /// Whether this agent's screen is shown in a small always-on-top window (PiP).
    pip: pip::PipState,
    /// Maximum steps allowed without my confirmation (0 = unlimited / external agent); the end of the
    /// progress bar.
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
            pip: Default::default(),
            max_steps: 0,
        }
    }
}

#[derive(Default, PartialEq, Clone, Copy)]
enum View {
    #[default]
    Home,
    Grid,
    Detail,
    /// Screen wall: every agent's screen filling the window (no list or panels).
    Wall,
    /// Teams: shared work boards for agents working together.
    Teams,
    /// What agents used, by day and by agent.
    Usage,
}

/// Kind of isolated screen a new agent uses.
#[derive(Clone, Copy, PartialEq)]
enum FormKind {
    Browser,
    Electron,
    Native,
    Remote,
    Session,
}

/// First choice in the new-agent window.
#[derive(Clone, Copy, PartialEq)]
enum Choice {
    /// Picks between the web and an app on this computer based on the task (`wizard::auto_pick`).
    Auto,
    Web,
    App,
    Session,
    Remote,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoteMode {
    Vnc,
    Ssh,
    Both,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoteEntry {
    Paste,
    Manual,
    Saved,
    Imported,
}

struct NewAgentForm {
    team_id: Option<u64>,
    team_role: String,
    choice: Option<Choice>,
    app_search: String,
    /// Web: use my browser's login state (which profile is `profile_idx`, picked in advanced
    /// settings).
    web_use_login: bool,
    /// Mac app (Electron): clone my app data (logins).
    app_use_login: bool,
    /// Dedicated account name for the separate login session.
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
    /// Experimental: run on a virtual display, plus consent to its side effects.
    native_vdisplay: bool,
    native_vdisplay_consent: bool,
    remote_host: String,
    remote_port: u16,
    remote_user: String,
    remote_pass: String,
    remote_dedicated: bool,
    /// Pairing code of a host computer (kept instead of an address when one was pasted).
    host_code: Option<String>,
    /// The saved entry this form was filled from (nothing to offer saving then).
    saved_id: Option<String>,
    remote_mode: RemoteMode,
    card_input: String,
    card_invalid: bool,
    remote_entry: RemoteEntry,
    ssh_profile: Option<String>,
    ssh: crate::ssh::SshOptions,
    name: String,
    workspace: String,
    refresh: bool,
    browser_idx: usize,
    /// Profile to clone (the wizard only uses it with `web_use_login` on). `None` = empty profile.
    profile_idx: Option<usize>,
    start_url: String,
    task: String,
    headless: bool,
    size_idx: usize,
    max_steps: u64,
    /// Files and commands for pi-backed agents (0 off, 1 read, 2 read, write and run).
    files: u8,
}

pub struct GhostApp {
    rt: Handle,
    /// AI connections added by the user (`ai.rs`). Agents work through the `ai_current` connection.
    ai: Vec<ai::AiConn>,
    /// Id of the current connection (none means the list is empty).
    ai_current: u64,
    /// Id for the next connection created.
    ai_next_id: u64,
    /// Connection id → model list fetched from the API.
    model_fetch: Arc<parking_lot::Mutex<std::collections::HashMap<u64, ModelFetch>>>,
    /// Search text in the home model button's list.
    model_query: String,
    /// Whether to remember API keys on this PC (encrypted).
    remember_keys: bool,
    /// UI language.
    lang: Lang,
    effort_idx: usize,
    /// The open guide (at first launch, or reopened from Settings).
    tour: Option<tour::Tour>,
    /// Whether the guide was finished or skipped (then it's not shown again).
    tour_seen: bool,
    /// Tasks given most recently (newest first, at most `RECENT_TASKS`), offered again on Home.
    recent_tasks: Vec<String>,
    /// Guide screenshot capture mode (`GHOSTDESK_TOUR_CAPTURE`).
    tour_capture: Option<tour::Capture>,
    /// Started by an agent's connection and not yet tucked into the taskbar (Windows, `--background`).
    park_pending: bool,
    /// macOS permission state (rechecked every second: `permissions`).
    #[cfg(target_os = "macos")]
    perm_cache: Option<(std::time::Instant, crate::macos::TccStatus)>,
    /// Whether the sidebar permission notice was dismissed for this run.
    perm_notice_hidden: bool,
    /// Fields in prefs.json this version doesn't know (written back unchanged on save).
    prefs_error: Option<String>,
    prefs_other: serde_json::Map<String, serde_json::Value>,
    ssh_profiles: ssh_profiles::Profiles,
    ssh_editor: Option<ssh_profiles::Editor>,
    share_connection: Option<Slot<remote_info::Share>>,
    /// Public address lookup started from the share card.
    public_ip: Option<Slot<Result<String, String>>>,
    /// Keeps the Mac/PC from sleeping while agents work, even with the screen locked (display off).
    keep_awake: bool,
    /// Tidy up finished screens by themselves (`cleanup`).
    auto_clean: bool,
    usage_ui: usage::UsageUi,
    palette: Option<palette::Palette>,
    account_prompt: Option<account::AccountPrompt>,
    import_prompt: Option<account::ImportPrompt>,
    routing: Option<route::Routing>,
    /// Result of an account session setup running in the background.
    setup_rx: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    usage_saved: Instant,
    sidebar_open: bool,
    /// When each outside agent's screen was first seen closed (for the list grace period).
    closed_since: HashMap<u64, Instant>,
    pip_auto_open: bool,
    pip_auto_close: bool,
    /// Permission bypass: approve external agents' sensitive requests (login cloning, running apps,
    /// remote access, …) without asking. Hard refusals in `control.rs` still apply.
    auto_approve: bool,
    /// Windows: let external agents open shells, terminals and system tools (after a person's yes, or at
    /// once with the permission bypass). Off refuses them. The password managers and GhostDesk itself
    /// stay refused either way.
    allow_shell_tools: bool,
    access: crate::access::Access,
    live_fps_limit: u32,
    /// Run key-based AI agents through pi when it is installed.
    pi_install: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    /// The engine update started from its toast, while it runs.
    pi_update: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    /// How the last engine update ended (shown in the toast until it is closed).
    pi_update_result: Option<Result<(), String>>,
    /// The engine-update toast was closed with 'Later' (until the next launch).
    pi_update_dismissed: bool,
    pi_note: Option<String>,
    toolbox: toolbox_ui::State,
    more_views_open: bool,
    /// The Teams/Usage view the sidebar last saw open, so opening another one unfolds "More" once.
    more_views_view: Option<View>,
    pi_offer_dismissed: bool,
    /// Consent once to experimental features, independently of ordinary permissions.
    allow_experimental: bool,
    /// Whether sleep is being blocked right now (`keep_awake` and an agent working).
    awake: Option<crate::platform::awake::KeepAwake>,
    browsers: Vec<InstalledBrowser>,
    /// Electron apps detected in the background (`None` = still detecting).
    electron_apps: Slot<Vec<crate::electron::ElectronApp>>,
    /// Local apps `(name, path)`. macOS: .app bundles in /Applications (read at startup); Windows:
    /// installed programs (read in the background, `None` = still checking).
    native_apps: Slot<Vec<(String, String)>>,
    form: NewAgentForm,
    agents: Vec<AgentHandle>,
    selected: Option<usize>,
    view: View,
    next_id: u64,
    /// Whether to draw the virtual mouse over agent screens.
    show_cursor: bool,
    /// Agent screen layout: 0 = screen first, 1 = balanced, 2 = chat first (the side panel's width).
    detail_layout: u8,
    /// The agent screen is in the narrow one-column form this frame (no layout choice then).
    compact_now: bool,
    /// Events from the external agent (MCP) control server.
    control_rx: mpsc::UnboundedReceiver<crate::control::ControlEvent>,
    /// Control server state (`None` = running fine).
    control_error: Arc<parking_lot::Mutex<Option<String>>>,
    /// External requests waiting for the user's approval.
    approvals: Vec<Approval>,
    /// Whether a Dock attention request (bounce) was sent for an approval.
    attention_requested: bool,
    /// Show the Connections window.
    show_connect: bool,
    /// Auto-connect result/state (updated in the background).
    connect_state: Arc<parking_lot::Mutex<ConnectState>>,
    /// Show the new-agent / settings window.
    wizard_open: bool,
    show_settings: bool,
    /// Tab shown when Settings opens (kept between openings).
    settings_tab: settings::SettingsTab,
    /// Shared work boards (team.rs), the same store the control server uses.
    teams: Arc<crate::team::TeamStore>,
    teams_ui: teams::TeamsUi,
    /// Settings > About > License, while open (the parsed package list is dropped when it closes)
    license_view: Option<settings::LicenseView>,
    show_session_guide: bool,
    /// Windows child sessions: whether the feature is on (`None` = not checked yet) / enable request
    /// progress and result.
    child_enabled: Option<bool>,
    child_enable: Arc<parking_lot::Mutex<ChildEnable>>,
    /// Development: with GHOSTDESK_DEMO=<n>, auto-starts n empty-profile browser agents.
    pending_demo: u32,
    toast: Option<(String, f64)>,
    host_ui: host_settings::HostUi,
    saved_remotes: Vec<saved_remotes::SavedRemote>,
    save_prompt: Option<saved_remotes::SavePrompt>,
    /// Version whose update toast the person closed with X.
    update_dismissed: Option<String>,
    /// A manual update check in progress or just finished (Settings → About).
    update_check: Option<crate::update::CheckSlot>,
    /// Whether the screen wall turned on full screen (undone when the wall closes).
    wall_fullscreen: bool,
    /// Start was pressed without an API key (shows a hint under the key field in the new-agent
    /// window).
    key_warn: bool,
    /// Whether to show the API key field in the new-agent window (pinned if there was no key when the
    /// window opened, so it doesn't vanish while typing).
    wizard_key_field: bool,
    /// Move input focus to the API key field next frame.
    focus_key: bool,
    /// Whether it was already full screen when the wall opened (the user's full screen is left alone
    /// on close).
    wall_fs_before: bool,
    /// Screen wall showing only this team's members' screens (None = every screen).
    wall_team: Option<u64>,
    /// Widths of the sidebar and detail right panel last frame (the title bar is painted in matching
    /// columns).
    title_cols: titlebar::Columns,
    /// Update notice and download state (updated in the background).
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
        let teams;
        {
            let server = crate::control::ControlServer::new(ctl_tx, cc.egui_ctx.clone());
            teams = server.teams();
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
            settings_tab: Default::default(),
            teams,
            teams_ui: Default::default(),
            license_view: None,
            show_session_guide: false,
            child_enabled: None,
            child_enable: Default::default(),
            electron_apps,
            native_apps,
            rt,
            // without saved settings: providers with a key in the environment (or just Claude); load_prefs
            // overrides this
            ai: Vec::new(),
            ai_current: 0,
            ai_next_id: 0,
            model_fetch: Default::default(),
            model_query: String::new(),
            remember_keys: prefs::CAN_REMEMBER_KEYS,
            lang: Lang::Ko,
            effort_idx: 1,
            ssh_profiles: Default::default(),
            ssh_editor: None,
            share_connection: None,
            public_ip: None,
            keep_awake: true,
            auto_clean: true,
            usage_ui: Default::default(),
            palette: None,
            account_prompt: None,
            import_prompt: None,
            routing: None,
            setup_rx: None,
            usage_saved: Instant::now(),
            sidebar_open: true,
            closed_since: HashMap::new(),
            pip_auto_open: false,
            pip_auto_close: true,
            auto_approve: false,
            allow_shell_tools: true,
            access: Default::default(),
            live_fps_limit: 60,
            pi_install: None,
            pi_update: None,
            pi_update_result: None,
            pi_update_dismissed: false,
            pi_note: None,
            toolbox: Default::default(),
            more_views_open: false,
            more_views_view: None,
            pi_offer_dismissed: false,
            allow_experimental: false,
            tour: None,
            tour_seen: false,
            recent_tasks: Vec::new(),
            tour_capture: tour::Capture::from_env(),
            park_pending: cfg!(windows) && std::env::args().skip(1).any(|a| a == "--background"),
            #[cfg(target_os = "macos")]
            perm_cache: None,
            perm_notice_hidden: false,
            prefs_other: Default::default(),
            prefs_error: None,
            awake: None,
            browsers,
            form: NewAgentForm::new(profile_idx),
            agents: Vec::new(),
            selected: None,
            view: View::default(),
            next_id: 1,
            show_cursor: true,
            detail_layout: 0,
            compact_now: false,
            pending_demo: std::env::var("GHOSTDESK_DEMO").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
            toast: None,
            host_ui: Default::default(),
            saved_remotes: saved_remotes::load(),
            save_prompt: None,
            update_dismissed: None,
            update_check: None,
            wall_fullscreen: false,
            key_warn: false,
            wizard_key_field: false,
            focus_key: false,
            wall_fs_before: false,
            wall_team: None,
            title_cols: titlebar::Columns { left: 248.0, right: 0.0 },
        };
        app.ai = ai::initial(&mut app.ai_next_id);
        app.ai_current = app.ai[0].id;
        // apply the saved language, AI and keys, and load fonts for that language
        app.load_prefs();
        // show the guide on first launch (not in capture mode)
        app.tour_seen = tour::already_seen(app.tour_seen);
        if !app.tour_seen && app.tour_capture.is_none() {
            app.open_tour();
        }
        i18n::set(app.lang);
        // the form was filled before the language was known
        app.form.name = tr("에이전트 1").into();
        fonts::setup_fonts(&cc.egui_ctx, app.lang);
        // started by an agent's connection: hide the window from the first moment (see park_background_window)
        #[cfg(windows)]
        if app.park_pending {
            park_background_window();
        }
        // is the installed engine older than this GhostDesk expects? (runs `pi --help`, so off the UI thread)
        if app.tour_capture.is_none() {
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || {
                crate::pi::check_outdated();
                ctx.request_repaint();
            });
        }
        app
    }

    /// Changes the UI language: text from the next frame, fonts to ones that have that language's
    /// glyphs.
    fn set_lang(&mut self, ctx: &egui::Context, lang: Lang) {
        if self.lang == lang {
            return;
        }
        self.lang = lang;
        i18n::set(lang);
        fonts::setup_fonts(ctx, lang);
        self.save_prefs();
    }

    /// Window asking the user about a sensitive external agent request (modal: blocks everything
    /// else until answered).
    fn approval_window(&mut self, ctx: &egui::Context) {
        // drop the dialog if the requester stopped listening (timeout or disconnect, receiver dropped)
        prune_closed_approvals(&mut self.approvals);
        let Some((title, detail, _)) = self.approvals.first() else {
            if self.attention_requested {
                self.attention_requested = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Reset));
            }
            return;
        };
        // repaint periodically so timed-out requests get cleaned up even without input
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
        let (title, detail) = (title.clone(), detail.clone());
        let mut answer = None;
        // no closing by clicking outside or Esc: one of allow/deny must be chosen
        egui::Modal::new(egui::Id::unique("approval")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::SHIELD_ALERT).size(20.0).color(theme::warn()));
                ui.label(RichText::new(&title).size(18.0).strong().color(theme::text()));
            });
            ui.add_space(6.0);
            ui.label(RichText::new(&detail).color(theme::body()));
            ui.add_space(8.0);
            ui.label(theme::hint(tr("묻지 않게 하려면 설정 > 일반 > 승인에서 바꿀 수 있어요")));
            ui.add_space(16.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::primary(ui, tr("허용")).clicked() {
                    answer = Some(true);
                }
                if theme::secondary(ui, tr("거부")).clicked() {
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

    /// Removes outside agents' screens from the lists a little while after they closed (the agent
    /// called desk_close, disconnected, or the program exited). The screen on display stays.
    fn tidy_finished(&mut self, ctx: &egui::Context) {
        cleanup::set_enabled(self.auto_clean);
        if !self.auto_clean {
            self.closed_since.clear();
            return;
        }
        let now = Instant::now();
        let shown = if self.view == View::Detail { self.selected } else { None };
        let mut gone = Vec::new();
        for (idx, a) in self.agents.iter().enumerate() {
            let finished = a.shared.external.lock().is_some() && a.shared.workspace().map_or(true, |w| w.is_closed());
            let id = a.shared.id;
            if !finished || Some(idx) == shown || matches!(a.shared.status(), AgentStatus::Preparing(_)) {
                self.closed_since.remove(&id);
            } else if now.duration_since(*self.closed_since.entry(id).or_insert(now)) >= cleanup::LIST_GRACE {
                gone.push(idx);
            }
        }
        if !self.agents.is_empty() {
            ctx.request_repaint_after(Duration::from_secs(10));
        }
        for idx in gone.into_iter().rev() {
            self.closed_since.remove(&self.agents[idx].shared.id);
            self.remove_agent(idx);
        }
    }

    /// Writes the usage history now and then (and when the window closes, see `on_exit`).
    fn save_usage(&mut self, ctx: &egui::Context) {
        if self.usage_saved.elapsed() >= Duration::from_secs(15) {
            self.usage_saved = Instant::now();
            save_usage_file();
            crate::platform::trim_memory();
        }
        ctx.request_repaint_after(Duration::from_secs(15));
    }

    fn remove_agent(&mut self, idx: usize) {
        if let Some(ssh) = self.agents[idx].shared.ssh.lock().as_ref() { ssh.close(); }
        let h = self.agents.remove(idx);
        // mark screens still being prepared (internal or external) to close as soon as they're ready
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

    /// Blocks sleep while "keep working while locked" is on and an agent is working, otherwise
    /// releases it.
    ///
    /// External (MCP) agents look idle between calls, so their screen counts as working while it's
    /// open.
    fn update_keep_awake(&mut self) {
        let working = self.keep_awake
            && self.agents.iter().any(|a| {
                detail::is_working(&a.shared.status())
                    || (a.shared.external.lock().is_some() && a.shared.workspace().is_some_and(|w| !w.is_closed()))
            });
        match (working, self.awake.is_some()) {
            (true, false) => self.awake = Some(crate::platform::awake::KeepAwake::start("GhostDesk agents are working")),
            (false, true) => self.awake = None,
            _ => {}
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

/// The file is small, so the write happens on the spot.
fn save_usage_file() {
    crate::usage::global().save_if_dirty();
}

impl eframe::App for GhostApp {
    fn on_exit(&mut self) {
        crate::usage::global().save_if_dirty();
    }

    /// Window clear color. The default (translucent black) shows through the 1 px gaps and
    /// separators between panels as black lines in the light theme.
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        theme::sync(&ctx);
        #[cfg(windows)]
        if self.park_pending {
            self.park_pending = !park_background_window();
            ctx.request_repaint();
        }
        // handle external agent events
        while let Ok(ev) = self.control_rx.try_recv() {
            match ev {
                crate::control::ControlEvent::Register(shared) => {
                    // a screen driven by an external agent has no conversation loop, so the instruction channel and
                    // task are empty
                    let (tx, _rx) = mpsc::unbounded_channel();
                    let task = self.rt.spawn(async {});
                    self.agents.push(AgentHandle::new(&self.rt, shared, tx, task));
                }
                crate::control::ControlEvent::Approval { title, reply, experimental, manual, .. }
                    if !manual && crate::control::approval_allowed(self.auto_approve, self.allow_experimental, experimental) => {
                    // permission bypass: answer at once, and say what was allowed so it never happens unseen
                    let _ = reply.send(true);
                    self.notify(&ctx, trf("자동 허용됨: {}", &[&title]));
                }
                crate::control::ControlEvent::RestartElevated { reply } => {
                    // the elevation prompt blocks until answered, so keep it off the UI thread
                    #[cfg(windows)]
                    {
                        let ctx = ctx.clone();
                        std::thread::spawn(move || match crate::winapp::relaunch_self_elevated() {
                            Ok(()) => {
                                let _ = reply.send(Ok(()));
                                // let the answer reach the agent, then make room for the elevated window
                                std::thread::sleep(Duration::from_millis(1500));
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            Err(e) => {
                                let _ = reply.send(Err(format!("{e:#}")));
                            }
                        });
                    }
                    #[cfg(not(windows))]
                    let _ = reply.send(Err("only supported on Windows".to_string()));
                }
                crate::control::ControlEvent::AccountCreated { name, password } => {
                    self.account_prompt = Some(account::AccountPrompt::created(name, password));
                    ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Critical));
                }
                crate::control::ControlEvent::Approval { title, detail, reply, .. } => {
                    self.approvals.push((title, detail, Some(reply)));
                    #[cfg(windows)]
                    show_approval_window();
                    // Keep the notice visible until the human opens GhostDesk; never take keyboard focus.
                    ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                        egui::UserAttentionType::Critical,
                    ));
                    self.attention_requested = true;
                }
            }
        }
        crate::control::set_shell_tools_allowed(self.allow_shell_tools);
        crate::access::set(self.access);
        crate::rfb::set_live_fps_limit(self.live_fps_limit);
        self.poll_route(&ctx);
        self.approval_window(&ctx);
        self.spawn_demos(&ctx);
        self.update_textures(&ctx);
        let notices: Vec<String> = self.agents.iter().flat_map(|a| std::mem::take(&mut *a.shared.notices.lock())).collect();
        if let Some(n) = notices.last() {
            let text = i18n::tr(n).to_string();
            self.notify(&ctx, text);
        }
        self.update_keep_awake();
        self.tidy_finished(&ctx);
        self.save_usage(&ctx);
        self.tour_capture_tick(&ctx);
        self.shortcuts(&ctx);

        // screen wall: only the screens, no other panels (back to home if every agent is gone)
        if self.view == View::Wall
            && (self.agents.is_empty()
                || (self.approvals.is_empty() && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))))
        {
            self.leave_wall(&ctx, View::Grid);
        }
        // the title bar must be placed before the side panels to span the full window width
        let mut toggle_sidebar = false;
        if titlebar::visible(&ctx) {
            let cols = if self.view == View::Wall { titlebar::Columns::default() } else { self.title_cols };
            // the detail view has no heading of its own: name the agent and the page/window it is on up
            // here (Claude/Codex style)
            let title = match (self.view, self.selected) {
                (View::Detail, Some(i)) if i < self.agents.len() => {
                    let a = &self.agents[i].shared;
                    let page = a.workspace().and_then(|w| {
                        let (views, active) = w.views();
                        views.into_iter().find(|t| Some(&t.id) == active.as_ref()).map(|t| t.title).filter(|t| !t.is_empty())
                    });
                    (a.name.clone(), page.unwrap_or_default())
                }
                _ => Default::default(),
            };
            egui::Panel::top("titlebar")
                .exact_size(titlebar::HEIGHT)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::NONE)
                .show(ui, |ui| toggle_sidebar |= titlebar::bar(ui, cols, self.sidebar_open, (&title.0, &title.1)));
        }
        if toggle_sidebar {
            self.sidebar_open = !self.sidebar_open;
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
        let update_shown = self.update_toast(&ctx);
        self.engine_toast(&ctx, update_shown);
        self.pip_windows(&ctx);
        titlebar::resize_edges(&ctx);
    }
}

impl GhostApp {
    /// App-wide shortcuts (⌘ on macOS, Ctrl elsewhere): K quick switcher, 1-4 switch views, N new agent,
    /// comma Settings, B hide or show the sidebar, period pause or resume every agent. Ignored while a dialog is open so they never act behind it.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, Modifiers};
        let pressed = |k: Key| ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, k));
        // the switcher closes with its own shortcut too
        if self.palette.is_some() {
            if pressed(Key::K) {
                self.palette = None;
            }
            return;
        }
        if self.tour.is_some() || self.wizard_open || self.show_settings || !self.approvals.is_empty() || self.view == View::Wall {
            return;
        }
        if pressed(Key::K) {
            self.open_palette();
        } else if pressed(Key::Num1) {
            self.view = View::Home;
        } else if pressed(Key::Num2) {
            self.view = View::Grid;
        } else if pressed(Key::Num3) {
            self.view = View::Teams;
        } else if pressed(Key::Num4) {
            self.view = View::Usage;
        } else if pressed(Key::N) {
            self.open_wizard();
        } else if pressed(Key::B) {
            self.sidebar_open = !self.sidebar_open;
        } else if pressed(Key::Comma) {
            self.open_settings(settings::SettingsTab::General);
        } else if pressed(Key::Period) {
            self.toggle_pause_all(ctx);
        }
    }

    /// Whether any agent is not paused. Agents under human takeover are already paused and are left alone.
    fn any_running(&self) -> bool {
        self.agents.iter().any(|a| !a.takeover && !a.shared.paused.load(Ordering::Relaxed))
    }

    /// One key for "stop everything for a moment": pauses every agent, or resumes them all when all are paused.
    fn toggle_pause_all(&mut self, ctx: &egui::Context) {
        if self.agents.is_empty() {
            return;
        }
        let pause = self.any_running();
        for a in self.agents.iter().filter(|a| !a.takeover) {
            a.shared.paused.store(pause, Ordering::Relaxed);
        }
        self.notify(ctx, if pause { tr("모든 에이전트를 일시정지했어요") } else { tr("모든 에이전트를 다시 시작했어요") });
    }

    /// Normal layout: left sidebar · (right panel in detail view) · center · modal windows.
    fn main_layout(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let ctx = ctx.clone();
        let (sidebar_w, detail_w, page_padding) = panel_sizes(ctx.content_rect().width());
        // when the view (or selected agent) changes, the center and right panel fade in over 0.2 s
        let fade = theme::appear(&ctx, egui::Id::unique("view_in"), self.view_key(), 0.2);
        // with a title bar, reduce the top margin so content follows right below it. The macOS title bar
        // is shorter (28), so leave a bit more room.
        let top = if titlebar::visible(&ctx) { if cfg!(target_os = "macos") { 10 } else { 4 } } else { 14 };
        let side = if self.sidebar_open {
            Some(
                egui::Panel::left("left")
                    .resizable(true)
                    .default_size(sidebar_w)
                    .min_size(200.0)
                    .max_size(if sidebar_w == 200.0 { 200.0 } else { 380.0 })
                    .frame(egui::Frame::new().fill(theme::surface()).inner_margin(Margin { left: 10, right: 10, top, bottom: 14 }))
                    .show(ui, |ui| self.sidebar(ui)),
            )
        } else {
            // hidden: a slim rail keeps the way back (and the two most used actions) out of the page's way
            Some(
                egui::Panel::left("rail")
                    .resizable(false)
                    .exact_size(68.0)
                    .frame(egui::Frame::new().fill(theme::surface()).inner_margin(Margin { left: 8, right: 8, top, bottom: 14 }))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        let held = ui.input(|i| i.modifiers.command);
                        let k = |key: &str| held.then(|| palette::cmd_key(key));
                        // the title bar holds the sidebar button; without one (full screen, Linux) it is here
                        if !titlebar::visible(&ctx) && theme::rail_button(ui, icon::PANEL_LEFT, k("B").as_deref(), &format!("{} ({})", tr("사이드바 열기"), palette::cmd_key("B"))).clicked() {
                            self.sidebar_open = true;
                        }
                        if theme::rail_button(ui, icon::PLUS, k("N").as_deref(), &format!("{} ({})", tr("새 에이전트"), palette::cmd_key("N"))).clicked() {
                            self.open_wizard();
                        }
                        if theme::rail_button(ui, icon::SEARCH, k("K").as_deref(), &format!("{} ({})", tr("찾기"), palette::cmd_key("K"))).clicked() {
                            self.open_palette();
                        }
                        if theme::rail_button(ui, icon::SETTINGS, k(",").as_deref(), &tr("설정").to_string()).clicked() {
                            self.open_settings(settings::SettingsTab::General);
                        }
                    }),
            )
        };
        let side_width = side.as_ref().map_or(0.0, |s| s.response.rect.width());
        self.title_cols = titlebar::Columns { left: side_width, right: 0.0 };
        // detail view: right panel (status, controls, log, instructions) + live screen in the center
        if self.view == View::Detail && self.selected.is_none_or(|i| i >= self.agents.len()) {
            self.view = View::Grid;
        }
        let compact_detail = ctx.content_rect().width() - side_width < 816.0;
        self.compact_now = compact_detail;
        if self.view == View::Detail && !compact_detail {
            if let Some(idx) = self.selected {
                let mut remove = false;
                let room = ctx.content_rect().width() - side_width;
                let layout = self.detail_layout.min(2);
                let (default_w, max_w) = detail_panel_widths(layout, room, detail_w);
                // a new id per layout so choosing one resets the panel to that layout's width
                let detail = egui::Panel::right(("detail_side", layout))
                    .resizable(true)
                    .default_size(default_w)
                    .min_size(320.0)
                    .max_size(max_w)
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
            .frame(egui::Frame::new().fill(theme::bg()).inner_margin(Margin::same(page_padding as i8)))
            .show(ui, |ui| {
                ui.multiply_opacity(fade);
                match self.view {
                    View::Home => self.welcome(ui),
                    View::Detail if compact_detail => self.compact_detail(ui, self.selected.unwrap()),
                    View::Detail => self.detail_view(ui, self.selected.unwrap()),
                    View::Teams => self.teams_view(ui),
                    View::Usage => self.usage_view(ui),
                    _ => self.grid(ui),
                }
            });

        // modal windows: with an approval request pending, only that shows (others are hidden for now,
        // typed text is kept)
        if self.approvals.is_empty() {
            self.settings_window(&ctx);
            self.connect_window(&ctx);
            self.palette_window(&ctx);
            self.account_window(&ctx);
            self.import_window(&ctx);
            self.save_remote_window(&ctx);
            self.new_agent_window(&ctx);
            self.tour_window(&ctx);
            // separate session guide (macOS: LaunchAgent; Windows: enabling child sessions)
            if caps::SESSION {
                self.session_guide_window(&ctx);
            }
        }
    }

    /// A value identifying the current view (a change restarts the transition).
    fn view_key(&self) -> u64 {
        let view = match self.view {
            View::Home => 0,
            View::Grid => 1,
            View::Detail => 2,
            View::Wall => 3,
            View::Teams => 4,
            View::Usage => 5,
        };
        let agent = self.selected.and_then(|i| self.agents.get(i)).map_or(0, |a| a.shared.id);
        view << 48 | agent
    }

    /// Update notice as a toast at the bottom right. It stays until the person presses X (hidden for that version).
    /// Returns whether it is on screen (other toasts then stack above it).
    fn update_toast(&mut self, ctx: &egui::Context) -> bool {
        use crate::update::State;
        let st = self.update.lock().clone();
        let r = match &st {
            State::None => return false,
            State::Available(r) | State::Downloading(r, _) | State::Ready(r) | State::Failed(r, _) => r.clone(),
        };
        if self.update_dismissed.as_deref() == Some(r.version.as_str()) {
            return false;
        }
        let mut close = false;
        egui::Area::new(egui::Id::new("update_toast"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -16.0])
            .order(egui::Order::Tooltip)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(theme::card_hover())
                    .stroke(Stroke::new(1.0, theme::border_hover()))
                    .corner_radius(theme::CARD_RADIUS)
                    .inner_margin(Margin::symmetric(14, 10))
                    .shadow(ui.style().visuals.popup_shadow)
                    .show(ui, |ui| {
                        ui.set_width(260.0);
                        ui.spacing_mut().item_spacing.y = 6.0;
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(trf("새 버전 {} 이 나왔어요", &[&r.version])).strong().color(theme::text()));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                close = theme::icon_button(ui, icon::X, tr("닫기")).clicked();
                            });
                        });
                        match &st {
                            State::Downloading(_, f) => {
                                ui.horizontal(|ui| {
                                    theme::spinner(ui, 13.0, theme::muted());
                                    ui.label(theme::hint(trf("내려받는 중… {}%", &[&((f * 100.0) as u32)])));
                                });
                                theme::progress(ui, egui::Id::new("update_progress"), *f, ui.available_width());
                            }
                            State::Ready(_) => {
                                if theme::primary(ui, tr("다시 시작해서 적용"))
                                    .on_hover_text(tr("GhostDesk 를 껐다 켜요. 일하던 에이전트는 멈춰요."))
                                    .clicked()
                                {
                                    crate::update::request_relaunch(ui.ctx());
                                }
                            }
                            _ => {
                                let failed = matches!(st, State::Failed(..));
                                if let State::Failed(_, e) = &st {
                                    ui.label(RichText::new(trf("업데이트 실패: {}", &[&crate::llm::short(e, 120)])).size(12.0).color(theme::warn()))
                                        .on_hover_text(e.as_str());
                                }
                                ui.horizontal(|ui| {
                                    if r.installable() {
                                        if theme::primary(ui, if failed { tr("다시 시도") } else { tr("업데이트") })
                                            .on_hover_text(tr("새 버전을 내려받아 바꿔 넣어요. 다 받으면 다시 시작해서 적용해요."))
                                            .clicked()
                                        {
                                            crate::update::download(&self.rt, self.update.clone(), ui.ctx().clone());
                                        }
                                        ui.hyperlink_to(RichText::new(tr("새 소식")).size(12.0), &r.page);
                                    } else {
                                        ui.hyperlink_to(RichText::new(tr("내려받기")).size(12.0), &r.page);
                                    }
                                });
                            }
                        }
                    });
            });
        if close {
            self.update_dismissed = Some(r.version.clone());
        }
        true
    }

    /// The installed AI engine (pi) is older than this GhostDesk expects: offer to update it, at the bottom right
    /// (above the app's own update notice when that is showing). Nothing runs until the person presses Update.
    fn engine_toast(&mut self, ctx: &egui::Context, above_update: bool) {
        if let Some(rx) = &self.pi_update {
            match rx.try_recv() {
                Ok(r) => {
                    self.pi_update = None;
                    self.pi_update_result = Some(r);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(300)),
                Err(_) => self.pi_update = None,
            }
        }
        let running = self.pi_update.is_some();
        let outdated = crate::pi::OUTDATED.load(Ordering::Relaxed);
        if self.pi_update_dismissed || self.tour_capture.is_some() || !(outdated || running || self.pi_update_result.is_some()) {
            return;
        }
        let (mut later, mut start, mut close) = (false, false, false);
        egui::Area::new(egui::Id::new("engine_toast"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, if above_update { -150.0 } else { -16.0 }])
            .order(egui::Order::Tooltip)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(theme::card_hover())
                    .stroke(Stroke::new(1.0, theme::border_hover()))
                    .corner_radius(theme::CARD_RADIUS)
                    .inner_margin(Margin::symmetric(14, 10))
                    .shadow(ui.style().visuals.popup_shadow)
                    .show(ui, |ui| {
                        ui.set_width(260.0);
                        ui.spacing_mut().item_spacing.y = 6.0;
                        match (&self.pi_update_result, running) {
                            (_, true) => {
                                ui.label(RichText::new(tr("AI 엔진 업데이트")).strong().color(theme::text()));
                                ui.horizontal(|ui| {
                                    theme::spinner(ui, 13.0, theme::muted());
                                    ui.label(theme::hint(tr("최신 버전을 설치하는 중… 1~2분 걸릴 수 있어요")));
                                });
                            }
                            (Some(Ok(())), _) => {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(format!("{}  {}", icon::CIRCLE_CHECK, tr("AI 엔진을 업데이트했어요"))).strong().color(theme::success()));
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        close = theme::icon_button(ui, icon::X, tr("닫기")).clicked();
                                    });
                                });
                                ui.label(theme::hint(tr("다음 지시부터 새 엔진으로 일해요")));
                            }
                            (failed, _) => {
                                if let Some(Err(e)) = failed {
                                    ui.label(RichText::new(tr("AI 엔진을 업데이트할까요?")).strong().color(theme::text()));
                                    ui.label(RichText::new(crate::llm::short(e, 140)).size(12.0).color(theme::warn())).on_hover_text(e.as_str());
                                } else {
                                    // the reason first, then the question the buttons answer
                                    ui.label(RichText::new(tr("GhostDesk에서 사용하는 기능이 현재 설치되어 있는 AI 엔진(pi)의 버전에서 지원되지 않아 에이전트가 제대로 작동하지 않을 수 있어요.")).color(theme::text()));
                                    ui.label(RichText::new(tr("업데이트를 진행하시겠어요?")).strong().color(theme::text()));
                                }
                                ui.horizontal(|ui| {
                                    start = theme::primary(ui, if failed.is_some() { tr("다시 시도") } else { tr("엔진 업데이트") })
                                        .on_hover_text(tr("pi 를 최신 버전으로 바꿔요. 내 pi 확장·설정은 건드리지 않아요."))
                                        .clicked();
                                    later = ui.link(theme::hint(tr("나중에"))).on_hover_text(tr("다음에 켤 때 다시 알려 드려요")).clicked();
                                });
                            }
                        }
                    });
            });
        if start {
            self.pi_update_result = None;
            let (tx, rx) = std::sync::mpsc::channel();
            self.pi_update = Some(rx);
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(crate::pi::update());
                ctx.request_repaint();
            });
        }
        if later {
            self.pi_update_dismissed = true;
        }
        if close {
            self.pi_update_result = None;
        }
    }

    /// Short-lived toast above every window. Rises slightly into view and fades out at the end.
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
                // every frame while appearing or disappearing; in between, repaint when the fade-out starts
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
        // clean up every isolated screen on exit: take them out of the lock first (never block_on while
        // holding it), close them all concurrently, with one overall timeout
        let mut desks = Vec::new();
        for a in self.agents.drain(..) {
            a.shared.removed.store(true, Ordering::SeqCst);
            if let Some(ssh) = a.shared.ssh.lock().as_ref() { ssh.close(); }
            a.task.abort();
            let ws = a.shared.workspace.lock().take();
            if let Some(ws) = ws {
                desks.push(ws);
            }
        }
        if desks.is_empty() {
            return;
        }
        // the timeout (Sleep) must be created inside the runtime: created outside block_on (on the main
        // thread) it panics with "there is no reactor running" and exits without cleaning up
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

#[cfg(windows)]
fn approval_show_mode(minimized: bool) -> i32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_SHOWNOACTIVATE, SW_SHOWNA};
    if minimized { SW_SHOWNOACTIVATE } else { SW_SHOWNA }
}

/// Started by an agent's connection (`--background`): keep the window out of the person's way, in the taskbar,
/// instead of opening over what they're doing. eframe shows the window itself after the first frame, and that
/// show undoes an earlier minimize; window styles (a transparent window) are reset by it too. So the window is
/// cloaked by DWM (never drawn, whatever its style) until eframe has shown it, then minimized without
/// activating, given a centered restore position and uncloaked. Called from `new` (before any frame) and every
/// frame until it returns true.
#[cfg(windows)]
fn park_background_window() -> bool {
    use crate::platform::win;
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CLOAK};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, GetWindowPlacement, SetWindowPlacement, SM_CXSCREEN, SM_CYSCREEN, SW_SHOWMINNOACTIVE, WINDOWPLACEMENT,
    };
    let me = std::process::id();
    let Some(w) = win::top_level_hwnds()
        .into_iter()
        .filter(|h| win::window_pid(*h) == me)
        .map(win::window_info)
        .find(|w| w.title == "GhostDesk")
    else {
        return false;
    };
    let h = w.hwnd as _;
    let cloak = |on: i32| unsafe {
        let _ = DwmSetWindowAttribute(h, DWMWA_CLOAK as u32, &on as *const i32 as *const _, 4);
    };
    if !w.visible {
        cloak(1);
        return false;
    }
    unsafe {
        let mut p: WINDOWPLACEMENT = std::mem::zeroed();
        p.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        if GetWindowPlacement(h, &mut p) != 0 {
            let r = p.rcNormalPosition;
            let (ww, wh) = (r.right - r.left, r.bottom - r.top);
            let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
            let (x, y) = (((sw - ww) / 2).max(0), ((sh - wh) / 2).max(0));
            p.rcNormalPosition = RECT { left: x, top: y, right: x + ww, bottom: y + wh };
            p.showCmd = SW_SHOWMINNOACTIVE as u32;
            SetWindowPlacement(h, &p);
        }
    }
    cloak(0);
    true
}

#[cfg(windows)]
fn show_approval_window() {
    use crate::platform::win;
    // egui's Minimized(false) uses SW_RESTORE, which activates the window on Windows.
    if let Some(w) = win::top_level_hwnds().into_iter().map(win::window_info)
        .find(|w| w.pid == std::process::id() && w.title == "GhostDesk" && (w.minimized || !w.visible)) {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::ShowWindowAsync(w.hwnd as _, approval_show_mode(w.minimized));
        }
    }
}

fn panel_sizes(width: f32) -> (f32, f32, f32) {
    if width < 1120.0 { (200.0, 320.0, 16.0) } else { (248.0, 360.0, 24.0) }
}

fn detail_panel_limit(available: f32) -> f32 {
    (available - 368.0).clamp(320.0, 520.0)
}

/// Side panel (default, max) width for an agent-screen layout: 0 screen first, 1 balanced, 2 chat first.
fn detail_panel_widths(layout: u8, room: f32, base: f32) -> (f32, f32) {
    let wide_max = (room - 280.0).max(320.0);
    match layout {
        1 => ((room * 0.5).clamp(320.0, wide_max), wide_max),
        2 => ((room * 0.72).clamp(320.0, wide_max), wide_max),
        _ => (base, detail_panel_limit(room).max(base.min(wide_max)).min(wide_max.max(320.0))),
    }
}

/// How long a toast stays visible (seconds).
const TOAST_SECS: f64 = 4.0;
/// How many recent tasks Home offers again.
const RECENT_TASKS: usize = 5;

/// A value filled in the background (`None` = still checking).
type Slot<T> = Arc<parking_lot::Mutex<Option<T>>>;

/// Drops approval requests whose requester went away (timeout or disconnect).
fn prune_closed_approvals(list: &mut Vec<Approval>) {
    list.retain(|(_, _, r)| r.as_ref().is_some_and(|r| !r.is_closed()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn approval_restore_does_not_request_activation() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SW_SHOWNOACTIVATE, SW_SHOWNA};
        assert_eq!(approval_show_mode(true), SW_SHOWNOACTIVATE);
        assert_eq!(approval_show_mode(false), SW_SHOWNA);
    }

    #[test]
    fn detail_columns_leave_room_for_the_live_screen() {
        for width in [900.0, 1120.0, 1280.0, 1920.0] {
            let (side, detail, padding) = panel_sizes(width);
            assert!(side >= 200.0 && detail >= 320.0);
            assert!(width - side - detail - padding * 2.0 >= 320.0, "width={width}");
            let max_detail = detail_panel_limit(width - side);
            assert!(max_detail >= detail);
            assert!(width - side - max_detail - padding * 2.0 >= 320.0, "width={width}");
            let resized_side = if width < 1120.0 { side } else { 380.0 };
            assert!(width - resized_side - detail_panel_limit(width - resized_side) - padding * 2.0 >= 320.0, "resized width={width}");
        }
    }

    #[test]
    fn stale_approvals_are_dropped() {
        let (tx1, rx1) = tokio::sync::oneshot::channel::<bool>();
        let (tx2, rx2) = tokio::sync::oneshot::channel::<bool>();
        let mut list: Vec<Approval> = vec![("a".into(), "".into(), Some(tx1)), ("b".into(), "".into(), Some(tx2)), ("c".into(), "".into(), None)];
        drop(rx1); // the requester gave up after a timeout
        prune_closed_approvals(&mut list);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "b");
        drop(rx2);
    }
}

#[cfg(test)]
mod icon_tests {
    #[test]
    fn window_icon_decodes() {
        let i = super::window_icon();
        assert_eq!((i.width, i.height), (256, 256));
        assert_eq!(i.rgba.len(), 256 * 256 * 4);
    }

    /// The Windows icon resource is well formed: an empty first entry, PNG icons, and one icon
    /// group whose entries point at them (windows/ghostdesk.res, linked by build.rs).
    #[test]
    fn windows_icon_resource_is_well_formed() {
        let d = include_bytes!("../windows/ghostdesk.res");
        let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap()) as usize;
        let u16_at = |o: usize| u16::from_le_bytes(d[o..o + 2].try_into().unwrap()) as usize;
        let (mut o, mut icons, mut group) = (0, Vec::new(), None);
        while o < d.len() {
            let (size, header) = (u32_at(o), u32_at(o + 4));
            let (typ, name) = (u16_at(o + 10), u16_at(o + 14));
            let data = &d[o + header..o + header + size];
            match typ {
                3 => {
                    assert!(data.starts_with(b"\x89PNG"));
                    icons.push(name);
                }
                14 => group = Some(data.to_vec()),
                _ => assert_eq!(size, 0),
            }
            o = (o + header + size + 3) & !3;
        }
        let g = group.expect("icon group");
        let count = u16::from_le_bytes([g[4], g[5]]) as usize;
        assert_eq!(count, icons.len());
        for i in 0..count {
            let id = u16::from_le_bytes([g[6 + i * 14 + 12], g[6 + i * 14 + 13]]) as usize;
            assert!(icons.contains(&id), "{id}");
        }
    }
}

