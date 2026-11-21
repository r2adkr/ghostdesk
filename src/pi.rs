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
        "curl --proto '=https' --tlsv1.2 -fsSL https://pi.dev/install.sh | sh"
    }
}

/// PATH for the installer: an app launched from the Dock has a short one that misses Homebrew and node.
#[cfg(not(windows))]
fn installer_path() -> String {
    let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
    let cur = std::env::var("PATH").unwrap_or_default();
    let own = standalone_node_bin().display().to_string();
    format!("/opt/homebrew/bin:/usr/local/bin:{home}/.local/bin:{cur}:{own}")
}

/// Where the Node.js GhostDesk installs itself (when the computer has none and Homebrew can't provide it).
pub fn standalone_node_bin() -> PathBuf {
    crate::platform::paths::data_root().join("node").join("current").join("bin")
}

/// Downloads Node.js 22 from nodejs.org (checked against its published hash) into GhostDesk's own folder.
#[cfg(not(windows))]
const NODE_STANDALONE: &str = r#"
set -e
case "$(uname -s)" in Darwin) p=darwin;; Linux) p=linux;; *) echo "error: no automatic Node.js for this system"; exit 1;; esac
case "$(uname -m)" in arm64|aarch64) a=arm64;; x86_64|amd64) a=x64;; *) echo "error: no automatic Node.js for this CPU"; exit 1;; esac
base=https://nodejs.org/dist/latest-v22.x
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl --proto '=https' --tlsv1.2 -fsSL "$base/SHASUMS256.txt" -o "$tmp/S"
f=$(awk -v s="-$p-$a.tar.xz" 'index($2,"node-v")==1 && substr($2,length($2)-length(s)+1)==s {print $2; exit}' "$tmp/S")
[ -n "$f" ] || { echo "error: no Node.js build for $p-$a"; exit 1; }
curl --proto '=https' --tlsv1.2 -fsSL "$base/$f" -o "$tmp/$f"
grep " $f\$" "$tmp/S" > "$tmp/one"
if command -v shasum >/dev/null 2>&1; then (cd "$tmp" && shasum -a 256 -c one >/dev/null); else (cd "$tmp" && sha256sum -c one >/dev/null); fi
mkdir -p "$DEST"
rm -rf "$DEST/${f%.tar.xz}"
tar -xf "$tmp/$f" -C "$DEST"
rm -f "$DEST/current"
ln -s "$DEST/${f%.tar.xz}" "$DEST/current"
"#;

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

/// What went wrong, from an installer's output: its progress bars and spinners are dropped, and a line that
/// says "error" wins over the last lines.
fn installer_message(stdout: &[u8], stderr: &[u8]) -> String {
    let text = format!("{}\n{}", String::from_utf8_lossy(stdout), String::from_utf8_lossy(stderr));
    let lines: Vec<&str> = text
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|l| !l.is_empty() && l.chars().any(|c| c.is_alphabetic()))
        .collect();
    let errors: Vec<&str> = lines.iter().copied().filter(|l| l.to_lowercase().contains("error") || l.to_lowercase().contains("fail")).collect();
    let pick = if errors.is_empty() { &lines[lines.len().saturating_sub(3)..] } else { &errors[errors.len().saturating_sub(3)..] };
    if pick.is_empty() { "(no details)".into() } else { pick.join(" / ") }
}

/// Runs the installer (blocking, no terminal): installs Node.js through Homebrew first when it is missing
/// (the official script can only ask about that in a terminal), then pi's own installer.
pub fn install() -> Result<(), String> {
    let fail = |what: &str, out: std::process::Output| format!("{what}: {}", crate::llm::short(&installer_message(&out.stdout, &out.stderr), 300));
    #[cfg(windows)]
    {
        let mut c = std::process::Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", "irm https://pi.dev/install.ps1 | iex"]);
        let out = run_with_timeout(c, UPDATE_TIMEOUT)?;
        return if out.status.success() { Ok(()) } else { Err(fail("설치에 실패했어요", out)) };
    }
    #[cfg(not(windows))]
    {
        let sh = |cmd: &str| std::process::Command::new("/bin/sh").env("PATH", installer_path()).args(["-c", cmd]).stdin(Stdio::null()).output();
        if !node_ok() {
            // Homebrew first (when there is one); if that is missing or fails, a Node.js of our own
            let brewed = sh("command -v brew").is_ok_and(|o| o.status.success()) && sh("brew install node").is_ok_and(|o| o.status.success());
            if !brewed || !node_ok() {
                let dest = crate::platform::paths::data_root().join("node");
                let out = std::process::Command::new("/bin/sh")
                    .env("PATH", installer_path())
                    .env("DEST", &dest)
                    .args(["-c", NODE_STANDALONE])
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|e| e.to_string())?;
                if !out.status.success() {
                    return Err(fail("Node.js 설치에 실패했어요", out));
                }
                if !node_ok() {
                    return Err("Node.js 를 설치했지만 쓸 수 없어요. nodejs.org 에서 22.19 이상을 설치한 뒤 다시 눌러 주세요".into());
                }
            }
        }
        let mut c = std::process::Command::new("/bin/sh");
        c.env("PATH", installer_path()).args(["-c", install_command()]);
        let out = run_with_timeout(c, UPDATE_TIMEOUT)?;
        if !out.status.success() {
            return Err(fail("설치에 실패했어요", out));
        }
        if find().is_none() {
            return Err(format!("설치는 끝났지만 pi 를 찾지 못했어요: {}", crate::llm::short(&installer_message(&out.stdout, &out.stderr), 200)));
        }
        Ok(())
    }
}

pub fn find() -> Option<PathBuf> {
    // lets screenshots and tests show the not-installed state
    if std::env::var_os("GHOSTDESK_NO_PI").is_some() {
        return None;
    }
    crate::install::find_cli("pi")
}

