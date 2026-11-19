//! Remote command execution and explicitly requested artifact receipt through system OpenSSH.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SshOptions {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub identity_file: String,
}

impl Default for SshOptions {
    fn default() -> Self {
        Self { host: String::new(), port: 22, username: String::new(), identity_file: String::new() }
    }
}

#[derive(Debug)]
pub enum InvalidSshOption {
    Host,
    Port,
    Username,
    KeyPath,
}

impl std::fmt::Display for InvalidSshOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Host => "Enter a hostname, IP address or OpenSSH alias without a port",
            Self::Port => "SSH port must be between 1 and 65535",
            Self::Username => "Invalid SSH username",
            Self::KeyPath => "Invalid SSH identity file path",
        })
    }
}

impl std::error::Error for InvalidSshOption {}

impl SshOptions {
    pub fn validate(&self) -> Result<(), InvalidSshOption> {
        let host = self.plain_host();
        let is_ipv6 = host.contains(':');
        if is_ipv6 {
            let (address, scope) = host.split_once('%').map_or((host, None), |(ip, scope)| (ip, Some(scope)));
            if address.parse::<std::net::Ipv6Addr>().is_err()
                || scope.is_some_and(|s| s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')))
            {
                return Err(InvalidSshOption::Host);
            }
        }
        if self.host.is_empty()
            || (!is_ipv6 && self.host.contains(['[', ']']))
            || self.host.starts_with('-')
            || self.host.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '@' | '/' | '\\' | '"' | '\''))
        {
            return Err(InvalidSshOption::Host);
        }
        if self.port == 0 {
            return Err(InvalidSshOption::Port);
        }
        if self.username.starts_with('-') || self.username.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '@' | '"' | '\'' | '='))
        {
            return Err(InvalidSshOption::Username);
        }
        if self.identity_file.chars().any(|c| c.is_control()) {
            return Err(InvalidSshOption::KeyPath);
        }
        Ok(())
    }

    fn plain_host(&self) -> &str {
        self.host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(&self.host)
    }

    fn destination(&self, brackets: bool) -> String {
        let host = self.plain_host();
        if brackets && host.contains(':') { format!("[{host}]") } else { host.to_owned() }
    }

    pub fn endpoint(&self) -> String {
        let host = self.destination(true);
        if self.username.is_empty() { format!("{host}:{}", self.port) } else { format!("{}@{host}:{}", self.username, self.port) }
    }

    fn command(&self, sftp: bool) -> Result<Command> {
        self.validate()?;
        let mut c = Command::new(if sftp { "sftp" } else { "ssh" });
        crate::platform::proc::hide_console(&mut c);
        c.args(["-oBatchMode=yes", "-oStrictHostKeyChecking=yes", "-oConnectTimeout=10", "-oConnectionAttempts=1"]);
        c.arg(if sftp { "-P" } else { "-p" }).arg(self.port.to_string());
        if !self.username.is_empty() {
            c.arg("-o").arg(format!("User={}", self.username));
        }
        if !self.identity_file.is_empty() {
            c.arg("-i").arg(&self.identity_file);
        }
        if sftp {
            c.args(["-b", "-"]);
        } else {
            c.arg("-T");
        }
        c.arg("--").arg(self.destination(sftp));
        Ok(c)
    }
}

#[derive(Clone, Debug)]
pub struct ReceivedFile {
    pub path: PathBuf,
    pub size: u64,
}

pub fn tool_definition() -> Value {
    json!({"name":"remote_control", "description":"Execute a command on the configured remote SSH host, or receive one exact absolute file path to the requesting computer ONLY when the human requested result files. Authentication uses SSH keys/agent and verified known_hosts. Nonzero exit_code is failure.",
        "input_schema":{"type":"object","properties":{"action":{"type":"string","enum":["execute","receive"]},"command":{"type":"string"},"remote_path":{"type":"string"},"timeout_seconds":{"type":"integer","minimum":1,"maximum":3600}},"required":["action"]}})
}

pub struct SshConnection {
    pub options: SshOptions,
    closed: AtomicBool,
}

