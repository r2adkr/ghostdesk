//! Creating the dedicated agent account (macOS): the question, the wait for macOS's administrator
//! prompt, and the one-time password with what to do next. See `crate::agent_account`.

use std::sync::Arc;

use parking_lot::Mutex;

use super::*;

type Slot = Arc<Mutex<Option<Result<String, String>>>>;

enum Step {
    /// Asking whether to make it
    Ask,
    /// macOS is asking for the administrator password; the result arrives in the slot
    Working(Slot),
    /// Made: the password is shown until the window is closed
    Done(String),
    Failed(String),
}

pub(super) struct AccountPrompt {
    name: String,
    step: Step,
    /// The password is also kept in the keychain (so the shown warning can say it can be looked at again)
    saved: bool,
}

impl AccountPrompt {
    /// The question (the New agent dialog asked for a session whose account is missing).
    pub(super) fn ask(name: &str) -> Self {
        AccountPrompt { name: name.to_string(), step: Step::Ask, saved: false }
    }

    /// An agent's request made the account; show how to log in to it.
    pub(super) fn created(name: String, password: String) -> Self {
        let saved = crate::agent_account::load_password(&name).is_some();
        AccountPrompt { name, step: Step::Done(password), saved }
    }

    /// Shows the saved password of an existing account (Err: none is saved).
    pub(super) fn show(name: &str) -> Option<Self> {
        let password = crate::agent_account::load_password(name)?;
        Some(AccountPrompt { name: name.to_string(), step: Step::Done(password), saved: true })
    }
}

impl GhostApp {
    pub(super) fn account_window(&mut self, ctx: &egui::Context) {
        let Some(mut prompt) = self.account_prompt.take() else { return };
        let mut keep = true;
        let mut create = false;
        let mut open_guide = false;
        if let Step::Working(slot) = &prompt.step {
            let done = slot.lock().take();
            match done {
                Some(Ok(password)) => {
                    prompt.saved = crate::agent_account::load_password(&prompt.name).is_some();
                    prompt.step = Step::Done(password)
                }
                Some(Err(e)) => prompt.step = Step::Failed(e),
                None => ctx.request_repaint_after(std::time::Duration::from_millis(250)),
            }
        }
        let name = prompt.name.clone();
        let modal = egui::Modal::new(egui::Id::unique("agent_account")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(520.0);
            keep &= !theme::modal_header(ui, tr("에이전트 전용 계정"));
            ui.spacing_mut().item_spacing.y = 8.0;
            match &prompt.step {
                Step::Ask => {
                    ui.label(trf("별도 계정 세션을 쓰려면 이 Mac 에 전용 사용자 '{}' 이(가) 필요해요. 만들까요?", &[&name]));
                    ui.label(theme::hint(tr("관리자가 아닌 표준 계정으로 만들어요. macOS 가 관리자 암호를 물어보며, GhostDesk 는 그 암호를 보지 못해요.")));
                    ui.label(theme::hint(tr("만든 뒤에는 그 계정으로 한 번 로그인해 세션을 시작해야 해요.")));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::primary(ui, tr("계정 만들기")).clicked() {
                                create = true;
                            }
                            if theme::secondary(ui, tr("취소")).clicked() {
                                keep = false;
                            }
                        });
                    });
                }
                Step::Working(_) => {
                    ui.horizontal(|ui| {
                        theme::spinner(ui, 14.0, theme::muted());
                        ui.label(tr("macOS 의 관리자 암호 창을 확인하세요…"));
                    });
                }
                Step::Done(password) => {
                    ui.label(trf("'{}' 계정의 로그인 암호예요.", &[&name]));
                    theme::card_frame().inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(password).monospace().size(16.0).color(theme::text()));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if theme::secondary(ui, tr("복사")).clicked() {
                                    ui.ctx().copy_text(password.clone());
                                    self.toast = Some((tr("암호를 복사했어요").to_string(), ui.input(|i| i.time) + TOAST_SECS));
                                }
                            });
                        });
                    });
                    if prompt.saved {
                        ui.label(theme::hint(tr("암호는 키체인에 저장했어요. 설정 화면의 '암호 보기'로 언제든 다시 볼 수 있어요.")));
                    } else {
                        ui.label(RichText::new(tr("이 창을 닫으면 다시 볼 수 없어요. 잊으면 시스템 설정 > 사용자 및 그룹에서 암호를 다시 정할 수 있어요.")).small().color(theme::warn()));
                    }
                    ui.add_space(4.0);
                    theme::section(ui, tr("다음 순서"));
                    ui.label(trf("1. 메뉴 막대의 빠른 사용자 전환(제어 센터)에서 '{}' 를 고르고 위 암호로 로그인해요.", &[&name]));
                    ui.label(tr("2. 처음 나오는 설정 화면은 건너뛰어도 돼요."));
                    ui.label(tr("3. 설정 안내의 LaunchAgent 와 권한 단계를 한 번 마치고, 다시 내 계정으로 돌아와요. 이 계정에서는 로그아웃하지 마세요."));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::primary(ui, tr("닫기")).clicked() {
                                keep = false;
                            }
                            if theme::secondary(ui, tr("설정 안내 열기")).clicked() {
                                open_guide = true;
                                keep = false;
                            }
                        });
                    });
                }
                Step::Failed(e) => {
                    ui.label(RichText::new(e).color(theme::danger()));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::primary(ui, tr("다시 시도")).clicked() {
                                create = true;
                            }
                            if theme::secondary(ui, tr("닫기")).clicked() {
                                keep = false;
                            }
                        });
                    });
                }
            }
        });
        if modal.should_close() && !matches!(prompt.step, Step::Working(_)) {
            keep = false;
        }
        if create {
            let slot: Slot = Arc::new(Mutex::new(None));
            let (s2, ctx2, name2) = (slot.clone(), ctx.clone(), prompt.name.clone());
            self.rt.spawn(async move {
                let r = crate::agent_account::create(&name2).await.map_err(|e| format!("{e:#}"));
                *s2.lock() = Some(r);
                ctx2.request_repaint();
            });
            prompt.step = Step::Working(slot);
        }
        if open_guide {
            self.show_session_guide = true;
        }
        if keep {
            self.account_prompt = Some(prompt);
        }
    }
}

