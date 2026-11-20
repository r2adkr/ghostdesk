//! Skills: folders with a `SKILL.md` that teach an agent a kind of work. They are imported (copied) into
//! GhostDesk's own folder, so what an agent reads can't change behind the person's back, and switched on or off
//! here. Only key-based agents use them.
use std::path::{Path, PathBuf};

const MAX_FILES: usize = 400;
const MAX_BYTES: u64 = 8 * 1024 * 1024;

pub fn dir() -> PathBuf {
    crate::platform::paths::data_root().join("skills")
}

#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub enabled: bool,
}

/// What a folder offers to import.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `name` and `description` from the frontmatter of a SKILL.md.
pub fn frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let mut name = None;
    let mut desc: Option<String> = None;
    let mut in_desc = false;
    for l in lines {
        if l.trim() == "---" {
            break;
        }
        if in_desc && (l.starts_with(' ') || l.starts_with('\t')) {
            let d = desc.get_or_insert_with(String::new);
            if !d.is_empty() {
                d.push(' ');
            }
            d.push_str(l.trim());
            continue;
        }
        in_desc = false;
        let unquote = |v: &str| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        if let Some(v) = l.strip_prefix("name:") {
            name = Some(unquote(v));
        } else if let Some(v) = l.strip_prefix("description:") {
            let v = v.trim();
            if matches!(v, ">" | "|" | ">-" | "|-" | "") {
                in_desc = true;
                desc = Some(String::new());
            } else {
                desc = Some(unquote(v));
            }
        }
    }
    (name.filter(|n| !n.is_empty()), desc.filter(|d| !d.is_empty()))
}

fn read_skill(folder: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(folder.join("SKILL.md")).ok()?;
    let (name, desc) = frontmatter(&text);
    let name = name.or_else(|| folder.file_name().map(|n| n.to_string_lossy().to_string()))?;
    Some((name, desc?))
}

fn subdirs(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

pub fn list() -> Vec<Skill> {
    subdirs(&dir())
        .into_iter()
        .filter_map(|p| {
            let (name, description) = read_skill(&p)?;
            let enabled = !p.join(".disabled").exists();
            Some(Skill { name, description, path: p, enabled })
        })
        .collect()
}

pub fn active() -> Vec<PathBuf> {
    list().into_iter().filter(|s| s.enabled).map(|s| s.path).collect()
}

pub fn set_enabled(skill: &Skill, on: bool) -> std::io::Result<()> {
    let marker = skill.path.join(".disabled");
    if on {
        match std::fs::remove_file(&marker) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    } else {
        std::fs::write(marker, b"")
    }
}

/// Removes an imported skill (only ever a folder directly inside GhostDesk's skills folder).
pub fn remove(skill: &Skill) -> std::io::Result<()> {
    if skill.path.parent() != Some(dir().as_path()) {
        return Err(std::io::Error::other("not an imported skill"));
    }
    std::fs::remove_dir_all(&skill.path)
}

/// Skills found in other places on this computer that could be imported.
pub fn candidates() -> Vec<Candidate> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    let have: Vec<String> = list().into_iter().map(|s| s.name).collect();
    [".claude/skills", ".agents/skills"]
        .iter()
        .flat_map(|r| subdirs(&home.join(r)))
        .filter_map(|p| {
            let (name, description) = read_skill(&p)?;
            (valid_name(&name) && !have.contains(&name)).then_some(Candidate { name, description, path: p })
        })
        .collect()
}

/// Looks inside a folder before importing: its skill and how big it is.
pub fn inspect(src: &Path) -> Result<(Candidate, usize, u64), String> {
    let (name, description) = read_skill(src).ok_or("이 폴더에는 SKILL.md 가 없거나 설명이 없어요")?;
    if !valid_name(&name) {
        return Err("스킬 이름은 영문 소문자·숫자·- 만 쓸 수 있어요".into());
    }
    let (mut files, mut bytes) = (0usize, 0u64);
    walk(src, &mut |_, len| {
        files += 1;
        bytes += len;
    })?;
    if files > MAX_FILES || bytes > MAX_BYTES {
        return Err("스킬이 너무 커요 (파일 400개, 8MB 까지)".into());
    }
    Ok((Candidate { name, description, path: src.to_path_buf() }, files, bytes))
}

