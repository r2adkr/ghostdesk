//! `ghostdesk --mcp` — Claude 앱, Claude Code, Codex 같은 외부 에이전트용 MCP 서버 (stdio).
//!
//! 외부 에이전트는 API 키 없이 GhostDesk 의 격리 화면(브라우저 / Mac·Windows 앱 / 원격 데스크톱)을
//! 도구로 쓴다. 이 프로세스는 GhostDesk 창의 제어 연결(macOS/Linux: 유닉스 소켓, Windows: 명명 파이프)로
//! 요청을 전달하는 얇은 다리이며, 창이 꺼져 있으면 자동으로 띄운다. 그래서 외부 에이전트가 하는 일을
//! 사용자가 GhostDesk 창에서 가상 마우스로 지켜볼 수 있다.
//!
//! 주의: stdout 에는 JSON-RPC 메시지만 쓴다. 로그는 stderr 로.
//! (시작할 때 진짜 stdout 을 따로 떼어 내고 fd 1(Windows: 표준 출력 핸들)을 stderr 로 돌려서,
//! 실수로 println! 해도 섞이지 않는다.)
//!
//! 구조:
//! - stdin 에서 한 줄씩 읽고, tools/call 은 요청마다 task 로 돌린다. ping·tools/list 는 긴 호출이
//!   진행 중이어도 곧바로 답한다. 응답은 stdout 전용 writer 하나로만 나간다.
//! - GhostDesk 창과는 연결 하나만 유지하고(창은 연결이 끊기면 그 연결이 만든 화면을 닫는다),
//!   요청 id 로 응답을 짝지어 여러 요청을 동시에 보낸다.
//! - 연결이 끊기면 다음 호출 때 다시 연결한다(창이 꺼져 있으면 다시 띄움). 보내기 전에 끊긴 요청은
//!   새 연결로 다시 보내지만, 보낸 뒤 끊긴 요청은 이미 실행되었을 수 있어 다시 보내지 않는다.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex as PMutex;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
};

use crate::{
    control,
    platform::{
        caps,
        ipc::{self, ConnectError},
        text,
    },
};

/// 에이전트에게 알려 줄 이 OS 의 화면 종류·낱말 (도구 설명과 안내문을 만든다)
struct Plat {
    /// "Mac 앱" / "Windows 앱"
    local_apps: &'static str,
    /// 단축키 예시의 기본 수정키 ("cmd" / "ctrl")
    primary_mod: &'static str,
    /// app 인자 설명
    app_hint: &'static str,
    /// kind=app 을 쓸 수 있는가 (Electron 숨김 복제 또는 일반 앱)
    app_kind: bool,
    /// attach_running (실행 중인 사용자 앱 조작)
    attach: bool,
    /// app_host (Windows 앱의 숨은 데스크톱·화면 밖 창 선택)
    app_host: bool,
    /// kind=session (별도 계정 세션)
    session: bool,
    /// kind=session 이 Windows 자식 세션(같은 계정)인가 — false 면 macOS 전용 계정 세션(account 인자)
    child_session: bool,
    /// virtual_display (가상 모니터)
    vdisplay: bool,
    /// desk_open 의 dedicated_machine 설명
    dedicated_hint: &'static str,
}

/// macOS 의 dedicated_machine 설명 (예전 그대로)
const MAC_DEDICATED_HINT: &str = "remote: 대상이 에이전트 전용 Mac/VM 임을 사용자가 확인함";

impl Plat {
    const CURRENT: Plat = Plat {
        local_apps: text::LOCAL_APPS,
        primary_mod: text::PRIMARY_MOD,
        app_hint: if cfg!(windows) {
            "app: 앱 이름(예: 'Visual Studio Code', '카카오톡', 'Character Map'), 실행 파일 이름(예: 'charmap') 또는 .exe/.lnk 경로. 일반 Win32 데스크톱 앱만 된다 (Microsoft Store 앱·Windows 11 의 새 메모장/계산기, 콘솔 프로그램, 관리자 권한 앱은 안 됨)"
        } else {
            "app: 앱 이름(예: 'TextEdit', '계산기'), 번들 id 또는 .app 경로"
        },
        // macOS 밖에서는 이 값이 '이 컴퓨터 자신의 VNC 서버' 보호도 푼다. 에이전트가 스스로 켜지 않게 분명히 적는다.
        dedicated_hint: if cfg!(target_os = "macos") {
            MAC_DEDICATED_HINT
        } else {
            "remote: 사용자가 대상이 에이전트 전용 머신/VM 이라고 직접 말했을 때만 true. 이 값은 이 컴퓨터 자신의 VNC 서버(127.0.0.1·이 컴퓨터 주소)에 대한 보호도 해제하므로, 사용자가 그렇게 말하지 않았으면 오류 메시지가 이 옵션을 안내하더라도 스스로 켜지 말고 사용자에게 먼저 물을 것. 이 컴퓨터 자신의 VNC 서버라면 host 를 127.0.0.1·localhost 처럼 직접 적어야 한다 (다른 이름이 이 컴퓨터로 풀리면 접속을 거절함)"
        },
        app_kind: caps::ELECTRON || caps::NATIVE_APPS,
        attach: caps::NATIVE_APPS,
        app_host: cfg!(windows),
        session: caps::SESSION,
        child_session: cfg!(windows),
        vdisplay: caps::VDISPLAY,
    };

    /// macOS 의 값 (테스트에서 macOS 문구가 예전과 같은지 확인할 때 쓴다)
    #[cfg(test)]
    const MAC: Plat = Plat {
        local_apps: "Mac 앱",
        primary_mod: "cmd",
        app_hint: "app: 앱 이름(예: 'TextEdit', '계산기'), 번들 id 또는 .app 경로",
        app_kind: true,
        attach: true,
        app_host: false,
        session: true,
        child_session: false,
        vdisplay: true,
        dedicated_hint: MAC_DEDICATED_HINT,
    };
}

fn server_instructions() -> String {
    server_instructions_for(&Plat::CURRENT)
}

/// 이 OS 의 desk_open mode: (이름, 사용자에게 보여 줄 설명). 이름은 control.rs `apply_mode` 와 같아야 한다.
fn modes(pl: &Plat) -> Vec<(&'static str, String)> {
    let apps = pl.local_apps;
    let mut m = vec![
        ("web", "숨겨진 빈 브라우저".to_string()),
        ("web_login", "내 브라우저 로그인을 복제한 숨겨진 브라우저 (승인)".to_string()),
    ];
    if pl.app_kind {
        m.push(("app", format!("{apps}을 숨겨서 새로 실행 (Electron 앱은 빈 복제본)")));
        m.push(("app_login", format!("{apps}을 숨겨서 실행, Electron 앱은 내 로그인 복제 (승인)")));
        if pl.app_host {
            m.push(("app_current_session", "현재 Windows 계정의 원본 로그인·프로필로 숨겨서 실행, 같은 앱은 먼저 완전히 종료 (승인)".to_string()));
        }
        if pl.attach {
            m.push(("app_running", "지금 실행 중인 내 앱을 숨기고 조작 (승인)".to_string()));
        }
        if pl.vdisplay {
            m.push(("app_vdisplay", "가상 모니터 위에서 실행, 실제 픽셀 클릭·드래그 (실험, 승인)".to_string()));
        }
    }
    if pl.session {
        m.push((
            "session",
            if pl.child_session {
                "같은 PC·같은 계정의 Windows 자식 세션 화면 전체 (승인, 사용자가 기능을 켜야 함)".to_string()
            } else {
                "전용 macOS 계정의 별도 로그인 세션 화면 전체 (account)".to_string()
            },
        ));
    }
    m.push(("remote", "VNC 원격 컴퓨터 (host/port, 승인)".to_string()));
    m
}

fn server_instructions_for(pl: &Plat) -> String {
    let apps = pl.local_apps;
    let app_tools = if pl.app_kind { format!(", {apps}은 desk_app 으로 메뉴·접근성 요소를") } else { String::new() };
    // 모드는 사용자가 고른다: 처음 한 번 묻고, 정해지면 묻지 않고 끝까지 진행한다 (모드 목록은 desk_open 의 mode 설명에 한 번만 둔다)
    format!(
        "사용자가 'GhostDesk 로', '고스트데스크에서', '내 화면 건드리지 말고' 같은 말로 작업을 맡기면, desk_open 전에 desk_open 의 mode 목록에서 작업에 맞는 것을 추천으로 맨 앞에 두고 어떤 모드로 할지 한 번 물어보세요. 사용자가 이미 모드를 말했거나 이 대화에서 고른 적이 있으면 묻지 마세요. 모드가 정해지면 desk_open(mode) → desk_action/desk_browser/desk_app 반복 → 결과 보고 → desk_close 로 끝까지 진행하세요. 모드가 실패하면 다른 모드로 몰래 바꾸지 말고 이유를 알린 뒤 다시 물어보세요. 화면은 사용자의 화면·마우스·키보드와 분리되어 있고, 동작마다 스크린샷이 돌아옵니다. 좌표는 최신 스크린샷의 픽셀입니다. 브라우저는 desk_browser 로 URL·탭을{app_tools} 다룹니다. 사용자가 GhostDesk 창에서 실시간으로 지켜봅니다. 메시지 전송·결제·삭제처럼 되돌릴 수 없는 동작은 먼저 확인받으세요."
    )
}

