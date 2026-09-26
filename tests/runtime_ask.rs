//! Runtime tests: Asks, attention for the inbox, `watch` and `status`.
mod common;
mod runtime_support;

use runtime_support::*;

fn watch_role(
    db: &Path,
    after: Option<i64>,
    timeout: Duration,
    role: dagq::domain::SessionRole,
) -> Value {
    use dagq::watch::{WatchOptions, watch};
    watch(
        db,
        &WatchOptions {
            after: after.map(EventId::new),
            timeout,
            interval: Duration::from_millis(50),
            role: Some(role),
        },
    )
    .unwrap()
}

/// The one notification is `ask`'s (ADR-0022 decision 5): a run reaching
/// awaiting_integration sends none, a new ask sends one to the inbox
/// workspace `up` recorded, and a repeated ask none.
#[test]
fn only_a_new_ask_notifies_and_it_goes_to_the_inbox() {
    use dagq::domain::{AskKind, NewAsk, SessionRole};
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "commit work; receipt \"$(git rev-parse HEAD)\"");
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let run_id = outcome["runs"][0]["id"].as_str().unwrap().to_owned();
    // The supervisor's own ask #1, of the failed stand-in review (task
    // 328), notified; it is closed so that the run can be asked again.
    {
        let mut notifications = backend.notifications.lock().unwrap();
        assert_eq!(notifications.len(), 1, "{notifications:?}");
        assert!(notifications[0].0.ends_with("ask #1 approve_landing"));
        notifications.clear();
    }
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .answer(dagq::domain::AskId::new(1), "withdrawn")
        .unwrap();
    queue.close_ask(dagq::domain::AskId::new(1)).unwrap();

    let new_ask = |question: &str| NewAsk {
        kind: AskKind::ApproveLanding,
        task_id: None,
        run_id: Some(RunId::new(run_id.clone()).unwrap()),
        question: question.into(),
        options: vec!["land".into()],
        asked_by: "worker".into(),
        reason_category: dagq::domain::AskReason::Scope,
        finding_id: None,
    };
    // Without an inbox the notification names no workspace; the bound
    // repository's main checkout names the queue.
    let other = repo.parent().unwrap().join("elsewhere");
    let asked = runtime::ask(&db, &other, new_ask(&"長".repeat(250)), &backend).unwrap();
    assert_eq!(asked["created"], true);
    assert_eq!(asked["notified"], true);
    let repo_name = repo.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(
        *backend.notifications.lock().unwrap(),
        vec![(
            format!("[{repo_name}] ask #2 approve_landing"),
            format!("{}…\ntask 1 run {run_id}", "長".repeat(200)),
            None
        )]
    );
    // The same run and kind again: the open ask, no notification.
    let again = runtime::ask(&db, &repo, new_ask("again"), &backend).unwrap();
    assert_eq!(again["created"], false);
    assert_eq!(again["notified"], false);
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);

    // With the inbox recorded, a new ask goes to its workspace.
    let queue = queue;
    queue
        .register_session_workspace(SessionRole::Inbox, "INBOX-UUID")
        .unwrap();
    let asked = runtime::ask(
        &db,
        &other,
        NewAsk {
            kind: AskKind::Decide,
            task_id: Some(TaskId::new(1)),
            run_id: None,
            question: "which?".into(),
            options: Vec::new(),
            asked_by: "worker".into(),
            reason_category: dagq::domain::AskReason::RecoveryFailed,
            finding_id: None,
        },
        &backend,
    )
    .unwrap();
    assert_eq!(asked["notified"], true);
    assert_eq!(
        backend.notifications.lock().unwrap()[1],
        (
            format!("[{repo_name}] ask #3 decide"),
            "which?\ntask 1".into(),
            Some("INBOX-UUID".into())
        )
    );
    // The observer's blocked ask on no task notifies the inbox too, with
    // the question alone as its body.
    let blocked = runtime::ask(
        &db,
        &other,
        NewAsk {
            kind: AskKind::Blocked,
            task_id: None,
            run_id: None,
            question: "slots idle".into(),
            options: Vec::new(),
            asked_by: "observer".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
        },
        &backend,
    )
    .unwrap();
    assert_eq!(blocked["task_id"], Value::Null);
    assert_eq!(blocked["notified"], true);
    assert_eq!(
        backend.notifications.lock().unwrap()[2],
        (
            format!("[{repo_name}] ask #4 blocked"),
            "slots idle".into(),
            Some("INBOX-UUID".into())
        )
    );
    assert_eq!(backend.notifications.lock().unwrap().len(), 3);
}

