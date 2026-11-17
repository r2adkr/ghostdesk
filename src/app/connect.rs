//! 외부 에이전트 연결 (Claude 앱·Claude Code·Codex 등에 GhostDesk MCP 등록). 파일 조회는 백그라운드에서 한다.

use super::*;

/// 연결 상태를 만들고 백그라운드에서 채운다 ('켤 때 자동으로 연결' 이 켜져 있으면 바로 연결까지)
pub(super) fn start(rt: &Handle, ctx: &egui::Context) -> Arc<parking_lot::Mutex<ConnectState>> {
    let auto = crate::install::auto_connect_enabled();
    let st = Arc::new(parking_lot::Mutex::new(ConnectState {
        auto,
        install_dir: crate::install::stable_app().display().to_string(),
        ..Default::default()
    }));
    if auto {
        spawn_connect(rt, st.clone(), ctx.clone());
    } else {
        refresh_connect_status(rt, st.clone(), ctx.clone());
    }
    st
}

/// 외부 에이전트 자동 연결 상태 (화면은 매 프레임 그리므로 파일·경로 조회가 필요한 값은 백그라운드에서 채워 둔다)
#[derive(Clone, Default)]
pub(super) struct ConnectState {
    busy: bool,
    clients: Vec<crate::install::ClientStatus>,
    /// clients 와 같은 순서: 지금 설치본으로 연결돼 있는가
    ok: Vec<bool>,
    messages: Vec<String>,
    /// '켤 때 자동으로 연결' 설정 (사용 설명 사진을 찍을 때는 tour.rs 가 화면에만 켜 둔다)
    pub(super) auto: bool,
    /// 설치 위치 (표시용)
    install_dir: String,
}

impl ConnectState {
    fn connected_any(&self) -> bool {
        self.ok.iter().any(|&b| b)
    }
}

impl GhostApp {
    /// '외부 연결' 단추의 상태 점: 연결 서버 오류면 빨강, 클라이언트가 하나라도 연결돼 있으면 초록, 아니면 없음.
    /// (연결 상태는 백그라운드에서 미리 계산해 둔다: 이 함수는 매 프레임 불린다)
    pub(super) fn connect_badge(&self) -> Option<Color32> {
        if self.control_error.lock().is_some() {
            return Some(theme::danger());
        }
        self.connect_state.lock().connected_any().then_some(theme::success())
    }

    /// 외부 연결 창을 연다. 다른 곳(터미널의 --connect 등)에서 바뀐 연결 상태도 보이게 새로 읽는다.
    pub(super) fn open_connect(&mut self, ctx: &egui::Context) {
        self.show_connect = true;
        if !self.connect_state.lock().busy {
            refresh_connect_status(&self.rt, self.connect_state.clone(), ctx.clone());
        }
    }

