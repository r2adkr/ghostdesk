//! 화면 밖 창 캡처: PrintWindow(PW_RENDERFULLCONTENT) → 32비트 DIB → 내용 영역만 잘라 → 긴 변 제한으로 축소.
//!
//! PrintWindow 는 DWM 이 가진 창 표면을 그려 주므로 창이 어느 모니터에도 걸치지 않아도(화면 밖) 찍힌다.
//! 단, 창이 **보이는 상태**(WS_VISIBLE, 최소화 아님)여야 한다 — 그래서 숨기지 않고 화면 밖에 둔다.

use anyhow::{bail, Result};
use image::RgbImage;
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, DIB_RGB_COLORS,
};

use super::w32::{self, Hwnd};

const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;

/// 캡처 한 장과 그 시점의 좌표 정보
pub struct Shot {
    pub img: RgbImage,
    /// 내용 영역 크기 (실제 픽셀)
    pub client_w: u32,
    pub client_h: u32,
}

/// 긴 변이 max_long_edge 이하가 되도록 (비율 유지) 출력 크기를 정한다. 0 = 원본.
pub fn fit_size(w: u32, h: u32, max_long_edge: u32) -> (u32, u32) {
    let (fw, fh) = (w.max(1) as f64, h.max(1) as f64);
    let k = if max_long_edge == 0 { 1.0 } else { (max_long_edge as f64 / fw.max(fh)).min(1.0) };
    (((fw * k).round() as u32).max(1), ((fh * k).round() as u32).max(1))
}

/// BGRA(위→아래) 버퍼에서 (x, y, w, h) 영역을 RGB 이미지로
pub fn crop_bgra(buf: &[u8], stride_px: usize, x: usize, y: usize, w: usize, h: usize) -> RgbImage {
    let mut img = RgbImage::new(w as u32, h as u32);
    for row in 0..h {
        let src = ((y + row) * stride_px + x) * 4;
        for col in 0..w {
            let i = src + col * 4;
            if i + 2 < buf.len() {
                img.put_pixel(col as u32, row as u32, image::Rgb([buf[i + 2], buf[i + 1], buf[i]]));
            }
        }
    }
    img
}

/// 창 전체를 PrintWindow 로 그려 BGRA 버퍼로: (버퍼, 너비, 높이)
fn print_window_bgra(h: Hwnd) -> Result<(Vec<u8>, usize, usize)> {
    let (l, t, r, b) = w32::window_rect(h);
    let (ww, wh) = ((r - l).max(0) as usize, (b - t).max(0) as usize);
    if ww == 0 || wh == 0 || ww > 16384 || wh > 16384 {
        bail!("창 크기가 올바르지 않습니다 ({ww}x{wh})");
    }
    unsafe {
        // 화면 DC(GetDC(NULL))는 숨은 데스크톱 스레드에서 실패하므로 쓰지 않는다
        let mem = CreateCompatibleDC(std::ptr::null_mut());
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: ww as i32,
            biHeight: -(wh as i32), // 위→아래
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if dib.is_null() || bits.is_null() {
            DeleteDC(mem);
            bail!("캡처용 비트맵을 만들지 못했습니다");
        }
        let old = SelectObject(mem, dib as _);
        let ok = w32::print_window(h, mem as _, PW_RENDERFULLCONTENT);
        GdiFlush();
        let buf = std::slice::from_raw_parts(bits as *const u8, ww * wh * 4).to_vec();
        SelectObject(mem, old);
        DeleteObject(dib as _);
        DeleteDC(mem);
        if !ok {
            bail!("PrintWindow 가 실패했습니다 (창이 닫혔거나 그릴 수 없는 상태)");
        }
        Ok((buf, ww, wh))
    }
}

/// 창의 내용 영역을 캡처한다 (긴 변 ≤ max_long_edge, 0 이면 원본 픽셀)
pub fn capture_client(h: Hwnd, max_long_edge: u32) -> Result<Shot> {
    if !w32::is_window(h) {
        bail!("창이 닫혔습니다");
    }
    if w32::is_hung(h) {
        bail!("앱이 응답하지 않아 화면을 찍을 수 없습니다 (잠시 뒤 다시 시도하세요)");
    }
    if w32::is_iconic(h) || !w32::is_visible(h) {
        bail!("창이 최소화되었거나 숨겨져 있어 찍을 수 없습니다");
    }
    let (cw, ch) = w32::client_size(h);
    if cw <= 0 || ch <= 0 {
        bail!("창의 내용 영역이 비어 있습니다");
    }
    let (buf, ww, wh) = print_window_bgra(h)?;
    let (l, t, _, _) = w32::window_rect(h);
    let (ox, oy) = w32::client_origin(h);
    let x = (ox - l).clamp(0, ww as i32) as usize;
    let y = (oy - t).clamp(0, wh as i32) as usize;
    let w = (cw as usize).min(ww - x);
    let hh = (ch as usize).min(wh - y);
    if w == 0 || hh == 0 {
        bail!("창의 내용 영역을 잘라내지 못했습니다");
    }
    let full = crop_bgra(&buf, ww, x, y, w, hh);
    let (tw, th) = fit_size(w as u32, hh as u32, max_long_edge);
    let img = if (tw, th) == (w as u32, hh as u32) {
        full
    } else {
        image::imageops::resize(&full, tw, th, image::imageops::FilterType::Triangle)
    };
    Ok(Shot { img, client_w: w as u32, client_h: hh as u32 })
}

/// 이미지가 거의 한 색인가 (그리기 실패 감지용)
pub fn is_blank(img: &RgbImage) -> bool {
    let Some(first) = img.pixels().next().copied() else { return true };
    let step = ((img.width() * img.height()) as usize / 4000).max(1);
    !img.pixels().step_by(step).any(|p| {
        (p[0] as i32 - first[0] as i32).abs() > 8 || (p[1] as i32 - first[1] as i32).abs() > 8 || (p[2] as i32 - first[2] as i32).abs() > 8
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_and_crop() {
        assert_eq!(fit_size(2000, 1000, 1280), (1280, 640));
        assert_eq!(fit_size(800, 600, 1280), (800, 600));
        assert_eq!(fit_size(1000, 3000, 1000), (333, 1000));
        assert_eq!(fit_size(500, 400, 0), (500, 400));
        // 3x2 BGRA, (1,1) 에서 2x1 자르기
        let mut buf = vec![0u8; 3 * 2 * 4];
        let i = (3 + 1) * 4;
        buf[i] = 10; // B
        buf[i + 1] = 20; // G
        buf[i + 2] = 30; // R
        let img = crop_bgra(&buf, 3, 1, 1, 2, 1);
        assert_eq!(img.dimensions(), (2, 1));
        assert_eq!(img.get_pixel(0, 0).0, [30, 20, 10]);
        assert!(is_blank(&RgbImage::new(10, 10)));
        assert!(!is_blank(&img));
    }
}
