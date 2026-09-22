//! `up` and `down`: the cold start and the stop of one queue's runtime. `up`
//! makes sure a supervisor is resident (as a launchd LaunchAgent, restarted
//! after any exit) and that the maintainer's Claude session has a cmux
//! workspace, and reports what the maintainer should look at first. `down`
//! unloads the agent so the supervisor drains and is not restarted. Both
//! are idempotent: a second `up` reuses what the first one started.
//!
//! A launchd-started supervisor is not a child of a cmux terminal, and cmux
//! admits such a process only by socket password. `up` therefore proves the
//! connection from outside cmux (a `ping` with the agent's environment, run
//! outside cmux's process tree) before it writes the agent, or launchd would
//! keep restarting a supervisor that fails its own preflight forever. Where
//! that password is not configured, `up --in-cmux` starts the supervisor
//! inside the cmux workspace `taskq <repo> supervisor` instead, with no
//! launchd involved and so nothing to restart it (ADR-0011).
use crate::{
    application::{
        AgentProvider, DetachedRefusal, LaunchAgent, ProcessControl, SupervisorEnvironment,
        WorkspaceBackend,
    },
    domain::{RunStatus, SupervisorMode, SupervisorRegistration},
    infrastructure::{
        adapters::{
            ClaudeCode, GitRepository, maintainer_workspace_name, path_text, shell_join,
            supervisor_workspace_name,
        },
        launchd::LaunchAgentSpec,
        location::QueueLocation,
        runtime_store::HEARTBEAT_TIMEOUT_SECS,
        sqlite::SqliteQueue,
    },
    runtime::{maintainer_prompt, unix_time},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

/// Set in the maintainer workspace's command so that `up`, run from inside
/// that session (the plugin skill calls it), does not open a second one.
pub const ROLE_ENV: &str = "CMUX_TASKQ_ROLE";
/// The queue database the maintainer session belongs to.
pub const QUEUE_ENV: &str = "CMUX_TASKQ_QUEUE";
pub const MAINTAINER_ROLE: &str = "maintainer";
/// File under the queue's log directory that launchd appends the
/// supervisor's stdout and stderr to.
pub const LAUNCHD_LOG_NAME: &str = "launchd.log";

/// What `up` reads from the process that runs it.
#[derive(Debug, Clone)]
pub struct UpEnvironment {
    /// `CMUX_TASKQ_ROLE`, if set.
    pub role: Option<String>,
    /// `CMUX_TASKQ_QUEUE`, if set.
    pub queue: Option<PathBuf>,
    /// `PATH`, copied into the agent so the supervisor finds what this shell finds.
    pub path: String,
    /// `CMUX_SOCKET_PASSWORD`, if this shell exported it (non-empty); it is
    /// then copied into the agent too.
    pub socket_password: Option<String>,
    /// The binary launchd runs: this one, by absolute path.
    pub current_exe: PathBuf,
}

/// What `up` says when cmux does not admit a process from outside its
/// terminals. The remedies are the operator's: cmux's CLI takes the password
/// saved in its Settings on its own, or `CMUX_SOCKET_PASSWORD` from the
/// shell that runs `up` (stored in the agent then).
pub const DETACHED_CMUX_HINT: &str = "cmux refused a connection from outside its own terminals, \
so the supervisor launchd starts could not reach it. Either save a socket password in cmux \
Settings (its CLI uses it on its own), or export CMUX_SOCKET_PASSWORD before `up` (it is then \
written into the LaunchAgent); or run `up --in-cmux`, which starts the supervisor inside a cmux \
workspace without launchd and without any automatic restart";

#[derive(Debug, Clone)]
pub struct UpOptions {
    pub parallel: u16,
    /// Start the supervisor inside the cmux workspace `taskq <repo>
    /// supervisor` instead of as a LaunchAgent: no launchd, no automatic
    /// restart, and no out-of-cmux preflight to pass.
    pub in_cmux: bool,
    /// Passed to the maintainer's `claude` as `--plugin-dir`.
    pub plugin_dir: Option<PathBuf>,
    /// Resolved executables; the agent runs the supervisor with these, and
    /// the maintainer workspace starts this `claude`.
    pub cmux: PathBuf,
    pub claude: PathBuf,
    /// How long a started supervisor may take to register before `up` fails.
    pub startup_timeout: Duration,
    pub poll: Duration,
}

/// Ensure the supervisor and the maintainer workspace exist and report the
/// queue's open work. Preflight first (cmux, claude, an initialized queue,
/// the repository), then prune registrations whose process is gone, start
/// the agent only when no live registration remains (after proving that
/// cmux admits a process with the agent's environment), and open the
/// maintainer workspace only outside a maintainer session.
pub fn up(
    location: &QueueLocation,
    repo: &Path,
    cmux: &dyn WorkspaceBackend,
    launchd: &dyn LaunchAgent,
    processes: &dyn ProcessControl,
    environment: &UpEnvironment,
    options: &UpOptions,
) -> Result<Value> {
    ensure!(options.parallel >= 1, "parallel must be at least 1");
    let db = location
        .db
        .canonicalize()
        .context("queue must already be initialized")?;
    let repository = GitRepository::inspect(repo)?;
    cmux.preflight()?;
    ClaudeCode {
        executable: options.claude.clone(),
    }
    .preflight()?;
    let plugin_dir = options
        .plugin_dir
        .as_deref()
        .map(|dir| {
            dir.canonicalize()
                .with_context(|| format!("plugin directory {}", dir.display()))
        })
        .transpose()?;
    let queue = SqliteQueue::open(&db)?;

    let mut pruned = Vec::new();
    let mut live = Vec::new();
    // Registrations that survive this pass are not the supervisor `up` is
    // about to start, so the wait below must not mistake one for it.
    let mut existing = HashSet::new();
    for registration in queue.supervisors()? {
        if !processes.alive(registration.pid) {
            queue.deregister_supervisor(&registration.token)?;
            pruned.push(json!({"token": registration.token, "pid": registration.pid}));
            continue;
        }
        existing.insert(registration.token.clone());
        if fresh(&registration, processes) {
            live.push(registration);
        }
        // Alive but silent: not ours to kill; it shows up as stale in doctor.
    }

    let supervisor = match live.first() {
        // Whoever started it recorded the mode; a supervisor started by
        // hand has none, and `up` does not claim one for it.
        Some(registration) => json!({
            "outcome": "reused",
            "mode": registration.mode.map(SupervisorMode::as_str),
            "pid": registration.pid,
            "token": registration.token,
            "workspace_id": registration.workspace_id,
            "plist": location.launch_agent,
            "log_dir": location.log_dir,
        }),
        None if options.in_cmux => start_in_cmux(
            location,
            &db,
            &repository,
            &queue,
            cmux,
            processes,
            environment,
            options,
            &existing,
        )?,
        None => start_under_launchd(
            location,
            &db,
            &repository,
            &queue,
            cmux,
            launchd,
            processes,
            environment,
            options,
            &existing,
        )?,
    };

    let name = maintainer_workspace_name(&repository.root);
    let inside_maintainer = environment.role.as_deref() == Some(MAINTAINER_ROLE)
        && environment
            .queue
            .as_deref()
            .and_then(|queue| queue.canonicalize().ok())
            .is_some_and(|queue| queue == db);
    let maintainer = if inside_maintainer {
        json!({"outcome": "skipped", "workspace_id": Value::Null, "name": name})
    } else if let Some(id) = cmux.find_named(&name)? {
        json!({"outcome": "reused", "workspace_id": id, "name": name})
    } else {
        let command = maintainer_command(
            &db,
            &location.log_dir,
            &options.claude,
            plugin_dir.as_deref(),
        )?;
        let id = cmux.create_named(&name, &repository.root, &command)?;
        json!({"outcome": "created", "workspace_id": id, "name": name})
    };

    Ok(json!({
        "supervisor": supervisor,
        "maintainer": maintainer,
        "pruned_supervisors": pruned,
        "doctor": open_work(&queue, processes)?,
    }))
}

/// Write and load the LaunchAgent, after proving that cmux admits a
/// process carrying its environment from outside cmux's process tree. A
/// refusal stops `up` before launchd ever sees the agent, because
/// `KeepAlive` would otherwise restart a supervisor that cannot work.
#[allow(clippy::too_many_arguments)]
fn start_under_launchd(
    location: &QueueLocation,
    db: &Path,
    repository: &GitRepository,
    queue: &SqliteQueue,
    cmux: &dyn WorkspaceBackend,
    launchd: &dyn LaunchAgent,
    processes: &dyn ProcessControl,
    environment: &UpEnvironment,
    options: &UpOptions,
    existing: &HashSet<String>,
) -> Result<Value> {
    let spec = launch_agent_spec(location, db, repository, environment, options)?;
    if let Err(error) = cmux.preflight_detached(&spec.environment) {
        // Only a refusal has the password (or `--in-cmux`) as its remedy; a
        // ping that could not be run or did not answer is its own error.
        return Err(if error.is::<DetachedRefusal>() {
            error.context(DETACHED_CMUX_HINT)
        } else {
            error.context(
                "cmux could not be asked whether it admits a connection from outside its own terminals",
            )
        });
    }
    launchd.install(&spec.label, &spec.plist, &spec.xml())?;
    let registration =
        wait_for_registration(queue, processes, options, existing).with_context(|| {
            format!(
                "supervisor did not register within {}s; see {}",
                options.startup_timeout.as_secs(),
                spec.log
            )
        })?;
    queue.set_supervisor_mode(&registration.token, SupervisorMode::Launchd, None)?;
    Ok(json!({
        "outcome": "started",
        "mode": SupervisorMode::Launchd.as_str(),
        "pid": registration.pid,
        "token": registration.token,
        "workspace_id": Value::Null,
        "plist": spec.plist,
        "log_dir": location.log_dir,
    }))
}

/// Run `supervise` inside its own cmux workspace instead: nothing about
/// launchd is touched, and the supervisor is a child of a cmux terminal, so
/// no socket password is needed. Nothing restarts it either.
///
/// cmux keeps a workspace open after its command exits, so a leftover
/// `taskq <repo> supervisor` may belong to a supervisor that crashed, or to
/// one that is alive but no longer heartbeating (which `up` never reuses
/// and never kills). Either way it is the maintainer's to close, and `up`
/// stops rather than open a second one or interfere with the first.
#[allow(clippy::too_many_arguments)]
fn start_in_cmux(
    location: &QueueLocation,
    db: &Path,
    repository: &GitRepository,
    queue: &SqliteQueue,
    cmux: &dyn WorkspaceBackend,
    processes: &dyn ProcessControl,
    environment: &UpEnvironment,
    options: &UpOptions,
    existing: &HashSet<String>,
) -> Result<Value> {
    let name = supervisor_workspace_name(&repository.root);
    if let Some(id) = cmux.find_named(&name)? {
        bail!(
            "cmux workspace {id} is already named {name:?} but no supervisor of this queue is \
registered and heartbeating; read its screen, then close it (`cmux workspace close {id}`) \
and run `up --in-cmux` again"
        );
    }
    let command = supervise_command(location, db, environment, options)?;
    let workspace_id = cmux.create_named(&name, &repository.root, &command)?;
    let registration =
        wait_for_registration(queue, processes, options, existing).with_context(|| {
            format!(
                "supervisor did not register within {}s; read workspace {workspace_id} ({name:?}) \
and close it before trying again",
                options.startup_timeout.as_secs(),
            )
        })?;
    queue
        .set_supervisor_mode(
            &registration.token,
            SupervisorMode::InCmux,
            Some(&workspace_id),
        )
        .with_context(|| {
            format!("supervisor started in workspace {workspace_id} ({name:?}); close it by hand")
        })?;
    Ok(json!({
        "outcome": "started",
        "mode": SupervisorMode::InCmux.as_str(),
        "pid": registration.pid,
        "token": registration.token,
        "workspace_id": workspace_id,
        "name": name,
        "plist": Value::Null,
        "log_dir": location.log_dir,
    }))
}

/// The supervisor workspace's command: this binary running `supervise` on
/// this queue, with the same arguments the LaunchAgent would carry. cmux
/// types it into a login shell, so every argument is quoted on its own. The
/// environment is the cmux terminal's, so nothing is set here.
pub fn supervise_command(
    location: &QueueLocation,
    db: &Path,
    environment: &UpEnvironment,
    options: &UpOptions,
) -> Result<String> {
    Ok(shell_join(&supervise_arguments(
        location,
        db,
        environment,
        options,
    )?))
}

/// `<this binary> --db <db> supervise --parallel N --log-dir <queue logs>
/// --cmux <resolved> --claude <resolved>`: what keeps a supervisor of this
/// queue going, in either mode. The executables are absolute so neither
/// launchd's PATH nor the terminal's decides which ones run.
fn supervise_arguments(
    location: &QueueLocation,
    db: &Path,
    environment: &UpEnvironment,
    options: &UpOptions,
) -> Result<Vec<String>> {
    Ok(vec![
        path_text(&environment.current_exe)?,
        "--db".into(),
        path_text(db)?,
        "supervise".into(),
        "--parallel".into(),
        options.parallel.to_string(),
        "--log-dir".into(),
        path_text(&location.log_dir)?,
        "--cmux".into(),
        path_text(&options.cmux)?,
        "--claude".into(),
        path_text(&options.claude)?,
    ])
}

fn fresh(registration: &SupervisorRegistration, processes: &dyn ProcessControl) -> bool {
    processes.alive(registration.pid)
        && unix_time() - registration.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS
}

/// The registration the supervisor `up` has just started writes for
/// itself. Tokens in `existing` were already registered when `up` looked,
/// so they belong to another process: one of them may start heartbeating
/// again mid-wait (an alive but silent supervisor `up` neither reuses nor
/// kills), and taking it for ours would stamp this start's mode and
/// workspace onto a supervisor that never ran in it.
fn wait_for_registration(
    queue: &SqliteQueue,
    processes: &dyn ProcessControl,
    options: &UpOptions,
    existing: &HashSet<String>,
) -> Result<SupervisorRegistration> {
    let deadline = Instant::now() + options.startup_timeout;
    loop {
        if let Some(registration) = queue.supervisors()?.into_iter().find(|registration| {
            !existing.contains(&registration.token) && fresh(registration, processes)
        }) {
            return Ok(registration);
        }
        ensure!(Instant::now() < deadline, "no live supervisor registration");
        thread::sleep(options.poll);
    }
}

/// The agent definition: this binary running `supervise` on this queue from
/// the repository root, with the caller's PATH (and exported socket
/// password) and the queue's log directory.
pub fn launch_agent_spec(
    location: &QueueLocation,
    db: &Path,
    repository: &GitRepository,
    environment: &UpEnvironment,
    options: &UpOptions,
) -> Result<LaunchAgentSpec> {
    Ok(LaunchAgentSpec {
        label: location.label.clone(),
        plist: location.launch_agent.clone(),
        program_arguments: supervise_arguments(location, db, environment, options)?,
        working_directory: path_text(&repository.root)?,
        environment: SupervisorEnvironment {
            path: environment.path.clone(),
            socket_password: environment.socket_password.clone(),
        },
        log: path_text(&location.log_dir.join(LAUNCHD_LOG_NAME))?,
    })
}

/// The maintainer workspace's command: `claude` with the role and queue in
/// its environment (so `up` from inside recognizes the session) and the
/// generated prompt as its first message.
pub fn maintainer_command(
    db: &Path,
    log_dir: &Path,
    claude: &Path,
    plugin_dir: Option<&Path>,
) -> Result<String> {
    let mut argv = vec![
        "env".to_owned(),
        format!("{ROLE_ENV}={MAINTAINER_ROLE}"),
        format!("{QUEUE_ENV}={}", path_text(db)?),
        path_text(claude)?,
    ];
    if let Some(dir) = plugin_dir {
        argv.push("--plugin-dir".into());
        argv.push(path_text(dir)?);
    }
    argv.push("--".into());
    argv.push(maintainer_prompt(db, log_dir)?);
    Ok(shell_join(&argv))
}

/// What the maintainer looks at first: unfinished runs with whether their
/// lease still has a live, heartbeating owner, and the runs that wait for
/// review (`awaiting_integration`) or a resumed session (`needs_session`).
fn open_work(queue: &SqliteQueue, processes: &dyn ProcessControl) -> Result<Value> {
    let now = unix_time();
    let leases = queue.run_leases()?;
    let unfinished: Vec<Value> = queue
        .active_runs()?
        .into_iter()
        .map(|run| {
            let lease_stale = leases.iter().find(|l| l.run_id == run.id).map(|lease| {
                !processes.alive(lease.pid) || now - lease.heartbeat_at > HEARTBEAT_TIMEOUT_SECS
            });
            json!({
                "run_id": run.id,
                "task_id": run.task_id,
                "status": run.status,
                "lease_stale": lease_stale,
            })
        })
        .collect();
    let brief = |status: RunStatus| -> Result<Vec<Value>> {
        Ok(queue
            .runs_with_status(status)?
            .into_iter()
            .map(|run| {
                json!({"run_id": run.id, "task_id": run.task_id, "last_error": run.last_error})
            })
            .collect())
    };
    Ok(json!({
        "unfinished_runs": unfinished,
        "awaiting_integration": brief(RunStatus::AwaitingIntegration)?,
        "needs_session": brief(RunStatus::NeedsSession)?,
    }))
}

#[derive(Debug, Clone)]
pub struct DownOptions {
    /// Block until the supervisor's registration is gone or its process died.
    pub wait: bool,
    /// SIGKILL after the unload and drop the registration rows.
    pub force: bool,
    pub poll: Duration,
}

/// Stop the queue's supervisors, whichever mode started them. The launchd
/// agent is always unloaded (that is what makes a `launchd` supervisor stop
/// without being restarted, and it also clears an agent left over from a
/// queue that has since moved to `--in-cmux`), which delivers SIGTERM to
/// the agent's own process; a supervisor started by hand gets the SIGTERM
/// from here, and an `in_cmux` one gets a SIGINT, the signal its terminal
/// would send. The runtime drains on either.
///
/// The cmux workspace of an `in_cmux` supervisor is closed once that
/// supervisor's process is gone: after the drain under `--wait`, after the
/// kill under `--force`, or straight away when it had already exited.
/// Closing it earlier would cut the drain short, so the default (which
/// returns while the supervisor drains) leaves it open and says so; the
/// maintainer closes it or runs `down --wait`. The maintainer workspace is
/// never touched.
pub fn down(
    location: &QueueLocation,
    cmux: &dyn WorkspaceBackend,
    launchd: &dyn LaunchAgent,
    processes: &dyn ProcessControl,
    options: &DownOptions,
) -> Result<Value> {
    let queue = SqliteQueue::open(&location.db)?;
    // Every registration is considered for the workspace close, whichever
    // path this `down` takes: the rule is the same for all of them, and a
    // queue can hold supervisors of both modes at once.
    let registrations = queue.supervisors()?;
    let (live, dead): (Vec<SupervisorRegistration>, Vec<SupervisorRegistration>) = registrations
        .iter()
        .cloned()
        .partition(|registration| processes.alive(registration.pid));
    // An agent whose supervisor never registered (a crash loop) is still
    // unloaded, or it would keep restarting.
    let agent = launchd.uninstall(&location.label, &location.launch_agent)?;
    let unloaded = agent.loaded;
    if live.is_empty() {
        let mut pruned = Vec::new();
        if options.force {
            for registration in &dead {
                queue.deregister_supervisor(&registration.token)?;
                pruned.push(json!({"token": registration.token, "pid": registration.pid}));
            }
        }
        return Ok(json!({
            "outcome": "not_running",
            "launch_agent_unloaded": unloaded,
            "pruned_supervisors": pruned,
            "supervisor_workspaces": close_supervisor_workspaces(
                cmux,
                processes,
                &registrations,
                // Nothing is alive to drain, so every workspace is ours.
                Stop::SeenThrough,
            ),
        }));
    }
    // launchd's bootout delivers the SIGTERM to the agent's own process; a
    // supervisor started by hand gets it from here. A second SIGTERM would
    // end a draining supervisor at once, so when the agent is loaded but
    // its pid is unknown nothing is signalled.
    for registration in &live {
        if registration.mode == Some(SupervisorMode::InCmux) {
            processes.interrupt(registration.pid)?;
        } else if (!agent.loaded || agent.pid.is_some()) && Some(registration.pid) != agent.pid {
            processes.terminate(registration.pid)?;
        }
    }
    let pids: Vec<u32> = live.iter().map(|r| r.pid).collect();
    let pid = pids[0];
    if options.force {
        for registration in &live {
            processes.kill(registration.pid)?;
            queue.deregister_supervisor(&registration.token)?;
        }
        // The dead ones go too, so no row is left pointing at a workspace
        // this call has just closed.
        let mut pruned = Vec::new();
        for registration in &dead {
            queue.deregister_supervisor(&registration.token)?;
            pruned.push(json!({"token": registration.token, "pid": registration.pid}));
        }
        return Ok(json!({
            "outcome": "killed",
            "pid": pid,
            "pids": pids,
            "launch_agent_unloaded": unloaded,
            "pruned_supervisors": pruned,
            "supervisor_workspaces": close_supervisor_workspaces(
                cmux,
                processes,
                &registrations,
                Stop::SeenThrough,
            ),
        }));
    }
    if options.wait {
        loop {
            let remaining = queue
                .supervisors()?
                .into_iter()
                .filter(|registration| {
                    live.iter().any(|l| l.token == registration.token)
                        && processes.alive(registration.pid)
                })
                .count();
            if remaining == 0 {
                break;
            }
            thread::sleep(options.poll);
        }
        return Ok(json!({
            "outcome": "stopped",
            "pid": pid,
            "pids": pids,
            "launch_agent_unloaded": unloaded,
            "supervisor_workspaces": close_supervisor_workspaces(
                cmux,
                processes,
                &registrations,
                Stop::SeenThrough,
            ),
        }));
    }
    Ok(json!({
        "outcome": "draining",
        "pid": pid,
        "pids": pids,
        "launch_agent_unloaded": unloaded,
        "supervisor_workspaces": close_supervisor_workspaces(
            cmux,
            processes,
            &registrations,
            Stop::Pending,
        ),
    }))
}

/// Whether this `down` saw the stop through, which decides what may be
/// done to an `in_cmux` supervisor's workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// `--wait` waited for the drain, or `--force` killed it: the
    /// supervisor is not going to do any more work, so its workspace is
    /// closed whatever its pid still looks like.
    SeenThrough,
    /// The default `down` returned while the supervisor drains. Closing
    /// the workspace would end that drain, so only one that had already
    /// exited before `down` ran is closed.
    Pending,
}

