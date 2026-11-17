//! Home screen and the screen wall (grid and full-screen wall).

use super::*;
use super::detail::preparing;
use super::labels::{attention, humanize_error, is_error, status_text};

/// Greetings for the middle of the home screen (one is picked at each launch).
pub(super) const GREETINGS: &[&str] = &[
    "오늘은 어떤 일을 대신 맡아 드릴까요?",
    "무엇을 도와 드릴까요?",
    "귀찮은 일, 저에게 넘겨 주세요",
    "오늘 할 일 중 하나를 맡겨 보세요",
    "어떤 일부터 시작할까요?",
    "반복되는 일은 제가 할게요",
    "손 놓고 지켜보기만 하세요",
    "클릭 대신 한 줄로 말해 주세요",
    "오늘은 뭘 끝내 볼까요?",
    "커피 한 잔 하는 동안 해 둘게요",
    "내 화면은 그대로, 일은 따로",
    "무엇이든 말만 하세요",
    "미뤄 둔 일이 있나요?",
    "오늘의 첫 번째 부탁은 무엇인가요?",
    "웹 검색부터 앱 조작까지, 맡겨 주세요",
    "어디서부터 도와 드릴까요?",
    "할 일을 적으면 바로 시작할게요",
    "지루한 일은 제 몫이에요",
    "어떤 화면에서 일해 볼까요?",
    "오늘도 대신 움직여 드릴게요",
    "잠깐 쉬세요, 제가 할게요",
    "무엇을 찾아 드릴까요?",
    "어떤 일을 자동으로 해 둘까요?",
    "몇 번의 클릭을 아껴 드릴까요?",
    "오늘 해치울 일을 알려 주세요",
    "생각만 말해 주세요, 손은 제가 쓸게요",
    "필요한 걸 말하면 찾아올게요",
    "한 번 맡기면 끝까지 해 둘게요",
    "오늘은 무엇을 맡겨 보실래요?",
    "준비됐어요. 무엇부터 할까요?",
];

