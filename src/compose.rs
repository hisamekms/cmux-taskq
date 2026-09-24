//! The composition root: each entry point opens the queue and the
//! repository, builds the adapters of the ports (`SqliteQueue`,
//! `GitRepository`, `Cmux`-backed recording, `ClaudeCode`, `Launchctl`'s
//! port, `SystemProcesses`, `LocalRunFiles`, the system clock and IDs) and
//! calls the use case in `application` (ADR-0013). `main` resolves the
//! queue location, parses the CLI and prints what these return; `runtime`
//! and `lifecycle` re-export them under the names the tests use.

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use crate::{
    application::{
        AgentProvider, Generators, LaunchAgent, MainRemote, NoteLog, ProcessControl, QueueOpener,
        Repository, WorkspaceBackend, health,
        integrate::{self as integration, IntegrateTarget, Integration},
        lifecycle::{
            self, DownOptions, Ports as LifecyclePorts, QUEUE_ENV, QueuePaths, REVIEWER_ROLE,
            ROLE_ENV, RepositoryPaths, UpEnvironment, UpOptions, session_env,
        },
        rebind::{self as rebinding, Rebind, RebindTarget},
        recording::RecordingBackend,
        review::{self as reviewing, Review},
        session::{self as wrapper, Session},
        stats as statistics,
        supervise::{self as supervisor, Heartbeat, Layout, LoopSettings, Ports},
    },
    domain::{
        IntegrationOutcome, NewAsk, RunId, SessionRole, TaskDetail, TaskId, TaskRun,
        stats::StatsQuery,
    },
    infrastructure::{
        adapters::{
            ClaudeCode, GitRepository, SystemProcesses, claude_trusts_repository, load_average,
            path_text,
        },
        clock,
        location::{QueueLocation, REPOSITORY_FILE_NAME, data_home, runs_dir},
        process::LocalSpawner,
        run_env::ShellVerifier,
        run_files::{LocalRunFiles, SupervisorLog},
        runtime_store::SqliteOpener,
        sqlite::SqliteQueue,
    },
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

/// `status`: see [`status_for`], with all of the attention.
pub fn status(db: &Path) -> Result<Value> {
    status_for(db, None)
}

/// `status --role`: see [`health::status`], measured to the queue
/// clock's now.
pub fn status_for(db: &Path, role: Option<SessionRole>) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let clock = queue.generators().clock.clone();
    health::status(&queue, &SystemProcesses, &*clock, role)
}

/// `doctor`: see [`health::doctor`].
pub fn doctor(db: &Path, full: bool) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let clock = queue.generators().clock.clone();
    health::doctor(&queue, &SystemProcesses, &LocalRunFiles, &*clock, full)
}

/// `recover`: see [`health::recover`].
pub fn recover(db: &Path, id: &RunId) -> Result<Value> {
    let mut queue = SqliteQueue::open(db)?;
    let clock = queue.generators().clock.clone();
    health::recover(&mut queue, &SystemProcesses, &LocalRunFiles, &*clock, id)
}

/// `ask`: register an ask and, when it is new, tell a person with one
/// `cmux notify` aimed at the inbox workspace `up` recorded (see
/// [`crate::application::ask::ask`]).
pub fn ask(db: &Path, checkout: &Path, ask: NewAsk, cmux: &dyn WorkspaceBackend) -> Result<Value> {
    crate::application::ask::ask(&mut SqliteQueue::open(db)?, checkout, ask, cmux)
}

/// Where the queue of `location` lives, as `up` and `down` take it.
fn queue_paths(location: &QueueLocation) -> QueuePaths {
    QueuePaths {
        db: location.db.clone(),
        hash: location.hash(),
        label: location.label.clone(),
        launch_agent: location.launch_agent.clone(),
        log_dir: location.log_dir.clone(),
    }
}

