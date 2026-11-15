//! ScreenCaptureKit 기반 캡처.
//!
//! - 단일 창: `SCContentFilter initWithDesktopIndependentWindow` + `SCScreenshotManager`.
//!   숨겨진(hidden)·최소화·가려진 창도 캡처된다 (이 Mac 에서 검증, 보통 50~80ms, 첫 호출 150~250ms).
//!   레거시 `CGWindowListCreateImage` 는 숨겨진 창에 NULL 을 돌려주므로 쓰지 않는다.
//! - 디스플레이: `capture_display_png` — 세션 헬퍼(전용 에이전트 로그인 세션)용.
//!
//! 주의: 새 바이너리가 처음 SCK 를 쓰면 macOS 가 1회성 시스템 확인 알림(화면 기록 사용 안내)을
//! 띄울 수 있다. 사용자에게 미리 안내할 것. 화면 기록 권한이 없으면 오류를 돌려준다.
//!
//! 모든 함수는 블로킹이다 (완료 핸들러를 채널로 기다린다). async 코드에서는 spawn_blocking 안에서 부를 것.

use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use block2::RcBlock;
use image::{Rgb, RgbImage};
use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_core_foundation::{CFRetained, CGRect};
use objc2_core_graphics::{CGDataProvider, CGImage, CGMainDisplayID};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCScreenshotManager, SCShareableContent, SCStreamConfiguration, SCWindow,
};

fn ns_err(e: *mut NSError) -> String {
    if e.is_null() {
        return "알 수 없는 오류".into();
    }
    let e = unsafe { &*e };
    format!("{} (domain={} code={})", e.localizedDescription(), e.domain(), e.code())
}

/// 공유 가능한 콘텐츠 목록 (onScreenWindowsOnly=false → 숨겨진 창 포함)
pub fn shareable_content() -> Result<Retained<SCShareableContent>> {
    // CLI 에서 CGS 연결을 초기화해 둔다 (GUI 앱에서는 이미 되어 있음)
    let _ = CGMainDisplayID();
    let (tx, rx) = mpsc::channel::<Result<Retained<SCShareableContent>, String>>();
    let block = RcBlock::new(move |c: *mut SCShareableContent, e: *mut NSError| {
        let r = if !e.is_null() {
            Err(ns_err(e))
        } else {
            unsafe { Retained::retain(c) }.ok_or_else(|| "빈 콘텐츠".to_string())
        };
        let _ = tx.send(r);
    });
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, false, &block,
        )
    };
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(e)) => bail!(
            "ScreenCaptureKit 콘텐츠 조회 실패: {e}\n→ 시스템 설정 > 개인정보 보호 및 보안 > 화면 및 시스템 오디오 기록 에서 GhostDesk(또는 실행한 터미널)를 허용하세요."
        ),
        Err(_) => bail!("ScreenCaptureKit 콘텐츠 조회가 10초 안에 끝나지 않았습니다"),
    }
}

/// 캡처용 필터 묶음 (스레드 간 이동 가능). 창 필터는 창이 움직여도 같은 창을 계속 따라간다.
pub struct Filter {
    filter: Retained<SCContentFilter>,
    /// 필터 생성 시점의 콘텐츠 영역 (포인트)
    pub content_rect: CGRect,
    /// 포인트 → 픽셀 배율 (레티나 2.0)
    pub scale: f64,
    /// 창 필터일 때: SCWindow.frame (전역 포인트, 좌상단 원점)
    pub window_frame: Option<CGRect>,
    pub wid: Option<u32>,
    pub created: Instant,
}
unsafe impl Send for Filter {}
unsafe impl Sync for Filter {}

fn find_window(content: &SCShareableContent, wid: u32) -> Option<Retained<SCWindow>> {
    let windows = unsafe { content.windows() };
    windows.iter().find(|w| unsafe { w.windowID() } == wid)
}

