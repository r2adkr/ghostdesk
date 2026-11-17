//! The "virtual mouse" drawn over agent screens.
//!
//! Isolated screens (headless browser, hidden app windows, remote sessions) don't draw a real
//! cursor, so the agent's actions are recorded and replayed in GhostDesk as a cursor, trail and
//! click effects. All coordinates are agent coordinates (screenshot pixels).

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32, Pos2, Stroke, Vec2};
use serde_json::Value;

use crate::{agent::coord, llm::short};

/// How long effects stay on screen.
const FX_LIFE: Duration = Duration::from_millis(1600);
/// How long the trail stays on screen.
const TRAIL_LIFE: Duration = Duration::from_millis(2500);
/// Glyph in front of key-press labels: ⌘ on macOS (as before), the Lucide keyboard icon
/// elsewhere.
///
/// It marks "a key was pressed", not the primary modifier, so modifier names like "Ctrl" aren't
/// used (it would read as "Ctrl Return"). To Windows users ⌘ is an unfamiliar Mac-only symbol.
const KEY_GLYPH: &str = if cfg!(target_os = "macos") { "⌘" } else { crate::icon::KEYBOARD };

#[derive(Clone, Debug, PartialEq)]
pub enum FxKind {
    Click,
    DoubleClick,
    RightClick,
    Drag,
    /// Scroll direction (up = -1, down = +1; left/right is horizontal).
    Scroll { dx: f32, dy: f32 },
    Type,
    Key,
}

#[derive(Clone, Debug)]
pub struct CursorFx {
    pub kind: FxKind,
    pub from: (f64, f64),
    pub to: (f64, f64),
    pub label: String,
    pub at: Instant,
    /// Whether this is the user's own takeover input (drawn differently from the agent).
    pub by_user: bool,
}

/// Glide duration for a distance in agent pixels. The backend movement and the on-screen cursor
/// use the same value.
pub fn glide_duration(dist: f64) -> Duration {
    Duration::from_millis((140.0 + dist * 0.3).clamp(140.0, 480.0) as u64)
}

/// Time between steps when real input is sent in pieces (about 60 fps).
pub const FRAME_MS: u64 = 16;
/// How long a button is held down.
pub const PRESS_MS: u64 = 45;
/// Gap between clicks of a double/triple click (well under the OS double-click time).
pub const MULTI_CLICK_GAP_MS: u64 = 70;

/// Number of one-frame steps in `dur`.
pub fn frames(dur: Duration) -> u32 {
    ((dur.as_millis() as u64 / FRAME_MS) as u32).clamp(6, 30)
}

/// Fractions (summing to 1) to split a scroll of `amount` pixels into: large at first, smaller
/// toward the end (ease-out). Small scrolls go in one piece.
pub fn scroll_parts(amount: f64) -> Vec<f64> {
    if amount <= 60.0 {
        return vec![1.0];
    }
    let n = ((amount / 40.0) as usize).clamp(4, 16);
    let out = |t: f64| 1.0 - (1.0 - t).powi(3);
    (1..=n).map(|i| out(i as f64 / n as f64) - out((i - 1) as f64 / n as f64)).collect()
}

/// ease-in-out (cubic)
pub fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

/// A glide in progress.
#[derive(Clone, Debug)]
pub struct Glide {
    pub from: (f64, f64),
    pub to: (f64, f64),
    pub start: Instant,
    pub dur: Duration,
}

#[derive(Default)]
pub struct CursorState {
    pub pos: Option<(f64, f64)>,
    pub glide: Option<Glide>,
    pub by_user: bool,
    pub fx: VecDeque<CursorFx>,
    pub trail: VecDeque<((f64, f64), Instant)>,
    pub last_change: Option<Instant>,
}

impl CursorState {
    fn push_fx(&mut self, fx: CursorFx) {
        self.fx.push_back(fx);
        while self.fx.len() > 12 {
            self.fx.pop_front();
        }
    }

    fn move_to(&mut self, p: (f64, f64), now: Instant) {
        if self.pos != Some(p) {
            if let Some(prev) = self.pos {
                if self.trail.is_empty() {
                    self.trail.push_back((prev, now));
                }
            }
            self.trail.push_back((p, now));
            while self.trail.len() > 40 {
                self.trail.pop_front();
            }
            self.pos = Some(p);
        }
    }

