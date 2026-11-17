//! GhostDesk 화면 테마: 색·글자 크기·간격·모서리, 그리고 여러 화면에서 쓰는 작은 위젯 모양.
//!
//! 방향: Meta Astryx 의 Neutral 테마를 따른다. 회색 단계로 면을 나누고, 강조색은 무채색 하나
//! (어두운 테마에서는 거의 흰색, 밝은 테마에서는 거의 검은색 — 주 버튼·선택·포커스),
//! 색은 상태(초록 일하는 중·노랑 질문·빨강 오류·주황 직접 조작·파랑 정보)에만 쓴다.
//! 마우스를 올리거나 누를 때는 반투명 겹침색으로 바탕을 살짝 바꾼다. 그라데이션·유리 효과 없음.
//! 밝은·어두운 테마는 Windows/macOS 설정을 따른다 (`sync` 가 매 프레임 지금 테마를 고른다).

use std::sync::atomic::{AtomicBool, Ordering};

use super::i18n::tr;

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Response, RichText, Shadow, Stroke, StrokeKind, TextStyle,
    Vec2,
};

// ───────────────────────── 색 ─────────────────────────

/// 한 테마의 색 묶음. 값은 Astryx Neutral 의 팔레트 단계(neutral.dark/light[n])에서 가져왔다.
struct Palette {
    /// 가운데 작업 영역 바탕
    bg: Color32,
    /// 사이드바·옆 패널·제목 표시줄 (작업 영역보다 한 단계 가라앉은 면)
    surface: Color32,
    /// 카드·타일
    card: Color32,
    card_hover: Color32,
    /// 창(모달)·떠 있는 막대 바탕
    raised: Color32,
    border: Color32,
    border_hover: Color32,
    /// 창(모달) 가장자리 (어두운 테마: 빛이 닿은 테두리처럼 옅은 흰 선)
    rim: Color32,
    text: Color32,
    /// 보통 글 (라벨)
    body: Color32,
    muted: Color32,
    /// 주 버튼·선택 테두리 (무채색)
    accent: Color32,
    accent_hover: Color32,
    /// 강조색 위의 글
    on_accent: Color32,
    /// 선택된 항목 바탕
    accent_tint: Color32,
    /// 보조 버튼·선택된 줄의 반투명 바탕
    neutral: Color32,
    overlay_hover: Color32,
    overlay_pressed: Color32,
    /// 입력칸 바탕
    field: Color32,
    info: Color32,
    /// 정보 색 글 (링크·외부 에이전트·내가 보낸 지시)
    info_text: Color32,
    selection: Color32,
    success: Color32,
    warn: Color32,
    danger: Color32,
    /// 사람이 직접 조작 중일 때의 표시
    takeover: Color32,
    switch_off: Color32,
    knob_off: Color32,
    /// 창 그림자 진하기 (0~255)
    shadow: u8,
    popup_shadow: u8,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}
/// 흰색·검은색을 `a`(0~255) 만큼 비치게 (egui 는 미리 곱한 알파를 쓴다)
const fn white(a: u8) -> Color32 {
    Color32::from_rgba_premultiplied(a, a, a, a)
}
const fn black(a: u8) -> Color32 {
    Color32::from_rgba_premultiplied(0, 0, 0, a)
}

const DARK: Palette = Palette {
    bg: rgb(0x1b1b1b),          // neutral.dark[10] (--color-background-body)
    surface: rgb(0x111111),     // neutral.dark[5]
    card: rgb(0x262626),        // neutral.dark[15] (--color-background-surface)
    card_hover: rgb(0x303030),  // neutral.dark[20]
    raised: rgb(0x1b1b1b),      // --color-background-popover
    border: white(0x1A),        // --color-border
    border_hover: rgb(0x525252), // --color-border-emphasized
    rim: white(0x26),           // --shadow-high 의 흰 테두리 (15%)
    text: rgb(0xf1f1f1),
    body: rgb(0xd4d4d4),
    muted: rgb(0x9e9e9e),       // --color-text-secondary
    accent: rgb(0xf1f1f1),      // --color-accent
    accent_hover: rgb(0xd4d4d4),
    on_accent: rgb(0x111111),   // --color-on-accent
    accent_tint: rgb(0x474747), // --color-accent-muted
    neutral: white(0x1A),       // --color-neutral
    overlay_hover: white(0x0D), // --color-overlay-hover
    overlay_pressed: white(0x1A),
    field: rgb(0x111111),
    info: rgb(0x6d9cfe),        // --color-info
    info_text: rgb(0xa1caff),   // --color-text-info
    selection: Color32::from_rgba_premultiplied(33, 45, 61, 61), // blue.dark[75] 24%
    success: rgb(0x64af4c),
    warn: rgb(0xffce2f),
    danger: rgb(0xff705d),
    takeover: rgb(0xff9e55),    // orange.light[75]
    switch_off: rgb(0x525252),
    knob_off: rgb(0xf1f1f1),
    shadow: 178,
    popup_shadow: 128,
};

