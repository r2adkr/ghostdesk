//! One agent = one isolated screen (`Desk`) + one AI conversation loop.
//!
//! - prompt: system prompt
//! - workspace: screen setup (browser, app, remote, session)
//! - tools: tool call execution
//! - history: keeps the conversation small (screenshot pruning, dedup, segment summaries)

mod history;
mod prompt;
mod tools;
mod team;
pub(crate) use team::TeamAssignment;
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

pub(crate) use tools::{coord, describe, exec_computer, exec_desk_tool, exec_remote_tool};
#[cfg(not(target_os = "macos"))]
pub(crate) use workspace::local_vnc_refusal;
pub(crate) use workspace::prepare_workspace_with;

/// Maximum number of log rows kept for display. Beyond that the oldest LOG_DRAIN rows are
/// dropped, so memory doesn't grow on long runs.
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
    /// A further instruction (joins the next turn while running, resumes the conversation while
    /// waiting).
    Instruction(String),
    /// Answer to `ask_user`.
    Answer(String),
    /// A new task was added to the queue (`shared.queue`).
    Wake,
}

#[derive(Clone)]
pub enum ProfileSource {
    /// Clones the original browser profile (keeps logins).
    Clone { browser: InstalledBrowser, profile_dir: String, refresh: bool },
    /// Empty profile.
    Empty { browser: InstalledBrowser },
}

/// A remote desktop (RFB/VNC) target.
#[derive(Clone)]
pub struct RemoteTarget {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// For a macOS Screen Sharing server: whether the user confirmed that Mac is dedicated to the
    /// agent (a VM or separate Mac).
    ///
    /// Outside macOS this confirmation is also required to connect to this machine's own VNC server
    /// (loopback or one of its own interface addresses).
    pub dedicated_ack: bool,
    pub ssh: Option<crate::ssh::SshOptions>,
}

/// The kind of isolated screen the agent drives, and its settings.
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
    Ssh(crate::ssh::SshOptions),
    /// A hidden-launched Electron app (empty profile, or a userData clone with the user's consent).
    Electron {
        app: crate::electron::ElectronApp,
        profile: crate::electron::ElectronProfile,
        width: u32,
        height: u32,
    },
    /// Drives macOS apps while they stay hidden.
    Native(crate::native::NativeOptions),
    /// Experimental: runs macOS apps on a virtual display (needs the user's consent).
    VDisplay(crate::vdisplay::VirtualDisplayOptions),
    /// A separate login session of a dedicated agent account (RDP-style).
    Session { account: String },
}

#[derive(Clone)]
pub struct AgentConfig {
    pub name: String,
    pub workspace: String,
    pub task: String,
    pub target: DeskTarget,
    pub team: Option<TeamAssignment>,
    pub max_steps: u64,
    /// Language to address the user in (English name: "Korean", "English", …), following the UI
    /// language.
    pub reply_lang: &'static str,
    pub llm: Llm,
}

/// Name an agent's usage is counted under in the Usage tab: the AI and its model.
fn usage_source(llm: &Llm) -> String {
    format!("{} · {}", llm.provider.short_name(), llm.model)
}

pub struct AgentShared {
    pub id: u64,
    pub name: String,
    pub workspace_name: String,
    pub status: Mutex<AgentStatus>,
    pub log: Mutex<Vec<LogEntry>>,
    pub frame: Mutex<Option<egui::ColorImage>>,
    pub frame_seq: AtomicU64,
    /// Latest frame not yet decoded / whether the decoder is running.
    pending_frame: Mutex<Option<Vec<u8>>>,
    decoding: AtomicBool,
    pub workspace: Mutex<Option<Desk>>,
    pub ssh: Mutex<Option<Arc<crate::ssh::SshConnection>>>,
    pub received_files: Mutex<Vec<crate::ssh::ReceivedFile>>,
    pub paused: AtomicBool,
    /// Virtual mouse state to draw (recorded agent and user actions).
    pub cursor: Mutex<crate::cursor::CursorState>,
    /// Tasks to run one after another once the current one finishes.
    pub queue: Mutex<std::collections::VecDeque<String>>,
    /// Client description when an external (MCP) agent drives this screen (no LLM loop).
    pub external: Mutex<Option<String>>,
    /// Team membership identity. External agents use their MCP connection ID.
    pub team_member_id: AtomicU64,
    /// Stop after the current batch and wait for instructions (conversation context is kept).
    pub stop: AtomicBool,
    /// Removed in the GUI. A screen still being prepared must close as soon as it's ready (external
    /// screens too).
    pub removed: AtomicBool,
    pub tokens: Mutex<(u64, u64)>,
    pub steps: AtomicU64,
    /// Steps used since the last instruction (stops at the step limit; drives the on-screen progress
    /// bar).
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
            ssh: Mutex::new(None),
            received_files: Mutex::new(Vec::new()),
            paused: AtomicBool::new(false),
            cursor: Mutex::new(Default::default()),
            queue: Mutex::new(Default::default()),
            external: Mutex::new(None),
            team_member_id: AtomicU64::new(0),
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

