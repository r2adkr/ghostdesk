//! Picture-in-picture windows for agent screens.

use super::*;
use super::labels::{attention, status_text};

#[derive(Default)]
pub(super) struct PipState {
    pub(super) visible: bool,
    auto_open_eligible: bool,
}

impl PipState {
    fn update(&mut self, live_screen: bool, auto_open: bool, auto_close: bool) {
        let eligible = auto_open && live_screen;
        if eligible && !self.auto_open_eligible {
            self.visible = true;
        } else if auto_close && !live_screen {
            self.visible = false;
        }
        self.auto_open_eligible = eligible;
    }
}

impl GhostApp {
    /// Shows agent screens in small always-on-top windows, for keeping an eye on progress while
    /// doing something else. Dragging moves the window, clicking opens that agent in the main
    /// window, hovering reveals a close button, and the edges resize it.
    pub(super) fn pip_windows(&mut self, ctx: &egui::Context) {
        let mut open = None;
        for (i, a) in self.agents.iter_mut().enumerate() {
            if !self.pip_auto_open && !a.pip.visible {
                a.pip.auto_open_eligible = false;
                continue;
            }
            let workspace = a.shared.workspace();
            let live = workspace.as_ref().is_some_and(|desk| desk.has_screen() && !desk.is_closed());
            a.pip.update(live, self.pip_auto_open, self.pip_auto_close);
            if !a.pip.visible {
                continue;
            }
            if self.pip_auto_close && live {
                // A closed workspace can stop producing frames before it requests another repaint.
                ctx.request_repaint_after(std::time::Duration::from_secs(1));
            }
            let (sw, sh) = workspace.map(|w| w.size()).unwrap_or((1280, 800));
            let aspect = sh as f32 / sw.max(1) as f32;
            let tex = a.texture.as_ref().map(|t| t.id());
            let name = a.shared.name.clone();
            let st = a.shared.status();
            let takeover = a.takeover;
            let builder = egui::ViewportBuilder::default()
                .with_title(format!("{name} · GhostDesk"))
                .with_inner_size([400.0, 400.0 * aspect])
                .with_min_inner_size([200.0, 120.0])
                .with_decorations(false)
                .with_active(false)
                .with_always_on_top();
            let (mut close, mut clicked) = (false, false);
            ctx.show_viewport_immediate(egui::ViewportId::from_hash_of(("pip", a.shared.id)), builder, |ui, _| {
                let pctx = ui.ctx().clone();
                close |= pctx.input(|i| i.viewport().close_requested());
                egui::CentralPanel::default().frame(egui::Frame::new().fill(Color32::BLACK)).show(ui, |ui| {
                    let area = ui.max_rect();
                    let resp = ui.interact(area, ui.scope_id().with("pip"), egui::Sense::click_and_drag());
                    if resp.drag_started_by(egui::PointerButton::Primary) {
                        pctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    clicked |= resp.clicked();
                    let p = ui.painter();
                    // letterbox at the screen's aspect ratio
                    let fit = if aspect >= area.height() / area.width() {
                        Vec2::new(area.height() / aspect, area.height())
                    } else {
                        Vec2::new(area.width(), area.width() * aspect)
                    };
                    let img = egui::Rect::from_center_size(area.center(), fit);
                    match tex {
                        Some(t) => {
                            p.image(t, img, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                        }
                        None => {
                            theme::paint_spinner(p, area.center() - Vec2::new(0.0, 12.0), 20.0, Color32::from_gray(160), ui.input(|i| i.time));
                            p.text(area.center() + Vec2::new(0.0, 14.0), egui::Align2::CENTER_CENTER, tr("준비 중…"), egui::FontId::proportional(13.0), Color32::from_gray(160));
                            pctx.request_repaint_after(std::time::Duration::from_millis(33));
                        }
                    }
                    // colored border when a person is needed (question, error, takeover)
                    if let Some(c) = attention(&st, takeover) {
                        p.rect_stroke(area, 0, Stroke::new(3.0, c), StrokeKind::Inside);
                    }
                    if pctx.input(|i| i.pointer.hover_pos().is_some()) {
                        // name tag
                        let label = format!("{name}  ·  {}", crate::llm::short(&status_text(&st), 24));
                        let g = p.layout_no_wrap(label, egui::FontId::proportional(12.0), Color32::WHITE);
                        let at = area.left_top() + Vec2::new(10.0, 9.0);
                        p.rect_filled(egui::Rect::from_min_size(at, g.size()).expand2(Vec2::new(8.0, 4.0)), 255, Color32::from_black_alpha(170));
                        p.galley(at, g, Color32::WHITE);
                        // close button
                        let x = egui::Rect::from_min_size(egui::pos2(area.right() - 34.0, area.top() + 6.0), Vec2::splat(28.0));
                        let xr = ui.interact(x, ui.scope_id().with("pip_close"), egui::Sense::click()).on_hover_text(tr("PiP 닫기"));
                        let fill = if xr.hovered() { Color32::from_rgb(0xc4, 0x2b, 0x1c) } else { Color32::from_black_alpha(170) };
                        ui.painter().rect_filled(x, 255, fill);
                        ui.painter().text(x.center(), egui::Align2::CENTER_CENTER, icon::X, egui::FontId::proportional(15.0), Color32::WHITE);
                        close |= xr.clicked();
                    }
                });
                titlebar::resize_edges(&pctx);
            });
            if close {
                a.pip.visible = false;
            } else if clicked {
                open = Some(i);
            }
        }
        if let Some(i) = open {
            self.selected = Some(i);
            self.view = View::Detail;
            ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pip_opens_once_per_live_screen_and_respects_manual_dismissal() {
        let mut pip = PipState::default();
        pip.update(false, true, true);
        assert!(!pip.visible);
        pip.update(true, true, true);
        assert!(pip.visible);
        pip.visible = false;
        pip.update(true, true, true);
        assert!(!pip.visible);
        pip.update(false, true, true);
        pip.update(true, true, true);
        assert!(pip.visible);
        pip.update(false, true, true);
        assert!(!pip.visible);
    }

    #[test]
    fn pip_settings_act_independently_on_existing_windows() {
        let mut pip = PipState::default();
        pip.update(true, false, true);
        assert!(!pip.visible);
        pip.update(true, true, false);
        assert!(pip.visible);
        pip.update(true, false, false);
        pip.update(false, false, false);
        assert!(pip.visible);
        pip.update(false, false, true);
        assert!(!pip.visible);
    }
}
