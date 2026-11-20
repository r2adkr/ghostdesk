//! Settings → AI → "pi": detect it, install it (after the user confirms the exact command), and choose
//! whether key-based AI agents run through it.
use super::*;

impl GhostApp {
    pub(super) fn pi_section(&mut self, ui: &mut egui::Ui) {
        // a finished install
        if let Some(rx) = &self.pi_install {
            match rx.try_recv() {
                Ok(result) => {
                    self.pi_install = None;
                    match result {
                        Ok(()) if crate::pi::find().is_some() => {
                            self.use_pi = true;
                            self.pi_note = Some(tr("pi 를 설치했어요").to_string());
                        }
                        Ok(()) => self.pi_note = Some(tr("설치는 끝났지만 pi 를 찾지 못했어요. 앱을 다시 시작해 보세요").to_string()),
                        Err(e) => self.pi_note = Some(e),
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => ui.ctx().request_repaint_after(std::time::Duration::from_millis(300)),
                Err(_) => self.pi_install = None,
            }
        }
        theme::section(ui, tr("pi 로 실행"));
        ui.label(theme::hint(tr("API 키로 쓰는 AI 는 pi 가 더 잘 해내요. pi 가 GhostDesk 화면을 직접 다뤄요 (Claude Code CLI·Codex CLI 는 해당 없음)")));
        ui.add_space(4.0);
        let installed = crate::pi::find().is_some();
        if installed {
            ui.label(RichText::new(trf("{}  pi 가 설치되어 있어요", &[&icon::CIRCLE_CHECK])).color(theme::body()));
            ui.add(theme::switch(&mut self.use_pi, tr("API 키로 쓰는 AI 는 pi 로 실행")));
            ui.label(theme::hint(tr("끄면 GhostDesk 자체 방식으로 실행해요. 새로 시작하는 에이전트부터 적용돼요")));
        } else if self.pi_install.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(tr("pi 설치 중… (몇 분 걸릴 수 있어요)"));
            });
        } else if !self.pi_confirm {
            ui.label(tr("pi 가 아직 설치되어 있지 않아요"));
            if theme::primary(ui, tr("pi 설치하기")).clicked() {
                self.pi_confirm = true;
                self.pi_note = None;
            }
        } else {
            ui.label(RichText::new(tr("아래 명령을 이 컴퓨터에서 실행해 pi 를 설치해요. Node.js 가 없으면 함께 설치될 수 있어요")).color(theme::warn()));
            ui.add_space(4.0);
            ui.label(RichText::new(crate::pi::install_command()).monospace());
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if theme::primary(ui, tr("설치 진행")).clicked() {
                    self.pi_confirm = false;
                    let (tx, rx) = std::sync::mpsc::channel();
                    self.pi_install = Some(rx);
                    let ctx = ui.ctx().clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(crate::pi::install());
                        ctx.request_repaint();
                    });
                }
                if theme::secondary(ui, tr("취소")).clicked() {
                    self.pi_confirm = false;
                }
            });
        }
        if let Some(note) = &self.pi_note {
            ui.label(theme::hint(note.clone()));
        }
    }
}