#[test]
fn asks_of_a_run_are_attention_for_the_inbox_until_closed() {
    use dagq::domain::{AskKind, NewAsk, SessionRole};
    let (_dir, _repo, db, run) = awaiting_run();
    let mut queue = SqliteQueue::open(&db).unwrap();
    // The ask of the failed stand-in review, closed unanswered: the run
    // falls back to a review by hand (task 328).
    let failed_review = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(failed_review.len(), 1);
    assert_eq!(failed_review[0].kind, AskKind::ApproveLanding);
    queue.answer(failed_review[0].id, "withdrawn").unwrap();
    queue.close_ask(failed_review[0].id).unwrap();
    let before = queue.latest_event_id().unwrap().as_i64();
    // A `decide` ask: the supervisor applies an `approve_landing` answer
    // itself (see a_third_review_that_does_not_pass_asks_a_person_and_land_lands_it).
    let new_ask = |question: &str| NewAsk {
        kind: AskKind::Decide,
        task_id: None,
        run_id: Some(run.id().clone()),
        question: question.into(),
        options: vec!["land".into(), "send back".into()],
        asked_by: "worker".into(),
        reason_category: dagq::domain::AskReason::RecoveryFailed,
        finding_id: None,
    };

    // An inbox watch started before the ask wakes on ask_opened alone.
    let watcher = {
        let db = db.clone();
        thread::spawn(move || {
            watch_role(
                &db,
                Some(before),
                Duration::from_secs(20),
                SessionRole::Inbox,
            )
        })
    };
    let opened = queue.ask(new_ask(&"q".repeat(250))).unwrap();
    assert!(opened.created);
    assert_eq!(opened.ask.task_id, Some(run.task_id()));
    let woke = joined(watcher, "the watch thread to return");
    assert_eq!(
        woke["events"],
        json!([{"id": before + 1, "kind": "ask_opened", "task_id": 1, "run_id": run.id(),
                "ask_id": opened.ask.id, "next": format!("answer ask {}", opened.ask.id),
                "reason_category": "recovery_failed",
                "created_at": woke["events"][0]["created_at"]}])
    );
    assert_eq!(woke["supervisors_changed"], false);
    // The same run and kind is registered once.
    let again = queue.ask(new_ask("other")).unwrap();
    assert!(!again.created);
    assert_eq!(again.ask.id, opened.ask.id);
    assert_eq!(queue.latest_event_id().unwrap().as_i64(), before + 1);

    // status lists the open ask; all attention is the inbox's
    // (ADR-0024 decision 6).
    let status = runtime::status_for(&db, Some(SessionRole::Inbox)).unwrap();
    let ask_attention: Vec<&Value> = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a.get("ask_id").is_some())
        .collect();
    assert_eq!(
        ask_attention,
        [
            &json!({"run_id": run.id(), "task_id": 1, "ask_id": opened.ask.id, "status": "open",
                "kind": "ask_opened", "last_error": null, "reason_category": "recovery_failed",
                "next": format!("answer ask {}", opened.ask.id)})
        ]
    );
    let asks = status["asks"].as_array().unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0]["kind"], "decide");
    assert_eq!(asks[0]["asked_by"], "worker");
    assert_eq!(asks[0]["run_id"], json!(run.id()));
    assert!(asks[0]["age_secs"].as_i64().unwrap() >= 0);
    assert_eq!(
        asks[0]["question"].as_str().unwrap().chars().count(),
        201,
        "200 characters and the ellipsis"
    );
    // The run's own attention keeps the event that brought it there (the
    // stand-in `claude` printed no verdict, so its review failed, and its
    // ask was closed).
    assert_eq!(
        run_attention_of(&status, run.id()).unwrap()["kind"],
        "review_failed"
    );
    assert_eq!(
        run_attention_of(&status, run.id()).unwrap()["next"],
        "review by hand"
    );
    assert!(
        runtime::status_for(&db, Some(SessionRole::Planner)).unwrap()["attention"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // The planner's watch never wakes.
    let quiet = watch_role(
        &db,
        Some(before),
        Duration::from_millis(200),
        SessionRole::Planner,
    );
    assert_eq!(quiet["events"], json!([]));
    assert_eq!(quiet["cursor"], json!(before));

    // The answer is the inbox's attention until the ask is closed.
    let answered = queue.answer(opened.ask.id, "land").unwrap();
    assert_eq!(answered.answer.as_deref(), Some("land"));
    let events = queue.run_events(run.id()).unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.kind, "ask_answered");
    assert_eq!(last.payload["ask_id"], json!(opened.ask.id));
    let woke = watch_role(
        &db,
        Some(before + 1),
        Duration::from_secs(20),
        SessionRole::Inbox,
    );
    assert_eq!(woke["events"][0]["kind"], "ask_answered");
    assert_eq!(
        woke["events"][0]["next"],
        format!("read the answer of ask {} and close it", opened.ask.id)
    );
    let status = runtime::status_for(&db, None).unwrap();
    assert_eq!(status["asks"], json!([]));
    assert!(status["attention"].as_array().unwrap().iter().any(|a| {
        a["kind"] == "ask_answered"
            && a["status"] == "answered"
            && a["ask_id"] == opened.ask.id.as_i64()
    }));
    queue.close_ask(opened.ask.id).unwrap();
    assert!(
        runtime::status(&db).unwrap()["attention"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a.get("ask_id").is_none())
    );
    // A closed ask frees its (run, kind) for a new one.
    assert!(queue.ask(new_ask("again")).unwrap().created);
}

