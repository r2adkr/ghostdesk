//! Claude Messages API 클라이언트 (Rust 공식 SDK 가 없으므로 raw HTTP).
//!
//! - computer use: `computer_toolset_20260801` (GA, beta 헤더 없음)
//! - adaptive thinking + `display: "updates"` 로 도구 호출 사이의 진행 메모를 받는다
//! - 오래된 스크린샷은 서버 측 context editing(`clear_tool_uses_20250919`)으로 정리
//! - 안전 분류기 거절 시 서버 측 fallback(`fallbacks: "default"`)

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

pub const MODELS: &[&str] = &["claude-opus-5-5", "claude-sonnet-5-5", "claude-fable-5-2", "claude-haiku-5-5"];
pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

#[derive(Clone)]
pub struct Llm {
    http: reqwest::Client,
    pub api_key: String,
    pub model: String,
    pub effort: String,
}

impl Llm {
    pub fn new(api_key: String, model: String, effort: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .expect("HTTP 클라이언트 생성 실패");
        Llm { http, api_key, model, effort }
    }

    fn is_haiku(&self) -> bool {
        self.model.contains("haiku")
    }

    /// 백엔드 종류에 맞는 도구 목록. 대화 중에는 바꾸지 않는다.
    pub fn tools(kind: crate::desk::DeskKind) -> Value {
        use crate::desk::DeskKind;
        let mut tools = vec![json!({ "type": "computer_toolset_20260801" })];
        match kind {
            DeskKind::Browser => tools.push(Self::browser_control_tool()),
            DeskKind::Native => tools.push(crate::native::app_control_tool()),
            DeskKind::VDisplay => tools.push(crate::vdisplay::app_control_tool()),
            DeskKind::Electron => tools.push(Self::app_menu_tool()),
            DeskKind::Remote | DeskKind::Session => {}
        }
        tools.push(Self::ask_user_tool());
        Value::Array(tools)
    }

    fn browser_control_tool() -> Value {
        json!({
                "name": "browser_control",
                "description": "Control the isolated browser outside the page viewport. The screen you see is only the page viewport (there is no address bar or tab strip), so use this tool to open URLs, go back/forward, reload and manage tabs. Actions: navigate(url), back, forward, reload, new_tab(url optional), switch_tab(index), close_tab(index), list_tabs.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["navigate", "back", "forward", "reload", "new_tab", "switch_tab", "close_tab", "list_tabs"] },
                        "url": { "type": "string", "description": "URL or search words for navigate/new_tab" },
                        "index": { "type": "integer", "description": "Tab index for switch_tab/close_tab" }
                    },
                    "required": ["action"]
                }
            })
    }

    fn app_menu_tool() -> Value {
        json!({
            "name": "app_menu",
            "description": "List or click items of the desktop app's native menus, which are not visible in screenshots. source=app is the app's menu bar; source=context is the context menu the app last tried to open (after a right click). Use action=list first, then action=click with a path like \"File > New Text File\". Native menu shortcuts cannot be triggered with keys, so use this tool or the app's own command palette.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "click"] },
                    "source": { "type": "string", "enum": ["app", "context"] },
                    "path": { "type": "string", "description": "Menu path for click, items separated by ' > '" }
                },
                "required": ["action"]
            }
        })
    }

    fn ask_user_tool() -> Value {
        json!({
                "name": "ask_user",
                "description": "Ask the human operator a question and wait for the answer. Use it when you need information only they have, a decision, a 2FA/CAPTCHA step, a login that is missing in the cloned profile, or explicit confirmation before an irreversible action (sending messages, purchases, deleting data, submitting forms).",
                "input_schema": {
                    "type": "object",
                    "properties": { "question": { "type": "string" } },
                    "required": ["question"]
                }
            })
    }

    pub async fn send(&self, system: &str, tools: &Value, messages: &[Value]) -> Result<Value> {
        let mut betas = vec!["context-management-2025-06-27"];
        let mut thinking = json!({ "type": "adaptive" });
        let mut body = json!({
            "model": self.model,
            "max_tokens": 16000,
            "system": system,
            "output_config": { "effort": self.effort },
            "tools": tools,
            "context_management": {
                "edits": [{
                    "type": "clear_tool_uses_20250919",
                    "trigger": { "type": "input_tokens", "value": 60000 },
                    "keep": { "type": "tool_uses", "value": 6 },
                    "clear_at_least": { "type": "input_tokens", "value": 10000 }
                }]
            },
            "messages": messages,
            // 자동 프롬프트 캐싱: 매 턴 늘어나는 대화의 앞부분을 캐시해 지연·비용을 줄인다
            "cache_control": { "type": "ephemeral" },
        });
        if !self.is_haiku() {
            betas.push("thinking-display-updates-2026-08-18");
            betas.push("server-side-fallback-2026-07-01");
            thinking["display"] = json!("updates");
            body["fallbacks"] = json!("default");
        }
        body["thinking"] = thinking;

        let mut attempt = 0;
        loop {
            attempt += 1;
            let res = self
                .http
                .post(format!("{}/v1/messages", base_url()))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", betas.join(","))
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await;
            match res {
                Ok(r) => {
                    let status = r.status();
                    let text = r.text().await.unwrap_or_default();
                    if status.is_success() {
                        return serde_json::from_str(&text).map_err(|e| anyhow!("응답 JSON 파싱 실패: {e}"));
                    }
                    let retryable = matches!(status.as_u16(), 408 | 409 | 429 | 500 | 502 | 503 | 504 | 529);
                    if !retryable || attempt >= 5 {
                        bail!("Claude API 오류 {status}: {}", short(&text, 800));
                    }
                }
                Err(e) => {
                    if attempt >= 5 {
                        bail!("Claude API 연결 실패: {e}");
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2u64.pow(attempt.min(5)))).await;
        }
    }
}

/// 프록시/테스트용으로 ANTHROPIC_BASE_URL 을 존중한다.
fn base_url() -> String {
    std::env::var("ANTHROPIC_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "https://api.anthropic.com".into())
}

pub fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}
