//! The landing recheck (ADR-0068): after this supervisor lands a run, the
//! runs that wait to land are checked against the new main off the loop,
//! one recheck at a time: `git merge-tree` for a conflict, then the
//! `[recheck] command` of `dagq.toml` on main's tree with the run merged
//! in, in one scratch worktree with one target directory of the queue's.
//! A run found no longer landing is parked for a resume at once, whatever
//! asks it waits on; a run this supervisor holds in a slot is parked when
//! it would land.

use super::*;
use crate::domain::recheck::{self, HELD, LANDING_RECHECK_FAILED, Landed, RecheckFailure};

/// Where the recheck keeps its scratch worktree and its target directory,
/// under the queue's directory.
const RECHECK_DIR: &str = "recheck";

/// One waiting run to check: the head it would land and its run directory
/// (where the command's log goes).
#[derive(Debug, Clone)]
pub(super) struct Target {
    run_id: RunId,
    head: CommitSha,
    run_dir: PathBuf,
}

/// What the recheck found for one run.
#[derive(Debug)]
pub(super) enum Finding {
    Clean,
    Failed(RecheckFailure),
    /// Git or the command could not be run: nothing is recorded on the run.
    Error(String),
}

/// A recheck in progress on its thread.
pub(super) struct RecheckWatch {
    landed: Landed,
    main: CommitSha,
    command: Option<String>,
    started: Instant,
    handle: Option<thread::JoinHandle<Vec<(Target, Finding)>>>,
}

/// The supervisor's rechecks: the one running and the latest landing that
/// waits for the next (a landing during a recheck is checked after it,
/// against the newest main).
#[derive(Default)]
pub(super) struct Rechecks {
    running: Option<RecheckWatch>,
    due: Option<Landed>,
}

impl Rechecks {
    /// Whether a recheck runs now: the loop waits for it like a job.
    pub(super) const fn running(&self) -> bool {
        self.running.is_some()
    }
}

impl Phase {
    /// Whether the run waits in this slot to land: for the integration
    /// slot, or for its session's `/exit` before it lands.
    fn waits_to_land(&self) -> bool {
        match self {
            Phase::AwaitingSlot => true,
            Phase::Exiting(watch) => matches!(watch.then, AfterExit::Land),
            _ => false,
        }
    }
}

