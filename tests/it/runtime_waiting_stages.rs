//! Runtime tests: runs that wait for a person while their live session
//! fixes a `revise` verdict or resolves what parked them (ADR-0071
//! decisions 1 and 15 to 18).
use crate::{common, runtime_support};

use runtime_support::*;

/// Longer than the resume timeout these tests give a stage: a question
/// held this long would have ended the stage if its clock ran on.
const HELD: Duration = Duration::from_secs(8);

/// The resume timeout of these tests: long enough for a session to ask
/// once its request arrived on a loaded host, shorter than [`HELD`].
const STAGE_TIMEOUT: Duration = Duration::from_secs(6);

/// A worker that asks a `worker_question` while it revises: once the revise
/// request arrives it asks, goes idle, waits for the answer in `$MESSAGE`
/// and commits it.
const REVISE_ASKING_AGENT: &str = r#"
commit work; receipt "$(git rev-parse HEAD)"; idle
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done; rm "$MESSAGE"
"$DAGQ" --db "$DB" ask --run "$RUN_ID" --kind worker_question --because scope --question 'Which line?' --cmux /usr/bin/true > /dev/null || exit 70
idle
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
cp "$MESSAGE" answer.txt; rm "$MESSAGE"
git add answer.txt; git commit -q -m answer
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#;

/// A worker whose change lands beside the others' (`two.txt`).
const BESIDE_AGENT: &str = r#"
printf 'two\n' > two.txt; git add two.txt; git commit -q -m two
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#;

/// A worker that asks at once and commits the answer it got (`first.txt`).
const ASKING_AGENT: &str = r#"
"$DAGQ" --db "$DB" ask --run "$RUN_ID" --kind worker_question --because scope --question 'Which word?' --cmux /usr/bin/true > /dev/null || exit 70
idle
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
cp "$MESSAGE" first.txt; git add first.txt; git commit -q -m answer
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#;

/// A resumed session that asks a `worker_question` once the resolution
/// request arrived (after `$EXIT.gate` when `gated`), goes idle, and
/// resolves the conflict once the answer came.
fn resume_asking(gated: bool) -> String {
    let gate = if gated {
        "while [ ! -f \"$EXIT.gate\" ]; do sleep 0.05; done"
    } else {
        ":"
    };
    format!(
        r#"await_message; rm "$MESSAGE"; {gate}
"$DAGQ" --db "$DB" ask --run "$RUN_ID" --kind worker_question --because scope --question 'Keep which side?' --cmux /usr/bin/true > /dev/null || exit 70
idle
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
resolve; receipt "$(git rev-parse HEAD)"; idle; await_exit
"#
    )
}

fn run_of(queue: &mut SqliteQueue, task: i64) -> Option<TaskRun> {
    queue.show(TaskId::new(task)).unwrap().runs.last().cloned()
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

fn at(kinds: &[String], kind: &str) -> usize {
    kinds
        .iter()
        .position(|k| k == kind)
        .unwrap_or_else(|| panic!("no {kind} in {kinds:?}"))
}

fn supervise_in_thread(
    db: &Path,
    repo: &Path,
    backend: &Arc<TestWorkspace>,
    reviewer: Option<&Arc<TestReviewer>>,
    options: SuperviseOptions,
) -> thread::JoinHandle<Result<Value>> {
    let (db, repo, backend) = (db.to_owned(), repo.to_owned(), backend.clone());
    let reviewer = reviewer.cloned();
    thread::spawn(move || {
        let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
        match reviewer {
            Some(reviewer) => runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            ),
            None => supervise_with(&db, &repo, &backend, &options),
        }
    })
}

