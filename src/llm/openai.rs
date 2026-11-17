//! OpenAI-compatible Chat Completions (GPT, Gemini, Grok, OpenRouter, Mistral, Ollama).
//!
//! The agent's conversation (Claude format) is converted to this format right before sending,
//! and the reply is converted back.
//! - Claude's computer toolset is emulated with a single `computer` function (action + args).
//!   `computer` calls in the reply come back as toolset calls (`name` = action, `toolset_name`).
//! - Old screenshots have already been replaced with text by the agent (`agent::history`), so
//!   they pass through unchanged.

use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use serde_json::{json, Value};

use super::{short, Llm};

impl Llm {
    pub(super) async fn send_openai(&self, system: &str, tools: &Value, messages: &[Value]) -> Result<Value> {
        let mut body = json!({
            "model": self.model,
            "messages": to_messages(system, messages),
            "tools": to_tools(tools),
            "tool_choice": "auto",
        });
        if self.provider.takes_effort() {
            body["reasoning_effort"] = json!(effort(&self.effort));
        }
        let url = format!("{}/chat/completions", self.base_url);
        let resp = match self.post(&url, &body, &[]).await {
            Err(e) if body.get("reasoning_effort").is_some()
                && format!("{e:#}").contains("400 Bad Request")
                && format!("{e:#}").contains("reasoning_effort") => {
                let why = format!("{e:#}");
                if why.contains("Function tools") && why.contains("'none'") {
                    // Omitting effort restores the default reasoning, which still rejects tools.
                    body["reasoning_effort"] = json!("none");
                } else {
                    // Models without reasoning effort (e.g. gpt-4.1) reject the parameter itself.
                    body.as_object_mut().unwrap().remove("reasoning_effort");
                }
                self.post(&url, &body, &[]).await?
            }
            r => r?,
        };
        from_response(&resp)
    }
}

/// Claude effort (low..max) → OpenAI `reasoning_effort` (low/medium/high).
fn effort(e: &str) -> &'static str {
    match e {
        "low" => "low",
        "medium" => "medium",
        _ => "high",
    }
}

/// A single function standing in for Claude's computer toolset.
fn computer_function() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "computer",
            "description": "Operate the screen shown in the screenshots. Coordinates are integer pixels [x, y] in the most recent screenshot (its size is written next to it). \
Actions: screenshot; left_click / right_click / middle_click / double_click / triple_click (coordinate; text = modifier keys to hold, e.g. \"shift\"); mouse_move (coordinate); \
left_click_drag (start_coordinate -> coordinate); left_mouse_down; left_mouse_up; scroll (coordinate, scroll_direction, scroll_amount); type (text); \
key (text, xdotool style such as \"Return\", \"ctrl+s\", \"alt+Tab\"; repeat); hold_key (text, duration); wait (duration seconds); zoom (region [x0, y0, x1, y1] to see details); cursor_position.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": [
                        "screenshot", "left_click", "right_click", "middle_click", "double_click", "triple_click", "mouse_move",
                        "left_click_drag", "left_mouse_down", "left_mouse_up", "scroll", "type", "key", "hold_key", "wait", "zoom", "cursor_position"
                    ] },
                    "coordinate": { "type": "array", "items": { "type": "integer" }, "description": "[x, y]" },
                    "start_coordinate": { "type": "array", "items": { "type": "integer" }, "description": "[x, y] drag start" },
                    "text": { "type": "string" },
                    "scroll_direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                    "scroll_amount": { "type": "integer" },
                    "duration": { "type": "number" },
                    "repeat": { "type": "integer" },
                    "region": { "type": "array", "items": { "type": "integer" }, "description": "[x0, y0, x1, y1]" }
                },
                "required": ["action"]
            }
        }
    })
}

fn to_tools(tools: &Value) -> Value {
    let out = list(tools)
        .iter()
        .map(|t| {
            if t["type"].as_str().is_some_and(|s| s.starts_with("computer")) {
                computer_function()
            } else {
                json!({ "type": "function", "function": {
                    "name": t["name"], "description": t["description"], "parameters": t["input_schema"]
                } })
            }
        })
        .collect();
    Value::Array(out)
}

