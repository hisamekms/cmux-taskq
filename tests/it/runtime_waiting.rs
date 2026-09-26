//! Runtime tests: runs that wait for a person outside the slots (ADR-0062).
use crate::runtime_support;

use runtime_support::*;

/// A worker that asks a `worker_question`, goes idle, and waits for the
/// answer in its terminal (`$MESSAGE`); it commits the answer it got.
const ASKING_AGENT: &str = r#"
"$DAGQ" --db "$DB" ask --run "$RUN_ID" --kind worker_question --because scope --question 'Which word?' --cmux /usr/bin/true > /dev/null || exit 70
idle
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
cp "$MESSAGE" answer.txt
git add answer.txt
git commit -q -m answer
receipt "$(git rev-parse HEAD)"
idle
await_exit
"#;

fn run_of(queue: &mut SqliteQueue, task: i64) -> Option<TaskRun> {
    queue.show(TaskId::new(task)).unwrap().runs.first().cloned()
}

fn open_ask_of(queue: &mut SqliteQueue, run: &TaskRun, kind: AskKind) -> Option<AskId> {
    queue
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.run_id.as_ref() == Some(run.id()) && ask.kind == kind && ask.is_open())
        .map(|ask| ask.id)
}

fn kinds_of(db: &Path, task: i64) -> Vec<String> {
    SqliteQueue::open(db)
        .unwrap()
        .show(TaskId::new(task))
        .unwrap()
        .events
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

fn supervise_in_thread(
    db: &Path,
    repo: &Path,
    backend: &Arc<TestWorkspace>,
    options: SuperviseOptions,
) -> thread::JoinHandle<Result<Value>> {
    let (db, repo, backend) = (db.to_owned(), repo.to_owned(), backend.clone());
    thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
}

/// With one slot, a run whose worker waits for the answer of its
/// `worker_question` leaves the slot and another task is claimed and
/// finished meanwhile (acceptance 1). `status` shows the wait and the
/// slot's use. The answer ends the wait; the run goes back to its free slot
/// and the answer is delivered from there as before (acceptance 3), and
/// `stats` counts the wait (acceptance 6).
#[test]
fn a_run_waiting_for_its_answer_leaves_the_slot_to_another_task() {
    let (_dir, repo, db) = fixture();
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second", &[]);
    let backend = TestWorkspace::new(&db, false, ASKING_AGENT);
    backend.script_for(2, VALID_AGENT);
    let backend = Arc::new(backend);
    let supervisor = supervise_in_thread(&db, &repo, &backend, supervise_options(1, true));
    // The second task runs to its rest while the first one's ask is open.
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 2).is_some_and(|run| queue.run_lease(run.id()).unwrap().is_none())
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first = run_of(&mut queue, 1).unwrap();
    assert_eq!(first.status(), RunStatus::Running);
    let ask = open_ask_of(&mut queue, &first, AskKind::WorkerQuestion).unwrap();
    let started = events_of(&db, first.id(), "run_waiting_started");
    assert_eq!(
        started,
        vec![
            json!({"ask_id": ask, "ask_kind": "worker_question", "phase": "session",
                    "status": "running", "waiting": 1, "limit": 4})
        ]
    );
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        status["supervisors"][0]["slots"],
        json!({"used": 0, "parallel": 1}),
        "{status}"
    );
    assert_eq!(
        status["supervisors"][0]["waiting"],
        json!({"count": 1, "returning": 0, "limit": 4})
    );
    let waiting = &status["waiting"][0];
    assert_eq!(waiting["run_id"], json!(first.id()));
    assert_eq!(waiting["state"], "waiting");
    assert_eq!(waiting["phase"], "session");
    assert_eq!(
        waiting["asks"],
        json!([{"id": ask, "kind": "worker_question"}])
    );
    // Nothing was typed into the waiting session.
    assert!(backend.texts().is_empty());

    queue.answer(ask, "blue").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let first = run_of(&mut queue, 1).unwrap();
    assert_eq!(first.status(), RunStatus::AwaitingIntegration);
    assert_eq!(
        backend.texts(),
        vec![(
            WORKSPACE_ID.to_owned(),
            format!("answer to ask {ask}: blue")
        )]
    );
    let ended = events_of(&db, first.id(), "run_waiting_ended");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["cause"], "answered");
    assert_eq!(ended[0]["ask_id"], json!(ask));
    let regained = events_of(&db, first.id(), "run_slot_regained");
    assert_eq!(regained.len(), 1);
    assert_eq!(regained[0]["over_parallel"], false);
    // The wait came before the delivery.
    let kinds = kinds_of(&db, 1);
    let at = |kind: &str| kinds.iter().position(|k| k == kind).unwrap();
    assert!(at("run_slot_regained") < at("ask_delivered"), "{kinds:?}");
    // Nothing waits any more.
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["waiting"], json!([]));
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["waiting"]["started"]["worker_question"], 1, "{stats}");
    assert_eq!(stats["waiting"]["waited"]["worker_question"]["count"], 1);
    assert_eq!(stats["waiting"]["slot_wait"]["count"], 1);
    assert_eq!(stats["waiting"]["over_parallel"], 0);
    assert_eq!(stats["waiting"]["deferred"], 0);
}

