use std::{
    env,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use cmux_taskq::{
    application::TaskQueue,
    domain::{GoalEdit, GoalVerdict, NewGoal, NewTask, TaskAction},
    infrastructure::{adapters::path_text, location::QueueLocation, sqlite::SqliteQueue},
};

#[derive(Parser)]
#[command(
    version,
    about = "Manage a local dependency-aware task queue (JSON output)"
)]
struct Cli {
    /// Queue database path. Without it, the queue of the repository containing
    /// the working directory is used: $XDG_DATA_HOME/cmux-taskq/<hash>/queue.db.
    #[arg(long)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize or migrate the queue, creating its directory if needed.
    Init,
    /// Show which queue this directory resolves to, without opening it.
    Locate,
    /// Register a draft task; verification commands are stored, not executed.
    Add {
        title: String,
        #[arg(long, default_value = "")]
        description: String,
        #[arg(long, default_value = "")]
        acceptance: String,
        #[arg(long = "verify")]
        verification_commands: Vec<String>,
        #[arg(long = "depends-on")]
        dependencies: Vec<i64>,
        /// Open goal the task belongs to.
        #[arg(long = "goal")]
        goal_id: Option<i64>,
        /// Why the task exists and what to read first; shown to the worker.
        #[arg(long, default_value = "")]
        context: String,
    },
    /// List all tasks in registration order.
    List,
    /// Show a task, its dependencies, run history, and events.
    Show { id: i64 },
    /// Make a draft task ready (dependencies may still block execution).
    Ready { id: i64 },
    /// Return a ready task to draft.
    Draft { id: i64 },
    /// Cancel a draft or ready task. Does not satisfy its dependents.
    Cancel { id: i64 },
    /// Manage prerequisites; TASK depends on PREDECESSOR.
    Dependency {
        #[command(subcommand)]
        command: DependencyCommand,
    },
    /// Manage goals: the higher-level problems that groups of tasks solve.
    Goal {
        #[command(subcommand)]
        command: GoalCommand,
    },
    /// Move a draft or ready task to an open goal, or out of its goal with --none.
    SetGoal {
        /// Draft or ready task to move.
        task: i64,
        /// Open goal to join; omit it and pass --none to leave the current goal.
        #[arg(required_unless_present = "none", conflicts_with = "none")]
        goal: Option<i64>,
        /// Remove the task from its goal.
        #[arg(long)]
        none: bool,
    },
    /// List ready tasks whose prerequisites are all completed; does not claim.
    Candidates,
    /// Run and monitor tasks in parallel until interrupted. Run this in a dedicated terminal.
    Supervise {
        /// Checkout of the repository whose `main` becomes the base commit;
        /// defaults to the working directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Maximum number of runs executing at once.
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..))]
        parallel: u16,
        /// Exit once no run is active and no task can be claimed, instead of
        /// waiting for new work.
        #[arg(long)]
        once: bool,
        /// cmux executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
        /// Claude Code executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
    },
    /// Land a validated run on main: rebase, re-validate, squash into one commit, complete the task.
    Integrate {
        /// Task whose run awaits integration or comes back from a session.
        #[arg(required_unless_present = "next", conflicts_with = "next")]
        id: Option<i64>,
        /// Land the oldest run awaiting integration instead of naming a task.
        #[arg(long)]
        next: bool,
        /// Checkout of the repository to land in; defaults to the working directory.
        /// Must be the repository the queue is bound to.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// List live supervisors and unfinished runs with their leases without changing anything.
    Status,
    /// Report every unfinished run with its lease, processes, heartbeats and paths without changing state.
    Doctor,
    /// Mark one unfinished run interrupted once its processes and supervisor are gone; keeps its worktree and workspace and leaves other runs alone.
    Recover {
        /// Run ID from `show` or `doctor`.
        run: String,
    },
    #[command(hide = true)]
    Session {
        #[arg(long)]
        run: String,
        #[arg(long)]
        lease: String,
        #[arg(long)]
        claude: PathBuf,
    },
}

