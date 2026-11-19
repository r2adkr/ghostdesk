//! Installed CLI model connections. Credentials remain owned by each CLI.

use crate::llm::Provider;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

pub fn executable(provider: Provider, explicit: &str) -> Option<PathBuf> {
    if !explicit.trim().is_empty() {
        return Path::new(explicit.trim()).is_file().then(|| PathBuf::from(explicit.trim()));
    }
    crate::install::find_cli(if provider == Provider::ClaudeCli { "claude" } else { "codex" })
}

fn command(provider: Provider, explicit: &str) -> Result<Command> {
    let executable = executable(provider, explicit).ok_or_else(|| {
        anyhow!("{} is not installed or the executable path is invalid. Install the CLI, sign in, then retry", provider.short_name())
    })?;
    let mut c = crate::install::cli_command(&executable);
    // The CLI's own login is the credential source. Inherited API keys must not silently change billing.
    for key in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "OPENAI_API_KEY", "CODEX_API_KEY"] {
        c.env_remove(key);
    }
    Ok(c)
}

pub async fn models(provider: Provider, explicit: &str) -> Result<Vec<String>> {
    verify_login(provider, explicit).await?;
    if provider == Provider::ClaudeCli {
        // The CLI only has aliases, so ask it which exact model each one means right now. Anything it
        // can't resolve stays as the alias (the CLI still understands that).
        let aliases = ["opus", "sonnet", "haiku"];
        let resolved = futures_util::future::join_all(aliases.iter().map(|a| {
            let explicit = explicit.to_string();
            let alias = a.to_string();
            async move { tokio::task::spawn_blocking(move || claude_alias_model(&explicit, &alias)).await.ok().flatten() }
        }))
        .await;
        let mut out: Vec<String> = Vec::new();
        for (alias, id) in aliases.iter().zip(resolved) {
            let name = id.unwrap_or_else(|| alias.to_string());
            if !out.contains(&name) {
                out.push(name);
            }
        }
        Ok(out)
    } else {
        let models = codex_models(explicit).await?;
        if models.is_empty() {
            bail!("This Codex account did not return any available models");
        }
        Ok(models)
    }
}

/// The exact model id a Claude CLI alias (opus/sonnet/haiku) points to. The CLI prints it in its first
/// `init` event, before any model request is made, so the process is stopped right after that line and
/// nothing is billed.
fn claude_alias_model(explicit: &str, alias: &str) -> Option<String> {
    let mut c = command(Provider::ClaudeCli, explicit).ok()?;
    c.args(["-p", ".", "--model", alias, "--output-format", "stream-json", "--verbose"])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = crate::ssh::ProcessGuard(crate::platform::proc::spawn_group(&mut c).ok()?);
    let stdout = child.0.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match std::io::Read::take(&mut reader, 4 * 1024 * 1024).read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Some(m) = init_model(&line) {
                        let _ = tx.send(m);
                        break;
                    }
                }
            }
        }
    });
    let model = rx.recv_timeout(Duration::from_secs(20)).ok();
    drop(child); // stops the CLI
    model
}

/// The `model` of a stream-json `system`/`init` line.
fn init_model(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    (v["type"] == "system" && v["subtype"] == "init").then(|| v["model"].as_str().map(str::to_string)).flatten().filter(|m| !m.is_empty())
}

pub async fn verify_login(provider: Provider, explicit: &str) -> Result<()> {
    let mut auth = command(provider, explicit)?;
    if provider == Provider::ClaudeCli {
        auth.args(["auth", "status"]);
    } else {
        auth.args(["login", "status"]);
    }
    let auth = run(auth, None, 15).await.context("Cannot verify CLI login. Sign in using 'claude auth login' or 'codex login', or update the CLI")?;
    if provider == Provider::ClaudeCli {
        let status: Value = serde_json::from_str(auth.trim()).context("Claude CLI does not support JSON auth status; update Claude Code")?;
        if status["loggedIn"] != true {
            bail!("Claude Code login required: run claude auth login, then retry");
        }
    }
    Ok(())
}

