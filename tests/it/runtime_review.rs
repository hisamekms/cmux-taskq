//! Runtime tests: The review job of a run, the exit after it, background work and
//! conflicts with a moving main.
use crate::common;
use crate::runtime_support;

use runtime_support::*;

/// The worker goes idle after its receipt and never exits by itself; each
/// time a text arrives in its terminal it appends a line, commits, rewrites
/// the receipt and goes idle again, `revises` times.
fn revising_agent(revises: usize) -> String {
    format!(
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
         for n in $(seq 1 {revises}); do \
           while [ ! -f \"$MESSAGE\" ]; do sleep 0.1; done; rm \"$MESSAGE\"; \
           printf 'fix %s\\n' \"$n\" >> change.txt; git commit -q -am \"fix $n\"; \
           receipt \"$(git rev-parse HEAD)\"; idle; \
         done; await_exit"
    )
}

/// The `send_exit` attempts that failed, as (attempt, max_attempts,
/// retry_after_ms).
fn exit_attempts(detail: &dagq::domain::TaskDetail) -> Vec<(Value, Value, Value)> {
    backend_failures(detail)
        .into_iter()
        .filter(|e| e.payload["op"] == "send_exit")
        .map(|e| {
            (
                e.payload["attempt"].clone(),
                e.payload["max_attempts"].clone(),
                e.payload["retry_after_ms"].clone(),
            )
        })
        .collect()
}

/// A `/exit` that cmux timed out before it reached the session (the screen
/// shows the input box and no trace of it) is sent again after a backoff,
/// and the run goes on without being given up (task 354).
#[test]
fn an_exit_that_timed_out_before_reaching_the_session_is_sent_again() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(1, Ordering::SeqCst);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 2);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(exit_attempts(&detail), [(json!(1), json!(3), json!(10))]);
    let kinds = event_kinds(&detail);
    // The session exited on the second /exit: nothing more to record.
    assert!(position(&kinds, "exit_requested") < position(&kinds, "session_exited"));
    for kind in ["exit_unsent", "exit_request_timed_out", "runtime_error"] {
        assert!(!kinds.contains(&kind), "{kinds:?}");
    }
}

/// A `/exit` that never got there on any attempt leaves a run whose review
/// passed and whose receipt still holds against a clean worktree to land:
/// the workspace is closed instead of waiting on a session that was never
/// asked, and `exit_unsent` records it (task 354).
#[test]
fn an_exit_that_never_got_there_closes_a_sound_passed_run_and_lands_it() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(usize::MAX, Ordering::SeqCst);
    backend.close_ends_session = true;
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    // Typed once and twice again, never more.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 3);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert_eq!(
        exit_attempts(&detail),
        [
            (json!(1), json!(3), json!(10)),
            (json!(2), json!(3), json!(20)),
            (json!(3), json!(3), Value::Null),
        ]
    );
    assert_eq!(
        payloads(&detail, "exit_unsent"),
        [
            &json!({"code": "backend_timeout", "workspace_id": WORKSPACE_ID, "attempts": 3, "action": "close_and_land"})
        ]
    );
    // Closing the workspace to land is recorded as a repair.
    assert_eq!(
        payloads(&detail, "auto_repaired"),
        [&json!({
            "layer": "runtime",
            "repair": "exit_forced_close",
            "conditions": {
                "cause": "backend_timeout",
                "attempts": 3,
                "exit_reached": false,
                "then": "land",
                "review": "pass",
                "receipt_holds": true,
            },
            "detail": {"workspace_id": WORKSPACE_ID},
        })]
    );
    let kinds = event_kinds(&detail);
    for (earlier, later) in [
        ("review_finished", "exit_requested"),
        ("exit_requested", "exit_unsent"),
        ("exit_unsent", "workspace_closed"),
        ("workspace_closed", "run_integrated"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    for kind in ["exit_request_timed_out", "runtime_error"] {
        assert!(!kinds.contains(&kind), "{kinds:?}");
    }
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
}

/// A `/exit` that never got there leaves a run that does not land on its
/// own (here its review failed) to the person, as a session that held its
/// `/exit` back is: the `stuck_exit` ask, at once, and the run kept (task
/// 354).
#[test]
fn an_exit_that_never_got_there_asks_for_a_run_that_cannot_land() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(usize::MAX, Ordering::SeqCst);
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_lease(run.id()).unwrap().is_some());
    // A run held back is not repaired: it goes to the ask.
    assert!(payloads(&detail, "auto_repaired").is_empty());
    assert_eq!(
        payloads(&detail, "exit_unsent"),
        [&json!({
            "code": "backend_timeout", "workspace_id": WORKSPACE_ID, "attempts": 3,
            "action": "ask", "held": "the run does not land after its exit",
        })]
    );
    assert_eq!(
        payloads(&detail, "exit_request_timed_out"),
        [
            &json!({"code": "exit_timeout", "workspace_id": WORKSPACE_ID, "timeout_secs": 120, "unsent": true})
        ]
    );
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::StuckExit);
    assert!(
        asks[0].question.contains("(exit_unsent)"),
        "{}",
        asks[0].question
    );
    // The person has the session exit; the run goes on as before.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 3);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "exit_unsent") < position(&kinds, "session_exited"));
    assert!(position(&kinds, "session_exited") < position(&kinds, "review_failed"));
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert!(queue.read_ask(asks[0].id).unwrap().closed_at.is_some());
}

/// A sound passed run whose workspace cannot be closed either is not
/// landed with its session alive: the `stuck_exit` ask says why (task 354).
#[test]
fn an_exit_that_never_got_there_asks_when_the_workspace_cannot_be_closed() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(usize::MAX, Ordering::SeqCst);
    backend.close_times_out = true;
    let backend = Arc::new(backend);
    let reviewer = Arc::new(TestReviewer::new(&[verdict(
        "pass",
        &[],
        "meets the acceptance",
    )]));
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
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    let unsent = payloads(&detail, "exit_unsent");
    assert_eq!(unsent[0]["action"], "ask", "{unsent:?}");
    assert!(
        unsent[0]["held"]
            .as_str()
            .unwrap()
            .starts_with("its workspace could not be closed"),
        "{unsent:?}"
    );
    assert!(run.workspace_closed_at().is_none());
    // The person has the session exit; the run lands without a close.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
}

