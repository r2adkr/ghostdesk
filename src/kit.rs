//! Connection kit: files that let AI agents not listed under Connections use GhostDesk too.
//!
//! GhostDesk is an MCP server (`ghostdesk --mcp`), so any agent with MCP support can use it.
//! Agents are configured differently, so the kit comes in three shapes:
//! - `ghostdesk.mcp.json`: the standard `mcpServers` config most apps read (connectors in
//!   Cursor, VS Code, Gemini CLI, Windsurf, Cline, …)
//! - `skills/ghostdesk/SKILL.md`: a usage guide in Agent Skills format, for agents that read
//!   skills
//! - `claude-plugin/`: both of the above as a Claude plugin (`.claude-plugin/plugin.json` +
//!   `.mcp.json` + `skills/`)
//!
//! The skill text is generated from the instructions, modes and tool list the MCP server gives
//! agents (`mcp`), so this file doesn't need updating when the tools change.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// Kit folder name (created under Downloads).
pub const KIT_DIR: &str = "GhostDesk 연결 패키지";
/// Client name the MCP server reports to the GhostDesk window when the app is unknown.
const CLIENT_NAME: &str = "외부 에이전트";

/// The standard `mcpServers` config. Nearly every MCP client accepts it as is.
pub fn mcp_config(exe: &Path) -> Value {
    json!({ "mcpServers": { "ghostdesk": server_entry(exe) } })
}

fn server_entry(exe: &Path) -> Value {
    json!({
        "command": exe.display().to_string(),
        "args": ["--mcp"],
        "env": { "GHOSTDESK_CLIENT_NAME": CLIENT_NAME },
    })
}

/// SKILL.md in Agent Skills format.
pub fn skill_markdown(exe: &Path) -> String {
    let tools = crate::mcp::tools();
    let tool_lines: String = tools
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| Some(format!("- `{}` — {}\n", t["name"].as_str()?, first_sentence(t["description"].as_str().unwrap_or("")))))
        .collect();
    let mode_lines: String = crate::mcp::current_modes().iter().map(|(n, d)| format!("- `{n}` — {d}\n")).collect();
    let config = serde_json::to_string_pretty(&mcp_config(exe)).unwrap_or_default();
    format!(
        "---\n\
name: ghostdesk\n\
description: Operate websites and desktop apps in GhostDesk's isolated, invisible screens without touching the user's own mouse, keyboard or screen. Use when the user asks to do something \"with GhostDesk\" / \"GhostDesk 로\", or to work on a site or app without disturbing their screen. Also use it, after asking the user once, when a task needs clicking and typing in a website or app UI and the user did not mention GhostDesk.\n\
---\n\n\
# GhostDesk\n\n\
GhostDesk gives you hidden browsers, hidden app windows and remote desktops that the user watches live in the GhostDesk window. \
You drive them through GhostDesk's MCP tools (`desk_*`).\n\n\
## Setup\n\n\
If the `desk_*` tools are not available, the GhostDesk MCP server is not connected yet. Add this to your MCP client's settings \
(it is the standard `mcpServers` format), then reload the client:\n\n\
```json\n{config}\n```\n\n\
GhostDesk starts by itself when a tool is called. No API key is needed.\n\n\
## How to work\n\n\
{instructions}\n\n\
## Modes for `desk_open`\n\n\
{mode_lines}\n\
## Tools\n\n\
{tool_lines}",
        instructions = crate::mcp::server_instructions(),
    )
}

/// Plugin manifest (`.claude-plugin/plugin.json`).
fn plugin_manifest() -> Value {
    json!({
        "name": "ghostdesk",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Operate websites and apps in GhostDesk's isolated screens without touching the user's own screen.",
        "homepage": "https://github.com/r2adkr/ghostdesk",
    })
}

/// Human-readable instructions (the kit folder's README).
fn readme(exe: &Path) -> String {
    format!(
        "GhostDesk 연결 패키지\n\
=====================\n\n\
'외부 연결' 목록에 없는 AI 앱에서도 GhostDesk 를 쓸 수 있게 하는 파일들이에요.\n\
쓰는 앱이 지원하는 방식 하나만 고르면 됩니다.\n\n\
1. ghostdesk.mcp.json — MCP 설정 (커넥터)\n\
   대부분의 AI 앱(Cursor, VS Code, Gemini CLI, Windsurf, Cline, Zed 등)의 'MCP 서버' 설정에\n\
   이 파일 내용을 넣거나 붙여 넣으세요. 이미 다른 서버가 있으면 \"ghostdesk\" 항목만 추가하면 돼요.\n\n\
2. skills/ghostdesk — 스킬\n\
   스킬(SKILL.md)을 읽는 에이전트라면 이 폴더를 그 앱의 스킬 폴더에 복사하세요.\n\
   스킬에는 사용법과 MCP 설정 방법이 함께 들어 있어요.\n\n\
3. claude-plugin — 플러그인\n\
   Claude 형식 플러그인을 받는 앱에서 이 폴더를 플러그인으로 추가하세요 (MCP 설정 + 스킬 포함).\n\n\
연결에 쓰는 GhostDesk 위치: {}\n\
GhostDesk 를 다른 곳으로 옮겼다면 연결 창에서 이 패키지를 다시 만드세요.\n",
        exe.display()
    )
}

