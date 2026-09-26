//! Runtime tests: Idle sessions without a receipt: the nudge and the stalled ask.
use crate::runtime_support;

use runtime_support::*;

/// Supervisor options whose receipt-less idle threshold is one second.
fn stall_options() -> SuperviseOptions {
    SuperviseOptions {
        stall: Some(dagq::domain::stall::StallConfig {
            idle_without_receipt_secs: 1,
            ..Default::default()
        }),
        ..supervise_options(4, true)
    }
}

/// The payloads of the task-less `stall_config_loaded` events.
fn stall_configs(db: &Path) -> Vec<Value> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT payload FROM run_events WHERE kind='stall_config_loaded' ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
        .collect()
}

/// Task 182: the worker committed and stopped while its background
/// `cargo test` ran, without a receipt. Past the threshold the supervisor
/// types one nudge (recorded with the background work), the session
/// answers it with its receipt, and the nudge is recorded as what resolved
/// it. No ask opens.
#[test]
fn a_receiptless_idle_is_nudged_once_and_the_receipt_resolves_it() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        r#"
commit work; idle_bg
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
cp "$MESSAGE" "$MESSAGE.seen"
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#,
    );
    let outcome = supervise_with(&db, &repo, &backend, &stall_options()).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    let texts = backend.texts();
    assert_eq!(texts.len(), 1, "{texts:?}");
    let nudge = &texts[0].1;
    assert!(nudge.contains(run.id().as_str()), "{nudge}");
    assert!(nudge.contains("without a receipt"), "{nudge}");
    assert!(nudge.contains("- cargo test: cargo test"), "{nudge}");
    assert!(nudge.contains("--kind worker_question"), "{nudge}");
    let nudged = payloads(&detail, "stall_nudged");
    assert_eq!(nudged.len(), 1, "{nudged:?}");
    assert_eq!(nudged[0]["phase"], "session");
    assert_eq!(nudged[0]["threshold_secs"], 1);
    assert_eq!(nudged[0]["background_running"], true);
    assert_eq!(nudged[0]["background_tasks"][0]["command"], "cargo test");
    assert!(nudged[0]["idle_secs"].as_i64().unwrap() >= 1);
    let resolved = payloads(&detail, "stall_resolved");
    assert_eq!(resolved.len(), 1, "{resolved:?}");
    assert_eq!(resolved[0]["detection"], "nudge");
    assert_eq!(resolved[0]["outcome"], "resolved_by_nudge");
    assert_eq!(resolved[0]["threshold"], "idle_without_receipt_secs");
    assert_eq!(resolved[0]["threshold_secs"], 1);
    assert!(stalled_asks(&queue).is_empty());
    // The supervisor recorded the thresholds it ran with.
    let configs = stall_configs(&db);
    assert_eq!(configs.len(), 1);
    assert_eq!(configs[0]["idle_without_receipt_secs"], 1);
    assert_eq!(configs[0]["send_confirm_secs"], 60);
    assert_eq!(configs[0]["background_alert_secs"], 1800);
}

/// A session idle without a receipt because its login ran out is neither
/// nudged nor raised as `stalled`: it joins the authentication ask
/// (ADR-0047 decision 42). Once that is answered, the idle counts again and
/// the nudge tells the session to go on.
#[test]
fn an_idle_session_at_a_login_that_ran_out_waits_in_the_authentication_ask() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        r#"
commit work; idle
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#,
    );
    *backend.screen.lock().unwrap() = LOGIN_SCREEN.into();
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &stall_options()))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"auth_required")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    // Well past the threshold, still no nudge and no stalled ask.
    thread::sleep(Duration::from_millis(1500));
    assert!(backend.texts().is_empty());
    assert!(stalled_asks(&queue).is_empty());
    let hold = queue
        .asks(AskQuery {
            open: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hold.len(), 1, "{hold:?}");
    assert_eq!(
        hold[0].reason_category,
        dagq::domain::AskReason::Authentication
    );
    // The error stays on the screen after the person logged in: the
    // answered ask is not opened again, and the nudge goes out.
    queue.answer(hold[0].id, "done").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.texts().len(), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "auth_required").len(), 1);
    assert_eq!(payloads(&detail, "stall_nudged").len(), 1);
    let asks = queue
        .asks(AskQuery {
            all: true,
            ..Default::default()
        })
        .unwrap();
    let holds = asks.iter().filter(|a| a.kind == AskKind::QueueHold).count();
    assert_eq!(holds, 1, "{asks:?}");
}