#[derive(Subcommand)]
enum DependencyCommand {
    Add { task: i64, predecessor: i64 },
    Remove { task: i64, predecessor: i64 },
}

#[derive(Subcommand)]
enum GoalCommand {
    /// Register a goal; it has no state machine and no verification commands.
    Add {
        title: String,
        #[arg(long, default_value = "")]
        description: String,
        #[arg(long, default_value = "")]
        acceptance: String,
        /// Naming, boundaries, and what not to do, shared by every task of the goal.
        #[arg(long, default_value = "")]
        constraints: String,
        /// Path of a reference document inside the repository.
        #[arg(long)]
        doc: Option<String>,
    },
    /// List goals with their task counts by status.
    List,
    /// Show a goal, its tasks, and its events.
    Show { id: i64 },
    /// Replace fields of a goal; runs already started keep their prompt.
    #[command(group = clap::ArgGroup::new("field").multiple(true).required(true))]
    Edit {
        id: i64,
        #[arg(long, group = "field")]
        title: Option<String>,
        #[arg(long, group = "field")]
        description: Option<String>,
        #[arg(long, group = "field")]
        acceptance: Option<String>,
        #[arg(long, group = "field")]
        constraints: Option<String>,
        /// New document path; an empty value clears it.
        #[arg(long, group = "field")]
        doc: Option<String>,
    },
    /// Record the verdict once. `achieved` needs every task completed or canceled; `abandoned` needs no task in progress.
    Close {
        id: i64,
        #[arg(long, value_parser = ["achieved", "abandoned"])]
        verdict: String,
    },
}

