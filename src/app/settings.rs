//! 설정 창(언어 · AI 연결 목록 · 지금 쓰는 AI · 화면 · 작업)과 홈 화면의 모델 고르기.
//! 새 에이전트 창도 지금 연결의 키·주소 칸을 같이 쓴다.
//!
//! AI 연결 하나의 규칙(이름·주소·빠진 설정)은 `ai.rs` 에 있고, 여기는 그것을 그리고 고치는 화면만 맡는다.

use super::ai::{self, AiConn};
use super::labels::{effort_label, effort_short};
use super::*;

impl GhostApp {
    /// 지금 쓰는 연결
    pub(super) fn cur(&self) -> Option<&AiConn> {
        self.ai.iter().find(|c| c.id == self.ai_current)
    }

    fn conn_index(&self, id: u64) -> Option<usize> {
        self.ai.iter().position(|c| c.id == id)
    }

    /// 지금 연결로 일을 시작하기에 빠진 설정. 다 있으면 None
    pub(super) fn missing_setup(&self) -> Option<String> {
        match self.cur() {
            None => Some(tr("설정에서 쓸 AI 를 추가해 주세요.").to_string()),
            Some(c) => c.missing(),
        }
    }

    /// 홈 메뉴에 보일 만큼 준비된 연결인가. 아무것도 필요 없는 Ollama 는 지금 쓰고 있을 때만 (켜 두지 않은 PC 가 많다)
    fn ready(&self, c: &AiConn) -> bool {
        if c.provider.needs_key() || c.provider.needs_url() {
            c.configured()
        } else {
            c.id == self.ai_current
        }
    }

    /// 연결의 모델 목록을 아직 안 불러왔으면 API 에서 불러오기 시작한다 (주소·필수 키가 없으면 하지 않음).
    fn fetch_models(&mut self, ctx: &egui::Context, id: u64) -> ModelFetch {
        let Some(c) = self.ai.iter().find(|c| c.id == id) else { return ModelFetch::Idle };
        let state = self.model_fetch.lock().get(&id).cloned().unwrap_or_default();
        if !matches!(state, ModelFetch::Idle) || !c.configured() {
            return state;
        }
        self.model_fetch.lock().insert(id, ModelFetch::Loading);
        let (slot, p, url, key, ctx) = (self.model_fetch.clone(), c.provider, c.base_url(), c.key.clone(), ctx.clone());
        self.rt.spawn(async move {
            let r = crate::llm::list_models(p, &url, &key).await.map_err(|e| format!("{e:#}"));
            slot.lock().insert(id, ModelFetch::Done(r));
            ctx.request_repaint();
        });
        ModelFetch::Loading
    }

    /// 키·주소가 바뀌었으니 그 연결의 모델 목록을 다음에 새로 불러온다
    fn refetch(&self, id: u64) {
        self.model_fetch.lock().remove(&id);
    }

    /// 연결에서 고를 수 있는 모델: API 에서 불러온 전체 목록 (불러오기 전·실패 때는 기본 목록).
    /// 지금 고른 모델은 목록에 없어도(이름을 직접 적었어도) 맨 앞에 넣는다.
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

