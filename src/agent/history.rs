//! 대화 기록 관리: 스크린샷을 보낼 블록으로 만들고, 오래된 것을 덜어 내고,
//! 바뀌지 않은 화면·창 목록은 다시 보내지 않으며, 너무 길어지면 새 구간을 시작할 요약을 만든다.
//! 모두 요청 크기(토큰·업로드)와 메모리를 줄이기 위한 것이다.

use std::hash::{DefaultHasher, Hash, Hasher};

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};

use super::{LogEntry, LogKind};
use crate::{desk::Desk, llm::short};

/// 요청 본문이 이 크기를 넘으면 새 대화 구간을 시작한다 (스크린샷은 몇 장만 남으므로 보통은 토큰 한도가 먼저 온다)
pub(super) const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
/// 직전 요청의 입력 토큰이 이보다 많으면 다음 턴에 새 대화 구간을 시작한다
/// (매 턴 전체 기록을 다시 보내므로 기록이 길수록 턴마다 비용이 커진다)
pub(super) const MAX_CONTEXT_TOKENS: u64 = 80_000;
/// 이미지로 남겨 둘 최근 스크린샷 수
const KEEP_IMAGES: usize = 3;
/// 한 번에 덜어 낼 최소 장 수. 매 턴 한 장씩 지우면 대화 앞부분이 매번 바뀌어 프롬프트 캐시가 깨지므로
/// KEEP_IMAGES + PRUNE_CHUNK 장이 쌓였을 때 한꺼번에 덜어 낸다.
const PRUNE_CHUNK: usize = 3;
const PRUNED_IMAGE: &str = "(older screenshot removed)";
const SAME_SCREEN: &str = "(screen unchanged since the previous screenshot)";
/// 스크린샷의 JPEG 품질 (PNG 보다 몇 배 작아 업로드·메모리가 준다. 토큰은 픽셀 수로 정해지므로 같다)
const JPEG_QUALITY: u8 = 80;
/// 새 구간 요약에 넣을 최근 기록 수
const SUMMARY_LOG_ENTRIES: usize = 40;

pub(super) fn json_len(v: &Value) -> usize {
    serde_json::to_vec(v).map(|b| b.len()).unwrap_or(0)
}

/// 원래 작업(첫 항목)은 항상 남기고, 이후 지시는 최근 것만 남긴다.
pub(super) fn remember(list: &mut Vec<String>, t: &str) {
    if t.trim().is_empty() {
        return;
    }
    list.push(t.to_string());
    while list.len() > 12 {
        list.remove(1);
    }
}

/// 새 대화 구간의 첫 메시지 텍스트: 원래 작업, 이후 지시, 최근 동작 기록, 직전 동작 결과(텍스트만).
pub(super) fn segment_summary(instructions: &[String], log: &[LogEntry], last: &[Value]) -> String {
    let mut out = String::from(
        "[대화 기록이 너무 커져 새 대화 구간으로 이어갑니다. 이전 대화와 스크린샷은 더 이상 볼 수 없으니, 아래 요약과 현재 화면을 바탕으로 작업을 이어서 하세요.]\n",
    );
    match instructions.split_first() {
        Some((first, rest)) => {
            out.push_str(&format!("\n원래 작업:\n{first}\n"));
            if !rest.is_empty() {
                out.push_str("\n이후 사용자 지시 (오래된 것부터):\n");
                for t in rest {
                    out.push_str(&format!("- {}\n", short(t, 400)));
                }
            }
        }
        None => out.push_str("\n원래 작업: (없음 — 사용자의 지시를 기다리며 진행 중이었습니다)\n"),
    }
    let recent: Vec<&LogEntry> = log.iter().filter(|l| l.kind != LogKind::Thinking).collect();
    if !recent.is_empty() {
        out.push_str("\n최근 기록 (오래된 것부터):\n");
        for l in &recent[recent.len().saturating_sub(SUMMARY_LOG_ENTRIES)..] {
            let k = match l.kind {
                LogKind::User => "사용자",
                LogKind::Assistant => "에이전트",
                LogKind::Action => "동작",
                LogKind::Error => "오류",
                LogKind::Info | LogKind::Thinking => "정보",
            };
            out.push_str(&format!("- [{k}] {}\n", short(&l.text.replace('\n', " "), 200)));
        }
    }
    // 직전 배치의 결과(텍스트) — 짝이 되는 tool_use 는 새 구간에 없으므로 내용만 옮긴다
    let mut results = Vec::new();
    for b in last {
        let texts: Vec<String> = match b["type"].as_str() {
            Some("text") => b["text"].as_str().map(|t| vec![t.to_string()]).unwrap_or_default(),
            Some("tool_result") => match &b["content"] {
                Value::String(t) => vec![t.clone()],
                Value::Array(a) => a.iter().filter_map(|c| c["text"].as_str().map(String::from)).collect(),
                _ => vec![],
            },
            _ => vec![],
        };
        for t in texts {
            if !t.trim().is_empty() {
                results.push(format!("- {}", short(&t.replace('\n', " "), 300)));
            }
        }
    }
    if !results.is_empty() {
        out.push_str("\n직전 동작 결과:\n");
        out.push_str(&results.join("\n"));
        out.push('\n');
    }
    out
}

