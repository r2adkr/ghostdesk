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
    note: String,
    task: String,
    /// team id whose delete button was pressed once (press again to delete)
    confirm_delete: Option<u64>,
    /// copy of the store, refreshed when it changes
    cache: Vec<Team>,
    cache_version: Option<u64>,
    error: Option<String>,
}

const LIST_W: f32 = 240.0;

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
        let store = self.teams.clone();
        let v = store.version();
        if self.teams_ui.cache_version != Some(v) {
            self.teams_ui.cache = store.snapshot();
            self.teams_ui.cache_version = Some(v);
        }
        let ts = &mut self.teams_ui;
        if ts.selected.is_none_or(|id| !ts.cache.iter().any(|t| t.id == id)) {
            ts.selected = ts.cache.first().map(|t| t.id);
        }

        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("팀")).size(20.0).strong().color(theme::text()));
            ui.label(RichText::new(ts.cache.len().to_string()).size(14.0).color(theme::muted()));
        });
        ui.label(theme::hint(tr("여러 에이전트가 목표·메모·할 일을 함께 보며 분야를 나눠 일해요. 화면은 에이전트마다 따로예요")));
        ui.add_space(16.0);

        let full = ui.available_rect_before_wrap();
        let list_rect = egui::Rect::from_min_size(full.min, egui::vec2(LIST_W, full.height()));
        let board_rect = egui::Rect::from_min_max(egui::pos2(full.min.x + LIST_W + 24.0, full.min.y), full.max);

        let mut list_ui = ui.new_child(egui::UiBuilder::new().max_rect(list_rect).layout(egui::Layout::top_down(egui::Align::Min)));
        team_list(&mut list_ui, ts, &store);
        let mut board_ui = ui.new_child(egui::UiBuilder::new().max_rect(board_rect).layout(egui::Layout::top_down(egui::Align::Min)));
        match ts.cache.iter().find(|t| Some(t.id) == ts.selected).cloned() {
            Some(team) => board(&mut board_ui, ts, &store, &team),
            None => empty_board(&mut board_ui),
        }
        ui.allocate_rect(full, egui::Sense::hover());
    }
}

/// Left column: the teams, and a form to create one.
fn team_list(ui: &mut egui::Ui, ts: &mut TeamsUi, store: &crate::team::TeamStore) {
    ui.spacing_mut().item_spacing.y = 2.0;
    for t in &ts.cache {
        let selected = ts.selected == Some(t.id);
        let r = theme::nav_item(ui, selected, icon::LIST_TODO, &t.name, (!t.members.is_empty()).then(theme::success));
        if r.clicked() {
            ts.selected = Some(t.id);
            ts.confirm_delete = None;
            ts.error = None;
        }
    }
    ui.add_space(8.0);
    if ts.creating {
        ui.spacing_mut().item_spacing.y = 6.0;
        let name = ui.add(
            egui::TextEdit::singleline(&mut ts.new_name)
                .desired_width(LIST_W)
                .margin(Margin::symmetric(10, 6))
                .hint_text(tr("팀 이름")),
        );
        if ts.new_name.is_empty() && !name.has_focus() {
            name.request_focus();
        }
        ui.add(
            egui::TextEdit::multiline(&mut ts.new_goal)
                .desired_width(LIST_W)
                .desired_rows(2)
                .margin(Margin::symmetric(10, 6))
                .hint_text(tr("목표 (예: 다음 주 제주 출장 일정과 예약 정리)")),
        );
        ui.horizontal(|ui| {
            if theme::primary(ui, tr("만들기")).clicked() || (name.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                match store.create(&ts.new_name, &ts.new_goal, USER) {
                    Ok(id) => {
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
    } else if theme::nav_item(ui, false, icon::PLUS, tr("새 팀"), None).clicked() {
        ts.creating = true;
    }
    if let Some(e) = &ts.error {
        ui.label(RichText::new(e).size(12.0).color(theme::danger()));
    }
}

fn empty_board(ui: &mut egui::Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.25).max(24.0));
        ui.label(RichText::new(tr("아직 팀이 없어요")).size(20.0).color(theme::text()));
        ui.add_space(8.0);
        ui.label(theme::hint(tr("왼쪽 '새 팀'으로 만들거나, AI 앱에게 팀을 만들어 같이 일하라고 하세요")));
    });
}

/// Right side: one team's board.
fn board(ui: &mut egui::Ui, ts: &mut TeamsUi, store: &crate::team::TeamStore, team: &Team) {
    // header: name · delete
    ui.horizontal(|ui| {
        ui.label(RichText::new(&team.name).size(17.0).color(theme::text()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let confirming = ts.confirm_delete == Some(team.id);
            let label = if confirming { tr("한 번 더 누르면 삭제") } else { tr("팀 삭제") };
            if theme::secondary(ui, label).on_hover_text(tr("메모와 할 일이 함께 지워져요")).clicked() {
                if confirming {
                    let _ = store.delete(&team.id.to_string());
                    ts.confirm_delete = None;
                } else {
                    ts.confirm_delete = Some(team.id);
                }
            }
        });
    });
    if !team.goal.is_empty() {
        ui.label(RichText::new(&team.goal).color(theme::body()));
    }
    ui.add_space(6.0);
    // members
    ui.horizontal_wrapped(|ui| {
        if team.members.is_empty() {
            ui.label(theme::hint(trf("참여한 에이전트가 없어요. AI 앱에게 \"GhostDesk 팀 '{}' 에 들어가서 …를 맡아줘\" 라고 하세요", &[&team.name])));
        } else {
            for m in &team.members {
                theme::chip(ui, theme::success(), &m.label());
            }
        }
    });
    ui.add_space(12.0);

    let avail = ui.available_size();
    let col_w = ((avail.x - 24.0) / 2.0).max(200.0);
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(egui::vec2(col_w, avail.y), egui::Layout::top_down(egui::Align::Min), |ui| tasks_column(ui, ts, store, team));
        ui.add_space(24.0);
        ui.allocate_ui_with_layout(egui::vec2(col_w, avail.y), egui::Layout::top_down(egui::Align::Min), |ui| notes_column(ui, ts, store, team));
    });
}

