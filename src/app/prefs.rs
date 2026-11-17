//! 사용자 설정 저장: 언어 · AI 연결 목록(회사·이름·주소·모델·키) · 지금 쓰는 연결 · 생각 깊이 · 가상 마우스 ·
//! 화면이 잠겨도 계속 작업.
//!
//! 파일은 <데이터 루트>/prefs.json (자동 연결 설정인 settings.json 과는 따로 — install.rs 가 그 파일을 쓴다).
//! API 키는 Windows 에서만, 그 Windows 계정으로만 풀 수 있게 DPAPI 로 암호화해 둔다.
//! 다른 OS 는 키를 저장하지 않는다 (환경변수로 채우거나 켤 때마다 넣는다).
//!
//! 예전 형식(회사마다 키·모델 한 칸, OpenAI 호환 주소 하나)도 읽어서 연결 목록으로 옮긴다 (`migrate`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::ai::AiConn;
use super::i18n::Lang;
use super::*;

/// 키를 이 PC 에 기억할 수 있는가 (암호화 저장소가 있는 OS 만)
pub(super) const CAN_REMEMBER_KEYS: bool = cfg!(windows);

#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
#[serde(default)]
pub(super) struct Prefs {
    /// 화면 언어 코드 (ko·en·ja·zh). 비어 있으면 한국어
    lang: String,
    /// AI 연결 목록 (None 이면 예전 형식의 파일)
    conns: Option<Vec<SavedConn>>,
    /// 지금 쓰는 연결의 순서 (conns 안)
    current: Option<usize>,
    effort: Option<usize>,
    show_cursor: Option<bool>,
    remember_keys: Option<bool>,
    /// 화면이 잠겨도 계속 작업 (작업 중 잠자기 방지)
    keep_awake: Option<bool>,
    /// 처음 켤 때의 사용 설명을 이미 봤는가
    tour_seen: Option<bool>,
    // ── 예전 형식 (읽기만 한다) ──
    /// 지금 쓰는 AI 회사 (`Provider::short_name`)
    #[serde(skip_serializing)]
    provider: String,
    /// 회사 → 고른 모델
    #[serde(skip_serializing)]
    models: BTreeMap<String, String>,
    /// 회사 → 암호화한 API 키 (base64)
    #[serde(skip_serializing)]
    keys: BTreeMap<String, String>,
    /// 'OpenAI 호환 API' 의 주소
    #[serde(skip_serializing)]
    custom_url: String,
    /// 이 버전이 모르는 칸 (나중 버전이 쓴 것). 그대로 다시 써서, 예전 버전으로 한 번 켰다가 돌아와도 지워지지 않게 한다
    #[serde(flatten)]
    other: serde_json::Map<String, serde_json::Value>,
}

/// 저장하는 연결 하나 (키는 암호화한 값, 기억하지 않으면 빈 글)
#[derive(Serialize, Deserialize, Default, Debug, PartialEq, Clone)]
#[serde(default)]
struct SavedConn {
    /// `Provider::short_name`
    provider: String,
    name: String,
    url: String,
    model: String,
    key: String,
}

fn path() -> std::path::PathBuf {
    crate::platform::paths::data_root().join("prefs.json")
}

fn provider_named(name: &str) -> Option<Provider> {
    Provider::ALL.into_iter().find(|p| p.short_name() == name)
}

/// 예전 형식 → 연결 목록: 키가 있거나(저장·환경변수) 지금 쓰던 회사, 그리고 주소를 넣어 둔 OpenAI 호환.
/// 지금 쓰던 회사의 순서도 돌려준다.
fn migrate(p: &Prefs) -> (Vec<SavedConn>, Option<usize>) {
    let mut out = Vec::new();
    let mut current = None;
    for pr in Provider::ALL {
        let name = pr.short_name();
        let used = p.provider == name;
        let url = if pr.needs_url() { p.custom_url.trim().to_string() } else { String::new() };
        let keep = used || p.keys.contains_key(name) || (pr.needs_key() && !pr.env_key().is_empty()) || !url.is_empty();
        if !keep {
            continue;
        }
        if used {
            current = Some(out.len());
        }
        out.push(SavedConn {
            provider: name.into(),
            name: String::new(),
            url,
            model: p.models.get(name).cloned().unwrap_or_default(),
            key: p.keys.get(name).cloned().unwrap_or_default(),
        });
    }
    (out, current)
}

