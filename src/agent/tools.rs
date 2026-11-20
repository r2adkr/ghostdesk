//! Executes the model's tool calls: computer actions, per-screen helper tools and `ask_user`.
//! External (MCP) agents go through the same functions.

use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::{
    history::{image_block, Seen},
    settle, wait_while_paused, AgentCmd, AgentShared, AgentStatus, LogKind,
};
use crate::{browser::MouseButton, desk::Desk, keys, llm::short};

pub(crate) struct Outcome {
    pub(crate) text: String,
    pub(crate) image: Option<Vec<u8>>,
}

/// The element number of a page action: a number, or one written as text ("12", "[12]") the way some AI apps send it.
fn element_ref(input: &Value) -> Value {
    match &input["ref"] {
        Value::String(t) => t.trim().trim_matches(['[', ']']).parse::<u64>().map(Value::from).unwrap_or(Value::Null),
        v => v.clone(),
    }
}

/// The element's name of a page action: `label`, or `text` / `name`, which callers often use for the same thing.
fn element_label(input: &Value) -> Option<&str> {
    ["label", "text", "name"].iter().find_map(|k| input[*k].as_str().map(str::trim).filter(|t| !t.is_empty()))
}

fn ok(text: impl Into<String>) -> Result<Outcome> {
    Ok(Outcome { text: text.into(), image: None })
}

pub(crate) async fn exec_remote_tool(shared: &Arc<AgentShared>, input: &Value) -> Result<Outcome> {
    let ssh = shared.ssh.lock().clone().ok_or_else(|| anyhow!("Configure SSH for this remote workspace first"))?;
    let result = match input["action"].as_str().unwrap_or_default() {
        "execute" => {
            let command = input["command"].as_str().ok_or_else(|| anyhow!("command is required"))?;
            let seconds = input.get("timeout_seconds").map(|v| v.as_u64().ok_or_else(|| anyhow!("timeout_seconds must be a positive integer"))).transpose()?.unwrap_or(120);
            let result = ssh.execute(command, seconds, &shared.stop).await?;
            shared.log(LogKind::Info, format!("SSH exit_code: {}\nstdout:\n{}\nstderr:\n{}", result["exit_code"], short(result["stdout"].as_str().unwrap_or_default(),6000), short(result["stderr"].as_str().unwrap_or_default(),6000)));
            if result["success"] != true { return Err(anyhow!("Remote command failed: {result}")); }
            result
        }
        "receive" => {
            let path = input["remote_path"].as_str().ok_or_else(|| anyhow!("remote_path is required"))?;
            let file = ssh.receive(path, &shared.workspace_name, &shared.stop).await?;
            let result = json!({"local_path":file.path,"size":file.size,"remote_path":path});
            shared.log(LogKind::Info, format!("Received: {} ({} bytes)", file.path.display(), file.size));
            shared.received_files.lock().push(file);
            shared.ctx.request_repaint();
            result
        }
        _ => return Err(anyhow!("action must be execute or receive")),
    };
    ok(serde_json::to_string(&result)?)
}

