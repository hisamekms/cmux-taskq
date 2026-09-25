use std::{
    path::{Path, PathBuf},
    str::FromStr,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, ensure};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior, params, params_from_iter,
    types::{Type, Value},
};
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::{
    application::{
        Generators, GraphGoalDependency, GraphInput, GraphTask, IdGenerator, LatestRun,
        StatusFilter, TaskListItem, TaskPage, TaskQuery, TaskStore, dependency_graph, timestamp,
    },
    domain::{
        ClaimOutcome, CommitSha, DomainError, EventId, Goal, GoalDetail, GoalEdit, GoalId,
        GoalPredecessor, GoalRecord, GoalSummary, GoalTask, GoalVerdict, NewGoal, NewNote, NewTask,
        NotePage, NoteQuery, NoteTarget, OBSERVATION_KIND, Predecessor, Priority, Proposal,
        ProposalId, Provider, RunEvent, RunId, RunRecord, Submission, Task, TaskAction, TaskDetail,
        TaskEdit, TaskId, TaskRecord, TaskRun, TaskStatus, TaskStatusCounts, goal,
        scope::validate_path_globs, task,
    },
    infrastructure::{clock, location::runs_dir, proposals},
};

const APPLICATION_ID: i64 = 0x43545131;
pub(super) const MIGRATIONS: &[&str] = &[
    include_str!("../../migrations/0001_queue.sql"),
    include_str!("../../migrations/0002_supervisor.sql"),
    include_str!("../../migrations/0003_workspace_close.sql"),
    include_str!("../../migrations/0004_integration.sql"),
    include_str!("../../migrations/0005_run_leases.sql"),
    include_str!("../../migrations/0006_merge_queue.sql"),
    include_str!("../../migrations/0007_supervisors.sql"),
    include_str!("../../migrations/0008_goals.sql"),
    include_str!("../../migrations/0009_supervisor_mode.sql"),
    include_str!("../../migrations/0010_supervisor_binary_version.sql"),
    include_str!("../../migrations/0011_session_workspaces.sql"),
    include_str!("../../migrations/0012_queue_events.sql"),
    include_str!("../../migrations/0013_goal_draft.sql"),
    include_str!("../../migrations/0014_asks.sql"),
    include_str!("../../migrations/0015_task_required_evidence.sql"),
    include_str!("../../migrations/0016_observer.sql"),
    include_str!("../../migrations/0017_stuck_exit_ask.sql"),
    include_str!("../../migrations/0018_task_paths.sql"),
    include_str!("../../migrations/0019_task_goal_dependencies.sql"),
    include_str!("../../migrations/0020_task_priority.sql"),
    include_str!("../../migrations/0021_proposals.sql"),
    include_str!("../../migrations/0022_follow_up_triage.sql"),
    include_str!("../../migrations/0023_planners.sql"),
];
/// Ready tasks whose predecessors are completed, whose goal dependencies
/// are all closed as achieved (ADR-0038), that own no unfinished run and
/// whose goal, if any, is not a draft (ADR-0024 decision 5).
/// In ID order: the claim order (ADR-0040 decision 4) needs the whole
/// dependency graph, so [`claim_order`] applies it, not SQL.
const READY_QUERY: &str = "
    SELECT t.* FROM tasks t
    WHERE t.status = 'ready'
      AND NOT EXISTS (
        SELECT 1 FROM goals g WHERE g.id = t.goal_id AND g.status = 'draft'
      )
      AND NOT EXISTS (
        SELECT 1 FROM task_dependencies d JOIN tasks p ON p.id = d.predecessor_id
        WHERE d.task_id = t.id AND p.status <> 'completed'
      )
      AND NOT EXISTS (
        SELECT 1 FROM task_goal_dependencies gd JOIN goals g ON g.id = gd.goal_id
        WHERE gd.task_id = t.id
          AND NOT (g.closed_at IS NOT NULL AND g.verdict = 'achieved')
      )
      AND NOT EXISTS (
        SELECT 1 FROM task_runs r WHERE r.task_id = t.id
          AND r.status IN ('claimed','starting','running','validating','awaiting_integration',
                           'integrating','needs_session')
      )
    ORDER BY t.id";

pub struct SqliteQueue {
    pub(super) conn: Connection,
    /// `runs/` next to the database as opened now. A run's directory, worktree,
    /// receipt and log are resolved under it by run ID, never read from the
    /// absolute paths stored at claim time, so a moved queue keeps its runs.
    pub(super) runs_dir: PathBuf,
    /// Where every time the queue writes and every run ID it creates come
    /// from; the system clock and random UUIDs unless a test fixes them.
    pub(super) generators: Generators,
}

impl SqliteQueue {
    /// `user_version` a fully migrated queue reports.
    pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

    /// Explicit initialization is the only operation that creates a database file.
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        let mut queue = Self::connect(path.as_ref(), true)?;
        queue.migrate(true)?;
        queue.conn.pragma_update(None, "journal_mode", "WAL")?;
        Ok(queue)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut queue = Self::connect(path.as_ref(), false)?;
        queue.migrate(false)?;
        Ok(queue)
    }

    /// The queue reading the time and creating IDs through `generators`
    /// instead of the system clock and random UUIDs.
    pub fn with_generators(mut self, generators: Generators) -> Self {
        self.generators = generators;
        self
    }

    pub fn generators(&self) -> &Generators {
        &self.generators
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    fn connect(path: &Path, create: bool) -> Result<Self> {
        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        if create {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }
        let conn = Connection::open_with_flags(path, flags)
            .with_context(|| format!("open queue at {} (use init to create it)", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // Canonical, like the paths `supervise` plans under, so a relative or
        // symlinked `--db` still names the queue's real `runs/`.
        let db = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        Ok(Self {
            conn,
            runs_dir: runs_dir(&db),
            generators: clock::system(),
        })
    }

    fn migrate(&mut self, allow_initialize: bool) -> Result<()> {
        // Table rebuilds drop and rename tables that other rows reference, so
        // enforcement is off during migration (a no-op inside a transaction)
        // and integrity is checked explicitly before commit.
        self.conn.pragma_update(None, "foreign_keys", false)?;
        let result = self.apply_migrations(allow_initialize);
        self.conn.pragma_update(None, "foreign_keys", true)?;
        result
    }

    fn apply_migrations(&mut self, allow_initialize: bool) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let app: i64 = tx.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(
            version >= 0 && version <= MIGRATIONS.len() as i64,
            "unsupported queue schema version {version}; this binary supports {}",
            MIGRATIONS.len()
        );
        if version == 0 && app == 0 {
            ensure!(allow_initialize, "queue is not initialized; use init first");
            let objects: i64 = tx.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )?;
            ensure!(objects == 0, "database is not an empty dagq queue");
        } else {
            ensure!(app == APPLICATION_ID, "database is not a dagq queue");
        }
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            tx.execute_batch(migration)
                .context("apply queue migration")?;
            tx.pragma_update(None, "user_version", (index + 1) as i64)?;
        }
        if app != APPLICATION_ID {
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        }
        let violations: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })?;
        ensure!(
            violations == 0,
            "queue migration would break {violations} foreign key references"
        );
        tx.commit()?;
        Ok(())
    }
}

