//! 사용자 설정 저장: 언어 · 지금 쓰는 AI · 회사별 모델 · 생각 깊이 · 가상 마우스 · API 키.
//!
//! 파일은 <데이터 루트>/prefs.json (자동 연결 설정인 settings.json 과는 따로 — install.rs 가 그 파일을 쓴다).
//! API 키는 Windows 에서만, 그 Windows 계정으로만 풀 수 있게 DPAPI 로 암호화해 둔다.
//! 다른 OS 는 키를 저장하지 않는다 (환경변수로 채우거나 켤 때마다 넣는다).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::*;
use super::i18n::Lang;

/// 키를 이 PC 에 기억할 수 있는가 (암호화 저장소가 있는 OS 만)
pub(super) const CAN_REMEMBER_KEYS: bool = cfg!(windows);

#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
#[serde(default)]
pub(super) struct Prefs {
    /// 화면 언어 코드 (ko·en·ja·zh). 비어 있으면 한국어
    lang: String,
    /// 지금 쓰는 AI 회사 (`Provider::short_name`)
    provider: String,
    /// 회사 → 고른 모델
    models: BTreeMap<String, String>,
    effort: Option<usize>,
    show_cursor: Option<bool>,
    remember_keys: Option<bool>,
    /// 회사 → 암호화한 API 키 (base64)
    keys: BTreeMap<String, String>,
}

fn path() -> std::path::PathBuf {
    crate::platform::paths::data_root().join("prefs.json")
}

fn provider_named(name: &str) -> Option<Provider> {
    Provider::ALL.into_iter().find(|p| p.short_name() == name)
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
        if let Some(pr) = provider_named(&p.provider) {
            self.provider = pr;
        }
        for (i, pr) in Provider::ALL.iter().enumerate() {
            if let Some(m) = p.models.get(pr.short_name()).filter(|m| !m.trim().is_empty()) {
                self.models[i] = m.clone();
            }
            // 저장한 키가 환경변수보다 앞선다 (설정에서 직접 넣은 값이 최신)
            if let Some(k) = p.keys.get(pr.short_name()).and_then(|b| secret::open(b)) {
                self.api_keys[i] = k;
            }
        }
        if let Some(e) = p.effort.filter(|e| *e < EFFORTS.len()) {
            self.effort_idx = e;
        }
        if let Some(c) = p.show_cursor {
            self.show_cursor = c;
        }
        self.remember_keys = p.remember_keys.unwrap_or(CAN_REMEMBER_KEYS) && CAN_REMEMBER_KEYS;
    }

    fn to_prefs(&self) -> Prefs {
        let names = Provider::ALL.iter().map(|p| p.short_name().to_string());
        Prefs {
            lang: self.lang.code().into(),
            provider: self.provider.short_name().into(),
            models: names.clone().zip(self.models.iter().cloned()).collect(),
            effort: Some(self.effort_idx),
            show_cursor: Some(self.show_cursor),
            remember_keys: Some(self.remember_keys),
            keys: if self.remember_keys {
                // 환경변수에서 온 키와 같으면 저장하지 않는다 (그 자리에 이미 있으므로)
                Provider::ALL
                    .iter()
                    .zip(&self.api_keys)
                    .filter(|(p, k)| !k.trim().is_empty() && **k != p.env_key())
                    .filter_map(|(p, k)| Some((p.short_name().to_string(), secret::seal(k.trim())?)))
                    .collect()
            } else {
                BTreeMap::new()
            },
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
