//! The tool box: third-party MCP servers GhostDesk's key-based agents can use. The list lives in the private
//! engine folder (`mcp.json`, owner-only) in the format the engine reads, so adding, switching off and removing
//! servers happens here and nowhere else.
use serde_json::{json, Map, Value};
use std::path::PathBuf;

/// How the tools of a server reach the model.
pub const DEFERRED: &str = "deferred";
pub const DIRECT: &str = "direct";

#[derive(Clone, Debug, PartialEq)]
pub struct Server {
    pub name: String,
    /// Program to start (stdio) — empty for a web server.
    pub command: String,
    pub args: Vec<String>,
    /// Address of a web server — empty for a program.
    pub url: String,
    pub env: Vec<(String, String)>,
    pub enabled: bool,
    /// `deferred` (found on demand, saves tokens) or `direct` (always shown to the model).
    pub exposure: String,
}

impl Server {
    /// What the person is asked to approve: the exact thing that will run or be contacted.
    pub fn runs(&self) -> String {
        if self.url.is_empty() {
            std::iter::once(self.command.as_str()).chain(self.args.iter().map(String::as_str)).collect::<Vec<_>>().join(" ")
        } else {
            self.url.clone()
        }
    }

    /// The tool-name prefix the engine gives this server's tools.
    pub fn tool_prefix(&self) -> String {
        format!("mcp__{}__", sanitize(&self.name))
    }
}

fn sanitize(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect()
}

pub fn path() -> PathBuf {
    crate::platform::paths::data_root().join("engine").join("mcp.json")
}

/// Server names: letters, digits, `_` and `-`; names differing only in `-`/`_` are the same server.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 40 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn same_name(a: &str, b: &str) -> bool {
    sanitize(a).eq_ignore_ascii_case(&sanitize(b))
}

/// Splits one line into a program and its arguments (quotes group words; no shell is ever involved).
pub fn split_command(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if started || !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn parse(text: &str) -> Vec<Server> {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return Vec::new() };
    let Some(map) = v["mcpServers"].as_object() else { return Vec::new() };
    map.iter()
        .filter(|(n, _)| valid_name(n))
        .map(|(name, s)| Server {
            name: name.clone(),
            command: s["command"].as_str().unwrap_or_default().to_string(),
            args: s["args"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
            url: s["url"].as_str().unwrap_or_default().to_string(),
            env: s["env"].as_object().map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect()).unwrap_or_default(),
            enabled: s["enabled"].as_bool().unwrap_or(true),
            exposure: if s["exposure"] == DIRECT { DIRECT } else { DEFERRED }.to_string(),
        })
        .collect()
}

fn render(servers: &[Server]) -> String {
    let mut map = Map::new();
    for s in servers {
        let mut o = Map::new();
        if s.url.is_empty() {
            o.insert("command".into(), json!(s.command));
            if !s.args.is_empty() {
                o.insert("args".into(), json!(s.args));
            }
        } else {
            o.insert("url".into(), json!(s.url));
        }
        if !s.env.is_empty() {
            o.insert("env".into(), Value::Object(s.env.iter().map(|(k, v)| (k.clone(), json!(v))).collect()));
        }
        o.insert("enabled".into(), json!(s.enabled));
        o.insert("exposure".into(), json!(s.exposure));
        map.insert(s.name.clone(), Value::Object(o));
    }
    serde_json::to_string_pretty(&json!({ "mcpServers": map, "autoEnableCodemode": false })).unwrap_or_default()
}

pub fn load() -> Vec<Server> {
    std::fs::read_to_string(path()).map(|t| parse(&t)).unwrap_or_default()
}

/// Servers agents actually get: the switched-on ones.
pub fn active() -> Vec<Server> {
    load().into_iter().filter(|s| s.enabled).collect()
}

pub fn save(servers: &[Server]) -> std::io::Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("json.tmp");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(render(servers).as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, render(servers))?;
    std::fs::rename(&tmp, &p)
}