fn watch_for(db: &Path, after: Option<i64>, timeout: Duration) -> Value {
    use dagq::watch::{WatchOptions, watch};
    watch(
        db,
        &WatchOptions {
            after: after.map(EventId::new),
            timeout,
            interval: Duration::from_millis(50),
            role: None,
        },
    )
    .unwrap()
}

/// `watch` in a thread, started once it has read its baseline.
fn spawn_watch(db: &Path, after: Option<i64>) -> thread::JoinHandle<Value> {
    let db = db.to_owned();
    let handle = thread::spawn(move || watch_for(&db, after, Duration::from_secs(20)));
    thread::sleep(Duration::from_millis(300));
    handle
}

#[test]
fn attention_events_are_read_past_a_cursor_and_wake_watch() {
    let (_dir, repo, db, run) = awaiting_run();
    let queue = SqliteQueue::open(&db).unwrap();
    let latest = queue.latest_event_id().unwrap().as_i64();

    // `status` derives the attention from the queue as it is now. The
    // accepted run is the supervisor's to review; the stand-in `claude`
    // printed no verdict, so the review failed and the run waits for a
    // person in the `approve_landing` ask opened with the failure.
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["cursor"], json!(latest));
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    let ask = queue.asks(AskQuery::default()).unwrap()[0].id;
    assert!(
        status["attention"].as_array().unwrap().iter().any(|a| a
            == &json!({
                "run_id": run.id(), "task_id": 1, "ask_id": ask, "status": "open",
                "kind": "ask_opened", "last_error": null, "next": format!("answer ask {ask}"),
                "reason_category": "scope",
            })),
        "{status}"
    );
    // `supervise --once` exited, so nothing supervises the queue.
    assert_eq!(status["attention"][0]["kind"], "supervisor_stopped");
    assert_eq!(status["attention"][0]["next"], "restart supervisor");

    // `events` defaults to attention, compact and without paths.
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    assert_eq!(events["cursor"], json!(latest));
    let listed = events["events"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{events}");
    assert_eq!(listed[0]["kind"], "ask_opened");
    assert_eq!(listed[0]["ask_id"], json!(ask));
    assert_eq!(listed[0]["next"], format!("answer ask {ask}"));
    assert_eq!(listed[0]["run_id"], json!(run.id()));
    let all = dagq::watch::events(&db, EventId::new(0), 1000, true).unwrap();
    let all_events = all["events"].as_array().unwrap();
    assert_eq!(all_events.len() as i64, latest);
    assert_eq!(all["cursor"], json!(latest));
    let ids: Vec<i64> = all_events
        .iter()
        .map(|e| e["id"].as_i64().unwrap())
        .collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "oldest first");
    let text = all.to_string();
    assert!(!text.contains(run.worktree_path().unwrap()), "{text}");
    assert!(!text.contains("\"receipt\""), "{text}");
    // A limit leaves the cursor on the last event returned.
    let page = dagq::watch::events(&db, EventId::new(0), 2, true).unwrap();
    assert_eq!(page["events"].as_array().unwrap().len(), 2);
    assert_eq!(page["cursor"], json!(ids[1]));
    let rest = dagq::watch::events(&db, EventId::new(ids[1]), 1000, true).unwrap();
    assert_eq!(rest["events"][0]["id"], json!(ids[2]));
    assert_eq!(
        dagq::watch::events(&db, EventId::new(latest), 100, false).unwrap(),
        json!({"events": [], "cursor": latest})
    );

    // An attention event already past the cursor returns at once.
    let started = Instant::now();
    let woke = watch_for(&db, Some(0), Duration::from_secs(20));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(woke["events"], events["events"]);
    assert_eq!(woke["cursor"], json!(latest));
    assert_eq!(woke["supervisors_changed"], false);
    assert_eq!(woke["supervisors"], json!([]));
    // Nothing new: the timeout returns empty and keeps the cursor.
    let started = Instant::now();
    let quiet = watch_for(&db, Some(latest), Duration::from_millis(300));
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(
        quiet,
        json!({"events": [], "supervisors_changed": false, "supervisors": [], "cursor": latest})
    );

    // A landing parked for a session the supervisor will resume wakes
    // nobody (ADR-0019) ...
    fs::remove_file(run.receipt_path().unwrap()).unwrap();
    let before = queue.latest_event_id().unwrap().as_i64();
    assert_eq!(
        integrate(&db, 1, &repo).unwrap()["outcome"],
        "needs_session"
    );
    assert_eq!(
        dagq::watch::events(&db, EventId::new(before), 100, false).unwrap()["events"],
        json!([])
    );
    // ... and neither does one whose resumes are used up: the supervisor
    // hands that run to a person as a `decide` ask (ADR-0024's
    // Consequences), whose `ask_opened` is the attention.
    for attempt in 1..=3 {
        queue
            .record_runtime_event(run.id(), "resume_started", json!({"attempt": attempt}))
            .unwrap();
        queue
            .record_runtime_event(
                run.id(),
                "resume_finished",
                json!({"attempt": attempt, "outcome": "unresolved", "status": "needs_session", "exhausted": attempt == 3}),
            )
            .unwrap();
    }
    let before = queue.latest_event_id().unwrap().as_i64();
    assert_eq!(
        integrate(&db, 1, &repo).unwrap()["outcome"],
        "needs_session"
    );
    let deferred = dagq::watch::events(&db, EventId::new(before), 100, true).unwrap();
    let deferred = deferred["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "integration_deferred")
        .unwrap()
        .clone();
    assert_eq!(deferred["status"], "needs_session");
    assert_eq!(deferred.get("next"), None, "{deferred}");
    assert_eq!(
        dagq::watch::events(&db, EventId::new(before), 100, false).unwrap()["events"],
        json!([])
    );
    let latest = queue.latest_event_id().unwrap().as_i64();
    let status = runtime::status(&db).unwrap();
    let parked = run_attention_of(&status, run.id()).unwrap();
    assert_eq!(parked["status"], "needs_session");
    assert_eq!(parked["kind"], "integration_deferred");
    assert_eq!(parked["next"], "resuming (runtime)");
    assert!(
        parked["last_error"]
            .as_str()
            .unwrap()
            .contains("receipt is missing")
    );
    assert_eq!(status["cursor"], json!(latest));

    // A canceled task needs nobody, whatever its last run was.
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE tasks SET status='canceled' WHERE id=1", [])
        .unwrap();
    assert!(run_attention_of(&runtime::status(&db).unwrap(), run.id()).is_none());
}

