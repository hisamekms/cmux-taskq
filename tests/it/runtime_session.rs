//! Runtime tests: The session of a run: its receipt, the validation of the receipt, the
//! exit request and the dialogs on the worker's screen.
use crate::common;
use crate::runtime_support;

use runtime_support::*;

/// A stub agent and the children it started die with the test's fixture
/// when the test ends while the stub still runs, here by a panic, and the
/// stub holds none of the test process's streams (task 317). To check a
/// whole run by hand: after `cargo test --locked`,
/// `ps -axo pid,ppid,command | grep -e 'test -f seed.txt' -e await_exit`
/// lists no stub.
#[test]
fn a_stub_agent_dies_with_its_fixture_and_holds_no_stream_of_the_test() {
    let out = tempfile::tempdir().unwrap();
    let (pids, streams) = (out.path().join("pids"), out.path().join("streams"));
    let (stub_pid, db) = {
        let (pids, streams) = (pids.clone(), streams.clone());
        let stub = Arc::new(Mutex::new(None));
        let started = stub.clone();
        let panicked = std::panic::catch_unwind(move || {
            let (_dir, _repo, db) = fixture();
            let mut spec = CommandSpec::new("/bin/sh");
            spec.env("PIDS", &pids)
                .env("STREAMS", &streams)
                .arg("-c")
                .arg(concat!(
                    watchdog!(),
                    r#"
for fd in 0 1 2; do [ /dev/fd/$fd -ef /dev/null ] && printf '%s ' null >> "$STREAMS.tmp"; done
mv "$STREAMS.tmp" "$STREAMS"
sleep 300 &
printf '%s %s\n' $$ $! > "$PIDS.tmp"
mv "$PIDS.tmp" "$PIDS"
while :; do sleep 0.05; done
"#
                ));
            let child = StubSpawner { db: db.clone() }
                .spawn(&spec, Streams::Inherit)
                .unwrap();
            *started.lock().unwrap() = Some((child.id(), db));
            let begun = Instant::now();
            while !pids.exists() {
                assert!(begun.elapsed() < Duration::from_secs(30));
                thread::sleep(Duration::from_millis(20));
            }
            panic!("the test fails while its stub runs");
        });
        assert!(panicked.is_err());
        stub.lock().unwrap().take().unwrap()
    };
    assert_eq!(fs::read_to_string(&streams).unwrap(), "null null null ");
    let text = fs::read_to_string(&pids).unwrap();
    let (shell, sleep) = text.trim().split_once(' ').unwrap();
    assert_eq!(shell, stub_pid.to_string());
    let sleep: u32 = sleep.parse().unwrap();
    // The shell is this process's child: reaped here once killed.
    let begun = Instant::now();
    loop {
        // SAFETY: waitpid(2) with a null status pointer writes nothing.
        let reaped =
            unsafe { libc::waitpid(stub_pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
        if reaped == stub_pid as libc::pid_t {
            break;
        }
        assert!(
            begun.elapsed() < Duration::from_secs(10),
            "stub {stub_pid} lives on"
        );
        thread::sleep(Duration::from_millis(20));
    }
    while running(sleep) {
        assert!(
            begun.elapsed() < Duration::from_secs(10),
            "sleep {sleep} lives on"
        );
        thread::sleep(Duration::from_millis(20));
    }
    // The fixture's queue starts no more stubs.
    let error = StubSpawner { db }
        .spawn(
            CommandSpec::new("/bin/sh").arg("-c").arg("exit 0"),
            Streams::Inherit,
        )
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "the test's fixture is gone");
}

/// While the agent works, a later binary applies a compatible migration
/// (ADR-0045 decision 6): the run's wrapper, the supervisor and the CLI the
/// worker runs are then older than the queue, and carry the run to its
/// receipt anyway.
#[test]
fn a_compatible_migration_during_a_run_leaves_the_run_working() {
    let newer = SqliteQueue::SCHEMA_VERSION + 1;
    let script = format!(
        "sqlite3 -cmd '.timeout 5000' \"$DB\" \\
           'ALTER TABLE task_runs ADD COLUMN future_hint TEXT;
            CREATE TABLE future_things (id INTEGER PRIMARY KEY);
            PRAGMA user_version = {newer};' || exit 97
         \"$DAGQ\" --db \"$DB\" show 1 > /dev/null || exit 98
         {VALID_AGENT}"
    );
    let (_dir, db, detail) = run_agent(&script);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(run.last_error().is_none());
    assert!(
        detail
            .processes
            .iter()
            .all(|p| p.exited_at.is_some() && p.exit_code == Some(0))
    );
    assert_eq!(
        SqliteQueue::open(&db).unwrap().schema_version().unwrap(),
        newer
    );
}

#[test]
fn valid_receipt_is_verified_and_awaits_integration() {
    let (_dir, db, detail) = run_agent(VALID_AGENT);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(run.last_error().is_none());
    let commit = run.result_commit().unwrap();
    assert_ne!(commit, run.base_commit());
    let head = Command::new("git")
        .arg("-C")
        .arg(run.worktree_path().unwrap())
        .args(["rev-parse", "HEAD"])
        .bounded_output()
        .unwrap();
    assert_eq!(String::from_utf8(head.stdout).unwrap().trim(), commit);
    assert_eq!(
        fs::read_to_string(run.log_path().unwrap()).unwrap(),
        "fixture log\n"
    );
    assert!(Path::new(run.run_dir().unwrap()).join("runner").exists());
    assert_eq!(detail.processes.len(), 2);
    assert!(
        detail
            .processes
            .iter()
            .all(|p| p.exited_at.is_some() && p.exit_code == Some(0))
    );
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"agent_started"));
    assert!(kinds.contains(&"receipt_observed"));
    // The only task has no goal, no context, no predecessor, and nothing else was in progress.
    let prompt = read_prompt(run);
    assert!(
        prompt.contains("Goal: none, this task stands alone\n"),
        "{prompt}"
    );
    assert!(prompt.contains("Context: none\n"), "{prompt}");
    assert!(prompt.contains("Predecessor tasks: none\n"), "{prompt}");
    assert!(
        prompt.contains("Sibling tasks in progress: none\n"),
        "{prompt}"
    );
    // Validation checks only the receipt: the verification commands wait
    // for integrate's rebase (ADR-0023 decision 1).
    assert!(!kinds.contains(&"verification_command"), "{kinds:?}");
    assert!(
        !Path::new(run.run_dir().unwrap())
            .join("verify-1.log")
            .exists()
    );
    let finished = detail
        .events
        .iter()
        .find(|e| e.kind == "validation_finished")
        .unwrap();
    assert_eq!(finished.payload["status"], "awaiting_integration");
    assert_eq!(finished.payload["result_commit"], json!(commit));
    assert_eq!(
        finished.payload["receipt"]["e2e"]["status"],
        "not_applicable"
    );
    // The workspace is closed only after validation succeeded; the branch stays.
    let closed = detail
        .events
        .iter()
        .find(|e| e.kind == "workspace_closed")
        .unwrap();
    assert!(closed.id > finished.id);
    assert_eq!(closed.payload["workspace_id"], WORKSPACE_ID);
    assert_eq!(
        closed.payload["closed_at"],
        json!(run.workspace_closed_at().unwrap())
    );
    let branch = Command::new("git")
        .arg("-C")
        .arg(run.worktree_path().unwrap())
        .args(["symbolic-ref", "HEAD"])
        .bounded_output()
        .unwrap();
    assert_eq!(
        String::from_utf8(branch.stdout).unwrap().trim(),
        format!("refs/heads/{}", run.branch().unwrap())
    );
    // The task still owns its slot until integration; no second run starts.
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(queue.show(TaskId::new(1)).unwrap().runs.len(), 1);
    assert!(queue.candidates().unwrap().is_empty());
}

