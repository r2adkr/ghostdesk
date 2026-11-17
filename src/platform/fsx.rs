//! File system helpers that paper over Windows quirks such as transient file locks and BOMs.

use std::path::Path;
use std::time::Duration;

/// Strips a UTF-8 BOM and reports whether there was one.
///
/// Windows PowerShell 5.1 (`Set-Content -Encoding UTF8` and friends) writes a BOM, and
/// serde_json refuses to parse one.
pub fn strip_bom(s: &str) -> (&str, bool) {
    match s.strip_prefix('\u{FEFF}') {
        Some(rest) => (rest, true),
        None => (s, false),
    }
}

/// Whether `e` is a sharing violation (32), lock violation (33) or access denied (5) caused by
/// another process (antivirus, indexer, the user's app) briefly holding the file on Windows.
pub fn is_transient_lock(e: &std::io::Error) -> bool {
    cfg!(windows) && matches!(e.raw_os_error(), Some(5) | Some(32) | Some(33))
}

fn retry<T>(mut f: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut wait = Duration::from_millis(25);
    for _ in 0..8 {
        match f() {
            Err(e) if is_transient_lock(&e) => {
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_millis(400));
            }
            r => return r,
        }
    }
    f()
}

/// Atomically replaces `to` with `from`, retrying briefly on transient sharing violations on
/// Windows. (`std::fs::rename` is `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` there.)
pub fn rename_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    retry(|| std::fs::rename(from, to))
}

/// Removes a directory tree, retrying while files held by a just-exited process are released.
pub fn remove_dir_all_retry(p: &Path) -> std::io::Result<()> {
    retry(|| match std::fs::remove_dir_all(p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    })
}

/// Removes a file. A missing file counts as success.
pub fn remove_file_retry(p: &Path) -> std::io::Result<()> {
    retry(|| match std::fs::remove_file(p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bom() {
        assert_eq!(strip_bom("\u{FEFF}{}"), ("{}", true));
        assert_eq!(strip_bom("{}"), ("{}", false));
        assert!(serde_json::from_str::<serde_json::Value>(strip_bom("\u{FEFF}{\"a\":1}").0).is_ok());
    }

    #[test]
    fn rename_and_remove() {
        let d = std::env::temp_dir().join(format!("gd-fsx-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let a = d.join("a.txt");
        let b = d.join("b.txt");
        std::fs::write(&a, "1").unwrap();
        std::fs::write(&b, "2").unwrap();
        rename_replace(&a, &b).unwrap();
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "1");
        remove_file_retry(&a).unwrap();
        remove_dir_all_retry(&d).unwrap();
        assert!(!d.exists());
        remove_dir_all_retry(&d).unwrap();
    }
}
