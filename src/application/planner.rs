//! Planner sessions (ADR-0041 decisions 1, 6, 12, 13): on-demand cmux
//! workspaces where a planner writes goals and tasks and submits them as a
//! proposal. A person opens one with `dagq plan` (as many as they like at
//! once); the runtime opens one for a proposal plan review sent back while
//! its own planner was closed ([`open_runtime_planner`]). Each is a
//! `planners` row with its own directory under the queue's `planners/`
//! (its prompt, the wrapper binary, the agent's settings, log and idle
//! marker).
//!
//! The workspace runs the session wrapper `planner-session`, which starts
//! the agent, registers itself and its agent, heartbeats and records the
//! agent's exit, the way a run's wrapper does; the agent's `Stop` hook
//! writes the idle marker. [`planner_view`] judges from these whether the
//! planner is alive and idle.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    thread,
};
use tracing::warn;

use super::{
    AgentProvider, AgentSignals, Clock, PlannerCommand, ProcessControl, Queue, RunFiles, Spawner,
    Streams, WorkspaceBackend,
    lifecycle::{
        PLANNER_ID_ENV, PLANNER_ORIGIN_ENV, QueueWorkspaces, ROLE_STATUS_KEY, session_look,
    },
    naming::{planner_workspace_name, shell_join},
    path_text, planner_idle_marker,
    prompt::{planner_prompt, runtime_planner_prompt},
};
use crate::domain::{
    IdleProbe, PlannerId, PlannerOrigin, PlannerProbe, PlannerSession, PlannerState, ProposalId,
    SessionRole, Task,
};

/// The planner's first message, which its wrapper hands the agent.
pub const PLANNER_PROMPT_FILE: &str = "prompt.txt";
/// The snapshot of the binary the planner's workspace runs as its wrapper,
/// so rebuilding the binary does not change a running one.
pub const PLANNER_RUNNER_FILE: &str = "runner";

/// The directory of planner `id` under the queue's `planners/` directory.
pub fn planner_dir(planners_dir: &Path, id: PlannerId) -> PathBuf {
    planners_dir.join(id.to_string())
}

/// What opening a planner works with: the queue and cmux, the files, the
/// queue's database, hash and `planners/` directory, the checkout the
/// planner works in, and what its workspace runs (this binary as the
/// wrapper, the agent's executable, the plugin directory it loads).
pub struct PlannerLaunch<'a> {
    pub queue: &'a dyn Queue,
    pub cmux: &'a dyn WorkspaceBackend,
    pub files: &'a dyn RunFiles,
    pub db: &'a Path,
    pub queue_hash: &'a str,
    pub planners_dir: &'a Path,
    pub repo_root: &'a Path,
    pub runner: &'a Path,
    pub claude: &'a Path,
    pub plugin_dir: Option<&'a Path>,
}

/// A planner whose workspace just opened: its record, the workspace's
/// title, its directory, and what cmux refused about its look.
#[derive(Debug, Clone, Serialize)]
pub struct OpenedPlanner {
    pub planner: PlannerSession,
    pub name: String,
    pub dir: PathBuf,
    pub warnings: Vec<String>,
}

/// Open a planner a person talks with (`dagq plan`): a new workspace every
/// time, next to any planner already open.
pub fn open_person_planner(launch: &PlannerLaunch<'_>) -> Result<OpenedPlanner> {
    open_planner(
        launch,
        PlannerOrigin::Person,
        None,
        &planner_prompt(launch.db)?,
    )
}

/// Open a planner of the runtime's for `proposal` (ADR-0041 decision 12):
/// its initial prompt carries the proposal, its `tasks` (as the caller read
/// them) and the `reasons` plan review sent it back with. It submits as the
/// runtime's planner (`DAGQ_PLANNER_ORIGIN=runtime`), which makes it the
/// proposal's owner.
pub fn open_runtime_planner(
    launch: &PlannerLaunch<'_>,
    proposal: ProposalId,
    tasks: &[Task],
    reasons: &[String],
) -> Result<OpenedPlanner> {
    // The proposal must exist; its record is the one the planner opens for.
    launch.queue.show_proposal(proposal)?;
    let prompt = runtime_planner_prompt(launch.db, proposal, tasks, reasons)?;
    open_planner(launch, PlannerOrigin::Runtime, Some(proposal), &prompt)
}