/// Close the cmux workspace of every `in_cmux` registration this `down` is
/// done with, and report the ones left to a running drain. A close that
/// cmux refuses is reported, not raised: the supervisor is already
/// stopped, which is what `down` was asked to do.
///
/// Liveness is only consulted in the `Pending` case, and there it is read
/// before anything was signalled. After a SIGKILL it would be useless:
/// `kill(2)` returns before the target is reaped, so `kill(pid, 0)` still
/// succeeds for a process that is already dying.
fn close_supervisor_workspaces(
    cmux: &dyn WorkspaceBackend,
    processes: &dyn ProcessControl,
    registrations: &[SupervisorRegistration],
    stop: Stop,
) -> Vec<Value> {
    registrations
        .iter()
        .filter(|registration| registration.mode == Some(SupervisorMode::InCmux))
        .filter_map(|registration| {
            let id = registration.workspace_id.as_deref()?;
            if stop == Stop::Pending && processes.alive(registration.pid) {
                return Some(json!({
                    "workspace_id": id,
                    "outcome": "left_open",
                    "reason": format!(
                        "supervisor pid {} is still draining; `down --wait` closes it",
                        registration.pid
                    ),
                }));
            }
            Some(match cmux.close(id) {
                Ok(()) => json!({"workspace_id": id, "outcome": "closed"}),
                Err(error) => json!({
                    "workspace_id": id,
                    "outcome": "close_failed",
                    "reason": format!("{error:#}"),
                }),
            })
        })
        .collect()
}
