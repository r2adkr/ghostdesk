//! Settings window (tabs: General, AI, Permissions on macOS, About) and the model picker
//! on the home screen. The new-agent window reuses the current connection's key and URL fields.
//!
//! The rules for one connection (name, URL, missing setup) live in `ai.rs`; this module only
//! draws and edits them.

use super::ai::{self, AiConn};
use super::labels::{effort_label, effort_short};
use super::*;

impl GhostApp {
    /// The current connection.
    pub(super) fn cur(&self) -> Option<&AiConn> {
        self.ai.iter().find(|c| c.id == self.ai_current)
    }

    fn conn_index(&self, id: u64) -> Option<usize> {
        self.ai.iter().position(|c| c.id == id)
    }

    /// What the current connection still needs before it can start work. `None` when ready.
    pub(super) fn missing_setup(&self) -> Option<String> {
        match self.cur() {
            None => Some(tr("설정에서 쓸 AI 를 추가해 주세요").to_string()),
            Some(c) if c.provider.is_cli() && !matches!(self.model_fetch.lock().get(&c.id),Some(ModelFetch::Done(Ok(models))) if !models.is_empty()) => Some(tr("설정에서 CLI 연결을 확인해 주세요").to_string()),
            Some(c) if c.provider.is_cli() && c.model.trim().is_empty() => Some(tr("CLI 모델을 선택해 주세요").to_string()),
            Some(c) => c.missing(),
        }
    }

    /// Whether a connection is ready enough to show in the home menu. Ollama needs nothing, so it
    /// only shows while in use (many PCs don't have it running).
    fn ready(&self, c: &AiConn) -> bool {
        if c.provider.is_cli() || c.provider.needs_key() || c.provider.needs_url() {
            c.configured()
        } else {
            c.id == self.ai_current
        }
    }

    /// Starts fetching a connection's model list from the API if not done yet (skipped without a
    /// URL or required key).
    fn fetch_models(&mut self, ctx: &egui::Context, id: u64) -> ModelFetch {
        let Some(c) = self.ai.iter().find(|c| c.id == id) else { return ModelFetch::Idle };
        if c.provider.is_cli() && crate::cli::login_changed(id, c.provider) {
            self.model_fetch.lock().remove(&id);
        }
        let state = self.model_fetch.lock().get(&id).cloned().unwrap_or_default();
        if !matches!(state, ModelFetch::Idle) || !c.configured() {
            return state;
        }
        static NEXT_REQUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let request = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
        self.model_fetch.lock().insert(id, ModelFetch::Loading(request));
        let (slot, p, url, key, ctx) = (self.model_fetch.clone(), c.provider, c.base_url(), c.key.clone(), ctx.clone());
        self.rt.spawn(async move {
            let r = crate::llm::list_models(p, &url, &key).await.map_err(|e| format!("{e:#}"));
            let mut slot = slot.lock();
            if matches!(slot.get(&id), Some(ModelFetch::Loading(active)) if *active == request) {
                slot.insert(id, ModelFetch::Done(r));
            }
            ctx.request_repaint();
        });
        ModelFetch::Loading(request)
    }

    /// The key or URL changed, so refetch that connection's model list next time.
    fn refetch(&self, id: u64) {
        self.model_fetch.lock().remove(&id);
    }

    /// Models a connection can pick: the full list from the API (defaults before fetching or on
    /// failure). The current model is put first even if it's not in the list (typed by hand).
    fn models_for(c: &AiConn, state: &ModelFetch) -> Vec<String> {
        let mut list: Vec<String> = match state {
            ModelFetch::Done(Ok(list)) if !list.is_empty() => list.clone(),
            _ => c.provider.default_models().iter().map(|s| s.to_string()).collect(),
        };
        let cur = c.model.trim();
        if !cur.is_empty() && !list.iter().any(|m| m == cur) {
            list.insert(0, cur.to_string());
        }
        list
    }

