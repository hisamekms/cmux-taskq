//! Runtime tests: Claim and provisioning, leases, supervisor registrations, backend
//! failures, stats and `recover`, the injected clock and IDs, migrations and
//! the adapters.
use crate::runtime_support;

use runtime_support::*;

#[test]
fn failed_agent_retains_worktree_and_does_not_complete_task() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; exit 7",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "failed");
    // A nonzero session exit is final; the receipt is not validated and the
    // workspace stays open for inspection.
    assert_eq!(outcome["runs"][0]["result_commit"], Value::Null);
    assert_eq!(outcome["runs"][0]["workspace_closed_at"], Value::Null);
    assert_eq!(
        outcome["runs"][0]["last_error"],
        "session exited with code 7"
    );
    assert!(backend.closed().is_empty());
    // A failed run is reported through `watch`, not a notification (ADR-0022).
    assert!(backend.notifications.lock().unwrap().is_empty());
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(
        detail.runs[0].last_error(),
        Some("session exited with code 7")
    );
    assert!(Path::new(detail.runs[0].worktree_path().unwrap()).exists());
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(queue.candidates().unwrap().is_empty());
    // A failed run does not free the task automatically, but a person may give up on it.
    assert_eq!(runtime::doctor(&db, true).unwrap()["runs"], json!([]));
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Canceled
    );
}

#[test]
fn provisioning_failure_retains_the_run_and_stops_claiming_other_tasks() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "untouched", &[]);
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    let error = format!("{:#}", supervise(&db, &repo, &backend).unwrap_err());
    assert!(error.contains("injected workspace"), "{error}");
    assert!(error.contains("claiming stopped"), "{error}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Starting);
    assert!(run.last_error().unwrap().contains("injected workspace"));
    assert!(Path::new(run.worktree_path().unwrap()).exists());
    // The environment is suspect: the second candidate was left alone.
    assert!(queue.show(TaskId::new(2)).unwrap().runs.is_empty());
    assert_eq!(queue.candidates().unwrap()[0].id(), TaskId::new(2));
    // The run is disowned, so nothing has to be stopped before recovering it;
    // the drained loop took its registration with it.
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(queue.supervisors().unwrap().is_empty());
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["supervisors"], json!([]));
    assert_eq!(report["runs"][0]["recoverable"], true);
    assert_eq!(
        runtime::recover(&db, run.id()).unwrap()["run"]["status"],
        "interrupted"
    );
    assert_eq!(queue.show(TaskId::new(1)).unwrap().runs.len(), 1);
}

/// A failed backend call carries the call, the error and the load it
/// failed under: the load average (or null), the supervisor's slots held
/// and its `--parallel` (task 109).
fn assert_backend_failure(
    event: &dagq::domain::RunEvent,
    op: &str,
    workspace: Option<&str>,
    error: &str,
    run_id: &RunId,
) {
    assert_eq!(event.run_id.as_ref(), Some(run_id));
    assert_eq!(event.payload["op"], op, "{:?}", event.payload);
    assert_eq!(event.payload["workspace_id"], json!(workspace));
    assert_eq!(event.payload["timeout_secs"], 30);
    assert!(
        event.payload["error"].as_str().unwrap().contains(error),
        "{:?}",
        event.payload
    );
    assert!(event.payload["load_avg"].is_f64() || event.payload["load_avg"].is_null());
    assert_eq!(event.payload["slots"], 1);
    assert_eq!(event.payload["parallel"], 4);
}

/// cmux failing to create, close or send is recorded as
/// `backend_call_failed` on the run, next to (and before) what the
/// supervisor already recorded for it: the abandon's `runtime_error`, and
/// `cleanup_failed`; `stats` counts them and raises `backend_failures`.
#[test]
fn failed_backend_calls_are_recorded_with_the_load_and_counted_by_stats() {
    // create: the provisioning failure abandons the run.
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap_err();
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let run = &detail.runs[0];
    let failures = backend_failures(&detail);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_backend_failure(
        failures[0],
        "create",
        None,
        "injected workspace creation failure",
        run.id(),
    );
    let abandoned = detail
        .events
        .iter()
        .find(|e| e.kind == "runtime_error")
        .unwrap();
    assert!(failures[0].id < abandoned.id);
    // The abandon carries the backend call's code and op (ADR-0034).
    assert_eq!(failures[0].payload["code"], "backend_failed");
    assert_eq!(abandoned.payload["code"], "backend_failed");
    assert_eq!(abandoned.payload["op"], "create");

    // close: `cleanup_failed` stays as it was, and the failure is recorded too.
    let (_dir, db, detail) = run_agent_with(VALID_AGENT, true);
    let run = &detail.runs[0];
    let failures = backend_failures(&detail);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_backend_failure(
        failures[0],
        "close",
        Some(WORKSPACE_ID),
        "injected workspace close failure",
        run.id(),
    );
    let cleanup = detail
        .events
        .iter()
        .find(|e| e.kind == "cleanup_failed")
        .unwrap();
    assert_eq!(
        cleanup
            .payload
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["code", "message", "op", "workspace_id"]
    );
    assert_eq!(cleanup.payload["code"], "backend_failed");
    assert_eq!(cleanup.payload["op"], "close");
    assert!(failures[0].id < cleanup.id);
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["backend_failures"]["count"], 1, "{stats}");
    assert!(
        !stats["alerts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "backend_failures")
    );

    // send: the /exit that timed out is not typed again, and the run goes
    // on, its screen read for whether the /exit got there (task 326).
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    backend.send_times_out = true;
    let cursor = runtime::status(&db).unwrap()["cursor"].as_i64().unwrap();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let failures = backend_failures(&detail);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_backend_failure(
        failures[0],
        "send_exit",
        Some(WORKSPACE_ID),
        "did not finish within 30s",
        run.id(),
    );
    // cmux's timeout is told apart from its other failures.
    assert_eq!(failures[0].payload["code"], "backend_timeout");
    // One of up to three attempts, not made again: the screen shows the
    // /exit got there (task 354).
    assert_eq!(failures[0].payload["attempt"], 1);
    assert_eq!(failures[0].payload["max_attempts"], 3);
    assert_eq!(failures[0].payload["retry_after_ms"], Value::Null);
    assert!(!detail.events.iter().any(|e| e.kind == "runtime_error"));

    // capture: a timeout is read again after a backoff, each failed
    // attempt recorded with its number and the backoff that followed.
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    backend.capture_timeouts.store(2, Ordering::SeqCst);
    let recording = runtime::RecordingBackend::new(&backend, db.clone(), None);
    assert_eq!(recording.capture(WORKSPACE_ID).unwrap(), READY_SCREEN);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let retried: Vec<_> = backend_failures(&detail)
        .into_iter()
        .filter(|e| e.payload["op"] == "capture")
        .map(|e| {
            (
                e.payload["attempt"].clone(),
                e.payload["max_attempts"].clone(),
                e.payload["retry_after_ms"].clone(),
                e.payload["code"].clone(),
            )
        })
        .collect();
    assert_eq!(
        retried,
        [
            (json!(1), json!(3), json!(10), json!("backend_timeout")),
            (json!(2), json!(3), json!(20), json!("backend_timeout")),
        ]
    );
    // Recorded on the run whose workspace it read.
    assert_eq!(retried.len(), 2);

    // A second failure in the same window is an alert.
    backend.exists_fails = true;
    let recording = runtime::RecordingBackend::new(&backend, db.clone(), None);
    assert!(recording.exists(WORKSPACE_ID).is_err());
    let stats = runtime::stats(
        &db,
        &dagq::domain::stats::StatsQuery {
            since: Some(EventId::new(cursor).into()),
            ..Default::default()
        },
    )
    .unwrap();
    let failures = &stats["backend_failures"];
    assert_eq!(failures["count"], 4, "{stats}");
    assert_eq!(
        failures["by_op"],
        json!({"capture": 2, "exists": 1, "send_exit": 1})
    );
    // The codes of the window, per code and per kind.
    let codes = &stats["reason_codes"];
    // `backend_call_failed` is `backend_failures`' to count, not again here.
    assert_eq!(codes["by_kind"].get("backend_call_failed"), None, "{stats}");
    assert_eq!(failures["max_slots"], 1);
    assert!(failures["max_load_avg"].is_f64() || failures["max_load_avg"].is_null());
    assert!(stats["alerts"].as_array().unwrap().contains(&json!({
        "kind": "backend_failures", "task_id": null, "run_id": null,
        "value": 4, "threshold": 2
    })));
}