/// A session ended by a signal (exit 143, SIGTERM) is classified as
/// `session_killed` with its exit code and signal (ADR-0034), and `status`,
/// `show` and `stats` report the code next to the unchanged free text.
#[test]
fn a_session_killed_by_a_signal_is_classified_in_status_show_and_stats() {
    let (_dir, db, detail) = run_agent("commit work; receipt \"$(git rev-parse HEAD)\"; exit 143");
    let run = &detail.runs[0];
    assert_eq!(run.last_error(), Some("session exited with code 143"));
    let finished = payloads(&detail, "supervision_finished");
    assert_eq!(
        finished[0],
        &json!({"status": "failed", "exit_code": 143, "code": "session_killed", "signal": 15})
    );
    let status = runtime::status(&db).unwrap();
    let failed = run_attention_of(&status, run.id()).unwrap();
    assert_eq!(failed["last_error"], "session exited with code 143");
    assert_eq!(failed["last_error_code"], "session_killed");
    let view = dagq::view::task_detail(&detail, 10);
    assert_eq!(
        view["runs"][0]["last_error_code"], "session_killed",
        "{view}"
    );
    assert!(
        view["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["payload"]["code"] == "session_killed"),
        "{view}"
    );
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(
        stats["reason_codes"]["by_code"]["session_killed"], 1,
        "{stats}"
    );
    assert_eq!(
        stats["reason_codes"]["by_kind"]["supervision_finished"],
        json!({"session_killed": 1})
    );
    // `watch` / `events` keep the code in their compact form.
    let events = dagq::watch::events(&db, EventId::new(0), 1000, true).unwrap();
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "supervision_finished" && e["code"] == "session_killed"),
        "{events}"
    );
}

