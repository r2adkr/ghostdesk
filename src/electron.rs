//! Electron app clone backend.
//!
//! Runs an installed Electron app (VS Code, …) separately in a "hidden mode" that never shows a
//! window on the user's screen, and handles screen capture, mouse and keyboard entirely through
//! CDP (Chrome DevTools Protocol). The OS cursor, keyboard focus and the user's app windows are
//! never touched.
//!
//! How hiding works: launching the app executable directly with `--inspect-brk=0` pauses the main
//! process on the first line of the app's main script. We attach with the Node inspector and patch
//! the electron module before any app code runs (activation policy prohibited; window
//! show/focus/resize/opacity changes neutralized; native dialogs, external links, notifications,
//! tray, global shortcuts, relaunch and devtools blocked). Windows become alpha 0, click-through
//! and unfocusable the moment they're created (BaseWindow._init, before show is handled), and are
//! only shown inactive at off-screen coordinates (-30000,-30000; macOS clamps to about 40px past
//! the left edge). A full hide() makes Chromium stop rendering so Page.captureScreenshot never
//! answers (measured), hence "shown but transparent" (ghost) is the default. (The environment
//! variable GHOSTDESK_ELECTRON_MODE=hide is experimental: the window isn't even in the onscreen
//! list, but screenshots don't work.) This only works for apps with the
//! `EnableNodeCliInspectArguments` fuse on. Apps with it off (Discord, Claude, Unity Hub, …) can't
//! be hidden, so they aren't supported.
//!
//! Safeguard: a watchdog checks CGWindowList and WindowServer's front process every 250 ms. If a
//! window of our process group becomes visible (alpha>0, onscreen) or comes to the front, it's
//! re-hidden through the inspector right away, and if that lasts over a second the whole process
//! group is killed.
//!
//! Windows: same principle (inspector injection + CDP). The app runs inside a Job object (suspend
//! → assign to Job → resume, so descendants are cleaned up together), and windows sit outside
//! every monitor (DIP coordinates accounting for DPI) at alpha 0, click-through, non-activating
//! (WS_EX_NOACTIVATE) and off the taskbar. Hidden instances use their own AppUserModelID so they
//! don't touch the taskbar group or jump list of the user's real app. The watchdog checks
//! top-level windows of the Job's processes (visible, DWM-cloaked, opacity, monitor overlap,
//! taskbar exposure) and the foreground window.
#![allow(dead_code)]

#[cfg(any(target_os = "macos", windows))]
use std::sync::atomic::AtomicI32;
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, ensure, Context, Result};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use crate::{
    browser::{BrowserWorkspace, ConnectOptions, FrameSink, MouseButton},
    cdp::Cdp,
    profiles::{self, CloneStats},
};

// ---- fuses (Electron Fuses) ----

/// Electron fuse sentinel. Followed by a version byte, a length byte, then '0'/'1'/'r' per fuse.
pub const FUSE_SENTINEL: &[u8] = b"dL7pKGdnNz796PbbjQWNKmHXBZaB9tsX";

/// Fuse order (as defined in Electron's fuses.json5).
pub const FUSE_NAMES: [&str; 9] = [
    "RunAsNode",
    "EnableCookieEncryption",
    "EnableNodeOptionsEnvironmentVariable",
    "EnableNodeCliInspectArguments",
    "EnableEmbeddedAsarIntegrityValidation",
    "OnlyLoadAppFromAsar",
    "LoadBrowserProcessSpecificV8Snapshot",
    "GrantFileProtocolExtraPrivileges",
    "WasmTrapHandlers",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FuseState {
    Disabled,
    Enabled,
    Removed,
    Unknown(u8),
}

impl FuseState {
    fn from_byte(b: u8) -> Self {
        match b {
            b'0' => FuseState::Disabled,
            b'1' => FuseState::Enabled,
            b'r' => FuseState::Removed,
            x => FuseState::Unknown(x),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ElectronFuses {
    pub version: u8,
    /// Raw fuse string (e.g. "101100011").
    pub wire: String,
    pub fuses: Vec<(String, FuseState)>,
}

impl ElectronFuses {
    pub fn get(&self, name: &str) -> Option<FuseState> {
        self.fuses.iter().find(|(n, _)| n == name).map(|(_, s)| *s)
    }

    pub fn is_on(&self, name: &str) -> bool {
        self.get(name) == Some(FuseState::Enabled)
    }

    /// Whether Node inspector arguments (--inspect-brk) are accepted = whether hidden mode is possible.
    pub fn inspect_allowed(&self) -> bool {
        self.is_on("EnableNodeCliInspectArguments")
    }

    pub fn summary(&self) -> String {
        self.fuses
            .iter()
            .map(|(n, s)| {
                let v = match s {
                    FuseState::Enabled => "ON".to_string(),
                    FuseState::Disabled => "off".to_string(),
                    FuseState::Removed => "removed".to_string(),
                    FuseState::Unknown(b) => format!("?{b}"),
                };
                format!("{n}={v}")
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Parses the first fuse block in a byte buffer.
pub fn parse_fuses(buf: &[u8]) -> Option<ElectronFuses> {
    let off = find_sentinel(buf)?;
    parse_fuse_block(&buf[off + FUSE_SENTINEL.len()..])
}

/// First position of the sentinel in the buffer. Boyer–Moore–Horspool skips about 32 bytes at a
/// time on average, so even Windows executables of several hundred MB scan quickly (debug builds
/// included). Same result as a naive search.
fn find_sentinel(hay: &[u8]) -> Option<usize> {
    let pat = FUSE_SENTINEL;
    let n = pat.len();
    if hay.len() < n {
        return None;
    }
    let mut skip = [n; 256];
    for (i, &b) in pat[..n - 1].iter().enumerate() {
        skip[b as usize] = n - 1 - i;
    }
    let mut i = 0;
    while i + n <= hay.len() {
        let last = hay[i + n - 1];
        if last == pat[n - 1] && &hay[i..i + n] == pat {
            return Some(i);
        }
        i += skip[last as usize];
    }
    None
}

/// Parses the [version][length][fuses...] block right after the sentinel.
fn parse_fuse_block(b: &[u8]) -> Option<ElectronFuses> {
    let version = *b.first()?;
    let len = *b.get(1)? as usize;
    let wire = b.get(2..2 + len)?;
    let fuses = wire
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let name = FUSE_NAMES.get(i).map(|s| s.to_string()).unwrap_or_else(|| format!("fuse{i}"));
            (name, FuseState::from_byte(*c))
        })
        .collect();
    Some(ElectronFuses { version, wire: String::from_utf8_lossy(wire).into_owned(), fuses })
}

/// Mach-O slice (offset, size) for the current architecture (arm64/x86_64). The whole file if not
/// fat.
fn macho_slice(f: &mut fs::File) -> Result<(u64, u64)> {
    let len = f.metadata()?.len();
    let mut hdr = [0u8; 8];
    f.seek(SeekFrom::Start(0))?;
    f.read_exact(&mut hdr)?;
    let magic = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    if magic != 0xCAFE_BABE && magic != 0xCAFE_BABF {
        return Ok((0, len));
    }
    let n = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
    let want: u32 = if cfg!(target_arch = "aarch64") { 0x0100_000C } else { 0x0100_0007 };
    let entry = if magic == 0xCAFE_BABE { 20 } else { 32 };
    let mut tbl = vec![0u8; n * entry];
    f.read_exact(&mut tbl)?;
    for k in 0..n {
        let e = &tbl[k * entry..(k + 1) * entry];
        let ct = u32::from_be_bytes([e[0], e[1], e[2], e[3]]);
        let (off, size) = if magic == 0xCAFE_BABE {
            (
                u32::from_be_bytes([e[8], e[9], e[10], e[11]]) as u64,
                u32::from_be_bytes([e[12], e[13], e[14], e[15]]) as u64,
            )
        } else {
            (
                u64::from_be_bytes(e[8..16].try_into().unwrap()),
                u64::from_be_bytes(e[16..24].try_into().unwrap()),
            )
        };
        if ct == want {
            return Ok((off, size));
        }
    }
    Ok((0, len))
}

/// Finds the (__TEXT,__const) section range within the slice (where the sentinel usually lives).
fn macho_text_const(f: &mut fs::File, slice_off: u64) -> Option<(u64, u64)> {
    let mut h = [0u8; 32];
    f.seek(SeekFrom::Start(slice_off)).ok()?;
    f.read_exact(&mut h).ok()?;
    if u32::from_le_bytes(h[0..4].try_into().ok()?) != 0xFEED_FACF {
        return None;
    }
    let ncmds = u32::from_le_bytes(h[16..20].try_into().ok()?) as usize;
    let sizeofcmds = u32::from_le_bytes(h[20..24].try_into().ok()?) as usize;
    let mut cmds = vec![0u8; sizeofcmds.min(4 << 20)];
    f.read_exact(&mut cmds).ok()?;
    let mut p = 0usize;
    for _ in 0..ncmds {
        let cmd = u32::from_le_bytes(cmds.get(p..p + 4)?.try_into().ok()?);
        let size = u32::from_le_bytes(cmds.get(p + 4..p + 8)?.try_into().ok()?) as usize;
        if cmd == 0x19 {
            // LC_SEGMENT_64: segname(16) ... nsects at +64, sections from +72 (80 bytes each)
            let nsects = u32::from_le_bytes(cmds.get(p + 64..p + 68)?.try_into().ok()?) as usize;
            for s in 0..nsects {
                let b = p + 72 + s * 80;
                let sect = cmds.get(b..b + 80)?;
                let sname = String::from_utf8_lossy(&sect[0..16]).trim_end_matches('\0').to_string();
                let segname = String::from_utf8_lossy(&sect[16..32]).trim_end_matches('\0').to_string();
                if segname == "__TEXT" && sname == "__const" {
                    let sz = u64::from_le_bytes(sect[40..48].try_into().ok()?);
                    let off = u32::from_le_bytes(sect[48..52].try_into().ok()?) as u64;
                    return Some((slice_off + off, sz));
                }
            }
        }
        p += size.max(8);
    }
    None
}

/// Scans the file range [start, start+len) for the sentinel and parses the fuses (read in chunks).
fn scan_range_for_fuses(f: &mut fs::File, start: u64, len: u64) -> Result<Option<ElectronFuses>> {
    const CHUNK: usize = 8 << 20;
    let overlap = FUSE_SENTINEL.len() + 64;
    let mut pos = start;
    let end = start + len;
    let mut buf = vec![0u8; CHUNK + overlap];
    while pos < end {
        let want = ((end - pos) as usize).min(CHUNK + overlap);
        f.seek(SeekFrom::Start(pos))?;
        let mut got = 0;
        while got < want {
            let n = f.read(&mut buf[got..want])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        if let Some(i) = find_sentinel(&buf[..got]) {
            // reread from that position to be sure the whole block is read
            let mut blk = [0u8; 64];
            f.seek(SeekFrom::Start(pos + (i + FUSE_SENTINEL.len()) as u64))?;
            let n = f.read(&mut blk)?;
            return Ok(parse_fuse_block(&blk[..n]));
        }
        if got < want {
            break;
        }
        pos += CHUNK as u64;
    }
    Ok(None)
}

/// File range (offset, size) of the section named `name` in a PE (.exe) file. None if not PE.
///
/// Windows Electron executables keep the fuse sentinel in `.rdata` (measured: 41 MB of .rdata in
/// a 235 MB Discord.exe).
fn pe_section(f: &mut fs::File, name: &[u8]) -> Option<(u64, u64)> {
    let mut mz = [0u8; 64];
    f.seek(SeekFrom::Start(0)).ok()?;
    f.read_exact(&mut mz).ok()?;
    if &mz[0..2] != b"MZ" {
        return None;
    }
    let pe = u32::from_le_bytes(mz[0x3c..0x40].try_into().ok()?) as u64;
    let mut h = [0u8; 24];
    f.seek(SeekFrom::Start(pe)).ok()?;
    f.read_exact(&mut h).ok()?;
    if &h[0..4] != b"PE\0\0" {
        return None;
    }
    let nsec = u16::from_le_bytes([h[6], h[7]]) as usize;
    let opt = u16::from_le_bytes([h[20], h[21]]) as u64;
    let mut tbl = vec![0u8; nsec.min(96) * 40];
    f.seek(SeekFrom::Start(pe + 24 + opt)).ok()?;
    f.read_exact(&mut tbl).ok()?;
    for s in tbl.chunks_exact(40) {
        let end = s[0..8].iter().position(|&b| b == 0).unwrap_or(8);
        if &s[0..end] == name {
            let size = u32::from_le_bytes(s[16..20].try_into().ok()?) as u64;
            let off = u32::from_le_bytes(s[20..24].try_into().ok()?) as u64;
            if size > 0 {
                return Some((off, size));
            }
        }
    }
    None
}

/// Reads the fuses from an executable (or macOS's Electron Framework), read only.
///
/// Looks at (__TEXT,__const) for Mach-O or .rdata for PE first, then falls back to scanning the
/// whole file (slice).
pub fn read_fuses_file(path: &Path) -> Result<ElectronFuses> {
    let mut f = fs::File::open(path).with_context(|| format!("{} 열기 실패", path.display()))?;
    let (off, size) = macho_slice(&mut f)?;
    if let Some((s, l)) = macho_text_const(&mut f, off) {
        if let Some(fz) = scan_range_for_fuses(&mut f, s, l)? {
            return Ok(fz);
        }
    }
    if let Some((s, l)) = pe_section(&mut f, b".rdata") {
        if let Some(fz) = scan_range_for_fuses(&mut f, s, l)? {
            return Ok(fz);
        }
    }
    scan_range_for_fuses(&mut f, off, size)?.ok_or_else(|| anyhow!("퓨즈 센티넬을 찾지 못했습니다: {}", path.display()))
}

/// Reads the fuses from the app's Electron binary (read only).
///
/// macOS: the app bundle's (…/Foo.app) Electron Framework. Windows: the app executable in the
/// install folder (or the .exe itself).
pub fn read_fuses(bundle: &Path) -> Result<ElectronFuses> {
    #[cfg(windows)]
    {
        let exe = win_apps::exe_for(bundle)
            .ok_or_else(|| anyhow!("Electron 실행 파일을 찾지 못했습니다: {}", bundle.display()))?;
        read_fuses_file(&exe)
    }
    #[cfg(not(windows))]
    {
        read_fuses_file(&bundle.join("Contents/Frameworks/Electron Framework.framework/Electron Framework"))
    }
}

// ---- ASAR (app package) reading ----

/// Reads one file from app.asar. (Header: [u32 4][u32 header pickle size][u32][u32 json
/// length][json])
pub fn asar_read(asar: &Path, rel: &str) -> Option<Vec<u8>> {
    let mut f = fs::File::open(asar).ok()?;
    let mut h = [0u8; 16];
    f.read_exact(&mut h).ok()?;
    let header_size = u32::from_le_bytes(h[4..8].try_into().ok()?) as u64;
    let json_len = u32::from_le_bytes(h[12..16].try_into().ok()?) as usize;
    if json_len > 64 << 20 {
        return None;
    }
    let mut js = vec![0u8; json_len];
    f.read_exact(&mut js).ok()?;
    let header: Value = serde_json::from_slice(&js).ok()?;
    let mut node = &header;
    for part in rel.split('/').filter(|p| !p.is_empty() && *p != ".") {
        node = node.get("files")?.get(part)?;
    }
    if node.get("unpacked").and_then(|v| v.as_bool()) == Some(true) {
        let mut up = asar.as_os_str().to_owned();
        up.push(".unpacked");
        return fs::read(PathBuf::from(up).join(rel)).ok();
    }
    let off: u64 = match &node["offset"] {
        Value::String(s) => s.parse().ok()?,
        Value::Number(n) => n.as_u64()?,
        _ => return None,
    };
    let size = node["size"].as_u64()? as usize;
    if size > 256 << 20 {
        return None;
    }
    f.seek(SeekFrom::Start(8 + header_size + off)).ok()?;
    let mut out = vec![0u8; size];
    f.read_exact(&mut out).ok()?;
    Some(out)
}

/// Reads a file from the app folder or app.asar in the app's resources folder (`res`: macOS
/// Contents/Resources, Windows <app folder>\resources).
fn read_res_file(res: &Path, rel: &str) -> Option<Vec<u8>> {
    if let Ok(b) = fs::read(res.join("app").join(rel)) {
        return Some(b);
    }
    asar_read(&res.join("app.asar"), rel)
}

fn read_res_json(res: &Path, rel: &str) -> Option<Value> {
    // hand-edited JSON on Windows may carry a UTF-8 BOM
    read_res_file(res, rel).and_then(|b| serde_json::from_slice(b.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&b)).ok())
}

/// Reads a file from the app bundle's resources (Contents/Resources/app folder or app.asar).
fn read_app_file(bundle: &Path, rel: &str) -> Option<Vec<u8>> {
    read_res_file(&bundle.join("Contents/Resources"), rel)
}

fn read_app_json(bundle: &Path, rel: &str) -> Option<Value> {
    read_app_file(bundle, rel).and_then(|b| serde_json::from_slice(&b).ok())
}

// ---- app detection ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppFamily {
    /// VS Code family (apps with dataFolderName in product.json: VS Code, Antigravity, Cursor, …).
    VsCode,
    /// OpenCode desktop (XDG_* paths must be redirected into the workspace too).
    OpenCode,
    /// Generic Electron app (only the userData path is redirected to the workspace).
    Generic,
}

#[derive(Clone, Debug)]
pub struct ElectronApp {
    pub name: String,
    pub bundle_id: String,
    pub bundle: PathBuf,
    pub exe: PathBuf,
    pub app_version: Option<String>,
    pub electron_version: Option<String>,
    pub fuses: Option<ElectronFuses>,
    /// Can run in hidden mode (= EnableNodeCliInspectArguments fuse on, not on the deny list).
    pub hideable: bool,
    /// Why it isn't supported (when hideable=false).
    pub reason: Option<String>,
    pub family: AppFamily,
    /// Guessed original userData folder (~/Library/Application Support/<name>), only if it exists.
    pub user_data_guess: Option<PathBuf>,
    /// package.json main (e.g. "out/main.js").
    pub main_script: Option<String>,
    /// Whether the user is running it right now (informational).
    pub running: bool,
    /// Description of how it launches (for the UI).
    pub launch_profile: String,
}

/// Apps never launched (they exit or can't be hidden once they see debugging switches).
const NEVER_LAUNCH: &[(&str, &str)] = &[
    ("com.anthropic.claudefordesktop", "Claude 는 디버깅 스위치를 감지하면 종료하며 숨김 모드를 지원하지 않습니다"),
    ("com.hnc.Discord", "Discord 는 Node 인스펙터 퓨즈가 꺼져 있어 창을 숨길 수 없습니다"),
    ("com.unity3d.unityhub", "Unity Hub 는 Node 인스펙터 퓨즈가 꺼져 있어 창을 숨길 수 없습니다"),
];

/// Windows: apps blocked by executable name (lowercase). Windows has no bundle IDs, so the list
/// above doesn't apply.
#[cfg(windows)]
const NEVER_LAUNCH_WIN_EXE: &[(&str, &str)] = &[
    ("claude.exe", "Claude 는 디버깅 스위치를 감지하면 종료하며 숨김 모드를 지원하지 않습니다"),
    ("discord.exe", "Discord 는 Node 인스펙터 퓨즈가 꺼져 있어 창을 숨길 수 없습니다"),
    ("discordptb.exe", "Discord 는 Node 인스펙터 퓨즈가 꺼져 있어 창을 숨길 수 없습니다"),
    ("discordcanary.exe", "Discord 는 Node 인스펙터 퓨즈가 꺼져 있어 창을 숨길 수 없습니다"),
    ("unity hub.exe", "Unity Hub 는 Node 인스펙터 퓨즈가 꺼져 있어 창을 숨길 수 없습니다"),
];

/// Reason if it's on the deny list (macOS: bundle ID; Windows: bundle ID or executable name).
fn blocked_reason(bundle_id: &str, exe: &Path) -> Option<&'static str> {
    if let Some((_, why)) = NEVER_LAUNCH.iter().find(|(id, _)| *id == bundle_id) {
        return Some(why);
    }
    #[cfg(windows)]
    {
        let name = exe.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        if let Some((_, why)) = NEVER_LAUNCH_WIN_EXE.iter().find(|(n, _)| *n == name) {
            return Some(why);
        }
    }
    let _ = exe;
    None
}

/// Installed Electron apps (read only).
///
/// macOS: .app in /Applications and ~/Applications. Windows: the installed programs list
/// (Uninstall registry), App Paths, %LOCALAPPDATA%\Programs, %LOCALAPPDATA% (Squirrel) and
/// Program Files.
pub fn detect_apps() -> Vec<ElectronApp> {
    #[cfg(windows)]
    {
        win_apps::detect()
    }
    #[cfg(not(windows))]
    {
        detect_bundles()
    }
}

/// Inspects one app. None if it isn't an Electron app.
///
/// macOS: a .app bundle path. Windows: an install folder (Squirrel root included) or the app
/// executable (.exe) path.
pub fn inspect_app(bundle: &Path) -> Option<ElectronApp> {
    #[cfg(windows)]
    {
        win_apps::inspect(bundle)
    }
    #[cfg(not(windows))]
    {
        inspect_bundle(bundle)
    }
}

/// Electron apps in /Applications and ~/Applications (read only).
#[cfg(not(windows))]
fn detect_bundles() -> Vec<ElectronApp> {
    let mut dirs_ = vec![PathBuf::from("/Applications")];
    if let Some(h) = dirs::home_dir() {
        dirs_.push(h.join("Applications"));
    }
    let running = running_exes();
    let mut out = Vec::new();
    for d in dirs_ {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.extension().and_then(|e| e.to_str()) != Some("app") {
                continue;
            }
            if let Some(mut a) = inspect_app(&p) {
                a.running = running.contains(&a.exe);
                out.push(a);
            }
        }
    }
    out
}

/// Inspects one app bundle. None if it isn't an Electron app.
#[cfg(not(windows))]
fn inspect_bundle(bundle: &Path) -> Option<ElectronApp> {
    let fw = bundle.join("Contents/Frameworks/Electron Framework.framework");
    if !fw.is_dir() {
        return None;
    }
    let info = read_plist_strings(
        &bundle.join("Contents/Info.plist"),
        &["CFBundleExecutable", "CFBundleIdentifier", "CFBundleName", "CFBundleDisplayName", "CFBundleShortVersionString"],
    );
    let exe_name = info.get("CFBundleExecutable").cloned().unwrap_or_else(|| {
        bundle.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    });
    let exe = bundle.join("Contents/MacOS").join(&exe_name);
    let bundle_id = info.get("CFBundleIdentifier").cloned().unwrap_or_default();
    let name = info
        .get("CFBundleDisplayName")
        .cloned()
        .or_else(|| bundle.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .or(info.get("CFBundleName").cloned())
        .unwrap_or_else(|| exe_name.clone());
    let electron_version = read_plist_strings(&fw.join("Resources/Info.plist"), &["CFBundleVersion"])
        .get("CFBundleVersion")
        .cloned();

    let pkg = read_app_json(bundle, "package.json");
    let product = read_app_json(bundle, "product.json");
    let main_script = pkg.as_ref().and_then(|p| p["main"].as_str()).map(normalize_main);
    let family = if product.as_ref().map(|p| p["dataFolderName"].is_string()).unwrap_or(false) {
        AppFamily::VsCode
    } else if bundle_id == "ai.opencode.desktop" {
        AppFamily::OpenCode
    } else {
        AppFamily::Generic
    };

    // guess userData: Electron's default is appData/<productName|name>
    let sup = dirs::data_dir().unwrap_or_default();
    let mut cands: Vec<String> = Vec::new();
    if let Some(p) = &product {
        for k in ["nameShort", "nameLong"] {
            if let Some(s) = p[k].as_str() {
                cands.push(s.to_string());
            }
        }
    }
    if let Some(p) = &pkg {
        for k in ["productName", "name"] {
            if let Some(s) = p[k].as_str() {
                cands.push(s.to_string());
            }
        }
    }
    cands.push(bundle_id.clone());
    cands.push(name.clone());
    if let Some(n) = info.get("CFBundleName") {
        cands.push(n.clone());
    }
    let user_data_guess = cands
        .iter()
        .filter(|c| !c.is_empty() && !c.contains('/'))
        .map(|c| sup.join(c))
        .find(|p| p.is_dir());

    let fuses = read_fuses(bundle).ok();
    let blocked = NEVER_LAUNCH.iter().find(|(id, _)| *id == bundle_id);
    let (hideable, reason) = if let Some((_, why)) = blocked {
        (false, Some(why.to_string()))
    } else if !exe.is_file() {
        (false, Some(format!("실행 파일이 없습니다: {}", exe.display())))
    } else {
        match &fuses {
            None => (false, Some("Electron 퓨즈를 읽지 못했습니다".into())),
            Some(f) if f.inspect_allowed() => (true, None),
            Some(_) => (
                false,
                Some("EnableNodeCliInspectArguments 퓨즈가 꺼져 있어 창을 숨긴 채 실행할 수 없습니다".into()),
            ),
        }
    };
    let launch_profile = match family {
        AppFamily::VsCode => "VS Code 계열: VSCODE_PORTABLE + --user-data-dir/--extensions-dir/--shared-data-dir 를 작업공간 안으로, 업데이트·텔레메트리·크래시 리포터 끔".into(),
        AppFamily::OpenCode => "OpenCode: --user-data-dir + XDG_DATA_HOME/XDG_CONFIG_HOME/XDG_CACHE_HOME/XDG_STATE_HOME 를 작업공간 안으로".into(),
        AppFamily::Generic => "일반: --user-data-dir + app.setPath('userData') 를 작업공간 안으로".into(),
    };
    Some(ElectronApp {
        name,
        bundle_id,
        bundle: bundle.to_path_buf(),
        exe,
        app_version: info.get("CFBundleShortVersionString").cloned(),
        electron_version,
        fuses,
        hideable,
        reason,
        family,
        user_data_guess,
        main_script,
        running: false,
        launch_profile,
    })
}

/// Normalizes package.json main to the "out/main.js" form.
fn normalize_main(m: &str) -> String {
    let mut s = m.trim().trim_start_matches("./").to_string();
    if s.is_empty() {
        s = "index.js".into();
    }
    if s.ends_with('/') {
        s.push_str("index.js");
    } else if !s.ends_with(".js") && !s.ends_with(".mjs") && !s.ends_with(".cjs") {
        s.push_str(".js");
    }
    s
}

fn regex_escape(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

/// URL regex for the main script breakpoint (urlRegex of CDP Debugger.setBreakpointByUrl).
///
/// macOS: the same `Resources/app(\.asar)?/<main>$` as before. Windows: the folder is lowercase
/// `resources`, the file system is case-insensitive, and older Node reports `\` paths like
/// `C:\…\resources\app\out\main.js`, so case and separators are ignored. urlRegex takes no flags,
/// so this is done with character classes. Characters percent-encoded in file:// URLs (spaces, …)
/// match both as is and encoded.
fn main_url_regex(main: &str, windows: bool) -> String {
    if !windows {
        return format!("Resources/app(\\.asar)?/{}$", regex_escape(main));
    }
    const SEP: &str = "[\\\\/]";
    let ci = |s: &str| -> String {
        let mut o = String::new();
        for c in s.chars() {
            if c.is_ascii_alphabetic() {
                o.push('[');
                o.push(c.to_ascii_uppercase());
                o.push(c.to_ascii_lowercase());
                o.push(']');
            } else if c.is_ascii_alphanumeric() || "-._~".contains(c) {
                o.push_str(&regex_escape(&c.to_string()));
            } else {
                let mut b = [0u8; 4];
                let enc: String = c.encode_utf8(&mut b).bytes().map(|x| format!("%{x:02X}")).collect();
                o.push_str(&format!("(?:{}|{})", regex_escape(&c.to_string()), enc));
            }
        }
        o
    };
    let parts: Vec<String> = main.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".").map(ci).collect();
    format!("{}{SEP}{}(\\.{})?{SEP}{}$", ci("resources"), ci("app"), ci("asar"), parts.join(SEP))
}

/// Whether `url` ends with `/<main>` (ignoring case and `\` vs `/`; for Windows).
fn url_ends_with_ci(url: &str, main: &str) -> bool {
    let u = url.replace('\\', "/").to_lowercase();
    u.ends_with(&format!("/{}", main.replace('\\', "/").to_lowercase()))
}

/// AppUserModelID of a hidden instance (Windows): differs per workspace so it doesn't share the
/// taskbar group or jump list with the user's real app.
///
/// AUMIDs must be at most 128 chars with no spaces, so it's squeezed to ASCII with a hash appended.
fn hidden_aumid(workspace: &str) -> String {
    let mut s: String = workspace
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .take(40)
        .collect();
    if s.trim_matches('-').is_empty() {
        s = "ws".into();
    }
    let mut h: u32 = 0x811c_9dc5;
    for b in workspace.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("GhostDesk.Hidden.{s}-{:06x}", h & 0xff_ffff)
}

/// Reason if the key combo is refused on Windows Electron screens.
///
/// Alt alone, F10, Alt+letter (no other modifiers), Shift+F10, Alt+Space and the context menu key
/// can open the app's native menus (menu bar mnemonics, system menu, context menu), and those menu
/// windows can get pulled into a monitor (near the current cursor) and show on the user's screen
/// even when the parent window is off-screen. `combo` is the value after cmd→ctrl localization.
/// Shift/Win+Alt+letter (VS Code's Shift+Alt+F format document, …) is allowed: Electron (views
/// RootView) only opens menu bar mnemonics when the modifiers are exactly Alt, and CDP key input
/// doesn't go through the Win32 WM_SYSKEYDOWN menu path.
fn menu_key_refusal(combo: &str) -> Option<String> {
    use crate::keys::{MOD_ALT, MOD_CTRL};
    let lc = combo.trim().to_ascii_lowercase();
    let menu_key = lc
        .split('+')
        .map(str::trim)
        .any(|t| matches!(t, "menu" | "apps" | "application" | "contextmenu" | "context_menu" | "compose"));
    let refuse = menu_key
        || match crate::keys::parse_combo(combo) {
            Err(_) => false, // unparseable keys are reported with the original error below
            Ok(c) => {
                let ctrl = c.mods & MOD_CTRL != 0;
                let alt = c.mods & MOD_ALT != 0;
                let k = c.main.key.as_str();
                if c.main.modifier_bit == MOD_ALT {
                    true // combos where Alt is pressed last (Alt alone, …): focus goes to the menu bar on release
                } else if k == "F10" {
                    !ctrl
                } else if alt && !ctrl {
                    // Alt+Space (system menu) is refused even with other modifiers; Alt+letter (mnemonic) only with
                    // Alt alone
                    k == " " || (c.mods == MOD_ALT && k.chars().count() == 1 && k.chars().all(|ch| ch.is_ascii_alphabetic()))
                } else {
                    false
                }
            }
        };
    refuse.then(|| {
        format!(
            "Windows 의 Electron 앱 화면에서는 '{combo}' 를 보낼 수 없습니다: 단독 Alt·F10·Shift+F10·Alt+글자(다른 수정키 없이)·\
             Alt+Space·메뉴 키는 앱의 네이티브 메뉴를 열 수 있고, 그 메뉴 창은 사용자의 실제 화면에 나타날 수 있습니다. \
             메뉴는 `app_menu` 도구(list 로 확인한 뒤 'File > New Text File' 같은 경로로 click)나 앱 안의 명령 팔레트를 쓰세요. \
             Shift+Alt+글자·Ctrl+Alt+글자 같은 단축키는 보낼 수 있습니다. \
             (Bare Alt, F10, Shift+F10, Alt+letter with no other modifier, Alt+Space and the menu key are blocked on Windows \
             Electron desks because native menus can pop up on the human's screen; use the `app_menu` tool instead. \
             Shift+Alt+letter and Ctrl+Alt+letter shortcuts are allowed.)"
        )
    })
}

/// Off-screen coordinate math (Windows): real pixel coordinates `px` outside every monitor → DIP
/// for Electron's setPosition.
///
/// Divides by the primary monitor's scale, and caps |DIP| × max scale ≤ 24000 so that converting
/// back to pixels at any monitor's scale stays well away from the Win32 16-bit coordinate limit
/// (±32767) and the minimized marker (-32000).
fn offscreen_dip_math(px: (i32, i32), primary_scale: f64, max_scale: f64) -> (i32, i32) {
    let p = if primary_scale.is_finite() && primary_scale >= 0.5 { primary_scale } else { 1.0 };
    let m = if max_scale.is_finite() { max_scale.max(p).max(1.0) } else { p.max(1.0) };
    let cap = (24000.0 / m).floor();
    let f = |v: i32| ((v as f64) / p).round().clamp(-cap, cap) as i32;
    (f(px.0), f(px.1))
}

#[cfg(target_os = "macos")]
fn read_plist_strings(path: &Path, keys: &[&str]) -> HashMap<String, String> {
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSDictionary, NSString};
    let mut out = HashMap::new();
    objc2::rc::autoreleasepool(|_| {
        let p = NSString::from_str(&path.to_string_lossy());
        #[allow(deprecated)]
        let Some(d) = (unsafe { NSDictionary::<NSString, AnyObject>::dictionaryWithContentsOfFile(&p) }) else {
            return;
        };
        for k in keys {
            if let Some(v) = d.objectForKey(&NSString::from_str(k)) {
                if let Some(s) = v.downcast_ref::<NSString>() {
                    out.insert(k.to_string(), s.to_string());
                }
            }
        }
    });
    out
}

#[cfg(not(target_os = "macos"))]
fn read_plist_strings(_path: &Path, _keys: &[&str]) -> HashMap<String, String> {
    HashMap::new()
}

/// Executable paths of all running processes.
#[cfg(not(windows))]
#[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
fn running_exes() -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    #[cfg(target_os = "macos")]
    for (pid, _, _) in proc::all() {
        if let Some(p) = proc::path(pid) {
            out.insert(p);
        }
    }
    out
}

// ---- pure functions for Windows app detection (compiled and tested on every OS) ----

/// Version numbers of a Squirrel version folder name ("app-1.0.9266", lowercase).
fn squirrel_version(name: &str) -> Option<Vec<u64>> {
    let v = name.strip_prefix("app-")?;
    let core = v.split(['-', '+']).next()?;
    let parts: Option<Vec<u64>> = core.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| !p.is_empty())
}

/// Whether a name could be a subfolder holding the app files (Velopack "current", version
/// "1.2.3", commit hash).
fn looks_versioned(name: &str) -> bool {
    name == "current"
        || (name.len() >= 7 && name.chars().all(|c| c.is_ascii_hexdigit()))
        || (name.chars().any(|c| c.is_ascii_digit()) && name.trim_start_matches('v').chars().all(|c| c.is_ascii_digit() || c == '.'))
}

/// Helper executables that aren't the app itself (lowercase file names).
fn is_helper_exe(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "update.exe",
        "squirrel.exe",
        "crashpad_handler.exe",
        "elevate.exe",
        "notification_helper.exe",
        "updater_launcher.exe",
        "createdump.exe",
        "inno_updater.exe",
        "winpty-agent.exe",
    ];
    EXACT.contains(&name) || name.starts_with("unins") || name.starts_with("uninstall") || name.ends_with("_helper.exe")
}

