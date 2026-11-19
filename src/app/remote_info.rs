//! A connection card carries only the public fields needed to fill the remote-agent form.
use super::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::Duration;

const PREFIX: &str = "ghostdesk:connect:v1:";
const MAX_TEXT: usize = 2048;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Card {
    host: String,
    user: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ssh: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vnc: Option<u16>,
    /// VNC password, only when the sharer chose to include it (the text is not encrypted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pass: Option<String>,
}

impl Card {
    fn valid(&self) -> bool {
        let host_ok = match self.host.parse::<IpAddr>() {
            Ok(ip) => {
                let ip = ip.to_canonical();
                !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast() && ip != IpAddr::V4(Ipv4Addr::BROADCAST)
            }
            Err(_) => {
                let dns = self.host.strip_suffix('.').unwrap_or(&self.host);
                !dns.is_empty()
                    && self.host.len() <= 253
                    && !dns.eq_ignore_ascii_case("localhost")
                    && dns.split('.').all(|part| {
                        !part.is_empty()
                            && part.len() <= 63
                            && !part.starts_with('-')
                            && !part.ends_with('-')
                            && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                    })
                    && !dns.bytes().all(|b| b.is_ascii_digit() || b == b'.')
            }
        };
        host_ok
            && (self.ssh.is_some() || self.vnc.is_some())
            && self.ssh != Some(0)
            && self.vnc != Some(0)
            && self.user.len() <= 256
            && self.pass.as_ref().map_or(true, |p| !p.is_empty() && p.len() <= 128 && self.vnc.is_some() && !p.chars().any(char::is_control))
            && !self.user.starts_with('-')
            && self.user.chars().all(|c| c.is_alphanumeric() || "-_.\\".contains(c))
    }

    fn encode(&self) -> Option<String> {
        if !self.valid() {
            return None;
        }
        Some(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).ok()?)))
    }

    fn decode(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.len() > MAX_TEXT {
            return None;
        }
        let bytes = URL_SAFE_NO_PAD.decode(text.strip_prefix(PREFIX)?).ok()?;
        let card: Self = serde_json::from_slice(&bytes).ok()?;
        card.valid().then_some(card)
    }

    fn apply(self, form: &mut NewAgentForm) {
        form.remote_entry = RemoteEntry::Imported;
        form.remote_mode = match (self.ssh, self.vnc) {
            (Some(_), Some(_)) => RemoteMode::Both,
            (Some(_), None) => RemoteMode::Ssh,
            _ => RemoteMode::Vnc,
        };
        form.clear_vnc_authorization();
        form.ssh_profile = None;
        form.ssh = crate::ssh::SshOptions::default();
        form.remote_host.clear();
        form.remote_user.clear();
        form.remote_port = self.vnc.unwrap_or(5900);
        if let Some(pass) = self.pass.clone() {
            form.remote_pass = pass;
        }
        if self.vnc.is_some() {
            form.remote_host = self.host.clone();
            form.remote_user = self.user.clone();
        }
        if let Some(port) = self.ssh {
            form.ssh.port = port;
            form.ssh.username = self.user;
            // In combined mode both services follow the displayed VNC address.
            if self.vnc.is_none() {
                form.ssh.host = self.host;
            }
        }
    }
}

pub(super) struct Share {
    host: String,
    user: String,
    ssh: bool,
    ssh_port: u16,
    vnc: bool,
    vnc_port: u16,
    with_pass: bool,
    pass: String,
    detected: bool,
    /// Other addresses this computer can be reached at: `(label, address)`. The first is the home-network
    /// address, which only works from the same network.
    choices: Vec<(String, String)>,
}

/// This computer's VPN address (Tailscale), which works from any network. Empty when it isn't installed.
fn tailscale_ip() -> Option<String> {
    let exe = ["tailscale", "/Applications/Tailscale.app/Contents/MacOS/Tailscale", r"C:\Program Files\Tailscale\tailscale.exe"]
        .into_iter()
        .find(|c| std::process::Command::new(c).arg("version").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false))?;
    let out = std::process::Command::new(exe).args(["ip", "-4"]).stderr(std::process::Stdio::null()).output().ok()?;
    let ip = String::from_utf8_lossy(&out.stdout).lines().next()?.trim().to_string();
    ip.parse::<Ipv4Addr>().ok().map(|_| ip)
}