fn tasks_column(ui: &mut egui::Ui, ts: &mut TeamsUi, store: &crate::team::TeamStore, team: &Team) {
    ui.horizontal(|ui| {
        theme::section(ui, tr("할 일"));
        theme::count_badge(ui, &team.open_tasks().to_string());
    });
    ui.add_space(4.0);
    let add = ui.add(
        egui::TextEdit::singleline(&mut ts.task)
            .desired_width(ui.available_width())
            .margin(Margin::symmetric(10, 6))
            .hint_text(tr("할 일 추가 (Enter)")),
    );
    if add.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !ts.task.trim().is_empty() {
        if store.task_add(&team.id.to_string(), None, &ts.task, "").is_ok() {
            ts.task.clear();
        }
        add.request_focus();
    }
    ui.add_space(6.0);
    let h = ui.available_height();
    egui::ScrollArea::vertical().id_salt(("team_tasks", team.id)).max_height(h).auto_shrink([false, true]).show(ui, |ui| {
        if team.tasks.is_empty() {
            ui.label(theme::hint(tr("아직 할 일이 없어요")));
        }
        // open and claimed first, finished at the end
        let mut tasks: Vec<&Task> = team.tasks.iter().collect();
        tasks.sort_by_key(|t| matches!(t.state, TaskState::Done { .. }));
        for t in tasks {
            task_row(ui, store, team, t);
        }
    });
}

fn task_row(ui: &mut egui::Ui, store: &crate::team::TeamStore, team: &Team, t: &Task) {
    let key = team.id.to_string();
    theme::card_frame().inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let done = matches!(t.state, TaskState::Done { .. });
            let title = RichText::new(format!("{}  {}", t.id, t.title)).color(if done { theme::muted() } else { theme::text() });
            ui.add(egui::Label::new(if done { title.strikethrough() } else { title }).wrap());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(egui::Button::new(RichText::new(icon::X).size(13.0).color(theme::muted())).frame_when_inactive(false)).on_hover_text(tr("할 일 지우기")).clicked() {
                    let _ = store.task_delete(&key, t.id);
                }
                if !matches!(t.state, TaskState::Open) && ui.link(RichText::new(tr("다시 열기")).size(12.0)).clicked() {
                    let _ = store.task_release(&key, None, t.id);
                }
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

fn notes_column(ui: &mut egui::Ui, ts: &mut TeamsUi, store: &crate::team::TeamStore, team: &Team) {
    ui.horizontal(|ui| {
        theme::section(ui, tr("메모"));
        theme::count_badge(ui, &team.notes.len().to_string());
    });
    ui.add_space(4.0);
    let add = ui.add(
        egui::TextEdit::singleline(&mut ts.note)
            .desired_width(ui.available_width())
            .margin(Margin::symmetric(10, 6))
            .hint_text(tr("팀에 메모 남기기 (Enter)")),
    );
    if add.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !ts.note.trim().is_empty() {
        if store.note(&team.id.to_string(), None, &ts.note).is_ok() {
            ts.note.clear();
        }
        add.request_focus();
    }
    ui.add_space(6.0);
    let h = ui.available_height();
    egui::ScrollArea::vertical()
        .id_salt(("team_notes", team.id))
        .max_height(h)
        .auto_shrink([false, true])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            if team.notes.is_empty() {
                ui.label(theme::hint(tr("에이전트가 알아낸 것과 결정이 여기에 쌓여요")));
            }
            for n in &team.notes {
                ui.horizontal(|ui| {
                    let who = if n.author == USER { theme::accent() } else { theme::text() };
                    ui.label(RichText::new(&n.author).size(12.0).color(who));
                    ui.label(RichText::new(ago(n.at)).size(11.0).color(theme::muted()));
                });
                ui.add(egui::Label::new(RichText::new(&n.text).color(theme::body())).wrap());
                ui.add_space(8.0);
            }
        });
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