#[test]
fn status_reports_failed_runs_and_unanswered_exit_requests() {
    let (_dir, db, detail) = run_agent("commit work; receipt \"$(git rev-parse HEAD)\"; exit 7");
    let run = &detail.runs[0];
    let status = runtime::status(&db).unwrap();
    let failed = run_attention_of(&status, run.id()).unwrap();
    assert_eq!(failed["status"], "failed");
    // The failed run itself is the supervisor's triage; its triage failed
    // (the stub `claude` prints no verdict), which is a person's.
    assert_eq!(failed["kind"], "triage_failed");
    assert_eq!(failed["last_error"], "session exited with code 7");
    assert_eq!(failed["last_error_code"], "session_exit_code");
    assert_eq!(failed["next"], "triage by hand");
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    assert_eq!(events["events"].as_array().unwrap().len(), 1, "{events}");
    assert_eq!(events["events"][0]["kind"], "triage_failed");
    assert_eq!(events["events"][0]["next"], "triage by hand");
    // Before its triage, the failed run is the supervisor's.
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_events WHERE kind='triage_failed'", [])
        .unwrap();
    let status = runtime::status(&db).unwrap();
    let pending = run_attention_of(&status, run.id()).unwrap();
    assert_eq!(pending["kind"], "failed");
    assert_eq!(pending["next"], "triaging (runtime)");

    // A running run whose /exit request went unanswered, until its session exits.
    let (_dir, repo, db) = fixture();
    let pid = std::process::id();
    let orphan = orphan_run(&repo, &db, "owner", pid, pid);
    assert!(run_attention_of(&runtime::status(&db).unwrap(), orphan.id()).is_none());
    let mut queue = SqliteQueue::open(&db).unwrap();
    let watcher = spawn_watch(&db, None);
    queue
        .record_runtime_event(
            orphan.id(),
            "exit_request_timed_out",
            json!({"workspace_id": "ws-1", "timeout_secs": 120}),
        )
        .unwrap();
    // The timeout alone is no attention: the supervisor's stuck_exit ask is.
    queue
        .ask(NewAsk {
            kind: AskKind::StuckExit,
            task_id: None,
            run_id: Some(orphan.id().clone()),
            question: "send /exit".into(),
            options: Vec::new(),
            asked_by: "supervisor".into(),
            reason_category: dagq::domain::AskReason::RecoveryFailed,
            finding_id: None,
        })
        .unwrap();
    let woke = joined(watcher, "the watch thread to return");
    assert_eq!(woke["events"].as_array().unwrap().len(), 1, "{woke}");
    assert_eq!(woke["events"][0]["kind"], "ask_opened");
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, orphan.id()).is_none(), "{status}");
    queue.wrapper_exited(orphan.id(), pid, 0).unwrap();
    assert!(run_attention_of(&runtime::status(&db).unwrap(), orphan.id()).is_none());
}

