//! Home screen and the screen wall (grid and full-screen wall).

use super::*;
use super::detail::preparing;
use super::labels::{attention, display_status, humanize_error, is_error};

impl GhostApp {
    /// Opens the screen wall.
    pub(super) fn enter_wall(&mut self, ctx: &egui::Context) {
        self.wall_fs_before = ctx.input(|i| i.viewport().fullscreen).unwrap_or(false);
        self.view = View::Wall;
    }

    /// Closes the wall and switches to another view, leaving full screen if the wall entered it.
    pub(super) fn leave_wall(&mut self, ctx: &egui::Context, to: View) {
        // only leave full screen if the wall turned it on (keep it if it was already full screen)
        if self.wall_fullscreen && !self.wall_fs_before {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
        self.wall_fullscreen = false;
        self.wall_team = None;
        self.view = to;
    }

    /// For each agent id: the team it works in and its role there ("제주 출장 · 조사"), from the
    /// membership identity of the agent that owns the screen.
    pub(super) fn team_badges(&mut self) -> std::collections::HashMap<u64, (u64, String)> {
        self.teams_ui_active();
        let mut out = std::collections::HashMap::new();
        for a in &self.agents {
            let conn = a.shared.team_member_id.load(Ordering::Relaxed);
            if conn == 0 {
                continue;
            }
            if let Some((t, m)) = self.teams_ui.cache().iter().find_map(|t| t.members.iter().find(|m| m.conn == conn).map(|m| (t, m))) {
                let label = if m.role.is_empty() { t.name.clone() } else { format!("{} · {}", t.name, m.role) };
                out.insert(a.shared.id, (t.id, label));
            }
        }
        out
    }

    pub(super) fn welcome(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width().min(800.0);
        egui::ScrollArea::vertical().id_salt("home").auto_shrink([false, false]).show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.allocate_ui_with_layout(Vec2::new(width, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(width);
                    // with nothing else on the page the composer sits a little above the middle, not under the title bar
                    ui.add_space(if self.agents.is_empty() { (ui.ctx().content_rect().height() * 0.12).clamp(24.0, 120.0) } else { 24.0 });
                    ui.heading(RichText::new(tr("어떤 일을 맡길까요?")).size(28.0).strong());
                    ui.add_space(16.0);
                    let id = egui::Id::unique("welcome_composer");
                    let focused = ui.memory(|m| m.has_focus(id));
                    let focus = ui.ctx().animate_bool_with_time(id.with("focus_t"), focused, 0.15);
                    let mut next = false;
                    theme::composer_frame(focus).show(ui, |ui| {
                        ui.set_width(width - 32.0);
                        let edit = ui.add(egui::TextEdit::multiline(&mut self.form.task)
                            .id(id).hint_text(tr("에이전트에게 맡길 일을 적어 보세요"))
                            .frame(egui::Frame::NONE).desired_rows(3).desired_width(f32::INFINITY)
                            .return_key(Some(egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter))));
                        // typing starts right away when nothing else holds the keyboard
                        if !self.wizard_open && self.tour.is_none() && !self.show_settings && ui.memory(|m| m.focused().is_none()) {
                            edit.request_focus();
                        }
                        next = edit.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if self.routing_active() {
                                    theme::spinner(ui, 14.0, theme::muted());
                                    ui.label(theme::hint(tr("어디서 일할지 정하는 중…")));
                                } else {
                                    next |= theme::primary(ui, tr("시작하기")).on_hover_text("Enter").clicked();
                                }
                                // quiet hint while typing; fades with focus so it doesn't crowd the idle card
                                if focus > 0.01 {
                                    ui.add_space(4.0);
                                    ui.label(theme::hint(tr("Enter 시작 · Shift+Enter 줄바꿈")).color(theme::muted().gamma_multiply(focus)));
                                }
                                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| self.model_picker(ui));
                            });
                        });
                    });
                    if next && !self.routing_active() {
                        // the AI picks where to work from the request; the dialog only opens when that isn't possible
                        self.open_wizard();
                        if self.begin_route(ui.ctx()) {
                            self.wizard_open = false;
                        }
                    }
                    // tasks given before come back with one click; examples only until there are any
                    if self.form.task.trim().is_empty() && !self.recent_tasks.is_empty() {
                        ui.add_space(14.0);
                        ui.horizontal(|ui| {
                            ui.label(theme::hint(tr("최근에 맡긴 일")));
                            // a quiet action at the far edge, apart from the label it acts on
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.link(theme::hint(tr("기록 지우기")).color(theme::info())).on_hover_text(tr("최근에 맡긴 일 목록을 이 컴퓨터에서 지워요")).clicked() {
                                    self.recent_tasks.clear();
                                    self.save_prefs();
                                }
                            });
                        });
                        ui.add_space(4.0);
                        let mut pick = None;
                        ui.horizontal_wrapped(|ui| {
                            for task in &self.recent_tasks {
                                // one line per chip: a long task is cut, the whole text is in the tooltip
                                let first = task.lines().next().unwrap_or_default();
                                if theme::secondary(ui, &crate::llm::short(first, 40)).on_hover_text(task).clicked() {
                                    pick = Some(task.clone());
                                }
                            }
                        });
                        if let Some(task) = pick {
                            self.form.task = task;
                            ui.memory_mut(|m| m.request_focus(id));
                        }
                    } else if self.form.task.trim().is_empty() {
                        ui.add_space(14.0);
                        ui.label(theme::hint(tr("이런 일을 맡겨 볼 수 있어요")));
                        ui.add_space(4.0);
                        ui.horizontal_wrapped(|ui| {
                            for example in [
                                tr("네이버에서 오늘 서울 날씨를 찾아 요약해줘"),
                                tr("메모 앱에 이번 주 장보기 목록을 만들어줘"),
                                tr("구글 캘린더에 내일 오후 3시 회의를 추가해줘"),
                            ] {
                                if theme::secondary(ui, example).clicked() {
                                    self.form.task = example.to_string();
                                }
                            }
                        });
                    }
                    ui.add_space(24.0);
                    theme::section(ui, tr("어디서 일할지 직접 고르기"));
                    ui.add_space(6.0);
                    let places: Vec<_> = wizard::kinds().into_iter().filter(|k| matches!(k.choice, Choice::Web | Choice::App | Choice::Remote)).collect();
                    let gap = 8.0;
                    let tile_w = (width - gap * (places.len() - 1) as f32) / places.len() as f32;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        for place in places {
                            if theme::tile(ui, false, place.icon, place.title, Vec2::new(tile_w, 48.0)).on_hover_text(place.about).clicked() {
                                self.open_wizard();
                                self.form.choice = Some(place.choice);
                            }
                        }
                    });
                    if !self.agents.is_empty() {
                        ui.add_space(32.0);
                        ui.horizontal(|ui| {
                            theme::section(ui, tr("최근 에이전트"));
                            if ui.link(tr("모든 에이전트")).clicked() { self.view = View::Grid; }
                        });
                        ui.add_space(6.0);
                        for (idx, agent) in self.agents.iter().enumerate().rev().take(4) {
                            let (status, _) = display_status(&agent.shared.status(), agent.shared.paused.load(Ordering::Relaxed), agent.takeover);
                            let label = format!("{}   ·   {}", agent.shared.name, status);
                            if theme::nav_item(ui, false, icon::GHOST, &label, Some(crate::cursor::agent_color(agent.shared.id)))
                                .on_hover_text(&label).clicked() {
                                self.selected = Some(idx);
                                self.view = View::Detail;
                            }
                        }
                    }
                });
            });
        });
    }

    pub(super) fn grid(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("모든 에이전트")).size(20.0).strong().color(theme::text()));
            theme::count_badge(ui, &self.agents.len().to_string());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::primary(ui, format!("{}  {}", icon::PLUS, tr("새 에이전트"))).clicked() {
                    self.open_wizard();
                }
                if !self.agents.is_empty()
                    && theme::secondary(ui, format!("{}  {}", icon::LAYOUT_DASHBOARD, tr("화면 모아보기"))).clicked()
                {
                    self.enter_wall(ui.ctx());
                }
            });
        });
        ui.add_space(16.0);
        if self.agents.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.3).max(24.0));
                ui.label(RichText::new(tr("아직 에이전트가 없습니다")).size(20.0).color(theme::text()));
            });
            return;
        }
        self.screens(ui, false);
    }

    /// Lays out every agent's screen to fill the window. Clicking a screen opens that agent's detail
    /// view.
    pub(super) fn screens(&mut self, ui: &mut egui::Ui, immersive: bool) {
        let ctx = ui.ctx().clone();
        // the window manager may have toggled full screen, so follow the actual state
        if let Some(fs) = ctx.input(|i| i.viewport().fullscreen).filter(|_| immersive) {
            self.wall_fullscreen = fs;
        }
        let mut exit = false;
        // full screen: F11 (⌃⌘F on macOS)
        let mut toggle_fs = immersive && ctx.input(|i| {
            i.key_pressed(egui::Key::F11) || (cfg!(target_os = "macos") && i.modifiers.mac_cmd && i.modifiers.ctrl && i.key_pressed(egui::Key::F))
        });

        // screens only: the whole window is for the layout, with buttons overlaid at the top right
        let area = ui.available_rect_before_wrap();
        ui.allocate_rect(area, egui::Sense::hover());
        let badges = self.team_badges();
        // on the wall of one team, only its members' screens
        let team = if immersive { self.wall_team } else { None };
        let visible: Vec<usize> = (0..self.agents.len())
            .filter(|&i| team.is_none_or(|t| {
                let agent = &self.agents[i].shared;
                badges.get(&agent.id).is_some_and(|(id, _)| *id == t)
                    && agent.workspace().is_some_and(|desk| desk.has_screen() && !desk.is_closed())
            }))
            .collect();
        let team_name = team.and_then(|t| self.teams_ui.cache().iter().find(|x| x.id == t).map(|x| x.name.clone()));
        if team.is_some() && visible.is_empty() {
            ui.painter().text(
                area.center(),
                egui::Align2::CENTER_CENTER,
                tr("이 팀 에이전트가 연 화면이 아직 없어요"),
                egui::FontId::proportional(16.0),
                theme::muted(),
            );
        }
        let n = visible.len();
        let aspect_of = |a: &AgentHandle| {
            a.shared.workspace().map(|w| {
                let (sw, sh) = w.size();
                sh as f32 / sw.max(1) as f32
            })
        };
        // size cells for the tallest screen so every screen fits with its aspect ratio intact
        let aspect = visible.iter().filter_map(|&i| aspect_of(&self.agents[i])).fold(0.0f32, f32::max);
        let aspect = if aspect > 0.0 { aspect } else { 0.625 };
        let gap = 10.0;
        let (cols, cell) = wall_layout(n, area.size(), aspect, gap);
        let rows = n.div_ceil(cols.max(1));
        let total_h = rows as f32 * cell.y + gap * (rows as f32 - 1.0);
        // the wall centers its screens; the list view starts under its header
        let top = if immersive { area.center().y - total_h / 2.0 } else { area.min.y };
        let show_cursor = self.show_cursor;
        let mut open = None;
        let mut repaint = false;
        for (k, (i, a)) in self.agents.iter_mut().enumerate().filter(|(i, _)| visible.contains(i)).enumerate() {
            let (r, c) = (k / cols, k % cols);
            // center the last row
            let in_row = if r + 1 == rows { n - r * cols } else { cols };
            let row_w = in_row as f32 * cell.x + gap * (in_row as f32 - 1.0);
            let left = area.center().x - row_w / 2.0;
            let cell_rect = egui::Rect::from_min_size(egui::pos2(left + c as f32 * (cell.x + gap), top + r as f32 * (cell.y + gap)), cell);
            let ws = a.shared.workspace();
            let asp = ws
                .as_ref()
                .map(|w| {
                    if !w.has_screen() { return aspect; }
                    let (sw, sh) = w.size();
                    sh as f32 / sw.max(1) as f32
                })
                .unwrap_or(aspect);
            let fit = if asp >= cell.y / cell.x { Vec2::new(cell.y / asp, cell.y) } else { Vec2::new(cell.x, cell.x * asp) };
            let rect = egui::Rect::from_center_size(cell_rect.center(), fit);
            let resp = match &a.texture {
                Some(t) => ui.put(rect, egui::Image::new((t.id(), rect.size())).corner_radius(theme::CARD_RADIUS).sense(egui::Sense::click())),
                None => {
                    let r = ui.allocate_rect(rect, egui::Sense::click());
                    if ws.as_ref().is_some_and(|w| !w.has_screen()) {
                        ui.painter().rect_filled(rect, theme::CARD_RADIUS, theme::card());
                        let recent = a.shared.log.lock().last().map(|entry| crate::llm::short(&entry.text,240)).unwrap_or_default();
                        let galley = ui.painter().layout(format!("{}\n\n{recent}",tr("SSH 작업 로그")),egui::FontId::proportional(13.0),theme::muted(),(rect.width()-32.0).max(1.0));
                        ui.painter().with_clip_rect(rect.shrink(8.0)).galley(rect.min+Vec2::splat(16.0),galley,theme::muted());
                    } else {
                        preparing(ui, rect, tr("준비 중…"));
                    }
                    r
                }
            };
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, trf("{} 크게 보기", &[&a.shared.name])));
            let st = a.shared.status();
            let p = ui.painter();
            if ws.is_none() && a.texture.is_some() && !matches!(st, AgentStatus::Preparing(_)) {
                p.rect_filled(rect, theme::CARD_RADIUS, Color32::from_black_alpha(160));
                p.text(rect.center(), egui::Align2::CENTER_CENTER, tr("닫힘"), egui::FontId::proportional(18.0), Color32::from_gray(200));
            }
            if let (true, Some(w), Some(_)) = (show_cursor, ws.as_ref(), a.texture.as_ref()) {
                let (sw, _) = w.size();
                let scale = rect.width() / sw.max(1) as f32;
                let mut cur = a.shared.cursor.lock();
                repaint |= crate::cursor::paint(
                    p, rect, scale, &mut cur, &mut a.tile_cursor_view,
                    w.mouse_pos(), crate::cursor::agent_color(a.shared.id), None, true,
                );
            }
            // top-left name tag: ● name · status (border and status text in the attention color if a
            // person is needed)
            let paused = a.shared.paused.load(Ordering::Relaxed);
            let attn = if paused && !a.takeover { None } else { attention(&st, a.takeover) };
            let mut job = egui::text::LayoutJob::default();
            let fmt = |color: Color32| egui::TextFormat { font_id: egui::FontId::proportional(12.0), color, ..Default::default() };
            job.append(&a.shared.name, 0.0, fmt(Color32::WHITE));
            let (status, _) = display_status(&st, paused, a.takeover);
            let status = if is_error(&st) { humanize_error(&status) } else { status };
            job.append(&format!("  ·  {}", crate::llm::short(&status, 28)), 0.0, fmt(attn.unwrap_or(Color32::from_gray(205))));
            job.wrap.max_width = (rect.width() - 48.0).max(1.0);
            job.wrap.max_rows = 1;
            job.wrap.break_anywhere = true;
            let g = p.layout_job(job);
            let at = rect.left_top() + Vec2::new(10.0, 9.0);
            let pill = egui::Rect::from_min_size(at, Vec2::new(g.size().x + 16.0, g.size().y)).expand2(Vec2::new(8.0, 4.0));
            p.rect_filled(pill, 255, Color32::from_black_alpha(170));
            p.circle_filled(egui::pos2(at.x + 4.0, at.y + g.size().y / 2.0), 4.0, crate::cursor::agent_color(a.shared.id));
            p.galley(at + Vec2::new(14.0, 0.0), g, Color32::WHITE);
            theme::focus_outline(ui, &resp, theme::CARD_RADIUS);
            // team member's screen: the team (and role) in a tag at the top right
            if let Some((_, label)) = badges.get(&a.shared.id) {
                let tg = p.layout_no_wrap(format!("{}  {}", icon::LIST_TODO, label), egui::FontId::proportional(12.0), Color32::WHITE);
                let w = tg.size().x.min(rect.width() * 0.5);
                let at = egui::pos2(rect.right() - 10.0 - w - 8.0, rect.top() + 9.0);
                let tag = egui::Rect::from_min_size(at, Vec2::new(w, tg.size().y)).expand2(Vec2::new(8.0, 4.0));
                p.rect_filled(tag, 255, theme::team_tag());
                p.with_clip_rect(tag).galley(at, tg, Color32::WHITE);
            }
            let resp = resp.on_hover_text(format!("{}\n{status}", a.shared.name));
            if let Some(c) = attn {
                p.rect_stroke(rect, theme::CARD_RADIUS, Stroke::new(3.0, c), StrokeKind::Outside);
            } else if resp.hovered() {
                p.rect_stroke(rect, theme::CARD_RADIUS, Stroke::new(1.5, theme::border_hover()), StrokeKind::Outside);
            }
            if resp.clicked() {
                open = Some(i);
            }
            resp.context_menu(|ui| {
                if !a.pip.visible && !a.shared.workspace().is_some_and(|desk| desk.has_screen() && !desk.is_closed()) { return; }
                if ui.button(format!("{}  {}", icon::PICTURE_IN_PICTURE, if a.pip.visible { tr("PiP 닫기") } else { tr("작은 창(PiP)으로 띄우기") })).clicked() {
                    a.pip.visible = !a.pip.visible;
                    ui.close();
                }
            });
        }
        // top-right buttons (in full screen, only while the pointer is near the top edge)
        let near_top = ctx.input(|i| i.pointer.hover_pos().is_some_and(|p| p.y < area.top() + 90.0));
        if immersive && (!self.wall_fullscreen || near_top) {
            // sit below the title bar if there is one, so the window buttons stay uncovered
            let top = if titlebar::visible(&ctx) { titlebar::HEIGHT } else { 0.0 } + 18.0;
            egui::Area::new(egui::Id::unique("wall_bar"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::RIGHT_TOP, [-18.0, top])
                .show(&ctx, |ui| {
                    theme::floating_frame().show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.add_space(4.0);
                            if let Some(name) = &team_name {
                                ui.label(RichText::new(format!("{}  {}", icon::LIST_TODO, name)).color(theme::text()));
                            }
                            ui.label(theme::hint(trf("화면 {}개", &[&n])));
                            if theme::icon_toggle(ui, self.show_cursor, icon::MOUSE_POINTER, tr("가상 마우스 표시")).clicked() {
                                self.show_cursor = !self.show_cursor;
                            }
                            if theme::toggle(ui, self.wall_fullscreen, tr("전체 화면"), theme::accent()).on_hover_text(pick("⌃⌘F", "F11")).clicked() {
                                toggle_fs = true;
                            }
                            if theme::secondary(ui, tr("닫기")).on_hover_text("Esc").clicked() {
                                exit = true;
                            }
                        });
                    });
                });
        }
        if toggle_fs {
            self.wall_fullscreen = !self.wall_fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.wall_fullscreen));
        }
        if repaint {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
        if let Some(i) = open {
            self.selected = Some(i);
            if immersive {
                self.leave_wall(&ctx, View::Detail);
            } else {
                self.view = View::Detail;
            }
        } else if exit {
            self.leave_wall(&ctx, View::Grid);
        }
    }
}

