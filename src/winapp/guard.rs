//! Exposure guard: if an app window touches the user's screen (any monitor), move it off-screen
//! immediately and log it.
//!
//! - A dedicated thread runs per-process WinEvent hooks (EVENT_OBJECT_CREATE/SHOW/
//!   LOCATIONCHANGE, out-of-context) alongside an 8 ms check (≈125 Hz). The hooks fire the moment
//!   a window is created or shown, so most windows are moved within one display frame.
//! - Newly created (still hidden) windows are parked off-screen ahead of time. When the app shows
//!   one later, the move request already queued is handled before the first paint, which cut
//!   dialog and message box exposure from tens of ms to a few.
//! - Every move is async (SWP_ASYNCWINDOWPOS), so a busy app thread never stalls the guard.
//! - Mode A app windows get WS_EX_TOOLWINDOW so they stay off the taskbar and Alt+Tab (style
//!   changes wait on the app thread, so they run on a separate worker). Main windows started
//!   hidden (SW_HIDE) at launch are shown off-screen with SW_SHOWNOACTIVATE, since PrintWindow
//!   only paints visible windows.
//! - If the app takes the foreground (keyboard focus), it's logged and handed back to the
//!   previous window. While input is being sent, the foreground lock (LockSetForegroundWindow)
//!   is held briefly so the app can't take it.
//! - Writes by the app to the system clipboard are undone: the user's clipboard is copied into
//!   memory whenever the user changes it, and if the app overwrites or empties it while the agent
//!   is working it, that copy is put back. The copy never leaves the process (not logged, saved or
//!   sent) and is dropped with the screen. The Job's UI limits don't stop every app (measured:
//!   Character Map changed the clipboard twice from one grid click), and mode B (attaching to a
//!   user's app) has no Job limits at all.
//! - Shell windows (taskbar, desktop) are never moved and never shown as views.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging as wm;

use super::w32::{self, Handle, Hwnd};
use crate::native::NativeMode;
use crate::platform::win::{self as pw, TopWindow};

const SCAN_EVERY: Duration = Duration::from_millis(8);
const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
const EVENT_OBJECT_CREATE: u32 = 0x8000;
const EVENT_OBJECT_SHOW: u32 = 0x8002;
const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;
const WINEVENT_SKIPOWNPROCESS: u32 = 0x0002;
const QS_ALLINPUT: u32 = 0x04FF;
const PM_REMOVE: u32 = 0x0001;

/// Helper window classes left out of app window lists and exposure checks (IME, tooltips,
/// drop-down lists, menus, shadows).
pub fn is_helper_class(class: &str) -> bool {
    matches!(
        class,
        "IME" | "tooltips_class32" | "ComboLBox" | "#32768" | "SysShadow" | "OleMainThreadWndClass" | "OleDdeWndClass"
            | "CicMarshalWndClass" | "Tapi32WndClass" | "MSCTFIME Composition" | "TaskSwitcherWnd" | "Ghost"
            // input language indicators: on a hidden desktop the text services create these inside the app
            // process
            | "UAC_InputIndicatorOverlayWnd" | "UAC Input Indicator" | "Shell_InputSwitchTopLevelWindow"
    ) || class.starts_with("MSCTFIME")
        || class.starts_with("GDI+ Hook Window")
        || class.starts_with(".NET-BroadcastEventWindow")
}

