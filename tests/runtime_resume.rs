//! Runtime tests: Runs that need a session: conflicts and the resumed sessions.
mod common;
mod runtime_support;

use runtime_support::*;

/// Both runs change the same file: the second cannot be rebased by the
/// runtime and waits for a session, which resolves, reruns verification and
/// rewrites the receipt; then it lands like any other run.
#[test]
fn conflicting_run_needs_a_session_and_lands_after_the_session_resolves_it() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let first_landed = git_out(&repo, &["rev-parse", "main"]);

    let run = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    let source = run.result_commit().cloned().unwrap();
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    assert_eq!(outcome["main"], json!(first_landed));
    let reason = outcome["reason"].as_str().unwrap();
    assert!(reason.contains("conflicted in change.txt"), "{reason}");
    assert!(
        reason.contains(&format!("git rebase {first_landed}")),
        "{reason}"
    );
    let parked = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(parked.status(), RunStatus::NeedsSession);
    assert_eq!(parked.last_error(), Some(reason));
    assert_eq!(
        parked.result_commit().map(CommitSha::as_str),
        Some(source.as_str())
    );
    // A parked run is reviewable against its own base.
    let review = runtime::review(&db, TaskId::new(2)).unwrap();
    assert_eq!(review["head"], json!(source));
    assert_eq!(review["base"], json!(seed));
    // The rebase was aborted: the worktree is back on its validated head, clean.
    assert_eq!(git_out(&worktree, &["rev-parse", "HEAD"]), source);
    assert_eq!(git_out(&worktree, &["status", "--porcelain"]), "");
    assert!(!worktree.join(".git").join("rebase-merge").exists());
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);
    let detail = queue.show(TaskId::new(2)).unwrap();
    let deferred = detail
        .events
        .iter()
        .find(|e| e.kind == "integration_deferred")
        .unwrap();
    assert_eq!(deferred.payload["status"], "needs_session");
    assert_eq!(deferred.payload["code"], "rebase_conflict");
    assert_eq!(deferred.payload["conflicts"], json!(["change.txt"]));
    assert_eq!(deferred.payload["aborted"], true);
    assert!(
        deferred.payload["output_tail"]
            .as_str()
            .unwrap()
            .contains("CONFLICT")
    );
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert!(queue.run_leases().unwrap().is_empty());
    // A parked run still owns its task and is not picked by --next.
    assert!(
        queue
            .transition(TaskId::new(2), TaskAction::BypassReview)
            .is_err()
    );
    assert_eq!(integrate_next(&db, &repo)["outcome"], "no_run_awaiting");
    assert!(queue.candidates().unwrap().is_empty());
    assert_eq!(runtime::doctor(&db, true).unwrap()["runs"], json!([]));

    // Nothing changed in the worktree: the runtime tries again and parks it again.
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");

    // The session resolves the conflict on top of main.
    let rebase = Command::new("git")
        .arg("-C")
        .arg(&worktree)
        .args(["rebase", &first_landed])
        .bounded_output()
        .unwrap();
    assert!(!rebase.status.success());
    fs::write(worktree.join("change.txt"), "resolved by the session\n").unwrap();
    git(&worktree, &["add", "change.txt"]);
    let status = Command::new("git")
        .arg("-C")
        .arg(&worktree)
        .env("GIT_EDITOR", "true")
        .args(["rebase", "--continue"])
        .bounded_status()
        .unwrap();
    assert!(status.success());
    let resolved = git_out(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(resolved, source);

    // Until the receipt names the new head, the session is not done.
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let reason = outcome["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("receipt commit {source} is not the head")),
        "{reason}"
    );
    assert_eq!(git_out(&worktree, &["rev-parse", "HEAD"]), resolved); // Left as the session made it.
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);

    // Every receipt that passed its checks was recorded, even when the
    // landing then stopped: the stale one names the validated head.
    let detail = queue.show(TaskId::new(2)).unwrap();
    let recorded = integration_receipts(&detail);
    assert_eq!(recorded.len(), 3, "{recorded:?}");
    assert!(recorded.iter().all(|p| p["commit"] == json!(source)));
    assert!(recorded.iter().all(|p| p["main"] == json!(first_landed)));

    let mut rewritten = session_receipt(&parked, &resolved, "succeeded", "resolved");
    rewritten["tests"]["evidence_or_reason"] = json!("cargo test after the rebase: 12 passed");
    rewritten["follow_ups"] = json!([
        {"title": "dedupe change.txt", "description": "both tasks wrote it"}
    ]);
    write_receipt_json(&parked, rewritten.clone());
    // The session's head sits on the landed main, so the review is taken
    // against that main and leaves out the first task's landing.
    let review = runtime::review(&db, TaskId::new(2)).unwrap();
    assert_eq!(review["base"], json!(first_landed));
    assert_eq!(review["head"], json!(resolved));
    assert_eq!(review["files_changed"], json!(1));
    let text = fs::read_to_string(review["path"].as_str().unwrap()).unwrap();
    assert!(text.contains("+resolved by the session"), "{text}");
    let commits = &text[text.find("## Commits").unwrap()..text.find("## Diffstat").unwrap()];
    let listed = &commits[commits.find("```").unwrap()..];
    assert_eq!(listed.matches(" work\n").count(), 1, "{commits}");
    assert!(!listed.contains(&first_landed[..7]), "{commits}");
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    // The session rebased the branch itself, so this rebase is a no-op; the
    // verification commands run here all the same, as on every landing.
    assert_eq!(outcome["verification_skipped"], json!(false), "{outcome}");
    let landed = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_landed(&repo, &landed, "second", &first_landed);
    // The receipt the session rewrote is what the DB keeps for the landing,
    // while validation_finished still holds the one from before the conflict.
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert!(!event_kinds(&detail).contains(&"integration_verification_skipped"));
    let rebased = detail
        .events
        .iter()
        .rev()
        .find(|e| e.kind == "integration_rebased")
        .unwrap();
    assert_eq!(rebased.payload["head_before"], json!(resolved));
    assert_eq!(rebased.payload["head_after"], json!(resolved));
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(verifications[0]["exit_code"], 0);
    let recorded = integration_receipts(&detail);
    assert_eq!(recorded.len(), 4, "{recorded:?}");
    let last = recorded[3];
    assert_eq!(last["commit"], json!(resolved));
    assert_eq!(last["main"], json!(first_landed));
    assert_eq!(last["receipt"], rewritten);
    assert_eq!(last["receipt"]["commit"], json!(resolved));
    assert_eq!(
        last["receipt"]["tests"]["evidence_or_reason"],
        json!("cargo test after the rebase: 12 passed")
    );
    assert_eq!(
        last["receipt"]["follow_ups"],
        json!([{"title": "dedupe change.txt", "description": "both tasks wrote it"}])
    );
    let validated = detail
        .events
        .iter()
        .find(|e| e.kind == "validation_finished")
        .unwrap();
    assert_eq!(validated.payload["receipt"]["commit"], json!(source));
    assert!(validated.payload["receipt"].get("follow_ups").is_none());
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", run.id())]
        ),
        resolved
    );
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "resolved by the session\n"
    );
    assert_eq!(
        git_out(&repo, &["rev-list", "--count", &format!("{seed}..main")]),
        "2"
    );
    assert_eq!(
        git_out(&repo, &["log", "-1", "--format=%b", "main"])
            .lines()
            .next(),
        Some("resolved")
    );
    assert_eq!(
        queue.show(TaskId::new(2)).unwrap().task.status(),
        TaskStatus::Completed
    );
    // The parked attempts were before the landing; the reason is cleared.
    assert!(landed.last_error().is_none());
    let detail = queue.show(TaskId::new(2)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "integration_deferred")
            .count(),
        3
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "integration_started")
            .count(),
        4
    );
    assert_eq!(kinds.iter().filter(|k| **k == "run_integrated").count(), 1);
}

