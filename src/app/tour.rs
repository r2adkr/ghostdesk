//! 처음 켤 때 보여 주는 사용 설명 (사진 + 짧은 설명, 몇 쪽).
//!
//! 사진은 앱이 자기 창을 찍어 만든다: `GHOSTDESK_TOUR_CAPTURE=<폴더>` 로 켜면 `PAGES` 의 장면을 차례로 띄우고
//! 창을 찍어 `<폴더>/<파일>` 로 저장한 뒤 끝난다 (`Capture`). 그 파일을 assets/tour 에 두면 빌드에 들어간다.
//! UI 가 바뀌면 같은 방법으로 다시 찍으면 된다 (방법은 assets/tour/README.md).
//!
//! 사진은 사용 설명 창이 열려 있을 때만 풀어서 올리고, 닫으면 버린다 (평소 메모리를 쓰지 않게).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::*;

/// 사용 설명의 한 쪽: 찍을 장면 · 사진 파일 · 제목 · 설명
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
        body: "홈 화면에 맡길 일을 적고 Enter 를 누르세요. 입력 칸 아래 모델 단추에서 어떤 AI 로 일할지 고를 수 있어요.",
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

/// 열려 있는 사용 설명: 지금 쪽과, 풀어 둔 사진
#[derive(Default)]
pub(super) struct Tour {
    page: usize,
    textures: Vec<Option<egui::TextureHandle>>,
}

impl Tour {
    /// `i` 쪽 사진 (처음 볼 때 풀어서 올린다)
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
    /// 사용 설명을 연다 (처음 켤 때, 설정의 '사용 설명 다시 보기')
    pub(super) fn open_tour(&mut self) {
        self.tour = Some(Tour::default());
    }

    /// 사용 설명 창. 끝까지 보거나 건너뛰면 다시 띄우지 않는다.
    pub(super) fn tour_window(&mut self, ctx: &egui::Context) {
        let Some(tour) = self.tour.as_mut() else { return };
        const W: f32 = 640.0;
        let i = tour.page.min(PAGES.len() - 1);
        let page = &PAGES[i];
        let tex = tour.texture(ctx, i);
        let mut done = false;
        let mut go: isize = 0;
        let m = egui::Modal::new(egui::Id::unique("tour")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(W);
            if let Some(t) = &tex {
                let size = t.size_vec2();
                let h = W * size.y / size.x.max(1.0);
                let r = ui.add(egui::Image::new(t).fit_to_exact_size(Vec2::new(W, h)).corner_radius(theme::CARD_RADIUS)).rect;
                // 어두운 사진이 창 바탕과 섞이지 않게 테두리를 두른다
                let line = ui.visuals().widgets.noninteractive.bg_stroke;
                ui.painter().rect_stroke(r, theme::CARD_RADIUS, line, egui::StrokeKind::Inside);
            }
            ui.add_space(16.0);
            ui.label(RichText::new(tr(page.title)).size(20.0).color(theme::text()));
            ui.add_space(6.0);
            ui.label(RichText::new(tr(page.body)).color(theme::body()));
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                // 몇 번째 쪽인지: 점
                for j in 0..PAGES.len() {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), egui::Sense::hover());
                    let c = if j == i { theme::text() } else { theme::overlay_pressed() };
                    ui.painter().circle_filled(r.center(), 3.5, c);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let last = i + 1 == PAGES.len();
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
        if let Some(t) = self.tour.as_mut() {
            t.page = (t.page as isize + go).clamp(0, PAGES.len() as isize - 1) as usize;
        }
        // 키보드: → / ← 로 넘기기
        let (right, left) = ctx.input(|inp| (inp.key_pressed(egui::Key::ArrowRight), inp.key_pressed(egui::Key::ArrowLeft)));
        if let Some(t) = self.tour.as_mut() {
            if right && t.page + 1 < PAGES.len() {
                t.page += 1;
            } else if left && t.page > 0 {
                t.page -= 1;
            }
        }
        if done || m.should_close() {
            self.tour = None;
            self.tour_seen = true;
            self.save_prefs();
        }
    }