/// Record a planner, write its directory and open its workspace running
/// the wrapper, with `DAGQ_ROLE=planner`, `DAGQ_QUEUE`, the origin and the
/// planner's ID in the workspace's environment and the queue's group
/// (ADR-0026). A workspace cmux does not open closes the record with the
/// error. The look (Blue, the `map` pill) is a warning when refused; a
/// planner is not pinned, being on demand.
pub fn open_planner(
    launch: &PlannerLaunch<'_>,
    origin: PlannerOrigin,
    proposal: Option<ProposalId>,
    prompt: &str,
) -> Result<OpenedPlanner> {
    let queue = launch.queue;
    let planner = queue.open_planner(origin, proposal)?;
    let dir = planner_dir(launch.planners_dir, planner.id);
    let workspaces = QueueWorkspaces::new(
        launch.cmux,
        launch.db,
        launch.queue_hash.to_owned(),
        launch.repo_root,
    );
    let name = planner_workspace_name(launch.repo_root, planner.id, proposal);
    let opened = create_workspace(launch, &workspaces, &planner, &dir, &name, prompt);
    let workspace_id = match opened {
        Ok(id) => id,
        Err(error) => {
            queue.close_planner(planner.id, Some(&format!("{error:#}")))?;
            return Err(error);
        }
    };
    queue.planner_workspace_created(planner.id, &workspace_id)?;
    let mut warnings = workspaces.take_warnings();
    if let Some((color, icon)) = session_look(SessionRole::Planner) {
        for (what, result) in [
            ("color", launch.cmux.set_color(&workspace_id, color)),
            (
                "status pill",
                launch.cmux.set_status(
                    &workspace_id,
                    ROLE_STATUS_KEY,
                    SessionRole::Planner.as_str(),
                    icon,
                ),
            ),
        ] {
            if let Err(error) = result {
                warnings.push(format!(
                    "cmux could not set the {what} of the planner workspace {workspace_id}: {error:#}"
                ));
            }
        }
    }
    Ok(OpenedPlanner {
        planner: queue.planner(planner.id)?,
        name,
        dir,
        warnings,
    })
}

fn create_workspace(
    launch: &PlannerLaunch<'_>,
    workspaces: &QueueWorkspaces<'_>,
    planner: &PlannerSession,
    dir: &Path,
    name: &str,
    prompt: &str,
) -> Result<String> {
    let files = launch.files;
    files
        .create_dir_all(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    files.write(&dir.join(PLANNER_PROMPT_FILE), prompt.as_bytes())?;
    let runner = dir.join(PLANNER_RUNNER_FILE);
    files
        .copy(launch.runner, &runner)
        .context("snapshot runtime binary")?;
    let mut argv = vec![
        path_text(&runner)?,
        "--db".into(),
        path_text(launch.db)?,
        "planner-session".into(),
        "--planner".into(),
        planner.id.to_string(),
        "--claude".into(),
        path_text(launch.claude)?,
    ];
    if let Some(plugin_dir) = launch.plugin_dir {
        argv.push("--plugin-dir".into());
        argv.push(path_text(plugin_dir)?);
    }
    let mut tags = workspaces.tags(SessionRole::Planner)?;
    tags.env.push((
        PLANNER_ORIGIN_ENV.to_owned(),
        planner.origin.as_str().to_owned(),
    ));
    tags.env
        .push((PLANNER_ID_ENV.to_owned(), planner.id.to_string()));
    if let Some(description) = &mut tags.description {
        description.push_str(&format!(" planner={}", planner.id));
    }
    launch
        .cmux
        .create_named(name, launch.repo_root, &shell_join(&argv), &tags)
}

/// What the planner's session wrapper works with, as the run's does.
pub struct PlannerWrapper<'a> {
    pub queue: &'a dyn Queue,
    pub provider: &'a dyn AgentProvider,
    pub spawner: &'a dyn Spawner,
    pub files: &'a dyn RunFiles,
    pub pid: u32,
}

