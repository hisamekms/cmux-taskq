//! Runtime tests: The recovery job of failed runs, dead runs and silent
//! wrappers.
use crate::runtime_support;

use runtime_support::*;

/// A worker script that fails its first run without a commit (the
/// session dies before it did anything) and lands every later one.
fn fails_once(dir: &Path) -> String {
    let mark = dir.join("failed-once");
    format!(
        "if [ ! -f '{mark}' ]; then : > '{mark}'; exit 7; fi; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
        mark = mark.display()
    )
}

/// The events of `kind` of one run, in order.
fn run_payloads(detail: &dagq::domain::TaskDetail, run: &TaskRun, kind: &str) -> Vec<Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == kind && e.run_id.as_ref() == Some(run.id()))
        .map(|e| e.payload.clone())
        .collect()
}

/// ADR-0047 decisions 39 and 40: a failed run is taken by the recovery job
/// (`recovery_requested` with `alert: failed`); its `retry` of a run whose
/// branch holds no commit of its own is applied, recorded as
/// `auto_repaired` (`layer: recovery`), `triage_finished` and
/// `recovery_finished`, the workspace is closed, and the next run lands.
#[test]
fn a_failed_run_without_commits_is_retried_by_its_recovery_job_and_lands() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &fails_once(db.parent().unwrap()));
    let reviewer =
        TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]).with_triages(&[repair(
            json!({"action": "retry"}),
            "the session died on its own",
        )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2);
    let first = &detail.runs[0];
    assert_eq!(first.status(), RunStatus::Failed);
    assert_landed_run(&detail.runs[1], &repo, &base);
    assert!(queue.run_leases().unwrap().is_empty());

    let events = queue.run_events(first.id()).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    let requested = &events[position(&kinds, "recovery_requested")].payload;
    assert_eq!(requested["alert"], "failed");
    assert_eq!(requested["attempt"], 1);
    assert_eq!(requested["status"], "failed");
    assert_eq!(requested["evidence"].as_array().unwrap().len(), 1);
    assert!(position(&kinds, "recovery_requested") < position(&kinds, "triage_started"));
    let finished = &events[position(&kinds, "triage_finished")].payload;
    assert_eq!(finished["action"], "retry");
    assert_eq!(finished["alert"], "failed");
    assert_eq!(finished["recovery_attempt"], 1);
    assert_eq!(finished["verdict"], "repair");
    assert_eq!(finished["confidence"], "high");
    assert_eq!(finished["reason"], "the session died on its own");
    let repaired = &events[position(&kinds, "auto_repaired")].payload;
    assert_eq!(repaired["layer"], "recovery");
    assert_eq!(repaired["repair"], "retry");
    assert_eq!(repaired["alert"], "failed");
    assert_eq!(repaired["conditions"]["own_commits"], false);
    let recovered = &events[position(&kinds, "recovery_finished")].payload;
    assert_eq!(recovered["applied"], json!(["retry"]));
    assert_eq!(recovered["escalated"], false);
    let closed = &events[position(&kinds, "workspace_closed")].payload;
    assert_eq!(closed["by"], "triage");
    assert!(position(&kinds, "triage_finished") < position(&kinds, "workspace_closed"));
    assert!(other_asks(&mut queue, true).is_empty());

    // The job read the run, the rules and the verdict schema.
    let prompts = reviewer.triage_prompts();
    assert_eq!(prompts.len(), 1);
    let (prompt, dir) = &prompts[0];
    assert_eq!(dir, Path::new(first.run_dir().unwrap()));
    for expected in [
        "which ended failed; its session is gone",
        "raised the alert failed",
        "Acceptance criteria:\nworks",
        "Last error of the run:\nsession exited with code 7",
        "Final screen of the session",
        "Earlier runs of the task:\nnone",
        "retry_inherit",
        "\"verdict\": \"repair\" | \"escalate\"",
    ] {
        assert!(prompt.contains(expected), "{expected:?} in {prompt}");
    }
    assert!(dir.join("recovery-failed-1.prompt.txt").is_file());
    assert!(dir.join("recovery-failed-1.out").is_file());
}

