use std::{
    env,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use dagq::{
    application::{StatusFilter, TaskQuery, TaskStore, dependency_graph},
    domain::{
        AskKind, GoalEdit, GoalVerdict, NewAsk, NewGoal, NewNote, NewTask, NoteQuery, NoteTarget,
        SessionRole, TaskAction, TaskStatus,
    },
    infrastructure::{adapters::path_text, location::QueueLocation, sqlite::SqliteQueue},
};

#[derive(Parser)]
#[command(
    version,
    about = "Manage a local dependency-aware task queue (JSON output)"
)]
struct Cli {
    /// Queue database path. Without it, the queue of the repository containing
    /// the working directory is used: $XDG_DATA_HOME/dagq/<hash>/queue.db.
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
        /// Receipt check validation requires to be passed with evidence; repeatable.
        /// A receipt without it parks the run as needs_session (evidence_missing).
        #[arg(long = "evidence", value_parser = ["tests", "e2e", "subagent_review"])]
        required_evidence: Vec<String>,
    },
    /// List one page of tasks, newest first: unfinished ones unless --status or --all says otherwise.
    /// Prints {"tasks", "next", "total"}; pass `next` to --before for the following page (null: none).
    List {
        /// Only these statuses (comma-separated, any of them): draft, ready, in_progress, completed, canceled.
        #[arg(long, value_delimiter = ',', conflicts_with = "all")]
        status: Vec<String>,
        /// Include completed and canceled tasks.
        #[arg(long)]
        all: bool,
        /// Only tasks of this goal.
        #[arg(long = "goal")]
        goal_id: Option<i64>,
        /// Page size.
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
        /// Start the page at this task ID (the previous page's `next`); lists IDs up to it.
        #[arg(long)]
        before: Option<i64>,
        /// Include description, acceptance, context, verification commands and timestamps.
        #[arg(long)]
        full: bool,
    },
    /// Show a task, its latest run and its latest events; long texts are cut
    /// to 300 characters (ending in `…`, with `truncated: true`).
    Show {
        id: i64,
        /// Print every run, event payload and process, and the texts in full.
        #[arg(long)]
        full: bool,
        /// How many of the latest events to show without --full.
        #[arg(long, default_value_t = dagq::view::DEFAULT_EVENTS, conflicts_with = "full")]
        events: usize,
    },
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
    /// Record a note (an `observation` run event) on a task, a run or a goal.
    #[command(group = clap::ArgGroup::new("target").required(true))]
    Note {
        #[arg(long, group = "target")]
        task: Option<i64>,
        #[arg(long, group = "target")]
        run: Option<String>,
        #[arg(long, group = "target")]
        goal: Option<i64>,
        #[arg(long)]
        text: String,
        /// Lowercase slug classifying the note (default: note).
        #[arg(long)]
        kind: Option<String>,
    },
    /// List notes oldest first: the latest --limit, or the next --limit after --since.
    /// Prints {"notes", "cursor"}; pass `cursor` to --since for the notes recorded later.
    Notes {
        /// Only notes on this goal and on its tasks and their runs.
        #[arg(long = "goal")]
        goal_id: Option<i64>,
        /// Only notes on this task and its runs.
        #[arg(long = "task")]
        task_id: Option<i64>,
        /// Event id (a previous `cursor`): only notes recorded after it.
        #[arg(long)]
        since: Option<i64>,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
    },
    /// List ready tasks whose prerequisites are all completed and whose goal is not a draft; does not claim.
    Candidates,
    /// Show the unfinished tasks' dependencies: per task its direct predecessors (`depends_on`),
    /// the unfinished ones (`ready_after`), the tasks it blocks directly and how many it
    /// releases transitively (`unblocks`); `candidates` in claim order and the `critical` chain.
    Graph {
        /// Only this goal's tasks and candidates; counts still span every goal.
        #[arg(long = "goal")]
        goal_id: Option<i64>,
    },
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
        /// Write one supervisor-<started_at>-<pid>.log per start into this
        /// directory (created if missing) in addition to stderr.
        #[arg(long)]
        log_dir: Option<PathBuf>,
        /// Start the observer job (`observe`) when this many seconds passed
        /// since the last one started or finished; 0 disables the observer.
        /// Default 3600, or 0 with --once.
        #[arg(long)]
        observe_interval: Option<u64>,
        /// Also run the daily observation of the last 24 hours once a day.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        observe_daily: bool,
    },
    /// Run the observer job once: headless Claude under DAGQ_ROLE=observer reads stats past the
    /// cursor, the latest notes, the open asks and the graph, and writes notes, blocked asks and
    /// draft goals only. Records observe_started / observe_finished and saves the new cursor.
    Observe {
        /// Event id to read stats past; defaults to the cursor the last observe saved
        /// (<queue dir>/observer/cursor), or with --daily the last event 24 hours ago.
        #[arg(long)]
        since: Option<i64>,
        /// Print the prompt instead of starting the agent.
        #[arg(long)]
        dry_run: bool,
        /// The daily observation: trends over the last 24 hours; leaves the cursor alone.
        #[arg(long)]
        daily: bool,
        /// Seconds the agent may run before it is killed.
        #[arg(long, default_value_t = 1800)]
        timeout: u64,
        /// Claude Code executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
    },
    /// Start the queue's runtime: a launchd-resident supervisor and the maintainer's cmux workspace. Idempotent; replaces a live supervisor of another version.
    Up {
        /// Maximum number of runs the supervisor executes at once.
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..))]
        parallel: u16,
        /// Run the supervisor in the cmux workspace `[<repo>]supervisor`
        /// instead of under launchd: no socket password needed, and nothing
        /// restarts it if it stops.
        #[arg(long)]
        in_cmux: bool,
        /// Do not wait for a supervisor of another version to drain: stop
        /// with an error instead when any run is still in flight.
        #[arg(long)]
        no_wait: bool,
        /// Claude Code plugin directory the maintainer session loads (`claude --plugin-dir`).
        #[arg(long)]
        plugin_dir: Option<PathBuf>,
        /// Checkout of the repository; defaults to the working directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// cmux executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
        /// Claude Code executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
    },
    /// Stop the queue's supervisor: unload its launchd agent so it drains and is not restarted, or signal and close the workspace of an in-cmux one. Leaves the maintainer workspace open.
    Down {
        /// Wait until the supervisor's registration is gone or its process exited.
        #[arg(long)]
        wait: bool,
        /// Kill the supervisor after the unload and drop its registration.
        #[arg(long, conflicts_with = "wait")]
        force: bool,
        /// cmux executable, used to close an in-cmux supervisor's workspace.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
    },
    /// Land a validated run on main: rebase, re-validate, squash into one commit, complete the task, then push main to origin.
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
        /// Do not push the landed main to origin (recorded as push_skipped).
        #[arg(long)]
        no_push: bool,
    },
    /// Write the review material of the task's run awaiting integration or a session to <run_dir>/review.md and report its path and diff size; the diff itself is only in the file.
    Review {
        /// Task whose run awaits integration or comes back from a session.
        id: i64,
    },
    /// List supervisors, unfinished runs, what waits for the maintainer (attention), the open asks and the event cursor, without changing anything.
    Status {
        /// Only the attention addressed to this role: inbox gets ask_opened, maintainer the rest.
        #[arg(long, value_parser = ROLES)]
        role: Option<String>,
    },
    /// Register a question for a person about a task or one of its runs; prints the ask.
    /// An open ask of the same task, run and kind is returned instead (`created: false`).
    #[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
    Ask {
        #[command(subcommand)]
        command: Option<AskCommand>,
        #[arg(long, required = true, value_parser = ["approve_landing", "answer_prompt", "decide", "worker_question", "blocked"])]
        kind: Option<String>,
        #[arg(long, required = true)]
        question: Option<String>,
        /// A choice to offer; repeat for several.
        #[arg(long = "option")]
        options: Vec<String>,
        /// Task the ask is about. Only a blocked ask may name neither a task nor a run.
        #[arg(long = "task", conflicts_with = "run")]
        task_id: Option<i64>,
        /// Run the ask is about (its task is implied).
        #[arg(long)]
        run: Option<String>,
    },
    /// Write the answer of an open ask; the maintainer then sees ask_answered.
    Answer {
        id: i64,
        #[arg(long)]
        text: String,
    },
    /// List asks nobody closed, oldest first.
    Asks {
        /// Only the unanswered ones.
        #[arg(long)]
        open: bool,
        /// Only the ones this role acts on: inbox answers open asks, maintainer reads answers.
        #[arg(long, value_parser = ROLES)]
        role: Option<String>,
        /// Include closed asks.
        #[arg(long)]
        all: bool,
    },
    /// Print the run events after a cursor, oldest first: attention events only unless --all. Reads only.
    Events {
        /// Event id to read past (the `cursor` of `status`, `events` or `watch`).
        #[arg(long, default_value_t = 0)]
        after: i64,
        /// Maximum number of events returned; the cursor then points at the last one.
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
        /// Every event kind, not only attention.
        #[arg(long)]
        all: bool,
    },
    /// Block until an attention event after the cursor arrives or the supervisors' health changes; returns empty on timeout. Reads only, never integrates.
    Watch {
        /// Event id to wait past; defaults to the newest event now.
        #[arg(long)]
        after: Option<i64>,
        /// Seconds to wait before returning with no events.
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// Seconds between reads of the queue.
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..))]
        interval: u64,
        /// Wake only for the attention addressed to this role: inbox for ask_opened,
        /// maintainer for the rest and the supervisors' health.
        #[arg(long, value_parser = ROLES)]
        role: Option<String>,
    },
    /// Per-run times in seconds (work, validate, wait_to_land, startup) and counts, per-goal and
    /// overall count/total/median, and alerts over thresholds, derived from run events. The latest
    /// 50 finished runs unless --full; pass `next_cursor` to --since for only the runs finished later.
    Stats {
        /// Event id (a previous `next_cursor`): only runs that finished after it.
        #[arg(long)]
        since: Option<i64>,
        /// Only runs of tasks in this goal.
        #[arg(long = "goal")]
        goal_id: Option<i64>,
        /// Every finished run instead of the latest 50 (or the next 50 past --since).
        #[arg(long)]
        full: bool,
    },
    /// Report every unfinished run and supervisor, one line's worth each, without changing state.
    Doctor {
        /// Include each run's lease, processes, heartbeats and paths, and every supervisor field.
        #[arg(long)]
        full: bool,
    },
    /// Bind the queue to the repository containing the working directory (or
    /// --repo) after the repository moved; the one command that changes the
    /// binding. Refused while a supervisor runs. Pass --db for a queue still
    /// in its old directory; `move_to` names where the repository now looks for it.
    Rebind {
        /// Checkout of the repository to bind to; defaults to the working directory.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
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
        /// Reopen the session of a `needs_session` run the supervisor resumes.
        #[arg(long)]
        resume: bool,
    },
}

