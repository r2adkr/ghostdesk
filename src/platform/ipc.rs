//! 같은 사용자 프로세스끼리의 로컬 연결 (GhostDesk 창 ↔ `ghostdesk --mcp`).
//!
//! - Unix: 데이터 루트의 유닉스 도메인 소켓 `control.sock` (권한 0600 + 상대 uid 확인) — 기존과 같다.
//! - Windows: 명명 파이프 `\\.\pipe\GhostDesk-control-<사용자 SID>`.
//!   - 보안 설명자: 소유자 = 이 사용자, DACL = 이 사용자 + SYSTEM 만 (다른 사용자는 열 수 없음)
//!   - 원격(SMB) 클라이언트 거부, 첫 인스턴스 플래그로 "이미 다른 창이 받고 있음"을 감지
//!   - 서버: 클라이언트 프로세스의 사용자 SID 를 한 번 더 확인 (확인할 수 없으면 DACL 에 맡김)
//!   - 클라이언트: 파이프 **소유자** SID 가 이 사용자인지 확인 (다른 사용자가 이름을 먼저 차지하는 공격 방지)
//!     + 파이프 객체의 무결성 레이블과 서버 프로세스의 무결성 수준이 보통(클라이언트가 그보다 낮으면 클라이언트 수준)보다
//!       낮지 않은지 확인 (같은 사용자의 샌드박스 프로세스가 차지하는 것 방지)
//!
//! 스트림은 읽기·쓰기 반쪽으로 나눠 상자에 담아 돌려주므로, 위쪽 코드(줄 단위 JSON RPC)는 OS 를 모른다.

#[cfg(unix)]
use std::path::PathBuf;

use tokio::io::{AsyncRead, AsyncWrite};

pub type Reader = Box<dyn AsyncRead + Send + Unpin>;
pub type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// 연결 지점
#[derive(Clone, Debug)]
pub struct Endpoint {
    #[cfg(unix)]
    path: PathBuf,
    #[cfg(windows)]
    name: String,
}

impl Endpoint {
    /// GhostDesk 창의 제어 연결 지점
    pub fn control() -> Endpoint {
        #[cfg(unix)]
        {
            Endpoint { path: super::paths::data_root().join("control.sock") }
        }
        #[cfg(windows)]
        {
            let who = super::win::current_user_sid().unwrap_or_else(|_| {
                // SID 를 못 얻는 경우는 사실상 없지만, 그래도 사용자마다 다른 이름이 되게 한다
                std::env::var("USERNAME").unwrap_or_else(|_| "user".into())
            });
            Endpoint { name: format!(r"\\.\pipe\GhostDesk-control-{who}") }
        }
    }

    /// 테스트·다른 용도용: 임의 이름(Unix 는 경로)
    #[allow(dead_code)]
    pub fn custom(id: &str) -> Endpoint {
        #[cfg(unix)]
        {
            Endpoint { path: std::env::temp_dir().join(format!("{id}.sock")) }
        }
        #[cfg(windows)]
        {
            let who = super::win::current_user_sid().unwrap_or_default();
            Endpoint { name: format!(r"\\.\pipe\{id}-{who}") }
        }
    }

    /// 사람이 읽을 위치 (UI·오류 메시지)
    pub fn display(&self) -> String {
        #[cfg(unix)]
        {
            self.path.display().to_string()
        }
        #[cfg(windows)]
        {
            self.name.clone()
        }
    }

