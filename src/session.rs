//! Separate macOS login session (dedicated agent account) backend: an "RDP-like" isolated screen.
//!
//! macOS allows only one GUI session per user, so the user creates a dedicated standard account
//! (e.g. `ghostdesk-agent`), logs into it once with fast user switching, and switches back. The
//! agent session stays alive in the background. Inside it, a LaunchAgent runs the helper
//! (`ghostdesk --session-helper --allow-uid <user uid>`), which exposes the agent session's
//! **entire screen** over a Unix domain socket (`/Users/Shared/GhostDesk/session-<account>.sock`).
//! GhostDesk in the user's session is just a client connecting through [`SessionDesk`]; it does
//! nothing to the user's screen, cursor or keyboard.
//!
//! Layout:
//! - [`SessionPlatform`]: capture/input platform layer (real = ScreenCaptureKit + global
//!   CGEventPost, tests = synthetic framebuffer + input-recording mock)
//! - Server: [`spawn_server`] / [`run_helper`]: JSON-lines RPC, client uid authenticated with
//!   `getpeereid()`
//! - Client: [`SessionDesk`]: backend contract methods with the same names/signatures as
//!   BrowserWorkspace
//! - Setup: [`launch_agent_plist`], [`SETUP_GUIDE_KO`]
//!
//! Safeguards (so the helper never drives the user's own screen):
//! 1. Refuses if the helper's uid is in the `--allow-uid` list (= it's running in the user's own
//!    session).
//! 2. Refuses if the helper account is an administrator (admin group); a dedicated standard
//!    account is assumed (`--allow-admin-account` lifts this).
//! 3. Refuses to run as root.
//! 4. Holds off capture and input while the helper's session is on the physical display (console
//!    session), since a person may be looking at it (`--allow-on-console` lifts this).
//!
//! RPC (one JSON per line, `id` on every request):
//! - `hello{version,max_long_edge}` → `{version,label,display,frame:[w,h],on_console}`
//! - `screenshot{max_long_edge,format:"png"|"jpeg",quality}` → `{data(base64),format,w,h,cursor:[x,y]}`
//! - `zoom{region:[fx0,fy0,fx1,fy1](0-1 fractions),out_w,out_h}` → `{data,w,h}`
//! - `pointer{action:"move"|"down"|"up",button(0=left,1=right,2=middle),x,y,fw,fh,count,mods}`
//!   (x,y are pixel coordinates in an fw×fh frame; the helper converts them to display points)
//! - `scroll{x?,y?,fw,fh,dx,dy,mods}`, `key{combo,repeat}`, `hold_key{combo,secs}`, `type{text}`,
//!   `cursor{fw,fh}`, `ping`
#![allow(dead_code)]

use std::io;
#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use image::{Rgb, RgbImage};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::browser::{FrameSink, MouseButton};
use crate::desk::ViewInfo;
use crate::platform::ipc::{Reader, Writer};

/// RPC protocol version (bump on incompatible changes).
pub const PROTO_VERSION: u64 = 1;
/// Socket directory (accessible to all users, sticky 1777).
#[cfg(unix)]
pub const SOCKET_DIR: &str = "/Users/Shared/GhostDesk";
/// LaunchAgent label (~/Library/LaunchAgents/<label>.plist in the agent account).
#[cfg(unix)]
pub const LAUNCH_AGENT_LABEL: &str = "com.ghostdesk.session-helper";
/// Default long edge of the agent coordinate space (screenshots).
pub const DEFAULT_MAX_LONG_EDGE: u32 = 1280;
const MAX_REQUEST_BYTES: u64 = 8 << 20;
const MAX_RESPONSE_BYTES: u64 = 256 << 20;
/// Maximum characters per type request (long text is split so the live view doesn't stall).
const TYPE_CHUNK_CHARS: usize = 400;

/// Account name → socket path.
#[cfg(unix)]
pub fn socket_path_for(username: &str) -> PathBuf {
    Path::new(SOCKET_DIR).join(format!("session-{username}.sock"))
}

// ---- display coordinates ----

/// Main display of the helper session (in points, global coordinates).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub origin_x: f64,
    pub origin_y: f64,
    pub width_pt: f64,
    pub height_pt: f64,
    /// Points → pixels scale (2.0 on Retina).
    pub scale: f64,
}

/// Picks the output pixel size so the long edge is at most max_long_edge (keeping the aspect
/// ratio). 0 = native pixels.
///
/// Same formula as `macos::capture::fit_size`; it must match the size the helper actually
/// captures.
pub fn fit_size(w_pt: f64, h_pt: f64, scale: f64, max_long_edge: u32) -> (u32, u32) {
    let (fw, fh) = ((w_pt * scale).max(1.0), (h_pt * scale).max(1.0));
    let k = if max_long_edge == 0 { 1.0 } else { (max_long_edge as f64 / fw.max(fh)).min(1.0) };
    (((fw * k).round() as u32).max(1), ((fh * k).round() as u32).max(1))
}

impl DisplayInfo {
    /// Native pixel size.
    pub fn pixel_size(&self) -> (u32, u32) {
        fit_size(self.width_pt, self.height_pt, self.scale, 0)
    }
    /// Frame size when captured at max_long_edge.
    pub fn frame_size(&self, max_long_edge: u32) -> (u32, u32) {
        fit_size(self.width_pt, self.height_pt, self.scale, max_long_edge)
    }
    /// Pixel coordinates in an fw×fh frame → global point coordinates (clamped to the display).
    pub fn to_points(&self, x: f64, y: f64, fw: u32, fh: u32) -> (f64, f64) {
        let fw = fw.max(1) as f64;
        let fh = fh.max(1) as f64;
        let px = (x * self.width_pt / fw).clamp(0.0, (self.width_pt - 1.0).max(0.0));
        let py = (y * self.height_pt / fh).clamp(0.0, (self.height_pt - 1.0).max(0.0));
        (self.origin_x + px, self.origin_y + py)
    }
    /// Global point coordinates → pixel coordinates in an fw×fh frame.
    pub fn to_frame(&self, x: f64, y: f64, fw: u32, fh: u32) -> (f64, f64) {
        let w = self.width_pt.max(1.0);
        let h = self.height_pt.max(1.0);
        ((x - self.origin_x) * fw as f64 / w, (y - self.origin_y) * fh as f64 / h)
    }
}

// ---- platform layer ----

/// Capture and input used by the helper. Every method blocks (the server calls them inside
/// spawn_blocking).
///
/// Coordinates are global points.
pub trait SessionPlatform: Send + Sync + 'static {
    fn name(&self) -> String;
    fn display(&self) -> Result<DisplayInfo>;
    /// Captures the main display (long edge capped, 0 = native pixels).
    fn capture(&self, max_long_edge: u32) -> Result<RgbImage>;
    /// Moves the cursor. pressed = the button being held (sent as drag events).
    fn mouse_move(&self, x: f64, y: f64, pressed: Option<u8>, mods: i64) -> Result<()>;
    fn mouse_button(&self, x: f64, y: f64, button: u8, down: bool, count: i64, mods: i64) -> Result<()>;
    /// Scrolls in pixels (points); dy > 0 = down.
    fn scroll(&self, dx: f64, dy: f64, mods: i64) -> Result<()>;
    /// Presses an xdotool-style key combo for `hold`.
    fn key(&self, combo: &str, hold: Duration, check: &dyn Fn() -> Result<()>) -> Result<()>;
    /// Unicode text (Korean included; the server turns newlines and tabs into keys).
    fn type_unicode(&self, text: &str, check: &dyn Fn() -> Result<()>) -> Result<()>;
    /// Inserts a text request; platforms may keep line breaks in the same synchronous path.
    fn type_text(&self, text: &str, check: &dyn Fn() -> Result<()>) -> Result<()> {
        type_text_segmented(self, text, check)
    }
    /// A screenshot can acknowledge only the input generation it started with.
    fn input_epoch(&self) -> u64 { 0 }
    fn observe_input(&self, _epoch: u64) {}
    fn supports_app_control(&self) -> bool { false }
    fn app_control(&self, _input: &Value, _state: &mut AppControlState, _frame: (u32, u32), _check: &dyn Fn() -> Result<()>) -> Result<String> {
        bail!("이 세션 헬퍼는 앱 요소 조작을 지원하지 않습니다");
    }
    /// Current cursor position (global points).
    fn cursor(&self) -> Result<(f64, f64)>;
    /// Whether this session is currently on the physical display (console session).
    fn on_console(&self) -> bool;
}

// ---- user / session info (libc) ----

#[cfg(unix)]
pub fn getuid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(unix)]
fn passwd_by_uid(uid: u32) -> Option<(String, u32)> {
    let mut buf = vec![0 as libc::c_char; 16384];
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    if r != 0 || out.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(pw.pw_name) }.to_string_lossy().into_owned();
    Some((name, pw.pw_gid))
}

/// Account name → uid.
#[cfg(unix)]
pub fn uid_of(name: &str) -> Option<u32> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut buf = vec![0 as libc::c_char; 16384];
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe { libc::getpwnam_r(cname.as_ptr(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    if r != 0 || out.is_null() {
        return None;
    }
    Some(pw.pw_uid)
}

#[cfg(unix)]
pub fn username_of(uid: u32) -> Option<String> {
    passwd_by_uid(uid).map(|(n, _)| n)
}

#[cfg(unix)]
fn group_gid(name: &str) -> Option<u32> {
    let cname = std::ffi::CString::new(name).ok()?;
    let g = unsafe { libc::getgrnam(cname.as_ptr()) };
    if g.is_null() {
        None
    } else {
        Some(unsafe { (*g).gr_gid })
    }
}

/// Whether the uid is in the admin group (true on lookup failure, to be safe).
#[cfg(unix)]
pub fn is_admin(uid: u32) -> bool {
    let Some((name, base_gid)) = passwd_by_uid(uid) else { return true };
    let admin_gid = group_gid("admin").unwrap_or(80);
    if base_gid == admin_gid {
        return true;
    }
    let Ok(cname) = std::ffi::CString::new(name) else { return true };
    let mut n: libc::c_int = 256;
    let mut groups = vec![0 as libc::c_int; n as usize];
    #[cfg(target_vendor = "apple")]
    let r = unsafe { libc::getgrouplist(cname.as_ptr(), base_gid as libc::c_int, groups.as_mut_ptr(), &mut n) };
    #[cfg(not(target_vendor = "apple"))]
    let r = unsafe {
        libc::getgrouplist(cname.as_ptr(), base_gid as _, groups.as_mut_ptr() as *mut _, &mut n)
    };
    if r < 0 {
        return true;
    }
    groups[..(n.max(0) as usize).min(groups.len())].iter().any(|g| *g as u32 == admin_gid)
}

/// uid of the console user currently on the physical display (owner of /dev/console). 0 at the
/// login window.
#[cfg(unix)]
pub fn console_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/dev/console").ok().map(|m| m.uid())
}

/// uid of the socket peer (getpeereid).
#[cfg(unix)]
pub fn peer_uid(s: &UnixStream) -> io::Result<u32> {
    #[cfg(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        use std::os::fd::AsRawFd;
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        let r = unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid)
    }
    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    {
        Ok(s.peer_cred()?.uid())
    }
}

/// Reason the helper must refuse to run (None = OK to run). A pure function, checked by the
/// self-test.
#[cfg(unix)]
pub fn refusal_reason(me: u32, allow_uids: &[u32], me_is_admin: bool, allow_admin: bool) -> Option<String> {
    if allow_uids.is_empty() {
        return Some("허용할 클라이언트 uid 가 없습니다. `--allow-uid <사용자 uid>` 를 지정하세요 (사용자 계정에서 `id -u` 로 확인).".into());
    }
    if me == 0 {
        return Some("root 로 실행할 수 없습니다. 헬퍼는 전용 에이전트 계정의 LaunchAgent(Aqua 세션)로만 실행하세요.".into());
    }
    if allow_uids.contains(&me) {
        return Some(format!(
            "헬퍼가 허용된 클라이언트와 같은 사용자(uid {me})로 실행되었습니다. 이 세션은 사용자 본인의 화면이므로 \
             조작을 거부합니다. 헬퍼는 전용 에이전트 계정(예: ghostdesk-agent)의 로그인 세션 안에서만 실행하세요."
        ));
    }
    if me_is_admin && !allow_admin {
        return Some(format!(
            "헬퍼 계정(uid {me})이 관리자 계정입니다. 사용자 본인 계정일 가능성이 높아 거부합니다. \
             전용 '표준' 계정을 만들어 그 세션에서 실행하세요. (정말 전용 계정이라면 --allow-admin-account)"
        ));
    }
    None
}

// ---- line I/O ----

async fn read_line_limited<R: AsyncBufRead + Unpin>(rd: &mut R, limit: u64) -> io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    let n = (&mut *rd).take(limit).read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        if n as u64 >= limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "메시지가 너무 깁니다"));
        }
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "메시지 도중 연결이 끊겼습니다"));
    }
    buf.pop();
    Ok(Some(buf))
}

async fn write_line<W: AsyncWrite + Unpin>(wr: &mut W, v: &Value) -> io::Result<()> {
    let mut b = serde_json::to_vec(v).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    b.push(b'\n');
    wr.write_all(&b).await?;
    wr.flush().await
}

fn encode_png(img: &RgbImage) -> Result<Vec<u8>> {
    let mut out = io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

fn encode_jpeg(img: &RgbImage, quality: u8) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(20, 95));
    enc.encode_image(img)?;
    Ok(out)
}

// ---- server ----

pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub struct ServerConfig {
    /// Client uids allowed to connect.
    pub allow_uids: Vec<u32>,
    /// Allow capture and input even while this session is on the console (physical display).
    pub allow_on_console: bool,
    /// Name reported in hello (usually the agent account name).
    pub label: String,
    pub log: LogFn,
}

struct Server {
    platform: Arc<dyn SessionPlatform>,
    cfg: ServerConfig,
    /// Input runs one at a time, even across multiple connections.
    input_lock: tokio::sync::Mutex<()>,
}

