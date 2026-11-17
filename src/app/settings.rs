//! 설정 창(언어 · 지금 쓰는 AI · 회사별 API 키 · 화면)과 홈 화면의 모델 고르기.
//! 새 에이전트 창도 키·모델 칸을 같이 쓴다.

use super::*;
use super::labels::{effort_label, effort_short};

impl GhostApp {
    /// 지금 고른 AI 회사의 순서 (`api_keys`·`models` 의 칸)
    pub(super) fn pi(&self) -> usize {
        Provider::ALL.iter().position(|p| *p == self.provider).unwrap_or(0)
    }

    pub(super) fn key(&self) -> &str {
        &self.api_keys[self.pi()]
    }

    /// 키가 필요한 회사인데 키가 비어 있는가
    pub(super) fn missing_key(&self) -> bool {
        self.provider.needs_key() && self.key().trim().is_empty()
    }

    /// 바로 쓸 수 있게 준비된 회사인가: 키가 있거나, 키가 필요 없는데(Ollama) 지금 쓰고 있는 회사
    fn ready(&self, i: usize) -> bool {
        let p = Provider::ALL[i];
        if p.needs_key() {
            !self.api_keys[i].trim().is_empty()
        } else {
            p == self.provider
        }
    }

    pub(super) fn provider_combo(&mut self, ui: &mut egui::Ui, width: f32) {
        egui::ComboBox::from_id_salt(ui.scope_id().with("provider")).width(width).selected_text(tr(self.provider.label())).show_ui(ui, |ui| {
            for p in Provider::ALL {
                ui.selectable_value(&mut self.provider, p, tr(p.label()));
            }
        });
    }

    /// 지금 회사의 API 키 칸 (새 에이전트 창)
    pub(super) fn key_field(&mut self, ui: &mut egui::Ui, width: f32, id: egui::Id) -> egui::Response {
        self.key_field_for(ui, self.pi(), width, id)
    }

    /// `i` 번째 회사의 API 키 칸. 키가 바뀌면 그 회사의 모델 목록을 새 키로 다시 불러온다.
    fn key_field_for(&mut self, ui: &mut egui::Ui, i: usize, width: f32, id: egui::Id) -> egui::Response {
        let p = Provider::ALL[i];
        let r = ui.add_enabled(
            p.needs_key(),
            egui::TextEdit::singleline(&mut self.api_keys[i])
                .id(id)
                .password(true)
                .desired_width(width)
                .margin(Margin::symmetric(10, 6))
                .hint_text(tr(p.key_hint())),
        );
        if r.changed() {
            self.model_fetch.lock()[i] = ModelFetch::Idle;
        }
        r
    }

    /// `i` 번째 회사의 모델 목록을 아직 안 불러왔으면 API 에서 불러오기 시작한다 (키가 필요한데 없으면 하지 않음).
    fn fetch_models(&mut self, ctx: &egui::Context, i: usize) -> ModelFetch {
        let p = Provider::ALL[i];
        let state = self.model_fetch.lock()[i].clone();
        if matches!(state, ModelFetch::Idle) && !(p.needs_key() && self.api_keys[i].trim().is_empty()) {
            self.model_fetch.lock()[i] = ModelFetch::Loading;
            let (slot, key, ctx) = (self.model_fetch.clone(), self.api_keys[i].clone(), ctx.clone());
            self.rt.spawn(async move {
                let r = crate::llm::list_models(p, &key).await.map_err(|e| format!("{e:#}"));
                slot.lock()[i] = ModelFetch::Done(r);
                ctx.request_repaint();
            });
            return ModelFetch::Loading;
        }
        state
    }

    /// `i` 번째 회사에서 고를 수 있는 모델: API 에서 불러온 전체 목록 (불러오기 전·실패 때는 기본 목록).
    /// 지금 고른 모델은 목록에 없어도(이름을 직접 적었어도) 맨 앞에 넣는다.
    fn models_for(&self, i: usize, state: &ModelFetch) -> Vec<String> {
        let mut list: Vec<String> = match state {
            ModelFetch::Done(Ok(list)) if !list.is_empty() => list.clone(),
            _ => Provider::ALL[i].default_models().iter().map(|s| s.to_string()).collect(),
        };
        let cur = self.models[i].trim();
        if !cur.is_empty() && !list.iter().any(|m| m == cur) {
            list.insert(0, cur.to_string());
        }
        list
    }