#[test]
fn failed_workspace_close_is_recorded_without_changing_run_status() {
    let (_dir, _db, detail) = run_agent_with(VALID_AGENT, true);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(run.result_commit().is_some());
    let error = run.last_error().unwrap();
    assert!(
        error.contains("injected workspace close failure"),
        "{error}"
    );
    assert!(error.contains(WORKSPACE_ID));
    let failed = detail
        .events
        .iter()
        .find(|e| e.kind == "cleanup_failed")
        .unwrap();
    assert_eq!(failed.payload["workspace_id"], WORKSPACE_ID);
    assert_eq!(failed.payload["message"], json!(error));
    // Validation itself was accepted; the failure is confined to cleanup.
    let finished = detail
        .events
        .iter()
        .find(|e| e.kind == "validation_finished")
        .unwrap();
    assert_eq!(finished.payload["accepted"], true);
    assert!(failed.id > finished.id);
}

#[test]
fn missing_receipt_fails_validation() {
    let (_dir, _db, detail) = run_agent("commit work");
    assert!(rejection_reason(&detail).contains("receipt was not submitted"));
    assert!(detail.runs[0].result_commit().is_none());
    assert!(
        !detail
            .events
            .iter()
            .any(|e| e.kind == "verification_command")
    );
}

#[test]
fn run_id_mismatch_fails_validation() {
    let (_dir, _db, detail) = run_agent("commit work; receipt \"$(git rev-parse HEAD)\" other-run");
    assert!(rejection_reason(&detail).contains("run_id other-run does not match"));
    assert!(detail.runs[0].result_commit().is_none());
}

#[test]
fn receipt_without_new_commit_fails_validation() {
    let (_dir, _db, detail) = run_agent("receipt \"$BASE\"");
    assert!(rejection_reason(&detail).contains("no commit was made"));
}

#[test]
fn receipt_commit_that_is_not_branch_head_fails_validation() {
    let (_dir, _db, detail) = run_agent("commit work; receipt \"$BASE\"");
    assert!(rejection_reason(&detail).contains("is not the head of dagq/"));
}

#[test]
fn dirty_worktree_fails_validation() {
    let (_dir, _db, detail) = run_agent(
        "commit work; printf 'scratch\n' > untracked.txt; receipt \"$(git rev-parse HEAD)\"",
    );
    let reason = rejection_reason(&detail);
    assert!(reason.contains("worktree is not clean"));
    assert!(reason.contains("untracked.txt"));
    // The verified commit is still recorded for inspection.
    assert!(detail.runs[0].result_commit().is_some());
}

#[test]
fn receipt_structure_is_checked_before_git() {
    use dagq::domain::Receipt;
    let valid = r#"{"run_id":"r","result":"succeeded","commit":"0123456789abcdef0123456789abcdef01234567",
        "tests":{"status":"passed","evidence_or_reason":"cargo test"},
        "e2e":{"status":"not_applicable","evidence_or_reason":"library only"},
        "subagent_review":{"status":"passed","evidence_or_reason":"no findings"},"summary":"ok"}"#;
    Receipt::parse(valid)
        .unwrap()
        .check(&RunId::new("r").unwrap())
        .unwrap();
    let cases = [
        (valid.replace("\"r\"", "\"other\""), "does not match"),
        (
            valid.replace("succeeded", "failed"),
            "agent reported result failed",
        ),
        (
            valid.replace("library only", " "),
            "e2e is not_applicable without evidence or reason",
        ),
        (
            valid.replace(
                "\"passed\",\"evidence_or_reason\":\"no findings\"",
                "\"failed\",\"evidence_or_reason\":\"bug\"",
            ),
            "subagent_review as failed: bug",
        ),
        (
            valid.replace("0123456789abcdef0123456789abcdef01234567", "0123456"),
            "receipt commit",
        ),
    ];
    for (text, expected) in cases {
        let error = format!(
            "{:#}",
            Receipt::parse(&text)
                .unwrap()
                .check(&RunId::new("r").unwrap())
                .unwrap_err()
        );
        assert!(error.contains(expected), "{error}");
    }
    assert!(Receipt::parse("{\"run_id\":\"r\"}").is_err());
    assert!(Receipt::parse(&valid.replace("passed", "maybe")).is_err());
    // follow_ups is optional and only its shape is checked.
    let without = Receipt::parse(valid).unwrap();
    assert!(without.follow_ups.is_none());
    assert!(
        !serde_json::to_string(&without)
            .unwrap()
            .contains("follow_ups")
    );
    let with = valid.replace(
        "\"summary\":\"ok\"",
        "\"summary\":\"ok\",\"follow_ups\":[{\"title\":\"next\",\"description\":\"later\"}]",
    );
    let receipt = Receipt::parse(&with).unwrap();
    receipt.check(&RunId::new("r").unwrap()).unwrap();
    assert_eq!(receipt.follow_ups.as_ref().unwrap()[0]["title"], "next");
    assert_eq!(
        serde_json::to_value(&receipt).unwrap()["follow_ups"][0]["description"],
        "later"
    );
    Receipt::parse(&valid.replace("\"summary\":\"ok\"", "\"summary\":\"ok\",\"follow_ups\":[]"))
        .unwrap()
        .check(&RunId::new("r").unwrap())
        .unwrap();
    let error = format!(
        "{:#}",
        Receipt::parse(&valid.replace(
            "\"summary\":\"ok\"",
            "\"summary\":\"ok\",\"follow_ups\":{\"title\":\"next\"}"
        ))
        .unwrap()
        .check(&RunId::new("r").unwrap())
        .unwrap_err()
    );
    assert!(error.contains("follow_ups must be an array"), "{error}");
}

