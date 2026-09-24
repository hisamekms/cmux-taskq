//! The session wrapper (`session`): the process cmux starts in a run's
//! workspace. It waits for the supervisor to record the workspace,
//! registers itself under the run's lease, starts the agent through the
//! [`Spawner`] with the run's prompt, heartbeats while the agent runs and
//! records its exit (ADR-0007). `resume` reopens the session of a
//! `needs_session` run instead (ADR-0019).

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

use super::{AgentProvider, Queue, RunFiles, Spawner, Streams};
use crate::domain::{RunId, TaskRun};

/// How long the wrapper waits for the supervisor to record the workspace
/// cmux started it in.
const WORKSPACE_REGISTRATION: Duration = Duration::from_secs(45);

/// What the wrapper works with: the queue, the agent, how it is started,
/// the run's files, and this process's pid.
pub struct Session<'a> {
    pub queue: &'a mut dyn Queue,
    pub provider: &'a dyn AgentProvider,
    pub spawner: &'a dyn Spawner,
    pub files: &'a dyn RunFiles,
    pub pid: u32,
}

/// Wrap the agent of run `id` under the lease `token` until it exits; the
/// agent's exit code. A failure is recorded as the run's runtime error,
/// and the wrapper's exit too unless the agent may still be running.
pub fn run_session(ctx: Session<'_>, id: &RunId, token: &str, resume: bool) -> Result<Value> {
    let Session {
        queue,
        provider,
        spawner,
        files,
        pid,
    } = ctx;
    let started = Instant::now();
    // cmux may start this command before its create response reaches
    // supervisor. A resumed run keeps the workspace of its first session.
    let run = loop {
        let run = queue.run(id)?;
        if run.workspace_id().is_some() {
            break run;
        }
        ensure!(
            started.elapsed() < WORKSPACE_REGISTRATION,
            "workspace registration timed out"
        );
        thread::sleep(Duration::from_millis(100));
    };
    if resume {
        queue.register_resume_wrapper(id, token, pid)?;
    } else {
        queue.register_wrapper(id, token, pid)?;
    }
    let mut child_may_be_alive = false;
    let result = drive_agent(
        queue,
        &run,
        provider,
        spawner,
        files,
        pid,
        resume,
        &mut child_may_be_alive,
    );
    match result {
        Ok(code) => {
            queue.wrapper_exited(id, pid, code)?;
            Ok(json!({"run_id": id, "exit_code": code}))
        }
        Err(error) => {
            let _ = queue.record_runtime_error(id, &format!("{error:#}"));
            if !child_may_be_alive {
                let _ = queue.wrapper_exited(id, pid, 127);
            }
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn drive_agent(
    queue: &mut dyn Queue,
    run: &TaskRun,
    provider: &dyn AgentProvider,
    spawner: &dyn Spawner,
    files: &dyn RunFiles,
    pid: u32,
    resume: bool,
    child_may_be_alive: &mut bool,
) -> Result<i32> {
    let command = if resume {
        provider.resume_command(run)?
    } else {
        let prompt_path =
            Path::new(run.run_dir().context("missing run directory")?).join("prompt.txt");
        provider.command(run, &files.read_to_string(&prompt_path)?)?
    };
    let mut child = spawner
        .spawn(&command, Streams::Inherit)
        .context("launch agent")?;
    *child_may_be_alive = true;
    let registered = if resume {
        queue.register_resume_agent(run.id(), pid, child.id())
    } else {
        queue.register_agent(run.id(), pid, child.id())
    };
    if let Err(error) = registered {
        let _ = child.kill();
        if child.wait().is_ok() {
            *child_may_be_alive = false;
        }
        return Err(error);
    }
    loop {
        if let Some(status) = child.try_wait()? {
            *child_may_be_alive = false;
            return Ok(status.code.unwrap_or(128));
        }
        if let Err(error) = queue.heartbeat_wrapper(run.id(), pid) {
            // Keep owning/waiting on the existing child even during a DB outage.
            tracing::warn!(
                run_id = %run.id(),
                error = %format_args!("{error:#}"),
                "wrapper heartbeat failed: {error:#}"
            );
        }
        thread::sleep(provider.wait_interval());
    }
}
