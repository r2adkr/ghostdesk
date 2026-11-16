//! 왼쪽 사이드바: 메뉴와 에이전트 목록.

use super::*;
use super::detail::is_working;
use super::labels::{attention, status_text};

impl GhostApp {
    /// 왼쪽 사이드바: 이름 · 새 에이전트 · 보기 · 에이전트 목록, 아래쪽에 외부 연결 · 설정.
    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = 2.0;
        // 앱이 제목 표시줄을 그리면 이름은 그 자리(사이드바 바로 위)에 있다
        if !titlebar::visible(ui.ctx()) {
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(RichText::new(icon::GHOST).size(18.0).color(theme::text()));
                ui.label(RichText::new("GhostDesk").size(16.0).strong().color(theme::text()));
            });
            ui.add_space(16.0);
        }
        if theme::nav_item(ui, false, icon::PLUS, tr("새 에이전트"), None)
            .on_hover_text(trf("에이전트가 일할 곳(웹사이트·{}·원격 컴퓨터)을 고르고 할 일을 맡겨요", &[&tr(text::LOCAL_APPS)]))
            .clicked()
        {
            self.open_wizard();
        }
        if theme::nav_item(ui, self.view == View::Home, icon::GHOST, tr("홈"), None).clicked() {
            self.view = View::Home;
        }
        if theme::nav_item(ui, self.view == View::Grid, icon::LAYOUT_GRID, tr("모든 에이전트"), None).clicked() {
            self.view = View::Grid;
        }
        if !self.agents.is_empty()
            && theme::nav_item(ui, false, icon::LAYOUT_DASHBOARD, tr("화면 모아보기"), None)
                .on_hover_text(pick(
                    tr("모든 에이전트 화면을 한곳에 크게 봐요 (⌃⌘F 전체 화면 · Esc 닫기)"),
                    tr("모든 에이전트 화면을 한곳에 크게 봐요 (F11 전체 화면 · Esc 닫기)"),
                ))
                .clicked()
        {
            self.enter_wall(ui.ctx());
        }
        // 아래쪽 고정 줄을 먼저 놓고, 남은 높이에 에이전트 목록
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            if theme::nav_item(ui, false, icon::SETTINGS, tr("설정"), None)
                .on_hover_text(tr("AI 회사 · API 키 · 모델 고르기"))
                .clicked()
            {
                self.show_settings = true;
            }
            // 외부 연결: 연결돼 있으면 초록 점, 연결 서버 오류면 빨간 점
            if theme::nav_item(ui, false, icon::PLUG, tr("외부 연결"), self.connect_badge())
                .on_hover_text(tr("Claude 앱 · Claude Code · Codex 에서 GhostDesk 쓰기 (키 필요 없음)"))
                .clicked()
            {
                self.open_connect(ui.ctx());
            }
            self.update_notice(ui);
            ui.add_space(4.0);
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                if !self.agents.is_empty() {
                    ui.add_space(20.0);
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.agent_list(ui));
                }
            });
        });
    }

    /// 새 버전 알림 카드 (외부 연결 위): 알림 → 내려받기(진행 막대) → 다시 시작해서 적용
    fn update_notice(&mut self, ui: &mut egui::Ui) {
        use crate::update::State;
        let st = self.update.lock().clone();
        let r = match &st {
            State::None => return,
            State::Available(r) | State::Downloading(r, _) | State::Ready(r) | State::Failed(r, _) => r.clone(),
        };
        ui.add_space(8.0);
        theme::tinted(theme::info()).show(ui, |ui| {
            // 사이드바 아래쪽은 아래에서 위로 쌓이므로 카드 안은 위에서 아래로 다시 놓는다
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 6.0;
                ui.label(RichText::new(trf("새 버전 {} 이 나왔어요", &[&r.version])).strong().color(theme::text()));
                match &st {
                    State::Downloading(_, f) => {
                        ui.horizontal(|ui| {
                            theme::spinner(ui, 13.0, theme::muted());
                            ui.label(theme::hint(trf("내려받는 중… {}%", &[&((f * 100.0) as u32)])));
                        });
                        theme::progress(ui, egui::Id::unique("update_progress"), *f, ui.available_width());
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
                            ui.label(RichText::new(trf("업데이트 실패: {}", &[&crate::llm::short(e, 120)])).size(12.0).color(theme::warn()));
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
                                // 앱 안에서 바꿔 넣을 수 없는 OS: 릴리스 페이지에서 직접 받는다
                                ui.hyperlink_to(RichText::new(tr("내려받기")).size(12.0), &r.page);
                            }
                        });
                    }
                }
            });
        });
    }

    pub(super) fn agent_list(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            theme::section(ui, tr("에이전트"));
            egui::Frame::new().fill(theme::card()).corner_radius(255).inner_margin(Margin::symmetric(7, 1)).show(ui, |ui| {
                ui.label(RichText::new(self.agents.len().to_string()).size(11.0).color(theme::muted()));
            });
        });
        ui.add_space(2.0);
        let mut clicked = None;
        for (i, a) in self.agents.iter().enumerate() {
            let st = a.shared.status();
            let sel = self.view == View::Detail && self.selected == Some(i);
            let fill = if sel { theme::card_hover() } else { Color32::TRANSPARENT };
            let row_id = egui::Id::unique(("agent_row", a.shared.id));
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
                                    theme::chip(ui, theme::warn(), tr("질문")).on_hover_text(status_text(&st));
                                }
                                (_, Some(c)) => {
                                    theme::dot(ui, c, 3.5).on_hover_text(if a.takeover { tr("직접 조작 중").to_string() } else { status_text(&st) });
                                }
                                // 일하는 중: 작게 도는 표시 (멈춰 있으면 표시 없음)
                                _ if is_working(&st) && !a.shared.paused.load(Ordering::Relaxed) => {
                                    theme::spinner(ui, 13.0, theme::muted()).on_hover_text(status_text(&st));
                                }
                                _ => {}
                            }
                            if external {
                                ui.label(theme::hint(tr("외부")));
                            }
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.add(egui::Label::new(RichText::new(&a.shared.name).color(theme::text())).truncate().selectable(false));
                            });
                        });
                    });
                    if q > 0 {
                        ui.horizontal(|ui| {
                            ui.add_space(14.0);
                            ui.label(theme::hint(trf("대기 중인 작업 {}개", &[&q])));
                        });
                    }
                })
                .response
                .interact(egui::Sense::click());
            r.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, sel, trf("{} 열기", &[&a.shared.name])));
            // 마우스를 올리면 테두리가 서서히 나타난다
            let h = ui.ctx().animate_bool_with_time(row_id, r.hovered() && !sel, 0.12);
            if h > 0.0 {
                ui.painter().rect_stroke(r.rect, theme::CARD_RADIUS, Stroke::new(1.0, theme::border().gamma_multiply(h)), StrokeKind::Inside);
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
}