/// Whether it's usable as a Windows folder name (for guessing userData).
fn dir_name_ok(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty()
        && t == s
        && t != "."
        && t != ".."
        && !t.ends_with('.')
        && !t.chars().any(|c| matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control())
        && !crate::platform::paths::is_reserved_name(t)
}

/// Stand-in for a bundle ID on Windows. Used by the workspace marker (to keep apps from mixing),
/// the deny list and the self-test, so it must not be empty.
///
/// Order: product.json darwinBundleIdentifier (the VS Code family uses the same ID as on macOS) →
/// win32AppUserModelId → package.json build.appId → Squirrel AUMID
/// (com.squirrel.<root>.<executable>) → win.<executable>.
fn win_bundle_id(product: Option<&Value>, pkg: Option<&Value>, squirrel_root: Option<&str>, exe_stem: &str) -> String {
    let s = |v: Option<&Value>| v.and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    s(product.map(|p| &p["darwinBundleIdentifier"]))
        .or_else(|| s(product.map(|p| &p["win32AppUserModelId"])))
        .or_else(|| s(pkg.map(|p| &p["build"]["appId"])))
        .or_else(|| squirrel_root.map(|r| format!("com.squirrel.{r}.{exe_stem}")))
        .unwrap_or_else(|| format!("win.{}", exe_stem.to_lowercase()))
}

/// Path comparison key (Windows: case-insensitive, trailing separator ignored).
fn path_key(p: &Path) -> String {
    let c: PathBuf = p.components().collect();
    if cfg!(windows) {
        c.to_string_lossy().to_lowercase()
    } else {
        c.to_string_lossy().into_owned()
    }
}

#[cfg(windows)]
mod win_apps {
    //! Finding and inspecting Electron apps on Windows (read only).
    //!
    //! Electron app = an executable in a folder that has `<folder>\resources\app.asar` (or
    //! `resources\app\package.json`) and the Chromium runtime files. The fuses live in that
    //! executable (.exe).
    use super::*;
    use crate::platform::win;
    use std::sync::OnceLock;
    use std::time::SystemTime;

    const RUNTIME_MARKERS: &[&str] = &["icudtl.dat", "v8_context_snapshot.bin", "snapshot_blob.bin", "chrome_100_percent.pak"];

    /// Large folders under %LOCALAPPDATA% that are definitely not apps (not scanned).
    const LOCAL_SKIP: &[&str] = &[
        "temp",
        "packages",
        "microsoft",
        "crashdumps",
        "d3dscache",
        "connecteddevicesplatform",
        "comms",
        "peerdistrepub",
        "virtualstore",
        "application data",
        "history",
        "temporary internet files",
        "isolatedstorage",
        "publishers",
        "placeholdertilelogofolder",
        "npm-cache",
        "pip",
        "ms-playwright",
        "package cache",
        "squirreltemp",
        "ghostdesk",
    ];

    /// Folders under Program Files not descended into further.
    fn skip_program_child(name: &str) -> bool {
        name.starts_with("windows")
            || matches!(
                name,
                "common files" | "microsoft office" | "reference assemblies" | "dotnet" | "modifiablewindowsapps" | "uninstall information"
            )
    }

    fn subdirs(d: &Path) -> Vec<PathBuf> {
        fs::read_dir(d)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn lower_name(p: &Path) -> String {
        p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
    }

    /// Whether the file is inside a Microsoft Store (MSIX) package (…\WindowsApps\<package>\…).
    ///
    /// Running a packaged app directly by path runs it without package identity or virtualization,
    /// changing its behavior (writing to the user's real folders, …), and if the app relaunches itself
    /// as a package, hiding can be lost.
    pub(super) fn is_msix_path(p: &Path) -> bool {
        p.components().any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case("WindowsApps"))
    }

    /// Whether `d` is a folder holding an Electron app executable.
    pub(super) fn is_app_dir(d: &Path) -> bool {
        let res = d.join("resources");
        if !res.is_dir() {
            return false;
        }
        (res.join("app.asar").is_file() || res.join("app").join("package.json").is_file())
            && RUNTIME_MARKERS.iter().any(|m| d.join(m).is_file())
    }

    /// Candidate folder → (install root, executable folder).
    ///
    /// For Squirrel (`<root>\app-<version>\`) picks the highest version; for other one-level-down
    /// folders (Velopack `current`, version or commit names) picks the most recently modified. The
    /// install root is the folder that doesn't change across updates.
    pub(super) fn resolve_layout(dir: &Path) -> Option<(PathBuf, PathBuf)> {
        if lower_name(dir) == "windowsapps" {
            return None; // the package folder listing itself (not scanned)
        }
        if is_app_dir(dir) {
            let name = lower_name(dir);
            if let Some(parent) = dir.parent() {
                if (squirrel_version(&name).is_some() || name == "current") && parent.join("Update.exe").is_file() {
                    return Some((parent.to_path_buf(), dir.to_path_buf()));
                }
            }
            return Some((dir.to_path_buf(), dir.to_path_buf()));
        }
        let mut best: Option<(Vec<u64>, PathBuf)> = None;
        let mut other: Option<(SystemTime, PathBuf)> = None;
        for c in subdirs(dir) {
            let name = lower_name(&c);
            let sq = squirrel_version(&name);
            if sq.is_none() && !looks_versioned(&name) {
                continue;
            }
            if !is_app_dir(&c) {
                continue;
            }
            match sq {
                Some(v) => {
                    if best.as_ref().is_none_or(|(b, _)| v > *b) {
                        best = Some((v, c));
                    }
                }
                None => {
                    let t = fs::metadata(&c).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
                    if other.as_ref().is_none_or(|(o, _)| t > *o) {
                        other = Some((t, c));
                    }
                }
            }
        }
        best.map(|(_, p)| p).or(other.map(|(_, p)| p)).map(|p| (dir.to_path_buf(), p))
    }

    /// The app folder's main executable: the largest .exe excluding helpers (Electron binaries are
    /// usually over 100 MB).
    pub(super) fn main_exe(appdir: &Path) -> Option<PathBuf> {
        let mut best: Option<(u64, PathBuf)> = None;
        for e in fs::read_dir(appdir).ok()?.flatten() {
            let p = e.path();
            let name = lower_name(&p);
            if !name.ends_with(".exe") || is_helper_exe(&name) {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            if !md.is_file() {
                continue;
            }
            if best.as_ref().is_none_or(|(l, _)| md.len() > *l) {
                best = Some((md.len(), p));
            }
        }
        best.map(|(_, p)| p)
    }

    /// Install folder or .exe path → the executable to read fuses from.
    pub(super) fn exe_for(p: &Path) -> Option<PathBuf> {
        if p.is_file() {
            return Some(p.to_path_buf());
        }
        let (_, appdir) = resolve_layout(p)?;
        main_exe(&appdir)
    }

    /// Fuse inspection cache (not reread if path, size and mtime match, since executables are hundreds
    /// of MB).
    fn cached_fuses(exe: &Path) -> Option<ElectronFuses> {
        type Entry = (u64, Option<SystemTime>, Option<ElectronFuses>);
        static CACHE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
        let cache = CACHE.get_or_init(Default::default);
        let md = fs::metadata(exe).ok()?;
        let (len, mt) = (md.len(), md.modified().ok());
        let key = path_key(exe);
        if let Some((l, m, f)) = cache.lock().get(&key) {
            if *l == len && *m == mt {
                return f.clone();
            }
        }
        let f = read_fuses_file(exe).ok();
        cache.lock().insert(key, (len, mt, f.clone()));
        f
    }

    /// Builds an ElectronApp from the install root and app folder (executable folder).
    pub(super) fn inspect_dirs(root: &Path, appdir: &Path, exe: Option<PathBuf>) -> Option<ElectronApp> {
        let exe = match exe {
            Some(e) => e,
            None => main_exe(appdir)?,
        };
        let res = appdir.join("resources");
        let pkg = read_res_json(&res, "package.json");
        let product = read_res_json(&res, "product.json");
        let vi = win::version_info(&exe).unwrap_or_default();
        let stem = exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let root_name = root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let squirrel = root != appdir && squirrel_version(&lower_name(appdir)).is_some();
        let bundle_id = win_bundle_id(product.as_ref(), pkg.as_ref(), squirrel.then_some(root_name.as_str()), &stem);
        let pstr = |v: &Option<Value>, k: &str| {
            v.as_ref().and_then(|v| v[k].as_str()).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
        };
        let not_electron = |s: &String| !s.eq_ignore_ascii_case("electron");
        let name = vi
            .product_name
            .clone()
            .filter(not_electron)
            .or_else(|| pstr(&product, "nameLong"))
            .or_else(|| pstr(&pkg, "productName"))
            .or_else(|| vi.file_description.clone().filter(not_electron))
            .unwrap_or_else(|| stem.clone());
        let app_version = vi.product_version.clone().or_else(|| pstr(&pkg, "version")).or_else(|| pstr(&product, "version"));
        let electron_version = fs::read_to_string(appdir.join("version"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s.len() < 40)
            .or_else(|| {
                pkg.as_ref()
                    .and_then(|p| ["devDependencies", "dependencies"].iter().find_map(|k| p[*k]["electron"].as_str()))
                    .map(|s| s.trim_start_matches(['^', '~', '=', 'v']).to_string())
            });
        let main_script = pkg.as_ref().and_then(|p| p["main"].as_str()).map(normalize_main);
        let mentions_opencode = [pstr(&pkg, "name"), pstr(&pkg, "productName"), Some(stem.clone())]
            .into_iter()
            .flatten()
            .any(|s| s.to_lowercase().replace(['-', ' ', '_'], "").contains("opencode"));
        let family = if product.as_ref().map(|p| p["dataFolderName"].is_string()).unwrap_or(false) {
            AppFamily::VsCode
        } else if bundle_id == "ai.opencode.desktop" || mentions_opencode {
            AppFamily::OpenCode
        } else {
            AppFamily::Generic
        };

        // guess userData: Electron's default is %APPDATA%\<productName|name> (VS Code: product.json
        // nameShort = "Code")
        let appdata = dirs::data_dir().unwrap_or_default();
        let mut cands: Vec<String> = Vec::new();
        for k in ["nameShort", "nameLong"] {
            cands.extend(pstr(&product, k));
        }
        for k in ["productName", "name"] {
            cands.extend(pstr(&pkg, k));
        }
        cands.push(name.clone());
        cands.extend(vi.product_name.clone());
        cands.push(stem.clone());
        let user_data_guess = cands.iter().filter(|c| dir_name_ok(c)).map(|c| appdata.join(c)).find(|p| p.is_dir());

        let fuses = cached_fuses(&exe);
        let (hideable, reason) = if let Some(why) = blocked_reason(&bundle_id, &exe) {
            (false, Some(why.to_string()))
        } else if is_msix_path(&exe) {
            (
                false,
                Some(
                    "Microsoft Store(MSIX) 패키지 앱은 패키지 밖에서 실행 인자를 바꿔 따로 띄울 수 없어 숨김 모드를 지원하지 않습니다"
                        .into(),
                ),
            )
        } else if !exe.is_file() {
            (false, Some(format!("실행 파일이 없습니다: {}", exe.display())))
        } else {
            match &fuses {
                None => (false, Some("Electron 퓨즈를 읽지 못했습니다".into())),
                Some(f) if f.inspect_allowed() => (true, None),
                Some(_) => (
                    false,
                    Some("EnableNodeCliInspectArguments 퓨즈가 꺼져 있어 창을 숨긴 채 실행할 수 없습니다".into()),
                ),
            }
        };
        let launch_profile = match family {
            AppFamily::VsCode => "VS Code 계열: --user-data-dir/--extensions-dir/--shared-data-dir 를 작업공간 안으로 (VSCODE_PORTABLE 도 지정), 업데이트·텔레메트리·크래시 리포터 끔".into(),
            AppFamily::OpenCode => "OpenCode: --user-data-dir + XDG_DATA_HOME/XDG_CONFIG_HOME/XDG_CACHE_HOME/XDG_STATE_HOME 를 작업공간 안으로".into(),
            AppFamily::Generic => "일반: --user-data-dir + app.setPath('userData') 를 작업공간 안으로".into(),
        };
        Some(ElectronApp {
            name,
            bundle_id,
            bundle: root.to_path_buf(),
            exe,
            app_version,
            electron_version,
            fuses,
            hideable,
            reason,
            family,
            user_data_guess,
            main_script,
            running: false,
            launch_profile,
        })
    }

    /// Inspects an install folder (or Squirrel root) or a single .exe.
    pub(super) fn inspect(path: &Path) -> Option<ElectronApp> {
        let path = crate::platform::paths::simplify(path);
        let mut a = if path.is_file() {
            let dir = path.parent()?;
            let (root, appdir) = resolve_layout(dir)?;
            let name = lower_name(&path);
            // if the Squirrel root's launcher (a small stub like Discord.exe) or a helper was picked, switch
            // to the real app executable
            let exe = if appdir == dir && name.ends_with(".exe") && !is_helper_exe(&name) {
                Some(path.clone())
            } else {
                let same = appdir.join(path.file_name()?);
                (same.is_file() && !is_helper_exe(&name)).then_some(same)
            };
            inspect_dirs(&root, &appdir, exe)?
        } else {
            let (root, appdir) = resolve_layout(&path)?;
            inspect_dirs(&root, &appdir, None)?
        };
        a.running = running_exes(std::slice::from_ref(&a)).contains(&path_key(&a.exe));
        Some(a)
    }

    /// Folders likely to contain apps (may have duplicates).
    fn candidate_dirs() -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = Vec::new();
        for (_, loc, icon) in win::installed_programs() {
            v.extend(loc);
            if let Some(i) = icon {
                if i.extension().map(|e| e.eq_ignore_ascii_case("exe")).unwrap_or(false) {
                    v.extend(i.parent().map(Path::to_path_buf));
                }
            }
        }
        const APP_PATHS: &str = r"Software\Microsoft\Windows\CurrentVersion\App Paths";
        for root in [win::HKEY_CURRENT_USER, win::HKEY_LOCAL_MACHINE] {
            for k in win::reg_subkeys(root, APP_PATHS) {
                if let Some(s) = win::reg_string(root, &format!(r"{APP_PATHS}\{k}"), None) {
                    let exe = PathBuf::from(s.trim().trim_matches('"'));
                    v.extend(exe.parent().map(Path::to_path_buf));
                }
            }
        }
        if let Some(local) = dirs::data_local_dir() {
            v.extend(subdirs(&local.join("Programs")));
            for c in subdirs(&local) {
                if !LOCAL_SKIP.contains(&lower_name(&c).as_str()) {
                    v.push(c);
                }
            }
        }
        for pf in win::program_files_dirs() {
            for c in subdirs(&pf) {
                let name = lower_name(&c);
                if name == "windowsapps" {
                    continue;
                }
                if !skip_program_child(&name) {
                    v.extend(subdirs(&c));
                }
                v.push(c);
            }
        }
        v
    }

    /// Running app executables (path_key). Only processes with matching names have their paths looked
    /// up.
    fn running_exes(apps: &[ElectronApp]) -> HashSet<String> {
        let names: HashSet<String> =
            apps.iter().filter_map(|a| a.exe.file_name()).map(|n| n.to_string_lossy().to_lowercase()).collect();
        let mut out = HashSet::new();
        if names.is_empty() {
            return out;
        }
        for (pid, _, exe) in win::processes() {
            if names.contains(&exe.to_lowercase()) {
                if let Some(p) = win::process_path(pid) {
                    out.insert(path_key(&crate::platform::paths::simplify(&p)));
                }
            }
        }
        out
    }

    pub(super) fn detect() -> Vec<ElectronApp> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<ElectronApp> = Vec::new();
        for dir in candidate_dirs() {
            let Some((root, appdir)) = resolve_layout(&dir) else { continue };
            if !seen.insert(path_key(&appdir)) {
                continue;
            }
            if let Some(a) = inspect_dirs(&root, &appdir, None) {
                out.push(a);
            }
        }
        let running = running_exes(&out);
        for a in &mut out {
            a.running = running.contains(&path_key(&a.exe));
        }
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.bundle.cmp(&b.bundle)));
        out
    }
}

// ---- workspace preparation ----

#[derive(Clone, Debug)]
pub enum ElectronProfile {
    /// Empty profile (an existing workspace is reused as is).
    Empty,
    /// Clones the original userData (keeps the login state). Only use after getting the user's consent
    /// in the UI.
    ///
    /// With `refresh`, the existing workspace's user-data is wiped and cloned again.
    Clone { source: PathBuf, refresh: bool },
}

#[derive(Clone, Debug)]
pub struct WorkspacePaths {
    /// Real workspace folder (~/Library/Application Support/GhostDesk/workspaces/<slug>).
    pub real_root: PathBuf,
    /// Short symlink (/tmp/gd/<slug>); every path handed to the app is based on it.
    pub root: PathBuf,
    pub user_data: PathBuf,
    pub extensions: PathBuf,
    pub shared_data: PathBuf,
    pub agent_plugins: PathBuf,
    pub xdg: PathBuf,
    pub log: PathBuf,
}

#[derive(Clone)]
pub struct ElectronLaunch {
    pub app: ElectronApp,
    /// Workspace name (normalized by profiles::slug).
    pub workspace: String,
    pub width: u32,
    pub height: u32,
    pub profile: ElectronProfile,
    /// VS Code family: folder/file to open (careful: opening a user folder outside the workspace lets
    /// the agent modify it).
    pub open_path: Option<PathBuf>,
    pub extra_args: Vec<String>,
    /// VS Code family: settings to overwrite in User/settings.json.
    pub vscode_settings: Map<String, Value>,
    /// Overrides the workspaces root (default: profiles::workspaces_root()).
    pub workspaces_root: Option<PathBuf>,
    /// Clone progress (files, bytes).
    pub progress: Option<Arc<dyn Fn(u64, u64) + Send + Sync>>,
}

impl ElectronLaunch {
    pub fn new(app: ElectronApp, workspace: &str, width: u32, height: u32) -> Self {
        ElectronLaunch {
            app,
            workspace: workspace.to_string(),
            width,
            height,
            profile: ElectronProfile::Empty,
            open_path: None,
            extra_args: Vec::new(),
            vscode_settings: vscode_default_settings(),
            workspaces_root: None,
            progress: None,
        }
    }
}

/// Default settings for the VS Code family: updates and telemetry off, and dialogs/menus drawn
/// inside the window (HTML) so no native window appears and they show up in screenshots.
pub fn vscode_default_settings() -> Map<String, Value> {
    let v = json!({
        "update.mode": "none",
        "update.showReleaseNotes": false,
        "telemetry.telemetryLevel": "off",
        "extensions.autoUpdate": false,
        "extensions.autoCheckUpdates": false,
        "workbench.enableExperiments": false,
        "workbench.startupEditor": "none",
        "workbench.tips.enabled": false,
        "security.workspace.trust.enabled": false,
        "window.dialogStyle": "custom",
        "window.titleBarStyle": "custom",
        "window.menuStyle": "custom",
        "window.nativeTabs": false,
        "window.restoreWindows": "none",
        "files.simpleDialog.enable": true,
        "terminal.integrated.enablePersistentSessions": false,
    });
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut m = v.as_object().cloned().unwrap_or_default();
    // Windows: the terminal's default right click (copyPaste) pastes the user's real clipboard with no
    // selection and copies over it with one. Make the agent's right click only open the context menu,
    // and turn off copy-on-select too.
    #[cfg(windows)]
    {
        m.insert("terminal.integrated.rightClickBehavior".into(), json!("default"));
        m.insert("terminal.integrated.copyOnSelection".into(), json!(false));
    }
    m
}

/// Entries skipped when cloning Electron userData (caches, locks, logs).
const CLONE_EXCLUDE_NAMES: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "DawnCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "GraphiteDawnCache",
    "GrShaderCache",
    "ShaderCache",
    "CacheStorage",
    "ScriptCache",
    "CachedData",
    "CachedExtensionVSIXs",
    "CachedProfilesData",
    "CachedConfigurations",
    "Crashpad",
    "Crash Reports",
    "SingletonLock",
    "SingletonSocket",
    "SingletonCookie",
    "DevToolsActivePort",
    "RunningChromeVersion",
    "component_crx_cache",
    "optimization_guide_model_store",
    "logs",
    ".DS_Store",
];
/// Entries skipped relative to the userData root (VS Code: unsaved buffer backups, per-folder
/// state).
const CLONE_EXCLUDE_REL: &[&str] = &["Backups", "User/workspaceStorage", "User/History"];

/// Clones the original Electron userData folder (the original is only read).
pub fn clone_user_data(src: &Path, dest: &Path, progress: &dyn Fn(u64, u64)) -> Result<CloneStats> {
    profiles::clone_dir(src, dest, CLONE_EXCLUDE_REL, CLONE_EXCLUDE_NAMES, progress)
}

/// Unix socket sun_path limit (104 bytes on macOS, NUL included).
const SUN_PATH_MAX: usize = 104;

/// Windows: no short link needed. VS Code/Chromium IPC uses named pipes
/// (`\\.\pipe\<hash>-<version>-main-sock`) rather than Unix sockets, so there's no path length
/// limit (sun_path) and the real folder is used as is. (Symlinks also need admin rights or
/// developer mode, another reason not to use them.)
#[cfg(windows)]
fn short_link(real: &Path, _slug: &str) -> Result<PathBuf> {
    Ok(real.to_path_buf())
}

#[cfg(not(any(unix, windows)))]
fn short_link(_real: &Path, _slug: &str) -> Result<PathBuf> {
    bail!("Electron 작업공간은 macOS·Windows 에서만 지원합니다")
}

/// Creates the `/tmp/gd/<slug>` → real workspace symlink.
#[cfg(unix)]
fn short_link(real: &Path, slug: &str) -> Result<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let mut base = PathBuf::from("/tmp/gd");
    let ok_base = |b: &Path| -> bool {
        if fs::create_dir_all(b).is_err() {
            return false;
        }
        match fs::symlink_metadata(b) {
            Ok(m) => {
                use std::os::unix::fs::MetadataExt;
                m.is_dir() && m.uid() == uid
            }
            Err(_) => false,
        }
    };
    if !ok_base(&base) {
        base = PathBuf::from(format!("/tmp/gd-{uid}"));
        ensure!(ok_base(&base), "{} 를 만들 수 없습니다", base.display());
    }
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&base, fs::Permissions::from_mode(0o700));
    }
    // truncate by bytes so multibyte names (Korean, …) stay within the socket path limit (104 bytes)
    let mut name = String::new();
    for c in slug.chars() {
        if name.len() + c.len_utf8() > 24 {
            break;
        }
        name.push(c);
    }
    let mut h: u32 = 0x811c_9dc5;
    for b in real.as_os_str().to_string_lossy().bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    name.push_str(&format!("-{:06x}", h & 0xff_ffff));
    let link = base.join(name);
    match fs::symlink_metadata(&link) {
        Ok(m) if m.file_type().is_symlink() => {
            if fs::read_link(&link).ok().as_deref() != Some(real) {
                fs::remove_file(&link)?;
                std::os::unix::fs::symlink(real, &link)?;
            }
        }
        Ok(_) => bail!("{} 가 심볼릭 링크가 아닙니다", link.display()),
        Err(_) => std::os::unix::fs::symlink(real, &link)?,
    }
    Ok(link)
}

/// Prepares the workspace folder, short link, (cloned/empty) profile and per-app initial settings.
pub fn prepare_workspace(opts: &ElectronLaunch) -> Result<WorkspacePaths> {
    // (on Windows, slug already avoids device names like CON, NUL, COM1 and overly long names)
    let slug = profiles::slug(&opts.workspace);
    // (same location as profiles::workspaces_root on macOS; non-roaming %LOCALAPPDATA% on Windows)
    let root_dir = opts.workspaces_root.clone().unwrap_or_else(crate::platform::paths::workspaces_root);
    let real_root = root_dir.join(&slug);
    fs::create_dir_all(&real_root).with_context(|| format!("{} 생성 실패", real_root.display()))?;

    // refuse a workspace another app was using, so they don't mix
    let marker = real_root.join("ghostdesk-electron.json");
    if let Ok(s) = fs::read_to_string(&marker) {
        if let Ok(v) = serde_json::from_str::<Value>(&s) {
            let prev = v["bundle_id"].as_str().unwrap_or_default();
            ensure!(
                prev.is_empty() || prev == opts.app.bundle_id,
                "작업공간 '{}' 은(는) 다른 앱({prev})용입니다. 다른 작업공간 이름을 쓰세요.",
                opts.workspace
            );
        }
    } else if real_root.read_dir().map(|mut d| d.next().is_some()).unwrap_or(false) {
        bail!("작업공간 폴더 '{}' 가 이미 다른 용도로 쓰이고 있습니다", real_root.display());
    }
    fs::write(&marker, json!({ "bundle_id": opts.app.bundle_id, "app": opts.app.name }).to_string())?;

    let root = short_link(&real_root, &slug)?;
    let p = WorkspacePaths {
        user_data: root.join("user-data"),
        extensions: root.join("extensions"),
        shared_data: root.join("shared-data"),
        agent_plugins: root.join("agent-plugins"),
        xdg: root.join("xdg"),
        log: real_root.join("ghostdesk-electron.log"),
        real_root: real_root.clone(),
        root,
    };
    // VS Code main IPC socket: <user-data>/<4-char version>-main.sock (no limit on Windows, which uses
    // named pipes)
    #[cfg(unix)]
    {
        let sock_len = p.user_data.as_os_str().len() + "/1.99-main.sock".len();
        ensure!(
            sock_len < SUN_PATH_MAX,
            "작업공간 경로가 너무 깁니다 (소켓 경로 {sock_len}바이트 ≥ {SUN_PATH_MAX})"
        );
    }

    match &opts.profile {
        ElectronProfile::Empty => {}
        ElectronProfile::Clone { source, refresh } => {
            let real_ud = real_root.join("user-data");
            // Windows: clone-in-progress marker (outside user-data so it doesn't end up in the clone). Keeps
            // the next run from mistaking user-data left by an unfinished clone (failed on locked cookies and
            // then failed to delete, copy error, forced exit, …) for a complete one and launching a
            // logged-out half copy (same approach as the browser clone in agent.rs).
            let mark = real_root.join(".ghostdesk-cloning");
            let unfinished = cfg!(windows) && mark.exists();
            let has = !unfinished && real_ud.read_dir().map(|mut d| d.next().is_some()).unwrap_or(false);
            if *refresh || !has {
                if cfg!(windows) {
                    fs::write(&mark, b"").with_context(|| format!("{} 생성 실패", mark.display()))?;
                }
                if real_ud.exists() {
                    // Windows: files held by an app that just exited are released shortly, so retry (once on macOS)
                    let r = crate::platform::fsx::remove_dir_all_retry(&real_ud);
                    #[cfg(windows)]
                    r.with_context(|| {
                        format!(
                            "이전 복제본 폴더를 비우지 못했습니다 (다른 프로그램이 사용 중일 수 있습니다 — 이 작업공간의 앱이 아직 \
                             닫히는 중이면 잠시 후 다시 시도하세요): {}",
                            real_ud.display()
                        )
                    })?;
                    #[cfg(not(windows))]
                    r?;
                }
                let noop = |_: u64, _: u64| {};
                let prog: &dyn Fn(u64, u64) = match &opts.progress {
                    Some(f) => f.as_ref(),
                    None => &noop,
                };
                clone_user_data(source, &real_ud, prog)?;
                if cfg!(windows) {
                    // only removed once the clone completes (if removal fails, the next run just clones again)
                    let _ = fs::remove_file(&mark);
                }
            }
        }
    }
    for d in [&p.user_data, &p.extensions, &p.shared_data, &p.agent_plugins] {
        fs::create_dir_all(d)?;
    }
    if opts.app.family == AppFamily::OpenCode {
        for s in ["data", "config", "cache", "state"] {
            fs::create_dir_all(p.xdg.join(s))?;
        }
    }
    if opts.app.family == AppFamily::VsCode {
        // portable argv.json: crash reporter (a separate crashpad process) off
        let argv = p.root.join("argv.json");
        if !argv.exists() {
            fs::write(&argv, "{\n  \"enable-crash-reporter\": false\n}\n")?;
        }
        write_vscode_settings(&p.user_data.join("User/settings.json"), &opts.vscode_settings)?;
    }
    Ok(p)
}

