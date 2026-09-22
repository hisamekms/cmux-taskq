use std::{path::Path, str::FromStr, time::Duration};

use anyhow::{Context, Result, ensure};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior, params, types::Type,
};
use serde::de::DeserializeOwned;
use serde_json::json;
use uuid::Uuid;

use crate::{
    application::TaskQueue,
    domain::{
        ClaimOutcome, NewTask, RunEvent, Task, TaskAction, TaskDetail, TaskRun,
        validate_base_commit,
    },
};

const APPLICATION_ID: i64 = 0x43545131;
const MIGRATIONS: &[&str] = &[
    include_str!("../../migrations/0001_queue.sql"),
    include_str!("../../migrations/0002_supervisor.sql"),
    include_str!("../../migrations/0003_workspace_close.sql"),
];
const READY_QUERY: &str = "
    SELECT t.* FROM tasks t
    WHERE t.status = 'ready'
      AND NOT EXISTS (
        SELECT 1 FROM task_dependencies d JOIN tasks p ON p.id = d.predecessor_id
        WHERE d.task_id = t.id AND p.status <> 'completed'
      )
      AND NOT EXISTS (
        SELECT 1 FROM task_runs r WHERE r.task_id = t.id
          AND r.status IN ('claimed','starting','running','validating','awaiting_integration')
      )
    ORDER BY t.id";

pub struct SqliteQueue {
    pub(super) conn: Connection,
}

impl SqliteQueue {
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
        Ok(Self { conn })
    }

    fn migrate(&mut self, allow_initialize: bool) -> Result<()> {
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
            ensure!(objects == 0, "database is not an empty cmux-taskq queue");
        } else {
            ensure!(app == APPLICATION_ID, "database is not a cmux-taskq queue");
        }
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            tx.execute_batch(migration)
                .context("apply queue migration")?;
            tx.pragma_update(None, "user_version", (index + 1) as i64)?;
        }
        if app != APPLICATION_ID {
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        }
        tx.commit()?;
        Ok(())
    }
}

impl TaskQueue for SqliteQueue {
    fn add(&mut self, task: NewTask) -> Result<Task> {
        task.validate()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO tasks(title, description, acceptance, verification_commands) VALUES (?1,?2,?3,?4)",
            params![task.title, task.description, task.acceptance, serde_json::to_string(&task.verification_commands)?],
        )?;
        let id = tx.last_insert_rowid();
        event(&tx, id, None, "task_created", json!({}))?;
        for predecessor in task.dependencies {
            insert_dependency(&tx, id, predecessor)?;
        }
        let result = read_task(&tx, id)?;
        tx.commit()?;
        Ok(result)
    }

    fn list(&self) -> Result<Vec<Task>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM tasks ORDER BY id")?
            .query_map([], task_row)?
            .collect::<rusqlite::Result<_>>()?)
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
            .query_map([task_id], run_row)?
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
        let next = task.status.transition(action)?;
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

    fn claim(&mut self, base_commit: &str) -> Result<ClaimOutcome> {
        validate_base_commit(base_commit)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let busy: Option<String> = tx.query_row(
            "SELECT id FROM task_runs WHERE status IN ('claimed','starting','running','validating') LIMIT 1",
            [], |r| r.get(0),
        ).optional()?;
        if let Some(run_id) = busy {
            return Ok(ClaimOutcome::Busy { run_id });
        }
        let candidate = tx
            .query_row(&format!("{READY_QUERY} LIMIT 1"), [], task_row)
            .optional()?;
        let Some(task) = candidate else {
            return Ok(ClaimOutcome::NoReadyTask);
        };
        let run_id = Uuid::new_v4().to_string();
        tx.execute("UPDATE tasks SET status='in_progress', updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1",
            [task.id])?;
        tx.execute(
            "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
             VALUES (?1,?2,'claimed','claude','claude',?3)",
            params![run_id, task.id, base_commit.to_ascii_lowercase()],
        )?;
        event(
            &tx,
            task.id,
            Some(&run_id),
            "run_claimed",
            json!({"from": "ready", "to": "in_progress", "provider": "claude"}),
        )?;
        let run = tx.query_row("SELECT * FROM task_runs WHERE id=?1", [&run_id], run_row)?;
        tx.commit()?;
        Ok(ClaimOutcome::Claimed { run: Box::new(run) })
    }
}

fn read_task(conn: &Connection, task_id: i64) -> Result<Task> {
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

fn enum_col<T: FromStr<Err = anyhow::Error>>(row: &Row<'_>, name: &str) -> rusqlite::Result<T> {
    let value: String = row.get(name)?;
    value.parse().map_err(|error: anyhow::Error| {
        rusqlite::Error::FromSqlConversionFailure(
            row.as_ref().column_index(name).unwrap_or(0),
            Type::Text,
            std::io::Error::other(error.to_string()).into(),
        )
    })
}

fn json_col<T: DeserializeOwned>(row: &Row<'_>, name: &str) -> rusqlite::Result<T> {
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
        status: enum_col(row, "status")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

pub(super) fn run_row(row: &Row<'_>) -> rusqlite::Result<TaskRun> {
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

fn event_row(row: &Row<'_>) -> rusqlite::Result<RunEvent> {
    Ok(RunEvent {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        run_id: row.get("run_id")?,
        kind: row.get("kind")?,
        payload: json_col(row, "payload")?,
        created_at: row.get("created_at")?,
    })
}
