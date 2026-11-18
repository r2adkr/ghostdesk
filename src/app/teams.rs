//! Teams screen: shared work boards for agents working together (team.rs).
//!
//! The user creates and deletes teams here, sees who joined with which role, and reads and adds
//! to the board (goal, tasks, notes). Agents join through the `team` MCP tool.

use super::*;
use crate::team::{Task, TaskState, Team, USER};

/// Teams screen state (inputs and selection).
#[derive(Default)]
pub(super) struct TeamsUi {
    selected: Option<u64>,
    new_name: String,
    new_goal: String,
    creating: bool,
    drafts: std::collections::HashMap<u64, BoardDraft>,
    focus_new_name: bool,
    /// open confirm/rename popup
    dialog: Option<Dialog>,
    /// copy of the store, refreshed when it changes
    cache: Vec<Team>,
    cache_version: Option<u64>,
    error: Option<String>,
}

#[derive(Default)]
struct BoardDraft {
    task: String,
    note: String,
    task_error: Option<String>,
    note_error: Option<String>,
}

/// Popups of the Teams screen.
enum Dialog {
    Delete { id: u64, name: String, notes: usize, tasks: usize },
    Rename { id: u64, name: String, error: Option<String> },
}

impl Dialog {
    fn delete(t: &Team) -> Self {
        Dialog::Delete { id: t.id, name: t.name.clone(), notes: t.notes.len(), tasks: t.tasks.len() }
    }

    fn rename(t: &Team) -> Self {
        Dialog::Rename { id: t.id, name: t.name.clone(), error: None }
    }
}

const LIST_W: f32 = 224.0;
const COLUMN_GAP: f32 = 20.0;
const BOARD_MIN_W: f32 = 440.0;

impl TeamsUi {
    /// The teams as last read from the store.
    pub(super) fn cache(&self) -> &[Team] {
        &self.cache
    }
}

impl GhostApp {
    /// Whether any agent is in a team right now (the sidebar's dot).
    pub(super) fn teams_ui_active(&mut self) -> bool {
        let v = self.teams.version();
        if self.teams_ui.cache_version != Some(v) {
            self.teams_ui.cache = self.teams.snapshot();
            self.teams_ui.cache_version = Some(v);
        }
        self.teams_ui.cache.iter().any(|t| !t.members.is_empty())
    }

    pub(super) fn teams_view(&mut self, ui: &mut egui::Ui) {
        // screens per team (members' screens, by the connection that opened them)
        let mut screens: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
        let badges = self.team_badges();
        for agent in &self.agents {
            if let Some((team, _)) = badges.get(&agent.shared.id) {
                if agent.shared.workspace().is_some_and(|desk| desk.has_screen() && !desk.is_closed()) {
                    *screens.entry(*team).or_default() += 1;
                }
            }
        }
        let store = self.teams.clone();
        if let Some(error) = &store.load_error {
            ui.label(RichText::new(error).color(theme::danger()));
            return;
        }
        let v = store.version();
        if self.teams_ui.cache_version != Some(v) {
            self.teams_ui.cache = store.snapshot();
            self.teams_ui.cache_version = Some(v);
        }
        let ts = &mut self.teams_ui;
        ts.drafts.retain(|id, _| ts.cache.iter().any(|team| team.id == *id));
        if ts.selected.is_none_or(|id| !ts.cache.iter().any(|t| t.id == id)) {
            ts.selected = ts.cache.first().map(|t| t.id);
        }

        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("팀")).size(20.0).strong().color(theme::text()));
            ui.label(RichText::new(ts.cache.len().to_string()).size(14.0).color(theme::muted()));
        });
        ui.label(theme::hint(tr("팀에서 에이전트와 목표·할 일·메모를 공유해요")));
        ui.add_space(16.0);

        let full = ui.available_rect_before_wrap();
        let mut action = None;
        let mut show_board = |ui: &mut egui::Ui, ts: &mut TeamsUi| match ts.cache.iter().find(|t| Some(t.id) == ts.selected).cloned() {
            Some(team) => {
                ui.push_id(("team_board", team.id), |ui| {
                    action = board(ui, ts, &store, &team, screens.get(&team.id).copied().unwrap_or(0)).map(|action| (team.id, action));
                });
            }
            None => empty_board(ui),
        };
        if full.width() < LIST_W + COLUMN_GAP + BOARD_MIN_W {
            egui::ScrollArea::vertical().id_salt("teams_compact").auto_shrink([false, false]).show(ui, |ui| {
                team_list(ui, ts, &store, 128.0);
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(12.0);
                show_board(ui, ts);
            });
        } else {
            let list_rect = egui::Rect::from_min_size(full.min, egui::vec2(LIST_W, full.height()));
            let board_rect = egui::Rect::from_min_max(egui::pos2(list_rect.right() + COLUMN_GAP, full.min.y), full.max);
            let mut list_ui = ui.new_child(egui::UiBuilder::new().max_rect(list_rect).layout(egui::Layout::top_down(egui::Align::Min)));
            // The list and its creation form must both remain reachable in a short window.
            egui::ScrollArea::vertical().id_salt("teams_list").auto_shrink([false, false]).show(&mut list_ui, |ui| {
                team_list(ui, ts, &store, f32::INFINITY);
            });
            let mut board_ui = ui.new_child(egui::UiBuilder::new().max_rect(board_rect).layout(egui::Layout::top_down(egui::Align::Min)));
            egui::ScrollArea::vertical().id_salt(("teams_board", ts.selected)).auto_shrink([false, false]).show(&mut board_ui, |ui| {
                show_board(ui, ts);
            });
            ui.allocate_rect(full, egui::Sense::hover());
        }
        dialog(ui.ctx(), ts, &store);
        match action {
            Some((id, BoardAction::StartAgent)) => {
                self.open_wizard();
                self.form.team_id = Some(id);
            }
            Some((id, BoardAction::WatchScreens)) => {
                self.enter_wall(ui.ctx());
                self.wall_team = Some(id);
            }
            None => {}
        }
    }
}

