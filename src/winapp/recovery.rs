//! Mode B recovery log: if GhostDesk crashes while a user's app window is parked off-screen, the
//! window stays there.
//!
//! Each move writes the original placement to a file, and a normal restore removes it. On the
//! next run, any logged window that's still off-screen is put back ([`recover`]).
//!
//! Location: `<data root>\winapp-parked.json` (override with `GHOSTDESK_WINAPP_RECOVERY_DIR`
//! for tests).

use std::path::PathBuf;

use parking_lot::Mutex;
use serde_json::{json, Value};

use super::w32::{self, Hwnd, Placement};
use crate::platform::win as pw;

static LOCK: Mutex<()> = Mutex::new(());

/// Windows this process currently has parked off-screen (live mode B workspaces).
///
/// `recover` leaves these alone, so recovery running at GUI startup or when a second workspace
/// starts never pulls an in-progress workspace's windows back onto the user's screen. Empty in a
/// fresh process after a crash.
static LIVE: Mutex<Vec<i64>> = Mutex::new(Vec::new());

fn path() -> PathBuf {
    let dir = std::env::var_os("GHOSTDESK_WINAPP_RECOVERY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(crate::platform::paths::data_root);
    dir.join("winapp-parked.json")
}

fn load() -> Vec<Value> {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(crate::platform::fsx::strip_bom(&s).0).ok())
        .unwrap_or_default()
}

fn save(v: &[Value]) {
    let p = path();
    if v.is_empty() {
        let _ = std::fs::remove_file(&p);
        return;
    }
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(&Value::from(v.to_vec())).unwrap_or_default()).is_ok() {
        let _ = crate::platform::fsx::rename_replace(&tmp, &p);
    }
}

/// One record: the original state before the move.
#[derive(Clone, Copy, Debug)]
pub struct Record {
    pub pid: u32,
    pub hwnd: Hwnd,
    pub placement: Placement,
    pub was_visible: bool,
    /// Actual screen rectangle of a snapped window (Aero Snap, snap layouts), only when it differs
    /// from the placement's normal position.
    pub arranged: Option<(i32, i32, i32, i32)>,
}

/// Encodes as JSON. "flags" and "rect" were added later and are optional when reading (old
/// records stay compatible).
pub fn encode(pid: u32, h: Hwnd, pl: &Placement, was_visible: bool, arranged: Option<(i32, i32, i32, i32)>) -> Value {
    let mut v = json!({
        "pid": pid,
        "hwnd": h as i64,
        "show_cmd": pl.show_cmd,
        "normal": [pl.normal.0, pl.normal.1, pl.normal.2, pl.normal.3],
        "min": [pl.min_pos.0, pl.min_pos.1],
        "max": [pl.max_pos.0, pl.max_pos.1],
        "flags": pl.flags,
        "was_visible": was_visible,
    });
    if let Some(r) = arranged {
        v["rect"] = json!([r.0, r.1, r.2, r.3]);
    }
    v
}

pub fn decode(v: &Value) -> Option<Record> {
    let a = |k: &str, i: usize| v[k].get(i).and_then(|x| x.as_i64()).map(|x| x as i32);
    let rect = || Some((a("rect", 0)?, a("rect", 1)?, a("rect", 2)?, a("rect", 3)?));
    Some(Record {
        pid: v["pid"].as_u64()? as u32,
        hwnd: v["hwnd"].as_i64()? as Hwnd,
        placement: Placement {
            show_cmd: v["show_cmd"].as_u64()? as u32,
            normal: (a("normal", 0)?, a("normal", 1)?, a("normal", 2)?, a("normal", 3)?),
            min_pos: (a("min", 0)?, a("min", 1)?),
            max_pos: (a("max", 0)?, a("max", 1)?),
            flags: v["flags"].as_u64().unwrap_or(0) as u32,
        },
        was_visible: v["was_visible"].as_bool().unwrap_or(true),
        arranged: rect(),
    })
}

/// Restores the saved original state without activating (async). Returns true if the placement
/// was restored.
///
/// A normal window that was visible goes back to its placement (normal position), then a
/// snapped one is resized to its actual rectangle.
pub fn apply_original(h: Hwnd, mut pl: Placement, was_visible: bool, arranged: Option<(i32, i32, i32, i32)>) -> bool {
    let normal_shown = pl.show_cmd == w32::SW_SHOWNORMAL;
    pl.show_cmd = match pl.show_cmd {
        w32::SW_SHOWNORMAL => w32::SW_SHOWNOACTIVATE as u32,
        w32::SW_SHOWMINIMIZED => w32::SW_SHOWMINNOACTIVE,
        c => c,
    };
    if !was_visible {
        // windows hidden when first seen (the app may have shown them since) only get their position
        // back; visibility stays as it is now
        pl.show_cmd = w32::keep_visibility_cmd(h);
    }
    let ok = w32::set_placement_async(h, &pl);
    if ok && normal_shown && was_visible {
        if let Some(r) = arranged {
            w32::set_rect_async(h, r);
        }
    }
    ok
}

