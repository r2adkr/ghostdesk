//! Settings → Tool box: the outside tools (MCP servers) key-based agents can use. Added, switched on and off and
//! removed here; the person approves exactly what will run before it is added, and agents ask again the first
//! time each tool is used.
use super::*;
use crate::skillbox::{self, Candidate, Skill};
use crate::toolbox::{self, Server, Status};
use std::sync::mpsc::{Receiver, TryRecvError};

#[derive(Default)]
pub(super) struct State {
    servers: Vec<Server>,
    loaded: bool,
    status: Vec<Status>,
    probing: Option<Receiver<Result<Vec<Status>, String>>>,
    note: Option<String>,
    adding: bool,
    name: String,
    line: String,
    env: String,
    /// The server waiting for the person's go-ahead (what will run is shown).
    confirm: Option<Server>,
    skills: Vec<Skill>,
    skills_loaded: bool,
    candidates: Vec<Candidate>,
    import_path: String,
    importing: bool,
    skill_confirm: Option<(Candidate, usize, u64)>,
    skill_note: Option<String>,
}

/// Environment lines `KEY=VALUE` (blank and malformed lines are skipped).
fn parse_env(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            let k = k.trim();
            (!k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).then(|| (k.to_string(), v.trim().to_string()))
        })
        .collect()
}

/// The server a form describes: a web address, or a program with its arguments.
fn from_form(name: &str, line: &str, env: &str) -> Server {
    let line = line.trim();
    let (command, args, url) = if line.starts_with("http://") || line.starts_with("https://") {
        (String::new(), Vec::new(), line.to_string())
    } else {
        let mut parts = toolbox::split_command(line);
        let command = if parts.is_empty() { String::new() } else { parts.remove(0) };
        (command, parts, String::new())
    };
    Server { name: name.trim().to_string(), command, args, url, env: parse_env(env), enabled: true, exposure: toolbox::DEFERRED.into() }
}

