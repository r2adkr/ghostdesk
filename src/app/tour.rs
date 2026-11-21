//! First-launch guide (a few pages of screenshots and short descriptions).
//!
//! The screenshots are taken by the app of its own window: with `GHOSTDESK_TOUR_CAPTURE=<dir>`
//! it shows each scene in `PAGES`, saves the window as `<dir>/<file>` and exits (`Capture`).
//! Files placed in assets/tour are built in. Retake them the same way when the UI changes (see
//! assets/tour/README.md).
//!
//! Images are decoded and uploaded only while the guide is open, and dropped when it closes, so
//! they cost no memory the rest of the time.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::*;

/// One page of the guide: scene to capture, image file, title, description.
pub(super) struct Page {
    scene: Scene,
    file: &'static str,
    image: &'static [u8],
    title: &'static str,
    body: &'static str,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Scene {
    Home,
    NewAgent,
    Work,
    Settings,
    Connect,
}

pub(super) const PAGES: &[Page] = &[
    Page {
        scene: Scene::Home,
        file: "1-home.jpg",
        image: include_bytes!("../../assets/tour/1-home.jpg"),
        title: "할 일을 적으면 시작해요",
        body: "홈에서 할 일과 모델을 고른 뒤 시작하기를 누르세요. 작업 장소를 먼저 선택할 수도 있어요.",
    },
    Page {
        scene: Scene::NewAgent,
        file: "2-new-agent.jpg",
        image: include_bytes!("../../assets/tour/2-new-agent.jpg"),
        title: "어디서 일할지 고르세요",
        body: "웹사이트, 이 컴퓨터의 앱, 원격 컴퓨터 중에서 고르면 그 화면을 따로 띄워요. 내 화면과 마우스는 건드리지 않아요.",
    },
    Page {
        scene: Scene::Work,
        file: "3-work.jpg",
        image: include_bytes!("../../assets/tour/3-work.jpg"),
        title: "지켜보다가 필요하면 넘겨받으세요",
        body: "에이전트가 누르는 곳은 가상 마우스로 보여요. 로그인이나 인증이 필요하면 '직접 조작'을 눌러 대신 처리하고 돌려주면 돼요.",
    },
    Page {
        scene: Scene::Settings,
        file: "4-settings.jpg",
        image: include_bytes!("../../assets/tour/4-settings.jpg"),
        title: "쓸 AI 를 연결하세요",
        body: "설정의 'AI 연결'에서 AI 를 추가하고 키를 넣어요. 여러 개를 넣어 두면 홈의 모델 단추에서 바꿔 쓸 수 있어요.",
    },
    Page {
        scene: Scene::Connect,
        file: "5-connect.jpg",
        image: include_bytes!("../../assets/tour/5-connect.jpg"),
        title: "쓰던 AI 앱에서도 부를 수 있어요",
        body: "'외부 연결'을 켜 두면 Claude 앱이나 Claude Code 에게 \"GhostDesk 로 …해줘\" 라고 말하면 돼요. 이때는 API 키가 필요 없어요.",
    },
];

/// Marker file recording that the guide was finished (`<data root>/tour-seen`).
///
/// It's also in prefs.json, but an older version that doesn't know the field would drop it when
/// rewriting the file, hence the separate marker.
fn seen_marker() -> std::path::PathBuf {
    crate::platform::paths::data_root().join("tour-seen")
}

/// Whether the guide was already seen (marker file or prefs). Writes the marker if only prefs
/// has it.
pub(super) fn already_seen(in_prefs: bool) -> bool {
    let marker = seen_marker().exists();
    if in_prefs && !marker {
        mark_seen();
    }
    in_prefs || marker
}

/// Records that the guide was seen (prefs still has it if this fails).
fn mark_seen() {
    let p = seen_marker();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(p, env!("CARGO_PKG_VERSION"));
}

/// Number of pages: the screenshot pages plus (on macOS) a final permissions page.
fn page_count() -> usize {
    PAGES.len() + usize::from(cfg!(target_os = "macos"))
}

/// An open guide: current page and decoded images.
#[derive(Default)]
pub(super) struct Tour {
    page: usize,
    textures: Vec<Option<egui::TextureHandle>>,
}

impl Tour {
    /// Image for page `i` (decoded and uploaded on first view).
    fn texture(&mut self, ctx: &egui::Context, i: usize) -> Option<egui::TextureHandle> {
        if self.textures.len() < PAGES.len() {
            self.textures.resize(PAGES.len(), None);
        }
        if self.textures[i].is_none() {
            let img = image::load_from_memory(PAGES[i].image).ok()?.to_rgba8();
            let ci = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
            self.textures[i] = Some(ctx.load_texture(format!("tour-{i}"), ci, egui::TextureOptions::LINEAR));
        }
        self.textures[i].clone()
    }
}