async fn codex_models(explicit: &str) -> Result<Vec<String>> {
    let mut c = command(Provider::CodexCli, explicit)?;
    c.args(["app-server", "--listen", "stdio://"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = crate::ssh::ProcessGuard(crate::platform::proc::spawn_group(&mut c)?);
    let mut stdin = child.0.stdin.take().ok_or_else(|| anyhow!("Missing Codex stdin"))?;
    let stdout = child.0.stdout.take().ok_or_else(|| anyhow!("Missing Codex stdout"))?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let reader = tokio::task::spawn_blocking(move || {
        let mut reader = std::io::BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match std::io::Read::take(&mut reader, 2 * 1024 * 1024).read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if line.len() >= 2 * 1024 * 1024 => break,
                Ok(_) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });
    writeln!(stdin, "{}", json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"ghostdesk","version":env!("CARGO_PKG_VERSION")}}}))?;
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let mut models = Vec::new();
        let mut pages = 0;
        while let Some(line) = rx.recv().await {
            let v: Value = serde_json::from_str(&line)?;
            if v.get("id").is_some() && v.get("error").is_some() {
                bail!("Codex protocol error: {}", v["error"]);
            }
            if v["id"] == 1 {
                writeln!(stdin, "{}", json!({"method":"initialized","params":{}}))?;
                writeln!(stdin, "{}", json!({"id":2,"method":"model/list","params":{}}))?;
            } else if v["id"] == 2 {
                let data = v["result"]["data"].as_array().ok_or_else(|| anyhow!("Unsupported Codex model response; update Codex"))?;
                models.extend(data.iter().filter_map(|m| m["model"].as_str().or_else(|| m["id"].as_str())).map(String::from));
                pages += 1;
                if let Some(cursor) = v["result"]["nextCursor"].as_str().filter(|s| !s.is_empty()) {
                    if pages >= 20 {
                        bail!("Codex model catalog exceeded 20 pages");
                    }
                    writeln!(stdin, "{}", json!({"id":2,"method":"model/list","params":{"cursor":cursor}}))?;
                } else {
                    return Ok(models);
                }
            }
        }
        bail!("Codex model connection closed before replying")
    })
    .await
    .context("Codex model lookup timed out")?;
    drop(stdin);
    drop(child);
    reader.abort();
    result
}

fn conversation(messages: &[Value]) -> (String, Vec<Value>) {
    fn visit(v: &mut Value, images: &mut Vec<Value>) {
        match v {
            Value::Object(map) if map.get("type") == Some(&json!("image")) => {
                images.push(Value::Object(map.clone()));
                *v = json!({"type":"text", "text":format!("[screenshot {} attached in chronological order]", images.len())});
            }
            Value::Object(map) => {
                for value in map.values_mut() {
                    visit(value, images);
                }
            }
            Value::Array(array) => {
                for value in array {
                    visit(value, images);
                }
            }
            _ => {}
        }
    }
    let mut values = json!(messages);
    let mut images = Vec::new();
    visit(&mut values, &mut images);
    (values.to_string(), images)
}

fn output_schema(tools: &Value) -> Value {
    let names: Vec<&str> = tools.as_array().into_iter().flatten().filter_map(|t| t["function"]["name"].as_str()).collect();
    json!({"type":"object","additionalProperties":false,"properties":{"message":{"type":"string"},"calls":{"type":"array","maxItems":20,"items":{"type":"object","additionalProperties":false,"properties":{"name":{"type":"string","enum":names},"arguments_json":{"type":"string"}},"required":["name","arguments_json"]}}},"required":["message","calls"]})
}