/// The supervisor resumes a `needs_session` run whose `integrate` was
/// called (ADR-0019 decision 1): it opens a workspace named like the worker's
/// with the worker's wrapper, types the resolution request, sends `/exit`
/// once the session rewrote its receipt for the worktree head and went
/// idle, closes the workspace and lands the run. The first attempt only
/// rewrites the receipt, so the landing conflicts again and the run comes
/// back for a second attempt, which resolves it.
#[test]
fn approved_needs_session_run_is_resumed_until_the_runtime_lands_it() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let reason = run.last_error().unwrap().to_owned();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(
        payloads(&detail, "integration_approved"),
        [&json!({"status": "awaiting_integration", "pid": std::process::id(), "push": true})]
    );
    // The inbox is told the runtime takes it from here.
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        run_attention_of(&status, run.id()).unwrap()["next"],
        "resuming (runtime)"
    );

    backend.resume_script_for(
        2,
        "await_message; mark=\"$(dirname \"$RECEIPT\")/attempted\"; if [ -f \"$mark\" ]; then resolve; else : > \"$mark\"; fi; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = queue.show(TaskId::new(2)).unwrap();
    let landed = detail.runs[0].clone();
    assert_landed(&repo, &landed, "second", &first_landed);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "resolved by the resumed session\n"
    );
    assert!(queue.run_leases().unwrap().is_empty());
    let started = payloads(&detail, "resume_started");
    assert_eq!(started.len(), 2, "{:?}", event_kinds(&detail));
    assert_eq!(
        started[0],
        &json!({"attempt": 1, "counted": false, "reason": reason, "main": first_landed})
    );
    // Each conflict-only resume left out of the count is a repair.
    let uncounted: Vec<&Value> = payloads(&detail, "auto_repaired")
        .into_iter()
        .filter(|p| p["repair"] == "conflict_resume_uncounted")
        .collect();
    assert_eq!(uncounted.len(), 2, "{:?}", event_kinds(&detail));
    assert_eq!(uncounted[0]["conditions"]["conflict_only_resumes"], 1);
    assert_eq!(uncounted[1]["conditions"]["conflict_only_resumes"], 2);
    assert_eq!(uncounted[0]["detail"]["attempt"], 1);
    assert_eq!(started[1]["attempt"], 2);
    assert!(
        started[1]["reason"]
            .as_str()
            .unwrap()
            .contains("conflicted in change.txt")
    );
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 2);
    for (index, payload) in finished.iter().enumerate() {
        assert_eq!(payload["attempt"], index + 1);
        assert_eq!(payload["outcome"], "resolved");
        assert_eq!(payload["status"], "needs_session");
        assert_eq!(payload["approved"], true);
        assert_eq!(payload["workspace_closed"], true);
    }
    assert_eq!(finished[0]["head"], json!(run.result_commit()));
    assert_eq!(
        finished[1]["head"],
        json!(git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", run.id())]
        ))
    );
    // Each resume ends before its landing starts; one landing conflicted.
    let kinds = event_kinds(&detail);
    let resumed = kinds
        .iter()
        .skip_while(|k| **k != "resume_started")
        .filter(|k| {
            matches!(
                **k,
                "resume_started"
                    | "resume_finished"
                    | "integration_started"
                    | "integration_deferred"
                    | "run_integrated"
            )
        })
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(
        resumed,
        [
            "resume_started",
            "resume_finished",
            "integration_started",
            "integration_deferred",
            "resume_started",
            "resume_finished",
            "integration_started",
            "run_integrated",
        ]
    );
    // Same wrapper and runtime snapshot as the worker, under the resume name.
    let resumes = backend.resumes.lock().unwrap().clone();
    assert_eq!(resumes.len(), 2);
    let run_dir = Path::new(run.run_dir().unwrap());
    for (name, command) in &resumes {
        assert_eq!(name, "[repo's directory]worker#2 - second");
        assert!(
            command.contains(&shell_join(&[
                "session".into(),
                "--run".into(),
                run.id().to_string()
            ])),
            "{command}"
        );
        assert!(
            command.starts_with(&shell_join(&[run_dir
                .join("runner")
                .to_string_lossy()
                .into_owned()])),
            "{command}"
        );
    }
    let texts = backend.texts();
    assert_eq!(texts.len(), 2);
    let text = &texts[0].1;
    for expected in [
        format!(
            "dagq: integrate could not land run {} (task 2) and returned needs_session.",
            run.id()
        ),
        format!("Reason: {reason}"),
        format!(
            "main is now {first_landed} (your base commit was {}).",
            run.base_commit()
        ),
        "Tasks landed on main since your base:\n- task 1: test task; summary: done".to_owned(),
        format!("git rebase {first_landed}"),
        "[\"test -f seed.txt\"]".to_owned(),
        format!(
            "Rewrite the receipt at {} with the new head commit",
            run.receipt_path().unwrap()
        ),
        "result failed".to_owned(),
        format!("4. {}", runtime::STOP_BACKGROUND),
        "Do not merge or push. When done, report briefly and stop; do not run /exit.".to_owned(),
    ] {
        assert!(text.contains(&expected), "{expected:?} not in {text}");
    }
    assert_eq!(
        &fs::read_to_string(run_dir.join("resume-1.txt")).unwrap(),
        text
    );
    assert!(run_dir.join("terminal-resume-2.txt").is_file());
    // /exit once per resumed session; both resume workspaces were closed.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 2);
    let closed = backend.closed();
    assert!(
        texts
            .iter()
            .all(|(workspace, _)| closed.contains(workspace))
    );
    // Nothing waits for a person: the watch sees no attention.
    assert_eq!(
        dagq::watch::events(&db, EventId::new(cursor), 100, false).unwrap()["events"],
        json!([])
    );
    assert!(run_attention_of(&runtime::status(&db).unwrap(), run.id()).is_none());
}