#[test]
fn idle_marker_after_receipt_triggers_exit_request_and_run_finishes() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_leases().unwrap().is_empty());
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("receipt_observed") < position("session_idle_observed"));
    assert!(position("session_idle_observed") < position("exit_requested"));
    assert!(position("exit_requested") < position("session_exited"));
    assert!(!kinds.contains(&"exit_request_timed_out"));
    // The idle evidence names the hook and session that produced it.
    let idle = detail
        .events
        .iter()
        .find(|e| e.kind == "session_idle_observed")
        .unwrap();
    assert_eq!(idle.payload["hook_event_name"], "Stop");
    assert_eq!(idle.payload["session_id"], json!(run.id()));
    assert_eq!(
        idle.payload["marker_path"],
        json!(run.idle_marker_path().unwrap())
    );
    assert!(
        idle.payload["marker_modified"].as_i64().unwrap()
            >= idle.payload["receipt_modified"].as_i64().unwrap()
    );
    let requested = detail
        .events
        .iter()
        .find(|e| e.kind == "exit_requested")
        .unwrap();
    assert_eq!(requested.payload["workspace_id"], WORKSPACE_ID);
    assert_eq!(requested.payload["timeout_secs"], 120);
}

/// The session exits, and its wrapper records `session_exited`, before
/// `send_exit` returns (a slow `cmux send` under load): `exit_requested` is
/// still recorded first, so the events read in causal order.
#[test]
fn exit_requested_precedes_a_session_exit_that_beats_the_send() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    backend.exit_returns_after_session = true;
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(
        position("exit_requested") < position("session_exited"),
        "{kinds:?}"
    );
}

#[test]
fn missing_or_stale_idle_marker_does_not_request_exit() {
    // No marker at all, then a marker older than the receipt (an earlier turn).
    // Both sessions end by themselves, as with a person's /exit.
    for script in [
        "commit work; receipt \"$(git rev-parse HEAD)\"; sleep 1",
        "idle; touch -t 200001010000 \"$IDLE\"; commit work; receipt \"$(git rev-parse HEAD)\"; sleep 1",
    ] {
        let (_dir, repo, db) = fixture();
        let backend = TestWorkspace::new(&db, false, script);
        let outcome = supervise(&db, &repo, &backend).unwrap();
        backend.join();
        assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
        assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
        let mut queue = SqliteQueue::open(&db).unwrap();
        let detail = queue.show(TaskId::new(1)).unwrap();
        let kinds = event_kinds(&detail);
        assert!(kinds.contains(&"receipt_observed"));
        assert!(!kinds.contains(&"session_idle_observed"));
        assert!(!kinds.contains(&"exit_requested"));
    }
}

/// A dialog screen as Claude Code draws it.
const DIALOG_SCREEN: &str = "\
 Auto mode is available

 ❯ 1. Yes, turn on auto mode
   2. No, keep asking

 Esc to cancel
";

/// Claude Code's confirmation of a `/exit` while background work runs.
const BACKGROUND_WORK_SCREEN: &str = "\
╭──────────────────────────────────────────────────────────────╮
│ Background work is running                                   │
│                                                              │
│ 1 background task is still running:                          │
│   · sleep 600 (shell)                                        │
│                                                              │
│ ❯ 1. Exit and stop tasks                                     │
│   2. Move to background and exit                             │
│   3. Stay                                                    │
╰──────────────────────────────────────────────────────────────╯
   Enter to confirm · Esc to cancel
";

/// The Settings panel `/status` leaves open over the input box.
const SETTINGS_SCREEN: &str = "\
────────────────────────────────────────────────────────────────
 Settings:  Status   Config   Usage   (←/→ or tab to cycle)

 Current session
 ███████████▌                               23% used

 Esc to exit
";

/// Commits once, waits for `$EXIT.go` before a second commit and the receipt.
const TWO_COMMIT_AGENT: &str = "commit first; while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; printf 'more\\n' >> change.txt; git commit -q -am second; receipt \"$(git rev-parse HEAD)\"";

/// The supervisor records `first_commit_observed` once, while the session
/// still works, when the worktree's HEAD first leaves the base commit; a
/// later commit records nothing more. It sits between `agent_started` and
/// `receipt_observed`, which is what `stats` reads as `startup`.
#[test]
fn the_first_commit_is_observed_once_while_the_session_works() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, TWO_COMMIT_AGENT));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    let observed = |queue: &mut SqliteQueue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap())
            .iter()
            .filter(|k| **k == "first_commit_observed")
            .count()
    };
    wait_until(&db, Duration::from_secs(30), |queue| observed(queue) == 1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let first = git_out(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(first, *run.base_commit());
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(!event_kinds(&detail).contains(&"receipt_observed"));

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");

    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(observed(&mut queue), 1, "{kinds:?}");
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("agent_started") < position("first_commit_observed"));
    assert!(position("first_commit_observed") < position("receipt_observed"));
    let payload = events_of(&db, run.id(), "first_commit_observed").remove(0);
    assert_eq!(payload["commit"], first.as_str());
    assert_eq!(payload["base_commit"], run.base_commit().as_str());
    let head = queue.show(TaskId::new(1)).unwrap().runs[0]
        .result_commit()
        .cloned()
        .unwrap();
    assert_ne!(head, first, "the second commit is the result");
}

