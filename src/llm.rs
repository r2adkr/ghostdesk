//! AI API clients, over raw HTTP since there is no official Rust SDK.
//!
//! The agent always keeps the conversation in Claude Messages format. Claude providers get it
//! as is (`anthropic`); everyone else gets it converted to the OpenAI-compatible format right
//! before sending, with the reply converted back (`openai`). The agent never needs to know
//! which provider it is talking to.

mod anthropic;
mod openai;
mod provider;

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

pub(crate) use anthropic::{CLEAR_TOOL_USES_AT, KEEP_TOOL_USES};
pub use provider::Provider;

pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

#[derive(Clone)]
pub struct Llm {
    http: reqwest::Client,
    pub provider: Provider,
    /// API base URL (`Provider::default_base_url` or a user-supplied OpenAI-compatible URL).
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub effort: String,
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().timeout(Duration::from_secs(600)).build().expect("HTTP 클라이언트 생성 실패")
}

impl Llm {
    pub fn new(provider: Provider, base_url: String, api_key: String, model: String, effort: String) -> Self {
        Llm { http: http(), provider, base_url, api_key, model, effort }
    }

    /// Tool definitions for a backend kind, in Claude format. Never changed mid-conversation.
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

    /// Sends one turn and returns the reply in Claude format (`content`, `stop_reason`, `usage`).
    pub async fn send(&self, system: &str, tools: &Value, messages: &[Value]) -> Result<Value> {
        if self.provider == Provider::Anthropic {
            self.send_anthropic(system, tools, messages).await
        } else {
            self.send_openai(system, tools, messages).await
        }
    }

    /// POSTs `body`, retrying transient errors up to 5 times with exponential backoff.
    pub(super) async fn post(&self, url: &str, body: &Value, extra: &[(&str, &str)]) -> Result<Value> {
        let name = self.provider.short_name();
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut req = auth(self.http.post(url), self.provider, &self.api_key).header("content-type", "application/json");
            for (k, v) in extra {
                req = req.header(*k, *v);
            }
            match req.json(body).send().await {
                Ok(r) => {
                    let status = r.status();
                    let text = r.text().await.unwrap_or_default();
                    if status.is_success() {
                        return serde_json::from_str(&text).map_err(|e| anyhow!("응답 JSON 파싱 실패: {e}"));
                    }
                    let retryable = matches!(status.as_u16(), 408 | 409 | 429 | 500 | 502 | 503 | 504 | 529);
                    if !retryable || attempt >= 5 {
                        bail!("{name} API 오류 {status}: {}", short(&text, 800));
                    }
                }
                Err(e) => {
                    if attempt >= 5 {
                        bail!("{name} API 연결 실패: {e}");
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2u64.pow(attempt.min(5)))).await;
        }
    }
}

fn auth(req: reqwest::RequestBuilder, p: Provider, key: &str) -> reqwest::RequestBuilder {
    match p {
        Provider::Anthropic => req.header("x-api-key", key),
        _ if key.trim().is_empty() => req,
        _ => req.bearer_auth(key.trim()),
    }
}

/// Model ids the provider actually offers (`GET /models`), minus models that can't do text or
/// image chat.
pub async fn list_models(p: Provider, base_url: &str, key: &str) -> Result<Vec<String>> {
    const SKIP: &[&str] = &["embed", "tts", "whisper", "dall-e", "image", "audio", "realtime", "moderation", "transcribe", "veo", "imagen", "aqa"];
    let mut ids: Vec<String> = Vec::new();
    // Anthropic returns at most 100 per page, so follow `after_id` until `has_more` is false
    let mut after: Option<String> = None;
    for _ in 0..20 {
        let url = match (p, &after) {
            (Provider::Anthropic, None) => format!("{base_url}/v1/models?limit=100"),
            (Provider::Anthropic, Some(a)) => format!("{base_url}/v1/models?limit=100&after_id={a}"),
            _ => format!("{base_url}/models"),
        };
        let mut req = auth(http().get(url), p, key).timeout(Duration::from_secs(20));
        if p == Provider::Anthropic {
            req = req.header("anthropic-version", "2023-06-01");
        }
        let r = req.send().await.map_err(|e| anyhow!("{} 에 연결하지 못했습니다: {e}", p.short_name()))?;
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("{} 오류 {status}: {}", p.short_name(), short(&text, 200));
        }
        let v: Value = serde_json::from_str(&text)?;
        ids.extend(
            v["data"]
                .as_array()
                .or_else(|| v["models"].as_array())
                .into_iter()
                .flatten()
                .filter_map(|m| m["id"].as_str().or_else(|| m["name"].as_str()))
                .map(|id| id.trim_start_matches("models/").to_string())
                .filter(|id| !SKIP.iter().any(|s| id.to_lowercase().contains(s))),
        );
        match v["last_id"].as_str() {
            Some(last) if p == Provider::Anthropic && v["has_more"].as_bool() == Some(true) => after = Some(last.to_string()),
            _ => break,
        }
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

pub fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}
