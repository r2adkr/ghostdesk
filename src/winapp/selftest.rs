//! `ghostdesk --selftest-native [--out DIR] [--skip-refusals] [--skip-charmap] [--skip-target] [--skip-attach]`
//!
//! 이 PC 를 쓰는 사람을 방해하지 않는지 지켜보면서(25ms 마다: 앱 창이 어느 모니터에 보이는지, 실제 커서 위치,
//! 전경 창, 시스템 클립보드 변경 번호) 다음을 시험한다.
//! 1. 거부 규칙: 스토어 앱 실행 별칭(메모장)·스토어 앱 실행기(계산기)·콘솔·관리자 권한 앱 — 아무것도 띄우지 않고 거부.
//! 2. 문자표(charmap.exe, 실제 Win32 앱): 숨김 실행, 스크린샷, ui_tree, 픽셀 클릭(문자 눈금·선택 단추), 한글+영문 입력,
//!    Ctrl+A/C/V(내부 클립보드), 체크 상자 press, 콤보 상자 choose, 휠, 거부(오른쪽 클릭·복사/도움말 단추·Alt·Ctrl+S), 종료.
//! 3. 시험 대상 창(이 실행 파일의 `--target`): 메뉴 가속기 흉내(Ctrl+S → 파일 > 저장), menus/menu, 차단 메뉴, 단추·체크·콤보,
//!    메시지 상자(새 창 자동 전환·노출 측정·확인 누르기), "저장할까요?" 대화상자가 떠도 2초 뒤 강제 종료.
//! 4. Mode B: 화면 밖에 이미 떠 있는 시험 대상에 동의 하에 붙어 입력하고, 끝나면 죽이지 않고 원래 자리로 되돌림.
//!
//! 내부 모드: `--target [--offscreen-visible]` = 시험 대상 창 (사용자 화면에는 나타나지 않는다).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde_json::json;

use super::w32;
use super::workspace::NativeAppWorkspace;
use crate::browser::MouseButton;
use crate::native::{NativeMode, NativeOptions};
use crate::platform::win as pw;

struct TestLog {
    ok: Vec<String>,
    fail: Vec<String>,
    warn: Vec<String>,
}

impl TestLog {
    fn check(&mut self, name: &str, ok: bool, detail: impl AsRef<str>) {
        let line = format!("{name}: {}", detail.as_ref());
        println!("[{}] {line}", if ok { "PASS" } else { "FAIL" });
        if ok {
            self.ok.push(line);
        } else {
            self.fail.push(line);
        }
    }
    fn warn(&mut self, name: &str, detail: impl AsRef<str>) {
        let line = format!("{name}: {}", detail.as_ref());
        println!("[WARN] {line}");
        self.warn.push(line);
    }
}

// ───────────────────────── 사용자 보호 감시 (25ms) ─────────────────────────

/// 시험 시작부터 지난 초 (단계·감시 기록을 맞춰 보려고)
fn ts() -> f64 {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_secs_f64()
}

fn step(s: &str) {
    eprintln!("[selftest] {:.3}s {s}", ts());
}

#[derive(Default, Debug)]
struct Report {
    /// 1ms 간격 창 노출 검사 횟수 / 보인 횟수
    probes: u64,
    onscreen_probes: u64,
    /// 25ms 간격 표본 (커서·전경·클립보드 + 창)
    samples: u64,
    onscreen_samples: u64,
    longest_run_ms: u64,
    onscreen: Vec<String>,
    fg_to_app: Vec<String>,
    fg_other_changes: u64,
    cursor_moves: u64,
    clip_changes: u64,
    clip_by_app: u64,
}

struct Sampler {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Report>>,
}

/// 이 시험 프로세스가 띄운 자식 프로세스 (시험 대상 앱들)
fn child_pids() -> Vec<u32> {
    let me = std::process::id();
    pw::processes()
        .into_iter()
        .filter(|(_, pp, name)| *pp == me && !name.eq_ignore_ascii_case("conhost.exe"))
        .map(|(p, _, _)| p)
        .collect()
}

fn start_sampler() -> Sampler {
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let handle = std::thread::spawn(move || {
        let mut r = Report::default();
        let mut last_fg = w32::foreground();
        let mut last_cur = pw::cursor_pos();
        let mut last_clip = w32::clipboard_sequence();
        let mut run: Option<Instant> = None;
        let mut kids: Vec<u32> = vec![];
        let mut last_kids = Instant::now() - Duration::from_secs(1);
        let mut last_slow = Instant::now() - Duration::from_secs(1);
        while !s2.load(Ordering::SeqCst) {
            // 앱 창 노출은 1ms 마다 (자식 프로세스 목록은 50ms 마다 갱신)
            if last_kids.elapsed() >= Duration::from_millis(50) {
                kids = child_pids();
                last_kids = Instant::now();
            }
            r.probes += 1;
            let on: Vec<_> = pw::windows_of(&kids).into_iter().filter(|w| w.shows_on_screen()).collect();
            if on.is_empty() {
                if let Some(t) = run.take() {
                    r.longest_run_ms = r.longest_run_ms.max(t.elapsed().as_millis() as u64);
                }
            } else {
                r.onscreen_probes += 1;
                run.get_or_insert_with(Instant::now);
                for w in on {
                    let d = format!("'{}'({}) @({},{})", w.title, w.class, w.rect.0, w.rect.1);
                    if !r.onscreen.contains(&d) {
                        r.onscreen.push(d);
                    }
                }
            }
            if last_slow.elapsed() < Duration::from_millis(25) {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            last_slow = Instant::now();
            r.samples += 1;
            let on: Vec<_> = pw::windows_of(&kids).into_iter().filter(|w| w.shows_on_screen()).collect();
            if !on.is_empty() {
                r.onscreen_samples += 1;
            }
            let fg = w32::foreground();
            if fg != last_fg {
                last_fg = fg;
                if kids.contains(&w32::thread_pid(fg).1) {
                    r.fg_to_app.push(format!("{:.3}s '{}' ({})", ts(), w32::window_text(fg), w32::class_name(fg)));
                    eprintln!("[selftest] {:.3}s 전경이 앱 창으로 바뀜", ts());
                } else {
                    r.fg_other_changes += 1;
                }
            }
            let cur = pw::cursor_pos();
            if cur != last_cur {
                last_cur = cur;
                r.cursor_moves += 1;
            }
            let clip = w32::clipboard_sequence();
            if clip != last_clip {
                last_clip = clip;
                r.clip_changes += 1;
                if kids.contains(&w32::thread_pid(w32::clipboard_owner()).1) {
                    r.clip_by_app += 1;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        if let Some(t) = run.take() {
            r.longest_run_ms = r.longest_run_ms.max(t.elapsed().as_millis() as u64);
        }
        r
    });
    Sampler { stop, handle: Some(handle) }
}

impl Sampler {
    fn finish(mut self) -> Report {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.take().and_then(|h| h.join().ok()).unwrap_or_default()
    }
}

// ───────────────────────── ui_tree 해석 ─────────────────────────

#[derive(Clone, Debug)]
struct El {
    id: usize,
    ty: String,
    name: String,
    value: String,
    rect: [f64; 4],
    flags: String,
}

impl El {
    fn center(&self) -> (f64, f64) {
        (self.rect[0] + self.rect[2] / 2.0, self.rect[1] + self.rect[3] / 2.0)
    }
}

fn parse_tree(tree: &str) -> Vec<El> {
    let mut out = Vec::new();
    for line in tree.lines().skip(1) {
        let t = line.trim_start();
        let mut it = t.splitn(3, ' ');
        let (Some(id), Some(ty)) = (it.next().and_then(|s| s.parse::<usize>().ok()), it.next()) else { continue };
        let rest = it.next().unwrap_or("");
        let name = rest.strip_prefix('"').and_then(|r| r.find('"').map(|e| r[..e].to_string())).unwrap_or_default();
        let value = rest.find("value=\"").map(|i| &rest[i + 7..]).and_then(|r| r.find('"').map(|e| r[..e].to_string())).unwrap_or_default();
        let (Some(lb), Some(rb)) = (rest.rfind('['), rest.rfind(']')) else { continue };
        let nums: Vec<f64> = rest[lb + 1..rb].split(',').filter_map(|s| s.trim().parse().ok()).collect();
        if nums.len() != 4 {
            continue;
        }
        out.push(El { id, ty: ty.to_string(), name, value, rect: [nums[0], nums[1], nums[2], nums[3]], flags: rest[rb + 1..].to_string() });
    }
    out
}

fn find<'a>(els: &'a [El], ty: &str, names: &[&str]) -> Option<&'a El> {
    els.iter().find(|e| e.ty == ty && (names.is_empty() || names.iter().any(|n| e.name.to_lowercase().starts_with(&n.to_lowercase()))))
}

fn png_ok(png: &[u8]) -> (bool, String) {
    match image::load_from_memory(png) {
        Ok(img) => {
            let rgb = img.to_rgb8();
            let mut colors = std::collections::HashSet::new();
            for p in rgb.pixels().step_by(((rgb.width() * rgb.height()) as usize / 5000).max(1)) {
                colors.insert(p.0);
            }
            (colors.len() > 8, format!("{}x{}, 색 {}가지", rgb.width(), rgb.height(), colors.len()))
        }
        Err(e) => (false, format!("PNG 해석 실패: {e}")),
    }
}

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1).cloned())
}