/// Merges settings into settings.json. Strict JSON is overwritten key by key; if it can't be
/// parsed (comments, …), the settings are inserted right after the opening brace (so the user's
/// existing values win).
fn write_vscode_settings(path: &Path, set: &Map<String, Value>) -> Result<()> {
    if set.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(path.parent().unwrap())?;
    let cur = fs::read_to_string(path).unwrap_or_default();
    let text = if cur.trim().is_empty() {
        serde_json::to_string_pretty(&Value::Object(set.clone()))?
    } else if let Ok(Value::Object(mut m)) = serde_json::from_str::<Value>(&cur) {
        for (k, v) in set {
            m.insert(k.clone(), v.clone());
        }
        serde_json::to_string_pretty(&Value::Object(m))?
    } else if let Some(i) = cur.find('{') {
        let mut ins = String::new();
        for (k, v) in set {
            ins.push_str(&format!("\n  {}: {},", serde_json::to_string(k)?, v));
        }
        format!("{}{}{}", &cur[..=i], ins, &cur[i + 1..])
    } else {
        serde_json::to_string_pretty(&Value::Object(set.clone()))?
    };
    fs::write(path, text)?;
    Ok(())
}

/// Launch arguments and environment variables per app kind.
fn launch_plan(opts: &ElectronLaunch, p: &WorkspacePaths) -> (Vec<String>, Vec<(String, String)>) {
    let d = |x: &Path| x.display().to_string();
    let mut args = vec![
        format!("--user-data-dir={}", d(&p.user_data)),
        "--remote-debugging-port=0".into(),
        // --remote-allow-origins isn't passed: GhostDesk's CDP client sends no Origin header so it isn't
        // needed, and passing it would let web pages open in the user's real browser attach to this app
        // (with its cloned login state)
        "--inspect-brk=0".into(),
        "--disable-renderer-backgrounding".into(),
        "--disable-backgrounding-occluded-windows".into(),
        "--disable-background-timer-throttling".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
    ];
    // Windows: when Chromium's native occlusion tracking reports off-screen/transparent windows as
    // "occluded" and stops rendering, Page.captureScreenshot never answers (VS Code turns the same
    // feature off itself). Only the last occurrence of a switch counts, so --disable-features values
    // from extra_args are merged here (PATCH_JS merges values the app appends at runtime with
    // app.commandLine.appendSwitch).
    let mut extra = opts.extra_args.clone();
    if cfg!(windows) {
        let (features, rest) = split_disable_features(&extra);
        extra = rest;
        args.push(format!("--disable-features={}", merge_features(&features, "CalculateNativeWinOcclusion")));
    }
    let mut env = Vec::new();
    match opts.app.family {
        AppFamily::VsCode => {
            args.extend([
                format!("--extensions-dir={}", d(&p.extensions)),
                format!("--shared-data-dir={}", d(&p.shared_data)),
                "--disable-updates".into(),
                "--disable-telemetry".into(),
                "--disable-crash-reporter".into(),
                "--disable-workspace-trust".into(),
                "--skip-welcome".into(),
                "--skip-release-notes".into(),
                "--new-window".into(),
            ]);
            // VSCODE_PORTABLE takes precedence over --user-data-dir, so set it so user-data ends up at
            // <root>/user-data (argv.json, extensions, shared-data and agent-plugins all go under <root> too)
            env.push(("VSCODE_PORTABLE".into(), d(&p.root)));
        }
        AppFamily::OpenCode => {
            for (k, s) in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state")] {
                env.push((k.into(), d(&p.xdg.join(s))));
            }
        }
        AppFamily::Generic => {}
    }
    args.extend(extra);
    if let Some(op) = &opts.open_path {
        args.push(op.display().to_string());
    }
    (args, env)
}

/// Pulls `--disable-features=…` out of an argument list (Windows Chromium also treats `-`/`/`
/// prefixes and differently cased names as the same switch), splitting into those values and the
/// remaining arguments. Anything after `--` (end of switches) is left alone.
#[cfg_attr(not(windows), allow(dead_code))]
fn split_disable_features(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut values = Vec::new();
    let mut rest = Vec::new();
    let mut ended = false;
    for a in args {
        if !ended && a == "--" {
            ended = true;
        }
        let name_val = a.strip_prefix("--").or_else(|| a.strip_prefix('-')).or_else(|| a.strip_prefix('/'));
        match name_val.and_then(|s| s.split_once('=')) {
            Some((n, v)) if !ended && n.eq_ignore_ascii_case("disable-features") => values.push(v.to_string()),
            _ => rest.push(a.clone()),
        }
    }
    (values, rest)
}

/// Merges comma-separated feature lists and adds `must` (order kept, duplicates removed).
#[cfg_attr(not(windows), allow(dead_code))]
fn merge_features(lists: &[String], must: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for f in lists.iter().flat_map(|l| l.split(',')).chain(std::iter::once(must)) {
        let f = f.trim();
        if !f.is_empty() && !out.contains(&f) {
            out.push(f);
        }
    }
    out.join(",")
}

// ---- main process injection script ----

/// Evaluated on the first line of the app's main script. `__CFG__` is replaced with JSON.
const PATCH_JS: &str = r#"(() => {
  if (globalThis.__ghostdesk) return 'already';
  const R = (typeof process.getBuiltinModule === 'function')
    ? process.getBuiltinModule('module').createRequire(process.execPath)
    : (typeof require === 'function' ? require : process.mainModule.require);
  const electron = R('electron');
  const app = electron.app;
  const CFG = __CFG__;
  const W32 = process.platform === 'win32';
  const G = globalThis.__ghostdesk = { cfg: CFG, electron, notes: [], log: [], wins: 0, blocked: 0, popup: null };
  const note = (s) => { try { G.notes.push(String(s).slice(0, 600)); if (G.notes.length > 200) G.notes.shift(); } catch (e) {} };
  const log = (s) => { try { G.log.push(String(s).slice(0, 300)); if (G.log.length > 300) G.log.shift(); } catch (e) {} };
  const safe = (name, f) => { try { f(); log('ok ' + name); } catch (e) { log('FAIL ' + name + ': ' + e); } };
  const noop = function () {};
  const short = (v) => { try { return (typeof v === 'string' ? v : JSON.stringify(v)).slice(0, 300); } catch (e) { return String(v); } };
  G.note = note;
  G.exit = app.exit.bind(app);
  // if GhostDesk dies abnormally (the watchdog is gone), the hidden app exits on its own
  if (CFG.parentPid > 1) {
    const t = setInterval(() => { try { process.kill(CFG.parentPid, 0); } catch (e) { if (e && e.code === 'ESRCH') G.exit(0); } }, 2000);
    if (t && t.unref) t.unref();
  }

  safe('userData', () => { if (CFG.userData) app.setPath('userData', CFG.userData); });
  // the original function is kept as G.setPolicy: if macOS (LaunchServices) switches the hidden
  // instance to Foreground on a Dock click or the like, the watchdog's rehide(true) sets it back to prohibited.
  safe('policy', () => { if (app.setActivationPolicy) { G.setPolicy = app.setActivationPolicy.bind(app); G.setPolicy('prohibited'); app.setActivationPolicy = noop; } });
  safe('app', () => {
    app.focus = noop;
    app.show = noop;
    app.relaunch = function () { note('앱이 스스로 재시작하려 했지만 막았습니다 (재시작하면 숨김 처리가 풀리기 때문). 필요하면 작업을 다시 시작하세요.'); };
    app.setLoginItemSettings = function () { note('앱의 로그인 항목 등록 시도를 막았습니다.'); };
    app.setAsDefaultProtocolClient = function (p) { note('앱의 URL 스킴(' + p + ') 기본 앱 등록 시도를 막았습니다.'); return false; };
    app.removeAsDefaultProtocolClient = function () { return false; };
    app.moveToApplicationsFolder = function () { return false; };
    app.showAboutPanel = function () { note('정보(About) 패널 표시를 막았습니다.'); };
    app.showEmojiPanel = noop;
    app.hideOthers = noop;
    if (app.dock) { app.dock.show = function () { return Promise.resolve(); }; app.dock.bounce = function () { return -1; }; app.dock.downloadFinished = noop; }
  });
  // Windows: give the hidden instance its own AppUserModelID so its taskbar group, jump list and recent items
  // don't mix with or overwrite the user's real app (same AUMID), and block the taskbar/jump list APIs.
  if (W32) safe('win-shell', () => {
    const oAumid = app.setAppUserModelId;
    if (typeof oAumid === 'function' && CFG.aumid) {
      oAumid.call(app, CFG.aumid);
      app.setAppUserModelId = function () { try { oAumid.call(app, CFG.aumid); } catch (e) {} };
    }
    app.setJumpList = function () { G.blocked++; return 'ok'; };
    app.setUserTasks = function () { G.blocked++; return true; };
    app.addRecentDocument = noop;
    app.clearRecentDocuments = noop;
    app.setBadgeCount = function () { return false; };
  });
  /*<gd-win-helpers>*/
  // Windows: Node's child_process defaults windowsHide to false, so launching a console program (git, python,
  // cmd, …) opens a new console (or Windows Terminal) window on the user's screen. Default windowsHide to true.
  // a: args array, hasArgs: functions whose second arg may be an args array, nullOk: functions treating null options as absent
  const gdHideOpts = (a, hasArgs, nullOk) => {
    a = Array.prototype.slice.call(a);
    const k = hasArgs && (Array.isArray(a[1]) || a[1] == null) ? 2 : 1;
    const o = a[k];
    if (typeof o === 'function') a.splice(k, 0, { windowsHide: true });
    else if (o === undefined || (o === null && nullOk)) { while (a.length < k) a.push(undefined); a[k] = { windowsHide: true }; }
    else if (o !== null && typeof o === 'object' && !Array.isArray(o) && o.windowsHide === undefined) a[k] = Object.assign({}, o, { windowsHide: true });
    return a;
  };
  const gdWrapCp = (cp) => {
    const P = Symbol.for('nodejs.util.promisify.custom');
    const spec = { spawn: [true, false], spawnSync: [true, false], execFile: [true, true], execFileSync: [true, true], fork: [true, true], exec: [false, true], execSync: [false, true] };
    for (const name of Object.keys(spec)) {
      const orig = cp[name];
      if (typeof orig !== 'function' || orig.__gdHide) continue;
      const [hasArgs, nullOk] = spec[name];
      const w = function (...a) { return orig.apply(this, gdHideOpts(a, hasArgs, nullOk)); };
      for (const key of Reflect.ownKeys(orig)) {
        if (key === 'length' || key === 'prototype' || key === P) continue;
        try { Object.defineProperty(w, key, Object.getOwnPropertyDescriptor(orig, key)); } catch (e) {}
      }
      // util.promisify(exec/execFile) uses a dedicated version returning { stdout, stderr }: rebuild it through the wrapper
      if (orig[P]) {
        Object.defineProperty(w, P, { enumerable: false, value: (...a) => {
          let child;
          const p = new Promise((res, rej) => { child = w(...a, (err, stdout, stderr) => { if (err !== null && err !== undefined) { err.stdout = stdout; err.stderr = stderr; rej(err); } else res({ stdout, stderr }); }); });
          p.child = child;
          return p;
        } });
      }
      Object.defineProperty(w, '__gdHide', { value: true });
      cp[name] = w;
    }
    return cp;
  };
  // Windows: only the last --disable-features counts, so merge to keep CalculateNativeWinOcclusion even if the app changes it
  const gdMergeFeatures = (v) => {
    const out = [];
    for (const f of String(v == null ? '' : v).split(',').concat(['CalculateNativeWinOcclusion'])) { const t = f.trim(); if (t && !out.includes(t)) out.push(t); }
    return out.join(',');
  };
  // Windows: a drag from the hidden app that becomes an OS drag (OLE DoDragDrop) drops onto the window under the user's
  // real cursor (the real mouse button isn't down, so it ends in an immediate drop). GhostDesk intercepts in-page drags
  // with Input.setInterceptDrags on the app window's CDP session, but these two paths are out of its reach:
  //  (a) webContents.startDrag({file, icon}): native file drags invoked over IPC from the renderer's dragstart. Blocked.
  //  (b) <webview> guests: a separate frame tree, so GhostDesk's session interception doesn't apply. The in-app debugger
  //      enables interception in the guest too and cancels intercepted drags (drag and drop inside the guest won't work).
  //      If interception can't be enabled, dragstart is blocked in the guest document.
  const gdGuardDrag = (wc, note, log) => {
    const blocked = function () { note('앱의 OS 드래그(webContents.startDrag)를 막았습니다 — 숨은 데스크에서는 사용자 화면으로 끌어다 놓을 수 없습니다. 파일은 앱 안에서 다른 방법(복사·저장)으로 옮기세요.'); };
    wc.startDrag = blocked;
    try { const P = Object.getPrototypeOf(wc); if (P && typeof P.startDrag === 'function' && !P.__gdNoDrag) { P.startDrag = blocked; Object.defineProperty(P, '__gdNoDrag', { value: true }); } } catch (e) {}
    let type = '';
    try { type = typeof wc.getType === 'function' ? String(wc.getType()) : ''; } catch (e) {}
    if (type !== 'webview') return Promise.resolve('start');
    let fallback = false;
    const noDragScript = "addEventListener('dragstart', (e) => e.preventDefault(), true)";
    const useFallback = (why) => {
      log('webview drag intercept ' + why);
      if (fallback) return;
      fallback = true;
      note('웹뷰(<webview>) 안의 드래그를 가로챌 수 없어 그 안의 끌어다 놓기를 막았습니다: ' + why);
      const inject = () => { try { if (!wc.isDestroyed()) Promise.resolve(wc.executeJavaScript(noDragScript)).catch(() => {}); } catch (e) {} };
      try { wc.on('dom-ready', inject); } catch (e) {}
      inject();
    };
    let d;
    const enable = () => {
      if (!d.isAttached()) d.attach('1.3');
      return Promise.resolve(d.sendCommand('Input.setInterceptDrags', { enabled: true })).then(() => 'webview', (e) => { useFallback(String(e)); return 'fallback'; });
    };
    try {
      d = wc.debugger;
      d.on('message', (_e, method, params) => {
        if (method !== 'Input.dragIntercepted') return;
        note('웹뷰(<webview>) 안에서 시작된 드래그를 취소했습니다 — 숨은 데스크에서는 웹뷰 안의 끌어다 놓기를 쓸 수 없습니다.');
        const data = (params && params.data) || { items: [], dragOperationsMask: 1 };
        try { Promise.resolve(d.sendCommand('Input.dispatchDragEvent', { type: 'dragCancel', x: 0, y: 0, data })).catch(() => {}); } catch (e) {}
      });
      // reattach if detached while the guest is still alive (after one failure, fall back to blocking dragstart)
      d.on('detach', (_e, reason) => {
        try { if (reason === 'target closed' || wc.isDestroyed()) return; } catch (e) { return; }
        try { enable().catch(() => {}); } catch (e) { useFallback('detach ' + reason + ': ' + e); }
      });
      return enable();
    } catch (e) {
      useFallback(String(e));
      return Promise.resolve('fallback');
    }
  };
  /*</gd-win-helpers>*/
  if (W32) safe('child-process', () => {
    gdWrapCp(R('child_process'));
    // also applies to ESM's import { spawn } from 'child_process'
    try { R('module').syncBuiltinESMExports(); } catch (e) {}
  });
  if (W32) safe('features', () => {
    const cl = app.commandLine, oAppend = cl.appendSwitch, oRemove = cl.removeSwitch;
    const isDF = (n) => String(n).replace(/^-+/, '').toLowerCase() === 'disable-features';
    cl.appendSwitch = function (name, value) {
      if (isDF(name)) return oAppend.call(cl, 'disable-features', gdMergeFeatures(value));
      return oAppend.apply(cl, arguments);
    };
    if (typeof oRemove === 'function') cl.removeSwitch = function (name) {
      const r = oRemove.apply(cl, arguments);
      if (isDF(name)) { try { oAppend.call(cl, 'disable-features', gdMergeFeatures('')); } catch (e) {} }
      return r;
    };
  });
  // the hidden app shares the user's app bundle ID, so when the user opens a file/URL with this app from Finder/Dock,
  // macOS may route it to the hidden instance (measured: a Dock click activated the hidden instance).
  // open-file/open-url events aren't passed to the app, so the user's files never open on the agent's screen.
  safe('open-events', () => {
    const oEmit = app.emit;
    app.emit = function (ev, ...a) {
      if (ev === 'open-file' || ev === 'open-url') {
        try { if (a[0] && typeof a[0].preventDefault === 'function') a[0].preventDefault(); } catch (e) {}
        note('macOS 가 사용자의 ' + (ev === 'open-file' ? '파일' : 'URL') + ' 열기 요청을 숨은 앱으로 보냈지만 무시했습니다 (사용자 앱과 번들 ID 가 같아서 생기는 일): ' + short(a[1]));
        return false;
      }
      return oEmit.call(this, ev, ...a);
    };
  });

  // ---- windows: neutralize show/focus, transparent, click-through, unfocusable and hidden from creation ----
  const BaseW = electron.BaseWindow, BW = electron.BrowserWindow;
  const BP = (BaseW && BaseW.prototype) || BW.prototype;
  const pick = (m) => (BW.prototype[m] || BP[m]);
  const orig = {};
  for (const m of ['hide', 'showInactive', 'isVisible', 'setOpacity', 'getOpacity', 'setIgnoreMouseEvents', 'setFocusable', 'setContentSize', 'getContentSize', 'setPosition']) orig[m] = pick(m);
  if (W32) for (const m of ['setSkipTaskbar', 'setMenuBarVisibility', 'setAutoHideMenuBar', 'isMenuBarVisible']) orig[m] = pick(m);
  G.orig = orig;
  const AllW = () => (BaseW ? BaseW.getAllWindows() : BW.getAllWindows());
  safe('windows', () => {
    const block = ['show', 'showInactive', 'focus', 'moveTop', 'maximize', 'unmaximize', 'minimize', 'restore', 'setFullScreen',
      'setSimpleFullScreen', 'setKiosk', 'flashFrame', 'setAlwaysOnTop', 'setVisibleOnAllWorkspaces', 'focusOnWebView',
      'setBounds', 'setSize', 'setContentSize', 'setContentBounds', 'setPosition', 'center',
      'setOpacity', 'setIgnoreMouseEvents', 'setFocusable', 'moveAbove', 'setParentWindow'];
    // Windows: taskbar buttons, thumbnails, progress, overlay icons, per-window AUMID, re-enabling the menu bar
    if (W32) block.push('setSkipTaskbar', 'setMenuBarVisibility', 'setAutoHideMenuBar', 'setThumbarButtons', 'setOverlayIcon',
      'setProgressBar', 'setAppDetails', 'setThumbnailClip', 'setThumbnailToolTip');
    for (const P of [BP, BW.prototype]) for (const m of block) {
      if (typeof P[m] === 'function') P[m] = function () { G.blocked++; };
    }
  });
  const ghost = (w) => {
    try { orig.setOpacity.call(w, 0); } catch (e) {}
    try { orig.setIgnoreMouseEvents.call(w, true); } catch (e) {}
    try { if (orig.setFocusable) orig.setFocusable.call(w, false); } catch (e) {}
    if (W32) {
      // off the taskbar and Alt+Tab (along with unfocusable = WS_EX_NOACTIVATE), and the in-window menu bar hidden.
      // with auto-hide off, pressing Alt alone doesn't bring up the menu bar (menus go through the app_menu tool).
      try { if (orig.setSkipTaskbar) orig.setSkipTaskbar.call(w, true); } catch (e) {}
      try { if (orig.setAutoHideMenuBar) orig.setAutoHideMenuBar.call(w, false); } catch (e) {}
      try { if (orig.setMenuBarVisibility) orig.setMenuBarVisibility.call(w, false); } catch (e) {}
    }
    if (CFG.mode === 'hide') { try { orig.hide.call(w); } catch (e) {} }
    else {
      // ghost: "show" it at off-screen coordinates, transparent (alpha 0), click-through and inactive, so rendering continues
      try { orig.setPosition.call(w, CFG.offX, CFG.offY); } catch (e) {}
      try { if (!orig.isVisible.call(w)) orig.showInactive.call(w); } catch (e) {}
    }
  };
  const done = new WeakSet();
  const setup = (w) => {
    if (done.has(w)) return;
    done.add(w);
    G.wins++;
    ghost(w);
    try { orig.setContentSize.call(w, CFG.width, CFG.height); } catch (e) { log('size ' + e); }
    let wc = null;
    try { wc = w.webContents; } catch (e) {}
    if (wc) {
      try { wc.setBackgroundThrottling(false); } catch (e) {}
    }
    // macOS moves the position back on screen right after show, so send it off-screen once more a moment later
    w.on('show', () => { ghost(w); log('show-event'); for (const t of [30, 150, 400]) setTimeout(() => { try { if (!w.isDestroyed()) ghost(w); } catch (e) {} }, t); });
  };
  // BaseWindow.prototype._init is called inside the native constructor before InitFromOptions (which handles show).
  // setting transparent/click-through/unfocusable here keeps even windows created with show:true invisible from the start.
  safe('init-hook', () => {
    const oInit = BP._init;
    if (typeof oInit === 'function') BP._init = function (...a) { const r = oInit.apply(this, a); try { setup(this); } catch (e) { log('init setup ' + e); } return r; };
  });
  app.on('browser-window-created', (_e, w) => safe('setup', () => setup(w)));
  G.rehide = (deactivate) => {
    let n = 0;
    for (const w of AllW()) { try { if (!w.isDestroyed()) { ghost(w); n++; } } catch (e) {} }
    if (deactivate) { try { if (G.setPolicy) G.setPolicy('prohibited'); } catch (e) {} }
    // Windows: if our window takes the foreground, give up focus (the next window in Z order = the user's previous window is reactivated)
    if (deactivate && W32) { for (const w of AllW()) { try { if (!w.isDestroyed() && w.isFocused()) w.blur(); } catch (e) {} } }
    if (deactivate || CFG.mode === 'hide') { try { app.hide(); } catch (e) {} }
    return n;
  };
  // Windows can be off by about 1px converting between DIP and real pixels
  const offPos = (a, t) => W32 ? Math.abs(a - t) > 2 : a !== t;
  G.maintain = () => {
    const out = { wins: 0, visible: 0, resized: 0, notes: G.notes.splice(0) };
    for (const w of AllW()) {
      try {
        if (w.isDestroyed()) continue;
        out.wins++;
        if (orig.getOpacity && orig.getOpacity.call(w) !== 0) orig.setOpacity.call(w, 0);
        if (CFG.mode === 'hide' && orig.isVisible.call(w)) { out.visible++; ghost(w); }
        if (W32) { try { if (orig.isMenuBarVisible && orig.isMenuBarVisible.call(w)) orig.setMenuBarVisibility.call(w, false); } catch (e) {} }
        if (CFG.mode !== 'hide') {
          if (!orig.isVisible.call(w)) { out.reshown = (out.reshown || 0) + 1; ghost(w); }
          const b = w.getBounds();
          if (offPos(b.x, CFG.offX) || offPos(b.y, CFG.offY)) { orig.setPosition.call(w, CFG.offX, CFG.offY); out.moved = (out.moved || 0) + 1; }
        }
        const s = orig.getContentSize.call(w);
        if (s[0] !== CFG.width || s[1] !== CFG.height) { orig.setContentSize.call(w, CFG.width, CFG.height); out.resized++; }
      } catch (e) {}
    }
    return out;
  };

  // Windows: keys that open native menus (menu bar mnemonics, F10, the context menu key) aren't passed to the app.
  // menu popup windows can get pulled into a monitor and show on the user's screen even when the parent is off-screen.
  // (GhostDesk refuses these combos too; this is a second line of defense.)
  // mnemonics only open when the modifiers are exactly Alt, so Shift/Win+Alt+letter shortcuts (Shift+Alt+F, …) pass through.
  const menuKey = (i) => {
    if (!i || i.control) return false;
    const k = String(i.key || ''), c = String(i.code || '');
    if (k === 'Alt' || k === 'F10' || k === 'ContextMenu' || c === 'ContextMenu') return true;
    return !!i.alt && ((!i.shift && !i.meta && /^Key[A-Z]$/.test(c)) || c === 'Space');
  };
  // ---- webContents: no devtools windows ----
  app.on('web-contents-created', (_e, wc) => safe('wc', () => {
    wc.openDevTools = function () { note('개발자 도구 창 열기를 막았습니다.'); };
    wc.toggleDevTools = function () { note('개발자 도구 창 열기를 막았습니다.'); };
    try { wc.setBackgroundThrottling(false); } catch (e) {}
    if (W32) wc.on('before-input-event', (ev, i) => { if (menuKey(i)) { ev.preventDefault(); G.blocked++; } });
    if (W32) gdGuardDrag(wc, note, log).then((r) => log('drag-guard ' + r), (e) => log('FAIL drag-guard ' + e));
  }));

  // ---- session permissions: deny notifications, camera/mic, location, screen sharing, external protocols (no TCC dialogs/banners) ----
  const BLOCK = new Set(['notifications', 'media', 'geolocation', 'midi', 'midiSysex', 'openExternal', 'display-capture', 'idle-detection']);
  const wrapSession = (s) => safe('session', () => {
    const SP = Object.getPrototypeOf(s);
    if (!SP.__gdWrapped) {
      SP.__gdWrapped = true;
      const oReq = SP.setPermissionRequestHandler, oChk = SP.setPermissionCheckHandler, oDisp = SP.setDisplayMediaRequestHandler;
      SP.setPermissionRequestHandler = function (h) {
        return oReq.call(this, (wc, perm, cb, det) => {
          if (BLOCK.has(perm)) { note('앱의 권한 요청(' + perm + ')을 거부했습니다.'); return cb(false); }
          if (typeof h === 'function') return h(wc, perm, cb, det);
          return cb(true);
        });
      };
      SP.setPermissionCheckHandler = function (h) {
        return oChk.call(this, (wc, perm, origin, det) => {
          if (BLOCK.has(perm)) return false;
          if (typeof h === 'function') return h(wc, perm, origin, det);
          return true;
        });
      };
      if (oDisp) SP.setDisplayMediaRequestHandler = function () {
        return oDisp.call(this, (_req, cb) => { note('화면 공유(캡처) 요청을 거부했습니다.'); cb({}); });
      };
    }
    s.setPermissionRequestHandler(null);
    s.setPermissionCheckHandler(null);
    if (s.setDisplayMediaRequestHandler) s.setDisplayMediaRequestHandler(null);
  });
  app.on('session-created', wrapSession);
  app.whenReady().then(() => { try { wrapSession(electron.session.defaultSession); } catch (e) {} });

  // ---- native dialogs: recorded, never shown ----
  safe('dialog', () => {
    const D = electron.dialog;
    const opt = (a) => { for (let i = a.length - 1; i >= 0; i--) { const x = a[i]; if (x && typeof x === 'object' && typeof x.getBounds !== 'function') return x; } return {}; };
    const cancelIdx = (o) => {
      if (typeof o.cancelId === 'number') return o.cancelId;
      const b = o.buttons || [];
      const i = b.findIndex(t => /cancel|취소|^no$|아니|close|닫기/i.test(String(t)));
      return i >= 0 ? i : (b.length > 1 ? b.length - 1 : 0);
    };
    const never = () => new Promise(() => {});
    D.showMessageBox = function (...a) { const o = opt(a); note('앱이 메시지 대화상자를 띄우려 해서 막았습니다 (응답하지 않은 상태로 둠): ' + short([o.title, o.message, o.detail, o.buttons].filter(Boolean))); return never(); };
    D.showMessageBoxSync = function (...a) { const o = opt(a); const i = cancelIdx(o); note('앱의 메시지 대화상자(동기)를 막고 "' + ((o.buttons || [])[i] || i) + '" 로 응답했습니다: ' + short([o.title, o.message, o.detail].filter(Boolean))); return i; };
    D.showErrorBox = function (t, c) { note('앱의 오류 대화상자를 막았습니다: ' + short([t, c])); };
    D.showOpenDialog = function (...a) { const o = opt(a); note('파일 열기 대화상자를 막고 취소로 처리했습니다' + (o.title ? ' (' + o.title + ')' : '') + '. 경로는 앱 안의 입력창으로 입력하세요.'); return Promise.resolve({ canceled: true, filePaths: [], bookmarks: [] }); };
    D.showOpenDialogSync = function () { note('파일 열기 대화상자(동기)를 막았습니다.'); return undefined; };
    D.showSaveDialog = function (...a) { const o = opt(a); note('파일 저장 대화상자를 막고 취소로 처리했습니다' + (o.title ? ' (' + o.title + ')' : '') + '.'); return Promise.resolve({ canceled: true, filePath: '' }); };
    D.showSaveDialogSync = function () { note('파일 저장 대화상자(동기)를 막았습니다.'); return undefined; };
    D.showCertificateTrustDialog = function () { note('인증서 신뢰 대화상자를 막았습니다.'); return never(); };
  });
  // ---- block opening external apps/browsers ----
  safe('shell', () => {
    const S = electron.shell;
    S.openExternal = function (url) { note('외부 링크 열기를 막았습니다 (사용자의 실제 브라우저가 열리기 때문): ' + short(url)); return Promise.resolve(); };
    S.openPath = function (p) { note('파일/폴더를 외부 앱으로 여는 것을 막았습니다: ' + short(p)); return Promise.resolve('GhostDesk 가 차단함'); };
    S.showItemInFolder = function (p) { note((W32 ? '파일 탐색기' : 'Finder') + ' 에서 보기를 막았습니다: ' + short(p)); };
    S.beep = noop;
    if (W32) S.writeShortcutLink = function (p) { note('바로 가기(.lnk) 만들기를 막았습니다: ' + short(p)); return false; };
  });
  // ---- native context menus: kept, not shown (pick with electron_menu) ----
  safe('menu', () => {
    const M = electron.Menu;
    M.prototype.popup = function (o) {
      G.popup = this;
      const labels = this.items.filter(i => i.visible !== false && i.type !== 'separator').map(i => i.label || i.role).filter(Boolean).slice(0, 30);
      note('앱이 컨텍스트 메뉴를 열려고 했습니다 (화면에 표시하지 않음). 항목: ' + labels.join(' | '));
      try { if (o && typeof o.callback === 'function') setTimeout(o.callback, 0); } catch (e) {}
    };
    M.prototype.closePopup = noop;
  });
  safe('notification', () => { electron.Notification.prototype.show = function () { note('알림 표시를 막았습니다: ' + short([this.title, this.body])); }; });
  safe('globalShortcut', () => {
    const g = electron.globalShortcut;
    g.register = function (acc) { note('전역 단축키(' + acc + ') 등록을 막았습니다.'); return false; };
    g.registerAll = function () { return false; };
  });
  safe('autoUpdater', () => { const u = electron.autoUpdater; u.checkForUpdates = noop; u.quitAndInstall = noop; u.setFeedURL = noop; });
  safe('systemPreferences', () => {
    const sp = electron.systemPreferences;
    if (sp.askForMediaAccess) sp.askForMediaAccess = function () { note('카메라/마이크 권한 요청을 거부했습니다.'); return Promise.resolve(false); };
    if (sp.promptTouchID) sp.promptTouchID = function () { return Promise.reject(new Error('GhostDesk 가 차단함')); };
  });
  safe('inAppPurchase', () => { const p = electron.inAppPurchase; if (p) { p.purchaseProduct = function () { note('앱 내 구매 시도를 막았습니다.'); return Promise.resolve(false); }; } });
  safe('desktopCapturer', () => { electron.desktopCapturer.getSources = function () { note('화면 캡처 소스 요청을 거부했습니다.'); return Promise.resolve([]); }; });
  // ---- tray (menu bar icon): removed on the first call ----
  safe('tray', () => {
    const T = electron.Tray, P = T.prototype;
    const destroy = P.destroy;
    const kill = (t) => { if (!t.__gdKilled) { t.__gdKilled = true; try { destroy.call(t); } catch (e) {} note('트레이(' + (W32 ? '알림 영역' : '메뉴 막대') + ') 아이콘을 제거했습니다.'); } };
    for (const k of Object.getOwnPropertyNames(P)) {
      if (k === 'constructor' || typeof P[k] !== 'function') continue;
      P[k] = function () { kill(this); if (k === 'isDestroyed') return false; if (k === 'getBounds') return { x: 0, y: 0, width: 0, height: 0 }; return undefined; };
    }
    const EE = Object.getPrototypeOf(P);
    for (const k of ['on', 'once', 'addListener']) P[k] = function (...a) { kill(this); return EE[k].apply(this, a); };
  });
  return G.log.filter(l => l.startsWith('FAIL')).join('; ') || 'ok';
})()"#;