impl TaskStore for SqliteQueue {
    fn add(&mut self, new: NewTask) -> Result<Task> {
        // Rejected before taking the write lock; `new` checks it again.
        new.validate()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = insert_task(&tx, new, &self.generators.clock.timestamp())?;
        tx.commit()?;
        Ok(result)
    }

    fn list(&self, query: &TaskQuery) -> Result<TaskPage> {
        ensure!(query.limit > 0, "limit must be at least 1");
        let mut filters = Vec::new();
        let mut values = Vec::new();
        let statuses: Vec<TaskStatus> = match &query.status {
            StatusFilter::Open => [
                TaskStatus::Draft,
                TaskStatus::Submitted,
                TaskStatus::Ready,
                TaskStatus::InProgress,
                TaskStatus::Completed,
                TaskStatus::Canceled,
            ]
            .into_iter()
            .filter(|status| !status.is_terminal())
            .collect(),
            StatusFilter::Any => Vec::new(),
            StatusFilter::Only(statuses) => statuses.clone(),
        };
        if !matches!(query.status, StatusFilter::Any) {
            filters.push(format!(
                "status IN ({})",
                vec!["?"; statuses.len()].join(",")
            ));
            values.extend(statuses.iter().map(|s| Value::from(s.as_str().to_owned())));
        }
        if let Some(goal_id) = query.goal_id {
            filters.push("goal_id = ?".into());
            values.push(Value::from(goal_id.as_i64()));
        }
        let matching = if filters.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", filters.join(" AND "))
        };
        // One read snapshot keeps the page, its count and its runs consistent.
        let tx = self.conn.unchecked_transaction()?;
        let total: i64 = tx.query_row(
            &format!("SELECT count(*) FROM tasks{matching}"),
            params_from_iter(&values),
            |r| r.get(0),
        )?;
        if let Some(before) = query.before {
            filters.push("id <= ?".into());
            values.push(Value::from(before.as_i64()));
        }
        let page = if filters.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", filters.join(" AND "))
        };
        // One extra row tells whether another page follows.
        values.push(Value::from(i64::try_from(query.limit)?.saturating_add(1)));
        let mut tasks: Vec<Task> = tx
            .prepare(&format!(
                "SELECT * FROM tasks{page} ORDER BY id DESC LIMIT ?"
            ))?
            .query_map(params_from_iter(&values), task_row)?
            .collect::<rusqlite::Result<_>>()?;
        let next = if tasks.len() > query.limit {
            tasks.pop().map(|task| task.id())
        } else {
            None
        };
        let mut dependencies = tx.prepare(
            "SELECT predecessor_id FROM task_dependencies WHERE task_id=?1 ORDER BY predecessor_id",
        )?;
        let mut goal_dependencies = tx.prepare(GOAL_DEPENDENCIES_QUERY)?;
        let mut latest_run = tx.prepare(
            "SELECT id, status FROM task_runs WHERE task_id=?1 ORDER BY rowid DESC LIMIT 1",
        )?;
        let tasks = tasks
            .into_iter()
            .map(|task| {
                let dependencies = dependencies
                    .query_map([task.id()], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                let goal_dependencies = goal_dependencies
                    .query_map([task.id()], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                let latest_run = latest_run
                    .query_row([task.id()], |row| {
                        Ok(LatestRun {
                            id: row.get("id")?,
                            status: enum_col(row, "status")?,
                        })
                    })
                    .optional()?;
                Ok(TaskListItem::new(
                    task,
                    dependencies,
                    goal_dependencies,
                    latest_run,
                    query.full,
                ))
            })
            .collect::<Result<_>>()?;
        Ok(TaskPage {
            tasks,
            next,
            total: usize::try_from(total)?,
        })
    }

    fn show(&mut self, task_id: TaskId) -> Result<TaskDetail> {
        // One read snapshot keeps task status, run history and events consistent.
        let tx = self.conn.transaction()?;
        let task = read_task(&tx, task_id)?;
        let dependencies = tx.prepare(
            "SELECT predecessor_id FROM task_dependencies WHERE task_id=?1 ORDER BY predecessor_id"
        )?.query_map([task_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let goal_dependencies = tx
            .prepare(GOAL_DEPENDENCIES_QUERY)?
            .query_map([task_id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let runs = tx
            .prepare("SELECT * FROM task_runs WHERE task_id=?1 ORDER BY rowid")?
            .query_map([task_id], run_row(&self.runs_dir))?
            .collect::<rusqlite::Result<_>>()?;
        let events = tx
            .prepare("SELECT * FROM run_events WHERE task_id=?1 ORDER BY id")?
            .query_map([task_id], event_row)?
            .collect::<rusqlite::Result<_>>()?;
        let processes = super::runtime_store::processes_for_task(&tx, task_id)?;
        tx.commit()?;
        Ok(TaskDetail {
            task,
            dependencies,
            goal_dependencies,
            runs,
            events,
            processes,
        })
    }

    fn transition(&mut self, task_id: TaskId, action: TaskAction) -> Result<Task> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = transition_task(&tx, task_id, action, &self.generators.clock.timestamp())?;
        tx.commit()?;
        Ok(result)
    }

    fn add_dependency(&mut self, task_id: TaskId, predecessor_id: TaskId) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_dependency(
            &tx,
            task_id,
            predecessor_id,
            &self.generators.clock.timestamp(),
        )?;
        tx.commit()?;
        Ok(())
    }

    fn remove_dependency(&mut self, task_id: TaskId, predecessor_id: TaskId) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        task::check_dependencies_editable(&read_task(&tx, task_id)?)?;
        let changed = tx.execute(
            "DELETE FROM task_dependencies WHERE task_id=?1 AND predecessor_id=?2",
            params![task_id, predecessor_id],
        )?;
        ensure!(
            changed == 1,
            "dependency {task_id} -> {predecessor_id} does not exist"
        );
        touch(&tx, task_id, &self.generators.clock.timestamp())?;
        event(
            &tx,
            task_id,
            None,
            "dependency_removed",
            json!({"predecessor_id": predecessor_id}),
        )?;
        tx.commit()?;
        Ok(())
    }

    fn add_goal_dependency(&mut self, task_id: TaskId, goal_id: GoalId) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_goal_dependency(&tx, task_id, goal_id, &self.generators.clock.timestamp())?;
        tx.commit()?;
        Ok(())
    }

    fn remove_goal_dependency(&mut self, task_id: TaskId, goal_id: GoalId) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        task::check_dependencies_editable(&read_task(&tx, task_id)?)?;
        let changed = tx.execute(
            "DELETE FROM task_goal_dependencies WHERE task_id=?1 AND goal_id=?2",
            params![task_id, goal_id],
        )?;
        ensure!(
            changed == 1,
            "dependency {task_id} -> goal {goal_id} does not exist"
        );
        touch(&tx, task_id, &self.generators.clock.timestamp())?;
        event(
            &tx,
            task_id,
            None,
            "goal_dependency_removed",
            json!({"goal_id": goal_id}),
        )?;
        tx.commit()?;
        Ok(())
    }

    fn candidates(&self) -> Result<Vec<Task>> {
        let tx = self.conn.unchecked_transaction()?;
        let order = claim_order(&tx)?;
        let mut ready = ready_tasks(&tx)?;
        ready.sort_by_key(|task| order.iter().position(|id| *id == task.id()));
        Ok(ready)
    }

    fn graph_input(&self) -> Result<GraphInput> {
        let tx = self.conn.unchecked_transaction()?;
        read_graph_input(&tx)
    }

    fn claim(&mut self, base_commit: &CommitSha) -> Result<ClaimOutcome> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome = claim_task(
            &tx,
            &self.runs_dir,
            self.generators.ids.as_ref(),
            self.generators.clock.system_time(),
            base_commit,
            &[],
        )?;
        tx.commit()?;
        Ok(outcome)
    }

    fn predecessors(&self, task_id: TaskId) -> Result<Vec<Predecessor>> {
        landed_tasks(
            &self.conn,
            &self.runs_dir,
            "SELECT p.* FROM task_dependencies d JOIN tasks p ON p.id = d.predecessor_id
             WHERE d.task_id = ?1 ORDER BY p.id",
            task_id.as_i64(),
        )
    }

    fn goal_predecessors(&self, task_id: TaskId) -> Result<Vec<GoalPredecessor>> {
        let goals: Vec<Goal> = self
            .conn
            .prepare(
                "SELECT g.* FROM task_goal_dependencies d JOIN goals g ON g.id = d.goal_id
                 WHERE d.task_id = ?1 ORDER BY g.id",
            )?
            .query_map([task_id], goal_row)?
            .collect::<rusqlite::Result<_>>()?;
        goals
            .into_iter()
            .map(|goal| {
                let tasks = landed_tasks(
                    &self.conn,
                    &self.runs_dir,
                    "SELECT * FROM tasks WHERE goal_id = ?1 AND status = 'completed' ORDER BY id",
                    goal.id().as_i64(),
                )?;
                Ok(GoalPredecessor { goal, tasks })
            })
            .collect()
    }

    fn tasks_in_progress(&self) -> Result<Vec<Task>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM tasks WHERE status = 'in_progress' ORDER BY id")?
            .query_map([], task_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn add_goal(&mut self, new: NewGoal) -> Result<Goal> {
        // Rejected before taking the write lock; `new` checks it again.
        new.validate()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let goal = Goal::new(
            GoalId::new(next_id(&tx, "goals")?),
            new,
            self.generators.clock.timestamp(),
        )?;
        let id = goal.id();
        tx.execute(
            "INSERT INTO goals(id, title, description, acceptance, constraints, doc, status,
                               created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                id,
                goal.title(),
                goal.description(),
                goal.acceptance(),
                goal.constraints(),
                goal.doc(),
                goal.status().as_str(),
                goal.created_at(),
                goal.updated_at()
            ],
        )?;
        let result = read_goal(&tx, id)?;
        goal_event(&tx, id, "goal_created", json!({"goal": result}))?;
        tx.commit()?;
        Ok(result)
    }

    fn list_goals(&self) -> Result<Vec<GoalSummary>> {
        let goals: Vec<Goal> = self
            .conn
            .prepare("SELECT * FROM goals ORDER BY id")?
            .query_map([], goal_row)?
            .collect::<rusqlite::Result<_>>()?;
        goals
            .into_iter()
            .map(|goal| {
                Ok(GoalSummary {
                    id: goal.id(),
                    status: goal.status(),
                    closed: goal.is_closed(),
                    verdict: goal.verdict(),
                    tasks: task_counts(&self.conn, goal.id())?,
                    title: goal.into_title(),
                })
            })
            .collect()
    }

    fn show_goal(&mut self, goal_id: GoalId) -> Result<GoalDetail> {
        let tx = self.conn.transaction()?;
        let goal = read_goal(&tx, goal_id)?;
        let tasks = tx
            .prepare("SELECT id, title, status FROM tasks WHERE goal_id=?1 ORDER BY id")?
            .query_map([goal_id], goal_task_row)?
            .collect::<rusqlite::Result<_>>()?;
        let dependents = tx
            .prepare(
                "SELECT t.id, t.title, t.status FROM task_goal_dependencies d
                 JOIN tasks t ON t.id = d.task_id
                 WHERE d.goal_id=?1 AND t.status NOT IN ('completed','canceled') ORDER BY t.id",
            )?
            .query_map([goal_id], goal_task_row)?
            .collect::<rusqlite::Result<_>>()?;
        let events = tx
            .prepare("SELECT * FROM run_events WHERE goal_id=?1 ORDER BY id")?
            .query_map([goal_id], event_row)?
            .collect::<rusqlite::Result<_>>()?;
        tx.commit()?;
        Ok(GoalDetail {
            closed: goal.is_closed(),
            goal,
            tasks,
            dependents,
            events,
        })
    }

    fn edit_goal(&mut self, goal_id: GoalId, edit: GoalEdit) -> Result<Goal> {
        ensure!(!edit.is_empty(), "goal edit changes nothing");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old = read_goal(&tx, goal_id)?;
        // The event keeps the goal before the edit, which consumes it.
        let old_json = serde_json::to_value(&old)?;
        let new = goal::edit(old, edit)?;
        tx.execute(
            "UPDATE goals SET title=?1, description=?2, acceptance=?3, constraints=?4, doc=?5,
             updated_at=?6 WHERE id=?7",
            params![
                new.title(),
                new.description(),
                new.acceptance(),
                new.constraints(),
                new.doc(),
                self.generators.clock.timestamp(),
                goal_id
            ],
        )?;
        goal_event(
            &tx,
            goal_id,
            "goal_updated",
            json!({"old": old_json, "new": new}),
        )?;
        let result = read_goal(&tx, goal_id)?;
        tx.commit()?;
        Ok(result)
    }

    fn close_goal(&mut self, goal_id: GoalId, verdict: GoalVerdict) -> Result<Goal> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let goal = read_goal(&tx, goal_id)?;
        let counts = task_counts(&tx, goal_id)?;
        let closed = goal::close(goal, verdict, &counts, self.generators.clock.timestamp())?;
        // `closed_at IS NULL` only detects a concurrent close; the domain
        // decided whether this one may happen.
        let changed = tx.execute(
            "UPDATE goals SET closed_at=?1, verdict=?2, updated_at=?3
             WHERE id=?4 AND closed_at IS NULL",
            params![
                closed.closed_at(),
                closed.verdict().map(GoalVerdict::as_str),
                closed.updated_at(),
                goal_id
            ],
        )?;
        ensure!(changed == 1, "goal {goal_id} was closed concurrently");
        goal_event(
            &tx,
            goal_id,
            "goal_closed",
            json!({"verdict": verdict, "tasks": counts}),
        )?;
        let result = read_goal(&tx, goal_id)?;
        tx.commit()?;
        Ok(result)
    }

    fn submit(&mut self, submission: Submission) -> Result<Proposal> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let proposal = proposals::submit(&tx, submission, &self.generators.clock.timestamp())?;
        tx.commit()?;
        Ok(proposal)
    }

    fn approve_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let proposal = proposals::approve(&tx, proposal_id, &self.generators.clock.timestamp())?;
        tx.commit()?;
        Ok(proposal)
    }

    fn send_back_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let proposal = proposals::send_back(&tx, proposal_id, &self.generators.clock.timestamp())?;
        tx.commit()?;
        Ok(proposal)
    }

    fn show_proposal(&self, proposal_id: ProposalId) -> Result<Proposal> {
        let tx = self.conn.unchecked_transaction()?;
        proposals::read(&tx, proposal_id)
    }

    fn proposals(&self, all: bool) -> Result<Vec<Proposal>> {
        let tx = self.conn.unchecked_transaction()?;
        proposals::list(&tx, all)
    }

    fn ready_goal(&mut self, goal_id: GoalId) -> Result<Goal> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let draft = read_goal(&tx, goal_id)?;
        let from = draft.status();
        let opened = goal::ready(draft)?;
        tx.execute(
            "UPDATE goals SET status=?1, updated_at=?2 WHERE id=?3",
            params![
                opened.status().as_str(),
                self.generators.clock.timestamp(),
                goal_id
            ],
        )?;
        goal_event(
            &tx,
            goal_id,
            "goal_status_changed",
            json!({"from": from, "to": opened.status()}),
        )?;
        let result = read_goal(&tx, goal_id)?;
        tx.commit()?;
        Ok(result)
    }

    fn add_note(&mut self, note: NewNote) -> Result<RunEvent> {
        note.validate()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let payload = note.payload();
        match &note.target {
            NoteTarget::Task(task_id) => {
                read_task(&tx, *task_id)?;
                event(&tx, *task_id, None, OBSERVATION_KIND, payload)?;
            }
            NoteTarget::Run(run_id) => {
                let task_id: TaskId = tx
                    .query_row("SELECT task_id FROM task_runs WHERE id=?1", [run_id], |r| {
                        r.get(0)
                    })
                    .optional()?
                    .with_context(|| format!("run {run_id} does not exist"))?;
                event(&tx, task_id, Some(run_id), OBSERVATION_KIND, payload)?;
            }
            NoteTarget::Goal(goal_id) => {
                read_goal(&tx, *goal_id)?;
                goal_event(&tx, *goal_id, OBSERVATION_KIND, payload)?;
            }
        }
        let result = tx.query_row(
            "SELECT * FROM run_events WHERE id=?1",
            [tx.last_insert_rowid()],
            event_row,
        )?;
        tx.commit()?;
        Ok(result)
    }

    fn notes(&self, query: &NoteQuery) -> Result<NotePage> {
        ensure!(query.limit > 0, "limit must be at least 1");
        let mut filters = vec!["kind = ?".to_owned()];
        let mut values = vec![Value::from(OBSERVATION_KIND.to_owned())];
        if let Some(goal_id) = query.goal_id {
            filters.push(
                "(goal_id = ? OR task_id IN (SELECT id FROM tasks WHERE goal_id = ?))".into(),
            );
            values.extend([Value::from(goal_id.as_i64()), Value::from(goal_id.as_i64())]);
        }
        if let Some(task_id) = query.task_id {
            filters.push("task_id = ?".into());
            values.push(Value::from(task_id.as_i64()));
        }
        // Past a cursor the page runs forward from it; without one it is
        // the latest `limit` notes. Either way it is printed oldest first.
        let order = if let Some(since) = query.since {
            filters.push("id > ?".into());
            values.push(Value::from(since.as_i64()));
            "ASC"
        } else {
            "DESC"
        };
        values.push(Value::from(i64::try_from(query.limit)?));
        let mut notes: Vec<RunEvent> = self
            .conn
            .prepare(&format!(
                "SELECT * FROM run_events WHERE {} ORDER BY id {order} LIMIT ?",
                filters.join(" AND ")
            ))?
            .query_map(params_from_iter(&values), event_row)?
            .collect::<rusqlite::Result<_>>()?;
        notes.sort_by_key(|note| note.id);
        let cursor = notes
            .last()
            .map(|note| note.id)
            .or(query.since)
            .unwrap_or(EventId::new(0));
        Ok(NotePage { notes, cursor })
    }

    fn set_goal(&mut self, task_id: TaskId, goal_id: Option<GoalId>) -> Result<Task> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = read_task(&tx, task_id)?;
        let from = task.goal_id();
        let task = task::set_goal(task, goal_id)?;
        if let Some(goal_id) = task.goal_id() {
            goal::check_accepts_tasks(&read_goal(&tx, goal_id)?)?;
            if from != Some(goal_id) {
                // The goal would wait for the task: the task must not wait
                // for the goal (ADR-0038).
                let direct: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM task_goal_dependencies
                     WHERE task_id=?1 AND goal_id=?2)",
                    params![task_id, goal_id],
                    |r| r.get(0),
                )?;
                let cycle = waits_for(&tx, Node::Task(task_id), Node::Goal(goal_id))?;
                task::check_membership_acyclic(&task, goal_id, direct, cycle)?;
            }
        }
        if from != task.goal_id() {
            tx.execute(
                "UPDATE tasks SET goal_id=?1, updated_at=?2 WHERE id=?3",
                params![task.goal_id(), self.generators.clock.timestamp(), task_id],
            )?;
            event(
                &tx,
                task_id,
                None,
                "task_goal_changed",
                json!({"from": from, "to": task.goal_id()}),
            )?;
        }
        let result = read_task(&tx, task_id)?;
        tx.commit()?;
        Ok(result)
    }

    fn set_paths(&mut self, task_id: TaskId, paths: Vec<String>) -> Result<Task> {
        // Checked before the task is read, so a bad glob is reported first.
        validate_path_globs(&paths)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = read_task(&tx, task_id)?;
        let from = task.paths().to_vec();
        let task = task::set_paths(task, paths)?;
        if from != task.paths() {
            tx.execute(
                "UPDATE tasks SET paths=?1, updated_at=?2 WHERE id=?3",
                params![
                    serde_json::to_string(task.paths())?,
                    self.generators.clock.timestamp(),
                    task_id
                ],
            )?;
            event(
                &tx,
                task_id,
                None,
                "task_paths_changed",
                json!({"from": from, "to": task.paths()}),
            )?;
        }
        let result = read_task(&tx, task_id)?;
        tx.commit()?;
        Ok(result)
    }

    fn edit_task(&mut self, task_id: TaskId, edit: TaskEdit) -> Result<Task> {
        ensure!(!edit.is_empty(), "task edit changes nothing");
        // Checked before the task is read, so a bad value is reported first.
        edit.validate()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old = read_task(&tx, task_id)?;
        // The event keeps the fields before the edit, which consumes the task.
        let old_json = serde_json::to_value(&old)?;
        let new = task::edit(old, edit)?;
        let new_json = serde_json::to_value(&new)?;
        let (mut from, mut to) = (serde_json::Map::new(), serde_json::Map::new());
        for field in EDITABLE_TASK_FIELDS {
            if old_json[field] != new_json[field] {
                from.insert(field.to_owned(), old_json[field].clone());
                to.insert(field.to_owned(), new_json[field].clone());
            }
        }
        if !to.is_empty() {
            tx.execute(
                "UPDATE tasks SET title=?1, description=?2, acceptance=?3,
                 verification_commands=?4, required_evidence=?5, paths=?6, context=?7,
                 updated_at=?8 WHERE id=?9",
                params![
                    new.title(),
                    new.description(),
                    new.acceptance(),
                    serde_json::to_string(new.verification_commands())?,
                    serde_json::to_string(new.required_evidence())?,
                    serde_json::to_string(new.paths())?,
                    new.context(),
                    self.generators.clock.timestamp(),
                    task_id
                ],
            )?;
            event(
                &tx,
                task_id,
                None,
                "task_edited",
                json!({"from": from, "to": to}),
            )?;
        }
        let result = read_task(&tx, task_id)?;
        tx.commit()?;
        Ok(result)
    }

    fn set_priority(&mut self, task_id: TaskId, priority: Priority) -> Result<Task> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = read_task(&tx, task_id)?;
        let from = task.priority();
        let task = task::set_priority(task, priority)?;
        if from != task.priority() {
            tx.execute(
                "UPDATE tasks SET priority=?1, updated_at=?2 WHERE id=?3",
                params![
                    task.priority().as_i64(),
                    self.generators.clock.timestamp(),
                    task_id
                ],
            )?;
            event(
                &tx,
                task_id,
                None,
                "task_priority_changed",
                json!({"from": from, "to": task.priority()}),
            )?;
        }
        let result = read_task(&tx, task_id)?;
        tx.commit()?;
        Ok(result)
    }
}