impl SshConnection {
    pub fn new(options: SshOptions) -> Result<std::sync::Arc<Self>> {
        options.validate()?;
        Ok(std::sync::Arc::new(Self { options, closed: AtomicBool::new(false) }))
    }
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub async fn execute(&self, command: &str, seconds: u64, stop: &AtomicBool) -> Result<Value> {
        if command.trim().is_empty() || command.contains('\0') {
            bail!("A non-empty remote command is required");
        }
        let mut c = self.options.command(false)?;
        c.arg(command);
        self.run(c, None, seconds, stop).await
    }

    async fn run(&self, mut c: Command, input: Option<String>, seconds: u64, stop: &AtomicBool) -> Result<Value> {
        if !(1..=3600).contains(&seconds) {
            bail!("timeout_seconds must be between 1 and 3600");
        }
        if self.is_closed() || stop.load(Ordering::SeqCst) {
            bail!("Remote operation cancelled");
        }
        c.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = crate::platform::proc::spawn_group(&mut c)
            .context("Cannot start OpenSSH. Install ssh and sftp, then confirm the host key and key/agent login in your terminal")?;
        let mut guard = ProcessGuard(child);
        let out = guard.0.stdout.take().ok_or_else(|| anyhow!("Missing SSH stdout"))?;
        let err = guard.0.stderr.take().ok_or_else(|| anyhow!("Missing SSH stderr"))?;
        let stdout = tokio::task::spawn_blocking(move || read_bounded(out));
        let stderr = tokio::task::spawn_blocking(move || read_bounded(err));
        let writer = if let Some(input) = input {
            let mut stdin = guard.0.stdin.take().ok_or_else(|| anyhow!("Missing SFTP stdin"))?;
            // SFTP batch commands are small; writing on a blocking worker avoids stalling the UI runtime.
            Some(tokio::task::spawn_blocking(move || stdin.write_all(input.as_bytes())))
        } else {
            None
        };
        let start = Instant::now();
        let status = loop {
            if stop.load(Ordering::SeqCst) || self.is_closed() {
                bail!("Remote operation cancelled. A remote child process may continue running; check the remote host before retrying");
            }
            if start.elapsed() > Duration::from_secs(seconds) {
                bail!("SSH operation timed out. A remote child process may continue running; check the remote host before retrying");
            }
            if let Some(status) = guard.0.try_wait()? {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(75)).await;
        };
        if let Some(writer) = writer {
            tokio::time::timeout(Duration::from_secs(3), writer).await.context("SFTP stdin did not close")???;
        }
        let stdout = tokio::time::timeout(Duration::from_secs(3), stdout).await.context("SSH stdout did not close")???;
        let stderr = tokio::time::timeout(Duration::from_secs(3), stderr).await.context("SSH stderr did not close")???;
        Ok(json!({ "exit_code": status.code(), "success": status.success(), "stdout": stdout, "stderr": stderr }))
    }

    pub async fn receive(&self, remote: &str, workspace: &str, stop: &AtomicBool) -> Result<ReceivedFile> {
        let name = receipt_name(remote)?;
        let base = dirs::download_dir().ok_or_else(|| anyhow!("Downloads folder is unavailable"))?;
        let directory = receipt_directory(&base, workspace)?;
        self.receive_into(remote, &directory, &name, stop).await
    }

    async fn receive_into(&self, remote: &str, directory: &Path, name: &str, stop: &AtomicBool) -> Result<ReceivedFile> {
        self.receive_with_command(remote, directory, name, stop, self.options.command(true)?).await
    }

    async fn receive_with_command(&self, remote: &str, directory: &Path, name: &str, stop: &AtomicBool, command: Command) -> Result<ReceivedFile> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        std::fs::create_dir_all(directory)?;
        let directory = directory.canonicalize()?;
        let partial = loop {
            let dir = directory.join(format!(".receive-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match std::fs::create_dir(&dir) {
                Ok(()) => break Partial(dir),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        };
        let staged = partial.0.join("file.part");
        let local = staged.to_str().ok_or_else(|| anyhow!("Download path is not valid Unicode"))?;
        let batch = format!("get {} {}\n", sftp_quote(remote)?, sftp_quote(&local.replace('\\', "/"))?);
        let result = self.run(command, Some(batch), 600, stop).await?;
        if result["success"] != true {
            bail!("SFTP failed: {}. Confirm known_hosts and SSH key/agent authentication first", result["stderr"].as_str().unwrap_or_default());
        }
        if stop.load(Ordering::SeqCst) || self.is_closed() {
            bail!("File receipt cancelled");
        }
        let size = std::fs::metadata(&staged).context("SFTP did not create the requested file")?.len();
        let path = publish(&staged, &directory, name)?;
        Ok(ReceivedFile { path, size })
    }
}

pub(crate) struct ProcessGuard(pub std::process::Child);
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        crate::platform::proc::kill_group(&mut self.0);
    }
}
struct Partial(PathBuf);
impl Drop for Partial {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn read_bounded(mut reader: impl Read) -> std::io::Result<String> {
    const MAX: usize = 1024 * 1024;
    let mut bytes = Vec::new();
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let keep = n.min(MAX.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&buf[..keep]);
        truncated |= keep < n;
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        text.push_str("\n[output truncated at 1 MiB]");
    }
    Ok(text)
}