/// A session that runs past `prompt_wait` has its screen read: an ordinary
/// screen records nothing, a dialog is recorded as `prompt_waiting` once and
/// raised to the inbox as an `answer_prompt` ask with the screen's excerpt
/// (ADR-0024's Consequences), and the screen going back to work records
/// `prompt_cleared` and closes the ask. No key is sent.
#[test]
fn a_dialog_on_the_screen_is_asked_once_and_cleared() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, PROMPTED_AGENT);
    backend.prompt_wait = Duration::from_millis(300);
    *backend.screen.lock().unwrap() = WORK_SCREEN.into();
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    let prompts = |queue: &mut SqliteQueue, kind: &str| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap())
            .iter()
            .filter(|k| **k == kind)
            .count()
    };
    // Ordinary work is read but not recorded.
    let started = Instant::now();
    while backend.captures.load(Ordering::SeqCst) < 2 {
        assert!(started.elapsed() < Duration::from_secs(30));
        thread::sleep(Duration::from_millis(20));
    }
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(prompts(&mut queue, "prompt_waiting"), 0);
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert!(run_attention_of(&runtime::status(&db).unwrap(), run.id()).is_none());

    *backend.screen.lock().unwrap() = DIALOG_SCREEN.into();
    wait_until(&db, Duration::from_secs(30), |queue| {
        prompts(queue, "prompt_waiting") == 1
    });
    // The same screen is read again but not recorded again.
    let captured = backend.captures.load(Ordering::SeqCst);
    let started = Instant::now();
    while backend.captures.load(Ordering::SeqCst) < captured + 2 {
        assert!(started.elapsed() < Duration::from_secs(30));
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(prompts(&mut queue, "prompt_waiting"), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let waiting = detail
        .events
        .iter()
        .find(|e| e.kind == "prompt_waiting")
        .unwrap();
    assert_eq!(waiting.payload["workspace_id"], WORKSPACE_ID);
    assert_eq!(waiting.payload["prompt"], "choice");
    assert_eq!(
        waiting.payload["excerpt"],
        "Auto mode is available\n ❯ 1. Yes, turn on auto mode\n   2. No, keep asking\n Esc to cancel"
    );
    assert_eq!(waiting.payload["screen_hash"].as_str().unwrap().len(), 64);
    // The dialog is an ask for the inbox, not an attention of the run.
    let asks = queue
        .asks(dagq::infrastructure::asks::AskQuery {
            open: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = &asks[0];
    assert_eq!(ask.kind, dagq::domain::AskKind::AnswerPrompt);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.task_id, Some(run.task_id()));
    assert_eq!(ask.asked_by, "supervisor");
    assert!(ask.options.is_empty());
    assert!(
        ask.question.contains(&format!(
            "waits at a choice dialog in workspace {WORKSPACE_ID}"
        )),
        "{}",
        ask.question
    );
    assert!(
        ask.question
            .ends_with("Auto mode is available\n ❯ 1. Yes, turn on auto mode\n   2. No, keep asking\n Esc to cancel"),
        "{}",
        ask.question
    );
    let status = runtime::status_for(&db, Some(dagq::domain::SessionRole::Inbox)).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "ask_opened" && a["ask_id"] == ask.id.as_i64())
    );
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["kind"] != "prompt_waiting")
    );
    // The same dialog is not asked about twice.
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);

    // Someone answers the dialog: the screen goes back to work.
    *backend.screen.lock().unwrap() = WORK_SCREEN.into();
    // `prompt_cleared` is recorded before the ask is closed: both are
    // waited for.
    wait_until(&db, Duration::from_secs(30), |queue| {
        prompts(queue, "prompt_cleared") == 1 && queue.read_ask(ask.id).unwrap().closed_at.is_some()
    });
    assert!(run_attention_of(&runtime::status(&db).unwrap(), run.id()).is_none());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    // The runtime closed the ask: it is no attention, and its answer says why.
    let closed = queue.read_ask(ask.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(
        closed.answer.as_deref(),
        Some("the dialog is gone; closed by the runtime")
    );
    assert!(
        runtime::status(&db).unwrap()["attention"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a.get("ask_id").is_none())
    );

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(
        event_kinds(&detail)
            .iter()
            .filter(|k| k.starts_with("prompt_"))
            .count(),
        2
    );
}

/// ADR-0047 decisions 39 and 40: a dialog the runtime does not answer is
/// the `prompt_waiting` alert's recovery job's first. Its `wait` is applied
/// (no ask while it holds); once it is over and the dialog is still there,
/// another job runs, and its escalation is the `answer_prompt` ask with the
/// job's diagnosis, options and reason category, closed once the dialog
/// is gone.
#[test]
fn a_dialog_goes_to_its_recovery_job_before_the_inbox() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, PROMPTED_AGENT);
    backend.prompt_wait = Duration::from_millis(300);
    *backend.screen.lock().unwrap() = DIALOG_SCREEN.into();
    let backend = Arc::new(backend);
    let reviewer = Arc::new(
        TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]).with_triages(&[
            repair(
                json!({"action": "wait", "recheck_after_secs": 1}),
                "the dialog may go by itself",
            ),
            recovery(json!({
                "verdict": "escalate",
                "confidence": "high",
                "diagnosis": "an auto mode offer only a person may accept",
                "question": "Turn auto mode on?",
                "options": ["2"],
                "reason_category": "scope",
            })),
        ]),
    );
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || {
            let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &supervise_options(4, true),
            )
        })
    };
    let open_asks = |queue: &SqliteQueue| {
        queue
            .asks(dagq::infrastructure::asks::AskQuery {
                open: true,
                ..Default::default()
            })
            .unwrap()
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !payloads(&queue.show(TaskId::new(1)).unwrap(), "recovery_finished").is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished[0]["alert"], "prompt_waiting");
    assert_eq!(finished[0]["applied"], json!(["wait"]));
    assert!(finished[0]["recheck_at_ms"].as_i64().is_some());
    assert!(open_asks(&queue).is_empty());

    // The ask opens before the job's `recovery_finished` is recorded: both
    // are waited for.
    wait_until(&db, Duration::from_secs(30), |queue| {
        !open_asks(queue).is_empty()
            && payloads(&queue.show(TaskId::new(1)).unwrap(), "recovery_finished").len() == 2
    });
    let ask = open_asks(&queue).remove(0);
    assert_eq!(ask.kind, dagq::domain::AskKind::AnswerPrompt);
    assert_eq!(ask.options, ["2"]);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::Scope);
    for part in [
        "waits at a choice dialog",
        "Its recovery job looked first, and the recovery job could not repair it",
        "Diagnosis: an auto mode offer only a person may accept",
        "Question: Turn auto mode on?",
        "recovery-prompt_waiting-2.prompt.txt",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    let detail = queue.show(TaskId::new(1)).unwrap();
    let waiting = detail
        .events
        .iter()
        .find(|e| e.kind == "prompt_waiting")
        .unwrap();
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 2, "{requested:?}");
    assert!(requested.iter().all(|p| p["alert"] == "prompt_waiting"));
    assert_eq!(requested[0]["evidence"], json!([waiting.id]));
    assert_eq!(requested[0]["prompt"], "choice");
    assert_eq!(requested[1]["attempt"], 2);
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished[1]["escalated"], true);
    assert_eq!(finished[1]["ask_id"], json!(ask.id));
    assert!(payloads(&detail, "auto_repaired").is_empty());
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);
    assert_eq!(reviewer.triage_prompts().len(), 2);

    // Someone answers the dialog: the ask closes, and the run goes on.
    *backend.screen.lock().unwrap() = WORK_SCREEN.into();
    // `prompt_cleared` is recorded before the ask is closed: both are
    // waited for.
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"prompt_cleared")
            && queue.read_ask(ask.id).unwrap().closed_at.is_some()
    });
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
}