    /// 사진 찍기 모드면 한 프레임 진행한다 (`Capture`)
    pub(super) fn tour_capture_tick(&mut self, ctx: &egui::Context) {
        let Some(mut cap) = self.tour_capture.take() else { return };
        if cap.tick(self, ctx) {
            self.tour_capture = Some(cap);
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// 장면을 띄운다: 다른 창을 모두 닫고 그 장면만
    fn show_scene(&mut self, ctx: &egui::Context, scene: Scene) {
        self.wizard_open = false;
        self.show_settings = false;
        self.show_connect = false;
        self.view = View::Home;
        match scene {
            Scene::Home => {}
            Scene::NewAgent => self.open_wizard(),
            Scene::Work => {
                // 데모 에이전트는 이 장면에서 처음 띄운다 (앞 장면의 사이드바가 비어 보이게)
                if self.agents.is_empty() && self.pending_demo == 0 {
                    self.pending_demo = 1;
                }
                if !self.agents.is_empty() {
                    self.selected = Some(0);
                    self.view = View::Detail;
                }
            }
            Scene::Settings => self.show_settings = true,
            Scene::Connect => {
                self.open_connect(ctx);
                // 찍을 때는 자동 설치·연결을 끄고 켜므로(GHOSTDESK_NO_AUTOCONNECT) 스위치만 평소 기본값(켜짐)으로 보인다
                self.connect_state.lock().auto = true;
            }
        }
    }
}

/// `GHOSTDESK_TOUR_CAPTURE=<폴더>`: 장면을 하나씩 띄워 창을 찍고 끝낸다.
/// 일하는 장면(Work)은 그 장면에서 데모 에이전트를 띄우고(가짜 AI 서버: `ANTHROPIC_BASE_URL`) 몇 단계 일한 뒤에 찍는다.
pub(super) struct Capture {
    dir: PathBuf,
    step: usize,
    frames: u32,
    /// 지금 장면의 사진을 저장했는가 (사진은 다음 프레임 뒤에 따로 도착한다)
    saved: Arc<AtomicBool>,
    requested: bool,
}

/// 장면이 자리 잡을 때까지 기다리는 프레임 수 (창 전환 효과 0.2초 포함)
const SETTLE_FRAMES: u32 = 45;
/// 일하는 장면에서 에이전트를 기다리는 최대 프레임 수
const WORK_TIMEOUT_FRAMES: u32 = 3000;
/// 저장할 사진의 너비 (사용 설명 창 640pt 의 레티나 두 배쯤)
const SHOT_WIDTH: u32 = 1280;

impl Capture {
    pub fn from_env() -> Option<Self> {
        let dir = PathBuf::from(std::env::var_os("GHOSTDESK_TOUR_CAPTURE")?);
        std::fs::create_dir_all(&dir).ok()?;
        Some(Capture { dir, step: 0, frames: 0, saved: Arc::default(), requested: false })
    }

    /// 한 프레임. 다 찍었으면 false
    fn tick(&mut self, app: &mut GhostApp, ctx: &egui::Context) -> bool {
        ctx.request_repaint();
        let Some(page) = PAGES.get(self.step) else { return false };
        if self.step == 0 && self.frames == 0 {
            // 모든 사진을 같은 크기로
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
            // 데모 에이전트의 라이브 화면과 기록이 몇 줄 쌓일 때까지
            Scene::Work => {
                app.show_scene(ctx, page.scene);
                // 마지막 동작(입력)까지 끝나고 다음 답을 생각하는 중일 때
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

/// 창 사진을 `SHOT_WIDTH` 너비로 줄여 JPEG 로 저장한다
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

    /// 넣어 둔 사진이 모두 풀리고, 파일 이름이 겹치지 않는다
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
