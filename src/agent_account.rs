//! The dedicated macOS account the "separate account session" mode runs in.
//!
//! When an agent (or the New agent dialog) asks for that session and the account does not exist, GhostDesk
//! offers to create it: a standard (non-admin) user with a random password, made with `sysadminctl` through
//! the system's own administrator prompt. GhostDesk never sees or asks for the administrator's password: macOS
//! shows its usual dialog. The new account's password is shown once so the person can log in to it (one login
//! through fast user switching starts its session).

use anyhow::{bail, Result};

/// The account made when there is none yet.
pub const DEFAULT_NAME: &str = "ghostdesk-agent";

/// Accounts that can take a session now: the default and the ones GhostDesk made that exist on this Mac, those
/// with a login session running first (an account nobody has logged in to can't show a screen yet).
pub fn candidates() -> Vec<String> {
    let mut all = vec![DEFAULT_NAME.to_string()];
    for n in known() {
        if !all.contains(&n) {
            all.push(n);
        }
    }
    all.retain(|n| exists(n));
    let active: Vec<String> = all.iter().filter(|n| has_session(n)).cloned().collect();
    if active.is_empty() { all } else { active }
}

/// Every agent account on this Mac: the default and the ones GhostDesk made (only those that still exist).
pub fn all() -> Vec<String> {
    let mut all = vec![DEFAULT_NAME.to_string()];
    for n in known() {
        if !all.contains(&n) {
            all.push(n);
        }
    }
    all.retain(|n| exists(n));
    all
}

/// The name for the next account to make: `ghostdesk-agent`, then `ghostdesk-agent-2`, `-3`, …
pub fn next_name() -> String {
    std::iter::once(DEFAULT_NAME.to_string())
        .chain((2..).map(|n| format!("{DEFAULT_NAME}-{n}")))
        .find(|n| !exists(n))
        .expect("endless iterator")
}