const LIGHT: Palette = Palette {
    bg: rgb(0xffffff),          // --color-background-surface
    surface: rgb(0xf1f1f1),     // neutral.light[95] (--color-background-body)
    card: rgb(0xffffff),
    card_hover: rgb(0xf1f1f1),
    raised: rgb(0xffffff),
    border: black(0x14),        // --color-border
    border_hover: rgb(0xd4d4d4), // --color-border-emphasized
    rim: black(0x14),
    text: rgb(0x111111),
    body: rgb(0x303030),
    muted: rgb(0x6a6a6a),       // neutral.light[45]
    accent: rgb(0x1b1b1b),      // --color-accent
    accent_hover: rgb(0x474747),
    on_accent: rgb(0xffffff),
    accent_tint: rgb(0xe2e2e2), // neutral.light[90]
    neutral: black(0x0F),       // --color-neutral
    overlay_hover: black(0x0D),
    overlay_pressed: black(0x1A),
    field: rgb(0xffffff),
    info: rgb(0x0074e2),
    info_text: rgb(0x0067ce),   // blue.light[45]
    selection: rgb(0xb9d7ff),   // blue.light[85]
    // 상태색은 밝은 바탕 위 글로도 읽히게 한 단계 진한 값 (--color-text-<status>)
    success: rgb(0x2f7d33),
    warn: rgb(0x836700),
    danger: rgb(0xc0262c),
    takeover: rgb(0xa14a00),
    switch_off: rgb(0xd4d4d4),
    knob_off: rgb(0xffffff),
    shadow: 38,
    popup_shadow: 26,
};

/// 지금 어두운 테마인가 (`sync` 가 매 프레임 갱신)
static DARK_MODE: AtomicBool = AtomicBool::new(true);

fn pal() -> &'static Palette {
    if DARK_MODE.load(Ordering::Relaxed) {
        &DARK
    } else {
        &LIGHT
    }
}

/// 매 프레임 맨 앞에서 부른다: 운영체제 설정에 따라 egui 가 고른 테마를 색 토큰에도 맞춘다.
pub fn sync(ctx: &egui::Context) {
    DARK_MODE.store(ctx.theme() == egui::Theme::Dark, Ordering::Relaxed);
}

pub fn bg() -> Color32 { pal().bg }
pub fn surface() -> Color32 { pal().surface }
pub fn card() -> Color32 { pal().card }
pub fn card_hover() -> Color32 { pal().card_hover }
pub fn raised() -> Color32 { pal().raised }
pub fn border() -> Color32 { pal().border }
pub fn border_hover() -> Color32 { pal().border_hover }
pub fn text() -> Color32 { pal().text }
pub fn body() -> Color32 { pal().body }
pub fn muted() -> Color32 { pal().muted }
pub fn accent() -> Color32 { pal().accent }
pub fn on_accent() -> Color32 { pal().on_accent }
pub fn overlay_hover() -> Color32 { pal().overlay_hover }
pub fn overlay_pressed() -> Color32 { pal().overlay_pressed }
pub fn info() -> Color32 { pal().info }
pub fn info_text() -> Color32 { pal().info_text }
pub fn success() -> Color32 { pal().success }
pub fn warn() -> Color32 { pal().warn }
pub fn danger() -> Color32 { pal().danger }
pub fn takeover() -> Color32 { pal().takeover }

