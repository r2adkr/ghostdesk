//! Mode B 복구 기록: 사용자의 앱 창을 화면 밖으로 옮겨 둔 채 GhostDesk 가 비정상 종료되면 그 창이 화면 밖에 남는다.
//! 옮길 때마다 원래 배치를 파일에 적어 두고, 정상적으로 되돌리면 지운다. 다음 실행 때 남아 있는 기록의 창이 아직
//! 화면 밖에 있으면 원래 자리로 되돌린다 ([`recover`]).
//!
//! 위치: `<데이터 루트>\winapp-parked.json` (시험할 때는 GHOSTDESK_WINAPP_RECOVERY_DIR 로 바꿀 수 있다).

use std::path::PathBuf;

use parking_lot::Mutex;
use serde_json::{json, Value};

use super::w32::{self, Hwnd, Placement};
use crate::platform::win as pw;

static LOCK: Mutex<()> = Mutex::new(());

/// 이 프로세스에서 지금 화면 밖에 옮겨 둔(살아 있는 Mode B 작업공간의) 창들.
/// `recover` 는 이 창들을 건드리지 않는다: 두 번째 작업공간을 실행할 때나 GUI 시작 때 복구가 돌아도
/// 진행 중인 작업공간의 창을 사용자 화면으로 되돌리지 않게. (비정상 종료 뒤 새 프로세스에서는 비어 있다)
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

/// 기록 하나 (옮기기 전의 원래 상태)
#[derive(Clone, Copy, Debug)]
pub struct Record {
    pub pid: u32,
    pub hwnd: Hwnd,
    pub placement: Placement,
    pub was_visible: bool,
    /// 스냅(Aero Snap·스냅 레이아웃)된 창의 실제 화면 사각형 — 배치의 보통 위치와 다를 때만
    pub arranged: Option<(i32, i32, i32, i32)>,
}

/// JSON 으로. "flags"·"rect" 는 나중에 더한 항목이라 읽을 때는 없어도 된다 (이전 기록과 호환).
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

/// 저장해 둔 원래 상태로 되돌린다 (활성화하지 않음, 비동기). 배치를 되돌렸으면 true.
/// 보이던 보통 창은 배치(보통 위치)로 되돌린 뒤, 스냅된 창이었으면 실제 사각형으로 다시 맞춘다.
pub fn apply_original(h: Hwnd, mut pl: Placement, was_visible: bool, arranged: Option<(i32, i32, i32, i32)>) -> bool {
    let normal_shown = pl.show_cmd == w32::SW_SHOWNORMAL;
    pl.show_cmd = match pl.show_cmd {
        w32::SW_SHOWNORMAL => w32::SW_SHOWNOACTIVATE as u32,
        w32::SW_SHOWMINIMIZED => w32::SW_SHOWMINNOACTIVE,
        c => c,
    };
    if !was_visible {
        // 처음 봤을 때 숨어 있던 창(그 뒤 앱이 보여 줬을 수 있음)은 자리만 되돌리고 보임 상태는 지금 그대로 둔다
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

/// 최소화된 창은 최소화된 채로 두고 복원 위치(보통 위치)만 원래 배치로 고친다. 원래 최대화였으면 '복원하면 최대화'.
fn fix_minimized_restore_position(h: Hwnd, mut pl: Placement) -> bool {
    if pl.show_cmd == w32::SW_SHOWMAXIMIZED {
        pl.flags |= w32::WPF_RESTORETOMAXIMIZED;
    }
    pl.show_cmd = w32::SW_SHOWMINNOACTIVE;
    w32::set_placement_async(h, &pl)
}

/// 옮긴 사용자 창 하나를 기록한다
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

/// 셀프테스트용: 이 창의 기록을 '비정상 종료한 이전 프로세스가 남긴 것' 처럼 다룬다 (살아 있는 목록에서만 뺀다)
pub fn simulate_crashed_record(h: Hwnd) {
    LIVE.lock().retain(|x| *x != h as i64);
}

/// 되돌린 창들의 기록을 지운다
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

/// 지난번에 되돌리지 못한(비정상 종료) 사용자 창을 원래 자리로. 되돌린 창 수.
/// 창이 아직 있고, 같은 프로세스 것이고, 지금 어느 모니터에도 걸치지 않을 때만 되돌린다.
pub fn recover() -> usize {
    let _g = LOCK.lock();
    let all = load();
    // 이 프로세스의 살아 있는 작업공간이 옮겨 둔 창은 복구 대상이 아니다 (기록도 그대로 둔다)
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
            // 지금 최소화된 창은 되살리지 않는다: 작업 표시줄에서 닿을 수 있고, 되살리면 사용자가 다른 작업 중일 때
            // (포그라운드 잠금을 못 얻어) 활성화되어 포커스를 빼앗을 수 있다. 복원 위치가 화면 밖에 남아 있을 때만
            // 그것을 원래대로 고쳐, 사용자가 복원하면 제자리에 나타나게 한다.
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
        // 스냅된 창의 실제 사각형
        let v = encode(42, 0x1234, &Placement { show_cmd: 1, flags: 0, ..pl }, true, Some((-7, 0, 967, 1047)));
        assert_eq!(decode(&v).unwrap().arranged, Some((-7, 0, 967, 1047)));
    }

    #[test]
    fn old_records_still_decode() {
        // flags·rect 가 없던 이전 버전의 기록
        let v = json!({"pid": 7, "hwnd": 99, "show_cmd": 1, "normal": [1, 2, 3, 4], "min": [-1, -1], "max": [-1, -1], "was_visible": true});
        let r = decode(&v).unwrap();
        assert_eq!((r.pid, r.hwnd, r.placement.flags, r.arranged), (7, 99, 0, None));
        assert_eq!(r.placement.normal, (1, 2, 3, 4));
    }
}