    #[cfg(unix)]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// 연결 실패의 종류
#[derive(Debug)]
pub enum ConnectError {
    /// 받는 쪽이 없다 (GhostDesk 창이 꺼져 있음) → 창을 띄우고 다시 시도
    NotRunning(std::io::Error),
    /// 연결 지점은 있지만 쓸 수 없다 (다른 사용자의 것, 권한 없음 등) → 다시 시도해도 소용없음.
    /// Windows 전용: Unix 는 권한 오류도 기존처럼 창을 띄우고 다시 시도한다 (`Other`).
    #[cfg_attr(unix, allow(dead_code))]
    Denied(String),
    Other(std::io::Error),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NotRunning(e) => write!(f, "GhostDesk 창이 연결을 받고 있지 않습니다 ({e})"),
            ConnectError::Denied(m) => write!(f, "{m}"),
            ConnectError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ConnectError {}

// ───────────────────────── Unix ─────────────────────────

#[cfg(unix)]
pub async fn connect(ep: &Endpoint) -> Result<(Reader, Writer), ConnectError> {
    match tokio::net::UnixStream::connect(&ep.path).await {
        Ok(s) => {
            let (r, w) = s.into_split();
            Ok((Box::new(r), Box::new(w)))
        }
        // 권한 오류(EACCES)도 Denied 가 아니라 Other: 기존처럼 창을 띄우고 다시 시도한다.
        // 새로 뜬 창의 bind 가 연결할 수 없는 남은 소켓(예: sudo 로 실행했던 GhostDesk 의 것)을 지우고 다시 만든다.
        Err(e) => match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => Err(ConnectError::NotRunning(e)),
            _ => Err(ConnectError::Other(e)),
        },
    }
}

#[cfg(unix)]
pub struct Listener {
    inner: tokio::net::UnixListener,
}

#[cfg(unix)]
impl Listener {
    /// 연결을 받기 시작한다. 다른 GhostDesk 가 이미 받고 있으면 오류.
    pub async fn bind(ep: &Endpoint) -> anyhow::Result<Listener> {
        use anyhow::Context;
        use std::os::unix::fs::PermissionsExt;
        let path = &ep.path;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if path.exists() {
            if tokio::net::UnixStream::connect(path).await.is_ok() {
                anyhow::bail!("다른 GhostDesk 창이 이미 외부 연결을 받고 있습니다");
            }
            let _ = std::fs::remove_file(path);
        }
        let inner = tokio::net::UnixListener::bind(path).with_context(|| format!("{} 열기 실패", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Listener { inner })
    }

    /// 같은 사용자의 연결 하나를 받는다 (다른 uid 는 조용히 버린다).
    pub async fn accept(&mut self) -> std::io::Result<(Reader, Writer)> {
        loop {
            let (stream, _) = self.inner.accept().await?;
            // 같은 사용자만 허용 (소켓 권한 0600 에 더해 한 번 더 확인)
            if let Ok(cred) = stream.peer_cred() {
                if cred.uid() != unsafe { libc::getuid() } {
                    continue;
                }
            }
            let (r, w) = stream.into_split();
            return Ok((Box::new(r), Box::new(w)));
        }
    }
}

// ───────────────────────── Windows ─────────────────────────

#[cfg(windows)]
const ERROR_FILE_NOT_FOUND: i32 = 2;
#[cfg(windows)]
const ERROR_ACCESS_DENIED: i32 = 5;
#[cfg(windows)]
const ERROR_PIPE_BUSY: i32 = 231;

#[cfg(windows)]
pub async fn connect(ep: &Endpoint) -> Result<(Reader, Writer), ConnectError> {
    use std::os::windows::io::AsRawHandle;
    use std::time::{Duration, Instant};
    use tokio::net::windows::named_pipe::ClientOptions;

    let start = Instant::now();
    let client = loop {
        match ClientOptions::new().open(&ep.name) {
            Ok(c) => break c,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                // 서버가 다음 인스턴스를 만드는 아주 짧은 틈: 잠깐 뒤 다시
                if start.elapsed() > Duration::from_secs(5) {
                    return Err(ConnectError::Other(e));
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => return Err(ConnectError::NotRunning(e)),
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                return Err(ConnectError::Denied(format!(
                    "GhostDesk 연결 파이프({})에 접근할 수 없습니다: 다른 사용자(또는 관리자 권한)로 실행된 GhostDesk 일 수 있습니다 ({e})",
                    ep.name
                )))
            }
            Err(e) => return Err(ConnectError::Other(e)),
        }
    };
    check_server(client.as_raw_handle() as _)?;
    let (r, w) = tokio::io::split(client);
    Ok((Box::new(r), Box::new(w)))
}

/// 연결한 파이프의 서버가 믿을 만한지 확인한다 (다른 프로그램이 같은 이름을 먼저 만들어 가로채는 것 방지).
/// 1. 파이프 소유자가 이 사용자인가 (다른 사용자)
/// 2. 파이프의 무결성 수준이 낮지 않은가 (같은 사용자라도 샌드박스 안의 낮은 무결성 프로세스는 소유자가
///    이 사용자로 같으므로 1 만으로는 못 막는다)
///    - 파이프 객체의 무결성 레이블: 낮은 무결성 프로세스가 만든 객체에는 그 수준이 붙고, 만든 쪽이 올리거나
///      뗄 수 없다. 자기 프로세스의 DACL 을 비워 토큰 조회를 막아도 이것은 가릴 수 없다.
///    - 파이프를 연 서버 프로세스의 토큰 무결성 수준 (읽을 수 있으면 한 번 더, 오류 메시지에 프로그램 이름도 담는다)
#[cfg(windows)]
fn check_server(pipe: windows_sys::Win32::Foundation::HANDLE) -> Result<(), ConnectError> {
    use super::win;
    let me = win::current_user_sid().map_err(ConnectError::Other)?;
    match win::object_owner_sid(pipe) {
        Ok(owner) if owner == me => {}
        Ok(owner) => {
            return Err(ConnectError::Denied(format!(
                "GhostDesk 연결 파이프의 소유자({owner})가 현재 사용자가 아닙니다. 다른 프로그램이 이름을 가로챘을 수 있어 연결하지 않습니다."
            )))
        }
        Err(e) => return Err(ConnectError::Denied(format!("GhostDesk 연결 파이프의 소유자를 확인하지 못했습니다: {e}"))),
    }
    let mine = win::current_integrity_rid().map_err(ConnectError::Other)?;
    let label = win::object_integrity_rid(pipe).map_err(|e| {
        ConnectError::Denied(format!("GhostDesk 연결 파이프의 무결성 수준을 확인하지 못했습니다: {e}"))
    })?;
    let mut pid = 0u32;
    if unsafe { windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId(pipe, &mut pid) } == 0 {
        return Err(ConnectError::Denied(format!(
            "GhostDesk 연결 파이프를 연 프로세스를 확인하지 못했습니다: {}",
            std::io::Error::last_os_error()
        )));
    }
    server_integrity_verdict(label, win::process_integrity_rid(pid), mine).map_err(|why| {
        let who = win::process_path(pid).map(|p| format!("{} (pid {pid})", p.display())).unwrap_or_else(|| format!("pid {pid}"));
        ConnectError::Denied(format!(
            "GhostDesk 연결 파이프를 연 프로세스 {who} 의 {why}. 다른 프로그램이 이름을 가로챘을 수 있어 연결하지 않습니다."
        ))
    })
}

/// 서버(파이프)에 필요한 최소 무결성 수준: 이 프로세스의 수준, 단 보통(Medium)을 넘지 않는다.
/// 관리자 권한(높음)으로 실행된 `ghostdesk --mcp` 도 보통 권한의 GhostDesk 창에 연결할 수 있어야 하고,
/// 막아야 할 것은 보통보다 낮은(샌드박스) 프로세스가 이름을 차지하는 경우다.
#[cfg(windows)]
fn required_integrity(mine: u32) -> u32 {
    mine.min(super::win::INTEGRITY_MEDIUM)
}

/// 서버의 무결성 판정. 거부하면 사유(문장 일부)를 돌려준다.
/// - `label`: 파이프 객체의 무결성 레이블 (레이블 없음 = 보통). 필요한 수준보다 낮으면 거부 — 이것이 기준이다.
/// - `process`: 서버 프로세스 토큰의 무결성 수준. 필요한 수준보다 낮으면 거부.
///   읽을 권한이 없으면(ERROR_ACCESS_DENIED) 더 높은 권한의 프로세스이거나 자기 DACL 을 막아 둔 프로세스인데,
///   어느 쪽이든 파이프 레이블로 이미 판단했으므로 허용. 그 밖의 오류(프로세스가 이미 끝남 등)는 거부.
#[cfg(windows)]
fn server_integrity_verdict(label: u32, process: std::io::Result<u32>, mine: u32) -> Result<(), String> {
    use super::win::integrity_name;
    let need = required_integrity(mine);
    if label < need {
        return Err(format!(
            "파이프 무결성 수준({})이 필요한 수준({})보다 낮습니다",
            integrity_name(label),
            integrity_name(need)
        ));
    }
    match process {
        Ok(rid) if rid < need => Err(format!(
            "무결성 수준({})이 필요한 수준({})보다 낮습니다",
            integrity_name(rid),
            integrity_name(need)
        )),
        Ok(_) => Ok(()),
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => Ok(()),
        Err(e) => Err(format!("무결성 수준을 확인하지 못했습니다 ({e})")),
    }
}

/// 이미 열린 같은 이름의 파이프가 믿을 수 없는 프로그램의 것이면 그 사유.
/// 잠깐 연결해 `check_server` 로 확인한다 (GhostDesk 창의 것이면 빈 연결 하나가 곧바로 끝날 뿐이다).
/// 연결할 수 없으면(더 높은 권한의 GhostDesk 창, 빈 인스턴스 없음 등) 판단하지 않는다.
#[cfg(windows)]
fn foreign_pipe_owner(name: &str) -> Option<String> {
    use std::os::windows::io::AsRawHandle;
    let probe = tokio::net::windows::named_pipe::ClientOptions::new().open(name).ok()?;
    match check_server(probe.as_raw_handle() as _) {
        Err(ConnectError::Denied(m)) => Some(m),
        _ => None,
    }
}

#[cfg(windows)]
pub struct Listener {
    name: String,
    sd: super::win::SecurityDescriptor,
    next: tokio::net::windows::named_pipe::NamedPipeServer,
}

#[cfg(windows)]
impl Listener {
    fn create(name: &str, sd: &super::win::SecurityDescriptor, first: bool) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
        use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};
        let mut sa = sd.attributes();
        let mut opts = ServerOptions::new();
        opts.first_pipe_instance(first).reject_remote_clients(true).pipe_mode(PipeMode::Byte);
        // SAFETY: sa 와 그 안의 보안 설명자는 호출하는 동안 살아 있다
        unsafe { opts.create_with_security_attributes_raw(name, &mut sa as *mut _ as *mut core::ffi::c_void) }
    }

