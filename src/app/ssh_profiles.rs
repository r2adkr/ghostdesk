//! Saved SSH endpoints shared by Settings and the new-agent wizard.

use super::*;
use crate::ssh::{InvalidSshOption, SshOptions};
use std::collections::BTreeMap;

pub(super) type Profiles = BTreeMap<String, SshOptions>;

#[derive(Default)]
pub(super) struct Editor {
    original: Option<String>,
    name: String,
    options: SshOptions,
    error: Option<String>,
    confirm_delete: bool,
    code: String,
}

pub(super) fn trim_options(options: &mut SshOptions) {
    options.host = options.host.trim().to_owned();
    options.username = options.username.trim().to_owned();
    options.identity_file = options.identity_file.trim().to_owned();
}

pub(super) fn validation_message(error: InvalidSshOption) -> &'static str {
    match error {
        InvalidSshOption::Host => tr("SSH 주소에는 호스트 이름이나 IP만 입력하고, 포트는 SSH 포트 칸에 입력하세요"),
        InvalidSshOption::Port => tr("SSH 포트는 1부터 65535 사이로 입력하세요"),
        InvalidSshOption::Username => tr("SSH 사용자 이름을 확인하세요"),
        InvalidSshOption::KeyPath => tr("개인 키 파일 경로를 확인하세요"),
    }
}

impl Editor {
    fn updated_profiles(&self, current: &Profiles) -> anyhow::Result<Profiles> {
        let name = self.name.trim();
        anyhow::ensure!(!name.is_empty(), "{}", tr("연결 이름을 입력하세요"));
        anyhow::ensure!(self.original.as_deref() == Some(name) || !current.contains_key(name), "{}", tr("이미 사용 중인 연결 이름이에요"));
        let mut options = self.options.clone();
        trim_options(&mut options);
        options.validate().map_err(|e| anyhow::anyhow!(validation_message(e)))?;

        let mut profiles = current.clone();
        if let Some(original) = &self.original {
            profiles.remove(original);
        }
        profiles.insert(name.to_owned(), options);
        Ok(profiles)
    }
}

pub(super) fn fields(ui: &mut egui::Ui, options: &mut SshOptions, host_hint: &str) {
    egui::Grid::new("ssh_fields").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label(tr("SSH 주소"));
        ui.add(egui::TextEdit::singleline(&mut options.host).hint_text(host_hint));
        ui.end_row();
        ui.label(tr("SSH 포트"));
        ui.add(egui::DragValue::new(&mut options.port).range(1..=65535));
        ui.end_row();
        ui.label(tr("SSH 사용자"));
        ui.add(egui::TextEdit::singleline(&mut options.username).hint_text(tr("선택 사항")));
        ui.end_row();
        ui.label(tr("개인 키 경로"));
        ui.add(egui::TextEdit::singleline(&mut options.identity_file).hint_text(tr("선택 사항")));
        ui.end_row();
    });
}

impl GhostApp {
    pub(super) fn settings_ssh(&mut self, ui: &mut egui::Ui) {
        if self.ssh_editor.is_none() {
            self.settings_host(ui);
            self.share_connection_info(ui);
            ui.add_space(14.0);
            self.settings_live_fps(ui);
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);
        }
        let title = match &self.ssh_editor {
            None => tr("저장된 SSH 연결"),
            Some(editor) if editor.original.is_some() => tr("SSH 연결 편집"),
            Some(_) => tr("SSH 연결 추가"),
        };
        theme::section(ui, title);
        ui.add_space(6.0);
        let Some(editor) = &mut self.ssh_editor else {
            if theme::primary(ui, tr("SSH 연결 추가")).clicked() {
                self.ssh_editor = Some(Editor::default());
            }
            ui.add_space(6.0);
            if self.ssh_profiles.is_empty() {
                ui.label(theme::hint(tr("저장한 연결이 없어요. 추가해 두면 새 에이전트에서 바로 고를 수 있어요")));
            }
            for (name, options) in &self.ssh_profiles {
                ui.horizontal(|ui| {
                    let width = (ui.available_width() - 80.0).max(100.0);
                    ui.allocate_ui_with_layout(egui::vec2(width, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.set_width(width);
                        ui.add(egui::Label::new(egui::RichText::new(name).strong()).truncate()).on_hover_text(name);
                        let endpoint = options.endpoint();
                        ui.add(egui::Label::new(theme::hint(&endpoint)).truncate()).on_hover_text(endpoint);
                    });
                    ui.push_id(name, |ui| {
                        if theme::secondary(ui, tr("편집")).clicked() {
                            self.ssh_editor = Some(Editor {
                                name: name.clone(), original: Some(name.clone()), options: options.clone(), ..Default::default()
                            });
                        }
                    });
                });
                ui.separator();
            }
            return;
        };