fn sys32(name: &str) -> PathBuf {
    PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())).join("System32").join(name)
}

fn opts(app: &Path) -> NativeOptions {
    // GHOSTDESK_SELFTEST_HOST=offscreen 이면 예전 방식(사용자 데스크톱의 화면 밖 창)으로 같은 시험을 돌린다
    let host = std::env::var("GHOSTDESK_SELFTEST_HOST").ok().and_then(|h| crate::native::NativeHost::parse(&h)).unwrap_or_default();
    NativeOptions { app: app.to_string_lossy().to_string(), new_instance: true, live_fps: 0.0, host, ..Default::default() }
}

fn noop_sink() -> crate::browser::FrameSink {
    Arc::new(|_b: &[u8]| {})
}

// ───────────────────────── 진입점 ─────────────────────────

pub async fn selftest(args: &[String]) -> Result<()> {
    if args.iter().any(|a| a == "--target") {
        return super::testapp::run(args.iter().any(|a| a == "--offscreen-visible"), args.iter().any(|a| a == "--minimized"));
    }
    let _ = ts();
    pw::set_dpi_aware();
    // 시험 대상(이 실행 파일, 콘솔 서브시스템)을 띄울 수 있게
    std::env::set_var("GHOSTDESK_WINAPP_SELFTEST", "1");
    let out_dir = PathBuf::from(
        arg_value(args, "--out").unwrap_or_else(|| std::env::temp_dir().join(format!("ghostdesk-winapp-selftest-{}", std::process::id())).to_string_lossy().to_string()),
    );
    std::fs::create_dir_all(&out_dir)?;
    let login_fixture = out_dir.join("login-state-fixture.txt");
    std::fs::write(&login_fixture, "synthetic-session-state")?;
    std::env::set_var("GHOSTDESK_SELFTEST_LOGIN_STATE", &login_fixture);
    println!("출력 폴더: {}", out_dir.display());
    // Mode B 복구 기록은 사용자 데이터 폴더가 아니라 시험 출력 폴더에
    std::env::set_var("GHOSTDESK_WINAPP_RECOVERY_DIR", &out_dir);
    let (vl, vt, vr, vb) = pw::virtual_screen();
    println!("가상 화면: ({vl},{vt})-({vr},{vb}), 화면 밖 주차 위치: {:?}", pw::offscreen_origin());
    let mut log = TestLog { ok: vec![], fail: vec![], warn: vec![] };
    let clip0 = w32::clipboard_sequence();
    let sampler = start_sampler();
    let t0 = Instant::now();

    if !args.iter().any(|a| a == "--skip-refusals") {
        refusals(&mut log).await;
        installed(&mut log);
    }
    if !args.iter().any(|a| a == "--skip-charmap") {
        if let Err(e) = charmap(&mut log, &out_dir).await {
            log.check("charmap", false, format!("{e:#}"));
        }
    }
    if !args.iter().any(|a| a == "--skip-target") {
        if let Err(e) = target(&mut log, &out_dir).await {
            log.check("target", false, format!("{e:#}"));
        }
    }
    if !args.iter().any(|a| a == "--skip-attach") {
        if let Err(e) = attach(&mut log, &out_dir).await {
            log.check("attach", false, format!("{e:#}"));
        }
    }
    if args.iter().any(|a| a == "--with-dialer") {
        let keys = [r"Software\Microsoft\Dialer", r"Software\Microsoft\Windows\CurrentVersion\Applets\Dialer", r"Software\Microsoft\Windows\CurrentVersion\Telephony"];
        let snaps: Vec<RegSnapshot> = keys.iter().map(|k| RegSnapshot::take(k)).collect();
        if let Err(e) = dialer(&mut log, &out_dir).await {
            log.check("dialer", false, format!("{e:#}"));
        }
        let ok = snaps.iter().all(|s| s.restore());
        log.check("dialer-user-settings-restored", ok, keys.join(", "));
    }

    // 남은 자식이 있으면 정리 (우리가 띄운 것만)
    tokio::time::sleep(Duration::from_millis(300)).await;
    let left = child_pids();
    log.check("no-leftover-processes", left.is_empty(), format!("{left:?}"));
    let rep = sampler.finish();
    println!(
        "\n── 사용자 보호 감시 ({:.1}초: 창 노출 1ms 간격 {}회, 커서·전경·클립보드 25ms 간격 {}회) ──",
        t0.elapsed().as_secs_f64(),
        rep.probes,
        rep.samples
    );
    println!("{rep:#?}");
    if rep.onscreen_probes == 0 {
        log.check("probe-no-app-window-on-screen", true, format!("1ms 검사 {}회 동안 앱 창이 어느 모니터에도 보인 적 없음", rep.probes));
    } else {
        // 짧게라도 보였다. 길게(>=100ms) 남았으면 실패로 본다.
        let ok = rep.longest_run_ms < 100;
        log.check(
            "probe-app-window-on-screen",
            ok,
            format!("1ms 검사 {}회 중 {}회 보임, 가장 긴 연속 {}ms: {:?}", rep.probes, rep.onscreen_probes, rep.longest_run_ms, rep.onscreen),
        );
    }
    log.check("foreground-never-taken", rep.fg_to_app.is_empty(), format!("{:?}", rep.fg_to_app));
    if rep.fg_other_changes > 0 {
        log.warn("foreground-changed-by-others", format!("{}회 (사용자 활동 — 앱 창으로 바뀐 적은 없음)", rep.fg_other_changes));
    }
    if rep.cursor_moves > 0 {
        log.warn("cursor-moved", format!("{}회 — GhostDesk 는 커서를 움직이는 API(SetCursorPos·SendInput)를 쓰지 않으므로 사용자 움직임", rep.cursor_moves));
    } else {
        log.check("cursor-unchanged", true, "실제 마우스 커서 위치가 그대로");
    }
    log.check("clipboard-not-written-by-app", rep.clip_by_app == 0, format!("앱이 쓴 횟수 {}", rep.clip_by_app));
    let clip1 = w32::clipboard_sequence();
    if clip1 == clip0 {
        log.check("system-clipboard-untouched", true, format!("변경 번호 {clip0} 그대로"));
    } else {
        log.warn("system-clipboard-changed", format!("{clip0} → {clip1} (앱이 쓴 것이 아님 — 사용자 활동)"));
    }

    println!("\n=== 결과: 통과 {} / 실패 {} / 경고 {} ===", log.ok.len(), log.fail.len(), log.warn.len());
    for f in &log.fail {
        println!("  실패: {f}");
    }
    for w in &log.warn {
        println!("  경고: {w}");
    }
    if !log.fail.is_empty() {
        bail!("Windows 앱 자체 시험 실패 {}건", log.fail.len());
    }
    Ok(())
}

// ───────────────────────── 1. 거부 규칙 ─────────────────────────

