//! Runtime tests: `integrate`: landing, rebasing, pushing, the review material, the prompt
//! of a run and `rebind`.
use crate::runtime_support;

use runtime_support::*;

/// A Git remote double: `origin` exists unless `missing`, and a push fails
/// with `failure` when set. Every push is counted.
#[derive(Default)]
struct TestRemote {
    missing: bool,
    failure: Option<String>,
    pushes: Mutex<Vec<String>>,
}

impl MainRemote for TestRemote {
    fn has_remote(&self, remote: &str) -> Result<bool> {
        Ok(!self.missing && remote == "origin")
    }

    fn push_main(&self, remote: &str) -> Result<()> {
        self.pushes.lock().unwrap().push(remote.to_owned());
        match &self.failure {
            Some(failure) => bail!("{failure}"),
            None => Ok(()),
        }
    }
}

fn integrate_with(db: &Path, repo: &Path, remote: Option<&dyn MainRemote>) -> Value {
    runtime::integrate(db, IntegrateTarget::Task(TaskId::new(1)), repo, remote).unwrap()
}

#[test]
fn integrate_pushes_the_landed_main_to_origin() {
    let (_dir, repo, db, run) = awaiting_run();
    let remote = TestRemote::default();
    let outcome = integrate_with(&db, &repo, Some(&remote));
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(
        outcome["push"],
        json!({"outcome": "pushed", "remote": "origin", "error": null})
    );
    assert_eq!(*remote.pushes.lock().unwrap(), ["origin"]);
    let landed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(
        events_of(&db, run.id(), "push_finished"),
        [json!({"remote": "origin", "commit": landed})]
    );
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    // The landing's message is searchable with its task and run (ADR-0046).
    let subject = git_out(&repo, &["log", "-1", "--format=%s", "main"]);
    let page = SqliteQueue::open(&db)
        .unwrap()
        .search(&SearchQuery {
            terms: subject.clone(),
            kinds: vec![SearchKind::Commit],
            limit: 5,
            ..SearchQuery::default()
        })
        .unwrap();
    assert_eq!(page.total, 1, "{subject}");
    let hit = &page.hits[0];
    assert_eq!(hit.id, SearchRef::Commit(landed));
    assert_eq!(hit.title, subject.trim());
    assert_eq!(
        (hit.task_id, hit.run_id.as_deref(), hit.status.as_deref()),
        (Some(1), Some(run.id().as_str()), Some("completed"))
    );
}

#[test]
fn a_failed_push_keeps_the_landing_and_waits_as_attention() {
    let (_dir, repo, db, run) = awaiting_run();
    let remote = TestRemote {
        failure: Some("rejected: fetch first".into()),
        ..TestRemote::default()
    };
    let outcome = integrate_with(&db, &repo, Some(&remote));
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(outcome["run"]["status"], "integrated");
    assert_eq!(outcome["task"]["status"], "completed");
    assert_eq!(outcome["push"]["outcome"], "failed");
    assert_eq!(outcome["push"]["remote"], "origin");
    assert_eq!(outcome["push"]["error"], "rejected: fetch first");
    let landed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(
        events_of(&db, run.id(), "push_failed"),
        [
            json!({"code": "push_failed", "remote": "origin", "commit": landed, "error": "rejected: fetch first"})
        ]
    );
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Completed
    );
    drop(queue);

    // `status` keeps it as an attention on the integrated run, and `events`
    // reports the push_failed event with its next.
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        run_attention_of(&status, run.id()).unwrap(),
        &json!({
            "run_id": run.id(), "task_id": 1, "status": "integrated",
            "kind": "push_failed", "last_error": "rejected: fetch first",
            "last_error_code": "push_failed", "next": "push main",
        })
    );
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    let failed = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "push_failed")
        .unwrap();
    assert_eq!(failed["next"], "push main");
    assert_eq!(failed["reason"], "rejected: fetch first");

    // A later successful push carries this landing too and clears it.
    SqliteQueue::open(&db)
        .unwrap()
        .record_runtime_event(run.id(), "push_finished", json!({"remote": "origin"}))
        .unwrap();
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
}

#[test]
fn no_push_and_a_missing_origin_skip_the_push() {
    let (_dir, repo, db, run) = awaiting_run();
    let remote = TestRemote::default();
    let outcome = integrate_with(&db, &repo, None);
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(
        outcome["push"],
        json!({"outcome": "skipped", "remote": "origin", "error": null, "reason": "--no-push"})
    );
    assert!(remote.pushes.lock().unwrap().is_empty());
    let skipped = events_of(&db, run.id(), "push_skipped");
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["reason"], "--no-push");

    let (_dir, repo, db, run) = awaiting_run();
    let remote = TestRemote {
        missing: true,
        ..TestRemote::default()
    };
    let outcome = integrate_with(&db, &repo, Some(&remote));
    assert_eq!(outcome["push"]["outcome"], "skipped", "{outcome}");
    assert_eq!(
        outcome["push"]["reason"],
        "the repository has no remote origin"
    );
    assert!(remote.pushes.lock().unwrap().is_empty());
    assert_eq!(events_of(&db, run.id(), "push_skipped").len(), 1);
}

/// The real Git adapter pushes main to a bare origin, and reports Git's
/// error when origin cannot take it.
#[test]
fn git_adapter_pushes_main_to_a_bare_origin() {
    let (dir, repo, db, run) = awaiting_run();
    let origin = dir.path().join("origin.git");
    let made = Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(&origin)
        .bounded_output()
        .unwrap();
    assert!(made.status.success());
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["push"]["outcome"], "pushed", "{outcome}");
    assert_eq!(
        git_out(&origin, &["rev-parse", "main"]),
        git_out(&repo, &["rev-parse", "main"])
    );
    assert_eq!(events_of(&db, run.id(), "push_finished").len(), 1);

    // An origin that is not a repository fails the push with Git's message.
    let adapter = GitRepository::inspect(&repo).unwrap();
    git(
        &repo,
        &["remote", "set-url", "origin", "/nonexistent/origin.git"],
    );
    assert!(adapter.has_remote("origin").unwrap());
    assert!(!adapter.has_remote("upstream").unwrap());
    let error = format!("{:#}", adapter.push_main("origin").unwrap_err());
    assert!(error.contains("git push origin main failed"), "{error}");
}

/// A task whose fake agent commits `file` with `content`; verification
/// commands default to the fixture's `test -f seed.txt`.
fn add_file_task(
    queue: &mut SqliteQueue,
    backend: &TestWorkspace,
    title: &str,
    file: &str,
    content: &str,
    verify: &[&str],
) -> TaskId {
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: verify.iter().map(|v| (*v).to_owned()).collect(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            kind: None,
            dependencies: vec![],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    backend.script_for(
        task.id().as_i64(),
        &format!(
            "printf '{content}\\n' > '{file}' && git add '{file}' && git commit -q -m '{title}'; receipt \"$(git rev-parse HEAD)\""
        ),
    );
    task.id()
}

#[test]
fn review_writes_the_run_material_to_review_md_and_returns_only_its_size() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let goal = queue
        .add_goal(NewGoal {
            title: "goal title".into(),
            description: String::new(),
            acceptance: "goal acceptance".into(),
            constraints: "goal constraints".into(),
            doc: None,
            draft: false,
        })
        .unwrap();
    queue.set_goal(TaskId::new(1), Some(goal.id())).unwrap();
    // A task without a run to review is refused.
    let error = format!("{:#}", runtime::review(&db, TaskId::new(1)).unwrap_err());
    assert!(
        error.contains("task 1 (ready) has no run awaiting integration or a session"),
        "{error}"
    );
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let head = run.result_commit().cloned().unwrap();
    let mut receipt = session_receipt(&run, head.as_str(), "succeeded", "summary of the change");
    receipt["follow_ups"] = json!([{"title": "later work", "description": "outside the task"}]);
    write_receipt_json(&run, receipt);

    let outcome = runtime::review(&db, TaskId::new(1)).unwrap();
    let path = Path::new(run.run_dir().unwrap()).join("review.md");
    assert_eq!(
        outcome,
        json!({
            "run_id": run.id(),
            "task_id": 1,
            "path": path.to_str().unwrap(),
            "base": run.base_commit(),
            "head": head,
            "files_changed": 1,
            "insertions": 1,
            "deletions": 0,
        })
    );
    assert!(!outcome.to_string().contains("diff --git"));
    assert!(!path.with_file_name(".review.md.tmp").exists());
    let text = fs::read_to_string(&path).unwrap();
    let sections = [
        "# Review of task 1: test task",
        "## Task",
        "### Description\n\nsmall change",
        "### Acceptance\n\nworks",
        "### Verification commands\n\n```sh\ntest -f seed.txt\n```",
        "## Goal 1: goal title",
        "### Goal acceptance\n\ngoal acceptance",
        "### Goal constraints\n\ngoal constraints",
        "## Receipt",
        "### Summary\n\nsummary of the change",
        "### Tests: passed\n\nreran",
        "### E2E: not_applicable\n\nnone",
        "### Subagent review: not_applicable\n\nsession",
        "### Follow-ups\n\n- later work: outside the task",
        "## Commits",
        "## Diffstat",
        "## Diff",
    ];
    let mut at = 0;
    for section in sections {
        let found = text[at..]
            .find(section)
            .unwrap_or_else(|| panic!("{section:?} missing after byte {at}:\n{text}"));
        at += found + section.len();
    }
    let commits = &text[text.find("## Commits").unwrap()..text.find("## Diffstat").unwrap()];
    assert!(commits.contains(&head.as_str()[..7]), "{commits}");
    assert!(commits.contains(" work\n"), "{commits}");
    let stat = &text[text.find("## Diffstat").unwrap()..text.find("## Diff\n").unwrap()];
    assert!(stat.contains("change.txt | 1 +"), "{stat}");
    let diff = &text[text.find("## Diff\n").unwrap()..];
    assert!(
        diff.contains("```diff\ndiff --git a/change.txt b/change.txt"),
        "{diff}"
    );
    assert!(diff.contains(&format!("+change by {}", run.id())), "{diff}");
    assert!(diff.ends_with("\n```\n"), "{diff}");

    // A landed run is no longer reviewable.
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let error = format!("{:#}", runtime::review(&db, TaskId::new(1)).unwrap_err());
    assert!(error.contains("task 1 (completed) has no run"), "{error}");
}