/// A running server. stop() closes every connection (after releasing pressed buttons) and ends
/// it; simply dropping the handle also stops the server.
pub struct ServerHandle {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl ServerHandle {
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

/// Per-connection state (pressed buttons are released on disconnect).
#[derive(Default)]
pub struct AppControlState {
    #[cfg(windows)]
    pub(crate) windows: crate::winapp::child::AppControl,
    #[cfg(test)]
    mock_calls: usize,
}

#[derive(Default)]
struct ConnState {
    pressed: Vec<u8>,
    last_pt: Option<(f64, f64)>,
    app_control: AppControlState,
}

/// Listener the helper accepts connections on. Unix = socket (client uid checked), Windows =
/// named pipe (`platform::ipc` only lets the same user in).
#[cfg(unix)]
pub type HelperListener = UnixListener;
#[cfg(windows)]
pub type HelperListener = crate::platform::ipc::Listener;

/// Accepts one connection: (read half, write half, client uid; None on Windows).
#[cfg(unix)]
async fn accept_conn(l: &mut HelperListener) -> io::Result<(Reader, Writer, Option<u32>)> {
    let (stream, _) = l.accept().await?;
    let uid = peer_uid(&stream)?;
    let (r, w) = stream.into_split();
    Ok((Box::new(r), Box::new(w), Some(uid)))
}
#[cfg(windows)]
async fn accept_conn(l: &mut HelperListener) -> io::Result<(Reader, Writer, Option<u32>)> {
    let (r, w) = l.accept().await?;
    Ok((r, w, None))
}

/// Starts the server on an already bound listener.
pub fn spawn_server(mut listener: HelperListener, platform: Arc<dyn SessionPlatform>, cfg: ServerConfig) -> ServerHandle {
    let (stop_tx, stop_rx) = watch::channel(false);
    let srv = Arc::new(Server { platform, cfg, input_lock: tokio::sync::Mutex::new(()) });
    let task = tokio::spawn(async move {
        let mut set = JoinSet::new();
        let mut stop = stop_rx.clone();
        loop {
            tokio::select! {
                r = accept_conn(&mut listener) => match r {
                    Ok((rd, wr, uid)) => {
                        let srv = srv.clone();
                        let rx = stop_rx.clone();
                        set.spawn(async move { srv.handle(rd, wr, uid, rx).await });
                    }
                    Err(e) => {
                        (srv.cfg.log)(&format!("accept 실패: {e}"));
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                },
                _ = stop.changed() => break,
                Some(_) = set.join_next(), if !set.is_empty() => {}
            }
        }
        while set.join_next().await.is_some() {}
    });
    ServerHandle { stop: stop_tx, task }
}

/// Serves connections that were already authenticated some other way (host mode: the pairing token over an
/// end-to-end encrypted link). No uid check, and the console gate is lifted because the screen being served is
/// this computer's own.
pub struct HostServer {
    srv: Arc<Server>,
    stop: watch::Sender<bool>,
}

impl HostServer {
    pub fn new(platform: Arc<dyn SessionPlatform>, label: String, log: LogFn) -> Self {
        let cfg = ServerConfig { allow_uids: vec![], allow_on_console: true, label, log };
        let (stop, _) = watch::channel(false);
        HostServer { srv: Arc::new(Server { platform, cfg, input_lock: tokio::sync::Mutex::new(()) }), stop }
    }

    /// Runs one connection until it ends or `stop_all` is called.
    pub async fn serve(&self, rd: Reader, wr: Writer) {
        self.srv.clone().handle(rd, wr, None, self.stop.subscribe()).await
    }

    /// Ends every running connection.
    pub fn stop_all(&self) {
        let _ = self.stop.send(true);
    }
}

/// The screen and input of this computer's own session, for host mode. Errors say what is missing.
pub fn host_platform() -> Result<Arc<dyn SessionPlatform>> {
    #[cfg(target_os = "macos")]
    {
        let s = crate::macos::tcc_status();
        let mut missing = Vec::new();
        if !s.screen_capture {
            missing.push("화면 기록");
        }
        if !s.accessibility || !s.post_event {
            missing.push("손쉬운 사용");
        }
        if !missing.is_empty() {
            bail!("호스트에 필요한 권한이 없습니다: {} (시스템 설정 > 개인정보 보호 및 보안 에서 GhostDesk 를 켜세요)", missing.join(", "));
        }
        Ok(Arc::new(mac::MacPlatform::new()))
    }
    #[cfg(windows)]
    {
        Ok(crate::winapp::child::host_platform())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        bail!("호스트 모드는 macOS 와 Windows 에서만 쓸 수 있습니다")
    }
}

fn num(v: &Value, k: &str) -> Result<f64> {
    v[k].as_f64().ok_or_else(|| anyhow!("'{k}' 숫자가 필요합니다"))
}
fn num_or(v: &Value, k: &str, d: f64) -> f64 {
    v[k].as_f64().unwrap_or(d)
}
fn uint_or(v: &Value, k: &str, d: u64) -> u64 {
    v[k].as_u64().unwrap_or(d)
}

/// Splits text into Unicode chunks and special keys (Return/Tab).
#[derive(Debug, PartialEq)]
enum Seg {
    Text(String),
    Key(&'static str),
}

pub(crate) fn type_text_segmented<P: SessionPlatform + ?Sized>(platform: &P, text: &str, check: &dyn Fn() -> Result<()>) -> Result<()> {
    for seg in split_text(text) {
        check()?;
        match seg {
            Seg::Text(t) => platform.type_unicode(&t, check)?,
            Seg::Key(k) => platform.key(k, Duration::ZERO, check)?,
        }
    }
    Ok(())
}

fn split_text(text: &str) -> Vec<Seg> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        let key = match c {
            '\n' => Some("Return"),
            '\r' => {
                if it.peek() == Some(&'\n') {
                    continue;
                }
                Some("Return")
            }
            '\t' => Some("Tab"),
            _ => None,
        };
        match key {
            Some(k) => {
                if !cur.is_empty() {
                    out.push(Seg::Text(std::mem::take(&mut cur)));
                }
                out.push(Seg::Key(k));
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(Seg::Text(cur));
    }
    out
}

fn console_gate(platform: &dyn SessionPlatform, allow_on_console: bool) -> Result<()> {
    if !allow_on_console && platform.on_console() {
        bail!(
            "에이전트 세션이 지금 실제 모니터에 표시되고 있습니다(콘솔 세션). 사람이 이 화면을 보고 있을 수 있어 \
             캡처·입력을 보류합니다. 빠른 사용자 전환으로 원래 계정으로 돌아가면 다시 동작합니다."
        );
    }
    Ok(())
}

/// Check throughout a hold so a console switch releases the key promptly.
pub(crate) fn hold_checked(hold: Duration, check: &dyn Fn() -> Result<()>) -> Result<()> {
    let end = Instant::now() + hold;
    loop {
        check()?;
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        std::thread::sleep(left.min(Duration::from_millis(20)));
    }
}

impl Server {
    fn log(&self, s: &str) {
        (self.cfg.log)(s)
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn SessionPlatform) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let p = self.platform.clone();
        tokio::task::spawn_blocking(move || f(&*p)).await.map_err(|e| anyhow!("작업 스레드 오류: {e}"))?
    }

    /// Holds off capture and input while this session is on the physical display.
    fn gate(&self) -> Result<()> {
        console_gate(&*self.platform, self.cfg.allow_on_console)
    }

    async fn guarded<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn SessionPlatform, &dyn Fn() -> Result<()>) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let allow = self.cfg.allow_on_console;
        self.blocking(move |p| {
            let check = || console_gate(p, allow);
            check()?;
            f(p, &check)
        }).await
    }

    async fn handle(self: Arc<Self>, rd: Reader, mut wr: Writer, uid: Option<u32>, mut stop: watch::Receiver<bool>) {
        // only Unix checks the uid allowlist (on Windows the named pipe DACL and owner check already
        // admit only the same user)
        if let Some(uid) = uid {
            if !self.cfg.allow_uids.contains(&uid) {
                self.log(&format!("인증 거부: uid {uid} 는 허용 목록 {:?} 에 없습니다", self.cfg.allow_uids));
                let _ = write_line(
                    &mut wr,
                    &json!({"id": 0, "ok": false, "error": format!(
                        "인증 거부: uid {uid} 는 이 세션 헬퍼에 접속할 수 없습니다 (허용: {:?}). 헬퍼의 --allow-uid 를 확인하세요.",
                        self.cfg.allow_uids)}),
                )
                .await;
                return;
            }
        }
        let uid = uid.map(|u| u.to_string()).unwrap_or_else(|| "같은 사용자".into());
        self.log(&format!("클라이언트 접속 (uid {uid})"));
        let mut rd = BufReader::new(rd);
        let mut cs = ConnState::default();
        loop {
            if *stop.borrow() {
                break;
            }
            let line = tokio::select! {
                r = read_line_limited(&mut rd, MAX_REQUEST_BYTES) => r,
                _ = stop.changed() => break,
            };
            let line = match line {
                Ok(Some(l)) => l,
                Ok(None) => break,
                Err(e) => {
                    self.log(&format!("요청 읽기 실패 → 연결 닫음: {e}"));
                    break;
                }
            };
            let (id, res) = match serde_json::from_slice::<Value>(&line) {
                Ok(v) => {
                    let id = v["id"].as_u64().unwrap_or(0);
                    (id, self.dispatch(&v, &mut cs).await)
                }
                Err(e) => (0, Err(anyhow!("요청 JSON 파싱 실패: {e}"))),
            };
            let out = match res {
                Ok(r) => json!({"id": id, "ok": true, "result": r}),
                Err(e) => json!({"id": id, "ok": false, "error": format!("{e:#}")}),
            };
            if let Err(e) = write_line(&mut wr, &out).await {
                self.log(&format!("응답 전송 실패 → 연결 닫음: {e}"));
                break;
            }
        }
        self.release(&mut cs).await;
        self.log(&format!("클라이언트 연결 종료 (uid {uid})"));
    }

    /// Releases any mouse buttons the connection was holding when it drops.
    async fn release(&self, cs: &mut ConnState) {
        if cs.pressed.is_empty() {
            return;
        }
        let pressed = std::mem::take(&mut cs.pressed);
        let pos = cs.last_pt;
        let _g = self.input_lock.lock().await;
        let r = self
            .blocking(move |p| {
                let (x, y) = match pos {
                    Some(v) => v,
                    None => p.cursor()?,
                };
                for b in pressed.iter().rev() {
                    p.mouse_button(x, y, *b, false, 1, 0)?;
                }
                Ok(())
            })
            .await;
        match r {
            Ok(()) => self.log("연결 종료로 눌려 있던 마우스 버튼을 놓았습니다"),
            Err(e) => self.log(&format!("마우스 버튼 해제 실패: {e:#}")),
        }
    }

