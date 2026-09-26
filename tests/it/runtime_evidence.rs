//! Runtime tests: Required evidence and declared paths.
use crate::runtime_support;

use runtime_support::*;

/// A fixture whose only ready task requires `evidence` in the receipt.
fn evidence_fixture(evidence: &[EvidenceCheck]) -> (Fixture, PathBuf, PathBuf) {
    let (dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let task = queue
        .add(NewTask {
            title: "needs evidence".into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: evidence.to_vec(),
            paths: Vec::new(),
            priority: Default::default(),
            kind: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    assert_eq!(task.id(), TaskId::new(2));
    assert_eq!(task.required_evidence(), evidence);
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    (dir, repo, db)
}

/// A receipt function for scripts: `receipt_e2e COMMIT STATUS EVIDENCE`
/// claims success with the given `e2e` check.
const RECEIPT_E2E: &str = r#"receipt_e2e() {
  printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"passed","evidence_or_reason":"ran"},"e2e":{"status":"%s","evidence_or_reason":"%s"},"subagent_review":{"status":"passed","evidence_or_reason":"reviewed"},"summary":"done"}' "$RUN_ID" "$1" "$2" "$3" > "$RECEIPT.tmp"
  mv "$RECEIPT.tmp" "$RECEIPT"
}
"#;

/// A task that requires `e2e` evidence gets a receipt without it parked as
/// `needs_session` (`evidence_missing`), not failed; the supervisor resumes
/// the session with the evidence request, and the rewritten receipt with
/// the evidence brings the run to `awaiting_integration`.
#[test]
fn missing_required_evidence_parks_the_run_for_a_resumed_session() {
    let (_dir, repo, db) = evidence_fixture(&[EvidenceCheck::E2e]);
    // The worker claims e2e passed but gives no evidence: without the
    // requirement that fails the receipt, with it the run waits.
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!("{RECEIPT_E2E}commit work; receipt_e2e \"$(git rev-parse HEAD)\" passed ' '"),
    );
    backend.resume_script_for(
        2,
        &format!(
            "{RECEIPT_E2E}await_message; receipt_e2e \"$(git rev-parse HEAD)\" passed 'cargo test --test e2e: 3 passed'; idle; await_exit"
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    let run = &detail.runs[0];
    // The worker knew up front.
    let prompt = read_prompt(run);
    assert!(prompt.contains("Required evidence: e2e ("), "{prompt}");
    // Validation parked it instead of failing it; the resolved resume is
    // validated again with its session open (ADR-0027 decision 3).
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 2);
    assert_eq!(validated[1]["status"], "awaiting_integration");
    assert_eq!(validated[0]["status"], "needs_session");
    assert_eq!(validated[0]["accepted"], false);
    assert_eq!(validated[0]["reason"], "evidence missing: e2e");
    assert_eq!(validated[0]["evidence_missing"], json!(["e2e"]));
    assert_eq!(validated[0]["code"], "evidence_missing");
    assert_eq!(validated[1].get("code"), None);
    assert!(validated[0]["result_commit"].is_string());
    assert_eq!(
        payloads(&detail, "evidence_missing"),
        [
            &json!({"code": "evidence_missing", "checks": ["e2e"], "reason": "evidence missing: e2e"})
        ]
    );
    // The worker's workspace was closed: the resume opens its own.
    assert!(event_kinds(&detail).contains(&"workspace_closed"));
    assert!(backend.closed().contains(&WORKSPACE_ID.to_owned()));
    // The resume asked for the missing check, not a rebase.
    let text = &backend.texts()[0].1;
    assert!(
        text.contains("found required evidence missing from the receipt"),
        "{text}"
    );
    assert!(text.contains("Reason: evidence missing: e2e"), "{text}");
    assert!(!text.contains("git rebase"), "{text}");
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["outcome"], "resolved");
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_leases().unwrap().is_empty());
    // It lands now that the receipt carries the evidence.
    let landed = integrate(&db, 2, &repo).unwrap();
    assert_eq!(landed["outcome"], "integrated", "{landed}");
}

/// A required check the receipt reports as `failed` is missing evidence
/// too: the run waits for a session instead of failing, and a resume that
/// reruns the check brings it to `awaiting_integration`.
#[test]
fn a_required_check_reported_failed_parks_the_run_instead_of_failing_it() {
    let (_dir, repo, db) = evidence_fixture(&[EvidenceCheck::E2e]);
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "{RECEIPT_E2E}commit work; receipt_e2e \"$(git rev-parse HEAD)\" failed 'cmux was not running'"
        ),
    );
    backend.resume_script_for(
        2,
        &format!(
            "{RECEIPT_E2E}await_message; receipt_e2e \"$(git rev-parse HEAD)\" passed 'e2e: 5 passed'; idle; await_exit"
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated[0]["status"], "needs_session");
    assert_eq!(validated[0]["reason"], "evidence missing: e2e");
    assert_eq!(
        payloads(&detail, "evidence_missing"),
        [
            &json!({"code": "evidence_missing", "checks": ["e2e"], "reason": "evidence missing: e2e"})
        ]
    );
    assert_eq!(
        payloads(&detail, "resume_finished")[0]["outcome"],
        "resolved"
    );
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
}

