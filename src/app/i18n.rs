//! UI language: Korean (source), English, Japanese, Chinese.
//!
//! UI text is written in the code in Korean and wrapped in `tr("…")`. Other languages are
//! looked up in the table in `table.rs`; anything missing shows the source text (the
//! `every_ui_string_is_translated` test catches omissions). Text with values in it uses
//! `trf("… {} …", &[&value])`, and each language fills the `{}` placeholders in order.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::LazyLock;

mod table;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Lang {
    #[default]
    Ko,
    En,
    Ja,
    Zh,
}

impl Lang {
    pub const ALL: [Lang; 4] = [Lang::Ko, Lang::En, Lang::Ja, Lang::Zh];

    /// Code stored in the settings file.
    pub fn code(self) -> &'static str {
        ["ko", "en", "ja", "zh"][self as usize]
    }

    pub fn from_code(c: &str) -> Lang {
        Self::ALL.into_iter().find(|l| l.code() == c).unwrap_or_default()
    }

    /// The language's own name. The language list always shows these, whatever the current
    /// language (Chinese uses two characters the Korean font also has, so it renders before the
    /// Chinese font is loaded).
    pub fn native_name(self) -> &'static str {
        ["한국어", "English", "日本語", "中文"][self as usize]
    }

    /// English name, used when telling the agent which language to answer in.
    pub fn english_name(self) -> &'static str {
        ["Korean", "English", "Japanese", "Simplified Chinese"][self as usize]
    }
}

/// Current UI language. Changing it in Settings switches all text from the next frame.
static LANG: AtomicU8 = AtomicU8::new(0);

pub fn set(l: Lang) {
    LANG.store(l as u8, Ordering::Relaxed);
}

pub fn current() -> Lang {
    Lang::ALL[LANG.load(Ordering::Relaxed) as usize]
}

/// Korean source → [English, Japanese, Chinese].
static INDEX: LazyLock<HashMap<&'static str, [&'static str; 3]>> =
    LazyLock::new(|| table::TABLE.iter().map(|[ko, en, ja, zh]| (*ko, [*en, *ja, *zh])).collect());

/// `ko` translated to `l`, or the source if it's not in the table.
pub fn t_in(l: Lang, ko: &str) -> &str {
    if l == Lang::Ko {
        return ko;
    }
    INDEX.get(ko).map_or(ko, |row| row[l as usize - 1])
}

/// Korean source text in the current language.
pub fn tr(ko: &str) -> &str {
    t_in(current(), ko)
}

/// Text with values: fills the translation's `{}` placeholders with `args` in order.
pub fn trf(ko: &str, args: &[&dyn Display]) -> String {
    fill(tr(ko), args)
}