impl GhostApp {
    pub(super) fn settings_toolbox(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if !self.toolbox.loaded {
            self.toolbox.servers = toolbox::load();
            self.toolbox.loaded = true;
        }
        if let Some(rx) = &self.toolbox.probing {
            match rx.try_recv() {
                Ok(r) => {
                    self.toolbox.probing = None;
                    match r {
                        Ok(s) => self.toolbox.status = s,
                        Err(e) => self.toolbox.note = Some(e),
                    }
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(300)),
                Err(_) => self.toolbox.probing = None,
            }
        }
        let installed = crate::pi::find().is_some();
        ui.label(theme::hint(tr("API 키로 쓰는 AI 에이전트가 쓸 수 있는 외부 도구예요. 도구는 처음 쓸 때마다 내게 허락을 물어요.")));
        if !installed {
            ui.add_space(6.0);
            ui.label(theme::hint(tr("이 기능을 쓰려면 먼저 AI 탭에서 설치해 주세요")));
        }
        ui.add_space(10.0);

        let mut changed = false;
        let mut remove: Option<usize> = None;
        for (i, s) in self.toolbox.servers.iter_mut().enumerate() {
            let status = self.toolbox.status.iter().find(|st| st.name == s.name);
            theme::soft_card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&s.name).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::icon_button(ui, icon::X, tr("목록에서 지우기")).clicked() {
                            remove = Some(i);
                        }
                        changed |= ui.add(theme::switch(&mut s.enabled, "")).changed();
                    });
                });
                ui.label(RichText::new(crate::llm::short(&s.runs(), 120)).monospace().small().color(theme::muted()));
                let (text, color) = match (s.enabled, status) {
                    (false, _) => (tr("꺼짐").to_string(), theme::muted()),
                    (true, Some(st)) if st.state == "connected" => (trf("연결됨 · 도구 {}개", &[&st.tools.len()]), theme::success()),
                    (true, Some(st)) => (trf("연결 실패: {}", &[&st.error.clone().unwrap_or_else(|| st.state.clone())]), theme::danger()),
                    (true, None) => (tr("아직 확인하지 않았어요").to_string(), theme::muted()),
                };
                ui.label(RichText::new(text).small().color(color));
                ui.horizontal(|ui| {
                    ui.label(theme::hint(tr("표시 방식")));
                    egui::ComboBox::from_id_salt(("tb_exposure", i))
                        .icon(theme::combo_icon)
                        .selected_text(if s.exposure == toolbox::DIRECT { tr("항상 보이기") } else { tr("필요할 때 찾기") })
                        .show_ui(ui, |ui| {
                            changed |= ui.selectable_value(&mut s.exposure, toolbox::DEFERRED.to_string(), tr("필요할 때 찾기")).changed();
                            changed |= ui.selectable_value(&mut s.exposure, toolbox::DIRECT.to_string(), tr("항상 보이기")).changed();
                        })
                        .response
                        .on_hover_text(tr("'필요할 때 찾기'는 AI 가 필요할 때만 도구를 불러와서 토큰을 아껴요"));
                });
            });
            ui.add_space(6.0);
        }
        if let Some(i) = remove {
            self.toolbox.servers.remove(i);
            changed = true;
        }
        if changed {
            if let Err(e) = toolbox::save(&self.toolbox.servers) {
                self.toolbox.note = Some(trf("저장하지 못했어요 ({})", &[&e]));
            }
            self.toolbox.status.clear();
        }
        if self.toolbox.servers.is_empty() && !self.toolbox.adding {
            ui.label(theme::hint(tr("아직 추가한 도구가 없어요")));
            ui.add_space(6.0);
        }

        ui.horizontal(|ui| {
            if !self.toolbox.adding && theme::secondary(ui, tr("도구 추가")).clicked() {
                self.toolbox.adding = true;
            }
            let busy = self.toolbox.probing.is_some();
            let can = installed && !busy && self.toolbox.servers.iter().any(|s| s.enabled);
            if ui.add_enabled(can, egui::Button::new(tr("연결 확인"))).on_hover_text(tr("켜진 도구를 실제로 실행해서 연결되는지 확인해요")).clicked() {
                let (tx, rx) = std::sync::mpsc::channel();
                self.toolbox.probing = Some(rx);
                self.toolbox.note = None;
                std::thread::spawn(move || {
                    let _ = tx.send(toolbox::probe());
                });
            }
            if busy {
                ui.spinner();
            }
        });
        if let Some(n) = &self.toolbox.note {
            ui.label(RichText::new(n.clone()).small().color(theme::danger()));
        }

        if self.toolbox.adding {
            ui.add_space(10.0);
            ui.separator();
            ui.add_space(8.0);
            theme::section(ui, tr("도구 추가"));
            ui.add_space(4.0);
            let w = ui.available_width();
            ui.add(egui::TextEdit::singleline(&mut self.toolbox.name).desired_width(w).margin(Margin::symmetric(10, 6)).hint_text(tr("이름 (예: files)")));
            ui.add_space(4.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.toolbox.line)
                    .desired_width(w)
                    .margin(Margin::symmetric(10, 6))
                    .hint_text(tr("실행 명령 또는 https:// 주소 (예: npx -y @modelcontextprotocol/server-filesystem .)")),
            );
            ui.add_space(4.0);
            ui.add(
                egui::TextEdit::multiline(&mut self.toolbox.env)
                    .desired_rows(2)
                    .desired_width(w)
                    .margin(Margin::symmetric(10, 6))
                    .hint_text(tr("환경 변수 (한 줄에 하나: 이름=값, 없으면 비워 두세요)")),
            );
            ui.add_space(6.0);
            let mut cancel = false;
            if let Some(c) = self.toolbox.confirm.clone() {
                theme::tinted(theme::warn()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(if c.url.is_empty() { tr("이 컴퓨터에서 아래 명령이 실행돼요") } else { tr("아래 주소와 연결해요") }).strong());
                    ui.label(RichText::new(c.runs()).monospace());
                    ui.label(theme::hint(tr("믿을 수 있는 곳의 도구만 추가하세요. 환경 변수는 이 컴퓨터에만 저장돼요.")));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if theme::primary(ui, tr("허락하고 추가")).clicked() {
                            self.toolbox.servers.push(c.clone());
                            match toolbox::save(&self.toolbox.servers) {
                                Ok(()) => {
                                    self.toolbox.status.clear();
                                    self.toolbox.name.clear();
                                    self.toolbox.line.clear();
                                    self.toolbox.env.clear();
                                    self.toolbox.adding = false;
                                }
                                Err(e) => {
                                    self.toolbox.servers.pop();
                                    self.toolbox.note = Some(trf("저장하지 못했어요 ({})", &[&e]));
                                }
                            }
                            self.toolbox.confirm = None;
                        }
                        if theme::secondary(ui, tr("취소")).clicked() {
                            self.toolbox.confirm = None;
                        }
                    });
                });
            } else {
                ui.horizontal(|ui| {
                    if theme::primary(ui, tr("확인")).clicked() {
                        let s = from_form(&self.toolbox.name, &self.toolbox.line, &self.toolbox.env);
                        match toolbox::check(&self.toolbox.servers, &s) {
                            Ok(()) => {
                                self.toolbox.note = None;
                                self.toolbox.confirm = Some(s);
                            }
                            Err(why) => self.toolbox.note = Some(tr(why).to_string()),
                        }
                    }
                    cancel = theme::secondary(ui, tr("취소")).clicked();
                });
            }
            if cancel {
                self.toolbox.adding = false;
                self.toolbox.note = None;
            }
        }
        ui.add_space(18.0);
        self.skills_section(ui);
    }
}

impl GhostApp {
    fn reload_skills(&mut self) {
        self.toolbox.skills = skillbox::list();
        self.toolbox.candidates = skillbox::candidates();
        self.toolbox.skills_loaded = true;
    }