/// A run whose file is Latin-1 text with a backtick run and whose commit
/// message is not UTF-8 still gets its review: Git's raw bytes go into
/// review.md under a longer fence, and the commit list is read lossily.
#[test]
fn review_writes_a_non_utf8_diff_as_raw_bytes() {
    let (_dir, db, detail) = run_agent(
        r#"printf 'caf\351 ````\n' > latin1.txt && git add latin1.txt && git commit -q -m "$(printf 'caf\351')"; receipt "$(git rev-parse HEAD)""#,
    );
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let outcome = runtime::review(&db, TaskId::new(1)).unwrap();
    assert_eq!(outcome["files_changed"], 1, "{outcome}");
    assert_eq!(outcome["insertions"], 1, "{outcome}");
    let run_dir = Path::new(run.run_dir().unwrap());
    let bytes = fs::read(run_dir.join("review.md")).unwrap();
    assert!(String::from_utf8(bytes.clone()).is_err());
    let text = String::from_utf8_lossy(&bytes);
    let commits = &text[text.find("## Commits").unwrap()..text.find("## Diffstat").unwrap()];
    // Git may re-encode the message on output; either way it is listed.
    assert!(commits.contains(" caf"), "{commits}");
    let diff_at = bytes.windows(8).position(|w| w == b"## Diff\n").unwrap();
    let diff = &bytes[diff_at..];
    let needle = b"+caf\xe9 ````\n";
    assert!(diff.windows(needle.len()).any(|w| w == needle), "{text}");
    assert!(
        text[text.find("## Diff\n").unwrap()..]
            .contains("`````diff\ndiff --git a/latin1.txt b/latin1.txt"),
        "{text}"
    );
    assert!(diff.ends_with(b"\n`````\n"), "{text}");
    // No temporary file is left beside review.md (the others are the
    // supervisor's headless review's).
    let leftovers: Vec<_> = fs::read_dir(run_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with(".review.md"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(run_dir.join("review.md").is_file());
}

#[test]
fn conflict_free_run_lands_as_one_squash_commit_and_releases_dependents() {
    let (dir, repo, db, run) = awaiting_run();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(seed, *run.base_commit());
    let source = run.result_commit().cloned().unwrap();
    // Another repository is refused even though it also has a main branch.
    let other = dir.path().join("other");
    fs::create_dir(&other).unwrap();
    git(&other, &["init", "-b", "main"]);
    git(&other, &["config", "user.name", "test"]);
    git(&other, &["config", "user.email", "test@example.invalid"]);
    git(&other, &["commit", "--allow-empty", "-m", "unrelated"]);
    let error = format!("{:#}", integrate(&db, 1, &other).unwrap_err());
    assert!(error.contains("the queue is bound to"), "{error}");
    assert!(integrate(&db, 1, &dir.path().join("missing")).is_err());

    // Landing from the run's own worktree resolves the same repository,
    // and the push that follows the worktree's removal still reaches Git.
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let outcome = integrate(&db, 1, &worktree).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(outcome["push"]["outcome"], "skipped", "{outcome}");
    // Main has not moved, so the rebase was a no-op; the verification
    // commands still run here, their only run for this commit (ADR-0023).
    assert_eq!(outcome["verification_skipped"], json!(false), "{outcome}");
    assert_eq!(outcome["task"]["status"], "completed");
    assert_eq!(outcome["run"]["status"], "integrated");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let landed = detail.runs[0].clone();
    assert_landed(&repo, &landed, "test task", &seed);
    assert!(landed.last_error().is_none());
    // No rebase was needed: the landed tree is the validated tree, and the
    // history ref points at the validated commit.
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", run.id())]
        ),
        source
    );
    assert_eq!(
        git_out(&repo, &["rev-parse", "main^{tree}"]),
        git_out(&repo, &["rev-parse", &format!("{source}^{{tree}}")])
    );
    let message = git_out(&repo, &["log", "-1", "--format=%B", "main"]);
    assert_eq!(
        message,
        format!("test task\n\ndone\n\nDagq-Task: 1\nDagq-Run: {}", run.id())
    );
    // The main checkout moved with the ref.
    assert_eq!(
        git_out(&repo, &["rev-parse", "HEAD"]),
        landed.result_commit().cloned().unwrap()
    );
    assert_eq!(git_out(&repo, &["status", "--porcelain"]), "");
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        format!("change by {}\n", run.id())
    );
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().rposition(|k| *k == kind).unwrap();
    assert!(position("validation_finished") < position("integration_started"));
    assert!(position("integration_started") < position("integration_rebased"));
    // The only verification commands are the landing's, after the rebase.
    let first = kinds
        .iter()
        .position(|k| *k == "verification_command")
        .unwrap();
    assert!(position("integration_rebased") < first);
    assert!(position("verification_command") < position("run_integrated"));
    assert!(!kinds.contains(&"integration_verification_skipped"));
    assert!(position("run_integrated") < position("worktree_removed"));
    assert!(!kinds.contains(&"cleanup_failed"));
    let integrated = detail
        .events
        .iter()
        .find(|e| e.kind == "run_integrated")
        .unwrap();
    assert_eq!(integrated.run_id.as_ref(), Some(run.id()));
    assert_eq!(
        integrated.payload["result_commit"],
        json!(landed.result_commit())
    );
    assert_eq!(integrated.payload["source_commit"], json!(source));
    assert_eq!(integrated.payload["main_before"], json!(seed));
    assert_eq!(
        integrated.payload["history_ref"],
        json!(format!("refs/dagq/runs/{}", run.id()))
    );
    assert_eq!(integrated.payload["verification_skipped"], json!(false));
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 1, "{verifications:?}");
    assert_eq!(verifications[0]["command"], "test -f seed.txt");
    assert_eq!(verifications[0]["exit_code"], 0);
    // Each command's time and the load it ran under (task 197).
    let duration = verifications[0]["duration_secs"].as_f64().unwrap();
    assert!((0.0..60.0).contains(&duration), "{}", verifications[0]);
    assert_load(verifications[0]);
    // The claim records the binary, the host and the load it was made
    // under; each interval's end, its load.
    let event = |kind: &str| {
        &detail
            .events
            .iter()
            .find(|e| e.kind == kind)
            .unwrap_or_else(|| panic!("no {kind}"))
            .payload
    };
    let claimed = event("run_claimed");
    assert_eq!(claimed["dagq_version"], dagq::VERSION, "{claimed}");
    assert_eq!(claimed["parallel"], 4, "{claimed}");
    assert_eq!(claimed["slots"], 0, "{claimed}");
    assert!(claimed["load_avg"].is_f64(), "{claimed}");
    // The stub agent is no versioned install of Claude Code.
    assert!(claimed["claude_version"].is_null(), "{claimed}");
    assert!(
        claimed["rustc_release"]
            .as_str()
            .is_some_and(|release| release.split('.').count() == 3),
        "{claimed}"
    );
    assert!(
        claimed["rustc_host"]
            .as_str()
            .is_some_and(|host| host.contains('-')),
        "{claimed}"
    );
    assert!(claimed.get("path").is_none(), "{claimed}");
    assert_load(event("receipt_observed"));
    assert_load(event("validation_finished"));
    assert!(
        detail
            .events
            .iter()
            .all(|e| e.kind != "verification_command" || e.payload["phase"] == "integration"),
        "{:?}",
        event_kinds(&detail)
    );
    let run_dir = Path::new(run.run_dir().unwrap());
    assert!(run_dir.join("integrate-1-verify-1.log").exists());
    assert!(!run_dir.join("verify-1.log").exists());
    let changed = detail
        .events
        .iter()
        .find(|e| e.kind == "task_status_changed" && e.payload["to"] == "completed")
        .unwrap();
    assert_eq!(changed.run_id.as_ref(), Some(run.id()));
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(
        queue
            .candidates()
            .unwrap()
            .iter()
            .map(|t| t.id())
            .collect::<Vec<_>>(),
        [TaskId::new(2)]
    );

    // Integration is one-shot, at every layer.
    let error = format!("{:#}", integrate(&db, 1, &repo).unwrap_err());
    assert!(error.contains("no run awaiting integration"), "{error}");
    assert!(queue.begin_integration(run.id(), "x", &sha(&seed)).is_err());
    let error = format!("{:#}", integrate(&db, 2, &repo).unwrap_err());
    assert!(error.contains("task 2 (ready) has no run"), "{error}");
    assert!(integrate(&db, 99, &repo).is_err());
    assert_eq!(integrate_next(&db, &repo)["outcome"], "no_run_awaiting");
    let raw = Connection::open(&db).unwrap();
    assert!(
        raw.execute(
            "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
             VALUES ('again',1,'integrated','claude','claude',?1)",
            [&run.base_commit()],
        )
        .is_err()
    );
    assert_eq!(queue.show(TaskId::new(1)).unwrap().runs.len(), 1);
}

