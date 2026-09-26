//! Runtime tests: The handoff of a supervisor to the next process.
use crate::runtime_support;

use runtime_support::*;

/// The only registered supervisor.
fn only_registration(db: &Path) -> dagq::domain::SupervisorRegistration {
    let registrations = SqliteQueue::open(db).unwrap().supervisors().unwrap();
    assert_eq!(registrations.len(), 1, "{registrations:?}");
    registrations.into_iter().next().unwrap()
}

/// Supervise until `condition` holds on the queue, then ask the supervisor
/// to hand off (ADR-0045 decision 10) and return its outcome and token.
fn hand_off_when(
    db: &Path,
    repo: &Path,
    backend: &Arc<TestWorkspace>,
    condition: impl FnMut(&mut SqliteQueue) -> bool,
) -> (Value, String) {
    let supervisor = {
        let (db, repo, backend) = (db.to_owned(), repo.to_owned(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(db, Duration::from_secs(30), condition);
    let registration = only_registration(db);
    assert!(registration.handoff_accepted, "{registration:?}");
    assert_eq!(registration.handoff_binary, None);
    let queue = SqliteQueue::open(db).unwrap();
    assert!(
        queue
            .request_handoff(&registration.token, "/next/dagq")
            .unwrap()
    );
    assert_eq!(
        queue
            .handoff_request(&registration.token)
            .unwrap()
            .as_deref(),
        Some("/next/dagq")
    );
    let outcome = joined(supervisor, "the supervisor asked to hand off").unwrap();
    assert_eq!(outcome["outcome"], "handoff", "{outcome}");
    assert_eq!(outcome["binary"], "/next/dagq");
    assert_eq!(outcome["token"], json!(registration.token));
    // The registration stays for the exec'd process, request and all.
    let kept = only_registration(db);
    assert_eq!(kept.token, registration.token);
    assert_eq!(kept.handoff_binary.as_deref(), Some("/next/dagq"));
    (outcome, registration.token)
}

/// The supervisor the exec'd binary runs: the same token, continued.
fn supervise_after_handoff(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    token: &str,
) -> Result<Value> {
    supervise_with(
        db,
        repo,
        backend,
        &SuperviseOptions {
            handoff_token: Some(token.to_owned()),
            ..supervise_options(4, true)
        },
    )
}

/// A handoff does not wait for the session (ADR-0045 decision 10): the
/// supervisor ends its loop while the worker still works, keeping its
/// registration and the run's lease, and the process that continues it
/// under the same token takes the run over without an adoption and
/// drives it to its receipt, one `/exit` and validation.
#[test]
fn a_handoff_leaves_the_session_running_and_the_next_process_drives_it_on() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, PROMPTED_AGENT));
    let (outcome, token) = hand_off_when(&db, &repo, &backend, |queue| {
        queue
            .show(TaskId::new(1))
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| run.status() == RunStatus::Running)
    });
    assert_eq!(outcome["handed_over"], 1, "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::Running);
    assert_eq!(queue.run_lease(run.id()).unwrap().unwrap().token, token);
    // The session was not asked anything.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);

    let next = {
        let (db, repo, backend, token) = (db.clone(), repo.clone(), backend.clone(), token.clone());
        thread::spawn(move || supervise_after_handoff(&db, &repo, &backend, &token))
    };
    // The registration is taken back before the run moves on.
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue
            .supervisors()
            .unwrap()
            .first()
            .is_some_and(|r| r.handoff_binary.is_none())
    });
    let taken = only_registration(&db);
    assert_eq!(taken.token, token);
    assert_eq!(taken.pid, std::process::id());
    assert_eq!(taken.binary_version.as_deref(), Some(VERSION));
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(next, "the supervisor after the handoff").unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished", "{outcome}");
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);

    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"run_adopted"), "{kinds:?}");
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert_eq!(kinds.iter().filter(|k| **k == "lease_acquired").count(), 1);
    assert_eq!(supervisor_token_of(&db, &run), token);
    assert_eq!(detail.runs.len(), 1);
    assert!(detail.runs[0].result_commit().is_some());
    let handed = events_of(&db, run.id(), runtime::SUPERVISOR_HANDED_OFF);
    assert_eq!(handed.len(), 1);
    assert_eq!(handed[0]["status"], "running");
    assert_eq!(handed[0]["state"], Value::Null);
    assert_eq!(handed[0]["supervisor"], json!(token));
    assert_eq!(handed[0]["previous_version"], VERSION);
    assert_eq!(handed[0]["version"], VERSION);
    // The continued supervisor ended like any other and removed its row.
    assert!(queue.supervisors().unwrap().is_empty());
    // The change marks (ADR-0051 decision 10): the start, the handoff and
    // one stop; the process that exec'd records no stop of its own.
    let marks: Vec<(String, Value)> = queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind.starts_with("supervisor_") && event.task_id.is_none())
        .map(|event| (event.kind, event.payload))
        .collect();
    let kinds: Vec<&str> = marks.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "supervisor_started",
            "supervisor_started",
            "supervisor_stopped"
        ]
    );
    assert_eq!(marks[0].1["handoff"], json!(false));
    assert_eq!(marks[1].1["handoff"], json!(true));
    assert_eq!(marks[1].1["previous_version"], VERSION);
    assert!(
        marks
            .iter()
            .all(|(_, payload)| payload["supervisor"] == json!(token))
    );
}

