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
    // `command` holds only the validated name and an alphanumeric password, so it can sit in an AppleScript string
    let script = format!(
        "do shell script \"{command}\" with administrator privileges with prompt \"GhostDesk 가 에이전트 전용 계정 '{name}' 을(를) 만듭니다\""
    );
    let out = tokio::process::Command::new("/usr/bin/osascript").arg("-e").arg(script).output().await?;
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
