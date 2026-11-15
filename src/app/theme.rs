//! GhostDesk 화면 테마: 색·글자 크기·간격·모서리, 그리고 여러 화면에서 쓰는 작은 위젯 모양.
//!
//! 방향: 왼쪽 사이드바(새 에이전트·보기·에이전트 목록·연결·설정) + 넓고 조용한 작업 영역.
//! 바탕은 살짝 푸른 기가 도는 무채색, 강조색은 파랑 하나(주 버튼·선택·포커스에만),
//! 상태색(초록 일하는 중·노랑 질문·빨강 오류·주황 직접 조작)은 상태에만. 그라데이션·유리 효과 없음.

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Response, RichText, Shadow, Stroke, StrokeKind, TextStyle,
    Vec2,
};

// ───────────────────────── 색 ─────────────────────────

/// 가운데 작업 영역 바탕
pub const BG: Color32 = Color32::from_rgb(22, 23, 26);
/// 상단 바·옆 패널
pub const SURFACE: Color32 = Color32::from_rgb(17, 18, 21);
/// 카드·버튼
pub const CARD: Color32 = Color32::from_rgb(32, 34, 38);
pub const CARD_HOVER: Color32 = Color32::from_rgb(40, 42, 47);
/// 창(모달) 바탕
pub const RAISED: Color32 = Color32::from_rgb(30, 31, 35);
pub const BORDER: Color32 = Color32::from_rgb(48, 50, 56);
pub const BORDER_HOVER: Color32 = Color32::from_rgb(74, 77, 86);
pub const TEXT: Color32 = Color32::from_rgb(240, 242, 245);
/// 보통 글 (라벨)
pub const BODY: Color32 = Color32::from_rgb(205, 208, 214);
pub const MUTED: Color32 = Color32::from_rgb(148, 152, 161);
pub const ACCENT: Color32 = Color32::from_rgb(47, 111, 235);
pub const ACCENT_TEXT: Color32 = Color32::from_rgb(125, 170, 250);
/// 선택된 항목 바탕 (강조색을 어둡게 섞은 색)
pub const ACCENT_TINT: Color32 = Color32::from_rgb(26, 40, 66);
pub const SUCCESS: Color32 = Color32::from_rgb(92, 184, 128);
pub const WARN: Color32 = Color32::from_rgb(229, 183, 72);
pub const DANGER: Color32 = Color32::from_rgb(232, 106, 96);
/// 사람이 직접 조작 중일 때의 표시
pub const TAKEOVER: Color32 = Color32::from_rgb(240, 146, 58);

pub const RADIUS: u8 = 8;
pub const CARD_RADIUS: u8 = 12;
/// 창(모달)·큰 틀의 모서리
pub const MODAL_RADIUS: u8 = 16;
/// 꺼진 스위치 바탕
const SWITCH_OFF: Color32 = Color32::from_rgb(70, 73, 80);

/// 앱 전체에 테마를 적용한다 (시스템이 밝은 테마여도 어두운 테마 하나로 통일).
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        s.text_styles = [
            (TextStyle::Heading, FontId::new(20.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
        ]
        .into();

        let sp = &mut s.spacing;
        sp.item_spacing = Vec2::new(8.0, 8.0);
        sp.button_padding = Vec2::new(12.0, 8.0);
        sp.interact_size = Vec2::new(40.0, 28.0);
        sp.window_margin = Margin::same(16);
        sp.menu_margin = Margin::same(8);
        sp.indent = 16.0;

        let v = &mut s.visuals;
        v.dark_mode = true;
        v.panel_fill = SURFACE;
        v.window_fill = RAISED;
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.window_corner_radius = CornerRadius::same(MODAL_RADIUS);
        v.menu_corner_radius = CornerRadius::same(CARD_RADIUS);
        v.window_shadow = Shadow { offset: [0, 12], blur: 32, spread: 0, color: Color32::from_black_alpha(140) };
        v.popup_shadow = Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(110) };
        v.extreme_bg_color = Color32::from_rgb(28, 29, 33);
        v.faint_bg_color = Color32::from_rgb(36, 38, 42);
        v.code_bg_color = CARD;
        v.hyperlink_color = ACCENT_TEXT;
        v.warn_fg_color = WARN;
        v.error_fg_color = DANGER;
        v.selection.bg_fill = Color32::from_rgb(36, 64, 120);
        v.selection.stroke = Stroke::new(1.0, ACCENT_TEXT);
        v.indent_has_left_vline = false;
        v.collapsing_header_frame = false;
        v.interact_cursor = Some(egui::CursorIcon::PointingHand);
        v.text_cursor.stroke = Stroke::new(2.0, ACCENT_TEXT);

        let r = CornerRadius::same(RADIUS);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = SURFACE;
        w.noninteractive.weak_bg_fill = SURFACE;
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0, BODY);
        w.noninteractive.corner_radius = r;

        w.inactive.weak_bg_fill = CARD;
        w.inactive.bg_fill = Color32::from_rgb(52, 54, 60);
        // 체크박스 칸·입력칸 테두리가 바탕에 묻히지 않게 조금 밝게
        w.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(66, 69, 76));
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.corner_radius = r;
        w.inactive.expansion = 0.0;

        w.hovered.weak_bg_fill = CARD_HOVER;
        w.hovered.bg_fill = Color32::from_rgb(60, 63, 70);
        w.hovered.bg_stroke = Stroke::new(1.0, BORDER_HOVER);
        w.hovered.fg_stroke = Stroke::new(1.5, TEXT);
        w.hovered.corner_radius = r;
        w.hovered.expansion = 0.0;

        w.active.weak_bg_fill = Color32::from_rgb(56, 59, 66);
        w.active.bg_fill = Color32::from_rgb(56, 59, 66);
        w.active.bg_stroke = Stroke::new(1.0, ACCENT);
        w.active.fg_stroke = Stroke::new(1.5, TEXT);
        w.active.corner_radius = r;
        w.active.expansion = 0.0;

        w.open.weak_bg_fill = CARD_HOVER;
        w.open.bg_fill = CARD_HOVER;
        w.open.bg_stroke = Stroke::new(1.0, BORDER_HOVER);
        w.open.fg_stroke = Stroke::new(1.0, TEXT);
        w.open.corner_radius = r;
    });
}

