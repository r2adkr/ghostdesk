//! 설정 창(언어 · 지금 쓰는 AI · 회사별 API 키 · 화면)과 홈 화면의 모델 고르기.
//! 새 에이전트 창도 키·모델 칸을 같이 쓴다.

use super::*;
use super::labels::effort_label;

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

    /// `i` 번째 회사에서 쓸 수 있는 모델 목록을 그린다 (처음 열 때 API 에서 불러오고, 그동안은 기본 목록).
    /// 고른 모델을 돌려준다.
    fn model_list(&mut self, ui: &mut egui::Ui, i: usize) -> Option<String> {
        let p = Provider::ALL[i];
        let state = self.model_fetch.lock()[i].clone();
        if matches!(state, ModelFetch::Idle) && !(p.needs_key() && self.api_keys[i].trim().is_empty()) {
            self.model_fetch.lock()[i] = ModelFetch::Loading;
            let (slot, key, ctx) = (self.model_fetch.clone(), self.api_keys[i].clone(), ui.ctx().clone());
            self.rt.spawn(async move {
                let r = crate::llm::list_models(p, &key).await.map_err(|e| format!("{e:#}"));
                slot.lock()[i] = ModelFetch::Done(r);
                ctx.request_repaint();
            });
        }
        let fetched = match &state {
            ModelFetch::Loading => {
                ui.horizontal(|ui| {
                    theme::spinner(ui, 14.0, theme::muted());
                    ui.label(theme::hint(tr("모델 목록을 불러오는 중…")));
                });
                None
            }
            ModelFetch::Done(Ok(list)) if !list.is_empty() => Some(list.clone()),
            ModelFetch::Done(Err(e)) => {
                ui.label(RichText::new(crate::llm::short(e, 120)).size(12.0).color(theme::warn()));
                None
            }
            _ => None,
        };
        let list = fetched.unwrap_or_else(|| p.default_models().iter().map(|s| s.to_string()).collect());
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
        let label = format!("{}  {}  {}", self.provider.short_name(), self.models[i], icon::CHEVRON_DOWN);
        let button = egui::Button::new(RichText::new(label).size(13.0).color(theme::body())).frame_when_inactive(false).min_size(Vec2::new(0.0, 28.0));
        let (r, _) = egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
            ui.set_min_width(280.0);
            let mut chosen = None;
            for (j, p) in Provider::ALL.into_iter().enumerate() {
                if !self.ready(j) && p != self.provider {
                    continue;
                }
                ui.add_space(4.0);
                ui.label(theme::hint(tr(p.label())));
                // 자주 쓰는 모델 몇 개 + 지금 고른 모델 (목록에 없는 이름을 직접 적었어도 보이게)
                let mut models: Vec<String> = p.default_models().iter().map(|s| s.to_string()).collect();
                if !models.contains(&self.models[j]) {
                    models.insert(0, self.models[j].clone());
                }
                for m in models {
                    let sel = self.provider == p && self.models[j] == m;
                    if ui.selectable_label(sel, &m).clicked() {
                        chosen = Some((j, m));
                    }
                }
                egui::containers::menu::SubMenuButton::new(theme::hint(tr("다른 모델"))).ui(ui, |ui| {
                    ui.set_min_width(260.0);
                    if let Some(m) = self.model_list(ui, j) {
                        chosen = Some((j, m));
                    }
                });
            }
            ui.separator();
            if ui.button(format!("{}  {}", icon::SETTINGS, tr("AI 회사 · API 키 관리"))).clicked() {
                self.show_settings = true;
                ui.close();
            }
            if let Some((j, m)) = chosen {
                self.provider = Provider::ALL[j];
                self.models[j] = m;
                self.save_prefs();
                ui.close();
            }
        });
        r.on_hover_text(tr(self.provider.label()));
        if self.missing_key() && ui.link(RichText::new(tr("API 키 넣기")).size(12.0).color(theme::warn())).clicked() {
            self.show_settings = true;
        }
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