async fn refusals(log: &mut TestLog) {
    let win = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()));
    let cases: Vec<(PathBuf, &str, &str)> = vec![
        (sys32("notepad.exe"), "notepad-alias", "실행 별칭"),
        (sys32("calc.exe"), "calc-store-stub", "계산기"),
        (sys32("cmd.exe"), "console-app", "콘솔"),
        (sys32("eudcedit.exe"), "elevation-required", "관리자 권한"),
        // 셸: 새로 띄우면 실행 중인 셸이 탐색기 창을 사용자 화면에 열고, 붙으면 작업 표시줄·바탕 화면이 옮겨진다
        (win.join("explorer.exe"), "shell-explorer", "셸"),
        (sys32("control.exe"), "shell-launcher-control", "셸"),
    ];
    for (p, name, want) in cases {
        if !p.is_file() {
            continue;
        }
        if name == "notepad-alias" && !super::launch::is_alias_redirect(&p) {
            continue;
        }
        let before = child_pids();
        let r = NativeAppWorkspace::launch(opts(&p), noop_sink()).await;
        let msg = match &r {
            Ok(_) => "실행되어 버림".to_string(),
            Err(e) => format!("{e:#}"),
        };
        if let Ok(ws) = r {
            ws.shutdown().await;
        }
        let spawned = child_pids().len() > before.len();
        log.check(&format!("refuse-{name}"), msg.contains(want) && !spawned, msg);
    }
    let r = NativeAppWorkspace::launch(opts(Path::new("존재하지않는앱xyz")), noop_sink()).await;
    log.check("refuse-unknown-app", r.as_ref().err().map(|e| e.to_string().contains("찾지 못했습니다")).unwrap_or(false), r.err().map(|e| e.to_string()).unwrap_or_default());
    // 이름으로 적은 'explorer' 도 (PATH 에서 찾은 뒤) 거부
    let before = child_pids();
    let r = NativeAppWorkspace::launch(opts(Path::new("explorer")), noop_sink()).await;
    let msg = r.as_ref().err().map(|e| format!("{e:#}")).unwrap_or_else(|| "실행되어 버림".into());
    if let Ok(ws) = r {
        ws.shutdown().await;
    }
    log.check("refuse-shell-by-name", msg.contains("셸") && child_pids().len() <= before.len(), msg);
}

fn installed(log: &mut TestLog) {
    let t = Instant::now();
    let apps = crate::native::installed_apps();
    log.check(
        "installed-apps",
        !apps.is_empty(),
        format!("{}개 ({}ms): {:?}…", apps.len(), t.elapsed().as_millis(), apps.iter().take(6).map(|(n, _)| n.as_str()).collect::<Vec<_>>()),
    );
    let p = crate::native::resolve_app_query("charmap");
    log.check("resolve-charmap", p.as_ref().map(|p| p.to_string_lossy().to_lowercase().ends_with("charmap.exe")).unwrap_or(false), format!("{p:?}"));
}

// ───────────────────────── 2. 문자표 ─────────────────────────

/// HKCU 키 하나의 값들을 시험 전 그대로 되돌리기 위한 스냅숏 (문자표는 닫을 때 글꼴·고급 보기 상태를 저장한다)
struct RegSnapshot {
    sub: Vec<u16>,
    existed: bool,
    values: Vec<(Vec<u16>, u32, Vec<u8>)>,
}

impl RegSnapshot {
    fn take(sub: &str) -> RegSnapshot {
        use windows_sys::Win32::System::Registry::{RegCloseKey, RegEnumValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ};
        let w = pw::wide(sub);
        let mut snap = RegSnapshot { sub: w.clone(), existed: false, values: vec![] };
        unsafe {
            let mut h: HKEY = std::ptr::null_mut();
            if RegOpenKeyExW(HKEY_CURRENT_USER, w.as_ptr(), 0, KEY_READ, &mut h) != 0 {
                return snap;
            }
            snap.existed = true;
            for i in 0..256u32 {
                let mut name = vec![0u16; 512];
                let mut nlen = name.len() as u32;
                let mut ty = 0u32;
                let mut data = vec![0u8; 65536];
                let mut dlen = data.len() as u32;
                if RegEnumValueW(h, i, name.as_mut_ptr(), &mut nlen, std::ptr::null(), &mut ty, data.as_mut_ptr(), &mut dlen) != 0 {
                    break;
                }
                name.truncate(nlen as usize);
                name.push(0);
                data.truncate(dlen as usize);
                snap.values.push((name, ty, data));
            }
            RegCloseKey(h);
        }
        snap
    }

    fn restore(&self) -> bool {
        use windows_sys::Win32::System::Registry::{
            RegCloseKey, RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE,
        };
        let now = RegSnapshot::take(&String::from_utf16_lossy(&self.sub[..self.sub.len() - 1]));
        if !now.existed {
            return !self.existed;
        }
        unsafe {
            if !self.existed {
                // 시험 전에 없던 키는 통째로 지운다
                return RegDeleteTreeW(HKEY_CURRENT_USER, self.sub.as_ptr()) == 0;
            }
            let mut h: HKEY = std::ptr::null_mut();
            if RegOpenKeyExW(HKEY_CURRENT_USER, self.sub.as_ptr(), 0, KEY_READ | KEY_WRITE, &mut h) != 0 {
                return false;
            }
            for (n, _, _) in &now.values {
                if !self.values.iter().any(|(m, _, _)| m == n) {
                    RegDeleteValueW(h, n.as_ptr());
                }
            }
            for (n, ty, data) in &self.values {
                RegSetValueExW(h, n.as_ptr(), 0, *ty, data.as_ptr(), data.len() as u32);
            }
            RegCloseKey(h);
        }
        true
    }
}

async fn charmap(log: &mut TestLog, out: &Path) -> Result<()> {
    let exe = sys32("charmap.exe");
    if !exe.is_file() {
        log.warn("charmap", "charmap.exe 가 없어 건너뜀");
        return Ok(());
    }
    let snap = RegSnapshot::take(r"Software\Microsoft\CharMap");
    let r = charmap_run(log, out, &exe).await;
    let restored = snap.restore();
    log.check("charmap-user-settings-restored", restored, format!("HKCU\\Software\\Microsoft\\CharMap 값 {}개로 되돌림", snap.values.len()));
    r
}

async fn charmap_run(log: &mut TestLog, out: &Path, exe: &Path) -> Result<()> {
    let frames = Arc::new(AtomicU64::new(0));
    let f2 = frames.clone();
    let t = Instant::now();
    let ws = NativeAppWorkspace::launch(
        NativeOptions { live_fps: 2.0, ..opts(exe) },
        Arc::new(move |_b: &[u8]| {
            f2.fetch_add(1, Ordering::SeqCst);
        }),
    )
    .await?;
    let pid = ws.pid() as u32;
    log.check("charmap-launch", ws.mode() == NativeMode::Launched, format!("pid {pid} '{}' {}ms", ws.app_name(), t.elapsed().as_millis()));
    let want_hidden = opts(exe).host != crate::native::NativeHost::Offscreen;
    log.check(
        "charmap-host",
        ws.on_hidden_desktop() == want_hidden,
        format!("숨은 데스크톱 {} (기대 {want_hidden})", ws.on_hidden_desktop()),
    );
    let r = charmap_steps(log, out, &ws).await;
    let exp = ws.exposure_report();
    log.check("charmap-exposure", exp.is_empty(), if exp.is_empty() { "노출 기록 없음".to_string() } else { format!("{exp:?}") });
    log.check("charmap-live-frames", frames.load(Ordering::SeqCst) > 0, format!("{} 프레임", frames.load(Ordering::SeqCst)));
    let ts = Instant::now();
    ws.shutdown().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    log.check("charmap-shutdown", !crate::platform::proc::pid_alive(pid), format!("pid {pid}, {}ms", ts.elapsed().as_millis()));
    r
}