/// A run rejected by validation waits for its session to take the `/exit`;
/// a handoff in that wait carries the request over in the run's
/// `handoff.json`, so the next process sends no second `/exit` and lets
/// the run rest once the session exits.
#[test]
fn a_handoff_while_a_rejected_run_waits_for_its_exit_sends_no_second_exit() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        &format!(
            "commit work; printf 'scratch\\n' > untracked.txt; receipt \"$(git rev-parse HEAD)\"; idle; {HOLD}"
        ),
    ));
    let (outcome, token) = hand_off_when(&db, &repo, &backend, |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"exit_requested")
    });
    assert_eq!(outcome["handed_over"], 1, "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::Failed);
    let snapshot = Path::new(run.run_dir().unwrap()).join("handoff.json");
    let written: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
    assert_eq!(written["phase"], "exit");
    assert_eq!(written["requested"], true);
    assert_eq!(written["close"], false);

    let next = {
        let (db, repo, backend, token) = (db.clone(), repo.clone(), backend.clone(), token.clone());
        thread::spawn(move || supervise_after_handoff(&db, &repo, &backend, &token))
    };
    wait_until(&db, Duration::from_secs(30), |_| !snapshot.exists());
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(next, "the supervisor after the handoff").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "failed");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert!(!kinds.contains(&"run_adopted"), "{kinds:?}");
}

/// A handoff while a resumed session resolves a conflict carries the
/// resume over in `handoff.json`: the next process goes on watching that
/// session instead of resuming the run again, and records it as
/// `auto_repaired` (`repair: resume_adopted`, ADR-0047 decision 24).
#[test]
fn a_handoff_during_a_resume_goes_on_watching_the_resumed_session() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let (run, _) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        "await_message; while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let (outcome, token) = hand_off_when(&db, &repo, &backend, |queue| {
        event_kinds(&queue.show(TaskId::new(2)).unwrap()).contains(&"resume_started")
    });
    assert_eq!(outcome["handed_over"], 1, "{outcome}");
    let snapshot = Path::new(run.run_dir().unwrap()).join("handoff.json");
    let written: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
    assert_eq!(written["phase"], "resume");

    let next = {
        let (db, repo, backend, token) = (db.clone(), repo.clone(), backend.clone(), token.clone());
        thread::spawn(move || supervise_after_handoff(&db, &repo, &backend, &token))
    };
    wait_until(&db, Duration::from_secs(30), |_| !snapshot.exists());
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(next, "the supervisor after the handoff").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "resume_started").count(), 1);
    let adopted: Vec<&Value> = payloads(&detail, "auto_repaired")
        .into_iter()
        .filter(|p| p["repair"] == "resume_adopted")
        .collect();
    assert_eq!(adopted.len(), 1, "{kinds:?}");
    assert_eq!(adopted[0]["layer"], "runtime");
    assert_eq!(adopted[0]["conditions"]["handoff"], true);
    assert_eq!(adopted[0]["conditions"]["attempt"], 1);
    assert_eq!(adopted[0]["detail"]["version"], VERSION);
    // The conflict-only resume was not counted, and that too is a repair.
    assert!(
        payloads(&detail, "auto_repaired")
            .iter()
            .any(|p| p["repair"] == "conflict_resume_uncounted"),
        "{kinds:?}"
    );
}