/// Window classes of the Windows shell (taskbar, secondary taskbars, desktop).
pub fn is_shell_class(class: &str) -> bool {
    matches!(class, "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "Progman" | "WorkerW")
}

/// Whether a window belongs to the Windows shell. Such windows are never moved or shown to the
/// agent.
///
/// 'WorkerW' is a name other apps using shell components can create too, so it only counts when
/// owned by the shell process.
pub fn is_shell_window(w: &TopWindow) -> bool {
    match w.class.as_str() {
        "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "Progman" => true,
        "WorkerW" => super::launch::shell_pids().contains(&w.pid),
        _ => false,
    }
}

/// Whether the agent may see this app window (visible top-level, not a helper or shell window,
/// big enough).
pub fn is_view_window(w: &TopWindow) -> bool {
    let (l, t, r, b) = w.rect;
    w.visible
        && !w.minimized
        && !w.cloaked
        && w.alpha != Some(0)
        && r - l >= 40
        && b - t >= 30
        && !is_helper_class(&w.class)
        && !is_shell_window(w)
}

/// Candidate main window left invisible by the hidden launch (to be shown off-screen).
fn is_reveal_candidate(w: &TopWindow, first_rect: (i32, i32, i32, i32), first_ex: u32) -> bool {
    let st = w32::style(w.hwnd);
    let (l, t, r, b) = first_rect;
    !w.visible
        && st & w32::WS_CHILD == 0
        && st & w32::WS_MINIMIZE == 0
        && (st & w32::WS_CAPTION == w32::WS_CAPTION || w.class == "#32770")
        && first_ex & w32::WS_EX_TOOLWINDOW == 0
        && r - l >= 100
        && b - t >= 60
        && l > -10000
        && t > -10000
        && !is_helper_class(&w.class)
}

/// Original state to restore when attaching (mode B).
#[derive(Clone, Copy, Debug)]
pub struct Saved {
    pub placement: Option<w32::Placement>,
    pub was_visible: bool,
    /// Actual screen rectangle of a snapped window (only when it differs from the placement's
    /// normal position).
    pub arranged: Option<(i32, i32, i32, i32)>,
}

pub struct Shared {
    stop: AtomicBool,
    mode: NativeMode,
    job: Option<Arc<Handle>>,
    pids: Mutex<Vec<u32>>,
    origin: Mutex<(i32, i32)>,
    events: Mutex<Vec<String>>,
    /// Longest exposure measured so far (ms).
    pub worst_flash_ms: AtomicU64,
    pub flashes: AtomicU64,
    pub fg_steals: AtomicU64,
    pub clip_writes: AtomicU64,
    lease_until: Mutex<Option<Instant>>,
    lock_held: AtomicBool,
    reveal_until: Mutex<Option<Instant>>,
    /// Mode B: the user brought the app back to the front (it isn't moved after that).
    pub reclaimed: AtomicBool,
    /// Shutting down or restoring: no more moves.
    paused: AtomicBool,
    saved: Mutex<HashMap<Hwnd, Saved>>,
    revealed: Mutex<HashSet<Hwnd>>,
    restyled: Mutex<HashSet<Hwnd>>,
}

pub struct Guard {
    pub shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Guard {
    pub fn start(mode: NativeMode, pids: Vec<u32>, job: Option<Arc<Handle>>) -> Guard {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            mode,
            job,
            pids: Mutex::new(pids),
            origin: Mutex::new(pw::offscreen_origin()),
            events: Mutex::new(vec![]),
            worst_flash_ms: AtomicU64::new(0),
            flashes: AtomicU64::new(0),
            fg_steals: AtomicU64::new(0),
            clip_writes: AtomicU64::new(0),
            lease_until: Mutex::new(None),
            lock_held: AtomicBool::new(false),
            reveal_until: Mutex::new(None),
            reclaimed: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            saved: Mutex::new(HashMap::new()),
            revealed: Mutex::new(HashSet::new()),
            restyled: Mutex::new(HashSet::new()),
        });
        let s2 = shared.clone();
        let thread = std::thread::Builder::new()
            .name("ghostdesk-winapp-guard".into())
            .spawn(move || run(s2))
            .ok();
        Guard { shared, thread }
    }

    /// Events logged by the guard (cleared when taken).
    pub fn take_events(&self) -> Vec<String> {
        std::mem::take(&mut *self.shared.events.lock())
    }

    pub fn pids(&self) -> Vec<u32> {
        self.shared.pids.lock().clone()
    }

    pub fn origin(&self) -> (i32, i32) {
        *self.shared.origin.lock()
    }

    /// Holds the foreground lock for `dur` from now (so the app can't take the foreground right
    /// after input is sent).
    pub fn lease(&self, dur: Duration) {
        let until = Instant::now() + dur;
        let mut l = self.shared.lease_until.lock();
        if l.map(|t| t < until).unwrap_or(true) {
            *l = Some(until);
        }
    }

    /// Tells the guard the caller just took the lock (the guard releases it when the lease ends). A
    /// failure (false) doesn't clear an existing lock record.
    pub fn adopt_lock(&self, held: bool) {
        if held {
            self.shared.lock_held.store(true, Ordering::SeqCst);
        }
    }

    /// Shows main windows that started hidden, off-screen, for `dur` from now (launch phase).
    pub fn reveal_for(&self, dur: Duration) {
        *self.shared.reveal_until.lock() = Some(Instant::now() + dur);
    }

    pub fn reclaimed(&self) -> bool {
        self.shared.reclaimed.load(Ordering::SeqCst)
    }

    pub fn worst_flash_ms(&self) -> u64 {
        self.shared.worst_flash_ms.load(Ordering::SeqCst)
    }

    /// Mode B: returns the moved user windows to their original place and state (without
    /// activating), and stops moving them.
    pub fn restore_all(&self) -> usize {
        self.shared.paused.store(true, Ordering::SeqCst);
        restore_saved(&self.shared)
    }

    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stop();
    }
}

