//! Custom-drawn title bar and window edges (Windows and macOS).
//!
//! Windows drops the native frame (`with_decorations(false)` in `main.rs`); macOS extends the
//! content under the title bar and hides the native one (`with_fullsize_content_view`). The
//! strip is then painted in the colors of the columns below it: sidebar color above the
//! sidebar, workspace color above the workspace, and the right panel's color above the detail
//! view's right panel, so every column runs to the top of the window without a seam. The app
//! name sits in the sidebar's header spot.
//!
//! Dragging empty space moves the window and double-clicking maximizes/restores. Windows gets
//! minimize, maximize and close drawn at the right end; macOS keeps the native traffic lights at
//! the top left (the app name moves right to clear them). With the frame gone, `resize_edges`
//! hands edge drags to the OS for resizing. Aero Snap, Windows 11 rounded corners and the window
//! shadow keep working thanks to the window styles winit leaves in place.

use eframe::egui::{self, pos2, vec2, Color32, CursorIcon, FontId, Rect, Sense, Stroke, ViewportCommand};

use super::i18n::tr;
use super::theme;

/// Height. On macOS it matches the native title bar that holds the traffic lights (28).
pub const HEIGHT: f32 = if cfg!(target_os = "macos") { 28.0 } else { 40.0 };
/// Width taken by the macOS traffic lights (close, minimize, zoom) on the left.
const MAC_TRAFFIC_W: f32 = 78.0;
/// Width of one window button (46 px, same as the native Windows buttons).
const BUTTON_W: f32 = 46.0;
/// Thickness of the resize edges.
const EDGE: f32 = 7.0;
/// Close button red, the same as native Windows: a danger signal users already know.
const CLOSE_RED: Color32 = Color32::from_rgb(0xc4, 0x2b, 0x1c);
const CLOSE_RED_PRESSED: Color32 = Color32::from_rgb(0xa3, 0x23, 0x17);

/// Whether to use the custom title bar: on Windows and macOS, when not full screen.
pub fn visible(ctx: &egui::Context) -> bool {
    (cfg!(windows) || cfg!(target_os = "macos")) && !ctx.input(|i| i.viewport().fullscreen.unwrap_or(false))
}

/// Widths of the side panels below the title bar (0 = no panel on that side).
#[derive(Clone, Copy, Default)]
pub struct Columns {
    pub left: f32,
    pub right: f32,
}

/// Draws the title bar. Call inside `Panel::top`.
pub fn bar(ui: &mut egui::Ui, cols: Columns) {
    let ctx = ui.ctx().clone();
    let rect = ui.max_rect();
    let p = ui.painter().clone();
    p.rect_filled(rect, 0, theme::bg());
    let left = Rect::from_min_size(rect.min, vec2(cols.left, rect.height()));
    let right = Rect::from_min_max(pos2(rect.right() - cols.right, rect.top()), rect.max);
    if cols.left > 0.0 {
        p.rect_filled(left, 0, theme::surface());
    }
    if cols.right > 0.0 {
        p.rect_filled(right, 0, theme::surface());
    }
    // extend the separators between the panels below up to the top. Like egui, draw half the stroke
    // width inside the panel so it lands on the same pixels.
    let line = ui.visuals().widgets.noninteractive.bg_stroke;
    let half = 0.5 * line.width;
    if cols.left > 0.0 {
        p.vline(left.right() - half, rect.y_range(), line);
    }
    if cols.right > 0.0 {
        p.vline(right.left() + half, rect.y_range(), line);
    }

    // app name: aligned with the sidebar's icon/text columns (sidebar inner margin 10 + icon center
    // 18 / text 36); on macOS it moves right of the traffic lights
    let mac = cfg!(target_os = "macos");
    // macOS: nudge down to the traffic lights' center (15.5 from the top)
    let cy = rect.center().y + if mac { 2.0 } else { 0.0 };
    let fg = theme::text();
    let x0 = rect.left() + if mac { MAC_TRAFFIC_W - 10.0 } else { 0.0 };
    let (icon_size, text_size) = if mac { (15.0, 13.0) } else { (17.0, 14.0) };
    p.text(pos2(x0 + 28.0, cy), egui::Align2::CENTER_CENTER, crate::icon::GHOST, FontId::proportional(icon_size), fg);
    p.text(pos2(x0 + 46.0, cy), egui::Align2::LEFT_CENTER, "GhostDesk", FontId::proportional(text_size), fg);

    // empty space: drag to move, double-click to maximize/restore. Created before the buttons so
    // the buttons win clicks on top.
    let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
    let drag_rect = if mac {
        rect.with_min_x(rect.left() + MAC_TRAFFIC_W)
    } else {
        rect.with_max_x(rect.right() - BUTTON_W * 3.0)
    };
    let drag = ui.interact(drag_rect, ui.scope_id().with("titlebar_drag"), Sense::click_and_drag());
    if drag.double_clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    } else if drag.drag_started_by(egui::PointerButton::Primary) {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }

    // macOS draws the window buttons natively
    if mac {
        return;
    }
    let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
    let mut x = rect.right() - BUTTON_W;
    for kind in [Caption::Close, if maximized { Caption::Restore } else { Caption::Maximize }, Caption::Minimize] {
        let r = Rect::from_min_size(pos2(x, rect.top()), vec2(BUTTON_W, rect.height()));
        if caption_button(ui, r, kind, focused).clicked() {
            ctx.send_viewport_cmd(match kind {
                Caption::Close => ViewportCommand::Close,
                Caption::Maximize => ViewportCommand::Maximized(true),
                Caption::Restore => ViewportCommand::Maximized(false),
                Caption::Minimize => ViewportCommand::Minimized(true),
            });
        }
        x -= BUTTON_W;
    }
}