async fn charmap_steps(log: &mut TestLog, out: &Path, ws: &NativeAppWorkspace) -> Result<()> {
    let (views, cur) = ws.views();
    log.check("charmap-views", !views.is_empty() && cur.is_some(), format!("{:?}", views.iter().map(|v| format!("{} [{}]", v.title, v.detail)).collect::<Vec<_>>()));
    let png = ws.screenshot().await?;
    std::fs::write(out.join("charmap-1.png"), &png)?;
    let (ok, d) = png_ok(&png);
    log.check("charmap-screenshot", ok, d);
    let tree = ws.app_control(&json!({"action": "ui_tree"})).await?;
    println!("{tree}");
    let els = parse_tree(&tree);
    log.check("charmap-ui-tree", els.len() >= 8, format!("{} 요소", els.len()));
    let grid = els.iter().filter(|e| e.rect[2] > 200.0 && e.rect[3] > 100.0).max_by(|a, b| (a.rect[2] * a.rect[3]).total_cmp(&(b.rect[2] * b.rect[3]))).cloned();
    let edit = els.iter().find(|e| e.ty == "Edit" || e.ty == "Document").cloned();
    let adv = find(&els, "CheckBox", &[]).cloned();
    let (Some(grid), Some(edit), Some(adv)) = (grid, edit, adv) else {
        bail!("문자 눈금·입력 칸·고급 보기 체크 상자를 ui_tree 에서 찾지 못했습니다");
    };
    // 주의: 문자표의 '선택' 단추·문자 더블클릭은 문자표가 스스로 시스템 클립보드를 거쳐 글자를 넣으므로 누르지 않는다.

    // a. 픽셀 클릭: 고급 보기 체크 상자 → 체크되고 요소가 늘어난다
    ws.click(Some(adv.center()), MouseButton::Left, 1, 0).await?;
    let _ = ws.screenshot().await?;
    let tree2 = ws.app_control(&json!({"action": "ui_tree"})).await?;
    let els2 = parse_tree(&tree2);
    let adv2 = find(&els2, "CheckBox", &[]).cloned();
    log.check(
        "charmap-pixel-click-checkbox",
        adv2.as_ref().map(|c| c.flags.contains(" checked")).unwrap_or(false) && els2.len() > els.len(),
        format!("요소 {} → {} {:?}", els.len(), els2.len(), ws.take_notes()),
    );
    std::fs::write(out.join("charmap-2-advanced.png"), ws.screenshot().await?)?;

    // b. 픽셀 클릭: 문자 눈금의 넷째 칸 → '선택한 문자' 설명(U+....)이 바뀐다
    let desc = |els: &[El]| els.iter().find(|e| e.name.starts_with("U+")).map(|e| e.name.clone()).unwrap_or_default();
    let before = desc(&els2);
    let cell = (grid.rect[0] + grid.rect[2] * 0.17, grid.rect[1] + grid.rect[3] * 0.06);
    ws.click(Some(cell), MouseButton::Left, 1, 0).await?;
    let tree3 = ws.app_control(&json!({"action": "ui_tree"})).await?;
    let after = desc(&parse_tree(&tree3));
    log.check("charmap-pixel-click-grid", !after.is_empty() && after != before, format!("'{before}' → '{after}' {:?}", ws.take_notes()));

    // c. 한글+영문 입력 (입력 칸 클릭 → End → type)
    ws.click(Some(edit.center()), MouseButton::Left, 1, 0).await?;
    ws.key("End", 1).await?;
    ws.type_text("한글 ABC 123").await?;
    println!("  notes: {:?}", ws.take_notes());
    let v = read_value(ws, &edit).await?;
    log.check("charmap-type-korean", v.contains("한글 ABC 123"), format!("{v:?}"));

    // d. Ctrl+A / Ctrl+C / End / Ctrl+V (내부 클립보드) — macOS 식 cmd+a 도
    let clip = w32::clipboard_sequence();
    ws.key("cmd+a", 1).await?;
    ws.key("ctrl+c", 1).await?;
    ws.key("End", 1).await?;
    ws.key("ctrl+v", 1).await?;
    let notes = ws.take_notes();
    println!("  notes: {notes:?}");
    let v2 = read_value(ws, &edit).await?;
    log.check(
        "charmap-ctrl-a-c-v-internal-clipboard",
        v2.matches("한글 ABC 123").count() == 2 && w32::clipboard_sequence() == clip,
        format!("{v2:?} (시스템 클립보드 변경 번호 {} → {})", clip, w32::clipboard_sequence()),
    );
    ws.key("ctrl+z", 1).await?;
    println!("  ctrl+z notes: {:?}", ws.take_notes());

    // e. 글꼴 콤보 상자 choose (펼치지 않고)
    let _ = ws.screenshot().await?;
    let tree4 = ws.app_control(&json!({"action": "ui_tree"})).await?;
    let els4 = parse_tree(&tree4);
    if let Some(combo) = find(&els4, "ComboBox", &["글꼴", "font"]) {
        let r = ws.app_control(&json!({"action": "choose", "id": combo.id, "option": "Consolas"})).await;
        log.check("charmap-choose-font", r.as_ref().map(|s| s.contains("확인됨")).unwrap_or(false), format!("{r:?}"));
    }

    // f. 거부: 오른쪽 클릭, 복사 단추, 도움말, Alt·F10·Shift+F10·Alt+Space, Ctrl+S(메뉴 없음), 끌기(끌어서 놓기 가능한 컨트롤)
    let r = ws.click(Some(grid.center()), MouseButton::Right, 1, 0).await;
    log.check("charmap-refuse-right-click", r.is_err(), format!("{:?}", r.err().map(|e| e.to_string())));
    if let Some(copy) = find(&els4, "Button", &["복사", "copy"]) {
        let r = ws.click(Some(copy.center()), MouseButton::Left, 1, 0).await;
        log.check("charmap-refuse-copy-button", r.as_ref().err().map(|e| e.to_string().contains("클립보드")).unwrap_or(false), format!("{:?}", r.err().map(|e| e.to_string())));
    }
    if let Some(help) = els4.iter().find(|e| e.name.starts_with("도움말") || e.name.to_lowercase().starts_with("help")) {
        let r = ws.app_control(&json!({"action": "press", "id": help.id})).await;
        log.check("charmap-refuse-help", r.is_err(), format!("{} → {:?}", help.ty, r.err().map(|e| e.to_string())));
        let r = ws.click(Some(help.center()), MouseButton::Left, 1, 0).await;
        log.check("charmap-refuse-help-click", r.is_err(), format!("{:?}", r.err().map(|e| e.to_string())));
    }
    for k in ["alt", "shift+F10", "F10", "alt+space", "F1", "super+r"] {
        let r = ws.key(k, 1).await;
        log.check(&format!("charmap-refuse-key-{k}"), r.is_err(), format!("{:?}", r.err().map(|e| e.to_string())));
    }
    let r = ws.key("ctrl+s", 1).await;
    log.check("charmap-ctrl-s-without-menu-errors", r.is_err(), format!("{:?}", r.err().map(|e| e.to_string())));
    let m = ws.app_control(&json!({"action": "menus"})).await?;
    log.check("charmap-menus-none", m.contains("메뉴 막대가 없습니다"), m);
    let r = ws.drag((grid.rect[0] + 10.0, grid.rect[1] + 10.0), (grid.rect[0] + 60.0, grid.rect[1] + 40.0), 0).await;
    log.check("charmap-refuse-ole-drag", r.as_ref().err().map(|e| e.to_string().contains("끌기")).unwrap_or(false), format!("{:?}", r.err().map(|e| e.to_string())));

    // g. 휠(문자 눈금은 휠을 처리하지 않아 옆의 스크롤 막대로 대체된다)·확대
    let shot0 = ws.screenshot().await?;
    let r = ws.scroll(Some(grid.center()), 0.0, 300.0, 0).await;
    let shot1 = ws.screenshot().await?;
    std::fs::write(out.join("charmap-3-after.png"), &shot1)?;
    log.check("charmap-wheel-scrolls", r.is_ok() && shot0 != shot1, format!("{:?} {:?} (화면 바뀜: {})", r.err(), ws.take_notes(), shot0 != shot1));
    let z = ws.zoom([grid.rect[0], grid.rect[1], grid.rect[0] + 100.0, grid.rect[1] + 60.0]).await?;
    std::fs::write(out.join("charmap-4-zoom.png"), &z)?;
    log.check("charmap-zoom", png_ok(&z).0, png_ok(&z).1);

    // h. app_control press 로 고급 보기를 다시 끈다 (UI Automation 경로)
    let tree5 = ws.app_control(&json!({"action": "ui_tree"})).await?;
    if let Some(cb) = find(&parse_tree(&tree5), "CheckBox", &[]) {
        let r = ws.app_control(&json!({"action": "press", "id": cb.id})).await;
        log.check("charmap-press-checkbox", r.as_ref().map(|s| s.contains("1 → 0")).unwrap_or(false), format!("{r:?}"));
    }
    Ok(())
}

async fn read_value(ws: &NativeAppWorkspace, el: &El) -> Result<String> {
    // ui_tree id 는 마지막 ui_tree 기준이므로 다시 찾는다
    let tree = ws.app_control(&json!({"action": "ui_tree"})).await?;
    let els = parse_tree(&tree);
    let e = els
        .iter()
        .find(|e| e.ty == el.ty && (e.rect[0] - el.rect[0]).abs() < 3.0 && (e.rect[1] - el.rect[1]).abs() < 3.0)
        .ok_or_else(|| anyhow!("요소를 다시 찾지 못했습니다"))?;
    let r = ws.app_control(&json!({"action": "read", "id": e.id})).await?;
    Ok(r.split_once("value:\n").map(|(_, v)| v.to_string()).unwrap_or(r))
}