fn restrict_codex_catalog(mut catalog: Value, selected: &str) -> Result<Value> {
    let models = catalog["models"].as_array_mut().ok_or_else(|| anyhow!("Codex did not return a supported model catalog; update the CLI"))?;
    models.retain(|m| m["slug"] == selected);
    if models.len() != 1 {
        bail!("The selected Codex model is absent or ambiguous in its native catalog: {selected}");
    }
    let model = &mut models[0];
    model["shell_type"] = json!("disabled");
    model["apply_patch_tool_type"] = Value::Null;
    model["experimental_supported_tools"] = json!([]);
    model["supports_search_tool"] = json!(false);
    model["node_repl_disabled"] = json!(true);
    model["tool_mode"] = json!("direct");
    // A catalog's model-specific multi-agent version overrides global feature flags.
    model["multi_agent_version"] = Value::Null;
    model["multi_agent_reasoning_effort"] = Value::Null;
    Ok(catalog)
}

fn codex_isolation_args(command: &mut Command, catalog: &Path) {
    for option in [
        "features.shell_tool=false",
        "features.unified_exec=false",
        "web_search=\"disabled\"",
        "features.image_generation=false",
        "features.view_image=false",
        "features.apps=false",
        "features.plugins=false",
        "features.remote_plugin=false",
        "features.tool_suggest=false",
        "features.goals=false",
        "features.hooks=false",
        "features.multi_agent=false",
        "features.multi_agent_v2=false",
        "features.code_mode=false",
        "features.code_mode_host=false",
        "tools.experimental_request_user_input.enabled=false",
        "tools.update_plan.enabled=false",
    ] {
        command.arg("-c").arg(option);
    }
    // JSON string encoding is valid TOML for this path; it is passed as an argv value, never to a shell.
    command.arg("-c").arg(format!("model_catalog_json={}", json!(catalog.to_string_lossy())));
}

fn decode_reply(value: &Value, tools: &Value) -> Result<Value> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let message = value["message"].as_str().ok_or_else(|| anyhow!("CLI reply has no message"))?;
    let calls = value["calls"].as_array().ok_or_else(|| anyhow!("CLI reply has no calls array"))?;
    if calls.len() > 20 {
        bail!("CLI returned too many tool calls");
    }
    let mut content = Vec::new();
    if !message.is_empty() {
        content.push(json!({"type":"text","text":message}));
    }
    for call in calls {
        let name = call["name"].as_str().ok_or_else(|| anyhow!("Missing CLI tool name"))?;
        if !tools.as_array().into_iter().flatten().any(|t| t["function"]["name"] == name) {
            bail!("CLI returned an unadvertised tool: {name}");
        }
        let mut args: Value = serde_json::from_str(call["arguments_json"].as_str().ok_or_else(|| anyhow!("Missing tool arguments"))?)?;
        if !args.is_object() {
            bail!("CLI tool arguments must be an object");
        }
        let id = format!("cli-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let mut block = if name == "computer" {
            let action = args["action"].as_str().ok_or_else(|| anyhow!("Computer action required"))?.to_string();
            args.as_object_mut().unwrap().remove("action");
            json!({"type":"tool_use","id":id,"name":action,"toolset_name":"computer","input":args})
        } else {
            json!({"type":"tool_use","id":id,"name":name,"input":args})
        };
        block["id"] = json!(id);
        content.push(block);
    }
    Ok(json!({"content":content,"stop_reason":if calls.is_empty(){"end_turn"}else{"tool_use"},"usage":{}}))
}

fn claude_result(stdout: &str) -> Result<Value> {
    for line in stdout.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v["type"] == "result" {
            if v["is_error"] == true {
                bail!("Claude Code failed: {}", v["result"]);
            }
            if let Some(result) = v.get("structured_output") {
                return Ok(result.clone());
            }
            bail!("Claude Code did not return structured output; update the CLI or choose another supported model");
        }
    }
    bail!("Claude Code exited without a result")
}

fn usage(stdout: &str, provider: Provider) -> Value {
    for line in stdout.lines().rev() {
        let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
        if provider == Provider::ClaudeCli && value["type"] == "result" {
            if value["usage"].is_object() {
                return value["usage"].clone();
            }
        }
        if provider == Provider::CodexCli && value["type"] == "turn.completed" {
            let input = value["usage"]["input_tokens"].as_u64().unwrap_or(0);
            let cached = value["usage"]["cached_input_tokens"].as_u64().unwrap_or(0);
            return json!({"input_tokens":input.saturating_sub(cached),"cache_read_input_tokens":cached,"output_tokens":value["usage"]["output_tokens"].as_u64().unwrap_or(0)});
        }
    }
    json!({})
}

