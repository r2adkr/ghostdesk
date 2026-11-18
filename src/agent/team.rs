//! Scoped team board tools for agents started in GhostDesk.

use std::{sync::atomic::Ordering, sync::Arc};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use super::{AgentShared, LogKind};
use crate::team::{FileEditAction, TeamStore};

#[derive(Clone)]
pub(crate) struct TeamAssignment {
    pub store: Arc<TeamStore>,
    pub id: u64,
    pub role: String,
}

/// Membership lasts as long as the agent loop, including between user instructions.
pub(super) struct Membership {
    team: TeamAssignment,
    shared: Arc<AgentShared>,
    id: u64,
}

impl Membership {
    pub fn join(team: &TeamAssignment, shared: &Arc<AgentShared>) -> Result<Self> {
        // MCP connections use increasing IDs from 1. App agents have their own namespace.
        let id = shared.id | (1 << 63);
        team.store.join(team.id, id, &shared.name, &team.role)?;
        shared.team_member_id.store(id, Ordering::Relaxed);
        shared.log(LogKind::Info, "팀 작업판에 연결했습니다");
        Ok(Self { team: team.clone(), shared: Arc::clone(shared), id })
    }

    pub fn finish_turn(&self) -> bool {
        let released = self.team.store.release_file_edits(self.team.id, self.id);
        if released {
            self.shared.ctx.request_repaint();
        }
        released
    }

    pub fn execute(&self, input: &Value) -> Result<Value> {
        let store = &self.team.store;
        let team = self.team.id;
        let member = self.id;
        let text = |field| input[field].as_str().unwrap_or_default();
        let task = || input["task"].as_u64().ok_or_else(|| anyhow!("A numeric task ID is required"));
        match text("action") {
            "read" => {
                let mut board = store.read_for(team, input["before"].as_u64(), Some(member))?;
                board["your_role"] = json!(self.team.role);
                Ok(board)
            }
            "note" => Ok(json!({ "note": store.note(team, Some(member), text("text"))? })),
            "add" => Ok(json!({ "task": store.task_add(team, Some(member), text("title"), text("detail"))? })),
            "claim" => store.task_claim(team, member, task()?).map(|_| json!({ "status": "claimed" })),
            "done" => store.task_done(team, Some(member), task()?, text("result")).map(|_| json!({ "status": "done" })),
            "release" => store.task_release(team, Some(member), task()?).map(|_| json!({ "status": "open" })),
            "claim_files" => store.edit_files(team, member, FileEditAction::Claim, &input["paths"]),
            "release_files" => store.edit_files(team, member, FileEditAction::Release, &input["paths"]),
            _ => bail!("Unknown team board action"),
        }
    }
}

impl Drop for Membership {
    fn drop(&mut self) {
        self.team.store.leave_all(self.id);
        self.shared.team_member_id.store(0, Ordering::Relaxed);
        self.shared.ctx.request_repaint();
    }
}

