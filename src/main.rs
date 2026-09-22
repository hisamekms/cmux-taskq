use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use cmux_taskq::{
    application::TaskQueue,
    domain::{NewTask, TaskAction},
    infrastructure::sqlite::SqliteQueue,
};

#[derive(Parser)]
#[command(
    version,
    about = "Manage a local dependency-aware task queue (JSON output)"
)]
struct Cli {
    /// Queue database path. Only `init` creates a database.
    #[arg(long)]
    db: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize or migrate a queue. The parent directory must exist.
    Init,
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
        /// Repository whose `main` becomes the base commit and worktree source.
        #[arg(long)]
        repo: PathBuf,
        /// cmux executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
        /// Claude Code executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
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
    if matches!(cli.command, Command::Init) {
        let queue = SqliteQueue::init(&cli.db)?;
        return Ok(json!({"db": cli.db, "schema_version": queue.schema_version()?}));
    }
    let mut queue = SqliteQueue::open(&cli.db)?;
    Ok(match cli.command {
        Command::Init => unreachable!(),
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
                &cli.db,
                &repo,
                &Cmux {
                    executable: executable(&cmux)?,
                },
                &executable(&claude)?,
                &std::env::current_exe()?,
            )?
        }
        Command::Doctor => cmux_taskq::runtime::doctor(&cli.db)?,
        Command::Recover { run } => cmux_taskq::runtime::recover(&cli.db, &run)?,
        Command::Session { run, lease, claude } => {
            cmux_taskq::runtime::session(&cli.db, &run, &lease, &claude)?
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