    /// 연결을 받기 시작한다. 다른 GhostDesk 가 이미 받고 있으면 오류.
    pub async fn bind(ep: &Endpoint) -> anyhow::Result<Listener> {
        let sd = super::win::SecurityDescriptor::owner_only()?;
        let next = match Self::create(&ep.name, &sd, true) {
            Ok(s) => s,
            // 첫 인스턴스가 아니면 ACCESS_DENIED: 이미 다른 GhostDesk 창(또는 같은 이름을 가진 무언가)이 있다
            // 인스턴스 수가 꽉 찬 파이프면 PIPE_BUSY: GhostDesk 창은 인스턴스 수를 제한하지 않으므로 다른 프로그램의 것이다
            Err(e) if matches!(e.raw_os_error(), Some(ERROR_ACCESS_DENIED) | Some(ERROR_PIPE_BUSY)) => {
                if let Some(why) = foreign_pipe_owner(&ep.name) {
                    anyhow::bail!("GhostDesk 가 아닌 다른 프로그램이 연결 파이프 이름을 차지하고 있어 외부 연결을 받을 수 없습니다: {why}")
                }
                if e.raw_os_error() == Some(ERROR_PIPE_BUSY) {
                    anyhow::bail!(
                        "GhostDesk 가 아닌 다른 프로그램이 연결 파이프({})를 차지하고 있어 외부 연결을 받을 수 없습니다 ({e})",
                        ep.name
                    )
                }
                anyhow::bail!("다른 GhostDesk 창이 이미 외부 연결을 받고 있습니다")
            }
            Err(e) => return Err(anyhow::anyhow!("{} 열기 실패: {e}", ep.name)),
        };
        Ok(Listener { name: ep.name.clone(), sd, next })
    }

