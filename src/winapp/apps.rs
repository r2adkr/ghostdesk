//! Finding installed apps: Start menu shortcuts (.lnk) and the App Paths registry key.
//!
//! .lnk files are parsed directly as MS-SHLLINK. Not going through COM's IShellLinkW means MSI
//! "advertised" shortcuts never trigger an accidental install or repair. The target is taken
//! from the environment variable block (%windir%\…), then LinkInfo's local path, then the
//! relative path.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::platform::paths;
use crate::platform::win as pw;

// ───────────────────────── .lnk ─────────────────────────

const HAS_ID_LIST: u32 = 0x1;
const HAS_LINK_INFO: u32 = 0x2;
const HAS_NAME: u32 = 0x4;
const HAS_RELATIVE_PATH: u32 = 0x8;
const HAS_WORKING_DIR: u32 = 0x10;
const HAS_ARGUMENTS: u32 = 0x20;
const HAS_ICON_LOCATION: u32 = 0x40;
const IS_UNICODE: u32 = 0x80;
const HAS_DARWIN_ID: u32 = 0x1000;

/// Information read from a .lnk.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lnk {
    /// LinkInfo's local path (base path + suffix).
    pub local_path: Option<String>,
    /// Target from the environment variable block (e.g. %windir%\system32\charmap.exe, unexpanded).
    pub env_target: Option<String>,
    pub relative_path: Option<String>,
    pub working_dir: Option<String>,
    pub arguments: Option<String>,
    /// MSI "advertised" shortcut (launching it may run an install or repair).
    pub darwin: bool,
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// NUL-terminated ANSI string (`None` if not ASCII, since the code page is unknown).
fn ansi_z(b: &[u8], o: usize) -> Option<String> {
    let rest = b.get(o..)?;
    let n = rest.iter().position(|&c| c == 0)?;
    let s = &rest[..n];
    if s.is_ascii() {
        Some(String::from_utf8_lossy(s).to_string())
    } else {
        None
    }
}

fn utf16_z(b: &[u8], o: usize) -> Option<String> {
    let mut v = Vec::new();
    let mut i = o;
    loop {
        let c = u16_at(b, i)?;
        if c == 0 {
            break;
        }
        v.push(c);
        i += 2;
        if v.len() > 4096 {
            return None;
        }
    }
    Some(String::from_utf16_lossy(&v))
}