/// The session wrapper of planner `id` (`planner-session`): register this
/// process, start the agent with the planner's prompt in `cwd`, register
/// it, heartbeat until it exits, and record its exit code, which it returns.
pub fn run_planner_session(
    ctx: PlannerWrapper<'_>,
    id: PlannerId,
    dir: &Path,
    cwd: &Path,
    plugin_dir: Option<&Path>,
) -> Result<Value> {
    let PlannerWrapper {
        queue,
        provider,
        spawner,
        files,
        pid,
    } = ctx;
    queue.register_planner_wrapper(id, pid)?;
    // An agent that never starts is an exit too, or the planner would look
    // lost rather than over.
    let started = files
        .read_to_string(&dir.join(PLANNER_PROMPT_FILE))
        .map_err(anyhow::Error::from)
        .and_then(|prompt| {
            provider.planner_command(&PlannerCommand {
                dir,
                cwd,
                prompt: &prompt,
                plugin_dir,
            })
        })
        .and_then(|command| {
            spawner
                .spawn(&command, Streams::Inherit)
                .context("launch agent")
        });
    let mut child = match started {
        Ok(child) => child,
        Err(error) => {
            let _ = queue.planner_exited(id, pid, 127);
            return Err(error);
        }
    };
    if let Err(error) = queue.register_planner_agent(id, pid, child.id()) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = queue.planner_exited(id, pid, 127);
        return Err(error);
    }
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code.unwrap_or(128),
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = queue.planner_exited(id, pid, 127);
                return Err(error);
            }
        }
        if let Err(error) = queue.heartbeat_planner(id, pid) {
            // Keep waiting on the agent through a queue outage.
            warn!(planner_id = %id, error = %format_args!("{error:#}"), "planner heartbeat failed: {error:#}");
        }
        thread::sleep(provider.wait_interval());
    };
    queue.planner_exited(id, pid, code)?;
    Ok(json!({"planner_id": id, "exit_code": code}))
}

/// A planner with how it stands now: its state, whether it is alive, and
/// since when its agent is idle (Unix seconds of its idle marker, while
/// the state is `idle`).
#[derive(Debug, Clone, Serialize)]
pub struct PlannerView {
    #[serde(flatten)]
    pub planner: PlannerSession,
    pub state: PlannerState,
    pub alive: bool,
    pub idle_since: Option<i64>,
    pub dir: PathBuf,
}

/// What judging a planner reads: cmux for its workspace and screen, the
/// processes for its wrapper, the files for its idle marker, the agent's
/// signals for the marker and the screen, and the clock.
pub struct PlannerProbes<'a> {
    pub cmux: &'a dyn WorkspaceBackend,
    pub processes: &'a dyn ProcessControl,
    pub files: &'a dyn RunFiles,
    pub signals: &'a dyn AgentSignals,
    pub clock: &'a dyn Clock,
    pub planners_dir: &'a Path,
}

/// Judge `planner` the way a worker session is judged: its workspace UUID
/// in `cmux workspace list`, its wrapper's pid and heartbeat, the idle
/// marker its agent's `Stop` hook wrote and, with a marker, whether the
/// screen shows the agent at work on a new turn. A closed planner is not
/// looked at.
pub fn planner_view(probes: &PlannerProbes<'_>, planner: PlannerSession) -> Result<PlannerView> {
    let dir = planner_dir(probes.planners_dir, planner.id);
    let mut probe = PlannerProbe {
        now: probes.clock.now(),
        workspace_listed: false,
        wrapper_alive: false,
        idle: None,
        working: None,
    };
    if planner.closed_at.is_none() {
        if let Some(workspace) = &planner.workspace_id {
            probe.workspace_listed = probes.cmux.exists(workspace)?;
        }
        probe.wrapper_alive = planner
            .wrapper_pid
            .is_some_and(|pid| probes.processes.alive(pid));
        if let Some((modified, bytes)) = probes.files.read_stamped(&planner_idle_marker(&dir))? {
            probe.idle = Some(IdleProbe {
                since: super::unix_seconds(modified),
                background_running: probes.signals.idle_hook(&bytes).background_running,
            });
            if probe.workspace_listed
                && let Some(workspace) = &planner.workspace_id
            {
                probe.working = probes
                    .cmux
                    .capture(workspace)
                    .ok()
                    .map(|screen| probes.signals.working(&screen));
            }
        }
    }
    let state = planner.state(&probe);
    Ok(PlannerView {
        state,
        alive: state.alive(),
        idle_since: probe
            .idle
            .filter(|_| state == PlannerState::Idle)
            .map(|idle| idle.since),
        dir,
        planner,
    })
}

/// Every planner not closed (with `all`, every planner), each judged by
/// [`planner_view`]. Nothing is written: `cmux workspace list` shows only
/// the caller's window, so a planner another window shows as `closed`
/// here is not given up in the queue on that evidence.
pub fn planner_views(
    queue: &dyn Queue,
    probes: &PlannerProbes<'_>,
    all: bool,
) -> Result<Vec<PlannerView>> {
    queue
        .planners(all)?
        .into_iter()
        .map(|planner| planner_view(probes, planner))
        .collect()
}
