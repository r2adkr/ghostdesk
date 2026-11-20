//! Installation and auto-connect.
//!
//! - Installs GhostDesk to a fixed location (no admin rights needed).
//!   - macOS: ~/Applications/GhostDesk.app (app bundle)
//!   - Windows: %LOCALAPPDATA%\Programs\GhostDesk\ghostdesk.exe (a single executable)
//! - Registers the GhostDesk MCP server with Claude Code, the Claude app, Codex, Kimi Code,
//!   OpenCode and pi automatically. The client's own CLI (`claude mcp add`) is used first when
//!   present; if it's missing or fails, the config file is edited directly, preserving formatting
//!   and key order (the original is backed up before the first edit). Settings other than the
//!   `ghostdesk` entry are left alone.
//! - Direct edits keep the original permissions (e.g. 0600) and symlinks (Windows has no
//!   permission bits, so new files inherit the folder's ACL). Files that had a UTF-8 BOM keep it.
//!   Files that can't be read or parsed stop with an error instead of being overwritten with an
//!   empty config.
//! - JSON and TOML are built as serde_json/toml_edit values rather than by string concatenation
//!   (so the `\` in Windows paths is escaped correctly).
//!
//! With this in place, nobody has to wire anything up by hand: just tell the agent "do … with
//! GhostDesk".

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    ffi::OsString,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::platform::fsx;

pub const SERVER_NAME: &str = "ghostdesk";
#[cfg(not(windows))]
pub(crate) const BUNDLE_ID: &str = "dev.ghostdesk.app";

/// Codex's MCP tool call timeout (seconds). The 60 s default cuts desk_open off before it finishes
/// (up to 180 s waiting for user approval + GUI launch + profile/app data cloning).
const CODEX_TOOL_TIMEOUT_SEC: i64 = 600;
/// Codex's MCP server startup (initialize) timeout (seconds).
const CODEX_STARTUP_TIMEOUT_SEC: i64 = 30;
/// OpenCode's tool list fetch timeout (milliseconds). The 5 s default can cut off while `--mcp`
/// launches the GUI.
const OPENCODE_TIMEOUT_MS: i64 = CODEX_STARTUP_TIMEOUT_SEC * 1000;

// ---- user folders ----

/// User folders and environment variables that determine install and config file locations.
///
/// Read from the OS in real runs and swapped for fakes under a temp folder in tests. (On Windows,
/// `dirs::home_dir()` ignores the HOME/USERPROFILE environment variables, so environment
/// variables can't redirect it; hence the explicit swap.)
#[derive(Clone, Debug)]
struct Bases {
    /// Home folder (~, %USERPROFILE%).
    home: PathBuf,
    /// %LOCALAPPDATA%
    #[cfg(windows)]
    local: PathBuf,
    /// %APPDATA%
    #[cfg(windows)]
    roaming: PathBuf,
    /// GhostDesk's own settings file (auto-connect, …).
    settings: PathBuf,
    /// When Some, only this list is consulted instead of the process environment (tests).
    env: Option<Vec<(String, OsString)>>,
}

impl Bases {
    #[cfg_attr(test, allow(dead_code))]
    fn real() -> Bases {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        Bases {
            // dirs uses SHGetKnownFolderPath on Windows, so the location is the same even if the MCP host
            // cleared the environment
            #[cfg(windows)]
            local: dirs::data_local_dir().unwrap_or_else(|| home.join("AppData").join("Local")),
            #[cfg(windows)]
            roaming: dirs::config_dir().unwrap_or_else(|| home.join("AppData").join("Roaming")),
            settings: crate::platform::paths::settings_path(),
            home,
            env: None,
        }
    }

    fn var(&self, name: &str) -> Option<OsString> {
        match &self.env {
            Some(vars) => vars
                .iter()
                .rev()
                .find(|(k, _)| if cfg!(windows) { k.eq_ignore_ascii_case(name) } else { k == name })
                .map(|(_, v)| v.clone()),
            None => std::env::var_os(name),
        }
    }

    /// A non-empty environment variable as a path.
    fn var_path(&self, name: &str) -> Option<PathBuf> {
        self.var(name).filter(|v| !v.is_empty()).map(PathBuf::from)
    }
}

#[cfg(test)]
thread_local! {
    /// Fake user folders set by tests (per thread).
    static TEST_BASES: std::cell::RefCell<Option<Bases>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
impl Bases {
    /// Fake user folders under `root` (with an empty environment too).
    fn under(root: &Path) -> Bases {
        Bases {
            home: root.join("home"),
            #[cfg(windows)]
            local: root.join("local"),
            #[cfg(windows)]
            roaming: root.join("roaming"),
            settings: root.join("settings").join("settings.json"),
            env: Some(Vec::new()),
        }
    }
}

fn bases() -> Bases {
    // tests never look at the real user folders or environment: without a fake set, it's an empty
    // temp folder
    #[cfg(test)]
    {
        TEST_BASES.with(|b| b.borrow().clone()).unwrap_or_else(|| {
            Bases::under(&std::env::temp_dir().join(format!("ghostdesk-install-sandbox-{}", std::process::id())))
        })
    }
    #[cfg(not(test))]
    {
        Bases::real()
    }
}

#[cfg(not(windows))]
fn home() -> PathBuf {
    bases().home
}

/// Tests only: whether the path is under the temp folder.
#[cfg(test)]
fn is_test_path(p: &Path) -> bool {
    let key = |x: &Path| {
        let s = crate::platform::paths::simplify(x).to_string_lossy().into_owned();
        if cfg!(windows) {
            s.replace('/', "\\").to_lowercase()
        } else {
            s
        }
    };
    let t = std::env::temp_dir();
    let roots = [t.clone(), crate::platform::paths::canonicalize(&t).unwrap_or(t)];
    let pk = key(p);
    roots.iter().any(|r| {
        let rk = key(r);
        let rk = rk.trim_end_matches(['\\', '/']);
        !rk.is_empty() && pk.len() > rk.len() && pk.starts_with(rk) && pk[rk.len()..].starts_with(['\\', '/'])
    })
}

/// Called before writing. In tests, trying to modify anything outside the temp folder (real user
/// files) stops before anything is written.
fn guard_write(p: &Path) {
    #[cfg(test)]
    assert!(is_test_path(p), "테스트가 임시 폴더 밖의 파일을 고치려 했습니다: {}", p.display());
    #[cfg(not(test))]
    let _ = p;
}

/// Path for display (Unix: ~/… when under home).
fn show_path(p: &Path) -> String {
    #[cfg(unix)]
    {
        if let Ok(rest) = p.strip_prefix(home()) {
            return format!("~/{}", rest.display());
        }
    }
    crate::platform::paths::simplify(p).display().to_string()
}

/// Install location. macOS: ~/Applications/GhostDesk.app, Windows:
/// %LOCALAPPDATA%\Programs\GhostDesk.
pub fn stable_app() -> PathBuf {
    #[cfg(windows)]
    {
        bases().local.join("Programs").join("GhostDesk")
    }
    #[cfg(not(windows))]
    {
        home().join("Applications").join("GhostDesk.app")
    }
}

/// Installed executable (the path used in client connection configs).
pub fn stable_exe() -> PathBuf {
    installed_exe(&stable_app())
}

/// Executable inside the install folder (bundle).
fn installed_exe(app: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        app.join(crate::platform::paths::EXE_NAME)
    }
    #[cfg(not(windows))]
    {
        app.join("Contents/MacOS/ghostdesk")
    }
}

fn settings_path() -> PathBuf {
    bases().settings
}

/// Reads the GhostDesk settings file (BOM tolerated). (value, whether it had a BOM)
fn read_settings(p: &Path) -> Option<(Value, bool)> {
    let s = std::fs::read_to_string(p).ok()?;
    let (body, bom) = fsx::strip_bom(&s);
    serde_json::from_str::<Value>(body).ok().map(|v| (v, bom))
}

/// Auto-connect setting (on by default). GHOSTDESK_NO_AUTOCONNECT=1 turns it off for this run.
pub fn auto_connect_enabled() -> bool {
    if std::env::var("GHOSTDESK_NO_AUTOCONNECT").map(|v| v == "1").unwrap_or(false) {
        return false;
    }
    read_settings(&settings_path()).and_then(|(v, _)| v["auto_connect"].as_bool()).unwrap_or(true)
}

pub fn set_auto_connect(on: bool) -> Result<()> {
    let p = settings_path();
    guard_write(&p);
    let (mut v, bom) = read_settings(&p).unwrap_or((json!({}), false));
    if !v.is_object() {
        v = json!({});
    }
    v["auto_connect"] = json!(on);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut text = serde_json::to_string_pretty(&v)?;
    if bom {
        text.insert(0, '\u{FEFF}');
    }
    std::fs::write(&p, text)?;
    Ok(())
}

// ---- installation ----

#[cfg(not(windows))]
fn current_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors().find(|p| p.extension().map(|e| e == "app").unwrap_or(false)).map(|p| p.to_path_buf())
}

/// The bundle's Info.plist (version = this build's Cargo.toml version).
#[cfg(not(windows))]
fn info_plist() -> String {
    let v = env!("CARGO_PKG_VERSION");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>GhostDesk</string>
<key>CFBundleDisplayName</key><string>GhostDesk</string>
<key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
<key>CFBundleExecutable</key><string>ghostdesk</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleVersion</key><string>{v}</string>
<key>CFBundleShortVersionString</key><string>{v}</string>
<key>LSMinimumSystemVersion</key><string>13.0</string>
<key>NSHighResolutionCapable</key><true/>
<key>CFBundleAllowMixedLocalizations</key><true/>
<key>CFBundleIconFile</key><string>AppIcon</string>
</dict></plist>
"#
    )
}

/// Finalizes the bundle: writes the source fingerprint (before signing, so it's part of the
/// signature seal) and ad-hoc signs it.
///
/// Without a fingerprint, any old one is removed so it doesn't linger.
#[cfg(not(windows))]
fn seal_bundle(bundle: &Path, src_hash: Option<&str>, local: bool) -> Result<()> {
    match src_hash {
        Some(h) => write_source_stamp(bundle, h)?,
        None => {
            let _ = std::fs::remove_file(source_stamp_path(bundle));
        }
    }
    // The installed copy is always signed with the same local self-signed certificate. macOS ties
    // Screen Recording and Accessibility to the signature's designated requirement; with ad-hoc
    // signing that is the build's hash, so every update or second copy lost the grants. With the
    // certificate it is "this identifier + this certificate", which survives updates.
    // Release packaging (`local == false`) stays ad-hoc; installing re-signs locally.
    // Falls back to ad-hoc if the certificate can't be made.
    if let Some(kc) = local.then(signing_keychain).flatten() {
        let ok = run(
            Command::new("/usr/bin/codesign")
                .args(["--force", "-s", SIGN_CERT, "--keychain"])
                .arg(&kc)
                .args(["--identifier", BUNDLE_ID])
                .arg(bundle),
            Duration::from_secs(120),
        )
        .is_ok();
        if ok {
            return Ok(());
        }
    }
    let _ = run(
        Command::new("/usr/bin/codesign").args(["--force", "-s", "-", "--identifier", BUNDLE_ID]).arg(bundle),
        Duration::from_secs(120),
    );
    Ok(())
}

/// Name of the local signing certificate and the password of its own keychain.
///
/// The keychain holds only this certificate. The password is fixed because GhostDesk has to unlock
/// it without asking; it protects nothing else.
#[cfg(not(windows))]
const SIGN_CERT: &str = "GhostDesk Local Signing";
#[cfg(not(windows))]
const SIGN_KC_PASS: &str = "ghostdesk";

/// Returns the signing keychain (~/Library/Keychains/ghostdesk-signing.keychain-db), creating it
/// with a new self-signed code-signing certificate if needed.
///
/// The login keychain is not touched, so no password prompt appears. None on failure.
#[cfg(not(windows))]
fn signing_keychain() -> Option<PathBuf> {
    // unit tests never create keychains (they sign ad-hoc)
    if cfg!(test) {
        return None;
    }
    let kc = home().join("Library/Keychains/ghostdesk-signing.keychain-db");
    let t = Duration::from_secs(60);
    if kc.exists() {
        let unlocked = run(Command::new("/usr/bin/security").args(["unlock-keychain", "-p", SIGN_KC_PASS]).arg(&kc), t).is_ok();
        let has_cert = run(Command::new("/usr/bin/security").args(["find-certificate", "-c", SIGN_CERT]).arg(&kc), t).is_ok();
        if unlocked && has_cert {
            return Some(kc);
        }
        let _ = std::fs::remove_file(&kc);
    }
    let work = std::env::temp_dir().join(format!("ghostdesk-sign-{}", std::process::id()));
    let res = (|| -> Result<()> {
        std::fs::create_dir_all(&work)?;
        let cnf = work.join("o.cnf");
        std::fs::write(
            &cnf,
            format!(
                "[req]\ndistinguished_name=dn\nx509_extensions=ext\nprompt=no\n[dn]\nCN={SIGN_CERT}\n[ext]\nbasicConstraints=critical,CA:false\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=critical,codeSigning\n"
            ),
        )?;
        let (key, crt, p12) = (work.join("k.pem"), work.join("c.pem"), work.join("c.p12"));
        run(
            Command::new("/usr/bin/openssl")
                .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "36500", "-config"])
                .arg(&cnf)
                .arg("-keyout")
                .arg(&key)
                .arg("-out")
                .arg(&crt),
            t,
        )?;
        run(
            Command::new("/usr/bin/openssl")
                .args(["pkcs12", "-export", "-passout", "pass:gd", "-inkey"])
                .arg(&key)
                .arg("-in")
                .arg(&crt)
                .arg("-out")
                .arg(&p12),
            t,
        )?;
        let sec = |args: &[&str], extra: Option<&Path>| {
            let mut c = Command::new("/usr/bin/security");
            c.args(args);
            if let Some(e) = extra {
                c.arg(e);
            }
            run(&mut c, t)
        };
        sec(&["create-keychain", "-p", SIGN_KC_PASS], Some(&kc))?;
        sec(&["set-keychain-settings"], Some(&kc))?; // no auto-lock
        sec(&["unlock-keychain", "-p", SIGN_KC_PASS], Some(&kc))?;
        run(
            Command::new("/usr/bin/security")
                .args(["import"])
                .arg(&p12)
                .args(["-k"])
                .arg(&kc)
                .args(["-P", "gd", "-T", "/usr/bin/codesign"]),
            t,
        )?;
        sec(&["set-key-partition-list", "-S", "apple-tool:,apple:,codesign:", "-s", "-k", SIGN_KC_PASS], Some(&kc))?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    if res.is_err() {
        let _ = std::fs::remove_file(&kc);
        return None;
    }
    Some(kc)
}

/// The macOS app icon, put into every bundle `build_bundle` makes.
#[cfg(not(windows))]
const APP_ICON: &[u8] = include_bytes!("../packaging/macos/AppIcon.icns");

/// Builds a fresh GhostDesk.app bundle at `dest` from the single executable `exe` (Info.plist,
/// executable, source fingerprint, signature).
///
/// Shared by installation (when run outside a bundle) and release packaging (`package_macos`).
#[cfg(not(windows))]
pub fn build_bundle(exe: &Path, dest: &Path, local: bool) -> Result<()> {
    let _ = std::fs::remove_dir_all(dest);
    let macos = dest.join("Contents/MacOS");
    std::fs::create_dir_all(&macos)?;
    std::fs::write(dest.join("Contents/Info.plist"), info_plist())?;
    // app icon (drawn by packaging/macos/make-art.swift)
    std::fs::create_dir_all(dest.join("Contents/Resources"))?;
    std::fs::write(dest.join("Contents/Resources/AppIcon.icns"), APP_ICON)?;
    std::fs::copy(exe, macos.join("ghostdesk")).with_context(|| format!("{} 를 복사하지 못했습니다", exe.display()))?;
    seal_bundle(dest, sha256_file(exe).ok().as_deref(), local)
}