/// A `retry` of a run whose branch holds commits would throw them away, so
/// the runtime does not apply it (ADR-0047 decision 40): the whole verdict
/// becomes the `decide` ask, with the job's diagnosis, its actions as the
/// recommendation, its options added and its reason category.
#[test]
fn a_retry_of_a_run_with_commits_is_refused_and_asked_with_the_jobs_options() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; exit 7",
    );
    let reviewer =
        TestReviewer::new(&[verdict("pass", &[], "unused")]).with_triages(&[recovery(json!({
            "verdict": "repair",
            "confidence": "high",
            "diagnosis": "flaky",
            "actions": [{"action": "retry"}],
            "options": ["retry anyway"],
            "reason_category": "discard",
        }))]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1);
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Failed);
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert!(payloads(&detail, "auto_repaired").is_empty());
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["action"], "ask");
    assert_eq!(finished[0]["status"], "failed");
    let recovered = payloads(&detail, "recovery_finished");
    assert_eq!(recovered[0]["escalated"], true);
    assert_eq!(recovered[0]["reason_category"], "discard");
    let ask = queue
        .read_ask(AskId::new(finished[0]["ask_id"].as_i64().unwrap()))
        .unwrap();
    assert_eq!(recovered[0]["ask_id"], json!(ask.id));
    assert_eq!(ask.kind, AskKind::Decide);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.asked_by, "supervisor");
    assert_eq!(ask.options, ["retry", "resume", "cancel", "retry anyway"]);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::Discard);
    for part in [
        "alert: failed",
        "its branch holds commits of its own",
        "Why a person: discard",
        "Diagnosis: flaky",
        "Recommended: [{\"action\":\"retry\"}]",
        "Last error: session exited with code 7",
        "recovery-failed-1.prompt.txt",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    assert!(ask.is_open());
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);
    // The run is no attention: its ask is.
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    assert_eq!(
        ask_attention(&status, ask.id)[0]["next"],
        format!("answer ask {}", ask.id)
    );
    assert!(
        ask.question
            .contains("Any other option goes back to the recovery job"),
        "{}",
        ask.question
    );

    // The person picks the job's own option: the supervisor hands it back
    // to the job, whose next round reads it (ADR-0047 decision 40).
    queue.answer(ask.id, "retry anyway").unwrap();
    assert_eq!(
        ask_attention(&runtime::status(&db).unwrap(), ask.id)[0]["next"],
        format!("applying the answer of ask {} (runtime)", ask.id)
    );
    let reviewer =
        TestReviewer::new(&[verdict("pass", &[], "unused")]).with_triages(&[recovery(json!({
            "verdict": "escalate",
            "confidence": "high",
            "diagnosis": "the person wants a retry, which throws the commits away",
            "reason_category": "discard",
        }))]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1);
    let decided = payloads(&detail, "triage_decided");
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0]["answer"], "retry anyway");
    assert_eq!(decided[0]["action"], "recover");
    assert_eq!(decided[0]["status"], "failed");
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 2, "{requested:?}");
    assert_eq!(requested[1]["alert"], "failed");
    assert_eq!(requested[1]["attempt"], 2);
    assert_eq!(requested[1]["person_answer"]["answer"], "retry anyway");
    assert_eq!(requested[1]["person_answer"]["ask_id"], json!(ask.id));
    let (prompt, _) = &reviewer.triage_prompts()[0];
    assert!(prompt.contains("retry anyway"), "{prompt}");
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 2);
    assert_eq!(finished[1]["action"], "ask");
    assert_eq!(finished[1]["recovery_attempt"], 2);
}

/// `resume`: the run becomes `needs_session` with the job's instruction,
/// its workspace is closed, and the supervisor resumes its session with a
/// request naming the instruction; the resumed run is validated, reviewed
/// and landed like any other.
#[test]
fn a_failed_run_the_recovery_job_resumes_is_resumed_in_its_session_and_lands() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, "commit work; exit 7");
    backend.resume_script_for(
        1,
        "await_message; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let reviewer =
        TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]).with_triages(&[repair(
            json!({"action": "resume", "instruction": "write the receipt for your commit"}),
            "the work is committed but the receipt is missing",
        )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1);
    assert_landed_run(&detail.runs[0], &repo, &base);
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["action"], "resume");
    assert_eq!(finished[0]["status"], "needs_session");
    assert_eq!(
        finished[0]["instruction"],
        "write the receipt for your commit"
    );
    let repaired = payloads(&detail, "auto_repaired");
    assert_eq!(repaired[0]["repair"], "resume");
    assert_eq!(repaired[0]["layer"], "recovery");
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "triage_finished") < position(&kinds, "workspace_closed"));
    assert!(position(&kinds, "workspace_closed") < position(&kinds, "resume_started"));
    assert_eq!(
        payloads(&detail, "resume_started")[0]["reason"],
        "write the receipt for your commit"
    );
    assert_eq!(backend.closed()[0], WORKSPACE_ID);
    let text = &backend.texts()[0].1;
    for expected in [
        "the supervisor's triage sent it back to this session to finish",
        "Reason: write the receipt for your commit",
        "1. Do what the reason asks in this worktree and commit",
    ] {
        assert!(text.contains(expected), "{expected:?} in {text}");
    }
}