/// Two sessions that stop at the same login that ran out are one
/// `authentication` ask for the inbox (ADR-0047 decision 42): the first
/// opens it with one notification, the second joins its `affected`, each
/// records `auth_required`, and neither is an `answer_prompt` ask. `status`
/// and `watch` show the reason.
#[test]
fn sessions_stopped_at_the_same_login_share_one_authentication_ask() {
    let (_dir, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "second task", &[]);
    }
    let mut backend = TestWorkspace::new(&db, false, PROMPTED_AGENT);
    backend.prompt_wait = Duration::from_millis(300);
    *backend.screen.lock().unwrap() = LOGIN_SCREEN.into();
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    let auth_events = |queue: &mut SqliteQueue| {
        [TaskId::new(1), TaskId::new(2)]
            .iter()
            .map(|id| {
                event_kinds(&queue.show(*id).unwrap())
                    .iter()
                    .filter(|k| **k == "auth_required")
                    .count()
            })
            .collect::<Vec<_>>()
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        auth_events(queue) == [1, 1]
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let open = queue
        .asks(dagq::infrastructure::asks::AskQuery {
            open: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    let ask = &open[0];
    assert_eq!(ask.kind, dagq::domain::AskKind::QueueHold);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::Authentication);
    assert_eq!((ask.task_id, ask.run_id.as_ref()), (None, None));
    let runs: Vec<String> = [TaskId::new(1), TaskId::new(2)]
        .iter()
        .map(|id| queue.show(*id).unwrap().runs[0].id().as_str().to_owned())
        .collect();
    let mut affected = ask.affected.clone();
    affected.sort();
    let mut expected = runs.clone();
    expected.sort();
    assert_eq!(affected, expected);
    for run in &runs {
        assert!(ask.question.contains(run.as_str()), "{}", ask.question);
    }
    assert_eq!(ask.options, ["done", "cancel_affected"]);
    // One notification for both runs.
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);
    let status = runtime::status_for(&db, Some(dagq::domain::SessionRole::Inbox)).unwrap();
    let entry = status["asks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == ask.id.as_i64())
        .unwrap()
        .clone();
    assert_eq!(entry["reason_category"], "authentication");
    assert_eq!(entry["affected"].as_array().unwrap().len(), 2);
    let attention = ask_attention(&status, ask.id);
    assert_eq!(attention.len(), 1, "{status}");
    assert_eq!(attention[0]["reason_category"], "authentication");
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    let opened: Vec<&Value> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "ask_opened")
        .collect();
    assert_eq!(opened.len(), 1, "{events}");
    assert_eq!(opened[0]["reason_category"], "authentication");
    // A login is no dialog to answer.
    for id in [TaskId::new(1), TaskId::new(2)] {
        assert!(
            !event_kinds(&queue.show(id).unwrap()).contains(&"prompt_waiting"),
            "{id}"
        );
    }

    // The person logs in; the sessions go back to work and finish.
    *backend.screen.lock().unwrap() = WORK_SCREEN.into();
    let answered = queue.answer(ask.id, "done").unwrap();
    assert_eq!(
        answered.reason_category,
        dagq::domain::AskReason::Authentication
    );
    for id in [TaskId::new(1), TaskId::new(2)] {
        let run = queue.show(id).unwrap().runs[0].clone();
        fs::write(
            exit_request_path(run.run_dir().unwrap()).with_extension("go"),
            "",
        )
        .unwrap();
    }
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(auth_events(&mut queue), [1, 1]);
}

/// A worker that registers a `worker_question` ask, goes idle once
/// `$EXIT.idle` exists and then waits for the answer in `$MESSAGE` (the
/// test backend's terminal); it commits the answer it got.
const ASKING_AGENT: &str = r#"
"$DAGQ" --db "$DB" ask --run "$RUN_ID" --kind worker_question --because scope --question 'Which word?' --cmux /usr/bin/true > /dev/null || exit 70
while [ ! -f "$EXIT.idle" ]; do sleep 0.05; done
idle
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
cp "$MESSAGE" answer.txt
git add answer.txt
git commit -q -m answer
receipt "$(git rev-parse HEAD)"
idle
await_exit
"#;

