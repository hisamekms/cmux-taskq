mod common;

use common::cli::*;

use std::path::Path;

use dagq::infrastructure::sqlite::SqliteQueue;
use serde_json::Value;

/// A claimed run of task 1 (goal 1) with the given events, their
/// `created_at` set to 2026-09-24 at the given `HH:MM:SS`; the run's ID.
fn run_with_events(db: &Path, events: &[(&str, serde_json::Value, &str)]) -> String {
    use dagq::domain::{ClaimOutcome, CommitSha};
    ok(db, &["init"]);
    ok(db, &["goal", "add", "measured"]);
    ok(db, &["add", "first", "--goal", "1"]);
    ok(db, &["ready", "1", "--bypass-review"]);
    let mut queue = SqliteQueue::open(db).unwrap();
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&base, "t").unwrap() else {
        panic!("nothing to claim");
    };
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "UPDATE run_events SET created_at='2026-09-24T00:00:00.000Z' WHERE run_id=?1",
        [run.id().as_str()],
    )
    .unwrap();
    for (kind, payload, at) in events {
        let before: i64 = conn
            .query_row("SELECT MAX(id) FROM run_events", [], |r| r.get(0))
            .unwrap();
        queue
            .record_runtime_event(run.id(), kind, payload.clone())
            .unwrap();
        // The event and the session spans written with it (ADR-0048).
        conn.execute(
            "UPDATE run_events SET created_at=?1 WHERE id>?2",
            rusqlite::params![format!("2026-09-24T{at}.000Z"), before],
        )
        .unwrap();
    }
    run.id().as_str().to_owned()
}

#[test]
fn events_and_watch_read_past_a_cursor() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    ok(&db, &["add", "second"]);
    let cursor = ok(&db, &["status"])["cursor"].as_i64().unwrap();
    assert!(cursor >= 2);
    // Registering tasks is no attention; --all shows every kind, oldest first.
    assert_eq!(
        ok(&db, &["events", "--after", "0"]),
        serde_json::json!({"events": [], "cursor": cursor})
    );
    let all = ok(&db, &["events", "--after", "0", "--all", "--limit", "1"]);
    assert_eq!(all["events"].as_array().unwrap().len(), 1);
    assert_eq!(all["events"][0]["kind"], "task_created");
    assert_eq!(all["events"][0]["task_id"], 1);
    let first = all["cursor"].as_i64().unwrap();
    assert_eq!(first, all["events"][0]["id"].as_i64().unwrap());
    let rest = ok(&db, &["events", "--after", &first.to_string(), "--all"]);
    assert_eq!(rest["events"][0]["task_id"], 2);
    assert_eq!(rest["cursor"], cursor);
    assert!(!invoke(&db, &["events", "--limit", "0"]).status.success());

    // With nothing to report, watch times out empty with the cursor unchanged.
    let started = std::time::Instant::now();
    let quiet = ok(
        &db,
        &["watch", "--after", "0", "--timeout", "1", "--interval", "1"],
    );
    assert!(started.elapsed() >= std::time::Duration::from_secs(1));
    assert_eq!(
        quiet,
        serde_json::json!({"events": [], "supervisors_changed": false, "supervisors": [], "cursor": 0})
    );
    let quiet = ok(&db, &["watch", "--timeout", "0"]);
    assert_eq!(quiet["cursor"], cursor);
    assert!(!invoke(&db, &["watch", "--interval", "0"]).status.success());
}