/// A workspace group cmux cannot make leaves a warning in the supervisor
/// log, and the run opens outside any group (ADR-0026).
#[test]
fn a_workspace_group_cmux_cannot_make_is_a_logged_warning() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "grouped", &[]);
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.group_fails = true;
    let options = supervise_options(1, true);
    let (telemetry, captured) = Telemetry::capture();
    let outcome = telemetry
        .in_scope(|| supervise_with(&db, &repo, &backend, &options))
        .unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().runs[0].status(),
        RunStatus::AwaitingIntegration
    );
    assert_eq!(backend.tags.lock().unwrap()[0].group, None);
    let log = captured.text();
    assert!(
        log.contains("warning: cmux workspace group")
            && log.contains("workspace-group create failed")
            && log.contains("\"level\":\"WARN\""),
        "{log}"
    );
    // The group belongs to no run, so its failure is recorded without one.
    let failures: Vec<_> = queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "backend_call_failed")
        .collect();
    // One per run workspace opened.
    assert_eq!(failures.len(), backend.groups.lock().unwrap().len());
    for failure in failures {
        assert_eq!(
            (failure.task_id, failure.run_id.as_ref().map(RunId::as_str)),
            (None, None)
        );
        assert_eq!(failure.payload["op"], "ensure_group");
        assert_eq!(failure.payload["slots"], 1);
        assert_eq!(failure.payload["parallel"], 1);
    }
}

/// With one slot the supervisor claims the candidate whose completion
/// releases the most unfinished tasks before the older task 1 (ADR-0023),
/// and records no event for the reordering.
#[test]
fn supervisor_claims_the_candidate_that_releases_the_most_tasks_first() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let root = add_ready_task(&mut queue, "root", &[]);
    let middle = add_ready_task(&mut queue, "middle", &[root]);
    add_ready_task(&mut queue, "leaf", &[middle]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(1, true)).unwrap();
    let claimed: Vec<i64> = outcome["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["task_id"].as_i64().unwrap())
        .collect();
    assert_eq!(claimed, [root.as_i64(), 1]);
    let mut claim_event = |task: TaskId| {
        let detail = queue.show(task).unwrap();
        assert!(!event_kinds(&detail).contains(&"claim_reordered"));
        detail
            .events
            .iter()
            .find(|event| event.kind == "run_claimed")
            .unwrap()
            .id
    };
    assert!(claim_event(root) < claim_event(TaskId::new(1)));
    // The same order ties back to ID once nothing is released.
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(1, true)).unwrap();
    assert_eq!(outcome["runs"][0]["task_id"], 1);
    assert_eq!(outcome["runs"][1]["task_id"], 2);
}

/// The supervisor claims in the order `candidates` and `graph` show: the
/// highest effective priority first, whatever the ID or unblocks, and a
/// candidate that an urgent ready task waits for inherits urgent
/// (ADR-0040 decision 4).
#[test]
fn supervisor_claims_by_effective_priority_like_candidates_and_graph() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let low = add_ready_task(&mut queue, "later", &[]);
    queue.set_priority(low, Priority::Low).unwrap();
    let base = add_ready_task(&mut queue, "base", &[]);
    let waiter = add_ready_task(&mut queue, "urgent waiter", &[base]);
    queue.set_priority(waiter, Priority::Urgent).unwrap();
    let high = add_ready_task(&mut queue, "high", &[]);
    queue.set_priority(high, Priority::High).unwrap();
    let expected = [base, high, TaskId::new(1), low];
    let candidates: Vec<TaskId> = queue.candidates().unwrap().iter().map(|t| t.id()).collect();
    assert_eq!(candidates, expected);
    assert_eq!(
        dependency_graph(queue.graph_input().unwrap(), None).candidates,
        expected
    );
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(1, true)).unwrap();
    let claimed: Vec<TaskId> = outcome["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| TaskId::new(run["task_id"].as_i64().unwrap()))
        .collect();
    assert_eq!(claimed, expected);
}