/// Move everything the queue at `db` owns (the database with its WAL files,
/// `runs/`, logs) from its directory into `to`, leaving the repository. The
/// runs keep the absolute paths they stored at claim time, as every queue
/// written before ADR-0017 does. Returns the database's new path.
fn move_queue(db: &Path, repo: &Path, to: &Path) -> PathBuf {
    fs::create_dir(to).unwrap();
    for entry in fs::read_dir(db.parent().unwrap()).unwrap() {
        let path = entry.unwrap().path();
        if path != repo && path != to {
            fs::rename(&path, to.join(path.file_name().unwrap())).unwrap();
        }
    }
    to.join(db.file_name().unwrap())
}

#[test]
fn moved_queue_directory_resolves_run_paths_and_lands_awaiting_runs() {
    let (dir, repo, db, run) = awaiting_run();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    let old_runs = dagq::infrastructure::location::runs_dir(&db.canonicalize().unwrap());
    // An unfinished run next to the awaiting one, for `status` and `doctor`.
    let other = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "unfinished", &[])
    };
    let unfinished = orphan_run(&repo, &db, "old-supervisor", dead_pid(), dead_pid());
    assert_eq!(unfinished.task_id(), other);

    let moved = dir.path().join("moved queue");
    let db = move_queue(&db, &repo, &moved);
    let runs = moved.canonicalize().unwrap().join("runs");
    assert!(!old_runs.exists());
    // The database still holds the paths of the old location: nothing is
    // migrated, they are resolved again from the run ID on every read.
    let raw = Connection::open(&db).unwrap();
    let stored: String = raw
        .query_row(
            "SELECT worktree_path FROM task_runs WHERE id=?1",
            [&run.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, run.worktree_path().unwrap().to_owned());
    assert!(stored.starts_with(old_runs.to_str().unwrap()), "{stored}");
    // Git still records the worktrees at their old paths.
    assert!(git_out(&repo, &["worktree", "list"]).contains("prunable"));

    let text = |path: PathBuf| Some(path.to_str().unwrap().to_owned());
    let expected = |id: &RunId| dagq::domain::RunPaths::new(&runs, id);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let shown = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let paths = expected(run.id());
    assert_eq!(shown.run_dir(), text(paths.run_dir.clone()).as_deref());
    assert_eq!(
        shown.worktree_path(),
        text(paths.worktree.clone()).as_deref()
    );
    assert_eq!(shown.receipt_path(), text(paths.receipt.clone()).as_deref());
    assert_eq!(shown.log_path(), text(paths.log.clone()).as_deref());
    assert_eq!(shown.repo_path(), run.repo_path());
    assert!(paths.worktree.is_dir() && paths.receipt.is_file());

    let status = runtime::status(&db).unwrap();
    let entry = &status["runs"].as_array().unwrap()[0];
    assert_eq!(entry["run_id"], json!(unfinished.id()), "{status}");
    assert_eq!(
        entry["worktree_path"],
        json!(text(expected(unfinished.id()).worktree)),
        "{status}"
    );
    let doctor = runtime::doctor(&db, true).unwrap();
    let health = &doctor["runs"].as_array().unwrap()[0];
    assert_eq!(health["run_id"], json!(unfinished.id()), "{doctor}");
    assert_eq!(
        health["worktree_path"],
        json!(text(expected(unfinished.id()).worktree))
    );
    assert_eq!(health["worktree_exists"], json!(true), "{doctor}");
    assert_eq!(
        health["run_dir"],
        json!(text(expected(unfinished.id()).run_dir))
    );
    assert_eq!(health["run_dir_exists"], json!(true), "{doctor}");

    // The awaiting run lands from the new location, and its worktree, whose
    // Git record is repaired on the way, is removed with its branch.
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let landed = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_landed(&repo, &landed, "test task", &seed);
    let kinds = event_kinds(&queue.show(TaskId::new(1)).unwrap())
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"worktree_removed".to_owned()), "{kinds:?}");
    assert!(!kinds.contains(&"cleanup_failed".to_owned()), "{kinds:?}");
    let listing = git_out(&repo, &["worktree", "list", "--porcelain"]);
    assert!(!listing.contains(run.id().as_str()), "{listing}");
}

/// A dependent's prompt names each predecessor with the commit `integrate`
/// landed and the summary its receipt carried, and lists the other tasks in
/// progress at claim time without the task itself.
#[test]
fn prompt_describes_landed_predecessors_and_sibling_tasks_in_progress() {
    let (_dir, repo, db, run) = awaiting_run();
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let landed = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(landed.status(), RunStatus::Integrated);
    let landed_commit = landed.result_commit().cloned().unwrap();
    assert_eq!(landed_commit, git_out(&repo, &["rev-parse", "main"]));
    // The squash commit, not the run's validated head, is what the prompt names.
    assert_ne!(landed_commit, *run.result_commit().unwrap());
    add_ready_task(&mut queue, "independent", &[]);

    // The queue's read-only view the prompt is built from.
    let predecessors = queue.predecessors(TaskId::new(2)).unwrap();
    assert_eq!(predecessors.len(), 1);
    assert_eq!(predecessors[0].task.id(), TaskId::new(1));
    assert_eq!(predecessors[0].task.title(), "test task");
    let integrated = predecessors[0].integrated_run.as_ref().unwrap();
    assert_eq!(integrated.id(), run.id());
    assert_eq!(
        integrated.result_commit().map(CommitSha::as_str),
        Some(landed_commit.as_str())
    );
    assert!(queue.predecessors(TaskId::new(3)).unwrap().is_empty());
    assert!(queue.tasks_in_progress().unwrap().is_empty());

    // The dependent (task 2) is claimed before the independent task 3.
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2);

    let dependent = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(dependent.status(), RunStatus::AwaitingIntegration);
    assert_eq!(*dependent.base_commit(), landed_commit);
    let prompt = read_prompt(&dependent);
    assert!(
        prompt.contains(&format!(
            "Predecessor tasks (their changes are already in your base commit):\n\
             - task 1: test task; result commit {landed_commit}; summary: done\n"
        )),
        "{prompt}"
    );
    assert!(!prompt.contains("Predecessor tasks: none"), "{prompt}");
    // Nothing else was in progress when task 2 was claimed; it is not listed itself.
    assert!(
        prompt.contains("Sibling tasks in progress: none\n"),
        "{prompt}"
    );
    assert!(!prompt.contains("- task 2: dependent"), "{prompt}");

    let independent = queue.show(TaskId::new(3)).unwrap().runs[0].clone();
    assert_eq!(independent.status(), RunStatus::AwaitingIntegration);
    let prompt = read_prompt(&independent);
    assert!(prompt.contains("Predecessor tasks: none\n"), "{prompt}");
    assert!(
        prompt.contains(
            "Sibling tasks in progress (other tasks executing now, each owning its own scope):\n\
             - task 2: dependent\n"
        ),
        "{prompt}"
    );
    assert!(!prompt.contains("- task 3: independent"), "{prompt}");
    // A completed task is not in progress.
    assert!(!prompt.contains("- task 1: test task"), "{prompt}");
    // The sections sit between the verification commands and the receipt contract.
    let position = |needle: &str| prompt.find(needle).unwrap();
    assert!(position("Verification commands") < position("Predecessor tasks"));
    assert!(position("Predecessor tasks") < position("Sibling tasks in progress"));
    assert!(position("Sibling tasks in progress") < position("Write a completion receipt"));
}

/// A ready task with a goal and a context, registered on the queue.
fn add_ready_task_in(
    queue: &mut SqliteQueue,
    title: &str,
    goal_id: Option<GoalId>,
    context: &str,
) -> TaskId {
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            kind: None,
            dependencies: vec![],
            goal_dependencies: Vec::new(),
            goal_id,
            context: context.into(),
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    task.id()
}

/// The section headings of a prompt in order of appearance, so tests can
/// compare the shape of prompts with and without a goal.
fn section_order(prompt: &str) -> Vec<usize> {
    [
        "Task title:",
        "Verification commands",
        "Goal",
        "Context",
        "Predecessor tasks",
        "Sibling tasks in progress",
        "Your assignment is this task only.",
        "Write a completion receipt",
    ]
    .iter()
    .map(|heading| {
        prompt
            .find(heading)
            .unwrap_or_else(|| panic!("{heading}: {prompt}"))
    })
    .collect()
}

