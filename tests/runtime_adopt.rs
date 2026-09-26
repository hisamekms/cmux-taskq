//! Runtime tests: Concurrent runs and the adoption of runs from a dead supervisor.
mod common;
mod runtime_support;

use runtime_support::*;

/// Two independent tasks execute at the same time under one resident
/// supervisor; the dependent task waits for `integrate` and is then picked up
/// by the same loop with the new `main` as its base.
#[test]
fn independent_tasks_run_concurrently_and_a_dependent_starts_after_integration() {
    let (dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "independent", &[]);
    add_ready_task(&mut queue, "dependent", &[TaskId::new(1)]);
    // Each session works until the test lets it finish (`$EXIT.go`): a
    // session that finished at once let its run reach validation before the
    // other one started under a loaded host, and the two were never seen
    // running together.
    // Starting a session is bounded by the same limit as the test's waits:
    // under a loaded host a run's wrapper took longer than the backend's
    // default windows to register.
    let mut backend = TestWorkspace::new(&db, false, PROMPTED_AGENT);
    backend.registration_timeout = common::STEP_LIMIT;
    backend.start_wait = common::STEP_LIMIT;
    let backend = Arc::new(backend);
    let options = supervise_options(4, false);
    let finish = |run: &TaskRun| {
        fs::write(
            Path::new(run.run_dir().unwrap()).join("exit-requested.go"),
            "",
        )
        .unwrap();
    };
    let supervisor = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };

    // Both independent sessions start; the dependent has no run. Each
    // session is held until the test finishes it, so this state, once
    // reached, stays: the limit bounds reaching it (starting two runs under
    // a loaded host took longer than 60 seconds), not a window to catch it.
    wait_until(&db, common::STEP_LIMIT, |queue| {
        let events = queue.all_events().unwrap();
        [1, 2]
            .iter()
            .all(|task| first_event(&events, *task, &["agent_started"]).is_some())
    });
    assert!(queue.show(TaskId::new(3)).unwrap().runs.is_empty());
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["supervisors"].as_array().unwrap().len(), 1);
    assert_eq!(
        status["supervisors"][0]["run_ids"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(status["runs"].as_array().unwrap().len(), 2);
    assert!(status["runs"][0]["lease"]["pid"].is_number());
    let doctor = runtime::doctor(&db, true).unwrap();
    assert_eq!(doctor["runs"].as_array().unwrap().len(), 2);
    assert!(
        doctor["runs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["recoverable"] == false)
    );
    for run in queue.active_runs().unwrap() {
        finish(&run);
    }

    // Accepted and past the supervisor's review (the stand-in `claude`
    // prints no verdict, so each waits for a review by hand).
    wait_until(&db, common::STEP_LIMIT, |queue| {
        [1, 2].iter().all(|task| {
            queue.show(TaskId::new(*task)).unwrap().runs[0].status()
                == RunStatus::AwaitingIntegration
        }) && queue.run_leases().unwrap().is_empty()
    });
    // Awaiting integration does not satisfy the dependency; the loop idles.
    thread::sleep(Duration::from_millis(500));
    assert!(queue.show(TaskId::new(3)).unwrap().runs.is_empty());
    assert!(queue.candidates().unwrap().is_empty());
    let first = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let second = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_ne!(first.workspace_id(), second.workspace_id());
    assert_eq!(first.base_commit(), second.base_commit());
    assert!(queue.run_leases().unwrap().is_empty());
    // The two ran together, as the record shows: one supervisor took both
    // leases and started both sessions before either session's supervision
    // finished or either lease was released.
    let events = queue.all_events().unwrap();
    let ended = [1, 2]
        .iter()
        .map(|task| {
            first_event(
                &events,
                *task,
                &["supervision_finished", "session_exited", "lease_released"],
            )
            .unwrap()
        })
        .min()
        .unwrap();
    let mut holders = Vec::new();
    for task in [1, 2] {
        let acquired = events
            .iter()
            .find(|e| e.task_id == Some(TaskId::new(task)) && e.kind == "lease_acquired")
            .unwrap();
        assert!(acquired.id.as_i64() < ended);
        holders.push(acquired.payload["pid"].clone());
        assert!(first_event(&events, task, &["agent_started"]).unwrap() < ended);
    }
    assert!(holders[0].is_number());
    assert_eq!(holders[0], holders[1]);
    assert!(first_event(&events, 3, &["run_claimed"]).is_none());

    // Landing unblocks the dependent; the resident loop claims it from the landed main.
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let landed = git_out(&repo, &["rev-parse", "main"]);
    assert_ne!(landed, first.result_commit().cloned().unwrap());
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().runs[0]
            .result_commit()
            .map(CommitSha::as_str),
        Some(landed.as_str())
    );
    wait_until(&db, common::STEP_LIMIT, |queue| {
        let runs = queue.show(TaskId::new(3)).unwrap().runs;
        if let Some(run) = runs.first().filter(|r| r.run_dir().is_some()) {
            finish(run);
        }
        runs.first()
            .is_some_and(|r| r.status() == RunStatus::AwaitingIntegration)
    });
    let third = queue.show(TaskId::new(3)).unwrap().runs[0].clone();
    assert_eq!(*third.base_commit(), landed);
    assert_ne!(third.base_commit(), second.base_commit());
    // The dependent was claimed only after its predecessor landed.
    let events = queue.all_events().unwrap();
    assert!(
        first_event(&events, 3, &["run_claimed"]).unwrap()
            > first_event(&events, 1, &["run_integrated"]).unwrap()
    );

    // A graceful stop ends the loop once nothing is active.
    options.stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "stopped");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 3);
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 3);
    let mut closed = backend.closed();
    closed.sort();
    assert_eq!(closed, [workspace_id(0), workspace_id(1), workspace_id(2)]);
    assert_eq!(runtime::doctor(&db, true).unwrap()["runs"], json!([]));
    // The independent second run conflicts with the first (same file) and
    // waits for a session; the dependent, built on the landing, lands cleanly.
    assert_eq!(
        integrate(&db, 2, &repo).unwrap()["outcome"],
        "needs_session"
    );
    assert_eq!(integrate(&db, 3, &repo).unwrap()["outcome"], "integrated");
    assert_eq!(
        git_out(&repo, &["rev-list", "--count", &format!("{landed}..main")]),
        "1"
    );
    drop(dir);
}

