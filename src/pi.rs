//! The engine behind key-based AI agents when pi is installed. Nothing here is visible to the user:
//! the agent screen, logs, usage, stop/pause/queue, questions and step limit behave exactly as with
//! GhostDesk's own loop.
//!
//! One long-lived `pi --mode rpc` process per agent, in a private config folder (the user's own pi
//! setup is never read or changed). Its only tools are GhostDesk's own, registered by a small
//! extension that forwards every call to this process over pi's extension-dialog channel; they run
//! through the same executor as the built-in loop (`agent::ToolBridge`), against the agent's own
//! screen. The system prompt is GhostDesk's, replacing pi's coding prompt.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::{
    agent::{AgentCmd, AgentConfig, AgentShared, AgentStatus, LogKind, Membership, ToolBridge},
    desk::Desk,
    llm::{Llm, Provider},
};

/// One-line installer shown to the user before it runs (it also installs Node.js when missing).
pub fn install_command() -> &'static str {
    if cfg!(windows) {
        "powershell -c \"irm https://pi.dev/install.ps1 | iex\""
    } else {
        "curl -fsSL https://pi.dev/install.sh | sh"
    }
}

/// PATH for the installer: an app launched from the Dock has a short one that misses Homebrew and node.
#[cfg(not(windows))]
fn installer_path() -> String {
    let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
    let cur = std::env::var("PATH").unwrap_or_default();
    format!("/opt/homebrew/bin:/usr/local/bin:{home}/.local/bin:{cur}")
}

/// Whether `node` 22.19 or newer is available.
#[cfg(not(windows))]
fn node_ok() -> bool {
    std::process::Command::new("/bin/sh")
        .env("PATH", installer_path())
        .args(["-c", "node -e 'const [a,b]=process.versions.node.split(\".\").map(Number);process.exit(a>22||(a===22&&b>=19)?0:1)'"])
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Runs the installer (blocking, no terminal): installs Node.js through Homebrew first when it is missing
/// (the official script can only ask about that in a terminal), then pi's own installer.
pub fn install() -> Result<(), String> {
    let fail = |what: &str, out: std::process::Output| {
        let err = String::from_utf8_lossy(&out.stderr);
        let text = if err.trim().is_empty() { String::from_utf8_lossy(&out.stdout).into_owned() } else { err.into_owned() };
        format!("{what}: {}", crate::llm::short(text.trim(), 300))
    };
    #[cfg(windows)]
    {
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", "irm https://pi.dev/install.ps1 | iex"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        return if out.status.success() { Ok(()) } else { Err(fail("설치에 실패했어요", out)) };
    }
    #[cfg(not(windows))]
    {
        let sh = |cmd: &str| std::process::Command::new("/bin/sh").env("PATH", installer_path()).args(["-c", cmd]).stdin(Stdio::null()).output();
        if !node_ok() {
            let has_brew = sh("command -v brew").is_ok_and(|o| o.status.success());
            if !has_brew {
                return Err("Node.js 22.19 이상이 필요해요. nodejs.org 에서 설치한 뒤 다시 눌러 주세요".into());
            }
            let out = sh("brew install node").map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(fail("Node.js 설치에 실패했어요", out));
            }
        }
        let out = sh(install_command()).map_err(|e| e.to_string())?;
        if out.status.success() { Ok(()) } else { Err(fail("설치에 실패했어요", out)) }
    }
}

pub fn find() -> Option<PathBuf> {
    // lets screenshots and tests show the not-installed state
    if std::env::var_os("GHOSTDESK_NO_PI").is_some() {
        return None;
    }
    crate::install::find_cli("pi")
}

/// pi's name for a provider and the environment variable holding its key. `None` for providers pi
/// can't be pointed at with just a key (local servers, custom URLs, CLI logins).
pub fn provider_args(p: Provider) -> Option<(&'static str, &'static str)> {
    Some(match p {
        Provider::Anthropic => ("anthropic", "ANTHROPIC_API_KEY"),
        Provider::OpenAi => ("openai", "OPENAI_API_KEY"),
        Provider::Gemini => ("google", "GEMINI_API_KEY"),
        Provider::Xai => ("xai", "XAI_API_KEY"),
        Provider::OpenRouter => ("openrouter", "OPENROUTER_API_KEY"),
        Provider::Mistral => ("mistral", "MISTRAL_API_KEY"),
        _ => return None,
    })
}

