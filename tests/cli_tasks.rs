mod common;

use common::cli::*;

use dagq::infrastructure::sqlite::SqliteQueue;
use serde_json::Value;

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
            expected["version"] = serde_json::json!(dagq::VERSION);
            expected["auto_update"] = serde_json::json!({"enabled": false, "state": "idle"});
            // No landing was rechecked yet (ADR-0068 decision 6).
            expected["landing_recheck"] = serde_json::Value::Null;
        } else {
            expected["schema"] = serde_json::json!({
                "schema_version": SqliteQueue::SCHEMA_VERSION,
                "binary_schema_version": SqliteQueue::SCHEMA_VERSION,
                "floor": dagq::infrastructure::schema::floor_for(SqliteQueue::SCHEMA_VERSION),
                "pending": [],
                "opens": true,
            });
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
fn list_options_filter_page_and_expand_tasks() {
    let (_dir, db) = queue();
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
fn cancel_duplicate_of_is_recorded_shown_listed_and_counted() {
    let (_dir, db) = queue();
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

/// `add --paths` stores the globs (ADR-0029), `show` and `list --full`
/// print them, and `set-paths` replaces them or removes them with --none.
#[test]
fn add_paths_is_stored_shown_and_replaced() {
    let (_dir, db) = queue();
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

/// `add --kind` and `edit --kind` (goal 21): the kind is stored, shown and
/// listed, a task without one has null, and only a draft or submitted task
/// changes it.
#[test]
fn kind_is_added_shown_listed_and_edited_before_ready() {
    let (_dir, db) = queue();
    let added = ok(&db, &["add", "docs change", "--kind", "docs"]);
    assert_eq!(added["kind"], "docs");
    let id = added["id"].to_string();
    assert_eq!(ok(&db, &["show", &id])["task"]["kind"], "docs");
    assert_eq!(ok(&db, &["show", &id, "--full"])["task"]["kind"], "docs");
    assert_eq!(ok(&db, &["list"])["tasks"][0]["kind"], "docs");
    let plain = ok(&db, &["add", "unsaid"]);
    assert_eq!(plain["kind"], Value::Null);
    assert_eq!(ok(&db, &["list"])["tasks"][0]["kind"], Value::Null);
    let edited = ok(&db, &["edit", &id, "--kind", "plugin"]);
    assert_eq!(edited["kind"], "plugin");
    let event = ok(&db, &["show", &id, "--full"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["kind"] == "task_edited")
        .unwrap()
        .clone();
    assert_eq!(event["payload"]["from"]["kind"], "docs");
    assert_eq!(event["payload"]["to"]["kind"], "plugin");
    // The same kind again changes nothing and records no event.
    ok(&db, &["edit", &id, "--kind", "plugin"]);
    let edits = ok(&db, &["show", &id, "--full"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "task_edited")
        .count();
    assert_eq!(edits, 1);
    for args in [
        &["add", "bad", "--kind", "src"][..],
        &["edit", &id, "--kind", "tests"][..],
    ] {
        assert!(!invoke(&db, args).status.success(), "{args:?}");
    }
    ok(&db, &["ready", &id, "--bypass-review"]);
    assert_eq!(
        refused(&db, &["edit", &id, "--kind", "runtime"]),
        format!("task {id} is ready; only a draft or submitted task can be edited")
    );
    assert_eq!(ok(&db, &["show", &id])["task"]["kind"], "plugin");
}

/// `add --priority` and `set-priority` take the level names only; `show`,
/// `list`, `candidates` and `graph` print them, and `candidates` and
/// `graph` agree on the claim order (ADR-0040 decision 4).
#[test]
fn priority_is_named_changed_while_editable_and_orders_candidates() {
    let (_dir, db) = queue();
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
    let (_dir, db) = queue();
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
    let (_dir, db) = queue();
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
