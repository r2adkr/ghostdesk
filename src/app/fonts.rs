//! Korean and icon font registration.

use super::*;

/// Korean font candidates. The first one that loads is appended to the default fonts as a
/// fallback.
pub(super) fn font_candidates() -> Vec<std::path::PathBuf> {
    #[cfg(windows)]
    {
        windows_font_candidates(&windows_fonts_dir(), dirs::data_local_dir().as_deref())
    }
    #[cfg(not(windows))]
    {
        [
            "/System/Library/Fonts/AppleSDGothicNeo.ttc",
            "/System/Library/Fonts/Supplemental/AppleGothic.ttf",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/nanum/NanumGothic.ttf",
        ]
        .iter()
        .map(std::path::PathBuf::from)
        .collect()
    }
}

/// Windows Korean font candidates: Malgun Gothic (the default Korean font) → Gulim/Batang
/// (optional fonts) → user-installed NanumGothic / Noto Sans KR (all-users `fonts` folder, then
/// the per-user font folder).
///
/// `local_appdata` is the base of the per-user font folder
/// (%LOCALAPPDATA%\Microsoft\Windows\Fonts).
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn windows_font_candidates(fonts: &std::path::Path, local_appdata: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    const SYSTEM: &[&str] = &["malgun.ttf", "malgunsl.ttf", "gulim.ttc", "batang.ttc"];
    const ADDED: &[&str] = &["NanumGothic.ttf", "NotoSansKR-Regular.otf", "NotoSansKR-Regular.ttf", "NotoSansCJKkr-Regular.otf"];
    let user = local_appdata.map(|d| d.join("Microsoft").join("Windows").join("Fonts"));
    let mut v: Vec<std::path::PathBuf> = SYSTEM.iter().map(|f| fonts.join(f)).collect();
    for f in ADDED {
        v.push(fonts.join(f));
        if let Some(u) = &user {
            v.push(u.join(f));
        }
    }
    v
}