/// A `needs_session` run that no `integrate` approved (here standing in for
/// validation's `evidence_missing`) is resumed with the evidence request
/// and, resolved, keeps its resumed session open through validation and the
/// supervisor's review like the worker's (ADR-0027 decision 3). The
/// stand-in `claude` prints no verdict, so the review is retried, fails,
/// and the run waits in an `approve_landing` ask.
#[test]
fn unapproved_resumed_run_is_validated_and_reviewed_with_its_session_open() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let mut queue = SqliteQueue::open(&db).unwrap();
    Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM run_events WHERE run_id=?1 AND kind='integration_approved'",
            [&run.id()],
        )
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            "evidence_missing",
            json!({"status": "needs_session", "reason": "e2e has no evidence"}),
        )
        .unwrap();
    backend.resume_script_for(
        2,
        "await_message; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");

    let detail = queue.show(TaskId::new(2)).unwrap();
    let back = detail.runs[0].clone();
    assert_eq!(back.status(), RunStatus::AwaitingIntegration);
    assert_eq!(back.result_commit(), run.result_commit());
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["outcome"], "resolved");
    assert_eq!(finished[0]["status"], "validating");
    assert_eq!(finished[0]["approved"], false);
    assert_eq!(finished[0]["workspace_closed"], false);
    assert_eq!(finished[0]["session_live"], true);
    let resume_workspace = finished[0]["workspace_id"].as_str().unwrap().to_owned();
    let kinds = event_kinds(&detail);
    let after: Vec<&str> = kinds
        .iter()
        .skip_while(|k| **k != "resume_finished")
        .filter(|k| {
            matches!(
                **k,
                "validation_finished"
                    | "review_started"
                    | "review_retried"
                    | "exit_requested"
                    | "session_exited"
                    | "workspace_closed"
                    | "review_failed"
            )
        })
        .copied()
        .collect();
    assert_eq!(
        after,
        [
            "validation_finished",
            "review_started",
            "review_retried",
            "review_started",
            "exit_requested",
            "session_exited",
            "workspace_closed",
            "review_failed",
        ]
    );
    assert!(backend.closed().contains(&resume_workspace));
    assert_eq!(
        payloads(&detail, "review_started").last().unwrap()["workspace_id"],
        json!(resume_workspace)
    );
    assert!(
        !event_kinds(&detail).contains(&"integration_started") || {
            // Only the two landings by hand before the resume.
            payloads(&detail, "integration_started").len() == 1
        }
    );
    let text = &backend.texts()[0].1;
    assert!(
        text.contains("found required evidence missing from the receipt"),
        "{text}"
    );
    assert!(text.contains("Reason: e2e has no evidence"), "{text}");
    assert!(
        text.contains("Run the checks the reason names as missing"),
        "{text}"
    );
    assert!(!text.contains("git rebase"), "{text}");
    assert!(text.contains(runtime::STOP_BACKGROUND), "{text}");
    // The inbox is woken only by the ask of the failed review (task 328).
    let events = dagq::watch::events(&db, EventId::new(cursor), 100, false).unwrap();
    assert_eq!(events["events"].as_array().unwrap().len(), 1, "{events}");
    assert_eq!(events["events"][0]["kind"], "ask_opened", "{events}");
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    // Integrating it now is the approval.
    assert_eq!(
        integrate(&db, 2, &repo).unwrap()["outcome"],
        "needs_session"
    );
    assert_eq!(
        payloads(&queue.show(TaskId::new(2)).unwrap(), "integration_approved"),
        [&json!({"status": "awaiting_integration", "pid": std::process::id(), "push": true})]
    );
}

/// Stand in for an earlier resume the supervisor judged `unresolved`
/// (task 122): one `resume_started` / `resume_finished` pair under another
/// token after the run was parked.
fn unresolved_attempt(db: &Path, run: &TaskRun, main: &str) {
    let mut queue = SqliteQueue::open(db).unwrap();
    let (_, attempt) = queue
        .begin_resume(run.id(), "earlier", &sha(main), None)
        .unwrap()
        .unwrap();
    assert_eq!(attempt, 1);
    queue
        .finish_resume(
            run.id(),
            "earlier",
            None,
            None,
            false,
            json!({"attempt": 1, "outcome": "unresolved", "exhausted": false}),
        )
        .unwrap();
}

/// Resolve the parked conflict in the run's worktree on top of `main` as
/// that session did, and return the new head.
fn resolve_in_worktree(run: &TaskRun, main: &str) -> String {
    let worktree = Path::new(run.worktree_path().unwrap());
    let rebase = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rebase", main])
        .bounded_output()
        .unwrap();
    assert!(!rebase.status.success());
    fs::write(worktree.join("change.txt"), "resolved by the session\n").unwrap();
    git(worktree, &["add", "change.txt"]);
    let status = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .env("GIT_EDITOR", "true")
        .args(["rebase", "--continue"])
        .bounded_status()
        .unwrap();
    assert!(status.success());
    git_out(worktree, &["rev-parse", "HEAD"])
}

/// A parked run whose earlier resume already rebased it onto main and
/// rewrote the receipt for its clean head (judged `unresolved` all the
/// same): the supervisor opens no session and uses no attempt, records
/// `resume_skipped` and, its integrate approved, lands it. The backend has
/// no resume script, so a resume would have failed.
#[test]
fn an_approved_run_resolved_by_an_earlier_resume_lands_without_a_session() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    unresolved_attempt(&db, &run, &first_landed);
    let resolved = resolve_in_worktree(&run, &first_landed);
    write_receipt(&run, &resolved, "succeeded", "resolved");

    let mut queue = SqliteQueue::open(&db).unwrap();
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert!(backend.resumes.lock().unwrap().is_empty());
    assert!(backend.texts().is_empty());
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    assert_eq!(
        payloads(&detail, "resume_skipped"),
        [
            &json!({"head": resolved, "main": first_landed, "approved": true, "status": "needs_session"})
        ]
    );
    let kinds = event_kinds(&detail);
    let after: Vec<&str> = kinds
        .iter()
        .skip_while(|k| **k != "resume_finished")
        .filter(|k| {
            matches!(
                **k,
                "resume_finished"
                    | "resume_skipped"
                    | "resume_started"
                    | "integration_started"
                    | "run_integrated"
            )
        })
        .copied()
        .collect();
    assert_eq!(
        after,
        [
            "resume_finished",
            "resume_skipped",
            "integration_started",
            "run_integrated"
        ]
    );
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(
        dagq::watch::events(&db, EventId::new(cursor), 100, false).unwrap()["events"],
        json!([])
    );
}

/// The same run without an approving `integrate` is validated and reviewed
/// without a session: the stand-in `claude` prints no verdict, so it waits
/// in `awaiting_integration` for a person's answer to the `approve_landing`
/// ask of its failed review.
#[test]
fn an_unapproved_run_resolved_by_an_earlier_resume_is_validated_without_a_session() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM run_events WHERE run_id=?1 AND kind='integration_approved'",
            [&run.id()],
        )
        .unwrap();
    unresolved_attempt(&db, &run, &first_landed);
    let resolved = resolve_in_worktree(&run, &first_landed);
    write_receipt(&run, &resolved, "succeeded", "resolved");

    let mut queue = SqliteQueue::open(&db).unwrap();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = queue.show(TaskId::new(2)).unwrap();
    let back = &detail.runs[0];
    assert_eq!(back.status(), RunStatus::AwaitingIntegration);
    assert_eq!(
        back.result_commit().map(CommitSha::as_str),
        Some(resolved.as_str())
    );
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);
    assert!(backend.resumes.lock().unwrap().is_empty());
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    assert_eq!(
        payloads(&detail, "resume_skipped"),
        [
            &json!({"head": resolved, "main": first_landed, "approved": false, "status": "validating"})
        ]
    );
    let kinds = event_kinds(&detail);
    let after: Vec<&str> = kinds
        .iter()
        .skip_while(|k| **k != "resume_skipped")
        .filter(|k| {
            matches!(
                **k,
                "resume_skipped"
                    | "validation_finished"
                    | "review_started"
                    | "review_retried"
                    | "review_failed"
            )
        })
        .copied()
        .collect();
    // The unreadable verdict is reviewed once more (task 328).
    assert_eq!(
        after,
        [
            "resume_skipped",
            "validation_finished",
            "review_started",
            "review_retried",
            "review_started",
            "review_failed"
        ]
    );
    assert_eq!(
        payloads(&detail, "review_started").last().unwrap()["workspace_id"],
        Value::Null
    );
    assert!(queue.run_leases().unwrap().is_empty());
}