/// 버튼·입력칸 모서리 (Astryx --radius-element)
pub const RADIUS: u8 = 10;
/// 카드·타일 (--radius-container)
pub const CARD_RADIUS: u8 = 12;
/// 창(모달)·큰 틀의 모서리
pub const MODAL_RADIUS: u8 = 16;

/// 앱 전체에 테마를 적용한다. 밝은·어두운 두 벌을 모두 만들어 두고, 어느 쪽을 쓸지는 운영체제 설정을 따른다.
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::System);
    for (theme, p) in [(egui::Theme::Dark, &DARK), (egui::Theme::Light, &LIGHT)] {
        ctx.style_mut_of(theme, |s| style(s, p, theme == egui::Theme::Dark));
    }
}

fn style(s: &mut egui::Style, p: &Palette, dark: bool) {
    // Astryx Neutral 의 글자 크기: 기준 14, 비율 1.2
    s.text_styles = [
        (TextStyle::Heading, FontId::new(20.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
    ]
    .into();
    // Astryx Neutral 의 빠른 움직임(125ms)
    s.animation_time = 0.125;

    let sp = &mut s.spacing;
    sp.item_spacing = Vec2::new(8.0, 8.0);
    sp.button_padding = Vec2::new(12.0, 6.0);
    sp.interact_size = Vec2::new(40.0, 28.0);
    sp.window_margin = Margin::same(16);
    sp.menu_margin = Margin::same(6);
    sp.indent = 16.0;

    let v = &mut s.visuals;
    v.dark_mode = dark;
    v.panel_fill = p.surface;
    v.window_fill = p.raised;
    v.window_stroke = Stroke::new(1.0, p.rim);
    v.window_corner_radius = CornerRadius::same(MODAL_RADIUS);
    v.menu_corner_radius = CornerRadius::same(CARD_RADIUS);
    v.window_shadow = Shadow { offset: [0, 12], blur: 24, spread: 0, color: Color32::from_black_alpha(p.shadow) };
    v.popup_shadow = Shadow { offset: [0, 4], blur: 12, spread: 0, color: Color32::from_black_alpha(p.popup_shadow) };
    v.extreme_bg_color = p.field;
    v.text_edit_bg_color = Some(p.field);
    v.faint_bg_color = p.card;
    v.code_bg_color = p.card;
    v.hyperlink_color = p.info_text;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.danger;
    v.selection.bg_fill = p.selection;
    v.selection.stroke = Stroke::new(1.0, p.text);
    v.indent_has_left_vline = false;
    v.collapsing_header_frame = false;
    v.interact_cursor = Some(egui::CursorIcon::PointingHand);
    v.text_cursor.stroke = Stroke::new(2.0, p.accent);
    v.handle_shape = egui::style::HandleShape::Circle;

    let r = CornerRadius::same(RADIUS);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.body);
    w.noninteractive.corner_radius = r;

    // 보조 버튼은 반투명 바탕(--color-neutral)이라 어느 면 위에서도 같은 단계만큼 떠 보인다
    w.inactive.weak_bg_fill = p.neutral;
    w.inactive.bg_fill = p.switch_off;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r;
    w.inactive.expansion = 0.0;

    w.hovered.weak_bg_fill = over(p.neutral, p.overlay_hover);
    w.hovered.bg_fill = p.border_hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.border_hover);
    w.hovered.fg_stroke = Stroke::new(1.5, p.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;

    // 누르면 살짝 작아진다 (Astryx: scale 0.98 + 눌림 겹침색)
    w.active.weak_bg_fill = over(p.neutral, p.overlay_pressed);
    w.active.bg_fill = p.border_hover;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.5, p.text);
    w.active.corner_radius = r;
    w.active.expansion = -1.0;

    w.open.weak_bg_fill = over(p.neutral, p.overlay_hover);
    w.open.bg_fill = p.card_hover;
    w.open.bg_stroke = Stroke::new(1.0, p.border_hover);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = r;
}

/// 반투명 색 두 개를 겹친 색 (미리 곱한 알파끼리의 source-over)
fn over(below: Color32, above: Color32) -> Color32 {
    let k = 255 - above.a() as u16;
    let mix = |b: u8, a: u8| (a as u16 + b as u16 * k / 255) as u8;
    Color32::from_rgba_premultiplied(
        mix(below.r(), above.r()),
        mix(below.g(), above.g()),
        mix(below.b(), above.b()),
        mix(below.a(), above.a()),
    )
}