fn restore_saved(s: &Shared) -> usize {
    let saved: Vec<(Hwnd, Saved)> = s.saved.lock().drain().collect();
    if saved.is_empty() {
        return 0;
    }
    let hwnds: Vec<Hwnd> = saved.iter().map(|(h, _)| *h).collect();
    let mut n = 0;
    // lock first so nothing activates (restoring maximized implies activation)
    let locked = w32::lock_foreground(true);
    for (h, sv) in saved {
        if !w32::is_window(h) {
            continue;
        }
        if let Some(pl) = sv.placement {
            if super::recovery::apply_original(h, pl, sv.was_visible, sv.arranged) {
                n += 1;
            }
        }
    }
    if locked {
        std::thread::sleep(Duration::from_millis(200));
        w32::lock_foreground(false);
    }
    super::recovery::forget(&hwnds);
    n
}

// ---- guard thread ----

thread_local! {
    static PENDING: RefCell<Vec<(u32, Hwnd)>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "system" fn on_win_event(_hook: isize, event: u32, hwnd: HWND, id_object: i32, id_child: i32, _t: u32, _time: u32) {
    // only events for the window itself: OBJID_WINDOW(0) + CHILDID_SELF(0)
    if hwnd.is_null() || id_object != 0 || id_child != 0 {
        return;
    }
    PENDING.with(|p| p.borrow_mut().push((event, hwnd as Hwnd)));
}

struct Loop {
    s: Arc<Shared>,
    hooks: HashMap<u32, (isize, isize)>,
    fg_hook: isize,
    seen: HashSet<Hwnd>,
    first: HashMap<Hwnd, ((i32, i32, i32, i32), u32)>,
    onscreen: HashMap<Hwnd, (Instant, String)>,
    last_park: HashMap<Hwnd, Instant>,
    restyle_pending: HashSet<Hwnd>,
    restyle_done: Arc<Mutex<HashSet<Hwnd>>>,
    fixed_minimized: HashMap<Hwnd, u32>,
    prev_fg: Hwnd,
    fg_restores: Vec<Instant>,
    flagged_fg: HashSet<Hwnd>,
    last_scan: Instant,
    last_pid_refresh: Instant,
    last_lock_try: Instant,
    clip_seq: u32,
    /// The user's clipboard as last seen (memory only), put back if the app overwrites it.
    clip_saved: Option<w32::ClipSnapshot>,
    /// Message-only window that owns restored clipboard data.
    clip_owner: Hwnd,
    clip_restores: Vec<Instant>,
    vscreen: (i32, i32, i32, i32),
}

fn run(s: Arc<Shared>) {
    let mut lp = Loop {
        s: s.clone(),
        hooks: HashMap::new(),
        fg_hook: w32::set_win_event_hook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            on_win_event,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        ),
        seen: HashSet::new(),
        first: HashMap::new(),
        onscreen: HashMap::new(),
        last_park: HashMap::new(),
        restyle_pending: HashSet::new(),
        restyle_done: Arc::new(Mutex::new(HashSet::new())),
        fixed_minimized: HashMap::new(),
        prev_fg: 0,
        fg_restores: vec![],
        flagged_fg: HashSet::new(),
        last_scan: Instant::now() - SCAN_EVERY,
        last_pid_refresh: Instant::now() - Duration::from_secs(1),
        last_lock_try: Instant::now() - Duration::from_secs(1),
        clip_seq: w32::clipboard_sequence(),
        clip_saved: w32::clipboard_snapshot(),
        clip_owner: w32::message_window(),
        clip_restores: vec![],
        vscreen: pw::virtual_screen(),
    };
    // foreground window at startup (handed back to if it isn't the app's)
    let fg = w32::foreground();
    if !lp.is_ours(fg) {
        lp.prev_fg = fg;
    }
    // mode B: leave hidden windows the user already had alone (so opening one from the tray
    // doesn't show it off-screen)
    if s.mode == NativeMode::Attached {
        for w in pw::windows_of(&s.pids.lock().clone()) {
            if !w.visible {
                lp.seen.insert(w.hwnd);
                lp.first.insert(w.hwnd, (w.rect, w32::ex_style(w.hwnd)));
            }
        }
    }
    while !s.stop.load(Ordering::SeqCst) {
        lp.sync_hooks();
        unsafe {
            wm::MsgWaitForMultipleObjects(0, std::ptr::null(), 0, 4, QS_ALLINPUT);
            let mut msg: wm::MSG = std::mem::zeroed();
            while wm::PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                wm::TranslateMessage(&msg);
                wm::DispatchMessageW(&msg);
            }
        }
        let pending = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
        for (ev, h) in pending {
            if ev == EVENT_SYSTEM_FOREGROUND {
                lp.check_foreground();
            } else if w32::is_toplevel(h) {
                let w = pw::window_info(h as HWND);
                if lp.pid_ours(w.pid) {
                    lp.visit(&w);
                }
            }
        }
        if lp.last_scan.elapsed() >= SCAN_EVERY {
            lp.scan();
        }
        lp.manage_lock();
        lp.check_clipboard();
    }
    w32::destroy_window(lp.clip_owner);
    for (_, (a, b)) in lp.hooks.drain() {
        w32::unhook_win_event(a);
        w32::unhook_win_event(b);
    }
    w32::unhook_win_event(lp.fg_hook);
    if s.lock_held.swap(false, Ordering::SeqCst) {
        w32::lock_foreground(false);
    }
}