pub async fn send(provider: Provider, explicit: &str, model: &str, system: &str, tools: &Value, messages: &[Value]) -> Result<Value> {
    let tools = crate::llm::cli_tools(tools);
    let schema = output_schema(&tools);
    let (history, images) = conversation(messages);
    let prompt = format!("{system}\n\nYou are the model for a GhostDesk agent. GhostDesk executes the tools below after your reply. Do not execute any CLI-native tools, shell commands, MCP tools or modify local files. Return only the requested structured response: message is your user-facing text; calls contains tool names and JSON-encoded arguments, or [] when done. Never claim an action happened until a subsequent tool result confirms it. Screenshots are in chronological order and coordinates refer to the latest.\n\nAvailable tools:\n{tools}\n\nConversation:\n{history}");
    let scratch = Scratch::new()?;
    let mut c = command(provider, explicit)?;
    c.current_dir(&scratch.0);
    if provider == Provider::ClaudeCli {
        c.args([
            "-p",
            "--verbose",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--tools",
            "",
            "--strict-mcp-config",
            "--mcp-config",
            "{\"mcpServers\":{}}",
            "--setting-sources",
            "",
            "--settings",
            "{\"disableAllHooks\":true}",
            "--no-session-persistence",
            "--model",
            model,
            "--json-schema",
        ])
        .arg(schema.to_string());
        let mut content = vec![json!({"type":"text","text":prompt})];
        content.extend(images);
        let input = format!("{}\n", json!({"type":"user","message":{"role":"user","content":content}}));
        let stdout = run(c, Some(input), 600).await?;
        let mut reply = decode_reply(&claude_result(&stdout)?, &tools)?;
        reply["usage"] = usage(&stdout, provider);
        Ok(reply)
    } else {
        let schema_path = scratch.0.join("response.schema.json");
        let output_path = scratch.0.join("response.json");
        let catalog_path = scratch.0.join("restricted-models.json");
        let mut catalog_command = command(provider, explicit)?;
        catalog_command.args(["debug", "models"]);
        let catalog: Value = serde_json::from_str(&run(catalog_command, None, 20).await?)
            .context("This Codex version does not expose its native model catalog; update the CLI")?;
        std::fs::write(&catalog_path, restrict_codex_catalog(catalog, model)?.to_string())?;
        std::fs::write(&schema_path, schema.to_string())?;
        c.args([
            "exec",
            "--ignore-user-config",
            "--ephemeral",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "--json",
            "--color",
            "never",
            "--model",
            model,
            "--output-schema",
        ])
        .arg(&schema_path)
        .arg("--output-last-message")
        .arg(&output_path);
        codex_isolation_args(&mut c, &catalog_path);
        for (i, image) in images.iter().enumerate() {
            let data = image["source"]["data"].as_str().ok_or_else(|| anyhow!("Invalid screenshot"))?;
            let path = scratch.0.join(format!("screen-{i}.{}", if image["source"]["media_type"] == "image/png" { "png" } else { "jpg" }));
            std::fs::write(&path, base64::engine::general_purpose::STANDARD.decode(data)?)?;
            c.arg("--image").arg(path);
        }
        c.arg("-");
        let stdout = run(c, Some(prompt), 600)
            .await
            .context("Codex model request failed. Update Codex if this version does not support the required flags")?;
        if std::fs::metadata(&output_path)?.len() > 1024 * 1024 {
            bail!("Codex response exceeds 1 MiB");
        }
        let response: Value = serde_json::from_slice(&std::fs::read(&output_path).context("Codex did not write a structured response")?)?;
        let mut reply = decode_reply(&response, &tools)?;
        reply["usage"] = usage(&stdout, provider);
        Ok(reply)
    }
}