/// Whether an agent with this AI can run through pi.
pub fn supports(p: Provider) -> bool {
    provider_args(p).is_some()
}

/// pi is installed and this AI (with its default endpoint and a key) can use it.
pub fn usable(llm: &Llm) -> bool {
    supports(llm.provider) && !llm.api_key.is_empty() && llm.base_url == llm.provider.default_base_url() && find().is_some()
}

/// Marks an extension dialog as a tool call (the title is `MARK` + JSON).
const MARK: &str = "\u{1}gd\u{1}";

/// The pi extension: registers GhostDesk's tools, forwards each call to GhostDesk and sets the system
/// prompt. `tools` is a list of `{ name, description, parameters }`.
pub fn extension_source(tools: &[Value], system: &str) -> String {
    format!(
        r#"const TOOLS = {tools};
const SYSTEM = {system};
const MARK = "\u0001gd\u0001";
export default function (pi) {{
  pi.on("before_agent_start", async () => ({{ systemPrompt: SYSTEM }}));
  for (const t of TOOLS) {{
    pi.registerTool({{
      name: t.name, label: t.name, description: t.description, parameters: t.parameters,
      async execute(id, params, signal, onUpdate, ctx) {{
        const raw = await ctx.ui.input(MARK + JSON.stringify({{ name: t.name, input: params }}), "");
        if (raw === undefined) throw new Error("Cancelled by the human operator.");
        const r = JSON.parse(raw);
        if (r.error) throw new Error(r.error);
        return {{ content: r.content, details: {{}} }};
      }},
    }});
  }}
}}
"#,
        tools = Value::Array(tools.to_vec()),
        system = Value::String(system.to_string()),
    )
}

/// The tools in pi's shape, from the Claude-format list the built-in loop uses.
pub fn tool_defs(tools: &Value) -> Vec<Value> {
    crate::llm::function_tools(tools)
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| json!({ "name": t["function"]["name"], "description": t["function"]["description"], "parameters": t["function"]["parameters"] }))
        .collect()
}

/// Claude-format content blocks → pi content blocks.
pub fn to_pi_content(blocks: &[Value]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|b| match b["type"].as_str()? {
            "text" => Some(json!({ "type": "text", "text": b["text"] })),
            "image" => Some(json!({ "type": "image", "data": b["source"]["data"], "mimeType": b["source"]["media_type"] })),
            _ => None,
        })
        .collect()
}

/// What a line of pi's output means to the agent loop.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// A finished assistant message: reasoning blocks, visible text and a provider error, if any.
    Assistant { thinking: Vec<String>, text: String, error: Option<String> },
    Usage { input: u64, output: u64 },
    /// A tool call from the extension, waiting for its result under `ui_id`.
    ToolCall { ui_id: String, name: String, input: Value },
    /// Any other dialog pi opens (none are expected): answered with "cancelled".
    Dialog { ui_id: String },
    /// pi has nothing more to do on its own.
    Settled,
    /// A command was refused.
    Rejected(String),
}

