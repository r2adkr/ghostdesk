//! Settings → Tool box: the outside tools (MCP servers) key-based agents can use. Added, switched on and off and
//! removed here; the person approves exactly what will run before it is added, and agents ask again the first
//! time each tool is used.
use super::*;
use crate::pluginbox::{self, Plugin};
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
    /// Which add form is open (from the "Add" menu).
    add: Option<Kind>,
    /// Which kinds the list shows, and the text it is narrowed to.
    kind: Kind,
    filter: String,
    /// The "found on this computer" group is unfolded.
    found_open: bool,
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
    /// "Import all" waiting for a go-ahead: each skill with its size.
    import_all: Option<Vec<(String, u64)>>,
    /// Tools found in other apps (and which are ticked), read when the tab opens.
    found: Vec<toolbox::Found>,
    found_loaded: bool,
    /// A found tool waiting for the person's go-ahead (what will run is shown under it).
    found_pick: Option<usize>,
    plugins: Vec<Plugin>,
    plugins_loaded: bool,
    plugin_source: String,
    plugin_ack: bool,
    plugin_busy: Option<Receiver<Result<(), String>>>,
    plugin_note: Option<String>,
}

/// What the tool box list shows.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(super) enum Kind {
    #[default]
    All,
    Tools,
    Skills,
    Plugins,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::All => tr("전체"),
            Kind::Tools => tr("외부 도구"),
            Kind::Skills => tr("스킬"),
            Kind::Plugins => tr("플러그인"),
        }
    }
}

/// Whether any of `texts` contains the filter `q` (lowercase; empty matches everything).
fn hit(q: &str, texts: &[&str]) -> bool {
    q.is_empty() || texts.iter().any(|t| t.to_lowercase().contains(q))
}

/// One line of text cut to `width` with an ellipsis.
fn one_line(ui: &egui::Ui, text: &str, font: egui::FontId, color: egui::Color32, width: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(10.0));
    ui.fonts_mut(|f| f.layout_job(job))
}

/// One item of the tool box: an icon tile, the name with an optional status badge, a one-line detail
/// (text, color, monospace) under it, and the item's controls (`add`, `trail_w` wide) at the right. A
/// divider below.
fn item_row<R>(
    ui: &mut egui::Ui,
    glyph: &str,
    name: &str,
    badge: Option<(&str, egui::Color32)>,
    detail: (&str, egui::Color32, bool),
    trail_w: f32,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    use egui::{pos2, FontId, Rect, Stroke};
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 58.0), egui::Sense::hover());
    let tile = Rect::from_center_size(pos2(rect.left() + 17.0, rect.center().y), Vec2::splat(34.0));
    let x = tile.right() + 12.0;
    let text_w = (rect.right() - trail_w - 16.0 - x).max(60.0);
    let badge_g = badge.map(|(t, c)| (one_line(ui, t, FontId::proportional(11.0), c, 140.0), c));
    let badge_w = badge_g.as_ref().map_or(0.0, |(g, _)| g.size().x + 12.0 + 8.0);
    let name_g = one_line(ui, name, FontId::proportional(14.0), theme::text(), text_w - badge_w);
    let (detail_text, detail_color, mono) = detail;
    let detail_font = if mono { FontId::monospace(11.5) } else { FontId::proportional(12.0) };
    let detail_g = (!detail_text.is_empty()).then(|| one_line(ui, detail_text, detail_font, detail_color, text_w));
    let elided = detail_g.as_ref().is_some_and(|g| g.elided) || name_g.elided;
    let p = ui.painter();
    p.rect_filled(tile, 8, theme::card());
    p.rect_stroke(tile, 8, Stroke::new(1.0, theme::border()), egui::StrokeKind::Inside);
    p.text(tile.center(), egui::Align2::CENTER_CENTER, glyph, FontId::proportional(16.0), theme::body());
    let block_h = name_g.size().y + detail_g.as_ref().map_or(0.0, |g| 2.0 + g.size().y);
    let mut y = rect.center().y - block_h / 2.0;
    let name_size = name_g.size();
    p.galley(pos2(x, y), name_g, theme::text());
    if let Some((g, c)) = badge_g {
        let r = Rect::from_min_size(pos2(x + name_size.x + 8.0, y + (name_size.y - g.size().y - 4.0) / 2.0), Vec2::new(g.size().x + 12.0, g.size().y + 4.0));
        p.rect_filled(r, 255, c.gamma_multiply(0.14));
        p.galley(pos2(r.left() + 6.0, r.top() + 2.0), g, c);
    }
    y += name_size.y + 2.0;
    if let Some(g) = detail_g {
        p.galley(pos2(x, y), g, detail_color);
    }
    p.hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, theme::rim()));
    if elided && !detail_text.is_empty() {
        resp.on_hover_text(format!("{name}\n{detail_text}"));
    }
    let trail = Rect::from_min_max(pos2(rect.right() - trail_w, rect.top()), rect.max);
    ui.scope_builder(egui::UiBuilder::new().max_rect(trail).layout(egui::Layout::right_to_left(egui::Align::Center)), add).inner
}

