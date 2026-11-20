//! RFB (VNC) client backend: the "remote desktop" kind of isolated screen.
//!
//! Lets the agent drive a screen separate from the user's console (a VNC server in a separate
//! login session, a VM, Xvnc in a container, …). Connects over RFB, receives the framebuffer and
//! sends input only as PointerEvent/KeyEvent. The real OS cursor and keyboard are never touched.
//!
//! No extra crates: only the project's existing dependencies (tokio, anyhow, image,
//! parking_lot). (getrandom is used for OS randomness on Windows only; elsewhere it's
//! /dev/urandom.) DES for VNC auth and MD5 / AES-128-ECB / Diffie-Hellman (Montgomery modpow) for
//! ARD auth are small hand-rolled implementations at the bottom of this file, unit-tested against
//! FIPS/RFC test vectors.
//!
//! Supported:
//! - Versions: RFB 3.3 / 3.7 / 3.8 and Apple's "RFB 003.889" (replies 3.8 by default, like
//!   libvncclient; `ReplyVersion::Apple889` replies 3.889).
//! - Security types: None(1), VNC Authentication(2), Apple Remote Desktop(30).
//!   Selection order: None if offered → ARD with username+password → VNC with a password.
//! - Encodings: ZRLE(16), Raw(0), CopyRect(1), RRE(2), Hextile(5).
//!   Pseudo-encodings: DesktopSize(-223), LastRect(-224), Cursor(-239, optional),
//!   ExtendedDesktopSize(-308, parsed only).
//!   ZRLE is zlib-compressed (one stream for the whole connection) and is what keeps the picture small over the
//!   internet; Tight isn't implemented.
//! - Pixel format: always requests 32bpp/depth 24/little-endian/true-colour (R<<16|G<<8|B) via
//!   SetPixelFormat and keeps the internal framebuffer as RGBA8.
//!
//! Coordinates: the public API uses "agent coordinates". By default they equal framebuffer
//! pixels; with `RfbOptions::max_agent_size`, the framebuffer scaled down to fit inside it becomes
//! the agent coordinate space (to bring Retina/5K screens down to the resolution computer use
//! recommends).
//!
//! Keys: xdotool names → X11 keysyms. `super`/`cmd`/`meta`/`win` go out as Super_L (0xffeb) by
//! default. Whether a macOS server (Screen Sharing) maps Super_L or Meta_L (0xffe7) to Command
//! hasn't been verified against a real server; switch `RfbOptions::cmd_keysym` to
//! `KEYSYM_META_L` if needed.
//!
//! Korean and other non-Latin-1 text: `type_text` sends Unicode keysyms (0x01000000 + code
//! point). Xvnc/TigerVNC generally accept them, but macOS Screen Sharing may ignore them
//! (unverified). We don't quietly fall back to pasting through the clipboard.

#![allow(dead_code)]

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Weak,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream,
    },
    sync::watch,
    task::JoinHandle,
};

use crate::browser::MouseButton;
use crate::desk::ViewInfo;

/// Called whenever the framebuffer changes (to request a UI repaint). Runs on the reader task, so
/// keep it cheap.
pub type FrameCallback = Arc<dyn Fn() + Send + Sync>;

// same modifier bits as keys.rs (CDP modifiers convention)
const MOD_ALT: i64 = 1;
const MOD_CTRL: i64 = 2;
const MOD_META: i64 = 4;
const MOD_SHIFT: i64 = 8;

pub const KEYSYM_SHIFT_L: u32 = 0xffe1;
pub const KEYSYM_CONTROL_L: u32 = 0xffe3;
pub const KEYSYM_META_L: u32 = 0xffe7;
pub const KEYSYM_ALT_L: u32 = 0xffe9;
pub const KEYSYM_SUPER_L: u32 = 0xffeb;

pub const SEC_NONE: u8 = 1;
pub const SEC_VNC: u8 = 2;
pub const SEC_ARD: u8 = 30;

const ENC_RAW: i32 = 0;
const ENC_COPYRECT: i32 = 1;
const ENC_RRE: i32 = 2;
const ENC_HEXTILE: i32 = 5;
const ENC_ZRLE: i32 = 16;
const ENC_DESKTOP_SIZE: i32 = -223;
const ENC_LAST_RECT: i32 = -224;
const ENC_CURSOR: i32 = -239;
const ENC_EXT_DESKTOP_SIZE: i32 = -308;

/// Which version to reply to a 3.889 (Apple) server with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ReplyVersion {
    /// Reply "RFB 003.008" even to 3.889 servers (like libvncclient; the standard flow).
    Auto,
    /// Reply "RFB 003.889" as is when the server is 3.889.
    Apple889,
}

#[derive(Clone, Debug)]
pub struct RfbOptions {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// ClientInit shared-flag (true = don't disconnect other clients).
    pub shared: bool,
    pub reply_version: ReplyVersion,
    /// Maximum agent coordinate size (None = framebuffer as is).
    pub max_agent_size: Option<(u32, u32)>,
    /// Whether to request the Cursor pseudo-encoding. When on, the server sends the cursor shape
    /// separately instead of drawing it into the framebuffer, and screenshots composite it at the
    /// current mouse position.
    pub cursor_pseudo: bool,
    /// keysym sent for Command (⌘)/super (Super_L by default).
    pub cmd_keysym: u32,
    pub connect_timeout: Duration,
    /// Delay between consecutive key presses.
    pub key_delay: Duration,
    /// Some(reason): if the peer address of the actual TCP connection is this computer itself, refuse
    /// with this reason before exchanging anything.
    ///
    /// Checked on the connected socket, since resolving the host name separately could be bypassed
    /// by a changing DNS answer.
    pub refuse_local: Option<String>,
}

impl RfbOptions {
    pub fn new(host: &str, port: u16) -> Self {
        RfbOptions {
            host: host.to_string(),
            port,
            username: None,
            password: None,
            shared: true,
            reply_version: ReplyVersion::Auto,
            max_agent_size: None,
            cursor_pseudo: true,
            cmd_keysym: KEYSYM_SUPER_L,
            connect_timeout: Duration::from_secs(10),
            key_delay: Duration::from_millis(6),
            refuse_local: None,
        }
    }
}

/// Result of a probe that only reads the version and security types, then disconnects.
#[derive(Clone, Debug)]
pub struct Greeting {
    /// ProtocolVersion string as sent by the server (without the newline).
    pub server_version: String,
    /// Version string we replied with.
    pub reply_version: String,
    /// 3.7+: security types offered by the server. 3.3: the single type the server picked.
    pub security_types: Vec<u8>,
    /// Reason sent by the server when it offers zero security types.
    pub failure_reason: Option<String>,
    /// Peer address actually connected to (for decisions based on this connection rather than
    /// resolving the host name again).
    pub peer: Option<std::net::SocketAddr>,
}

pub fn security_type_name(t: u8) -> &'static str {
    match t {
        0 => "Invalid",
        1 => "None",
        2 => "VNC Authentication",
        5 => "RA2",
        6 => "RA2ne",
        16 => "Tight",
        17 => "Ultra",
        18 => "TLS",
        19 => "VeNCrypt",
        20 => "GTK-VNC SASL",
        21 => "MD5 hash",
        22 => "Colin Dean xvp",
        30 => "Apple Remote Desktop (DH)",
        33 => "Apple proprietary (33, unsupported)",
        35 => "Apple proprietary (35, unsupported)",
        36 => "Apple proprietary (36, unsupported)",
        113 => "MSLogonII",
        _ => "unknown",
    }
}

struct Framebuffer {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}

/// Upper bound on the framebuffer size the server announces (8192x8192 = 2^26 pixels, 256 MiB of
/// RGBA).
///
/// ServerInit/DesktopSize are u16, so up to 65535x65535 can arrive; accepting that as is would
/// allocate 17 GiB and actually consume it by writing every pixel's alpha.
const MAX_FB_PIXELS: u64 = 1 << 26;

fn check_fb_dims(w: u32, h: u32) -> Result<()> {
    if w as u64 * h as u64 > MAX_FB_PIXELS {
        bail!("원격 프레임버퍼 크기가 비정상적으로 큼: {w}x{h} (상한 {MAX_FB_PIXELS} 픽셀)");
    }
    Ok(())
}

impl Framebuffer {
    fn new(w: u32, h: u32) -> Self {
        let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Framebuffer { w, h, rgba }
    }

    fn check(&self, x: u32, y: u32, w: u32, h: u32) -> Result<()> {
        if x as u64 + w as u64 > self.w as u64 || y as u64 + h as u64 > self.h as u64 {
            bail!("사각형이 프레임버퍼 밖: {x},{y} {w}x{h} (fb {}x{})", self.w, self.h);
        }
        Ok(())
    }

    /// Writes 32bpp little-endian (B,G,R,X) pixel data into the (x,y,w,h) region.
    fn put_bgrx(&mut self, x: u32, y: u32, w: u32, h: u32, data: &[u8]) {
        let stride = self.w as usize * 4;
        for row in 0..h as usize {
            let dst0 = (y as usize + row) * stride + x as usize * 4;
            let src0 = row * w as usize * 4;
            for col in 0..w as usize {
                let s = &data[src0 + col * 4..src0 + col * 4 + 4];
                let d = &mut self.rgba[dst0 + col * 4..dst0 + col * 4 + 4];
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = 255;
            }
        }
    }

    fn fill(&mut self, x: u32, y: u32, w: u32, h: u32, rgba: [u8; 4]) {
        let stride = self.w as usize * 4;
        for row in 0..h as usize {
            let dst0 = (y as usize + row) * stride + x as usize * 4;
            for col in 0..w as usize {
                self.rgba[dst0 + col * 4..dst0 + col * 4 + 4].copy_from_slice(&rgba);
            }
        }
    }

    fn copy_rect(&mut self, sx: u32, sy: u32, dx: u32, dy: u32, w: u32, h: u32) {
        let stride = self.w as usize * 4;
        let rowlen = w as usize * 4;
        let mut tmp = vec![0u8; rowlen * h as usize];
        for row in 0..h as usize {
            let s = (sy as usize + row) * stride + sx as usize * 4;
            tmp[row * rowlen..(row + 1) * rowlen].copy_from_slice(&self.rgba[s..s + rowlen]);
        }
        for row in 0..h as usize {
            let d = (dy as usize + row) * stride + dx as usize * 4;
            self.rgba[d..d + rowlen].copy_from_slice(&tmp[row * rowlen..(row + 1) * rowlen]);
        }
    }
}

#[derive(Clone)]
struct CursorShape {
    hot_x: u32,
    hot_y: u32,
    w: u32,
    h: u32,
    /// RGBA; alpha 0 outside the mask.
    rgba: Vec<u8>,
}

#[derive(Default)]
struct InputState {
    /// Agent coordinates.
    mouse: (f64, f64),
    /// RFB button mask (bit0 left, bit1 middle, bit2 right).
    buttons: u8,
    /// Modifiers held down along with mouse actions (MOD_* bits).
    held_mods: i64,
}

pub struct RfbDesk {
    opts: RfbOptions,
    name: Mutex<String>,
    server_version: String,
    security_type: u8,
    fb: Mutex<Framebuffer>,
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    input: Mutex<InputState>,
    closed: AtomicBool,
    /// Number of completed FramebufferUpdates.
    seq_tx: watch::Sender<u64>,
    /// Total area of pixel rectangles received since the last refresh request.
    coverage: AtomicU64,
    notes: Mutex<Vec<String>>,
    on_frame: FrameCallback,
    reader: Mutex<Option<JoinHandle<()>>>,
    cursor: Mutex<Option<CursorShape>>,
    server_clipboard: Mutex<Option<String>>,
    last_request: Mutex<Instant>,
    /// The whole picture was received at least once, so later screenshots can use the framebuffer as it is
    /// (kept current by incremental updates) instead of asking the server for the entire screen again.
    synced: AtomicBool,
    /// When the last input went out and has not been answered by a picture yet.
    input_at: Mutex<Option<Instant>>,
    /// Smoothed time from an input to the next picture update (ms): the link's round trip plus the server's reaction.
    lag_ms: AtomicU64,
}

/// Cleans up what people type as a VNC address: `vnc://host:5901/`, `host:5901`, `[fe80::1]:5900`, spaces.
/// A port written in the address wins over the separate port field.
pub fn normalize_endpoint(host: &str, port: u16) -> (String, u16) {
    let mut h = host.trim();
    for scheme in ["vnc://", "rfb://", "tcp://", "http://", "https://"] {
        if h.len() >= scheme.len() && h[..scheme.len()].eq_ignore_ascii_case(scheme) {
            h = &h[scheme.len()..];
        }
    }
    let h = h.split(['/', '?', '#']).next().unwrap_or(h).trim();
    let h = h.rsplit_once('@').map_or(h, |(_, rest)| rest); // user@host
    if let Some(rest) = h.strip_prefix('[') {
        // [v6]:port
        if let Some((addr, tail)) = rest.split_once(']') {
            let p = tail.strip_prefix(':').and_then(|p| p.parse().ok()).unwrap_or(port);
            return (addr.to_string(), p);
        }
    }
    // exactly one ':' means host:port (more means a bare IPv6 address)
    if h.matches(':').count() == 1 {
        if let Some((a, p)) = h.split_once(':') {
            if let Ok(p) = p.parse::<u16>() {
                return (a.to_string(), p);
            }
            return (a.to_string(), port);
        }
    }
    (h.to_string(), port)
}