/// A passed run whose worktree changed after its review does not land
/// without its session's exit: the `/exit` that never got there is the
/// `stuck_exit` ask, saying why (task 354).
#[test]
fn an_exit_that_never_got_there_asks_for_a_passed_run_whose_worktree_changed() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(usize::MAX, Ordering::SeqCst);
    let backend = Arc::new(backend);
    // The review runs in the worktree; this one leaves a file behind.
    let reviewer = Arc::new(TestReviewer::new(&[format!(
        "printf 'x\\n' > stray.txt; {}",
        verdict("pass", &[], "meets the acceptance")
    )]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || supervise_reviewed(&db, &repo, &backend, &reviewer))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    let unsent = payloads(&detail, "exit_unsent");
    assert_eq!(unsent.len(), 1, "{unsent:?}");
    assert_eq!(unsent[0]["action"], "ask");
    let held = unsent[0]["held"].as_str().unwrap();
    assert!(
        held.starts_with("its receipt no longer holds: worktree is not clean"),
        "{held}"
    );
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks[0].kind, AskKind::StuckExit);
    assert!(asks[0].question.contains(held), "{}", asks[0].question);
    // The person cleans up and has the session exit: the run lands.
    fs::remove_file(Path::new(run.worktree_path().unwrap()).join("stray.txt")).unwrap();
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 3);
}

/// A receipt accepted with the session still open is reviewed before the
/// session is asked to exit (ADR-0027 decision 1); on `pass` the supervisor
/// sends `/exit`, closes the workspace and lands the run without anyone
/// calling `integrate`.
#[test]
fn a_passing_review_exits_the_live_session_and_lands_it() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert!(queue.run_leases().unwrap().is_empty());
    let kinds = event_kinds(&detail);
    // The session lives through validation and review; /exit comes after
    // the verdict, the landing after the close.
    for (earlier, later) in [
        ("session_idle_observed", "supervision_finished"),
        ("supervision_finished", "validation_finished"),
        ("validation_finished", "review_started"),
        ("review_started", "review_finished"),
        ("review_finished", "exit_requested"),
        ("exit_requested", "session_exited"),
        ("session_exited", "workspace_closed"),
        ("workspace_closed", "integration_started"),
        ("integration_started", "run_integrated"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    assert!(!kinds.contains(&"integration_approved"), "{kinds:?}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
    let supervised = payloads(&detail, "supervision_finished");
    assert_eq!(
        supervised[0],
        &json!({"status": "validating", "exit_code": null, "session_live": true})
    );
    let started = payloads(&detail, "review_started");
    assert_eq!(started.len(), 1);
    // The job's own session id (ADR-0048 decision 4).
    let session_id = started[0]["session_id"].as_str().unwrap();
    assert_eq!(
        started,
        [
            &json!({"attempt": 1, "workspace_id": WORKSPACE_ID, "session_live": true, "session_id": session_id})
        ]
    );
    // The worker's session and the review's are spans, each closed once
    // (ADR-0048 decision 2).
    let opened: Vec<(&str, &str)> = payloads(&detail, "session_opened")
        .iter()
        .map(|p| {
            (
                p["kind"].as_str().unwrap(),
                p["session_id"].as_str().unwrap(),
            )
        })
        .collect();
    let run_id = detail.runs[0].id().as_str();
    assert_eq!(opened, [("worker", run_id), ("review", session_id)]);
    let closed: Vec<(&str, &str)> = payloads(&detail, "session_closed")
        .iter()
        .map(|p| (p["kind"].as_str().unwrap(), p["reason"].as_str().unwrap()))
        .collect();
    assert_eq!(closed, [("review", "job_finished"), ("worker", "exited")]);
    // No transcript of either session exists: their active time is not
    // recorded, and the run lands all the same (ADR-0048 decision 10).
    for closed in payloads(&detail, "session_closed") {
        assert_eq!(closed["active"], "unavailable", "{closed}");
        assert_eq!(
            closed["active_unavailable"], "transcript_missing",
            "{closed}"
        );
    }
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["verdict"], "pass");
    assert_eq!(finished[0]["reasons"], json!([]));
    assert_eq!(finished[0]["summary"], "meets the acceptance");
    assert_eq!(finished[0]["attempt"], 1);
    assert!(finished[0]["duration_secs"].is_u64());
    // The reviewer reads review.md and is told the acceptance, the schema
    // and the verdicts.
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 1);
    let run_dir = Path::new(run.run_dir().unwrap());
    let review_md = run_dir.join("review.md");
    assert!(review_md.is_file());
    for expected in [
        format!("Read the review material at {}", review_md.display()),
        "Acceptance criteria of the task:\nworks".to_owned(),
        r#"{"verdict": "pass" | "revise" | "concern", "reasons": [string], "summary": string}"#
            .to_owned(),
        "- revise: findings the worker can fix without a person's judgment".to_owned(),
        "- concern: findings that need a person's judgment".to_owned(),
    ] {
        assert!(
            prompts[0].contains(&expected),
            "{expected:?} not in {}",
            prompts[0]
        );
    }
    assert_eq!(
        fs::read_to_string(run_dir.join("review-prompt-1.txt")).unwrap(),
        prompts[0]
    );
    assert!(run_dir.join("terminal-final.txt").is_file());
    // Nothing waits for anyone.
    assert!(run_attention_of(&runtime::status(&db).unwrap(), run.id()).is_none());
}

/// A `revise` verdict goes to the live session as a fixed request; once
/// the session rewrote its receipt for a new head and went idle, the run is
/// validated and reviewed again, and lands on `pass` (ADR-0027 decision 2).
#[test]
fn a_revise_verdict_is_fixed_by_the_live_session_and_reviewed_again() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &revising_agent(1));
    let reviewer = TestReviewer::new(&[
        verdict("revise", &["add a line to change.txt"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        format!("change by {}\nfix 1\n", run.id())
    );
    let requested = payloads(&detail, "revise_requested");
    assert_eq!(requested.len(), 1);
    assert_eq!(requested[0]["attempt"], 1);
    assert_eq!(requested[0]["reasons"], json!(["add a line to change.txt"]));
    let revised = payloads(&detail, "revise_finished");
    assert_eq!(revised.len(), 1);
    let head = git_out(
        &repo,
        &["rev-parse", &format!("refs/dagq/runs/{}", run.id())],
    );
    assert_eq!(revised[0], &json!({"attempt": 1, "head": head}));
    let verdicts: Vec<&Value> = payloads(&detail, "review_finished")
        .iter()
        .map(|p| &p["verdict"])
        .collect();
    assert_eq!(verdicts, [&json!("revise"), &json!("pass")]);
    assert_eq!(payloads(&detail, "validation_finished").len(), 2);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "revise_requested") < position(&kinds, "revise_finished"));
    assert!(position(&kinds, "revise_finished") < position(&kinds, "exit_requested"));
    // One /exit, after the second review; the request named the findings.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let texts = backend.texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0].0, WORKSPACE_ID);
    let text = &texts[0].1;
    for expected in [
        format!(
            "dagq: the supervisor's review of run {} (task 1) asks for changes (revise 1 of 2).",
            run.id()
        ),
        "Findings:\n- add a line to change.txt".to_owned(),
        "[\"test -f seed.txt\"]".to_owned(),
        format!(
            "Rewrite the receipt at {} with the new head commit",
            run.receipt_path().unwrap()
        ),
        runtime::STOP_BACKGROUND.to_owned(),
        "Do not merge or push. When done, report briefly and stop; do not run /exit.".to_owned(),
    ] {
        assert!(text.contains(&expected), "{expected:?} not in {text}");
    }
    let run_dir = Path::new(run.run_dir().unwrap());
    assert_eq!(
        &fs::read_to_string(run_dir.join("revise-1.txt")).unwrap(),
        text
    );
}

