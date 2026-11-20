//! New-agent window: pick where to work (web, app, separate screen, remote) → settings → start.

use super::*;
use super::labels::profile_short;

impl NewAgentForm {
    pub(super) fn clear_vnc_authorization(&mut self) {
        self.remote_pass.clear();
        self.remote_dedicated = false;
    }

    pub(super) fn ssh_options(&self) -> Option<crate::ssh::SshOptions> {
        if self.remote_mode == RemoteMode::Vnc {
            return None;
        }
        let mut options = self.ssh.clone();
        if self.remote_mode == RemoteMode::Both && options.host.trim().is_empty() {
            options.host = self.remote_host.clone();
        }
        ssh_profiles::trim_options(&mut options);
        Some(options)
    }

    /// Defaults for a freshly opened new-agent window. `profile_idx` is the default browser profile
    /// to copy logins from.
    pub(super) fn new(profile_idx: Option<usize>) -> Self {
        NewAgentForm {
            team_id: None,
            team_role: String::new(),
            choice: None,
            app_search: String::new(),
            web_use_login: false,
            app_use_login: false,
            session_account: String::new(),
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
            host_code: None,
            saved_id: None,
            remote_mode: RemoteMode::Vnc,
            card_input: String::new(),
            card_invalid: false,
            remote_entry: RemoteEntry::Paste,
            ssh_profile: None,
            ssh: Default::default(),
            name: tr("에이전트 1").into(),
            workspace: "ws-1".into(),
            refresh: false,
            browser_idx: 0,
            profile_idx,
            start_url: "https://www.google.com".into(),
            task: String::new(),
            headless: true,
            size_idx: 0,
            max_steps: 60,
        }
    }
}

/// Runs a slow lookup in the background and fills the slot with the result (`None` = still
/// checking). Repaints when done.
pub(super) fn load_in_background<T: Send + 'static>(rt: &Handle, ctx: &egui::Context, f: impl FnOnce() -> T + Send + 'static) -> Slot<T> {
    let slot = Arc::new(parking_lot::Mutex::new(None));
    let (s, ctx) = (slot.clone(), ctx.clone());
    rt.spawn_blocking(move || {
        let v = f();
        *s.lock() = Some(v);
        ctx.request_repaint();
    });
    slot
}

/// The local app list `(name, path)`. macOS reads /Applications directly; on Windows installed
/// programs (registry, Start menu, …) can be slow, so they're read in the background.
pub(super) fn native_app_list(rt: &Handle, ctx: &egui::Context) -> Slot<Vec<(String, String)>> {
    #[cfg(target_os = "macos")]
    {
        let _ = (rt, ctx);
        Arc::new(parking_lot::Mutex::new(Some(list_mac_apps())))
    }
    #[cfg(not(target_os = "macos"))]
    {
        if caps::NATIVE_APPS {
            load_in_background(rt, ctx, crate::native::installed_apps)
        } else {
            Arc::new(parking_lot::Mutex::new(Some(Vec::new())))
        }
    }
}

impl GhostApp {
    /// Development: with GHOSTDESK_DEMO=<n>, auto-starts n browser agents.
    ///
    /// Demos never use user data: empty profile, blank page.
    pub(super) fn spawn_demos(&mut self, ctx: &egui::Context) {
        while self.pending_demo > 0 {
            self.pending_demo -= 1;
            self.form.kind = FormKind::Browser;
            self.form.profile_idx = None;
            self.form.start_url = "about:blank".into();
            // override with GHOSTDESK_DEMO_TASK when a plausible task is needed (e.g. the guide screenshots
            // in tour.rs)
            self.form.task = std::env::var("GHOSTDESK_DEMO_TASK").unwrap_or_else(|_| tr("데모 작업").into());
            self.spawn_agent(ctx);
        }
    }
}

pub(super) const SIZES: &[(u32, u32)] = &[(1280, 800), (1366, 768), (1440, 900), (1024, 768)];

/// Whether to show the local app card (OSes with hidden Electron launch or a regular app
/// backend).
pub(super) const LOCAL_APP_CARD: bool = caps::ELECTRON || caps::NATIVE_APPS;

/// One entry in the local app list (Mac apps / Windows apps).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct AppListItem {
    pub(super) label: String,
    pub(super) path: String,
    /// Index if this is an Electron app that can be launched hidden.
    pub(super) electron: Option<usize>,
    /// Why the entry can't be picked, if so (Electron apps that can't be launched hidden).
    pub(super) disabled: Option<String>,
}

/// Where auto mode decided to work.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum AutoPick {
    Web,
    App { label: String, path: String, electron: Option<usize> },
    /// An app that needs a full Windows screen instead of hidden-window control.
    ChildSession { label: String },
}

/// Windows 11 built-ins that are Store apps (`launch::PACKAGED_STUBS` and Notepad's execution
/// alias): refused on the hidden desktop and missing from the app list. `(Korean, English)`.
const STORE_APPS: &[(&str, &str)] =
    &[("메모장", "notepad"), ("계산기", "calculator"), ("그림판", "paint"), ("캡처 도구", "snipping tool"), ("워드패드", "wordpad")];

/// Whether `needle` occurs in `hay` with no ASCII letter or digit right before or after it.
fn word_at(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, m)| {
        let edge = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_alphanumeric());
        edge(hay[..i].chars().next_back()) && edge(hay[i + m.len()..].chars().next())
    })
}

/// The Store app named in the task, by its Korean name.
fn store_app(task: &str) -> Option<&'static str> {
    let t = task.to_lowercase();
    STORE_APPS.iter().find(|(ko, en)| t.contains(ko) || word_at(&t, en)).map(|(ko, _)| *ko)
}

/// The app on this computer named in the task; the longest name wins
/// when several match ('Visual Studio Code' over 'Code'). A URL (http, www) means web.
///
/// English names only match on word boundaries ('Notes' doesn't match 'footnotes'), and names
/// shorter than two characters are ignored. Built-in OS apps (Weather, Notes, Photos, News, …)
/// share names with common words, so they're only picked when the word "app" comes with them, as
/// in '날씨 앱' or 'Notes app' ("오늘 날씨 찾아줘" goes to the web).
fn auto_app<'a>(task: &str, items: &'a [AppListItem]) -> Option<&'a AppListItem> {
    let t = task.to_lowercase();
    if t.contains("http://") || t.contains("https://") || t.contains("www.") {
        return None;
    }
    items
        .iter()
        .filter_map(|it| {
            // match on both the display name (메모) and the file name (Notes)
            let file = std::path::Path::new(&it.path).file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
            let shown = it.label.trim().trim_end_matches(".app").to_lowercase();
            let system = is_system_app(&it.path);
            let hit = |name: &str| {
                name.chars().count() >= 2
                    && if system {
                        ["앱", " 앱", " app"].iter().any(|suffix| word_at(&t, &format!("{name}{suffix}")))
                    } else {
                        word_at(&t, name)
                    }
            };
            [shown, file].into_iter().filter(|n| hit(n)).map(|n| n.chars().count()).max().map(|n| (n, it))
        })
        .max_by_key(|(n, _)| *n)
        .map(|(_, it)| it)
}

/// Whether an app ships with the OS (macOS: /System/Applications; Windows: under the Windows
/// folder).
fn is_system_app(path: &str) -> bool {
    let p = path.to_lowercase().replace('\\', "/");
    p.starts_with("/system/") || p.contains("/windows/system32/") || p.contains(":/windows/")
}