/// `--max-waiting` bounds the waits (acceptance 2): with a limit of one,
/// the second run that asks stays counted in its slot and records
/// `run_waiting_deferred` once; the third task gets the slot the first
/// run left. Once the first run's wait ends, the second one waits.
#[test]
fn the_waits_stay_within_their_limit() {
    let (_dir, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "second", &[]);
        add_ready_task(&mut queue, "third", &[]);
    }
    let backend = TestWorkspace::new(&db, false, ASKING_AGENT);
    backend.script_for(3, VALID_AGENT);
    let backend = Arc::new(backend);
    let options = SuperviseOptions {
        max_waiting: 1,
        ..supervise_options(2, true)
    };
    let supervisor = supervise_in_thread(&db, &repo, &backend, options);
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 3).is_some_and(|run| queue.run_lease(run.id()).unwrap().is_none())
    });
    wait_until(&db, Duration::from_secs(30), |queue| {
        let (Some(one), Some(two)) = (run_of(queue, 1), run_of(queue, 2)) else {
            return false;
        };
        let deferred = |run: &TaskRun| !events_of(&db, run.id(), "run_waiting_deferred").is_empty();
        deferred(&one) || deferred(&two)
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let runs = [
        run_of(&mut queue, 1).unwrap(),
        run_of(&mut queue, 2).unwrap(),
    ];
    let started: Vec<usize> = runs
        .iter()
        .map(|run| events_of(&db, run.id(), "run_waiting_started").len())
        .collect();
    assert_eq!(started.iter().sum::<usize>(), 1, "{started:?}");
    let (waiting, deferred) = if started[0] == 1 {
        (&runs[0], &runs[1])
    } else {
        (&runs[1], &runs[0])
    };
    let deferrals = events_of(&db, deferred.id(), "run_waiting_deferred");
    assert_eq!(deferrals.len(), 1);
    assert_eq!(deferrals[0]["waiting"], 1);
    assert_eq!(deferrals[0]["limit"], 1);
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        status["supervisors"][0]["waiting"],
        json!({"count": 1, "returning": 0, "limit": 1}),
        "{status}"
    );
    assert_eq!(
        status["supervisors"][0]["slots"],
        json!({"used": 1, "parallel": 2})
    );

    // The first wait ends; the deferred run takes its place in the waits.
    let ask = open_ask_of(&mut queue, waiting, AskKind::WorkerQuestion).unwrap();
    queue.answer(ask, "red").unwrap();
    wait_until(&db, Duration::from_secs(30), |_| {
        !events_of(&db, deferred.id(), "run_waiting_started").is_empty()
    });
    let other = open_ask_of(&mut queue, deferred, AskKind::WorkerQuestion).unwrap();
    queue.answer(other, "green").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    for run in &runs {
        assert_eq!(
            run_of(&mut queue, run.task_id().as_i64()).unwrap().status(),
            RunStatus::AwaitingIntegration
        );
    }
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["waiting"]["deferred"], 1, "{stats}");
    assert_eq!(stats["waiting"]["started"]["worker_question"], 2);
}

