//! Conversation history management: turns screenshots into blocks to send, prunes old ones,
//! skips resending unchanged screens and window lists, and builds a summary to start a fresh
//! segment when things get too long. All of it is about keeping request size (tokens, upload)
//! and memory down.

use std::hash::{DefaultHasher, Hash, Hasher};

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};

use super::{LogEntry, LogKind};
use crate::{desk::Desk, llm::short};

/// Start a new conversation segment when the request body exceeds this. Only a few screenshots
/// are kept, so the token limit usually hits first.
pub(super) const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
/// Start a new segment on the next turn when the previous request's input tokens exceed this.
///
/// The whole history is resent every turn, so the longer it gets the more each turn costs.
pub(super) const MAX_CONTEXT_TOKENS: u64 = 80_000;
/// Number of recent screenshots kept as images.
const KEEP_IMAGES: usize = 3;
/// Minimum number of images pruned at once.
///
/// Pruning one per turn would change the start of the conversation every time and break the
/// prompt cache, so they are pruned in bulk once KEEP_IMAGES + PRUNE_CHUNK have accumulated.
const PRUNE_CHUNK: usize = 3;
const PRUNED_IMAGE: &str = "(older screenshot removed)";
/// Placeholder left where an old tool result was cleared.
const CLEARED_RESULT: &str = "(older tool result cleared to save context)";
/// For providers whose servers don't clear old tool results, clear them ourselves once the
/// previous input tokens exceed this (same threshold as Claude's server).
pub(super) const CLEAR_TOOL_RESULTS_AT: u64 = crate::llm::CLEAR_TOOL_USES_AT;
const SAME_SCREEN: &str = "(screen unchanged since the previous screenshot)";
/// JPEG quality for screenshots. Several times smaller than PNG, which saves upload and memory;
/// tokens depend on pixel count, so they're the same.
const JPEG_QUALITY: u8 = 80;
/// Number of recent log entries in a new segment's summary.
const SUMMARY_LOG_ENTRIES: usize = 40;

pub(super) fn json_len(v: &Value) -> usize {
    serde_json::to_vec(v).map(|b| b.len()).unwrap_or(0)
}

/// Always keeps the original task (first item); later instructions keep only the most recent.
pub(super) fn remember(list: &mut Vec<String>, t: &str) {
    if t.trim().is_empty() {
        return;
    }
    list.push(t.to_string());
    while list.len() > 12 {
        list.remove(1);
    }
}

/// Text of a new segment's first message: original task, later instructions, recent action log,
/// and the previous action results (text only).
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
    // previous batch results (text): their matching tool_use isn't in the new segment, so only the
    // content moves over
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

/// Turns a screenshot into an image block to send, re-encoded as JPEG to shrink the request.
///
/// JPEG input is passed through; anything that can't be decoded goes out as the original PNG.
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

/// The current screen, for messages that start a task or instruction. A short note if it's the
/// same as the last one sent.
pub(super) async fn screenshot_context(ws: &Desk, seen: &mut Seen) -> Value {
    match ws.screenshot().await {
        Ok(png) => seen.screen(&png, 0),
        Err(e) => json!({ "type": "text", "text": format!("(스크린샷 실패: {e})") }),
    }
}

/// Number of images in a block (including inside tool result content).
fn count(b: &Value) -> usize {
    let nested = b["content"].as_array().map_or(0, |c| c.iter().filter(|c| c["type"] == "image").count());
    usize::from(b["type"] == "image") + nested
}

fn images(messages: &[Value]) -> usize {
    messages.iter().flat_map(|m| m["content"].as_array().into_iter().flatten()).map(count).sum()
}

/// Replaces old screenshots with text, keeping only the last KEEP_IMAGES as images. Returns
/// roughly how many bytes were saved.
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
    // oldest first: by message, then block, then position inside tool results
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

