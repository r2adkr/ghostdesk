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

pub(crate) fn cli_tools(tools: &Value) -> Value { openai::to_tools(tools) }

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
        let mut tools = if kind == DeskKind::Ssh { Vec::new() } else { vec![json!({ "type": "computer_toolset_20260801" })] };
        match kind {
            DeskKind::Browser => tools.push(Self::browser_control_tool()),
            DeskKind::Native => tools.push(crate::native::app_control_tool()),
            DeskKind::VDisplay => tools.push(crate::vdisplay::app_control_tool()),
            DeskKind::Electron => tools.push(Self::app_menu_tool()),
            DeskKind::Session => {
                #[cfg(windows)]
                tools.push(Self::session_control_tool());
            }
            DeskKind::Remote | DeskKind::Ssh => tools.push(crate::ssh::tool_definition()),
        }
        tools.push(Self::ask_user_tool());
        Value::Array(tools)
    }

    #[cfg(windows)]
    fn session_control_tool() -> Value {
        let mut tool = crate::native::app_control_tool();
        tool["description"] = json!("Inspect and operate windows and UI Automation elements inside the Windows child session. Actions: list_windows, switch_window(index), ui_tree(max_depth optional), press(id), set_value(id,text), focus(id), select(id), choose(id,option), read(id). Use set_value for exact long-text replacement and read to verify it. Element IDs belong to the last ui_tree and observed foreground window; refresh the tree after switching windows or changing focus. Operate application menus using screenshots and keyboard shortcuts.");
        tool["input_schema"]["properties"]["action"]["enum"] = json!(["list_windows", "switch_window", "ui_tree", "press", "set_value", "focus", "select", "choose", "read"]);
        tool["input_schema"]["properties"].as_object_mut().unwrap().remove("path");
        tool
    }

    fn browser_control_tool() -> Value {
        json!({
                "name": "browser_control",
                "description": "Control the isolated browser outside the page viewport. There is no address bar or tab strip. Actions: navigate(url), back, forward, reload, new_tab(url optional), switch_tab(index), close_tab(index), list_tabs. Read pending JavaScript confirmations/prompts with dialog; respond_dialog requires the returned id and explicit accept=true/false, plus prompt_text for a prompt. Dialogs are never accepted automatically. A dialog can interrupt an input that already ran; query it instead of repeating the input. Follow the task's authorization before confirming an irreversible action.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["navigate", "back", "forward", "reload", "new_tab", "switch_tab", "close_tab", "list_tabs", "dialog", "respond_dialog"] },
                        "url": { "type": "string", "description": "URL or search words for navigate/new_tab" },
                        "index": { "type": "integer", "description": "Tab index for switch_tab/close_tab" },
                        "id": { "type": "integer", "minimum": 1, "description": "respond_dialog: current dialog ID from dialog" },
                        "accept": { "type": "boolean", "description": "respond_dialog: required explicit accept or cancel" },
                        "prompt_text": { "type": "string", "description": "respond_dialog: answer for a JavaScript prompt" }
                    },
                    "required": ["action"]
                }
            })
    }

    fn app_menu_tool() -> Value {
        json!({
            "name": "app_menu",
            "description": "List or click hidden native menus: list, then click(path), with source=app or context. Read pending message/JavaScript dialogs with dialog; respond_dialog requires its exact source and id. For source=native provide response, the zero-based button index. For source=javascript provide explicit accept=true/false and optional prompt_text. Do not repeat input interrupted by a dialog or accept irreversible actions without task authorization. Native file open/save dialogs remain cancelled; use in-window alternatives.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "click", "dialog", "respond_dialog"] },
                    "source": { "type": "string", "enum": ["app", "context", "javascript", "native"], "description": "list/click: app or context; respond_dialog: source returned by dialog" },
                    "path": { "type": "string", "description": "Menu path for click, items separated by ' > '" },
                    "id": { "type": "integer", "minimum": 1, "description": "respond_dialog: current dialog ID" },
                    "response": { "type": "integer", "minimum": 0, "description": "native respond_dialog: zero-based button index" },
                    "accept": { "type": "boolean", "description": "javascript respond_dialog: required explicit accept or cancel" },
                    "prompt_text": { "type": "string", "description": "javascript respond_dialog: prompt answer" }
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
        if self.provider.is_cli() {
            crate::cli::send(self.provider, &self.base_url, &self.model, system, tools, messages).await
        } else if self.provider == Provider::Anthropic {
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
    if p.is_cli() { return crate::cli::models(p, base_url).await; }
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

#[cfg(test)]
mod tool_contract_tests {
    use super::*;
    use crate::desk::DeskKind;

    #[test]
    fn dialog_tools_require_explicit_response_fields() {
        for (kind, name) in [(DeskKind::Browser, "browser_control"), (DeskKind::Electron, "app_menu")] {
            let tools = Llm::tools(kind);
            let tool = tools.as_array().unwrap().iter().find(|tool| tool["name"] == name).unwrap();
            let props = &tool["input_schema"]["properties"];
            for action in ["dialog", "respond_dialog"] {
                assert!(props["action"]["enum"].as_array().unwrap().contains(&json!(action)), "{name}: {action}");
            }
            assert_eq!(props["id"]["type"], "integer");
            assert_eq!(props["accept"]["type"], "boolean");
            assert!(props["accept"].get("default").is_none(), "accepting a confirmation must never be implicit");
            assert_eq!(props["prompt_text"]["type"], "string");
            if kind == DeskKind::Electron {
                assert!(props["source"]["enum"].as_array().unwrap().contains(&json!("native")));
                assert!(props["source"]["enum"].as_array().unwrap().contains(&json!("javascript")));
                assert_eq!(props["response"]["minimum"], 0);
            }
        }
    }

    #[test]
    fn session_app_control_matches_supported_platform_and_actions() {
        let tools = Llm::tools(DeskKind::Session);
        let control = tools.as_array().unwrap().iter().find(|tool| tool["name"] == "app_control");
        assert_eq!(control.is_some(), cfg!(windows));
        if let Some(control) = control {
            assert_eq!(control["input_schema"]["properties"]["action"]["enum"],
                json!(["list_windows", "switch_window", "ui_tree", "press", "set_value", "focus", "select", "choose", "read"]));
            assert!(control["input_schema"]["properties"].get("path").is_none());
        }
    }
}