/// The fields `dagq edit` replaces, as the task JSON names them; `task_edited`
/// records the ones that changed.
const EDITABLE_TASK_FIELDS: [&str; 7] = [
    "title",
    "description",
    "acceptance",
    "verification_commands",
    "required_evidence",
    "paths",
    "context",
];

/// Register `new` inside the caller's write transaction with
/// `task_created` and its dependencies: what `add` does, and what a
/// follow-up triage's `adopt` does in the transaction that cancels the draft.
pub(super) fn insert_task(tx: &Connection, new: NewTask, now: &str) -> Result<Task> {
    let dependencies = new.dependencies.clone();
    let goal_dependencies = new.goal_dependencies.clone();
    let task = Task::new(TaskId::new(next_id(tx, "tasks")?), new, now.to_owned())?;
    if let Some(goal_id) = task.goal_id() {
        goal::check_accepts_tasks(&read_goal(tx, goal_id)?)?;
    }
    let id = task.id();
    tx.execute(
            "INSERT INTO tasks(id, title, description, acceptance, verification_commands, status, goal_id,
                               context, required_evidence, paths, priority, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![id, task.title(), task.description(), task.acceptance(),
                serde_json::to_string(task.verification_commands())?, task.status().as_str(),
                task.goal_id(), task.context(), serde_json::to_string(task.required_evidence())?,
                serde_json::to_string(task.paths())?, task.priority().as_i64(), task.created_at(),
                task.updated_at()],
        )?;
    event(
        tx,
        id,
        None,
        "task_created",
        json!({"goal_id": task.goal_id()}),
    )?;
    // Inserted after the task, which is in its goal already, so the
    // cycle checks see the goal's wait for it (ADR-0038).
    for predecessor in dependencies {
        insert_dependency(tx, id, predecessor, now)?;
    }
    for goal_id in goal_dependencies {
        insert_goal_dependency(tx, id, goal_id, now)?;
    }
    read_task(tx, id)
}