/// Menu list/click script (evaluated in the inspector).
const MENU_JS: &str = r#"((src, path, targetId, allowed) => {
  const G = globalThis.__ghostdesk; const e = G.electron;
  let m = src === 'context' ? G.popup : e.Menu.getApplicationMenu();
  if (!m) return { error: src === 'context' ? '최근 컨텍스트 메뉴가 없습니다' : '앱 메뉴가 없습니다' };
  const lab = (i) => (i.label || '').replace(/&/g, '').trim();
  if (!path) {
    const walk = (menu, d) => menu.items.filter(i => i.visible !== false).map(i => i.type === 'separator' ? '  '.repeat(d) + '──' :
      '  '.repeat(d) + (lab(i) || '(' + (i.role || '') + ')') + (i.enabled === false ? ' [비활성]' : '') + (i.checked ? ' ✓' : '') +
      (i.accelerator ? '  ⌨ ' + i.accelerator : '') + (i.submenu ? '\n' + walk(i.submenu, d + 1).join('\n') : ''));
    return { text: walk(m, 0).join('\n') };
  }
  let item = null;
  for (let k = 0; k < path.length; k++) {
    const want = path[k].trim().toLowerCase();
    const items = m.items.filter(i => i.visible !== false && i.type !== 'separator');
    item = items.find(i => lab(i).toLowerCase() === want) || items.find(i => (i.role || '').toLowerCase() === want) || items.find(i => lab(i).toLowerCase().includes(want));
    if (!item) return { error: '메뉴 항목을 찾지 못했습니다: ' + path[k] };
    if (k < path.length - 1) { if (!item.submenu) return { error: '하위 메뉴가 없습니다: ' + path[k] }; m = item.submenu; }
  }
  if (item.submenu) return { error: '하위 메뉴가 있는 항목입니다. 더 깊은 경로를 지정하세요.' };
  if (item.enabled === false) return { error: '비활성 항목입니다: ' + lab(item) };
  const role = (item.role || '').toLowerCase();
  if (role && !allowed.includes(role)) return { error: 'GhostDesk 가 허용하지 않는 메뉴 역할입니다 (화면/다른 앱에 영향): ' + role };
  let win = null;
  try { const wc = targetId && e.webContents.fromDevToolsTargetId ? e.webContents.fromDevToolsTargetId(targetId) : null; win = wc ? e.BrowserWindow.fromWebContents(wc) : null; } catch (err) {}
  win = win || e.BrowserWindow.getAllWindows()[0] || null;
  item.click(undefined, win, win ? win.webContents : undefined);
  return { clicked: lab(item) || role };
})"#;

/// Roles allowed to run through a menu click (other roles could affect the screen or other apps,
/// so they're refused).
const ALLOWED_MENU_ROLES: &[&str] = &[
    "undo", "redo", "cut", "copy", "paste", "pasteandmatchstyle", "delete", "selectall", "reload", "forcereload",
    "resetzoom", "zoomin", "zoomout", "close", "quit",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuSource {
    /// The app menu bar (Menu.getApplicationMenu).
    App,
    /// The context menu the app last tried to open.
    Context,
}

// ---- workspace ----

#[derive(Clone, Debug, Default)]
pub struct WatchStats {
    pub ticks: u64,
    pub violations: u64,
    pub rehides: u64,
    pub killed: bool,
    pub last_violation: Option<String>,
    /// Number of windows the inspector maintain loop found and re-hid.
    pub maintain_rehidden: u64,
}

pub struct ElectronWorkspace {
    inner: Arc<BrowserWorkspace>,
    insp: Arc<Cdp>,
    pub app: ElectronApp,
    pub paths: WorkspacePaths,
    pub main_pid: i32,
    pub pgid: i32,
    child: Arc<Mutex<Option<Child>>>,
    notes: Arc<Mutex<Vec<String>>>,
    closed: Arc<AtomicBool>,
    stats: Arc<Mutex<WatchStats>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub width: u32,
    pub height: u32,
}

/// Guard that cleans up the process group and reaps the child on failure.
struct GroupGuard {
    pgid: i32,
    main_pid: i32,
    child: Arc<Mutex<Option<Child>>>,
    armed: bool,
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            terminate_and_reap(self.main_pid, self.pgid, None, &self.child);
        }
    }
}

/// Flag stopping the pre-watcher (set in Drop so it stops however the function exits).
struct StopFlag(Arc<AtomicBool>);

impl Drop for StopFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Checks whether the child process has exited **without reaping it**: while it's a zombie, its
/// pid and pgid can't be reused by another process, so a later kill_tree can't hit some unrelated
/// user process.
///
/// Reaping only happens in `terminate_and_reap`, after kill_tree.
fn peek_exit(c: &Child) -> Option<String> {
    #[cfg(unix)]
    {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::waitid(libc::P_PID, c.id() as libc::id_t, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT)
        };
        if r != 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                return None;
            }
            return Some("종료됨".into());
        }
        if info.si_pid == 0 {
            return None;
        }
        Some(if info.si_code == libc::CLD_EXITED {
            format!("exit status: {}", info.si_status)
        } else {
            format!("signal: {}", info.si_status)
        })
    }
    // Windows: the pid can't be reused while Child holds the process handle, so the same invariant
    // holds
    #[cfg(windows)]
    {
        crate::platform::proc::peek_exit(c).map(|code| {
            if (0..0x10000).contains(&code) {
                format!("exit status: {code}")
            } else {
                format!("exit status: {:#010x}", code as u32)
            }
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = c;
        None
    }
}

/// Whether the not-yet-reaped child is alive.
fn child_alive(child: &Mutex<Option<Child>>) -> bool {
    child.lock().as_ref().map(|c| peek_exit(c).is_none()).unwrap_or(false)
}

/// Kills the process group/descendants only if the child hasn't been reaped yet (= the pid/pgid
/// can't have been reused).
///
/// (Windows: ends descendants through the Job object. Child stays in the Option, so its handle is
/// kept too.)
fn kill_group_if_held(main_pid: i32, pgid: i32, child: &Mutex<Option<Child>>) {
    #[cfg(not(windows))]
    {
        let g = child.lock();
        if g.is_some() {
            kill_tree(main_pid, pgid, None);
        }
    }
    #[cfg(windows)]
    {
        let _ = (main_pid, pgid);
        if let Some(c) = child.lock().as_mut() {
            crate::platform::proc::kill_group(c);
        }
    }
}

/// Kills the process group/descendants, then reaps the child. Does nothing if already reaped
/// (after reaping, main_pid/pgid may have been reused by other processes, so never kill then).
fn terminate_and_reap(main_pid: i32, pgid: i32, extra: Option<&HashSet<i32>>, child: &Mutex<Option<Child>>) {
    let mut g = child.lock();
    if let Some(mut c) = g.take() {
        #[cfg(not(windows))]
        {
            kill_tree(main_pid, pgid, extra);
            let _ = c.kill();
            let _ = c.wait();
        }
        // Windows: end the Job (TerminateJobObject + close handle = KILL_ON_JOB_CLOSE) → kill the main
        // process → reap
        #[cfg(windows)]
        {
            let _ = (main_pid, pgid, extra);
            crate::platform::proc::kill_group(&mut c);
        }
    }
}

/// Pre-watcher: before the patch is injected there's no way to re-hide windows, so the moment a
/// visible window of our process (alpha>0, onscreen) or a front state is seen, the process group is
/// killed (40 ms interval).
///
/// A safety net for when the app runs unpatched because the inspector didn't open or couldn't pause
/// on the main script (a misread fuse, …).
fn spawn_prewatch(
    main_pid: i32,
    pgid: i32,
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<AtomicBool>,
    tripped: Arc<Mutex<Option<String>>>,
    side: WatchSide,
) {
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            if let Some(desc) = scan(main_pid, pgid, &side) {
                kill_group_if_held(main_pid, pgid, &child);
                eprintln!("[ghostdesk] Electron 사전 감시자: {desc} → 앱 강제 종료");
                *tripped.lock() = Some(desc);
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    });
}

#[derive(Default)]
struct Endpoints {
    node: Option<String>,
    devtools: Option<String>,
}

/// Launches the Electron app in hidden mode and connects over CDP.
pub async fn launch(opts: ElectronLaunch, sink: FrameSink) -> Result<Arc<ElectronWorkspace>> {
    ensure!(cfg!(any(target_os = "macos", windows)), "Electron 숨김 실행은 macOS·Windows 에서만 지원합니다");
    let app = opts.app.clone();
    if let Some(why) = blocked_reason(&app.bundle_id, &app.exe) {
        bail!("{} 은(는) 실행할 수 없습니다: {why}", app.name);
    }
    #[cfg(windows)]
    ensure!(
        !win_apps::is_msix_path(&app.exe),
        "{} 은(는) Microsoft Store(MSIX) 패키지 앱이라 숨김 모드로 실행할 수 없습니다",
        app.name
    );
    ensure!(
        app.hideable,
        "{} 은(는) 숨김 모드를 지원하지 않습니다: {}",
        app.name,
        app.reason.clone().unwrap_or_default()
    );
    ensure!(opts.width >= 200 && opts.height >= 200, "화면 크기가 너무 작습니다");

    let o2 = opts.clone();
    let paths = tokio::task::spawn_blocking(move || prepare_workspace(&o2)).await??;
    let (args, env) = launch_plan(&opts, &paths);

    let mut log = fs::OpenOptions::new().create(true).append(true).open(&paths.log)?;
    let _ = writeln!(log, "\n===== GhostDesk Electron 실행: {} {:?}", app.exe.display(), args);
    let mut cmd = Command::new(&app.exe);
    cmd.args(&args);
    // clear inherited environment variables that could attach to the user's other VS Code/Electron
    // instances
    for (k, _) in std::env::vars_os() {
        let ks = k.to_string_lossy();
        // Windows environment variable names are case-insensitive (Node_Options is read as NODE_OPTIONS)
        #[cfg(windows)]
        let ks = std::borrow::Cow::<str>::Owned(ks.to_ascii_uppercase());
        if ks.starts_with("VSCODE_") || ks.starts_with("ELECTRON_") || ks.starts_with("NODE_") || ks == "CHROME_DESKTOP" {
            cmd.env_remove(&k);
        }
    }
    for (k, v) in &env {
        cmd.env(k, v);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.current_dir(&paths.real_root);
    cmd.stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::piped());
    #[cfg(not(windows))]
    let mut child = cmd.spawn().with_context(|| format!("앱 실행 실패: {}", app.exe.display()))?;
    // Windows: start suspended, put it in the Job object, then resume (so every Chromium/Node
    // descendant lands in the same Job)
    #[cfg(windows)]
    let mut child =
        crate::platform::proc::spawn_group(&mut cmd).with_context(|| format!("앱 실행 실패: {}", app.exe.display()))?;
    let main_pid = child.id() as i32;
    let pgid = main_pid;
    // without a Job, descendant windows can't be watched or cleaned up together, so don't launch
    #[cfg(windows)]
    if crate::platform::proc::group_pids(main_pid as u32).is_none() {
        crate::platform::proc::kill_group(&mut child);
        bail!("앱 프로세스를 Job 객체에 넣지 못해 안전하게 숨김 실행할 수 없습니다 (하위 프로세스 감시·정리 불가)");
    }
    let stderr = child.stderr.take();
    let child = Arc::new(Mutex::new(Some(child)));
    let mut guard = GroupGuard { pgid, main_pid, child: child.clone(), armed: true };
    // pre-watcher guarding against exposure before the patch and main watchdog are ready (stops when
    // launch finishes)
    let prewatch_tripped: Arc<Mutex<Option<String>>> = Default::default();
    let prewatch_stop = StopFlag(Arc::new(AtomicBool::new(false)));
    // things the watchdog tells the agent (Windows: hidden console windows, new Windows Terminal
    // windows), through the same channel as workspace notes
    let notes: Arc<Mutex<Vec<String>>> = Default::default();
    let side = WatchSide::new(notes.clone());
    spawn_prewatch(main_pid, pgid, child.clone(), prewatch_stop.0.clone(), prewatch_tripped.clone(), side.clone());
    let tripped = || prewatch_tripped.lock().clone();

    // stderr → log file, extracting debugger/DevTools addresses
    let eps: Arc<Mutex<Endpoints>> = Default::default();
    if let Some(err) = stderr {
        let eps2 = eps.clone();
        let mut log2 = log.try_clone()?;
        std::thread::spawn(move || {
            let rd = std::io::BufReader::new(err);
            for line in rd.split(b'\n').map_while(|l| l.ok()) {
                let line = String::from_utf8_lossy(&line).into_owned();
                let _ = writeln!(log2, "{line}");
                let ws = line.split_whitespace().find(|w| w.starts_with("ws://")).map(String::from);
                if let Some(ws) = ws {
                    let mut e = eps2.lock();
                    if line.contains("Debugger listening on") && e.node.is_none() {
                        e.node = Some(ws);
                    } else if line.contains("DevTools listening on") && e.devtools.is_none() {
                        e.devtools = Some(ws);
                    }
                }
            }
        });
    }
    let log_tail = |p: &Path| -> String {
        let s = fs::read_to_string(p).unwrap_or_default();
        s.lines().rev().take(10).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
    };
    // only checks for exit without reaping (see peek_exit)
    let exited = |c: &Arc<Mutex<Option<Child>>>| -> Option<String> { c.lock().as_ref().and_then(peek_exit) };

    // 1) Node inspector address
    let start = Instant::now();
    let node_url = loop {
        if let Some(d) = tripped() {
            bail!("앱 창이 숨김 처리 전에 화면에 나타나 즉시 강제 종료했습니다: {d}");
        }
        if let Some(u) = eps.lock().node.clone() {
            break u;
        }
        if let Some(s) = exited(&child) {
            bail!("앱이 바로 종료되었습니다 ({s}).\n{}", log_tail(&paths.log));
        }
        if start.elapsed() > Duration::from_secs(20) {
            bail!("Node 인스펙터 주소를 20초 안에 찾지 못했습니다 (퓨즈가 꺼져 있을 수 있음).\n{}", log_tail(&paths.log));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    // 2) inject the patch on the first line of the main script
    let insp = Cdp::connect(&node_url).await.context("Node 인스펙터 연결 실패")?;
    // off-screen coordinates (DIP). -30000 on macOS (which clamps to about 40px past the left edge).
    // On Windows, the area beyond the bottom-right of all monitors converted by DPI scale and kept
    // within the 16-bit coordinate limit (±32767).
    #[cfg(not(windows))]
    let (off_x, off_y) = (-30000, -30000);
    #[cfg(windows)]
    let (off_x, off_y) = win_screen::offscreen_dip();
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cfg = json!({
        "width": opts.width, "height": opts.height,
        "mode": std::env::var("GHOSTDESK_ELECTRON_MODE").unwrap_or_else(|_| "ghost".into()),
        "offX": off_x, "offY": off_y,
        "parentPid": std::process::id(),
        "userData": if app.family == AppFamily::VsCode { Value::Null } else { json!(paths.user_data.display().to_string()) },
    });
    #[cfg(windows)]
    {
        let ws_name = paths.real_root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        cfg["aumid"] = json!(hidden_aumid(&ws_name));
    }
    let patch = PATCH_JS.replace("__CFG__", &cfg.to_string());
    let inject = inject_patch(&insp, app.main_script.as_deref(), &patch).await;
    if let Some(d) = tripped() {
        bail!("앱 창이 숨김 처리 전에 화면에 나타나 즉시 강제 종료했습니다: {d}");
    }
    let inject_res = match inject {
        Ok(r) => r,
        Err(e) => bail!("메인 프로세스 패치 주입 실패: {e:#}\n{}", log_tail(&paths.log)),
    };
    if inject_res != "ok" && inject_res != "already" {
        eprintln!("[ghostdesk] Electron 패치 일부 실패: {inject_res}");
    }

    // 3) browser CDP address (stderr or DevToolsActivePort)
    let start = Instant::now();
    let port_file = paths.user_data.join("DevToolsActivePort");
    let ws_url = loop {
        if let Some(d) = tripped() {
            bail!("앱 창이 숨김 처리 전에 화면에 나타나 즉시 강제 종료했습니다: {d}");
        }
        if let Some(u) = eps.lock().devtools.clone() {
            break u;
        }
        if let Some(u) = crate::browser::read_devtools_port_file(&port_file) {
            break u;
        }
        if let Some(s) = exited(&child) {
            bail!("앱이 종료되었습니다 ({s}).\n{}", log_tail(&paths.log));
        }
        if start.elapsed() > Duration::from_secs(40) {
            bail!("앱 디버깅 포트를 40초 안에 찾지 못했습니다.\n{}", log_tail(&paths.log));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let closed = Arc::new(AtomicBool::new(false));
    let stats: Arc<Mutex<WatchStats>> = Default::default();

    // the watchdog runs from before the CDP connection (to catch exposure right after window creation
    // too)
    let mut tasks = vec![tokio::spawn(watchdog(
        insp.clone(),
        main_pid,
        pgid,
        child.clone(),
        notes.clone(),
        closed.clone(),
        stats.clone(),
        side,
    ))];

    let inner = match BrowserWorkspace::connect(
        &ws_url,
        None,
        ConnectOptions {
            width: opts.width,
            height: opts.height,
            app_mode: true,
            fix_headless_ua: false,
            start_url: String::new(),
            page_wait: Duration::from_secs(45),
            // Windows: these apps have windows, so an HTML5 drag handed to OLE DoDragDrop would drop onto
            // whatever is under the user's real cursor (their Explorer, documents, …). Intercept it and
            // finish it inside the app's page. (macOS unchanged.)
            intercept_drags: cfg!(windows),
        },
        sink,
    )
    .await
    {
        Ok(w) => w,
        Err(e) => {
            closed.store(true, Ordering::SeqCst);
            for t in &tasks {
                t.abort();
            }
            return Err(e.context(format!("앱 CDP 연결 실패\n{}", log_tail(&paths.log))));
        }
    };

    tasks.push(tokio::spawn(maintain_loop(
        insp.clone(),
        inner.clone(),
        child.clone(),
        notes.clone(),
        closed.clone(),
        stats.clone(),
    )));
    if let Some(d) = tripped() {
        closed.store(true, Ordering::SeqCst);
        for t in &tasks {
            t.abort();
        }
        bail!("앱 창이 화면에 나타나 즉시 강제 종료했습니다: {d}");
    }
    guard.armed = false;
    Ok(Arc::new(ElectronWorkspace {
        inner,
        insp,
        app,
        paths,
        main_pid,
        pgid,
        child,
        notes,
        closed,
        stats,
        tasks: Mutex::new(tasks),
        width: opts.width,
        height: opts.height,
    }))
}

/// Waits for the first line of the app's main script in the main process paused by
/// `--inspect-brk` and evaluates the patch.
async fn inject_patch(insp: &Arc<Cdp>, main_script: Option<&str>, patch: &str) -> Result<String> {
    let mut ev = insp.subscribe();
    insp.call("Runtime.enable", json!({}), None).await?;
    insp.call("Debugger.enable", json!({}), None).await?;
    let main_re = main_script.map(|m| main_url_regex(m, cfg!(windows)));
    let bp = match &main_re {
        Some(re) => insp
            .call("Debugger.setBreakpointByUrl", json!({ "urlRegex": re, "lineNumber": 0 }), None)
            .await
            .ok()
            .and_then(|v| v["breakpointId"].as_str().map(String::from)),
        None => None,
    };
    insp.call("Runtime.runIfWaitingForDebugger", json!({}), None).await?;
    let mut scripts: HashMap<String, String> = HashMap::new();
    let deadline = Instant::now() + Duration::from_secs(25);
    let result = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("앱 메인 스크립트에서 멈추지 않았습니다 (25초)");
        }
        let e = match tokio::time::timeout(left, ev.recv()).await {
            Ok(Ok(e)) => e,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => bail!("인스펙터 연결이 끊겼습니다"),
            Err(_) => bail!("앱 메인 스크립트에서 멈추지 않았습니다 (25초)"),
        };
        match e.method.as_str() {
            "__closed" => bail!("인스펙터 연결이 끊겼습니다 (앱 종료)"),
            "Debugger.scriptParsed" => {
                scripts.insert(
                    e.params["scriptId"].as_str().unwrap_or_default().to_string(),
                    e.params["url"].as_str().unwrap_or_default().to_string(),
                );
            }
            "Debugger.paused" => {
                let f = &e.params["callFrames"][0];
                let sid = f["location"]["scriptId"].as_str().unwrap_or_default();
                let url = scripts.get(sid).cloned().unwrap_or_default();
                let hit_main = main_script.map(|m| url.ends_with(&format!("/{m}")) || (cfg!(windows) && url_ends_with_ci(&url, m))).unwrap_or(false)
                    || e.params["hitBreakpoints"].as_array().map(|a| !a.is_empty()).unwrap_or(false);
                let brk_start = e.params["reason"] == "Break on start" && url.starts_with("file://");
                if hit_main || brk_start {
                    let r = insp
                        .call(
                            "Debugger.evaluateOnCallFrame",
                            json!({ "callFrameId": f["callFrameId"], "expression": patch, "returnByValue": true }),
                            None,
                        )
                        .await;
                    if let Some(b) = &bp {
                        let _ = insp.call("Debugger.removeBreakpoint", json!({ "breakpointId": b }), None).await;
                    }
                    let _ = insp.call("Debugger.resume", json!({}), None).await;
                    let r = r?;
                    if let Some(ex) = r.get("exceptionDetails") {
                        bail!("패치 평가 예외: {}", ex["exception"]["description"].as_str().unwrap_or(&ex.to_string()));
                    }
                    break r["result"]["value"].as_str().unwrap_or("?").to_string();
                }
                let _ = insp.call("Debugger.resume", json!({}), None).await;
            }
            _ => {}
        }
    };
    let _ = insp.call("Debugger.disable", json!({}), None).await;
    let _ = insp.call("Runtime.disable", json!({}), None).await;
    Ok(result)
}

async fn main_eval(insp: &Cdp, expr: &str, timeout: Duration) -> Result<Value> {
    let r = insp
        .call_timeout("Runtime.evaluate", json!({ "expression": expr, "returnByValue": true }), None, timeout)
        .await?;
    if let Some(ex) = r.get("exceptionDetails") {
        bail!("메인 프로세스 평가 예외: {}", ex["exception"]["description"].as_str().unwrap_or(&ex.to_string()));
    }
    Ok(r["result"]["value"].clone())
}

fn push_note(notes: &Mutex<Vec<String>>, n: String) {
    let mut v = notes.lock();
    if v.last() != Some(&n) {
        v.push(n);
    }
    if v.len() > 100 {
        v.remove(0);
    }
}

/// Every 250 ms: checks whether our processes (group/descendants) have a visible window (alpha>0,
/// onscreen) or are frontmost.
async fn watchdog(
    insp: Arc<Cdp>,
    main_pid: i32,
    pgid: i32,
    child: Arc<Mutex<Option<Child>>>,
    notes: Arc<Mutex<Vec<String>>>,
    closed: Arc<AtomicBool>,
    stats: Arc<Mutex<WatchStats>>,
    side: WatchSide,
) {
    let mut consecutive = 0u32;
    loop {
        if closed.load(Ordering::SeqCst) {
            break;
        }
        let s2 = side.clone();
        let v = tokio::task::spawn_blocking(move || scan(main_pid, pgid, &s2)).await.ok().flatten();
        {
            let mut s = stats.lock();
            s.ticks += 1;
        }
        match v {
            None => consecutive = 0,
            Some(desc) => {
                consecutive += 1;
                eprintln!("[ghostdesk] Electron 감시자: {desc}");
                {
                    let mut s = stats.lock();
                    s.violations += 1;
                    s.last_violation = Some(desc.clone());
                }
                let front = desc.contains("front");
                let r = main_eval(
                    &insp,
                    &format!("globalThis.__ghostdesk && globalThis.__ghostdesk.rehide({front})"),
                    Duration::from_millis(800),
                )
                .await;
                if r.is_ok() {
                    stats.lock().rehides += 1;
                }
                if consecutive == 1 {
                    push_note(&notes, "앱 창이 화면에 나타나려 해서 즉시 다시 숨겼습니다.".into());
                }
                if consecutive >= 4 {
                    // not hidden for over a second: kill it to protect the user
                    kill_group_if_held(main_pid, pgid, &child);
                    stats.lock().killed = true;
                    push_note(&notes, "앱 창을 숨길 수 없어 사용자 화면 보호를 위해 앱을 강제 종료했습니다.".into());
                    closed.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Channel for telling the agent what happened during watching, plus (Windows) console window and
/// Windows Terminal watch state.
///
/// Unused on macOS (scan calls scan_violation directly).
#[derive(Clone)]
struct WatchSide {
    notes: Arc<Mutex<Vec<String>>>,
    #[cfg(windows)]
    console: Arc<Mutex<win_screen::ConsoleWatch>>,
}

impl WatchSide {
    fn new(notes: Arc<Mutex<Vec<String>>>) -> Self {
        WatchSide {
            notes,
            #[cfg(windows)]
            console: Arc::new(Mutex::new(win_screen::ConsoleWatch::new())),
        }
    }
}

/// One watch pass (shared by the pre-watcher and main watchdog). The violation description, if any.
fn scan(main_pid: i32, pgid: i32, side: &WatchSide) -> Option<String> {
    #[cfg(windows)]
    {
        let _ = pgid;
        scan_violation_with(main_pid, Some(side))
    }
    #[cfg(not(windows))]
    {
        let _ = side;
        scan_violation(main_pid, pgid)
    }
}

#[cfg(target_os = "macos")]
fn scan_violation(main_pid: i32, pgid: i32) -> Option<String> {
    let tree = proc::tree(main_pid, pgid);
    if tree.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for w in mac::windows(true) {
        if tree.contains(&w.pid) && w.alpha > 0.0 {
            out.push(format!(
                "보이는 창 pid={} wid={} layer={} alpha={:.2} bounds={:?}",
                w.pid, w.wid, w.layer, w.alpha, w.bounds
            ));
        }
    }
    let f = mac::front_pid();
    if f > 0 && tree.contains(&f) {
        out.push(format!("front 프로세스가 우리 앱(pid={f})"));
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join("; "))
    }
}

/// Windows: top-level windows of the Job's processes that could show on the user's screen
/// (monitors, taskbar), or whether our process is in the foreground (keyboard focus).
#[cfg(windows)]
fn scan_violation(main_pid: i32, _pgid: i32) -> Option<String> {
    scan_violation_with(main_pid, None)
}

/// With `side` (the watchdog), first hides our processes' console windows (instead of killing the
/// app, plan_console_hide) and records newly appeared Windows Terminal windows. Console windows
/// being hidden are excluded from violations.
#[cfg(windows)]
fn scan_violation_with(main_pid: i32, side: Option<&WatchSide>) -> Option<String> {
    let pids = win_screen::our_pids(main_pid);
    if let Some(side) = side {
        for n in side.console.lock().wt_pass() {
            eprintln!("[ghostdesk] Electron 감시자: {n}");
            push_note(&side.notes, n);
        }
    }
    if pids.is_empty() {
        return None;
    }
    let mut wins: Vec<WinState> = crate::platform::win::windows_of(&pids).iter().map(win_screen::state).collect();
    let mut skip: HashSet<isize> = HashSet::new();
    if let Some(side) = side {
        let mut cw = side.console.lock();
        let (hide, s) = plan_console_hide(&wins, &cw.first_hidden, Instant::now());
        for h in hide {
            if let Some(n) = cw.hide(h, &wins) {
                eprintln!("[ghostdesk] Electron 감시자: {n}");
                push_note(&side.notes, n);
            }
        }
        skip = s;
        wins.retain(|w| !skip.contains(&w.hwnd));
    }
    // a console window being hidden that holds the foreground isn't a foreground violation (hiding it
    // returns the foreground to the user's window too)
    let fg = win_screen::foreground_hwnd();
    let front = if fg.is_some_and(|h| skip.contains(&h)) {
        None
    } else {
        crate::platform::win::foreground_pid().filter(|f| pids.contains(f))
    };
    describe_violations(&wins, front)
}

/// Console window class (conhost). Consoles handed to Windows Terminal are drawn in a
/// CASCADIA_HOSTING_WINDOW_CLASS window instead.
const CONSOLE_CLASS: &str = "ConsoleWindowClass";
const WT_CLASS: &str = "CASCADIA_HOSTING_WINDOW_CLASS";
/// If the same console window is still visible this long after we first tried to hide it, give up
/// hiding and treat it as a violation (→ kill).
///
/// Counted in time, not attempts: the pre-watcher (40 ms) and main watchdog (250 ms) share the same
/// record, so counting attempts would use them all up during pre-watching (~160 ms) before the
/// console host even processes SW_HIDE.
const CONSOLE_HIDE_GRACE: Duration = Duration::from_millis(1500);

/// Windows watchdog: visible console windows of our (Job) processes are hidden instead of killing
/// the app (console windows of git, python, cmd, … launched by processes PATCH_JS's windowsHide
/// default doesn't reach, like Electron's extension host and utility processes).
///
/// Returns (windows to hide, windows to exclude from the violation check). `first_hidden`: when we
/// first tried to hide each window.
fn plan_console_hide(
    wins: &[WinState],
    first_hidden: &HashMap<isize, Instant>,
    now: Instant,
) -> (Vec<isize>, HashSet<isize>) {
    let mut hide = Vec::new();
    let mut skip = HashSet::new();
    for w in wins {
        if w.class != CONSOLE_CLASS || !(w.on_screen || w.taskbar) {
            continue;
        }
        if first_hidden.get(&w.hwnd).map_or(true, |t| now.saturating_duration_since(*t) < CONSOLE_HIDE_GRACE) {
            hide.push(w.hwnd);
            skip.insert(w.hwnd);
        }
    }
    (hide, skip)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn scan_violation(_main_pid: i32, _pgid: i32) -> Option<String> {
    None
}

/// Per-window state for decisions (Windows watchdog and sampler).
#[derive(Clone, Debug)]
struct WinState {
    pid: u32,
    hwnd: isize,
    class: String,
    rect: (i32, i32, i32, i32),
    alpha: Option<u8>,
    /// Overlaps a monitor, visible and opaque (TopWindow::shows_on_screen).
    on_screen: bool,
    /// A window that gets a taskbar button or Alt+Tab entry even when off-screen or transparent.
    taskbar: bool,
}

/// Violation description (None if none). Foreground violations include "front" so the watchdog
/// calls rehide(true).
fn describe_violations(wins: &[WinState], front: Option<u32>) -> Option<String> {
    let mut out = Vec::new();
    for w in wins {
        if w.on_screen {
            out.push(format!(
                "보이는 창 pid={} hwnd={:#x} class={} alpha={:?} rect={:?}",
                w.pid, w.hwnd, w.class, w.alpha, w.rect
            ));
        } else if w.taskbar {
            out.push(format!("작업 표시줄에 나타날 수 있는 창 pid={} hwnd={:#x} class={} rect={:?}", w.pid, w.hwnd, w.class, w.rect));
        }
    }
    if let Some(f) = front {
        out.push(format!("front 프로세스가 우리 앱(pid={f})"));
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join("; "))
    }
}

#[cfg(windows)]
mod win_screen {
    //! Windows screen and window queries (read only): off-screen coordinates, our window state, cursor
    //! shape.
    use super::WinState;
    use crate::platform::win;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};

    /// Processes to watch: the Job's pid list (just the main process without a Job).
    pub fn our_pids(main_pid: i32) -> Vec<u32> {
        if main_pid <= 0 {
            return vec![];
        }
        crate::platform::proc::group_pids(main_pid as u32).unwrap_or_else(|| vec![main_pid as u32])
    }

    /// Whether the window appears on the taskbar and Alt+Tab: visible + not DWM-cloaked +
    /// (WS_EX_APPWINDOW, or not a tool window, not non-activating and unowned). The button appears
    /// even off-screen at alpha 0.
    pub fn taskbar_eligible(t: &win::TopWindow) -> bool {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindow, GetWindowLongPtrW, GWL_EXSTYLE, GW_OWNER, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        };
        if !t.visible || t.cloaked {
            return false;
        }
        let h = t.hwnd as HWND;
        let ex = unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } as u32;
        if ex & WS_EX_APPWINDOW != 0 {
            return true;
        }
        if ex & (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) != 0 {
            return false;
        }
        unsafe { GetWindow(h, GW_OWNER) }.is_null()
    }

    pub fn state(t: &win::TopWindow) -> WinState {
        WinState {
            pid: t.pid,
            hwnd: t.hwnd,
            class: t.class.clone(),
            rect: t.rect,
            alpha: t.alpha,
            on_screen: t.shows_on_screen(),
            taskbar: taskbar_eligible(t),
        }
    }

    /// Per-monitor (rect, scale). Scale is effective DPI / 96 (1 for DPI-unaware processes).
    pub fn monitors() -> Vec<((i32, i32, i32, i32), f64)> {
        use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
        use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
        unsafe extern "system" fn cb(h: HMONITOR, _dc: HDC, r: *mut RECT, lp: LPARAM) -> windows_sys::core::BOOL {
            let v = &mut *(lp as *mut Vec<(HMONITOR, RECT)>);
            if !r.is_null() {
                v.push((h, *r));
            }
            1
        }
        let mut raw: Vec<(HMONITOR, RECT)> = Vec::new();
        unsafe {
            EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(cb), &mut raw as *mut _ as LPARAM);
        }
        raw.into_iter()
            .map(|(h, r)| {
                let (mut dx, mut dy) = (96u32, 96u32);
                let ok = unsafe { GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) } == 0;
                let scale = if ok && dx >= 48 { dx as f64 / 96.0 } else { 1.0 };
                ((r.left, r.top, r.right, r.bottom), scale)
            })
            .collect()
    }

    /// DIP coordinates to put hidden windows at: the area beyond the bottom-right of all monitors
    /// (platform::win::offscreen_origin, real pixels) converted at the primary monitor's (the one
    /// containing the origin) scale.
    pub fn offscreen_dip() -> (i32, i32) {
        let px = win::offscreen_origin();
        let mons = monitors();
        let primary = mons
            .iter()
            .find(|((l, t, r, b), _)| *l <= 0 && 0 < *r && *t <= 0 && 0 < *b)
            .map(|m| m.1)
            .unwrap_or(1.0);
        let max = mons.iter().map(|m| m.1).fold(primary, f64::max);
        super::offscreen_dip_math(px, primary, max)
    }

    /// Cursor shape identifier (changes when the shape changes; query only).
    pub fn cursor_shape() -> i64 {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO};
        let mut ci: CURSORINFO = unsafe { std::mem::zeroed() };
        ci.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
        if unsafe { GetCursorInfo(&mut ci) } == 0 {
            return -1;
        }
        ((ci.hCursor as isize as i64) << 1) | (ci.flags as i64 & 1)
    }

    /// Process executable name (for reporting).
    pub fn proc_name(pid: u32) -> String {
        win::process_path(pid)
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "?".into())
    }

    pub fn foreground_hwnd() -> Option<isize> {
        use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        let h = unsafe { GetForegroundWindow() };
        (!h.is_null()).then_some(h as isize)
    }

    fn class_of(h: HWND) -> String {
        use windows_sys::Win32::UI::WindowsAndMessaging::GetClassNameW;
        let mut b = [0u16; 128];
        let n = unsafe { GetClassNameW(h, b.as_mut_ptr(), b.len() as i32) };
        String::from_utf16_lossy(&b[..n.max(0) as usize])
    }

    /// Windows Terminal top-level windows (visible only, or all).
    pub fn wt_windows(visible_only: bool) -> Vec<isize> {
        use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
        win::top_level_hwnds()
            .into_iter()
            .filter(|&h| (!visible_only || unsafe { IsWindowVisible(h) } != 0) && class_of(h) == super::WT_CLASS)
            .map(|h| h as isize)
            .collect()
    }

    /// Watchdog state for console windows and Windows Terminal.
    pub struct ConsoleWatch {
        /// Our console windows we tried to hide → when we first tried.
        pub first_hidden: std::collections::HashMap<isize, std::time::Instant>,
        /// Windows Terminal windows already known (present before launch + already reported).
        pub wt_known: std::collections::HashSet<isize>,
    }

    impl ConsoleWatch {
        pub fn new() -> Self {
            ConsoleWatch { first_hidden: Default::default(), wt_known: wt_windows(false).into_iter().collect() }
        }

        /// Hides one of our console windows (asynchronously, so the watchdog doesn't block even if the
        /// console host is stuck).
        ///
        /// Returns a note to report only the first time it's hidden. The app isn't killed.
        pub fn hide(&mut self, h: isize, wins: &[WinState]) -> Option<String> {
            use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindowAsync, SW_HIDE};
            unsafe { ShowWindowAsync(h as HWND, SW_HIDE) };
            if self.first_hidden.contains_key(&h) {
                return None;
            }
            self.first_hidden.insert(h, std::time::Instant::now());
            let pid = wins.iter().find(|w| w.hwnd == h).map(|w| w.pid).unwrap_or(0);
            Some(format!(
                "숨은 앱(또는 그 확장)이 실행한 콘솔 프로그램의 창이 사용자 화면에 나타나 숨겼습니다 (pid {pid} {}). \
                 앱은 계속 실행됩니다. 그 콘솔의 출력은 화면으로 볼 수 없으니 앱 안의 터미널·출력 패널을 쓰세요.",
                proc_name(pid)
            ))
        }

        /// Finds and reports newly visible Windows Terminal windows. When Windows Terminal is the default
        /// terminal, console programs' windows open outside GhostDesk's Job (WindowsTerminal.exe) and can't
        /// be hidden or closed. They may also be windows the user opened, so they're only recorded, never
        /// touched.
        pub fn wt_pass(&mut self) -> Vec<String> {
            let mut out = Vec::new();
            for h in wt_windows(true) {
                if self.wt_known.insert(h) {
                    out.push(format!(
                        "작업 중에 새 Windows Terminal 창이 사용자 화면에 나타났습니다 (hwnd {h:#x}). 숨은 앱이 실행한 콘솔 \
                         프로그램이 Windows Terminal 로 넘겨졌을 수 있습니다 — 그 창은 GhostDesk 밖에 있어 숨기거나 닫지 \
                         않습니다 (사용자가 직접 연 창일 수도 있음). 콘솔 프로그램이 필요하면 앱 안의 터미널을 쓰세요."
                    ));
                }
            }
            out
        }
    }
}