// ───────────────────────── 3. 시험 대상 창 ─────────────────────────

fn current_title(ws: &NativeAppWorkspace) -> String {
    let (v, c) = ws.views();
    v.iter().find(|x| Some(&x.id) == c.as_ref()).map(|x| x.title.clone()).unwrap_or_default()
}

async fn target(log: &mut TestLog, out: &Path) -> Result<()> {
    let exe = std::env::current_exe()?;
    let t = Instant::now();
    let options = NativeOptions { extra_args: vec!["--selftest-native".into(), "--target".into()], ..opts(&exe) };
    let ws = if options.host == crate::native::NativeHost::CurrentSession {
        let (first, second) = tokio::join!(
            NativeAppWorkspace::launch(options.clone(), noop_sink()),
            NativeAppWorkspace::launch(options, noop_sink()),
        );
        match (first, second) {
            (Ok(ws), Err(error)) | (Err(error), Ok(ws)) => {
                log.check("current-session-concurrent-launch", error.to_string().contains("current_session"), error.to_string());
                ws
            }
            (Ok(ws), Ok(other)) => {
                log.check("current-session-concurrent-launch", false, "같은 원본 프로필에 두 인스턴스가 실행됨");
                other.shutdown().await;
                ws
            }
            (Err(error), Err(_)) => return Err(error),
        }
    } else {
        NativeAppWorkspace::launch(options, noop_sink()).await?
    };
    step("Mode A: 시험 대상 실행됨");
    let pid = ws.pid() as u32;
    log.check("target-launch", ws.mode() == NativeMode::Launched && current_title(&ws) == super::testapp::TITLE, format!("pid {pid} '{}' {}ms", current_title(&ws), t.elapsed().as_millis()));
    let exp0 = ws.exposure_report();
    log.check("target-launch-exposure", exp0.is_empty(), format!("{exp0:?}"));
    let duplicate = NativeAppWorkspace::launch(
        NativeOptions {
            host: crate::native::NativeHost::CurrentSession,
            extra_args: vec!["--selftest-native".into(), "--target".into()],
            ..opts(&exe)
        },
        noop_sink(),
    ).await;
    let refusal = duplicate.as_ref().err().map(|e| e.to_string()).unwrap_or_default();
    if let Ok(other) = duplicate {
        other.shutdown().await;
    }
    log.check("current-session-refuses-running-app", refusal.contains("current_session") && crate::platform::proc::pid_alive(pid), refusal);
    let r = target_steps(log, out, &ws).await;
    // 저장하지 않은 변경 → 닫을 때 "저장할까요?" 대화상자가 떠도 2초 뒤 강제 종료
    let _ = ws.screenshot().await;
    step("Mode A: 종료 시작 (저장 확인 대화상자)");
    let ts = Instant::now();
    ws.shutdown().await;
    step("Mode A: 종료 끝");
    tokio::time::sleep(Duration::from_millis(300)).await;
    log.check(
        "target-shutdown-with-save-prompt",
        !crate::platform::proc::pid_alive(pid) && ts.elapsed() < Duration::from_secs(7),
        format!("{}ms", ts.elapsed().as_millis()),
    );
    let exp = ws.exposure_report();
    println!("  시험 대상 노출 기록(대화상자 포함): {exp:?}");
    r
}

