//! The queue and its records for the queue tests (`tests/queue_*.rs`).

use std::path::Path;

use dagq::{
    domain::search::{SearchQuery, SearchRef},
    domain::{CommitSha, NewGoal, NewTask},
    infrastructure::{schema::MIGRATIONS, sqlite::SqliteQueue},
};
use rusqlite::Connection;
use tempfile::TempDir;

pub const BASE: &str = "0123456789abcdef0123456789abcdef01234567";

pub fn base() -> CommitSha {
    CommitSha::try_from(BASE).unwrap()
}

pub fn new_task(title: &str) -> NewTask {
    NewTask {
        title: title.into(),
        description: "A small development task".into(),
        acceptance: "The regression test passes".into(),
        verification_commands: vec!["cargo test".into()],
        required_evidence: Vec::new(),
        paths: Vec::new(),
        priority: Default::default(),
        kind: None,
        dependencies: vec![],
        goal_dependencies: Vec::new(),
        goal_id: None,
        context: String::new(),
    }
}

pub fn new_goal(title: &str) -> NewGoal {
    NewGoal {
        title: title.into(),
        description: "One problem several tasks solve".into(),
        acceptance: "Every task landed and the feature works end to end".into(),
        constraints: "Keep the module boundary".into(),
        doc: Some("docs/adr/0009-goal-groups-tasks.md".into()),
        draft: false,
    }
}

/// An old queue after `dagq migrate`: opening it alone is refused and
/// leaves its `user_version` as it was (ADR-0045 decision 5).
pub fn migrated(path: &Path) -> SqliteQueue {
    let version = |path: &Path| -> i64 {
        Connection::open(path)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    };
    let before = version(path);
    let error = SqliteQueue::open(path).err().unwrap().to_string();
    assert!(error.contains("run `dagq migrate`"), "{error}");
    assert_eq!(version(path), before);
    let report = SqliteQueue::migrate(path, None, 1_700_000_000).unwrap();
    assert_eq!(report.previous_version, before);
    assert_eq!(report.schema_version, SqliteQueue::SCHEMA_VERSION);
    // Every migration so far is breaking, so the old queue was copied first.
    let backup = report.backup.unwrap();
    assert!(backup.ends_with(format!("backups/queue-{before}-1700000000.sqlite3")));
    assert_eq!(version(&backup), before);
    SqliteQueue::open(path).unwrap()
}

pub fn fixture() -> (TempDir, SqliteQueue) {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("queue.db")).unwrap();
    (dir, queue)
}

/// The migrations after schema `version` as `migrate` reports them, each
/// with whether it declares itself compatible: what a test expects without
/// naming the latest schema version (ADR-0067 decision 4).
pub fn pending_after(version: i64) -> Vec<(i64, bool)> {
    MIGRATIONS
        .iter()
        .enumerate()
        .skip(usize::try_from(version).unwrap())
        .map(|(index, migration)| {
            (
                index as i64 + 1,
                dagq::infrastructure::schema::is_compatible(migration),
            )
        })
        .collect()
}

pub fn search(
    queue: &SqliteQueue,
    terms: &str,
    adjust: impl FnOnce(&mut SearchQuery),
) -> Vec<String> {
    let mut query = SearchQuery {
        terms: terms.into(),
        limit: 20,
        ..SearchQuery::default()
    };
    adjust(&mut query);
    let page = queue.search(&query).unwrap();
    assert_eq!(page.total, page.hits.len());
    page.hits
        .iter()
        .map(|hit| {
            let id = match &hit.id {
                SearchRef::Id(id) => id.to_string(),
                SearchRef::Commit(sha) => sha.clone(),
            };
            format!(
                "{} {id} {} {}: {}",
                hit.kind.as_str(),
                hit.status.as_deref().unwrap_or("-"),
                hit.field,
                hit.excerpt
            )
        })
        .collect()
}
