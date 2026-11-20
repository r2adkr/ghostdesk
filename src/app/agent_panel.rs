//! The side panel of an agent that lives inside GhostDesk: the conversation with it (your instructions as
//! bubbles, its replies, a plan card, folded steps), its question pinned above the input, and one rounded
//! input box. Agents driven from another AI app (external) keep the plainer watch panel in `detail.rs`.
//!
//! Reading order, top to bottom: who and what state → the conversation → what needs an answer → the input.
//! Controls sit where the eye already is: pause and stop beside the status, taking over beside the name.

use super::detail::{is_working, split_choices, split_plan};
use super::labels::display_status;
use super::*;

/// Gaps of the panel (a 4-point rhythm): inside a group, between groups, between the big blocks.
const GAP_S: f32 = 4.0;
const GAP_M: f32 = 8.0;
const GAP_L: f32 = 16.0;
/// Corner of message bubbles and the input box.
const BUBBLE_RADIUS: u8 = 14;
/// Row height of a tappable choice.
const CHOICE_H: f32 = 36.0;

impl GhostApp {
    /// Panel of an in-app agent. Returns true when the person asked to remove it.
    pub(super) fn agent_panel(&mut self, ui: &mut egui::Ui, idx: usize) -> bool {
        let compact_now = self.compact_now;
        let layout = &mut self.detail_layout;
        let a = &mut self.agents[idx];
        let st = a.shared.status();
        let paused = a.shared.paused.load(Ordering::Relaxed);
        let working = is_working(&st) && !paused && !a.takeover;
        let workspace = a.shared.workspace();
        let has_screen = workspace.as_ref().is_none_or(|w| w.has_screen());
        let mut remove = false;

        // ── who: name, taking over, more ──────────────────────────────────────────────
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP_M;
            theme::dot(ui, crate::cursor::agent_color(a.shared.id), 5.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let more = egui::Button::new(RichText::new(icon::ELLIPSIS).size(16.0).color(theme::body())).frame_when_inactive(false).min_size(Vec2::splat(32.0));
                let (r, _) = egui::containers::menu::MenuButton::from_button(more).ui(ui, |ui| {
                    if !compact_now {
                        for (i, label) in [(0u8, tr("화면 크게")), (1, tr("반반")), (2, tr("채팅 크게"))] {
                            if ui.selectable_label(*layout == i, label).clicked() {
                                *layout = i;
                                ui.close();
                            }
                        }
                        ui.separator();
                    }
                    let can_pip = a.pip.visible || workspace.as_ref().is_some_and(|desk| desk.has_screen() && !desk.is_closed());
                    if can_pip
                        && ui
                            .selectable_label(a.pip.visible, tr("작은 창으로 띄우기"))
                            .on_hover_text(tr("작은 창(PiP)으로 띄우기 — 다른 일을 하면서 지켜볼 수 있어요"))
                            .clicked()
                    {
                        a.pip.visible = !a.pip.visible;
                        ui.close();
                    }
                    if ui.button(RichText::new(tr("에이전트 제거")).color(theme::danger())).on_hover_text(tr("에이전트 화면을 닫아요 (작업공간 폴더는 남아요)")).clicked() {
                        remove = true;
                        ui.close();
                    }
                });
                r.on_hover_text(tr("더 보기"));
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
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(&a.shared.name).size(16.0).strong().color(theme::text())).truncate()).on_hover_text(&a.shared.name);
                });
            });
        });

        // ── what state: one line of status, pause and stop at its end ─────────────────
        let tokens = *a.shared.tokens.lock();
        let (status, color) = display_status(&st, paused, a.takeover);
        let info = trf("단계 {} · 토큰 {} / {} · 작업공간 {}", &[&a.shared.steps.load(Ordering::Relaxed), &tokens.0, &tokens.1, &a.shared.workspace_name]);
        ui.add_space(GAP_S);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP_M;
            let controls = (is_working(&st) || paused) && !a.takeover;
            let controls_w = if controls { 72.0 } else { 0.0 };
            ui.allocate_ui_with_layout(Vec2::new((ui.available_width() - controls_w).max(60.0), 28.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                if working {
                    theme::spinner(ui, 14.0, theme::text());
                    theme::shimmer(ui, &crate::llm::short(&status, 40), 13.0).on_hover_text(&info);
                } else {
                    theme::chip(ui, color, &crate::llm::short(&status, 48)).on_hover_text(format!("{status}\n{info}"));
                }
            });
            if controls {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::icon_button(ui, icon::SQUARE, tr("지금 동작만 끝내고 멈춰요. 다시 지시하면 이어서 해요.")).clicked() {
                        a.shared.stop.store(true, Ordering::SeqCst);
                        a.shared.paused.store(false, Ordering::Relaxed);
                    }
                    if theme::icon_button(ui, if paused { icon::PLAY } else { icon::PAUSE }, if paused { tr("재개") } else { tr("일시정지") }).clicked() {
                        a.shared.paused.store(!paused, Ordering::Relaxed);
                    }
                });
            }
        });
        // a step limit shows as a thin bar, nothing otherwise
        let run = a.shared.run_steps.load(Ordering::Relaxed);
        if run > 0 && a.max_steps > 0 {
            ui.add_space(GAP_S);
            theme::progress(ui, egui::Id::unique(("steps", a.shared.id)), run as f32 / a.max_steps as f32, ui.available_width()).on_hover_text(trf("단계 {} / {}", &[&run, &a.max_steps]));
        }

        let files = a.shared.received_files.lock().clone();
        if !files.is_empty() {
            ui.add_space(GAP_M);
            theme::section(ui, tr("받은 파일"));
            egui::ScrollArea::vertical().id_salt(("received", a.shared.id)).max_height(120.0).show(ui, |ui| {
                for file in files.iter().rev() {
                    ui.label(theme::hint(file.path.display().to_string()));
                    ui.horizontal(|ui| {
                        if ui.link(tr("파일 열기")).clicked() {
                            crate::kit::reveal(&file.path);
                        }
                        if ui.link(tr("폴더 열기")).clicked() {
                            if let Some(parent) = file.path.parent() {
                                crate::kit::reveal(parent);
                            }
                        }
                    });
                }
            });
        }
        ui.add_space(GAP_M);

        // ── the pinned bottom: question, queue, input. Its height of the last frame is reserved, so the
        //    conversation shrinks first and the input never leaves the panel ─────────────
        let typing_id = egui::Id::new(("typing_answer", a.shared.id));
        let input_id = egui::Id::new(("composer_input", a.shared.id));
        let choices_pending = matches!(&st, AgentStatus::WaitingUser(q) if !split_choices(q).1.is_empty());
        if !choices_pending {
            ui.data_mut(|d| d.insert_temp(typing_id, false));
        }
        let typing: bool = ui.data(|d| d.get_temp(typing_id)).unwrap_or(false);
        let show_input = !choices_pending || typing;
        let bottom_id = egui::Id::unique(("agent_bottom_h", a.shared.id));
        let bottom_h: f32 = ui.data(|d| d.get_temp(bottom_id)).unwrap_or(120.0);
        let log_h = (ui.available_height() - bottom_h - GAP_L).max(0.0);
        if log_h >= 48.0 {
            egui::ScrollArea::vertical().id_salt("log").stick_to_bottom(true).auto_shrink([false, false]).max_height(log_h).show(ui, |ui| {
                let log = a.shared.log.lock();
                let acting = matches!(st, AgentStatus::Acting(_)) && !paused;
                chat_log(ui, &log, acting, a.shared.id);
            });
        }
        ui.add_space(GAP_L);

        let bottom = ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = GAP_M;
            if let AgentStatus::WaitingUser(q) = &st {
                let (body, choices) = split_choices(q);
                let mut picked = None;
                theme::soft_card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = GAP_M;
                    ui.label(theme::hint(format!("{}  {}", icon::CIRCLE_HELP, tr("에이전트의 질문"))));
                    egui::ScrollArea::vertical().id_salt("question").max_height(150.0).show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(body).color(theme::text())).wrap());
                    });
                    if !choices.is_empty() {
                        for c in &choices {
                            if choice_button(ui, c, theme::text()).clicked() {
                                picked = Some(c.clone());
                            }
                        }
                        if !typing && choice_button(ui, tr("직접 입력"), theme::muted()).clicked() {
                            ui.data_mut(|d| d.insert_temp(typing_id, true));
                            ui.ctx().memory_mut(|m| m.request_focus(input_id));
                        }
                    }
                });
                if let Some(c) = picked {
                    let _ = a.tx.send(AgentCmd::Answer(c));
                }
            }
            if show_input {
                let waiting = matches!(st, AgentStatus::WaitingUser(_));
                let mut remove_q = None;
                {
                    let q = a.shared.queue.lock();
                    if !q.is_empty() {
                        egui::ScrollArea::vertical().id_salt("queue").max_height(72.0).show(ui, |ui| {
                            for (i, t) in q.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    if ui.small_button(icon::X).on_hover_text(tr("대기열에서 빼기")).clicked() {
                                        remove_q = Some(i);
                                    }
                                    ui.label(theme::hint(trf("다음 {}. {}", &[&(i + 1), &crate::llm::short(t, 40)])));
                                });
                            }
                        });
                    }
                }
                if let Some(i) = remove_q {
                    a.shared.queue.lock().remove(i);
                }
                let sent = composer(ui, a, input_id, waiting, working);
                if let Some(cmd) = sent {
                    let _ = a.tx.send(cmd);
                }
            }
        });
        let h = bottom.response.rect.height();
        if (h - bottom_h).abs() > 0.5 {
            ui.data_mut(|d| d.insert_temp(bottom_id, h));
            ui.ctx().request_repaint();
        }
        remove
    }
}

