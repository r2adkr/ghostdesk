//! Settings → Remote connection → "Lend this computer": the host switch, the pairing code, replacing the code.
use super::*;
use std::sync::mpsc::{channel, Receiver, TryRecvError};

#[derive(Default)]
pub(super) struct HostUi {
    /// Whether the login item is installed (read once, then kept in step with the switch).
    on: Option<bool>,
    /// A switch change running in the background: the state it is moving to, and its answer.
    busy: Option<(bool, Receiver<Result<(), String>>)>,
    code: Option<String>,
    show: bool,
    error: Option<String>,
}

impl GhostApp {
    pub(super) fn settings_host(&mut self, ui: &mut egui::Ui) {
        let ui_state = &mut self.host_ui;
        let on = *ui_state.on.get_or_insert_with(crate::host::autostart::enabled);
        // finish a switch change
        if let Some((target, rx)) = &ui_state.busy {
            match rx.try_recv() {
                Ok(Ok(())) => {
                    ui_state.on = Some(*target);
                    ui_state.code = None;
                    ui_state.error = None;
                    ui_state.busy = None;
                }
                Ok(Err(e)) => {
                    ui_state.error = Some(e);
                    ui_state.busy = None;
                }
                Err(TryRecvError::Empty) => ui.ctx().request_repaint_after(std::time::Duration::from_millis(200)),
                Err(TryRecvError::Disconnected) => ui_state.busy = None,
            }
        }
        theme::section(ui, tr("이 컴퓨터를 호스트로"));
        ui.label(theme::hint(tr("켜 두면 다른 곳의 GhostDesk 가 접속 코드로 이 컴퓨터의 화면을 쓸 수 있어요. VPN 이나 포트 설정은 필요 없어요.")));
        ui.add_space(6.0);
        let busy = ui_state.busy.is_some();
        let mut want = ui_state.busy.as_ref().map(|(t, _)| *t).unwrap_or(on);
        let changed = ui.add_enabled(!busy, theme::switch(&mut want, tr("호스트 켜기 (로그인할 때 자동 시작)"))).changed();
        if busy {
            ui.horizontal(|ui| {
                theme::spinner(ui, 13.0, theme::muted());
                ui.label(theme::hint(tr("적용하는 중…")));
            });
        }
        if changed {
            let (tx, rx) = channel();
            std::thread::spawn(move || {
                let _ = tx.send(crate::host::autostart::set(want).map_err(|e| format!("{e:#}")));
            });
            ui_state.busy = Some((want, rx));
            ui_state.error = None;
        }
        if let Some(e) = &ui_state.error {
            ui.colored_label(theme::danger(), e);
        }
        if !on || busy {
            ui.add_space(8.0);
            return;
        }
        ui.add_space(6.0);
        if ui_state.code.is_none() {
            ui_state.code = crate::host::pairing_code().ok();
        }
        let Some(code) = ui_state.code.clone() else { return };
        ui.horizontal(|ui| {
            ui.label(tr("접속 코드"));
            let shown = if ui_state.show { code.clone() } else { "•".repeat(24) };
            ui.add(egui::Label::new(theme::hint(shown)).truncate());
        });
        let mut copied = false;
        let mut renew = false;
        ui.horizontal(|ui| {
            if theme::primary(ui, tr("코드 복사")).clicked() {
                ui.ctx().copy_text(code.clone());
                copied = true;
            }
            if theme::secondary(ui, if ui_state.show { tr("숨기기") } else { tr("보기") }).clicked() {
                ui_state.show = !ui_state.show;
            }
            renew = theme::secondary(ui, tr("새 코드 만들기")).on_hover_text(tr("지금까지 준 코드는 모두 쓸 수 없게 돼요. 호스트가 다시 시작돼요.")).clicked();
        });
        ui.label(theme::hint(tr("코드를 가진 사람은 이 컴퓨터를 조작할 수 있어요. 비밀번호처럼 다루세요.")));
        if renew {
            let (tx, rx) = channel();
            std::thread::spawn(move || {
                let r = crate::host::regenerate_token().and_then(|_| crate::host::autostart::set(true));
                let _ = tx.send(r.map_err(|e| format!("{e:#}")));
            });
            ui_state.busy = Some((true, rx));
        }
        if copied {
            self.notify(ui.ctx(), tr("접속 코드를 복사했어요"));
        }
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);
    }
}