/// 창 하나에 대한 필터를 만든다 (숨김/최소화 창 포함).
pub fn window_filter(wid: u32) -> Result<Filter> {
    let content = shareable_content()?;
    let win = find_window(&content, wid)
        .ok_or_else(|| anyhow!("창 {wid} 를 ScreenCaptureKit 목록에서 찾지 못했습니다 (창이 닫혔을 수 있음)"))?;
    let frame = unsafe { win.frame() };
    let filter = unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &win) };
    let scale = unsafe { filter.pointPixelScale() } as f64;
    let content_rect = unsafe { filter.contentRect() };
    Ok(Filter {
        filter,
        content_rect,
        scale: if scale > 0.0 { scale } else { 1.0 },
        window_frame: Some(frame),
        wid: Some(wid),
        created: Instant::now(),
    })
}

/// 현재 세션의 디스플레이 필터 (display_id = 0 이면 메인 디스플레이)
pub fn display_filter(display_id: u32) -> Result<Filter> {
    let content = shareable_content()?;
    let want = if display_id == 0 { CGMainDisplayID() } else { display_id };
    let displays = unsafe { content.displays() };
    let d: Retained<SCDisplay> = displays
        .iter()
        .find(|d| unsafe { d.displayID() } == want)
        .ok_or_else(|| anyhow!("디스플레이 {want} 를 찾지 못했습니다"))?;
    let empty: Retained<NSArray<SCWindow>> = NSArray::new();
    let filter =
        unsafe { SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &d, &empty) };
    let scale = unsafe { filter.pointPixelScale() } as f64;
    let content_rect = unsafe { filter.contentRect() };
    Ok(Filter {
        filter,
        content_rect,
        scale: if scale > 0.0 { scale } else { 1.0 },
        window_frame: None,
        wid: None,
        created: Instant::now(),
    })
}

/// 긴 변이 max_long_edge 이하가 되도록 (비율 유지) 출력 픽셀 크기를 정한다. 0 = 원본 픽셀.
pub fn fit_size(w_pt: f64, h_pt: f64, scale: f64, max_long_edge: u32) -> (u32, u32) {
    let (fw, fh) = ((w_pt * scale).max(1.0), (h_pt * scale).max(1.0));
    let k = if max_long_edge == 0 { 1.0 } else { (max_long_edge as f64 / fw.max(fh)).min(1.0) };
    (((fw * k).round() as u32).max(1), ((fh * k).round() as u32).max(1))
}

impl Filter {
    /// 이 필터로 한 장 캡처. (w, h) = 출력 픽셀 크기 (SCK 가 직접 축소한다).
    pub fn capture(&self, w: u32, h: u32) -> Result<RgbImage> {
        let cfg = unsafe { SCStreamConfiguration::new() };
        unsafe {
            cfg.setWidth(w as usize);
            cfg.setHeight(h as usize);
            cfg.setShowsCursor(false);
            cfg.setIgnoreShadowsSingleWindow(true);
        }
        let (tx, rx) = mpsc::channel::<Result<CFRetained<CGImage>, String>>();
        let block = RcBlock::new(move |img: *mut CGImage, e: *mut NSError| {
            let r = if !e.is_null() {
                Err(ns_err(e))
            } else {
                NonNull::new(img).map(|p| unsafe { CFRetained::retain(p) }).ok_or_else(|| "빈 이미지".to_string())
            };
            let _ = tx.send(r);
        });
        unsafe {
            SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(&self.filter, &cfg, Some(&block))
        };
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(img)) => cgimage_to_rgb(&img),
            Ok(Err(e)) => bail!("창 캡처 실패: {e}"),
            Err(_) => bail!("창 캡처가 10초 안에 끝나지 않았습니다"),
        }
    }

    /// 투명도를 유지한 캡처 (RGBA, straight alpha) — 시트 등을 겹쳐 그릴 때 쓴다
    pub fn capture_rgba(&self, w: u32, h: u32) -> Result<image::RgbaImage> {
        let cfg = unsafe { SCStreamConfiguration::new() };
        unsafe {
            cfg.setWidth(w as usize);
            cfg.setHeight(h as usize);
            cfg.setShowsCursor(false);
            cfg.setIgnoreShadowsSingleWindow(true);
        }
        let (tx, rx) = mpsc::channel::<Result<CFRetained<CGImage>, String>>();
        let block = RcBlock::new(move |img: *mut CGImage, e: *mut NSError| {
            let r = if !e.is_null() {
                Err(ns_err(e))
            } else {
                NonNull::new(img).map(|p| unsafe { CFRetained::retain(p) }).ok_or_else(|| "빈 이미지".to_string())
            };
            let _ = tx.send(r);
        });
        unsafe {
            SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(&self.filter, &cfg, Some(&block))
        };
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(img)) => cgimage_to_rgba(&img),
            Ok(Err(e)) => bail!("창 캡처 실패: {e}"),
            Err(_) => bail!("창 캡처가 10초 안에 끝나지 않았습니다"),
        }
    }

    /// 긴 변 제한으로 캡처
    pub fn capture_fit(&self, max_long_edge: u32) -> Result<RgbImage> {
        let (w, h) = fit_size(self.content_rect.size.width, self.content_rect.size.height, self.scale, max_long_edge);
        self.capture(w, h)
    }
}

