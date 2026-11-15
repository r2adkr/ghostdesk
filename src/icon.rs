//! Lucide 아이콘 (https://lucide.dev, ISC 라이선스 — assets/lucide/LICENSE).
//!
//! lucide-static 의 아이콘 글꼴을 본문 글꼴 맨 앞에 넣어 두었으므로(app::setup_fonts),
//! 아래 글자를 아무 글(RichText·버튼·painter.text)에 섞어 쓰면 그 자리에 아이콘이 그려진다.
//! 새 아이콘은 lucide-static 의 font/codepoints.json 에서 이름으로 번호를 찾아 추가한다.

/// 글꼴 파일 (lucide-static 1.73.0 의 lucide.ttf 에서 아이콘 글자(U+E000~F8FF)만 남긴 것).
/// 원본은 영문 소문자·숫자 글자도 들어 있어, 맨 앞에 두면 보통 글자가 깨진다 ("GhostDesk" → "GD").
/// 다시 만들기: pyftsubset lucide.ttf --unicodes="U+E000-F8FF" --layout-features='' --output-file=lucide.ttf
pub const FONT: &[u8] = include_bytes!("../assets/lucide/lucide.ttf");

pub const APP_WINDOW: &str = "\u{e426}";
pub const ARROW_LEFT: &str = "\u{e048}";
pub const ARROW_RIGHT: &str = "\u{e049}";
pub const BOOK_OPEN: &str = "\u{e05f}";
pub const CIRCLE_CHECK: &str = "\u{e226}";
pub const CIRCLE_HELP: &str = "\u{e082}";
pub const COPY: &str = "\u{e09e}";
pub const CORNER_DOWN_LEFT: &str = "\u{e0a1}";
pub const ELLIPSIS: &str = "\u{e0b6}";
pub const FLASK: &str = "\u{e0d5}";
pub const GHOST: &str = "\u{e20e}";
pub const GLOBE: &str = "\u{e0e8}";
pub const KEYBOARD: &str = "\u{e284}";
pub const LAYOUT_DASHBOARD: &str = "\u{e1c1}";
pub const LAYOUT_GRID: &str = "\u{e0ff}";
pub const LIST_TODO: &str = "\u{e4c3}";
pub const LOCK_OPEN: &str = "\u{e10c}";
pub const MONITOR: &str = "\u{e11d}";
pub const MOUSE_POINTER: &str = "\u{e1c3}";
pub const PLUG: &str = "\u{e37f}";
pub const PLUS: &str = "\u{e13d}";
pub const ROTATE_CW: &str = "\u{e149}";
pub const SETTINGS: &str = "\u{e154}";
pub const SHIELD_ALERT: &str = "\u{e1fe}";
pub const STAR: &str = "\u{e176}";
pub const TRIANGLE_ALERT: &str = "\u{e193}";
pub const USER: &str = "\u{e19f}";
pub const X: &str = "\u{e1b2}";
