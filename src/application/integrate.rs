//! Landing a validated run on `main` (ADR-0016, ADR-0019, ADR-0023): the
//! integration slot, the rebase onto the current `main`, the re-validation
//! and the verification commands, the squash commit, the push and the
//! follow-ups; and the receipt check the supervisor's validation shares.
//! The queue, Git, the verification commands, the push, time, IDs and
//! process liveness come in through the ports.

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use super::{
    Clock, IdGenerator, Landing, MainRemote, ProcessControl, Queue, Repository, RunFiles, RunStore,
    Verifier, path_text, reason_of_error, tail,
};
use crate::domain::{
    CommitSha, EvidenceCheck, IntegrationOutcome, MAX_RESUME_ATTEMPTS, NewTask, PUSH_REMOTE,
    PushReport, PushResult, Reason, ReasonCode, Receipt, ReceiptResult, RegisteredFollowUp, RunId,
    RunStatus, Task, TaskId, TaskRun, evidence_missing_reason, heartbeat_stale,
    scope::{out_of_scope, scope_violation_reason},
};

/// Why validation did not accept a run's receipt, with what it could
/// verify on the way.
pub struct Rejection {
    pub reason: String,
    /// The code of `reason` (ADR-0034).
    pub code: ReasonCode,
    pub commit: Option<CommitSha>,
    pub receipt: Option<Receipt>,
    /// The task's required checks the receipt does not back, when that is
    /// all that is wrong: the run waits for a session instead of failing.
    pub evidence_missing: Vec<EvidenceCheck>,
    /// The changed paths outside the task's `paths` (ADR-0029), when the
    /// run is otherwise sound: it waits for a session to take them out.
    pub scope_violation: Vec<String>,
}

/// Cross-check the agent's receipt against Git: the receipt names the
/// clean head of the run branch, new work on top of the base commit, within
/// the task's paths and with the task's required evidence. `Ok(Err(_))` is
/// a verdict on the run; `Err` a failure of the checks themselves.
pub fn check_receipt(
    repository: &dyn Repository,
    files: &dyn RunFiles,
    task: &Task,
    run: &TaskRun,
) -> Result<std::result::Result<(Receipt, CommitSha), Rejection>> {
    let reject =
        |code: ReasonCode, reason: String, commit: Option<CommitSha>, receipt: Option<Receipt>| {
            Ok(Err(Rejection {
                reason,
                code,
                commit,
                receipt,
                evidence_missing: Vec::new(),
                scope_violation: Vec::new(),
            }))
        };
    let receipt_path = Path::new(run.receipt_path().context("missing receipt path")?);
    let text = match files.read_to_string(receipt_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return reject(
                ReasonCode::ReceiptMissing,
                format!("receipt was not submitted at {}", receipt_path.display()),
                None,
                None,
            );
        }
        Err(error) => return Err(error).context("read receipt"),
    };
    let receipt = match Receipt::parse(&text) {
        Ok(receipt) => receipt,
        Err(error) => {
            return reject(
                ReasonCode::of_receipt_error(&error),
                format!("{error:#}"),
                None,
                None,
            );
        }
    };
    if let Err(error) = receipt.check_requiring(run.id(), task.required_evidence()) {
        return reject(
            ReasonCode::of_receipt_error(&error),
            format!("{error:#}"),
            None,
            Some(receipt),
        );
    }
    // The commit must be the head of the run branch, checked out in the worktree,
    // and new work on top of the base commit.
    let worktree = Path::new(run.worktree_path().context("missing worktree")?);
    let branch = run.branch().context("missing branch")?;
    let expected_ref = format!("refs/heads/{branch}");
    match repository.current_branch(worktree)? {
        Some(current) if current == expected_ref => (),
        current => {
            return reject(
                ReasonCode::CommitMismatch,
                format!(
                    "worktree is on {} instead of {expected_ref}",
                    current.as_deref().unwrap_or("a detached HEAD")
                ),
                None,
                Some(receipt),
            );
        }
    }
    let head = repository.head(worktree)?;
    if head.as_str() != receipt.commit.to_ascii_lowercase() {
        return reject(
            ReasonCode::CommitMismatch,
            format!(
                "receipt commit {} is not the head of {branch} ({head})",
                receipt.commit
            ),
            None,
            Some(receipt),
        );
    }
    let commit = head;
    if commit == *run.base_commit() {
        return reject(
            ReasonCode::CommitMismatch,
            format!("no commit was made on top of base {}", run.base_commit()),
            Some(commit),
            Some(receipt),
        );
    }
    if !repository.is_ancestor(run.base_commit().as_str(), commit.as_str())? {
        return reject(
            ReasonCode::CommitMismatch,
            format!(
                "commit {commit} does not descend from base {}",
                run.base_commit()
            ),
            Some(commit),
            Some(receipt),
        );
    }
    let status = repository.status(worktree)?;
    if !status.trim().is_empty() {
        return reject(
            ReasonCode::WorktreeDirty,
            format!("worktree is not clean:\n{}", status.trim_end()),
            Some(commit),
            Some(receipt),
        );
    }
    // Checked last, like the evidence below: only a run that is otherwise
    // sound waits for a session to take out what it changed outside the
    // task's paths (ADR-0029). The diff starts where the branch forked from
    // the current main, not at the base commit: a resumed session that
    // rebased carries what other tasks landed since, which is not its change.
    let outside = if task.paths().is_empty() {
        Vec::new()
    } else {
        let fork = repository
            .merge_base(repository.main_head()?.as_str(), commit.as_str())?
            .context("the run branch shares no history with main")?;
        out_of_scope(
            task.paths(),
            &repository.changed_paths(fork.as_str(), commit.as_str())?,
        )
    };
    if !outside.is_empty() {
        return Ok(Err(Rejection {
            reason: scope_violation_reason(&outside),
            code: ReasonCode::ScopeViolation,
            commit: Some(commit),
            receipt: Some(receipt),
            evidence_missing: Vec::new(),
            scope_violation: outside,
        }));
    }
    // Only a run that is otherwise sound waits for a session to add the
    // evidence (ADR-0019 decision 5).
    let missing = receipt.missing_evidence(task.required_evidence());
    if !missing.is_empty() {
        return Ok(Err(Rejection {
            reason: evidence_missing_reason(&missing),
            code: ReasonCode::EvidenceMissing,
            commit: Some(commit),
            receipt: Some(receipt),
            evidence_missing: missing,
            scope_violation: Vec::new(),
        }));
    }
    Ok(Ok((receipt, commit)))
}

