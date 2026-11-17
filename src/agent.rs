//! 에이전트 하나 = 격리 화면(Desk) 하나 + AI 대화 루프 하나.
//!
//! - prompt: 시스템 프롬프트
//! - workspace: 화면 준비 (브라우저·앱·원격·세션)
//! - tools: 도구 호출 실행
//! - history: 대화 기록을 작게 유지 (스크린샷 정리·중복 생략·새 구간 요약)

mod history;
mod prompt;
mod tools;
mod workspace;

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use eframe::egui;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::{
    desk::Desk,
    llm::{short, Llm},
    profiles::InstalledBrowser,
};
use history::{
    clear_old_tool_results, json_len, prune_images, remember, screenshot_context, segment_summary, Seen, CLEAR_TOOL_RESULTS_AT, MAX_CONTEXT_TOKENS,
    MAX_REQUEST_BYTES,
};

pub(crate) use tools::{coord, describe, exec_computer, exec_desk_tool};
#[cfg(not(target_os = "macos"))]
pub(crate) use workspace::local_vnc_refusal;
pub(crate) use workspace::prepare_workspace_with;

/// 화면 기록으로 남길 최대 줄 수. 넘으면 오래된 것부터 LOG_DRAIN 줄씩 버린다 (오래 돌아도 메모리가 늘지 않게)
const LOG_CAP: usize = 2000;
const LOG_DRAIN: usize = 500;

#[derive(Clone, Debug, PartialEq)]
pub enum AgentStatus {
    Preparing(String),
    Thinking,
    Acting(String),
    WaitingUser(String),
    Idle(String),
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LogKind {
    User,
    Assistant,
    Thinking,
    Action,
    Info,
    Error,
}

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub kind: LogKind,
    pub text: String,
    pub secs: f32,
}

pub enum AgentCmd {
    /// 추가 지시 (실행 중이면 다음 턴에 합류, 대기 중이면 대화 재개)
    Instruction(String),
    /// ask_user 에 대한 답
    Answer(String),
    /// 대기열(shared.queue)에 새 작업이 들어왔음을 알림
    Wake,
}

#[derive(Clone)]
pub enum ProfileSource {
    /// 원본 브라우저 프로필 복제 (로그인 상태 유지)
    Clone { browser: InstalledBrowser, profile_dir: String, refresh: bool },
    /// 빈 프로필
    Empty { browser: InstalledBrowser },
}

/// 원격 데스크톱(RFB/VNC) 대상
#[derive(Clone)]
pub struct RemoteTarget {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// 대상이 macOS 화면 공유 서버일 때, 그 Mac 이 에이전트 전용(VM/별도 Mac)임을 사용자가 확인했는가.
    /// macOS 밖에서는 이 컴퓨터 자신의 VNC 서버(루프백·자기 인터페이스 주소)에 접속할 때도 이 확인이 필요하다.
    pub dedicated_ack: bool,
}

/// 에이전트가 조작할 격리 화면의 종류와 설정
#[derive(Clone)]
pub enum DeskTarget {
    Browser {
        source: ProfileSource,
        headless: bool,
        width: u32,
        height: u32,
        start_url: String,
    },
    Remote(RemoteTarget),
    /// 숨김 실행 Electron 앱 (빈 프로필 또는 사용자 동의 하의 userData 복제)
    Electron {
        app: crate::electron::ElectronApp,
        profile: crate::electron::ElectronProfile,
        width: u32,
        height: u32,
    },
    /// macOS 앱을 숨긴 채 조작
    Native(crate::native::NativeOptions),
    /// 실험: 가상 디스플레이 위에서 macOS 앱 실행 (사용자 동의 필요)
    VDisplay(crate::vdisplay::VirtualDisplayOptions),
    /// 전용 에이전트 계정의 별도 로그인 세션 (RDP 방식)
    Session { account: String },
}

#[derive(Clone)]
pub struct AgentConfig {
    pub name: String,
    pub workspace: String,
    pub task: String,
    pub target: DeskTarget,
    pub max_steps: u64,
    /// 사람에게 답할 언어 (영어 이름: "Korean", "English" …) — 화면 언어를 따른다
    pub reply_lang: &'static str,
    pub llm: Llm,
}