/// The adapters `up` and `down` run on: the queue at a path through
/// `SqliteQueue` with the system clock and IDs, Git for the repository,
/// the local files and Claude Code's global config for the folder trust.
fn lifecycle_ports<'a>(
    cmux: &'a dyn WorkspaceBackend,
    launchd: &'a dyn LaunchAgent,
    processes: &'a dyn ProcessControl,
    clock: &'a dyn crate::application::Clock,
    queues: &'a dyn Fn(&Path) -> Arc<dyn QueueOpener>,
    inspect_repository: &'a dyn Fn(&Path) -> Result<RepositoryPaths>,
) -> LifecyclePorts<'a> {
    LifecyclePorts {
        cmux,
        launchd,
        processes,
        files: &LocalRunFiles,
        clock,
        queues,
        inspect_repository,
        trusts_repository: &claude_trusts_repository,
        load_average,
    }
}

fn open_queues(db: &Path) -> Arc<dyn QueueOpener> {
    Arc::new(SqliteOpener {
        db: db.to_path_buf(),
        generators: clock::system(),
    })
}

fn inspect_repository(repo: &Path) -> Result<RepositoryPaths> {
    let repository = GitRepository::inspect(repo)?;
    Ok(RepositoryPaths {
        root: repository.root,
        common_dir: repository.common_dir,
    })
}

/// `up`: see [`lifecycle::up`]. `repo` is any checkout of the repository.
pub fn up(
    location: &QueueLocation,
    repo: &Path,
    cmux: &dyn WorkspaceBackend,
    launchd: &dyn LaunchAgent,
    processes: &dyn ProcessControl,
    environment: &UpEnvironment,
    options: &UpOptions,
) -> Result<Value> {
    let claude = ClaudeCode {
        executable: options.claude.clone(),
    };
    let generators = clock::system();
    lifecycle::up(
        &lifecycle_ports(
            cmux,
            launchd,
            processes,
            &*generators.clock,
            &open_queues,
            &inspect_repository,
        ),
        &claude,
        &queue_paths(location),
        repo,
        environment,
        options,
    )
}

/// `down`: see [`lifecycle::down`].
pub fn down(
    location: &QueueLocation,
    cmux: &dyn WorkspaceBackend,
    launchd: &dyn LaunchAgent,
    processes: &dyn ProcessControl,
    options: &DownOptions,
) -> Result<Value> {
    let generators = clock::system();
    lifecycle::down(
        &lifecycle_ports(
            cmux,
            launchd,
            processes,
            &*generators.clock,
            &open_queues,
            &inspect_repository,
        ),
        &queue_paths(location),
        options,
    )
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

/// `review`: see [`reviewing::review`]. The run's checkout is opened as a
/// Git repository.
pub fn review(db: &Path, task_id: TaskId) -> Result<Value> {
    let mut queue = SqliteQueue::open(db)?;
    let open_repository = |checkout: &Path| -> Result<Box<dyn Repository>> {
        Ok(Box::new(GitRepository::inspect(checkout)?))
    };
    reviewing::review(
        Review {
            queue: &mut queue,
            files: &LocalRunFiles,
            open_repository: &open_repository,
            pid: std::process::id(),
        },
        task_id,
    )
}

/// `rebind`: bind the queue at `db` to the repository containing `repo`
/// (see [`rebinding::rebind`]).
pub fn rebind(db: &Path, repo: &Path) -> Result<Value> {
    let db = db
        .canonicalize()
        .context("queue must already be initialized")?;
    let mut queue = SqliteQueue::open(&db)?;
    let repository = GitRepository::inspect(repo)?;
    let common_dir = path_text(&repository.common_dir)?;
    let location = QueueLocation::explicit(&db);
    let repository_queue_dir = data_home()
        .ok()
        .map(|home| QueueLocation::for_repository(&repository.common_dir, &home).queue_dir);
    let clock = queue.generators().clock.clone();
    rebinding::rebind(
        Rebind {
            queue: &mut queue,
            repository: &repository,
            files: &LocalRunFiles,
            processes: &SystemProcesses,
            clock: &*clock,
        },
        RebindTarget {
            repository_file: location.queue_dir.join(REPOSITORY_FILE_NAME),
            db,
            common_dir,
            queue_dir: location.queue_dir,
            log_dir: location.log_dir,
            repository_queue_dir,
        },
    )
}

/// `stats`: see [`statistics::stats`], measured to the queue clock's now.
pub fn stats(db: &Path, query: &StatsQuery) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let now = queue.generators().clock.now();
    Ok(serde_json::to_value(statistics::stats(
        &queue,
        &SystemProcesses,
        now,
        query,
    )?)?)
}