/// Leaves a minimized window minimized and only fixes its restore position (normal position)
/// from the original placement. Originally maximized windows restore maximized.
fn fix_minimized_restore_position(h: Hwnd, mut pl: Placement) -> bool {
    if pl.show_cmd == w32::SW_SHOWMAXIMIZED {
        pl.flags |= w32::WPF_RESTORETOMAXIMIZED;
    }
    pl.show_cmd = w32::SW_SHOWMINNOACTIVE;
    w32::set_placement_async(h, &pl)
}

/// Records one moved user window.
pub fn remember(pid: u32, h: Hwnd, pl: &Placement, was_visible: bool, arranged: Option<(i32, i32, i32, i32)>) {
    let _g = LOCK.lock();
    {
        let mut live = LIVE.lock();
        if !live.contains(&(h as i64)) {
            live.push(h as i64);
        }
    }
    let mut v = load();
    v.retain(|e| e["hwnd"].as_i64() != Some(h as i64));
    v.push(encode(pid, h, pl, was_visible, arranged));
    save(&v);
}

/// For the self-test: treats this window's record as left over by a crashed process (only
/// removes it from the live list).
pub fn simulate_crashed_record(h: Hwnd) {
    LIVE.lock().retain(|x| *x != h as i64);
}

/// Removes the records of restored windows.
pub fn forget(hwnds: &[Hwnd]) {
    let _g = LOCK.lock();
    LIVE.lock().retain(|x| !hwnds.iter().any(|h| *x == *h as i64));
    let mut v = load();
    let n = v.len();
    v.retain(|e| !hwnds.iter().any(|h| e["hwnd"].as_i64() == Some(*h as i64)));
    if v.len() != n {
        save(&v);
    }
}

/// Puts user windows left off-screen by a crash back where they were, returning how many.
///
/// Only restores a window that still exists, belongs to the same process and isn't on any
/// monitor right now.
pub fn recover() -> usize {
    let _g = LOCK.lock();
    let all = load();
    // windows parked by this process's live workspaces aren't recovered (their records stay)
    let live = LIVE.lock().clone();
    let (keep, v): (Vec<Value>, Vec<Value>) =
        all.into_iter().partition(|e| e["hwnd"].as_i64().is_some_and(|h| live.contains(&h)));
    if v.is_empty() {
        return 0;
    }
    let mut n = 0;
    let locked = w32::lock_foreground(true);
    for e in &v {
        let Some(r) = decode(e) else { continue };
        let h = r.hwnd;
        if !w32::is_window(h) || w32::thread_pid(h).1 != r.pid {
            continue;
        }
        if w32::is_iconic(h) {
            // don't restore windows that are minimized now: they're reachable from the taskbar, and
            // restoring them while the user is busy elsewhere can activate them (without the foreground
            // lock) and steal focus. Only fix a restore position left off-screen, so they reappear in
            // place when the user restores them.
            let restore_off_screen = w32::get_placement(h)
                .is_some_and(|c| !pw::intersects_any_monitor(w32::normal_to_screen(h, c.normal)));
            if restore_off_screen && fix_minimized_restore_position(h, r.placement) {
                n += 1;
            }
            continue;
        }
        if pw::intersects_any_monitor(w32::window_rect(h)) {
            continue;
        }
        if apply_original(h, r.placement, r.was_visible, r.arranged) {
            n += 1;
        }
    }
    if locked {
        std::thread::sleep(std::time::Duration::from_millis(150));
        w32::lock_foreground(false);
    }
    save(&keep);
    if n > 0 {
        eprintln!("[ghostdesk] 지난번에 화면 밖에 남은 사용자 앱 창 {n}개를 원래 자리로 되돌렸습니다");
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let pl = Placement { show_cmd: 2, normal: (1, 2, 300, 400), min_pos: (-1, -1), max_pos: (-8, -8), flags: w32::WPF_RESTORETOMAXIMIZED };
        let v = encode(42, 0x1234, &pl, true, None);
        let r = decode(&v).unwrap();
        assert_eq!((r.pid, r.hwnd, r.was_visible, r.arranged), (42, 0x1234, true, None));
        let p2 = r.placement;
        assert_eq!((p2.show_cmd, p2.normal, p2.min_pos, p2.max_pos, p2.flags), (2, (1, 2, 300, 400), (-1, -1), (-8, -8), 2));
        assert!(decode(&json!({"pid": 1})).is_none());
        // a snapped window's actual rectangle
        let v = encode(42, 0x1234, &Placement { show_cmd: 1, flags: 0, ..pl }, true, Some((-7, 0, 967, 1047)));
        assert_eq!(decode(&v).unwrap().arranged, Some((-7, 0, 967, 1047)));
    }

    #[test]
    fn old_records_still_decode() {
        // a record from an older version without flags and rect
        let v = json!({"pid": 7, "hwnd": 99, "show_cmd": 1, "normal": [1, 2, 3, 4], "min": [-1, -1], "max": [-1, -1], "was_visible": true});
        let r = decode(&v).unwrap();
        assert_eq!((r.pid, r.hwnd, r.placement.flags, r.arranged), (7, 99, 0, None));
        assert_eq!(r.placement.normal, (1, 2, 3, 4));
    }
}