fn tools() -> Value {
    tools_for(&Plat::CURRENT)
}

fn tools_for(pl: &Plat) -> Value {
    let apps = pl.local_apps;
    let modes = modes(pl);
    let names: Vec<&str> = modes.iter().map(|(n, _)| *n).collect();
    let mode_desc = modes.iter().map(|(n, d)| format!("{n}={d}")).collect::<Vec<_>>().join("; ");
    let app_desc = format!(
        "앱 화면의 보조 조작. Electron 앱: action=list|click (source=app|context, path='File > New Text File') 로 네이티브 메뉴 사용. {apps}: action=list_windows | switch_window{{index}} | ui_tree{{max_depth}} | press{{id}} | set_value{{id,text}} | focus{{id}} | select{{id}} | choose{{id,option}} | menus | menu{{path:'파일 > 신규'}} | read{{id}}. ui_tree 의 id 는 다음 ui_tree 전까지 유효하다."
    );
    let mut t = json!([
        {
            "name": "desk_open",
            "description": "격리된 화면을 연다 (사용자가 고른 mode). 결과로 desk_id 와 첫 스크린샷을 돌려준다.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "enum": names, "description": mode_desc },
                    "app": { "type": "string", "description": pl.app_hint },
                    "url": { "type": "string", "description": "web: 처음 열 URL" },
                    "account": { "type": "string", "description": "session: 전용 에이전트 macOS 계정 이름 (기본 ghostdesk-agent)" },
                    "browser": { "type": "string", "description": "web: Chrome / Edge / Whale 등 (기본: 처음 찾은 것)" },
                    "name": { "type": "string", "description": "화면 이름" },
                    "host": { "type": "string" }, "port": { "type": "integer" },
                    "username": { "type": "string" }, "password": { "type": "string" },
                    "dedicated_machine": { "type": "boolean", "description": pl.dedicated_hint },
                    "width": { "type": "integer" }, "height": { "type": "integer" }
                },
                "required": ["mode"]
            }
        },
        {
            "name": "desk_action",
            "description": format!("화면에서 마우스/키보드 동작을 한다 (Claude computer-use 와 같은 동작 이름). action: screenshot, left_click, right_click, middle_click, double_click, triple_click, left_click_drag(start_coordinate→coordinate), mouse_move, left_mouse_down, left_mouse_up, scroll(scroll_direction up/down/left/right, scroll_amount), type(text, 한글 가능), key(text: 'Return', '{}+a', 'ctrl+shift+t'; repeat), hold_key(text, duration), wait(duration), zoom(region [x0,y0,x1,y1]), cursor_position. 동작 뒤 스크린샷을 돌려준다.", pl.primary_mod),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "desk_id": { "type": "string" },
                    "action": { "type": "string" },
                    "coordinate": { "type": "array", "items": { "type": "number" }, "minItems": 2, "maxItems": 2 },
                    "start_coordinate": { "type": "array", "items": { "type": "number" }, "minItems": 2, "maxItems": 2 },
                    "text": { "type": "string" },
                    "scroll_direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                    "scroll_amount": { "type": "integer" },
                    "duration": { "type": "number" },
                    "repeat": { "type": "integer" },
                    "region": { "type": "array", "items": { "type": "number" }, "minItems": 4, "maxItems": 4 },
                    "screenshot": { "type": "boolean", "description": "동작 뒤 스크린샷을 받을지 (기본 true)" }
                },
                "required": ["desk_id", "action"]
            }
        },
        {
            "name": "desk_browser",
            "description": "브라우저 화면의 주소 이동과 탭 관리 (화면에 주소창이 없으므로 이것을 쓴다). action: navigate(url), back, forward, reload, new_tab(url), switch_tab(index), close_tab(index), list_tabs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "desk_id": { "type": "string" },
                    "action": { "type": "string", "enum": ["navigate", "back", "forward", "reload", "new_tab", "switch_tab", "close_tab", "list_tabs"] },
                    "url": { "type": "string" },
                    "index": { "type": "integer" }
                },
                "required": ["desk_id", "action"]
            }
        },
        {
            "name": "desk_app",
            "description": app_desc,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "desk_id": { "type": "string" },
                    "action": { "type": "string" },
                    "id": { "type": "integer" }, "index": { "type": "integer" },
                    "text": { "type": "string" }, "option": { "type": "string" },
                    "path": { "type": "string" }, "source": { "type": "string", "enum": ["app", "context"] },
                    "max_depth": { "type": "integer" }
                },
                "required": ["desk_id", "action"]
            }
        },
        {
            "name": "desk_list",
            "description": "열려 있는 GhostDesk 화면 목록 (desk_id, 종류, 크기, 탭/창).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "desk_close",
            "description": "화면을 닫고 그 프로그램을 종료한다 (작업공간 데이터는 남는다).",
            "inputSchema": { "type": "object", "properties": { "desk_id": { "type": "string" } }, "required": ["desk_id"] }
        }
    ]);
    // 이 OS 에서 쓸 수 없는 인자·도구는 알리지 않는다 (에이전트가 고른 뒤 승인 단계에서 실패하지 않게)
    if let Some(props) = t[0]["inputSchema"]["properties"].as_object_mut() {
        let drop: [(&str, bool); 2] = [
            ("account", pl.session && !pl.child_session), // Windows 자식 세션은 같은 계정
            ("app", pl.app_kind),
        ];
        for (key, keep) in drop {
            if !keep {
                props.shift_remove(key);
            }
        }
    }
    if !pl.app_kind {
        if let Some(list) = t.as_array_mut() {
            list.retain(|tool| tool["name"] != "desk_app");
        }
    }
    t
}

/// 제어 연결 호출 실패의 종류. 재시도해도 되는지가 여기서 갈린다.
#[derive(Debug)]
enum CallError {
    /// 요청을 소켓에 쓰기 전에 연결이 이미 끊겨 있었다 → GhostDesk 쪽에서 실행되지 않았으므로 다시 보내도 안전
    NotSent(String),
    /// 요청을 보낸 뒤 응답을 받기 전에 연결이 끊겼다 → 이미 실행되었을 수 있다
    Lost(String),
    /// 제어 서버가 돌려준 오류 (연결은 멀쩡함)
    Remote(String),
}

type Reply = std::result::Result<Value, String>;

#[derive(Default)]
struct Pending {
    /// Some 이면 연결이 끊긴 이유 (더 이상 이 연결로 보내지 않는다)
    dead: Option<String>,
    map: HashMap<u64, oneshot::Sender<Reply>>,
}

/// GhostDesk 창의 제어 소켓 클라이언트 (연결 하나를 여러 요청이 id 로 나눠 쓴다).
///
/// 제어 서버는 연결이 끊기면 그 연결이 만든 화면을 닫으므로, 요청마다 새 연결을 열지 않고
/// 연결 하나를 유지한다. 응답은 리더 task 가 id 로 찾아 해당 요청에 넘기고(순서 무관),
/// 쓰기는 전용 writer task 가 한 줄씩 통째로 쓴다(호출이 중간에 취소되어도 줄이 섞이지 않게).
struct Bridge {
    pending: Arc<PMutex<Pending>>,
    writes: mpsc::UnboundedSender<(String, oneshot::Sender<std::io::Result<()>>)>,
    next: AtomicU64,
    tasks: [tokio::task::AbortHandle; 2],
}

impl Drop for Bridge {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// 요청이 끝나기 전에 호출이 취소되면(notifications/cancelled) 대기 목록에서 지운다
struct PendingGuard<'a> {
    pending: &'a PMutex<Pending>,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.pending.lock().map.remove(&self.id);
    }
}

