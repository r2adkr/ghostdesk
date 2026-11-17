//! Windows installer and uninstaller, drawn with the app's own theme instead of a stock wizard.
//!
//! The release ships the GhostDesk executable itself as `GhostDesk-Setup-<version>-windows-x64.exe`.
//! Run under a name containing "setup", it opens this small window instead of the app:
//!
//! - Install: copies itself to `%LOCALAPPDATA%\Programs\GhostDesk\ghostdesk.exe` (per user, no admin
//!   rights), adds Start menu (and optionally desktop) shortcuts and registers an uninstall entry
//!   under HKCU so GhostDesk shows up in Settings > Apps.
//! - Uninstall (`ghostdesk.exe --uninstall`, the registered uninstall command): runs from a temporary
//!   copy so the install folder can be deleted, removes the AI app connections, shortcuts, the
//!   uninstall entry and the install folder, and optionally the user's data.
//!
//! The install location is fixed on purpose: the MCP configs of AI apps point at it, and a single
//! known path is what keeps updates and permissions from getting tangled.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use eframe::egui::{self, vec2, Align, Layout, RichText};
use parking_lot::Mutex;

use super::i18n::{self, tr, trf};
use super::{fonts, theme, titlebar};
use crate::install;
use crate::platform::{paths, win};

/// Registry key of the uninstall entry (per user).
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\GhostDesk";
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Install,
    Uninstall,
}

/// Which setup window to open, if any: `--uninstall`, `--setup`, or an executable whose name
/// contains "setup" that isn't the installed copy.
pub fn mode(args: &[String]) -> Option<Mode> {
    let has = |flag: &str| args.iter().skip(1).any(|a| a == flag);
    if has("--uninstall") {
        return Some(Mode::Uninstall);
    }
    if has("--setup") {
        return Some(Mode::Install);
    }
    let me = std::env::current_exe().ok()?;
    let name = me.file_name()?.to_string_lossy().to_lowercase();
    (name.contains("setup") && !paths::same_path(&me, &install::stable_exe())).then_some(Mode::Install)
}