impl GhostApp {
    /// 켤 때 한 번: 저장해 둔 설정을 적용한다 (없거나 깨졌으면 기본값 그대로)
    pub(super) fn load_prefs(&mut self) {
        let Ok(bytes) = std::fs::read(path()) else { return };
        let Ok(p) = serde_json::from_slice::<Prefs>(&bytes) else { return };
        self.apply_prefs(p);
    }

    fn apply_prefs(&mut self, p: Prefs) {
        self.lang = Lang::from_code(&p.lang);
        self.prefs_other = p.other.clone();
        let (saved, current) = match &p.conns {
            Some(c) => (c.clone(), p.current),
            None => migrate(&p),
        };
        let conns: Vec<AiConn> = saved
            .iter()
            .filter_map(|s| {
                let provider = provider_named(&s.provider)?;
                self.ai_next_id += 1;
                let mut c = AiConn::new(self.ai_next_id, provider);
                c.name = s.name.clone();
                c.url = s.url.clone();
                if !s.model.trim().is_empty() {
                    c.model = s.model.clone();
                }
                // 저장한 키가 환경변수보다 앞선다 (설정에서 직접 넣은 값이 최신)
                if let Some(k) = secret::open(&s.key) {
                    c.key = k;
                }
                Some(c)
            })
            .collect();
        // 예전 파일에 쓸 만한 것이 하나도 없으면 처음 목록(환경변수·Claude)을 그대로 둔다
        if p.conns.is_some() || !conns.is_empty() {
            self.ai_current = current.and_then(|i| conns.get(i)).or(conns.first()).map_or(0, |c| c.id);
            self.ai = conns;
        }
        if let Some(e) = p.effort.filter(|e| *e < EFFORTS.len()) {
            self.effort_idx = e;
        }
        if let Some(c) = p.show_cursor {
            self.show_cursor = c;
        }
        if let Some(k) = p.keep_awake {
            self.keep_awake = k;
        }
        self.tour_seen = p.tour_seen.unwrap_or(false);
        self.remember_keys = p.remember_keys.unwrap_or(CAN_REMEMBER_KEYS) && CAN_REMEMBER_KEYS;
    }

    fn to_prefs(&self) -> Prefs {
        let conns = self
            .ai
            .iter()
            .map(|c| SavedConn {
                provider: c.provider.short_name().into(),
                name: c.name.trim().into(),
                url: c.url.trim().into(),
                model: c.model.trim().into(),
                // 환경변수에서 온 키와 같으면 저장하지 않는다 (그 자리에 이미 있으므로)
                key: if self.remember_keys && !c.key.trim().is_empty() && c.key != c.provider.env_key() {
                    secret::seal(c.key.trim()).unwrap_or_default()
                } else {
                    String::new()
                },
            })
            .collect();
        Prefs {
            lang: self.lang.code().into(),
            conns: Some(conns),
            current: self.ai.iter().position(|c| c.id == self.ai_current),
            effort: Some(self.effort_idx),
            show_cursor: Some(self.show_cursor),
            remember_keys: Some(self.remember_keys),
            keep_awake: Some(self.keep_awake),
            tour_seen: Some(self.tour_seen),
            other: self.prefs_other.clone(),
            ..Default::default()
        }
    }