pub(super) fn read_goal(conn: &Connection, goal_id: GoalId) -> Result<Goal> {
    conn.query_row("SELECT * FROM goals WHERE id=?1", [goal_id], goal_row)
        .optional()?
        .with_context(|| format!("goal {goal_id} does not exist"))
}

/// The goals a task depends on, ascending.
const GOAL_DEPENDENCIES_QUERY: &str =
    "SELECT goal_id FROM task_goal_dependencies WHERE task_id=?1 ORDER BY goal_id";

fn goal_task_row(row: &Row<'_>) -> rusqlite::Result<GoalTask> {
    Ok(GoalTask {
        id: row.get("id")?,
        title: row.get("title")?,
        status: enum_col(row, "status")?,
    })
}

/// The tasks `query` selects by `key`, each with the run that landed it.
fn landed_tasks(
    conn: &Connection,
    runs_dir: &Path,
    query: &str,
    key: i64,
) -> Result<Vec<Predecessor>> {
    let tasks: Vec<Task> = conn
        .prepare(query)?
        .query_map([key], task_row)?
        .collect::<rusqlite::Result<_>>()?;
    tasks
        .into_iter()
        .map(|task| {
            // At most one run per task is integrated (`one_integrated_run_per_task`).
            let integrated_run = conn
                .query_row(
                    "SELECT * FROM task_runs WHERE task_id = ?1 AND status = 'integrated'",
                    [task.id()],
                    run_row(runs_dir),
                )
                .optional()?;
            Ok(Predecessor {
                task,
                integrated_run,
            })
        })
        .collect()
}

