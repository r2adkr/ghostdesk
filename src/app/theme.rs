//! GhostDesk's UI theme: colors, type sizes, spacing, corners, and small widgets shared across
//! screens.
//!
//! Direction: follows Meta Astryx's Neutral theme. Surfaces are separated by gray steps, with a
//! single achromatic accent (near white in dark mode, near black in light mode) for primary
//! buttons, selection and focus. Color is reserved for status (green working, yellow question,
//! red error, orange takeover, blue info). Hover and press tint the background with translucent
//! overlays. No gradients or glass effects. Light/dark follows the Windows/macOS setting (`sync`
//! picks the current theme every frame).

use std::sync::atomic::{AtomicBool, Ordering};

use super::i18n::tr;

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Response, RichText, Shadow, Stroke, StrokeKind, TextStyle,
    Vec2,
};

// ---- colors ----

/// One theme's colors, taken from the Astryx Neutral palette steps (neutral.dark/light[n]).
struct Palette {
    /// Background of the central work area.
    bg: Color32,
    /// Sidebar, side panels and title bar (one step below the work area).
    surface: Color32,
    /// Cards and tiles.
    card: Color32,
    card_hover: Color32,
    /// Modal windows and floating bars.
    raised: Color32,
    border: Color32,
    border_hover: Color32,
    /// Modal edge (dark theme: a faint white line, like light catching the rim).
    rim: Color32,
    text: Color32,
    /// Regular text (labels).
    body: Color32,
    muted: Color32,
    /// Primary buttons and selection borders (achromatic).
    accent: Color32,
    accent_hover: Color32,
    /// Text on the accent color.
    on_accent: Color32,
    /// Background of a selected item.
    accent_tint: Color32,
    /// Translucent background of secondary buttons and selected rows.
    neutral: Color32,
    overlay_hover: Color32,
    overlay_pressed: Color32,
    /// Input field background.
    field: Color32,
    info: Color32,
    /// Info-colored text (links, external agents, instructions you sent).
    info_text: Color32,
    selection: Color32,
    success: Color32,
    warn: Color32,
    danger: Color32,
    /// Marker for a person taking over.
    takeover: Color32,
    switch_off: Color32,
    knob_off: Color32,
    /// Window shadow strength (0–255).
    shadow: u8,
    popup_shadow: u8,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}
/// White/black at opacity `a` (0–255); egui uses premultiplied alpha.
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
    rim: white(0x26), // white rim of --shadow-high (15%)
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
    // status colors one step darker so they read as text on light backgrounds (--color-text-<status>)
    success: rgb(0x2f7d33),
    warn: rgb(0x836700),
    danger: rgb(0xc0262c),
    takeover: rgb(0xa14a00),
    switch_off: rgb(0xd4d4d4),
    knob_off: rgb(0xffffff),
    shadow: 38,
    popup_shadow: 26,
};

/// Whether the dark theme is active (updated every frame by `sync`).
static DARK_MODE: AtomicBool = AtomicBool::new(true);

fn pal() -> &'static Palette {
    if DARK_MODE.load(Ordering::Relaxed) {
        &DARK
    } else {
        &LIGHT
    }
}

/// Call first thing every frame: matches the color tokens to the theme egui picked from the OS
/// setting.
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

/// Corner radius of buttons and fields (Astryx --radius-element).
pub const RADIUS: u8 = 10;
/// Cards and tiles (--radius-container).
pub const CARD_RADIUS: u8 = 12;
/// Corner radius of modals and large frames.
pub const MODAL_RADIUS: u8 = 16;

/// Applies the theme to the whole app. Both light and dark variants are built; which one is used
/// follows the OS setting.
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::System);
    for (theme, p) in [(egui::Theme::Dark, &DARK), (egui::Theme::Light, &LIGHT)] {
        ctx.style_mut_of(theme, |s| style(s, p, theme == egui::Theme::Dark));
    }
}