/// A session that asks a `worker_question` while it revises waits for the
/// answer outside the one slot (decision 1): another task is claimed and
/// lands meanwhile, and the revise's clock stops, so holding the question
/// past the resume timeout neither ends the revise nor asks a person
/// (decisions 15 and 16). The answer ends the wait; back in its slot the
/// run gets the answer typed, rewrites its receipt and lands.
#[test]
fn a_question_while_revising_waits_outside_the_slot_with_its_clock_stopped() {
    let (_dir, repo, db) = fixture();
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second", &[]);
    let mut backend = TestWorkspace::new(&db, false, REVISE_ASKING_AGENT);
    backend.resume_timeout = STAGE_TIMEOUT;
    backend.script_for(2, BESIDE_AGENT);
    let backend = Arc::new(backend);
    // The first revise, the second task's pass while the first waits, and
    // the pass of the first once answered.
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("revise", &["say which line"], "one gap"),
        verdict("pass", &[], "fine"),
        verdict("pass", &[], "fixed"),
    ]));
    let supervisor = supervise_in_thread(
        &db,
        &repo,
        &backend,
        Some(&reviewer),
        supervise_options(1, true),
    );
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 2).is_some_and(|run| run.status() == RunStatus::Integrated)
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first = run_of(&mut queue, 1).unwrap();
    assert_eq!(first.status(), RunStatus::AwaitingIntegration);
    let ask = open_ask_of(&mut queue, &first, AskKind::WorkerQuestion).unwrap();
    let started = events_of(&db, first.id(), "run_waiting_started");
    assert_eq!(
        started,
        vec![
            json!({"ask_id": ask, "ask_kind": "worker_question", "phase": "revise",
                    "status": "awaiting_integration", "waiting": 1, "limit": 4})
        ]
    );
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        status["waiting"][0]["run_id"],
        json!(first.id()),
        "{status}"
    );
    assert_eq!(status["waiting"][0]["phase"], "revise");
    assert_eq!(status["waiting"][0]["state"], "waiting");
    // Held past the resume timeout: the revise goes on waiting.
    thread::sleep(HELD);
    let kinds = kinds_of(&db, 1);
    assert!(!kinds.iter().any(|k| k == "exit_requested"), "{kinds:?}");
    assert!(!kinds.iter().any(|k| k == "run_waiting_ended"), "{kinds:?}");
    assert!(other_asks(&mut queue, false).iter().all(|a| a.id == ask));

    queue.answer(ask, "the second").unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(
        payloads(&detail, "ask_answered")[0]["runtime_delivers"],
        true
    );
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let first = run_of(&mut queue, 1).unwrap();
    assert_eq!(first.status(), RunStatus::Integrated);
    let ended = events_of(&db, first.id(), "run_waiting_ended");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["cause"], "answered");
    let kinds = kinds_of(&db, 1);
    assert!(at(&kinds, "run_slot_regained") < at(&kinds, "ask_delivered"));
    assert!(at(&kinds, "ask_delivered") < at(&kinds, "revise_finished"));
    assert_eq!(
        fs::read_to_string(repo.join("answer.txt")).unwrap(),
        format!("answer to ask {ask}: the second")
    );
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["waiting"]["started"]["worker_question"], 1, "{stats}");
}