#[test]
fn claim_creates_a_lease_that_only_its_owner_can_use_or_release() {
    use dagq::{domain::ClaimOutcome, infrastructure::runtime_store::RunPlan};
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.bind_repository("/repo/one/.git").unwrap();
    assert!(queue.bind_repository("/repo/two/.git").is_err());
    let base = "0123456789abcdef0123456789abcdef01234567";
    let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&sha(base), "first").unwrap()
    else {
        panic!()
    };
    assert!(matches!(
        queue.claim_for_supervisor(&sha(base), "first").unwrap(),
        ClaimOutcome::NoReadyTask
    ));
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_eq!(lease.pid, std::process::id());
    assert_eq!(queue.run_leases().unwrap().len(), 1);
    // An idle supervisor heartbeats nothing; the owner heartbeats its runs.
    assert_eq!(queue.heartbeat_leases("second").unwrap(), 0);
    assert_eq!(queue.heartbeat_leases("first").unwrap(), 1);
    let plan = RunPlan {
        repo_path: "/test".into(),
        run_dir: "/run".into(),
        branch: "dagq/test".into(),
        worktree_path: "/run/worktree".into(),
        receipt_path: "/run/receipt.json".into(),
        log_path: "/run/log".into(),
    };
    assert!(queue.plan_run(run.id(), "second", &plan).is_err());
    let raw = Connection::open(&db).unwrap();
    raw.execute("UPDATE run_leases SET heartbeat_at=0", [])
        .unwrap();
    assert!(queue.release_lease(run.id(), "second").is_err());
    assert_eq!(queue.run_lease(run.id()).unwrap().unwrap().heartbeat_at, 0);
    // A stale lease of the writer's own token is renewed, not refused
    // (ADR-0039 decision 7).
    queue.plan_run(run.id(), "first", &plan).unwrap();
    assert!(queue.run_lease(run.id()).unwrap().unwrap().heartbeat_at > 0);
    queue.release_lease(run.id(), "first").unwrap();
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert!(queue.release_lease(run.id(), "first").is_err());
    // The token stays on the run as a record of who executed it.
    let raw_token: String = raw
        .query_row(
            "SELECT supervisor_token FROM task_runs WHERE id=?1",
            [&run.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(raw_token, "first");
}

/// A clock the test moves by hand, in whole seconds.
#[derive(Clone)]
struct ManualClock(Arc<AtomicI64>);

impl ManualClock {
    fn at(secs: i64) -> Self {
        Self(Arc::new(AtomicI64::new(secs)))
    }

    fn set(&self, secs: i64) {
        self.0.store(secs, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn system_time(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.0.load(Ordering::SeqCst) as u64)
    }
}

/// IDs handed out in order.
struct FixedIds(Mutex<Vec<&'static str>>);

impl IdGenerator for FixedIds {
    fn uuid(&self) -> String {
        self.0.lock().unwrap().remove(0).to_owned()
    }
}

#[test]
fn an_injected_clock_decides_lease_staleness_and_injected_ids_name_the_run() {
    use dagq::{
        domain::{ClaimOutcome, HEARTBEAT_TIMEOUT_SECS},
        infrastructure::runtime_store::{RunPlan, lease_is_stale},
    };
    const T: i64 = 1_900_000_000;
    const RUN: &str = "11111111-1111-4111-8111-111111111111";
    let (_dir, _repo, db) = fixture();
    let clock = ManualClock::at(T);
    let mut queue = SqliteQueue::open(&db).unwrap().with_generators(Generators {
        clock: Arc::new(clock.clone()),
        ids: Arc::new(FixedIds(Mutex::new(vec![RUN]))),
    });
    let registration = queue.register_supervisor("first", 1, 1, VERSION).unwrap();
    assert_eq!((registration.started_at, registration.heartbeat_at), (T, T));
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&sha("0123456789abcdef0123456789abcdef01234567"), "first")
        .unwrap()
    else {
        panic!()
    };
    // The run ID, the claim time and the first heartbeat come from the
    // generators, the times in the form the columns always had.
    assert_eq!(run.id().as_str(), RUN);
    assert_eq!(run.created_at(), "2030-03-17T17:46:40.000Z");
    let task = queue.show(run.task_id()).unwrap().task;
    assert_eq!(task.updated_at(), "2030-03-17T17:46:40.000Z");
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_eq!(lease.heartbeat_at, T);
    assert!(!lease_is_stale(&lease, T + HEARTBEAT_TIMEOUT_SECS));
    assert!(lease_is_stale(&lease, T + HEARTBEAT_TIMEOUT_SECS + 1));
    let plan = RunPlan {
        repo_path: "/test".into(),
        run_dir: "/run".into(),
        branch: "dagq/test".into(),
        worktree_path: "/run/worktree".into(),
        receipt_path: "/run/receipt.json".into(),
        log_path: "/run/log".into(),
    };
    // The store stamps heartbeats by the same clock, both the process
    // heartbeat and the renewal of a lease-guarded write.
    clock.set(T + HEARTBEAT_TIMEOUT_SECS + 1);
    assert_eq!(queue.heartbeat("first").unwrap(), 1);
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_eq!(lease.heartbeat_at, T + HEARTBEAT_TIMEOUT_SECS + 1);
    assert_eq!(
        queue.supervisors().unwrap()[0].heartbeat_at,
        T + HEARTBEAT_TIMEOUT_SECS + 1
    );
    clock.set(T + HEARTBEAT_TIMEOUT_SECS + 5);
    queue.plan_run(run.id(), "first", &plan).unwrap();
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_eq!(lease.heartbeat_at, T + HEARTBEAT_TIMEOUT_SECS + 5);
}

/// ADR-0039 decision 7: a host sleep jumps the wall clock 120 s past the
/// last heartbeat between two lease-guarded writes. While the lease row
/// still carries the supervisor's token, the next write renews it and goes
/// on, so no other supervisor adopts the run afterwards. A supervisor that
/// stays asleep until another one adopted its stale lease (the adoption of a
/// dead supervisor's lease works as before) is refused and writes nothing.
#[test]
fn a_lease_of_its_own_token_is_renewed_after_a_host_sleep_until_another_supervisor_adopts_it() {
    use dagq::{
        domain::ClaimOutcome,
        infrastructure::runtime_store::{RunPlan, lease_is_stale},
    };
    const T: i64 = 1_900_000_000;
    const SLEEP: i64 = 120;
    let (_dir, _repo, db) = fixture();
    let clock = ManualClock::at(T);
    let mut queue = SqliteQueue::open(&db).unwrap().with_generators(Generators {
        clock: Arc::new(clock.clone()),
        ids: Arc::new(FixedIds(Mutex::new(vec![
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
        ]))),
    });
    add_ready_task(&mut queue, "second", &[]);
    let plan = |run: &TaskRun| RunPlan {
        repo_path: "/test".into(),
        run_dir: format!("/run/{}", run.id()),
        branch: format!("dagq/{}", run.id()),
        worktree_path: format!("/run/{}/worktree", run.id()),
        receipt_path: format!("/run/{}/receipt.json", run.id()),
        log_path: format!("/run/{}/log", run.id()),
    };
    let base = sha("0123456789abcdef0123456789abcdef01234567");
    let start = |queue: &mut SqliteQueue, token: &str| {
        let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&base, token).unwrap()
        else {
            panic!()
        };
        queue.plan_run(run.id(), token, &plan(&run)).unwrap();
        run
    };

    // The sleeper claims and plans at T, then the host sleeps 120 s.
    let run = start(&mut queue, "sleeper");
    clock.set(T + SLEEP);
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert!(lease_is_stale(&lease, T + SLEEP));
    // Woken up, it goes on with the run: every write renews the lease.
    queue
        .workspace_created(run.id(), "sleeper", "ws-1")
        .unwrap();
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_eq!(
        (lease.token.as_str(), lease.heartbeat_at),
        ("sleeper", T + SLEEP)
    );
    queue
        .register_wrapper(run.id(), "sleeper", std::process::id())
        .unwrap();
    queue
        .register_agent(run.id(), std::process::id(), std::process::id())
        .unwrap();
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Running);
    // The renewed lease is fresh, so another supervisor does not adopt it.
    assert!(
        queue
            .adopt_run(run.id(), "sleeper", "other", 2, json!({}))
            .unwrap()
            .is_none()
    );
    assert!(queue.holds_lease(run.id(), "sleeper").unwrap());
    assert_eq!(supervisor_token_of(&db, &run), "sleeper");
    assert!(!queue.has_run_event(run.id(), "run_adopted").unwrap());

    // A second run whose supervisor sleeps until another one adopts it.
    let taken = start(&mut queue, "late");
    queue.workspace_created(taken.id(), "late", "ws-2").unwrap();
    queue
        .register_wrapper(taken.id(), "late", std::process::id())
        .unwrap();
    queue
        .register_agent(taken.id(), std::process::id(), std::process::id())
        .unwrap();
    clock.set(T + 2 * SLEEP);
    let adopted = queue
        .adopt_run(taken.id(), "late", "adopter", 3, json!({}))
        .unwrap()
        .unwrap();
    assert_eq!(adopted.status(), RunStatus::Running);
    let events = queue.show(taken.task_id()).unwrap().events.len();
    // Woken up after the adoption, the late supervisor is refused and
    // writes nothing: the lease, the run and its events stay the adopter's.
    let error = queue.finish_supervision(taken.id(), "late").unwrap_err();
    assert_eq!(
        error.to_string(),
        "run lease is missing or held by another supervisor"
    );
    let lease = queue.run_lease(taken.id()).unwrap().unwrap();
    assert_eq!((lease.token.as_str(), lease.pid), ("adopter", 3));
    assert_eq!(supervisor_token_of(&db, &taken), "adopter");
    assert_eq!(queue.run(taken.id()).unwrap().status(), RunStatus::Running);
    assert_eq!(queue.show(taken.task_id()).unwrap().events.len(), events);
    // The adopter's own writes go on.
    queue
        .finish_supervision_live(taken.id(), "adopter")
        .unwrap();
    assert!(!queue.holds_lease(taken.id(), "late").unwrap());
}