/// A skipped run the landing parks again (its verification fails on the
/// resolved head) is resumed with a session next, not skipped again.
#[test]
fn a_run_parked_again_after_a_skip_is_resumed_not_skipped() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    unresolved_attempt(&db, &run, &first_landed);
    let resolved = resolve_in_worktree(&run, &first_landed);
    write_receipt(&run, &resolved, "succeeded", "resolved");
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands='[\"false\"]' WHERE id=2",
            [],
        )
        .unwrap();

    // The resumes after the second park fail (the backend has no script)
    // until the attempts are used up; none is skipped. Whether one pass of
    // `--once` makes both depends on whether it reads the run parked again
    // before it reaps the landing's slot, so passes are run until the last
    // attempt started (each pass makes at least one).
    let mut queue = SqliteQueue::open(&db).unwrap();
    let mut errors = Vec::new();
    for _ in 0..MAX_RESUME_ATTEMPTS {
        let outcome = supervise(&db, &repo, &backend).unwrap();
        errors.extend(outcome["errors"].as_array().unwrap().iter().cloned());
        let detail = queue.show(TaskId::new(2)).unwrap();
        if payloads(&detail, "resume_started")
            .last()
            .is_some_and(|p| p["attempt"] == MAX_RESUME_ATTEMPTS)
        {
            break;
        }
    }
    assert_eq!(errors.len(), 2, "{errors:?}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);
    assert_eq!(payloads(&detail, "resume_skipped").len(), 1);
    let kinds = event_kinds(&detail);
    let after: Vec<&str> = kinds
        .iter()
        .skip_while(|k| **k != "resume_skipped")
        .filter(|k| {
            matches!(
                **k,
                "resume_skipped" | "integration_deferred" | "resume_started" | "resume_finished"
            )
        })
        .copied()
        .collect();
    assert_eq!(
        after,
        [
            "resume_skipped",
            "integration_deferred",
            "resume_started",
            "resume_finished",
            "resume_started",
            "resume_finished"
        ]
    );
    assert_eq!(
        payloads(&detail, "resume_started").last().unwrap()["attempt"],
        3
    );
}

/// An unapproved run a supervisor moved on by `resume_skipped` and then
/// died holding (no session, no wrapper registration since) is adopted by
/// the next supervisor and validated and reviewed with no session.
#[test]
fn a_skipped_run_whose_supervisor_died_is_adopted() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM run_events WHERE run_id=?1 AND kind='integration_approved'",
            [&run.id()],
        )
        .unwrap();
    // The attempt clears the worker's process rows: no wrapper is left.
    unresolved_attempt(&db, &run, &first_landed);
    let resolved = resolve_in_worktree(&run, &first_landed);
    write_receipt(&run, &resolved, "succeeded", "resolved");
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.processes(run.id()).unwrap().is_empty());
    let skipped = queue
        .skip_resume(
            run.id(),
            "dead",
            &sha(&resolved),
            &sha(&first_landed),
            false,
        )
        .unwrap()
        .unwrap();
    assert_eq!(skipped.status(), RunStatus::Validating);
    // Taken already: a second skip or resume finds it leased.
    assert!(
        queue
            .skip_resume(
                run.id(),
                "other",
                &sha(&resolved),
                &sha(&first_landed),
                false
            )
            .unwrap()
            .is_none()
    );
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET pid=?2 WHERE run_id=?1",
            rusqlite::params![run.id(), dead_pid()],
        )
        .unwrap();

    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(backend.resumes.lock().unwrap().is_empty());
    let adopted = payloads(&detail, "run_adopted");
    assert_eq!(adopted.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(adopted[0]["wrapper"], Value::Null);
    assert_eq!(
        payloads(&detail, "review_started").last().unwrap()["workspace_id"],
        Value::Null
    );
    assert!(queue.run_leases().unwrap().is_empty());
}

/// Takes one condition of the skip away from a resolved run (the repo, the
/// queue, the run and its resolved head).
type Spoil = fn(&Path, &Path, &TaskRun, &str);

/// A parked run lacking any one condition of the skip is resumed as
/// before: the resume uses an attempt (and fails here, the backend having
/// no resume script) and no `resume_skipped` is recorded. `resumed` says
/// whether an unresolved resume came before. One test per condition, so
/// the conditions run in parallel (task 324: the eight in one test took
/// over a minute).
fn a_run_missing_a_condition_of_the_skip_is_resumed(case: &str, resumed: bool, spoil: Spoil) {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    if case == "a person sent it back" {
        SqliteQueue::open(&db)
            .unwrap()
            .record_runtime_event(
                run.id(),
                "landing_decided",
                json!({"status": "needs_session", "reason": "findings sent back"}),
            )
            .unwrap();
    }
    if resumed {
        unresolved_attempt(&db, &run, &first_landed);
    }
    let resolved = resolve_in_worktree(&run, &first_landed);
    write_receipt(&run, &resolved, "succeeded", "resolved");
    spoil(&repo, &db, &run, &resolved);

    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(
        outcome["errors"].as_array().unwrap().len(),
        1,
        "{case}: {outcome}"
    );
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert!(
        payloads(&detail, "resume_skipped").is_empty(),
        "{case}: {:?}",
        event_kinds(&detail)
    );
    let started = payloads(&detail, "resume_started");
    assert_eq!(started.len(), usize::from(resumed) + 1, "{case}");
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.last().unwrap()["outcome"], "error", "{case}");
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession, "{case}");
}

#[test]
fn a_run_not_resumed_since_it_was_parked_is_resumed() {
    a_run_missing_a_condition_of_the_skip_is_resumed(
        "no resume since it was parked",
        false,
        |_, _, _, _| {},
    );
}

#[test]
fn a_run_a_person_sent_back_is_resumed() {
    // Recorded before the unresolved attempt.
    a_run_missing_a_condition_of_the_skip_is_resumed(
        "a person sent it back",
        true,
        |_, _, _, _| {},
    );
}

#[test]
fn a_run_whose_receipt_names_the_old_head_is_resumed() {
    a_run_missing_a_condition_of_the_skip_is_resumed(
        "the receipt names the old head",
        true,
        |_, _, run, _| {
            write_receipt(
                run,
                run.result_commit().unwrap().as_str(),
                "succeeded",
                "stale",
            );
        },
    );
}

#[test]
fn a_run_with_a_dirty_worktree_is_resumed() {
    a_run_missing_a_condition_of_the_skip_is_resumed(
        "the worktree is dirty",
        true,
        |_, _, run, _| {
            let worktree = Path::new(run.worktree_path().unwrap());
            fs::write(worktree.join("stray.txt"), "left over\n").unwrap();
        },
    );
}

#[test]
fn a_run_main_moved_past_is_resumed() {
    a_run_missing_a_condition_of_the_skip_is_resumed(
        "main moved past the head",
        true,
        |repo, _, _, _| {
            git(repo, &["commit", "-q", "--allow-empty", "-m", "moved on"]);
        },
    );
}

#[test]
fn a_run_with_another_runs_receipt_is_resumed() {
    a_run_missing_a_condition_of_the_skip_is_resumed(
        "the receipt is another run's",
        true,
        |_, _, run, resolved| {
            let mut receipt = session_receipt(run, resolved, "succeeded", "resolved");
            receipt["run_id"] = json!("another-run");
            write_receipt_json(run, receipt);
        },
    );
}