    /// 같은 사용자의 연결 하나를 받는다.
    pub async fn accept(&mut self) -> std::io::Result<(Reader, Writer)> {
        use std::os::windows::io::AsRawHandle;
        loop {
            if let Err(e) = self.next.connect().await {
                // 고장 난 인스턴스를 계속 재시도하지 않게 새 인스턴스로 바꿔 두고 오류를 알린다
                // (클라이언트가 붙자마자 끊은 경우(ERROR_NO_DATA)는 연결된 것으로 처리되어 여기 오지 않는다)
                if let Ok(fresh) = Self::create(&self.name, &self.sd, false) {
                    self.next = fresh;
                }
                return Err(e);
            }
            // 다음 클라이언트를 위해 곧바로 새 인스턴스를 만든다 (틈이 생기면 클라이언트는 PIPE_BUSY 로 재시도)
            let fresh = Self::create(&self.name, &self.sd, false)?;
            let conn = std::mem::replace(&mut self.next, fresh);
            // 클라이언트 프로세스의 사용자 확인: DACL 이 이미 막지만 한 번 더 (확인할 수 없으면 DACL 에 맡긴다)
            let mut pid = 0u32;
            let got = unsafe {
                windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId(conn.as_raw_handle() as _, &mut pid)
            } != 0;
            if got {
                if let (Ok(peer), Ok(me)) = (super::win::process_user_sid(pid), super::win::current_user_sid()) {
                    if peer != me {
                        eprintln!("[ghostdesk] 다른 사용자({peer})의 연결을 거부했습니다");
                        continue;
                    }
                }
            }
            let (r, w) = tokio::io::split(conn);
            return Ok((Box::new(r), Box::new(w)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn roundtrip_and_single_instance() {
        let ep = Endpoint::custom(&format!("ghostdesk-ipc-test-{}", std::process::id()));
        #[cfg(unix)]
        let _ = std::fs::remove_file(ep.path());
        // 아무도 없으면 NotRunning
        assert!(matches!(connect(&ep).await, Err(ConnectError::NotRunning(_))));
        let mut l = Listener::bind(&ep).await.unwrap();
        // 두 번째 bind 는 거부 (같은 프로세스가 연 파이프이므로 '다른 프로그램'이 아니라 '다른 GhostDesk 창')
        let dup = Listener::bind(&ep).await.err().expect("두 번째 bind 는 실패해야 함");
        assert!(format!("{dup:#}").contains("다른 GhostDesk 창"), "{dup:#}");
        let server = tokio::spawn(async move {
            let mut served = 0;
            while served < 2 {
                let (r, mut w) = l.accept().await.unwrap();
                let mut lines = BufReader::new(r).lines();
                // Windows: 두 번째 bind 가 확인하려고 잠깐 연결했다 끊은 빈 연결은 건너뛴다
                let Ok(Some(line)) = lines.next_line().await else { continue };
                w.write_all(format!("echo:{line}\n").as_bytes()).await.unwrap();
                served += 1;
            }
        });
        for i in 0..2 {
            let (r, mut w) = connect(&ep).await.unwrap();
            w.write_all(format!("hi{i}\n").as_bytes()).await.unwrap();
            let mut lines = BufReader::new(r).lines();
            assert_eq!(lines.next_line().await.unwrap().unwrap(), format!("echo:hi{i}"));
        }
        server.await.unwrap();
        #[cfg(unix)]
        let _ = std::fs::remove_file(ep.path());
    }

    #[cfg(windows)]
    #[test]
    fn server_integrity_rules() {
        use super::super::win::{INTEGRITY_HIGH, INTEGRITY_LOW, INTEGRITY_MEDIUM};
        const M: u32 = INTEGRITY_MEDIUM;
        // 같거나 높으면 허용, 낮으면 거부
        assert!(server_integrity_verdict(M, Ok(M), M).is_ok());
        assert!(server_integrity_verdict(M, Ok(INTEGRITY_HIGH), M).is_ok());
        let low = server_integrity_verdict(M, Ok(INTEGRITY_LOW), M).unwrap_err();
        assert!(low.contains("무결성 수준(낮음)") && low.contains("보통"), "{low}");
        assert!(server_integrity_verdict(M, Ok(0), M).is_err());
        // 관리자 권한(높음) 클라이언트도 보통 권한의 창에는 연결한다 (보통 미만만 거부)
        assert!(server_integrity_verdict(M, Ok(M), INTEGRITY_HIGH).is_ok());
        assert!(server_integrity_verdict(M, Ok(INTEGRITY_LOW), INTEGRITY_HIGH).is_err());
        // 낮은 무결성 클라이언트끼리는 그 수준까지 허용
        assert!(server_integrity_verdict(INTEGRITY_LOW, Ok(INTEGRITY_LOW), INTEGRITY_LOW).is_ok());
        // 파이프 레이블이 낮으면 프로세스 토큰과 상관없이 거부 (토큰을 숨겨도 마찬가지)
        let denied = || std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED);
        let lbl = server_integrity_verdict(INTEGRITY_LOW, Err(denied()), M).unwrap_err();
        assert!(lbl.contains("파이프 무결성 수준(낮음)"), "{lbl}");
        assert!(server_integrity_verdict(INTEGRITY_LOW, Ok(INTEGRITY_HIGH), INTEGRITY_HIGH).is_err());
        // 레이블이 보통 이상이고 토큰을 읽을 권한이 없으면(더 높은 권한) 허용, 그 밖의 오류(끝난 프로세스 등) → 거부
        assert!(server_integrity_verdict(M, Err(denied()), M).is_ok());
        let gone = std::io::Error::from_raw_os_error(87); // ERROR_INVALID_PARAMETER
        assert!(server_integrity_verdict(M, Err(gone), M).is_err());
    }

    /// 이 프로세스가 만든 객체: 레이블 없음(보통으로 봄) 또는 이 프로세스 수준 이상, 그리고 명시한 낮은 레이블은 그대로 읽힌다
    #[cfg(windows)]
    #[test]
    fn object_integrity_label() {
        use super::super::win::{self, INTEGRITY_LOW, INTEGRITY_MEDIUM};
        use std::os::windows::io::AsRawHandle;
        let mine = win::current_integrity_rid().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _g = rt.enter();
        let plain = Endpoint::custom(&format!("ghostdesk-label-plain-{}", std::process::id())).name;
        let sd = win::SecurityDescriptor::owner_only().unwrap();
        let p = Listener::create(&plain, &sd, true).unwrap();
        let rid = win::object_integrity_rid(p.as_raw_handle() as _).unwrap();
        assert!(rid >= mine.min(INTEGRITY_MEDIUM), "레이블 {rid:#x}, 이 프로세스 {mine:#x}");
        // 낮은 무결성 레이블을 명시한 파이프 (자기보다 낮은 레이블은 누구나 붙일 수 있다)
        let low = Endpoint::custom(&format!("ghostdesk-label-low-{}", std::process::id())).name;
        let sid = win::current_user_sid().unwrap();
        let lsd = win::SecurityDescriptor::from_sddl(&format!("O:{sid}D:P(A;;GA;;;{sid})S:(ML;;NW;;;LW)")).unwrap();
        let q = Listener::create(&low, &lsd, true).unwrap();
        assert_eq!(win::object_integrity_rid(q.as_raw_handle() as _).unwrap(), INTEGRITY_LOW);
        // 그 파이프에 클라이언트로 붙으면 check_server 가 거부한다 (보통 이상에서 실행할 때)
        if mine >= INTEGRITY_MEDIUM {
            let c = tokio::net::windows::named_pipe::ClientOptions::new().open(&low).unwrap();
            match check_server(c.as_raw_handle() as _) {
                Err(ConnectError::Denied(m)) => assert!(m.contains("파이프 무결성 수준(낮음)"), "{m}"),
                other => panic!("낮은 레이블 파이프를 받아들임: {:?}", other.err()),
            }
        }
    }

    /// 수동 확인용: 낮은 무결성(Low IL) 프로세스가 미리 만들어 둔 파이프(빈 인스턴스 2개)의 전체 이름을
    /// GHOSTDESK_TEST_LOWIL_PIPE 로 주면, 클라이언트 연결과 창의 bind 가 모두 '가로챈 프로그램'으로 알아챈다.
    #[cfg(windows)]
    #[tokio::test]
    #[ignore]
    async fn low_integrity_squatter_is_refused() {
        let Ok(name) = std::env::var("GHOSTDESK_TEST_LOWIL_PIPE") else { return };
        let ep = Endpoint { name };
        match connect(&ep).await {
            Err(ConnectError::Denied(m)) => {
                eprintln!("connect: {m}");
                assert!(m.contains("무결성 수준(낮음)"), "{m}")
            }
            Err(e) => panic!("다른 오류: {e}"),
            Ok(_) => panic!("낮은 무결성 서버에 연결됨"),
        }
        let e = Listener::bind(&ep).await.err().expect("이름이 이미 있으므로 bind 는 실패");
        eprintln!("bind: {e:#}");
        assert!(format!("{e:#}").contains("GhostDesk 가 아닌 다른 프로그램"), "{e:#}");
    }

    /// 이 프로세스가 연 파이프는 소유자·무결성 확인을 통과한다
    #[cfg(windows)]
    #[tokio::test]
    async fn own_pipe_passes_server_check() {
        use std::os::windows::io::AsRawHandle;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let ep = Endpoint::custom(&format!("ghostdesk-ipc-check-{}", std::process::id()));
        let mut l = Listener::bind(&ep).await.unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let n = accepted.clone();
        let server = tokio::spawn(async move {
            while let Ok(c) = l.accept().await {
                n.fetch_add(1, Ordering::SeqCst);
                drop(c);
            }
        });
        let c = tokio::net::windows::named_pipe::ClientOptions::new().open(&ep.name).unwrap();
        assert!(check_server(c.as_raw_handle() as _).is_ok());
        drop(c);
        // 서버가 다음 인스턴스를 만들 때까지 기다렸다가, bind 가 쓰는 확인(잠깐 연결)도 통과하는지
        let t0 = std::time::Instant::now();
        while accepted.load(Ordering::SeqCst) < 1 {
            assert!(t0.elapsed() < std::time::Duration::from_secs(5));
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(foreign_pipe_owner(&ep.name), None);
        while accepted.load(Ordering::SeqCst) < 2 {
            assert!(t0.elapsed() < std::time::Duration::from_secs(5), "확인용 연결이 서버에 닿지 않음");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        server.abort();
    }
}
