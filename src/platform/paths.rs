//! GhostDesk 가 쓰는 폴더·파일 위치.
//!
//! | | macOS (기존과 같음) | Windows | Linux |
//! |---|---|---|---|
//! | 데이터 루트 | ~/Library/Application Support/GhostDesk | %LOCALAPPDATA%\GhostDesk | ~/.local/share/GhostDesk |
//! | 로그 | ~/Library/Logs/GhostDesk | %LOCALAPPDATA%\GhostDesk\logs | <데이터 루트>/logs |
//!
//! Windows 에서는 %APPDATA%(로밍)가 아니라 %LOCALAPPDATA% 를 쓴다: 복제한 브라우저 프로필은 수 GB 가
//! 될 수 있고, 설정(자동 연결 등)도 이 컴퓨터의 설치 경로에 묶여 있어 다른 PC 로 옮겨 가면 안 된다.
//! `dirs` 는 Windows 에서 SHGetKnownFolderPath 를 쓰므로, MCP 호스트가 환경변수를 지운 채
//! 실행해도 같은 위치가 나온다.

use std::path::{Path, PathBuf};

/// GhostDesk 데이터 루트 (작업공간·설정·제어 소켓의 부모)
pub fn data_root() -> PathBuf {
    // 시험·사진 찍기용: 사용자 설정을 건드리지 않게 따로 둔 폴더를 쓴다
    if let Some(d) = std::env::var_os("GHOSTDESK_DATA_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    #[cfg(windows)]
    let base = dirs::data_local_dir();
    #[cfg(not(windows))]
    let base = dirs::data_dir();
    base.unwrap_or_else(std::env::temp_dir).join("GhostDesk")
}

/// 작업공간(복제된 프로필·앱 데이터) 루트
pub fn workspaces_root() -> PathBuf {
    data_root().join("workspaces")
}

/// 앱 설정 파일 (자동 연결 등)
pub fn settings_path() -> PathBuf {
    data_root().join("settings.json")
}

/// 로그 폴더
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

/// 실행 파일 이름 (설치·연결 설정에 쓰는 이름)
pub const EXE_NAME: &str = if cfg!(windows) { "ghostdesk.exe" } else { "ghostdesk" };

/// `\\?\C:\...` 같은 verbatim 접두사를 (안전할 때만) 떼어 낸다.
/// Windows 의 `canonicalize` 는 항상 verbatim 경로를 돌려주는데, 그대로 두면 다른 프로그램
/// (Claude 앱·Codex·cmd.exe 등)이 인식하지 못하거나 사람이 읽기 어렵다.
pub fn simplify(p: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.as_os_str().to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            // 드라이브 문자 경로이고 260자 제한 안이면 접두사를 떼도 의미가 같다
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

/// `std::fs::canonicalize` 와 같지만 Windows 에서 verbatim 접두사를 붙이지 않는다.
pub fn canonicalize(p: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(p).map(|c| simplify(&c))
}

/// 두 경로가 같은 파일을 가리키는가 (Windows·macOS 기본 파일 시스템은 대소문자 무시)
pub fn same_path(a: &Path, b: &Path) -> bool {
    let ca = canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    if cfg!(any(windows, target_os = "macos")) {
        ca.to_string_lossy().to_lowercase() == cb.to_string_lossy().to_lowercase()
    } else {
        ca == cb
    }
}

/// Windows 예약 장치 이름(CON, NUL, COM1 …)은 폴더 이름으로 쓸 수 없다.
/// COM·LPT 뒤에는 0~9 와 위 첨자 ¹ ² ³ (U+00B9, U+00B2, U+00B3) 도 장치 이름이 된다.
/// 위 첨자는 UTF-8 로 2바이트이므로 바이트가 아니라 글자 단위로 비교한다.
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