#[test]
fn a_run_whose_receipt_reports_failed_is_resumed() {
    a_run_missing_a_condition_of_the_skip_is_resumed(
        "the receipt reports failed",
        true,
        |_, _, run, resolved| {
            write_receipt(run, resolved, "failed", "gave up");
        },
    );
}

#[test]
fn a_run_missing_the_required_evidence_is_resumed() {
    a_run_missing_a_condition_of_the_skip_is_resumed(
        "the required evidence is missing",
        true,
        |_, db, run, _| {
            Connection::open(db)
                .unwrap()
                .execute(
                    "UPDATE tasks SET required_evidence='[\"e2e\"]' WHERE id=?1",
                    [run.task_id()],
                )
                .unwrap();
        },
    );
}

/// ADR-0047 decision 24: a run whose landing was approved and that waits
/// only because its rebase conflicts with main is resumed without using up
/// one of the three attempts, up to the conflict-only limit. Past it, the
/// run is not handed to a person: it fails, its head is kept under
/// `refs/dagq/runs/<run-id>`, the task is ready again, and the next run's
/// prompt asks to bring that commit onto the current main.
#[test]
fn conflict_only_resumes_are_not_counted_and_a_used_up_run_is_retried_with_its_branch() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.resume_timeout = Duration::from_secs(1);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let source = run.result_commit().cloned().unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    // No session resolves it: each never goes idle and exits at the /exit
    // of the resume timeout.
    backend.resume_script_for(2, "await_exit");
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = queue.show(TaskId::new(2)).unwrap();
    let first = detail.runs[0].clone();
    assert_eq!(first.id(), run.id());
    let started: Vec<&Value> = detail
        .events
        .iter()
        .filter(|e| e.kind == "resume_started" && e.run_id.as_ref() == Some(run.id()))
        .map(|e| &e.payload)
        .collect();
    assert_eq!(
        started.len(),
        CONFLICT_ONLY_RESUME_LIMIT,
        "{:?}",
        event_kinds(&detail)
    );
    assert!(started.iter().all(|p| p["counted"] == false), "{started:?}");
    assert_eq!(
        started
            .iter()
            .map(|p| p["attempt"].clone())
            .collect::<Vec<_>>(),
        (1..=CONFLICT_ONLY_RESUME_LIMIT)
            .map(|n| json!(n))
            .collect::<Vec<_>>()
    );
    assert_eq!(first.status(), RunStatus::Failed);
    let last_error = first.last_error().unwrap();
    assert!(
        last_error.starts_with(
            "resumed 5 times (0 of at most 3 counted, and 5 of at most 5 for conflicts only after its review passed) and still needs a session: rebase onto main"
        ),
        "{last_error}"
    );
    // Retried by the runtime, with nobody asked.
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["by"], "runtime");
    assert_eq!(finished[0]["code"], "resume_exhausted");
    assert_eq!(finished[0]["action"], "retry_inherit");
    assert_eq!(finished[0]["counted_resumes"], 0);
    assert_eq!(finished[0]["conflict_only_resumes"], 5);
    assert_eq!(finished[0]["inherit"]["head"], json!(source));
    assert_eq!(
        finished[0]["inherit"]["branch"],
        json!(format!("dagq/{}", run.id()))
    );
    assert!(finished[0].get("ask_id").is_none());
    // Each conflict-only resume was left out of the count, then the
    // retry carried the branch over: all repairs (ADR-0047 decision 38).
    let repaired = payloads(&detail, "auto_repaired");
    let repairs: Vec<&str> = repaired
        .iter()
        .map(|p| p["repair"].as_str().unwrap())
        .collect();
    let mut expected = vec!["conflict_resume_uncounted"; CONFLICT_ONLY_RESUME_LIMIT];
    expected.push("inherit_retry");
    assert_eq!(repairs, expected, "{repaired:?}");
    let repaired = &repaired[CONFLICT_ONLY_RESUME_LIMIT..];
    assert_eq!(repaired[0]["layer"], "runtime");
    assert_eq!(repaired[0]["conditions"]["review"], "pass");
    assert_eq!(repaired[0]["conditions"]["parked"], "rebase_conflict");
    assert!(
        other_asks(&mut queue, true)
            .iter()
            .all(|ask| ask.kind != AskKind::Decide),
        "{:?}",
        other_asks(&mut queue, true)
    );
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", run.id())]
        ),
        source.as_str()
    );

    // The next run carries the work over.
    assert_eq!(detail.runs.len(), 2, "{:?}", event_kinds(&detail));
    let next = detail.runs[1].clone();
    let inherited: Vec<&Value> = detail
        .events
        .iter()
        .filter(|e| e.kind == "run_inherited")
        .map(|e| &e.payload)
        .collect();
    assert_eq!(inherited.len(), 1);
    assert_eq!(inherited[0]["inherit_from_run"], json!(run.id()));
    assert_eq!(inherited[0]["head"], json!(source));
    assert!(
        detail
            .events
            .iter()
            .any(|e| e.kind == "run_inherited" && e.run_id.as_ref() == Some(next.id()))
    );
    let prompt = fs::read_to_string(Path::new(next.run_dir().unwrap()).join("prompt.txt")).unwrap();
    // Its own commits start at the merge base of its head and the new
    // run's base (the landed main): here the base the first run was made on.
    let base = run.base_commit();
    assert!(
        prompt.contains(&format!(
            "Carried over from run {}: its review passed, but its landing kept conflicting with main until its resumes were used up, so this run starts from its work instead of from scratch. Its work is commit {source} (kept as refs/dagq/runs/{id}, branch dagq/{id}); its own commits are {base}..{source}. Bring them onto your base",
            run.id(),
            id = run.id()
        )),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("`git cherry-pick {base}..{source}`")),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!(
            "Its receipt ({}) summary: ",
            run.receipt_path().unwrap()
        )),
        "{prompt}"
    );
    assert_eq!(next.base_commit().as_str(), first_landed);
}