/// Which run `integrate` lands.
#[derive(Debug, Clone, Copy)]
pub enum IntegrateTarget {
    /// The task's run that awaits integration or comes back from a session.
    Task(TaskId),
    /// The oldest run awaiting integration by validation time (FIFO).
    Next,
}

/// What `integrate` needs besides the run: the queue, the repository the
/// queue is bound to (`common_dir` is its Git common directory as text),
/// how the verification commands run, the remote `main` is pushed to
/// (`None` is `--no-push`), the run files (the worktree, the receipt and
/// the verification logs), the time and IDs, and process liveness for the
/// lease check.
pub struct Integration<'a> {
    pub queue: &'a mut dyn Queue,
    pub repository: &'a dyn Repository,
    pub verifier: &'a dyn Verifier,
    pub remote: Option<&'a dyn MainRemote>,
    pub files: &'a dyn RunFiles,
    pub common_dir: &'a str,
    pub clock: &'a dyn Clock,
    pub ids: &'a dyn IdGenerator,
    pub processes: &'a dyn ProcessControl,
    /// This process, recorded with the approval to land.
    pub pid: u32,
}

/// A run that holds the integration slot under `token`: `previous` is the
/// status it returns to when the landing stops before `main` moves, and
/// `main` the head it lands on.
pub struct Begun {
    pub run: TaskRun,
    pub previous: RunStatus,
    pub main: CommitSha,
    pub token: String,
}

/// The first half of `integrate`: pick the run of `target`, record the
/// call as the approval to land it and take the integration slot. `repo`
/// is the checkout the caller named, for the error when it belongs to
/// another repository than the queue's. `None` means no run awaits
/// integration. The caller keeps the lease alive while
/// [`land_integrating`] lands the run.
pub fn begin(
    ctx: &mut Integration<'_>,
    target: IntegrateTarget,
    repo: &Path,
) -> Result<Option<Begun>> {
    let queue = &mut *ctx.queue;
    let common_dir = ctx.common_dir;
    let bound = queue
        .repository_binding()?
        .context("queue is not bound to a repository; no run was supervised")?;
    ensure!(
        bound == common_dir,
        "{} belongs to {common_dir}, but the queue is bound to {bound}",
        repo.display()
    );
    let run = match target {
        IntegrateTarget::Task(task_id) => {
            let detail = queue.show(task_id)?;
            if let Some(busy) = detail
                .runs
                .iter()
                .find(|r| r.status() == RunStatus::Integrating)
            {
                bail!(
                    "run {} of task {task_id} is already integrating (see doctor if it is stuck)",
                    busy.id()
                );
            }
            detail
                .runs
                .iter()
                .find(|r| {
                    matches!(
                        r.status(),
                        RunStatus::AwaitingIntegration | RunStatus::NeedsSession
                    )
                })
                .cloned()
                .with_context(|| {
                    format!(
                        "task {task_id} ({}) has no run awaiting integration or a session",
                        detail.task.status().as_str()
                    )
                })?
        }
        IntegrateTarget::Next => match queue.next_awaiting_integration()? {
            Some(run) => run,
            None => return Ok(None),
        },
    };
    // A run the supervisor holds (its review, ADR-0027, or its resume) is
    // not approved by a call that cannot land it.
    if let Some(lease) = queue.run_lease(run.id())?
        && !heartbeat_stale(
            ctx.processes.alive(lease.pid),
            ctx.clock.now() - lease.heartbeat_at,
        )
    {
        bail!(
            "run {} is held by the supervisor (its review or resume is in progress); see show for its review_finished / resume_finished events",
            run.id()
        );
    }
    // The call is the approval to land (ADR-0016 decision 5): a run it
    // parks as `needs_session` is landed by the supervisor once a resumed
    // session resolved it (ADR-0019 decision 1).
    if !queue.has_run_event(run.id(), "integration_approved")? {
        queue.record_runtime_event(
            run.id(),
            "integration_approved",
            json!({"status": run.status().as_str(), "pid": ctx.pid, "push": ctx.remote.is_some()}),
        )?;
    }
    let previous = run.status();
    let token = ctx.ids.uuid();
    let main = ctx.repository.main_head()?;
    let run = queue.begin_integration(run.id(), &token, &main)?;
    Ok(Some(Begun {
        run,
        previous,
        main,
        token,
    }))
}

