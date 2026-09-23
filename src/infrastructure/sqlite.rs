use std::{
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior, params, params_from_iter,
    types::{Type, Value},
};
use serde::de::DeserializeOwned;
use serde_json::json;
use uuid::Uuid;

use crate::{
    application::{
        GraphInput, GraphTask, LatestRun, StatusFilter, TaskListItem, TaskPage, TaskQuery,
        TaskStore,
    },
    domain::{
        ClaimOutcome, DomainError, Goal, GoalDetail, GoalEdit, GoalStatus, GoalSummary, GoalTask,
        GoalVerdict, NewGoal, NewNote, NewTask, NotePage, NoteQuery, NoteTarget, OBSERVATION_KIND,
        Predecessor, RunEvent, Task, TaskAction, TaskDetail, TaskRun, TaskStatus, TaskStatusCounts,
        validate_base_commit,
    },
    infrastructure::location::runs_dir,
};

const APPLICATION_ID: i64 = 0x43545131;
const MIGRATIONS: &[&str] = &[
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
];
/// Ready tasks whose predecessors are completed, that own no unfinished run
/// and whose goal, if any, is not a draft (ADR-0024 decision 5).
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
    fn add(&mut self, task: NewTask) -> Result<Task> {
        task.validate()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(goal_id) = task.goal_id {
            ensure_goal_open(&tx, goal_id)?;
        }
        tx.execute(
            "INSERT INTO tasks(title, description, acceptance, verification_commands, goal_id, context, required_evidence)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![task.title, task.description, task.acceptance, serde_json::to_string(&task.verification_commands)?,
                task.goal_id, task.context, serde_json::to_string(&task.required_evidence())?],
        )?;
        let id = tx.last_insert_rowid();
        event(
            &tx,
            id,
            None,
            "task_created",
            json!({"goal_id": task.goal_id}),
        )?;
        for predecessor in task.dependencies {
            insert_dependency(&tx, id, predecessor)?;
        }
        let result = read_task(&tx, id)?;
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
            values.push(Value::from(goal_id));
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
            values.push(Value::from(before));
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
            tasks.pop().map(|task| task.id)
        } else {
            None
        };
        let mut dependencies = tx.prepare(
            "SELECT predecessor_id FROM task_dependencies WHERE task_id=?1 ORDER BY predecessor_id",
        )?;
        let mut latest_run = tx.prepare(
            "SELECT id, status FROM task_runs WHERE task_id=?1 ORDER BY rowid DESC LIMIT 1",
        )?;
        let tasks = tasks
            .into_iter()
            .map(|task| {
                let dependencies = dependencies
                    .query_map([task.id], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                let latest_run = latest_run
                    .query_row([task.id], |row| {
                        Ok(LatestRun {
                            id: row.get("id")?,
                            status: enum_col(row, "status")?,
                        })
                    })
                    .optional()?;
                Ok(TaskListItem::new(
                    task,
                    dependencies,
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

    fn show(&mut self, task_id: i64) -> Result<TaskDetail> {
        // One read snapshot keeps task status, run history and events consistent.
        let tx = self.conn.transaction()?;
        let task = read_task(&tx, task_id)?;
        let dependencies = tx.prepare(
            "SELECT predecessor_id FROM task_dependencies WHERE task_id=?1 ORDER BY predecessor_id"
        )?.query_map([task_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
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
            runs,
            events,
            processes,
        })
    }

    fn transition(&mut self, task_id: i64, action: TaskAction) -> Result<Task> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = read_task(&tx, task_id)?;
        let next = task
            .status
            .transition(action, has_unfinished_run(&tx, task_id)?)?;
        tx.execute("UPDATE tasks SET status=?1, updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?2",
            params![next.as_str(), task_id])?;
        event(
            &tx,
            task_id,
            None,
            "task_status_changed",
            json!({"from": task.status, "to": next}),
        )?;
        let result = read_task(&tx, task_id)?;
        tx.commit()?;
        Ok(result)
    }

    fn add_dependency(&mut self, task_id: i64, predecessor_id: i64) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_dependency(&tx, task_id, predecessor_id)?;
        tx.commit()?;
        Ok(())
    }

    fn remove_dependency(&mut self, task_id: i64, predecessor_id: i64) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            read_task(&tx, task_id)?.status.dependencies_editable(),
            "dependencies can only be changed for draft or ready tasks"
        );
        let changed = tx.execute(
            "DELETE FROM task_dependencies WHERE task_id=?1 AND predecessor_id=?2",
            params![task_id, predecessor_id],
        )?;
        ensure!(
            changed == 1,
            "dependency {task_id} -> {predecessor_id} does not exist"
        );
        touch(&tx, task_id)?;
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

    fn candidates(&self) -> Result<Vec<Task>> {
        Ok(self
            .conn
            .prepare(READY_QUERY)?
            .query_map([], task_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn graph_input(&self) -> Result<GraphInput> {
        let tx = self.conn.unchecked_transaction()?;
        let tasks: Vec<Task> = tx
            .prepare(
                "SELECT * FROM tasks WHERE status IN ('draft','ready','in_progress') ORDER BY id",
            )?
            .query_map([], task_row)?
            .collect::<rusqlite::Result<_>>()?;
        let mut dependencies = tx.prepare(
            "SELECT predecessor_id FROM task_dependencies WHERE task_id=?1 ORDER BY predecessor_id",
        )?;
        let tasks = tasks
            .into_iter()
            .map(|task| {
                let goal_status = task
                    .goal_id
                    .map(|goal_id| read_goal(&tx, goal_id).map(|goal| goal.status))
                    .transpose()?;
                Ok(GraphTask {
                    depends_on: dependencies
                        .query_map([task.id], |r| r.get(0))?
                        .collect::<rusqlite::Result<_>>()?,
                    goal_status,
                    id: task.id,
                    status: task.status,
                    title: task.title,
                    goal_id: task.goal_id,
                })
            })
            .collect::<Result<_>>()?;
        let candidates = tx
            .prepare(READY_QUERY)?
            .query_map([], |r| r.get("id"))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(GraphInput { tasks, candidates })
    }

    fn claim(&mut self, base_commit: &str) -> Result<ClaimOutcome> {
        validate_base_commit(base_commit)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome = claim_task(&tx, &self.runs_dir, base_commit, &[])?;
        tx.commit()?;
        Ok(outcome)
    }

    fn predecessors(&self, task_id: i64) -> Result<Vec<Predecessor>> {
        let tasks: Vec<Task> = self
            .conn
            .prepare(
                "SELECT p.* FROM task_dependencies d JOIN tasks p ON p.id = d.predecessor_id
                 WHERE d.task_id = ?1 ORDER BY p.id",
            )?
            .query_map([task_id], task_row)?
            .collect::<rusqlite::Result<_>>()?;
        tasks
            .into_iter()
            .map(|task| {
                // At most one run per task is integrated (`one_integrated_run_per_task`).
                let integrated_run = self
                    .conn
                    .query_row(
                        "SELECT * FROM task_runs WHERE task_id = ?1 AND status = 'integrated'",
                        [task.id],
                        run_row(&self.runs_dir),
                    )
                    .optional()?;
                Ok(Predecessor {
                    task,
                    integrated_run,
                })
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

    fn add_goal(&mut self, goal: NewGoal) -> Result<Goal> {
        goal.validate()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO goals(title, description, acceptance, constraints, doc, status)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                goal.title,
                goal.description,
                goal.acceptance,
                goal.constraints,
                goal.doc.filter(|d| !d.trim().is_empty()),
                if goal.draft {
                    GoalStatus::Draft
                } else {
                    GoalStatus::Open
                }
                .as_str()
            ],
        )?;
        let id = tx.last_insert_rowid();
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
                    id: goal.id,
                    status: goal.status,
                    closed: goal.is_closed(),
                    verdict: goal.verdict,
                    tasks: task_counts(&self.conn, goal.id)?,
                    title: goal.title,
                })
            })
            .collect()
    }

    fn show_goal(&mut self, goal_id: i64) -> Result<GoalDetail> {
        let tx = self.conn.transaction()?;
        let goal = read_goal(&tx, goal_id)?;
        let tasks = tx
            .prepare("SELECT id, title, status FROM tasks WHERE goal_id=?1 ORDER BY id")?
            .query_map([goal_id], |row| {
                Ok(GoalTask {
                    id: row.get("id")?,
                    title: row.get("title")?,
                    status: enum_col(row, "status")?,
                })
            })?
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
            events,
        })
    }

    fn edit_goal(&mut self, goal_id: i64, edit: GoalEdit) -> Result<Goal> {
        ensure!(!edit.is_empty(), "goal edit changes nothing");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old = read_goal(&tx, goal_id)?;
        let new = edit.apply(&old)?;
        tx.execute(
            "UPDATE goals SET title=?1, description=?2, acceptance=?3, constraints=?4, doc=?5,
             updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?6",
            params![
                new.title,
                new.description,
                new.acceptance,
                new.constraints,
                new.doc,
                goal_id
            ],
        )?;
        goal_event(
            &tx,
            goal_id,
            "goal_updated",
            json!({"old": old, "new": new}),
        )?;
        let result = read_goal(&tx, goal_id)?;
        tx.commit()?;
        Ok(result)
    }

    fn close_goal(&mut self, goal_id: i64, verdict: GoalVerdict) -> Result<Goal> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let goal = read_goal(&tx, goal_id)?;
        let counts = task_counts(&tx, goal_id)?;
        verdict.check_close(&goal, &counts)?;
        tx.execute(
            "UPDATE goals SET closed_at=strftime('%Y-%m-%dT%H:%M:%fZ','now'), verdict=?1,
             updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?2",
            params![verdict.as_str(), goal_id],
        )?;
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

    fn ready_goal(&mut self, goal_id: i64) -> Result<Goal> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        read_goal(&tx, goal_id)?.check_ready()?;
        tx.execute(
            "UPDATE goals SET status='open', updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id=?1",
            [goal_id],
        )?;
        goal_event(
            &tx,
            goal_id,
            "goal_status_changed",
            json!({"from": GoalStatus::Draft, "to": GoalStatus::Open}),
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
                let task_id: i64 = tx
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
            values.extend([Value::from(goal_id), Value::from(goal_id)]);
        }
        if let Some(task_id) = query.task_id {
            filters.push("task_id = ?".into());
            values.push(Value::from(task_id));
        }
        // Past a cursor the page runs forward from it; without one it is
        // the latest `limit` notes. Either way it is printed oldest first.
        let order = if let Some(since) = query.since {
            filters.push("id > ?".into());
            values.push(Value::from(since));
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
            .unwrap_or(0);
        Ok(NotePage { notes, cursor })
    }

    fn set_goal(&mut self, task_id: i64, goal_id: Option<i64>) -> Result<Task> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = read_task(&tx, task_id)?;
        ensure!(
            task.status.dependencies_editable(),
            "the goal can only be changed for draft or ready tasks"
        );
        if let Some(goal_id) = goal_id {
            ensure_goal_open(&tx, goal_id)?;
        }
        if task.goal_id != goal_id {
            tx.execute(
                "UPDATE tasks SET goal_id=?1, updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?2",
                params![goal_id, task_id],
            )?;
            event(
                &tx,
                task_id,
                None,
                "task_goal_changed",
                json!({"from": task.goal_id, "to": goal_id}),
            )?;
        }
        let result = read_task(&tx, task_id)?;
        tx.commit()?;
        Ok(result)
    }
}

