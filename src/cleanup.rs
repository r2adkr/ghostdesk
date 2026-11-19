//! Tidying up after agent screens that are finished.
//!
//! Two things pile up when agents open screens: entries in the window's lists (a closed screen
//! stays until someone removes it) and workspace folders on disk (one per screen an agent opened
//! without naming a workspace). With "Tidy up finished screens" on (the default), both go away by
//! themselves:
//!
//! - a screen an outside agent closed leaves the lists after a short grace period (long enough
//!   to read how it ended);
//! - its scratch workspace is deleted when it closes, and scratch workspaces left over from
//!   earlier runs are deleted once they are a week old.
//!
//! Only scratch workspaces are touched: folders an agent chose a name for (it may come back to
//! them), and anything holding a cloned login, are always kept.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

static ENABLED: AtomicBool = AtomicBool::new(true);

/// How long a closed screen stays in the lists.
pub const LIST_GRACE: Duration = Duration::from_secs(90);
/// How old a leftover scratch workspace must be before the startup sweep deletes it.
const LEFTOVER_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Marker `control.rs` writes into workspaces opened without a login.
pub const SCRATCH_MARKER: &str = "ghostdesk-mcp-blank";

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// A scratch workspace: no-login MCP namespace, marked as blank by GhostDesk.
fn is_scratch(dir: &Path) -> bool {
    dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with("mcp_")) && dir.join(SCRATCH_MARKER).is_file()
}

/// Deletes the workspace of a screen that just closed, if it was a scratch one the agent didn't
/// name. Runs in the background; returns at once.
pub fn forget_workspace(name: &str, agent_named: bool) {
    if !enabled() || agent_named {
        return;
    }
    let dir = crate::profiles::workspaces_root().join(crate::profiles::slug(name));
    if !is_scratch(&dir) {
        return;
    }
    std::thread::spawn(move || {
        let _ = crate::platform::fsx::remove_dir_all_retry(&dir);
    });
}

/// Startup sweep: deletes scratch workspaces untouched for a week. Returns how many it removed.
pub fn sweep_leftovers() -> usize {
    sweep(&crate::profiles::workspaces_root(), LEFTOVER_AGE)
}

fn sweep(root: &Path, older_than: Duration) -> usize {
    if !enabled() {
        return 0;
    }
    let Ok(rd) = std::fs::read_dir(root) else { return 0 };
    let now = SystemTime::now();
    let mut n = 0;
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() || !is_scratch(&p) {
            continue;
        }
        let age = std::fs::metadata(&p).and_then(|m| m.modified()).ok().and_then(|t| now.duration_since(t).ok());
        if age.is_some_and(|a| a >= older_than) && crate::platform::fsx::remove_dir_all_retry(&p).is_ok() {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_removes_only_old_scratch_workspaces() {
        let root = std::env::temp_dir().join(format!("gd-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (name, marked) in [("mcp_old", true), ("mcp_unmarked", false), ("mcp-login-x", true)] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            if marked {
                std::fs::write(root.join(name).join(SCRATCH_MARKER), "").unwrap();
            }
        }
        // nothing is old enough yet
        assert_eq!(sweep(&root, Duration::from_secs(3600)), 0);
        // with a zero age everything scratch goes, login clones and unmarked folders stay
        assert_eq!(sweep(&root, Duration::ZERO), 1);
        assert!(!root.join("mcp_old").exists());
        assert!(root.join("mcp_unmarked").exists() && root.join("mcp-login-x").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