    async fn dispatch(&self, v: &Value, cs: &mut ConnState) -> Result<Value> {
        let op = v["op"].as_str().unwrap_or("");
        match op {
            "hello" => {
                let ver = uint_or(v, "version", 0);
                if ver != PROTO_VERSION {
                    bail!("프로토콜 버전 불일치: 클라이언트 {ver}, 헬퍼 {PROTO_VERSION}. 양쪽 GhostDesk 를 같은 버전으로 맞추세요.");
                }
                let max = uint_or(v, "max_long_edge", DEFAULT_MAX_LONG_EDGE as u64) as u32;
                let d = self.blocking(|p| p.display()).await?;
                let (fw, fh) = d.frame_size(max);
                Ok(json!({
                    "version": PROTO_VERSION,
                    "label": self.cfg.label,
                    "platform": self.platform.name(),
                    "display": d,
                    "frame": [fw, fh],
                    "on_console": self.platform.on_console(),
                    "allow_on_console": self.cfg.allow_on_console,
                    "server_pid": std::process::id(),
                    "server_version": env!("CARGO_PKG_VERSION"),
                    "capabilities": {"app_control": self.platform.supports_app_control()},
                }))
            }
            "ping" => Ok(json!({"pong": true, "on_console": self.platform.on_console()})),
            "screenshot" => {
                self.gate()?;
                let max = uint_or(v, "max_long_edge", DEFAULT_MAX_LONG_EDGE as u64) as u32;
                let jpeg = v["format"].as_str() == Some("jpeg");
                let quality = uint_or(v, "quality", 70) as u8;
                let observe_input = v["observe_input"] == true;
                let (bytes, w, h, cur) = self
                    .guarded(move |p, check| {
                        let epoch = p.input_epoch();
                        let img = p.capture(max)?;
                        let (w, h) = img.dimensions();
                        let bytes = if jpeg { encode_jpeg(&img, quality)? } else { encode_png(&img)? };
                        let cur = match (p.cursor(), p.display()) {
                            (Ok((cx, cy)), Ok(d)) => Some(d.to_frame(cx, cy, w, h)),
                            _ => None,
                        };
                        check()?;
                        if observe_input { p.observe_input(epoch); }
                        Ok((bytes, w, h, cur))
                    })
                    .await?;
                Ok(json!({
                    "data": B64.encode(&bytes),
                    "format": if jpeg { "jpeg" } else { "png" },
                    "w": w, "h": h,
                    "cursor": cur.map(|(x, y)| json!([x, y])),
                    "on_console": self.platform.on_console(),
                }))
            }
            "zoom" => {
                self.gate()?;
                let r = v["region"].as_array().ok_or_else(|| anyhow!("'region' 배열이 필요합니다"))?;
                if r.len() != 4 {
                    bail!("'region' 은 [x0,y0,x1,y1] (0~1 비율) 이어야 합니다");
                }
                let f: Vec<f64> = r.iter().map(|x| x.as_f64().unwrap_or(0.0).clamp(0.0, 1.0)).collect();
                let out_w = (uint_or(v, "out_w", 1280) as u32).clamp(1, 8192);
                let out_h = (uint_or(v, "out_h", 800) as u32).clamp(1, 8192);
                let (bytes, w, h) = self
                    .guarded(move |p, check| {
                        let img = p.capture(0)?;
                        let (iw, ih) = img.dimensions();
                        let (x0, x1) = (f[0].min(f[2]), f[0].max(f[2]));
                        let (y0, y1) = (f[1].min(f[3]), f[1].max(f[3]));
                        let px0 = ((x0 * iw as f64).floor() as u32).min(iw.saturating_sub(1));
                        let py0 = ((y0 * ih as f64).floor() as u32).min(ih.saturating_sub(1));
                        let px1 = ((x1 * iw as f64).ceil() as u32).clamp(px0 + 1, iw);
                        let py1 = ((y1 * ih as f64).ceil() as u32).clamp(py0 + 1, ih);
                        let crop = image::imageops::crop_imm(&img, px0, py0, px1 - px0, py1 - py0).to_image();
                        let out = if crop.dimensions() != (out_w, out_h) {
                            image::imageops::resize(&crop, out_w, out_h, image::imageops::FilterType::CatmullRom)
                        } else {
                            crop
                        };
                        let (w, h) = out.dimensions();
                        let bytes = encode_png(&out)?;
                        check()?;
                        Ok((bytes, w, h))
                    })
                    .await?;
                Ok(json!({"data": B64.encode(&bytes), "w": w, "h": h}))
            }
            "pointer" => {
                let action = v["action"].as_str().unwrap_or("move").to_string();
                let button = (uint_or(v, "button", 0) as u8).min(2);
                // releasing a button this connection pressed is allowed even while held off for the console (so a
                // button isn't left stuck if someone switches to the agent session right after a down)
                let releasing = action == "up" && cs.pressed.contains(&button);
                if !releasing {
                    self.gate()?;
                }
                let fw = uint_or(v, "fw", 0) as u32;
                let fh = uint_or(v, "fh", 0) as u32;
                let count = v["count"].as_i64().unwrap_or(1).clamp(1, 5);
                let mods = v["mods"].as_i64().unwrap_or(0);
                let pos = match (v["x"].as_f64(), v["y"].as_f64()) {
                    (Some(x), Some(y)) if fw > 0 && fh > 0 => {
                        let d = self.blocking(|p| p.display()).await?;
                        Some(d.to_points(x, y, fw, fh))
                    }
                    (Some(_), Some(_)) => bail!("'fw','fh' (프레임 크기)가 필요합니다"),
                    _ => None,
                };
                let _g = self.input_lock.lock().await;
                let last = cs.last_pt;
                let (x, y) = match pos.or(last) {
                    Some(p) => p,
                    None => self.blocking(|p| p.cursor()).await?,
                };
                match action.as_str() {
                    "move" => {
                        let pressed = cs.pressed.last().copied();
                        self.guarded(move |p, _| p.mouse_move(x, y, pressed, mods)).await?;
                    }
                    "down" => {
                        if pos.is_some() && pos != last {
                            let pressed = cs.pressed.last().copied();
                            self.guarded(move |p, _| p.mouse_move(x, y, pressed, mods)).await?;
                        }
                        self.guarded(move |p, _| p.mouse_button(x, y, button, true, count, mods)).await?;
                        if !cs.pressed.contains(&button) {
                            cs.pressed.push(button);
                        }
                    }
                    "up" => {
                        let allow = self.cfg.allow_on_console || releasing;
                        self.blocking(move |p| {
                            console_gate(p, allow)?;
                            p.mouse_button(x, y, button, false, count, mods)
                        }).await?;
                        cs.pressed.retain(|b| *b != button);
                    }
                    other => bail!("알 수 없는 pointer action: {other}"),
                }
                cs.last_pt = Some((x, y));
                Ok(json!({"x": x, "y": y}))
            }
            "scroll" => {
                self.gate()?;
                let fw = uint_or(v, "fw", 0) as u32;
                let fh = uint_or(v, "fh", 0) as u32;
                if fw == 0 || fh == 0 {
                    bail!("'fw','fh' (프레임 크기)가 필요합니다");
                }
                let dx = num_or(v, "dx", 0.0);
                let dy = num_or(v, "dy", 0.0);
                let mods = v["mods"].as_i64().unwrap_or(0);
                let d = self.blocking(|p| p.display()).await?;
                // scale the scroll amount by the frame pixel → point ratio
                let (sx, sy) = (d.width_pt / fw as f64, d.height_pt / fh as f64);
                let pos = match (v["x"].as_f64(), v["y"].as_f64()) {
                    (Some(x), Some(y)) => Some(d.to_points(x, y, fw, fh)),
                    _ => None,
                };
                let _g = self.input_lock.lock().await;
                if let Some((x, y)) = pos {
                    let pressed = cs.pressed.last().copied();
                    self.guarded(move |p, _| p.mouse_move(x, y, pressed, mods)).await?;
                    cs.last_pt = Some((x, y));
                }
                self.guarded(move |p, _| p.scroll(dx * sx, dy * sy, mods)).await?;
                Ok(json!({}))
            }
            "key" => {
                self.gate()?;
                let combo = v["combo"].as_str().ok_or_else(|| anyhow!("'combo' 가 필요합니다"))?.to_string();
                let repeat = uint_or(v, "repeat", 1).clamp(1, 100) as u32;
                let _g = self.input_lock.lock().await;
                self.guarded(move |p, check| {
                    for i in 0..repeat {
                        if i > 0 {
                            std::thread::sleep(Duration::from_millis(30));
                        }
                        check()?;
                        p.key(&combo, Duration::ZERO, check)?;
                    }
                    Ok(())
                })
                .await?;
                Ok(json!({}))
            }
            "hold_key" => {
                self.gate()?;
                let combo = v["combo"].as_str().ok_or_else(|| anyhow!("'combo' 가 필요합니다"))?.to_string();
                let secs = num(v, "secs")?.clamp(0.0, 30.0);
                let _g = self.input_lock.lock().await;
                self.guarded(move |p, check| p.key(&combo, Duration::from_secs_f64(secs), check)).await?;
                Ok(json!({}))
            }
            "type" => {
                self.gate()?;
                let text = v["text"].as_str().ok_or_else(|| anyhow!("'text' 가 필요합니다"))?.to_string();
                let _g = self.input_lock.lock().await;
                self.guarded(move |p, check| p.type_text(&text, check)).await?;
                Ok(json!({}))
            }
            "app_control" => {
                self.gate()?;
                let input = v["input"].as_object().ok_or_else(|| anyhow!("input 객체가 필요합니다"))?.clone();
                let frame = (uint_or(v, "fw", 0).min(32768) as u32, uint_or(v, "fh", 0).min(32768) as u32);
                let _g = self.input_lock.lock().await;
                let mut state = std::mem::take(&mut cs.app_control);
                let (state, result) = self.guarded(move |p, check| {
                    let result = p.app_control(&Value::Object(input), &mut state, frame, check);
                    Ok((state, result))
                }).await?;
                cs.app_control = state;
                Ok(json!({"text": result?}))
            }
            "cursor" => {
                let fw = uint_or(v, "fw", 0) as u32;
                let fh = uint_or(v, "fh", 0) as u32;
                let (x, y, d) = self
                    .blocking(|p| {
                        let (x, y) = p.cursor()?;
                        Ok((x, y, p.display()?))
                    })
                    .await?;
                let f = if fw > 0 && fh > 0 { Some(d.to_frame(x, y, fw, fh)) } else { None };
                Ok(json!({"x_pt": x, "y_pt": y, "frame": f.map(|(a, b)| json!([a, b]))}))
            }
            "" => bail!("'op' 가 필요합니다"),
            other => bail!("알 수 없는 요청: {other}"),
        }
    }
}

// ---- real macOS platform (helper only) ----

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use crate::macos::capture::{display_filter, Filter};
    use crate::macos::input::global;
    use objc2_core_graphics::{CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode, CGEvent, CGMainDisplayID};

    /// The real platform, **only used inside the dedicated agent login session**: ScreenCaptureKit
    /// display capture + session-global CGEventPost.
    pub struct MacPlatform {
        filter: Mutex<Option<(Filter, Instant)>>,
    }

    impl MacPlatform {
        pub fn new() -> Self {
            MacPlatform { filter: Mutex::new(None) }
        }

        fn with_filter<R>(&self, f: impl Fn(&Filter) -> Result<R>) -> Result<R> {
            for attempt in 0..2 {
                let mut g = self.filter.lock();
                let stale = match &*g {
                    Some((_, t)) => t.elapsed() > Duration::from_secs(10),
                    None => true,
                };
                if stale || attempt > 0 {
                    *g = Some((display_filter(0)?, Instant::now()));
                }
                let r = f(&g.as_ref().unwrap().0);
                match r {
                    Ok(v) => return Ok(v),
                    Err(e) if attempt == 0 => {
                        *g = None;
                        let _ = e;
                    }
                    Err(e) => return Err(e),
                }
            }
            unreachable!()
        }
    }

    impl SessionPlatform for MacPlatform {
        fn name(&self) -> String {
            "macos".into()
        }
        /// Display info comes from the CGDisplay API only, without touching ScreenCaptureKit (so hello
        /// doesn't access screen contents or the window list while held off for the console).
        fn display(&self) -> Result<DisplayInfo> {
            let id = CGMainDisplayID();
            let b = CGDisplayBounds(id);
            let scale = match CGDisplayCopyDisplayMode(id) {
                Some(m) => {
                    let (pw, w) = (CGDisplayMode::pixel_width(Some(&m)), CGDisplayMode::width(Some(&m)));
                    if pw > 0 && w > 0 { pw as f64 / w as f64 } else { 1.0 }
                }
                None => 1.0,
            };
            if b.size.width <= 0.0 || b.size.height <= 0.0 {
                bail!("메인 디스플레이 크기를 알 수 없습니다 (GUI 세션 밖에서 실행 중일 수 있음)");
            }
            Ok(DisplayInfo {
                origin_x: b.origin.x,
                origin_y: b.origin.y,
                width_pt: b.size.width,
                height_pt: b.size.height,
                scale,
            })
        }
        fn capture(&self, max_long_edge: u32) -> Result<RgbImage> {
            self.with_filter(|f| f.capture_fit(max_long_edge))
        }
        fn mouse_move(&self, x: f64, y: f64, pressed: Option<u8>, mods: i64) -> Result<()> {
            global::post_mouse_move(x, y, pressed, mods)
        }
        fn mouse_button(&self, x: f64, y: f64, button: u8, down: bool, count: i64, mods: i64) -> Result<()> {
            global::post_mouse_button(x, y, button, down, count, mods)
        }
        fn scroll(&self, dx: f64, dy: f64, mods: i64) -> Result<()> {
            if mods == 0 {
                return global::post_scroll(dx, dy);
            }
            // modifiers + scroll (e.g. ctrl+scroll to zoom): the same event as global::post_scroll with flags
            // added
            use objc2_core_graphics::{CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGScrollEventUnit};
            let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState);
            let ev = CGEvent::new_scroll_wheel_event2(
                src.as_deref(),
                CGScrollEventUnit::Pixel,
                2,
                (-dy) as i32,
                (-dx) as i32,
                0,
            )
            .ok_or_else(|| anyhow!("스크롤 이벤트 생성 실패"))?;
            CGEvent::set_flags(Some(&ev), crate::macos::input::mods_to_flags(mods));
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
            Ok(())
        }
        fn key(&self, combo: &str, hold: Duration, check: &dyn Fn() -> Result<()>) -> Result<()> {
            global::post_combo(combo, hold, check)
        }
        fn type_unicode(&self, text: &str, check: &dyn Fn() -> Result<()>) -> Result<()> {
            global::post_unicode(text, check)
        }
        fn cursor(&self) -> Result<(f64, f64)> {
            let ev = CGEvent::new(None).ok_or_else(|| anyhow!("CGEvent 생성 실패"))?;
            let p = CGEvent::location(Some(&ev));
            Ok((p.x, p.y))
        }
        fn on_console(&self) -> bool {
            console_uid() == Some(getuid())
        }
    }
}

// ---- helper process (--session-helper) ----

#[cfg(unix)]
struct HelperArgs {
    allow_uids: Vec<u32>,
    socket: Option<PathBuf>,
    allow_on_console: bool,
    allow_admin: bool,
}

#[cfg(unix)]
fn parse_helper_args(args: &[String]) -> Result<HelperArgs> {
    let mut h = HelperArgs { allow_uids: vec![], socket: None, allow_on_console: false, allow_admin: false };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--allow-uid" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--allow-uid 뒤에 uid 가 필요합니다"))?;
                for p in v.split(',').filter(|s| !s.trim().is_empty()) {
                    h.allow_uids.push(p.trim().parse().with_context(|| format!("잘못된 uid: {p}"))?);
                }
                i += 1;
            }
            "--socket" => {
                h.socket = Some(PathBuf::from(args.get(i + 1).ok_or_else(|| anyhow!("--socket 뒤에 경로가 필요합니다"))?));
                i += 1;
            }
            "--allow-on-console" => h.allow_on_console = true,
            "--allow-admin-account" => h.allow_admin = true,
            other => bail!("알 수 없는 인자: {other}\n사용법: ghostdesk --session-helper --allow-uid <uid>[,<uid>] [--socket 경로] [--allow-on-console] [--allow-admin-account]"),
        }
        i += 1;
    }
    Ok(h)
}

/// Helper log: stderr + session-helper.log in the agent account's log folder (macOS:
/// ~/Library/Logs/GhostDesk).
pub(crate) fn helper_logger() -> LogFn {
    // if the home folder is unknown, log to stderr only, without a file (as before)
    let file = dirs::home_dir().map(|_| crate::platform::paths::logs_dir().join("session-helper.log"));
    if let Some(f) = &file {
        if let Some(d) = f.parent() {
            let _ = std::fs::create_dir_all(d);
        }
    }
    let file = Arc::new(Mutex::new(file.and_then(|p| {
        std::fs::OpenOptions::new().create(true).append(true).open(p).ok()
    })));
    Arc::new(move |s: &str| {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let line = format!("[ghostdesk-session-helper {t:.3}] {s}");
        eprintln!("{line}");
        if let Some(f) = file.lock().as_mut() {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
        }
    })
}

/// Creates the socket directory (1777), removes a stale dead socket, then binds (0666).
#[cfg(unix)]
fn bind_socket(path: &Path, log: &LogFn) -> Result<UnixListener> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        if !dir.exists() {
            std::fs::create_dir_all(dir).with_context(|| format!("소켓 디렉터리 생성 실패: {}", dir.display()))?;
        }
        if let Ok(m) = std::fs::metadata(dir) {
            if m.uid() == getuid() && m.mode() & 0o7777 != 0o1777 {
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o1777));
            } else if m.mode() & 0o1000 == 0 {
                log(&format!("경고: {} 에 sticky 비트가 없습니다 (권장 모드 1777)", dir.display()));
            }
        }
    }
    if path.exists() {
        // refuse if a live helper is there, otherwise remove the dead socket file
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            bail!("다른 세션 헬퍼가 이미 {} 에서 실행 중입니다", path.display());
        }
        std::fs::remove_file(path).with_context(|| {
            format!("오래된 소켓 파일을 지울 수 없습니다: {} (다른 사용자 소유일 수 있음)", path.display())
        })?;
    }
    let l = UnixListener::bind(path).with_context(|| format!("소켓 bind 실패: {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(l)
}

/// Helper server running inside another macOS user's (the dedicated agent account's) session.
///
/// `ghostdesk --session-helper --allow-uid <user uid>`
#[cfg(unix)]
pub async fn run_helper(args: &[String]) -> Result<()> {
    let h = parse_helper_args(args)?;
    let me = getuid();
    let user = username_of(me).ok_or_else(|| anyhow!("현재 사용자(uid {me}) 이름을 알 수 없습니다"))?;
    // ---- safety checks, before calling any screen or input API ----
    let admin = is_admin(me);
    if let Some(reason) = refusal_reason(me, &h.allow_uids, admin, h.allow_admin) {
        // on refusal, don't even create the log file (nothing is written to the user's home)
        eprintln!("[ghostdesk-session-helper] 실행 거부: {reason}");
        bail!("세션 헬퍼 실행 거부: {reason}");
    }
    let log = helper_logger();
    let console = console_uid();
    log(&format!(
        "시작: 계정 {user}(uid {me}), 허용 클라이언트 uid {:?}, 콘솔 사용자 uid {:?}{}",
        h.allow_uids,
        console,
        if console == Some(me) {
            " — 지금 이 세션이 화면에 표시 중입니다. 빠른 사용자 전환으로 원래 계정으로 돌아가면 동작을 시작합니다."
        } else {
            ""
        }
    ));
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (&h, &user, &log);
        bail!("세션 헬퍼는 macOS 전용입니다");
    }
    #[cfg(target_os = "macos")]
    {
        let s = crate::macos::tcc_status();
        let mut missing = Vec::new();
        if !s.screen_capture {
            missing.push("• 화면 기록: (에이전트 계정에서) 시스템 설정 > 개인정보 보호 및 보안 > 화면 및 시스템 오디오 기록 에서 ghostdesk 를 켜세요.");
        }
        if !s.accessibility || !s.post_event {
            missing.push("• 손쉬운 사용: (에이전트 계정에서) 시스템 설정 > 개인정보 보호 및 보안 > 손쉬운 사용 에서 ghostdesk 를 켜세요.");
        }
        if !missing.is_empty() {
            let m = format!("에이전트 세션에 필요한 권한이 없습니다:\n{}\n권한을 켠 뒤 헬퍼가 다시 시작되면(최대 30초) 적용됩니다.", missing.join("\n"));
            log(&m);
            bail!(m);
        }
        let path = h.socket.clone().unwrap_or_else(|| socket_path_for(&user));
        let listener = bind_socket(&path, &log)?;
        log(&format!("소켓 대기 중: {}", path.display()));
        let cfg = ServerConfig {
            allow_uids: h.allow_uids.clone(),
            allow_on_console: h.allow_on_console,
            label: user.clone(),
            log: log.clone(),
        };
        let handle = spawn_server(listener, Arc::new(mac::MacPlatform::new()), cfg);
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => log("SIGINT 수신 — 종료합니다"),
            _ = term.recv() => log("SIGTERM 수신 — 종료합니다"),
        }
        handle.stop().await;
        let _ = std::fs::remove_file(&path);
        log("종료됨");
        Ok(())
    }
}