pub(super) fn tool() -> Value {
    json!({
        "name": "team_board",
        "description": "Shared board of the team selected by the human. Read its goal, members, tasks, active file edits and recent notes with read (optional before note ID). Use claim_files and release_files with paths to coordinate file edits. Add tasks with add (title, detail); claim, done (result), or release a task by numeric task ID; share progress with note (text).",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["read", "add", "claim", "done", "release", "note", "claim_files", "release_files"]},
                "paths": {"type": "array", "minItems": 1, "items": {"type": "string"}, "description": "Exact repository-relative paths from the team's shared project root (e.g. src/main.rs). Use the repository's spelling and / separators. Roots and absolute paths are invalid."},
                "task": {"type": "integer", "minimum": 1},
                "before": {"type": "integer", "minimum": 1},
                "title": {"type": "string"},
                "detail": {"type": "string"},
                "result": {"type": "string"},
                "text": {"type": "string"}
            },
            "required": ["action"],
            "additionalProperties": false
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{history::Seen, tools::execute_batch};
    use crate::desk::Desk;
    use crate::team::{TaskState, USER};

    fn fixture() -> (TeamAssignment, Arc<AgentShared>) {
        let store = Arc::new(TeamStore::in_memory());
        let id = store.create("Team QA", "Verify the team workflow", USER).unwrap();
        let shared = AgentShared::new(1, "Local agent".into(), "team-qa".into(), eframe::egui::Context::default());
        (TeamAssignment { store, id, role: "Reviewer".into() }, shared)
    }

    #[tokio::test]
    async fn team_review_stop_interrupts_a_question_without_an_answer() {
        let (team, shared) = fixture();
        let member = Membership::join(&team, &shared).unwrap();
        let desk = Desk::Ssh(crate::ssh::SshConnection::new(crate::ssh::SshOptions { host: "test".into(), ..Default::default() }).unwrap());
        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let batch = [
            json!({"id":"ask","name":"ask_user","input":{"question":"Continue?"}}),
            json!({"id":"note","name":"team_board","input":{"action":"note","text":"unexpected"}}),
        ];
        let stop = async {
            while !matches!(shared.status(), crate::agent::AgentStatus::WaitingUser(_)) {
                tokio::task::yield_now().await;
            }
            shared.stop.store(true, Ordering::SeqCst);
        };
        let mut seen = Seen::default();
        let work = execute_batch(&shared, &desk, &batch, &mut rx, &mut seen, Some(&member));
        let (_, results) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(stop, work)
        }).await.expect("Stop must unblock the unanswered question");
        assert!(results.iter().all(|r| r["is_error"] == true));
        assert!(team.store.snapshot()[0].notes.is_empty());
    }

    #[tokio::test]
    async fn losing_a_file_claim_skips_the_rest_of_the_batch() {
        let (team, shared) = fixture();
        let owner = Membership::join(&team, &shared).unwrap();
        owner.execute(&json!({"action":"claim_files","paths":["src/main.rs"]})).unwrap();

        let other = AgentShared::new(2, "Other agent".into(), "team-qa".into(), shared.ctx.clone());
        let member = Membership::join(&team, &other).unwrap();
        let read = member.execute(&json!({"action":"read"})).unwrap();
        assert_eq!(read["file_edits"], json!([{"path":"src/main.rs","by":"Local agent (Reviewer)"}]));
        for path in [".", "src/", "src/main.rs/child"] {
            assert!(member.execute(&json!({"action":"claim_files","paths":[path]})).is_err(), "{path}");
        }
        assert!(member.execute(&json!({"action":"release_files","paths":["src/main.rs"]})).is_err());
        assert!(member.execute(&json!({"action":"claim_files","paths":["../outside.rs"]})).is_err());
        assert!(member.execute(&json!({"action":"claim_files","paths":["src/main.rs.bak"]})).is_ok());
        member.execute(&json!({"action":"release_files","paths":["src/main.rs.bak"]})).unwrap();

        let desk = Desk::Ssh(crate::ssh::SshConnection::new(crate::ssh::SshOptions { host: "test".into(), ..Default::default() }).unwrap());
        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let batch = [
            json!({"id":"claim","name":"team_board","input":{"action":"claim_files","paths":["src/free.rs","./src//main.rs"]}}),
            json!({"id":"note","name":"team_board","input":{"action":"note","text":"Should not run after losing a file claim"}}),
        ];
        let results = execute_batch(&other, &desk, &batch, &mut rx, &mut Seen::default(), Some(&member)).await;
        assert!(results.iter().all(|result| result["is_error"] == true));
        assert_eq!(member.execute(&json!({"action":"read"})).unwrap()["file_edits"], read["file_edits"]);
        assert!(team.store.snapshot()[0].notes.is_empty());

        drop(owner);
        assert_eq!(member.execute(&json!({"action":"read"})).unwrap()["file_edits"], json!([]));
        member.execute(&json!({"action":"claim_files","paths":["src/main.rs"]})).unwrap();
        member.finish_turn();
        let board = member.execute(&json!({"action":"read"})).unwrap();
        assert_eq!(board["file_edits"], json!([]));
        assert_eq!(board["members"], json!(["Other agent (Reviewer)"]));
    }

    #[tokio::test]
    async fn local_member_shares_claims_with_mcp_members_and_releases_on_exit() {
        let (team, shared) = fixture();
        let other_team = team.store.create("Other team", "", USER).unwrap();
        team.store.join(team.id, 1, "MCP agent", "").unwrap();
        let external_task = team.store.task_add(team.id, Some(1), "Externally claimed", "").unwrap();
        team.store.task_claim(team.id, 1, external_task).unwrap();

        let member = Membership::join(&team, &shared).unwrap();
        let read = member.execute(&json!({"action":"read"})).unwrap();
        assert_eq!(read["goal"], "Verify the team workflow");
        assert_eq!(read["your_role"], "Reviewer");
        assert_ne!(shared.team_member_id.load(Ordering::Relaxed), 1);
        assert_eq!(team.store.snapshot()[0].members.len(), 2);

        let desk = Desk::Ssh(crate::ssh::SshConnection::new(crate::ssh::SshOptions { host: "test".into(), ..Default::default() }).unwrap());
        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let batch = [
            json!({"id":"claim","name":"team_board","input":{"action":"claim","task":external_task}}),
            json!({"id":"note","name":"team_board","input":{"action":"note","text":"Should not run after losing a claim"}}),
        ];
        let results = execute_batch(&shared, &desk, &batch, &mut rx, &mut Seen::default(), Some(&member)).await;
        assert!(results.iter().all(|result| result["is_error"] == true));
        assert!(team.store.snapshot()[0].notes.is_empty());

        let other_task = team.store.task_add(other_team, None, "Other board task", "").unwrap();
        assert!(member.execute(&json!({"action":"claim","task":other_task})).is_err());
        let local_task = member.execute(&json!({"action":"add","title":"Local work"})).unwrap()["task"].as_u64().unwrap();
        member.execute(&json!({"action":"claim","task":local_task})).unwrap();
        assert!(team.store.task_claim(team.id, 1, local_task).is_err());
        let done = team.store.task_add(team.id, None, "Completed work", "").unwrap();
        member.execute(&json!({"action":"claim","task":done})).unwrap();
        member.execute(&json!({"action":"done","task":done,"result":"Verified"})).unwrap();
        member.execute(&json!({"action":"note","text":"Shared update"})).unwrap();
        drop(member);

        let snapshot = team.store.snapshot();
        let board = &snapshot[0];
        assert_eq!(shared.team_member_id.load(Ordering::Relaxed), 0);
        assert_eq!(board.members.len(), 1);
        assert_eq!(board.members[0].conn, 1);
        assert!(matches!(board.tasks[0].state, TaskState::Claimed { conn: 1, .. }));
        assert!(matches!(board.tasks[1].state, TaskState::Open));
        assert!(matches!(board.tasks[2].state, TaskState::Done { ref result, .. } if result == "Verified"));
        assert_eq!(board.notes[0].author, "Local agent (Reviewer)");
        assert!(snapshot[1].notes.is_empty());
    }

    #[tokio::test]
    #[ignore = "Uses the installed Codex CLI account for one inference"]
    async fn live_codex_can_call_the_local_team_board() {
        use crate::llm::{Llm, Provider};

        let executable = std::env::var("GHOSTDESK_TEST_CODEX").unwrap();
        let models = crate::cli::models(Provider::CodexCli, &executable).await.unwrap();
        let model = models.first().expect("Codex model");
        let (team, shared) = fixture();
        let member = Membership::join(&team, &shared).unwrap();
        let llm = Llm::new(Provider::CodexCli, executable, String::new(), model.clone(), String::new());
        let reply = llm
            .send(
                "You are testing a local GhostDesk QA board. Use the provided team_board tool to record the requested note.",
                &json!([tool()]),
                &[json!({"role":"user","content":"Add a team note with the exact text: Codex team tool verified"})],
            )
            .await
            .unwrap();
        let uses: Vec<Value> = reply["content"].as_array().unwrap().iter().filter(|b| b["type"] == "tool_use").cloned().collect();
        assert_eq!(uses.len(), 1, "{reply}");
        assert_eq!(uses[0]["name"], "team_board");
        let desk = Desk::Ssh(crate::ssh::SshConnection::new(crate::ssh::SshOptions { host: "test".into(), ..Default::default() }).unwrap());
        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let results = execute_batch(&shared, &desk, &uses, &mut rx, &mut Seen::default(), Some(&member)).await;
        assert_ne!(results[0]["is_error"], true, "{results:?}");
        assert_eq!(team.store.snapshot()[0].notes[0].text, "Codex team tool verified");
    }
}