impl Bridge {
    /// 연결 하나(읽기·쓰기 반쪽)로 브리지를 만든다. 소켓·명명 파이프 모두 `ipc` 가 상자에 담아 준다.
    fn new((rd, mut wr): (ipc::Reader, ipc::Writer)) -> Arc<Bridge> {
        let pending = Arc::new(PMutex::new(Pending::default()));

        let p = pending.clone();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(rd).lines();
            let why = loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let v: Value = match serde_json::from_str(&line) {
                            Ok(v) => v,
                            Err(e) => break format!("GhostDesk 창의 응답을 해석하지 못했습니다: {e}"),
                        };
                        let Some(id) = v["id"].as_u64() else {
                            eprintln!("[ghostdesk-mcp] id 없는 응답 무시: {}", crate::llm::short(&line, 300));
                            continue;
                        };
                        let tx = p.lock().map.remove(&id);
                        let reply = match v.get("error").filter(|e| !e.is_null()) {
                            Some(e) => Err(e.as_str().map(String::from).unwrap_or_else(|| e.to_string())),
                            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        match tx {
                            Some(tx) => {
                                let _ = tx.send(reply);
                            }
                            None => eprintln!("[ghostdesk-mcp] 기다리는 요청이 없는 응답 무시 (id {id})"),
                        }
                    }
                    Ok(None) => break "GhostDesk 창이 연결을 닫았습니다".to_string(),
                    Err(e) => break e.to_string(),
                }
            };
            // 끊긴 표시와 대기 목록 비우기를 한 번에: 깨어난 요청은 이미 dead 가 설정된 상태를 본다
            let mut g = p.lock();
            g.dead.get_or_insert(why);
            g.map.clear();
        });

        let (wtx, mut wrx) = mpsc::unbounded_channel::<(String, oneshot::Sender<std::io::Result<()>>)>();
        let p = pending.clone();
        let writer = tokio::spawn(async move {
            while let Some((line, ack)) = wrx.recv().await {
                let r = wr.write_all(line.as_bytes()).await;
                let failed = r.as_ref().err().map(|e| e.to_string());
                let _ = ack.send(r);
                if let Some(e) = failed {
                    // 큐에 남은 요청은 ack 가 버려져 '보내지 않음' 으로 처리된다
                    let mut g = p.lock();
                    g.dead.get_or_insert(e);
                    g.map.clear();
                    break;
                }
            }
        });

        Arc::new(Bridge { pending, writes: wtx, next: AtomicU64::new(1), tasks: [reader.abort_handle(), writer.abort_handle()] })
    }

    fn is_dead(&self) -> bool {
        self.pending.lock().dead.is_some()
    }

    fn mark_dead(&self, why: String) {
        let mut g = self.pending.lock();
        g.dead.get_or_insert(why);
        g.map.clear();
    }

    async fn call(&self, method: &str, params: Value) -> std::result::Result<Value, CallError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let rx = {
            let mut g = self.pending.lock();
            if let Some(why) = &g.dead {
                return Err(CallError::NotSent(why.clone()));
            }
            let (tx, rx) = oneshot::channel();
            g.map.insert(id, tx);
            rx
        };
        let _guard = PendingGuard { pending: &self.pending, id };
        let line = format!("{}\n", json!({ "id": id, "method": method, "params": params }));
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.writes.send((line, ack_tx)).is_err() {
            let why = "GhostDesk 창과의 연결이 끊겼습니다".to_string();
            self.mark_dead(why.clone());
            return Err(CallError::NotSent(why));
        }
        match ack_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(CallError::NotSent(e.to_string())),
            Err(_) => return Err(CallError::NotSent("GhostDesk 창과의 연결이 끊겼습니다".into())),
        }
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(CallError::Remote(e)),
            Err(_) => {
                let why = self.pending.lock().dead.clone().unwrap_or_else(|| "GhostDesk 창과의 연결이 끊겼습니다".into());
                Err(CallError::Lost(why))
            }
        }
    }
}

/// GUI 를 띄우는 함수 (테스트에서는 가짜 서버를 띄운다)
type Launcher = Arc<dyn Fn() -> Result<()> + Send + Sync>;

/// 제어 연결 관리자: 끊기면 다음 호출 때 다시 연결하고, 창이 꺼져 있으면 띄운다.
struct Conn {
    ep: ipc::Endpoint,
    launcher: Option<Launcher>,
    state: tokio::sync::Mutex<ConnState>,
}

#[derive(Default)]
struct ConnState {
    bridge: Option<Arc<Bridge>>,
    launched_at: Option<Instant>,
}

/// 응답을 못 받았어도 다시 보내도 되는 요청 (부작용 없음)
fn idempotent(method: &str) -> bool {
    matches!(method, "hello" | "list")
}

fn lost_message(method: &str, why: &str) -> String {
    match method {
        "open" => format!(
            "GhostDesk 창과의 연결이 desk_open 처리 중에 끊겼습니다 ({why}). 화면이 이미 열렸을 수 있으니, 다시 열기 전에 desk_list 로 열린 화면을 확인하세요."
        ),
        "act" => format!(
            "GhostDesk 창과의 연결이 동작 처리 중에 끊겼습니다 ({why}). 동작이 이미 실행되었을 수 있으니, 같은 동작을 반복하기 전에 desk_list 와 desk_action(action=screenshot) 으로 상태를 확인하세요."
        ),
        _ => format!("GhostDesk 창과의 연결이 요청 처리 중에 끊겼습니다 ({why}). 요청이 처리되었는지 desk_list 로 확인하세요."),
    }
}

/// 연결 실패를 사용자·에이전트에게 보여 줄 문구 (Unix 는 예전처럼 OS 오류 그대로)
fn connect_error_text(e: &ConnectError) -> String {
    match e {
        ConnectError::NotRunning(e) | ConnectError::Other(e) => e.to_string(),
        ConnectError::Denied(m) => m.clone(),
    }
}

impl Conn {
    fn new(ep: ipc::Endpoint, launcher: Option<Launcher>) -> Conn {
        Conn { ep, launcher, state: tokio::sync::Mutex::new(ConnState::default()) }
    }

    /// 살아 있는 연결을 돌려준다. 없거나 끊겼으면 (필요하면 GUI 를 띄워서) 새로 연결한다.
    /// 상태 잠금을 쥔 채 연결하므로 동시에 들어온 호출들이 창을 여러 번 띄우지 않는다.
    async fn bridge(&self) -> Result<Arc<Bridge>> {
        let mut st = self.state.lock().await;
        if let Some(b) = st.bridge.as_ref().filter(|b| !b.is_dead()) {
            return Ok(b.clone());
        }
        if st.bridge.take().is_some() {
            eprintln!("[ghostdesk-mcp] GhostDesk 창과의 연결이 끊겨 다시 연결합니다");
        }
        let stream = match ipc::connect(&self.ep).await {
            Ok(s) => s,
            // 연결 지점은 있지만 쓸 수 없다 (다른 사용자·관리자 권한의 창, 이름을 가로챈 프로그램):
            // 창을 또 띄워도 소용없으므로 바로 알린다
            Err(ConnectError::Denied(m)) => bail!("GhostDesk 창에 연결하지 못했습니다: {m}"),
            Err(first) => {
                let Some(launch) = &self.launcher else {
                    bail!("GhostDesk 창에 연결하지 못했습니다 ({})", connect_error_text(&first));
                };
                // 방금 띄웠으면(아직 기동 중) 또 띄우지 않고 기다리기만 한다
                if st.launched_at.map_or(true, |t| t.elapsed() > Duration::from_secs(30)) {
                    launch()?;
                    st.launched_at = Some(Instant::now());
                }
                let mut last = first;
                let mut found = None;
                for _ in 0..80 {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    match ipc::connect(&self.ep).await {
                        Ok(s) => {
                            found = Some(s);
                            break;
                        }
                        Err(ConnectError::Denied(m)) => bail!("GhostDesk 창에 연결하지 못했습니다: {m}"),
                        Err(e) => last = e,
                    }
                }
                found.ok_or_else(|| {
                    anyhow!(
                        "GhostDesk 창에 연결하지 못했습니다 ({}). 이전 버전의 GhostDesk 창이 이미 켜져 있으면 그 창을 종료한 뒤 다시 시도하세요. \
                         창의 '외부 연결' 화면에서 연결 서버 상태를 확인할 수 있습니다.",
                        connect_error_text(&last)
                    )
                })?
            }
        };
        let b = Bridge::new(stream);
        let hello = match b.call("hello", json!({})).await {
            Ok(v) => v,
            Err(CallError::Remote(e)) | Err(CallError::NotSent(e)) | Err(CallError::Lost(e)) => {
                bail!("GhostDesk 창과 연결을 확인하지 못했습니다: {e}")
            }
        };
        eprintln!("[ghostdesk-mcp] GhostDesk 창에 연결됨: {hello}");
        st.bridge = Some(b.clone());
        Ok(b)
    }

    /// 제어 서버 요청. 연결이 끊겨 있었으면 다시 연결해 한 번 더 보낸다.
    /// 단, 보낸 뒤에 끊긴 요청은 이미 실행되었을 수 있으므로 부작용이 없는 요청만 다시 보낸다.
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let mut retried = false;
        loop {
            let b = self.bridge().await?;
            match b.call(method, params.clone()).await {
                Ok(v) => return Ok(v),
                Err(CallError::Remote(e)) => bail!("{e}"),
                Err(CallError::NotSent(why)) => {
                    if !retried {
                        retried = true;
                        continue;
                    }
                    bail!("GhostDesk 창과의 연결이 끊겼습니다: {why}");
                }
                Err(CallError::Lost(why)) => {
                    if !retried && idempotent(method) {
                        retried = true;
                        continue;
                    }
                    bail!("{}", lost_message(method, &why));
                }
            }
        }
    }
}


