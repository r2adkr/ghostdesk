//! Runs an agent through pi (a general coding-agent harness) instead of GhostDesk's own loop.
//!
//! pi talks to the AI provider with the user's API key and reaches screens through GhostDesk's own
//! MCP server (`ghostdesk --mcp`, registered in pi's `~/.pi/agent/mcp.json`), so every desk it opens
//! goes through the usual approval flow. Output is pi's JSONL event stream (`--mode json`).

use std::{
    io::Read,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
};

use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::{
    agent::{AgentCmd, AgentConfig, AgentShared, AgentStatus, LogKind},
    llm::Provider,
};

/// One-line installer shown to the user before it runs (it also installs Node.js when missing).
pub fn install_command() -> &'static str {
    if cfg!(windows) {
        "powershell -c \"irm https://pi.dev/install.ps1 | iex\""
    } else {
        "curl -fsSL https://pi.dev/install.sh | sh"
    }
}

/// Runs the installer (blocking). The user confirmed the exact command in Settings first.
pub fn install() -> Result<(), String> {
    let mut c = if cfg!(windows) {
        let mut c = std::process::Command::new("powershell");
        c.args(["-NoProfile", "-Command", "irm https://pi.dev/install.ps1 | iex"]);
        c
    } else {
        let mut c = std::process::Command::new("/bin/sh");
        c.args(["-c", install_command()]);
        c
    };
    let out = c.stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!("설치에 실패했어요: {}", crate::llm::short(err.trim(), 300)))
    }
}

pub fn find() -> Option<PathBuf> {
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

/// Command line (after the executable) for one prompt. The key travels in the environment, never in
/// arguments (visible to other processes).
pub fn build_args(provider: &str, model: &str, session: &str, prompt: &str) -> Vec<String> {
    let mut a: Vec<String> = ["--mode", "json", "--provider", provider, "--model", model, "--session-id", session]
        .into_iter()
        .map(String::from)
        .collect();
    // only GhostDesk's screens: pi's own shell and file tools stay off
    a.extend(["--tools".into(), "mcp__ghostdesk__*".into()]);
    a.push("--".into());
    a.push(prompt.to_string());
    a
}

/// What a line of pi's event stream means to the agent screen.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// A finished assistant message.
    Text(String),
    /// A tool call started.
    Tool { name: String, args: String },
    ToolError { name: String },
    Usage { input: u64, output: u64 },
    /// pi has nothing more to do on its own.
    Settled { aborted: bool },
}

pub fn parse_event(line: &str) -> Option<Event> {
    let v: Value = serde_json::from_str(line).ok()?;
    match v["type"].as_str()? {
        "message_end" => {
            let m = &v["message"];
            if m["role"] != "assistant" {
                return None;
            }
            let usage = &m["usage"];
            let _ = usage;
            let text = match &m["content"] {
                Value::String(s) => s.clone(),
                Value::Array(a) => a.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
                _ => String::new(),
            };
            (!text.trim().is_empty()).then_some(Event::Text(text))
        }
        "turn_end" => {
            let u = &v["message"]["usage"];
            let input = u["input"].as_u64().unwrap_or(0) + u["cacheRead"].as_u64().unwrap_or(0) + u["cacheWrite"].as_u64().unwrap_or(0);
            let output = u["output"].as_u64().unwrap_or(0);
            (input + output > 0).then_some(Event::Usage { input, output })
        }
        "tool_execution_start" => Some(Event::Tool {
            name: v["toolName"].as_str()?.to_string(),
            args: crate::llm::short(&v["args"].to_string(), 80),
        }),
        "tool_execution_end" if v["isError"] == true => Some(Event::ToolError { name: v["toolName"].as_str().unwrap_or("").to_string() }),
        "agent_settled" => Some(Event::Settled { aborted: v["aborted"] == true }),
        _ => None,
    }
}

/// Registers GhostDesk in pi's MCP settings (idempotent).
fn ensure_mcp() -> Result<()> {
    let exe = std::env::current_exe()?;
    crate::install::register(crate::install::Client::Pi, &exe).map(|_| ())
}