/// Opens the TCP connection: every address the name resolves to is tried in turn (IPv4 first, since many VNC
/// servers listen on IPv4 only), each with its own time limit, and a failure says what likely went wrong.
pub async fn connect_tcp(host: &str, port: u16) -> Result<TcpStream> {
    let (host, port) = normalize_endpoint(host, port);
    let mut addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| anyhow!("'{host}' 주소를 찾지 못했어요 ({e}). 이름이나 IP 를 확인하세요"))?
        .collect();
    if addrs.is_empty() {
        bail!("'{host}' 주소를 찾지 못했어요");
    }
    addrs.sort_by_key(|a| a.is_ipv6());
    let mut last = String::new();
    let mut hint = "";
    for a in addrs {
        match tokio::time::timeout(Duration::from_secs(6), TcpStream::connect(a)).await {
            Ok(Ok(s)) => return Ok(s),
            Ok(Err(e)) => {
                hint = match e.kind() {
                    std::io::ErrorKind::ConnectionRefused => {
                        "상대 컴퓨터는 응답했지만 그 포트에서 VNC 서버가 듣고 있지 않아요. VNC(화면 공유) 서버가 켜져 있는지, 포트(보통 5900)가 맞는지 확인하세요"
                    }
                    _ if cfg!(target_os = "macos") && e.raw_os_error() == Some(65) => {
                        "macOS 의 '로컬 네트워크' 권한이 막혀 있을 수 있어요. 시스템 설정 > 개인정보 보호 및 보안 > 로컬 네트워크에서 GhostDesk 를 허용하세요"
                    }
                    _ => "상대 컴퓨터가 켜져 있고 같은 네트워크(또는 VPN)에 있는지, 방화벽이 포트를 막지 않는지 확인하세요",
                };
                last = format!("{a}: {e}");
            }
            Err(_) => {
                hint = "응답이 없어요. 주소가 맞는지, 상대 컴퓨터가 켜져 있는지, 방화벽이 막고 있지 않은지 확인하세요";
                last = format!("{a}: 시간 초과");
            }
        }
    }
    bail!("{host}:{port} TCP 접속 실패 ({last}). {hint}")
}

/// Connects with default options.
pub async fn connect(
    host: &str,
    port: u16,
    username: Option<String>,
    password: Option<String>,
    on_frame: FrameCallback,
) -> Result<Arc<RfbDesk>> {
    let mut o = RfbOptions::new(host, port);
    o.username = username;
    o.password = password;
    RfbDesk::connect_with(o, on_frame).await
}

/// Reads only the version string and security type list, then disconnects right away (no auth
/// attempt, no input sent).
pub async fn probe(host: &str, port: u16, reply: ReplyVersion, timeout: Duration) -> Result<Greeting> {
    let fut = async {
        let mut s = connect_tcp(host, port).await?;
        let peer = s.peer_addr().ok();
        let (server_version, minor, reply_str) = read_version(&mut s, reply).await?;
        s.write_all(reply_str.as_bytes()).await?;
        let mut g = Greeting {
            server_version,
            reply_version: reply_str.trim_end().to_string(),
            security_types: vec![],
            failure_reason: None,
            peer,
        };
        if minor < 7 {
            let t = read_u32(&mut s).await?;
            if t == 0 {
                g.failure_reason = Some(read_string(&mut s).await?);
            } else {
                g.security_types.push(t.min(255) as u8);
            }
        } else {
            let n = read_u8(&mut s).await?;
            if n == 0 {
                g.failure_reason = Some(read_string(&mut s).await?);
            } else {
                let mut types = vec![0u8; n as usize];
                s.read_exact(&mut types).await?;
                g.security_types = types;
            }
        }
        let _ = s.shutdown().await;
        Ok::<_, anyhow::Error>(g)
    };
    tokio::time::timeout(timeout, fut).await.map_err(|_| anyhow!("probe 시간 초과"))?
}

/// Whether `host` points at this computer itself (loopback/unspecified, or an address of one of
/// this computer's network interfaces).
///
/// Connecting to this computer's own VNC server would let the agent drive the user's real screen,
/// so callers block it.
pub async fn is_local_host(host: &str, port: u16) -> bool {
    let (host, port) = normalize_endpoint(host, port);
    let Ok(addrs) = tokio::net::lookup_host((host.as_str(), port)).await else { return false };
    let local = addrs.into_iter().any(is_local_addr);
    local
}

/// With `refuse_local`, refuses with that reason when the connected socket's peer is this
/// computer itself.
///
/// If the peer address is unknown it can't be checked, so it's refused.
fn check_peer(s: &TcpStream, refuse_local: Option<&str>) -> Result<()> {
    let Some(why) = refuse_local else { return Ok(()) };
    match s.peer_addr() {
        Ok(a) if !is_local_addr(a) => Ok(()),
        Ok(_) => Err(anyhow!("{why}")),
        Err(e) => Err(anyhow!("접속한 상대 주소를 확인하지 못했습니다: {e}")),
    }
}

/// Whether an address belongs to this computer.
///
/// Instead of reading the interface list (/sbin/ifconfig, GetAdaptersAddresses, …), it tries to
/// bind a UDP socket to the address. The OS only allows binding to addresses assigned to its own
/// interfaces (EADDRNOTAVAIL / WSAEADDRNOTAVAIL), so macOS, Windows and Linux are judged the same
/// way with no extra process, privileges or crates. (It only binds and closes right away; no
/// packet is sent. IPv6 link-local addresses keep their scope id.)
pub fn is_local_addr(addr: std::net::SocketAddr) -> bool {
    use std::net::{IpAddr, SocketAddr};
    let ip = addr.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    // ::ffff:a.b.c.d counts as an IPv4 address
    if let IpAddr::V6(v6) = ip {
        if let Some(v4) = v6.to_ipv4_mapped() {
            return is_local_addr(SocketAddr::new(IpAddr::V4(v4), 0));
        }
    }
    // multicast and broadcast addresses aren't a specific computer (even if some OSes allow the bind)
    if ip.is_multicast() || matches!(ip, IpAddr::V4(v4) if v4.is_broadcast()) {
        return false;
    }
    let mut probe = addr;
    probe.set_port(0);
    std::net::UdpSocket::bind(probe).is_ok()
}

impl RfbDesk {
    pub async fn connect_with(mut opts: RfbOptions, on_frame: FrameCallback) -> Result<Arc<RfbDesk>> {
        let (h, p) = normalize_endpoint(&opts.host, opts.port);
        (opts.host, opts.port) = (h, p);
        let timeout = opts.connect_timeout;
        let desk = tokio::time::timeout(timeout, Self::connect_inner(opts, on_frame))
            .await
            .map_err(|_| anyhow!("RFB 접속/인증 시간 초과"))??;
        // wait briefly for the first frame (carry on if it doesn't come). This has to be outside the
        // connect timeout, so a slow handshake doesn't throw away an already successful connection as
        // "timed out".
        let _ = desk.wait_coverage(Duration::from_secs(3)).await;
        Ok(desk)
    }

