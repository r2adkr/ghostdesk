//! 사용자·에이전트에게 보여 주는 문구 중 운영체제에 따라 달라지는 낱말.

/// "이 Mac" / "이 PC"
pub const THIS_COMPUTER: &str = if cfg!(target_os = "macos") { "이 Mac" } else { "이 PC" };

/// 영문 프롬프트용 운영체제 이름
pub const OS_NAME: &str = if cfg!(target_os = "macos") {
    "macOS"
} else if cfg!(windows) {
    "Windows"
} else {
    "Linux"
};

/// 기본 단축키 수정키 이름 (에이전트에게 알려 줄 때): macOS 는 cmd, 그 밖은 ctrl
pub const PRIMARY_MOD: &str = if cfg!(target_os = "macos") { "cmd" } else { "ctrl" };

/// 로컬 앱 카드 아이콘 (Lucide app-window, 함께 넣은 아이콘 글꼴로 그려 OS 와 상관없이 같은 모양)
pub const APP_ICON: &str = crate::icon::APP_WINDOW;

/// "Mac 앱" / "Windows 앱"
pub const LOCAL_APPS: &str = if cfg!(target_os = "macos") {
    "Mac 앱"
} else if cfg!(windows) {
    "Windows 앱"
} else {
    "Linux 앱"
};

/// 지원하지 않는 기능을 고른 경우의 공통 안내
pub fn unsupported(feature: &str) -> String {
    format!("{feature} 은(는) {OS_NAME} 에서 아직 지원하지 않습니다 (현재 macOS 전용)")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 앱 카드 아이콘은 함께 넣은 Lucide 글꼴에 있어야 한다 (없으면 빈 네모로 그려진다)
    #[test]
    fn app_icon_is_in_bundled_font() {
        use eframe::egui::epaint::text::{FontData, FontDefinitions, FontFamily, FontId, Fonts, TextOptions};
        let mut defs = FontDefinitions::empty();
        defs.font_data.insert("lucide".into(), std::sync::Arc::new(FontData::from_static(crate::icon::FONT)));
        defs.families.insert(FontFamily::Proportional, vec!["lucide".into()]);
        let mut fonts = Fonts::new(TextOptions::default(), defs);
        assert!(fonts.has_glyphs(&FontId::proportional(14.0), APP_ICON));
    }
}
