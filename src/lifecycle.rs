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
//! keep restarting a supervisor that fails its own preflight forever.
use crate::{
    application::{
        AgentProvider, DetachedRefusal, LaunchAgent, ProcessControl, SupervisorEnvironment,
        WorkspaceBackend,
    },
    domain::{RunStatus, SupervisorRegistration},
    infrastructure::{
        adapters::{ClaudeCode, GitRepository, maintainer_workspace_name, path_text, shell_join},
        launchd::LaunchAgentSpec,
        location::QueueLocation,
        runtime_store::HEARTBEAT_TIMEOUT_SECS,
        sqlite::SqliteQueue,
    },
    runtime::{maintainer_prompt, unix_time},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
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
written into the LaunchAgent); until the in-cmux supervisor mode exists, the alternative is \
to start `supervise` by hand in a cmux terminal and run `up` again, which reuses it";

#[derive(Debug, Clone)]
pub struct UpOptions {
    pub parallel: u16,
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
    for registration in queue.supervisors()? {
        if !processes.alive(registration.pid) {
            queue.deregister_supervisor(&registration.token)?;
            pruned.push(json!({"token": registration.token, "pid": registration.pid}));
        } else if fresh(&registration, processes) {
            live.push(registration);
        }
        // Alive but silent: not ours to kill; it shows up as stale in doctor.
    }

    let supervisor = match live.first() {
        Some(registration) => json!({
            "outcome": "reused",
            "pid": registration.pid,
            "token": registration.token,
            "plist": location.launch_agent,
            "log_dir": location.log_dir,
        }),
        None => {
            let spec = launch_agent_spec(location, &db, &repository, environment, options)?;
            if let Err(error) = cmux.preflight_detached(&spec.environment) {
                // Only a refusal has the password as its remedy; a ping
                // that could not be run or did not answer is its own error.
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
                wait_for_registration(&queue, processes, options).with_context(|| {
                    format!(
                        "supervisor did not register within {}s; see {}",
                        options.startup_timeout.as_secs(),
                        spec.log
                    )
                })?;
            json!({
                "outcome": "started",
                "pid": registration.pid,
                "token": registration.token,
                "plist": spec.plist,
                "log_dir": location.log_dir,
            })
        }
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

fn fresh(registration: &SupervisorRegistration, processes: &dyn ProcessControl) -> bool {
    processes.alive(registration.pid)
        && unix_time() - registration.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS
}

fn wait_for_registration(
    queue: &SqliteQueue,
    processes: &dyn ProcessControl,
    options: &UpOptions,
) -> Result<SupervisorRegistration> {
    let deadline = Instant::now() + options.startup_timeout;
    loop {
        if let Some(registration) = queue
            .supervisors()?
            .into_iter()
            .find(|registration| fresh(registration, processes))
        {
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
        program_arguments: vec![
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
        ],
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

/// Stop the queue's supervisor. Unloading the agent sends its process
/// SIGTERM, on which it stops claiming and drains its active runs; a
/// supervisor that was started by hand gets the SIGTERM from here. The
/// maintainer workspace is left open.
pub fn down(
    location: &QueueLocation,
    launchd: &dyn LaunchAgent,
    processes: &dyn ProcessControl,
    options: &DownOptions,
) -> Result<Value> {
    let queue = SqliteQueue::open(&location.db)?;
    let (live, dead): (Vec<SupervisorRegistration>, Vec<SupervisorRegistration>) = queue
        .supervisors()?
        .into_iter()
        .partition(|registration| processes.alive(registration.pid));
    if live.is_empty() {
        // An agent whose supervisor never registered (a crash loop) is
        // still unloaded, or it would keep restarting.
        let agent = launchd.uninstall(&location.label, &location.launch_agent)?;
        let mut pruned = Vec::new();
        if options.force {
            for registration in &dead {
                queue.deregister_supervisor(&registration.token)?;
                pruned.push(json!({"token": registration.token, "pid": registration.pid}));
            }
        }
        return Ok(json!({
            "outcome": "not_running",
            "launch_agent_unloaded": agent.loaded,
            "pruned_supervisors": pruned,
        }));
    }
    let agent = launchd.uninstall(&location.label, &location.launch_agent)?;
    // launchd's bootout delivers the SIGTERM to the agent's own process; a
    // supervisor started by hand gets it from here. A second SIGTERM would
    // end a draining supervisor at once, so when the agent is loaded but
    // its pid is unknown nothing is signalled.
    if !agent.loaded || agent.pid.is_some() {
        for registration in live.iter().filter(|r| Some(r.pid) != agent.pid) {
            processes.terminate(registration.pid)?;
        }
    }
    let unloaded = agent.loaded;
    let pids: Vec<u32> = live.iter().map(|r| r.pid).collect();
    let pid = pids[0];
    if options.force {
        for registration in &live {
            if processes.alive(registration.pid) {
                processes.kill(registration.pid)?;
            }
            queue.deregister_supervisor(&registration.token)?;
        }
        return Ok(json!({
            "outcome": "killed",
            "pid": pid,
            "pids": pids,
            "launch_agent_unloaded": unloaded,
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
        }));
    }
    Ok(json!({
        "outcome": "draining",
        "pid": pid,
        "pids": pids,
        "launch_agent_unloaded": unloaded,
    }))
}
