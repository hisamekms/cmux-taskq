//! The entry points of the runtime's use cases: each opens the queue and
//! the repository, builds the adapters of the ports and calls the use case
//! in `application` (ADR-0013). `supervise` and `session` are
//! [`crate::application::supervise`] and [`crate::application::session`];
//! `integrate` is [`crate::application::integrate`]; `review`, `rebind`
//! and `stats` are built in [`crate::compose`] and re-exported here, and
//! the inbox and planner prompts are [`crate::application::prompt`].
//! `status`, `doctor`, `recover` and `ask` still work on the queue here.
pub use crate::application::recording::{
    BACKEND_ERROR_CHARS, RecordingBackend, backend_failure_payload,
};
pub use crate::application::{
    health::{LeaseHealth, ProcessHealth, RunHealth},
    integrate::{
        IntegrateTarget, integrate_logs, integrate_verify_log, next_integrate_attempt,
        register_follow_ups,
    },
    prompt::{
        PredecessorSummary, STOP_BACKGROUND, WORKER_READING, inbox_prompt, planner_prompt, prompt,
        siblings_in_progress,
    },
    rebind::REBIND_LOG,
    review::review_logs_hint,
    supervise::{PromptKind, RunError, TRIAGE_TOOLS, detect_prompt, review_prompt},
};
pub use crate::compose::{rebind, review, stats};
pub use crate::infrastructure::run_files::SupervisorLog;
use crate::{
    application::{
        AgentProvider, Generators, MainRemote, NoteLog, WorkspaceBackend,
        health::run_health,
        integrate::{self as integration, Integration},
        session::{self as wrapper, Session},
        supervise::{self as supervisor, Heartbeat, Layout, LoopSettings, Ports},
    },
    domain::{
        IntegrationOutcome, RunId, RunLease, RunStatus, SessionRole, SupervisorMode,
        SupervisorRegistration, TaskDetail, TaskId, TaskRun, heartbeat_stale,
    },
    infrastructure::{
        adapters::{
            ClaudeCode, GitRepository, SystemProcesses, load_average, path_text, process_alive,
        },
        clock,
        location::{QueueLocation, runs_dir},
        process::LocalSpawner,
        run_env::ShellVerifier,
        run_files::LocalRunFiles,
        runtime_store::{HEARTBEAT_TIMEOUT_SECS, SqliteOpener},
        sqlite::SqliteQueue,
    },
    lifecycle::{QUEUE_ENV, REVIEWER_ROLE, ROLE_ENV, session_env},
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

const IDLE_POLL: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_secs(1);

/// How the supervisor loop is driven. `stop` is the graceful drain switch
/// (SIGINT in the CLI): no more claims, exit once every active run rests.
#[derive(Debug, Clone)]
pub struct SuperviseOptions {
    /// Upper bound on runs executing at once.
    pub parallel: usize,
    /// Exit when no run is active and no task can be claimed, instead of
    /// polling for new work.
    pub once: bool,
    pub stop: Arc<AtomicBool>,
    /// Directory for one `supervisor-<started_at>-<pid>.log` per start,
    /// created if missing; `None` keeps the messages on stderr only.
    pub log_dir: Option<PathBuf>,
    /// Start the observer job when this long passed since the last one
    /// started or finished (ADR-0024 decision 4); zero disables the
    /// observer, the daily one included.
    pub observe_interval: Duration,
    /// Also run the daily observation once every 24 hours.
    pub observe_daily: bool,
    /// Pause between two passes over the active runs; tests shorten it.
    pub tick: Duration,
    /// Pause between two looks for claimable work while no run is active.
    pub idle_poll: Duration,
    /// The clock and IDs of everything the supervisor records; tests fix them.
    pub generators: Generators,
}

impl SuperviseOptions {
    pub fn new(parallel: usize, once: bool) -> Self {
        Self {
            parallel,
            once,
            stop: Arc::new(AtomicBool::new(false)),
            log_dir: None,
            observe_interval: Duration::ZERO,
            observe_daily: false,
            tick: TICK,
            idle_poll: IDLE_POLL,
            generators: clock::system(),
        }
    }

    fn settings(&self) -> LoopSettings {
        LoopSettings {
            parallel: self.parallel,
            once: self.once,
            stop: self.stop.clone(),
            observe_interval: self.observe_interval,
            observe_daily: self.observe_daily,
            tick: self.tick,
            idle_poll: self.idle_poll,
        }
    }
}

/// Run and monitor tasks until the loop ends (see
/// [`supervisor::supervise`]). Accepted runs are reviewed headless by
/// `claude` itself (ADR-0027).
pub fn supervise(
    db: &Path,
    repo: &Path,
    cmux: &dyn WorkspaceBackend,
    claude: &Path,
    runner: &Path,
    options: &SuperviseOptions,
) -> Result<Value> {
    let reviewer = ClaudeCode {
        executable: claude.into(),
    };
    supervise_with_reviewer(db, repo, cmux, claude, &reviewer, runner, options)
}

/// [`supervise`] with the provider of the headless review given apart
/// from the `claude` the run sessions start (a test double in tests).
pub fn supervise_with_reviewer(
    db: &Path,
    repo: &Path,
    cmux: &dyn WorkspaceBackend,
    claude: &Path,
    reviewer: &dyn AgentProvider,
    runner: &Path,
    options: &SuperviseOptions,
) -> Result<Value> {
    ensure!(options.parallel >= 1, "parallel must be at least 1");
    let db = db
        .canonicalize()
        .context("queue must already be initialized")?;
    let repository = GitRepository::inspect(repo)?;
    let pid = std::process::id();
    let generators = options.generators.clone();
    let layout = Layout {
        runs_dir: runs_dir(&db),
        queue_hash: QueueLocation::explicit(&db).hash(),
        repo_root: repository.root.clone(),
        common_dir: repository.common_dir.clone(),
        claude: claude.into(),
        runner: runner.into(),
        pid,
        version: crate::VERSION.to_owned(),
        worker_env: session_env(SessionRole::Worker, &db)?,
        job_env: vec![
            (ROLE_ENV.to_owned(), REVIEWER_ROLE.to_owned()),
            (QUEUE_ENV.to_owned(), path_text(&db)?),
        ],
        observer_env_remove: vec![ROLE_ENV.to_owned()],
        db: db.clone(),
    };
    let agent = ClaudeCode {
        executable: claude.into(),
    };
    let review_db = db.clone();
    let review_material = move |task_id: TaskId| review(&review_db, task_id);
    let log_dir = options.log_dir.clone();
    let log_clock = generators.clock.clone();
    let open_log = move |started_at: i64| -> Result<Arc<dyn NoteLog>> {
        Ok(Arc::new(match &log_dir {
            Some(dir) => SupervisorLog::open(dir, started_at, pid, log_clock.clone())?,
            None => SupervisorLog::default(),
        }))
    };
    let ports = Ports {
        queues: Arc::new(SqliteOpener {
            db: db.clone(),
            generators: generators.clone(),
        }),
        verifier: Arc::new(ShellVerifier {
            checkout: main_checkout(&repository),
            db: db.clone(),
        }),
        remote: Arc::new(repository.clone()),
        repository: Arc::new(repository),
        cmux,
        agent: &agent,
        reviewer,
        spawner: &LocalSpawner,
        files: Arc::new(LocalRunFiles),
        processes: Arc::new(SystemProcesses),
        generators,
        review_material: &review_material,
        open_log: &open_log,
        load_average,
        layout,
    };
    supervisor::supervise(&ports, &options.settings())
}

/// The runtime's own constructor of the cmux wrapper `up`, `down` and the
/// tests use: failures are recorded in the queue at `db`, and `token` is
/// the supervisor whose slots are reported (`None` reports all of them).
impl<'a> RecordingBackend<'a> {
    pub fn new(inner: &'a dyn WorkspaceBackend, db: PathBuf, token: Option<String>) -> Self {
        Self::over(
            inner,
            Arc::new(SqliteOpener {
                db,
                generators: clock::system(),
            }),
            token,
            load_average,
        )
    }
}

/// Land one validated run on `main`: take the single integration slot,
/// rebase the run worktree onto the current `refs/heads/main`, re-validate
/// (receipt, descent from main, clean tree) and run the verification
/// commands, the only run of them for the commit (ADR-0023), squash
/// the tree into one commit with `Dagq-Task` / `Dagq-Run` trailers and
/// fast-forward `main` to it. Never a merge commit, never a fast-forward of
/// the run branch itself. A conflict or a failed re-validation parks the run
/// as `needs_session` for a resumed session to fix; a rewritten receipt that
/// reports `failed` ends the run. `repo` is any checkout of the repository
/// the queue is bound to. After a landing, `main` is pushed to `origin`
/// through `remote` (ADR-0019 decision 3); `None` is `--no-push`. The push
/// never changes the landing: its outcome is an event and the `push` of the
/// result. The landed receipt's `follow_ups` become draft tasks
/// (ADR-0019 decision 4), listed as the result's `follow_ups`.
///
/// The use case is [`integration::begin`] and
/// [`integration::land_integrating`]; this entry point opens the queue and
/// the repository and keeps the lease alive in between.
pub fn integrate(
    db: &Path,
    target: IntegrateTarget,
    repo: &Path,
    remote: Option<&dyn MainRemote>,
) -> Result<Value> {
    let db = db
        .canonicalize()
        .context("queue must already be initialized")?;
    let mut queue = SqliteQueue::open(&db)?;
    let repository = GitRepository::inspect(repo)?;
    let common_dir = path_text(&repository.common_dir)?;
    let generators = queue.generators().clone();
    let verifier = ShellVerifier {
        checkout: main_checkout(&repository),
        db: db.clone(),
    };
    let mut integration = Integration {
        queue: &mut queue,
        repository: &repository,
        verifier: &verifier,
        remote,
        common_dir: &common_dir,
        clock: &*generators.clock,
        ids: &*generators.ids,
        processes: &SystemProcesses,
        pid: std::process::id(),
    };
    let Some(begun) = integration::begin(&mut integration, target, repo)? else {
        return Ok(serde_json::to_value(IntegrationOutcome::NoRunAwaiting)?);
    };
    let heartbeat = Heartbeat::start(
        Arc::new(SqliteOpener {
            db: db.clone(),
            generators: generators.clone(),
        }),
        begun.token.clone(),
    );
    let outcome = integration::land_integrating(
        &mut integration,
        &begun.run,
        begun.previous,
        &begun.main,
        &begun.token,
    )?;
    drop(heartbeat); // Stops the lease heartbeat before this process reports.
    Ok(serde_json::to_value(outcome)?)
}

/// A process that owns runs, as `status` and `doctor` report it: a resident
/// `supervise` through its registration (`registered`, with `parallel` and
/// `started_at`), or an `integrate` process through the lease it holds
/// (`registered: false`). `run_ids` are the leases carrying its token, and
/// they share its heartbeat. `stale` is a registration or lease that no
/// working process stands behind: a dead pid or a heartbeat older than
/// `HEARTBEAT_TIMEOUT_SECS`. `mode` is how `up` started it (`launchd`, or
/// `in_cmux` with the `workspace_id` it runs in); a supervisor started by
/// hand and an `integrate` process have none. Nothing here is deleted
/// automatically.
#[derive(Debug, Clone, Serialize)]
pub struct SupervisorHealth {
    pub pid: u32,
    pub alive: bool,
    pub registered: bool,
    pub mode: Option<SupervisorMode>,
    pub workspace_id: Option<String>,
    /// The `dagq` version the registered process runs; `None` for a
    /// lease holder without a registration, or a registration older than
    /// the column (ADR-0014).
    pub binary_version: Option<String>,
    pub parallel: Option<u32>,
    pub started_at: Option<i64>,
    pub heartbeat_at: i64,
    pub heartbeat_age_secs: i64,
    pub stale: bool,
    pub run_ids: Vec<RunId>,
}

impl SupervisorHealth {
    /// The supervisor in `doctor`'s default output, one line's worth.
    pub fn summary(&self) -> Value {
        json!({
            "pid": self.pid,
            "alive": self.alive,
            "registered": self.registered,
            "mode": self.mode,
            "workspace_id": self.workspace_id,
            "binary_version": self.binary_version,
            "heartbeat_age_secs": self.heartbeat_age_secs,
            "stale": self.stale,
            "run_ids": self.run_ids,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub checked_at: i64,
    pub supervisors: Vec<SupervisorHealth>,
    pub runs: Vec<RunHealth>,
}

/// Registered supervisors, lease holders and the unfinished runs with their
/// leases, without inspecting the runs' processes, plus what waits for a
/// person (`attention`) and the newest event id (`cursor`) to `watch`
/// from (ADR-0016).
pub fn status(db: &Path) -> Result<Value> {
    status_for(db, None)
}

/// Characters of an ask's question `status` keeps before `…`.
const ASK_QUESTION_CHARS: usize = 200;

/// `ask`: register an ask and, when it is new, tell a person with one
/// `cmux notify` aimed at the inbox workspace `up` recorded (without a
/// workspace when there is none). This is the runtime's only notification
/// (ADR-0022 decision 5): the process that asks sends it, whether a
/// worker, a job or the supervisor's own loop. A repeated ask notifies
/// nobody. The ask stands whether or not the notification goes
/// out; a failure is reported as `notify_error` next to `notified: false`.
pub fn ask(
    db: &Path,
    checkout: &Path,
    ask: crate::domain::NewAsk,
    cmux: &dyn WorkspaceBackend,
) -> Result<Value> {
    crate::application::ask::ask(&mut SqliteQueue::open(db)?, checkout, ask, cmux)
}

/// `status --role`: the attention narrowed to what `role` acts on
/// (ADR-0022; `None` is all of it), and every open ask with its question
/// cut to 200 characters.
pub fn status_for(db: &Path, role: Option<crate::domain::SessionRole>) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    // Read before the state it describes, so a transition in between is
    // seen again by `watch --after cursor` rather than missed.
    let cursor = queue.latest_event_id()?;
    let now = queue.generators().clock.now();
    let registrations = queue.supervisors()?;
    let leases = queue.run_leases()?;
    let runs = queue
        .active_runs()?
        .into_iter()
        .map(|run| {
            let lease = leases
                .iter()
                .find(|l| l.run_id == *run.id())
                .map(|l| lease_health(l, now));
            json!({
                "run_id": run.id(),
                "task_id": run.task_id(),
                "status": run.status(),
                "workspace_id": run.workspace_id(),
                "worktree_path": run.worktree_path(),
                "lease": lease,
            })
        })
        .collect::<Vec<_>>();
    let asks = queue
        .asks(crate::infrastructure::asks::AskQuery {
            open: true,
            ..Default::default()
        })?
        .into_iter()
        .map(|ask| {
            json!({
                "id": ask.id,
                "kind": ask.kind,
                "question": crate::view::truncate(&ask.question, ASK_QUESTION_CHARS)
                    .unwrap_or(ask.question),
                "task_id": ask.task_id,
                "run_id": ask.run_id,
                "asked_by": ask.asked_by,
                "age_secs": now - ask.created_at,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "checked_at": now,
        "supervisors": supervisors(&registrations, &leases, now),
        "runs": runs,
        "attention": crate::watch::attention(&queue, &registrations, now)?
            .into_iter()
            .filter(|_| crate::watch::for_role(role))
            .collect::<Vec<_>>(),
        "asks": asks,
        "cursor": cursor,
    }))
}

/// Inspect every registered supervisor and every unfinished run with its
/// lease, processes and paths. Reads only.
/// `doctor`: with `full`, every unfinished run with its lease, processes
/// and paths; without it, one line's worth per run and per supervisor
/// ([`RunHealth::summary`], [`SupervisorHealth::summary`]).
pub fn doctor(db: &Path, full: bool) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let now = queue.generators().clock.now();
    let registrations = queue.supervisors()?;
    let leases = queue.run_leases()?;
    let runs = queue
        .active_runs()?
        .into_iter()
        .map(|run| {
            let processes = queue.processes(run.id())?;
            let lease = leases
                .iter()
                .find(|l| l.run_id == *run.id())
                .map(|l| lease_health(l, now));
            Ok(run_health(
                &run,
                &processes,
                lease,
                now,
                &SystemProcesses,
                &LocalRunFiles,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let supervisors = supervisors(&registrations, &leases, now);
    if !full {
        return Ok(json!({
            "checked_at": now,
            "supervisors": supervisors.iter().map(SupervisorHealth::summary).collect::<Vec<_>>(),
            "runs": runs.iter().map(RunHealth::summary).collect::<Vec<_>>(),
        }));
    }
    Ok(serde_json::to_value(DoctorReport {
        checked_at: now,
        supervisors,
        runs,
    })?)
}

/// Mark an orphaned run `interrupted` (or a run whose `integrate` process
/// died `awaiting_integration` again) and drop its lease, after checking that
/// nothing registered for it is still alive. Never reruns, never deletes the
/// worktree or workspace, leaves the task `in_progress`, and does not touch
/// any other run.
pub fn recover(db: &Path, id: &RunId) -> Result<Value> {
    let mut queue = SqliteQueue::open(db)?;
    let run = queue.run(id)?;
    ensure!(
        matches!(
            run.status(),
            RunStatus::Claimed
                | RunStatus::Starting
                | RunStatus::Running
                | RunStatus::Validating
                | RunStatus::Integrating
        ),
        "run {id} is {}; only unfinished runs can be recovered",
        run.status().as_str()
    );
    let now = queue.generators().clock.now();
    let lease = queue.run_lease(id)?.map(|l| lease_health(&l, now));
    let processes = queue.processes(run.id())?;
    let health = run_health(
        &run,
        &processes,
        lease,
        now,
        &SystemProcesses,
        &LocalRunFiles,
    );
    ensure!(
        health.recoverable,
        "refusing to recover run {id}: {}",
        health.blockers.join("; ")
    );
    let report = json!({"run": health});
    let run = queue.recover_run(run.id(), processes.len(), report)?;
    Ok(json!({"outcome": "recovered", "run": run}))
}

fn lease_health(lease: &RunLease, now: i64) -> LeaseHealth {
    let age = now - lease.heartbeat_at;
    LeaseHealth {
        pid: lease.pid,
        alive: process_alive(lease.pid),
        heartbeat_at: lease.heartbeat_at,
        heartbeat_age_secs: age,
        stale: age > HEARTBEAT_TIMEOUT_SECS,
    }
}

/// Registered supervisors in registration order, then any other lease
/// holder (an `integrate` process) in lease order; leases join by token.
pub(crate) fn supervisors(
    registrations: &[SupervisorRegistration],
    leases: &[RunLease],
    now: i64,
) -> Vec<SupervisorHealth> {
    let health = |pid: u32, heartbeat_at: i64, registration: Option<&SupervisorRegistration>| {
        let alive = process_alive(pid);
        let age = now - heartbeat_at;
        SupervisorHealth {
            pid,
            alive,
            registered: registration.is_some(),
            mode: registration.and_then(|r| r.mode),
            workspace_id: registration.and_then(|r| r.workspace_id.clone()),
            binary_version: registration.and_then(|r| r.binary_version.clone()),
            parallel: registration.map(|r| r.parallel),
            started_at: registration.map(|r| r.started_at),
            heartbeat_at,
            heartbeat_age_secs: age,
            stale: heartbeat_stale(alive, age),
            run_ids: Vec::new(),
        }
    };
    let mut entries: Vec<(&str, SupervisorHealth)> = registrations
        .iter()
        .map(|r| (r.token.as_str(), health(r.pid, r.heartbeat_at, Some(r))))
        .collect();
    for lease in leases {
        let index = match entries.iter().position(|(token, _)| *token == lease.token) {
            Some(index) => index,
            None => {
                entries.push((
                    lease.token.as_str(),
                    health(lease.pid, lease.heartbeat_at, None),
                ));
                entries.len() - 1
            }
        };
        let entry = &mut entries[index].1;
        entry.run_ids.push(lease.run_id.clone());
        if !entry.registered {
            // Every lease of one process carries the same heartbeat; the
            // freshest one stands for the process.
            let age = now - lease.heartbeat_at;
            if age < entry.heartbeat_age_secs {
                entry.heartbeat_at = lease.heartbeat_at;
                entry.heartbeat_age_secs = age;
                entry.stale = heartbeat_stale(entry.alive, age);
            }
        }
    }
    entries.into_iter().map(|(_, health)| health).collect()
}

/// The main worktree of the repository: the parent of a `.git` common
/// directory, or the inspected root for a bare common directory.
fn main_checkout(repository: &GitRepository) -> PathBuf {
    match repository.common_dir.parent() {
        Some(parent) if repository.common_dir.file_name() == Some(".git".as_ref()) => {
            parent.to_path_buf()
        }
        _ => repository.root.clone(),
    }
}

/// What the headless triage is asked about a run (see
/// [`supervisor::triage_prompt`]), its files read from `dir`.
pub fn triage_prompt(
    detail: &TaskDetail,
    run: &TaskRun,
    resumes: usize,
    dir: &Path,
) -> Result<String> {
    supervisor::triage_prompt(&LocalRunFiles, detail, run, resumes, dir)
}

/// Run from cmux, not from a pipe; stdout must remain a terminal for Claude.
/// `resume` reopens the session of a `needs_session` run the supervisor is
/// resuming (ADR-0019) instead of starting the worker.
pub fn session(db: &Path, id: &RunId, token: &str, claude: &Path, resume: bool) -> Result<Value> {
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "interactive Claude wrapper requires a terminal"
    );
    let provider = ClaudeCode {
        executable: claude.into(),
    };
    run_session(db, id, token, &provider, resume)
}

pub fn session_with_provider(
    db: &Path,
    id: &RunId,
    token: &str,
    provider: &dyn AgentProvider,
) -> Result<Value> {
    run_session(db, id, token, provider, false)
}

/// The wrapper of a resumed session: `session --resume`.
pub fn resume_session_with_provider(
    db: &Path,
    id: &RunId,
    token: &str,
    provider: &dyn AgentProvider,
) -> Result<Value> {
    run_session(db, id, token, provider, true)
}

fn run_session(
    db: &Path,
    id: &RunId,
    token: &str,
    provider: &dyn AgentProvider,
    resume: bool,
) -> Result<Value> {
    let mut queue = SqliteQueue::open(db)?;
    wrapper::run_session(
        Session {
            queue: &mut queue,
            provider,
            spawner: &LocalSpawner,
            files: &LocalRunFiles,
            pid: std::process::id(),
        },
        id,
        token,
        resume,
    )
}