/// Screenshot block → OpenAI image parts, prefixed with the size so coordinates line up.
fn image_parts(b: &Value) -> Vec<Value> {
    let media = b["source"]["media_type"].as_str().unwrap_or("image/jpeg");
    let data = b["source"]["data"].as_str().unwrap_or_default();
    let mut out = Vec::with_capacity(2);
    if let Some((w, h)) = image_size(data) {
        out.push(json!({ "type": "text", "text": format!("[screenshot {w}x{h} px]") }));
    }
    out.push(json!({ "type": "image_url", "image_url": { "url": format!("data:{media};base64,{data}") } }));
    out
}

fn image_size(b64: &str) -> Option<(u32, u32)> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?.into_dimensions().ok()
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn list(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// Claude assistant turn → OpenAI assistant message.
///
/// Returns `None` when there is neither text nor a call (thinking only): several providers
/// reject empty assistant messages.
fn assistant_message(blocks: &[Value]) -> Option<Value> {
    let text: Vec<&str> = blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
    let calls: Vec<Value> = blocks
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .map(|b| {
            let (name, args) = if b.get("toolset_name").is_some() {
                let mut a = if b["input"].is_object() { b["input"].clone() } else { json!({}) };
                a["action"] = b["name"].clone();
                ("computer", a)
            } else {
                (b["name"].as_str().unwrap_or_default(), b["input"].clone())
            };
            let mut c = json!({ "id": b["id"], "type": "function", "function": { "name": name, "arguments": args.to_string() } });
            // send provider-specific values (like Gemini's thought signature) back exactly as received
            if let Some(x) = b.get("_extra") {
                c["extra_content"] = x.clone();
            }
            c
        })
        .collect();
    let text = text.join("\n");
    if text.trim().is_empty() && calls.is_empty() {
        return None;
    }
    // content is null when there are only calls (per the OpenAI spec; some providers reject "")
    let mut msg = json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) } });
    if !calls.is_empty() {
        msg["tool_calls"] = Value::Array(calls);
    }
    Some(msg)
}

fn to_messages(system: &str, messages: &[Value]) -> Vec<Value> {
    let mut out = vec![json!({ "role": "system", "content": system })];
    for m in messages {
        if m["role"] == "assistant" {
            out.extend(assistant_message(list(&m["content"])));
            continue;
        }
        if let Some(s) = m["content"].as_str() {
            out.push(json!({ "role": "user", "content": s }));
            continue;
        }
        // user turn: tool results first as tool messages (they must follow the calls directly), then
        // images and text in one user message
        let mut parts = Vec::new();
        for b in list(&m["content"]) {
            match b["type"].as_str() {
                Some("tool_result") => {
                    let mut t = text_of(&b["content"]);
                    if b["is_error"] == true && !t.starts_with("Error") {
                        t = format!("Error: {t}");
                    }
                    let id = b["tool_use_id"].as_str().or(b["id"].as_str());
                    out.push(json!({ "role": "tool", "tool_call_id": id, "content": if t.is_empty() { "OK".into() } else { t } }));
                    for c in list(&b["content"]).iter().filter(|c| c["type"] == "image") {
                        parts.extend(image_parts(c));
                    }
                }
                Some("text") => parts.push(json!({ "type": "text", "text": b["text"] })),
                Some("image") => parts.extend(image_parts(b)),
                _ => {}
            }
        }
        if !parts.is_empty() {
            out.push(json!({ "role": "user", "content": parts }));
        }
    }
    out
}