/// `ghostdesk --package-macos <executable> <output.zip>`: builds a release GhostDesk.app and zips
/// it.
///
/// The zip contains just GhostDesk.app; in-app updates (`update.rs`) unpack this layout and swap
/// it in.
#[cfg(not(windows))]
pub fn package_macos(args: &[String]) -> Result<()> {
    let [exe, out] = args else { anyhow::bail!("사용법: ghostdesk --package-macos <실행 파일> <출력.zip>") };
    let work = std::env::temp_dir().join(format!("ghostdesk-package-{}", std::process::id()));
    let app = work.join("GhostDesk.app");
    let res = (|| {
        build_bundle(Path::new(exe), &app, false)?;
        let _ = std::fs::remove_file(out);
        run(Command::new("/usr/bin/ditto").args(["-c", "-k", "--keepParent"]).arg(&app).arg(out), Duration::from_secs(300))?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    res
}

/// File inside the installed copy recording "which source executable it was installed from".
///
/// macOS: codesign rewrites the installed executable, so it can't be compared byte for byte with
/// the source. Instead the source's SHA-256 before signing is written here and compared on the
/// next run. Windows: nothing is signed, but the same scheme avoids rehashing the installed copy
/// on every start.
const SOURCE_STAMP: &str = if cfg!(windows) { "ghostdesk.src-sha256" } else { "Contents/Resources/ghostdesk.src-sha256" };

fn source_stamp_path(app: &Path) -> PathBuf {
    app.join(SOURCE_STAMP)
}

/// Whether `app` was installed from source hash `src_hash` (the executable exists and the recorded
/// fingerprint matches).
fn installed_from(app: &Path, src_hash: &str) -> bool {
    installed_exe(app).is_file()
        && std::fs::read_to_string(source_stamp_path(app)).map(|s| s.trim() == src_hash).unwrap_or(false)
}

/// Writes the source fingerprint into the installed copy (on macOS, the staging bundle). On macOS
/// it must be called before codesign to be covered by the signature seal.
fn write_source_stamp(app: &Path, src_hash: &str) -> Result<()> {
    let p = source_stamp_path(app);
    guard_write(&p);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&p, format!("{src_hash}\n"))?;
    Ok(())
}

/// The app bundle's version (CFBundleShortVersionString in Info.plist).
#[cfg(target_os = "macos")]
fn bundle_version(app: &Path) -> Option<String> {
    let plist = std::fs::read_to_string(app.join("Contents/Info.plist")).ok()?;
    plist_string(&plist, "CFBundleShortVersionString")
}

/// The `<string>` value after `<key>{key}</key>` in an XML plist.
#[cfg(not(windows))]
fn plist_string(plist: &str, key: &str) -> Option<String> {
    let rest = &plist[plist.find(&format!("<key>{key}</key>"))?..];
    let start = rest.find("<string>")? + "<string>".len();
    let end = start + rest[start..].find("</string>")?;
    Some(rest[start..end].trim().to_string())
}

/// Makes sure only the installed copy (~/Applications/GhostDesk.app) runs as the GhostDesk app.
///
/// macOS ties Screen Recording and Accessibility to one signed copy, so a second copy running
/// itself (opened where it was unpacked, straight from the .dmg, or dragged into /Applications)
/// would show the permissions as off and its Allow would reset them. Any such copy hands over: if
/// the installed copy is missing or older, it gracefully quits the running old copy and swaps this
/// app in; then it opens the installed copy. The installed copy itself, if still signed ad-hoc, is
/// re-signed with the local certificate once and reopened. Returns true if this process should
/// exit without opening a window.
///
/// Does nothing when running with separate data or when auto-connect is off.
#[cfg(target_os = "macos")]
pub fn take_over_install() -> bool {
    if std::env::var_os("GHOSTDESK_DATA_DIR").is_some() || !auto_connect_enabled() {
        return false;
    }
    let Some(me) = current_bundle() else { return false };
    let target = stable_app();
    // The installed copy itself, still signed ad-hoc by an older version: re-sign it once with the
    // local certificate and reopen, so the permissions granted from now on survive updates.
    // (Granting them from this ad-hoc process would tie them to the old signature again.)
    if me.canonicalize().ok() == target.canonicalize().ok() {
        if signed_with_local_cert(&me) || signing_keychain().is_none() {
            return false;
        }
        if let Err(e) = install_app_inner(true) {
            eprintln!("[ghostdesk] 설치본을 다시 서명하지 못했습니다: {e:#}");
            return false;
        }
        if !signed_with_local_cert(&target) {
            return false;
        }
        return Command::new("/usr/bin/open").arg("-n").arg(&target).status().map(|s| s.success()).unwrap_or(false);
    }
    // Any other copy (unpacked download, mounted .dmg, /Applications, …) never runs itself: the
    // installed copy holds the permissions. If the installed copy is newer it is simply opened.
    let installed_newer = bundle_version(&target).is_some_and(|v| crate::update::newer(&v, env!("CARGO_PKG_VERSION")));
    if !installed_newer {
        if !crate::macos::apps::quit_instances_at(BUNDLE_ID, &target, Duration::from_secs(8)) {
            eprintln!("[ghostdesk] 설치된 GhostDesk 가 종료되지 않아 이 앱을 그대로 엽니다");
            return false;
        }
        if let Err(e) = install_app() {
            eprintln!("[ghostdesk] 설치본을 바꾸지 못했습니다: {e:#}");
            return false;
        }
    }
    // an older install may still carry the quarantine flag; opening it would translocate it again
    // and bring us straight back here
    clear_quarantine(&target);
    Command::new("/usr/bin/open").arg("-n").arg(&target).status().map(|s| s.success()).unwrap_or(false)
}

/// Whether the app or executable at `p` is signed with a certificate (the local one) rather than
/// ad-hoc.
#[cfg(target_os = "macos")]
pub(crate) fn signed_with_local_cert(p: &Path) -> bool {
    Command::new("/usr/bin/codesign")
        .args(["-d", "-r-"])
        .arg(p)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("certificate leaf"))
        .unwrap_or(false)
}

/// Removes the download quarantine flag from an app bundle.
///
/// A quarantined copy that wasn't moved by Finder is run by macOS from a random read-only location
/// (App Translocation), which keeps it out of the Screen Recording list and would make every
/// launch look like a fresh download.
#[cfg(target_os = "macos")]
fn clear_quarantine(app: &Path) {
    let _ = run(Command::new("/usr/bin/xattr").args(["-dr", "com.apple.quarantine"]).arg(app), Duration::from_secs(30));
}

/// Installs (or updates) the running GhostDesk at the fixed location (`stable_app`) and returns
/// its executable path.
///
/// Does nothing if it was already installed from the same source. A running installed copy is
/// updated safely too (macOS: swapped for a new bundle folder; Windows: the running exe is moved
/// aside and the new one put in its place).
pub fn install_app() -> Result<PathBuf> {
    install_app_inner(false)
}

/// `resign`: re-sign the installed copy with the local certificate even if it is this app itself
/// or was installed from the same source (macOS; it keeps the recorded source fingerprint).
fn install_app_inner(resign: bool) -> Result<PathBuf> {
    let me = std::env::current_exe().context("실행 파일 경로를 알 수 없습니다")?;
    #[cfg(windows)]
    {
        let _ = resign;
        install_exe_into(&me, &stable_app())
    }
    #[cfg(not(windows))]
    {
        let target_app = stable_app();
        let target_exe = stable_exe();
        let is_installed_copy = me.canonicalize().ok() == target_exe.canonicalize().ok();
        if is_installed_copy && !resign {
            return Ok(target_exe);
        }
        // compare the fingerprint of the unsigned source (codesign changed the installed copy's bytes)
        let src_hash = if is_installed_copy {
            std::fs::read_to_string(source_stamp_path(&target_app)).ok().map(|s| s.trim().to_string())
        } else {
            sha256_file(&me).ok()
        };
        if let Some(h) = &src_hash {
            if installed_from(&target_app, h) && !resign {
                return Ok(target_exe);
            }
        }
        // don't overwrite a newer installed copy with an older version (e.g. reopening an old download)
        #[cfg(target_os = "macos")]
        if bundle_version(&target_app).is_some_and(|v| crate::update::newer(&v, env!("CARGO_PKG_VERSION"))) {
            return Ok(target_exe);
        }
        let parent = target_app.parent().ok_or_else(|| anyhow!("설치 경로 오류"))?;
        guard_write(parent);
        std::fs::create_dir_all(parent)?;
        let staging = parent.join(".GhostDesk.app.installing");
        let _ = std::fs::remove_dir_all(&staging);
        match current_bundle() {
            Some(bundle) => {
                // --noqtn: don't carry over the downloaded app's quarantine flag (if left, macOS would translocate
                // the installed copy too when opening it)
                run(Command::new("/usr/bin/ditto").arg("--noqtn").arg(&bundle).arg(&staging), Duration::from_secs(120))?;
                // the fingerprint is written before signing (changing Resources after signing breaks the bundle
                // seal)
                clear_quarantine(&staging);
                seal_bundle(&staging, src_hash.as_deref(), true)?;
            }
            None => build_bundle(&me, &staging, true)?,
        }
        // swap: move the existing installed copy aside, then put the new one in place (running processes
        // keep using the old files)
        let old = parent.join(".GhostDesk.app.old");
        let _ = std::fs::remove_dir_all(&old);
        if target_app.exists() {
            std::fs::rename(&target_app, &old).context("기존 설치본을 옮기지 못했습니다")?;
        }
        std::fs::rename(&staging, &target_app).context("새 설치본을 옮기지 못했습니다")?;
        let _ = std::fs::remove_dir_all(&old);
        // register with LaunchServices (so Spotlight and open can find it)
        let _ = run(
            Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
                .arg("-f")
                .arg(&target_app),
            Duration::from_secs(30),
        );
        Ok(target_exe)
    }
}

/// Windows install: puts executable `me` into `dir` as `ghostdesk.exe`.
///
/// The installed copy may be running (the GhostDesk window, or `ghostdesk.exe --mcp` started by
/// Claude/Codex). A running exe can't be deleted or overwritten but can be renamed, so it's moved
/// aside (`ghostdesk.exe.old-<time>`) and the new exe put in its place. The whole folder isn't
/// swapped (a folder holding a running file can't be moved, which would fail the whole connect).
/// The moved-aside old exe is deleted on the next install once its process has exited.
#[cfg(windows)]
fn install_exe_into(me: &Path, dir: &Path) -> Result<PathBuf> {
    let target = installed_exe(dir);
    if target.is_file() && (crate::platform::paths::same_path(me, &target) || is_displaced_copy(me, dir)) {
        // running as the installed copy itself, or as an old copy moved aside by a swap (don't roll the
        // new installed copy back)
        return Ok(target);
    }
    let src_hash = sha256_file(me).ok();
    if let Some(h) = &src_hash {
        if installed_from(dir, h) && same_len(me, &target) {
            sweep_install_leftovers(dir);
            return Ok(target);
        }
    }
    // don't overwrite a newer installed copy with an older version (e.g. rerunning an old download)
    if target.is_file() && installed_version(dir).is_some_and(|v| crate::update::newer(&v, env!("CARGO_PKG_VERSION"))) {
        return Ok(target);
    }
    guard_write(dir);
    std::fs::create_dir_all(dir).with_context(|| format!("설치 폴더 {} 을(를) 만들지 못했습니다", dir.display()))?;
    sweep_install_leftovers(dir);
    // copy into the same folder first (so the swap is just a rename). The pid is appended so it
    // doesn't collide with another GhostDesk installing at the same time.
    let staging = with_suffix(&target, &format!(".installing-{}", std::process::id()));
    let _ = fsx::remove_file_retry(&staging);
    if let Err(e) = std::fs::copy(me, &staging) {
        let _ = std::fs::remove_file(&staging);
        return Err(e).with_context(|| format!("{} 을(를) {} 로 복사하지 못했습니다", me.display(), staging.display()));
    }
    // remove the old fingerprint first (so an interrupted swap reinstalls on the next run)
    let _ = fsx::remove_file_retry(&source_stamp_path(dir));
    if let Err(e) = swap_in(&staging, &target) {
        let _ = std::fs::remove_file(&staging);
        return Err(e);
    }
    // failing to record the fingerprint/version doesn't undo the install (the next run just copies
    // once more)
    if let Some(h) = &src_hash {
        let _ = write_source_stamp(dir, h);
    }
    let _ = std::fs::write(dir.join(VERSION_STAMP), format!("{}\n", env!("CARGO_PKG_VERSION")));
    sweep_install_leftovers(dir);
    Ok(target)
}

/// Puts `staging` in place of `target` (Windows).
#[cfg(windows)]
pub(crate) fn swap_in(staging: &Path, target: &Path) -> Result<()> {
    // if it isn't running, swap in one go (MoveFileExW + REPLACE_EXISTING, atomic)
    let first = match std::fs::rename(staging, target) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    if std::fs::symlink_metadata(target).is_err() {
        // failed with no target present: the new file was briefly locked (antivirus scan, …), so retry
        return fsx::rename_replace(staging, target)
            .with_context(|| format!("새 설치본을 {} 로 옮기지 못했습니다 ({first})", target.display()));
    }
    // running installed copy: just rename it aside (renaming is allowed even for a running exe)
    let old = old_exe_name(target);
    fsx::rename_replace(target, &old)
        .with_context(|| format!("기존 설치본 {} 을(를) 옆으로 옮기지 못했습니다 ({first})", target.display()))?;
    if let Err(e) = fsx::rename_replace(staging, target) {
        // roll back (never leave things without an installed copy)
        let _ = std::fs::rename(&old, target);
        return Err(e).with_context(|| format!("새 설치본을 {} 로 옮기지 못했습니다", target.display()));
    }
    Ok(())
}

/// Name for the old installed copy moved aside: ghostdesk.exe.old-<milliseconds>.
#[cfg(windows)]
fn old_exe_name(target: &Path) -> PathBuf {
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    (0u32..)
        .map(|n| with_suffix(target, &if n == 0 { format!(".old-{ts}") } else { format!(".old-{ts}-{n}") }))
        .find(|p| std::fs::symlink_metadata(p).is_err())
        .unwrap_or_else(|| with_suffix(target, ".old"))
}

/// Installed copy's version, recorded in the install folder (the exe carries no version info).
/// Missing for installs from before 0.4.1.
#[cfg(windows)]
const VERSION_STAMP: &str = "ghostdesk.version";