/// A resume that cannot start, or a session that cannot resolve the run,
/// uses up an attempt; after the third the supervisor stops resuming it and
/// hands it to a person (ADR-0024's Consequences): the run becomes `failed`
/// with a `decide` ask for the inbox (`retry` or `cancel`), recorded as the
/// runtime's `triage_finished` so no headless triage runs, and the answer is
/// applied like a triage's. The sessions behave like Claude: they
/// never exit by themselves, so the supervisor sends `/exit` once when one
/// goes idle without a resolving receipt, or when one never goes idle within
/// the resume timeout. The conflict that parked the run is made to count
/// (a conflict-only resume does not; see the test above).
#[test]
fn resuming_stops_after_three_attempts() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.resume_timeout = Duration::from_secs(1);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    count_resumes_of_parked(&db);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let reason = run.last_error().unwrap().to_owned();

    // No resume script: the workspace cannot be opened.
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"].as_array().unwrap().len(), 1, "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["outcome"], "error");
    assert_eq!(finished[0]["exhausted"], false);
    // The failed cmux call itself is recorded on the run (task 109).
    let failures = backend_failures(&detail);
    assert_eq!(failures.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(failures[0].run_id.as_ref(), Some(run.id()));
    assert_eq!(failures[0].payload["op"], "create_resume");
    assert!(
        finished[0]["error"]
            .as_str()
            .unwrap()
            .contains("no resume script")
    );
    assert_eq!(detail.runs[0].last_error(), Some(reason.as_str()));
    assert!(queue.run_leases().unwrap().is_empty());

    // The second attempt answers without resolving and goes idle; the third
    // never goes idle. Neither exits until it is asked to.
    backend.resume_script_for(
        2,
        "await_message; mark=\"$(dirname \"$RECEIPT\")/went-idle\"; if [ ! -f \"$mark\" ]; then : > \"$mark\"; idle; fi; await_exit",
    );
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert_eq!(
        detail.runs[0].last_error(),
        Some(format!("resumed 3 times (at most 3) and still needs a session: {reason}").as_str())
    );
    let started = payloads(&detail, "resume_started");
    assert_eq!(
        started
            .iter()
            .map(|p| p["attempt"].clone())
            .collect::<Vec<_>>(),
        [json!(1), json!(2), json!(3)]
    );
    assert!(started.iter().all(|p| p["reason"] == json!(reason)));
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 3);
    assert_eq!(finished[1]["outcome"], "unresolved");
    assert_eq!(finished[1]["exhausted"], false);
    assert_eq!(finished[2]["outcome"], "unresolved");
    assert_eq!(finished[2]["exhausted"], true);
    assert_eq!(finished[2]["status"], "needs_session");
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 2);
    assert_eq!(backend.closed().len(), 2 + 2); // two workers, two resumes
    // The used-up run goes to the inbox as the triage's `decide` ask, and
    // no headless triage runs for it.
    let asks = other_asks(&mut queue, false);
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::Decide);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.asked_by, "supervisor");
    assert_eq!(ask.options, ["retry", "cancel"]);
    assert!(
        ask.question
            .contains("was resumed 3 times (at most 3) and still needs a session"),
        "{}",
        ask.question
    );
    assert!(ask.question.contains(&reason), "{}", ask.question);
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["by"], "runtime");
    assert_eq!(finished[0]["action"], "ask");
    assert_eq!(finished[0]["ask_id"], json!(ask.id));
    assert_eq!(finished[0]["previous_status"], "needs_session");
    assert_eq!(finished[0]["status"], "failed");
    assert!(payloads(&detail, "triage_started").is_empty());
    // The ask is the one attention; the exhausted resume is none.
    let events = dagq::watch::events(&db, EventId::new(cursor), 100, false).unwrap();
    let listed = events["events"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{events}");
    assert_eq!(listed[0]["kind"], "ask_opened");
    assert_eq!(listed[0]["next"], format!("answer ask {}", ask.id));
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    // No fourth attempt, no second ask.
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert_eq!(
        payloads(&queue.show(TaskId::new(2)).unwrap(), "resume_started").len(),
        3
    );
    assert_eq!(other_asks(&mut queue, false).len(), 1);

    // The person cancels the task; the supervisor applies it.
    queue.answer(ask.id, "cancel").unwrap();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    assert_eq!(
        payloads(&detail, "triage_decided")[0]["answer"],
        json!("cancel")
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
}

/// A resumed session that does not exit within the exit timeout of `/exit`
/// is let go as `unresolved` (its lease released, its workspace kept), so the
/// supervisor's slot and a drain are not held forever. While it runs, the run
/// is the supervisor's (`resuming (runtime)`) and is not resumed again; once it
/// ended, the next pass closes the workspace it left and resumes the run.
/// Task 285: the resolution request waits for Claude Code's input box. A
/// booting session's screen gets nothing; past the registration timeout
/// the run records `input_not_ready` and asks the inbox once, and the
/// request goes as soon as the box is drawn. The ask closes when the
/// session exits.
#[test]
fn a_resumed_session_gets_its_request_only_once_its_input_box_is_ready() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.registration_timeout = Duration::from_secs(2);
    *backend.screen.lock().unwrap() = BOOT_SCREEN.into();
    let mut queue = SqliteQueue::open(&db).unwrap();
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = thread::scope(|scope| {
        scope.spawn(|| {
            let started = Instant::now();
            // Nothing is typed while the session boots.
            while started.elapsed() < Duration::from_secs(4) {
                assert!(backend.texts().is_empty());
                thread::sleep(Duration::from_millis(20));
            }
            *backend.screen.lock().unwrap() = READY_SCREEN.into();
        });
        supervise(&db, &repo, &backend).unwrap()
    });
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(backend.texts().len(), 1);
    let not_ready = payloads(&detail, "input_not_ready");
    assert_eq!(not_ready.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(not_ready[0]["waited_secs"], 2);
    assert_eq!(not_ready[0]["prompt"], Value::Null);
    assert!(
        not_ready[0]["excerpt"]
            .as_str()
            .unwrap()
            .contains("'session'")
    );
    let asks = other_asks(&mut queue, true);
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::AnswerPrompt);
    assert_eq!(asks[0].run_id.as_ref(), Some(run.id()));
    assert!(
        asks[0].question.contains("input box is not ready"),
        "{}",
        asks[0].question
    );
    assert_eq!(
        asks[0].answer.as_deref(),
        Some("the input box got ready and the request was sent; closed by the runtime")
    );
    assert!(payloads(&detail, "submit_retried").is_empty());
}

/// Task 285: a request whose Enter a long paste swallowed stays in the
/// input box; Enter alone goes again, the text is typed once, and the run
/// goes on as usual.
#[test]
fn a_request_left_in_the_input_box_gets_enter_again_not_the_text() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (_run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.swallowed_enters.store(2, Ordering::SeqCst);
    let mut queue = SqliteQueue::open(&db).unwrap();
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(backend.texts().len(), 1);
    assert_eq!(backend.enters.load(Ordering::SeqCst), 2);
    let retried = payloads(&detail, "submit_retried");
    assert_eq!(retried.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(retried[0]["what"], "resolution request");
    assert_eq!(retried[0]["input"], "text");
    assert_eq!(retried[0]["retries"], 2);
    assert_eq!(retried[0]["submitted"], true);
    assert!(payloads(&detail, "submit_unconfirmed").is_empty());
    // The Enters that got it through are one repair (ADR-0047 decision 38).
    let repaired: Vec<&Value> = payloads(&detail, "auto_repaired")
        .into_iter()
        .filter(|p| p["repair"] == "submit_enter_retry")
        .collect();
    assert_eq!(repaired.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(repaired[0]["layer"], "runtime");
    assert_eq!(
        repaired[0]["conditions"],
        json!({"input": "text", "retries": 2, "submitted": true})
    );
    assert_eq!(repaired[0]["detail"]["what"], "resolution request");
    assert!(other_asks(&mut queue, true).is_empty());
}

/// Task 285: a request still in the input box after the Enters sent again
/// is recorded and raised to the inbox as an `answer_prompt` ask; `/exit`
/// left there gets Enter again too but is never typed twice.
#[test]
fn a_request_stuck_in_the_input_box_is_asked_to_the_inbox() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    backend.swallowed_enters.store(1000, Ordering::SeqCst);
    let mut queue = SqliteQueue::open(&db).unwrap();
    // The fake session still reads the request, so the run resolves.
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    let unconfirmed = payloads(&detail, "submit_unconfirmed");
    assert_eq!(unconfirmed.len(), 2, "{:?}", event_kinds(&detail));
    assert_eq!(unconfirmed[0]["input"], "text");
    assert_eq!(unconfirmed[0]["retries"], 3);
    assert!(
        unconfirmed[0]["excerpt"]
            .as_str()
            .unwrap()
            .contains("do not run /exit")
    );
    assert_eq!(unconfirmed[1]["input"], "exit");
    // Three Enters after the request and three after /exit, sent once.
    assert_eq!(backend.enters.load(Ordering::SeqCst), 6);
    assert_eq!(backend.texts().len(), 1);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let asks = other_asks(&mut queue, true);
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::AnswerPrompt);
    assert_eq!(asks[0].run_id.as_ref(), Some(run.id()));
    assert!(
        asks[0].question.contains(
            "resolution request the supervisor typed stays in the input box after 4 Enters"
        ),
        "{}",
        asks[0].question
    );
    assert!(asks[0].closed_at.is_some());
}