/// Whether the account is logged in (its per-user launchd domain exists).
pub fn has_session(name: &str) -> bool {
    #[cfg(unix)]
    {
        let Some(uid) = crate::session::uid_of(name) else { return false };
        std::process::Command::new("/bin/launchctl")
            .args(["print", &format!("gui/{uid}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        false
    }
}

/// The least busy of `cands` (`load`: agents using or waiting for it); ties keep the list order, so a free
/// account is always preferred and agents spread over the accounts instead of queueing on one.
pub fn pick(cands: &[String], load: impl Fn(&str) -> usize) -> Option<String> {
    cands.iter().min_by_key(|n| load(n)).cloned()
}

/// The account for a request that didn't name one: the least busy account that can take a session, or the
/// default one (which then gets made) when none exists.
pub fn auto_pick() -> String {
    let cands = candidates();
    pick(&cands, |n| {
        let st = crate::turns::global().state(&crate::turns::account_key(n));
        st.owner.is_some() as usize + st.waiting.len()
    })
    .unwrap_or_else(|| DEFAULT_NAME.to_string())
}

/// Account names GhostDesk will create or use: plain, so nothing in them can be read as a path or option.
pub fn valid_name(a: &str) -> bool {
    !a.is_empty()
        && a.len() <= 64
        && !a.starts_with('-')
        && !a.starts_with('.')
        && a.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// Whether a user with this name exists on this Mac.
pub fn exists(name: &str) -> bool {
    #[cfg(unix)]
    {
        crate::session::uid_of(name).is_some()
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        false
    }
}

const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// 20 random letters and digits (no look-alikes such as 0/O or 1/l, so it can be typed from the screen).
pub fn random_password() -> Result<String> {
    let mut bytes = [0u8; 20];
    #[cfg(unix)]
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    }
    // accounts are only made on macOS; elsewhere the standard library's OS-seeded hasher stands in (tests, builds)
    #[cfg(not(unix))]
    {
        use std::hash::{BuildHasher, Hasher};
        for chunk in bytes.chunks_mut(8) {
            let r = std::collections::hash_map::RandomState::new().build_hasher().finish().to_le_bytes();
            chunk.copy_from_slice(&r[..chunk.len()]);
        }
    }
    Ok(bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect())
}

/// The command run as administrator to make the account.
pub fn create_command(name: &str, password: &str) -> Result<String> {
    if !valid_name(name) {
        bail!("계정 이름은 영문 소문자·숫자·'.'·'_'·'-' 만 쓸 수 있어요 (받은 값: {name})");
    }
    if password.len() < 12 || !password.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("암호 형식이 올바르지 않습니다");
    }
    Ok(format!(
        "/usr/sbin/sysadminctl -addUser {name} -fullName 'GhostDesk Agent' -password {password} -home /Users/{name} -shell /bin/zsh"
    ))
}

/// Creates the account and returns its password. macOS asks the person for their administrator password.
#[cfg(target_os = "macos")]
pub async fn create(name: &str) -> Result<String> {
    let password = random_password()?;
    let command = create_command(name, &password)?;
    // the session setup that needs no click (helper, LaunchAgent, lock screen) runs in the same administrator step,
    // so there is a single password prompt; if it can't be staged the account is still made
    let staged = stage_setup(name).ok();
    let chained = match staged.as_ref().map(|(_, path)| guarded_command(path)) {
        Some(Ok(run)) => format!("{command}; {run}"),
        _ => command.clone(),
    };
    // `chained` holds only the validated name, an alphanumeric password and paths we made, so it can sit in an AppleScript string
    let script = format!(
        "do shell script \"{}\" with administrator privileges with prompt \"GhostDesk 가 에이전트 전용 계정 '{name}' 을(를) 만들고 세션 설정을 적용합니다\"",
        applescript_string(&chained)
    );
    let out = tokio::process::Command::new("/usr/bin/osascript").arg("-e").arg(script).output().await;
    if let Some((dir, _)) = &staged {
        let _ = std::fs::remove_dir_all(dir);
    }
    let out = out?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("-128") {
            bail!("관리자 암호 입력을 취소해서 계정을 만들지 않았어요");
        }
        bail!("계정을 만들지 못했어요: {}", err.trim().lines().last().unwrap_or("알 수 없는 오류"));
    }
    // keep the password in the login keychain so the person can look at it later; a failure only means "not saved"
    let _ = save_password(name, &password);
    remember(name);
    // sysadminctl can report success text without creating anything (for example on a policy refusal)
    if !exists(name) {
        bail!("명령은 끝났지만 '{name}' 계정이 보이지 않아요. 시스템 설정 > 사용자 및 그룹에서 확인해 주세요");
    }
    Ok(password)
}

const KEYCHAIN_SERVICE: &str = "GhostDesk agent account";

/// Stores the account's password in the login keychain (replacing an older one).
#[cfg(target_os = "macos")]
pub fn save_password(name: &str, password: &str) -> Result<()> {
    if !valid_name(name) {
        bail!("invalid account name");
    }
    let st = std::process::Command::new("/usr/bin/security")
        .args(["add-generic-password", "-U", "-a", name, "-s", KEYCHAIN_SERVICE, "-w", password])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if !st.success() {
        bail!("keychain refused the password");
    }
    Ok(())
}

/// The saved password, if there is one.
#[cfg(target_os = "macos")]
pub fn load_password(name: &str) -> Option<String> {
    if !valid_name(name) {
        return None;
    }
    let out = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-a", name, "-s", KEYCHAIN_SERVICE, "-w"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let p = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !p.is_empty()).then_some(p)
}

#[cfg(not(target_os = "macos"))]
pub fn save_password(_name: &str, _password: &str) -> Result<()> {
    bail!("macOS only")
}

#[cfg(not(target_os = "macos"))]
pub fn load_password(_name: &str) -> Option<String> {
    None
}

fn list_path() -> std::path::PathBuf {
    crate::platform::paths::data_root().join("agent-accounts.json")
}

/// Names of the accounts GhostDesk made (only names, never passwords).
pub fn known() -> Vec<String> {
    let mut v: Vec<String> =
        std::fs::read(list_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    v.retain(|n| valid_name(n));
    v
}

fn remember(name: &str) {
    let mut v = known();
    if !v.iter().any(|n| n == name) {
        v.push(name.to_string());
        if let Ok(b) = serde_json::to_vec(&v) {
            let _ = std::fs::write(list_path(), b);
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub async fn create(_name: &str) -> Result<String> {
    bail!("전용 에이전트 계정은 macOS 에서만 만들 수 있어요")
}

// ---- session setup for the account (macOS) ----

/// Where the helper binary the agent account runs lives (the account can't read the person's home).
pub const HELPER_DIR: &str = "/Users/Shared/GhostDesk/bin";

/// The administrator script that does the setup steps of the session guide that need no permission click: the
/// helper binary in the shared folder, the LaunchAgent in the account's home, and a lock screen that never asks for
/// a password or starts the screen saver in that account. Each step is allowed to fail alone (the account still
/// works; the guide says what is left). Granting Screen Recording and Accessibility to the helper stays a click in
/// that account's own session: macOS offers no way to do it from outside.
pub fn session_setup_script(account: &str, exe: &str, plist: &str) -> Result<String> {
    if !valid_name(account) {
        bail!("invalid account name");
    }
    for v in [exe, plist] {
        if v.is_empty() || v.contains(['"', '\\', '$', '`', '\n', '\'']) {
            bail!("unsafe path in setup script");
        }
    }
    Ok(format!(
        "ACC={account}\n\
         H=/Users/$ACC\n\
         [ -d \"$H\" ] || /usr/sbin/createhomedir -c -u $ACC > /dev/null 2>&1 || true\n\
         /bin/mkdir -p {HELPER_DIR} || true\n\
         /bin/cp \"{exe}\" {HELPER_DIR}/ghostdesk && /bin/chmod 755 {HELPER_DIR}/ghostdesk || true\n\
         /bin/chmod 1777 /Users/Shared/GhostDesk || true\n\
         /usr/bin/sudo -u $ACC /bin/mkdir -p \"$H/Library/LaunchAgents\" || true\n\
         /bin/cp \"{plist}\" \"$H/Library/LaunchAgents/com.ghostdesk.session-helper.plist\" || true\n\
         /usr/sbin/chown $ACC \"$H/Library/LaunchAgents/com.ghostdesk.session-helper.plist\" || true\n\
         /usr/bin/sudo -u $ACC /usr/bin/defaults write com.apple.screensaver askForPassword -int 0 || true\n\
         /usr/bin/sudo -u $ACC /usr/bin/defaults -currentHost write com.apple.screensaver idleTime -int 0 || true\n\
         true\n"
    ))
}

/// The administrator step must run exactly the script that was just staged. The staging folder belongs to the
/// current user, so another process of theirs could swap the file between writing it and root running it.
/// This reads the file once, checks its hash and runs what it read (not the path again).
#[cfg(target_os = "macos")]
fn guarded_command(script: &std::path::Path) -> Result<String> {
    let body = std::fs::read(script)?;
    let trimmed: &[u8] = {
        let mut end = body.len();
        while end > 0 && body[end - 1] == b'\n' {
            end -= 1;
        }
        &body[..end]
    };
    let mut h = crate::install::Sha256::new();
    h.update(trimmed);
    let hash = h.finish_hex();
    let path = script.display().to_string();
    anyhow::ensure!(!path.contains(['\'', '"', '\\', '$', '`']), "경로에 쓸 수 없는 문자가 있어요");
    Ok(format!(
        "s=$(/bin/cat '{path}'); [ \"$(printf %s \"$s\" | /usr/bin/shasum -a 256 | /usr/bin/cut -d' ' -f1)\" = {hash} ] && /bin/sh -c \"$s\""
    ))
}

/// Text for an AppleScript string literal.
#[cfg(target_os = "macos")]
fn applescript_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Writes the setup script and the LaunchAgent plist into a private temp folder; returns `(folder, script path)`.
#[cfg(target_os = "macos")]
fn stage_setup(account: &str) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    use std::os::unix::fs::PermissionsExt;
    let exe = std::env::current_exe()?;
    let uid = unsafe { libc::getuid() };
    let dir = std::env::temp_dir().join(format!("ghostdesk-setup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
    let plist = dir.join("helper.plist");
    std::fs::write(&plist, crate::session::launch_agent_plist(&format!("{HELPER_DIR}/ghostdesk"), uid))?;
    std::fs::set_permissions(&plist, std::fs::Permissions::from_mode(0o644))?;
    let script = match session_setup_script(account, &exe.to_string_lossy(), &plist.to_string_lossy()) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    let path = dir.join("run.sh");
    std::fs::write(&path, script)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Ok((dir, path))
}

/// Applies the setup to an account that already exists (the New agent screen offers it for older accounts).
#[cfg(target_os = "macos")]
pub async fn setup_session(account: &str) -> Result<()> {
    if !exists(account) {
        bail!("'{account}' 계정이 없습니다");
    }
    let (dir, script_path) = stage_setup(account)?;
    let run = match guarded_command(&script_path) {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    let osa = format!(
        "do shell script \"{}\" with administrator privileges with prompt \"GhostDesk 가 '{account}' 계정의 세션 설정을 적용합니다\"",
        applescript_string(&run)
    );
    let res = tokio::process::Command::new("/usr/bin/osascript").arg("-e").arg(osa).output().await;
    let _ = std::fs::remove_dir_all(&dir);
    let res = res?;
    if !res.status.success() {
        let err = String::from_utf8_lossy(&res.stderr);
        if err.contains("-128") {
            bail!("관리자 암호 입력을 취소해서 적용하지 않았어요");
        }
        bail!("세션 설정을 적용하지 못했어요: {}", err.trim().lines().last().unwrap_or("알 수 없는 오류"));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub async fn setup_session(_account: &str) -> Result<()> {
    bail!("macOS 에서만 쓸 수 있어요")
}

// ---- browser login import (macOS) ----

/// `(keychain service, keychain account)` that a Chromium browser keeps its cookie key under. A profile copied to
/// another account only keeps its logins if that account's keychain holds the same key.
pub fn safe_storage(browser: &str) -> Option<(&'static str, &'static str)> {
    Some(match browser {
        "Google Chrome" | "Chrome Beta" => ("Chrome Safe Storage", "Chrome"),
        "Microsoft Edge" => ("Microsoft Edge Safe Storage", "Microsoft Edge"),
        "Brave" => ("Brave Safe Storage", "Brave"),
        "Naver Whale" => ("Whale Safe Storage", "Whale"),
        "Vivaldi" => ("Vivaldi Safe Storage", "Vivaldi"),
        "Chromium" => ("Chromium Safe Storage", "Chromium"),
        _ => return None,
    })
}

/// A path that can sit inside a double-quoted shell word and an AppleScript string as it is.
fn plain_path(p: &std::path::Path) -> bool {
    p.to_str().is_some_and(|s| !s.is_empty() && !s.contains(['"', '\\', '$', '`', '\n', '\'']))
}

/// The shell script run as administrator: puts the staged profile into the account's home, owned by that account,
/// and adds the cookie key to that account's login keychain (inside its own session), trusting the browser app.
pub fn import_script(account: &str, rel: &str, staged: &str, keyfile: &str, service: &str, kc_account: &str, app: &str) -> Result<String> {
    if !valid_name(account) {
        bail!("invalid account name");
    }
    for v in [rel, staged, keyfile, service, kc_account, app] {
        if v.is_empty() || v.contains(['"', '\\', '$', '`', '\n', '\'']) {
            bail!("unsafe value in import script");
        }
    }
    Ok(format!(
        "set -e\n\
         ACC={account}\n\
         D=\"/Users/$ACC/Library/Application Support/{rel}\"\n\
         UIDN=$(/usr/bin/id -u $ACC)\n\
         /usr/bin/sudo -u $ACC /bin/mkdir -p \"$D\"\n\
         /usr/bin/ditto \"{staged}\" \"$D\"\n\
         /usr/sbin/chown -R $ACC \"$D\"\n\
         KEY=$(/bin/cat \"{keyfile}\")\n\
         /bin/launchctl asuser $UIDN /usr/bin/sudo -u $ACC /usr/bin/security add-generic-password -U -a \"{kc_account}\" -s \"{service}\" -w \"$KEY\" -T \"{app}\" > /dev/null\n"
    ))
}

/// Copies one profile of the person's browser (cookies, logins) into the agent account's copy of that browser, and the
/// key that decrypts them into that account's keychain. macOS asks for the person's login (to read the key) and for the
/// administrator password (to write into the other account); GhostDesk sees neither.
#[cfg(target_os = "macos")]
pub async fn import_browser_login(account: &str, browser: &crate::profiles::InstalledBrowser, profile_dir: &str) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    if !valid_name(account) || !exists(account) {
        bail!("'{account}' 계정이 없습니다");
    }
    if !has_session(account) {
        bail!("'{account}' 계정이 로그인되어 있지 않습니다. 빠른 사용자 전환으로 그 계정에 한 번 로그인해 둔 채로 다시 시도하세요 (그 계정의 키체인은 로그인해야 열려요)");
    }
    let (service, kc_account) = safe_storage(&browser.name).ok_or_else(|| anyhow::anyhow!("{} 은(는) 로그인 가져오기를 지원하지 않습니다", browser.name))?;
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("홈 폴더를 알 수 없습니다"))?;
    let sup = home.join("Library/Application Support");
    let rel = browser.data_dir.strip_prefix(&sup).map_err(|_| anyhow::anyhow!("브라우저 데이터 위치가 예상과 다릅니다"))?.to_path_buf();
    let app_bundle = browser.exe.ancestors().nth(3).map(|p| p.to_path_buf()).filter(|p| p.extension().is_some_and(|e| e == "app"))
        .ok_or_else(|| anyhow::anyhow!("브라우저 앱 위치를 알 수 없습니다"))?;

    // the key: the person's keychain asks them to allow it
    let out = tokio::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-w", "-s", service, "-a", kc_account])
        .output()
        .await?;
    let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || key.is_empty() || key.len() > 128 || !key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')) {
        bail!("{} 의 로그인 키를 읽지 못했어요 (키체인 접근을 허용하지 않았거나, 이 브라우저를 한 번도 쓰지 않았어요)", browser.name);
    }

    // stage the copy where only this person can read it; the administrator step moves it
    let stage = std::env::temp_dir().join(format!("ghostdesk-import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o755))?;
    let cleanup = |stage: &std::path::Path| {
        let _ = std::fs::remove_dir_all(stage);
    };
    let (data_dir, pdir, staged) = (browser.data_dir.clone(), profile_dir.to_string(), stage.join("data"));
    let copied = tokio::task::spawn_blocking({
        let staged = staged.clone();
        move || crate::profiles::clone_profile(&data_dir, &pdir, &staged, &|_, _| {})
    })
    .await?;
    if let Err(e) = copied {
        cleanup(&stage);
        bail!("프로필을 복사하지 못했어요: {e:#}");
    }
    // the administrator step runs as root and reads the key from a file only this person (and root) can open
    let keyfile = stage.join("key");
    std::fs::write(&keyfile, &key)?;
    std::fs::set_permissions(&keyfile, std::fs::Permissions::from_mode(0o600))?;
    let script_path = stage.join("run.sh");
    let rel_s = rel.to_string_lossy().to_string();
    let script = match (plain_path(&staged), plain_path(&keyfile), plain_path(&app_bundle)) {
        (true, true, true) => import_script(account, &rel_s, &staged.to_string_lossy(), &keyfile.to_string_lossy(), service, kc_account, &app_bundle.to_string_lossy()),
        _ => Err(anyhow::anyhow!("경로에 쓸 수 없는 문자가 있어요")),
    };
    let script = match script {
        Ok(s) => s,
        Err(e) => {
            cleanup(&stage);
            return Err(e);
        }
    };
    std::fs::write(&script_path, script)?;
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))?;
    let run = match guarded_command(&script_path) {
        Ok(r) => r,
        Err(e) => {
            cleanup(&stage);
            return Err(e);
        }
    };
    let osa = format!(
        "do shell script \"{}\" with administrator privileges with prompt \"GhostDesk 가 브라우저 로그인을 '{account}' 계정으로 복사합니다\"",
        applescript_string(&run)
    );
    let res = tokio::process::Command::new("/usr/bin/osascript").arg("-e").arg(osa).output().await;
    cleanup(&stage);
    let res = res?;
    if !res.status.success() {
        let err = String::from_utf8_lossy(&res.stderr);
        if err.contains("-128") {
            bail!("관리자 암호 입력을 취소해서 가져오지 않았어요");
        }
        bail!("로그인을 가져오지 못했어요: {}", err.trim().lines().last().unwrap_or("알 수 없는 오류"));
    }
    Ok(format!("{} 의 로그인을 '{account}' 계정으로 가져왔어요", browser.name))
}

#[cfg(not(target_os = "macos"))]
pub async fn import_browser_login(_account: &str, _browser: &crate::profiles::InstalledBrowser, _profile_dir: &str) -> Result<String> {
    bail!("macOS 에서만 쓸 수 있어요")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_plain() {
        assert!(valid_name("ghostdesk-agent") && valid_name("agent.2_x"));
        for bad in ["", "-x", ".x", "A", "a b", "a;b", "a/b", "a'b", "$(x)", &"a".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn import_script_is_plain_and_refuses_odd_values() {
        let s = import_script("ghostdesk-agent", "Google/Chrome", "/tmp/x/data", "/tmp/x/key", "Chrome Safe Storage", "Chrome", "/Applications/Google Chrome.app").unwrap();
        assert!(s.contains("sudo -u $ACC /bin/mkdir") && s.contains("chown -R $ACC") && s.contains("-T \"/Applications/Google Chrome.app\""));
        assert!(s.contains("asuser $UIDN"), "the key goes in through the account's own session");
        assert!(!s.contains("Abc"), "no secret value in the script itself");
        for bad in ["a\"b", "a$b", "a`b", "a\\b", "a'b", "a\nb", ""] {
            assert!(import_script("ghostdesk-agent", bad, "/tmp/x", "/tmp/k", "s", "a", "/Applications/A.app").is_err(), "{bad:?}");
        }
        assert!(import_script("x; rm", "r", "/tmp/x", "/tmp/k", "s", "a", "/Applications/A.app").is_err());
        assert_eq!(safe_storage("Google Chrome"), Some(("Chrome Safe Storage", "Chrome")));
        assert_eq!(safe_storage("Firefox"), None);
    }

    #[test]
    fn session_setup_script_installs_helper_and_plist_for_the_account_only() {
        let s = session_setup_script("ghostdesk-agent", "/Applications/GhostDesk.app/Contents/MacOS/ghostdesk", "/tmp/x/helper.plist").unwrap();
        assert!(s.contains("/Users/Shared/GhostDesk/bin/ghostdesk") && s.contains("LaunchAgents/com.ghostdesk.session-helper.plist"));
        assert!(s.contains("sudo -u $ACC /usr/bin/defaults write com.apple.screensaver askForPassword -int 0"));
        assert!(!s.contains("pmset"), "the person's own sleep settings are not touched");
        assert!(session_setup_script("a;b", "/x", "/y").is_err());
        assert!(session_setup_script("ok", "/x\"y", "/y").is_err());
        assert!(session_setup_script("ok", "/x", "").is_err());
    }

    #[test]
    fn requests_spread_over_the_accounts() {
        let c: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let load = |busy: &'static [(&'static str, usize)]| move |n: &str| busy.iter().find(|(k, _)| *k == n).map_or(0, |(_, v)| *v);
        assert_eq!(pick(&c, load(&[])), Some("a".into()), "all free: first");
        assert_eq!(pick(&c, load(&[("a", 1)])), Some("b".into()), "a is taken");
        assert_eq!(pick(&c, load(&[("a", 1), ("b", 1)])), Some("c".into()));
        assert_eq!(pick(&c, load(&[("a", 2), ("b", 1), ("c", 3)])), Some("b".into()), "all busy: the shortest line");
        assert_eq!(pick(&[], load(&[])), None);
    }

    #[test]
    fn passwords_are_random_and_typeable() {
        let (a, b) = (random_password().unwrap(), random_password().unwrap());
        assert_eq!(a.len(), 20);
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| ALPHABET.contains(&c)), "{a}");
    }

    #[test]
    fn command_makes_a_standard_user_and_rejects_odd_input() {
        let c = create_command("ghostdesk-agent", "Abcdefghjk23456789xy").unwrap();
        assert!(c.starts_with("/usr/sbin/sysadminctl -addUser ghostdesk-agent "));
        assert!(!c.contains("-admin"), "{c}");
        assert!(c.contains("-home /Users/ghostdesk-agent"));
        assert!(create_command("x; rm -rf /", "Abcdefghjk23456789xy").is_err());
        assert!(create_command("ok", "has space and 'quote'").is_err());
        assert!(create_command("ok", "short").is_err());
    }
}

#[cfg(all(test, target_os = "macos"))]
mod guard_tests {
    use super::*;

    #[test]
    fn a_swapped_script_does_not_run() {
        let dir = std::env::temp_dir().join(format!("gd-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (script, mark) = (dir.join("run.sh"), dir.join("ran"));
        std::fs::write(&script, format!("touch '{}'\n", mark.display())).unwrap();
        let run = guarded_command(&script).unwrap();
        // the unchanged script runs
        assert!(std::process::Command::new("/bin/sh").args(["-c", &run]).status().unwrap().success());
        assert!(mark.exists());
        std::fs::remove_file(&mark).unwrap();
        // a replaced one does not
        std::fs::write(&script, format!("touch '{}'\n# changed\n", mark.display())).unwrap();
        assert!(!std::process::Command::new("/bin/sh").args(["-c", &run]).status().unwrap().success());
        assert!(!mark.exists());
        assert_eq!(applescript_string(r#"a"b\c"#), r#"a\"b\\c"#);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