/// A task's prompt carries its goal as a Goal section (ID, title,
/// description, acceptance, constraints and the doc path, unread) and its
/// context as a Context section; a task without either says so in the same
/// place, so both prompts have the same sequence of sections.
#[test]
fn prompt_describes_the_goal_and_the_context_and_keeps_one_shape_without_them() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let goal = queue
        .add_goal(NewGoal {
            title: "goal title".into(),
            description: "goal description\nsecond line".into(),
            acceptance: "goal acceptance".into(),
            constraints: "goal constraints".into(),
            doc: Some("docs/plans/goal.md".into()),
            draft: false,
        })
        .unwrap();
    fs::write(repo.join("unrelated.md"), "not read\n").unwrap();
    add_ready_task_in(
        &mut queue,
        "grouped",
        Some(goal.id()),
        "why this task exists\nread docs/design/x.md first",
    );
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2);

    let alone = read_prompt(&queue.show(TaskId::new(1)).unwrap().runs[0]);
    assert!(
        alone.contains("Goal: none, this task stands alone\n"),
        "{alone}"
    );
    assert!(alone.contains("Context: none\n"), "{alone}");
    assert!(!alone.contains("goal title"), "{alone}");

    let grouped = read_prompt(&queue.show(TaskId::new(2)).unwrap().runs[0]);
    assert!(
        grouped.contains(&format!(
            "Goal (the higher-level problem this task and its sibling tasks solve together):\n\
             Goal ID: {}\nGoal title: goal title\nGoal description:\ngoal description\nsecond line\n\
             Goal acceptance:\ngoal acceptance\nGoal constraints:\ngoal constraints\n\
             Goal doc: docs/plans/goal.md (a path in the repository; read it for the full picture)\n",
            goal.id()
        )),
        "{grouped}"
    );
    // The doc is named by path only; the prompt never embeds its content.
    assert!(!grouped.contains("not read"), "{grouped}");
    assert!(
        grouped.contains(
            "Context (why this task exists and what to read first):\n\
             why this task exists\nread docs/design/x.md first\n"
        ),
        "{grouped}"
    );
    assert!(!grouped.contains("Goal: none"), "{grouped}");
    assert!(!grouped.contains("Context: none"), "{grouped}");
    // Both prompts have the same sections in the same order.
    let order = section_order(&grouped);
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{grouped}");
    let order = section_order(&alone);
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{alone}");
    // The receipt example shows the optional follow_ups, and the scope rule names it.
    for prompt in [&alone, &grouped] {
        assert!(
            prompt.contains(
                "\"summary\":\"...\",\"follow_ups\":[{\"title\":\"...\",\"description\":\"...\"}]}\n"
            ),
            "{prompt}"
        );
        assert!(
            prompt.contains(
                "Your assignment is this task only. Do not change what a sibling task owns; \
                 if you find work outside this task, record it in the receipt as follow_ups instead of doing it.\n"
            ),
            "{prompt}"
        );
        assert!(prompt.contains("follow_ups is optional"), "{prompt}");
        // The worker reads only what its run needs, never the queue.
        assert!(prompt.contains(runtime::WORKER_READING), "{prompt}");
        assert!(
            prompt.contains("the worker section of the repository instructions"),
            "{prompt}"
        );
        assert!(prompt.contains("Do not run `dagq list`"), "{prompt}");
        assert!(
            !prompt.contains("Read its repository instructions"),
            "{prompt}"
        );
    }
}

/// The Sibling section of a task with a goal lists only the in-progress
/// tasks of that goal; a task without a goal still sees every task in
/// progress. Claims in one pass go in ID order, so a prompt lists the
/// siblings claimed before it.
#[test]
fn prompt_lists_only_the_siblings_of_the_same_goal_in_progress() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    // Task 1 (no goal) is in progress before the grouped tasks are claimed.
    assert_eq!(
        supervise(&db, &repo, &backend).unwrap()["errors"],
        json!([])
    );
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::InProgress
    );
    let a = queue
        .add_goal(NewGoal {
            title: "goal a".into(),
            ..NewGoal::default()
        })
        .unwrap();
    let b = queue
        .add_goal(NewGoal {
            title: "goal b".into(),
            ..NewGoal::default()
        })
        .unwrap();
    assert_eq!(
        add_ready_task_in(&mut queue, "a first", Some(a.id()), ""),
        TaskId::new(2)
    );
    assert_eq!(
        add_ready_task_in(&mut queue, "b only", Some(b.id()), ""),
        TaskId::new(3)
    );
    assert_eq!(
        add_ready_task_in(&mut queue, "a second", Some(a.id()), ""),
        TaskId::new(4)
    );
    assert_eq!(
        add_ready_task_in(&mut queue, "alone", None, ""),
        TaskId::new(5)
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 4);

    let mut prompt_of =
        |task_id: i64| read_prompt(&queue.show(TaskId::new(task_id)).unwrap().runs[0]);
    // Task 2: task 1 is in progress but belongs to no goal, so it is not a sibling.
    let first = prompt_of(2);
    assert!(
        first.contains("Sibling tasks in progress: none\n"),
        "{first}"
    );
    assert!(!first.contains("- task 1: test task"), "{first}");
    // Task 3: nothing of goal b is in progress; goal a's task 2 is not listed.
    let only = prompt_of(3);
    assert!(only.contains("Sibling tasks in progress: none\n"), "{only}");
    assert!(!only.contains("- task 2: a first"), "{only}");
    // Task 4: its sibling task 2 is listed, task 3 of goal b and task 1 are not.
    let second = prompt_of(4);
    assert!(
        second.contains(
            "Sibling tasks in progress (other tasks executing now, each owning its own scope):\n\
             - task 2: a first\n"
        ),
        "{second}"
    );
    assert!(!second.contains("- task 3: b only"), "{second}");
    assert!(!second.contains("- task 1: test task"), "{second}");
    assert!(!second.contains("- task 4: a second"), "{second}");
    // Task 5 has no goal and sees every task in progress except itself.
    let alone = prompt_of(5);
    assert!(
        alone.contains(
            "Sibling tasks in progress (other tasks executing now, each owning its own scope):\n\
             - task 1: test task\n- task 2: a first\n- task 3: b only\n- task 4: a second\n"
        ),
        "{alone}"
    );
    assert!(!alone.contains("- task 5: alone"), "{alone}");
}

