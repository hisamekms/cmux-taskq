//! The supervisor's side of the automatic update (ADR-0045 decision 17):
//! on a pass at most every `UpdateSettings::interval`, with `auto_update`
//! on its registration, it applies the answers of the `update_failed`
//! asks, and starts the update job ([`crate::application::update::run`])
//! when main moved past the commit it last updated to and the commits in
//! between change the runtime. The job runs in a session of its own and
//! outlives an exec of this process; one job runs at a time, and a landing
//! during it is built once it ended (the latest head, once).

use super::*;
use crate::application::update::{
    JOB_STEPS, UPDATE_ANSWERED, UPDATE_ASKER, UPDATE_FAILED, UPDATE_RETRY, UPDATE_STARTED,
    base_commit, changes_runtime, in_progress, job_pid, latest_job_step, record, retry_requested,
    step_commit,
};
use crate::domain::{AskKind, RunEvent, UPDATE_FAILED_OPTIONS};

/// How often the supervisor looks at main for an update by default.
pub const UPDATE_INTERVAL: Duration = Duration::from_secs(30);

/// How a supervisor updates its own binary (ADR-0045 decision 17).
#[derive(Debug, Clone)]
pub struct UpdateSettings {
    /// Register with the automatic update on (`supervise --auto-update`,
    /// which `up --auto-update` starts); `up` turns it on and off on a
    /// registered supervisor too.
    pub register: bool,
    /// Least time between two looks at main.
    pub interval: Duration,
    /// A shell command the job runs in place of `cargo build --release
    /// --locked` (tests).
    pub build_command: Option<String>,
    /// The cmux the job's `up` uses when it starts an in-cmux supervisor
    /// again.
    pub cmux: Option<PathBuf>,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            register: false,
            interval: UPDATE_INTERVAL,
            build_command: None,
            cmux: None,
        }
    }
}

/// What the supervisor remembers between its looks at main.
#[derive(Default)]
pub(super) struct UpdateWatch {
    last_check: Option<Instant>,
    /// The job this process started, reaped once it exits.
    job: Option<Box<dyn Spawned>>,
    /// A head already found to change nothing of the runtime.
    seen: Option<String>,
    /// Main when this process first looked: the base of a build that names
    /// no commit (a release, or `+unknown`) and has updated nothing yet.
    first_head: Option<String>,
}