/// A run whose answer came while its one slot is taken waits to go back,
/// and still counts toward `--max-waiting` (ADR-0071 (f2)): `status`
/// shows it in `waiting.count` and in `returning`, and with the limit of
/// one reached, the run in the slot that asks next is deferred.
#[test]
fn a_returning_run_counts_toward_the_limit() {
    let (_dir, repo, db) = fixture();
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second", &[]);
    let backend = TestWorkspace::new(&db, false, ASKING_AGENT);
    // The second worker asks only once the test lets it.
    backend.script_for(
        2,
        &format!("while [ ! -f \"$EXIT.gate\" ]; do sleep 0.05; done\n{ASKING_AGENT}"),
    );
    let backend = Arc::new(backend);
    let options = SuperviseOptions {
        max_waiting: 1,
        ..supervise_options(1, true)
    };
    let supervisor = supervise_in_thread(&db, &repo, &backend, options);
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 2).is_some()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first = run_of(&mut queue, 1).unwrap();
    let second = run_of(&mut queue, 2).unwrap();
    let ask = open_ask_of(&mut queue, &first, AskKind::WorkerQuestion).unwrap();
    queue.answer(ask, "blue").unwrap();
    wait_until(&db, Duration::from_secs(30), |_| {
        !events_of(&db, first.id(), "run_waiting_ended").is_empty()
    });
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        status["supervisors"][0]["waiting"],
        json!({"count": 1, "returning": 1, "limit": 1}),
        "{status}"
    );
    assert_eq!(
        status["supervisors"][0]["slots"],
        json!({"used": 1, "parallel": 1})
    );
    assert_eq!(status["waiting"][0]["run_id"], json!(first.id()));
    assert_eq!(status["waiting"][0]["state"], "returning");
    assert_eq!(status["waiting"][0]["cause"], "answered");

    // The second worker asks while the returning run fills the limit.
    fs::write(
        exit_request_path(second.run_dir().unwrap()).with_extension("gate"),
        "",
    )
    .unwrap();
    wait_until(&db, Duration::from_secs(30), |_| {
        !events_of(&db, second.id(), "run_waiting_deferred").is_empty()
    });
    let deferrals = events_of(&db, second.id(), "run_waiting_deferred");
    assert_eq!(deferrals.len(), 1);
    assert_eq!(deferrals[0]["waiting"], 1);
    assert_eq!(deferrals[0]["limit"], 1);
    assert!(events_of(&db, second.id(), "run_waiting_started").is_empty());

    let other = open_ask_of(&mut queue, &second, AskKind::WorkerQuestion).unwrap();
    queue.answer(other, "green").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    for task in [1, 2] {
        assert_eq!(
            run_of(&mut queue, task).unwrap().status(),
            RunStatus::AwaitingIntegration
        );
    }
    assert_eq!(events_of(&db, first.id(), "run_slot_regained").len(), 1);
}

/// A session that holds the `/exit` after its verdict back waits for the
/// person outside the slot, as its `stuck_exit` ask says; another task
/// runs meanwhile. When the person lets it exit, the session's end is
/// seen during the wait (acceptance 4) and its ask closed; the run goes
/// back to a slot and on to what follows its exit (acceptance 3).
#[test]
fn a_stuck_exit_waits_outside_the_slot_until_its_session_exits() {
    let (_dir, repo, db) = fixture();
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second", &[]);
    let mut backend = TestWorkspace::new(&db, false, HELD_AGENT);
    backend.exit_timeout = Duration::from_millis(500);
    backend.script_for(2, VALID_AGENT);
    let backend = Arc::new(backend);
    let supervisor = supervise_in_thread(&db, &repo, &backend, supervise_options(1, true));
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 2).is_some_and(|run| queue.run_lease(run.id()).unwrap().is_none())
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first = run_of(&mut queue, 1).unwrap();
    let ask = open_ask_of(&mut queue, &first, AskKind::StuckExit).unwrap();
    let started = events_of(&db, first.id(), "run_waiting_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["ask_kind"], "stuck_exit");
    assert_eq!(started[0]["phase"], "exit");
    assert_eq!(started[0]["status"], "awaiting_integration");
    // The lease stays with the waiting run.
    assert!(queue.run_lease(first.id()).unwrap().is_some());

    release_held_session(first.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(queue.read_ask(ask).unwrap().closed_at.is_some());
    let ended = events_of(&db, first.id(), "run_waiting_ended");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["cause"], "session_exited");
    let kinds = kinds_of(&db, 1);
    let at = |kind: &str| kinds.iter().rposition(|k| k == kind).unwrap();
    assert!(
        at("run_waiting_ended") < at("run_slot_regained"),
        "{kinds:?}"
    );
    // What follows the exit came after the run was back in its slot.
    assert!(at("run_slot_regained") < at("review_failed"), "{kinds:?}");
}

