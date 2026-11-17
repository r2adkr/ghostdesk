//! 화면 언어: 한국어(원문) · English · 日本語 · 中文.
//!
//! 화면 글은 코드에 한국어 원문 그대로 적고 `tr("…")` 로 감싼다. 다른 언어는 `table.rs` 의 표에서 찾고,
//! 표에 없으면 원문이 그대로 보인다 (빠뜨린 글은 `every_ui_string_is_translated` 테스트가 잡는다).
//! 값을 끼워 넣는 글은 `trf("… {} …", &[&값])` — 자리표시 `{}` 는 언어마다 순서대로 채운다.

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

    /// 설정 파일에 적는 코드
    pub fn code(self) -> &'static str {
        ["ko", "en", "ja", "zh"][self as usize]
    }

    pub fn from_code(c: &str) -> Lang {
        Self::ALL.into_iter().find(|l| l.code() == c).unwrap_or_default()
    }

    /// 그 언어로 쓴 이름. 언어 목록은 지금 언어와 상관없이 늘 이 이름으로 보인다
    /// (중국어는 한국어 글꼴에도 있는 두 글자만 써서, 중국어 글꼴을 불러오기 전에도 깨지지 않게).
    pub fn native_name(self) -> &'static str {
        ["한국어", "English", "日本語", "中文"][self as usize]
    }

    /// 에이전트에게 '이 언어로 답하라' 고 알릴 때의 영어 이름
    pub fn english_name(self) -> &'static str {
        ["Korean", "English", "Japanese", "Simplified Chinese"][self as usize]
    }
}

/// 지금 화면 언어 (설정에서 바꾸면 다음 프레임부터 모든 글이 바뀐다)
static LANG: AtomicU8 = AtomicU8::new(0);

pub fn set(l: Lang) {
    LANG.store(l as u8, Ordering::Relaxed);
}

pub fn current() -> Lang {
    Lang::ALL[LANG.load(Ordering::Relaxed) as usize]
}

/// 한국어 원문 → [영어, 일본어, 중국어]
static INDEX: LazyLock<HashMap<&'static str, [&'static str; 3]>> =
    LazyLock::new(|| table::TABLE.iter().map(|[ko, en, ja, zh]| (*ko, [*en, *ja, *zh])).collect());

/// `ko` 를 `l` 로 옮긴 글. 표에 없으면 원문 그대로.
pub fn t_in(l: Lang, ko: &str) -> &str {
    if l == Lang::Ko {
        return ko;
    }
    INDEX.get(ko).map_or(ko, |row| row[l as usize - 1])
}

/// 한국어 원문을 지금 언어로
pub fn tr(ko: &str) -> &str {
    t_in(current(), ko)
}

/// 값을 끼워 넣는 글: 옮긴 글의 `{}` 를 `args` 로 차례대로 채운다
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

    /// 표의 모든 줄: 원문이 겹치지 않고, 빈 번역이 없고, 자리표시 `{}` 수가 언어마다 같다
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

    /// 화면 코드(src/app)의 한국어 글은 모두 표에 있어야 한다 (새 글을 넣고 번역을 빠뜨리지 않게).
    /// 원문 그대로 비교하는 글(상태 글의 앞머리 등)과 테스트 안의 글은 뺀다.
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
            ("sidebar.rs", include_str!("sidebar.rs")),
            ("theme.rs", include_str!("theme.rs")),
            ("titlebar.rs", include_str!("titlebar.rs")),
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
        // 에이전트가 상태에 넣는 이유 글도 화면에 보이므로 표에 있어야 한다
        for s in super::super::labels::AGENT_REASONS {
            if !INDEX.contains_key(s) {
                missing.push(format!("agent: {s}"));
            }
        }
        // 다른 모듈에서 가져와 tr() 로 감싸 보이는 글: AI 회사 이름 · 키 안내 · 로컬 앱 카드 제목
        let dynamic = crate::llm::Provider::ALL.iter().flat_map(|p| [p.label(), p.key_hint()]).chain([crate::platform::text::LOCAL_APPS]);
        for s in dynamic.filter(|s| s.chars().any(|c| ('가'..='힣').contains(&c))) {
            if !INDEX.contains_key(s) {
                missing.push(format!("dynamic: {s}"));
            }
        }
        assert!(missing.is_empty(), "번역이 없는 글 {}개:\n{}", missing.len(), missing.join("\n"));
    }

    /// 화면에 보이지 않고 원문 그대로 비교하는 데 쓰는 글
    const NOT_UI: &[&str] = &["오류", "연결 실패"];

    /// 소스에서 한국어가 든 문자열 리터럴의 값 (주석과 맨 아래 테스트 모듈은 뺀다). `format!` 의
    /// 자리표시는 `trf` 와 같은 `{}` 로 맞춰 둔 코드만 있으므로 값 그대로 비교한다.
    fn korean_literals(src: &str) -> Vec<String> {
        // 줄바꿈이 CRLF 인 파일도 있어 `mod tests {` 만으로 자른다
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
                    // 글자 리터럴('"', '\'') 은 건너뛰고, 수명('a) 은 그대로 지나간다
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
                                // 줄 끝의 `\` : 다음 줄 앞 공백까지 건너뛴다
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
