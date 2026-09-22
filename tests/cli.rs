use std::{
    path::Path,
    process::{Command, Output},
};

use cmux_taskq::infrastructure::sqlite::SqliteQueue;
use serde_json::Value;

fn invoke(db: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cmux-taskq"))
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
    assert_eq!(ok(&db, &["list"]).as_array().unwrap().len(), 2);
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
        assert_eq!(
            report,
            serde_json::json!({"supervisors": [], "runs": []}),
            "{command}"
        );
    }
    assert!(!invoke(&db, &["recover", "missing-run"]).status.success());
    assert!(!invoke(&db, &["show", "1"]).status.success());
    assert!(!invoke(&db, &["add", "  "]).status.success());
    assert_eq!(ok(&db, &["list"]), serde_json::json!([]));
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
    let output = Command::new(env!("CARGO_BIN_EXE_cmux-taskq"))
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