#[cfg(windows)]
pub(crate) fn installed_version(dir: &Path) -> Option<String> {
    let v = std::fs::read_to_string(dir.join(VERSION_STAMP)).ok()?;
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// When a freshly downloaded GhostDesk exe runs (Windows): if it's a different build from the
/// installed copy (%LOCALAPPDATA%\Programs\GhostDesk\ghostdesk.exe) and the installed copy isn't
/// newer, closes the running old GhostDesk window (like its close button), swaps this exe in and
/// launches the installed copy. Returns true if so (this process then exits without opening a
/// window).
///
/// Does nothing when running as the installed copy, the same build, with separate data, or with
/// auto-connect off.
#[cfg(windows)]
pub fn take_over_install() -> bool {
    if std::env::var_os("GHOSTDESK_DATA_DIR").is_some() || !auto_connect_enabled() {
        return false;
    }
    let Ok(me) = std::env::current_exe() else { return false };
    let dir = stable_app();
    let target = installed_exe(&dir);
    if crate::platform::paths::same_path(&me, &target) || is_displaced_copy(&me, &dir) {
        return false;
    }
    if target.is_file() {
        if installed_version(&dir).is_some_and(|v| !crate::update::newer(env!("CARGO_PKG_VERSION"), &v)) {
            // installed copy is the same or newer: nothing to replace (a different build of the same version
            // gets synced by the regular install on startup)
            return false;
        }
        if sha256_file(&me).ok().is_some_and(|h| installed_from(&dir, &h) && same_len(&me, &target)) {
            return false;
        }
    }
    if !close_gui_windows(Duration::from_secs(8)) {
        eprintln!("[ghostdesk] 떠 있는 GhostDesk 가 닫히지 않아 이 exe 를 그대로 실행합니다");
        return false;
    }
    if let Err(e) = install_exe_into(&me, &dir) {
        eprintln!("[ghostdesk] 설치본을 바꾸지 못했습니다: {e:#}");
        return false;
    }
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    Command::new(&target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .is_ok()
}

/// Sends close to this user's other GhostDesk windows (title "GhostDesk", executable name starting
/// with ghostdesk) and waits up to `wait` for those processes to exit. false if any remain.
#[cfg(windows)]
pub(crate) fn close_gui_windows(wait: Duration) -> bool {
    use crate::platform::win;
    use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
    let me = std::process::id();
    let is_ghostdesk = |pid: u32| {
        win::process_path(pid)
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
            .is_some_and(|n| n.starts_with("ghostdesk") && n.ends_with(".exe"))
    };
    let windows: Vec<win::TopWindow> = win::top_level_hwnds()
        .into_iter()
        .map(win::window_info)
        .filter(|w| w.pid != me && w.title == "GhostDesk" && is_ghostdesk(w.pid))
        .collect();
    if windows.is_empty() {
        return true;
    }
    for w in &windows {
        unsafe { PostMessageW(w.hwnd as _, WM_CLOSE, 0, 0) };
    }
    let deadline = std::time::Instant::now() + wait;
    while std::time::Instant::now() < deadline {
        if windows.iter().all(|w| win::process_path(w.pid).is_none()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// Closes this user's other GhostDesk windows for an install or uninstall, and never waits longer than a
/// fixed time: a window that does not answer (or a Win32 call that blocks on a hung one) must not stall the
/// installer on "closing GhostDesk". After the polite close, windows that are still there are ended.
#[cfg(windows)]
pub(crate) fn stop_running_gui() {
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let polite = close_gui_windows(Duration::from_secs(4));
        let _ = tx.send(polite);
        if !polite {
            end_gui_processes();
        }
    });
    // 4 s polite wait + the forced end; give up after 12 s whatever happens in the helper thread
    let _ = rx.recv_timeout(Duration::from_secs(12));
    std::thread::sleep(Duration::from_millis(300));
}

/// Ends the other GhostDesk processes that still own a "GhostDesk" window.
#[cfg(windows)]
fn end_gui_processes() {
    use crate::platform::win;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    let me = std::process::id();
    let pids: std::collections::BTreeSet<u32> = win::top_level_hwnds()
        .into_iter()
        .map(win::window_info)
        .filter(|w| w.pid != me && w.title == "GhostDesk")
        .filter(|w| {
            win::process_path(w.pid)
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
                .is_some_and(|n| n.starts_with("ghostdesk") && n.ends_with(".exe"))
        })
        .map(|w| w.pid)
        .collect();
    for pid in pids {
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                TerminateProcess(h, 1);
                CloseHandle(h);
            }
        }
    }
}

/// Whether `me` is an old installed copy moved aside in the install folder (ghostdesk.exe.old-*).
#[cfg(windows)]
fn is_displaced_copy(me: &Path, dir: &Path) -> bool {
    let name = me.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    name.starts_with(&format!("{}.old-", crate::platform::paths::EXE_NAME))
        && me.parent().map(|p| crate::platform::paths::same_path(p, dir)).unwrap_or(false)
}

#[cfg(windows)]
fn same_len(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.len() == y.len(),
        _ => false,
    }
}

/// Cleans up files left by earlier installs (Windows, failures ignored): old executables moved
/// aside (still-running ones can't be deleted, so they're retried next time) and interrupted
/// copies.
#[cfg(windows)]
fn sweep_install_leftovers(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let exe = crate::platform::paths::EXE_NAME;
    let old_prefix = format!("{exe}.old");
    let staging_prefix = format!("{exe}.installing-");
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if name.starts_with(&old_prefix) {
            let _ = std::fs::remove_file(e.path());
        } else if let Some(pid) = name.strip_prefix(&staging_prefix) {
            // leave copies another GhostDesk is installing right now alone
            let busy = pid.parse::<u32>().map(|p| p != std::process::id() && crate::platform::proc::pid_alive(p)).unwrap_or(false);
            if !busy {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

// ---- SHA-256 ----
// hand-rolled to avoid another dependency (a fingerprint for deciding whether to install)

pub(crate) struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
    0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Sha256 {
    pub(crate) fn new() -> Self {
        Sha256 {
            h: [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19],
            buf: [0; 64],
            buf_len: 0,
            total: 0,
        }
    }

    fn block(h: &mut [u32; 8], b: &[u8]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut bb, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(SHA256_K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & bb) ^ (a & c) ^ (bb & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = bb;
            bb = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, bb, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }

    pub(crate) fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let n = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + n].copy_from_slice(&data[..n]);
            self.buf_len += n;
            data = &data[n..];
            if self.buf_len < 64 {
                return;
            }
            let buf = self.buf;
            Self::block(&mut self.h, &buf);
            self.buf_len = 0;
        }
        let mut chunks = data.chunks_exact(64);
        for c in &mut chunks {
            Self::block(&mut self.h, c);
        }
        let rest = chunks.remainder();
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    pub(crate) fn finish_hex(mut self) -> String {
        let bits = self.total.wrapping_mul(8);
        let mut pad = vec![0x80u8];
        let used = (self.buf_len + 1) % 64;
        let zeros = if used <= 56 { 56 - used } else { 120 - used };
        pad.extend(std::iter::repeat(0u8).take(zeros));
        pad.extend_from_slice(&bits.to_be_bytes());
        let total = self.total;
        self.update(&pad);
        self.total = total;
        debug_assert_eq!(self.buf_len, 0);
        self.h.iter().map(|x| format!("{x:08x}")).collect()
    }
}

pub(crate) fn sha256_file(p: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finish_hex())
}

// ---- helpers ----

/// Runs a command with a timeout and returns its stdout.
#[cfg(not(windows))]
fn run(cmd: &mut Command, timeout: Duration) -> Result<String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let start = Instant::now();
    loop {
        if let Some(st) = child.try_wait()? {
            let out = child.wait_with_output()?;
            let so = String::from_utf8_lossy(&out.stdout).to_string();
            if st.success() {
                return Ok(so);
            }
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim().to_string() + &so);
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("시간 초과");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Runs a command with a timeout and returns its stdout (Windows).
///
/// - Launched without a console window (CREATE_NO_WINDOW) inside a Job object: on timeout this
///   ends not just cmd.exe but descendants like node too. Leftover descendants are also cleaned up
///   after a normal exit.
/// - Output is read separately (a full pipe would stall the child).
/// - cmd.exe error messages are in the OEM code page (CP949 on Korean Windows), not UTF-8, so
///   they're decoded accordingly.
#[cfg(windows)]
fn run(cmd: &mut Command, timeout: Duration) -> Result<String> {
    use crate::platform::proc;
    use std::sync::mpsc;
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = proc::spawn_group(cmd)?;
    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    if let Some(mut p) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = p.read_to_end(&mut b);
            let _ = tx.send((false, b));
        });
    }
    if let Some(mut p) = child.stderr.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = p.read_to_end(&mut b);
            let _ = tx.send((true, b));
        });
    }
    drop(tx);
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Ok(st),
            Ok(None) if start.elapsed() > timeout => break Err(anyhow!("시간 초과")),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => break Err(e.into()),
        }
    };
    // finished or timed out, close the Job to clean up remaining descendants (which also closes the
    // pipes)
    proc::kill_group(&mut child);
    let (mut so, mut se) = (Vec::new(), Vec::new());
    // don't wait forever if a grandchild that escaped the Job still holds the pipe
    let deadline = Instant::now() + Duration::from_secs(3);
    for _ in 0..2 {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((true, b)) => se = b,
            Ok((false, b)) => so = b,
            Err(_) => break,
        }
    }
    let st = status?;
    let so = decode_output(&so);
    if st.success() {
        return Ok(so);
    }
    bail!("{}", decode_output(&se).trim().to_string() + &so)
}

/// Child process output → string. Non-UTF-8 is decoded with the OEM code page (cmd.exe error
/// messages, …).
#[cfg(windows)]
fn decode_output(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => oem_to_string(b).unwrap_or_else(|| String::from_utf8_lossy(b).into_owned()),
    }
}

#[cfg(windows)]
fn oem_to_string(b: &[u8]) -> Option<String> {
    // declared by hand so the Win32_Globalization feature of windows-sys doesn't have to be enabled
    // for this one function
    #[link(name = "kernel32")]
    extern "system" {
        fn MultiByteToWideChar(codepage: u32, flags: u32, src: *const u8, src_len: i32, dst: *mut u16, dst_len: i32) -> i32;
    }
    const CP_OEMCP: u32 = 1;
    let len = i32::try_from(b.len()).ok().filter(|n| *n > 0)?;
    unsafe {
        let n = MultiByteToWideChar(CP_OEMCP, 0, b.as_ptr(), len, std::ptr::null_mut(), 0);
        if n <= 0 {
            return None;
        }
        let mut w = vec![0u16; n as usize];
        let m = MultiByteToWideChar(CP_OEMCP, 0, b.as_ptr(), len, w.as_mut_ptr(), n);
        if m <= 0 {
            return None;
        }
        w.truncate(m as usize);
        Some(String::from_utf16_lossy(&w))
    }
}

/// PATH for running client CLIs.
///
/// A GUI launched from Finder, the Dock or `open -g` only gets launchd's PATH
/// (/usr/bin:/bin:/usr/sbin:/sbin). Then the claude CLI (npm install), which starts with
/// `#!/usr/bin/env node`, can't find node and fails, so the CLI's own folder (nvm and friends keep
/// node next to it) and common install locations are prepended.
#[cfg(not(windows))]
fn cli_path_env(cli: &Path) -> OsString {
    let b = bases();
    let h = b.home.clone();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = cli.parent() {
        dirs.push(d.to_path_buf());
    }
    dirs.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        h.join(".local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/sbin"),
    ]);
    if let Some(cur) = b.var("PATH") {
        dirs.extend(std::env::split_paths(&cur));
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| !d.as_os_str().is_empty() && seen.insert(d.clone()));
    std::env::join_paths(dirs).unwrap_or_else(|_| OsString::from("/usr/bin:/bin:/usr/sbin:/sbin"))
}

/// PATH for running client CLIs (Windows).
///
/// The CLI's own folder (npm's claude.cmd looks for node.exe in the same folder first) + the
/// inherited PATH + this user's current PATH from the registry (so node installed after GhostDesk
/// started is found, and so it works even when an MCP host launched GhostDesk with a trimmed
/// environment). Unix fixed folders (/usr/bin, …) mean nothing on Windows.
#[cfg(windows)]
fn cli_path_env(cli: &Path) -> OsString {
    let b = bases();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = cli.parent() {
        dirs.push(d.to_path_buf());
    }
    if let Some(cur) = b.var("PATH") {
        dirs.extend(std::env::split_paths(&cur));
    }
    // tests (fake environment) don't mix in the real user environment
    if b.env.is_none() {
        if let Some(env) = crate::platform::win::user_environment() {
            if let Some((_, p)) = env.iter().find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case("PATH")) {
                dirs.extend(std::env::split_paths(p));
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| {
        let key = d.to_string_lossy().to_lowercase().trim_end_matches(['\\', '/']).to_string();
        !key.is_empty() && seen.insert(key)
    });
    std::env::join_paths(&dirs).unwrap_or_else(|_| b.var("PATH").unwrap_or_default())
}

/// Command for running a CLI (with PATH set explicitly).
///
/// Windows: batch files like npm's `claude.cmd` are run by Rust std as
/// `cmd.exe /e:ON /v:OFF /d /c "…"`, quoting and `%`-escaping arguments per cmd rules (Rust
/// 1.77+, the CVE-2024-24576 fix; arguments that can't be passed safely, like newlines, error out
/// instead of running). cmd.exe comes from the system folder, not PATH. No console window is
/// shown.
pub(crate) fn cli_command(cli: &Path) -> Command {
    let mut c = Command::new(cli);
    c.env("PATH", cli_path_env(cli));
    #[cfg(windows)]
    {
        // cmd.exe looks up bare command names (like the "node" that npm's claude.cmd calls) in the current
        // folder before PATH. So that a node.exe/node.cmd planted in the current folder GhostDesk
        // inherited (the Downloads folder it first ran from, the folder --connect ran in, …) never runs,
        // run from the CLI's own folder and turn off current-folder lookup. (cli_path_env puts the CLI
        // folder first in PATH, so programs there are still found.)
        let dir = cli.parent().filter(|d| d.is_dir()).map(Path::to_path_buf).or_else(|| Some(bases().home).filter(|h| h.is_dir()));
        if let Some(dir) = dir {
            c.current_dir(dir);
        }
        c.env("NoDefaultCurrentDirectoryInExePath", "1");
        crate::platform::proc::hide_console(&mut c);
    }
    c
}

#[cfg(not(windows))]
pub(crate) fn find_cli(name: &str) -> Option<PathBuf> {
    let b = bases();
    let h = b.home.clone();
    let mut cands = vec![
        h.join(".local/bin").join(name),
        h.join(format!(".{name}/local/{name}")),
        h.join(format!(".{name}/bin/{name}")), // opencode install script (~/.opencode/bin)
        PathBuf::from("/opt/homebrew/bin").join(name),
        PathBuf::from("/usr/local/bin").join(name),
        PathBuf::from("/Applications/Codex.app/Contents/Resources").join(name),
        PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS").join(name),
    ];
    // launched from a terminal, it may also be on PATH (nvm, ~/.npm-global, bun, …)
    if let Some(cur) = b.var("PATH") {
        cands.extend(std::env::split_paths(&cur).filter(|d| d.is_absolute()).map(|d| d.join(name)));
    }
    // tests never run an actually installed CLI (it would modify the real config)
    #[cfg(test)]
    cands.retain(|p| is_test_path(p));
    cands.into_iter().find(|p| p.is_file())
}

/// Runnable extensions, in preference order. Only those in PATHEXT are considered.
///
/// Extensionless files (the `claude` sh script npm installs alongside) and .ps1 can't be run with
/// CreateProcess, so they're never picked.
#[cfg(windows)]
const CLI_EXTS: [&str; 4] = [".exe", ".cmd", ".bat", ".com"];

/// Windows: looks for `name.exe` → `name.cmd` → `name.bat` in common install locations and PATH.
#[cfg(windows)]
pub(crate) fn find_cli(name: &str) -> Option<PathBuf> {
    let b = bases();
    let pathext = b
        .var("PATHEXT")
        .map(|v| v.to_string_lossy().to_lowercase())
        .filter(|v| !v.trim().is_empty())
        // an MCP host may have trimmed the environment so PATHEXT is missing: use cmd.exe's default
        .unwrap_or_else(|| ".com;.exe;.bat;.cmd".into());
    let exts: Vec<&str> = CLI_EXTS.iter().copied().filter(|e| pathext.split(';').any(|x| x.trim() == *e)).collect();
    let mut dirs = vec![
        b.home.join(".local").join("bin"), // Claude Code default install (claude.exe)
        b.roaming.join("npm"),                                   // npm -g (claude.cmd, codex.cmd)
        b.local.join("Microsoft").join("WinGet").join("Links"),  // winget
        b.home.join("scoop").join("shims"),                      // scoop
        b.local.join("pnpm"),                                    // pnpm -g
        b.home.join(".bun").join("bin"),                         // bun
        b.local.join("Volta").join("bin"),                       // volta
        b.home.join(format!(".{name}")).join("local"), // claude's old local install
        b.home.join(format!(".{name}")).join("bin"), // opencode install script (~/.opencode/bin)
    ];
    if let Some(cur) = b.var("PATH") {
        dirs.extend(std::env::split_paths(&cur).filter(|d| d.is_absolute()));
    }
    for d in dirs {
        for e in &exts {
            let p = d.join(format!("{name}{e}"));
            // tests never run an actually installed CLI (it would modify the real config)
            #[cfg(test)]
            if !is_test_path(&p) {
                continue;
            }
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Sibling path with a suffix appended to `p` (the extension is kept).
fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Real file path with symlinks followed all the way (to preserve dotfile links). If the file
/// doesn't exist, the path the link points to.
///
/// Returns a plain path without the `\\?\` prefix on Windows too.
fn resolve_target(p: &Path) -> PathBuf {
    if let Ok(r) = crate::platform::paths::canonicalize(p) {
        return r;
    }
    // a (dangling) link whose target doesn't exist yet: follow it by hand
    let mut cur = p.to_path_buf();
    for _ in 0..32 {
        match std::fs::read_link(&cur) {
            Ok(t) => {
                cur = if t.is_absolute() { t } else { cur.parent().unwrap_or(Path::new("/")).join(t) };
            }
            Err(_) => break,
        }
    }
    cur
}

/// Permissions of the original file (0600 if missing: config files may hold secrets, so keep it
/// tight).
///
/// Windows has no permission bits (new files inherit the folder's ACL: the user, SYSTEM,
/// Administrators).
fn file_mode(real: &Path) -> u32 {
    #[cfg(unix)]
    {
        std::fs::metadata(real).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o600)
    }
    #[cfg(not(unix))]
    {
        let _ = real;
        0o600
    }
}

/// Creates a new file (Unix: with the given mode; Windows: inheriting the folder's ACL).
fn create_new_with_mode(path: &Path, mode: u32) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    o.mode(mode);
    #[cfg(not(unix))]
    let _ = mode;
    o.open(path)
}

/// Undoes the umask's effect (keeps the original mode). Unix only.
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Reads file contents. None if missing; other errors (permissions, a directory, …) are returned.
fn read_bytes_opt(p: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(p) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("{} 을(를) 읽지 못했습니다", p.display())),
    }
}