/// A vertex of the wait graph (ADR-0038): a task waits for its predecessor
/// tasks and the goals it depends on, and a goal waits for its tasks.
#[derive(Debug, Clone, Copy)]
enum Node {
    Task(TaskId),
    Goal(GoalId),
}

impl Node {
    fn key(self) -> (&'static str, i64) {
        match self {
            Self::Task(id) => ("task", id.as_i64()),
            Self::Goal(id) => ("goal", id.as_i64()),
        }
    }
}

/// Whether `from` already waits for `to`, directly or not, over every task
/// and goal: the edge `to -> from` would close a cycle. Found in SQL
/// because it needs the whole graph; whether that rejects the edge is the
/// domain's rule (ADR-0013).
fn waits_for(conn: &Connection, from: Node, to: Node) -> Result<bool> {
    let (from_kind, from_id) = from.key();
    let (to_kind, to_id) = to.key();
    Ok(conn.query_row(
        "WITH RECURSIVE
           edges(from_kind, from_id, to_kind, to_id) AS (
             SELECT 'task', task_id, 'task', predecessor_id FROM task_dependencies
             UNION ALL
             SELECT 'task', task_id, 'goal', goal_id FROM task_goal_dependencies
             UNION ALL
             SELECT 'goal', goal_id, 'task', id FROM tasks WHERE goal_id IS NOT NULL
           ),
           reached(kind, id) AS (
             SELECT ?1, ?2
             UNION
             SELECT e.to_kind, e.to_id FROM edges e
               JOIN reached r ON e.from_kind = r.kind AND e.from_id = r.id
           )
         SELECT EXISTS(SELECT 1 FROM reached WHERE kind = ?3 AND id = ?4)",
        params![from_kind, from_id, to_kind, to_id],
        |r| r.get(0),
    )?)
}

