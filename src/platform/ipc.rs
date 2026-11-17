//! Local connections between processes of the same user (GhostDesk window ↔ `ghostdesk --mcp`).
//!
//! - Unix: a Unix domain socket `control.sock` in the data root (mode 0600 plus a peer uid
//!   check), as before.
//! - Windows: the named pipe `\\.\pipe\GhostDesk-control-<user SID>`.
//!   Explicit isolated data folders get their own suffix, as Unix data roots already do.
//!   - Security descriptor: owner = this user, DACL = this user + SYSTEM only (other users can't
//!     open it)
//!   - Remote (SMB) clients are rejected; the first-instance flag detects "another window is
//!     already listening"
//!   - Server: double-checks the client process's user SID (falls back to the DACL if it can't)
//!   - Client: checks that the pipe's **owner** SID is this user (stops another user from
//!     squatting the name), and that neither the pipe object's integrity label nor the server
//!     process's integrity level is below medium (or the client's own level if lower), which
//!     stops a sandboxed process of the same user from squatting it
//!
//! Streams are returned split into boxed read and write halves, so the code above (line-based
//! JSON RPC) doesn't know about the OS.

#[cfg(unix)]
use std::path::PathBuf;

use tokio::io::{AsyncRead, AsyncWrite};

pub type Reader = Box<dyn AsyncRead + Send + Unpin>;
pub type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// Keep preview/test instances separate without changing the installed app's default names.
#[cfg(any(windows, test))]
pub fn instance_suffix(data_dir: Option<&std::ffi::OsStr>) -> String {
    use std::hash::{Hash, Hasher};
    let Some(dir) = data_dir.filter(|d| !d.is_empty()) else { return String::new() };
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    dir.hash(&mut hash);
    format!("-{:016x}", hash.finish())
}

/// A connection endpoint.
#[derive(Clone, Debug)]
pub struct Endpoint {
    #[cfg(unix)]
    path: PathBuf,
    #[cfg(windows)]
    name: String,
}

impl Endpoint {
    /// Control endpoint of the GhostDesk window.
    pub fn control() -> Endpoint {
        #[cfg(unix)]
        {
            Endpoint { path: super::paths::data_root().join("control.sock") }
        }
        #[cfg(windows)]
        {
            let who = super::win::current_user_sid().unwrap_or_else(|_| {
                // failing to get the SID practically never happens, but keep the name per-user anyway
                std::env::var("USERNAME").unwrap_or_else(|_| "user".into())
            });
            let scope = instance_suffix(std::env::var_os("GHOSTDESK_DATA_DIR").as_deref());
            Endpoint { name: format!(r"\\.\pipe\GhostDesk-control-{who}{scope}") }
        }
    }

    /// For tests and other uses: an arbitrary name (a path on Unix).
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

    /// Human-readable location (UI, error messages).
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

/// Kinds of connection failure.
#[derive(Debug)]
pub enum ConnectError {
    /// Nobody is listening (the GhostDesk window is closed): open the window and retry.
    NotRunning(std::io::Error),
    /// The endpoint exists but can't be used (another user's, no permission, …), so retrying won't
    /// help.
    ///
    /// Windows only: on Unix permission errors also open the window and retry, as before (`Other`).
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
        // permission errors (EACCES) are Other too, not Denied: open the window and retry as before.
        // The new window's bind removes a stale socket it can't connect to (e.g. one left by a GhostDesk
        // run with sudo) and recreates it.
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
    /// Starts listening. Fails if another GhostDesk is already listening.
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