/// A full-width answer row of a question.
fn choice_button(ui: &mut egui::Ui, label: &str, color: Color32) -> egui::Response {
    let resp = ui.add(egui::Button::new(RichText::new(label).color(color)).min_size(Vec2::new(ui.available_width(), CHOICE_H)));
    theme::focus_outline(ui, &resp, theme::RADIUS);
    resp
}

/// The input box: one rounded container with the text on top and, underneath, the key hint on the left and
/// send on the right. Enter sends, Shift+Enter breaks the line. Returns the command to send.
fn composer(ui: &mut egui::Ui, a: &mut AgentHandle, id: egui::Id, waiting: bool, working: bool) -> Option<AgentCmd> {
    let focused = ui.memory(|m| m.has_focus(id));
    let focus = ui.ctx().animate_bool_with_time(id.with("focus_t"), focused, 0.15);
    let mut cmd = None;
    let mut next_up = false;
    theme::composer_frame(focus).inner_margin(Margin::symmetric(12, 10)).shadow(egui::Shadow::NONE).corner_radius(BUBBLE_RADIUS).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let hint = if waiting { tr("답변을 입력하세요") } else { tr("지시를 입력하세요") };
        let edit = ui.add(
            egui::TextEdit::multiline(&mut a.input)
                .id(id)
                .hint_text(hint)
                .frame(egui::Frame::NONE)
                .desired_rows(1)
                .desired_width(f32::INFINITY)
                .return_key(Some(egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter))),
        );
        let enter = edit.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
        ui.add_space(GAP_S);
        let has_text = !a.input.trim().is_empty();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP_M;
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if send_button(ui, has_text, if waiting { tr("답변 보내기") } else { tr("보내기") }).clicked() && has_text || (enter && has_text) {
                    let text = a.input.trim().to_string();
                    cmd = Some(if waiting { AgentCmd::Answer(text) } else { AgentCmd::Instruction(text) });
                    a.input.clear();
                }
                if !waiting && working && has_text {
                    let resp = ui
                        .add(egui::Button::new(RichText::new(tr("다음 작업으로")).size(13.0).color(theme::body())).frame_when_inactive(false).min_size(Vec2::new(0.0, 28.0)))
                        .on_hover_text(tr("지금 일이 끝나면 이어서 할 일로 줄 세워 둬요. 멈춰 있을 때는 '보내기'로 먼저 다시 시작해 주세요."));
                    next_up = resp.clicked();
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if focus > 0.01 {
                        ui.label(theme::hint(tr("Enter 전송 · Shift+Enter 줄바꿈")).color(theme::muted().gamma_multiply(focus)));
                    }
                });
            });
        });
    });
    if next_up && !a.input.trim().is_empty() {
        a.shared.queue.lock().push_back(a.input.trim().to_string());
        a.input.clear();
        let _ = a.tx.send(AgentCmd::Wake);
    }
    cmd
}