impl GhostApp {
    pub(super) fn spawn_agent(&mut self, ctx: &egui::Context) {
        if let Some(msg) = self.missing_setup() {
            self.key_warn = true;
            if self.wizard_open {
                // inside the new-agent window, show the hint under the key field and focus it (the bottom toast
                // would overlap)
                self.wizard_key_field = true;
                self.focus_key = true;
                return;
            }
            return self.notify(ctx, msg);
        }
        self.key_warn = false;
        let ws_name = self.form.workspace.trim().to_string();
        if ws_name.is_empty() {
            return self.notify(ctx, tr("작업공간 이름을 입력하세요"));
        }
        if self.agents.iter().any(|a| profiles::slug(&a.shared.workspace_name) == profiles::slug(&ws_name)) {
            return self.notify(ctx, tr("같은 작업공간을 쓰는 에이전트가 이미 있습니다. 다른 이름을 쓰세요."));
        }
        // Windows can't use device names like CON, NUL, COM1 as folder names
        if cfg!(windows) && crate::platform::paths::is_reserved_name(&profiles::slug(&ws_name)) {
            return self.notify(ctx, trf("'{}' 은(는) Windows 에서 폴더 이름으로 쓸 수 없습니다. 다른 작업공간 이름을 쓰세요.", &[&ws_name]));
        }
        let target = match self.form.kind {
            FormKind::Browser => {
                let Some(browser) = self.browsers.get(self.form.browser_idx).cloned() else {
                    return self.notify(ctx, tr("Chromium 계열 브라우저(Chrome/Edge/Brave/Whale 등)를 찾지 못했습니다."));
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
                    return self.notify(ctx, unsupported(tr("Electron 앱 숨김 실행")));
                }
                let apps = self.electron_apps.lock().clone().unwrap_or_default();
                let Some(app) = self.form.electron_idx.and_then(|i| apps.get(i)).cloned() else {
                    return self.notify(ctx, tr("Electron 앱을 선택하세요"));
                };
                if !app.hideable {
                    return self.notify(ctx, trf("{} 은(는) 지원하지 않습니다: {}", &[&app.name, &app.reason.clone().unwrap_or_default()]));
                }
                let profile = if self.form.electron_clone {
                    let Some(src) = app.user_data_guess.clone() else {
                        return self.notify(ctx, tr("이 앱의 원본 데이터 폴더를 찾지 못했습니다. 빈 프로필을 쓰세요."));
                    };
                    if !self.form.electron_clone_consent {
                        return self.notify(ctx, tr("앱 데이터(로그인 정보·쿠키) 복제에 동의해야 합니다."));
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
                    return self.notify(ctx, unsupported(tr(text::LOCAL_APPS)));
                }
                let app = self.form.native_app.trim().to_string();
                if app.is_empty() {
                    return self.notify(
                        ctx,
                        pick(tr("앱(.app 경로 또는 번들 id)을 입력하거나 고르세요"), tr("앱(실행 파일 .exe 경로 또는 이름)을 입력하거나 고르세요")),
                    );
                }
                #[cfg(target_os = "macos")]
                if let Err(e) = crate::macos::preflight() {
                    return self.notify(ctx, format!("{e:#}"));
                }
                // virtual display only where this OS supports it (on Windows the check box isn't even shown)
                if self.form.native_vdisplay && caps::VDISPLAY {
                    if !self.allow_experimental && !self.form.native_vdisplay_consent {
                        return self.notify(ctx, tr("가상 디스플레이 사용에 대한 안내를 확인하고 동의 체크를 켜세요"));
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
                    return self.notify(ctx, unsupported(tr("별도 계정 세션")));
                }
                // Windows child sessions use the same account, so no account name is needed
                // macOS: an empty name means "whichever account is free"; a named one is used as is. If the account
                // does not exist yet, offer to make it instead of failing later
                #[cfg(target_os = "macos")]
                let chosen = {
                    let typed = self.form.session_account.trim();
                    if typed.is_empty() { crate::agent_account::auto_pick() } else { typed.to_string() }
                };
                #[cfg(not(target_os = "macos"))]
                let chosen = self.form.session_account.trim().to_string();
                #[cfg(target_os = "macos")]
                {
                    let name = chosen.as_str();
                    if !crate::agent_account::valid_name(name) {
                        return self.notify(ctx, tr("계정 이름은 영문 소문자·숫자·'.'·'_'·'-' 만 쓸 수 있어요"));
                    }
                    if !crate::agent_account::exists(name) {
                        self.account_prompt = Some(account::AccountPrompt::ask(name));
                        return;
                    }
                }
                DeskTarget::Session { account: chosen }
            }
            FormKind::Remote if self.form.host_code.is_some() => {
                DeskTarget::Host { code: self.form.host_code.clone().unwrap_or_default() }
            }
            FormKind::Remote => {
                if self.form.remote_mode != RemoteMode::Ssh && self.form.remote_host.trim().is_empty() {
                    return self.notify(ctx, tr("원격 데스크톱 주소를 입력하세요"));
                }
                let ssh = self.form.ssh_options();
                if let Some(options) = &ssh {
                    if let Err(error) = options.validate() {
                        return self.notify(ctx, ssh_profiles::validation_message(error));
                    }
                }
                if self.form.remote_mode == RemoteMode::Ssh {
                    DeskTarget::Ssh(ssh.expect("SSH-only always configures SSH"))
                } else {
                    DeskTarget::Remote(RemoteTarget {
                        host: self.form.remote_host.trim().to_string(),
                        port: self.form.remote_port,
                        username: Some(self.form.remote_user.trim().to_string()),
                        password: Some(self.form.remote_pass.clone()),
                        dedicated_ack: self.form.remote_dedicated,
                        ssh,
                    })
                }
            }
        };
        let team = self.form.team_id.map(|id| agent::TeamAssignment {
            store: self.teams.clone(),
            id,
            role: self.form.team_role.trim().chars().take(40).collect(),
        });
        if team.as_ref().is_some_and(|team| self.teams.read_json(team.id, None).is_err()) {
            return self.notify(ctx, tr("선택한 팀을 찾을 수 없어요"));
        }
        let cfg = AgentConfig {
            team,
            name: self.form.name.clone(),
            workspace: ws_name.clone(),
            task: self.form.task.clone(),
            target,
            max_steps: self.form.max_steps,
            reply_lang: self.lang.english_name(),
            use_pi: self.use_pi && crate::pi::find().is_some(),
            // missing_setup was checked above, so a connection exists
            llm: self.cur().map(|c| c.llm(EFFORTS[self.effort_idx])).expect("missing_setup checked the connection"),
        };
        let id = self.next_id;
        self.next_id += 1;
        let shared = AgentShared::new(id, cfg.name.clone(), ws_name, ctx.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        let max_steps = cfg.max_steps;
        let task = self.rt.spawn(agent::run_agent(shared.clone(), cfg, rx));
        let mut handle = AgentHandle::new(&self.rt, shared, tx, task);
        handle.max_steps = max_steps;
        self.agents.push(handle);
        self.form.name = trf("에이전트 {}", &[&self.next_id]);
        self.form.workspace = format!("ws-{}", self.next_id);
        self.form.task.clear();
    }

    pub(super) fn open_wizard(&mut self) {
        self.wizard_open = true;
        self.form.clear_vnc_authorization();
        self.form.card_input.clear();
        self.form.card_invalid = false;
        self.form.remote_entry = RemoteEntry::Paste;
        self.form.host_code = None;
        self.form.saved_id = None;
        self.form.team_id = None;
        self.form.team_role.clear();
        // auto comes preselected: just write the task and start (the window height is the same from the
        // start, so its position doesn't jump)
        self.form.choice = Some(Choice::Auto);
        self.key_warn = false;
        self.wizard_key_field = self.missing_setup().is_some();
    }

    fn team_picker(&mut self, ui: &mut egui::Ui) {
        self.teams_ui_active();
        let teams = self.teams_ui.cache();
        if teams.is_empty() && self.form.team_id.is_none() {
            return;
        }
        let selected = self.form.team_id.map_or(tr("팀 없이 시작"), |id| {
            teams.iter().find(|team| team.id == id).map_or(tr("선택한 팀을 찾을 수 없어요"), |team| team.name.as_str())
        });
        ui.horizontal_wrapped(|ui| {
            theme::section(ui, tr("팀"));
            egui::ComboBox::from_id_salt("agent_team")
                .width(200.0)
                .selected_text(crate::llm::short(selected, 32))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.form.team_id, None, tr("팀 없이 시작"));
                    for team in teams {
                        ui.selectable_value(&mut self.form.team_id, Some(team.id), &team.name);
                    }
                });
            if self.form.team_id.is_some() {
                ui.add(field(&mut self.form.team_role).desired_width(160.0).char_limit(40).hint_text(tr("역할 (선택)")));
            }
        });
        ui.add_space(12.0);
    }