/// Left column: the teams, and a form to create one.
fn team_list(ui: &mut egui::Ui, ts: &mut TeamsUi, store: &crate::team::TeamStore, max_list_height: f32) {
    if !ts.creating && theme::primary(ui, format!("{}  {}", icon::PLUS, tr("새 팀"))).clicked() {
        ts.creating = true;
        ts.focus_new_name = true;
        ts.error = None;
    }
    if ts.creating {
        ui.spacing_mut().item_spacing.y = 6.0;
        let name = ui.add(
            egui::TextEdit::singleline(&mut ts.new_name)
                .desired_width(ui.available_width())
                .margin(Margin::symmetric(10, 6))
                .hint_text(tr("팀 이름")),
        );
        if std::mem::take(&mut ts.focus_new_name) {
            name.request_focus();
        }
        ui.add(
            egui::TextEdit::multiline(&mut ts.new_goal)
                .desired_width(ui.available_width())
                .desired_rows(2)
                .margin(Margin::symmetric(10, 6))
                .hint_text(tr("목표 (예: 다음 주 제주 출장 일정과 예약 정리)")),
        );
        ui.horizontal(|ui| {
            if theme::primary(ui, tr("만들기")).clicked() || (name.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                match store.create(&ts.new_name, &ts.new_goal, USER) {
                    Ok(id) => {
                        ts.cache_version = Some(store.version());
                        ts.cache = store.snapshot();
                        ts.selected = Some(id);
                        ts.creating = false;
                        ts.new_name.clear();
                        ts.new_goal.clear();
                        ts.error = None;
                    }
                    Err(e) => ts.error = Some(format!("{e:#}")),
                }
            }
            if theme::secondary(ui, tr("취소")).clicked() {
                ts.creating = false;
                ts.error = None;
            }
        });
    }
    if let Some(e) = &ts.error {
        ui.label(RichText::new(e).size(12.0).color(theme::danger()));
    }
    if !ts.cache.is_empty() {
        ui.add_space(8.0);
        let mut show_rows = |ui: &mut egui::Ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for t in &ts.cache {
                let selected = ts.selected == Some(t.id);
                let r = theme::nav_item(ui, selected, icon::LIST_TODO, &t.name, (!t.members.is_empty()).then(theme::success));
                if r.clicked() {
                    ts.selected = Some(t.id);
                    ts.error = None;
                }
                // right click: rename or delete
                r.context_menu(|ui| {
                    if ui.button(tr("이름 바꾸기")).clicked() {
                        ts.dialog = Some(Dialog::rename(t));
                        ui.close();
                    }
                    if ui.button(RichText::new(tr("팀 삭제")).color(theme::danger())).clicked() {
                        ts.dialog = Some(Dialog::delete(t));
                        ui.close();
                    }
                });
            }
        };
        if max_list_height.is_finite() {
            egui::ScrollArea::vertical().id_salt("compact_team_list").max_height(max_list_height).show(ui, show_rows);
        } else {
            show_rows(ui);
        }
    }
}