/// Wall layout: `(columns, cell size)` that fits `n` screens into `area` as large as possible.
/// `aspect` is height / width.
pub(super) fn wall_layout(n: usize, area: Vec2, aspect: f32, gap: f32) -> (usize, Vec2) {
    let n = n.max(1);
    let mut best = (1, Vec2::ZERO);
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        let by_width = (area.x - gap * (cols as f32 - 1.0)) / cols as f32;
        let by_height = ((area.y - gap * (rows as f32 - 1.0)) / rows as f32) / aspect;
        let w = by_width.min(by_height).max(1.0);
        if w > best.1.x + 0.01 {
            best = (cols, Vec2::new(w, w * aspect));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_layout_fills_area() {
        let wide = Vec2::new(1600.0, 900.0);
        // a single screen fills the height
        let (c, cell) = wall_layout(1, wide, 0.625, 10.0);
        assert_eq!(c, 1);
        assert!((cell.y - 900.0).abs() < 0.5 && cell.x <= 1600.0, "{cell:?}");
        // four make a 2x2
        assert_eq!(wall_layout(4, wide, 0.625, 10.0).0, 2);
        // two sit side by side in a wide area
        assert_eq!(wall_layout(2, wide, 0.625, 10.0).0, 2);
        // and stack in a tall one
        assert_eq!(wall_layout(2, Vec2::new(700.0, 1200.0), 0.625, 10.0).0, 1);
        // never overflows the area
        for n in 1..=12 {
            for area in [wide, Vec2::new(800.0, 600.0), Vec2::new(500.0, 1400.0)] {
                let (c, cell) = wall_layout(n, area, 0.625, 10.0);
                let rows = n.div_ceil(c);
                assert!(c as f32 * cell.x + (c - 1) as f32 * 10.0 <= area.x + 0.5, "n={n} {area:?}");
                assert!(rows as f32 * cell.y + (rows - 1) as f32 * 10.0 <= area.y + 0.5, "n={n} {area:?}");
            }
        }
    }
}