    /// 불러오는 중이면 작은 표시. 실패했으면 무슨 일인지와 '다시 시도' (API 원문 오류는 글에 마우스를 올리면).
    fn fetch_status(&mut self, ui: &mut egui::Ui, i: usize, state: &ModelFetch) {
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
                        self.model_fetch.lock()[i] = ModelFetch::Idle;
                    }
                });
            }
            _ => {}
        }
    }

    /// `i` 번째 회사에서 쓸 수 있는 모델 목록을 그린다 (설정 창). 고른 모델을 돌려준다.
    fn model_list(&mut self, ui: &mut egui::Ui, i: usize) -> Option<String> {
        let p = Provider::ALL[i];
        let state = self.fetch_models(ui.ctx(), i);
        self.fetch_status(ui, i, &state);
        let list = self.models_for(i, &state);
        let mut picked = None;
        egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
            for m in list {
                let sel = self.provider == p && self.models[i] == m;
                if ui.selectable_label(sel, &m).clicked() {
                    picked = Some(m);
                }
            }
        });
        picked
    }

    /// 모델 이름 칸 + ▾ 목록 (설정 창)
    pub(super) fn model_field(&mut self, ui: &mut egui::Ui, width: f32) {
        let i = self.pi();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.add(egui::TextEdit::singleline(&mut self.models[i]).desired_width(width - 40.0).margin(Margin::symmetric(10, 6)));
            ui.menu_button(icon::CHEVRON_DOWN, |ui| {
                ui.set_min_width(260.0);
                if let Some(m) = self.model_list(ui, i) {
                    self.models[i] = m;
                    ui.close();
                }
            })
            .response
            .on_hover_text(tr("쓸 수 있는 모델 보기"));
        });
    }

    /// 홈 입력 상자 아래의 모델 단추: 키를 넣어 둔 회사의 모델을 한 목록에서 골라 바로 바꾼다.
    pub(super) fn model_picker(&mut self, ui: &mut egui::Ui) {
        let i = self.pi();
        // 회사 이름(흐리게) · 모델 이름 · ▾ 를 한 줄 가운데에 맞춰 놓는다 (아이콘 글꼴은 기준선이 달라 가운데 정렬이 필요)
        let mut label = egui::text::LayoutJob::default();
        let part = |size: f32, color: Color32| egui::TextFormat {
            font_id: egui::FontId::proportional(size),
            color,
            valign: egui::Align::Center,
            ..Default::default()
        };
        label.append(self.provider.short_name(), 0.0, part(13.0, theme::muted()));
        label.append(&self.models[i], 6.0, part(13.0, theme::body()));
        if self.provider.supports_effort() {
            label.append(&format!("· {}", effort_short(EFFORTS[self.effort_idx])), 6.0, part(13.0, theme::muted()));
        }
        label.append(icon::CHEVRON_DOWN, 6.0, part(12.0, theme::muted()));
        let button = egui::Button::new(label).frame_when_inactive(false).min_size(Vec2::new(0.0, 28.0));
        let (r, _) = egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
            // 폭을 고정한다 (검색 칸이 남는 폭을 다 차지하려 해서 메뉴가 화면만큼 넓어지지 않게)
            ui.set_width(340.0);
            // 머리글·검색 글·모델 이름을 모두 메뉴 줄의 글 시작점(단추 안쪽 여백)에 맞춘다
            // (메뉴는 기본 안쪽 여백이 2px 라 선택 배경 끝에 글이 붙으므로 넓힌다)
            ui.spacing_mut().button_padding.x = 10.0;
            let pad = ui.spacing().button_padding.x;
            let mut chosen = None;
            // 키를 넣어 둔(연결된) 모든 회사의 모델을 API 에서 불러와 한 목록에 다 보여 준다. 많으면 검색으로 좁힌다.
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
            let mut first: Option<(usize, String)> = None;
            let mut shown = 0;
            egui::ScrollArea::vertical().max_height(420.0).auto_shrink([false, true]).show(ui, |ui| {
                for (j, p) in Provider::ALL.into_iter().enumerate() {
                    if !self.ready(j) && p != self.provider {
                        continue;
                    }
                    let state = self.fetch_models(ui.ctx(), j);
                    let models: Vec<String> =
                        self.models_for(j, &state).into_iter().filter(|m| query.is_empty() || m.to_lowercase().contains(&query)).collect();
                    if models.is_empty() && !matches!(state, ModelFetch::Loading | ModelFetch::Done(Err(_))) {
                        continue;
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space(pad);
                        ui.label(theme::hint(tr(p.label())));
                        if let ModelFetch::Done(Ok(all)) = &state {
                            ui.label(theme::hint(format!("· {}", all.len())));
                        }
                    });
                    if matches!(state, ModelFetch::Loading | ModelFetch::Done(Err(_))) {
                        ui.horizontal(|ui| {
                            ui.add_space(pad);
                            ui.vertical(|ui| self.fetch_status(ui, j, &state));
                        });
                    }
                    ui.add_space(2.0);
                    for m in models {
                        shown += 1;
                        if first.is_none() {
                            first = Some((j, m.clone()));
                        }
                        let sel = self.provider == p && self.models[j] == m;
                        if ui.selectable_label(sel, &m).clicked() {
                            chosen = Some((j, m));
                        }
                    }
                }
                if shown == 0 && !query.is_empty() {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space(pad);
                        ui.label(theme::hint(tr("맞는 모델이 없어요")));
                    });
                }
            });
            if enter && chosen.is_none() && !query.is_empty() {
                chosen = first;
            }
            if self.provider.supports_effort() {
                ui.separator();
                self.effort_row(ui, pad);
            }
            ui.separator();
            if ui.button(format!("{}  {}", icon::SETTINGS, tr("AI 회사 · API 키 관리"))).clicked() {
                self.show_settings = true;
                ui.close();
            }
            if let Some((j, m)) = chosen {
                self.provider = Provider::ALL[j];
                self.models[j] = m;
                self.model_query.clear();
                self.save_prefs();
                ui.close();
            }
        });
        r.on_hover_text(tr(self.provider.label()));
    }

    /// 모델 메뉴 안의 '생각 깊이' 줄: 낮음 … 최대 중 하나를 바로 고른다 (메뉴는 닫지 않는다)
    fn effort_row(&mut self, ui: &mut egui::Ui, pad: f32) {
        ui.horizontal(|ui| {
            ui.add_space(pad);
            ui.label(theme::hint(tr("생각 깊이")));
        });
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.spacing_mut().button_padding.x = 8.0;
            ui.add_space(pad - 8.0);
            for (i, e) in EFFORTS.iter().enumerate() {
                if ui.selectable_label(self.effort_idx == i, effort_short(e)).on_hover_text(effort_label(e)).clicked() && self.effort_idx != i {
                    self.effort_idx = i;
                    self.save_prefs();
                }
            }
        })
        .response
        .on_hover_text(tr("높을수록 꼼꼼하지만 느리고 비용이 더 듭니다"));
    }

    /// 키를 어디서 받는지 · 어디에 두는지 한 줄로 (새 에이전트 창)
    pub(super) fn key_help(&self, ui: &mut egui::Ui) {
        let p = self.provider;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if p.needs_key() {
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

    /// ⚙ 설정: 언어 · 지금 쓰는 AI · 회사별 API 키 · 화면
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

                ui.add_space(18.0);
                theme::section(ui, tr("지금 쓰는 AI"));
                ui.add_space(4.0);
                egui::Grid::new("settings_ai").num_columns(2).min_col_width(LABEL).spacing([14.0, 10.0]).show(ui, |ui| {
                    ui.label(tr("AI 회사"));
                    self.provider_combo(ui, field);
                    ui.end_row();
                    ui.label(tr("모델"));
                    self.model_field(ui, field);
                    ui.end_row();
                    ui.label(tr("생각 깊이"));
                    egui::ComboBox::from_id_salt("effort").width(field).selected_text(effort_label(EFFORTS[self.effort_idx])).show_ui(ui, |ui| {
                        for (i, e) in EFFORTS.iter().enumerate() {
                            ui.selectable_value(&mut self.effort_idx, i, effort_label(e));
                        }
                    })
                    .response
                    .on_hover_text(tr("높을수록 꼼꼼하지만 느리고 비용이 더 듭니다"));
                    ui.end_row();
                });

                ui.add_space(18.0);
                theme::section(ui, tr("API 키"));
                ui.label(theme::hint(tr("여러 회사의 키를 넣어 두면 홈 화면의 모델 단추에서 바로 바꿔 쓸 수 있어요.")));
                ui.add_space(4.0);
                self.key_rows(ui, LABEL, field);
                ui.add_space(4.0);
                if prefs::CAN_REMEMBER_KEYS {
                    ui.add(theme::switch(&mut self.remember_keys, tr("키를 이 PC 에 기억하기")))
                        .on_hover_text(tr("Windows 계정으로 암호화해 저장해요. 다른 계정이나 다른 PC 에서는 풀 수 없어요."));
                } else {
                    ui.label(theme::hint(self.key_storage_note()));
                }

                ui.add_space(18.0);
                theme::section(ui, tr("화면"));
                ui.add_space(4.0);
                ui.add(theme::switch(&mut self.show_cursor, tr("가상 마우스 보이기")))
                    .on_hover_text(tr("에이전트가 어디를 누르는지 화면에 보여 줍니다 (내가 직접 조작할 때는 주황색)"));
                if ti + to > 0 {
                    ui.add_space(6.0);
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

    /// 회사마다 한 줄: 이름 · 키 칸 · (들어 있으면 ✓, 없으면 '키 받기')
    fn key_rows(&mut self, ui: &mut egui::Ui, label_w: f32, field: f32) {
        let link_w = 64.0;
        egui::Grid::new("settings_keys").num_columns(2).min_col_width(label_w).spacing([14.0, 8.0]).show(ui, |ui| {
            for (i, p) in Provider::ALL.into_iter().enumerate() {
                ui.label(p.short_name()).on_hover_text(tr(p.label()));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if p.needs_key() {
                        self.key_field_for(ui, i, field - link_w - 8.0, egui::Id::unique(("settings_key", i)));
                        if self.api_keys[i].trim().is_empty() {
                            ui.hyperlink_to(RichText::new(tr("키 받기")).size(12.0), p.key_page());
                        } else {
                            ui.label(RichText::new(icon::CIRCLE_CHECK).color(theme::success())).on_hover_text(tr("키가 들어 있어요"));
                        }
                    } else {
                        ui.label(theme::hint(tr("키가 필요 없어요 (내 PC 에서 실행)")));
                        ui.hyperlink_to(RichText::new(tr("내려받기")).size(12.0), p.key_page());
                    }
                });
                ui.end_row();
            }
        });
    }
}