/// The ID an `AUTOINCREMENT` insert into `table` would take now: one past
/// the highest ever used. Inside the caller's write transaction no other
/// insert can take it first, so the aggregate is built with its ID before
/// it is saved.
pub(super) fn next_id(conn: &Connection, table: &str) -> Result<i64> {
    Ok(conn.query_row(
        &format!(
            "SELECT max(coalesce((SELECT seq FROM sqlite_sequence WHERE name=?1), 0),
                        coalesce((SELECT max(id) FROM {table}), 0)) + 1"
        ),
        [table],
        |r| r.get(0),
    )?)
}

fn task_counts(conn: &Connection, goal_id: GoalId) -> Result<TaskStatusCounts> {
    let mut counts = TaskStatusCounts::default();
    let mut rows =
        conn.prepare("SELECT status, count(*) AS n FROM tasks WHERE goal_id=?1 GROUP BY status")?;
    for row in rows.query_map([goal_id], |row| {
        Ok((
            enum_col::<TaskStatus>(row, "status")?,
            row.get::<_, i64>("n")?,
        ))
    })? {
        let (status, n) = row?;
        counts.count(status, usize::try_from(n)?);
    }
    Ok(counts)
}

pub(super) fn goal_event(
    conn: &Connection,
    goal_id: GoalId,
    kind: &str,
    payload: serde_json::Value,
) -> Result<()> {
    conn.execute(
        "INSERT INTO run_events(goal_id,kind,payload) VALUES (?1,?2,?3)",
        params![goal_id, kind, serde_json::to_string(&payload)?],
    )?;
    Ok(())
}