/// The session roles attention is addressed to.
const ROLES: [&str; 3] = ["maintainer", "inbox", "planner"];

fn parse_role(value: Option<String>) -> Result<Option<SessionRole>> {
    Ok(value.map(|value| value.parse()).transpose()?)
}

#[derive(Subcommand)]
enum AskCommand {
    /// Mark an answered ask read. An open ask is withdrawn by answering it first.
    Close { id: i64 },
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
        /// Register a draft: its tasks are not candidates until `goal ready`.
        #[arg(long)]
        draft: bool,
    },
    /// Open a draft goal so the supervisor may claim its ready tasks.
    Ready { id: i64 },
    /// List goals with their status and task counts by status.
    List,
    /// Show a goal, its tasks, and the kinds of its latest 10 events; long
    /// texts are cut to 300 characters (ending in `…`, with `truncated: true`).
    Show {
        id: i64,
        /// Print the texts and every event with its payload in full.
        #[arg(long)]
        full: bool,
    },
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

/// The error of a command the observer may not run.
const OBSERVER_DENIED: &str = "observer may not change queue state";

/// What the observer's environment may run (ADR-0024 decision 4).
#[derive(Debug, PartialEq, Eq)]
enum ObserverAccess {
    Allowed,
    Denied,
    /// `add` into this goal, allowed only while it is a draft.
    DraftGoal(i64),
}