    /// Windows child sessions feature state plus an enable (admin approval) button.
    #[cfg(windows)]
    pub(super) fn child_session_status(&mut self, ui: &mut egui::Ui) {
        let req = self.child_enable.lock().clone();
        if matches!(req, ChildEnable::Done(_)) && self.child_enabled != Some(true) {
            self.child_enabled = None; // recheck once the request is done
        }
        let on = *self.child_enabled.get_or_insert_with(crate::winapp::child::enabled);
        if on {
            ui.label(RichText::new(trf("{}  자식 세션 기능이 켜져 있습니다.", &[&icon::CIRCLE_CHECK])).color(theme::body()));
            return;
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("자식 세션 기능이 꺼져 있습니다.")).color(theme::danger()));
            let waiting = req == ChildEnable::Waiting;
            if ui.add_enabled(!waiting, egui::Button::new(trf("{}  자식 세션 켜기 (관리자 승인)", &[&icon::LOCK_OPEN]))).clicked() {
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
                theme::spinner(ui, 14.0, theme::muted());
            }
        });
        if let ChildEnable::Done(Err(e)) = &req {
            ui.label(RichText::new(e).color(theme::danger()));
        }
    }

    /// First-time setup guide for the separate account session (RDP-style).
    pub(super) fn session_guide_window(&mut self, ctx: &egui::Context) {
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
            close |= theme::modal_header(ui, pick(tr("별도 계정 세션 설정"), tr("자식 세션(PiP) 설정")));
            theme::modal_scroll(ui, max_h, |ui| {
                ui.label(crate::session::SETUP_GUIDE_KO);
                #[cfg(unix)]
                ui.add_space(6.0);
                #[cfg(unix)]
                ui.horizontal(|ui| {
                    ui.strong(tr("LaunchAgent plist (에이전트 계정의 ~/Library/LaunchAgents 에 저장)"));
                });
                #[cfg(unix)]
                {
                    let plist = crate::session::launch_agent_plist(exe, uid);
                    if ui.small_button(trf("{}  복사", &[&icon::COPY])).clicked() {
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

    /// ＋ New agent: pick where to work and write the task. Everything else is folded into
    /// advanced settings.
    pub(super) fn new_agent_window(&mut self, ctx: &egui::Context) {
        if !self.wizard_open {
            return;
        }
        const W: f32 = 600.0;
        let max_h = (ctx.content_rect().height() - 200.0).max(260.0);
        let mut close = false;
        let mut start = false;
        let mut go_connect = false;
        let kinds = kinds();
        let m = egui::Modal::new(egui::Id::unique("new_agent")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(W);
            close |= theme::modal_header(ui, tr("새 에이전트"));
            theme::modal_scroll(ui, max_h, |ui| {
                ui.set_width(W);
                theme::section(ui, tr("할 일"));
                ui.add_space(4.0);
                let task_hint = if self.form.choice == Some(Choice::Remote) && self.form.remote_mode == RemoteMode::Ssh {
                    tr("예) 서버에서 테스트를 실행하고 결과 파일을 받아줘")
                } else {
                    tr("예) 네이버에서 오늘 날씨를 찾아 요약해줘  (비워 두면 나중에 지시)")
                };
                ui.add(
                    egui::TextEdit::multiline(&mut self.form.task)
                        .desired_rows(2)
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(10, 8))
                        .hint_text(task_hint),
                );
                ui.add_space(16.0);
                theme::section(ui, tr("어디서 일할까요"));
                ui.add_space(4.0);
                let gap = 8.0;
                let widest = kinds.iter().map(|k| ui.painter().layout_no_wrap(
                    k.title.to_owned(), egui::FontId::proportional(14.0), theme::text(),
                ).size().x).fold(0.0f32, f32::max);
                let fit = (((W + gap) / (widest + 36.0 + gap)).floor() as usize).clamp(1, kinds.len());
                // spread over the rows it needs (3 + 2, not 4 + 1)
                let cols = kinds.len().div_ceil(kinds.len().div_ceil(fit));
                let tw = ((W - gap * (cols - 1) as f32) / cols as f32).floor();
                for row in kinds.chunks(cols) {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        for k in row {
                            if theme::tile(ui, self.form.choice == Some(k.choice), k.icon, k.title, Vec2::new(tw, 42.0)).clicked() {
                                self.form.choice = Some(k.choice);
                            }
                        }
                    });
                }
                ui.add_space(6.0);
                // a single line describing the chosen place (not one per card)
                let Some(choice) = self.form.choice else {
                    ui.label(theme::hint(tr("에이전트가 일할 곳을 하나 골라 주세요")));
                    return;
                };
                // (Auto explains itself in the line below)
                if choice != Choice::Auto {
                    if let Some(k) = kinds.iter().find(|k| k.choice == choice) {
                        ui.label(theme::hint(k.about));
                    }
                }
                ui.add_space(14.0);
                match choice {
                    Choice::Auto => self.simple_auto(ui),
                    Choice::Web => self.simple_web(ui),
                    Choice::App => self.simple_app(ui),
                    Choice::Remote => self.form_remote(ui),
                    Choice::Session => self.form_session(ui),
                }
                ui.add_space(18.0);
                self.team_picker(ui);
                ui.add_space(10.0);
                egui::CollapsingHeader::new(tr("고급 설정")).default_open(false).show(ui, |ui| self.advanced(ui, choice));
                if self.wizard_key_field {
                    ui.add_space(18.0);
                    theme::section(ui, tr("AI 와 API 키"));
                    ui.add_space(4.0);
                    // fill in the current connection's missing URL or key right here (other AIs are picked in
                    // Settings or the home model button)
                    if self.key_warn && self.missing_setup().is_some() {
                        ui.visuals_mut().widgets.inactive.bg_stroke = Stroke::new(1.0, theme::warn());
                    }
                    let focus = std::mem::take(&mut self.focus_key);
                    if self.setup_fields(ui, W, focus) {
                        self.key_warn = false;
                    }
                    ui.horizontal(|ui| {
                        if ui
                            .link(tr("키 없이 쓰기"))
                            .on_hover_text(tr("Claude 앱·Claude Code 같은 AI 앱에 연결하면 키가 필요 없어요"))
                            .clicked()
                        {
                            go_connect = true;
                        }
                    });
                    self.key_help(ui);
                    if self.key_warn {
                        if let Some(msg) = self.missing_setup() {
                            ui.label(RichText::new(msg).size(12.0).color(theme::warn()));
                        }
                    }
                }
            });
            ui.add_space(16.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ready = self.form.choice.is_some() && (self.form.choice != Some(Choice::Remote) || !matches!(self.form.remote_entry, RemoteEntry::Paste | RemoteEntry::Saved));
                if ui.add_enabled_ui(ready, |ui| theme::primary(ui, tr("에이전트 시작"))).inner.clicked() {
                    start = true;
                }
                if theme::secondary(ui, tr("취소")).clicked() {
                    close = true;
                }
            });
        });
        // clicking outside doesn't close it (so typed text isn't lost by accident); Esc, ✕ and Cancel do
        let esc = m.is_top_modal && !m.any_popup_open && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        if start {
            // auto: the AI reads the task and picks where to work (the keyword guess is the fallback)
            if self.form.choice == Some(Choice::Auto) && self.begin_route(ctx) {
                self.wizard_open = false;
                self.notify(ctx, tr("어디서 일할지 정하는 중…"));
            } else {
                self.start_from_wizard(ctx);
            }
        }
        if close || esc {
            self.wizard_open = false;
        }
        if go_connect {
            self.wizard_open = false;
            self.open_connect(ctx);
        }
    }

    /// Auto: previews where the current task will run (the same `auto_pick` is used at start).
    fn simple_auto(&mut self, ui: &mut egui::Ui) {
        let line = match self.auto_pick() {
            AutoPick::App { label, .. } => trf("→ '{}' 앱에서 일해요", &[&label]),
            AutoPick::ChildSession { label } => trf("→ '{}' 앱은 별도 Windows 화면에서 실행해요. 실제 계정·데이터를 사용해요.", &[&label]),
            AutoPick::Web if self.form.task.trim().is_empty() => tr("→ 할 일을 적으면 알맞은 곳을 골라요").to_string(),
            AutoPick::Web => tr("→ 웹사이트에서 일해요 (보이지 않는 브라우저)").to_string(),
        };
        ui.label(RichText::new(line).color(theme::text()));
    }

    /// Auto mode's decision: the app if the task names an app on this computer, otherwise the web.
    pub(super) fn auto_pick(&self) -> AutoPick {
        if !LOCAL_APP_CARD {
            return AutoPick::Web;
        }
        let electrons = self.electron_apps.lock().clone().unwrap_or_default();
        let natives = self.native_apps.lock().clone().unwrap_or_default();
        let items = app_list_items(&electrons, &natives, false);
        match auto_app(&self.form.task, &items) {
            Some(it) if it.disabled.is_some() && cfg!(windows) && caps::SESSION => AutoPick::ChildSession { label: it.label.clone() },
            Some(it) if it.disabled.is_some() => AutoPick::Web,
            Some(it) => AutoPick::App { label: it.label.clone(), path: it.path.clone(), electron: it.electron },
            None => match store_app(&self.form.task) {
                Some(label) if cfg!(windows) && caps::SESSION => AutoPick::ChildSession { label: tr(label).to_string() },
                _ => AutoPick::Web,
            },
        }
    }

    pub(super) fn simple_web(&mut self, ui: &mut egui::Ui) {
        if self.browsers.is_empty() {
            ui.colored_label(theme::danger(), tr("Chrome·Edge·Brave·Whale 같은 Chromium 계열 브라우저를 찾지 못했어요"));
            return;
        }
        egui::Grid::new("simple_web").num_columns(2).min_col_width(LABEL_W).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(tr("시작 주소"));
            ui.add(field(&mut self.form.start_url));
            ui.end_row();
        });
        ui.add_space(6.0);
        let bname = self.browsers.get(self.form.browser_idx).map(|b| b.name.clone()).unwrap_or_default();
        let profiles = self.browsers.get(self.form.browser_idx).map(|b| b.profiles.clone()).unwrap_or_default();
        // with no profile to clone the switch is off and disabled (so a disabled switch never holds true)
        if profiles.is_empty() {
            self.form.web_use_login = false;
        }
        ui.horizontal(|ui| {
            let r = ui
                .add_enabled(!profiles.is_empty(), theme::switch(&mut self.form.web_use_login, trf("내 {} 로그인 사용", &[&bname])))
                .on_hover_text(tr("내 브라우저의 로그인을 복사해서 써요. 원래 브라우저는 건드리지 않아요. 어떤 프로필을 복사할지는 고급 설정에서 골라요."));
            if r.changed() && self.form.web_use_login && self.form.profile_idx.is_none_or(|i| i >= profiles.len()) {
                self.form.profile_idx = Some(0);
            }
            // Keep the login choice beside its switch so the source account is visible before starting.
            if self.form.web_use_login && profiles.len() > 1 {
                let selected = self.form.profile_idx.and_then(|i| profiles.get(i)).map(|p| profile_short(&p.name)).unwrap_or_default();
                egui::ComboBox::from_id_salt("simple_profile").icon(theme::combo_icon).selected_text(selected).width(160.0).show_ui(ui, |ui| {
                    for (i, p) in profiles.iter().enumerate() {
                        ui.selectable_value(&mut self.form.profile_idx, Some(i), &p.name);
                    }
                });
            }
        });
    }

    /// App: pick from the list first; only options relevant to the chosen app show below it.
    pub(super) fn simple_app(&mut self, ui: &mut egui::Ui) {
        let electrons = self.electron_apps.lock().clone().unwrap_or_default();
        let current = cfg!(windows) && self.form.native_current_session;
        if current {
            self.form.native_attach = false;
            self.form.app_use_login = false;
            self.form.native_app = current_session_app_path(&electrons, &self.form.native_app);
        }
        ui.add(field(&mut self.form.app_search).hint_text(pick(tr("앱 검색 (예: Code, 텍스트 편집기, 카카오톡)"), tr("앱 검색 (예: Code, 카카오톡, Character Map)"))));
        ui.add_space(6.0);
        let q = self.form.app_search.to_lowercase();
        let natives = self.native_apps.lock().clone();
        let detecting = self.electron_apps.lock().is_none() || natives.is_none();
        let items = app_list_items(&electrons, &natives.unwrap_or_default(), current);
        // if Electron detection finished late, recheck whether the chosen app is Electron (so it doesn't
        // leak onto the native path)
        if self.form.electron_idx.is_none() {
            self.form.electron_idx = electrons.iter().position(|e| electron_matches(e, &self.form.native_app));
        }
        egui::Frame::new().fill(theme::bg()).stroke(Stroke::new(1.0, theme::border())).corner_radius(theme::CARD_RADIUS).inner_margin(Margin::same(6)).show(ui, |ui| {
            egui::ScrollArea::vertical().max_height(180.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.set_width(ui.available_width());
                // one-line list: left-aligned, each row clickable across its full width
                ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    let mut found = false;
                    for it in items.iter().filter(|it| q.is_empty() || it.label.to_lowercase().contains(&q) || it.path.to_lowercase().contains(&q)) {
                        found = true;
                        match &it.disabled {
                            // Electron apps that can't be launched hidden: listed but unpickable, with the reason
                            Some(reason) => {
                                ui.add_enabled(false, egui::Button::selectable(false, it.label.as_str())).on_disabled_hover_text(reason.as_str());
                            }
                            None => {
                                let sel = self.form.native_app == it.path;
                                if ui.add(egui::Button::selectable(sel, it.label.as_str()).min_size(Vec2::new(0.0, 28.0))).clicked() {
                                    self.form.native_app = it.path.clone();
                                    self.form.electron_idx = it.electron;
                                }
                            }
                        }
                    }
                    if !found && !detecting {
                        ui.label(theme::hint(tr("검색 결과가 없어요. 앱 이름을 줄여서 찾아보세요.")));
                        if !q.is_empty() && theme::secondary(ui, tr("검색 지우기")).clicked() {
                            self.form.app_search.clear();
                        }
                    }
                });
            });
        });
        if detecting {
            ui.horizontal(|ui| {
                theme::spinner(ui, 14.0, theme::muted());
                ui.label(theme::hint(pick(tr("Electron 앱을 확인하는 중…"), tr("설치된 앱을 확인하는 중…"))));
            });
        } else if self.form.native_app.is_empty() && items.iter().any(|it| it.electron.is_some()) {
            ui.label(theme::hint(trf("{} 표시 앱은 완전히 숨겨서 실행돼요", &[&icon::STAR])))
                .on_hover_text(pick(tr("나머지 앱은 창을 숨긴 채 조작해요"), tr("나머지 앱은 창을 화면 밖에 두고 조작해요")));
        }
        ui.add_space(8.0);
        // Windows: run as the current account on a hidden screen (child session). It changes the list
        // itself, so it shows before picking too.
        if cfg!(windows) {
            ui.add(theme::switch(&mut self.form.native_current_session, tr("숨은 화면에서 내 계정 그대로 실행")))
                .on_hover_text(tr("보이지 않는 별도 Windows 화면에 앱을 새로 띄워요. 앱의 실제 로그인과 데이터를 써요."));
            if !current && items.iter().any(|it| it.disabled.is_some()) {
                ui.label(theme::hint(tr("선택할 수 없는 앱은 위 옵션이나 '별도 세션'에서 실행을 시도할 수 있어요. 실제 계정·데이터를 사용해요.")));
            }
        }
        if current {
            self.form.electron_idx = None;
            theme::tinted(theme::warn()).show(ui, |ui| {
                ui.label(
                    tr("에이전트가 한 일은 실제 계정에 남아요. 시작 전에 그 앱을 완전히 꺼 주세요 (트레이 아이콘까지). \
                     Store 앱 · 관리자 권한 앱 · 일부 게임은 안 될 수 있어요."),
                );
            });
            return;
        }
        if self.form.native_app.is_empty() {
            return;
        }
        let electron = self.form.electron_idx.and_then(|i| electrons.get(i)).filter(|e| electron_matches(e, &self.form.native_app));
        match electron {
            Some(e) if !e.hideable => {
                ui.colored_label(theme::danger(), trf("{} 은(는) 숨김 실행을 지원하지 않아요: {}", &[&e.name, &e.reason.clone().unwrap_or_default()]));
            }
            Some(e) => {
                // with no original data folder the switch is off and disabled (so a disabled switch never holds
                // true)
                if e.user_data_guess.is_none() {
                    self.form.app_use_login = false;
                }
                ui.add_enabled(e.user_data_guess.is_some(), theme::switch(&mut self.form.app_use_login, trf("내 {} 로그인 복사해서 쓰기", &[&e.name])))
                    .on_hover_text(tr("앱 데이터를 작업공간으로 복사해서 써요. 원래 앱은 건드리지 않아요."));
            }
            None => {
                self.form.electron_idx = None;
                ui.add(theme::switch(&mut self.form.native_attach, tr("지금 켜 둔 내 앱 쓰기")))
                    .on_hover_text(tr("카카오톡처럼 한 번에 하나만 로그인되는 앱에 써요. 작업 중에는 그 창이 숨겨져요. 끄면 숨긴 앱을 새로 띄워요."));
                // virtual display (CGVirtualDisplay) is macOS-only: elsewhere the switch is hidden and off
                if caps::VDISPLAY {
                    ui.add(theme::switch(&mut self.form.native_vdisplay, trf("{}  가상 디스플레이에서 실행 (실험)", &[&icon::FLASK])))
                        .on_hover_text(tr("앱 창을 숨기는 대신 보이지 않는 가상 모니터 위에 띄워 드래그·우클릭 같은 실제 마우스 동작을 그대로 써요"));
                } else {
                    self.form.native_vdisplay = false;
                }
                if self.form.native_vdisplay {
                    theme::tinted(theme::warn()).show(ui, |ui| {
                        ui.label(RichText::new(tr("가상 모니터가 디스플레이 배치의 가장 바깥에 잠시 추가됩니다. 그동안 마우스 커서가 그쪽으로 넘어갈 수 있고, 설정에 따라 Dock·메뉴 막대가 그 화면에도 보일 수 있습니다. 작업이 끝나면 가상 모니터는 제거됩니다.")).small());
                        if self.allow_experimental {
                            ui.label(theme::hint(tr("설정에서 실험적 기능을 허용했습니다")));
                        } else {
                            ui.add(theme::switch(&mut self.form.native_vdisplay_consent, tr("이해했고 가상 모니터 추가에 동의합니다")));
                        }
                    });
                } else {
                    self.form.native_vdisplay_consent = false;
                }
                // macOS permission hints (Accessibility, Screen Recording) only on macOS; Windows has no such
                // consent
                #[cfg(target_os = "macos")]
                {
                    let t = crate::macos::tcc_status();
                    if !(t.accessibility && t.screen_capture) {
                        ui.horizontal_wrapped(|ui| {
                            ui.colored_label(theme::warn(), tr("이 방식은 macOS 의 '손쉬운 사용'과 '화면 기록' 권한이 필요해요"));
                            if ui.link(tr("권한 설정")).on_hover_text(tr("설정의 '권한'에서 하나씩 허용할 수 있어요")).clicked() {
                                // close the new-agent window so the two don't overlap (typed text is kept)
                                self.wizard_open = false;
                                self.open_settings(settings::SettingsTab::Permissions);
                            }
                        });
                    }
                }
                ui.label(theme::hint(pick(
                    tr("클릭은 버튼 누르기·입력칸 선택 같은 동작으로 바뀌어요 (드래그·우클릭 불가). 캡처 중 메뉴 막대에 화면 기록 표시가 나타나요."),
                    tr("일부 앱(게임 등)은 화면이 비어 보일 수 있어요"),
                )));
            }
        }
    }

    /// Turns a decision of auto mode into the form's choice.
    pub(super) fn apply_pick(&mut self, pick: AutoPick) {
        match pick {
            AutoPick::App { path, electron, .. } => {
                self.form.choice = Some(Choice::App);
                self.form.native_app = path;
                self.form.electron_idx = electron;
            }
            AutoPick::ChildSession { .. } => self.form.choice = Some(Choice::Session),
            AutoPick::Web => self.form.choice = Some(Choice::Web),
        }
    }

    pub(super) fn start_from_wizard(&mut self, ctx: &egui::Context) {
        if self.form.choice == Some(Choice::Auto) {
            let pick = self.auto_pick();
            self.apply_pick(pick);
        }
        match self.form.choice {
            Some(Choice::Web) => {
                self.form.kind = FormKind::Browser;
                let n = self.browsers.get(self.form.browser_idx).map(|b| b.profiles.len()).unwrap_or(0);
                // respect the profile chosen in advanced settings (first profile only if out of range)
                self.form.profile_idx = match (self.form.web_use_login && n > 0, self.form.profile_idx) {
                    (false, _) => None,
                    (true, Some(i)) if i < n => Some(i),
                    (true, _) => Some(0),
                };
            }
            Some(Choice::App) => {
                if self.form.native_app.trim().is_empty() {
                    return self.notify(ctx, tr("조작할 앱을 고르세요"));
                }
                if cfg!(windows) && self.form.native_current_session {
                    let electrons = self.electron_apps.lock().clone().unwrap_or_default();
                    self.form.native_app = current_session_app_path(&electrons, self.form.native_app.trim());
                    self.form.kind = FormKind::Native;
                    self.form.native_new_instance = true;
                    self.form.native_attach = false;
                    self.form.electron_idx = None;
                } else {
                    // Electron apps are decided purely by detection: apps not yet detected, or that can't be
                    // launched hidden, are never quietly run the native (hidden window) way
                    let Some(electrons) = self.electron_apps.lock().clone() else {
                        return self.notify(ctx, tr("앱 목록을 확인하는 중입니다. 잠시 후 다시 시작하세요."));
                    };
                    let found = electrons.iter().position(|e| electron_matches(e, self.form.native_app.trim()));
                    if let Some(i) = found {
                        let e = &electrons[i];
                        if !e.hideable {
                            return self.notify(ctx, trf("{} 은(는) 숨김 실행을 지원하지 않습니다: {}", &[&e.name, &e.reason.clone().unwrap_or_default()]));
                        }
                        self.form.electron_idx = Some(i);
                    } else {
                        self.form.electron_idx = None;
                    }
                    if let Some(e) = self.form.electron_idx.and_then(|i| electrons.get(i)) {
                        self.form.kind = FormKind::Electron;
                        let clone = self.form.app_use_login && e.user_data_guess.is_some();
                        self.form.electron_clone = clone;
                        // the check box text says it clones app data, so checking it counts as consent
                        self.form.electron_clone_consent = clone;
                    } else {
                        self.form.kind = FormKind::Native;
                        self.form.native_new_instance = !self.form.native_attach;
                    }
                }
            }
            Some(Choice::Remote) => self.form.kind = FormKind::Remote,
            Some(Choice::Session) => self.form.kind = FormKind::Session,
            // auto was already turned into web or app above and never gets here
            Some(Choice::Auto) | None => return self.notify(ctx, tr("무엇을 조작할지 먼저 고르세요")),
        }
        let before = self.agents.len();
        self.spawn_agent(ctx);
        if self.agents.len() > before {
            // a remote connection used for the first time: offer to keep it
            if self.form.kind == FormKind::Remote {
                self.offer_save_remote();
            }
            self.wizard_open = false;
            self.form.web_use_login = false;
            self.form.app_use_login = false;
            self.form.native_current_session = false;
            self.selected = Some(self.agents.len() - 1);
        }
    }

    /// Remote computer: address (:port), user, password, plus the dedicated-machine confirmation.
    pub(super) fn form_remote(&mut self, ui: &mut egui::Ui) {
        if self.form.remote_entry != RemoteEntry::Imported {
            ui.horizontal(|ui| {
                if theme::tab(ui, self.form.remote_entry == RemoteEntry::Paste, tr("받은 연결 정보")).clicked() {
                    self.form.remote_entry = RemoteEntry::Paste;
                }
                if theme::tab(ui, self.form.remote_entry == RemoteEntry::Saved, tr("저장된 정보")).clicked() {
                    self.form.remote_entry = RemoteEntry::Saved;
                }
                if theme::tab(ui, self.form.remote_entry == RemoteEntry::Manual, tr("직접 설정")).clicked() {
                    self.form.remote_entry = RemoteEntry::Manual;
                }
            });
            ui.separator();
            ui.add_space(6.0);
        }
        match self.form.remote_entry {
            RemoteEntry::Paste => {
                ui.label(theme::hint(tr("상대방의 설정 → 원격 연결에서 복사한 정보를 붙여넣으세요.")));
                if !self.form.import_connection_info(ui) { return; }
                self.form.show_connection_summary(ui);
            }
            RemoteEntry::Saved => {
                self.saved_remotes_tab(ui);
                if self.form.remote_entry == RemoteEntry::Saved { return; }
                self.form.show_connection_summary(ui);
            }
            RemoteEntry::Manual => self.manual_connection_fields(ui),
            RemoteEntry::Imported => self.form.show_connection_summary(ui),
        }
        ui.add_space(6.0);
        if self.form.remote_mode != RemoteMode::Ssh { ui.add(theme::switch(&mut self.form.remote_dedicated, tr("에이전트 전용 머신입니다"))).on_hover_text(pick(
            tr("macOS 화면 공유에 접속할 때 필요합니다. \
             GhostDesk 는 Apple 의 세션 선택 단계를 구현하지 않아, 대상 Mac 의 콘솔 사용자와 다른 계정으로 접속하면 그 Mac 의 화면이 전환될 수 있습니다. 이 Mac 자신에는 항상 접속을 거부합니다."),
            tr("macOS 화면 공유나 이 PC 의 VNC 서버에 접속할 때 필요합니다. \
             GhostDesk 는 Apple 의 세션 선택 단계를 구현하지 않아, 대상 Mac 의 콘솔 사용자와 다른 계정으로 접속하면 그 Mac 의 화면이 전환될 수 있습니다. \
             또 이 PC 자신에서 실행 중인 VNC 서버는 지금 보고 계신 실제 화면일 수 있어, 확인 없이는 접속하지 않습니다. \
             WSL·Hyper-V 가상 머신처럼 이 PC 주소로 전달되는 별도 데스크톱일 때만 켜세요."),
        )); }
    }

    fn manual_connection_fields(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(tr("접속 방식"));
            ui.selectable_value(&mut self.form.remote_mode, RemoteMode::Vnc, tr("화면 (VNC)"));
            ui.selectable_value(&mut self.form.remote_mode, RemoteMode::Ssh, tr("명령·파일 (SSH)"));
            ui.selectable_value(&mut self.form.remote_mode, RemoteMode::Both, "VNC + SSH");
        });
        ui.add_space(6.0);
        if self.form.remote_mode != RemoteMode::Ssh {
            egui::Grid::new("form_remote").num_columns(2).min_col_width(LABEL_W).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label(tr("VNC 주소"));
                let endpoint_changed = ui.horizontal(|ui| {
                    let w = (ui.available_width() - 76.0).max(160.0);
                    let host = ui.add(field(&mut self.form.remote_host).hint_text(tr("예) 192.168.0.20 또는 vm.local")).desired_width(w));
                    ui.label(":");
                    let port = ui.add(egui::DragValue::new(&mut self.form.remote_port).range(1..=65535)).on_hover_text(tr("포트 (보통 5900)"));
                    host.changed() || port.changed()
                }).inner;
                ui.end_row();
                ui.label(tr("사용자"));
                let user = ui.add(field(&mut self.form.remote_user).hint_text(tr("필요할 때만 (macOS 화면 공유는 계정 이름)")));
                if endpoint_changed || user.changed() {
                    self.form.clear_vnc_authorization();
                }
                ui.end_row();
                ui.label(tr("비밀번호"));
                ui.add(field(&mut self.form.remote_pass).password(true));
                ui.end_row();
            });
            ui.add_space(8.0);
        }
        if self.form.remote_mode != RemoteMode::Vnc {
            ui.label(theme::hint(tr("SSH 키 또는 에이전트로 인증해요. 호스트 키를 터미널에서 먼저 확인하세요")));
            if self.ssh_profiles.is_empty() {
                ui.label(theme::hint(tr("설정 → 원격 연결에서 자주 쓰는 연결을 저장할 수 있어요")));
            } else {
                if self.form.ssh_profile.as_ref().and_then(|name| self.ssh_profiles.get(name)) != Some(&self.form.ssh) {
                    self.form.ssh_profile = None;
                }
                ui.horizontal(|ui| {
                    ui.label(tr("저장된 SSH 연결"));
                    let selected = self.form.ssh_profile.as_deref().unwrap_or(tr("직접 입력"));
                    egui::ComboBox::from_id_salt("remote_ssh_profile").width(208.0).truncate().selected_text(selected).show_ui(ui, |ui| {
                        if ui.selectable_label(self.form.ssh_profile.is_none(), tr("직접 입력")).clicked() {
                            self.form.ssh_profile = None;
                        }
                        for (name, options) in &self.ssh_profiles {
                            let choice = egui::Button::selectable(self.form.ssh_profile.as_ref() == Some(name), name).truncate();
                            if ui.add(choice).on_hover_text(format!("{name}\n{}", options.endpoint())).clicked() {
                                self.form.ssh_profile = Some(name.clone());
                                self.form.ssh = options.clone();
                            }
                        }
                    });
                });
            }
            let hint = if self.form.remote_mode == RemoteMode::Ssh { tr("예) 192.168.0.20 또는 vm.local") } else { tr("비워 두면 위 주소 사용") };
            ssh_profiles::fields(ui, &mut self.form.ssh, hint);
            ui.label(theme::hint(tr("요청한 결과 파일은 다운로드/GhostDesk 폴더에 저장해요")));
        }
    }

    /// Separate account session (macOS: dedicated account name) / child session (Windows: whether the
    /// feature is on), plus the first-time setup guide.
    pub(super) fn form_session(&mut self, ui: &mut egui::Ui) {
        if cfg!(target_os = "macos") {
            theme::section(ui, tr("전용 계정"));
            ui.add_space(4.0);
            self.account_tools(ui);
        }
        #[cfg(windows)]
        self.child_session_status(ui);
        ui.add_space(4.0);
        if ui.link(trf("{}  처음 설정 방법 보기", &[&icon::BOOK_OPEN])).clicked() {
            self.show_session_guide = true;
        }
    }

    /// Which dedicated account the session uses: "Auto" (the least busy one, the default) or one of the accounts on
    /// this Mac, picked from a list. New accounts get their names from GhostDesk, so nothing is typed.
    fn account_tools(&mut self, ui: &mut egui::Ui) {
        let all = crate::agent_account::all();
        // a remembered choice of an account that is gone goes back to auto
        if !self.form.session_account.is_empty() && !all.contains(&self.form.session_account) {
            self.form.session_account.clear();
        }
        ui.horizontal_wrapped(|ui| {
            if ui.selectable_label(self.form.session_account.is_empty(), tr("자동")).clicked() {
                self.form.session_account.clear();
            }
            for n in &all {
                if ui.selectable_label(self.form.session_account == *n, n).clicked() {
                    self.form.session_account = n.clone();
                }
            }
        });
        ui.horizontal(|ui| {
            let chosen = self.form.session_account.clone();
            if !chosen.is_empty() && theme::secondary(ui, tr("암호 보기")).clicked() {
                match account::AccountPrompt::show(&chosen) {
                    Some(p) => self.account_prompt = Some(p),
                    None => self.toast = Some((tr("저장된 암호가 없어요").to_string(), ui.input(|i| i.time) + TOAST_SECS)),
                }
            }
            if !chosen.is_empty() && cfg!(target_os = "macos") && theme::secondary(ui, tr("브라우저 로그인 가져오기")).clicked() {
                self.import_prompt = Some(account::ImportPrompt::new(&chosen));
            }
            if !chosen.is_empty() && cfg!(target_os = "macos") && self.setup_rx.is_none() && theme::secondary(ui, tr("세션 설정 적용")).clicked() {
                let (tx, rx) = std::sync::mpsc::channel();
                self.setup_rx = Some(rx);
                let name = chosen.clone();
                std::thread::spawn(move || {
                    let res = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string()).and_then(|rt| {
                        rt.block_on(crate::agent_account::setup_session(&name)).map_err(|e| e.to_string())
                    });
                    let _ = tx.send(res);
                });
            }
            if theme::secondary(ui, tr("새 계정 만들기")).clicked() {
                let name = crate::agent_account::next_name();
                self.account_prompt = Some(account::AccountPrompt::ask(&name));
                self.form.session_account = name;
            }
        });
        ui.label(theme::hint(if all.is_empty() {
            tr("아직 계정이 없어요. 시작하면 만들지 물어봐요")
        } else {
            tr("자동은 비어 있는 계정을 골라요. 계정이 여러 개면 에이전트가 나뉘어 일해요")
        }));
    }

    /// Advanced settings: name, workspace, max steps, plus details for the chosen kind (in one grid
    /// so the fields line up).
    pub(super) fn advanced(&mut self, ui: &mut egui::Ui, choice: Choice) {
        egui::Grid::new("adv").num_columns(2).min_col_width(LABEL_W).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(tr("이름"));
            ui.add(field(&mut self.form.name));
            ui.end_row();
            ui.label(tr("작업공간"));
            ui.add(field(&mut self.form.workspace)).on_hover_text(tr("에이전트마다 달라야 해요. 같은 이름을 쓰면 전에 복사한 로그인을 다시 써요."));
            ui.end_row();
            ui.label(tr("최대 단계")).on_hover_text(tr("에이전트가 내 확인 없이 할 수 있는 동작 수예요. 다 쓰면 멈추고 기다려요."));
            ui.horizontal(|ui| {
                let mut unlimited = self.form.max_steps == 0;
                if ui.add(theme::switch(&mut unlimited, tr("무제한"))).changed() {
                    self.form.max_steps = if unlimited { 0 } else { 60 };
                }
                if !unlimited {
                    ui.add(egui::DragValue::new(&mut self.form.max_steps).range(1..=1000));
                }
            });
            ui.end_row();
            match choice {
                Choice::Web | Choice::Auto => self.browser_rows(ui),
                Choice::App => {
                    ui.label(tr("직접 지정"));
                    ui.add(field(&mut self.form.native_app).hint_text(pick(tr("번들 id 또는 .app 경로"), tr("실행 파일(.exe) 경로 또는 프로그램 이름"))))
                        .on_hover_text(tr("목록에 없는 앱을 쓸 때만 적어요"));
                    ui.end_row();
                }
                Choice::Remote | Choice::Session => {}
            }
        });
    }

    /// Browser rows of the advanced settings: browser, profile to copy, recopy, screen size, hide
    /// window.
    fn browser_rows(&mut self, ui: &mut egui::Ui) {
        ui.label(tr("브라우저"));
        let bname = self.browsers.get(self.form.browser_idx).map(|b| b.name.clone()).unwrap_or(tr("(없음)").into());
        egui::ComboBox::from_id_salt("browser").icon(theme::combo_icon).selected_text(bname).show_ui(ui, |ui| {
            for (i, b) in self.browsers.iter().enumerate() {
                if ui.selectable_label(self.form.browser_idx == i, &b.name).clicked() {
                    self.form.browser_idx = i;
                    self.form.profile_idx = if b.profiles.is_empty() { None } else { Some(0) };
                }
            }
        });
        ui.end_row();

        ui.label(tr("프로필"));
        let profiles = self.browsers.get(self.form.browser_idx).map(|b| b.profiles.clone()).unwrap_or_default();
        // mirrors the "use login" switch above: picking a profile turns the switch on, picking "empty
        // profile" turns it off (neither overrides the other)
        let picked = if self.form.web_use_login { self.form.profile_idx.and_then(|i| profiles.get(i)) } else { None };
        let ptext = picked.map_or_else(|| tr("빈 프로필 (로그인 없음)").to_string(), |p| trf("복사: {}", &[&p.name]));
        egui::ComboBox::from_id_salt("profile").icon(theme::combo_icon).selected_text(ptext).width(220.0).show_ui(ui, |ui| {
            for (i, p) in profiles.iter().enumerate() {
                let sel = self.form.web_use_login && self.form.profile_idx == Some(i);
                if ui.selectable_label(sel, trf("복사: {}", &[&p.name])).clicked() {
                    self.form.profile_idx = Some(i);
                    self.form.web_use_login = true;
                }
            }
            if ui.selectable_label(!self.form.web_use_login, tr("빈 프로필 (로그인 없음)")).clicked() {
                self.form.web_use_login = false;
            }
        });
        ui.end_row();

        ui.label("");
        ui.add(theme::switch(&mut self.form.refresh, tr("원본에서 다시 복사 (로그인 새로 고침)")));
        ui.end_row();

        ui.label(tr("화면 크기"));
        let (w, h) = SIZES[self.form.size_idx];
        egui::ComboBox::from_id_salt("size").icon(theme::combo_icon).selected_text(format!("{w}x{h}")).show_ui(ui, |ui| {
            for (i, (w, h)) in SIZES.iter().enumerate() {
                ui.selectable_value(&mut self.form.size_idx, i, format!("{w}x{h}"));
            }
        });
        ui.end_row();

        ui.label("");
        ui.add(theme::switch(&mut self.form.headless, tr("브라우저 창 숨기기")))
            .on_hover_text(tr("끄면 실제 브라우저 창이 보여요. 그래도 내 마우스 · 키보드는 쓰지 않아요."));
        ui.end_row();
    }
}