/// Visits regular files below `root`; symlinks and `.git` are ignored.
fn walk(root: &Path, f: &mut dyn FnMut(&Path, u64)) -> Result<(), String> {
    let mut stack = vec![root.to_path_buf()];
    let mut seen = 0usize;
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).map_err(|e| e.to_string())?.flatten() {
            seen += 1;
            if seen > MAX_FILES * 4 {
                return Err("스킬이 너무 커요 (파일 400개, 8MB 까지)".into());
            }
            let Ok(t) = e.file_type() else { continue };
            if t.is_symlink() || e.file_name() == ".git" {
                continue;
            }
            if t.is_dir() {
                stack.push(e.path());
            } else if t.is_file() {
                f(&e.path(), e.metadata().map(|m| m.len()).unwrap_or(0));
            }
        }
    }
    Ok(())
}

/// Copies a skill folder into GhostDesk's skills folder under its name.
pub fn import(src: &Path) -> Result<String, String> {
    let (c, _, _) = inspect(src)?;
    let dest = dir().join(&c.name);
    if dest.exists() {
        return Err("같은 이름의 스킬이 이미 있어요".into());
    }
    let tmp = dir().join(format!(".import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut err: Option<String> = None;
    let root = src.to_path_buf();
    let result = walk(src, &mut |p, _| {
        if err.is_some() {
            return;
        }
        let Ok(rel) = p.strip_prefix(&root) else { return };
        let to = tmp.join(rel);
        if let Some(parent) = to.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                err = Some(e.to_string());
                return;
            }
        }
        if let Err(e) = std::fs::copy(p, &to) {
            err = Some(e.to_string());
        }
    });
    if let Err(e) = result.and_then(|_| err.map_or(Ok(()), Err)) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, &dest).map_err(|e| {
        let _ = std::fs::remove_dir_all(&tmp);
        e.to_string()
    })?;
    Ok(c.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_reads_plain_quoted_and_folded_values() {
        let t = "---\nname: pdf-tools\ndescription: \"Extract text. Use for PDFs.\"\n---\n# x";
        assert_eq!(frontmatter(t), (Some("pdf-tools".into()), Some("Extract text. Use for PDFs.".into())));
        let t = "---\nname: a\ndescription: >\n  first line\n  second line\nlicense: MIT\n---\n";
        assert_eq!(frontmatter(t).1.as_deref(), Some("first line second line"));
        assert_eq!(frontmatter("no frontmatter"), (None, None));
    }

    #[test]
    fn names_follow_the_skill_rules() {
        assert!(valid_name("pdf-tools2"));
        for bad in ["", "-a", "a-", "a--b", "Abc", "a_b", "a b", "../x"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn import_copies_files_skips_symlinks_and_refuses_duplicates() {
        let root = std::env::temp_dir().join(format!("gd-skill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src").join("demo-skill");
        std::fs::create_dir_all(src.join("scripts")).unwrap();
        std::fs::write(src.join("SKILL.md"), "---\nname: demo-skill\ndescription: Does a demo.\n---\nbody").unwrap();
        std::fs::write(src.join("scripts/run.sh"), "echo hi").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hosts", src.join("link")).unwrap();
        let (c, files, _) = inspect(&src).unwrap();
        assert_eq!((c.name.as_str(), files), ("demo-skill", 2));
        // import into a scratch skills folder through the same code path
        let dest = root.join("skills").join("demo-skill");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        let tmp = root.join("skills").join(".t");
        walk(&src, &mut |p, _| {
            let to = tmp.join(p.strip_prefix(&src).unwrap());
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::copy(p, to).unwrap();
        })
        .unwrap();
        assert!(tmp.join("scripts/run.sh").exists() && !tmp.join("link").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