/// Parses MS-SHLLINK bytes.
pub fn parse_lnk(b: &[u8]) -> Option<Lnk> {
    if u32_at(b, 0)? != 0x4C || b.get(4..20)? != [0x01, 0x14, 0x02, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46] {
        return None;
    }
    let flags = u32_at(b, 0x14)?;
    let mut out = Lnk { darwin: flags & HAS_DARWIN_ID != 0, ..Default::default() };
    let mut o = 0x4C;
    if flags & HAS_ID_LIST != 0 {
        o += 2 + u16_at(b, o)? as usize;
    }
    if flags & HAS_LINK_INFO != 0 {
        let li = o;
        let size = u32_at(b, li)? as usize;
        let hdr = u32_at(b, li + 4)? as usize;
        let li_flags = u32_at(b, li + 8)?;
        if li_flags & 1 != 0 {
            // 0x10 LocalBasePathOffset, 0x18 CommonPathSuffixOffset, (header ≥ 0x24) 0x1C/0x20 Unicode variants
            let base_off = u32_at(b, li + 0x10)? as usize;
            let suffix_off = u32_at(b, li + 0x18)? as usize;
            let (base, suffix) = if hdr >= 0x24 {
                let bu = u32_at(b, li + 0x1C)? as usize;
                let su = u32_at(b, li + 0x20)? as usize;
                (
                    if bu != 0 { utf16_z(b, li + bu) } else { ansi_z(b, li + base_off) },
                    if su != 0 { utf16_z(b, li + su) } else { ansi_z(b, li + suffix_off) },
                )
            } else {
                (ansi_z(b, li + base_off), ansi_z(b, li + suffix_off))
            };
            if let Some(base) = base {
                let suffix = suffix.unwrap_or_default();
                let joined = if suffix.is_empty() || base.ends_with('\\') { format!("{base}{suffix}") } else { format!("{base}\\{suffix}") };
                out.local_path = Some(joined);
            }
        }
        o = li + size;
    }
    let unicode = flags & IS_UNICODE != 0;
    let read_str = |o: &mut usize| -> Option<String> {
        let n = u16_at(b, *o)? as usize;
        *o += 2;
        let s = if unicode {
            let v: Vec<u16> = (0..n).filter_map(|i| u16_at(b, *o + i * 2)).collect();
            *o += n * 2;
            String::from_utf16_lossy(&v)
        } else {
            let s = b.get(*o..*o + n).map(|s| String::from_utf8_lossy(s).to_string()).unwrap_or_default();
            *o += n;
            s
        };
        Some(s)
    };
    if flags & HAS_NAME != 0 {
        read_str(&mut o)?;
    }
    if flags & HAS_RELATIVE_PATH != 0 {
        out.relative_path = read_str(&mut o).filter(|s| !s.is_empty());
    }
    if flags & HAS_WORKING_DIR != 0 {
        out.working_dir = read_str(&mut o).filter(|s| !s.is_empty());
    }
    if flags & HAS_ARGUMENTS != 0 {
        out.arguments = read_str(&mut o).filter(|s| !s.is_empty());
    }
    if flags & HAS_ICON_LOCATION != 0 {
        read_str(&mut o)?;
    }
    // ExtraData blocks
    while let Some(size) = u32_at(b, o) {
        let size = size as usize;
        if size < 8 {
            break;
        }
        let sig = u32_at(b, o + 4).unwrap_or(0);
        if sig == 0xA000_0001 && size >= 0x314 {
            // EnvironmentVariableDataBlock: TargetAnsi[260] followed by TargetUnicode[520 bytes]
            out.env_target = utf16_z(b, o + 8 + 260).filter(|s| !s.is_empty()).or_else(|| ansi_z(b, o + 8).filter(|s| !s.is_empty()));
        } else if sig == 0xA000_0006 {
            out.darwin = true;
        }
        o += size;
    }
    Some(out)
}

/// Expands %VAR%.
pub fn expand_env(s: &str) -> String {
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    if !s.contains('%') {
        return s.to_string();
    }
    let w = pw::wide(s);
    unsafe {
        let n = ExpandEnvironmentStringsW(w.as_ptr(), std::ptr::null_mut(), 0);
        if n == 0 {
            return s.to_string();
        }
        let mut buf = vec![0u16; n as usize + 1];
        let m = ExpandEnvironmentStringsW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32);
        if m == 0 {
            return s.to_string();
        }
        pw::from_wide_buf(&buf)
    }
}