#[test]
fn watch_returns_when_supervisor_registrations_or_health_change() {
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let pid = std::process::id();

    // A supervisor registers.
    let watcher = spawn_watch(&db, Some(cursor));
    queue.register_supervisor("first", pid, 2, VERSION).unwrap();
    let woke = joined(watcher, "the watch thread to return");
    assert_eq!(woke["events"], json!([]));
    assert_eq!(woke["supervisors_changed"], true);
    assert_eq!(woke["cursor"], json!(cursor));
    assert_eq!(woke["supervisors"][0]["pid"], json!(pid));
    assert_eq!(woke["supervisors"][0]["stale"], false);
    // A healthy supervisor clears the stopped attention.
    let status = runtime::status(&db).unwrap();
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["next"] != "restart supervisor"),
        "{status}"
    );

    // Its heartbeat goes stale.
    let watcher = spawn_watch(&db, Some(cursor));
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE supervisors SET heartbeat_at=0", [])
        .unwrap();
    let woke = joined(watcher, "the watch thread to return");
    assert_eq!(woke["supervisors_changed"], true);
    assert_eq!(woke["supervisors"][0]["stale"], true);
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["attention"][0]["kind"], "supervisor_stale");
    assert_eq!(status["attention"][0]["status"], "stale");
    assert_eq!(status["attention"][0]["pid"], json!(pid));
    assert_eq!(status["attention"][0]["next"], "restart supervisor");

    // A stale supervisor that stays stale does not wake a watch.
    let quiet = watch_for(&db, Some(cursor), Duration::from_millis(300));
    assert_eq!(quiet["supervisors_changed"], false);

    // Its registration disappears.
    let watcher = spawn_watch(&db, Some(cursor));
    assert!(queue.deregister_supervisor("first").unwrap());
    let woke = joined(watcher, "the watch thread to return");
    assert_eq!(woke["supervisors_changed"], true);
    assert_eq!(woke["supervisors"], json!([]));
    assert_eq!(
        runtime::status(&db).unwrap()["attention"][0]["kind"],
        "supervisor_stopped"
    );
}

