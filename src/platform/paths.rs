//! Where GhostDesk keeps its folders and files.
//!
//! | | macOS (unchanged) | Windows | Linux |
//! |---|---|---|---|
//! | Data root | ~/Library/Application Support/GhostDesk | %LOCALAPPDATA%\GhostDesk | ~/.local/share/GhostDesk |
//! | Logs | ~/Library/Logs/GhostDesk | %LOCALAPPDATA%\GhostDesk\logs | <data root>/logs |
//!
//! Windows uses %LOCALAPPDATA% rather than roaming %APPDATA%: cloned browser profiles can be
//! several GB, and settings (auto-connect and the like) are tied to this machine's install path
//! and must not roam to another PC. `dirs` uses SHGetKnownFolderPath on Windows, so the location
//! is the same even when an MCP host launches us with a scrubbed environment.

use std::path::{Path, PathBuf};

/// GhostDesk data root (parent of workspaces, settings and the control socket).
pub fn data_root() -> PathBuf {
    // for tests and screenshots: use a separate folder so the user's settings are untouched
    if let Some(d) = std::env::var_os("GHOSTDESK_DATA_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    #[cfg(windows)]
    let base = dirs::data_local_dir();
    #[cfg(not(windows))]
    let base = dirs::data_dir();
    base.unwrap_or_else(std::env::temp_dir).join("GhostDesk")
}

/// Root for workspaces (cloned profiles, app data).
pub fn workspaces_root() -> PathBuf {
    data_root().join("workspaces")
}

/// App settings file (auto-connect and such).
pub fn settings_path() -> PathBuf {
    data_root().join("settings.json")
}

/// Log folder.
pub fn logs_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().unwrap_or_else(std::env::temp_dir).join("Library/Logs/GhostDesk")
    }
    #[cfg(not(target_os = "macos"))]
    {
        data_root().join("logs")
    }
}

/// Executable name, as used for installing and in connection configs.
pub const EXE_NAME: &str = if cfg!(windows) { "ghostdesk.exe" } else { "ghostdesk" };

/// Strips a verbatim prefix like `\\?\C:\...`, but only when that's safe.
///
/// `canonicalize` on Windows always returns verbatim paths, which other programs (the Claude
/// app, Codex, cmd.exe, …) may not understand and people find hard to read.
pub fn simplify(p: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.as_os_str().to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            // a drive-letter path under the 260-char limit means the same thing without the prefix
            let b = rest.as_bytes();
            if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' && rest.len() < 248 {
                return PathBuf::from(rest);
            }
        }
        p.to_path_buf()
    }
    #[cfg(not(windows))]
    {
        p.to_path_buf()
    }
}

/// Like `std::fs::canonicalize`, but without the verbatim prefix on Windows.
pub fn canonicalize(p: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(p).map(|c| simplify(&c))
}

/// Whether two paths point to the same file (the default Windows and macOS file systems are
/// case-insensitive).
pub fn same_path(a: &Path, b: &Path) -> bool {
    let ca = canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    if cfg!(any(windows, target_os = "macos")) {
        ca.to_string_lossy().to_lowercase() == cb.to_string_lossy().to_lowercase()
    } else {
        ca == cb
    }
}

/// Reserved Windows device names (CON, NUL, COM1, …) can't be used as folder names.
///
/// After COM and LPT, the digits 0–9 and the superscripts ¹ ² ³ (U+00B9, U+00B2, U+00B3) also
/// form device names. The superscripts are two bytes in UTF-8, so compare characters, not bytes.
pub fn is_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let cs: Vec<char> = stem.chars().collect();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (cs.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && (cs[3].is_ascii_digit() || matches!(cs[3], '\u{b9}' | '\u{b2}' | '\u{b3}')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_names() {
        for n in ["con", "CON", "nul.txt", "COM1", "lpt9", "aux", "COM0", "COM\u{b2}", "lpt\u{b9}", "Com\u{b3}.log", "LPT\u{b3} "] {
            assert!(is_reserved_name(n), "{n}");
        }
        for n in ["console", "com", "work", "COM10", "nul_", "_nul", "COM\u{b2}\u{b2}", "COM\u{2074}", "LPT\u{b9}x", "COMa", "_COM\u{b2}"] {
            assert!(!is_reserved_name(n), "{n}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn simplify_verbatim() {
        assert_eq!(simplify(Path::new(r"\\?\C:\Users\a\b")), PathBuf::from(r"C:\Users\a\b"));
        assert_eq!(simplify(Path::new(r"\\?\UNC\srv\share\x")), PathBuf::from(r"\\srv\share\x"));
        assert_eq!(simplify(Path::new(r"C:\x")), PathBuf::from(r"C:\x"));
    }

    #[test]
    fn data_layout() {
        assert!(workspaces_root().starts_with(data_root()));
        assert!(settings_path().starts_with(data_root()));
        #[cfg(windows)]
        assert!(data_root().starts_with(dirs::data_local_dir().unwrap()));
    }
}