pub struct AgentShared {
    pub id: u64,
    pub name: String,
    pub workspace_name: String,
    pub status: Mutex<AgentStatus>,
    pub log: Mutex<Vec<LogEntry>>,
    pub frame: Mutex<Option<egui::ColorImage>>,
    pub frame_seq: AtomicU64,
    /// 아직 디코딩하지 않은 최신 프레임 / 디코더 실행 중 여부
    pending_frame: Mutex<Option<Vec<u8>>>,
    decoding: AtomicBool,
    pub workspace: Mutex<Option<Desk>>,
    pub paused: AtomicBool,
    /// 화면에 그릴 가상 마우스 상태 (에이전트/사용자 동작 기록)
    pub cursor: Mutex<crate::cursor::CursorState>,
    /// 현재 작업이 끝나면 차례로 이어서 실행할 작업들
    pub queue: Mutex<std::collections::VecDeque<String>>,
    /// 외부(MCP) 에이전트가 조작하는 화면이면 그 클라이언트 설명 (LLM 루프 없음)
    pub external: Mutex<Option<String>>,
    /// 현재 배치 이후 멈추고 지시를 기다리게 한다 (대화 맥락은 유지)
    pub stop: AtomicBool,
    /// GUI 에서 '제거'됨. 준비 중이던 화면은 준비가 끝나는 즉시 닫아야 한다 (외부 화면 포함)
    pub removed: AtomicBool,
    pub tokens: Mutex<(u64, u64)>,
    pub steps: AtomicU64,
    /// 마지막 지시 뒤로 쓴 단계 (최대 단계에 닿으면 멈춘다 — 화면의 진행 막대)
    pub run_steps: AtomicU64,
    pub ctx: egui::Context,
    started: Instant,
}

impl AgentShared {
    pub fn new(id: u64, name: String, workspace_name: String, ctx: egui::Context) -> Arc<Self> {
        Arc::new(AgentShared {
            id,
            name,
            workspace_name,
            status: Mutex::new(AgentStatus::Preparing("시작".into())),
            log: Mutex::new(Vec::new()),
            frame: Mutex::new(None),
            frame_seq: AtomicU64::new(0),
            pending_frame: Mutex::new(None),
            decoding: AtomicBool::new(false),
            workspace: Mutex::new(None),
            paused: AtomicBool::new(false),
            cursor: Mutex::new(Default::default()),
            queue: Mutex::new(Default::default()),
            external: Mutex::new(None),
            stop: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            tokens: Mutex::new((0, 0)),
            steps: AtomicU64::new(0),
            run_steps: AtomicU64::new(0),
            ctx,
            started: Instant::now(),
        })
    }

    pub fn log(&self, kind: LogKind, text: impl Into<String>) {
        let mut log = self.log.lock();
        if log.len() >= LOG_CAP {
            log.drain(..LOG_DRAIN);
        }
        log.push(LogEntry { kind, text: text.into(), secs: self.started.elapsed().as_secs_f32() });
        drop(log);
        self.ctx.request_repaint();
    }

    pub fn set_status(&self, s: AgentStatus) {
        *self.status.lock() = s;
        self.ctx.request_repaint();
    }

    pub fn status(&self) -> AgentStatus {
        self.status.lock().clone()
    }

    pub fn workspace(&self) -> Option<Desk> {
        self.workspace.lock().clone()
    }

    /// 원시 RGBA 프레임을 라이브 화면으로 올린다.
    pub fn push_rgba(&self, w: u32, h: u32, rgba: &[u8]) {
        if w == 0 || h == 0 || rgba.len() < (w * h * 4) as usize {
            return;
        }
        let ci = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba[..(w * h * 4) as usize]);
        *self.frame.lock() = Some(ci);
        self.frame_seq.fetch_add(1, Ordering::Relaxed);
        self.ctx.request_repaint();
    }

    /// 인코딩된 프레임(PNG/JPEG)을 라이브 화면으로 올린다.
    /// 프레임이 몰려도 가장 최신 것 하나만 백그라운드 스레드에서 디코딩한다 (latest-wins).
    pub fn push_frame(self: &Arc<Self>, bytes: &[u8]) {
        *self.pending_frame.lock() = Some(bytes.to_vec());
        if self.decoding.swap(true, Ordering::AcqRel) {
            return; // 이미 디코더가 돌고 있으면 그쪽이 최신 프레임을 가져간다
        }
        let me = self.clone();
        let work = move || me.decode_pending();
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn_blocking(work);
            }
            Err(_) => work(),
        }
    }

    fn decode_pending(&self) {
        loop {
            let next = self.pending_frame.lock().take();
            let Some(bytes) = next else {
                self.decoding.store(false, Ordering::Release);
                // 플래그를 내린 사이에 새 프레임이 들어왔으면 다시 맡는다
                if self.pending_frame.lock().is_some() && !self.decoding.swap(true, Ordering::AcqRel) {
                    continue;
                }
                return;
            };
            if let Ok(img) = image::load_from_memory(&bytes) {
                let rgba = img.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let ci = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                *self.frame.lock() = Some(ci);
                self.frame_seq.fetch_add(1, Ordering::Relaxed);
                self.ctx.request_repaint();
            }
        }
    }
}