/// GUI 를 백그라운드로 띄운다 (.app 번들 안이면 open 으로, 아니면 같은 실행 파일을 인자 없이).
///
/// Windows: 설치본(`install::stable_exe()`)이 있으면 그것을, 없으면 이 실행 파일을 띄운다.
/// MCP 호스트와 분리해서(콘솔 없음·호스트의 Job 밖·사용자의 기본 환경변수) 띄우므로, 호스트가 MCP 서버를
/// 끝내도 창은 남고 호스트의 파이프 핸들도 물려받지 않는다 (`run` 이 표준 핸들 상속을 미리 꺼 둔다).
fn launch_gui() -> Result<()> {
    let exe = gui_exe()?;
    eprintln!("[ghostdesk-mcp] GhostDesk 창이 꺼져 있어 띄웁니다: {}", exe.display());
    let bundle = exe.ancestors().find(|p| p.extension().map(|e| e == "app").unwrap_or(false)).map(|p| p.to_path_buf());
    let mut cmd = match bundle {
        Some(b) if cfg!(target_os = "macos") => {
            let mut c = std::process::Command::new("/usr/bin/open");
            c.arg("-g").arg(b);
            c
        }
        _ => std::process::Command::new(&exe),
    };
    // Windows: macOS 의 `open -g` 처럼 포커스를 빼앗지 않는 창으로 (이미 떠 있으면 그 창을 앞으로 가져오지 않고 끝난다)
    #[cfg(windows)]
    cmd.arg("--background");
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    if let Some(dir) = exe.parent() {
        // MCP 호스트의 작업 폴더(사용자 프로젝트)를 창이 계속 붙잡고 있지 않게
        cmd.current_dir(dir);
    }
    // Unix: 새 프로세스 그룹 (예전과 같음), Windows: DETACHED_PROCESS + Job 탈출 + 사용자 기본 환경
    crate::platform::proc::spawn_detached(&mut cmd).context("GhostDesk 창 실행 실패")?;
    Ok(())
}

/// 띄울 GUI 실행 파일
fn gui_exe() -> Result<PathBuf> {
    let me = std::env::current_exe().context("실행 파일 경로를 알 수 없습니다")?;
    #[cfg(windows)]
    {
        // 연결 설정이 가리키는 설치본이 있으면 그것을 (임시 위치에서 돌던 사본이 창을 띄우지 않게)
        let installed = crate::install::stable_exe();
        if installed.is_file() {
            return Ok(installed);
        }
    }
    Ok(me)
}

fn text(t: impl Into<String>) -> Value {
    json!({ "type": "text", "text": t.into() })
}

fn image(b64: &str) -> Value {
    json!({ "type": "image", "data": b64, "mimeType": "image/png" })
}

/// 제어 서버 응답 → MCP 콘텐츠 블록
fn to_content(r: &Value, head: Option<String>) -> Vec<Value> {
    let mut c = Vec::new();
    let mut lines = Vec::new();
    if let Some(h) = head {
        lines.push(h);
    }
    if let Some(t) = r["text"].as_str() {
        lines.push(t.to_string());
    }
    if let Some(n) = r["notes"].as_array().filter(|n| !n.is_empty()) {
        lines.push(format!("Events: {}", n.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" / ")));
    }
    if let Some(n) = r["notes"].as_str() {
        lines.push(n.to_string());
    }
    if let Some(v) = r["views"].as_str().filter(|v| !v.trim().is_empty()) {
        lines.push(format!("Views (▶ = current):\n{v}"));
    }
    if let Some(s) = r["size"].as_array() {
        lines.push(format!("Screenshot size: {}x{}", s.first().and_then(|x| x.as_u64()).unwrap_or(0), s.get(1).and_then(|x| x.as_u64()).unwrap_or(0)));
    }
    c.push(text(lines.join("\n")));
    if let Some(b) = r["screenshot_png"].as_str() {
        c.push(image(b));
    }
    c
}

async fn call_tool(conn: &Conn, name: &str, args: &Value) -> Result<Vec<Value>> {
    let args = if args.is_object() { args.clone() } else { json!({}) };
    let desk_id = args["desk_id"].clone();
    let res = match name {
        "desk_open" => {
            let mut p = args.clone();
            p["client"] = json!(std::env::var("GHOSTDESK_CLIENT_NAME").unwrap_or_else(|_| "외부 에이전트".into()));
            let r = conn.call("open", p).await?;
            to_content(&r, Some(format!("desk_id: {} ({})", r["desk_id"].as_str().unwrap_or(""), r["kind"].as_str().unwrap_or(""))))
        }
        "desk_action" => {
            let mut input = args.clone();
            if let Some(o) = input.as_object_mut() {
                o.remove("desk_id");
                o.remove("screenshot");
            }
            let r = conn
                .call("act", json!({ "desk_id": desk_id, "tool": "computer", "input": input, "screenshot": args["screenshot"].as_bool().unwrap_or(true) }))
                .await?;
            to_content(&r, None)
        }
        "desk_browser" | "desk_app" => {
            let mut input = args.clone();
            if let Some(o) = input.as_object_mut() {
                o.remove("desk_id");
            }
            let tool = if name == "desk_browser" { "browser" } else { "app" };
            let r = conn.call("act", json!({ "desk_id": desk_id, "tool": tool, "input": input })).await?;
            to_content(&r, None)
        }
        "desk_list" => {
            let r = conn.call("list", json!({})).await?;
            vec![text(serde_json::to_string_pretty(&r["desks"])?)]
        }
        "desk_close" => {
            let r = conn.call("close", json!({ "desk_id": desk_id })).await?;
            vec![text(format!("닫음: {}", r["closed"]))]
        }
        other => bail!("알 수 없는 도구: {other}"),
    };
    Ok(res)
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

/// 진행 중인 tools/call (요청 id 의 JSON 문자열 → (순번, 취소 핸들))
type InFlight = Arc<PMutex<HashMap<String, (u64, tokio::task::AbortHandle)>>>;

/// tools/call 이 아닌 요청은 곧바로 답한다
fn immediate_response(id: Value, method: &str, msg: &Value) -> Value {
    match method {
        "initialize" => {
            let pv = msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18").to_string();
            json!({ "jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": pv,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "ghostdesk", "title": "GhostDesk", "version": env!("CARGO_PKG_VERSION") },
                "instructions": server_instructions()
            }})
        }
        "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools() } }),
        "resources/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "resources": [] } }),
        "prompts/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "prompts": [] } }),
        other => rpc_error(id, -32601, format!("지원하지 않는 메서드: {other}")),
    }
}

/// MCP 서버 루프. 한 줄에 하나의 JSON-RPC 메시지를 읽고, 응답 줄은 `out` 으로 보낸다.
///
/// tools/call 은 각자 task 로 돌려서(승인 대기·wait 처럼 오래 걸리는 호출이 있어도) ping, tools/list,
/// 다른 tools/call 이 막히지 않는다. 응답은 `out` 하나로만 나가므로 줄이 섞이지 않는다.
/// 입력이 끝나면 진행 중인 호출이 끝날 때까지 기다렸다가 돌아온다.
async fn serve<R>(input: R, out: mpsc::UnboundedSender<String>, conn: Arc<Conn>) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let send = |v: Value| {
        let _ = out.send(format!("{v}\n"));
    };
    let mut lines = BufReader::new(input).lines();
    let inflight: InFlight = Arc::new(PMutex::new(HashMap::new()));
    let mut tasks = tokio::task::JoinSet::new();
    let mut seq: u64 = 0;
    loop {
        let line = tokio::select! {
            line = lines.next_line() => match line? {
                Some(l) => l,
                None => break,
            },
            Some(_) = tasks.join_next(), if !tasks.is_empty() => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg = match serde_json::from_str::<Value>(&line) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[ghostdesk-mcp] JSON 파싱 실패: {}", crate::llm::short(&line, 300));
                send(rpc_error(Value::Null, -32700, format!("Parse error: {e}")));
                continue;
            }
        };
        // MCP 2025-06-18 부터 JSON-RPC 배치는 지원하지 않는다 → 배열 전체에 오류 하나로 답한다
        if msg.is_array() {
            send(rpc_error(Value::Null, -32600, "Invalid Request: batch not supported"));
            continue;
        }
        let Some(obj) = msg.as_object() else {
            send(rpc_error(Value::Null, -32600, "Invalid Request"));
            continue;
        };
        let id = obj.get("id").cloned();
        let Some(method) = obj.get("method").and_then(|m| m.as_str()).map(str::to_string) else {
            // 우리가 보낸 요청에 대한 응답(result/error)이면 무시, 그 밖에 id 가 있으면 잘못된 요청
            if let Some(id) = id.filter(|_| !obj.contains_key("result") && !obj.contains_key("error")) {
                send(rpc_error(id, -32600, "Invalid Request: method 가 없습니다"));
            }
            continue;
        };
        let Some(id) = id else {
            // 알림(notification)은 응답하지 않는다. 취소 알림이면 해당 호출을 멈춘다.
            if method == "notifications/cancelled" {
                let key = msg["params"]["requestId"].to_string();
                if let Some((_, h)) = inflight.lock().remove(&key) {
                    eprintln!("[ghostdesk-mcp] 요청 {key} 취소됨");
                    h.abort();
                }
            }
            continue;
        };
        if !(id.is_string() || id.is_number()) {
            send(rpc_error(Value::Null, -32600, "Invalid Request: id 는 문자열이나 숫자여야 합니다"));
            continue;
        }
        if method != "tools/call" {
            send(immediate_response(id, &method, &msg));
            continue;
        }
        let Some(name) = msg["params"]["name"].as_str().map(str::to_string) else {
            send(rpc_error(id, -32602, "Invalid params: params.name 이 필요합니다"));
            continue;
        };
        let args = msg["params"]["arguments"].clone();
        let key = id.to_string();
        let (conn, out, map) = (conn.clone(), out.clone(), inflight.clone());
        let task_key = key.clone();
        seq += 1;
        let my_seq = seq;
        // 등록이 끝나기 전에 task 가 끝나 자기 항목을 지우지 못하는 일이 없도록 잠금을 쥔 채 spawn
        let mut guard = inflight.lock();
        let handle = tasks.spawn(async move {
            let resp = match call_tool(&conn, &name, &args).await {
                Ok(content) => json!({ "jsonrpc": "2.0", "id": id, "result": { "content": content, "isError": false } }),
                Err(e) => json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [text(format!("오류: {e:#}"))], "isError": true } }),
            };
            {
                let mut m = map.lock();
                if m.get(&task_key).is_some_and(|(s, _)| *s == my_seq) {
                    m.remove(&task_key);
                }
            }
            // 취소 알림을 받은 task 는 위의 await 지점에서 멈추므로 여기까지 오지 않는다
            let _ = out.send(format!("{resp}\n"));
        });
        guard.insert(key, (my_seq, handle));
        drop(guard);
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}