impl GhostApp {
    /// Opens the guide (at first launch, or from "Show guide again" in Settings).
    pub(super) fn open_tour(&mut self) {
        self.tour = Some(Tour::default());
    }

    /// The guide window. Not shown again once finished or skipped.
    pub(super) fn tour_window(&mut self, ctx: &egui::Context) {
        let Some(tour) = self.tour.as_mut() else { return };
        const W: f32 = 640.0;
        let count = page_count();
        let i = tour.page.min(count - 1);
        let last = i + 1 == count;
        // screenshot pages get their image; the final permissions page has none
        let page = PAGES.get(i);
        let tex = page.and_then(|_| tour.texture(ctx, i));
        let mut done = false;
        let mut go: isize = 0;
        // closes via "Get started" or "Skip" (not Esc or clicking outside)
        egui::Modal::new(egui::Id::unique("tour")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(W);
            match page {
                Some(page) => {
                    if let Some(t) = &tex {
                        let size = t.size_vec2();
                        let h = W * size.y / size.x.max(1.0);
                        let r = ui.add(egui::Image::new(t).fit_to_exact_size(Vec2::new(W, h)).corner_radius(theme::CARD_RADIUS)).rect;
                        // border so dark screenshots don't blend into the window background
                        let line = ui.visuals().widgets.noninteractive.bg_stroke;
                        ui.painter().rect_stroke(r, theme::CARD_RADIUS, line, egui::StrokeKind::Inside);
                    }
                    ui.add_space(16.0);
                    ui.label(RichText::new(tr(page.title)).size(20.0).color(theme::text()));
                    ui.add_space(6.0);
                    ui.label(RichText::new(tr(page.body)).color(theme::body()));
                }
                None => self.permission_page(ui),
            }
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                // page indicator dots
                for j in 0..count {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), egui::Sense::hover());
                    let c = if j == i { theme::text() } else { theme::overlay_pressed() };
                    ui.painter().circle_filled(r.center(), 3.5, c);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::primary(ui, if last { tr("시작하기") } else { tr("다음") }).clicked() {
                        if last {
                            done = true;
                        } else {
                            go = 1;
                        }
                    }
                    if i > 0 && theme::secondary(ui, tr("이전")).clicked() {
                        go = -1;
                    }
                    if !last && ui.link(theme::hint(tr("건너뛰기"))).clicked() {
                        done = true;
                    }
                });
            });
        });
        // keyboard: → / ← to turn pages
        let (right, left) = ctx.input(|inp| (inp.key_pressed(egui::Key::ArrowRight), inp.key_pressed(egui::Key::ArrowLeft)));
        if right && !last {
            go = 1;
        } else if left && i > 0 {
            go = -1;
        }
        if let Some(t) = self.tour.as_mut() {
            t.page = (i as isize + go).clamp(0, count as isize - 1) as usize;
        }
        if done {
            self.tour = None;
            self.tour_seen = true;
            mark_seen();
            self.save_prefs();
        }
    }

    /// Final page (macOS): turn on the permissions needed to drive Mac apps, one by one. Uses the
    /// same rows as Settings > Permissions.
    fn permission_page(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.label(RichText::new(icon::SHIELD_ALERT).size(36.0).color(theme::text()));
        ui.add_space(10.0);
        ui.label(RichText::new(tr("마지막으로 권한을 켜 주세요")).size(20.0).color(theme::text()));
        ui.add_space(6.0);
        ui.label(
            RichText::new(tr("이 컴퓨터의 Mac 앱을 맡기려면 두 권한이 필요해요. '허용'을 누르면 macOS 의 요청 창이 하나 뜨고, 그 창의 '시스템 설정 열기'에서 GhostDesk 를 켜면 돼요. 웹사이트만 쓸 거라면 나중에 설정 > 권한 에서 해도 돼요."))
                .color(theme::body()),
        );
        ui.add_space(12.0);
        #[cfg(target_os = "macos")]
        self.permission_rows(ui);
    }
    /// Advances one frame in capture mode (`Capture`).
    pub(super) fn tour_capture_tick(&mut self, ctx: &egui::Context) {
        let Some(mut cap) = self.tour_capture.take() else { return };
        if cap.tick(self, ctx) {
            self.tour_capture = Some(cap);
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Shows a scene: closes every other window and shows only that scene.
    fn show_scene(&mut self, ctx: &egui::Context, scene: Scene) {
        self.wizard_open = false;
        self.show_settings = false;
        self.show_connect = false;
        self.view = View::Home;
        match scene {
            Scene::Home => {}
            Scene::NewAgent => self.open_wizard(),
            Scene::Work => {
                // the demo agent first appears in this scene (so earlier scenes show an empty sidebar)
                if self.agents.is_empty() && self.pending_demo == 0 {
                    self.pending_demo = 1;
                }
                if !self.agents.is_empty() {
                    self.selected = Some(0);
                    self.view = View::Detail;
                }
            }
            Scene::Settings => self.open_settings(settings::SettingsTab::Ai),
            Scene::Connect => {
                self.open_connect(ctx);
                // auto-install/connect is off while capturing (GHOSTDESK_NO_AUTOCONNECT), so only show the
                // switch at its usual default (on)
                self.connect_state.lock().auto = true;
            }
        }
    }
}

/// `GHOSTDESK_TOUR_CAPTURE=<dir>`: shows each scene, captures the window, then exits.
///
/// The Work scene starts a demo agent (against a fake AI server via `ANTHROPIC_BASE_URL`) and is
/// captured after it has done a few steps.
pub(super) struct Capture {
    dir: PathBuf,
    step: usize,
    frames: u32,
    /// Whether the current scene's image was saved (images arrive separately, a frame later).
    saved: Arc<AtomicBool>,
    requested: bool,
}

/// Frames to wait for a scene to settle (including the 0.2 s window transition).
const SETTLE_FRAMES: u32 = 45;
/// Maximum frames to wait for the agent in the Work scene.
const WORK_TIMEOUT_FRAMES: u32 = 3000;
/// Width of saved images (about 2x the guide's 640 pt for Retina).
const SHOT_WIDTH: u32 = 1280;

impl Capture {
    pub fn from_env() -> Option<Self> {
        let dir = PathBuf::from(std::env::var_os("GHOSTDESK_TOUR_CAPTURE")?);
        std::fs::create_dir_all(&dir).ok()?;
        Some(Capture { dir, step: 0, frames: 0, saved: Arc::default(), requested: false })
    }

    /// One frame. Returns false when everything is captured.
    fn tick(&mut self, app: &mut GhostApp, ctx: &egui::Context) -> bool {
        ctx.request_repaint();
        let Some(page) = PAGES.get(self.step) else { return false };
        if self.step == 0 && self.frames == 0 {
            // every image at the same size
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(1280.0, 800.0)));
        }
        if self.frames == 0 {
            app.show_scene(ctx, page.scene);
        }
        self.frames += 1;
        if self.saved.swap(false, Ordering::SeqCst) {
            eprintln!("tour: {}", self.dir.join(page.file).display());
            self.step += 1;
            self.frames = 0;
            self.requested = false;
            return true;
        }
        let ready = match page.scene {
            // until the demo agent's live screen is up and a few log rows have accumulated
            Scene::Work => {
                app.show_scene(ctx, page.scene);
                // once the last action (typing) is done and it's thinking about the next reply
                let agent_ready = app.agents.first().is_some_and(|a| {
                    a.texture.is_some() && a.shared.log.lock().len() >= 6 && matches!(a.shared.status(), crate::agent::AgentStatus::Thinking)
                });
                (agent_ready && self.frames > SETTLE_FRAMES) || self.frames > WORK_TIMEOUT_FRAMES
            }
            _ => self.frames > SETTLE_FRAMES,
        };
        if ready && !self.requested {
            self.requested = true;
            let (path, saved) = (self.dir.join(page.file), self.saved.clone());
            ctx.request_screenshot(move |img| {
                if let Err(e) = save_jpeg(&img, &path) {
                    eprintln!("tour: failed to save {}: {e:#}", path.display());
                }
                saved.store(true, Ordering::SeqCst);
            });
        }
        true
    }
}