/// The shortcut's target executable (only if it exists).
pub fn lnk_target(lnk_path: &Path) -> Option<PathBuf> {
    let bytes = std::fs::read(lnk_path).ok()?;
    let l = parse_lnk(&bytes)?;
    if l.darwin && l.env_target.is_none() && l.local_path.is_none() {
        return None;
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(e) = &l.env_target {
        cands.push(PathBuf::from(expand_env(e)));
    }
    if let Some(p) = &l.local_path {
        cands.push(PathBuf::from(p));
    }
    if let (Some(r), Some(dir)) = (&l.relative_path, lnk_path.parent()) {
        cands.push(dir.join(r));
    }
    // don't even touch network or device paths (\\server\share, \\?\UNC\…, \\.\…): is_file() alone
    // sends SMB auth (the user's NTLM credentials) to that server, and offline shares stall for
    // seconds. Such targets are never opened anyway.
    cands
        .into_iter()
        .filter(|p| !is_network_or_device(p))
        .find(|p| p.is_file())
        .map(|p| paths::canonicalize(&p).unwrap_or(p))
}

/// Whether a path is not on a local drive: `\\server\share`, `//server`, `\\?\UNC\…`,
/// `\\?\GLOBALROOT\…`, `\\.\…`, `\??\…` (`\\?\C:\…` is local).
///
/// Decided from the string alone, without touching the file system.
fn is_network_or_device(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let s = s.trim().trim_matches('"').trim().replace('/', "\\");
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        return !(b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\');
    }
    s.starts_with(r"\\") || s.starts_with(r"\??\")
}

// ---- installed apps ----

fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use windows_sys::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    unsafe {
        let mut p: *mut u16 = std::ptr::null_mut();
        if SHGetKnownFolderPath(id, KF_FLAG_DEFAULT as _, std::ptr::null_mut(), &mut p) != 0 || p.is_null() {
            return None;
        }
        let s = pw::from_wide_ptr(p);
        windows_sys::Win32::System::Com::CoTaskMemFree(p as *const _);
        Some(PathBuf::from(s))
    }
}

fn start_menu_dirs() -> Vec<PathBuf> {
    use windows_sys::Win32::UI::Shell::{FOLDERID_CommonPrograms, FOLDERID_Programs};
    let mut v: Vec<PathBuf> = [known_folder(&FOLDERID_Programs), known_folder(&FOLDERID_CommonPrograms)].into_iter().flatten().collect();
    if v.is_empty() {
        if let Some(a) = std::env::var_os("APPDATA") {
            v.push(PathBuf::from(a).join(r"Microsoft\Windows\Start Menu\Programs"));
        }
        if let Some(a) = std::env::var_os("ProgramData") {
            v.push(PathBuf::from(a).join(r"Microsoft\Windows\Start Menu\Programs"));
        }
    }
    v
}

fn walk_lnk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() && depth < 5 {
            walk_lnk(&p, depth + 1, out);
        } else if ft.is_file() && p.extension().map(|x| x.eq_ignore_ascii_case("lnk")).unwrap_or(false) {
            out.push(p);
        }
    }
}

const SKIP_NAME_WORDS: [&str; 22] = [
    "uninstall", "uninst", "제거", "삭제", "readme", "read me", "help", "도움말", "manual", "설명서", "website", "web site",
    "홈페이지", "license", "라이선스", "release notes", "changelog", "documentation", "what's new", "support", "고객 지원", "온라인",
];

/// The Windows shell plus system launchers and admin tools that only start other programs (no
/// window to drive, a shell window like the taskbar or desktop, or system-wide effects).
///
/// Left out of the app list and refused by the pre-launch check ([`super::launch::preflight`]).
pub const SHELL_LAUNCHERS: [&str; 13] = [
    "hh.exe", "explorer.exe", "control.exe", "mmc.exe", "rundll32.exe", "msiexec.exe", "wscript.exe", "cscript.exe", "mshta.exe",
    "regedit.exe", "taskmgr.exe", "msconfig.exe", "systemsettingsadminflows.exe",
];

/// Shortcuts to leave out of the app list (uninstallers, help, web sites, …).
pub fn skip_shortcut(name: &str, target: &Path) -> bool {
    let n = name.to_lowercase();
    if SKIP_NAME_WORDS.iter().any(|w| n.contains(w)) {
        return true;
    }
    let t = target.file_name().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    t.starts_with("unins") || t.starts_with("uninstall") || SHELL_LAUNCHERS.contains(&t.as_str()) || t.ends_with(".url")
}

fn is_gui_exe(p: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(p) else { return false };
    let mut buf = vec![0u8; 4096];
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    super::launch::pe_subsystem(&buf) == Some(2)
}