// ───────────────────────── 틀 ─────────────────────────

/// 둥근 카드
pub fn card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(card())
        .stroke(Stroke::new(1.0, border()))
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
        .fill(raised())
        .stroke(Stroke::new(1.0, pal().rim))
        .corner_radius(MODAL_RADIUS)
        .inner_margin(Margin::same(24))
        .shadow(Shadow { offset: [0, 12], blur: 24, spread: 0, color: Color32::from_black_alpha(pal().shadow) })
}

/// 떠 있는 작은 막대 (화면 모아보기의 단추 묶음 등)
pub fn floating_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(raised())
        .stroke(Stroke::new(1.0, pal().rim))
        .corner_radius(CARD_RADIUS)
        .inner_margin(Margin::same(6))
        .shadow(Shadow { offset: [0, 4], blur: 12, spread: 0, color: Color32::from_black_alpha(pal().popup_shadow) })
}

// ───────────────────────── 글 ─────────────────────────

/// 묶음 이름 (작고 흐린 글)
pub fn section(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(muted()).strong());
}

/// 흐린 보조 설명
pub fn hint(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).size(12.0).color(muted())
}

/// 모달 머리: 제목 + 닫기(✕). 닫기를 누르면 true.
pub fn modal_header(ui: &mut egui::Ui, title: &str) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(18.0).strong().color(text()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            close = icon_button(ui, crate::icon::X, tr("닫기 (Esc)")).clicked();
        });
    });
    ui.add_space(8.0);
    close
}

// ───────────────────────── 버튼 ─────────────────────────

/// 바탕색이 정해진 버튼. 마우스를 올리면 바탕을 바꾼다 (egui 는 직접 정한 바탕을 바꾸지 않으므로
/// 지난 프레임의 같은 버튼이 마우스 아래 있었는지로 고른다).
fn filled(ui: &mut egui::Ui, label: RichText, size: Vec2, radius: u8) -> Response {
    let id = ui.next_auto_id();
    let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered() && r.enabled());
    let h = ui.ctx().animate_bool_with_time(id.with("hover_t"), hovered, 0.12);
    let fill = accent().lerp_to_gamma(pal().accent_hover, h);
    ui.add(egui::Button::new(label.color(on_accent()).strong()).fill(fill).stroke(Stroke::NONE).corner_radius(radius).min_size(size))
}

/// 주 동작 버튼 (강조색)
pub fn primary(ui: &mut egui::Ui, text: impl Into<String>) -> Response {
    filled(ui, RichText::new(text.into()), Vec2::new(0.0, 32.0), RADIUS)
}

/// 보조 버튼 (반투명 바탕)
pub fn secondary(ui: &mut egui::Ui, label: impl Into<String>) -> Response {
    ui.add(egui::Button::new(RichText::new(label.into()).color(text())).min_size(Vec2::new(0.0, 32.0)))
}

/// 아이콘만 있는 버튼 (마우스를 올릴 때만 바탕이 보임)
pub fn icon_button(ui: &mut egui::Ui, icon: &str, tip: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(icon).size(16.0).color(body()))
            .frame_when_inactive(false)
            .min_size(Vec2::splat(32.0)),
    )
    .on_hover_text(tip)
}

/// 켜고 끄는 버튼 (켜지면 색을 입힘)
pub fn toggle(ui: &mut egui::Ui, on: bool, label: &str, color: Color32) -> Response {
    let b = egui::Button::new(RichText::new(label).color(if on { color } else { text() })).min_size(Vec2::new(0.0, 32.0));
    let b = if on { b.fill(color.gamma_multiply(0.14)).stroke(Stroke::new(1.0, color)) } else { b };
    ui.add(b)
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
        let galley = ui.painter().layout_no_wrap(self.text.clone(), FontId::proportional(14.0), if enabled { text() } else { muted() });
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
            let p = pal();
            let fill = lerp_color(p.switch_off, p.accent, t);
            let knob = lerp_color(p.knob_off, p.on_accent, t);
            let painter = ui.painter();
            painter.rect_filled(tr, 255, if enabled { fill } else { fill.gamma_multiply(0.5) });
            if resp.hovered() && enabled {
                painter.rect_stroke(tr, 255, Stroke::new(1.0, p.border_hover), StrokeKind::Outside);
            }
            let r = track.y / 2.0 - 2.5;
            let cx = egui::lerp((tr.left() + track.y / 2.0)..=(tr.right() - track.y / 2.0), t);
            painter.circle_filled(egui::pos2(cx, tr.center().y), r, if enabled { knob } else { p.muted });
            painter.galley(egui::pos2(tr.right() + 8.0, rect.center().y - galley.size().y / 2.0), galley, p.text);
        }
        resp
    }
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

