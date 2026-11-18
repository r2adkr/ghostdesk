//! Detail view for one agent: live screen, takeover, log and instructions.

use super::*;
use super::labels::display_status;

/// One takeover input from the user, in agent coordinates.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum TakeoverInput {
    /// Drag start: move to the point, then press the button.
    DragStart(f64, f64),
    MouseMove(f64, f64),
    MouseUp,
    Click { pos: (f64, f64), button: MouseButton, count: u32 },
    Type(String),
    /// A key combo pressed by a person (clipboard combos pass through too).
    Key(String),
    Scroll { pos: Option<(f64, f64)>, dx: f64, dy: f64 },
}

impl TakeoverInput {
    async fn apply(self, w: &crate::desk::Desk) -> anyhow::Result<()> {
        match self {
            TakeoverInput::DragStart(x, y) => {
                w.mouse_move(x, y, 0).await?;
                w.mouse_down(MouseButton::Left, 1, 0).await
            }
            TakeoverInput::MouseMove(x, y) => w.mouse_move(x, y, 0).await,
            TakeoverInput::MouseUp => w.mouse_up(MouseButton::Left, 1, 0).await,
            TakeoverInput::Click { pos, button, count } => w.click(Some(pos), button, count, 0).await,
            TakeoverInput::Type(t) => w.type_text(&t).await,
            TakeoverInput::Key(k) => w.key_by_human(&k, 1).await,
            TakeoverInput::Scroll { pos, dx, dy } => w.scroll(pos, dx, dy, 0).await,
        }
    }
}