/// Revise is sent at most twice; a third review that does not pass becomes
/// an `approve_landing` ask after `/exit` and the close. `land` then lands
/// the run as an approved one.
#[test]
fn a_third_review_that_does_not_pass_asks_a_person_and_land_lands_it() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &revising_agent(2));
    let reviewer = TestReviewer::new(&[verdict("revise", &["still short"], "not yet")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(payloads(&detail, "revise_requested").len(), 2);
    assert_eq!(payloads(&detail, "revise_finished").len(), 2);
    assert_eq!(payloads(&detail, "review_finished").len(), 3);
    assert_eq!(backend.texts().len(), 2);
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
    assert!(queue.run_leases().unwrap().is_empty());
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "workspace_closed") < position(&kinds, "ask_opened"));
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    let ask = &asks[0];
    assert_eq!(ask.kind, dagq::domain::AskKind::ApproveLanding);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.options, ["land", "send_back", "cancel"]);
    assert_eq!(ask.asked_by, "supervisor");
    assert!(
        ask.question.contains(
            "returned revise (the review still asks for changes after 2 revises): not yet"
        ),
        "{}",
        ask.question
    );
    assert!(ask.question.contains("\n- still short"), "{}", ask.question);
    // The ask is the attention, for the inbox; the run itself is not one.
    let status = runtime::status(&db).unwrap();
    assert!(
        run_attention_of(&status, run.id()).is_none() || {
            let entries: Vec<&Value> = status["attention"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|a| a["run_id"] == json!(run.id()))
                .collect();
            entries.iter().all(|a| a["ask_id"] == json!(ask.id))
        }
    );
    let entry = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["ask_id"] == json!(ask.id))
        .unwrap();
    assert_eq!(entry["next"], format!("answer ask {}", ask.id));

    queue.answer(ask.id, "land").unwrap();
    let status = runtime::status(&db).unwrap();
    let entry = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["ask_id"] == json!(ask.id))
        .unwrap();
    assert_eq!(
        entry["next"],
        format!("applying the answer of ask {} (runtime)", ask.id)
    );
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed_run(&detail.runs[0], &repo, &base);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let approved = payloads(&detail, "integration_approved");
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0]["ask_id"], ask.id.as_i64());
    // No fourth review.
    assert_eq!(reviewer.prompts().len(), 3);
}

/// A `concern` exits and closes the session and asks a person; `send_back`
/// parks the run for a resume whose request names the findings, and the
/// resumed session goes through validation and review like the worker's
/// (ADR-0027 decision 3): the review passes and the supervisor lands it.
#[test]
fn a_concern_sent_back_is_resumed_reviewed_again_and_landed() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[
        verdict(
            "concern",
            &["changes a file the task did not name"],
            "scope",
        ),
        verdict("pass", &[], "fixed"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(backend.texts().is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "review_finished") < position(&kinds, "exit_requested"));
    assert!(position(&kinds, "workspace_closed") < position(&kinds, "ask_opened"));
    let ask = queue.asks(Default::default()).unwrap()[0].clone();
    assert!(
        ask.question.contains("returned concern: scope"),
        "{}",
        ask.question
    );
    // The ask was opened through `runtime::ask`, which notifies the inbox.
    let notified = backend.notifications.lock().unwrap().clone();
    assert_eq!(notified.len(), 1, "{notified:?}");
    assert!(
        notified[0].1.contains(&format!("run {}", run.id())),
        "{notified:?}"
    );

    queue.answer(ask.id, "send_back").unwrap();
    backend.resume_script_for(
        1,
        "await_message; printf 'narrowed\\n' > change.txt; unlocked git commit -q -am narrowed; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let landed = detail.runs[0].clone();
    assert_landed_run(&landed, &repo, &base);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "narrowed\n"
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let decided = payloads(&detail, "landing_decided");
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0]["answer"], "send_back");
    assert_eq!(decided[0]["status"], "needs_session");
    let text = &backend.texts()[0].1;
    assert!(
        text.contains("raised findings a person sent back to you"),
        "{text}"
    );
    assert!(
        text.contains("changes a file the task did not name"),
        "{text}"
    );
    assert!(text.contains("Fix the findings in the reason"), "{text}");
    // The resumed session stayed open through the review: validation,
    // review, then /exit and the close of its workspace, then the landing.
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1, "{finished:?} {:?}", event_kinds(&detail));
    assert_eq!(finished[0]["outcome"], "resolved");
    assert_eq!(finished[0]["status"], "validating");
    assert_eq!(finished[0]["workspace_closed"], false);
    assert_eq!(finished[0]["session_live"], true);
    let resume_workspace = finished[0]["workspace_id"].as_str().unwrap().to_owned();
    let kinds = event_kinds(&detail);
    let after_resume: Vec<&str> = kinds
        .iter()
        .skip_while(|k| **k != "resume_finished")
        .filter(|k| {
            matches!(
                **k,
                "validation_finished"
                    | "review_started"
                    | "review_finished"
                    | "exit_requested"
                    | "workspace_closed"
                    | "integration_started"
                    | "run_integrated"
            )
        })
        .copied()
        .collect();
    assert_eq!(
        after_resume,
        [
            "validation_finished",
            "review_started",
            "review_finished",
            "exit_requested",
            "workspace_closed",
            "integration_started",
            "run_integrated",
        ]
    );
    let closed = payloads(&detail, "workspace_closed");
    assert_eq!(
        closed.last().unwrap(),
        &&json!({"workspace_id": resume_workspace, "resume_attempt": 1})
    );
    assert!(backend.closed().contains(&resume_workspace));
    assert_eq!(payloads(&detail, "review_started")[1]["session_live"], true);
    assert_eq!(reviewer.prompts().len(), 2);
}

/// `cancel` fails the run and cancels its task.
#[test]
fn a_concern_canceled_fails_the_run_and_cancels_the_task() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("concern", &["not wanted"], "drop it")]);
    supervise_reviewed(&db, &repo, &backend, &reviewer);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue.asks(Default::default()).unwrap()[0].clone();
    queue.answer(ask.id, "cancel").unwrap();
    supervise_reviewed(&db, &repo, &backend, &reviewer);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert_eq!(
        detail.runs[0].last_error(),
        Some(format!("canceled by ask {}", ask.id).as_str())
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert_eq!(reviewer.prompts().len(), 1);
}