    async fn connect_inner(opts: RfbOptions, on_frame: FrameCallback) -> Result<Arc<RfbDesk>> {
        let mut s = connect_tcp(&opts.host, opts.port).await?;
        check_peer(&s, opts.refuse_local.as_deref())?;
        let _ = s.set_nodelay(true);

        // ── ProtocolVersion
        let (server_version, minor, reply) = read_version(&mut s, opts.reply_version).await?;
        s.write_all(reply.as_bytes()).await?;

        // ── Security
        let sec = negotiate_security(&mut s, minor, &opts).await?;

        // ── ClientInit / ServerInit
        s.write_all(&[opts.shared as u8]).await?;
        let w = read_u16(&mut s).await? as u32;
        let h = read_u16(&mut s).await? as u32;
        check_fb_dims(w, h)?;
        let mut pf = [0u8; 16];
        s.read_exact(&mut pf).await?;
        let name_len = read_u32(&mut s).await? as usize;
        if name_len > 1 << 20 {
            bail!("ServerInit 이름 길이가 비정상: {name_len}");
        }
        let mut name = vec![0u8; name_len];
        s.read_exact(&mut name).await?;
        let name = String::from_utf8_lossy(&name).to_string();

        // ── SetPixelFormat: 32bpp, depth 24, little-endian, true colour, R<<16 G<<8 B
        let mut msg = vec![0u8, 0, 0, 0];
        msg.extend_from_slice(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
        // ── SetEncodings
        let mut encs = vec![ENC_ZRLE, ENC_COPYRECT, ENC_HEXTILE, ENC_RRE, ENC_RAW, ENC_DESKTOP_SIZE, ENC_LAST_RECT];
        if opts.cursor_pseudo {
            encs.push(ENC_CURSOR);
        }
        msg.extend_from_slice(&[2, 0]);
        msg.extend_from_slice(&(encs.len() as u16).to_be_bytes());
        for e in &encs {
            msg.extend_from_slice(&e.to_be_bytes());
        }
        // ---- initial full-screen request ----
        msg.extend_from_slice(&fb_request(false, 0, 0, w as u16, h as u16));
        s.write_all(&msg).await?;

        let (rd, wr) = s.into_split();
        let (seq_tx, _) = watch::channel(0u64);
        let desk = Arc::new(RfbDesk {
            name: Mutex::new(name),
            server_version,
            security_type: sec,
            fb: Mutex::new(Framebuffer::new(w, h)),
            writer: tokio::sync::Mutex::new(wr),
            input: Mutex::new(InputState::default()),
            closed: AtomicBool::new(false),
            seq_tx,
            coverage: AtomicU64::new(0),
            notes: Mutex::new(Vec::new()),
            on_frame,
            reader: Mutex::new(None),
            cursor: Mutex::new(None),
            server_clipboard: Mutex::new(None),
            last_request: Mutex::new(Instant::now()),
            synced: AtomicBool::new(false),
            input_at: Mutex::new(None),
            lag_ms: AtomicU64::new(0),
            opts,
        });
        let weak = Arc::downgrade(&desk);
        let handle = tokio::spawn(reader_loop(weak, rd));
        *desk.reader.lock() = Some(handle);
        Ok(desk)
    }

    // ---- queries ----

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn desktop_name(&self) -> String {
        self.name.lock().clone()
    }

    pub fn server_version(&self) -> &str {
        &self.server_version
    }

    pub fn security_type(&self) -> u8 {
        self.security_type
    }

    /// Actual framebuffer size.
    pub fn fb_size(&self) -> (u32, u32) {
        let fb = self.fb.lock();
        (fb.w, fb.h)
    }

    /// Agent coordinate space size (= screenshot PNG size).
    pub fn size(&self) -> (u32, u32) {
        let (w, h) = self.fb_size();
        agent_size(w, h, self.opts.max_agent_size)
    }

    /// Notification channel that ticks on every completed FramebufferUpdate.
    pub fn subscribe_frames(&self) -> watch::Receiver<u64> {
        self.seq_tx.subscribe()
    }

    /// Number of completed FramebufferUpdates (lets the UI decide whether to refresh its texture).
    pub fn frame_seq(&self) -> u64 {
        *self.seq_tx.borrow()
    }

    /// Gives access to the current framebuffer (RGBA8, framebuffer resolution).
    pub fn with_frame<R>(&self, f: impl FnOnce(u32, u32, &[u8]) -> R) -> R {
        let fb = self.fb.lock();
        f(fb.w, fb.h, &fb.rgba)
    }

    /// The last ServerCutText sent by the server (decoded as Latin-1).
    pub fn server_clipboard(&self) -> Option<String> {
        self.server_clipboard.lock().clone()
    }

    pub fn views(&self) -> (Vec<ViewInfo>, Option<String>) {
        let (w, h) = self.fb_size();
        let v = ViewInfo {
            id: "desktop".into(),
            title: self.desktop_name(),
            detail: format!("vnc://{}:{} {}x{}", self.opts.host, self.opts.port, w, h),
        };
        (vec![v], Some("desktop".into()))
    }

    pub async fn switch_view(&self, index: usize) -> Result<()> {
        if index == 0 {
            Ok(())
        } else {
            bail!("원격 데스크톱에는 뷰가 하나뿐입니다 (index 0)")
        }
    }

    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.notes.lock())
    }

    fn note(&self, s: impl Into<String>) {
        self.notes.lock().push(s.into());
    }

    pub fn mouse_pos(&self) -> (f64, f64) {
        self.input.lock().mouse
    }

    // ---- screen ----

    /// Waits until the next FramebufferUpdate completes. Returns true if an update came.
    pub async fn wait_next_update(&self, timeout: Duration) -> bool {
        let mut rx = self.seq_tx.subscribe();
        rx.borrow_and_update();
        matches!(tokio::time::timeout(timeout, rx.changed()).await, Ok(Ok(())))
    }

    /// Sends a non-incremental full-screen request and waits until the area received since then
    /// covers the whole screen (or until the timeout). Returns true if the whole screen was updated.
    pub async fn refresh(&self, timeout: Duration) -> Result<bool> {
        self.coverage.store(0, Ordering::SeqCst);
        let (w, h) = self.fb_size();
        self.send(&fb_request(false, 0, 0, w as u16, h as u16)).await?;
        Ok(self.wait_coverage(timeout).await)
    }

    /// Makes the framebuffer current before a screenshot. Once the whole picture has been received, incremental
    /// updates keep it current, so nothing is asked for again: wait only until the changes caused by the last action
    /// have arrived (no new update for a moment). Before that, or after a size change, ask for the whole screen.
    async fn fresh_frame(&self) -> Result<()> {
        if !self.synced.load(Ordering::SeqCst) {
            let _ = self.refresh(Duration::from_millis(1500)).await?;
            return Ok(());
        }
        self.quiet(self.settle_window(Duration::from_millis(60)), Duration::from_millis(700)).await;
        Ok(())
    }

    /// Waits until no FramebufferUpdate has arrived for `quiet`, at most `max` (animations never go quiet).
    pub async fn quiet(&self, quiet: Duration, max: Duration) {
        let start = tokio::time::Instant::now();
        let mut rx = self.seq_tx.subscribe();
        rx.borrow_and_update();
        loop {
            let left = max.saturating_sub(start.elapsed());
            if left.is_zero() || self.is_closed() {
                return;
            }
            match tokio::time::timeout(quiet.min(left), rx.changed()).await {
                Ok(Ok(())) => continue,
                _ => return,
            }
        }
    }

    async fn wait_coverage(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut rx = self.seq_tx.subscribe();
        loop {
            let (w, h) = self.fb_size();
            if self.coverage.load(Ordering::SeqCst) >= w as u64 * h as u64 {
                return true;
            }
            if self.is_closed() {
                return false;
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => continue,
                _ => {
                    let (w, h) = self.fb_size();
                    return self.coverage.load(Ordering::SeqCst) >= w as u64 * h as u64;
                }
            }
        }
    }

    /// PNG of the current framebuffer (agent coordinate size). Requests a fresh full frame first.
    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        if self.is_closed() {
            bail!("RFB 연결이 끊어졌습니다");
        }
        self.fresh_frame().await?;
        if self.fb_size().0 == 0 || self.fb_size().1 == 0 {
            bail!("원격 프레임버퍼 크기가 0 입니다");
        }
        let img = self.composite_rgb();
        let (aw, ah) = self.size();
        let img = if img.width() != aw || img.height() != ah {
            image::imageops::resize(&img, aw, ah, image::imageops::FilterType::Triangle)
        } else {
            img
        };
        encode_png_rgb(img)
    }

    /// Zoomed-in PNG of a region. region = [x0,y0,x1,y1] (agent coordinates).
    pub async fn zoom(&self, region: [f64; 4]) -> Result<Vec<u8>> {
        if self.is_closed() {
            bail!("RFB 연결이 끊어졌습니다");
        }
        self.fresh_frame().await?;
        let (aw, ah) = self.size();
        let (fw, fh) = self.fb_size();
        if fw == 0 || fh == 0 {
            // keeps the clamp(fx0 + 1, fw) below from panicking with min > max
            bail!("원격 프레임버퍼 크기가 0 입니다");
        }
        let sx = fw as f64 / aw as f64;
        let sy = fh as f64 / ah as f64;
        let (x0, y0) = (region[0].min(region[2]), region[1].min(region[3]));
        let (x1, y1) = (region[0].max(region[2]), region[1].max(region[3]));
        let fx0 = ((x0 * sx).floor().max(0.0) as u32).min(fw.saturating_sub(1));
        let fy0 = ((y0 * sy).floor().max(0.0) as u32).min(fh.saturating_sub(1));
        let fx1 = ((x1 * sx).ceil() as u32).clamp(fx0 + 1, fw);
        let fy1 = ((y1 * sy).ceil() as u32).clamp(fy0 + 1, fh);
        let img = self.composite_rgb();
        let crop = image::imageops::crop_imm(&img, fx0, fy0, fx1 - fx0, fy1 - fy0).to_image();
        // scale up to the agent screen size (keeping the aspect ratio, 1-8x)
        let (cw, ch) = ((x1 - x0).max(1.0), (y1 - y0).max(1.0));
        let scale = (aw as f64 / cw).min(ah as f64 / ch).clamp(1.0, 8.0);
        let tw = ((cw * scale).round() as u32).max(1);
        let th = ((ch * scale).round() as u32).max(1);
        let out = if (tw, th) != (crop.width(), crop.height()) {
            image::imageops::resize(&crop, tw, th, image::imageops::FilterType::CatmullRom)
        } else {
            crop
        };
        encode_png_rgb(out)
    }

    /// RGB image of the framebuffer with the cursor composited in (if the Cursor pseudo-encoding was
    /// received), at framebuffer resolution.
    fn composite_rgb(&self) -> image::RgbImage {
        let (w, h, mut rgb) = {
            let fb = self.fb.lock();
            let mut rgb = Vec::with_capacity(fb.rgba.len() / 4 * 3);
            for px in fb.rgba.chunks_exact(4) {
                rgb.extend_from_slice(&px[..3]);
            }
            (fb.w, fb.h, rgb)
        };
        if let Some(c) = self.cursor.lock().clone() {
            let (mx, my) = self.to_fb(self.mouse_pos());
            let ox = mx as i64 - c.hot_x as i64;
            let oy = my as i64 - c.hot_y as i64;
            for cy in 0..c.h as i64 {
                for cx in 0..c.w as i64 {
                    let (px, py) = (ox + cx, oy + cy);
                    if px < 0 || py < 0 || px >= w as i64 || py >= h as i64 {
                        continue;
                    }
                    let si = ((cy * c.w as i64 + cx) * 4) as usize;
                    if c.rgba[si + 3] == 0 {
                        continue;
                    }
                    let di = ((py * w as i64 + px) * 3) as usize;
                    rgb[di..di + 3].copy_from_slice(&c.rgba[si..si + 3]);
                }
            }
        }
        image::RgbImage::from_raw(w, h, rgb).expect("fb 크기 불일치")
    }

    // ---- coordinates ----

    fn to_fb(&self, (x, y): (f64, f64)) -> (u16, u16) {
        let (fw, fh) = self.fb_size();
        let (aw, ah) = agent_size(fw, fh, self.opts.max_agent_size);
        let fx = (x * fw as f64 / aw as f64).round().clamp(0.0, fw.saturating_sub(1) as f64);
        let fy = (y * fh as f64 / ah as f64).round().clamp(0.0, fh.saturating_sub(1) as f64);
        (fx as u16, fy as u16)
    }

    fn clamp_agent(&self, x: f64, y: f64) -> (f64, f64) {
        let (aw, ah) = self.size();
        (x.clamp(0.0, aw.saturating_sub(1) as f64), y.clamp(0.0, ah.saturating_sub(1) as f64))
    }

    // ---- sending ----

    async fn send(&self, data: &[u8]) -> Result<()> {
        if self.is_closed() {
            bail!("RFB 연결이 끊어졌습니다");
        }
        let mut w = self.writer.lock().await;
        w.write_all(data).await.context("RFB 전송 실패")?;
        Ok(())
    }

    fn mark_input(&self) {
        let mut at = self.input_at.lock();
        at.get_or_insert_with(Instant::now);
    }

    /// A picture update arrived: if an input was waiting for one, that is a sample of the link's lag.
    fn note_update(&self) {
        if let Some(t) = self.input_at.lock().take() {
            let sample = (t.elapsed().as_millis() as u64).min(1500);
            let old = self.lag_ms.load(Ordering::Relaxed);
            self.lag_ms.store(if old == 0 { sample } else { (old * 3 + sample) / 4 }, Ordering::Relaxed);
        }
    }

    /// How long to wait for a picture to stop changing: at least `base`, longer on a slow link (a quick wait would
    /// return before the answer to the input has even arrived).
    pub fn settle_window(&self, base: Duration) -> Duration {
        let lag = Duration::from_millis(self.lag_ms.load(Ordering::Relaxed) * 3 / 2);
        base.max(lag).min(Duration::from_millis(600))
    }

    async fn send_pointer(&self) -> Result<()> {
        let (pos, mask) = {
            let st = self.input.lock();
            (st.mouse, st.buttons)
        };
        self.mark_input();
        let (x, y) = self.to_fb(pos);
        let mut m = [5u8, mask, 0, 0, 0, 0];
        m[2..4].copy_from_slice(&x.to_be_bytes());
        m[4..6].copy_from_slice(&y.to_be_bytes());
        self.send(&m).await
    }

    async fn send_key(&self, keysym: u32, down: bool) -> Result<()> {
        self.mark_input();
        let mut m = [4u8, down as u8, 0, 0, 0, 0, 0, 0];
        m[4..8].copy_from_slice(&keysym.to_be_bytes());
        self.send(&m).await
    }

    fn mod_keysyms(&self, mods: i64) -> Vec<(i64, u32)> {
        let mut v = Vec::new();
        if mods & MOD_CTRL != 0 {
            v.push((MOD_CTRL, KEYSYM_CONTROL_L));
        }
        if mods & MOD_ALT != 0 {
            v.push((MOD_ALT, KEYSYM_ALT_L));
        }
        if mods & MOD_SHIFT != 0 {
            v.push((MOD_SHIFT, KEYSYM_SHIFT_L));
        }
        if mods & MOD_META != 0 {
            v.push((MOD_META, self.opts.cmd_keysym));
        }
        v
    }

    /// Brings the modifier state for mouse actions to `mods` (sending down/up only for the
    /// differences).
    async fn set_mods(&self, mods: i64) -> Result<()> {
        let held = self.input.lock().held_mods;
        if held == mods {
            return Ok(());
        }
        for (bit, ks) in self.mod_keysyms(held & !mods).into_iter().rev() {
            self.send_key(ks, false).await?;
            self.input.lock().held_mods &= !bit;
        }
        for (bit, ks) in self.mod_keysyms(mods & !held) {
            self.send_key(ks, true).await?;
            self.input.lock().held_mods |= bit;
        }
        Ok(())
    }

    // ---- mouse ----

    pub async fn mouse_move(&self, x: f64, y: f64, mods: i64) -> Result<()> {
        self.set_mods(mods).await?;
        let p = self.clamp_agent(x, y);
        self.input.lock().mouse = p;
        self.send_pointer().await
    }

    pub async fn mouse_down(&self, button: MouseButton, _count: i64, mods: i64) -> Result<()> {
        self.set_mods(mods).await?;
        self.input.lock().buttons |= button_bit(button);
        self.send_pointer().await
    }

    pub async fn mouse_up(&self, button: MouseButton, _count: i64, mods: i64) -> Result<()> {
        self.input.lock().buttons &= !button_bit(button);
        self.send_pointer().await?;
        if mods == 0 {
            self.set_mods(0).await?;
        }
        Ok(())
    }

    pub async fn click(&self, pos: Option<(f64, f64)>, button: MouseButton, count: u32, mods: i64) -> Result<()> {
        self.set_mods(mods).await?;
        if let Some((x, y)) = pos {
            self.mouse_move(x, y, mods).await?;
        }
        for i in 0..count.clamp(1, 5) {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            self.mouse_down(button, i as i64 + 1, mods).await?;
            tokio::time::sleep(Duration::from_millis(15)).await;
            self.mouse_up(button, i as i64 + 1, mods).await?;
        }
        self.set_mods(0).await
    }

    pub async fn drag(&self, from: (f64, f64), to: (f64, f64), mods: i64) -> Result<()> {
        self.mouse_move(from.0, from.1, mods).await?;
        self.mouse_down(MouseButton::Left, 1, mods).await?;
        let steps = 12;
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let x = from.0 + (to.0 - from.0) * t;
            let y = from.1 + (to.1 - from.1) * t;
            tokio::time::sleep(Duration::from_millis(16)).await;
            self.mouse_move(x, y, mods).await?;
        }
        self.mouse_up(MouseButton::Left, 1, mods).await?;
        self.set_mods(0).await
    }

    /// dx/dy follow the same "pixel delta" convention as browser.rs (the agent passes
    /// scroll_amount * 100).
    ///
    /// Every 100 units is one press+release of a wheel button (4=up, 5=down, 6=left, 7=right).
    pub async fn scroll(&self, pos: Option<(f64, f64)>, dx: f64, dy: f64, mods: i64) -> Result<()> {
        self.set_mods(mods).await?;
        if let Some((x, y)) = pos {
            self.mouse_move(x, y, mods).await?;
        }
        let steps = |d: f64| if d == 0.0 { 0 } else { ((d.abs() / 100.0).round() as u32).clamp(1, 50) };
        let mut plan: Vec<(u8, u32)> = Vec::new();
        if dy != 0.0 {
            plan.push((if dy < 0.0 { 8 } else { 16 }, steps(dy)));
        }
        if dx != 0.0 {
            plan.push((if dx < 0.0 { 32 } else { 64 }, steps(dx)));
        }
        for (bit, n) in plan {
            for _ in 0..n {
                self.input.lock().buttons |= bit;
                self.send_pointer().await?;
                self.input.lock().buttons &= !bit;
                self.send_pointer().await?;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        self.set_mods(0).await
    }

    // ---- keyboard ----

    /// Parses an xdotool-style combo into (modifier keysyms, main keysym).
    pub fn parse_combo(&self, combo: &str) -> Result<(Vec<u32>, u32)> {
        let combo = combo.trim();
        let parts: Vec<String> = if combo.ends_with("++") {
            let mut v: Vec<String> = combo[..combo.len() - 2]
                .split('+')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            v.push("+".into());
            v
        } else if combo == "+" {
            vec!["+".into()]
        } else {
            combo.split('+').map(|s| s.trim().to_string()).collect()
        };
        if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
            bail!("잘못된 키 조합: {combo:?}");
        }
        let mut syms = Vec::new();
        for p in &parts {
            syms.push(keysym_for(p, self.opts.cmd_keysym).ok_or_else(|| anyhow!("알 수 없는 키 이름: {p}"))?);
        }
        let mut main = syms.pop().unwrap();
        let mut mods = syms;
        let shift_held = mods.iter().any(|&k| k == KEYSYM_SHIFT_L || k == 0xffe2);
        if main < 0x100 && (main as u8 as char).is_ascii_lowercase() && shift_held {
            // shift+a → 'A' keysym (same as a real X client)
            main = (main as u8 as char).to_ascii_uppercase() as u32;
        } else if main < 0x100 && needs_shift(main as u8 as char) && !shift_held {
            mods.push(KEYSYM_SHIFT_L);
        }
        Ok((mods, main))
    }

    async fn combo_down(&self, mods: &[u32], main: u32) -> Result<()> {
        for &m in mods {
            self.send_key(m, true).await?;
        }
        self.send_key(main, true).await
    }

    async fn combo_up(&self, mods: &[u32], main: u32) -> Result<()> {
        self.send_key(main, false).await?;
        for &m in mods.iter().rev() {
            self.send_key(m, false).await?;
        }
        Ok(())
    }

    pub async fn key(&self, combo: &str, repeat: u32) -> Result<()> {
        let (mods, main) = self.parse_combo(combo)?;
        for i in 0..repeat.clamp(1, 100) {
            if i > 0 {
                tokio::time::sleep(self.opts.key_delay).await;
            }
            self.combo_down(&mods, main).await?;
            self.combo_up(&mods, main).await?;
        }
        Ok(())
    }

    pub async fn hold_key(&self, combo: &str, secs: f64) -> Result<()> {
        let (mods, main) = self.parse_combo(combo)?;
        self.combo_down(&mods, main).await?;
        // clamp returns NaN for NaN and from_secs_f64 would panic, so treat it as 0
        let secs = if secs.is_finite() { secs.clamp(0.0, 300.0) } else { 0.0 };
        tokio::time::sleep(Duration::from_secs_f64(secs)).await;
        self.combo_up(&mods, main).await
    }

    /// Types text. ASCII/Latin-1 uses the matching keysym (wrapped in Shift_L if needed); other
    /// characters (Korean, …) use the Unicode keysym 0x01000000 + code point.
    ///
    /// Note: whether macOS Screen Sharing servers accept Unicode keysyms is unverified.
    pub async fn type_text(&self, text: &str) -> Result<()> {
        let text = text.replace("\r\n", "\n");
        for ch in text.chars() {
            let (ks, shift) = match ch {
                '\n' | '\r' => (0xff0d, false),
                '\t' => (0xff09, false),
                c if c.is_control() => continue,
                c if (c as u32) < 0x100 => (c as u32, needs_shift(c)),
                c => (0x0100_0000 + c as u32, false),
            };
            if shift {
                self.send_key(KEYSYM_SHIFT_L, true).await?;
            }
            self.send_key(ks, true).await?;
            self.send_key(ks, false).await?;
            if shift {
                self.send_key(KEYSYM_SHIFT_L, false).await?;
            }
            tokio::time::sleep(self.opts.key_delay).await;
        }
        Ok(())
    }

    /// Puts Latin-1 text on the server clipboard (ClientCutText). Doesn't paste.
    pub async fn set_server_clipboard(&self, text: &str) -> Result<()> {
        let bytes: Vec<u8> = text.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }).collect();
        let mut m = vec![6u8, 0, 0, 0];
        m.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        m.extend_from_slice(&bytes);
        self.send(&m).await
    }

    // ---- shutdown ----

    pub async fn shutdown(&self) {
        if !self.is_closed() {
            // release any buttons and modifiers left pressed
            let pressed = self.input.lock().buttons;
            if pressed != 0 {
                self.input.lock().buttons = 0;
                let _ = self.send_pointer().await;
            }
            let _ = self.set_mods(0).await;
        }
        self.closed.store(true, Ordering::SeqCst);
        {
            let mut w = self.writer.lock().await;
            let _ = w.shutdown().await;
        }
        if let Some(h) = self.reader.lock().take() {
            h.abort();
        }
    }
}