/// CGImage(BGRA premultiplied 등) → RGB (투명 부분은 흰색 위에 합성)
pub fn cgimage_to_rgb(img: &CGImage) -> Result<RgbImage> {
    let w = CGImage::width(Some(img));
    let h = CGImage::height(Some(img));
    let bpr = CGImage::bytes_per_row(Some(img));
    let bpp = CGImage::bits_per_pixel(Some(img));
    if bpp != 32 {
        bail!("지원하지 않는 픽셀 형식 (bpp={bpp})");
    }
    let info = CGImage::bitmap_info(Some(img)).0;
    let prov = CGImage::data_provider(Some(img)).ok_or_else(|| anyhow!("이미지 데이터 없음"))?;
    let data = CGDataProvider::data(Some(&prov)).ok_or_else(|| anyhow!("이미지 데이터 복사 실패"))?;
    let bytes = data.to_vec();
    // byte order: 0x2000 = 32bit little endian (BGRA in memory), 그 외 big endian (ARGB/RGBA)
    let little = (info & 0x7000) == 0x2000;
    let alpha_info = info & 0x1f; // 1=premul last, 2=premul first, 3=last, 4=first, 5/6=skip
    let alpha_first = matches!(alpha_info, 2 | 4 | 6);
    let mut out = RgbImage::new(w as u32, h as u32);
    for y in 0..h {
        let row = &bytes[y * bpr..(y * bpr + w * 4).min(bytes.len())];
        for x in 0..w.min(row.len() / 4) {
            let p = &row[x * 4..x * 4 + 4];
            // 메모리 순서를 논리적 ARGB/RGBA 로
            let (r, g, b, a) = if little {
                if alpha_first { (p[2], p[1], p[0], p[3]) } else { (p[3], p[2], p[1], p[0]) }
            } else if alpha_first {
                (p[1], p[2], p[3], p[0])
            } else {
                (p[0], p[1], p[2], p[3])
            };
            let (r, g, b) = match alpha_info {
                0 | 5 | 6 => (r, g, b),
                1 | 2 => {
                    let add = 255 - a as u16;
                    ((r as u16 + add).min(255) as u8, (g as u16 + add).min(255) as u8, (b as u16 + add).min(255) as u8)
                }
                _ => {
                    let af = a as f32 / 255.0;
                    let mix = |c: u8| (c as f32 * af + 255.0 * (1.0 - af)).round() as u8;
                    (mix(r), mix(g), mix(b))
                }
            };
            out.put_pixel(x as u32, y as u32, Rgb([r, g, b]));
        }
    }
    Ok(out)
}