impl GhostApp {
    /// Right panel of the detail view: name/status → controls → (question / external notice) → log
    /// → instruction box at the bottom.
    pub(super) fn detail_side(&mut self, ui: &mut egui::Ui, idx: usize) -> bool {
        let a = &mut self.agents[idx];
        let st = a.shared.status();
        let mut remove = false;
        // row 1: ● name ……… ⋯ (remove lives in the menu so it isn't hit by accident)
        ui.horizontal(|ui| {
            theme::dot(ui, crate::cursor::agent_color(a.shared.id), 5.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button(RichText::new(icon::ELLIPSIS).size(18.0).color(theme::body()), |ui| {
                    if ui
                        .button(RichText::new(tr("에이전트 제거")).color(theme::danger()))
                        .on_hover_text(tr("에이전트 화면을 닫아요 (작업공간 폴더는 남아요)"))
                        .clicked()
                    {
                        remove = true;
                        ui.close();
                    }
                })
                .response
                .on_hover_text(tr("더 보기"));
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(&a.shared.name).size(18.0).strong().color(theme::text())).truncate()).on_hover_text(&a.shared.name);
                });
            });
        });
        let files = a.shared.received_files.lock().clone();
        if !files.is_empty() {
            theme::section(ui, tr("받은 파일"));
            egui::ScrollArea::vertical().id_salt(("received", a.shared.id)).max_height(140.0).show(ui, |ui| {
                for file in files.iter().rev() {
                    ui.label(theme::hint(file.path.display().to_string()));
                    ui.horizontal(|ui| {
                        if ui.link(tr("파일 열기")).clicked() { crate::kit::reveal(&file.path); }
                        if ui.link(tr("폴더 열기")).clicked() { if let Some(parent) = file.path.parent() { crate::kit::reveal(parent); } }
                    });
                }
            });
        }
        let tokens = *a.shared.tokens.lock();
        let paused = a.shared.paused.load(Ordering::Relaxed);
        let (status, color) = display_status(&st, paused, a.takeover);
        let info = trf("단계 {} · 토큰 {} / {} · 작업공간 {}", &[&a.shared.steps.load(Ordering::Relaxed), &tokens.0, &tokens.1, &a.shared.workspace_name]);
        // spinner plus shimmering status text while working, a colored chip when stopped
        if is_working(&st) && !paused && !a.takeover {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                theme::spinner(ui, 15.0, theme::text());
                theme::shimmer(ui, &crate::llm::short(&status, 48), 13.0)
            })
            .response
            .on_hover_text(info);
        } else {
            theme::chip(ui, color, &status).on_hover_text(format!("{status}\n{info}"));
        }
        // steps: how many moves since the last instruction (a bar when there's a step limit)
        let run = a.shared.run_steps.load(Ordering::Relaxed);
        if run > 0 {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                if a.max_steps > 0 {
                    ui.label(theme::hint(trf("단계 {} / {}", &[&run, &a.max_steps])));
                    let w = ui.available_width().min(220.0);
                    theme::progress(ui, egui::Id::unique(("steps", a.shared.id)), run as f32 / a.max_steps as f32, w);
                } else {
                    ui.label(theme::hint(trf("단계 {}", &[&run])));
                }
            });
        }
        ui.add_space(2.0);
        ui.horizontal_wrapped(|ui| {
            if theme::secondary(ui, if paused { tr("재개") } else { tr("일시정지") }).clicked() {
                a.shared.paused.store(!paused, Ordering::Relaxed);
            }
            if theme::secondary(ui, tr("중지")).on_hover_text(tr("지금 동작만 끝내고 멈춰요. 다시 지시하면 이어서 해요.")).clicked() {
                a.shared.stop.store(true, Ordering::SeqCst);
                a.shared.paused.store(false, Ordering::Relaxed);
            }
            let workspace = a.shared.workspace();
            let has_screen = workspace.as_ref().is_none_or(|w| w.has_screen());
            if has_screen {
                let before = a.takeover;
                if theme::toggle(ui, a.takeover, tr("직접 조작"), theme::takeover())
                    .on_hover_text(tr("에이전트를 멈추고 내가 직접 화면을 눌러요 (로그인 · 인증할 때)"))
                    .clicked()
                {
                    a.takeover = !a.takeover;
                }
                if a.takeover != before {
                    a.shared.paused.store(a.takeover, Ordering::Relaxed);
                }
            }
            if a.pip.visible || workspace.is_some_and(|desk| desk.has_screen() && !desk.is_closed()) {
                if theme::icon_toggle(ui, a.pip.visible, icon::PICTURE_IN_PICTURE, tr("작은 창(PiP)으로 띄우기 — 다른 일을 하면서 지켜볼 수 있어요")).clicked() {
                    a.pip.visible = !a.pip.visible;
                }
            }
        });

        if let AgentStatus::WaitingUser(q) = &st {
            ui.add_space(2.0);
            theme::tinted(theme::warn()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(tr("에이전트의 질문")).strong().color(theme::warn()));
                // cap the height so a long question doesn't push the answer box away
                egui::ScrollArea::vertical().id_salt("question").max_height(160.0).show(ui, |ui| {
                    ui.label(q);
                });
            });
        }
        let external = a.shared.external.lock().clone();
        if let Some(client) = &external {
            ui.add_space(2.0);
            theme::tinted(theme::info()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(trf("{} 이(가) 조작하는 화면", &[&client])).strong().color(theme::info_text()));
                ui.label(theme::hint(tr("지시는 그 AI 앱에서 해 주세요. 여기서는 지켜보거나 '직접 조작'으로 도울 수 있어요.")));
            });
        }
        ui.add_space(6.0);

        // middle: the log; bottom: the instruction box (only for agents inside GhostDesk).
        // reserve the log area using the composer height measured last frame, and redraw once more if
        // it changed. When space runs short the log shrinks; the box and send button always stay visible.
        let has_composer = external.is_none();
        let composer_id = egui::Id::unique(("composer_h", a.shared.id));
        let composer_h: f32 = ui.data(|d| d.get_temp(composer_id)).unwrap_or(150.0);
        let log_h = (ui.available_height() - if has_composer { composer_h + 10.0 } else { 0.0 }).max(0.0);
        if log_h >= 60.0 {
            egui::Frame::new().fill(theme::bg()).corner_radius(theme::CARD_RADIUS).inner_margin(Margin::same(10)).show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("log")
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .max_height(log_h - 20.0)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 7.0;
                        let log = a.shared.log.lock();
                        // the last action row gets a spinner while that action is still running
                        let acting = matches!(st, AgentStatus::Acting(_)) && !paused;
                        let last_action = log.iter().rposition(|l| l.kind == LogKind::Action);
                        let n = log.len();
                        for (i, l) in log.iter().enumerate() {
                            let live = acting && Some(i) == last_action;
                            if i + 1 == n {
                                // new rows fade in
                                let t = theme::appear(ui.ctx(), egui::Id::unique(("log_in", a.shared.id)), n as u64, 0.25);
                                ui.scope(|ui| {
                                    ui.multiply_opacity(t);
                                    log_row(ui, l, live);
                                });
                            } else {
                                log_row(ui, l, live);
                            }
                        }
                    });
            });
        }

        if has_composer {
            ui.add_space(10.0);
            let waiting = matches!(st, AgentStatus::WaitingUser(_));
            let composer = ui.scope(|ui| {
                let mut remove_q = None;
                {
                    let q = a.shared.queue.lock();
                    if !q.is_empty() {
                        egui::ScrollArea::vertical().id_salt("queue").max_height(84.0).show(ui, |ui| {
                            for (i, t) in q.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    ui.label(theme::hint(trf("다음 {}. {}", &[&(i + 1), &crate::llm::short(t, 40)])));
                                    if ui.small_button(icon::X).on_hover_text(tr("대기열에서 빼기")).clicked() {
                                        remove_q = Some(i);
                                    }
                                });
                            }
                        });
                    }
                }
                if let Some(i) = remove_q {
                    a.shared.queue.lock().remove(i);
                }
                let hint = if waiting {
                    pick(tr("답변 입력 (Cmd+Enter 전송)"), tr("답변 입력 (Ctrl+Enter 전송)"))
                } else {
                    pick(tr("지시 입력 (Cmd+Enter 전송)"), tr("지시 입력 (Ctrl+Enter 전송)"))
                };
                let te = ui.add(
                    egui::TextEdit::multiline(&mut a.input)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(10, 8))
                        .hint_text(hint),
                );
                let send_key = te.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.command);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if (theme::primary(ui, if waiting { tr("답변 보내기") } else { tr("보내기") }).on_hover_text(tr("지금 바로 지시해요")).clicked() || send_key)
                        && !a.input.trim().is_empty()
                    {
                        let text = a.input.trim().to_string();
                        let cmd = if waiting { AgentCmd::Answer(text) } else { AgentCmd::Instruction(text) };
                        let _ = a.tx.send(cmd);
                        a.input.clear();
                    }
                    if !waiting
                        && theme::secondary(ui, tr("다음 작업으로"))
                            .on_hover_text(tr("지금 일이 끝나면 이어서 할 일로 줄 세워 둬요. 멈춰 있을 때는 '보내기'로 먼저 다시 시작해 주세요."))
                            .clicked()
                        && !a.input.trim().is_empty()
                    {
                        a.shared.queue.lock().push_back(a.input.trim().to_string());
                        a.input.clear();
                        let _ = a.tx.send(AgentCmd::Wake);
                    }
                });
            });
            let h = composer.response.rect.height();
            if (h - composer_h).abs() > 0.5 {
                ui.data_mut(|d| d.insert_temp(composer_id, h));
                ui.ctx().request_repaint();
            }
        }
        remove
    }

    pub(super) fn detail_view(&mut self, ui: &mut egui::Ui, idx: usize) {
        let rt = self.rt.clone();
        let ws_opt = self.agents[idx].shared.workspace();
        if ws_opt.as_ref().is_some_and(|w| !w.has_screen()) {
            if theme::secondary(ui, tr("‹  모두 보기")).clicked() { self.view = View::Grid; }
            theme::section(ui, tr("SSH 작업 로그"));
            let log = self.agents[idx].shared.log.lock().clone();
            egui::ScrollArea::vertical().id_salt(("ssh_log", self.agents[idx].shared.id)).stick_to_bottom(true).show(ui, |ui| {
                for entry in &log { log_row(ui,entry,false); }
            });
            return;
        }
        let browser = ws_opt.as_ref().and_then(|w| w.as_browser().cloned());
        let compact_browser = browser.is_some() && ui.available_width() < 520.0;
        let (views, active) = ws_opt.as_ref().map(|w| w.views()).unwrap_or_default();
        // row 1: ‹ all agents · (browser: ◀ ▶ ⟳ + address / app: current window title) · virtual
        // mouse at the far right
        ui.horizontal(|ui| {
            if ui
                .add(egui::Button::new(RichText::new(tr("‹  모두 보기")).color(theme::text())).frame_when_inactive(false).min_size(Vec2::new(0.0, 32.0)))
                .clicked()
            {
                self.view = View::Grid;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::icon_toggle(ui, self.show_cursor, icon::MOUSE_POINTER, tr("가상 마우스 표시")).clicked() {
                    self.show_cursor = !self.show_cursor;
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if let Some(b) = &browser {
                        let w = b.clone();
                        if theme::icon_button(ui, icon::ARROW_LEFT, tr("뒤로")).clicked() {
                            rt.spawn(async move { let _ = w.history_go(-1).await; });
                        }
                        let w = b.clone();
                        if theme::icon_button(ui, icon::ARROW_RIGHT, tr("앞으로")).clicked() {
                            rt.spawn(async move { let _ = w.history_go(1).await; });
                        }
                        let w = b.clone();
                        if theme::icon_button(ui, icon::ROTATE_CW, tr("새로 고침")).clicked() {
                            rt.spawn(async move { let _ = w.reload().await; });
                        }
                        let w = b.clone();
                        if theme::icon_button(ui, "＋", tr("새 탭")).clicked() {
                            rt.spawn(async move { let _ = w.new_tab("").await; });
                        }
                        if !compact_browser {
                            browser_address(ui, b, &rt, &mut self.agents[idx].url_input);
                        }
                    } else if let Some(t) = views.iter().find(|t| Some(&t.id) == active.as_ref()) {
                        let title = if t.title.is_empty() { t.detail.clone() } else { t.title.clone() };
                        ui.add(egui::Label::new(theme::hint(title)).truncate());
                    }
                });
            });
        });
        if compact_browser {
            if let Some(b) = &browser {
                browser_address(ui, b, &rt, &mut self.agents[idx].url_input);
            }
        }
        // row 2: only with two or more tabs/windows (wraps when it overflows)
        if views.len() > 1 {
            ui.horizontal_wrapped(|ui| {
                for (i, t) in views.iter().enumerate() {
                    let title = if t.title.is_empty() { t.detail.clone() } else { t.title.clone() };
                    let sel = Some(&t.id) == active.as_ref();
                    if ui.add(egui::Button::selectable(sel, crate::llm::short(&title, 24))).on_hover_text(&t.detail).clicked() && !sel {
                        if let Some(ws) = &ws_opt {
                            let w = ws.clone();
                            rt.spawn(async move { let _ = w.switch_view(i).await; });
                        }
                    }
                }
            });
        }
        let a = &mut self.agents[idx];
        let Some(ws) = ws_opt else {
            let (status, color) = display_status(&a.shared.status(), a.shared.paused.load(Ordering::Relaxed), a.takeover);
            ui.centered_and_justified(|ui| ui.label(RichText::new(status).color(color)));
            return;
        };
        ui.add_space(6.0);

        // live screen (centered, aspect ratio kept)
        let avail = ui.available_size();
        let (sw, sh) = ws.size();
        let scale = (avail.x / sw.max(1) as f32).min(avail.y / sh.max(1) as f32).max(0.05);
        let size = Vec2::new(sw as f32 * scale, sh as f32 * scale);
        let sense = if a.takeover { egui::Sense::click_and_drag() } else { egui::Sense::hover() };
        let pad = ((avail.x - size.x) / 2.0).max(0.0);
        let resp = ui
            .horizontal(|ui| {
                ui.add_space(pad);
                match &a.texture {
                    Some(t) => ui.add(egui::Image::new((t.id(), size)).corner_radius(theme::CARD_RADIUS).sense(sense)),
                    None => {
                        let r = ui.allocate_response(size, sense);
                        preparing(ui, r.rect, tr("화면 준비 중…"));
                        r
                    }
                }
            })
            .inner;
        let rect = resp.rect;
        if !a.takeover {
            // thin border so black pages stand apart from the background
            ui.painter().rect_stroke(rect, 10, Stroke::new(1.0, theme::border()), StrokeKind::Outside);
        }
        // virtual mouse: isolated screens have no real cursor, so draw one from the agent's recorded
        // actions
        if self.show_cursor && a.texture.is_some() {
            let mut st = a.shared.cursor.lock();
            let again = crate::cursor::paint(
                ui.painter(), rect, scale, &mut st, &mut a.cursor_view, ws.mouse_pos(),
                crate::cursor::agent_color(a.shared.id), Some(&a.shared.name), false,
            );
            if again {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(16));
            }
        }
        let shared = a.shared.clone();
        let rec = move |name: &str, input: serde_json::Value, pos: (f64, f64)| {
            shared.cursor.lock().record(name, &input, pos, true);
        };
        if a.takeover {
            // during takeover: orange border plus a badge at the top left
            let p = ui.painter();
            p.rect_stroke(rect, 10, egui::Stroke::new(2.0, theme::takeover()), egui::StrokeKind::Outside);
            let at = rect.left_top() + Vec2::new(12.0, 10.0);
            let g = p.layout_no_wrap(tr("직접 조작 중").to_string(), egui::FontId::proportional(12.0), Color32::BLACK);
            p.rect_filled(egui::Rect::from_min_size(at, g.size()).expand2(Vec2::new(8.0, 3.0)), 255, theme::takeover());
            p.galley(at, g, Color32::BLACK);
        }

        if !a.takeover {
            return;
        }
        let to_page = |p: egui::Pos2| (((p.x - rect.min.x) / scale) as f64, ((p.y - rect.min.y) / scale) as f64);
        let hovered = resp.hovered();
        if hovered || resp.has_focus() {
            resp.request_focus();
        }
        // all input goes through one queue per agent and is applied in order
        let input = a.input_tx.clone();
        let send = move |ev: TakeoverInput| {
            let _ = input.send(ev);
        };
        if resp.drag_started() {
            if let Some(p) = resp.interact_pointer_pos() {
                let (x, y) = to_page(p);
                rec("left_mouse_down", serde_json::json!({ "coordinate": [x, y] }), (x, y));
                send(TakeoverInput::DragStart(x, y));
            }
        } else if resp.dragged() {
            if let Some(p) = resp.interact_pointer_pos() {
                let (x, y) = to_page(p);
                rec("mouse_move", serde_json::json!({ "coordinate": [x, y] }), (x, y));
                send(TakeoverInput::MouseMove(x, y));
            }
        }
        if resp.drag_stopped() {
            send(TakeoverInput::MouseUp);
        } else if resp.clicked() || resp.secondary_clicked() || resp.double_clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                let (x, y) = to_page(p);
                let (btn, count) = if resp.secondary_clicked() {
                    (MouseButton::Right, 1)
                } else if resp.double_clicked() {
                    (MouseButton::Left, 2)
                } else {
                    (MouseButton::Left, 1)
                };
                let name = match (btn, count) {
                    (MouseButton::Right, _) => "right_click",
                    (_, 2) => "double_click",
                    _ => "left_click",
                };
                rec(name, serde_json::json!({ "coordinate": [x, y] }), (x, y));
                send(TakeoverInput::Click { pos: (x, y), button: btn, count });
            }
        }
        if !hovered {
            // leaving the screen turns the IME off, so clear any in-progress composition too
            a.preedit.clear();
            return;
        }
        // Windows: keep the IME on during takeover. egui turns it off when no text field has focus
        // (ImmAssociateContextEx), which makes a Korean layout type Latin letters. Committed text is
        // sent as Type.
        if cfg!(windows) {
            let at = resp.hover_pos().unwrap_or(rect.left_top());
            ui.ctx().output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    purpose: egui::IMEPurpose::Normal,
                    rect,
                    cursor_rect: egui::Rect::from_min_size(at, Vec2::new(1.0, 18.0)),
                    should_interrupt_composition: false,
                });
            });
        }
        let events = ui.input(|i| i.events.clone());
        for ev in events {
            match ev {
                // paste: it's the person's own action, so type the user's clipboard text as is
                egui::Event::Text(t) | egui::Event::Paste(t) => {
                    rec("type", serde_json::json!({ "text": t }), ws.mouse_pos());
                    send(TakeoverInput::Type(t));
                }
                // egui turns cmd+C / cmd+X (Windows: ctrl+C, ctrl+Insert / ctrl+X, shift+Delete) into Copy /
                // Cut instead of Key events, so forward them as that OS's combos. On Windows, Insert/Delete stay
                // as pressed (the copy/cut happens inside that screen; Electron's clipboard block only applies
                // to agents)
                egui::Event::Copy | egui::Event::Cut => {
                    let combo = crate::platform::input::takeover_clipboard_combo(matches!(ev, egui::Event::Copy), ui.input(|i| i.modifiers));
                    rec("key", serde_json::json!({ "text": combo }), ws.mouse_pos());
                    send(TakeoverInput::Key(combo));
                }
                // in-progress composition is only shown on screen (it only arrives on Windows, where the IME
                // is kept on)
                egui::Event::Ime(egui::ImeEvent::Preedit { text: t, .. }) if cfg!(windows) => {
                    a.preedit = t;
                }
                egui::Event::Ime(egui::ImeEvent::Commit(t)) => {
                    a.preedit.clear();
                    send(TakeoverInput::Type(t));
                }
                egui::Event::MouseWheel { delta, unit, .. } => {
                    let mul = match unit {
                        egui::MouseWheelUnit::Point => 1.0,
                        egui::MouseWheelUnit::Line => 40.0,
                        egui::MouseWheelUnit::Page => 600.0,
                    };
                    let (dx, dy) = (-(delta.x * mul) as f64, -(delta.y * mul) as f64);
                    let pos = resp.hover_pos().map(to_page);
                    if let Some(p) = pos {
                        let dir = if dy.abs() >= dx.abs() { if dy < 0.0 { "up" } else { "down" } } else if dx < 0.0 { "left" } else { "right" };
                        rec("scroll", serde_json::json!({ "coordinate": [p.0, p.1], "scroll_direction": dir }), p);
                    }
                    send(TakeoverInput::Scroll { pos, dx, dy });
                }
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    if let Some(combo) = egui_key_combo(key, modifiers) {
                        rec("key", serde_json::json!({ "text": combo }), ws.mouse_pos());
                        send(TakeoverInput::Key(combo));
                    }
                }
                _ => {}
            }
        }
        if !a.preedit.is_empty() {
            let painter = ui.painter();
            let at = resp.hover_pos().unwrap_or(rect.left_bottom()) + Vec2::new(14.0, 14.0);
            let galley = painter.layout_no_wrap(a.preedit.clone(), egui::FontId::proportional(16.0), Color32::WHITE);
            let bg = egui::Rect::from_min_size(at, galley.size()).expand(4.0);
            painter.rect_filled(bg, 4.0, theme::raised());
            painter.rect_stroke(bg, 4.0, egui::Stroke::new(1.0, theme::takeover()), egui::StrokeKind::Outside);
            painter.galley(at, galley, Color32::WHITE);
        }
    }
}