/// 스크린샷을 보낼 이미지 블록으로 만든다. 요청 크기를 줄이려고 JPEG 로 다시 인코딩한다
/// (이미 JPEG 이면 그대로, 디코딩할 수 없으면 원본 PNG 그대로).
pub(crate) fn image_block(img: &[u8]) -> Value {
    let (media_type, bytes) = match to_jpeg(img) {
        Some(j) => ("image/jpeg", j),
        None => ("image/png", img.to_vec()),
    };
    json!({
        "type": "image",
        "source": { "type": "base64", "media_type": media_type, "data": B64.encode(&bytes) }
    })
}

fn to_jpeg(img: &[u8]) -> Option<Vec<u8>> {
    use image::ImageEncoder;
    if img.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(img.to_vec());
    }
    let rgb = image::load_from_memory(img).ok()?.to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
        .write_image(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8)
        .ok()?;
    Some(out)
}

/// 현재 화면 (작업·지시를 시작하는 메시지용). 직전에 보낸 화면과 같으면 짧은 글.
pub(super) async fn screenshot_context(ws: &Desk, seen: &mut Seen) -> Value {
    match ws.screenshot().await {
        Ok(png) => seen.screen(&png, 0),
        Err(e) => json!({ "type": "text", "text": format!("(스크린샷 실패: {e})") }),
    }
}

/// 블록 하나(와 그 안의 도구 결과 내용)에 든 이미지 수
fn count(b: &Value) -> usize {
    let nested = b["content"].as_array().map_or(0, |c| c.iter().filter(|c| c["type"] == "image").count());
    usize::from(b["type"] == "image") + nested
}

fn images(messages: &[Value]) -> usize {
    messages.iter().flat_map(|m| m["content"].as_array().into_iter().flatten()).map(count).sum()
}

/// 오래된 스크린샷을 글로 바꾼다 (최근 KEEP_IMAGES 장만 이미지로). 줄어든 바이트 수(대략)를 돌려준다.
pub(super) fn prune_images(messages: &mut [Value]) -> usize {
    let total = images(messages);
    if total < KEEP_IMAGES + PRUNE_CHUNK {
        return 0;
    }
    let (mut left, mut freed) = (total - KEEP_IMAGES, 0);
    let mut strip = |b: &mut Value| {
        if left > 0 && b["type"] == "image" {
            freed += b["source"]["data"].as_str().map_or(0, str::len);
            *b = json!({ "type": "text", "text": PRUNED_IMAGE });
            left -= 1;
        }
    };
    // 오래된 것부터: 메시지 순, 블록 순, 도구 결과 안의 순서
    for m in messages.iter_mut() {
        for b in m["content"].as_array_mut().into_iter().flatten() {
            strip(b);
            for c in b["content"].as_array_mut().into_iter().flatten() {
                strip(c);
            }
        }
    }
    freed
}

