//! AI 제공자 목록과 제공자별 고정 정보 (주소·키 이름·기본 모델).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    Anthropic,
    OpenAi,
    Gemini,
    Xai,
    OpenRouter,
    Mistral,
    Ollama,
    /// 사용자가 주소를 넣는 OpenAI 호환 API (DeepSeek·Groq·Together·LM Studio·vLLM 등)
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

    /// 짧은 이름 (오류·안내 문구용)
    pub fn short_name(self) -> &'static str {
        match self {
            Provider::Anthropic => "Claude",
            Provider::OpenAi => "OpenAI",
            Provider::Gemini => "Gemini",
            Provider::Xai => "xAI",
            Provider::OpenRouter => "OpenRouter",
            Provider::Mistral => "Mistral",
            Provider::Ollama => "Ollama",
            // 설정 파일의 키 이름이기도 하므로 바꾸지 않는다
            Provider::Custom => "Custom",
        }
    }

    /// 기본 API 주소. Claude 는 Messages API 의 뿌리 주소, 나머지는 OpenAI 호환 API 주소.
    /// `Custom` 은 기본 주소가 없다 (사용자가 넣은 주소를 `normalize_base_url` 로 다듬어 쓴다).
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

    /// 키를 자동으로 채울 환경변수 (앞의 것부터)
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

    /// API 키를 만드는 곳 (`Custom` 은 없음: 빈 글)
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

    /// 키가 꼭 있어야 하는가
    pub fn needs_key(self) -> bool {
        !matches!(self, Provider::Ollama | Provider::Custom)
    }

    /// 키 칸을 보여 주는가 (`Custom` 은 키가 없어도 되지만 넣을 수는 있다)
    pub fn takes_key(self) -> bool {
        self != Provider::Ollama
    }

    /// 사용자가 API 주소를 넣어야 하는가
    pub fn needs_url(self) -> bool {
        self == Provider::Custom
    }

    /// 처음 보여 줄 모델 (목록은 '모델' 칸을 열면 API 에서 실제로 불러온다)
    pub fn default_models(self) -> &'static [&'static str] {
        match self {
            Provider::Anthropic => &["claude-opus-5-5", "claude-sonnet-5-5", "claude-fable-5-2", "claude-haiku-5-5"],
            Provider::OpenAi => &["gpt-5.5", "gpt-5.5-mini"],
            Provider::Gemini => &["gemini-3.1-pro-preview", "gemini-3-flash-preview"],
            Provider::Xai => &["grok-4"],
            Provider::OpenRouter => &["openai/gpt-5.5", "google/gemini-3.1-pro-preview", "anthropic/claude-sonnet-5-5"],
            Provider::Mistral => &["mistral-medium-latest"],
            Provider::Ollama => &["qwen2.5vl"],
            // 서버마다 모델이 달라 기본값이 없다 (목록은 그 서버의 /models 에서 불러온다)
            Provider::Custom => &[],
        }
    }

    /// `reasoning_effort` 를 받는 제공자 (받지 않는 모델이면 오류를 보고 빼고 다시 보낸다)
    pub(super) fn takes_effort(self) -> bool {
        matches!(self, Provider::OpenAi | Provider::Gemini)
    }

    /// 사용자가 넣은 주소를 API 의 뿌리 주소로 다듬는다: 앞뒤 공백·끝의 '/' 와,
    /// 실수로 붙여 넣은 `/chat/completions`·`/models` 를 뗀다 (예: "https://api.deepseek.com/v1/").
    pub fn normalize_base_url(url: &str) -> String {
        let mut u = url.trim().trim_end_matches('/');
        for tail in ["/chat/completions", "/models"] {
            u = u.strip_suffix(tail).unwrap_or(u);
        }
        u.trim_end_matches('/').to_string()
    }

    /// 오래된 도구 결과를 서버가 지워 주는가 (Claude 의 context_management). 아니면 에이전트가 직접 지운다.
    pub fn clears_old_tool_results(self) -> bool {
        self == Provider::Anthropic
    }

    /// '생각 깊이'(effort) 설정이 요청에 실리는 제공자 (Claude 는 output_config.effort, 그 밖은 reasoning_effort)
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
        // 받는지 모르는 옵션은 보내지 않는다. 오래된 도구 결과는 에이전트가 직접 지운다
        assert!(!c.supports_effort() && !c.clears_old_tool_results());
        for p in Provider::ALL.into_iter().filter(|p| *p != Provider::Custom) {
            assert!(!p.needs_url() && !p.default_base_url().is_empty() && !p.default_models().is_empty(), "{p:?}");
        }
    }
}