impl Drop for RfbDesk {
    fn drop(&mut self) {
        if let Some(h) = self.reader.lock().take() {
            h.abort();
        }
    }
}

// ---- reader task ----

/// Sets `closed` and wakes waiters even if the reader task ends in a panic.
///
/// Without it, is_closed() would stay false after a panic and stale frames would keep being
/// captured.
struct ReaderPanicGuard(Weak<RfbDesk>);

impl Drop for ReaderPanicGuard {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        if let Some(me) = self.0.upgrade() {
            if !me.closed.swap(true, Ordering::SeqCst) {
                me.note("원격 데스크톱 리더 태스크 panic — 연결을 닫힌 것으로 처리합니다");
            }
            me.seq_tx.send_modify(|s| *s += 1);
        }
    }
}

async fn reader_loop(weak: Weak<RfbDesk>, mut rd: OwnedReadHalf) {
    let _guard = ReaderPanicGuard(weak.clone());
    let res = reader_inner(&weak, &mut rd).await;
    if let Some(me) = weak.upgrade() {
        let was_closed = me.closed.swap(true, Ordering::SeqCst);
        if !was_closed {
            let msg = match res {
                Ok(()) => "원격 데스크톱 연결이 종료되었습니다".to_string(),
                Err(e) => format!("원격 데스크톱 연결 끊김: {e:#}"),
            };
            me.note(msg);
        }
        me.seq_tx.send_modify(|s| *s += 1);
        (me.on_frame)();
    }
}

async fn reader_inner(weak: &Weak<RfbDesk>, rd: &mut OwnedReadHalf) -> Result<()> {
    let mut hextile_buf = Vec::new();
    // ZRLE rectangles of one connection share a single zlib stream
    let mut zrle = Zrle::new();
    loop {
        let typ = match read_u8(rd).await {
            Ok(t) => t,
            Err(_) => return Ok(()),
        };
        let Some(me) = weak.upgrade() else { return Ok(()) };
        match typ {
            0 => {
                let mut hdr = [0u8; 3];
                rd.read_exact(&mut hdr).await?;
                let n = u16::from_be_bytes([hdr[1], hdr[2]]);
                let mut resized = false;
                // pixel area received in this message. It's only added to coverage once the message is done, so
                // refresh() never captures a half-applied frame.
                let mut area: u64 = 0;
                for _ in 0..n {
                    let x = read_u16(rd).await? as u32;
                    let y = read_u16(rd).await? as u32;
                    let w = read_u16(rd).await? as u32;
                    let h = read_u16(rd).await? as u32;
                    let enc = read_u32(rd).await? as i32;
                    match enc {
                        ENC_RAW => {
                            me.fb.lock().check(x, y, w, h)?;
                            let mut data = vec![0u8; w as usize * h as usize * 4];
                            rd.read_exact(&mut data).await?;
                            me.fb.lock().put_bgrx(x, y, w, h, &data);
                            area += w as u64 * h as u64;
                        }
                        ENC_COPYRECT => {
                            let sx = read_u16(rd).await? as u32;
                            let sy = read_u16(rd).await? as u32;
                            let mut fb = me.fb.lock();
                            fb.check(x, y, w, h)?;
                            fb.check(sx, sy, w, h)?;
                            fb.copy_rect(sx, sy, x, y, w, h);
                            drop(fb);
                            area += w as u64 * h as u64;
                        }
                        ENC_RRE => {
                            me.fb.lock().check(x, y, w, h)?;
                            let nsub = read_u32(rd).await?;
                            // subrectangles are at least one pixel, so more than that is bogus (avoids a huge allocation
                            // aborting the process)
                            if nsub as u64 > w as u64 * h as u64 {
                                bail!("RRE 서브사각형 수 비정상: {nsub} (사각형 {w}x{h})");
                            }
                            let bg = read_pixel(rd).await?;
                            let mut subs = vec![0u8; nsub as usize * 12];
                            rd.read_exact(&mut subs).await?;
                            let mut fb = me.fb.lock();
                            fb.fill(x, y, w, h, bg);
                            for s in subs.chunks_exact(12) {
                                let c = [s[2], s[1], s[0], 255];
                                let sx = u16::from_be_bytes([s[4], s[5]]) as u32;
                                let sy = u16::from_be_bytes([s[6], s[7]]) as u32;
                                let sw = u16::from_be_bytes([s[8], s[9]]) as u32;
                                let sh = u16::from_be_bytes([s[10], s[11]]) as u32;
                                if sx + sw <= w && sy + sh <= h {
                                    fb.fill(x + sx, y + sy, sw, sh, c);
                                }
                            }
                            drop(fb);
                            area += w as u64 * h as u64;
                        }
                        ENC_ZRLE => {
                            me.fb.lock().check(x, y, w, h)?;
                            let len = read_u32(rd).await? as usize;
                            if len > 64 << 20 {
                                bail!("ZRLE 데이터 길이 비정상: {len}");
                            }
                            let mut z = vec![0u8; len];
                            rd.read_exact(&mut z).await?;
                            let raw = zrle.inflate(&z, w as usize * h as usize * 4 + 4096)?;
                            zrle_tiles(&mut me.fb.lock(), x, y, w, h, raw)?;
                            area += w as u64 * h as u64;
                        }
                        ENC_HEXTILE => {
                            me.fb.lock().check(x, y, w, h)?;
                            read_hextile(&me, rd, x, y, w, h, &mut hextile_buf).await?;
                            area += w as u64 * h as u64;
                        }
                        ENC_DESKTOP_SIZE => {
                            me.resize(w, h)?;
                            resized = true;
                        }
                        ENC_EXT_DESKTOP_SIZE => {
                            let ns = read_u8(rd).await?;
                            let mut skip = vec![0u8; 3 + ns as usize * 16];
                            rd.read_exact(&mut skip).await?;
                            // y = status, 0 means success
                            if y == 0 && w > 0 && h > 0 {
                                me.resize(w, h)?;
                                resized = true;
                            }
                        }
                        ENC_CURSOR => {
                            // the cursor isn't bounds-checked against the framebuffer, so cap its size separately. (Computing
                            // w*h*4 as u32 overflows at 65535x65535: debug panics, release reads the wrong length and the
                            // stream goes out of sync.)
                            if w > 1024 || h > 1024 {
                                bail!("Cursor 의사 인코딩 크기 비정상: {w}x{h}");
                            }
                            let npx = w as usize * h as usize;
                            let mut pix = vec![0u8; npx * 4];
                            rd.read_exact(&mut pix).await?;
                            let row = ((w + 7) / 8) as usize;
                            let mut mask = vec![0u8; row * h as usize];
                            rd.read_exact(&mut mask).await?;
                            let mut rgba = vec![0u8; npx * 4];
                            for cy in 0..h as usize {
                                for cx in 0..w as usize {
                                    let i = (cy * w as usize + cx) * 4;
                                    let on = mask[cy * row + cx / 8] & (0x80 >> (cx % 8)) != 0;
                                    rgba[i] = pix[i + 2];
                                    rgba[i + 1] = pix[i + 1];
                                    rgba[i + 2] = pix[i];
                                    rgba[i + 3] = if on { 255 } else { 0 };
                                }
                            }
                            *me.cursor.lock() = if w > 0 && h > 0 {
                                Some(CursorShape { hot_x: x, hot_y: y, w, h, rgba })
                            } else {
                                None
                            };
                        }
                        ENC_LAST_RECT => break,
                        other => bail!("지원하지 않는 인코딩 {other} 수신 — 스트림을 해석할 수 없음"),
                    }
                }
                let covered = me.coverage.fetch_add(area, Ordering::SeqCst) + area;
                let (fw, fh) = me.fb_size();
                if covered >= fw as u64 * fh as u64 {
                    me.synced.store(true, Ordering::SeqCst);
                }
                me.note_update();
                me.seq_tx.send_modify(|s| *s += 1);
                (me.on_frame)();
                // next incremental request (at least 2 ms apart: it is the only thing that makes the server send
                // more, so every ms spent here is added to the delay of the next picture)
                let wait = {
                    let last = *me.last_request.lock();
                    Duration::from_millis(2).saturating_sub(last.elapsed())
                };
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
                *me.last_request.lock() = Instant::now();
                let (fw, fh) = me.fb_size();
                me.send(&fb_request(!resized, 0, 0, fw as u16, fh as u16)).await?;
            }
            1 => {
                let mut hdr = [0u8; 5];
                rd.read_exact(&mut hdr).await?;
                let n = u16::from_be_bytes([hdr[3], hdr[4]]) as usize;
                let mut skip = vec![0u8; n * 6];
                rd.read_exact(&mut skip).await?;
            }
            2 => {
                // Bell: ignored
            }
            3 => {
                let mut pad = [0u8; 3];
                rd.read_exact(&mut pad).await?;
                let len = read_u32(rd).await? as i32;
                // negative length = Extended Clipboard (not negotiated, but skipped defensively)
                let n = len.unsigned_abs() as usize;
                if n > 64 << 20 {
                    bail!("ServerCutText 길이 비정상: {n}");
                }
                let mut buf = vec![0u8; n];
                rd.read_exact(&mut buf).await?;
                if len >= 0 {
                    *me.server_clipboard.lock() = Some(buf.iter().map(|&b| b as char).collect());
                }
            }
            other => bail!("알 수 없는 서버 메시지 타입 {other}"),
        }
    }
}

impl RfbDesk {
    fn resize(&self, w: u32, h: u32) -> Result<()> {
        check_fb_dims(w, h)?;
        *self.fb.lock() = Framebuffer::new(w, h);
        self.coverage.store(0, Ordering::SeqCst);
        self.synced.store(false, Ordering::SeqCst);
        self.note(format!("원격 화면 크기 변경: {w}x{h}"));
        Ok(())
    }
}

/// The zlib stream shared by all ZRLE rectangles of a connection.
struct Zrle {
    z: flate2::Decompress,
    out: Vec<u8>,
}