/// The takeover input queue and one task applying it in order. The task ends when the sender
/// (`AgentHandle`) goes away.
pub(super) fn spawn_input_queue(rt: &Handle, shared: Arc<AgentShared>) -> mpsc::UnboundedSender<TakeoverInput> {
    let (tx, mut rx) = mpsc::unbounded_channel::<TakeoverInput>();
    rt.spawn(async move {
        while let Some(ev) = rx.recv().await {
            if shared.removed.load(Ordering::SeqCst) {
                break;
            }
            if let Some(w) = shared.workspace() {
                let _ = ev.apply(&w).await;
            }
        }
    });
    tx
}

/// Placeholder before the first frame arrives: card background, centered spinner and text.
pub(super) fn preparing(ui: &egui::Ui, rect: egui::Rect, text: &str) {
    let p = ui.painter();
    p.rect_filled(rect, theme::CARD_RADIUS, theme::card());
    theme::paint_spinner(p, rect.center() - Vec2::new(0.0, 14.0), 22.0, theme::muted(), ui.input(|i| i.time));
    p.text(rect.center() + Vec2::new(0.0, 16.0), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(13.0), theme::muted());
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
}

/// Whether the agent is working (preparing, thinking, acting).
pub(super) fn is_working(st: &AgentStatus) -> bool {
    matches!(st, AgentStatus::Preparing(_) | AgentStatus::Thinking | AgentStatus::Acting(_))
}