fn read_goal(conn: &Connection, goal_id: i64) -> Result<Goal> {
    conn.query_row("SELECT * FROM goals WHERE id=?1", [goal_id], goal_row)
        .optional()?
        .with_context(|| format!("goal {goal_id} does not exist"))
}

/// Tasks join and move between open goals only; a closed goal is a record.
fn ensure_goal_open(conn: &Connection, goal_id: i64) -> Result<()> {
    let goal = read_goal(conn, goal_id)?;
    ensure!(
        !goal.is_closed(),
        "goal {goal_id} is closed as {}; create a new goal for further work",
        goal.verdict.map_or("?", GoalVerdict::as_str)
    );
    Ok(())
}

fn task_counts(conn: &Connection, goal_id: i64) -> Result<TaskStatusCounts> {
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

fn goal_event(
    conn: &Connection,
    goal_id: i64,
    kind: &str,
    payload: serde_json::Value,
) -> Result<()> {
    conn.execute(
        "INSERT INTO run_events(goal_id,kind,payload) VALUES (?1,?2,?3)",
        params![goal_id, kind, serde_json::to_string(&payload)?],
    )?;
    Ok(())
}

/// Reserve a dependency-ready task inside the caller's write transaction:
/// the first task of `order` that is still a candidate, or the lowest-ID
/// candidate when none of them is (an empty `order` means ID order). There
/// is no queue-wide execution slot; `one_unfinished_run_per_task` is the
/// only limit, so concurrent claims take different tasks.
pub(super) fn claim_task(
    tx: &Connection,
    runs_dir: &Path,
    base_commit: &str,
    order: &[i64],
) -> Result<ClaimOutcome> {
    let mut ready: Vec<Task> = tx
        .prepare(READY_QUERY)?
        .query_map([], task_row)?
        .collect::<rusqlite::Result<_>>()?;
    let preferred = order
        .iter()
        .find_map(|id| ready.iter().position(|task| task.id == *id))
        .unwrap_or(0);
    if ready.is_empty() {
        return Ok(ClaimOutcome::NoReadyTask);
    }
    let task = ready.swap_remove(preferred);
    let run_id = Uuid::new_v4().to_string();
    tx.execute("UPDATE tasks SET status='in_progress', updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1",
        [task.id])?;
    tx.execute(
        "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES (?1,?2,'claimed','claude','claude',?3)",
        params![run_id, task.id, base_commit.to_ascii_lowercase()],
    )?;
    event(
        tx,
        task.id,
        Some(&run_id),
        "run_claimed",
        json!({"from": "ready", "to": "in_progress", "provider": "claude"}),
    )?;
    let run = tx.query_row(
        "SELECT * FROM task_runs WHERE id=?1",
        [&run_id],
        run_row(runs_dir),
    )?;
    Ok(ClaimOutcome::Claimed { run: Box::new(run) })
}

/// Executing, awaiting or undergoing integration, or waiting for a session;
/// the same set as `one_unfinished_run_per_task`.
fn has_unfinished_run(conn: &Connection, task_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM task_runs WHERE task_id=?1
         AND status IN ('claimed','starting','running','validating','awaiting_integration',
                        'integrating','needs_session'))",
        [task_id],
        |r| r.get(0),
    )?)
}