/// A worker's `dagq ask` shows in `status` as an open `worker_question`;
/// while it is unclosed the screen is not read for a dialog. Its answer is
/// not typed until the worker went idle after asking, then it is typed once
/// into the worker's terminal as `answer to ask <id>: ...`, the ask is
/// closed, and `ask_delivered` is recorded.
#[test]
fn an_answered_worker_question_is_typed_into_the_idle_worker_and_closed() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, ASKING_AGENT);
    backend.prompt_wait = Duration::from_millis(300);
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(Default::default()).unwrap().is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue.asks(Default::default()).unwrap().remove(0);
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(ask.kind.as_str(), "worker_question");
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["asks"][0]["id"], ask.id.as_i64(), "{status}");
    assert_eq!(
        ask_attention(&status, ask.id)[0]["next"],
        format!("answer ask {}", ask.id)
    );

    // A dialog-like screen while the ask is unclosed is not read or recorded.
    *backend.screen.lock().unwrap() = DIALOG_SCREEN.into();
    // A poll that looked for the ask just before it was registered is over.
    thread::sleep(Duration::from_millis(200));
    let captured = backend.captures.load(Ordering::SeqCst);
    // Well past `prompt_wait`, when the screen would otherwise be read.
    thread::sleep(Duration::from_millis(1000));
    assert_eq!(backend.captures.load(Ordering::SeqCst), captured);
    let status = runtime::status(&db).unwrap();
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["kind"] != "prompt_waiting"),
        "{status}"
    );

    // Answered while the worker has not gone idle since asking: not typed.
    queue.answer(ask.id, "use blue").unwrap();
    thread::sleep(Duration::from_millis(500));
    assert!(backend.texts().is_empty());
    let status = runtime::status(&db).unwrap();
    let attention = ask_attention(&status, ask.id);
    assert_eq!(attention.len(), 1, "{status}");
    assert_eq!(attention[0]["kind"], "ask_answered");
    assert_eq!(
        attention[0]["next"],
        format!("delivering the answer of ask {} (runtime)", ask.id)
    );
    // The answer of a worker_question does not wake the inbox.
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["kind"] != "ask_answered"),
        "{events}"
    );

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("idle"),
        "",
    )
    .unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue.read_ask(ask.id).unwrap().closed_at.is_some()
    });
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    // Typed once, with its prefix.
    assert_eq!(
        backend.texts(),
        vec![(
            WORKSPACE_ID.to_owned(),
            format!("answer to ask {}: use blue", ask.id)
        )]
    );
    let worktree = Path::new(run.worktree_path().unwrap());
    assert_eq!(
        fs::read_to_string(worktree.join("answer.txt")).unwrap(),
        format!("answer to ask {}: use blue", ask.id)
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    let delivered = payloads(&detail, "ask_delivered");
    assert_eq!(
        delivered,
        vec![&json!({"ask_id": ask.id, "workspace_id": WORKSPACE_ID})]
    );
    assert!(payloads(&detail, "prompt_waiting").is_empty());
    assert!(ask_attention(&runtime::status(&db).unwrap(), ask.id).is_empty());
}

/// A send that fails is not retried: `ask_delivery_failed` is recorded
/// once, the ask stays unclosed and surfaces for the inbox to deliver.
#[test]
fn a_failed_answer_delivery_is_left_to_the_inbox() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "\"$DAGQ\" --db \"$DB\" ask --run \"$RUN_ID\" --kind worker_question --because scope --question 'Which?' --cmux /usr/bin/true >/dev/null; idle; {HOLD}; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit"
        ),
    );
    backend.text_fails = true;
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(Default::default()).unwrap().is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue.asks(Default::default()).unwrap().remove(0);
    queue.answer(ask.id, "blue").unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        !payloads(&queue.show(TaskId::new(1)).unwrap(), "ask_delivery_failed").is_empty()
    });
    // Several passes later the send was not retried.
    thread::sleep(Duration::from_millis(500));
    assert_eq!(backend.texts().len(), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let failed = payloads(&detail, "ask_delivery_failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["ask_id"], ask.id.as_i64());
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("injected cmux send failure")
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_none());
    let status = runtime::status(&db).unwrap();
    let attention = ask_attention(&status, ask.id);
    assert_eq!(attention.len(), 1, "{status}");
    assert_eq!(attention[0]["kind"], "ask_delivery_failed");
    assert_eq!(
        attention[0]["next"],
        format!(
            "send the answer of ask {} to the worker and close it",
            ask.id
        )
    );
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "ask_delivery_failed"),
        "{events}"
    );

    let run = detail.runs[0].clone();
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.texts().len(), 1);
    // The run is at rest: the inbox delivers by hand, then closes it.
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        ask_attention(&status, ask.id)[0]["next"],
        format!(
            "send the answer of ask {} to the worker and close it",
            ask.id
        )
    );
    queue.close_ask(ask.id).unwrap();
    assert!(ask_attention(&runtime::status(&db).unwrap(), ask.id).is_empty());

    // Answered after the run stopped running: nobody types it, so its
    // `ask_answered` wakes the inbox.
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let late = queue
        .ask(dagq::domain::NewAsk {
            kind: "worker_question".parse().unwrap(),
            task_id: None,
            run_id: Some(run.id().clone()),
            question: "Late?".into(),
            options: vec![],
            asked_by: "worker".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
        })
        .unwrap()
        .ask;
    queue.answer(late.id, "yes").unwrap();
    let events = dagq::watch::events(&db, EventId::new(cursor), 100, false).unwrap();
    let answered: Vec<&Value> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "ask_answered")
        .collect();
    assert_eq!(answered.len(), 1, "{events}");
    assert_eq!(
        answered[0]["next"],
        format!(
            "send the answer of ask {} to the worker and close it",
            late.id
        )
    );
}

/// The "Background work is running" dialog that holds the supervisor's
/// `/exit` back is answered by rule at the exit timeout (ADR-0047 decision
/// 29): the worktree is clean and the receipt names its HEAD, so "Exit and
/// stop tasks" is picked, recorded as `auto_repaired` with the conditions
/// and the screen, and the session exits without a timeout or an ask.
#[test]
fn a_background_work_dialog_after_exit_is_answered_when_the_receipt_is_at_head() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, HELD_AGENT);
    backend.exit_timeout = Duration::from_millis(500);
    *backend.screen.lock().unwrap() = BACKGROUND_WORK_SCREEN.into();
    let backend = Arc::new(backend);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(*backend.keys.lock().unwrap(), ["enter"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"exit_request_timed_out"), "{kinds:?}");
    assert!(!kinds.contains(&"known_dialog_unanswered"), "{kinds:?}");
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("exit_requested") < position("auto_repaired"));
    assert!(position("auto_repaired") < position("session_exited"));
    let repaired = detail
        .events
        .iter()
        .find(|e| e.kind == "auto_repaired")
        .unwrap();
    let head = detail.runs[0].result_commit().unwrap().to_string();
    assert_eq!(repaired.payload["layer"], "runtime");
    assert_eq!(repaired.payload["repair"], "dialog_answered");
    assert_eq!(repaired.payload["dialog"], "background_work");
    assert_eq!(repaired.payload["keys"], json!(["enter"]));
    assert_eq!(
        repaired.payload["conditions"],
        json!({"exit_requested": true, "clean": true, "head": head, "receipt_commit": head})
    );
    assert_eq!(repaired.payload["detail"]["workspace_id"], WORKSPACE_ID);
    assert!(
        repaired.payload["detail"]["excerpt"]
            .as_str()
            .unwrap()
            .contains("❯ 1. Exit and stop tasks")
    );
    // No stuck_exit ask; the only one is the failed review's.
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert!(
        asks.iter().all(|a| a.kind != AskKind::StuckExit),
        "{asks:?}"
    );
}

