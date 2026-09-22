use std::{
    env,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use cmux_taskq::{
    application::TaskQueue,
    domain::{NewTask, TaskAction},
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
    /// List ready tasks whose prerequisites are all completed; does not claim.
    Candidates,
    /// Run and monitor one task. Run this in a dedicated terminal.
    Supervise {
        /// Checkout of the repository whose `main` becomes the base commit;
        /// defaults to the working directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// cmux executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
        /// Claude Code executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
    },
    /// Confirm that a task's awaiting run was merged into main and complete the task.
    Integrate {
        id: i64,
        /// Checkout of the repository to check; defaults to the working directory.
        /// Must be the repository the queue is bound to.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Inspect supervisor ownership and heartbeat without changing it.
    Status,
    /// Report the lease, unfinished runs, their processes, heartbeats and paths without changing state.
    Doctor,
    /// Mark an unfinished run interrupted once its processes and supervisor are gone; keeps its worktree and workspace.
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
        } => serde_json::to_value(queue.add(NewTask {
            title,
            description,
            acceptance,
            verification_commands,
            dependencies,
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
        Command::Candidates => serde_json::to_value(queue.candidates()?)?,
        Command::Status => {
            let lease = queue.supervisor_lease()?;
            let stale = lease.as_ref().map(|l| {
                cmux_taskq::runtime::unix_time() - l.heartbeat_at
                    > cmux_taskq::infrastructure::runtime_store::HEARTBEAT_TIMEOUT_SECS
            });
            json!({"supervisor": lease, "heartbeat_stale": stale})
        }
        Command::Supervise { repo, cmux, claude } => {
            use cmux_taskq::infrastructure::adapters::{Cmux, executable};
            cmux_taskq::runtime::supervise(
                &db,
                &checkout(repo),
                &Cmux {
                    executable: executable(&cmux)?,
                },
                &executable(&claude)?,
                &env::current_exe()?,
            )?
        }
        Command::Integrate { id, repo } => {
            cmux_taskq::runtime::integrate(&db, id, &checkout(repo))?
        }
        Command::Doctor => cmux_taskq::runtime::doctor(&db)?,
        Command::Recover { run } => cmux_taskq::runtime::recover(&db, &run)?,
        Command::Session { run, lease, claude } => {
            cmux_taskq::runtime::session(&db, &run, &lease, &claude)?
        }
    })
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