/// The id of `task`'s first event of one of `kinds`, in the queue's order.
fn first_event(events: &[dagq::domain::RunEvent], task: i64, kinds: &[&str]) -> Option<i64> {
    events
        .iter()
        .find(|e| e.task_id == Some(TaskId::new(task)) && kinds.contains(&e.kind.as_str()))
        .map(|e| e.id.as_i64())
}

/// A run that does not answer the exit request keeps its lease without
/// disturbing the run next to it, and is validated once its session ends.
#[test]
fn a_timed_out_run_is_kept_while_the_other_run_is_accepted() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "healthy", &[]);
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.script_for(1, HELD_AGENT);
    backend.exit_timeout = Duration::from_secs(1);
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"exit_request_timed_out")
            && queue
                .show(TaskId::new(2))
                .unwrap()
                .runs
                .first()
                .is_some_and(|r| {
                    r.status() == RunStatus::AwaitingIntegration
                        && queue.run_lease(r.id()).unwrap().is_none()
                })
    });
    let stuck = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let healthy = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert!(healthy.last_error().is_none());
    assert!(healthy.workspace_closed_at().is_some());
    // The stuck session held back the /exit that followed its review, so
    // its run is already accepted and its supervisor still holds it.
    assert_eq!(stuck.status(), RunStatus::AwaitingIntegration);
    assert!(stuck.last_error().is_none());
    assert!(queue.run_lease(stuck.id()).unwrap().is_some());
    // Its stuck_exit ask is the attention, not the run (task 104).
    wait_until(&db, Duration::from_secs(10), |queue| {
        queue.asks(AskQuery::default()).unwrap().iter().any(|a| {
            a.kind == AskKind::StuckExit
                && a.run_id.as_ref().map(RunId::as_str) == Some(stuck.id().as_str())
        })
    });
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, stuck.id()).is_none(), "{status}");
    assert!(runtime::recover(&db, stuck.id()).is_err());

    release_held_session(stuck.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2);
    for task in [1, 2] {
        assert_eq!(
            queue.show(TaskId::new(task)).unwrap().runs[0].status(),
            RunStatus::AwaitingIntegration
        );
    }
    assert!(queue.run_leases().unwrap().is_empty());
}

