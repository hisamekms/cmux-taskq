//! The composition root: each entry point opens the queue and the
//! repository, builds the adapters of the ports (`SqliteQueue`,
//! `GitRepository`, `Cmux`-backed recording, `ClaudeCode`, `Launchctl`'s
//! port, `SystemProcesses`, `LocalRunFiles`, the system clock and IDs) and
//! calls the use case in `application` (ADR-0013). `main` resolves the
//! queue location, assembles the clock and IDs once ([`OneShot`] and
//! [`SuperviseOptions::generators`]), parses the CLI and prints what these
//! return; `runtime` and `lifecycle` re-export them under the names the
//! tests use.

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
        AgentProvider, Generators, LaunchAgent, MainRemote, ProcessControl, QueueOpener,
        Repository, Spawner, WorkspaceBackend, health,
        integrate::{self as integration, IntegrateTarget, Integration},
        lifecycle::{
            self, DownOptions, Ports as LifecyclePorts, QUEUE_ENV, QueuePaths, REVIEWER_ROLE,
            ROLE_ENV, RepositoryPaths, UpEnvironment, UpOptions, session_env,
        },
        planner::{self, PlannerLaunch, PlannerProbes, PlannerWrapper},
        prompt,
        rebind::{self as rebinding, Rebind, RebindTarget},
        recording::RecordingBackend,
        review::{self as reviewing, Review},
        session::{self as wrapper, Session},
        stats::{self as statistics, StatsSources, WorkspaceListing},
        supervise::{self as supervisor, Heartbeat, Layout, LoopSettings, Ports},
    },
    domain::{
        IntegrationOutcome, NewAsk, PlannerId, RunId, SessionRole, TaskDetail, TaskId, TaskRun,
        stall::StallConfig,
        stats::{ConflictConfigReport, StatsQuery},
    },
    infrastructure::{
        adapters::{
            ClaudeCode, GitRepository, SystemProcesses, claude_trusts_repository, load_average,
            path_text,
        },
        clock,
        location::{
            QueueLocation, REPOSITORY_FILE_NAME, data_home, plan_reviews_dir, planners_dir,
            runs_dir,
        },
        process::LocalSpawner,
        run_env::{ShellVerifier, load_conflict_config, load_stall_config},
        run_files::LocalRunFiles,
        runtime_store::SqliteOpener,
        sqlite::SqliteQueue,
    },
};

const IDLE_POLL: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_secs(1);
/// How often the supervisor sweeps the workspaces of ended runs.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// The default planner timeout: an hour, like a run's resume timeout
/// (ADR-0041 decision 13).
pub const PLANNER_TIMEOUT: Duration = Duration::from_secs(3600);

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
    /// Least time between two sweeps of the workspaces of ended runs; tests
    /// shorten it.
    pub sweep_interval: Duration,
    /// The clock and IDs of everything the supervisor records; tests fix them.
    pub generators: Generators,
    /// The thresholds of the stalled-session checks; `None` reads `[stall]`
    /// from the `dagq.toml` of the repository's main checkout (ADR-0043
    /// decision 4). Tests set them.
    pub stall: Option<StallConfig>,
    /// Upper bound on the planners the runtime has open at once (ADR-0041
    /// decision 12); a person's planners do not count.
    pub runtime_planners: usize,
    /// How long a planner may take to submit a proposal sent back to it
    /// before the inbox is told (ADR-0041 decision 13).
    pub planner_timeout: Duration,
    /// The plugin directory the planners the runtime opens load.
    pub plugin_dir: Option<PathBuf>,
}

impl SuperviseOptions {
    pub fn new(parallel: usize, once: bool) -> Self {
        Self {
            parallel,
            once,
            stop: Arc::new(AtomicBool::new(false)),
            observe_interval: Duration::ZERO,
            observe_daily: false,
            tick: TICK,
            idle_poll: IDLE_POLL,
            sweep_interval: SWEEP_INTERVAL,
            generators: clock::system(),
            stall: None,
            runtime_planners: 1,
            planner_timeout: PLANNER_TIMEOUT,
            plugin_dir: None,
        }
    }