fn sftp_quote(path: &str) -> Result<String> {
    if path.len() > 32768 {
        bail!("File path exceeds 32 KiB");
    }
    if path.chars().any(|c| c.is_control()) {
        bail!("File paths cannot contain control characters");
    }
    let mut out = String::from("\"");
    for c in path.chars() {
        // OpenSSH's batch lexer already protects glob characters inside double quotes.
        // Escaping them again makes the backslash part of the remote filename.
        if matches!(c, '\\' | '"') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Ok(out)
}

fn receipt_name(remote: &str) -> Result<String> {
    if !remote.starts_with('/') {
        bail!("An absolute remote file path is required");
    }
    sftp_quote(remote)?;
    let name = remote.rsplit('/').next().unwrap_or_default();
    if name.is_empty() || name == "." || name == ".." {
        bail!("Specify a file, not a directory");
    }
    let mut safe: String = name.chars().map(|c| if matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c }).collect();
    safe = safe.trim_end_matches([' ', '.']).to_string();
    if safe.is_empty() {
        bail!("Invalid result file name");
    }
    if safe.len() > 180 {
        safe = format!("result-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos());
    }
    let stem = safe.split('.').next().unwrap_or_default().to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.ends_with(|c: char| c.is_ascii_digit()))
    {
        safe.insert(0, '_');
    }
    Ok(safe)
}