fn bytes_to_str<'a>(p: &Path, b: &'a [u8]) -> Result<&'a str> {
    std::str::from_utf8(b).with_context(|| format!("{} 이(가) 올바른 UTF-8 텍스트가 아닙니다 (고치지 않음)", p.display()))
}

/// Leaves the original, from before the first edit, as `.ghostdesk-backup` in the same folder.
///
/// An existing backup is never overwritten (the first original is kept). The backup gets the same
/// permissions as the original.
fn backup(p: &Path) -> Result<()> {
    let b = with_suffix(p, ".ghostdesk-backup");
    if std::fs::symlink_metadata(&b).is_ok() {
        return Ok(());
    }
    let Some(bytes) = read_bytes_opt(p)? else {
        return Ok(());
    };
    guard_write(&b);
    let mode = file_mode(&resolve_target(p));
    let mut f = match create_new_with_mode(&b, mode) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("백업 {} 을(를) 만들지 못했습니다", b.display())),
    };
    f.write_all(&bytes)?;
    f.sync_all()?;
    set_mode(&b, mode)?;
    Ok(())
}

/// Atomically replaces `p` with `contents`.
///
/// - For a symlink, the link stays and the real target file is replaced.
/// - The original permissions are kept (new files get 0600), never widened.
/// - With `expect`, the file is reread right before the swap; if it differs (another program
///   changed it meanwhile), nothing is written and false is returned.
/// - Windows: retries a few times if another program (antivirus, a running client) briefly holds
///   the file.
fn write_atomic(p: &Path, contents: &[u8], expect: Option<&Option<Vec<u8>>>) -> Result<bool> {
    let real = resolve_target(p);
    guard_write(p);
    guard_write(&real);
    let mode = file_mode(&real);
    let tmp = with_suffix(&real, ".ghostdesk-tmp");
    let _ = std::fs::remove_file(&tmp);
    let res = (|| -> Result<bool> {
        {
            let mut f = create_new_with_mode(&tmp, mode)
                .with_context(|| format!("{} 을(를) 만들지 못했습니다", tmp.display()))?;
            f.write_all(contents)?;
            f.sync_all()?;
        }
        // undo the umask's effect (keep the original mode)
        set_mode(&tmp, mode)?;
        if let Some(exp) = expect {
            if read_bytes_opt(p)? != *exp {
                return Ok(false);
            }
        }
        fsx::rename_replace(&tmp, &real).with_context(|| format!("{} 을(를) 바꾸지 못했습니다", real.display()))?;
        Ok(true)
    })();
    if !matches!(res, Ok(true)) {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Reads a config file → modifies it with `edit` → rereads it right away and writes only if it
/// didn't change in between.
///
/// A running Claude Code and the like rewrite the same file frequently, so on a change it starts
/// over with the new contents. `edit` gets the original text (None if the file is missing, BOM
/// stripped) and returns the new text; None means nothing to change. If the original had a UTF-8
/// BOM, the new contents get one too (so programs using the file see the same encoding).
fn update_file(p: &Path, edit: &dyn Fn(Option<&str>) -> Result<Option<String>>) -> Result<()> {
    for _ in 0..5 {
        let orig = read_bytes_opt(p)?;
        let (text, bom) = match &orig {
            Some(b) => {
                let (t, bom) = fsx::strip_bom(bytes_to_str(p, b)?);
                (Some(t), bom)
            }
            None => (None, false),
        };
        let Some(mut new) = edit(text)? else {
            return Ok(());
        };
        if bom {
            new.insert(0, '\u{FEFF}');
        }
        backup(p)?;
        if write_atomic(p, new.as_bytes(), Some(&orig))? {
            return Ok(());
        }
    }
    bail!("{} 이(가) 다른 프로그램에 의해 계속 바뀌고 있어 고치지 못했습니다. 잠시 뒤 다시 시도하세요", p.display())
}

/// Parses JSON text. Empty means {}. Unparseable text is an error (so it isn't overwritten with an
/// empty config).
///
/// A leading UTF-8 BOM is ignored (added by Windows PowerShell 5.1's
/// `Set-Content -Encoding UTF8`, among others).
fn parse_json(p: &Path, text: Option<&str>) -> Result<Value> {
    match text.map(|s| fsx::strip_bom(s).0) {
        None => Ok(json!({})),
        Some(s) if s.trim().is_empty() => Ok(json!({})),
        Some(s) => {
            let v: Value = serde_json::from_str(s).with_context(|| format!("{} 이(가) 올바른 JSON 이 아닙니다 (고치지 않음)", p.display()))?;
            if !v.is_object() {
                bail!("{} 의 형식이 예상과 다릅니다 (최상위가 객체가 아님, 고치지 않음)", p.display());
            }
            Ok(v)
        }
    }
}

fn read_json(p: &Path) -> Result<Value> {
    let b = read_bytes_opt(p)?;
    let s = match &b {
        Some(b) => Some(bytes_to_str(p, b)?),
        None => None,
    };
    parse_json(p, s)
}

/// Sets `<key>.ghostdesk` in a JSON config to `entry` (None removes it). Other keys and their
/// order are untouched.
///
/// `key` is the name of the server list object (mostly "mcpServers"; "mcp" for OpenCode).
fn json_set_server(p: &Path, key: &str, entry: Option<Value>) -> Result<()> {
    update_file(p, &|text| {
        let mut v = parse_json(p, text)?;
        let obj = v.as_object_mut().ok_or_else(|| anyhow!("{} 형식 오류", p.display()))?;
        match &entry {
            Some(e) => {
                let servers = obj.entry(key).or_insert_with(|| json!({}));
                if servers.is_null() {
                    *servers = json!({});
                }
                let m = servers
                    .as_object_mut()
                    .ok_or_else(|| anyhow!("{} 의 {key} 형식이 예상과 다릅니다 (고치지 않음)", p.display()))?;
                if m.get(SERVER_NAME) == Some(e) {
                    return Ok(None);
                }
                m.insert(SERVER_NAME.to_string(), e.clone());
            }
            None => match obj.get_mut(key).and_then(|s| s.as_object_mut()) {
                Some(m) if m.contains_key(SERVER_NAME) => {
                    m.shift_remove(SERVER_NAME);
                }
                _ => return Ok(None),
            },
        }
        Ok(Some(serde_json::to_string_pretty(&v)? + "\n"))
    })
}

/// Parses Codex's config.toml (preserving formatting and comments). Unparseable is an error.
fn parse_toml(p: &Path, text: Option<&str>) -> Result<toml_edit::DocumentMut> {
    fsx::strip_bom(text.unwrap_or(""))
        .0
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("{} 이(가) 올바른 TOML 이 아닙니다 (고치지 않음)", p.display()))
}

/// Only reads Codex's config.toml (BOM ignored). None if missing or unparseable.
fn read_toml(p: &Path) -> Option<toml_edit::DocumentMut> {
    let b = read_bytes_opt(p).ok()??;
    fsx::strip_bom(&String::from_utf8_lossy(&b)).0.parse::<toml_edit::DocumentMut>().ok()
}

/// The [mcp_servers.ghostdesk] table for Codex.
fn codex_table(exe: &str, client_name: &str) -> toml_edit::Table {
    let mut t = toml_edit::Table::new();
    t["command"] = toml_edit::value(exe);
    let mut args = toml_edit::Array::new();
    args.push("--mcp");
    t["args"] = toml_edit::value(args);
    let mut env = toml_edit::InlineTable::new();
    env.insert("GHOSTDESK_CLIENT_NAME", client_name.into());
    t["env"] = toml_edit::value(env);
    // desk_open can exceed Codex's default timeout (60 s) with approval waits, GUI launch and profile
    // cloning
    t["startup_timeout_sec"] = toml_edit::value(CODEX_STARTUP_TIMEOUT_SEC);
    t["tool_timeout_sec"] = toml_edit::value(CODEX_TOOL_TIMEOUT_SEC);
    t
}

/// Whether the registered Codex entry has timeouts (entries registered by older versions don't →
/// rewritten).
fn codex_has_timeouts(p: &Path) -> bool {
    let Some(doc) = read_toml(p) else { return false };
    let Some(t) = doc.get("mcp_servers").and_then(|m| m.get(SERVER_NAME)) else { return false };
    t.get("tool_timeout_sec").and_then(|v| v.as_integer()).map(|n| n >= CODEX_TOOL_TIMEOUT_SEC).unwrap_or(false)
        && t.get("startup_timeout_sec").and_then(|v| v.as_integer()).is_some()
}

fn codex_set_server(p: &Path, entry: Option<toml_edit::Table>) -> Result<()> {
    update_file(p, &|text| {
        let mut doc = parse_toml(p, text)?;
        match &entry {
            Some(t) => {
                if doc.get("mcp_servers").is_none() {
                    let mut m = toml_edit::Table::new();
                    m.set_implicit(true);
                    doc["mcp_servers"] = toml_edit::Item::Table(m);
                }
                let m = doc["mcp_servers"]
                    .as_table_like_mut()
                    .ok_or_else(|| anyhow!("{} 의 mcp_servers 형식이 예상과 다릅니다 (고치지 않음)", p.display()))?;
                m.insert(SERVER_NAME, toml_edit::Item::Table(t.clone()));
            }
            None => match doc.get_mut("mcp_servers").and_then(|i| i.as_table_like_mut()) {
                Some(m) if m.contains_key(SERVER_NAME) => {
                    m.remove(SERVER_NAME);
                }
                _ => return Ok(None),
            },
        }
        Ok(Some(doc.to_string()))
    })
}

// ---- clients ----

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Client {
    ClaudeCode,
    ClaudeApp,
    Codex,
    KimiCode,
    OpenCode,
    /// pi has no built-in MCP support. Registered in ~/.pi/agent/mcp.json, which MCP extensions
    /// (pi-mcp-adapter, …) read.
    Pi,
}

impl Client {
    pub const ALL: [Client; 6] = [Client::ClaudeCode, Client::ClaudeApp, Client::Codex, Client::KimiCode, Client::OpenCode, Client::Pi];
    pub fn name(self) -> &'static str {
        match self {
            Client::ClaudeCode => "Claude Code",
            Client::ClaudeApp => "Claude 앱",
            Client::Codex => "Codex",
            Client::KimiCode => "Kimi Code",
            Client::OpenCode => "OpenCode",
            Client::Pi => "pi",
        }
    }
    /// Config file for clients that are home-relative on every OS (None for the rest).
    ///
    /// - Kimi Code: mcp.json in %KIMI_CODE_HOME% or ~/.kimi-code
    /// - OpenCode: opencode/opencode.json in %XDG_CONFIG_HOME% or ~/.config (or opencode.jsonc if
    ///   only that exists)
    /// - pi: mcp.json in %PI_CODING_AGENT_DIR% or ~/.pi/agent
    fn home_config(self, b: &Bases) -> Option<PathBuf> {
        match self {
            Client::KimiCode => Some(kimi_home(b).join("mcp.json")),
            Client::OpenCode => {
                let d = opencode_dir(b);
                let (json, jsonc) = (d.join("opencode.json"), d.join("opencode.jsonc"));
                Some(if !json.exists() && jsonc.is_file() { jsonc } else { json })
            }
            Client::Pi => Some(pi_agent_dir(b).join("mcp.json")),
            _ => None,
        }
    }
    /// The config file this client reads (the path actually modified on registration).
    ///
    /// - macOS: ~/.claude.json, ~/Library/Application Support/Claude/claude_desktop_config.json,
    ///   ~/.codex/config.toml
    /// - Windows: .claude.json in %CLAUDE_CONFIG_DIR% or %USERPROFILE%, the Claude app data folder
    ///   (`claude_app_config`), config.toml in %CODEX_HOME% or %USERPROFILE%\.codex
    pub fn config(self) -> PathBuf {
        let b = bases();
        if let Some(p) = self.home_config(&b) {
            return p;
        }
        #[cfg(windows)]
        {
            match self {
                Client::ClaudeCode => b.var_path("CLAUDE_CONFIG_DIR").unwrap_or_else(|| b.home.clone()).join(".claude.json"),
                Client::ClaudeApp => claude_app_config(&b),
                _ => codex_home(&b).join("config.toml"),
            }
        }
        #[cfg(not(windows))]
        {
            let h = b.home;
            match self {
                Client::ClaudeCode => h.join(".claude.json"),
                Client::ClaudeApp => h.join("Library/Application Support/Claude/claude_desktop_config.json"),
                _ => h.join(".codex/config.toml"),
            }
        }
    }
    /// Whether it's installed on this computer.
    pub fn installed(self) -> bool {
        let b = bases();
        match self {
            Client::KimiCode => return find_cli("kimi").is_some() || kimi_home(&b).exists(),
            Client::OpenCode => return find_cli("opencode").is_some() || opencode_dir(&b).exists(),
            Client::Pi => return find_cli("pi").is_some() || pi_agent_dir(&b).exists(),
            _ => {}
        }
        #[cfg(windows)]
        {
            match self {
                Client::ClaudeCode => find_cli("claude").is_some() || self.config().exists(),
                Client::ClaudeApp => claude_app_installed(&b),
                // the Codex desktop app (MSIX) uses the same ~/.codex as the CLI
                _ => find_cli("codex").is_some() || codex_home(&b).exists() || !package_ids(&b, "OpenAI.Codex").is_empty(),
            }
        }
        #[cfg(not(windows))]
        {
            match self {
                Client::ClaudeCode => find_cli("claude").is_some() || self.config().exists(),
                Client::ClaudeApp => Path::new("/Applications/Claude.app").exists() || self.config().parent().map(|d| d.exists()).unwrap_or(false),
                _ => find_cli("codex").is_some() || b.home.join(".codex").exists(),
            }
        }
    }
    /// The currently registered ghostdesk command path (None if not registered).
    pub fn registered(self) -> Option<String> {
        registered_in(self, &self.config())
    }
}

/// Codex config folder (Windows): %CODEX_HOME% or %USERPROFILE%\.codex.
#[cfg(windows)]
fn codex_home(b: &Bases) -> PathBuf {
    b.var_path("CODEX_HOME").unwrap_or_else(|| b.home.join(".codex"))
}

/// Kimi Code config folder: %KIMI_CODE_HOME% or ~/.kimi-code.
fn kimi_home(b: &Bases) -> PathBuf {
    b.var_path("KIMI_CODE_HOME").unwrap_or_else(|| b.home.join(".kimi-code"))
}

/// OpenCode global config folder: %XDG_CONFIG_HOME%/opencode or ~/.config/opencode (same rule on
/// Windows).
fn opencode_dir(b: &Bases) -> PathBuf {
    b.var_path("XDG_CONFIG_HOME").unwrap_or_else(|| b.home.join(".config")).join("opencode")
}