    fn settings(&self, stall: StallConfig, conflicts: ConflictConfigReport) -> LoopSettings {
        LoopSettings {
            parallel: self.parallel,
            once: self.once,
            stop: self.stop.clone(),
            observe_interval: self.observe_interval,
            observe_daily: self.observe_daily,
            tick: self.tick,
            idle_poll: self.idle_poll,
            sweep_interval: self.sweep_interval,
            stall,
            conflicts,
            runtime_planners: self.runtime_planners,
            planner_timeout: self.planner_timeout,
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
    let stall = match options.stall {
        Some(stall) => stall,
        None => load_stall_config(&main_checkout(&repository))?.unwrap_or_default(),
    };
    // Only for what the plan review is told: a `[conflicts]` that cannot
    // be read leaves the defaults rather than stopping the supervisor.
    let conflicts = statistics::conflict_config(
        load_conflict_config(&main_checkout(&repository)).unwrap_or_else(|error| {
            tracing::warn!(error = %format_args!("{error:#}"), "[conflicts] of dagq.toml not read: {error:#}; using the defaults");
            None
        }),
    );
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
        planners_dir: planners_dir(&db),
        plugin_dir: options
            .plugin_dir
            .as_deref()
            .map(|dir| {
                dir.canonicalize()
                    .with_context(|| format!("plugin directory {}", dir.display()))
            })
            .transpose()?,
        plan_reviews_dir: plan_reviews_dir(&db),
        db: db.clone(),
    };
    let agent = ClaudeCode {
        executable: claude.into(),
    };
    let review_db = db.clone();
    let review_material = move |task_id: TaskId| review(&review_db, task_id);
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
        signals: &agent,
        reviewer,
        spawner: &LocalSpawner,
        files: Arc::new(LocalRunFiles),
        processes: Arc::new(SystemProcesses),
        generators,
        review_material: &review_material,
        load_average,
        layout,
    };
    supervisor::supervise(&ports, &options.settings(stall, conflicts))
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

/// The one-shot entry points (`integrate`, `status`, `doctor`, `recover`,
/// `stats`, `rebind`, `up` and `down`) on the clock and IDs `main`
/// assembled once: the queue each of them opens reads the time and creates
/// IDs through `generators`, and so does the use case, instead of the
/// queue's own default (ADR-0013 policy 7). Tests fix them.
#[derive(Debug, Clone)]
pub struct OneShot {
    pub generators: Generators,
}

impl OneShot {
    pub fn new(generators: Generators) -> Self {
        Self { generators }
    }

    /// The wall clock and random UUIDs, what the binary runs with.
    pub fn system() -> Self {
        Self::new(clock::system())
    }

    /// The queue at `db`, writing through these generators.
    fn open(&self, db: &Path) -> Result<SqliteQueue> {
        Ok(SqliteQueue::open(db)?.with_generators(self.generators.clone()))
    }