/// A headless review that fails (a non-zero exit, stdout without a verdict,
/// or the timeout) exits and closes the session, and in the step that
/// records `review_failed` opens an `approve_landing` ask with the failure
/// and where the review material is (task 328). Stdout without a readable
/// verdict (here an unescaped quote, or prose) is reviewed
/// once more first; a job that failed is not. The ask, not the failure, is
/// the attention, and its answer is applied by the supervisor: `land`
/// lands the run, `cancel` fails it and cancels the task.
#[test]
fn a_failed_review_closes_the_session_and_asks_a_person_in_the_same_step() {
    let unquoted = r#"printf '%s\n' '{"verdict":"pass","reasons":[],"summary":"says "fine""}'"#;
    for (script, timeout, expected, reviews, answer) in [
        (
            "echo broken >&2; exit 3",
            60,
            "exited with exit status: 3: broken",
            1,
            "land",
        ),
        (
            "echo 'no verdict here'",
            60,
            "the review printed no verdict JSON",
            2,
            "cancel",
        ),
        (
            unquoted,
            60,
            "the review printed no verdict JSON",
            2,
            "land",
        ),
        ("sleep 30", 1, "did not finish within 1 seconds", 1, "land"),
    ] {
        let (_dir, repo, db) = fixture();
        let base = git_out(&repo, &["rev-parse", "main"]);
        let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
        let mut reviewer = TestReviewer::new(&[script.to_owned()]);
        reviewer.timeout = Duration::from_secs(timeout);
        let cursor = SqliteQueue::open(&db)
            .unwrap()
            .latest_event_id()
            .unwrap()
            .as_i64();
        let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
        let mut queue = SqliteQueue::open(&db).unwrap();
        let detail = queue.show(TaskId::new(1)).unwrap();
        let run = detail.runs[0].clone();
        assert!(queue.run_leases().unwrap().is_empty());
        assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
        let kinds = event_kinds(&detail);
        assert!(!kinds.contains(&"review_finished"), "{kinds:?}");
        assert_eq!(reviewer.prompts().len(), reviews, "{script}");
        assert_eq!(payloads(&detail, "review_started").len(), reviews);
        let retried = payloads(&detail, "review_retried");
        assert_eq!(retried.len(), reviews - 1, "{kinds:?}");
        if let Some(retried) = retried.first() {
            assert_eq!(retried["attempt"], 1);
            assert!(retried["error"].as_str().unwrap().contains(expected));
        }
        assert!(position(&kinds, "workspace_closed") < position(&kinds, "ask_opened"));
        assert!(position(&kinds, "ask_opened") < position(&kinds, "review_failed"));
        let failed = payloads(&detail, "review_failed");
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0]["attempt"], reviews);
        assert_eq!(failed[0]["status"], "awaiting_integration");
        let error = failed[0]["error"].as_str().unwrap();
        assert!(error.contains(expected), "{error}");
        // The ask carries the failure and the material.
        let asks = queue.asks(Default::default()).unwrap();
        assert_eq!(asks.len(), 1, "{asks:?}");
        let ask = &asks[0];
        assert_eq!(failed[0]["ask_id"], ask.id.as_i64());
        assert_eq!(ask.kind, dagq::domain::AskKind::ApproveLanding);
        assert_eq!(ask.run_id.as_ref(), Some(run.id()));
        assert_eq!(ask.options, ["land", "send_back", "cancel"]);
        assert_eq!(ask.asked_by, "supervisor");
        let run_dir = run.run_dir().unwrap();
        for part in [
            format!("failed and gave no verdict (review {reviews}): "),
            expected.to_owned(),
            format!("Review material: {run_dir}/review.md"),
            format!("{run_dir}/review-{reviews}.out"),
        ] {
            assert!(ask.question.contains(&part), "{part} in {}", ask.question);
        }
        {
            let notifications = backend.notifications.lock().unwrap();
            assert_eq!(notifications.len(), 1, "{notifications:?}");
            assert!(notifications[0].0.ends_with("approve_landing"));
        }
        // The ask is the attention, and the only event that wakes the inbox.
        let status = runtime::status(&db).unwrap();
        assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
        assert!(
            status["attention"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["ask_id"] == json!(ask.id)
                    && a["next"] == format!("answer ask {}", ask.id)),
            "{status}"
        );
        let events = dagq::watch::events(&db, EventId::new(cursor), 100, false).unwrap();
        let events = events["events"].as_array().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["kind"], "ask_opened");
        // The supervisor applies the answer.
        queue.answer(ask.id, answer).unwrap();
        let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        let detail = queue.show(TaskId::new(1)).unwrap();
        assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
        if answer == "land" {
            assert_landed_run(&detail.runs[0], &repo, &base);
            assert_eq!(detail.task.status(), TaskStatus::Completed);
            assert_eq!(
                payloads(&detail, "integration_approved")[0]["ask_id"],
                ask.id.as_i64()
            );
        } else {
            assert_eq!(detail.runs[0].status(), RunStatus::Failed);
            assert_eq!(detail.task.status(), TaskStatus::Canceled);
        }
        // No review after the answer.
        assert_eq!(reviewer.prompts().len(), reviews);
    }
}

/// A review whose stdout holds no readable verdict is reviewed once more
/// with the same input (task 328); a verdict from the retry goes on as
/// usual, here a pass that lands without anyone asked.
#[test]
fn an_unreadable_verdict_is_reviewed_again_and_a_readable_one_lands() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[
        r#"printf '%s\n' '{"verdict":"pass","reasons":[],"summary":"says "fine""}'"#.to_owned(),
        verdict("pass", &[], "meets the acceptance"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed_run(&detail.runs[0], &repo, &base);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 2);
    // The same input, so the same prompt.
    assert_eq!(prompts[0], prompts[1]);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"review_failed"), "{kinds:?}");
    let retried = payloads(&detail, "review_retried");
    assert_eq!(retried.len(), 1);
    assert_eq!(retried[0]["attempt"], 1);
    assert!(
        retried[0]["error"]
            .as_str()
            .unwrap()
            .contains("the review printed no verdict JSON")
    );
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["attempt"], 2);
    assert_eq!(finished[0]["verdict"], "pass");
    assert!(position(&kinds, "review_retried") < position(&kinds, "review_finished"));
    assert!(
        queue
            .asks(AskQuery {
                all: true,
                ..Default::default()
            })
            .unwrap()
            .is_empty()
    );
    assert!(backend.notifications.lock().unwrap().is_empty());
}