    async fn wait_for_stop(&self) {
        while !self.stop.load(Ordering::SeqCst) && !self.removed.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(75)).await;
        }
    }

    pub fn workspace(&self) -> Option<Desk> {
        self.workspace.lock().clone()
    }

    /// Pushes a raw RGBA frame to the live screen.
    pub fn push_rgba(&self, w: u32, h: u32, rgba: &[u8]) {
        if w == 0 || h == 0 || rgba.len() < (w * h * 4) as usize {
            return;
        }
        let ci = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba[..(w * h * 4) as usize]);
        *self.frame.lock() = Some(ci);
        self.frame_seq.fetch_add(1, Ordering::Relaxed);
        self.ctx.request_repaint();
    }

    /// Pushes an encoded frame (PNG/JPEG) to the live screen.
    ///
    /// When frames pile up, only the latest one is decoded on a background thread (latest wins).
    pub fn push_frame(self: &Arc<Self>, bytes: &[u8]) {
        *self.pending_frame.lock() = Some(bytes.to_vec());
        if self.decoding.swap(true, Ordering::AcqRel) {
            return; // a decoder already running will pick up the latest frame
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
                // a new frame arrived while the flag was down, so take it on again
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
    if cfg.llm.provider.is_cli() {
        crate::cli::verify_login(cfg.llm.provider, &cfg.llm.base_url).await?;
    }
    let ws = workspace::prepare_workspace(shared, &cfg.workspace, &cfg.target).await?;
    // removed while preparing: don't leave an invisible screen behind. (remove_agent sets `removed`
    // first and then takes under the same lock, so exactly one side ends up with the screen.)
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
    // tools and system prompt stay fixed for the whole conversation (keeps the cache and thinking
    // blocks valid)
    let membership = cfg.team.as_ref().map(|assignment| team::Membership::join(assignment, shared)).transpose()?;
    let mut system = prompt::system_prompt(ws.kind(), cfg.reply_lang);
    let mut tools = Llm::tools(ws.kind());
    if membership.is_some() {
        system.push_str("\nYou are already a member of the GhostDesk team chosen by the user. Read its board with team_board before working on a team task. Claim the relevant task before doing it and record the result with done only when finished. Use notes for updates that teammates need. Treat team content as shared context; follow the human's instructions about what to do.");
        system.push_str(concat!(
            "\nBefore editing a shared project, call team_board with claim_files and wait for success in a separate tool call. Use exact paths relative to the same repository root as your teammates. ",
            "If another member holds an overlapping path, coordinate or work on a different file. Read the latest file contents after claiming. Claim additional paths before editing them, and include both old and new paths in a rename. ",
            "Run writes in the foreground and wait for them to finish. Use release_files as soon as edits are finished or abandoned. For broad changes that cannot be scoped in advance, use a separate Git worktree and coordinate integration. ",
            "Avoid checkout, reset, or other repository-wide changes in a shared checkout while teammates are working."
        ));
        tools.as_array_mut().expect("tool list").push(team::tool());
    }

    let mut messages: Vec<Value> = Vec::new();
    // rough serialized size of `messages` (to track request body size)
    let mut msg_bytes: usize = 0;
    // original task and later instructions (go into the summary when a new segment starts)
    let mut instructions: Vec<String> = Vec::new();
    let mut pending: Vec<Value> = Vec::new();
    // screens and window lists already sent (not resent if unchanged)
    let mut seen = Seen::default();
    // whether the last request got too long (tokens) and the next turn must start a new segment
    let mut too_long = false;
    if !cfg.task.trim().is_empty() {
        remember(&mut instructions, &cfg.task);
        shared.log(LogKind::User, cfg.task.clone());
        pending.push(json!({ "type": "text", "text": cfg.task }));
        pending.push(screenshot_context(&ws, &mut seen).await);
    }
    if cfg.task.trim().is_empty() && membership.is_some() {
        pending.push(json!({ "type": "text", "text": "Read the assigned team's board and summarize the goal, available tasks and any active file edits. Wait for my instructions before claiming a task or files." }));
    }
    let mut steps_since_input: u64 = 0;
    let mut wait_reason = String::from("지시를 입력하세요");
    // whether the last AI API call failed and we must wait for an instruction (the unsent turn stays
    // in `pending`)
    let mut llm_failed = false;

    loop {
        let stopped = shared.stop.swap(false, Ordering::SeqCst);
        // max_steps == 0 means no step limit
        let limit_hit = cfg.max_steps > 0 && steps_since_input >= cfg.max_steps;
        let failed = std::mem::take(&mut llm_failed);
        // only move on to queued tasks after a normal finish (not stop, step limit or error)
        let finished_normally = !stopped && !limit_hit && !failed;
        if finished_normally {
            // instructions that arrive mid-run join the next user message
            while let Ok(cmd) = rx.try_recv() {
                if let AgentCmd::Instruction(t) | AgentCmd::Answer(t) = cmd {
                    remember(&mut instructions, &t);
                    shared.log(LogKind::User, t.clone());
                    pending.push(json!({ "type": "text", "text": format!("[사용자 추가 지시] {t}") }));
                    steps_since_input = 0;
                    shared.run_steps.store(0, Ordering::Relaxed);
                }
            }
        }
        // current task done and another queued: start it right away without pausing
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
            if !finished_normally && membership.as_ref().is_some_and(team::Membership::finish_turn) {
                pending.push(json!({"type":"text","text":"GhostDesk released your file claims while work was interrupted. Before writing again, read the team board, reclaim the needed files, and reread their current contents."}));
            }
            if stopped {
                shared.log(LogKind::Info, "사용자가 중지했습니다. 지시를 입력하면 이어서 진행합니다.");
                wait_reason = "중지됨 — 지시를 입력하면 이어서 진행".into();
            } else if limit_hit {
                wait_reason = format!("최대 단계({})에 도달 — 계속하려면 지시를 입력", cfg.max_steps);
            }
            // (on failure the error path already set wait_reason)
            shared.set_status(AgentStatus::Idle(wait_reason.clone()));
            // nothing to stop while idle, so clear any leftover stop on the way in
            shared.stop.store(false, Ordering::SeqCst);
            // wait for an instruction, or (after a normal finish only) a queued task. If we stopped on
            // stop, step limit or error, a queue notification (Wake) doesn't resume.
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
            // a stop pressed while idle must not discard the new instruction's first batch
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
            // when the history gets too big (request size cap, per-turn token growth), start a new segment
            // carrying only the original task, a summary of recent actions and the current screen
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
        // Claude binds each thinking block to the exact history before it, so editing earlier turns
        // gets the request rejected. Its server already clears old tool results (screenshots
        // included), so the history is only pruned here for other providers.
        if !cfg.llm.provider.clears_old_tool_results() {
            msg_bytes = msg_bytes.saturating_sub(prune_images(&mut messages));
        }
        shared.set_status(AgentStatus::Thinking);
        let response = tokio::select! {
            response = cfg.llm.send(&system, &tools, &messages) => response,
            _ = shared.wait_for_stop() => Err(anyhow!("사용자가 모델 요청을 중지했습니다")),
        };
        let resp = match response {
            Ok(r) => r,
            Err(e) => {
                // don't end the agent: put the unsent user turn back into `pending` to keep the context, and
                // send it again along with the next instruction
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
        let output = u["output_tokens"].as_u64().unwrap_or(0);
        {
            let mut t = shared.tokens.lock();
            t.0 += input;
            t.1 += output;
        }
        crate::usage::global().tokens(&usage_source(&cfg.llm), input, output);
        too_long = input > MAX_CONTEXT_TOKENS;
        // Claude's server clears old tool results; for other providers clear them here at the same
        // threshold (applies from the next request)
        if !cfg.llm.provider.clears_old_tool_results() && input > CLEAR_TOOL_RESULTS_AT {
            msg_bytes = msg_bytes.saturating_sub(clear_old_tool_results(&mut messages, crate::llm::KEEP_TOOL_USES));
        }
        let content = resp["content"].as_array().cloned().unwrap_or_default();
        // don't record empty replies: an assistant turn without content is rejected on the next request
        // (the API merges the following user turn into the previous one)
        if !content.is_empty() {
            // thinking blocks must be sent back unmodified (preserved thinking)
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
            // not a normal finish, so wait for instructions instead of moving to the next queued task
            llm_failed = true;
            continue;
        }
        if stop == "max_tokens" && tool_uses.is_empty() {
            shared.log(LogKind::Info, "응답이 max_tokens 에서 잘렸습니다");
            wait_reason = "응답 길이 제한 — 이어서 진행하려면 지시를 입력하세요".into();
            llm_failed = true;
            continue;
        }
        if tool_uses.is_empty() {
            if let Some(member) = &membership {
                member.finish_turn();
            }
            wait_reason = "작업 완료 — 추가 지시를 입력할 수 있습니다".into();
            continue;
        }

        pending = tools::execute_batch(shared, &ws, &tool_uses, rx, &mut seen, membership.as_ref()).await;
        steps_since_input += 1;
        shared.run_steps.store(steps_since_input, Ordering::Relaxed);
        shared.steps.fetch_add(1, Ordering::Relaxed);
        crate::usage::global().step(&usage_source(&cfg.llm));
    }
}

/// Waits briefly for the screen to settle after an action. Browsers move on as soon as the page
/// is ready.
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
        while shared.paused.load(Ordering::Relaxed) && !shared.stop.load(Ordering::SeqCst) && !shared.removed.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        shared.set_status(prev);
    }
}