    /// The queue at `db` on a read-only connection, for a command that only
    /// reads (ADR-0045 decision 18, [`SqliteQueue::open_read_only`]).
    fn open_read_only(&self, db: &Path) -> Result<SqliteQueue> {
        Ok(SqliteQueue::open_read_only(db)?.with_generators(self.generators.clone()))
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
    /// the repository and keeps the lease alive in between. The slot's token
    /// is one of these generators' IDs.
    pub fn integrate(
        &self,
        db: &Path,
        target: IntegrateTarget,
        repo: &Path,
        remote: Option<&dyn MainRemote>,
    ) -> Result<Value> {
        let db = db
            .canonicalize()
            .context("queue must already be initialized")?;
        let mut queue = self.open(&db)?;
        let repository = GitRepository::inspect(repo)?;
        let common_dir = path_text(&repository.common_dir)?;
        let verifier = ShellVerifier {
            checkout: main_checkout(&repository),
            db: db.clone(),
        };
        let mut integration = Integration {
            queue: &mut queue,
            repository: &repository,
            verifier: &verifier,
            remote,
            files: &LocalRunFiles,
            common_dir: &common_dir,
            clock: &*self.generators.clock,
            ids: &*self.generators.ids,
            processes: &SystemProcesses,
            pid: std::process::id(),
        };
        let Some(begun) = integration::begin(&mut integration, target, repo)? else {
            return Ok(serde_json::to_value(IntegrationOutcome::NoRunAwaiting)?);
        };
        let heartbeat = Heartbeat::start(
            Arc::new(SqliteOpener {
                db: db.clone(),
                generators: self.generators.clone(),
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

    /// `status --role`: see [`health::status`], measured to these
    /// generators' now.
    pub fn status_for(&self, db: &Path, role: Option<SessionRole>) -> Result<Value> {
        let queue = self.open_read_only(db)?;
        health::status(&queue, &SystemProcesses, &*self.generators.clock, role)
    }

    /// `doctor`: see [`health::doctor`].
    pub fn doctor(&self, db: &Path, full: bool) -> Result<Value> {
        let queue = self.open_read_only(db)?;
        health::doctor(
            &queue,
            &SystemProcesses,
            &LocalRunFiles,
            &*self.generators.clock,
            full,
        )
    }

    /// `recover`: see [`health::recover`].
    pub fn recover(&self, db: &Path, id: &RunId) -> Result<Value> {
        let mut queue = self.open(db)?;
        health::recover(
            &mut queue,
            &SystemProcesses,
            &LocalRunFiles,
            &*self.generators.clock,
            id,
        )
    }

    /// `stats`: see [`statistics::stats`], measured to these generators'
    /// now. The run directories are read from disk as Claude Code writes
    /// them, `[stall]` and `[conflicts]` from the `dagq.toml` of the main
    /// checkout of the repository the queue is bound to, main's history
    /// from that checkout, and the workspaces from
    /// `workspaces` (`None`: `workspace_mismatch` is not judged).
    pub fn stats(
        &self,
        db: &Path,
        query: &StatsQuery,
        workspaces: Option<&dyn WorkspaceListing>,
    ) -> Result<Value> {
        let queue = self.open_read_only(db)?;
        let now = self.generators.clock.now();
        let checkout = queue
            .repository_binding()?
            .map(PathBuf::from)
            .and_then(|common_dir| {
                (common_dir.file_name() == Some(".git".as_ref()))
                    .then(|| common_dir.parent().map(Path::to_path_buf))
                    .flatten()
            });
        let config_file = || match &checkout {
            Some(checkout) => load_stall_config(checkout),
            None => Ok(None),
        };
        let conflicts_file = || match &checkout {
            Some(checkout) => load_conflict_config(checkout),
            None => Ok(None),
        };
        let history = |since| match &checkout {
            Some(checkout) => GitRepository::inspect(checkout)?.main_history(since),
            None => anyhow::bail!("the queue is bound to no repository checkout"),
        };
        let signals = ClaudeCode {
            executable: PathBuf::from("claude"),
        };
        let queue_hash = QueueLocation::explicit(db).hash();
        let sources = StatsSources {
            files: &LocalRunFiles,
            signals: &signals,
            workspaces,
            queue_hash: &queue_hash,
            config_file: &config_file,
            conflicts_file: &conflicts_file,
            history: &history,
        };
        Ok(serde_json::to_value(statistics::stats(
            &queue,
            &SystemProcesses,
            now,
            query,
            &sources,
        )?)?)
    }

    /// `rebind`: bind the queue at `db` to the repository containing `repo`
    /// (see [`rebinding::rebind`]).
    pub fn rebind(&self, db: &Path, repo: &Path) -> Result<Value> {
        let db = db
            .canonicalize()
            .context("queue must already be initialized")?;
        let mut queue = self.open(&db)?;
        let repository = GitRepository::inspect(repo)?;
        let common_dir = path_text(&repository.common_dir)?;
        let location = QueueLocation::explicit(&db);
        let repository_queue_dir = data_home()
            .ok()
            .map(|home| QueueLocation::for_repository(&repository.common_dir, &home).queue_dir);
        rebinding::rebind(
            Rebind {
                queue: &mut queue,
                repository: &repository,
                files: &LocalRunFiles,
                processes: &SystemProcesses,
                clock: &*self.generators.clock,
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

    /// `up`: see [`lifecycle::up`]. `repo` is any checkout of the repository.
    #[allow(clippy::too_many_arguments)]
    pub fn up(
        &self,
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
        let queues = |db: &Path| self.queues(db);
        lifecycle::up(
            &self.lifecycle_ports(cmux, launchd, processes, &queues),
            &claude,
            &queue_paths(location),
            repo,
            environment,
            options,
        )
    }

    /// `down`: see [`lifecycle::down`].
    pub fn down(
        &self,
        location: &QueueLocation,
        cmux: &dyn WorkspaceBackend,
        launchd: &dyn LaunchAgent,
        processes: &dyn ProcessControl,
        options: &DownOptions,
    ) -> Result<Value> {
        let queues = |db: &Path| self.queues(db);
        lifecycle::down(
            &self.lifecycle_ports(cmux, launchd, processes, &queues),
            &queue_paths(location),
            options,
        )
    }

    /// Open a planner a person talks with (`dagq plan`, ADR-0041 decision
    /// 6): preflight cmux and `options.claude`, then open a new workspace
    /// next to any planner still open (see
    /// [`planner::open_person_planner`]). `repo` is the checkout the
    /// planner works in.
    pub fn plan(
        &self,
        location: &QueueLocation,
        repo: &Path,
        cmux: &dyn WorkspaceBackend,
        options: &PlanOptions,
    ) -> Result<Value> {
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
        let queue = self.open(&db)?;
        let recording = RecordingBackend::over(cmux, self.queues(&db), None, load_average);
        let opened = planner::open_person_planner(&PlannerLaunch {
            queue: &queue,
            cmux: &recording,
            files: &LocalRunFiles,
            db: &db,
            queue_hash: &QueueLocation::explicit(&db).hash(),
            planners_dir: &planners_dir(&db),
            repo_root: &repository.root,
            runner: &options.runner,
            claude: &options.claude,
            plugin_dir: plugin_dir.as_deref(),
        })?;
        Ok(serde_json::to_value(opened)?)
    }

    /// `planners`: every planner not closed (with `all`, every one), with
    /// its state judged by [`planner::planner_views`].
    pub fn planners(&self, db: &Path, cmux: &dyn WorkspaceBackend, all: bool) -> Result<Value> {
        let queue = self.open_read_only(db)?;
        // Claude Code's signals only read what its hook and screen show.
        let signals = ClaudeCode {
            executable: PathBuf::from("claude"),
        };
        let views = planner::planner_views(
            &queue,
            &PlannerProbes {
                cmux,
                processes: &SystemProcesses,
                files: &LocalRunFiles,
                signals: &signals,
                clock: &*self.generators.clock,
                planners_dir: &planners_dir(db),
            },
            all,
        )?;
        Ok(serde_json::json!({ "planners": views }))
    }

    /// The queue at a path, as `up` and `down` open it, writing through
    /// these generators.
    fn queues(&self, db: &Path) -> Arc<dyn QueueOpener> {
        Arc::new(SqliteOpener {
            db: db.to_path_buf(),
            generators: self.generators.clone(),
        })
    }

    /// The adapters `up` and `down` run on: the queue at a path through
    /// `SqliteQueue` with these generators, Git for the repository, the
    /// local files and Claude Code's global config for the folder trust.
    fn lifecycle_ports<'a>(
        &'a self,
        cmux: &'a dyn WorkspaceBackend,
        launchd: &'a dyn LaunchAgent,
        processes: &'a dyn ProcessControl,
        queues: &'a dyn Fn(&Path) -> Arc<dyn QueueOpener>,
    ) -> LifecyclePorts<'a> {
        LifecyclePorts {
            cmux,
            launchd,
            processes,
            files: &LocalRunFiles,
            clock: &*self.generators.clock,
            queues,
            inspect_repository: &inspect_repository,
            trusts_repository: &claude_trusts_repository,
            load_average,
        }
    }
}

/// `integrate` on the system clock and IDs: see [`OneShot::integrate`].
pub fn integrate(
    db: &Path,
    target: IntegrateTarget,
    repo: &Path,
    remote: Option<&dyn MainRemote>,
) -> Result<Value> {
    OneShot::system().integrate(db, target, repo, remote)
}

/// `status`: see [`status_for`], with all of the attention.
pub fn status(db: &Path) -> Result<Value> {
    status_for(db, None)
}

/// `status --role` on the system clock: see [`OneShot::status_for`].
pub fn status_for(db: &Path, role: Option<SessionRole>) -> Result<Value> {
    OneShot::system().status_for(db, role)
}

/// `doctor` on the system clock: see [`OneShot::doctor`].
pub fn doctor(db: &Path, full: bool) -> Result<Value> {
    OneShot::system().doctor(db, full)
}

/// `recover` on the system clock: see [`OneShot::recover`].
pub fn recover(db: &Path, id: &RunId) -> Result<Value> {
    OneShot::system().recover(db, id)
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

fn inspect_repository(repo: &Path) -> Result<RepositoryPaths> {
    let repository = GitRepository::inspect(repo)?;
    Ok(RepositoryPaths {
        root: repository.root,
        common_dir: repository.common_dir,
    })
}

/// `up` on the system clock: see [`OneShot::up`].
pub fn up(
    location: &QueueLocation,
    repo: &Path,
    cmux: &dyn WorkspaceBackend,
    launchd: &dyn LaunchAgent,
    processes: &dyn ProcessControl,
    environment: &UpEnvironment,
    options: &UpOptions,
) -> Result<Value> {
    OneShot::system().up(
        location,
        repo,
        cmux,
        launchd,
        processes,
        environment,
        options,
    )
}

/// `down` on the system clock: see [`OneShot::down`].
pub fn down(
    location: &QueueLocation,
    cmux: &dyn WorkspaceBackend,
    launchd: &dyn LaunchAgent,
    processes: &dyn ProcessControl,
    options: &DownOptions,
) -> Result<Value> {
    OneShot::system().down(location, cmux, launchd, processes, options)
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
/// [`prompt::triage_prompt`]), its files read from `dir`.
pub fn triage_prompt(
    detail: &TaskDetail,
    run: &TaskRun,
    resumes: usize,
    dir: &Path,
) -> Result<String> {
    prompt::triage_prompt(&LocalRunFiles, detail, run, resumes, dir)
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
    run_session(db, id, token, &provider, &LocalSpawner, resume)
}

/// The wrapper with `provider`'s agent started by `spawner`.
pub fn session_with_provider(
    db: &Path,
    id: &RunId,
    token: &str,
    provider: &dyn AgentProvider,
    spawner: &dyn Spawner,
) -> Result<Value> {
    run_session(db, id, token, provider, spawner, false)
}

/// The wrapper of a resumed session: `session --resume`.
pub fn resume_session_with_provider(
    db: &Path,
    id: &RunId,
    token: &str,
    provider: &dyn AgentProvider,
    spawner: &dyn Spawner,
) -> Result<Value> {
    run_session(db, id, token, provider, spawner, true)
}

fn run_session(
    db: &Path,
    id: &RunId,
    token: &str,
    provider: &dyn AgentProvider,
    spawner: &dyn Spawner,
    resume: bool,
) -> Result<Value> {
    let mut queue = SqliteQueue::open(db)?;
    wrapper::run_session(
        Session {
            queue: &mut queue,
            provider,
            spawner,
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

/// What `dagq plan` opens a planner with: the resolved Claude Code
/// executable, the plugin directory its session loads, and the binary its
/// workspace runs as the session wrapper (this one).
#[derive(Debug, Clone)]
pub struct PlanOptions {
    pub claude: PathBuf,
    pub plugin_dir: Option<PathBuf>,
    pub runner: PathBuf,
}

/// `plan` on the system clock: see [`OneShot::plan`].
pub fn plan(
    location: &QueueLocation,
    repo: &Path,
    cmux: &dyn WorkspaceBackend,
    options: &PlanOptions,
) -> Result<Value> {
    OneShot::system().plan(location, repo, cmux, options)
}

/// `planners` on the system clock: see [`OneShot::planners`].
pub fn planners(db: &Path, cmux: &dyn WorkspaceBackend, all: bool) -> Result<Value> {
    OneShot::system().planners(db, cmux, all)
}

/// The session wrapper of a planner (`planner-session`), run from its cmux
/// workspace: stdout must remain a terminal for Claude.
pub fn planner_session(
    db: &Path,
    id: PlannerId,
    claude: &Path,
    plugin_dir: Option<&Path>,
) -> Result<Value> {
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "interactive Claude wrapper requires a terminal"
    );
    let provider = ClaudeCode {
        executable: claude.into(),
    };
    planner_session_with_provider(db, id, &provider, plugin_dir)
}

/// [`planner_session`] with any provider, in the working directory.
pub fn planner_session_with_provider(
    db: &Path,
    id: PlannerId,
    provider: &dyn AgentProvider,
    plugin_dir: Option<&Path>,
) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let cwd = std::env::current_dir().context("working directory is unavailable")?;
    planner::run_planner_session(
        PlannerWrapper {
            queue: &queue,
            provider,
            spawner: &LocalSpawner,
            files: &LocalRunFiles,
            pid: std::process::id(),
        },
        id,
        &planner::planner_dir(&planners_dir(db), id),
        &cwd,
        plugin_dir,
    )
}

/// `rebind` on the system clock: see [`OneShot::rebind`].
pub fn rebind(db: &Path, repo: &Path) -> Result<Value> {
    OneShot::system().rebind(db, repo)
}

/// `stats` on the system clock without cmux: see [`OneShot::stats`].
pub fn stats(db: &Path, query: &StatsQuery) -> Result<Value> {
    OneShot::system().stats(db, query, None)
}