/// Round send button: filled with the accent color while there is something to send, a quiet disc otherwise.
fn send_button(ui: &mut egui::Ui, active: bool, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(32.0), egui::Sense::click());
    let (fill, ink) = if active {
        let f = if resp.hovered() { theme::accent().lerp_to_gamma(theme::text(), 0.15) } else { theme::accent() };
        (f, theme::on_accent())
    } else {
        (theme::raised(), theme::muted())
    };
    ui.painter().circle_filled(rect.center(), 16.0, fill);
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, icon::ARROW_UP, egui::FontId::proportional(16.0), ink);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, active, tip));
    theme::focus_outline(ui, &resp, 16);
    resp.on_hover_text(tip)
}

/// The conversation: your messages as bubbles, replies as plain text (a numbered list becomes a plan card),
/// thinking and runs of actions folded into one line that opens.
pub(super) fn chat_log(ui: &mut egui::Ui, log: &[crate::agent::LogEntry], acting: bool, agent: u64) {
    ui.spacing_mut().item_spacing.y = GAP_M;
    let n = log.len();
    let mut i = 0;
    while i < n {
        let l = &log[i];
        match l.kind {
            LogKind::User => {
                // a new instruction starts a new turn: a little more air above it
                if i > 0 {
                    ui.add_space(GAP_S);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                    egui::Frame::new().fill(theme::raised()).corner_radius(BUBBLE_RADIUS).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
                        ui.set_max_width((ui.available_width() * 0.86).max(120.0));
                        // the text reads from the left even though the bubble sits on the right
                        ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                            ui.add(egui::Label::new(RichText::new(&l.text).color(theme::text())).wrap());
                        });
                    });
                });
                i += 1;
            }
            LogKind::Assistant => {
                match split_plan(&l.text) {
                    Some((before, steps, after)) => {
                        if !before.is_empty() {
                            ui.add(egui::Label::new(RichText::new(before).color(theme::text())).wrap());
                        }
                        theme::soft_card().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.spacing_mut().item_spacing.y = GAP_S + 2.0;
                            ui.label(theme::hint(format!("{}  {}", icon::LIST_TODO, tr("계획"))));
                            for (k, step) in steps.iter().enumerate() {
                                ui.horizontal_top(|ui| {
                                    ui.spacing_mut().item_spacing.x = GAP_M;
                                    // a fixed number column keeps every step's text on one edge
                                    let (r, _) = ui.allocate_exact_size(Vec2::new(14.0, 18.0), egui::Sense::hover());
                                    ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, k + 1, egui::FontId::proportional(12.0), theme::muted());
                                    ui.add(egui::Label::new(RichText::new(step).color(theme::text())).wrap());
                                });
                            }
                        });
                        if !after.is_empty() {
                            ui.add(egui::Label::new(RichText::new(after).color(theme::text())).wrap());
                        }
                    }
                    None => {
                        ui.add(egui::Label::new(RichText::new(&l.text).color(theme::text())).wrap());
                    }
                }
                i += 1;
            }
            LogKind::Thinking | LogKind::Action => {
                // the whole run of thinking and actions is one line
                let start = i;
                while i < n && matches!(log[i].kind, LogKind::Thinking | LogKind::Action) {
                    i += 1;
                }
                let run = &log[start..i];
                let live = acting && i == n && run.iter().any(|e| e.kind == LogKind::Action);
                let steps = run.iter().filter(|e| e.kind == LogKind::Action).count();
                let head = if steps == 0 {
                    tr("생각").to_string()
                } else if live {
                    run.iter().rev().find(|e| e.kind == LogKind::Action).map(|e| crate::llm::short(&e.text, 40)).unwrap_or_default()
                } else {
                    trf("작업 {}단계", &[&steps])
                };
                egui::CollapsingHeader::new(RichText::new(head).size(13.0).color(theme::muted()))
                    .id_salt(egui::Id::new(("chat_run", agent, start)))
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        let last_action = run.iter().rposition(|e| e.kind == LogKind::Action);
                        for (k, e) in run.iter().enumerate() {
                            super::detail::log_row(ui, e, live && Some(k) == last_action);
                        }
                    });
                if live {
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
                }
            }
            _ => {
                super::detail::log_row(ui, l, false);
                i += 1;
            }
        }
    }
}
