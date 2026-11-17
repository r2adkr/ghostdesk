//! `ghostdesk --mcp`: MCP server (stdio) for external agents like the Claude app, Claude Code
//! and Codex.
//!
//! External agents use GhostDesk's isolated screens (browser / Mac and Windows apps / remote
//! desktop) as tools, without an API key. This process is a thin bridge forwarding requests over
//! the GhostDesk window's control connection (Unix socket on macOS/Linux, named pipe on Windows),
//! and it launches the window if it isn't running. That way the user can watch what the external
//! agent does, virtual mouse included, in the GhostDesk window.
//!
//! Only JSON-RPC messages go to stdout; logs go to stderr. (At startup the real stdout is split
//! off and fd 1 (Windows: the stdout handle) is pointed at stderr, so a stray `println!` can't
//! corrupt the stream.)
//!
//! Structure:
//! - Reads stdin line by line and runs each tools/call as its own task. ping and tools/list are
//!   answered right away even while long calls are in progress. Responses go out through a
//!   single stdout writer.
//! - Keeps a single connection to the GhostDesk window (the window closes screens created by a
//!   connection when it drops) and multiplexes requests over it, matching responses by id.
//! - A dropped connection is re-established on the next call (relaunching the window if it's
//!   closed). Requests that failed before being sent are resent on the new connection; requests
//!   lost after sending may already have run, so they aren't.

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

/// This OS's screen kinds and wording, as told to agents (used to build tool descriptions and
/// instructions).
struct Plat {
    /// "Mac 앱" / "Windows 앱".
    local_apps: &'static str,
    /// Primary modifier in shortcut examples ("cmd" / "ctrl").
    primary_mod: &'static str,
    /// Description of the `app` argument.
    app_hint: &'static str,
    /// Whether kind=app is available (hidden Electron clones or regular apps).
    app_kind: bool,
    /// attach_running (driving the user's running app).
    attach: bool,
    /// app_host (hidden desktop vs off-screen window for Windows apps).
    app_host: bool,
    /// kind=session (separate account session).
    session: bool,
    /// Whether kind=session is a Windows child session (same account); false means the macOS
    /// dedicated account session (`account` argument).
    child_session: bool,
    /// virtual_display (virtual monitor).
    vdisplay: bool,
    /// Description of desk_open's `dedicated_machine`.
    dedicated_hint: &'static str,
}

/// macOS description of `dedicated_machine` (unchanged).
const MAC_DEDICATED_HINT: &str = "remote: 대상이 에이전트 전용 Mac/VM 임을 사용자가 확인함";