/// A public address looked up once from a plain-text echo service, only when the person asks.
async fn public_ip() -> Result<String, String> {
    let text = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| e.to_string())?
        .get("https://api.ipify.org")
        .send()
        .await
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let ip = text.trim().to_string();
    ip.parse::<IpAddr>().map(|_| ip).map_err(|_| "unexpected reply".to_string())
}

// Both protocols announce themselves before authentication. A different listener on the
// same port must not be offered as an available SSH or VNC service.
fn service_at(address: SocketAddr, prefix: &[u8; 4]) -> bool {
    let timeout = Duration::from_millis(500);
    let Ok(mut stream) = TcpStream::connect_timeout(&address, timeout) else { return false };
    let mut banner = [0; 4];
    stream.set_read_timeout(Some(timeout)).is_ok() && stream.read_exact(&mut banner).is_ok() && &banner == prefix
}

impl Share {
    fn detect() -> Self {
        // UDP connect only asks the kernel for its outgoing interface. It sends no data.
        let ip = [("0.0.0.0:0", "192.0.2.1:9"), ("[::]:0", "[2001:db8::1]:9")].into_iter().find_map(|(bind, route)| {
            let socket = UdpSocket::bind(bind).ok()?;
            socket.connect(route).ok()?;
            Some(socket.local_addr().ok()?.ip())
        });
        // Check only this computer's advertised interface, never other machines on the network.
        let ssh = ip.is_some_and(|ip| service_at(SocketAddr::new(ip, 22), b"SSH-"));
        let vnc = ip.is_some_and(|ip| service_at(SocketAddr::new(ip, 5900), b"RFB "));
        let lan = ip.map(|ip| ip.to_string()).unwrap_or_default();
        let mut choices = Vec::new();
        if !lan.is_empty() {
            choices.push((tr("같은 네트워크").to_string(), lan.clone()));
        }
        if let Some(vpn) = tailscale_ip() {
            choices.push((tr("VPN (Tailscale)").to_string(), vpn));
        }
        Self {
            choices,
            host: lan,
            user: std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).unwrap_or_default(),
            ssh,
            ssh_port: 22,
            vnc,
            vnc_port: 5900,
            with_pass: false,
            pass: String::new(),
            detected: ssh || vnc,
        }
    }

    fn card(&self) -> Card {
        Card {
            host: self.host.trim().into(),
            user: self.user.trim().into(),
            ssh: self.ssh.then_some(self.ssh_port),
            vnc: self.vnc.then_some(self.vnc_port),
            pass: (self.vnc && self.with_pass && !self.pass.is_empty()).then(|| self.pass.clone()),
        }
    }
}