    /// Skills: imported folders agents can read when a task matches.
    fn skills_section(&mut self, ui: &mut egui::Ui) {
        if !self.toolbox.skills_loaded {
            self.reload_skills();
        }
        theme::section(ui, tr("스킬"));
        ui.add_space(4.0);
        ui.label(theme::hint(tr("스킬은 특정 작업을 잘 하게 해 주는 안내서예요. 가져온 스킬은 이 앱 안에 복사되어 따로 보관돼요.")));
        ui.add_space(8.0);
        let mut changed = false;
        let mut remove: Option<usize> = None;
        let mut toggle: Option<(usize, bool)> = None;
        for (i, s) in self.toolbox.skills.iter().enumerate() {
            theme::soft_card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&s.name).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::icon_button(ui, icon::X, tr("지우기")).clicked() {
                            remove = Some(i);
                        }
                        let mut on = s.enabled;
                        if ui.add(theme::switch(&mut on, "")).changed() {
                            toggle = Some((i, on));
                        }
                    });
                });
                ui.label(theme::hint(crate::llm::short(&s.description, 160)));
            });
            ui.add_space(6.0);
        }
        if let Some((i, on)) = toggle {
            if let Err(e) = skillbox::set_enabled(&self.toolbox.skills[i], on) {
                self.toolbox.skill_note = Some(trf("저장하지 못했어요 ({})", &[&e]));
            }
            changed = true;
        }
        if let Some(i) = remove {
            if let Err(e) = skillbox::remove(&self.toolbox.skills[i]) {
                self.toolbox.skill_note = Some(trf("저장하지 못했어요 ({})", &[&e]));
            }
            changed = true;
        }
        if changed {
            self.reload_skills();
        }
        if self.toolbox.skills.is_empty() {
            ui.label(theme::hint(tr("아직 가져온 스킬이 없어요")));
            ui.add_space(6.0);
        }
        if let Some(n) = &self.toolbox.skill_note {
            ui.label(RichText::new(n.clone()).small().color(theme::danger()));
        }
        if let Some((c, files, bytes)) = self.toolbox.skill_confirm.clone() {
            theme::tinted(theme::warn()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(trf("'{}' 스킬을 가져올까요?", &[&c.name])).strong());
                ui.label(theme::hint(crate::llm::short(&c.description, 160)));
                ui.label(theme::hint(trf("파일 {}개 · {}KB. 스킬에 든 스크립트는 에이전트가 실행하려 할 때 내게 허락을 물어요.", &[&files, &(bytes / 1024)])));
                ui.horizontal(|ui| {
                    if theme::primary(ui, tr("가져오기")).clicked() {
                        match skillbox::import(&c.path) {
                            Ok(_) => {
                                self.toolbox.skill_note = None;
                                self.toolbox.import_path.clear();
                                self.reload_skills();
                            }
                            Err(e) => self.toolbox.skill_note = Some(tr(&e).to_string()),
                        }
                        self.toolbox.skill_confirm = None;
                    }
                    if theme::secondary(ui, tr("취소")).clicked() {
                        self.toolbox.skill_confirm = None;
                    }
                });
            });
            return;
        }
        let mut pick: Option<std::path::PathBuf> = None;
        if !self.toolbox.candidates.is_empty() {
            ui.label(theme::hint(tr("이 컴퓨터에서 찾은 스킬")));
            for c in &self.toolbox.candidates {
                ui.horizontal(|ui| {
                    if theme::secondary(ui, tr("가져오기")).clicked() {
                        pick = Some(c.path.clone());
                    }
                    ui.label(RichText::new(&c.name).strong()).on_hover_text(crate::llm::short(&c.description, 200));
                });
            }
            ui.add_space(6.0);
        }
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 90.0).max(120.0);
            ui.add(egui::TextEdit::singleline(&mut self.toolbox.import_path).desired_width(w).margin(Margin::symmetric(10, 6)).hint_text(tr("스킬 폴더 경로 (SKILL.md 가 든 폴더)")));
            if theme::secondary(ui, tr("확인")).clicked() && !self.toolbox.import_path.trim().is_empty() {
                pick = Some(std::path::PathBuf::from(self.toolbox.import_path.trim()));
            }
        });
        if let Some(path) = pick {
            match skillbox::inspect(&path) {
                Ok(found) => {
                    self.toolbox.skill_note = None;
                    self.toolbox.skill_confirm = Some(found);
                }
                Err(e) => self.toolbox.skill_note = Some(tr(&e).to_string()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_builds_a_program_or_a_web_server() {
        let p = from_form(" files ", "npx -y pkg \"a b\"", "A=1\nbad line\n=x\nB = two");
        assert_eq!((p.name.as_str(), p.command.as_str()), ("files", "npx"));
        assert_eq!(p.args, ["-y", "pkg", "a b"]);
        assert_eq!(p.env, [("A".to_string(), "1".to_string()), ("B".to_string(), "two".to_string())]);
        let w = from_form("docs", "https://example.com/mcp", "");
        assert_eq!((w.url.as_str(), w.command.as_str()), ("https://example.com/mcp", ""));
        assert!(w.enabled && w.exposure == toolbox::DEFERRED);
    }
}