fn scan_installed() -> Vec<(String, String)> {
    let mut lnks = Vec::new();
    for d in start_menu_dirs() {
        walk_lnk(&d, 0, &mut lnks);
    }
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for l in lnks {
        let name = l.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let Some(t) = lnk_target(&l) else { continue };
        if !t.extension().map(|x| x.eq_ignore_ascii_case("exe")).unwrap_or(false) || skip_shortcut(&name, &t) {
            continue;
        }
        let key = t.to_string_lossy().to_lowercase();
        if seen.contains(&key) || !is_gui_exe(&t) {
            continue;
        }
        seen.insert(key);
        out.push((name, t));
    }
    // App Paths (apps not in the Start menu)
    let base = r"Software\Microsoft\Windows\CurrentVersion\App Paths";
    for root in [pw::HKEY_CURRENT_USER, pw::HKEY_LOCAL_MACHINE] {
        for k in pw::reg_subkeys(root, base) {
            if !k.to_lowercase().ends_with(".exe") {
                continue;
            }
            let Some(v) = pw::reg_string(root, &format!(r"{base}\{k}"), None) else { continue };
            let p = PathBuf::from(expand_env(v.trim().trim_matches('"')));
            if !p.is_file() {
                continue;
            }
            let key = p.to_string_lossy().to_lowercase();
            if seen.contains(&key) || !is_gui_exe(&p) || p.to_string_lossy().to_lowercase().contains("\\windowsapps\\") {
                continue;
            }
            seen.insert(key);
            let name = super::launch::display_name(&p);
            if skip_shortcut(&name, &p) {
                continue;
            }
            out.push((name, p));
        }
    }
    out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    out.into_iter().map(|(n, p)| (n, p.to_string_lossy().to_string())).collect()
}

/// Installed apps `(display name, executable path)`, cached for a minute.
pub fn installed_apps() -> Vec<(String, String)> {
    static CACHE: Mutex<Option<(Instant, Vec<(String, String)>)>> = Mutex::new(None);
    if let Some((t, v)) = CACHE.lock().as_ref() {
        if t.elapsed() < Duration::from_secs(60) {
            return v.clone();
        }
    }
    let v = scan_installed();
    *CACHE.lock() = Some((Instant::now(), v.clone()));
    v
}

fn search_path(exe: &str) -> Option<PathBuf> {
    let win = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let mut dirs = vec![win.join("System32"), win.clone()];
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    dirs.into_iter().map(|d| d.join(exe)).find(|p| p.is_file())
}