/// A nonzero session exit and a rejected receipt in one pass leave the
/// accepted run untouched; every run releases its lease.
#[test]
fn failed_runs_in_the_same_pass_do_not_affect_the_accepted_run() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "crashes", &[]);
    add_ready_task(&mut queue, "no receipt", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.script_for(2, "commit work; exit 7");
    backend.script_for(3, "commit work");
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]));
    let mut statuses: Vec<(i64, String)> = outcome["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["task_id"].as_i64().unwrap(),
                r["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    statuses.sort();
    assert_eq!(
        statuses,
        [
            (1, "awaiting_integration".to_owned()),
            (2, "failed".to_owned()),
            (3, "failed".to_owned())
        ]
    );
    assert!(
        queue.show(TaskId::new(3)).unwrap().runs[0]
            .last_error()
            .unwrap()
            .contains("receipt was not submitted")
    );
    assert_eq!(backend.closed(), [workspace_id(0)]);
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(runtime::doctor(&db, true).unwrap()["runs"], json!([]));
    // Failed tasks can be retried independently; the accepted one still owns its slot.
    queue.transition(TaskId::new(2), TaskAction::Ready).unwrap();
    assert!(queue.transition(TaskId::new(1), TaskAction::Ready).is_err());
    assert_eq!(queue.candidates().unwrap()[0].id(), TaskId::new(2));
}

/// Recovering one orphaned run touches neither the lease nor the processes of
/// the run that shares its supervisor.
#[test]
fn recovering_one_orphaned_run_leaves_the_other_running() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "still alive", &[]);
    let mut dead_wrapper = sleeper();
    let mut dead_agent = sleeper();
    let mut live_wrapper = sleeper();
    let mut live_agent = sleeper();
    let orphan = orphan_run(&repo, &db, "owner", dead_wrapper.id(), dead_agent.id());
    let survivor = orphan_run(&repo, &db, "owner", live_wrapper.id(), live_agent.id());
    // One supervisor, two leases; it died and took nothing with it.
    let raw = Connection::open(&db).unwrap();
    raw.execute("UPDATE run_leases SET heartbeat_at=0, pid=?1", [dead_pid()])
        .unwrap();
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["supervisors"].as_array().unwrap().len(), 1);
    assert_eq!(
        report["supervisors"][0]["run_ids"],
        json!([orphan.id(), survivor.id()])
    );
    assert_eq!(report["supervisors"][0]["stale"], true);
    assert_eq!(report["runs"].as_array().unwrap().len(), 2);
    assert!(
        report["runs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["recoverable"] == false && r["lease"]["stale"] == true)
    );
    for child in [&mut dead_wrapper, &mut dead_agent] {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["runs"][0]["recoverable"], true);
    assert_eq!(report["runs"][1]["recoverable"], false);
    assert_eq!(
        runtime::recover(&db, orphan.id()).unwrap()["run"]["status"],
        "interrupted"
    );
    // The survivor keeps its lease, processes and status; only the orphan changed.
    assert_eq!(
        queue.run(survivor.id()).unwrap().status(),
        RunStatus::Running
    );
    let leases = queue.run_leases().unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].run_id, *survivor.id());
    assert_eq!(queue.processes(survivor.id()).unwrap().len(), 2);
    let error = format!("{:#}", runtime::recover(&db, survivor.id()).unwrap_err());
    assert!(error.contains("wrapper pid"), "{error}");
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["runs"].as_array().unwrap().len(), 1);
    assert_eq!(status["runs"][0]["run_id"], json!(survivor.id()));
    for child in [&mut live_wrapper, &mut live_agent] {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    assert_eq!(
        runtime::recover(&db, survivor.id()).unwrap()["run"]["status"],
        "interrupted"
    );
    assert!(queue.run_leases().unwrap().is_empty());
}

/// A `running` run whose supervisor stopped heartbeating (lease 31 s old)
/// while its wrapper keeps heartbeating is adopted by the next supervisor
/// with a free slot: the lease and `supervisor_token` move to the adopter,
/// `run_adopted` records what was taken over, and the adopter drives the
/// run through receipt, idle, one exit request, validation and close.
#[test]
fn stale_lease_of_a_live_wrapper_is_adopted_and_driven_to_awaiting_integration() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    age_lease(&db, &run, 31);
    let mut queue = SqliteQueue::open(&db).unwrap();
    // Nothing else is claimable: the pass exists only for the adoption.
    assert!(queue.candidates().unwrap().is_empty());
    assert_eq!(
        runtime::status(&db).unwrap()["runs"][0]["lease"]["stale"],
        true
    );

    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished", "{outcome}");
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(outcome["runs"][0]["id"], json!(run.id()));
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);

    let detail = queue.show(TaskId::new(1)).unwrap();
    let adopted_run = &detail.runs[0];
    assert_eq!(adopted_run.status(), RunStatus::AwaitingIntegration);
    assert!(adopted_run.last_error().is_none());
    assert!(adopted_run.result_commit().is_some());
    assert!(adopted_run.workspace_closed_at().is_some());
    assert!(queue.run_leases().unwrap().is_empty());
    let adopted = adoption_events(&detail);
    assert_eq!(adopted.len(), 1, "{adopted:?}");
    let payload = adopted[0];
    assert_eq!(payload["previous_token"], "dead-supervisor");
    assert_eq!(payload["previous_pid"], json!(std::process::id()));
    let age = payload["previous_heartbeat_age_secs"].as_i64().unwrap();
    assert!(age >= 31, "{payload}");
    assert_eq!(payload["wrapper"]["pid"], json!(std::process::id()));
    assert_eq!(payload["wrapper"]["alive"], true);
    assert_eq!(payload["wrapper"]["exited_at"], Value::Null);
    assert_eq!(payload["pid"], json!(std::process::id()));
    // The adopter's token replaced the claimer's on the run.
    let adopter = payload["token"].as_str().unwrap();
    assert_ne!(adopter, "dead-supervisor");
    assert_eq!(supervisor_token_of(&db, &run), adopter);
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("lease_acquired") < position("run_adopted"));
    assert!(position("run_adopted") < position("receipt_observed"));
    // The session stays open through validation and the review (the
    // stand-in `claude` prints no verdict, so the review fails); /exit
    // follows (ADR-0027).
    assert!(position("receipt_observed") < position("session_idle_observed"));
    assert!(position("session_idle_observed") < position("supervision_finished"));
    assert!(position("supervision_finished") < position("validation_finished"));
    assert!(position("validation_finished") < position("review_started"));
    assert!(position("review_started") < position("exit_requested"));
    assert!(position("exit_requested") < position("session_exited"));
    assert!(position("session_exited") < position("workspace_closed"));
    assert!(position("workspace_closed") < position("review_failed"));
    assert!(position("review_failed") < position("lease_released"));
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert!(!kinds.contains(&"runtime_error"));
    assert!(!kinds.contains(&"run_recovered"));
    // The adoption is a fact of this run's history, not a second run.
    assert_eq!(detail.runs.len(), 1);
    let event = detail
        .events
        .iter()
        .find(|e| e.kind == "run_adopted")
        .unwrap();
    assert_eq!(event.run_id.as_ref(), Some(run.id()));
}