fn fill(pattern: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut rest = pattern;
    let mut args = args.iter();
    while let Some(i) = rest.find("{}") {
        out.push_str(&rest[..i]);
        if let Some(a) = args.next() {
            use std::fmt::Write as _;
            let _ = write!(out, "{a}");
        }
        rest = &rest[i + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_and_fill() {
        assert_eq!(t_in(Lang::Ko, "설정"), "설정");
        assert_eq!(t_in(Lang::En, "설정"), "Settings");
        assert_eq!(t_in(Lang::Ja, "없는 글 그대로"), "없는 글 그대로");
        assert_eq!(fill("단계 {} / {}", &[&3, &"60"]), "단계 3 / 60");
        assert_eq!(fill("{}개", &[]), "개");
        assert!(Lang::ALL.iter().all(|l| Lang::from_code(l.code()) == *l));
        assert_eq!(Lang::from_code("fr"), Lang::Ko);
    }

    /// Every table row: no duplicate sources, no empty translations, and the same number of `{}`
    /// placeholders in each language.
    #[test]
    fn table_rows_are_consistent() {
        let mut seen = std::collections::HashSet::new();
        for row in table::TABLE {
            assert!(seen.insert(row[0]), "원문이 두 번 있음: {}", row[0]);
            let n = row[0].matches("{}").count();
            for (i, s) in row.iter().enumerate().skip(1) {
                assert!(!s.trim().is_empty(), "빈 번역 [{i}]: {}", row[0]);
                assert_eq!(s.matches("{}").count(), n, "자리표시 수가 다름 [{i}]: {} → {s}", row[0]);
            }
        }
    }

    /// All Korean UI text in the UI code (src/app) must be in the table, so new text can't ship
    /// untranslated. Text compared verbatim (status prefixes and the like) and text inside tests is
    /// excluded.
    #[test]
    fn every_ui_string_is_translated() {
        const SOURCES: &[(&str, &str)] = &[
            ("app.rs", include_str!("../app.rs")),
            ("ai.rs", include_str!("ai.rs")),
            ("connect.rs", include_str!("connect.rs")),
            ("detail.rs", include_str!("detail.rs")),
            ("labels.rs", include_str!("labels.rs")),
            ("pip.rs", include_str!("pip.rs")),
            ("prefs.rs", include_str!("prefs.rs")),
            ("screens.rs", include_str!("screens.rs")),
            ("settings.rs", include_str!("settings.rs")),
            ("setup.rs", include_str!("setup.rs")),
            ("sidebar.rs", include_str!("sidebar.rs")),
            ("theme.rs", include_str!("theme.rs")),
            ("titlebar.rs", include_str!("titlebar.rs")),
            ("tour.rs", include_str!("tour.rs")),
            ("wizard.rs", include_str!("wizard.rs")),
        ];
        let mut missing = Vec::new();
        for (file, src) in SOURCES {
            for s in korean_literals(src) {
                if !INDEX.contains_key(s.as_str()) && !NOT_UI.contains(&s.as_str()) {
                    missing.push(format!("{file}: {s}"));
                }
            }
        }
        // reason strings the agent puts in its status show up in the UI too, so they must be in the table
        for s in super::super::labels::AGENT_REASONS {
            if !INDEX.contains_key(s) {
                missing.push(format!("agent: {s}"));
            }
        }
        // text from other modules shown through tr(): provider names, key hints, local-app card titles
        let dynamic = crate::llm::Provider::ALL.iter().flat_map(|p| [p.label(), p.key_hint()]).chain([crate::platform::text::LOCAL_APPS]);
        for s in dynamic.filter(|s| s.chars().any(|c| ('가'..='힣').contains(&c))) {
            if !INDEX.contains_key(s) {
                missing.push(format!("dynamic: {s}"));
            }
        }
        assert!(missing.is_empty(), "번역이 없는 글 {}개:\n{}", missing.len(), missing.join("\n"));
    }

    /// Strings never shown in the UI, only compared verbatim.
    // "앱" / " 앱": words auto mode uses to spot an app named in the task, as in "날씨 앱" (wizard.rs auto_app)
    const NOT_UI: &[&str] = &["오류", "연결 실패", "앱", " 앱"];

    /// Values of string literals containing Korean in a source file (comments and the trailing test
    /// module excluded). `format!` placeholders are only ever `{}`, matching `trf`, so values are
    /// compared as is.
    fn korean_literals(src: &str) -> Vec<String> {
        // some files use CRLF, so split on `mod tests {` alone
        let src = src.split("mod tests {").next().unwrap_or(src);
        let b: Vec<char> = src.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                '/' if b.get(i + 1) == Some(&'/') => {
                    while i < b.len() && b[i] != '\n' {
                        i += 1;
                    }
                }
                '\'' => {
                    // skip char literals ('"', '\'') and pass over lifetimes ('a)
                    let end = if b.get(i + 1) == Some(&'\\') { i + 3 } else { i + 2 };
                    i = if b.get(end) == Some(&'\'') { end + 1 } else { i + 1 };
                    continue;
                }
                '"' => {
                    let mut s = String::new();
                    i += 1;
                    while i < b.len() && b[i] != '"' {
                        if b[i] == '\\' {
                            i += 1;
                            match b.get(i) {
                                Some('n') => s.push('\n'),
                                Some('t') => s.push('\t'),
                                // `\` at end of line: skip the next line's leading whitespace
                                Some('\r' | '\n') => {
                                    while b.get(i + 1).is_some_and(|c| c.is_whitespace()) {
                                        i += 1;
                                    }
                                }
                                Some(c) => s.push(*c),
                                None => {}
                            }
                        } else {
                            s.push(b[i]);
                        }
                        i += 1;
                    }
                    if s.chars().any(|c| ('가'..='힣').contains(&c)) {
                        out.push(s);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        out
    }
}
