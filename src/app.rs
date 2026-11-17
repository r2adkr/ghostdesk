//! The egui UI: creating agents, the live multi-agent grid, and the detail view (log,
//! instructions, takeover).

use std::sync::{atomic::Ordering, Arc};

use eframe::egui::{self, Color32, Margin, RichText, Stroke, StrokeKind, Vec2};
use tokio::{runtime::Handle, sync::mpsc};

use crate::{
    icon,
    agent::{self, AgentCmd, AgentConfig, AgentShared, AgentStatus, DeskTarget, LogKind, ProfileSource, RemoteTarget},
    browser::MouseButton,
    llm::{Provider, EFFORTS},
    platform::{caps, text},
    profiles::{self, InstalledBrowser},
};

mod connect;
mod detail;
mod fonts;
mod ai;
mod i18n;
mod labels;
mod pip;
mod prefs;
mod screens;
mod settings;
mod sidebar;
mod theme;
mod titlebar;
mod tour;
mod wizard;

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
    Loading,
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
    pip: bool,
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
    /// Screen wall: every agent's screen filling the window (no list or panels).
    Wall,
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

struct NewAgentForm {
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
    /// Guide screenshot capture mode (`GHOSTDESK_TOUR_CAPTURE`).
    tour_capture: Option<tour::Capture>,
    /// macOS permission state (rechecked every second: `permissions`).
    #[cfg(target_os = "macos")]
    perm_cache: Option<(std::time::Instant, crate::macos::TccStatus)>,
    /// Whether the sidebar permission notice was dismissed for this run.
    perm_notice_hidden: bool,
    /// Fields in prefs.json this version doesn't know (written back unchanged on save).
    prefs_other: serde_json::Map<String, serde_json::Value>,
    /// Keeps the Mac/PC from sleeping while agents work, even with the screen locked (display off).
    keep_awake: bool,
    /// Whether sleep is being blocked right now (`keep_awake` and an agent working).
    awake: Option<crate::platform::awake::KeepAwake>,
    /// Home screen greeting (different each launch).
    greeting: usize,
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
    show_session_guide: bool,
    /// Windows child sessions: whether the feature is on (`None` = not checked yet) / enable request
    /// progress and result.
    child_enabled: Option<bool>,
    child_enable: Arc<parking_lot::Mutex<ChildEnable>>,
    /// Development: with GHOSTDESK_DEMO=<n>, auto-starts n empty-profile browser agents.
    pending_demo: u32,
    toast: Option<(String, f64)>,
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
            settings_tab: Default::default(),
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
            keep_awake: true,
            tour: None,
            tour_seen: false,
            tour_capture: tour::Capture::from_env(),
            #[cfg(target_os = "macos")]
            perm_cache: None,
            perm_notice_hidden: false,
            prefs_other: Default::default(),
            awake: None,
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
        fonts::setup_fonts(&cc.egui_ctx, app.lang);
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

impl eframe::App for GhostApp {
    /// Window clear color. The default (translucent black) shows through the 1 px gaps and
    /// separators between panels as black lines in the light theme.
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        theme::sync(&ctx);
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
                crate::control::ControlEvent::Approval { title, detail, reply } => {
                    self.approvals.push((title, detail, Some(reply)));
                    // flag it on the Dock icon (Windows: flash the taskbar button) so a window in the background
                    // notices too (without taking focus)
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
        self.update_keep_awake();
        self.tour_capture_tick(&ctx);

        // screen wall: only the screens, no other panels (back to home if every agent is gone)
        if self.view == View::Wall
            && (self.agents.is_empty()
                || (self.approvals.is_empty() && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))))
        {
            self.leave_wall(&ctx, View::Grid);
        }
        // the title bar must be placed before the side panels to span the full window width
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
    /// Normal layout: left sidebar · (right panel in detail view) · center · modal windows.
    fn main_layout(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let ctx = ctx.clone();
        // when the view (or selected agent) changes, the center and right panel fade in over 0.2 s
        let fade = theme::appear(&ctx, egui::Id::unique("view_in"), self.view_key(), 0.2);
        // with a title bar, reduce the top margin so content follows right below it. The macOS title bar
        // is shorter (28), so leave a bit more room.
        let top = if titlebar::visible(&ctx) { if cfg!(target_os = "macos") { 10 } else { 4 } } else { 14 };
        let side = egui::Panel::left("left")
            .resizable(true)
            .default_size(248.0)
            .min_size(200.0)
            .max_size(380.0)
            .frame(egui::Frame::new().fill(theme::surface()).inner_margin(Margin { left: 10, right: 10, top, bottom: 14 }))
            .show(ui, |ui| self.sidebar(ui));
        self.title_cols = titlebar::Columns { left: side.response.rect.width(), right: 0.0 };
        // detail view: right panel (status, controls, log, instructions) + live screen in the center
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

        // modal windows: with an approval request pending, only that shows (others are hidden for now,
        // typed text is kept)
        if self.approvals.is_empty() {
            self.settings_window(&ctx);
            self.connect_window(&ctx);
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
        };
        let agent = self.selected.and_then(|i| self.agents.get(i)).map_or(0, |a| a.shared.id);
        view << 48 | agent
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

/// How long a toast stays visible (seconds).
const TOAST_SECS: f64 = 4.0;

/// A value filled in the background (`None` = still checking).
type Slot<T> = Arc<parking_lot::Mutex<Option<T>>>;

/// Drops approval requests whose requester went away (timeout or disconnect).
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
        drop(rx1); // the requester gave up after a timeout
        prune_closed_approvals(&mut list);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "b");
        drop(rx2);
    }
}