async fn target_steps(log: &mut TestLog, out: &Path, ws: &NativeAppWorkspace) -> Result<()> {
    let png = ws.screenshot().await?;
    std::fs::write(out.join("target-1.png"), &png)?;
    log.check("target-screenshot", png_ok(&png).0, png_ok(&png).1);
    let tree = ws.app_control(&json!({"action": "ui_tree"})).await?;
    println!("{tree}");
    let els = parse_tree(&tree);
    let (Some(edit), Some(btn)) = (els.iter().find(|e| e.ty == "Edit" || e.ty == "Document").cloned(), find(&els, "Button", &["확인"]).cloned()) else {
        bail!("시험 대상의 입력 칸·단추를 찾지 못했습니다");
    };

    // Mode A Job: 앱은 시스템 클립보드를 읽거나 쓸 수 없고(UI 제한), 오류 대화상자를 띄우지 않는다 (물려받은 오류 모드).
    // 시험 대상이 자기 프로세스에서 잰 값을 창에 적어 둔다 — 사용자 클립보드를 실제로 건드려 보지 않고 확인한다.
    let env_node = els.iter().find(|e| e.name.starts_with("환경 job-ui=")).ok_or_else(|| anyhow!("환경 요소가 없음"))?;
    let env = ws.app_control(&json!({"action":"read", "id":env_node.id})).await?;
    let account = pw::current_user_sid()?;
    let profile = std::env::var("USERPROFILE")?;
    log.check("target-original-account-profile", env.contains(&format!("account={account} profile={profile}")), "원래 Windows 계정과 사용자 프로필을 사용");
    log.check("target-persisted-state-reused", env.contains("persisted=synthetic-session-state"), "앱이 원래 경로의 합성 세션 데이터를 읽음 (실제 자격 증명은 읽지 않음)");
    let field = |k: &str| env.split_whitespace().find_map(|t| t.strip_prefix(k)).and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok());
    let want_ui = super::launch::JOB_UI_LIMITS;
    log.check(
        "target-job-blocks-clipboard",
        field("job-ui=").map(|u| u & want_ui == want_ui).unwrap_or(false),
        format!("'{env}' (기대 UI 제한 {want_ui:#x}: 클립보드 읽기·쓰기, 시스템 설정, 화면 설정, 종료, 데스크톱)"),
    );
    let quiet = super::launch::QUIET_ERROR_MODE;
    log.check(
        "target-quiet-error-mode",
        field("errmode=").map(|m| m & quiet == quiet).unwrap_or(false) && super::launch::process_error_mode() & quiet == quiet,
        format!("'{env}', GhostDesk {:#x} (기대 {quiet:#x} 포함)", super::launch::process_error_mode()),
    );

    // 픽셀 클릭 → 단추 (BN_CLICKED)
    ws.click(Some(btn.center()), MouseButton::Left, 1, 0).await?;
    let tree = ws.app_control(&json!({"action": "ui_tree"})).await?;
    log.check("target-pixel-click-button", tree.contains("클릭 1"), format!("{:?}", ws.take_notes()));

    // 입력 (여러 줄)
    ws.click(Some(edit.center()), MouseButton::Left, 1, 0).await?;
    ws.type_text("가나다 abc\n둘째 줄").await?;
    let v = read_value(ws, &edit).await?;
    log.check("target-type-multiline", v.contains("가나다 abc") && v.contains("둘째 줄"), format!("{v:?} {:?}", ws.take_notes()));

    // 일반 입력 칸(끌어서 놓기 없음)에서는 끌기로 글자를 선택할 수 있다
    let from = (edit.rect[0] + 3.0, edit.rect[1] + 6.0);
    let to = (edit.rect[0] + edit.rect[2] * 0.6, edit.rect[1] + 6.0);
    let r = ws.drag(from, to, 0).await;
    let tree = ws.app_control(&json!({"action": "ui_tree"})).await?;
    let id = parse_tree(&tree).into_iter().find(|e| e.ty == "Edit" || e.ty == "Document").map(|e| e.id).unwrap_or(0);
    let raw = ws.app_control(&json!({"action": "read", "id": id})).await.unwrap_or_default();
    let sel = raw.lines().find(|l| l.starts_with("selected text:")).unwrap_or("").to_string();
    log.check("target-drag-select-in-edit", r.is_ok() && sel.contains("가나다"), format!("{r:?} {sel:?}"));

    // Ctrl+S → 메뉴 '파일 > 저장' (가속기 글자 흉내), cmd+s 도 같은 뜻
    ws.key("ctrl+s", 1).await?;
    let n1 = ws.take_notes();
    ws.key("cmd+s", 1).await?;
    let n2 = ws.take_notes();
    let title = current_title(ws);
    log.check("target-ctrl-s-menu-accelerator", title == "저장됨 2", format!("제목 '{title}' {n1:?} {n2:?}"));
    let r = ws.key("ctrl+d", 1).await;
    log.check("target-disabled-accelerator", r.as_ref().err().map(|e| e.to_string().contains("비활성")).unwrap_or(false), format!("{:?}", r.err().map(|e| e.to_string())));
    let r = ws.key("ctrl+q", 1).await;
    log.check("target-unknown-shortcut-errors", r.is_err(), format!("{:?}", r.err().map(|e| e.to_string())));

    // 편집 칸에서 Ctrl+C 는 앱의 '편집 > 복사'(시스템 클립보드)가 아니라 내부 클립보드로
    ws.key("ctrl+a", 1).await?;
    ws.key("ctrl+c", 1).await?;
    let notes = ws.take_notes();
    log.check("target-ctrl-c-internal", notes.iter().any(|n| n.contains("내부 클립보드")) && !current_title(ws).contains("복사 실행됨"), format!("{notes:?}"));

    // menus / menu
    let m = ws.app_control(&json!({"action": "menus"})).await?;
    println!("{m}");
    log.check(
        "target-menus",
        m.contains("파일") && m.contains("저장(S) (Ctrl+S)") && m.contains("도움말") && m.contains("차단") && m.contains("[비활성]"),
        m.replace('\n', " | "),
    );
    let r = ws.app_control(&json!({"action": "menu", "path": "도움말 > 정보"})).await;
    log.check("target-menu-help-blocked", r.is_err() && !current_title(ws).contains("정보 열림"), format!("{:?}", r.err().map(|e| e.to_string())));
    let r = ws.app_control(&json!({"action": "menu", "path": "편집 > 복사"})).await;
    log.check("target-menu-copy-blocked", r.as_ref().err().map(|e| e.to_string().contains("클립보드")).unwrap_or(false), format!("{:?}", r.err().map(|e| e.to_string())));
    let r = ws.app_control(&json!({"action": "menu", "path": "파일 > 새로 만들기"})).await;
    let v = read_value(ws, &edit).await?;
    log.check("target-menu-new", r.is_ok() && v.is_empty() && current_title(ws) == "새 문서", format!("{r:?} 입력 칸 {v:?}"));

    // 체크 상자 press, 콤보 상자 choose (펼치지 않고, CBN_SELCHANGE 알림)
    let _ = ws.screenshot().await?;
    let tree = ws.app_control(&json!({"action": "ui_tree"})).await?;
    let els = parse_tree(&tree);
    if let Some(cb) = find(&els, "CheckBox", &[]) {
        let r = ws.app_control(&json!({"action": "press", "id": cb.id})).await;
        log.check("target-press-checkbox", r.as_ref().map(|s| s.contains("0 → 1")).unwrap_or(false), format!("{r:?}"));
    }
    if let Some(combo) = find(&els, "ComboBox", &[]) {
        let r = ws.app_control(&json!({"action": "choose", "id": combo.id, "option": "바나나"})).await;
        log.check("target-choose-combo", current_title(ws) == "선택: 바나나", format!("{r:?} 제목 '{}'", current_title(ws)));
        let r = ws.click(Some(combo.center()), MouseButton::Left, 1, 0).await;
        log.check("target-refuse-combo-dropdown", r.as_ref().err().map(|e| e.to_string().contains("choose")).unwrap_or(false), format!("{:?}", r.err().map(|e| e.to_string())));
        combo_keys(log, ws, &btn).await?;
    }

    step("Mode A: Ctrl+M (메시지 상자)");
    // Ctrl+M → 메시지 상자 (새 창으로 자동 전환, 노출 측정) → 확인 누르기
    ws.key("ctrl+m", 1).await?;
    step("Mode A: Ctrl+M 보냄");
    tokio::time::sleep(Duration::from_millis(300)).await;
    // 컴퓨터가 바쁘면 메시지 상자가 늦게 뜬다 — 창 목록을 다시 보며(새 창 자동 전환) 최대 3초 기다린다
    let t = Instant::now();
    while !ws.views().0.iter().any(|v| v.detail.starts_with("#32770")) && t.elapsed() < Duration::from_secs(3) {
        let _ = ws.app_control(&json!({"action": "list_windows"})).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let notes = ws.take_notes();
    let (views, cur) = ws.views();
    let dlg = views.iter().find(|v| v.detail.starts_with("#32770")).cloned();
    log.check(
        "target-messagebox-autoswitch",
        dlg.as_ref().map(|d| Some(&d.id) == cur.as_ref()).unwrap_or(false),
        format!("{:?} / {notes:?}", views.iter().map(|v| format!("{} [{}]", v.title, v.detail)).collect::<Vec<_>>()),
    );
    if dlg.is_some() {
        // 모달 대화상자가 열린 동안: 본 창(비활성)의 메뉴·단축키는 실행하지 않는다 (실제 키보드와 같게)
        let saved3 = |ws: &NativeAppWorkspace| ws.views().0.iter().any(|v| v.title == "저장됨 3");
        let r = ws.key("ctrl+s", 1).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        log.check(
            "target-modal-refuses-shortcut",
            r.as_ref().err().map(|e| e.to_string().contains("모달")).unwrap_or(false) && !saved3(ws),
            format!("{:?}", r.err().map(|e| e.to_string())),
        );
        let r = ws.app_control(&json!({"action": "menu", "path": "파일 > 저장"})).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        log.check(
            "target-modal-refuses-menu",
            r.as_ref().err().map(|e| e.to_string().contains("모달")).unwrap_or(false) && !saved3(ws),
            format!("{:?}", r.err().map(|e| e.to_string())),
        );
        let m = ws.app_control(&json!({"action": "menus"})).await;
        log.check(
            "target-modal-menus-read-only",
            m.as_ref().map(|s| s.contains("실행할 수 없") && s.contains("저장")).unwrap_or(false),
            m.map(|s| s.replace('\n', " | ")).unwrap_or_else(|e| e.to_string()),
        );
        modal_element_actions(log, ws).await?;
        let png = ws.screenshot().await?;
        std::fs::write(out.join("target-2-messagebox.png"), &png)?;
        let tree = ws.app_control(&json!({"action": "ui_tree"})).await?;
        println!("{tree}");
        let els = parse_tree(&tree);
        if let Some(ok) = find(&els, "Button", &["확인", "ok"]) {
            let r = ws.app_control(&json!({"action": "press", "id": ok.id})).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let (views2, _) = ws.views();
            log.check("target-messagebox-press-ok", r.is_ok() && !views2.iter().any(|v| v.detail.starts_with("#32770")), format!("{r:?}"));
        }
    }
    step("Mode A: 메시지 상자 확인 끝");
    modal_alt_mnemonic(log, ws).await?;
    step("Mode A: 질문 상자 끝");
    // 저장하지 않은 변경을 만든다 (종료 시험용)
    let _ = ws.screenshot().await?;
    ws.click(Some(edit.center()), MouseButton::Left, 1, 0).await?;
    ws.type_text("저장하지 않은 변경").await?;
    Ok(())
}