/// Width of the label column in the new-agent window (same in every grid so rows line up).
const LABEL_W: f32 = 72.0;

/// Notice for a feature this OS doesn't support (the UI translation of `text::unsupported`).
fn unsupported(feature: &str) -> String {
    trf("{} 은(는) {} 에서 아직 지원하지 않습니다 (현재 macOS 전용)", &[&feature, &text::OS_NAME])
}

fn field(s: &mut String) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(s).desired_width(f32::INFINITY).margin(Margin::symmetric(10, 6))
}

/// One card under "Where should it work?".
pub(super) struct Kind {
    pub(super) choice: Choice,
    pub(super) icon: &'static str,
    pub(super) title: &'static str,
    /// One-line description shown below when picked.
    pub(super) about: &'static str,
}

/// Only places available on this OS (unsupported kinds are hidden; see `platform::caps`).
pub(super) fn kinds() -> Vec<Kind> {
    let mut v = vec![Kind {
        choice: Choice::Auto,
        icon: icon::STAR,
        title: tr("자동"),
        about: tr("할 일을 보고 웹사이트와 이 컴퓨터의 앱 중에서 알맞은 곳을 골라요"),
    }];
    v.push(Kind {
        choice: Choice::Web,
        icon: icon::GLOBE,
        title: tr("웹사이트"),
        about: tr("보이지 않는 브라우저에서 일해요. 내 화면은 그대로예요."),
    });
    if LOCAL_APP_CARD {
        v.push(Kind { choice: Choice::App, icon: text::APP_ICON, title: tr(text::LOCAL_APPS), about: tr("앱을 보이지 않게 띄워서 일해요. 내 화면은 그대로예요.") });
    }
    if caps::SESSION {
        v.push(Kind {
            choice: Choice::Session,
            icon: icon::USER,
            title: pick(tr("별도 계정 세션"), tr("자식 세션")),
            about: pick(
                tr("이 Mac 의 다른 사용자 계정 화면을 통째로 써요. 내 로그인은 없어요. 처음 한 번 설정이 필요해요."),
                tr("같은 PC 에 보이지 않는 Windows 화면을 하나 더 만들어 써요. 진행은 이 창에서 봐요."),
            ),
        });
    }
    v.push(Kind { choice: Choice::Remote, icon: icon::MONITOR, title: tr("원격 컴퓨터"), about: tr("다른 컴퓨터의 화면을 조작하거나 명령을 실행해요") });
    v
}