#[test]
fn a_follow_up_draft_records_its_origin_and_its_planner_question_is_delivered_by_the_runtime() {
    use dagq::{
        application::{PlannerAnswerRoute, integrate::register_follow_ups},
        domain::{
            DraftOrigin, NewAsk, PLANNER_QUESTION_OPTIONS, PlannerOrigin, PlannerOwner, Submission,
        },
    };
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let mut queue = SqliteQueue::init(&db).unwrap();
    let source = queue
        .add(NewTask {
            title: "source".into(),
            description: String::new(),
            acceptance: "a".into(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            priority: Default::default(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    queue
        .transition(source.id(), TaskAction::BypassReview)
        .unwrap();
    let dagq::domain::ClaimOutcome::Claimed { run } = queue
        .claim(&sha("0123456789abcdef0123456789abcdef01234567"))
        .unwrap()
    else {
        panic!("nothing claimed");
    };
    // integrate registers the draft one follow-up deeper than its source,
    // with where it came from.
    queue.set_follow_up_depth(source.id(), 1).unwrap();
    let registered = register_follow_ups(
        &mut queue,
        &source,
        run.id(),
        Some(&json!([{"title": "follow", "description": "d"}])),
    );
    let draft = registered[0].task_id;
    assert_eq!(queue.follow_up_depth(draft).unwrap(), 2);
    let (origin, material) = queue.draft_origin(draft).unwrap().unwrap();
    assert_eq!(origin, DraftOrigin::FollowUp);
    assert_eq!(material["source_task_id"], source.id().as_i64());
    assert_eq!(material["source_run_id"], run.id().as_str());
    let targets = queue.planner_drafts().unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].task.id(), draft);

    // A planner of the runtime's may not submit it: it has no goal and is
    // two follow-ups from a person.
    queue
        .edit_task(
            draft,
            dagq::domain::TaskEdit {
                acceptance: Some("works".into()),
                verification_commands: Some(vec!["true".into()]),
                ..Default::default()
            },
        )
        .unwrap();
    let runtime_owner = PlannerOwner {
        origin: PlannerOrigin::Runtime,
        workspace_id: Some("RT".into()),
    };
    let refused = queue
        .submit(Submission {
            tasks: vec![draft],
            goals: Vec::new(),
            proposal: None,
            owner: runtime_owner.clone(),
        })
        .unwrap_err()
        .to_string();
    assert!(refused.contains("planner_question"), "{refused}");

    // So it asks; the question keeps the draft from other planners, and its
    // answer goes to a new planner (none works on the draft).
    let asked = queue
        .ask(NewAsk {
            kind: AskKind::PlannerQuestion,
            task_id: Some(draft),
            run_id: None,
            question: "adopt it?".into(),
            options: PLANNER_QUESTION_OPTIONS
                .iter()
                .map(|o| (*o).into())
                .collect(),
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
        })
        .unwrap()
        .ask;
    assert!(queue.planner_drafts().unwrap().is_empty());
    let answered = queue.answer(asked.id, "adopt").unwrap();
    assert_eq!(
        queue.planner_answer_route(&answered).unwrap(),
        PlannerAnswerRoute::NewPlanner
    );
    let status = runtime::status_for(&db, Some(SessionRole::Inbox)).unwrap();
    let attention = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["ask_id"] == asked.id.as_i64())
        .cloned()
        .unwrap();
    assert_eq!(
        attention["next"],
        format!("delivering the answer of ask {} (runtime)", asked.id)
    );
    let events = dagq::watch::events(&db, dagq::domain::EventId::new(0), 100, false).unwrap();
    assert!(
        !events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "ask_answered"),
        "{events}"
    );

    // With the person's adopt the runtime's planner submits it, and the
    // follow-up counts from 0 again.
    queue
        .submit(Submission {
            tasks: vec![draft],
            goals: Vec::new(),
            proposal: None,
            owner: runtime_owner,
        })
        .unwrap();
    assert_eq!(queue.follow_up_depth(draft).unwrap(), 0);
    let adopted: Vec<_> = queue
        .show(draft)
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind == "follow_up_adopted")
        .collect();
    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0].payload["by"], "person");
    assert_eq!(adopted[0].payload["ask_id"], asked.id.as_i64());
    assert_eq!(adopted[0].payload["source_task_id"], source.id().as_i64());
    // The draft moved on: its answer is closed by the runtime, not typed.
    assert_eq!(
        queue.planner_answer_route(&answered).unwrap(),
        PlannerAnswerRoute::Close
    );
}