/// Whether a server can be added as it is (the reason in Korean otherwise).
pub fn check(servers: &[Server], new: &Server) -> Result<(), &'static str> {
    if !valid_name(&new.name) {
        return Err("이름은 영문·숫자·-·_ 만 쓸 수 있어요");
    }
    if servers.iter().any(|s| same_name(&s.name, &new.name)) {
        return Err("같은 이름의 도구가 이미 있어요");
    }
    if new.url.is_empty() && new.command.trim().is_empty() {
        return Err("실행할 명령이나 주소를 입력하세요");
    }
    if !new.url.is_empty() && !(new.url.starts_with("https://") || new.url.starts_with("http://localhost") || new.url.starts_with("http://127.0.0.1")) {
        return Err("주소는 https:// 로 시작해야 해요");
    }
    Ok(())
}

/// Connection state of one server, as the engine reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub name: String,
    pub state: String,
    pub tools: Vec<String>,
    pub error: Option<String>,
}

pub fn parse_status(text: &str) -> Vec<Status> {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return Vec::new() };
    v["servers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            Some(Status {
                name: s["name"].as_str()?.to_string(),
                state: s["state"].as_str().unwrap_or("unknown").to_string(),
                tools: s["tools"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
                error: s["error"].as_str().map(String::from),
            })
        })
        .collect()
}

/// Connects to every switched-on server and reports (blocking; call from a background thread). This starts
/// the programs the person added, so it only runs on their request.
pub fn probe() -> Result<Vec<Status>, String> {
    let exe = crate::pi::find().ok_or("엔진이 설치되지 않았어요")?;
    let home = path().parent().map(PathBuf::from).ok_or("경로 오류")?;
    let mut c = crate::pi::engine_command(&exe, &home);
    c.args(["mcp", "list", "--json"]).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    let mut child = crate::ssh::ProcessGuard(crate::platform::proc::spawn_group(&mut c).map_err(|e| e.to_string())?);
    let (tx, rx) = std::sync::mpsc::channel();
    let mut out = child.0.stdout.take().ok_or("출력 오류")?;
    std::thread::spawn(move || {
        let mut t = String::new();
        let _ = std::io::Read::read_to_string(&mut out, &mut t);
        let _ = tx.send(t);
    });
    match rx.recv_timeout(std::time::Duration::from_secs(60)) {
        Ok(t) => Ok(parse_status(&t)),
        Err(_) => Err("연결 확인이 너무 오래 걸려요".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(name: &str) -> Server {
        Server { name: name.into(), command: "npx".into(), args: vec!["-y".into(), "pkg".into()], url: String::new(), env: vec![("K".into(), "v".into())], enabled: true, exposure: DEFERRED.into() }
    }

    #[test]
    fn split_command_groups_quotes_without_a_shell() {
        assert_eq!(split_command("npx -y pkg \"a b\" 'c d'"), ["npx", "-y", "pkg", "a b", "c d"]);
        assert_eq!(split_command("  x   y "), ["x", "y"]);
        assert_eq!(split_command("x \"\""), ["x", ""]);
        assert!(split_command("   ").is_empty());
    }

    #[test]
    fn roundtrip_keeps_every_field_and_turns_codemode_off() {
        let mut web = s("docs");
        web.command.clear();
        web.args.clear();
        web.url = "https://example.com/mcp".into();
        web.enabled = false;
        web.exposure = DIRECT.into();
        let list = vec![s("fs"), web];
        let text = render(&list);
        assert_eq!(parse(&text), list);
        assert!(text.contains("\"autoEnableCodemode\": false"));
    }

    #[test]
    fn names_urls_and_duplicates_are_checked() {
        let list = vec![s("my-tool")];
        assert!(check(&list, &s("other")).is_ok());
        assert!(check(&list, &s("my_tool")).is_err());
        assert!(check(&list, &s("bad name")).is_err());
        let mut w = s("w");
        w.command.clear();
        w.url = "http://evil.example/mcp".into();
        assert!(check(&list, &w).is_err());
        w.url = "https://ok.example/mcp".into();
        assert!(check(&list, &w).is_ok());
        assert_eq!(s("a-b").tool_prefix(), "mcp__a_b__");
    }

    #[test]
    fn status_output_is_read() {
        let t = r#"{"servers":[{"name":"fs","state":"connected","tools":["a","b"]},{"name":"bad","state":"failed","tools":[],"error":"spawn x ENOENT"}],"errors":[]}"#;
        let st = parse_status(t);
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].tools.len(), 2);
        assert_eq!(st[1].error.as_deref(), Some("spawn x ENOENT"));
        assert!(parse_status("not json").is_empty());
    }
}