/// 진짜 stdout 을 따로 떼어 내고, fd 1 은 stderr 로 돌린다.
/// 그러면 어디선가 println! 이 불려도 JSON-RPC 스트림을 더럽히지 않는다.
#[cfg(unix)]
fn take_stdout() -> Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: 표준 fd 를 복제·재지정할 뿐이며, 복제한 fd 는 여기서 만든 File 하나만 소유한다
    unsafe {
        let fd = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3);
        if fd < 0 {
            bail!("stdout 복제 실패: {}", std::io::Error::last_os_error());
        }
        if libc::dup2(2, 1) < 0 {
            // stderr 가 닫혀 있는 등: 떠도는 출력은 버린다
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
            if null < 0 || libc::dup2(null, 1) < 0 {
                let e = std::io::Error::last_os_error();
                libc::close(fd);
                bail!("stdout 재지정 실패: {e}");
            }
            libc::close(null);
        }
        Ok(std::fs::File::from_raw_fd(fd))
    }
}

/// Windows: 진짜 stdout 을 상속되지 않는 사본(DuplicateHandle)으로 떼어 내고,
/// 표준 출력 핸들은 stderr(없으면 NUL)로 돌린다. Rust 의 println! 은 쓸 때마다 표준 출력 핸들을 다시 읽으므로
/// 어디선가 println! 이 불려도 JSON-RPC 스트림을 더럽히지 않는다.
#[cfg(windows)]
fn take_stdout() -> Result<std::fs::File> {
    use std::os::windows::io::{AsHandle, IntoRawHandle};
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
    let owned = std::io::stdout().as_handle().try_clone_to_owned().map_err(|e| anyhow!("stdout 복제 실패: {e}"))?;
    let file = std::fs::File::from(owned);
    // SAFETY: 표준 핸들을 읽고 바꿀 뿐이다. NUL 핸들은 프로세스가 끝날 때까지 표준 출력으로 쓰므로 일부러 닫지 않는다.
    unsafe {
        let err = GetStdHandle(STD_ERROR_HANDLE);
        let target: HANDLE = if !err.is_null() && err != INVALID_HANDLE_VALUE {
            err
        } else {
            // stderr 가 없는 등: 떠도는 출력은 버린다
            match std::fs::OpenOptions::new().write(true).open("NUL") {
                Ok(f) => f.into_raw_handle() as HANDLE,
                Err(e) => bail!("stdout 재지정 실패: {e}"),
            }
        };
        if SetStdHandle(STD_OUTPUT_HANDLE, target) == 0 {
            bail!("stdout 재지정 실패: {}", std::io::Error::last_os_error());
        }
    }
    Ok(file)
}

/// MCP stdio 서버 본체
pub async fn run(_args: &[String]) -> Result<()> {
    // Windows: MCP 호스트가 넘긴 표준 입출력 파이프가 GhostDesk 창(→ 브라우저 …)으로 상속되면, 이 프로세스가
    // 끝나도 호스트가 EOF 를 받지 못한다. 무엇이든 띄우기 전에 상속을 끈다.
    #[cfg(windows)]
    crate::platform::win::harden_std_handles();
    let mut stdout = take_stdout()?;
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    // stdout 은 이 스레드 하나만 쓴다 (한 줄씩 통째로 쓰고 flush)
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        while let Some(line) = rx.blocking_recv() {
            if let Err(e) = stdout.write_all(line.as_bytes()).and_then(|_| stdout.flush()) {
                eprintln!("[ghostdesk-mcp] stdout 쓰기 실패: {e}");
                break;
            }
        }
    });
    let launcher: Launcher = Arc::new(launch_gui);
    let conn = Arc::new(Conn::new(control::endpoint(), Some(launcher)));
    let r = serve(tokio::io::stdin(), tx, conn).await;
    // serve 가 보낸 응답을 다 쓸 때까지 기다린다 (tx 는 serve 와 함께 버려졌다)
    let _ = tokio::task::spawn_blocking(move || writer.join()).await;
    r
}


const SNIPPET_CLI_TITLE: &str = if cfg!(windows) { "Claude Code (터미널: 명령 프롬프트·PowerShell)" } else { "Claude Code (터미널)" };
const SNIPPET_CLAUDE_APP_TITLE: &str = if cfg!(windows) {
    // Windows 의 Claude 앱은 설치 방식(MSIX 등)에 따라 설정 파일 위치가 달라, 앱이 직접 열어 주는 메뉴로 안내한다
    "Claude 앱 (Settings > Developer > Edit Config 로 여는 claude_desktop_config.json 의 mcpServers 에 추가)"
} else {
    "Claude 앱 (claude_desktop_config.json 의 mcpServers 에 추가)"
};
const SNIPPET_CODEX_TITLE: &str =
    if cfg!(windows) { r"Codex (%USERPROFILE%\.codex\config.toml 에 추가)" } else { "Codex (~/.codex/config.toml 에 추가)" };

