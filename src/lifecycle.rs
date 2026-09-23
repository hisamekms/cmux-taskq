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
//! inside the cmux workspace `[<repo>]dagq supervisor` instead, with no
//! launchd involved and so nothing to restart it (ADR-0011).
//!
//! A supervisor is only reused while it runs this binary's own version.
//! Every registration carries the `binary_version` its process recorded,
//! and `up` drains a live supervisor of any other build before starting one
//! of its own in its place, so replacing `~/.local/bin/dagq` and
//! running `up` is the whole binary update (ADR-0014).
use crate::{
    VERSION,
    application::{
        AgentProvider, DetachedRefusal, LaunchAgent, ProcessControl, SupervisorEnvironment,
        WorkspaceBackend, WorkspaceTags,
    },
    domain::{RunStatus, SessionRole, SupervisorMode, SupervisorRegistration},
    infrastructure::{
        adapters::{
            ClaudeCode, GitRepository, claude_trusts_repository, maintainer_workspace_name,
            path_text, shell_join, supervisor_workspace_name, workspace_description,
            workspace_group_name,
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
    cell::{OnceCell, RefCell},
    collections::HashSet,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

/// Set in the environment of every workspace of a queue (`--env`, which
/// every shell of the workspace inherits): the role the workspace plays, so
/// that `up`, run from inside the maintainer session (the plugin skill calls
/// it), does not open a second one, and the plugin's hook knows the session
/// however it was started (ADR-0026).
pub const ROLE_ENV: &str = "DAGQ_ROLE";
/// The queue database the workspace belongs to.
pub const QUEUE_ENV: &str = "DAGQ_QUEUE";
pub const MAINTAINER_ROLE: &str = SessionRole::Maintainer.as_str();
/// File under the queue's log directory that launchd appends the
/// supervisor's stdout and stderr to.
pub const LAUNCHD_LOG_NAME: &str = "launchd.log";

/// What `up` reads from the process that runs it.
#[derive(Debug, Clone)]
pub struct UpEnvironment {
    /// `DAGQ_ROLE`, if set.
    pub role: Option<String>,
    /// `DAGQ_QUEUE`, if set.
    pub queue: Option<PathBuf>,
    /// `PATH`, copied into the agent so the supervisor finds what this shell finds.
    pub path: String,
    /// `CMUX_SOCKET_PASSWORD`, if this shell exported it (non-empty); it is
    /// then copied into the agent too.
    pub socket_password: Option<String>,
    /// The binary launchd runs: this one, by absolute path.
    pub current_exe: PathBuf,
    /// Claude Code's global config, which records the folder trust of each
    /// repository (`claude_global_config`); `None` trusts nothing.
    pub claude_config: Option<PathBuf>,
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

/// What `up` says when Claude Code has not trusted the repository: every
/// run session would stop at the folder trust dialog, since run worktrees
/// take their trust from the repository root.
pub fn untrusted_repository_hint(root: &Path, config: Option<&Path>) -> String {
    format!(
        "Claude Code has not trusted the repository {root}, so every run session would stop at \
its folder trust dialog (run worktrees take their trust from the repository root). Start `claude` \
once in {root} and accept \"Yes, I trust this folder\", then run `up` again; {config} must then \
record projects[\"{root}\"].hasTrustDialogAccepted = true",
        root = root.display(),
        config = config.map_or_else(|| "~/.claude.json".into(), |c| c.display().to_string()),
    )
}

#[derive(Debug, Clone)]
pub struct UpOptions {
    pub parallel: u16,
    /// Start the supervisor inside the cmux workspace `[<repo>]dagq
    /// supervisor` instead of as a LaunchAgent: no launchd, no automatic
    /// restart, and no out-of-cmux preflight to pass.
    pub in_cmux: bool,
    /// Refuse to wait for a supervisor of another version to drain. Only a
    /// replacement reads it (ADR-0014): with runs in flight `up` stops
    /// without touching anything, and with none it replaces the supervisor
    /// but bounds the drain by `startup_timeout` rather than waiting for a
    /// supervisor that turns out not to stop.
    pub no_wait: bool,
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
/// queue's open work. Preflight first (cmux, claude, Claude Code's trust of
/// the repository root, an initialized queue, the repository), then prune registrations whose process is gone, start
/// the agent only when no live registration of this binary's version
/// remains (after proving that cmux admits a process with the agent's
/// environment) — draining and replacing a live supervisor of any other
/// version — and open the maintainer workspace only outside a maintainer
/// session.
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
    // Claude Code keys trust by the main checkout even for a linked
    // worktree, and `up` may run from any worktree of the repository.
    let trust_root = repository
        .common_dir
        .parent()
        .filter(|_| repository.common_dir.file_name() == Some(".git".as_ref()))
        .unwrap_or(&repository.root);
    let trusted = match environment.claude_config.as_deref() {
        Some(config) => claude_trusts_repository(config, trust_root)?,
        None => false,
    };
    ensure!(
        trusted,
        untrusted_repository_hint(trust_root, environment.claude_config.as_deref())
    );
    let plugin_dir = options
        .plugin_dir
        .as_deref()
        .map(|dir| {
            dir.canonicalize()
                .with_context(|| format!("plugin directory {}", dir.display()))
        })
        .transpose()?;
    let queue = SqliteQueue::open(&db)?;
    let workspaces = QueueWorkspaces::new(cmux, &db, location.hash(), &repository.root);

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

    // A supervisor of another build is not ours to reuse: it would keep
    // serving this queue with the code the operator has just replaced, and
    // it would migrate the schema of a queue the new binary owns (ADR-0014).
    let outdated = live
        .iter()
        .any(|registration| registration.binary_version.as_deref() != Some(VERSION));
    let supervisor = match live.first() {
        // Whoever started it recorded the mode; a supervisor started by
        // hand has none, and `up` does not claim one for it.
        Some(registration) if !outdated => json!({
            "outcome": "reused",
            "mode": registration.mode.map(SupervisorMode::as_str),
            "version": registration.binary_version,
            "pid": registration.pid,
            "token": registration.token,
            "workspace_id": registration.workspace_id,
            "plist": location.launch_agent,
            "log_dir": location.log_dir,
        }),
        Some(_) => replace_supervisors(
            location,
            &db,
            &repository,
            &queue,
            &workspaces,
            launchd,
            processes,
            environment,
            options,
            &live,
        )?,
        None => start_supervisor(
            location,
            &db,
            &repository,
            &queue,
            &workspaces,
            launchd,
            processes,
            environment,
            options,
            &existing,
            false,
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
    } else if let Some(id) = recorded_workspace(&queue, cmux, SessionRole::Maintainer)? {
        json!({"outcome": "reused", "workspace_id": id, "name": name})
    } else {
        let command = maintainer_command(
            &db,
            &location.log_dir,
            &options.claude,
            plugin_dir.as_deref(),
        )?;
        let id = cmux.create_named(
            &name,
            &repository.root,
            &command,
            &workspaces.tags(SessionRole::Maintainer)?,
        )?;
        queue.register_session_workspace(SessionRole::Maintainer, &id)?;
        json!({"outcome": "created", "workspace_id": id, "name": name})
    };

    Ok(json!({
        "supervisor": supervisor,
        "maintainer": maintainer,
        "pruned_supervisors": pruned,
        "warnings": workspaces.warnings.take(),
        "doctor": open_work(&queue, processes)?,
    }))
}

/// `DAGQ_ROLE=<role>` and `DAGQ_QUEUE=<db>`: the environment every
/// workspace of the queue at `db` is opened with (ADR-0026).
pub fn session_env(role: SessionRole, db: &Path) -> Result<Vec<(String, String)>> {
    Ok(vec![
        (ROLE_ENV.to_owned(), role.as_str().to_owned()),
        (QUEUE_ENV.to_owned(), path_text(db)?),
    ])
}

/// What every workspace `up` opens for a queue carries (ADR-0026): its role
/// and queue in the environment, the description line, and the queue's
/// workspace group. The group is made when the first workspace needs it
/// (cmux opens an anchor workspace with it), so an `up` that reuses
/// everything touches no group. A group cmux cannot make is a warning in
/// `up`'s result, and the workspace opens outside it.
pub struct QueueWorkspaces<'a> {
    cmux: &'a dyn WorkspaceBackend,
    db: &'a Path,
    hash: String,
    group_name: String,
    group: OnceCell<Option<String>>,
    warnings: RefCell<Vec<String>>,
}

impl<'a> QueueWorkspaces<'a> {
    pub fn new(
        cmux: &'a dyn WorkspaceBackend,
        db: &'a Path,
        hash: String,
        repo_root: &Path,
    ) -> Self {
        Self {
            cmux,
            db,
            hash,
            group_name: workspace_group_name(repo_root),
            group: OnceCell::new(),
            warnings: RefCell::new(Vec::new()),
        }
    }

    /// The tags of a workspace of `role` that belongs to no run.
    pub fn tags(&self, role: SessionRole) -> Result<WorkspaceTags> {
        Ok(WorkspaceTags {
            env: session_env(role, self.db)?,
            description: Some(workspace_description(role, &self.hash, None, None)),
            group: self.group(),
        })
    }

    fn group(&self) -> Option<String> {
        self.group
            .get_or_init(
                || match self.cmux.ensure_group(&self.hash, &self.group_name) {
                    Ok(group) => Some(group),
                    Err(error) => {
                        self.warnings.borrow_mut().push(format!(
                            "cmux workspace group {:?} (external ID {}) could not be made, so the \
workspace opens outside it: {error:#}",
                            self.group_name, self.hash
                        ));
                        None
                    }
                },
            )
            .clone()
    }
}

/// The workspace the queue recorded for `role`, while cmux still lists it.
/// A recorded UUID cmux no longer lists (the workspace was closed, or cmux
/// restarted) is forgotten, so the caller opens a new one. The title is
/// never consulted: people rename workspaces (ADR-0026).
fn recorded_workspace(
    queue: &SqliteQueue,
    cmux: &dyn WorkspaceBackend,
    role: SessionRole,
) -> Result<Option<String>> {
    let Some(id) = queue.session_workspace(role)? else {
        return Ok(None);
    };
    if cmux.exists(&id)? {
        return Ok(Some(id));
    }
    queue.remove_session_workspace(role)?;
    Ok(None)
}

/// Start one supervisor in the mode this `up` was asked for. The mode of a
/// supervisor being replaced does not decide it: `up --in-cmux` moves a
/// launchd queue into a workspace and a plain `up` moves it back, and
/// either way the old one has already been drained and its agent unloaded.
#[allow(clippy::too_many_arguments)]
fn start_supervisor(
    location: &QueueLocation,
    db: &Path,
    repository: &GitRepository,
    queue: &SqliteQueue,
    workspaces: &QueueWorkspaces,
    launchd: &dyn LaunchAgent,
    processes: &dyn ProcessControl,
    environment: &UpEnvironment,
    options: &UpOptions,
    existing: &HashSet<String>,
    detached_proven: bool,
) -> Result<Value> {
    if options.in_cmux {
        start_in_cmux(
            location,
            db,
            repository,
            queue,
            workspaces,
            processes,
            environment,
            options,
            existing,
        )
    } else {
        start_under_launchd(
            location,
            db,
            repository,
            queue,
            workspaces.cmux,
            launchd,
            processes,
            environment,
            options,
            existing,
            detached_proven,
        )
    }
}

/// Drain every live supervisor of another build and start one of this
/// binary's version in its place (ADR-0014), so that updating the fixed
/// binary is `up` and nothing else. The stop is `down --wait`'s: unload the
/// LaunchAgent (whose bootout carries the SIGTERM, and whose `KeepAlive`
/// would otherwise restart the old binary at once), SIGINT an in-cmux
/// supervisor, SIGTERM one launchd did not signal, then wait for each
/// registration to go — the supervisor stops claiming, finishes the runs it
/// holds and deregisters — and close the workspaces of the in-cmux ones
/// before a new one could want the same name.
///
/// The drain is unbounded because a run is a Claude session: `--no-wait` is
/// the way to ask for the replacement only if nothing is in flight, and it
/// bounds the drain too.
///
/// Only live registrations are replaced. A supervisor that is alive but no
/// longer heartbeating is one `up` neither reuses nor kills, so an
/// old-binary one in that state is left running beside the new supervisor
/// and reported `stale`; stopping it stays the maintainer's call
/// (ADR-0014's Consequences).
#[allow(clippy::too_many_arguments)]
fn replace_supervisors(
    location: &QueueLocation,
    db: &Path,
    repository: &GitRepository,
    queue: &SqliteQueue,
    workspaces: &QueueWorkspaces,
    launchd: &dyn LaunchAgent,
    processes: &dyn ProcessControl,
    environment: &UpEnvironment,
    options: &UpOptions,
    live: &[SupervisorRegistration],
) -> Result<Value> {
    // The version reported as replaced is an outdated one, not merely the
    // first: a mixed set is drained whole, but naming a version that
    // matched would read as if nothing had been out of date.
    let cmux = workspaces.cmux;
    let previous_version = live
        .iter()
        .find(|registration| registration.binary_version.as_deref() != Some(VERSION))
        .and_then(|registration| registration.binary_version.clone());
    if options.no_wait {
        // Read before anything is signalled, so a refusal leaves the old
        // supervisor serving the queue exactly as it was.
        let in_flight = queue.active_runs()?;
        ensure!(
            in_flight.is_empty(),
            "refusing to replace the supervisor of version {} with {VERSION} without waiting: \
{} run(s) are still in flight ({}); run `up` without --no-wait to drain them, or wait for them \
to finish",
            previous_version.as_deref().unwrap_or("(unrecorded)"),
            in_flight.len(),
            in_flight
                .iter()
                .map(|run| run.id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    // Settle what can refuse the new supervisor before the old one is
    // touched: draining a working supervisor and then failing to start its
    // replacement would leave the queue with nothing serving it. For
    // launchd that is the out-of-cmux connection (not asked again below);
    // for `--in-cmux` it is the recorded supervisor workspace.
    if options.in_cmux {
        ensure_supervisor_workspace_free(queue, cmux, live)?;
    } else {
        let spec = launch_agent_spec(location, db, repository, environment, options)?;
        prove_detached_cmux(cmux, &spec)?;
    }
    let replaced: Vec<Value> = live
        .iter()
        .map(|registration| {
            json!({
                "token": registration.token,
                "pid": registration.pid,
                "mode": registration.mode.map(SupervisorMode::as_str),
                "workspace_id": registration.workspace_id,
                "version": registration.binary_version,
            })
        })
        .collect();
    let agent = launchd.uninstall(&location.label, &location.launch_agent)?;
    for registration in live {
        if registration.mode == Some(SupervisorMode::InCmux) {
            processes.interrupt(registration.pid)?;
        } else if (!agent.loaded || agent.pid.is_some()) && Some(registration.pid) != agent.pid {
            // A second SIGTERM would end a draining supervisor at once, so
            // the one bootout delivered is never repeated here.
            processes.terminate(registration.pid)?;
        }
    }
    // `--no-wait` promised not to sit through a drain. The runs were the
    // reason a drain is long, and there were none, but a supervisor can
    // still fail to stop (a loop wedged on a hung cmux or git call keeps
    // its row while its heartbeat thread runs on, and a run claimed
    // between that check and the signal is a session again), so the wait
    // is bounded there instead of unbounded.
    let deadline = options
        .no_wait
        .then(|| Instant::now() + options.startup_timeout);
    loop {
        let remaining: Vec<String> = queue
            .supervisors()?
            .into_iter()
            .filter(|registration| {
                live.iter().any(|l| l.token == registration.token)
                    && processes.alive(registration.pid)
            })
            .map(|registration| format!("{} (pid {})", registration.token, registration.pid))
            .collect();
        if remaining.is_empty() {
            break;
        }
        if let Some(deadline) = deadline {
            ensure!(
                Instant::now() < deadline,
                "--no-wait: the supervisor did not stop within {}s of the signal; still \
registered: {}. It has been asked to drain and its LaunchAgent is unloaded, so run `up` again \
once `status` shows it gone",
                options.startup_timeout.as_secs(),
                remaining.join(", "),
            );
        }
        thread::sleep(options.poll);
    }
    // The drain can also end because the process died with its row intact
    // (launchd's `ExitTimeOut` SIGKILL, or the heartbeat failure that keeps
    // the row on purpose because the database may be unreachable). Those
    // rows go the way `down --force` drops them, so none is left pointing
    // at the workspace closed just below.
    let surviving: Vec<String> = queue
        .supervisors()?
        .into_iter()
        .map(|registration| registration.token)
        .collect();
    for registration in live {
        if surviving.contains(&registration.token) {
            queue.deregister_supervisor(&registration.token)?;
        }
    }
    // The drain is over, so every workspace of a replaced supervisor is
    // ours to close; one left open would stop the next in-cmux supervisor
    // from opening its own.
    let closed = close_supervisor_workspaces(queue, cmux, processes, live, Stop::SeenThrough);
    // Whatever survived the drain (an alive-but-silent supervisor `up`
    // neither reuses nor kills) belongs to another process, not to the one
    // started below.
    let existing: HashSet<String> = queue
        .supervisors()?
        .into_iter()
        .map(|registration| registration.token)
        .collect();
    let mut started = start_supervisor(
        location,
        db,
        repository,
        queue,
        workspaces,
        launchd,
        processes,
        environment,
        options,
        &existing,
        !options.in_cmux,
    )?;
    let object = started
        .as_object_mut()
        .expect("a started supervisor is a JSON object");
    object.insert("outcome".into(), json!("restarted"));
    object.insert("previous_version".into(), json!(previous_version));
    object.insert("replaced".into(), json!(replaced));
    object.insert("supervisor_workspaces".into(), json!(closed));
    Ok(started)
}

/// Refuse, before anything is stopped, when the queue's recorded
/// supervisor workspace is still open and this replacement will not close
/// it. `up` prunes a dead registration without closing its workspace and
/// cmux keeps a workspace open after its command exits, so the workspace of
/// a crashed supervisor that is no longer registered at all can still be
/// open. Finding that only after the drain would cost a working supervisor
/// and leave the queue with nothing serving it.
fn ensure_supervisor_workspace_free(
    queue: &SqliteQueue,
    cmux: &dyn WorkspaceBackend,
    live: &[SupervisorRegistration],
) -> Result<()> {
    let Some(id) = recorded_workspace(queue, cmux, SessionRole::Supervisor)? else {
        return Ok(());
    };
    ensure!(
        live.iter().any(|registration| {
            registration.mode == Some(SupervisorMode::InCmux)
                && registration.workspace_id.as_deref() == Some(id.as_str())
        }),
        "cmux workspace {id}, recorded as this queue's supervisor workspace, is still open but \
belongs to no supervisor this `up` would drain, so the replacement could not open its own; read \
its screen, then close it (`cmux workspace close {id}`) and run `up --in-cmux` again"
    );
    Ok(())
}

/// Ask cmux whether it admits a process carrying the agent's environment
/// from outside its process tree, which is how the launchd-started
/// supervisor will connect. Kept apart from the start so a replacement can
/// settle the question before it drains a supervisor that works.
fn prove_detached_cmux(cmux: &dyn WorkspaceBackend, spec: &LaunchAgentSpec) -> Result<()> {
    cmux.preflight_detached(&spec.environment).map_err(|error| {
        // Only a refusal has the password (or `--in-cmux`) as its remedy; a
        // ping that could not be run or did not answer is its own error.
        if error.is::<DetachedRefusal>() {
            error.context(DETACHED_CMUX_HINT)
        } else {
            error.context(
                "cmux could not be asked whether it admits a connection from outside its own terminals",
            )
        }
    })
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
    detached_proven: bool,
) -> Result<Value> {
    let spec = launch_agent_spec(location, db, repository, environment, options)?;
    if !detached_proven {
        prove_detached_cmux(cmux, &spec)?;
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
        "version": VERSION,
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
/// `[<repo>]dagq supervisor` may belong to a supervisor that crashed, or to
/// one that is alive but no longer heartbeating (which `up` never reuses
/// and never kills). Either way it is the maintainer's to close, and `up`
/// stops rather than open a second one or interfere with the first.
#[allow(clippy::too_many_arguments)]
fn start_in_cmux(
    location: &QueueLocation,
    db: &Path,
    repository: &GitRepository,
    queue: &SqliteQueue,
    workspaces: &QueueWorkspaces,
    processes: &dyn ProcessControl,
    environment: &UpEnvironment,
    options: &UpOptions,
    existing: &HashSet<String>,
) -> Result<Value> {
    let cmux = workspaces.cmux;
    let name = supervisor_workspace_name(&repository.root);
    if let Some(id) = recorded_workspace(queue, cmux, SessionRole::Supervisor)? {
        bail!(
            "cmux workspace {id}, recorded as this queue's supervisor workspace, is still open \
but no supervisor of this queue is registered and heartbeating; read its screen, then close it \
(`cmux workspace close {id}`) and run `up --in-cmux` again"
        );
    }
    let command = supervise_command(location, db, environment, options)?;
    let workspace_id = cmux.create_named(
        &name,
        &repository.root,
        &command,
        &workspaces.tags(SessionRole::Supervisor)?,
    )?;
    // Recorded before the wait, so a supervisor that never registers still
    // leaves its workspace where the next `up` finds it.
    queue.register_session_workspace(SessionRole::Supervisor, &workspace_id)?;
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
        "version": VERSION,
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

/// The maintainer workspace's command: `claude` with the generated prompt as
/// its first message. The role and queue are the workspace's own `--env`
/// (ADR-0026), not a prefix of this command, so a `claude` started again in
/// that workspace still has them.
pub fn maintainer_command(
    db: &Path,
    log_dir: &Path,
    claude: &Path,
    plugin_dir: Option<&Path>,
) -> Result<String> {
    let mut argv = vec![path_text(claude)?];
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
                &queue,
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
                &queue,
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
                &queue,
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
            &queue,
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
    queue: &SqliteQueue,
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
                Ok(()) => {
                    // The record goes with the workspace. Forgetting it is
                    // tidiness only: `up` drops a UUID cmux no longer lists.
                    if queue
                        .session_workspace(SessionRole::Supervisor)
                        .ok()
                        .flatten()
                        .as_deref()
                        == Some(id)
                    {
                        let _ = queue.remove_session_workspace(SessionRole::Supervisor);
                    }
                    json!({"workspace_id": id, "outcome": "closed"})
                }
                Err(error) => json!({
                    "workspace_id": id,
                    "outcome": "close_failed",
                    "reason": format!("{error:#}"),
                }),
            })
        })
        .collect()
}
