//! What agents on the engine may do on this computer, set in Settings → Permissions. Every kind of access
//! is asked each time, always allowed or always refused. The permission bypass for outside agents never
//! answers these: only this setting or a click does.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rule {
    #[default]
    Ask,
    Allow,
    Deny,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Reading a file outside the agent's own folder.
    Read,
    /// Writing or editing a file outside the agent's own folder.
    Write,
    /// Running a command.
    Run,
    /// Using a tool from the tool box.
    Tool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Access {
    pub read: Rule,
    pub write: Rule,
    pub run: Rule,
    pub tool: Rule,
    /// Whether plugins may be installed at all (each install still needs a click).
    pub plugins: bool,
}

impl Access {
    pub fn rule(&self, kind: Kind) -> Rule {
        match kind {
            Kind::Read => self.read,
            Kind::Write => self.write,
            Kind::Run => self.run,
            Kind::Tool => self.tool,
        }
    }
}

static CURRENT: parking_lot::Mutex<Access> =
    parking_lot::Mutex::new(Access { read: Rule::Ask, write: Rule::Ask, run: Rule::Ask, tool: Rule::Ask, plugins: false });

pub fn set(a: Access) {
    *CURRENT.lock() = a;
}

pub fn get() -> Access {
    *CURRENT.lock()
}

/// The kind of access a request from the engine's file, command or tool-box tools is.
pub fn kind_of(tool: &str) -> Kind {
    match tool {
        "bash" | "powershell" => Kind::Run,
        "write" | "edit" => Kind::Write,
        "mcp" => Kind::Tool,
        _ => Kind::Read,
    }
}

/// What to do with a request of this kind: the setting, with commands refused while shells and system tools
/// are switched off.
pub fn decide(kind: Kind, shell_tools_allowed: bool) -> Rule {
    if kind == Kind::Run && !shell_tools_allowed {
        return Rule::Deny;
    }
    get().rule(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_rules() {
        assert_eq!(kind_of("powershell"), Kind::Run);
        assert_eq!(kind_of("edit"), Kind::Write);
        assert_eq!(kind_of("mcp"), Kind::Tool);
        assert_eq!(kind_of("grep"), Kind::Read);
        let a = Access { run: Rule::Allow, ..Default::default() };
        assert_eq!(a.rule(Kind::Run), Rule::Allow);
        assert_eq!(a.rule(Kind::Read), Rule::Ask);
        assert!(!a.plugins);
    }

    #[test]
    fn old_files_without_the_section_mean_ask_everything() {
        let a: Access = serde_json::from_str("{}").unwrap();
        assert_eq!(a, Access::default());
        let b: Access = serde_json::from_str(r#"{"run":"deny","plugins":true}"#).unwrap();
        assert_eq!((b.run, b.read, b.plugins), (Rule::Deny, Rule::Ask, true));
    }
}
