//! Claude Messages API. The conversation is already in Claude format, so it is sent as is.
//!
//! - Computer use: `computer_toolset_20260801` (GA, no beta header).
//! - Adaptive thinking with `display: "updates"` returns progress notes between tool calls.
//! - Old tool results are also trimmed server-side with context editing
//!   (`clear_tool_uses_20250919`). The agent already drops old screenshots itself, so this
//!   mostly catches long text results.
//! - Thinking blocks are bound to the history before them, so the agent never edits earlier
//!   turns for Claude; `drop_block` keeps a stray mismatch from failing the request.
//! - Safety-classifier refusals fall back server-side (`fallbacks: "default"`).

use anyhow::Result;
use serde_json::{json, Value};

use super::Llm;

/// Number of recent tool calls whose results survive server-side clearing. The agent only skips
/// resending an unchanged screen within this window.
pub(crate) const KEEP_TOOL_USES: usize = 6;
/// Input-token count at which old tool results are cleared (by the server for Claude, by
/// `agent::history` for other providers).
pub(crate) const CLEAR_TOOL_USES_AT: u64 = 60_000;

impl Llm {
    pub(super) async fn send_anthropic(&self, system: &str, tools: &Value, messages: &[Value]) -> Result<Value> {
        // Haiku supports neither progress notes nor server-side fallback
        let haiku = self.model.contains("haiku");
        let mut betas = vec!["context-management-2025-06-27", "thinking-binding-controls-2026-08-01"];
        // if the history ever stops matching a thinking block, drop that reasoning instead of
        // failing the request (newer accounts get a 400 by default)
        let mut thinking = json!({ "type": "adaptive", "block_binding": { "prefix_mismatch_behavior": "drop_block" } });
        let mut body = json!({
            "model": self.model,
            "max_tokens": 16000,
            "system": system,
            "output_config": { "effort": self.effort },
            "tools": tools,
            "context_management": {
                "edits": [{
                    "type": "clear_tool_uses_20250919",
                    "trigger": { "type": "input_tokens", "value": CLEAR_TOOL_USES_AT },
                    "keep": { "type": "tool_uses", "value": KEEP_TOOL_USES },
                    "clear_at_least": { "type": "input_tokens", "value": 10000 }
                }]
            },
            "messages": messages,
            // automatic prompt caching: cache the growing conversation prefix to cut latency and cost
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
        self.post(&format!("{}/v1/messages", self.base_url), &body, &headers).await
    }
}