fn style(s: &mut egui::Style, p: &Palette, dark: bool) {
    // Astryx Neutral type scale: base 14, ratio 1.2
    s.text_styles = [
        (TextStyle::Heading, FontId::new(20.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
    ]
    .into();
    // Astryx Neutral fast motion (125 ms)
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

    // secondary buttons use a translucent background (--color-neutral), so they lift by the same step
    // on any surface
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

    // shrinks slightly when pressed (Astryx: scale 0.98 + pressed overlay)
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

/// Two translucent colors composited (source-over on premultiplied alpha).
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

// ---- frames ----

/// Rounded card.
pub fn card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(card())
        .stroke(Stroke::new(1.0, border()))
        .corner_radius(CARD_RADIUS)
        .inner_margin(Margin::same(16))
}

/// Lightly tinted notice card (question, external agent, warning).
pub fn tinted(color: Color32) -> egui::Frame {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.10))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.40)))
        .corner_radius(CARD_RADIUS)
        .inner_margin(Margin::symmetric(12, 10))
}

/// Frame of a modal window.
pub fn modal_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(raised())
        .stroke(Stroke::new(1.0, pal().rim))
        .corner_radius(MODAL_RADIUS)
        .inner_margin(Margin::same(24))
        .shadow(Shadow { offset: [0, 12], blur: 24, spread: 0, color: Color32::from_black_alpha(pal().shadow) })
}

/// A small floating bar (e.g. the button group on the screen wall).
pub fn floating_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(raised())
        .stroke(Stroke::new(1.0, pal().rim))
        .corner_radius(CARD_RADIUS)
        .inner_margin(Margin::same(6))
        .shadow(Shadow { offset: [0, 4], blur: 12, spread: 0, color: Color32::from_black_alpha(pal().popup_shadow) })
}

// ---- text ----

/// Section label (small, dimmed text).
pub fn section(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(muted()).strong());
}

/// Dimmed secondary text.
pub fn hint(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).size(12.0).color(muted())
}

/// Modal header: title + close (✕). Returns true when close is clicked.
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

// ---- buttons ----

/// A button with a fixed fill that changes on hover. egui won't change an explicit fill, so the
/// hover state comes from whether the same button was under the pointer last frame.
fn filled(ui: &mut egui::Ui, label: RichText, size: Vec2, radius: u8) -> Response {
    filled_with(ui, label, size, radius, accent(), pal().accent_hover, on_accent())
}

fn filled_with(ui: &mut egui::Ui, label: RichText, size: Vec2, radius: u8, base: Color32, hover: Color32, fg: Color32) -> Response {
    let id = ui.next_auto_id();
    let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered() && r.enabled());
    let h = ui.ctx().animate_bool_with_time(id.with("hover_t"), hovered, 0.12);
    let fill = base.lerp_to_gamma(hover, h);
    let resp = ui.add(egui::Button::new(label.color(fg).strong()).fill(fill).stroke(Stroke::NONE).corner_radius(radius).min_size(size));
    focus_outline(ui, &resp, radius);
    resp
}

/// Button for an action that deletes something (danger color), used in confirm dialogs.
pub fn destructive(ui: &mut egui::Ui, text: impl Into<String>) -> Response {
    let d = danger();
    filled_with(ui, RichText::new(text.into()), Vec2::new(BUTTON_MIN_W, 32.0), RADIUS, d, d.gamma_multiply(0.85), Color32::WHITE)
}

/// Primary action button (accent color).
pub fn primary(ui: &mut egui::Ui, text: impl Into<String>) -> Response {
    filled(ui, RichText::new(text.into()), Vec2::new(BUTTON_MIN_W, 32.0), RADIUS)
}

/// Narrowest text button, so one-word actions ("OK", "Start") still get a comfortable target.
const BUTTON_MIN_W: f32 = 72.0;

/// Secondary button (translucent background).
pub fn secondary(ui: &mut egui::Ui, label: impl Into<String>) -> Response {
    let resp = ui.add(egui::Button::new(RichText::new(label.into()).color(text())).min_size(Vec2::new(BUTTON_MIN_W, 32.0)));
    focus_outline(ui, &resp, RADIUS);
    resp
}