#[cfg(windows)]
pub use crate::winapp::child::{run_helper, selftest, SETUP_GUIDE_KO};

// ---- client ----

#[derive(Clone, Debug)]
pub struct SessionOptions {
    /// Long edge of the agent coordinate space (screenshots).
    pub max_long_edge: u32,
    /// Live view polling fps (0 = off).
    pub live_fps: f64,
    /// Long edge of live view JPEGs.
    pub live_max_long_edge: u32,
    /// How long to keep trying to reconnect after the connection drops.
    pub reconnect_window: Duration,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            max_long_edge: DEFAULT_MAX_LONG_EDGE,
            live_fps: crate::rfb::live_fps_limit() as f64,
            live_max_long_edge: 960,
            reconnect_window: Duration::from_secs(5),
        }
    }
}

struct Conn {
    rd: BufReader<Reader>,
    wr: Writer,
}

/// Helper endpoint. Unix = socket path, Windows = named pipe.
#[cfg(unix)]
pub type Target = PathBuf;
#[cfg(windows)]
pub type Target = crate::platform::ipc::Endpoint;

#[derive(Default)]
struct ClientState {
    size: (u32, u32),
    mouse: (f64, f64),
    buttons: u8,
    notes: Vec<String>,
    display: Option<DisplayInfo>,
    last_agent_shot: Option<Instant>,
    on_console: bool,
    server_label: String,
    server_version: String,
    app_control_supported: bool,
}

/// Backend driving the entire screen of the dedicated agent login session (session helper client).
pub struct SessionDesk {
    path: Target,
    label: String,
    expect_peer_uid: Option<u32>,
    opts: SessionOptions,
    conn: tokio::sync::Mutex<Option<Conn>>,
    st: Mutex<ClientState>,
    closed: AtomicBool,
    sink: FrameSink,
    next_id: AtomicU64,
    poller: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Whether the current connection passed the input leak check (canary move); reset to false on
    /// every (re)connect.
    canary_ok: AtomicBool,
    /// Reads the cursor position in the user's (client) session (swapped out by the self-test).
    local_cursor: Mutex<LocalCursorFn>,
    /// This agent's turn on the one session screen (turns.rs); released when the screen closes so
    /// the next agent in line gets it.
    turn: Mutex<Option<crate::turns::Turn>>,
    guard: Mutex<Option<Box<dyn std::any::Any + Send>>>,
}

/// Reads the current cursor position in the user's (client) session, for input leak detection.
type LocalCursorFn = Arc<dyn Fn() -> Option<(f64, f64)> + Send + Sync>;

/// Cursor position in the user's session (read only).
fn real_local_cursor() -> Option<(f64, f64)> {
    #[cfg(target_os = "macos")]
    {
        let ev = objc2_core_graphics::CGEvent::new(None)?;
        let p = objc2_core_graphics::CGEvent::location(Some(&ev));
        Some((p.x, p.y))
    }
    #[cfg(windows)]
    {
        let mut p = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
        (unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p) } != 0).then(|| (p.x as f64, p.y as f64))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None
    }
}

/// Whether pointer input sent to the helper looks like it **leaked into the user's screen**: the
/// user session's cursor moved during the request and now sits almost exactly on the point the
/// helper moved to.
///
/// Catches macOS behavior where a background session's global CGEventPost reaches the console
/// session, should it exist.
fn leak_suspected(before: (f64, f64), after: (f64, f64), target: (f64, f64)) -> bool {
    let moved = (before.0 - after.0).abs() > 0.5 || (before.1 - after.1).abs() > 0.5;
    let hit = (after.0 - target.0).abs() <= 0.75 && (after.1 - target.1).abs() <= 0.75;
    moved && hit
}

fn button_code(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
    }
}

/// "ghostdesk-agent" or a socket path → (path, expected helper uid, display name).
#[cfg(unix)]
pub fn resolve_target(username_or_socket: &str) -> Result<(PathBuf, Option<u32>, String)> {
    let s = username_or_socket.trim();
    if s.is_empty() {
        bail!("에이전트 계정 이름 또는 소켓 경로가 비어 있습니다");
    }
    if s.contains('/') {
        let p = PathBuf::from(s);
        let label = p.file_stem().map(|x| x.to_string_lossy().into_owned()).unwrap_or_else(|| s.to_string());
        return Ok((p, None, label));
    }
    let uid = uid_of(s).ok_or_else(|| anyhow!("macOS 사용자 '{s}' 를 찾을 수 없습니다. 전용 에이전트 계정을 먼저 만드세요."))?;
    if uid == getuid() {
        bail!("자기 자신의 계정('{s}')에는 연결할 수 없습니다. 별도의 전용 에이전트 계정을 지정하세요.");
    }
    Ok((socket_path_for(s), Some(uid), s.to_string()))
}

impl SessionDesk {
    /// Connects by agent account name (e.g. "ghostdesk-agent") or socket path.
    ///
    /// When connecting by account name, checks that the socket peer (helper) uid is that account
    /// (guards against a spoofed socket).
    pub async fn connect(username_or_socket: &str, sink: FrameSink) -> Result<Arc<SessionDesk>> {
        Self::connect_with(username_or_socket, sink, SessionOptions::default()).await
    }

    /// Windows: the account name isn't used. Launches a child session (PiP) for the same user and
    /// connects to the helper inside it.
    #[cfg(windows)]
    pub async fn connect_with(_account: &str, sink: FrameSink, opts: SessionOptions) -> Result<Arc<SessionDesk>> {
        let ep = crate::winapp::child::endpoint();
        // The helper only starts at the child session's logon (RunOnce). If the session was already logged
        // on without one, log it off and start over once, instead of asking the user to do that by hand.
        let mut restarted = false;
        let result = loop {
            if let Err(e) = crate::winapp::child::ensure_connected().await {
                break Err(e);
            }
            // if the child session just logged on, the helper takes a moment to start
            let deadline = Instant::now() + crate::winapp::child::HELPER_WAIT;
            let last = loop {
                match Self::connect_target(ep.clone(), None, "자식 세션", sink.clone(), opts.clone()).await {
                    Ok(d) => return {
                        crate::winapp::child::forget_helper_start();
                        Ok(d)
                    },
                    Err(e) if Instant::now() >= deadline => break e,
                    Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
                }
            };
            if restarted {
                break Err(last.context(crate::winapp::child::HELPER_MISSING));
            }
            restarted = true;
            if let Err(e) = crate::winapp::child::restart_session().await {
                break Err(last.context(format!("{e:#}")).context(crate::winapp::child::HELPER_MISSING));
            }
        };
        crate::winapp::child::forget_helper_start();
        result
    }

    #[cfg(unix)]
    pub async fn connect_with(username_or_socket: &str, sink: FrameSink, opts: SessionOptions) -> Result<Arc<SessionDesk>> {
        let (path, expect, label) = resolve_target(username_or_socket)?;
        Self::connect_path(&path, expect, &label, sink, opts).await
    }

    /// Connects directly by socket path, checking the helper uid if expect_peer_uid is given.
    #[cfg(unix)]
    pub async fn connect_path(
        path: &Path,
        expect_peer_uid: Option<u32>,
        label: &str,
        sink: FrameSink,
        opts: SessionOptions,
    ) -> Result<Arc<SessionDesk>> {
        Self::connect_target(path.to_path_buf(), expect_peer_uid, label, sink, opts).await
    }

    pub async fn connect_target(
        path: Target,
        expect_peer_uid: Option<u32>,
        label: &str,
        sink: FrameSink,
        opts: SessionOptions,
    ) -> Result<Arc<SessionDesk>> {
        let me = Arc::new(SessionDesk {
            path,
            label: label.to_string(),
            expect_peer_uid,
            opts,
            conn: tokio::sync::Mutex::new(None),
            st: Mutex::new(ClientState::default()),
            closed: AtomicBool::new(false),
            sink,
            next_id: AtomicU64::new(1),
            poller: Mutex::new(None),
            canary_ok: AtomicBool::new(false),
            local_cursor: Mutex::new(Arc::new(real_local_cursor)),
            turn: Mutex::new(None),
            guard: Mutex::new(None),
        });
        let (c, hello) = me.open().await?;
        me.apply_hello(&hello, true);
        *me.conn.lock().await = Some(c);
        if me.opts.live_fps > 0.0 {
            let weak = Arc::downgrade(&me);
            let period = Duration::from_secs_f64(1.0 / me.opts.live_fps.clamp(0.1, 120.0));
            *me.poller.lock() = Some(tokio::spawn(live_poller(weak, period)));
        }
        Ok(me)
    }

    pub fn socket_path(&self) -> &Target {
        &self.path
    }

    fn note(&self, s: impl Into<String>) {
        let s = s.into();
        let mut st = self.st.lock();
        if st.notes.last() != Some(&s) {
            st.notes.push(s);
        }
    }

    /// Connects to the helper (Unix: socket + peer uid check).
    #[cfg(unix)]
    async fn dial(&self) -> Result<(Reader, Writer)> {
        let stream = tokio::time::timeout(Duration::from_secs(3), UnixStream::connect(&self.path))
            .await
            .map_err(|_| anyhow!("세션 헬퍼 소켓 연결 시간 초과: {}", self.path.display()))?
            .with_context(|| {
                format!(
                    "세션 헬퍼 소켓에 연결할 수 없습니다: {} — 에이전트 계정이 로그인되어 있고(빠른 사용자 전환) 헬퍼 LaunchAgent 가 실행 중인지 확인하세요",
                    self.path.display()
                )
            })?;
        if let Some(want) = self.expect_peer_uid {
            let got = peer_uid(&stream).context("소켓 상대방 uid 확인 실패")?;
            if got != want {
                bail!("소켓 소유자 uid {got} 가 예상한 에이전트 계정 uid {want} 와 다릅니다 — 위조된 헬퍼일 수 있어 접속을 거부합니다");
            }
        }
        let (rd, wr) = stream.into_split();
        Ok((Box::new(rd), Box::new(wr)))
    }

    /// Connects to the helper (Windows: named pipe; `platform::ipc` checks the pipe owner is the same
    /// user and its integrity level).
    #[cfg(windows)]
    async fn dial(&self) -> Result<(Reader, Writer)> {
        tokio::time::timeout(Duration::from_secs(3), crate::platform::ipc::connect(&self.path))
            .await
            .map_err(|_| anyhow!("세션 헬퍼 파이프 연결 시간 초과: {}", self.path.display()))?
            .map_err(|e| anyhow!("세션 헬퍼 파이프에 연결할 수 없습니다: {} ({e})", self.path.display()))
    }

    /// Connect + hello.
    async fn open(&self) -> Result<(Conn, Value)> {
        let (rd, wr) = self.dial().await?;
        let mut c = Conn { rd: BufReader::new(rd), wr };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let hello = json!({"id": id, "op": "hello", "version": PROTO_VERSION, "max_long_edge": self.opts.max_long_edge});
        // on an auth refusal the helper writes an error line before closing, so read the response even if
        // the write failed
        let _ = write_line(&mut c.wr, &hello).await;
        let line = tokio::time::timeout(Duration::from_secs(10), read_line_limited(&mut c.rd, MAX_RESPONSE_BYTES))
            .await
            .map_err(|_| anyhow!("세션 헬퍼 hello 응답 시간 초과"))?
            .context("세션 헬퍼 hello 응답 수신 실패")?
            .ok_or_else(|| anyhow!("세션 헬퍼가 hello 에 응답하지 않고 연결을 닫았습니다"))?;
        let v: Value = serde_json::from_slice(&line).context("hello 응답 JSON 파싱 실패")?;
        if v["ok"].as_bool() != Some(true) {
            bail!("{}", v["error"].as_str().unwrap_or("세션 헬퍼가 접속을 거부했습니다"));
        }
        Ok((c, v["result"].clone()))
    }

    fn apply_hello(&self, h: &Value, first: bool) {
        let display: Option<DisplayInfo> = serde_json::from_value(h["display"].clone()).ok();
        let frame = (
            h["frame"][0].as_u64().unwrap_or(0) as u32,
            h["frame"][1].as_u64().unwrap_or(0) as u32,
        );
        let on_console = h["on_console"].as_bool().unwrap_or(false);
        let mut st = self.st.lock();
        st.display = display;
        st.server_label = h["label"].as_str().unwrap_or_default().to_string();
        st.server_version = h["server_version"].as_str().unwrap_or("알 수 없음").to_string();
        st.app_control_supported = h["capabilities"]["app_control"] == true;
        if frame.0 > 0 && frame.1 > 0 {
            if first {
                st.size = frame;
                st.mouse = (frame.0 as f64 / 2.0, frame.1 as f64 / 2.0);
            } else if frame != st.size {
                let msg = format!(
                    "에이전트 세션 화면 크기가 {}x{} → {}x{} 로 바뀌었습니다. 새 스크린샷 기준으로 좌표를 다시 잡으세요.",
                    st.size.0, st.size.1, frame.0, frame.1
                );
                st.notes.push(msg);
                st.size = frame;
            }
        }
        if on_console && (first || !st.on_console) {
            st.notes.push("에이전트 세션이 지금 실제 모니터에 표시 중이라 헬퍼가 캡처·입력을 보류합니다 (빠른 사용자 전환으로 원래 계정에 돌아가면 재개).".into());
        }
        st.on_console = on_console;
        if !first {
            st.buttons = 0;
        }
    }

