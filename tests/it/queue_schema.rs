//! Queue tests: initializing and opening a queue of another schema version,
//! the compatibility floor, read-only opens and when `migrate` may run.
use crate::common;

use dagq::{
    application::{TaskQuery, TaskStore, dependency_graph},
    domain::{ClaimOutcome, TaskAction, TaskId},
    infrastructure::{
        schema::{MIGRATIONS, floor_for},
        sqlite::{ReadOnlyQueue, SqliteQueue},
    },
};
use rusqlite::Connection;
use tempfile::TempDir;

use common::queue::*;

#[test]
fn initialization_is_repeatable_and_preserves_existing_tasks() {
    let (dir, mut queue) = fixture();
    queue.add(new_task("preserved")).unwrap();
    drop(queue);
    let queue = SqliteQueue::init(dir.path().join("queue.db")).unwrap();
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    assert_eq!(queue.list(&TaskQuery::default()).unwrap().total, 1);
    assert!(SqliteQueue::open(dir.path().join("typo.db")).is_err());
    assert!(!dir.path().join("typo.db").exists());
}

#[test]
fn foreign_and_future_databases_are_rejected_without_rewriting_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foreign.db");
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch("CREATE TABLE other_app(value); INSERT INTO other_app VALUES ('keep');")
        .unwrap();
    assert!(SqliteQueue::init(&path).is_err());
    assert_eq!(
        raw.query_row("SELECT value FROM other_app", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "keep"
    );
    assert_eq!(
        raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    // A newer queue whose floor is above this binary's schema: refused by
    // every entry point, and left as it was.
    let future = dir.path().join("future.db");
    drop(SqliteQueue::init(&future).unwrap());
    let raw = Connection::open(&future).unwrap();
    raw.pragma_update(None, "user_version", 99).unwrap();
    raw.execute("UPDATE schema_floor SET floor = 99", [])
        .unwrap();
    for error in [
        SqliteQueue::open(&future).err().unwrap(),
        SqliteQueue::init(&future).err().unwrap(),
        SqliteQueue::migrate(&future, None, 0).err().unwrap(),
    ] {
        let error = error.to_string();
        assert!(
            error.contains("unsupported queue schema version 99")
                && error.contains("older than schema 99")
                && error.contains("install a newer dagq"),
            "{error}"
        );
    }
    assert!(!SqliteQueue::schema(&future).unwrap().opens);
    assert_eq!(
        raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        99
    );
}

/// What a later binary's compatible migration does: a table and a nullable
/// column this binary does not know (ADR-0045 decision 6).
fn apply_future_compatible_migration(raw: &Connection) {
    raw.execute_batch(&format!(
        "ALTER TABLE tasks ADD COLUMN future_hint TEXT;
         ALTER TABLE task_runs ADD COLUMN future_weight INTEGER NOT NULL DEFAULT 0;
         CREATE TABLE future_things (id INTEGER PRIMARY KEY, note TEXT);
         PRAGMA user_version = {};",
        SqliteQueue::SCHEMA_VERSION + 1
    ))
    .unwrap();
}

#[test]
fn a_newer_queue_within_the_floor_is_used_as_it_is() {
    let (dir, mut queue) = fixture();
    let first = queue.add(new_task("before")).unwrap().id();
    drop(queue);
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    apply_future_compatible_migration(&raw);
    let newer = SqliteQueue::SCHEMA_VERSION + 1;
    let schema = SqliteQueue::schema(&path).unwrap();
    assert_eq!(
        (schema.schema_version, schema.floor, schema.opens),
        (newer, floor_for(SqliteQueue::SCHEMA_VERSION), true)
    );
    assert!(schema.pending.is_empty());
    // Reads, writes and a claim all work on the columns this binary knows.
    let mut queue = SqliteQueue::open(&path).unwrap();
    let second = queue.add(new_task("after")).unwrap().id();
    queue.transition(second, TaskAction::BypassReview).unwrap();
    assert!(matches!(
        queue.claim(&base()).unwrap(),
        ClaimOutcome::Claimed { .. }
    ));
    assert_eq!(queue.show(first).unwrap().task.title(), "before");
    assert_eq!(queue.list(&TaskQuery::default()).unwrap().total, 2);
    drop(queue);
    drop(SqliteQueue::init(&path).unwrap());
    // Nothing to apply, and a newer queue is never taken down to this binary.
    let report = SqliteQueue::migrate(&path, None, 0).unwrap();
    assert_eq!(
        (report.previous_version, report.schema_version),
        (newer, newer)
    );
    assert!(report.applied.is_empty() && report.backup.is_none());
    assert!(!dir.path().join("backups").exists());
    assert_eq!(
        raw.query_row("SELECT count(*) FROM future_things", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        newer
    );
}

/// A queue at schema 23, before the floor table: what the fixed binary
/// leaves until `dagq migrate` runs.
fn queue_before_the_floor(dir: &TempDir) -> std::path::PathBuf {
    let path = dir.path().join("queue.db");
    let raw = Connection::open(&path).unwrap();
    for migration in &MIGRATIONS[..23] {
        raw.execute_batch(migration).unwrap();
    }
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 23).unwrap();
    path
}

#[test]
fn opening_or_initializing_an_older_queue_never_migrates_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = queue_before_the_floor(&dir);
    for error in [
        SqliteQueue::open(&path).err().unwrap(),
        SqliteQueue::init(&path).err().unwrap(),
    ] {
        assert_eq!(
            error.to_string(),
            format!(
                "queue schema version 23 is older than this binary's schema {}; run `dagq \
                 migrate` to apply the {} pending migration(s)",
                SqliteQueue::SCHEMA_VERSION,
                SqliteQueue::SCHEMA_VERSION - 23
            )
        );
    }
    let schema = SqliteQueue::schema(&path).unwrap();
    assert_eq!((schema.schema_version, schema.floor), (23, 23));
    assert!(!schema.opens);
    assert_eq!(
        schema
            .pending
            .iter()
            .map(|m| (m.version, m.compatible))
            .collect::<Vec<_>>(),
        pending_after(23)
    );
    assert_eq!(
        schema.pending[..3]
            .iter()
            .map(|m| (m.version, m.compatible))
            .collect::<Vec<_>>(),
        [(24, false), (25, false), (26, true)]
    );
    let raw = Connection::open(&path).unwrap();
    assert_eq!(
        raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        23
    );
    // A copy from an earlier attempt in the same second is kept.
    std::fs::create_dir_all(dir.path().join("backups")).unwrap();
    std::fs::write(dir.path().join("backups/queue-23-5.sqlite3"), "earlier").unwrap();
    let report = SqliteQueue::migrate(&path, Some(&|_| false), 5).unwrap();
    assert_eq!(report.floor, floor_for(SqliteQueue::SCHEMA_VERSION));
    assert_eq!(
        report.applied.len(),
        usize::try_from(SqliteQueue::SCHEMA_VERSION - 23).unwrap()
    );
    let backup = report.backup.unwrap();
    assert!(
        backup.ends_with("backups/queue-23-5-1.sqlite3"),
        "{backup:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("backups/queue-23-5.sqlite3")).unwrap(),
        "earlier"
    );
    let schema = SqliteQueue::schema(&path).unwrap();
    assert!(schema.opens && schema.pending.is_empty());
    SqliteQueue::open(&path).unwrap();
}