/// TOML 문자열 값. 따옴표·역슬래시가 없으면 예전처럼 "…", Windows 경로처럼 역슬래시가 있으면 '…'(리터럴),
/// 그것도 안 되면 이스케이프한 "…".
fn toml_str(s: &str) -> String {
    let plain = |c: char| !c.is_control() || c == '\t';
    if !s.contains(['"', '\\']) && s.chars().all(plain) {
        return format!("\"{s}\"");
    }
    if !s.contains('\'') && s.chars().all(plain) {
        return format!("'{s}'");
    }
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() && c != '\t' => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 연결 안내 (UI 와 README 에서 사용). `exe` 는 GhostDesk 실행 파일 경로.
/// 경로는 JSON·TOML 규칙대로 이스케이프한다 (Windows 의 `C:\Users\…` 도 그대로 붙여 넣어 쓸 수 있게).
pub fn setup_snippets(exe: &str) -> Vec<(&'static str, String)> {
    let json_exe = serde_json::to_string(exe).unwrap_or_else(|_| format!("\"{exe}\""));
    let toml_exe = toml_str(exe);
    vec![
        (
            SNIPPET_CLI_TITLE,
            format!("claude mcp add ghostdesk -e GHOSTDESK_CLIENT_NAME=\"Claude Code\" -- \"{exe}\" --mcp"),
        ),
        (
            SNIPPET_CLAUDE_APP_TITLE,
            format!("\"ghostdesk\": {{\n  \"command\": {json_exe},\n  \"args\": [\"--mcp\"],\n  \"env\": {{ \"GHOSTDESK_CLIENT_NAME\": \"Claude 앱\" }}\n}}"),
        ),
        (
            SNIPPET_CODEX_TITLE,
            format!("[mcp_servers.ghostdesk]\ncommand = {toml_exe}\nargs = [\"--mcp\"]\nenv = {{ GHOSTDESK_CLIENT_NAME = \"Codex\" }}"),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tools_have_schemas() {
        let t = tools();
        for tool in t.as_array().unwrap() {
            assert!(tool["name"].is_string());
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
        let c = to_content(&json!({"text": "OK", "notes": ["a"], "size": [10, 20], "screenshot_png": "AAA"}), None);
        assert_eq!(c.len(), 2);
        assert_eq!(c[1]["type"], "image");
    }

    fn mode_names(pl: &Plat) -> Vec<String> {
        let t = tools_for(pl);
        t[0]["inputSchema"]["properties"]["mode"]["enum"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn schema_matches_platform_caps() {
        let t = tools();
        let open = &t[0];
        let props = &open["inputSchema"]["properties"];
        assert_eq!(open["inputSchema"]["required"], json!(["mode"]));
        // 옛 조합 인자는 알리지 않는다 (mode 하나로 고른다)
        for gone in ["kind", "use_my_login", "app_host", "attach_running", "virtual_display"] {
            assert!(props.get(gone).is_none(), "{gone}");
        }
        let modes = mode_names(&Plat::CURRENT);
        assert_eq!(modes.contains(&"session".into()), caps::SESSION);
        assert_eq!(modes.contains(&"app".into()), caps::ELECTRON || caps::NATIVE_APPS);
        assert_eq!(modes.contains(&"app_vdisplay".into()), caps::VDISPLAY);
        assert!(modes.contains(&"web".into()) && modes.contains(&"web_login".into()) && modes.contains(&"remote".into()));
        assert_eq!(props.get("account").is_some(), caps::SESSION && !cfg!(windows));
        // mcp 와 control 의 mode 이름이 어긋나지 않게
        for m in mode_names(&Plat::CURRENT).iter().chain(mode_names(&Plat::MAC).iter()) {
            assert!(crate::control::MODES.contains(&m.as_str()), "{m}");
        }
        let mac = mode_names(&Plat::MAC);
        assert!(mac.contains(&"app_vdisplay".into()) && !mac.contains(&"app_current_session".into()));
        let ins = server_instructions();
        assert!(ins.contains("어떤 모드로 할지") && ins.contains("mode"), "{ins}");
        if cfg!(windows) {
            assert!(modes.contains(&"app_current_session".into()) && modes.contains(&"app_running".into()));
            let desc = props["mode"]["description"].as_str().unwrap();
            assert!(desc.contains("자식 세션") && !desc.contains("Mac") && !desc.contains("macOS"), "{desc}");
            assert!(!ins.contains("Mac"), "{ins}");
            assert!(t[1]["description"].as_str().unwrap().contains("'ctrl+a'"));
            assert!(t[3]["description"].as_str().unwrap().contains("Windows 앱:"));
            let app = props["app"]["description"].as_str().unwrap();
            // 앱 이름 예시는 시작 메뉴 바로 가기(.lnk) 이름으로 실제로 찾을 수 있는 것이어야 한다
            assert!(app.contains(".exe") && app.contains("'Character Map'") && !app.contains("문자표"), "{app}");
        }
        if !cfg!(target_os = "macos") {
            // 이 컴퓨터 자신의 VNC 서버 보호도 풀리므로, 사용자가 말했을 때만 켜라고 적는다
            let d = props["dedicated_machine"]["description"].as_str().unwrap();
            assert!(d.contains("직접 말했을 때만") && d.contains("이 컴퓨터 자신의 VNC 서버") && !d.contains("Mac"), "{d}");
        }
    }

    #[test]
    fn setup_snippets_are_valid_json_and_toml() {
        for exe in [
            r"C:\Users\홍길동\AppData\Local\Programs\GhostDesk\ghostdesk.exe",
            r"C:\Users\O'Brien\Apps\GhostDesk\ghostdesk.exe",
            "/Users/a b/Applications/GhostDesk.app/Contents/MacOS/ghostdesk",
            "/tmp/we\"ird\\x",
        ] {
            let s = setup_snippets(exe);
            assert_eq!(s.len(), 3);
            assert!(s[0].1.contains(&format!("\"{exe}\" --mcp")), "{}", s[0].1);
            let j: Value = serde_json::from_str(&format!("{{{}}}", s[1].1)).unwrap_or_else(|e| panic!("{exe}: {e}\n{}", s[1].1));
            assert_eq!(j["ghostdesk"]["command"], exe);
            assert_eq!(j["ghostdesk"]["args"], json!(["--mcp"]));
            let doc: toml_edit::DocumentMut = s[2].1.parse().unwrap_or_else(|e| panic!("{exe}: {e}\n{}", s[2].1));
            assert_eq!(doc["mcp_servers"]["ghostdesk"]["command"].as_str(), Some(exe), "{}", s[2].1);
            assert_eq!(doc["mcp_servers"]["ghostdesk"]["env"]["GHOSTDESK_CLIENT_NAME"].as_str(), Some("Codex"));
        }
        // macOS 경로는 예전과 글자 하나 다르지 않다
        let mac = "/Users/a/Applications/GhostDesk.app/Contents/MacOS/ghostdesk";
        let s = setup_snippets(mac);
        assert_eq!(s[0].1, format!("claude mcp add ghostdesk -e GHOSTDESK_CLIENT_NAME=\"Claude Code\" -- \"{mac}\" --mcp"));
        assert_eq!(
            s[1].1,
            format!("\"ghostdesk\": {{\n  \"command\": \"{mac}\",\n  \"args\": [\"--mcp\"],\n  \"env\": {{ \"GHOSTDESK_CLIENT_NAME\": \"Claude 앱\" }}\n}}")
        );
        assert_eq!(s[2].1, format!("[mcp_servers.ghostdesk]\ncommand = \"{mac}\"\nargs = [\"--mcp\"]\nenv = {{ GHOSTDESK_CLIENT_NAME = \"Codex\" }}"));
        if cfg!(target_os = "macos") {
            let titles: Vec<&str> = s.iter().map(|x| x.0).collect();
            assert_eq!(titles, ["Claude Code (터미널)", "Claude 앱 (claude_desktop_config.json 의 mcpServers 에 추가)", "Codex (~/.codex/config.toml 에 추가)"]);
        }
        if cfg!(windows) {
            assert!(s[2].0.contains(r"%USERPROFILE%\.codex\config.toml"));
            // Windows 경로는 읽기 쉬운 TOML 리터럴 문자열로
            let w = setup_snippets(r"C:\Program Files\GhostDesk\ghostdesk.exe");
            assert!(w[2].1.contains(r"command = 'C:\Program Files\GhostDesk\ghostdesk.exe'"), "{}", w[2].1);
            assert!(w[1].1.contains(r#""command": "C:\\Program Files\\GhostDesk\\ghostdesk.exe""#), "{}", w[1].1);
        }
    }

    mod bridge {
        use super::super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::{io::AsyncWriteExt, sync::watch};

        /// 가짜 제어 서버 (GhostDesk 창 대신). 요청마다 task 를 돌려 응답 순서가 요청 순서와 달라질 수 있다.
        /// 실제와 같은 `ipc` 연결 지점(Unix: 임시 소켓, Windows: 명명 파이프) 위에서 돈다.
        struct Fake {
            conns: Arc<AtomicUsize>,
            acts: Arc<AtomicUsize>,
            stop: watch::Sender<bool>,
            /// Unix 는 멈출 때 소켓 파일을 지운다 (명명 파이프는 마지막 핸들이 닫히면 저절로 사라진다)
            #[cfg_attr(not(unix), allow(dead_code))]
            ep: ipc::Endpoint,
        }

        impl Fake {
            async fn start(ep: &ipc::Endpoint) -> Fake {
                #[cfg(unix)]
                let _ = std::fs::remove_file(ep.path());
                // 앞서 멈춘 서버의 파이프 인스턴스가 아직 닫히는 중일 수 있어(Windows 의 첫 인스턴스 검사) 잠깐 다시 시도한다
                let t0 = Instant::now();
                let mut listener = loop {
                    match ipc::Listener::bind(ep).await {
                        Ok(l) => break l,
                        Err(e) if t0.elapsed() > Duration::from_secs(5) => panic!("가짜 서버를 열지 못함: {e:#}"),
                        Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                    }
                };
                let conns = Arc::new(AtomicUsize::new(0));
                let acts = Arc::new(AtomicUsize::new(0));
                let (stop, stop_rx) = watch::channel(false);
                let (c, a) = (conns.clone(), acts.clone());
                let mut accept_stop = stop_rx.clone();
                tokio::spawn(async move {
                    loop {
                        let stream = tokio::select! {
                            r = listener.accept() => match r { Ok(s) => s, Err(_) => return },
                            _ = accept_stop.changed() => return,
                        };
                        c.fetch_add(1, Ordering::SeqCst);
                        tokio::spawn(Self::conn(stream, a.clone(), stop_rx.clone()));
                    }
                });
                Fake { conns, acts, stop, ep: ep.clone() }
            }

            /// 동기 런처(GUI 띄우기 흉내) 안에서 쓰는 `start`
            fn start_blocking(ep: &ipc::Endpoint) -> Fake {
                tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(Fake::start(ep)))
            }

            async fn conn((rd, wr): (ipc::Reader, ipc::Writer), acts: Arc<AtomicUsize>, mut stop: watch::Receiver<bool>) {
                let wr = Arc::new(tokio::sync::Mutex::new(wr));
                let (kill_tx, mut kill_rx) = mpsc::unbounded_channel::<()>();
                let mut lines = BufReader::new(rd).lines();
                loop {
                    let line = tokio::select! {
                        l = lines.next_line() => match l { Ok(Some(l)) => l, _ => break },
                        _ = stop.changed() => break,
                        _ = kill_rx.recv() => break,
                    };
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let (wr, acts, kill) = (wr.clone(), acts.clone(), kill_tx.clone());
                    tokio::spawn(async move {
                        let id = req["id"].clone();
                        let p = &req["params"];
                        let resp = match req["method"].as_str().unwrap() {
                            "hello" => json!({ "id": id, "result": { "protocol": 1 } }),
                            "list" => json!({ "id": id, "result": { "desks": [] } }),
                            "open" => json!({ "id": id, "result": { "desk_id": "d101", "kind": "browser", "text": "OK" } }),
                            "act" => {
                                acts.fetch_add(1, Ordering::SeqCst);
                                let input = &p["input"];
                                match input["action"].as_str().unwrap_or("") {
                                    // 요청을 받은 뒤 응답 없이 연결을 끊는다 (실행 도중 GUI 종료)
                                    // (명명 파이프는 shutdown 으로 반쯤 닫히지 않으므로, 연결 task 가 두 반쪽을 모두 버려 닫는다)
                                    "drop" => {
                                        let _ = wr.lock().await.shutdown().await;
                                        let _ = kill.send(());
                                        return;
                                    }
                                    "fail" => json!({ "id": id, "error": "가짜 오류" }),
                                    _ => {
                                        let ms = input["delay_ms"].as_u64().unwrap_or(0);
                                        tokio::time::sleep(Duration::from_millis(ms)).await;
                                        json!({ "id": id, "result": { "text": format!("done {}", input["text"].as_str().unwrap_or("")) } })
                                    }
                                }
                            }
                            other => json!({ "id": id, "error": format!("알 수 없는 요청: {other}") }),
                        };
                        let _ = wr.lock().await.write_all(format!("{resp}\n").as_bytes()).await;
                    });
                }
                let _ = wr.lock().await.shutdown().await;
            }

            /// GUI 종료 흉내: 모든 연결을 끊고 연결 지점을 없앤다 (Unix: 소켓 파일 삭제)
            fn stop(&self) {
                let _ = self.stop.send(true);
                #[cfg(unix)]
                let _ = std::fs::remove_file(self.ep.path());
            }
        }

        impl Drop for Fake {
            fn drop(&mut self) {
                self.stop();
            }
        }

        /// 테스트마다 다른 연결 지점 (Unix: 임시 폴더의 .sock, Windows: \\.\pipe\gdmcp-…-<SID>)
        pub(super) fn test_ep() -> ipc::Endpoint {
            static N: AtomicUsize = AtomicUsize::new(0);
            ipc::Endpoint::custom(&format!("gdmcp-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)))
        }

        fn act(text: &str, delay_ms: u64) -> Value {
            json!({ "desk_id": "d101", "tool": "computer", "input": { "action": "x", "text": text, "delay_ms": delay_ms } })
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn multiplexes_out_of_order_responses_on_one_connection() {
            let ep = test_ep();
            let fake = Fake::start(&ep).await;
            let conn = Conn::new(ep.clone(), None);
            let t0 = Instant::now();
            let (slow, fast) = tokio::join!(conn.call("act", act("A", 400)), async {
                let r = conn.call("act", act("B", 0)).await;
                (r, t0.elapsed())
            });
            assert_eq!(slow.unwrap()["text"], "done A");
            let (fast, fast_at) = fast;
            assert_eq!(fast.unwrap()["text"], "done B");
            assert!(fast_at < Duration::from_millis(300), "빠른 요청이 느린 요청에 막힘: {fast_at:?}");
            // 제어 서버 오류는 연결을 끊지 않는다
            let e = conn.call("act", json!({ "input": { "action": "fail" } })).await.unwrap_err();
            assert!(format!("{e}").contains("가짜 오류"));
            assert_eq!(conn.call("list", json!({})).await.unwrap()["desks"], json!([]));
            assert_eq!(fake.conns.load(Ordering::SeqCst), 1, "연결은 하나만 써야 한다");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn reconnects_and_relaunches_after_gui_restart() {
            let ep = test_ep();
            let first = Fake::start(&ep).await;
            let relaunched: Arc<PMutex<Option<Fake>>> = Arc::new(PMutex::new(None));
            let launches = Arc::new(AtomicUsize::new(0));
            let (r, l, p) = (relaunched.clone(), launches.clone(), ep.clone());
            let launcher: Launcher = Arc::new(move || {
                l.fetch_add(1, Ordering::SeqCst);
                *r.lock() = Some(Fake::start_blocking(&p));
                Ok(())
            });
            let conn = Conn::new(ep.clone(), Some(launcher));
            assert_eq!(conn.call("act", act("1", 0)).await.unwrap()["text"], "done 1");

            // GUI 종료 → 브리지가 끊김을 알아챈 뒤, 다음 호출은 창을 다시 띄우고 재연결해서 성공해야 한다
            first.stop();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(conn.call("act", act("2", 0)).await.unwrap()["text"], "done 2");
            assert_eq!(launches.load(Ordering::SeqCst), 1);
            let second = relaunched.lock().take().unwrap();
            assert_eq!(second.conns.load(Ordering::SeqCst), 1);
            assert_eq!(second.acts.load(Ordering::SeqCst), 1);

            // 연결 지점은 살아 있고 연결만 끊긴 경우(창은 켜져 있음)에는 다시 띄우지 않고 재연결만 한다
            let _ = second.stop.send(true);
            let third = Fake::start(&ep).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(conn.call("list", json!({})).await.unwrap()["desks"], json!([]));
            assert_eq!(launches.load(Ordering::SeqCst), 1);
            assert_eq!(third.conns.load(Ordering::SeqCst), 1);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn request_lost_mid_flight_is_not_retried() {
            let ep = test_ep();
            let fake = Fake::start(&ep).await;
            let conn = Conn::new(ep.clone(), None);
            let e = conn.call("act", json!({ "desk_id": "d101", "input": { "action": "drop" } })).await.unwrap_err();
            let msg = format!("{e}");
            assert!(msg.contains("desk_list"), "{msg}");
            assert_eq!(fake.acts.load(Ordering::SeqCst), 1, "실행되었을 수 있는 요청을 다시 보내면 안 된다");
            // 다음 호출은 새 연결로 정상 동작
            assert_eq!(conn.call("act", act("again", 0)).await.unwrap()["text"], "done again");
            assert_eq!(fake.conns.load(Ordering::SeqCst), 2);
        }

        #[tokio::test]
        async fn connect_failure_without_launcher_is_an_error() {
            let conn = Conn::new(test_ep(), None);
            assert!(conn.call("list", json!({})).await.is_err());
        }

        /// serve 를 띄우고 (입력 쓰기 핸들, 응답 수신기, serve 핸들) 을 돌려준다
        pub(super) fn start_serve(
            ep: &ipc::Endpoint,
        ) -> (tokio::io::DuplexStream, mpsc::UnboundedReceiver<String>, tokio::task::JoinHandle<Result<()>>) {
            let (client, server) = tokio::io::duplex(1 << 16);
            let (tx, rx) = mpsc::unbounded_channel();
            let conn = Arc::new(Conn::new(ep.clone(), None));
            let h = tokio::spawn(serve(server, tx, conn));
            (client, rx, h)
        }

        pub(super) async fn send_line(w: &mut tokio::io::DuplexStream, v: &str) {
            w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
        }

        pub(super) async fn recv(rx: &mut mpsc::UnboundedReceiver<String>) -> Value {
            let line = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("응답 없음").expect("채널 닫힘");
            assert!(line.ends_with('\n') && !line.trim_end().contains('\n'), "한 줄짜리 JSON 이어야 함: {line:?}");
            serde_json::from_str(&line).unwrap()
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn ping_and_tools_list_answer_while_call_runs() {
            let ep = test_ep();
            let _fake = Fake::start(&ep).await;
            let (mut w, mut rx, h) = start_serve(&ep);
            send_line(
                &mut w,
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desk_action","arguments":{"desk_id":"d101","action":"x","text":"slow","delay_ms":600}}}"#,
            )
            .await;
            send_line(&mut w, r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#).await;
            send_line(&mut w, r#"{"jsonrpc":"2.0","id":"t","method":"tools/list"}"#).await;
            send_line(
                &mut w,
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"desk_action","arguments":{"desk_id":"d101","action":"x","text":"fast"}}}"#,
            )
            .await;
            let a = recv(&mut rx).await;
            assert_eq!(a["id"], 2);
            assert_eq!(a["result"], json!({}));
            let b = recv(&mut rx).await;
            assert_eq!(b["id"], "t");
            assert!(b["result"]["tools"].is_array());
            let c = recv(&mut rx).await;
            assert_eq!(c["id"], 4, "빠른 호출이 느린 호출보다 먼저 끝나야 함: {c}");
            assert_eq!(c["result"]["content"][0]["text"], "done fast");
            // 입력을 닫아도 진행 중인 호출의 응답은 보낸 뒤 끝난다
            drop(w);
            let d = recv(&mut rx).await;
            assert_eq!(d["id"], 1);
            assert_eq!(d["result"]["isError"], false);
            assert_eq!(d["result"]["content"][0]["text"], "done slow");
            h.await.unwrap().unwrap();
        }

        #[tokio::test]
        async fn batch_parse_error_and_notifications() {
            let ep = test_ep();
            let (mut w, mut rx, h) = start_serve(&ep);
            send_line(&mut w, r#"[{"jsonrpc":"2.0","id":3,"method":"ping"},{"jsonrpc":"2.0","id":4,"method":"ping"}]"#).await;
            let r = recv(&mut rx).await;
            assert_eq!(r["id"], Value::Null);
            assert_eq!(r["error"]["code"], -32600);
            assert!(r["error"]["message"].as_str().unwrap().contains("batch"));
            send_line(&mut w, "[]").await;
            assert_eq!(recv(&mut rx).await["error"]["code"], -32600);
            send_line(&mut w, "{not json").await;
            let r = recv(&mut rx).await;
            assert_eq!(r["error"]["code"], -32700);
            assert_eq!(r["id"], Value::Null);
            // 알림과 클라이언트가 보낸 응답에는 답하지 않는다
            send_line(&mut w, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await;
            send_line(&mut w, r#"{"jsonrpc":"2.0","id":77,"result":{}}"#).await;
            send_line(&mut w, r#"{"jsonrpc":"2.0","id":5,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#).await;
            let r = recv(&mut rx).await;
            assert_eq!(r["id"], 5);
            assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
            send_line(&mut w, r#"{"jsonrpc":"2.0","id":6,"method":"nope"}"#).await;
            assert_eq!(recv(&mut rx).await["error"]["code"], -32601);
            send_line(&mut w, r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{}}"#).await;
            assert_eq!(recv(&mut rx).await["error"]["code"], -32602);
            drop(w);
            h.await.unwrap().unwrap();
            assert!(rx.recv().await.is_none());
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn cancelled_call_gets_no_response() {
            let ep = test_ep();
            let _fake = Fake::start(&ep).await;
            let (mut w, mut rx, h) = start_serve(&ep);
            send_line(
                &mut w,
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desk_action","arguments":{"desk_id":"d101","action":"x","delay_ms":3000}}}"#,
            )
            .await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            send_line(&mut w, r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#).await;
            send_line(&mut w, r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#).await;
            assert_eq!(recv(&mut rx).await["id"], 2);
            drop(w);
            let t0 = Instant::now();
            h.await.unwrap().unwrap();
            assert!(t0.elapsed() < Duration::from_secs(2), "취소된 호출을 기다리면 안 됨");
            assert!(rx.recv().await.is_none(), "취소된 요청에는 응답하지 않는다");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn tool_call_reports_lost_request_clearly() {
            let ep = test_ep();
            let _fake = Fake::start(&ep).await;
            let (mut w, mut rx, h) = start_serve(&ep);
            send_line(
                &mut w,
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desk_action","arguments":{"desk_id":"d101","action":"drop"}}}"#,
            )
            .await;
            let r = recv(&mut rx).await;
            assert_eq!(r["result"]["isError"], true);
            assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("desk_list"));
            drop(w);
            h.await.unwrap().unwrap();
        }
    }

    /// Windows: take_stdout 뒤의 println! 은 stderr 로 가고, 떼어 낸 핸들로 쓴 것만 진짜 stdout 에 남는다.
    /// (프로세스의 표준 핸들을 바꾸므로 이 테스트 실행 파일을 자식 프로세스로 다시 돌려서 확인한다)
    #[cfg(windows)]
    #[test]
    fn stray_prints_do_not_reach_stdout() {
        if std::env::var_os("GHOSTDESK_TEST_TAKE_STDOUT").is_some() {
            use std::io::Write;
            let mut f = take_stdout().unwrap();
            println!("STRAY-PRINT");
            f.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1}\n").unwrap();
            f.flush().unwrap();
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "mcp::tests::stray_prints_do_not_reach_stdout", "--nocapture", "--test-threads=1"])
            .env("GHOSTDESK_TEST_TAKE_STDOUT", "1")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let (so, se) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "{so}\n{se}");
        assert!(so.contains("{\"jsonrpc\":\"2.0\",\"id\":1}\n"), "stdout: {so}");
        assert!(!so.contains("STRAY-PRINT"), "떠도는 출력이 stdout 에 섞임: {so}");
        assert!(se.contains("STRAY-PRINT"), "stderr: {se}");
    }

    /// Windows: run() 처럼 표준 핸들 상속을 끈 뒤 분리 실행한 자식(GUI 대역)은 호스트의 파이프를 물려받지 않는다.
    /// 그래서 MCP 서버가 끝나는 즉시 호스트가 stdout EOF 를 받는다 (자식이 아직 살아 있어도).
    #[cfg(windows)]
    #[test]
    fn launched_gui_does_not_inherit_host_pipes() {
        use std::process::{Command, Stdio};
        if std::env::var_os("GHOSTDESK_TEST_INHERIT").is_some() {
            crate::platform::win::harden_std_handles();
            let _out = take_stdout().unwrap();
            // GUI 대신 3초쯤 살아 있는 콘솔 없는 프로세스
            let mut cmd = Command::new("cmd.exe");
            cmd.args(["/d", "/c", "ping -n 4 127.0.0.1 >NUL"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            crate::platform::proc::spawn_detached(&mut cmd).unwrap();
            return;
        }
        use std::io::Read;
        let t0 = Instant::now();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "mcp::tests::launched_gui_does_not_inherit_host_pipes", "--nocapture", "--test-threads=1"])
            .env("GHOSTDESK_TEST_INHERIT", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut so = String::new();
        child.stdout.take().unwrap().read_to_string(&mut so).unwrap();
        let eof_at = t0.elapsed();
        let mut se = String::new();
        child.stderr.take().unwrap().read_to_string(&mut se).unwrap();
        assert!(child.wait().unwrap().success(), "{so}\n{se}");
        assert!(eof_at < Duration::from_millis(2500), "자식이 호스트의 stdout 파이프를 물려받아 EOF 가 늦음: {eof_at:?}");
    }

    /// stdio 서버 루프: initialize → (알림) → tools/list → ping. 창이 없어도 곧바로 답해야 한다.
    #[tokio::test]
    async fn initialize_then_tools_list_over_stdio_loop() {
        use bridge::{recv, send_line, start_serve, test_ep};
        let ep = test_ep();
        let (mut w, mut rx, h) = start_serve(&ep);
        send_line(
            &mut w,
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        )
        .await;
        let r = recv(&mut rx).await;
        assert_eq!(r["jsonrpc"], "2.0");
        assert_eq!(r["id"], 0);
        assert_eq!(r["result"]["serverInfo"]["name"], "ghostdesk");
        assert_eq!(r["result"]["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(r["result"]["instructions"], server_instructions());
        send_line(&mut w, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await;
        send_line(&mut w, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).await;
        let r = recv(&mut rx).await;
        assert_eq!(r["id"], 1);
        let names: Vec<&str> = r["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["desk_open", "desk_action", "desk_browser", "desk_app", "desk_list", "desk_close"]);
        assert_eq!(r["result"]["tools"], tools());
        send_line(&mut w, r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#).await;
        assert_eq!(recv(&mut rx).await["id"], "p");
        drop(w);
        h.await.unwrap().unwrap();
        assert!(rx.recv().await.is_none(), "알림에는 답하지 않는다");
    }

    /// `ghostdesk --mcp` 의 stdio 루프 → 실제 제어 서버(control.rs, GUI 없이)까지 한 번에.
    /// 연결 지점은 실제와 같은 ipc 계층(Unix 소켓 / Windows 명명 파이프)이다.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdio_loop_to_real_control_server() {
        use bridge::{recv, send_line, start_serve, test_ep};
        use crate::control::ControlServer;
        let ep = test_ep();
        #[cfg(unix)]
        let _ = std::fs::remove_file(ep.path());
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let srv = ControlServer::new(ev_tx, eframe::egui::Context::default());
        let (srv2, ep2) = (srv.clone(), ep.clone());
        let server = tokio::spawn(async move { srv2.serve_on(&ep2).await });
        // 연결을 받기 시작할 때까지
        let t0 = Instant::now();
        while ipc::connect(&ep).await.is_err() {
            assert!(t0.elapsed() < Duration::from_secs(5), "제어 서버가 열리지 않음");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // 같은 연결 지점에 두 번째 창은 열 수 없다
        let (tx2, _rx2) = mpsc::unbounded_channel();
        let dup = ControlServer::new(tx2, eframe::egui::Context::default()).serve_on(&ep).await.unwrap_err();
        assert!(format!("{dup:#}").contains("다른 GhostDesk 창"), "{dup:#}");

        let (mut w, mut rx, h) = start_serve(&ep);
        let call = |id: u32, name: &str, args: Value| {
            json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": args } }).to_string()
        };
        send_line(&mut w, &call(1, "desk_list", json!({}))).await;
        let r = recv(&mut rx).await;
        assert_eq!(r["result"]["isError"], false, "{r}");
        assert_eq!(r["result"]["content"][0]["text"], "[]");
        send_line(&mut w, &call(2, "desk_close", json!({ "desk_id": "nope" }))).await;
        let r = recv(&mut rx).await;
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("찾을 수 없습니다"), "{r}");
        if !caps::SESSION {
            // 지원하지 않는 종류는 승인 창 없이 바로 오류
            send_line(&mut w, &call(3, "desk_open", json!({ "kind": "session" }))).await;
            let r = recv(&mut rx).await;
            assert_eq!(r["result"]["isError"], true);
            assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains(text::OS_NAME), "{r}");
        }
        #[cfg(windows)]
        {
            // 셸은 이름만으로 승인 창 없이 거부
            send_line(&mut w, &call(4, "desk_open", json!({ "kind": "app", "app": "powershell" }))).await;
            let r = recv(&mut rx).await;
            assert_eq!(r["result"]["isError"], true);
            assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("보안상"), "{r}");
        }
        assert!(ev_rx.try_recv().is_err(), "승인 창이 뜨면 안 됨");
        drop(w);
        h.await.unwrap().unwrap();
        server.abort();
    }
}