/// 아이콘 켜고 끄기 (켜지면 선택 바탕, 꺼지면 마우스를 올릴 때만 바탕)
pub fn icon_toggle(ui: &mut egui::Ui, on: bool, icon: &str, tip: &str) -> Response {
    let b = egui::Button::new(RichText::new(icon).size(15.0).color(if on { text() } else { muted() })).min_size(Vec2::splat(32.0));
    let b = if on { b.fill(pal().accent_tint).stroke(Stroke::NONE) } else { b.frame_when_inactive(false) };
    ui.add(b).on_hover_text(tip)
}

// ───────────────────────── 사이드바 ─────────────────────────

/// 사이드바 한 줄: 아이콘 · 글 · (오른쪽 상태 점). 선택되면 옅은 바탕, 마우스를 올리면 더 옅은 바탕.
pub fn nav_item(ui: &mut egui::Ui, selected: bool, icon: &str, label: &str, badge: Option<Color32>) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, selected, label));
    if ui.is_rect_visible(rect) {
        let h = hover_t(ui.ctx(), &resp);
        let p = ui.painter();
        let pressed = resp.is_pointer_button_down_on();
        if selected || pressed {
            p.rect_filled(rect, RADIUS, if pressed { over(pal().neutral, pal().overlay_pressed) } else { pal().neutral });
        } else if h > 0.0 {
            p.rect_filled(rect, RADIUS, overlay_hover().gamma_multiply(h));
        }
        let fg = if selected { text() } else { body().lerp_to_gamma(text(), h) };
        p.text(egui::pos2(rect.left() + 18.0, rect.center().y), egui::Align2::CENTER_CENTER, icon, FontId::proportional(16.0), fg);
        p.text(egui::pos2(rect.left() + 36.0, rect.center().y), egui::Align2::LEFT_CENTER, label, FontId::proportional(14.0), fg);
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
        let h = hover_t(ui.ctx(), &resp);
        let p = ui.painter();
        let fill = accent().lerp_to_gamma(pal().accent_hover, h);
        let r = if resp.is_pointer_button_down_on() { 16.0 } else { 17.0 };
        p.circle_filled(rect.center(), r, fill);
        p.text(rect.center(), egui::Align2::CENTER_CENTER, crate::icon::CORNER_DOWN_LEFT, FontId::proportional(17.0), on_accent());
    }
    resp.on_hover_text(tip)
}

/// 큰 입력 상자의 틀 (처음 화면의 '맡길 일' 입력). `focus`: 0(평소)~1(입력 중), 테두리가 서서히 진해진다
pub fn composer_frame(focus: f32) -> egui::Frame {
    egui::Frame::new()
        .fill(card())
        .stroke(Stroke::new(1.0, border().lerp_to_gamma(border_hover(), focus)))
        .corner_radius(MODAL_RADIUS)
        .inner_margin(Margin::symmetric(16, 12))
        .shadow(Shadow { offset: [0, 4], blur: 12, spread: 0, color: Color32::from_black_alpha(pal().popup_shadow / 2) })
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

/// 작은 개수 배지 (예: 사이드바 '에이전트' 옆의 숫자). 글자 크기에 맞춘 알약 모양을 줄 가운데에 그린다.
/// Frame 으로 감싸면 가로 줄의 높이만큼 위아래로 늘어나므로, 크기를 정해 두고 직접 그린다.
pub fn count_badge(ui: &mut egui::Ui, text: &str) -> Response {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(11.0), muted());
    let pad = Vec2::new(6.0, 2.0);
    let h = galley.size().y + pad.y * 2.0;
    let size = Vec2::new((galley.size().x + pad.x * 2.0).max(h), h);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 255, card());
    p.galley(rect.center() - galley.size() / 2.0, galley, muted());
    resp
}