/// `prompt.txt` is a snapshot taken at claim time: a run claimed before a
/// goal edit keeps the old wording, and a run claimed after it gets the new.
#[test]
fn prompt_snapshots_the_goal_at_claim_time() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let goal = queue
        .add_goal(NewGoal {
            title: "before edit".into(),
            acceptance: "old acceptance".into(),
            ..NewGoal::default()
        })
        .unwrap();
    add_ready_task_in(&mut queue, "early", Some(goal.id()), "");
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    assert_eq!(
        supervise(&db, &repo, &backend).unwrap()["errors"],
        json!([])
    );
    backend.join();
    let early = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    let before = read_prompt(&early);
    assert!(before.contains("Goal title: before edit\n"), "{before}");
    assert!(
        before.contains("Goal acceptance:\nold acceptance\n"),
        "{before}"
    );

    queue
        .edit_goal(
            goal.id(),
            GoalEdit {
                title: Some("after edit".into()),
                acceptance: Some("new acceptance".into()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
    add_ready_task_in(&mut queue, "late", Some(goal.id()), "");
    assert_eq!(
        supervise(&db, &repo, &backend).unwrap()["errors"],
        json!([])
    );
    backend.join();
    let late = queue.show(TaskId::new(3)).unwrap().runs[0].clone();
    let after = read_prompt(&late);
    assert!(after.contains("Goal title: after edit\n"), "{after}");
    assert!(
        after.contains("Goal acceptance:\nnew acceptance\n"),
        "{after}"
    );
    assert!(!after.contains("before edit"), "{after}");
    // The earlier run's prompt was not rewritten by the edit or the later claim.
    assert_eq!(read_prompt(&early), before);
    // The late run also sees the early one as a sibling still in progress.
    assert!(after.contains("- task 2: early\n"), "{after}");
}

/// A predecessor whose receipt is gone from its run directory is still
/// named in the prompt; the successor's run starts and runs as usual.
#[test]
fn successor_starts_when_the_predecessor_receipt_is_unavailable() {
    let (_dir, repo, db, run) = awaiting_run();
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let receipt = Path::new(run.receipt_path().unwrap());
    assert_eq!(
        receipt,
        Path::new(run.run_dir().unwrap()).join("receipt.json")
    );
    fs::remove_file(receipt).unwrap();

    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["errors"], json!([]));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    let dependent = &detail.runs[0];
    assert_eq!(dependent.status(), RunStatus::AwaitingIntegration);
    let kinds = event_kinds(&detail);
    assert!(kinds.contains(&"agent_started"), "{kinds:?}");
    let landed_commit = queue.show(TaskId::new(1)).unwrap().runs[0]
        .result_commit()
        .cloned()
        .unwrap();
    let prompt = read_prompt(dependent);
    assert!(
        prompt.contains(&format!(
            "- task 1: test task; result commit {landed_commit}; summary: (receipt unavailable)\n"
        )),
        "{prompt}"
    );
    // A corrupt receipt is described the same way.
    let corrupt = queue.predecessors(TaskId::new(2)).unwrap();
    fs::write(receipt, "not json").unwrap();
    let summary = runtime::PredecessorSummary::from_predecessor(&LocalRunFiles, &corrupt[0]);
    assert_eq!(summary.summary, "(receipt unavailable)");
    assert_eq!(summary.result_commit, landed_commit);
    assert_eq!(
        (summary.task_id, summary.title.as_str()),
        (TaskId::new(1), "test task")
    );
    // A predecessor completed without an integrated run has neither.
    let by_hand = dagq::domain::Predecessor {
        task: corrupt[0].task.clone(),
        integrated_run: None,
    };
    let summary = runtime::PredecessorSummary::from_predecessor(&LocalRunFiles, &by_hand);
    assert_eq!(summary.result_commit, "(not landed)");
    assert_eq!(summary.summary, "(receipt unavailable)");
}

/// Two accepted runs land in the order their validation finished; the
/// second is rebased onto the first landing and main stays linear with one
/// commit per task, whether or not a checkout has main checked out.
#[test]
fn runs_land_fifo_by_validation_time_and_later_ones_are_rebased() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(TaskId::new(1), TaskAction::Draft).unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let a = add_file_task(
        &mut queue,
        &backend,
        "task a",
        "a.txt",
        "a",
        &["test -f seed.txt"],
    );
    let b = add_file_task(
        &mut queue,
        &backend,
        "task b",
        "b.txt",
        "b",
        &["test -f seed.txt"],
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2);
    let seed = git_out(&repo, &["rev-parse", "main"]);
    let run_a = queue.show(a).unwrap().runs[0].clone();
    let run_b = queue.show(b).unwrap().runs[0].clone();
    // FIFO is by validation time, not task id.
    let mut validated_at = |run: &TaskRun| {
        queue
            .show(run.task_id())
            .unwrap()
            .events
            .iter()
            .find(|e| e.kind == "validation_finished")
            .unwrap()
            .id
    };
    let (first, second) = if validated_at(&run_a) < validated_at(&run_b) {
        (run_a.clone(), run_b.clone())
    } else {
        (run_b.clone(), run_a.clone())
    };
    assert_eq!(
        queue.next_awaiting_integration().unwrap().unwrap().id(),
        first.id()
    );

    let outcome = integrate_next(&db, &repo);
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(outcome["run"]["id"], json!(first.id()));
    let first_landed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(
        queue.next_awaiting_integration().unwrap().unwrap().id(),
        second.id()
    );

    // No checkout has main now: the ref is updated directly.
    git(&repo, &["checkout", "-q", "--detach"]);
    let outcome = integrate_next(&db, &repo);
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(outcome["run"]["id"], json!(second.id()));
    assert_eq!(integrate_next(&db, &repo)["outcome"], "no_run_awaiting");
    let second_landed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), first_landed); // Detached HEAD untouched.
    git(&repo, &["checkout", "-q", "main"]);

    let landed_second = queue.show(second.task_id()).unwrap().runs[0].clone();
    assert_landed(&repo, &landed_second, "task ", &first_landed);
    let landed_first = queue.show(first.task_id()).unwrap().runs[0].clone();
    assert_eq!(
        landed_first.result_commit().map(CommitSha::as_str),
        Some(first_landed.as_str())
    );
    // Linear: seed → first → second, one commit per task, both files present.
    assert_eq!(
        git_out(&repo, &["rev-list", "--first-parent", "main"])
            .lines()
            .collect::<Vec<_>>(),
        [second_landed.as_str(), first_landed.as_str(), seed.as_str()]
    );
    assert!(repo.join("a.txt").exists() && repo.join("b.txt").exists());
    // The second run was rebased: its history ref sits on the first landing.
    let history = git_out(
        &repo,
        &["rev-parse", &format!("refs/dagq/runs/{}", second.id())],
    );
    assert_ne!(history, second.result_commit().cloned().unwrap());
    assert_eq!(
        git_out(&repo, &["rev-parse", &format!("{history}^")]),
        first_landed
    );
    let rebased = queue
        .show(second.task_id())
        .unwrap()
        .events
        .into_iter()
        .find(|e| e.kind == "integration_rebased")
        .unwrap();
    assert_eq!(rebased.payload["main"], json!(first_landed));
    assert_eq!(
        rebased.payload["head_before"],
        json!(second.result_commit())
    );
    assert_eq!(rebased.payload["head_after"], json!(history));
    for task in [a, b] {
        assert_eq!(
            queue.show(task).unwrap().task.status(),
            TaskStatus::Completed
        );
    }
    assert!(queue.run_leases().unwrap().is_empty());
}

/// The rebase applies cleanly but the earlier landing broke this run's
/// verification (a semantic conflict): the run is parked with the rebased
/// tree in place so a session can fix it on top of main.
#[test]
fn verification_failure_after_rebase_needs_a_session_and_keeps_the_rebased_tree() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(TaskId::new(1), TaskAction::Draft).unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let breaker = queue
        .add(NewTask {
            title: "drop seed".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            kind: None,
            dependencies: vec![],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap()
        .id();
    queue.transition(breaker, TaskAction::BypassReview).unwrap();
    backend.script_for(
        breaker.as_i64(),
        "git rm -q seed.txt && git commit -q -m 'drop seed'; receipt \"$(git rev-parse HEAD)\"",
    );
    let victim = add_file_task(
        &mut queue,
        &backend,
        "needs seed",
        "v.txt",
        "v",
        &["test -f seed.txt"],
    );
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(
        integrate(&db, breaker.as_i64(), &repo).unwrap()["outcome"],
        "integrated"
    );
    let main = git_out(&repo, &["rev-parse", "main"]);
    assert!(!repo.join("seed.txt").exists());

    let run = queue.show(victim).unwrap().runs[0].clone();
    let outcome = integrate(&db, victim.as_i64(), &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let reason = outcome["reason"].as_str().unwrap();
    assert!(
        reason.contains("\"test -f seed.txt\" exited with 1 after the rebase"),
        "{reason}"
    );
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let head = git_out(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(head, run.result_commit().cloned().unwrap());
    assert_eq!(git_out(&worktree, &["rev-parse", "HEAD^"]), main);
    assert_eq!(git_out(&worktree, &["status", "--porcelain"]), "");
    assert!(
        Path::new(run.run_dir().unwrap())
            .join("integrate-1-verify-1.log")
            .exists()
    );
    let parked = queue.show(victim).unwrap().runs[0].clone();
    assert_eq!(parked.status(), RunStatus::NeedsSession);
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main);
    // The receipt was read and recorded before the verification failed.
    let detail = queue.show(victim).unwrap();
    let recorded = integration_receipts(&detail);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(
        recorded[0]["commit"],
        json!(run.result_commit().cloned().unwrap())
    );
    assert_eq!(recorded[0]["main"], json!(main));
    assert_eq!(recorded[0]["receipt"]["run_id"], json!(run.id()));
    assert_eq!(recorded[0]["receipt"]["result"], "succeeded");
    assert_eq!(
        recorded[0]["receipt"]["commit"],
        json!(run.result_commit().cloned().unwrap())
    );
    let kinds = event_kinds(&detail);
    let receipt_at = kinds
        .iter()
        .position(|k| *k == "integration_receipt")
        .unwrap();
    let deferred_at = kinds
        .iter()
        .rposition(|k| *k == "integration_deferred")
        .unwrap();
    assert!(receipt_at < deferred_at, "{kinds:?}");
    // The session restores what the verification needs and reports the new head.
    fs::write(worktree.join("seed.txt"), "restored\n").unwrap();
    git(&worktree, &["add", "seed.txt"]);
    git(&worktree, &["commit", "-q", "-m", "restore seed"]);
    write_receipt(
        &parked,
        &git_out(&worktree, &["rev-parse", "HEAD"]),
        "succeeded",
        "restored seed",
    );
    assert_eq!(
        integrate(&db, victim.as_i64(), &repo).unwrap()["outcome"],
        "integrated"
    );
    let landed = queue.show(victim).unwrap().runs[0].clone();
    assert_landed(&repo, &landed, "needs seed", &main);
    assert!(repo.join("seed.txt").exists() && repo.join("v.txt").exists());
    let detail = queue.show(victim).unwrap();
    let recorded = integration_receipts(&detail);
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    assert_eq!(
        recorded[1]["commit"],
        json!(git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", run.id())]
        ))
    );
}

/// One run lands at a time. An `integrate` process that dies leaves the run
/// `integrating` with a stale lease; `recover` puts it back in the queue
/// instead of interrupting it, and the next `integrate` lands it. A landing
/// that cannot fast-forward the main checkout gives the slot back too.
#[test]
fn integration_slot_is_exclusive_and_an_abandoned_landing_is_recoverable() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let other = add_file_task(
        &mut queue,
        &backend,
        "other",
        "o.txt",
        "o",
        &["test -f seed.txt"],
    );
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let seed = git_out(&repo, &["rev-parse", "main"]);

    // Take the slot by hand, as a crashed `integrate` would have.
    let taken = queue
        .begin_integration(run.id(), "crashed", &sha(&seed))
        .unwrap();
    assert_eq!(taken.status(), RunStatus::Integrating);
    assert!(
        queue
            .transition(TaskId::new(1), TaskAction::BypassReview)
            .is_err()
    );
    let error = format!("{:#}", integrate(&db, other.as_i64(), &repo).unwrap_err());
    assert!(
        error.contains(&format!("run {} is integrating", run.id())),
        "{error}"
    );
    let error = format!("{:#}", integrate(&db, 1, &repo).unwrap_err());
    assert!(error.contains("is already integrating"), "{error}");
    assert!(
        queue
            .begin_integration(run.id(), "again", &sha(&seed))
            .is_err()
    );
    assert_eq!(
        queue.show(other).unwrap().runs[0].status(),
        RunStatus::AwaitingIntegration
    );
    // It is visible while alive, and recoverable once its process is gone.
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["runs"][0]["run_id"], json!(run.id()));
    assert_eq!(report["runs"][0]["status"], "integrating");
    assert_eq!(report["runs"][0]["recoverable"], false);
    assert!(runtime::recover(&db, run.id()).is_err());
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE run_leases SET heartbeat_at=0, pid=?1", [dead_pid()])
        .unwrap();
    assert_eq!(
        runtime::doctor(&db, true).unwrap()["runs"][0]["recoverable"],
        true
    );
    let recovered = runtime::recover(&db, run.id()).unwrap();
    assert_eq!(recovered["run"]["status"], "awaiting_integration");
    let event = queue
        .show(TaskId::new(1))
        .unwrap()
        .events
        .into_iter()
        .find(|e| e.kind == "run_recovered")
        .unwrap();
    assert_eq!(event.payload["previous_status"], "integrating");
    assert_eq!(event.payload["status"], "awaiting_integration");
    assert!(queue.run_leases().unwrap().is_empty());

    // A local change in the main checkout that collides with the landing
    // makes the fast-forward fail; the run goes back to the queue.
    fs::write(repo.join("change.txt"), "uncommitted local edit\n").unwrap();
    let error = format!("{:#}", integrate(&db, 1, &repo).unwrap_err());
    assert!(error.contains("fast-forward main"), "{error}");
    assert!(
        error.contains("returned to awaiting_integration"),
        "{error}"
    );
    let returned = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(returned.status(), RunStatus::AwaitingIntegration);
    assert!(returned.last_error().unwrap().contains("before main moved"));
    assert!(event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"integration_error"));
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), seed);
    assert!(queue.run_leases().unwrap().is_empty());
    fs::remove_file(repo.join("change.txt")).unwrap();
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let landed = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_landed(&repo, &landed, "test task", &seed);
    assert_eq!(
        integrate(&db, other.as_i64(), &repo).unwrap()["outcome"],
        "integrated"
    );
    assert_eq!(
        git_out(&repo, &["rev-list", "--count", &format!("{seed}..main")]),
        "2"
    );
}