/// `escalate`: a `decide` ask for the inbox, the run stays `failed`; a cmux
/// failure closing the workspace is recorded and the round goes on. The
/// supervisor applies the answer (`cancel` here) and closes the ask.
#[test]
fn an_escalation_waits_for_a_person_and_the_supervisor_applies_the_answer() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, "commit work; exit 7");
    backend.close_fail = true;
    let reviewer =
        TestReviewer::new(&[verdict("pass", &[], "unused")]).with_triages(&[recovery(json!({
            "verdict": "escalate",
            "confidence": "high",
            "diagnosis": "the acceptance cannot be met",
            "question": "Is task 1 still wanted?",
        }))]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Failed);
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished[0]["action"], "ask");
    assert_eq!(finished[0]["status"], "failed");
    let ask = queue
        .read_ask(AskId::new(finished[0]["ask_id"].as_i64().unwrap()))
        .unwrap();
    assert_eq!(ask.options, ["retry", "resume", "cancel"]);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::RecoveryFailed);
    for part in [
        "the recovery job could not repair it",
        "Why a person: recovery_failed",
        "Diagnosis: the acceptance cannot be met",
        "Question: Is task 1 still wanted?",
        "Last error: session exited with code 7",
        "recovery-failed-1.prompt.txt",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);
    // The close failed: recorded, the workspace kept, the ask still made.
    let cleanup = payloads(&detail, "cleanup_failed");
    assert_eq!(cleanup.len(), 1);
    assert_eq!(cleanup[0]["workspace_id"], WORKSPACE_ID);
    assert!(
        cleanup[0]["message"]
            .as_str()
            .unwrap()
            .contains("injected workspace close failure")
    );
    assert!(run.workspace_closed_at().is_none());
    assert!(!event_kinds(&detail).contains(&"workspace_closed"));
    assert_eq!(run.last_error(), Some("session exited with code 7"));
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");

    // An answer the supervisor applies is its own, not a person's.
    queue.answer(ask.id, "cancel").unwrap();
    let answered = queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .rfind(|e| e.kind == "ask_answered")
        .unwrap();
    assert_eq!(answered.payload["runtime_delivers"], true);
    assert_eq!(
        ask_attention(&runtime::status(&db).unwrap(), ask.id)[0]["next"],
        format!("applying the answer of ask {} (runtime)", ask.id)
    );
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    let decided = payloads(&detail, "triage_decided");
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0]["answer"], "cancel");
    assert_eq!(decided[0]["ask_id"], ask.id.as_i64());
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert!(ask_attention(&runtime::status(&db).unwrap(), ask.id).is_empty());
}

/// `retry_inherit` of a run whose branch has commits (task 358's retry):
/// the task is ready again and its next run carries the branch over. That
/// run fails too, and its recovery job cannot start: nothing moves, and the
/// run waits to be recovered by hand (`triage_failed`, ADR-0047 decision 40).
/// A low-confidence repair and a second `retry_inherit` of the task are not
/// applied.
#[test]
fn retry_inherit_carries_the_branch_over_once_and_a_failed_job_is_recovered_by_hand() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; exit 7",
    );
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "unused")]).with_triages(&[repair(
        json!({"action": "retry_inherit"}),
        "a flaky test killed the session after the work was done",
    )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2, "{:?}", event_kinds(&detail));
    let (first, second) = (detail.runs[0].clone(), detail.runs[1].clone());
    let finished = run_payloads(&detail, &first, "triage_finished");
    assert_eq!(finished[0]["action"], "retry_inherit");
    let head = finished[0]["inherit"]["head"].as_str().unwrap().to_owned();
    assert_eq!(
        finished[0]["inherit"]["branch"],
        json!(format!("dagq/{}", first.id()))
    );
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", first.id())]
        ),
        head
    );
    let repaired = run_payloads(&detail, &first, "auto_repaired");
    assert_eq!(repaired[0]["repair"], "retry_inherit");
    assert_eq!(repaired[0]["conditions"]["own_commits"], true);
    let inherited = run_payloads(&detail, &second, "run_inherited");
    assert_eq!(inherited[0]["inherit_from_run"], json!(first.id()));
    assert_eq!(inherited[0]["head"], json!(head));

    // The second run's job cannot start: recover it by hand.
    assert_eq!(second.status(), RunStatus::Failed);
    let failed = run_payloads(&detail, &second, "triage_failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["alert"], "failed");
    assert_eq!(failed[0]["code"], "job_failed");
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("the recovery job could not start")
    );
    let recovered = run_payloads(&detail, &second, "recovery_finished");
    assert_eq!(recovered[0]["outcome"], "job_failed");
    assert_eq!(recovered[0]["reason_category"], "recovery_failed");
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        run_attention_of(&status, second.id()).unwrap()["next"],
        "triage by hand",
        "{status}"
    );
    assert!(other_asks(&mut queue, true).is_empty());

    // Handed back (as a person would, by hand): a repair of low confidence
    // and a second retry_inherit of the task are asked, not applied.
    Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM run_events WHERE run_id=?1 AND kind='triage_failed'",
            [second.id()],
        )
        .unwrap();
    let reviewer =
        TestReviewer::new(&[verdict("pass", &[], "unused")]).with_triages(&[recovery(json!({
            "verdict": "repair",
            "confidence": "high",
            "diagnosis": "again",
            "actions": [{"action": "retry_inherit"}],
        }))]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2);
    let finished = run_payloads(&detail, &second, "triage_finished");
    assert_eq!(finished.last().unwrap()["action"], "ask");
    let ask = queue
        .read_ask(AskId::new(
            finished.last().unwrap()["ask_id"].as_i64().unwrap(),
        ))
        .unwrap();
    assert!(
        ask.question
            .contains("was retried with a branch carried over already"),
        "{}",
        ask.question
    );
    let requested = run_payloads(&detail, &second, "recovery_requested");
    assert_eq!(requested.len(), 2);
    assert_eq!(requested[1]["attempt"], 2);
}