impl Zrle {
    fn new() -> Self {
        Zrle { z: flate2::Decompress::new(true), out: Vec::new() }
    }

    /// Inflates one rectangle's data (the stream continues across rectangles). `limit` caps the output: a rectangle
    /// cannot hold more than its pixels, so more is a broken or hostile stream.
    fn inflate(&mut self, data: &[u8], limit: usize) -> Result<&[u8]> {
        self.out.clear();
        let before = self.z.total_in();
        loop {
            let consumed = (self.z.total_in() - before) as usize;
            self.out.reserve(16 * 1024);
            let out_before = self.out.len();
            let status = self
                .z
                .decompress_vec(&data[consumed..], &mut self.out, flate2::FlushDecompress::Sync)
                .map_err(|e| anyhow!("ZRLE zlib 오류: {e}"))?;
            let now = (self.z.total_in() - before) as usize;
            if status == flate2::Status::StreamEnd {
                break;
            }
            // everything was read and the output buffer was not filled up: the flush is complete
            if now >= data.len() && self.out.len() < self.out.capacity() {
                break;
            }
            if now == consumed && self.out.len() == out_before {
                bail!("ZRLE 압축 해제가 멈춤");
            }
            if self.out.len() > limit {
                bail!("ZRLE 압축 해제 결과가 너무 큼");
            }
        }
        Ok(&self.out)
    }
}

/// Reads one ZRLE run length: 1 + the sum of bytes up to the first one below 255.
fn zrle_run(d: &[u8], i: &mut usize) -> Result<usize> {
    let mut n = 1usize;
    loop {
        let b = *d.get(*i).ok_or_else(|| anyhow!("ZRLE 데이터가 모자람"))?;
        *i += 1;
        n += b as usize;
        if b != 255 {
            return Ok(n);
        }
    }
}

/// Decodes the (already inflated) tiles of a ZRLE rectangle into the framebuffer. The pixel format is 32bpp with
/// 24 significant bits, so a "CPIXEL" is the 3 low bytes: B, G, R.
fn zrle_tiles(fb: &mut Framebuffer, x: u32, y: u32, w: u32, h: u32, d: &[u8]) -> Result<()> {
    let mut i = 0usize;
    let need = |i: usize, n: usize| -> Result<()> {
        if i + n > d.len() { bail!("ZRLE 데이터가 모자람") } else { Ok(()) }
    };
    let cpix = |i: &mut usize| -> Result<[u8; 4]> {
        need(*i, 3)?;
        let p = [d[*i + 2], d[*i + 1], d[*i], 255];
        *i += 3;
        Ok(p)
    };
    let mut ty = 0;
    while ty < h {
        let th = 64.min(h - ty);
        let mut tx = 0;
        while tx < w {
            let tw = 64.min(w - tx);
            let n = (tw * th) as usize;
            need(i, 1)?;
            let sub = d[i];
            i += 1;
            let rle = sub & 128 != 0;
            let psize = (sub & 127) as usize;
            if psize > 127 {
                bail!("ZRLE 팔레트 크기 비정상");
            }
            let mut palette = Vec::with_capacity(psize);
            for _ in 0..psize {
                palette.push(cpix(&mut i)?);
            }
            // RGBA pixels of the tile, row by row
            let mut px: Vec<[u8; 4]> = Vec::with_capacity(n);
            match (rle, psize) {
                (false, 0) => {
                    for _ in 0..n {
                        px.push(cpix(&mut i)?);
                    }
                }
                (false, 1) => px.resize(n, palette[0]),
                (false, 2..=16) => {
                    let bits = if psize == 2 { 1 } else if psize <= 4 { 2 } else { 4 };
                    for _ in 0..th {
                        let mut left = tw;
                        while left > 0 {
                            need(i, 1)?;
                            let byte = d[i];
                            i += 1;
                            let per = 8 / bits;
                            for k in 0..per.min(left as usize) {
                                let idx = ((byte >> (8 - bits * (k + 1))) & ((1u8 << bits) - 1)) as usize;
                                px.push(*palette.get(idx).ok_or_else(|| anyhow!("ZRLE 팔레트 번호 비정상"))?);
                            }
                            left -= per.min(left as usize) as u32;
                        }
                    }
                }
                (true, 0) => {
                    while px.len() < n {
                        let c = cpix(&mut i)?;
                        let run = zrle_run(d, &mut i)?;
                        if px.len() + run > n { bail!("ZRLE 반복 길이 비정상"); }
                        px.resize(px.len() + run, c);
                    }
                }
                (true, 2..=127) => {
                    while px.len() < n {
                        need(i, 1)?;
                        let b = d[i];
                        i += 1;
                        let c = *palette.get((b & 127) as usize).ok_or_else(|| anyhow!("ZRLE 팔레트 번호 비정상"))?;
                        let run = if b & 128 != 0 { zrle_run(d, &mut i)? } else { 1 };
                        if px.len() + run > n { bail!("ZRLE 반복 길이 비정상"); }
                        px.resize(px.len() + run, c);
                    }
                }
                _ => bail!("ZRLE 하위 인코딩 {sub} 은(는) 정의되지 않음"),
            }
            let stride = fb.w as usize * 4;
            for row in 0..th as usize {
                let dst0 = (y + ty) as usize * stride + (x + tx) as usize * 4 + row * stride;
                for col in 0..tw as usize {
                    fb.rgba[dst0 + col * 4..dst0 + col * 4 + 4].copy_from_slice(&px[row * tw as usize + col]);
                }
            }
            tx += 64;
        }
        ty += 64;
    }
    Ok(())
}

async fn read_hextile<R: AsyncRead + Unpin>(
    me: &RfbDesk,
    rd: &mut R,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    buf: &mut Vec<u8>,
) -> Result<()> {
    let mut bg = [0u8, 0, 0, 255];
    let mut fg = [0u8, 0, 0, 255];
    let mut ty = y;
    while ty < y + h {
        let th = 16.min(y + h - ty);
        let mut tx = x;
        while tx < x + w {
            let tw = 16.min(x + w - tx);
            let sub = read_u8(rd).await?;
            if sub & 1 != 0 {
                buf.resize((tw * th * 4) as usize, 0);
                rd.read_exact(buf).await?;
                me.fb.lock().put_bgrx(tx, ty, tw, th, buf);
            } else {
                if sub & 2 != 0 {
                    bg = read_pixel(rd).await?;
                }
                if sub & 4 != 0 {
                    fg = read_pixel(rd).await?;
                }
                let mut rects = Vec::new();
                if sub & 8 != 0 {
                    let n = read_u8(rd).await? as usize;
                    let coloured = sub & 16 != 0;
                    let per = if coloured { 6 } else { 2 };
                    buf.resize(n * per, 0);
                    rd.read_exact(buf).await?;
                    for r in buf.chunks_exact(per) {
                        let (c, xy, wh) = if coloured {
                            ([r[2], r[1], r[0], 255], r[4], r[5])
                        } else {
                            (fg, r[0], r[1])
                        };
                        rects.push((c, (xy >> 4) as u32, (xy & 15) as u32, (wh >> 4) as u32 + 1, (wh & 15) as u32 + 1));
                    }
                }
                let mut fb = me.fb.lock();
                fb.fill(tx, ty, tw, th, bg);
                for (c, sx, sy, sw, sh) in rects {
                    if sx + sw <= tw && sy + sh <= th {
                        fb.fill(tx + sx, ty + sy, sw, sh, c);
                    }
                }
            }
            tx += 16;
        }
        ty += 16;
    }
    Ok(())
}

// ---- handshake ----

/// (server version string, negotiated minor (3/7/8), our reply string)
async fn read_version<R: AsyncRead + Unpin>(s: &mut R, reply: ReplyVersion) -> Result<(String, u32, String)> {
    let mut v = [0u8; 12];
    s.read_exact(&mut v).await.context("ProtocolVersion 수신 실패")?;
    let (major, minor) = parse_version(&v)?;
    let raw = String::from_utf8_lossy(&v[..11]).to_string();
    let (eff, reply_str) = if major > 3 || minor >= 8 {
        if minor == 889 && reply == ReplyVersion::Apple889 {
            (8, "RFB 003.889\n".to_string())
        } else {
            (8, "RFB 003.008\n".to_string())
        }
    } else if minor == 7 {
        (7, "RFB 003.007\n".to_string())
    } else {
        (3, "RFB 003.003\n".to_string())
    };
    Ok((raw, eff, reply_str))
}

pub fn parse_version(v: &[u8; 12]) -> Result<(u32, u32)> {
    let s = std::str::from_utf8(v).map_err(|_| anyhow!("RFB 서버가 아님 (버전 문자열이 텍스트가 아님)"))?;
    if !s.starts_with("RFB ") || v[7] != b'.' || v[11] != b'\n' {
        bail!("RFB 서버가 아님: {s:?}");
    }
    let major: u32 = s[4..7].parse().map_err(|_| anyhow!("버전 파싱 실패: {s:?}"))?;
    let minor: u32 = s[8..11].parse().map_err(|_| anyhow!("버전 파싱 실패: {s:?}"))?;
    if major < 3 {
        bail!("지원하지 않는 RFB 버전 {major}.{minor}");
    }
    Ok((major, minor))
}

async fn negotiate_security(s: &mut TcpStream, minor: u32, o: &RfbOptions) -> Result<u8> {
    let chosen;
    if minor < 7 {
        let t = read_u32(s).await?;
        match t {
            0 => bail!("서버가 접속 거부: {}", read_string(s).await?),
            1 => return Ok(SEC_NONE),
            2 => {
                chosen = SEC_VNC;
            }
            other => bail!("RFB 3.3 서버가 지원하지 않는 보안 타입 {other} 지정"),
        }
    } else {
        let n = read_u8(s).await?;
        if n == 0 {
            bail!("서버가 접속 거부: {}", read_string(s).await?);
        }
        let mut types = vec![0u8; n as usize];
        s.read_exact(&mut types).await?;
        let has = |t: u8| types.contains(&t);
        let have_pw = o.password.is_some();
        let have_user = o.username.as_deref().map(|u| !u.is_empty()).unwrap_or(false);
        chosen = if has(SEC_NONE) {
            SEC_NONE
        } else if has(SEC_ARD) && have_user && have_pw {
            SEC_ARD
        } else if has(SEC_VNC) && have_pw {
            SEC_VNC
        } else {
            let list: Vec<String> = types.iter().map(|t| format!("{t}({})", security_type_name(*t))).collect();
            bail!(
                "사용 가능한 보안 타입 없음. 서버 제공: [{}]. (ARD=30 은 사용자 이름+암호, VNC=2 는 암호 필요)",
                list.join(", ")
            );
        };
        s.write_all(&[chosen]).await?;
    }
    match chosen {
        SEC_NONE => {}
        SEC_VNC => {
            let mut chal = [0u8; 16];
            s.read_exact(&mut chal).await?;
            let resp = vnc_auth_response(o.password.as_deref().unwrap_or(""), &chal);
            s.write_all(&resp).await?;
        }
        SEC_ARD => {
            let g = read_u16(s).await?;
            let keylen = read_u16(s).await? as usize;
            if keylen == 0 || keylen > 1024 {
                bail!("ARD 키 길이 비정상: {keylen}");
            }
            let mut p = vec![0u8; keylen];
            s.read_exact(&mut p).await?;
            let mut y = vec![0u8; keylen];
            s.read_exact(&mut y).await?;
            let (ct, pubk) = ard_auth_response(
                g,
                &p,
                &y,
                o.username.as_deref().unwrap_or(""),
                o.password.as_deref().unwrap_or(""),
            )?;
            let mut out = ct.to_vec();
            out.extend_from_slice(&pubk);
            s.write_all(&out).await?;
        }
        _ => unreachable!(),
    }
    // SecurityResult: always on 3.8; on 3.3/3.7 only when not None
    if minor >= 8 || chosen != SEC_NONE {
        let r = read_u32(s).await?;
        if r != 0 {
            let reason = if minor >= 8 { read_string(s).await.unwrap_or_default() } else { String::new() };
            bail!(
                "RFB 인증 실패 (보안 타입 {} {}, 결과 {r}){}",
                chosen,
                security_type_name(chosen),
                if reason.is_empty() { String::new() } else { format!(": {reason}") }
            );
        }
    }
    Ok(chosen)
}

// ---- utilities ----

fn agent_size(w: u32, h: u32, max: Option<(u32, u32)>) -> (u32, u32) {
    match max {
        Some((mw, mh)) if w > mw || h > mh => {
            let s = (mw as f64 / w as f64).min(mh as f64 / h as f64);
            (((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1))
        }
        _ => (w.max(1), h.max(1)),
    }
}

fn fb_request(incremental: bool, x: u16, y: u16, w: u16, h: u16) -> [u8; 10] {
    let mut m = [3u8, incremental as u8, 0, 0, 0, 0, 0, 0, 0, 0];
    m[2..4].copy_from_slice(&x.to_be_bytes());
    m[4..6].copy_from_slice(&y.to_be_bytes());
    m[6..8].copy_from_slice(&w.to_be_bytes());
    m[8..10].copy_from_slice(&h.to_be_bytes());
    m
}

fn button_bit(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 4,
    }
}

fn needs_shift(c: char) -> bool {
    c.is_ascii_uppercase() || "~!@#$%^&*()_+{}|:\"<>?".contains(c)
}

