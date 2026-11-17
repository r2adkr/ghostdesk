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

fn ok(text: impl Into<String>) -> Result<Outcome> {
    Ok(Outcome { text: text.into(), image: None })
}

/// Runs one turn's tool calls in order and returns their `tool_result`s.
///
/// The last result gets the current screenshot and window/tab list attached, unless the model
/// has already seen the same thing (`seen`).
pub(super) async fn execute_batch(
    shared: &Arc<AgentShared>,
    ws: &Desk,
    tool_uses: &[Value],
    rx: &mut mpsc::UnboundedReceiver<AgentCmd>,
    seen: &mut Seen,
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

        if failed || shared.stop.load(Ordering::SeqCst) {
            result["is_error"] = json!(true);
            result["content"] = json!(if failed {
                "Not executed: an earlier computer action in this turn failed."
            } else {
                "Not executed: the human operator stopped the agent."
            });
            results.push(result);
            last_has_image = false;
            continue;
        }

        wait_while_paused(shared).await;
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
            exec_custom(shared, ws, &name, input, rx).await
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
                failed = toolset.is_some();
                last_has_image = false;
            }
        }
        results.push(result);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // if the batch didn't end with a screenshot, attach the latest screen and tabs to the last result
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
        matches!(name, "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click" | "mouse_move" | "scroll"),
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
        other => Err(anyhow!("지원하지 않는 동작: {other}")),
    }
}

async fn exec_custom(
    shared: &Arc<AgentShared>,
    desk: &Desk,
    name: &str,
    input: &Value,
    rx: &mut mpsc::UnboundedReceiver<AgentCmd>,
) -> Result<Outcome> {
    match name {
        "ask_user" => {
            let q = input["question"].as_str().unwrap_or("(질문 없음)").to_string();
            shared.log(LogKind::Assistant, format!("{} {q}", crate::icon::CIRCLE_HELP));
            shared.set_status(AgentStatus::WaitingUser(q));
            let ans = loop {
                match rx.recv().await {
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
        other => exec_desk_tool(desk, other, input).await,
    }
}

/// Per-screen helper tools (`browser_control`, `app_control`, `app_menu`). Shared with
/// external (MCP) agents.
pub(crate) async fn exec_desk_tool(desk: &Desk, name: &str, input: &Value) -> Result<Outcome> {
    match name {
        "app_control" => {
            if let Some(v) = desk.as_vdisplay() {
                return ok(v.app_control(input).await?);
            }
            let apps = if cfg!(target_os = "macos") { "macOS 앱" } else { crate::platform::text::LOCAL_APPS };
            let ws = desk
                .as_native()
                .ok_or_else(|| anyhow!("app_control 은 {apps} 작업공간에서만 쓸 수 있습니다"))?;
            ok(ws.app_control(input).await?)
        }
        "app_menu" => {
            use crate::electron::MenuSource;
            let ws = desk
                .as_electron()
                .ok_or_else(|| anyhow!("app_menu 는 Electron 앱 작업공간에서만 쓸 수 있습니다"))?;
            let src = if input["source"].as_str() == Some("context") { MenuSource::Context } else { MenuSource::App };
            match input["action"].as_str().unwrap_or("list") {
                "click" => {
                    let path = input["path"].as_str().ok_or_else(|| anyhow!("path 가 필요합니다"))?;
                    // block copy/cut/paste menu items for the same reason as the shortcuts: the clipboard is the user's
                    if is_clipboard_menu_path(path) {
                        return Err(anyhow!(
                            "Electron 앱 화면에서는 복사·잘라내기·붙여넣기 메뉴('{path}')를 쓸 수 없습니다: 이 앱은 사용자의 실제 시스템 클립보드를 공유합니다. \
                             텍스트를 넣으려면 `type` 동작을 쓰세요. (Clipboard menu items are blocked on Electron desks; use the `type` action instead.)"
                        ));
                    }
                    ok(ws.menu_click(src, path).await?)
                }
                _ => {
                    let t = ws.menu_text(src).await?;
                    if t.trim().is_empty() {
                        ok("메뉴가 비어 있습니다. 앱이 아직 메뉴를 만들지 않았을 수 있으니 잠시 후(wait 1~2초) 다시 시도하세요. \
                            source=context 는 우클릭 직후에만 내용이 있습니다.")
                    } else {
                        ok(t)
                    }
                }
            }
        }
        "browser_control" => {
            let ws = desk
                .as_browser()
                .ok_or_else(|| anyhow!("browser_control 은 브라우저 작업공간에서만 쓸 수 있습니다"))?;
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
                a => return Err(anyhow!("알 수 없는 action: {a}")),
            }
            let t = ws.active_tab().map(|t| format!("현재 탭: {} — {}", t.title, t.url)).unwrap_or_default();
            ok(t)
        }
        other => Err(anyhow!("알 수 없는 도구: {other}")),
    }
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
    fn clipboard_menu_paths() {
        for p in ["Edit > Paste", "Edit > Copy", "Edit>Cut", "편집 > 붙여넣기", "편집 > 복사", "Edit > Paste and Match Style", "Copy Path"] {
            assert!(is_clipboard_menu_path(p), "{p}");
        }
        for p in ["File > New Text File", "Code > Settings > Keyboard Shortcuts", "Edit > Undo", "Run > Execute"] {
            assert!(!is_clipboard_menu_path(p), "{p}");
        }
    }
}
