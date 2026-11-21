//! Short labels and colors for the UI: agent status, error messages, thinking depth, profile
//! names.

use super::*;

/// Whether the agent stopped on an error (after an API error it waits with "Error — …").
pub(super) fn is_error(st: &AgentStatus) -> bool {
    match st {
        AgentStatus::Error(_) => true,
        AgentStatus::Idle(s) => s.starts_with("오류"),
        _ => false,
    }
}

/// The color to flag an agent that needs a person: orange while taken over, yellow for a
/// question, red for an error.
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

/// Status color: green while working, yellow while waiting for the user, red on error, gray
/// when idle.
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

/// Status text, kept short for the UI.
pub(super) fn status_text(st: &AgentStatus) -> String {
    match st {
        AgentStatus::Thinking => tr("생각 중").into(),
        AgentStatus::Acting(s) => trf("실행 중: {}", &[&s]),
        AgentStatus::Idle(s) if s.starts_with("오류") => tr("오류 — 지시하면 다시 시도").into(),
        // reasons sent by the agent are translated when they're in the table; ones with names or
        // numbers in them are shown as is
        AgentStatus::Preparing(s) => trf("준비 중: {}", &[&tr(s)]),
        AgentStatus::WaitingUser(_) => tr("사용자 응답 대기").into(),
        AgentStatus::Idle(s) => trf("대기: {}", &[&tr(s)]),
        AgentStatus::Error(s) => trf("오류: {}", &[&s]),
    }
}

/// Human control and pause describe what is happening now, even if the last AI action is pending.
pub(super) fn display_status(st: &AgentStatus, paused: bool, takeover: bool) -> (String, Color32) {
    if takeover {
        (tr("직접 조작 중").into(), theme::takeover())
    } else if paused {
        (tr("일시정지됨").into(), theme::muted())
    } else {
        (status_text(st), status_color(st))
    }
}

/// Reason strings the agent (`agent.rs`, `agent/workspace.rs`) puts into its status. They must
/// be in the translation table to show up in the UI language.
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
    "응답 길이 제한 — 이어서 진행하려면 지시를 입력하세요",
];

/// Turns an error into a short, readable message. The full text is shown on hover.
pub(super) fn humanize_error(s: &str) -> String {
    let l = s.to_lowercase();
    // providers word these differently, so match on status codes and common words
    // (Gemini answers a bad key with 400 "API key not valid")
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

/// Short name for a thinking depth, for tight spots like the model button.
pub(super) fn effort_short(e: &str) -> String {
    match e {
        "low" => tr("낮음"),
        "medium" => tr("보통"),
        "high" => tr("높음"),
        "xhigh" => tr("매우 높음"),
        "max" => tr("최대"),
        _ => e,
    }
    .to_string()
}

/// Thinking depth label, with the raw API value in parentheses.
pub(super) fn effort_label(e: &str) -> String {
    let short = effort_short(e);
    if short == e {
        short
    } else {
        format!("{short} ({e})")
    }
}

/// Strips the account part "(…@…)" from a browser profile's display name.
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
    fn visible_status_prioritizes_human_control_and_pause() {
        let st = AgentStatus::Thinking;
        assert_eq!(display_status(&st, false, false), (status_text(&st), status_color(&st)));
        assert_eq!(display_status(&st, true, false), (tr("일시정지됨").into(), theme::muted()));
        assert_eq!(display_status(&st, true, true), (tr("직접 조작 중").into(), theme::takeover()));
        let error = AgentStatus::Error("연결 실패".into());
        assert_eq!(display_status(&error, false, false), (status_text(&error), theme::danger()));
    }

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