    /// Reconnects (retrying for reconnect_window).
    async fn reopen(&self) -> Result<Conn> {
        let deadline = Instant::now() + self.opts.reconnect_window;
        let mut last = anyhow!("재접속 시도 안 함");
        loop {
            if self.closed.load(Ordering::SeqCst) {
                bail!("세션 연결이 닫혔습니다");
            }
            match self.open().await {
                Ok((c, hello)) => {
                    self.canary_ok.store(false, Ordering::SeqCst);
                    self.apply_hello(&hello, false);
                    self.note("세션 헬퍼 연결이 끊겼다가 다시 연결되었습니다 (눌려 있던 마우스 버튼은 해제됨).");
                    return Ok(c);
                }
                Err(e) => last = e,
            }
            if Instant::now() >= deadline {
                return Err(last);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// One RPC. Idempotent requests are resent once after reconnecting if the connection drops.
    async fn call(&self, req: Value, idempotent: bool, tmo: Duration) -> Result<Value> {
        if self.closed.load(Ordering::SeqCst) {
            bail!("세션 헬퍼 연결이 닫혔습니다 ({})", self.path.display());
        }
        let op = req["op"].as_str().unwrap_or("?").to_string();
        let mut g = self.conn.lock().await;
        let mut retried = false;
        loop {
            if g.is_none() {
                match self.reopen().await {
                    Ok(c) => *g = Some(c),
                    Err(e) => {
                        self.closed.store(true, Ordering::SeqCst);
                        self.note("세션 헬퍼에 다시 연결하지 못해 세션 화면을 닫았습니다.");
                        bail!("세션 헬퍼에 연결할 수 없습니다: {e:#}");
                    }
                }
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            match rpc(g.as_mut().unwrap(), id, &req, tmo).await {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err(msg)) => bail!("{msg}"),
                Err(e) => {
                    *g = None;
                    if self.closed.load(Ordering::SeqCst) {
                        bail!("세션 연결이 닫혔습니다");
                    }
                    if idempotent && !retried {
                        retried = true;
                        continue;
                    }
                    // input requests aren't resent (they could be applied twice); just reconnect
                    match self.reopen().await {
                        Ok(c) => {
                            *g = Some(c);
                            bail!("세션 헬퍼 연결이 끊겨 다시 연결했습니다. '{op}' 동작이 적용되었는지 알 수 없으니 스크린샷으로 확인하세요 ({e:#})");
                        }
                        Err(e2) => {
                            self.closed.store(true, Ordering::SeqCst);
                            self.note("세션 헬퍼 연결이 끊어졌고 다시 연결하지 못했습니다.");
                            bail!("세션 헬퍼 연결이 끊어졌습니다: {e:#} / 재접속 실패: {e2:#}");
                        }
                    }
                }
            }
        }
    }

    fn agent_frame(&self) -> (u32, u32) {
        self.st.lock().size
    }

    fn update_console(&self, r: &Value) {
        if let Some(c) = r["on_console"].as_bool() {
            let mut st = self.st.lock();
            if c && !st.on_console {
                st.notes.push("에이전트 세션이 실제 모니터에 표시되기 시작해 헬퍼가 캡처·입력을 보류합니다.".into());
            }
            st.on_console = c;
        }
    }

    // ---- backend contract methods ----

    /// Screenshot pixel size (agent coordinate space).
    pub fn size(&self) -> (u32, u32) {
        self.agent_frame()
    }

    /// PNG of the entire agent session screen (exactly size()). Also sent to the live view.
    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        let r = self
            .call(
                json!({"op": "screenshot", "max_long_edge": self.opts.max_long_edge, "format": "png", "observe_input": true}),
                true,
                Duration::from_secs(20),
            )
            .await?;
        let bytes = B64.decode(r["data"].as_str().unwrap_or_default()).context("스크린샷 base64 디코드 실패")?;
        let (w, h) = (r["w"].as_u64().unwrap_or(0) as u32, r["h"].as_u64().unwrap_or(0) as u32);
        self.update_console(&r);
        let (aw, ah) = self.size();
        let bytes = if (w, h) != (aw, ah) {
            if w.abs_diff(aw) <= 2 && h.abs_diff(ah) <= 2 {
                // rounding differences: resize to keep the agent coordinate space
                let img = image::load_from_memory(&bytes)?.to_rgb8();
                encode_png(&image::imageops::resize(&img, aw, ah, image::imageops::FilterType::Triangle))?
            } else {
                {
                    let mut st = self.st.lock();
                    st.notes.push(format!(
                        "에이전트 세션 화면 크기가 {aw}x{ah} → {w}x{h} 로 바뀌었습니다. 새 스크린샷 기준으로 좌표를 다시 잡으세요."
                    ));
                    st.size = (w, h);
                }
                bytes
            }
        } else {
            bytes
        };
        self.st.lock().last_agent_shot = Some(Instant::now());
        (self.sink)(&bytes);
        Ok(bytes)
    }

    /// PNG of region [x0,y0,x1,y1] (agent coordinates) cropped at native resolution and scaled up.
    pub async fn zoom(&self, region: [f64; 4]) -> Result<Vec<u8>> {
        let (aw, ah) = self.size();
        if aw == 0 || ah == 0 {
            bail!("화면 크기를 아직 모릅니다");
        }
        let (x0, y0) = (region[0].min(region[2]), region[1].min(region[3]));
        let (x1, y1) = (region[0].max(region[2]), region[1].max(region[3]));
        let (cw, ch) = ((x1 - x0).max(1.0), (y1 - y0).max(1.0));
        let scale = (aw as f64 / cw).min(ah as f64 / ch).clamp(1.0, 8.0);
        let (tw, th) = (((cw * scale).round() as u32).max(1), ((ch * scale).round() as u32).max(1));
        let frac = [x0 / aw as f64, y0 / ah as f64, (x0 + cw) / aw as f64, (y0 + ch) / ah as f64];
        let r = self
            .call(json!({"op": "zoom", "region": frac, "out_w": tw, "out_h": th}), true, Duration::from_secs(30))
            .await?;
        Ok(B64.decode(r["data"].as_str().unwrap_or_default()).context("확대 이미지 base64 디코드 실패")?)
    }

    fn clamp(&self, x: f64, y: f64) -> (f64, f64) {
        let (aw, ah) = self.size();
        (x.clamp(0.0, aw.saturating_sub(1) as f64), y.clamp(0.0, ah.saturating_sub(1) as f64))
    }

    /// Pointer request. Always carries the position the agent last moved to (mouse_pos); if it
    /// differs from the last position the helper remembers (e.g. right after reconnecting), the
    /// helper moves there before pressing.
    ///
    /// mouse_pos only changes through agent actions (so it doesn't race with cursor values from live
    /// view polling).
    async fn pointer(&self, action: &str, button: u8, count: i64, mods: i64, idempotent: bool) -> Result<()> {
        let (fw, fh) = self.size();
        let (x, y) = self.mouse_pos();
        let req = json!({"op": "pointer", "action": action, "button": button, "x": x, "y": y,
                         "fw": fw, "fh": fh, "count": count, "mods": mods});
        let check = action != "up";
        let before = if check { self.read_local_cursor() } else { None };
        let r = self.call(req, idempotent, Duration::from_secs(10)).await?;
        if check {
            // the first check on each connection waits a little for the event to take effect
            let wait = !self.canary_ok.load(Ordering::SeqCst);
            self.check_leak(before, &r, wait).await?;
            self.canary_ok.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Swaps the function that reads the user session's cursor (for the self-test to simulate a leak).
    fn set_local_cursor_reader(&self, f: LocalCursorFn) {
        *self.local_cursor.lock() = f;
    }

    fn read_local_cursor(&self) -> Option<(f64, f64)> {
        let f = self.local_cursor.lock().clone();
        f()
    }

    /// If the user session's cursor moved to the helper-side target after a pointer request, input is
    /// leaking into the user's screen, so the connection is closed immediately (the helper releases
    /// pressed buttons on disconnect).
    async fn check_leak(&self, before: Option<(f64, f64)>, r: &Value, wait: bool) -> Result<()> {
        let (Some(before), Some(tx), Some(ty)) = (before, r["x"].as_f64(), r["y"].as_f64()) else {
            return Ok(());
        };
        let tries = if wait { 6 } else { 1 };
        for i in 0..tries {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let Some(after) = self.read_local_cursor() else { return Ok(()) };
            if leak_suspected(before, after, (tx, ty)) {
                self.closed.store(true, Ordering::SeqCst);
                if let Some(h) = self.poller.lock().take() {
                    h.abort();
                }
                if let Ok(mut g) = tokio::time::timeout(Duration::from_secs(3), self.conn.lock()).await {
                    g.take();
                }
                let msg = format!(
                    "안전 중단: 에이전트 세션으로 보낸 마우스 입력이 사용자 화면의 커서를 ({:.1},{:.1}) 로 움직인 것으로 \
                     보여 세션 연결을 닫았습니다. 이 Mac 에서는 백그라운드 세션의 입력이 사용자 세션으로 전달되므로 \
                     이 백엔드를 쓰면 안 됩니다.",
                    after.0, after.1
                );
                self.note(msg.clone());
                bail!(msg);
            }
        }
        Ok(())
    }

    /// If this connection hasn't been leak-checked yet, moves the agent cursor to its current position
    /// before any key or button input (harmless in the agent session) and checks the user's cursor
    /// doesn't follow.
    async fn ensure_canary(&self) -> Result<()> {
        if self.canary_ok.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.pointer("move", 0, 1, 0, true).await
    }

    pub async fn mouse_move(&self, x: f64, y: f64, mods: i64) -> Result<()> {
        let p = self.clamp(x, y);
        self.st.lock().mouse = p;
        self.pointer("move", 0, 1, mods, true).await
    }

    pub async fn mouse_down(&self, b: MouseButton, count: i64, mods: i64) -> Result<()> {
        let code = button_code(b);
        self.ensure_canary().await?;
        self.pointer("down", code, count, mods, false).await?;
        self.st.lock().buttons |= 1 << code;
        Ok(())
    }

    pub async fn mouse_up(&self, b: MouseButton, count: i64, mods: i64) -> Result<()> {
        let code = button_code(b);
        self.st.lock().buttons &= !(1 << code);
        self.pointer("up", code, count, mods, false).await
    }

    pub async fn click(&self, pos: Option<(f64, f64)>, b: MouseButton, count: u32, mods: i64) -> Result<()> {
        if let Some((x, y)) = pos {
            self.mouse_move(x, y, mods).await?;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        for i in 1..=count.clamp(1, 5) as i64 {
            if i > 1 {
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            self.mouse_down(b, i, mods).await?;
            tokio::time::sleep(Duration::from_millis(25)).await;
            self.mouse_up(b, i, mods).await?;
        }
        Ok(())
    }

    pub async fn drag(&self, from: (f64, f64), to: (f64, f64), mods: i64) -> Result<()> {
        self.mouse_move(from.0, from.1, mods).await?;
        self.mouse_down(MouseButton::Left, 1, mods).await?;
        let steps = 12;
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            tokio::time::sleep(Duration::from_millis(16)).await;
            let r = self
                .mouse_move(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t, mods)
                .await;
            if let Err(e) = r {
                let _ = self.mouse_up(MouseButton::Left, 1, mods).await;
                return Err(e);
            }
        }
        self.mouse_up(MouseButton::Left, 1, mods).await
    }

    /// dy > 0 = down, in pixels (one wheel notch ≈ 100).
    pub async fn scroll(&self, pos: Option<(f64, f64)>, dx: f64, dy: f64, mods: i64) -> Result<()> {
        if let Some((x, y)) = pos {
            // position changes go out as a move first so they pass the leak check (the scroll request carries
            // no position)
            self.mouse_move(x, y, mods).await?;
        } else {
            self.ensure_canary().await?;
        }
        let (fw, fh) = self.size();
        let req = json!({"op": "scroll", "fw": fw, "fh": fh, "dx": dx, "dy": dy, "mods": mods});
        self.call(req, false, Duration::from_secs(10)).await?;
        Ok(())
    }

    /// xdotool style ("Return", "ctrl+a", "cmd+shift+z").
    pub async fn key(&self, combo: &str, repeat: u32) -> Result<()> {
        let repeat = repeat.clamp(1, 100);
        self.ensure_canary().await?;
        self.call(
            json!({"op": "key", "combo": combo, "repeat": repeat}),
            false,
            Duration::from_secs(10 + repeat as u64 / 5),
        )
        .await?;
        Ok(())
    }

    pub async fn hold_key(&self, combo: &str, secs: f64) -> Result<()> {
        let secs = secs.clamp(0.0, 30.0);
        self.ensure_canary().await?;
        self.call(
            json!({"op": "hold_key", "combo": combo, "secs": secs}),
            false,
            Duration::from_secs_f64(secs + 10.0),
        )
        .await?;
        Ok(())
    }

    /// Types text (Korean included; Unicode key events bypass the IME).
    pub async fn type_text(&self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.ensure_canary().await?;
        // a \r\n split across a chunk boundary would give two Returns, so normalize to \n first
        let text = text.replace("\r\n", "\n");
        let chars: Vec<char> = text.chars().collect();
        for chunk in chars.chunks(TYPE_CHUNK_CHARS) {
            let s: String = chunk.iter().collect();
            self.call(json!({"op": "type", "text": s}), false, Duration::from_secs(60)).await?;
        }
        Ok(())
    }

    pub async fn app_control(&self, input: &Value) -> Result<String> {
        {
            let st = self.st.lock();
            if !st.app_control_supported {
                bail!("이 세션 헬퍼({})는 앱 요소 조작을 지원하지 않습니다. GhostDesk 앱과 자식 세션의 GhostDesk 헬퍼를 최신 버전으로 다시 시작하세요. 기존 화면·키보드 조작은 계속 사용할 수 있습니다.", st.server_version);
            }
        }
        if !matches!(input["action"].as_str(), Some("list_windows" | "ui_tree" | "read")) {
            self.ensure_canary().await?;
        }
        let (fw, fh) = self.size();
        let result = self.call(json!({"op":"app_control", "input":input, "fw":fw, "fh":fh}), false, Duration::from_secs(30)).await?;
        result["text"].as_str().map(str::to_string).ok_or_else(|| anyhow!("앱 요소 조작 응답에 text 가 없습니다"))
    }

    pub fn mouse_pos(&self) -> (f64, f64) {
        self.st.lock().mouse
    }

    pub fn views(&self) -> (Vec<ViewInfo>, Option<String>) {
        let st = self.st.lock();
        let who = if st.server_label.is_empty() { self.label.clone() } else { st.server_label.clone() };
        let detail = match st.display {
            Some(d) => format!(
                "{:.0}x{:.0}pt @{:.0}x, 에이전트 좌표 {}x{}{} — {}",
                d.width_pt,
                d.height_pt,
                d.scale,
                st.size.0,
                st.size.1,
                if st.on_console { " (지금 모니터에 표시 중 — 보류)" } else { "" },
                self.path.display()
            ),
            None => self.path.display().to_string(),
        };
        (
            vec![ViewInfo { id: "session".into(), title: format!("{who} 로그인 세션 (화면 전체)"), detail }],
            Some("session".into()),
        )
    }

    pub async fn switch_view(&self, index: usize) -> Result<()> {
        if index == 0 {
            Ok(())
        } else {
            bail!("에이전트 로그인 세션에는 뷰가 하나뿐입니다 (index 0). 창 전환은 화면에서 직접(예: cmd+tab) 하세요.")
        }
    }

    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut self.st.lock().notes)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Keeps `turn` until this screen closes.
    /// Keeps something alive for as long as this connection (the host bridge: closing it ends the link).
    pub fn keep_alive(&self, guard: Box<dyn std::any::Any + Send>) {
        *self.guard.lock() = Some(guard);
    }

    pub fn hold(&self, turn: crate::turns::Turn) {
        *self.turn.lock() = Some(turn);
    }

    /// Only closes the connection (the agent session and helper keep running).
    pub async fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // the next agent in line may use the session now
        self.turn.lock().take();
        if let Some(h) = self.poller.lock().take() {
            h.abort();
        }
        if let Ok(mut g) = tokio::time::timeout(Duration::from_secs(3), self.conn.lock()).await {
            if let Some(mut c) = g.take() {
                let _ = c.wr.shutdown().await;
            }
        }
    }
}

impl Drop for SessionDesk {
    fn drop(&mut self) {
        if let Some(h) = self.poller.lock().take() {
            h.abort();
        }
    }
}

/// Sends one request and waits for the response with the same id. The outer Err is a transport
/// error (disconnect/timeout).
async fn rpc(conn: &mut Conn, id: u64, req: &Value, tmo: Duration) -> Result<std::result::Result<Value, String>> {
    let mut msg = req.clone();
    msg["id"] = json!(id);
    let fut = async {
        write_line(&mut conn.wr, &msg).await.context("요청 전송 실패")?;
        loop {
            let line = read_line_limited(&mut conn.rd, MAX_RESPONSE_BYTES)
                .await
                .context("응답 수신 실패")?
                .ok_or_else(|| anyhow!("세션 헬퍼가 연결을 닫았습니다"))?;
            let v: Value = serde_json::from_slice(&line).context("응답 JSON 파싱 실패")?;
            let rid = v["id"].as_u64().unwrap_or(0);
            if rid != id && rid != 0 {
                continue;
            }
            return Ok(if v["ok"].as_bool() == Some(true) {
                Ok(v["result"].clone())
            } else {
                Err(v["error"].as_str().unwrap_or("알 수 없는 오류").to_string())
            });
        }
    };
    tokio::time::timeout(tmo, fut)
        .await
        .map_err(|_| anyhow!("세션 헬퍼 응답 시간 초과 ({:.0}초)", tmo.as_secs_f64()))?
}

/// Live view: fetches a small JPEG and sends it to the sink if the agent hasn't captured recently.
async fn live_poller(weak: Weak<SessionDesk>, period: Duration) {
    loop {
        tokio::time::sleep(period).await;
        let Some(me) = weak.upgrade() else { break };
        if me.is_closed() {
            break;
        }
        let recent = me.st.lock().last_agent_shot.map(|t| t.elapsed() < period).unwrap_or(false);
        if recent {
            continue;
        }
        let r = me
            .call(
                json!({"op": "screenshot", "max_long_edge": me.opts.live_max_long_edge, "format": "jpeg", "quality": 70}),
                true,
                Duration::from_secs(10),
            )
            .await;
        if let Ok(r) = r {
            if let Ok(bytes) = B64.decode(r["data"].as_str().unwrap_or_default()) {
                let (w, h) = (r["w"].as_u64().unwrap_or(0) as u32, r["h"].as_u64().unwrap_or(0) as u32);
                // the cursor position isn't applied (it would race with the agent's moves); only the console
                // state is updated
                let _ = (w, h);
                me.update_console(&r);
                (me.sink)(&bytes);
            }
        }
    }
}

// ---- setup helpers ----

#[cfg(unix)]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// Contents of `~/Library/LaunchAgents/com.ghostdesk.session-helper.plist` in the agent account.
///
/// exe_path must be somewhere the agent account can read and execute (e.g.
/// /Users/Shared/GhostDesk/bin/ghostdesk).
#[cfg(unix)]
pub fn launch_agent_plist(exe_path: &str, allow_uid: u32) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>--session-helper</string>
		<string>--allow-uid</string>
		<string>{uid}</string>
	</array>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>ThrottleInterval</key>
	<integer>30</integer>
	<key>ProcessType</key>
	<string>Interactive</string>
</dict>
</plist>
"#,
        label = LAUNCH_AGENT_LABEL,
        exe = xml_escape(exe_path),
        uid = allow_uid
    )
}

/// `ghostdesk --print-session-plist <allowed uid> [helper executable path]`: prints the plist to
/// stdout. Without a path, the currently running binary is used.
#[cfg(unix)]
pub fn print_session_plist(args: &[String]) -> Result<()> {
    let uid: u32 = args
        .first()
        .ok_or_else(|| anyhow!("사용법: ghostdesk --print-session-plist <허용할 사용자 uid> [실행 파일 경로]"))?
        .parse()
        .context("uid 는 숫자여야 합니다")?;
    let exe = match args.get(1) {
        Some(p) => p.clone(),
        None => std::env::current_exe()?.to_string_lossy().into_owned(),
    };
    print!("{}", launch_agent_plist(&exe, uid));
    Ok(())
}

/// Setup guide for the README (Markdown).
#[cfg(unix)]
pub const SETUP_GUIDE_KO: &str = r#"## 별도 로그인 세션 모드 (전용 에이전트 계정)

에이전트가 **사용자 화면이 아닌 별도의 macOS 로그인 세션** 전체(모든 앱)를 조작하는 모드입니다.
macOS 는 사용자당 GUI 세션 하나만 허용하므로 에이전트 전용 계정을 하나 만들어 그 계정의 세션을
백그라운드에 띄워 두고, 그 안의 헬퍼가 화면 캡처·입력을 대신합니다. 사용자 화면·커서·키보드에는
아무 영향이 없습니다.

### 자동으로 되는 것 / 직접 해야 하는 것
GhostDesk 에서 에이전트 계정을 만들면(관리자 암호 한 번) 아래가 **자동**으로 적용됩니다.
- 표준 계정 생성, 암호를 키체인에 저장
- 헬퍼 바이너리를 `/Users/Shared/GhostDesk/bin/ghostdesk` 에 복사
- 그 계정의 LaunchAgent(`com.ghostdesk.session-helper.plist`) 설치
- 그 계정의 화면 보호기·암호 요구 끄기
macOS 가 밖에서 대신 해 주지 못하게 막는 것은 **한 번만 직접** 하세요 (아래 2).

### 1. 빠른 사용자 전환
제어 센터 > **빠른 사용자 전환** 메뉴를 켭니다 (시스템 설정 > 제어 센터). 본인 계정의 uid 는 `id -u` (보통 501).
기존 계정에는 새 에이전트 화면의 '세션 설정 적용' 으로 같은 자동 설정을 다시 적용할 수 있습니다.

### 2. 에이전트 세션 안에서 (빠른 사용자 전환으로 `ghostdesk-agent` 로그인, 한 번)
1. 한 번 로그인해 둡니다 (로그인해야 LaunchAgent 가 뜹니다).
2. 시스템 설정 > 개인정보 보호 및 보안 > **화면 및 시스템 오디오 기록** 에 `/Users/Shared/GhostDesk/bin/ghostdesk` 추가·허용.
3. 같은 곳의 **손쉬운 사용** 에도 추가·허용 (입력 이벤트 게시에 필요).
4. 헬퍼 재시작: `launchctl kickstart -k gui/$(id -u)/com.ghostdesk.session-helper`
   로그: `~/Library/Logs/GhostDesk/session-helper.log` (에이전트 계정).
5. 시스템 설정 > 개인정보 보호 및 보안 > 고급 — "일정 시간 동안 사용하지 않으면 로그아웃" **끄기**. (절전·디스플레이 끄기 설정은 내 Mac 설정이라 GhostDesk 가 건드리지 않습니다.)
6. 화면 기록 권한의 주기적 재확인 알림이 뜨면 에이전트 세션으로 전환해 허용해야 합니다.

### 3. (수동 설치를 원할 때) 명령어
```sh
sudo mkdir -p /Users/Shared/GhostDesk/bin && sudo cp /path/to/ghostdesk /Users/Shared/GhostDesk/bin/ghostdesk
sudo chmod 755 /Users/Shared/GhostDesk/bin/ghostdesk && sudo chmod 1777 /Users/Shared/GhostDesk
# 에이전트 계정의 터미널에서 (<UID> = 본인 uid)
mkdir -p ~/Library/LaunchAgents
/Users/Shared/GhostDesk/bin/ghostdesk --print-session-plist <UID> > ~/Library/LaunchAgents/com.ghostdesk.session-helper.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.ghostdesk.session-helper.plist
```
바이너리를 교체(재빌드)하면 권한(2번)을 다시 켜야 할 수 있습니다 (TCC 는 바이너리 서명/경로 기준).

### 4. 원래 계정으로 돌아오기
빠른 사용자 전환 메뉴에서 본인 계정을 선택해 돌아옵니다. **에이전트 계정에서 로그아웃하지 마세요**
(로그아웃하면 세션과 헬퍼가 끝납니다). 에이전트 세션이 화면에 표시되는 동안에는 사람이 보고 있을 수
있으므로 헬퍼가 캡처·입력을 보류하고, 본인 계정으로 돌아오면 다시 동작합니다.
GhostDesk 에서 백엔드로 '별도 로그인 세션'을 고르고 계정 이름(`ghostdesk-agent`)을 입력하면 접속합니다.

### 주의
- **재부팅/로그아웃 후**에는 에이전트 세션이 없습니다. 매번 빠른 사용자 전환으로 `ghostdesk-agent` 에
  한 번 로그인했다가 본인 계정으로 돌아와야 합니다 (LaunchAgent 는 로그인 시 자동 시작).
- **FileVault** 를 켠 Mac 은 부팅 시 자동 로그인을 쓸 수 없습니다. 부팅 후 본인 계정으로 디스크를 잠금
  해제한 뒤, 빠른 사용자 전환으로 에이전트 계정에 로그인하세요 (에이전트 계정은 FileVault 잠금 해제
  권한이 없어도 됩니다).
- Mac 이 잠자기에 들어가면 에이전트 세션도 멈춥니다. 작업 중에는 본인 계정의 에너지 설정에서
  "디스플레이가 꺼져 있을 때 자동으로 잠자기 방지"를 켜 두세요.
- 백그라운드(비콘솔) 세션에서 화면 캡처·입력이 동작하는지는 macOS 버전에 따라 다를 수 있습니다.
  헬퍼 로그와 GhostDesk 의 첫 스크린샷으로 확인하세요.
- 에이전트 세션은 사용자 본인 계정과 완전히 분리되어 있어 본인의 로그인 상태(쿠키·앱 계정)는 공유되지
  않습니다. 필요한 서비스는 에이전트 계정에서 따로 로그인하세요.
"#;

// ---- self-test (mock platform) ----

/// Synthetic framebuffer: pixel color = position (R = x fraction, G = y fraction, B = 77). Input
/// is recorded as strings.
pub(crate) struct MockPlatform {
    disp: DisplayInfo,
    events: Mutex<Vec<String>>,
    console: AtomicBool,
    #[cfg(test)]
    observations: AtomicU64,
    #[cfg(test)]
    console_after_input: AtomicBool,
    cursor: Mutex<(f64, f64)>,
}

fn mock_color(u: f64, v: f64) -> Rgb<u8> {
    Rgb([(u.clamp(0.0, 1.0) * 255.0).round() as u8, (v.clamp(0.0, 1.0) * 255.0).round() as u8, 77])
}

impl MockPlatform {
    pub(crate) fn new() -> Self {
        MockPlatform {
            disp: DisplayInfo { origin_x: 0.0, origin_y: 0.0, width_pt: 1512.0, height_pt: 982.0, scale: 2.0 },
            events: Mutex::new(Vec::new()),
            console: AtomicBool::new(false),
            #[cfg(test)]
            observations: AtomicU64::new(0),
            #[cfg(test)]
            console_after_input: AtomicBool::new(false),
            cursor: Mutex::new((10.0, 10.0)),
        }
    }
    fn rec(&self, s: String) {
        self.events.lock().push(s);
        #[cfg(test)]
        if self.console_after_input.load(Ordering::SeqCst) {
            self.console.store(true, Ordering::SeqCst);
        }
    }
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.events.lock())
    }
}