/// A revise whose rewritten receipt does not name the clean worktree HEAD
/// (here the commit before the fix) is not handed to validation, which
/// would fail the run and its work: the live session is asked to rewrite
/// it for HEAD, and once it does the run is reviewed again and lands
/// (task 107, description (d)).
#[test]
fn a_revise_receipt_for_another_commit_is_sent_back_to_the_session_until_it_names_head() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let script = "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
        while [ ! -f \"$MESSAGE\" ]; do sleep 0.1; done; rm \"$MESSAGE\"; \
        before=$(git rev-parse HEAD); printf 'fix\\n' >> change.txt; git commit -q -am fix; \
        receipt \"$before\"; idle; \
        while [ ! -f \"$MESSAGE\" ]; do sleep 0.1; done; rm \"$MESSAGE\"; \
        receipt \"$(git rev-parse HEAD)\"; idle; await_exit";
    let backend = TestWorkspace::new(&db, false, script);
    let reviewer = TestReviewer::new(&[
        verdict("revise", &["add a line"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        format!("change by {}\nfix\n", run.id())
    );
    let rejected = payloads(&detail, "revise_receipt_rejected");
    assert_eq!(rejected.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(rejected[0]["attempt"], 1);
    assert!(
        rejected[0]["reason"]
            .as_str()
            .unwrap()
            .contains("but the worktree HEAD is"),
        "{}",
        rejected[0]
    );
    // Validation saw only the receipt for HEAD: nothing failed.
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 2);
    assert!(validated.iter().all(|v| v["accepted"] == true));
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "revise_receipt_rejected") < position(&kinds, "revise_finished"));
    let texts = backend.texts();
    assert_eq!(texts.len(), 2);
    assert!(
        texts[1].1.contains(&format!(
            "dagq: the receipt you rewrote for revise 1 of run {} cannot be accepted",
            run.id()
        )),
        "{}",
        texts[1].1
    );
    assert!(texts[1].1.contains("git rev-parse HEAD"), "{}", texts[1].1);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
}

/// A supervisor died while it waited for the session to exit after a
/// passing review, with the exit timeout recorded and no `stuck_exit` ask
/// made yet. The adopter reviews nothing again and sends no second `/exit`
/// (ADR-0027), asks about the stuck exit once with what follows (the
/// landing), closes that ask once the session exits, and lands the run.
#[test]
fn adopted_run_waiting_for_its_exit_after_a_pass_asks_once_and_lands() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let idle = run.idle_marker_path().unwrap();
    wait_until(&db, Duration::from_secs(20), |_| idle.is_file());
    let head = git_out(
        Path::new(run.worktree_path().unwrap()),
        &["rev-parse", "HEAD"],
    );
    // What the dead supervisor got through: validation, a passing review,
    // the /exit and its timeout.
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), head],
        )
        .unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    for (kind, payload) in [
        (
            "validation_finished",
            json!({"status": "awaiting_integration"}),
        ),
        ("review_started", json!({"attempt": 1})),
        (
            "review_finished",
            json!({"verdict": "pass", "reasons": [], "summary": "ok", "attempt": 1}),
        ),
        (
            "exit_requested",
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
        ),
        (
            "exit_request_timed_out",
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
        ),
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    age_lease(&db, &run, 31);
    let reviewer = Arc::new(TestReviewer::new(&[verdict("concern", &["x"], "never")]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || {
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
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    thread::sleep(Duration::from_millis(1500));
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::StuckExit);
    assert!(
        ask.question
            .contains("The run stays awaiting_integration under the supervisor after its validation and review, and lands on main once the session exits"),
        "{}",
        ask.question
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    // The person's /exit reaches the session.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert!(reviewer.prompts().is_empty(), "reviewed again");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert_eq!(kinds.iter().filter(|k| **k == "review_started").count(), 1);
    let closed = queue.read_ask(ask.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(
        closed.answer.as_deref(),
        Some("the session exited; closed by the runtime")
    );
    assert_eq!(
        queue
            .asks(AskQuery {
                all: true,
                ..Default::default()
            })
            .unwrap()
            .len(),
        1
    );
}

/// Waits until the run's idle marker shows background work running.
fn wait_for_background(run: &TaskRun) {
    let marker = run.idle_marker_path().unwrap();
    let started = Instant::now();
    while !fs::read_to_string(&marker).is_ok_and(|text| text.contains("\"running\"")) {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "no background marker"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Writes the Stop hook's marker for the run as the session would, with
/// `background_tasks` as given.
fn write_idle_marker(run: &TaskRun, background_tasks: Value) {
    let marker = run.idle_marker_path().unwrap();
    let hook = json!({
        "session_id": run.id(),
        "hook_event_name": "Stop",
        "stop_hook_active": false,
        "background_tasks": background_tasks,
    });
    let tmp = marker.with_extension("tmp");
    fs::write(&tmp, hook.to_string()).unwrap();
    fs::rename(&tmp, &marker).unwrap();
}

/// Lets the supervisor poll a while: what it did not do by then it holds.
const HOLD_PERIOD: Duration = Duration::from_millis(600);

/// The session goes idle after its receipt with background work still
/// running (task 147): the supervisor does not take it for idle, so neither
/// validation nor `/exit` starts, until the work ended and the Stop hook
/// wrote a marker with empty `background_tasks`.
#[test]
fn background_work_holds_the_first_session_until_it_ends() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle_bg; \
         while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; idle_bg_done; await_exit",
    ));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"receipt_observed")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    wait_for_background(&run);
    thread::sleep(HOLD_PERIOD);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Running);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"session_idle_observed"), "{kinds:?}");
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "session_idle_observed") < position(&kinds, "exit_requested"));
    assert!(!kinds.contains(&"exit_request_timed_out"));
}

/// A marker whose background work is over (`completed`) or that names none
/// is idle as before.
#[test]
fn a_marker_without_running_background_work_is_idle() {
    for tasks in [
        json!([]),
        json!([{"id": "b1", "type": "shell", "status": "completed"}]),
    ] {
        let script = format!(
            "commit work; receipt \"$(git rev-parse HEAD)\"; \
             printf '%s' '{}' > \"$IDLE.tmp\"; mv \"$IDLE.tmp\" \"$IDLE\"; await_exit",
            json!({"session_id": "s", "hook_event_name": "Stop", "background_tasks": tasks})
        );
        let (_dir, repo, db) = fixture();
        let backend = TestWorkspace::new(&db, false, &script);
        let outcome = supervise(&db, &repo, &backend).unwrap();
        backend.join();
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
        assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    }
}

/// After the review, the `/exit` waits while the session's marker shows
/// background work running (the session took a turn up again after its
/// receipt), and goes once the work ended.
#[test]
fn background_work_holds_the_exit_after_the_review() {
    let (dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let gate = dir.path().join("review-gate");
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let reviewer = Arc::new(TestReviewer::new(&[format!(
        "while [ ! -f {} ]; do sleep 0.05; done; {}",
        shell_join(&[gate.to_string_lossy().into_owned()]),
        verdict("pass", &[], "meets the acceptance")
    )]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || supervise_reviewed(&db, &repo, &backend, &reviewer))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"review_started")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    write_idle_marker(
        &run,
        json!([{"id": "b1", "type": "shell", "status": "running", "command": "cargo test"}]),
    );
    fs::write(&gate, "").unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"review_finished")
    });
    thread::sleep(HOLD_PERIOD);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);

    write_idle_marker(&run, json!([]));
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_landed(
        &repo,
        &queue.show(TaskId::new(1)).unwrap().runs[0],
        "test task",
        &base,
    );
}