/// A resumed session that asks a `worker_question` waits for the answer
/// outside the one slot with its clock stopped (decisions 1, 15 and 16):
/// the question held past the resume timeout does not get it `/exit`, and
/// another task is claimed meanwhile. Its answer is the runtime's to type
/// (decision 17) once the run is back in a slot; the idle after that
/// answer, with the conflict resolved, ends the resume with its
/// `exit_requested` recorded, and the run lands.
#[test]
fn a_question_while_resuming_waits_outside_the_slot_and_gets_its_answer() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "third", &[]);
    backend.resume_timeout = STAGE_TIMEOUT;
    backend.resume_script_for(2, &resume_asking(false));
    backend.script_for(3, BESIDE_AGENT);
    let backend = Arc::new(backend);
    let supervisor = supervise_in_thread(&db, &repo, &backend, None, supervise_options(1, true));
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 3).is_some_and(|three| queue.run_lease(three.id()).unwrap().is_none())
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = open_ask_of(&mut queue, &run, AskKind::WorkerQuestion).unwrap();
    let started = events_of(&db, run.id(), "run_waiting_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["phase"], "resume");
    assert_eq!(started[0]["status"], "needs_session");
    assert_eq!(started[0]["ask_id"], json!(ask));
    // The third task got the slot the waiting resume left.
    let mut id_of = |task: i64, kind: &str| {
        queue
            .show(TaskId::new(task))
            .unwrap()
            .events
            .iter()
            .find(|e| e.kind == kind)
            .map(|e| e.id)
            .unwrap()
    };
    assert!(id_of(2, "run_waiting_started") < id_of(3, "run_claimed"));
    thread::sleep(HELD);
    let kinds = kinds_of(&db, 2);
    assert!(!kinds.iter().any(|k| k == "resume_finished"), "{kinds:?}");
    let resumed = at(&kinds, "resume_started");
    assert!(
        !kinds[resumed..].iter().any(|k| k == "exit_requested"),
        "{kinds:?}"
    );
    let status = runtime::status(&db).unwrap();
    let waiting = &status["waiting"][0];
    assert_eq!(waiting["run_id"], json!(run.id()), "{status}");
    assert_eq!(waiting["phase"], "resume");
    assert_eq!(waiting["status"], "needs_session");

    queue.answer(ask, "theirs").unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(
        payloads(&detail, "ask_answered")[0]["runtime_delivers"],
        true
    );
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(
        payloads(&detail, "run_waiting_ended")[0]["cause"],
        "answered"
    );
    assert!(
        backend
            .texts()
            .contains(&(workspace_id(2), format!("answer to ask {ask}: theirs"))),
        "{:?}",
        backend.texts()
    );
    let kinds = kinds_of(&db, 2);
    let resumed = at(&kinds, "resume_started");
    let exit = resumed
        + kinds[resumed..]
            .iter()
            .position(|k| k == "exit_requested")
            .unwrap();
    assert!(at(&kinds, "run_slot_regained") < at(&kinds, "ask_delivered"));
    assert!(at(&kinds, "ask_delivered") < exit, "{kinds:?}");
    let requested = payloads(&detail, "exit_requested");
    assert_eq!(requested.last().unwrap()["resume_attempt"], 1);
    assert_eq!(
        payloads(&detail, "resume_finished")[0]["outcome"],
        "resolved"
    );
}

/// With the waits at their limit, a resumed session's question keeps the
/// run in its slot (`run_waiting_deferred`, decision 7), where its clock
/// still stops while the question is open (decisions 15 and 16): no
/// `/exit` past the resume timeout, and the answer typed from the slot
/// lets it resolve and land.
#[test]
fn a_resume_question_past_the_limit_waits_in_its_slot_with_its_clock_stopped() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "third", &[]);
    backend.resume_timeout = STAGE_TIMEOUT;
    backend.resume_script_for(2, &resume_asking(true));
    backend.script_for(3, ASKING_AGENT);
    let backend = Arc::new(backend);
    let options = SuperviseOptions {
        max_waiting: 1,
        ..supervise_options(2, true)
    };
    let supervisor = supervise_in_thread(&db, &repo, &backend, None, options);
    // The third task's first session takes the one wait.
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 3)
            .is_some_and(|three| !events_of(&db, three.id(), "run_waiting_started").is_empty())
    });
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("gate"),
        "",
    )
    .unwrap();
    wait_until(&db, Duration::from_secs(30), |_| {
        !events_of(&db, run.id(), "run_waiting_deferred").is_empty()
    });
    let deferrals = events_of(&db, run.id(), "run_waiting_deferred");
    assert_eq!(deferrals.len(), 1);
    assert_eq!(deferrals[0]["ask_kind"], "worker_question");
    assert_eq!(deferrals[0]["waiting"], 1);
    assert_eq!(deferrals[0]["limit"], 1);
    thread::sleep(HELD);
    let kinds = kinds_of(&db, 2);
    let resumed = at(&kinds, "resume_started");
    assert!(
        !kinds[resumed..].iter().any(|k| k == "exit_requested"),
        "{kinds:?}"
    );
    assert!(events_of(&db, run.id(), "run_waiting_started").is_empty());

    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = open_ask_of(&mut queue, &run, AskKind::WorkerQuestion).unwrap();
    queue.answer(ask, "theirs").unwrap();
    wait_until(&db, Duration::from_secs(60), |queue| {
        queue.run(run.id()).unwrap().status() == RunStatus::Integrated
    });
    let three = run_of(&mut queue, 3).unwrap();
    let other = open_ask_of(&mut queue, &three, AskKind::WorkerQuestion).unwrap();
    queue.answer(other, "blue").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(payloads(&detail, "ask_delivered").len(), 1);
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["waiting"]["deferred"], 1, "{stats}");
}