impl Plat {
    const CURRENT: Plat = Plat {
        local_apps: text::LOCAL_APPS,
        primary_mod: text::PRIMARY_MOD,
        app_hint: if cfg!(windows) {
            "app: 앱 이름(예: 'Visual Studio Code', '카카오톡', 'Character Map'), 실행 파일 이름(예: 'charmap') 또는 .exe/.lnk 경로. 일반 Win32 데스크톱 앱만 된다 (콘솔 프로그램, 관리자 권한 앱은 안 됨). Microsoft Store 앱·Windows 11 의 새 메모장/계산기는 mode=session 인 자식 세션을 사용한다 (기능 켜기·승인 필요)"
        } else {
            "app: 앱 이름(예: 'TextEdit', '계산기'), 번들 id 또는 .app 경로"
        },
        // outside macOS this also lifts the "this computer's own VNC server" protection, so say plainly
        // that agents must not set it on their own
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

    /// The macOS values (tests use them to check the macOS wording is unchanged).
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

pub(crate) fn server_instructions() -> String {
    server_instructions_for(&Plat::CURRENT)
}

/// This OS's desk_open modes (for the connection kit's skill doc).
pub(crate) fn current_modes() -> Vec<(&'static str, String)> {
    modes(&Plat::CURRENT)
}

/// This OS's desk_open modes: `(name, description shown to the user)`. Names must match
/// `apply_mode` in control.rs.
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
    let child_session_hint = if pl.child_session {
        " Windows 의 새 메모장·계산기 같은 Microsoft Store 앱은 session 으로 여세요 (자식 세션 기능 켜기·GhostDesk 창에서 승인 필요)."
    } else {
        ""
    };
    // agents pick the mode themselves to fit the task (without asking the user first); sensitive
    // modes are guarded by the approval dialog in the GhostDesk window. If the user didn't mention
    // GhostDesk but it suits the task, ask once whether to use it. The mode list lives only in
    // desk_open's mode description.
    format!(
        "사용자가 GhostDesk 를 말하지 않았더라도, 웹사이트나 앱 화면을 직접 눌러 보고 입력해야 하는 일(사이트에서 찾기·양식 채우기·화면 확인, 앱 조작 등)이고 \
         GhostDesk 로 하면 사용자의 화면·마우스를 빼앗지 않아 더 낫다고 판단되면, 시작하기 전에 \"GhostDesk 로 할까요?\" 처럼 한 문장으로 쓸지 물어보세요. \
         파일·명령어·API 로 끝낼 수 있는 일에는 묻지 마세요. 같은 대화에서 이미 답을 들었으면 다시 묻지 말고 그 답을 따르세요. \
         사용자가 'GhostDesk 로', '고스트데스크에서', '내 화면 건드리지 말고' 같은 말로 작업을 맡기면, 사용자에게 모드를 묻지 말고 desk_open 의 mode 목록에서 할 일에 가장 알맞은 것을 스스로 골라 바로 desk_open 하세요. \
         고르는 기준: 웹사이트에서 할 일이면 web, 그중 내 로그인(메일·쇼핑·SNS 계정 등)이 필요하면 web_login, 이 컴퓨터의 특정 앱에서 할 일이면 app, \
         그 앱에 이미 로그인된 상태가 필요하면 app_login.{child_session_hint} 사용자가 모드를 말했으면 그것을 쓰세요. 로그인 복제·실행 중인 앱·원격 접속 같은 모드는 \
         GhostDesk 창에서 사용자가 승인하므로 따로 묻지 않아도 됩니다. desk_open(mode) → desk_action/desk_browser/desk_app 반복 → 결과 보고 → desk_close 로 끝까지 진행하세요. \
         고른 모드가 실패하거나 승인이 거절되면 이유를 짧게 알리고, 더 알맞은 다른 모드가 있으면 그것으로 다시 시도하세요 (같은 승인을 되풀이해 요청하지는 마세요). \
         화면은 사용자의 화면·마우스·키보드와 분리되어 있고, 동작마다 스크린샷이 돌아옵니다. 좌표는 최신 스크린샷의 픽셀입니다. 브라우저는 desk_browser 로 URL·탭을{app_tools} 다룹니다. \
         사용자가 GhostDesk 창에서 실시간으로 지켜봅니다. 메시지 전송·결제·삭제처럼 되돌릴 수 없는 동작은 먼저 확인받으세요. \
         사용자가 팀 이름을 주거나 여러 에이전트가 함께 하라고 하면 team(action=join, role=맡을 분야) 으로 들어가 목표·할 일·메모를 읽고, \
         할 일은 team_task 로 claim → done(결과) 하며, 다른 팀원에게 필요한 발견·결정은 team(action=note) 로 짧게 남기세요. 일을 시작하기 전과 끝낸 뒤에 team(action=read) 로 새 메모를 확인하세요."
    )
}

pub(crate) fn tools() -> Value {
    tools_for(&Plat::CURRENT)
}

fn tools_for(pl: &Plat) -> Value {
    let apps = pl.local_apps;
    let child_type_hint = if pl.child_session {
        " Windows 자식 세션에서 긴 글을 쓸 때는 입력 칸의 포커스·선택 상태를 스크린샷으로 확인한 뒤 다음 호출에서 type 하세요. 포커스를 바꾸는 키·클릭과 긴 글 입력은 묶지 마세요."
    } else { "" };
    let modes = modes(pl);
    let names: Vec<&str> = modes.iter().map(|(n, _)| *n).collect();
    let mode_desc = modes.iter().map(|(n, d)| format!("{n}={d}")).collect::<Vec<_>>().join("; ");
    let app_desc = format!(
        "앱 화면의 보조 조작. Electron 앱: action=list|click (source=app|context, path='File > New Text File') 로 네이티브 메뉴 사용. dialog 로 대기 중인 대화상자를 조회하고 respond_dialog 에 반환된 source·id 를 지정한다. source=native 는 response (0부터 시작하는 버튼 번호), source=javascript 는 accept (true/false)와 prompt_text 로 응답한다. 삭제·결제 등 되돌릴 수 없는 응답은 작업에서 허용한 범위인지 먼저 확인한다. {apps}: action=list_windows | switch_window{{index}} | ui_tree{{max_depth}} | press{{id}} | set_value{{id,text}} | focus{{id}} | select{{id}} | choose{{id,option}} | menus | menu{{path:'파일 > 신규'}} | read{{id}}. ui_tree 의 id 는 다음 ui_tree 전까지 유효하다."
    );
    let app_desc = if pl.child_session {
        format!("{app_desc} Windows 자식 세션에서도 list_windows, switch_window, ui_tree, press, set_value, focus, select, choose, read 를 사용할 수 있다. 창·포커스를 바꾼 뒤 ui_tree 를 다시 조회한다. 긴 글은 set_value 후 read 로 확인한다. 이 세션의 앱 메뉴는 화면이나 키보드로 조작한다.")
    } else { app_desc };
    let mut t = json!([
        {
            "name": "desk_open",
            "description": "격리된 화면을 연다 (할 일에 맞는 mode). 결과로 desk_id 와 첫 스크린샷을 돌려준다.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "enum": names, "description": mode_desc },
                    "app": { "type": "string", "description": pl.app_hint },
                    "url": { "type": "string", "description": "web: 처음 열 URL" },
                    "account": { "type": "string", "description": "session: 전용 에이전트 macOS 계정 이름 (기본 ghostdesk-agent)" },
                    "browser": { "type": "string", "description": "web: Chrome / Edge / Whale 등 (기본: 처음 찾은 것)" },
                    "browser_profile": { "type": "string", "description": "web_login: 탐지된 브라우저 프로필 ID (Default, Profile 1 등) 또는 표시 이름. 생략하면 첫 프로필" },
                    "workspace": { "type": "string", "description": "재사용할 작업공간 이름. 이전 복제 로그인·앱 데이터를 유지한다. 로그인 사용 여부별로 분리되며 동시에 사용 중이면 별도 이름으로 열린다" },
                    "refresh_login": { "type": "boolean", "description": "web_login/app_login 전용: 기존 작업공간의 복제 데이터를 원본에서 새로 복사한 데이터로 교체한다. 이전 복제본에서 변경한 데이터는 사라질 수 있다" },
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
            "description": format!("화면에서 마우스/키보드 동작을 한다 (Claude computer-use 와 같은 동작 이름). action: screenshot, left_click, right_click, middle_click, double_click, triple_click, left_click_drag(start_coordinate→coordinate), mouse_move, left_mouse_down, left_mouse_up, scroll(scroll_direction up/down/left/right, scroll_amount), type(text, 한글 가능), key(text: 'Return', '{}+a', 'ctrl+shift+t'; repeat), hold_key(text, duration), wait(duration), zoom(region [x0,y0,x1,y1]), cursor_position. 동작 뒤 스크린샷을 돌려준다. \
                 여러 동작을 한 번에: action 대신 actions 에 동작 객체 배열을 넣으면 순서대로 실행하고 끝에 스크린샷 한 장만 돌려준다 \
                 (예: [{{\"action\":\"left_click\",\"coordinate\":[400,300]}},{{\"action\":\"type\",\"text\":\"서울 날씨\"}},{{\"action\":\"key\",\"text\":\"Return\"}}]). \
                 결과를 보고 판단해야 하는 지점에서는 묶음을 끊으세요.{child_type_hint} 하나가 실패하면 거기서 멈추고 몇 번째까지 했는지 알려 준다 (최대 50개).", pl.primary_mod),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "desk_id": { "type": "string" },
                    "action": { "type": "string" },
                    "actions": {
                        "type": "array",
                        "description": "action 대신: 순서대로 실행할 동작들. 각 항목은 action 과 그 동작의 인자(coordinate, text, …)를 가진 객체",
                        "items": { "type": "object", "properties": { "action": { "type": "string" } }, "required": ["action"] },
                        "minItems": 1, "maxItems": 50
                    },
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
                "required": ["desk_id"]
            }
        },
        {
            "name": "desk_browser",
            "description": "브라우저 화면의 주소 이동·탭 관리·대화상자 응답. action: navigate(url), back, forward, reload, new_tab(url), switch_tab(index), close_tab(index), list_tabs, dialog, respond_dialog(id,accept,prompt_text). confirm/prompt 는 자동 수락하지 않는다. dialog 에서 받은 현재 id 와 명시적인 accept=true/false 로 응답한다. 입력 중 대화상자가 열렸다면 같은 입력을 반복하지 말고 먼저 조회한다. 삭제·결제 같은 응답은 작업에서 허용한 범위인지 확인한다.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "desk_id": { "type": "string" },
                    "action": { "type": "string", "enum": ["navigate", "back", "forward", "reload", "new_tab", "switch_tab", "close_tab", "list_tabs", "dialog", "respond_dialog"] },
                    "url": { "type": "string" },
                    "index": { "type": "integer" },
                    "id": { "type": "integer", "minimum": 1, "description": "respond_dialog: dialog 에서 받은 현재 id" },
                    "accept": { "type": "boolean", "description": "respond_dialog: 수락 true / 취소 false 를 반드시 명시" },
                    "prompt_text": { "type": "string", "description": "respond_dialog: prompt 에 넣을 답변" }
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
                    "actions": {
                        "type": "array",
                        "description": "action 대신: 순서대로 실행할 동작들. 각 항목은 action 과 그 동작의 인자(coordinate, text, …)를 가진 객체",
                        "items": { "type": "object", "properties": { "action": { "type": "string" } }, "required": ["action"] },
                        "minItems": 1, "maxItems": 50
                    },
                    "id": { "type": "integer" }, "index": { "type": "integer" },
                    "text": { "type": "string" }, "option": { "type": "string" },
                    "path": { "type": "string" }, "source": { "type": "string", "enum": ["app", "context", "javascript", "native"], "description": "메뉴 list/click: app|context. respond_dialog: dialog 가 반환한 javascript|native" },
                    "max_depth": { "type": "integer" },
                    "response": { "type": "integer", "minimum": 0, "description": "native respond_dialog: 0부터 시작하는 버튼 번호" },
                    "accept": { "type": "boolean", "description": "javascript respond_dialog: 명시적 수락 또는 취소" },
                    "prompt_text": { "type": "string", "description": "javascript respond_dialog: prompt 답변" }
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
        },
        {
            "name": "team",
            "description": "여러 에이전트가 함께 쓰는 팀 작업판 (목표·메모·할 일). action: list | create(name, goal, role) | join(team, role: 맡을 분야) | read(team, before: 이 메모 번호보다 오래된 것) | note(team, text: 알아낸 것·결정·진행 상황) | goal(team, goal) | leave(team) | delete(team, 사용자 승인). join 하면 목표·할 일·최근 메모가 돌아온다. 화면(desk)은 팀원끼리 공유되지 않는다.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "create", "join", "read", "note", "goal", "leave", "delete"] },
                    "team": { "type": "string", "description": "팀 이름" },
                    "name": { "type": "string", "description": "create: 새 팀 이름" },
                    "goal": { "type": "string" },
                    "role": { "type": "string", "description": "이 팀에서 맡는 분야 (예: 조사, 구현, 검증)" },
                    "text": { "type": "string" },
                    "before": { "type": "integer" }
                },
                "required": ["action"]
            }
        },
        {
            "name": "team_task",
            "description": "팀의 할 일. action: add(team, title, detail) | claim(team, task: 번호, 한 할 일은 한 에이전트만) | done(team, task, result: 결과 요약) | release(team, task: 맡은 것 내려놓기). 먼저 team(action=join) 해야 한다.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["add", "claim", "done", "release"] },
                    "team": { "type": "string" },
                    "task": { "type": "integer" },
                    "title": { "type": "string" },
                    "detail": { "type": "string" },
                    "result": { "type": "string" }
                },
                "required": ["action", "team"]
            }
        }
    ]);
    // don't advertise arguments or tools this OS can't use (so agents don't pick them and then fail
    // at approval)
    if let Some(props) = t[0]["inputSchema"]["properties"].as_object_mut() {
        let drop: [(&str, bool); 2] = [
            ("account", pl.session && !pl.child_session), // Windows child sessions use the same account
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

/// Kinds of control connection call failures; this decides whether retrying is safe.
#[derive(Debug)]
enum CallError {
    /// The connection was already gone before the request was written, so it never ran in GhostDesk
    /// and is safe to resend.
    NotSent(String),
    /// The connection dropped after sending but before the response, so it may already have run.
    Lost(String),
    /// An error returned by the control server (the connection is fine).
    Remote(String),
}

type Reply = std::result::Result<Value, String>;

#[derive(Default)]
struct Pending {
    /// Why the connection dropped, if it did (nothing more is sent on it).
    dead: Option<String>,
    map: HashMap<u64, oneshot::Sender<Reply>>,
}

/// Client for the GhostDesk window's control socket (one connection shared by many requests,
/// told apart by id).
///
/// The control server closes a connection's screens when it drops, so one connection is kept
/// rather than opening one per request. A reader task matches responses to requests by id (in
/// any order), and a dedicated writer task writes whole lines (so a call cancelled midway can't
/// interleave lines).
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

/// Removes the request from the pending list if the call is cancelled before it finishes
/// (notifications/cancelled).
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
    /// Builds a bridge on one connection (read and write halves). `ipc` boxes both sockets and named
    /// pipes.
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
            // mark dead and clear the pending list in one step: woken requests already see `dead` set
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
                    // requests left in the queue have their ack dropped and count as "not sent"
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

/// Function that launches the GUI (tests launch a fake server).
type Launcher = Arc<dyn Fn() -> Result<()> + Send + Sync>;

/// Control connection manager: reconnects on the next call after a drop, launching the window if
/// it's closed.
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

/// Requests that are safe to resend even without a response (no side effects).
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

/// Text shown to the user and agent for a connection failure (Unix keeps the raw OS error, as
/// before).
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

    /// Returns a live connection, connecting anew (launching the GUI if needed) when there is none
    /// or it dropped.
    ///
    /// Connects while holding the state lock, so concurrent calls don't launch the window several
    /// times.
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
            // the endpoint exists but can't be used (another user's or an elevated window, or a program that
            // grabbed the name): launching another window won't help, so report it right away
            Err(ConnectError::Denied(m)) => bail!("GhostDesk 창에 연결하지 못했습니다: {m}"),
            Err(first) => {
                let Some(launch) = &self.launcher else {
                    bail!("GhostDesk 창에 연결하지 못했습니다 ({})", connect_error_text(&first));
                };
                // just launched (still starting up): wait instead of launching again
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

    /// A control server request. If the connection had dropped, reconnects and sends once more.
    ///
    /// Requests lost after sending may already have run, so only side-effect-free requests are
    /// resent.
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


/// Launches the GUI in the background (with `open` inside an .app bundle, otherwise the same
/// executable without arguments).
///
/// Windows: launches the installed copy (`install::stable_exe()`) if there is one, otherwise this
/// executable. It's detached from the MCP host (no console, outside the host's Job, the user's
/// default environment), so the window survives the host ending the MCP server and doesn't
/// inherit the host's pipe handles (`run` turns off std handle inheritance beforehand).
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
    // Windows: a window that doesn't take focus, like macOS's `open -g` (if one is already up, it
    // just exits without bringing it forward)
    #[cfg(windows)]
    cmd.arg("--background");
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    if let Some(dir) = exe.parent() {
        // don't let the window keep the MCP host's working directory (the user's project) busy
        cmd.current_dir(dir);
    }
    // Unix: a new process group (as before); Windows: DETACHED_PROCESS + Job breakaway + the user's
    // default environment
    crate::platform::proc::spawn_detached(&mut cmd).context("GhostDesk 창 실행 실패")?;
    Ok(())
}

/// GUI executable to launch.
fn gui_exe() -> Result<PathBuf> {
    let me = std::env::current_exe().context("실행 파일 경로를 알 수 없습니다")?;
    #[cfg(windows)]
    {
        // prefer the installed copy the connection config points at (so a copy running from a temp
        // location doesn't launch the window)
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

/// Control server response → MCP content blocks.
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
        "team" | "team_task" => {
            let mut p = args.clone();
            if name == "team_task" {
                let a = args["action"].as_str().unwrap_or_default();
                p["action"] = json!(format!("task_{a}"));
            }
            p["client"] = json!(std::env::var("GHOSTDESK_CLIENT_NAME").unwrap_or_else(|_| "외부 에이전트".into()));
            let r = conn.call("team", p).await?;
            match r["text"].as_str() {
                Some(t) => vec![text(t.to_string())],
                None => vec![text(serde_json::to_string_pretty(&r)?)],
            }
        }
        other => bail!("알 수 없는 도구: {other}"),
    };
    Ok(res)
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

/// tools/call requests in flight (request id as JSON string → (sequence, abort handle)).
type InFlight = Arc<PMutex<HashMap<String, (u64, tokio::task::AbortHandle)>>>;

/// Answers anything other than tools/call right away.
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

/// The MCP server loop. Reads one JSON-RPC message per line and sends response lines to `out`.
///
/// Each tools/call runs as its own task, so long calls (waiting for approval, `wait`) don't block
/// ping, tools/list or other tools/call requests. Responses only go out through `out`, so lines
/// never interleave. When input ends, waits for calls in progress to finish before returning.
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
        // JSON-RPC batches aren't supported since MCP 2025-06-18, so answer the whole array with one error
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
            // ignore responses (result/error) to requests we sent; anything else with an id is invalid
            if let Some(id) = id.filter(|_| !obj.contains_key("result") && !obj.contains_key("error")) {
                send(rpc_error(id, -32600, "Invalid Request: method 가 없습니다"));
            }
            continue;
        };
        let Some(id) = id else {
            // notifications get no response; a cancel notification stops the matching call
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
        // spawn while holding the lock, so the task can't finish and fail to remove its entry before it's
        // registered
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
            // a task that received a cancel stops at the await above and never gets here
            let _ = out.send(format!("{resp}\n"));
        });
        guard.insert(key, (my_seq, handle));
        drop(guard);
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}