/// The incident of task 15: the supervisor was killed a moment ago, so its
/// lease pid is dead while its heartbeat is still fresh. The pid rule alone
/// makes the lease stale, and the run is adopted without waiting for the TTL.
#[test]
fn dead_supervisor_pid_with_a_fresh_heartbeat_is_adopted() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "killed");
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET pid=?2, heartbeat_at=unixepoch() WHERE run_id=?1",
            rusqlite::params![run.id(), dead_pid()],
        )
        .unwrap();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(
        outcome["runs"][0]["status"], "awaiting_integration",
        "{outcome}"
    );
    assert_eq!(outcome["errors"], json!([]));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let adopted = adoption_events(&detail);
    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0]["previous_token"], "killed");
    assert_ne!(adopted[0]["previous_pid"], json!(std::process::id()));
    assert!(adopted[0]["previous_heartbeat_age_secs"].as_i64().unwrap() < 30);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
}

/// Everything adoption must leave alone: a fresh lease; a stale lease whose
/// wrapper is dead or silent (that is `recover`'s case); `claimed` /
/// `starting` runs; runs without a lease row; and an `integrating` run. A
/// supervisor pass over them adopts nothing and writes no `run_adopted`
/// event. The runs whose dead supervisor's lease is stale and whose
/// processes are all gone (the dead wrapper, `starting`, `claimed`) it
/// recovers itself (task 236); the silent wrapper's live process and the
/// fresh lease still block that.
#[test]
fn fresh_leases_dead_wrappers_early_runs_leaseless_and_integrating_runs_are_not_adopted() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(TaskId::new(1), TaskAction::Draft).unwrap();
    for title in [
        "fresh",
        "dead wrapper",
        "silent wrapper",
        "starting",
        "leaseless",
        "claimed",
    ] {
        add_ready_task(&mut queue, title, &[]);
    }
    let raw = Connection::open(&db).unwrap();
    // Fresh lease, live wrapper (children that heartbeat nothing but are alive; the
    // lease is what decides here).
    let mut children: Vec<std::process::Child> = Vec::new();
    let mut spawn = || {
        let child = sleeper();
        let pid = child.id();
        children.push(child);
        pid
    };
    let fresh = orphan_run(&repo, &db, "fresh-owner", spawn(), spawn());
    // Stale lease, wrapper dead: recoverable, never adopted.
    let dead_wrapper = orphan_run(&repo, &db, "gone", dead_pid(), dead_pid());
    // Stale lease, wrapper alive but silent for longer than the TTL.
    let silent_wrapper = orphan_run(&repo, &db, "gone", spawn(), spawn());
    raw.execute(
        "UPDATE run_leases SET heartbeat_at=unixepoch()-31, pid=?1 WHERE token='gone'",
        [dead_pid()],
    )
    .unwrap();
    raw.execute(
        "UPDATE run_processes SET heartbeat_at=unixepoch()-31 WHERE run_id IN (?1, ?2)",
        [&dead_wrapper.id(), &silent_wrapper.id()],
    )
    .unwrap();
    // `starting` with a stale lease: register_wrapper needs the claimer's token.
    use dagq::{domain::ClaimOutcome, infrastructure::runtime_store::RunPlan};
    let base = queue.run(fresh.id()).unwrap().base_commit().clone();
    let ClaimOutcome::Claimed { run: starting } =
        queue.claim_for_supervisor(&base, "gone-early").unwrap()
    else {
        panic!()
    };
    queue
        .plan_run(
            starting.id(),
            "gone-early",
            &RunPlan {
                repo_path: "/test".into(),
                run_dir: "/run".into(),
                branch: "dagq/starting".into(),
                worktree_path: "/run/worktree".into(),
                receipt_path: "/run/receipt.json".into(),
                log_path: "/run/log".into(),
            },
        )
        .unwrap();
    // Its wrapper exited before the agent registered: still `starting`,
    // which only `recover` handles (task 236).
    let early_wrapper = dead_pid();
    queue
        .workspace_created(starting.id(), "gone-early", "ws-starting")
        .unwrap();
    queue
        .register_wrapper(starting.id(), "gone-early", early_wrapper)
        .unwrap();
    queue
        .wrapper_exited(starting.id(), early_wrapper, 1)
        .unwrap();
    let starting = queue.run(starting.id()).unwrap();
    assert_eq!(starting.status(), RunStatus::Starting);
    // `running` without a lease: abandoned by a runtime error or recovered.
    let leaseless = orphan_run(&repo, &db, "abandoned", spawn(), spawn());
    queue
        .abandon_run(
            leaseless.id(),
            "abandoned",
            "exit request timed out",
            &ReasonCode::Other.into(),
        )
        .unwrap();
    assert!(queue.run_lease(leaseless.id()).unwrap().is_none());
    // `claimed` with a stale lease.
    let ClaimOutcome::Claimed { run: claimed } =
        queue.claim_for_supervisor(&base, "gone-early").unwrap()
    else {
        panic!()
    };
    raw.execute(
        "UPDATE run_leases SET heartbeat_at=0, pid=?1 WHERE token='gone-early'",
        [dead_pid()],
    )
    .unwrap();
    assert!(queue.candidates().unwrap().is_empty());
    let before = runtime::doctor(&db, true).unwrap();
    let health = |report: &Value, run: &TaskRun| -> Value {
        report["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["run_id"] == json!(run.id()))
            .cloned()
            .unwrap()
    };
    assert_eq!(health(&before, &dead_wrapper)["recoverable"], true);
    assert_eq!(health(&before, &silent_wrapper)["recoverable"], false);
    assert_eq!(health(&before, &fresh)["recoverable"], false);
    assert_eq!(health(&before, &starting)["recoverable"], true);
    assert_eq!(health(&before, &claimed)["recoverable"], true);

    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["outcome"], "finished", "{outcome}");
    assert_eq!(outcome["errors"], json!([]));
    for run in [
        &fresh,
        &dead_wrapper,
        &silent_wrapper,
        &starting,
        &leaseless,
        &claimed,
    ] {
        let detail = queue.show(run.task_id()).unwrap();
        assert!(
            adoption_events(&detail).is_empty(),
            "run {} of task {} was adopted",
            run.id(),
            run.task_id()
        );
    }
    for run in [&fresh, &silent_wrapper, &leaseless] {
        assert_eq!(queue.run(run.id()).unwrap().status(), run.status());
        assert!(
            payloads(&queue.show(run.task_id()).unwrap(), "run_recovered").is_empty(),
            "run {} was recovered",
            run.id()
        );
    }
    for run in [&dead_wrapper, &starting, &claimed] {
        assert_ne!(queue.run(run.id()).unwrap().status(), run.status());
        let detail = queue.show(run.task_id()).unwrap();
        let recovered = payloads(&detail, "run_recovered");
        assert_eq!(recovered.len(), 1, "run {}", run.id());
        assert_eq!(recovered[0]["by"], "supervisor");
        assert_eq!(recovered[0]["previous_status"], json!(run.status()));
        assert_eq!(recovered[0]["status"], "interrupted");
        assert_eq!(recovered[0]["lease_deleted"], true);
        assert!(queue.run_lease(run.id()).unwrap().is_none());
    }
    // Leases, tokens and doctor's verdicts of the rest are exactly as
    // before the pass.
    assert_eq!(
        queue.run_lease(fresh.id()).unwrap().unwrap().token,
        "fresh-owner"
    );
    assert_eq!(
        queue.run_lease(silent_wrapper.id()).unwrap().unwrap().token,
        "gone"
    );
    assert!(queue.run_lease(leaseless.id()).unwrap().is_none());
    let after = runtime::doctor(&db, true).unwrap();
    for run in [&fresh, &silent_wrapper] {
        assert_eq!(
            health(&after, run)["recoverable"],
            health(&before, run)["recoverable"]
        );
        assert_eq!(
            health(&after, run)["lease"]["stale"],
            health(&before, run)["lease"]["stale"]
        );
    }
    for child in &mut children {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

/// An `integrating` run is never adopted, however stale its lease: a
/// crashed landing is `recover`'s case and goes back to the merge queue.
#[test]
fn integrating_run_with_a_stale_lease_is_not_adopted() {
    let (_dir, repo, db, run) = awaiting_run();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let main = git_out(&repo, &["rev-parse", "main"]);
    queue
        .begin_integration(run.id(), "crashed", &sha(&main))
        .unwrap();
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE run_leases SET heartbeat_at=0, pid=?1", [dead_pid()])
        .unwrap();
    assert!(queue.candidates().unwrap().is_empty()); // The dependent still waits.
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::Integrating
    );
    assert_eq!(queue.run_lease(run.id()).unwrap().unwrap().token, "crashed");
    assert!(adoption_events(&queue.show(TaskId::new(1)).unwrap()).is_empty());
    assert_eq!(
        runtime::recover(&db, run.id()).unwrap()["run"]["status"],
        "awaiting_integration"
    );
}