fn execute(cli: Cli) -> Result<Value> {
    let cwd = env::current_dir().context("working directory is unavailable")?;
    let location = QueueLocation::resolve(cli.db.as_deref(), &cwd)?;
    let db = location.db.clone();
    // The binding is checked on every command of a repository queue; a `--db`
    // queue is bound by its first `supervise` and checked there and by `integrate`.
    let common_dir = location
        .git_common_dir
        .as_deref()
        .map(path_text)
        .transpose()?;
    if matches!(cli.command, Command::Locate) {
        let mut value = serde_json::to_value(&location)?;
        value["db_exists"] = json!(db.is_file());
        return Ok(value);
    }
    if matches!(cli.command, Command::Init) {
        location.prepare()?;
        let mut queue = SqliteQueue::init(&db)?;
        if let Some(common_dir) = &common_dir {
            queue.bind_repository(common_dir)?;
        }
        return Ok(json!({
            "db": db,
            "schema_version": queue.schema_version()?,
            "source": location.source,
            "git_common_dir": common_dir,
        }));
    }
    let mut queue = SqliteQueue::open(&db)?;
    if let Some(common_dir) = &common_dir {
        queue.assert_repository(common_dir)?;
    }
    // A repository queue already resolved the working directory; `--repo`
    // overrides it for a `--db` queue used from elsewhere or a moved checkout.
    let checkout = |repo: Option<PathBuf>| repo.unwrap_or_else(|| cwd.clone());
    Ok(match cli.command {
        Command::Init | Command::Locate => unreachable!(),
        Command::Add {
            title,
            description,
            acceptance,
            verification_commands,
            dependencies,
            goal_id,
            context,
        } => serde_json::to_value(queue.add(NewTask {
            title,
            description,
            acceptance,
            verification_commands,
            dependencies,
            goal_id,
            context,
        })?)?,
        Command::List => serde_json::to_value(queue.list()?)?,
        Command::Show { id } => serde_json::to_value(queue.show(id)?)?,
        Command::Ready { id } => serde_json::to_value(queue.transition(id, TaskAction::Ready)?)?,
        Command::Draft { id } => serde_json::to_value(queue.transition(id, TaskAction::Draft)?)?,
        Command::Cancel { id } => serde_json::to_value(queue.transition(id, TaskAction::Cancel)?)?,
        Command::Dependency { command } => {
            let id = match command {
                DependencyCommand::Add { task, predecessor } => {
                    queue.add_dependency(task, predecessor)?;
                    task
                }
                DependencyCommand::Remove { task, predecessor } => {
                    queue.remove_dependency(task, predecessor)?;
                    task
                }
            };
            serde_json::to_value(queue.show(id)?)?
        }
        Command::Goal { command } => match command {
            GoalCommand::Add {
                title,
                description,
                acceptance,
                constraints,
                doc,
            } => serde_json::to_value(queue.add_goal(NewGoal {
                title,
                description,
                acceptance,
                constraints,
                doc,
            })?)?,
            GoalCommand::List => serde_json::to_value(queue.list_goals()?)?,
            GoalCommand::Show { id } => serde_json::to_value(queue.show_goal(id)?)?,
            GoalCommand::Edit {
                id,
                title,
                description,
                acceptance,
                constraints,
                doc,
            } => serde_json::to_value(queue.edit_goal(
                id,
                GoalEdit {
                    title,
                    description,
                    acceptance,
                    constraints,
                    doc,
                },
            )?)?,
            GoalCommand::Close { id, verdict } => {
                serde_json::to_value(queue.close_goal(id, verdict.parse::<GoalVerdict>()?)?)?
            }
        },
        Command::SetGoal {
            task,
            goal,
            none: _,
        } => serde_json::to_value(queue.set_goal(task, goal)?)?,
        Command::Candidates => serde_json::to_value(queue.candidates()?)?,
        Command::Status => cmux_taskq::runtime::status(&db)?,
        Command::Supervise {
            repo,
            parallel,
            once,
            cmux,
            claude,
        } => {
            use cmux_taskq::infrastructure::adapters::{Cmux, executable};
            use cmux_taskq::runtime::SuperviseOptions;
            let options = SuperviseOptions {
                parallel: usize::from(parallel),
                once,
                stop: install_stop_signal()?,
            };
            cmux_taskq::runtime::supervise(
                &db,
                &checkout(repo),
                &Cmux {
                    executable: executable(&cmux)?,
                },
                &executable(&claude)?,
                &env::current_exe()?,
                &options,
            )?
        }
        Command::Integrate { id, next, repo } => {
            use cmux_taskq::runtime::IntegrateTarget;
            let target = match (id, next) {
                (Some(id), false) => IntegrateTarget::Task(id),
                _ => IntegrateTarget::Next,
            };
            cmux_taskq::runtime::integrate(&db, target, &checkout(repo))?
        }
        Command::Doctor => cmux_taskq::runtime::doctor(&db)?,
        Command::Recover { run } => cmux_taskq::runtime::recover(&db, &run)?,
        Command::Session { run, lease, claude } => {
            cmux_taskq::runtime::session(&db, &run, &lease, &claude)?
        }
    })
}

static STOP: OnceLock<Arc<AtomicBool>> = OnceLock::new();

extern "C" fn request_stop(signal: libc::c_int) {
    if let Some(stop) = STOP.get() {
        stop.store(true, Ordering::SeqCst);
    }
    // SAFETY: restoring the default disposition is async-signal-safe, so a
    // second signal terminates the process the usual way.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
    }
}

/// The first SIGINT/SIGTERM asks the supervisor to stop claiming and drain its
/// active runs; the second one terminates it (leases then go stale).
fn install_stop_signal() -> Result<Arc<AtomicBool>> {
    let stop = STOP
        .get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone();
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the handler only stores an atomic and resets the disposition.
        let previous =
            unsafe { libc::signal(signal, request_stop as extern "C" fn(libc::c_int) as usize) };
        anyhow::ensure!(previous != libc::SIG_ERR, "install signal handler");
    }
    Ok(stop)
}

fn main() -> ExitCode {
    let result = execute(Cli::parse()).and_then(|value| {
        let mut stdout = io::stdout().lock();
        serde_json::to_writer_pretty(&mut stdout, &value)?;
        writeln!(stdout)?;
        Ok(())
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", json!({"error": format!("{error:#}")}));
            ExitCode::FAILURE
        }
    }
}
