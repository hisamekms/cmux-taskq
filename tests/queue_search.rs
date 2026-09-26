//! Queue tests: full-text search over every kind of record.
mod common;

use dagq::{
    application::TaskStore,
    domain::search::{SearchKind, SearchQuery},
    domain::{GoalEdit, GoalVerdict, NewGoal, NewNote, NewTask, NoteTarget, TaskAction, TaskEdit},
    infrastructure::sqlite::SqliteQueue,
};
use rusqlite::Connection;

use common::queue::*;

#[test]
fn search_finds_every_kind_in_every_status_and_follows_edits() {
    let (dir, mut queue) = fixture();
    let goal = queue
        .add_goal(NewGoal {
            constraints: "埋め込みは入れない".into(),
            ..new_goal("重複を見つける")
        })
        .unwrap()
        .id();
    let task = queue
        .add(NewTask {
            description: "FTS5 の trigram で src/infrastructure/search.rs を足す".into(),
            goal_id: Some(goal),
            ..new_task("runtime: 全文検索を入れる")
        })
        .unwrap()
        .id();
    let other = queue
        .add(new_task("Find duplicate tasks by their test names"))
        .unwrap()
        .id();
    queue.transition(other, TaskAction::Cancel).unwrap();
    for (target, text) in [
        (NoteTarget::Task(task), "ID の衝突に注意する"),
        (NoteTarget::Goal(goal), "goal の観察メモ"),
    ] {
        queue
            .add_note(NewNote {
                target,
                text: text.into(),
                kind: None,
                by: "human".into(),
            })
            .unwrap();
    }
    assert_eq!(
        search(&queue, "全文検索", |_| {}),
        ["task 1 draft title: runtime: «全文検索»を入れる"]
    );
    assert_eq!(
        search(&queue, "埋め込み", |_| {}),
        ["goal 1 open constraints: «埋め込み»は入れない"]
    );
    assert_eq!(
        search(&queue, "search.rs", |_| {}),
        ["task 1 draft description: …am で src/infrastructure/«search.rs» を足す"]
    );
    // A canceled task is found, and --status keeps to the statuses given.
    assert_eq!(
        search(&queue, "DUPLICATE", |_| {}),
        ["task 2 canceled title: Find «duplicate» tasks by their test nam…"]
    );
    assert!(search(&queue, "duplicate", |q| q.statuses = vec!["draft".into()]).is_empty());
    // Terms under three characters are matched too, all of them.
    assert_eq!(
        search(&queue, "ID", |_| {}),
        ["note 5 draft text: «ID» の衝突に注意する"]
    );
    assert_eq!(
        search(&queue, "観察 goal", |_| {}),
        ["note 6 open text: «goal» の観察メモ"]
    );
    assert!(search(&queue, "ID 観察", |_| {}).is_empty());
    assert_eq!(
        search(&queue, "trigram OR 観察メモ", |q| q.kinds =
            vec![SearchKind::Note])
        .len(),
        1
    );
    assert_eq!(
        search(&queue, "を入れる OR 見つける OR 観察メモ", |_| {}).len(),
        3
    );
    assert_eq!(
        search(
            &queue,
            "を入れる OR 見つける OR 観察メモ OR 注意する OR test",
            |q| { q.goal_id = Some(goal) }
        )
        .len(),
        4
    );
    assert!(
        queue
            .search(&SearchQuery {
                terms: "ID OR x".into(),
                limit: 1,
                ..SearchQuery::default()
            })
            .unwrap_err()
            .to_string()
            .contains("cannot be combined")
    );

    // Edits replace what is indexed; the new status reaches the notes.
    queue
        .edit_task(
            task,
            TaskEdit {
                description: Some("SQLite の索引を使う".into()),
                ..TaskEdit::default()
            },
        )
        .unwrap();
    assert!(search(&queue, "trigram", |_| {}).is_empty());
    assert_eq!(search(&queue, "sqlite", |_| {}).len(), 1);
    queue
        .edit_goal(
            goal,
            GoalEdit {
                constraints: Some("意味の検索は後回し".into()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
    assert!(search(&queue, "埋め込み", |_| {}).is_empty());
    assert_eq!(search(&queue, "後回し", |_| {}).len(), 1);
    queue.transition(task, TaskAction::Cancel).unwrap();
    queue.close_goal(goal, GoalVerdict::Abandoned).unwrap();
    let mut closed = search(&queue, "注意する OR 観察メモ OR 後回し", |_| {});
    closed.sort();
    assert_eq!(
        closed,
        [
            "goal 1 abandoned constraints: 意味の検索は«後回し»",
            "note 5 canceled text: ID の衝突に«注意する»",
            "note 6 abandoned text: goal の«観察メモ»",
        ]
    );

    // --full adds every field and the score.
    let full = queue
        .search(&SearchQuery {
            terms: "後回し".into(),
            limit: 5,
            full: true,
            ..SearchQuery::default()
        })
        .unwrap();
    let hit = &full.hits[0];
    assert!(hit.score.is_some());
    assert_eq!(
        hit.fields.as_ref().unwrap()["constraints"],
        "意味の検索は後回し"
    );

    // What an older binary writes is indexed by the triggers alike.
    drop(queue);
    let raw = Connection::open(dir.path().join("queue.db")).unwrap();
    raw.execute(
        "INSERT INTO tasks(title,description,acceptance,verification_commands)
         VALUES ('older binary の登録','','','[]')",
        [],
    )
    .unwrap();
    let queue = SqliteQueue::open(dir.path().join("queue.db")).unwrap();
    assert_eq!(
        search(&queue, "older", |_| {}),
        ["task 3 draft title: «older» binary の登録"]
    );
}