/// 이전에 보낸 것과 같은 내용(화면·창 목록)을 다시 보내지 않게 기억한다.
/// 모델이 그 내용을 아직 볼 수 있을 때만 생략한다: 최근 도구 호출 `KEEP_TOOL_USES` 개 밖으로 밀려나면
/// 서버 측 정리(Claude)로 지워질 수 있으므로 다시 보낸다. 새 대화 구간에서는 새로 만든다.
#[derive(Default)]
pub(super) struct Seen {
    screen: Sent,
    views: Sent,
}

#[derive(Default)]
struct Sent {
    hash: Option<u64>,
    /// 보낸 뒤로 지나간 도구 호출 수
    since: usize,
}

impl Sent {
    /// 이번(도구 호출 `uses` 개) 결과에 다시 붙여야 하는가
    fn needs(&mut self, data: &[u8], uses: usize) -> bool {
        let mut h = DefaultHasher::new();
        data.hash(&mut h);
        let h = h.finish();
        if self.hash == Some(h) && self.since + uses < crate::llm::KEEP_TOOL_USES {
            self.since += uses;
            return false;
        }
        *self = Sent { hash: Some(h), since: 0 };
        true
    }
}

impl Seen {
    /// 배치(도구 호출 `uses` 개) 뒤 화면 → 보낼 블록. 직전에 보낸 화면과 같으면 짧은 글.
    pub(super) fn screen(&mut self, png: &[u8], uses: usize) -> Value {
        if self.screen.needs(png, uses) {
            image_block(png)
        } else {
            json!({ "type": "text", "text": SAME_SCREEN })
        }
    }

    /// 창·탭 목록을 다시 보내야 하는가
    pub(super) fn views(&mut self, text: &str, uses: usize) -> bool {
        self.views.needs(text.as_bytes(), uses)
    }