impl SessionPlatform for MockPlatform {
    #[cfg(test)]
    fn supports_app_control(&self) -> bool { true }
    #[cfg(test)]
    fn app_control(&self, input: &Value, state: &mut AppControlState, _frame: (u32, u32), check: &dyn Fn() -> Result<()>) -> Result<String> {
        check()?;
        state.mock_calls += 1;
        Ok(format!("mock {} {}", input["action"].as_str().unwrap_or(""), state.mock_calls))
    }
    #[cfg(test)]
    fn observe_input(&self, _epoch: u64) {
        self.observations.fetch_add(1, Ordering::SeqCst);
    }
    fn name(&self) -> String {
        "mock".into()
    }
    fn display(&self) -> Result<DisplayInfo> {
        Ok(self.disp)
    }
    fn capture(&self, max_long_edge: u32) -> Result<RgbImage> {
        let (w, h) = self.disp.frame_size(max_long_edge);
        Ok(RgbImage::from_fn(w, h, |x, y| {
            mock_color((x as f64 + 0.5) / w as f64, (y as f64 + 0.5) / h as f64)
        }))
    }
    fn mouse_move(&self, x: f64, y: f64, pressed: Option<u8>, mods: i64) -> Result<()> {
        *self.cursor.lock() = (x, y);
        self.rec(match pressed {
            Some(b) => format!("drag{b} {x:.2},{y:.2} m{mods}"),
            None => format!("move {x:.2},{y:.2} m{mods}"),
        });
        Ok(())
    }
    fn mouse_button(&self, x: f64, y: f64, button: u8, down: bool, count: i64, mods: i64) -> Result<()> {
        self.rec(format!("{}{button} {x:.2},{y:.2} c{count} m{mods}", if down { "down" } else { "up" }));
        Ok(())
    }
    fn scroll(&self, dx: f64, dy: f64, mods: i64) -> Result<()> {
        self.rec(format!("scroll {dx:.2},{dy:.2} m{mods}"));
        Ok(())
    }
    fn key(&self, combo: &str, hold: Duration, check: &dyn Fn() -> Result<()>) -> Result<()> {
        check()?;
        crate::keys::parse_combo(combo)?;
        self.rec(format!("key {combo} {}ms", hold.as_millis()));
        hold_checked(hold, check)
    }
    fn type_unicode(&self, text: &str, check: &dyn Fn() -> Result<()>) -> Result<()> {
        check()?;
        self.rec(format!("type {text}"));
        Ok(())
    }
    fn cursor(&self) -> Result<(f64, f64)> {
        Ok(*self.cursor.lock())
    }
    fn on_console(&self) -> bool {
        self.console.load(Ordering::SeqCst)
    }
}