/// pi agent folder: %PI_CODING_AGENT_DIR% or ~/.pi/agent.
fn pi_agent_dir(b: &Bases) -> PathBuf {
    b.var_path("PI_CODING_AGENT_DIR").unwrap_or_else(|| b.home.join(".pi").join("agent"))
}

/// Key holding the MCP server list in the JSON config.
fn servers_key(c: Client) -> &'static str {
    if c == Client::OpenCode {
        "mcp"
    } else {
        "mcpServers"
    }
}

/// Publisher IDs of installed MSIX packages `<name>_<publisher ID>` (from %LOCALAPPDATA%\Packages,
/// lowercase, sorted).
#[cfg(windows)]
fn package_ids(b: &Bases, name: &str) -> Vec<String> {
    let prefix = format!("{name}_").to_lowercase();
    let mut ids: Vec<String> = std::fs::read_dir(b.local.join("Packages"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            let id = n.strip_prefix(&prefix)?;
            (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())).then(|| id.to_string())
        })
        .collect();
    ids.sort();
    ids
}

/// Claude app (MSIX) data folders: %LOCALAPPDATA%\Claude-Data\claude_<publisher ID> → (lowercase
/// ID, folder). Sorted by name.
#[cfg(windows)]
fn claude_data_dirs(b: &Bases) -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(b.local.join("Claude-Data"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            let id = n.strip_prefix("claude_")?.to_string();
            (!id.is_empty()).then(|| (id, e.path()))
        })
        .collect();
    v.sort();
    v
}

/// Whether the old Claude data folder (%APPDATA%\Claude) carries the "data moved away" marker
/// (.user-data-left.*).
#[cfg(windows)]
fn has_left_marker(legacy: &Path) -> bool {
    std::fs::read_dir(legacy)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().to_lowercase().starts_with(".user-data-left"))
}

/// Location of the Claude app's claude_desktop_config.json (Windows).
///
/// 1. The current Claude app (MSIX) uses %LOCALAPPDATA%\Claude-Data\claude_<publisher ID>\. Picks
///    the folder whose ID matches the installed package (%LOCALAPPDATA%\Packages\Claude_<ID>) →
///    a folder that already has the config file → the first folder.
/// 2. %APPDATA%\Claude is the old install's folder. Once the MSIX app has moved the data out, only
///    the marker (.user-data-left.*) is left and it's no longer read, so it's only used without
///    the marker (an old install, or not migrated yet, in which case the app moves this file along
///    on first launch).
/// 3. With no data folder yet but the package present (installed, never launched), picks the
///    folder the app will use.
/// 4. With nothing at all, %APPDATA%\Claude (the default location in the official docs).
#[cfg(windows)]
fn claude_app_config(b: &Bases) -> PathBuf {
    const FILE: &str = "claude_desktop_config.json";
    let pkgs = package_ids(b, "Claude");
    let data = claude_data_dirs(b);
    let pick = data
        .iter()
        .find(|(id, _)| pkgs.contains(id))
        .or_else(|| data.iter().find(|(_, d)| d.join(FILE).is_file()))
        .or_else(|| data.first());
    if let Some((_, d)) = pick {
        return d.join(FILE);
    }
    let legacy = b.roaming.join("Claude");
    if legacy.is_dir() && !has_left_marker(&legacy) {
        return legacy.join(FILE);
    }
    if let Some(id) = pkgs.first() {
        return b.local.join("Claude-Data").join(format!("claude_{id}")).join(FILE);
    }
    legacy.join(FILE)
}

/// Whether the Claude app is installed (Windows): MSIX data folder or package, the old installer's
/// (Squirrel) folder, or an old data folder that hasn't been migrated.
#[cfg(windows)]
fn claude_app_installed(b: &Bases) -> bool {
    let legacy = b.roaming.join("Claude");
    b.local.join("Claude-Data").is_dir()
        || !package_ids(b, "Claude").is_empty()
        || b.local.join("AnthropicClaude").is_dir()
        || (legacy.is_dir() && !has_left_marker(&legacy))
}

fn registered_in(c: Client, p: &Path) -> Option<String> {
    match c {
        Client::OpenCode => {
            // in OpenCode, command is an array [executable, args…]
            let v = read_json(p).ok()?;
            v["mcp"][SERVER_NAME]["command"][0].as_str().map(String::from)
        }
        Client::ClaudeCode | Client::ClaudeApp | Client::KimiCode | Client::Pi => {
            let v = read_json(p).ok()?;
            v["mcpServers"][SERVER_NAME]["command"].as_str().map(String::from)
        }
        Client::Codex => {
            let doc = read_toml(p)?;
            doc.get("mcp_servers")?.get(SERVER_NAME)?.get("command")?.as_str().map(String::from)
        }
    }
}

#[derive(Clone, Debug)]
pub struct ClientStatus {
    pub client: Client,
    pub installed: bool,
    pub registered: Option<String>,
}

pub fn status_all() -> Vec<ClientStatus> {
    Client::ALL
        .iter()
        .map(|c| ClientStatus { client: *c, installed: c.installed(), registered: c.registered() })
        .collect()
}

/// The ghostdesk entry for clients with JSON configs.
///
/// Note: Claude Code has no per-server tool timeout field (only the global MCP_TOOL_TIMEOUT
/// environment variable).
fn json_entry(c: Client, exe: &str) -> Value {
    let env = json!({ "GHOSTDESK_CLIENT_NAME": c.name() });
    match c {
        Client::ClaudeCode => json!({ "type": "stdio", "command": exe, "args": ["--mcp"], "env": env }),
        // desk_open can take a while (approval wait, GUI launch), so give it the same timeout as Codex
        Client::KimiCode => json!({
            "command": exe, "args": ["--mcp"], "env": env,
            "startupTimeoutMs": CODEX_STARTUP_TIMEOUT_SEC * 1000,
            "toolTimeoutMs": CODEX_TOOL_TIMEOUT_SEC * 1000,
        }),
        Client::OpenCode => json!({
            "type": "local", "command": [exe, "--mcp"], "environment": env, "enabled": true, "timeout": OPENCODE_TIMEOUT_MS,
        }),
        // pi-mcp-adapter disconnects idle servers after 10 minutes, and GhostDesk closes a connection's
        // screens when it drops, so keep it alive
        Client::Pi => json!({ "command": exe, "args": ["--mcp"], "env": env, "lifecycle": "lazy-keep-alive" }),
        _ => json!({ "command": exe, "args": ["--mcp"], "env": env }),
    }
}

/// Notice after registering pi (pi itself has no MCP).
const PI_NOTE: &str = "pi 에는 MCP 확장이 필요: pi install npm:pi-mcp-adapter";

/// Notice after changing the Claude app's config (on Windows the Claude app stays in the tray
/// after its window closes).
const CLAUDE_APP_RESTART: &str = if cfg!(windows) {
    "Claude 앱을 트레이 아이콘에서 완전히 종료한 뒤 다시 실행하면 적용"
} else {
    "Claude 앱 재시작 후 적용"
};

/// Registers ghostdesk with one client (left as is if the path is already the same).
pub fn register(c: Client, exe: &Path) -> Result<String> {
    let exe_s = exe.display().to_string();
    let up_to_date = match c {
        Client::Codex => codex_has_timeouts(&c.config()),
        // only left as is if the whole entry, timeouts included, matches
        Client::KimiCode | Client::OpenCode | Client::Pi => {
            read_json(&c.config()).map(|v| v[servers_key(c)][SERVER_NAME] == json_entry(c, &exe_s)).unwrap_or(false)
        }
        _ => true,
    };
    if c.registered().as_deref() == Some(exe_s.as_str()) && up_to_date {
        return Ok("이미 연결됨".into());
    }
    let label = format!("GHOSTDESK_CLIENT_NAME={}", c.name());
    match c {
        Client::ClaudeCode => {
            // use the claude CLI first (it safely edits its own config even while Claude Code is running)
            let mut cli_err = None;
            if let Some(cli) = find_cli("claude") {
                if c.registered().is_some() {
                    let _ = run(cli_command(&cli).args(["mcp", "remove", "--scope", "user", SERVER_NAME]), Duration::from_secs(30));
                }
                match run(
                    cli_command(&cli).args(["mcp", "add", "--scope", "user", SERVER_NAME, "-e", &label, "--"]).arg(&exe_s).arg("--mcp"),
                    Duration::from_secs(40),
                ) {
                    Ok(_) => return Ok("claude mcp add 로 연결".into()),
                    // the CLI failed to run (e.g. node not found) → fall back to editing the JSON directly below
                    Err(e) => cli_err = Some(e),
                }
            }
            let p = c.config();
            // the folder set by CLAUDE_CONFIG_DIR may not exist yet
            if let Some(d) = p.parent().filter(|d| !d.exists()) {
                guard_write(d);
                std::fs::create_dir_all(d)?;
            }
            json_set_server(&p, "mcpServers", Some(json_entry(c, &exe_s)))?;
            let shown = show_path(&p);
            Ok(match cli_err {
                Some(e) => format!("{shown} 에 추가 (claude CLI 실패로 직접 수정: {})", first_line(&e.to_string())),
                None => format!("{shown} 에 추가"),
            })
        }
        Client::ClaudeApp => {
            let p = c.config();
            if let Some(d) = p.parent() {
                guard_write(d);
                std::fs::create_dir_all(d)?;
            }
            json_set_server(&p, "mcpServers", Some(json_entry(c, &exe_s)))?;
            // macOS has a single location so just the file name; on Windows it varies by app version, so show
            // the real path
            let shown = if cfg!(windows) { show_path(&p) } else { "claude_desktop_config.json".to_string() };
            Ok(format!("{shown} 에 추가 ({CLAUDE_APP_RESTART})"))
        }
        Client::Codex => {
            // the codex CLI rewrites config.toml dropping comments and can't add timeouts, so only the
            // format-preserving direct edit is used
            let _ = &label;
            let p = c.config();
            if let Some(d) = p.parent() {
                guard_write(d);
                std::fs::create_dir_all(d)?;
            }
            codex_set_server(&p, Some(codex_table(&exe_s, c.name())))?;
            Ok(format!("{} 에 추가", show_path(&p)))
        }
        // all three edit the JSON file directly without a CLI (the kimi/opencode CLIs are interactive or
        // can't set timeouts)
        Client::KimiCode | Client::OpenCode | Client::Pi => {
            let p = c.config();
            if let Some(d) = p.parent() {
                guard_write(d);
                std::fs::create_dir_all(d)?;
            }
            json_set_server(&p, servers_key(c), Some(json_entry(c, &exe_s)))?;
            Ok(match c {
                Client::Pi => format!("{} 에 추가 ({PI_NOTE})", show_path(&p)),
                _ => format!("{} 에 추가", show_path(&p)),
            })
        }
    }
}

fn first_line(s: &str) -> String {
    let l = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    l.chars().take(120).collect()
}

/// Message for an unregister result. Windows also shows the modified config path (macOS keeps the
/// existing wording).
fn removed_msg(p: &Path, note: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if cfg!(windows) {
        parts.push(show_path(p));
    }
    if let Some(n) = note {
        parts.push(n.to_string());
    }
    if parts.is_empty() {
        "해제됨".into()
    } else {
        format!("해제됨 ({})", parts.join(", "))
    }
}

/// Removes the ghostdesk registration from one client.
pub fn unregister(c: Client) -> Result<String> {
    if c.registered().is_none() {
        return Ok("연결되어 있지 않음".into());
    }
    let p = c.config();
    match c {
        Client::ClaudeCode => {
            if let Some(cli) = find_cli("claude") {
                if run(cli_command(&cli).args(["mcp", "remove", "--scope", "user", SERVER_NAME]), Duration::from_secs(30)).is_ok() {
                    return Ok("해제됨".into());
                }
            }
            json_set_server(&p, "mcpServers", None)?;
            Ok(removed_msg(&p, None))
        }
        Client::ClaudeApp => {
            json_set_server(&p, "mcpServers", None)?;
            Ok(removed_msg(&p, Some(CLAUDE_APP_RESTART)))
        }
        Client::Codex => {
            codex_set_server(&p, None)?;
            Ok(removed_msg(&p, None))
        }
        Client::KimiCode | Client::OpenCode | Client::Pi => {
            json_set_server(&p, servers_key(c), None)?;
            Ok(removed_msg(&p, None))
        }
    }
}

/// Installs and registers with every installed client. Returns (client, message) results.
pub fn connect_all() -> Result<(PathBuf, Vec<(Client, Result<String>)>)> {
    let exe = install_app()?;
    let res = Client::ALL
        .iter()
        .filter(|c| c.installed())
        .map(|c| (*c, register(*c, &exe)))
        .collect();
    Ok((exe, res))
}

pub fn disconnect_all() -> Vec<(Client, Result<String>)> {
    Client::ALL.iter().map(|c| (*c, unregister(*c))).collect()
}

