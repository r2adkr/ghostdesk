//! Settings → AI → "pi": detect it, install it (after the user confirms the exact command), and choose
//! whether key-based AI agents run through it.
use super::*;

impl GhostApp {
    pub(super) fn pi_section(&mut self, ui: &mut egui::Ui) {
        self.poll_pi_install(ui.ctx());
        theme::section(ui, tr("pi 로 실행"));
        ui.add_space(4.0);
        let installed = crate::pi::find().is_some();
        if installed {
            ui.add(theme::switch(&mut self.use_pi, tr("API 키로 쓰는 AI 는 pi 로 실행")))
                .on_hover_text(tr("pi 가 설치되어 있어요. 새로 시작하는 에이전트부터 적용돼요"));
        } else if self.pi_install.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(tr("pi 설치 중… (몇 분 걸릴 수 있어요)"));
            });
        } else {
            ui.horizontal(|ui| {
                if theme::primary(ui, tr("pi 설치하기")).clicked() {
                    self.start_pi_install(ui.ctx());
                }
                ui.label(theme::hint(tr("API 키로 쓰는 AI 를 더 잘 쓰게 해줘요")));
            })
            .response
            .on_hover_text(tr("버튼 한 번으로 설치해요. Node.js 가 없으면 Homebrew 로 함께 설치해요 (몇 분 걸려요)"));
        }
        if let Some(note) = &self.pi_note {
            ui.label(theme::hint(note.clone()));
        }
    }

    /// Picks up a finished background install (turns pi on after a successful one).
    pub(super) fn poll_pi_install(&mut self, ctx: &egui::Context) {
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
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(300)),
                Err(_) => self.pi_install = None,
            }
        }
    }

    pub(super) fn start_pi_install(&mut self, ctx: &egui::Context) {
        self.pi_note = None;
        let (tx, rx) = std::sync::mpsc::channel();
        self.pi_install = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::pi::install());
            ctx.request_repaint();
        });
    }

    /// In the new-agent dialog: for an AI that can use pi while pi is missing, offer the install once.
    pub(super) fn pi_offer(&mut self, ui: &mut egui::Ui) {
        self.poll_pi_install(ui.ctx());
        let supported = self.cur().is_some_and(|c| crate::pi::supports(c.provider));
        if !supported || crate::pi::find().is_some() || self.pi_offer_dismissed {
            if let Some(note) = &self.pi_note {
                ui.label(theme::hint(note.clone()));
            }
            return;
        }
        ui.add_space(14.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if self.pi_install.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(tr("pi 설치 중… (몇 분 걸릴 수 있어요)"));
                });
                return;
            }
            ui.label(RichText::new(tr("이 AI 는 pi 로 실행하면 더 잘해요")).strong().color(theme::text()));
            ui.label(theme::hint(tr("버튼 한 번으로 설치해요. Node.js 가 없으면 Homebrew 로 함께 설치해요 (몇 분 걸려요)")));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if theme::primary(ui, tr("pi 설치하기")).clicked() {
                    self.start_pi_install(ui.ctx());
                }
                if theme::secondary(ui, tr("나중에")).clicked() {
                    self.pi_offer_dismissed = true;
                    self.save_prefs();
                }
            });
            if let Some(note) = &self.pi_note {
                ui.label(RichText::new(note.clone()).size(12.0).color(theme::warn()));
            }
        });
    }
}