    /// Announces the target right before an action runs, so the on-screen cursor glides along the
    /// same curve.
    pub fn aim(&mut self, to: (f64, f64), fallback_from: (f64, f64)) {
        let from = self.current(Instant::now()).unwrap_or(self.pos.unwrap_or(fallback_from));
        let dist = ((to.0 - from.0).powi(2) + (to.1 - from.1).powi(2)).sqrt();
        let now = Instant::now();
        self.glide = Some(Glide { from, to, start: now, dur: glide_duration(dist) });
        self.last_change = Some(now);
    }

    /// Commits the target as the current position (trail included) once the glide is over. Returns
    /// true while still gliding.
    ///
    /// Keeps the cursor from snapping back to the start and returning when the backend action
    /// finishes later than the UI glide and `record()` hasn't been called yet (`record()` with the
    /// same coordinates is a no-op).
    fn settle_glide(&mut self, now: Instant) -> bool {
        let gliding = self.glide.as_ref().map(|g| now.duration_since(g.start) < g.dur).unwrap_or(false);
        if !gliding {
            if let Some(g) = self.glide.take() {
                self.move_to(g.to, now);
            }
        }
        gliding
    }

    /// Current position while gliding.
    fn current(&self, now: Instant) -> Option<(f64, f64)> {
        let g = self.glide.as_ref()?;
        let t = ease(now.duration_since(g.start).as_secs_f32() / g.dur.as_secs_f32().max(0.001)) as f64;
        Some((g.from.0 + (g.to.0 - g.from.0) * t, g.from.1 + (g.to.1 - g.from.1) * t))
    }

    /// Records one computer-use action. `pos_after` is the mouse position the backend reported after
    /// the action.
    pub fn record(&mut self, name: &str, input: &Value, pos_after: (f64, f64), by_user: bool) {
        let now = Instant::now();
        let target = coord(&input["coordinate"]).unwrap_or(pos_after);
        let before = self.pos.unwrap_or(target);
        let fx = |kind: FxKind, from: (f64, f64), to: (f64, f64), label: String| CursorFx {
            kind,
            from,
            to,
            label,
            at: now,
            by_user,
        };
        let new_fx = match name {
            "left_click" | "middle_click" | "left_mouse_down" => Some(fx(FxKind::Click, target, target, String::new())),
            "double_click" | "triple_click" => Some(fx(FxKind::DoubleClick, target, target, String::new())),
            "right_click" => Some(fx(FxKind::RightClick, target, target, String::new())),
            "left_click_drag" => {
                let from = coord(&input["start_coordinate"]).unwrap_or(before);
                Some(fx(FxKind::Drag, from, target, String::new()))
            }
            "scroll" => {
                let (dx, dy) = match input["scroll_direction"].as_str().unwrap_or("down") {
                    "up" => (0.0, -1.0),
                    "left" => (-1.0, 0.0),
                    "right" => (1.0, 0.0),
                    _ => (0.0, 1.0),
                };
                Some(fx(FxKind::Scroll { dx, dy }, target, target, String::new()))
            }
            "type" => Some(fx(
                FxKind::Type,
                before,
                before,
                format!("{} {}", crate::icon::KEYBOARD, short(input["text"].as_str().unwrap_or(""), 24).replace('\n', "⏎")),
            )),
            "key" | "hold_key" => Some(fx(FxKind::Key, before, before, format!("{KEY_GLYPH} {}", input["text"].as_str().unwrap_or("")))),
            _ => None,
        };
        let moves = !matches!(name, "type" | "key" | "hold_key" | "screenshot" | "zoom" | "wait" | "cursor_position");
        if moves {
            if name == "left_click_drag" {
                if let Some(f) = &new_fx {
                    self.move_to(f.from, now);
                }
            }
            self.move_to(if coord(&input["coordinate"]).is_some() { target } else { pos_after }, now);
        }
        if let Some(f) = new_fx {
            self.push_fx(f);
        }
        self.by_user = by_user;
        self.last_change = Some(now);
    }