/// The previous supervisor already asked the session to exit: the adopter
/// rebuilds that from the `exit_requested` event and does not send `/exit`
/// again, and its receipt observation is not repeated either.
#[test]
fn adopter_does_not_repeat_an_exit_request_the_previous_supervisor_sent() {
    let (_dir, repo, db) = fixture();
    // The session ends on its own once the adopter has watched it for a
    // while, as it would after the /exit that was already typed.
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        &format!("commit work; receipt \"$(git rev-parse HEAD)\"; idle; {HOLD}"),
    ));
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let receipt = PathBuf::from(run.receipt_path().unwrap());
    wait_until(&db, Duration::from_secs(10), |_| receipt.is_file());
    // What the previous supervisor recorded before it died.
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_runtime_event(
            run.id(),
            "receipt_observed",
            json!({"path": run.receipt_path(), "validated": false}),
        )
        .unwrap();
    queue
        .record_runtime_event(run.id(), "session_idle_observed", json!({}))
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            "exit_requested",
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
        )
        .unwrap();
    age_lease(&db, &run, 31);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !adoption_events(&queue.show(TaskId::new(1)).unwrap()).is_empty()
    });
    // Several passes over the idle session send nothing.
    thread::sleep(Duration::from_millis(500));
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(
        outcome["runs"][0]["status"], "awaiting_integration",
        "{outcome}"
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(adoption_events(&detail).len(), 1);
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert_eq!(
        kinds.iter().filter(|k| **k == "receipt_observed").count(),
        1
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "session_idle_observed")
            .count(),
        1
    );
    assert!(!kinds.contains(&"exit_request_timed_out"));
}