    /// A small indicator while loading. On failure, what went wrong plus "Retry" (the raw API error
    /// shows on hover).
    fn fetch_status(&self, ui: &mut egui::Ui, id: u64, state: &ModelFetch) {
        match state {
            ModelFetch::Loading(_) => {
                ui.horizontal(|ui| {
                    theme::spinner(ui, 14.0, theme::muted());
                    ui.label(theme::hint(tr("모델 목록을 불러오는 중…")));
                });
            }
            ModelFetch::Done(Err(e)) => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(if self.ai.iter().any(|c| c.id == id && c.provider.is_cli()) { tr("CLI 연결을 확인하지 못했습니다") } else { tr("모델 목록을 못 불러와서 기본 모델만 보여요") }).size(12.0).color(theme::warn()))
                        .on_hover_text(crate::llm::short(e, 300));
                    if ui.link(RichText::new(tr("다시 시도")).size(12.0)).clicked() {
                        self.refetch(id);
                    }
                });
            }
            _ => {}
        }
    }

    // ---- input fields (shared by Settings and the new-agent window) ----

    /// API key field of connection `i`. A changed key refetches that connection's model list.
    pub(super) fn key_field(&mut self, ui: &mut egui::Ui, i: usize, width: f32, id: egui::Id) -> egui::Response {
        let c = &mut self.ai[i];
        let r = ui.add(
            egui::TextEdit::singleline(&mut c.key)
                .id(id)
                .password(true)
                .desired_width(width)
                .margin(Margin::symmetric(10, 6))
                .hint_text(tr(c.provider.key_hint())),
        );
        if r.changed() {
            self.refetch(self.ai[i].id);
        }
        r
    }

    /// URL field of (OpenAI-compatible) connection `i`. A change refetches the model list.
    pub(super) fn url_field(&mut self, ui: &mut egui::Ui, i: usize, width: f32) -> egui::Response {
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.ai[i].url)
                .desired_width(width)
                .margin(Margin::symmetric(10, 6))
                .hint_text(tr("API 주소 (예: https://api.deepseek.com/v1)")),
        );
        if r.changed() {
            self.refetch(self.ai[i].id);
        }
        r
    }

    /// New-agent window: fields to fill in the current connection's missing key or URL right there
    /// (or a button opening Settings when there's no connection). Returns true if a field changed.
    pub(super) fn setup_fields(&mut self, ui: &mut egui::Ui, width: f32, focus_key: bool) -> bool {
        let Some(i) = self.conn_index(self.ai_current) else {
            if theme::secondary(ui, trf("{}  AI 추가", &[&icon::PLUS])).clicked() {
                self.open_settings(SettingsTab::Ai);
            }
            return false;
        };
        let mut changed = false;
        ui.label(RichText::new(self.ai[i].title()).strong().color(theme::text()));
        if self.ai[i].provider.is_cli() {
            if theme::secondary(ui, tr("설치·로그인 안내")).clicked() { self.open_settings(SettingsTab::Ai); }
        }
        if self.ai[i].provider.needs_url() {
            changed |= self.url_field(ui, i, width).changed();
        }
        if self.ai[i].provider.takes_key() {
            let r = self.key_field(ui, i, width, egui::Id::unique("wizard_api_key"));
            if focus_key {
                r.request_focus();
            }
            changed |= r.changed();
        }
        changed
    }

    /// Current connection's model name field plus the ▾ list (Settings).
    fn model_field(&mut self, ui: &mut egui::Ui, width: f32) {
        let Some(i) = self.conn_index(self.ai_current) else { return };
        let id = self.ai[i].id;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.add(egui::TextEdit::singleline(&mut self.ai[i].model).desired_width(width - 40.0).margin(Margin::symmetric(10, 6)));
            ui.menu_button(icon::CHEVRON_DOWN, |ui| {
                ui.set_min_width(260.0);
                let state = self.fetch_models(ui.ctx(), id);
                self.fetch_status(ui, id, &state);
                let list = Self::models_for(&self.ai[i], &state);
                egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                    for m in list {
                        if ui.selectable_label(self.ai[i].model == m, &m).clicked() {
                            self.ai[i].model = m;
                            ui.close();
                        }
                    }
                });
            })
            .response
            .on_hover_text(tr("쓸 수 있는 모델 보기"));
        });
    }

    /// Thinking depth slider (low … max). Returns true if changed.
    fn effort_slider(&mut self, ui: &mut egui::Ui, width: f32) -> bool {
        let label_w = 96.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().slider_width = (width - label_w).max(80.0);
            let r = ui
                .add(egui::Slider::new(&mut self.effort_idx, 0..=EFFORTS.len() - 1).show_value(false))
                .on_hover_text(tr("높을수록 꼼꼼하지만 느리고 비용이 더 듭니다"));
            ui.label(RichText::new(effort_short(EFFORTS[self.effort_idx])).color(theme::body())).on_hover_text(effort_label(EFFORTS[self.effort_idx]));
            r.changed()
        })
        .inner
    }

    // ---- model button on the home screen ----

    /// Model button under the home input box: pick from every ready connection's models in one list
    /// and switch right away.
    pub(super) fn model_picker(&mut self, ui: &mut egui::Ui) {
        // connection name (dimmed), model name, thinking depth and ▾ vertically centered on one line
        // (the icon font has a different baseline, so centering is needed)
        let mut label = egui::text::LayoutJob::default();
        let part = |size: f32, color: Color32| egui::TextFormat {
            font_id: egui::FontId::proportional(size),
            color,
            valign: egui::Align::Center,
            ..Default::default()
        };
        match self.cur() {
            Some(c) => {
                label.append(&c.title(), 0.0, part(13.0, theme::muted()));
                label.append(&c.model, 6.0, part(13.0, theme::body()));
                if c.provider.supports_effort() {
                    label.append(&format!("· {}", effort_short(EFFORTS[self.effort_idx])), 6.0, part(13.0, theme::muted()));
                }
            }
            None => label.append(tr("AI 를 추가해 주세요"), 0.0, part(13.0, theme::muted())),
        }
        label.append(icon::CHEVRON_DOWN, 6.0, part(12.0, theme::muted()));
        let button = egui::Button::new(label).truncate().frame_when_inactive(false).min_size(Vec2::new(0.0, 28.0));
        let hover = self.cur().map(|c| tr(c.provider.label()).to_string()).unwrap_or_default();
        let (r, _) = egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
            // fixed width, otherwise the search field grabs all remaining width and the menu grows as wide
            // as the screen
            ui.set_width(340.0);
            // align headers, search text and model names to the menu rows' text start (button padding).
            // Menus default to 2 px padding, which puts text against the selection background's edge, so
            // widen it.
            ui.spacing_mut().button_padding.x = 10.0;
            let pad = ui.spacing().button_padding.x;
            let indent = |ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui)| {
                ui.horizontal(|ui| {
                    ui.add_space(pad);
                    add(ui);
                });
            };
            let mut chosen: Option<(u64, String)> = None;
            // Enter in the search field picks the top visible model (checked before the field eats Enter)
            let qid = ui.make_persistent_id("model_query");
            let enter = ui.ctx().memory(|m| m.has_focus(qid)) && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let q = ui.add(
                egui::TextEdit::singleline(&mut self.model_query)
                    .id(qid)
                    .desired_width(ui.available_width())
                    .margin(Margin::symmetric(pad as i8, 6))
                    .hint_text(tr("모델 검색")),
            );
            // ready to type as soon as the list opens (unless typing in another field)
            if ui.ctx().memory(|m| m.focused().is_none()) {
                q.request_focus();
            }
            let query = self.model_query.trim().to_lowercase();
            let mut first: Option<(u64, String)> = None;
            let mut shown = 0;
            let ids: Vec<u64> = self.ai.iter().filter(|c| self.ready(c)).map(|c| c.id).collect();
            egui::ScrollArea::vertical().max_height(420.0).auto_shrink([false, true]).show(ui, |ui| {
                for id in ids {
                    let state = self.fetch_models(ui.ctx(), id);
                    let Some(c) = self.ai.iter().find(|c| c.id == id) else { continue };
                    let models: Vec<String> =
                        Self::models_for(c, &state).into_iter().filter(|m| query.is_empty() || m.to_lowercase().contains(&query)).collect();
                    let busy = matches!(state, ModelFetch::Loading(_) | ModelFetch::Done(Err(_)));
                    if models.is_empty() && !busy {
                        continue;
                    }
                    ui.add_space(8.0);
                    let title = c.title();
                    let count = match &state {
                        ModelFetch::Done(Ok(all)) => format!("· {}", all.len()),
                        _ => String::new(),
                    };
                    indent(ui, &mut |ui| {
                        ui.label(theme::hint(&title));
                        ui.label(theme::hint(&count));
                    });
                    if busy {
                        indent(ui, &mut |ui| {
                            ui.vertical(|ui| self.fetch_status(ui, id, &state));
                        });
                    }
                    ui.add_space(2.0);
                    let selected_model = (id == self.ai_current).then(|| c.model.clone());
                    let blocked = c.provider.is_cli() && !matches!(&state, ModelFetch::Done(Ok(models)) if !models.is_empty());
                    for m in models {
                        shown += 1;
                        if !blocked && first.is_none() {
                            first = Some((id, m.clone()));
                        }
                        if ui.add_enabled(!blocked, egui::Button::selectable(selected_model.as_deref() == Some(m.as_str()), &m)).clicked() {
                            chosen = Some((id, m));
                        }
                    }
                }
                if shown == 0 {
                    ui.add_space(8.0);
                    let msg = if query.is_empty() { tr("설정에서 AI 를 추가하고 키를 넣어 주세요") } else { tr("맞는 모델이 없어요") };
                    indent(ui, &mut |ui| {
                        ui.label(theme::hint(msg));
                    });
                }
            });
            if enter && chosen.is_none() && !query.is_empty() {
                chosen = first;
            }
            if self.cur().is_some_and(|c| c.provider.supports_effort()) {
                ui.separator();
                indent(ui, &mut |ui| {
                    ui.label(theme::hint(tr("생각 깊이")));
                });
                let w = ui.available_width() - 2.0 * pad;
                let changed = ui
                    .horizontal(|ui| {
                        ui.add_space(pad);
                        self.effort_slider(ui, w)
                    })
                    .inner;
                if changed {
                    self.save_prefs();
                }
            }
            ui.separator();
            if ui.button(format!("{}  {}", icon::SETTINGS, tr("AI 회사 · API 키 관리"))).clicked() {
                self.open_settings(SettingsTab::Ai);
                ui.close();
            }
            if let Some((id, m)) = chosen {
                if let Some(i) = self.conn_index(id) {
                    self.ai[i].model = m;
                    self.ai_current = id;
                }
                self.model_query.clear();
                self.save_prefs();
                ui.close();
            }
        });
        if !hover.is_empty() {
            r.on_hover_text(hover);
        }
    }

    /// One line on where to get the key and where it's kept (new-agent window).
    pub(super) fn key_help(&self, ui: &mut egui::Ui) {
        let Some(c) = self.cur() else { return };
        let p = c.provider;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if p.needs_url() {
                ui.label(theme::hint(tr("OpenAI 호환 API 의 주소와 키를 입력하세요")));
            } else if p.needs_key() {
                ui.label(theme::hint(tr("키가 없나요?")));
                ui.hyperlink_to(RichText::new(trf("{} 에서 만들기", &[&p.short_name()])).size(12.0), p.key_page());
            } else if !p.is_cli() {
                ui.label(theme::hint(tr("키 없이 내 PC 에서 실행해요")));
                ui.hyperlink_to(RichText::new(tr("내려받기")).size(12.0), p.key_page());
            }
        });
    }

    fn key_storage_note(&self) -> &'static str {
        if self.remember_keys {
            tr("키는 Windows 계정으로 암호화해 이 PC 에만 저장해요")
        } else {
            tr("키는 이 PC 에 저장되지 않아요")
        }
    }

    // ---- settings window ----

    /// Opens Settings on a given tab.
    pub(super) fn open_settings(&mut self, tab: SettingsTab) {
        self.settings_tab = tab;
        self.show_settings = true;
    }

    /// ⚙ Settings, split into tabs: General (language, display, work) · AI (connections, current
    /// AI) · Permissions (macOS only) · About.
    pub(super) fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let width = (ctx.content_rect().width() - 96.0).clamp(560.0, 820.0);
        const NAV: f32 = 200.0;
        let body_h = (ctx.content_rect().height() - 200.0).clamp(240.0, 540.0);
        let mut close = false;
        let mut lang = self.lang;
        let perm_ok = self.permissions_ok(ctx);
        let mut tabs = vec![SettingsTab::General, SettingsTab::Ai, SettingsTab::Toolbox, SettingsTab::Ssh];
        tabs.push(SettingsTab::Permissions);
        tabs.push(SettingsTab::About);
        // as in Claude's and macOS settings: the page list on a panel of its own at the left, the page's
        // title over its rows at the right
        let m = egui::Modal::new(egui::Id::unique("settings")).frame(theme::modal_frame().inner_margin(0)).show(ctx, |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::Frame::new().inner_margin(egui::Margin { left: 24, right: 16, top: 16, bottom: 4 }).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                close |= theme::modal_header(ui, tr("설정"));
                if let Some(error) = &self.prefs_error {
                    ui.colored_label(theme::danger(), trf("설정 파일을 읽거나 저장하지 못했어요 ({})", &[error]));
                    ui.add_space(8.0);
                }
            });
            let line_y = ui.cursor().top();
            ui.painter().hline(ui.min_rect().x_range(), line_y, egui::Stroke::new(1.0, theme::border()));
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                let r = theme::MODAL_RADIUS;
                egui::Frame::new()
                    .fill(theme::surface())
                    .corner_radius(egui::CornerRadius { nw: 0, ne: 0, sw: r, se: 0 })
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| ui.vertical(|ui| {
                        ui.set_width(NAV - 24.0);
                        ui.set_height(body_h + 16.0);
                        ui.spacing_mut().item_spacing.y = 2.0;
                        for &tab in &tabs {
                            let glyph = match tab {
                                SettingsTab::General => icon::SETTINGS,
                                SettingsTab::Ai => icon::PLUG,
                                SettingsTab::Ssh => icon::MONITOR,
                                SettingsTab::Toolbox => icon::APP_WINDOW,
                                SettingsTab::Permissions => icon::SHIELD_ALERT,
                                SettingsTab::About => icon::BOOK_OPEN,
                            };
                            let badge = (tab == SettingsTab::Permissions && !perm_ok).then(theme::warn);
                            if theme::nav_item(ui, self.settings_tab == tab, glyph, tr(tab.label()), badge).clicked() {
                                self.settings_tab = tab;
                                self.license_view = None;
                            }
                        }
                    }));
                egui::Frame::new().inner_margin(egui::Margin { left: 28, right: 20, top: 20, bottom: 12 }).show(ui, |ui| ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
                    let body_w = ui.available_width();
                    ui.label(RichText::new(tr(self.settings_tab.label())).size(20.0).strong().color(theme::text()));
                    ui.add_space(6.0);
                    ui.push_id(("settings_body", self.settings_tab as u8), |ui| {
                        theme::modal_scroll(ui, body_h - 28.0, |ui| {
                            // room for the scroll bar at the right
                            ui.set_width(body_w - 12.0);
                            ui.set_min_height(body_h - 28.0);
                            match self.settings_tab {
                                SettingsTab::General => self.settings_general(ui, &mut lang),
                                SettingsTab::Ssh => self.settings_ssh(ui),
                                SettingsTab::Toolbox => self.settings_toolbox(ui),
                                SettingsTab::Ai => self.settings_ai(ui, body_w - 12.0),
                                SettingsTab::Permissions => self.settings_permissions(ui),
                                SettingsTab::About => self.settings_about(ui),
                            }
                        });
                    });
                }));
            });
        });
        if lang != self.lang {
            self.set_lang(ctx, lang);
        }
        if close || m.should_close() {
            self.show_settings = false;
            self.license_view = None;
            self.save_prefs();
            if let Some(error) = &self.prefs_error {
                self.notify(ctx, trf("설정 파일을 읽거나 저장하지 못했어요 ({})", &[error]));
            }
        }
    }

    /// General tab: language, display, work.
    fn settings_general(&mut self, ui: &mut egui::Ui, lang: &mut Lang) {
        theme::setting_row(ui, tr("언어"), "", false, 180.0, |ui| {
            // language names always in their own language (findable even if you can't read the current one)
            egui::ComboBox::from_id_salt("lang").icon(theme::combo_icon).width(180.0).selected_text(lang.native_name()).show_ui(ui, |ui| {
                for l in Lang::ALL {
                    ui.selectable_value(lang, l, l.native_name());
                }
            });
        });

        group(ui, tr("화면"));
        switch_row(ui, &mut self.show_cursor, tr("가상 마우스 보이기"), tr("에이전트가 어디를 누르는지 화면에 보여 줍니다 (내가 직접 조작할 때는 주황색)"));
        switch_row(ui, &mut self.pip_auto_open, tr("에이전트 화면이 열리면 PiP 자동 표시"), "");
        switch_row(ui, &mut self.pip_auto_close, tr("에이전트 화면이 닫히면 PiP 자동 닫기"), "");
        let tour = theme::setting_row(ui, tr("사용 설명 다시 보기"), "", false, 96.0, |ui| theme::secondary(ui, tr("열기")).clicked());
        if tour {
            self.show_settings = false;
            self.save_prefs();
            self.open_tour();
        }

        group(ui, tr("작업"));
        switch_row(ui, &mut self.keep_awake, tr("화면이 잠겨도 계속 작업"), tr("에이전트가 일하는 동안에는 컴퓨터가 잠자기에 들어가지 않게 해요. 화면은 꺼지거나 잠겨도 돼요."));
        switch_row(ui, &mut self.auto_clean, tr("끝난 화면 자동 정리"), tr("외부 에이전트가 닫은 화면을 목록에서 지우고, 이름 없이 열린 임시 작업공간 폴더도 삭제해요. 이름을 정해 둔 작업공간과 로그인 복제본은 그대로 둬요."));

        group(ui, tr("실험적 기능"));
        switch_row(ui, &mut self.allow_experimental, tr("실험적 기능 허용"), tr("켜 두면 가상 디스플레이 등 실험적 기능의 추가 동의를 생략해요. 일반 권한 확인은 별도로 유지돼요"));
    }

    /// Permissions tab: approval rules for outside agents (and the macOS permission list above them).
    fn settings_permissions(&mut self, ui: &mut egui::Ui) {
        #[cfg(target_os = "macos")]
        {
            self.permission_rows(ui);
            ui.add_space(18.0);
        }
        heading(ui, tr("승인"));
        // the warning is the description, in the warning color while it's on
        let warn = tr("외부 에이전트가 로그인 복제, 실행 중인 앱, 원격 접속을 묻지 않고 바로 시작해요. 믿는 에이전트만 연결했을 때 켜세요");
        let on = self.auto_approve;
        theme::setting_row(ui, tr("권한 확인 건너뛰기"), warn, on, 36.0, |ui| ui.add(theme::switch(&mut self.auto_approve, tr("권한 확인 건너뛰기")).text_hidden()));

        // the shell and system-tool refusals of the app modes
        let off = if cfg!(windows) {
            tr("끄면 터미널, PowerShell, 레지스트리 편집기 같은 도구는 외부 에이전트가 열 수 없어요")
        } else {
            tr("끄면 터미널, 스크립트 편집기, 시스템 설정 같은 도구는 외부 에이전트가 열 수 없어요")
        };
        let note = if !self.allow_shell_tools {
            off
        } else if self.auto_approve {
            if cfg!(windows) {
                tr("지금은 '권한 확인 건너뛰기' 때문에 묻지 않고 바로 열려요. 암호 관리자와 GhostDesk 자신은 항상 막혀요")
            } else {
                tr("지금은 '권한 확인 건너뛰기' 때문에 묻지 않고 바로 열려요. 암호 관리자는 항상 막혀요")
            }
        } else if cfg!(windows) {
            tr("외부 에이전트가 이런 도구를 열려고 하면 그때마다 물어봐요. 암호 관리자와 GhostDesk 자신은 항상 막혀요")
        } else {
            tr("외부 에이전트가 이런 도구를 열려고 하면 그때마다 물어봐요. 암호 관리자는 항상 막혀요")
        };
        let risky = self.allow_shell_tools && self.auto_approve;
        let title = tr("셸·터미널·시스템 도구 허용");
        theme::setting_row(ui, title, note, risky, 36.0, |ui| ui.add(theme::switch(&mut self.allow_shell_tools, title).text_hidden()));

        self.access_section(ui);
    }

    /// What agents on the engine may do on this computer, one row per kind of access.
    fn access_section(&mut self, ui: &mut egui::Ui) {
        use crate::access::Rule;
        group(ui, tr("에이전트의 내 컴퓨터 접근"));
        ui.label(theme::hint(tr("API 키 AI 에이전트가 파일·명령·도구를 쓰려 할 때 어떻게 할지 정해요. '권한 확인 건너뛰기'는 여기에 영향을 주지 않아요.")));
        let rows: [(&str, &str, bool); 4] = [
            ("작업 폴더 밖 파일 읽기", "에이전트 자기 폴더 밖의 파일을 읽으려 할 때", false),
            ("작업 폴더 밖 파일 쓰기·수정", "에이전트 자기 폴더 밖의 파일을 바꾸려 할 때", true),
            ("명령 실행", "에이전트가 터미널 명령을 실행하려 할 때", true),
            ("도구 상자의 도구 사용", "연결된 외부 도구를 쓰려 할 때 (도구마다 한 번)", false),
        ];
        let shell_off = !self.allow_shell_tools;
        for (i, (label, tip, risky)) in rows.into_iter().enumerate() {
            let rule = match i {
                0 => &mut self.access.read,
                1 => &mut self.access.write,
                2 => &mut self.access.run,
                _ => &mut self.access.tool,
            };
            let (desc, warn) = if i == 2 && shell_off {
                (tr("'셸·터미널·시스템 도구 허용'이 꺼져 있어 명령은 항상 막혀요"), false)
            } else if *rule == Rule::Allow && risky {
                (tr("묻지 않고 바로 실행돼요. 믿는 에이전트에만 켜세요"), true)
            } else {
                (tr(tip), false)
            };
            theme::setting_row(ui, tr(label), desc, warn, 240.0, |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for (r, name) in [(Rule::Deny, tr("막기")), (Rule::Allow, tr("항상 허용")), (Rule::Ask, tr("묻기"))] {
                    if ui.add(egui::Button::selectable(*rule == r, name)).clicked() {
                        *rule = r;
                    }
                }
            });
        }
        switch_row(ui, &mut self.access.plugins, tr("플러그인 설치 허용"), tr("플러그인은 이 앱 안에서 코드를 실행해요. 켜도 설치할 때마다 내가 직접 확인해야 해요"));
    }

    /// Remote tab: ceiling for the live view's frame rate.
    pub(super) fn settings_live_fps(&mut self, ui: &mut egui::Ui) {
        group(ui, tr("화면 속도"));
        let high = self.live_fps_limit > 30;
        let desc = if high {
            tr("30fps보다 높으면 CPU와 네트워크를 많이 써서 오히려 끊기거나 컴퓨터가 느려질 수 있어요. 느려지면 낮추세요")
        } else {
            tr("낮출수록 CPU와 네트워크를 덜 써요. 이 값이 호스트 모드를 포함한 모든 원격 화면의 상한이에요")
        };
        theme::setting_row(ui, tr("원격 화면 최대 프레임 (fps)"), desc, high, 220.0, |ui| {
            ui.add(egui::Slider::new(&mut self.live_fps_limit, 1..=120).suffix(" fps"));
        });
    }

    /// AI tab: connections (keys) and the AI/model in use.
    fn settings_ai(&mut self, ui: &mut egui::Ui, width: f32) {
        heading(ui, tr("AI 연결"));
        ui.add_space(4.0);
        self.conn_cards(ui, width);
        self.add_conn_button(ui);
        ui.add_space(6.0);
        if ui.link(tr("이 컴퓨터의 CLI 찾기")).clicked() {
            for provider in [Provider::ClaudeCli, Provider::CodexCli] {
                if !self.ai.iter().any(|c| c.provider == provider) && crate::cli::executable(provider, "").is_some() {
                    self.ai_next_id += 1;
                    let c = AiConn::new(self.ai_next_id, provider);
                    self.ai.push(c);
                }
            }
        }
        ui.add_space(4.0);
        if prefs::CAN_REMEMBER_KEYS {
            switch_row(ui, &mut self.remember_keys, tr("키를 이 PC 에 기억하기"), tr("Windows 계정으로 암호화해 저장해요. 다른 계정이나 다른 PC 에서는 풀 수 없어요."));
        }

        self.pi_section(ui);

        if self.ai.is_empty() {
            return;
        }
        group(ui, tr("지금 쓰는 AI"));
        const W: f32 = 280.0;
        let title = self.cur().map(AiConn::title).unwrap_or_default();
        theme::setting_row(ui, tr("AI"), "", false, W, |ui| {
            egui::ComboBox::from_id_salt("current_ai").icon(theme::combo_icon).width(W).selected_text(title).show_ui(ui, |ui| {
                for c in &self.ai {
                    ui.selectable_value(&mut self.ai_current, c.id, c.title());
                }
            });
        });
        theme::setting_row(ui, tr("모델"), "", false, W, |ui| ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| self.model_field(ui, W - 10.0)));
        if self.cur().is_some_and(|c| c.provider.supports_effort()) {
            theme::setting_row(ui, tr("생각 깊이"), tr("높을수록 꼼꼼하지만 느리고 비용이 더 듭니다"), false, W, |ui| {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| self.effort_slider(ui, W))
            });
        }
    }

    /// About tab: version, release notes, license, token usage so far.
    fn settings_about(&mut self, ui: &mut egui::Ui) {
        if let Some(view) = &mut self.license_view {
            if license_page(ui, view) {
                self.license_view = None;
            }
            return;
        }
        ui.label(RichText::new(trf("GhostDesk 버전 {}", &[&env!("CARGO_PKG_VERSION")])).color(theme::text()));
        ui.label(theme::hint(trf("{} 라이선스 · 오픈소스 패키지 포함", &[&crate::licenses::OWN_NAME])));
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if theme::secondary(ui, tr("릴리스 노트")).clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(crate::update::RELEASES_PAGE));
            }
            if theme::secondary(ui, tr("라이선스")).clicked() {
                self.license_view = Some(LicenseView::new());
            }
        });
        if let crate::update::State::Available(r) | crate::update::State::Failed(r, _) = &*self.update.lock() {
            ui.add_space(8.0);
            ui.label(theme::hint(trf("새 버전 {} 이 나왔어요", &[&r.version])));
        }
        // manual update check
        ui.add_space(8.0);
        let result = self.update_check.as_ref().map(|s| s.lock().clone());
        let checking = matches!(result, Some(None));
        ui.horizontal(|ui| {
            if ui.add_enabled_ui(!checking, |ui| theme::secondary(ui, tr("업데이트 확인"))).inner.clicked() {
                // a check by hand shows the toast again even if it was closed for this version
                self.update_dismissed = None;
                self.update_check = Some(crate::update::check_now(&self.rt, self.update.clone(), ui.ctx().clone()));
            }
            match &result {
                Some(None) => {
                    theme::spinner(ui, 13.0, theme::muted());
                    ui.label(theme::hint(tr("확인하는 중…")));
                }
                Some(Some(Ok(false))) => {
                    ui.label(theme::hint(tr("지금 최신 버전이에요")));
                }
                Some(Some(Ok(true))) => {
                    ui.label(theme::hint(tr("새 버전이 있어요. 오른쪽 아래 알림에서 업데이트하세요")));
                }
                Some(Some(Err(e))) => {
                    ui.label(RichText::new(trf("확인하지 못했어요: {}", &[&crate::llm::short(e, 80)])).size(12.0).color(theme::warn())).on_hover_text(e.as_str());
                }
                None => {}
            }
        });
        let (ti, to) = self.agents.iter().fold((0, 0), |acc, a| {
            let t = *a.shared.tokens.lock();
            (acc.0 + t.0, acc.1 + t.1)
        });
        if ti + to > 0 {
            ui.add_space(10.0);
            ui.label(theme::hint(trf("지금까지 쓴 양: 보낸 글 {} · 받은 글 {} 토큰", &[&ti, &to])));
        }
    }

    /// macOS permission state, rechecked at most once a second rather than every frame (shared by
    /// the sidebar, Settings and the guide).
    ///
    /// Requests a repaint a second later so changes made in System Settings show up quickly
    /// (Accessibility applies immediately).
    #[cfg(target_os = "macos")]
    pub(super) fn permissions(&mut self, ctx: &egui::Context) -> crate::macos::TccStatus {
        const EVERY: std::time::Duration = std::time::Duration::from_secs(1);
        let fresh = self.perm_cache.filter(|(at, _)| at.elapsed() < EVERY);
        let t = match fresh {
            Some((_, t)) => t,
            None => {
                let t = crate::macos::tcc_status();
                self.perm_cache = Some((std::time::Instant::now(), t));
                t
            }
        };
        ctx.request_repaint_after(EVERY);
        t
    }

    /// Whether every permission needed to drive Mac apps is on (always true outside macOS).
    pub(super) fn permissions_ok(&mut self, ctx: &egui::Context) -> bool {
        #[cfg(target_os = "macos")]
        {
            let t = self.permissions(ctx);
            t.accessibility && t.screen_capture
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = ctx;
            true
        }
    }

    /// macOS permissions needed to drive Mac apps: ✓ when on, otherwise "Allow" per permission (it
    /// shows a single macOS prompt).
    #[cfg(target_os = "macos")]
    pub(super) fn permission_rows(&mut self, ui: &mut egui::Ui) {
        let t = self.permissions(ui.ctx());
        let restart = crate::macos::screen_capture_needs_restart();
        // one permission per row: ✓ when on, otherwise "Allow" (one macOS prompt). Screen recording is
        // asked for after Accessibility is on
        let rows: [(bool, &str, fn()); 2] = [
            (t.accessibility, tr("손쉬운 사용"), crate::macos::request_accessibility),
            (t.screen_capture, tr("화면 기록"), crate::macos::request_screen_capture),
        ];
        let mut earlier_missing = false;
        for (ok, name, request) in rows {
            ui.horizontal(|ui| {
                if ok {
                    ui.label(RichText::new(icon::CIRCLE_CHECK).color(theme::success()));
                } else {
                    ui.label(RichText::new(icon::SHIELD_ALERT).color(theme::warn()));
                }
                ui.label(RichText::new(name).color(theme::text()));
                ui.label(theme::hint(if ok { tr("켜짐") } else { tr("꺼짐") }));
                if !ok {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let btn = ui.add_enabled_ui(!earlier_missing, |ui| theme::secondary(ui, tr("허용"))).inner;
                        if btn.on_hover_text(tr("macOS 의 요청 창이 떠요. 그 창의 '시스템 설정 열기'에서 GhostDesk 스위치를 켜세요.")).clicked() {
                            request();
                            // check again right after a click
                            self.perm_cache = None;
                        }
                    });
                }
            });
            earlier_missing |= !ok;
        }
        if t.accessibility && t.screen_capture {
            ui.label(theme::hint(tr("Mac 앱을 조작하는 데 필요한 권한이 모두 켜져 있어요")));
            return;
        }
        if restart {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(tr("화면 기록을 켰다면 GhostDesk 를 다시 시작해야 반영돼요")).color(theme::warn()));
                if theme::secondary(ui, tr("다시 시작")).clicked() {
                    self.save_prefs();
                    crate::update::request_relaunch(ui.ctx());
                }
            });
            // after asking, macOS should list GhostDesk; if it doesn't, it can be added by hand
            ui.horizontal_wrapped(|ui| {
                ui.label(theme::hint(tr("화면 기록 목록에 GhostDesk 가 없으면 목록 아래 + 를 눌러 Finder 에 보이는 GhostDesk 를 추가하세요.")));
                if ui.link(RichText::new(tr("화면 기록 설정과 앱 위치 열기")).size(12.0)).clicked() {
                    crate::macos::add_to_screen_capture_by_hand();
                }
            });
        }
    }

    /// One card per connection: name (editable for OpenAI-compatible) · provider · ✕, with URL and
    /// key fields below.
    fn conn_cards(&mut self, ui: &mut egui::Ui, width: f32) {
        let mut remove = None;
        for i in 0..self.ai.len() {
            let p = self.ai[i].provider;
            theme::card_frame().inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
                ui.set_width(width - 28.0);
                let inner = ui.available_width();
                ui.horizontal(|ui| {
                    if p.needs_url() {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.ai[i].name)
                                .desired_width(180.0)
                                .margin(Margin::symmetric(8, 4))
                                .hint_text(tr("이름 (예: DeepSeek)")),
                        );
                    } else {
                        ui.label(RichText::new(self.ai[i].title()).strong().color(theme::text()));
                    }
                    // the kind next to the name only when it adds something ("Codex CLI  Codex CLI" doesn't)
                    if self.ai[i].title() != tr(p.label()) {
                        ui.label(theme::hint(tr(p.label())));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::icon_button(ui, icon::X, tr("이 AI 연결 지우기")).clicked() {
                            remove = Some(i);
                        }
                        if self.ai[i].configured() && p.takes_key() {
                            ui.label(RichText::new(icon::CIRCLE_CHECK).color(theme::success())).on_hover_text(tr("쓸 준비가 됐어요"));
                        }
                    });
                });
                if p.needs_url() {
                    self.url_field(ui, i, inner);
                }
                if p.is_cli() {
                    let changed = ui.add(egui::TextEdit::singleline(&mut self.ai[i].url).desired_width(inner).hint_text(tr("경로 (보통 비워 두세요)"))).changed();
                    let id = self.ai[i].id;
                    if changed { self.refetch(id); }
                    let state = self.fetch_models(ui.ctx(), id);
                    match &state {
                        ModelFetch::Done(Ok(models)) => {
                            ui.label(RichText::new(tr("CLI 로그인 확인됨")).size(12.0).color(theme::success()));
                            if self.ai[i].model.trim().is_empty() { if let Some(model) = models.first() { self.ai[i].model = model.clone(); } }
                        }
                        _ => self.fetch_status(ui, id, &state),
                    }
                    if let ModelFetch::Done(Err(error)) = &state { ui.label(theme::hint(crate::llm::short(error, 500))); }
                    ui.horizontal(|ui| {
                        if ui.link(tr("연결 다시 확인")).clicked() { self.refetch(id); }
                        ui.hyperlink_to(tr("설치·로그인 안내"), p.key_page());
                    });
                } else if p.takes_key() {
                    ui.horizontal(|ui| {
                        let link = p.needs_key() && self.ai[i].key.trim().is_empty() && !p.key_page().is_empty();
                        let w = if link { inner - 72.0 } else { inner };
                        self.key_field(ui, i, w, egui::Id::unique(("settings_key", self.ai[i].id)));
                        if link {
                            ui.hyperlink_to(RichText::new(tr("키 받기")).size(12.0), p.key_page());
                        }
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.label(theme::hint(tr("키가 필요 없어요 (내 PC 에서 실행)")));
                        ui.hyperlink_to(RichText::new(tr("내려받기")).size(12.0), p.key_page());
                    });
                }
            });
            ui.add_space(6.0);
        }
        if let Some(i) = remove {
            let gone = self.ai.remove(i);
            self.refetch(gone.id);
            if gone.id == self.ai_current {
                self.ai_current = self.ai.first().map_or(0, |c| c.id);
            }
        }
    }

    /// "+ Add AI": creates a connection from providers not added yet or OpenAI-compatible (any
    /// number).
    fn add_conn_button(&mut self, ui: &mut egui::Ui) {
        let choices = ai::addable(&self.ai);
        ui.menu_button(format!("{}  {}", icon::PLUS, tr("AI 추가")), |ui| {
            ui.set_min_width(260.0);
            for p in choices {
                if ui.button(tr(p.label())).clicked() {
                    self.ai_next_id += 1;
                    let c = AiConn::new(self.ai_next_id, p);
                    // the first connection added becomes the current one
                    if self.ai.is_empty() {
                        self.ai_current = c.id;
                    }
                    self.ai.push(c);
                    ui.close();
                }
            }
        });
    }
}