/// A supervisor that hands off keeps the waiting run's lease, and the
/// process that continues it takes the wait over from the run's events
/// without starting it again (acceptance 5); the answer then reaches the
/// worker as before.
#[test]
fn a_wait_is_taken_over_after_a_handoff() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, ASKING_AGENT));
    let supervisor = supervise_in_thread(&db, &repo, &backend, supervise_options(4, true));
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 1)
            .is_some_and(|run| !events_of(&db, run.id(), "run_waiting_started").is_empty())
    });
    let token = SqliteQueue::open(&db).unwrap().supervisors().unwrap()[0]
        .token
        .clone();
    let queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.request_handoff(&token, "/next/dagq").unwrap());
    let outcome = joined(supervisor, "the supervisor asked to hand off").unwrap();
    assert_eq!(outcome["outcome"], "handoff", "{outcome}");
    assert_eq!(outcome["handed_over"], 1);

    let next = supervise_in_thread(
        &db,
        &repo,
        &backend,
        SuperviseOptions {
            handoff_token: Some(token.clone()),
            ..supervise_options(4, true)
        },
    );
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue
            .supervisors()
            .unwrap()
            .first()
            .is_some_and(|r| r.handoff_binary.is_none())
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = run_of(&mut queue, 1).unwrap();
    // Still waiting, under the same wait.
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["waiting"][0]["state"], "waiting", "{status}");
    let ask = open_ask_of(&mut queue, &run, AskKind::WorkerQuestion).unwrap();
    queue.answer(ask, "blue").unwrap();
    let outcome = joined(next, "the supervisor after the handoff").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(
        run_of(&mut queue, 1).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
    assert_eq!(events_of(&db, run.id(), "run_waiting_started").len(), 1);
    assert_eq!(events_of(&db, run.id(), "run_waiting_ended").len(), 1);
    assert_eq!(events_of(&db, run.id(), "run_slot_regained").len(), 1);
    assert_eq!(
        backend.texts(),
        vec![(
            WORKSPACE_ID.to_owned(),
            format!("answer to ask {ask}: blue")
        )]
    );
}

/// `--max-waiting 0` keeps the runs in their slots, as before.
#[test]
fn no_wait_without_a_limit() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, ASKING_AGENT));
    let options = SuperviseOptions {
        max_waiting: 0,
        ..supervise_options(1, true)
    };
    let supervisor = supervise_in_thread(&db, &repo, &backend, options);
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 1)
            .is_some_and(|run| open_ask_of(queue, &run, AskKind::WorkerQuestion).is_some())
    });
    thread::sleep(Duration::from_millis(300));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = run_of(&mut queue, 1).unwrap();
    assert!(events_of(&db, run.id(), "run_waiting_started").is_empty());
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        status["supervisors"][0]["waiting"],
        json!({"count": 0, "returning": 0, "limit": 0}),
        "{status}"
    );
    let ask = open_ask_of(&mut queue, &run, AskKind::WorkerQuestion).unwrap();
    queue.answer(ask, "blue").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}

/// A dialog screen as Claude Code draws it.
const DIALOG_SCREEN: &str = "\
 Auto mode is available

 ❯ 1. Yes, turn on auto mode
   2. No, keep asking

 Esc to cancel
";