fn browser_address(ui: &mut egui::Ui, browser: &Arc<crate::browser::BrowserWorkspace>, rt: &Handle, input: &mut String) {
    let current = browser.active_tab().map(|t| t.url).unwrap_or_default();
    let r = ui.add(egui::TextEdit::singleline(input).desired_width(f32::INFINITY)
        .margin(Margin::symmetric(10, 6)).hint_text(&current)).on_hover_text(&current);
    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !input.trim().is_empty() {
        let browser = browser.clone();
        let url = std::mem::take(input);
        rt.spawn(async move { let _ = browser.navigate(&url).await; });
    }
}

/// One log row: kind marker plus text (wrapped lines indent past the marker). The time shows on
/// hover.
///
/// Action rows look like steps: finished ones get ✓, the running one (`live`) a spinner.
pub(super) fn log_row(ui: &mut egui::Ui, l: &crate::agent::LogEntry, live: bool) {
    let (dot, color) = match l.kind {
        LogKind::User => (theme::info_text(), theme::text()),
        LogKind::Assistant => (theme::muted(), theme::text()),
        LogKind::Thinking => (theme::muted(), theme::muted()),
        LogKind::Action => (theme::success(), theme::body()),
        LogKind::Info => (theme::muted(), theme::muted()),
        LogKind::Error => (theme::danger(), theme::danger()),
    };
    let row = ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let (r, _) = ui.allocate_exact_size(Vec2::new(12.0, 18.0), egui::Sense::hover());
        let mark = egui::pos2(r.center().x, r.top() + 9.0);
        match (l.kind, live) {
            (LogKind::Action, true) => {
                theme::paint_spinner(ui.painter(), mark, 12.0, theme::text(), ui.input(|i| i.time));
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
            }
            (LogKind::Action, false) => {
                ui.painter().text(mark, egui::Align2::CENTER_CENTER, icon::CIRCLE_CHECK, egui::FontId::proportional(11.0), dot);
            }
            _ => {
                ui.painter().circle_filled(mark, 3.0, dot);
            }
        }
        let mut t = RichText::new(&l.text).color(color);
        match l.kind {
            LogKind::User => t = t.strong(),
            LogKind::Thinking => t = t.italics(),
            LogKind::Action | LogKind::Info => t = t.size(13.0),
            _ => {}
        }
        ui.add(egui::Label::new(t).wrap());
    });
    row.response.on_hover_text(trf("{}초", &[&format!("{:.1}", l.secs)]));
}