/// A run the supervisor gives up (here: its wrapper never registers) keeps
/// its status without a lease; with its session gone, the supervisor itself
/// recovers it on its next pass and triages it (ADR-0024 decision 3). One
/// whose session may still live waits for `recover`. A `runtime_error` that
/// releases no lease is only a note.
#[test]
fn an_abandoned_run_is_recovered_and_triaged_by_the_supervisor() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.no_session = true;
    backend.registration_timeout = Duration::from_secs(1);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    let errors = outcome["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{outcome}");
    assert!(
        errors[0]["message"]
            .as_str()
            .unwrap()
            .contains("wrapper did not register within 1 seconds"),
        "{outcome}"
    );
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::Interrupted);
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let events = queue.run_events(run.id()).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    let error = position(&kinds, "runtime_error");
    assert_eq!(events[error].payload["lease_released"], true);
    let recovered = position(&kinds, "run_recovered");
    assert!(error < recovered, "{kinds:?}");
    assert_eq!(events[recovered].payload["by"], "supervisor");
    assert_eq!(events[recovered].payload["previous_status"], "starting");
    assert_eq!(events[recovered].payload["run"]["blockers"], json!([]));
    assert!(recovered < position(&kinds, "triage_started"), "{kinds:?}");
    assert_eq!(
        outcome["triaged"][0]["run_id"],
        json!(run.id()),
        "{outcome}"
    );
    assert_eq!(outcome["triaged"][0]["status"], "interrupted");

    // The stub `claude` prints no verdict: a person triages the run.
    let status = runtime::status(&db).unwrap();
    let waiting = run_attention_of(&status, run.id()).unwrap();
    assert_eq!(waiting["status"], "interrupted");
    assert_eq!(waiting["kind"], "triage_failed");
    assert_eq!(waiting["next"], "triage by hand");
    let woke = watch_for(&db, Some(cursor), Duration::from_secs(20));
    let kinds: Vec<&Value> = woke["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| &e["kind"])
        .collect();
    assert_eq!(
        kinds,
        [&json!("runtime_error"), &json!("triage_failed")],
        "{woke}"
    );

    // A run nobody leases whose session may still live is not recovered:
    // it waits for `recover`.
    add_ready_task(&mut queue, "abandoned", &[]);
    let pid = std::process::id();
    let abandoned = orphan_run(&repo, &db, "owner", pid, pid);
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases WHERE run_id=?1", [&abandoned.id()])
        .unwrap();
    let status = runtime::status(&db).unwrap();
    let still = run_attention_of(&status, abandoned.id()).unwrap();
    assert_eq!(still["next"], "recover run");
    supervise(&db, &repo, &backend).unwrap();
    assert_eq!(
        queue.run(abandoned.id()).unwrap().status(),
        RunStatus::Running
    );
    assert!(
        !queue
            .has_run_event(abandoned.id(), "run_recovered")
            .unwrap()
    );

    // A runtime error recorded on a leased run is not an attention.
    add_ready_task(&mut queue, "noted", &[]);
    let pid = std::process::id();
    let noted = orphan_run(&repo, &db, "owner", pid, pid);
    let cursor = queue.latest_event_id().unwrap().as_i64();
    queue
        .record_runtime_error(noted.id(), "a passing error", &ReasonCode::Other.into())
        .unwrap();
    assert!(run_attention_of(&runtime::status(&db).unwrap(), noted.id()).is_none());
    assert_eq!(
        dagq::watch::events(&db, EventId::new(cursor), 100, false).unwrap()["events"],
        json!([])
    );
    let quiet = watch_for(&db, Some(cursor), Duration::from_millis(300));
    assert_eq!(quiet["events"], json!([]));
}