/// OpenAI-compatible reply → Claude-format reply.
///
/// An error or empty body is returned as an error even with a 200 status (OpenRouter and others
/// report upstream failures as 200 + `error`).
fn from_response(resp: &Value) -> Result<Value> {
    if let Some(e) = resp.get("error").filter(|e| !e.is_null()) {
        let why = e["message"].as_str().map(String::from).unwrap_or_else(|| e.to_string());
        bail!("AI 응답 오류: {}", short(&why, 400));
    }
    let choice = resp["choices"].get(0).ok_or_else(|| anyhow!("AI 응답에 내용이 없습니다: {}", short(&resp.to_string(), 300)))?;
    let msg = &choice["message"];
    let mut content = Vec::new();
    // some providers (DeepSeek, OpenRouter, …) return the reasoning separately; it only shows in
    // the log
    if let Some(r) = msg["reasoning_content"].as_str().or(msg["reasoning"].as_str()).filter(|s| !s.trim().is_empty()) {
        content.push(json!({ "type": "thinking", "thinking": r }));
    }
    if let Some(t) = msg["content"].as_str().filter(|s| !s.trim().is_empty()) {
        content.push(json!({ "type": "text", "text": t }));
    }
    let calls = list(&msg["tool_calls"]);
    for (i, c) in calls.iter().enumerate() {
        let name = c["function"]["name"].as_str().unwrap_or_default();
        let mut input: Value = match &c["function"]["arguments"] {
            Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
            v if v.is_object() => v.clone(),
            _ => json!({}),
        };
        let id = c["id"].as_str().filter(|s| !s.is_empty()).map(String::from).unwrap_or_else(|| format!("call_{i}"));
        let mut b = if name == "computer" {
            let action = input.as_object_mut().and_then(|o| o.remove("action")).and_then(|a| a.as_str().map(String::from)).unwrap_or_default();
            json!({ "type": "tool_use", "id": id, "name": action, "input": input, "toolset_name": "computer" })
        } else {
            json!({ "type": "tool_use", "id": id, "name": name, "input": input })
        };
        if let Some(x) = c.get("extra_content") {
            b["_extra"] = x.clone();
        }
        content.push(b);
    }
    // any call means tool_use regardless of finish_reason (Gemini says "stop" even with calls)
    let stop = match choice["finish_reason"].as_str() {
        _ if !calls.is_empty() => "tool_use",
        Some("length") => "max_tokens",
        Some("content_filter") => "refusal",
        _ => "end_turn",
    };
    Ok(json!({
        "content": content,
        "stop_reason": stop,
        "usage": {
            "input_tokens": resp["usage"]["prompt_tokens"],
            "output_tokens": resp["usage"]["completion_tokens"],
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Provider;

    async fn send_mock(reply: fn(&Value) -> (u16, Value)) -> (Result<Value>, Vec<Value>) {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let received = requests.clone();
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                assert_eq!(line.trim(), "POST /v1/chat/completions HTTP/1.1");
                let mut length = 0;
                loop {
                    line.clear();
                    stream.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = n.trim().parse().unwrap();
                    }
                }
                let mut bytes = vec![0; length];
                stream.read_exact(&mut bytes).await.unwrap();
                let body = serde_json::from_slice(&bytes).unwrap();
                let (status, response) = reply(&body);
                received.lock().unwrap().push(body);
                let response = response.to_string();
                let headers = format!("HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len());
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let llm = Llm::new(Provider::OpenAi, url, "test-key".into(), "gpt-6-luna".into(), "high".into());
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            llm.send("test", &json!([{ "type": "computer_toolset_20260801" }]), &[json!({ "role": "user", "content": "test" })]),
        )
        .await
        .expect("mock API should finish without transient retries");
        server.abort();
        let requests = requests.lock().unwrap().clone();
        (result, requests)
    }

    fn completed() -> Value {
        json!({ "choices": [{ "message": { "content": "done" }, "finish_reason": "stop" }] })
    }

    #[tokio::test]
    async fn accepted_reasoning_effort_is_preserved() {
        let (result, requests) = send_mock(|_| (200, completed())).await;
        assert!(result.is_ok());
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["reasoning_effort"], "high");
    }

    #[tokio::test]
    async fn function_tools_reasoning_error_retries_with_none() {
        let (result, requests) = send_mock(|body| {
            if body["reasoning_effort"] == "none" {
                (200, completed())
            } else {
                (400, json!({ "error": {
                    "message": "Function tools with reasoning_effort are not supported for gpt-6-luna in /v1/chat/completions. To use function tools, use /v1/responses or set reasoning_effort to 'none'.",
                    "type": "invalid_request_error", "param": "reasoning_effort"
                } }))
            }
        }).await;
        assert_eq!(result.unwrap()["content"][0]["text"], "done");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["reasoning_effort"], "high");
        assert_eq!(requests[1]["reasoning_effort"], "none");
        assert_eq!(requests[0]["tools"], requests[1]["tools"]);
        assert_eq!(requests[0]["messages"], requests[1]["messages"]);
    }

    #[tokio::test]
    async fn unsupported_effort_parameter_still_retries_without_it() {
        let (result, requests) = send_mock(|body| {
            if body.get("reasoning_effort").is_none() {
                (200, completed())
            } else {
                (400, json!({ "error": { "message": "Unsupported parameter: 'reasoning_effort'.", "param": "reasoning_effort" } }))
            }
        }).await;
        assert!(result.is_ok());
        assert_eq!(requests.len(), 2);
        assert!(requests[1].get("reasoning_effort").is_none());
    }

    #[tokio::test]
    async fn authentication_error_is_not_retried_as_effort_incompatibility() {
        let (result, requests) = send_mock(|_| (401, json!({ "error": { "message": "No access to reasoning_effort models" } }))).await;
        assert!(result.unwrap_err().to_string().contains("401"));
        assert_eq!(requests.len(), 1);
    }

    #[tokio::test]
    async fn unrelated_reasoning_error_is_not_retried() {
        let (result, requests) = send_mock(|_| (400, json!({ "error": { "message": "Invalid reasoning message in tool history", "param": "messages" } }))).await;
        assert!(result.unwrap_err().to_string().contains("Invalid reasoning message"));
        assert_eq!(requests.len(), 1);
    }

    /// A Claude-format conversation converts to OpenAI format, and the reply's computer call
    /// converts back to Claude format.
    #[test]
    fn openai_round_trip() {
        let img = json!({ "type": "image", "source": { "type": "base64", "media_type": "image/jpeg", "data": "AAAA" } });
        let messages = vec![
            json!({ "role": "user", "content": [{ "type": "text", "text": "작업" }, img.clone()] }),
            json!({ "role": "assistant", "content": [
                { "type": "thinking", "thinking": "..." },
                { "type": "tool_use", "id": "c1", "name": "left_click", "input": { "coordinate": [10, 20] }, "toolset_name": "computer", "_extra": { "google": { "thought_signature": "s" } } },
                { "type": "tool_use", "id": "c2", "name": "ask_user", "input": { "question": "?" } }
            ] }),
            json!({ "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "c1", "toolset_name": "computer", "content": [{ "type": "text", "text": "OK" }, img.clone()] },
                { "type": "tool_result", "tool_use_id": "c2", "is_error": true, "content": "실패" }
            ] }),
            // thinking-only turns aren't sent
            json!({ "role": "assistant", "content": [{ "type": "thinking", "thinking": "..." }] }),
            json!({ "role": "user", "content": [{ "type": "text", "text": "계속" }] }),
        ];
        let m = to_messages("sys", &messages);
        let roles: Vec<&str> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "tool", "user", "user"]);
        assert!(m[2]["content"].is_null(), "호출만 있으면 content 는 null");
        let call = &m[2]["tool_calls"][0];
        assert_eq!(call["function"]["name"], "computer");
        let args: Value = serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["action"], "left_click");
        assert_eq!(call["extra_content"]["google"]["thought_signature"], "s");
        assert_eq!(m[3]["tool_call_id"], "c1");
        assert_eq!(m[4]["content"], "Error: 실패");
        // screenshots from tool results go in a user message after the tool message
        assert!(list(&m[5]["content"]).iter().any(|p| p["type"] == "image_url"));

        // "stop" with calls (as Gemini does) is still tool_use
        let resp = json!({
            "choices": [{ "finish_reason": "stop", "message": { "content": null, "tool_calls": [
                { "id": "x", "type": "function", "function": { "name": "computer", "arguments": "{\"action\":\"type\",\"text\":\"hi\"}" } }
            ] } }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 2 }
        });
        let r = from_response(&resp).unwrap();
        assert_eq!(r["stop_reason"], "tool_use");
        assert_eq!(r["content"][0]["name"], "type");
        assert_eq!(r["content"][0]["input"]["text"], "hi");
        assert_eq!(r["content"][0]["toolset_name"], "computer");
        assert_eq!(r["usage"]["input_tokens"], 5);
    }

    /// An error or empty reply with a 200 status is an error, not an empty turn.
    #[test]
    fn error_bodies_are_errors() {
        let e = from_response(&json!({ "error": { "message": "upstream overloaded", "code": 502 } })).unwrap_err();
        assert!(format!("{e}").contains("upstream overloaded"));
        assert!(from_response(&json!({ "choices": [] })).is_err());
        assert!(from_response(&json!({ "error": null, "choices": [{ "message": { "content": "hi" } }] })).is_ok());
    }
}