// ───────────────────────── 틀 ─────────────────────────

/// 둥근 카드
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CARD_RADIUS)
        .inner_margin(Margin::same(16))
}

/// 색을 살짝 입힌 안내 카드 (질문·외부 에이전트·경고)
pub fn tinted(color: Color32) -> egui::Frame {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.10))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.40)))
        .corner_radius(CARD_RADIUS)
        .inner_margin(Margin::symmetric(12, 10))
}

/// 모달 창의 틀
pub fn modal_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(RAISED)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(MODAL_RADIUS)
        .inner_margin(Margin::same(24))
        .shadow(Shadow { offset: [0, 16], blur: 40, spread: 0, color: Color32::from_black_alpha(150) })
}

// ───────────────────────── 글 ─────────────────────────

/// 묶음 이름 (작고 흐린 글)
pub fn section(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(MUTED).strong());
}

/// 흐린 보조 설명
pub fn hint(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).size(12.0).color(MUTED)
}

/// 모달 머리: 제목 + 닫기(✕). 닫기를 누르면 true.
pub fn modal_header(ui: &mut egui::Ui, title: &str) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(18.0).strong().color(TEXT));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            close = icon_button(ui, crate::icon::X, "닫기 (Esc)").clicked();
        });
    });
    ui.add_space(8.0);
    close
}

// ───────────────────────── 버튼 ─────────────────────────

/// 주 동작 버튼 (강조색)
pub fn primary(ui: &mut egui::Ui, text: impl Into<String>) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text.into()).color(Color32::WHITE).strong())
            .fill(ACCENT)
            .stroke(Stroke::NONE)
            .corner_radius(RADIUS)
            .min_size(Vec2::new(0.0, 32.0)),
    )
}

/// 보조 버튼 (카드 색 + 얇은 테두리)
pub fn secondary(ui: &mut egui::Ui, text: impl Into<String>) -> Response {
    ui.add(egui::Button::new(RichText::new(text.into()).color(TEXT)).min_size(Vec2::new(0.0, 32.0)))
}

/// 큰 주 동작 버튼 (처음 화면 등)
pub fn primary_big(ui: &mut egui::Ui, text: impl Into<String>, width: f32) -> Response {
    ui.add_sized(
        [width, 44.0],
        egui::Button::new(RichText::new(text.into()).size(15.0).color(Color32::WHITE).strong())
            .fill(ACCENT)
            .stroke(Stroke::NONE)
            .corner_radius(CARD_RADIUS),
    )
}

pub fn secondary_big(ui: &mut egui::Ui, text: impl Into<String>, width: f32) -> Response {
    ui.add_sized([width, 44.0], egui::Button::new(RichText::new(text.into()).size(15.0).color(TEXT)).corner_radius(CARD_RADIUS))
}

/// 아이콘만 있는 버튼 (마우스를 올릴 때만 바탕이 보임)
pub fn icon_button(ui: &mut egui::Ui, icon: &str, tip: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(icon).size(16.0).color(BODY))
            .frame_when_inactive(false)
            .min_size(Vec2::splat(32.0)),
    )
    .on_hover_text(tip)
}