impl Supervisor<'_> {
    /// Note that `run` landed: the waiting runs are checked against the
    /// main it moved (ADR-0068 decision 1).
    pub(super) fn note_landed(&mut self, run: &TaskRun) {
        self.rechecks.due = Some(Landed {
            run_id: run.id().clone(),
            task_id: run.task_id(),
        });
    }

    /// Apply the recheck that finished, and start the one that is due
    /// unless this pass drains. An error is logged: the recheck is an aid,
    /// and the landing checks every run again anyway. Whether a recheck
    /// was applied: the next pass resumes the runs it parked.
    pub(super) fn recheck_pass(&mut self) -> bool {
        let mut applied = false;
        if self
            .rechecks
            .running
            .as_ref()
            .is_some_and(|watch| watch.handle.as_ref().is_none_or(|h| h.is_finished()))
            && let Some(mut watch) = self.rechecks.running.take()
        {
            let found = watch
                .handle
                .take()
                .map(|handle| handle.join())
                .transpose()
                .unwrap_or_else(|_| {
                    warn!("the landing recheck thread panicked");
                    None
                })
                .unwrap_or_default();
            if let Err(error) = self.apply_recheck(&watch, found) {
                warn!(error = %format_args!("{error:#}"), "the landing recheck against main {} could not be recorded: {error:#}", watch.main);
            }
            applied = true;
        }
        if self.rechecks.running.is_none()
            && !self.draining
            && let Some(landed) = self.rechecks.due.take()
            && let Err(error) = self.start_recheck(landed)
        {
            warn!(error = %format_args!("{error:#}"), "the landing recheck could not start: {error:#}");
        }
        applied
    }

    /// The runs a recheck looks at (ADR-0068 decision 1): those awaiting
    /// integration with a reviewed head that nobody leases (waiting for an
    /// ask's answer or for a recover), and those this supervisor holds in
    /// a slot to land. Runs in review or revise, and runs another process
    /// leases, are left to their own checks.
    fn recheck_targets(&mut self) -> Result<Vec<Target>> {
        let mut targets = Vec::new();
        for run in self
            .queue
            .runs_with_status(RunStatus::AwaitingIntegration)?
        {
            let (Some(head), Some(run_dir)) = (run.result_commit(), run.run_dir()) else {
                continue;
            };
            let waiting = match self.queue.run_lease(run.id())? {
                None => true,
                Some(lease) => {
                    lease.token == self.token
                        && self
                            .slots
                            .iter()
                            .any(|slot| slot.run.id() == run.id() && slot.phase.waits_to_land())
                }
            };
            if waiting {
                targets.push(Target {
                    run_id: run.id().clone(),
                    head: head.clone(),
                    run_dir: PathBuf::from(run_dir),
                });
            }
        }
        Ok(targets)
    }

    fn start_recheck(&mut self, landed: Landed) -> Result<()> {
        let targets = self.recheck_targets()?;
        if targets.is_empty() {
            return Ok(());
        }
        let main = self.repository.main_head()?;
        // A command whose program [run.env] cannot find would fail on every
        // run (ADR-0049 decision 9): only the merge is checked then.
        let command = if self.run_env_missing {
            None
        } else {
            self.verifier.recheck_command()?
        };
        let dir = self
            .layout
            .db
            .parent()
            .context("queue database has no directory")?
            .join(RECHECK_DIR);
        info!(
            "landing recheck of {} waiting run(s) against main {main} after run {} (task {}) landed{}",
            targets.len(),
            landed.run_id,
            landed.task_id,
            command
                .as_ref()
                .map(|c| format!("; command {c:?}"))
                .unwrap_or_default()
        );
        let repository = self.repository.clone();
        let verifier = self.verifier.clone();
        let files = self.files.clone();
        let on = main.clone();
        let run_command = command.clone();
        let handle = spawn_traced(move || {
            recheck_runs(
                &*repository,
                &*verifier,
                &*files,
                &on,
                run_command.as_deref(),
                &dir,
                targets,
            )
        });
        self.rechecks.running = Some(RecheckWatch {
            landed,
            main,
            command,
            started: Instant::now(),
            handle: Some(handle),
        });
        Ok(())
    }

    /// Record what a recheck found: each failure on its run (and its open
    /// asks), and `landing_recheck_finished` with the counts on the run
    /// whose landing it followed.
    fn apply_recheck(&mut self, watch: &RecheckWatch, found: Vec<(Target, Finding)>) -> Result<()> {
        let mut counts = json!({
            "checked": found.len(),
            "clean": 0,
            "conflicts": 0,
            "check_failed": 0,
            "errors": 0,
            "resumed": 0,
            "held": 0,
        });
        let mut failed_runs = Vec::new();
        for (target, finding) in found {
            let failure = match finding {
                Finding::Clean => {
                    bump(&mut counts, "clean");
                    continue;
                }
                Finding::Error(error) => {
                    bump(&mut counts, "errors");
                    warn!(run_id = %target.run_id, error = %error, "run {}: the landing recheck against main {} could not judge it: {error}", target.run_id, watch.main);
                    continue;
                }
                Finding::Failed(failure) => failure,
            };
            bump(
                &mut counts,
                match failure {
                    RecheckFailure::Conflict { .. } => "conflicts",
                    RecheckFailure::CheckFailed { .. } => "check_failed",
                },
            );
            match self.record_recheck_failure(watch, &target, &failure) {
                Ok(Some(action)) => {
                    bump(&mut counts, action);
                    failed_runs.push(json!({
                        "run_id": target.run_id,
                        "code": failure.code(),
                        "action": action,
                    }));
                }
                Ok(None) => {
                    info!(run_id = %target.run_id, "run {}: the landing recheck found it no longer landing on main {}, but it moved on meanwhile", target.run_id, watch.main);
                }
                Err(error) => {
                    warn!(run_id = %target.run_id, error = %format_args!("{error:#}"), "run {}: the landing recheck's finding could not be recorded: {error:#}", target.run_id);
                }
            }
        }
        let mut payload = json!({
            "main": watch.main,
            "landed_run_id": watch.landed.run_id,
            "landed_task_id": watch.landed.task_id,
            "command": watch.command,
            "duration_secs": watch.started.elapsed().as_secs(),
            "failed_runs": failed_runs,
            "supervisor": self.token,
        });
        if let (Some(payload), Some(counts)) = (payload.as_object_mut(), counts.as_object()) {
            payload.extend(counts.clone());
        }
        info!(
            "landing recheck against main {} finished: {counts}",
            watch.main
        );
        self.queue.record_runtime_event(
            &watch.landed.run_id,
            recheck::LANDING_RECHECK_FINISHED,
            payload,
        )?;
        Ok(())
    }

    /// Record `failure` on the run (ADR-0068 decisions 3 and 4): a run
    /// nobody leases is parked for a resume in the same transaction; one
    /// this supervisor holds in a slot records it as held and is parked
    /// when it would land. Either way its open asks get the finding. `None`
    /// when the run moved on since it was checked (another head, another
    /// status, or a lease of another process).
    fn record_recheck_failure(
        &mut self,
        watch: &RecheckWatch,
        target: &Target,
        failure: &RecheckFailure,
    ) -> Result<Option<&'static str>> {
        let run = self.queue.run(&target.run_id)?;
        if run.status() != RunStatus::AwaitingIntegration
            || run.result_commit() != Some(&target.head)
        {
            return Ok(None);
        }
        let reason = failure.reason(&watch.landed, &watch.main);
        let mut payload = failure.payload(&watch.landed, &watch.main, &target.head);
        let action = match self.queue.run_lease(run.id())? {
            None => match self
                .queue
                .park_rechecked(run.id(), None, &reason, payload)?
            {
                Some(_) => recheck::RESUMED,
                None => return Ok(None),
            },
            Some(lease) if lease.token == self.token => {
                payload["action"] = json!(HELD);
                payload["reason"] = json!(reason);
                self.queue
                    .record_runtime_event(run.id(), LANDING_RECHECK_FAILED, payload)?;
                HELD
            }
            Some(_) => return Ok(None),
        };
        warn!(run_id = %run.id(), "run {}: {reason}; {}", run.id(), if action == HELD {
            "it is parked for a resume instead of landing"
        } else {
            "it is resumed without waiting for its asks"
        });
        let note = recheck::ask_note(&reason, action == recheck::RESUMED);
        for ask in self
            .queue
            .note_on_asks(run.id(), &note, LANDING_RECHECK_FAILED)?
        {
            info!(run_id = %run.id(), ask_id = %ask.id, "run {}: the landing recheck's finding was added to its {} ask {}", run.id(), ask.kind.as_str(), ask.id);
        }
        Ok(Some(action))
    }

    /// Park `run`, which this supervisor holds to land onto `main`, when
    /// the latest recheck found its head not landing on that main
    /// (ADR-0068 decision 3): the landing would only fail the same way.
    /// The parked run, or `None` to land it.
    pub(super) fn park_held_by_recheck(
        &mut self,
        run: &TaskRun,
        main: &CommitSha,
    ) -> Result<Option<TaskRun>> {
        let Some(head) = run.result_commit() else {
            return Ok(None);
        };
        let events = self.queue.run_events(run.id())?;
        let Some(held) = recheck::held_against(&events, main.as_str(), head.as_str()) else {
            return Ok(None);
        };
        let reason = held.payload["reason"]
            .as_str()
            .unwrap_or("the landing recheck found that the run no longer lands on main")
            .to_owned();
        let mut payload = held.payload.clone();
        if let Some(fields) = payload.as_object_mut() {
            for key in ["action", "reason", "status"] {
                fields.remove(key);
            }
        }
        payload["repeat"] = json!(true);
        let parked = self
            .queue
            .park_rechecked(run.id(), Some(&self.token), &reason, payload)?;
        if parked.is_some() {
            info!(run_id = %run.id(), "run {}: parked for a resume instead of landing onto main {main}: {reason}", run.id());
        }
        Ok(parked)
    }
}