/// With the required evidence in the first receipt, validation accepts the
/// run as it would without a requirement.
#[test]
fn required_evidence_present_in_the_receipt_awaits_integration() {
    let (_dir, repo, db) = evidence_fixture(&[EvidenceCheck::E2e, EvidenceCheck::Tests]);
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "{RECEIPT_E2E}commit work; receipt_e2e \"$(git rev-parse HEAD)\" passed 'e2e: 3 passed'"
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(read_prompt(run).contains("Required evidence: e2e, tests ("));
    assert!(!event_kinds(&detail).contains(&"evidence_missing"));
    assert!(
        !payloads(&detail, "validation_finished")[0]
            .as_object()
            .unwrap()
            .contains_key("evidence_missing")
    );
}

/// A resumed session that comes back without the evidence has not resolved
/// the run: every attempt is `unresolved`, and once the resumes are used up
/// the run is `failed` and waits for a person's `decide` ask. An `integrate`
/// of such a run does not land either: it defers the run with the missing
/// `checks`.
#[test]
fn a_resume_or_integrate_without_the_required_evidence_does_not_land() {
    let (_dir, repo, db) = evidence_fixture(&[EvidenceCheck::E2e]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.resume_script_for(
        2,
        "await_message; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 3);
    assert!(finished.iter().all(|f| f["outcome"] == "unresolved"));
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::Decide);
    // Parked for a session again (as an older runtime left it), the run is
    // still not landed by `integrate`.
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='needs_session' WHERE id=?1",
            [&detail.runs[0].id()],
        )
        .unwrap();
    let before = git_out(&repo, &["rev-parse", "main"]);
    let deferred = integrate(&db, 2, &repo).unwrap();
    assert_eq!(deferred["outcome"], "needs_session", "{deferred}");
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), before);
    let detail = queue.show(TaskId::new(2)).unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::NeedsSession);
    assert_eq!(run.last_error(), Some("evidence missing: e2e"));
    let parked = payloads(&detail, "integration_deferred");
    assert_eq!(parked.last().unwrap()["checks"], json!(["e2e"]));
}