/// egui key event → xdotool-style combo. Plain characters arrive as Text events, so only combos
/// with modifiers are handled.
pub(super) fn egui_key_combo(key: egui::Key, m: egui::Modifiers) -> Option<String> {
    use egui::Key::*;
    let base = match key {
        Enter => "Return".to_string(),
        Tab => "Tab".into(),
        Backspace => "BackSpace".into(),
        Delete => "Delete".into(),
        Escape => "Escape".into(),
        ArrowUp => "Up".into(),
        ArrowDown => "Down".into(),
        ArrowLeft => "Left".into(),
        ArrowRight => "Right".into(),
        Home => "Home".into(),
        End => "End".into(),
        PageUp => "Page_Up".into(),
        PageDown => "Page_Down".into(),
        F1 => "F1".into(), F2 => "F2".into(), F3 => "F3".into(), F4 => "F4".into(),
        F5 => "F5".into(), F6 => "F6".into(), F7 => "F7".into(), F8 => "F8".into(),
        F9 => "F9".into(), F10 => "F10".into(), F11 => "F11".into(), F12 => "F12".into(),
        k => {
            let name = k.name();
            if name.chars().count() == 1 && (m.ctrl || m.mac_cmd || m.alt) {
                name.to_ascii_lowercase()
            } else {
                return None;
            }
        }
    };
    let mut s = String::new();
    if m.mac_cmd { s.push_str("cmd+"); }
    if m.ctrl { s.push_str("ctrl+"); }
    if m.alt { s.push_str("alt+"); }
    if m.shift { s.push_str("shift+"); }
    s.push_str(&base);
    Some(s)
}