/// Sessions that stop at a dialog wait outside the one slot, so both
/// tasks run into it; their screens are read while they wait (no key is
/// sent), and once a person answers the dialog both go back at once,
/// the second past `--parallel` (decision 10), and finish.
#[test]
fn sessions_a_person_moved_go_back_at_once() {
    let (_dir, repo, db) = fixture();
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second", &[]);
    let mut backend = TestWorkspace::new(&db, false, PROMPTED_AGENT);
    backend.prompt_wait = Duration::from_millis(300);
    *backend.screen.lock().unwrap() = DIALOG_SCREEN.into();
    let backend = Arc::new(backend);
    let supervisor = supervise_in_thread(&db, &repo, &backend, supervise_options(1, true));
    wait_until(&db, Duration::from_secs(60), |queue| {
        [1, 2].iter().all(|task| {
            run_of(queue, *task)
                .is_some_and(|run| !events_of(&db, run.id(), "run_waiting_started").is_empty())
        })
    });
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["supervisors"][0]["waiting"]["count"], 2, "{status}");
    *backend.screen.lock().unwrap() = WORK_SCREEN.into();
    wait_until(&db, Duration::from_secs(30), |queue| {
        [1, 2].iter().all(|task| {
            run_of(queue, *task)
                .is_some_and(|run| !events_of(&db, run.id(), "run_slot_regained").is_empty())
        })
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let mut over = 0;
    for task in [1, 2] {
        let run = run_of(&mut queue, task).unwrap();
        let ended = events_of(&db, run.id(), "run_waiting_ended");
        assert_eq!(ended[0]["cause"], "dialog_cleared");
        assert_eq!(ended[0]["ask_kind"], "answer_prompt");
        let regained = events_of(&db, run.id(), "run_slot_regained");
        assert_eq!(regained[0]["slot_wait_secs"], 0);
        over += usize::from(regained[0]["over_parallel"] == true);
        assert!(open_ask_of(&mut queue, &run, AskKind::AnswerPrompt).is_none());
        fs::write(
            exit_request_path(run.run_dir().unwrap()).with_extension("go"),
            "",
        )
        .unwrap();
    }
    assert_eq!(over, 1);
    assert!(backend.keys.lock().unwrap().is_empty());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["waiting"]["over_parallel"], 1, "{stats}");
}

/// A supervisor that took over the run of one that died adopts its wait
/// from the run's events without a free slot (decision 11): the one slot
/// goes to another task meanwhile, and the answer then reaches the worker.
#[test]
fn an_adopted_run_keeps_waiting_outside_the_slot() {
    let (_dir, repo, db) = fixture();
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second", &[]);
    let backend = TestWorkspace::new(&db, false, ASKING_AGENT);
    backend.script_for(2, VALID_AGENT);
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    wait_until(&db, Duration::from_secs(30), |queue| {
        open_ask_of(queue, &run, AskKind::WorkerQuestion).is_some()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = open_ask_of(&mut queue, &run, AskKind::WorkerQuestion).unwrap();
    // What the dead supervisor recorded when the run began to wait.
    queue
        .record_runtime_event(
            run.id(),
            "run_waiting_started",
            json!({"ask_id": ask, "ask_kind": "worker_question", "phase": "session",
                   "status": "running", "waiting": 1, "limit": 4}),
        )
        .unwrap();
    age_lease(&db, &run, 31);
    let supervisor = supervise_in_thread(&db, &repo, &backend, supervise_options(1, true));
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 2).is_some_and(|two| queue.run_lease(two.id()).unwrap().is_none())
    });
    assert_eq!(
        adoption_events(&queue.show(TaskId::new(1)).unwrap()).len(),
        1
    );
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["waiting"][0]["run_id"], json!(run.id()), "{status}");
    assert_eq!(status["waiting"][0]["state"], "waiting");
    queue.answer(ask, "blue").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
    assert_eq!(events_of(&db, run.id(), "run_waiting_started").len(), 1);
    assert_eq!(
        events_of(&db, run.id(), "run_waiting_ended")[0]["cause"],
        "answered"
    );
}

/// Point the run's wrapper row at `pid` with a heartbeat long expired, as
/// if the wrapper stopped heartbeating: the real wrapper's heartbeats no
/// longer match the row.
fn stop_wrapper_heartbeat(db: &Path, run: &TaskRun, pid: u32) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE run_processes SET pid=?2, heartbeat_at=0 WHERE run_id=?1 AND role='wrapper'",
            rusqlite::params![run.id(), pid],
        )
        .unwrap();
}

