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
}

impl Provider {
    pub const ALL: [Provider; 7] =
        [Provider::Anthropic, Provider::OpenAi, Provider::Gemini, Provider::Xai, Provider::OpenRouter, Provider::Mistral, Provider::Ollama];

    pub fn label(self) -> &'static str {
        match self {
            Provider::Anthropic => "Claude (Anthropic)",
            Provider::OpenAi => "GPT (OpenAI)",
            Provider::Gemini => "Gemini (Google)",
            Provider::Xai => "Grok (xAI)",
            Provider::OpenRouter => "OpenRouter (여러 AI 를 키 하나로)",
            Provider::Mistral => "Mistral",
            Provider::Ollama => "Ollama (내 PC 에서 실행)",
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
        }
    }

    /// API 주소. Claude 는 Messages API 의 뿌리 주소, 나머지는 OpenAI 호환 API 주소.
    pub(super) fn base_url(self) -> String {
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
            Provider::Ollama => &[],
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
        }
    }

    /// API 키를 만드는 곳
    pub fn key_page(self) -> &'static str {
        match self {
            Provider::Anthropic => "https://console.anthropic.com/settings/keys",
            Provider::OpenAi => "https://platform.openai.com/api-keys",
            Provider::Gemini => "https://aistudio.google.com/apikey",
            Provider::Xai => "https://console.x.ai",
            Provider::OpenRouter => "https://openrouter.ai/keys",
            Provider::Mistral => "https://console.mistral.ai/api-keys",
            Provider::Ollama => "https://ollama.com/download",
        }
    }

    pub fn needs_key(self) -> bool {
        self != Provider::Ollama
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
        }
    }

    /// `reasoning_effort` 를 받는 제공자 (받지 않는 모델이면 오류를 보고 빼고 다시 보낸다)
    pub(super) fn takes_effort(self) -> bool {
        matches!(self, Provider::OpenAi | Provider::Gemini)
    }
}
