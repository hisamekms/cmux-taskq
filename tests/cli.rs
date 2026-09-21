use std::{
    path::Path,
    process::{Command, Output},
};

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
    assert_eq!(ok(&db, &["init"])["schema_version"], 1);
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
    assert!(!invoke(&db, &["show", "1"]).status.success());
    assert!(!invoke(&db, &["add", "  "]).status.success());
    assert_eq!(ok(&db, &["list"]), serde_json::json!([]));
}
