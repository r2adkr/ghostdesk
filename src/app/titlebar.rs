//! 앱이 직접 그리는 제목 표시줄과 창 가장자리 (Windows · macOS).
//!
//! Windows 는 기본 창 틀을 없애고(`main.rs` 의 `with_decorations(false)`), macOS 는 내용을 제목 줄 밑까지 넓히고
//! 기본 제목 줄을 투명하게 숨긴 뒤(`with_fullsize_content_view`), 그 자리를 아래 열과 같은 색으로 나눠 칠한다:
//! 사이드바 위는 사이드바 색, 작업 영역 위는 작업 영역 색, 상세 보기의 오른쪽 패널 위는 그 패널 색.
//! 그래서 경계선 없이 각 열이 창 위끝까지 이어져 보인다. 앱 이름은 사이드바 머리 자리에 둔다.
//! 빈 곳을 끌면 창 이동, 두 번 누르면 최대화/복원. Windows 는 오른쪽 끝에 최소화·최대화·닫기를 그리고,
//! macOS 는 왼쪽 위의 기본 신호등 단추를 그대로 쓴다 (앱 이름은 그 오른쪽으로 비켜 둔다).
//! 테두리가 없어진 창의 크기 바꾸기는 `resize_edges` 가 가장자리에서 운영체제에 넘긴다.
//! Aero Snap·Windows 11 둥근 모서리·창 그림자는 winit 이 남겨 둔 창 스타일 덕분에 그대로 동작한다.

use eframe::egui::{self, pos2, vec2, Color32, CursorIcon, FontId, Rect, Sense, Stroke, ViewportCommand};

use super::i18n::tr;
use super::theme;

/// 높이: macOS 는 신호등 단추가 놓인 기본 제목 줄 높이(28)에 맞춘다
pub const HEIGHT: f32 = if cfg!(target_os = "macos") { 28.0 } else { 40.0 };
/// macOS 신호등 단추(닫기·최소화·확대)가 차지하는 왼쪽 너비
const MAC_TRAFFIC_W: f32 = 78.0;
/// 창 단추 하나의 너비 (Windows 기본 단추와 같은 46px)
const BUTTON_W: f32 = 46.0;
/// 창 크기를 바꾸는 가장자리 두께
const EDGE: f32 = 7.0;
/// 닫기 단추의 빨강 (Windows 기본과 같은 색: 사용자가 이미 아는 위험 신호)
const CLOSE_RED: Color32 = Color32::from_rgb(0xc4, 0x2b, 0x1c);
const CLOSE_RED_PRESSED: Color32 = Color32::from_rgb(0xa3, 0x23, 0x17);

/// 앱이 그린 제목 표시줄을 쓸 것인가: Windows·macOS 에서 전체 화면이 아닐 때
pub fn visible(ctx: &egui::Context) -> bool {
    (cfg!(windows) || cfg!(target_os = "macos")) && !ctx.input(|i| i.viewport().fullscreen.unwrap_or(false))
}

/// 제목 표시줄 아래에 놓인 옆 패널의 너비 (0 이면 그쪽 패널 없음)
#[derive(Clone, Copy, Default)]
pub struct Columns {
    pub left: f32,
    pub right: f32,
}

/// 제목 표시줄을 그린다. `Panel::top` 안에서 부른다.
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
    // 아래 패널 사이의 구분선을 위끝까지 잇는다 (egui 처럼 선 두께 반만큼 패널 안쪽에 그려야 같은 화소에 놓인다)
    let line = ui.visuals().widgets.noninteractive.bg_stroke;
    let half = 0.5 * line.width;
    if cols.left > 0.0 {
        p.vline(left.right() - half, rect.y_range(), line);
    }
    if cols.right > 0.0 {
        p.vline(right.left() + half, rect.y_range(), line);
    }

    // 앱 이름: 사이드바의 아이콘·글 줄과 같은 세로줄에 맞춘다 (사이드바 안쪽 여백 10 + 아이콘 가운데 18 / 글 36)
    // macOS 는 신호등 단추 오른쪽으로 비켜 둔다
    let mac = cfg!(target_os = "macos");
    // macOS: 신호등 단추의 가운데(위에서 15.5)에 맞춰 조금 내린다
    let cy = rect.center().y + if mac { 2.0 } else { 0.0 };
    let fg = theme::text();
    let x0 = rect.left() + if mac { MAC_TRAFFIC_W - 10.0 } else { 0.0 };
    let (icon_size, text_size) = if mac { (15.0, 13.0) } else { (17.0, 14.0) };
    p.text(pos2(x0 + 28.0, cy), egui::Align2::CENTER_CENTER, crate::icon::GHOST, FontId::proportional(icon_size), fg);
    p.text(pos2(x0 + 46.0, cy), egui::Align2::LEFT_CENTER, "GhostDesk", FontId::proportional(text_size), fg);

    // 빈 곳: 끌면 이동, 두 번 누르면 최대화/복원 (단추보다 먼저 만들어 단추가 위에서 눌리게)
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

    // macOS 는 창 단추를 운영체제가 그린다
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
        // 창이 앞에 있지 않으면 단추 글자를 흐리게 (Windows 기본 창과 같은 신호)
        _ => (Color32::TRANSPARENT, if focused { theme::text() } else { theme::muted() }),
    };
    let p = ui.painter();
    p.rect_filled(rect, 0, fill);
    // 1px 선을 화면 화소에 맞춰 또렷하게 그린다
    let ppp = ui.ctx().pixels_per_point();
    let snap = |v: f32| (v * ppp).round() / ppp + 0.5 / ppp;
    let c = pos2(snap(rect.center().x), snap(rect.center().y));
    let s = Stroke::new(1.0, fg);
    let h = 5.0; // 기호 반 크기 (10px)
    match kind {
        Caption::Minimize => {
            p.line_segment([pos2(c.x - h, c.y), pos2(c.x + h, c.y)], s);
        }
        Caption::Maximize => {
            p.rect_stroke(Rect::from_center_size(c, vec2(2.0 * h, 2.0 * h)), 1, s, egui::StrokeKind::Middle);
        }
        Caption::Restore => {
            // 앞 네모 + 뒤 네모의 위·오른쪽 모서리
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

/// 창 가장자리에서 크기 바꾸기. 모든 위젯을 그린 뒤 부른다 (가장자리 커서가 위젯 커서보다 우선).
/// 최대화·전체 화면일 때와 Windows 가 아닐 때는 아무것도 하지 않는다.
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

/// `pos` 가 창 `r` 의 어느 가장자리 위인가 (모서리는 조금 넓게 잡아 대각선으로 잡기 쉽게)
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
        // 모서리 근처의 가장자리는 대각선으로
        assert_eq!(at(10.0, 1.0), Some(D::NorthWest));
        assert_eq!(at(798.0, 590.0), Some(D::SouthEast));
        assert_eq!(at(900.0, 300.0), None);
    }
}