/// A resumed session that rewrote its receipt and stopped with background
/// work running is not asked to exit until the work ended.
#[test]
fn background_work_holds_the_resumed_session_until_it_ends() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let sent_before = backend.exits_sent.load(Ordering::SeqCst);
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle_bg; \
         while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; idle_bg_done; await_exit",
    );
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_for_background(&run);
    thread::sleep(HOLD_PERIOD);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert!(payloads(&detail, "resume_finished").is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), sent_before);

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), sent_before + 1);
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(
        payloads(&detail, "resume_finished")[0]["outcome"],
        "resolved"
    );
}

/// A session revising its work that stops with background work running is
/// not taken for done: the revise waits until the work ended.
#[test]
fn background_work_holds_the_revise_until_it_ends() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
         while [ ! -f \"$MESSAGE\" ]; do sleep 0.05; done; rm \"$MESSAGE\"; \
         printf 'fix\\n' >> change.txt; git commit -q -am fix; \
         receipt \"$(git rev-parse HEAD)\"; idle_bg; \
         while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; idle_bg_done; await_exit",
    ));
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("revise", &["add a line"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || supervise_reviewed(&db, &repo, &backend, &reviewer))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"revise_requested")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    wait_for_background(&run);
    thread::sleep(HOLD_PERIOD);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"revise_finished"), "{kinds:?}");
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
}

/// A revise the session will not finish (it goes idle without rewriting the
/// receipt) ends in `/exit`; a session that holds that `/exit` back past the
/// exit timeout raises one `stuck_exit` ask, closed by the runtime once the
/// session exits, and the run then goes on to its `approve_landing` ask.
#[test]
fn a_revise_session_that_holds_exit_back_raises_a_stuck_exit_ask() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
             while [ ! -f \"$MESSAGE\" ]; do sleep 0.05; done; idle; {HOLD}"
        ),
    );
    backend.exit_timeout = Duration::from_secs(1);
    let backend = Arc::new(backend);
    let reviewer = Arc::new(TestReviewer::new(&[verdict(
        "revise",
        &["add a line"],
        "one gap",
    )]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || supervise_reviewed(&db, &repo, &backend, &reviewer))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    thread::sleep(HOLD_PERIOD);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::StuckExit);
    assert!(
        ask.question
            .contains("opens an approve_landing ask for the person once the session exits"),
        "{}",
        ask.question
    );
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "revise_requested") < position(&kinds, "exit_requested"));
    assert!(!kinds.contains(&"revise_finished"), "{kinds:?}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);

    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let closed = queue.read_ask(ask.id).unwrap();
    assert!(closed.closed_at.is_some());
    let open = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].kind, AskKind::ApproveLanding);
}

/// Background work that never ends does not hold the run forever: past the
/// resume timeout from the receipt the run goes on to validation (its
/// `session_idle_observed` saying the work still ran), and past it again the
/// `/exit` goes, where a dialog would become a `stuck_exit` ask.
#[test]
fn background_work_that_never_ends_is_waited_for_up_to_the_resume_timeout() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle_bg; await_exit",
    );
    backend.resume_timeout = Duration::from_secs(1);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let idle = payloads(&detail, "session_idle_observed");
    assert_eq!(idle.len(), 1);
    assert_eq!(idle[0]["background_running"], true);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "session_idle_observed") < position(&kinds, "exit_requested"));
}

/// A reviewer script that moves main in the main checkout with a change to
/// `change.txt` that conflicts with the run's, then passes the run.
fn moving_main_then_pass() -> String {
    format!(
        "cd \"$(git rev-parse --path-format=absolute --git-common-dir)/..\" && \
         printf 'main moved by %s\\n' $$ > change.txt && git add change.txt && \
         git commit -q -m 'main moves' && {}",
        verdict("pass", &[], "meets the acceptance")
    )
}

/// The worker goes idle after its receipt and never exits by itself; each
/// time a conflict request arrives in its terminal it rebases onto the main
/// the request names, resolves `change.txt`, rewrites the receipt and goes
/// idle again, `requests` times.
fn rebasing_agent(requests: usize) -> String {
    format!(
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; {RESUME_PRELUDE}\n\
         for n in $(seq 1 {requests}); do \
           await_message; rm \"$MESSAGE\"; resolve || exit 1; \
           receipt \"$(git rev-parse HEAD)\"; idle; \
         done; await_exit"
    )
}

