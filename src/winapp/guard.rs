//! 노출 감시기: 앱의 창이 사용자 화면(어느 모니터든)에 걸치면 즉시 화면 밖으로 옮기고 기록한다.
//!
//! - 전용 스레드가 앱 프로세스별 WinEvent 훅(EVENT_OBJECT_CREATE/SHOW/LOCATIONCHANGE, 프로세스 밖 훅)과
//!   8ms 주기 검사(≈125Hz)를 함께 돈다. 훅은 창이 만들어지거나 보이는 순간 알려 주므로 대부분 한 화면 프레임 안에 옮긴다.
//! - 새로 만들어진(아직 숨은) 창은 미리 화면 밖으로 옮겨 둔다(사전 주차). 그러면 앱이 나중에 그 창을 보여 줘도
//!   대기열에 먼저 들어간 이동 요청이 첫 그리기 전에 처리된다 — 대화상자·메시지 상자의 노출이 수십 ms → 수 ms 로 줄었다.
//! - 모든 이동은 비동기(SWP_ASYNCWINDOWPOS)다. 앱 스레드가 바빠도 감시기는 멈추지 않는다.
//! - Mode A 의 앱 창에는 WS_EX_TOOLWINDOW 를 더해 작업 표시줄·Alt+Tab 에 나타나지 않게 한다(스타일 변경은 앱 스레드를
//!   기다리므로 별도 작업 스레드에서). 실행 단계에서 숨김(SW_HIDE)으로 시작한 주 창은 화면 밖에서 SW_SHOWNOACTIVATE 로
//!   보이게 한다(PrintWindow 는 보이는 창만 그린다).
//! - 앱이 전경(키보드 포커스)을 가져가면 기록하고 직전 창에 되돌린다. 입력을 보내는 동안에는 전경 잠금
//!   (LockSetForegroundWindow)을 잠시 걸어 앱이 전경을 가져가지 못하게 한다.
//! - 앱이 시스템 클립보드에 쓰면(앱 자체의 복사 기능) 기록한다 (내용은 읽지 않는다). Mode A 의 앱은 Job 의 UI 제한으로
//!   클립보드를 아예 읽거나 쓸 수 없으므로 이 기록은 사실상 Mode B(사용자 앱에 붙기)용이다.
//! - 셸 창(작업 표시줄·바탕 화면)은 어떤 경우에도 옮기지 않고 뷰로 보이지 않는다.

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

/// 앱 창 목록·노출 판단에서 뺄 보조 창 클래스 (입력기·툴팁·드롭다운 목록·메뉴·그림자)
pub fn is_helper_class(class: &str) -> bool {
    matches!(
        class,
        "IME" | "tooltips_class32" | "ComboLBox" | "#32768" | "SysShadow" | "OleMainThreadWndClass" | "OleDdeWndClass"
            | "CicMarshalWndClass" | "Tapi32WndClass" | "MSCTFIME Composition" | "TaskSwitcherWnd" | "Ghost"
            // 입력 언어 표시기: 숨은 데스크톱에서는 텍스트 서비스가 앱 프로세스 안에 이 창들을 만든다
            | "UAC_InputIndicatorOverlayWnd" | "UAC Input Indicator" | "Shell_InputSwitchTopLevelWindow"
    ) || class.starts_with("MSCTFIME")
        || class.starts_with("GDI+ Hook Window")
        || class.starts_with(".NET-BroadcastEventWindow")
}

