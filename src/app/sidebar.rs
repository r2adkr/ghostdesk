//! Left sidebar: menu and agent list.

use super::*;
use super::detail::is_working;
use super::labels::{attention, display_status};

impl GhostApp {
    /// The left sidebar: views and the agent list, with Connections and Settings
    /// pinned to the bottom.
    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui) {
        // holding ⌘ (Ctrl) shows each row's shortcut, like a menu
        let held = ui.input(|i| i.modifiers.command);
        let key = |k: &'static str| -> Option<String> { held.then(|| palette::cmd_key(k)) };
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
        // quick actions first: start something (wide button), find something and fold the sidebar (icons)
        let folds = !titlebar::visible(ui.ctx());
        // icons flush with the right edge (in line with the row badges below), the wide button takes what is left;
        // laid out right to left so the row is never wider than the panel, whatever size the icons are
        // a fixed 32pt height: a bare right-to-left layout would take (and center in) all the height left
        ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 32.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if folds && theme::icon_button_keys(ui, icon::PANEL_LEFT, key("B").as_deref(), &format!("{} ({})", tr("사이드바 접기"), palette::cmd_key("B"))).clicked() {
                self.sidebar_open = false;
            }
            if theme::icon_button_keys(ui, icon::SEARCH, key("K").as_deref(), &format!("{} ({})", tr("찾기"), palette::cmd_key("K"))).clicked() {
                self.open_palette();
            }
            ui.add_space(4.0);
            if theme::nav_item_keys(ui, false, icon::PLUS, tr("새 에이전트"), None, key("N").as_deref()).on_hover_text(palette::cmd_key("N")).clicked() {
                self.open_wizard();
            }
        });
        ui.add_space(6.0);
        if theme::nav_item_keys(ui, self.view == View::Home, icon::GHOST, tr("홈"), None, key("1").as_deref()).clicked() {
            self.view = View::Home;
        }
        if theme::nav_item_keys(ui, self.view == View::Grid, icon::LAYOUT_GRID, tr("모든 에이전트"), None, key("2").as_deref()).clicked() {
            self.view = View::Grid;
        }
        // teams and usage are the less used views: folded away unless one is open or a team is active
        let active = self.teams_ui_active();
        let in_more = matches!(self.view, View::Teams | View::Usage);
        let more_open = self.more_views_open || in_more || active;
        if !more_open {
            if theme::nav_item(ui, false, icon::ELLIPSIS, tr("더 보기"), None).on_hover_text(tr("팀 · 사용량")).clicked() {
                self.more_views_open = true;
            }
        } else {
            // green dot while any agent is in a team
            if theme::nav_item_keys(ui, self.view == View::Teams, icon::LIST_TODO, tr("팀"), active.then(theme::success), key("3").as_deref())
                .on_hover_text(tr("여러 에이전트가 같은 목표·메모·할 일을 보며 함께 일해요"))
                .clicked()
            {
                self.view = View::Teams;
            }
            if theme::nav_item_keys(ui, self.view == View::Usage, icon::CHART_COLUMN, tr("사용량"), None, key("4").as_deref())
                .on_hover_text(tr("에이전트가 쓴 토큰·동작·화면을 날짜별로 봐요"))
                .clicked()
            {
                self.view = View::Usage;
            }
        }
        // lay out the pinned bottom rows first; the agent list gets whatever height is left
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            // same shape as the top row: the icon flush right, Settings takes the rest
            ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 32.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                // green dot when connected, red when the control server failed
                let r = theme::icon_button(ui, icon::PLUG, tr("Claude 앱 · Claude Code · Codex 에서 GhostDesk 쓰기 (키 필요 없음)"));
                if let Some(c) = self.connect_badge() {
                    ui.painter().circle_filled(r.rect.right_top() + Vec2::new(-8.0, 8.0), 3.5, c);
                }
                if r.on_hover_text(tr("외부 연결")).clicked() {
                    self.open_connect(ui.ctx());
                }
                ui.add_space(4.0);
                if theme::nav_item_keys(ui, false, icon::SETTINGS, tr("설정"), None, key(",").as_deref()).clicked() {
                    self.open_settings(settings::SettingsTab::General);
                }
            });
            if self.auto_approve
                && theme::nav_item(ui, false, icon::SHIELD_ALERT, tr("권한 확인 꺼짐"), Some(theme::warn()))
                    .on_hover_text(tr("외부 에이전트의 요청을 묻지 않고 허용하고 있어요. 눌러서 설정 열기"))
                    .clicked()
            {
                self.open_settings(settings::SettingsTab::Permissions);
            }
            // a footer of its own, set apart from the scrolling list (as in Claude and Codex)
            ui.add_space(2.0);
            ui.separator();
            ui.add_space(2.0);
            // Right below the menu: agent list and notices. Notices go in this top-down layout
            // because in the bottom-up one the card border would stretch to fill the remaining height.
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.separator();
                egui::ScrollArea::vertical().id_salt("sidebar_activity").auto_shrink([false, false]).show(ui, |ui| {
                    if !self.agents.is_empty() {
                        ui.add_space(12.0);
                        self.agent_list(ui);
                    }
                    self.permission_notice(ui);
                });
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

    pub(super) fn agent_list(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            theme::section(ui, tr("에이전트"));
            theme::count_badge(ui, &self.agents.len().to_string());
        });
        ui.add_space(2.0);
        let mut clicked = None;
        let badges = self.team_badges();
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
                            // a team member's screen shows its team instead of just "external"
                            match badges.get(&a.shared.id) {
                                Some((_, team)) => {
                                    ui.label(RichText::new(format!("{} {}", icon::LIST_TODO, crate::llm::short(team, 14))).size(12.0).color(theme::info_text()))
                                        .on_hover_text(team.as_str());
                                }
                                None if external => {
                                    ui.label(theme::hint(tr("외부")));
                                }
                                None => {}
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