/// 고를 수 있는 낮은 타일 (아이콘 + 제목을 가운데 한 줄로). 누르면 clicked.
pub fn tile(ui: &mut egui::Ui, selected: bool, icon: &str, title: &str, size: Vec2) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let p = pal();
    let h = hover_t(ui.ctx(), &resp);
    let sel = ui.ctx().animate_bool_with_time(resp.id.with("sel_t"), selected, 0.15);
    let fill = p.card.lerp_to_gamma(p.card_hover, h).lerp_to_gamma(p.accent_tint, sel);
    let stroke = Stroke::new(1.0, p.border.lerp_to_gamma(p.border_hover, h).lerp_to_gamma(p.accent, sel));
    let painter = ui.painter();
    painter.rect(rect, CARD_RADIUS, fill, stroke, StrokeKind::Inside);
    // 아이콘과 제목을 한 덩어리로 보고 가운데에 둔다 (칸 폭이 좁아도 글자가 잘리지 않게)
    let ig = painter.layout_no_wrap(icon.to_owned(), FontId::proportional(17.0), if selected { p.text } else { p.body });
    let tg = painter.layout_no_wrap(title.to_owned(), FontId::proportional(14.0), p.text);
    let (is, ts, gap) = (ig.size(), tg.size(), 8.0);
    let x = rect.center().x - (is.x + gap + ts.x) / 2.0;
    let cy = rect.center().y;
    painter.galley(egui::pos2(x, cy - is.y / 2.0), ig, p.text);
    painter.galley(egui::pos2(x + is.x + gap, cy - ts.y / 2.0), tg, p.text);
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

// ───────────────────────── 움직임 ─────────────────────────
//
// 모든 움직임은 짧고(0.12~0.25초) 끝에서 느려진다. 계속 도는 표시(스피너·반짝이는 글)는
// 초당 30번만 다시 그려 CPU 를 아낀다.

/// 계속 움직이는 표시의 다시 그리기 간격 (약 30fps)
const TICK: std::time::Duration = std::time::Duration::from_millis(33);

/// 끝에서 부드럽게 멈추는 곡선 (0..=1 → 0..=1)
pub fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// 0 에서 1 로 차오르는 '나타나기' 값. 처음 그릴 때, `key` 가 바뀔 때, 또는 지난 패스에 그리지 않았다가
/// 다시 그릴 때 0 부터 시작한다. 화면 전환 · 새 기록 줄 · 알림을 서서히 보이게 할 때 쓴다.
pub fn appear(ctx: &egui::Context, id: egui::Id, key: u64, dur: f32) -> f32 {
    let now = ctx.input(|i| i.time);
    let pass = ctx.cumulative_pass_nr();
    let start = ctx.data_mut(|d| {
        let start = match d.get_temp::<(f64, u64, u64)>(id) {
            Some((start, last, k)) if k == key && last + 1 >= pass => start,
            _ => now,
        };
        d.insert_temp(id, (start, pass, key));
        start
    });
    let t = ((now - start) as f32 / dur).min(1.0);
    if t < 1.0 {
        ctx.request_repaint();
    }
    ease_out(t)
}

/// 마우스를 올리고 뗄 때 0↔1 로 부드럽게 바뀌는 값 (바탕·테두리 색을 서서히 바꿀 때)
pub fn hover_t(ctx: &egui::Context, resp: &Response) -> f32 {
    ctx.animate_bool_with_time(resp.id.with("hover_t"), resp.hovered() || resp.has_focus(), 0.12)
}

/// 일하는 중 표시: 여덟 갈래 빛살이 돌면서 뒤쪽부터 옅어진다
pub fn spinner(ui: &mut egui::Ui, size: f32, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        paint_spinner(ui.painter(), rect.center(), size, color, ui.input(|i| i.time));
        ui.ctx().request_repaint_after(TICK);
    }
    resp
}