struct Checker {
    fails: Vec<String>,
    passes: usize,
}

impl Checker {
    fn check(&mut self, name: &str, ok: bool, detail: impl std::fmt::Display) {
        if ok {
            self.passes += 1;
            println!("  ✓ {name}");
        } else {
            println!("  ✗ {name} — {detail}");
            self.fails.push(format!("{name}: {detail}"));
        }
    }
}

fn quiet_log(tag: &'static str) -> LogFn {
    Arc::new(move |s: &str| println!("    [{tag}] {s}"))
}

async fn wait_for(mut f: impl FnMut() -> bool, tmo: Duration) -> bool {
    let end = Instant::now() + tmo;
    while Instant::now() < end {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    f()
}

/// `ghostdesk --selftest-session [--dir <temp dir>]`
///
/// Exercises the whole client↔server RPC on a temp socket with the mock platform. No real capture
/// or input.
#[cfg(unix)]
pub async fn selftest(args: &[String]) -> anyhow::Result<()> {
    let mut base: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--dir" {
            base = args.get(i + 1).map(PathBuf::from);
            i += 1;
        }
        i += 1;
    }
    let dir = base
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("gd-sess-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let r = selftest_inner(&dir).await;
    let _ = std::fs::remove_dir_all(&dir);
    r
}

#[cfg(unix)]
async fn selftest_inner(dir: &Path) -> Result<()> {
    let mut c = Checker { fails: vec![], passes: 0 };
    let me = getuid();

    // ---- 1. pure functions ----
    println!("[1] 순수 함수: 좌표 변환, 텍스트 분할, 거부 규칙, plist");
    let d = MockPlatform::new().disp;
    c.check("frame_size(1280) = 1280x831", d.frame_size(1280) == (1280, 831), format!("{:?}", d.frame_size(1280)));
    c.check("pixel_size = 3024x1964", d.pixel_size() == (3024, 1964), format!("{:?}", d.pixel_size()));
    let p = d.to_points(640.0, 415.5, 1280, 831);
    c.check("to_points 중앙 → (756, 491)", (p.0 - 756.0).abs() < 1e-9 && (p.1 - 491.0).abs() < 1e-9, format!("{p:?}"));
    let p = d.to_points(5000.0, -5.0, 1280, 831);
    c.check("to_points clamp", p == (1511.0, 0.0), format!("{p:?}"));
    let f = d.to_frame(756.0, 491.0, 1280, 831);
    c.check("to_frame 역변환", (f.0 - 640.0).abs() < 1e-9 && (f.1 - 415.5).abs() < 1e-9, format!("{f:?}"));
    let segs = split_text("안녕\r\n하세요\t끝\n");
    c.check(
        "split_text (\\r\\n→Return, \\t→Tab)",
        segs == vec![
            Seg::Text("안녕".into()),
            Seg::Key("Return"),
            Seg::Text("하세요".into()),
            Seg::Key("Tab"),
            Seg::Text("끝".into()),
            Seg::Key("Return")
        ],
        format!("{segs:?}"),
    );
    c.check("거부: 같은 uid", refusal_reason(501, &[501], false, false).is_some(), "None");
    c.check("거부: 관리자 계정", refusal_reason(502, &[501], true, false).is_some(), "None");
    c.check("거부: root", refusal_reason(0, &[501], false, false).is_some(), "None");
    c.check("거부: allow-uid 없음", refusal_reason(502, &[], false, false).is_some(), "None");
    c.check("허용: 전용 표준 계정", refusal_reason(502, &[501], false, false).is_none(), "Some");
    c.check("허용: --allow-admin-account", refusal_reason(502, &[501], true, true).is_none(), "Some");
    let real_admin = is_admin(me);
    let real = refusal_reason(me, &[me], real_admin, false);
    c.check(
        &format!("이 Mac 의 현재 사용자(uid {me}, admin={real_admin}, console={:?}) 세션에서는 헬퍼 거부", console_uid()),
        real.is_some(),
        "거부하지 않음",
    );
    let plist = launch_agent_plist("/Users/Shared/GhostDesk/bin/ghost&desk", 501);
    c.check(
        "plist 내용",
        plist.contains("<string>Aqua</string>")
            && plist.contains("<string>--session-helper</string>")
            && plist.contains("<string>501</string>")
            && plist.contains("ghost&amp;desk")
            && plist.contains(LAUNCH_AGENT_LABEL),
        "필수 키 누락",
    );
    let plist_path = dir.join("helper.plist");
    std::fs::write(&plist_path, &plist)?;
    match std::process::Command::new("/usr/bin/plutil").arg("-lint").arg(&plist_path).output() {
        Ok(o) => c.check("plutil -lint", o.status.success(), String::from_utf8_lossy(&o.stdout).trim().to_string()),
        Err(e) => println!("  - plutil 없음, 건너뜀 ({e})"),
    }
    c.check(
        "socket_path_for",
        socket_path_for("ghostdesk-agent") == Path::new("/Users/Shared/GhostDesk/session-ghostdesk-agent.sock"),
        format!("{:?}", socket_path_for("ghostdesk-agent")),
    );
    c.check("resolve_target(자기 계정) 거부", username_of(me).map(|n| resolve_target(&n).is_err()).unwrap_or(true), "허용됨");
    c.check("resolve_target(없는 계정) 거부", resolve_target("no-such-user-gd-xyz").is_err(), "허용됨");
    c.check(
        "leak_suspected: 사용자 커서가 목표로 이동 → 누출",
        leak_suspected((10.0, 10.0), (756.0, 472.68), (756.0, 472.68))
            && leak_suspected((10.0, 10.0), (756.5, 472.5), (756.0, 472.68)),
        "감지 못 함",
    );
    c.check(
        "leak_suspected: 움직이지 않음/다른 곳 → 정상",
        !leak_suspected((756.0, 472.68), (756.0, 472.68), (756.0, 472.68))
            && !leak_suspected((10.0, 10.0), (300.0, 200.0), (756.0, 472.68)),
        "오탐",
    );

    // ---- 2. server + client ----
    println!("[2] mock 서버 ↔ SessionDesk RPC");
    let sock = dir.join("s.sock");
    let mock = Arc::new(MockPlatform::new());
    let cfg = ServerConfig { allow_uids: vec![me], allow_on_console: false, label: "mock-agent".into(), log: quiet_log("srv") };
    let srv = spawn_server(UnixListener::bind(&sock)?, mock.clone(), cfg.clone());

    let frames = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let fr = frames.clone();
    let sink: FrameSink = Arc::new(move |b: &[u8]| fr.lock().push(b.to_vec()));
    let opts = SessionOptions { live_fps: 4.0, reconnect_window: Duration::from_secs(3), ..Default::default() };
    let desk = SessionDesk::connect_with(sock.to_str().unwrap(), sink.clone(), opts.clone()).await?;
    c.check("size() = 1280x831", desk.size() == (1280, 831), format!("{:?}", desk.size()));
    let (views, active) = desk.views();
    c.check("views 하나 + active", views.len() == 1 && active.as_deref() == Some("session"), format!("{views:?}"));

    // screenshot
    let png = desk.screenshot().await?;
    let img = image::load_from_memory(&png)?.to_rgb8();
    c.check("screenshot PNG = size()", img.dimensions() == desk.size(), format!("{:?}", img.dimensions()));
    let px = *img.get_pixel(640, 400);
    let want = mock_color(640.5 / 1280.0, 400.5 / 831.0);
    c.check("screenshot 픽셀 내용", px == want, format!("{px:?} != {want:?}"));
    c.check("sink 에 PNG 전달", frames.lock().iter().any(|f| f.starts_with(&[0x89, b'P', b'N', b'G'])), "없음");

    // click coordinate conversion
    mock.take();
    desk.click(Some((640.0, 400.0)), MouseButton::Left, 2, crate::keys::MOD_SHIFT).await?;
    let (ex, ey) = d.to_points(640.0, 400.0, 1280, 831);
    let ev = mock.take();
    let want = vec![
        format!("move {ex:.2},{ey:.2} m8"),
        format!("down0 {ex:.2},{ey:.2} c1 m8"),
        format!("up0 {ex:.2},{ey:.2} c1 m8"),
        format!("down0 {ex:.2},{ey:.2} c2 m8"),
        format!("up0 {ex:.2},{ey:.2} c2 m8"),
    ];
    c.check("더블클릭 (shift) → 포인트 좌표 756.00,472.68", ev == want && ex == 756.0, format!("{ev:?}"));
    c.check("mouse_pos = (640,400)", desk.mouse_pos() == (640.0, 400.0), format!("{:?}", desk.mouse_pos()));

    // right click
    desk.click(Some((0.0, 0.0)), MouseButton::Right, 1, 0).await?;
    let ev = mock.take();
    c.check("오른쪽 클릭", ev.iter().any(|e| e.starts_with("down1 0.00,0.00")) && ev.iter().any(|e| e.starts_with("up1")), format!("{ev:?}"));

    // drag
    desk.drag((100.0, 100.0), (400.0, 300.0), 0).await?;
    let ev = mock.take();
    let (dx0, dy0) = d.to_points(100.0, 100.0, 1280, 831);
    let (dx1, dy1) = d.to_points(400.0, 300.0, 1280, 831);
    c.check(
        "드래그: move → down0 → drag0 x12 → up0",
        ev.first() == Some(&format!("move {dx0:.2},{dy0:.2} m0"))
            && ev.get(1).map(|s| s.starts_with("down0")).unwrap_or(false)
            && ev.iter().filter(|e| e.starts_with("drag0")).count() == 12
            && ev.iter().any(|e| e == &format!("drag0 {dx1:.2},{dy1:.2} m0"))
            && ev.last() == Some(&format!("up0 {dx1:.2},{dy1:.2} c1 m0")),
        format!("{ev:?}"),
    );

    // scroll
    desk.scroll(Some((640.0, 400.0)), 0.0, 300.0, 0).await?;
    let ev = mock.take();
    let sy = 300.0 * 982.0 / 831.0;
    c.check(
        "스크롤 (위치 이동 후 dy 포인트 환산)",
        ev == vec![format!("move {ex:.2},{ey:.2} m0"), format!("scroll 0.00,{sy:.2} m0")],
        format!("{ev:?}"),
    );

    // keys
    desk.key("cmd+shift+z", 2).await?;
    desk.hold_key("shift", 0.2).await?;
    let ev = mock.take();
    c.check(
        "key 반복 + hold_key",
        ev == vec!["key cmd+shift+z 0ms", "key cmd+shift+z 0ms", "key shift 200ms"],
        format!("{ev:?}"),
    );
    let e = desk.key("ctrl+nosuchkey", 1).await;
    c.check(
        "잘못된 키 → 오류 전달 (연결 유지)",
        e.as_ref().err().map(|e| e.to_string().contains("알 수 없는 키")).unwrap_or(false) && !desk.is_closed(),
        format!("{e:?}"),
    );

    // Korean text
    desk.type_text("안녕하세요 GhostDesk 👋\n둘째 줄\t끝").await?;
    let ev = mock.take();
    c.check(
        "한글/이모지 텍스트 + 줄바꿈/탭",
        ev == vec!["type 안녕하세요 GhostDesk 👋", "key Return 0ms", "type 둘째 줄", "key Tab 0ms", "type 끝"],
        format!("{ev:?}"),
    );
    let long: String = "가나다라마".repeat(200);
    desk.type_text(&long).await?;
    let ev = mock.take();
    let joined: String = ev.iter().map(|e| e.trim_start_matches("type ")).collect();
    c.check("긴 한글 텍스트 분할 전송 (3 요청, 내용 동일)", ev.len() == 3 && joined == long, format!("{} 요청", ev.len()));
    let crlf = format!("{}\r\nb", "a".repeat(TYPE_CHUNK_CHARS - 1));
    desk.type_text(&crlf).await?;
    let ev = mock.take();
    c.check(
        "덩어리 경계의 \\r\\n → Return 한 번",
        ev.iter().filter(|e| e.as_str() == "key Return 0ms").count() == 1 && ev.last().map(|e| e.as_str()) == Some("type b"),
        format!("{ev:?}"),
    );

    // zoom
    let z = desk.zoom([100.0, 100.0, 300.0, 200.0]).await?;
    let zi = image::load_from_memory(&z)?.to_rgb8();
    c.check("zoom 크기 1280x640", zi.dimensions() == (1280, 640), format!("{:?}", zi.dimensions()));
    let zc = *zi.get_pixel(640, 320);
    let want = mock_color(200.0 / 1280.0, 150.0 / 831.0);
    let close = (0..3).all(|k| (zc[k] as i32 - want[k] as i32).abs() <= 3);
    c.check("zoom 중심 픽셀 = 영역 중심", close, format!("{zc:?} vs {want:?}"));

    // live view + polling doesn't overwrite mouse_pos (even when the helper-side cursor is elsewhere)
    desk.mouse_move(640.0, 400.0, 0).await?;
    *mock.cursor.lock() = (378.0, 245.5);
    frames.lock().clear();
    let ok = wait_for(|| frames.lock().iter().filter(|f| f.starts_with(&[0xFF, 0xD8])).count() >= 2, Duration::from_secs(3)).await;
    c.check("라이브 뷰 폴러가 JPEG 프레임 전달", ok, "3초 안에 없음");
    c.check("폴링이 mouse_pos 를 덮어쓰지 않음", desk.mouse_pos() == (640.0, 400.0), format!("{:?}", desk.mouse_pos()));
    mock.take();
    desk.mouse_down(MouseButton::Left, 1, 0).await?;
    desk.mouse_up(MouseButton::Left, 1, 0).await?;
    let ev = mock.take();
    c.check(
        "down/up 은 마지막 move 위치에서",
        ev == vec![format!("down0 {ex:.2},{ey:.2} c1 m0"), format!("up0 {ex:.2},{ey:.2} c1 m0")],
        format!("{ev:?}"),
    );

    // held off while on the console, except releasing a button already pressed
    desk.mouse_down(MouseButton::Left, 1, 0).await?;
    mock.take();
    mock.console.store(true, Ordering::SeqCst);
    let up = desk.mouse_up(MouseButton::Left, 1, 0).await;
    let ev = mock.take();
    c.check(
        "콘솔 표시 중에도 눌린 버튼 해제(up)는 전달",
        up.is_ok() && ev == vec![format!("up0 {ex:.2},{ey:.2} c1 m0")],
        format!("{up:?} / {ev:?}"),
    );
    let e1 = desk.screenshot().await;
    let e2 = desk.click(Some((10.0, 10.0)), MouseButton::Left, 1, 0).await;
    let ev = mock.take();
    c.check(
        "콘솔 표시 중 → 캡처·입력 보류",
        e1.is_err()
            && e2.is_err()
            && ev.is_empty()
            && e1.as_ref().err().map(|e| e.to_string().contains("콘솔")).unwrap_or(false),
        format!("{e1:?} / {e2:?} / {ev:?}"),
    );
    mock.console.store(false, Ordering::SeqCst);
    c.check("보류 해제 후 정상", desk.screenshot().await.is_ok() && !desk.is_closed(), "실패");

    // ---- 3. authentication ----
    println!("[3] 인증");
    let sock2 = dir.join("deny.sock");
    let cfg2 = ServerConfig { allow_uids: vec![me + 4242], ..cfg.clone() };
    let srv2 = spawn_server(UnixListener::bind(&sock2)?, Arc::new(MockPlatform::new()), cfg2);
    let r = SessionDesk::connect_with(sock2.to_str().unwrap(), sink.clone(), opts.clone()).await;
    let msg = r.as_ref().err().map(|e| format!("{e:#}")).unwrap_or_default();
    c.check("허용 목록에 없는 uid → 인증 거부", r.is_err() && msg.contains("인증 거부"), &msg);
    srv2.stop().await;
    let r = SessionDesk::connect_path(&sock, Some(me + 1), "x", sink.clone(), opts.clone()).await;
    let msg = r.as_ref().err().map(|e| format!("{e:#}")).unwrap_or_default();
    c.check("헬퍼 uid 불일치 → 위조 소켓으로 보고 거부", r.is_err() && msg.contains("위조"), &msg);
    let r = SessionDesk::connect_path(&dir.join("none.sock"), None, "x", sink.clone(), opts.clone()).await;
    c.check("소켓 없음 → 안내 오류", r.as_ref().err().map(|e| format!("{e:#}").contains("헬퍼")).unwrap_or(false), format!("{:?}", r.err()));

    // ---- 4. disconnect / reconnect / close ----
    println!("[4] 연결 끊김 처리");
    desk.mouse_move(200.0, 200.0, 0).await?;
    desk.mouse_down(MouseButton::Left, 1, 0).await?;
    mock.take();
    srv.stop().await; // helper exits (simulating a launchd restart)
    let ev = mock.take();
    let (rx, ry) = d.to_points(200.0, 200.0, 1280, 831);
    c.check("연결 종료 시 서버가 눌린 버튼을 놓음", ev == vec![format!("up0 {rx:.2},{ry:.2} c1 m0")], format!("{ev:?}"));
    let _ = std::fs::remove_file(&sock);
    // restart the helper after 400 ms
    let sock_r = sock.clone();
    let mock_r = mock.clone();
    let cfg_r = cfg.clone();
    let restart = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        spawn_server(UnixListener::bind(&sock_r).unwrap(), mock_r, cfg_r)
    });
    let shot = desk.screenshot().await;
    let notes = desk.take_notes();
    c.check(
        "재시작된 헬퍼에 자동 재접속 (screenshot 재시도 성공 + 알림)",
        shot.is_ok() && notes.iter().any(|n| n.contains("다시 연결")) && !desk.is_closed(),
        format!("{:?} / {notes:?}", shot.err()),
    );
    let srv3 = restart.await?;
    mock.take();
    desk.mouse_down(MouseButton::Left, 1, 0).await?;
    desk.mouse_up(MouseButton::Left, 1, 0).await?;
    let ev = mock.take();
    c.check(
        "재접속 직후 down → 헬퍼가 먼저 마지막 위치로 옮긴 뒤 누름",
        ev == vec![format!("move {rx:.2},{ry:.2} m0"), format!("down0 {rx:.2},{ry:.2} c1 m0"), format!("up0 {rx:.2},{ry:.2} c1 m0")],
        format!("{ev:?}"),
    );
    desk.click(Some((50.0, 50.0)), MouseButton::Left, 1, 0).await?;
    let ev = mock.take();
    c.check("재접속 후 클릭 정상", ev.len() == 3, format!("{ev:?}"));
    srv3.stop().await;
    let _ = std::fs::remove_file(&sock);
    let e = desk.type_text("보내지면 안 됨").await;
    let ev = mock.take();
    c.check(
        "헬퍼가 사라지면 입력은 재전송하지 않고 오류",
        e.is_err() && ev.is_empty(),
        format!("{e:?} / {ev:?}"),
    );
    c.check("재접속 실패 → is_closed()", desk.is_closed(), "닫히지 않음");
    c.check("닫힌 뒤 호출은 즉시 오류", desk.screenshot().await.is_err(), "성공함");
    desk.shutdown().await;
    let n_frames = frames.lock().len();
    tokio::time::sleep(Duration::from_millis(600)).await;
    c.check("shutdown 후 폴러 정지", frames.lock().len() == n_frames, "프레임 계속 옴");

    // shutdown behavior (new server)
    let srv4 = spawn_server(UnixListener::bind(&sock)?, mock.clone(), cfg.clone());
    let d2 = SessionDesk::connect_with(sock.to_str().unwrap(), sink.clone(), SessionOptions { live_fps: 0.0, ..opts.clone() }).await?;
    d2.shutdown().await;
    c.check("shutdown → is_closed, 이후 호출 오류", d2.is_closed() && d2.key("a", 1).await.is_err(), "실패");

    // ---- 5. input leak detection (a mock cursor simulates the user's cursor moving to the helper-side target) ----
    println!("[5] 입력 누출 감지");
    let d3 = SessionDesk::connect_with(sock.to_str().unwrap(), sink.clone(), SessionOptions { live_fps: 0.0, ..opts.clone() }).await?;
    let mc = mock.clone();
    d3.set_local_cursor_reader(Arc::new(move || Some(*mc.cursor.lock())));
    *mock.cursor.lock() = (3.0, 3.0);
    mock.take();
    let e = d3.key("a", 1).await;
    let ev = mock.take();
    c.check(
        "첫 입력 전 카나리 이동에서 누출 감지 → 키 미전송 + 연결 닫힘",
        e.as_ref().err().map(|e| e.to_string().contains("안전 중단")).unwrap_or(false)
            && d3.is_closed()
            && ev.len() == 1
            && ev[0].starts_with("move "),
        format!("{e:?} / {ev:?}"),
    );
    c.check("누출 알림이 take_notes 로 전달", d3.take_notes().iter().any(|n| n.contains("안전 중단")), "없음");
    srv4.stop().await;

    println!("\n결과: 통과 {} / 실패 {}", c.passes, c.fails.len());
    if !c.fails.is_empty() {
        bail!("세션 셀프테스트 실패:\n{}", c.fails.join("\n"));
    }
    println!("세션 백엔드 셀프테스트 통과");
    Ok(())
}