/// 켜고 끄는 버튼 (켜지면 색을 입힘)
pub fn toggle(ui: &mut egui::Ui, on: bool, text: &str, color: Color32) -> Response {
    let (fill, stroke, fg) = if on {
        (color.gamma_multiply(0.14), Stroke::new(1.0, color), color)
    } else {
        (CARD, Stroke::new(1.0, BORDER), TEXT)
    };
    ui.add(
        egui::Button::new(RichText::new(text).color(fg))
            .fill(fill)
            .stroke(stroke)
            .min_size(Vec2::new(0.0, 32.0)),
    )
}

/// 켜고 끄는 스위치 (체크박스 대신). `ui.add(theme::switch(&mut on, "글"))`, `add_enabled` 와도 같이 쓴다.
pub fn switch(on: &mut bool, text: impl Into<String>) -> Switch<'_> {
    Switch { on, text: text.into() }
}

pub struct Switch<'a> {
    on: &'a mut bool,
    text: String,
}

impl egui::Widget for Switch<'_> {
    fn ui(self, ui: &mut egui::Ui) -> Response {
        let enabled = ui.is_enabled();
        let track = Vec2::new(30.0, 17.0);
        let galley = ui.painter().layout_no_wrap(self.text.clone(), FontId::proportional(14.0), if enabled { TEXT } else { MUTED });
        let size = Vec2::new(track.x + 8.0 + galley.size().x, ui.spacing().interact_size.y.max(galley.size().y));
        let (rect, mut resp) = ui.allocate_exact_size(size, egui::Sense::click());
        if resp.clicked() {
            *self.on = !*self.on;
            resp.mark_changed();
        }
        let on = *self.on;
        resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, enabled, on, &self.text));
        if ui.is_rect_visible(rect) {
            let t = ui.ctx().animate_bool_responsive(resp.id, on);
            let tr = egui::Rect::from_min_size(egui::pos2(rect.left(), rect.center().y - track.y / 2.0), track);
            let off = SWITCH_OFF;
            let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
            let fill = Color32::from_rgb(mix(off.r(), ACCENT.r()), mix(off.g(), ACCENT.g()), mix(off.b(), ACCENT.b()));
            let p = ui.painter();
            p.rect_filled(tr, 255, if enabled { fill } else { fill.gamma_multiply(0.5) });
            if resp.hovered() && enabled {
                p.rect_stroke(tr, 255, Stroke::new(1.0, BORDER_HOVER), StrokeKind::Outside);
            }
            let r = track.y / 2.0 - 2.5;
            let cx = egui::lerp((tr.left() + track.y / 2.0)..=(tr.right() - track.y / 2.0), t);
            p.circle_filled(egui::pos2(cx, tr.center().y), r, if enabled { TEXT } else { MUTED });
            p.galley(egui::pos2(tr.right() + 8.0, rect.center().y - galley.size().y / 2.0), galley, TEXT);
        }
        resp
    }
}

/// 아이콘 켜고 끄기 (켜지면 강조색 바탕, 꺼지면 마우스를 올릴 때만 바탕)
pub fn icon_toggle(ui: &mut egui::Ui, on: bool, icon: &str, tip: &str) -> Response {
    let b = egui::Button::new(RichText::new(icon).size(15.0).color(if on { ACCENT_TEXT } else { MUTED })).min_size(Vec2::splat(32.0));
    let b = if on { b.fill(ACCENT_TINT).stroke(Stroke::new(1.0, ACCENT)) } else { b.frame_when_inactive(false) };
    ui.add(b).on_hover_text(tip)
}

// ───────────────────────── 사이드바 ─────────────────────────

/// 사이드바 한 줄: 아이콘 · 글 · (오른쪽 상태 점). 선택되면 옅은 바탕, 마우스를 올리면 더 옅은 바탕.
pub fn nav_item(ui: &mut egui::Ui, selected: bool, icon: &str, text: &str, badge: Option<Color32>) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, selected, text));
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if selected {
            p.rect_filled(rect, RADIUS, CARD);
        } else if resp.hovered() {
            p.rect_filled(rect, RADIUS, Color32::from_white_alpha(8));
        }
        let fg = if selected || resp.hovered() { TEXT } else { BODY };
        p.text(egui::pos2(rect.left() + 18.0, rect.center().y), egui::Align2::CENTER_CENTER, icon, FontId::proportional(16.0), fg);
        p.text(egui::pos2(rect.left() + 36.0, rect.center().y), egui::Align2::LEFT_CENTER, text, FontId::proportional(14.0), fg);
        if let Some(c) = badge {
            p.circle_filled(egui::pos2(rect.right() - 14.0, rect.center().y), 3.5, c);
        }
    }
    resp
}

/// 보내기 버튼: 강조색 동그라미 안에 Enter 아이콘 (Lucide corner-down-left)
pub fn enter_button(ui: &mut egui::Ui, tip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(34.0), egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, tip));
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let fill = if resp.hovered() { ACCENT_TEXT } else { ACCENT };
        p.circle_filled(rect.center(), 17.0, fill);
        p.text(rect.center(), egui::Align2::CENTER_CENTER, crate::icon::CORNER_DOWN_LEFT, FontId::proportional(17.0), Color32::WHITE);
    }
    resp.on_hover_text(tip)
}