/// Title bar of a small fixed-size window (the installer): app name, drag to move, minimize and
/// close only.
pub fn dialog_bar(ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let rect = ui.max_rect();
    let p = ui.painter().clone();
    p.rect_filled(rect, 0, theme::bg());
    let fg = theme::text();
    p.text(pos2(rect.left() + 28.0, rect.center().y), egui::Align2::CENTER_CENTER, crate::icon::GHOST, FontId::proportional(17.0), fg);
    p.text(pos2(rect.left() + 46.0, rect.center().y), egui::Align2::LEFT_CENTER, "GhostDesk", FontId::proportional(14.0), fg);
    let drag = ui.interact(rect.with_max_x(rect.right() - BUTTON_W * 2.0), ui.scope_id().with("dialog_drag"), Sense::drag());
    if drag.drag_started_by(egui::PointerButton::Primary) {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }
    let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
    let mut x = rect.right() - BUTTON_W;
    for kind in [Caption::Close, Caption::Minimize] {
        let r = Rect::from_min_size(pos2(x, rect.top()), vec2(BUTTON_W, rect.height()));
        if caption_button(ui, r, kind, focused).clicked() {
            ctx.send_viewport_cmd(if kind == Caption::Close { ViewportCommand::Close } else { ViewportCommand::Minimized(true) });
        }
        x -= BUTTON_W;
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Caption {
    Minimize,
    Maximize,
    Restore,
    Close,
}

fn caption_button(ui: &mut egui::Ui, rect: Rect, kind: Caption, focused: bool) -> egui::Response {
    let (label, tip) = match kind {
        Caption::Minimize => (tr("최소화"), tr("최소화")),
        Caption::Maximize => (tr("최대화"), tr("최대화")),
        Caption::Restore => (tr("이전 크기로"), tr("이전 크기로 복원")),
        Caption::Close => (tr("닫기"), tr("닫기")),
    };
    let resp = ui.interact(rect, ui.scope_id().with(("caption", label)), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
    let pressed = resp.is_pointer_button_down_on();
    let hot = resp.hovered() || pressed;
    let (fill, fg) = match (kind, hot) {
        (Caption::Close, true) => (if pressed { CLOSE_RED_PRESSED } else { CLOSE_RED }, Color32::WHITE),
        (_, true) => (if pressed { theme::overlay_pressed() } else { theme::overlay_hover() }, theme::text()),
        // dim the glyphs when the window isn't focused (same cue as native Windows windows)
        _ => (Color32::TRANSPARENT, if focused { theme::text() } else { theme::muted() }),
    };
    let p = ui.painter();
    p.rect_filled(rect, 0, fill);
    // snap the 1 px lines to device pixels so they stay crisp
    let ppp = ui.ctx().pixels_per_point();
    let snap = |v: f32| (v * ppp).round() / ppp + 0.5 / ppp;
    let c = pos2(snap(rect.center().x), snap(rect.center().y));
    let s = Stroke::new(1.0, fg);
    let h = 5.0; // half glyph size (10 px)
    match kind {
        Caption::Minimize => {
            p.line_segment([pos2(c.x - h, c.y), pos2(c.x + h, c.y)], s);
        }
        Caption::Maximize => {
            p.rect_stroke(Rect::from_center_size(c, vec2(2.0 * h, 2.0 * h)), 1, s, egui::StrokeKind::Middle);
        }
        Caption::Restore => {
            // front square plus the top and right edges of the back one
            let front = Rect::from_center_size(c + vec2(-1.0, 1.0), vec2(8.0, 8.0));
            p.rect_stroke(front, 1, s, egui::StrokeKind::Middle);
            let (l, t, r, b) = (front.left() + 2.0, front.top() - 2.0, front.right() + 2.0, front.bottom() - 2.0);
            p.line_segment([pos2(l, t), pos2(r, t)], s);
            p.line_segment([pos2(r, t), pos2(r, b)], s);
        }
        Caption::Close => {
            p.line_segment([pos2(c.x - h, c.y - h), pos2(c.x + h, c.y + h)], s);
            p.line_segment([pos2(c.x - h, c.y + h), pos2(c.x + h, c.y - h)], s);
        }
    }
    resp.on_hover_text(tip)
}

/// Resizing from the window edges. Call after drawing every widget (the edge cursor takes
/// priority over widget cursors).
///
/// Does nothing when maximized or full screen, or when not on Windows.
pub fn resize_edges(ctx: &egui::Context) {
    if !cfg!(windows) || ctx.input(|i| i.viewport().maximized.unwrap_or(false) || i.viewport().fullscreen.unwrap_or(false)) {
        return;
    }
    let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else { return };
    let Some(dir) = edge_at(ctx.viewport_rect(), pos) else { return };
    use egui::ResizeDirection as D;
    ctx.set_cursor_icon(match dir {
        D::North => CursorIcon::ResizeNorth,
        D::South => CursorIcon::ResizeSouth,
        D::East => CursorIcon::ResizeEast,
        D::West => CursorIcon::ResizeWest,
        D::NorthEast => CursorIcon::ResizeNorthEast,
        D::NorthWest => CursorIcon::ResizeNorthWest,
        D::SouthEast => CursorIcon::ResizeSouthEast,
        D::SouthWest => CursorIcon::ResizeSouthWest,
    });
    if ctx.input(|i| i.pointer.primary_pressed()) {
        ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
    }
}

/// Which edge of window `r` the point `pos` is over. Corners are grabbed a bit generously so
/// diagonal resizing is easy to hit.
fn edge_at(r: Rect, pos: egui::Pos2) -> Option<egui::ResizeDirection> {
    use egui::ResizeDirection as D;
    if !r.expand(1.0).contains(pos) {
        return None;
    }
    let corner = EDGE * 3.0;
    let (l, rt, t, b) = (pos.x - r.left() < EDGE, r.right() - pos.x < EDGE, pos.y - r.top() < EDGE, r.bottom() - pos.y < EDGE);
    let (lc, rc, tc, bc) =
        (pos.x - r.left() < corner, r.right() - pos.x < corner, pos.y - r.top() < corner, r.bottom() - pos.y < corner);
    Some(match () {
        _ if (t && lc) || (l && tc) => D::NorthWest,
        _ if (t && rc) || (rt && tc) => D::NorthEast,
        _ if (b && lc) || (l && bc) => D::SouthWest,
        _ if (b && rc) || (rt && bc) => D::SouthEast,
        _ if t => D::North,
        _ if b => D::South,
        _ if l => D::West,
        _ if rt => D::East,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::ResizeDirection as D;

    #[test]
    fn edges_and_corners() {
        let r = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let at = |x, y| edge_at(r, pos2(x, y));
        assert_eq!(at(400.0, 300.0), None);
        assert_eq!(at(400.0, 1.0), Some(D::North));
        assert_eq!(at(400.0, 598.0), Some(D::South));
        assert_eq!(at(1.0, 300.0), Some(D::West));
        assert_eq!(at(799.0, 300.0), Some(D::East));
        assert_eq!(at(2.0, 2.0), Some(D::NorthWest));
        // edges near a corner resize diagonally
        assert_eq!(at(10.0, 1.0), Some(D::NorthWest));
        assert_eq!(at(798.0, 590.0), Some(D::SouthEast));
        assert_eq!(at(900.0, 300.0), None);
    }
}
