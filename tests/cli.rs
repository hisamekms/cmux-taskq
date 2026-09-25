mod common;

use common::Bounded;

use std::{
    path::Path,
    process::{Command, Output},
};

use dagq::infrastructure::sqlite::SqliteQueue;
use serde_json::Value;

fn invoke(db: &Path, args: &[&str]) -> Output {
    invoke_as(None, db, args)
}

/// Run with `DAGQ_ROLE` set to `role`, or unset: the tests do not inherit
/// the role of the session running them. A stub `cmux` next to the queue
/// comes first on PATH, so `ask` never notifies the person running the
/// tests; it appends its arguments to [`notifications`] instead.
fn invoke_as(role: Option<&str>, db: &Path, args: &[&str]) -> Output {
    let bin = db.parent().unwrap().join("bin");
    if !bin.join("cmux").exists() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&bin).unwrap();
        let stub = bin.join("cmux");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n",
                bin.join("notifications").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command.env("PATH", path).env_remove("DAGQ_ROLE");
    if let Some(role) = role {
        command.env("DAGQ_ROLE", role);
    }
    command
        .arg("--db")
        .arg(db)
        .args(args)
        .bounded_output()
        .unwrap()
}

/// Every argument the stub `cmux` of `db`'s directory was called with, one per line.
fn notifications(db: &Path) -> String {
    std::fs::read_to_string(db.parent().unwrap().join("bin/notifications")).unwrap_or_default()
}

fn ok_as(role: &str, db: &Path, args: &[&str]) -> Value {
    let output = invoke_as(Some(role), db, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
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
    ok(&db, &["ready", &b, "--bypass-review"]);
    assert_eq!(ok(&db, &["candidates"]), serde_json::json!([]));
    ok(&db, &["ready", &a, "--bypass-review"]);
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
            expected["asks"] = serde_json::json!([]);
            expected["proposals"] = serde_json::json!([]);
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
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!("{} {}", env!("CARGO_PKG_NAME"), dagq::VERSION)
    );
}