/// Land a run that holds the integration slot under `token` (see
/// [`begin`]) and record the outcome; shared by `integrate` and by the
/// supervisor landing an approved run it resumed. An error before `main`
/// moved gives the slot back and returns the run to `previous`.
pub fn land_integrating(
    ctx: &mut Integration<'_>,
    run: &TaskRun,
    previous: RunStatus,
    main: &CommitSha,
    token: &str,
) -> Result<IntegrationOutcome> {
    let queue = &mut *ctx.queue;
    let repository = ctx.repository;
    let task = queue.show(run.task_id())?.task;
    // Every event of the landing, the push and the follow-ups carries the run.
    let _span =
        tracing::info_span!("integrate", run_id = %run.id(), task_id = %run.task_id()).entered();
    info!(
        op = "integrate",
        main = %main,
        "run {} integrating task {} onto main {main}",
        run.id(),
        run.task_id()
    );
    let verdict = match land(queue, repository, ctx.verifier, ctx.files, &task, run, main) {
        Ok(verdict) => verdict,
        Err(error) => {
            // Nothing reached main: give the slot back and keep the run where it was.
            let message = format!("integration stopped before main moved: {error:#}");
            if let Err(record) = queue.abort_integration(
                run.id(),
                token,
                previous.as_str(),
                &message,
                &reason_of_error(&error, ReasonCode::Other),
            ) {
                warn!(
                    op = "integrate",
                    error = %format_args!("{record:#}"),
                    "run {}: could not record the error: {record:#}",
                    run.id()
                );
            }
            return Err(error.context(format!(
                "run {} returned to {}",
                run.id(),
                previous.as_str()
            )));
        }
    };
    Ok(match verdict {
        Verdict::Landed(landing, proposed) => {
            let verification_skipped = landing.verification_skipped;
            let (task, run) = queue
                .finish_integration(run.id(), token, &landing, ctx.common_dir)
                .with_context(|| {
                    format!(
                        "main advanced to {} but run {} could not be completed; inspect show and doctor",
                        landing.commit, run.id()
                    )
                })?;
            info!(
                op = "integrate",
                commit = %landing.commit,
                "task {} landed as {} on main; run {} integrated",
                task.id(),
                landing.commit,
                run.id()
            );
            remove_landed_worktree(queue, repository, &run);
            let push = push_main(queue, ctx.remote, run.id(), &landing.commit);
            let follow_ups = register_follow_ups(queue, &task, run.id(), proposed.as_ref());
            IntegrationOutcome::Integrated {
                task: Box::new(task),
                run: Box::new(run),
                verification_skipped,
                push: Box::new(push),
                follow_ups,
            }
        }
        Verdict::Deferred { reason, mut detail } => {
            warn!(
                op = "integrate",
                reason = %reason,
                "run {} needs a session: {reason}",
                run.id()
            );
            // How many more times the supervisor resumes it (ADR-0019); the
            // event is a person's only once none are left (as an ask).
            detail["resumes_left"] =
                json!(MAX_RESUME_ATTEMPTS.saturating_sub(resume_attempts(queue, run.id())));
            let run = queue.defer_integration(run.id(), token, &reason, detail)?;
            IntegrationOutcome::NeedsSession {
                run: Box::new(run),
                main: main.clone(),
                reason,
            }
        }
        Verdict::ReceiptFailed { reason, receipt } => {
            warn!(
                op = "integrate",
                reason = %reason,
                "run {} failed: {reason}",
                run.id()
            );
            let run = queue.fail_integration(run.id(), token, &reason, receipt)?;
            IntegrationOutcome::Failed {
                run: Box::new(run),
                reason,
            }
        }
    })
}

/// Push the landed `main` to [`PUSH_REMOTE`] and record the outcome as
/// `push_finished`, `push_skipped` or `push_failed` on the landed run. A
/// failure to record is only reported: the landing stands either way.
fn push_main(
    queue: &dyn Queue,
    remote: Option<&dyn MainRemote>,
    run_id: &RunId,
    commit: &CommitSha,
) -> PushReport {
    let skipped = |reason: &str| PushReport {
        outcome: PushResult::Skipped,
        remote: PUSH_REMOTE.to_owned(),
        error: None,
        reason: Some(reason.to_owned()),
    };
    let report = match remote {
        None => skipped("--no-push"),
        Some(remote) => match remote.has_remote(PUSH_REMOTE) {
            Ok(false) => skipped(&format!("the repository has no remote {PUSH_REMOTE}")),
            Ok(true) => match remote.push_main(PUSH_REMOTE) {
                Ok(()) => PushReport {
                    outcome: PushResult::Pushed,
                    remote: PUSH_REMOTE.to_owned(),
                    error: None,
                    reason: None,
                },
                Err(error) => failed_push(&error),
            },
            Err(error) => failed_push(&error),
        },
    };
    let (kind, payload) = match report.outcome {
        PushResult::Pushed => (
            "push_finished",
            json!({"remote": report.remote, "commit": commit}),
        ),
        PushResult::Skipped => (
            "push_skipped",
            json!({"remote": report.remote, "commit": commit, "reason": report.reason}),
        ),
        PushResult::Failed => (
            "push_failed",
            json!({"code": ReasonCode::PushFailed, "remote": report.remote, "commit": commit, "error": report.error}),
        ),
    };
    match &report.error {
        Some(error) => warn!(
            op = "push",
            run_id = %run_id,
            remote = PUSH_REMOTE,
            error = %error,
            "run {run_id}: push of main failed: {error}"
        ),
        None => info!(
            op = "push",
            run_id = %run_id,
            remote = PUSH_REMOTE,
            outcome = kind,
            "run {run_id}: {kind} ({PUSH_REMOTE})"
        ),
    }
    if let Err(error) = queue.record_runtime_event(run_id, kind, payload) {
        warn!(
            op = "push",
            run_id = %run_id,
            error = %format_args!("{error:#}"),
            "run {run_id}: could not record {kind}: {error:#}"
        );
    }
    report
}

