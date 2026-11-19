//! Starting from the home chat: the person only writes the task, and the AI decides where to work
//! (a website in a hidden browser, or an app on this computer). The keyword guess in
//! `wizard::auto_pick` is the fallback when the AI can't be reached.

use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{json, Value};

use super::wizard::AutoPick;
use super::*;

type Slot = Arc<Mutex<Option<Option<String>>>>;

/// A decision in progress: the AI's answer (None inside = failed) arrives in the slot.
pub(super) struct Routing {
    slot: Slot,
    /// The app list the answer is matched against (label, path, electron index)
    apps: Vec<(String, String, Option<usize>)>,
    started: Instant,
}

const SYSTEM: &str = "You decide where a computer-use agent should work for a task. Answer with one JSON object and nothing else: \
{\"mode\":\"web\"|\"app\",\"app\":\"<exact name from the list, or empty>\"}. \
Choose \"web\" (a hidden browser) for anything that can be done on a website: searching, reading, shopping, booking, mail, calendars, \
documents or chat through their websites, even when a desktop app of the same service exists. \
Choose \"app\" only when the task clearly needs a program installed on this computer: the person names the app, or it works on local files \
or a native feature a website cannot offer. When unsure choose \"web\". Never invent an app: \"app\" must be copied exactly from the list.";

impl GhostApp {
    /// Starts deciding where the task in the home chat should run. Returns false when the AI route
    /// isn't usable, so the caller opens the normal dialog instead.
    pub(super) fn begin_route(&mut self, ctx: &egui::Context) -> bool {
        if self.form.task.trim().is_empty() || self.missing_setup().is_some() || self.routing.is_some() {
            return false;
        }
        let Some(llm) = self.cur().map(|c| c.llm("low")) else { return false };
        let electrons = self.electron_apps.lock().clone().unwrap_or_default();
        let natives = self.native_apps.lock().clone().unwrap_or_default();
        let items = wizard::app_list_items(&electrons, &natives, false);
        let usable: Vec<_> = items.iter().filter(|it| it.disabled.is_none()).take(200).collect();
        let list = usable.iter().map(|it| it.label.trim().trim_end_matches(".app").to_string()).collect::<Vec<_>>().join(", ");
        let system = format!("{SYSTEM}\nInstalled apps: {list}");
        let task = self.form.task.trim().to_string();
        let slot: Slot = Arc::new(Mutex::new(None));
        let (s2, ctx2) = (slot.clone(), ctx.clone());
        self.rt.spawn(async move {
            let msg = json!([{ "role": "user", "content": [{ "type": "text", "text": format!("Task: {task}") }] }]);
            let r = tokio::time::timeout(Duration::from_secs(40), llm.send(&system, &json!([]), &[msg[0].clone()])).await;
            let text = match r {
                Ok(Ok(reply)) => reply_text(&reply),
                _ => None,
            };
            *s2.lock() = Some(text);
            ctx2.request_repaint();
        });
        self.routing = Some(Routing {
            slot,
            apps: usable.iter().map(|it| (it.label.trim().trim_end_matches(".app").to_string(), it.path.clone(), it.electron)).collect(),
            started: Instant::now(),
        });
        true
    }

    /// Called every frame: when the AI has answered (or failed), starts the agent where it said.
    pub(super) fn poll_route(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.setup_rx {
            match rx.try_recv() {
                Ok(res) => {
                    self.setup_rx = None;
                    let msg = match res {
                        Ok(()) => tr("세션 설정을 적용했어요").to_string(),
                        Err(e) => e,
                    };
                    self.toast = Some((msg, ctx.input(|i| i.time) + 6.0));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(300)),
                Err(_) => self.setup_rx = None,
            }
        }
        let Some(r) = &self.routing else { return };
        let answer = r.slot.lock().take();
        let Some(answer) = answer else {
            ctx.request_repaint_after(Duration::from_millis(200));
            return;
        };
        let r = self.routing.take().expect("checked above");
        // the keyword guess still decides what the AI can't: Windows Store apps (never in the list)
        let guess = self.auto_pick();
        let pick = match (&guess, answer.as_deref().and_then(parse_answer)) {
            (AutoPick::ChildSession { .. }, _) => guess,
            (_, Some((true, name))) => match find_app(&r.apps, &name) {
                Some((label, path, electron)) => AutoPick::App { label, path, electron },
                None => AutoPick::Web,
            },
            (_, Some((false, _))) => AutoPick::Web,
            (_, None) => guess,
        };
        let _ = r.started;
        self.apply_pick(pick);
        let before = self.agents.len();
        self.start_from_wizard(ctx);
        if self.agents.len() == before {
            // something needs the person's attention (a name clash, a missing app): show the dialog with everything filled in
            self.wizard_open = true;
        }
    }

    pub(super) fn routing_active(&self) -> bool {
        self.routing.is_some()
    }
}

/// The model's text of a reply in Claude format. CLI models answer in `message`; others in text blocks.
fn reply_text(reply: &Value) -> Option<String> {
    let t: String = reply["content"].as_array()?.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
    (!t.trim().is_empty()).then_some(t)
}

/// `(is_app, app name)` from the first JSON object in the answer.
fn parse_answer(text: &str) -> Option<(bool, String)> {
    let (a, b) = (text.find('{')?, text.rfind('}')?);
    let v: Value = serde_json::from_str(text.get(a..=b)?).ok()?;
    match v["mode"].as_str()?.to_lowercase().as_str() {
        "app" => Some((true, v["app"].as_str().unwrap_or_default().trim().to_string())),
        "web" => Some((false, String::new())),
        _ => None,
    }
}

fn find_app(apps: &[(String, String, Option<usize>)], name: &str) -> Option<(String, String, Option<usize>)> {
    let n = name.trim().to_lowercase();
    if n.is_empty() {
        return None;
    }
    apps.iter().find(|(l, ..)| l.to_lowercase() == n).or_else(|| apps.iter().find(|(l, ..)| l.to_lowercase().contains(&n) || n.contains(&l.to_lowercase()))).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_read_loosely_but_never_invent_apps() {
        assert_eq!(parse_answer("{\"mode\":\"web\",\"app\":\"\"}"), Some((false, String::new())));
        assert_eq!(parse_answer("Sure! ```json\n{\"mode\": \"App\", \"app\": \"Notes\"}\n```"), Some((true, "Notes".into())));
        assert_eq!(parse_answer("no idea"), None);
        assert_eq!(parse_answer("{\"mode\":\"desktop\"}"), None);
        let apps = vec![("Notes".to_string(), "/x/Notes.app".to_string(), None), ("Visual Studio Code".to_string(), "/y".to_string(), Some(1))];
        assert_eq!(find_app(&apps, "notes").map(|a| a.1), Some("/x/Notes.app".into()));
        assert_eq!(find_app(&apps, "Photoshop"), None);
        assert_eq!(find_app(&apps, ""), None);
    }
}
