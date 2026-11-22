//! Saved remote connections: the "Saved" tab of the Remote computer form, and the "save this connection?" popup
//! shown after a connection was used for the first time.
//!
//! The list lives in its own file (`saved-remotes.json`) so it never races the preferences file. Secrets (a VNC
//! password, a host pairing code) are kept out of that file: in the login keychain on macOS, sealed with DPAPI on
//! Windows, and not saved at all elsewhere.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub(super) struct SavedRemote {
    pub id: String,
    pub name: String,
    /// `host` (pairing code), `vnc`, `ssh` or `both`.
    pub kind: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub ssh: crate::ssh::SshOptions,
    /// Windows only: the secrets, sealed with DPAPI.
    pub sealed: String,
}

impl Default for SavedRemote {
    fn default() -> Self {
        SavedRemote { id: String::new(), name: String::new(), kind: "vnc".into(), host: String::new(), port: 5900, user: String::new(), ssh: Default::default(), sealed: String::new() }
    }
}

#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
struct Secrets {
    pass: String,
    host_code: String,
}

fn list_path() -> std::path::PathBuf {
    crate::platform::paths::data_root().join("saved-remotes.json")
}

pub(super) fn load() -> Vec<SavedRemote> {
    let mut v: Vec<SavedRemote> = std::fs::read(list_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    v.retain(|r| !r.id.is_empty() && !r.name.trim().is_empty());
    v
}

fn store(list: &[SavedRemote]) -> anyhow::Result<()> {
    let path = list_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().expect("has a parent"))?;
    serde_json::to_writer_pretty(tmp.as_file_mut(), list)?;
    tmp.as_file().sync_all()?;
    tmp.persist(&path)?;
    Ok(())
}

// ---- secrets ----

#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "GhostDesk saved connection";