/// Drop-down arrow for `ComboBox::icon`: the same chevron as the other drop-downs, instead of
/// egui's filled triangle.
pub fn combo_icon(ui: &egui::Ui, rect: egui::Rect, visuals: &egui::style::WidgetVisuals, _open: bool) {
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, crate::icon::CHEVRON_DOWN, egui::FontId::proportional(14.0), visuals.fg_stroke.color);
}

/// Icon-only button (background only on hover).
pub fn icon_button(ui: &mut egui::Ui, icon: &str, tip: &str) -> Response {
    let resp = ui.add(
        egui::Button::new(RichText::new(icon).size(16.0).color(body()))
            .frame_when_inactive(false)
            .min_size(Vec2::splat(32.0)),
    );
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, resp.enabled(), tip));
    focus_outline(ui, &resp, RADIUS);
    resp.on_hover_text(tip)
}

/// One tab in a tab row: plain text, with an accent underline when selected.
pub fn tab(ui: &mut egui::Ui, selected: bool, label: &str) -> Response {
    let color = if selected { text() } else { muted() };
    let galley = ui.painter().layout_no_wrap(label.to_owned(), FontId::proportional(14.0), color);
    let size = Vec2::new(galley.size().x + 20.0, 34.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Tab, resp.enabled(), selected, label));
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let color = if (resp.hovered() || resp.has_focus()) && !selected { body() } else { color };
        p.galley_with_override_text_color(rect.center() - galley.size() / 2.0 - Vec2::new(0.0, 1.0), galley, color);
        if selected {
            let bar = egui::Rect::from_min_max(egui::pos2(rect.left() + 6.0, rect.bottom() - 2.0), egui::pos2(rect.right() - 6.0, rect.bottom()));
            p.rect_filled(bar, 1, accent());
        }
    }
    focus_outline(ui, &resp, RADIUS);
    resp
}

/// On/off button (tinted when on).
pub fn toggle(ui: &mut egui::Ui, on: bool, label: &str, color: Color32) -> Response {
    let b = egui::Button::new(RichText::new(label).color(if on { color } else { text() })).min_size(Vec2::new(0.0, 32.0));
    let b = if on { b.fill(color.gamma_multiply(0.14)).stroke(Stroke::new(1.0, color)) } else { b };
    ui.add(b)
}

/// On/off switch, in place of a check box. Use as `ui.add(theme::switch(&mut on, "text"))`; works
/// with `add_enabled` too.
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
            let keyboard = resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Space) || i.key_pressed(egui::Key::Enter));
            let t = ui.ctx().animate_bool_with_time(resp.id, on, if keyboard { 0.0 } else { 0.125 });
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
        focus_outline(ui, &resp, RADIUS);
        resp
    }
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

/// Icon toggle (selected background when on, background only on hover when off).
pub fn icon_toggle(ui: &mut egui::Ui, on: bool, icon: &str, tip: &str) -> Response {
    let b = egui::Button::new(RichText::new(icon).size(15.0).color(if on { text() } else { muted() })).min_size(Vec2::splat(32.0));
    let b = if on { b.fill(pal().accent_tint).stroke(Stroke::NONE) } else { b.frame_when_inactive(false) };
    let resp = ui.add(b);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, resp.enabled(), on, tip));
    focus_outline(ui, &resp, RADIUS);
    resp.on_hover_text(tip)
}

/// Keyboard focus is separate from hover and selection, and appears without a transition.
pub fn focus_outline(ui: &egui::Ui, resp: &Response, radius: u8) {
    if resp.has_focus() && resp.enabled() {
        ui.painter().rect_stroke(resp.rect.expand(4.0), radius.saturating_add(4), Stroke::new(2.0, accent()), StrokeKind::Inside);
    }
}

// ---- sidebar ----