    /// Whether an animation is running (needs a repaint).
    pub fn animating(&self, now: Instant) -> bool {
        self.last_change.map(|t| now.duration_since(t) < TRAIL_LIFE).unwrap_or(false)
    }

    fn prune(&mut self, now: Instant) {
        while self.fx.front().map(|f| now.duration_since(f.at) > FX_LIFE).unwrap_or(false) {
            self.fx.pop_front();
        }
        while self.trail.front().map(|t| now.duration_since(t.1) > TRAIL_LIFE).unwrap_or(false) {
            self.trail.pop_front();
        }
    }
}

/// A distinct color per agent id.
pub fn agent_color(id: u64) -> Color32 {
    const P: [Color32; 8] = [
        Color32::from_rgb(255, 92, 92),
        Color32::from_rgb(64, 160, 255),
        Color32::from_rgb(80, 210, 120),
        Color32::from_rgb(255, 170, 40),
        Color32::from_rgb(190, 110, 255),
        Color32::from_rgb(40, 210, 210),
        Color32::from_rgb(255, 110, 200),
        Color32::from_rgb(200, 200, 70),
    ];
    P[((id.max(1) - 1) % P.len() as u64) as usize]
}

fn with_alpha(c: Color32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a.clamp(0.0, 1.0) * 255.0) as u8)
}

/// Arrow cursor with its tip at `tip`. `size` is the height in screen pixels.
fn draw_arrow(p: &egui::Painter, tip: Pos2, size: f32, fill: Color32) {
    let s = size / 18.0;
    let pt = |x: f32, y: f32| tip + Vec2::new(x * s, y * s);
    // drawn as convex polygons: a head (triangle) and a tail (quad)
    let head = vec![pt(0.0, 0.0), pt(0.0, 15.0), pt(11.5, 11.5)];
    let tail = vec![pt(4.2, 11.8), pt(7.2, 11.0), pt(10.2, 17.5), pt(7.4, 18.6)];
    let outline = Stroke::new((1.6 * s).max(1.0), Color32::from_black_alpha(230));
    for poly in [&head, &tail] {
        p.add(egui::Shape::convex_polygon(poly.clone(), Color32::from_black_alpha(230), outline));
    }
    let inset = |poly: &Vec<Pos2>| {
        let c = poly.iter().fold(Vec2::ZERO, |a, q| a + q.to_vec2()) / poly.len() as f32;
        poly.iter().map(|q| (c + (q.to_vec2() - c) * 0.78).to_pos2()).collect::<Vec<_>>()
    };
    p.add(egui::Shape::convex_polygon(inset(&head), fill, Stroke::NONE));
    p.add(egui::Shape::convex_polygon(inset(&tail), fill, Stroke::NONE));
}

/// The user's takeover input is shown as a ring instead of a pointer.
fn draw_user_pointer(p: &egui::Painter, at: Pos2, size: f32) {
    let r = size * 0.38;
    p.circle_filled(at, r, Color32::from_rgba_unmultiplied(255, 180, 60, 70));
    p.circle_stroke(at, r, Stroke::new(2.0, Color32::from_rgb(255, 180, 60)));
    p.circle_filled(at, 2.0, Color32::from_rgb(255, 180, 60));
}

/// Display state, kept on the UI side for smooth movement.
#[derive(Default)]
pub struct CursorView {
    disp: Option<Pos2>,
    last_frame: Option<Instant>,
}