impl GhostApp {
    pub(super) fn share_connection_info(&mut self, ui: &mut egui::Ui) {
        let Some(slot) = self.share_connection.as_ref() else {
            if theme::secondary(ui, tr("이 컴퓨터의 연결 정보 공유")).clicked() {
                self.share_connection = Some(wizard::load_in_background(&self.rt, ui.ctx(), Share::detect));
            }
            return;
        };
        let mut state = slot.lock();
        let Some(share) = state.as_mut() else {
            ui.label(tr("이 컴퓨터의 주소와 포트를 확인하는 중…"));
            return;
        };
        theme::section(ui, tr("이 컴퓨터의 연결 정보 공유"));
        ui.label(theme::hint(tr("상대방이 홈 → 원격 컴퓨터에 붙여넣으면 접속 정보가 채워져요.")));
        if !share.detected {
            ui.label(theme::hint(tr("기본 포트에서 서비스를 찾지 못했어요. 이 컴퓨터에서 원격 로그인(SSH)이나 화면 공유(VNC)를 켜거나 사용 중인 포트를 선택하세요.")));
        }
        egui::Grid::new("share_connection_fields").num_columns(2).show(ui, |ui| {
            ui.label(tr("주소"));
            ui.text_edit_singleline(&mut share.host);
            ui.end_row();
            ui.label(tr("사용자"));
            ui.text_edit_singleline(&mut share.user);
            ui.end_row();
            ui.checkbox(&mut share.ssh, tr("명령·파일 (SSH)"));
            ui.add_enabled(share.ssh, egui::DragValue::new(&mut share.ssh_port).range(1..=65535));
            ui.end_row();
            ui.checkbox(&mut share.vnc, tr("화면 (VNC)"));
            ui.add_enabled(share.vnc, egui::DragValue::new(&mut share.vnc_port).range(1..=65535));
            ui.end_row();
        });
        if share.vnc {
            ui.checkbox(&mut share.with_pass, tr("VNC 비밀번호도 함께 복사"));
            if share.with_pass {
                ui.add(egui::TextEdit::singleline(&mut share.pass).password(true));
                ui.label(theme::hint(tr("비밀번호는 암호화되지 않은 텍스트로 들어가요. 믿을 수 있는 사람에게만 보내세요.")));
            }
        }
        // which address to hand out: the home-network one only works from the same network
        ui.horizontal_wrapped(|ui| {
            let mut choices = share.choices.clone();
            if let Some(Ok(ip)) = self.public_ip.as_ref().and_then(|s| s.lock().clone()) {
                if !choices.iter().any(|(_, a)| *a == ip) {
                    choices.push((tr("공인 IP").to_string(), ip));
                }
            }
            for (label, addr) in &choices {
                if ui.selectable_label(share.host == *addr, format!("{label}  {addr}")).clicked() {
                    share.host = addr.clone();
                }
            }
            if self.public_ip.is_none() && theme::secondary(ui, tr("공인 IP 찾기")).on_hover_text(tr("다른 네트워크에서 접속할 때 필요해요. 공인 주소를 알려 주는 서비스에 한 번 물어봐요.")).clicked() {
                let slot = std::sync::Arc::new(parking_lot::Mutex::new(None));
                let (s2, ctx) = (slot.clone(), ui.ctx().clone());
                self.rt.spawn(async move {
                    *s2.lock() = Some(public_ip().await);
                    ctx.request_repaint();
                });
                self.public_ip = Some(slot);
            }
        });
        if let Some(Err(_)) = self.public_ip.as_ref().and_then(|s| s.lock().clone()) {
            ui.label(theme::hint(tr("공인 IP를 찾지 못했어요. 주소를 직접 입력하세요.")));
        }
        ui.label(theme::hint(tr("다른 네트워크에서 접속하려면 VPN(Tailscale 등) 주소를 쓰거나, 공인 IP로 공유기에서 포트를 이 컴퓨터로 전달해야 해요.")));
        let encoded = share.card().encode();
        let mut copied = false;
        let mut close = false;
        ui.horizontal(|ui| {
            if ui.add_enabled_ui(encoded.is_some(), |ui| theme::primary(ui, tr("연결 정보 복사"))).inner.clicked() {
                if let Some(text) = encoded.as_ref() {
                    ui.ctx().copy_text(text.clone());
                    copied = true;
                }
            }
            close = theme::secondary(ui, tr("닫기")).clicked();
        });
        if encoded.is_none() {
            ui.label(theme::hint(tr("접속 가능한 주소와 사용자 이름을 확인하고, 사용할 접속 방식을 선택하세요.")));
        }
        drop(state);
        if close || copied {
            self.share_connection = None;
            self.public_ip = None;
        }
        if copied {
            self.notify(ui.ctx(), tr("연결 정보를 복사했어요"));
        }
    }
}