/// 모달 대화상자가 열린 동안 보기를 본 창(비활성)으로 바꿔도 요소 동작(press·choose)은 보내지 않는다 —
/// 보내면 앱의 명령 처리가 대화상자의 모달 루프 안에서 다시 실행된다. 끝나면 대화상자로 돌아간다.
async fn modal_element_actions(log: &mut TestLog, ws: &NativeAppWorkspace) -> Result<()> {
    let (views, _) = ws.views();
    let (Some(mi), Some(di)) = (
        views.iter().position(|v| v.detail.starts_with("GhostDeskWinAppTestTarget")),
        views.iter().position(|v| v.detail.starts_with("#32770")),
    ) else {
        log.check("target-modal-refuses-element-actions", false, "본 창·대화상자를 창 목록에서 찾지 못함");
        return Ok(());
    };
    let main_id = views[mi].id.clone();
    ws.switch_view(mi).await?;
    let on_main = ws.views().1.as_deref() == Some(main_id.as_str());
    let main_title = |ws: &NativeAppWorkspace| ws.views().0.iter().find(|v| v.id == main_id).map(|v| v.title.clone()).unwrap_or_default();
    let title_before = main_title(ws);
    let tree = ws.app_control(&json!({"action": "ui_tree"})).await?;
    let els = parse_tree(&tree);
    let clicks = |els: &[El]| els.iter().find(|e| e.name.starts_with("클릭 ")).map(|e| e.name.clone());
    let clicks_before = clicks(&els);
    let rp = match find(&els, "Button", &["확인"]) {
        Some(b) => ws.app_control(&json!({"action": "press", "id": b.id})).await,
        None => Err(anyhow!("본 창의 확인 단추를 찾지 못함")),
    };
    let rc = match find(&els, "ComboBox", &[]) {
        Some(c) => ws.app_control(&json!({"action": "choose", "id": c.id, "option": "사과"})).await,
        None => Err(anyhow!("본 창의 콤보 상자를 찾지 못함")),
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = ws.app_control(&json!({"action": "list_windows"})).await;
    let clicks_after = clicks(&parse_tree(&ws.app_control(&json!({"action": "ui_tree"})).await?));
    let title_after = main_title(ws);
    let refused = |r: &Result<String>| r.as_ref().err().map(|e| e.to_string().contains("비활성")).unwrap_or(false);
    log.check(
        "target-modal-refuses-element-actions",
        on_main && refused(&rp) && refused(&rc) && clicks_before.is_some() && clicks_before == clicks_after && title_before == title_after,
        format!(
            "본 창으로 전환 {on_main}, press {:?}, choose {:?}, 라벨 {clicks_before:?} → {clicks_after:?}, 제목 '{title_before}' → '{title_after}'",
            rp.map_err(|e| e.to_string()),
            rc.map_err(|e| e.to_string())
        ),
    );
    // 대화상자로 돌아간다 (창 목록 순서가 바뀌었을 수 있어 다시 찾는다)
    let di = ws.views().0.iter().position(|v| v.detail.starts_with("#32770")).unwrap_or(di);
    ws.switch_view(di).await?;
    Ok(())
}

/// 질문 상자(예(&Y)/아니요(&N))가 열린 동안 Alt+Y: 본 창 메뉴에 같은 Alt+Y 단축키가 있어도
/// 실제 키보드처럼 대화상자의 '예' 니모닉 단추가 눌려야 한다 (본 창 메뉴는 실행되지 않음).
async fn modal_alt_mnemonic(log: &mut TestLog, ws: &NativeAppWorkspace) -> Result<()> {
    let _ = ws.screenshot().await?;
    ws.key("ctrl+k", 1).await?;
    let t = Instant::now();
    let dialog_current = |ws: &NativeAppWorkspace| {
        let (v, c) = ws.views();
        v.iter().any(|x| x.detail.starts_with("#32770") && Some(&x.id) == c.as_ref())
    };
    while !dialog_current(ws) && t.elapsed() < Duration::from_secs(3) {
        let _ = ws.app_control(&json!({"action": "list_windows"})).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !dialog_current(ws) {
        log.check("target-modal-alt-mnemonic", false, format!("질문 상자로 전환되지 않음 {:?}", ws.take_notes()));
        return Ok(());
    }
    let _ = ws.take_notes();
    let r = ws.key("alt+y", 1).await;
    let notes = ws.take_notes();
    let pressed = r.is_ok() && notes.iter().any(|n| n.contains("단추"));
    let answered = |ws: &NativeAppWorkspace| ws.views().0.iter().any(|v| v.title == "질문: 예");
    let t = Instant::now();
    while !answered(ws) && t.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = ws.app_control(&json!({"action": "list_windows"})).await;
    }
    let (views, _) = ws.views();
    let menu_ran = views.iter().any(|v| v.title.contains("Alt+Y 메뉴"));
    let dialog_left = views.iter().any(|v| v.detail.starts_with("#32770"));
    log.check(
        "target-modal-alt-mnemonic",
        pressed && answered(ws) && !menu_ran && !dialog_left,
        format!("{:?} {notes:?} / 창 {:?}", r.map_err(|e| e.to_string()), views.iter().map(|v| format!("{} [{}]", v.title, v.detail)).collect::<Vec<_>>()),
    );
    if dialog_left {
        // 실패했으면 아니요로 닫아 둔다 (다음 시험이 본 창을 쓰도록)
        let tree = ws.app_control(&json!({"action": "ui_tree"})).await.unwrap_or_default();
        if let Some(no) = find(&parse_tree(&tree), "Button", &["아니요", "no"]) {
            let _ = ws.app_control(&json!({"action": "press", "id": no.id})).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
    Ok(())
}

/// 키보드로 콤보 상자에 포커스를 옮긴 뒤(확인 단추 클릭 → Tab → Tab): F4 와 확장 UI 의 아래 화살표는 목록을 사용자 화면에 열므로 거부,
/// 확장 UI 가 아닌 콤보 상자의 아래 화살표는 목록을 열지 않고 다음 항목을 고른다 (허용).
async fn combo_keys(log: &mut TestLog, ws: &NativeAppWorkspace, btn: &El) -> Result<()> {
    let pid = ws.pid() as u32;
    let _ = pid;
    let list_open = || ws.app_top_windows().iter().any(|w| w.class == "ComboLBox" && w.visible);
    let main = ws.app_top_windows().into_iter().find(|w| w.visible && w.class == "GhostDeskWinAppTestTarget").map(|w| w.hwnd).unwrap_or(0);
    let Some(combo) = w32::child_windows(main).into_iter().find(|c| w32::class_name(*c) == "ComboBox") else {
        log.check("target-combo-keys", false, "콤보 상자 창을 찾지 못함");
        return Ok(());
    };
    // 숨은 데스크톱의 포커스는 시험 대상 스레드에서 읽는다 (다른 데스크톱에서는 조회할 수 없음).
    let thread_focus = || w32::send_timeout(main, super::testapp::QUERY_FOCUS, 0, 0, 1000).unwrap_or(0) as w32::Hwnd;
    let _ = ws.screenshot().await?;
    ws.click(Some(btn.center()), MouseButton::Left, 1, 0).await?;
    ws.key("Tab", 1).await?;
    ws.key("Tab", 1).await?;
    let focused = thread_focus() == combo;
    let r = ws.key("F4", 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    log.check(
        "target-refuse-f4-combo",
        focused && r.as_ref().err().map(|e| e.to_string().contains("choose")).unwrap_or(false) && !list_open(),
        format!("콤보 상자 포커스 {focused}, {:?}", r.err().map(|e| e.to_string())),
    );
    // 확장 UI (아래 화살표로 목록이 열림) → 거부
    w32::send_timeout(combo, w32::CB_SETEXTENDEDUI, 1, 0, 1000);
    let ext = w32::combo_extended_ui(combo);
    let r = ws.key("Down", 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let refused = ext && r.as_ref().err().map(|e| e.to_string().contains("확장 UI")).unwrap_or(false) && !list_open();
    w32::send_timeout(combo, w32::CB_SETEXTENDEDUI, 0, 0, 1000);
    log.check("target-refuse-down-extended-ui-combo", refused, format!("확장 UI {ext}, {:?}", r.err().map(|e| e.to_string())));
    // 보통 콤보 상자: 아래 화살표 = 다음 항목 (목록은 열리지 않음). 제목은 창 목록을 다시 볼 때 바뀌므로 최대 3초 기다린다.
    let r = ws.key("Down", 1).await;
    let t = Instant::now();
    let mut title = current_title(ws);
    while title != "선택: 포도" && t.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = ws.app_control(&json!({"action": "list_windows"})).await;
        title = current_title(ws);
    }
    log.check(
        "target-down-on-plain-combo",
        r.is_ok() && thread_focus() == combo && title == "선택: 포도" && !list_open(),
        format!("{r:?} 제목 '{title}'"),
    );
    Ok(())
}

// ───────────────────────── (선택) 전화 걸기: 시작하자마자 대화상자를 띄우는 실제 앱 ─────────────────────────

/// `--with-dialer`: 전화 걸기(dialer.exe)는 시작하자마자 "연결에 사용할 모뎀" 대화상자와 경고 메시지 상자를 띄운다.
/// 실행 중 노출(가장 나쁜 경우)을 재고, 메시지 상자 자동 전환·확인 누르기·실제 앱의 메뉴 막대 읽기를 본다.
async fn dialer(log: &mut TestLog, out: &Path) -> Result<()> {
    let exe = sys32("dialer.exe");
    if !exe.is_file() {
        log.warn("dialer", "dialer.exe 가 없어 건너뜀");
        return Ok(());
    }
    step("dialer: 실행");
    let t = Instant::now();
    let ws = NativeAppWorkspace::launch(opts(&exe), noop_sink()).await?;
    step("dialer: 실행됨");
    let exp = ws.exposure_report();
    log.check("dialer-launch-exposure", exp.is_empty(), format!("{}ms, 기록 {exp:?}", t.elapsed().as_millis()));
    let (views, _) = ws.views();
    println!("  dialer 창: {:?}", views.iter().map(|v| format!("{} [{}]", v.title, v.detail)).collect::<Vec<_>>());
    let r = async {
        std::fs::write(out.join("dialer-1.png"), ws.screenshot().await?)?;
        // 열린 대화상자를 확인으로 닫아 가며 주 창까지 간다 (최대 3번)
        for _ in 0..3 {
            let tree = ws.app_control(&json!({"action": "ui_tree"})).await?;
            let els = parse_tree(&tree);
            let Some(ok) = find(&els, "Button", &["확인", "ok"]).or_else(|| find(&els, "Button", &["취소", "cancel"])).cloned() else { break };
            println!("{tree}");
            let r = ws.app_control(&json!({"action": "press", "id": ok.id})).await;
            println!("  press {:?} → {r:?} {:?}", ok.name, ws.take_notes());
            tokio::time::sleep(Duration::from_millis(400)).await;
            let _ = ws.screenshot().await;
        }
        std::fs::write(out.join("dialer-2.png"), ws.screenshot().await?)?;
        let m = ws.app_control(&json!({"action": "menus"})).await?;
        println!("{m}");
        log.check("dialer-menus", m.contains("[차단") || m.contains(':'), m.replace('\n', " | "));
        anyhow::Ok(())
    }
    .await;
    step("dialer: 종료");
    ws.shutdown().await;
    let exp = ws.exposure_report();
    log.check("dialer-total-exposure", exp.is_empty(), format!("{exp:?}"));
    r
}

// ───────────────────────── 4. Mode B ─────────────────────────

async fn attach(log: &mut TestLog, out: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let exe = std::env::current_exe()?;
    // 동의 없이 붙기 → 거부 (사용자 앱이 이미 실행 중)
    let mut child = std::process::Command::new(&exe)
        .args(["--selftest-native", "--target", "--offscreen-visible", "--minimized"])
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let pid = child.id();
    let r = attach_steps(log, out, &exe, pid).await;
    let _ = child.kill();
    let _ = child.wait();
    r
}

async fn attach_steps(log: &mut TestLog, out: &Path, exe: &Path, pid: u32) -> Result<()> {
    let t = Instant::now();
    let main = loop {
        if let Some(w) = pw::windows_of(&[pid]).into_iter().find(|w| w.visible && w.title == super::testapp::TITLE) {
            break w;
        }
        if t.elapsed() > Duration::from_secs(10) {
            bail!("Mode B 시험 대상 창이 뜨지 않았습니다");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    // 사용자의 앱이 최소화되어 있던 상황: 붙으면 화면 밖에서 원래 크기로 되돌려 조작하고, 끝나면 다시 최소화되어야 한다
    let before = (main.minimized, main.visible, w32::get_placement(main.hwnd).map(|p| p.normal));
    // 시험 대상은 '최대화 상태에서 최소화된 창'처럼 '복원하면 최대화' 플래그를 갖고 시작한다
    let restore_max = |h: w32::Hwnd| w32::get_placement(h).map(|p| p.flags & w32::WPF_RESTORETOMAXIMIZED != 0).unwrap_or(false);
    let before_max = restore_max(main.hwnd);
    step("Mode B: 시험 대상 창 확인");
    let r = NativeAppWorkspace::launch(NativeOptions { new_instance: false, ..opts(exe) }, noop_sink()).await;
    log.check("attach-requires-consent", r.as_ref().err().map(|e| e.to_string().contains("동의")).unwrap_or(false), format!("{:?}", r.err().map(|e| e.to_string())));

    let ws = NativeAppWorkspace::launch(
        NativeOptions { attach_consent: true, attach_pid: Some(pid as i32), new_instance: false, ..opts(exe) },
        noop_sink(),
    )
    .await?;
    step("Mode B: 붙음");
    log.check("attach-mode-b", ws.mode() == NativeMode::Attached && ws.pid() as u32 == pid, format!("pid {}", ws.pid()));
    let png = ws.screenshot().await?;
    std::fs::write(out.join("attach-1.png"), &png)?;
    log.check("attach-screenshot", png_ok(&png).0, png_ok(&png).1);
    let tree = ws.app_control(&json!({"action": "ui_tree"})).await?;
    let els = parse_tree(&tree);
    if let Some(edit) = els.iter().find(|e| e.ty == "Edit" || e.ty == "Document").cloned() {
        step("Mode B: 클릭·입력 시작");
        ws.click(Some(edit.center()), MouseButton::Left, 1, 0).await?;
        ws.type_text("붙기 시험 ok").await?;
        step("Mode B: 클릭·입력 끝");
        let v = read_value(&ws, &edit).await?;
        log.check("attach-type", v.contains("붙기 시험 ok"), format!("{v:?}"));
    }
    step("Mode B: 종료(되돌리기) 시작");
    ws.shutdown().await;
    step("Mode B: 종료 끝");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = pw::windows_of(&[pid]).into_iter().find(|w| w.title == super::testapp::TITLE).map(|w| (w.minimized, w.visible, w32::get_placement(w.hwnd).map(|p| p.normal)));
    log.check("attach-not-killed", crate::platform::proc::pid_alive(pid), format!("pid {pid}"));
    log.check("attach-restored", before.0 && after == Some(before), format!("(최소화, 보임, 보통 위치) 전 {before:?} → 후 {after:?}"));
    if before_max {
        log.check("attach-restore-to-maximized-kept", restore_max(main.hwnd), format!("WPF_RESTORETOMAXIMIZED 전 {before_max} → 후 {}", restore_max(main.hwnd)));
    } else {
        log.warn("attach-restore-to-maximized-kept", "시험 대상에 '복원하면 최대화' 플래그를 걸지 못해 건너뜀");
    }
    let rec = out.join("winapp-parked.json");
    log.check("attach-recovery-record-cleared", !rec.exists(), format!("{}", rec.display()));

    // 비정상 종료 복구: 남은 기록의 '원래 배치'로 되돌린다 (화면 밖 위치끼리 옮기므로 사용자 화면에는 나타나지 않음)
    if let Some(w) = pw::windows_of(&[pid]).into_iter().find(|w| w.title == super::testapp::TITLE) {
        if let Some(mut pl) = w32::get_placement(w.hwnd) {
            pl.normal = (pl.normal.0 + 100, pl.normal.1 + 100, pl.normal.2 + 100, pl.normal.3 + 100);
            pl.show_cmd = w32::SW_SHOWNORMAL;
            super::recovery::remember(pid, w.hwnd, &pl, true, None);
            let had = rec.exists();
            // 살아 있는 작업공간의 창은 복구하지 않는다
            let n_live = crate::native::recover_parked_windows();
            log.check("live-parked-window-not-recovered", n_live == 0 && rec.exists(), format!("되돌림 {n_live}개"));
            super::recovery::simulate_crashed_record(w.hwnd);
            step("복구 시작");
            let n = crate::native::recover_parked_windows();
            step("복구 끝");
            tokio::time::sleep(Duration::from_millis(400)).await;
            let now = w32::get_placement(w.hwnd).map(|p| p.normal);
            log.check(
                "crash-recovery-restores-placement",
                // 시험 대상은 지금 최소화돼 있다: 복구는 되살리지 않고(포커스를 빼앗지 않게) 복원 위치만 고친다
                had && n == 1 && now.map(|r| (r.0 - pl.normal.0).abs() <= 2 && (r.1 - pl.normal.1).abs() <= 2 && (r.2 - pl.normal.2).abs() <= 2 && (r.3 - pl.normal.3).abs() <= 2).unwrap_or(false) && w32::is_iconic(w.hwnd) && !rec.exists(),
                format!("기록 {had}, 되돌림 {n}개, 보통 위치 {now:?} (기대 {:?} ±2px — DPI 를 모르는 창은 논리↔실제 픽셀 변환에서 1px 반올림이 생김), 최소화 유지 {}", pl.normal, w32::is_iconic(w.hwnd)),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_parsing() {
        let t = "창 'x' (스크린샷 100x100 픽셀 좌표):\n1 Button \"선택(S)\" [10,20,30,40]\n  2 Edit \"복사할 문자(A):\" value=\"abc\" [1,2,3,4] focused\n3 CheckBox \"고급 보기(V)\" [5,6,7,8] unchecked";
        let e = parse_tree(t);
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].ty, "Button");
        assert_eq!(e[0].name, "선택(S)");
        assert_eq!(e[0].center(), (25.0, 40.0));
        assert_eq!(e[1].value, "abc");
        assert!(e[1].flags.contains("focused"));
        assert!(find(&e, "Button", &["선택"]).is_some());
        assert!(find(&e, "Button", &["복사"]).is_none());
    }
}