fn failed_push(error: &anyhow::Error) -> PushReport {
    PushReport {
        outcome: PushResult::Failed,
        remote: PUSH_REMOTE.to_owned(),
        error: Some(format!("{error:#}")),
        reason: None,
    }
}

/// Register the landed receipt's `follow_ups` of `task`'s run `run_id` as
/// draft tasks of the task's goal (ADR-0019 decision 4): the title and
/// description as proposed, no acceptance, verification commands or
/// dependencies, and a context naming where they came from. A closed goal
/// takes no task, so the follow-up is registered without a goal and its
/// `follow_up_registered` says `goal_closed: true`. An entry whose `title`
/// is not a non-blank string or whose `description` is not a string is not
/// registered: its `follow_up_registered` has `task_id: null`, the `skipped`
/// reason and the entry itself as `follow_up`. Every event carries the
/// entry's `index`, and an entry already recorded is not looked at again, so
/// a second call for the same run adds nothing (the task and its event are
/// written one after the other, so only a failure to record between them
/// could let a later call register it twice). A registration that fails is
/// only reported: the landing stands either way. Returns what this call
/// registered.
pub fn register_follow_ups<Q: Queue + ?Sized>(
    queue: &mut Q,
    task: &Task,
    run_id: &RunId,
    follow_ups: Option<&Value>,
) -> Vec<RegisteredFollowUp> {
    let Some(entries) = follow_ups.and_then(Value::as_array) else {
        return Vec::new();
    };
    let registered: Vec<u64> = match queue.run_events(run_id) {
        Ok(events) => events
            .iter()
            .filter(|e| e.kind == "follow_up_registered")
            .filter_map(|e| e.payload["index"].as_u64())
            .collect(),
        Err(error) => {
            warn!(
                op = "follow_up",
                run_id = %run_id,
                error = %format_args!("{error:#}"),
                "run {run_id}: follow_ups not registered: {error:#}"
            );
            return Vec::new();
        }
    };
    let goal_closed = match task.goal_id() {
        Some(goal_id) => match queue.show_goal(goal_id) {
            Ok(detail) => detail.closed,
            Err(error) => {
                warn!(
                    op = "follow_up",
                    run_id = %run_id,
                    error = %format_args!("{error:#}"),
                    "run {run_id}: follow_ups not registered: {error:#}"
                );
                return Vec::new();
            }
        },
        None => false,
    };
    let mut added = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if registered.contains(&(index as u64)) {
            continue;
        }
        let title = entry["title"].as_str().map(str::trim).unwrap_or_default();
        let description = entry["description"].as_str();
        let skipped = if title.is_empty() {
            Some("title is not a non-blank string")
        } else if description.is_none() {
            Some("description is not a string")
        } else {
            None
        };
        if let Some(reason) = skipped {
            warn!(
                op = "follow_up",
                run_id = %run_id,
                reason,
                "run {run_id}: follow_up {index} was not registered: {reason}"
            );
            let payload = json!({
                "task_id": null,
                "title": entry["title"],
                "index": index,
                "skipped": reason,
                "follow_up": entry,
            });
            if let Err(error) = queue.record_runtime_event(run_id, "follow_up_registered", payload)
            {
                warn!(
                    op = "follow_up",
                    run_id = %run_id,
                    error = %format_args!("{error:#}"),
                    "run {run_id}: could not record follow_up_registered: {error:#}"
                );
            }
            continue;
        }
        let new = NewTask {
            title: title.to_owned(),
            description: description.unwrap_or_default().to_owned(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: task.goal_id().filter(|_| !goal_closed),
            context: format!(
                "task {}（{}）の run {run_id} の receipt が提案した follow_up",
                task.id(),
                task.title()
            ),
        };
        let created = match queue.add(new) {
            Ok(created) => created,
            Err(error) => {
                warn!(
                    op = "follow_up",
                    run_id = %run_id,
                    error = %format_args!("{error:#}"),
                    "run {run_id}: follow_up {title:?} was not registered: {error:#}"
                );
                continue;
            }
        };
        let mut payload =
            json!({"task_id": created.id(), "title": created.title(), "index": index});
        if goal_closed {
            payload["goal_closed"] = json!(true);
        }
        if let Err(error) = queue.record_runtime_event(run_id, "follow_up_registered", payload) {
            warn!(
                op = "follow_up",
                run_id = %run_id,
                error = %format_args!("{error:#}"),
                "run {run_id}: could not record follow_up_registered: {error:#}"
            );
        }
        info!(
            op = "follow_up",
            run_id = %run_id,
            follow_up_task_id = %created.id(),
            "run {run_id}: follow_up {:?} registered as draft task {}",
            created.title(),
            created.id()
        );
        added.push(RegisteredFollowUp {
            task_id: created.id(),
            title: created.title().to_owned(),
        });
    }
    added
}

enum Verdict {
    /// Landed with the receipt's `follow_ups`.
    Landed(Landing, Option<Value>),
    /// Re-validation did not pass; the worktree is left for a session.
    Deferred { reason: String, detail: Value },
    /// The session's rewritten receipt reports `failed`; `receipt` is its
    /// JSON, kept with the `integration_failed` event.
    ReceiptFailed { reason: String, receipt: Value },
}