/// The repository moves after a run was validated: every check against the
/// old binding fails until `rebind`, which is refused while a supervisor or
/// an `integrate` lives, and afterwards the queue lists, reports and lands
/// the awaiting run from the new checkout (ADR-0020).
#[test]
fn rebind_follows_a_moved_repository_and_the_awaiting_run_lands() {
    use dagq::infrastructure::adapters::{GitRepository, path_text};
    let (dir, repo, db, run) = awaiting_run();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    let old_common_dir = path_text(&GitRepository::inspect(&repo).unwrap().common_dir).unwrap();
    let moved = dir.path().join("moved repo");
    fs::rename(&repo, &moved).unwrap();
    let new_common_dir = path_text(&GitRepository::inspect(&moved).unwrap().common_dir).unwrap();
    let worktree = PathBuf::from(run.worktree_path().unwrap().to_owned());
    // The run worktree's `.git` file still points into the old repository.
    assert!(
        !Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .arg("status")
            .bounded_output()
            .unwrap()
            .status
            .success()
    );

    // Nothing rebinds implicitly.
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.assert_repository(&new_common_dir).is_err());
    assert!(queue.bind_repository(&new_common_dir).is_err());
    let refused = integrate(&db, 1, &moved).unwrap_err().to_string();
    assert!(refused.contains("the queue is bound to"), "{refused}");

    // A live supervisor, even one of another binary, blocks the rebind.
    queue
        .register_supervisor("live", std::process::id(), 1, "0.0.1")
        .unwrap();
    let refused = runtime::rebind(&db, &moved).unwrap_err().to_string();
    assert!(refused.contains("supervisor is running"), "{refused}");
    assert_eq!(
        queue.repository_binding().unwrap().as_deref(),
        Some(old_common_dir.as_str())
    );
    // A registration left behind by a dead one does not.
    assert!(queue.deregister_supervisor("live").unwrap());
    queue
        .register_supervisor("dead", dead_pid(), 1, VERSION)
        .unwrap();

    let rebound = runtime::rebind(&db, &moved).unwrap();
    assert_eq!(rebound["outcome"], "rebound", "{rebound}");
    assert_eq!(rebound["previous_git_common_dir"], json!(old_common_dir));
    assert_eq!(rebound["git_common_dir"], json!(new_common_dir));
    assert_eq!(
        rebound["worktrees"],
        json!([{"run_id": run.id(), "worktree_path": worktree, "repaired": true, "error": null}]),
        "{rebound}"
    );
    assert_eq!(
        queue.repository_binding().unwrap().as_deref(),
        Some(new_common_dir.as_str())
    );
    queue.assert_repository(&new_common_dir).unwrap();
    assert!(queue.assert_repository(&old_common_dir).is_err());
    // The change is recorded next to the supervisor logs.
    let log = fs::read_to_string(
        db.canonicalize()
            .unwrap()
            .with_file_name("logs")
            .join(runtime::REBIND_LOG),
    )
    .unwrap();
    let entry: Value = serde_json::from_str(log.trim()).unwrap();
    assert_eq!(entry["previous_git_common_dir"], json!(old_common_dir));
    assert_eq!(entry["git_common_dir"], json!(new_common_dir));
    // The worktree works again, so a resumed session could use it.
    git(&worktree, &["status"]);
    // Rebinding to the same repository changes nothing and logs nothing.
    let again = runtime::rebind(&db, &moved).unwrap();
    assert_eq!(again["outcome"], "unchanged", "{again}");
    assert_eq!(
        fs::read_to_string(
            db.canonicalize()
                .unwrap()
                .with_file_name("logs")
                .join(runtime::REBIND_LOG),
        )
        .unwrap(),
        log
    );

    let listed = queue
        .list(&dagq::application::TaskQuery {
            status: dagq::application::StatusFilter::Any,
            goal_id: None,
            limit: 20,
            before: None,
            full: false,
        })
        .unwrap();
    assert_eq!(serde_json::to_value(listed).unwrap()["total"], 2);
    runtime::status(&db).unwrap();
    let outcome = integrate(&db, 1, &moved).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let landed = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_landed(&moved, &landed, "test task", &seed);
}