#[test]
fn shell_arguments_round_trip_without_expansion_and_cmux_handles_are_strict() {
    let value = "a'b $HOME $(echo injected) `echo injected`\nmore";
    let result = Command::new("/bin/sh")
        .arg("-c")
        .arg(shell_join(&["printf".into(), "%s".into(), value.into()]))
        .bounded_output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(String::from_utf8(result.stdout).unwrap(), value);
    assert_eq!(workspace_handle("OK workspace:7\n").unwrap(), "workspace:7");
    assert!(workspace_handle("OK workspace:7; touch file").is_err());
    assert!(workspace_handle("OK surface:7").is_err());
}

#[test]
fn migration_from_v1_preserves_task_and_initializes_runtime_tables() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("old.db");
    let raw = Connection::open(&db).unwrap();
    raw.execute_batch(include_str!("../../migrations/0001_queue.sql"))
        .unwrap();
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 1).unwrap();
    raw.execute("INSERT INTO tasks(title,description,acceptance,verification_commands) VALUES ('preserved','','','[]')", []).unwrap();
    assert!(SqliteQueue::open(&db).is_err());
    SqliteQueue::migrate(&db, None, 0).unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.title(),
        "preserved"
    );
    assert!(queue.run_leases().unwrap().is_empty());
}

#[test]
fn wrapper_registration_is_one_shot_and_rejects_other_owners() {
    use dagq::{
        domain::ClaimOutcome,
        infrastructure::runtime_store::{RunPlan, Validation},
    };
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&sha("0123456789abcdef0123456789abcdef01234567"), "owner")
        .unwrap()
    else {
        panic!()
    };
    queue
        .plan_run(
            run.id(),
            "owner",
            &RunPlan {
                repo_path: "/test".into(),
                run_dir: "/run".into(),
                branch: "dagq/test".into(),
                worktree_path: "/run/worktree".into(),
                receipt_path: "/run/receipt.json".into(),
                log_path: "/run/log".into(),
            },
        )
        .unwrap();
    assert!(queue.register_wrapper(run.id(), "owner", 10).is_err()); // Workspace not attached yet.
    queue
        .workspace_created(run.id(), "owner", "workspace")
        .unwrap();
    assert!(queue.register_wrapper(run.id(), "other-owner", 10).is_err());
    let raw = Connection::open(&db).unwrap();
    raw.execute("UPDATE run_leases SET heartbeat_at=0", [])
        .unwrap();
    // A stale lease of the owner's token is renewed, not refused (ADR-0039
    // decision 7).
    queue.register_wrapper(run.id(), "owner", 10).unwrap();
    assert!(queue.run_lease(run.id()).unwrap().unwrap().heartbeat_at > 0);
    assert!(queue.register_wrapper(run.id(), "owner", 11).is_err());
    assert!(queue.register_agent(run.id(), 11, 12).is_err());
    queue.register_agent(run.id(), 10, 12).unwrap();
    assert!(queue.finish_supervision(run.id(), "owner").is_err()); // Still live.
    queue.wrapper_exited(run.id(), 10, 0).unwrap();
    assert!(queue.heartbeat_wrapper(run.id(), 10).is_err());
    assert_eq!(
        queue
            .finish_supervision(run.id(), "owner")
            .unwrap()
            .status(),
        RunStatus::Validating
    );
    let validation = Validation {
        accepted: false,
        result_commit: None,
        reason: Some("receipt was not submitted".into()),
        code: Some(ReasonCode::ReceiptMissing),
        receipt: Value::Null,
        evidence_missing: Vec::new(),
        scope_violation: Vec::new(),
        allowed_paths: Vec::new(),
        load: Default::default(),
    };
    assert!(
        queue
            .finish_validation(run.id(), "other-owner", &validation)
            .is_err()
    );
    let failed = queue
        .finish_validation(run.id(), "owner", &validation)
        .unwrap();
    assert_eq!(failed.status(), RunStatus::Failed);
    assert_eq!(failed.last_error(), Some("receipt was not submitted"));
    assert!(
        queue
            .finish_validation(run.id(), "owner", &validation)
            .is_err()
    ); // Terminal.
    // A failed run never records a workspace close or cleanup failure.
    assert!(queue.workspace_closed(run.id(), "owner").is_err());
    assert!(
        queue
            .cleanup_failed(run.id(), "owner", "late", &ReasonCode::BackendFailed.into())
            .is_err()
    );
}

#[test]
fn workspace_close_is_recorded_once_and_only_for_accepted_runs() {
    use dagq::{
        domain::ClaimOutcome,
        infrastructure::runtime_store::{RunPlan, Validation},
    };
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&sha("0123456789abcdef0123456789abcdef01234567"), "owner")
        .unwrap()
    else {
        panic!()
    };
    queue
        .plan_run(
            run.id(),
            "owner",
            &RunPlan {
                repo_path: "/test".into(),
                run_dir: "/run".into(),
                branch: "dagq/test".into(),
                worktree_path: "/run/worktree".into(),
                receipt_path: "/run/receipt.json".into(),
                log_path: "/run/log".into(),
            },
        )
        .unwrap();
    queue
        .workspace_created(run.id(), "owner", WORKSPACE_ID)
        .unwrap();
    queue.register_wrapper(run.id(), "owner", 10).unwrap();
    queue.register_agent(run.id(), 10, 12).unwrap();
    // Still running: neither close nor cleanup failure may be recorded.
    assert!(queue.workspace_closed(run.id(), "owner").is_err());
    assert!(
        queue
            .cleanup_failed(
                run.id(),
                "owner",
                "early",
                &ReasonCode::BackendFailed.into()
            )
            .is_err()
    );
    queue.wrapper_exited(run.id(), 10, 0).unwrap();
    queue.finish_supervision(run.id(), "owner").unwrap();
    let accepted = queue
        .finish_validation(
            run.id(),
            "owner",
            &Validation {
                accepted: true,
                result_commit: Some(sha("89abcdef0123456789abcdef0123456789abcdef")),
                reason: None,
                code: None,
                receipt: Value::Null,
                evidence_missing: Vec::new(),
                scope_violation: Vec::new(),
                allowed_paths: Vec::new(),
                load: Default::default(),
            },
        )
        .unwrap();
    assert_eq!(accepted.status(), RunStatus::AwaitingIntegration);
    assert!(accepted.workspace_closed_at().is_none());
    assert!(queue.workspace_closed(run.id(), "other-owner").is_err());
    let failed = queue
        .cleanup_failed(
            run.id(),
            "owner",
            "cmux down",
            &ReasonCode::BackendFailed.into(),
        )
        .unwrap();
    assert_eq!(failed.status(), RunStatus::AwaitingIntegration);
    assert_eq!(failed.last_error(), Some("cmux down"));
    assert!(failed.workspace_closed_at().is_none());
    // A later successful close clears nothing but records the close once.
    let closed = queue.workspace_closed(run.id(), "owner").unwrap();
    assert!(closed.workspace_closed_at().is_some());
    assert_eq!(closed.status(), RunStatus::AwaitingIntegration);
    assert!(queue.workspace_closed(run.id(), "owner").is_err());
    assert!(
        queue
            .cleanup_failed(run.id(), "owner", "late", &ReasonCode::BackendFailed.into())
            .is_err()
    );
    let kinds: Vec<String> = queue
        .show(TaskId::new(1))
        .unwrap()
        .events
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert_eq!(kinds.iter().filter(|k| *k == "workspace_closed").count(), 1);
    assert_eq!(kinds.iter().filter(|k| *k == "cleanup_failed").count(), 1);
}

