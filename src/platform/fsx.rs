//! 파일 시스템 도우미 (Windows 의 파일 잠금·BOM 같은 차이를 흡수).

use std::path::Path;
use std::time::Duration;

/// UTF-8 BOM 을 떼어 낸다. (Windows PowerShell 5.1 의 `Set-Content -Encoding UTF8` 등은 BOM 을 붙이고,
/// serde_json 은 BOM 이 있으면 파싱하지 못한다.) 반환값의 bool 은 BOM 이 있었는지.
pub fn strip_bom(s: &str) -> (&str, bool) {
    match s.strip_prefix('\u{FEFF}') {
        Some(rest) => (rest, true),
        None => (s, false),
    }
}

/// Windows 에서 다른 프로세스(백신·인덱서·사용자 앱)가 잠깐 파일을 쥐고 있어 생기는
/// 공유 위반(32)·잠금 위반(33)·접근 거부(5)인가.
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

/// 원자적 교체(rename). Windows 에서는 잠깐의 공유 위반을 몇 번 재시도한다.
/// (std 의 rename 은 Windows 에서 MoveFileExW(MOVEFILE_REPLACE_EXISTING) 이다.)
pub fn rename_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    retry(|| std::fs::rename(from, to))
}

/// 폴더 통째로 지우기 (Windows 에서 방금 끝난 프로세스가 쥐고 있던 파일은 잠시 뒤 풀린다)
pub fn remove_dir_all_retry(p: &Path) -> std::io::Result<()> {
    retry(|| match std::fs::remove_dir_all(p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    })
}

/// 파일 지우기 (없으면 성공)
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
