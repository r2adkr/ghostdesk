//! Teams: a shared work board for several agents working on one job.
//!
//! A team has a goal, a member list (the external agents that joined, each with a role such as
//! "research" or "build"), notes everyone can read, and a task list where an agent claims a task
//! so two agents never do the same one. The user creates and deletes teams in the GhostDesk window
//! and can add notes and tasks there too; agents do the same through the `team` and `team_task`
//! MCP tools (control.rs).
//!
//! Screens stay per agent: a team shares what the agents know and who does what, not the input on
//! one screen.
//!
//! Teams, notes and tasks are kept in `<data root>/teams.json` and survive a restart. Members are
//! live connections, so they are not saved: after a restart the members are gone and their claimed
//! tasks are open again.

use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Notes kept per team (the oldest go first).
const MAX_NOTES: usize = 500;
/// Notes returned by one read (agents read the latest; older ones with `before`).
const READ_NOTES: usize = 30;
/// Longest note or task text accepted.
const MAX_TEXT: usize = 4000;
/// Teams at most.
const MAX_TEAMS: usize = 50;
/// Author name of things the user writes in the GhostDesk window.
pub const USER: &str = "사용자";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Note {
    pub id: u64,
    /// Unix seconds
    pub at: u64,
    /// "Claude Code (research)" or "사용자"
    pub author: String,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TaskState {
    Open,
    /// `by`: who claimed it; `conn`: that member's connection (not saved)
    Claimed {
        by: String,
        #[serde(skip)]
        conn: u64,
    },
    Done {
        by: String,
        result: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Task {
    pub id: u64,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    pub created_by: String,
    #[serde(flatten)]
    pub state: TaskState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub conn: u64,
    pub client: String,
    pub role: String,
    /// Unix seconds
    pub joined: u64,
}

impl Member {
    pub fn label(&self) -> String {
        if self.role.is_empty() {
            self.client.clone()
        } else {
            format!("{} ({})", self.client, self.role)
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Team {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub goal: String,
    pub created: u64,
    pub created_by: String,
    #[serde(default)]
    pub notes: Vec<Note>,
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(skip)]
    pub members: Vec<Member>,
}

impl Team {
    fn member(&self, conn: u64) -> Option<&Member> {
        self.members.iter().find(|m| m.conn == conn)
    }

    pub fn open_tasks(&self) -> usize {
        self.tasks.iter().filter(|t| !matches!(t.state, TaskState::Done { .. })).count()
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    teams: Vec<Team>,
}

/// All teams. Shared by the control server (agents) and the GUI (the user).
pub struct TeamStore {
    inner: Mutex<Saved>,
    /// None in tests: nothing is written.
    path: Option<PathBuf>,
    /// Bumped on every change, so the GUI knows when to look again.
    version: std::sync::atomic::AtomicU64,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn clean(text: &str, what: &str) -> Result<String> {
    let t = text.trim();
    if t.is_empty() {
        bail!("{what} 이(가) 비어 있습니다");
    }
    if t.chars().count() > MAX_TEXT {
        bail!("{what} 이(가) 너무 깁니다 ({MAX_TEXT}자까지). 요약해서 다시 보내세요");
    }
    Ok(t.to_string())
}

impl TeamStore {
    /// The store saved in `<data root>/teams.json`.
    pub fn load() -> Self {
        let path = crate::platform::paths::data_root().join("teams.json");
        let saved: Saved = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self::from_saved(saved, Some(path))
    }

    /// A store that is never written to disk (tests).
    pub fn in_memory() -> Self {
        Self::from_saved(Saved::default(), None)
    }

    fn from_saved(mut saved: Saved, path: Option<PathBuf>) -> Self {
        // members are not saved, so tasks claimed before a restart are open again
        for t in &mut saved.teams {
            for task in &mut t.tasks {
                if matches!(task.state, TaskState::Claimed { .. }) {
                    task.state = TaskState::Open;
                }
            }
        }
        let max = saved.teams.iter().flat_map(|t| std::iter::once(t.id).chain(t.notes.iter().map(|n| n.id)).chain(t.tasks.iter().map(|x| x.id))).max().unwrap_or(0);
        saved.next_id = saved.next_id.max(max + 1);
        TeamStore { inner: Mutex::new(saved), path, version: Default::default() }
    }

    pub fn version(&self) -> u64 {
        self.version.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Runs `f` on the teams, then saves and bumps the version if it succeeded.
    fn change<R>(&self, f: impl FnOnce(&mut Saved) -> Result<R>) -> Result<R> {
        let mut s = self.inner.lock();
        let r = f(&mut s)?;
        if let Some(p) = &self.path {
            if let Ok(b) = serde_json::to_vec_pretty(&*s) {
                if let Some(d) = p.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                let tmp = p.with_extension("json.tmp");
                if std::fs::write(&tmp, b).is_ok() {
                    let _ = std::fs::rename(&tmp, p);
                }
            }
        }
        self.version.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(r)
    }

    fn take_id(s: &mut Saved) -> u64 {
        let id = s.next_id.max(1);
        s.next_id = id + 1;
        id
    }

    /// Index of the team called `key` (its name, case-insensitive, or its id).
    fn find(s: &Saved, key: &str) -> Result<usize> {
        let key = key.trim();
        s.teams
            .iter()
            .position(|t| t.name.eq_ignore_ascii_case(key) || t.id.to_string() == key)
            .ok_or_else(|| anyhow!("팀 '{key}' 이(가) 없습니다. team(action=list) 로 팀 목록을 확인하세요"))
    }

    /// Copy of every team (for the GUI).
    pub fn snapshot(&self) -> Vec<Team> {
        self.inner.lock().teams.clone()
    }

    pub fn create(&self, name: &str, goal: &str, by: &str) -> Result<u64> {
        let name = clean(name, "팀 이름")?;
        if name.chars().count() > 60 {
            bail!("팀 이름은 60자까지입니다");
        }
        self.change(|s| {
            if s.teams.iter().any(|t| t.name.eq_ignore_ascii_case(&name)) {
                bail!("'{name}' 팀이 이미 있습니다. 그 팀에 join 하거나 다른 이름을 쓰세요");
            }
            if s.teams.len() >= MAX_TEAMS {
                bail!("팀은 {MAX_TEAMS}개까지 만들 수 있습니다. 필요 없는 팀을 지우세요");
            }
            let id = Self::take_id(s);
            s.teams.push(Team {
                id,
                name,
                goal: goal.trim().chars().take(MAX_TEXT).collect(),
                created: now(),
                created_by: by.to_string(),
                notes: Vec::new(),
                tasks: Vec::new(),
                members: Vec::new(),
            });
            Ok(id)
        })
    }

    pub fn delete(&self, key: &str) -> Result<String> {
        self.change(|s| {
            let i = Self::find(s, key)?;
            Ok(s.teams.remove(i).name)
        })
    }

    pub fn set_goal(&self, key: &str, goal: &str) -> Result<()> {
        self.change(|s| {
            let i = Self::find(s, key)?;
            s.teams[i].goal = goal.trim().chars().take(MAX_TEXT).collect();
            Ok(())
        })
    }

    /// Joins (or changes role in) a team. One membership per connection per team.
    pub fn join(&self, key: &str, conn: u64, client: &str, role: &str) -> Result<()> {
        let role: String = role.trim().chars().take(40).collect();
        self.change(|s| {
            let i = Self::find(s, key)?;
            let t = &mut s.teams[i];
            match t.members.iter_mut().find(|m| m.conn == conn) {
                Some(m) => m.role = role,
                None => t.members.push(Member { conn, client: client.to_string(), role, joined: now() }),
            }
            Ok(())
        })
    }

    /// Leaves a team; tasks this member claimed are open again.
    pub fn leave(&self, key: &str, conn: u64) -> Result<()> {
        self.change(|s| {
            let i = Self::find(s, key)?;
            Self::drop_member(&mut s.teams[i], conn);
            Ok(())
        })
    }

    /// A connection ended: leaves every team it was in.
    pub fn leave_all(&self, conn: u64) {
        if !self.inner.lock().teams.iter().any(|t| t.member(conn).is_some()) {
            return;
        }
        let _ = self.change(|s| {
            for t in &mut s.teams {
                Self::drop_member(t, conn);
            }
            Ok(())
        });
    }

    fn drop_member(t: &mut Team, conn: u64) {
        t.members.retain(|m| m.conn != conn);
        for task in &mut t.tasks {
            if matches!(task.state, TaskState::Claimed { conn: c, .. } if c == conn) {
                task.state = TaskState::Open;
            }
        }
    }

    /// Who is writing: a member's label, or the user when `conn` is None. Agents must join first.
    fn author(t: &Team, conn: Option<u64>) -> Result<String> {
        match conn {
            None => Ok(USER.to_string()),
            Some(c) => t
                .member(c)
                .map(Member::label)
                .ok_or_else(|| anyhow!("먼저 team(action=join, team='{}', role=…) 으로 이 팀에 들어오세요", t.name)),
        }
    }

    pub fn note(&self, key: &str, conn: Option<u64>, text: &str) -> Result<u64> {
        let text = clean(text, "메모")?;
        self.change(|s| {
            let i = Self::find(s, key)?;
            let author = Self::author(&s.teams[i], conn)?;
            let id = Self::take_id(s);
            let t = &mut s.teams[i];
            t.notes.push(Note { id, at: now(), author, text });
            if t.notes.len() > MAX_NOTES {
                let extra = t.notes.len() - MAX_NOTES;
                t.notes.drain(..extra);
            }
            Ok(id)
        })
    }

    pub fn task_add(&self, key: &str, conn: Option<u64>, title: &str, detail: &str) -> Result<u64> {
        let title = clean(title, "할 일 제목")?;
        self.change(|s| {
            let i = Self::find(s, key)?;
            let created_by = Self::author(&s.teams[i], conn)?;
            let id = Self::take_id(s);
            s.teams[i].tasks.push(Task {
                id,
                title,
                detail: detail.trim().chars().take(MAX_TEXT).collect(),
                created_by,
                state: TaskState::Open,
            });
            Ok(id)
        })
    }

    /// Claims an open task. Fails if someone else has it, so two agents never take the same one.
    pub fn task_claim(&self, key: &str, conn: u64, task: u64) -> Result<()> {
        self.change(|s| {
            let i = Self::find(s, key)?;
            let by = Self::author(&s.teams[i], Some(conn))?;
            let t = Self::task_mut(&mut s.teams[i], task)?;
            match &t.state {
                TaskState::Open => {
                    t.state = TaskState::Claimed { by, conn };
                    Ok(())
                }
                TaskState::Claimed { conn: c, .. } if *c == conn => Ok(()),
                TaskState::Claimed { by: other, .. } => bail!("할 일 {task} 은(는) {other} 이(가) 이미 맡았습니다. 다른 할 일을 고르세요"),
                TaskState::Done { .. } => bail!("할 일 {task} 은(는) 이미 끝났습니다"),
            }
        })
    }

    /// Marks a task done with a short result. Only whoever claimed it (or the user) can.
    pub fn task_done(&self, key: &str, conn: Option<u64>, task: u64, result: &str) -> Result<()> {
        self.change(|s| {
            let i = Self::find(s, key)?;
            let by = Self::author(&s.teams[i], conn)?;
            let t = Self::task_mut(&mut s.teams[i], task)?;
            match (&t.state, conn) {
                (TaskState::Claimed { conn: c, .. }, Some(me)) if *c != me => bail!("할 일 {task} 은(는) 다른 에이전트가 맡고 있습니다"),
                (TaskState::Open, Some(_)) => bail!("할 일 {task} 을(를) 먼저 claim 하세요"),
                (TaskState::Done { .. }, _) => bail!("할 일 {task} 은(는) 이미 끝났습니다"),
                _ => {}
            }
            t.state = TaskState::Done { by, result: result.trim().chars().take(MAX_TEXT).collect() };
            Ok(())
        })
    }

    /// Gives a claimed task back (or, for the user, reopens any task).
    pub fn task_release(&self, key: &str, conn: Option<u64>, task: u64) -> Result<()> {
        self.change(|s| {
            let i = Self::find(s, key)?;
            Self::author(&s.teams[i], conn)?;
            let t = Self::task_mut(&mut s.teams[i], task)?;
            if let (TaskState::Claimed { conn: c, .. }, Some(me)) = (&t.state, conn) {
                if *c != me {
                    bail!("할 일 {task} 은(는) 다른 에이전트가 맡고 있습니다");
                }
            }
            if let (TaskState::Done { .. }, Some(_)) = (&t.state, conn) {
                bail!("끝난 할 일은 사용자만 다시 열 수 있습니다");
            }
            t.state = TaskState::Open;
            Ok(())
        })
    }

    pub fn task_delete(&self, key: &str, task: u64) -> Result<()> {
        self.change(|s| {
            let i = Self::find(s, key)?;
            let before = s.teams[i].tasks.len();
            s.teams[i].tasks.retain(|t| t.id != task);
            if s.teams[i].tasks.len() == before {
                bail!("할 일 {task} 이(가) 없습니다");
            }
            Ok(())
        })
    }

    fn task_mut(t: &mut Team, id: u64) -> Result<&mut Task> {
        let name = t.name.clone();
        t.tasks.iter_mut().find(|x| x.id == id).ok_or_else(|| anyhow!("팀 '{name}' 에 할 일 {id} 이(가) 없습니다"))
    }

    /// Team list for agents: name, goal, how many members and open tasks.
    pub fn list_json(&self, conn: u64) -> Value {
        let s = self.inner.lock();
        json!(s
            .teams
            .iter()
            .map(|t| json!({
                "team": t.name,
                "goal": t.goal,
                "members": t.members.iter().map(Member::label).collect::<Vec<_>>(),
                "open_tasks": t.open_tasks(),
                "notes": t.notes.len(),
                "joined": t.member(conn).is_some(),
            }))
            .collect::<Vec<_>>())
    }

    /// What an agent reads: goal, members, every task, and the latest notes (or the ones before
    /// note id `before`, for paging back), oldest first.
    pub fn read_json(&self, key: &str, before: Option<u64>) -> Result<Value> {
        let s = self.inner.lock();
        let t = &s.teams[Self::find(&s, key)?];
        let pool: Vec<&Note> = t.notes.iter().filter(|n| before.is_none_or(|b| n.id < b)).collect();
        let start = pool.len().saturating_sub(READ_NOTES);
        let notes = &pool[start..];
        Ok(json!({
            "team": t.name,
            "goal": t.goal,
            "members": t.members.iter().map(Member::label).collect::<Vec<_>>(),
            "tasks": t.tasks,
            "notes": notes,
            "older_notes": start,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> TeamStore {
        let s = TeamStore::in_memory();
        s.create("Trip", "Plan a 3-day trip", USER).unwrap();
        s
    }

    #[test]
    fn create_is_unique_and_case_insensitive() {
        let s = store();
        assert!(s.create("trip", "", USER).is_err());
        assert!(s.create("  ", "", USER).is_err());
        s.create("Other", "", USER).unwrap();
        assert_eq!(s.snapshot().len(), 2);
        assert_eq!(s.delete("TRIP").unwrap(), "Trip");
        assert!(s.delete("Trip").is_err());
    }

    #[test]
    fn agents_must_join_before_writing() {
        let s = store();
        assert!(s.note("Trip", Some(1), "hello").unwrap_err().to_string().contains("join"));
        s.join("Trip", 1, "Claude Code", "research").unwrap();
        s.note("Trip", Some(1), "flights are cheaper on Tuesday").unwrap();
        s.note("Trip", None, "budget 500k").unwrap();
        let r = s.read_json("trip", None).unwrap();
        let authors: Vec<&str> = r["notes"].as_array().unwrap().iter().map(|n| n["author"].as_str().unwrap()).collect();
        assert_eq!(authors, ["Claude Code (research)", USER]);
        assert_eq!(r["members"], json!(["Claude Code (research)"]));
    }

    #[test]
    fn a_task_goes_to_one_agent() {
        let s = store();
        s.join("Trip", 1, "Claude Code", "research").unwrap();
        s.join("Trip", 2, "Codex", "booking").unwrap();
        let id = s.task_add("Trip", None, "Find hotels", "").unwrap();
        s.task_claim("Trip", 1, id).unwrap();
        // claiming again is fine for the same agent, refused for another
        s.task_claim("Trip", 1, id).unwrap();
        assert!(s.task_claim("Trip", 2, id).unwrap_err().to_string().contains("Claude Code (research)"));
        assert!(s.task_done("Trip", Some(2), id, "x").is_err());
        s.task_done("Trip", Some(1), id, "3 hotels near the station").unwrap();
        assert!(s.task_claim("Trip", 2, id).is_err());
        let t = &s.snapshot()[0].tasks[0];
        assert_eq!(t.state, TaskState::Done { by: "Claude Code (research)".into(), result: "3 hotels near the station".into() });
        // only the user reopens a finished task
        assert!(s.task_release("Trip", Some(1), id).is_err());
        s.task_release("Trip", None, id).unwrap();
        assert_eq!(s.snapshot()[0].open_tasks(), 1);
    }

    #[test]
    fn leaving_or_disconnecting_frees_claimed_tasks() {
        let s = store();
        s.join("Trip", 7, "Codex", "").unwrap();
        let a = s.task_add("Trip", Some(7), "A", "").unwrap();
        s.task_claim("Trip", 7, a).unwrap();
        s.leave_all(7);
        let t = &s.snapshot()[0];
        assert!(t.members.is_empty());
        assert_eq!(t.tasks[0].state, TaskState::Open);
    }

    #[test]
    fn reads_the_latest_notes_and_pages_back() {
        let s = store();
        for i in 0..45 {
            s.note("Trip", None, &format!("n{i}")).unwrap();
        }
        let r = s.read_json("Trip", None).unwrap();
        let notes = r["notes"].as_array().unwrap();
        assert_eq!(notes.len(), READ_NOTES);
        assert_eq!(notes.last().unwrap()["text"], "n44");
        assert_eq!(r["older_notes"], 15);
        let first = notes[0]["id"].as_u64().unwrap();
        let older = s.read_json("Trip", Some(first)).unwrap();
        assert_eq!(older["notes"].as_array().unwrap().len(), 15);
        assert_eq!(older["older_notes"], 0);
    }

    #[test]
    fn saved_and_loaded_without_members_or_claims() {
        let path = std::env::temp_dir().join(format!("ghostdesk-teams-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let s = TeamStore::from_saved(Saved::default(), Some(path.clone()));
            s.create("Trip", "goal", USER).unwrap();
            s.join("Trip", 1, "Codex", "").unwrap();
            let id = s.task_add("Trip", Some(1), "A", "").unwrap();
            s.task_claim("Trip", 1, id).unwrap();
            s.note("Trip", Some(1), "hi").unwrap();
        }
        let saved: Saved = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let s = TeamStore::from_saved(saved, None);
        let t = &s.snapshot()[0];
        assert!(t.members.is_empty());
        assert_eq!(t.tasks[0].state, TaskState::Open);
        assert_eq!(t.notes[0].text, "hi");
        // new ids keep counting after the saved ones
        let n = s.note("Trip", None, "again").unwrap();
        assert!(n > t.notes[0].id);
        let _ = std::fs::remove_file(&path);
    }
}