/// One sidebar row: icon · text · (status dot on the right). A light background when selected, a
/// lighter one on hover.
pub fn nav_item(ui: &mut egui::Ui, selected: bool, icon: &str, label: &str, badge: Option<Color32>) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, resp.enabled(), selected, label));
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
    focus_outline(ui, &resp, RADIUS);
    resp
}

/// Send button: an Enter icon (Lucide corner-down-left) in an accent-colored circle.
pub fn enter_button(ui: &mut egui::Ui, tip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(34.0), egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, resp.enabled(), tip));
    if ui.is_rect_visible(rect) {
        let h = hover_t(ui.ctx(), &resp);
        let p = ui.painter();
        let fill = accent().lerp_to_gamma(pal().accent_hover, h);
        let r = if resp.is_pointer_button_down_on() { 16.0 } else { 17.0 };
        p.circle_filled(rect.center(), r, fill);
        p.text(rect.center(), egui::Align2::CENTER_CENTER, crate::icon::CORNER_DOWN_LEFT, FontId::proportional(17.0), on_accent());
    }
    focus_outline(ui, &resp, RADIUS);
    resp.on_hover_text(tip)
}

/// Frame of the large input box (the task box on the home screen). `focus` goes 0 (idle) to 1
/// (typing) and the border darkens gradually.
pub fn composer_frame(focus: f32) -> egui::Frame {
    egui::Frame::new()
        .fill(card())
        .stroke(Stroke::new(1.0, border().lerp_to_gamma(border_hover(), focus)))
        .corner_radius(MODAL_RADIUS)
        .inner_margin(Margin::symmetric(16, 12))
        .shadow(Shadow { offset: [0, 4], blur: 12, spread: 0, color: Color32::from_black_alpha(pal().popup_shadow / 2) })
}

// ---- small markers ----

/// A round dot (status or agent color).
pub fn dot(ui: &mut egui::Ui, color: Color32, radius: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(radius * 2.0 + 2.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), radius, color);
    resp
}

/// Status: a small dot and the text in the status color, without a pill around it (a status reads
/// as quiet text next to a name, not as a badge). The dot and text are one text run, so it looks
/// the same regardless of the surrounding layout direction.
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
    ui.add(egui::Label::new(job).truncate().selectable(false))
}

/// Small count next to a section label (e.g. "Agents 3" in the sidebar): quiet muted figures,
/// not a pill.
pub fn count_badge(ui: &mut egui::Ui, text: &str) -> Response {
    ui.add(egui::Label::new(RichText::new(text).monospace().size(11.0).color(muted())).selectable(false))
}

/// A key on the keyboard drawn as a small keycap (shortcut hints like Enter, Shift).
pub fn keycap(ui: &mut egui::Ui, key: &str) -> Response {
    let galley = ui.painter().layout_no_wrap(key.to_string(), FontId::proportional(11.0), body());
    let size = Vec2::new((galley.size().x + 12.0).max(20.0), 20.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    let p = ui.painter();
    p.rect(rect, 4, card(), Stroke::new(1.0, border()), StrokeKind::Inside);
    // a slightly heavier bottom edge, like a real key
    p.line_segment([rect.left_bottom() + Vec2::new(3.0, -0.5), rect.right_bottom() + Vec2::new(-3.0, -0.5)], Stroke::new(1.0, border_hover()));
    p.galley(rect.center() - galley.size() / 2.0, galley, body());
    resp
}

/// A low, selectable tile (icon + title centered on one line). Clicked when pressed.
pub fn tile(ui: &mut egui::Ui, selected: bool, icon: &str, title: &str, size: Vec2) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let p = pal();
    let h = hover_t(ui.ctx(), &resp);
    let keyboard = resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Space) || i.key_pressed(egui::Key::Enter));
    let sel = ui.ctx().animate_bool_with_time(resp.id.with("sel_t"), selected, if keyboard { 0.0 } else { 0.15 });
    let fill = p.card.lerp_to_gamma(p.card_hover, h).lerp_to_gamma(p.accent_tint, sel);
    let stroke = Stroke::new(1.0, p.border.lerp_to_gamma(p.border_hover, h).lerp_to_gamma(p.accent, sel));
    let painter = ui.painter();
    painter.rect(rect, CARD_RADIUS, fill, stroke, StrokeKind::Inside);
    // treat icon and title as one unit and center it (so text doesn't get cut in a narrow cell)
    let ig = painter.layout_no_wrap(icon.to_owned(), FontId::proportional(17.0), if selected { p.text } else { p.body });
    let tg = painter.layout_no_wrap(title.to_owned(), FontId::proportional(14.0), p.text);
    let (is, ts, gap) = (ig.size(), tg.size(), 8.0);
    let x = rect.center().x - (is.x + gap + ts.x) / 2.0;
    let cy = rect.center().y;
    painter.galley(egui::pos2(x, cy - is.y / 2.0), ig, p.text);
    painter.galley(egui::pos2(x + is.x + gap, cy - ts.y / 2.0), tg, p.text);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, resp.enabled(), selected, title));
    focus_outline(ui, &resp, CARD_RADIUS);
    resp
}