/// Windows `--selftest-session` and tests: round trip between a mock helper and SessionDesk over
/// a temp named pipe (no real capture or input).
#[cfg(windows)]
pub(crate) async fn pipe_roundtrip() -> Result<()> {
    let ep = crate::platform::ipc::Endpoint::custom(&format!("ghostdesk-sess-test-{}", std::process::id()));
    let mock = Arc::new(MockPlatform::new());
    let cfg = ServerConfig { allow_uids: vec![], allow_on_console: false, label: "mock".into(), log: quiet_log("srv") };
    let srv = spawn_server(HelperListener::bind(&ep).await?, mock.clone(), cfg);
    let sink: FrameSink = Arc::new(|_: &[u8]| {});
    let opts = SessionOptions { live_fps: 0.0, ..Default::default() };
    let d = SessionDesk::connect_target(ep, None, "mock", sink, opts).await?;
    #[cfg(test)]
    {
        anyhow::ensure!(d.app_control(&json!({"action":"ui_tree"})).await? == "mock ui_tree 1");
        {
            let mut state = d.st.lock();
            state.app_control_supported = false;
            state.server_version = "0.6.0".into();
        }
        let error = d.app_control(&json!({"action":"ui_tree"})).await.unwrap_err().to_string();
        anyhow::ensure!(error.contains("0.6.0") && error.contains("다시 시작") && !d.is_closed(), "{error}");
        d.st.lock().app_control_supported = true;
    }
    let shot = d.screenshot().await?;
    anyhow::ensure!(shot.starts_with(&[0x89, b'P', b'N', b'G']), "스크린샷이 PNG 가 아닙니다");
    d.click(Some((640.0, 400.0)), MouseButton::Left, 1, 0).await?;
    d.type_text("안녕").await?;
    let ev = mock.take();
    anyhow::ensure!(ev.iter().any(|e| e.starts_with("down0")) && ev.iter().any(|e| e == "type 안녕"), "입력 기록: {ev:?}");
    d.shutdown().await;
    srv.stop().await;
    Ok(())
}

#[cfg(all(test, windows))]
mod win_tests {
    #[tokio::test]
    async fn named_pipe_roundtrip() {
        super::pipe_roundtrip().await.unwrap();
    }
}

#[cfg(test)]
mod safety_tests {
    use super::*;
    use std::future::Future;
    use std::task::Poll;

    fn server(mock: Arc<MockPlatform>) -> Server {
        Server {
            platform: mock,
            cfg: ServerConfig { allow_uids: vec![], allow_on_console: false, label: "test".into(), log: quiet_log("test") },
            input_lock: tokio::sync::Mutex::new(()),
        }
    }

    #[tokio::test]
    async fn app_control_keeps_element_state_per_connection() {
        let srv = server(Arc::new(MockPlatform::new()));
        let mut a = ConnState::default();
        let mut b = ConnState::default();
        let request = json!({"op":"app_control", "input":{"action":"ui_tree"}});
        assert_eq!(srv.dispatch(&request, &mut a).await.unwrap()["text"], "mock ui_tree 1");
        assert_eq!(srv.dispatch(&request, &mut b).await.unwrap()["text"], "mock ui_tree 1");
        assert_eq!(srv.dispatch(&request, &mut a).await.unwrap()["text"], "mock ui_tree 2");
    }

    #[tokio::test]
    async fn helper_hello_reports_semantic_capabilities_and_app_version() {
        let srv = server(Arc::new(MockPlatform::new()));
        let hello = srv.dispatch(&json!({"op":"hello", "version":PROTO_VERSION}), &mut ConnState::default()).await.unwrap();
        assert_eq!(hello["server_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(hello["capabilities"]["app_control"], true);
    }

    #[tokio::test]
    async fn only_explicit_agent_screenshots_observe_input() {
        let mock = Arc::new(MockPlatform::new());
        let srv = server(mock.clone());
        srv.dispatch(&json!({"op":"screenshot", "max_long_edge":8, "format":"jpeg"}), &mut ConnState::default()).await.unwrap();
        assert_eq!(mock.observations.load(Ordering::SeqCst), 0);
        srv.dispatch(&json!({"op":"screenshot", "max_long_edge":8, "format":"png", "observe_input":true}), &mut ConnState::default()).await.unwrap();
        assert_eq!(mock.observations.load(Ordering::SeqCst), 1);
        srv.dispatch(&json!({"op":"screenshot", "max_long_edge":8, "format":"jpeg"}), &mut ConnState::default()).await.unwrap();
        assert_eq!(mock.observations.load(Ordering::SeqCst), 1, "live view must not acknowledge queued input");
    }

    #[tokio::test]
    async fn queued_inputs_recheck_console_before_sending() {
        for request in [
            json!({"op":"pointer", "action":"down"}),
            json!({"op":"scroll", "fw":1280, "fh":800, "dy":10}),
            json!({"op":"key", "combo":"a"}),
            json!({"op":"hold_key", "combo":"a", "secs":1}),
            json!({"op":"type", "text":"hello"}),
            json!({"op":"app_control", "input":{"action":"press", "id":1}}),
        ] {
            let mock = Arc::new(MockPlatform::new());
            let srv = server(mock.clone());
            let lock = srv.input_lock.lock().await;
            let mut cs = ConnState::default();
            let mut pending = Box::pin(srv.dispatch(&request, &mut cs));
            assert!(std::future::poll_fn(|cx| Poll::Ready(pending.as_mut().poll(cx))).await.is_pending());
            mock.console.store(true, Ordering::SeqCst);
            drop(lock);
            assert!(pending.await.is_err(), "{request}");
            assert!(mock.take().is_empty(), "{request}");
        }
    }

    #[tokio::test]
    async fn repeated_inputs_stop_when_session_becomes_console() {
        for request in [
            json!({"op":"key", "combo":"a", "repeat":3}),
            json!({"op":"type", "text":"hello\nworld"}),
            json!({"op":"hold_key", "combo":"a", "secs":1}),
        ] {
            let mock = Arc::new(MockPlatform::new());
            mock.console_after_input.store(true, Ordering::SeqCst);
            let srv = server(mock.clone());
            assert!(srv.dispatch(&request, &mut ConnState::default()).await.is_err(), "{request}");
            assert_eq!(mock.take().len(), 1, "{request}");
        }
    }

    #[tokio::test]
    async fn owned_mouse_button_can_be_released_on_console() {
        let mock = Arc::new(MockPlatform::new());
        mock.console.store(true, Ordering::SeqCst);
        let srv = server(mock.clone());
        let mut cs = ConnState::default();
        cs.pressed.push(0);
        srv.dispatch(&json!({"op":"pointer", "action":"up"}), &mut cs).await.unwrap();
        assert!(cs.pressed.is_empty());
        assert!(mock.take()[0].starts_with("up0 "));
    }

    #[test]
    fn hold_rechecks_console_while_waiting() {
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let result = hold_checked(Duration::from_secs(1), &|| {
            if checks.fetch_add(1, Ordering::SeqCst) > 0 {
                bail!("console switched");
            }
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(checks.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn explicit_console_override_still_allows_input() {
        let mock = Arc::new(MockPlatform::new());
        mock.console.store(true, Ordering::SeqCst);
        let mut srv = server(mock.clone());
        srv.cfg.allow_on_console = true;
        srv.dispatch(&json!({"op":"key", "combo":"a", "repeat":2}), &mut ConnState::default()).await.unwrap();
        assert_eq!(mock.take().len(), 2);
    }
}