/// Compares app paths. Windows ignores case and separator differences (macOS compares exactly, as
/// before).
pub(super) fn same_app_path(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        let norm = |s: &str| s.trim().replace('/', "\\").trim_end_matches('\\').to_lowercase();
        norm(a) == norm(b)
    } else {
        a == b
    }
}

/// Whether the chosen path is this Electron app (macOS: app bundle path; Windows: install folder
/// or executable path).
pub(super) fn electron_matches(e: &crate::electron::ElectronApp, path: &str) -> bool {
    same_app_path(&e.bundle.display().to_string(), path) || (cfg!(windows) && same_app_path(&e.exe.display().to_string(), path))
}

/// Path to run in the child session ("current account" mode): the executable if the chosen app
/// is Electron, otherwise the chosen path as is.
pub(super) fn current_session_app_path(electrons: &[crate::electron::ElectronApp], selected: &str) -> String {
    electrons.iter().find(|e| electron_matches(e, selected))
        .map(|e| e.exe.display().to_string()).unwrap_or_else(|| selected.to_string())
}

/// Builds one list from Electron apps (starred if launchable hidden, otherwise disabled with a
/// reason) and the other local apps.
///
/// Detected Electron apps aren't added again as native entries (so they never leak onto the
/// native path). On Windows an installed program whose executable path matches an Electron app's
/// executable counts as the same app.
pub(super) fn app_list_items(electrons: &[crate::electron::ElectronApp], native_apps: &[(String, String)], current_session: bool) -> Vec<AppListItem> {
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
            let reason = e.reason.clone().filter(|r| !r.trim().is_empty()).unwrap_or_else(|| tr("숨김 실행을 지원하지 않습니다").into());
            items.push(AppListItem {
                label: trf("{} — 숨김 실행 불가", &[&e.name]),
                path,
                electron: None,
                disabled: Some(trf("{} 은(는) 숨김 실행을 지원하지 않습니다: {}", &[&e.name, &reason])),
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
    // pickable apps first, apps that can't be launched hidden last (order otherwise kept)
    items.sort_by_key(|it| it.disabled.is_some());
    items
}

/// Apps in /Applications and /System/Applications `(name, path)`.
#[cfg(target_os = "macos")]
/// Names are shown in the user's language like Finder does ('메모' instead of 'Notes'). The
/// English file name stays in the path for search and auto mode.
pub(super) fn list_mac_apps() -> Vec<(String, String)> {
    use objc2_foundation::{NSFileManager, NSString};
    let fm = NSFileManager::defaultManager();
    let mut v = Vec::new();
    for dir in ["/Applications", "/System/Applications", "/System/Applications/Utilities"] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "app").unwrap_or(false) {
                    let path = p.display().to_string();
                    let shown = fm.displayNameAtPath(&NSString::from_str(&path)).to_string();
                    let name = match shown.trim().trim_end_matches(".app") {
                        "" => p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                        n => n.to_string(),
                    };
                    v.push((name, path));
                }
            }
        }
    }
    v.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn switching_remote_modes_resolves_only_the_current_vnc_host() {
        let mut form = NewAgentForm::new(None);
        form.remote_host = "old".into();
        assert!(form.ssh_options().is_none());
        form.remote_mode = RemoteMode::Both;
        assert_eq!(form.ssh_options().unwrap().host, "old");
        form.remote_mode = RemoteMode::Ssh;
        assert!(form.ssh_options().unwrap().validate().is_err());
        form.remote_host = "new".into();
        form.remote_mode = RemoteMode::Both;
        assert_eq!(form.ssh_options().unwrap().host, "new");
        form.ssh.host = "explicit".into();
        assert_eq!(form.ssh_options().unwrap().host, "explicit");
    }

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
        // Electron apps that can't be launched hidden don't vanish: they show disabled with a reason and
        // aren't re-added as native entries
        let discord: Vec<_> = items.iter().filter(|i| i.path.ends_with("Discord.app")).collect();
        assert_eq!(discord.len(), 1);
        assert!(discord[0].disabled.as_deref().unwrap().contains("퓨즈가 꺼져 있음"));
        assert_eq!(discord[0].electron, None);
        let te = items.iter().find(|i| i.path.ends_with("TextEdit.app")).unwrap();
        assert!(te.disabled.is_none() && te.electron.is_none());
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
        // typing the executable path directly is also recognized as the Electron app
        assert!(electron_matches(&electrons[0], r"C:\Users\u\AppData\Local\Programs\Microsoft VS Code\Code.exe"));
        assert!(electron_matches(&electrons[0], r"c:/users/u/appdata/local/programs/microsoft vs code/"));
        assert!(!electron_matches(&electrons[0], r"C:\Windows\notepad.exe"));
    }

    #[test]
    fn unsupported_cards_follow_caps() {
        // macOS has every card; Windows has no virtual display (separate session = child session)
        if cfg!(target_os = "macos") {
            assert!(LOCAL_APP_CARD && caps::SESSION && caps::VDISPLAY);
            assert_eq!(text::PRIMARY_MOD, "cmd");
        }
        if cfg!(windows) {
            assert!(LOCAL_APP_CARD && caps::SESSION && !caps::VDISPLAY);
            assert_eq!(text::LOCAL_APPS, "Windows 앱");
            // takeover copy/cut uses ctrl on Windows
            assert_eq!(format!("{}+c", text::PRIMARY_MOD), "ctrl+c");
        }
        assert_eq!(pick("mac", "other"), if cfg!(target_os = "macos") { "mac" } else { "other" });
        // the new-agent window's cards follow the same caps: web first, remote last
        let k = kinds();
        assert_eq!(k.len(), 3 + LOCAL_APP_CARD as usize + caps::SESSION as usize);
        assert!(k[0].choice == Choice::Auto && k[1].choice == Choice::Web && k.last().unwrap().choice == Choice::Remote);
    }

    #[test]
    fn auto_mode_finds_the_app_named_in_the_task() {
        let app = |label: &str, disabled: bool| AppListItem {
            label: label.into(),
            path: format!("/Applications/{label}.app"),
            electron: None,
            disabled: disabled.then(|| "x".to_string()),
        };
        let items = vec![app("Code", false), app("Visual Studio Code", false), app("카카오톡", false), app("Notes", false), app("Slack", true)];
        let pick = |t: &str| auto_app(t, &items).map(|it| it.label.as_str());
        assert_eq!(pick("카카오톡으로 엄마한테 안부 보내줘"), Some("카카오톡"));
        assert_eq!(pick("Open Visual Studio Code and run the tests"), Some("Visual Studio Code"), "긴 이름 우선");
        assert_eq!(pick("notes 앱에 장보기 목록 적어줘"), Some("Notes"));
        assert_eq!(pick("Write footnotes for the paper"), None, "낱말 안의 일부는 아님");
        assert_eq!(pick("슬랙 대신 Slack 에 올려줘"), Some("Slack"), "지원하지 않는 앱도 먼저 식별해서 다른 실행 경로를 안내");
        assert_eq!(pick("https://code.org 에서 강의 찾아줘"), None, "주소가 있으면 웹");
        assert_eq!(pick("네이버에서 오늘 날씨 찾아줘"), None);

        // built-in apps only when "app" is said along with them
        let sys = |label: &str| AppListItem { label: label.into(), path: format!("/System/Applications/{label}.app"), electron: None, disabled: None };
        let items = vec![sys("날씨"), sys("메모"), sys("Notes"), AppListItem { label: "계산기".into(), path: "/System/Applications/Calculator.app".into(), electron: None, disabled: None }];
        let pick = |t: &str| auto_app(t, &items).map(|it| it.label.as_str());
        assert_eq!(pick("오늘 날씨 찾아줘"), None);
        assert_eq!(pick("날씨 앱에서 서울 날씨 봐줘"), Some("날씨"));
        assert_eq!(pick("메모앱에 장보기 목록 적어줘"), Some("메모"));
        assert_eq!(pick("open the Notes app"), Some("Notes"));
        // found by the English file name even when the display name is Korean
        assert_eq!(pick("open the Calculator app"), Some("계산기"));
        assert!(is_system_app(r"C:\Windows\System32\notepad.exe") && !is_system_app(r"C:\Program Files\Slack\slack.exe"));

        // Store apps aren't in the app list; auto mode sends them to a child session instead of the web
        assert!(auto_app("메모장을 이용해서 소설을 써줘", &items).is_none());
        assert_eq!(store_app("메모장을 이용해서 소설을 써줘"), Some("메모장"));
        assert_eq!(store_app("Open Notepad and write a poem"), Some("메모장"));
        assert_eq!(store_app("paint 앱으로 그려줘"), Some("그림판"));
        assert_eq!(store_app("Fix the paintball schedule"), None, "낱말 안의 일부는 아님");
        assert_eq!(store_app("오늘 날씨 찾아줘"), None);
    }
}