/// System font folder: known folder (FOLDERID_Fonts) → %SystemRoot%\Fonts → C:\Windows\Fonts.
///
/// The known-folder API comes first so fonts are found even when an MCP host starts the GUI with
/// a scrubbed environment.
#[cfg(windows)]
pub(super) fn windows_fonts_dir() -> std::path::PathBuf {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Fonts, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    let known = unsafe {
        let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(&FOLDERID_Fonts, KF_FLAG_DEFAULT as u32, std::ptr::null_mut(), &mut p);
        let out = (hr >= 0 && !p.is_null()).then(|| std::path::PathBuf::from(crate::platform::win::from_wide_ptr(p)));
        if !p.is_null() {
            CoTaskMemFree(p as *const core::ffi::c_void);
        }
        out
    };
    known
        .filter(|d| !d.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("SystemRoot")
                .or_else(|| std::env::var_os("windir"))
                .map(|w| std::path::PathBuf::from(w).join("Fonts"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows\Fonts"))
}

/// Font candidates for Japanese and Chinese UIs, filling in kana and simplified characters the
/// Korean font lacks. None for Korean and English.
///
/// Only loaded for the chosen language: Chinese fonts are close to 20 MB, too much to keep
/// around.
pub(super) fn lang_font_candidates(lang: Lang) -> Vec<std::path::PathBuf> {
    #[cfg(windows)]
    let (dir, ja, zh): (std::path::PathBuf, &[&str], &[&str]) =
        (windows_fonts_dir(), &["YuGothM.ttc", "meiryo.ttc", "msgothic.ttc"], &["msyh.ttc", "simsun.ttc", "simhei.ttf"]);
    #[cfg(not(windows))]
    let (dir, ja, zh): (std::path::PathBuf, &[&str], &[&str]) = (
        "/".into(),
        &["System/Library/Fonts/ヒラギノ角ゴシック W3.ttc", "usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"],
        &["System/Library/Fonts/PingFang.ttc", "System/Library/Fonts/STHeiti Light.ttc", "usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"],
    );
    let names: &[&str] = match lang {
        Lang::Ja => ja,
        Lang::Zh => zh,
        Lang::Ko | Lang::En => &[],
    };
    names.iter().map(|n| dir.join(n)).collect()
}

/// Which face of a font collection to use. Windows' Japanese collections hold a document face first
/// and a UI face after it; the document faces (Yu Gothic Medium: a 0.5 em line gap and a short ascent)
/// sit about a third of the font size higher than Hangul, Latin and the icons, so Japanese text looked
/// raised in every row. The UI faces have the same line metrics as Malgun Gothic.
fn face_index(path: &std::path::Path) -> u32 {
    match path.file_name().and_then(|n| n.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("yugothm.ttc") => 1,  // Yu Gothic UI
        Some("meiryo.ttc") => 2,   // Meiryo UI
        Some("msgothic.ttc") => 1, // MS UI Gothic
        _ => 0,
    }
}

/// Memory-maps a font file.
///
/// Reading it whole would put the macOS Korean font (a multi-weight .ttc, about 55 MB) straight
/// on the heap; a map only pages in glyphs actually drawn, and the OS can reclaim those at any
/// time. Maps stay open for the life of the process and are reused (switching languages and
/// re-registering fonts doesn't reopen them).
fn map_font(path: &std::path::Path) -> Option<&'static [u8]> {
    static OPEN: parking_lot::Mutex<Vec<(std::path::PathBuf, &'static [u8])>> = parking_lot::Mutex::new(Vec::new());
    let mut open = OPEN.lock();
    if let Some((_, bytes)) = open.iter().find(|(p, _)| p == path) {
        return Some(bytes);
    }
    let file = std::fs::File::open(path).ok()?;
    // SAFETY: files in the system font folder aren't truncated or modified while we run (the macOS
    // system volume is read-only; Windows won't delete or shrink a mapped file). The map is leaked
    // on purpose to get a 'static slice.
    let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    let bytes: &'static [u8] = Box::leak(Box::new(map));
    open.push((path.to_path_buf(), bytes));
    Some(bytes)
}

/// Registers fonts. Called at startup and on language change.
pub(super) fn setup_fonts(ctx: &egui::Context, lang: Lang) {
    let mut fonts = egui::FontDefinitions::default();
    // earlier entries win: (Japanese/Chinese font) → Korean font → egui defaults. The same Han
    // characters look different in Japanese and Chinese, so that language's font goes first.
    let first = |c: Vec<std::path::PathBuf>| c.into_iter().find_map(|p| map_font(&p).map(|b| (b, face_index(&p))));
    for (name, font) in [("lang", first(lang_font_candidates(lang))), ("korean", first(font_candidates()))].into_iter().rev() {
        let Some((bytes, index)) = font else { continue };
        let mut data = egui::FontData::from_static(bytes);
        data.index = index;
        fonts.font_data.insert(name.into(), Arc::new(data));
        // proportional text puts the system font (Malgun Gothic, Apple SD Gothic Neo, …) first so Latin
        // and Hangul match; monospace keeps its font and only falls back for Hangul and Han.
        fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, name.into());
        let mono = fonts.families.entry(egui::FontFamily::Monospace).or_default();
        let at = mono.iter().position(|f| f == "korean").unwrap_or(mono.len());
        mono.insert(at, name.into());
    }
    // Lucide only has Private Use Area glyphs (U+E000..), so putting it first changes nothing else
    fonts.font_data.insert("lucide".into(), Arc::new(egui::FontData::from_static(crate::icon::FONT)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().insert(0, "lucide".into());
    }
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_font_candidate_order() {
        use std::path::Path;
        let v = windows_font_candidates(Path::new(r"D:\Win\Fonts"), Some(Path::new(r"C:\Users\u\AppData\Local")));
        // Malgun Gothic first, from the system font folder
        assert_eq!(v[0], Path::new(r"D:\Win\Fonts").join("malgun.ttf"));
        let pos = |p: &std::path::Path| v.iter().position(|x| x == p).unwrap_or_else(|| panic!("{p:?} 없음: {v:?}"));
        let gulim = pos(&Path::new(r"D:\Win\Fonts").join("gulim.ttc"));
        let nanum_sys = pos(&Path::new(r"D:\Win\Fonts").join("NanumGothic.ttf"));
        let nanum_user = pos(&Path::new(r"C:\Users\u\AppData\Local").join("Microsoft").join("Windows").join("Fonts").join("NanumGothic.ttf"));
        assert!(0 < gulim && gulim < nanum_sys && nanum_sys < nanum_user, "{v:?}");
        // without a per-user font folder, only the system one
        let only_sys = windows_font_candidates(Path::new(r"C:\Windows\Fonts"), None);
        assert!(only_sys.iter().all(|p| p.starts_with(r"C:\Windows\Fonts")), "{only_sys:?}");
    }

    #[test]
    fn korean_font_is_loaded() {
        // loading a real font into egui yields Hangul glyphs (egui's default fonts have none)
        let ctx = egui::Context::default();
        setup_fonts(&ctx, Lang::Ko);
        ctx.run_ui(Default::default(), |_ui| {}).drop_without_applying_deltas();
        let ok = ctx.fonts_mut(|f| f.has_glyphs(&egui::FontId::proportional(14.0), "한글 에이전트 설정"));
        if cfg!(any(windows, target_os = "macos")) {
            assert!(ok, "한글 글꼴을 찾지 못함: {:?}", font_candidates());
        }
    }

    /// Choosing Japanese or Chinese renders kana and simplified characters, and Hangul still renders
    /// (fonts are memory-mapped).
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn cjk_fonts_cover_their_language() {
        for (lang, sample) in [(Lang::Ja, "設定を開く ひらがな カタカナ"), (Lang::Zh, "设置 选择模型 语言")] {
            let ctx = egui::Context::default();
            setup_fonts(&ctx, lang);
            ctx.run_ui(Default::default(), |_ui| {}).drop_without_applying_deltas();
            let id = egui::FontId::proportional(14.0);
            assert!(ctx.fonts_mut(|f| f.has_glyphs(&id, sample)), "{lang:?}: {:?}", lang_font_candidates(lang));
            assert!(ctx.fonts_mut(|f| f.has_glyphs(&id, "한글")), "{lang:?} 에서도 한글이 보여야 함");
        }
        assert!(lang_font_candidates(Lang::En).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_finds_malgun_gothic() {
        let dir = windows_fonts_dir();
        assert!(dir.is_dir(), "{dir:?}");
        assert!(dir.ends_with("Fonts"), "{dir:?}");
        // Windows 10/11 always ship Malgun Gothic
        let first = font_candidates().into_iter().find(|p| p.is_file()).expect("한글 글꼴 없음");
        assert!(first.file_name().unwrap().to_string_lossy().eq_ignore_ascii_case("malgun.ttf"), "{first:?}");
    }
}