/// The exit timeout of an adopted run restarts at adoption: a session that
/// still ignores the earlier request is reported after the adopter's own
/// timeout, with one `exit_requested` event in total, and the adopter keeps
/// the run until the session ends.
#[test]
fn adopted_exit_request_times_out_from_the_adoption() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_timeout = Duration::from_secs(1);
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_runtime_event(
            run.id(),
            "exit_requested",
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
        )
        .unwrap();
    age_lease(&db, &run, 31);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"exit_request_timed_out")
    });
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Running);
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_ne!(lease.token, "dead-supervisor");
    // Let the fake session out, the way a person answering it would.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(adoption_events(&detail).len(), 1);
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "exit_request_timed_out")
            .count(),
        1
    );
    assert!(!kinds.contains(&"runtime_error"));
    assert!(queue.run_leases().unwrap().is_empty());
}

/// A run whose exit request already timed out under the previous supervisor
/// is adopted without recording the timeout again, and is validated once
/// its session ends.
#[test]
fn adopted_run_does_not_record_an_exit_timeout_twice() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_timeout = Duration::from_secs(1);
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let mut queue = SqliteQueue::open(&db).unwrap();
    for kind in ["exit_requested", "exit_request_timed_out"] {
        queue
            .record_runtime_event(
                run.id(),
                kind,
                json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
            )
            .unwrap();
    }
    age_lease(&db, &run, 31);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !adoption_events(&queue.show(TaskId::new(1)).unwrap()).is_empty()
    });
    // Well past the adopter's own timeout.
    thread::sleep(Duration::from_millis(1500));
    let kinds = event_kinds(&queue.show(TaskId::new(1)).unwrap())
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "exit_request_timed_out")
            .count(),
        1
    );
    assert!(queue.run_lease(run.id()).unwrap().is_some());
    // The timeout the dead supervisor recorded without its ask gets one
    // stuck_exit ask from the adopter, once.
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::StuckExit);
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "exit_request_timed_out")
            .count(),
        1
    );
    assert!(!kinds.contains(&"runtime_error"));
    assert!(queue.read_ask(asks[0].id).unwrap().closed_at.is_some());
    // The other notification is the ask of its stand-in review.
    let notifications = backend.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 2, "{notifications:?}");
    assert!(notifications[1].0.ends_with("approve_landing"));
}