    /// 지금 설정을 파일에 쓴다. 설정 창을 닫을 때 · 모델을 고를 때 · 언어를 바꿀 때 · 끌 때 부른다.
    pub(super) fn save_prefs(&self) {
        let p = path();
        let json = match serde_json::to_vec_pretty(&self.to_prefs()) {
            Ok(j) => j,
            Err(_) => return,
        };
        // 쓰다가 꺼져도 파일이 반쯤 남지 않게 옆에 쓰고 바꿔 끼운다
        let tmp = p.with_extension("json.tmp");
        let ok = p.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok())
            && std::fs::write(&tmp, json).is_ok()
            && std::fs::rename(&tmp, &p).is_ok();
        if !ok {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// API 키 봉인: Windows 는 DPAPI(그 사용자 계정으로만 풀림), 그 밖의 OS 는 저장하지 않는다.
mod secret {
    use base64::Engine as _;

    const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

    pub(super) fn seal(plain: &str) -> Option<String> {
        dpapi(plain.as_bytes(), true).map(|v| B64.encode(v))
    }

    pub(super) fn open(sealed: &str) -> Option<String> {
        let raw = B64.decode(sealed).ok()?;
        String::from_utf8(dpapi(&raw, false)?).ok().filter(|s| !s.trim().is_empty())
    }

    #[cfg(windows)]
    fn dpapi(data: &[u8], protect: bool) -> Option<Vec<u8>> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        };
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        use std::ptr::{null, null_mut};
        // UI_FORBIDDEN: 암호화·복호화 중에 어떤 창도 띄우지 않는다
        let ok = unsafe {
            if protect {
                CryptProtectData(&input, null(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            } else {
                CryptUnprotectData(&input, null_mut(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            }
        };
        if ok == 0 || out.pbData.is_null() {
            return None;
        }
        let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
        unsafe { LocalFree(out.pbData as _) };
        Some(v)
    }

    // ponytail: macOS 는 키를 저장하지 않는다. 필요해지면 Keychain(Security.framework)으로.
    #[cfg(not(windows))]
    fn dpapi(_data: &[u8], _protect: bool) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn keys_round_trip_only_for_this_account() {
        let sealed = secret::seal("sk-test-123").expect("DPAPI 암호화");
        assert!(!sealed.contains("sk-test"), "평문이 남으면 안 됨");
        assert_eq!(secret::open(&sealed).as_deref(), Some("sk-test-123"));
        assert_eq!(secret::open("not base64!"), None);
    }

    #[test]
    fn old_format_migrates_to_connections() {
        let p: Prefs = serde_json::from_str(
            r#"{"provider":"Gemini","models":{"Claude":"claude-x","Gemini":"gemini-y"},"keys":{"Claude":"sealed"},"custom_url":"http://localhost:1234/v1"}"#,
        )
        .unwrap();
        let (conns, current) = migrate(&p);
        let names: Vec<&str> = conns.iter().map(|c| c.provider.as_str()).collect();
        // 키를 저장해 둔 Claude, 쓰던 Gemini, 주소를 넣어 둔 OpenAI 호환 (환경변수에 키가 있는 회사는 더 붙을 수 있다)
        for n in ["Claude", "Gemini", "Custom"] {
            assert!(names.contains(&n), "{names:?}");
        }
        let cur = &conns[current.unwrap()];
        assert_eq!((cur.provider.as_str(), cur.model.as_str()), ("Gemini", "gemini-y"));
        assert_eq!(conns.iter().find(|c| c.provider == "Custom").unwrap().url, "http://localhost:1234/v1");
        // 새 형식으로 쓸 때 예전 칸은 남기지 않는다
        let json = serde_json::to_string(&Prefs { conns: Some(conns), ..Default::default() }).unwrap();
        assert!(!json.contains("custom_url") && !json.contains("\"models\""), "{json}");
    }

    /// 이 버전이 모르는 칸은 읽었다가 그대로 다시 쓴다 (나중 버전의 설정을 지우지 않게)
    #[test]
    fn unknown_fields_survive_a_save() {
        let p: Prefs = serde_json::from_str(r#"{"lang":"en","future_flag":true,"future_list":[1,2]}"#).unwrap();
        assert_eq!(p.other.get("future_flag"), Some(&serde_json::json!(true)));
        let again: serde_json::Value = serde_json::to_value(&Prefs { lang: p.lang.clone(), other: p.other.clone(), ..Default::default() }).unwrap();
        assert_eq!(again["future_flag"], serde_json::json!(true));
        assert_eq!(again["future_list"], serde_json::json!([1, 2]));
        // 이 버전이 아는 예전 칸은 모르는 칸으로 섞여 들어가지 않는다
        let old: Prefs = serde_json::from_str(r#"{"provider":"Claude","custom_url":"x"}"#).unwrap();
        assert!(old.other.is_empty(), "{:?}", old.other);
    }

    #[test]
    fn old_or_partial_files_load_with_defaults() {
        let p: Prefs = serde_json::from_str(r#"{"lang":"ja","models":{"Claude":"claude-x"}}"#).unwrap();
        assert_eq!(p.lang, "ja");
        assert_eq!(p.models.get("Claude").map(String::as_str), Some("claude-x"));
        assert_eq!(p.effort, None);
        // 모르는 칸이 있어도 읽힌다 (나중 버전의 파일)
        assert!(serde_json::from_str::<Prefs>(r#"{"future":1}"#).is_ok());
        assert!(Provider::ALL.iter().all(|p| provider_named(p.short_name()) == Some(*p)));
    }
}