async fn run(mut c: Command, input: Option<String>, seconds: u64) -> Result<String> {
    c.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = crate::ssh::ProcessGuard(crate::platform::proc::spawn_group(&mut c)?);
    let stdout = child.0.stdout.take().ok_or_else(|| anyhow!("Missing CLI stdout"))?;
    let stderr = child.0.stderr.take().ok_or_else(|| anyhow!("Missing CLI stderr"))?;
    let out = tokio::task::spawn_blocking(move || crate::ssh::read_bounded(stdout));
    let err = tokio::task::spawn_blocking(move || crate::ssh::read_bounded(stderr));
    let writer = if let Some(input) = input {
        let mut stdin = child.0.stdin.take().ok_or_else(|| anyhow!("Missing CLI stdin"))?;
        Some(tokio::task::spawn_blocking(move || stdin.write_all(input.as_bytes())))
    } else {
        None
    };
    let start = Instant::now();
    let status = loop {
        if start.elapsed() > Duration::from_secs(seconds) {
            bail!("CLI request timed out");
        }
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        tokio::time::sleep(Duration::from_millis(75)).await;
    };
    if let Some(writer) = writer {
        tokio::time::timeout(Duration::from_secs(3), writer).await???;
    }
    let out = tokio::time::timeout(Duration::from_secs(3), out).await???;
    let err = tokio::time::timeout(Duration::from_secs(3), err).await???;
    if !status.success() {
        bail!("CLI exited with {}: {} {}", status, crate::llm::short(&err, 700), crate::llm::short(&out, 700));
    }
    Ok(if out.trim().is_empty() { err } else { out })
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        loop {
            let path =
                crate::platform::paths::data_root().join("cli-work").join(format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(path.parent().unwrap())?;
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
                    }
                    return Ok(Self(path));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn init_line_gives_the_exact_model() {
        let l = r#"{"type":"system","subtype":"init","model":"claude-opus-5-5","tools":[]}"#;
        assert_eq!(super::init_model(l).as_deref(), Some("claude-opus-5-5"));
        assert!(super::init_model(r#"{"type":"assistant","model":"x"}"#).is_none());
        assert!(super::init_model("not json").is_none());
    }

    /// Needs an installed, signed-in Claude Code: `GHOSTDESK_CLAUDE_PATH=/path/to/claude cargo test live_claude_aliases -- --ignored`.
    #[test]
    #[ignore = "Explicit opt-in: starts the installed Claude CLI three times (stopped before any model call)"]
    fn live_claude_aliases_resolve_to_exact_models() {
        for a in ["opus", "sonnet", "haiku"] {
            let explicit = std::env::var("GHOSTDESK_CLAUDE_PATH").unwrap_or_default();
            let m = super::claude_alias_model(&explicit, a).expect("alias resolves");
            assert!(m.starts_with("claude-") && m.contains(a), "{a} -> {m}");
        }
    }
    use super::*;
    use serde_json::json;

    #[test]
    fn cli_reply_only_dispatches_advertised_tools() {
        let tools = json!([{"type":"function","function":{"name":"computer"}}]);
        let reply = decode_reply(
            &json!({"message":"Working","calls":[{"name":"computer","arguments_json":"{\"action\":\"left_click\",\"coordinate\":[10,20]}"}]}),
            &tools,
        )
        .unwrap();
        assert_eq!(reply["content"][1]["name"], "left_click");
        assert_eq!(reply["content"][1]["toolset_name"], "computer");
        assert!(decode_reply(&json!({"message":"","calls":[{"name":"shell","arguments_json":"{}"}]}), &tools).is_err());
        assert!(decode_reply(&json!({"message":"","calls":[{"name":"computer","arguments_json":"[]"}]}), &tools).is_err());
    }

    #[test]
    fn screenshots_are_attached_without_base64_in_prompt_text() {
        let messages = vec![
            json!({"role":"user","content":[{"type":"text","text":"click"},{"type":"image","source":{"media_type":"image/png","data":"c2VjcmV0"}}]}),
        ];
        let (text, images) = conversation(&messages);
        assert!(!text.contains("c2VjcmV0"));
        assert!(text.contains("screenshot 1"));
        assert_eq!(images[0]["source"]["data"], "c2VjcmV0");
    }

    #[test]
    fn failed_cli_results_never_become_successful_model_replies() {
        assert!(claude_result("{\"type\":\"result\",\"is_error\":true,\"result\":\"Please log in\"}").is_err());
        assert!(claude_result("{\"type\":\"result\",\"is_error\":false,\"result\":\"plain text without structured result\"}").is_err());
    }

    #[test]
    fn usage_preserves_cached_tokens_without_double_counting() {
        let codex =
            usage(r#"{"type":"turn.completed","usage":{"input_tokens":1000,"cached_input_tokens":700,"output_tokens":25}}"#, Provider::CodexCli);
        assert_eq!(codex["input_tokens"], 300);
        assert_eq!(codex["cache_read_input_tokens"], 700);
        assert_eq!(codex["output_tokens"], 25);
        let claude = usage(r#"{"type":"result","usage":{"input_tokens":30,"cache_read_input_tokens":20,"output_tokens":10}}"#, Provider::ClaudeCli);
        assert_eq!(claude["input_tokens"], 30);
        assert_eq!(claude["cache_read_input_tokens"], 20);
    }

    #[test]
    fn codex_catalog_restriction_keeps_only_selected_model_and_no_native_tools() {
        let catalog = json!({"models":[{"slug":"chosen","shell_type":"unified_exec","apply_patch_tool_type":"freeform","experimental_supported_tools":["clock"],"supports_search_tool":true,"multi_agent_version":"v2","multi_agent_reasoning_effort":"medium"},{"slug":"other"}]});
        let restricted = restrict_codex_catalog(catalog.clone(), "chosen").unwrap();
        assert_eq!(restricted["models"].as_array().unwrap().len(), 1);
        let model = &restricted["models"][0];
        assert_eq!(model["slug"], "chosen");
        assert_eq!(model["shell_type"], "disabled");
        assert!(model["apply_patch_tool_type"].is_null());
        assert_eq!(model["experimental_supported_tools"], json!([]));
        assert_eq!(model["supports_search_tool"], false);
        assert!(model["multi_agent_version"].is_null());
        assert!(model["multi_agent_reasoning_effort"].is_null());
        assert!(restrict_codex_catalog(catalog, "missing").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_process_preserves_stdin_and_reports_failures() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "cat"]);
        assert_eq!(run(command, Some("한글 입력\nsecond line".into()), 5).await.unwrap(), "한글 입력\nsecond line");
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "echo not-logged-in >&2; exit 2"]);
        assert!(run(command, None, 5).await.unwrap_err().to_string().contains("not-logged-in"));
    }

    #[tokio::test]
    #[ignore = "Explicit opt-in: invokes the installed authenticated Codex CLI model once"]
    async fn live_codex_model_transport() {
        let path = std::env::var("GHOSTDESK_TEST_CODEX").expect("Set GHOSTDESK_TEST_CODEX to the installed executable");
        let models = models(Provider::CodexCli, &path).await.unwrap();
        let model = models.first().expect("account must expose a model");
        let reply = send(Provider::CodexCli,&path,model,"Transport test. Reply with the message GhostDesk CLI OK and calls [].",&json!([{"name":"ask_user","description":"Ask human","input_schema":{"type":"object","properties":{"question":{"type":"string"}},"required":["question"]}}]),&[json!({"role":"user","content":"Return GhostDesk CLI OK without any tool calls."})]).await.unwrap();
        assert_eq!(reply["stop_reason"], "end_turn");
        assert!(reply["content"].as_array().unwrap().iter().any(|b| b["text"].as_str().is_some_and(|s| s.contains("GhostDesk CLI OK"))));
    }
}
