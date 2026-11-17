//! ScreenCaptureKit-based capture.
//!
//! - Single window: `SCContentFilter initWithDesktopIndependentWindow` + `SCScreenshotManager`.
//!   Hidden, minimized and covered windows are captured too (verified on this Mac, typically
//!   50–80 ms, 150–250 ms on the first call). The legacy `CGWindowListCreateImage` returns NULL
//!   for hidden windows, so it isn't used.
//! - Display: `capture_display_png`, for the session helper (dedicated agent login session).
//!
//! The first time a new binary uses SCK, macOS may show a one-time system notice about screen
//! recording; tell the user beforehand. Without screen recording permission this returns an
//! error.
//!
//! Every function blocks (waiting on the completion handler through a channel). From async code,
//! call them inside `spawn_blocking`.

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

/// Shareable content (`onScreenWindowsOnly=false`, so hidden windows are included).
pub fn shareable_content() -> Result<Retained<SCShareableContent>> {
    // initialize the CGS connection when running as a CLI (GUI apps already have one)
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

/// A capture filter that can move between threads. A window filter keeps following the same
/// window as it moves.
pub struct Filter {
    filter: Retained<SCContentFilter>,
    /// Content rect when the filter was created, in points.
    pub content_rect: CGRect,
    /// Points-to-pixels scale (2.0 on Retina).
    pub scale: f64,
    /// For window filters: `SCWindow.frame` (global points, top-left origin).
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

/// Creates a filter for one window (hidden and minimized windows included).
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

/// Display filter for the current session (`display_id` 0 means the main display).
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

/// Output pixel size with the long edge at most `max_long_edge`, keeping the aspect ratio.
/// 0 means original pixels.
pub fn fit_size(w_pt: f64, h_pt: f64, scale: f64, max_long_edge: u32) -> (u32, u32) {
    let (fw, fh) = ((w_pt * scale).max(1.0), (h_pt * scale).max(1.0));
    let k = if max_long_edge == 0 { 1.0 } else { (max_long_edge as f64 / fw.max(fh)).min(1.0) };
    (((fw * k).round() as u32).max(1), ((fh * k).round() as u32).max(1))
}

impl Filter {
    /// Captures one frame with this filter. `(w, h)` is the output size in pixels (SCK does the
    /// scaling).
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

    /// Capture that keeps transparency (RGBA, straight alpha), for compositing sheets and the like.
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

    /// Captures with a long-edge limit.
    pub fn capture_fit(&self, max_long_edge: u32) -> Result<RgbImage> {
        let (w, h) = fit_size(self.content_rect.size.width, self.content_rect.size.height, self.scale, max_long_edge);
        self.capture(w, h)
    }
}

/// `CGImage` (premultiplied BGRA and others) → RGB, compositing transparent areas onto white.
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
    // byte order: 0x2000 = 32-bit little endian (BGRA in memory), otherwise big endian (ARGB/RGBA)
    let little = (info & 0x7000) == 0x2000;
    let alpha_info = info & 0x1f; // 1=premul last, 2=premul first, 3=last, 4=first, 5/6=skip
    let alpha_first = matches!(alpha_info, 2 | 4 | 6);
    let mut out = RgbImage::new(w as u32, h as u32);
    for y in 0..h {
        let row = &bytes[y * bpr..(y * bpr + w * 4).min(bytes.len())];
        for x in 0..w.min(row.len() / 4) {
            let p = &row[x * 4..x * 4 + 4];
            // memory order → logical ARGB/RGBA
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

/// Alpha-blends an RGBA image onto an RGB image at `(x, y)`.
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

/// Captures the current session's whole display as PNG: `(bytes, width, height)`.
///
/// `display_id` 0 means the main display. Shrinks the long edge to `max_long_edge` (0 =
/// original). **Only for use inside the session helper (dedicated agent login session)**;
/// called in the user's session it would capture the user's entire screen.
pub fn capture_display_png(display_id: u32, max_long_edge: u32) -> Result<(Vec<u8>, u32, u32)> {
    let f = display_filter(display_id)?;
    let img = f.capture_fit(max_long_edge)?;
    let (w, h) = img.dimensions();
    Ok((encode_png(&img)?, w, h))
}

/// Captures one window with a long-edge limit (building a fresh filter each time).
pub fn capture_window(wid: u32, max_long_edge: u32) -> Result<(RgbImage, Filter)> {
    let f = window_filter(wid)?;
    let img = f.capture_fit(max_long_edge)?;
    Ok((img, f))
}