/// `wait` of a run that ended changes nothing until its recheck; then the
/// job runs again for the same alert. Past [`MAX_RECOVERY_ATTEMPTS`] jobs
/// the alert is escalated without another job, and a low-confidence repair
/// in between is not applied.
#[test]
fn a_wait_runs_the_job_again_and_an_alert_past_its_jobs_is_asked() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "exit 7");
    let wait = || repair(json!({"action": "wait", "recheck_after_secs": 0}), "slow");
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "unused")]).with_triages(&[
        wait(),
        recovery(json!({
            "verdict": "repair",
            "confidence": "low",
            "diagnosis": "maybe the machine slept",
            "actions": [{"action": "wait", "recheck_after_secs": 0}],
        })),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1);
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 2, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["action"], "wait");
    assert!(finished[0]["recheck_at"].as_i64().is_some());
    assert_eq!(finished[1]["action"], "ask");
    assert_eq!(finished[1]["verdict"], "repair");
    assert_eq!(finished[1]["confidence"], "low");
    assert!(payloads(&detail, "auto_repaired").is_empty());
    let ask = queue
        .read_ask(AskId::new(finished[1]["ask_id"].as_i64().unwrap()))
        .unwrap();
    assert!(ask.question.contains("confidence low"), "{}", ask.question);

    // A person hands it back twice more (by hand): the third job waits,
    // and the fourth round asks without a job.
    queue.answer(ask.id, "look again").unwrap();
    queue.close_ask(ask.id).unwrap();
    let rewind = || {
        Connection::open(&db)
            .unwrap()
            .execute(
                "UPDATE run_events SET kind='triage_finished_old' WHERE id=(SELECT MAX(id) FROM run_events WHERE kind='triage_finished')",
                [],
            )
            .unwrap();
    };
    rewind();
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "unused")]).with_triages(&[wait()]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 3, "{:?}", event_kinds(&detail));
    assert!(requested.iter().all(|p| p["alert"] == "failed"));
    let finished = payloads(&detail, "triage_finished");
    let used_up = finished.last().unwrap();
    assert_eq!(used_up["action"], "ask");
    assert!(used_up["verdict"].is_null());
    let ask = queue
        .read_ask(AskId::new(used_up["ask_id"].as_i64().unwrap()))
        .unwrap();
    assert!(
        ask.question
            .contains("the recovery job ran 3 times for this alert already (at most 3)"),
        "{}",
        ask.question
    );
    assert_eq!(reviewer.triage_prompts().len(), 1);
}