        if !editor.confirm_delete {
            ui.horizontal(|ui| {
                ui.label(tr("이름"));
                ui.text_edit_singleline(&mut editor.name);
            });
            fields(ui, &mut editor.options, tr("예) 192.168.0.20 또는 vm.local"));
            // adding: a connection code from the other computer fills the form
            if editor.original.is_none() {
                ui.add_space(6.0);
                ui.label(theme::hint(tr("또는 연결 코드로 채우기")));
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 90.0).max(120.0);
                    ui.add(egui::TextEdit::singleline(&mut editor.code).desired_width(w).hint_text(tr("연결 코드를 붙여 넣으세요")));
                    if theme::secondary(ui, tr("코드로 채우기")).clicked() {
                        match super::remote_info::ssh_from_code(&editor.code) {
                            Ok((name, options)) => {
                                if editor.name.trim().is_empty() {
                                    editor.name = name;
                                }
                                editor.options = options;
                                editor.code.clear();
                                editor.error = None;
                            }
                            Err(why) => editor.error = Some(tr(why).to_string()),
                        }
                    }
                });
            }
        }
        if let Some(error) = &editor.error {
            ui.colored_label(theme::danger(), error);
        }
        let mut update = None;
        let mut cancel = false;
        if let Some(original) = editor.original.as_ref().filter(|_| editor.confirm_delete) {
            ui.label(trf("‘{}’ 연결을 삭제할까요?", &[original]));
            ui.horizontal(|ui| {
                if theme::secondary(ui, tr("취소")).clicked() {
                    editor.confirm_delete = false;
                }
                if theme::destructive(ui, tr("삭제")).clicked() {
                    let mut profiles = self.ssh_profiles.clone();
                    profiles.remove(original);
                    update = Some(Ok(profiles));
                }
            });
        } else {
            ui.horizontal(|ui| {
                if theme::primary(ui, tr("저장")).clicked() {
                    update = Some(editor.updated_profiles(&self.ssh_profiles));
                }
                cancel = theme::secondary(ui, tr("취소")).clicked();
                if editor.original.is_some() && theme::secondary(ui, tr("삭제")).clicked() {
                    editor.confirm_delete = true;
                    editor.error = None;
                }
            });
        }
        if cancel {
            self.ssh_editor = None;
        } else if let Some(update) = update {
            match update.and_then(|profiles| self.save_ssh_profiles(profiles)) {
                Ok(()) => self.ssh_editor = None,
                Err(error) => {
                    if let Some(editor) = &mut self.ssh_editor {
                        editor.error = Some(error.to_string());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_a_profile_preserves_other_hosts_and_rejects_conflicting_names() {
        let mut editor = Editor { name: " 개발 ".into(), options: SshOptions { host: " dev ".into(), port: 2222, ..Default::default() }, ..Default::default() };
        let mut profiles = editor.updated_profiles(&Profiles::new()).unwrap();
        assert_eq!(profiles["개발"].host, "dev");
        profiles.insert("build".into(), SshOptions { host: "other".into(), ..Default::default() });
        editor.original = Some("개발".into());
        editor.name = "build".into();
        assert!(editor.updated_profiles(&profiles).is_err());
        editor.name = "lab".into();
        let renamed = editor.updated_profiles(&profiles).unwrap();
        assert!(!renamed.contains_key("개발"));
        assert_eq!(renamed["lab"].port, 2222);
        assert_eq!(renamed["build"], profiles["build"]);
        editor.options.host.clear();
        assert!(editor.updated_profiles(&renamed).is_err());
    }
}
