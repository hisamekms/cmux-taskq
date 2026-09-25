//! Planner sessions (ADR-0041 decisions 1, 6, 12, 13): one `planners` row
//! per on-demand planner workspace, with its origin, the proposal the
//! runtime opened it for, its workspace UUID and what its session wrapper
//! records (pids, heartbeat, the agent's exit).
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, Row, params};

use super::sqlite::{SqliteQueue, enum_col};
use crate::domain::{PlannerId, PlannerOrigin, PlannerSession, ProposalId};

impl SqliteQueue {
    /// Record a new planner before its workspace exists; the caller opens
    /// the workspace and records it with [`Self::planner_workspace_created`].
    pub fn open_planner(
        &self,
        origin: PlannerOrigin,
        proposal: Option<ProposalId>,
    ) -> Result<PlannerSession> {
        self.conn.execute(
            "INSERT INTO planners(origin, proposal_id, created_at) VALUES (?1, ?2, ?3)",
            params![origin.as_str(), proposal, self.generators.clock.now()],
        )?;
        self.planner(PlannerId::new(self.conn.last_insert_rowid()))
    }

    /// The UUID of the workspace cmux opened for the planner.
    pub fn planner_workspace_created(&self, id: PlannerId, workspace_id: &str) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE planners SET workspace_id=?2 WHERE id=?1 AND workspace_id IS NULL",
                params![id, workspace_id],
            )? == 1,
            "planner {id} already has a workspace or does not exist"
        );
        Ok(())
    }

    /// Give the planner up: its workspace failed to open (`error`), or is
    /// gone. Closing a closed planner keeps the first close.
    pub fn close_planner(&self, id: PlannerId, error: Option<&str>) -> Result<PlannerSession> {
        self.conn.execute(
            "UPDATE planners SET closed_at=?2, error=coalesce(error, ?3)
             WHERE id=?1 AND closed_at IS NULL",
            params![id, self.generators.clock.now(), error],
        )?;
        self.planner(id)
    }

    pub fn planner(&self, id: PlannerId) -> Result<PlannerSession> {
        self.conn
            .query_row("SELECT * FROM planners WHERE id=?1", [id], planner_row)
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("planner {id} does not exist"))
    }

    /// The planners not closed, oldest first; with `all`, every planner.
    pub fn planners(&self, all: bool) -> Result<Vec<PlannerSession>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM planners WHERE ?1 OR closed_at IS NULL ORDER BY id")?
            .query_map([all], planner_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The wrapper `pid` started in the planner's workspace. A planner has
    /// one session: a second wrapper, or one for a closed planner, is refused.
    pub fn register_planner_wrapper(&self, id: PlannerId, pid: u32) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE planners SET wrapper_pid=?2, heartbeat_at=?3
                 WHERE id=?1 AND wrapper_pid IS NULL AND closed_at IS NULL",
                params![id, pid, self.generators.clock.now()],
            )? == 1,
            "planner {id} already has a session or is closed"
        );
        Ok(())
    }

    pub fn register_planner_agent(
        &self,
        id: PlannerId,
        wrapper_pid: u32,
        agent: u32,
    ) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE planners SET agent_pid=?3 WHERE id=?1 AND wrapper_pid=?2",
                params![id, wrapper_pid, agent],
            )? == 1,
            "wrapper {wrapper_pid} does not own planner {id}"
        );
        Ok(())
    }

    pub fn heartbeat_planner(&self, id: PlannerId, wrapper_pid: u32) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE planners SET heartbeat_at=?3
                 WHERE id=?1 AND wrapper_pid=?2 AND exited_at IS NULL",
                params![id, wrapper_pid, self.generators.clock.now()],
            )? == 1,
            "wrapper {wrapper_pid} does not own planner {id}"
        );
        Ok(())
    }

    /// The planner's agent exited with `exit_code`.
    pub fn planner_exited(&self, id: PlannerId, wrapper_pid: u32, exit_code: i32) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE planners SET exit_code=?3, exited_at=?4
                 WHERE id=?1 AND wrapper_pid=?2 AND exited_at IS NULL",
                params![id, wrapper_pid, exit_code, self.generators.clock.now()],
            )? == 1,
            "wrapper {wrapper_pid} does not own planner {id}"
        );
        Ok(())
    }
}