fn publish(staged: &Path, directory: &Path, name: &str) -> Result<PathBuf> {
    for i in 0..10_000 {
        let filename = if i == 0 { name.to_string() } else { format!("{i}-{name}") };
        let path = directory.join(filename);
        match std::fs::hard_link(staged, &path) {
            Ok(()) => {
                std::fs::remove_file(staged)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("Cannot publish received file atomically"),
        }
    }
    bail!("Too many result files with the same name")
}

fn receipt_directory(downloads: &Path, workspace: &str) -> Result<PathBuf> {
    fn directory(parent: &Path, name: &str) -> Result<PathBuf> {
        let path = parent.join(name);
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("Result directory must be a real directory, not a symlink: {}", path.display());
        }
        let canonical = path.canonicalize()?;
        if canonical.parent() != Some(parent) {
            bail!("Result directory is outside the Downloads/GhostDesk workspace");
        }
        Ok(canonical)
    }
    let downloads = downloads.canonicalize()?;
    let root = directory(&downloads, "GhostDesk")?;
    directory(&root, &crate::profiles::slug(workspace))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_option_injection_and_invalid_ports() {
        for host in ["-oProxyCommand=bad", "user@host", "host\nbye", "", "host name"] {
            assert!(SshOptions { host: host.into(), ..Default::default() }.validate().is_err());
        }
        assert!(SshOptions { host: "build-server".into(), port: 0, ..Default::default() }.validate().is_err());
        assert!(SshOptions { host: "build-server".into(), username: "-root".into(), ..Default::default() }.validate().is_err());
        assert!(SshOptions { host: "2001:db8::1".into(), username: "worker".into(), ..Default::default() }.validate().is_ok());
    }

    #[test]
    fn ssh_and_sftp_use_the_same_ipv6_host_and_reject_ambiguous_addresses() {
        for host in ["dev:2222", "[::1]:2222", "[::1", "[dev]", "::zzz", "fe80::1%"] {
            let options = SshOptions { host: host.into(), ..Default::default() };
            assert!(options.validate().is_err(), "{host}");
        }
        for host in ["::1", "[::1]", "fe80::1%en0", "[fe80::1%en0]"] {
            let options = SshOptions { host: host.into(), ..Default::default() };
            let plain = host.trim_matches(['[', ']']);
            for (sftp, expected) in [(false, plain.to_owned()), (true, format!("[{plain}]"))] {
                let cmd = options.command(sftp).unwrap();
                assert_eq!(cmd.get_args().last().unwrap().to_str(), Some(expected.as_str()));
            }
        }
    }

    #[test]
    fn quotes_sftp_paths_literally() {
        assert_eq!(sftp_quote("/tmp/결과 [v1]*.txt").unwrap(), "\"/tmp/결과 [v1]*.txt\"");
        assert!(sftp_quote("/tmp/a\nget /secrets").is_err());
        assert_eq!(receipt_name("/tmp/../결과.txt").unwrap(), "결과.txt");
        for p in ["relative.txt", "/", "/tmp/..", "/tmp/."] {
            assert!(receipt_name(p).is_err(), "{p}");
        }
    }

    #[test]
    fn receipt_publication_never_overwrites_existing_file() {
        let dir = std::env::temp_dir().join(format!("ghostdesk-receipt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.part");
        std::fs::write(&a, "first").unwrap();
        let first = publish(&a, &dir, "result.txt").unwrap();
        std::fs::write(&a, "second").unwrap();
        let second = publish(&a, &dir, "result.txt").unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(first).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(second).unwrap(), "second");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ssh_options_never_disable_host_verification_and_ipv6_is_a_destination() {
        let options =
            SshOptions { host: "2001:db8::1".into(), username: "worker".into(), identity_file: "/keys/my key".into(), ..Default::default() };
        let c = options.command(true).unwrap();
        let args: Vec<_> = c.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert!(args.contains(&"-oBatchMode=yes".into()));
        assert!(args.contains(&"-oStrictHostKeyChecking=yes".into()));
        assert_eq!(args.last().unwrap(), "[2001:db8::1]");
        assert!(args.contains(&"/keys/my key".into()));
    }

    #[test]
    fn output_is_bounded_but_stream_is_fully_drained() {
        let text = read_bounded(std::io::Cursor::new(vec![b'x'; 1024 * 1024 + 100])).unwrap();
        assert!(text.ends_with("[output truncated at 1 MiB]"));
        assert!(text.len() < 1024 * 1024 + 100);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn subprocess_reports_nonzero_timeout_and_cancellation() {
        let connection = SshConnection::new(SshOptions { host: "test".into(), ..Default::default() }).unwrap();
        let stop = AtomicBool::new(false);
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf output; printf failure >&2; exit 7"]);
        let result = connection.run(command, None, 5, &stop).await.unwrap();
        assert_eq!(result["exit_code"], 7);
        assert_eq!(result["stdout"], "output");
        assert_eq!(result["stderr"], "failure");
        assert_eq!(result["success"], false);
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 10"]);
        assert!(connection.run(command, None, 1, &stop).await.unwrap_err().to_string().contains("timed out"));
        stop.store(true, Ordering::SeqCst);
        let command = Command::new("/bin/echo");
        assert!(connection.run(command, None, 5, &stop).await.unwrap_err().to_string().contains("cancelled"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sftp_receipt_publishes_only_complete_files_and_cleans_partial_failures() {
        let dir = std::env::temp_dir().join(format!("ghostdesk-sftp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let connection = SshConnection::new(SshOptions { host: "test".into(), ..Default::default() }).unwrap();
        let stop = AtomicBool::new(false);
        let mut command = Command::new("/usr/bin/python3");
        command.args(["-c", "import sys,shlex,pathlib; args=shlex.split(sys.stdin.readline()); pathlib.Path(args[2]).write_bytes(b'complete')"]);
        let result = connection.receive_with_command("/tmp/결과 file.txt", &dir, "결과 file.txt", &stop, command).await.unwrap();
        assert_eq!(std::fs::read(&result.path).unwrap(), b"complete");
        assert_eq!(result.size, 8);
        let mut command = Command::new("/usr/bin/python3");
        command.args([
            "-c",
            "import sys,shlex,pathlib; args=shlex.split(sys.stdin.readline()); pathlib.Path(args[2]).write_bytes(b'partial'); sys.exit(1)",
        ]);
        assert!(connection.receive_with_command("/tmp/file.txt", &dir, "file.txt", &stop, command).await.is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn result_directory_rejects_symlinks_outside_downloads() {
        let root = std::env::temp_dir().join(format!("ghostdesk-receipt-link-{}", std::process::id()));
        let downloads = root.join("Downloads");
        let outside = root.join("outside");
        std::fs::create_dir_all(downloads.join("GhostDesk")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, downloads.join("GhostDesk/ws")).unwrap();
        assert!(receipt_directory(&downloads, "ws").is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn real_openssh_sftp_preserves_literal_special_character_paths() {
        let root = std::env::temp_dir().join(format!("ghostdesk-real-sftp-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let connection = SshConnection::new(SshOptions { host: "test".into(), ..Default::default() }).unwrap();
        let stop = AtomicBool::new(false);
        for name in ["결과 [v1]*.txt", "what?.txt", "quote\".txt", "back\\slash.txt"] {
            let source = root.join(name);
            std::fs::write(&source, b"literal file").unwrap();
            let mut c = Command::new("/usr/bin/sftp");
            c.args(["-D", "/usr/libexec/sftp-server", "-b", "-"]);
            let file = connection
                .receive_with_command(source.to_str().unwrap(), &root.join("received"), &receipt_name(source.to_str().unwrap()).unwrap(), &stop, c)
                .await
                .unwrap();
            assert_eq!(std::fs::read(file.path).unwrap(), b"literal file");
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}

impl SshConnection {
    pub fn size(&self) -> (u32, u32) {
        (0, 0)
    }
    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        anyhow::bail!("SSH workspaces have no screen; use remote_control execute/receive")
    }
    pub async fn zoom(&self, _region: [f64; 4]) -> Result<Vec<u8>> {
        anyhow::bail!("SSH workspaces have no screen; use remote_control execute/receive")
    }
    pub async fn mouse_move(&self, _x: f64, _y: f64, _mods: i64) -> Result<()> {
        anyhow::bail!("SSH workspaces have no screen; use remote_control execute/receive")
    }
    pub async fn mouse_down(&self, _b: crate::browser::MouseButton, _count: i64, _mods: i64) -> Result<()> {
        anyhow::bail!("SSH workspaces have no screen; use remote_control execute/receive")
    }
    pub async fn mouse_up(&self, _b: crate::browser::MouseButton, _count: i64, _mods: i64) -> Result<()> {
        anyhow::bail!("SSH workspaces have no screen; use remote_control execute/receive")
    }
    pub async fn click(&self, _pos: Option<(f64, f64)>, _b: crate::browser::MouseButton, _count: u32, _mods: i64) -> Result<()> {
        anyhow::bail!("SSH workspaces have no screen; use remote_control execute/receive")
    }
    pub async fn drag(&self, _from: (f64, f64), _to: (f64, f64), _mods: i64) -> Result<()> {
        anyhow::bail!("SSH workspaces have no screen; use remote_control execute/receive")
    }
    pub async fn scroll(&self, _pos: Option<(f64, f64)>, _dx: f64, _dy: f64, _mods: i64) -> Result<()> {
        anyhow::bail!("SSH workspaces have no screen; use remote_control execute/receive")
    }
    pub async fn key(&self, _combo: &str, _repeat: u32) -> Result<()> {
        anyhow::bail!("SSH workspaces have no screen; use remote_control execute/receive")
    }
    pub async fn hold_key(&self, _combo: &str, _secs: f64) -> Result<()> {
        anyhow::bail!("SSH workspaces have no screen; use remote_control execute/receive")
    }
    pub async fn type_text(&self, _text: &str) -> Result<()> {
        anyhow::bail!("SSH workspaces have no screen; use remote_control execute/receive")
    }
    pub fn mouse_pos(&self) -> (f64, f64) {
        (0.0, 0.0)
    }
    pub fn views(&self) -> (Vec<crate::desk::ViewInfo>, Option<String>) {
        (Vec::new(), None)
    }
    pub async fn switch_view(&self, _index: usize) -> Result<()> {
        anyhow::bail!("SSH workspaces have no screen; use remote_control execute/receive")
    }
    pub fn take_notes(&self) -> Vec<String> {
        Vec::new()
    }
    pub async fn shutdown(&self) {
        self.close();
    }
}