/// Creates the kit folder under `parent` (rewriting it if present) and returns its path.
pub fn write_kit(parent: &Path, exe: &Path) -> Result<PathBuf> {
    let root = parent.join(KIT_DIR);
    let config = serde_json::to_string_pretty(&mcp_config(exe))? + "\n";
    let skill = skill_markdown(exe);
    let files: [(PathBuf, String); 6] = [
        (root.join("README.txt"), readme(exe)),
        (root.join("ghostdesk.mcp.json"), config.clone()),
        (root.join("skills/ghostdesk/SKILL.md"), skill.clone()),
        (root.join("claude-plugin/.claude-plugin/plugin.json"), serde_json::to_string_pretty(&plugin_manifest())? + "\n"),
        (root.join("claude-plugin/.mcp.json"), config),
        (root.join("claude-plugin/skills/ghostdesk/SKILL.md"), skill),
    ];
    for (path, text) in files {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("폴더를 만들지 못했습니다: {}", dir.display()))?;
        }
        std::fs::write(&path, text).with_context(|| format!("파일을 쓰지 못했습니다: {}", path.display()))?;
    }
    Ok(root)
}

/// Default location for the kit: Downloads, or home if there is none.
pub fn default_parent() -> PathBuf {
    dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."))
}

/// Reveals the folder in Finder / Explorer.
pub fn reveal(dir: &Path) {
    let opener = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener).arg(dir).spawn();
}

/// `ghostdesk --export-kit [dir]`: installs GhostDesk, builds the kit and prints its path.
pub fn cli(args: &[String]) -> Result<()> {
    let exe = crate::install::install_app()?;
    let parent = args.first().map(PathBuf::from).unwrap_or_else(default_parent);
    let root = write_kit(&parent, &exe)?;
    println!("{}", root.display());
    Ok(())
}

/// First sentence of a tool description (the skill lists one line per tool).
fn first_sentence(s: &str) -> &str {
    let end = s.find(". ").map(|i| i + 1).or_else(|| s.find('\n')).unwrap_or(s.len());
    s[..end].trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kit_files_are_consistent() {
        let dir = std::env::temp_dir().join(format!("ghostdesk-kit-test-{}", std::process::id()));
        let exe = Path::new("/Applications/GhostDesk.app/Contents/MacOS/ghostdesk");
        let root = write_kit(&dir, exe).unwrap();
        let read = |p: &str| std::fs::read_to_string(root.join(p)).unwrap();

        // MCP config: the standard shape, identical to the one inside the plugin
        let config: Value = serde_json::from_str(&read("ghostdesk.mcp.json")).unwrap();
        assert_eq!(config["mcpServers"]["ghostdesk"]["command"], exe.display().to_string());
        assert_eq!(config["mcpServers"]["ghostdesk"]["args"], json!(["--mcp"]));
        assert_eq!(read("ghostdesk.mcp.json"), read("claude-plugin/.mcp.json"));

        // skill: has front matter (name, description) and mentions every tool and mode
        let skill = read("skills/ghostdesk/SKILL.md");
        assert!(skill.starts_with("---\nname: ghostdesk\ndescription: "), "{skill}");
        assert_eq!(skill, read("claude-plugin/skills/ghostdesk/SKILL.md"));
        for t in crate::mcp::tools().as_array().unwrap() {
            assert!(skill.contains(&format!("`{}`", t["name"].as_str().unwrap())), "{}", t["name"]);
        }
        for (m, _) in crate::mcp::current_modes() {
            assert!(skill.contains(&format!("`{m}`")), "{m}");
        }
        assert!(skill.contains(&crate::mcp::server_instructions()));

        let manifest: Value = serde_json::from_str(&read("claude-plugin/.claude-plugin/plugin.json")).unwrap();
        assert_eq!(manifest["name"], "ghostdesk");
        assert!(read("README.txt").contains(&exe.display().to_string()));

        // rebuilding is fine (overwrites)
        write_kit(&dir, exe).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Builds a sample kit to inspect without touching the installed copy:
    /// `GHOSTDESK_KIT_OUT=<dir> cargo test write_sample_kit -- --ignored`
    #[test]
    #[ignore]
    fn write_sample_kit() {
        let out = std::env::var("GHOSTDESK_KIT_OUT").expect("GHOSTDESK_KIT_OUT 에 폴더를 주세요");
        let root = write_kit(Path::new(&out), &crate::install::stable_exe()).unwrap();
        println!("{}", root.display());
    }

    #[test]
    fn first_sentence_cuts_at_the_first_period() {
        assert_eq!(first_sentence("Open a desk. Then act."), "Open a desk.");
        assert_eq!(first_sentence("한 줄 설명"), "한 줄 설명");
    }
}