pub(super) fn planner_row(r: &Row<'_>) -> rusqlite::Result<PlannerSession> {
    Ok(PlannerSession {
        id: r.get("id")?,
        origin: enum_col(r, "origin")?,
        proposal_id: r.get("proposal_id")?,
        draft_task_id: r.get("draft_task_id")?,
        workspace_id: r.get("workspace_id")?,
        wrapper_pid: r.get("wrapper_pid")?,
        agent_pid: r.get("agent_pid")?,
        heartbeat_at: r.get("heartbeat_at")?,
        exit_code: r.get("exit_code")?,
        exited_at: r.get("exited_at")?,
        closed_at: r.get("closed_at")?,
        error: r.get("error")?,
        created_at: r.get("created_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::PlannerState;

    #[test]
    fn planners_are_recorded_one_row_each_with_their_session() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let person = queue.open_planner(PlannerOrigin::Person, None).unwrap();
        let other = queue.open_planner(PlannerOrigin::Person, None).unwrap();
        assert_ne!(person.id, other.id);
        assert_eq!(person.workspace_id, None);
        queue.planner_workspace_created(person.id, "W1").unwrap();
        queue.planner_workspace_created(other.id, "W2").unwrap();
        // A workspace is recorded once, and never shared.
        assert!(queue.planner_workspace_created(person.id, "W3").is_err());
        queue.register_planner_wrapper(person.id, 10).unwrap();
        assert!(queue.register_planner_wrapper(person.id, 12).is_err());
        queue.register_planner_agent(person.id, 10, 11).unwrap();
        assert!(queue.register_planner_agent(person.id, 99, 11).is_err());
        queue.heartbeat_planner(person.id, 10).unwrap();
        assert!(queue.heartbeat_planner(person.id, 99).is_err());
        let live = queue.planner(person.id).unwrap();
        assert_eq!(
            (
                live.workspace_id.as_deref(),
                live.wrapper_pid,
                live.agent_pid
            ),
            (Some("W1"), Some(10), Some(11))
        );
        assert!(live.heartbeat_at.is_some());
        queue.planner_exited(person.id, 10, 0).unwrap();
        assert!(queue.planner_exited(person.id, 10, 0).is_err());
        assert!(queue.heartbeat_planner(person.id, 10).is_err());
        let exited = queue.planner(person.id).unwrap();
        assert_eq!(exited.exit_code, Some(0));
        assert_eq!(queue.planners(false).unwrap().len(), 2);

        let failed = queue.open_planner(PlannerOrigin::Runtime, None).unwrap();
        let closed = queue
            .close_planner(failed.id, Some("cmux refused"))
            .unwrap();
        assert_eq!(closed.error.as_deref(), Some("cmux refused"));
        let first = closed.closed_at;
        assert!(first.is_some());
        let again = queue.close_planner(failed.id, Some("other")).unwrap();
        assert_eq!(
            (again.closed_at, again.error.as_deref()),
            (first, Some("cmux refused"))
        );
        assert!(queue.register_planner_wrapper(failed.id, 20).is_err());
        assert_eq!(queue.planners(false).unwrap().len(), 2);
        assert_eq!(queue.planners(true).unwrap().len(), 3);
        assert!(queue.planner(PlannerId::new(99)).is_err());
        let probe = crate::domain::PlannerProbe {
            now: 0,
            workspace_listed: true,
            wrapper_alive: true,
            idle: None,
            working: None,
        };
        assert_eq!(again.state(&probe), PlannerState::Closed);
    }
}