/// Scales the window image to `SHOT_WIDTH` and saves it as JPEG.
fn save_jpeg(img: &egui::ColorImage, path: &std::path::Path) -> anyhow::Result<()> {
    use image::ImageEncoder;
    let [w, h] = img.size;
    let rgb: Vec<u8> = img.pixels.iter().flat_map(|p| [p.r(), p.g(), p.b()]).collect();
    let full = image::RgbImage::from_raw(w as u32, h as u32, rgb).ok_or_else(|| anyhow::anyhow!("screenshot size mismatch"))?;
    let scale = SHOT_WIDTH as f32 / w as f32;
    let small = if scale < 1.0 {
        image::imageops::resize(&full, SHOT_WIDTH, (h as f32 * scale).round() as u32, image::imageops::FilterType::Lanczos3)
    } else {
        full
    };
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).write_image(small.as_raw(), small.width(), small.height(), image::ExtendedColorType::Rgb8)?;
    std::fs::write(path, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every bundled image decodes and no file names collide.
    #[test]
    fn tour_images_decode() {
        let mut names = std::collections::HashSet::new();
        for p in PAGES {
            assert!(names.insert(p.file), "{}", p.file);
            let img = image::load_from_memory(p.image).unwrap_or_else(|e| panic!("{}: {e}", p.file));
            assert!(img.width() >= 640, "{} 이 너무 작음", p.file);
        }
    }
}