pub fn parse_event(line: &str) -> Option<Event> {
    let v: Value = serde_json::from_str(line).ok()?;
    match v["type"].as_str()? {
        "message_end" => {
            let m = &v["message"];
            if m["role"] != "assistant" {
                return None;
            }
            let (mut thinking, mut text) = (Vec::new(), Vec::new());
            match &m["content"] {
                Value::String(s) => text.push(s.clone()),
                Value::Array(a) => {
                    for b in a {
                        match b["type"].as_str() {
                            Some("text") => text.extend(b["text"].as_str().map(String::from)),
                            Some("thinking") => thinking.extend(b["thinking"].as_str().filter(|t| !t.trim().is_empty()).map(String::from)),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            let error = (m["stopReason"] == "error").then(|| m["errorMessage"].as_str().unwrap_or("unknown error").to_string());
            let text = text.join("\n");
            (!thinking.is_empty() || !text.trim().is_empty() || error.is_some()).then_some(Event::Assistant { thinking, text, error })
        }
        "turn_end" => {
            let u = &v["message"]["usage"];
            let input = u["input"].as_u64().unwrap_or(0) + u["cacheRead"].as_u64().unwrap_or(0) + u["cacheWrite"].as_u64().unwrap_or(0);
            let output = u["output"].as_u64().unwrap_or(0);
            (input + output > 0).then_some(Event::Usage { input, output })
        }
        "extension_ui_request" => {
            let ui_id = v["id"].as_str()?.to_string();
            if !matches!(v["method"].as_str()?, "select" | "confirm" | "input" | "editor") {
                return None;
            }
            match v["title"].as_str().and_then(|t| t.strip_prefix(MARK)) {
                Some(call) => {
                    let c: Value = serde_json::from_str(call).ok()?;
                    Some(Event::ToolCall { ui_id, name: c["name"].as_str()?.to_string(), input: c["input"].clone() })
                }
                None => Some(Event::Dialog { ui_id }),
            }
        }
        "response" if v["success"] == false => Some(Event::Rejected(v["error"].as_str().unwrap_or("rejected").to_string())),
        "agent_settled" => Some(Event::Settled),
        _ => None,
    }
}

/// pi's thinking level for GhostDesk's effort setting (None = leave pi's default).
fn thinking_level(effort: &str) -> Option<&'static str> {
    Some(match effort {
        "off" | "none" => "off",
        "minimal" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        "max" => "max",
        _ => return None,
    })
}

/// Private config folder: pi's own settings, keys and extensions of the user are never touched.
fn home_dir() -> PathBuf {
    crate::platform::paths::data_root().join("engine")
}

/// Command-line arguments for one pi process.
pub fn build_args(provider: &str, model: &str, extension: &Path, effort: &str) -> Vec<String> {
    let mut a: Vec<String> = ["--mode", "rpc", "--no-session", "--provider", provider, "--model", model]
        .into_iter()
        .map(String::from)
        .collect();
    // nothing of pi's own: no shell/file tools, no MCP servers, no discovered extensions, skills,
    // templates, themes or project context files
    a.extend(["--no-builtin-tools", "--no-mcp", "--no-extensions", "--no-skills", "--no-prompt-templates", "--no-themes", "--no-context-files"].map(String::from));
    a.push("-e".into());
    a.push(extension.display().to_string());
    if let Some(level) = thinking_level(effort) {
        a.push("--thinking".into());
        a.push(level.into());
    }
    a
}

fn base_command(exe: &Path, home: &Path, env_key: &str, api_key: &str) -> std::process::Command {
    let mut c = crate::install::cli_command(exe);
    #[cfg(not(windows))]
    c.current_dir(home);
    c.env(env_key, api_key)
        .env("PI_CODING_AGENT_DIR", home)
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("PI_TELEMETRY", "0")
        .stdin(Stdio::null());
    c
}

/// Whether pi's model list has this model for this provider (else the built-in loop is used).
async fn model_known(exe: &Path, home: &Path, provider: &str, env_key: &str, llm: &Llm) -> bool {
    let mut c = base_command(exe, home, env_key, &llm.api_key);
    c.args(["--list-models", &llm.model]).stdout(Stdio::piped()).stderr(Stdio::null());
    let out = tokio::task::spawn_blocking(move || {
        let mut child = crate::ssh::ProcessGuard(crate::platform::proc::spawn_group(&mut c).ok()?);
        let mut text = String::new();
        child.0.stdout.take()?.read_to_string(&mut text).ok()?;
        Some(text)
    });
    let Ok(Ok(Some(text))) = tokio::time::timeout(Duration::from_secs(30), out).await else { return false };
    text.lines().any(|l| {
        let mut cols = l.split_whitespace();
        cols.next() == Some(provider) && cols.next() == Some(llm.model.as_str())
    })
}

/// A running pi process: lines it printed, a way to write commands, its last words on stderr.
struct Proc {
    _guard: crate::ssh::ProcessGuard,
    input: std::sync::mpsc::Sender<String>,
    lines: mpsc::UnboundedReceiver<String>,
    err: Arc<parking_lot::Mutex<String>>,
}

impl Proc {
    fn spawn(mut c: std::process::Command) -> Result<Proc> {
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = crate::ssh::ProcessGuard(crate::platform::proc::spawn_group(&mut c)?);
        let mut out = child.0.stdout.take().ok_or_else(|| anyhow!("엔진 출력을 읽을 수 없습니다"))?;
        let mut err = child.0.stderr.take().ok_or_else(|| anyhow!("엔진 오류 출력을 읽을 수 없습니다"))?;
        let mut stdin = child.0.stdin.take().ok_or_else(|| anyhow!("엔진 입력을 열 수 없습니다"))?;
        let (tx, lines) = mpsc::unbounded_channel::<String>();
        std::thread::spawn(move || {
            // records end at LF only (Unicode separators are valid inside strings)
            let (mut buf, mut chunk) = (Vec::<u8>::new(), [0u8; 16384]);
            while let Ok(n) = out.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                while let Some(i) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=i).collect();
                    if tx.send(String::from_utf8_lossy(&line).trim_end().to_string()).is_err() {
                        return;
                    }
                }
            }
        });
        let tail = Arc::new(parking_lot::Mutex::new(String::new()));
        let tail2 = tail.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = err.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut t = tail2.lock();
                t.push_str(&String::from_utf8_lossy(&chunk[..n]));
                if t.len() > 8192 {
                    let cut = t.len() - 8192;
                    let cut = (cut..t.len()).find(|i| t.is_char_boundary(*i)).unwrap_or(t.len());
                    t.drain(..cut);
                }
            }
        });
        let (input, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            while let Ok(l) = rx.recv() {
                if stdin.write_all(l.as_bytes()).and_then(|_| stdin.write_all(b"\n")).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        Ok(Proc { _guard: child, input, lines, err: tail })
    }

    fn send(&self, v: Value) {
        let _ = self.input.send(v.to_string());
    }
}