/// The claimable tasks, in ID order.
fn ready_tasks(conn: &Connection) -> Result<Vec<Task>> {
    Ok(conn
        .prepare(READY_QUERY)?
        .query_map([], task_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// The unfinished tasks with their predecessors, goal dependencies and
/// priorities, and the IDs of the claimable ones, read inside the caller's
/// transaction so they are one snapshot.
fn read_graph_input(conn: &Connection) -> Result<GraphInput> {
    let tasks: Vec<Task> = conn
        .prepare("SELECT * FROM tasks WHERE status IN ('draft','submitted','ready','in_progress') ORDER BY id")?
        .query_map([], task_row)?
        .collect::<rusqlite::Result<_>>()?;
    let mut dependencies = conn.prepare(
        "SELECT predecessor_id FROM task_dependencies WHERE task_id=?1 ORDER BY predecessor_id",
    )?;
    let mut goal_dependencies = conn.prepare(
        "SELECT g.* FROM task_goal_dependencies d JOIN goals g ON g.id = d.goal_id
         WHERE d.task_id=?1 ORDER BY g.id",
    )?;
    let tasks = tasks
        .into_iter()
        .map(|task| {
            let goal_status = task
                .goal_id()
                .map(|goal_id| read_goal(conn, goal_id).map(|goal| goal.status()))
                .transpose()?;
            Ok(GraphTask {
                depends_on: dependencies
                    .query_map([task.id()], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?,
                goal_dependencies: goal_dependencies
                    .query_map([task.id()], goal_row)?
                    .map(|goal| {
                        goal.map(|goal| GraphGoalDependency {
                            goal_id: goal.id(),
                            verdict: goal.verdict(),
                        })
                    })
                    .collect::<rusqlite::Result<_>>()?,
                goal_status,
                id: task.id(),
                status: task.status(),
                priority: task.priority(),
                goal_id: task.goal_id(),
                title: task.into_title(),
            })
        })
        .collect::<Result<_>>()?;
    let candidates = conn
        .prepare(READY_QUERY)?
        .query_map([], |r| r.get("id"))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(GraphInput { tasks, candidates })
}

/// The claimable task IDs in claim order (ADR-0040 decision 4), from the
/// one ordering [`dependency_graph`] applies, so `candidates`, `graph` and
/// every claim agree.
fn claim_order(conn: &Connection) -> Result<Vec<TaskId>> {
    Ok(dependency_graph(read_graph_input(conn)?, None).candidates)
}

/// Reserve a dependency-ready task inside the caller's write transaction:
/// the first task of `order` that is still a candidate, or the first
/// candidate in claim order when none of them is (an empty `order` means
/// claim order). There
/// is no queue-wide execution slot; `one_unfinished_run_per_task` is the
/// only limit, so concurrent claims take different tasks. The run is
/// created at `at` with an ID from `ids`.
pub(super) fn claim_task(
    tx: &Connection,
    runs_dir: &Path,
    ids: &dyn IdGenerator,
    at: SystemTime,
    base_commit: &CommitSha,
    order: &[TaskId],
) -> Result<ClaimOutcome> {
    let mut ready = ready_tasks(tx)?;
    if ready.is_empty() {
        return Ok(ClaimOutcome::NoReadyTask);
    }
    let position = |ids: &[TaskId]| {
        ids.iter()
            .find_map(|id| ready.iter().position(|task| task.id() == *id))
    };
    let preferred = match position(order) {
        Some(index) => index,
        None => position(&claim_order(tx)?).unwrap_or(0),
    };
    let task = task::claim(ready.swap_remove(preferred))?;
    let now = timestamp(at);
    let run_id = RunId::new(ids.uuid())?;
    let run = TaskRun::new(run_id, &task, base_commit, Provider::Claude, now.clone())?;
    // `status='ready'` only detects a concurrent change; the domain decided the claim.
    ensure!(
        tx.execute(
            "UPDATE tasks SET status=?1, updated_at=?2 WHERE id=?3 AND status='ready'",
            params![task.status().as_str(), now, task.id()],
        )? == 1,
        "task {} changed concurrently",
        task.id()
    );
    tx.execute(
        "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            run.id(),
            run.task_id(),
            run.status().as_str(),
            run.requested_provider().as_str(),
            run.actual_provider().as_str(),
            run.base_commit(),
            run.created_at()
        ],
    )?;
    event(
        tx,
        task.id(),
        Some(run.id()),
        "run_claimed",
        json!({"from": "ready", "to": task.status(), "provider": run.actual_provider()}),
    )?;
    let run = run.relocated(runs_dir);
    Ok(ClaimOutcome::Claimed { run: Box::new(run) })
}

/// Executing, awaiting or undergoing integration, or waiting for a session;
/// the same set as `one_unfinished_run_per_task`.
/// Apply `action` to the task inside the caller's transaction, recording
/// `task_status_changed`: what `transition` does, and what the triage does
/// to the task of the run it triaged.
pub(super) fn transition_task(
    conn: &Connection,
    task_id: TaskId,
    action: TaskAction,
    now: &str,
) -> Result<Task> {
    let task = read_task(conn, task_id)?;
    let from = task.status();
    let task = task::transition(task, action, has_unfinished_run(conn, task_id)?)?;
    // `status=?3` only detects a concurrent change; the domain decided the move.
    let changed = conn.execute(
        "UPDATE tasks SET status=?1, updated_at=?2 WHERE id=?3 AND status=?4",
        params![task.status().as_str(), now, task_id, from.as_str()],
    )?;
    ensure!(changed == 1, "task {task_id} changed concurrently");
    if action == TaskAction::BypassReview {
        // A person readied the task past plan review: its follow-ups count
        // again from 1 (ADR-0037 decision 6, kept by ADR-0041 decision 16).
        // The follow-up triage's adopt writes the draft's depth afterwards.
        conn.execute("UPDATE tasks SET follow_up_depth=0 WHERE id=?1", [task_id])?;
    }
    event(
        conn,
        task_id,
        None,
        "task_status_changed",
        json!({"from": from, "to": task.status()}),
    )?;
    // A person skipped plan review (ADR-0041 decision 8).
    if action == TaskAction::BypassReview {
        event(
            conn,
            task_id,
            None,
            "review_bypassed",
            json!({"from": from}),
        )?;
    }
    read_task(conn, task_id)
}

fn has_unfinished_run(conn: &Connection, task_id: TaskId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM task_runs WHERE task_id=?1
         AND status IN ('claimed','starting','running','validating','awaiting_integration',
                        'integrating','needs_session'))",
        [task_id],
        |r| r.get(0),
    )?)
}

pub(super) fn read_task(conn: &Connection, task_id: TaskId) -> Result<Task> {
    conn.query_row("SELECT * FROM tasks WHERE id=?1", [task_id], task_row)
        .optional()?
        .with_context(|| format!("task {task_id} does not exist"))
}