pub(super) fn read_task(conn: &Connection, task_id: i64) -> Result<Task> {
    conn.query_row("SELECT * FROM tasks WHERE id=?1", [task_id], task_row)
        .optional()?
        .with_context(|| format!("task {task_id} does not exist"))
}

fn insert_dependency(conn: &Connection, task_id: i64, predecessor_id: i64) -> Result<()> {
    ensure!(task_id != predecessor_id, "a task cannot depend on itself");
    ensure!(
        read_task(conn, task_id)?.status.dependencies_editable(),
        "dependencies can only be changed for draft or ready tasks"
    );
    read_task(conn, predecessor_id)?;
    let cycle: bool = conn.query_row(
        "WITH RECURSIVE ancestors(id) AS (
            SELECT ?1 UNION
            SELECT d.predecessor_id FROM task_dependencies d JOIN ancestors a ON d.task_id=a.id
         ) SELECT EXISTS(SELECT 1 FROM ancestors WHERE id=?2)",
        params![predecessor_id, task_id],
        |r| r.get(0),
    )?;
    ensure!(
        !cycle,
        "dependency {task_id} -> {predecessor_id} would create a cycle"
    );
    let inserted = conn.execute(
        "INSERT INTO task_dependencies(task_id, predecessor_id) VALUES (?1,?2)
         ON CONFLICT(task_id, predecessor_id) DO NOTHING",
        params![task_id, predecessor_id],
    )?;
    if inserted != 0 {
        touch(conn, task_id)?;
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

fn touch(conn: &Connection, task_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1",
        [task_id],
    )?;
    Ok(())
}

pub(super) fn event(
    conn: &Connection,
    task_id: i64,
    run_id: Option<&str>,
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
    Ok(Task {
        id: row.get("id")?,
        title: row.get("title")?,
        description: row.get("description")?,
        acceptance: row.get("acceptance")?,
        verification_commands: json_col(row, "verification_commands")?,
        required_evidence: json_col(row, "required_evidence")?,
        status: enum_col(row, "status")?,
        goal_id: row.get("goal_id")?,
        context: row.get("context")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn goal_row(row: &Row<'_>) -> rusqlite::Result<Goal> {
    let verdict: Option<String> = row.get("verdict")?;
    Ok(Goal {
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
}

/// Reads a run with its queue-local paths resolved under `runs_dir`.
pub(super) fn run_row(runs_dir: &Path) -> impl Fn(&Row<'_>) -> rusqlite::Result<TaskRun> + '_ {
    move |row| Ok(stored_run_row(row)?.relocated(runs_dir))
}

fn stored_run_row(row: &Row<'_>) -> rusqlite::Result<TaskRun> {
    Ok(TaskRun {
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