    /// 외부 에이전트 연결 (Claude 앱 · Claude Code · Codex)
    pub(super) fn connect_window(&mut self, ctx: &egui::Context) {
        if !self.show_connect {
            return;
        }
        let err = self.control_error.lock().clone();
        let st = self.connect_state.lock().clone();
        let max_h = (ctx.content_rect().height() - 220.0).max(220.0);
        let mut close = false;
        let mut auto_err = None;
        let m = egui::Modal::new(egui::Id::unique("connect")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(560.0);
            close |= theme::modal_header(ui, tr("외부 에이전트 연결"));
            theme::modal_scroll(ui, max_h, |ui| {
                ui.label(RichText::new(tr("연결하면 그 AI 앱에게 \"GhostDesk 로 …해줘\" 라고 말하면 돼요. API 키는 필요 없어요.")).color(theme::body()));
                if let Some(e) = &err {
                    theme::tinted(theme::danger()).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(trf("연결 서버 오류: {}", &[&e])).color(theme::danger()));
                    });
                }
                ui.add_space(4.0);
                for (i, c) in st.clients.iter().enumerate() {
                    let ok = st.ok.get(i).copied().unwrap_or(false);
                    theme::card_frame().inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(c.client.name()).strong().color(theme::text()));
                            let (txt, col) = match (&c.registered, c.installed) {
                                (Some(_), _) if ok => (tr("연결됨"), theme::success()),
                                (Some(_), _) => (tr("다른 경로로 연결됨"), theme::warn()),
                                (None, true) => (tr("연결 안 됨"), theme::muted()),
                                (None, false) => (tr("설치 안 됨"), theme::muted()),
                            };
                            theme::chip(ui, col, txt);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if c.registered.is_some() && ui.add_enabled_ui(!st.busy, |ui| theme::secondary(ui, tr("해제"))).inner.clicked() {
                                    run_client_action(&self.rt, self.connect_state.clone(), ui.ctx().clone(), Some(c.client), false);
                                }
                                if c.installed && !ok && ui.add_enabled_ui(!st.busy, |ui| theme::secondary(ui, tr("연결"))).inner.clicked() {
                                    run_client_action(&self.rt, self.connect_state.clone(), ui.ctx().clone(), Some(c.client), true);
                                }
                            });
                        });
                    });
                }
                if st.clients.is_empty() {
                    ui.horizontal(|ui| {
                        theme::spinner(ui, 14.0, theme::muted());
                        ui.label(theme::hint(tr("연결 상태를 확인하는 중…")));
                    });
                }
                self.other_agents_card(ui, st.busy);
                if st.busy {
                    ui.horizontal(|ui| {
                        theme::spinner(ui, 14.0, theme::muted());
                        ui.label(tr("연결하는 중…"));
                    });
                }
                for m in &st.messages {
                    ui.label(theme::hint(m.clone()));
                }
                ui.add_space(2.0);
                egui::CollapsingHeader::new(tr("직접 설정하기 (고급)")).default_open(false).show(ui, |ui| {
                    ui.label(theme::hint(trf("설치 위치: {} (연결은 이 경로를 씁니다)", &[&st.install_dir])));
                    let exe = crate::install::stable_exe().display().to_string();
                    for (title, snippet) in crate::mcp::setup_snippets(&exe) {
                        egui::Frame::new().fill(theme::bg()).corner_radius(theme::CARD_RADIUS).inner_margin(Margin::same(10)).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(title).strong().color(theme::text()));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.small_button(trf("{}  복사", &[&icon::COPY])).clicked() {
                                        ui.ctx().copy_text(snippet.clone());
                                    }
                                });
                            });
                            ui.add(egui::TextEdit::multiline(&mut snippet.as_str()).code_editor().desired_rows(1).desired_width(f32::INFINITY));
                        });
                    }
                    ui.label(theme::hint(tr("로그인 복사 · 켜 둔 앱 사용 · 원격 접속은 GhostDesk 창에서 한 번 더 허락해야 해요. GhostDesk 가 꺼져 있으면 저절로 켜져요.")));
                });
            });
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                let mut auto = st.auto;
                if ui.add(theme::switch(&mut auto, tr("켤 때 자동으로 연결"))).changed() {
                    match crate::install::set_auto_connect(auto) {
                        Ok(()) => self.connect_state.lock().auto = auto,
                        Err(e) => auto_err = Some(trf("자동 연결 설정을 저장하지 못했습니다: {}", &[&format!("{:#}", e)])),
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled_ui(!st.busy, |ui| theme::primary(ui, tr("  모두 연결  "))).inner.clicked() {
                        spawn_connect(&self.rt, self.connect_state.clone(), ui.ctx().clone());
                    }
                });
            });
        });
        if close || m.should_close() {
            self.show_connect = false;
        }
        if let Some(e) = auto_err {
            self.notify(ctx, e);
        }
    }
}

impl GhostApp {
    /// 목록에 없는 AI 앱: MCP 설정을 복사하거나, MCP 설정·스킬·플러그인을 담은 연결 패키지를 만든다
    fn other_agents_card(&mut self, ui: &mut egui::Ui, busy: bool) {
        theme::card_frame().inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(tr("다른 AI 앱")).strong().color(theme::text()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled_ui(!busy, |ui| theme::secondary(ui, tr("연결 패키지 만들기"))).inner.clicked() {
                        spawn_kit(&self.rt, self.connect_state.clone(), ui.ctx().clone());
                    }
                    if theme::secondary(ui, trf("{}  MCP 설정 복사", &[&icon::COPY])).clicked() {
                        let config = crate::kit::mcp_config(&crate::install::stable_exe());
                        ui.ctx().copy_text(serde_json::to_string_pretty(&config).unwrap_or_default());
                    }
                });
            });
            ui.label(theme::hint(tr("MCP 를 지원하는 앱이면 어디든 연결할 수 있어요. 설정을 복사해 그 앱의 MCP 서버 설정에 붙여 넣거나, 스킬·플러그인이 든 패키지를 만들어 쓰세요.")));
        });
    }
}

/// 연결 패키지를 내려받기 폴더에 만들고 Finder / 탐색기에서 연다 (백그라운드: 설치본 복사가 포함됨)
fn spawn_kit(rt: &Handle, st: Arc<parking_lot::Mutex<ConnectState>>, ctx: egui::Context) {
    st.lock().busy = true;
    rt.spawn_blocking(move || {
        let r = crate::install::install_app().and_then(|exe| crate::kit::write_kit(&crate::kit::default_parent(), &exe));
        let msg = match r {
            Ok(dir) => {
                crate::kit::reveal(&dir);
                trf("연결 패키지를 만들었어요: {}", &[&dir.display()])
            }
            Err(e) => trf("연결 패키지를 만들지 못했어요: {}", &[&format!("{:#}", e)]),
        };
        let mut s = st.lock();
        s.busy = false;
        s.messages = vec![msg];
        ctx.request_repaint();
    });
}

/// 클라이언트 상태와 표시용 값을 읽는다 (파일 조회 — UI 스레드에서 부르지 않는다)
pub(super) fn load_connect_status() -> ConnectState {
    let clients = crate::install::status_all();
    let exe = crate::install::stable_exe();
    ConnectState {
        ok: clients.iter().map(|c| c.registered.as_deref().is_some_and(|p| registered_is(p, &exe))).collect(),
        clients,
        install_dir: crate::install::stable_app().display().to_string(),
        ..Default::default()
    }
}