#[test]
fn no_ready_task_ends_a_once_pass_without_creating_a_run_or_lease() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(TaskId::new(1), TaskAction::Draft).unwrap();
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["runs"], json!([]));
    assert_eq!(outcome["errors"], json!([]));
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(queue.supervisors().unwrap().is_empty());
    assert!(queue.show(TaskId::new(1)).unwrap().runs.is_empty());
    assert!(supervise_with(&db, &repo, &backend, &supervise_options(0, true)).is_err());
    assert!(queue.supervisors().unwrap().is_empty());
}

/// A resident supervisor that holds no run is still listed by `status` and
/// `doctor` through its registration, which its heartbeat refreshes and a
/// graceful stop removes.
#[test]
fn resident_supervisor_without_runs_is_listed_until_it_stops() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(TaskId::new(1), TaskAction::Draft).unwrap();
    assert_eq!(runtime::status(&db).unwrap()["supervisors"], json!([]));
    let backend = Arc::new(TestWorkspace::new(&db, true, VALID_AGENT));
    let options = supervise_options(3, false);
    let supervisor = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(10), |queue| {
        queue.supervisors().unwrap().len() == 1
    });
    let registered = queue.supervisors().unwrap().remove(0);
    assert_eq!(registered.pid, std::process::id());
    assert_eq!(registered.parallel, 3);
    for report in [
        runtime::status(&db).unwrap(),
        runtime::doctor(&db, true).unwrap(),
    ] {
        assert_eq!(report["runs"], json!([]));
        assert_eq!(report["supervisors"].as_array().unwrap().len(), 1);
        let entry = &report["supervisors"][0];
        assert_eq!(entry["pid"], json!(std::process::id()));
        assert_eq!(entry["alive"], true);
        assert_eq!(entry["registered"], true);
        assert_eq!(entry["parallel"], 3);
        assert_eq!(entry["started_at"], json!(registered.started_at));
        assert_eq!(entry["stale"], false);
        assert!(entry["heartbeat_age_secs"].as_i64().unwrap() <= 5);
        assert_eq!(entry["run_ids"], json!([]));
    }
    // The heartbeat keeps the registration fresh while nothing runs.
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE supervisors SET heartbeat_at=0", [])
        .unwrap();
    wait_until(&db, Duration::from_secs(10), |queue| {
        queue.supervisors().unwrap()[0].heartbeat_at > 0
    });
    assert!(queue.run_leases().unwrap().is_empty());

    options.stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["outcome"], "stopped");
    assert_eq!(outcome["runs"], json!([]));
    assert!(queue.supervisors().unwrap().is_empty());
    assert_eq!(runtime::status(&db).unwrap()["supervisors"], json!([]));
    assert_eq!(
        runtime::doctor(&db, true).unwrap()["supervisors"],
        json!([])
    );

    // An error out of the loop itself (here: main vanished before a claim)
    // ends the process with nothing active, so it deregisters too.
    let options = supervise_options(1, false);
    let supervisor = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(10), |queue| {
        queue.supervisors().unwrap().len() == 1
    });
    git(&repo, &["update-ref", "-d", "refs/heads/main"]);
    queue
        .transition(TaskId::new(1), TaskAction::BypassReview)
        .unwrap();
    let error = format!(
        "{:#}",
        joined(supervisor, "the supervisor thread to return").unwrap_err()
    );
    assert!(error.contains("Needed a single revision"), "{error}");
    assert!(queue.supervisors().unwrap().is_empty());
    assert!(queue.show(TaskId::new(1)).unwrap().runs.is_empty());
}

/// A supervisor's progress goes to its process's JSON Lines file in the
/// log directory (ADR-0033): one record per line, with the startup facts,
/// the progress messages that also go to stderr, their run and task IDs as
/// fields, and the final result.
#[test]
fn supervise_records_its_progress_as_json_lines() {
    let (dir, repo, db) = fixture();
    let log_dir = dir.path().join("logs").join("nested");
    let options = supervise_options(2, true);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let telemetry = Telemetry::open(&log_dir, "supervise");
    let outcome = telemetry
        .in_scope(|| supervise_with(&db, &repo, &backend, &options))
        .unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    let pid = std::process::id();
    let path = telemetry.path.clone().unwrap();
    let name = path.file_name().unwrap().to_str().unwrap();
    assert!(
        name.starts_with("supervise-") && name.ends_with(&format!("Z-{pid}.jsonl")),
        "{name}"
    );
    assert_eq!(path.parent().unwrap(), log_dir);
    let text = fs::read_to_string(&path).unwrap();
    let records: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records[0]["message"], "dagq supervise started");
    let messages: Vec<&str> = records
        .iter()
        .map(|r| r["message"].as_str().unwrap())
        .collect();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs.remove(0);
    let token: String = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT supervisor_token FROM task_runs WHERE id=?1",
            [&run.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        messages.contains(&format!(
            "supervisor {token} started: version {VERSION}, pid {pid}, parallel 2, db {}, repository {}",
            db.canonicalize().unwrap().display(),
            repo.canonicalize().unwrap().display()
        ).as_str()),
        "{text}"
    );
    let running = records
        .iter()
        .find(|r| {
            r["message"]
                == format!(
                    "task 1 running in workspace {WORKSPACE_ID}; run {}",
                    run.id()
                )
        })
        .unwrap();
    assert_eq!(running["fields"]["run_id"], run.id().as_str());
    assert_eq!(running["fields"]["task_id"], "1");
    assert_eq!(running["level"], "INFO");
    assert_eq!(running["target"], "dagq::application::supervise::session");
    assert!(text.contains(&format!("receipt received for {}", run.id())));
    assert!(text.contains(&format!("run {} is awaiting_integration", run.id())));
    assert!(messages.iter().any(|m| m.starts_with(&format!(
        "supervisor {token} exiting: {{\"errors\":[],\"outcome\":\"finished\""
    ))));
}