/// An allowlist: reads, notes, blocked asks, draft goals and tasks of a draft goal. Every
/// other command, including ones added later, is refused until listed here.
fn observer_access(command: &Command) -> ObserverAccess {
    match command {
        Command::Locate
        | Command::List { .. }
        | Command::Show { .. }
        | Command::Candidates
        | Command::Graph { .. }
        | Command::Status { .. }
        | Command::Asks { .. }
        | Command::Events { .. }
        | Command::Watch { .. }
        | Command::Stats { .. }
        | Command::Doctor { .. }
        | Command::Note { .. }
        | Command::Notes { .. }
        | Command::Goal {
            command:
                GoalCommand::List | GoalCommand::Show { .. } | GoalCommand::Add { draft: true, .. },
        } => ObserverAccess::Allowed,
        Command::Add {
            goal_id: Some(goal_id),
            ..
        } => ObserverAccess::DraftGoal(*goal_id),
        // The threshold crossings it raises to the inbox, and nothing else.
        Command::Ask {
            command: None,
            kind: Some(kind),
            ..
        } if kind == AskKind::Blocked.as_str() => ObserverAccess::Allowed,
        _ => ObserverAccess::Denied,
    }
}

fn execute(cli: Cli) -> Result<Value> {
    let role = env::var(dagq::lifecycle::ROLE_ENV)
        .ok()
        .filter(|role| !role.is_empty());
    let observer = role.as_deref() == Some(dagq::lifecycle::OBSERVER_ROLE);
    let access = if observer {
        observer_access(&cli.command)
    } else {
        ObserverAccess::Allowed
    };
    if access == ObserverAccess::Denied {
        bail!(OBSERVER_DENIED);
    }
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
    // A repository queue already resolved the working directory; `--repo`
    // overrides it for a `--db` queue used from elsewhere or a moved checkout.
    let checkout = |repo: Option<PathBuf>| repo.unwrap_or_else(|| cwd.clone());
    // `rebind` is the one command that runs on a queue bound elsewhere.
    if let Command::Rebind { repo } = cli.command {
        return dagq::runtime::rebind(&db, &checkout(repo));
    }
    let mut queue = SqliteQueue::open(&db)?;
    if let Some(common_dir) = &common_dir {
        queue.assert_repository(common_dir)?;
    }
    if let ObserverAccess::DraftGoal(goal_id) = access
        && !queue.show_goal(goal_id)?.goal.is_draft()
    {
        bail!(OBSERVER_DENIED);
    }
    Ok(match cli.command {
        Command::Init | Command::Locate | Command::Rebind { .. } => unreachable!(),
        Command::Add {
            title,
            description,
            acceptance,
            verification_commands,
            dependencies,
            goal_id,
            context,
            required_evidence,
        } => serde_json::to_value(
            queue.add(NewTask {
                title,
                description,
                acceptance,
                verification_commands,
                dependencies,
                goal_id,
                context,
                required_evidence: required_evidence
                    .iter()
                    .map(|name| name.parse())
                    .collect::<Result<_, _>>()?,
            })?,
        )?,
        Command::List {
            status,
            all,
            goal_id,
            limit,
            before,
            full,
        } => {
            let status = if all {
                StatusFilter::Any
            } else if status.is_empty() {
                StatusFilter::Open
            } else {
                StatusFilter::Only(
                    status
                        .iter()
                        .map(|value| value.trim().parse::<TaskStatus>())
                        .collect::<Result<_, _>>()?,
                )
            };
            serde_json::to_value(queue.list(&TaskQuery {
                status,
                goal_id,
                limit: usize::try_from(limit)?,
                before,
                full,
            })?)?
        }
        Command::Show { id, full, events } => {
            let detail = queue.show(id)?;
            if full {
                serde_json::to_value(detail)?
            } else {
                dagq::view::task_detail(&detail, events)
            }
        }
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
                draft,
            } => serde_json::to_value(queue.add_goal(NewGoal {
                title,
                description,
                acceptance,
                constraints,
                doc,
                draft,
            })?)?,
            GoalCommand::Ready { id } => serde_json::to_value(queue.ready_goal(id)?)?,
            GoalCommand::List => serde_json::to_value(queue.list_goals()?)?,
            GoalCommand::Show { id, full } => {
                let detail = queue.show_goal(id)?;
                if full {
                    serde_json::to_value(detail)?
                } else {
                    dagq::view::goal_detail(&detail)
                }
            }
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
        Command::Note {
            task,
            run,
            goal,
            text,
            kind,
        } => {
            let target = match (task, run, goal) {
                (Some(task), _, _) => NoteTarget::Task(task),
                (_, Some(run), _) => NoteTarget::Run(run),
                (_, _, goal) => NoteTarget::Goal(goal.context("note needs a target")?),
            };
            serde_json::to_value(queue.add_note(NewNote {
                target,
                text,
                kind,
                by: role.unwrap_or_else(|| "human".into()),
            })?)?
        }
        Command::Notes {
            goal_id,
            task_id,
            since,
            limit,
        } => serde_json::to_value(queue.notes(&NoteQuery {
            goal_id,
            task_id,
            since,
            limit: usize::try_from(limit)?,
        })?)?,
        Command::Candidates => serde_json::to_value(queue.candidates()?)?,
        Command::Graph { goal_id } => {
            serde_json::to_value(dependency_graph(queue.graph_input()?, goal_id))?
        }
        Command::Status { role: r } => dagq::runtime::status_for(&db, parse_role(r)?)?,
        Command::Ask {
            command: Some(AskCommand::Close { id }),
            ..
        } => serde_json::to_value(queue.close_ask(id)?)?,
        Command::Ask {
            command: None,
            kind,
            question,
            options,
            task_id,
            run,
        } => serde_json::to_value(queue.ask(NewAsk {
            kind: kind.unwrap_or_default().parse::<AskKind>()?,
            task_id,
            run_id: run,
            question: question.unwrap_or_default(),
            options,
            // The session's role; a person at a plain terminal has none.
            asked_by: role.unwrap_or_else(|| "human".into()),
        })?)?,
        Command::Answer { id, text } => serde_json::to_value(queue.answer(id, &text)?)?,
        Command::Asks { open, role: r, all } => {
            json!({"asks": queue.asks(dagq::infrastructure::asks::AskQuery {
            all,
            open,
            role: parse_role(r)?,
        })?})
        }
        Command::Events { after, limit, all } => {
            dagq::watch::events(&db, after, limit as usize, all)?
        }
        Command::Watch {
            after,
            timeout,
            interval,
            role: r,
        } => dagq::watch::watch(
            &db,
            &dagq::watch::WatchOptions {
                after,
                timeout: Duration::from_secs(timeout),
                interval: Duration::from_secs(interval),
                role: parse_role(r)?,
            },
        )?,
        Command::Supervise {
            repo,
            parallel,
            once,
            cmux,
            claude,
            log_dir,
            observe_interval,
            observe_daily,
        } => {
            use dagq::infrastructure::adapters::{Cmux, executable};
            use dagq::runtime::SuperviseOptions;
            let options = SuperviseOptions {
                parallel: usize::from(parallel),
                once,
                stop: install_stop_signal()?,
                log_dir,
                // A one-shot pass observes only when asked to.
                observe_interval: Duration::from_secs(observe_interval.unwrap_or(if once {
                    0
                } else {
                    3600
                })),
                observe_daily,
            };
            dagq::runtime::supervise(
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
        Command::Up {
            parallel,
            in_cmux,
            no_wait,
            plugin_dir,
            repo,
            cmux,
            claude,
        } => {
            use dagq::infrastructure::adapters::{SOCKET_PASSWORD_ENV, claude_global_config};
            use dagq::infrastructure::{
                adapters::{Cmux, SystemProcesses, executable},
                launchd::Launchctl,
            };
            use dagq::lifecycle::{QUEUE_ENV, ROLE_ENV, UpEnvironment, UpOptions};
            let environment = UpEnvironment {
                role: env::var(ROLE_ENV).ok(),
                queue: env::var_os(QUEUE_ENV).map(PathBuf::from),
                path: env::var("PATH").context("PATH is unset")?,
                socket_password: env::var(SOCKET_PASSWORD_ENV)
                    .ok()
                    .filter(|password| !password.is_empty()),
                current_exe: env::current_exe()?,
                claude_config: claude_global_config(
                    env::var("CLAUDE_CONFIG_DIR").ok().as_deref(),
                    env::var("HOME").ok().as_deref(),
                ),
            };
            let options = UpOptions {
                parallel,
                in_cmux,
                no_wait,
                plugin_dir,
                cmux: executable(&cmux)?,
                claude: executable(&claude)?,
                startup_timeout: Duration::from_secs(30),
                poll: Duration::from_millis(500),
            };
            dagq::lifecycle::up(
                &location,
                &checkout(repo),
                &Cmux {
                    executable: options.cmux.clone(),
                },
                &Launchctl { uid: current_uid() },
                &SystemProcesses,
                &environment,
                &options,
            )?
        }
        Command::Down { wait, force, cmux } => {
            use dagq::infrastructure::{
                adapters::{Cmux, SystemProcesses, executable},
                launchd::Launchctl,
            };
            use dagq::lifecycle::DownOptions;
            // cmux is only needed to close an in-cmux supervisor's
            // workspace, so a queue without one still goes down when cmux
            // is not installed; the unresolved name then fails only there.
            dagq::lifecycle::down(
                &location,
                &Cmux {
                    executable: executable(&cmux).unwrap_or(cmux),
                },
                &Launchctl { uid: current_uid() },
                &SystemProcesses,
                &DownOptions {
                    wait,
                    force,
                    poll: Duration::from_secs(2),
                },
            )?
        }
        Command::Integrate {
            id,
            next,
            repo,
            no_push,
        } => {
            use dagq::{infrastructure::adapters::GitRepository, runtime::IntegrateTarget};
            let target = match (id, next) {
                (Some(id), false) => IntegrateTarget::Task(id),
                _ => IntegrateTarget::Next,
            };
            let repo = checkout(repo);
            let remote = if no_push {
                None
            } else {
                Some(GitRepository::inspect(&repo)?)
            };
            dagq::runtime::integrate(
                &db,
                target,
                &repo,
                remote
                    .as_ref()
                    .map(|r| r as &dyn dagq::application::MainRemote),
            )?
        }
        Command::Review { id } => dagq::runtime::review(&db, id)?,
        Command::Stats {
            since,
            goal_id,
            full,
        } => dagq::runtime::stats(
            &db,
            &dagq::domain::stats::StatsQuery {
                since,
                goal_id,
                full,
            },
        )?,
        Command::Observe {
            since,
            dry_run,
            daily,
            timeout,
            claude,
        } => {
            use dagq::infrastructure::adapters::{ClaudeCode, executable};
            use dagq::observer::{ObserveMode, ObserveOptions};
            // A dry run starts nothing, so it needs no Claude Code.
            let executable = if dry_run {
                claude
            } else {
                executable(&claude)?
            };
            dagq::observer::observe(
                &db,
                &ClaudeCode { executable },
                &ObserveOptions {
                    mode: if daily {
                        ObserveMode::Daily
                    } else {
                        ObserveMode::Hourly
                    },
                    since,
                    dry_run,
                    timeout: Duration::from_secs(timeout),
                    dagq: env::current_exe()?,
                },
            )?
        }
        Command::Doctor { full } => dagq::runtime::doctor(&db, full)?,
        Command::Recover { run } => dagq::runtime::recover(&db, &run)?,
        Command::Session {
            run,
            lease,
            claude,
            resume,
        } => dagq::runtime::session(&db, &run, &lease, &claude, resume)?,
    })
}

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
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