impl Loop {
    fn pid_ours(&self, pid: u32) -> bool {
        self.s.pids.lock().contains(&pid)
    }

    fn is_ours(&self, h: Hwnd) -> bool {
        h != 0 && self.pid_ours(w32::thread_pid(h).1)
    }

    fn record(&self, msg: String) {
        eprintln!("[ghostdesk] 앱 창 감시: {msg}");
        self.s.events.lock().push(msg);
    }

    /// Syncs the per-process hooks with the app's process list (the Job).
    fn sync_hooks(&mut self) {
        if self.last_pid_refresh.elapsed() >= Duration::from_millis(100) {
            self.last_pid_refresh = Instant::now();
            if let Some(job) = &self.s.job {
                let now = super::launch::job_pids(job.0);
                if !now.is_empty() {
                    *self.s.pids.lock() = now;
                }
            }
            let vs = pw::virtual_screen();
            if vs != self.vscreen {
                // monitor layout changed, so recompute the parking spot
                self.vscreen = vs;
                *self.s.origin.lock() = pw::offscreen_origin();
            }
        }
        let pids = self.s.pids.lock().clone();
        for p in &pids {
            if !self.hooks.contains_key(p) {
                let a = w32::set_win_event_hook(EVENT_OBJECT_CREATE, EVENT_OBJECT_SHOW, on_win_event, *p, WINEVENT_OUTOFCONTEXT);
                let b = w32::set_win_event_hook(
                    EVENT_OBJECT_LOCATIONCHANGE,
                    EVENT_OBJECT_LOCATIONCHANGE,
                    on_win_event,
                    *p,
                    WINEVENT_OUTOFCONTEXT,
                );
                self.hooks.insert(*p, (a, b));
            }
        }
        let gone: Vec<u32> = self.hooks.keys().filter(|p| !pids.contains(p)).copied().collect();
        for p in gone {
            if let Some((a, b)) = self.hooks.remove(&p) {
                w32::unhook_win_event(a);
                w32::unhook_win_event(b);
            }
        }
    }