/// An `integrate` in progress holds the old repository's paths as well.
#[test]
fn rebind_is_refused_while_a_run_is_integrating() {
    let (dir, repo, db, run) = awaiting_run();
    let main = git_out(&repo, &["rev-parse", "main"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .begin_integration(run.id(), "integrator", &sha(&main))
        .unwrap();
    let other = dir.path().join("other");
    fs::create_dir(&other).unwrap();
    git(&other, &["init", "-q", "-b", "main"]);
    git(
        &other,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "seed",
        ],
    );
    let refused = runtime::rebind(&db, &other).unwrap_err().to_string();
    assert!(refused.contains("is integrating"), "{refused}");
}

#[test]
fn integrate_registers_the_landed_follow_ups_as_draft_tasks_of_the_goal_once() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let goal = queue
        .add_goal(NewGoal {
            title: "goal".into(),
            description: String::new(),
            acceptance: "done".into(),
            constraints: String::new(),
            doc: None,
            draft: false,
        })
        .unwrap();
    queue.set_goal(TaskId::new(1), Some(goal.id())).unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let head = run.result_commit().cloned().unwrap();
    let follow_ups = json!([
        {"title": "later work", "description": "outside the task"},
        {"title": "  ", "description": "no title, not a task"},
        {"title": "more work", "description": ""},
        {"title": "no description"}
    ]);

    // A receipt that does not name the head parks the run: nothing landed,
    // so nothing is registered.
    let mut stale = session_receipt(&run, run.base_commit().as_str(), "succeeded", "stale");
    stale["follow_ups"] = follow_ups.clone();
    write_receipt_json(&run, stale);
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    assert!(events_of(&db, run.id(), "follow_up_registered").is_empty());
    assert_eq!(
        queue.list(&Default::default()).unwrap().total,
        1,
        "only the task itself"
    );

    // The landing registers each titled follow-up as a draft task of the goal.
    let mut receipt = session_receipt(&run, head.as_str(), "succeeded", "landed");
    receipt["follow_ups"] = follow_ups.clone();
    write_receipt_json(&run, receipt);
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(
        outcome["follow_ups"],
        json!([
            {"task_id": 2, "title": "later work"},
            {"task_id": 3, "title": "more work"}
        ])
    );
    let context = format!(
        "task 1（test task）の run {} の receipt が提案した follow_up",
        run.id()
    );
    for (id, title, description) in [(2, "later work", "outside the task"), (3, "more work", "")] {
        let detail = queue.show(TaskId::new(id)).unwrap();
        assert_eq!(detail.task.status(), TaskStatus::Draft);
        assert_eq!(detail.task.title(), title);
        assert_eq!(detail.task.description(), description);
        assert_eq!(detail.task.goal_id(), Some(goal.id()));
        assert_eq!(detail.task.context(), context);
        assert_eq!(detail.task.acceptance(), "");
        assert!(detail.task.verification_commands().is_empty());
        assert!(detail.dependencies.is_empty());
    }
    assert_eq!(
        events_of(&db, run.id(), "follow_up_registered"),
        vec![
            json!({"task_id": 2, "title": "later work", "index": 0}),
            json!({
                "task_id": null, "title": "  ", "index": 1,
                "skipped": "title is not a non-blank string",
                "follow_up": {"title": "  ", "description": "no title, not a task"},
            }),
            json!({"task_id": 3, "title": "more work", "index": 2}),
            json!({
                "task_id": null, "title": "no description", "index": 3,
                "skipped": "description is not a string",
                "follow_up": {"title": "no description"},
            }),
        ]
    );
    // Drafts are not picked up by the supervisor.
    assert!(queue.candidates().unwrap().is_empty());

    // The run is integrated: another integrate finds nothing to land, and
    // registering the same run's follow-ups again adds nothing.
    assert!(integrate(&db, 1, &repo).is_err());
    let task = queue.show(TaskId::new(1)).unwrap().task;
    assert!(
        runtime::register_follow_ups(&mut queue, &task, run.id(), Some(&follow_ups)).is_empty()
    );
    assert_eq!(queue.list(&Default::default()).unwrap().total, 2);
    assert_eq!(events_of(&db, run.id(), "follow_up_registered").len(), 4);

    // A closed goal takes no task: a new follow-up is registered without it.
    queue
        .close_goal(goal.id(), dagq::domain::GoalVerdict::Abandoned)
        .unwrap();
    let mut extended = follow_ups.as_array().unwrap().clone();
    extended.push(json!({"title": "after the goal", "description": "d"}));
    let added = runtime::register_follow_ups(&mut queue, &task, run.id(), Some(&json!(extended)));
    assert_eq!(added.len(), 1);
    let detail = queue.show(added[0].task_id).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Draft);
    assert_eq!(detail.task.goal_id(), None);
    assert_eq!(
        events_of(&db, run.id(), "follow_up_registered")[4],
        json!({"task_id": added[0].task_id, "title": "after the goal", "index": 4, "goal_closed": true})
    );
    // Nothing to register without follow_ups.
    assert!(runtime::register_follow_ups(&mut queue, &task, run.id(), None).is_empty());
}

/// Validation does not run the verification commands, so a commit that
/// breaks them is accepted there and parked as `needs_session` by
/// integrate, whose run of the commands is the only one (ADR-0023).
#[test]
fn failing_verification_command_passes_validation_and_needs_a_session_at_integrate() {
    let (_dir, db, detail) = run_agent(
        "git rm -q seed.txt && git commit -q -m 'drop seed'; receipt \"$(git rev-parse HEAD)\"",
    );
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(!event_kinds(&detail).contains(&"verification_command"));
    let repo = Path::new(&db).parent().unwrap().join("repo's directory");
    let main = git_out(&repo, &["rev-parse", "main"]);
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let reason = outcome["reason"].as_str().unwrap();
    assert!(
        reason.contains("verification command \"test -f seed.txt\" exited with 1"),
        "{reason}"
    );
    assert!(reason.contains("integrate-1-verify-1.log"), "{reason}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    // The failure is classified, with the command's index and exit code (ADR-0034).
    let deferred = payloads(&detail, "integration_deferred");
    assert_eq!(deferred[0]["code"], "verification_failed");
    assert_eq!(deferred[0]["index"], 1);
    assert_eq!(deferred[0]["exit_code"], 1);
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 1, "{verifications:?}");
    assert_eq!(verifications[0]["exit_code"], 1);
    assert_eq!(verifications[0]["attempt"], 1);
    // Nothing landed.
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main);

    // A second integrate of the same run keeps the first attempt's log: each
    // attempt writes its own `integrate-<attempt>-verify-N.log`.
    let run = &detail.runs[0];
    let run_dir = Path::new(run.run_dir().unwrap());
    let first = run_dir.join("integrate-1-verify-1.log");
    fs::write(&first, "the first attempt's output\n").unwrap();
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    assert!(
        outcome["reason"]
            .as_str()
            .unwrap()
            .contains("integrate-2-verify-1.log"),
        "{outcome}"
    );
    let second = run_dir.join("integrate-2-verify-1.log");
    assert!(second.exists());
    assert_eq!(
        fs::read_to_string(&first).unwrap(),
        "the first attempt's output\n"
    );
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 2, "{verifications:?}");
    assert_eq!(verifications[1]["attempt"], 2);
    assert_eq!(
        verifications[1]["log_path"],
        json!(second.to_str().unwrap())
    );

    // The triage reads the latest attempt's log and names the earlier one.
    let run = &detail.runs[0];
    let prompt = runtime::triage_prompt(&detail, run, 0, run_dir).unwrap();
    assert!(
        prompt.contains(&format!("Verification log {} (end)", second.display())),
        "{prompt}"
    );
    assert!(
        !prompt.contains(&format!("Verification log {} (end)", first.display())),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!(
            "Logs of earlier integrate attempts (not shown): {}",
            first.display()
        )),
        "{prompt}"
    );
    // review.md names the latest attempt's logs.
    let head = run.result_commit().cloned().unwrap();
    write_receipt_json(run, session_receipt(run, head.as_str(), "succeeded", "s"));
    runtime::review(&db, TaskId::new(1)).unwrap();
    let review = fs::read_to_string(run_dir.join("review.md")).unwrap();
    assert!(
        review.contains(&format!("latest attempt: {}", second.display())),
        "{review}"
    );
}

/// Integrate's verification logs are numbered per attempt; a run directory
/// with the name used before (`integrate-verify-N.log`) is still read, as
/// the attempt before the numbered ones.
#[test]
fn integrate_logs_are_kept_per_attempt_and_old_names_are_read() {
    let dir = TempDir::new().unwrap();
    let run_dir = dir.path();
    assert_eq!(runtime::next_integrate_attempt(run_dir), 1);
    assert_eq!(runtime::integrate_logs(run_dir), (vec![], vec![]));
    assert_eq!(
        runtime::review_logs_hint(Some(run_dir.to_str().unwrap())),
        format!(
            "{}/integrate-<attempt>-verify-N.log (one set per integrate attempt, written when integrate runs the verification commands after its rebase); none yet",
            run_dir.display()
        )
    );
    assert_eq!(runtime::review_logs_hint(None), "(no run directory)");
    for name in [
        "integrate-verify-1.log",
        "integrate-verify-2.log",
        "verify-1.log",
        "integrate-x-verify-1.log",
        "integrate-verify-y.log",
        "notes.txt",
    ] {
        fs::write(run_dir.join(name), name).unwrap();
    }
    assert_eq!(
        runtime::integrate_logs(run_dir),
        (
            vec![
                run_dir.join("integrate-verify-1.log"),
                run_dir.join("integrate-verify-2.log")
            ],
            vec![]
        )
    );
    assert_eq!(runtime::next_integrate_attempt(run_dir), 1);
    assert_eq!(
        runtime::integrate_verify_log(run_dir, 1, 2),
        run_dir.join("integrate-1-verify-2.log")
    );
    for name in [
        "integrate-1-verify-1.log",
        "integrate-10-verify-2.log",
        "integrate-10-verify-10.log",
        "integrate-2-verify-1.log",
    ] {
        fs::write(run_dir.join(name), name).unwrap();
    }
    let (latest, earlier) = runtime::integrate_logs(run_dir);
    assert_eq!(
        latest,
        vec![
            run_dir.join("integrate-10-verify-2.log"),
            run_dir.join("integrate-10-verify-10.log")
        ]
    );
    assert_eq!(
        earlier,
        vec![
            run_dir.join("integrate-verify-1.log"),
            run_dir.join("integrate-verify-2.log"),
            run_dir.join("integrate-1-verify-1.log"),
            run_dir.join("integrate-2-verify-1.log")
        ]
    );
    assert_eq!(runtime::next_integrate_attempt(run_dir), 11);
    assert!(
        runtime::review_logs_hint(Some(run_dir.to_str().unwrap())).ends_with(&format!(
            "latest attempt: {}, {}",
            run_dir.join("integrate-10-verify-2.log").display(),
            run_dir.join("integrate-10-verify-10.log").display()
        ))
    );
    assert_eq!(runtime::next_integrate_attempt(&run_dir.join("missing")), 1);
}