/// xdotool key name / single character → X11 keysym.
pub fn keysym_for(name: &str, cmd_keysym: u32) -> Option<u32> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.clone().next()) {
        return Some(match c {
            '\n' | '\r' => 0xff0d,
            '\t' => 0xff09,
            c if (c as u32) < 0x20 => return None,
            c if (c as u32) < 0x100 => c as u32,
            c => 0x0100_0000 + c as u32,
        });
    }
    let n = name.to_ascii_lowercase();
    let k = match n.as_str() {
        "return" | "enter" => 0xff0d,
        "kp_enter" => 0xff8d,
        "tab" | "iso_left_tab" => 0xff09,
        "escape" | "esc" => 0xff1b,
        "backspace" => 0xff08,
        "delete" | "del" => 0xffff,
        "kp_delete" => 0xff9f,
        "insert" => 0xff63,
        "home" | "kp_home" => 0xff50,
        "left" | "arrowleft" | "kp_left" => 0xff51,
        "up" | "arrowup" | "kp_up" => 0xff52,
        "right" | "arrowright" | "kp_right" => 0xff53,
        "down" | "arrowdown" | "kp_down" => 0xff54,
        "page_up" | "pageup" | "prior" => 0xff55,
        "page_down" | "pagedown" | "next" => 0xff56,
        "end" | "kp_end" => 0xff57,
        "menu" => 0xff67,
        "print" => 0xff61,
        "pause" => 0xff13,
        "scroll_lock" => 0xff14,
        "num_lock" => 0xff7f,
        "caps_lock" => 0xffe5,
        "space" => 0x20,
        "shift" | "shift_l" => KEYSYM_SHIFT_L,
        "shift_r" => 0xffe2,
        "ctrl" | "control" | "control_l" | "ctrl_l" => KEYSYM_CONTROL_L,
        "control_r" | "ctrl_r" => 0xffe4,
        "alt" | "option" | "alt_l" => KEYSYM_ALT_L,
        "alt_r" => 0xffea,
        "super" | "super_l" | "cmd" | "command" | "meta" | "win" => cmd_keysym,
        "super_r" => 0xffec,
        "meta_l" => KEYSYM_META_L,
        "meta_r" => 0xffe8,
        "plus" => b'+' as u32,
        "minus" => b'-' as u32,
        "comma" => b',' as u32,
        "period" => b'.' as u32,
        "slash" => b'/' as u32,
        "backslash" => b'\\' as u32,
        "semicolon" => b';' as u32,
        "apostrophe" => b'\'' as u32,
        "grave" => b'`' as u32,
        "equal" => b'=' as u32,
        "bracketleft" => b'[' as u32,
        "bracketright" => b']' as u32,
        _ => {
            if let Some(num) = n.strip_prefix('f').and_then(|s| s.parse::<u32>().ok()) {
                if (1..=24).contains(&num) {
                    return Some(0xffbe + num - 1);
                }
            }
            if let Some(d) = n.strip_prefix("kp_").and_then(|s| s.parse::<u32>().ok()) {
                if d <= 9 {
                    return Some(0xffb0 + d);
                }
            }
            return None;
        }
    };
    Some(k)
}

fn encode_png_rgb(img: image::RgbImage) -> Result<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

async fn read_u8<R: AsyncRead + Unpin>(s: &mut R) -> Result<u8> {
    Ok(s.read_u8().await?)
}
async fn read_u16<R: AsyncRead + Unpin>(s: &mut R) -> Result<u16> {
    Ok(s.read_u16().await?)
}
async fn read_u32<R: AsyncRead + Unpin>(s: &mut R) -> Result<u32> {
    Ok(s.read_u32().await?)
}
async fn read_string<R: AsyncRead + Unpin>(s: &mut R) -> Result<String> {
    let n = read_u32(s).await? as usize;
    if n > 1 << 20 {
        bail!("문자열 길이 비정상: {n}");
    }
    let mut b = vec![0u8; n];
    s.read_exact(&mut b).await?;
    Ok(String::from_utf8_lossy(&b).to_string())
}
/// One 32bpp LE (B,G,R,X) pixel → RGBA.
async fn read_pixel<R: AsyncRead + Unpin>(s: &mut R) -> Result<[u8; 4]> {
    let mut p = [0u8; 4];
    s.read_exact(&mut p).await?;
    Ok([p[2], p[1], p[0], 255])
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    // Windows: the OS CSPRNG (ProcessPrng, the same generator as BCryptGenRandom)
    #[cfg(windows)]
    {
        if getrandom::fill(&mut out).is_ok() {
            return out;
        }
    }
    #[cfg(not(windows))]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            if f.read_exact(&mut out).is_ok() {
                return out;
            }
        }
    }
    // fallback: RandomState hash + time (weak; for environments without an OS RNG)
    use std::hash::{BuildHasher, Hasher};
    let rs = std::collections::hash_map::RandomState::new();
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        let mut h = rs.build_hasher();
        h.write_usize(i);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        let v = h.finish().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
    out
}

// ---- authentication ----

/// VNC Authentication: uses the first 8 bytes of the password (zero-padded), with each byte's
/// bits reversed, as the DES key and DES-ECB encrypts the 16-byte challenge 8 bytes at a time.
pub fn vnc_auth_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (i, b) in password.bytes().take(8).enumerate() {
        key[i] = b.reverse_bits();
    }
    let ks = des::key_schedule(u64::from_be_bytes(key));
    let mut out = [0u8; 16];
    for i in 0..2 {
        let blk = u64::from_be_bytes(challenge[i * 8..i * 8 + 8].try_into().unwrap());
        out[i * 8..i * 8 + 8].copy_from_slice(&des::encrypt(blk, &ks).to_be_bytes());
    }
    out
}

/// Apple Remote Desktop (30): DH shared key → MD5 → AES-128-ECB encrypts
/// [username (64, NUL-terminated) | password (64, NUL-terminated)].
///
/// Returns (128-byte ciphertext, client public key of keylen bytes).
pub fn ard_auth_response(g: u16, p: &[u8], server_pub: &[u8], user: &str, pass: &str) -> Result<([u8; 128], Vec<u8>)> {
    let keylen = p.len();
    let modulus = bigint::from_be(p);
    if !bigint::is_odd(&modulus) {
        bail!("ARD: 짝수 모듈러스는 지원하지 않음");
    }
    let priv_key = bigint::from_be(&random_bytes(keylen));
    let gen = bigint::from_be(&g.to_be_bytes());
    let pubk = bigint::modpow(&gen, &priv_key, &modulus);
    let shared = bigint::modpow(&bigint::from_be(server_pub), &priv_key, &modulus);
    let shared_bytes = bigint::to_be(&shared, keylen);
    let key = md5::digest(&shared_bytes);
    let mut creds = [0u8; 128];
    creds.copy_from_slice(&random_bytes(128));
    let u = user.as_bytes();
    let pw = pass.as_bytes();
    let ul = u.len().min(63);
    let pl = pw.len().min(63);
    creds[..ul].copy_from_slice(&u[..ul]);
    creds[ul] = 0;
    creds[64..64 + pl].copy_from_slice(&pw[..pl]);
    creds[64 + pl] = 0;
    let aes = aes128::Aes128::new(&key);
    let mut ct = [0u8; 128];
    for i in 0..8 {
        let mut blk: [u8; 16] = creds[i * 16..i * 16 + 16].try_into().unwrap();
        aes.encrypt_block(&mut blk);
        ct[i * 16..i * 16 + 16].copy_from_slice(&blk);
    }
    Ok((ct, bigint::to_be(&pubk, keylen)))
}

/// Minimal DES (encryption only, single block).
mod des {
    const IP: [u8; 64] = [
        58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6, 64, 56, 48, 40,
        32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61, 53, 45, 37, 29, 21, 13, 5, 63,
        55, 47, 39, 31, 23, 15, 7,
    ];
    const E: [u8; 48] = [
        32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17, 16, 17, 18, 19, 20, 21, 20,
        21, 22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
    ];
    const P: [u8; 32] = [
        16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9, 19, 13, 30, 6, 22, 11,
        4, 25,
    ];
    const PC1: [u8; 56] = [
        57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60, 52, 44, 36, 63,
        55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29, 21, 13, 5, 28, 20, 12, 4,
    ];
    const PC2: [u8; 48] = [
        14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2, 41, 52, 31, 37, 47, 55,
        30, 40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
    ];
    const SHIFTS: [u8; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
    const S: [[u8; 64]; 8] = [
        [
            14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12, 11, 9, 5, 3, 8,
            4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9, 1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
        ],
        [
            15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1, 10, 6, 9, 11, 5,
            0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15, 4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
        ],
        [
            10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14, 12, 11, 15, 1,
            13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6, 9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12,
        ],
        [
            7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2, 12, 1, 10, 14, 9,
            10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1, 13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
        ],
        [
            2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15, 10, 3, 9, 8, 6,
            4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14, 2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
        ],
        [
            12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13, 14, 0, 11, 3, 8,
            9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5, 15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
        ],
        [
            4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5, 12, 2, 15, 8, 6,
            1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4, 10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
        ],
        [
            13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6, 11, 0, 14, 9, 2,
            7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10, 8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
        ],
    ];

    /// Builds a new value by picking bits at the table's 1-based positions (most significant bit is 1).
    fn permute(input: u64, in_bits: u32, table: &[u8]) -> u64 {
        let mut out = 0u64;
        for &p in table {
            out = (out << 1) | ((input >> (in_bits - p as u32)) & 1);
        }
        out
    }

    pub fn key_schedule(key: u64) -> [u64; 16] {
        let cd = permute(key, 64, &PC1);
        let mut c = (cd >> 28) & 0x0fff_ffff;
        let mut d = cd & 0x0fff_ffff;
        let mut ks = [0u64; 16];
        for (i, &s) in SHIFTS.iter().enumerate() {
            for _ in 0..s {
                c = ((c << 1) | (c >> 27)) & 0x0fff_ffff;
                d = ((d << 1) | (d >> 27)) & 0x0fff_ffff;
            }
            ks[i] = permute((c << 28) | d, 56, &PC2);
        }
        ks
    }

    fn feistel(r: u32, k: u64) -> u32 {
        let x = permute(r as u64, 32, &E) ^ k;
        let mut out = 0u32;
        for i in 0..8 {
            let b = ((x >> (42 - 6 * i)) & 0x3f) as usize;
            let row = ((b & 0x20) >> 4) | (b & 1);
            let col = (b >> 1) & 0xf;
            out = (out << 4) | S[i][row * 16 + col] as u32;
        }
        permute(out as u64, 32, &P) as u32
    }

    pub fn encrypt(block: u64, ks: &[u64; 16]) -> u64 {
        let ip = permute(block, 64, &IP);
        let mut l = (ip >> 32) as u32;
        let mut r = ip as u32;
        for k in ks {
            let t = r;
            r = l ^ feistel(r, *k);
            l = t;
        }
        let pre = ((r as u64) << 32) | l as u64;
        // final permutation = inverse of IP
        let mut fp = [0u8; 64];
        for (i, &p) in IP.iter().enumerate() {
            fp[p as usize - 1] = i as u8 + 1;
        }
        permute(pre, 64, &fp)
    }
}

/// Minimal MD5 (RFC 1321).
mod md5 {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14,
        20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6,
        10, 15, 21,
    ];

    pub fn digest(data: &[u8]) -> [u8; 16] {
        let k: Vec<u32> = (0..64).map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32).collect();
        let mut st: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
        let mut msg = data.to_vec();
        let bitlen = (data.len() as u64).wrapping_mul(8);
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bitlen.to_le_bytes());
        for chunk in msg.chunks_exact(64) {
            let m: Vec<u32> = chunk.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect();
            let (mut a, mut b, mut c, mut d) = (st[0], st[1], st[2], st[3]);
            for i in 0..64 {
                let (f, g) = match i / 16 {
                    0 => ((b & c) | (!b & d), i),
                    1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                    2 => (b ^ c ^ d, (3 * i + 5) % 16),
                    _ => (c ^ (b | !d), (7 * i) % 16),
                };
                let f2 = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
                a = d;
                d = c;
                c = b;
                b = b.wrapping_add(f2.rotate_left(S[i]));
            }
            st[0] = st[0].wrapping_add(a);
            st[1] = st[1].wrapping_add(b);
            st[2] = st[2].wrapping_add(c);
            st[3] = st[3].wrapping_add(d);
        }
        let mut out = [0u8; 16];
        for i in 0..4 {
            out[i * 4..i * 4 + 4].copy_from_slice(&st[i].to_le_bytes());
        }
        out
    }
}

/// Minimal AES-128 (encryption only, one ECB block at a time).
mod aes128 {
    fn sbox() -> [u8; 256] {
        let mut sbox = [0u8; 256];
        let (mut p, mut q) = (1u8, 1u8);
        loop {
            p = p ^ (p << 1) ^ if p & 0x80 != 0 { 0x1b } else { 0 };
            q ^= q << 1;
            q ^= q << 2;
            q ^= q << 4;
            if q & 0x80 != 0 {
                q ^= 0x09;
            }
            let x = q ^ q.rotate_left(1) ^ q.rotate_left(2) ^ q.rotate_left(3) ^ q.rotate_left(4);
            sbox[p as usize] = x ^ 0x63;
            if p == 1 {
                break;
            }
        }
        sbox[0] = 0x63;
        sbox
    }

    fn xtime(b: u8) -> u8 {
        (b << 1) ^ if b & 0x80 != 0 { 0x1b } else { 0 }
    }

    pub struct Aes128 {
        rk: [u8; 176],
        sbox: [u8; 256],
    }