/// State of Settings > About > License (the parsed package list lives only while it's open).
pub(super) struct LicenseView {
    data: crate::licenses::ThirdParty,
    query: String,
    /// package whose license texts are shown
    open: Option<usize>,
    own_open: bool,
}

impl LicenseView {
    fn new() -> Self {
        Self { data: crate::licenses::third_party(), query: String::new(), open: None, own_open: false }
    }
}

/// License page: GhostDesk's own license, then every open-source package built in, searchable,
/// with its license texts on click. Returns true when "back" was clicked.
fn license_page(ui: &mut egui::Ui, v: &mut LicenseView) -> bool {
    let mono = |t: &str| RichText::new(t.to_owned()).monospace().size(11.0).color(theme::body());
    let back = ui.link(RichText::new(format!("‹ {}", tr("정보"))).size(13.0)).clicked();
    ui.add_space(8.0);

    theme::section(ui, "GhostDesk");
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(crate::licenses::OWN_NAME).color(theme::text()));
        ui.hyperlink_to(RichText::new(crate::licenses::OWN_URL).size(12.0), crate::licenses::OWN_URL);
    });
    ui.label(theme::hint(tr("누구나 자유롭게 쓰고 고치고 나눠 줄 수 있어요. 나눠 줄 때는 이 라이선스 전문을 함께 주고 고친 파일에는 고쳤다고 표시해야 해요. 'GhostDesk' 이름은 허락에 포함되지 않고, 소프트웨어는 보증 없이 있는 그대로 제공돼요")));
    let label = if v.own_open { tr("전문 접기") } else { tr("전문 보기") };
    if ui.link(RichText::new(label).size(12.0)).clicked() {
        v.own_open = !v.own_open;
    }
    if v.own_open {
        theme::card_frame().inner_margin(Margin::same(10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(mono(&crate::licenses::own_text())).wrap());
        });
    }

    ui.add_space(14.0);
    theme::section(ui, tr("오픈소스 라이선스"));
    ui.label(theme::hint(trf("GhostDesk 에 들어간 오픈소스 패키지 {}개와 각 라이선스예요", &[&v.data.packages.len()])));
    ui.add_space(4.0);
    ui.add(
        egui::TextEdit::singleline(&mut v.query)
            .desired_width(ui.available_width())
            .margin(Margin::symmetric(10, 6))
            .hint_text(tr("패키지 이름이나 라이선스로 찾기")),
    );
    ui.add_space(6.0);
    let q = v.query.trim().to_lowercase();
    let mut shown = 0;
    for (i, p) in v.data.packages.iter().enumerate() {
        if !q.is_empty() && !p.name.to_lowercase().contains(&q) && !p.license.to_lowercase().contains(&q) {
            continue;
        }
        shown += 1;
        let open = v.open == Some(i);
        let row = ui
            .horizontal(|ui| {
                ui.label(RichText::new(if open { "▾" } else { "▸" }).color(theme::muted()));
                ui.label(RichText::new(format!("{} {}", p.name, p.version)).color(theme::text()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // a few packages have very long expressions; keep the row to the window width
                    const MAX: usize = 32;
                    let full = if p.license.is_empty() { "-".to_string() } else { p.license.clone() };
                    let short = if full.chars().count() > MAX {
                        format!("{}…", full.chars().take(MAX).collect::<String>().trim_end())
                    } else {
                        full.clone()
                    };
                    let r = ui.label(theme::hint(short));
                    if full.chars().count() > MAX {
                        r.on_hover_text(full);
                    }
                });
            })
            .response
            .interact(egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if row.clicked() {
            v.open = if open { None } else { Some(i) };
        }
        if open {
            if let Some(r) = &p.repository {
                ui.hyperlink_to(RichText::new(r).size(12.0), r);
            }
            for &t in &p.texts {
                theme::card_frame().inner_margin(Margin::same(10)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add(egui::Label::new(mono(&crate::licenses::reflow(&v.data.texts[t]))).wrap());
                });
                ui.add_space(4.0);
            }
        }
    }
    if shown == 0 {
        ui.label(theme::hint(tr("찾는 패키지가 없어요")));
    }
    back
}

/// Settings window tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) enum SettingsTab {
    #[default]
    General,
    Ai,
    Ssh,
    Toolbox,
    /// macOS only (permissions needed to drive Mac apps).
    Permissions,
    About,
}

impl SettingsTab {
    fn label(self) -> &'static str {
        match self {
            Self::General => "일반",
            Self::Ai => "AI",
            Self::Ssh => "원격 연결",
            Self::Toolbox => "도구 상자",
            Self::Permissions => "권한",
            Self::About => "정보",
        }
    }
}

/// A titled group of settings rows.
pub(super) fn group(ui: &mut egui::Ui, title: &str) {
    ui.add_space(24.0);
    heading(ui, title);
}

/// The title of a page's first group (right under the page title).
pub(super) fn heading(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(13.0).strong().color(theme::body()));
    ui.add_space(2.0);
}

/// A settings row with a switch at the right.
fn switch_row(ui: &mut egui::Ui, on: &mut bool, title: &str, desc: &str) -> egui::Response {
    theme::setting_row(ui, title, desc, false, 36.0, |ui| ui.add(theme::switch(on, title).text_hidden()))
}