/// The same dialog is not answered when the work it would stop can still
/// change the result (here the worktree is dirty after the `/exit`): the
/// conditions are recorded as `known_dialog_unanswered`, no key is sent,
/// and the exit timeout raises the `stuck_exit` ask as before.
#[test]
fn a_background_work_dialog_over_a_dirty_worktree_is_left_to_the_ask() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; while [ ! -f \"$EXIT\" ]; do sleep 0.05; done; echo more > untracked.txt; while [ ! -f \"$EXIT.held\" ]; do sleep 0.05; done",
    );
    backend.exit_timeout = Duration::from_millis(500);
    *backend.screen.lock().unwrap() = BACKGROUND_WORK_SCREEN.into();
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(|a| a.kind == AskKind::StuckExit)
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert!(backend.keys.lock().unwrap().is_empty());
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"auto_repaired"), "{kinds:?}");
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("known_dialog_unanswered") < position("exit_request_timed_out"));
    let unanswered = detail
        .events
        .iter()
        .find(|e| e.kind == "known_dialog_unanswered")
        .unwrap();
    assert_eq!(unanswered.payload["dialog"], "background_work");
    assert_eq!(unanswered.payload["conditions"]["exit_requested"], true);
    assert_eq!(unanswered.payload["conditions"]["clean"], false);
    assert!(
        unanswered.payload["excerpt"]
            .as_str()
            .unwrap()
            .contains("Background work is running")
    );
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(backend.keys.lock().unwrap().is_empty());
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "known_dialog_unanswered")
            .count(),
        1
    );
}

/// A Settings panel left open over a working session's input box is closed
/// with Escape once (ADR-0047 decision 29), recorded as `auto_repaired`,
/// without a `prompt_waiting` or an `answer_prompt` ask.
#[test]
fn a_settings_panel_is_closed_with_escape() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, PROMPTED_AGENT);
    backend.prompt_wait = Duration::from_millis(300);
    *backend.screen.lock().unwrap() = SETTINGS_SCREEN.into();
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"auto_repaired")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(*backend.keys.lock().unwrap(), ["escape"]);
    let repaired = detail
        .events
        .iter()
        .find(|e| e.kind == "auto_repaired")
        .unwrap();
    assert_eq!(repaired.payload["repair"], "dialog_answered");
    assert_eq!(repaired.payload["dialog"], "settings_panel");
    assert_eq!(repaired.payload["keys"], json!(["escape"]));
    assert_eq!(repaired.payload["conditions"], json!({}));
    // The screen is back at work: more reads find nothing to answer or ask.
    let captured = backend.captures.load(Ordering::SeqCst);
    let started = Instant::now();
    while backend.captures.load(Ordering::SeqCst) < captured + 2 {
        assert!(started.elapsed() < Duration::from_secs(30));
        thread::sleep(Duration::from_millis(20));
    }
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"prompt_waiting"), "{kinds:?}");
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());
    assert_eq!(backend.keys.lock().unwrap().len(), 1);
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}

/// An unanswered `/exit` is recorded once and raised as one `stuck_exit` ask
/// to the inbox, notified once through the ask path, but the supervisor
/// keeps the lease and keeps watching: when the session ends later, the ask
/// is closed by the runtime and the run moves on as the verdict said. The
/// `/exit` comes after the validation and the review (ADR-0027; here a
/// failed review, the stand-in `claude` printing no verdict), so the run is
/// already `awaiting_integration` and the question says what follows.
#[test]
fn unanswered_exit_request_times_out_and_keeps_the_run() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, HELD_AGENT);
    backend.exit_timeout = Duration::from_secs(1);
    let screen = (1..=20)
        .map(|n| format!("line {n}"))
        .chain(["❯ 1. Exit anyway".into(), "  2. Cancel".into()])
        .collect::<Vec<_>>()
        .join("\n");
    *backend.screen.lock().unwrap() = screen;
    let backend = Arc::new(backend);
    SqliteQueue::open(&db)
        .unwrap()
        .register_session_workspace(SessionRole::Inbox, "inbox-ws")
        .unwrap();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    // The stuck_exit ask follows the timeout through its recovery job on a
    // later pass: it is opened (and notified), then the job's
    // `recovery_finished` naming it is recorded in another write. Both are
    // waited for, since a loaded host can take longer than the pause below
    // between them.
    wait_until(&db, Duration::from_secs(30), |queue| {
        let detail = queue.show(TaskId::new(1)).unwrap();
        let asks = queue.asks(AskQuery::default()).unwrap();
        event_kinds(&detail).contains(&"exit_request_timed_out")
            && asks.iter().any(|ask| {
                ask.kind == AskKind::StuckExit
                    && payloads(&detail, "recovery_finished")
                        .iter()
                        .any(|p| p["ask_id"] == json!(ask.id))
            })
    });
    // Let a few more polls pass: the timeout is not recorded again and the
    // run is not given up.
    thread::sleep(Duration::from_millis(500));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(run.last_error().is_none());
    assert!(queue.run_lease(run.id()).unwrap().is_some());
    let kinds = event_kinds(&detail);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "exit_request_timed_out")
            .count(),
        1
    );
    assert!(!kinds.contains(&"runtime_error"));
    assert!(!kinds.contains(&"session_exited"));
    // Nothing after the verdict happens until the session exits.
    assert!(!kinds.contains(&"review_failed"));
    let timed_out = detail
        .events
        .iter()
        .find(|e| e.kind == "exit_request_timed_out")
        .unwrap();
    assert_eq!(
        timed_out.payload,
        json!({"code": "exit_timeout", "workspace_id": WORKSPACE_ID, "timeout_secs": 1})
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    // One stuck_exit ask by the supervisor, with the screen's last 15 lines.
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = &asks[0];
    assert_eq!(ask.kind, AskKind::StuckExit);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.task_id, Some(TaskId::new(1)));
    assert_eq!(ask.asked_by, "supervisor");
    assert_eq!(ask.options, ["exit", "wait"]);
    assert!(ask.is_open());
    assert!(ask.question.contains(run.id().as_str()), "{}", ask.question);
    assert!(ask.question.contains("task 1"), "{}", ask.question);
    assert!(ask.question.contains(WORKSPACE_ID), "{}", ask.question);
    assert!(ask.question.contains("line 8\n"), "{}", ask.question);
    assert!(!ask.question.contains("line 7\n"), "{}", ask.question);
    assert!(ask.question.ends_with("  2. Cancel"), "{}", ask.question);
    assert!(
        ask.question.contains(
            "The run stays awaiting_integration under the supervisor after its validation and review, and opens an approve_landing ask for the person about its failed review once the session exits"
        ),
        "{}",
        ask.question
    );
    assert!(!ask.question.contains("stays running"), "{}", ask.question);
    assert_eq!(
        kinds.iter().filter(|k| **k == "ask_opened").count(),
        1,
        "{kinds:?}"
    );
    // Notified once, to the inbox, by the ask; no run transition notifies.
    {
        let notifications = backend.notifications.lock().unwrap();
        assert_eq!(notifications.len(), 1, "{notifications:?}");
        assert!(
            notifications[0]
                .0
                .ends_with(&format!("ask #{} stuck_exit", ask.id)),
            "{notifications:?}"
        );
        assert!(
            notifications[0]
                .1
                .ends_with(&format!("task 1 run {}", run.id()))
        );
        assert_eq!(notifications[0].2.as_deref(), Some("inbox-ws"));
    }
    // The ask is the attention, for the inbox; nobody is told to send
    // /exit, and recovery is refused while the supervisor holds the lease.
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    let attention = status["attention"].as_array().unwrap();
    assert!(
        attention.iter().all(|a| a["next"] != "send /exit"),
        "{status}"
    );
    assert!(
        attention
            .iter()
            .any(|a| a["kind"] == "ask_opened" && a["next"] == format!("answer ask {}", ask.id)),
        "{status}"
    );
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    let events = events["events"].as_array().unwrap();
    assert!(events.iter().all(|e| e["next"] != "send /exit"));
    assert!(runtime::recover(&db, run.id()).is_err());

    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_leases().unwrap().is_empty());
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("validation_finished") < position("exit_requested"));
    assert!(position("exit_request_timed_out") < position("session_exited"));
    assert!(position("session_exited") < position("workspace_closed"));
    assert!(position("workspace_closed") < position("review_failed"));
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "exit_request_timed_out")
            .count(),
        1
    );
    assert!(!kinds.contains(&"runtime_error"));
    // The runtime closed the ask when the session exited; the closing
    // answer is no attention, and nothing else was notified.
    let closed = queue.read_ask(ask.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(
        closed.answer.as_deref(),
        Some("the session exited; closed by the runtime")
    );
    // Recorded as the runtime's answer (task 325), no option chosen.
    assert_eq!(closed.answered_by.as_deref(), Some("runtime"));
    assert_eq!(closed.option_index, None);
    assert!(position("session_exited") < position("ask_answered"));
    assert!(position("ask_answered") < position("workspace_closed"));
    assert!(position("ask_answered") < position("review_failed"));
    // The only open ask is the one of the failed review, the only other
    // notification.
    let open = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].kind, AskKind::ApproveLanding);
    assert!(position("review_retried") < position("review_failed"));
    assert_eq!(backend.notifications.lock().unwrap().len(), 2);
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["kind"] != "ask_answered"),
        "{events}"
    );
    // The session exited, so the attention is the ask of the failed review.
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["ask_id"] == json!(open[0].id)),
        "{status}"
    );
}