/// Opens the setup window and returns when it closes.
pub fn run(mode: Mode) -> Result<()> {
    // the uninstaller can't delete the folder it runs from: continue from a temporary copy
    if mode == Mode::Uninstall && relaunch_from_temp()? {
        return Ok(());
    }
    // `--quiet`: no window (scripted installs); keeps settings and workspaces on uninstall
    if std::env::args().any(|a| a == "--quiet") {
        let r = match mode {
            Mode::Install => do_install(true, &|_| {}),
            Mode::Uninstall => do_uninstall(false, &|_| {}),
        };
        if mode == Mode::Uninstall {
            delete_self_later();
        }
        return r;
    }
    let lang = super::prefs::saved_lang();
    i18n::set(lang);
    let viewport = egui::ViewportBuilder::default()
        .with_title(if mode == Mode::Install { "GhostDesk Setup" } else { "GhostDesk Uninstall" })
        .with_icon(super::window_icon())
        .with_inner_size([520.0, 360.0])
        .with_resizable(false)
        .with_maximize_button(false)
        .with_decorations(false);
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "GhostDesk Setup",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            fonts::setup_fonts(&cc.egui_ctx, lang);
            Ok(Box::new(Setup::new(mode)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("could not open the setup window: {e}"))?;
    if mode == Mode::Uninstall {
        delete_self_later();
    }
    Ok(())
}

/// Keeps the installer's registration in step after the app updated itself (the updater swaps
/// the exe but knows nothing about the uninstall entry).
///
/// An install that never went through the setup window (updated in the app from a version before
/// the installer, or installed by the app itself on first launch) has no entry at all: when this is
/// the installed copy, it gets what the installer would have made, a Start menu shortcut and the
/// uninstall entry (no desktop shortcut, which the installer only makes when asked).
pub fn refresh_registration() {
    match win::reg_string(win::HKEY_CURRENT_USER, UNINSTALL_KEY, Some("DisplayVersion")) {
        Some(v) if v != VERSION => {
            let _ = reg_set(UNINSTALL_KEY, &[("DisplayVersion", Val::Str(VERSION.into()))]);
        }
        Some(_) => {}
        None => {
            if std::env::var_os("GHOSTDESK_DATA_DIR").is_some() {
                return;
            }
            let exe = install::stable_exe();
            let is_installed_copy = std::env::current_exe().is_ok_and(|me| paths::same_path(&me, &exe));
            if !is_installed_copy {
                return;
            }
            let link = start_menu_link();
            if !link.exists() {
                let _ = shortcut(&link, &exe);
            }
            let _ = register_uninstall(&exe, &install::stable_app());
        }
    }
}

// ---- window ----

enum Phase {
    Ready,
    Working,
    Done,
    Failed(String),
}

struct Setup {
    mode: Mode,
    phase: Phase,
    /// What the worker thread is doing (shown under the spinner); `None` when it finished.
    progress: Arc<Mutex<Option<String>>>,
    result: Arc<Mutex<Option<Result<(), String>>>>,
    desktop_shortcut: bool,
    remove_data: bool,
    /// Version already installed (install mode).
    installed: Option<String>,
}

impl Setup {
    fn new(mode: Mode) -> Self {
        let installed = install::stable_exe().is_file().then(|| install::installed_version(&install::stable_app())).flatten();
        Setup {
            mode,
            phase: Phase::Ready,
            progress: Arc::default(),
            result: Arc::default(),
            desktop_shortcut: true,
            remove_data: false,
            installed,
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        self.phase = Phase::Working;
        let (progress, result, ctx) = (self.progress.clone(), self.result.clone(), ctx.clone());
        let (mode, desktop, remove_data) = (self.mode, self.desktop_shortcut, self.remove_data);
        std::thread::spawn(move || {
            let step = |s: &str| {
                *progress.lock() = Some(s.to_string());
                ctx.request_repaint();
            };
            let r = match mode {
                Mode::Install => do_install(desktop, &step),
                Mode::Uninstall => do_uninstall(remove_data, &step),
            };
            *result.lock() = Some(r.map_err(|e| format!("{e:#}")));
            ctx.request_repaint();
        });
    }

    fn body(&mut self, ui: &mut egui::Ui) {
        let install = self.mode == Mode::Install;
        if let Some(r) = self.result.lock().take() {
            self.phase = match r {
                Ok(()) => Phase::Done,
                Err(e) => Phase::Failed(e),
            };
        }
        let newer_installed = self.installed.as_deref().is_some_and(|v| crate::update::newer(v, VERSION));

        // title block
        let (title, sub) = match (&self.phase, install) {
            (Phase::Done, true) => (tr("설치했어요").to_string(), tr("시작 메뉴에서 GhostDesk 를 찾을 수 있어요").to_string()),
            (Phase::Done, false) => (tr("제거했어요").to_string(), tr("GhostDesk 와 AI 앱 연결을 모두 지웠어요").to_string()),
            (Phase::Failed(_), true) => (tr("설치하지 못했어요").to_string(), String::new()),
            (Phase::Failed(_), false) => (tr("제거하지 못했어요").to_string(), String::new()),
            (_, true) => (trf("GhostDesk {} 설치", &[&VERSION]), tr("AI 에이전트가 내 화면을 건드리지 않고 일하는 작업 공간").to_string()),
            (_, false) => (tr("GhostDesk 제거").to_string(), tr("앱과 AI 앱 연결을 지워요").to_string()),
        };
        ui.label(RichText::new(title).size(20.0).strong().color(theme::text()));
        if !sub.is_empty() {
            ui.add_space(4.0);
            ui.label(RichText::new(sub).color(theme::muted()));
        }
        ui.add_space(20.0);

        match &self.phase {
            Phase::Ready if install => {
                theme::section(ui, tr("설치 위치"));
                ui.add_space(4.0);
                theme::card_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(paths::simplify(&install::stable_app()).display().to_string()).monospace().color(theme::body()));
                });
                ui.add_space(12.0);
                ui.add(theme::switch(&mut self.desktop_shortcut, tr("바탕 화면에 바로 가기 만들기")));
                if let Some(v) = &self.installed {
                    ui.add_space(8.0);
                    let note = if newer_installed {
                        trf("더 새 버전 {} 이(가) 이미 설치되어 있어요", &[v])
                    } else {
                        trf("설치된 {} 을(를) 이 버전으로 바꿔요. 설정과 작업 공간은 그대로예요", &[v])
                    };
                    ui.label(theme::hint(note));
                }
            }
            Phase::Ready => {
                ui.add(theme::switch(&mut self.remove_data, tr("설정과 작업 공간도 지우기")));
                ui.add_space(4.0);
                let note = tr("저장한 AI 키, 복제한 로그인 정보, 앱 데이터가 함께 지워져요. 다시 설치할 거라면 꺼 두세요");
                ui.label(if self.remove_data { RichText::new(note).size(12.0).color(theme::warn()) } else { theme::hint(note) });
            }
            Phase::Working => {
                ui.horizontal(|ui| {
                    theme::spinner(ui, 16.0, theme::muted());
                    ui.add_space(4.0);
                    ui.label(RichText::new(self.progress.lock().clone().unwrap_or_default()).color(theme::body()));
                });
            }
            Phase::Done => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(crate::icon::CIRCLE_CHECK).size(18.0).color(theme::success()));
                    ui.label(RichText::new(if install { tr("준비됐어요") } else { tr("완료") }).color(theme::body()));
                });
            }
            Phase::Failed(e) => {
                ui.label(RichText::new(e).color(theme::danger()));
            }
        }

        // buttons, bottom right
        let ctx = ui.ctx().clone();
        ui.with_layout(Layout::bottom_up(Align::Max), |ui| {
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    match (&self.phase, install) {
                        (Phase::Ready, true) if newer_installed => {
                            if theme::primary(ui, tr("GhostDesk 열기")).clicked() {
                                launch_installed();
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        }
                        (Phase::Ready, true) => {
                            let label = if self.installed.is_some() { tr("업데이트") } else { tr("설치") };
                            if theme::primary(ui, label).clicked() {
                                self.start(&ctx);
                            }
                        }
                        (Phase::Ready, false) => {
                            if danger_button(ui, tr("제거")).clicked() {
                                self.start(&ctx);
                            }
                        }
                        (Phase::Working, _) => {}
                        (Phase::Done, true) => {
                            if theme::primary(ui, tr("GhostDesk 열기")).clicked() {
                                launch_installed();
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        }
                        (Phase::Failed(_), _) => {
                            if theme::primary(ui, tr("다시 시도")).clicked() {
                                self.start(&ctx);
                            }
                        }
                        (Phase::Done, false) => {}
                    }
                    let close = if matches!(self.phase, Phase::Ready | Phase::Failed(_)) { tr("취소") } else { tr("닫기") };
                    if !matches!(self.phase, Phase::Working) && theme::secondary(ui, close).clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        });
    }
}

