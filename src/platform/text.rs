//! OS-dependent words in text shown to users and agents.

/// "This Mac" / "this PC".
pub const THIS_COMPUTER: &str = if cfg!(target_os = "macos") { "이 Mac" } else { "이 PC" };

/// OS name for English prompts.
pub const OS_NAME: &str = if cfg!(target_os = "macos") {
    "macOS"
} else if cfg!(windows) {
    "Windows"
} else {
    "Linux"
};

/// Primary shortcut modifier as told to the agent: `cmd` on macOS, `ctrl` elsewhere.
pub const PRIMARY_MOD: &str = if cfg!(target_os = "macos") { "cmd" } else { "ctrl" };

/// Icon for the local-app card. Drawn from the bundled Lucide font (app-window), so it looks
/// the same on every OS.
pub const APP_ICON: &str = crate::icon::APP_WINDOW;

/// "Mac apps" / "Windows apps".
pub const LOCAL_APPS: &str = if cfg!(target_os = "macos") {
    "Mac 앱"
} else if cfg!(windows) {
    "Windows 앱"
} else {
    "Linux 앱"
};

/// Common message for picking a feature this OS doesn't support.
pub fn unsupported(feature: &str) -> String {
    format!("{feature} 은(는) {OS_NAME} 에서 아직 지원하지 않습니다 (현재 macOS 전용)")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app-card icon must exist in the bundled Lucide font, otherwise it renders as tofu.
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
