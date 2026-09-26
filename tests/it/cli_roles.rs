use crate::common;

use common::cli::*;

use serde_json::Value;

#[test]
fn ask_answer_asks_and_close_through_the_cli() {
    let (_dir, db) = queue();
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

    let answered = ok_as("inbox", &db, &["answer", &id_text, "--text", "0030"]);
    assert_eq!(answered["answer"], "0030");
    assert!(answered["answered_at"].is_i64());
    // Task 325: who answered (the session's role) and the option chosen.
    assert_eq!(answered["answered_by"], "inbox");
    assert_eq!(answered["option_index"], 1);
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
    let withdrawn = ok(&db, &["answer", &next_id, "--text", "withdrawn"]);
    // A plain terminal is a person; an answer no option names is free.
    assert_eq!(withdrawn["answered_by"], "person");
    assert!(withdrawn["option_index"].is_null());
    ok(&db, &["ask", "close", &next_id]);
    // `stats` counts both asks by kind, asker, answerer and choice.
    let asks = &ok(&db, &["stats", "--full"])["asks"];
    assert_eq!(asks["opened"]["count"], 2, "{asks}");
    assert_eq!(asks["opened"]["by_kind"]["decide"], 2, "{asks}");
    assert_eq!(asks["opened"]["by_asked_by"]["human"], 2, "{asks}");
    assert_eq!(
        asks["opened"]["by_reason_category"]["recovery_failed"], 2,
        "{asks}"
    );
    // `auto_repairs` puts the asks opened each day next to the repairs:
    // none here.
    let repairs = &ok(&db, &["stats", "--full"])["auto_repairs"];
    assert_eq!(repairs["count"], 0, "{repairs}");
    assert_eq!(repairs["by_layer"], serde_json::json!({}));
    let days = repairs["by_day"].as_object().unwrap();
    assert_eq!(days.len(), 1, "{repairs}");
    let day = days.values().next().unwrap();
    assert_eq!(day["asks_opened"], 2, "{repairs}");
    assert_eq!(day["auto_repaired"], 0, "{repairs}");
    assert_eq!(day["asks_by_reason"]["recovery_failed"], 2, "{repairs}");
    assert_eq!(
        asks["answered"]["by_answered_by"],
        serde_json::json!({"inbox": 1, "person": 1})
    );
    assert_eq!(
        asks["answered"]["choices"]["decide"],
        serde_json::json!({"by_option": {"0030": 1}, "free": 1, "unknown": 0})
    );
    // `--since` past both leaves nothing.
    let latest = ok(&db, &["status"])["cursor"].to_string();
    let later = &ok(&db, &["stats", "--since", &latest])["asks"];
    assert_eq!(later["opened"]["count"], 0, "{later}");
    assert_eq!(later["answered"]["count"], 0, "{later}");
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
fn notes_are_observations_read_by_notes_show_and_goal_show() {
    let (_dir, db) = queue();
    ok(&db, &["goal", "add", "observed"]);
    ok(&db, &["add", "task", "--goal", "1"]);
    let on_goal = ok(&db, &["note", "--goal", "1", "--text", "goal is slow"]);
    assert_eq!(on_goal["kind"], "observation");
    assert_eq!(
        on_goal["payload"],
        serde_json::json!({"text": "goal is slow", "kind": "note", "by": "human"})
    );
    let on_task = ok_as(
        "planner",
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
    assert_eq!(on_task["payload"]["by"], "planner");
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
    assert_eq!(shown["observations"][0]["by"], "planner");
    let goal = ok(&db, &["goal", "show", "1"]);
    assert_eq!(
        goal["observations"],
        serde_json::json!([{"id": on_goal["id"], "created_at": on_goal["created_at"],
                            "text": "goal is slow", "kind": "note", "by": "human"}])
    );
}

#[test]
fn observer_may_record_findings_and_ask_but_not_change_queue_state() {
    let (_dir, db) = queue();
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
        // Notes, draft goals and their tasks are no longer the observer's
        // (ADR-0044 decision 4); a finding carries what it sees.
        &["goal", "add", "proposal", "--draft"],
        &["note", "--goal", "1", "--text", "seen"],
        &["finding", "dismiss", "1", "--reason", "x"],
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

    // Reads, findings and blocked asks are allowed.
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
        &["findings", "--all", "--full"],
    ] {
        ok_as("observer", &db, args);
    }
    let finding = ok_as(
        "observer",
        &db,
        &[
            "finding",
            "record",
            "--kind",
            "stall",
            "--queue",
            "--summary",
            "slots idle",
        ],
    );
    assert_eq!(finding["recorded_by"], "observer");
    // A threshold crossing goes to the inbox as a blocked ask on its
    // finding, on a task or on nothing; registering the same one again
    // returns the open ask. Without a finding it is refused.
    denied(&[
        "ask",
        "--kind",
        "blocked",
        "--because",
        "scope",
        "--question",
        "no finding",
    ]);
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
            "--finding",
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
            "--finding",
            "1",
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
            "--finding",
            "1",
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
    ok_as(
        "observer",
        &db,
        &[
            "finding",
            "resolve",
            "1",
            "--reason",
            "slots are busy again",
        ],
    );
    // Other roles are not restricted.
    ok_as("planner", &db, &["add", "planned"]);
}

/// A finding is one row per problem (ADR-0044 decision 18): recording the
/// same kind, target and subject again adds the new evidence as one more
/// occurrence, and a blocked ask raises it with one open ask per finding
/// (decision 23).
#[test]
fn findings_are_recorded_once_per_problem_and_listed_by_impact() {
    let (_dir, db) = queue();
    ok(&db, &["goal", "add", "g"]);
    ok(&db, &["add", "t", "--goal", "1"]);
    ok(&db, &["add", "u"]);
    let events: Vec<String> = ok(&db, &["events", "--all"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].to_string())
        .collect();
    assert!(events.len() >= 3, "{events:?}");
    let record = |extra: &[&str]| {
        let mut args = vec![
            "finding",
            "record",
            "--kind",
            "stall",
            "--queue",
            "--subject",
            "idle_slots",
            "--summary",
            "slots idle",
        ];
        args.extend_from_slice(extra);
        ok_as("observer", &db, &args)
    };
    let first = record(&["--evidence", &events[0]]);
    assert_eq!(first["created"], true);
    assert_eq!(
        (
            &first["target"],
            &first["status"],
            &first["impact"],
            &first["occurrences"]
        ),
        (
            &Value::from("queue"),
            &Value::from("open"),
            &Value::from("normal"),
            &Value::from(1)
        )
    );
    let id = first["id"].to_string();
    // New evidence is one more occurrence of the same finding.
    let second = record(&["--evidence", &events[0], "--evidence", &events[1]]);
    assert_eq!(
        (&second["id"], &second["created"]),
        (&first["id"], &Value::Bool(false))
    );
    assert_eq!(second["occurrences"], 2);
    assert_eq!(
        second["evidence"],
        serde_json::json!([first["evidence"][0], second["evidence"][1]])
    );
    assert_eq!(second["evidence"].as_array().unwrap().len(), 2);
    // Nothing new: nothing written.
    let same = record(&["--evidence", &events[1]]);
    assert_eq!(same["changed"], serde_json::json!([]));
    assert_eq!(same["occurrences"], 2);
    // A different subject or target is another finding.
    let task = ok_as(
        "observer",
        &db,
        &[
            "finding",
            "record",
            "--kind",
            "failure",
            "--task",
            "1",
            "--summary",
            "fails twice",
            "--impact",
            "high",
            "--propose",
            "recurs",
        ],
    );
    assert_eq!(task["created"], true);
    assert_eq!(task["task_id"], 1);
    assert_eq!(task["propose_reason"], "recurs");
    let task_id = task["id"].to_string();
    let listed = ok(&db, &["findings"])["findings"].clone();
    assert_eq!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["id"].clone())
            .collect::<Vec<_>>(),
        vec![task["id"].clone(), first["id"].clone()],
        "high impact first"
    );
    assert_eq!(listed[0]["proposal_status"], Value::Null);
    assert!(listed[0].get("evidence_events").is_none());
    assert_eq!(
        ok(&db, &["findings", "--task", "1"])["findings"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        ok(&db, &["findings", "--queue"])["findings"][0]["id"],
        first["id"]
    );
    assert_eq!(
        ok(&db, &["findings", "--kind", "wait"])["findings"],
        serde_json::json!([])
    );
    let full = ok(&db, &["findings", &id, "--full"])["findings"][0].clone();
    assert_eq!(full["evidence_events"].as_array().unwrap().len(), 2);
    assert_eq!(full["evidence_events"][0]["id"], first["evidence"][0]);
    // Its own events ride on the queue (no task) or on its target.
    let kinds: Vec<(Value, Value)> = ok(&db, &["events", "--all"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"].as_str().unwrap().starts_with("finding_"))
        .map(|e| (e["kind"].clone(), e["task_id"].clone()))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (Value::from("finding_recorded"), Value::Null),
            (Value::from("finding_updated"), Value::Null),
            (Value::from("finding_recorded"), Value::from(1)),
        ]
    );

    // A task-less blocked ask per finding (ADR-0044 decision 23).
    let ask = |finding: &str| {
        ok_as(
            "observer",
            &db,
            &[
                "ask",
                "--kind",
                "blocked",
                "--question",
                "q",
                "--because",
                "scope",
                "--finding",
                finding,
            ],
        )
    };
    let on_first = ask(&id);
    let on_task = ask(&task_id);
    assert_eq!(
        (&on_first["created"], &on_task["created"]),
        (&Value::Bool(true), &Value::Bool(true))
    );
    assert_eq!(on_first["finding_id"], first["id"]);
    assert_eq!(ask(&id)["id"], on_first["id"]);
    assert_eq!(
        ok(&db, &["findings", &id])["findings"][0]["open_asks"],
        serde_json::json!([on_first["id"]])
    );
    let error = |role: &str, args: &[&str]| {
        let output = invoke_as(Some(role), &db, args);
        assert!(!output.status.success(), "{args:?}");
        serde_json::from_slice::<Value>(&output.stderr).unwrap()["error"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(
        error(
            "planner",
            &[
                "ask",
                "--kind",
                "decide",
                "--task",
                "1",
                "--question",
                "q",
                "--because",
                "scope",
                "--finding",
                &id
            ]
        ),
        "only a blocked ask may name a finding, not decide"
    );
    assert_eq!(
        error(
            "observer",
            &[
                "ask",
                "--kind",
                "blocked",
                "--question",
                "q",
                "--because",
                "scope",
                "--finding",
                "99"
            ]
        ),
        "finding 99 does not exist"
    );
    assert_eq!(
        error(
            "observer",
            &[
                "finding",
                "record",
                "--kind",
                "x",
                "--queue",
                "--summary",
                "s",
                "--evidence",
                "9999"
            ]
        ),
        "event 9999 does not exist"
    );
    assert_eq!(
        error(
            "observer",
            &[
                "finding",
                "record",
                "--kind",
                "Bad",
                "--queue",
                "--summary",
                "s"
            ]
        ),
        "finding kind \"Bad\" must be a slug of lowercase letters, digits, '-' and '_'"
    );

    // Resolved, it leaves the default list; occurring again reopens it.
    let resolved = ok_as(
        "observer",
        &db,
        &["finding", "resolve", &id, "--reason", "busy again"],
    );
    assert_eq!(
        (&resolved["status"], &resolved["status_reason"]),
        (&Value::from("resolved"), &Value::from("busy again"))
    );
    assert_eq!(
        ok(&db, &["findings"])["findings"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        ok(&db, &["findings", "--status", "resolved"])["findings"][0]["id"],
        first["id"]
    );
    let again = record(&["--evidence", &events[2]]);
    assert_eq!(
        (&again["id"], &again["status"], &again["occurrences"]),
        (&first["id"], &Value::from("open"), &Value::from(3))
    );
    // Dismissed by a person, it only counts what recurs.
    let dismissed = ok(
        &db,
        &[
            "finding",
            "dismiss",
            &task_id,
            "--reason",
            "task 2 covers it",
        ],
    );
    assert_eq!(dismissed["status"], "dismissed");
    assert_eq!(
        error(
            "planner",
            &["finding", "dismiss", &task_id, "--reason", "again"]
        ),
        format!("finding {task_id} is dismissed; it cannot become dismissed")
    );
    assert_eq!(
        error(
            "planner",
            &["finding", "resolve", &task_id, "--reason", "x"]
        ),
        format!("finding {task_id} is dismissed; it cannot become resolved")
    );
    assert_eq!(
        ok(&db, &["findings", "--all"])["findings"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let counted = ok_as(
        "observer",
        &db,
        &[
            "finding",
            "record",
            "--kind",
            "failure",
            "--task",
            "1",
            "--summary",
            "fails",
            "--evidence",
            &events[0],
        ],
    );
    assert_eq!(
        (
            &counted["status"],
            &counted["occurrences"],
            &counted["summary"]
        ),
        (
            &Value::from("dismissed"),
            &Value::from(2),
            &Value::from("fails twice")
        )
    );
    // The headless reviewer reads findings but writes none.
    ok_as("reviewer", &db, &["findings"]);
    assert_eq!(
        error("reviewer", &["finding", "resolve", &id, "--reason", "x"]),
        "reviewer may not change queue state"
    );
}

/// The supervisor's headless review runs under `DAGQ_ROLE=reviewer`
/// (ADR-0027): it may read the queue and nothing else.
#[test]
fn reviewer_may_only_read_the_queue() {
    let (_dir, db) = queue();
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