/// `ghostdesk --connect` / `--disconnect` (for the terminal).
pub fn cli(args: &[String], connect: bool) -> Result<()> {
    let _ = args;
    if connect {
        let (exe, res) = connect_all()?;
        println!("GhostDesk 설치 위치: {}", exe.display());
        for (c, r) in res {
            match r {
                Ok(m) => println!("✔ {}: {m}", c.name()),
                Err(e) => println!("✘ {}: {e:#}", c.name()),
            }
        }
        println!("이제 에이전트에게 \"GhostDesk 로 …해줘\" 라고 말하면 됩니다.");
    } else {
        for (c, r) in disconnect_all() {
            match r {
                Ok(m) => println!("✔ {}: {m}", c.name()),
                Err(e) => println!("✘ {}: {e:#}", c.name()),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    /// Fake user folders for tests. While alive, this thread's `bases()` points here (the real home,
    /// %APPDATA%, %LOCALAPPDATA% and environment are neither read nor modified).
    struct FakeHome(PathBuf);
    impl FakeHome {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("ghostdesk-install-test-{}-{tag}", std::process::id()));
            let _ = fsx::remove_dir_all_retry(&d);
            std::fs::create_dir_all(&d).unwrap();
            TEST_BASES.with(|b| *b.borrow_mut() = Some(Bases::under(&d)));
            FakeHome(d)
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
        /// Fake environment variables (later values win).
        #[cfg_attr(not(windows), allow(dead_code))]
        fn set_env(&self, k: &str, v: impl Into<OsString>) {
            let v = v.into();
            TEST_BASES.with(|b| b.borrow_mut().as_mut().unwrap().env.as_mut().unwrap().push((k.to_string(), v)));
        }
    }
    impl Drop for FakeHome {
        fn drop(&mut self) {
            TEST_BASES.with(|b| *b.borrow_mut() = None);
            let _ = fsx::remove_dir_all_retry(&self.0);
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn mkdir(p: impl AsRef<Path>) {
        std::fs::create_dir_all(p).unwrap();
    }

    #[cfg(unix)]
    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

    #[cfg(target_os = "macos")]
    #[test]
    fn newer_installed_app_is_not_downgraded() {
        let h = FakeHome::new("no-downgrade");
        let app = h.path("home/Applications/GhostDesk.app");
        mkdir(app.join("Contents/MacOS"));
        std::fs::write(app.join("Contents/MacOS/ghostdesk"), "newer build").unwrap();
        std::fs::write(app.join("Contents/Info.plist"), info_plist().replace(env!("CARGO_PKG_VERSION"), "999.0.0")).unwrap();
        assert_eq!(bundle_version(&app).as_deref(), Some("999.0.0"));
        // a newer installed copy is left as is and its path is used
        assert_eq!(install_app().unwrap(), stable_exe());
        assert_eq!(std::fs::read_to_string(app.join("Contents/MacOS/ghostdesk")).unwrap(), "newer build");
    }

    #[cfg(windows)]
    #[test]
    fn windows_install_records_version_and_never_downgrades() {
        let h = FakeHome::new("win-version");
        let dir = h.path("dir");
        mkdir(&dir);
        let src = h.path("GhostDesk-new.exe");
        std::fs::write(&src, "new build").unwrap();
        install_exe_into(&src, &dir).unwrap();
        assert_eq!(installed_version(&dir).as_deref(), Some(env!("CARGO_PKG_VERSION")));
        // a newer installed copy isn't overwritten by a different (older) exe
        std::fs::write(dir.join(VERSION_STAMP), "999.0.0\n").unwrap();
        let old = h.path("GhostDesk-old.exe");
        std::fs::write(&old, "older build").unwrap();
        assert_eq!(install_exe_into(&old, &dir).unwrap(), installed_exe(&dir));
        assert_eq!(std::fs::read_to_string(installed_exe(&dir)).unwrap(), "new build");
        // old installed copies without a version record are replaced
        std::fs::remove_file(dir.join(VERSION_STAMP)).unwrap();
        install_exe_into(&old, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(installed_exe(&dir)).unwrap(), "older build");
    }

    #[cfg(not(windows))]
    #[test]
    fn reads_plist_strings() {
        let p = info_plist();
        assert_eq!(plist_string(&p, "CFBundleShortVersionString").as_deref(), Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(plist_string(&p, "CFBundleIdentifier").as_deref(), Some(BUNDLE_ID));
        assert_eq!(plist_string(&p, "NoSuchKey"), None);
    }

    #[test]
    fn toml_edit_preserves_other_servers() {
        let src = "model = \"x\"\n\n[mcp_servers.other]\ncommand = \"a\"\n# 주석\n";
        let mut doc = src.parse::<toml_edit::DocumentMut>().unwrap();
        let mut t = toml_edit::Table::new();
        t["command"] = toml_edit::value("/x/ghostdesk");
        doc["mcp_servers"]["ghostdesk"] = toml_edit::Item::Table(t);
        let out = doc.to_string();
        assert!(out.contains("[mcp_servers.other]") && out.contains("# 주석") && out.contains("[mcp_servers.ghostdesk]"));
        let doc2 = out.parse::<toml_edit::DocumentMut>().unwrap();
        doc2["mcp_servers"]["other"]["command"].as_str().unwrap();
    }

    #[test]
    fn json_keeps_key_order() {
        let mut v: Value = serde_json::from_str(r#"{"z":1,"a":{"b":2},"mcpServers":{"x":{}}}"#).unwrap();
        v["mcpServers"]["ghostdesk"] = json!({"command": "c"});
        let s = serde_json::to_string(&v).unwrap();
        assert!(s.find("\"z\"").unwrap() < s.find("\"a\"").unwrap(), "키 순서 보존");
        assert!(s.contains("\"x\":{}"));
    }

    #[test]
    fn sha256_known_vectors() {
        let hx = |d: &[u8]| {
            let mut h = Sha256::new();
            h.update(d);
            h.finish_hex()
        };
        assert_eq!(hx(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hx(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            hx(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(hx(&vec![b'a'; 1_000_000]), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
        // feeding it in pieces gives the same result
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 7 + 3) as u8).collect();
        for step in [1usize, 3, 63, 64, 65, 1000] {
            let mut h = Sha256::new();
            for c in data.chunks(step) {
                h.update(c);
            }
            assert_eq!(h.finish_hex(), hx(&data), "step {step}");
        }
    }

    #[test]
    fn source_stamp_detects_installed_build() {
        let h = FakeHome::new("stamp");
        let src = h.path("build/ghostdesk");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, b"unsigned binary v1").unwrap();
        let app = h.path("Applications/GhostDesk.app");
        let exe = installed_exe(&app);
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        // (macOS) the installed executable's bytes were changed by codesign (they differ from the source)
        std::fs::write(&exe, b"signed binary v1 (different bytes)").unwrap();
        let hash = sha256_file(&src).unwrap();
        assert!(!installed_from(&app, &hash), "지문이 없으면 다시 설치");
        write_source_stamp(&app, &hash).unwrap();
        assert!(installed_from(&app, &hash), "같은 원본이면 다시 설치하지 않음");
        std::fs::write(&src, b"unsigned binary v2").unwrap();
        assert!(!installed_from(&app, &sha256_file(&src).unwrap()), "원본이 바뀌면 다시 설치");
        std::fs::remove_file(&exe).unwrap();
        assert!(!installed_from(&app, &hash), "실행 파일이 없으면 다시 설치");
    }

    #[cfg(unix)]
    #[test]
    fn write_preserves_mode_and_symlink() {
        let h = FakeHome::new("mode");
        // a regular file keeps 0600
        let p = h.path(".claude.json");
        std::fs::write(&p, "{\"a\":1}").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        json_set_server(&p, "mcpServers", Some(json!({"command": "/x/ghostdesk"}))).unwrap();
        assert_eq!(mode_of(&p), 0o600, "권한이 넓어지면 안 됨");
        assert_eq!(read_json(&p).unwrap()["mcpServers"][SERVER_NAME]["command"], "/x/ghostdesk");
        assert_eq!(mode_of(&with_suffix(&p, ".ghostdesk-backup")), 0o600, "백업도 같은 권한");
        assert!(!with_suffix(&p, ".ghostdesk-tmp").exists());

        // other modes like 0640 are kept too (neither narrowed nor widened)
        let q = h.path("other.json");
        std::fs::write(&q, "{}").unwrap();
        std::fs::set_permissions(&q, std::fs::Permissions::from_mode(0o640)).unwrap();
        json_set_server(&q, "mcpServers", Some(json!({"command": "c"}))).unwrap();
        assert_eq!(mode_of(&q), 0o640);

        // new files get 0600
        let n = h.path("new/claude_desktop_config.json");
        std::fs::create_dir_all(n.parent().unwrap()).unwrap();
        json_set_server(&n, "mcpServers", Some(json!({"command": "c"}))).unwrap();
        assert_eq!(mode_of(&n), 0o600);

        // symlink: the link stays, the real target file changes
        let dot = h.path("dotfiles/claude.json");
        std::fs::create_dir_all(dot.parent().unwrap()).unwrap();
        std::fs::write(&dot, "{\"keep\":true}").unwrap();
        std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = h.path("link.json");
        symlink("dotfiles/claude.json", &link).unwrap();
        json_set_server(&link, "mcpServers", Some(json!({"command": "/x/ghostdesk"}))).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "링크가 일반 파일로 바뀌면 안 됨");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&dot).unwrap()).unwrap();
        assert_eq!(v["keep"], true);
        assert_eq!(v["mcpServers"][SERVER_NAME]["command"], "/x/ghostdesk");
        assert_eq!(mode_of(&dot), 0o600);

        // Codex config.toml works the same way
        let c = h.path(".codex/config.toml");
        std::fs::create_dir_all(c.parent().unwrap()).unwrap();
        std::fs::write(&c, "model = \"x\"\n").unwrap();
        std::fs::set_permissions(&c, std::fs::Permissions::from_mode(0o600)).unwrap();
        codex_set_server(&c, Some(codex_table("/x/ghostdesk", "Codex"))).unwrap();
        assert_eq!(mode_of(&c), 0o600);
    }

    #[test]
    fn refuses_unreadable_or_unparsable_config() {
        let h = FakeHome::new("bad");
        let p = h.path(".claude.json");
        let broken = "{\"oauthAccount\": {\"x\": 1}, broken";
        std::fs::write(&p, broken).unwrap();
        assert!(json_set_server(&p, "mcpServers", Some(json!({"command": "c"}))).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), broken, "해석 못 하는 파일은 덮어쓰지 않음");

        // invalid UTF-8
        std::fs::write(&p, b"{\"a\": \"\xff\xfe\"}").unwrap();
        assert!(json_set_server(&p, "mcpServers", Some(json!({"command": "c"}))).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"{\"a\": \"\xff\xfe\"}");

        // top level isn't an object / mcpServers isn't an object → an error, not a panic
        std::fs::write(&p, "[1,2]").unwrap();
        assert!(json_set_server(&p, "mcpServers", Some(json!({"command": "c"}))).is_err());
        std::fs::write(&p, "{\"mcpServers\": \"x\"}").unwrap();
        assert!(json_set_server(&p, "mcpServers", Some(json!({"command": "c"}))).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"mcpServers\": \"x\"}");

        // unreadable path (a directory)
        let d = h.path("dir.json");
        std::fs::create_dir_all(&d).unwrap();
        assert!(json_set_server(&d, "mcpServers", Some(json!({"command": "c"}))).is_err());

        // Codex: unparseable TOML is left as is
        let c = h.path("config.toml");
        std::fs::write(&c, "[[[ not toml").unwrap();
        assert!(codex_set_server(&c, Some(codex_table("/x", "Codex"))).is_err());
        assert!(codex_set_server(&c, None).is_err());
        assert_eq!(std::fs::read_to_string(&c).unwrap(), "[[[ not toml");

        // a missing file starts from an empty config
        let missing = h.path("missing.json");
        json_set_server(&missing, "mcpServers", Some(json!({"command": "c"}))).unwrap();
        assert_eq!(read_json(&missing).unwrap()["mcpServers"][SERVER_NAME]["command"], "c");
    }

    #[test]
    fn backup_keeps_first_original() {
        let h = FakeHome::new("backup");
        let p = h.path(".claude.json");
        let original = "{\"original\": true}";
        std::fs::write(&p, original).unwrap();
        let b = with_suffix(&p, ".ghostdesk-backup");
        json_set_server(&p, "mcpServers", Some(json!({"command": "/a"}))).unwrap();
        assert_eq!(std::fs::read_to_string(&b).unwrap(), original);
        json_set_server(&p, "mcpServers", None).unwrap();
        json_set_server(&p, "mcpServers", Some(json!({"command": "/b"}))).unwrap();
        assert_eq!(std::fs::read_to_string(&b).unwrap(), original, "백업은 처음 원본 그대로");
        // nothing to change → nothing written
        let before = std::fs::read_to_string(&p).unwrap();
        json_set_server(&p, "mcpServers", Some(json!({"command": "/b"}))).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
        // unregistering removes only ghostdesk, other keys stay
        json_set_server(&p, "mcpServers", None).unwrap();
        let v = read_json(&p).unwrap();
        assert_eq!(v["original"], true);
        assert!(v["mcpServers"].get(SERVER_NAME).is_none());
    }

    #[test]
    fn write_atomic_detects_concurrent_change() {
        let h = FakeHome::new("race");
        let p = h.path("f.json");
        std::fs::write(&p, "A").unwrap();
        let stale = Some(b"OLD".to_vec());
        assert!(!write_atomic(&p, b"B", Some(&stale)).unwrap(), "그 사이 바뀌었으면 쓰지 않음");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "A");
        assert!(!with_suffix(&p, ".ghostdesk-tmp").exists());
        let cur = Some(b"A".to_vec());
        assert!(write_atomic(&p, b"B", Some(&cur)).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "B");
    }

    #[test]
    fn codex_entry_has_timeouts() {
        let h = FakeHome::new("codex");
        let c = h.path(".codex/config.toml");
        std::fs::create_dir_all(c.parent().unwrap()).unwrap();
        let src = "# 내 설정\nmodel = \"x\"\n\n[mcp_servers.other]\ncommand = \"a\"\n\n[mcp_servers.ghostdesk]\ncommand = \"/old/ghostdesk\"\n";
        std::fs::write(&c, src).unwrap();
        assert!(!codex_has_timeouts(&c), "예전 항목은 시간 제한이 없음 → 다시 써야 함");
        codex_set_server(&c, Some(codex_table("/x/ghostdesk", "Codex"))).unwrap();
        let out = std::fs::read_to_string(&c).unwrap();
        assert!(out.contains("# 내 설정") && out.contains("[mcp_servers.other]"));
        let doc = out.parse::<toml_edit::DocumentMut>().unwrap();
        let g = &doc["mcp_servers"]["ghostdesk"];
        assert_eq!(g["command"].as_str(), Some("/x/ghostdesk"));
        assert_eq!(g["tool_timeout_sec"].as_integer(), Some(600));
        assert_eq!(g["startup_timeout_sec"].as_integer(), Some(30));
        assert_eq!(g["env"]["GHOSTDESK_CLIENT_NAME"].as_str(), Some("Codex"));
        assert!(codex_has_timeouts(&c));
        assert_eq!(registered_in(Client::Codex, &c).as_deref(), Some("/x/ghostdesk"));
        assert_eq!(std::fs::read_to_string(with_suffix(&c, ".ghostdesk-backup")).unwrap(), src);
        codex_set_server(&c, None).unwrap();
        assert!(registered_in(Client::Codex, &c).is_none());
        assert!(std::fs::read_to_string(&c).unwrap().contains("[mcp_servers.other]"));
    }

    #[cfg(unix)]
    #[test]
    fn cli_runs_with_explicit_path() {
        // a `#!/usr/bin/env fakenode` script: it only runs if PATH finds fakenode in the CLI's folder
        let h = FakeHome::new("path");
        let bin = h.path("nvm/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let node = bin.join("ghostdesk-fakenode");
        std::fs::write(&node, "#!/bin/sh\necho ran-with-node\n").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cli = bin.join("claude");
        std::fs::write(&cli, "#!/usr/bin/env ghostdesk-fakenode\n").unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = cli_path_env(&cli);
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(dirs[0], bin, "CLI 자신의 폴더가 맨 앞");
        for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
            assert!(dirs.contains(&PathBuf::from(d)), "{d} 포함");
        }
        assert!(dirs.contains(&home().join(".local/bin")));
        let out = run(&mut cli_command(&cli), Duration::from_secs(10)).unwrap();
        assert_eq!(out.trim(), "ran-with-node");
        // reproduces the failure without an explicit PATH (the GUI's launchd PATH)
        let fail = run(Command::new(&cli).env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin"), Duration::from_secs(10));
        assert!(fail.is_err());
    }

    // ---- BOM, escaping, full flow (all OSes) ----

    #[test]
    fn bom_is_tolerated_and_preserved() {
        let h = FakeHome::new("bom");
        let exe = r"C:\g\ghostdesk.exe";
        // a .claude.json with a BOM, as written by Windows PowerShell 5.1's Set-Content -Encoding UTF8
        let p = h.path(".claude.json");
        std::fs::write(&p, "\u{FEFF}{\n  \"numStartups\": 3,\n  \"mcpServers\": {\"other\": {\"command\": \"x\"}}\n}").unwrap();
        assert!(read_json(&p).is_ok(), "BOM 이 있어도 읽음");
        json_set_server(&p, "mcpServers", Some(json_entry(Client::ClaudeCode, exe))).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        assert!(bytes.starts_with(&BOM), "BOM 유지");
        assert!(!bytes[3..].starts_with(&BOM), "BOM 이 두 번 붙으면 안 됨");
        let v = read_json(&p).unwrap();
        assert_eq!(v["numStartups"], 3);
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["mcpServers"][SERVER_NAME]["type"], "stdio");
        assert_eq!(registered_in(Client::ClaudeCode, &p).as_deref(), Some(exe));
        assert!(std::fs::read(with_suffix(&p, ".ghostdesk-backup")).unwrap().starts_with(&BOM), "백업은 원본 그대로");
        json_set_server(&p, "mcpServers", None).unwrap();
        assert!(std::fs::read(&p).unwrap().starts_with(&BOM));
        assert!(registered_in(Client::ClaudeCode, &p).is_none());
        assert_eq!(read_json(&p).unwrap()["mcpServers"]["other"]["command"], "x");

        // files without a BOM don't get one
        let q = h.path("plain.json");
        std::fs::write(&q, "{}").unwrap();
        json_set_server(&q, "mcpServers", Some(json!({"command": "c"}))).unwrap();
        assert!(!std::fs::read(&q).unwrap().starts_with(&BOM));

        // Codex config.toml
        let c = h.path("config.toml");
        std::fs::write(&c, "\u{FEFF}# 내 설정\nmodel = \"x\"\n").unwrap();
        codex_set_server(&c, Some(codex_table(exe, "Codex"))).unwrap();
        let out = std::fs::read(&c).unwrap();
        assert!(out.starts_with(&BOM) && !out[3..].starts_with(&BOM));
        assert!(String::from_utf8_lossy(&out).contains("# 내 설정"));
        assert!(codex_has_timeouts(&c));
        assert_eq!(registered_in(Client::Codex, &c).as_deref(), Some(exe));
        codex_set_server(&c, None).unwrap();
        assert!(std::fs::read(&c).unwrap().starts_with(&BOM));
        assert!(registered_in(Client::Codex, &c).is_none());
    }

    #[test]
    fn settings_tolerate_bom() {
        let h = FakeHome::new("settings");
        let p = settings_path();
        assert!(p.starts_with(&h.0));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "\u{FEFF}{\"auto_connect\": false, \"other\": 1}").unwrap();
        assert!(!auto_connect_enabled(), "BOM 이 있어도 '꺼짐' 을 읽음");
        set_auto_connect(true).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.starts_with('\u{FEFF}') && !s[3..].starts_with('\u{FEFF}'), "BOM 유지");
        let v: Value = serde_json::from_str(fsx::strip_bom(&s).0).unwrap();
        assert_eq!(v["auto_connect"], true);
        assert_eq!(v["other"], 1, "다른 설정은 그대로");
        if std::env::var("GHOSTDESK_NO_AUTOCONNECT").as_deref() != Ok("1") {
            assert!(auto_connect_enabled());
        }
        // even a config file that isn't an object is rewritten without panicking
        std::fs::write(&p, "[1, 2]").unwrap();
        set_auto_connect(false).unwrap();
        assert!(!auto_connect_enabled());
    }

    #[test]
    fn windows_paths_are_escaped() {
        let h = FakeHome::new("escape");
        for (i, exe) in [
            r"C:\Users\홍 길동\AppData\Local\Programs\GhostDesk\ghostdesk.exe",
            r"C:\Users\O'Brien & Co (x)\AppData\Local\Programs\GhostDesk\ghostdesk.exe",
            // misreading A, \n or \t as escapes would yield a different path
            r"\\server\share\u0041\new\tools\ghostdesk.exe",
        ]
        .into_iter()
        .enumerate()
        {
            let p = h.path(&format!("claude_desktop_config-{i}.json"));
            json_set_server(&p, "mcpServers", Some(json_entry(Client::ClaudeApp, exe))).unwrap();
            let raw = std::fs::read_to_string(&p).unwrap();
            let v: Value = serde_json::from_str(&raw).expect("올바른 JSON");
            assert_eq!(v["mcpServers"][SERVER_NAME]["command"].as_str(), Some(exe));
            assert!(raw.contains(r"\\"), "역슬래시가 이스케이프되어야 함: {raw}");
            assert_eq!(registered_in(Client::ClaudeApp, &p).as_deref(), Some(exe));

            let c = h.path(&format!("config-{i}.toml"));
            codex_set_server(&c, Some(codex_table(exe, "Codex"))).unwrap();
            let raw = std::fs::read_to_string(&c).unwrap();
            let doc = raw.parse::<toml_edit::DocumentMut>().expect("올바른 TOML");
            assert_eq!(doc["mcp_servers"][SERVER_NAME]["command"].as_str(), Some(exe), "{raw}");
            assert_eq!(registered_in(Client::Codex, &c).as_deref(), Some(exe));
        }
    }

    /// Full register → check → unregister flow inside fake user folders (config files are edited
    /// directly when no CLI is installed).
    #[test]
    fn register_end_to_end_in_sandbox() {
        let h = FakeHome::new("e2e");
        let exe = stable_exe();
        assert!(exe.starts_with(&h.0));
        for c in Client::ALL {
            let p = c.config();
            assert!(p.starts_with(&h.0), "{p:?}");
            assert!(c.registered().is_none());
            let msg = register(c, &exe).unwrap();
            assert_eq!(c.registered().as_deref(), Some(exe.display().to_string().as_str()), "{c:?}");
            // the message shows the file actually modified
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            assert!(msg.contains(&name), "{msg}");
            #[cfg(windows)]
            assert!(msg.contains(&p.display().to_string()), "Windows 는 전체 경로: {msg}");
            assert_eq!(register(c, &exe).unwrap(), "이미 연결됨");
            let m = unregister(c).unwrap();
            assert!(m.starts_with("해제됨"), "{m}");
            #[cfg(windows)]
            assert!(m.contains(&p.display().to_string()), "{m}");
            assert!(c.registered().is_none());
            assert_eq!(unregister(c).unwrap(), "연결되어 있지 않음");
        }
        // Claude Code entries have type: stdio, Codex ones include timeouts
        register(Client::ClaudeCode, &exe).unwrap();
        assert_eq!(read_json(&Client::ClaudeCode.config()).unwrap()["mcpServers"][SERVER_NAME]["type"], "stdio");
        register(Client::Codex, &exe).unwrap();
        assert!(codex_has_timeouts(&Client::Codex.config()));
        let st = status_all();
        assert!(st.iter().any(|s| s.client == Client::Codex && s.installed && s.registered.is_some()));
    }

    /// Kimi Code, OpenCode, pi: location, entry shape, other settings preserved, environment
    /// variables.
    #[test]
    fn kimi_opencode_pi_entries() {
        let h = FakeHome::new("harness");
        let exe = Path::new("/x/ghostdesk");
        let home = bases().home;
        assert_eq!(Client::KimiCode.config(), home.join(".kimi-code").join("mcp.json"));
        assert_eq!(Client::OpenCode.config(), home.join(".config").join("opencode").join("opencode.json"));
        assert_eq!(Client::Pi.config(), home.join(".pi").join("agent").join("mcp.json"));
        assert!(!Client::KimiCode.installed() && !Client::OpenCode.installed() && !Client::Pi.installed());

        // OpenCode: if only opencode.jsonc exists, that file is edited and other settings stay
        let oc = home.join(".config").join("opencode");
        mkdir(&oc);
        std::fs::write(oc.join("opencode.jsonc"), r#"{"$schema":"https://opencode.ai/config.json","provider":{"p":{}}}"#).unwrap();
        assert!(Client::OpenCode.installed());
        assert_eq!(Client::OpenCode.config(), oc.join("opencode.jsonc"));
        register(Client::OpenCode, exe).unwrap();
        let v = read_json(&oc.join("opencode.jsonc")).unwrap();
        assert_eq!(v["provider"], json!({"p":{}}));
        let g = &v["mcp"][SERVER_NAME];
        assert_eq!((g["type"].as_str(), g["command"].clone()), (Some("local"), json!(["/x/ghostdesk", "--mcp"])));
        assert_eq!(g["environment"]["GHOSTDESK_CLIENT_NAME"], "OpenCode");
        assert_eq!(Client::OpenCode.registered().as_deref(), Some("/x/ghostdesk"));
        // JSONC with comments can't be parsed, so it isn't overwritten
        std::fs::write(oc.join("opencode.jsonc"), "{ // 주석\n}").unwrap();
        assert!(register(Client::OpenCode, Path::new("/y")).is_err());
        assert_eq!(std::fs::read_to_string(oc.join("opencode.jsonc")).unwrap(), "{ // 주석\n}");

        // Kimi: timeouts; old entries (without timeouts) are rewritten
        let kp = Client::KimiCode.config();
        mkdir(kp.parent().unwrap());
        std::fs::write(&kp, r#"{"mcpServers":{"other":{"command":"a"},"ghostdesk":{"command":"/x/ghostdesk"}}}"#).unwrap();
        assert_ne!(register(Client::KimiCode, exe).unwrap(), "이미 연결됨");
        let v = read_json(&kp).unwrap();
        assert_eq!(v["mcpServers"]["other"]["command"], "a");
        assert_eq!(v["mcpServers"][SERVER_NAME]["toolTimeoutMs"], CODEX_TOOL_TIMEOUT_SEC * 1000);
        assert_eq!(register(Client::KimiCode, exe).unwrap(), "이미 연결됨");

        // pi: keep-alive + extension notice
        assert!(register(Client::Pi, exe).unwrap().contains("pi-mcp-adapter"));
        assert_eq!(read_json(&Client::Pi.config()).unwrap()["mcpServers"][SERVER_NAME]["lifecycle"], "lazy-keep-alive");

        // location changed through an environment variable
        h.set_env("KIMI_CODE_HOME", h.path("k"));
        h.set_env("XDG_CONFIG_HOME", h.path("xdg"));
        h.set_env("PI_CODING_AGENT_DIR", h.path("pi"));
        assert_eq!(Client::KimiCode.config(), h.path("k").join("mcp.json"));
        assert_eq!(Client::OpenCode.config(), h.path("xdg").join("opencode").join("opencode.json"));
        assert_eq!(Client::Pi.config(), h.path("pi").join("mcp.json"));
    }

    /// Tests never touch real user files.
    #[test]
    fn tests_never_touch_real_user_files() {
        // 1) threads without fake folders still see an empty temp folder, not the real folders or
        // environment
        std::thread::spawn(|| {
            let mut paths = vec![stable_app(), stable_exe(), settings_path()];
            paths.extend(Client::ALL.iter().map(|c| c.config()));
            for p in &paths {
                assert!(is_test_path(p), "{p:?}");
            }
            if let Some(real) = dirs::home_dir() {
                for p in [real.join(".claude.json"), real.join(".codex").join("config.toml")] {
                    assert!(!paths.contains(&p));
                }
            }
            assert!(bases().var("PATH").is_none(), "실제 환경변수도 보지 않음");
            assert!(find_cli("claude").is_none() && find_cli("codex").is_none(), "실제로 설치된 CLI 를 실행하지 않음");
        })
        .join()
        .unwrap();

        // 2) writing outside the temp folder stops before anything is written (the path has no parent
        // folder, so even if it weren't caught, no file would actually be created)
        let outside = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")).join("ghostdesk-test-must-not-exist").join("x.json");
        assert!(!is_test_path(&outside));
        let o = outside.clone();
        assert!(std::thread::spawn(move || write_atomic(&o, b"{}", None)).join().is_err(), "write_atomic 은 막혀야 함");
        let o = outside.clone();
        assert!(std::thread::spawn(move || json_set_server(&o, "mcpServers", Some(json!({})))).join().is_err());
        let o = outside.clone();
        assert!(std::thread::spawn(move || codex_set_server(&o, Some(codex_table("x", "Codex")))).join().is_err());
        let o = outside.clone();
        assert!(std::thread::spawn(move || {
            let mut b = Bases::under(&std::env::temp_dir().join("ghostdesk-unused"));
            b.settings = o;
            TEST_BASES.with(|t| *t.borrow_mut() = Some(b));
            set_auto_connect(true)
        })
        .join()
        .is_err());
        assert!(!outside.parent().unwrap().exists());
    }

    // ───────── Windows ─────────

    #[cfg(windows)]
    fn system32() -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into())).join("System32")
    }

    #[cfg(windows)]
    #[test]
    fn windows_install_location() {
        let h = FakeHome::new("loc");
        let b = bases();
        assert_eq!(stable_app(), b.local.join("Programs").join("GhostDesk"));
        assert_eq!(stable_exe(), stable_app().join("ghostdesk.exe"));
        assert!(stable_exe().starts_with(&h.0));
        assert_eq!(source_stamp_path(&stable_app()), stable_app().join("ghostdesk.src-sha256"));
    }

    #[cfg(windows)]
    #[test]
    fn claude_app_config_resolution() {
        const F: &str = "claude_desktop_config.json";
        // nothing at all → not installed, default location is %APPDATA%\Claude
        {
            let _h = FakeHome::new("capp0");
            let b = bases();
            assert!(!Client::ClaudeApp.installed());
            assert_eq!(Client::ClaudeApp.config(), b.roaming.join("Claude").join(F));
        }
        // the current MSIX app: the Claude-Data folder matching the package ID (even if the config file is
        // in another folder)
        {
            let _h = FakeHome::new("capp1");
            let b = bases();
            let data = b.local.join("Claude-Data");
            mkdir(b.local.join("Packages").join("Claude_pzs8sxrjxfjjc"));
            mkdir(b.local.join("Packages").join("ClaudeHelper_aaaaaaaaaaaaa"));
            mkdir(data.join("claude_aaaaaaaaaaaaa"));
            std::fs::write(data.join("claude_aaaaaaaaaaaaa").join(F), "{}").unwrap();
            mkdir(data.join("claude_pzs8sxrjxfjjc"));
            std::fs::write(data.join("README.txt"), "x").unwrap();
            let legacy = b.roaming.join("Claude");
            mkdir(&legacy);
            std::fs::write(legacy.join(".user-data-left.claude_pzs8sxrjxfjjc"), "x").unwrap();
            assert!(Client::ClaudeApp.installed());
            let want = data.join("claude_pzs8sxrjxfjjc").join(F);
            assert_eq!(Client::ClaudeApp.config(), want);
            // registering creates only that file, leaving the old folder and other data folders alone
            let exe = stable_exe();
            let msg = register(Client::ClaudeApp, &exe).unwrap();
            assert!(msg.contains(&want.display().to_string()) && msg.contains("트레이"), "{msg}");
            assert_eq!(Client::ClaudeApp.registered().as_deref(), Some(exe.display().to_string().as_str()));
            assert!(!legacy.join(F).exists());
            assert_eq!(std::fs::read_to_string(data.join("claude_aaaaaaaaaaaaa").join(F)).unwrap(), "{}");
        }
        // data folders without package info: the folder with the config file → else the first by name
        {
            let _h = FakeHome::new("capp2");
            let b = bases();
            let data = b.local.join("Claude-Data");
            mkdir(data.join("claude_aaa"));
            mkdir(data.join("claude_bbb"));
            mkdir(data.join("other_ccc"));
            assert!(Client::ClaudeApp.installed());
            assert_eq!(Client::ClaudeApp.config(), data.join("claude_aaa").join(F));
            std::fs::write(data.join("claude_bbb").join(F), "{}").unwrap();
            assert_eq!(Client::ClaudeApp.config(), data.join("claude_bbb").join(F));
        }
        // old installer (Squirrel): %APPDATA%\Claude (no marker)
        {
            let _h = FakeHome::new("capp3");
            let b = bases();
            mkdir(b.local.join("AnthropicClaude"));
            assert!(Client::ClaudeApp.installed());
            mkdir(b.roaming.join("Claude"));
            assert_eq!(Client::ClaudeApp.config(), b.roaming.join("Claude").join(F));
        }
        // MSIX installed but data not migrated yet (no marker in the old folder) → old folder (moved along
        // on first launch)
        {
            let _h = FakeHome::new("capp4");
            let b = bases();
            mkdir(b.local.join("Packages").join("Claude_xyz0123456789"));
            mkdir(b.roaming.join("Claude"));
            assert_eq!(Client::ClaudeApp.config(), b.roaming.join("Claude").join(F));
        }
        // installed but never launched (no data folder, only the marker in the old folder) → the
        // Claude-Data folder the app will create
        {
            let _h = FakeHome::new("capp5");
            let b = bases();
            mkdir(b.local.join("Packages").join("Claude_xyz0123456789"));
            let legacy = b.roaming.join("Claude");
            mkdir(&legacy);
            std::fs::write(legacy.join(".user-data-left.claude_xyz0123456789"), "x").unwrap();
            assert!(Client::ClaudeApp.installed());
            assert_eq!(Client::ClaudeApp.config(), b.local.join("Claude-Data").join("claude_xyz0123456789").join(F));
        }
        // an old folder with only the marker doesn't count as installed
        {
            let _h = FakeHome::new("capp6");
            let b = bases();
            let legacy = b.roaming.join("Claude");
            mkdir(&legacy);
            std::fs::write(legacy.join(".user-data-left.claude_x"), "x").unwrap();
            assert!(!Client::ClaudeApp.installed());
        }
    }

    #[cfg(windows)]
    #[test]
    fn env_overrides_for_config_dirs() {
        {
            let h = FakeHome::new("envdirs");
            let b = bases();
            assert_eq!(Client::ClaudeCode.config(), b.home.join(".claude.json"));
            assert_eq!(Client::Codex.config(), b.home.join(".codex").join("config.toml"));
            assert!(!Client::Codex.installed());
            // empty values are ignored
            h.set_env("CLAUDE_CONFIG_DIR", "");
            h.set_env("CODEX_HOME", "");
            assert_eq!(Client::ClaudeCode.config(), b.home.join(".claude.json"));
            assert_eq!(Client::Codex.config(), b.home.join(".codex").join("config.toml"));
            let cc = h.path("claude cfg");
            let cx = h.path("codex home");
            h.set_env("claude_config_dir", cc.as_os_str()); // Windows environment variable names are case-insensitive
            h.set_env("CODEX_HOME", cx.as_os_str());
            assert_eq!(Client::ClaudeCode.config(), cc.join(".claude.json"));
            assert_eq!(Client::Codex.config(), cx.join("config.toml"));
            // a folder that doesn't exist yet is created and registered
            let exe = stable_exe();
            register(Client::ClaudeCode, &exe).unwrap();
            register(Client::Codex, &exe).unwrap();
            assert!(cc.join(".claude.json").is_file() && cx.join("config.toml").is_file());
            assert!(Client::Codex.installed() && Client::ClaudeCode.installed());
            assert!(!b.home.join(".claude.json").exists() && !b.home.join(".codex").exists());
        }
        // the Codex desktop app (MSIX) alone counts as installed
        {
            let _h = FakeHome::new("envdirs-msix");
            assert!(!Client::Codex.installed());
            mkdir(bases().local.join("Packages").join("OpenAI.Codex_2p2nqsd0c76g0"));
            assert!(Client::Codex.installed());
        }
    }

    #[cfg(windows)]
    #[test]
    fn find_cli_is_pathext_aware() {
        let h = FakeHome::new("pathext");
        let b = bases();
        let npm = b.roaming.join("npm");
        mkdir(&npm);
        // the extensionless sh script and PowerShell script npm installs alongside can't be run
        std::fs::write(npm.join("claude"), "#!/bin/sh\n").unwrap();
        std::fs::write(npm.join("claude.ps1"), "").unwrap();
        assert_eq!(find_cli("claude"), None);
        std::fs::write(npm.join("claude.cmd"), "@echo off\r\n").unwrap();
        assert_eq!(find_cli("claude"), Some(npm.join("claude.cmd")));
        assert!(Client::ClaudeCode.installed());
        // in the same folder, .exe comes first, and .cmd before .bat
        std::fs::write(npm.join("claude.bat"), "").unwrap();
        assert_eq!(find_cli("claude"), Some(npm.join("claude.cmd")));
        std::fs::write(npm.join("claude.exe"), "").unwrap();
        assert_eq!(find_cli("claude"), Some(npm.join("claude.exe")));
        std::fs::remove_file(npm.join("claude.exe")).unwrap();
        // the default install (~/.local/bin) comes before npm
        let local_bin = b.home.join(".local").join("bin");
        mkdir(&local_bin);
        std::fs::write(local_bin.join("claude.exe"), "").unwrap();
        assert_eq!(find_cli("claude"), Some(local_bin.join("claude.exe")));
        std::fs::remove_file(local_bin.join("claude.exe")).unwrap();
        // extensions not in PATHEXT aren't picked
        h.set_env("PATHEXT", ".COM;.EXE");
        assert_eq!(find_cli("claude"), None);
        h.set_env("PATHEXT", ".COM;.EXE;.BAT;.CMD;.VBS;.JS");
        assert_eq!(find_cli("claude"), Some(npm.join("claude.cmd")));
        // only on PATH (a quoted entry, a folder with spaces)
        let custom = h.path("my tools");
        mkdir(&custom);
        std::fs::write(custom.join("codex.bat"), "@echo off\r\n").unwrap();
        mkdir(custom.join("codex.exe")); // a folder isn't an executable
        assert_eq!(find_cli("codex"), None);
        h.set_env("Path", format!("relative\\dir;\"{}\"", custom.display()));
        assert_eq!(find_cli("codex"), Some(custom.join("codex.bat")));
        assert!(Client::Codex.installed());
    }

    #[cfg(windows)]
    #[test]
    fn cli_path_env_windows() {
        let h = FakeHome::new("winpath");
        let cli = h.path("npm").join("claude.cmd");
        h.set_env("Path", r"C:\A;C:\B\;c:\a;;C:\npm-dup");
        let dirs: Vec<PathBuf> = std::env::split_paths(&cli_path_env(&cli)).collect();
        assert_eq!(dirs[0], h.path("npm"), "CLI 자신의 폴더가 맨 앞");
        // inherited PATH (deduplicated case-insensitively). Unix folders (/usr/bin, …) aren't added
        assert_eq!(dirs[1..], [PathBuf::from(r"C:\A"), PathBuf::from(r"C:\B\"), PathBuf::from(r"C:\npm-dup")]);
    }

    /// Arguments reach batch files like npm's claude.cmd intact (paths with spaces, &, parentheses
    /// and %, Korean folder names).
    #[cfg(windows)]
    #[test]
    fn cmd_shim_receives_args_intact() {
        let h = FakeHome::new("cmdq");
        let dir = h.path("npm 폴더 & (x)");
        mkdir(&dir);
        let cli = dir.join("claude.cmd");
        // write the received argument line (%*) to a file as is (npm's claude.cmd passes this %* to node)
        std::fs::write(&cli, "@echo off\r\n>\"%~dp0args.txt\" echo(%*\r\necho ran-ok\r\n").unwrap();
        assert_eq!(find_cli("claude"), None, "가짜 PATH 에 없으면 못 찾음");
        h.set_env("PATH", dir.as_os_str());
        assert_eq!(find_cli("claude"), Some(cli.clone()));
        let exe = r"C:\Users\A&B (x) 100%\AppData\Local\Programs\GhostDesk\ghostdesk.exe";
        let label = "GHOSTDESK_CLIENT_NAME=Claude Code";
        let out = run(
            cli_command(&cli).args(["mcp", "add", "--scope", "user", SERVER_NAME, "-e", label, "--"]).arg(exe).arg("--mcp").arg("%PATH%"),
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(out.trim(), "ran-ok");
        let got = decode_output(&std::fs::read(dir.join("args.txt")).unwrap());
        assert_eq!(
            got.trim_end(),
            format!("mcp add --scope user ghostdesk -e \"{label}\" -- \"{exe}\" --mcp \"%PATH%\""),
            "환경변수가 펼쳐지거나 따옴표가 깨지면 안 됨"
        );
    }

    /// On timeout, the CLI and all its descendants (cmd → node, …) are ended, without hanging on the
    /// output pipe.
    #[cfg(windows)]
    #[test]
    fn cli_timeout_kills_whole_tree() {
        let h = FakeHome::new("cmdtimeout");
        h.set_env("PATH", system32().as_os_str());
        let dir = h.path("bin");
        mkdir(&dir);
        let cli = dir.join("claude.cmd");
        // mimics a CLI that spawns a separate grandchild and waits a long time. If the grandchild
        // survives, it leaves leak.txt after 3 s.
        std::fs::write(
            &cli,
            "@echo off\r\ncd /d \"%~dp0\"\r\nstart \"\" /b cmd /d /c \"ping -n 4 127.0.0.1 >NUL & echo leaked>leak.txt\"\r\nping -n 30 127.0.0.1\r\necho finished>finished.txt\r\n",
        )
        .unwrap();
        let t = Instant::now();
        let e = run(&mut cli_command(&cli), Duration::from_millis(1500)).unwrap_err();
        assert!(e.to_string().contains("시간 초과"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(8), "파이프를 기다리며 멈춤: {:?}", t.elapsed());
        std::thread::sleep(Duration::from_secs(5));
        assert!(!dir.join("leak.txt").exists(), "손자 프로세스가 남아 있음");
        assert!(!dir.join("finished.txt").exists());
    }

    /// The CLI runs from its own folder, and cmd.exe doesn't look up bare command names in the current
    /// folder (so a node.cmd planted in the current folder GhostDesk inherited never runs).
    #[cfg(windows)]
    #[test]
    fn cli_runs_in_own_folder_without_cwd_lookup() {
        let h = FakeHome::new("cmdcwd");
        h.set_env("PATH", system32().as_os_str());
        let dir = h.path("bin");
        mkdir(&dir);
        let other = h.path("downloads");
        mkdir(&other);
        std::fs::write(other.join("ghostdesk-planted-node.cmd"), "@echo planted-ran\r\n").unwrap();
        std::fs::write(dir.join("ghostdesk-own-tool.cmd"), "@echo own-ran\r\n").unwrap();
        let cli = dir.join("claude.cmd");
        std::fs::write(
            &cli,
            format!(
                "@echo off\r\necho cwd=%CD%\r\ncall ghostdesk-own-tool\r\ncd /d \"{}\"\r\ncall ghostdesk-planted-node\r\nexit /b 0\r\n",
                other.display()
            ),
        )
        .unwrap();
        let c = cli_command(&cli);
        assert_eq!(c.get_current_dir(), Some(dir.as_path()));
        assert!(c
            .get_envs()
            .any(|(k, v)| k == "NoDefaultCurrentDirectoryInExePath" && v == Some(std::ffi::OsStr::new("1"))));
        let out = run(&mut cli_command(&cli), Duration::from_secs(30)).unwrap();
        assert!(out.to_lowercase().contains(&format!("cwd={}", dir.display()).to_lowercase()), "{out}");
        // programs in the CLI's own folder (first in PATH) are still found
        assert!(out.contains("own-ran"), "{out}");
        // programs only in the current folder don't run
        assert!(!out.contains("planted-ran"), "{out}");
    }

    /// Error message of a failed CLI (cmd.exe writes in the OEM code page).
    #[cfg(windows)]
    #[test]
    fn cli_errors_are_decoded() {
        let h = FakeHome::new("cmderr");
        h.set_env("PATH", system32().as_os_str());
        let dir = h.path("bin");
        mkdir(&dir);
        let cli = dir.join("claude.cmd");
        std::fs::write(&cli, "@echo off\r\nghostdesk-no-such-command-xyz\r\nexit /b 9\r\n").unwrap();
        let e = run(&mut cli_command(&cli), Duration::from_secs(30)).unwrap_err().to_string();
        assert!(e.contains("ghostdesk-no-such-command-xyz"), "{e}");
        assert!(!e.contains('\u{FFFD}'), "깨진 글자: {e}");
        assert_eq!(decode_output("한글 ok".as_bytes()), "한글 ok");
        #[link(name = "kernel32")]
        extern "system" {
            fn GetOEMCP() -> u32;
        }
        if unsafe { GetOEMCP() } == 949 {
            assert_eq!(decode_output(&[0xC7, 0xD1, 0xB1, 0xDB]), "한글");
        }
    }

    /// Editing a config file while another program has it open.
    #[cfg(windows)]
    #[test]
    fn windows_replace_while_file_is_open() {
        use std::os::windows::fs::OpenOptionsExt;
        let h = FakeHome::new("winopen");
        let p = h.path(".claude.json");
        std::fs::write(&p, "{\"a\":1}").unwrap();
        // opened the usual way (delete sharing allowed: the Node and Rust default)
        {
            let _reader = std::fs::File::open(&p).unwrap();
            json_set_server(&p, "mcpServers", Some(json!({"command": "c"}))).unwrap();
        }
        assert_eq!(read_json(&p).unwrap()["mcpServers"][SERVER_NAME]["command"], "c");
        assert_eq!(read_json(&p).unwrap()["a"], 1);
        // locked without delete sharing: retries briefly, then errors leaving the original untouched
        {
            let _lock = std::fs::OpenOptions::new().read(true).share_mode(1 /* FILE_SHARE_READ */).open(&p).unwrap();
            let before = std::fs::read(&p).unwrap();
            let e = json_set_server(&p, "mcpServers", Some(json!({"command": "d"}))).unwrap_err();
            assert!(format!("{e:#}").contains("바꾸지 못했습니다"), "{e:#}");
            assert_eq!(std::fs::read(&p).unwrap(), before);
            assert!(!with_suffix(&p, ".ghostdesk-tmp").exists());
        }
        json_set_server(&p, "mcpServers", Some(json!({"command": "d"}))).unwrap();
    }

    /// Only shows this computer's real locations (read only: looks at folder listings and existence,
    /// never file contents).
    ///
    /// `cargo test install::tests::show_real_locations -- --ignored --nocapture`
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn show_real_locations() {
        let b = Bases::real();
        println!("설치 위치: {}", b.local.join("Programs").join("GhostDesk").join(crate::platform::paths::EXE_NAME).display());
        println!("GhostDesk 설정: {}", b.settings.display());
        println!("Claude Code 설정: {}", b.var_path("CLAUDE_CONFIG_DIR").unwrap_or_else(|| b.home.clone()).join(".claude.json").display());
        println!("Claude 앱 설정: {} (설치됨: {})", claude_app_config(&b).display(), claude_app_installed(&b));
        println!("Codex 설정: {} (MSIX: {:?})", codex_home(&b).join("config.toml").display(), package_ids(&b, "OpenAI.Codex"));
    }

    /// The installed copy can be replaced with a new version even while running (ghostdesk.exe --mcp
    /// started by Claude, …).
    #[cfg(windows)]
    #[test]
    fn windows_install_replaces_running_exe() {
        use crate::platform::proc;
        let h = FakeHome::new("winstall");
        let dir = stable_app();
        let target = stable_exe();
        mkdir(&dir);
        let names = || -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
            v.sort();
            v
        };
        // mimic a running installed copy: copy cmd.exe under the installed name and run it windowless
        std::fs::copy(system32().join("cmd.exe"), &target).unwrap();
        let mut running = proc::spawn_group(
            Command::new(&target).args(["/d", "/c", "ping -n 10 127.0.0.1 >NUL"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(proc::peek_exit(&running).is_none(), "설치본이 실행 중이어야 함");
        assert!(std::fs::write(&target, b"x").is_err(), "실행 중인 exe 는 덮어쓸 수 없음 (그래서 옆으로 옮긴다)");
        // copies from other interrupted installs: those of exited processes are deleted, those of live
        // ones are kept
        let dead = with_suffix(&target, ".installing-4000000000");
        let busy = with_suffix(&target, &format!(".installing-{}", running.id()));
        std::fs::write(&dead, b"partial").unwrap();
        std::fs::write(&busy, b"partial").unwrap();

        let src = h.path("build").join("ghostdesk.exe");
        mkdir(src.parent().unwrap());
        std::fs::write(&src, b"new build v2").unwrap();
        assert_eq!(install_exe_into(&src, &dir).unwrap(), target);
        assert_eq!(std::fs::read(&target).unwrap(), b"new build v2");
        assert!(proc::peek_exit(&running).is_none(), "실행 중이던 옛 설치본은 그대로 계속 실행");
        let olds: Vec<String> = names().into_iter().filter(|n| n.starts_with("ghostdesk.exe.old-")).collect();
        assert_eq!(olds.len(), 1, "옛 설치본은 옆으로 옮겨져 있음: {:?}", names());
        assert!(!dead.exists() && busy.exists(), "{:?}", names());
        assert_eq!(std::fs::read_to_string(source_stamp_path(&dir)).unwrap().trim(), sha256_file(&src).unwrap());

        // the same source isn't copied again
        let mtime = std::fs::metadata(&target).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        install_exe_into(&src, &dir).unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().modified().unwrap(), mtime);
        // running as the installed copy itself or as an old copy moved aside does nothing (the new
        // installed copy isn't rolled back)
        install_exe_into(&target, &dir).unwrap();
        install_exe_into(&dir.join(&olds[0]), &dir).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new build v2");

        // once the old process exits, the next install deletes the old file. If not running, it's swapped
        // in one go (no old file left)
        proc::kill_group(&mut running);
        std::thread::sleep(Duration::from_millis(500));
        std::fs::write(&src, b"new build v3 (longer)").unwrap();
        install_exe_into(&src, &dir).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new build v3 (longer)");
        // the install folder holds only the installed copy, the source fingerprint and the installed
        // version (so an old exe can't overwrite a newer installed copy)
        assert_eq!(names(), vec!["ghostdesk.exe".to_string(), "ghostdesk.src-sha256".to_string(), "ghostdesk.version".to_string()]);
        // fingerprint matches but the installed copy changed (different size): reinstall
        std::fs::write(&target, b"tampered").unwrap();
        install_exe_into(&src, &dir).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new build v3 (longer)");
    }
}