/// An adopted run whose timeout already has a stuck_exit ask (answered by
/// the inbox here, the session still up) is not asked again; the runtime
/// only closes the answered ask once the session exits.
#[test]
fn adopted_run_does_not_ask_about_its_exit_twice() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_timeout = Duration::from_secs(1);
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let mut queue = SqliteQueue::open(&db).unwrap();
    for kind in ["exit_requested", "exit_request_timed_out"] {
        queue
            .record_runtime_event(
                run.id(),
                kind,
                json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
            )
            .unwrap();
    }
    let asked = queue
        .ask(NewAsk {
            kind: AskKind::StuckExit,
            task_id: None,
            run_id: Some(run.id().clone()),
            question: "send /exit".into(),
            options: Vec::new(),
            asked_by: "supervisor".into(),
            reason_category: dagq::domain::AskReason::RecoveryFailed,
            finding_id: None,
        })
        .unwrap()
        .ask;
    queue.answer(asked.id, "sent /exit").unwrap();
    age_lease(&db, &run, 31);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !adoption_events(&queue.show(TaskId::new(1)).unwrap()).is_empty()
    });
    thread::sleep(Duration::from_millis(2500));
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
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let closed = queue.read_ask(asked.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(closed.answer.as_deref(), Some("sent /exit"));
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "ask_answered").count(), 1);
    // Only the ask of its stand-in review notifies.
    let notifications = backend.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1, "{notifications:?}");
    assert!(notifications[0].0.ends_with("approve_landing"));
}

/// The supervisor died after the wrapper reported its exit but before
/// `supervision_finished`, and, separately, while a run was `validating`.
/// Both are adopted: the first finishes supervision from the recorded exit,
/// the second restarts validation from the receipt and worktree.
#[test]
fn exited_wrapper_and_validating_runs_are_adopted_and_validated() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "validating", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    // The first session goes idle after its receipt and then ends on its own
    // (a person's /exit by the old procedure): the adopter must not
    // send /exit to a session that already exited.
    backend.script_for(
        1,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; sleep 1",
    );
    let exited = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-a");
    let validating = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-b");
    backend.join(); // Both sessions end by themselves.
    for run in [&exited, &validating] {
        assert!(event_kinds(&queue.show(run.task_id()).unwrap()).contains(&"session_exited"));
        assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Running);
    }
    queue.finish_supervision(validating.id(), "dead-b").unwrap();
    assert_eq!(
        queue.run(validating.id()).unwrap().status(),
        RunStatus::Validating
    );
    age_lease(&db, &exited, 31);
    age_lease(&db, &validating, 31);

    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    let mut closed = backend.closed();
    closed.sort();
    assert_eq!(closed, [workspace_id(0), workspace_id(1)]);
    for run in [&exited, &validating] {
        let detail = queue.show(run.task_id()).unwrap();
        let after = &detail.runs[0];
        assert_eq!(
            after.status(),
            RunStatus::AwaitingIntegration,
            "{}",
            run.id()
        );
        assert!(after.last_error().is_none(), "{after:?}");
        assert!(!event_kinds(&detail).contains(&"exit_requested"));
        assert!(after.result_commit().is_some());
        assert!(after.workspace_closed_at().is_some());
        let adopted = adoption_events(&detail);
        assert_eq!(adopted.len(), 1);
        assert_eq!(adopted[0]["wrapper"]["alive"], Value::Null);
        assert!(adopted[0]["wrapper"]["exited_at"].is_number());
        let kinds = event_kinds(&detail);
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == "supervision_finished")
                .count(),
            1
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == "validation_finished")
                .count(),
            1
        );
        // Validation checks the receipt only; integrate runs the commands.
        assert!(!kinds.contains(&"verification_command"));
    }
    let detail = queue.show(exited.task_id()).unwrap();
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("session_exited") < position("run_adopted"));
    assert!(position("run_adopted") < position("supervision_finished"));
    let detail = queue.show(validating.task_id()).unwrap();
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("supervision_finished") < position("run_adopted"));
    assert!(position("run_adopted") < position("validation_finished"));
    assert!(queue.run_leases().unwrap().is_empty());
}