/// 큰 입력 상자의 틀 (처음 화면의 '맡길 일' 입력)
pub fn composer_frame(focused: bool) -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, if focused { BORDER_HOVER } else { BORDER }))
        .corner_radius(MODAL_RADIUS)
        .inner_margin(Margin::symmetric(16, 12))
        .shadow(Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(60) })
}

// ───────────────────────── 작은 표시 ─────────────────────────

/// 동그란 점 (상태·에이전트 색)
pub fn dot(ui: &mut egui::Ui, color: Color32, radius: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(radius * 2.0 + 2.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), radius, color);
    resp
}

/// 상태 칩: ● 글 (색을 옅게 깐 알약 모양). 점과 글을 한 덩어리 글로 그려 주변 정렬 방향과 상관없이 같은 모양.
pub fn chip(ui: &mut egui::Ui, color: Color32, text: &str) -> Response {
    let fmt = |size: f32| egui::TextFormat {
        font_id: FontId::proportional(size),
        color,
        valign: egui::Align::Center,
        ..Default::default()
    };
    let mut job = egui::text::LayoutJob::default();
    job.append("● ", 0.0, fmt(9.0));
    job.append(text, 0.0, fmt(12.0));
    egui::Frame::new()
        .fill(color.gamma_multiply(0.14))
        .corner_radius(255)
        .inner_margin(Margin::symmetric(9, 3))
        .show(ui, |ui| {
            ui.add(egui::Label::new(job).truncate().selectable(false));
        })
        .response
}

/// 고를 수 있는 큰 타일 (아이콘 · 제목 · 한 줄 설명). 누르면 clicked.
pub fn tile(ui: &mut egui::Ui, selected: bool, icon: &str, title: &str, sub: &str, size: Vec2) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let hovered = resp.hovered();
    let (fill, stroke) = if selected {
        (ACCENT_TINT, Stroke::new(1.0, ACCENT))
    } else if hovered {
        (CARD_HOVER, Stroke::new(1.0, BORDER_HOVER))
    } else {
        (CARD, Stroke::new(1.0, BORDER))
    };
    let p = ui.painter();
    p.rect(rect, CARD_RADIUS, fill, stroke, StrokeKind::Inside);
    let x = rect.left() + 16.0;
    p.text(egui::pos2(x, rect.top() + 16.0), egui::Align2::LEFT_TOP, icon, FontId::proportional(20.0), if selected { ACCENT_TEXT } else { BODY });
    p.text(
        egui::pos2(x, rect.bottom() - 40.0),
        egui::Align2::LEFT_TOP,
        title,
        FontId::proportional(14.0),
        TEXT,
    );
    p.text(egui::pos2(x, rect.bottom() - 21.0), egui::Align2::LEFT_TOP, sub, FontId::proportional(12.0), MUTED);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, selected, title));
    resp
}

/// 모달 안의 세로 스크롤 영역. 높이가 아직 내용(최대 max_h)에 맞지 않으면(처음 열 때·내용이 바뀔 때 몇 프레임)
/// 다시 그리도록 해서, 입력이 없어도 창이 내용 크기에 맞게 자리 잡게 한다.
pub fn modal_scroll<R>(ui: &mut egui::Ui, max_h: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let out = egui::ScrollArea::vertical()
        .max_height(max_h)
        .min_scrolled_height(max_h)
        .auto_shrink([false, true])
        .show(ui, add);
    // 내용이 늘거나 줄었는데(예: 다른 종류를 고름) 아직 높이가 맞지 않으면 한 번 더 그려 맞춘다
    // (화면이 아주 작아 끝내 맞출 수 없는 경우 계속 다시 그리지 않게 연속 10번까지만)
    let shown = out.inner_rect.height();
    let want = out.content_size.y.min(max_h);
    // (내용 높이가 바뀌면 새로 맞추는 것이므로 횟수를 다시 센다)
    let tries_id = out.id.with("settle");
    let (tries, last_want): (u32, f32) = ui.data(|d| d.get_temp(tries_id)).unwrap_or((0, want));
    let tries = if (last_want - want).abs() > 1.0 { 0 } else { tries };
    if (shown - want).abs() > 1.0 && tries < 10 {
        ui.data_mut(|d| d.insert_temp(tries_id, (tries + 1, want)));
        ui.ctx().request_repaint();
    } else if (shown - want).abs() <= 1.0 && (tries != 0 || (last_want - want).abs() > 1.0) {
        ui.data_mut(|d| d.insert_temp(tries_id, (0u32, want)));
    }
    out.inner
}