/// A wrapper that dies during a `stuck_exit` wait without recording its
/// exit (its heartbeat expired and its pid is gone) is detected as the
/// session's end, as `ExitWatch` does (acceptance 4): the `stuck_exit` ask
/// is closed at once, the wait ends `session_exited`, and back in its slot
/// the run goes on to what follows its exit.
#[test]
fn a_wrapper_that_dies_during_a_wait_ends_it_as_the_sessions_end() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, HELD_AGENT);
    backend.exit_timeout = Duration::from_millis(500);
    let backend = Arc::new(backend);
    let supervisor = supervise_in_thread(&db, &repo, &backend, supervise_options(1, true));
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 1)
            .is_some_and(|run| !events_of(&db, run.id(), "run_waiting_started").is_empty())
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = run_of(&mut queue, 1).unwrap();
    let ask = open_ask_of(&mut queue, &run, AskKind::StuckExit).unwrap();
    stop_wrapper_heartbeat(&db, &run, dead_pid());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(queue.read_ask(ask).unwrap().closed_at.is_some());
    let ended = events_of(&db, run.id(), "run_waiting_ended");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["cause"], "session_exited");
    let kinds = kinds_of(&db, 1);
    let at = |kind: &str| kinds.iter().rposition(|k| k == kind).unwrap();
    assert!(
        at("run_waiting_ended") < at("run_slot_regained"),
        "{kinds:?}"
    );
    assert!(at("run_slot_regained") < at("review_failed"), "{kinds:?}");
    assert!(!kinds.iter().any(|k| k == "runtime_error"), "{kinds:?}");
    // The stub session is let go; its wrapper no longer owns the row.
    release_held_session(run.run_dir().unwrap());
}

/// A worker's first session whose wrapper goes silent during a wait (its
/// heartbeat expired while its process lives) is detected as before
/// (acceptance 4): `wrapper_heartbeat_expired` is recorded, the wait ends
/// `wrapper_silent` and the run goes back to its slot at once, where the
/// supervisor sends the session its `/exit`. When the wrapper then dies,
/// the run is given up as before.
#[test]
fn a_wrapper_that_goes_silent_during_a_wait_sends_the_run_back_for_its_exit() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        r#"
"$DAGQ" --db "$DB" ask --run "$RUN_ID" --kind worker_question --because scope --question 'Which word?' --cmux /usr/bin/true > /dev/null || exit 70
idle
await_exit
"#,
    ));
    let supervisor = supervise_in_thread(&db, &repo, &backend, supervise_options(1, true));
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 1)
            .is_some_and(|run| !events_of(&db, run.id(), "run_waiting_started").is_empty())
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = run_of(&mut queue, 1).unwrap();
    let mut silent = sleeper();
    stop_wrapper_heartbeat(&db, &run, silent.id());
    wait_until(&db, Duration::from_secs(30), |_| {
        !events_of(&db, run.id(), "exit_requested").is_empty()
    });
    let ended = events_of(&db, run.id(), "run_waiting_ended");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["cause"], "wrapper_silent");
    let regained = events_of(&db, run.id(), "run_slot_regained");
    assert_eq!(regained.len(), 1);
    assert_eq!(regained[0]["slot_wait_secs"], 0);
    let expired = events_of(&db, run.id(), "wrapper_heartbeat_expired");
    assert_eq!(expired.len(), 1, "{expired:?}");
    assert_eq!(expired[0]["pid"], silent.id());
    let kinds = kinds_of(&db, 1);
    let at = |kind: &str| kinds.iter().position(|k| k == kind).unwrap();
    assert!(at("run_slot_regained") < at("exit_requested"), "{kinds:?}");
    // The wrapper dies without recording its exit: the run is given up.
    silent.kill().unwrap();
    silent.wait().unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["errors"][0]["run_id"], json!(run.id()), "{outcome}");
    assert!(
        outcome["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("wrapper heartbeat expired"),
        "{outcome}"
    );
}