impl NewAgentForm {
    pub(super) fn show_connection_summary(&mut self, ui: &mut egui::Ui) {
        if self.host_code.is_some() {
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::Label::new(RichText::new(tr("호스트 컴퓨터 (접속 코드)")).strong()).wrap());
                if ui.link(tr("연결 정보 수정")).clicked() {
                    self.host_code = None;
                    self.remote_entry = RemoteEntry::Paste;
                }
            });
            ui.label(theme::hint(tr("인터넷으로 연결해요. VPN 이나 포트 설정은 필요 없어요.")));
            return;
        }
        let has_vnc = self.remote_mode != RemoteMode::Ssh;
        let host = if has_vnc { &self.remote_host } else { &self.ssh.host };
        let user = if has_vnc { &self.remote_user } else { &self.ssh.username };
        let target = if user.is_empty() { host.clone() } else { format!("{user}@{host}") };
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::Label::new(RichText::new(&target).strong()).wrap());
            if ui.link(tr("연결 정보 수정")).clicked() {
                self.remote_entry = RemoteEntry::Manual;
            }
        });
        let ports = match self.remote_mode {
            RemoteMode::Both => format!("VNC {} · SSH {}", self.remote_port, self.ssh.port),
            RemoteMode::Vnc => format!("VNC {}", self.remote_port),
            RemoteMode::Ssh => format!("SSH {}", self.ssh.port),
        };
        ui.label(theme::hint(ports));
        if has_vnc {
            ui.horizontal(|ui| {
                ui.label(tr("VNC 비밀번호"));
                ui.add(egui::TextEdit::singleline(&mut self.remote_pass).password(true));
            });
        }
        if self.remote_mode != RemoteMode::Vnc {
            ui.label(theme::hint(tr("SSH 키 또는 에이전트로 인증해요. 호스트 키를 터미널에서 먼저 확인하세요")));
        }
    }

    pub(super) fn import_connection_info(&mut self, ui: &mut egui::Ui) -> bool {
        let mut apply = false;
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.card_input)
                    .id(ui.make_persistent_id("remote_card_input"))
                    .desired_width((ui.available_width() - 90.0).clamp(80.0, 360.0))
                    .hint_text(tr("상대방의 GhostDesk 연결 정보 붙여넣기")),
            );
            if field.changed() {
                self.card_invalid = false;
                apply = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))));
            }
            apply |= ui.add_enabled(!self.card_input.trim().is_empty(), egui::Button::new(tr("적용"))).clicked();
        });
        if apply {
            if crate::host::parse_code(&self.card_input).is_ok() {
                // a host computer's pairing code: no address, port or password to fill in
                self.host_code = Some(self.card_input.trim().to_string());
                self.remote_entry = RemoteEntry::Imported;
                self.card_input.clear();
                self.card_invalid = false;
                return true;
            }
            if let Some(card) = Card::decode(&self.card_input) {
                self.host_code = None;
                card.apply(self);
                self.card_input.clear();
                self.card_invalid = false;
                return true;
            } else {
                self.card_invalid = true;
            }
        }
        if self.card_invalid {
            ui.colored_label(theme::danger(), tr("연결 정보를 읽을 수 없어요. 상대방의 설정 → 원격 연결에서 다시 복사하세요."));
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vnc_password_travels_only_when_chosen_and_only_with_vnc() {
        let card = Card { host: "192.0.2.5".into(), user: "me".into(), ssh: None, vnc: Some(5900), pass: Some("s3cret".into()) };
        let loaded = Card::decode(&card.encode().unwrap()).unwrap();
        let mut form = NewAgentForm::new(None);
        loaded.apply(&mut form);
        assert_eq!(form.remote_pass, "s3cret");
        let ssh_only = Card { host: "192.0.2.5".into(), user: "me".into(), ssh: Some(22), vnc: None, pass: Some("x".into()) };
        assert!(ssh_only.encode().is_none(), "a password without VNC is refused");
        let none = Card { pass: None, ..card };
        assert!(!none.encode().unwrap().is_empty() && Card::decode(&none.encode().unwrap()).unwrap().pass.is_none());
    }

    #[test]
    fn sharing_and_applying_only_copies_public_connection_fields() {
        for (ssh, vnc, mode) in
            [(Some(2222), Some(5910), RemoteMode::Both), (Some(22), None, RemoteMode::Ssh), (None, Some(5900), RemoteMode::Vnc)]
        {
            let card = Card { host: "2001:db8::20".into(), user: "operator".into(), ssh, vnc, pass: None };
            let text = card.encode().unwrap();
            let mut form = NewAgentForm::new(None);
            form.remote_pass = "old password".into();
            form.remote_dedicated = true;
            form.ssh.identity_file = "old key path".into();
            form.ssh_profile = Some("old".into());
            let loaded = Card::decode(&format!(" {text}\n")).unwrap();
            assert_eq!(loaded, card);
            loaded.apply(&mut form);
            assert!(form.remote_mode == mode);
            assert!(form.remote_pass.is_empty() && !form.remote_dedicated && form.ssh_profile.is_none());
            assert!(form.ssh.identity_file.is_empty());
            if let Some(port) = ssh {
                let options = form.ssh_options().unwrap();
                assert_eq!((options.host, options.port, options.username), (card.host.clone(), port, card.user.clone()));
            } else {
                assert!(form.ssh_options().is_none());
            }
            if let Some(port) = vnc {
                assert_eq!((form.remote_host, form.remote_port, form.remote_user), (card.host, port, card.user));
            }
        }
    }

    #[test]
    fn paste_in_remote_form_applies_only_a_valid_card() {
        let ctx = egui::Context::default();
        let mut form = NewAgentForm::new(None);
        form.remote_host = "before".into();
        let mut frame = |events: Vec<egui::Event>| {
            let mut applied = false;
            let input = egui::RawInput { events, ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                applied |= form.import_connection_info(ui);
            });
            output.textures_delta.clear();
            (applied, form.card_invalid, form.remote_host.clone(), form.card_input.clone())
        };
        frame(vec![]);
        let pos = egui::pos2(22.0, 12.0);
        for pressed in [true, false] {
            frame(vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE },
            ]);
        }
        let card = Card { host: "dev.local".into(), user: "dev".into(), ssh: Some(2222), vnc: Some(5900), pass: None };
        let (applied, invalid, host, text) = frame(vec![egui::Event::Paste(card.encode().unwrap())]);
        assert!(applied && !invalid && text.is_empty());
        assert_eq!(host, "dev.local");
        let (applied, invalid, host, _) = frame(vec![egui::Event::Paste("not a card".into())]);
        assert!(!applied && invalid);
        assert_eq!(host, "dev.local");
    }

    #[test]
    fn detects_service_banner_on_the_local_port() {
        use std::{io::Write, net::TcpListener, thread};
        for (banner, expected) in [(b"SSH-", true), (b"RFB ", false)] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let sender = thread::spawn(move || listener.accept().unwrap().0.write_all(banner).unwrap());
            assert_eq!(service_at(address, b"SSH-"), expected);
            sender.join().unwrap();
        }
    }

    #[test]
    fn malformed_or_privileged_cards_are_rejected() {
        let invalid = [
            r#"{"host":"dev","user":"","ssh":22,"identity_file":"/keys/work"}"#,
            r#"{"host":"dev","user":"","vnc":5900,"dedicated_ack":true}"#,
            r#"{"host":"dev","host":"other","user":"","ssh":22}"#,
            r#"{"host":"dev","user":"","vnc":0}"#,
            r#"{"host":"dev","user":"","ssh":70000}"#,
            r#"{"host":"dev","user":""}"#,
            r#"{"host":"foo:2222","user":"","ssh":22}"#,
            r#"{"host":"127.0.0.1","user":"","ssh":22}"#,
            r#"{"host":"dev","user":"a@host","ssh":22}"#,
        ];
        for json in invalid {
            assert!(Card::decode(&format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(json))).is_none(), "{json}");
        }
        for text in [String::new(), format!("{PREFIX}$$"), format!("{PREFIX}{}", "A".repeat(MAX_TEXT))] {
            assert!(Card::decode(&text).is_none());
        }
    }
}