#[test]
fn events_full_and_filters_narrow_what_they_read() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let run = run_with_events(
        &db,
        &[
            ("agent_started", json!({"pid": 1}), "00:00:05"),
            (
                "receipt_observed",
                json!({"path": "/r/receipt.json"}),
                "10:00:00",
            ),
            (
                "validation_finished",
                json!({"accepted": true, "receipt": {"summary": "s"}}),
                "10:00:01",
            ),
        ],
    );
    ok(&db, &["add", "other"]);
    let kinds = |value: &Value| {
        value["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };

    // --full keeps every field and the whole payload; the compact form drops the payload.
    let full = ok(&db, &["events", "--all", "--full", "--run", &run]);
    let events = full["events"].as_array().unwrap();
    assert!(events.len() >= 4);
    assert!(
        events
            .iter()
            .all(|e| e["run_id"] == run.as_str() && e["payload"].is_object())
    );
    let validated = events.last().unwrap();
    assert_eq!(validated["payload"]["receipt"]["summary"], "s");
    assert_eq!(validated["task_id"], 1);
    let compact = ok(&db, &["events", "--all", "--run", &run]);
    assert!(compact["events"][0].get("payload").is_none());

    // --kind reads that kind, attention or not; repeated, several.
    assert_eq!(
        kinds(&ok(
            &db,
            &[
                "events",
                "--kind",
                "receipt_observed",
                "--kind",
                "agent_started"
            ]
        )),
        ["agent_started", "receipt_observed"]
    );
    // Without --all or --kind, the filters still keep attention only.
    assert_eq!(
        kinds(&ok(&db, &["events", "--run", &run])),
        Vec::<String>::new()
    );
    // --task and --goal.
    let other = kinds(&ok(&db, &["events", "--all", "--task", "2"]));
    assert_eq!(other, ["task_created"]);
    // --goal reads the goal's own events and those of its tasks and runs.
    let goal = ok(&db, &["events", "--all", "--goal", "1"]);
    let goal_kinds = kinds(&goal);
    assert!(goal_kinds.contains(&"goal_created".to_owned()));
    assert!(goal_kinds.contains(&"receipt_observed".to_owned()));
    assert!(
        goal["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["task_id"] != 2)
    );
    // --since is inclusive, --until exclusive; a date is its midnight.
    assert_eq!(
        kinds(&ok(
            &db,
            &[
                "events",
                "--all",
                "--run",
                &run,
                "--since",
                "2026-09-24T00:00:05Z",
                "--until",
                "2026-09-24T10:00:01",
            ]
        )),
        // The worker's span opens with its session (ADR-0048).
        ["agent_started", "session_opened", "receipt_observed"]
    );
    assert_eq!(
        kinds(&ok(
            &db,
            &["events", "--all", "--run", &run, "--until", "2026-09-24"]
        )),
        Vec::<String>::new()
    );
    for bad in [
        "yesterday",
        "2026-09-26T1:00:00Z",
        "2026-09-26T24:00:00",
        "2026-13-01",
        "2026-09-26T10:00:00.Z",
    ] {
        let output = invoke(&db, &["events", "--since", bad]);
        assert!(!output.status.success(), "{bad}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("not a UTC time"));
    }
}

#[test]
fn timeline_names_the_long_gap_before_the_receipt() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    // Task 182's shape: the session starts, then nothing until the receipt
    // ten hours later; the run waits on a question for an hour after it.
    let run = run_with_events(
        &db,
        &[
            ("agent_started", json!({}), "00:00:05"),
            ("receipt_observed", json!({}), "10:00:00"),
            (
                "ask_opened",
                json!({"ask_id": 9, "kind": "worker_question"}),
                "10:00:10",
            ),
            (
                "ask_answered",
                json!({"ask_id": 9, "kind": "worker_question"}),
                "11:00:10",
            ),
        ],
    );
    let timeline = ok(&db, &["timeline", &run]);
    assert_eq!(timeline["run_id"], run.as_str());
    assert_eq!(timeline["task_id"], 1);
    assert_eq!(timeline["status"], "claimed");
    assert_eq!(timeline["gap_secs"], 300);
    let gaps = timeline["gaps"].as_array().unwrap();
    assert_eq!(gaps[0]["reason"], "idle");
    assert_eq!(gaps[0]["phase"], "session");
    assert_eq!(gaps[0]["confirmed"], false);
    assert_eq!(gaps[0]["secs"], 10 * 3600 - 5);
    assert_eq!(gaps[0]["from"], "2026-09-24T00:00:05.000Z");
    assert_eq!(gaps[0]["until"], "2026-09-24T10:00:00.000Z");
    assert_eq!(gaps[1]["reason"], "waiting_ask");
    assert_eq!(gaps[1]["ask_ids"], json!([9]));
    // The run is not finished: the time since its last event is a gap too.
    let last = gaps.last().unwrap();
    assert!(last["before_event"].is_null() && last["until"].is_null());
    assert_eq!(last["reason"], "after_receipt");
    assert!(timeline["events"][0].get("payload").is_none());
    let full = ok(&db, &["timeline", &run, "--full", "--gap", "7200"]);
    assert!(full["events"][0]["payload"].is_object());
    assert_eq!(full["gaps"][0]["secs"], 10 * 3600 - 5);
    assert!(!invoke(&db, &["timeline", "no-such-run"]).status.success());
    assert!(
        !invoke(&db, &["timeline", &run, "--gap", "0"])
            .status
            .success()
    );
}

#[test]
fn show_goal_show_and_doctor_are_compact_unless_full() {
    let (_dir, db) = queue();
    let long = "長".repeat(400);
    ok(
        &db,
        &[
            "goal",
            "add",
            "compact",
            "--description",
            &long,
            "--acceptance",
            "short",
        ],
    );
    ok(
        &db,
        &[
            "add",
            "task",
            "--goal",
            "1",
            "--context",
            &long,
            "--verify",
            "true",
        ],
    );
    // Draft and ready back and forth: one event each, twelve in all with `task_created`.
    for _ in 0..6 {
        ok(&db, &["ready", "1", "--bypass-review"]);
        ok(&db, &["draft", "1"]);
    }
    ok(&db, &["goal", "edit", "1", "--constraints", &long]);

    let full = ok(&db, &["show", "1", "--full"]);
    assert_eq!(full["task"]["context"], long.as_str());
    assert!(full["task"].get("truncated").is_none());
    let all_events = full["events"].as_array().unwrap().len();
    assert!(all_events > 10);
    assert!(full["events"][0]["payload"].is_object());
    assert!(full.get("events_total").is_none());

    let shown = ok(&db, &["show", "1"]);
    let context = shown["task"]["context"].as_str().unwrap();
    assert!(context.ends_with('…'), "{context}");
    assert_eq!(context.chars().count(), 301);
    assert_eq!(shown["task"]["truncated"], true);
    assert_eq!(
        shown["task"]["verification_commands"],
        serde_json::json!(["true"])
    );
    assert_eq!(shown["runs"], serde_json::json!([]));
    assert_eq!(shown["events_total"], all_events);
    let events = shown["events"].as_array().unwrap();
    assert_eq!(events.len(), 10);
    assert_eq!(events[9]["id"], full["events"][all_events - 1]["id"]);
    assert_eq!(
        events[9]["payload"],
        serde_json::json!({"from": "ready", "to": "draft"})
    );
    let three = ok(&db, &["show", "1", "--events", "3"]);
    assert_eq!(three["events"].as_array().unwrap().len(), 3);
    assert!(
        !invoke(&db, &["show", "1", "--full", "--events", "3"])
            .status
            .success()
    );

    let full = ok(&db, &["goal", "show", "1", "--full"]);
    assert_eq!(full["goal"]["description"], long.as_str());
    assert!(full["events"][0]["payload"].is_object());
    let goal = ok(&db, &["goal", "show", "1"]);
    assert_eq!(goal["goal"]["truncated"], true);
    assert_eq!(goal["goal"]["acceptance"], "short");
    assert!(goal["goal"]["description"].as_str().unwrap().ends_with('…'));
    assert!(goal["goal"]["constraints"].as_str().unwrap().ends_with('…'));
    assert_eq!(
        goal["tasks"],
        serde_json::json!([{"id": 1, "title": "task", "status": "draft"}])
    );
    assert_eq!(
        goal["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["goal_created", "goal_updated"]
    );
    assert!(goal["events"][0].get("payload").is_none());

    for args in [&["doctor"][..], &["doctor", "--full"]] {
        let report = ok(&db, args);
        assert_eq!(report["runs"], serde_json::json!([]), "{args:?}");
        assert_eq!(report["supervisors"], serde_json::json!([]), "{args:?}");
    }
}

#[test]
fn search_prints_hits_with_excerpts_and_checks_its_filters() {
    let (_dir, db) = queue();
    ok(
        &db,
        &[
            "goal",
            "add",
            "重複を見つける",
            "--acceptance",
            "上位 5 件に出る",
        ],
    );
    ok(
        &db,
        &[
            "add",
            "runtime: 全文検索",
            "--goal",
            "1",
            "--description",
            "FTS5 の trigram",
        ],
    );
    ok(&db, &["add", "unrelated"]);
    ok(&db, &["cancel", "2"]);
    ok(&db, &["note", "--task", "1", "--text", "全文検索のメモ"]);
    let found = ok(&db, &["search", "全文検索"]);
    assert_eq!(found["total"], 2, "{found}");
    let kinds: Vec<&str> = found["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds.len(), 2);
    assert!(kinds.contains(&"task") && kinds.contains(&"note"));
    let goal = ok(
        &db,
        &["search", "上位", "--kind", "goal", "--status", "open"],
    );
    assert_eq!(
        goal["hits"][0],
        serde_json::json!({
            "kind": "goal", "id": 1, "status": "open", "title": "重複を見つける",
            "field": "acceptance", "excerpt": "«上位» 5 件に出る",
        })
    );
    let full = ok(
        &db,
        &[
            "search", "trigram", "--goal", "1", "--kind", "task", "--full", "--limit", "1",
        ],
    );
    assert_eq!(full["hits"][0]["fields"]["description"], "FTS5 の trigram");
    assert!(full["hits"][0]["score"].is_number(), "{full}");
    let canceled = ok(
        &db,
        &["search", "unrelated", "--status", "canceled,completed"],
    );
    assert_eq!(canceled["hits"][0]["id"], 2);
    assert_eq!(
        refused(&db, &["search", "x", "--status", "closed"]),
        "unknown status: closed"
    );
    assert!(refused(&db, &["search", "AND"]).contains("the query has no term"));
    assert!(refused(&db, &["search", "trigram OR"]).starts_with("invalid search query: fts5"));
    // Reading, so the observer and the headless reviewer may search.
    ok_as("observer", &db, &["search", "全文検索"]);
    ok_as("reviewer", &db, &["search", "全文検索"]);
}

#[test]
fn related_prints_candidates_with_their_clues() {
    let (_dir, db) = queue();
    ok(
        &db,
        &[
            "add",
            "test: a_run_parked_again_after_a_skip が負荷下で落ちる",
            "--description",
            "tests/runtime.rs の assert が落ちた",
        ],
    );
    ok(
        &db,
        &[
            "add",
            "test: a_run_parked_again_after_a_skip fails under load",
            "--description",
            "tests/runtime.rs again",
        ],
    );
    ok(&db, &["add", "unrelated"]);
    // A title with FTS5's syntax in it still makes a valid query.
    ok(&db, &["add", "fix: (a*b) ^c \"d\" NOT x OR y:z"]);
    assert_eq!(ok(&db, &["related", "4"])["total"], 0);
    let related = ok(&db, &["related", "1"]);
    assert_eq!(related["task_id"], 1);
    assert_eq!(related["total"], 1, "{related}");
    let candidate = &related["related"][0];
    assert_eq!(candidate["id"], 2);
    assert_eq!(candidate["status"], "draft");
    assert!(candidate["score"].as_f64().unwrap() > 0.0);
    let clues: Vec<(&str, &str)> = candidate["clues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["clue"].as_str().unwrap(), c["value"].as_str().unwrap()))
        .collect();
    assert!(
        clues.contains(&("test", "a_run_parked_again_after_a_skip")),
        "{clues:?}"
    );
    assert!(clues.contains(&("file", "tests/runtime.rs")), "{clues:?}");
    let none = ok(
        &db,
        &[
            "related",
            "1",
            "--status",
            "ready,completed",
            "--limit",
            "1",
        ],
    );
    assert_eq!(none["related"], serde_json::json!([]));
    assert!(
        !invoke(&db, &["related", "1", "--status", "closed"])
            .status
            .success()
    );
    assert_eq!(refused(&db, &["related", "9"]), "task 9 does not exist");
    ok_as("observer", &db, &["related", "1"]);
    ok_as("reviewer", &db, &["related", "1"]);
}