#[test]
fn a_read_only_open_never_writes_and_reads_an_older_queue_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let path = queue_before_the_floor(&dir);
    let raw = Connection::open(&path).unwrap();
    // A live queue is in WAL mode, with rows not yet checkpointed.
    raw.pragma_update(None, "journal_mode", "WAL").unwrap();
    raw.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    raw.execute_batch(
        "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('old','','','[]','ready');",
    )
    .unwrap();
    let version = || -> i64 {
        raw.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    };
    let tables = || -> i64 {
        raw.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
            .unwrap()
    };
    let before = tables();
    // The older queue is read as migrated, from a copy: the file is left at
    // schema 23 with its tables as they were (ADR-0045 decision 18).
    let mut queue = SqliteQueue::open_read_only(&path).unwrap();
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    let listed = queue.list(&TaskQuery::default()).unwrap();
    assert_eq!(listed.total, 1);
    assert_eq!(queue.show(TaskId::new(1)).unwrap().task.title(), "old");
    dependency_graph(queue.graph_input().unwrap(), None);
    // A write reaches only the copy.
    queue.add(new_task("in memory")).unwrap();
    drop(queue);
    assert_eq!((version(), tables()), (23, before));
    assert_eq!(
        raw.query_row("SELECT count(*) FROM tasks", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(!dir.path().join("backups").exists());
    // `doctor`'s open reads the file's schema and the migrated copy on one
    // connection.
    let (schema, opened) = SqliteQueue::inspect_read_only(&path).unwrap();
    assert_eq!(schema.schema_version, 23);
    assert!(!schema.refuses_binary() && !schema.pending.is_empty());
    let ReadOnlyQueue::Readable(queue) = opened else {
        panic!("an older queue is readable")
    };
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    assert_eq!(queue.list(&TaskQuery::default()).unwrap().total, 1);
    drop(queue);
    assert_eq!((version(), tables()), (23, before));

    // At this binary's schema the file itself is opened, and read-only.
    SqliteQueue::migrate(&path, None, 0).unwrap();
    let mut queue = SqliteQueue::open_read_only(&path).unwrap();
    assert_eq!(queue.list(&TaskQuery::default()).unwrap().total, 1);
    let error = queue.add(new_task("refused")).err().unwrap();
    assert!(format!("{error:#}").contains("readonly"), "{error:#}");
    drop(queue);

    // A newer queue within the floor is read as it is.
    apply_future_compatible_migration(&raw);
    let mut queue = SqliteQueue::open_read_only(&path).unwrap();
    assert_eq!(
        queue.schema_version().unwrap(),
        SqliteQueue::SCHEMA_VERSION + 1
    );
    assert_eq!(queue.show(TaskId::new(1)).unwrap().task.title(), "old");
    drop(queue);

    // Above the floor, or not a queue at all, it is refused like `open`.
    raw.execute("UPDATE schema_floor SET floor = 99", [])
        .unwrap();
    let error = SqliteQueue::open_read_only(&path)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("install a newer dagq"), "{error}");
    let (schema, opened) = SqliteQueue::inspect_read_only(&path).unwrap();
    assert!(schema.refuses_binary());
    let ReadOnlyQueue::Refused {
        binding,
        error: refused,
    } = opened
    else {
        panic!("a queue above the floor is refused")
    };
    assert_eq!(refused.to_string(), error);
    assert_eq!(binding.repository_binding().unwrap(), None);
    binding.assert_repository("/elsewhere/.git").unwrap();
    let empty = dir.path().join("empty.db");
    drop(Connection::open(&empty).unwrap());
    let error = SqliteQueue::open_read_only(&empty)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("use init first"), "{error}");
    assert!(SqliteQueue::open_read_only(dir.path().join("missing.db")).is_err());
    assert!(!dir.path().join("missing.db").exists());
}