/// `--version` names the build (ADR-0045 decision 2): a development version
/// carries the commit it was built from, and a release its version alone.
#[test]
fn version_is_the_build_identifier() {
    let package = env!("CARGO_PKG_VERSION");
    if dagq::build_id::is_prerelease(package) {
        let metadata = dagq::VERSION
            .strip_prefix(&format!("{package}+"))
            .unwrap_or_else(|| panic!("{} lacks build metadata", dagq::VERSION));
        let commit = metadata.strip_suffix(".dirty").unwrap_or(metadata);
        assert!(
            commit == dagq::build_id::UNKNOWN_COMMIT
                || (commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit())),
            "{}",
            dagq::VERSION
        );
    } else {
        assert_eq!(dagq::VERSION, package);
    }
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
    ok(&db, &["ready", "2", "--bypass-review"]);
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
        serde_json::json!({"total": 2, "draft": 1, "submitted": 0, "ready": 1, "in_progress": 0,
                           "completed": 0, "canceled": 0})
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
    ok(&db, &["ready", &a.to_string(), "--bypass-review"]);
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
        queue
            .record_runtime_event(run.id(), kind, payload.clone())
            .unwrap();
        conn.execute(
            "UPDATE run_events SET created_at=?1 WHERE id=(SELECT MAX(id) FROM run_events)",
            [format!("2026-09-24T{at}.000Z")],
        )
        .unwrap();
    }
    run.id().as_str().to_owned()
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
        ["agent_started", "receipt_observed"]
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
fn ask_answer_asks_and_close_through_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    ok(&db, &["add", "first"]);
    let cursor = ok(&db, &["status"])["cursor"].as_i64().unwrap();
    let asked = ok(
        &db,
        &[
            "ask",
            "--kind",
            "decide",
            "--because",
            "recovery_failed",
            "--question",
            "Which ADR number?",
            "--option",
            "0029",
            "--option",
            "0030",
            "--task",
            "1",
        ],
    );
    assert_eq!(asked["created"], true);
    assert_eq!(asked["notified"], true);
    // A new ask sends one notification; no inbox is recorded, so it names
    // no workspace, and a `--db` queue is named after the working directory.
    let repo = std::env::current_dir().unwrap();
    let repo = repo.file_name().unwrap().to_string_lossy();
    assert_eq!(
        notifications(&db),
        format!("notify\n--title\n[{repo}] ask #1 decide\n--body\nWhich ADR number?\ntask 1\n")
    );
    assert_eq!(asked["kind"], "decide");
    assert_eq!(asked["reason_category"], "recovery_failed");
    assert_eq!(asked["task_id"], 1);
    assert_eq!(asked["options"], serde_json::json!(["0029", "0030"]));
    assert!(asked["answer"].is_null());
    let id = asked["id"].as_i64().unwrap();
    let id_text = id.to_string();
    // The same task and kind is not registered twice.
    let again = ok(
        &db,
        &[
            "ask",
            "--kind",
            "decide",
            "--because",
            "recovery_failed",
            "--question",
            "again",
            "--task",
            "1",
        ],
    );
    assert_eq!(again["created"], false);
    assert_eq!(again["notified"], false);
    assert_eq!(notifications(&db).matches("notify\n").count(), 1);
    assert_eq!(again["id"], id);
    assert_eq!(again["question"], "Which ADR number?");
    // Missing target, unknown kind, unknown task and a blank question fail.
    for args in [
        &[
            "ask",
            "--kind",
            "decide",
            "--because",
            "recovery_failed",
            "--question",
            "q",
        ][..],
        &[
            "ask",
            "--kind",
            "bogus",
            "--because",
            "recovery_failed",
            "--question",
            "q",
            "--task",
            "1",
        ],
        &[
            "ask",
            "--kind",
            "decide",
            "--because",
            "recovery_failed",
            "--question",
            "q",
            "--task",
            "9",
        ],
        &[
            "ask",
            "--kind",
            "decide",
            "--because",
            "recovery_failed",
            "--question",
            " ",
            "--task",
            "1",
        ],
        &[
            "ask",
            "--kind",
            "decide",
            "--because",
            "recovery_failed",
            "--question",
            "q",
            "--run",
            "nope",
        ],
        &["status", "--role", "worker"],
    ] {
        assert!(!invoke(&db, args).status.success(), "{args:?}");
    }
    // Every ask says why a person is needed (ADR-0047 decision 41): none,
    // an unknown reason, or authentication and cost (the runtime's one
    // queue_hold ask per queue) fail, and nothing is registered.
    for because in [None, Some("bogus"), Some("authentication"), Some("cost")] {
        let mut args = vec![
            "ask",
            "--kind",
            "decide",
            "--question",
            "why?",
            "--task",
            "1",
        ];
        if let Some(because) = because {
            args.extend(["--because", because]);
        }
        let output = invoke(&db, &args);
        assert!(!output.status.success(), "{because:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        match because {
            None => assert!(stderr.contains("--because"), "{stderr}"),
            Some("authentication") => {
                assert!(
                    stderr.contains("queue_hold asks the runtime opens"),
                    "{stderr}"
                )
            }
            _ => {}
        }
    }
    assert!(
        !invoke(
            &db,
            &[
                "ask",
                "--kind",
                "queue_hold",
                "--because",
                "authentication",
                "--question",
                "q"
            ]
        )
        .status
        .success()
    );
    assert_eq!(notifications(&db).matches("notify\n").count(), 1);

    let status = ok(&db, &["status", "--role", "inbox"]);
    assert_eq!(status["asks"][0]["id"], id);
    assert_eq!(status["asks"][0]["question"], "Which ADR number?");
    assert_eq!(status["asks"][0]["reason_category"], "recovery_failed");
    // All attention is the inbox's (ADR-0024 decision 6), the stopped
    // supervisor included; none is the planner's.
    assert_eq!(status["attention"][0]["kind"], "supervisor_stopped");
    assert_eq!(status["attention"][1]["kind"], "ask_opened");
    assert_eq!(status["attention"][1]["reason_category"], "recovery_failed");
    assert_eq!(status["attention"].as_array().unwrap().len(), 2);
    let planner = ok(&db, &["status", "--role", "planner"]);
    assert_eq!(planner["attention"], serde_json::json!([]));
    assert_eq!(planner["asks"][0]["id"], id);
    assert_eq!(ok(&db, &["asks", "--open"])["asks"][0]["id"], id);
    assert_eq!(
        ok(&db, &["asks", "--role", "planner"])["asks"],
        serde_json::json!([])
    );

    // watch --role inbox wakes on ask_opened; the planner's times out.
    let after = cursor.to_string();
    let inbox = ok(&db, &["watch", "--after", &after, "--role", "inbox"]);
    assert_eq!(inbox["events"][0]["kind"], "ask_opened");
    assert_eq!(inbox["events"][0]["ask_id"], id);
    let quiet = ok(
        &db,
        &[
            "watch",
            "--after",
            &after,
            "--role",
            "planner",
            "--timeout",
            "0",
        ],
    );
    assert_eq!(quiet["events"], serde_json::json!([]));

    let answered = ok(&db, &["answer", &id_text, "--text", "0030"]);
    assert_eq!(answered["answer"], "0030");
    assert!(answered["answered_at"].is_i64());
    assert!(
        !invoke(&db, &["answer", &id_text, "--text", "x"])
            .status
            .success()
    );
    let opened = inbox["cursor"].as_i64().unwrap().to_string();
    let woke = ok(&db, &["watch", "--after", &opened, "--role", "inbox"]);
    assert_eq!(woke["events"][0]["kind"], "ask_answered");
    assert_eq!(
        woke["events"][0]["next"],
        format!("read the answer of ask {id} and close it")
    );
    let quiet = ok(
        &db,
        &[
            "watch",
            "--after",
            &opened,
            "--role",
            "planner",
            "--timeout",
            "0",
        ],
    );
    assert_eq!(quiet["events"], serde_json::json!([]));
    assert_eq!(ok(&db, &["status"])["asks"], serde_json::json!([]));
    assert_eq!(ok(&db, &["asks", "--role", "inbox"])["asks"][0]["id"], id);

    let closed = ok(&db, &["ask", "close", &id_text]);
    assert!(closed["closed_at"].is_i64());
    assert!(!invoke(&db, &["ask", "close", &id_text]).status.success());
    assert_eq!(ok(&db, &["asks"])["asks"], serde_json::json!([]));
    assert_eq!(ok(&db, &["asks", "--all"])["asks"][0]["id"], id);
    // An open ask is not closed; it is withdrawn by answering it.
    let next = ok(
        &db,
        &[
            "ask",
            "--kind",
            "decide",
            "--because",
            "recovery_failed",
            "--question",
            "next",
            "--task",
            "1",
        ],
    );
    assert_eq!(next["created"], true);
    let next_id = next["id"].to_string();
    assert!(!invoke(&db, &["ask", "close", &next_id]).status.success());
    ok(&db, &["answer", &next_id, "--text", "withdrawn"]);
    ok(&db, &["ask", "close", &next_id]);
    assert!(!invoke(&db, &["ask", "close", "99"]).status.success());
    assert_eq!(
        ok(&db, &["asks", "--role", "inbox"])["asks"],
        serde_json::json!([])
    );
    // Answers and closes notify nobody: two asks, two notifications.
    assert_eq!(notifications(&db).matches("notify\n").count(), 2);
    // The ask stands when the notification cannot go out.
    let unsent = ok(
        &db,
        &[
            "ask",
            "--kind",
            "answer_prompt",
            "--because",
            "recovery_failed",
            "--question",
            "q",
            "--task",
            "1",
            "--cmux",
            "/nonexistent/cmux",
        ],
    );
    assert_eq!(unsent["created"], true);
    assert_eq!(unsent["notified"], false);
    assert!(unsent["notify_error"].is_string());
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
        ok(&db, &["ready", id, "--bypass-review"]);
    }
    ok(&db, &["cancel", "5"]);

    let graph = ok(&db, &["graph"]);
    let tasks = graph["tasks"].as_array().unwrap();
    let ids: Vec<i64> = tasks.iter().map(|t| t["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, [1, 2, 3, 4, 6]);
    assert_eq!(
        tasks[1],
        serde_json::json!({
            "id": 2, "status": "ready", "priority": "normal",
            "effective_priority": "normal", "title": "root", "goal_id": null,
            "depends_on": [], "goal_dependencies": [], "blocks": [3], "unblocks": 2,
            "ready_after": [],
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

/// The domain's refusal of `args`, as the CLI prints it.
fn refused(db: &Path, args: &[&str]) -> String {
    let output = invoke(db, args);
    assert!(!output.status.success(), "{args:?} succeeded");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    error["error"].as_str().unwrap().to_owned()
}

#[test]
fn a_task_waits_for_its_goal_dependency_until_the_goal_is_achieved() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    // Goal 1 holds task 1; task 2 (no goal) waits for goal 1.
    ok(&db, &["goal", "add", "upstream"]);
    ok(&db, &["add", "upstream work", "--goal", "1"]);
    let waiting = ok(&db, &["add", "downstream", "--depends-on-goal", "1"]);
    assert_eq!(waiting["id"], 2);
    for id in ["1", "2"] {
        ok(&db, &["ready", id, "--bypass-review"]);
    }
    let shown = ok(&db, &["show", "2"]);
    assert_eq!(shown["goal_dependencies"], serde_json::json!([1]));
    assert_eq!(shown["dependencies"], serde_json::json!([]));
    assert_eq!(
        ok(&db, &["show", "2", "--full"])["goal_dependencies"],
        serde_json::json!([1])
    );
    let listed = ok(&db, &["list"]);
    assert_eq!(listed["tasks"][0]["id"], 2);
    assert_eq!(
        listed["tasks"][0]["goal_dependencies"],
        serde_json::json!([1])
    );
    assert_eq!(
        listed["tasks"][1]["goal_dependencies"],
        serde_json::json!([])
    );
    let goal = ok(&db, &["goal", "show", "1"]);
    assert_eq!(
        goal["dependents"],
        serde_json::json!([{"id": 2, "title": "downstream", "status": "ready"}])
    );
    let candidates = |db: &Path| -> Vec<i64> {
        ok(db, &["candidates"])
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_i64().unwrap())
            .collect()
    };
    assert_eq!(candidates(&db), [1]);
    let graph = ok(&db, &["graph"]);
    assert_eq!(graph["tasks"][0]["blocks"], serde_json::json!([2]));
    assert_eq!(graph["tasks"][0]["unblocks"], 1);
    assert_eq!(
        graph["tasks"][1]["ready_after"],
        serde_json::json!([{"goal": 1}])
    );
    assert_eq!(
        graph["tasks"][1]["goal_dependencies"],
        serde_json::json!([1])
    );
    assert_eq!(graph["critical"], serde_json::json!([1, 2]));

    // The goal's only task is done, yet the goal is open: still waiting.
    // Canceling stands in for completion; both are terminal for the goal.
    ok(&db, &["cancel", "1"]);
    assert!(candidates(&db).is_empty());
    assert_eq!(
        ok(&db, &["graph"])["tasks"][0]["ready_after"],
        serde_json::json!([{"goal": 1}])
    );
    ok(&db, &["goal", "close", "1", "--verdict", "achieved"]);
    assert_eq!(candidates(&db), [2]);
    assert_eq!(
        ok(&db, &["graph"])["tasks"][0]["ready_after"],
        serde_json::json!([])
    );

    // An abandoned goal never releases its dependents.
    ok(&db, &["goal", "add", "dropped"]);
    ok(&db, &["add", "stuck", "--depends-on-goal", "2"]);
    ok(&db, &["ready", "3", "--bypass-review"]);
    ok(&db, &["goal", "close", "2", "--verdict", "abandoned"]);
    assert_eq!(candidates(&db), [2]);
    let graph = ok(&db, &["graph"]);
    let stuck = graph["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == 3)
        .unwrap();
    assert_eq!(stuck["ready_after"], serde_json::json!([{"goal": 2}]));

    // dependency add / remove --goal.
    ok(&db, &["goal", "add", "third"]);
    let added = ok(&db, &["dependency", "add", "3", "--goal", "3"]);
    assert_eq!(added["goal_dependencies"], serde_json::json!([2, 3]));
    let removed = ok(&db, &["dependency", "remove", "3", "--goal", "2"]);
    assert_eq!(removed["goal_dependencies"], serde_json::json!([3]));
    assert!(refused(&db, &["dependency", "remove", "3", "--goal", "2"]).contains("does not exist"));
    assert!(!invoke(&db, &["dependency", "add", "3"]).status.success());
    assert!(
        !invoke(&db, &["dependency", "add", "3", "1", "--goal", "3"])
            .status
            .success()
    );
    let kinds: Vec<String> = ok(&db, &["show", "3", "--full"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_owned())
        .collect();
    assert!(kinds.contains(&"goal_dependency_added".to_owned()));
    assert!(kinds.contains(&"goal_dependency_removed".to_owned()));
}

#[test]
fn goal_dependencies_on_the_own_goal_or_through_membership_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    ok(&db, &["goal", "add", "a"]);
    ok(&db, &["goal", "add", "b"]);
    // Task 1 in goal 1; task 2 in goal 2 waits for goal 1.
    ok(&db, &["add", "in a", "--goal", "1"]);
    ok(
        &db,
        &["add", "in b", "--goal", "2", "--depends-on-goal", "1"],
    );
    let own = "a task cannot depend on its own goal 1; the goal already waits for it";
    assert_eq!(
        refused(&db, &["dependency", "add", "1", "--goal", "1"]),
        own
    );
    assert_eq!(
        refused(&db, &["add", "x", "--goal", "1", "--depends-on-goal", "1"]),
        own
    );
    // Goal 1 waiting for goal 2 through task 1 closes 2 -> 1 -> 1's task.
    assert_eq!(
        refused(&db, &["dependency", "add", "1", "--goal", "2"]),
        "dependency 1 -> goal 2 would create a cycle"
    );
    // A task of goal 1 waiting for goal 2 is refused at registration too.
    assert_eq!(
        refused(&db, &["add", "y", "--goal", "1", "--depends-on-goal", "2"]),
        "dependency 3 -> goal 2 would create a cycle"
    );
    // A task dependency that closes the loop through the goals. The
    // refused registrations took no ID, so this is task 3.
    assert_eq!(ok(&db, &["add", "free"])["id"], 3);
    assert_eq!(
        refused(&db, &["dependency", "add", "1", "2"]),
        "dependency 1 -> 2 would create a cycle"
    );
    // set-goal: task 2 into goal 1, which it waits for.
    assert_eq!(refused(&db, &["set-goal", "2", "1"]), own);
    // set-goal: task 3 waits for task 2 (which waits for goal 1); moving it
    // into goal 1 makes goal 1 wait for it.
    ok(&db, &["dependency", "add", "3", "2"]);
    assert_eq!(
        refused(&db, &["set-goal", "3", "1"]),
        "moving task 3 to goal 1 would create a cycle: the task already waits for the goal"
    );
    // Nothing refused was written; the unrelated moves still work.
    assert_eq!(
        ok(&db, &["show", "1"])["goal_dependencies"],
        serde_json::json!([])
    );
    assert_eq!(ok(&db, &["set-goal", "3", "2"])["goal_id"], 2);
    assert_eq!(ok(&db, &["set-goal", "2", "2"])["goal_id"], 2);
}

#[test]
fn draft_goal_tasks_wait_for_goal_ready() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    let goal = ok(&db, &["goal", "add", "proposal", "--draft"]);
    assert_eq!(goal["status"], "draft");
    let id = goal["id"].to_string();
    ok(&db, &["add", "proposed", "--goal", &id]);
    ok(&db, &["ready", "1", "--bypass-review"]);
    assert_eq!(ok(&db, &["candidates"]), serde_json::json!([]));
    assert_eq!(ok(&db, &["goal", "list"])[0]["status"], "draft");
    assert_eq!(ok(&db, &["goal", "show", &id])["goal"]["status"], "draft");
    let graph = ok(&db, &["graph"]);
    assert_eq!(graph["tasks"][0]["goal_status"], "draft");
    assert_eq!(graph["candidates"], serde_json::json!([]));

    let opened = ok(&db, &["goal", "ready", &id]);
    assert_eq!(opened["status"], "open");
    let candidates = ok(&db, &["candidates"]);
    assert_eq!(candidates[0]["id"], 1);
    assert_eq!(ok(&db, &["graph"])["tasks"][0]["goal_status"], "open");
    let again = invoke(&db, &["goal", "ready", &id]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("is not a draft"));
    // A goal registered without --draft is open.
    assert_eq!(ok(&db, &["goal", "add", "plain"])["status"], "open");
}

#[test]
fn ready_tasks_of_a_draft_goal_do_not_raise_idle_slots() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    SqliteQueue::open(&db)
        .unwrap()
        .register_supervisor("live", std::process::id(), 2, "0.0.1")
        .unwrap();
    let idle = |db: &Path| {
        ok(db, &["stats"])["alerts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|alert| alert["kind"] == "idle_slots")
    };
    ok(&db, &["goal", "add", "proposal", "--draft"]);
    ok(&db, &["add", "proposed", "--goal", "1"]);
    ok(&db, &["ready", "1", "--bypass-review"]);
    assert!(!idle(&db));
    // A ready task blocked by a predecessor still raises it.
    ok(&db, &["add", "first"]);
    ok(&db, &["add", "blocked", "--depends-on", "2"]);
    ok(&db, &["ready", "3", "--bypass-review"]);
    assert!(idle(&db));
}

#[test]
fn cancel_duplicate_of_is_recorded_shown_listed_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    for title in ["original", "copy", "another copy", "unrelated", "plain"] {
        ok(&db, &["add", title]);
    }
    // Invalid targets: none, itself, a canceled task and one canceled as a duplicate.
    assert!(
        refused(&db, &["cancel", "2", "--duplicate-of", "99"]).contains("task 99 does not exist")
    );
    assert!(refused(&db, &["cancel", "2", "--duplicate-of", "2"]).contains("itself"));
    ok(&db, &["cancel", "5"]);
    assert!(refused(&db, &["cancel", "2", "--duplicate-of", "5"]).contains("task 5 is canceled"));
    let canceled = ok(&db, &["cancel", "2", "--duplicate-of", "1"]);
    assert_eq!(canceled["status"], "canceled");
    let chained = refused(&db, &["cancel", "3", "--duplicate-of", "2"]);
    assert!(
        chained.contains("duplicate of task 1") && chained.contains("--duplicate-of 1"),
        "{chained}"
    );
    // The duplicate cannot point back: task 1 is refused as a duplicate of its duplicate.
    assert!(refused(&db, &["cancel", "1", "--duplicate-of", "2"]).contains("task 2 is canceled"));
    ok(&db, &["cancel", "3", "--duplicate-of", "1"]);

    let copy = ok(&db, &["show", "2"]);
    assert_eq!(copy["duplicate_of"], 1);
    assert_eq!(copy["duplicates"], serde_json::json!([]));
    let original = ok(&db, &["show", "1"]);
    assert_eq!(original["duplicate_of"], Value::Null);
    assert_eq!(original["duplicates"], serde_json::json!([2, 3]));
    assert_eq!(ok(&db, &["show", "2", "--full"])["duplicate_of"], 1);
    assert_eq!(ok(&db, &["show", "5"])["duplicate_of"], Value::Null);

    let listed = ok(&db, &["list", "--all"]);
    let row = |id: i64| {
        listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(row(2)["duplicate_of"], 1);
    assert_eq!(row(3)["duplicate_of"], 1);
    assert!(row(5).get("duplicate_of").is_none());
    assert!(row(1).get("duplicate_of").is_none());

    let stats = ok(&db, &["stats", "--full"]);
    assert_eq!(stats["duplicate_cancels"]["count"], 2);
    assert_eq!(
        stats["duplicate_cancels"]["tasks"],
        serde_json::json!([
            {"task_id": 2, "duplicate_of": 1},
            {"task_id": 3, "duplicate_of": 1},
        ])
    );
    // Past the cursor, nothing new was canceled as a duplicate.
    let cursor = stats["next_cursor"].as_i64().unwrap().to_string();
    let later = ok(&db, &["stats", "--since", &cursor]);
    assert_eq!(later["duplicate_cancels"]["count"], 0);
    ok(&db, &["cancel", "4", "--duplicate-of", "1"]);
    let later = ok(&db, &["stats", "--since", &cursor]);
    assert_eq!(later["duplicate_cancels"]["count"], 1);
    assert_eq!(later["duplicate_cancels"]["tasks"][0]["task_id"], 4);
}

#[test]
fn notes_are_observations_read_by_notes_show_and_goal_show() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    ok(&db, &["goal", "add", "observed"]);
    ok(&db, &["add", "task", "--goal", "1"]);
    let on_goal = ok(&db, &["note", "--goal", "1", "--text", "goal is slow"]);
    assert_eq!(on_goal["kind"], "observation");
    assert_eq!(
        on_goal["payload"],
        serde_json::json!({"text": "goal is slow", "kind": "note", "by": "human"})
    );
    let on_task = ok_as(
        "observer",
        &db,
        &[
            "note",
            "--task",
            "1",
            "--text",
            "failed twice",
            "--kind",
            "retry",
        ],
    );
    assert_eq!(on_task["task_id"], 1);
    assert_eq!(on_task["payload"]["by"], "observer");
    assert_eq!(on_task["payload"]["kind"], "retry");
    assert!(
        !invoke(&db, &["note", "--task", "1", "--goal", "1", "--text", "x"])
            .status
            .success()
    );
    assert!(!invoke(&db, &["note", "--text", "x"]).status.success());
    assert!(
        !invoke(&db, &["note", "--run", "missing", "--text", "x"])
            .status
            .success()
    );
    assert!(
        !invoke(
            &db,
            &["note", "--goal", "1", "--text", "x", "--kind", "Bad Kind"]
        )
        .status
        .success()
    );

    let notes = ok(&db, &["notes"]);
    let texts = |page: &Value| {
        page["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["payload"]["text"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(texts(&notes), ["goal is slow", "failed twice"]);
    assert_eq!(notes["cursor"], on_task["id"]);
    assert_eq!(texts(&ok(&db, &["notes", "--goal", "1"])).len(), 2);
    assert_eq!(texts(&ok(&db, &["notes", "--task", "1"])), ["failed twice"]);
    let since = on_goal["id"].to_string();
    assert_eq!(
        texts(&ok(&db, &["notes", "--since", &since, "--limit", "1"])),
        ["failed twice"]
    );

    let shown = ok(&db, &["show", "1"]);
    assert_eq!(shown["observations"][0]["text"], "failed twice");
    assert_eq!(shown["observations"][0]["by"], "observer");
    let goal = ok(&db, &["goal", "show", "1"]);
    assert_eq!(
        goal["observations"],
        serde_json::json!([{"id": on_goal["id"], "created_at": on_goal["created_at"],
                            "text": "goal is slow", "kind": "note", "by": "human"}])
    );
}

#[test]
fn observer_may_note_and_propose_but_not_change_queue_state() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    ok(&db, &["goal", "add", "open goal"]);
    ok(&db, &["add", "existing", "--goal", "1"]);
    let denied = |args: &[&str]| {
        let output = invoke_as(Some("observer"), &db, args);
        assert!(!output.status.success(), "{args:?} was allowed");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(
            error,
            serde_json::json!({"error": "observer may not change queue state"}),
            "{args:?}"
        );
    };
    for args in [
        &["ready", "1", "--bypass-review"][..],
        &["submit", "1"],
        &["draft", "1"],
        &["cancel", "1"],
        &["integrate", "1"],
        &["integrate", "--next"],
        &["recover", "run"],
        &["goal", "close", "1", "--verdict", "abandoned"],
        &["goal", "ready", "1"],
        &["goal", "edit", "1", "--title", "x"],
        &["goal", "add", "not a draft"],
        &["add", "loose"],
        &["add", "into open goal", "--goal", "1"],
        &["set-goal", "1", "--none"],
        &["dependency", "add", "1", "1"],
        &["init"],
        &["review", "1"],
        &["down"],
        &["plan"],
        &["planner-session", "--planner", "1", "--claude", "claude"],
        &[
            "ask",
            "--kind",
            "decide",
            "--because",
            "recovery_failed",
            "--question",
            "q",
            "--task",
            "1",
        ],
        &["answer", "1", "--text", "x"],
        &["ask", "close", "1"],
        // The observer does not start another observer.
        &["observe", "--dry-run"],
        &["supervise", "--once"],
    ] {
        denied(args);
    }

    // Reads, notes, a draft goal and draft tasks in it are allowed.
    for args in [
        &["list"][..],
        &["show", "1"],
        &["candidates"],
        &["graph"],
        &["status"],
        &["events"],
        &["stats"],
        &["doctor"],
        &["goal", "list"],
        &["goal", "show", "1"],
        &["notes"],
        &["asks"],
        &["status", "--role", "inbox"],
        &["planners", "--all"],
    ] {
        ok_as("observer", &db, args);
    }
    ok_as("observer", &db, &["note", "--goal", "1", "--text", "seen"]);
    // A threshold crossing goes to the inbox as a blocked ask, on a task or
    // on nothing; registering the same one again returns the open ask.
    let on_task = ok_as(
        "observer",
        &db,
        &[
            "ask",
            "--kind",
            "blocked",
            "--because",
            "scope",
            "--question",
            "stuck",
            "--task",
            "1",
        ],
    );
    assert_eq!(on_task["task_id"], 1);
    assert_eq!(on_task["asked_by"], "observer");
    let idle = ok_as(
        "observer",
        &db,
        &[
            "ask",
            "--kind",
            "blocked",
            "--because",
            "scope",
            "--question",
            "slots idle",
            "--option",
            "leave it",
        ],
    );
    assert_eq!(idle["task_id"], Value::Null);
    assert_eq!(idle["created"], true);
    // The blocked ask on no task is notified with its question alone.
    assert_eq!(idle["notified"], true);
    assert!(
        notifications(&db).ends_with(&format!(
            "--title\n[{}] ask #{} blocked\n--body\nslots idle\n",
            std::env::current_dir()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy(),
            idle["id"]
        )),
        "{}",
        notifications(&db)
    );
    let again = ok_as(
        "observer",
        &db,
        &[
            "ask",
            "--kind",
            "blocked",
            "--because",
            "scope",
            "--question",
            "slots idle again",
        ],
    );
    assert_eq!(
        (again["id"].clone(), again["created"].clone()),
        (idle["id"].clone(), Value::Bool(false))
    );
    assert_eq!(notifications(&db).matches("notify\n").count(), 2);
    let inbox = ok(&db, &["status", "--role", "inbox"]);
    assert!(
        inbox["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["ask_id"] == idle["id"] && a["task_id"].is_null()),
        "{inbox}"
    );
    let draft = ok_as("observer", &db, &["goal", "add", "proposal", "--draft"]);
    assert_eq!(draft["status"], "draft");
    let task = ok_as(
        "observer",
        &db,
        &["add", "proposed", "--goal", "2", "--depends-on", "1"],
    );
    assert_eq!(task["status"], "draft");
    // The observer cannot adopt its own proposal; the planner does.
    denied(&["goal", "ready", "2"]);
    ok(&db, &["goal", "ready", "2"]);
    denied(&["add", "after adoption", "--goal", "2"]);
    // Other roles are not restricted.
    ok_as("planner", &db, &["add", "planned"]);
}

mod stats {
    use std::collections::HashMap;

    use dagq::domain::{
        EventId, GoalId, RunEvent, RunId, TaskId,
        stats::{LiveSnapshot, SlotSnapshot, StatsQuery, stats, timestamp_millis},
    };
    use serde_json::{Value, json};

    use super::{invoke, ok};

    /// Builds run events one after another; `at` is minutes after 12:00.
    #[derive(Default)]
    struct Events(Vec<RunEvent>);

    impl Events {
        fn push(&mut self, task: i64, run: Option<&str>, kind: &str, minute: i64, payload: Value) {
            self.0.push(RunEvent {
                id: EventId::new(i64::try_from(self.0.len()).unwrap() + 1),
                task_id: Some(TaskId::new(task)),
                goal_id: None,
                run_id: run.map(|run| RunId::new(run).unwrap()),
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
            self.0.last().unwrap().id.as_i64()
        }
    }

    /// Task → goal, as the queue maps them.
    fn goals<const N: usize>(pairs: [(i64, Option<i64>); N]) -> HashMap<TaskId, Option<GoalId>> {
        pairs
            .into_iter()
            .map(|(task, goal)| (TaskId::new(task), goal.map(GoalId::new)))
            .collect()
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
        // A resume that ends with the run still parked is not a new park either.
        events.status(2, "b", "resume_finished", 44, "needs_session");
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
        let goals = goals([(1, Some(7)), (2, Some(7)), (3, None), (4, Some(7))]);
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
            &LiveSnapshot::default(),
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
                goal_id: Some(GoalId::new(7)),
                ..Default::default()
            },
            &LiveSnapshot::default(),
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
                since: Some(EventId::new(runs[1]["finished_event_id"].as_i64().unwrap())),
                ..Default::default()
            },
            &LiveSnapshot::default(),
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
                since: Some(EventId::new(
                    since["runs"][0]["finished_event_id"].as_i64().unwrap(),
                )),
                ..Default::default()
            },
            &LiveSnapshot::default(),
        ));
        assert_eq!(later["runs"].as_array().unwrap().len(), 1);
        assert!(later["alerts"].as_array().unwrap().contains(&json!({
            "kind": "task_failed", "task_id": 3, "run_id": "c2", "value": 2, "threshold": 2
        })));
    }

    #[test]
    fn backend_failures_are_counted_per_window_with_the_highest_load() {
        let mut events = Events::default();
        let failure = |op: &str, load: Value, slots: i64| {
            json!({"op": op, "workspace_id": "w", "timeout_secs": 30, "error": "timed out",
                   "load_avg": load, "slots": slots, "parallel": 4})
        };
        events.run(1, "a", "run_claimed", 0);
        events.push(
            1,
            Some("a"),
            "backend_call_failed",
            1,
            failure("close", json!(23.5), 3),
        );
        events.status(1, "a", "supervision_finished", 2, "failed");
        let cursor = events.last_id();
        events.run(2, "b", "run_claimed", 3);
        events.push(
            2,
            Some("b"),
            "backend_call_failed",
            4,
            failure("send_exit", json!(34.25), 4),
        );
        events.push(
            2,
            Some("b"),
            "backend_call_failed",
            5,
            failure("send_exit", Value::Null, 2),
        );
        // A call for no run (up's group): no task, no run.
        events.push(
            1,
            None,
            "backend_call_failed",
            6,
            failure("ensure_group", json!(1.0), 0),
        );
        events.0.last_mut().unwrap().task_id = None;
        events.status(2, "b", "supervision_finished", 7, "failed");
        let goals = goals([(1, None), (2, Some(5))]);
        let run = |query: StatsQuery| {
            value(&stats(
                &events.0,
                &goals,
                at(8),
                SlotSnapshot::default(),
                &query,
                &LiveSnapshot::default(),
            ))
        };
        let alert = json!({"kind": "backend_failures", "task_id": null, "run_id": null,
                           "value": 4, "threshold": 2});

        let all = run(StatsQuery::default());
        assert_eq!(
            all["backend_failures"],
            json!({"count": 4, "by_op": {"close": 1, "ensure_group": 1, "send_exit": 2},
                   "max_load_avg": 34.25, "max_slots": 4})
        );
        assert!(all["alerts"].as_array().unwrap().contains(&alert));

        // Past the cursor: three failures, the close is before it.
        let since = run(StatsQuery {
            since: Some(EventId::new(cursor)),
            ..Default::default()
        });
        assert_eq!(since["backend_failures"]["count"], 3);
        assert_eq!(since["backend_failures"]["by_op"]["close"], Value::Null);
        assert!(
            since["alerts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["kind"] == "backend_failures" && a["value"] == 3)
        );

        // --goal keeps only the failures of its runs; one is no alert.
        let goal = run(StatsQuery {
            goal_id: Some(GoalId::new(5)),
            since: Some(EventId::new(cursor)),
            ..Default::default()
        });
        assert_eq!(goal["backend_failures"]["count"], 2);
        let only_close = run(StatsQuery {
            goal_id: Some(GoalId::new(9)),
            full: true,
            ..Default::default()
        });
        assert_eq!(
            only_close["backend_failures"],
            json!({"count": 0, "by_op": {}, "max_load_avg": null, "max_slots": null})
        );

        // Nothing past the last event: an empty window.
        let empty = run(StatsQuery {
            since: Some(EventId::new(events.last_id())),
            ..Default::default()
        });
        assert_eq!(empty["backend_failures"]["count"], 0);
        assert!(
            !empty["alerts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["kind"] == "backend_failures")
        );
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
        let goals = goals([(1, Some(1)), (2, Some(1)), (3, Some(1))]);
        let report = value(&stats(
            &events.0,
            &goals,
            at(60),
            SlotSnapshot::default(),
            &StatsQuery::default(),
            &LiveSnapshot::default(),
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
                &LiveSnapshot::default(),
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
            since: Some(EventId::new(0)),
            ..Default::default()
        });
        assert_eq!(first["runs"].as_array().unwrap().len(), 50);
        assert_eq!(first["runs"][49]["run_id"], "r50");
        assert_eq!(first["next_cursor"], 100);
        let rest = run(StatsQuery {
            since: Some(EventId::new(100)),
            ..Default::default()
        });
        assert_eq!(rest["runs"].as_array().unwrap().len(), 10);
        assert_eq!(rest["runs"][0]["run_id"], "r51");
        assert_eq!(rest["next_cursor"], events.last_id());
        assert_eq!(
            run(StatsQuery {
                since: Some(EventId::new(events.last_id())),
                ..Default::default()
            })["runs"],
            json!([])
        );
    }

    #[test]
    fn cli_stats_reads_the_queue_and_since_returns_only_new_runs() {
        use dagq::{
            domain::{ClaimOutcome, CommitSha},
            infrastructure::sqlite::SqliteQueue,
        };
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
        ok(&db, &["ready", "1", "--bypass-review"]);
        ok(&db, &["ready", "2", "--bypass-review"]);
        let base = "0123456789abcdef0123456789abcdef01234567";
        let finish = |queue: &mut SqliteQueue| {
            let ClaimOutcome::Claimed { run } = queue
                .claim_for_supervisor(&CommitSha::try_from(base).unwrap(), "t")
                .unwrap()
            else {
                panic!("nothing to claim");
            };
            queue
                .record_runtime_event(run.id(), "receipt_observed", json!({}))
                .unwrap();
            queue
                .record_runtime_event(run.id(), "validation_finished", json!({"status": "failed"}))
                .unwrap();
            run.id().clone()
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

/// `add --paths` stores the globs (ADR-0029), `show` and `list --full`
/// print them, and `set-paths` replaces them or removes them with --none.
#[test]
fn add_paths_is_stored_shown_and_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    let added = ok(
        &db,
        &[
            "add",
            "docs change",
            "--paths",
            "docs/**",
            "--paths",
            "*.md",
        ],
    );
    let expected = serde_json::json!(["docs/**", "*.md"]);
    assert_eq!(added["paths"], expected);
    let id = added["id"].to_string();
    assert_eq!(ok(&db, &["show", &id])["task"]["paths"], expected);
    assert_eq!(ok(&db, &["list", "--full"])["tasks"][0]["paths"], expected);
    assert!(ok(&db, &["list"])["tasks"][0].get("paths").is_none());
    let replaced = ok(&db, &["set-paths", &id, "--paths", "src/**"]);
    assert_eq!(replaced["paths"], serde_json::json!(["src/**"]));
    let cleared = ok(&db, &["set-paths", &id, "--none"]);
    assert_eq!(cleared["paths"], serde_json::json!([]));
    // Without --paths nothing is limited.
    assert_eq!(ok(&db, &["add", "any"])["paths"], serde_json::json!([]));
    for args in [
        &["add", "bad", "--paths", "/abs"][..],
        &["set-paths", &id][..],
        &["set-paths", &id, "--paths", "x", "--none"][..],
    ] {
        assert!(!invoke(&db, args).status.success(), "{args:?}");
    }
}

/// `add --priority` and `set-priority` take the level names only; `show`,
/// `list`, `candidates` and `graph` print them, and `candidates` and
/// `graph` agree on the claim order (ADR-0040 decision 4).
#[test]
fn priority_is_named_changed_while_editable_and_orders_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    assert_eq!(ok(&db, &["add", "plain"])["priority"], "normal");
    let low = ok(&db, &["add", "later", "--priority", "low"]);
    assert_eq!(low["priority"], "low");
    ok(&db, &["add", "base"]);
    ok(
        &db,
        &[
            "add",
            "urgent waiter",
            "--priority",
            "urgent",
            "--depends-on",
            "3",
        ],
    );
    for id in ["1", "2", "3", "4"] {
        ok(&db, &["ready", id, "--bypass-review"]);
    }
    assert_eq!(ok(&db, &["show", "2"])["task"]["priority"], "low");
    assert_eq!(ok(&db, &["list"])["tasks"][0]["priority"], "urgent");

    let candidates = ok(&db, &["candidates"]);
    let order: Vec<(i64, &str, &str)> = candidates
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["id"].as_i64().unwrap(),
                t["priority"].as_str().unwrap(),
                t["effective_priority"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        order,
        [
            (3, "normal", "urgent"),
            (1, "normal", "normal"),
            (2, "low", "low")
        ]
    );
    assert_eq!(
        ok(&db, &["graph"])["candidates"],
        serde_json::json!([3, 1, 2])
    );

    let raised = ok(&db, &["set-priority", "2", "interrupt"]);
    assert_eq!(raised["priority"], "interrupt");
    assert_eq!(
        ok(&db, &["graph"])["candidates"],
        serde_json::json!([2, 3, 1])
    );
    let ids: Vec<i64> = ok(&db, &["candidates"])
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [2, 3, 1]);

    for args in [
        &["add", "numeric", "--priority", "3"][..],
        &["add", "unknown", "--priority", "critical"][..],
        &["set-priority", "1", "4"][..],
        &["set-priority", "1", "High"][..],
        &["set-priority", "1"][..],
    ] {
        assert!(!invoke(&db, args).status.success(), "{args:?}");
    }
    ok(&db, &["cancel", "1"]);
    assert_eq!(
        refused(&db, &["set-priority", "1", "high"]),
        "the priority can only be changed for draft, submitted or ready tasks"
    );
}

/// `edit` replaces the given fields of a draft task, a repeatable flag the
/// whole list (`--no-*` empties it); `show` prints the change as
/// `task_edited`, and a ready task is refused (ADR-0041 decision 9).
#[test]
fn edit_replaces_fields_of_a_draft_task_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    let id = ok(
        &db,
        &[
            "add",
            "old",
            "--verify",
            "cargo test",
            "--evidence",
            "tests",
            "--paths",
            "src/**",
        ],
    )["id"]
        .to_string();
    let edited = ok(
        &db,
        &[
            "edit",
            &id,
            "--title",
            "new",
            "--description",
            "d",
            "--acceptance",
            "a",
            "--context",
            "c",
            "--verify",
            "cargo fmt --all --check",
            "--verify",
            "cargo test --locked --test plugin",
            "--evidence",
            "e2e",
            "--paths",
            "docs/**",
        ],
    );
    assert_eq!(
        (
            &edited["title"],
            &edited["description"],
            &edited["acceptance"],
            &edited["context"]
        ),
        (
            &serde_json::json!("new"),
            &serde_json::json!("d"),
            &serde_json::json!("a"),
            &serde_json::json!("c")
        )
    );
    assert_eq!(
        edited["verification_commands"],
        serde_json::json!([
            "cargo fmt --all --check",
            "cargo test --locked --test plugin"
        ])
    );
    assert_eq!(edited["required_evidence"], serde_json::json!(["e2e"]));
    assert_eq!(edited["paths"], serde_json::json!(["docs/**"]));
    let cleared = ok(
        &db,
        &["edit", &id, "--no-verify", "--no-evidence", "--no-paths"],
    );
    for field in ["verification_commands", "required_evidence", "paths"] {
        assert_eq!(cleared[field], serde_json::json!([]), "{field}");
    }
    assert_eq!(cleared["title"], "new");
    let shown = ok(&db, &["show", &id, "--full"]);
    let edits: Vec<&Value> = shown["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "task_edited")
        .collect();
    assert_eq!(edits.len(), 2);
    assert_eq!(edits[0]["payload"]["from"]["title"], "old");
    assert_eq!(edits[0]["payload"]["to"]["title"], "new");
    assert_eq!(
        edits[1]["payload"]["to"],
        serde_json::json!({"verification_commands": [], "required_evidence": [], "paths": []})
    );
    // The compact `show` cuts the long texts inside `from` / `to`.
    let long = "x".repeat(400);
    ok(&db, &["edit", &id, "--description", &long]);
    let compact = ok(&db, &["show", &id]);
    let latest = compact["events"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["kind"] == "task_edited")
        .unwrap();
    let to = &latest["payload"]["to"];
    assert!(to["description"].as_str().unwrap().ends_with('…'));
    assert_eq!(to["truncated"], true);
    assert_eq!(latest["payload"]["from"]["description"], "d");
    for args in [
        &["edit", &id][..],
        &["edit", &id, "--verify", "x", "--no-verify"][..],
        &["edit", &id, "--evidence", "coverage"][..],
        &["edit", &id, "--paths", "/abs"][..],
        &["edit", &id, "--title", " "][..],
    ] {
        assert!(!invoke(&db, args).status.success(), "{args:?}");
    }
    ok(&db, &["ready", &id, "--bypass-review"]);
    assert_eq!(
        refused(&db, &["edit", &id, "--title", "late"]),
        format!("task {id} is ready; only a draft or submitted task can be edited")
    );
    // Back to draft, it can be edited again.
    ok(&db, &["draft", &id]);
    assert_eq!(ok(&db, &["edit", &id, "--title", "late"])["title"], "late");
}

#[test]
fn add_evidence_is_stored_and_shown() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    let added = ok(
        &db,
        &[
            "add",
            "runtime change",
            "--evidence",
            "e2e",
            "--evidence",
            "subagent_review",
            "--evidence",
            "e2e",
        ],
    );
    // Each check once, in the order given.
    let expected = serde_json::json!(["e2e", "subagent_review"]);
    assert_eq!(added["required_evidence"], expected);
    let id = added["id"].to_string();
    assert_eq!(
        ok(&db, &["show", &id])["task"]["required_evidence"],
        expected
    );
    assert_eq!(
        ok(&db, &["show", &id, "--full"])["task"]["required_evidence"],
        expected
    );
    assert_eq!(
        ok(&db, &["list", "--full"])["tasks"][0]["required_evidence"],
        expected
    );
    assert!(
        ok(&db, &["list"])["tasks"][0]
            .get("required_evidence")
            .is_none()
    );
    // Without --evidence nothing is required.
    let plain = ok(&db, &["add", "docs change"]);
    assert_eq!(plain["required_evidence"], serde_json::json!([]));
    // Only receipt check names are accepted.
    let output = invoke(&db, &["add", "bad", "--evidence", "coverage"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("coverage"));
}

/// The supervisor's headless review runs under `DAGQ_ROLE=reviewer`
/// (ADR-0027): it may read the queue and nothing else.
#[test]
fn reviewer_may_only_read_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    ok(&db, &["goal", "add", "open goal"]);
    ok(&db, &["add", "existing", "--goal", "1"]);
    for args in [
        &["ready", "1", "--bypass-review"][..],
        &["note", "--task", "1", "--text", "x"],
        &[
            "ask",
            "--kind",
            "decide",
            "--because",
            "recovery_failed",
            "--question",
            "q",
            "--task",
            "1",
        ],
        &["integrate", "1"],
        &["review", "1"],
        &["goal", "add", "draft", "--draft"],
        &["submit", "1"],
        &["plan"],
    ] {
        let output = invoke_as(Some("reviewer"), &db, args);
        assert!(!output.status.success(), "{args:?} was allowed");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(
            error,
            serde_json::json!({"error": "reviewer may not change queue state"}),
            "{args:?}"
        );
    }
    for args in [
        &["list"][..],
        &["show", "1"],
        &["status"],
        &["asks"],
        &["notes"],
        &["goal", "show", "1"],
        &["proposal", "list", "--all"],
        &["planners"],
    ] {
        ok_as("reviewer", &db, args);
    }
    // No planner was opened yet.
    assert_eq!(
        ok(&db, &["planners", "--all"]),
        serde_json::json!({"planners": []})
    );
}

/// `submit` as a planner session would run it: in a cmux workspace, with
/// the planner's origin when the runtime opened it.
fn submit_from(db: &Path, workspace: Option<&str>, origin: Option<&str>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command
        .env_remove("DAGQ_ROLE")
        .env_remove("CMUX_WORKSPACE_ID")
        .env_remove("DAGQ_PLANNER_ORIGIN");
    if let Some(workspace) = workspace {
        command.env("CMUX_WORKSPACE_ID", workspace);
    }
    if let Some(origin) = origin {
        command.env("DAGQ_PLANNER_ORIGIN", origin);
    }
    command
        .arg("--db")
        .arg(db)
        .arg("submit")
        .args(args)
        .bounded_output()
        .unwrap()
}

/// ADR-0041 decisions 7 and 8: `submit` bundles drafts into a proposal
/// owned by the planner's workspace; a submitted task is never claimed,
/// and only plan review or `ready --bypass-review` makes it ready.
#[test]
fn submit_bundles_drafts_into_a_proposal_that_plan_review_or_a_bypass_readies() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    ok(&db, &["goal", "add", "planned", "--draft"]);
    ok(&db, &["add", "in goal", "--goal", "1"]);
    ok(&db, &["add", "alone"]);
    ok(&db, &["add", "left out"]);

    let submitted = submit_from(&db, Some("W-1"), None, &["2", "--goal", "1"]);
    assert!(
        submitted.status.success(),
        "{}",
        String::from_utf8_lossy(&submitted.stderr)
    );
    let proposal: Value = serde_json::from_slice(&submitted.stdout).unwrap();
    assert_eq!(proposal["id"], 1);
    assert_eq!(proposal["status"], "submitted");
    assert_eq!(proposal["task_ids"], serde_json::json!([1, 2]));
    assert_eq!(proposal["goal_ids"], serde_json::json!([1]));
    assert_eq!(
        proposal["owner"],
        serde_json::json!({"origin": "person", "workspace_id": "W-1"})
    );
    assert_eq!(ok(&db, &["show", "1"])["task"]["status"], "submitted");
    let listed = ok(&db, &["list", "--status", "submitted"]);
    assert_eq!(listed["total"], 2);
    assert_eq!(ok(&db, &["candidates"]), serde_json::json!([]));
    let graph = ok(&db, &["graph"]);
    assert_eq!(graph["candidates"], serde_json::json!([]));
    assert_eq!(ok(&db, &["status"])["proposals"][0]["id"], 1);
    assert_eq!(ok(&db, &["proposal", "list"])["proposals"][0]["id"], 1);
    assert_eq!(
        ok(&db, &["proposal", "show", "1"])["task_ids"],
        serde_json::json!([1, 2])
    );
    assert_eq!(ok(&db, &["goal", "list"])[0]["tasks"]["submitted"], 1);
    // A submitted task is still edited in place.
    ok(&db, &["edit", "1", "--acceptance", "sharper"]);

    // Without the bypass, ready is plan review's.
    for id in ["1", "3"] {
        let refused = invoke(&db, &["ready", id]);
        assert!(!refused.status.success());
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("pass --bypass-review"),
            "{}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }
    let again = submit_from(&db, None, None, &["1"]);
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("task 1 already belongs to proposal 1")
    );
    let bad_origin = submit_from(&db, None, Some("robot"), &["3"]);
    assert!(!bad_origin.status.success());
    assert!(!invoke(&db, &["submit"]).status.success(), "no members");

    let runtime = submit_from(&db, None, Some("runtime"), &["3"]);
    let runtime: Value = serde_json::from_slice(&runtime.stdout).unwrap();
    assert_eq!(
        runtime["owner"],
        serde_json::json!({"origin": "runtime", "workspace_id": null})
    );

    let bypassed = ok(&db, &["ready", "1", "--bypass-review"]);
    assert_eq!(bypassed["status"], "ready");
    let shown = ok(&db, &["show", "1", "--full"]);
    assert!(
        shown["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "review_bypassed")
    );
    assert_eq!(
        ok(&db, &["candidates"]).as_array().unwrap().len(),
        0,
        "draft goal"
    );
    ok(&db, &["draft", "2"]);
    assert_eq!(ok(&db, &["show", "2"])["task"]["status"], "draft");
}

/// `proposal withdraw` releases a submitted proposal's goal and tasks as
/// drafts for another proposal; the observer and the reviewer may not run it.
#[test]
fn proposal_withdraw_releases_the_members_for_another_proposal() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    ok(&db, &["goal", "add", "planned", "--draft"]);
    ok(&db, &["add", "in goal", "--goal", "1"]);
    ok(&db, &["add", "alone"]);
    let submitted = submit_from(&db, Some("W-1"), None, &["2", "--goal", "1"]);
    assert!(submitted.status.success());

    for role in ["observer", "reviewer"] {
        let refused = invoke_as(Some(role), &db, &["proposal", "withdraw", "1"]);
        assert!(!refused.status.success(), "{role}");
        ok_as(role, &db, &["proposal", "show", "1"]);
    }
    let withdrawn = ok(&db, &["proposal", "withdraw", "1"]);
    assert_eq!(withdrawn["status"], "canceled");
    assert_eq!(withdrawn["task_ids"], serde_json::json!([1, 2]));
    for id in ["1", "2"] {
        assert_eq!(ok(&db, &["show", id])["task"]["status"], "draft");
    }
    assert_eq!(
        ok(&db, &["proposal", "list"]),
        serde_json::json!({"proposals": []})
    );
    let again = invoke(&db, &["proposal", "withdraw", "1"]);
    assert!(!again.status.success());
    assert!(
        String::from_utf8_lossy(&again.stderr)
            .contains("only a submitted or revising proposal is withdrawn")
    );

    let resubmitted = submit_from(&db, Some("W-2"), None, &["2", "--goal", "1"]);
    let resubmitted: Value = serde_json::from_slice(&resubmitted.stdout).unwrap();
    assert_eq!(resubmitted["id"], 2);
    assert_eq!(resubmitted["task_ids"], serde_json::json!([1, 2]));
    assert_eq!(resubmitted["goal_ids"], serde_json::json!([1]));
}

/// Runs `binary` (a copy of this one, as `claim` leaves a run's wrapper in
/// `runs/<id>/runner`) against `db`.
fn run_copy(binary: &Path, db: &Path, args: &[&str]) -> Output {
    Command::new(binary)
        .env_remove("DAGQ_ROLE")
        .arg("--db")
        .arg(db)
        .args(args)
        .bounded_output()
        .unwrap()
}

#[test]
fn migrate_is_explicit_and_older_binaries_keep_working_within_the_floor() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let raw = rusqlite::Connection::open(&db).unwrap();
    let version = || -> i64 {
        raw.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    };
    // A queue left at schema 23 by the binary before the floor table.
    for migration in &dagq::infrastructure::schema::MIGRATIONS[..23] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch("PRAGMA application_id = 1129599281; PRAGMA user_version = 23;")
        .unwrap();
    raw.execute_batch(
        "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('old task','','','[]','draft');",
    )
    .unwrap();
    // Commands that change the queue need `migrate` first.
    for args in [&["add", "new task"][..], &["ready", "1"], &["init"]] {
        let error = refused(&db, args);
        assert!(error.contains("run `dagq migrate`"), "{args:?}: {error}");
    }
    // Commands that only read open it read-only and see it as migrated in
    // memory; the file stays at schema 23 (ADR-0045 decision 18).
    assert_eq!(ok(&db, &["list"])["total"], 1);
    assert_eq!(ok(&db, &["show", "1"])["task"]["title"], "old task");
    for args in [
        &["status"][..],
        &["graph"],
        &["stats"],
        &["doctor"],
        &["goal", "list"],
    ] {
        ok(&db, args);
    }
    assert_eq!(version(), 23);
    let check = ok(&db, &["migrate", "--check"]);
    assert_eq!(check["schema_version"], 23);
    assert_eq!(check["opens"], false);
    assert_eq!(
        check["pending"],
        serde_json::json!([
            {"version": 24, "compatible": false},
            {"version": 25, "compatible": false},
            {"version": 26, "compatible": true},
            {"version": 27, "compatible": false},
            {"version": 28, "compatible": false},
            {"version": 29, "compatible": false}
        ])
    );
    assert_eq!(version(), 23);
    let migrated = ok(&db, &["migrate"]);
    assert_eq!(migrated["previous_version"], 23);
    assert_eq!(migrated["schema_version"], SqliteQueue::SCHEMA_VERSION);
    assert_eq!(
        migrated["floor"],
        dagq::infrastructure::schema::floor_for(SqliteQueue::SCHEMA_VERSION)
    );
    assert_eq!(migrated["commit_messages_filled"], 0);
    let backup = migrated["backup"].as_str().unwrap();
    assert!(Path::new(backup).starts_with(dir.path().canonicalize().unwrap().join("backups")));
    assert_eq!(version(), SqliteQueue::SCHEMA_VERSION);
    ok(&db, &["add", "task one"]);

    // A later binary's compatible migration: this binary, and the wrapper
    // copy of it a running run uses, now are the older binaries.
    let runner = dir.path().join("runs/run-1/runner");
    std::fs::create_dir_all(runner.parent().unwrap()).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_dagq"), &runner).unwrap();
    raw.execute_batch(&format!(
        "ALTER TABLE tasks ADD COLUMN future_hint TEXT;
         CREATE TABLE future_things (id INTEGER PRIMARY KEY);
         PRAGMA user_version = {};",
        SqliteQueue::SCHEMA_VERSION + 1
    ))
    .unwrap();
    for args in [
        &["add", "task two"][..],
        &["show", "2"],
        &["list"],
        &["status"],
        &["doctor"],
        &["migrate"],
    ] {
        let output = run_copy(&runner, &db, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(ok(&db, &["list"])["total"], 3);
    assert_eq!(version(), SqliteQueue::SCHEMA_VERSION + 1);

    // A later breaking migration raises the floor: the older binaries stop
    // with the reason, and leave the queue alone.
    raw.execute_batch(&format!(
        "UPDATE schema_floor SET floor = {0}; PRAGMA user_version = {0};",
        SqliteQueue::SCHEMA_VERSION + 2
    ))
    .unwrap();
    for args in [
        &["list"][..],
        &["migrate"],
        &[
            "session",
            "--run",
            "run-1",
            "--lease",
            "token",
            "--claude",
            "/bin/false",
        ],
    ] {
        let output = run_copy(&runner, &db, args);
        assert!(!output.status.success(), "{args:?} succeeded");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        let error = error["error"].as_str().unwrap();
        assert!(
            error.contains(&format!(
                "unsupported queue schema version {0}: the queue refuses binaries older than schema {0}",
                SqliteQueue::SCHEMA_VERSION + 2
            )),
            "{args:?}: {error}"
        );
    }
    assert_eq!(version(), SqliteQueue::SCHEMA_VERSION + 2);
}

/// ADR-0041 decision 10: `lint` checks the fixed rules of TASKs and of a
/// proposal's members, printing each violation with its code, and an empty
/// list when the plan passes. It only reads, so the observer and the
/// reviewer may run it.
#[test]
fn lint_reports_violations_by_code_and_passes_a_sound_plan() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    let sound = ["--acceptance", "works", "--verify", "cargo test"];
    ok(&db, &[&["add", "done"][..], &sound].concat());
    ok(&db, &["ready", "1", "--bypass-review"]);
    ok(&db, &["cancel", "1"]);
    ok(&db, &[&["add", "sound"][..], &sound].concat());
    ok(&db, &["add", "loose", "--depends-on", "1"]);
    ok(
        &db,
        &[&["add", "Sound", "--depends-on", "2"][..], &sound].concat(),
    );

    let passed = ok(&db, &["lint", "2"]);
    assert_eq!(passed, serde_json::json!({"tasks": [2], "violations": []}));

    let submitted = submit_from(&db, None, None, &["3", "4"]);
    assert!(submitted.status.success());
    let linted = ok(&db, &["lint", "2", "--proposal", "1"]);
    assert_eq!(linted["tasks"], serde_json::json!([2, 3, 4]));
    let found: Vec<(i64, &str)> = linted["violations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| (v["task_id"].as_i64().unwrap(), v["code"].as_str().unwrap()))
        .collect();
    assert_eq!(
        found,
        [
            (2, "duplicate_title"),
            (3, "depends_on_canceled"),
            (3, "unscoped_without_verification"),
            (3, "blank_acceptance"),
            (4, "duplicate_title"),
        ]
    );
    assert_eq!(
        linted["violations"][1]["reason"],
        "it depends on task 1, which was canceled and never completes"
    );
    // A task given both directly and through its proposal is linted once.
    assert_eq!(
        ok(&db, &["lint", "4", "--proposal", "1"])["tasks"],
        serde_json::json!([4, 3])
    );
    assert_eq!(
        ok_as("observer", &db, &["lint", "2"])["violations"],
        serde_json::json!([])
    );
    assert_eq!(
        ok_as("reviewer", &db, &["lint", "2"])["violations"],
        serde_json::json!([])
    );

    // A missing task or proposal is an error, and lint needs a target.
    assert!(!invoke(&db, &["lint", "99"]).status.success());
    assert!(!invoke(&db, &["lint", "--proposal", "9"]).status.success());
    assert!(!invoke(&db, &["lint"]).status.success());
}

#[test]
fn search_prints_hits_with_excerpts_and_checks_its_filters() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
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
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
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

/// Only when [`a_wait_past_its_limit_fails_with_the_test_and_the_condition`]
/// runs it: a wait that never ends, timed with a short limit.
#[test]
#[ignore = "run by a_wait_past_its_limit_fails_with_the_test_and_the_condition"]
fn deadline_probe() {
    if std::env::var_os("DAGQ_DEADLINE_PROBE").is_none() {
        return;
    }
    let _waiting = common::within(
        std::time::Duration::from_millis(300),
        "the probe's condition to hold",
    );
    loop {
        std::thread::park();
    }
}

/// A wait past its limit ends the test binary as a failure, naming the test
/// and the condition it waited for (task 324), instead of hanging.
#[test]
fn a_wait_past_its_limit_fails_with_the_test_and_the_condition() {
    let started = std::time::Instant::now();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "deadline_probe",
            "--ignored",
            "--test-threads",
            "2",
        ])
        .env("DAGQ_DEADLINE_PROBE", "1")
        .bounded_output()
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(common::TIMED_OUT), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "test deadline_probe timed out: the probe's condition to hold did not happen within 300ms"
        ),
        "{stderr}"
    );
}