impl eframe::App for Setup {
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        theme::sync(&ctx);
        // closing mid-way would leave a half-installed copy: the close button waits for the worker
        if matches!(self.phase, Phase::Working) && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        egui::Panel::top("setup_title").exact_size(titlebar::HEIGHT).frame(egui::Frame::NONE).show(ui, |ui| {
            titlebar::dialog_bar(ui);
        });
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme::bg()).inner_margin(egui::Margin { left: 28, right: 28, top: 12, bottom: 24 }))
            .show(ui, |ui| self.body(ui));
    }
}

/// Red button for the destructive action.
fn danger_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(egui::Color32::WHITE))
            .fill(theme::danger())
            .corner_radius(theme::RADIUS)
            .min_size(vec2(0.0, 32.0)),
    )
}

// ---- install ----

fn do_install(desktop: bool, step: &dyn Fn(&str)) -> Result<()> {
    step(tr("실행 중인 GhostDesk 를 닫는 중"));
    install::close_gui_windows(std::time::Duration::from_secs(8));
    step(tr("파일을 복사하는 중"));
    let exe = install::install_app()?;
    step(tr("바로 가기를 만드는 중"));
    let dir = install::stable_app();
    shortcut(&start_menu_link(), &exe)?;
    if desktop {
        shortcut(&desktop_link(), &exe)?;
    } else {
        let _ = std::fs::remove_file(desktop_link());
    }
    register_uninstall(&exe, &dir)?;
    Ok(())
}