    impl Aes128 {
        pub fn new(key: &[u8; 16]) -> Self {
            let sbox = sbox();
            let mut rk = [0u8; 176];
            rk[..16].copy_from_slice(key);
            let mut rcon = 1u8;
            for i in 4..44 {
                let mut t = [rk[(i - 1) * 4], rk[(i - 1) * 4 + 1], rk[(i - 1) * 4 + 2], rk[(i - 1) * 4 + 3]];
                if i % 4 == 0 {
                    t = [sbox[t[1] as usize] ^ rcon, sbox[t[2] as usize], sbox[t[3] as usize], sbox[t[0] as usize]];
                    rcon = xtime(rcon);
                }
                for j in 0..4 {
                    rk[i * 4 + j] = rk[(i - 4) * 4 + j] ^ t[j];
                }
            }
            Aes128 { rk, sbox }
        }

        pub fn encrypt_block(&self, s: &mut [u8; 16]) {
            let add = |s: &mut [u8; 16], r: usize| {
                for i in 0..16 {
                    s[i] ^= self.rk[r * 16 + i];
                }
            };
            add(s, 0);
            for round in 1..=10 {
                for b in s.iter_mut() {
                    *b = self.sbox[*b as usize];
                }
                // ShiftRows (state[r + 4c])
                let t = *s;
                for c in 0..4 {
                    for r in 0..4 {
                        s[r + 4 * c] = t[r + 4 * ((c + r) % 4)];
                    }
                }
                if round != 10 {
                    for c in 0..4 {
                        let a = [s[4 * c], s[4 * c + 1], s[4 * c + 2], s[4 * c + 3]];
                        let all = a[0] ^ a[1] ^ a[2] ^ a[3];
                        for r in 0..4 {
                            s[4 * c + r] = a[r] ^ all ^ xtime(a[r] ^ a[(r + 1) % 4]);
                        }
                    }
                }
                add(s, round);
            }
        }
    }
}

/// Minimal big integers for DH (little-endian u32 limbs, Montgomery modpow; odd moduli only).
mod bigint {
    pub type Big = Vec<u32>;

    pub fn from_be(b: &[u8]) -> Big {
        let mut v = Vec::with_capacity(b.len() / 4 + 1);
        let mut i = b.len();
        while i > 0 {
            let lo = i.saturating_sub(4);
            let mut w = 0u32;
            for &x in &b[lo..i] {
                w = (w << 8) | x as u32;
            }
            v.push(w);
            i = lo;
        }
        trim(&mut v);
        v
    }

    pub fn to_be(a: &Big, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        for (i, w) in a.iter().enumerate() {
            for j in 0..4 {
                let pos = i * 4 + j;
                if pos < len {
                    out[len - 1 - pos] = (w >> (8 * j)) as u8;
                }
            }
        }
        out
    }

    fn trim(v: &mut Big) {
        while v.last() == Some(&0) {
            v.pop();
        }
    }

    pub fn is_odd(a: &Big) -> bool {
        a.first().map(|w| w & 1 == 1).unwrap_or(false)
    }

    fn bits(a: &Big) -> usize {
        match a.last() {
            None => 0,
            Some(&top) => (a.len() - 1) * 32 + (32 - top.leading_zeros() as usize),
        }
    }

    fn bit(a: &Big, i: usize) -> bool {
        a.get(i / 32).map(|w| (w >> (i % 32)) & 1 == 1).unwrap_or(false)
    }

    /// Fixed-width compare/subtract over k limbs.
    fn geq(a: &[u32], n: &[u32]) -> bool {
        for i in (0..a.len().max(n.len())).rev() {
            let x = a.get(i).copied().unwrap_or(0);
            let y = n.get(i).copied().unwrap_or(0);
            if x != y {
                return x > y;
            }
        }
        true
    }

    fn sub_in(a: &mut [u32], n: &[u32]) {
        let mut borrow = 0i64;
        for i in 0..a.len() {
            let v = a[i] as i64 - n.get(i).copied().unwrap_or(0) as i64 - borrow;
            if v < 0 {
                a[i] = (v + (1i64 << 32)) as u32;
                borrow = 1;
            } else {
                a[i] = v as u32;
                borrow = 0;
            }
        }
    }

    /// a mod n (bit by bit; slow, but only used a few times).
    fn modulo(a: &Big, n: &[u32]) -> Vec<u32> {
        let k = n.len();
        let mut r = vec![0u32; k + 1];
        for i in (0..bits(a)).rev() {
            // r = 2r + bit
            let mut carry = bit(a, i) as u32;
            for w in r.iter_mut() {
                let nc = *w >> 31;
                *w = (*w << 1) | carry;
                carry = nc;
            }
            if geq(&r, n) {
                sub_in(&mut r, n);
            }
        }
        r.truncate(k);
        r
    }

    fn mont_mul(a: &[u32], b: &[u32], n: &[u32], n0inv: u32) -> Vec<u32> {
        let k = n.len();
        let mut t = vec![0u32; k + 2];
        for i in 0..k {
            let mut c: u64 = 0;
            let bi = b[i] as u64;
            for j in 0..k {
                let v = t[j] as u64 + a[j] as u64 * bi + c;
                t[j] = v as u32;
                c = v >> 32;
            }
            let v = t[k] as u64 + c;
            t[k] = v as u32;
            t[k + 1] = (v >> 32) as u32;
            let m = t[0].wrapping_mul(n0inv) as u64;
            let v = t[0] as u64 + m * n[0] as u64;
            let mut c = v >> 32;
            for j in 1..k {
                let v = t[j] as u64 + m * n[j] as u64 + c;
                t[j - 1] = v as u32;
                c = v >> 32;
            }
            let v = t[k] as u64 + c;
            t[k - 1] = v as u32;
            t[k] = t[k + 1] + (v >> 32) as u32;
            t[k + 1] = 0;
        }
        t.truncate(k + 1);
        if geq(&t, n) {
            sub_in(&mut t, n);
        }
        t.truncate(k);
        t
    }