/// Replaces the text of all but the last `keep` tool results with a short marker (images are
/// handled separately by `prune_images`).
///
/// For providers whose servers don't do this (anything but Claude). Changing the start of the
/// conversation breaks the prompt cache, so this runs in bulk only once the history passes
/// `CLEAR_TOOL_RESULTS_AT`, not every turn. Returns roughly how many bytes were saved.
pub(super) fn clear_old_tool_results(messages: &mut [Value], keep: usize) -> usize {
    let is_result = |b: &Value| b["type"] == "tool_result";
    let total = messages.iter().flat_map(|m| m["content"].as_array().into_iter().flatten()).filter(|b| is_result(b)).count();
    let mut left = total.saturating_sub(keep);
    let mut freed = 0;
    for b in messages.iter_mut().flat_map(|m| m["content"].as_array_mut().into_iter().flatten()) {
        if left == 0 {
            break;
        }
        if !is_result(b) {
            continue;
        }
        left -= 1;
        match &mut b["content"] {
            Value::String(t) if t != CLEARED_RESULT => {
                freed += t.len();
                *t = CLEARED_RESULT.into();
            }
            Value::Array(parts) => {
                for c in parts.iter_mut().filter(|c| c["type"] == "text" && c["text"] != CLEARED_RESULT) {
                    freed += c["text"].as_str().map_or(0, str::len);
                    *c = json!({ "type": "text", "text": CLEARED_RESULT });
                }
            }
            _ => {}
        }
    }
    freed
}

/// Remembers what was already sent (screen, window list) so it isn't sent again.
///
/// Only skipped while the model can still see it: once it falls outside the last
/// `KEEP_TOOL_USES` tool calls, server-side clearing (Claude) may have removed it, so it's sent
/// again. Rebuilt for each new segment.
#[derive(Default)]
pub(super) struct Seen {
    screen: Sent,
    views: Sent,
}

#[derive(Default)]
struct Sent {
    hash: Option<u64>,
    /// Tool calls since it was sent.
    since: usize,
}

impl Sent {
    /// Whether it needs re-attaching to this result (after `uses` tool calls).
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
    /// Screen after a batch (`uses` tool calls) → block to send. A short note if it's the same as the
    /// last one sent.
    pub(super) fn screen(&mut self, png: &[u8], uses: usize) -> Value {
        if self.screen.needs(png, uses) {
            image_block(png)
        } else {
            json!({ "type": "text", "text": SAME_SCREEN })
        }
    }

    /// Whether the window/tab list needs resending.
    pub(super) fn views(&mut self, text: &str, uses: usize) -> bool {
        self.views.needs(text.as_bytes(), uses)
    }

    /// Images the model received directly (screenshot, zoom) blur what "the previous screen" was, so
    /// the next screen is sent in full.
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
    fn old_tool_results_are_cleared_but_recent_ones_stay() {
        let result = |i: usize| json!({ "type": "tool_result", "tool_use_id": format!("t{i}"), "content": [{ "type": "text", "text": format!("long output {i}") }] });
        let mut messages: Vec<Value> = (0..5).map(|i| json!({ "role": "user", "content": [result(i)] })).collect();
        messages[0]["content"][0]["content"] = json!("plain string result");
        let freed = clear_old_tool_results(&mut messages, 2);
        assert!(freed > 0);
        assert_eq!(messages[0]["content"][0]["content"], CLEARED_RESULT);
        for m in &messages[1..3] {
            assert_eq!(m["content"][0]["content"][0]["text"], CLEARED_RESULT);
        }
        for (i, m) in messages.iter().enumerate().skip(3) {
            assert_eq!(m["content"][0]["content"][0]["text"], format!("long output {i}"), "최근 결과는 그대로");
        }
        // tool_use_id stays so pairs still match; calling again has nothing more to shrink
        assert_eq!(messages[1]["content"][0]["tool_use_id"], "t1");
        assert_eq!(clear_old_tool_results(&mut messages, 2), 0);
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
        // undecodable data passes through unchanged (labeled PNG)
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
        // even on a noisy screen JPEG must beat PNG (lasting much longer before the request size cap)
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

    /// Keeps only the last KEEP_IMAGES but leaves things alone until PRUNE_CHUNK more pile up
    /// (preserving the prompt cache).
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

    /// The same screen becomes a short note, but turns back into an image once the screen sent
    /// falls outside the recent tool-call window.
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
