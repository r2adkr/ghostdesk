//! GhostDesk's own license and the open-source packages built into it (Settings > About > License).
//!
//! `assets/third-party-licenses.json` lists every package compiled into the macOS and Windows
//! builds (normal dependencies only, not build or dev tools) with its license and the license
//! texts it ships, deduplicated (packages that ship no license file get the standard SPDX texts
//! from `assets/licenses/`). It is generated from `cargo metadata` and the packages' own
//! license files by the `third_party_list_is_current` test:
//!
//! ```bash
//! GHOSTDESK_UPDATE_LICENSES=1 cargo test third_party_list_is_current
//! ```
//!
//! The same test fails when Cargo.lock changes and the list wasn't regenerated.

use serde::{Deserialize, Serialize};

/// GhostDesk's own license (Apache License 2.0)
pub const OWN: &str = include_str!("../LICENSE");
/// SPDX id of GhostDesk's license
pub const OWN_SPDX: &str = "Apache-2.0";
/// Name shown on the License page
pub const OWN_NAME: &str = "Apache License 2.0";
pub const OWN_URL: &str = "https://www.apache.org/licenses/LICENSE-2.0";

const THIRD_PARTY: &str = include_str!("../assets/third-party-licenses.json");

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Package {
    pub name: String,
    pub version: String,
    /// SPDX expression from the package (empty if it declares none)
    pub license: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// indexes into `ThirdParty::texts`
    #[serde(default)]
    pub texts: Vec<usize>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct ThirdParty {
    pub packages: Vec<Package>,
    pub texts: Vec<String>,
}

/// Parses the embedded list. Called only while the license view is open, and dropped with it.
pub fn third_party() -> ThirdParty {
    serde_json::from_str(THIRD_PARTY).unwrap_or_default()
}

/// GhostDesk's own license laid out for the window: the file is indented and hard-wrapped for a
/// fixed-width terminal, so the indentation is dropped and paragraphs are joined (`reflow`).
pub fn own_text() -> String {
    let flat: String = OWN.lines().map(str::trim).collect::<Vec<_>>().join("\n");
    reflow(&flat)
}

/// License text for display: hard-wrapped lines inside a paragraph are joined so the text wraps
/// to the window, while blank lines, indented lines and list items keep their own line.
pub fn reflow(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_joinable = false;
    for line in text.lines() {
        let t = line.trim_end();
        let starts_item = t.starts_with(char::is_whitespace)
            || t.starts_with(['-', '*', '•', '(', '#', '|'])
            || t.split_once(['.', ')']).is_some_and(|(n, _)| !n.is_empty() && n.len() <= 3 && n.chars().all(|c| c.is_ascii_alphanumeric()));
        if t.is_empty() {
            out.push_str("\n\n");
            prev_joinable = false;
            continue;
        }
        if prev_joinable && !starts_item {
            out.push(' ');
        } else if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(t.trim_start_matches(|c: char| c == ' ' && !starts_item));
        prev_joinable = true;
    }
    // collapse runs of blank lines
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::path::Path;

    /// The platforms GhostDesk is released for
    const TARGETS: [&str; 3] = ["aarch64-apple-darwin", "x86_64-apple-darwin", "x86_64-pc-windows-msvc"];

    #[test]
    fn own_license_is_apache_2() {
        assert!(OWN.trim_start().starts_with("Apache License"));
        assert!(OWN.contains("Version 2.0, January 2004") && OWN.contains("END OF TERMS AND CONDITIONS"));
        assert_eq!(env!("CARGO_PKG_LICENSE"), OWN_SPDX);
    }

    #[test]
    fn reflow_joins_wrapped_lines_only() {
        let t = "MIT License\n\nCopyright (c) 2020 A\n\nPermission is hereby granted, free of charge,\nto any person.\n\n1. First item\n2. Second\n   indented\n";
        assert_eq!(
            reflow(t),
            "MIT License\n\nCopyright (c) 2020 A\n\nPermission is hereby granted, free of charge, to any person.\n\n1. First item\n2. Second\n   indented"
        );
    }

    #[test]
    fn own_text_reads_as_paragraphs() {
        let t = own_text();
        assert!(t.starts_with("Apache License Version 2.0, January 2004"), "{}", &t[..80]);
        // a definition is one paragraph, not a run of hard-wrapped lines
        assert!(t.contains("\"License\" shall mean the terms and conditions for use, reproduction, and distribution as defined by Sections 1 through 9 of this document."));
    }

    #[test]
    fn embedded_list_parses() {
        let t = third_party();
        assert!(t.packages.len() > 100, "{}", t.packages.len());
        for p in &t.packages {
            assert!(p.texts.iter().all(|&i| i < t.texts.len()), "{}", p.name);
            assert!(!p.texts.is_empty(), "{} has no license text", p.name);
        }
        let names: HashSet<&str> = t.packages.iter().map(|p| p.name.as_str()).collect();
        for n in ["eframe", "egui", "tokio", "serde"] {
            assert!(names.contains(n), "{n}");
        }
    }

    /// Normal (shipped) dependencies of the root package for every release target
    fn shipped_packages() -> Vec<Value> {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut by_id: BTreeMap<String, Value> = BTreeMap::new();
        let mut keep: HashSet<String> = HashSet::new();
        for t in TARGETS {
            let out = std::process::Command::new(&cargo)
                .args(["metadata", "--format-version", "1", "--locked", "--filter-platform", t])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .output()
                .expect("cargo metadata");
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            let m: Value = serde_json::from_slice(&out.stdout).unwrap();
            let nodes: HashMap<&str, &Value> =
                m["resolve"]["nodes"].as_array().unwrap().iter().map(|n| (n["id"].as_str().unwrap(), n)).collect();
            let root = m["resolve"]["root"].as_str().unwrap();
            let mut stack = vec![root];
            let mut seen: HashSet<&str> = HashSet::from([root]);
            while let Some(id) = stack.pop() {
                for d in nodes[id]["deps"].as_array().unwrap() {
                    let normal = d["dep_kinds"].as_array().unwrap().iter().any(|k| k["kind"].is_null());
                    let pkg = d["pkg"].as_str().unwrap();
                    if normal && seen.insert(pkg) {
                        stack.push(pkg);
                    }
                }
            }
            for p in m["packages"].as_array().unwrap() {
                let id = p["id"].as_str().unwrap();
                if seen.contains(id) && id != root {
                    keep.insert(id.to_string());
                    by_id.insert(id.to_string(), p.clone());
                }
            }
        }
        let mut v: Vec<Value> = by_id.into_iter().filter(|(id, _)| keep.contains(id)).map(|(_, p)| p).collect();
        v.sort_by(|a, b| (a["name"].as_str(), a["version"].as_str()).cmp(&(b["name"].as_str(), b["version"].as_str())));
        v
    }

    /// License files a package ships (LICENSE*, LICENCE*, COPYING*, UNLICENSE*, or `license_file`)
    fn license_files(p: &Value) -> Vec<String> {
        let dir = Path::new(p["manifest_path"].as_str().unwrap()).parent().unwrap().to_path_buf();
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|f| {
                        let n = f.file_name().unwrap_or_default().to_string_lossy().to_ascii_uppercase();
                        f.is_file() && ["LICENSE", "LICENCE", "COPYING", "UNLICENSE"].iter().any(|s| n.starts_with(s))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(f) = p["license_file"].as_str() {
            let f = dir.join(f);
            if f.is_file() && !files.contains(&f) {
                files.push(f);
            }
        }
        files.sort();
        files
            .iter()
            .filter_map(|f| std::fs::read(f).ok())
            .map(|b| String::from_utf8_lossy(&b).replace("\r\n", "\n").trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Standard SPDX texts (assets/licenses/<id>.txt) for packages that ship no license file
    fn standard_texts(expr: &str) -> Vec<String> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/licenses");
        let mut v = Vec::new();
        for id in expr.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '/')) {
            if id.is_empty() || matches!(id, "AND" | "OR" | "WITH") {
                continue;
            }
            match std::fs::read_to_string(dir.join(format!("{id}.txt"))) {
                Ok(t) => v.push(t.replace("\r\n", "\n").trim().to_string()),
                Err(_) => panic!("no license file shipped and no standard text for {id}: add assets/licenses/{id}.txt"),
            }
        }
        v
    }

    fn generate() -> ThirdParty {
        let mut out = ThirdParty::default();
        let mut index: HashMap<String, usize> = HashMap::new();
        for p in shipped_packages() {
            let mut texts = Vec::new();
            let mut files = license_files(&p);
            if files.is_empty() {
                files = standard_texts(p["license"].as_str().unwrap_or_default());
            }
            for t in files {
                let i = *index.entry(t.clone()).or_insert_with(|| {
                    out.texts.push(t);
                    out.texts.len() - 1
                });
                if !texts.contains(&i) {
                    texts.push(i);
                }
            }
            out.packages.push(Package {
                name: p["name"].as_str().unwrap().into(),
                version: p["version"].as_str().unwrap().into(),
                license: p["license"].as_str().unwrap_or_default().into(),
                repository: p["repository"].as_str().map(String::from),
                texts,
            });
        }
        out
    }

    #[test]
    fn third_party_list_is_current() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/third-party-licenses.json");
        if std::env::var("GHOSTDESK_UPDATE_LICENSES").is_ok_and(|v| v == "1") {
            let fresh = generate();
            std::fs::write(&path, serde_json::to_string(&fresh).unwrap() + "\n").unwrap();
            return;
        }
        // only names and versions are compared (license files come from the local registry)
        let key = |ps: &[Package]| ps.iter().map(|p| format!("{} {}", p.name, p.version)).collect::<Vec<_>>();
        let want: Vec<String> = shipped_packages()
            .iter()
            .map(|p| format!("{} {}", p["name"].as_str().unwrap(), p["version"].as_str().unwrap()))
            .collect();
        assert_eq!(
            key(&third_party().packages),
            want,
            "assets/third-party-licenses.json is out of date: run `GHOSTDESK_UPDATE_LICENSES=1 cargo test third_party_list_is_current`"
        );
    }
}