fn empty_board(ui: &mut egui::Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.25).max(24.0));
        ui.label(RichText::new(tr("아직 팀이 없어요")).size(20.0).color(theme::text()));
        ui.add_space(8.0);
        ui.label(theme::hint(tr("'새 팀'을 누르거나 AI 앱에 팀을 만들어 달라고 하세요")));
    });
}

enum BoardAction {
    StartAgent,
    WatchScreens,
}

/// Right side: one team's board.
fn board(ui: &mut egui::Ui, ts: &mut TeamsUi, store: &crate::team::TeamStore, team: &Team, screens: usize) -> Option<BoardAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.menu_button(RichText::new(icon::ELLIPSIS).size(18.0).color(theme::body()), |ui| {
                if ui.button(tr("이름 바꾸기")).clicked() {
                    ts.dialog = Some(Dialog::rename(team));
                    ui.close();
                }
                if ui.button(RichText::new(tr("팀 삭제")).color(theme::danger())).clicked() {
                    ts.dialog = Some(Dialog::delete(team));
                    ui.close();
                }
            })
            .response
            .on_hover_text(tr("더 보기"));
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(RichText::new(&team.name).size(18.0).strong()).truncate()).on_hover_text(&team.name);
            });
        });
    });
    if !team.goal.is_empty() {
        ui.label(RichText::new(&team.goal).color(theme::body()));
    }
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        theme::section(ui, trf("참여 에이전트 {}명", &[&team.members.len()]).as_str());
        for member in &team.members {
            ui.label(theme::hint(member.label()));
        }
    });
    if team.members.is_empty() {
        ui.label(theme::hint(tr("팀 에이전트를 시작하거나, 참여 요청을 연결된 외부 AI 앱에 붙여넣으세요")));
    }
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        if theme::primary(ui, tr("팀 에이전트 시작")).clicked() {
            action = Some(BoardAction::StartAgent);
        }
        let copy_key = ui.scope_id().with("copied_team_join_request");
        let now = ui.input(|input| input.time);
        if theme::secondary(ui, tr("참여 요청 복사")).clicked() {
            ui.ctx().copy_text(trf("GhostDesk 팀 ‘{}’에 참여해서 목표와 할 일을 확인해 줘", &[&team.name]));
            ui.ctx().data_mut(|data| data.insert_temp(copy_key, now + 2.0));
            ui.ctx().request_repaint_after(std::time::Duration::from_secs(2));
        }

        let btn = ui.add_enabled_ui(screens > 0, |ui| theme::secondary(ui, format!("{}  {}", icon::LAYOUT_DASHBOARD, tr("팀 화면 모아보기")))).inner;
        let hint = if screens == 0 { tr("팀원이 연 화면이 아직 없어요").to_string() } else { trf("팀원 화면 {}개", &[&screens]) };
        if btn.on_disabled_hover_text(&hint).on_hover_text(hint).clicked() {
            action = Some(BoardAction::WatchScreens);
        }

        if ui.ctx().data(|data| data.get_temp::<f64>(copy_key)).is_some_and(|until| until > now) {
            ui.label(theme::hint(tr("참여 요청을 복사했어요")));
        }
    });
    ui.add_space(12.0);

    let files: Vec<_> = team.file_edits().collect();
    if files.is_empty() {
        ui.label(theme::hint(tr("수정 중인 파일 없음")));
    } else {
        egui::CollapsingHeader::new(trf("수정 중인 파일 {}개", &[&files.len()]))
            .id_salt("file_edits")
            .default_open(true)
            .show(ui, |ui| {
                for (path, owner) in files {
                    ui.horizontal_wrapped(|ui| {
                        ui.add(egui::Label::new(RichText::new(path).monospace()).truncate()).on_hover_text(path);
                        ui.label(theme::hint(owner.label()));
                    });
                }
            });
    }
    ui.add_space(12.0);

    let draft = ts.drafts.entry(team.id).or_default();
    if ui.available_width() < BOARD_MIN_W {
        tasks_column(ui, draft, store, team);
        ui.add_space(16.0);
        ui.separator();
        ui.add_space(16.0);
        notes_column(ui, draft, store, team);
    } else {
        ui.spacing_mut().item_spacing.x = COLUMN_GAP;
        ui.columns(2, |columns| {
            tasks_column(&mut columns[0], draft, store, team);
            notes_column(&mut columns[1], draft, store, team);
        });
    }
    action
}