/// Task 285: a request the session never got (typed into a box that lost
/// it) shows no sign of work within `start_wait`; with the input box empty
/// it is sent once more, and the run goes on without waiting out the
/// resume timeout.
#[test]
fn a_lost_request_is_sent_again_after_no_sign_of_work() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (_run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.start_wait = Duration::from_secs(1);
    backend.dropped_texts.store(1, Ordering::SeqCst);
    let mut queue = SqliteQueue::open(&db).unwrap();
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    let texts = backend.texts();
    assert_eq!(texts.len(), 2);
    assert_eq!(texts[0], texts[1]);
    let resent = payloads(&detail, "submit_resent");
    assert_eq!(resent.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(resent[0]["what"], "resolution request");
    assert_eq!(resent[0]["waited_secs"], 1);
    assert!(payloads(&detail, "submit_not_started").is_empty());
    assert!(other_asks(&mut queue, true).is_empty());
}

/// Task 285: a request lost twice is not sent a third time: the run
/// records `submit_not_started` and asks the inbox.
#[test]
fn a_request_lost_twice_is_asked_to_the_inbox() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    count_resumes_of_parked(&db);
    backend.start_wait = Duration::from_secs(1);
    backend.resume_timeout = Duration::from_secs(4);
    backend.dropped_texts.store(usize::MAX, Ordering::SeqCst);
    let mut queue = SqliteQueue::open(&db).unwrap();
    // It never gets the request, and exits at the /exit of the resume timeout.
    backend.resume_script_for(2, "await_exit");
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    // Each resume (three, all unresolved) sends it twice and asks once.
    let resumes = payloads(&detail, "resume_started").len();
    assert_eq!(resumes, 3, "{:?}", event_kinds(&detail));
    assert_eq!(backend.texts().len(), 2 * resumes);
    let not_started = payloads(&detail, "submit_not_started");
    assert_eq!(not_started.len(), resumes, "{:?}", event_kinds(&detail));
    assert!(not_started.iter().all(|p| p["resent"] == true));
    assert_eq!(payloads(&detail, "submit_resent").len(), resumes);
    let asks = queue
        .asks(AskQuery {
            all: true,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .filter(|a| a.kind == AskKind::AnswerPrompt)
        .collect::<Vec<_>>();
    assert_eq!(asks.len(), resumes, "{asks:?}");
    assert_eq!(asks[0].run_id.as_ref(), Some(run.id()));
    // Each closes once its session exited.
    assert!(asks.iter().all(|a| a.closed_at.is_some()));
    assert!(
        asks[0].question.contains(
            "showed no sign of work within 1s of the resolution request the supervisor sent twice"
        ),
        "{}",
        asks[0].question
    );
}

#[test]
fn a_resumed_session_that_ignores_exit_is_let_go() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.exit_timeout = Duration::from_secs(1);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let mut queue = SqliteQueue::open(&db).unwrap();
    backend.resume_script_for(2, &format!("await_message; idle; {HOLD}"));
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["outcome"], "unresolved");
    assert_eq!(finished[0]["exit_timed_out"], true);
    assert_eq!(finished[0]["workspace_closed"], false);
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let kept = finished[0]["workspace_id"].as_str().unwrap().to_owned();
    assert!(!backend.closed().contains(&kept));
    assert_eq!(
        run_attention_of(&runtime::status(&db).unwrap(), run.id()).unwrap()["next"],
        "resuming (runtime)"
    );
    // Its dialog does not go away by itself: one stuck_exit ask goes to the
    // inbox (task 147), as for the worker's session.
    let asks = other_asks(&mut queue, false);
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::StuckExit);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert!(ask.question.contains(&kept), "{}", ask.question);
    assert!(
        ask.question.contains(
            "The run stays needs_session, and the supervisor resumes it again once the session exits"
        ),
        "{}",
        ask.question
    );
    // A pass while the session still runs neither resumes the run nor asks
    // again, nor closes the ask.
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert_eq!(other_asks(&mut queue, false).len(), 1);
    assert!(queue.read_ask(ask.id).unwrap().is_open());

    // Its session ends; the workspace it left no longer blocks the run.
    release_held_session(run.run_dir().unwrap());
    backend.join();
    assert_eq!(
        run_attention_of(&runtime::status(&db).unwrap(), run.id()).unwrap()["next"],
        "resuming (runtime)"
    );
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(backend.closed().contains(&kept));
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(payloads(&detail, "resume_started").len(), 2);
    // The next pass closed the ask of the session that ended.
    let closed = queue.read_ask(ask.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(
        closed.answer.as_deref(),
        Some("the session exited; closed by the runtime")
    );
    assert!(other_asks(&mut queue, false).is_empty());
    // One ask about the session, besides the one of the run's failed
    // stand-in review before it was integrated by hand (task 328).
    let opened: Vec<&Value> = payloads(&detail, "ask_opened")
        .into_iter()
        .filter(|p| p["kind"] != "approve_landing")
        .collect();
    assert_eq!(opened.len(), 1, "{opened:?}");
}

/// The supervisor never resumes a run next to the live session of a
/// supervisor that died mid-resume: the run shows as `resuming (runtime)`,
/// keeps a person's `integrate` out, and once that session exited the next
/// supervisor resumes and lands the run.
#[test]
fn a_session_nobody_watches_blocks_the_resume_until_it_ends() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let mut queue = SqliteQueue::open(&db).unwrap();
    // A supervisor started a resume, its session registered, and then the
    // supervisor died: its lease goes stale while the session lives on.
    let (_, attempt) = queue
        .begin_resume(run.id(), "dead-supervisor", &sha(&first_landed), None)
        .unwrap()
        .unwrap();
    assert_eq!(attempt, 1);
    assert!(
        queue
            .begin_resume(run.id(), "another", &sha(&first_landed), None)
            .unwrap()
            .is_none()
    );
    queue
        .register_resume_wrapper(run.id(), "dead-supervisor", std::process::id())
        .unwrap();
    assert!(
        queue
            .register_resume_wrapper(run.id(), "dead-supervisor", std::process::id())
            .is_err()
    );
    age_lease(&db, &run, 60);
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        run_attention_of(&status, run.id()).unwrap()["next"],
        "resuming (runtime)"
    );
    let refused = integrate(&db, 2, &repo).unwrap_err();
    assert!(
        format!("{refused:#}").contains("run is still leased"),
        "{refused:#}"
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert_eq!(
        payloads(&queue.show(TaskId::new(2)).unwrap(), "resume_started").len(),
        1
    );

    // Its session ends; the next supervisor takes the stale lease over.
    queue
        .wrapper_exited(run.id(), std::process::id(), 0)
        .unwrap();
    assert_eq!(
        run_attention_of(&runtime::status(&db).unwrap(), run.id()).unwrap()["next"],
        "resuming (runtime)"
    );
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    let started = payloads(&detail, "resume_started");
    assert_eq!(
        started.len(),
        2,
        "{:?}",
        detail
            .events
            .iter()
            .map(|e| (&e.kind, &e.payload))
            .collect::<Vec<_>>()
    );
    assert_eq!(started[1]["attempt"], 2);
    let acquired = detail
        .events
        .iter()
        .filter(|e| e.kind == "lease_acquired" && e.payload["reason"] == "resume")
        .map(|e| e.payload["previous_token"].clone())
        .collect::<Vec<_>>();
    assert_eq!(acquired, [json!(null), json!("dead-supervisor")]);
}