    fn scan(&mut self) {
        self.last_scan = Instant::now();
        let pids = self.s.pids.lock().clone();
        let wins = pw::windows_of(&pids);
        let alive: HashSet<Hwnd> = wins.iter().map(|w| w.hwnd).collect();
        for w in &wins {
            self.visit(w);
        }
        // drop windows that are gone (a window closed while exposed ends its exposure)
        let closed: Vec<Hwnd> = self.onscreen.keys().filter(|h| !alive.contains(h)).copied().collect();
        for h in closed {
            if let Some((t0, desc)) = self.onscreen.remove(&h) {
                self.finish_flash(&desc, t0, "닫힘");
            }
        }
        self.seen.retain(|h| alive.contains(h));
        self.first.retain(|h, _| alive.contains(h));
        self.last_park.retain(|h, _| alive.contains(h));
        self.check_foreground();
    }

    fn finish_flash(&self, desc: &str, t0: Instant, how: &str) {
        let ms = t0.elapsed().as_millis() as u64;
        self.s.flashes.fetch_add(1, Ordering::SeqCst);
        self.s.worst_flash_ms.fetch_max(ms, Ordering::SeqCst);
        self.record(format!("{desc} 이(가) 사용자 화면에 약 {ms}ms 나타났다가 {how}"));
    }

    fn park(&mut self, w: &TopWindow) {
        let now = Instant::now();
        if let Some(t) = self.last_park.get(&w.hwnd) {
            if now.duration_since(*t) < Duration::from_millis(15) {
                return;
            }
        }
        self.last_park.insert(w.hwnd, now);
        let (ox, oy) = *self.s.origin.lock();
        if self.s.mode == NativeMode::Attached {
            self.save_original(w.hwnd, w.pid, w.visible);
        }
        if w32::is_zoomed(w.hwnd) {
            // maximized windows snap to a monitor, so restore them to normal size off-screen (without
            // activating)
            if let Some(mut pl) = w32::get_placement(w.hwnd) {
                let (l, t, r, b) = pl.normal;
                pl.normal = (ox, oy, ox + (r - l).max(200), oy + (b - t).max(150));
                pl.show_cmd = w32::SW_SHOWNOACTIVATE as u32;
                w32::set_placement_async(w.hwnd, &pl);
                return;
            }
        }
        w32::move_async(w.hwnd, ox, oy);
    }