/// The agent loop for a pi-backed agent: wait for an instruction, run pi on it, show its events.
pub async fn run(shared: &Arc<AgentShared>, cfg: &AgentConfig, rx: &mut mpsc::UnboundedReceiver<AgentCmd>) -> Result<()> {
    let exe = find().ok_or_else(|| anyhow!("pi 가 설치되어 있지 않습니다. 설정 → AI 에서 설치하세요"))?;
    let (name, env_key) = provider_args(cfg.llm.provider).ok_or_else(|| anyhow!("이 AI 는 pi 로 실행할 수 없습니다"))?;
    ensure_mcp()?;
    let session = format!("ghostdesk-{}-{}", std::process::id(), shared.id);
    let mut next = (!cfg.task.trim().is_empty()).then(|| cfg.task.clone());
    loop {
        let prompt = match next.take() {
            Some(t) => t,
            None => {
                shared.set_status(AgentStatus::Idle("지시를 입력하세요".into()));
                loop {
                    match rx.recv().await {
                        None => return Ok(()),
                        Some(AgentCmd::Instruction(t) | AgentCmd::Answer(t)) => break t,
                        Some(_) => {
                            if let Some(t) = shared.queue.lock().pop_front() {
                                break t;
                            }
                        }
                    }
                }
            }
        };
        shared.log(LogKind::User, prompt.clone());
        shared.stop.store(false, std::sync::atomic::Ordering::SeqCst);
        shared.set_status(AgentStatus::Thinking);
        let mut c = crate::install::cli_command(&exe);
        c.args(build_args(name, &cfg.llm.model, &session, &prompt));
        c.env(env_key, &cfg.llm.api_key);
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = crate::ssh::ProcessGuard(crate::platform::proc::spawn_group(&mut c)?);
        let mut out = child.0.stdout.take().ok_or_else(|| anyhow!("pi 출력을 읽을 수 없습니다"))?;
        let mut err = child.0.stderr.take().ok_or_else(|| anyhow!("pi 오류 출력을 읽을 수 없습니다"))?;
        let (tx, mut lines) = mpsc::unbounded_channel::<String>();
        std::thread::spawn(move || {
            // records end at LF only (Unicode separators are valid inside strings)
            let (mut buf, mut chunk) = (Vec::<u8>::new(), [0u8; 8192]);
            while let Ok(n) = out.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                while let Some(i) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=i).collect();
                    let _ = tx.send(String::from_utf8_lossy(&line).trim_end().to_string());
                }
            }
        });
        let err_text = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = (&mut err).take(64 * 1024).read_to_string(&mut s);
            s
        });
        let mut stopped = false;
        loop {
            tokio::select! {
                line = lines.recv() => {
                    let Some(line) = line else { break };
                    match parse_event(&line) {
                        Some(Event::Text(t)) => shared.log(LogKind::Assistant, t),
                        Some(Event::Tool { name, args }) => {
                            shared.set_status(AgentStatus::Acting(crate::llm::short(&name, 60)));
                            shared.log(LogKind::Action, format!("{name} {args}"));
                        }
                        Some(Event::ToolError { name }) => shared.log(LogKind::Error, format!("{name} 도구 오류")),
                        Some(Event::Usage { input, output }) => {
                            let mut t = shared.tokens.lock();
                            t.0 += input;
                            t.1 += output;
                            crate::usage::global().tokens(&format!("pi · {}", cfg.llm.model), input, output);
                        }
                        Some(Event::Settled { .. }) | None => {}
                    }
                }
                _ = shared.wait_for_stop() => { stopped = true; break; }
            }
        }
        if stopped {
            drop(child); // kills pi's process group
            shared.log(LogKind::Info, "사용자가 중지했습니다. 지시를 입력하면 이어서 진행합니다.");
            continue;
        }
        let status = child.0.wait()?;
        let err_text = err_text.join().unwrap_or_default();
        if !status.success() {
            bail!("pi 가 오류로 끝났습니다: {}", crate::llm::short(err_text.trim(), 400));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_keep_the_key_out_and_only_allow_ghostdesk_tools() {
        let a = build_args("anthropic", "claude-sonnet-5-5", "s1", "-hello");
        assert!(a.windows(2).any(|w| w == ["--tools", "mcp__ghostdesk__*"]));
        assert!(!a.iter().any(|x| x.contains("api-key")));
        // the prompt comes after `--`, so one starting with a dash is still a prompt
        assert_eq!(&a[a.len() - 2..], ["--", "-hello"]);
    }

    #[test]
    fn events_are_understood() {
        assert_eq!(
            parse_event(r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#),
            Some(Event::Text("done".into()))
        );
        assert_eq!(parse_event(r#"{"type":"message_end","message":{"role":"user","content":"x"}}"#), None);
        assert_eq!(
            parse_event(r#"{"type":"tool_execution_start","toolCallId":"1","toolName":"mcp__ghostdesk__desk_open","args":{"mode":"web"}}"#),
            Some(Event::Tool { name: "mcp__ghostdesk__desk_open".into(), args: r#"{"mode":"web"}"#.into() })
        );
        assert_eq!(
            parse_event(r#"{"type":"turn_end","message":{"usage":{"input":10,"output":2,"cacheRead":5,"cacheWrite":0}},"toolResults":[]}"#),
            Some(Event::Usage { input: 15, output: 2 })
        );
        assert_eq!(parse_event(r#"{"type":"agent_settled","aborted":true}"#), Some(Event::Settled { aborted: true }));
        assert_eq!(parse_event("not json"), None);
    }

    #[test]
    fn only_key_based_providers_can_use_pi() {
        assert!(supports(Provider::Anthropic) && supports(Provider::Gemini));
        assert!(!supports(Provider::ClaudeCli) && !supports(Provider::Ollama) && !supports(Provider::Custom));
    }
}
