//! 화면에 보일 짧은 글과 색: 에이전트 상태·오류 문구·생각 깊이·프로필 이름.

use super::*;

/// 오류로 멈춘 상태인가 (AI API 오류 뒤에는 '오류 — …' 로 대기 상태가 된다)
pub(super) fn is_error(st: &AgentStatus) -> bool {
    match st {
        AgentStatus::Error(_) => true,
        AgentStatus::Idle(s) => s.starts_with("오류"),
        _ => false,
    }
}

/// 사람이 볼 일이 있으면 그 표시 색: 직접 조작 중 = 주황, 질문 = 노랑, 오류 = 빨강
pub(super) fn attention(st: &AgentStatus, takeover: bool) -> Option<Color32> {
    if takeover {
        Some(theme::takeover())
    } else if matches!(st, AgentStatus::WaitingUser(_)) {
        Some(theme::warn())
    } else if is_error(st) {
        Some(theme::danger())
    } else {
        None
    }
}

/// 상태 표시 색 (테마 색): 일하는 중 = 초록, 사람 응답 대기 = 노랑, 오류 = 빨강, 쉬는 중 = 회색
pub(super) fn status_color(st: &AgentStatus) -> Color32 {
    match st {
        _ if is_error(st) => theme::danger(),
        AgentStatus::Preparing(_) => theme::muted(),
        AgentStatus::Thinking | AgentStatus::Acting(_) => theme::success(),
        AgentStatus::WaitingUser(_) => theme::warn(),
        AgentStatus::Idle(_) => theme::muted(),
        AgentStatus::Error(_) => theme::danger(),
    }
}

/// 상태 표시 글 (화면용으로 짧게)
pub(super) fn status_text(st: &AgentStatus) -> String {
    match st {
        AgentStatus::Thinking => tr("생각 중").into(),
        AgentStatus::Acting(s) => trf("실행 중: {}", &[&s]),
        AgentStatus::Idle(s) if s.starts_with("오류") => tr("오류 — 지시하면 다시 시도").into(),
        // 에이전트가 보낸 이유 글은 표에 있으면 옮기고, 이름·숫자가 든 글은 그대로 보인다
        AgentStatus::Preparing(s) => trf("준비 중: {}", &[&tr(s)]),
        AgentStatus::WaitingUser(_) => tr("사용자 응답 대기").into(),
        AgentStatus::Idle(s) => trf("대기: {}", &[&tr(s)]),
        AgentStatus::Error(s) => trf("오류: {}", &[&s]),
    }
}

/// 에이전트(agent.rs · agent/workspace.rs)가 상태에 넣는 이유 글. 표에 있어야 화면 언어로 보인다.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) const AGENT_REASONS: &[&str] = &[
    "시작",
    "지시를 입력하세요",
    "중지됨 — 지시를 입력하면 이어서 진행",
    "오류 — 지시를 입력하면 다시 시도",
    "모델 거절 — 다른 지시를 입력하세요",
    "작업 완료 — 추가 지시를 입력할 수 있습니다",
    "일시정지됨",
    "가상 디스플레이 준비 중",
    "원격 데스크톱 확인 중",
    "원격 데스크톱 접속 중",
    "프로필 복제 중",
    "브라우저 실행 중",
];

/// 오류 기록을 알아보기 쉬운 짧은 말로 (전체 글은 마우스를 올리면 보인다)
pub(super) fn humanize_error(s: &str) -> String {
    let l = s.to_lowercase();
    // 제공자마다 문구가 달라 상태 코드와 흔한 낱말로 본다 (Gemini 는 잘못된 키에 400 "API key not valid")
    if l.contains("401") || l.contains("invalid x-api-key") || l.contains("authentication") || l.contains("api key") || l.contains("api_key") {
        tr("API 키를 확인하세요").into()
    } else if l.contains("429") || l.contains("rate limit") || l.contains("rate_limit") {
        tr("요청 한도 초과 — 잠시 후 다시").into()
    } else if l.contains("529") || l.contains("overloaded") {
        tr("AI 서버가 혼잡함 — 잠시 후 다시").into()
    } else if l.contains("error sending request") || l.contains("연결 실패") || l.contains("dns") || l.contains("timed out") {
        tr("AI 서버에 연결할 수 없음").into()
    } else {
        crate::llm::short(s, 60)
    }
}

/// 생각 깊이 표시 (API 값은 그대로 괄호 안에)
pub(super) fn effort_label(e: &str) -> String {
    let ko = match e {
        "low" => tr("낮음"),
        "medium" => tr("보통"),
        "high" => tr("높음"),
        "xhigh" => tr("매우 높음"),
        "max" => tr("최대"),
        _ => return e.to_string(),
    };
    format!("{ko} ({e})")
}

/// 브라우저 프로필 표시 이름에서 계정 주소 부분 '(…@…)' 을 뗀다
pub(super) fn profile_short(name: &str) -> String {
    match name.rfind(" (") {
        Some(i) if name.ends_with(')') && name[i..].contains('@') => name[..i].to_string(),
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attention_and_names() {
        assert_eq!(attention(&AgentStatus::Thinking, false), None);
        assert_eq!(attention(&AgentStatus::Idle("오류 — 지시를 입력하면 다시 시도".into()), false), Some(theme::danger()));
        assert_eq!(attention(&AgentStatus::WaitingUser("?".into()), false), Some(theme::warn()));
        assert_eq!(attention(&AgentStatus::Idle("완료".into()), true), Some(theme::takeover()));
        assert_eq!(profile_short("재완 (me@x.com)"), "재완");
        assert_eq!(profile_short("업무 (회사) (me@x.com)"), "업무 (회사)");
        assert_eq!(profile_short("업무 (회사)"), "업무 (회사)");
        assert_eq!(humanize_error("OpenAI API 연결 실패: error sending request for url"), "AI 서버에 연결할 수 없음");
        assert_eq!(humanize_error("Gemini API 오류 400: API key not valid"), "API 키를 확인하세요");
        assert_eq!(humanize_error("Claude API 오류 529: overloaded"), "AI 서버가 혼잡함 — 잠시 후 다시");
        assert_eq!(humanize_error("HTTP 401: invalid x-api-key"), "API 키를 확인하세요");
    }
}
