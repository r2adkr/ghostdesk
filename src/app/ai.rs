//! The user's list of AI connections.
//!
//! A connection is a provider (`Provider`) plus a key and the chosen model, and for
//! OpenAI-compatible APIs also a name and URL. Built-in providers can appear once each;
//! OpenAI-compatible APIs can be added once per URL. A connection's `id` is fixed at creation:
//! model-list fetches and input fields key off it, so deleting or reordering connections never
//! mixes up their state.

use crate::llm::{Llm, Provider};

use super::i18n::{tr, trf};

#[derive(Clone, Debug, PartialEq)]
pub(super) struct AiConn {
    pub id: u64,
    pub provider: Provider,
    /// Display name of an OpenAI-compatible connection (e.g. "DeepSeek"). Empty for built-in
    /// providers, which use the provider name.
    pub name: String,
    /// URL of an OpenAI-compatible connection, as typed by the user (normalized on use).
    pub url: String,
    pub key: String,
    pub model: String,
}

impl AiConn {
    /// A new connection. The key is filled from the environment if set; the model is the
    /// provider's first default.
    pub fn new(id: u64, provider: Provider) -> Self {
        AiConn {
            id,
            provider,
            name: String::new(),
            url: String::new(),
            key: provider.env_key(),
            model: provider.default_models().first().copied().unwrap_or_default().to_string(),
        }
    }

    /// Name shown in lists and buttons.
    pub fn title(&self) -> String {
        match self.name.trim() {
            "" if self.provider.needs_url() => tr("OpenAI 호환").to_string(),
            "" => self.provider.short_name().to_string(),
            n => n.to_string(),
        }
    }

    /// API base URL.
    pub fn base_url(&self) -> String {
        if self.provider.is_cli() {
            self.url.trim().to_string()
        } else if self.provider.needs_url() {
            Provider::normalize_base_url(&self.url)
        } else {
            self.provider.default_base_url()
        }
    }

    /// What is still missing before this connection can take a task (URL or required key).
    /// `None` when it's ready.
    pub fn missing(&self) -> Option<String> {
        if self.provider.needs_url() && self.base_url().is_empty() {
            Some(trf("설정에서 {} 의 API 주소를 넣어 주세요.", &[&self.title()]))
        } else if self.provider.needs_key() && self.key.trim().is_empty() {
            Some(trf("설정에서 {} API 키를 넣어 주세요.", &[&self.title()]))
        } else {
            None
        }
    }

    /// Whether the model list can be fetched (URL and any required key are set).
    pub fn configured(&self) -> bool {
        self.missing().is_none()
    }

    /// An AI client for this connection.
    pub fn llm(&self, effort: &str) -> Llm {
        Llm::new(self.provider, self.base_url(), self.key.trim().to_string(), self.model.trim().to_string(), effort.to_string())
    }
}

/// Providers offered in the "+ Add AI" menu: those not added yet, plus OpenAI-compatible,
/// which can be added any number of times.
pub(super) fn addable(conns: &[AiConn]) -> Vec<Provider> {
    Provider::ALL.into_iter().filter(|p| p.needs_url() || !conns.iter().any(|c| c.provider == *p)).collect()
}

/// The initial list when there are no saved settings: providers with a key in the environment,
/// or just Claude.
pub(super) fn initial(next_id: &mut u64) -> Vec<AiConn> {
    let mut list: Vec<AiConn> = Provider::ALL
        .into_iter()
        .filter(|p| p.needs_key() && !p.env_key().is_empty())
        .map(|p| {
            *next_id += 1;
            AiConn::new(*next_id, p)
        })
        .collect();
    for provider in [Provider::ClaudeCli, Provider::CodexCli] {
        if crate::cli::executable(provider, "").is_some() {
            *next_id += 1; list.push(AiConn::new(*next_id, provider));
        }
    }
    if list.is_empty() {
        *next_id += 1;
        list.push(AiConn::new(*next_id, Provider::Anthropic));
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(id: u64, p: Provider) -> AiConn {
        AiConn { key: String::new(), ..AiConn::new(id, p) }
    }

    #[test]
    fn missing_setup_follows_the_provider() {
        let mut c = conn(1, Provider::Anthropic);
        assert!(c.missing().is_some(), "Claude 는 키가 있어야 함");
        c.key = "sk-ant-x".into();
        assert!(c.missing().is_none() && c.configured());

        let mut custom = conn(2, Provider::Custom);
        assert!(custom.missing().is_some(), "OpenAI 호환은 주소가 있어야 함");
        custom.url = "https://api.deepseek.com/v1/chat/completions".into();
        assert!(custom.missing().is_none(), "키는 없어도 됨");
        assert_eq!(custom.base_url(), "https://api.deepseek.com/v1");
        assert_eq!(custom.title(), tr("OpenAI 호환"));
        custom.name = "DeepSeek".into();
        assert_eq!(custom.title(), "DeepSeek");

        assert!(conn(3, Provider::Ollama).missing().is_none());
    }

    #[test]
    fn fixed_providers_once_custom_many() {
        let list = vec![conn(1, Provider::Anthropic), conn(2, Provider::Custom)];
        let add = addable(&list);
        assert!(!add.contains(&Provider::Anthropic));
        assert!(add.contains(&Provider::Custom) && add.contains(&Provider::OpenAi));
    }

    #[test]
    fn llm_uses_the_connection_settings() {
        let mut c = conn(1, Provider::Custom);
        c.url = " http://localhost:1234/v1/ ".into();
        c.key = " k ".into();
        c.model = "m".into();
        let l = c.llm("high");
        assert_eq!((l.base_url.as_str(), l.api_key.as_str(), l.model.as_str(), l.effort.as_str()), ("http://localhost:1234/v1", "k", "m", "high"));
    }
}
