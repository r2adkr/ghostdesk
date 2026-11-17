//! Supported AI providers and their fixed details (base URL, key variables, default models).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    Anthropic,
    OpenAi,
    Gemini,
    Xai,
    OpenRouter,
    Mistral,
    Ollama,
    /// Any OpenAI-compatible API at a user-supplied URL (DeepSeek, Groq, Together, LM Studio,
    /// vLLM, …).
    Custom,
}

impl Provider {
    pub const ALL: [Provider; 8] = [
        Provider::Anthropic,
        Provider::OpenAi,
        Provider::Gemini,
        Provider::Xai,
        Provider::OpenRouter,
        Provider::Mistral,
        Provider::Ollama,
        Provider::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Provider::Anthropic => "Claude (Anthropic)",
            Provider::OpenAi => "GPT (OpenAI)",
            Provider::Gemini => "Gemini (Google)",
            Provider::Xai => "Grok (xAI)",
            Provider::OpenRouter => "OpenRouter (여러 AI 를 키 하나로)",
            Provider::Mistral => "Mistral",
            Provider::Ollama => "Ollama (내 PC 에서 실행)",
            Provider::Custom => "OpenAI 호환 API (주소 직접 입력)",
        }
    }

    /// Short name, for errors and hints.
    pub fn short_name(self) -> &'static str {
        match self {
            Provider::Anthropic => "Claude",
            Provider::OpenAi => "OpenAI",
            Provider::Gemini => "Gemini",
            Provider::Xai => "xAI",
            Provider::OpenRouter => "OpenRouter",
            Provider::Mistral => "Mistral",
            Provider::Ollama => "Ollama",
            // also the key in the settings file, don't rename
            Provider::Custom => "Custom",
        }
    }

    /// Default API base URL: the Messages API root for Claude, an OpenAI-compatible root for the
    /// rest.
    ///
    /// `Custom` has none; the user's URL is cleaned up with `normalize_base_url` instead.
    pub fn default_base_url(self) -> String {
        match self {
            Provider::Anthropic => std::env::var("ANTHROPIC_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim_end_matches('/').to_string())
                .unwrap_or_else(|| "https://api.anthropic.com".into()),
            Provider::OpenAi => "https://api.openai.com/v1".into(),
            Provider::Gemini => "https://generativelanguage.googleapis.com/v1beta/openai".into(),
            Provider::Xai => "https://api.x.ai/v1".into(),
            Provider::OpenRouter => "https://openrouter.ai/api/v1".into(),
            Provider::Mistral => "https://api.mistral.ai/v1".into(),
            Provider::Ollama => "http://localhost:11434/v1".into(),
            Provider::Custom => String::new(),
        }
    }

    /// Environment variables to fill the key from, in order of preference.
    pub fn env_keys(self) -> &'static [&'static str] {
        match self {
            Provider::Anthropic => &["ANTHROPIC_API_KEY"],
            Provider::OpenAi => &["OPENAI_API_KEY"],
            Provider::Gemini => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            Provider::Xai => &["XAI_API_KEY"],
            Provider::OpenRouter => &["OPENROUTER_API_KEY"],
            Provider::Mistral => &["MISTRAL_API_KEY"],
            Provider::Ollama | Provider::Custom => &[],
        }
    }

    pub fn env_key(self) -> String {
        self.env_keys().iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty())).unwrap_or_default()
    }

    pub fn key_hint(self) -> &'static str {
        match self {
            Provider::Anthropic => "sk-ant-…",
            Provider::OpenAi => "sk-…",
            Provider::Gemini => "AIza…",
            Provider::Xai => "xai-…",
            Provider::OpenRouter => "sk-or-…",
            Provider::Mistral => "API 키",
            Provider::Ollama => "필요 없음",
            Provider::Custom => "API 키 (필요 없으면 비워 두기)",
        }
    }

    /// Where to create an API key (empty for `Custom`).
    pub fn key_page(self) -> &'static str {
        match self {
            Provider::Anthropic => "https://console.anthropic.com/settings/keys",
            Provider::OpenAi => "https://platform.openai.com/api-keys",
            Provider::Gemini => "https://aistudio.google.com/apikey",
            Provider::Xai => "https://console.x.ai",
            Provider::OpenRouter => "https://openrouter.ai/keys",
            Provider::Mistral => "https://console.mistral.ai/api-keys",
            Provider::Ollama => "https://ollama.com/download",
            Provider::Custom => "",
        }
    }

    /// Whether a key is required.
    pub fn needs_key(self) -> bool {
        !matches!(self, Provider::Ollama | Provider::Custom)
    }

    /// Whether to show a key field (`Custom` doesn't need one but accepts one).
    pub fn takes_key(self) -> bool {
        self != Provider::Ollama
    }

    /// Whether the user must enter an API URL.
    pub fn needs_url(self) -> bool {
        self == Provider::Custom
    }

    /// Models to show before the real list is fetched from the API (when the model field opens).
    pub fn default_models(self) -> &'static [&'static str] {
        match self {
            Provider::Anthropic => &["claude-opus-5-5", "claude-sonnet-5-5", "claude-fable-5-2", "claude-haiku-5-5"],
            Provider::OpenAi => &["gpt-5.5", "gpt-5.5-mini"],
            Provider::Gemini => &["gemini-3.1-pro-preview", "gemini-3-flash-preview"],
            Provider::Xai => &["grok-4"],
            Provider::OpenRouter => &["openai/gpt-5.5", "google/gemini-3.1-pro-preview", "anthropic/claude-sonnet-5-5"],
            Provider::Mistral => &["mistral-medium-latest"],
            Provider::Ollama => &["qwen2.5vl"],
            // models differ per server, so no defaults; the list comes from that server's /models
            Provider::Custom => &[],
        }
    }

    /// Providers that accept `reasoning_effort`. Models that reject it get the request resent
    /// without it.
    pub(super) fn takes_effort(self) -> bool {
        matches!(self, Provider::OpenAi | Provider::Gemini)
    }

    /// Cleans a user-supplied URL down to the API root: trims whitespace and a trailing '/', and
    /// strips an accidentally pasted `/chat/completions` or `/models`
    /// (e.g. "https://api.deepseek.com/v1/").
    pub fn normalize_base_url(url: &str) -> String {
        let mut u = url.trim().trim_end_matches('/');
        for tail in ["/chat/completions", "/models"] {
            u = u.strip_suffix(tail).unwrap_or(u);
        }
        u.trim_end_matches('/').to_string()
    }

    /// Whether the server clears old tool results itself (Claude's context management). Otherwise
    /// the agent does it.
    pub fn clears_old_tool_results(self) -> bool {
        self == Provider::Anthropic
    }

    /// Providers whose requests carry the thinking-depth (effort) setting: `output_config.effort`
    /// for Claude, `reasoning_effort` for the others.
    pub fn supports_effort(self) -> bool {
        self == Provider::Anthropic || self.takes_effort()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_url_is_normalized_to_the_api_root() {
        for raw in [
            "https://api.deepseek.com/v1",
            " https://api.deepseek.com/v1/ ",
            "https://api.deepseek.com/v1/chat/completions",
            "https://api.deepseek.com/v1/models/",
        ] {
            assert_eq!(Provider::normalize_base_url(raw), "https://api.deepseek.com/v1", "{raw:?}");
        }
        assert_eq!(Provider::normalize_base_url("  "), "");
    }

    #[test]
    fn custom_provider_needs_a_url_but_not_a_key() {
        let c = Provider::Custom;
        assert!(c.needs_url() && c.takes_key() && !c.needs_key());
        assert!(c.default_base_url().is_empty() && c.default_models().is_empty());
        // don't send options it might not accept; the agent clears old tool results itself
        assert!(!c.supports_effort() && !c.clears_old_tool_results());
        for p in Provider::ALL.into_iter().filter(|p| *p != Provider::Custom) {
            assert!(!p.needs_url() && !p.default_base_url().is_empty() && !p.default_models().is_empty(), "{p:?}");
        }
    }
}