/// CGImage → RGBA (straight alpha)
pub fn cgimage_to_rgba(img: &CGImage) -> Result<image::RgbaImage> {
    let w = CGImage::width(Some(img));
    let h = CGImage::height(Some(img));
    let bpr = CGImage::bytes_per_row(Some(img));
    if CGImage::bits_per_pixel(Some(img)) != 32 {
        bail!("지원하지 않는 픽셀 형식");
    }
    let info = CGImage::bitmap_info(Some(img)).0;
    let prov = CGImage::data_provider(Some(img)).ok_or_else(|| anyhow!("이미지 데이터 없음"))?;
    let data = CGDataProvider::data(Some(&prov)).ok_or_else(|| anyhow!("이미지 데이터 복사 실패"))?;
    let bytes = data.to_vec();
    let little = (info & 0x7000) == 0x2000;
    let alpha_info = info & 0x1f;
    let alpha_first = matches!(alpha_info, 2 | 4 | 6);
    let premul = matches!(alpha_info, 1 | 2);
    let mut out = image::RgbaImage::new(w as u32, h as u32);
    for y in 0..h {
        let row = &bytes[y * bpr..(y * bpr + w * 4).min(bytes.len())];
        for x in 0..w.min(row.len() / 4) {
            let p = &row[x * 4..x * 4 + 4];
            let (r, g, b, a) = if little {
                if alpha_first { (p[2], p[1], p[0], p[3]) } else { (p[3], p[2], p[1], p[0]) }
            } else if alpha_first {
                (p[1], p[2], p[3], p[0])
            } else {
                (p[0], p[1], p[2], p[3])
            };
            let a = if matches!(alpha_info, 0 | 5 | 6) { 255 } else { a };
            let un = |c: u8| if premul && a > 0 && a < 255 { ((c as u32 * 255) / a as u32).min(255) as u8 } else { c };
            out.put_pixel(x as u32, y as u32, image::Rgba([un(r), un(g), un(b), a]));
        }
    }
    Ok(out)
}

/// RGBA 이미지를 RGB 이미지 위 (x, y) 에 알파 합성
pub fn blend_onto(dst: &mut RgbImage, src: &image::RgbaImage, x: i64, y: i64) {
    for (sx, sy, p) in src.enumerate_pixels() {
        let (dx, dy) = (x + sx as i64, y + sy as i64);
        if dx < 0 || dy < 0 || dx >= dst.width() as i64 || dy >= dst.height() as i64 {
            continue;
        }
        let a = p[3] as u32;
        if a == 0 {
            continue;
        }
        let d = dst.get_pixel_mut(dx as u32, dy as u32);
        for i in 0..3 {
            d[i] = ((p[i] as u32 * a + d[i] as u32 * (255 - a)) / 255) as u8;
        }
    }
}

pub fn encode_png(img: &RgbImage) -> Result<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

pub fn encode_jpeg(img: &RgbImage, quality: u8) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    enc.encode_image(img)?;
    Ok(out)
}

/// 현재 세션의 디스플레이 전체를 PNG 로 캡처한다. (bytes, width, height)
///
/// display_id = 0 이면 메인 디스플레이. 긴 변을 max_long_edge 이하로 줄인다 (0 = 원본).
/// **세션 헬퍼(전용 에이전트 로그인 세션) 안에서만 쓸 것** — 사용자 세션에서 부르면
/// 사용자 화면 전체를 찍게 된다.
pub fn capture_display_png(display_id: u32, max_long_edge: u32) -> Result<(Vec<u8>, u32, u32)> {
    let f = display_filter(display_id)?;
    let img = f.capture_fit(max_long_edge)?;
    let (w, h) = img.dimensions();
    Ok((encode_png(&img)?, w, h))
}

/// 창 하나를 긴 변 제한으로 캡처 (필터를 매번 새로 만든다).
pub fn capture_window(wid: u32, max_long_edge: u32) -> Result<(RgbImage, Filter)> {
    let f = window_filter(wid)?;
    let img = f.capture_fit(max_long_edge)?;
    Ok((img, f))
}
