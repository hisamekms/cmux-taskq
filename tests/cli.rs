use std::{
    path::Path,
    process::{Command, Output},
};

use dagq::infrastructure::sqlite::SqliteQueue;
use serde_json::Value;

fn invoke(db: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dagq"))
        .arg("--db")
        .arg(db)
        .args(args)
        .output()
        .unwrap()
}

fn ok(db: &Path, args: &[&str]) -> Value {
    let output = invoke(db, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn cli_persists_across_processes_and_reports_dependency_errors_as_json() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue with spaces.db");
    assert_eq!(
        ok(&db, &["init"])["schema_version"],
        SqliteQueue::SCHEMA_VERSION
    );
    let first = ok(
        &db,
        &[
            "add",
            "先行 'task'",
            "--description",
            "do something",
            "--acceptance",
            "works",
            "--verify",
            "cargo test",
        ],
    );
    assert_eq!(first["status"], "draft");
    let a = first["id"].to_string();
    let second = ok(&db, &["add", "second", "--depends-on", &a]);
    let b = second["id"].to_string();
    ok(&db, &["ready", &b]);
    assert_eq!(ok(&db, &["candidates"]), serde_json::json!([]));
    ok(&db, &["ready", &a]);
    assert_eq!(ok(&db, &["candidates"])[0]["id"], first["id"]);
    let output = invoke(&db, &["dependency", "add", &a, &b]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(error["error"].as_str().unwrap().contains("cycle"));
    let detail = ok(&db, &["show", &a]);
    assert_eq!(detail["task"]["title"], first["title"]);
    assert_eq!(detail["dependencies"], serde_json::json!([]));
    assert_eq!(ok(&db, &["list"])["tasks"].as_array().unwrap().len(), 2);
    ok(&db, &["cancel", &a]);
    assert_eq!(ok(&db, &["candidates"]), serde_json::json!([]));
    ok(&db, &["dependency", "remove", &b, &a]);
    assert_eq!(ok(&db, &["candidates"])[0]["id"], second["id"]);
}

#[test]
fn reads_do_not_create_a_queue_and_unknown_tasks_fail() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("missing.db");
    assert!(!invoke(&db, &["list"]).status.success());
    assert!(!db.exists());
    ok(&db, &["init"]);
    // `doctor` and `status` stamp `checked_at` with the current unix second,
    // so the timestamp is checked for its type and dropped before comparing.
    for command in ["doctor", "status"] {
        let mut report = ok(&db, &[command]);
        let checked_at = report.as_object_mut().unwrap().remove("checked_at");
        assert!(
            checked_at.is_some_and(|value| value.is_u64()),
            "{command} reports checked_at as unix seconds"
        );
        let mut expected = serde_json::json!({"supervisors": [], "runs": []});
        if command == "status" {
            // Nothing supervises a fresh queue; no event exists yet.
            expected["attention"] = serde_json::json!([{
                "run_id": null, "task_id": null, "status": "stopped",
                "kind": "supervisor_stopped", "last_error": null, "next": "restart supervisor",
            }]);
            expected["cursor"] = serde_json::json!(0);
        }
        assert_eq!(report, expected, "{command}");
    }
    assert!(!invoke(&db, &["recover", "missing-run"]).status.success());
    assert!(!invoke(&db, &["show", "1"]).status.success());
    assert!(!invoke(&db, &["add", "  "]).status.success());
    assert_eq!(
        ok(&db, &["list"]),
        serde_json::json!({"tasks": [], "next": null, "total": 0})
    );
    ok(&db, &["add", "never run"]);
    // A `--db` queue that was never supervised is bound to no repository, so
    // there is nothing to land in; the run lookup comes first for a task.
    let output = invoke(&db, &["integrate", "1"]);
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    let message = error["error"].as_str().unwrap();
    assert!(
        message.contains("not bound to a repository") || message.contains("not inside a Git"),
        "{message}"
    );
    // `integrate` needs exactly one of a task ID or --next.
    assert!(!invoke(&db, &["integrate"]).status.success());
    assert!(!invoke(&db, &["integrate", "1", "--next"]).status.success());
}

#[test]
fn version_works_outside_a_repository_and_without_a_queue() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .arg("--version")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn goals_group_tasks_and_report_counts_by_status() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("goals.db");
    ok(&db, &["init"]);
    assert_eq!(ok(&db, &["goal", "list"]), serde_json::json!([]));
    let goal = ok(
        &db,
        &[
            "goal",
            "add",
            "Goal groups",
            "--description",
            "ADR-0009",
            "--acceptance",
            "four tasks landed",
            "--constraints",
            "no new crates",
            "--doc",
            "docs/adr/0009-goal-groups-tasks.md",
        ],
    );
    assert_eq!(goal["id"], 1);
    assert_eq!(goal["doc"], "docs/adr/0009-goal-groups-tasks.md");
    assert!(goal["closed_at"].is_null());
    let first = ok(
        &db,
        &[
            "add",
            "prompt",
            "--goal",
            "1",
            "--context",
            "Stage 1 of the ADR",
        ],
    );
    assert_eq!(first["goal_id"], 1);
    assert_eq!(first["context"], "Stage 1 of the ADR");
    let second = ok(&db, &["add", "entity", "--goal", "1", "--depends-on", "1"]);
    assert_eq!(second["context"], "");
    let alone = ok(&db, &["add", "stands alone"]);
    assert!(alone["goal_id"].is_null());
    ok(&db, &["ready", "2"]);
    let detail = ok(&db, &["show", "1"]);
    assert_eq!(detail["task"]["goal_id"], 1);
    assert_eq!(detail["task"]["context"], "Stage 1 of the ADR");
    let shown = ok(&db, &["goal", "show", "1"]);
    assert_eq!(shown["goal"]["title"], "Goal groups");
    assert_eq!(shown["closed"], false);
    assert_eq!(
        shown["tasks"],
        serde_json::json!([
            {"id": 1, "title": "prompt", "status": "draft"},
            {"id": 2, "title": "entity", "status": "ready"}
        ])
    );
    assert_eq!(shown["events"][0]["kind"], "goal_created");
    assert!(shown["events"][0]["run_id"].is_null());
    let listed = ok(&db, &["goal", "list"]);
    assert_eq!(listed[0]["id"], 1);
    assert_eq!(listed[0]["closed"], false);
    assert!(listed[0]["verdict"].is_null());
    assert_eq!(
        listed[0]["tasks"],
        serde_json::json!({"total": 2, "draft": 1, "ready": 1, "in_progress": 0, "completed": 0, "canceled": 0})
    );
    // Moving tasks and editing the goal.
    assert_eq!(ok(&db, &["set-goal", "3", "1"])["goal_id"], 1);
    assert!(ok(&db, &["set-goal", "3", "--none"])["goal_id"].is_null());
    assert!(!invoke(&db, &["set-goal", "3"]).status.success());
    assert!(
        !invoke(&db, &["set-goal", "3", "1", "--none"])
            .status
            .success()
    );
    assert!(!invoke(&db, &["goal", "edit", "1"]).status.success());
    let edited = ok(
        &db,
        &[
            "goal",
            "edit",
            "1",
            "--title",
            "Goal groups (ADR-0009)",
            "--doc",
            "",
        ],
    );
    assert_eq!(edited["title"], "Goal groups (ADR-0009)");
    assert!(edited["doc"].is_null());
    // Closing follows the task statuses and blocks further membership.
    let output = invoke(&db, &["goal", "close", "1", "--verdict", "achieved"]);
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("cannot be closed")
    );
    assert!(
        !invoke(&db, &["goal", "close", "1", "--verdict", "done"])
            .status
            .success()
    );
    ok(&db, &["cancel", "1"]);
    ok(&db, &["cancel", "2"]);
    let closed = ok(&db, &["goal", "close", "1", "--verdict", "achieved"]);
    assert_eq!(closed["verdict"], "achieved");
    assert!(closed["closed_at"].is_string());
    assert!(
        !invoke(&db, &["add", "late", "--goal", "1"])
            .status
            .success()
    );
    assert!(!invoke(&db, &["set-goal", "3", "1"]).status.success());
    assert!(!invoke(&db, &["goal", "show", "2"]).status.success());
    let listed = ok(&db, &["goal", "list"]);
    assert_eq!(listed[0]["closed"], true);
    assert_eq!(listed[0]["verdict"], "achieved");
    assert_eq!(listed[0]["tasks"]["canceled"], 2);
    assert_eq!(
        ok(&db, &["goal", "show", "1"])["events"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn list_options_filter_page_and_expand_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    let goal = ok(&db, &["goal", "add", "grouped"])["id"].to_string();
    let a = ok(&db, &["add", "first", "--goal", &goal])["id"]
        .as_i64()
        .unwrap();
    let b = ok(&db, &["add", "second", "--description", "long"])["id"]
        .as_i64()
        .unwrap();
    let c = ok(&db, &["add", "third"])["id"].as_i64().unwrap();
    ok(&db, &["ready", &a.to_string()]);
    ok(&db, &["cancel", &c.to_string()]);

    let ids = |value: &Value| -> Vec<i64> {
        value["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|task| task["id"].as_i64().unwrap())
            .collect()
    };
    let listed = ok(&db, &["list"]);
    assert_eq!(ids(&listed), vec![b, a]);
    assert_eq!(listed["total"], 2);
    assert!(listed["tasks"][0].get("description").is_none());
    assert_eq!(ids(&ok(&db, &["list", "--all"])), vec![c, b, a]);
    assert_eq!(
        ids(&ok(&db, &["list", "--status", "ready,canceled"])),
        vec![c, a]
    );
    assert_eq!(ids(&ok(&db, &["list", "--goal", &goal])), vec![a]);
    assert!(ids(&ok(&db, &["list", "--goal", &goal, "--status", "draft"])).is_empty());
    let page = ok(&db, &["list", "--all", "--limit", "2"]);
    assert_eq!(ids(&page), vec![c, b]);
    assert_eq!(page["next"], a);
    let rest = ok(
        &db,
        &["list", "--all", "--limit", "2", "--before", &a.to_string()],
    );
    assert_eq!(ids(&rest), vec![a]);
    assert_eq!(rest["next"], Value::Null);
    let full = ok(&db, &["list", "--full", "--status", "draft"]);
    assert_eq!(full["tasks"][0]["description"], "long");
    for key in [
        "acceptance",
        "context",
        "verification_commands",
        "created_at",
        "updated_at",
    ] {
        assert!(full["tasks"][0].get(key).is_some(), "{key}");
    }

    let output = invoke(&db, &["list", "--status", "ready,done"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(error["error"].as_str().unwrap().contains("done"));
}

#[test]
fn events_and_watch_read_past_a_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
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
fn show_goal_show_and_doctor_are_compact_unless_full() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
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
        ok(&db, &["ready", "1"]);
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
fn graph_reports_unfinished_dependencies_releases_and_the_critical_chain() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    let goal = ok(&db, &["goal", "add", "goal"])["id"].to_string();
    ok(&db, &["add", "in goal", "--goal", &goal]);
    ok(&db, &["add", "root"]);
    ok(&db, &["add", "middle", "--depends-on", "2"]);
    ok(&db, &["add", "leaf", "--depends-on", "3"]);
    ok(&db, &["add", "canceled", "--depends-on", "2"]);
    ok(
        &db,
        &["add", "after goal", "--goal", &goal, "--depends-on", "1"],
    );
    for id in ["1", "2", "3"] {
        ok(&db, &["ready", id]);
    }
    ok(&db, &["cancel", "5"]);

    let graph = ok(&db, &["graph"]);
    let tasks = graph["tasks"].as_array().unwrap();
    let ids: Vec<i64> = tasks.iter().map(|t| t["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, [1, 2, 3, 4, 6]);
    assert_eq!(
        tasks[1],
        serde_json::json!({
            "id": 2, "status": "ready", "title": "root", "goal_id": null,
            "depends_on": [], "blocks": [3], "unblocks": 2, "ready_after": [],
        })
    );
    assert_eq!(tasks[2]["ready_after"], serde_json::json!([2]));
    assert_eq!(tasks[0]["unblocks"], 1);
    assert_eq!(graph["candidates"], serde_json::json!([2, 1]));
    assert_eq!(graph["critical"], serde_json::json!([2, 3, 4]));

    let in_goal = ok(&db, &["graph", "--goal", &goal]);
    let ids: Vec<i64> = in_goal["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [1, 6]);
    assert_eq!(in_goal["candidates"], serde_json::json!([1]));
    assert_eq!(in_goal["critical"], serde_json::json!([1, 6]));
}

mod stats {
    use std::collections::HashMap;

    use dagq::domain::{
        RunEvent,
        stats::{SlotSnapshot, StatsQuery, stats, timestamp_millis},
    };
    use serde_json::{Value, json};

    use super::{invoke, ok};

    /// Builds run events one after another; `at` is minutes after 12:00.
    #[derive(Default)]
    struct Events(Vec<RunEvent>);

    impl Events {
        fn push(&mut self, task: i64, run: Option<&str>, kind: &str, minute: i64, payload: Value) {
            self.0.push(RunEvent {
                id: i64::try_from(self.0.len()).unwrap() + 1,
                task_id: Some(task),
                goal_id: None,
                run_id: run.map(str::to_owned),
                kind: kind.to_owned(),
                payload,
                created_at: format!(
                    "2026-09-23T{:02}:{:02}:00.000Z",
                    12 + minute / 60,
                    minute % 60
                ),
            });
        }

        fn run(&mut self, task: i64, run: &str, kind: &str, minute: i64) {
            self.push(task, Some(run), kind, minute, json!({}));
        }

        fn status(&mut self, task: i64, run: &str, kind: &str, minute: i64, status: &str) {
            self.push(task, Some(run), kind, minute, json!({"status": status}));
        }

        fn last_id(&self) -> i64 {
            self.0.last().unwrap().id
        }
    }

    /// 12:00 plus `minute` minutes, in unix seconds.
    fn at(minute: i64) -> i64 {
        timestamp_millis("2026-09-23T12:00:00Z").unwrap() / 1000 + minute * 60
    }

    fn value(stats: &impl serde::Serialize) -> Value {
        serde_json::to_value(stats).unwrap()
    }

    #[test]
    fn timestamps_parse_the_queue_format() {
        assert_eq!(timestamp_millis("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(timestamp_millis("1970-01-02T00:00:01.5Z"), Some(86_401_500));
        assert_eq!(
            timestamp_millis("2026-09-23 12:00:00"),
            Some(1_790_164_800_000)
        );
        assert_eq!(
            timestamp_millis("2000-02-29T00:00:00Z"),
            Some(951_782_400_000)
        );
        for bad in [
            "",
            "2026-09-23",
            "2026-13-01T00:00:00Z",
            "2026-09-23Tx:00:00Z",
        ] {
            assert_eq!(timestamp_millis(bad), None, "{bad}");
        }
    }

    #[test]
    fn runs_goals_and_alerts_come_from_the_event_sequence() {
        let mut events = Events::default();
        // Task 1 (goal 7): integrated after 10 min of work, 2 of validation,
        // 20 waiting to land; startup 3 min; one resume and a pass review.
        events.run(1, "a", "run_claimed", 0);
        events.run(1, "a", "agent_started", 1);
        events.run(1, "a", "first_commit_observed", 4);
        events.run(1, "a", "receipt_observed", 10);
        events.status(1, "a", "validation_finished", 12, "awaiting_integration");
        events.run(1, "a", "resume_started", 13);
        events.push(
            1,
            Some("a"),
            "review_finished",
            14,
            json!({"verdict": "pass"}),
        );
        events.run(1, "a", "integration_started", 30);
        events.run(1, "a", "run_integrated", 32);
        // Task 2 (goal 7): 40 min of work — twice the goal median is 2 × 25 —
        // then three needs_session and the landing.
        events.run(2, "b", "run_claimed", 0);
        events.run(2, "b", "receipt_observed", 40);
        events.status(2, "b", "validation_finished", 41, "awaiting_integration");
        for minute in [42, 43, 44] {
            events.status(2, "b", "integration_deferred", minute, "needs_session");
        }
        // An `integrate` that errors puts `needs_session` back; not a new park.
        events.status(2, "b", "integration_error", 44, "needs_session");
        events.run(2, "b", "run_integrated", 45);
        // Task 3 (no goal) failed twice in two runs; no receipt the second time.
        events.run(3, "c1", "run_claimed", 0);
        events.run(3, "c1", "receipt_observed", 5);
        events.status(3, "c1", "validation_finished", 6, "failed");
        events.run(3, "c2", "run_claimed", 10);
        events.status(3, "c2", "supervision_finished", 15, "failed");
        // Task 4 (goal 7) still waits to land since minute 50; an ask on it
        // has been open since minute 55, another was answered.
        events.run(4, "d", "run_claimed", 46);
        events.run(4, "d", "receipt_observed", 48);
        events.status(4, "d", "validation_finished", 50, "awaiting_integration");
        // A failed landing attempt does not restart the wait.
        events.run(4, "d", "integration_started", 52);
        events.status(4, "d", "integration_error", 53, "awaiting_integration");
        events.push(4, Some("d"), "ask_opened", 55, json!({"ask_id": 1}));
        events.push(4, None, "ask_opened", 56, json!({"ask_id": 2}));
        events.push(4, None, "ask_answered", 57, json!({"ask_id": 2}));
        let goals = HashMap::from([(1, Some(7)), (2, Some(7)), (3, None), (4, Some(7))]);
        let slots = SlotSnapshot {
            free_slots: 2,
            candidates: 0,
            ready: 1,
        };

        let report = value(&stats(
            &events.0,
            &goals,
            at(120),
            slots,
            &StatsQuery::default(),
        ));
        let runs = report["runs"].as_array().unwrap();
        let ids = runs.iter().map(|r| r["run_id"].clone()).collect::<Vec<_>>();
        // Finished runs in the order they finished; `d` is still in flight.
        assert_eq!(ids, [json!("a"), json!("b"), json!("c1"), json!("c2")]);
        assert_eq!(
            runs[0],
            json!({
                "run_id": "a", "task_id": 1, "goal_id": 7, "status": "integrated",
                "finished_event_id": 9, "work": 600, "validate": 120,
                "wait_to_land": 1200, "startup": 180, "resumes": 1,
                "review_verdict": "pass", "needs_session": 0, "failed": 0,
            })
        );
        assert_eq!(runs[1]["needs_session"], 3);
        assert!(runs[1]["startup"].is_null());
        assert!(runs[1]["review_verdict"].is_null());
        assert_eq!(runs[2]["status"], "failed");
        assert_eq!(runs[3]["work"], Value::Null);
        assert_eq!(
            report["goals"],
            json!([
                {"goal_id": 7, "runs": 2,
                 "work": {"count": 2, "total": 3000, "median": 1500},
                 "validate": {"count": 2, "total": 180, "median": 90},
                 "wait_to_land": {"count": 2, "total": 1440, "median": 720},
                 "startup": {"count": 1, "total": 180, "median": 180}},
                {"goal_id": null, "runs": 2,
                 "work": {"count": 1, "total": 300, "median": 300},
                 "validate": {"count": 1, "total": 60, "median": 60},
                 "wait_to_land": {"count": 0, "total": 0, "median": null},
                 "startup": {"count": 0, "total": 0, "median": null}},
            ])
        );
        assert_eq!(report["overall"]["runs"], 4);
        assert_eq!(report["overall"]["work"]["median"], 600);
        assert_eq!(report["next_cursor"], events.last_id());
        let alerts = report["alerts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                (
                    a["kind"].as_str().unwrap(),
                    a["run_id"].clone(),
                    a["value"].as_i64().unwrap(),
                    a["threshold"].as_i64().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            alerts,
            [
                ("awaiting_integration", json!("a"), 1200, 900),
                ("needs_session", json!("b"), 3, 3),
                ("awaiting_integration", json!("d"), 4200, 900),
                ("task_failed", json!("c2"), 2, 2),
                ("ask_unanswered", json!("d"), 3900, 3600),
                ("idle_slots", Value::Null, 2, 0),
            ]
        );
        // Task 2's 40 minutes are under twice goal 7's median (2 × 25 minutes).
        assert_eq!(report["alerts"][2]["task_id"], 4);
        assert!(report["alerts"][5]["task_id"].is_null());

        // --goal keeps the runs and alerts of goal 7's tasks only.
        let goal = value(&stats(
            &events.0,
            &goals,
            at(120),
            SlotSnapshot::default(),
            &StatsQuery {
                goal_id: Some(7),
                ..Default::default()
            },
        ));
        assert_eq!(goal["runs"].as_array().unwrap().len(), 2);
        assert_eq!(goal["goals"].as_array().unwrap().len(), 1);
        assert!(
            goal["alerts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|a| a["task_id"] != 3 && a["kind"] != "idle_slots")
        );

        // --since: only runs that finished after the cursor.
        let since = value(&stats(
            &events.0,
            &goals,
            at(120),
            SlotSnapshot::default(),
            &StatsQuery {
                since: Some(runs[1]["finished_event_id"].as_i64().unwrap()),
                ..Default::default()
            },
        ));
        let ids = since["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["run_id"].clone())
            .collect::<Vec<_>>();
        assert_eq!(ids, [json!("c1"), json!("c2")]);
        assert_eq!(since["next_cursor"], events.last_id());

        // Task 3's first failure is before the cursor, and still counts.
        let later = value(&stats(
            &events.0,
            &goals,
            at(120),
            SlotSnapshot::default(),
            &StatsQuery {
                since: Some(since["runs"][0]["finished_event_id"].as_i64().unwrap()),
                ..Default::default()
            },
        ));
        assert_eq!(later["runs"].as_array().unwrap().len(), 1);
        assert!(later["alerts"].as_array().unwrap().contains(&json!({
            "kind": "task_failed", "task_id": 3, "run_id": "c2", "value": 2, "threshold": 2
        })));
    }

    #[test]
    fn work_over_the_goal_median_is_an_alert() {
        let mut events = Events::default();
        for (task, work) in [(1, 10), (2, 10), (3, 30)] {
            let run = format!("r{task}");
            events.run(task, &run, "run_claimed", 0);
            events.run(task, &run, "receipt_observed", work);
            events.status(
                task,
                &run,
                "validation_finished",
                work,
                "awaiting_integration",
            );
            events.run(task, &run, "run_integrated", work + 1);
        }
        let goals = HashMap::from([(1, Some(1)), (2, Some(1)), (3, Some(1))]);
        let report = value(&stats(
            &events.0,
            &goals,
            at(60),
            SlotSnapshot::default(),
            &StatsQuery::default(),
        ));
        assert_eq!(
            report["alerts"],
            json!([{"kind": "work_over_median", "task_id": 3, "run_id": "r3",
                    "value": 1800, "threshold": 1200}])
        );
    }

    #[test]
    fn at_most_fifty_runs_unless_full_and_since_pages_forward() {
        let mut events = Events::default();
        for task in 1..=60 {
            let run = format!("r{task}");
            events.run(task, &run, "run_claimed", 0);
            events.status(task, &run, "supervision_finished", 1, "failed");
        }
        let goals = HashMap::new();
        let run = |query: StatsQuery| {
            value(&stats(
                &events.0,
                &goals,
                at(2),
                SlotSnapshot::default(),
                &query,
            ))
        };
        let latest = run(StatsQuery::default());
        assert_eq!(latest["runs"].as_array().unwrap().len(), 50);
        assert_eq!(latest["runs"][0]["run_id"], "r11");
        assert_eq!(latest["next_cursor"], events.last_id());
        assert_eq!(
            run(StatsQuery {
                full: true,
                ..Default::default()
            })["runs"]
                .as_array()
                .unwrap()
                .len(),
            60
        );
        let first = run(StatsQuery {
            since: Some(0),
            ..Default::default()
        });
        assert_eq!(first["runs"].as_array().unwrap().len(), 50);
        assert_eq!(first["runs"][49]["run_id"], "r50");
        assert_eq!(first["next_cursor"], 100);
        let rest = run(StatsQuery {
            since: Some(100),
            ..Default::default()
        });
        assert_eq!(rest["runs"].as_array().unwrap().len(), 10);
        assert_eq!(rest["runs"][0]["run_id"], "r51");
        assert_eq!(rest["next_cursor"], events.last_id());
        assert_eq!(
            run(StatsQuery {
                since: Some(events.last_id()),
                ..Default::default()
            })["runs"],
            json!([])
        );
    }

    #[test]
    fn cli_stats_reads_the_queue_and_since_returns_only_new_runs() {
        use dagq::{domain::ClaimOutcome, infrastructure::sqlite::SqliteQueue};
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("queue.db");
        ok(&db, &["init"]);
        let empty = ok(&db, &["stats"]);
        assert_eq!(empty["runs"], json!([]));
        assert_eq!(empty["alerts"], json!([]));
        assert_eq!(empty["overall"]["work"]["median"], Value::Null);
        ok(&db, &["goal", "add", "measured"]);
        ok(&db, &["add", "first", "--goal", "1"]);
        ok(&db, &["add", "second"]);
        ok(&db, &["ready", "1"]);
        ok(&db, &["ready", "2"]);
        let base = "0123456789abcdef0123456789abcdef01234567";
        let finish = |queue: &mut SqliteQueue| {
            let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(base, "t").unwrap()
            else {
                panic!("nothing to claim");
            };
            queue
                .record_runtime_event(&run.id, "receipt_observed", json!({}))
                .unwrap();
            queue
                .record_runtime_event(&run.id, "validation_finished", json!({"status": "failed"}))
                .unwrap();
            run.id
        };
        let mut queue = SqliteQueue::open(&db).unwrap();
        let first = finish(&mut queue);
        let report = ok(&db, &["stats"]);
        assert_eq!(report["runs"][0]["run_id"], first.as_str());
        assert_eq!(report["runs"][0]["goal_id"], 1);
        assert_eq!(report["runs"][0]["failed"], 1);
        assert!(report["runs"][0]["work"].is_i64());
        let cursor = report["next_cursor"].as_i64().unwrap();
        assert_eq!(cursor, ok(&db, &["status"])["cursor"].as_i64().unwrap());

        let second = finish(&mut queue);
        let since = ok(&db, &["stats", "--since", &cursor.to_string()]);
        assert_eq!(since["runs"].as_array().unwrap().len(), 1);
        assert_eq!(since["runs"][0]["run_id"], second.as_str());
        assert!(since["next_cursor"].as_i64().unwrap() > cursor);
        assert_eq!(
            ok(&db, &["stats", "--full"])["runs"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let goal = ok(&db, &["stats", "--goal", "1"]);
        assert_eq!(goal["runs"].as_array().unwrap().len(), 1);
        assert_eq!(goal["goals"][0]["goal_id"], 1);
        assert!(!invoke(&db, &["stats", "--since", "x"]).status.success());
    }
}