#[test]
fn claude_stop_hook_settings_publish_the_idle_marker() {
    use dagq::infrastructure::adapters::{ClaudeCode, stop_hook_settings};
    let dir = tempfile::tempdir().unwrap();
    let run_dir = dir.path().join("run's dir");
    fs::create_dir(&run_dir).unwrap();
    let run = TaskRun::restore(dagq::domain::RunRecord {
        id: RunId::new("11111111-2222-4333-8444-555555555555").unwrap(),
        task_id: TaskId::new(1),
        status: RunStatus::Starting,
        requested_provider: dagq::domain::Provider::Claude,
        actual_provider: dagq::domain::Provider::Claude,
        base_commit: sha("0123456789abcdef0123456789abcdef01234567"),
        branch: Some("dagq/x".into()),
        worktree_path: Some(dir.path().to_str().unwrap().into()),
        workspace_id: None,
        receipt_path: Some(run_dir.join("receipt.json").to_str().unwrap().into()),
        log_path: Some(run_dir.join("claude.debug.log").to_str().unwrap().into()),
        result_commit: None,
        repo_path: None,
        run_dir: Some(run_dir.to_str().unwrap().into()),
        last_error: None,
        workspace_closed_at: None,
        created_at: String::new(),
    })
    .unwrap();
    let command = ClaudeCode {
        executable: "claude".into(),
    }
    .command(&run, "prompt")
    .unwrap();
    let args: Vec<String> = command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let settings = run_dir.join("claude-settings.json");
    assert!(args.contains(&"--settings".to_string()));
    assert!(args.contains(&settings.to_str().unwrap().to_string()));
    let text = fs::read_to_string(&settings).unwrap();
    assert_eq!(
        text,
        stop_hook_settings(&run.idle_marker_path().unwrap()).unwrap()
    );
    let parsed: Value = serde_json::from_str(&text).unwrap();
    // A non-empty auto mode environment from flag settings keeps the
    // "Teach auto mode" dialog away; `$defaults` keeps the built-in entries.
    assert_eq!(parsed["autoMode"]["environment"], json!(["$defaults"]));
    let hook = &parsed["hooks"]["Stop"][0]["hooks"][0];
    assert_eq!(hook["type"], "command");
    // Run the hook exactly as Claude would: shell command, event JSON on stdin.
    let payload =
        r#"{"session_id":"11111111-2222-4333-8444-555555555555","hook_event_name":"Stop"}"#;
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(hook["command"].as_str().unwrap())
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            let _waiting = common::within(common::STEP_LIMIT, "the Stop hook to exit");
            child.stdin.take().unwrap().write_all(payload.as_bytes())?;
            child.wait()
        })
        .unwrap();
    assert!(status.success());
    assert_eq!(
        fs::read_to_string(run.idle_marker_path().unwrap()).unwrap(),
        payload
    );
    assert!(!run_dir.join("idle.json.tmp").exists());
    // Each marker is also appended, with the time, to the log next to it.
    let log = fs::read_to_string(run_dir.join("idle.log")).unwrap();
    let (secs, marker) = log.strip_suffix('\n').unwrap().split_once('\t').unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(now.abs_diff(secs.parse().unwrap()) < 60, "{log}");
    assert_eq!(marker, payload);
}