/// Set when the installed pi turns out older than this GhostDesk expects (an option it doesn't know); the
/// window then offers an update. Cleared after a successful update.
pub static OUTDATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn mark_outdated() {
    OUTDATED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// At launch (blocking, so on a background thread): flags an installed pi that lacks an option GhostDesk
/// passes, so the update is offered before an agent runs into it.
pub fn check_outdated() {
    if let Some(exe) = find() {
        if !engine_has_option(&exe, &home_dir(), "--no-mcp") {
            mark_outdated();
        }
    }
}

/// Brings pi up to date: `pi update --self` (pi only, never the person's own pi packages), or the installer
/// when that isn't possible. Blocking; run it off the UI thread. Agents pick the new pi up the next time their
/// engine starts.
pub fn update() -> Result<(), String> {
    // one at a time (a second press, or the toast of another window)
    static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if RUNNING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err("이미 엔진을 업데이트하는 중이에요".into());
    }
    let r = update_inner();
    RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
    r
}

/// Whether the pi now on PATH is new enough (help read afresh).
fn engine_current() -> bool {
    forget_engine_help();
    find().is_some_and(|exe| engine_has_option(&exe, &home_dir(), "--no-mcp"))
}

fn update_inner() -> Result<(), String> {
    let exe = find().ok_or_else(|| "pi 를 찾지 못했어요".to_string())?;
    let mut last_err = String::new();
    // pi's own updater first; the installer when that is missing (older pi) or didn't help. Each is tried twice:
    // on Windows a running agent's engine can hold a file for a moment (EBUSY), and a network hiccup passes.
    for attempt in 0..2 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        let mut c = engine_command(&exe, &home_dir());
        c.args(["update", "--self"]);
        match run_with_timeout(c, UPDATE_TIMEOUT) {
            Ok(o) if o.status.success() && engine_current() => return updated(),
            Ok(o) if !o.status.success() => last_err = installer_message(&o.stdout, &o.stderr),
            Ok(_) => {}
            Err(e) => last_err = e,
        }
        match install() {
            Ok(()) if engine_current() => return updated(),
            Ok(()) => {
                last_err = format!("설치는 끝났지만 {} 의 pi 가 여전히 이전 버전이에요 (다른 곳에 설치된 pi 가 먼저 쓰이고 있을 수 있어요)", exe.display())
            }
            Err(e) => last_err = e,
        }
    }
    Err(format!("엔진 업데이트에 실패했어요: {}", crate::llm::short(&last_err, 300)))
}

fn updated() -> Result<(), String> {
    OUTDATED.store(false, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// An update that takes longer than this is stuck (a prompt nobody can answer, a dead download).
const UPDATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Runs a command to the end, killing it after `limit`. Output is read on threads so a full pipe can't block it.
fn run_with_timeout(mut c: std::process::Command, limit: std::time::Duration) -> Result<std::process::Output, String> {
    use std::io::Read;
    let mut child = c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let read = |p: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut v = vec![];
            if let Some(mut p) = p {
                let _ = p.read_to_end(&mut v);
            }
            v
        })
    };
    let out = read(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = read(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            break st;
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{}분 넘게 끝나지 않아 중단했어요", limit.as_secs() / 60));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    Ok(std::process::Output { status, stdout: out.join().unwrap_or_default(), stderr: err.join().unwrap_or_default() })
}

/// How pi reaches an agent's AI: a provider it knows by name, or one GhostDesk describes to it (a custom
/// address, a local server such as Ollama, a proxy) in its private `models.json`.
#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    pub provider: String,
    pub env_key: &'static str,
    /// `(base URL, API style)` of a provider GhostDesk defines itself.
    pub custom: Option<(String, &'static str)>,
}

/// The variable that carries the key of a GhostDesk-defined provider.
const CUSTOM_KEY: &str = "GHOSTDESK_API_KEY";

fn builtin(p: Provider) -> Option<(&'static str, &'static str)> {
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

/// Name of a GhostDesk-defined provider: stable for one address and API style.
fn custom_name(base_url: &str, api: &str) -> String {
    let mut h = crate::install::Sha256::new();
    h.update(format!("{api}\n{base_url}").as_bytes());
    format!("gd-{}", &h.finish_hex()[..10])
}

/// Where pi should send this AI's requests (`None`: not through pi, e.g. the CLI logins).
pub fn route(llm: &Llm) -> Option<Route> {
    if llm.provider.is_cli() {
        return None;
    }
    let default = llm.provider.default_base_url();
    let url = llm.base_url.trim().trim_end_matches('/').to_string();
    if let Some((name, env_key)) = builtin(llm.provider) {
        if url.is_empty() || url == default.trim_end_matches('/') {
            return Some(Route { provider: name.into(), env_key, custom: None });
        }
    }
    if url.is_empty() {
        return None;
    }
    let api = if llm.provider == Provider::Anthropic { "anthropic-messages" } else { "openai-completions" };
    Some(Route { provider: custom_name(&url, api), env_key: CUSTOM_KEY, custom: Some((url, api)) })
}

/// Whether an agent with this AI can run through pi.
pub fn supports(p: Provider) -> bool {
    !p.is_cli()
}

/// The key to hand pi (a placeholder for servers that need none).
fn key_for(llm: &Llm) -> String {
    if llm.api_key.is_empty() { "none".into() } else { llm.api_key.clone() }
}

/// pi is installed and this AI can use it.
pub fn usable(llm: &Llm) -> bool {
    route(llm).is_some() && (!llm.api_key.is_empty() || !llm.provider.needs_key()) && find().is_some()
}

/// Why an agent with a key-based AI can't use pi although pi is installed (None: it can, or it isn't a
/// case worth mentioning — no pi, or an AI that never goes through pi).
pub fn unavailable(llm: &Llm) -> Option<&'static str> {
    if find().is_none() || !supports(llm.provider) {
        return None;
    }
    if llm.provider.needs_key() && llm.api_key.is_empty() {
        return Some("API 키가 없어 기본 방식으로 실행해요");
    }
    if route(llm).is_none() {
        return Some("주소가 없어 기본 방식으로 실행해요");
    }
    None
}

/// Marks an extension dialog as a tool call (the title is `MARK` + JSON).
const MARK: &str = "\u{1}gd\u{1}";

/// Marks an extension dialog as a request for the person's approval (`ASK` + JSON `{ tool, what }`).
const ASK: &str = "\u{1}ask\u{1}";

/// pi's own file and command tools an agent may use at a level (0 off, 1 read, 2 read/write/run).
pub fn file_tools(level: u8) -> &'static [&'static str] {
    match level {
        0 => &[],
        1 => &["read", "grep", "find", "ls"],
        _ if cfg!(windows) => &["read", "grep", "find", "ls", "write", "edit", "powershell"],
        _ => &["read", "grep", "find", "ls", "write", "edit", "bash"],
    }
}