/// The answers `resume` and `retry` of a triage's ask, applied by the
/// queue: `resume` parks the run for a session with the reason, `retry`
/// readies the task; an answer outside the options, a leased run, a run
/// that is not failed and an ask already closed are refused.
#[test]
fn triage_answers_resume_the_run_or_ready_the_task() {
    let (_dir, db, detail) = run_agent("commit work; exit 7");
    let run = detail.runs[0].clone();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = |queue: &mut SqliteQueue| {
        queue
            .ask(NewAsk {
                kind: AskKind::Decide,
                task_id: None,
                run_id: Some(run.id().clone()),
                question: "what now?".into(),
                options: vec!["retry".into(), "resume".into(), "cancel".into()],
                asked_by: "supervisor".into(),
                reason_category: dagq::domain::AskReason::RecoveryFailed,
                finding_id: None,
            })
            .unwrap()
            .ask
    };
    let first = ask(&mut queue);
    queue.answer(first.id, "resume").unwrap();
    assert_eq!(queue.triage_answers().unwrap()[0].id, first.id);
    assert!(
        queue
            .decide_triage(run.id(), first.id, "land", "x")
            .is_err()
    );
    let parked = queue
        .decide_triage(run.id(), first.id, "resume", "fix the test")
        .unwrap();
    assert_eq!(parked.status(), RunStatus::NeedsSession);
    assert_eq!(parked.last_error(), Some("fix the test"));
    assert!(queue.read_ask(first.id).unwrap().closed_at.is_some());
    assert!(queue.triage_answers().unwrap().is_empty());
    assert!(
        queue
            .decide_triage(run.id(), first.id, "retry", "x")
            .unwrap_err()
            .to_string()
            .contains("not failed or interrupted")
    );

    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='failed' WHERE id=?1",
            [&run.id()],
        )
        .unwrap();
    // An ask already applied (closed) is not applied twice, by another
    // supervisor or later.
    assert!(
        queue
            .decide_triage(run.id(), first.id, "retry", "x")
            .unwrap_err()
            .to_string()
            .contains("not an answered, unclosed ask")
    );
    let second = ask(&mut queue);
    queue.answer(second.id, "retry").unwrap();
    Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO run_leases(run_id,token,pid) VALUES (?1,'other',?2)",
            rusqlite::params![run.id(), std::process::id()],
        )
        .unwrap();
    assert!(
        queue
            .decide_triage(run.id(), second.id, "retry", "x")
            .unwrap_err()
            .to_string()
            .contains("is leased")
    );
    // A stale lease (its triage's supervisor stalled) does not block the
    // answer, and it goes with it: woken up, that supervisor cannot renew
    // it and write after the decision (ADR-0039 decision 7).
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE run_leases SET heartbeat_at=0", [])
        .unwrap();
    queue
        .decide_triage(run.id(), second.id, "retry", "x")
        .unwrap();
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Ready
    );
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert!(
        queue
            .finish_triage(
                run.id(),
                "other",
                &dagq::application::TriageAction::Retry,
                json!({}),
                Vec::new(),
            )
            .unwrap_err()
            .to_string()
            .contains("run lease is missing")
    );
}

/// A run whose wrapper died and that nobody leases is recovered by the
/// supervisor (ADR-0024 decision 3, amending ADR-0012) and triaged as
/// `interrupted`; `retry` runs the task again, and it lands.
#[test]
fn a_dead_run_nobody_leases_is_recovered_triaged_and_retried() {
    let (_dir, repo, db) = fixture();
    let orphan = orphan_run(&repo, &db, "owner", dead_pid(), dead_pid());
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases", [])
        .unwrap();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")])
        .with_triages(&[repair(json!({"action": "retry"}), "the machine restarted")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2);
    assert_eq!(detail.runs[0].id(), orphan.id());
    assert_eq!(detail.runs[0].status(), RunStatus::Interrupted);
    assert_landed_run(&detail.runs[1], &repo, &base);
    let events = queue.run_events(orphan.id()).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    let recovered = &events[position(&kinds, "run_recovered")].payload;
    assert_eq!(recovered["by"], "supervisor");
    assert_eq!(recovered["previous_status"], "running");
    assert_eq!(recovered["lease_deleted"], false);
    assert_eq!(
        events[position(&kinds, "triage_started")].payload["status"],
        "interrupted"
    );
    assert_eq!(
        events[position(&kinds, "triage_finished")].payload["action"],
        "retry"
    );
    // Its workspace is one cmux does not list: nothing to close.
    assert!(!kinds.contains(&"workspace_closed"));
    assert!(!kinds.contains(&"cleanup_failed"));
    let (prompt, _) = &reviewer.triage_prompts()[0];
    assert!(prompt.contains("which ended interrupted"), "{prompt}");
    assert!(prompt.contains("raised the alert interrupted"), "{prompt}");
    assert_eq!(
        events[position(&kinds, "recovery_requested")].payload["alert"],
        "interrupted"
    );
    assert_eq!(
        events[position(&kinds, "auto_repaired")].payload["repair"],
        "retry"
    );
}

/// A run whose supervisor and wrapper both died keeps the dead supervisor's
/// stale lease. Nobody adopts it (its wrapper is dead), so the next
/// supervisor recovers it itself as it does a run nobody leases (task 236),
/// triages it as `interrupted`, and `retry` runs the task again to landing.
#[test]
fn a_dead_run_whose_dead_supervisor_still_leases_it_is_recovered_and_retried() {
    let (_dir, repo, db) = fixture();
    let orphan = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET pid=?1, heartbeat_at=unixepoch()-31",
            [dead_pid()],
        )
        .unwrap();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")])
        .with_triages(&[repair(json!({"action": "retry"}), "the machine restarted")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2);
    assert_eq!(detail.runs[0].id(), orphan.id());
    assert_eq!(detail.runs[0].status(), RunStatus::Interrupted);
    assert_landed_run(&detail.runs[1], &repo, &base);
    let events = queue.run_events(orphan.id()).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(!kinds.contains(&"run_adopted"), "{kinds:?}");
    let recovered = &events[position(&kinds, "run_recovered")].payload;
    assert_eq!(recovered["by"], "supervisor");
    assert_eq!(recovered["previous_status"], "running");
    assert_eq!(recovered["lease_deleted"], true);
    assert_eq!(recovered["run"]["lease"]["alive"], false);
    assert_eq!(
        events[position(&kinds, "triage_finished")].payload["action"],
        "retry"
    );
}

/// Commit a change in the worktree of `run` (a `running` orphan), write its
/// receipt and move it to `awaiting_integration` the way validation does,
/// leaving its lease and wrapper registration as they are: a run whose
/// supervisor died during its review.
fn validated_orphan(db: &Path, run: &TaskRun) -> String {
    let worktree = Path::new(run.worktree_path().unwrap());
    fs::write(
        worktree.join("change.txt"),
        format!("change by {}\n", run.id()),
    )
    .unwrap();
    git(worktree, &["add", "change.txt"]);
    git(worktree, &["commit", "-q", "-m", "work"]);
    let head = git_out(worktree, &["rev-parse", "HEAD"]);
    write_receipt(run, &head, "succeeded", "done");
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), head],
        )
        .unwrap();
    SqliteQueue::open(db)
        .unwrap()
        .record_runtime_event(
            run.id(),
            "validation_finished",
            json!({"status": "awaiting_integration"}),
        )
        .unwrap();
    head
}