/// A passed run whose head conflicts with the main that moved during its
/// review is not asked to exit (ADR-0027 decision 4): `git merge-tree` finds
/// the conflict without touching the worktree, `conflict_precheck` is
/// recorded, and the live session gets the resume's resolution request. It
/// rebases and rewrites its receipt; the run is validated and reviewed
/// again, the second precheck finds no conflict, and the run lands without
/// a `needs_session` or a resume.
#[test]
fn a_passed_run_that_conflicts_with_main_is_rebased_by_its_live_session_and_lands() {
    let (_dir, repo, db) = fixture();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &rebasing_agent(1));
    let reviewer = TestReviewer::new(&[
        moving_main_then_pass(),
        verdict("pass", &[], "still meets it"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let run = detail.runs[0].clone();
    let moved = git_out(&repo, &["rev-parse", "main~1"]);
    assert_eq!(git_out(&repo, &["rev-parse", "main~2"]), seed);
    assert_landed(&repo, &run, "test task", &moved);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "resolved by the resumed session\n"
    );
    let validated: Vec<&Value> = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 2);
    let source = validated[0]["receipt"]["commit"].as_str().unwrap();
    let prechecks = payloads(&detail, "conflict_precheck");
    assert_eq!(prechecks.len(), 1);
    let precheck = prechecks[0];
    assert_eq!(precheck["main"], json!(moved));
    // The head that passed, untouched by the precheck.
    assert_eq!(precheck["head"], json!(source));
    assert_eq!(precheck["merge_base"], json!(seed));
    assert_eq!(precheck["conflicts"], json!(["change.txt"]));
    assert_eq!(precheck["attempt"], 1);
    assert_eq!(precheck["requested"], true);
    assert!(precheck["sent_at"].is_i64());
    let resolved = payloads(&detail, "conflict_resolved");
    let head = git_out(
        &repo,
        &["rev-parse", &format!("refs/dagq/runs/{}", run.id())],
    );
    assert_eq!(resolved, [&json!({"attempt": 1, "head": head})]);
    assert_eq!(
        git_out(&repo, &["rev-parse", &format!("{head}~1")]),
        moved,
        "the session rebased onto the main the request named"
    );
    let verdicts: Vec<&Value> = payloads(&detail, "review_finished")
        .iter()
        .map(|p| &p["verdict"])
        .collect();
    assert_eq!(verdicts, [&json!("pass"), &json!("pass")]);
    let kinds = event_kinds(&detail);
    for (earlier, later) in [
        ("review_finished", "conflict_precheck"),
        ("conflict_precheck", "conflict_resolved"),
        ("conflict_resolved", "exit_requested"),
        ("exit_requested", "workspace_closed"),
        ("workspace_closed", "integration_started"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    for absent in ["resume_started", "integration_deferred", "revise_requested"] {
        assert!(!kinds.contains(&absent), "{absent} in {kinds:?}");
    }
    assert_eq!(payloads(&detail, "integration_started").len(), 1);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
    // The request is the resume's, for the live session.
    let texts = backend.texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0].0, WORKSPACE_ID);
    let text = &texts[0].1;
    for expected in [
        format!(
            "dagq: the supervisor's review of run {} (task 1) passed, but integrate would conflict with main, so the run was not landed.",
            run.id()
        ),
        format!(
            "Reason: git merge-tree finds that main {moved} conflicts with the run in change.txt"
        ),
        format!("main is now {moved} (your base commit was {seed})."),
        "Tasks landed on main since your base: none.".to_owned(),
        format!("1. In this worktree run git rebase {moved} and resolve the conflicts."),
        "[\"test -f seed.txt\"]".to_owned(),
        "3. Keep the worktree clean.".to_owned(),
        runtime::STOP_BACKGROUND.to_owned(),
        format!(
            "Rewrite the receipt at {} with the new head commit",
            run.receipt_path().unwrap()
        ),
        "Do not merge or push. When done, report briefly and stop; do not run /exit.".to_owned(),
    ] {
        assert!(text.contains(&expected), "{expected:?} not in {text}");
    }
    let run_dir = Path::new(run.run_dir().unwrap());
    assert_eq!(
        &fs::read_to_string(run_dir.join("conflict-1.txt")).unwrap(),
        text
    );
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(run_attention_of(&runtime::status(&db).unwrap(), run.id()).is_none());
}

/// A passed run that merges cleanly with main is not sent anything: no
/// `conflict_precheck`, one `/exit`, and the landing, as before.
#[test]
fn a_passed_run_that_merges_cleanly_with_main_lands_without_a_request() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    // Main moves during the review, but in another file.
    let reviewer = TestReviewer::new(&[format!(
        "cd \"$(git rev-parse --path-format=absolute --git-common-dir)/..\" && \
         printf 'other\\n' > other.txt && git add other.txt && \
         git commit -q -m 'main moves elsewhere' && {}",
        verdict("pass", &[], "meets the acceptance")
    )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let moved = git_out(&repo, &["rev-parse", "main~1"]);
    assert_landed(&repo, &detail.runs[0], "test task", &moved);
    assert!(repo.join("other.txt").is_file());
    assert!(payloads(&detail, "conflict_precheck").is_empty());
    assert!(backend.texts().is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(reviewer.prompts().len(), 1);
}

/// Conflict requests and resumes share the run's `MAX_RESUME_ATTEMPTS`:
/// when main keeps moving into the run, the precheck after the third
/// request exits the session and opens an `approve_landing` ask instead of
/// a fourth request.
#[test]
fn conflict_requests_past_the_limit_ask_a_person() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, &rebasing_agent(3));
    let reviewer = TestReviewer::new(&[moving_main_then_pass()]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    let prechecks = payloads(&detail, "conflict_precheck");
    let requested: Vec<&Value> = prechecks.iter().map(|p| &p["requested"]).collect();
    assert_eq!(
        requested,
        [&json!(true), &json!(true), &json!(true), &json!(false)]
    );
    assert_eq!(prechecks[3]["attempt"], 4);
    assert!(prechecks[3].get("sent_at").is_none());
    assert!(
        prechecks[3]["asked"]
            .as_str()
            .unwrap()
            .ends_with("after 3 conflict requests and 0 resumes")
    );
    assert_eq!(payloads(&detail, "conflict_resolved").len(), 3);
    assert_eq!(payloads(&detail, "review_finished").len(), 4);
    assert_eq!(backend.texts().len(), 3);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
    assert!(queue.run_leases().unwrap().is_empty());
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "workspace_closed") < position(&kinds, "ask_opened"));
    assert!(!kinds.contains(&"integration_started"), "{kinds:?}");
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    let ask = &asks[0];
    assert_eq!(ask.kind, dagq::domain::AskKind::ApproveLanding);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert!(
        ask.question.contains("returned pass (git merge-tree finds that main")
            && ask
                .question
                .contains("conflicts with the run in change.txt, after 3 conflict requests and 0 resumes): meets the acceptance"),
        "{}",
        ask.question
    );
}