#[test]
fn a_breaking_migration_waits_for_an_idle_queue() {
    let dir = tempfile::tempdir().unwrap();
    let path = queue_before_the_floor(&dir);
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch(&format!(
        "INSERT INTO supervisors(token, pid, parallel) VALUES ('live', 101, 1), ('dead', 102, 1);
         INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('t','','','[]','in_progress');
         INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
         VALUES ('run-live',1,'running','claude','claude','{BASE}');
         INSERT INTO run_processes(run_id,role,pid) VALUES ('run-live','wrapper',103);"
    ))
    .unwrap();
    let alive = |pid: u32| pid != 102;
    let error = SqliteQueue::migrate(&path, Some(&alive), 0)
        .err()
        .unwrap()
        .to_string();
    assert!(
        error.contains("breaking migration(s) 24, 25, 27, 28")
            && error.contains("supervisor live (pid 101)")
            && !error.contains("dead")
            && error.contains("run run-live (running)")
            && error.contains("wrapper of run run-live (pid 103)")
            && error.contains("down --wait"),
        "{error}"
    );
    // Refused before anything was copied or applied.
    assert!(!dir.path().join("backups").exists());
    assert_eq!(
        raw.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        23
    );
    // Once the supervisor and the run are gone, it goes through.
    raw.execute_batch(
        "DELETE FROM supervisors WHERE token='live';
         UPDATE run_processes SET exited_at=1, exit_code=0;
         UPDATE task_runs SET status='failed';",
    )
    .unwrap();
    let report = SqliteQueue::migrate(&path, Some(&alive), 0).unwrap();
    assert_eq!(report.schema_version, SqliteQueue::SCHEMA_VERSION);
    // A second migrate has nothing left to do.
    let again = SqliteQueue::migrate(&path, Some(&alive), 0).unwrap();
    assert!(again.applied.is_empty() && again.backup.is_none());
}