fn insert_dependency(
    conn: &Connection,
    task_id: TaskId,
    predecessor_id: TaskId,
    now: &str,
) -> Result<()> {
    task::check_not_self(task_id, predecessor_id)?;
    task::check_dependencies_editable(&read_task(conn, task_id)?)?;
    read_task(conn, predecessor_id)?;
    // Through goals too: the predecessor may wait for a goal that waits
    // for the task (ADR-0038).
    let cycle = waits_for(conn, Node::Task(predecessor_id), Node::Task(task_id))?;
    task::check_acyclic(task_id, predecessor_id, cycle)?;
    let inserted = conn.execute(
        "INSERT INTO task_dependencies(task_id, predecessor_id) VALUES (?1,?2)
         ON CONFLICT(task_id, predecessor_id) DO NOTHING",
        params![task_id, predecessor_id],
    )?;
    if inserted != 0 {
        touch(conn, task_id, now)?;
        event(
            conn,
            task_id,
            None,
            "dependency_added",
            json!({"predecessor_id": predecessor_id}),
        )?;
    }
    Ok(())
}

fn insert_goal_dependency(
    conn: &Connection,
    task_id: TaskId,
    goal_id: GoalId,
    now: &str,
) -> Result<()> {
    let task = read_task(conn, task_id)?;
    task::check_dependencies_editable(&task)?;
    read_goal(conn, goal_id)?;
    task::check_not_own_goal(&task, goal_id)?;
    let cycle = waits_for(conn, Node::Goal(goal_id), Node::Task(task_id))?;
    task::check_goal_acyclic(task_id, goal_id, cycle)?;
    let inserted = conn.execute(
        "INSERT INTO task_goal_dependencies(task_id, goal_id) VALUES (?1,?2)
         ON CONFLICT(task_id, goal_id) DO NOTHING",
        params![task_id, goal_id],
    )?;
    if inserted != 0 {
        touch(conn, task_id, now)?;
        event(
            conn,
            task_id,
            None,
            "goal_dependency_added",
            json!({"goal_id": goal_id}),
        )?;
    }
    Ok(())
}

fn touch(conn: &Connection, task_id: TaskId, now: &str) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET updated_at=?1 WHERE id=?2",
        params![now, task_id],
    )?;
    Ok(())
}

pub(super) fn event(
    conn: &Connection,
    task_id: TaskId,
    run_id: Option<&RunId>,
    kind: &str,
    payload: serde_json::Value,
) -> Result<()> {
    conn.execute(
        "INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (?1,?2,?3,?4)",
        params![task_id, run_id, kind, serde_json::to_string(&payload)?],
    )?;
    Ok(())
}

pub(super) fn enum_col<T: FromStr<Err = DomainError>>(
    row: &Row<'_>,
    name: &str,
) -> rusqlite::Result<T> {
    let value: String = row.get(name)?;
    value.parse().map_err(|error: DomainError| {
        rusqlite::Error::FromSqlConversionFailure(
            row.as_ref().column_index(name).unwrap_or(0),
            Type::Text,
            Box::new(error),
        )
    })
}

pub(super) fn json_col<T: DeserializeOwned>(row: &Row<'_>, name: &str) -> rusqlite::Result<T> {
    let value: String = row.get(name)?;
    serde_json::from_str(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            row.as_ref().column_index(name).unwrap_or(0),
            Type::Text,
            Box::new(error),
        )
    })
}

fn task_row(row: &Row<'_>) -> rusqlite::Result<Task> {
    Task::restore(TaskRecord {
        id: row.get("id")?,
        title: row.get("title")?,
        description: row.get("description")?,
        acceptance: row.get("acceptance")?,
        verification_commands: json_col(row, "verification_commands")?,
        required_evidence: json_col(row, "required_evidence")?,
        paths: json_col(row, "paths")?,
        priority: Priority::from_i64(row.get("priority")?).map_err(restore_error)?,
        status: enum_col(row, "status")?,
        goal_id: row.get("goal_id")?,
        context: row.get("context")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
    .map_err(restore_error)
}

fn goal_row(row: &Row<'_>) -> rusqlite::Result<Goal> {
    let verdict: Option<String> = row.get("verdict")?;
    Goal::restore(GoalRecord {
        id: row.get("id")?,
        title: row.get("title")?,
        description: row.get("description")?,
        acceptance: row.get("acceptance")?,
        constraints: row.get("constraints")?,
        doc: row.get("doc")?,
        status: enum_col(row, "status")?,
        closed_at: row.get("closed_at")?,
        verdict: verdict
            .map(|_| enum_col::<GoalVerdict>(row, "verdict"))
            .transpose()?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
    .map_err(restore_error)
}

/// A stored row the domain refuses to restore, reported like a column that
/// does not convert, with the domain's message as the cause.
fn restore_error(error: DomainError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, Type::Null, Box::new(error))
}

/// Reads a run with its queue-local paths resolved under `runs_dir`.
pub(super) fn run_row(runs_dir: &Path) -> impl Fn(&Row<'_>) -> rusqlite::Result<TaskRun> + '_ {
    move |row| Ok(stored_run_row(row)?.relocated(runs_dir))
}

/// Reads a run as stored, paths included: what a command starts from
/// before the store saves its result.
pub(super) fn stored_run_row(row: &Row<'_>) -> rusqlite::Result<TaskRun> {
    TaskRun::restore(RunRecord {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        status: enum_col(row, "status")?,
        requested_provider: enum_col(row, "requested_provider")?,
        actual_provider: enum_col(row, "actual_provider")?,
        base_commit: row.get("base_commit")?,
        branch: row.get("branch")?,
        worktree_path: row.get("worktree_path")?,
        workspace_id: row.get("workspace_id")?,
        receipt_path: row.get("receipt_path")?,
        log_path: row.get("log_path")?,
        result_commit: row.get("result_commit")?,
        repo_path: row.get("repo_path")?,
        run_dir: row.get("run_dir")?,
        last_error: row.get("last_error")?,
        workspace_closed_at: row.get("workspace_closed_at")?,
        created_at: row.get("created_at")?,
    })
    .map_err(restore_error)
}

pub(super) fn event_row(row: &Row<'_>) -> rusqlite::Result<RunEvent> {
    Ok(RunEvent {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        goal_id: row.get("goal_id")?,
        run_id: row.get("run_id")?,
        kind: row.get("kind")?,
        payload: json_col(row, "payload")?,
        created_at: row.get("created_at")?,
    })
}