/// Make the wrapper of `run` one that died without recording its exit (a
/// dead pid and a heartbeat past the TTL) and the lease one a dead
/// supervisor left behind.
fn kill_supervisor_and_wrapper(db: &Path, run: &TaskRun) {
    let raw = Connection::open(db).unwrap();
    raw.execute(
        "UPDATE run_processes SET pid=?2, heartbeat_at=unixepoch()-31 WHERE run_id=?1",
        rusqlite::params![run.id(), dead_pid()],
    )
    .unwrap();
    raw.execute(
        "UPDATE run_leases SET pid=?2, heartbeat_at=unixepoch()-31 WHERE run_id=?1",
        rusqlite::params![run.id(), dead_pid()],
    )
    .unwrap();
}

/// The supervisor and the session's wrapper both died while an
/// `awaiting_integration` run was under review (task 236): the next
/// supervisor adopts it all the same, since its review needs no session,
/// reviews it with the session taken for ended, sends no `/exit` and lands
/// it, with nobody touching the queue.
#[test]
fn an_awaiting_run_whose_supervisor_and_wrapper_died_is_adopted_reviewed_and_landed() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let run = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    validated_orphan(&db, &run);
    kill_supervisor_and_wrapper(&db, &run);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1);
    assert_landed_run(&detail.runs[0], &repo, &base);
    let adopted = adoption_events(&detail);
    assert_eq!(adopted.len(), 1, "{adopted:?}");
    assert_eq!(adopted[0]["previous_token"], "dead-supervisor");
    assert_eq!(adopted[0]["wrapper"]["alive"], false);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"run_recovered"), "{kinds:?}");
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    assert_eq!(
        payloads(&detail, "review_started")[0]["session_live"],
        false
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    assert!(queue.run_leases().unwrap().is_empty());
}

/// A `revise` verdict for such a run has no session to revise it: the run
/// is asked about (`approve_landing`) instead of being given up.
#[test]
fn a_revise_for_an_adopted_run_whose_wrapper_died_asks_a_person() {
    let (_dir, repo, db) = fixture();
    let run = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    validated_orphan(&db, &run);
    kill_supervisor_and_wrapper(&db, &run);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("revise", &["add a test"], "one gap")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::ApproveLanding);
    assert!(
        asks[0]
            .question
            .contains("the session had ended, so nobody could revise the run"),
        "{}",
        asks[0].question
    );
    assert!(backend.texts().is_empty());
    assert!(queue.run_leases().unwrap().is_empty());
}

/// Make the live wrapper of `run_id` go silent the way a wrapper whose
/// heartbeat stopped does while its process lives on: its row names another
/// live process (`stand_in`, so the in-test wrapper's heartbeats no longer
/// match and fail), and once any heartbeat in flight has landed its
/// heartbeat is made older than the timeout. Returns the wrapper's own pid.
fn silence_wrapper(db: &Path, run_id: &RunId, stand_in: u32) -> u32 {
    let raw = Connection::open(db).unwrap();
    let pid: u32 = raw
        .query_row(
            "SELECT pid FROM run_processes WHERE run_id=?1 AND role='wrapper' AND exited_at IS NULL",
            [run_id],
            |r| r.get(0),
        )
        .unwrap();
    raw.execute(
        "UPDATE run_processes SET pid=?2 WHERE run_id=?1 AND role='wrapper' AND exited_at IS NULL",
        rusqlite::params![run_id, stand_in],
    )
    .unwrap();
    thread::sleep(TEST_TICK * 4);
    raw.execute(
        "UPDATE run_processes SET heartbeat_at=unixepoch()-31 WHERE run_id=?1 AND role='wrapper' AND exited_at IS NULL",
        [run_id],
    )
    .unwrap();
    pid
}