/// A task that runs `script` in its worktree before committing everything
/// and writing its receipt, verified by `verify`.
fn add_script_task(
    queue: &mut SqliteQueue,
    backend: &TestWorkspace,
    title: &str,
    script: &str,
    verify: &[&str],
) -> TaskId {
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "adds a migration".into(),
            acceptance: "works".into(),
            verification_commands: verify.iter().map(|v| (*v).to_owned()).collect(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            kind: None,
            dependencies: vec![],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    backend.script_for(
        task.id().as_i64(),
        &format!(
            "{script} && git add -A && git commit -q -m '{title}'; receipt \"$(git rev-parse HEAD)\""
        ),
    );
    task.id()
}

/// Fails when two migration files share a number, as the build would.
const NO_SHARED_NUMBER: &str = "test -z \"$(ls migrations | cut -c1-4 | sort | uniq -d)\"";

/// A repository whose main has `migrations/0001_first.sql`.
fn migration_fixture() -> (Fixture, PathBuf, PathBuf) {
    let (dir, repo, db) = fixture();
    fs::create_dir(repo.join("migrations")).unwrap();
    fs::write(repo.join("migrations/0001_first.sql"), "-- first\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "first migration"]);
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Draft)
        .unwrap();
    (dir, repo, db)
}

fn migrations_on(repo: &Path, commit: &str) -> Vec<String> {
    git_out(
        repo,
        &["ls-tree", "--name-only", commit, "--", "migrations/"],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

/// Two runs add migration 0002 at once. The first lands; the second's
/// rebase applies cleanly but would leave two files of number 0002, so
/// integrate moves its migration to 0003, commits that on the run branch,
/// records `migration_renumbered` and lands it after the verification
/// (ADR-0067 decision 3).
#[test]
fn integrate_renumbers_a_migration_whose_number_main_took() {
    let (_dir, repo, db) = migration_fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let first = add_script_task(
        &mut queue,
        &backend,
        "add goals",
        "printf -- '-- goals\\n' > migrations/0002_goals.sql",
        &[NO_SHARED_NUMBER],
    );
    let second = add_script_task(
        &mut queue,
        &backend,
        "add asks",
        "printf -- '-- asks\\n' > migrations/0002_asks.sql && printf 'asks\\n' > asks.txt",
        &[NO_SHARED_NUMBER],
    );
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(
        integrate(&db, first.as_i64(), &repo).unwrap()["outcome"],
        "integrated"
    );
    let main = git_out(&repo, &["rev-parse", "main"]);

    let run = queue.show(second).unwrap().runs[0].clone();
    let outcome = integrate(&db, second.as_i64(), &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(
        migrations_on(&repo, "main"),
        [
            "migrations/0001_first.sql",
            "migrations/0002_goals.sql",
            "migrations/0003_asks.sql"
        ]
    );
    assert_eq!(
        git_out(&repo, &["show", "main:migrations/0003_asks.sql"]),
        "-- asks"
    );
    assert!(repo.join("asks.txt").exists());
    let detail = queue.show(second).unwrap();
    let renumbered = detail
        .events
        .iter()
        .find(|e| e.kind == "migration_renumbered")
        .unwrap();
    let history = git_out(
        &repo,
        &["rev-parse", &format!("refs/dagq/runs/{}", run.id())],
    );
    assert_eq!(renumbered.payload["main"], json!(main));
    assert_eq!(
        renumbered.payload["from"],
        json!("migrations/0002_asks.sql")
    );
    assert_eq!(renumbered.payload["to"], json!("migrations/0003_asks.sql"));
    assert_eq!(renumbered.payload["old_number"], json!("0002"));
    assert_eq!(renumbered.payload["new_number"], json!("0003"));
    assert_eq!(renumbered.payload["head_after"], json!(history));
    // The rename is its own commit on top of the rebased run.
    assert_eq!(
        git_out(&repo, &["rev-parse", &format!("{history}^")]),
        renumbered.payload["head_before"].as_str().unwrap()
    );
    // It comes before the verification, which saw the renumbered tree.
    let kinds = event_kinds(&detail);
    let renumbered_at = kinds
        .iter()
        .position(|k| *k == "migration_renumbered")
        .unwrap();
    let verified_at = kinds
        .iter()
        .rposition(|k| *k == "verification_command")
        .unwrap();
    assert!(renumbered_at < verified_at, "{kinds:?}");
    let landed = queue.show(second).unwrap().runs[0].clone();
    assert_landed(&repo, &landed, "add asks", &main);
}

/// A migration integrate cannot move by itself: its number is mentioned by
/// another file the run changes, or the run adds more than one migration.
/// The run waits for a session with the next free number and the reason
/// (`migration_number_taken`), main and the run's tree stay as they were,
/// and once the session renumbers it, it lands.
#[test]
fn a_migration_that_cannot_be_renumbered_mechanically_needs_a_session() {
    let (_dir, repo, db) = migration_fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let first = add_script_task(
        &mut queue,
        &backend,
        "add goals",
        "printf -- '-- goals\\n' > migrations/0002_goals.sql",
        &[NO_SHARED_NUMBER],
    );
    let referred = add_script_task(
        &mut queue,
        &backend,
        "add asks",
        "printf -- '-- asks\\n' > migrations/0002_asks.sql && printf 'migration 0002 adds asks\\n' > notes.md",
        &[NO_SHARED_NUMBER],
    );
    let two = add_script_task(
        &mut queue,
        &backend,
        "add two",
        "printf -- '-- a\\n' > migrations/0002_a.sql && printf -- '-- b\\n' > migrations/0003_b.sql",
        &[NO_SHARED_NUMBER],
    );
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(
        integrate(&db, first.as_i64(), &repo).unwrap()["outcome"],
        "integrated"
    );
    let main = git_out(&repo, &["rev-parse", "main"]);

    for (task, why) in [
        (referred, "the run's other changes mention 0002 (notes.md)"),
        (
            two,
            "the run adds 2 migrations, which are not renumbered mechanically",
        ),
    ] {
        let run = queue.show(task).unwrap().runs[0].clone();
        let outcome = integrate(&db, task.as_i64(), &repo).unwrap();
        assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
        let reason = outcome["reason"].as_str().unwrap();
        assert!(reason.contains(why), "{reason}");
        assert!(
            reason.contains("migrations/0002_") && reason.contains("the next free number is 0003"),
            "{reason}"
        );
        let detail = queue.show(task).unwrap();
        assert!(!event_kinds(&detail).contains(&"migration_renumbered"));
        let deferred = detail
            .events
            .iter()
            .rfind(|e| e.kind == "integration_deferred")
            .unwrap();
        assert_eq!(deferred.payload["code"], "migration_number_taken");
        assert_eq!(deferred.payload["next_number"], "0003");
        assert_eq!(
            queue.show(task).unwrap().runs[0].status(),
            RunStatus::NeedsSession
        );
        // The rebased run is left for the session, without a rename.
        let worktree = PathBuf::from(run.worktree_path().unwrap());
        assert_eq!(git_out(&worktree, &["rev-parse", "HEAD^"]), main);
        assert_eq!(git_out(&worktree, &["status", "--porcelain"]), "");
        assert_eq!(git_out(&repo, &["rev-parse", "main"]), main);
    }
    let deferred = queue
        .show(referred)
        .unwrap()
        .events
        .into_iter()
        .rfind(|e| e.kind == "integration_deferred")
        .unwrap();
    assert_eq!(deferred.payload["referring"], json!(["notes.md"]));

    // The session renumbers the migration and what mentions it.
    let parked = queue.show(referred).unwrap().runs[0].clone();
    let worktree = PathBuf::from(parked.worktree_path().unwrap());
    git(
        &worktree,
        &["mv", "migrations/0002_asks.sql", "migrations/0003_asks.sql"],
    );
    fs::write(worktree.join("notes.md"), "migration 0003 adds asks\n").unwrap();
    git(&worktree, &["commit", "-q", "-a", "-m", "renumber"]);
    write_receipt(
        &parked,
        &git_out(&worktree, &["rev-parse", "HEAD"]),
        "succeeded",
        "renumbered to 0003",
    );
    let outcome = integrate(&db, referred.as_i64(), &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(
        migrations_on(&repo, "main"),
        [
            "migrations/0001_first.sql",
            "migrations/0002_goals.sql",
            "migrations/0003_asks.sql"
        ]
    );
}

/// A run that renames a migration main already had keeps its number: the
/// collision is judged on the rebased tree, where no other file has it.
#[test]
fn a_renamed_migration_is_not_renumbered() {
    let (_dir, repo, db) = migration_fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let first = add_script_task(
        &mut queue,
        &backend,
        "add goals",
        "printf -- '-- goals\\n' > migrations/0002_goals.sql",
        &[NO_SHARED_NUMBER],
    );
    let renamer = add_script_task(
        &mut queue,
        &backend,
        "rename first",
        "git mv migrations/0001_first.sql migrations/0001_initial.sql",
        &[NO_SHARED_NUMBER],
    );
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    for task in [first, renamer] {
        let outcome = integrate(&db, task.as_i64(), &repo).unwrap();
        assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    }
    assert!(!event_kinds(&queue.show(renamer).unwrap()).contains(&"migration_renumbered"));
    assert_eq!(
        migrations_on(&repo, "main"),
        ["migrations/0001_initial.sql", "migrations/0002_goals.sql"]
    );
}

/// The load average an interval's end recorded: a mean no higher than
/// the maximum.
fn assert_load(payload: &Value) {
    let mean = payload["load_avg_mean"].as_f64();
    let max = payload["load_avg_max"].as_f64();
    assert!(
        mean.zip(max).is_some_and(|(mean, max)| mean <= max),
        "{payload}"
    );
}