fn launch_installed() {
    let mut cmd = std::process::Command::new(install::stable_exe());
    let _ = crate::platform::proc::spawn_detached(&mut cmd);
}

fn start_menu_link() -> PathBuf {
    dirs::data_dir().unwrap_or_default().join(r"Microsoft\Windows\Start Menu\Programs\GhostDesk.lnk")
}

fn desktop_link() -> PathBuf {
    dirs::desktop_dir().unwrap_or_default().join("GhostDesk.lnk")
}

/// Writes a .lnk at `link` pointing at `target` (through the shell's IShellLinkW).
fn shortcut(link: &Path, target: &Path) -> Result<()> {
    use windows::core::{Interface, HSTRING};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    if let Some(d) = link.parent() {
        std::fs::create_dir_all(d)?;
    }
    unsafe {
        // S_FALSE (already initialized on this thread) is fine
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).context("the shell can't create shortcuts")?;
        sl.SetPath(&HSTRING::from(target.as_os_str()))?;
        if let Some(d) = target.parent() {
            sl.SetWorkingDirectory(&HSTRING::from(d.as_os_str()))?;
        }
        sl.SetDescription(&HSTRING::from("GhostDesk"))?;
        sl.cast::<IPersistFile>()?
            .Save(&HSTRING::from(link.as_os_str()), true)
            .with_context(|| format!("could not create {}", link.display()))?;
    }
    Ok(())
}

fn register_uninstall(exe: &Path, dir: &Path) -> Result<()> {
    let size_kb = std::fs::metadata(exe).map(|m| (m.len() / 1024) as u32).unwrap_or(0);
    reg_set(
        UNINSTALL_KEY,
        &[
            ("DisplayName", Val::Str("GhostDesk".into())),
            ("DisplayVersion", Val::Str(VERSION.into())),
            ("Publisher", Val::Str("GhostDesk".into())),
            ("DisplayIcon", Val::Str(format!("{},0", exe.display()))),
            ("InstallLocation", Val::Str(dir.display().to_string())),
            ("UninstallString", Val::Str(format!("\"{}\" --uninstall", exe.display()))),
            ("QuietUninstallString", Val::Str(format!("\"{}\" --uninstall --quiet", exe.display()))),
            ("URLInfoAbout", Val::Str("https://github.com/r2adkr/ghostdesk".into())),
            ("NoModify", Val::Dword(1)),
            ("NoRepair", Val::Dword(1)),
            ("EstimatedSize", Val::Dword(size_kb)),
        ],
    )
}

// ---- uninstall ----

/// When running from the install folder, copies this exe to %TEMP% and reopens it there.
/// Returns true if so (this process then exits).
fn relaunch_from_temp() -> Result<bool> {
    let me = std::env::current_exe()?;
    let dir = install::stable_app();
    if !me.parent().is_some_and(|p| paths::same_path(p, &dir)) {
        return Ok(false);
    }
    let tmp = std::env::temp_dir().join(format!("GhostDesk-uninstall-{}.exe", std::process::id()));
    std::fs::copy(&me, &tmp).context("could not copy the uninstaller to the temp folder")?;
    let mut cmd = std::process::Command::new(&tmp);
    cmd.args(std::env::args().skip(1));
    crate::platform::proc::spawn_detached(&mut cmd)?;
    Ok(true)
}