    /// 모델이 직접 받은 이미지(screenshot·zoom)가 끼면 '직전 화면' 이 무엇인지 흐려지므로 다음 화면은 그대로 보낸다
    pub(super) fn forget_screen(&mut self) {
        self.screen = Sent::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn screenshots_are_sent_as_jpeg() {
        let p = png(320, 200);
        let b = image_block(&p);
        assert_eq!(b["source"]["media_type"], "image/jpeg");
        let data = B64.decode(b["source"]["data"].as_str().unwrap()).unwrap();
        assert!(data.starts_with(&[0xFF, 0xD8, 0xFF]));
        let back = image::load_from_memory(&data).unwrap();
        assert_eq!((back.width(), back.height()), (320, 200));
        // 디코딩할 수 없는 데이터는 원본 그대로 (PNG 로 표시)
        let b = image_block(b"not an image");
        assert_eq!(b["source"]["media_type"], "image/png");
    }

    #[test]
    fn remember_keeps_original_task() {
        let mut v = Vec::new();
        remember(&mut v, "원래 작업");
        remember(&mut v, "  ");
        for i in 0..30 {
            remember(&mut v, &format!("지시 {i}"));
        }
        assert_eq!(v.len(), 12);
        assert_eq!(v[0], "원래 작업");
        assert_eq!(v.last().unwrap(), "지시 29");
    }

    #[test]
    fn segment_summary_has_task_log_and_results_without_images() {
        let log = vec![
            LogEntry { kind: LogKind::User, text: "원래 작업".into(), secs: 0.0 },
            LogEntry { kind: LogKind::Thinking, text: "비밀 사고".into(), secs: 0.1 },
            LogEntry { kind: LogKind::Action, text: "left_click {\"coordinate\":[1,2]}".into(), secs: 0.2 },
        ];
        let last = vec![
            json!({ "type": "tool_result", "tool_use_id": "t1", "content": [
                { "type": "text", "text": "OK" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/jpeg", "data": "AAAA" } }
            ] }),
            json!({ "type": "tool_result", "tool_use_id": "t2", "is_error": true, "content": "Error: 실패" }),
            json!({ "type": "text", "text": "[사용자 추가 지시] 더 빨리" }),
        ];
        let s = segment_summary(&["원래 작업".into(), "두 번째 지시".into()], &log, &last);
        assert!(s.contains("원래 작업:\n원래 작업"));
        assert!(s.contains("- 두 번째 지시"));
        assert!(s.contains("[동작] left_click"));
        assert!(!s.contains("비밀 사고"), "사고 블록은 요약에 넣지 않음");
        assert!(s.contains("- OK") && s.contains("Error: 실패") && s.contains("더 빨리"));
        assert!(!s.contains("AAAA"), "이미지 데이터는 넣지 않음");
    }

    #[test]
    fn jpeg_shrinks_requests() {
        // 잡음이 많은 화면에서도 JPEG 는 PNG 보다 작아야 한다 (요청 크기 한도까지 훨씬 오래 간다)
        let img = image::RgbaImage::from_fn(1280, 800, |x, y| {
            let v = ((x * 7919 + y * 104729) % 251) as u8;
            image::Rgba([v, v.wrapping_mul(3), v.wrapping_add(x as u8), 255])
        });
        let mut p = std::io::Cursor::new(Vec::new());
        img.write_to(&mut p, image::ImageFormat::Png).unwrap();
        let p = p.into_inner();
        let j = json_len(&image_block(&p));
        let raw = B64.encode(&p).len();
        assert!(j < raw, "jpeg {j} >= png {raw}");
    }

    fn img() -> Value {
        json!({ "type": "image", "source": { "type": "base64", "media_type": "image/jpeg", "data": "AAAA" } })
    }

    /// 최근 KEEP_IMAGES 장만 남기되, PRUNE_CHUNK 장이 더 쌓일 때까지는 건드리지 않는다 (프롬프트 캐시 보존)
    #[test]
    fn prunes_old_images_in_chunks() {
        let turn = |n: usize| {
            json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": format!("t{n}"), "content": [{ "type": "text", "text": "OK" }, img()] }] })
        };
        let mut m = vec![json!({ "role": "user", "content": [{ "type": "text", "text": "작업" }, img()] })];
        m.extend((1..KEEP_IMAGES + PRUNE_CHUNK - 1).map(turn));
        assert_eq!(prune_images(&mut m), 0, "아직 덜어 낼 만큼 쌓이지 않음");
        m.push(turn(99));
        assert_eq!(prune_images(&mut m), 4 * PRUNE_CHUNK);
        assert_eq!(images(&m), KEEP_IMAGES);
        assert_eq!(m[0]["content"][1]["text"], PRUNED_IMAGE, "가장 오래된 것부터");
        assert_eq!(m.last().unwrap()["content"][0]["content"][1]["type"], "image", "가장 최근 것은 남음");
        assert_eq!(prune_images(&mut m), 0);
    }

    /// 같은 화면은 짧은 글로, 단 보낸 화면이 최근 도구 호출 범위를 벗어나면 다시 이미지로
    #[test]
    fn unchanged_screens_are_not_resent() {
        let (a, b) = (png(8, 8), png(9, 9));
        let mut s = Seen::default();
        assert_eq!(s.screen(&a, 2)["type"], "image");
        assert_eq!(s.screen(&a, 2)["type"], "text");
        assert_eq!(s.screen(&a, 3)["type"], "text");
        assert_eq!(s.screen(&a, 1)["type"], "image", "보낸 화면이 최근 호출 범위 밖으로 밀려남");
        assert_eq!(s.screen(&b, 1)["type"], "image");
        s.forget_screen();
        assert_eq!(s.screen(&b, 1)["type"], "image", "screenshot·zoom 뒤에는 다시 보냄");
        assert!(s.views("Tabs", 1) && !s.views("Tabs", 1) && s.views("Tabs2", 1));
    }
}