/// 읽어 온 상태를 반영한다 (진행 중 표시·메시지·자동 연결 설정은 그대로 — 자동 연결은 이 창의 스위치로만 바뀐다)
pub(super) fn apply_connect_status(dst: &mut ConnectState, src: ConnectState) {
    dst.clients = src.clients;
    dst.ok = src.ok;
    dst.install_dir = src.install_dir;
}

pub(super) fn refresh_connect_status(rt: &Handle, st: Arc<parking_lot::Mutex<ConnectState>>, ctx: egui::Context) {
    rt.spawn_blocking(move || {
        let fresh = load_connect_status();
        apply_connect_status(&mut st.lock(), fresh);
        ctx.request_repaint();
    });
}

/// 설치 + 모든 클라이언트 연결 (백그라운드)
pub(super) fn spawn_connect(rt: &Handle, st: Arc<parking_lot::Mutex<ConnectState>>, ctx: egui::Context) {
    st.lock().busy = true;
    rt.spawn_blocking(move || {
        let mut msgs = Vec::new();
        match crate::install::connect_all() {
            Ok((_, res)) => {
                // 성공은 각 앱 줄의 상태(연결됨 등)가 이미 보여 주므로 실패만 이유와 함께 적는다
                for (c, r) in res {
                    if let Err(e) = r {
                        msgs.push(trf("{}: 실패 — {}", &[&c.name(), &format!("{:#}", e)]));
                    }
                }
            }
            Err(e) => msgs.push(trf("설치 실패: {}", &[&format!("{:#}", e)])),
        }
        let fresh = load_connect_status();
        let mut s = st.lock();
        s.busy = false;
        apply_connect_status(&mut s, fresh);
        s.messages = msgs;
        ctx.request_repaint();
    });
}

/// 한 클라이언트 연결/해제 (백그라운드)
pub(super) fn run_client_action(rt: &Handle, st: Arc<parking_lot::Mutex<ConnectState>>, ctx: egui::Context, c: Option<crate::install::Client>, connect: bool) {
    st.lock().busy = true;
    rt.spawn_blocking(move || {
        let mut msgs = Vec::new();
        if let Some(c) = c {
            let r = if connect {
                crate::install::install_app().and_then(|exe| crate::install::register(c, &exe))
            } else {
                crate::install::unregister(c)
            };
            if let Err(e) = r {
                msgs.push(trf("{}: 실패 — {}", &[&c.name(), &format!("{:#}", e)]));
            }
        }
        let fresh = load_connect_status();
        let mut s = st.lock();
        s.busy = false;
        apply_connect_status(&mut s, fresh);
        s.messages = msgs;
        ctx.request_repaint();
    });
}

/// 클라이언트 설정에 등록된 실행 파일 경로가 지금 설치된 GhostDesk 인가.
/// macOS 는 기존대로 문자열 비교. Windows 는 대소문자·구분자·`\\?\` 접두사 차이를 무시하고,
/// 그래도 다르면 실제 파일이 같은지 본다 (드라이브 문자 대소문자 등으로 '다른 경로로 연결됨' 이 잘못 뜨지 않게).
pub(super) fn registered_is(registered: &str, exe: &std::path::Path) -> bool {
    if !cfg!(windows) {
        return registered == exe.display().to_string();
    }
    use crate::platform::paths;
    let norm = |p: &std::path::Path| paths::simplify(p).to_string_lossy().replace('/', "\\").to_lowercase();
    let reg = std::path::Path::new(registered.trim());
    norm(reg) == norm(exe) || paths::same_path(reg, exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_path_comparison() {
        use std::path::Path;
        let exe = if cfg!(windows) { Path::new(r"C:\Users\u\AppData\Local\Programs\GhostDesk\ghostdesk.exe") } else { Path::new("/Applications/GhostDesk.app/Contents/MacOS/ghostdesk") };
        assert!(registered_is(&exe.display().to_string(), exe));
        assert!(!registered_is("/somewhere/else/ghostdesk", exe));
        if cfg!(windows) {
            // 드라이브 문자 대소문자·구분자·verbatim 접두사 차이는 같은 경로
            assert!(registered_is(r"c:\users\u\appdata\local\programs\ghostdesk\GHOSTDESK.EXE", exe));
            assert!(registered_is("C:/Users/u/AppData/Local/Programs/GhostDesk/ghostdesk.exe", exe));
            assert!(registered_is(r"\\?\C:\Users\u\AppData\Local\Programs\GhostDesk\ghostdesk.exe", exe));
            assert!(!registered_is(r"C:\Users\u\Downloads\ghostdesk.exe", exe));
        } else {
            // macOS 는 기존대로 정확히 같은 문자열만
            assert!(!registered_is(&exe.display().to_string().to_uppercase(), exe));
        }
    }
}