/// A resumed session that finds the change no longer needed writes a failed
/// receipt; the run ends `failed` without landing.
#[test]
fn resumed_session_with_a_failed_receipt_fails_the_run() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        "await_message; receipt \"$(git rev-parse HEAD)\" failed 'already on main'; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "failed", "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert_eq!(
        detail.runs[0].last_error(),
        Some("session reported the run as failed: already on main")
    );
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished[0]["outcome"], "failed");
    assert_eq!(finished[0]["status"], "failed");
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);
    assert!(Path::new(run.worktree_path().unwrap()).exists());
    // The failed run goes to the triage; the stub `claude` prints no
    // verdict, so the triage fails and the run waits for a person.
    let failed = payloads(&detail, "triage_failed");
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("no verdict JSON"),
        "{}",
        failed[0]
    );
    assert_eq!(
        run_attention_of(&runtime::status(&db).unwrap(), run.id()).unwrap()["next"],
        "triage by hand"
    );
}

/// A session that finds the change no longer needed writes a failed receipt
/// with the reason; the run ends without touching main and the task can be
/// retried or canceled.
#[test]
fn failed_receipt_from_a_session_ends_the_run_without_landing() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let main = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(
        integrate(&db, 2, &repo).unwrap()["outcome"],
        "needs_session"
    );
    let run = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    write_receipt(
        &run,
        run.result_commit().map(CommitSha::as_str).unwrap(),
        "failed",
        "already covered by task 1",
    );
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "failed", "{outcome}");
    let reason = outcome["reason"].as_str().unwrap();
    assert!(reason.contains("already covered by task 1"), "{reason}");
    let failed = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(failed.status(), RunStatus::Failed);
    assert_eq!(failed.last_error(), Some(reason));
    assert!(Path::new(failed.worktree_path().unwrap()).exists());
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main);
    assert!(
        git_out(&repo, &["for-each-ref", "refs/dagq/runs/"])
            .lines()
            .count()
            == 1
    );
    assert_eq!(
        queue.show(TaskId::new(2)).unwrap().task.status(),
        TaskStatus::InProgress
    );
    assert!(queue.run_leases().unwrap().is_empty());
    let detail = queue.show(TaskId::new(2)).unwrap();
    let failed_event = detail
        .events
        .iter()
        .find(|e| e.kind == "integration_failed")
        .unwrap();
    assert_eq!(failed_event.payload["status"], "failed");
    assert_eq!(failed_event.payload["reason"], json!(reason));
    assert_eq!(
        failed_event.payload["receipt"],
        session_receipt(
            &run,
            run.result_commit().map(CommitSha::as_str).unwrap(),
            "failed",
            "already covered by task 1"
        )
    );
    // A failed receipt is not a receipt for a landing: only the first
    // (conflicting) attempt recorded one.
    assert_eq!(integration_receipts(&detail).len(), 1);
    // Retry or give up is a person's call, as after any failed run.
    queue
        .transition(TaskId::new(2), TaskAction::Cancel)
        .unwrap();
}

/// Task 466: `stats` ties the conflict that parked the second run to the
/// first run's landing, whose main it was rebased onto, and counts the
/// resumes by reason: the first resolved in its session but was deferred
/// again, the second landed the run. A time window narrows the runs.
#[test]
fn stats_ties_a_deferred_landing_to_the_landing_that_broke_it() {
    use dagq::domain::stats::{Cursor, StatsQuery};
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        "await_message; mark=\"$(dirname \"$RECEIPT\")/attempted\"; if [ -f \"$mark\" ]; then resolve; else : > \"$mark\"; fi; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let first = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();

    let query = |since: Option<Cursor>, until: Option<Cursor>| {
        runtime::stats(
            &db,
            &StatsQuery {
                since,
                until,
                full: true,
                ..StatsQuery::default()
            },
        )
        .unwrap()
    };
    let stats = query(None, None);
    let runs = stats["runs"].as_array().unwrap();
    let row = |id: &RunId| {
        runs.iter()
            .find(|row| row["run_id"] == id.as_str())
            .unwrap_or_else(|| panic!("no run {id} in {stats}"))
    };
    let (landing, broken) = (row(first.id()), row(run.id()));
    assert_eq!(landing["title"], "test task");
    assert_eq!(broken["title"], "second");
    assert_eq!(landing["broke_runs"], 1);
    assert_eq!(landing["integrate_attempts"], 1);
    assert_eq!(broken["broke_runs"], 0);
    // Deferred at the first landing and after the first resume, landed at the third.
    assert_eq!(broken["integrate_attempts"], 3);
    assert_eq!(broken["deferrals"], json!({"rebase_conflict": 2}));
    assert_eq!(broken["conflict_files"], json!(["change.txt"]));
    assert_eq!(
        broken["broken_by"],
        json!([{
            "task_id": 1,
            "run_id": first.id().as_str(),
            "landed_at": landing["landed_at"],
            "main": first_landed,
            "code": "rebase_conflict",
        }])
    );
    let attempts = broken["resume_attempts"].as_array().unwrap();
    assert_eq!(
        attempts
            .iter()
            .map(|a| (
                a["attempt"].clone(),
                a["reason"].clone(),
                a["resolved"].clone()
            ))
            .collect::<Vec<_>>(),
        [
            (json!(1), json!("rebase_conflict"), json!(false)),
            (json!(2), json!("rebase_conflict"), json!(true)),
        ]
    );
    assert!(attempts.iter().all(|a| a["secs"].is_i64()), "{attempts:?}");
    for row in [landing, broken] {
        for key in ["claimed_at", "validated_at", "landed_at"] {
            assert!(
                row[key].as_str().is_some_and(|at| at.ends_with('Z')),
                "{key} of {row}"
            );
        }
    }
    let outcomes = &stats["overall"]["resume_outcomes"];
    assert_eq!(outcomes["attempts"], 2);
    let conflict = &outcomes["by_reason"]["rebase_conflict"];
    assert_eq!(
        (
            &conflict["resolved"],
            &conflict["unresolved"],
            &conflict["resolved_percent"]
        ),
        (&json!(1), &json!(1), &json!(50))
    );
    assert_eq!(conflict["secs"]["count"], 2);

    // Up to the first landing's time only it finished; after it, only the second.
    let landed_at: Cursor = landing["landed_at"].as_str().unwrap().parse().unwrap();
    let ids = |stats: &Value| {
        stats["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["run_id"].clone())
            .collect::<Vec<_>>()
    };
    let until = query(None, Some(landed_at));
    assert_eq!(ids(&until), [json!(first.id().as_str())]);
    assert!(until["next_cursor"].as_i64() < stats["next_cursor"].as_i64());
    assert_eq!(
        ids(&query(Some(landed_at), None)),
        [json!(run.id().as_str())]
    );
}