/// Runs task 1 under a supervisor that died after it recorded a request to
/// the live session and typed it (the test writes the text the session
/// reads), with `events` as what it recorded after the validation, and
/// lets another supervisor adopt the run with `reviewer`. Returns the
/// adopted run's detail once the supervisor returns.
fn adopt_pending_request(
    repo: &Path,
    db: &Path,
    backend: &TestWorkspace,
    reviewer: &TestReviewer,
    events: impl FnOnce(&str, i64) -> Vec<(&'static str, Value)>,
    message: impl FnOnce() -> String,
) -> dagq::domain::TaskDetail {
    let run = start_run_under_dead_supervisor(repo, db, backend, "dead-supervisor");
    let idle = run.idle_marker_path().unwrap();
    wait_until(db, Duration::from_secs(20), |_| idle.is_file());
    let head = git_out(
        Path::new(run.worktree_path().unwrap()),
        &["rev-parse", "HEAD"],
    );
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), head],
        )
        .unwrap();
    // The request is sent a second after the session's idle marker, which
    // then predates it.
    thread::sleep(Duration::from_millis(1100));
    let sent_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut queue = SqliteQueue::open(db).unwrap();
    for (kind, payload) in events(&head, sent_at) {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    fs::write(resume_message_path(run.run_dir().unwrap()), message()).unwrap();
    age_lease(db, &run, 31);
    let outcome = supervise_reviewed(db, repo, backend, reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(adoption_events(&detail).len(), 1);
    detail
}

/// A conflict request recorded as sent (`requested: true`, recorded before
/// the text is typed) is not sent again by the supervisor that adopts the
/// run: it waits for the live session to resolve it, then validates,
/// reviews, and lands the run.
#[test]
fn an_adopted_run_with_a_pending_conflict_request_waits_without_sending_it_again() {
    let (_dir, repo, db) = fixture();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &rebasing_agent(1));
    fs::write(repo.join("change.txt"), "main moved\n").unwrap();
    git(&repo, &["add", "change.txt"]);
    git(&repo, &["commit", "-q", "-m", "main moves"]);
    let moved = git_out(&repo, &["rev-parse", "main"]);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "still meets it")]);
    let detail = adopt_pending_request(
        &repo,
        &db,
        &backend,
        &reviewer,
        |head, sent_at| {
            vec![
                (
                    "validation_finished",
                    json!({"status": "awaiting_integration"}),
                ),
                ("review_started", json!({"attempt": 1})),
                (
                    "review_finished",
                    json!({"verdict": "pass", "reasons": [], "summary": "ok", "attempt": 1}),
                ),
                (
                    "conflict_precheck",
                    json!({
                        "code": "rebase_conflict",
                        "main": moved,
                        "head": head,
                        "conflicts": ["change.txt"],
                        "attempt": 1,
                        "requested": true,
                        "sent_at": sent_at,
                    }),
                ),
            ]
        },
        || format!("main is now {moved} (your base commit was {seed})."),
    );
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &moved);
    assert!(backend.texts().is_empty(), "{:?}", backend.texts());
    let prechecks = payloads(&detail, "conflict_precheck");
    assert_eq!(prechecks.len(), 1, "{prechecks:?}");
    let head = git_out(
        &repo,
        &["rev-parse", &format!("refs/dagq/runs/{}", run.id())],
    );
    assert_eq!(
        payloads(&detail, "conflict_resolved"),
        [&json!({"attempt": 1, "head": head})]
    );
    assert_eq!(reviewer.prompts().len(), 1);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert!(!event_kinds(&detail).contains(&"resume_started"));
}

/// A revise request recorded as sent is not sent again by the supervisor
/// that adopts the run either: the live session's rewritten receipt is
/// validated, reviewed, and landed.
#[test]
fn an_adopted_run_with_a_pending_revise_waits_without_sending_it_again() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &revising_agent(1));
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fixed")]);
    let detail = adopt_pending_request(
        &repo,
        &db,
        &backend,
        &reviewer,
        |_, sent_at| {
            vec![
                (
                    "validation_finished",
                    json!({"status": "awaiting_integration"}),
                ),
                ("review_started", json!({"attempt": 1})),
                (
                    "review_finished",
                    json!({"verdict": "revise", "reasons": ["add a line"], "summary": "one gap", "attempt": 1}),
                ),
                (
                    "revise_requested",
                    json!({"attempt": 1, "reasons": ["add a line"], "sent_at": sent_at}),
                ),
            ]
        },
        || "dagq: the supervisor's review asks for changes (revise 1 of 2).".to_owned(),
    );
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        format!("change by {}\nfix 1\n", run.id())
    );
    assert!(backend.texts().is_empty(), "{:?}", backend.texts());
    assert_eq!(payloads(&detail, "revise_requested").len(), 1);
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    assert_eq!(reviewer.prompts().len(), 1);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
}

/// A revise request that cannot be typed is withdrawn: the
/// `revise_requested` recorded before the send is followed by
/// `revise_unsent`, and a person is asked after the session's `/exit`.
#[test]
fn a_revise_that_cannot_be_sent_is_withdrawn_and_asks_a_person() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.text_fails = true;
    let reviewer = TestReviewer::new(&[verdict("revise", &["add a line"], "one gap")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "revise_requested") < position(&kinds, "revise_unsent"));
    assert!(position(&kinds, "revise_unsent") < position(&kinds, "exit_requested"));
    let unsent = payloads(&detail, "revise_unsent");
    assert_eq!(unsent[0]["attempt"], 1);
    assert!(
        unsent[0]["error"]
            .as_str()
            .unwrap()
            .starts_with("the revise request could not be sent")
    );
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind, dagq::domain::AskKind::ApproveLanding);
}

/// A conflict request that cannot be typed is withdrawn by a
/// `conflict_precheck` with `unsent: true`, and the run lands as without a
/// session to ask: the rebase conflicts and parks it for a resume.
#[test]
fn a_conflict_request_that_cannot_be_sent_is_withdrawn_and_the_run_lands() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.text_fails = true;
    let reviewer = TestReviewer::new(&[moving_main_then_pass()]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    // The test backend has no resume script: the parked run stays parked.
    assert_eq!(outcome["runs"][0]["status"], "needs_session", "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let prechecks = payloads(&detail, "conflict_precheck");
    assert_eq!(prechecks.len(), 2, "{prechecks:?}");
    assert_eq!(prechecks[0]["requested"], true);
    assert_eq!(prechecks[0]["conflicts"], json!(["change.txt"]));
    assert_eq!(prechecks[1]["requested"], false);
    assert_eq!(prechecks[1]["unsent"], true);
    assert_eq!(prechecks[1]["attempt"], 1);
    assert!(prechecks[1].get("conflicts").is_none());
    assert!(
        prechecks[1]["error"]
            .as_str()
            .unwrap()
            .starts_with("the request could not be sent")
    );
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "conflict_precheck") < position(&kinds, "exit_requested"));
    assert!(position(&kinds, "exit_requested") < position(&kinds, "integration_started"));
    assert!(!kinds.contains(&"conflict_resolved"), "{kinds:?}");
}

/// A supervisor that adopts a run after a withdrawn revise request
/// (`revise_unsent`) does not send it: it exits the session and asks a
/// person, as the supervisor that could not send it was doing.
#[test]
fn an_adopted_run_with_a_withdrawn_revise_asks_a_person_without_sending_it() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("concern", &["x"], "never")]);
    let detail = adopt_pending_request(
        &repo,
        &db,
        &backend,
        &reviewer,
        |_, sent_at| {
            vec![
                (
                    "validation_finished",
                    json!({"status": "awaiting_integration"}),
                ),
                ("review_started", json!({"attempt": 1})),
                (
                    "review_finished",
                    json!({"verdict": "revise", "reasons": ["add a line"], "summary": "one gap", "attempt": 1}),
                ),
                (
                    "revise_requested",
                    json!({"attempt": 1, "reasons": ["add a line"], "sent_at": sent_at}),
                ),
                (
                    "revise_unsent",
                    json!({"attempt": 1, "error": "the revise request could not be sent: injected"}),
                ),
            ]
        },
        String::new,
    );
    assert!(backend.texts().is_empty(), "{:?}", backend.texts());
    assert!(reviewer.prompts().is_empty(), "reviewed again");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let queue = SqliteQueue::open(&db).unwrap();
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind, dagq::domain::AskKind::ApproveLanding);
    assert!(
        asks[0]
            .question
            .contains("the revise request could not be sent: injected"),
        "{}",
        asks[0].question
    );
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
}