    fn visit(&mut self, w: &TopWindow) {
        if is_shell_window(w) {
            // never move the shell (taskbar, desktop), even if it got past the launch and attach checks
            return;
        }
        let h = w.hwnd;
        let first = self.seen.insert(h);
        if first {
            self.first.insert(h, (w.rect, w32::ex_style(h)));
        }
        if self.s.paused.load(Ordering::SeqCst) || self.s.reclaimed.load(Ordering::SeqCst) {
            return;
        }
        let launched = self.s.mode == NativeMode::Launched;
        let (r0, ex0) = self.first.get(&h).copied().unwrap_or((w.rect, 0));
        let reveal_cand = launched && self.reveal_active() && is_reveal_candidate(w, r0, ex0);
        // mode A: keep off the taskbar and Alt+Tab (visible app windows, or the main window about to be
        // shown)
        if launched && !self.restyle_pending.contains(&h) && !is_helper_class(&w.class) {
            let ex = w32::ex_style(h);
            let unowned = w32::owner(h) == 0;
            let wants = (unowned || ex & w32::WS_EX_APPWINDOW != 0) && (w.visible || reveal_cand);
            if wants && (ex & w32::WS_EX_TOOLWINDOW == 0 || ex & w32::WS_EX_APPWINDOW != 0) {
                self.start_restyle(h, ex, w.visible);
            } else if wants {
                self.restyle_done.lock().insert(h);
            }
        }
        if w.shows_on_screen() {
            if !self.onscreen.contains_key(&h) {
                let desc = format!(
                    "창 '{}'({})",
                    if w.title.is_empty() { "(제목 없음)" } else { &w.title },
                    w.class
                );
                self.onscreen.insert(h, (Instant::now(), desc));
            }
            self.park(w);
            return;
        }
        if let Some((t0, desc)) = self.onscreen.remove(&h) {
            self.finish_flash(&desc, t0, "화면 밖으로 옮겼습니다");
        }
        // park newly created hidden windows off-screen in advance (so they're in place before the first
        // frame when shown)
        if first && !w.visible && !w.class.starts_with("MSCTFIME") && w.class != "IME" {
            let (l, t, r, b) = w.rect;
            if r - l > 1 && b - t > 1 && pw::intersects_any_monitor(w.rect) {
                let (ox, oy) = *self.s.origin.lock();
                if self.s.mode == NativeMode::Attached {
                    self.save_original(h, w.pid, false);
                }
                w32::move_async(h, ox, oy);
            }
        }
        // mode A launch phase: show the main window that started hidden, off-screen (after the taskbar
        // style change)
        if reveal_cand && !self.s.revealed.lock().contains(&h) {
            let owner = w32::owner(h);
            // show owned windows only while the owner is disabled (a hidden modal dialog is up)
            let owner_ok = owner == 0 || (self.is_ours(owner) && !w32::is_enabled(owner));
            let styled = owner != 0 || self.restyle_done.lock().contains(&h);
            if owner_ok && styled {
                let (ox, oy) = *self.s.origin.lock();
                w32::move_async(h, ox, oy);
                w32::show_async(h, w32::SW_SHOWNOACTIVATE);
                self.s.revealed.lock().insert(h);
                self.s.restyled.lock().insert(h);
            }
        }
        // minimized app windows can't be captured, so restore them to normal size off-screen (mode B
        // saves the original state and minimizes again at the end)
        if w.visible && w.minimized && !is_helper_class(&w.class) && w32::style(h) & w32::WS_CAPTION == w32::WS_CAPTION {
            let n = self.fixed_minimized.entry(h).or_insert(0);
            if *n < 3 {
                *n += 1;
                if self.s.mode == NativeMode::Attached {
                    self.save_original(h, w.pid, true);
                }
                if let Some(mut pl) = w32::get_placement(h) {
                    let (ox, oy) = *self.s.origin.lock();
                    let (l, t, r, b) = pl.normal;
                    pl.normal = (ox, oy, ox + (r - l).max(200), oy + (b - t).max(150));
                    pl.show_cmd = w32::SW_SHOWNOACTIVATE as u32;
                    w32::set_placement_async(h, &pl);
                }
            }
        }
    }