pub async fn run_agent(shared: Arc<AgentShared>, cfg: AgentConfig, mut rx: mpsc::UnboundedReceiver<AgentCmd>) {
    if let Err(e) = run_inner(&shared, &cfg, &mut rx).await {
        shared.log(LogKind::Error, format!("{e:#}"));
        shared.set_status(AgentStatus::Error(short(&format!("{e}"), 80)));
    }
}

async fn run_inner(shared: &Arc<AgentShared>, cfg: &AgentConfig, rx: &mut mpsc::UnboundedReceiver<AgentCmd>) -> Result<()> {
    let ws = workspace::prepare_workspace(shared, &cfg.workspace, &cfg.target).await?;
    // 준비 중에 '제거'됐으면 보이지 않는 화면을 남기지 않는다.
    // (remove_agent 는 removed 를 먼저 세운 뒤 같은 락으로 take 하므로 둘 중 한쪽만 화면을 가진다)
    let stored = {
        let mut slot = shared.workspace.lock();
        if shared.removed.load(Ordering::SeqCst) {
            false
        } else {
            *slot = Some(ws.clone());
            true
        }
    };
    if !stored {
        ws.shutdown().await;
        return Ok(());
    }
    let _ = ws.screenshot().await;
    // 도구 목록과 시스템 프롬프트는 대화 내내 고정한다 (캐시/사고 블록 보존을 위해)
    let system = prompt::system_prompt(ws.kind(), cfg.reply_lang);
    let tools = Llm::tools(ws.kind());

    let mut messages: Vec<Value> = Vec::new();
    // messages 를 직렬화했을 때의 대략적인 크기 (요청 본문 크기 추적용)
    let mut msg_bytes: usize = 0;
    // 원래 작업과 이후 지시 (새 대화 구간을 시작할 때 요약에 넣는다)
    let mut instructions: Vec<String> = Vec::new();
    let mut pending: Vec<Value> = Vec::new();
    // 이미 보낸 화면·창 목록 (같으면 다시 보내지 않는다)
    let mut seen = Seen::default();
    // 직전 요청이 너무 길어(토큰) 다음 턴에 새 대화 구간을 시작해야 하는가
    let mut too_long = false;
    if !cfg.task.trim().is_empty() {
        remember(&mut instructions, &cfg.task);
        shared.log(LogKind::User, cfg.task.clone());
        pending.push(json!({ "type": "text", "text": cfg.task }));
        pending.push(screenshot_context(&ws, &mut seen).await);
    }
    let mut steps_since_input: u64 = 0;
    let mut wait_reason = String::from("지시를 입력하세요");
    // 직전 AI API 호출이 실패해 지시를 기다려야 하는가 (보내지 못한 턴은 pending 에 남아 있다)
    let mut llm_failed = false;

    loop {
        // 실행 중 들어온 추가 지시는 다음 user 메시지에 합류
        while let Ok(cmd) = rx.try_recv() {
            if let AgentCmd::Instruction(t) | AgentCmd::Answer(t) = cmd {
                remember(&mut instructions, &t);
                shared.log(LogKind::User, t.clone());
                pending.push(json!({ "type": "text", "text": format!("[사용자 추가 지시] {t}") }));
                steps_since_input = 0;
                shared.run_steps.store(0, Ordering::Relaxed);
            }
        }

        let stopped = shared.stop.swap(false, Ordering::SeqCst);
        // max_steps == 0 이면 단계 제한 없이 계속 진행
        let limit_hit = cfg.max_steps > 0 && steps_since_input >= cfg.max_steps;
        let failed = std::mem::take(&mut llm_failed);
        // 중지·단계 제한·오류가 아니라 정상적으로 끝났을 때만 대기열 작업으로 이어간다
        let finished_normally = !stopped && !limit_hit && !failed;
        // 현재 작업이 끝났고 대기열에 다음 작업이 있으면 멈추지 않고 바로 이어서 시작
        if pending.is_empty() && finished_normally {
            let next = shared.queue.lock().pop_front();
            if let Some(t) = next {
                remember(&mut instructions, &t);
                shared.log(LogKind::User, format!("{} 대기열의 다음 작업: {t}", crate::icon::LIST_TODO));
                pending.push(json!({ "type": "text", "text": format!("이전 작업이 끝났습니다. 다음 작업을 이어서 하세요: {t}") }));
                pending.push(screenshot_context(&ws, &mut seen).await);
                steps_since_input = 0;
                shared.run_steps.store(0, Ordering::Relaxed);
            }
        }
        let needs_input = pending.is_empty() || !finished_normally;
        if needs_input {
            if stopped {
                shared.log(LogKind::Info, "사용자가 중지했습니다. 지시를 입력하면 이어서 진행합니다.");
                wait_reason = "중지됨 — 지시를 입력하면 이어서 진행".into();
            } else if limit_hit {
                wait_reason = format!("최대 단계({})에 도달 — 계속하려면 지시를 입력", cfg.max_steps);
            }
            // (failed 이면 wait_reason 은 오류 처리 쪽에서 이미 정했다)
            shared.set_status(AgentStatus::Idle(wait_reason.clone()));
            // 대기 중에는 멈출 동작이 없다: 대기에 들어가며 남은 '중지'를 지운다
            shared.stop.store(false, Ordering::SeqCst);
            // 지시가 오거나, (정상적으로 끝났을 때만) 대기열에 작업이 들어올 때까지 기다린다.
            // 중지·단계 제한·오류로 멈췄다면 대기열 알림(Wake)으로는 재개하지 않는다.
            let t = loop {
                let Some(cmd) = rx.recv().await else { return Ok(()) };
                match cmd {
                    AgentCmd::Instruction(t) | AgentCmd::Answer(t) => break t,
                    AgentCmd::Wake => {
                        if !finished_normally {
                            continue;
                        }
                        if let Some(t) = shared.queue.lock().pop_front() {
                            break format!("다음 작업: {t}");
                        }
                    }
                }
            };
            // 대기 중에 눌린 '중지'는 새 지시의 첫 배치를 버리게 하면 안 된다
            shared.stop.store(false, Ordering::SeqCst);
            remember(&mut instructions, &t);
            shared.log(LogKind::User, t.clone());
            let only_text = pending.is_empty();
            pending.push(json!({ "type": "text", "text": t }));
            if only_text {
                pending.push(screenshot_context(&ws, &mut seen).await);
            }
            steps_since_input = 0;
            shared.run_steps.store(0, Ordering::Relaxed);
        }

        wait_while_paused(shared).await;
        if ws.is_closed() {
            return Err(anyhow!("에이전트 화면이 닫혔습니다"));
        }

        let mut user_msg = json!({ "role": "user", "content": std::mem::take(&mut pending) });
        let mut user_len = json_len(&user_msg);
        let too_big = msg_bytes + user_len > MAX_REQUEST_BYTES;
        if !messages.is_empty() && (too_big || std::mem::take(&mut too_long)) {
            // 기록이 너무 커지면(요청 크기 한도·턴마다 늘어나는 토큰) 새 대화 구간을 시작한다:
            // 원래 작업 + 최근 동작 요약 + 현재 화면만 이어간다.
            let why = if too_big { format!("약 {:.1} MB 로 커져", (msg_bytes + user_len) as f64 / 1e6) } else { "길어져".into() };
            shared.log(LogKind::Info, format!("대화 기록이 {why} 새 대화 구간을 시작합니다 (원래 작업·최근 동작 요약·현재 화면만 이어감)"));
            seen = Seen::default();
            let last = user_msg["content"].as_array().cloned().unwrap_or_default();
            let summary = {
                let log = shared.log.lock();
                segment_summary(&instructions, &log, &last)
            };
            user_msg = json!({
                "role": "user",
                "content": [{ "type": "text", "text": summary }, screenshot_context(&ws, &mut seen).await],
            });
            user_len = json_len(&user_msg);
            messages.clear();
            msg_bytes = 0;
        }
        messages.push(user_msg);
        msg_bytes += user_len;
        msg_bytes = msg_bytes.saturating_sub(prune_images(&mut messages));
        shared.set_status(AgentStatus::Thinking);
        let resp = match cfg.llm.send(&system, &tools, &messages).await {
            Ok(r) => r,
            Err(e) => {
                // 에이전트를 끝내지 않는다: 보내지 못한 user 턴을 pending 으로 되돌려 대화 맥락을 유지하고,
                // 다음 지시가 오면 그 내용과 함께 다시 보낸다.
                if let Some(Value::Object(mut m)) = messages.pop() {
                    msg_bytes = msg_bytes.saturating_sub(user_len);
                    if let Some(Value::Array(c)) = m.remove("content") {
                        pending = c;
                    }
                }
                shared.log(LogKind::Error, format!("AI API 오류: {e:#}"));
                wait_reason = "오류 — 지시를 입력하면 다시 시도".into();
                llm_failed = true;
                continue;
            }
        };
        let u = &resp["usage"];
        let input = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"].iter().map(|k| u[*k].as_u64().unwrap_or(0)).sum::<u64>();
        {
            let mut t = shared.tokens.lock();
            t.0 += input;
            t.1 += u["output_tokens"].as_u64().unwrap_or(0);
        }
        too_long = input > MAX_CONTEXT_TOKENS;
        // Claude 는 서버가 오래된 도구 결과를 지운다. 다른 회사는 같은 기준으로 여기서 지운다 (다음 요청부터 적용)
        if !cfg.llm.provider.clears_old_tool_results() && input > CLEAR_TOOL_RESULTS_AT {
            msg_bytes = msg_bytes.saturating_sub(clear_old_tool_results(&mut messages, crate::llm::KEEP_TOOL_USES));
        }
        let content = resp["content"].as_array().cloned().unwrap_or_default();
        // 빈 답은 기록하지 않는다: 내용 없는 assistant 턴은 다음 요청에서 거절된다
        // (이어지는 user 턴은 API 가 앞 user 턴과 합쳐 준다)
        if !content.is_empty() {
            // 사고(thinking) 블록은 수정 없이 그대로 되돌려 보내야 한다 (preserved thinking)
            let assistant = json!({ "role": "assistant", "content": content.clone() });
            msg_bytes += json_len(&assistant);
            messages.push(assistant);
        }

        let mut tool_uses = Vec::new();
        for b in &content {
            match b["type"].as_str() {
                Some("text") => {
                    let t = b["text"].as_str().unwrap_or_default();
                    if !t.trim().is_empty() {
                        shared.log(LogKind::Assistant, t);
                    }
                }
                Some("thinking") => {
                    let t = b["thinking"].as_str().unwrap_or_default();
                    if !t.trim().is_empty() {
                        shared.log(LogKind::Thinking, t);
                    }
                }
                Some("tool_use") => tool_uses.push(b.clone()),
                _ => {}
            }
        }

        let stop = resp["stop_reason"].as_str().unwrap_or("");
        if stop == "refusal" {
            let cat = resp["stop_details"]["category"].as_str().unwrap_or("unknown");
            shared.log(LogKind::Error, format!("모델이 요청을 거절했습니다 (category: {cat})"));
            wait_reason = "모델 거절 — 다른 지시를 입력하세요".into();
            // 정상 완료가 아니므로 대기열의 다음 작업으로 넘어가지 않고 지시를 기다린다
            llm_failed = true;
            continue;
        }
        if stop == "max_tokens" && tool_uses.is_empty() {
            shared.log(LogKind::Info, "응답이 max_tokens 에서 잘렸습니다");
        }
        if tool_uses.is_empty() {
            wait_reason = "작업 완료 — 추가 지시를 입력할 수 있습니다".into();
            continue;
        }

        pending = tools::execute_batch(shared, &ws, &tool_uses, rx, &mut seen).await;
        steps_since_input += 1;
        shared.run_steps.store(steps_since_input, Ordering::Relaxed);
        shared.steps.fetch_add(1, Ordering::Relaxed);
    }
}

/// 동작 뒤 화면이 안정될 때까지 짧게 기다린다. 브라우저는 페이지가 준비되면 바로 넘어간다.
pub(crate) async fn settle(ws: &Desk) {
    if let Some(b) = ws.as_browser() {
        b.wait_ready(Duration::from_millis(1500)).await;
        tokio::time::sleep(Duration::from_millis(120)).await;
    } else if let Some(e) = ws.as_electron() {
        e.inner().wait_ready(Duration::from_millis(1500)).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
    } else {
        tokio::time::sleep(Duration::from_millis(350)).await;
    }
}

async fn wait_while_paused(shared: &AgentShared) {
    if shared.paused.load(Ordering::Relaxed) {
        let prev = shared.status();
        shared.set_status(AgentStatus::Idle("일시정지됨".into()));
        while shared.paused.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        shared.set_status(prev);
    }
}