impl Supervisor<'_> {
    /// One look at main for the automatic update; what fails is logged and
    /// looked at again on the next pass.
    pub(super) fn auto_update_pass(&mut self, options: &LoopSettings) {
        if let Err(error) = self.auto_update(options) {
            warn!(error = %format_args!("{error:#}"), "automatic update: {error:#}");
        }
    }

    fn auto_update(&mut self, options: &LoopSettings) -> Result<()> {
        if let Some(job) = self.update.job.as_mut()
            && let Some(exit) = job.try_wait()?
        {
            info!("automatic update job exited: {exit}");
            self.update.job = None;
        }
        if self
            .update
            .last_check
            .is_some_and(|last| last.elapsed() < options.update.interval)
        {
            return Ok(());
        }
        self.update.last_check = Some(Instant::now());
        let enabled = self
            .queue
            .supervisors()?
            .iter()
            .any(|registration| registration.token == self.token && registration.auto_update);
        if !enabled {
            return Ok(());
        }
        self.apply_update_answers()?;
        if self.update.job.is_some() {
            return Ok(());
        }
        let updates = self.queue.update_events(50)?;
        if let Some(step) = latest_job_step(&updates) {
            // A job this process started before it exec'd is its child
            // with no other reaper: collect it once it ended.
            if let Some(pid) = job_pid(step) {
                self.processes.reap(pid);
            }
            if in_progress(step, &*self.processes) {
                return Ok(());
            }
            if JOB_STEPS.contains(&step.kind.as_str()) {
                return self.job_interrupted(step);
            }
        }
        let head = self.repository.main_head()?.to_string();
        let first = self
            .update
            .first_head
            .get_or_insert_with(|| head.clone())
            .clone();
        let base = base_commit(&updates, &self.layout.version, Some(&first));
        if !retry_requested(&updates) {
            if base.as_deref() == Some(head.as_str())
                || self.update.seen.as_deref() == Some(head.as_str())
            {
                return Ok(());
            }
            let changes = match &base {
                Some(base) => match self.repository.changed_paths(base, &head) {
                    Ok(paths) => changes_runtime(&paths),
                    // A base Git does not know (a build of another clone):
                    // build main's head once, which becomes the base.
                    Err(error) => {
                        warn!(error = %format_args!("{error:#}"), "automatic update: what changed since {base} is unknown, so main's {head} is built: {error:#}");
                        true
                    }
                },
                None => true,
            };
            if !changes {
                self.update.seen = Some(head);
                return Ok(());
            }
        }
        self.start_update_job(&head, base.as_deref(), options)
    }

    /// A job that died before it recorded how it ended (killed, a reboot):
    /// record `update_failed` for its commit and ask the inbox, so the
    /// update is neither lost nor retried on its own.
    fn job_interrupted(&mut self, step: &RunEvent) -> Result<()> {
        let commit = step_commit(step).unwrap_or_default().to_owned();
        let question = format!(
            "The automatic update's job for main's {} (pid {}) ended without recording how, at \
its {}; nothing tells whether the binary was replaced. Its logs are in the queue's logs/ \
directory. Answer `retry` to build main's head again at the supervisor's next check, or `skip` \
to wait for the next landing that changes the runtime.",
            &commit[..commit.len().min(12)],
            job_pid(step).unwrap_or_default(),
            step.kind
        );
        let ask = self.queue.open_update_ask(
            AskKind::UpdateFailed,
            &question,
            UPDATE_FAILED_OPTIONS,
            UPDATE_ASKER,
        )?;
        record(
            &*self.queue,
            UPDATE_FAILED,
            step_commit(step),
            json!({"stage": "interrupted", "after": step.kind, "ask_id": ask.id, "supervisor": self.token}),
        )?;
        warn!(
            "automatic update: the job for {commit} was interrupted; ask {} opened",
            ask.id
        );
        Ok(())
    }

    /// Close the answered `update_failed` asks: `retry` asks the next look
    /// to build main's head again, `skip` waits for the next landing. Any
    /// other answer is left for the inbox to read.
    fn apply_update_answers(&mut self) -> Result<()> {
        for ask in self.queue.update_answers(&AskKind::UpdateFailed)? {
            let answer = ask.answer.as_deref().map(str::trim).unwrap_or_default();
            let kind = match answer {
                "retry" => UPDATE_RETRY,
                "skip" => UPDATE_ANSWERED,
                _ => continue,
            };
            record(
                &*self.queue,
                kind,
                None,
                json!({"ask_id": ask.id, "answer": answer, "supervisor": self.token}),
            )?;
            self.queue.close_ask(ask.id)?;
            info!(ask_id = %ask.id, "automatic update: ask {} answered {answer}", ask.id);
        }
        Ok(())
    }

    /// Start the update job for main's `head` in a session of its own, its
    /// output in the queue's `logs/`, and record `update_started`.
    fn start_update_job(
        &mut self,
        head: &str,
        base: Option<&str>,
        options: &LoopSettings,
    ) -> Result<()> {
        let layout = self.layout;
        let queue_dir = layout.db.parent().unwrap_or(Path::new("."));
        let logs = queue_dir.join("logs");
        self.files
            .create_dir_all(&logs)
            .with_context(|| format!("create {}", logs.display()))?;
        let name = format!(
            "update-{}-{}",
            self.generators.clock.now(),
            &head[..head.len().min(12)]
        );
        let (log, build_log, report) = (
            logs.join(format!("{name}.log")),
            logs.join(format!("{name}.build.log")),
            logs.join(format!("{name}.json")),
        );
        let mut command = CommandSpec::new(&layout.runner);
        command
            .arg("--db")
            .arg(&layout.db)
            .arg("auto-update")
            .args(["--commit", head, "--token", &self.token])
            .arg("--to")
            .arg(&layout.runner)
            .arg("--repo")
            .arg(&layout.repo_root)
            .arg("--log")
            .arg(&build_log)
            .arg("--claude")
            .arg(&layout.claude)
            .current_dir(&layout.repo_root)
            .new_session();
        if let Some(cmux) = &options.update.cmux {
            command.arg("--cmux").arg(cmux);
        }
        if let Some(dir) = &layout.plugin_dir {
            command.arg("--plugin-dir").arg(dir);
        }
        if let Some(build) = &options.update.build_command {
            command.arg("--build-command").arg(build);
        }
        let job = self.spawner.spawn(
            &command,
            Streams::Files {
                stdout: &report,
                stderr: &log,
            },
        )?;
        record(
            &*self.queue,
            UPDATE_STARTED,
            Some(head),
            json!({
                "pid": job.id(),
                "base": base,
                "supervisor": self.token,
                "version": layout.version,
                "log": log,
                "build_log": build_log,
                "report": report,
            }),
        )?;
        info!(
            "automatic update: main's {head} changes the runtime since {}; job {} builds it (log {})",
            base.unwrap_or("(none)"),
            job.id(),
            log.display()
        );
        self.update.job = Some(job);
        Ok(())
    }
}