    /// Mode B: saves the original placement before the first move of a user window and writes it to
    /// the recovery log (in case of a crash).
    fn save_original(&self, h: Hwnd, pid: u32, was_visible: bool) {
        let mut saved = self.s.saved.lock();
        if saved.contains_key(&h) {
            return;
        }
        let placement = w32::get_placement(h);
        // a snapped window's placement holds the pre-snap rectangle, so remember the actual one too
        let arranged = placement
            .filter(|pl| was_visible && pl.show_cmd == w32::SW_SHOWNORMAL && !w32::is_iconic(h) && !w32::is_zoomed(h))
            .and_then(|pl| {
                let actual = w32::window_rect(h);
                w32::arranged_rect_differs(w32::normal_to_screen(h, pl.normal), actual).then_some(actual)
            });
        if let Some(pl) = &placement {
            super::recovery::remember(pid, h, pl, was_visible, arranged);
        }
        saved.insert(h, Saved { placement, was_visible, arranged });
    }

    fn reveal_active(&self) -> bool {
        self.s.reveal_until.lock().map(|t| Instant::now() < t).unwrap_or(false)
    }

    /// Extended style changes block until the app thread handles its messages, so do them on a
    /// worker thread.
    fn start_restyle(&mut self, h: Hwnd, ex: u32, visible: bool) {
        self.restyle_pending.insert(h);
        let done = self.restyle_done.clone();
        let new_ex = (ex | w32::WS_EX_TOOLWINDOW) & !w32::WS_EX_APPWINDOW;
        let _ = std::thread::Builder::new().name("ghostdesk-winapp-style".into()).spawn(move || {
            if visible {
                // an already visible window has to be hidden, restyled and shown again for its taskbar button
                // to go away
                w32::show_async(h, w32::SW_HIDE);
            }
            w32::set_ex_style_blocking(h, new_ex);
            w32::frame_changed_async(h);
            if visible {
                w32::show_async(h, w32::SW_SHOWNOACTIVATE);
            }
            done.lock().insert(h);
        });
    }

    fn check_foreground(&mut self) {
        let fg = w32::foreground();
        if fg == 0 {
            return;
        }
        if !self.is_ours(fg) {
            self.prev_fg = fg;
            self.flagged_fg.clear();
            return;
        }
        if self.s.paused.load(Ordering::SeqCst) {
            return;
        }
        // right after our own input (foreground lease active) and with no real user input, it's ours,
        // so treat it as an interception
        let ours = self.s.lease_until.lock().map(|t| Instant::now() < t).unwrap_or(false) && w32::ms_since_user_input() > 500;
        match self.s.mode {
            NativeMode::Attached if !ours => {
                // the user brought their app to the front (taskbar, Alt+Tab, …), so hand it back
                if !self.s.reclaimed.swap(true, Ordering::SeqCst) {
                    let n = restore_saved(&self.s);
                    self.record(format!(
                        "사용자가 앱을 다시 앞으로 가져와 창 {n}개를 원래 자리로 되돌렸습니다 — 사용자가 직접 쓰는 중일 수 있습니다"
                    ));
                }
            }
            _ => {
                if !self.flagged_fg.insert(fg) {
                    return;
                }
                self.s.fg_steals.fetch_add(1, Ordering::SeqCst);
                self.fg_restores.retain(|t| t.elapsed() < Duration::from_secs(10));
                let restored = if self.fg_restores.len() < 5 && self.prev_fg != 0 {
                    self.fg_restores.push(Instant::now());
                    w32::restore_foreground(self.prev_fg)
                } else {
                    false
                };
                self.record(format!(
                    "앱 창 '{}' 이(가) 키보드 포커스(전경)를 가져갔습니다 — {}",
                    w32::window_text(fg),
                    if restored { "직전 창에 되돌렸습니다" } else { "되돌리지 못했습니다 (그동안의 키 입력이 앱으로 갔을 수 있음)" }
                ));
            }
        }
    }