/// A registration whose process died, or whose heartbeat stopped, is
/// reported as stale by `status` and `doctor` and left for a person;
/// neither a later supervisor nor `recover` removes it, and an `integrate`
/// or orphaned lease holder is listed next to it without a registration.
#[test]
fn killed_supervisor_registration_is_reported_stale_and_never_deleted() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let dead = dead_pid();
    let killed = queue
        .register_supervisor("killed", dead, 4, VERSION)
        .unwrap();
    // Killed a moment ago: the heartbeat is fresh, the pid is gone.
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["runs"], json!([]));
    let entry = &status["supervisors"][0];
    assert_eq!(entry["pid"], json!(dead));
    assert_eq!(entry["alive"], false);
    assert_eq!(entry["stale"], true);
    assert_eq!(entry["registered"], true);
    assert_eq!(entry["parallel"], 4);
    // The build the process ran, which `up` compares against its own.
    assert_eq!(entry["binary_version"], VERSION);
    assert_eq!(entry["heartbeat_at"], json!(killed.heartbeat_at));
    assert!(entry["heartbeat_age_secs"].as_i64().unwrap() <= 5);
    // Alive but silent: stale by heartbeat age alone.
    queue
        .register_supervisor("hung", std::process::id(), 1, VERSION)
        .unwrap();
    let raw = Connection::open(&db).unwrap();
    raw.execute(
        "UPDATE supervisors SET heartbeat_at=1700000000 WHERE token='hung'",
        [],
    )
    .unwrap();
    drop(raw);
    let doctor = runtime::doctor(&db, true).unwrap();
    assert_eq!(doctor["supervisors"].as_array().unwrap().len(), 2);
    let hung = &doctor["supervisors"][1];
    assert_eq!(hung["alive"], true);
    assert_eq!(hung["stale"], true);
    assert!(hung["heartbeat_age_secs"].as_i64().unwrap() > 30);
    assert_eq!(hung["run_ids"], json!([]));
    let compact = runtime::doctor(&db, false).unwrap();
    let keys: Vec<&String> = compact["supervisors"][1]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(
        keys,
        [
            "alive",
            "auto_update",
            "binary_version",
            "heartbeat_age_secs",
            "mode",
            "pid",
            "registered",
            "run_ids",
            "stale",
            "workspace_id"
        ]
    );
    assert_eq!(compact["supervisors"][1]["stale"], true);

    // A run owned without a registration (an `integrate` process, or a
    // supervisor from before the registry) is still attributed to its lease.
    let orphan = orphan_run(&repo, &db, "owner", std::process::id(), std::process::id());
    let status = runtime::status(&db).unwrap();
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 3);
    assert_eq!(supervisors[2]["registered"], false);
    assert_eq!(supervisors[2]["parallel"], Value::Null);
    // A lease holder without a registration recorded no version either.
    assert_eq!(supervisors[2]["binary_version"], Value::Null);
    assert_eq!(supervisors[2]["started_at"], Value::Null);
    assert_eq!(supervisors[2]["pid"], json!(std::process::id()));
    assert_eq!(supervisors[2]["alive"], true);
    assert_eq!(supervisors[2]["stale"], false);
    assert_eq!(supervisors[2]["run_ids"], json!([orphan.id()]));
    assert_eq!(status["runs"][0]["lease"]["pid"], json!(std::process::id()));
    // A registered supervisor's leases join it by token rather than by pid.
    queue
        .register_supervisor("owner", std::process::id(), 2, VERSION)
        .unwrap();
    let status = runtime::status(&db).unwrap();
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 3);
    assert_eq!(supervisors[2]["registered"], true);
    assert_eq!(supervisors[2]["parallel"], 2);
    assert_eq!(supervisors[2]["run_ids"], json!([orphan.id()]));

    // Recovery of the run and a later supervisor's own registration and
    // deregistration leave the stale rows alone.
    queue
        .wrapper_exited(orphan.id(), std::process::id(), 0)
        .unwrap();
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases", [])
        .unwrap();
    assert_eq!(
        runtime::recover(&db, orphan.id()).unwrap()["run"]["status"],
        "interrupted"
    );
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    assert_eq!(
        supervise(&db, &repo, &backend).unwrap()["outcome"],
        "finished"
    );
    let tokens: Vec<String> = queue
        .supervisors()
        .unwrap()
        .into_iter()
        .map(|s| s.token)
        .collect();
    assert_eq!(tokens, ["killed", "hung", "owner"]);
    assert_eq!(
        runtime::doctor(&db, true).unwrap()["supervisors"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

/// Workspaces as the test lists them, or a failing cmux.
struct Listing(Result<Vec<(&'static str, String)>, &'static str>);

impl dagq::application::stats::WorkspaceListing for Listing {
    fn list_workspaces(&self) -> Result<Vec<dagq::domain::stats::ListedWorkspace>> {
        match &self.0 {
            Ok(workspaces) => Ok(workspaces
                .iter()
                .map(|(id, description)| dagq::domain::stats::ListedWorkspace {
                    id: (*id).to_owned(),
                    description: Some(description.clone()),
                })
                .collect()),
            Err(message) => bail!("{message}"),
        }
    }
}

/// Task 182 (ADR-0043 decision 5): a running worker that stopped with a
/// background `cargo test` left running and no receipt is in `stats`'s
/// `running_alerts`, judged by the `[stall]` of the main checkout's
/// `dagq.toml`, with the cmux workspaces that do not match the runs.
#[test]
fn stats_raise_running_alerts_for_a_worker_idle_without_a_receipt() {
    let (_dir, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        "[stall]\nidle_without_receipt_secs = 600\nbackground_alert_secs = 3600\n",
    )
    .unwrap();
    let run = orphan_run(&repo, &db, "owner", dead_pid(), dead_pid());
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    fs::write(
        run_dir.join("idle.json"),
        r#"{"hook_event_name":"Stop","background_tasks":[{"id":"b1","type":"shell","status":"running","description":"cargo test","command":"cargo test --locked"}]}"#,
    )
    .unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let one_shot = |late: i64| {
        runtime::OneShot::new(Generators {
            clock: Arc::new(ManualClock::at(now + late)),
            ids: Arc::new(FixedIds(Mutex::new(vec![]))),
        })
    };
    let hash = QueueLocation::explicit(&db).hash();
    let left = Listing(Ok(vec![
        (
            "WS-LEFT",
            format!(
                "dagq role=worker queue={hash} run=99999999-9999-4999-8999-999999999999 task=7"
            ),
        ),
        (
            "WS-OTHER",
            "dagq role=worker queue=other run=x task=1".to_owned(),
        ),
    ]));

    // Past 600 seconds of idle, under the hour of background work.
    let stats = one_shot(700)
        .stats(&db, &Default::default(), Some(&left))
        .unwrap();
    assert_eq!(stats["stall_config"]["source"], "file", "{stats}");
    assert_eq!(stats["stall_config"]["idle_without_receipt_secs"], 600);
    assert_eq!(
        stats["workspace_check"],
        json!({"status": "checked", "workspaces": 2})
    );
    let alerts = stats["running_alerts"].as_array().unwrap();
    let idle = &alerts[0];
    assert_eq!(idle["kind"], "idle_without_receipt", "{stats}");
    assert_eq!(idle["run_id"], run.id().as_str());
    assert_eq!(idle["phase"], "session");
    assert_eq!(idle["threshold"], 600);
    assert!(idle["value"].as_i64().unwrap() >= 699, "{idle}");
    assert_eq!(idle["nudged"], false);
    assert_eq!(idle["asked"], false);
    assert_eq!(
        idle["background_tasks"][0]["command"],
        "cargo test --locked"
    );
    let mismatches: Vec<_> = alerts
        .iter()
        .filter(|alert| alert["kind"] == "workspace_mismatch")
        .map(|alert| (alert["reason"].clone(), alert["workspace_id"].clone()))
        .collect();
    assert_eq!(
        mismatches,
        [
            (json!("run_without_workspace"), json!("ws-1")),
            (json!("workspace_without_run"), json!("WS-LEFT")),
        ]
    );
    assert!(
        !alerts
            .iter()
            .any(|alert| alert["kind"] == "long_background")
    );
    // The finished-run alerts are where they were.
    assert!(stats["alerts"].as_array().unwrap().is_empty(), "{stats}");

    // Hours later the background work is an alert too; a cmux that cannot
    // be asked leaves only the workspaces unjudged.
    let stats = one_shot(4 * 3600)
        .stats(
            &db,
            &Default::default(),
            Some(&Listing(Err("cmux is gone"))),
        )
        .unwrap();
    let kinds: Vec<_> = stats["running_alerts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|alert| alert["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(kinds, ["idle_without_receipt", "long_background"]);
    assert_eq!(stats["workspace_check"]["status"], "unavailable");
    assert!(
        stats["workspace_check"]["reason"]
            .as_str()
            .unwrap()
            .contains("cmux is gone")
    );

    // A receipt of the session ends the idle alert; without cmux nothing
    // is said about the workspaces.
    fs::write(run_dir.join("receipt.json"), "{}").unwrap();
    let stats = one_shot(700).stats(&db, &Default::default(), None).unwrap();
    assert_eq!(stats["running_alerts"], json!([]), "{stats}");
    assert_eq!(stats["workspace_check"]["status"], "unavailable");
}

/// Task 331 (ADR-0043 decision 5): background work is timed from the first
/// idle marker that listed it as running, from the hook's log, so a session
/// that keeps taking turns does not restart the count.
#[test]
fn stats_time_background_work_from_its_first_marker() {
    let (_dir, repo, db) = fixture();
    let run = orphan_run(&repo, &db, "owner", dead_pid(), dead_pid());
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let running = |ids: &[&str]| {
        let tasks: Vec<Value> = ids
            .iter()
            .map(|id| json!({"id": id, "status": "running", "description": id, "command": "sleep"}))
            .collect();
        json!({"hook_event_name": "Stop", "background_tasks": tasks}).to_string()
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    fs::write(run_dir.join("idle.json"), running(&["b1", "b2"])).unwrap();
    let background = |log: String| {
        fs::write(run_dir.join("idle.log"), log).unwrap();
        let stats = runtime::OneShot::new(Generators {
            clock: Arc::new(ManualClock::at(now + 60)),
            ids: Arc::new(FixedIds(Mutex::new(vec![]))),
        })
        .stats(&db, &Default::default(), None)
        .unwrap();
        stats["running_alerts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|alert| alert["kind"] == "long_background")
            .cloned()
    };

    // b1 first listed 50 minutes ago, through turns taken since; a line
    // that is not a marker is skipped.
    let alert = background(format!(
        "{}\t{}\nnot a marker\n{}\t{}\n{now}\t{}\n",
        now - 3000,
        running(&["b1"]),
        now - 100,
        running(&["b1", "b2"]),
        running(&["b1", "b2"]),
    ))
    .expect("long_background");
    assert_eq!(alert["run_id"], run.id().as_str());
    assert_eq!(alert["threshold"], 1800);
    assert!(
        (3060..3065).contains(&alert["value"].as_i64().unwrap()),
        "{alert}"
    );
    assert_eq!(alert["background_tasks"][1]["id"], "b2");

    // A marker without b1 ended it: the b1 listed since started later.
    assert!(
        background(format!(
            "{}\t{}\n{}\t{}\n{}\t{}\n",
            now - 3000,
            running(&["b1"]),
            now - 1000,
            running(&[]),
            now - 900,
            running(&["b1", "b2"]),
        ))
        .is_none()
    );
    // Without a log (a session started before the hook kept one), the
    // marker's time is all there is.
    fs::remove_file(run_dir.join("idle.log")).unwrap();
    let stats = runtime::OneShot::new(Generators {
        clock: Arc::new(ManualClock::at(now + 1900)),
        ids: Arc::new(FixedIds(Mutex::new(vec![]))),
    })
    .stats(&db, &Default::default(), None)
    .unwrap();
    assert!(
        stats["running_alerts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|alert| alert["kind"] == "long_background"),
        "{stats}"
    );
}

#[test]
fn recover_requires_dead_processes_and_stale_lease_then_allows_a_new_run() {
    let (_dir, repo, db) = fixture();
    let mut wrapper = sleeper();
    let mut agent = sleeper();
    let run = orphan_run(&repo, &db, "owner", wrapper.id(), agent.id());
    let mut queue = SqliteQueue::open(&db).unwrap();

    // Everything is alive: doctor says so and recover refuses.
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["supervisors"][0]["pid"], json!(std::process::id()));
    assert_eq!(report["supervisors"][0]["stale"], false);
    assert_eq!(report["supervisors"][0]["alive"], true);
    assert_eq!(report["supervisors"][0]["run_ids"], json!([run.id()]));
    let health = &report["runs"][0];
    assert_eq!(health["run_id"], json!(run.id()));
    assert_eq!(health["status"], "running");
    assert_eq!(health["workspace_id"], "ws-1");
    assert_eq!(health["lease"]["stale"], false);
    assert_eq!(health["lease"]["alive"], true);
    assert_eq!(health["worktree_exists"], true);
    assert_eq!(health["run_dir_exists"], true);
    assert_eq!(health["receipt_exists"], false);
    assert_eq!(health["recoverable"], false);
    let processes = health["processes"].as_array().unwrap();
    assert_eq!(processes.len(), 2);
    assert!(processes.iter().all(|p| p["alive"] == true));
    let error = format!("{:#}", runtime::recover(&db, run.id()).unwrap_err());
    assert!(
        error.contains("wrapper pid") && error.contains("lease heartbeat"),
        "{error}"
    );
    assert!(
        queue
            .transition(TaskId::new(1), TaskAction::BypassReview)
            .is_err()
    );

    // The supervisor is gone (stale heartbeat, dead PID) but the session is not.
    let raw = Connection::open(&db).unwrap();
    raw.execute("UPDATE run_leases SET heartbeat_at=0, pid=?1", [dead_pid()])
        .unwrap();
    raw.execute("UPDATE run_processes SET heartbeat_at=0", [])
        .unwrap();
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["supervisors"][0]["stale"], true);
    assert_eq!(report["supervisors"][0]["alive"], false);
    assert_eq!(report["runs"][0]["lease"]["stale"], true);
    assert_eq!(report["runs"][0]["processes"][0]["heartbeat_stale"], true);
    let error = format!("{:#}", runtime::recover(&db, run.id()).unwrap_err());
    assert!(
        error.contains("agent pid") && !error.contains("supervisor"),
        "{error}"
    );
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Running);
    assert!(queue.run_lease(run.id()).unwrap().is_some());

    // The session processes are gone too: recovery is allowed and explicit.
    agent.kill().unwrap();
    agent.wait().unwrap();
    wrapper.kill().unwrap();
    wrapper.wait().unwrap();
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["runs"][0]["recoverable"], true);
    assert_eq!(report["runs"][0]["blockers"], json!([]));
    let outcome = runtime::recover(&db, run.id()).unwrap();
    assert_eq!(outcome["outcome"], "recovered");
    assert_eq!(outcome["run"]["status"], "interrupted");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(detail.runs[0].status(), RunStatus::Interrupted);
    assert!(queue.run_leases().unwrap().is_empty());
    let recovered = detail
        .events
        .iter()
        .find(|e| e.kind == "run_recovered")
        .unwrap();
    assert_eq!(recovered.payload["previous_status"], "running");
    assert_eq!(recovered.payload["lease_deleted"], true);
    assert_eq!(recovered.payload["run"]["processes"][0]["alive"], false);
    assert_eq!(recovered.payload["run"]["lease"]["stale"], true);
    // Registrations and resources are left as observed.
    assert!(detail.processes.iter().all(|p| p.exited_at.is_none()));
    assert!(Path::new(run.worktree_path().unwrap()).exists());
    assert!(runtime::recover(&db, run.id()).is_err()); // No longer unfinished.
    assert_eq!(runtime::doctor(&db, true).unwrap()["runs"], json!([]));
    assert!(queue.candidates().unwrap().is_empty());

    // Retry is a separate decision: ready again, then a second run with new paths.
    queue.transition(TaskId::new(1), TaskAction::Ready).unwrap();
    assert_eq!(queue.candidates().unwrap().len(), 1);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2);
    assert_eq!(detail.runs[0].status(), RunStatus::Interrupted);
    assert_eq!(detail.runs[0].worktree_path(), run.worktree_path());
    assert!(Path::new(run.worktree_path().unwrap()).exists());
    assert_ne!(detail.runs[1].worktree_path(), run.worktree_path());
    assert!(
        queue
            .transition(TaskId::new(1), TaskAction::BypassReview)
            .is_err()
    ); // Awaiting integration still owns the task.
}

#[test]
fn recover_ignores_exited_processes_and_tolerates_a_missing_lease() {
    let (_dir, repo, db) = fixture();
    // The wrapper reported its exit before the supervisor died; its live PID
    // (this test process) must not block recovery.
    let pid = std::process::id();
    let run = orphan_run(&repo, &db, "owner", pid, pid);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.wrapper_exited(run.id(), pid, 0).unwrap();
    let error = format!("{:#}", runtime::recover(&db, run.id()).unwrap_err());
    assert!(
        error.contains("supervisor pid") && !error.contains("wrapper pid"),
        "{error}"
    );
    let report = runtime::doctor(&db, true).unwrap();
    assert!(
        report["runs"][0]["processes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["alive"].is_null() && p["heartbeat_stale"] == false)
    );
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases", [])
        .unwrap();
    assert_eq!(
        runtime::doctor(&db, true).unwrap()["supervisors"],
        json!([])
    );
    let outcome = runtime::recover(&db, run.id()).unwrap();
    assert_eq!(outcome["run"]["status"], "interrupted");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let recovered = detail
        .events
        .iter()
        .find(|e| e.kind == "run_recovered")
        .unwrap();
    assert_eq!(recovered.payload["lease_deleted"], false);
    assert_eq!(recovered.payload["run"]["lease"], Value::Null);
    // The task can be edited again before a retry.
    assert!(
        queue
            .add_dependency(TaskId::new(1), TaskId::new(1))
            .is_err()
    );
    queue.transition(TaskId::new(1), TaskAction::Draft).unwrap();
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Draft
    );
    assert!(runtime::recover(&db, &RunId::new("no-such-run").unwrap()).is_err());
}

#[test]
fn a_refused_run_transition_keeps_the_domain_reason_beside_the_old_error() {
    use dagq::{domain::ClaimOutcome, infrastructure::runtime_store::REFUSALS_LOG};
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let base = "0123456789abcdef0123456789abcdef01234567";
    let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&sha(base), "owner").unwrap()
    else {
        panic!()
    };
    let run_dir = dagq::infrastructure::location::runs_dir(&db.canonicalize().unwrap())
        .join(run.id().as_str());
    fs::create_dir_all(&run_dir).unwrap();
    // A claimed run is not awaiting integration: the error keeps the
    // store's message, and the domain's reason goes to the run's log.
    let error = queue.restart_validation(run.id(), "owner").unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "run is not awaiting integration under this supervisor"
    );
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Claimed);
    let log = fs::read_to_string(run_dir.join(REFUSALS_LOG)).unwrap();
    let line = log.lines().next().unwrap();
    assert!(line.starts_with('['), "{line}");
    assert!(
        line.ends_with(
            "] run is not awaiting integration under this supervisor: \
             cannot validate again a run in claimed state"
        ),
        "{line}"
    );
    // A refusal whose run directory is gone still fails the same way.
    fs::remove_dir_all(&run_dir).unwrap();
    let error = queue.restart_validation(run.id(), "owner").unwrap_err();
    assert_eq!(
        error.to_string(),
        "run is not awaiting integration under this supervisor"
    );
    assert!(!run_dir.exists());
}