/// Two supervisors with free slots find the same stale lease at once: the
/// transaction lets exactly one of them adopt, the other sees no lease
/// under the old token and moves on. One `run_adopted` event, one owner.
#[test]
fn two_supervisors_racing_for_one_stale_lease_adopt_it_once() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    age_lease(&db, &run, 31);
    let racers: Vec<_> = (0..2)
        .map(|_| {
            let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
            thread::spawn(move || supervise(&db, &repo, &backend))
        })
        .collect();
    let outcomes: Vec<Value> = racers
        .into_iter()
        .map(|racer| joined(racer, "a racing supervisor thread to return").unwrap())
        .collect();
    backend.join();
    let driven: Vec<&Value> = outcomes
        .iter()
        .filter(|o| !o["runs"].as_array().unwrap().is_empty())
        .collect();
    assert_eq!(driven.len(), 1, "{outcomes:?}");
    assert_eq!(driven[0]["runs"][0]["status"], "awaiting_integration");
    assert!(outcomes.iter().all(|o| o["errors"] == json!([])));
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let adopted = adoption_events(&detail);
    assert_eq!(adopted.len(), 1, "{adopted:?}");
    assert_eq!(supervisor_token_of(&db, &run), adopted[0]["token"]);
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);

    // The queue method itself: a second adoption under the old token, or
    // one against a fresh lease, takes nothing.
    add_ready_task(&mut queue, "second", &[]);
    add_ready_task(&mut queue, "early", &[]);
    let second = orphan_run(&repo, &db, "fresh", std::process::id(), std::process::id());
    assert!(
        queue
            .adopt_run(second.id(), "fresh", "eager", 1, json!({}))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        queue.run_lease(second.id()).unwrap().unwrap().token,
        "fresh"
    );
    age_lease(&db, &second, 31);
    let taken = queue
        .adopt_run(second.id(), "fresh", "first", 1, json!({"pid": 1}))
        .unwrap()
        .unwrap();
    assert_eq!(taken.status(), RunStatus::Running);
    assert!(
        queue
            .adopt_run(second.id(), "fresh", "second", 2, json!({}))
            .unwrap()
            .is_none()
    );
    let lease = queue.run_lease(second.id()).unwrap().unwrap();
    assert_eq!((lease.token.as_str(), lease.pid), ("first", 1));
    assert!(SystemClock.now() - lease.heartbeat_at <= 5);
    assert_eq!(supervisor_token_of(&db, &second), "first");
    assert!(queue.holds_lease(second.id(), "first").unwrap());
    assert!(!queue.holds_lease(second.id(), "fresh").unwrap());
    assert!(queue.has_run_event(second.id(), "run_adopted").unwrap());
    assert!(
        !queue
            .has_run_event(second.id(), "receipt_observed")
            .unwrap()
    );
    let payload = &adoption_events(&queue.show(second.task_id()).unwrap())[0].clone();
    assert_eq!(payload["wrapper"], json!({"pid": 1}));
    assert_eq!(payload["previous_token"], "fresh");
    assert_eq!(payload["previous_pid"], json!(std::process::id()));
    // A `starting` run is refused by the method too, stale or not.
    use dagq::domain::ClaimOutcome;
    let ClaimOutcome::Claimed { run: early } = queue
        .claim_for_supervisor(second.base_commit(), "early")
        .unwrap()
    else {
        panic!()
    };
    age_lease(&db, &early, 31);
    assert!(
        queue
            .adopt_run(early.id(), "early", "eager", 1, json!({}))
            .unwrap()
            .is_none()
    );
    // The status/doctor lists the adopted run under the adopter's token like any other.
    let status = runtime::status(&db).unwrap();
    let holders: Vec<&Value> = status["supervisors"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| {
            s["run_ids"]
                .as_array()
                .unwrap()
                .contains(&json!(second.id()))
        })
        .collect();
    assert_eq!(holders.len(), 1);
    assert_eq!(holders[0]["pid"], 1);
}

/// A resident supervisor whose lease was taken over (here by hand, as an
/// adopter or `recover` would) drops the run from its slots without writing
/// anything more about it, while the adopter drives the run to the end.
#[test]
fn a_supervisor_that_lost_its_lease_stops_touching_the_run() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let options = supervise_options(2, false);
    let original = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(20), |queue| {
        queue
            .active_runs()
            .unwrap()
            .first()
            .is_some_and(|r| r.status() == RunStatus::Running)
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.active_runs().unwrap().remove(0);
    // Draining: the original keeps driving its run but adopts and claims
    // nothing more, so it cannot take the lease back once it loses it.
    options.stop.store(true, Ordering::SeqCst);
    // The lease changes hands: another token, stale, as a killed
    // supervisor's would look to an adopter.
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET token='taken', heartbeat_at=unixepoch()-31 WHERE run_id=?1",
            [&run.id()],
        )
        .unwrap();
    // The original notices within a tick, drops the run and, draining with
    // nothing active, exits.
    let outcome = joined(original, "the first supervisor thread to return").unwrap();
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Running);
    assert_eq!(queue.run_lease(run.id()).unwrap().unwrap().token, "taken");
    let adopter = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(
        adopter["runs"][0]["status"], "awaiting_integration",
        "{adopter}"
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(outcome["outcome"], "stopped");
    assert_eq!(outcome["runs"], json!([]));
    assert_eq!(outcome["errors"][0]["run_id"], json!(run.id()));
    assert!(
        outcome["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("held by another process"),
        "{outcome}"
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(detail.runs[0].last_error().is_none());
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    let adopted = adoption_events(&detail);
    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0]["previous_token"], "taken");
    assert!(queue.supervisors().unwrap().is_empty());
}