/// A handoff that finds a leased run it has nothing to rebuild from (a
/// resting run without a `handoff.json`) gives the lease back, and a
/// registration that is gone refuses the continuation.
#[test]
fn after_a_handoff_a_run_without_state_gives_its_lease_back() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .register_supervisor("gone-by", std::process::id(), 1, "0.0.1")
        .unwrap();
    // No handoff for a registration that does not take one.
    assert!(!queue.request_handoff("gone-by", "/next/dagq").unwrap());
    queue.accept_handoff("gone-by").unwrap();
    assert!(queue.request_handoff("gone-by", "/next/dagq").unwrap());
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "gone-by");
    backend.join();
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='succeeded' WHERE id=?1",
            [run.id()],
        )
        .unwrap();
    assert_eq!(queue.runs_leased_by("gone-by").unwrap().len(), 1);
    let outcome = supervise_after_handoff(&db, &repo, &backend, "gone-by").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert!(queue.runs_leased_by("gone-by").unwrap().is_empty());
    let error = supervise_after_handoff(&db, &repo, &backend, "gone-by").unwrap_err();
    assert!(
        format!("{error:#}").contains("is no longer registered"),
        "{error:#}"
    );
}

/// The supervisor's look at main for the automatic update (ADR-0045
/// decision 17): with `auto_update` on its registration it starts the
/// update job for main's head when the commits since the last update
/// change the runtime, not for documentation alone, and builds the same
/// head again when the `update_failed` ask is answered `retry`. The stub
/// build fails, so nothing is ever replaced.
#[test]
fn auto_update_builds_runtime_landings_and_retries_on_the_answer() {
    let (_fixture, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let options = SuperviseOptions {
        update: dagq::application::supervise::UpdateSettings {
            register: true,
            interval: Duration::ZERO,
            build_command: Some("echo no build here >&2; exit 1".into()),
            cmux: None,
        },
        ..supervise_options(1, true)
    };
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let head = |repo: &Path| {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "main"])
            .bounded_output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    let commit = |path: &str| {
        let file = repo.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, path).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", path]);
        head(&repo)
    };
    let updates = || SqliteQueue::open(&db).unwrap().binary_updates(100).unwrap();
    let of = |kind: &str, sha: &str| {
        updates()
            .iter()
            .filter(|u| u.kind == kind && u.commit.as_deref() == Some(sha))
            .count()
    };
    let wait_failed = |sha: &str, times: usize| {
        let started = Instant::now();
        while of("update_failed", sha) < times {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the job for {sha} did not fail: {:?}",
                updates()
            );
            thread::sleep(Duration::from_millis(100));
        }
    };

    // This build names a commit the repository does not have: main's head
    // is built once.
    let seed = head(&repo);
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(of("update_started", &seed), 1, "{:?}", updates());
    wait_failed(&seed, 1);
    let failed = updates()
        .into_iter()
        .find(|u| u.kind == "update_failed")
        .unwrap();
    assert_eq!(failed.payload["stage"], "build", "{failed:?}");
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["auto_update"]["state"], "failed", "{status}");
    assert_eq!(status["auto_update"]["enabled"], false, "{status}");

    // Nothing new on main, then documentation only: no job.
    supervise_with(&db, &repo, &backend, &options).unwrap();
    let docs = commit("docs/notes.md");
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(of("update_started", &seed), 1);
    assert_eq!(of("update_started", &docs), 0, "{:?}", updates());

    // `retry` builds main's head again, whatever it changed.
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.subject.as_deref() == Some("update_failed"))
        .unwrap();
    queue.answer(ask.id, "retry").unwrap();
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(of("update_started", &docs), 1, "{:?}", updates());
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert!(updates().iter().any(|u| u.kind == "update_retry"));
    wait_failed(&docs, 1);

    // A change of the runtime starts the job for it.
    let source = commit("src/lib.rs");
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(of("update_started", &source), 1, "{:?}", updates());
    wait_failed(&source, 1);

    // A job that died without recording how it ended is reported, not
    // rebuilt on its own; an answer row does not hide a running job.
    let queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_binary_update(
            "update_started",
            Some(&source),
            json!({"pid": 999_999_999u32}),
        )
        .unwrap();
    supervise_with(&db, &repo, &backend, &options).unwrap();
    let latest = updates().remove(0);
    assert_eq!(latest.kind, "update_failed", "{latest:?}");
    assert_eq!(latest.payload["stage"], "interrupted", "{latest:?}");
    assert_eq!(of("update_started", &source), 2);
    queue
        .record_binary_update(
            "update_started",
            Some(&source),
            json!({"pid": std::process::id()}),
        )
        .unwrap();
    queue
        .record_binary_update("update_retry", None, json!({}))
        .unwrap();
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(of("update_started", &source), 3, "{:?}", updates());
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["auto_update"]["state"], "building", "{status}");
    backend.join();
}