/// Resolves what the user typed (path, .lnk, display name, executable name) to an executable
/// path.
pub fn resolve_app_query(query: &str) -> Option<PathBuf> {
    let q = query.trim().trim_matches('"').trim();
    if q.is_empty() {
        return None;
    }
    // an absolute path to an existing file is used as is: expanding it again could turn a path
    // that literally contains '%NAME%' (the file checked and approved) into a different file.
    // Expansion is only for paths a person typed, like '%SystemRoot%\…'.
    let raw = Path::new(q);
    let expanded = if raw.is_absolute() && raw.is_file() { q.to_string() } else { expand_env(q) };
    let p = PathBuf::from(&expanded);
    if expanded.contains(['\\', '/']) || p.is_absolute() {
        if !p.is_file() {
            return None;
        }
        let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        return match ext.as_str() {
            "lnk" => lnk_target(&p),
            "exe" => Some(paths::canonicalize(&p).unwrap_or(p)),
            _ => None,
        };
    }
    let apps = installed_apps();
    let lq = q.to_lowercase();
    if let Some((_, path)) = apps.iter().find(|(n, _)| n.to_lowercase() == lq) {
        return Some(PathBuf::from(path));
    }
    let exe = if lq.ends_with(".exe") { q.to_string() } else { format!("{q}.exe") };
    if let Some(p) = pw::app_path(&exe).or_else(|| search_path(&exe)) {
        return Some(paths::canonicalize(&p).unwrap_or(p));
    }
    let stem = lq.trim_end_matches(".exe");
    if let Some((_, path)) = apps.iter().find(|(_, p)| Path::new(p).file_stem().map(|s| s.to_string_lossy().to_lowercase() == stem).unwrap_or(false)) {
        return Some(PathBuf::from(path));
    }
    apps.iter().find(|(n, _)| n.to_lowercase().contains(&lq)).map(|(_, p)| PathBuf::from(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal .lnk: LinkInfo (local path) + relative path + arguments (+ environment
    /// variable block).
    fn build_lnk(local: &str, rel: &str, args: &str, env: Option<&str>) -> Vec<u8> {
        let mut b = vec![0u8; 0x4C];
        b[0..4].copy_from_slice(&0x4Cu32.to_le_bytes());
        b[4..20].copy_from_slice(&[0x01, 0x14, 0x02, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
        let flags = HAS_LINK_INFO | HAS_RELATIVE_PATH | HAS_ARGUMENTS | IS_UNICODE;
        b[0x14..0x18].copy_from_slice(&flags.to_le_bytes());
        // LinkInfo: 0x1C header, minimal volume id, local base path (ANSI), empty suffix
        let mut li = vec![0u8; 0x1C];
        let vol = [0x10u8, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0];
        let vol_off = li.len();
        li.extend_from_slice(&vol);
        let base_off = li.len();
        li.extend_from_slice(local.as_bytes());
        li.push(0);
        let suf_off = li.len();
        li.push(0);
        let size = li.len() as u32;
        li[0..4].copy_from_slice(&size.to_le_bytes());
        li[4..8].copy_from_slice(&0x1Cu32.to_le_bytes());
        li[8..12].copy_from_slice(&1u32.to_le_bytes());
        li[12..16].copy_from_slice(&(vol_off as u32).to_le_bytes());
        li[16..20].copy_from_slice(&(base_off as u32).to_le_bytes());
        li[24..28].copy_from_slice(&(suf_off as u32).to_le_bytes());
        b.extend_from_slice(&li);
        for s in [rel, args] {
            let w: Vec<u16> = s.encode_utf16().collect();
            b.extend_from_slice(&(w.len() as u16).to_le_bytes());
            for c in w {
                b.extend_from_slice(&c.to_le_bytes());
            }
        }
        if let Some(e) = env {
            let mut blk = vec![0u8; 0x314];
            blk[0..4].copy_from_slice(&0x314u32.to_le_bytes());
            blk[4..8].copy_from_slice(&0xA000_0001u32.to_le_bytes());
            for (i, c) in e.encode_utf16().enumerate() {
                blk[8 + 260 + i * 2..8 + 260 + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
            }
            b.extend_from_slice(&blk);
        }
        b.extend_from_slice(&0u32.to_le_bytes());
        b
    }

    #[test]
    fn lnk_parsing() {
        let b = build_lnk(r"C:\Windows\System32\charmap.exe", r"..\..\charmap.exe", "-x \"a b\"", None);
        let l = parse_lnk(&b).unwrap();
        assert_eq!(l.local_path.as_deref(), Some(r"C:\Windows\System32\charmap.exe"));
        assert_eq!(l.relative_path.as_deref(), Some(r"..\..\charmap.exe"));
        assert_eq!(l.arguments.as_deref(), Some("-x \"a b\""));
        assert!(!l.darwin);
        let b = build_lnk(r"C:\x\y.exe", "", "", Some(r"%windir%\system32\charmap.exe"));
        let l = parse_lnk(&b).unwrap();
        assert_eq!(l.env_target.as_deref(), Some(r"%windir%\system32\charmap.exe"));
        assert!(parse_lnk(b"garbage").is_none());
        assert!(expand_env("%SystemRoot%").to_lowercase().contains("windows"));
    }

    #[test]
    fn lnk_files_on_this_pc() {
        // Start menu shortcuts on this PC whose target exists must resolve to an executable path
        let mut lnks = Vec::new();
        for d in start_menu_dirs() {
            walk_lnk(&d, 0, &mut lnks);
        }
        let resolved = lnks.iter().filter_map(|l| lnk_target(l)).count();
        if !lnks.is_empty() {
            assert!(resolved > 0, "{} 개 중 하나도 못 풂", lnks.len());
        }
    }

    #[test]
    fn skip_rules_and_resolve() {
        assert!(skip_shortcut("Uninstall Foo", Path::new(r"C:\foo\x.exe")));
        assert!(skip_shortcut("Foo", Path::new(r"C:\foo\unins000.exe")));
        assert!(skip_shortcut("Foo 도움말", Path::new(r"C:\foo\foo.exe")));
        assert!(!skip_shortcut("문자표", Path::new(r"C:\Windows\System32\charmap.exe")));
        let sys = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()));
        if sys.join(r"System32\charmap.exe").is_file() {
            let p = resolve_app_query("charmap").unwrap();
            assert!(p.to_string_lossy().to_lowercase().ends_with(r"system32\charmap.exe"), "{p:?}");
            let p = resolve_app_query(r"%SystemRoot%\System32\charmap.exe").unwrap();
            assert!(p.is_file());
        }
        assert!(resolve_app_query("").is_none());
        assert!(resolve_app_query(r"C:\no\such\app.exe").is_none());
        // shell and system launchers are left out of the list
        assert!(skip_shortcut("파일 탐색기", Path::new(r"C:\Windows\explorer.exe")));
        assert!(skip_shortcut("Script", Path::new(r"C:\Windows\System32\cscript.exe")));
    }

    #[test]
    fn existing_absolute_path_is_not_env_expanded() {
        // an executable in a folder literally named '%USERNAME%' stays that file (not a same-named file
        // in the user's folder)
        let user = std::env::var("USERNAME").unwrap_or_default();
        if user.is_empty() {
            return;
        }
        let root = std::env::temp_dir().join(format!("ghostdesk-resolve-{}", std::process::id()));
        let literal = root.join("%USERNAME%");
        let other = root.join(&user);
        std::fs::create_dir_all(&literal).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(literal.join("tool.exe"), b"MZ").unwrap();
        std::fs::write(other.join("tool.exe"), b"MZ").unwrap();
        let q = literal.join("tool.exe");
        let got = resolve_app_query(&q.to_string_lossy()).unwrap();
        assert!(got.to_string_lossy().contains("%USERNAME%"), "{got:?}");
        assert!(paths::same_path(&got, &q), "{got:?}");
        // same with quotes around it
        let got = resolve_app_query(&format!("\"{}\"", q.display())).unwrap();
        assert!(paths::same_path(&got, &q), "{got:?}");
        // if no such file exists, expand as before (a '%VAR%\…' path a person typed)
        let q2 = root.join("%USERNAME%").join("only-in-user.exe");
        std::fs::write(other.join("only-in-user.exe"), b"MZ").unwrap();
        let got = resolve_app_query(&q2.to_string_lossy()).unwrap();
        assert!(paths::same_path(&got, &other.join("only-in-user.exe")), "{got:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn network_targets_are_never_touched() {
        for p in [
            r"\\server\share\app.exe",
            "//server/share/app.exe",
            r"\\?\UNC\server\share\app.exe",
            r"\\?\GLOBALROOT\Device\Mup\server\x.exe",
            r"\\.\pipe\x",
            r"\??\UNC\server\x.exe",
            r#""\\server\share\app.exe""#,
        ] {
            assert!(is_network_or_device(Path::new(p)), "{p}");
        }
        for p in [r"C:\Windows\System32\charmap.exe", r"\\?\C:\Windows\System32\charmap.exe", r"D:\apps\x.exe"] {
            assert!(!is_network_or_device(Path::new(p)), "{p}");
        }
    }
}