/// Give the silenced wrapper its own pid back, so it heartbeats and records
/// its exit again.
fn revive_wrapper(db: &Path, run_id: &RunId, pid: u32) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE run_processes SET pid=?2 WHERE run_id=?1 AND role='wrapper' AND exited_at IS NULL",
            rusqlite::params![run_id, pid],
        )
        .unwrap();
}

/// A live process to stand in for a silent wrapper; killed on drop.
struct StandIn(std::process::Child);

impl StandIn {
    fn new() -> Self {
        Self(
            Command::new("sleep")
                .arg("600")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The worker's wrapper stops heartbeating while its process lives on (task
/// 170): the supervisor records `wrapper_heartbeat_expired`, sends the
/// session the single `/exit` a finished one gets, and when it does not
/// exit within the exit timeout opens a `stuck_exit` ask to the inbox that
/// says why, keeping the run and its lease. Once the session exits the ask
/// is closed and the run goes on to validating as usual.
#[test]
fn a_silent_wrapper_with_a_live_session_is_asked_to_exit_then_raised_to_the_inbox() {
    let (_dir, repo, db) = fixture();
    // A receipt but no idle marker: nothing else would ask it to exit.
    let mut backend = TestWorkspace::new(
        &db,
        false,
        &format!("commit work; receipt \"$(git rev-parse HEAD)\"; await_exit; {HOLD}"),
    );
    backend.exit_timeout = Duration::from_secs(1);
    let backend = Arc::new(backend);
    SqliteQueue::open(&db)
        .unwrap()
        .register_session_workspace(SessionRole::Inbox, "inbox-ws")
        .unwrap();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"receipt_observed")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let stand_in = StandIn::new();
    let own = silence_wrapper(&db, run.id(), stand_in.pid());
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Running);
    assert!(run.last_error().is_none());
    assert!(queue.run_lease(run.id()).unwrap().is_some());
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("wrapper_heartbeat_expired") < position("exit_requested"));
    assert!(position("exit_requested") < position("exit_request_timed_out"));
    let expired = payloads(&detail, "wrapper_heartbeat_expired");
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0]["pid"], stand_in.pid());
    assert_eq!(expired[0]["workspace_id"], WORKSPACE_ID);
    assert!(expired[0]["heartbeat_age_secs"].as_i64().unwrap() > 30);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::StuckExit);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.options, ["exit", "wait"]);
    assert!(
        ask.question.contains(
            "Its wrapper stopped heartbeating while its process lived on (wrapper_heartbeat_expired), so the supervisor sent the /exit. The run stays running, and goes on to validating once the session exits"
        ),
        "{}",
        ask.question
    );
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);

    revive_wrapper(&db, run.id(), own);
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("exit_request_timed_out") < position("session_exited"));
    assert!(position("session_exited") < position("validation_finished"));
    assert_eq!(payloads(&detail, "wrapper_heartbeat_expired").len(), 1);
    let closed = queue.read_ask(ask.id).unwrap();
    assert_eq!(
        closed.answer.as_deref(),
        Some("the session exited; closed by the runtime")
    );
}

/// A wrapper whose heartbeat expired and whose process is gone is handled
/// as before: nothing is asked to exit, and the run is given up with the
/// heartbeat error.
#[test]
fn a_silent_wrapper_whose_process_is_gone_is_given_up_as_before() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        &format!("commit work; receipt \"$(git rev-parse HEAD)\"; {HOLD}"),
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
    let own = silence_wrapper(&db, run.id(), dead_pid());
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"runtime_error")
    });
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"wrapper_heartbeat_expired"), "{kinds:?}");
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    assert_eq!(
        payloads(&detail, "runtime_error")[0]["message"],
        "wrapper heartbeat expired; session may still be alive"
    );
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());
    // Let the in-test wrapper finish so the supervisor's pass can end.
    revive_wrapper(&db, run.id(), own);
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert!(
        outcome["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["message"] == "wrapper heartbeat expired; session may still be alive"),
        "{outcome}"
    );
}