// ---- importing a browser login into the agent account ----

enum ImportStep {
    Ask,
    Working(Slot),
    Done(String),
    Failed(String),
}

/// The dialog that copies one browser profile's login into an agent account (macOS). Browser logins are
/// copied; native apps are logged in once inside the agent account by the person.
pub(super) struct ImportPrompt {
    account: String,
    browser: usize,
    profile: usize,
    step: ImportStep,
}

impl ImportPrompt {
    pub(super) fn new(account: &str) -> Self {
        ImportPrompt { account: account.to_string(), browser: 0, profile: 0, step: ImportStep::Ask }
    }
}

impl GhostApp {
    pub(super) fn import_window(&mut self, ctx: &egui::Context) {
        let Some(mut prompt) = self.import_prompt.take() else { return };
        let mut keep = true;
        let mut start = false;
        if let ImportStep::Working(slot) = &prompt.step {
            let done = slot.lock().take();
            match done {
                Some(Ok(msg)) => prompt.step = ImportStep::Done(msg),
                Some(Err(e)) => prompt.step = ImportStep::Failed(e),
                None => ctx.request_repaint_after(std::time::Duration::from_millis(250)),
            }
        }
        prompt.browser = prompt.browser.min(self.browsers.len().saturating_sub(1));
        let account = prompt.account.clone();
        let modal = egui::Modal::new(egui::Id::unique("agent_import")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(520.0);
            keep &= !theme::modal_header(ui, tr("브라우저 로그인 가져오기"));
            ui.spacing_mut().item_spacing.y = 8.0;
            match &prompt.step {
                ImportStep::Ask => {
                    if self.browsers.is_empty() {
                        ui.label(RichText::new(tr("Chromium 계열 브라우저를 찾지 못했어요")).color(theme::danger()));
                    } else {
                        ui.label(trf("내 브라우저의 로그인(쿠키)을 '{}' 계정의 같은 브라우저로 복사해요.", &[&account]));
                        ui.horizontal(|ui| {
                            ui.label(tr("브라우저"));
                            let names: Vec<String> = self.browsers.iter().map(|b| b.name.clone()).collect();
                            egui::ComboBox::from_id_salt("import_browser").icon(theme::combo_icon).selected_text(names[prompt.browser].clone()).show_ui(ui, |ui| {
                                for (i, n) in names.iter().enumerate() {
                                    if ui.selectable_value(&mut prompt.browser, i, n).clicked() {
                                        prompt.profile = 0;
                                    }
                                }
                            });
                        });
                        let profiles = self.browsers[prompt.browser].profiles.clone();
                        prompt.profile = prompt.profile.min(profiles.len().saturating_sub(1));
                        if !profiles.is_empty() {
                            ui.horizontal(|ui| {
                                ui.label(tr("프로필"));
                                let shown = super::labels::profile_short(&profiles[prompt.profile].name);
                                egui::ComboBox::from_id_salt("import_profile").icon(theme::combo_icon).selected_text(shown).show_ui(ui, |ui| {
                                    for (i, p) in profiles.iter().enumerate() {
                                        ui.selectable_value(&mut prompt.profile, i, &p.name);
                                    }
                                });
                            });
                        }
                        ui.label(theme::hint(tr("macOS 가 두 번 물어봐요. 먼저 내 로그인 암호(브라우저의 로그인 키를 읽을 때), 다음에 관리자 암호(그 계정에 쓸 때)예요. GhostDesk 는 두 암호를 보지 못해요.")));
                        ui.label(theme::hint(tr("그 계정에 로그인되어 있어야 하고, 그 계정의 같은 브라우저는 꺼져 있어야 해요. 그 계정에 이미 있던 이 브라우저의 데이터는 덮어써요.")));
                        ui.label(RichText::new(tr("에이전트가 이 사이트들에 내 로그인 그대로 들어갈 수 있게 돼요. 믿는 에이전트에게만 쓰세요.")).small().color(theme::warn()));
                        ui.label(theme::hint(tr("네이티브 앱은 가져올 수 없어요. 그 계정으로 로그인해서 앱에 한 번 직접 로그인해 두세요.")));
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add_enabled_ui(!self.browsers.is_empty(), |ui| theme::primary(ui, tr("가져오기"))).inner.clicked() {
                                start = true;
                            }
                            if theme::secondary(ui, tr("취소")).clicked() {
                                keep = false;
                            }
                        });
                    });
                }
                ImportStep::Working(_) => {
                    ui.horizontal(|ui| {
                        theme::spinner(ui, 14.0, theme::muted());
                        ui.label(tr("macOS 의 암호 창을 확인하세요…"));
                    });
                }
                ImportStep::Done(msg) => {
                    ui.label(RichText::new(msg).color(theme::success()));
                    ui.label(theme::hint(tr("그 계정의 세션에서 브라우저를 열면 로그인되어 있어요.")));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::primary(ui, tr("닫기")).clicked() {
                                keep = false;
                            }
                        });
                    });
                }
                ImportStep::Failed(e) => {
                    ui.label(RichText::new(e).color(theme::danger()));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::primary(ui, tr("다시 시도")).clicked() {
                                prompt.step = ImportStep::Ask;
                            }
                            if theme::secondary(ui, tr("닫기")).clicked() {
                                keep = false;
                            }
                        });
                    });
                }
            }
        });
        if modal.should_close() && !matches!(prompt.step, ImportStep::Working(_)) {
            keep = false;
        }
        if start {
            if let Some(b) = self.browsers.get(prompt.browser).cloned() {
                let pdir = b.profiles.get(prompt.profile).map(|p| p.dir.clone()).unwrap_or_else(|| "Default".into());
                let slot: Slot = Arc::new(Mutex::new(None));
                let (s2, ctx2, acc) = (slot.clone(), ctx.clone(), prompt.account.clone());
                self.rt.spawn(async move {
                    let r = crate::agent_account::import_browser_login(&acc, &b, &pdir).await.map_err(|e| format!("{e:#}"));
                    *s2.lock() = Some(r);
                    ctx2.request_repaint();
                });
                prompt.step = ImportStep::Working(slot);
            }
        }
        if keep {
            self.import_prompt = Some(prompt);
        }
    }
}