fn do_uninstall(remove_data: bool, step: &dyn Fn(&str)) -> Result<()> {
    let dir = install::stable_app();
    step(tr("실행 중인 GhostDesk 를 닫는 중"));
    install::close_gui_windows(std::time::Duration::from_secs(8));
    // MCP servers started by AI apps run from the install folder and would keep their files locked
    kill_processes_in(&dir);
    step(tr("AI 앱 연결을 지우는 중"));
    let _ = install::disconnect_all();
    step(tr("파일을 지우는 중"));
    let _ = std::fs::remove_file(start_menu_link());
    let _ = std::fs::remove_file(desktop_link());
    reg_delete(UNINSTALL_KEY);
    if dir.exists() {
        crate::platform::fsx::remove_dir_all_retry(&dir).with_context(|| format!("could not delete {}", dir.display()))?;
    }
    if remove_data {
        let data = paths::data_root();
        if data.exists() {
            crate::platform::fsx::remove_dir_all_retry(&data).with_context(|| format!("could not delete {}", data.display()))?;
        }
    }
    Ok(())
}

/// Ends this user's processes whose executable lives in `dir` (other than this one).
fn kill_processes_in(dir: &Path) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    let me = std::process::id();
    for (pid, _, _) in win::processes() {
        if pid == me || !win::process_path(pid).is_some_and(|p| p.parent().is_some_and(|d| paths::same_path(d, dir))) {
            continue;
        }
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                TerminateProcess(h, 1);
                CloseHandle(h);
            }
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
}

/// The temporary uninstaller copy deletes itself once it has exited (a hidden cmd waits a moment,
/// then removes the file).
fn delete_self_later() {
    let Ok(me) = std::env::current_exe() else { return };
    if !me.starts_with(std::env::temp_dir()) {
        return;
    }
    let mut cmd = std::process::Command::new("cmd");
    cmd.args(["/d", "/c", "ping", "127.0.0.1", "-n", "3", ">nul", "&", "del", "/f", "/q"]).arg(&me);
    crate::platform::proc::hide_console(&mut cmd);
    let _ = cmd.spawn();
}

// ---- registry ----

enum Val {
    Str(String),
    Dword(u32),
}

fn reg_set(key: &str, values: &[(&str, Val)]) -> Result<()> {
    use windows_sys::Win32::System::Registry::{RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ};
    let k = win::wide(key);
    let mut h: HKEY = std::ptr::null_mut();
    let rc = unsafe {
        RegCreateKeyExW(win::HKEY_CURRENT_USER, k.as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, std::ptr::null(), &mut h, std::ptr::null_mut())
    };
    anyhow::ensure!(rc == 0, "could not create the registry key ({rc})");
    for (name, v) in values {
        let n = win::wide(name);
        let rc = unsafe {
            match v {
                Val::Str(s) => {
                    let w = win::wide(s);
                    RegSetValueExW(h, n.as_ptr(), 0, REG_SZ, w.as_ptr() as *const u8, (w.len() * 2) as u32)
                }
                Val::Dword(d) => RegSetValueExW(h, n.as_ptr(), 0, REG_DWORD, d as *const u32 as *const u8, 4),
            }
        };
        if rc != 0 {
            unsafe { RegCloseKey(h) };
            anyhow::bail!("could not write registry value {name} ({rc})");
        }
    }
    unsafe { RegCloseKey(h) };
    Ok(())
}

fn reg_delete(key: &str) {
    use windows_sys::Win32::System::Registry::RegDeleteTreeW;
    let k = win::wide(key);
    unsafe { RegDeleteTreeW(win::HKEY_CURRENT_USER, k.as_ptr()) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_pick_the_mode() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(mode(&args(&["x", "--uninstall"])) == Some(Mode::Uninstall));
        assert!(mode(&args(&["x", "--setup"])) == Some(Mode::Install));
        // the test binary's name has no "setup" in it
        assert!(mode(&args(&["x"])).is_none());
    }
}