/// A resumed session whose wrapper goes silent is sent the `/exit` too, and
/// is let go with a `stuck_exit` ask when it does not exit, as a resumed
/// session that ignores `/exit` is.
#[test]
fn a_resumed_session_with_a_silent_wrapper_is_asked_to_exit_then_let_go() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.exit_timeout = Duration::from_secs(1);
    let backend = Arc::new(backend);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(2, &format!("await_message; {HOLD}"));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |_| {
        resume_message_path(run.run_dir().unwrap()).exists()
    });
    let stand_in = StandIn::new();
    let own = silence_wrapper(&db, run.id(), stand_in.pid());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert_eq!(payloads(&detail, "wrapper_heartbeat_expired").len(), 1);
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1, "{kinds:?}");
    assert_eq!(finished[0]["outcome"], "unresolved");
    assert_eq!(finished[0]["exit_timed_out"], true);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let asks = other_asks(&mut queue, false);
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::StuckExit);
    assert!(
        asks[0].question.contains(
            "(wrapper_heartbeat_expired), so the supervisor sent the /exit. The run stays needs_session, and the supervisor resumes it again once the session exits"
        ),
        "{}",
        asks[0].question
    );
    revive_wrapper(&db, run.id(), own);
    release_held_session(run.run_dir().unwrap());
    backend.join();
}

/// A session kept open through its review whose wrapper goes silent while
/// the supervisor waits for its `/exit` is not given up either: the silence
/// is recorded, the `stuck_exit` ask does not blame the silence for an
/// `/exit` sent before it, and the run moves on as its verdict said once the
/// session exits.
#[test]
fn a_silent_wrapper_after_the_review_waits_for_the_exit_with_an_ask() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, HELD_AGENT);
    backend.exit_timeout = Duration::from_secs(3);
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"exit_requested")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let stand_in = StandIn::new();
    let own = silence_wrapper(&db, run.id(), stand_in.pid());
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert_eq!(payloads(&detail, "wrapper_heartbeat_expired").len(), 1);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let ask = queue.asks(AskQuery::default()).unwrap()[0].clone();
    assert!(
        ask.question
            .contains("exit back. The run stays awaiting_integration under the supervisor"),
        "{}",
        ask.question
    );
    assert!(
        !ask.question.contains("wrapper_heartbeat_expired"),
        "{}",
        ask.question
    );
    revive_wrapper(&db, run.id(), own);
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(kinds.contains(&"review_failed"), "{kinds:?}");
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
}

/// A silent wrapper that dies without recording its exit after the `/exit`
/// leaves no session to exit: the run is given up with the heartbeat error
/// as before, and the `stuck_exit` ask the silence raised is closed.
#[test]
fn a_silent_wrapper_that_dies_after_the_exit_closes_its_ask() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        &format!("commit work; receipt \"$(git rev-parse HEAD)\"; await_exit; {HOLD}"),
    );
    backend.exit_timeout = Duration::from_secs(1);
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"receipt_observed")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let stand_in = StandIn::new();
    let own = silence_wrapper(&db, run.id(), stand_in.pid());
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    let ask = queue.asks(AskQuery::default()).unwrap()[0].clone();
    drop(stand_in);
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"runtime_error")
    });
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(
        payloads(&detail, "runtime_error")[0]["message"],
        "wrapper heartbeat expired; session may still be alive"
    );
    let closed = queue.read_ask(ask.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(
        closed.answer.as_deref(),
        Some("the session exited; closed by the runtime")
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    revive_wrapper(&db, run.id(), own);
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert!(
        outcome["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["message"] == "wrapper heartbeat expired; session may still be alive"),
        "{outcome}"
    );
}

/// Without a supervisor, `recover` takes the stale lease of an
/// `awaiting_integration` run whose supervisor and wrapper died (task 236):
/// the run stays `awaiting_integration` and `integrate` lands it. A run
/// awaiting integration that nobody leases has nothing to recover, and a
/// live supervisor's lease is refused as before.
#[test]
fn recover_releases_the_stale_lease_of_an_awaiting_run_for_integrate() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let run = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    validated_orphan(&db, &run);
    let refused = runtime::recover(&db, run.id()).unwrap_err().to_string();
    assert!(refused.contains("lease heartbeat is"), "{refused}");
    kill_supervisor_and_wrapper(&db, &run);
    let error = integrate(&db, 1, &repo).unwrap_err().to_string();
    assert!(error.contains("run is still leased"), "{error}");
    let recovered = runtime::recover(&db, run.id()).unwrap();
    assert_eq!(recovered["run"]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let recovered = payloads(&queue.show(TaskId::new(1)).unwrap(), "run_recovered")[0].clone();
    assert_eq!(recovered["previous_status"], "awaiting_integration");
    assert_eq!(recovered["status"], "awaiting_integration");
    assert_eq!(recovered["lease_deleted"], true);
    let again = runtime::recover(&db, run.id()).unwrap_err().to_string();
    assert!(
        again.contains("only unfinished runs, or a run awaiting integration that is still leased"),
        "{again}"
    );
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed_run(&detail.runs[0], &repo, &base);
}