/// `integrate` reads its time and its token from the generators `main`
/// hands it, not from the queue's own clock and UUIDs: a lease heartbeat
/// fresh to the injected clock holds the run, a stale one lets it through,
/// and only the injected token takes over a lease stored under it.
#[test]
fn integrate_takes_its_time_and_token_from_the_injected_generators() {
    let (_dir, db, detail) = run_agent(
        "git rm -q seed.txt && git commit -q -m 'drop seed'; receipt \"$(git rev-parse HEAD)\"",
    );
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let repo = Path::new(&db).parent().unwrap().join("repo's directory");
    Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,'held',?2,1000)",
            rusqlite::params![run.id(), std::process::id()],
        )
        .unwrap();
    let clock = ManualClock::at(1_005);
    let one_shot = |ids: Vec<&'static str>| {
        runtime::OneShot::new(Generators {
            clock: Arc::new(clock.clone()),
            ids: Arc::new(FixedIds(Mutex::new(ids))),
        })
    };
    let target = || IntegrateTarget::Task(TaskId::new(1));

    // Five seconds after the heartbeat by the injected clock: still held.
    let held = one_shot(vec![])
        .integrate(&db, target(), &repo, None)
        .unwrap_err();
    assert!(
        format!("{held:#}").contains("is held by the supervisor"),
        "{held:#}"
    );

    // Long after it the lease is stale, but a lease is only taken over
    // under its own token.
    clock.set(1_000 + 10 * dagq::domain::HEARTBEAT_TIMEOUT_SECS);
    let leased = one_shot(vec!["other"])
        .integrate(&db, target(), &repo, None)
        .unwrap_err();
    assert!(
        format!("{leased:#}").contains("run is still leased"),
        "{leased:#}"
    );
    let outcome = one_shot(vec!["held"])
        .integrate(&db, target(), &repo, None)
        .unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let started: i64 = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM run_events WHERE run_id=?1 AND kind='integration_started'",
            [run.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(started, 1);
}

/// `status` and `doctor` measure everything to the injected clock's now,
/// read once per call.
#[test]
fn status_and_doctor_measure_to_the_injected_clock() {
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue
        .ask(NewAsk {
            kind: AskKind::Blocked,
            task_id: Some(TaskId::new(1)),
            run_id: None,
            question: "which way?".into(),
            options: vec![],
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
        })
        .unwrap()
        .ask;
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE asks SET created_at=1000", [])
        .unwrap();
    let one_shot = runtime::OneShot::new(Generators {
        clock: Arc::new(ManualClock::at(1_042)),
        ids: Arc::new(FixedIds(Mutex::new(vec![]))),
    });
    let status = one_shot.status_for(&db, None).unwrap();
    assert_eq!(status["checked_at"], 1_042, "{status}");
    assert_eq!(status["asks"][0]["id"], ask.id.as_i64(), "{status}");
    assert_eq!(status["asks"][0]["age_secs"], 42, "{status}");
    let doctor = one_shot.doctor(&db, false, None).unwrap();
    assert_eq!(doctor["checked_at"], 1_042, "{doctor}");
}
