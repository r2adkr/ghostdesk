//! User preferences: language, AI connections (provider, name, URL, model, key), current
//! connection, thinking depth, virtual mouse, and keep working while locked.
//!
//! Stored in `<data root>/prefs.json`, separate from settings.json (auto-connect, written by
//! install.rs). API keys are only stored on Windows, encrypted with DPAPI so only that Windows
//! account can decrypt them. Other OSes don't store keys (fill them from the environment or
//! enter them each launch).
//!
//! The old format (one key and model per provider, a single OpenAI-compatible URL) is still read
//! and migrated to the connection list (`migrate`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::ai::AiConn;
use super::i18n::Lang;
use super::*;

/// Whether keys can be remembered on this PC (only on OSes with encrypted storage).
pub(super) const CAN_REMEMBER_KEYS: bool = cfg!(windows);

#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
#[serde(default)]
pub(super) struct Prefs {
    /// UI language code (ko, en, ja, zh). Empty means Korean.
    lang: String,
    /// AI connections (`None` for an old-format file).
    conns: Option<Vec<SavedConn>>,
    /// Index of the current connection in `conns`.
    current: Option<usize>,
    effort: Option<usize>,
    show_cursor: Option<bool>,
    remember_keys: Option<bool>,
    /// Keep working while the screen is locked (prevents sleep during work).
    keep_awake: Option<bool>,
    /// Whether the first-launch guide was already seen.
    tour_seen: Option<bool>,
    // ---- old format (read only) ----
    /// Current AI provider (`Provider::short_name`).
    #[serde(skip_serializing)]
    provider: String,
    /// Provider → chosen model.
    #[serde(skip_serializing)]
    models: BTreeMap<String, String>,
    /// Provider → encrypted API key (base64).
    #[serde(skip_serializing)]
    keys: BTreeMap<String, String>,
    /// URL of the OpenAI-compatible API.
    #[serde(skip_serializing)]
    custom_url: String,
    /// Fields this version doesn't know (written by a newer one). Written back as is, so launching
    /// an older version once doesn't wipe them.
    #[serde(flatten)]
    other: serde_json::Map<String, serde_json::Value>,
}

/// One saved connection (key is encrypted; empty if not remembered).
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

/// Old format → connection list: providers with a key (saved or from the environment) or in
/// use, plus the OpenAI-compatible one if it has a URL. Also returns the index of the provider in
/// use.
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
    /// Applies saved preferences once at startup (defaults stay if missing or broken).
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
                // a saved key wins over the environment (a value entered in Settings is newer)
                if let Some(k) = secret::open(&s.key) {
                    c.key = k;
                }
                Some(c)
            })
            .collect();
        // if the old file had nothing usable, keep the initial list (environment, Claude)
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
                // don't store a key equal to the one from the environment (it's already there)
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

    /// Writes the current preferences. Called when Settings closes, a model is picked, the language
    /// changes, and on quit.
    pub(super) fn save_prefs(&self) {
        let p = path();
        let json = match serde_json::to_vec_pretty(&self.to_prefs()) {
            Ok(j) => j,
            Err(_) => return,
        };
        // write next to it and swap in, so quitting mid-write never leaves half a file
        let tmp = p.with_extension("json.tmp");
        let ok = p.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok())
            && std::fs::write(&tmp, json).is_ok()
            && std::fs::rename(&tmp, &p).is_ok();
        if !ok {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// API key sealing: DPAPI on Windows (only that user account can decrypt); not stored on other
/// OSes.
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
        // UI_FORBIDDEN: never show any window while encrypting or decrypting
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

    // ponytail: macOS doesn't store keys. Use the Keychain (Security.framework) if that's ever needed.
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
        // Claude with a saved key, Gemini in use, OpenAI-compatible with a URL (providers with an
        // environment key may be added too)
        for n in ["Claude", "Gemini", "Custom"] {
            assert!(names.contains(&n), "{names:?}");
        }
        let cur = &conns[current.unwrap()];
        assert_eq!((cur.provider.as_str(), cur.model.as_str()), ("Gemini", "gemini-y"));
        assert_eq!(conns.iter().find(|c| c.provider == "Custom").unwrap().url, "http://localhost:1234/v1");
        // writing the new format leaves no old fields behind
        let json = serde_json::to_string(&Prefs { conns: Some(conns), ..Default::default() }).unwrap();
        assert!(!json.contains("custom_url") && !json.contains("\"models\""), "{json}");
    }

    /// Fields this version doesn't know are read and written back unchanged (so a newer version's
    /// settings survive).
    #[test]
    fn unknown_fields_survive_a_save() {
        let p: Prefs = serde_json::from_str(r#"{"lang":"en","future_flag":true,"future_list":[1,2]}"#).unwrap();
        assert_eq!(p.other.get("future_flag"), Some(&serde_json::json!(true)));
        let again: serde_json::Value = serde_json::to_value(&Prefs { lang: p.lang.clone(), other: p.other.clone(), ..Default::default() }).unwrap();
        assert_eq!(again["future_flag"], serde_json::json!(true));
        assert_eq!(again["future_list"], serde_json::json!([1, 2]));
        // old fields this version knows don't leak into the unknown fields
        let old: Prefs = serde_json::from_str(r#"{"provider":"Claude","custom_url":"x"}"#).unwrap();
        assert!(old.other.is_empty(), "{:?}", old.other);
    }

    #[test]
    fn old_or_partial_files_load_with_defaults() {
        let p: Prefs = serde_json::from_str(r#"{"lang":"ja","models":{"Claude":"claude-x"}}"#).unwrap();
        assert_eq!(p.lang, "ja");
        assert_eq!(p.models.get("Claude").map(String::as_str), Some("claude-x"));
        assert_eq!(p.effort, None);
        // unknown fields still parse (a newer version's file)
        assert!(serde_json::from_str::<Prefs>(r#"{"future":1}"#).is_ok());
        assert!(Provider::ALL.iter().all(|p| provider_named(p.short_name()) == Some(*p)));
    }
}