/// Task 182 to the end: the session takes the nudge and stops again with
/// its background work still running, so one `stalled` ask opens with the
/// background work and the screen. `wait` closes it and counts again, so
/// a second one opens without a second nudge; `intervene` leaves it to a
/// person and no third one follows. The receipt closes the answered ask.
#[test]
fn a_session_idle_after_its_nudge_gets_one_stalled_ask_and_its_answers_are_applied() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        r#"
commit work; idle_bg
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
idle_bg
while [ ! -f "$EXIT.go" ]; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#,
    ));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &stall_options()))
    };
    let mut queue = SqliteQueue::open(&db).unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        !stalled_asks(queue).is_empty()
    });
    let first = stalled_asks(&queue).remove(0);
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(first.run_id.as_ref(), Some(run.id()));
    assert_eq!(first.asked_by, "supervisor");
    assert_eq!(first.options, ["wait", "intervene"]);
    for part in [
        "idle_without_receipt",
        "- cargo test (b1): cargo test",
        "? for shortcuts",
        "`intervene`",
    ] {
        assert!(first.question.contains(part), "{part}: {}", first.question);
    }
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);
    // Not asked twice, nor nudged again.
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(stalled_asks(&queue).len(), 1);
    assert_eq!(backend.texts().len(), 1);

    queue.answer(first.id, "wait").unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        stalled_asks(queue).len() == 2
    });
    assert!(queue.read_ask(first.id).unwrap().closed_at.is_some());
    let second = stalled_asks(&queue).remove(1);
    assert!(second.is_open());
    assert_eq!(backend.texts().len(), 1);

    queue.answer(second.id, "intervene").unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        payloads(&queue.show(TaskId::new(1)).unwrap(), "stall_resolved").len() == 3
    });
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(stalled_asks(&queue).len(), 2);
    assert!(queue.read_ask(second.id).unwrap().closed_at.is_none());
    assert_eq!(
        ask_attention(&runtime::status(&db).unwrap(), second.id)[0]["next"],
        format!("read the answer of ask {} and close it", second.id)
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
    let closed = queue.read_ask(second.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(closed.answer.as_deref(), Some("intervene"));
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "stall_nudged").len(), 1);
    let resolved: Vec<(&Value, &Value, &Value)> = payloads(&detail, "stall_resolved")
        .into_iter()
        .map(|p| (&p["detection"], &p["outcome"], &p["threshold_secs"]))
        .collect();
    assert_eq!(
        resolved,
        [
            (&json!("nudge"), &json!("escalated"), &json!(1)),
            (&json!("ask"), &json!("answered_wait"), &json!(1)),
            (&json!("ask"), &json!("answered_intervene"), &json!(1)),
        ]
    );
}

/// A worker idle at its own `worker_question` waits for a person, not
/// stalled: nothing is typed until the answer, and no nudge follows it.
#[test]
fn a_worker_idle_at_its_question_is_not_nudged() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        r#"
"$DAGQ" --db "$DB" ask --run "$RUN_ID" --kind worker_question --because scope --question 'Which word?' --cmux /usr/bin/true > /dev/null || exit 70
idle
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
commit work; receipt "$(git rev-parse HEAD)"; idle; await_exit
"#,
    ));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &stall_options()))
    };
    let mut queue = SqliteQueue::open(&db).unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(Default::default()).unwrap().is_empty()
    });
    let ask = queue.asks(Default::default()).unwrap().remove(0);
    thread::sleep(Duration::from_millis(2500));
    assert!(backend.texts().is_empty(), "{:?}", backend.texts());
    queue.answer(ask.id, "blue").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.texts().len(), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(payloads(&detail, "stall_nudged").is_empty());
    assert!(payloads(&detail, "stall_resolved").is_empty());
    assert!(stalled_asks(&queue).is_empty());
}