    /// 불러오는 중이면 작은 표시. 실패했으면 무슨 일인지와 '다시 시도' (API 원문 오류는 글에 마우스를 올리면).
    fn fetch_status(&self, ui: &mut egui::Ui, id: u64, state: &ModelFetch) {
        match state {
            ModelFetch::Loading => {
                ui.horizontal(|ui| {
                    theme::spinner(ui, 14.0, theme::muted());
                    ui.label(theme::hint(tr("모델 목록을 불러오는 중…")));
                });
            }
            ModelFetch::Done(Err(e)) => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(tr("모델 목록을 못 불러와서 기본 모델만 보여요")).size(12.0).color(theme::warn()))
                        .on_hover_text(crate::llm::short(e, 300));
                    if ui.link(RichText::new(tr("다시 시도")).size(12.0)).clicked() {
                        self.refetch(id);
                    }
                });
            }
            _ => {}
        }
    }

    // ── 입력 칸 (설정 창·새 에이전트 창이 같이 쓴다) ──

    /// `i` 번째 연결의 API 키 칸. 키가 바뀌면 그 연결의 모델 목록을 새 키로 다시 불러온다.
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

    /// `i` 번째(OpenAI 호환) 연결의 주소 칸. 바뀌면 모델 목록을 다시 불러온다.
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

    /// 새 에이전트 창: 지금 연결에 빠진 키·주소를 바로 넣는 칸 (연결이 없으면 설정을 여는 단추).
    /// 칸이 바뀌면 true.
    pub(super) fn setup_fields(&mut self, ui: &mut egui::Ui, width: f32, focus_key: bool) -> bool {
        let Some(i) = self.conn_index(self.ai_current) else {
            if theme::secondary(ui, trf("{}  AI 추가", &[&icon::PLUS])).clicked() {
                self.show_settings = true;
            }
            return false;
        };
        let mut changed = false;
        ui.label(RichText::new(self.ai[i].title()).strong().color(theme::text()));
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

    /// 지금 연결의 모델 이름 칸 + ▾ 목록 (설정 창)
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

    /// 생각 깊이 슬라이더 (낮음 … 최대). 바뀌면 true
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

    // ── 홈 화면의 모델 단추 ──

    /// 홈 입력 상자 아래의 모델 단추: 준비된 모든 연결의 모델을 한 목록에서 골라 바로 바꾼다.
    pub(super) fn model_picker(&mut self, ui: &mut egui::Ui) {
        // 연결 이름(흐리게) · 모델 이름 · 생각 깊이 · ▾ 를 한 줄 가운데에 맞춰 놓는다 (아이콘 글꼴은 기준선이 달라 가운데 정렬이 필요)
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
        let button = egui::Button::new(label).frame_when_inactive(false).min_size(Vec2::new(0.0, 28.0));
        let hover = self.cur().map(|c| tr(c.provider.label()).to_string()).unwrap_or_default();
        let (r, _) = egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
            // 폭을 고정한다 (검색 칸이 남는 폭을 다 차지하려 해서 메뉴가 화면만큼 넓어지지 않게)
            ui.set_width(340.0);
            // 머리글·검색 글·모델 이름을 모두 메뉴 줄의 글 시작점(단추 안쪽 여백)에 맞춘다
            // (메뉴는 기본 안쪽 여백이 2px 라 선택 배경 끝에 글이 붙으므로 넓힌다)
            ui.spacing_mut().button_padding.x = 10.0;
            let pad = ui.spacing().button_padding.x;
            let indent = |ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui)| {
                ui.horizontal(|ui| {
                    ui.add_space(pad);
                    add(ui);
                });
            };
            let mut chosen: Option<(u64, String)> = None;
            // 검색 칸에서 Enter: 맨 위에 보이는 모델을 고른다 (칸이 Enter 를 가져가기 전에 본다)
            let qid = ui.make_persistent_id("model_query");
            let enter = ui.ctx().memory(|m| m.has_focus(qid)) && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let q = ui.add(
                egui::TextEdit::singleline(&mut self.model_query)
                    .id(qid)
                    .desired_width(ui.available_width())
                    .margin(Margin::symmetric(pad as i8, 6))
                    .hint_text(tr("모델 검색")),
            );
            // 목록을 열면 바로 칠 수 있게 (다른 칸에 입력 중이 아닐 때)
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
                    let busy = matches!(state, ModelFetch::Loading | ModelFetch::Done(Err(_)));
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
                    for m in models {
                        shown += 1;
                        if first.is_none() {
                            first = Some((id, m.clone()));
                        }
                        if ui.selectable_label(selected_model.as_deref() == Some(m.as_str()), &m).clicked() {
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
                self.show_settings = true;
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

    /// 키를 어디서 받는지 · 어디에 두는지 한 줄로 (새 에이전트 창)
    pub(super) fn key_help(&self, ui: &mut egui::Ui) {
        let Some(c) = self.cur() else { return };
        let p = c.provider;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if p.needs_url() {
                ui.label(theme::hint(tr("DeepSeek·Groq·Together·LM Studio·vLLM 처럼 OpenAI 와 같은 방식의 API 를 쓸 수 있어요.")));
            } else if p.needs_key() {
                ui.label(theme::hint(tr("키가 없나요?")));
                ui.hyperlink_to(RichText::new(trf("{} 에서 만들기", &[&p.short_name()])).size(12.0), p.key_page());
                ui.label(theme::hint(format!("· {}", self.key_storage_note())));
            } else {
                ui.label(theme::hint(tr("Ollama 를 설치해 켜 두면 키 없이 내 PC 에서 돌아갑니다.")));
                ui.hyperlink_to(RichText::new(tr("내려받기")).size(12.0), p.key_page());
            }
        });
    }

    fn key_storage_note(&self) -> &'static str {
        if self.remember_keys {
            tr("키는 Windows 계정으로 암호화해 이 PC 에만 저장해요.")
        } else {
            tr("키는 이 PC 에 저장되지 않아요.")
        }
    }

    // ── 설정 창 ──

    /// ⚙ 설정: 언어 · AI 연결 · 지금 쓰는 AI · 화면 · 작업
    pub(super) fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        const W: f32 = 520.0;
        const LABEL: f32 = 96.0;
        let field = W - LABEL - 14.0;
        let max_h = (ctx.content_rect().height() - 160.0).max(260.0);
        let mut close = false;
        let mut lang = self.lang;
        let (ti, to) = self.agents.iter().fold((0, 0), |acc, a| {
            let t = *a.shared.tokens.lock();
            (acc.0 + t.0, acc.1 + t.1)
        });
        let m = egui::Modal::new(egui::Id::unique("settings")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(W);
            close |= theme::modal_header(ui, tr("설정"));
            theme::modal_scroll(ui, max_h, |ui| {
                ui.set_width(W);
                egui::Grid::new("settings_lang").num_columns(2).min_col_width(LABEL).spacing([14.0, 10.0]).show(ui, |ui| {
                    ui.label(tr("언어"));
                    // 언어 이름은 늘 그 언어로 (지금 언어를 못 읽어도 찾을 수 있게)
                    egui::ComboBox::from_id_salt("lang").width(field).selected_text(lang.native_name()).show_ui(ui, |ui| {
                        for l in Lang::ALL {
                            ui.selectable_value(&mut lang, l, l.native_name());
                        }
                    });
                    ui.end_row();
                });

                // 권한이 꺼져 있으면 찾기 쉽게 맨 위에 (다 켜지면 아래 원래 자리로)
                #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
                let perm_first = !self.permissions_ok(ctx);
                #[cfg(target_os = "macos")]
                if perm_first {
                    ui.add_space(18.0);
                    theme::section(ui, tr("권한"));
                    ui.add_space(4.0);
                    self.permission_rows(ui);
                }

                ui.add_space(18.0);
                theme::section(ui, tr("AI 연결"));
                ui.label(theme::hint(tr("여러 AI 를 추가해 두면 홈 화면의 모델 단추에서 바로 바꿔 쓸 수 있어요.")));
                ui.add_space(4.0);
                self.conn_cards(ui, W);
                self.add_conn_button(ui);
                ui.add_space(4.0);
                if prefs::CAN_REMEMBER_KEYS {
                    ui.add(theme::switch(&mut self.remember_keys, tr("키를 이 PC 에 기억하기")))
                        .on_hover_text(tr("Windows 계정으로 암호화해 저장해요. 다른 계정이나 다른 PC 에서는 풀 수 없어요."));
                } else {
                    ui.label(theme::hint(self.key_storage_note()));
                }

                if !self.ai.is_empty() {
                    ui.add_space(18.0);
                    theme::section(ui, tr("지금 쓰는 AI"));
                    ui.add_space(4.0);
                    egui::Grid::new("settings_ai").num_columns(2).min_col_width(LABEL).spacing([14.0, 10.0]).show(ui, |ui| {
                        ui.label(tr("AI"));
                        let title = self.cur().map(AiConn::title).unwrap_or_default();
                        egui::ComboBox::from_id_salt("current_ai").width(field).selected_text(title).show_ui(ui, |ui| {
                            for c in &self.ai {
                                ui.selectable_value(&mut self.ai_current, c.id, c.title());
                            }
                        });
                        ui.end_row();
                        ui.label(tr("모델"));
                        self.model_field(ui, field);
                        ui.end_row();
                        if self.cur().is_some_and(|c| c.provider.supports_effort()) {
                            ui.label(tr("생각 깊이"));
                            self.effort_slider(ui, field);
                            ui.end_row();
                        }
                    });
                }

                ui.add_space(18.0);
                theme::section(ui, tr("화면"));
                ui.add_space(4.0);
                ui.add(theme::switch(&mut self.show_cursor, tr("가상 마우스 보이기")))
                    .on_hover_text(tr("에이전트가 어디를 누르는지 화면에 보여 줍니다 (내가 직접 조작할 때는 주황색)"));
                ui.add_space(6.0);
                if theme::secondary(ui, tr("사용 설명 다시 보기")).clicked() {
                    self.show_settings = false;
                    self.save_prefs();
                    self.open_tour();
                }

                ui.add_space(18.0);
                theme::section(ui, tr("작업"));
                ui.add_space(4.0);
                ui.add(theme::switch(&mut self.keep_awake, tr("화면이 잠겨도 계속 작업")))
                    .on_hover_text(tr("에이전트가 일하는 동안에는 컴퓨터가 잠자기에 들어가지 않게 해요. 화면은 꺼지거나 잠겨도 돼요."));
                ui.label(theme::hint(tr("노트북 덮개를 닫으면 전원에 연결하고 외부 모니터를 쓰는 경우가 아니면 잠자기에 들어가요.")));
                #[cfg(target_os = "macos")]
                if !perm_first {
                    ui.add_space(18.0);
                    theme::section(ui, tr("권한"));
                    ui.add_space(4.0);
                    self.permission_rows(ui);
                }
                if ti + to > 0 {
                    ui.add_space(10.0);
                    ui.label(theme::hint(trf("지금까지 쓴 양: 보낸 글 {} · 받은 글 {} 토큰", &[&ti, &to])));
                }
            });
        });
        if lang != self.lang {
            self.set_lang(ctx, lang);
        }
        if close || m.should_close() {
            self.show_settings = false;
            self.save_prefs();
        }
    }

    /// macOS 권한 상태. 매 프레임 묻지 않고 1초에 한 번만 다시 확인한다 (사이드바·설정·사용 설명이 같이 쓴다).
    /// 시스템 설정에서 바꾼 것이 곧바로 보이게 1초 뒤 다시 그리게 한다 (손쉬운 사용은 바로 반영된다).
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

    /// Mac 앱 조작에 필요한 권한이 다 켜져 있는가 (macOS 밖은 늘 그렇다)
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

    /// macOS 권한 (Mac 앱 조작에 필요): 켜졌으면 ✓, 아니면 권한마다 '허용' (macOS 요청 창 하나만 띄운다)
    #[cfg(target_os = "macos")]
    pub(super) fn permission_rows(&mut self, ui: &mut egui::Ui) {
        let t = self.permissions(ui.ctx());
        let restart = crate::macos::screen_capture_needs_restart();
        // 한 줄에 권한 하나: 켜졌으면 ✓, 아니면 '허용' (macOS 요청 창 하나만 뜬다). 화면 기록은 손쉬운 사용을 켠 뒤에 묻는다
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
                            // 누른 뒤 바로 다시 확인한다
                            self.perm_cache = None;
                        }
                    });
                }
            });
            earlier_missing |= !ok;
        }
        if t.accessibility && t.screen_capture {
            ui.label(theme::hint(tr("Mac 앱을 조작하는 데 필요한 권한이 모두 켜져 있어요.")));
            return;
        }
        if restart {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(tr("화면 기록을 켰다면 GhostDesk 를 다시 시작해야 반영돼요.")).color(theme::warn()));
                if theme::secondary(ui, tr("다시 시작")).clicked() {
                    self.save_prefs();
                    crate::update::request_relaunch(ui.ctx());
                }
            });
        }
        ui.label(theme::hint(tr("스위치가 이미 켜져 있는데 여기서 꺼짐으로 보이면 업데이트로 앱 서명이 바뀐 거예요. '허용'을 누르면 옛 항목을 지우고 새로 요청해요.")));
    }

    /// 연결마다 카드 하나: 이름(OpenAI 호환은 고칠 수 있음) · 회사 · ✕, 그 아래 주소 · 키 칸
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
                    ui.label(theme::hint(tr(p.label())));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button(icon::X).on_hover_text(tr("이 AI 연결 지우기")).clicked() {
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
                if p.takes_key() {
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

    /// '+ AI 추가' — 아직 없는 회사와 OpenAI 호환(여러 개 가능) 중에서 골라 새 연결을 만든다
    fn add_conn_button(&mut self, ui: &mut egui::Ui) {
        let choices = ai::addable(&self.ai);
        ui.menu_button(format!("{}  {}", icon::PLUS, tr("AI 추가")), |ui| {
            ui.set_min_width(260.0);
            for p in choices {
                if ui.button(tr(p.label())).clicked() {
                    self.ai_next_id += 1;
                    let c = AiConn::new(self.ai_next_id, p);
                    // 처음 추가한 연결이면 바로 그것을 쓴다
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