/// Rebase, re-validate and land one run. `Ok(Deferred)` and
/// `Ok(ReceiptFailed)` are verdicts on the run; `Err` is a failure of the
/// landing itself (Git, files) before `main` moved.
fn land(
    queue: &mut dyn Queue,
    repository: &dyn Repository,
    verifier: &dyn Verifier,
    files: &dyn RunFiles,
    task: &Task,
    run: &TaskRun,
    main: &CommitSha,
) -> Result<Verdict> {
    let defer = |code: Reason, reason: String, detail: Value| {
        Ok(Verdict::Deferred {
            reason,
            detail: code.on(detail),
        })
    };
    let worktree = Path::new(run.worktree_path().context("missing worktree")?);
    ensure!(
        files.is_dir(worktree),
        "worktree {} is missing",
        worktree.display()
    );
    // A worktree whose queue directory moved is still found through its own
    // `.git` file, but the repository's record of it points at the old path
    // until repaired, and removing it after landing would fail (ADR-0017).
    repository.repair_worktree(worktree)?;
    let branch = run.branch().context("missing branch")?;
    let run_dir = Path::new(run.run_dir().context("missing run directory")?);
    // A rebase left behind by a crashed landing or an unfinished session is undone first.
    if repository.rebase_in_progress(worktree)? {
        repository.rebase_abort(worktree)?;
        queue.record_runtime_event(
            run.id(),
            "integration_rebase_aborted",
            json!({"code": ReasonCode::RebaseInProgress, "reason": "a rebase was left in progress"}),
        )?;
    }
    let expected_ref = format!("refs/heads/{branch}");
    match repository.current_branch(worktree)? {
        Some(current) if current == expected_ref => (),
        current => {
            return defer(
                ReasonCode::CommitMismatch.into(),
                format!(
                    "worktree is on {} instead of {expected_ref}",
                    current.as_deref().unwrap_or("a detached HEAD")
                ),
                json!({}),
            );
        }
    }
    let head = repository.head(worktree)?;
    // The receipt must describe this head: the validated one for a fresh run,
    // the one the session rewrote after resolving otherwise. A stale receipt
    // means the session is not done.
    let receipt_path = Path::new(run.receipt_path().context("missing receipt path")?);
    let receipt = match files.read_to_string(receipt_path) {
        Ok(text) => match Receipt::parse(&text) {
            Ok(receipt) => receipt,
            Err(error) => {
                return defer(
                    ReasonCode::of_receipt_error(&error).into(),
                    format!("{error:#}"),
                    json!({}),
                );
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return defer(
                ReasonCode::ReceiptMissing.into(),
                format!("receipt is missing at {}", receipt_path.display()),
                json!({}),
            );
        }
        Err(error) => return Err(error).context("read receipt"),
    };
    if receipt.result == ReceiptResult::Failed {
        return Ok(Verdict::ReceiptFailed {
            reason: format!("session reported the run as failed: {}", receipt.summary),
            receipt: serde_json::to_value(&receipt)?,
        });
    }
    if let Err(error) = receipt.check_requiring(run.id(), task.required_evidence()) {
        return defer(
            ReasonCode::of_receipt_error(&error).into(),
            format!("{error:#}"),
            json!({}),
        );
    }
    // A resumed session may have come back without the evidence it was
    // asked for; `checks` tells the next resume to ask for it again.
    let missing = receipt.missing_evidence(task.required_evidence());
    if !missing.is_empty() {
        return defer(
            ReasonCode::EvidenceMissing.into(),
            evidence_missing_reason(&missing),
            json!({"checks": missing}),
        );
    }
    // The receipt read here is the one that lands (or the one a session
    // rewrote after resolving), so it is recorded whatever happens next: the
    // DB otherwise keeps only the receipt seen at validation time.
    queue.record_runtime_event(
        run.id(),
        "integration_receipt",
        json!({
            "main": main,
            "commit": receipt.commit,
            "receipt": serde_json::to_value(&receipt)?,
        }),
    )?;
    if head.as_str() != receipt.commit.to_ascii_lowercase() {
        return defer(
            ReasonCode::CommitMismatch.into(),
            format!(
                "receipt commit {} is not the head of {branch} ({head}); rerun the verification commands and rewrite the receipt for the current head",
                receipt.commit
            ),
            json!({"head": head}),
        );
    }
    let status = repository.status(worktree)?;
    if !status.trim().is_empty() {
        return defer(
            ReasonCode::WorktreeDirty.into(),
            format!("worktree is not clean:\n{}", status.trim_end()),
            json!({"head": head}),
        );
    }
    // Onto the current main. A no-op when the run already sits on it.
    if let Err(output) = repository.rebase(worktree, main.as_str())? {
        let conflicts = repository.conflicted_files(worktree).unwrap_or_default();
        if repository.rebase_in_progress(worktree)? {
            repository.rebase_abort(worktree)?;
        }
        return defer(
            ReasonCode::RebaseConflict.into(),
            format!(
                "rebase onto main {main} conflicted in {}; resolve it in the worktree (git rebase {main}), rerun the verification commands, and rewrite the receipt with the new head",
                if conflicts.is_empty() {
                    "the run branch".to_owned()
                } else {
                    conflicts.join(", ")
                }
            ),
            json!({
                "main": main,
                "head": head,
                "conflicts": conflicts,
                "output_tail": tail(&output, 2000),
                "aborted": true,
            }),
        );
    }
    let rebased = repository.head(worktree)?;
    queue.record_runtime_event(
        run.id(),
        "integration_rebased",
        json!({"main": main, "head_before": head, "head_after": rebased}),
    )?;
    if rebased == *main {
        return defer(
            ReasonCode::RebaseEmpty.into(),
            format!(
                "no commit remains on top of main {main} after the rebase; if the change is no longer needed, write a failed receipt with the reason"
            ),
            json!({"main": main, "head": rebased}),
        );
    }
    ensure!(
        repository.is_ancestor(main.as_str(), rebased.as_str())?,
        "rebased head {rebased} does not descend from main {main}"
    );
    let status = repository.status(worktree)?;
    if !status.trim().is_empty() {
        return defer(
            ReasonCode::WorktreeDirty.into(),
            format!(
                "worktree is not clean after the rebase:\n{}",
                status.trim_end()
            ),
            json!({"main": main, "head": rebased}),
        );
    }
    // What lands is the squash of main..rebased, so that is the diff held to
    // the task's paths (ADR-0029): the rebase may have changed it since
    // validation, and a resumed session may have committed more.
    let outside = out_of_scope(
        task.paths(),
        &repository.changed_paths(main.as_str(), rebased.as_str())?,
    );
    if !outside.is_empty() {
        return defer(
            ReasonCode::ScopeViolation.into(),
            format!(
                "{} after the rebase onto main {main}; take them out of the run branch (or ask for the task's --paths to be widened), commit, and rewrite the receipt with the new head",
                scope_violation_reason(&outside)
            ),
            json!({"main": main, "head": rebased, "scope_violation": outside, "allowed": task.paths()}),
        );
    }
    // The task's verification commands run here, once per commit, on the
    // rebased tree: validation only checks the receipt (ADR-0023 decision 1).
    let commands = task.verification_commands();
    let run_env = if commands.is_empty() {
        Vec::new()
    } else {
        verifier.run_env(run_dir)?
    };
    // Each attempt keeps its own logs, so a second integrate of the run does
    // not overwrite why the first one failed.
    let attempt = next_integrate_attempt(files, run_dir);
    for (index, command) in commands.iter().enumerate() {
        let log = integrate_verify_log(run_dir, attempt, index + 1);
        let status = verifier.run_to_log(command, worktree, &run_env, &log)?;
        let exit_code = status.code.unwrap_or(128);
        let output = files.read_to_string(&log).unwrap_or_default();
        queue.record_runtime_event(
            run.id(),
            "verification_command",
            json!({
                "phase": "integration",
                "attempt": attempt,
                "index": index + 1,
                "command": command,
                "exit_code": exit_code,
                "log_path": path_text(&log)?,
                "output_tail": tail(&output, 2000),
            }),
        )?;
        if exit_code != 0 {
            return defer(
                Reason::new(ReasonCode::VerificationFailed).with("index", index + 1),
                format!(
                    "verification command {command:?} exited with {exit_code} after the rebase onto {main}; see {}",
                    log.display()
                ),
                json!({"main": main, "head": rebased, "command": command, "exit_code": exit_code}),
            );
        }
    }
    // One commit on main with the rebased tree; the run's own history stays
    // reachable under refs/dagq/runs/<run-id>.
    let paragraphs = commit_message(task, run, &receipt);
    let tree = repository.tree_of(rebased.as_str())?;
    let commit = repository.commit_tree(&tree, main.as_str(), &paragraphs)?;
    let history_ref = format!("refs/dagq/runs/{}", run.id());
    repository.update_ref(&history_ref, rebased.as_str())?;
    repository.advance_main(main.as_str(), commit.as_str())?;
    Ok(Verdict::Landed(
        Landing {
            commit,
            source_commit: rebased,
            main_before: main.clone(),
            history_ref,
            message: paragraphs.join("\n\n"),
            verification_skipped: false,
        },
        receipt.follow_ups,
    ))
}

/// Where integrate's attempt `attempt` writes the log of its `index`th
/// verification command (both from 1): `integrate-<attempt>-verify-<index>.log`.
pub fn integrate_verify_log(run_dir: &Path, attempt: u32, index: usize) -> PathBuf {
    run_dir.join(format!("integrate-{attempt}-verify-{index}.log"))
}

/// The attempt and command index of an integrate verification log's file
/// name. The name used before attempts were counted,
/// `integrate-verify-<index>.log`, is attempt 0: it came before any
/// numbered one.
fn integrate_log_key(name: &str) -> Option<(u32, usize)> {
    let stem = name.strip_prefix("integrate-")?.strip_suffix(".log")?;
    if let Some(index) = stem.strip_prefix("verify-") {
        return Some((0, index.parse().ok()?));
    }
    let (attempt, index) = stem.split_once("-verify-")?;
    Some((attempt.parse().ok()?, index.parse().ok()?))
}

/// The files of `dir` with their names; none when it cannot be read.
pub fn log_names(files: &dyn RunFiles, dir: &Path) -> Vec<(String, PathBuf)> {
    files
        .read_dir(dir)
        .map(|entries| {
            entries
                .into_iter()
                .filter_map(|path| {
                    let name = path.file_name()?.to_str()?.to_owned();
                    Some((name, path))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The attempt integrate's next run of the verification commands writes
/// its logs under: one past the highest attempt in `run_dir` (1 when none).
pub fn next_integrate_attempt(files: &dyn RunFiles, run_dir: &Path) -> u32 {
    log_names(files, run_dir)
        .iter()
        .filter_map(|(name, _)| integrate_log_key(name))
        .map(|(attempt, _)| attempt)
        .max()
        .map_or(1, |attempt| attempt + 1)
}

/// Integrate's verification logs in `run_dir`: those of the latest attempt
/// in command order, and those of the earlier attempts, oldest first.
pub fn integrate_logs(files: &dyn RunFiles, run_dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut keyed: Vec<((u32, usize), PathBuf)> = log_names(files, run_dir)
        .into_iter()
        .filter_map(|(name, path)| Some((integrate_log_key(&name)?, path)))
        .collect();
    keyed.sort();
    let Some(&((latest, _), _)) = keyed.last() else {
        return (Vec::new(), Vec::new());
    };
    let (current, earlier): (Vec<_>, Vec<_>) = keyed
        .into_iter()
        .partition(|((attempt, _), _)| *attempt == latest);
    (
        current.into_iter().map(|(_, path)| path).collect(),
        earlier.into_iter().map(|(_, path)| path).collect(),
    )
}

/// Title, the receipt's summary, and the trailers that tie the commit to
/// the queue, as paragraphs.
fn commit_message(task: &Task, run: &TaskRun, receipt: &Receipt) -> Vec<String> {
    let mut paragraphs = vec![task.title().trim().to_owned()];
    let summary = receipt.summary.trim();
    if !summary.is_empty() {
        paragraphs.push(summary.to_owned());
    }
    paragraphs.push(format!("Dagq-Task: {}\nDagq-Run: {}", task.id(), run.id()));
    paragraphs
}

/// Drop the landed run's worktree and branch. The result is already on
/// `main` and under the history ref, so a failure here is only recorded.
fn remove_landed_worktree(queue: &mut dyn Queue, repository: &dyn Repository, run: &TaskRun) {
    let (Some(worktree), Some(branch)) = (&run.worktree_path(), &run.branch()) else {
        return;
    };
    let recorded = match repository.remove_worktree_and_branch(Path::new(worktree), branch) {
        Ok(()) => queue.record_runtime_event(
            run.id(),
            "worktree_removed",
            json!({"path": worktree, "branch": branch}),
        ),
        Err(error) => {
            let message = format!("landed worktree {worktree} could not be removed: {error:#}");
            warn!(op = "cleanup", run_id = %run.id(), "run {}: {message}", run.id());
            queue.record_cleanup_failure(run.id(), &message, &ReasonCode::GitFailed.into())
        }
    };
    if let Err(error) = recorded {
        warn!(
            op = "cleanup",
            run_id = %run.id(),
            error = %format_args!("{error:#}"),
            "run {}: could not record the cleanup: {error:#}",
            run.id()
        );
    }
}

/// How many resumes of the run were started, for a resume error recorded
/// outside the resume watch; unreadable counts as the last attempt.
pub fn resume_attempts<Q: RunStore + ?Sized>(queue: &Q, id: &RunId) -> usize {
    queue
        .run_events(id)
        .map(|events| events.iter().filter(|e| e.kind == "resume_started").count())
        .unwrap_or(MAX_RESUME_ATTEMPTS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::memory_files::MemoryFiles;
    use crate::domain::{Provider, RunRecord, TaskRecord, TaskStatus};
    use std::cell::RefCell;

    const BASE: &str = "1111111111111111111111111111111111111111";
    const HEAD: &str = "2222222222222222222222222222222222222222";
    const RUN: &str = "00000000-0000-4000-8000-000000000001";

    /// A repository whose worktree is on `branch` at `head`, with `status`.
    struct FakeRepository {
        branch: Option<String>,
        head: CommitSha,
        status: String,
        calls: RefCell<Vec<String>>,
    }

    impl FakeRepository {
        fn sound() -> Self {
            Self {
                branch: Some(format!("refs/heads/dagq/{RUN}")),
                head: sha(HEAD),
                status: String::new(),
                calls: RefCell::default(),
            }
        }
    }

    impl Repository for FakeRepository {
        fn main_head(&self) -> Result<CommitSha> {
            Ok(sha(BASE))
        }
        fn current_branch(&self, _: &Path) -> Result<Option<String>> {
            Ok(self.branch.clone())
        }
        fn head(&self, _: &Path) -> Result<CommitSha> {
            Ok(self.head.clone())
        }
        fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
            self.calls
                .borrow_mut()
                .push(format!("is_ancestor {ancestor} {descendant}"));
            Ok(ancestor == BASE)
        }
        fn merge_base(&self, _: &str, _: &str) -> Result<Option<CommitSha>> {
            Ok(Some(sha(BASE)))
        }
        fn status(&self, _: &Path) -> Result<String> {
            Ok(self.status.clone())
        }
        fn rebase_in_progress(&self, _: &Path) -> Result<bool> {
            unimplemented!()
        }
        fn rebase_abort(&self, _: &Path) -> Result<()> {
            unimplemented!()
        }
        fn rebase(&self, _: &Path, _: &str) -> Result<std::result::Result<(), String>> {
            unimplemented!()
        }
        fn conflicted_files(&self, _: &Path) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn changed_paths(&self, _: &str, _: &str) -> Result<Vec<String>> {
            Ok(vec!["src/lib.rs".to_owned()])
        }
        fn tree_of(&self, _: &str) -> Result<String> {
            unimplemented!()
        }
        fn commit_tree(&self, _: &str, _: &str, _: &[String]) -> Result<CommitSha> {
            unimplemented!()
        }
        fn update_ref(&self, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn advance_main(&self, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn repair_worktree(&self, _: &Path) -> Result<()> {
            unimplemented!()
        }
        fn remove_worktree_and_branch(&self, _: &Path, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn main_checkout(&self) -> Result<Option<PathBuf>> {
            unimplemented!()
        }
        fn create_worktree(&self, _: &TaskRun) -> Result<String> {
            unimplemented!()
        }
        fn merge_conflicts(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn landed_task_ids(&self, _: &str, _: &str) -> Result<Vec<crate::domain::TaskId>> {
            unimplemented!()
        }
        fn log_oneline(&self, _: &str, _: &str) -> Result<String> {
            unimplemented!()
        }
        fn diff_stat(&self, _: &str, _: &str) -> Result<String> {
            unimplemented!()
        }
        fn diff_numbers(&self, _: &str, _: &str) -> Result<crate::application::DiffNumbers> {
            unimplemented!()
        }
        fn diff_to_file(&self, _: &str, _: &str, _: &Path) -> Result<()> {
            unimplemented!()
        }
    }

    fn sha(text: &str) -> CommitSha {
        CommitSha::parse(text, "commit").unwrap()
    }

    fn task(paths: &[&str]) -> Task {
        Task::restore(TaskRecord {
            id: TaskId::new(7),
            title: "  land the change  ".to_owned(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: vec![EvidenceCheck::Tests],
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            priority: Default::default(),
            status: TaskStatus::InProgress,
            goal_id: None,
            context: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap()
    }

    fn run(dir: &Path) -> TaskRun {
        TaskRun::restore(RunRecord {
            id: RunId::new(RUN).unwrap(),
            task_id: TaskId::new(7),
            status: RunStatus::Validating,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            base_commit: sha(BASE),
            branch: Some(format!("dagq/{RUN}")),
            worktree_path: Some(path_text(dir).unwrap()),
            workspace_id: None,
            receipt_path: Some(path_text(&dir.join("receipt.json")).unwrap()),
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: Some(path_text(dir).unwrap()),
            last_error: None,
            workspace_closed_at: None,
            created_at: String::new(),
        })
        .unwrap()
    }

    /// Where the run's files are in [`MemoryFiles`].
    const DIR: &str = "/runs/run";

    fn write_receipt(files: &MemoryFiles, dir: &Path, tests: &str) {
        let receipt = json!({
            "run_id": RUN,
            "result": "succeeded",
            "commit": HEAD,
            "tests": {"status": tests, "evidence_or_reason": "cargo test"},
            "e2e": {"status": "not_applicable", "evidence_or_reason": "no runtime change"},
            "subagent_review": {"status": "not_applicable", "evidence_or_reason": "small"},
            "summary": "  squash it  ",
        });
        files
            .write(&dir.join("receipt.json"), receipt.to_string().as_bytes())
            .unwrap();
    }

    #[test]
    fn a_sound_receipt_is_accepted_through_the_repository_port() {
        let (files, dir) = (MemoryFiles::default(), Path::new(DIR));
        write_receipt(&files, dir, "passed");
        let repository = FakeRepository::sound();
        let (receipt, commit) = check_receipt(&repository, &files, &task(&[]), &run(dir))
            .unwrap()
            .unwrap_or_else(|rejection| panic!("{}", rejection.reason));
        assert_eq!(commit, sha(HEAD));
        assert_eq!(
            repository.calls.borrow().as_slice(),
            [format!("is_ancestor {BASE} {HEAD}")]
        );
        assert_eq!(
            commit_message(&task(&[]), &run(dir), &receipt),
            [
                "land the change".to_owned(),
                "squash it".to_owned(),
                format!("Dagq-Task: 7\nDagq-Run: {RUN}"),
            ]
        );
    }

    #[test]
    fn check_receipt_rejects_what_git_does_not_back() {
        let (files, dir) = (MemoryFiles::default(), Path::new(DIR));
        let reason = |repository: &FakeRepository, task: &Task| match check_receipt(
            repository,
            &files,
            task,
            &run(dir),
        )
        .unwrap()
        {
            Ok(_) => panic!("accepted"),
            Err(rejection) => rejection,
        };
        let missing = reason(&FakeRepository::sound(), &task(&[]));
        assert!(missing.reason.starts_with("receipt was not submitted at "));
        assert!(missing.receipt.is_none());

        write_receipt(&files, dir, "passed");
        let detached = FakeRepository {
            branch: None,
            ..FakeRepository::sound()
        };
        assert_eq!(
            reason(&detached, &task(&[])).reason,
            format!("worktree is on a detached HEAD instead of refs/heads/dagq/{RUN}")
        );
        let dirty = FakeRepository {
            status: " M src/lib.rs\n".to_owned(),
            ..FakeRepository::sound()
        };
        let rejection = reason(&dirty, &task(&[]));
        assert_eq!(rejection.reason, "worktree is not clean:\n M src/lib.rs");
        assert_eq!(rejection.commit, Some(sha(HEAD)));
        let outside = reason(&FakeRepository::sound(), &task(&["docs/**"]));
        assert_eq!(outside.scope_violation, ["src/lib.rs"]);

        write_receipt(&files, dir, "not_applicable");
        let evidence = reason(&FakeRepository::sound(), &task(&[]));
        assert_eq!(evidence.evidence_missing, [EvidenceCheck::Tests]);
    }

    #[test]
    fn integrate_logs_are_read_through_the_run_files_port() {
        let (files, dir) = (MemoryFiles::default(), Path::new(DIR));
        assert_eq!(next_integrate_attempt(&files, dir), 1);
        for name in [
            "integrate-verify-1.log",
            "integrate-2-verify-2.log",
            "integrate-2-verify-1.log",
            "verify-1.log",
        ] {
            files.write(&dir.join(name), b"").unwrap();
        }
        files
            .write(Path::new("/elsewhere/integrate-9-verify-1.log"), b"")
            .unwrap();
        assert_eq!(next_integrate_attempt(&files, dir), 3);
        assert_eq!(
            integrate_logs(&files, dir),
            (
                vec![
                    dir.join("integrate-2-verify-1.log"),
                    dir.join("integrate-2-verify-2.log")
                ],
                vec![dir.join("integrate-verify-1.log")]
            )
        );
    }

    #[test]
    fn tail_keeps_the_last_bytes_on_a_character_boundary() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("ab", 3), "ab");
        // A multi-byte character is not split.
        assert_eq!(tail("aé", 2), "é");
        assert_eq!(tail("aé", 1), "");
    }
}