fn composer(ui: &mut egui::Ui, text: &mut String, hint: &str, button: &str) -> bool {
    let mut submit = false;
    ui.horizontal(|ui| {
        let edit = ui.add(
            egui::TextEdit::singleline(text).desired_width((ui.available_width() - 88.0).max(48.0)).margin(Margin::symmetric(10, 6)).hint_text(hint),
        );
        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let clicked = ui.add_enabled_ui(!text.trim().is_empty(), |ui| theme::secondary(ui, button)).inner.clicked();
        submit = (enter || clicked) && !text.trim().is_empty();
        if submit {
            edit.request_focus();
        }
    });
    submit
}

fn tasks_column(ui: &mut egui::Ui, draft: &mut BoardDraft, store: &crate::team::TeamStore, team: &Team) {
    ui.horizontal(|ui| {
        theme::section(ui, tr("할 일"));
        ui.label(theme::hint(trf("남은 일 {}개", &[&team.open_tasks()])));
    });
    if composer(ui, &mut draft.task, tr("할 일 추가 (Enter)"), tr("추가")) {
        match store.task_add(team.id, None, &draft.task, "") {
            Ok(_) => {
                draft.task.clear();
                draft.task_error = None;
            }
            Err(e) => draft.task_error = Some(e.to_string()),
        }
    }
    if let Some(error) = &draft.task_error {
        ui.label(RichText::new(error).small().color(theme::danger()));
    }
    ui.add_space(8.0);
    if team.tasks.is_empty() {
        ui.label(theme::hint(tr("아직 할 일이 없어요")));
    }
    let mut tasks: Vec<&Task> = team.tasks.iter().collect();
    tasks.sort_by_key(|t| matches!(t.state, TaskState::Done { .. }));
    for task in tasks {
        ui.push_id(task.id, |ui| task_row(ui, store, team, task, &mut draft.task_error));
    }
}

fn task_row(ui: &mut egui::Ui, store: &crate::team::TeamStore, team: &Team, t: &Task, error: &mut Option<String>) {
    theme::card_frame().inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                ui.menu_button(RichText::new(icon::ELLIPSIS).color(theme::muted()), |ui| {
                    if !matches!(t.state, TaskState::Open) && ui.button(tr("다시 열기")).clicked() {
                        *error = store.task_release(team.id, None, t.id).err().map(|e| e.to_string());
                        ui.close();
                    }
                    if ui.button(RichText::new(tr("할 일 지우기")).color(theme::danger())).clicked() {
                        *error = store.task_delete(team.id, t.id).err().map(|e| e.to_string());
                        ui.close();
                    }
                })
                .response
                .on_hover_text(tr("더 보기"));
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                    let done = matches!(t.state, TaskState::Done { .. });
                    let title = RichText::new(&t.title).color(if done { theme::muted() } else { theme::text() });
                    ui.add(egui::Label::new(title).wrap()).on_hover_text(format!("#{}", t.id));
                });
            });
        });
        match &t.state {
            TaskState::Open => {
                ui.label(theme::hint(tr("아무도 맡지 않음")));
            }
            TaskState::Claimed { by, .. } => {
                theme::chip(ui, theme::warn(), &trf("{} 이(가) 하는 중", &[by]));
            }
            TaskState::Done { by, result } => {
                theme::chip(ui, theme::success(), &trf("{} 완료", &[by]));
                if !result.is_empty() {
                    ui.add(egui::Label::new(RichText::new(result).size(12.0).color(theme::body())).wrap());
                }
            }
        }
        if !t.detail.is_empty() {
            ui.add(egui::Label::new(RichText::new(&t.detail).size(12.0).color(theme::muted())).wrap());
        }
    });
    ui.add_space(4.0);
}

