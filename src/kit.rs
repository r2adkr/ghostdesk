//! 연결 패키지: '외부 연결' 목록에 없는 AI 에이전트도 GhostDesk 를 쓰게 하는 파일 묶음.
//!
//! GhostDesk 는 MCP 서버(`ghostdesk --mcp`)이므로, MCP 를 지원하는 에이전트라면 어디든 붙일 수 있다.
//! 에이전트마다 설정 방식이 달라 세 가지 모양으로 내보낸다:
//! - `ghostdesk.mcp.json` — 대부분의 앱이 읽는 표준 `mcpServers` 설정 (Cursor·VS Code·Gemini CLI·Windsurf·Cline 등의 '커넥터')
//! - `skills/ghostdesk/SKILL.md` — Agent Skills 형식의 사용 설명서 (스킬을 읽는 에이전트용)
//! - `claude-plugin/` — 위 두 가지를 묶은 Claude 형식 플러그인 (`.claude-plugin/plugin.json` + `.mcp.json` + `skills/`)
//!
//! 스킬 내용은 MCP 서버가 에이전트에게 주는 안내문·모드·도구 목록(`mcp`)에서 그대로 만든다.
//! 그래서 도구가 바뀌어도 이 파일을 따로 고칠 필요가 없다.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// 패키지 폴더 이름 (내려받기 폴더 아래에 만든다)
pub const KIT_DIR: &str = "GhostDesk 연결 패키지";
/// MCP 서버가 GhostDesk 창에 알리는 클라이언트 이름 (어떤 앱인지 모를 때)
const CLIENT_NAME: &str = "외부 에이전트";

/// 표준 `mcpServers` 설정. 거의 모든 MCP 클라이언트가 이 모양을 그대로 받는다.
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

/// Agent Skills 형식의 SKILL.md
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
description: Operate websites and desktop apps in GhostDesk's isolated, invisible screens without touching the user's own mouse, keyboard or screen. Use when the user asks to do something \"with GhostDesk\" / \"GhostDesk 로\", or to work on a site or app without disturbing their screen.\n\
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

/// 플러그인 설명 파일 (`.claude-plugin/plugin.json`)
fn plugin_manifest() -> Value {
    json!({
        "name": "ghostdesk",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Operate websites and apps in GhostDesk's isolated screens without touching the user's own screen.",
        "homepage": "https://github.com/r2adkr/ghostdesk",
    })
}

/// 사람이 읽는 안내 (패키지 폴더의 README)
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

/// `parent` 아래에 패키지 폴더를 만들고(있으면 새로 쓴다) 그 경로를 돌려준다.
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

/// 패키지를 둘 기본 위치: 내려받기 폴더 (없으면 홈)
pub fn default_parent() -> PathBuf {
    dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."))
}

/// 만든 폴더를 Finder / 탐색기에서 연다
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

/// `ghostdesk --export-kit [폴더]` — GhostDesk 를 설치하고 패키지를 만든 뒤 경로를 출력한다
pub fn cli(args: &[String]) -> Result<()> {
    let exe = crate::install::install_app()?;
    let parent = args.first().map(PathBuf::from).unwrap_or_else(default_parent);
    let root = write_kit(&parent, &exe)?;
    println!("{}", root.display());
    Ok(())
}

/// 도구 설명의 첫 문장 (스킬의 도구 목록은 한 줄씩만)
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

        // MCP 설정: 표준 모양이고 플러그인 안의 것과 같다
        let config: Value = serde_json::from_str(&read("ghostdesk.mcp.json")).unwrap();
        assert_eq!(config["mcpServers"]["ghostdesk"]["command"], exe.display().to_string());
        assert_eq!(config["mcpServers"]["ghostdesk"]["args"], json!(["--mcp"]));
        assert_eq!(read("ghostdesk.mcp.json"), read("claude-plugin/.mcp.json"));

        // 스킬: 앞머리(name·description)가 있고, 모든 도구와 모드를 알린다
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

        // 다시 만들어도 된다 (덮어쓰기)
        write_kit(&dir, exe).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 패키지 미리보기 (설치본을 건드리지 않고 내용을 볼 때):
    /// `GHOSTDESK_KIT_OUT=<폴더> cargo test write_sample_kit -- --ignored`
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