/// A supervisor that takes over a revising run whose supervisor died
/// adopts its wait from the run's events without a free slot (decision
/// 11): the one slot goes to another task, which lands, while the question
/// stays open; the answer then reaches the live session, and the run lands.
#[test]
fn an_adopted_revise_keeps_waiting_outside_the_slot() {
    let (_dir, repo, db) = fixture();
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second", &[]);
    let backend = TestWorkspace::new(&db, false, REVISE_ASKING_AGENT);
    backend.script_for(2, BESIDE_AGENT);
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let idle = run.idle_marker_path().unwrap();
    wait_until(&db, Duration::from_secs(20), |_| idle.is_file());
    let head = git_out(
        Path::new(run.worktree_path().unwrap()),
        &["rev-parse", "HEAD"],
    );
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), head],
        )
        .unwrap();
    // The request is sent a second after the session's idle marker.
    thread::sleep(Duration::from_millis(1100));
    let sent_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut queue = SqliteQueue::open(&db).unwrap();
    for (kind, payload) in [
        (
            "validation_finished",
            json!({"status": "awaiting_integration"}),
        ),
        ("review_started", json!({"attempt": 1})),
        (
            "review_finished",
            json!({"verdict": "revise", "reasons": ["say which line"], "summary": "one gap", "attempt": 1}),
        ),
        (
            "revise_requested",
            json!({"attempt": 1, "reasons": ["say which line"], "sent_at": sent_at}),
        ),
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    fs::write(resume_message_path(run.run_dir().unwrap()), "revise 1").unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        open_ask_of(queue, &run, AskKind::WorkerQuestion).is_some()
    });
    let ask = open_ask_of(&mut queue, &run, AskKind::WorkerQuestion).unwrap();
    // What the dead supervisor recorded when the run began to wait.
    queue
        .record_runtime_event(
            run.id(),
            "run_waiting_started",
            json!({"ask_id": ask, "ask_kind": "worker_question", "phase": "revise",
                   "status": "awaiting_integration", "waiting": 1, "limit": 4}),
        )
        .unwrap();
    age_lease(&db, &run, 31);
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("pass", &[], "fine"),
        verdict("pass", &[], "fixed"),
    ]));
    let supervisor = supervise_in_thread(
        &db,
        &repo,
        &backend,
        Some(&reviewer),
        supervise_options(1, true),
    );
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 2).is_some_and(|two| two.status() == RunStatus::Integrated)
    });
    assert_eq!(
        adoption_events(&queue.show(TaskId::new(1)).unwrap()).len(),
        1
    );
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["waiting"][0]["run_id"], json!(run.id()), "{status}");
    assert_eq!(status["waiting"][0]["phase"], "revise");
    assert_eq!(status["waiting"][0]["state"], "waiting");
    assert!(queue.read_ask(ask).unwrap().closed_at.is_none());

    queue.answer(ask, "the first").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Integrated);
    assert_eq!(events_of(&db, run.id(), "run_waiting_started").len(), 1);
    assert_eq!(
        events_of(&db, run.id(), "run_waiting_ended")[0]["cause"],
        "answered"
    );
    assert_eq!(
        fs::read_to_string(repo.join("answer.txt")).unwrap(),
        format!("answer to ask {ask}: the first")
    );
}