/// How one run of pi (one instruction) ended.
#[derive(Default)]
struct Outcome {
    stopped: bool,
    limit_hit: bool,
    failed: bool,
    /// The process ended on its own (or had to be killed).
    died: bool,
    /// The agent was closed.
    closed: bool,
}

/// Everything `run` needs from the agent.
pub struct Job<'a> {
    pub shared: &'a Arc<AgentShared>,
    pub cfg: &'a AgentConfig,
    pub ws: &'a Desk,
    pub system: &'a str,
    pub tools: &'a Value,
    pub membership: Option<&'a Membership>,
}

/// The first and later screens of an instruction: a short recap when the process had to be restarted.
fn recap(shared: &AgentShared) -> String {
    let log = shared.log.lock();
    let lines: Vec<String> = log
        .iter()
        .filter(|l| matches!(l.kind, LogKind::User | LogKind::Assistant))
        .rev()
        .take(12)
        .map(|l| format!("{}: {}", if l.kind == LogKind::User { "Human" } else { "You" }, crate::llm::short(&l.text, 600)))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("[Earlier in this conversation, before an interruption]\n{}\n\n", lines.join("\n"))
}

/// The agent loop on pi. Returns `Ok(false)` (before touching anything) when pi can't serve this agent,
/// so the caller runs its own loop instead.
pub async fn run(job: Job<'_>, rx: &mut mpsc::UnboundedReceiver<AgentCmd>) -> Result<bool> {
    let Job { shared, cfg, ws, system, tools, membership } = job;
    let (Some(exe), Some((name, env_key))) = (find(), provider_args(cfg.llm.provider)) else { return Ok(false) };
    let home = home_dir();
    std::fs::create_dir_all(&home)?;
    if !model_known(&exe, &home, name, env_key, &cfg.llm).await {
        return Ok(false);
    }
    let extension = home.join(format!("bridge-{}.mjs", shared.id));
    std::fs::write(&extension, extension_source(&tool_defs(tools), system))?;
    let result = drive_agent(shared, cfg, ws, &exe, &home, &extension, name, env_key, membership, rx).await;
    let _ = std::fs::remove_file(&extension);
    result.map(|_| true)
}

