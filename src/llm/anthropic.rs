//! Claude Messages API. 대화가 이미 Claude 형식이라 그대로 보낸다.
//!
//! - computer use: `computer_toolset_20260801` (GA, beta 헤더 없음)
//! - adaptive thinking + `display: "updates"` 로 도구 호출 사이의 진행 메모를 받는다
//! - 오래된 도구 결과는 서버 측 context editing(`clear_tool_uses_20250919`)으로도 정리한다
//!   (스크린샷은 에이전트가 먼저 덜어 내므로 주로 긴 글 결과가 대상)
//! - 안전 분류기 거절 시 서버 측 fallback(`fallbacks: "default"`)

use anyhow::Result;
use serde_json::{json, Value};

use super::Llm;

/// 서버 측 정리가 결과를 남겨 두는 최근 도구 호출 수. 에이전트는 이 범위 안에서만 같은 화면을 다시 보내지 않는다.
pub(crate) const KEEP_TOOL_USES: usize = 6;

impl Llm {
    pub(super) async fn send_anthropic(&self, system: &str, tools: &Value, messages: &[Value]) -> Result<Value> {
        // Haiku 는 진행 메모·서버 fallback 을 지원하지 않는다
        let haiku = self.model.contains("haiku");
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
                    "keep": { "type": "tool_uses", "value": KEEP_TOOL_USES },
                    "clear_at_least": { "type": "input_tokens", "value": 10000 }
                }]
            },
            "messages": messages,
            // 자동 프롬프트 캐싱: 매 턴 늘어나는 대화의 앞부분을 캐시해 지연·비용을 줄인다
            "cache_control": { "type": "ephemeral" },
        });
        if !haiku {
            betas.push("thinking-display-updates-2026-08-18");
            betas.push("server-side-fallback-2026-07-01");
            thinking["display"] = json!("updates");
            body["fallbacks"] = json!("default");
        }
        body["thinking"] = thinking;
        let beta = betas.join(",");
        let headers = [("anthropic-version", "2023-06-01"), ("anthropic-beta", beta.as_str())];
        self.post(&format!("{}/v1/messages", self.provider.base_url()), &body, &headers).await
    }
}