/// Short description of the skill in a folder (for searching).
fn skill_desc(dir: &Path) -> String {
    let text = std::fs::read_to_string(dir.join("SKILL.md")).unwrap_or_default();
    crate::llm::short(&crate::skillbox::frontmatter(&text).1.unwrap_or_default(), 300)
}

/// Above this many skills the system prompt only counts them; the agent searches with read_skill.
const SKILLS_LISTED: usize = 12;

/// The pi extension: registers GhostDesk's tools, forwards each call to GhostDesk and sets the system
/// prompt. `tools` is a list of `{ name, description, parameters }`. With `files` (the agent's own folder),
/// pi's file tools work freely inside that folder; anything outside it, and every command, is sent to the
/// person for approval first.
pub fn extension_source(tools: &[Value], system: &str, files: Option<&Path>, mcp: bool, skills: &[(String, PathBuf)], plugins: bool) -> String {
    let mcp = mcp || plugins;
    let (top, hook) = match (files, mcp) {
        (Some(dir), _) => (
            format!(
                r#"
const WORK = {work};
const ASK = "\u0001ask\u0001";
import {{ resolve, relative, isAbsolute, dirname, basename }} from "node:path";
import {{ realpathSync }} from "node:fs";
const real = (p) => {{
  const r = resolve(WORK, String(p ?? "."));
  let t = r, tail = "";
  for (;;) {{
    try {{ return tail ? resolve(realpathSync(t), tail) : realpathSync(t); }} catch (e) {{}}
    const up = dirname(t);
    if (up === t) return r;
    tail = tail ? resolve(basename(t), tail) : basename(t);
    t = up;
  }}
}};
const norm = (p) => (process.platform === "win32" ? p.toLowerCase() : p);
const inside = (p) => {{ const rel = relative(norm(realpathSync(WORK)), norm(real(p))); return rel === "" || (!rel.startsWith("..") && !isAbsolute(rel)); }};
async function allowed(ctx, tool, what) {{
  const v = await ctx.ui.input(ASK + JSON.stringify({{ tool, what }}), "");
  return v === "yes";
}}
const DENIED = {{ block: true, reason: "The human operator did not allow this. Do not retry it; continue without it or ask the human." }};
// a search pattern or glob that climbs out of the folder (or starts at the root) reaches files `path` does not show
const climbs = (v) => typeof v === "string" && (isAbsolute(v) || v.startsWith("~") || /(^|[\\/])\.\.([\\/]|$)/.test(v));
"#,
                work = Value::String(dir.display().to_string()),
            ),
            r#"
  const ours = new Set([...TOOLS.map((t) => t.name), "read_skill"]);
  const okMcp = new Set();
  const plug = new Set();
  pi.on("tool_call", async (event, ctx) => {
    const n = event.toolName;
    if (ours.has(n)) return;
    if (n.startsWith("mcp__") || n === "read_mcp_resource" || plug.has(n)) {
      if (okMcp.has(n)) return;
      const what = n + " " + JSON.stringify(event.input ?? {}).slice(0, 300);
      if (!(await allowed(ctx, "mcp", what))) return DENIED;
      okMcp.add(n);
      return;
    }
    if (n === "bash" || n === "powershell") return (await allowed(ctx, n, String(event.input.command ?? ""))) ? undefined : DENIED;
    const p = event.input.path ?? event.input.file_path ?? ".";
    const reach = climbs(event.input.pattern) || climbs(event.input.glob);
    if (inside(p) && !reach) return;
    return (await allowed(ctx, n, reach ? real(p) + " (" + (event.input.pattern ?? event.input.glob) + ")" : real(p))) ? undefined : DENIED;
  });"#
                .to_string(),
        ),
        (None, true) => (
            r#"
const ASK = "\u0001ask\u0001";
async function allowed(ctx, tool, what) {
  const v = await ctx.ui.input(ASK + JSON.stringify({ tool, what }), "");
  return v === "yes";
}
const DENIED = { block: true, reason: "The human operator did not allow this. Do not retry it; continue without it or ask the human." };
"#
            .to_string(),
            r#"
  const ours = new Set([...TOOLS.map((t) => t.name), "read_skill"]);
  const okMcp = new Set();
  const plug = new Set();
  pi.on("tool_call", async (event, ctx) => {
    const n = event.toolName;
    if (ours.has(n) || !(n.startsWith("mcp__") || n === "read_mcp_resource" || plug.has(n))) return;
    if (okMcp.has(n)) return;
    const what = n + " " + JSON.stringify(event.input ?? {}).slice(0, 300);
    if (!(await allowed(ctx, "mcp", what))) return DENIED;
    okMcp.add(n);
  });"#
                .to_string(),
        ),
        (None, false) => (String::new(), String::new()),
    };
    let skill_tool = if skills.is_empty() {
        String::new()
    } else {
        let list: Vec<Value> = skills.iter().map(|(n, d)| json!({ "name": n, "dir": d.display().to_string(), "desc": skill_desc(d) })).collect();
        format!(
            r#"
  const SKILLS = {list};
  pi.registerTool({{
    name: "read_skill", label: "read_skill",
    description: "Read a skill's instructions (SKILL.md) or a file that belongs to it. Skills are listed in the system prompt.",
    parameters: {{ type: "object", properties: {{ name: {{ type: "string", description: "Skill name" }}, file: {{ type: "string", description: "File inside the skill folder; default SKILL.md" }}, query: {{ type: "string", description: "Instead of a name: words to search skill names and descriptions" }} }} }},
    async execute(id, params) {{
      if (!params.name) {{
        const q = String(params.query ?? "").toLowerCase().split(/\s+/).filter(Boolean);
        const hits = SKILLS.filter((s) => !q.length || q.some((w) => (s.name + " " + s.desc).toLowerCase().includes(w))).slice(0, 15);
        const text = hits.length ? hits.map((s) => "- " + s.name + ": " + s.desc).join("\n") : "No skill matches.";
        return {{ content: [{{ type: "text", text }}], details: {{}} }};
      }}
      const sk = SKILLS.find((s) => s.name === params.name);
      if (!sk) throw new Error("No such skill.");
      const root = rps(sk.dir);
      const target = rps(res(root, String(params.file ?? "SKILL.md")));
      const r = rel2(root, target);
      if (r.startsWith("..") || isabs(r)) throw new Error("Outside the skill folder.");
      const text = rfs(target, "utf8");
      return {{ content: [{{ type: "text", text: text.length > 40000 ? text.slice(0, 40000) + "\n…(truncated)" : text }}], details: {{}} }};
    }},
  }});"#,
            list = Value::Array(list)
        )
    };
    let skill_import = if skills.is_empty() {
        ""
    } else {
        "
import { readFileSync as rfs, realpathSync as rps } from \"node:fs\";
import { resolve as res, relative as rel2, isAbsolute as isabs } from \"node:path\";"
    };
    let plugin_tools = if plugins {
        r#"
  pi.on("session_start", async () => {
    const own = new Set([...TOOLS.map((t) => t.name), "read_skill"]);
    const extra = pi.getAllTools().filter((t) => t.sourceInfo && t.sourceInfo.source !== "builtin" && t.sourceInfo.source !== "sdk" && !own.has(t.name) && !String(t.name).startsWith("mcp__")).map((t) => t.name);
    for (const n of extra) plug.add(n);
  });"#
    } else {
        ""
    };
    format!(
        r#"const TOOLS = {tools};
const SYSTEM = {system};
const MARK = "\u0001gd\u0001";{skill_import}{top}
export default function (pi) {{
  pi.on("before_agent_start", async () => ({{ systemPrompt: SYSTEM }}));{hook}{skill_tool}{plugin_tools}
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
        top = top,
        hook = hook,
        skill_tool = skill_tool,
        plugin_tools = plugin_tools,
        skill_import = skill_import,
    )
}

/// The tools in pi's shape, from the Claude-format list the built-in loop uses.
pub fn tool_defs(tools: &Value) -> Vec<Value> {
    crate::llm::cli_tools(tools)
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
    /// pi's own file or command tool wants the person's approval.
    Ask { ui_id: String, tool: String, what: String },
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
            if let Some(ask) = v["title"].as_str().and_then(|t| t.strip_prefix(ASK)) {
                let a: Value = serde_json::from_str(ask).ok()?;
                return Some(Event::Ask { ui_id, tool: a["tool"].as_str()?.to_string(), what: a["what"].as_str().unwrap_or_default().to_string() });
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
pub fn build_args(provider: &str, model: &str, extension: &Path, effort: &str, tools: &[String], files: u8, mcp: &[crate::toolbox::Server], skills: &[PathBuf], plugins: &[String]) -> Vec<String> {
    let mut a: Vec<String> = ["--mode", "rpc", "--no-session", "--provider", provider, "--model", model]
        .into_iter()
        .map(String::from)
        .collect();
    // nothing of pi's own beyond the chosen file tools: no MCP servers, no discovered extensions, skills,
    // templates, themes or project context files
    a.extend(["--no-extensions", "--no-skills", "--no-prompt-templates", "--no-themes", "--no-context-files"].map(String::from));
    if mcp.is_empty() {
        a.push("--no-mcp".into());
    } else {
        // only the MCP support itself and tool search (tools are found on demand); never codemode scripts
        a.extend(["-e", "builtin:mcp", "-e", "builtin:tool-search"].map(String::from));
    }
    // never codemode scripts
    let mut exclude: Vec<&str> = vec!["codemode"];
    if !plugins.is_empty() {
        // a plugin's tools are not known in advance, so no allowlist: the built-in tools this agent may not
        // have are switched off instead
        let allowed = file_tools(files);
        exclude.extend(["read", "grep", "find", "ls", "write", "edit", "bash", "powershell"].into_iter().filter(|t| !allowed.contains(t)));
        a.push("--exclude-tools".into());
        a.push(exclude.join(","));
        // by default pi has only read, bash, edit and write: the search tools are switched on explicitly
        let mut plus: Vec<&str> = allowed.iter().copied().filter(|t| matches!(*t, "grep" | "find" | "ls")).collect();
        if !mcp.is_empty() {
            plus.push("tool_search");
        }
        if !plus.is_empty() {
            a.push("--tools".into());
            a.push(plus.iter().map(|t| format!("+{t}")).collect::<Vec<_>>().join(","));
        }
    } else if !mcp.is_empty() {
        a.push("--exclude-tools".into());
        a.push("codemode".into());
    }
    if !plugins.is_empty() {
    } else if files == 0 && mcp.is_empty() {
        a.push("--no-builtin-tools".into());
    } else {
        // the allowlist names every tool the model gets: GhostDesk's own, the file tools of this level and
        // the tool box (found through tool search; a server's exposure decides what is shown up front)
        let mut all: Vec<String> = tools.to_vec();
        all.extend(file_tools(files).iter().map(|t| t.to_string()));
        if !skills.is_empty() {
            all.push("read_skill".into());
        }
        if !mcp.is_empty() {
            all.push("tool_search".into());
            all.extend(mcp.iter().map(|s| format!("{}*", s.tool_prefix())));
        }
        a.push("--tools".into());
        a.push(all.join(","));
    }
    a.push("-e".into());
    a.push(extension.display().to_string());
    for p in plugins {
        a.push("-e".into());
        a.push(p.clone());
    }
    if let Some(level) = thinking_level(effort) {
        a.push("--thinking".into());
        a.push(level.into());
    }
    a
}

/// A pi command in the private config folder (no key, no session).
pub fn engine_command(exe: &Path, home: &Path) -> std::process::Command {
    let mut c = crate::install::cli_command(exe);
    c.env("PI_CODING_AGENT_DIR", home).env("PI_SKIP_VERSION_CHECK", "1").env("PI_TELEMETRY", "0").stdin(Stdio::null());
    c
}

/// Whether the installed pi lists `flag` in its `--help` (asked once per executable, then remembered).
/// If the help can't be read, the flag is assumed supported, as before.
static ENGINE_HELP: parking_lot::Mutex<Option<std::collections::HashMap<PathBuf, Option<String>>>> = parking_lot::Mutex::new(None);

/// After an update the old help text no longer describes the installed pi.
fn forget_engine_help() {
    *ENGINE_HELP.lock() = None;
}

fn engine_has_option(exe: &Path, home: &Path, flag: &str) -> bool {
    let mut cache = ENGINE_HELP.lock();
    let help = cache.get_or_insert_with(Default::default).entry(exe.to_path_buf()).or_insert_with(|| {
        let mut c = engine_command(exe, home);
        c.arg("--help").stdin(Stdio::null()).stderr(Stdio::null());
        c.output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    });
    help.as_deref().is_none_or(|h| help_lists(h, flag))
}

/// `flag` appears in pi's help as an option of its own (not as the start of a longer one).
fn help_lists(help: &str, flag: &str) -> bool {
    help.match_indices(flag).any(|(i, _)| !help[i + flag.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-'))
}

/// pi stopped because it doesn't know an option GhostDesk passes: an older pi than this GhostDesk expects.
fn outdated_engine(stderr: &str) -> bool {
    stderr.contains("Unknown option")
}

fn base_command(exe: &Path, home: &Path, cwd: &Path, env_key: &str, api_key: &str) -> std::process::Command {
    let mut c = engine_command(exe, home);
    c.current_dir(cwd);
    c.env(env_key, api_key);
    c
}

static MODELS_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Tells pi about this agent's model in its private `models.json`: a GhostDesk-defined provider with its
/// address, or a model id pi's catalog doesn't list yet. Entries of other agents are kept.
fn register_model(home: &Path, route: &Route, llm: &Llm, listed: bool) -> Result<()> {
    if listed && route.custom.is_none() {
        return Ok(());
    }
    let _g = MODELS_LOCK.lock();
    let path = home.join("models.json");
    let mut root: Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}));
    if !root.is_object() {
        root = json!({});
    }
    let providers = root.as_object_mut().expect("object").entry("providers").or_insert_with(|| json!({}));
    if !providers.is_object() {
        *providers = json!({});
    }
    let entry = providers.as_object_mut().expect("object").entry(route.provider.clone()).or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    let e = entry.as_object_mut().expect("object");
    if let Some((url, api)) = &route.custom {
        e.insert("baseUrl".into(), json!(url));
        e.insert("api".into(), json!(api));
        e.insert("apiKey".into(), json!(format!("${CUSTOM_KEY}")));
    }
    let models = e.entry("models").or_insert_with(|| json!([]));
    if !models.is_array() {
        *models = json!([]);
    }
    let list = models.as_array_mut().expect("array");
    if !list.iter().any(|m| m["id"] == llm.model.as_str()) {
        // screenshots go to the model, so it is declared as taking images
        list.push(json!({ "id": llm.model, "input": ["text", "image"] }));
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&root)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Whether pi's model list has this model for this provider.
async fn model_known(exe: &Path, home: &Path, provider: &str, env_key: &str, llm: &Llm) -> bool {
    let mut c = base_command(exe, home, home, env_key, &key_for(llm));
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

/// A file removed when dropped (also when the agent is cancelled mid-run).
struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
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

/// The agent loop on pi. Returns `Ok(Some(reason))` (before touching anything) when pi can't serve this
/// agent, so the caller runs its own loop instead.
pub async fn run(job: Job<'_>, rx: &mut mpsc::UnboundedReceiver<AgentCmd>) -> Result<Option<&'static str>> {
    let Job { shared, cfg, ws, system, tools, membership } = job;
    let (Some(exe), Some(route)) = (find(), route(&cfg.llm)) else { return Ok(None) };
    let home = home_dir();
    std::fs::create_dir_all(&home)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
    }
    // a model pi's catalog doesn't list (new, local, custom address) is described to it instead of refused
    let listed = route.custom.is_none() && model_known(&exe, &home, &route.provider, route.env_key, &cfg.llm).await;
    if let Err(e) = register_model(&home, &route, &cfg.llm, listed) {
        shared.log(LogKind::Error, format!("모델 정보를 저장하지 못했어요: {e}"));
        return Ok(Some("이 모델을 고급 엔진에 알리지 못해서 기본 방식으로 실행해요"));
    }
    // per process and agent, so two GhostDesk windows never share (or delete) each other's file
    let extension = TempFile(home.join(format!("bridge-{}-{}.mjs", std::process::id(), shared.id)));
    // the agent's own folder for files (pi's file tools are offered only where paths can be confined to it)
    let work = (cfg.files > 0).then(|| crate::profiles::workspaces_root().join(crate::profiles::slug(&cfg.workspace)).join("files"));
    if let Some(dir) = &work {
        std::fs::create_dir_all(dir)?;
    }
    // the tool box: switched-on servers (the engine reads them from its private folder)
    let mcp = crate::toolbox::active();
    let plugins = crate::pluginbox::active();
    let skills: Vec<crate::skillbox::Skill> = crate::skillbox::list().into_iter().filter(|s| s.enabled).collect();
    let mut system = system.to_string();
    if !mcp.is_empty() {
        system.push_str("\n\nTool box: extra tools from connected services are available. Use tool_search to find them by what you need; each tool is shown to the human for approval the first time you use it. If they refuse, do not retry.");
    }
    if !skills.is_empty() {
        if skills.len() <= SKILLS_LISTED {
            system.push_str("\n\nSkills (instructions for particular kinds of work). When a task matches one, read it first with read_skill before acting:");
            for k in &skills {
                system.push_str(&format!("\n- {}: {}", k.name, crate::llm::short(&k.description, 300)));
            }
        } else {
            system.push_str(&format!("\n\nSkills: {} skills (instructions for particular kinds of work) are available. Before acting on a task, search them with read_skill using query (words about the task), then read a match with read_skill name.", skills.len()));
        }
    }
    if let Some(dir) = &work {
        system.push_str(&format!(
            "\n\nFiles: you also have file tools (read, grep, find, ls{}). Your work folder is {}; keep files you create there. Anything outside it, and every command, is shown to the human for approval first; if they refuse, do not retry — continue without it or ask.",
            if cfg.files >= 2 { if cfg!(windows) { ", write, edit, powershell" } else { ", write, edit, bash" } } else { "" },
            dir.display()
        ));
    }
    std::fs::write(&extension.0, extension_source(&tool_defs(tools), &system, work.as_deref(), !mcp.is_empty(), &skills.iter().map(|k| (k.name.clone(), k.path.clone())).collect::<Vec<_>>(), !plugins.is_empty()))?;
    let names: Vec<String> = tool_defs(tools).iter().filter_map(|t| t["name"].as_str().map(String::from)).collect();
    let cwd = work.clone().unwrap_or_else(|| home.clone());
    let spec = Spec { extension: &extension.0, tools: names, cwd, mcp, skills: skills.iter().map(|k| k.path.clone()).collect(), plugins };
    drive_agent(shared, cfg, ws, &exe, &home, &spec, &route, membership, rx).await.map(|_| None)
}

/// How to start the process: the extension, the names of GhostDesk's tools and the working folder.
struct Spec<'a> {
    extension: &'a Path,
    tools: Vec<String>,
    cwd: PathBuf,
    mcp: Vec<crate::toolbox::Server>,
    skills: Vec<PathBuf>,
    plugins: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
async fn drive_agent(
    shared: &Arc<AgentShared>,
    cfg: &AgentConfig,
    ws: &Desk,
    exe: &Path,
    home: &Path,
    spec: &Spec<'_>,
    route: &Route,
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
            let mut c = base_command(exe, home, &spec.cwd, route.env_key, &key_for(&cfg.llm));
            let mut args = build_args(&route.provider, &cfg.llm.model, spec.extension, &cfg.llm.effort, &spec.tools, cfg.files, &spec.mcp, &spec.skills, &spec.plugins);
            // pi releases without MCP support have nothing to switch off and reject the flag outright
            // (runs pi --help once: a blocking call, kept off the async worker)
            if !tokio::task::block_in_place(|| engine_has_option(exe, home, "--no-mcp")) {
                // it still works without the flag, but it is an older pi: offer the update
                args.retain(|a| a != "--no-mcp");
                mark_outdated();
            }
            c.args(args);
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
            if outdated_engine(&why) {
                mark_outdated();
                shared.log(LogKind::Info, "GhostDesk에서 사용하는 기능이 현재 설치되어 있는 AI 엔진(pi)의 버전에서 지원되지 않아요. 오른쪽 아래 알림에서 '엔진 업데이트'를 누른 뒤 다시 지시해 주세요.");
            }
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
                    Some(Event::Ask { ui_id, tool, what }) => {
                        let (title, kind) = match tool.as_str() {
                            "bash" | "powershell" => ("에이전트가 명령을 실행하려고 해요", "실행할 명령"),
                            "mcp" => ("에이전트가 연결된 도구를 쓰려고 해요", "도구"),
                            "write" | "edit" => ("에이전트가 내 컴퓨터의 파일을 바꾸려고 해요", "파일"),
                            _ => ("에이전트가 내 컴퓨터의 파일을 읽으려고 해요", "파일"),
                        };
                        // the person's permission settings decide first; only "ask" shows the window
                        let allowed = match crate::access::decide(crate::access::kind_of(&tool), crate::control::shell_tools_allowed()) {
                            crate::access::Rule::Allow => {
                                shared.log(LogKind::Info, format!("설정에 따라 허용: {kind} {}", crate::llm::short(&what, 200)));
                                true
                            }
                            crate::access::Rule::Deny => {
                                shared.log(LogKind::Info, format!("설정에 따라 막음: {kind} {}", crate::llm::short(&what, 200)));
                                false
                            }
                            crate::access::Rule::Ask => {
                                shared.log(LogKind::Info, format!("허락을 기다리는 중: {kind} {}", crate::llm::short(&what, 200)));
                                tokio::select! {
                                    a = crate::control::ask_person(title, format!("{}\n\n{kind}: {what}", shared.name)) => a,
                                    _ = shared.wait_for_stop() => false,
                                }
                            }
                        };
                        p.send(json!({ "type": "extension_ui_response", "id": ui_id, "value": if allowed { "yes" } else { "no" } }));
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
mod option_probe_tests {
    use super::*;

    #[test]
    fn help_lists_only_whole_options() {
        let old = "  --no-session   Don't save
  --no-extensions, -ne  Disable
";
        assert!(help_lists(old, "--no-session") && help_lists(old, "--no-extensions"));
        assert!(!help_lists(old, "--no-mcp"));
        assert!(!help_lists("  --no-mcp-servers  x", "--no-mcp"), "a longer option is not the same one");
        assert!(help_lists("  --no-mcp, -nm  x", "--no-mcp"));
        assert!(outdated_engine("Error: Unknown option: --no-mcp") && !outdated_engine("rate limited"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_isolate_pi_and_keep_the_key_out() {
        let a = build_args("anthropic", "claude-sonnet-5-5", Path::new("/x/bridge.mjs"), "high", &[], 0, &[], &[], &[]);
        for f in ["--no-builtin-tools", "--no-mcp", "--no-extensions", "--no-skills", "--no-context-files", "--no-session"] {
            assert!(a.iter().any(|x| x == f), "{f}");
        }
        assert!(a.windows(2).any(|w| w == ["-e", "/x/bridge.mjs"]));
        assert!(a.windows(2).any(|w| w == ["--thinking", "high"]));
        assert!(!a.iter().any(|x| x.contains("api-key")));
        assert!(!build_args("openai", "m", Path::new("e"), "weird", &[], 0, &[], &[], &[]).iter().any(|x| x == "--thinking"));
    }

    #[test]
    fn installer_errors_skip_progress_bars() {
        let err = b"######## 100.0%\r##### 20%\n";
        let out = b"checking node\nerror: Pi requires Node.js 22.19.0 or newer. Found v18.\n";
        assert_eq!(installer_message(out, err), "error: Pi requires Node.js 22.19.0 or newer. Found v18.");
        assert_eq!(installer_message(b"", b"###\r##"), "(no details)");
        assert_eq!(installer_message(b"a\nb\nc\nd\n", b""), "b / c / d");
    }

    #[test]
    fn extension_with_skills_is_valid_javascript() {
        let dir = std::env::temp_dir().join(format!("gd-ext-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (i, files) in [None, Some(dir.as_path())].into_iter().enumerate() {
            let src = extension_source(&[], "s", files, true, &[("demo".into(), dir.clone())], true);
            let f = dir.join(format!("ext{i}.mjs"));
            std::fs::write(&f, src).unwrap();
            // skipped where node is missing
            if let Ok(o) = std::process::Command::new("node").arg("--check").arg(&f).output() {
                assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_box_adds_mcp_and_tool_search_but_never_codemode() {
        let srv = crate::toolbox::Server { name: "my-fs".into(), command: "x".into(), args: vec![], url: String::new(), env: vec![], enabled: true, exposure: "deferred".into() };
        let tools = vec!["computer".to_string()];
        let a = build_args("openai", "m", Path::new("e"), "high", &tools, 0, &[srv.clone()], &[PathBuf::from("/s/a")], &["npm:x@1.0.0".to_string()]);
        assert!(!a.iter().any(|x| x == "--no-mcp" || x == "--no-builtin-tools"));
        assert!(a.windows(2).any(|w| w == ["-e", "builtin:mcp"]));
        assert!(a.windows(2).any(|w| w[0] == "--exclude-tools" && w[1].starts_with("codemode")));
        let nopl = build_args("openai", "m", Path::new("e"), "high", &tools, 0, &[srv.clone()], &[], &[]);
        assert!(nopl.windows(2).any(|w| w == ["--exclude-tools", "codemode"]));
        let pl = build_args("openai", "m", Path::new("e"), "high", &tools, 1, &[], &[], &["npm:x@1.0.0".to_string()]);
        assert!(!pl.iter().any(|x| x == "--no-builtin-tools"));
        assert!(pl.windows(2).any(|w| w == ["--tools", "+grep,+find,+ls"]));
        assert!(pl.windows(2).any(|w| w == ["--exclude-tools", "codemode,write,edit,bash,powershell"]));
        assert!(a.windows(2).any(|w| w == ["-e", "npm:x@1.0.0"]));
        assert!(extension_source(&[], "s", None, true, &[], false).contains("mcp__"));
        assert!(extension_source(&[], "s", None, false, &[], true).contains("session_start"));
        let sk = build_args("openai", "m", Path::new("e"), "high", &tools, 0, &[srv.clone()], &[PathBuf::from("/s/a")], &[]);
        assert!(sk.windows(2).any(|w| w == ["--tools", "computer,read_skill,tool_search,mcp__my_fs__*"]));
        assert!(extension_source(&[], "s", None, false, &[("demo".into(), PathBuf::from("/s/demo"))], false).contains("read_skill"));
    }

    #[test]
    fn file_tools_are_allowed_by_level_and_ask_before_leaving_the_folder() {
        let tools = vec!["computer".to_string(), "ask_user".to_string()];
        let a = build_args("openai", "m", Path::new("e"), "high", &tools, 1, &[], &[], &[]);
        assert!(!a.iter().any(|x| x == "--no-builtin-tools"));
        assert!(a.windows(2).any(|w| w == ["--tools", "computer,ask_user,read,grep,find,ls"]));
        let b = build_args("openai", "m", Path::new("e"), "high", &tools, 2, &[], &[], &[]);
        assert!(b.iter().any(|x| x.ends_with(if cfg!(windows) { "write,edit,powershell" } else { "write,edit,bash" })));
        let src = extension_source(&[], "s", Some(Path::new("/work/files")), false, &[], false);
        assert!(src.contains(r#"const WORK = "/work/files";"#) && src.contains("tool_call") && src.contains("DENIED"));
        assert!(!extension_source(&[], "s", None, false, &[], false).contains("tool_call"));
        let ask = json!({ "type": "extension_ui_request", "id": "a1", "method": "input", "title": format!("{ASK}{}", json!({ "tool": "bash", "what": "ls" })) });
        assert_eq!(parse_event(&ask.to_string()), Some(Event::Ask { ui_id: "a1".into(), tool: "bash".into(), what: "ls".into() }));
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
        let src = extension_source(&tools, "Line 1\n\"quoted\"", None, false, &[], false);
        assert!(src.contains(r#""name":"ask_user""#));
        assert!(src.contains(r#"const SYSTEM = "Line 1\n\"quoted\"";"#));
        assert!(src.contains("before_agent_start"));
    }

    fn llm(p: Provider, url: &str) -> Llm {
        Llm::new(p, url.into(), "k".into(), "m1".into(), "medium".into())
    }

    #[test]
    fn every_api_ai_has_a_route_but_the_cli_logins_do_not() {
        assert!(supports(Provider::Anthropic) && supports(Provider::Ollama) && supports(Provider::Custom));
        assert!(!supports(Provider::ClaudeCli) && !supports(Provider::CodexCli));
        let r = route(&llm(Provider::OpenAi, &Provider::OpenAi.default_base_url())).unwrap();
        assert_eq!((r.provider.as_str(), r.env_key, r.custom.is_none()), ("openai", "OPENAI_API_KEY", true));
        // a changed address, a local server and a custom API become providers GhostDesk describes
        let proxy = route(&llm(Provider::OpenAi, "https://proxy.example.com/v1/")).unwrap();
        assert_eq!(proxy.custom, Some(("https://proxy.example.com/v1".to_string(), "openai-completions")));
        assert!(proxy.provider.starts_with("gd-") && proxy.env_key == "GHOSTDESK_API_KEY");
        assert_eq!(route(&llm(Provider::Anthropic, "https://gw.example.com")).unwrap().custom.unwrap().1, "anthropic-messages");
        assert!(route(&llm(Provider::Ollama, "http://localhost:11434/v1")).unwrap().custom.is_some());
        assert!(route(&llm(Provider::Custom, "")).is_none());
        // one address, one provider name
        assert_eq!(route(&llm(Provider::Custom, "http://a/v1")), route(&llm(Provider::Custom, "http://a/v1/")));
        assert_ne!(route(&llm(Provider::Custom, "http://a/v1")), route(&llm(Provider::Custom, "http://b/v1")));
    }

    #[test]
    fn unlisted_and_custom_models_are_registered_without_touching_other_entries() {
        let home = std::env::temp_dir().join(format!("gd-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("models.json"), r#"{"providers":{"other":{"models":[{"id":"x"}]}}}"#).unwrap();
        let local = llm(Provider::Ollama, "http://localhost:11434/v1");
        let r = route(&local).unwrap();
        register_model(&home, &r, &local, false).unwrap();
        register_model(&home, &r, &local, false).unwrap();
        let known = llm(Provider::OpenAi, &Provider::OpenAi.default_base_url());
        let rk = route(&known).unwrap();
        register_model(&home, &rk, &known, true).unwrap(); // listed: nothing to add
        let v: Value = serde_json::from_str(&std::fs::read_to_string(home.join("models.json")).unwrap()).unwrap();
        assert_eq!(v["providers"]["other"]["models"][0]["id"], "x");
        let e = &v["providers"][r.provider.as_str()];
        assert_eq!(e["models"].as_array().unwrap().len(), 1);
        assert_eq!((e["api"].as_str(), e["apiKey"].as_str()), (Some("openai-completions"), Some("$GHOSTDESK_API_KEY")));
        assert!(v["providers"].get("openai").is_none());
        let _ = std::fs::remove_dir_all(&home);
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;

    #[test]
    fn a_stuck_command_is_stopped_and_a_quick_one_keeps_its_output() {
        let (quick, slow) = if cfg!(windows) {
            let mut q = std::process::Command::new("cmd");
            q.args(["/C", "echo hi"]);
            let mut s = std::process::Command::new("powershell");
            s.args(["-NoProfile", "-Command", "Start-Sleep 30"]);
            (q, s)
        } else {
            let mut q = std::process::Command::new("/bin/sh");
            q.args(["-c", "echo hi"]);
            let mut s = std::process::Command::new("/bin/sh");
            s.args(["-c", "sleep 30"]);
            (q, s)
        };
        let o = run_with_timeout(quick, std::time::Duration::from_secs(20)).unwrap();
        assert!(o.status.success() && String::from_utf8_lossy(&o.stdout).contains("hi"));
        let t = std::time::Instant::now();
        assert!(run_with_timeout(slow, std::time::Duration::from_secs(1)).is_err());
        assert!(t.elapsed() < std::time::Duration::from_secs(10));
    }
}
