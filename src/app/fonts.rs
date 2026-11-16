//! 한글·아이콘 글꼴 등록.

use super::*;

/// 한글 글꼴 후보 (앞에서부터 처음 읽히는 하나를 기본 글꼴 뒤에 대체 글꼴로 붙인다).
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

/// Windows 한글 글꼴 후보: 맑은 고딕(Windows 기본 한글 글꼴) → 굴림·바탕(선택 설치 글꼴) →
/// 사용자가 설치한 나눔고딕·Noto Sans KR (모든 사용자용 `fonts` 폴더, 그다음 사용자별 글꼴 폴더).
/// `local_appdata` 는 사용자별 글꼴 폴더(%LOCALAPPDATA%\Microsoft\Windows\Fonts)의 기준.
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

/// 시스템 글꼴 폴더: 알려진 폴더(FOLDERID_Fonts) → %SystemRoot%\Fonts → C:\Windows\Fonts.
/// (MCP 호스트가 환경변수를 지운 채 GUI 를 띄워도 찾을 수 있게 알려진 폴더 API 를 먼저 쓴다.)
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

/// 일본어·중국어 화면용 글꼴 후보 (한글 글꼴에 없는 가나·간체자를 채운다). 한국어·영어는 없음.
/// 고른 언어일 때만 읽는다: 중국어 글꼴은 20MB 가까이 되어, 늘 들고 있으면 메모리를 많이 쓴다.
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

/// 글꼴을 등록한다. 켤 때와 언어를 바꿀 때 부른다.
pub(super) fn setup_fonts(ctx: &egui::Context, lang: Lang) {
    let mut fonts = egui::FontDefinitions::default();
    // 앞에 둘수록 먼저 쓰인다: (일본어·중국어 글꼴) → 한글 글꼴 → egui 기본 글꼴.
    // 일본어·중국어는 같은 한자도 모양이 달라 그 언어 글꼴을 한글 글꼴보다 앞에 둔다.
    let first = |c: Vec<std::path::PathBuf>| c.into_iter().find_map(|p| std::fs::read(p).ok());
    for (name, bytes) in [("lang", first(lang_font_candidates(lang))), ("korean", first(font_candidates()))].into_iter().rev() {
        let Some(bytes) = bytes else { continue };
        fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_owned(bytes)));
        // 본문 글꼴은 시스템 글꼴(맑은 고딕·Apple SD 산돌고딕 Neo 등)을 맨 앞에 두어 영문·한글 모양을 맞춘다.
        // 고정폭 글꼴은 기존 글꼴을 쓰고 한글·한자만 이 글꼴로 채운다.
        fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, name.into());
        let mono = fonts.families.entry(egui::FontFamily::Monospace).or_default();
        let at = mono.iter().position(|f| f == "korean").unwrap_or(mono.len());
        mono.insert(at, name.into());
    }
    // Lucide 아이콘 글꼴은 사용자 영역(U+E000~) 글자만 있으므로 맨 앞에 두어도 다른 글자 모양을 바꾸지 않는다
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
        // 맑은 고딕이 가장 먼저, 시스템 글꼴 폴더 기준
        assert_eq!(v[0], Path::new(r"D:\Win\Fonts").join("malgun.ttf"));
        let pos = |p: &std::path::Path| v.iter().position(|x| x == p).unwrap_or_else(|| panic!("{p:?} 없음: {v:?}"));
        let gulim = pos(&Path::new(r"D:\Win\Fonts").join("gulim.ttc"));
        let nanum_sys = pos(&Path::new(r"D:\Win\Fonts").join("NanumGothic.ttf"));
        let nanum_user = pos(&Path::new(r"C:\Users\u\AppData\Local").join("Microsoft").join("Windows").join("Fonts").join("NanumGothic.ttf"));
        assert!(0 < gulim && gulim < nanum_sys && nanum_sys < nanum_user, "{v:?}");
        // 사용자별 글꼴 폴더를 모르면 시스템 폴더만
        let only_sys = windows_font_candidates(Path::new(r"C:\Windows\Fonts"), None);
        assert!(only_sys.iter().all(|p| p.starts_with(r"C:\Windows\Fonts")), "{only_sys:?}");
    }

    #[test]
    fn korean_font_is_loaded() {
        // 실제 글꼴 파일을 찾아 egui 에 넣었을 때 한글 글리프가 생기는가 (기본 egui 글꼴에는 한글이 없다)
        let ctx = egui::Context::default();
        setup_fonts(&ctx, Lang::Ko);
        ctx.run_ui(Default::default(), |_ui| {}).drop_without_applying_deltas();
        let ok = ctx.fonts_mut(|f| f.has_glyphs(&egui::FontId::proportional(14.0), "한글 에이전트 설정"));
        if cfg!(any(windows, target_os = "macos")) {
            assert!(ok, "한글 글꼴을 찾지 못함: {:?}", font_candidates());
        }
    }

    /// 일본어·중국어를 고르면 가나·간체자가 그려지고, 한글도 그대로 그려진다
    #[cfg(windows)]
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
        // Windows 10/11 에는 맑은 고딕이 항상 설치되어 있다
        let first = font_candidates().into_iter().find(|p| p.is_file()).expect("한글 글꼴 없음");
        assert!(first.file_name().unwrap().to_string_lossy().eq_ignore_ascii_case("malgun.ttf"), "{first:?}");
    }
}
