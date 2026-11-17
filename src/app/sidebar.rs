//! Left sidebar: menu and agent list.

use super::*;
use super::detail::is_working;
use super::labels::{attention, display_status};

impl GhostApp {
    /// The left sidebar: name, new agent, views and the agent list, with Connections and Settings
    /// pinned to the bottom.
    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = 2.0;
        // when the app draws its own title bar, the name lives there (right above the sidebar)
        if !titlebar::visible(ui.ctx()) {
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(RichText::new(icon::GHOST).size(18.0).color(theme::text()));
                ui.label(RichText::new("GhostDesk").size(16.0).strong().color(theme::text()));
            });
            ui.add_space(16.0);
        }
        if theme::nav_item(ui, self.view == View::Home, icon::GHOST, tr("홈"), None).clicked() {
            self.view = View::Home;
        }
        if theme::nav_item(ui, false, icon::PLUS, tr("새 에이전트"), None)
            .on_hover_text(trf("에이전트가 일할 곳(웹사이트·{}·원격 컴퓨터)을 고르고 할 일을 맡겨요", &[&tr(text::LOCAL_APPS)]))
            .clicked()
        {
            self.open_wizard();
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
        // lay out the pinned bottom rows first; the agent list gets whatever height is left
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            if theme::nav_item(ui, false, icon::SETTINGS, tr("설정"), None)
                .on_hover_text(tr("AI 회사 · API 키 · 모델 고르기"))
                .clicked()
            {
                self.show_settings = true;
            }
            if self.auto_approve
                && theme::nav_item(ui, false, icon::SHIELD_ALERT, tr("권한 확인 꺼짐"), Some(theme::warn()))
                    .on_hover_text(tr("외부 에이전트의 요청을 묻지 않고 허용하고 있어요. 눌러서 설정 열기"))
                    .clicked()
            {
                self.open_settings(settings::SettingsTab::General);
            }
            // green dot when connected, red when the control server failed
            if theme::nav_item(ui, false, icon::PLUG, tr("외부 연결"), self.connect_badge())
                .on_hover_text(tr("Claude 앱 · Claude Code · Codex 에서 GhostDesk 쓰기 (키 필요 없음)"))
                .clicked()
            {
                self.open_connect(ui.ctx());
            }
            ui.add_space(4.0);
            // right below the menu: notice cards, then the agent list. Notices go in this top-down layout
            // because in the bottom-up one the card border would stretch to fill the remaining height.
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                self.update_notice(ui);
                self.permission_notice(ui);
                if !self.agents.is_empty() {
                    ui.add_space(20.0);
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.agent_list(ui));
                }
            });
        });
    }

    /// macOS permission card below the menu, shown whenever a permission needed for Mac apps is
    /// off. "Permissions" opens Settings, which keeps that section on top while anything is missing.
    fn permission_notice(&mut self, ui: &mut egui::Ui) {
        if self.perm_notice_hidden || self.permissions_ok(ui.ctx()) {
            return;
        }
        #[cfg(target_os = "macos")]
        let restart = crate::macos::screen_capture_needs_restart();
        #[cfg(not(target_os = "macos"))]
        let restart = false;
        ui.add_space(8.0);
        theme::tinted(theme::warn()).show(ui, |ui| {
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 6.0;
                ui.label(RichText::new(tr("Mac 앱 권한이 꺼져 있어요")).strong().color(theme::text()));
                if restart {
                    ui.label(theme::hint(tr("화면 기록을 켰다면 GhostDesk 를 다시 시작해야 반영돼요")));
                } else {
                    ui.label(theme::hint(tr("이 컴퓨터의 앱을 맡기려면 필요해요")));
                }
                ui.horizontal(|ui| {
                    if restart {
                        if theme::primary(ui, tr("다시 시작")).clicked() {
                            self.save_prefs();
                            crate::update::request_relaunch(ui.ctx());
                        }
                    } else if theme::primary(ui, tr("권한 설정")).clicked() {
                        self.open_settings(settings::SettingsTab::Permissions);
                    }
                    if ui.link(theme::hint(tr("나중에"))).on_hover_text(tr("다음에 켤 때 다시 알려 드려요")).clicked() {
                        self.perm_notice_hidden = true;
                    }
                });
            });
        });
    }

    /// Update card below the menu: notice → download (progress bar) → restart to apply.
    fn update_notice(&mut self, ui: &mut egui::Ui) {
        use crate::update::State;
        let st = self.update.lock().clone();
        let r = match &st {
            State::None => return,
            State::Available(r) | State::Downloading(r, _) | State::Ready(r) | State::Failed(r, _) => r.clone(),
        };
        ui.add_space(8.0);
        theme::tinted(theme::info()).show(ui, |ui| {
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
                                // this OS can't swap the app in place; download it from the release page instead
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
            theme::count_badge(ui, &self.agents.len().to_string());
        });
        ui.add_space(2.0);
        let mut clicked = None;
        for (i, a) in self.agents.iter().enumerate() {
            let st = a.shared.status();
            let paused = a.shared.paused.load(Ordering::Relaxed);
            let (status, _) = display_status(&st, paused, a.takeover);
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
                        // place the right-hand bits (status dot, external marker) first, then truncate the name to fit
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            match (&st, a.takeover, paused, attention(&st, a.takeover)) {
                                (_, true, _, _) => {
                                    theme::chip(ui, theme::takeover(), tr("직접 조작")).on_hover_text(&status);
                                }
                                (_, false, true, _) => {
                                    theme::chip(ui, theme::muted(), tr("일시정지")).on_hover_text(&status);
                                }
                                (AgentStatus::WaitingUser(_), _, _, _) => {
                                    theme::chip(ui, theme::warn(), tr("질문")).on_hover_text(&status);
                                }
                                (_, _, _, Some(c)) => {
                                    theme::dot(ui, c, 3.5).on_hover_text(&status);
                                }
                                // small spinner while working (nothing while paused)
                                _ if is_working(&st) => {
                                    theme::spinner(ui, 13.0, theme::muted()).on_hover_text(&status);
                                }
                                _ => {}
                            }
                            if external {
                                ui.label(theme::hint(tr("외부")));
                            }
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.add(egui::Label::new(RichText::new(&a.shared.name).color(theme::text())).truncate().selectable(false)).on_hover_text(&a.shared.name);
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
            theme::focus_outline(ui, &r, theme::CARD_RADIUS);
            let r = r.on_hover_text(format!("{}\n{status}", a.shared.name));
            // fade the border in on hover
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