/// Bytes as KB or MB.
fn size_text(b: u64) -> String {
    if b >= 1024 * 1024 { format!("{:.1}MB", b as f64 / 1048576.0) } else { format!("{}KB", b.div_ceil(1024)) }
}

/// The "…" menu at the end of a row.
fn row_menu(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.menu_button(RichText::new(icon::ELLIPSIS).size(15.0).color(theme::body()), add).response.on_hover_text(tr("더 보기"));
}

/// A group title over a run of rows, with a count after it.
fn group_title(ui: &mut egui::Ui, title: &str, count: usize) {
    ui.add_space(18.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(13.0).strong().color(theme::body()));
        if count > 0 {
            theme::count_badge(ui, &count.to_string());
        }
    });
    ui.add_space(2.0);
}

/// A quiet line standing in for an empty group.
fn empty_line(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(theme::hint(text));
    ui.add_space(4.0);
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
    /// Settings → Tool box: a filter, the kind picker and an Add menu on top, then everything installed as
    /// one list of rows (tools, skills, plugins), and what was found elsewhere on this computer folded at the end.
    pub(super) fn settings_toolbox(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if !self.toolbox.loaded {
            self.toolbox.servers = toolbox::load();
            self.toolbox.loaded = true;
        }
        if !self.toolbox.skills_loaded {
            self.reload_skills();
        }
        if !self.toolbox.plugins_loaded {
            self.toolbox.plugins = pluginbox::load();
            self.toolbox.plugins_loaded = true;
        }
        if !self.toolbox.found_loaded {
            self.toolbox.found = toolbox::discover();
            self.toolbox.found_loaded = true;
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
        self.poll_plugin(ui);
        let installed = crate::pi::find().is_some();
        ui.label(theme::hint(tr("API 키로 쓰는 AI 에이전트가 쓸 수 있는 외부 도구예요. 도구는 처음 쓸 때마다 내게 허락을 물어요.")));
        if !installed {
            ui.label(RichText::new(tr("이 기능을 쓰려면 먼저 AI 탭에서 설치해 주세요")).size(12.0).color(theme::warn()));
        }
        ui.add_space(10.0);
        self.toolbox_toolbar(ui);
        if let Some(kind) = self.toolbox.add {
            ui.add_space(10.0);
            theme::soft_card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                match kind {
                    Kind::Skills => self.add_skill_form(ui),
                    Kind::Plugins => self.add_plugin_form(ui),
                    _ => self.add_tool_form(ui),
                }
            });
        }
        let q = self.toolbox.filter.trim().to_lowercase();
        let kind = self.toolbox.kind;
        let show = |k: Kind| kind == Kind::All || kind == k;
        if show(Kind::Tools) {
            self.tools_group(ui, &q, installed);
        }
        if show(Kind::Skills) {
            self.skills_group(ui, &q);
        }
        if show(Kind::Plugins) {
            self.plugins_group(ui, &q);
        }
        self.found_group(ui, &q);
    }

    /// Filter field on its own line, then the kind picker at the left and the Add menu at the right.
    fn toolbox_toolbar(&mut self, ui: &mut egui::Ui) {
        let w = ui.available_width();
        ui.add(
            egui::TextEdit::singleline(&mut self.toolbox.filter)
                .desired_width(w)
                .margin(Margin::symmetric(10, 7))
                .hint_text(format!("{}  {}", icon::SEARCH, tr("이름이나 설명으로 찾기"))),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            egui::Frame::new()
                .fill(theme::card())
                .stroke(egui::Stroke::new(1.0, theme::border()))
                .corner_radius(theme::RADIUS)
                .inner_margin(Margin::same(2))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for k in [Kind::All, Kind::Tools, Kind::Skills, Kind::Plugins] {
                        if ui.add(egui::Button::selectable(self.toolbox.kind == k, k.label()).min_size(Vec2::new(0.0, 26.0))).clicked() {
                            self.toolbox.kind = k;
                        }
                    }
                });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let plugins_ok = self.access.plugins;
                ui.menu_button(format!("{}  {}", icon::PLUS, tr("추가")), |ui| {
                    if ui.button(tr("도구 추가")).clicked() {
                        self.toolbox.add = Some(Kind::Tools);
                    }
                    if ui.button(tr("스킬 가져오기")).clicked() {
                        self.toolbox.add = Some(Kind::Skills);
                    }
                    let r = ui.add_enabled(plugins_ok, egui::Button::new(tr("플러그인 설치")));
                    if r.clicked() {
                        self.toolbox.add = Some(Kind::Plugins);
                    }
                    if !plugins_ok {
                        r.on_disabled_hover_text(tr("플러그인을 쓰려면 설정 → 권한에서 '플러그인 설치 허용'을 먼저 켜세요"));
                    }
                });
            });
        });
    }

    /// Outside tools (MCP servers): switch, status, and a menu with how the AI sees them and removal.
    fn tools_group(&mut self, ui: &mut egui::Ui, q: &str, installed: bool) {
        group_title(ui, tr("외부 도구"), self.toolbox.servers.len());
        let mut changed = false;
        let mut remove: Option<usize> = None;
        let mut shown = 0;
        for i in 0..self.toolbox.servers.len() {
            let runs = self.toolbox.servers[i].runs();
            if !hit(q, &[&self.toolbox.servers[i].name, &runs]) {
                continue;
            }
            shown += 1;
            let status = self.toolbox.status.iter().find(|st| st.name == self.toolbox.servers[i].name).cloned();
            let s = &mut self.toolbox.servers[i];
            let failed = status.as_ref().filter(|st| s.enabled && st.state != "connected").map(|st| st.error.clone().unwrap_or_else(|| st.state.clone()));
            let connected = status.as_ref().filter(|st| s.enabled && st.state == "connected").map(|st| st.tools.len());
            let badge_text = connected.map(|n| trf("연결됨 · 도구 {}개", &[&n]));
            let badge = match (&badge_text, &failed) {
                (Some(t), _) => Some((t.as_str(), theme::success())),
                (None, Some(_)) => Some((tr("연결 실패"), theme::danger())),
                _ => None,
            };
            let detail = match &failed {
                Some(e) => (e.as_str(), theme::danger(), false),
                None => (runs.as_str(), theme::muted(), true),
            };
            let name = s.name.clone();
            item_row(ui, icon::PLUG, &name, badge, detail, 96.0, |ui| {
                row_menu(ui, |ui| {
                    ui.label(theme::hint(tr("표시 방식")));
                    changed |= ui
                        .radio_value(&mut s.exposure, toolbox::DEFERRED.to_string(), tr("필요할 때 찾기"))
                        .on_hover_text(tr("'필요할 때 찾기'는 AI 가 필요할 때만 도구를 불러와서 토큰을 아껴요"))
                        .changed();
                    changed |= ui.radio_value(&mut s.exposure, toolbox::DIRECT.to_string(), tr("항상 보이기")).changed();
                    ui.separator();
                    if ui.button(RichText::new(tr("목록에서 지우기")).color(theme::danger())).clicked() {
                        remove = Some(i);
                    }
                });
                changed |= ui.add(theme::switch(&mut s.enabled, name.as_str()).text_hidden()).changed();
            });
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
        if self.toolbox.servers.is_empty() {
            empty_line(ui, tr("아직 추가한 도구가 없어요"));
        } else if shown == 0 {
            empty_line(ui, tr("맞는 항목이 없어요"));
        }
        let busy = self.toolbox.probing.is_some();
        let can = installed && !busy && self.toolbox.servers.iter().any(|s| s.enabled);
        if !self.toolbox.servers.is_empty() {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(can, egui::Button::new(tr("연결 확인"))).on_hover_text(tr("켜진 도구를 실제로 실행해서 연결되는지 확인해요")).clicked() {
                    let (tx, rx) = std::sync::mpsc::channel();
                    self.toolbox.probing = Some(rx);
                    self.toolbox.note = None;
                    std::thread::spawn(move || {
                        let _ = tx.send(toolbox::probe());
                    });
                }
                if busy {
                    theme::spinner(ui, 13.0, theme::muted());
                }
            });
        }
        if self.toolbox.add != Some(Kind::Tools) {
            if let Some(n) = &self.toolbox.note {
                ui.label(RichText::new(n.clone()).size(12.0).color(theme::danger()));
            }
        }
    }

    /// The form behind Add → Add tool, and the go-ahead showing exactly what will run.
    fn add_tool_form(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new(tr("도구 추가")).strong().color(theme::text()));
        ui.add_space(6.0);
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
                                self.toolbox.add = None;
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
        if let Some(n) = &self.toolbox.note {
            ui.label(RichText::new(n.clone()).size(12.0).color(theme::danger()));
        }
        if cancel {
            self.toolbox.add = None;
            self.toolbox.note = None;
        }
    }

    fn reload_skills(&mut self) {
        self.toolbox.skills = skillbox::list();
        self.toolbox.candidates = skillbox::candidates();
        self.toolbox.skills_loaded = true;
    }

    /// Imported skills: a switch and a menu to remove each.
    fn skills_group(&mut self, ui: &mut egui::Ui, q: &str) {
        group_title(ui, tr("스킬"), self.toolbox.skills.len());
        let mut remove: Option<usize> = None;
        let mut toggle: Option<(usize, bool)> = None;
        let mut shown = 0;
        for (i, s) in self.toolbox.skills.iter().enumerate() {
            if !hit(q, &[&s.name, &s.description]) {
                continue;
            }
            shown += 1;
            item_row(ui, icon::BOOK_OPEN, &s.name, None, (&s.description, theme::muted(), false), 96.0, |ui| {
                row_menu(ui, |ui| {
                    if ui.button(RichText::new(tr("지우기")).color(theme::danger())).clicked() {
                        remove = Some(i);
                    }
                });
                let mut on = s.enabled;
                if ui.add(theme::switch(&mut on, s.name.as_str()).text_hidden()).changed() {
                    toggle = Some((i, on));
                }
            });
        }
        let mut changed = false;
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
            empty_line(ui, tr("아직 가져온 스킬이 없어요"));
        } else if shown == 0 {
            empty_line(ui, tr("맞는 항목이 없어요"));
        }
        if self.toolbox.add != Some(Kind::Skills) {
            if let Some(n) = &self.toolbox.skill_note {
                ui.label(RichText::new(n.clone()).size(12.0).color(theme::danger()));
            }
        }
    }

    /// The go-ahead for importing one skill (what it is, how big).
    fn skill_confirm_card(&mut self, ui: &mut egui::Ui) {
        let Some((c, files, bytes)) = self.toolbox.skill_confirm.clone() else { return };
        ui.add_space(6.0);
        theme::tinted(theme::warn()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(trf("'{}' 스킬을 가져올까요?", &[&c.name])).strong());
            ui.label(theme::hint(crate::llm::short(&c.description, 160)));
            ui.label(theme::hint(trf("파일 {}개 · {}. 스킬에 든 스크립트는 에이전트가 실행하려 할 때 내게 허락을 물어요.", &[&files, &size_text(bytes)])));
            if bytes > skillbox::LARGE {
                ui.label(RichText::new(trf("용량이 커요 ({}). 이 앱 폴더에 그대로 복사돼 그만큼 디스크를 차지해요.", &[&size_text(bytes)])).size(12.0).color(theme::danger()));
            }
            ui.horizontal(|ui| {
                if theme::primary(ui, tr("가져오기")).clicked() {
                    match skillbox::import(&c.path) {
                        Ok(_) => {
                            self.toolbox.skill_note = None;
                            self.toolbox.import_path.clear();
                            if self.toolbox.add == Some(Kind::Skills) {
                                self.toolbox.add = None;
                            }
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
        ui.add_space(6.0);
    }

    /// Looks a skill folder over and asks for the go-ahead.
    fn pick_skill(&mut self, path: std::path::PathBuf) {
        match skillbox::inspect(&path) {
            Ok(found) => {
                self.toolbox.skill_note = None;
                self.toolbox.skill_confirm = Some(found);
            }
            Err(e) => self.toolbox.skill_note = Some(tr(&e).to_string()),
        }
    }

    /// The form behind Add → Import skill: a folder path.
    fn add_skill_form(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new(tr("스킬 가져오기")).strong().color(theme::text()));
        ui.label(theme::hint(tr("스킬은 특정 작업을 잘 하게 해 주는 안내서예요. 가져온 스킬은 이 앱 안에 복사되어 따로 보관돼요.")));
        ui.add_space(6.0);
        // a skill picked by path is confirmed here; one picked from the found list is confirmed under its row
        let picked_by_path = self.toolbox.skill_confirm.as_ref().is_some_and(|(c, ..)| !self.toolbox.candidates.iter().any(|k| k.path == c.path));
        if picked_by_path {
            self.skill_confirm_card(ui);
        } else {
            let mut pick = None;
            ui.add(
                egui::TextEdit::singleline(&mut self.toolbox.import_path)
                    .desired_width(ui.available_width())
                    .margin(Margin::symmetric(10, 6))
                    .hint_text(tr("스킬 폴더 경로 (SKILL.md 가 든 폴더)")),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if theme::primary(ui, tr("확인")).clicked() && !self.toolbox.import_path.trim().is_empty() {
                    pick = Some(std::path::PathBuf::from(self.toolbox.import_path.trim()));
                }
                if theme::secondary(ui, tr("취소")).clicked() {
                    self.toolbox.add = None;
                    self.toolbox.skill_note = None;
                }
            });
            if let Some(path) = pick {
                self.pick_skill(path);
            }
        }
        if let Some(n) = &self.toolbox.skill_note {
            ui.label(RichText::new(n.clone()).size(12.0).color(theme::danger()));
        }
    }

    /// Finished plugin installs and removals.
    fn poll_plugin(&mut self, ui: &egui::Ui) {
        if let Some(rx) = &self.toolbox.plugin_busy {
            match rx.try_recv() {
                Ok(r) => {
                    self.toolbox.plugin_busy = None;
                    self.toolbox.plugins = pluginbox::load();
                    match r {
                        Ok(()) => {
                            self.toolbox.plugin_note = None;
                            self.toolbox.plugin_source.clear();
                            self.toolbox.plugin_ack = false;
                            if self.toolbox.add == Some(Kind::Plugins) {
                                self.toolbox.add = None;
                            }
                        }
                        Err(e) => self.toolbox.plugin_note = Some(e),
                    }
                }
                Err(TryRecvError::Empty) => ui.ctx().request_repaint_after(std::time::Duration::from_millis(300)),
                Err(_) => self.toolbox.plugin_busy = None,
            }
        }
    }

    /// Installed plugins: a switch and a menu to remove each.
    fn plugins_group(&mut self, ui: &mut egui::Ui, q: &str) {
        group_title(ui, tr("플러그인"), self.toolbox.plugins.len());
        let mut changed = false;
        let mut remove: Option<usize> = None;
        let mut shown = 0;
        for (i, p) in self.toolbox.plugins.iter_mut().enumerate() {
            if !hit(q, &[&p.source]) {
                continue;
            }
            shown += 1;
            let source = p.source.clone();
            item_row(ui, icon::ZAP, &source, None, ("", theme::muted(), true), 96.0, |ui| {
                row_menu(ui, |ui| {
                    if ui.button(RichText::new(tr("지우기")).color(theme::danger())).clicked() {
                        remove = Some(i);
                    }
                });
                changed |= ui.add(theme::switch(&mut p.enabled, source.as_str()).text_hidden()).changed();
            });
        }
        if let Some(i) = remove {
            let src = self.toolbox.plugins[i].source.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            self.toolbox.plugin_busy = Some(rx);
            std::thread::spawn(move || {
                let _ = tx.send(pluginbox::remove(&src));
            });
        }
        if changed {
            if let Err(e) = pluginbox::save(&self.toolbox.plugins) {
                self.toolbox.plugin_note = Some(trf("저장하지 못했어요 ({})", &[&e]));
            }
        }
        if !self.access.plugins {
            empty_line(ui, tr("플러그인을 쓰려면 설정 → 권한에서 '플러그인 설치 허용'을 먼저 켜세요"));
        } else if self.toolbox.plugins.is_empty() {
            empty_line(ui, tr("아직 설치한 플러그인이 없어요"));
        } else if shown == 0 {
            empty_line(ui, tr("맞는 항목이 없어요"));
        }
        if self.toolbox.add != Some(Kind::Plugins) {
            if let Some(n) = &self.toolbox.plugin_note {
                ui.label(RichText::new(n.clone()).size(12.0).color(theme::danger()));
            }
        }
    }

    /// The form behind Add → Install plugin: the source, the warning, and the tick it needs.
    fn add_plugin_form(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new(tr("플러그인 설치")).strong().color(theme::text()));
        ui.label(theme::hint(tr("플러그인은 에이전트에 새 기능을 더하는 코드예요.")));
        ui.add_space(6.0);
        let busy = self.toolbox.plugin_busy.is_some();
        ui.add(
            egui::TextEdit::singleline(&mut self.toolbox.plugin_source)
                .desired_width(ui.available_width())
                .margin(Margin::symmetric(10, 6))
                .hint_text(tr("npm:이름@버전  ·  git:주소@태그  ·  폴더 전체 경로")),
        );
        let ready = !self.toolbox.plugin_source.trim().is_empty();
        if ready {
            ui.add_space(6.0);
            theme::tinted(theme::danger()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(tr("설치 전에 꼭 읽어 주세요")).strong().color(theme::danger()));
                ui.label(tr("플러그인은 이 앱 안에서 코드를 그대로 실행해요. 에이전트의 AI 키와 이 컴퓨터의 파일에 접근할 수 있고, 내 승인 창을 거치지 않고 일할 수도 있어요. 만든 사람을 믿을 수 있을 때만 설치하세요. 가능하면 버전을 고정해서 쓰세요."));
                ui.add_space(4.0);
                ui.label(RichText::new(self.toolbox.plugin_source.trim()).monospace());
                ui.add_space(4.0);
                ui.checkbox(&mut self.toolbox.plugin_ack, tr("위험을 이해했고, 이 플러그인을 믿고 설치해요"));
            });
        }
        if let Some(n) = &self.toolbox.plugin_note {
            ui.label(RichText::new(n.clone()).size(12.0).color(theme::danger()));
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let can = ready && self.toolbox.plugin_ack && !busy;
            if ui.add_enabled(can, egui::Button::new(tr("설치"))).clicked() {
                let src = self.toolbox.plugin_source.trim().to_string();
                match pluginbox::valid_source(&src) {
                    Ok(()) => {
                        self.toolbox.plugin_note = None;
                        let (tx, rx) = std::sync::mpsc::channel();
                        self.toolbox.plugin_busy = Some(rx);
                        std::thread::spawn(move || {
                            let _ = tx.send(pluginbox::install(&src));
                        });
                    }
                    Err(why) => self.toolbox.plugin_note = Some(tr(why).to_string()),
                }
            }
            if theme::secondary(ui, tr("취소")).clicked() {
                self.toolbox.add = None;
                self.toolbox.plugin_note = None;
            }
            if busy {
                theme::spinner(ui, 13.0, theme::muted());
            }
        });
    }

    /// Tools set up in other apps and skills found on this computer, folded into one line until opened
    /// (or while filtering). Each is brought in on its own, after the person sees what it is.
    fn found_group(&mut self, ui: &mut egui::Ui, q: &str) {
        let kind = self.toolbox.kind;
        let tools: Vec<usize> = if matches!(kind, Kind::All | Kind::Tools) {
            (0..self.toolbox.found.len()).filter(|&i| hit(q, &[&self.toolbox.found[i].server.name, self.toolbox.found[i].from])).collect()
        } else {
            vec![]
        };
        let skills: Vec<usize> = if matches!(kind, Kind::All | Kind::Skills) {
            (0..self.toolbox.candidates.len()).filter(|&i| hit(q, &[&self.toolbox.candidates[i].name, &self.toolbox.candidates[i].description])).collect()
        } else {
            vec![]
        };
        let total = tools.len() + skills.len();
        if total == 0 {
            return;
        }
        ui.add_space(18.0);
        let open = self.toolbox.found_open || !q.is_empty();
        let names: Vec<&str> = tools.iter().map(|&i| self.toolbox.found[i].server.name.as_str()).chain(skills.iter().map(|&i| self.toolbox.candidates[i].name.as_str())).take(3).collect();
        let mut summary = names.join(", ");
        if total > names.len() {
            summary = trf("{} 외 {}개", &[&summary, &(total - names.len())]);
        }
        let header = ui.horizontal(|ui| {
            ui.label(RichText::new(if open { icon::CHEVRON_DOWN } else { "›" }).size(if open { 13.0 } else { 16.0 }).color(theme::muted()));
            ui.label(RichText::new(tr("이 컴퓨터에서 찾음")).size(13.0).strong().color(theme::body()));
            theme::count_badge(ui, &total.to_string());
            if !open {
                ui.add(egui::Label::new(theme::hint(summary)).truncate());
            }
        });
        let resp = ui.interact(header.response.rect, ui.id().with("found_header"), egui::Sense::click());
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            self.toolbox.found_open = !self.toolbox.found_open;
        }
        if !open {
            return;
        }
        ui.label(theme::hint(tr("Claude, Cursor, Codex 등에 이미 설정해 둔 도구와 이 컴퓨터에 있는 스킬이에요. 가져오기 전에 무엇인지 보여 드려요.")));
        let mut pick_tool = None;
        let mut pick_skill = None;
        for i in tools {
            let f = &self.toolbox.found[i];
            let runs = f.server.runs();
            item_row(ui, icon::PLUG, &f.server.name, Some((f.from, theme::info())), (&runs, theme::muted(), true), 110.0, |ui| {
                if theme::secondary(ui, tr("가져오기")).clicked() {
                    pick_tool = Some(i);
                }
            });
            if self.toolbox.found_pick == Some(i) {
                self.found_confirm_card(ui, i);
            }
        }
        if skills.len() > 1 {
            ui.add_space(8.0);
            if let Some(sizes) = self.toolbox.import_all.clone() {
                theme::tinted(theme::warn()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(trf("스킬 {}개를 모두 가져올까요?", &[&self.toolbox.candidates.len()])).strong());
                    ui.label(theme::hint(tr("각 스킬이 이 앱 안에 복사돼요. 스킬에 든 스크립트는 에이전트가 실행하려 할 때 내게 허락을 물어요.")));
                    ui.label(theme::hint(trf("모두 합쳐 {}", &[&size_text(sizes.iter().map(|(_, b)| b).sum())])));
                    egui::ScrollArea::vertical().max_height(160.0).auto_shrink([false, true]).show(ui, |ui| {
                        for (n, b) in &sizes {
                            let c = if *b > skillbox::LARGE { theme::danger() } else { theme::body() };
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(n).size(12.5).color(c));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.label(RichText::new(size_text(*b)).size(12.0).color(c));
                                });
                            });
                        }
                    });
                    ui.horizontal(|ui| {
                        if theme::primary(ui, tr("모두 가져오기")).clicked() {
                            let bad: Vec<String> = self.toolbox.candidates.clone().iter().filter_map(|c| skillbox::import(&c.path).err().map(|e| format!("{} ({})", c.name, tr(&e)))).collect();
                            self.toolbox.skill_note = (!bad.is_empty()).then(|| trf("가져오지 못한 스킬: {}", &[&bad.join(", ")]));
                            self.toolbox.import_all = None;
                            self.reload_skills();
                        }
                        if theme::secondary(ui, tr("취소")).clicked() {
                            self.toolbox.import_all = None;
                        }
                    });
                });
            } else if theme::secondary(ui, trf("스킬 {}개 모두 가져오기", &[&self.toolbox.candidates.len()])).clicked() {
                self.toolbox.import_all = Some(self.toolbox.candidates.iter().filter_map(|c| skillbox::inspect(&c.path).ok().map(|(_, _, b)| (c.name.clone(), b))).collect());
            }
        }
        for i in skills {
            let Some(c) = self.toolbox.candidates.get(i) else { break };
            let path = c.path.clone();
            item_row(ui, icon::BOOK_OPEN, &c.name, Some((tr("스킬"), theme::muted())), (&c.description, theme::muted(), false), 110.0, |ui| {
                if theme::secondary(ui, tr("가져오기")).clicked() {
                    pick_skill = Some(path.clone());
                }
            });
            if self.toolbox.skill_confirm.as_ref().is_some_and(|(k, ..)| k.path == path) {
                self.skill_confirm_card(ui);
            }
        }
        if let Some(i) = pick_tool {
            self.toolbox.found_pick = Some(i);
        }
        if let Some(path) = pick_skill {
            self.pick_skill(path);
        }
    }

    /// The go-ahead for one found tool, showing what will run.
    fn found_confirm_card(&mut self, ui: &mut egui::Ui, i: usize) {
        let s = self.toolbox.found[i].server.clone();
        ui.add_space(6.0);
        theme::tinted(theme::warn()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(if s.url.is_empty() { tr("이 컴퓨터에서 아래 명령이 실행돼요") } else { tr("아래 주소와 연결해요") }).strong());
            ui.label(RichText::new(s.runs()).monospace());
            ui.label(theme::hint(tr("믿을 수 있는 도구만 가져오세요. 환경 변수는 이 컴퓨터에만 저장돼요.")));
            ui.horizontal(|ui| {
                if theme::primary(ui, tr("허락하고 추가")).clicked() {
                    match toolbox::check(&self.toolbox.servers, &s) {
                        Ok(()) => {
                            self.toolbox.servers.push(s.clone());
                            match toolbox::save(&self.toolbox.servers) {
                                Ok(()) => self.toolbox.status.clear(),
                                Err(e) => self.toolbox.note = Some(trf("저장하지 못했어요 ({})", &[&e])),
                            }
                        }
                        Err(why) => self.toolbox.note = Some(tr(why).to_string()),
                    }
                    self.toolbox.found_loaded = false;
                    self.toolbox.found_pick = None;
                }
                if theme::secondary(ui, tr("취소")).clicked() {
                    self.toolbox.found_pick = None;
                }
            });
        });
        ui.add_space(6.0);
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
