//! AI API 클라이언트 (Rust 공식 SDK 가 없으므로 raw HTTP).
//!
//! 대화는 에이전트 쪽에서 늘 Claude Messages 형식으로 쌓는다. 제공자가 Claude 면 그대로(anthropic),
//! 아니면 보내기 직전에 OpenAI 호환 형식으로 바꿔 보내고 답도 Claude 형식으로 되돌린다(openai).
//! 그래서 에이전트는 제공자를 몰라도 된다.

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
    pub api_key: String,
    pub model: String,
    pub effort: String,
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().timeout(Duration::from_secs(600)).build().expect("HTTP 클라이언트 생성 실패")
}

impl Llm {
    pub fn new(provider: Provider, api_key: String, model: String, effort: String) -> Self {
        Llm { http: http(), provider, api_key, model, effort }
    }

    /// 백엔드 종류에 맞는 도구 목록 (Claude 형식). 대화 중에는 바꾸지 않는다.
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

    /// 한 턴을 보내고 Claude 형식의 답(`content`·`stop_reason`·`usage`)을 돌려준다.
    pub async fn send(&self, system: &str, tools: &Value, messages: &[Value]) -> Result<Value> {
        if self.provider == Provider::Anthropic {
            self.send_anthropic(system, tools, messages).await
        } else {
            self.send_openai(system, tools, messages).await
        }
    }

    /// 재시도(일시 오류는 지수 대기 후 최대 5번)를 포함한 POST
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

/// 제공자가 실제로 쓸 수 있는 모델 이름 목록 (`GET /models`). 글·이미지 대화와 상관없는 모델은 뺀다.
pub async fn list_models(p: Provider, key: &str) -> Result<Vec<String>> {
    const SKIP: &[&str] = &["embed", "tts", "whisper", "dall-e", "image", "audio", "realtime", "moderation", "transcribe", "veo", "imagen", "aqa"];
    let mut ids: Vec<String> = Vec::new();
    // Anthropic 은 한 번에 100개까지만 주므로 has_more 가 꺼질 때까지 다음 쪽(after_id)을 이어 받는다
    let mut after: Option<String> = None;
    for _ in 0..20 {
        let url = match (p, &after) {
            (Provider::Anthropic, None) => format!("{}/v1/models?limit=100", p.base_url()),
            (Provider::Anthropic, Some(a)) => format!("{}/v1/models?limit=100&after_id={a}", p.base_url()),
            _ => format!("{}/models", p.base_url()),
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