/// The supervisor that nudged the session and opened its `stalled` ask
/// died: the adopter types nothing and asks nothing again, and closes the
/// ask itself once the session moves on with its receipt.
#[test]
fn an_adopted_stalled_session_is_neither_nudged_nor_asked_again() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        r#"
commit work; idle_bg
while [ ! -f "$EXIT.go" ]; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#,
    ));
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let marker = run.idle_marker_path().unwrap();
    wait_until(&db, Duration::from_secs(30), |_| marker.exists());
    thread::sleep(Duration::from_millis(1100));
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_runtime_event(
            run.id(),
            "stall_nudged",
            json!({"phase": "session", "idle_secs": 1200, "threshold_secs": 1200}),
        )
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            "stall_resolved",
            json!({"phase": "session", "detection": "nudge", "outcome": "escalated"}),
        )
        .unwrap();
    let asked = queue
        .ask(NewAsk {
            kind: AskKind::Stalled,
            task_id: None,
            run_id: Some(run.id().clone()),
            question: "the session is idle without a receipt".into(),
            options: vec!["wait".into(), "intervene".into()],
            asked_by: "supervisor".into(),
            reason_category: dagq::domain::AskReason::RecoveryFailed,
            finding_id: None,
        })
        .unwrap()
        .ask;
    age_lease(&db, &run, 31);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &stall_options()))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !adoption_events(&queue.show(TaskId::new(1)).unwrap()).is_empty()
    });
    thread::sleep(Duration::from_millis(2500));
    assert!(backend.texts().is_empty(), "{:?}", backend.texts());
    assert_eq!(stalled_asks(&queue).len(), 1);
    assert!(queue.read_ask(asked.id).unwrap().is_open());

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let closed = queue.read_ask(asked.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(
        closed.answer.as_deref(),
        Some("the session moved on; closed by the runtime")
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "stall_nudged").len(), 1);
    let resolved = payloads(&detail, "stall_resolved");
    assert_eq!(resolved.len(), 2, "{resolved:?}");
    assert_eq!(resolved[1]["detection"], "ask");
    assert_eq!(resolved[1]["outcome"], "resolved_by_itself");
    assert_eq!(resolved[1]["ask_id"], json!(asked.id));
}

/// The inbox answered `wait` and closed the `stalled` ask itself: the
/// supervisor still takes it as `wait` and asks again once the session
/// stays idle, rather than as a person stepping in.
#[test]
fn a_stalled_ask_closed_after_wait_is_asked_again() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        r#"
commit work; idle_bg
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
idle_bg
while [ ! -f "$EXIT.go" ]; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#,
    ));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &stall_options()))
    };
    let mut queue = SqliteQueue::open(&db).unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        !stalled_asks(queue).is_empty()
    });
    let first = stalled_asks(&queue).remove(0);
    queue.answer(first.id, "wait").unwrap();
    queue.close_ask(first.id).unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        stalled_asks(queue).len() == 2
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
    let detail = queue.show(TaskId::new(1)).unwrap();
    let outcomes: Vec<&Value> = payloads(&detail, "stall_resolved")
        .into_iter()
        .map(|p| &p["outcome"])
        .collect();
    assert_eq!(
        outcomes,
        [
            &json!("escalated"),
            &json!("answered_wait"),
            &json!("resolved_by_itself")
        ]
    );
    assert!(
        stalled_asks(&queue)
            .iter()
            .all(|ask| ask.closed_at.is_some())
    );
}
