//! Lucide icons (https://lucide.dev, ISC license — see assets/lucide/LICENSE).
//!
//! The lucide-static icon font is placed first in the proportional font family
//! (`app::setup_fonts`), so these characters can be mixed into any text (`RichText`,
//! buttons, `painter.text`) and render as icons. To add an icon, look up its codepoint by
//! name in lucide-static's `font/codepoints.json`.

/// The icon font: `lucide.ttf` from lucide-static 1.73.0, subset to the Private Use Area
/// (U+E000..F8FF). The original also maps ASCII letters and digits, which would hijack regular
/// text when the font comes first ("GhostDesk" renders as "GD").
///
/// To regenerate: `pyftsubset lucide.ttf --unicodes="U+E000-F8FF" --layout-features='' --output-file=lucide.ttf`
pub const FONT: &[u8] = include_bytes!("../assets/lucide/lucide.ttf");

pub const APP_WINDOW: &str = "\u{e426}";
pub const ARROW_LEFT: &str = "\u{e048}";
pub const ARROW_RIGHT: &str = "\u{e049}";
pub const BOOK_OPEN: &str = "\u{e05f}";
pub const CHEVRON_DOWN: &str = "\u{e06d}";
pub const CIRCLE_CHECK: &str = "\u{e226}";
pub const CIRCLE_HELP: &str = "\u{e082}";
pub const CHART_COLUMN: &str = "\u{e2a3}";
pub const CLOCK: &str = "\u{e087}";
pub const COINS: &str = "\u{e097}";
pub const COPY: &str = "\u{e09e}";
pub const ELLIPSIS: &str = "\u{e0b6}";
pub const FLASK: &str = "\u{e0d5}";
pub const PANEL_LEFT: &str = "\u{e12a}";
pub const SEARCH: &str = "\u{e151}";
pub const ZAP: &str = "\u{e1b4}";
pub const GHOST: &str = "\u{e20e}";
pub const GLOBE: &str = "\u{e0e8}";
pub const KEYBOARD: &str = "\u{e284}";
pub const LAYOUT_DASHBOARD: &str = "\u{e1c1}";
pub const LAYOUT_GRID: &str = "\u{e0ff}";
pub const LIST_TODO: &str = "\u{e4c3}";
pub const LOCK_OPEN: &str = "\u{e10c}";
pub const MONITOR: &str = "\u{e11d}";
pub const MOUSE_POINTER: &str = "\u{e1c3}";
pub const PICTURE_IN_PICTURE: &str = "\u{e3af}";
pub const PLUG: &str = "\u{e37f}";
pub const PLUS: &str = "\u{e13d}";
pub const ROTATE_CW: &str = "\u{e149}";
pub const SETTINGS: &str = "\u{e154}";
pub const SHIELD_ALERT: &str = "\u{e1fe}";
pub const STAR: &str = "\u{e176}";
pub const USER: &str = "\u{e19f}";
pub const X: &str = "\u{e1b2}";