/// Runs one turn's tool calls in order and returns their `tool_result`s.
///
/// The last result gets the current screenshot and window/tab list attached, unless the model
/// has already seen the same thing (`seen`).
pub(crate) async fn execute_batch(
    shared: &Arc<AgentShared>,
    ws: &Desk,
    tool_uses: &[Value],
    rx: &mut mpsc::UnboundedReceiver<AgentCmd>,
    seen: &mut Seen,
    membership: Option<&super::team::Membership>,
) -> Vec<Value> {
    let mut results: Vec<Value> = Vec::new();
    let mut failed = false;
    let mut last_has_image = false;

    for tu in tool_uses {
        let id = tu["id"].as_str().unwrap_or_default().to_string();
        let name = tu["name"].as_str().unwrap_or_default().to_string();
        let toolset = tu["toolset_name"].as_str().map(String::from);
        let input = &tu["input"];

        let mut result = json!({ "type": "tool_result", "tool_use_id": id });
        if let Some(ts) = &toolset {
            result["toolset_name"] = json!(ts);
        }

        if !failed {
            wait_while_paused(shared).await;
        }
        if failed || shared.stop.load(Ordering::SeqCst) || shared.removed.load(Ordering::SeqCst) {
            result["is_error"] = json!(true);
            result["content"] = json!(if failed {
                "Not executed: an earlier action in this turn failed."
            } else {
                "Not executed: the human operator stopped the agent."
            });
            results.push(result);
            last_has_image = false;
            continue;
        }

        let desc = describe(&name, input);
        shared.set_status(AgentStatus::Acting(short(&desc, 60)));
        shared.log(LogKind::Action, desc);

        // tells the on-screen cursor where we're heading so it glides along the same curve as the
        // real movement
        let aim = |p: (f64, f64)| {
            shared.cursor.lock().aim(p, ws.mouse_pos());
            shared.ctx.request_repaint();
        };
        let out = if toolset.is_some() {
            exec_computer(ws, &name, input, &aim).await
        } else {
            exec_custom(shared, ws, &name, input, rx, membership).await
        };
        if toolset.is_some() {
            shared.cursor.lock().record(&name, input, ws.mouse_pos(), false);
            shared.ctx.request_repaint();
        }
        match out {
            Ok(o) => {
                let mut c = vec![json!({ "type": "text", "text": if o.text.is_empty() { "OK".to_string() } else { o.text } })];
                last_has_image = o.image.is_some();
                if let Some(png) = o.image {
                    c = vec![image_block(&png)];
                    seen.forget_screen();
                }
                result["content"] = json!(c);
            }
            Err(e) => {
                shared.log(LogKind::Error, format!("{name} 실패: {e:#}"));
                result["is_error"] = json!(true);
                result["content"] = json!(format!("Error: {e:#}"));
                // A losing claim must not be followed by work from the same batch.
                failed = toolset.is_some() || (name == "team_board" && matches!(input["action"].as_str(), Some("claim" | "claim_files")));
                last_has_image = false;
            }
        }
        results.push(result);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // if the batch didn't end with a screenshot, attach the latest screen and tabs to the last result
    if !ws.has_screen() { return results; }
    settle(ws).await;
    let uses = tool_uses.len();
    let mut extra = Vec::new();
    let views = ws.views_text();
    let mut info = if seen.views(&views, uses) { format!("{} (▶ = current):\n{views}", ws.views_label()) } else { String::new() };
    let notes = ws.take_notes();
    if !notes.is_empty() {
        info.push_str(&format!("Events: {}\n", notes.join(" / ")));
    }
    if !info.is_empty() {
        extra.push(json!({ "type": "text", "text": info }));
    }
    if !last_has_image {
        if let Ok(png) = ws.screenshot().await {
            extra.push(seen.screen(&png, uses));
        }
    }
    if let Some(last) = results.last_mut() {
        let mut c = match last["content"].take() {
            Value::Array(a) => a,
            Value::String(s) => vec![json!({ "type": "text", "text": s })],
            _ => vec![],
        };
        c.extend(extra);
        last["content"] = json!(c);
    }
    results
}

pub(crate) fn coord(v: &Value) -> Option<(f64, f64)> {
    let a = v.as_array()?;
    Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
}

fn mods_from(v: &Value) -> i64 {
    let Some(s) = v.as_str() else { return 0 };
    s.split('+').fold(0, |acc, m| {
        acc | match m.trim().to_ascii_lowercase().as_str() {
            "shift" => keys::MOD_SHIFT,
            "ctrl" | "control" => keys::MOD_CTRL,
            "alt" | "option" => keys::MOD_ALT,
            "super" | "cmd" | "command" | "meta" | "win" => keys::MOD_META,
            _ => 0,
        }
    })
}

/// Glides to the target like a person would, updating the on-screen cursor and sending a
/// stream of real `mouseMoved` events. No-op without coordinates.
async fn glide(ws: &Desk, p: (f64, f64), aim: &(dyn Fn((f64, f64)) + Sync)) -> Result<()> {
    aim(p);
    ws.glide_to(p.0, p.1, 0).await
}

pub(crate) async fn exec_computer(
    ws: &Desk,
    name: &str,
    input: &Value,
    aim: &(dyn Fn((f64, f64)) + Sync),
) -> Result<Outcome> {
    let pos = coord(&input["coordinate"]);
    let mods = mods_from(&input["text"]);
    if let (Some(p), true) = (
        pos,
        matches!(name, "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click" | "mouse_move" | "scroll" | "fill_at"),
    ) {
        glide(ws, p, aim).await?;
    }
    match name {
        "screenshot" => Ok(Outcome { text: String::new(), image: Some(ws.screenshot().await?) }),
        "zoom" => {
            let r = input["region"].as_array().ok_or_else(|| anyhow!("region 누락"))?;
            let v: Vec<f64> = r.iter().filter_map(|x| x.as_f64()).collect();
            anyhow::ensure!(v.len() == 4, "region 은 [x0,y0,x1,y1] 이어야 합니다");
            Ok(Outcome { text: String::new(), image: Some(ws.zoom([v[0], v[1], v[2], v[3]]).await?) })
        }
        "left_click" => { ws.click(pos, MouseButton::Left, 1, mods).await?; ok("") }
        "right_click" => { ws.click(pos, MouseButton::Right, 1, mods).await?; ok("") }
        "middle_click" => { ws.click(pos, MouseButton::Middle, 1, mods).await?; ok("") }
        "double_click" => { ws.click(pos, MouseButton::Left, 2, mods).await?; ok("") }
        "triple_click" => { ws.click(pos, MouseButton::Left, 3, mods).await?; ok("") }
        "left_click_drag" => {
            let from = coord(&input["start_coordinate"]).unwrap_or_else(|| ws.mouse_pos());
            let to = pos.ok_or_else(|| anyhow!("coordinate 누락"))?;
            glide(ws, from, aim).await?;
            aim(to);
            ws.drag(from, to, mods).await?;
            ok("")
        }
        "mouse_move" => {
            let (x, y) = pos.ok_or_else(|| anyhow!("coordinate 누락"))?;
            ws.mouse_move(x, y, 0).await?;
            ok("")
        }
        "left_mouse_down" => { ws.mouse_down(MouseButton::Left, 1, 0).await?; ok("") }
        "left_mouse_up" => { ws.mouse_up(MouseButton::Left, 1, 0).await?; ok("") }
        "cursor_position" => {
            let (x, y) = ws.mouse_pos();
            ok(format!("X={}, Y={}", x.round(), y.round()))
        }
        "scroll" => {
            let amount = input["scroll_amount"].as_f64().unwrap_or(3.0) * 100.0;
            let (dx, dy) = match input["scroll_direction"].as_str().unwrap_or("down") {
                "up" => (0.0, -amount),
                "left" => (-amount, 0.0),
                "right" => (amount, 0.0),
                _ => (0.0, amount),
            };
            ws.scroll(pos, dx, dy, mods).await?;
            tokio::time::sleep(Duration::from_millis(250)).await;
            ok("")
        }
        "type" => {
            ws.type_text(input["text"].as_str().unwrap_or_default()).await?;
            ok("")
        }
        "key" => {
            let k = input["text"].as_str().ok_or_else(|| anyhow!("text 누락"))?;
            let rep = input["repeat"].as_u64().unwrap_or(1) as u32;
            ws.key(k, rep).await?;
            ok("")
        }
        "hold_key" => {
            let k = input["text"].as_str().ok_or_else(|| anyhow!("text 누락"))?;
            ws.hold_key(k, input["duration"].as_f64().unwrap_or(1.0)).await?;
            ok("")
        }
        "wait" => {
            let d = input["duration"].as_f64().unwrap_or(1.0).clamp(0.0, 300.0);
            tokio::time::sleep(Duration::from_secs_f64(d)).await;
            ok("")
        }
        // wait until the picture changes (a page or app finishing what it was doing), instead of guessing a delay
        "wait_change" => {
            let limit = Duration::from_secs_f64(input["duration"].as_f64().unwrap_or(5.0).clamp(0.5, 60.0));
            let started = std::time::Instant::now();
            let digest = |b: &[u8]| {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                b.hash(&mut h);
                h.finish()
            };
            let before = digest(&ws.screenshot().await?);
            loop {
                tokio::time::sleep(Duration::from_millis(300)).await;
                if digest(&ws.screenshot().await?) != before {
                    // let an animation settle before the agent looks
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    return ok(format!("화면이 바뀌었어요 ({:.1}초)", started.elapsed().as_secs_f64()));
                }
                if started.elapsed() >= limit {
                    return ok(format!("{:.0}초 동안 화면이 바뀌지 않았어요", limit.as_secs_f64()));
                }
            }
        }
        // click a field, replace its content, type, optionally press Enter (three clicks select the line)
        "fill_at" => {
            let text = input["text"].as_str().ok_or_else(|| anyhow!("text 누락"))?;
            ws.click(pos, MouseButton::Left, 3, 0).await?;
            tokio::time::sleep(Duration::from_millis(80)).await;
            if text.is_empty() {
                ws.key("Delete", 1).await?;
            } else {
                ws.type_text(text).await?;
            }
            if input["submit"].as_bool().unwrap_or(false) {
                ws.key("Return", 1).await?;
            }
            ok("")
        }
        other => Err(anyhow!("지원하지 않는 동작: {other}")),
    }
}

async fn exec_custom(
    shared: &Arc<AgentShared>,
    desk: &Desk,
    name: &str,
    input: &Value,
    rx: &mut mpsc::UnboundedReceiver<AgentCmd>,
    membership: Option<&super::team::Membership>,
) -> Result<Outcome> {
    match name {
        "ask_user" => {
            let q = input["question"].as_str().unwrap_or("(질문 없음)").to_string();
            shared.log(LogKind::Assistant, format!("{} {q}", crate::icon::CIRCLE_HELP));
            // options arrive as buttons: they ride along as "- option" lines the UI turns back into buttons
            let mut asked = q;
            if let Some(opts) = input["options"].as_array() {
                for o in opts.iter().filter_map(|o| o.as_str()).map(str::trim).filter(|o| !o.is_empty()).take(6) {
                    asked.push_str(&format!("\n- {o}"));
                }
            }
            shared.set_status(AgentStatus::WaitingUser(asked));
            let ans = loop {
                let command = tokio::select! {
                    biased;
                    _ = shared.wait_for_stop() => return Err(anyhow!("질문 대기 중지")),
                    command = rx.recv() => command,
                };
                match command {
                    Some(AgentCmd::Answer(a)) | Some(AgentCmd::Instruction(a)) => break a,
                    // a queue notification is not an answer, keep waiting
                    Some(AgentCmd::Wake) => continue,
                    None => return Err(anyhow!("에이전트 종료")),
                }
            };
            shared.log(LogKind::User, ans.clone());
            shared.set_status(AgentStatus::Thinking);
            ok(format!("Human answered: {ans}"))
        }
        "team_board" => {
            let member = membership.ok_or_else(|| anyhow!("No team was selected for this agent"))?;
            let result = member.execute(input)?;
            shared.ctx.request_repaint();
            ok(result.to_string())
        }
        "remote_control" => exec_remote_tool(shared, input).await,
        other => exec_desk_tool(desk, other, input).await,
    }
}

/// Per-screen helper tools (`browser_control`, `app_control`, `app_menu`). Shared with
/// external (MCP) agents.
/// `find` and name-based targets for app elements: both read the element list (`ui_tree`) and pick
/// the line, so an agent can say `press label="저장"` instead of reading the whole tree for an id.
async fn app_with_names(desk: &Desk, input: &Value) -> Result<Outcome> {
    let action = input["action"].as_str().unwrap_or_default();
    let plain = |s: &str| -> String {
        // drop the pixel rectangle: it is the costliest part of a line and the agent has ids
        match s.rfind(" [") {
            Some(i) if s[i..].ends_with(']') && s[i..].matches(',').count() == 3 => s[..i].to_string(),
            _ => s.to_string(),
        }
    };
    let lines_matching = |tree: &str, q: &str| -> Vec<(usize, String)> {
        let q = q.to_lowercase();
        tree.lines()
            .filter_map(|l| {
                let t = l.trim_start();
                let id: usize = t.split_whitespace().next()?.parse().ok()?;
                t.to_lowercase().contains(&q).then(|| (id, plain(t)))
            })
            .collect()
    };
    let tree_of = |depth: u64| async move {
        let mut i = json!({ "action": "ui_tree" });
        i["max_depth"] = json!(depth);
        app_dispatch(desk, &i).await
    };
    if action == "find" {
        let q = input["query"].as_str().filter(|q| !q.trim().is_empty()).ok_or_else(|| anyhow!("query 가 필요합니다"))?;
        let hits = lines_matching(&tree_of(30).await?.text, q);
        return ok(if hits.is_empty() {
            format!("'{q}' 가 들어간 요소를 못 찾았어요 (창을 바꿨다면 switch_window 뒤에 다시)")
        } else {
            hits.iter().take(15).map(|(_, l)| l.as_str()).collect::<Vec<_>>().join("\n")
        });
    }
    let mut input = input.clone();
    if input["action"] == "click" {
        input["action"] = json!("press");
    }
    if let (Some(label), true) = (input["label"].as_str().map(String::from), input.get("id").is_none_or(Value::is_null)) {
        let hits = lines_matching(&tree_of(30).await?.text, &label);
        // exact quoted name first, then the first that contains it
        let quoted = format!("\"{}\"", label.to_lowercase());
        let pick = hits.iter().find(|(_, l)| l.to_lowercase().contains(&quoted)).or(hits.first());
        let (id, _) = pick.ok_or_else(|| anyhow!("'{label}' 라는 이름의 요소를 못 찾았어요 (find 로 찾아 보세요)"))?;
        input["id"] = json!(id);
        if hits.len() > 1 {
            let mut out = app_dispatch(desk, &input).await?;
            out.text.push_str(&format!(" (같은 이름이 {}개 있어 {}번을 골랐어요)", hits.len(), id));
            return Ok(out);
        }
    }
    app_dispatch(desk, &input).await
}

/// The app tool of whatever kind of screen this is.
async fn app_dispatch(desk: &Desk, input: &Value) -> Result<Outcome> {
    if cfg!(windows) {
        if let Some(session) = desk.as_session() { return ok(session.app_control(input).await?); }
    }
    if let Some(v) = desk.as_vdisplay() {
        return ok(v.app_control(input).await?);
    }
    let apps = if cfg!(target_os = "macos") { "macOS 앱" } else { crate::platform::text::LOCAL_APPS };
    let ws = desk
        .as_native()
        .ok_or_else(|| anyhow!("app_control 은 {apps} 작업공간에서만 쓸 수 있습니다"))?;
    ok(ws.app_control(input).await?)
}

pub(crate) async fn exec_desk_tool(desk: &Desk, name: &str, input: &Value) -> Result<Outcome> {
    match name {
        "app_control" => app_with_names(desk, input).await,
        "app_menu" => {
            let ws = desk
                .as_electron()
                .ok_or_else(|| anyhow!("app_menu 는 Electron 앱 작업공간에서만 쓸 수 있습니다"))?;
            match input["action"].as_str().unwrap_or("list") {
                "dialog" => ok(ws.dialog().await?.to_string()),
                "respond_dialog" => {
                    let id = dialog_id(input)?;
                    let source = input["source"].as_str().ok_or_else(|| anyhow!("source (javascript/native)가 필요합니다"))?;
                    let response = input.get("response").map(|value| value.as_u64()
                        .and_then(|value| usize::try_from(value).ok()).ok_or_else(|| anyhow!("response 는 0 이상의 버튼 번호여야 합니다"))).transpose()?;
                    let accept = input.get("accept").map(|value| value.as_bool().ok_or_else(|| anyhow!("accept 는 true/false 여야 합니다"))).transpose()?;
                    ws.respond_dialog(source, id, response, accept, dialog_prompt(input)?).await?;
                    ok("대화상자에 응답했습니다")
                }
                "click" => {
                    let path = input["path"].as_str().ok_or_else(|| anyhow!("path 가 필요합니다"))?;
                    // block copy/cut/paste menu items for the same reason as the shortcuts: the clipboard is the user's
                    if is_clipboard_menu_path(path) {
                        return Err(anyhow!(
                            "Electron 앱 화면에서는 복사·잘라내기·붙여넣기 메뉴('{path}')를 쓸 수 없습니다: 이 앱은 사용자의 실제 시스템 클립보드를 공유합니다. \
                             텍스트를 넣으려면 `type` 동작을 쓰세요. (Clipboard menu items are blocked on Electron desks; use the `type` action instead.)"
                        ));
                    }
                    ok(ws.menu_click(menu_source(input)?, path).await?)
                }
                "list" => {
                    let t = ws.menu_text(menu_source(input)?).await?;
                    if t.trim().is_empty() {
                        ok("메뉴가 비어 있습니다. 앱이 아직 메뉴를 만들지 않았을 수 있으니 잠시 후(wait 1~2초) 다시 시도하세요. \
                            source=context 는 우클릭 직후에만 내용이 있습니다.")
                    } else {
                        ok(t)
                    }
                }
                action => Err(anyhow!("알 수 없는 action: {action}")),
            }
        }
        "browser_control" => {
            let action = input["action"].as_str().unwrap_or_default();
            // an Electron app's window is a page too: elements can be read and driven, but it must not be navigated
            // away or given tabs
            let ws = match (desk.as_browser(), desk.as_electron()) {
                (Some(b), _) => b,
                (None, Some(e)) if matches!(action, "read" | "text" | "find" | "links" | "click" | "double_click" | "fill" | "select" | "scroll_to" | "wait_for") => e.page(),
                (None, Some(_)) => return Err(anyhow!("Electron 앱에서는 read/text/find/links/click/fill/select/scroll_to/wait_for 만 쓸 수 있습니다 (메뉴는 desk_app)")),
                _ => return Err(anyhow!("browser_control 은 브라우저·Electron 작업공간에서만 쓸 수 있습니다")),
            };
            let url = input["url"].as_str().unwrap_or_default();
            let idx = input["index"].as_u64().unwrap_or(0) as usize;
            match input["action"].as_str().unwrap_or_default() {
                "navigate" => ws.navigate(url).await?,
                "back" => ws.history_go(-1).await?,
                "forward" => ws.history_go(1).await?,
                "reload" => ws.reload().await?,
                "new_tab" => ws.new_tab(url).await?,
                "switch_tab" => ws.switch_tab(idx).await?,
                "close_tab" => ws.close_tab(idx).await?,
                "list_tabs" => return ok(ws.tabs_text()),
                // reading and acting by page elements (a few lines of text instead of a screenshot)
                "read" => {
                    let all = input["scope"].as_str() == Some("page");
                    let max = input["max_items"].as_u64().unwrap_or(if all { 120 } else { 60 }).clamp(5, 300) as usize;
                    return ok(ws.dom_read(all, max).await?);
                }
                "text" => {
                    let max = input["max_chars"].as_u64().unwrap_or(4000).clamp(200, 40_000) as usize;
                    return ok(ws.dom_text(input["selector"].as_str(), max).await?);
                }
                "find" => {
                    let q = input["query"].as_str().filter(|q| !q.trim().is_empty()).ok_or_else(|| anyhow!("query 가 필요합니다"))?;
                    return ok(ws.dom_find(q).await?);
                }
                "links" => return ok(ws.dom_links(input["max_items"].as_u64().unwrap_or(40).clamp(5, 200) as usize).await?),
                "click" | "double_click" => {
                    let (r, l) = (&element_ref(input), element_label(input));
                    return ok(ws.dom_click(r, l, input["action"] == "double_click").await?);
                }
                "fill" => {
                    let value = input["value"].as_str().ok_or_else(|| anyhow!("value 가 필요합니다"))?;
                    return ok(ws.dom_fill(&element_ref(input), element_label(input), value, input["submit"].as_bool().unwrap_or(false)).await?);
                }
                "select" => {
                    let option = input["option"].as_str().ok_or_else(|| anyhow!("option 이 필요합니다"))?;
                    return ok(ws.dom_select(&element_ref(input), element_label(input), option).await?);
                }
                "scroll_to" => return ok(ws.dom_scroll_to(&element_ref(input), element_label(input)).await?),
                "wait_for" => {
                    let t = input["text"].as_str().filter(|t| !t.is_empty()).ok_or_else(|| anyhow!("text 가 필요합니다"))?;
                    return ok(ws.dom_wait_for(t, input["gone"].as_bool().unwrap_or(false), input["timeout"].as_f64().unwrap_or(10.0)).await?);
                }
                "dialog" => return ok(ws.dialog().unwrap_or(Value::Null).to_string()),
                "respond_dialog" => {
                    let accept = input["accept"].as_bool().ok_or_else(|| anyhow!("accept (true/false)를 명시해야 합니다"))?;
                    ws.respond_dialog(dialog_id(input)?, accept, dialog_prompt(input)?).await?;
                    return ok("대화상자에 응답했습니다");
                }
                a => return Err(anyhow!("알 수 없는 action: {a}")),
            }
            let t = ws.active_tab().map(|t| format!("현재 탭: {} — {}", t.title, t.url)).unwrap_or_default();
            ok(t)
        }
        other => Err(anyhow!("알 수 없는 도구: {other}")),
    }
}

fn dialog_id(input: &Value) -> Result<u64> {
    input["id"].as_u64().filter(|id| *id > 0).ok_or_else(|| anyhow!("dialog 에서 확인한 id (1 이상의 정수)가 필요합니다"))
}

fn menu_source(input: &Value) -> Result<crate::electron::MenuSource> {
    use crate::electron::MenuSource;
    match input.get("source") {
        None => Ok(MenuSource::App),
        Some(Value::String(source)) if source == "app" => Ok(MenuSource::App),
        Some(Value::String(source)) if source == "context" => Ok(MenuSource::Context),
        _ => Err(anyhow!("메뉴 list/click 의 source 는 app / context 중 하나입니다. 대화상자는 dialog/respond_dialog 를 쓰세요.")),
    }
}

fn dialog_prompt(input: &Value) -> Result<Option<&str>> {
    input.get("prompt_text").map(|value| value.as_str().ok_or_else(|| anyhow!("prompt_text 는 문자열이어야 합니다"))).transpose()
}

/// Whether a menu path ("Edit > Paste") names copy, cut or paste. Matches whole words, so
/// "Shortcuts" doesn't count.
fn is_clipboard_menu_path(path: &str) -> bool {
    path.split('>').any(|seg| {
        let l = seg.trim().to_lowercase();
        l.split(|c: char| !c.is_alphanumeric()).any(|w| matches!(w, "copy" | "cut" | "paste"))
            || ["붙여넣", "복사", "오려", "잘라내"].iter().any(|k| l.contains(k))
    })
}

pub(crate) fn describe(name: &str, input: &Value) -> String {
    match name {
        "browser_control" => format!(
            "browser_control {} {}",
            input["action"].as_str().unwrap_or(""),
            input["url"].as_str().map(String::from).or(input["index"].as_u64().map(|i| i.to_string())).unwrap_or_default()
        ),
        "ask_user" => "ask_user".into(),
        "team_board" => format!("team_board {}", input["action"].as_str().unwrap_or_default()),
        "app_control" | "app_menu" => format!(
            "{name} {} {}",
            input["action"].as_str().unwrap_or(""),
            input["path"].as_str().map(String::from)
                .or(input["id"].as_u64().map(|i| format!("#{i}")))
                .or(input["index"].as_u64().map(|i| i.to_string()))
                .unwrap_or_default()
        ),
        "type" => format!("type \"{}\"", short(input["text"].as_str().unwrap_or(""), 60)),
        _ => {
            let s = input.to_string();
            if s == "{}" { name.to_string() } else { format!("{name} {}", short(&s, 80)) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_actions_accept_the_ref_as_text_and_the_name_as_text() {
        assert_eq!(element_ref(&json!({"ref": 12})), json!(12));
        assert_eq!(element_ref(&json!({"ref": "12"})), json!(12));
        assert_eq!(element_ref(&json!({"ref": "[12]"})), json!(12));
        assert_eq!(element_ref(&json!({"ref": "abc"})), Value::Null);
        assert_eq!(element_ref(&json!({})), Value::Null);
        assert_eq!(element_label(&json!({"label": "검색"})), Some("검색"));
        assert_eq!(element_label(&json!({"text": " 검색 "})), Some("검색"));
        assert_eq!(element_label(&json!({"label": "", "name": "x"})), Some("x"));
        assert_eq!(element_label(&json!({})), None);
    }

    #[test]
    fn menu_sources_never_fall_back_from_dialog_or_invalid_sources() {
        use crate::electron::MenuSource;
        assert!(matches!(menu_source(&json!({})).unwrap(), MenuSource::App));
        assert!(matches!(menu_source(&json!({"source":"app"})).unwrap(), MenuSource::App));
        assert!(matches!(menu_source(&json!({"source":"context"})).unwrap(), MenuSource::Context));
        for source in [json!("native"), json!("javascript"), json!("other"), json!(42), Value::Null] {
            assert!(menu_source(&json!({"source":source})).is_err(), "{source}");
        }
    }

    #[test]
    fn dialog_identifiers_and_prompt_text_do_not_silently_coerce() {
        for input in [json!({}), json!({"id":0}), json!({"id":-1}), json!({"id":"1"}), json!({"id":true})] {
            assert!(dialog_id(&input).is_err(), "{input}");
        }
        assert_eq!(dialog_id(&json!({"id":3})).unwrap(), 3);
        assert_eq!(dialog_prompt(&json!({})).unwrap(), None);
        let input = json!({"prompt_text":"재용👋"});
        assert_eq!(dialog_prompt(&input).unwrap(), Some("재용👋"));
        assert!(dialog_prompt(&json!({"prompt_text":false})).is_err());
    }

    #[test]
    fn clipboard_menu_paths() {
        for p in ["Edit > Paste", "Edit > Copy", "Edit>Cut", "편집 > 붙여넣기", "편집 > 복사", "Edit > Paste and Match Style", "Copy Path"] {
            assert!(is_clipboard_menu_path(p), "{p}");
        }
        for p in ["File > New Text File", "Code > Settings > Keyboard Shortcuts", "Edit > Undo", "Run > Execute"] {
            assert!(!is_clipboard_menu_path(p), "{p}");
        }
    }
}
