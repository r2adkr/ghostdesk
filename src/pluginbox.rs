//! Plugins: extra code for the engine (pi packages from npm, git or a local folder). They run inside the
//! engine's process with the agent's AI key and can reach this computer without going through GhostDesk's
//! approval windows, so adding one takes the permission switch, a warning and an explicit confirmation.
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Plugin {
    pub source: String,
    pub enabled: bool,
}

pub fn path() -> PathBuf {
    crate::platform::paths::data_root().join("engine").join("plugins.json")
}

/// Sources that can be added: an npm package, a git repository, or a folder on this computer.
pub fn valid_source(s: &str) -> Result<(), &'static str> {
    let s = s.trim();
    if s.is_empty() || s.len() > 300 || s.chars().any(|c| c.is_control() || c == ' ') || s.starts_with('-') {
        return Err("주소를 확인해 주세요");
    }
    let local = std::path::Path::new(s).is_absolute();
    if s.starts_with("npm:") || s.starts_with("git:") || s.starts_with("https://") || local {
        Ok(())
    } else {
        Err("npm:이름, git:주소, https:// 주소, 또는 폴더의 전체 경로만 쓸 수 있어요")
    }
}

pub fn load() -> Vec<Plugin> {
    let Ok(text) = std::fs::read_to_string(path()) else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return Vec::new() };
    v["plugins"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some(Plugin { source: p["source"].as_str()?.to_string(), enabled: p["enabled"].as_bool().unwrap_or(true) }))
        .filter(|p| valid_source(&p.source).is_ok())
        .collect()
}

pub fn save(list: &[Plugin]) -> std::io::Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(&json!({ "plugins": list.iter().map(|x| json!({ "source": x.source, "enabled": x.enabled })).collect::<Vec<_>>() }))?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &p)
}

/// Sources of the plugins an agent loads (only while plugin installs are permitted).
pub fn active() -> Vec<String> {
    if !crate::access::get().plugins {
        return Vec::new();
    }
    load().into_iter().filter(|p| p.enabled).map(|p| p.source).collect()
}

fn run(args: &[&str]) -> Result<(), String> {
    let exe = crate::pi::find().ok_or("엔진이 설치되지 않았어요")?;
    let home = path().parent().map(PathBuf::from).ok_or("경로 오류")?;
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let mut c = crate::pi::engine_command(&exe, &home);
    c.args(args).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let out = c.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let text = if err.trim().is_empty() { String::from_utf8_lossy(&out.stdout).into_owned() } else { err.into_owned() };
        Err(crate::llm::short(text.trim(), 300))
    }
}

/// Fetches and checks a plugin (blocking; call from a background thread), then lists it.
pub fn install(source: &str) -> Result<(), String> {
    valid_source(source).map_err(String::from)?;
    if !crate::access::get().plugins {
        return Err("권한 설정에서 플러그인 설치를 먼저 허용해 주세요".into());
    }
    run(&["install", source])?;
    let mut list = load();
    if !list.iter().any(|p| p.source == source) {
        list.push(Plugin { source: source.to_string(), enabled: true });
    }
    save(&list).map_err(|e| e.to_string())
}

/// Removes a plugin from the list and from the engine (blocking).
pub fn remove(source: &str) -> Result<(), String> {
    let _ = run(&["remove", source]);
    let list: Vec<Plugin> = load().into_iter().filter(|p| p.source != source).collect();
    save(&list).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_source_kinds_are_accepted() {
        for ok in ["npm:@scope/pkg@1.0.0", "git:github.com/a/b@v1", "https://github.com/a/b"] {
            assert!(valid_source(ok).is_ok(), "{ok}");
        }
        for bad in ["", "pkg", "--extension x", "npm:a b", "ftp://x", "./relative", "npm:a\nb"] {
            assert!(valid_source(bad).is_err(), "{bad:?}");
        }
        #[cfg(unix)]
        assert!(valid_source("/opt/plugin").is_ok());
    }
}