fn notes_column(ui: &mut egui::Ui, draft: &mut BoardDraft, store: &crate::team::TeamStore, team: &Team) {
    ui.horizontal(|ui| {
        theme::section(ui, tr("메모"));
        theme::count_badge(ui, &team.notes.len().to_string());
    });
    if composer(ui, &mut draft.note, tr("팀에 메모 남기기 (Enter)"), tr("보내기")) {
        match store.note(team.id, None, &draft.note) {
            Ok(_) => {
                draft.note.clear();
                draft.note_error = None;
            }
            Err(e) => draft.note_error = Some(e.to_string()),
        }
    }
    if let Some(error) = &draft.note_error {
        ui.label(RichText::new(error).small().color(theme::danger()));
    }
    ui.add_space(8.0);
    if team.notes.is_empty() {
        ui.label(theme::hint(tr("에이전트가 알아낸 것과 결정이 여기에 쌓여요")));
    }
    for note in team.notes.iter().rev() {
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::Label::new(RichText::new(&note.author).size(12.0).color(theme::text())).truncate()).on_hover_text(&note.author);
            ui.label(RichText::new(ago(note.at)).size(11.0).color(theme::muted()));
        });
        ui.add(egui::Label::new(RichText::new(&note.text).color(theme::body())).wrap());
        ui.add_space(8.0);
    }
}

/// The delete confirmation and rename popups.
fn dialog(ctx: &egui::Context, ts: &mut TeamsUi, store: &crate::team::TeamStore) {
    let Some(d) = &mut ts.dialog else { return };
    let mut close = false;
    let m = egui::Modal::new(egui::Id::unique("team_dialog")).frame(theme::modal_frame()).show(ctx, |ui| {
        ui.set_width(400.0);
        match d {
            Dialog::Delete { id, name, notes, tasks } => {
                close |= theme::modal_header(ui, tr("팀 삭제"));
                ui.label(RichText::new(trf("'{}' 팀을 삭제할까요?", &[name])).color(theme::text()));
                ui.add_space(4.0);
                ui.label(theme::hint(trf(
                    "메모 {}개와 할 일 {}개가 함께 지워지고 되돌릴 수 없어요. 팀에 들어와 있는 에이전트는 팀에서 빠져요",
                    &[notes, tasks],
                )));
                ui.add_space(16.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::destructive(ui, tr("삭제")).clicked() {
                        ts.error = store.delete(*id).err().map(|e| e.to_string());
                        close = true;
                    }
                    if theme::secondary(ui, tr("취소")).clicked() {
                        close = true;
                    }
                });
            }
            Dialog::Rename { id, name, error } => {
                close |= theme::modal_header(ui, tr("팀 이름 바꾸기"));
                let edit = ui.add(
                    egui::TextEdit::singleline(name).desired_width(ui.available_width()).margin(Margin::symmetric(10, 6)).hint_text(tr("팀 이름")),
                );
                if !edit.has_focus() && error.is_none() && ui.memory(|m| m.focused().is_none()) {
                    edit.request_focus();
                }
                let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.label(theme::hint(tr("에이전트는 새 이름으로 이 팀을 찾아요")));
                if let Some(e) = error {
                    ui.label(RichText::new(e.as_str()).size(12.0).color(theme::danger()));
                }
                ui.add_space(16.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::primary(ui, tr("바꾸기")).clicked() || enter {
                        match store.rename(*id, name) {
                            Ok(()) => close = true,
                            Err(e) => *error = Some(format!("{e:#}")),
                        }
                    }
                    if theme::secondary(ui, tr("취소")).clicked() {
                        close = true;
                    }
                });
            }
        }
    });
    if close || m.should_close() {
        ts.dialog = None;
    }
}

/// How long ago, in words ("방금", "5분 전", "2시간 전", "3일 전").
fn ago(unix: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(unix);
    let s = now.saturating_sub(unix);
    match s {
        0..=59 => tr("방금").to_string(),
        60..=3599 => trf("{}분 전", &[&(s / 60)]),
        3600..=86_399 => trf("{}시간 전", &[&(s / 3600)]),
        _ => trf("{}일 전", &[&(s / 86_400)]),
    }
}