#[allow(clippy::too_many_arguments)]
async fn drive_agent(
    shared: &Arc<AgentShared>,
    cfg: &AgentConfig,
    ws: &Desk,
    exe: &Path,
    home: &Path,
    extension: &Path,
    provider: &str,
    env_key: &str,
    membership: Option<&Membership>,
    rx: &mut mpsc::UnboundedReceiver<AgentCmd>,
) -> Result<()> {
    let mut bridge = ToolBridge::new();
    let mut proc: Option<Proc> = None;
    let mut restarted = false;
    // (text to show in the log, text for the model)
    let mut next: Option<(Option<String>, String)> = if !cfg.task.trim().is_empty() {
        Some((Some(cfg.task.clone()), cfg.task.clone()))
    } else if membership.is_some() {
        Some((None, "Read the assigned team's board and summarize the goal, available tasks and any active file edits. Wait for my instructions before claiming a task or files.".into()))
    } else {
        None
    };
    let mut finished_normally = true;
    let mut wait_reason = String::from("지시를 입력하세요");
    loop {
        let (shown, prompt) = match next.take() {
            Some(n) => n,
            None => {
                // a finished task moves straight on to the next queued one
                let queued = if finished_normally { shared.queue.lock().pop_front() } else { None };
                match queued {
                    Some(t) => (
                        Some(format!("{} 대기열의 다음 작업: {t}", crate::icon::LIST_TODO)),
                        format!("이전 작업이 끝났습니다. 다음 작업을 이어서 하세요: {t}"),
                    ),
                    None => {
                        shared.set_status(AgentStatus::Idle(wait_reason.clone()));
                        shared.stop.store(false, std::sync::atomic::Ordering::SeqCst);
                        let t = loop {
                            let Some(cmd) = rx.recv().await else { return Ok(()) };
                            match cmd {
                                AgentCmd::Instruction(t) | AgentCmd::Answer(t) => break t,
                                AgentCmd::Wake => {
                                    if finished_normally {
                                        if let Some(t) = shared.queue.lock().pop_front() {
                                            break format!("다음 작업: {t}");
                                        }
                                    }
                                }
                            }
                        };
                        (Some(t.clone()), t)
                    }
                }
            }
        };
        if let Some(s) = shown {
            shared.log(LogKind::User, s);
        }
        shared.stop.store(false, std::sync::atomic::Ordering::SeqCst);
        shared.run_steps.store(0, std::sync::atomic::Ordering::Relaxed);
        if ws.is_closed() {
            bail!("에이전트 화면이 닫혔습니다");
        }
        if proc.is_none() {
            let mut c = base_command(exe, home, env_key, &cfg.llm.api_key);
            c.args(build_args(provider, &cfg.llm.model, extension, &cfg.llm.effort));
            proc = Some(Proc::spawn(c)?);
        }
        let p = proc.as_mut().expect("just started");
        // the current screen goes along with the instruction
        let mut command = json!({ "type": "prompt", "message": if restarted { format!("{}{prompt}", recap(shared)) } else { prompt } });
        restarted = false;
        let block = bridge.screen(ws).await;
        if block["type"] == "image" {
            command["images"] = json!([{ "type": "image", "data": block["source"]["data"], "mimeType": block["source"]["media_type"] }]);
        } else if let Some(t) = block["text"].as_str() {
            command["message"] = json!(format!("{}\n\n{t}", command["message"].as_str().unwrap_or_default()));
        }
        p.send(command);
        shared.set_status(AgentStatus::Thinking);
        let out = drive(p, shared, cfg, ws, &mut bridge, membership, rx).await;
        if out.closed {
            return Ok(());
        }
        if out.died {
            let why = p.err.lock().trim().to_string();
            shared.log(LogKind::Error, format!("AI 엔진이 예기치 않게 끝났습니다{}", if why.is_empty() { String::new() } else { format!(": {}", crate::llm::short(&why, 300)) }));
            proc = None;
            restarted = true;
        }
        shared.stop.store(false, std::sync::atomic::Ordering::SeqCst);
        finished_normally = !(out.stopped || out.limit_hit || out.failed || out.died);
        if let Some(m) = membership {
            m.finish_turn();
        }
        wait_reason = if out.stopped {
            shared.log(LogKind::Info, "사용자가 중지했습니다. 지시를 입력하면 이어서 진행합니다.");
            "중지됨 — 지시를 입력하면 이어서 진행".into()
        } else if out.limit_hit {
            format!("최대 단계({})에 도달 — 계속하려면 지시를 입력", cfg.max_steps)
        } else if out.failed || out.died {
            "오류 — 지시를 입력하면 다시 시도".into()
        } else {
            "작업 완료 — 추가 지시를 입력할 수 있습니다".into()
        };
    }
}

