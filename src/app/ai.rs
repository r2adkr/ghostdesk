//! 사용자가 추가한 AI 연결 목록.
//!
//! 연결 하나 = 회사(`Provider`) + 키 + 고른 모델 (+ OpenAI 호환이면 이름과 주소).
//! 정해진 회사는 하나씩만 둘 수 있고, OpenAI 호환 API 는 주소마다 여러 개 둘 수 있다.
//! 연결의 `id` 는 만들 때 정해져 바뀌지 않는다: 모델 목록 불러오기 상태와 입력 칸이 이것으로 연결을 찾는다
//! (목록에서 지우거나 순서가 바뀌어도 다른 연결의 상태가 섞이지 않게).

use crate::llm::{Llm, Provider};

use super::i18n::{tr, trf};

#[derive(Clone, Debug, PartialEq)]
pub(super) struct AiConn {
    pub id: u64,
    pub provider: Provider,
    /// OpenAI 호환 연결의 이름 (예: "DeepSeek"). 다른 회사는 비워 두고 회사 이름을 쓴다
    pub name: String,
    /// OpenAI 호환 연결의 주소 (사용자가 넣은 그대로, 쓸 때 다듬는다)
    pub url: String,
    pub key: String,
    pub model: String,
}

impl AiConn {
    /// 새 연결: 키는 환경변수에 있으면 채우고, 모델은 그 회사의 첫 기본 모델
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

    /// 목록·단추에 보일 이름
    pub fn title(&self) -> String {
        match self.name.trim() {
            "" if self.provider.needs_url() => tr("OpenAI 호환").to_string(),
            "" => self.provider.short_name().to_string(),
            n => n.to_string(),
        }
    }

    /// API 뿌리 주소
    pub fn base_url(&self) -> String {
        if self.provider.needs_url() {
            Provider::normalize_base_url(&self.url)
        } else {
            self.provider.default_base_url()
        }
    }

    /// 일을 맡기기에 빠진 설정 (주소 또는 필수 키). 다 있으면 None
    pub fn missing(&self) -> Option<String> {
        if self.provider.needs_url() && self.base_url().is_empty() {
            Some(trf("설정에서 {} 의 API 주소를 넣어 주세요.", &[&self.title()]))
        } else if self.provider.needs_key() && self.key.trim().is_empty() {
            Some(trf("설정에서 {} API 키를 넣어 주세요.", &[&self.title()]))
        } else {
            None
        }
    }

    /// 모델 목록을 부를 수 있는가 (주소·필수 키가 있음)
    pub fn configured(&self) -> bool {
        self.missing().is_none()
    }

    /// 이 연결로 일하는 AI 클라이언트
    pub fn llm(&self, effort: &str) -> Llm {
        Llm::new(self.provider, self.base_url(), self.key.trim().to_string(), self.model.trim().to_string(), effort.to_string())
    }
}

/// '+ AI 추가' 메뉴에 보일 회사: 아직 없는 회사들, 그리고 늘 OpenAI 호환 (여러 개 둘 수 있으므로)
pub(super) fn addable(conns: &[AiConn]) -> Vec<Provider> {
    Provider::ALL.into_iter().filter(|p| p.needs_url() || !conns.iter().any(|c| c.provider == *p)).collect()
}

/// 처음 켤 때(저장한 설정이 없을 때)의 목록: 환경변수에 키가 있는 회사들, 없으면 Claude 하나
pub(super) fn initial(next_id: &mut u64) -> Vec<AiConn> {
    let mut list: Vec<AiConn> = Provider::ALL
        .into_iter()
        .filter(|p| p.needs_key() && !p.env_key().is_empty())
        .map(|p| {
            *next_id += 1;
            AiConn::new(*next_id, p)
        })
        .collect();
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