/// Every 500 ms: collects injected notes, maintains window size/opacity, detects process exit, and
/// requests a new window when all windows are closed.
async fn maintain_loop(
    insp: Arc<Cdp>,
    inner: Arc<BrowserWorkspace>,
    child: Arc<Mutex<Option<Child>>>,
    notes: Arc<Mutex<Vec<String>>>,
    closed: Arc<AtomicBool>,
    stats: Arc<Mutex<WatchStats>>,
) {
    let mut no_window_since: Option<Instant> = None;
    let mut last_activate: Option<Instant> = None;
    let mut fails = 0;
    let mut tick = 0u64;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        tick += 1;
        if tick % 4 == 1 {
            // Electron rarely sends title change events (targetInfoChanged), so refresh periodically
            let _ = inner.refresh_targets().await;
        }
        if closed.load(Ordering::SeqCst) {
            break;
        }
        // check without reaping (reaped after the group kill in shutdown/Drop)
        let gone = !child_alive(&child);
        if gone {
            push_note(&notes, "앱 프로세스가 종료되었습니다.".into());
            closed.store(true, Ordering::SeqCst);
            break;
        }
        match main_eval(&insp, "globalThis.__ghostdesk ? globalThis.__ghostdesk.maintain() : null", Duration::from_secs(2)).await {
            Ok(v) => {
                fails = 0;
                if let Some(arr) = v["notes"].as_array() {
                    for n in arr.iter().filter_map(|n| n.as_str()) {
                        push_note(&notes, n.to_string());
                    }
                }
                let vis = v["visible"].as_u64().unwrap_or(0);
                if vis > 0 {
                    stats.lock().maintain_rehidden += vis;
                }
                let wins = v["wins"].as_u64().unwrap_or(0);
                let tabs = inner.tabs().0.len();
                if wins == 0 && tabs == 0 {
                    let since = *no_window_since.get_or_insert_with(Instant::now);
                    let recent = last_activate.map(|t| t.elapsed() < Duration::from_secs(10)).unwrap_or(false);
                    if since.elapsed() > Duration::from_millis(1500) && !recent {
                        last_activate = Some(Instant::now());
                        let _ = main_eval(
                            &insp,
                            "(() => { const e = globalThis.__ghostdesk.electron; e.app.emit('activate', {}, false); return true; })()",
                            Duration::from_secs(2),
                        )
                        .await;
                        push_note(&notes, "앱 창이 모두 닫혀 새 창을 요청했습니다.".into());
                    }
                } else {
                    no_window_since = None;
                }
            }
            Err(_) => {
                fails += 1;
                if fails >= 6 && inner.is_closed() {
                    closed.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }
    }
}

impl ElectronWorkspace {
    pub fn inner(&self) -> &Arc<BrowserWorkspace> {
        &self.inner
    }

    pub fn watch_stats(&self) -> WatchStats {
        self.stats.lock().clone()
    }

    /// Every process we launched (group + descendants).
    pub fn pids(&self) -> Vec<i32> {
        #[cfg(target_os = "macos")]
        {
            let mut v: Vec<i32> = proc::tree(self.main_pid, self.pgid).into_iter().collect();
            v.sort();
            v
        }
        // Windows: the Job object's process list
        #[cfg(windows)]
        {
            let mut v: Vec<i32> = win_screen::our_pids(self.main_pid).into_iter().map(|p| p as i32).collect();
            v.sort();
            v
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            vec![self.main_pid]
        }
    }

    // ---- backend contract (same names/signatures as BrowserWorkspace) ----

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        self.inner.screenshot().await
    }

    pub async fn zoom(&self, region: [f64; 4]) -> Result<Vec<u8>> {
        self.inner.zoom(region).await
    }

    // modifiers and key combos follow this computer's OS (cmd → ctrl outside macOS; unchanged on
    // macOS)

    pub async fn mouse_move(&self, x: f64, y: f64, mods: i64) -> Result<()> {
        self.inner.mouse_move(x, y, crate::platform::input::localize_mods(mods)).await
    }

    pub async fn mouse_down(&self, b: MouseButton, count: i64, mods: i64) -> Result<()> {
        self.inner.mouse_down(b, count, crate::platform::input::localize_mods(mods)).await
    }

    pub async fn mouse_up(&self, b: MouseButton, count: i64, mods: i64) -> Result<()> {
        self.inner.mouse_up(b, count, crate::platform::input::localize_mods(mods)).await
    }

    pub async fn click(&self, pos: Option<(f64, f64)>, b: MouseButton, count: u32, mods: i64) -> Result<()> {
        self.inner.click(pos, b, count, crate::platform::input::localize_mods(mods)).await
    }

    pub async fn drag(&self, from: (f64, f64), to: (f64, f64), mods: i64) -> Result<()> {
        self.inner.drag(from, to, crate::platform::input::localize_mods(mods)).await
    }

    pub async fn scroll(&self, pos: Option<(f64, f64)>, dx: f64, dy: f64, mods: i64) -> Result<()> {
        self.inner.scroll(pos, dx, dy, crate::platform::input::localize_mods(mods)).await
    }

    /// On Windows, refuses key combos that open native menus (Alt, F10, Alt+letter, …;
    /// `menu_key_refusal`).
    pub async fn key(&self, combo: &str, repeat: u32) -> Result<()> {
        let combo = crate::platform::input::localize_combo(combo);
        #[cfg(windows)]
        if let Some(why) = menu_key_refusal(&combo) {
            bail!("{why}");
        }
        self.inner.key(&combo, repeat).await
    }

    pub async fn hold_key(&self, combo: &str, secs: f64) -> Result<()> {
        let combo = crate::platform::input::localize_combo(combo);
        #[cfg(windows)]
        if let Some(why) = menu_key_refusal(&combo) {
            bail!("{why}");
        }
        self.inner.hold_key(&combo, secs).await
    }

    pub async fn type_text(&self, text: &str) -> Result<()> {
        self.inner.type_text(text).await
    }

    pub fn mouse_pos(&self) -> (f64, f64) {
        self.inner.mouse_pos()
    }

    pub fn views(&self) -> (Vec<crate::desk::ViewInfo>, Option<String>) {
        let (mut v, a) = self.inner.views();
        for x in &mut v {
            // show the app name instead of long internal URLs like VS Code's workbench.html
            if x.detail.starts_with("vscode-file://") || x.detail.starts_with("file://") || x.detail.starts_with("app://") {
                x.detail = format!("{} 창", self.app.name);
            }
        }
        (v, a)
    }

    pub async fn switch_view(&self, index: usize) -> Result<()> {
        self.inner.switch_view(index).await
    }

    pub fn take_notes(&self) -> Vec<String> {
        let mut v = std::mem::take(&mut *self.notes.lock());
        v.extend(self.inner.take_notes());
        v
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.inner.is_closed()
    }

    /// app.exit(0) through the inspector → wait 3 s → SIGKILL the process group/descendants → reap the
    /// child.
    pub async fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        for t in self.tasks.lock().drain(..) {
            t.abort();
        }
        self.inner.shutdown().await;
        if self.child.lock().is_some() {
            // grab the descendant list right before exit (once main dies, their parent becomes launchd). The
            // child hasn't been reaped yet, so main_pid/pgid haven't been reused.
            #[cfg(target_os = "macos")]
            let tree: HashSet<i32> = proc::tree(self.main_pid, self.pgid);
            #[cfg(not(target_os = "macos"))]
            let tree: HashSet<i32> = HashSet::new();
            if child_alive(&self.child) {
                let _ = main_eval(
                    &self.insp,
                    "(() => { const G = globalThis.__ghostdesk; setTimeout(() => G.exit(0), 10); return true; })()",
                    Duration::from_secs(2),
                )
                .await;
                for _ in 0..30 {
                    if !child_alive(&self.child) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            terminate_and_reap(self.main_pid, self.pgid, Some(&tree), &self.child);
        }
        // the short link is recreated on the next run (Windows uses the real folder without a link, so
        // nothing to remove)
        if self.paths.root != self.paths.real_root {
            let _ = fs::remove_file(&self.paths.root);
        }
    }

    // ---- extras ----

    /// Evaluates JS in the active window's page.
    pub async fn eval(&self, expr: &str) -> Result<Value> {
        self.inner.eval(expr).await
    }

    /// Evaluates JS in the main process (for debugging/self-tests).
    pub async fn main_eval(&self, expr: &str) -> Result<Value> {
        main_eval(&self.insp, expr, Duration::from_secs(5)).await
    }

    /// The app menu (or last context menu) tree as text.
    pub async fn menu_text(&self, src: MenuSource) -> Result<String> {
        let v = self.menu_call(src, None).await?;
        if let Some(e) = v["error"].as_str() {
            bail!("{e}");
        }
        Ok(v["text"].as_str().unwrap_or_default().to_string())
    }

    /// Runs a menu item by path like "File > New Text File" (roles affecting the screen or other apps
    /// are refused).
    pub async fn menu_click(&self, src: MenuSource, path: &str) -> Result<String> {
        let parts: Vec<String> = path.split('>').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        ensure!(!parts.is_empty(), "메뉴 경로가 비었습니다");
        let v = self.menu_call(src, Some(parts)).await?;
        if let Some(e) = v["error"].as_str() {
            bail!("{e}");
        }
        Ok(v["clicked"].as_str().unwrap_or_default().to_string())
    }

    async fn menu_call(&self, src: MenuSource, path: Option<Vec<String>>) -> Result<Value> {
        let src = match src {
            MenuSource::App => "app",
            MenuSource::Context => "context",
        };
        let active = self.inner.tabs().1;
        let expr = format!(
            "{}({}, {}, {}, {})",
            MENU_JS,
            json!(src),
            json!(path),
            json!(active),
            json!(ALLOWED_MENU_ROLES)
        );
        main_eval(&self.insp, &expr, Duration::from_secs(5)).await
    }
}

impl Drop for ElectronWorkspace {
    fn drop(&mut self) {
        for t in self.tasks.lock().drain(..) {
            t.abort();
        }
        // only does anything if shutdown didn't run (kills nothing if already reaped)
        terminate_and_reap(self.main_pid, self.pgid, None, &self.child);
    }
}

/// SIGKILLs the process group + descendants (+ a list grabbed earlier).
fn kill_tree(main_pid: i32, pgid: i32, extra: Option<&HashSet<i32>>) {
    #[cfg(target_os = "macos")]
    {
        let mut all = proc::tree(main_pid, pgid);
        if let Some(x) = extra {
            all.extend(x.iter().copied());
        }
        // never kill our own group (launched with process_group(0), so it should differ)
        if pgid > 1 && pgid != unsafe { libc::getpgrp() } {
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
        let me = std::process::id() as i32;
        for p in all {
            if p > 1 && p != me {
                unsafe { libc::kill(p, libc::SIGKILL) };
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (extra, main_pid);
        #[cfg(unix)]
        if pgid > 1 && pgid != unsafe { libc::getpgrp() } {
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
        // Windows cleans up through the Job object (kill_group_if_held / terminate_and_reap)
        #[cfg(not(unix))]
        let _ = pgid;
    }
}

// ---- macOS native helpers (read-only queries only) ----

#[cfg(target_os = "macos")]
mod proc {
    use std::{collections::HashSet, ffi::c_void, path::PathBuf};

    /// Full (pid, ppid, pgid) list.
    pub fn all() -> Vec<(i32, i32, i32)> {
        let mut out = Vec::new();
        unsafe {
            let n = libc::proc_listallpids(std::ptr::null_mut(), 0);
            if n <= 0 {
                return out;
            }
            let mut buf = vec![0i32; n as usize + 256];
            let n = libc::proc_listallpids(buf.as_mut_ptr() as *mut c_void, (buf.len() * 4) as i32);
            if n <= 0 {
                return out;
            }
            for &pid in &buf[..(n as usize).min(buf.len())] {
                if pid <= 0 {
                    continue;
                }
                let mut info: libc::proc_bsdinfo = std::mem::zeroed();
                let sz = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
                let r = libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut c_void, sz);
                if r == sz {
                    out.push((pid, info.pbi_ppid as i32, info.pbi_pgid as i32));
                }
            }
        }
        out
    }

    /// main_pid's descendants + the same process group.
    pub fn tree(main_pid: i32, pgid: i32) -> HashSet<i32> {
        let all = all();
        let mut set: HashSet<i32> = HashSet::new();
        for &(pid, _, g) in &all {
            if pid == main_pid || (pgid > 1 && g == pgid) {
                set.insert(pid);
            }
        }
        loop {
            let before = set.len();
            for &(pid, ppid, _) in &all {
                if set.contains(&ppid) {
                    set.insert(pid);
                }
            }
            if set.len() == before {
                break;
            }
        }
        set
    }

    pub fn path(pid: i32) -> Option<PathBuf> {
        let mut buf = vec![0u8; 4096];
        let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
        if n <= 0 {
            return None;
        }
        Some(PathBuf::from(String::from_utf8_lossy(&buf[..n as usize]).into_owned()))
    }

    pub fn name(pid: i32) -> String {
        let mut buf = vec![0u8; 256];
        let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
        if n <= 0 {
            return "?".into();
        }
        String::from_utf8_lossy(&buf[..n as usize]).into_owned()
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{c_void, CString};
    use std::sync::OnceLock;

    use objc2::runtime::AnyObject;
    use objc2_core_foundation::CFRetained;
    use objc2_core_graphics::{CGEvent, CGWindowListCopyWindowInfo, CGWindowListOption};
    use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

    #[derive(Clone, Debug)]
    pub struct WinRow {
        pub pid: i32,
        pub wid: u32,
        pub layer: i64,
        pub onscreen: bool,
        pub alpha: f64,
        pub bounds: [f64; 4],
    }

    fn num(d: &NSDictionary<NSString, AnyObject>, k: &str) -> Option<f64> {
        let o = d.objectForKey(&NSString::from_str(k))?;
        o.downcast_ref::<NSNumber>().map(|n| n.doubleValue())
    }

    /// CGWindowList query (values needing Screen Recording permission, like window names, aren't
    /// read).
    pub fn windows(onscreen_only: bool) -> Vec<WinRow> {
        objc2::rc::autoreleasepool(|_| {
            let opt = if onscreen_only { CGWindowListOption::OptionOnScreenOnly } else { CGWindowListOption::OptionAll };
            let Some(arr) = CGWindowListCopyWindowInfo(opt, 0) else {
                return vec![];
            };
            let ns: &NSArray<NSDictionary<NSString, AnyObject>> =
                unsafe { &*(CFRetained::as_ptr(&arr).as_ptr() as *const NSArray<NSDictionary<NSString, AnyObject>>) };
            let mut out = Vec::new();
            for d in ns.iter() {
                let mut b = [0.0; 4];
                if let Some(o) = d.objectForKey(&NSString::from_str("kCGWindowBounds")) {
                    if let Some(bd) = o.downcast_ref::<NSDictionary>() {
                        let bd: &NSDictionary<NSString, AnyObject> = unsafe { &*(bd as *const NSDictionary as *const _) };
                        b = [
                            num(bd, "X").unwrap_or(0.0),
                            num(bd, "Y").unwrap_or(0.0),
                            num(bd, "Width").unwrap_or(0.0),
                            num(bd, "Height").unwrap_or(0.0),
                        ];
                    }
                }
                out.push(WinRow {
                    pid: num(&d, "kCGWindowOwnerPID").unwrap_or(-1.0) as i32,
                    wid: num(&d, "kCGWindowNumber").unwrap_or(0.0) as u32,
                    layer: num(&d, "kCGWindowLayer").unwrap_or(0.0) as i64,
                    onscreen: num(&d, "kCGWindowIsOnscreen").unwrap_or(0.0) != 0.0,
                    alpha: num(&d, "kCGWindowAlpha").unwrap_or(1.0),
                    bounds: b,
                });
            }
            out
        })
    }

    /// Current cursor position (CGEventGetLocation(CGEventCreate(NULL))).
    pub fn cursor() -> (f64, f64) {
        let ev = CGEvent::new(None);
        let p = CGEvent::location(ev.as_deref());
        (p.x, p.y)
    }

    #[repr(C)]
    #[derive(Default)]
    struct Psn {
        hi: u32,
        lo: u32,
    }

    type GetFront = unsafe extern "C" fn(*mut Psn) -> i32;
    type ToPid = unsafe extern "C" fn(*const Psn, *mut i32) -> i32;

    fn syms() -> Option<(GetFront, ToPid)> {
        static S: OnceLock<Option<(usize, usize)>> = OnceLock::new();
        let s = S.get_or_init(|| unsafe {
            for lib in [
                "/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight",
                "/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices",
            ] {
                let c = CString::new(lib).unwrap();
                libc::dlopen(c.as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL);
            }
            let f = CString::new("_SLPSGetFrontProcess").unwrap();
            let g = CString::new("GetProcessPID").unwrap();
            let a = libc::dlsym(libc::RTLD_DEFAULT, f.as_ptr());
            let b = libc::dlsym(libc::RTLD_DEFAULT, g.as_ptr());
            if a.is_null() || b.is_null() {
                None
            } else {
                Some((a as usize, b as usize))
            }
        });
        s.map(|(a, b)| unsafe {
            (std::mem::transmute::<*mut c_void, GetFront>(a as *mut c_void), std::mem::transmute::<*mut c_void, ToPid>(b as *mut c_void))
        })
    }

    /// Cursor shape change counter (SLSCurrentCursorSeed, query only). -1 if unavailable.
    pub fn cursor_seed() -> i64 {
        static S: OnceLock<usize> = OnceLock::new();
        let f = *S.get_or_init(|| unsafe {
            syms();
            for n in ["SLSCurrentCursorSeed", "CGSCurrentCursorSeed"] {
                let c = CString::new(n).unwrap();
                let p = libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr());
                if !p.is_null() {
                    return p as usize;
                }
            }
            0
        });
        if f == 0 {
            return -1;
        }
        let f: unsafe extern "C" fn() -> i32 = unsafe { std::mem::transmute::<usize, unsafe extern "C" fn() -> i32>(f) };
        unsafe { f() as i64 }
    }

    /// pid of WindowServer's front process (query only).
    pub fn front_pid() -> i32 {
        let Some((gf, tp)) = syms() else { return -2 };
        let mut psn = Psn::default();
        let mut pid = -1;
        unsafe {
            if gf(&mut psn) == 0 {
                tp(&psn, &mut pid);
            }
        }
        pid
    }
}

// ---- self-test ----

/// Non-intrusive sampler: records the cursor, the front process and our processes' onscreen
/// windows every 20 ms.
#[cfg(target_os = "macos")]
struct Sampler {
    stop: Arc<AtomicBool>,
    main_pid: Arc<AtomicI32>,
    handle: Option<std::thread::JoinHandle<SamplerReport>>,
}

/// Non-intrusive sampler (Windows): records the real cursor position and shape, the foreground
/// process and our processes' windows (visible on a monitor or able to show on the taskbar) every
/// 20 ms. Query only.
#[cfg(windows)]
struct Sampler {
    stop: Arc<AtomicBool>,
    main_pid: Arc<AtomicI32>,
    handle: Option<std::thread::JoinHandle<SamplerReport>>,
}

/// How the sampler tells which processes are "ours".
#[cfg(any(target_os = "macos", windows))]
#[derive(Clone)]
enum OursBy {
    /// main_pid's process group/descendants (Electron self-test).
    Group,
    /// Processes created after the sampler started whose executable path contains this string.
    NewWithPath(String),
}

#[derive(Debug, Default)]
struct SamplerReport {
    samples: u64,
    secs: f64,
    max_gap_ms: f64,
    cursor_changes: Vec<String>,
    cursor_shape_changes: Vec<String>,
    front_changes: Vec<String>,
    our_front: u64,
    our_visible: u64,
    our_visible_examples: Vec<String>,
    our_alpha0_onscreen: u64,
    our_alpha0_examples: Vec<String>,
}

#[cfg(target_os = "macos")]
impl Sampler {
    fn start() -> Self {
        Self::start_with(OursBy::Group)
    }

    fn start_with(by: OursBy) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let main_pid = Arc::new(AtomicI32::new(0));
        let (s2, m2) = (stop.clone(), main_pid.clone());
        let handle = std::thread::spawn(move || {
            let t0 = Instant::now();
            let mut rep = SamplerReport::default();
            let mut last_cursor = mac::cursor();
            let mut last_seed = mac::cursor_seed();
            rep.cursor_shape_changes.push(format!("0ms 시작 seed={last_seed}"));
            let mut last_front = mac::front_pid();
            rep.front_changes.push(format!("0ms 시작 front={} ({})", last_front, proc::name(last_front)));
            rep.cursor_changes.push(format!("0ms 시작 cursor=({:.1},{:.1})", last_cursor.0, last_cursor.1));
            let mut tree: HashSet<i32> = HashSet::new();
            let baseline: HashSet<i32> = proc::all().into_iter().map(|p| p.0).collect();
            let mut last_tree = Instant::now() - Duration::from_secs(1);
            let mut last_tick = Instant::now();
            while !s2.load(Ordering::SeqCst) {
                let now = Instant::now();
                let gap = now.duration_since(last_tick).as_secs_f64() * 1000.0;
                if rep.samples > 0 && gap > rep.max_gap_ms {
                    rep.max_gap_ms = gap;
                }
                last_tick = now;
                let ms = t0.elapsed().as_millis();
                let mut mp = m2.load(Ordering::SeqCst);
                if last_tree.elapsed() > Duration::from_millis(200) {
                    // keep the earlier list after exit too (union) to catch processes that linger
                    match &by {
                        OursBy::Group if mp > 0 => tree.extend(proc::tree(mp, mp)),
                        OursBy::NewWithPath(sub) => {
                            for (pid, _, _) in proc::all() {
                                if !baseline.contains(&pid) && !tree.contains(&pid) {
                                    if let Some(p) = proc::path(pid) {
                                        if p.to_string_lossy().contains(sub.as_str()) {
                                            tree.insert(pid);
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    last_tree = Instant::now();
                }
                if matches!(by, OursBy::NewWithPath(_)) {
                    mp = if tree.is_empty() { 0 } else { -1 };
                }
                let c = mac::cursor();
                if (c.0 - last_cursor.0).abs() > 0.01 || (c.1 - last_cursor.1).abs() > 0.01 {
                    if rep.cursor_changes.len() < 60 {
                        rep.cursor_changes.push(format!("{ms}ms cursor=({:.1},{:.1})", c.0, c.1));
                    }
                    last_cursor = c;
                }
                let seed = mac::cursor_seed();
                if seed != last_seed {
                    if rep.cursor_shape_changes.len() < 60 {
                        rep.cursor_shape_changes.push(format!("{ms}ms seed={seed}"));
                    }
                    last_seed = seed;
                }
                let f = mac::front_pid();
                if f != last_front {
                    let ours = tree.contains(&f) || (mp > 0 && unsafe { libc::getpgid(f) } == mp);
                    if rep.front_changes.len() < 60 {
                        rep.front_changes.push(format!("{ms}ms front={} ({}){}", f, proc::name(f), if ours { " ★우리 프로세스★" } else { "" }));
                    }
                    last_front = f;
                }
                let our_front = tree.contains(&f) || (mp > 0 && unsafe { libc::getpgid(f) } == mp);
                if our_front {
                    rep.our_front += 1;
                }
                if mp != 0 {
                    for w in mac::windows(true) {
                        let ours = tree.contains(&w.pid) || (mp > 0 && unsafe { libc::getpgid(w.pid) } == mp);
                        if !ours {
                            continue;
                        }
                        if w.alpha > 0.0 {
                            rep.our_visible += 1;
                            if rep.our_visible_examples.len() < 10 {
                                rep.our_visible_examples.push(format!("{ms}ms {w:?}"));
                            }
                        } else {
                            rep.our_alpha0_onscreen += 1;
                            if rep.our_alpha0_examples.len() < 3 {
                                rep.our_alpha0_examples.push(format!("{ms}ms {w:?}"));
                            }
                        }
                    }
                }
                rep.samples += 1;
                let el = now.elapsed();
                if el < Duration::from_millis(20) {
                    std::thread::sleep(Duration::from_millis(20) - el);
                }
            }
            rep.secs = t0.elapsed().as_secs_f64();
            rep
        });
        Sampler { stop, main_pid, handle: Some(handle) }
    }

    fn finish(mut self) -> SamplerReport {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.take().map(|h| h.join().unwrap_or_default()).unwrap_or_default()
    }
}

#[cfg(windows)]
impl Sampler {
    fn start() -> Self {
        Self::start_with(OursBy::Group)
    }

    fn start_with(by: OursBy) -> Self {
        use crate::platform::win;
        let stop = Arc::new(AtomicBool::new(false));
        let main_pid = Arc::new(AtomicI32::new(0));
        let (s2, m2) = (stop.clone(), main_pid.clone());
        let handle = std::thread::spawn(move || {
            let t0 = Instant::now();
            let mut rep = SamplerReport::default();
            let mut last_cursor = win::cursor_pos();
            let mut last_seed = win_screen::cursor_shape();
            rep.cursor_shape_changes.push(format!("0ms 시작 shape={last_seed:#x}"));
            let mut last_front = win::foreground_pid().unwrap_or(0);
            rep.front_changes.push(format!("0ms 시작 front={} ({})", last_front, win_screen::proc_name(last_front)));
            rep.cursor_changes.push(format!("0ms 시작 cursor={last_cursor:?}"));
            let mut tree: HashSet<u32> = HashSet::new();
            let baseline: HashSet<u32> = win::processes().into_iter().map(|p| p.0).collect();
            // path comparison ignores case and separators
            let sub = match &by {
                OursBy::NewWithPath(s) => s.to_lowercase().replace('/', "\\"),
                OursBy::Group => String::new(),
            };
            let mut last_tree = Instant::now() - Duration::from_secs(1);
            let mut last_tick = Instant::now();
            while !s2.load(Ordering::SeqCst) {
                let now = Instant::now();
                let gap = now.duration_since(last_tick).as_secs_f64() * 1000.0;
                if rep.samples > 0 && gap > rep.max_gap_ms {
                    rep.max_gap_ms = gap;
                }
                last_tick = now;
                let ms = t0.elapsed().as_millis();
                let mp = m2.load(Ordering::SeqCst);
                if last_tree.elapsed() > Duration::from_millis(200) {
                    // keep the earlier list after exit too (union) to catch processes that linger
                    match &by {
                        OursBy::Group if mp > 0 => tree.extend(win_screen::our_pids(mp)),
                        OursBy::NewWithPath(_) => {
                            for (pid, _, _) in win::processes() {
                                if !baseline.contains(&pid) && !tree.contains(&pid) {
                                    if let Some(p) = win::process_path(pid) {
                                        if p.to_string_lossy().to_lowercase().contains(sub.as_str()) {
                                            tree.insert(pid);
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    last_tree = Instant::now();
                }
                let c = win::cursor_pos();
                if c != last_cursor {
                    if rep.cursor_changes.len() < 60 {
                        rep.cursor_changes.push(format!("{ms}ms cursor={c:?}"));
                    }
                    last_cursor = c;
                }
                let seed = win_screen::cursor_shape();
                if seed != last_seed {
                    if rep.cursor_shape_changes.len() < 60 {
                        rep.cursor_shape_changes.push(format!("{ms}ms shape={seed:#x}"));
                    }
                    last_seed = seed;
                }
                let f = win::foreground_pid().unwrap_or(0);
                if f != last_front {
                    let ours = tree.contains(&f);
                    if rep.front_changes.len() < 60 {
                        rep.front_changes.push(format!(
                            "{ms}ms front={} ({}){}",
                            f,
                            win_screen::proc_name(f),
                            if ours { " ★우리 프로세스★" } else { "" }
                        ));
                    }
                    last_front = f;
                }
                if f != 0 && tree.contains(&f) {
                    rep.our_front += 1;
                }
                if !tree.is_empty() {
                    let pids: Vec<u32> = tree.iter().copied().collect();
                    for t in win::windows_of(&pids) {
                        let s = win_screen::state(&t);
                        if s.on_screen || s.taskbar {
                            rep.our_visible += 1;
                            if rep.our_visible_examples.len() < 10 {
                                rep.our_visible_examples.push(format!("{ms}ms {s:?}"));
                            }
                        } else if t.visible && !t.cloaked && !t.minimized && win::intersects_any_monitor(t.rect) {
                            // a window on a monitor but transparent (alpha 0); informational
                            rep.our_alpha0_onscreen += 1;
                            if rep.our_alpha0_examples.len() < 3 {
                                rep.our_alpha0_examples.push(format!("{ms}ms {s:?}"));
                            }
                        }
                    }
                }
                rep.samples += 1;
                let el = now.elapsed();
                if el < Duration::from_millis(20) {
                    std::thread::sleep(Duration::from_millis(20) - el);
                }
            }
            rep.secs = t0.elapsed().as_secs_f64();
            rep
        });
        Sampler { stop, main_pid, handle: Some(handle) }
    }

    fn finish(mut self) -> SamplerReport {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.take().map(|h| h.join().unwrap_or_default()).unwrap_or_default()
    }
}

fn img_diff_frac(a: &[u8], b: &[u8]) -> f64 {
    let (Ok(a), Ok(b)) = (image::load_from_memory(a), image::load_from_memory(b)) else {
        return -1.0;
    };
    let (a, b) = (a.to_rgb8(), b.to_rgb8());
    if a.dimensions() != b.dimensions() {
        return 1.0;
    }
    let mut diff = 0u64;
    for (p, q) in a.pixels().zip(b.pixels()) {
        let d: i32 = (0..3).map(|i| (p[i] as i32 - q[i] as i32).abs()).sum();
        if d > 30 {
            diff += 1;
        }
    }
    diff as f64 / (a.width() * a.height()) as f64
}

const EDITOR_TEXT_JS: &str = r#"(() => { const e = document.querySelector('.part.editor .editor-group-container.active .monaco-editor .view-lines') || document.querySelector('.part.editor .monaco-editor .view-lines'); return e ? e.innerText.replace(/ /g, ' ') : null })()"#;

/// `ghostdesk --selftest-electron [output folder]`
pub async fn selftest(args: &[String]) -> Result<()> {
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = args;
        bail!("macOS·Windows 전용 셀프 테스트입니다");
    }
    #[cfg(any(target_os = "macos", windows))]
    {
        if args.first().map(String::as_str) == Some("sample") {
            let secs = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(30.0);
            let default_target = if cfg!(windows) { "chrome.exe" } else { "Google Chrome" };
            return sample_only(secs, args.get(2).cloned().unwrap_or_else(|| default_target.into())).await;
        }
        if args.first().map(String::as_str) == Some("orphan") {
            // to check the hidden app exits on its own even if GhostDesk is killed: launch and wait (kill -9
            // from outside)
            let code = detect_apps()
                .into_iter()
                .find(|a| a.bundle_id == "com.microsoft.VSCode" && a.hideable)
                .ok_or_else(|| anyhow!("VS Code 없음"))?;
            let mut o = ElectronLaunch::new(code, &format!("selftest-orphan-{}", std::process::id()), 1024, 700);
            o.vscode_settings.insert("chat.disableAIFeatures".into(), json!(true));
            let ws = launch(o, Arc::new(|_b: &[u8]| {})).await?;
            println!("PIDS {:?}", ws.pids());
            println!("WS {}", ws.paths.real_root.display());
            println!("LINK {}", ws.paths.root.display());
            tokio::time::sleep(Duration::from_secs(120)).await;
            ws.shutdown().await;
            return Ok(());
        }
        let outdir = args.first().map(PathBuf::from);
        if let Some(d) = &outdir {
            fs::create_dir_all(d)?;
        }
        selftest_static()?;

        let apps = detect_apps();
        let code = apps
            .iter()
            .find(|a| {
                a.bundle_id == "com.microsoft.VSCode"
                    || (cfg!(windows) && a.family == AppFamily::VsCode && a.name.contains("Visual Studio Code") && !a.name.contains("Insiders"))
            })
            .cloned()
            .ok_or_else(|| anyhow!("Visual Studio Code 를 찾지 못했습니다"))?;
        ensure!(code.hideable, "VS Code 가 숨김 모드를 지원하지 않습니다: {:?}", code.reason);

        // treat every process inside a newly created app bundle as ours, so window/front/Dock changes
        // right after launch (before launch finishes and main_pid is known) are caught too (if the user
        // opens the same app during the test it counts as a violation; conservative)
        let sampler =
            Sampler::start_with(OursBy::NewWithPath(format!("{}{}", code.bundle.display(), std::path::MAIN_SEPARATOR)));
        tokio::time::sleep(Duration::from_millis(500)).await;
        let res = selftest_live(code, outdir.as_deref(), &sampler).await;
        // keep observing for 3 s after shutdown
        tokio::time::sleep(Duration::from_secs(3)).await;
        let rep = sampler.finish();
        print_report(&rep);
        let r = res?;
        ensure!(rep.our_front == 0, "비간섭 위반: 우리 프로세스가 front 가 됨");
        ensure!(rep.our_visible == 0, "비간섭 위반: 우리 창이 화면에 보임");
        println!("\n{r}\n모든 Electron 셀프 테스트 통과");
        Ok(())
    }
}

#[cfg(any(target_os = "macos", windows))]
fn print_report(rep: &SamplerReport) {
    {
        println!("\n── 비간섭 샘플러 (20ms 간격) ──");
        println!(
            "  샘플 {}개 / {:.1}초 / 최대 간격 {:.0}ms",
            rep.samples, rep.secs, rep.max_gap_ms
        );
        println!("  커서 변화 {}건: {:?}", rep.cursor_changes.len().saturating_sub(1), rep.cursor_changes);
        println!("  커서 모양(seed) 변화 {}건: {:?}", rep.cursor_shape_changes.len().saturating_sub(1), rep.cursor_shape_changes);
        println!("  front 변화 {}건: {:?}", rep.front_changes.len().saturating_sub(1), rep.front_changes);
        println!("  우리 프로세스가 front 였던 샘플: {}", rep.our_front);
        println!("  우리 프로세스의 보이는 창(onscreen, alpha>0) 샘플: {} {:?}", rep.our_visible, rep.our_visible_examples);
        println!("  우리 프로세스의 onscreen alpha=0 창 샘플(정보): {} {:?}", rep.our_alpha0_onscreen, rep.our_alpha0_examples);
    }
}

/// `ghostdesk --selftest-electron sample <seconds> <part of executable path>`: observes
/// non-interference by the same criteria while another test (e.g. the browser --selftest) runs.
#[cfg(any(target_os = "macos", windows))]
async fn sample_only(secs: f64, sub: String) -> Result<()> {
    println!("비간섭 샘플러 {secs}초 시작 (새로 생긴 '{sub}' 프로세스를 우리 것으로 간주)");
    let s = Sampler::start_with(OursBy::NewWithPath(sub));
    tokio::time::sleep(Duration::from_secs_f64(secs)).await;
    let rep = s.finish();
    print_report(&rep);
    ensure!(rep.our_front == 0 && rep.our_visible == 0, "비간섭 위반");
    println!("비간섭 위반 없음");
    Ok(())
}

/// Fuse parsing (every installed app, read only) and a fake folder clone test.
fn selftest_static() -> Result<()> {
    println!("── 설치된 Electron 앱 (읽기 전용 검사) ──");
    let t = Instant::now();
    let apps = detect_apps();
    for a in &apps {
        println!(
            "  {} [{}] electron={} family={:?} hideable={} running={}\n     fuses: {}\n     userData: {:?} main: {:?}{}",
            a.name,
            a.bundle_id,
            a.electron_version.as_deref().unwrap_or("?"),
            a.family,
            a.hideable,
            a.running,
            a.fuses.as_ref().map(|f| format!("v{} {} ({})", f.version, f.wire, f.summary())).unwrap_or("?".into()),
            a.user_data_guess,
            a.main_script,
            a.reason.as_ref().map(|r| format!("\n     이유: {r}")).unwrap_or_default()
        );
        ensure!(a.fuses.is_some(), "{} 퓨즈 파싱 실패", a.name);
    }
    println!("  ({}개, {:.2}초)", apps.len(), t.elapsed().as_secs_f64());
    for (id, _) in NEVER_LAUNCH {
        if let Some(a) = apps.iter().find(|a| a.bundle_id == *id) {
            ensure!(!a.hideable, "{} 는 차단 목록인데 hideable", a.name);
        }
    }
    // Windows: also check apps blocked by executable name
    #[cfg(windows)]
    for a in &apps {
        if blocked_reason(&a.bundle_id, &a.exe).is_some() {
            ensure!(!a.hideable, "{} 는 차단 목록인데 hideable", a.name);
        }
    }
    println!("✔ 퓨즈 파싱 / 차단 목록");

    let tmp = std::env::temp_dir().join(format!("ghostdesk-electron-clone-{}", std::process::id()));
    let r = (|| -> Result<()> {
        let src = tmp.join("fake-src");
        for d in ["Cache/Cache_Data", "Code Cache/js", "GPUCache", "Local Storage/leveldb", "User/globalStorage", "Backups/1", "logs/x", "Partitions/p/Cache"] {
            fs::create_dir_all(src.join(d))?;
        }
        fs::write(src.join("Cookies"), "c")?;
        fs::write(src.join("Local Storage/leveldb/000003.log"), "ls")?;
        fs::write(src.join("User/settings.json"), "{}")?;
        fs::write(src.join("User/globalStorage/state.vscdb"), "db")?;
        fs::write(src.join("Cache/Cache_Data/big"), "x")?;
        fs::write(src.join("Code Cache/js/big"), "x")?;
        fs::write(src.join("Backups/1/unsaved"), "secret")?;
        fs::write(src.join("logs/x/log"), "x")?;
        fs::write(src.join("Partitions/p/Cookies"), "pc")?;
        fs::write(src.join("Partitions/p/Cache/big"), "x")?;
        fs::write(src.join("SingletonLock"), "x")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hosts", src.join("link-out"))?;
        let dst = tmp.join("dst");
        let st = clone_user_data(&src, &dst, &|_, _| {})?;
        ensure!(st.files == 5, "복제 파일 수 {} != 5", st.files);
        for ok in ["Cookies", "Local Storage/leveldb/000003.log", "User/settings.json", "User/globalStorage/state.vscdb", "Partitions/p/Cookies"] {
            ensure!(dst.join(ok).is_file(), "{ok} 누락");
        }
        for no in ["Cache", "Code Cache", "Backups", "logs", "Partitions/p/Cache", "SingletonLock", "link-out"] {
            ensure!(!dst.join(no).exists(), "{no} 가 복제됨");
        }

        // the whole workspace preparation path (fake VS Code family app + fake original, nothing launched)
        let fake = ElectronApp {
            name: "Fake Code".into(),
            bundle_id: "test.ghostdesk.fake".into(),
            bundle: tmp.join("Fake.app"),
            exe: tmp.join("Fake.app/Contents/MacOS/Fake"),
            app_version: None,
            electron_version: None,
            fuses: None,
            hideable: false,
            reason: None,
            family: AppFamily::VsCode,
            user_data_guess: None,
            main_script: None,
            running: false,
            launch_profile: String::new(),
        };
        let mut o = ElectronLaunch::new(fake, "clone test", 1024, 768);
        o.workspaces_root = Some(tmp.join("ws"));
        o.profile = ElectronProfile::Clone { source: src.clone(), refresh: false };
        let p = prepare_workspace(&o)?;
        // Windows uses the real folder without a short link (named pipes have no path length limit)
        #[cfg(windows)]
        let link_ok = p.root == p.real_root;
        #[cfg(not(windows))]
        let link_ok = fs::read_link(&p.root).ok() == Some(p.real_root.clone());
        let ud = p.real_root.join("user-data");
        let set: Value = serde_json::from_str(&fs::read_to_string(ud.join("User/settings.json"))?)?;
        let (args, env) = launch_plan(&o, &p);
        if p.root != p.real_root {
            let _ = fs::remove_file(&p.root);
        }
        ensure!(link_ok, "짧은 링크가 실제 작업공간을 가리키지 않음");
        #[cfg(windows)]
        ensure!(args.iter().any(|a| a == "--disable-features=CalculateNativeWinOcclusion"), "가림 추적 끄기 인자 누락");
        ensure!(ud.join("Cookies").is_file() && !ud.join("Cache").exists(), "작업공간 복제 결과 이상");
        ensure!(set["window.dialogStyle"] == json!("custom") && set["update.mode"] == json!("none"), "설정 병합 실패: {set}");
        ensure!(p.real_root.join("argv.json").is_file(), "argv.json 없음");
        ensure!(env.iter().any(|(k, v)| k == "VSCODE_PORTABLE" && Path::new(v) == p.root), "VSCODE_PORTABLE 누락");
        ensure!(args.iter().any(|a| a.starts_with("--extensions-dir=") && a.contains(&*p.root.to_string_lossy())), "--extensions-dir 누락");
        ensure!(args.iter().any(|a| a == "--inspect-brk=0"), "--inspect-brk 누락");
        println!("  작업공간 준비: 링크 {} → {}", p.root.display(), p.real_root.display());
        // refuse another app trying to use the same workspace
        let mut o2 = o.clone();
        o2.app.bundle_id = "other.app".into();
        ensure!(prepare_workspace(&o2).is_err(), "다른 앱의 작업공간 재사용이 허용됨");
        Ok(())
    })();
    let _ = fs::remove_dir_all(&tmp);
    r?;
    println!("✔ userData 복제 + 작업공간 준비 (가짜 폴더, 캐시/잠금/백업/링크 제외, 설정 병합, 짧은 링크)");
    Ok(())
}

#[cfg(any(target_os = "macos", windows))]
async fn selftest_live(code: ElectronApp, outdir: Option<&Path>, sampler: &Sampler) -> Result<String> {
    use std::sync::atomic::AtomicUsize;
    let slug = format!("selftest-electron-{}", std::process::id());
    let root = crate::platform::paths::workspaces_root();
    let real = root.join(&slug);
    ensure!(!real.exists(), "테스트 작업공간이 이미 있습니다: {}", real.display());
    let mut opts = ElectronLaunch::new(code, &slug, 1280, 800);
    opts.vscode_settings.insert("chat.disableAIFeatures".into(), json!(true));
    let frames = Arc::new(AtomicUsize::new(0));
    let f2 = frames.clone();
    let t0 = Instant::now();
    let launched = launch(opts, Arc::new(move |_b: &[u8]| {
        f2.fetch_add(1, Ordering::Relaxed);
    }))
    .await;
    let ws = match launched {
        Ok(w) => w,
        Err(e) => {
            let _ = fs::remove_dir_all(&real);
            return Err(e);
        }
    };
    sampler.main_pid.store(ws.main_pid, Ordering::SeqCst);
    println!("\n✔ VS Code 숨김 실행 + CDP 연결 ({:.1}초) pid={} pgid={}", t0.elapsed().as_secs_f64(), ws.main_pid, ws.pgid);
    let r = selftest_drive(&ws, outdir, &frames).await;
    let pids_before = ws.pids();
    let st = ws.watch_stats();
    println!("  감시자: {st:?}");
    ws.shutdown().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    #[cfg(unix)]
    let alive: Vec<i32> = pids_before.iter().copied().filter(|p| unsafe { libc::kill(*p, 0) } == 0).collect();
    #[cfg(windows)]
    let alive: Vec<i32> = pids_before.iter().copied().filter(|p| crate::platform::proc::pid_alive(*p as u32)).collect();
    println!("✔ 종료: 프로세스 {}개 중 남은 것 {:?}", pids_before.len(), alive);
    let link_gone = !ws.paths.root.exists();
    #[cfg(unix)]
    let _ = fs::remove_dir_all(&real);
    // Windows: files held by a process that just exited may linger briefly, so retry
    #[cfg(windows)]
    let _ = crate::platform::fsx::remove_dir_all_retry(&real);
    println!("  작업공간 삭제: {} (짧은 링크 제거: {link_gone})", !real.exists());
    let summary = r?;
    ensure!(alive.is_empty(), "종료 후 남은 프로세스: {alive:?}");
    ensure!(!st.killed, "감시자가 앱을 강제 종료함");
    Ok(summary)
}

#[cfg(any(target_os = "macos", windows))]
async fn selftest_drive(ws: &ElectronWorkspace, outdir: Option<&Path>, frames: &std::sync::atomic::AtomicUsize) -> Result<String> {
    let save = |name: &str, png: &[u8]| {
        if let Some(d) = outdir {
            let _ = fs::write(d.join(format!("{name}.png")), png);
        }
    };
    // wait for the workbench to be ready
    let start = Instant::now();
    loop {
        if ws.eval("!!document.querySelector('.monaco-workbench .part.editor')").await.ok() == Some(json!(true)) {
            break;
        }
        ensure!(start.elapsed() < Duration::from_secs(30), "워크벤치가 30초 안에 준비되지 않았습니다");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let doc = ws
        .eval("({w: innerWidth, h: innerHeight, dpr: devicePixelRatio, vis: document.visibilityState, focus: document.hasFocus()})")
        .await?;
    println!("  문서 상태: {doc}");
    let main = ws
        .main_eval("(() => { const G = globalThis.__ghostdesk; const B = G.electron.BrowserWindow; return { mode: G.cfg.mode, bounds: B.getAllWindows().map(w => w.getBounds()), log: G.log.filter(l => !l.startsWith('ok wc') && !l.startsWith('ok setup') && !l.startsWith('ok session')), wins: B.getAllWindows().map(w => ({ vis: G.orig.isVisible.call(w), op: G.orig.getOpacity.call(w), size: G.orig.getContentSize.call(w), focusable: w.isFocusable() })), policy: G.electron.app.getActivationPolicy ? G.electron.app.getActivationPolicy() : null, blocked: G.blocked }; })()")
        .await?;
    println!("  메인 프로세스 상태: {main}");

    let s1 = ws.screenshot().await?;
    let img = image::load_from_memory(&s1)?;
    ensure!(img.width() == 1280 && img.height() == 800, "스크린샷 크기 {}x{}", img.width(), img.height());
    save("1_initial", &s1);
    println!("✔ 스크린샷 {}x{} ({} KB)", img.width(), img.height(), s1.len() / 1024);

    let tabs_js = "(() => ({editors: document.querySelectorAll('.part.editor .monaco-editor').length, tabs: [...document.querySelectorAll('.tab .label-name')].map(t => t.innerText)}))()";
    let before = ws.eval(tabs_js).await?;
    ws.key("cmd+n", 1).await?;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let after = ws.eval(tabs_js).await?;
    println!("  cmd+n 전 {before} → 후 {after}");
    ensure!(after["editors"].as_u64().unwrap_or(0) > before["editors"].as_u64().unwrap_or(0), "cmd+n 으로 새 편집기가 열리지 않음");
    let s2 = ws.screenshot().await?;
    save("2_after_cmd_n", &s2);
    println!("✔ cmd+n 새 편집기 (화면 변화 {:.1}%)", img_diff_frac(&s1, &s2) * 100.0);

    ws.type_text("hello ghostdesk 안녕").await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let text = ws.eval(EDITOR_TEXT_JS).await?;
    println!("  편집기 내용: {text}");
    ensure!(text.as_str().unwrap_or("").contains("hello ghostdesk 안녕"), "타이핑 결과 불일치: {text}");
    let s3 = ws.screenshot().await?;
    save("3_after_typing", &s3);
    let d23 = img_diff_frac(&s2, &s3);
    ensure!(d23 > 0.0005, "타이핑 후 화면 변화가 없음 ({d23})");
    println!("✔ 영문+한글 타이핑 (DOM 확인, 화면 변화 {:.2}%)", d23 * 100.0);

    let sb = "(() => { const e = document.querySelector('.part.sidebar'); return e ? (getComputedStyle(e).display + '/' + e.offsetWidth) : 'none' })()";
    let rect = ws
        .eval("(() => { const a = document.querySelector('.activitybar .action-item .codicon-explorer-view-icon') || document.querySelector('.activitybar .action-item a'); if (!a) return null; const r = a.getBoundingClientRect(); return {x: r.x + r.width/2, y: r.y + r.height/2, label: a.getAttribute('aria-label')} })()")
        .await?;
    let sb0 = ws.eval(sb).await?;
    let (x, y) = (rect["x"].as_f64().ok_or_else(|| anyhow!("탐색기 아이콘 없음"))?, rect["y"].as_f64().unwrap_or(0.0));
    ws.click(Some((x, y)), MouseButton::Left, 1, 0).await?;
    tokio::time::sleep(Duration::from_millis(900)).await;
    let sb1 = ws.eval(sb).await?;
    let s4 = ws.screenshot().await?;
    save("4_after_click", &s4);
    println!("  탐색기 아이콘 {rect} 클릭: 사이드바 {sb0} → {sb1}");
    ensure!(sb0 != sb1, "클릭 후 사이드바 상태가 바뀌지 않음");
    println!("✔ 마우스 클릭 (화면 변화 {:.1}%)", img_diff_frac(&s3, &s4) * 100.0);

    ws.scroll(Some((640.0, 400.0)), 0.0, 300.0, 0).await?;
    let z = ws.zoom([0.0, 0.0, 320.0, 200.0]).await?;
    save("5_zoom", &z);
    println!("✔ scroll / zoom ({} KB)", z.len() / 1024);

    let (views, active) = ws.views();
    for (i, v) in views.iter().enumerate() {
        println!("  뷰 [{i}] {} — {} {}", v.title, v.detail, if Some(&v.id) == active.as_ref() { "▶" } else { "" });
    }
    ensure!(!views.is_empty(), "뷰 목록이 비었음");
    ws.switch_view(0).await?;
    println!("✔ views / switch_view");

    // check the socket path length (lsof, read only)
    #[cfg(unix)]
    {
        let lsof = Command::new("lsof").args(["-a", "-p", &ws.main_pid.to_string(), "-U", "-F", "n"]).output();
        if let Ok(o) = lsof {
            let socks: Vec<String> = String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| l.starts_with('n') && l.contains(".sock"))
                .map(|l| l[1..].to_string())
                .collect();
            for s in &socks {
                println!("  unix 소켓: {s} ({}바이트)", s.len());
            }
            ensure!(socks.iter().all(|s| s.len() < SUN_PATH_MAX), "소켓 경로가 너무 김");
        }
    }
    // Windows: VS Code main IPC is a named pipe (names listed only, no connection; no length limit)
    #[cfg(windows)]
    if let Ok(rd) = fs::read_dir(r"\\.\pipe\") {
        let n = rd.flatten().filter(|e| e.file_name().to_string_lossy().ends_with("-main-sock")).count();
        println!("  VS Code 메인 IPC 명명 파이프(-main-sock): {n}개 (사용자 인스턴스 포함)");
    }
    let menu = ws.menu_text(MenuSource::App).await.unwrap_or_else(|e| format!("(메뉴 없음: {e})"));
    println!("  앱 메뉴 (앞부분): {}", menu.lines().take(6).collect::<Vec<_>>().join(" / "));

    // check the blocking hooks: only call after confirming the function is patched (so no real
    // browser or dialog appears)
    let hooked = ws
        .main_eval("(() => { const e = globalThis.__ghostdesk.electron; return String(e.shell.openExternal).includes('외부 링크') && String(e.dialog.showMessageBox).includes('메시지 대화상자') && String(e.dialog.showOpenDialog).includes('파일 열기') && String(e.app.emit).includes('open-file'); })()")
        .await?;
    ensure!(hooked == json!(true), "차단 훅이 설치되지 않음");
    ws.main_eval("(() => { const e = globalThis.__ghostdesk.electron; e.dialog.showMessageBox({ message: 'GhostDesk 셀프 테스트 대화상자' }); e.shell.openExternal('https://example.com/ghostdesk-selftest'); e.dialog.showOpenDialog({ title: 'GhostDesk 테스트' }); new e.Notification({ title: 'GhostDesk', body: '테스트' }).show(); e.app.emit('open-file', { preventDefault() {} }, '/nonexistent/ghostdesk-selftest.txt'); return true; })()")
        .await?;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let notes = ws.take_notes();
    println!("  메모: {notes:#?}");
    ensure!(notes.iter().any(|n| n.contains("메시지 대화상자")), "대화상자 차단 메모 없음");
    ensure!(notes.iter().any(|n| n.contains("외부 링크")), "외부 링크 차단 메모 없음");
    ensure!(notes.iter().any(|n| n.contains("ghostdesk-selftest.txt")), "open-file 차단 메모 없음");
    println!("✔ 대화상자/외부 링크/알림/open-file 차단 + 메모");

    println!("  프로세스: {:?}", ws.pids());
    println!("✔ 라이브 프레임 수신: {}개", frames.load(Ordering::Relaxed));
    Ok(format!("VS Code 숨김 실행 테스트 완료 (프레임 {}개)", frames.load(Ordering::Relaxed)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuse_block_parsing() {
        let mut buf = b"xxxx".to_vec();
        buf.extend_from_slice(FUSE_SENTINEL);
        buf.extend_from_slice(&[1, 9]);
        buf.extend_from_slice(b"101100r1?");
        buf.extend_from_slice(b"tail");
        let f = parse_fuses(&buf).unwrap();
        assert_eq!(f.version, 1);
        assert_eq!(f.wire, "101100r1?");
        assert!(f.is_on("RunAsNode"));
        assert!(!f.is_on("EnableCookieEncryption"));
        assert!(f.inspect_allowed());
        assert_eq!(f.get("LoadBrowserProcessSpecificV8Snapshot"), Some(FuseState::Removed));
        assert_eq!(f.get("WasmTrapHandlers"), Some(FuseState::Unknown(b'?')));
        assert!(parse_fuses(b"no sentinel here").is_none());
    }

    #[test]
    fn installed_apps_fuses() {
        // every installed app (read only). If VS Code is present, its inspector fuse must be on.
        for a in detect_apps() {
            assert!(a.fuses.is_some(), "{} 퓨즈 파싱 실패", a.name);
            if a.bundle_id == "com.microsoft.VSCode" {
                assert!(a.hideable);
                assert_eq!(a.family, AppFamily::VsCode);
                assert_eq!(a.main_script.as_deref(), Some("out/main.js"));
            }
            if NEVER_LAUNCH.iter().any(|(id, _)| *id == a.bundle_id) {
                assert!(!a.hideable);
            }
        }
    }

    #[test]
    fn clone_fake_dir() {
        selftest_static().unwrap();
    }

    #[test]
    fn settings_merge() {
        let tmp = std::env::temp_dir().join(format!("gd-set-{}", std::process::id()));
        let p = tmp.join("User/settings.json");
        let mut m = Map::new();
        m.insert("a.b".into(), json!(true));
        write_vscode_settings(&p, &m).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["a.b"], json!(true));
        fs::write(&p, "// c\n{\n \"x\": 1,\n}\n").unwrap();
        write_vscode_settings(&p, &m).unwrap();
        let s = fs::read_to_string(&p).unwrap();
        assert!(s.contains("\"a.b\": true,") && s.contains("\"x\": 1"));
        fs::remove_dir_all(&tmp).ok();
    }

    /// Windows: the terminal's right click must neither paste nor overwrite the user's real clipboard
    /// (macOS keeps the existing settings).
    #[test]
    fn vscode_terminal_clipboard_defaults() {
        let m = vscode_default_settings();
        if cfg!(windows) {
            assert_eq!(m["terminal.integrated.rightClickBehavior"], json!("default"));
            assert_eq!(m["terminal.integrated.copyOnSelection"], json!(false));
            assert_eq!(m.len(), 18);
        } else {
            assert!(!m.contains_key("terminal.integrated.rightClickBehavior"));
            assert!(!m.contains_key("terminal.integrated.copyOnSelection"));
            assert_eq!(m.len(), 16);
        }
        assert_eq!(m["window.menuStyle"], json!("custom"));
    }

    #[test]
    fn disable_features_are_merged() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (f, rest) = split_disable_features(&a(&[
            "--foo",
            "--disable-features=HardwareMediaKeyHandling,X",
            "-Disable-Features=Y",
            "/disable-features=X,Z",
            "--enable-features=Q",
            "--disable-features-x=1",
            "--",
            "--disable-features=AfterEnd",
        ]));
        assert_eq!(f, a(&["HardwareMediaKeyHandling,X", "Y", "X,Z"]));
        assert_eq!(rest, a(&["--foo", "--enable-features=Q", "--disable-features-x=1", "--", "--disable-features=AfterEnd"]));
        assert_eq!(merge_features(&f, "CalculateNativeWinOcclusion"), "HardwareMediaKeyHandling,X,Y,Z,CalculateNativeWinOcclusion");
        assert_eq!(merge_features(&[], "CalculateNativeWinOcclusion"), "CalculateNativeWinOcclusion");
        assert_eq!(merge_features(&a(&[" A , ,CalculateNativeWinOcclusion"]), "CalculateNativeWinOcclusion"), "A,CalculateNativeWinOcclusion");
    }

    #[test]
    fn console_windows_are_hidden_not_violations() {
        let w = |hwnd: isize, class: &str, on_screen: bool, taskbar: bool| WinState {
            pid: 7,
            hwnd,
            class: class.into(),
            rect: (0, 0, 100, 100),
            alpha: None,
            on_screen,
            taskbar,
        };
        let wins = [
            w(1, CONSOLE_CLASS, true, true),
            w(2, CONSOLE_CLASS, false, true),
            w(3, CONSOLE_CLASS, false, false), // hidden console (windowsHide): nothing to do
            w(4, "Chrome_WidgetWin_1", true, false),
            w(5, CONSOLE_CLASS, true, false),
        ];
        let now = Instant::now();
        let mut first = HashMap::new();
        first.insert(5isize, now - CONSOLE_HIDE_GRACE); // a window still visible past the grace period
        first.insert(2isize, now - Duration::from_millis(200)); // being hidden (still within the grace period after several 40 ms pre-watch passes)
        let (hide, skip) = plan_console_hide(&wins, &first, now);
        assert_eq!(hide, vec![1, 2]);
        assert_eq!(skip, HashSet::from([1, 2]));
        // once the grace period passes, 2 is given up on too
        let (hide2, _) = plan_console_hide(&wins, &first, now + CONSOLE_HIDE_GRACE);
        assert_eq!(hide2, vec![1]);
        // what's left: the app window (4) and the given-up console (5) are violations
        let rest: Vec<WinState> = wins.iter().filter(|x| !skip.contains(&x.hwnd)).cloned().collect();
        let d = describe_violations(&rest, None).unwrap();
        assert!(d.contains("hwnd=0x4") && d.contains("hwnd=0x5") && !d.contains("hwnd=0x1"), "{d}");
    }

    /// Checks PATCH_JS's Windows helpers (child_process windowsHide default, disable-features merging,
    /// OS drag blocking) and the menuKey filter in the V8 of headless Chrome (empty temp profile), and
    /// that the whole PATCH_JS compiles without syntax errors.
    #[cfg(windows)]
    #[test]
    fn patch_js_helpers_in_v8() {
        let Some(chrome) = ["chrome.exe", "msedge.exe"].iter().find_map(|e| crate::platform::win::app_path(e)) else { return };
        let a = PATCH_JS.find("/*<gd-win-helpers>*/").unwrap();
        let b = PATCH_JS.find("/*</gd-win-helpers>*/").unwrap();
        let helpers = &PATCH_JS[a..b];
        let mk = PATCH_JS.find("const menuKey").unwrap();
        let menu_key = &PATCH_JS[mk..mk + PATCH_JS[mk..].find("};").unwrap() + 2];
        let whole = PATCH_JS.replace("__CFG__", "{}");
        let js = format!(
            r#"<script>
const out = {{}};
try {{ new Function({whole}); out.compiles = true; }} catch (e) {{ out.compiles = String(e); }}
try {{
{helpers}
{menu_key}
const P = Symbol.for('nodejs.util.promisify.custom');
const calls = [];
const enc = (a) => a.map(x => typeof x === 'function' ? 'fn' : (x === undefined ? 'undef' : x));
const mk = (name) => function (...a) {{ calls.push([name, enc(a)]); const cb = a[a.length - 1]; if (typeof cb === 'function') cb(null, 'out', 'err'); return {{ pid: 1 }}; }};
const cp = {{}};
for (const n of ['spawn', 'spawnSync', 'execFile', 'execFileSync', 'fork', 'exec', 'execSync']) cp[n] = mk(n);
Object.defineProperty(cp.exec, P, {{ enumerable: false, value: () => 'orig' }});
Object.defineProperty(cp.execFile, P, {{ enumerable: false, value: () => 'orig' }});
cp.exec.extra = 42;
gdWrapCp(cp); gdWrapCp(cp);
const fn = () => {{}};
const o = {{ cwd: 'c' }};
cp.spawn('a'); cp.spawn('a', ['x']); cp.spawn('a', ['x'], o); cp.spawn('a', {{ cwd: 'c' }}); cp.spawn('a', ['x'], {{ windowsHide: false }});
cp.spawn('a', ['x'], null); cp.spawnSync('a', undefined, {{ x: 1 }});
cp.execFile('f', fn); cp.execFile('f', ['x'], fn); cp.execFile('f', ['x'], null, fn); cp.execFile('f', {{ env: 1 }}, fn); cp.execFileSync('f', ['x']);
cp.exec('c'); cp.exec('c', fn); cp.exec('c', {{ shell: 'x' }}, fn); cp.execSync('c', {{ windowsHide: false }});
cp.fork('m'); cp.fork('m', {{ silent: true }});
out.calls = calls; out.notMutated = o.windowsHide === undefined; out.extra = cp.exec.extra; out.marked = cp.spawn.__gdHide === true;
const p = cp.exec[P]('c2');
out.child = p.child && p.child.pid;
// gdGuardDrag: checks startDrag blocking and webview guest drag interception with a fake webContents/debugger
const mkWc = (type, failIntercept) => {{
  const ev = {{}}, dev = {{}}, cmds = [];
  const dbg = {{ attached: false, isAttached() {{ return this.attached; }}, attach(v) {{ this.attached = true; cmds.push(['attach', v]); }},
    on(n, f) {{ dev[n] = f; }},
    sendCommand(m, q) {{ cmds.push([m, q]); return (failIntercept && m === 'Input.setInterceptDrags') ? Promise.reject(new Error('nope')) : Promise.resolve({{}}); }} }};
  function WC() {{}}
  WC.prototype.startDrag = function () {{ cmds.push(['osdrag']); }};
  const wc = new WC();
  wc.getType = () => type; wc.debugger = dbg; wc.isDestroyed = () => false;
  wc.on = (n, f) => {{ ev[n] = f; }};
  wc.executeJavaScript = (js) => {{ cmds.push(['js', js.includes('dragstart') && js.includes('preventDefault')]); return Promise.resolve(); }};
  return {{ wc, dbg, dev, ev, cmds, WC }};
}};
const dragTest = async () => {{
  const r = {{}}, notes = [], logs = [];
  const nt = (s) => notes.push(s), lg = (s) => logs.push(s);
  const w = mkWc('window');
  r.window = await gdGuardDrag(w.wc, nt, lg);
  w.wc.startDrag({{ file: 'x' }});
  new w.WC().startDrag({{ file: 'y' }});
  r.windowCmds = w.cmds;
  const g = mkWc('webview');
  r.webview = await gdGuardDrag(g.wc, nt, lg);
  g.dev.message({{}}, 'Input.dragIntercepted', {{ data: {{ items: [{{ mimeType: 'text/plain', data: 'x' }}], dragOperationsMask: 1 }} }});
  g.dev.message({{}}, 'Input.other', {{}});
  g.dev.detach({{}}, 'target closed');
  g.dbg.attached = false;
  g.dev.detach({{}}, 'canceled_by_user');
  await new Promise((res) => setTimeout(res, 0));
  r.webviewCmds = g.cmds;
  const f = mkWc('webview', true);
  r.fallback = await gdGuardDrag(f.wc, nt, lg);
  f.ev['dom-ready']();
  r.fallbackCmds = f.cmds;
  r.notes = notes.length;
  return r;
}};
Promise.all([p, dragTest()]).then(([r, d]) => {{ out.promisified = r; out.lastCall = calls[calls.length - 1]; out.drag = d; document.body.textContent = 'RES:' + JSON.stringify(out); }},
  (e) => {{ out.error = String(e && e.stack || e); document.body.textContent = 'RES:' + JSON.stringify(out); }});
out.merge = [gdMergeFeatures('A,B, CalculateNativeWinOcclusion,A'), gdMergeFeatures(undefined)];
out.menu = [{{ key: 'f', code: 'KeyF', alt: true }}, {{ key: 'F', code: 'KeyF', alt: true, shift: true }}, {{ code: 'KeyX', alt: true, meta: true }},
  {{ code: 'Space', alt: true, shift: true }}, {{ key: 'Alt' }}, {{ key: 'Alt', shift: true }}, {{ key: 'F10' }}, {{ key: 'F10', control: true }},
  {{ code: 'KeyF', alt: true, control: true }}, {{ key: 'ContextMenu' }}, {{ key: '1', code: 'Digit1', alt: true }}].map(menuKey);
}} catch (e) {{ out.error = String(e && e.stack || e); }}
document.body.textContent = 'RES:' + JSON.stringify(out);
</script>"#,
            whole = serde_json::to_string(&whole).unwrap(),
        );
        let html = std::env::temp_dir().join(format!("gd-v8p-{}.html", std::process::id()));
        fs::write(&html, format!("<!doctype html><html><body>{js}</body></html>")).unwrap();
        let prof = std::env::temp_dir().join(format!("gd-v8p-{}", std::process::id()));
        let out = Command::new(chrome)
            .args(["--headless=new", "--disable-gpu", "--no-first-run", "--no-default-browser-check", "--dump-dom"])
            .arg(format!("--user-data-dir={}", prof.display()))
            .arg(&html)
            .output()
            .unwrap();
        let dom = String::from_utf8_lossy(&out.stdout).to_string();
        let _ = crate::platform::fsx::remove_dir_all_retry(&prof);
        let _ = fs::remove_file(&html);
        let res = dom.split("RES:").nth(1).unwrap_or_else(|| panic!("결과 없음: {dom}"));
        let res = &res[..res.rfind('}').unwrap() + 1];
        let v: Value = serde_json::from_str(&res.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"")).unwrap();
        assert_eq!(v["compiles"], json!(true), "{v}");
        assert!(v.get("error").is_none(), "{v}");
        let wh = json!({ "windowsHide": true });
        let expect = json!([
            ["spawn", ["a", "undef", wh]],
            ["spawn", ["a", ["x"], wh]],
            ["spawn", ["a", ["x"], { "cwd": "c", "windowsHide": true }]],
            ["spawn", ["a", { "cwd": "c", "windowsHide": true }]],
            ["spawn", ["a", ["x"], { "windowsHide": false }]],
            ["spawn", ["a", ["x"], null]],
            ["spawnSync", ["a", "undef", { "x": 1, "windowsHide": true }]],
            ["execFile", ["f", wh, "fn"]],
            ["execFile", ["f", ["x"], wh, "fn"]],
            ["execFile", ["f", ["x"], wh, "fn"]],
            ["execFile", ["f", { "env": 1, "windowsHide": true }, "fn"]],
            ["execFileSync", ["f", ["x"], wh]],
            ["exec", ["c", wh]],
            ["exec", ["c", wh, "fn"]],
            ["exec", ["c", { "shell": "x", "windowsHide": true }, "fn"]],
            ["execSync", ["c", { "windowsHide": false }]],
            ["fork", ["m", "undef", wh]],
            ["fork", ["m", { "silent": true, "windowsHide": true }]],
            // the util.promisify(exec) path also goes through the wrapped function
            ["exec", ["c2", wh, "fn"]]
        ]);
        assert_eq!(v["calls"], expect, "{v}");
        assert_eq!(v["notMutated"], json!(true));
        assert_eq!(v["extra"], json!(42));
        assert_eq!(v["marked"], json!(true));
        assert_eq!(v["child"], json!(1));
        assert_eq!(v["promisified"], json!({ "stdout": "out", "stderr": "err" }), "{v}");
        assert_eq!(v["lastCall"], json!(["exec", ["c2", wh, "fn"]]), "{v}");
        assert_eq!(v["merge"], json!(["A,B,CalculateNativeWinOcclusion", "CalculateNativeWinOcclusion"]));
        assert_eq!(v["menu"], json!([true, false, false, true, true, true, true, false, false, true, false]), "{v}");
        // startDrag is blocked on both instances and the prototype (the original is never called), and
        // regular windows get no debugger attached
        let d = &v["drag"];
        assert_eq!(d["window"], json!("start"), "{v}");
        assert_eq!(d["windowCmds"], json!([]), "{v}");
        // webview guest: enable interception → cancel the intercepted drag → (target closed is ignored)
        // reattach and re-enable after detaching
        let on = json!(["Input.setInterceptDrags", { "enabled": true }]);
        let data = json!({ "items": [{ "mimeType": "text/plain", "data": "x" }], "dragOperationsMask": 1 });
        assert_eq!(d["webview"], json!("webview"), "{v}");
        assert_eq!(
            d["webviewCmds"],
            json!([
                ["attach", "1.3"],
                on,
                ["Input.dispatchDragEvent", { "type": "dragCancel", "x": 0, "y": 0, "data": data }],
                ["attach", "1.3"],
                on
            ]),
            "{v}"
        );
        // if interception can't be enabled, block dragstart in the guest document (now + on every
        // dom-ready)
        assert_eq!(d["fallback"], json!("fallback"), "{v}");
        assert_eq!(d["fallbackCmds"], json!([["attach", "1.3"], on, ["js", true], ["js", true]]), "{v}");
        assert_eq!(d["notes"], json!(4), "{v}");
    }

    /// Windows: user-data from an unfinished clone (marker left behind) isn't reused on the next run
    /// but cloned again.
    ///
    /// A failure on locked cookies is reported as CloneLocked and leaves the marker.
    #[cfg(windows)]
    #[test]
    fn windows_unfinished_clone_is_not_reused() {
        use std::os::windows::fs::OpenOptionsExt;
        let tmp = std::env::temp_dir().join(format!("gd-ws-mark-{}", std::process::id()));
        let _ = crate::platform::fsx::remove_dir_all_retry(&tmp);
        let src = tmp.join("src");
        fs::create_dir_all(src.join("Partitions/p/Network")).unwrap();
        fs::write(src.join("Preferences"), "{}").unwrap();
        fs::write(src.join("Partitions/p/Network/Cookies"), "c").unwrap();
        let fake = ElectronApp {
            name: "Fake".into(),
            bundle_id: "test.ghostdesk.mark".into(),
            bundle: tmp.join("app"),
            exe: tmp.join("app/Fake.exe"),
            app_version: None,
            electron_version: None,
            fuses: None,
            hideable: false,
            reason: None,
            family: AppFamily::Generic,
            user_data_guess: None,
            main_script: None,
            running: false,
            launch_profile: String::new(),
        };
        let mut o = ElectronLaunch::new(fake, "mark", 1024, 768);
        o.workspaces_root = Some(tmp.join("ws"));
        o.profile = ElectronProfile::Clone { source: src.clone(), refresh: false };
        // 1) if the original app has the cookies locked: CloneLocked, and the marker remains
        let held = fs::OpenOptions::new().read(true).write(true).share_mode(0).open(src.join("Partitions/p/Network/Cookies")).unwrap();
        let err = prepare_workspace(&o).err().expect("잠긴 쿠키면 실패해야 함");
        assert!(err.downcast_ref::<profiles::CloneLocked>().is_some(), "{err:#}");
        let root = tmp.join("ws").join(profiles::slug("mark"));
        assert!(root.join(".ghostdesk-cloning").exists());
        drop(held);
        // 2) a half clone left after a failed delete: with the marker present, it's cloned again rather
        // than reused
        fs::create_dir_all(root.join("user-data")).unwrap();
        fs::write(root.join("user-data/stale"), "partial").unwrap();
        let p = prepare_workspace(&o).unwrap();
        let ud = p.real_root.join("user-data");
        assert!(!ud.join("stale").exists(), "반쪽 복제본을 재사용함");
        assert!(ud.join("Partitions/p/Network/Cookies").is_file() && ud.join("Preferences").is_file());
        assert!(!root.join(".ghostdesk-cloning").exists());
        // 3) a complete clone is reused as is (unless refresh)
        fs::write(ud.join("kept"), "x").unwrap();
        prepare_workspace(&o).unwrap();
        assert!(ud.join("kept").exists());
        crate::platform::fsx::remove_dir_all_retry(&tmp).unwrap();
    }

    /// Windows: --disable-features from extra_args is merged into one switch with the occlusion
    /// tracking opt-out.
    #[cfg(windows)]
    #[test]
    fn windows_launch_plan_single_disable_features() {
        let tmp = std::env::temp_dir().join(format!("gd-ws-df-{}", std::process::id()));
        let fake = ElectronApp {
            name: "Fake".into(),
            bundle_id: "test.ghostdesk.df".into(),
            bundle: tmp.join("app"),
            exe: tmp.join("app/Fake.exe"),
            app_version: None,
            electron_version: None,
            fuses: None,
            hideable: false,
            reason: None,
            family: AppFamily::Generic,
            user_data_guess: None,
            main_script: None,
            running: false,
            launch_profile: String::new(),
        };
        let mut o = ElectronLaunch::new(fake, "df", 1024, 768);
        o.workspaces_root = Some(tmp.join("ws"));
        o.extra_args = vec!["--disable-features=HardwareMediaKeyHandling".into(), "--lang=ko".into()];
        let p = prepare_workspace(&o).unwrap();
        let (args, _) = launch_plan(&o, &p);
        let df: Vec<&String> = args.iter().filter(|a| a.to_ascii_lowercase().contains("disable-features")).collect();
        assert_eq!(df, vec!["--disable-features=HardwareMediaKeyHandling,CalculateNativeWinOcclusion"], "{args:?}");
        assert!(args.iter().any(|a| a == "--lang=ko"));
        let _ = crate::platform::fsx::remove_dir_all_retry(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn peek_exit_does_not_reap() {
        let mut c = Command::new("/bin/sleep").arg("0.2").spawn().unwrap();
        assert!(peek_exit(&c).is_none());
        std::thread::sleep(Duration::from_millis(600));
        assert!(peek_exit(&c).is_some());
        // not reaped yet, so the pid must still exist (as a zombie)
        assert_eq!(unsafe { libc::kill(c.id() as i32, 0) }, 0);
        assert!(peek_exit(&c).is_some());
        assert!(c.wait().unwrap().success());
    }

    #[cfg(unix)]
    #[test]
    fn short_link_multibyte_name() {
        let tmp = std::env::temp_dir().join(format!("gd-link-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        let slug = profiles::slug("아주긴한글작업공간이름입니다정말로아주길어요");
        let link = short_link(&tmp, &slug).unwrap();
        let name = link.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.len() <= 24 + 7, "{name} ({}바이트)", name.len());
        assert!(link.join("user-data/1.99-main.sock").as_os_str().len() < SUN_PATH_MAX);
        fs::remove_file(&link).ok();
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn main_normalize() {
        assert_eq!(normalize_main("./out/main.js"), "out/main.js");
        assert_eq!(normalize_main("dist/main"), "dist/main.js");
        assert_eq!(normalize_main("lib/"), "lib/index.js");
        assert_eq!(regex_escape("out/main.js"), "out/main\\.js");
    }

    // ---- Windows port: cross-platform pure functions ----

    #[test]
    fn sentinel_search_matches_naive() {
        let naive = |h: &[u8]| h.windows(FUSE_SENTINEL.len()).position(|w| w == FUSE_SENTINEL);
        // pseudo-random bytes + sentinels at several positions (including traps matching only a prefix)
        let mut x: u32 = 12345;
        let mut buf: Vec<u8> = (0..200_000)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                (x >> 16) as u8
            })
            .collect();
        assert_eq!(find_sentinel(&buf), naive(&buf));
        buf[1000..1031].copy_from_slice(&FUSE_SENTINEL[..31]); // only the last byte differs
        buf[150_000..150_032].copy_from_slice(FUSE_SENTINEL);
        buf[190_000..190_032].copy_from_slice(FUSE_SENTINEL);
        assert_eq!(find_sentinel(&buf), Some(150_000));
        assert_eq!(find_sentinel(&buf), naive(&buf));
        assert_eq!(find_sentinel(FUSE_SENTINEL), Some(0));
        assert_eq!(find_sentinel(&FUSE_SENTINEL[1..]), None);
        assert_eq!(find_sentinel(b""), None);
        let mut tail = vec![0u8; 77];
        tail.extend_from_slice(FUSE_SENTINEL);
        assert_eq!(find_sentinel(&tail), Some(77));
    }

    /// A minimal PE file (only the section table is valid) with two sections, .text and .rdata.
    fn fake_pe(rdata: &[u8], text: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8; 0x400];
        f[0..2].copy_from_slice(b"MZ");
        f[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        f[0x80..0x84].copy_from_slice(b"PE\0\0");
        f[0x86..0x88].copy_from_slice(&2u16.to_le_bytes()); // NumberOfSections
        f[0x94..0x96].copy_from_slice(&0xF0u16.to_le_bytes()); // SizeOfOptionalHeader
        let sec = 0x80 + 24 + 0xF0;
        let text_off = 0x400u32;
        let rdata_off = text_off + text.len() as u32;
        for (i, (name, off, size)) in [(&b".text\0\0\0"[..], text_off, text.len() as u32), (&b".rdata\0\0"[..], rdata_off, rdata.len() as u32)]
            .into_iter()
            .enumerate()
        {
            let s = sec + i * 40;
            f[s..s + 8].copy_from_slice(name);
            f[s + 16..s + 20].copy_from_slice(&size.to_le_bytes());
            f[s + 20..s + 24].copy_from_slice(&off.to_le_bytes());
        }
        f.extend_from_slice(text);
        f.extend_from_slice(rdata);
        f
    }

    fn fuse_blob(wire: &[u8]) -> Vec<u8> {
        let mut b = b"....".to_vec();
        b.extend_from_slice(FUSE_SENTINEL);
        b.extend_from_slice(&[1, wire.len() as u8]);
        b.extend_from_slice(wire);
        b.extend_from_slice(b"....");
        b
    }

    #[test]
    fn pe_rdata_fuses() {
        let tmp = std::env::temp_dir().join(format!("gd-pe-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        // fuses in .rdata win over a (fake) sentinel in .text
        let p = tmp.join("a.exe");
        fs::write(&p, fake_pe(&fuse_blob(b"001100011"), &fuse_blob(b"111111111"))).unwrap();
        let mut f = fs::File::open(&p).unwrap();
        let (off, size) = pe_section(&mut f, b".rdata").unwrap();
        assert!(off > 0x400 && size > 32);
        assert!(pe_section(&mut f, b".nope").is_none());
        let fz = read_fuses_file(&p).unwrap();
        assert_eq!(fz.wire, "001100011");
        assert!(fz.inspect_allowed());
        // if .rdata has none, search the whole file
        let q = tmp.join("b.exe");
        fs::write(&q, fake_pe(b"nothing here", &fuse_blob(b"000000000"))).unwrap();
        assert_eq!(read_fuses_file(&q).unwrap().wire, "000000000");
        // a file that's neither PE nor Mach-O
        let r = tmp.join("c.bin");
        fs::write(&r, fuse_blob(b"0101")).unwrap();
        assert_eq!(read_fuses_file(&r).unwrap().wire, "0101");
        assert!(pe_section(&mut fs::File::open(&r).unwrap(), b".rdata").is_none());
        fs::write(&r, b"MZ but too short").unwrap();
        assert!(read_fuses_file(&r).is_err());
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn breakpoint_url_regex() {
        // macOS keeps the same regex as before
        assert_eq!(main_url_regex("out/main.js", false), "Resources/app(\\.asar)?/out/main\\.js$");
        let w = main_url_regex("out/main.js", true);
        assert_eq!(
            w,
            "[Rr][Ee][Ss][Oo][Uu][Rr][Cc][Ee][Ss][\\\\/][Aa][Pp][Pp](\\.[Aa][Ss][Aa][Rr])?[\\\\/][Oo][Uu][Tt][\\\\/][Mm][Aa][Ii][Nn]\\.[Jj][Ss]$"
        );
        // characters that get percent-encoded match both as is and encoded
        let s = main_url_regex("dist/main process.js", true);
        assert!(s.contains("(?: |%20)"), "{s}");
        let k = main_url_regex("앱/main.js", true);
        assert!(k.contains("(?:앱|%EC%95%B1)"), "{k}");
        assert!(url_ends_with_ci("file:///C:/Program%20Files/X/resources/app/Out/Main.js", "out/main.js"));
        assert!(url_ends_with_ci(r"C:\X\resources\app\out\main.js", "out/main.js"));
        assert!(!url_ends_with_ci("file:///C:/X/resources/app/out/main.jsx", "out/main.js"));
    }

    #[test]
    fn menu_keys_refused() {
        for c in [
            "alt", "Alt", "alt_l", "option", "shift+alt", "ctrl+alt", "F10", "f10", "shift+F10", "alt+F10", "alt+f", "alt+e", "Alt+x",
            "option+f", "alt+space", "shift+alt+space", "super+alt+space", "menu", "shift+menu", "Apps",
        ] {
            assert!(menu_key_refusal(c).is_some(), "{c} 는 거부되어야 함");
        }
        for c in [
            "ctrl+n", "ctrl+shift+p", "ctrl+F10", "ctrl+alt+f", "alt+1", "alt+Left", "alt+F4", "F5", "Return", "a", "shift+Tab", "ctrl+alt+Delete",
            "F1", "ctrl+,", "unknownkey+q",
            // Shift/Win+Alt+letter doesn't open mnemonics (VS Code: Shift+Alt+F format document, Shift+Alt+A
            // block comment, …)
            "shift+alt+f", "alt+shift+f", "Shift+Alt+A", "shift+alt+i", "super+alt+x", "alt+super+x",
            // parse_combo adds Shift for uppercase, so Shift+Alt+F is actually sent
            "alt+F", "Alt+E",
        ] {
            assert!(menu_key_refusal(c).is_none(), "{c} 는 허용되어야 함");
        }
        // cmd is judged after localization (ctrl), so cmd+alt+f is allowed
        let l = crate::platform::input::localize_combo_for("cmd+alt+f", true);
        assert!(menu_key_refusal(&l).is_none());
        let why = menu_key_refusal("alt+f").unwrap();
        assert!(why.contains("app_menu") && why.contains("Shift+Alt+글자"), "{why}");
    }

    /// PATCH_JS's before-input-event filter (menuKey) must follow the same rules.
    #[test]
    fn patch_menu_key_filter_matches() {
        let i = PATCH_JS.find("const menuKey").unwrap();
        let body = &PATCH_JS[i..i + PATCH_JS[i..].find("};").unwrap()];
        assert!(body.contains("(!i.shift && !i.meta && /^Key[A-Z]$/.test(c)) || c === 'Space'"), "{body}");
        assert!(body.contains("if (!i || i.control) return false;"));
    }

    #[test]
    fn offscreen_math() {
        // 100%: real pixels = DIP
        assert_eq!(offscreen_dip_math((5920, 5080), 1.0, 1.0), (5920, 5080));
        // 150% primary monitor: pixels / 1.5
        assert_eq!(offscreen_dip_math((7840, 6160), 1.5, 1.5), (5227, 4107));
        // limit: |DIP| × max scale ≤ 24000 (well away from 16-bit coordinates and the -32000 minimized
        // marker)
        for (px, p, m) in [((28000, 28000), 1.0, 1.0), ((28000, 28000), 1.0, 2.5), ((28000, 28000), 1.25, 3.0), ((20000, 9000), 2.0, 2.0)] {
            let (x, y) = offscreen_dip_math(px, p, m);
            let mx = m.max(p);
            assert!((x.abs() as f64) * mx <= 24000.0 && (y.abs() as f64) * mx <= 24000.0, "{px:?} {p} {m} → {x},{y}");
            assert!(x > 0 && y > 0);
        }
        // odd scale values
        assert_eq!(offscreen_dip_math((3000, 2000), f64::NAN, f64::NAN), (3000, 2000));
        assert_eq!(offscreen_dip_math((3000, 2000), 0.0, 0.0), (3000, 2000));
    }

    #[test]
    fn aumid_is_valid_and_unique() {
        let a = hidden_aumid("selftest-electron-1234");
        let b = hidden_aumid("selftest-electron-1235");
        let k = hidden_aumid("한글_작업공간");
        let long = hidden_aumid(&"x".repeat(500));
        for s in [&a, &b, &k, &long] {
            assert!(s.starts_with("GhostDesk.Hidden."), "{s}");
            assert!(s.len() <= 128 && s.is_ascii() && !s.contains(' '), "{s}");
        }
        assert_ne!(a, b);
        assert_ne!(k, hidden_aumid("다른_작업공간"));
        assert_eq!(a, hidden_aumid("selftest-electron-1234"));
    }

    #[test]
    fn violation_descriptions() {
        let w = |on_screen, taskbar| WinState {
            pid: 42,
            hwnd: 0x1234,
            class: "Chrome_WidgetWin_1".into(),
            rect: (0, 0, 10, 10),
            alpha: Some(0),
            on_screen,
            taskbar,
        };
        assert!(describe_violations(&[w(false, false)], None).is_none());
        let d = describe_violations(&[w(true, false)], None).unwrap();
        assert!(d.contains("보이는 창") && !d.contains("front"));
        assert!(describe_violations(&[w(false, true)], None).unwrap().contains("작업 표시줄"));
        // foreground violations must include "front" so the watchdog calls rehide(true)
        assert!(describe_violations(&[], Some(42)).unwrap().contains("front"));
    }

    #[test]
    fn windows_discovery_helpers() {
        assert_eq!(squirrel_version("app-1.0.9266"), Some(vec![1, 0, 9266]));
        assert_eq!(squirrel_version("app-2.10.0-beta1"), Some(vec![2, 10, 0]));
        assert!(squirrel_version("app-1.10.0") > squirrel_version("app-1.9.9"));
        assert_eq!(squirrel_version("app"), None);
        assert_eq!(squirrel_version("app-x"), None);
        assert_eq!(squirrel_version("packages"), None);
        for n in ["current", "1.2.3", "v1.2", "abcdef1234", "0123456"] {
            assert!(looks_versioned(n), "{n}");
        }
        for n in ["resources", "locales", "bin", "abc", "1.2.3-beta", "tools"] {
            assert!(!looks_versioned(n), "{n}");
        }
        for n in ["update.exe", "unins000.exe", "uninstall code.exe", "crashpad_handler.exe", "notification_helper.exe"] {
            assert!(is_helper_exe(n), "{n}");
        }
        for n in ["code.exe", "cursor.exe", "discord.exe", "code - insiders.exe"] {
            assert!(!is_helper_exe(n), "{n}");
        }
        for n in ["Code", "Visual Studio Code", "discord", "Cursor", "my-app_2"] {
            assert!(dir_name_ok(n), "{n}");
        }
        for n in ["", " Code", "a/b", "a\\b", "com.x:y", "CON", "nul", "x.", "..", "a|b"] {
            assert!(!dir_name_ok(n), "{n}");
        }
        let product = json!({ "darwinBundleIdentifier": "com.microsoft.VSCode", "win32AppUserModelId": "Microsoft.VisualStudioCode" });
        assert_eq!(win_bundle_id(Some(&product), None, None, "Code"), "com.microsoft.VSCode");
        let product = json!({ "win32AppUserModelId": "Anysphere.Cursor" });
        assert_eq!(win_bundle_id(Some(&product), None, None, "Cursor"), "Anysphere.Cursor");
        let pkg = json!({ "build": { "appId": "com.example.app" } });
        assert_eq!(win_bundle_id(None, Some(&pkg), Some("Example"), "Example"), "com.example.app");
        assert_eq!(win_bundle_id(None, Some(&json!({})), Some("Discord"), "Discord"), "com.squirrel.Discord.Discord");
        assert_eq!(win_bundle_id(None, None, None, "Some App"), "win.some app");
    }

    // ---- Windows only ----

    /// A minimal app.asar (header pickle + file contents).
    #[cfg(windows)]
    fn write_asar(path: &Path, files: &[(&str, &[u8])]) {
        let mut entries = Map::new();
        let mut data = Vec::new();
        for (name, body) in files {
            entries.insert(name.to_string(), json!({ "size": body.len(), "offset": data.len().to_string() }));
            data.extend_from_slice(body);
        }
        let js = serde_json::to_vec(&json!({ "files": entries })).unwrap();
        let padded = js.len().div_ceil(4) * 4;
        let mut out = Vec::new();
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&((8 + padded) as u32).to_le_bytes());
        out.extend_from_slice(&((4 + padded) as u32).to_le_bytes());
        out.extend_from_slice(&(js.len() as u32).to_le_bytes());
        out.extend_from_slice(&js);
        out.resize(16 + padded, 0);
        out.extend_from_slice(&data);
        fs::write(path, out).unwrap();
    }

    /// Fake Electron app folder: executable (with fuses, made large) + helpers + resources + runtime
    /// files.
    #[cfg(windows)]
    fn fake_app_dir(dir: &Path, exe: &str, wire: &[u8], pkg: Value, product: Option<Value>, asar: bool) {
        fs::create_dir_all(dir.join("resources")).unwrap();
        let mut body = vec![0u8; 64 * 1024];
        body.extend_from_slice(&fuse_blob(wire));
        fs::write(dir.join(exe), fake_pe(&body, b"text")).unwrap();
        fs::write(dir.join("unins000.exe"), vec![1u8; 300 * 1024]).unwrap(); // helpers aren't picked even when larger
        fs::write(dir.join("icudtl.dat"), b"icu").unwrap();
        let pkg_b = serde_json::to_vec(&pkg).unwrap();
        let product_b = product.map(|p| serde_json::to_vec(&p).unwrap());
        if asar {
            let mut files: Vec<(&str, &[u8])> = vec![("package.json", &pkg_b)];
            if let Some(p) = &product_b {
                files.push(("product.json", p));
            }
            write_asar(&dir.join("resources/app.asar"), &files);
        } else {
            fs::create_dir_all(dir.join("resources/app")).unwrap();
            fs::write(dir.join("resources/app/package.json"), &pkg_b).unwrap();
            if let Some(p) = &product_b {
                // JSON with a BOM must be read too
                let mut b = b"\xEF\xBB\xBF".to_vec();
                b.extend_from_slice(p);
                fs::write(dir.join("resources/app/product.json"), b).unwrap();
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_fake_layouts() {
        use win_apps::{inspect, inspect_dirs, is_app_dir, main_exe, resolve_layout};
        let tmp = std::env::temp_dir().join(format!("gd-winapps-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);

        // 1) VS Code family (folder-style resources\app, product.json with a BOM)
        let code = tmp.join("Fake Code");
        fake_app_dir(
            &code,
            "Code.exe",
            b"101100011",
            json!({ "name": "code-oss-dev", "version": "1.99.0", "main": "./out/main.js" }),
            Some(json!({ "nameShort": "Code", "nameLong": "Fake Visual Studio Code", "dataFolderName": ".vscode",
                         "darwinBundleIdentifier": "com.microsoft.VSCode", "win32AppUserModelId": "Microsoft.VisualStudioCode" })),
            false,
        );
        assert!(is_app_dir(&code));
        assert_eq!(main_exe(&code).unwrap().file_name().unwrap(), "Code.exe");
        let a = inspect(&code).expect("VS Code 계열 감지");
        assert_eq!(a.bundle_id, "com.microsoft.VSCode");
        assert_eq!(a.name, "Fake Visual Studio Code");
        assert_eq!(a.family, AppFamily::VsCode);
        assert_eq!(a.main_script.as_deref(), Some("out/main.js"));
        assert_eq!(a.app_version.as_deref(), Some("1.99.0"));
        assert!(a.hideable, "{:?}", a.reason);
        assert_eq!(a.fuses.as_ref().unwrap().wire, "101100011");
        assert_eq!(a.bundle, code);
        assert!(!a.running);
        // the .exe path yields the same app
        let b = inspect(&code.join("Code.exe")).unwrap();
        assert_eq!((b.bundle_id.as_str(), b.exe.clone()), ("com.microsoft.VSCode", code.join("Code.exe")));
        assert_eq!(read_fuses(&code).unwrap().wire, "101100011");

        // 2) Squirrel (root\Update.exe + app-<version>\, asar): highest version, root is the bundle
        let sq = tmp.join("SqApp");
        fs::create_dir_all(&sq).unwrap();
        fs::write(sq.join("Update.exe"), b"u").unwrap();
        fs::write(sq.join("SqApp.exe"), b"stub").unwrap();
        // only the old version (1.9.0) has the inspector fuse on → picking the latest (1.10.0) means it
        // can't be hidden
        fake_app_dir(&sq.join("app-1.9.0"), "SqApp.exe", b"0001", json!({ "name": "sq", "productName": "Sq App", "main": "index.js" }), None, true);
        fake_app_dir(&sq.join("app-1.10.0"), "SqApp.exe", b"0000", json!({ "name": "sq", "productName": "Sq App", "main": "index.js" }), None, true);
        let (root, appdir) = resolve_layout(&sq).unwrap();
        assert_eq!((root.as_path(), appdir.as_path()), (sq.as_path(), sq.join("app-1.10.0").as_path()));
        // pointing directly at an app-<version> folder still yields the Squirrel root
        assert_eq!(resolve_layout(&sq.join("app-1.9.0")).unwrap().0, sq);
        let s = inspect(&sq).unwrap();
        assert_eq!(s.bundle_id, "com.squirrel.SqApp.SqApp");
        assert_eq!(s.name, "Sq App");
        assert_eq!(s.exe, sq.join("app-1.10.0").join("SqApp.exe"));
        assert_eq!(s.main_script.as_deref(), Some("index.js"));
        assert_eq!(s.family, AppFamily::Generic);
        assert!(!s.hideable && s.reason.as_deref().unwrap_or("").contains("EnableNodeCliInspectArguments"));
        // the root's launcher (stub) maps to the latest version's real executable
        assert_eq!(inspect(&sq.join("SqApp.exe")).unwrap().exe, sq.join("app-1.10.0").join("SqApp.exe"));
        // a helper maps to the main executable too
        assert_eq!(inspect(&sq.join("Update.exe")).unwrap().exe, sq.join("app-1.10.0").join("SqApp.exe"));

        // 3) a commit-named folder one level down
        let nested = tmp.join("Nested");
        fake_app_dir(&nested.join("abcdef1234"), "Nested.exe", b"0001", json!({ "name": "OpenCode", "main": "main.js" }), None, true);
        fs::create_dir_all(nested.join("logs")).unwrap();
        let (r, d) = resolve_layout(&nested).unwrap();
        assert_eq!((r, d), (nested.clone(), nested.join("abcdef1234")));
        let n = inspect(&nested).unwrap();
        assert_eq!(n.family, AppFamily::OpenCode);
        assert_eq!(n.bundle_id, "win.nested");

        // 4) a non-Electron folder / deny list (executable name)
        let not = tmp.join("NotElectron");
        fs::create_dir_all(not.join("resources/foo")).unwrap();
        fs::write(not.join("x.exe"), b"x").unwrap();
        assert!(inspect(&not).is_none() && resolve_layout(&not).is_none());
        let dc = tmp.join("Discordish");
        fake_app_dir(&dc, "Discord.exe", b"0001", json!({ "name": "discord" }), None, true);
        let d = inspect_dirs(&dc, &dc, None).unwrap();
        assert!(!d.hideable && d.reason.unwrap().contains("Discord"));
        assert!(blocked_reason("anything", Path::new(r"C:\x\Discord.exe")).is_some());
        assert!(blocked_reason("anything", Path::new(r"C:\x\Code.exe")).is_none());
        assert!(blocked_reason("com.anthropic.claudefordesktop", Path::new(r"C:\x\x.exe")).is_some());

        fs::remove_dir_all(&tmp).ok();
    }

    /// Electron apps installed on this PC (read only). If Discord is present, its fuses must be read
    /// and it must be judged not hideable.
    #[cfg(windows)]
    #[test]
    fn windows_installed_discord() {
        let Some(local) = dirs::data_local_dir() else { return };
        if !local.join("Discord").join("Update.exe").is_file() {
            return;
        }
        let t = Instant::now();
        let apps = detect_apps();
        let first = t.elapsed();
        let d = apps.iter().find(|a| a.exe.file_name().map(|n| n.eq_ignore_ascii_case("Discord.exe")).unwrap_or(false));
        let d = d.expect("Discord 감지");
        assert!(d.bundle.ends_with("Discord"), "{:?}", d.bundle);
        assert!(d.exe.parent().unwrap().file_name().unwrap().to_string_lossy().starts_with("app-"));
        let f = d.fuses.as_ref().expect("Discord 퓨즈");
        assert!(!f.inspect_allowed());
        assert!(!d.hideable);
        // the second detection must be fast thanks to the fuse cache
        let t = Instant::now();
        let again = detect_apps();
        assert_eq!(again.len(), apps.len());
        eprintln!("detect_apps: 첫 {:?}, 두 번째 {:?}, {}개", first, t.elapsed(), apps.len());
    }

    #[cfg(windows)]
    #[test]
    fn windows_peek_exit_and_kill_paths() {
        use crate::platform::proc::{group_pids, pid_alive, spawn_group};
        // exit code shown (checked without reaping)
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/c", "exit 3"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let c = Arc::new(Mutex::new(Some(spawn_group(&mut cmd).unwrap())));
        let start = Instant::now();
        while child_alive(&c) && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(c.lock().as_ref().and_then(peek_exit).as_deref(), Some("exit status: 3"));
        terminate_and_reap(0, 0, None, &c);
        assert!(c.lock().is_none());
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/c", "exit -1073741819"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let mut ch = cmd.spawn().unwrap();
        ch.wait().unwrap();
        assert_eq!(peek_exit(&ch).as_deref(), Some("exit status: 0xc0000005"));

        // watchdog kill (kill_group_if_held): the Job's grandchildren end too and Child remains → reaped
        // later
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/c", "ping -n 30 127.0.0.1 >NUL"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let child = spawn_group(&mut cmd).unwrap();
        let pid = child.id() as i32;
        let c = Arc::new(Mutex::new(Some(child)));
        let start = Instant::now();
        let mut pids = vec![];
        while start.elapsed() < Duration::from_secs(10) {
            pids = group_pids(pid as u32).unwrap_or_default();
            if pids.len() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(pids.len() >= 2, "{pids:?}");
        let mut ours = win_screen::our_pids(pid);
        ours.sort();
        pids.sort();
        assert_eq!(ours, pids);
        // console programs start with CREATE_NO_WINDOW and have no window → no violation
        assert!(scan_violation(pid, pid).is_none());
        assert!(child_alive(&c));
        kill_group_if_held(pid, pid, &c);
        assert!(c.lock().is_some());
        assert!(!child_alive(&c));
        std::thread::sleep(Duration::from_millis(200));
        for p in &pids {
            assert!(!pid_alive(*p), "pid {p} 가 남아 있음");
        }
        terminate_and_reap(pid, pid, None, &c);
        assert!(c.lock().is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_workspace_without_link() {
        let tmp = std::env::temp_dir().join(format!("gd-ws-{}", std::process::id()));
        let fake = ElectronApp {
            name: "Fake".into(),
            bundle_id: "test.ghostdesk.win".into(),
            bundle: tmp.join("app"),
            exe: tmp.join("app/Fake.exe"),
            app_version: None,
            electron_version: None,
            fuses: None,
            hideable: false,
            reason: None,
            family: AppFamily::Generic,
            user_data_guess: None,
            main_script: None,
            running: false,
            launch_profile: String::new(),
        };
        // reserved device name + a very long Korean name (Windows has no sun_path limit)
        for name in ["CON", &"아주긴한글작업공간이름".repeat(6)] {
            let mut o = ElectronLaunch::new(fake.clone(), name, 1024, 768);
            o.workspaces_root = Some(tmp.join("ws"));
            let p = prepare_workspace(&o).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert_eq!(p.root, p.real_root);
            assert!(p.user_data.is_dir());
            let (args, env) = launch_plan(&o, &p);
            assert!(args.iter().any(|a| a == "--disable-features=CalculateNativeWinOcclusion"));
            assert!(env.is_empty());
        }
        let con = profiles::slug("CON");
        assert!(!crate::platform::paths::is_reserved_name(&con), "{con}");
        assert!(tmp.join("ws").join(&con).is_dir());
        fs::remove_dir_all(&tmp).ok();
    }

    /// Off-screen coordinates computed from the real display info (query only).
    #[cfg(windows)]
    #[test]
    fn windows_offscreen_dip_outside_monitors() {
        let mons = win_screen::monitors();
        if mons.is_empty() {
            return; // a session without an interactive desktop (services, …)
        }
        let (x, y) = win_screen::offscreen_dip();
        assert!(x > 0 && y > 0 && x < 24000 && y < 24000, "{x},{y}");
        // if all monitors share a scale, DIP × scale = real pixels outside every monitor
        let s = mons[0].1;
        if mons.iter().all(|m| (m.1 - s).abs() < 1e-6) {
            let px = ((x as f64 * s) as i32, (y as f64 * s) as i32);
            assert!(!crate::platform::win::intersects_any_monitor((px.0, px.1, px.0 + 1280, px.1 + 800)), "{px:?}");
        }
    }

    /// Checks V8 regex semantics with headless Chrome (empty temp profile): the same engine as the
    /// real CDP urlRegex.
    ///
    /// Run with `cargo test -- --ignored breakpoint_regex_in_v8`.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn breakpoint_regex_in_v8() {
        let Some(chrome) = crate::platform::win::app_path("chrome.exe") else { return };
        let cases = [
            ("file:///C:/Users/a/AppData/Local/Programs/Microsoft%20VS%20Code/resources/app/out/main.js", true),
            ("file:///C:/Program%20Files/Microsoft%20VS%20Code/1a2b3c4d5e/resources/app/out/main.js", true),
            (r"C:\Users\a\AppData\Local\Programs\cursor\resources\app\out\main.js", true),
            ("file:///C:/X/Resources/App.asar/Out/Main.js", true),
            ("file:///C:/X/resources/app.asar/out/main.js", true),
            ("file:///C:/X/resources/app/out/main.js.map", false),
            ("file:///C:/X/resources/app/node_modules/out/main.js", false),
            ("file:///C:/X/resources/app/out/main2.js", false),
        ];
        let re = main_url_regex("out/main.js", true);
        let re_sp = main_url_regex("dist/main process.js", true);
        let js = format!(
            "<script>const r=new RegExp({}), s=new RegExp({}); document.body.textContent = 'RES:' + JSON.stringify([{}].map(u => r.test(u))) + ':' + s.test('file:///C:/a/resources/app/dist/main%20process.js') + s.test('C:\\\\a\\\\resources\\\\app\\\\dist\\\\main process.js');</script>",
            serde_json::to_string(&re).unwrap(),
            serde_json::to_string(&re_sp).unwrap(),
            cases.iter().map(|(u, _)| serde_json::to_string(u).unwrap()).collect::<Vec<_>>().join(",")
        );
        let html = tmp_html(&js);
        let prof = std::env::temp_dir().join(format!("gd-v8-{}", std::process::id()));
        let out = Command::new(chrome)
            .args(["--headless=new", "--disable-gpu", "--no-first-run", "--no-default-browser-check", "--dump-dom"])
            .arg(format!("--user-data-dir={}", prof.display()))
            .arg(&html)
            .output()
            .unwrap();
        let dom = String::from_utf8_lossy(&out.stdout).to_string();
        let _ = fs::remove_dir_all(&prof);
        let _ = fs::remove_file(&html);
        let want: Vec<bool> = cases.iter().map(|(_, w)| *w).collect();
        let got = dom.split("RES:").nth(1).unwrap_or_else(|| panic!("결과 없음: {dom}"));
        assert!(got.starts_with(&format!("{}:truetrue", serde_json::to_string(&want).unwrap())), "{got}");
    }

    /// Checks the Windows path with a real Electron executable, **without running a single line of app
    /// code**.
    ///
    /// Launches it with an empty temp profile inside a Job, finds the Node inspector address in stderr
    /// (GUI executable), and while paused on the first line of the main script checks the breakpoint
    /// regex match and the PATCH_JS evaluation result (Windows branches included), then ends the whole
    /// Job without resuming. The product's pre-watcher (40 ms) runs alongside and kills it immediately
    /// if a window becomes visible or takes the foreground. Target: GHOSTDESK_TEST_ELECTRON_EXE=<exe>
    /// or the first detected app with the inspector fuse on.
    ///
    /// `cargo test -- --ignored windows_live_patch_paused --nocapture`
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn windows_live_patch_paused() {
        let app = match std::env::var_os("GHOSTDESK_TEST_ELECTRON_EXE") {
            Some(p) => win_apps::inspect(Path::new(&p)).expect("Electron 앱이 아님"),
            None => match detect_apps().into_iter().find(|a| a.fuses.as_ref().map(|f| f.inspect_allowed()).unwrap_or(false)) {
                Some(a) => a,
                None => return,
            },
        };
        let main = app.main_script.clone().expect("package.json main");
        eprintln!("대상: {} {} (electron {:?}, main {main})", app.name, app.exe.display(), app.electron_version);
        let tmp = std::env::temp_dir().join(format!("gd-live-{}", std::process::id()));
        let mut o = ElectronLaunch::new(app.clone(), "live-paused", 1024, 700);
        o.workspaces_root = Some(tmp.clone());
        let p = prepare_workspace(&o).unwrap(); // empty profile (no clone)
        let (args, env) = launch_plan(&o, &p);
        let log = fs::File::create(tmp.join("app.log")).unwrap();
        let mut cmd = Command::new(&app.exe);
        cmd.args(&args);
        for (k, _) in std::env::vars_os() {
            let ks = k.to_string_lossy().to_ascii_uppercase();
            if ks.starts_with("VSCODE_") || ks.starts_with("ELECTRON_") || ks.starts_with("NODE_") || ks == "CHROME_DESKTOP" {
                cmd.env_remove(&k);
            }
        }
        cmd.envs(env).current_dir(&p.real_root).stdin(Stdio::null()).stdout(Stdio::from(log)).stderr(Stdio::piped());
        let mut child = crate::platform::proc::spawn_group(&mut cmd).expect("실행");
        let pid = child.id() as i32;
        let stderr = child.stderr.take().unwrap();
        let c = Arc::new(Mutex::new(Some(child)));
        assert!(crate::platform::proc::group_pids(pid as u32).is_some(), "Job 없음");
        let tripped: Arc<Mutex<Option<String>>> = Default::default();
        let stop = StopFlag(Arc::new(AtomicBool::new(false)));
        spawn_prewatch(pid, pid, c.clone(), stop.0.clone(), tripped.clone(), WatchSide::new(Default::default()));
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr).split(b'\n').map_while(|l| l.ok()) {
                let l = String::from_utf8_lossy(&line).into_owned();
                eprintln!("  [app stderr] {}", l.trim_end());
                if l.contains("Debugger listening on") {
                    if let Some(ws) = l.split_whitespace().find(|w| w.starts_with("ws://")) {
                        let _ = tx.send(ws.to_string());
                    }
                }
            }
        });
        let (off_x, off_y) = win_screen::offscreen_dip();
        let cfg = json!({ "width": 1024, "height": 700, "mode": "ghost", "offX": off_x, "offY": off_y, "parentPid": std::process::id(),
                          "userData": p.user_data.display().to_string(), "aumid": hidden_aumid("live-paused") });
        let patch = PATCH_JS.replace("__CFG__", &cfg.to_string());
        let res: Result<Value> = async {
            let node = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(20))).await?
                .map_err(|_| anyhow!("stderr 에서 Node 인스펙터 주소를 20초 안에 찾지 못함"))?;
            let insp = Cdp::connect(&node).await?;
            let mut ev = insp.subscribe();
            insp.call("Runtime.enable", json!({}), None).await?;
            insp.call("Debugger.enable", json!({}), None).await?;
            let re = main_url_regex(&main, true);
            let bp = insp.call("Debugger.setBreakpointByUrl", json!({ "urlRegex": re, "lineNumber": 0 }), None).await?["breakpointId"]
                .as_str()
                .map(String::from);
            insp.call("Runtime.runIfWaitingForDebugger", json!({}), None).await?;
            let mut scripts: HashMap<String, String> = HashMap::new();
            let mut resolved = false;
            let deadline = Instant::now() + Duration::from_secs(25);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let e = tokio::time::timeout(left, ev.recv()).await.map_err(|_| anyhow!("25초 안에 멈추지 않음"))??;
                match e.method.as_str() {
                    "Debugger.scriptParsed" => {
                        scripts.insert(
                            e.params["scriptId"].as_str().unwrap_or_default().into(),
                            e.params["url"].as_str().unwrap_or_default().into(),
                        );
                    }
                    "Debugger.breakpointResolved" => resolved |= e.params["breakpointId"].as_str() == bp.as_deref(),
                    "Debugger.paused" => {
                        let f = &e.params["callFrames"][0];
                        let url = scripts.get(f["location"]["scriptId"].as_str().unwrap_or_default()).cloned().unwrap_or_default();
                        let reason = e.params["reason"].as_str().unwrap_or_default().to_string();
                        // if it paused somewhere other than the main script, fail without resuming (so no app code runs)
                        ensure!(url_ends_with_ci(&url, &main), "메인 스크립트가 아닌 곳에서 멈춤: {url} ({reason})");
                        let hit = e.params["hitBreakpoints"].as_array().map(|a| a.iter().any(|b| b.as_str() == bp.as_deref())).unwrap_or(false);
                        let r = insp
                            .call("Debugger.evaluateOnCallFrame", json!({ "callFrameId": f["callFrameId"], "expression": patch, "returnByValue": true }), None)
                            .await?;
                        let probe = insp
                            .call(
                                "Debugger.evaluateOnCallFrame",
                                json!({ "callFrameId": f["callFrameId"], "returnByValue": true, "expression":
                                    "(() => { const G = globalThis.__ghostdesk, B = G.electron.BrowserWindow.prototype, a = G.electron.app; \
                                      const blocked = (f) => String(f).includes('G.blocked'); \
                                      return { log: G.log, platform: process.platform, electron: process.versions.electron, \
                                        winBlocked: ['setSkipTaskbar', 'setMenuBarVisibility', 'setAutoHideMenuBar', 'setProgressBar', 'setOverlayIcon', 'setAppDetails', 'show', 'focus'].filter(m => !blocked(B[m])), \
                                        jump: blocked(a.setJumpList), origs: Object.keys(G.orig).filter(k => typeof G.orig[k] !== 'function'), \
                                        aumidHooked: String(a.setAppUserModelId).includes('CFG.aumid'), cfg: G.cfg }; })()" }),
                                None,
                            )
                            .await?;
                        return Ok(json!({ "url": url, "reason": reason, "hitBp": hit, "resolved": resolved, "patch": r["result"]["value"], "probe": probe["result"]["value"] }));
                    }
                    _ => {}
                }
            }
        }
        .await;
        // end the whole Job without resuming
        drop(stop);
        let pids = crate::platform::proc::group_pids(pid as u32).unwrap_or_default();
        terminate_and_reap(pid, pid, None, &c);
        std::thread::sleep(Duration::from_millis(300));
        let alive: Vec<u32> = pids.iter().copied().filter(|p| crate::platform::proc::pid_alive(*p)).collect();
        let _ = crate::platform::fsx::remove_dir_all_retry(&tmp);
        let tr = tripped.lock().clone();
        eprintln!("결과: {res:#?}\n사전 감시자: {tr:?}\nJob 프로세스 {pids:?} → 남은 것 {alive:?}");
        assert!(tr.is_none(), "사전 감시자가 위반을 잡음: {tr:?}");
        assert!(alive.is_empty(), "남은 프로세스 {alive:?}");
        let v = res.expect("실패");
        assert_eq!(v["patch"], json!("ok"), "패치 일부 실패: {v}");
        assert_eq!(v["probe"]["platform"], json!("win32"));
        assert_eq!(v["probe"]["winBlocked"], json!([]), "막히지 않은 창 메서드: {v}");
        assert_eq!(v["probe"]["origs"], json!([]), "원래 함수를 못 잡음: {v}");
        assert_eq!(v["probe"]["jump"], json!(true));
        assert_eq!(v["probe"]["aumidHooked"], json!(true));
        assert!(v["resolved"] == json!(true) || v["hitBp"] == json!(true), "중단점 정규식이 메인 스크립트와 일치하지 않음: {v}");
    }

    #[cfg(windows)]
    fn tmp_html(body: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("gd-v8-{}.html", std::process::id()));
        fs::write(&p, format!("<!doctype html><html><body>{body}</body></html>")).unwrap();
        p
    }

    /// Launches headless Chrome (empty temp profile) in a Job to check the watchdog's verdicts against
    /// the real process and window lists.
    ///
    /// Headless means no visible window, so there must be no violation.
    /// `cargo test -- --ignored windows_watchdog_headless`
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn windows_watchdog_headless_chrome() {
        let Some(chrome) = crate::platform::win::app_path("chrome.exe") else { return };
        let prof = std::env::temp_dir().join(format!("gd-wd-{}", std::process::id()));
        let mut cmd = Command::new(chrome);
        cmd.args(["--headless=new", "--disable-gpu", "--no-first-run", "--no-default-browser-check", "--remote-debugging-port=0"])
            .arg(format!("--user-data-dir={}", prof.display()))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = crate::platform::proc::spawn_group(&mut cmd).unwrap();
        let pid = child.id() as i32;
        let c = Arc::new(Mutex::new(Some(child)));
        let start = Instant::now();
        let mut max = 0;
        // the watchdog path (including console hiding and Windows Terminal recording) must produce no
        // violations or notes either
        let side = WatchSide::new(Default::default());
        while start.elapsed() < Duration::from_secs(6) {
            max = max.max(win_screen::our_pids(pid).len());
            assert_eq!(scan_violation(pid, pid), None);
            assert_eq!(scan(pid, pid, &side), None);
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(side.notes.lock().is_empty(), "{:?}", side.notes.lock());
        assert!(side.console.lock().first_hidden.is_empty());
        let wins = crate::platform::win::windows_of(&win_screen::our_pids(pid));
        eprintln!("Job 프로세스 최대 {max}개, 최상위 창 {}개: {:?}", wins.len(), wins.iter().map(win_screen::state).collect::<Vec<_>>());
        assert!(max >= 2, "Chrome 자손이 Job 에 들어가야 함");
        terminate_and_reap(pid, pid, None, &c);
        std::thread::sleep(Duration::from_millis(300));
        let _ = crate::platform::fsx::remove_dir_all_retry(&prof);
    }
}