/// Draws the cursor and effects over the agent screen `rect`. `scale` is screen pixels per agent
/// pixel. Returns true if a repaint is needed.
pub fn paint(
    painter: &egui::Painter,
    rect: egui::Rect,
    scale: f32,
    state: &mut CursorState,
    view: &mut CursorView,
    fallback_pos: (f64, f64),
    color: Color32,
    label: Option<&str>,
    compact: bool,
) -> bool {
    let now = Instant::now();
    state.prune(now);
    let painter = painter.with_clip_rect(rect);
    let to_screen = |p: (f64, f64)| rect.min + Vec2::new(p.0 as f32 * scale, p.1 as f32 * scale);
    // position on the curve while gliding, the recorded position once done
    let gliding = state.settle_glide(now);
    let logical = if gliding { state.current(now) } else { None };
    let target = to_screen(logical.or(state.pos).unwrap_or(fallback_pos));

    // only ease out the remaining error (40 ms exponential decay); big moves follow the glide curve
    let dt = view.last_frame.map(|t| now.duration_since(t).as_secs_f32()).unwrap_or(1.0).min(0.1);
    view.last_frame = Some(now);
    let disp = match view.disp {
        Some(d) if gliding => {
            let _ = d;
            target
        }
        Some(d) if d.distance(target) < 2000.0 => {
            let k = 1.0 - (-dt / 0.04).exp();
            d + (target - d) * k
        }
        _ => target,
    };
    view.disp = Some(disp);
    let moving = gliding || disp.distance(target) > 0.5;

    // trail
    if state.trail.len() >= 2 {
        let pts: Vec<(Pos2, f32)> = state
            .trail
            .iter()
            .map(|(p, t)| (to_screen(*p), 1.0 - now.duration_since(*t).as_secs_f32() / TRAIL_LIFE.as_secs_f32()))
            .collect();
        for w in pts.windows(2) {
            let a = w[0].1.min(w[1].1);
            painter.line_segment([w[0].0, w[1].0], Stroke::new(2.0, with_alpha(color, a * 0.55)));
        }
    }

    // action effects
    let base = if compact { 9.0 } else { 14.0 };
    for f in state.fx.iter() {
        let age = now.duration_since(f.at).as_secs_f32();
        let t = (age / FX_LIFE.as_secs_f32()).clamp(0.0, 1.0);
        let fade = 1.0 - t;
        let c = if f.by_user { Color32::from_rgb(255, 180, 60) } else { color };
        let at = to_screen(f.to);
        match &f.kind {
            FxKind::Click | FxKind::DoubleClick | FxKind::RightClick => {
                let rings = if f.kind == FxKind::DoubleClick { 2 } else { 1 };
                for i in 0..rings {
                    let tt = (t * 1.6 - i as f32 * 0.25).clamp(0.0, 1.0);
                    let r = base * 0.4 + base * 1.8 * tt;
                    painter.circle_stroke(at, r, Stroke::new(2.5, with_alpha(c, (1.0 - tt) * 0.9)));
                }
                painter.circle_filled(at, base * 0.3, with_alpha(c, fade * 0.8));
                if f.kind == FxKind::RightClick && !compact {
                    painter.text(at + Vec2::new(base, -base), egui::Align2::LEFT_BOTTOM, "우클릭", egui::FontId::proportional(11.0), with_alpha(c, fade));
                }
            }
            FxKind::Drag => {
                let from = to_screen(f.from);
                painter.arrow(from, at - from, Stroke::new(2.5, with_alpha(c, fade)));
                painter.circle_stroke(from, base * 0.5, Stroke::new(2.0, with_alpha(c, fade)));
            }
            FxKind::Scroll { dx, dy } => {
                let dir = Vec2::new(*dx, *dy).normalized() * base * 1.6;
                let off = dir * (t * 0.6);
                for i in 0..2 {
                    let p0 = at + off + dir * (i as f32 * 0.45);
                    painter.arrow(p0 - dir * 0.3, dir * 0.6, Stroke::new(2.0, with_alpha(c, fade * (1.0 - i as f32 * 0.35))));
                }
            }
            FxKind::Type | FxKind::Key => {
                if compact && f.kind == FxKind::Key {
                    continue;
                }
                let font = egui::FontId::proportional(if compact { 10.0 } else { 12.5 });
                let anchor = disp + Vec2::new(base * 1.1, base * 1.3 + 4.0 * t);
                let galley = painter.layout_no_wrap(f.label.clone(), font, Color32::WHITE);
                let bg = egui::Rect::from_min_size(anchor, galley.size()).expand(4.0);
                painter.rect_filled(bg, 5.0, with_alpha(Color32::from_rgb(20, 20, 24), fade * 0.85));
                painter.rect_stroke(bg, 5.0, Stroke::new(1.0, with_alpha(c, fade)), egui::StrokeKind::Outside);
                painter.galley(anchor, galley, with_alpha(Color32::WHITE, fade));
            }
        }
    }

    // the cursor itself
    let size = if compact { 13.0 } else { 20.0 };
    if state.by_user {
        draw_user_pointer(&painter, disp, size);
    } else {
        draw_arrow(&painter, disp, size, color);
    }
    if let Some(l) = label.filter(|_| !compact) {
        let font = egui::FontId::proportional(11.0);
        let galley = painter.layout_no_wrap(l.to_string(), font, Color32::WHITE);
        let at = disp + Vec2::new(size * 0.7, size * 0.9);
        let bg = egui::Rect::from_min_size(at, galley.size()).expand(3.0);
        painter.rect_filled(bg, 4.0, with_alpha(color, 0.9));
        painter.galley(at, galley, Color32::WHITE);
    }

    moving || state.animating(now) || !state.fx.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scroll_is_split_and_slows_down() {
        assert_eq!(scroll_parts(30.0), vec![1.0]);
        let p = scroll_parts(300.0);
        assert!(p.len() >= 4);
        assert!((p.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        assert!(p.windows(2).all(|w| w[0] >= w[1]), "{p:?}");
        assert_eq!(frames(glide_duration(0.0)), 8);
        assert_eq!(frames(Duration::from_secs(5)), 30);
    }

    #[test]
    fn records_actions() {
        let mut s = CursorState::default();
        s.record("left_click", &json!({"coordinate": [10, 20]}), (10.0, 20.0), false);
        assert_eq!(s.pos, Some((10.0, 20.0)));
        assert_eq!(s.fx.back().unwrap().kind, FxKind::Click);
        s.record("type", &json!({"text": "안녕하세요"}), (10.0, 20.0), false);
        assert_eq!(s.pos, Some((10.0, 20.0)), "타이핑은 커서를 움직이지 않음");
        assert!(s.fx.back().unwrap().label.contains("안녕"));
        s.record("left_click_drag", &json!({"start_coordinate": [1, 2], "coordinate": [30, 40]}), (30.0, 40.0), false);
        assert_eq!(s.pos, Some((30.0, 40.0)));
        assert_eq!(s.fx.back().unwrap().from, (1.0, 2.0));
        s.record("scroll", &json!({"coordinate": [5, 5], "scroll_direction": "up"}), (5.0, 5.0), true);
        assert!(matches!(s.fx.back().unwrap().kind, FxKind::Scroll { dy, .. } if dy < 0.0));
        assert!(s.by_user);
        assert!(s.trail.len() >= 3);
        s.record("key", &json!({"text": "ctrl+c"}), (5.0, 5.0), false);
        let label = &s.fx.back().unwrap().label;
        assert_eq!(label, &format!("{KEY_GLYPH} ctrl+c"));
        if cfg!(target_os = "macos") {
            assert_eq!(label, "⌘ ctrl+c", "macOS 표시는 그대로");
        } else {
            assert!(!label.contains('⌘'), "Mac 전용 기호를 쓰지 않음: {label}");
        }
    }

    #[test]
    fn glide_end_commits_target_before_record() {
        let mut s = CursorState::default();
        s.record("left_click", &json!({"coordinate": [0, 0]}), (0.0, 0.0), false);
        s.aim((400.0, 0.0), (0.0, 0.0));
        let g = s.glide.clone().unwrap();
        // position doesn't change mid-glide
        assert!(s.settle_glide(g.start));
        assert_eq!(s.pos, Some((0.0, 0.0)));
        // once done it commits to the target even before record() (no rubber-banding back)
        assert!(!s.settle_glide(g.start + g.dur + Duration::from_millis(500)));
        assert_eq!(s.pos, Some((400.0, 0.0)));
        assert!(s.glide.is_none());
        let trail = s.trail.len();
        s.record("left_click", &json!({"coordinate": [400, 0]}), (400.0, 0.0), false);
        assert_eq!(s.pos, Some((400.0, 0.0)));
        assert_eq!(s.trail.len(), trail, "같은 좌표의 record 는 궤적을 늘리지 않음");
    }
}