/// Windows 셸 창의 클래스 (작업 표시줄·보조 모니터 작업 표시줄·바탕 화면)
pub fn is_shell_class(class: &str) -> bool {
    matches!(class, "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "Progman" | "WorkerW")
}

/// Windows 셸의 창인가 — 어떤 경우에도 옮기거나 에이전트에게 보이지 않는다.
/// 'WorkerW' 는 셸 구성 요소를 쓰는 다른 앱도 만들 수 있는 이름이라 셸 프로세스의 것일 때만 해당한다.
pub fn is_shell_window(w: &TopWindow) -> bool {
    match w.class.as_str() {
        "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "Progman" => true,
        "WorkerW" => super::launch::shell_pids().contains(&w.pid),
        _ => false,
    }
}

/// 에이전트가 볼 수 있는 앱 창인가 (보이는 최상위 창, 보조 창·셸 창 아님, 충분한 크기)
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

/// 숨김 실행 때문에 보이지 않게 된 앱의 주 창 후보 (화면 밖에서 보이게 할 창)
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

/// 붙기(Mode B)에서 복원할 원래 상태
#[derive(Clone, Copy, Debug)]
pub struct Saved {
    pub placement: Option<w32::Placement>,
    pub was_visible: bool,
    /// 스냅된 창의 실제 화면 사각형 (배치의 보통 위치와 다를 때만)
    pub arranged: Option<(i32, i32, i32, i32)>,
}

pub struct Shared {
    stop: AtomicBool,
    mode: NativeMode,
    job: Option<Arc<Handle>>,
    pids: Mutex<Vec<u32>>,
    origin: Mutex<(i32, i32)>,
    events: Mutex<Vec<String>>,
    /// 지금까지 잰 가장 긴 노출 (ms)
    pub worst_flash_ms: AtomicU64,
    pub flashes: AtomicU64,
    pub fg_steals: AtomicU64,
    pub clip_writes: AtomicU64,
    lease_until: Mutex<Option<Instant>>,
    lock_held: AtomicBool,
    reveal_until: Mutex<Option<Instant>>,
    /// Mode B: 사용자가 앱을 다시 앞으로 가져왔다 (그 뒤로는 옮기지 않는다)
    pub reclaimed: AtomicBool,
    /// 종료·복원 중: 더 옮기지 않는다
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

    /// 감시기가 남긴 기록 (가져가면 비워진다)
    pub fn take_events(&self) -> Vec<String> {
        std::mem::take(&mut *self.shared.events.lock())
    }

    pub fn pids(&self) -> Vec<u32> {
        self.shared.pids.lock().clone()
    }

    pub fn origin(&self) -> (i32, i32) {
        *self.shared.origin.lock()
    }

    /// 지금부터 dur 동안 전경 잠금을 유지한다 (입력을 보낸 직후 앱이 전경을 가져가지 못하게)
    pub fn lease(&self, dur: Duration) {
        let until = Instant::now() + dur;
        let mut l = self.shared.lease_until.lock();
        if l.map(|t| t < until).unwrap_or(true) {
            *l = Some(until);
        }
    }

    /// 호출한 쪽이 방금 잠금을 걸었음을 알린다 (임대가 끝나면 감시기가 푼다). 실패(false)는 기존 잠금 기록을 지우지 않는다.
    pub fn adopt_lock(&self, held: bool) {
        if held {
            self.shared.lock_held.store(true, Ordering::SeqCst);
        }
    }

    /// 지금부터 dur 동안 숨김으로 시작한 주 창을 화면 밖에서 보이게 한다 (실행 단계)
    pub fn reveal_for(&self, dur: Duration) {
        *self.shared.reveal_until.lock() = Some(Instant::now() + dur);
    }

    pub fn reclaimed(&self) -> bool {
        self.shared.reclaimed.load(Ordering::SeqCst)
    }

    pub fn worst_flash_ms(&self) -> u64 {
        self.shared.worst_flash_ms.load(Ordering::SeqCst)
    }

    /// Mode B: 옮겨 둔 사용자 창을 원래 자리·상태로 되돌린다 (활성화하지 않음). 더 이상 옮기지 않는다.
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
    // 활성화되지 않게 잠금을 걸고 되돌린다 (최대화 복원은 활성화를 동반한다)
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

// ───────────────────────── 감시 스레드 ─────────────────────────

thread_local! {
    static PENDING: RefCell<Vec<(u32, Hwnd)>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "system" fn on_win_event(_hook: isize, event: u32, hwnd: HWND, id_object: i32, id_child: i32, _t: u32, _time: u32) {
    // OBJID_WINDOW(0) + CHILDID_SELF(0) 인 창 자체의 사건만
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
        vscreen: pw::virtual_screen(),
    };
    // 시작 시점의 전경 창 (앱 것이 아니면 되돌릴 대상)
    let fg = w32::foreground();
    if !lp.is_ours(fg) {
        lp.prev_fg = fg;
    }
    // Mode B: 사용자가 이미 띄워 둔 숨은 창은 건드리지 않는다 (사용자가 트레이에서 열 때 화면 밖에 뜨지 않게)
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

    /// 앱 프로세스 목록(Job)과 프로세스별 훅을 맞춘다
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
                // 모니터 구성이 바뀌었다 → 주차 위치를 다시 계산
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
        // 사라진 창 정리 (노출 중이던 창이 닫혔으면 노출 끝으로 기록)
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
            // 최대화된 창은 모니터에 붙는다 → 보통 크기로 바꾸며 화면 밖에 둔다 (활성화 없이)
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
            // 셸(작업 표시줄·바탕 화면)은 절대 옮기지 않는다 (실행 전 검사·붙기 검사를 지나쳐도)
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
        // Mode A: 작업 표시줄·Alt+Tab 에서 빼기 (보이는 앱 창, 또는 곧 보이게 할 주 창)
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
        // 새로 만든 숨은 창은 미리 화면 밖에 둔다 (보여질 때 첫 프레임 전에 자리를 잡게)
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
        // Mode A 실행 단계: 숨김으로 시작한 주 창을 화면 밖에서 보이게 (작업 표시줄 스타일을 먼저 바꾼 뒤)
        if reveal_cand && !self.s.revealed.lock().contains(&h) {
            let owner = w32::owner(h);
            // 소유된 창은 소유자가 비활성(=모달 대화상자가 숨은 채 걸려 있음)일 때만 보이게 한다
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
        // 최소화된 앱 창은 캡처가 안 되므로 화면 밖에서 원래 크기로 되돌린다 (Mode B 는 원래 상태를 저장해 두고 끝나면 다시 최소화)
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

    /// Mode B: 사용자 창을 처음 옮기기 전에 원래 배치를 저장하고 복구 기록에 적는다 (비정상 종료 대비)
    fn save_original(&self, h: Hwnd, pid: u32, was_visible: bool) {
        let mut saved = self.s.saved.lock();
        if saved.contains_key(&h) {
            return;
        }
        let placement = w32::get_placement(h);
        // 스냅된 창은 배치의 보통 위치가 스냅 전 사각형이다 → 실제 사각형도 기억한다
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

    /// 확장 스타일 변경은 앱 스레드가 메시지를 처리할 때까지 막히므로 작업 스레드에서 한다
    fn start_restyle(&mut self, h: Hwnd, ex: u32, visible: bool) {
        self.restyle_pending.insert(h);
        let done = self.restyle_done.clone();
        let new_ex = (ex | w32::WS_EX_TOOLWINDOW) & !w32::WS_EX_APPWINDOW;
        let _ = std::thread::Builder::new().name("ghostdesk-winapp-style".into()).spawn(move || {
            if visible {
                // 이미 보이는 창은 숨겼다가 스타일을 바꿔 다시 보여야 작업 표시줄 단추가 사라진다
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
        // 우리가 입력을 보낸 직후(전경 잠금 임대 중)이고 사용자의 실제 입력이 없었다면 우리 입력 때문이다 → 가로챔으로 처리
        let ours = self.s.lease_until.lock().map(|t| Instant::now() < t).unwrap_or(false) && w32::ms_since_user_input() > 500;
        match self.s.mode {
            NativeMode::Attached if !ours => {
                // 사용자가 작업 표시줄·Alt+Tab 등으로 자기 앱을 앞으로 가져왔다 → 돌려준다
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
            // 사용자 조작(Alt·클릭)으로 잠금이 풀릴 수 있으니 주기적으로 다시 건다
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
        if owner != 0 && self.is_ours(owner) {
            self.s.clip_writes.fetch_add(1, Ordering::SeqCst);
            self.record(
                "앱이 자기 복사 기능으로 시스템 클립보드에 썼습니다 — 사용자의 클립보드 내용이 바뀌었을 수 있습니다 (GhostDesk 는 읽거나 쓰지 않음)"
                    .into(),
            );
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
                tool_window: false,
            };
            if c != "WorkerW" || shell_pid != 0 {
                assert!(!is_view_window(&w), "{c}");
            }
        }
        assert!(!is_shell_class("CabinetWClass"));
        // 이 PC 의 실제 작업 표시줄·바탕 화면
        for w in pw::windows_of(&shell) {
            if is_shell_class(&w.class) {
                assert!(is_shell_window(&w) && !is_view_window(&w), "{}", w.class);
            }
        }
    }
}
