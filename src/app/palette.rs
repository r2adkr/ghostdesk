//! Quick switcher (⌘K / Ctrl+K): type to jump to a screen, an agent or an action.

use super::*;

#[derive(Default)]
pub(super) struct Palette {
    query: String,
    sel: usize,
    /// The search box takes the keyboard on the first frame only
    focused: bool,
}

#[derive(Clone)]
enum Do {
    View(View),
    Agent(usize),
    NewAgent,
    Settings,
    Connect,
    ToggleSidebar,
    Tour,
}

struct Item {
    icon: &'static str,
    label: String,
    keys: &'static str,
    run: Do,
}

/// "⌘ N" on macOS (a space after the symbol so it doesn't run into the letter), "Ctrl+N" elsewhere.
pub(super) fn cmd_key(rest: &str) -> String {
    format!("{}{rest}", if cfg!(target_os = "macos") { "⌘ " } else { "Ctrl+" })
}

impl GhostApp {
    pub(super) fn open_palette(&mut self) {
        self.palette = Some(Palette::default());
    }

    fn palette_items(&self) -> Vec<Item> {
        let mut v = vec![
            Item { icon: icon::PLUS, label: tr("새 에이전트").into(), keys: "N", run: Do::NewAgent },
            Item { icon: icon::GHOST, label: tr("홈").into(), keys: "1", run: Do::View(View::Home) },
            Item { icon: icon::LAYOUT_GRID, label: tr("모든 에이전트").into(), keys: "2", run: Do::View(View::Grid) },
            Item { icon: icon::LIST_TODO, label: tr("팀").into(), keys: "3", run: Do::View(View::Teams) },
            Item { icon: icon::CHART_COLUMN, label: tr("사용량").into(), keys: "4", run: Do::View(View::Usage) },
            Item { icon: icon::PLUG, label: tr("외부 연결").into(), keys: "", run: Do::Connect },
            Item { icon: icon::SETTINGS, label: tr("설정").into(), keys: ",", run: Do::Settings },
            Item { icon: icon::PANEL_LEFT, label: tr("사이드바 접기/펴기").into(), keys: "B", run: Do::ToggleSidebar },
            Item { icon: icon::BOOK_OPEN, label: tr("사용 설명 다시 보기").into(), keys: "", run: Do::Tour },
        ];
        for (i, a) in self.agents.iter().enumerate() {
            v.push(Item { icon: icon::GHOST, label: a.shared.name.clone(), keys: "", run: Do::Agent(i) });
        }
        v
    }

    pub(super) fn palette_window(&mut self, ctx: &egui::Context) {
        let Some(mut pal) = self.palette.take() else { return };
        let all = self.palette_items();
        let q = pal.query.trim().to_lowercase();
        let shown: Vec<&Item> = all.iter().filter(|it| q.is_empty() || it.label.to_lowercase().contains(&q)).collect();
        pal.sel = pal.sel.min(shown.len().saturating_sub(1));
        let (down, up, enter) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
            )
        });
        if down && !shown.is_empty() {
            pal.sel = (pal.sel + 1) % shown.len();
        }
        if up && !shown.is_empty() {
            pal.sel = (pal.sel + shown.len() - 1) % shown.len();
        }
        let mut run: Option<Do> = None;
        if enter {
            run = shown.get(pal.sel).map(|it| it.run.clone());
        }
        let modal = egui::Modal::new(egui::Id::new("palette")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(520.0);
            let edit = ui.add(
                egui::TextEdit::singleline(&mut pal.query)
                    .hint_text(format!("{}  {}", icon::SEARCH, tr("화면·에이전트·동작 찾기")))
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(12, 10)),
            );
            if !pal.focused {
                edit.request_focus();
                pal.focused = true;
            }
            if edit.changed() {
                pal.sel = 0;
            }
            ui.add_space(8.0);
            if shown.is_empty() {
                ui.add_space(8.0);
                ui.label(theme::hint(tr("맞는 항목이 없어요")));
                ui.add_space(8.0);
            }
            egui::ScrollArea::vertical().max_height(320.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for (i, it) in shown.iter().enumerate() {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), egui::Sense::click());
                    let selected = i == pal.sel;
                    if selected || resp.hovered() {
                        ui.painter().rect_filled(rect, theme::RADIUS, if selected { theme::card_hover() } else { theme::overlay_hover() });
                    }
                    if selected {
                        resp.scroll_to_me(None);
                    }
                    let p = ui.painter();
                    p.text(egui::pos2(rect.left() + 18.0, rect.center().y), egui::Align2::CENTER_CENTER, it.icon, egui::FontId::proportional(16.0), theme::body());
                    p.text(egui::pos2(rect.left() + 38.0, rect.center().y), egui::Align2::LEFT_CENTER, &it.label, egui::FontId::proportional(14.0), theme::text());
                    if !it.keys.is_empty() {
                        p.text(
                            egui::pos2(rect.right() - 12.0, rect.center().y),
                            egui::Align2::RIGHT_CENTER,
                            cmd_key(it.keys),
                            egui::FontId::monospace(13.0),
                            theme::muted(),
                        );
                    }
                    if resp.clicked() {
                        run = Some(it.run.clone());
                    }
                    if resp.hovered() && ui.input(|i| i.pointer.delta() != Vec2::ZERO) {
                        pal.sel = i;
                    }
                }
            });
        });
        if let Some(action) = run {
            match action {
                Do::View(v) => self.view = v,
                Do::Agent(i) if i < self.agents.len() => {
                    self.selected = Some(i);
                    self.view = View::Detail;
                }
                Do::Agent(_) => {}
                Do::NewAgent => self.open_wizard(),
                Do::Settings => self.open_settings(settings::SettingsTab::General),
                Do::Connect => self.open_connect(ctx),
                Do::ToggleSidebar => self.sidebar_open = !self.sidebar_open,
                Do::Tour => self.open_tour(),
            }
            return;
        }
        // Esc or a click outside closes it
        if !modal.should_close() {
            self.palette = Some(pal);
        }
    }
}