/// Keeps the secrets of entry `r` somewhere safe. `Ok(false)` = this OS has no safe place, so only the rest was saved.
fn put_secrets(r: &mut SavedRemote, s: &Secrets) -> anyhow::Result<bool> {
    if s == &Secrets::default() {
        return Ok(true);
    }
    let json = serde_json::to_string(s)?;
    #[cfg(target_os = "macos")]
    {
        let st = std::process::Command::new("/usr/bin/security")
            .args(["add-generic-password", "-U", "-a", &r.id, "-s", KEYCHAIN_SERVICE, "-w", &json])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        anyhow::ensure!(st.success(), "keychain refused the secret");
        Ok(true)
    }
    #[cfg(not(target_os = "macos"))]
    {
        match super::prefs::secret::seal(&json) {
            Some(sealed) => {
                r.sealed = sealed;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

fn get_secrets(r: &SavedRemote) -> Secrets {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/usr/bin/security")
            .args(["find-generic-password", "-a", &r.id, "-s", KEYCHAIN_SERVICE, "-w"])
            .stderr(std::process::Stdio::null())
            .output();
        out.ok().filter(|o| o.status.success()).and_then(|o| serde_json::from_slice(String::from_utf8_lossy(&o.stdout).trim().as_bytes()).ok()).unwrap_or_default()
    }
    #[cfg(not(target_os = "macos"))]
    {
        super::prefs::secret::open(&r.sealed).and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
    }
}

fn delete_secrets(_r: &SavedRemote) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("/usr/bin/security")
        .args(["delete-generic-password", "-a", &_r.id, "-s", KEYCHAIN_SERVICE])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn new_id() -> String {
    // unique enough for a handful of entries: time plus a counter
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{t:x}-{}", N.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

// ---- form ↔ entry ----

impl NewAgentForm {
    /// What the form would connect to, as a saved entry (without a name). `None` while it is empty.
    pub(super) fn to_saved(&self) -> Option<SavedRemote> {
        if self.host_code.is_some() {
            return Some(SavedRemote { kind: "host".into(), ..Default::default() });
        }
        let ssh = self.ssh_options();
        match self.remote_mode {
            RemoteMode::Ssh => {
                let o = ssh?;
                (!o.host.trim().is_empty()).then(|| SavedRemote { kind: "ssh".into(), host: o.host.clone(), port: o.port, user: o.username.clone(), ssh: o, ..Default::default() })
            }
            mode => {
                if self.remote_host.trim().is_empty() {
                    return None;
                }
                Some(SavedRemote {
                    kind: if mode == RemoteMode::Both { "both" } else { "vnc" }.into(),
                    host: self.remote_host.trim().into(),
                    port: self.remote_port,
                    user: self.remote_user.trim().into(),
                    ssh: ssh.unwrap_or_default(),
                    ..Default::default()
                })
            }
        }
    }

    fn secrets(&self) -> Secrets {
        Secrets { pass: self.remote_pass.clone(), host_code: self.host_code.clone().unwrap_or_default() }
    }

    /// Fills the form from a saved entry (and its secrets).
    pub(super) fn apply_saved(&mut self, r: &SavedRemote) {
        let s = get_secrets(r);
        self.clear_vnc_authorization();
        self.host_code = None;
        self.ssh_profile = None;
        self.ssh = Default::default();
        self.remote_host.clear();
        self.remote_user.clear();
        self.remote_port = 5900;
        match r.kind.as_str() {
            "host" => {
                self.host_code = Some(s.host_code).filter(|c| !c.is_empty());
                self.remote_mode = RemoteMode::Vnc;
            }
            "ssh" => {
                self.remote_mode = RemoteMode::Ssh;
                self.ssh = r.ssh.clone();
            }
            kind => {
                self.remote_mode = if kind == "both" { RemoteMode::Both } else { RemoteMode::Vnc };
                self.remote_host = r.host.clone();
                self.remote_port = r.port;
                self.remote_user = r.user.clone();
                self.remote_pass = s.pass;
                if kind == "both" {
                    self.ssh = r.ssh.clone();
                }
            }
        }
        self.saved_id = Some(r.id.clone());
        self.remote_entry = RemoteEntry::Imported;
    }
}

impl SavedRemote {
    /// One line for lists.
    fn summary(&self) -> String {
        match self.kind.as_str() {
            "host" => tr("호스트 컴퓨터 (접속 코드)").to_string(),
            "ssh" => format!("SSH {}{}", if self.user.is_empty() { String::new() } else { format!("{}@", self.user) }, self.host),
            k => format!("{}{}:{}{}", if self.user.is_empty() { String::new() } else { format!("{}@", self.user) }, self.host, self.port, if k == "both" { " · SSH" } else { "" }),
        }
    }

    /// Same destination (secrets and the name don't count).
    fn same_target(&self, o: &SavedRemote) -> bool {
        (&self.kind, &self.host, self.port, &self.user, &self.ssh) == (&o.kind, &o.host, o.port, &o.user, &o.ssh)
    }
}

/// The "save this connection?" popup.
pub(super) struct SavePrompt {
    entry: SavedRemote,
    secrets: Secrets,
    name: String,
    error: Option<String>,
}

impl GhostApp {
    /// The "Saved" tab: pick a saved connection, or delete one.
    pub(super) fn saved_remotes_tab(&mut self, ui: &mut egui::Ui) {
        if self.saved_remotes.is_empty() {
            ui.label(theme::hint(tr("저장된 연결이 아직 없어요. 연결을 한 번 쓰고 나면 저장할지 물어봐요.")));
            return;
        }
        let mut pick = None;
        let mut remove = None;
        egui::ScrollArea::vertical().max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
            for (i, r) in self.saved_remotes.iter().enumerate() {
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 40.0).max(120.0);
                    let resp = ui
                        .allocate_ui_with_layout(egui::vec2(w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                            ui.set_width(w);
                            let name = ui.add(egui::Label::new(RichText::new(&r.name).strong()).truncate().sense(egui::Sense::click()));
                            let sum = r.summary();
                            ui.add(egui::Label::new(theme::hint(&sum)).truncate());
                            name
                        })
                        .inner;
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        pick = Some(i);
                    }
                    if theme::icon_button(ui, icon::X, tr("삭제")).clicked() {
                        remove = Some(i);
                    }
                });
                ui.add_space(4.0);
            }
        });
        if let Some(i) = pick {
            let r = self.saved_remotes[i].clone();
            self.form.apply_saved(&r);
        }
        if let Some(i) = remove {
            let r = self.saved_remotes.remove(i);
            delete_secrets(&r);
            let _ = store(&self.saved_remotes);
        }
    }

    /// After an agent was started from the Remote form: offers to save the connection when it is new.
    pub(super) fn offer_save_remote(&mut self) {
        if self.form.saved_id.is_some() {
            return;
        }
        let Some(entry) = self.form.to_saved() else { return };
        if self.saved_remotes.iter().any(|r| r.same_target(&entry)) && entry.kind != "host" {
            return;
        }
        let name = match entry.kind.as_str() {
            "host" => tr("호스트 컴퓨터").to_string(),
            _ => entry.host.clone(),
        };
        self.save_prompt = Some(SavePrompt { entry, secrets: self.form.secrets(), name, error: None });
    }

    pub(super) fn save_remote_window(&mut self, ctx: &egui::Context) {
        let Some(mut prompt) = self.save_prompt.take() else { return };
        let mut keep = true;
        let mut save = false;
        egui::Modal::new(egui::Id::unique("save_remote")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(420.0);
            keep &= !theme::modal_header(ui, tr("이 연결 정보를 저장할까요?"));
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.label(theme::hint(prompt.entry.summary()));
            ui.horizontal(|ui| {
                ui.label(tr("이름"));
                ui.add(egui::TextEdit::singleline(&mut prompt.name).desired_width(260.0));
            });
            ui.label(theme::hint(tr("다음에는 '저장된 정보'에서 골라 바로 쓸 수 있어요. 비밀번호와 접속 코드는 이 컴퓨터의 안전한 저장소에 따로 보관해요.")));
            if let Some(e) = &prompt.error {
                ui.colored_label(theme::danger(), e);
            }
            ui.add_space(4.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled_ui(!prompt.name.trim().is_empty(), |ui| theme::primary(ui, tr("저장"))).inner.clicked() {
                    save = true;
                }
                if theme::secondary(ui, tr("저장 안 함")).clicked() {
                    keep = false;
                }
            });
        });
        if save {
            let mut entry = prompt.entry.clone();
            entry.id = new_id();
            entry.name = prompt.name.trim().to_string();
            match put_secrets(&mut entry, &prompt.secrets) {
                Ok(saved_secrets) => {
                    self.saved_remotes.push(entry);
                    match store(&self.saved_remotes) {
                        Ok(()) => {
                            self.notify(ctx, if saved_secrets { tr("연결 정보를 저장했어요") } else { tr("연결 정보를 저장했어요. 비밀번호는 이 컴퓨터에 저장할 수 없어서 빠졌어요") });
                            return;
                        }
                        Err(e) => {
                            self.saved_remotes.pop();
                            prompt.error = Some(format!("{e:#}"));
                        }
                    }
                }
                Err(e) => prompt.error = Some(format!("{e:#}")),
            }
        }
        if keep {
            self.save_prompt = Some(prompt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_round_trips_through_a_saved_entry_without_its_secrets() {
        let mut form = NewAgentForm::new(None);
        form.remote_mode = RemoteMode::Vnc;
        form.remote_host = " vm.local ".into();
        form.remote_port = 5901;
        form.remote_user = "me".into();
        form.remote_pass = "secret".into();
        let e = form.to_saved().unwrap();
        assert_eq!((e.kind.as_str(), e.host.as_str(), e.port, e.user.as_str()), ("vnc", "vm.local", 5901, "me"));
        assert!(!serde_json::to_string(&e).unwrap().contains("secret"), "the password never goes into the list file");
        assert!(e.same_target(&e.clone()));
        let mut other = e.clone();
        other.port = 5902;
        assert!(!e.same_target(&other));
        // an empty form has nothing to save
        assert!(NewAgentForm::new(None).to_saved().is_none());
        let mut host = NewAgentForm::new(None);
        host.host_code = Some("ghostdesk:host:v1:x".into());
        assert_eq!(host.to_saved().unwrap().kind, "host");
    }
}