/// Splits off the real stdout and points fd 1 at stderr, so a `println!` anywhere can't corrupt
/// the JSON-RPC stream.
#[cfg(unix)]
fn take_stdout() -> Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: only duplicates and redirects std fds; the duplicated fd is owned solely by the File
    // created here
    unsafe {
        let fd = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3);
        if fd < 0 {
            bail!("stdout 복제 실패: {}", std::io::Error::last_os_error());
        }
        if libc::dup2(2, 1) < 0 {
            // stderr closed or similar: discard stray output
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

/// Windows: splits off the real stdout as a non-inheritable duplicate (DuplicateHandle) and
/// points the stdout handle at stderr (NUL if there is none).
///
/// Rust's `println!` rereads the stdout handle on every write, so a `println!` anywhere can't
/// corrupt the JSON-RPC stream.
#[cfg(windows)]
fn take_stdout() -> Result<std::fs::File> {
    use std::os::windows::io::{AsHandle, IntoRawHandle};
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
    let owned = std::io::stdout().as_handle().try_clone_to_owned().map_err(|e| anyhow!("stdout 복제 실패: {e}"))?;
    let file = std::fs::File::from(owned);
    // SAFETY: only reads and swaps std handles. The NUL handle serves as stdout until the process
    // exits, so it's deliberately never closed.
    unsafe {
        let err = GetStdHandle(STD_ERROR_HANDLE);
        let target: HANDLE = if !err.is_null() && err != INVALID_HANDLE_VALUE {
            err
        } else {
            // no stderr or similar: discard stray output
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

/// The MCP stdio server.
pub async fn run(_args: &[String]) -> Result<()> {
    // Windows: if the stdio pipes the MCP host passed us are inherited by the GhostDesk window (→
    // browsers, …), the host never sees EOF when this process ends. Turn inheritance off before
    // launching anything.
    #[cfg(windows)]
    crate::platform::win::harden_std_handles();
    let mut stdout = take_stdout()?;
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    // only this thread writes stdout (whole lines, then flush)
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
    // wait until every response from serve is written (tx was dropped along with serve)
    let _ = tokio::task::spawn_blocking(move || writer.join()).await;
    r
}


const SNIPPET_CLI_TITLE: &str = if cfg!(windows) { "Claude Code (터미널: 명령 프롬프트·PowerShell)" } else { "Claude Code (터미널)" };
const SNIPPET_CLAUDE_APP_TITLE: &str = if cfg!(windows) {
    // on Windows the Claude app's config location depends on how it was installed (MSIX, …), so point
    // to the menu that opens it directly
    "Claude 앱 (Settings > Developer > Edit Config 로 여는 claude_desktop_config.json 의 mcpServers 에 추가)"
} else {
    "Claude 앱 (claude_desktop_config.json 의 mcpServers 에 추가)"
};
const SNIPPET_CODEX_TITLE: &str =
    if cfg!(windows) { r"Codex (%USERPROFILE%\.codex\config.toml 에 추가)" } else { "Codex (~/.codex/config.toml 에 추가)" };

/// A TOML string value: "…" as before when there are no quotes or backslashes, '…' (literal)
/// when there are backslashes like Windows paths, and an escaped "…" otherwise.
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

/// Connection instructions (used by the UI and README). `exe` is the GhostDesk executable path.
///
/// The path is escaped per JSON and TOML rules (so Windows `C:\Users\…` paths can be pasted as
/// is).
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
        // the old combination arguments aren't advertised (a single `mode` picks)
        for gone in ["kind", "use_my_login", "app_host", "attach_running", "virtual_display"] {
            assert!(props.get(gone).is_none(), "{gone}");
        }
        let modes = mode_names(&Plat::CURRENT);
        assert_eq!(modes.contains(&"session".into()), caps::SESSION);
        assert_eq!(modes.contains(&"app".into()), caps::ELECTRON || caps::NATIVE_APPS);
        assert_eq!(modes.contains(&"app_vdisplay".into()), caps::VDISPLAY);
        assert!(modes.contains(&"web".into()) && modes.contains(&"web_login".into()) && modes.contains(&"remote".into()));
        assert_eq!(props.get("account").is_some(), caps::SESSION && !cfg!(windows));
        // keep mode names in mcp and control in sync
        for m in mode_names(&Plat::CURRENT).iter().chain(mode_names(&Plat::MAC).iter()) {
            assert!(crate::control::MODES.contains(&m.as_str()), "{m}");
        }
        let mac = mode_names(&Plat::MAC);
        assert!(mac.contains(&"app_vdisplay".into()) && !mac.contains(&"app_current_session".into()));
        let ins = server_instructions();
        // tells agents to pick the mode themselves without asking
        assert!(ins.contains("스스로 골라") && !ins.contains("어떤 모드로 할지") && ins.contains("mode"), "{ins}");
        // if GhostDesk wasn't mentioned but fits the task, ask first whether to use it
        assert!(ins.contains("GhostDesk 로 할까요?") && ins.contains("묻지 마세요"), "{ins}");
        if cfg!(windows) {
            assert!(modes.contains(&"app_current_session".into()) && modes.contains(&"app_running".into()));
            let desc = props["mode"]["description"].as_str().unwrap();
            assert!(desc.contains("자식 세션") && !desc.contains("Mac") && !desc.contains("macOS"), "{desc}");
            assert!(!ins.contains("Mac"), "{ins}");
            assert!(t[1]["description"].as_str().unwrap().contains("'ctrl+a'"));
            assert!(t[3]["description"].as_str().unwrap().contains("Windows 앱:"));
            let app = props["app"]["description"].as_str().unwrap();
            // example app names must be findable by their Start menu shortcut (.lnk) names
            assert!(app.contains(".exe") && app.contains("'Character Map'") && !app.contains("문자표"), "{app}");
            assert!(app.contains("자식 세션") && app.contains("session"), "{app}");
            assert!(ins.contains("메모장") && ins.contains("session"), "{ins}");
        }
        if !cfg!(target_os = "macos") {
            // it also lifts the protection for this computer's own VNC server, so it says to set it only when
            // the user says so
            let d = props["dedicated_machine"]["description"].as_str().unwrap();
            assert!(d.contains("직접 말했을 때만") && d.contains("이 컴퓨터 자신의 VNC 서버") && !d.contains("Mac"), "{d}");
        }
    }

    #[test]
    fn schema_exposes_clone_refresh_profile_and_explicit_dialog_responses() {
        let tools = tools();
        let open = &tools[0]["inputSchema"]["properties"];
        for key in ["workspace", "browser_profile"] { assert_eq!(open[key]["type"], "string", "{key}"); }
        assert_eq!(open["refresh_login"]["type"], "boolean");
        assert!(open["refresh_login"]["description"].as_str().unwrap().contains("교체"));
        for name in ["desk_browser", "desk_app"] {
            let Some(tool) = tools.as_array().unwrap().iter().find(|tool| tool["name"] == name) else { continue };
            let props = &tool["inputSchema"]["properties"];
            assert_eq!(props["id"]["type"], "integer");
            assert_eq!(props["accept"]["type"], "boolean");
            assert_eq!(props["prompt_text"]["type"], "string");
            assert!(props["accept"].get("default").is_none());
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
        // macOS paths are unchanged to the character
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
            // Windows paths become readable TOML literal strings
            let w = setup_snippets(r"C:\Program Files\GhostDesk\ghostdesk.exe");
            assert!(w[2].1.contains(r"command = 'C:\Program Files\GhostDesk\ghostdesk.exe'"), "{}", w[2].1);
            assert!(w[1].1.contains(r#""command": "C:\\Program Files\\GhostDesk\\ghostdesk.exe""#), "{}", w[1].1);
        }
    }

    mod bridge {
        use super::super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::{io::AsyncWriteExt, sync::watch};

        /// A fake control server (standing in for the GhostDesk window). Each request runs as a task, so
        /// responses can arrive out of request order.
        ///
        /// Runs on the same `ipc` endpoint as the real thing (Unix: temp socket, Windows: named pipe).
        struct Fake {
            conns: Arc<AtomicUsize>,
            acts: Arc<AtomicUsize>,
            stop: watch::Sender<bool>,
            /// On Unix the socket file is removed on stop (a named pipe disappears by itself when its last
            /// handle closes).
            #[cfg_attr(not(unix), allow(dead_code))]
            ep: ipc::Endpoint,
        }

        impl Fake {
            async fn start(ep: &ipc::Endpoint) -> Fake {
                #[cfg(unix)]
                let _ = std::fs::remove_file(ep.path());
                // a previously stopped server's pipe instance may still be closing (Windows first-instance
                // check), so retry briefly
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

            /// `start` for use inside the synchronous launcher (mimicking a GUI launch).
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
                                    // receive the request, then drop the connection without answering (GUI exiting mid-run). A named
                                    // pipe can't be half-closed with shutdown, so the connection task drops both halves to close it.
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

            /// Mimics the GUI exiting: drops every connection and removes the endpoint (Unix: deletes the
            /// socket file).
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

        /// A different endpoint per test (Unix: .sock in a temp folder, Windows:
        /// \\.\pipe\gdmcp-…-<SID>).
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
            // control server errors don't drop the connection
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

            // GUI exits → once the bridge notices, the next call must relaunch the window, reconnect and
            // succeed
            first.stop();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(conn.call("act", act("2", 0)).await.unwrap()["text"], "done 2");
            assert_eq!(launches.load(Ordering::SeqCst), 1);
            let second = relaunched.lock().take().unwrap();
            assert_eq!(second.conns.load(Ordering::SeqCst), 1);
            assert_eq!(second.acts.load(Ordering::SeqCst), 1);

            // endpoint alive and only the connection dropped (window still up): reconnect without relaunching
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
            // the next call works on a fresh connection
            assert_eq!(conn.call("act", act("again", 0)).await.unwrap()["text"], "done again");
            assert_eq!(fake.conns.load(Ordering::SeqCst), 2);
        }

        #[tokio::test]
        async fn connect_failure_without_launcher_is_an_error() {
            let conn = Conn::new(test_ep(), None);
            assert!(conn.call("list", json!({})).await.is_err());
        }

        /// Starts serve and returns `(input write handle, response receiver, serve handle)`.
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
            // closing input still lets in-flight calls send their responses before finishing
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
            // notifications and responses sent by the client get no reply
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

    /// Windows: after take_stdout, `println!` goes to stderr and only writes through the split-off
    /// handle reach the real stdout.
    ///
    /// This changes the process's std handles, so it's checked by rerunning this test binary as a
    /// child process.
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

    /// Windows: a detached child (GUI stand-in) launched after turning off std handle inheritance,
    /// as `run()` does, doesn't inherit the host's pipes. So the host gets stdout EOF as soon as the
    /// MCP server ends, even while the child is still alive.
    #[cfg(windows)]
    #[test]
    fn launched_gui_does_not_inherit_host_pipes() {
        use std::process::{Command, Stdio};
        if std::env::var_os("GHOSTDESK_TEST_INHERIT").is_some() {
            crate::platform::win::harden_std_handles();
            let _out = take_stdout().unwrap();
            // a console-less process that lives about 3 s, standing in for the GUI
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

    /// stdio server loop: initialize → (notification) → tools/list → ping. Must answer right away
    /// even without a window.
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
        assert_eq!(names, ["desk_open", "desk_action", "desk_browser", "desk_app", "desk_list", "desk_close", "team", "team_task"]);
        assert_eq!(r["result"]["tools"], tools());
        send_line(&mut w, r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#).await;
        assert_eq!(recv(&mut rx).await["id"], "p");
        drop(w);
        h.await.unwrap().unwrap();
        assert!(rx.recv().await.is_none(), "알림에는 답하지 않는다");
    }

    /// The `ghostdesk --mcp` stdio loop end to end through the real control server (control.rs,
    /// without a GUI).
    ///
    /// The endpoint is the same ipc layer as in production (Unix socket / Windows named pipe).
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
        // until it starts accepting connections
        let t0 = Instant::now();
        while ipc::connect(&ep).await.is_err() {
            assert!(t0.elapsed() < Duration::from_secs(5), "제어 서버가 열리지 않음");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // a second window can't open on the same endpoint
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
            // unsupported kinds error out right away without an approval dialog
            send_line(&mut w, &call(3, "desk_open", json!({ "kind": "session" }))).await;
            let r = recv(&mut rx).await;
            assert_eq!(r["result"]["isError"], true);
            assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains(text::OS_NAME), "{r}");
        }
        #[cfg(windows)]
        {
            // shells are refused by name alone, without an approval dialog
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