    fn manage_lock(&mut self) {
        let want = self.s.lease_until.lock().map(|t| Instant::now() < t).unwrap_or(false);
        if want {
            // user actions (Alt, clicks) can release the lock, so retake it periodically
            if self.last_lock_try.elapsed() >= Duration::from_millis(400) {
                self.last_lock_try = Instant::now();
                if w32::lock_foreground(true) {
                    self.s.lock_held.store(true, Ordering::SeqCst);
                }
            }
        } else if self.s.lock_held.swap(false, Ordering::SeqCst) {
            w32::lock_foreground(false);
        }
    }

    fn check_clipboard(&mut self) {
        let seq = w32::clipboard_sequence();
        if seq == self.clip_seq {
            return;
        }
        self.clip_seq = seq;
        let owner = w32::clipboard_owner();
        // the app wrote (its window owns the clipboard), or it was emptied with no owner right after the agent's input
        let agent_recent = self.s.lease_until.lock().is_some_and(|t| Instant::now() < t + Duration::from_millis(1500));
        let by_app = (owner != 0 && self.is_ours(owner)) || (owner == 0 && agent_recent);
        // in an app the user also uses (mode B), their own copy in it is theirs to keep
        let users_own = self.s.mode == NativeMode::Attached && w32::ms_since_user_input() < 500 && !agent_recent;
        if !by_app || users_own {
            // the user's new content: this is what gets put back if the app touches it
            if owner != self.clip_owner {
                self.clip_saved = w32::clipboard_snapshot();
            }
            return;
        }
        self.s.clip_writes.fetch_add(1, Ordering::SeqCst);
        // never fight an app that keeps writing: a few restores in a short while at most
        self.clip_restores.retain(|t| t.elapsed() < Duration::from_secs(5));
        let restored = self.clip_restores.len() < 6 && self.clip_saved.as_ref().is_some_and(|snap| w32::clipboard_restore(snap, self.clip_owner));
        if restored {
            self.clip_restores.push(Instant::now());
            // our own write moved the sequence on again
            self.clip_seq = w32::clipboard_sequence();
            self.record("앱이 시스템 클립보드를 바꿔서 사용자의 원래 클립보드 내용으로 되돌렸습니다 (되돌릴 내용은 메모리에만 두고 기록·전송하지 않음)".into());
        } else {
            self.record("앱이 자기 복사 기능으로 시스템 클립보드에 썼고 원래 내용으로 되돌리지 못했습니다 — 사용자의 클립보드 내용이 바뀌었을 수 있습니다".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_classes() {
        for c in ["IME", "MSCTFIME UI", "tooltips_class32", "ComboLBox", "#32768", "Shell_InputSwitchTopLevelWindow"] {
            assert!(is_helper_class(c), "{c}");
        }
        for c in ["#32770", "Notepad", "DialerClass", "CharGridWClass"] {
            assert!(!is_helper_class(c), "{c}");
        }
    }

    #[test]
    fn shell_windows_are_never_views() {
        let shell = super::super::launch::shell_pids();
        let shell_pid = shell.first().copied().unwrap_or(0);
        for c in ["Shell_TrayWnd", "Shell_SecondaryTrayWnd", "Progman", "WorkerW"] {
            assert!(is_shell_class(c), "{c}");
            let w = TopWindow {
                hwnd: 1,
                pid: if c == "WorkerW" { shell_pid } else { 1 },
                title: String::new(),
                class: c.into(),
                rect: (0, 0, 1920, 48),
                visible: true,
                minimized: false,
                cloaked: false,
                alpha: None,
            };
            if c != "WorkerW" || shell_pid != 0 {
                assert!(!is_view_window(&w), "{c}");
            }
        }
        assert!(!is_shell_class("CabinetWClass"));
        // this PC's real taskbar and desktop
        for w in pw::windows_of(&shell) {
            if is_shell_class(&w.class) {
                assert!(is_shell_window(&w) && !is_view_window(&w), "{}", w.class);
            }
        }
    }
}