/// `spinner` 를 정해진 자리에 그린다 (화면 준비 중 자리 등). 다시 그리기는 부르는 쪽이 요청한다.
pub fn paint_spinner(p: &egui::Painter, c: egui::Pos2, size: f32, color: Color32, time: f64) {
    const RAYS: usize = 8;
    // 한 바퀴 0.9초. 빛살이 이어서 도는 것처럼 보이게 정수 단위가 아니라 연속으로 옅어진다
    let head = ((time / 0.9).fract() * RAYS as f64) as f32;
    let width = (size * 0.1).max(1.4);
    for k in 0..RAYS {
        let a = k as f32 / RAYS as f32 * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
        let age = (head - k as f32).rem_euclid(RAYS as f32) / RAYS as f32;
        let alpha = 0.18 + 0.82 * (1.0 - age).powf(1.6);
        let dir = Vec2::new(a.cos(), a.sin());
        p.line_segment([c + dir * size * 0.2, c + dir * size * 0.45], Stroke::new(width, color.gamma_multiply(alpha)));
    }
}

/// 밝은 띠가 글자 위를 지나가는 글 ('생각 중…' 처럼 지금 일하는 중인 상태)
pub fn shimmer(ui: &mut egui::Ui, text: &str, size: f32) -> Response {
    let (base, hi) = (muted(), pal().text);
    let n = text.chars().count() as f32;
    // 띠가 글 앞뒤로 조금 벗어났다가 다시 들어온다 (한 번 지나가는 데 1.6초)
    let head = ((ui.input(|i| i.time) / 1.6).fract() as f32) * (n + 8.0) - 4.0;
    let mut job = egui::text::LayoutJob::default();
    let mut buf = [0u8; 4];
    for (i, ch) in text.chars().enumerate() {
        let d = ((i as f32 - head).abs() / 3.0).min(1.0);
        let fmt = egui::TextFormat { font_id: FontId::proportional(size), color: hi.lerp_to_gamma(base, ease_out(d)), ..Default::default() };
        job.append(ch.encode_utf8(&mut buf), 0.0, fmt);
    }
    ui.ctx().request_repaint_after(TICK);
    ui.add(egui::Label::new(job).selectable(false))
}

/// 얇은 진행 막대 (0..=1). 값이 바뀌면 막대가 부드럽게 늘어난다.
pub fn progress(ui: &mut egui::Ui, id: egui::Id, frac: f32, width: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 4.0), egui::Sense::hover());
    let f = ui.ctx().animate_value_with_time(id, frac.clamp(0.0, 1.0), 0.25);
    let p = ui.painter();
    p.rect_filled(rect, 255, pal().neutral);
    if f > 0.0 {
        let fill = egui::Rect::from_min_size(rect.min, Vec2::new((rect.width() * f).max(rect.height()), rect.height()));
        p.rect_filled(fill, 255, pal().accent);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 대비 (WCAG): 글은 4.5:1 이상
    fn contrast(a: Color32, b: Color32) -> f32 {
        let lum = |c: Color32| {
            let ch = |v: u8| {
                let v = v as f32 / 255.0;
                if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
        };
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn text_tokens_meet_contrast_on_every_surface() {
        for p in [&DARK, &LIGHT] {
            for ground in [p.bg, p.surface, p.card, p.raised] {
                for (name, fg) in [
                    ("text", p.text), ("body", p.body), ("muted", p.muted), ("info", p.info_text),
                    ("success", p.success), ("warn", p.warn), ("danger", p.danger), ("takeover", p.takeover),
                ] {
                    let c = contrast(fg, ground);
                    assert!(c >= 4.5, "{name} {fg:?} on {ground:?}: {c:.2}");
                }
            }
            assert!(contrast(p.on_accent, p.accent) >= 4.5);
            assert!(contrast(p.on_accent, p.accent_hover) >= 4.5);
            assert!(contrast(p.text, p.accent_tint) >= 4.5);
        }
    }

    #[test]
    fn overlay_composes_premultiplied() {
        assert_eq!(over(Color32::TRANSPARENT, white(0x1A)), white(0x1A));
        let o = over(white(0x1A), white(0x0D));
        assert!(o.a() > 0x1A && o.r() == o.a());
    }
}