/// Runs one instruction: shows pi's messages, runs its tool calls on the screen, passes along new
/// instructions and Stop, and returns when pi has settled.
async fn drive(
    p: &mut Proc,
    shared: &Arc<AgentShared>,
    cfg: &AgentConfig,
    ws: &Desk,
    bridge: &mut ToolBridge,
    membership: Option<&Membership>,
    rx: &mut mpsc::UnboundedReceiver<AgentCmd>,
) -> Outcome {
    use std::sync::atomic::Ordering::Relaxed;
    let mut out = Outcome::default();
    let mut abort_by: Option<Instant> = None;
    let respond = |p: &Proc, ui_id: &str, value: Value| {
        p.send(json!({ "type": "extension_ui_response", "id": ui_id, "value": value.to_string() }));
    };
    loop {
        tokio::select! {
            line = p.lines.recv() => {
                let Some(line) = line else { out.died = true; break };
                match parse_event(&line) {
                    Some(Event::Assistant { thinking, text, error }) => {
                        for t in thinking {
                            shared.log(LogKind::Thinking, t);
                        }
                        if !text.trim().is_empty() {
                            shared.log(LogKind::Assistant, text);
                        }
                        if let Some(e) = error {
                            shared.log(LogKind::Error, format!("AI API 오류: {e}"));
                            out.failed = true;
                        }
                    }
                    Some(Event::Usage { input, output }) => {
                        let mut t = shared.tokens.lock();
                        t.0 += input;
                        t.1 += output;
                        crate::usage::global().tokens(&crate::agent::usage_source(&cfg.llm), input, output);
                    }
                    Some(Event::ToolCall { ui_id, name, input }) => {
                        if abort_by.is_some() {
                            respond(p, &ui_id, json!({ "error": "Not executed: the human operator stopped the agent." }));
                            continue;
                        }
                        let (content, is_error) = bridge.call(shared, ws, &name, &input, rx, membership).await;
                        if is_error {
                            let text = content.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
                            respond(p, &ui_id, json!({ "error": text }));
                        } else {
                            respond(p, &ui_id, json!({ "content": to_pi_content(&content) }));
                        }
                        let n = shared.run_steps.fetch_add(1, Relaxed) + 1;
                        shared.steps.fetch_add(1, Relaxed);
                        crate::usage::global().step(&crate::agent::usage_source(&cfg.llm));
                        shared.set_status(AgentStatus::Thinking);
                        if cfg.max_steps > 0 && n >= cfg.max_steps {
                            out.limit_hit = true;
                            p.send(json!({ "type": "abort" }));
                            abort_by = Some(Instant::now() + Duration::from_secs(8));
                        }
                    }
                    Some(Event::Dialog { ui_id }) => {
                        p.send(json!({ "type": "extension_ui_response", "id": ui_id, "cancelled": true }));
                    }
                    Some(Event::Rejected(why)) => {
                        shared.log(LogKind::Error, format!("AI API 오류: {why}"));
                        out.failed = true;
                        break;
                    }
                    Some(Event::Settled) => break,
                    None => {}
                }
            }
            cmd = rx.recv() => match cmd {
                None => { out.closed = true; break }
                Some(AgentCmd::Instruction(t) | AgentCmd::Answer(t)) => {
                    // an instruction that arrives mid-run joins before pi's next model call
                    shared.log(LogKind::User, t.clone());
                    shared.run_steps.store(0, Relaxed);
                    p.send(json!({ "type": "steer", "message": format!("[사용자 추가 지시] {t}") }));
                }
                Some(AgentCmd::Wake) => {}
            },
            _ = shared.wait_for_stop(), if abort_by.is_none() => {
                out.stopped = true;
                p.send(json!({ "type": "abort" }));
                abort_by = Some(Instant::now() + Duration::from_secs(8));
            }
            _ = async { tokio::time::sleep_until(abort_by.expect("guarded").into()).await }, if abort_by.is_some() => {
                // pi did not settle after the abort: end the process; the next instruction starts a new one
                out.died = !out.stopped && !out.limit_hit;
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_isolate_pi_and_keep_the_key_out() {
        let a = build_args("anthropic", "claude-sonnet-5-5", Path::new("/x/bridge.mjs"), "high");
        for f in ["--no-builtin-tools", "--no-mcp", "--no-extensions", "--no-skills", "--no-context-files", "--no-session"] {
            assert!(a.iter().any(|x| x == f), "{f}");
        }
        assert!(a.windows(2).any(|w| w == ["-e", "/x/bridge.mjs"]));
        assert!(a.windows(2).any(|w| w == ["--thinking", "high"]));
        assert!(!a.iter().any(|x| x.contains("api-key")));
        assert!(!build_args("openai", "m", Path::new("e"), "weird").iter().any(|x| x == "--thinking"));
    }

    #[test]
    fn events_are_understood() {
        assert_eq!(
            parse_event(r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hm"},{"type":"text","text":"done"}]}}"#),
            Some(Event::Assistant { thinking: vec!["hm".into()], text: "done".into(), error: None })
        );
        assert_eq!(
            parse_event(r#"{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"401"}}"#),
            Some(Event::Assistant { thinking: vec![], text: String::new(), error: Some("401".into()) })
        );
        assert_eq!(parse_event(r#"{"type":"message_end","message":{"role":"user","content":"x"}}"#), None);
        assert_eq!(
            parse_event(r#"{"type":"turn_end","message":{"usage":{"input":10,"output":2,"cacheRead":5,"cacheWrite":0}},"toolResults":[]}"#),
            Some(Event::Usage { input: 15, output: 2 })
        );
        assert_eq!(parse_event(r#"{"type":"agent_settled","aborted":true}"#), Some(Event::Settled));
        assert_eq!(parse_event(r#"{"type":"response","command":"prompt","success":false,"error":"busy"}"#), Some(Event::Rejected("busy".into())));
        assert_eq!(parse_event(r#"{"type":"response","command":"prompt","success":true}"#), None);
        assert_eq!(parse_event("not json"), None);
    }

    #[test]
    fn tool_calls_arrive_as_dialogs() {
        let line = json!({ "type": "extension_ui_request", "id": "u1", "method": "input", "title": format!("{MARK}{}", json!({ "name": "computer", "input": { "action": "screenshot" } })) });
        assert_eq!(
            parse_event(&line.to_string()),
            Some(Event::ToolCall { ui_id: "u1".into(), name: "computer".into(), input: json!({ "action": "screenshot" }) })
        );
        let other = json!({ "type": "extension_ui_request", "id": "u2", "method": "confirm", "title": "Sure?" });
        assert_eq!(parse_event(&other.to_string()), Some(Event::Dialog { ui_id: "u2".into() }));
        // fire-and-forget notifications need no answer
        assert_eq!(parse_event(r#"{"type":"extension_ui_request","id":"u3","method":"notify","message":"x"}"#), None);
    }

    #[test]
    fn results_keep_text_and_screens() {
        let blocks = vec![
            json!({ "type": "text", "text": "OK" }),
            json!({ "type": "image", "source": { "type": "base64", "media_type": "image/jpeg", "data": "AAAA" } }),
        ];
        assert_eq!(
            to_pi_content(&blocks),
            vec![json!({ "type": "text", "text": "OK" }), json!({ "type": "image", "data": "AAAA", "mimeType": "image/jpeg" })]
        );
    }

    #[test]
    fn the_extension_carries_tools_and_the_system_prompt() {
        let tools = vec![json!({ "name": "ask_user", "description": "d", "parameters": { "type": "object" } })];
        let src = extension_source(&tools, "Line 1\n\"quoted\"");
        assert!(src.contains(r#""name":"ask_user""#));
        assert!(src.contains(r#"const SYSTEM = "Line 1\n\"quoted\"";"#));
        assert!(src.contains("before_agent_start"));
    }

    #[test]
    fn only_key_based_providers_can_use_pi() {
        assert!(supports(Provider::Anthropic) && supports(Provider::Gemini));
        assert!(!supports(Provider::ClaudeCli) && !supports(Provider::Ollama) && !supports(Provider::Custom));
    }
}