    /// base^exp mod n (n odd, > 1).
    pub fn modpow(base: &Big, exp: &Big, n: &Big) -> Big {
        let k = n.len();
        let mut inv: u32 = 1;
        for _ in 0..5 {
            inv = inv.wrapping_mul(2u32.wrapping_sub(n[0].wrapping_mul(inv)));
        }
        let n0inv = inv.wrapping_neg();
        // R^2 mod n, R = 2^(32k)
        let mut r2 = vec![0u32; 2 * k + 1];
        r2[2 * k] = 1;
        trim(&mut r2);
        let r2 = modulo(&r2, n);
        let b = modulo(base, n);
        let bm = mont_mul(&b, &r2, n, n0inv);
        let mut one = vec![0u32; k];
        one[0] = 1;
        let mut acc = mont_mul(&one, &r2, n, n0inv);
        for i in (0..bits(exp)).rev() {
            acc = mont_mul(&acc, &acc, n, n0inv);
            if bit(exp, i) {
                acc = mont_mul(&acc, &bm, n, n0inv);
            }
        }
        let mut out = mont_mul(&acc, &one, n, n0inv);
        trim(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn crlf_types_one_return_and_preserves_other_text() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        let received = tokio::spawn(async move {
            let mut bytes = Vec::new();
            peer.read_to_end(&mut bytes).await.unwrap();
            bytes
        });
        let (_, writer) = stream.into_split();
        let mut opts = RfbOptions::new("127.0.0.1", 0);
        opts.key_delay = Duration::ZERO;
        let desk = RfbDesk {
            opts, name: Mutex::new("test".into()), server_version: "RFB 003.008\n".into(), security_type: SEC_NONE,
            fb: Mutex::new(Framebuffer::new(16, 16)), writer: tokio::sync::Mutex::new(writer),
            input: Mutex::new(InputState::default()), closed: AtomicBool::new(false),
            seq_tx: watch::channel(0).0, coverage: AtomicU64::new(0), notes: Mutex::new(Vec::new()),
            on_frame: Arc::new(|| {}), reader: Mutex::new(None), cursor: Mutex::new(None),
            server_clipboard: Mutex::new(None), last_request: Mutex::new(Instant::now()), synced: AtomicBool::new(false), input_at: Mutex::new(None), lag_ms: AtomicU64::new(0),
        };
        desk.type_text("a\r\nb\rc\n가\t👋").await.unwrap();
        desk.shutdown().await;
        let bytes = tokio::time::timeout(Duration::from_secs(3), received).await.unwrap().unwrap();
        let expected: Vec<u8> = ['a' as u32, 0xff0d, 'b' as u32, 0xff0d, 'c' as u32, 0xff0d, 0x0100_0000 + '가' as u32, 0xff09, 0x0100_0000 + '👋' as u32]
            .into_iter().flat_map(|keysym| [true, false].into_iter().flat_map(move |down| {
                let mut packet = vec![4, down as u8, 0, 0];
                packet.extend_from_slice(&keysym.to_be_bytes());
                packet
            })).collect();
        assert_eq!(bytes, expected, "CRLF must send one Return; lone CR/LF, tab and Unicode stay unchanged");
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn des_fips_vector() {
        let ks = des::key_schedule(0x133457799BBCDFF1);
        assert_eq!(des::encrypt(0x0123456789ABCDEF, &ks), 0x85E813540F0AB405);
    }

    #[test]
    fn md5_vectors() {
        assert_eq!(hex(&md5::digest(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5::digest(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(&md5::digest(b"12345678901234567890123456789012345678901234567890123456789012345678901234567890")),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn aes_fips197_vector() {
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        let mut blk: [u8; 16] = core::array::from_fn(|i| (i as u8) * 0x11);
        aes128::Aes128::new(&key).encrypt_block(&mut blk);
        assert_eq!(hex(&blk), "69c4e0d86a7b0430d8cdb78070b4c55a");
    }

    #[test]
    fn modpow_small_and_large() {
        let r = bigint::modpow(&bigint::from_be(&[4]), &bigint::from_be(&[13]), &bigint::from_be(&[0x01, 0xf1]));
        assert_eq!(bigint::to_be(&r, 2), vec![0x01, 0xbd]); // 4^13 mod 497 = 445
        // 2^(2^64) mod (2^127 - 1) = 2^(2^64 mod 127) = 2^(2^64 mod 127)
        let mut m = vec![0xffu8; 16];
        m[0] = 0x7f;
        let e = bigint::from_be(&[1, 0, 0, 0, 0, 0, 0, 0, 0]);
        let r = bigint::modpow(&bigint::from_be(&[2]), &e, &bigint::from_be(&m));
        // 2^64 mod 127: 2^7 ≡ 1 → 64 mod 7 = 1 → 2^64 ≡ 2 (mod 127) → result 2^2 = 4
        assert_eq!(bigint::to_be(&r, 16)[15], 4);
        assert!(bigint::to_be(&r, 16)[..15].iter().all(|&b| b == 0));
    }

    #[test]
    fn keysyms() {
        assert_eq!(keysym_for("Return", KEYSYM_SUPER_L), Some(0xff0d));
        assert_eq!(keysym_for("F12", KEYSYM_SUPER_L), Some(0xffc9));
        assert_eq!(keysym_for("Page_Down", KEYSYM_SUPER_L), Some(0xff56));
        assert_eq!(keysym_for("cmd", KEYSYM_SUPER_L), Some(KEYSYM_SUPER_L));
        assert_eq!(keysym_for("한", KEYSYM_SUPER_L), Some(0x0100_0000 + '한' as u32));
    }

    /// The agent calls Desk methods inside tokio::spawn, so futures of public async methods must be
    /// Send.
    #[allow(dead_code, unreachable_code, unused_variables)]
    fn public_futures_are_send() {
        fn s<T: Send>(_: T) {}
        let d: Arc<RfbDesk> = unreachable!();
        s(d.screenshot());
        s(d.zoom([0.0; 4]));
        s(d.click(None, MouseButton::Left, 1, 0));
        s(d.drag((0.0, 0.0), (1.0, 1.0), 0));
        s(d.scroll(None, 0.0, 1.0, 0));
        s(d.key("a", 1));
        s(d.hold_key("a", 1.0));
        s(d.type_text("a"));
        s(d.mouse_move(0.0, 0.0, 0));
        s(d.mouse_down(MouseButton::Left, 1, 0));
        s(d.mouse_up(MouseButton::Left, 1, 0));
        s(d.switch_view(0));
        s(d.shutdown());
    }

    /// Minimal RFB 3.8 / None server: sends the `update` bytes after ServerInit(w,h) and discards
    /// client messages.
    async fn fake_server(w: u16, h: u16, update: Vec<u8>) -> u16 {
        use tokio::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            s.write_all(b"RFB 003.008\n").await.unwrap();
            let mut v = [0u8; 12];
            s.read_exact(&mut v).await.unwrap();
            s.write_all(&[1, SEC_NONE]).await.unwrap();
            let mut b = [0u8; 1];
            s.read_exact(&mut b).await.unwrap();
            s.write_all(&0u32.to_be_bytes()).await.unwrap();
            s.read_exact(&mut b).await.unwrap(); // ClientInit
            let mut init = Vec::new();
            init.extend_from_slice(&w.to_be_bytes());
            init.extend_from_slice(&h.to_be_bytes());
            init.extend_from_slice(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
            init.extend_from_slice(&0u32.to_be_bytes());
            s.write_all(&init).await.unwrap();
            let _ = s.write_all(&update).await;
            let mut sink = [0u8; 4096];
            while let Ok(n) = s.read(&mut sink).await {
                if n == 0 {
                    break;
                }
            }
        });
        port
    }

    fn update_hdr(x: u16, y: u16, w: u16, h: u16, enc: i32) -> Vec<u8> {
        let mut m = vec![0u8, 0, 0, 1];
        for v in [x, y, w, h] {
            m.extend_from_slice(&v.to_be_bytes());
        }
        m.extend_from_slice(&enc.to_be_bytes());
        m
    }

    async fn wait_closed(d: &RfbDesk) -> bool {
        for _ in 0..100 {
            if d.is_closed() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    /// A full-screen ZRLE update over a real socket: decoded correctly, and afterwards a screenshot uses the picture
    /// it already has instead of asking the server for the whole screen again.
    #[tokio::test]
    async fn zrle_update_fills_the_screen_and_screenshots_do_not_refetch() {
        use std::io::Write;
        let (w, h) = (80u16, 70u16);
        // two tile rows x two tile columns, each solid but different
        let mut raw = Vec::new();
        for c in [[0u8, 0, 255], [0, 255, 0], [255, 0, 0], [0, 255, 255]] {
            raw.push(1);
            raw.extend(c);
        }
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&raw).unwrap();
        enc.flush().unwrap();
        let z = enc.get_mut().clone();
        let mut u = update_hdr(0, 0, w, h, ENC_ZRLE);
        u.extend_from_slice(&(z.len() as u32).to_be_bytes());
        u.extend_from_slice(&z);
        let port = fake_server(w, h, u).await;
        let mut o = RfbOptions::new("127.0.0.1", port);
        o.connect_timeout = Duration::from_secs(5);
        let d = RfbDesk::connect_with(o, Arc::new(|| {})).await.unwrap();
        assert!(d.wait_coverage(Duration::from_secs(3)).await, "the whole screen arrived");
        assert!(d.synced.load(Ordering::SeqCst));
        let px = |x: u32, y: u32| {
            let fb = d.fb.lock();
            let i = (y as usize * fb.w as usize + x as usize) * 4;
            [fb.rgba[i], fb.rgba[i + 1], fb.rgba[i + 2]]
        };
        assert_eq!((px(0, 0), px(79, 0), px(0, 69), px(79, 69)), ([255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]));
        // the server never answers a second request (the fake only sent one update), so a screenshot that
        // re-requested the whole screen would wait out the 1.5 s refresh; this one does not
        let t = Instant::now();
        let png = d.screenshot().await.unwrap();
        assert!(png.len() > 50 && t.elapsed() < Duration::from_millis(1200), "{:?}", t.elapsed());
        d.shutdown().await;
    }

    #[tokio::test]
    async fn huge_cursor_closes_cleanly() {
        let port = fake_server(16, 16, update_hdr(0, 0, 65535, 65535, ENC_CURSOR)).await;
        let mut o = RfbOptions::new("127.0.0.1", port);
        o.connect_timeout = Duration::from_secs(5);
        let d = RfbDesk::connect_with(o, Arc::new(|| {})).await.unwrap();
        assert!(wait_closed(&d).await, "거대한 Cursor 뒤에도 is_closed()==false");
        assert!(d.take_notes().iter().any(|n| n.contains("Cursor")));
    }

    #[tokio::test]
    async fn huge_rre_count_closes_cleanly() {
        let mut u = update_hdr(0, 0, 4, 4, ENC_RRE);
        u.extend_from_slice(&u32::MAX.to_be_bytes());
        let port = fake_server(16, 16, u).await;
        let d = connect("127.0.0.1", port, None, None, Arc::new(|| {})).await.unwrap();
        assert!(wait_closed(&d).await);
        assert!(d.take_notes().iter().any(|n| n.contains("RRE")));
    }

    #[tokio::test]
    async fn zero_sized_framebuffer_does_not_panic() {
        let port = fake_server(0, 0, Vec::new()).await;
        let d = connect("127.0.0.1", port, None, None, Arc::new(|| {})).await.unwrap();
        assert!(d.zoom([0.0, 0.0, 10.0, 10.0]).await.is_err());
        assert!(d.screenshot().await.is_err());
        d.hold_key("a", f64::NAN).await.unwrap();
        d.shutdown().await;
    }

    #[test]
    fn local_addresses() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
        let sa = |ip: IpAddr| SocketAddr::new(ip, 5900);
        // loopback and unspecified addresses
        assert!(is_local_addr(sa(IpAddr::V4(Ipv4Addr::LOCALHOST))));
        assert!(is_local_addr(sa(IpAddr::V4(Ipv4Addr::new(127, 3, 2, 1)))));
        assert!(is_local_addr(sa(IpAddr::V6(Ipv6Addr::LOCALHOST))));
        assert!(is_local_addr(sa(IpAddr::V4(Ipv4Addr::UNSPECIFIED))));
        assert!(is_local_addr(sa("::ffff:127.0.0.1".parse().unwrap())));
        // documentation addresses (assigned to no computer), multicast and broadcast
        assert!(!is_local_addr(sa("192.0.2.123".parse().unwrap())));
        assert!(!is_local_addr(sa("203.0.113.7".parse().unwrap())));
        assert!(!is_local_addr(sa("2001:db8::1234".parse().unwrap())));
        assert!(!is_local_addr(sa("224.0.0.1".parse().unwrap())));
        assert!(!is_local_addr(sa("ff02::1".parse().unwrap())));
        assert!(!is_local_addr(sa(IpAddr::V4(Ipv4Addr::BROADCAST))));
        // this computer's real interface address (UDP connect only picks a route, no packet is sent).
        // Skipped when there's no network.
        let own = std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| s.connect("192.0.2.1:9").map(|_| s))
            .and_then(|s| s.local_addr())
            .ok()
            .filter(|a| !a.ip().is_unspecified());
        if let Some(a) = own {
            assert!(is_local_addr(SocketAddr::new(a.ip(), 5900)), "{a}");
        }
    }

    #[tokio::test]
    async fn local_host_names() {
        assert!(is_local_host("localhost", 5900).await);
        assert!(is_local_host("127.0.0.1", 5900).await);
        assert!(is_local_host("::1", 5900).await);
        assert!(!is_local_host("192.0.2.50", 5900).await);
    }

    #[tokio::test]
    async fn refuse_local_checks_the_connected_peer() {
        // server that reports how many bytes the client sent (sends the version string first)
        async fn listener() -> (u16, tokio::sync::oneshot::Receiver<usize>) {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let (mut s, _) = l.accept().await.unwrap();
                let _ = s.write_all(b"RFB 003.008\n\x01\x01").await; // version + one security type (None)
                let mut got = 0;
                let mut buf = [0u8; 64];
                while let Ok(Ok(n)) = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await {
                    if n == 0 {
                        break;
                    }
                    got += n;
                }
                let _ = tx.send(got);
            });
            (port, rx)
        }
        // if the peer is this computer, refuse with the given reason without sending anything
        let (port, sent) = listener().await;
        let mut o = RfbOptions::new("127.0.0.1", port);
        o.refuse_local = Some("이 컴퓨터라 거절".into());
        o.connect_timeout = Duration::from_secs(5);
        let e = RfbDesk::connect_with(o, Arc::new(|| {})).await.err().expect("거절해야 함");
        assert_eq!(format!("{e}"), "이 컴퓨터라 거절");
        assert_eq!(sent.await.unwrap(), 0, "거절 전에 서버로 아무것도 보내지 않아야 함");
        // probe connections report the peer address actually connected to
        let (port, _sent) = listener().await;
        let g = probe("127.0.0.1", port, ReplyVersion::Auto, Duration::from_secs(5)).await.unwrap();
        let peer = g.peer.expect("상대 주소");
        assert!(peer.ip().is_loopback() && peer.port() == port && is_local_addr(peer), "{peer}");
    }

    #[test]
    fn os_random_bytes() {
        let a = random_bytes(64);
        let b = random_bytes(64);
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(a.iter().any(|&x| x != 0));
        assert!(random_bytes(0).is_empty());
        // a length that isn't a multiple of 8 (same length as the fallback path's chunk handling)
        assert_eq!(random_bytes(13).len(), 13);
    }

    #[test]
    fn version_parse() {
        assert_eq!(parse_version(b"RFB 003.889\n").unwrap(), (3, 889));
        assert_eq!(parse_version(b"RFB 003.008\n").unwrap(), (3, 8));
        assert!(parse_version(b"HTTP/1.1 200").is_err());
    }
}

#[cfg(test)]
mod zrle_tests {
    use super::*;
    use std::io::Write;

    fn fb(w: u32, h: u32) -> Framebuffer {
        Framebuffer::new(w, h)
    }
    fn px(f: &Framebuffer, x: u32, y: u32) -> [u8; 3] {
        let i = (y as usize * f.w as usize + x as usize) * 4;
        [f.rgba[i], f.rgba[i + 1], f.rgba[i + 2]]
    }

    #[test]
    fn every_tile_kind_decodes() {
        // 4x2 rectangle. CPIXEL bytes are B,G,R.
        let red = [0u8, 0, 255];
        let blue = [255u8, 0, 0];
        // solid
        let mut f = fb(4, 2);
        zrle_tiles(&mut f, 0, 0, 4, 2, &[vec![1u8], red.to_vec()].concat()).unwrap();
        assert_eq!(px(&f, 3, 1), [255, 0, 0]);
        // raw
        let mut f = fb(4, 2);
        let mut d = vec![0u8];
        for i in 0..8u8 { d.extend([i, 0, 255 - i]); }
        zrle_tiles(&mut f, 0, 0, 4, 2, &d).unwrap();
        assert_eq!(px(&f, 1, 0), [254, 0, 1]);
        // packed palette of 2: 1 bit per pixel, rows padded to a byte: 1010 / 0101
        let mut f = fb(4, 2);
        let d = [vec![2u8], red.to_vec(), blue.to_vec(), vec![0b1010_0000, 0b0101_0000]].concat();
        zrle_tiles(&mut f, 0, 0, 4, 2, &d).unwrap();
        assert_eq!((px(&f, 0, 0), px(&f, 1, 0), px(&f, 0, 1), px(&f, 1, 1)), ([0, 0, 255], [255, 0, 0], [255, 0, 0], [0, 0, 255]));
        // plain RLE: red x5 (run byte 4), blue x3 (run byte 2)
        let mut f = fb(4, 2);
        let d = [vec![128u8], red.to_vec(), vec![4], blue.to_vec(), vec![2]].concat();
        zrle_tiles(&mut f, 0, 0, 4, 2, &d).unwrap();
        assert_eq!((px(&f, 0, 1), px(&f, 1, 1), px(&f, 3, 1)), ([255, 0, 0], [0, 0, 255], [0, 0, 255]));
        // palette RLE: index 0 once, index 1 with a run of 7
        let mut f = fb(4, 2);
        let d = [vec![130u8], red.to_vec(), blue.to_vec(), vec![0, 128 | 1, 6]].concat();
        zrle_tiles(&mut f, 0, 0, 4, 2, &d).unwrap();
        assert_eq!((px(&f, 0, 0), px(&f, 1, 0), px(&f, 3, 1)), ([255, 0, 0], [0, 0, 255], [0, 0, 255]));
        // a run past the tile, a missing palette entry and truncated data are errors, not panics
        assert!(zrle_tiles(&mut fb(4, 2), 0, 0, 4, 2, &[vec![128u8], red.to_vec(), vec![200]].concat()).is_err());
        assert!(zrle_tiles(&mut fb(4, 2), 0, 0, 4, 2, &[vec![130u8], red.to_vec(), blue.to_vec(), vec![5, 0]].concat()).is_err());
        assert!(zrle_tiles(&mut fb(4, 2), 0, 0, 4, 2, &[0u8, 1, 2]).is_err());
    }

    #[test]
    fn tiles_cover_a_rectangle_larger_than_64() {
        // 70x3: two tiles per row band (64 + 6 wide), each solid
        let mut f = fb(70, 3);
        let d = [vec![1u8, 0, 0, 255], vec![1u8, 0, 255, 0]].concat();
        zrle_tiles(&mut f, 0, 0, 70, 3, &d).unwrap();
        assert_eq!((px(&f, 63, 2), px(&f, 64, 0), px(&f, 69, 2)), ([255, 0, 0], [0, 255, 0], [0, 255, 0]));
    }

    #[test]
    fn one_zlib_stream_continues_across_rectangles() {
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        let mut rects = Vec::new();
        for n in 0..3u8 {
            // each rectangle: a solid 1-tile picture of a different colour, flushed so it can be read on its own
            enc.write_all(&[1, n, n, n]).unwrap();
            enc.flush().unwrap();
            rects.push(std::mem::take(enc.get_mut()));
        }
        let mut z = Zrle::new();
        for (n, r) in rects.iter().enumerate() {
            let out = z.inflate(r, 1 << 16).unwrap().to_vec();
            assert_eq!(out, vec![1, n as u8, n as u8, n as u8]);
        }
        // a rectangle that inflates to far more than its pixels is refused (zlib bomb)
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(&vec![0u8; 4 << 20]).unwrap();
        let bomb = enc.finish().unwrap();
        assert!(Zrle::new().inflate(&bomb, 1 << 12).is_err());
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::normalize_endpoint as n;

    #[test]
    fn typed_addresses_are_cleaned_up() {
        assert_eq!(n(" 192.168.0.20 ", 5900), ("192.168.0.20".into(), 5900));
        assert_eq!(n("192.168.0.20:5901", 5900), ("192.168.0.20".into(), 5901));
        assert_eq!(n("vnc://vm.local:5902/", 5900), ("vm.local".into(), 5902));
        assert_eq!(n("[fe80::1]:5905", 5900), ("fe80::1".into(), 5905));
        assert_eq!(n("fe80::1", 5900), ("fe80::1".into(), 5900));
        assert_eq!(n("me@host", 5900), ("host".into(), 5900));
        assert_eq!(n("host:", 5900), ("host".into(), 5900));
    }
}