fn bump(counts: &mut Value, key: &str) {
    counts[key] = json!(counts[key].as_u64().unwrap_or(0) + 1);
}

/// Check each target against `main`, one after another (ADR-0068 decision
/// 2): `git merge-tree` first; a clean merge, when there is a `command`,
/// is committed on top of main, checked out in the scratch worktree under
/// `dir` and the command run there in `/bin/sh` with the run's `[run.env]`
/// and `CARGO_TARGET_DIR` set to the one target directory under `dir`, its
/// output in `recheck-<main>.log` of the run directory.
fn recheck_runs(
    repository: &dyn Repository,
    verifier: &dyn Verifier,
    files: &dyn RunFiles,
    main: &CommitSha,
    command: Option<&str>,
    dir: &Path,
    targets: Vec<Target>,
) -> Vec<(Target, Finding)> {
    targets
        .into_iter()
        .map(|target| {
            let finding = recheck_run(repository, verifier, files, main, command, dir, &target)
                .unwrap_or_else(|error| Finding::Error(format!("{error:#}")));
            (target, finding)
        })
        .collect()
}

fn recheck_run(
    repository: &dyn Repository,
    verifier: &dyn Verifier,
    files: &dyn RunFiles,
    main: &CommitSha,
    command: Option<&str>,
    dir: &Path,
    target: &Target,
) -> Result<Finding> {
    let tree = match repository.merged_tree(main.as_str(), target.head.as_str())? {
        Ok(tree) => tree,
        Err(paths) => return Ok(Finding::Failed(RecheckFailure::Conflict { paths })),
    };
    let Some(command) = command else {
        return Ok(Finding::Clean);
    };
    let merged = repository.commit_tree(
        &tree,
        main.as_str(),
        &[format!(
            "dagq landing recheck of run {} on main {main}",
            target.run_id
        )],
    )?;
    let scratch = dir.join("worktree");
    files.create_dir_all(dir)?;
    repository.checkout_scratch(&scratch, merged.as_str())?;
    let mut env = verifier.run_env(&target.run_dir)?;
    env.retain(|(key, _)| key != "CARGO_TARGET_DIR");
    env.push((
        "CARGO_TARGET_DIR".to_owned(),
        path_text(&dir.join("target"))?,
    ));
    let log = target
        .run_dir
        .join(format!("recheck-{}.log", &main.as_str()[..12]));
    let exit = verifier.run_to_log(command, &scratch, &env, &log)?;
    if exit.success {
        return Ok(Finding::Clean);
    }
    let output = files.read_to_string(&log).unwrap_or_default();
    Ok(Finding::Failed(RecheckFailure::CheckFailed {
        command: command.to_owned(),
        exit_code: exit.code.unwrap_or(128),
        log_path: path_text(&log)?,
        output_tail: tail(&output, 2000).to_owned(),
    }))
}