/// Vertical scroll area inside a modal. While its height doesn't match the content yet (capped at
/// `max_h`; a few frames when first opened or when the content changes), it requests repaints so
/// the window settles to the content size without any input.
pub fn modal_scroll<R>(ui: &mut egui::Ui, max_h: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let out = egui::ScrollArea::vertical()
        .max_height(max_h)
        .min_scrolled_height(max_h)
        .auto_shrink([false, true])
        .show(ui, add);
    // content grew or shrank (e.g. picked another kind) and the height doesn't match yet: draw once
    // more (at most 10 times in a row, so a tiny screen that can never fit doesn't repaint forever)
    let shown = out.inner_rect.height();
    let want = out.content_size.y.min(max_h);
    // (a new content height is a new fit, so the count restarts)
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

// ---- motion ----
//
// All motion is short (0.12–0.25 s) and eases out. Continuous indicators (spinner, shimmering
// text) repaint only 30 times a second to save CPU.

/// Repaint interval for continuous indicators (about 30 fps).
const TICK: std::time::Duration = std::time::Duration::from_millis(33);

/// Easing curve that slows to a stop (0..=1 → 0..=1).
pub fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// An "appear" value rising from 0 to 1. Starts at 0 on first draw, when `key` changes, or when
/// drawn again after being skipped last pass. Used to fade in view changes, new log rows and
/// toasts.
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

/// A value that moves smoothly between 0 and 1 on hover in/out (for fading background and border
/// colors).
pub fn hover_t(ctx: &egui::Context, resp: &Response) -> f32 {
    if resp.has_focus() { 1.0 } else { ctx.animate_bool_with_time(resp.id.with("hover_t"), resp.hovered(), 0.12) }
}

/// Working indicator: eight rays rotating, fading from the tail.
pub fn spinner(ui: &mut egui::Ui, size: f32, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        paint_spinner(ui.painter(), rect.center(), size, color, ui.input(|i| i.time));
        ui.ctx().request_repaint_after(TICK);
    }
    resp
}

/// Draws a `spinner` at a given spot (placeholders while a screen prepares, …). The caller
/// requests repaints.
pub fn paint_spinner(p: &egui::Painter, c: egui::Pos2, size: f32, color: Color32, time: f64) {
    const RAYS: usize = 8;
    // one turn per 0.9 s; rays fade continuously rather than in whole steps so the rotation looks
    // smooth
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

/// Text with a bright band sweeping across it (statuses like "Thinking…" while working).
pub fn shimmer(ui: &mut egui::Ui, text: &str, size: f32) -> Response {
    let (base, hi) = (muted(), pal().text);
    let n = text.chars().count() as f32;
    // the band runs a little past either end before coming back (1.6 s per sweep)
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

/// Thin progress bar (0..=1). Changes animate smoothly.
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

    /// Contrast ratio (WCAG): text needs at least 4.5:1.
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