/// A different greeting each launch, picked from the clock's nanoseconds; it only has to look
/// varied.
pub(super) fn random_greeting() -> usize {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos() as usize);
    nanos % GREETINGS.len()
}

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
        self.view = to;
    }

    pub(super) fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            let w = ui.available_width().min(680.0);
            ui.add_space((ui.available_height() * 0.28).max(24.0));
            ui.label(RichText::new(tr(GREETINGS[self.greeting])).size(28.0).color(theme::text()));
            ui.add_space(28.0);
            // task box: Enter (or the ↵ button) opens the new-agent window with the typed task
            let id = egui::Id::unique("welcome_composer");
            let focused = ui.memory(|m| m.has_focus(id));
            let focus = ui.ctx().animate_bool_with_time(id.with("focus_t"), focused, 0.15);
            let mut submit = false;
            ui.allocate_ui(Vec2::new(w, 0.0), |ui| {
                theme::composer_frame(focus).show(ui, |ui| {
                    ui.set_width(w - 32.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), |ui| {
                            submit |= theme::enter_button(ui, tr("시작하기 (Enter)")).clicked();
                            let edit = egui::TextEdit::multiline(&mut self.form.task)
                                .id(id)
                                .hint_text(tr("에이전트에게 맡길 일을 적어 보세요"))
                                .frame(egui::Frame::NONE)
                                // nudge the first line down to balance against the ↵ button at the bottom right
                                .margin(Margin { left: 4, right: 4, top: 8, bottom: 2 })
                                .desired_rows(2)
                                .desired_width(ui.available_width())
                                .return_key(Some(egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter)));
                            let r = ui.add(edit);
                            if r.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift) {
                                submit = true;
                            }
                        });
                    });
                });
            });
            // right below the box, aligned to its left edge: which AI to work with
            ui.add_space(6.0);
            ui.allocate_ui_with_layout(Vec2::new(w, 28.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add_space(4.0);
                self.model_picker(ui);
            });
            if submit {
                self.open_wizard();
            }
        });
    }

    pub(super) fn grid(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("모든 에이전트")).size(20.0).strong().color(theme::text()));
            ui.label(RichText::new(self.agents.len().to_string()).size(14.0).color(theme::muted()));
        });
        ui.add_space(16.0);
        if self.agents.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.3).max(24.0));
                ui.label(RichText::new(tr("아직 에이전트가 없습니다")).size(20.0).color(theme::text()));
                ui.add_space(8.0);
                ui.label(theme::hint(tr("홈에서 할 일을 적거나 새 에이전트를 만들어 보세요.")));
                ui.add_space(20.0);
                if theme::primary(ui, tr("새 에이전트")).clicked() {
                    self.open_wizard();
                }
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
        let n = self.agents.len();
        let aspect_of = |a: &AgentHandle| {
            a.shared.workspace().map(|w| {
                let (sw, sh) = w.size();
                sh as f32 / sw.max(1) as f32
            })
        };
        // size cells for the tallest screen so every screen fits with its aspect ratio intact
        let aspect = self.agents.iter().filter_map(aspect_of).fold(0.0f32, f32::max);
        let aspect = if aspect > 0.0 { aspect } else { 0.625 };
        let gap = 10.0;
        let (cols, cell) = wall_layout(n, area.size(), aspect, gap);
        let rows = n.div_ceil(cols.max(1));
        let total_h = rows as f32 * cell.y + gap * (rows as f32 - 1.0);
        let top = area.center().y - total_h / 2.0;
        let show_cursor = self.show_cursor;
        let mut open = None;
        let mut repaint = false;
        for (i, a) in self.agents.iter_mut().enumerate() {
            let (r, c) = (i / cols, i % cols);
            // center the last row
            let in_row = if r + 1 == rows { n - r * cols } else { cols };
            let row_w = in_row as f32 * cell.x + gap * (in_row as f32 - 1.0);
            let left = area.center().x - row_w / 2.0;
            let cell_rect = egui::Rect::from_min_size(egui::pos2(left + c as f32 * (cell.x + gap), top + r as f32 * (cell.y + gap)), cell);
            let ws = a.shared.workspace();
            let asp = ws
                .as_ref()
                .map(|w| {
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
                    preparing(ui, rect, tr("준비 중…"));
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
            let attn = attention(&st, a.takeover);
            let mut job = egui::text::LayoutJob::default();
            let fmt = |color: Color32| egui::TextFormat { font_id: egui::FontId::proportional(12.0), color, ..Default::default() };
            job.append(&a.shared.name, 0.0, fmt(Color32::WHITE));
            let status = status_text(&st);
            let status = if is_error(&st) { humanize_error(&status) } else { status };
            job.append(&format!("  ·  {}", crate::llm::short(&status, 28)), 0.0, fmt(attn.unwrap_or(Color32::from_gray(205))));
            let g = p.layout_job(job);
            let at = rect.left_top() + Vec2::new(10.0, 9.0);
            let pill = egui::Rect::from_min_size(at, Vec2::new(g.size().x + 16.0, g.size().y)).expand2(Vec2::new(8.0, 4.0));
            p.rect_filled(pill, 255, Color32::from_black_alpha(170));
            p.circle_filled(egui::pos2(at.x + 4.0, at.y + g.size().y / 2.0), 4.0, crate::cursor::agent_color(a.shared.id));
            p.galley(at + Vec2::new(14.0, 0.0), g, Color32::WHITE);
            if let Some(c) = attn {
                p.rect_stroke(rect, theme::CARD_RADIUS, Stroke::new(3.0, c), StrokeKind::Outside);
            } else if resp.hovered() {
                p.rect_stroke(rect, theme::CARD_RADIUS, Stroke::new(1.5, theme::border_hover()), StrokeKind::Outside);
            }
            if resp.clicked() {
                open = Some(i);
            }
            resp.context_menu(|ui| {
                if ui.button(format!("{}  {}", icon::PICTURE_IN_PICTURE, if a.pip { tr("PiP 닫기") } else { tr("작은 창(PiP)으로 띄우기") })).clicked() {
                    a.pip = !a.pip;
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