/// A fixture whose only ready task declares `paths` (ADR-0029).
fn scope_fixture(paths: &[&str]) -> (Fixture, PathBuf, PathBuf) {
    let (dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let task = queue
        .add(NewTask {
            title: "scoped".into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            priority: Default::default(),
            kind: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    assert_eq!(task.id(), TaskId::new(2));
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    (dir, repo, db)
}

/// A run of a task declaring `docs/**` that changes `change.txt` is parked
/// by validation as `needs_session` (`scope_violation`, with the paths),
/// not accepted; the supervisor resumes the session with a request to take
/// the path out, and the resolved run is validated again and lands.
#[test]
fn a_change_outside_the_declared_paths_parks_the_run_for_a_resumed_session() {
    let (_dir, repo, db) = scope_fixture(&["docs/**", "*.md"]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.resume_script_for(
        2,
        "await_message; unlocked git rm -q change.txt && mkdir -p docs && printf 'doc\\n' > docs/a.md && unlocked git add docs && unlocked git commit -q -m 'keep to docs'; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    let run = &detail.runs[0];
    let prompt = read_prompt(run);
    assert!(
        prompt.contains("Paths you may change (globs from the repository root; `*` stays in one directory, `**` spans any depth): docs/**, *.md."),
        "{prompt}"
    );
    let reason = "changed paths outside the task's --paths: change.txt";
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 2);
    assert_eq!(validated[0]["status"], "needs_session");
    assert_eq!(validated[0]["accepted"], false);
    assert_eq!(validated[0]["reason"], reason);
    assert_eq!(validated[0]["scope_violation"], json!(["change.txt"]));
    assert_eq!(validated[0]["allowed_paths"], json!(["docs/**", "*.md"]));
    assert!(validated[0]["result_commit"].is_string());
    assert_eq!(
        payloads(&detail, "scope_violation"),
        [
            &json!({"code": "scope_violation", "paths": ["change.txt"], "allowed": ["docs/**", "*.md"], "reason": reason})
        ]
    );
    assert!(!event_kinds(&detail).contains(&"evidence_missing"));
    // The resume asked to take the path out, not for a rebase or evidence.
    let text = &backend.texts()[0].1;
    assert!(
        text.contains("changes paths outside the task's --paths (docs/**, *.md)"),
        "{text}"
    );
    assert!(text.contains(&format!("Reason: {reason}")), "{text}");
    assert!(text.contains("Take the changes to the paths"), "{text}");
    assert_eq!(
        payloads(&detail, "resume_finished")[0]["outcome"],
        "resolved"
    );
    assert_eq!(validated[1]["status"], "awaiting_integration");
    assert!(
        !validated[1]
            .as_object()
            .unwrap()
            .contains_key("scope_violation")
    );
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let landed = integrate(&db, 2, &repo).unwrap();
    assert_eq!(landed["outcome"], "integrated", "{landed}");
    assert_eq!(
        git_out(&repo, &["show", "--name-only", "--format=", "main"]).trim(),
        "docs/a.md"
    );
}

/// A run that changes only declared paths is validated and landed as if
/// the task declared none.
#[test]
fn a_change_inside_the_declared_paths_awaits_integration_and_lands() {
    let (_dir, repo, db) = scope_fixture(&["*.txt"]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(!event_kinds(&detail).contains(&"scope_violation"));
    let landed = integrate(&db, 2, &repo).unwrap();
    assert_eq!(landed["outcome"], "integrated", "{landed}");
}

/// Validation diffs from where the branch forked from the current main,
/// not from the base commit: a branch rebased onto a main that gained
/// `src/lib.rs` from another task changes only `change.txt` itself.
#[test]
fn a_branch_rebased_onto_a_moved_main_is_held_only_to_its_own_changes() {
    let (_dir, repo, db) = scope_fixture(&["*.txt"]);
    // Another task lands src/lib.rs on main while the worker runs, and the
    // worker rebases onto it before its receipt.
    let backend = TestWorkspace::new(
        &db,
        false,
        "git switch -q -c side main && mkdir -p src && printf 'x\\n' > src/lib.rs && git add src && git commit -q -m other && git update-ref refs/heads/main HEAD && git switch -q - && git branch -q -D side && commit work && git rebase -q main; receipt \"$(git rev-parse HEAD)\"",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    assert!(!event_kinds(&detail).contains(&"scope_violation"));
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    let landed = integrate(&db, 2, &repo).unwrap();
    assert_eq!(landed["outcome"], "integrated", "{landed}");
    assert_eq!(
        git_out(&repo, &["show", "--name-only", "--format=", "main"]).trim(),
        "change.txt"
    );
}

/// `integrate` holds the diff it squashes (main..rebased head) to the
/// task's paths after its rebase: a commit outside them that a session
/// added after validation defers the run to `needs_session` without moving
/// main or running the verification commands, and the supervisor's resume
/// asks to take it out and then lands the approved run.
#[test]
fn integrate_refuses_a_rebased_diff_outside_the_declared_paths() {
    let (_dir, repo, db) = scope_fixture(&["*.txt"]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    // main moves, so the landing rebases.
    fs::write(repo.join("other.txt"), "main moved\n").unwrap();
    git(&repo, &["add", "other.txt"]);
    git(&repo, &["commit", "-m", "main moved"]);
    let main = git_out(&repo, &["rev-parse", "main"]);
    // After validation the branch gains a path outside `*.txt`.
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    fs::create_dir_all(worktree.join("src")).unwrap();
    fs::write(worktree.join("src/lib.rs"), "// out of scope\n").unwrap();
    git(&worktree, &["add", "src"]);
    git(&worktree, &["commit", "-m", "outside"]);
    write_receipt(
        &run,
        &git_out(&worktree, &["rev-parse", "HEAD"]),
        "succeeded",
        "more",
    );

    let deferred = integrate(&db, 2, &repo).unwrap();
    assert_eq!(deferred["outcome"], "needs_session", "{deferred}");
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main);
    let detail = queue.show(TaskId::new(2)).unwrap();
    let parked = &detail.runs[0];
    assert_eq!(parked.status(), RunStatus::NeedsSession);
    let reason = parked.last_error().unwrap();
    assert!(
        reason.starts_with(&format!(
            "changed paths outside the task's --paths: src/lib.rs after the rebase onto main {}",
            main.trim()
        )),
        "{reason}"
    );
    assert!(event_kinds(&detail).contains(&"integration_rebased"));
    let payload = payloads(&detail, "integration_deferred")[0];
    assert_eq!(payload["scope_violation"], json!(["src/lib.rs"]));
    assert_eq!(payload["allowed"], json!(["*.txt"]));
    assert!(integration_verifications(&detail).is_empty());

    // The supervisor resumes it with the scope request and lands it.
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.resume_script_for(
        2,
        "await_message; unlocked git rm -q -r src && unlocked git commit -q -m 'back to scope'; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let text = &backend.texts()[0].1;
    assert!(
        text.contains("changes paths outside the task's --paths (*.txt)"),
        "{text}"
    );
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(
        detail.task.status(),
        TaskStatus::Completed,
        "{:?}",
        event_kinds(&detail)
    );
    assert_eq!(
        git_out(&repo, &["show", "--name-only", "--format=", "main"]).trim(),
        "change.txt"
    );
}