    /// Accepts one connection from the same user (other uids are silently dropped).
    pub async fn accept(&mut self) -> std::io::Result<(Reader, Writer)> {
        loop {
            let (stream, _) = self.inner.accept().await?;
            // same user only (a second check on top of the socket's 0600 mode)
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
                // the brief gap while the server creates the next instance: retry shortly
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

/// Checks that the server of a connected pipe can be trusted (stops another program from
/// creating the same name first and intercepting).
///
/// 1. The pipe's owner is this user (not another user).
/// 2. The pipe's integrity level isn't low. A low-integrity sandboxed process of the same user
///    has the same owner, so 1 alone can't stop it.
///    - The pipe object's integrity label: objects created by a low-integrity process carry its
///      level, and the creator can't raise or remove it. Emptying its own process DACL to block
///      token queries doesn't hide this.
///    - The integrity level of the server process's token (checked as well when readable, and
///      used to name the program in the error message).
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

/// Minimum integrity level required of the server (pipe): this process's level, capped at
/// medium.
///
/// A `ghostdesk --mcp` running elevated (high) must still be able to reach a GhostDesk window at
/// medium; what has to be stopped is a below-medium (sandboxed) process squatting the name.
#[cfg(windows)]
fn required_integrity(mine: u32) -> u32 {
    mine.min(super::win::INTEGRITY_MEDIUM)
}

/// Integrity verdict on the server. Returns a reason (part of a sentence) when rejecting.
///
/// - `label`: the pipe object's integrity label (no label = medium). Rejected if below the
///   required level; this is the deciding check.
/// - `process`: the server process token's integrity level. Rejected if below the required
///   level. Without access (ERROR_ACCESS_DENIED) it's either a higher-privileged process or one
///   that locked down its own DACL; either way the pipe label already decided, so it's allowed.
///   Any other error (process already gone, …) is rejected.
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

/// If a pipe with the same name is already open and belongs to an untrusted program, the
/// reason.
///
/// Connects briefly and checks with `check_server` (if it's a GhostDesk window, that's just one
/// empty connection that ends right away). If it can't connect (a higher-privileged GhostDesk
/// window, no free instance, …), no verdict is made.
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
        // SAFETY: sa and the security descriptor inside it outlive the call
        unsafe { opts.create_with_security_attributes_raw(name, &mut sa as *mut _ as *mut core::ffi::c_void) }
    }

    /// Starts listening. Fails if another GhostDesk is already listening.
    pub async fn bind(ep: &Endpoint) -> anyhow::Result<Listener> {
        let sd = super::win::SecurityDescriptor::owner_only()?;
        let next = match Self::create(&ep.name, &sd, true) {
            Ok(s) => s,
            // ACCESS_DENIED if we're not the first instance: another GhostDesk window (or something with the
            // same name) exists. PIPE_BUSY if the pipe is at its instance limit: GhostDesk windows don't
            // limit instances, so it belongs to another program
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

    /// Accepts one connection from the same user.
    pub async fn accept(&mut self) -> std::io::Result<(Reader, Writer)> {
        use std::os::windows::io::AsRawHandle;
        loop {
            if let Err(e) = self.next.connect().await {
                // swap in a fresh instance so a broken one isn't retried forever, and report the error (a client
                // that disconnects right after connecting (ERROR_NO_DATA) counts as connected and never gets
                // here)
                if let Ok(fresh) = Self::create(&self.name, &self.sd, false) {
                    self.next = fresh;
                }
                return Err(e);
            }
            // create the next instance right away for the next client (during any gap clients retry on
            // PIPE_BUSY)
            let fresh = Self::create(&self.name, &self.sd, false)?;
            let conn = std::mem::replace(&mut self.next, fresh);
            // check the client process's user: the DACL already blocks others, this is a second check
            // (falls back to the DACL if it can't tell)
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

    #[test]
    fn isolated_data_instances_have_separate_names() {
        use std::ffi::OsStr;
        assert_eq!(instance_suffix(None), "");
        assert_eq!(instance_suffix(Some(OsStr::new(""))), "");
        let first = instance_suffix(Some(OsStr::new(r"C:\qa\first")));
        assert!(!first.is_empty());
        assert_eq!(first, instance_suffix(Some(OsStr::new(r"C:\qa\first"))));
        assert_ne!(first, instance_suffix(Some(OsStr::new(r"C:\qa\second"))));
    }

    #[tokio::test]
    async fn roundtrip_and_single_instance() {
        let ep = Endpoint::custom(&format!("ghostdesk-ipc-test-{}", std::process::id()));
        #[cfg(unix)]
        let _ = std::fs::remove_file(ep.path());
        // nobody listening → NotRunning
        assert!(matches!(connect(&ep).await, Err(ConnectError::NotRunning(_))));
        let mut l = Listener::bind(&ep).await.unwrap();
        // a second bind is refused (the pipe was opened by this same process, so it's "another GhostDesk
        // window", not "another program")
        let dup = Listener::bind(&ep).await.err().expect("두 번째 bind 는 실패해야 함");
        assert!(format!("{dup:#}").contains("다른 GhostDesk 창"), "{dup:#}");
        let server = tokio::spawn(async move {
            let mut served = 0;
            while served < 2 {
                let (r, mut w) = l.accept().await.unwrap();
                let mut lines = BufReader::new(r).lines();
                // Windows: skip the empty connection the second bind made briefly to check
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
        // equal or higher is allowed, lower is rejected
        assert!(server_integrity_verdict(M, Ok(M), M).is_ok());
        assert!(server_integrity_verdict(M, Ok(INTEGRITY_HIGH), M).is_ok());
        let low = server_integrity_verdict(M, Ok(INTEGRITY_LOW), M).unwrap_err();
        assert!(low.contains("무결성 수준(낮음)") && low.contains("보통"), "{low}");
        assert!(server_integrity_verdict(M, Ok(0), M).is_err());
        // an elevated (high) client still connects to a medium window (only below medium is rejected)
        assert!(server_integrity_verdict(M, Ok(M), INTEGRITY_HIGH).is_ok());
        assert!(server_integrity_verdict(M, Ok(INTEGRITY_LOW), INTEGRITY_HIGH).is_err());
        // low-integrity clients are allowed down to their own level
        assert!(server_integrity_verdict(INTEGRITY_LOW, Ok(INTEGRITY_LOW), INTEGRITY_LOW).is_ok());
        // a low pipe label is rejected whatever the process token says (even if the token is hidden)
        let denied = || std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED);
        let lbl = server_integrity_verdict(INTEGRITY_LOW, Err(denied()), M).unwrap_err();
        assert!(lbl.contains("파이프 무결성 수준(낮음)"), "{lbl}");
        assert!(server_integrity_verdict(INTEGRITY_LOW, Ok(INTEGRITY_HIGH), INTEGRITY_HIGH).is_err());
        // with a label at medium or above and no access to the token (higher privilege) it's allowed;
        // other errors (process gone, …) are rejected
        assert!(server_integrity_verdict(M, Err(denied()), M).is_ok());
        let gone = std::io::Error::from_raw_os_error(87); // ERROR_INVALID_PARAMETER
        assert!(server_integrity_verdict(M, Err(gone), M).is_err());
    }

    /// Objects created by this process have no label (treated as medium) or at least this process's
    /// level, and an explicitly set low label reads back as is.
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
        // a pipe with an explicit low integrity label (anyone can set a label below their own)
        let low = Endpoint::custom(&format!("ghostdesk-label-low-{}", std::process::id())).name;
        let sid = win::current_user_sid().unwrap();
        let lsd = win::SecurityDescriptor::from_sddl(&format!("O:{sid}D:P(A;;GA;;;{sid})S:(ML;;NW;;;LW)")).unwrap();
        let q = Listener::create(&low, &lsd, true).unwrap();
        assert_eq!(win::object_integrity_rid(q.as_raw_handle() as _).unwrap(), INTEGRITY_LOW);
        // connecting to that pipe as a client is rejected by check_server (when running at medium or
        // above)
        if mine >= INTEGRITY_MEDIUM {
            let c = tokio::net::windows::named_pipe::ClientOptions::new().open(&low).unwrap();
            match check_server(c.as_raw_handle() as _) {
                Err(ConnectError::Denied(m)) => assert!(m.contains("파이프 무결성 수준(낮음)"), "{m}"),
                other => panic!("낮은 레이블 파이프를 받아들임: {:?}", other.err()),
            }
        }
    }

    /// Manual check: with GHOSTDESK_TEST_LOWIL_PIPE set to the full name of a pipe pre-created by a
    /// low-integrity (Low IL) process (two free instances), both the client connection and the
    /// window's bind flag it as an intercepting program.
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

    /// A pipe opened by this process passes the owner and integrity checks.
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
        // wait for the server to create the next instance, then check bind's probe (a brief connect)
        // passes too
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
