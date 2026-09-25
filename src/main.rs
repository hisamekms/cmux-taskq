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
    application::{StatusFilter, TaskQuery, TaskStore, claim_candidates, dependency_graph},
    domain::{
        AskId, AskKind, EventId, GoalEdit, GoalId, GoalVerdict, NewAsk, NewGoal, NewNote, NewTask,
        NoteQuery, NoteTarget, PlannerOrigin, PlannerOwner, ProposalId, RunId, SessionRole,
        Submission, TaskAction, TaskEdit, TaskId, TaskStatus,
        search::{self, SearchQuery},
    },
    infrastructure::{adapters::path_text, location::QueueLocation, sqlite::SqliteQueue},
};

#[derive(Parser)]
#[command(
    version = dagq::VERSION,
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
    /// Initialize the queue, creating its directory if needed. An existing queue is checked, never migrated.
    Init,
    /// Apply the migrations this binary knows and the queue lacks; opening a queue never does (ADR-0045).
    /// A breaking migration is refused while a supervisor, run or wrapper uses the queue, and the
    /// database is copied to `backups/` first.
    Migrate {
        /// Report the schema and what would be applied, without changing anything.
        #[arg(long)]
        check: bool,
    },
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
        /// Goal the task waits for until it is closed as achieved; repeatable. Never the
        /// task's own goal.
        #[arg(long = "depends-on-goal")]
        goal_dependencies: Vec<i64>,
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
        /// Glob of the paths the task may change, from the repository root; repeatable.
        /// `*` and `?` stay inside one segment, a `**` segment spans any depth. Validation
        /// parks a run changing anything else as needs_session (scope_violation) and
        /// integrate refuses to land it. Omitted: no limit.
        #[arg(long = "paths")]
        paths: Vec<String>,
        /// How urgently the supervisor should claim it: interrupt (ahead of every other ready
        /// task), urgent (a defect stopping the operation), high (groundwork other work needs
        /// soon), normal, or low (later). A ready task it waits for inherits it.
        #[arg(long, default_value = "normal", value_parser = PRIORITIES)]
        priority: String,
    },
    /// List one page of tasks, newest first: unfinished ones unless --status or --all says otherwise.
    /// Prints {"tasks", "next", "total"}; pass `next` to --before for the following page (null: none).
    List {
        /// Only these statuses (comma-separated, any of them): draft, submitted, ready, in_progress,
        /// completed, canceled.
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
    /// Make a task ready (dependencies may still block execution). Plan review readies submitted
    /// tasks; by hand a draft or submitted task needs --bypass-review. Without it only an
    /// in-progress task whose runs all failed or were interrupted returns to ready (a retry).
    Ready {
        id: i64,
        /// Skip plan review (recorded as a review_bypassed event).
        #[arg(long)]
        bypass_review: bool,
    },
    /// Return a ready or submitted task to draft.
    Draft { id: i64 },
    /// Submit draft tasks for plan review as one proposal owned by this planner session: TASKs,
    /// and with --goal a goal and its draft tasks. The tasks become submitted, which no claim
    /// takes; plan review makes them ready. Prints the proposal.
    #[command(group = clap::ArgGroup::new("members").multiple(true).required(true))]
    Submit {
        /// Draft task to submit; repeatable.
        #[arg(group = "members")]
        tasks: Vec<i64>,
        /// Open or draft goal to submit with its draft tasks; repeatable.
        #[arg(long = "goal", group = "members")]
        goals: Vec<i64>,
        /// Submit this proposal again after plan review sent it back, with the drafts it holds.
        #[arg(long, group = "members")]
        proposal: Option<i64>,
    },
    /// Check the fixed rules of a plan (plan review's mechanical checks): dependency cycles,
    /// dependencies on completed, canceled or unsubmitted draft tasks and on abandoned goals, no
    /// verification (with or without declared paths), invalid path globs, blank acceptance and
    /// titles repeated within the set. Lints TASKs and the members of each --proposal. Prints
    /// {"tasks", "violations"}, each violation {"code", "task_id", "reason"}; none: an empty list.
    #[command(group = clap::ArgGroup::new("targets").multiple(true).required(true))]
    Lint {
        /// Task to check; repeatable.
        #[arg(group = "targets")]
        tasks: Vec<i64>,
        /// Proposal whose tasks to check; repeatable.
        #[arg(long = "proposal", group = "targets")]
        proposals: Vec<i64>,
    },
    /// Read proposals: the goals and tasks submitted together for plan review.
    Proposal {
        #[command(subcommand)]
        command: ProposalCommand,
    },
    /// Cancel a draft, submitted or ready task. Does not satisfy its dependents.
    Cancel {
        id: i64,
        /// Record the task as a duplicate of this one (ADR-0046): another task that exists and is
        /// not canceled; a completed one means it is already implemented there. `show`, `list`
        /// and `stats` report it.
        #[arg(long = "duplicate-of")]
        duplicate_of: Option<i64>,
    },
    /// Manage prerequisites; TASK depends on PREDECESSOR, or with --goal on a goal
    /// that must be closed as achieved first.
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
    /// Replace the paths a draft or ready task may change (`add --paths`), or remove the limit with --none.
    SetPaths {
        /// Draft or ready task.
        task: i64,
        /// Glob of a path the task may change; repeatable. Replaces every glob it had.
        #[arg(
            long = "paths",
            required_unless_present = "none",
            conflicts_with = "none"
        )]
        paths: Vec<String>,
        /// Declare no paths: runs may change anything.
        #[arg(long)]
        none: bool,
    },
    /// Replace fields of a draft or submitted task; each given field replaces the old value, and a
    /// repeatable flag replaces the whole list. Prints the task; `show` lists the change as
    /// a `task_edited` event with the old and new values. Other statuses are refused: a
    /// ready task goes back to draft (`draft ID`) first, and a running run keeps its prompt.
    #[command(group = clap::ArgGroup::new("field").multiple(true).required(true))]
    Edit {
        /// Draft or submitted task.
        task: i64,
        #[arg(long, group = "field")]
        title: Option<String>,
        #[arg(long, group = "field")]
        description: Option<String>,
        #[arg(long, group = "field")]
        acceptance: Option<String>,
        /// Why the task exists and what to read first.
        #[arg(long, group = "field")]
        context: Option<String>,
        /// Verification command; repeatable. Replaces every command the task had.
        #[arg(long = "verify", group = "field", conflicts_with = "no_verify")]
        verification_commands: Vec<String>,
        /// Remove every verification command.
        #[arg(long, group = "field")]
        no_verify: bool,
        /// Required receipt check (`add --evidence`); repeatable. Replaces every check.
        #[arg(
            long = "evidence",
            group = "field",
            conflicts_with = "no_evidence",
            value_parser = ["tests", "e2e", "subagent_review"]
        )]
        required_evidence: Vec<String>,
        /// Require no receipt check.
        #[arg(long, group = "field")]
        no_evidence: bool,
        /// Glob of a path the task may change (`add --paths`); repeatable. Replaces every glob.
        #[arg(long = "paths", group = "field", conflicts_with = "no_paths")]
        paths: Vec<String>,
        /// Declare no paths: runs may change anything.
        #[arg(long, group = "field")]
        no_paths: bool,
    },
    /// Give a draft or ready task another priority (`add --priority`); it takes effect at the
    /// next claim and never stops a running run.
    SetPriority {
        /// Draft or ready task.
        task: i64,
        /// interrupt, urgent, high, normal or low.
        #[arg(value_parser = PRIORITIES)]
        level: String,
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
    /// Full-text search of tasks (title, description, acceptance, context), goals (title,
    /// description, acceptance, constraints), notes and the messages of landed commits, in every
    /// status (ADR-0046). QUERY is words (all must match; `"..."` for a phrase) with FTS5's AND,
    /// OR, NOT and parentheses; any substring of 3 or more characters matches, including in
    /// Japanese, and shorter terms must all be present. Prints {"hits", "total"}, best first: per
    /// hit its kind, id (a task, goal or note event ID, or a commit SHA), status, title and the
    /// matching field with an excerpt marking the match with « ».
    Search {
        query: String,
        /// Only these statuses (comma-separated): a task's (draft, submitted, ready, in_progress,
        /// completed, canceled) or a goal's (draft, open, achieved, abandoned); a note or commit
        /// has the status of its task or goal.
        #[arg(long, value_delimiter = ',')]
        status: Vec<String>,
        /// Only these kinds (comma-separated): task, goal, note, commit.
        #[arg(long = "kind", value_delimiter = ',', value_parser = ["task", "goal", "note", "commit"])]
        kinds: Vec<String>,
        /// Only this goal, its tasks and their notes and commits.
        #[arg(long = "goal")]
        goal_id: Option<i64>,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
        /// Include every searched field in full and the bm25 score.
        #[arg(long)]
        full: bool,
    },
    /// List ready tasks whose prerequisites are all completed, whose goal dependencies are all
    /// closed as achieved and whose goal is not a draft, in claim order (highest
    /// `effective_priority`, then most `unblocks`, then lowest ID); does not claim.
    Candidates,
    /// Show the unfinished tasks' dependencies: per task its direct predecessors (`depends_on`),
    /// its goal dependencies (`goal_dependencies`), what it still waits for (`ready_after`: unfinished
    /// predecessors, then `{"goal": ID}` for goals not closed as achieved), the tasks it blocks
    /// directly (including those waiting for its open goal) and how many it releases transitively
    /// (`unblocks`), its `priority` and the `effective_priority` it inherits from the ready tasks
    /// waiting for it; `candidates` in claim order and the `critical` chain.
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
        /// Write this start's JSON Lines log (supervise-<UTC time>-<pid>.jsonl)
        /// into this directory (created if missing) instead of the queue's
        /// logs/; messages also go to stderr.
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
        /// Maximum number of planners the runtime opens at once for proposals plan review sent
        /// back (apart from --parallel; planners a person opened do not count).
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
        runtime_planners: u16,
        /// Seconds a planner may take to submit a proposal sent back to it before the inbox is
        /// told.
        #[arg(long, default_value_t = 3600)]
        planner_timeout: u64,
        /// Claude Code plugin directory the planners the runtime opens load.
        #[arg(long)]
        plugin_dir: Option<PathBuf>,
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
    /// Start the queue's runtime: a launchd-resident supervisor and the inbox's cmux workspace. Idempotent; replaces a live supervisor of another version. Opens no planner (`plan` does) and forgets the resident planner's record.
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
        /// Claude Code plugin directory the inbox session loads (`claude --plugin-dir`).
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
    /// Open a new planner session in a cmux workspace `[<repo>]planner#<id>`, next to any planner
    /// already open; every call opens another. Prints the planner, its workspace and directory.
    Plan {
        /// Claude Code plugin directory the planner session loads (`claude --plugin-dir`).
        #[arg(long)]
        plugin_dir: Option<PathBuf>,
        /// Checkout the planner works in; defaults to the working directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// cmux executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
        /// Claude Code executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
    },
    /// List the planner sessions not closed, each with its state (opening, working, idle, exited,
    /// lost, closed), whether it is alive and since when it is idle. Reads only.
    Planners {
        /// Include closed planners.
        #[arg(long)]
        all: bool,
        /// cmux executable, used to look for each planner's workspace.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
    },
    /// Stop the queue's supervisor: unload its launchd agent so it drains and is not restarted, or signal and close the workspace of an in-cmux one. Leaves the inbox and planner workspaces open.
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
    /// List supervisors, unfinished runs, what waits for a person (attention), the open asks and the event cursor, without changing anything.
    Status {
        /// Only the attention addressed to this role: inbox gets all of it, planner none.
        #[arg(long, value_parser = ROLES)]
        role: Option<String>,
    },
    /// Register a question for a person about a task or one of its runs; prints the ask.
    /// An open ask of the same task, run and kind is returned instead (`created: false`).
    /// A new ask sends one `cmux notify` to the inbox workspace (`notified`, or `notify_error`).
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
        /// cmux executable, used to notify the inbox; a bare name is resolved on PATH.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
    },
    /// Write the answer of an open ask; the inbox then sees ask_answered unless the runtime applies it.
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
        /// Only the ones this role acts on: inbox answers open asks and reads the answers, planner none.
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
        /// Wake only for the attention addressed to this role: inbox for all of it and the
        /// supervisors' health, planner never.
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
        /// cmux executable, used to list the workspaces for `workspace_mismatch`; a bare name is
        /// resolved on PATH.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
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
    #[command(hide = true)]
    PlannerSession {
        #[arg(long)]
        planner: i64,
        #[arg(long)]
        claude: PathBuf,
        #[arg(long)]
        plugin_dir: Option<PathBuf>,
    },
}

/// The session roles attention is addressed to.
const ROLES: [&str; 2] = ["inbox", "planner"];
/// The names of the task priorities (ADR-0040 decision 4), highest first.
const PRIORITIES: [&str; 5] = ["interrupt", "urgent", "high", "normal", "low"];

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
    Add {
        task: i64,
        #[arg(required_unless_present = "goal")]
        predecessor: Option<i64>,
        /// A goal instead of a predecessor task: TASK waits until it is closed as achieved.
        #[arg(long, conflicts_with = "predecessor")]
        goal: Option<i64>,
    },
    Remove {
        task: i64,
        #[arg(required_unless_present = "goal")]
        predecessor: Option<i64>,
        /// Remove the dependency on this goal instead of a predecessor task.
        #[arg(long, conflicts_with = "predecessor")]
        goal: Option<i64>,
    },
}

#[derive(Subcommand)]
enum ProposalCommand {
    /// The submitted and revising proposals, oldest submission first (plan review's order).
    List {
        /// Include accepted and canceled proposals.
        #[arg(long)]
        all: bool,
    },
    /// Show a proposal with its member task and goal IDs.
    Show { id: i64 },
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
/// The error of a command the headless reviewer may not run.
const REVIEWER_DENIED: &str = "reviewer may not change queue state";

/// The commands that only read the queue. They open it on a read-only
/// connection (ADR-0045 decision 18), and the supervisor's headless review
/// may run them and nothing else (ADR-0027).
fn reads_only(command: &Command) -> bool {
    matches!(
        command,
        Command::Locate
            | Command::List { .. }
            | Command::Show { .. }
            | Command::Candidates
            | Command::Graph { .. }
            | Command::Status { .. }
            | Command::Asks { .. }
            | Command::Events { .. }
            | Command::Stats { .. }
            | Command::Doctor { .. }
            | Command::Notes { .. }
            | Command::Search { .. }
            | Command::Proposal { .. }
            | Command::Planners { .. }
            | Command::Lint { .. }
            | Command::Goal {
                command: GoalCommand::List | GoalCommand::Show { .. },
            }
    )
}

/// What the supervisor's headless review may run (ADR-0027): reads only.
fn reviewer_access(command: &Command) -> ObserverAccess {
    if reads_only(command) {
        ObserverAccess::Allowed
    } else {
        ObserverAccess::Denied
    }
}

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
        | Command::Search { .. }
        | Command::Proposal { .. }
        | Command::Planners { .. }
        | Command::Lint { .. }
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
    let role = env::var(dagq::application::lifecycle::ROLE_ENV)
        .ok()
        .filter(|role| !role.is_empty());
    let observer = role.as_deref() == Some(dagq::application::lifecycle::OBSERVER_ROLE);
    let access = if observer {
        observer_access(&cli.command)
    } else {
        ObserverAccess::Allowed
    };
    if access == ObserverAccess::Denied {
        bail!(OBSERVER_DENIED);
    }
    if role.as_deref() == Some(dagq::application::lifecycle::REVIEWER_ROLE)
        && reviewer_access(&cli.command) == ObserverAccess::Denied
    {
        bail!(REVIEWER_DENIED);
    }
    let cwd = env::current_dir().context("working directory is unavailable")?;
    let location = QueueLocation::resolve(cli.db.as_deref(), &cwd)?;
    let db = location.db.clone();
    install_telemetry(&cli.command, &location);
    // The clock and IDs of every queue and use case this command runs.
    let generators = dagq::infrastructure::clock::system();
    let one_shot = dagq::compose::OneShot::new(generators.clone());
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
        let mut queue = SqliteQueue::init(&db)?.with_generators(generators);
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
    if let Command::Migrate { check } = cli.command {
        if check {
            return Ok(serde_json::to_value(SqliteQueue::schema(&db)?)?);
        }
        let report = SqliteQueue::migrate(
            &db,
            Some(&dagq::infrastructure::adapters::process_alive),
            generators.clock.now(),
        )?;
        let mut value = serde_json::to_value(report)?;
        // A landing recorded without its message (ADR-0046 decision 3).
        if let Ok(mut queue) = SqliteQueue::open(&db) {
            let fallback = common_dir.clone();
            value["commit_messages_filled"] =
                json!(queue.fill_commit_messages(|dir, commit| {
                    let dir = dir.map(str::to_owned).or_else(|| fallback.clone())?;
                    dagq::infrastructure::adapters::commit_message(
                        std::path::Path::new(&dir),
                        commit,
                    )
                })?);
        }
        value["db"] = json!(db);
        return Ok(value);
    }
    // A repository queue already resolved the working directory; `--repo`
    // overrides it for a `--db` queue used from elsewhere or a moved checkout.
    let checkout = |repo: Option<PathBuf>| repo.unwrap_or_else(|| cwd.clone());
    // `rebind` is the one command that runs on a queue bound elsewhere.
    if let Command::Rebind { repo } = cli.command {
        return one_shot.rebind(&db, &checkout(repo));
    }
    let mut queue = if reads_only(&cli.command) {
        SqliteQueue::open_read_only(&db)?
    } else {
        SqliteQueue::open(&db)?
    }
    .with_generators(generators.clone());
    if let Some(common_dir) = &common_dir {
        queue.assert_repository(common_dir)?;
    }
    if let ObserverAccess::DraftGoal(goal_id) = access
        && !queue.show_goal(GoalId::new(goal_id))?.goal.is_draft()
    {
        bail!(OBSERVER_DENIED);
    }
    Ok(match cli.command {
        Command::Init | Command::Locate | Command::Rebind { .. } | Command::Migrate { .. } => {
            unreachable!()
        }
        Command::Add {
            title,
            description,
            acceptance,
            verification_commands,
            dependencies,
            goal_dependencies,
            goal_id,
            context,
            required_evidence,
            paths,
            priority,
        } => serde_json::to_value(
            queue.add(NewTask {
                title,
                description,
                acceptance,
                verification_commands,
                dependencies: dependencies.into_iter().map(TaskId::new).collect(),
                goal_dependencies: goal_dependencies.into_iter().map(GoalId::new).collect(),
                goal_id: goal_id.map(GoalId::new),
                context,
                required_evidence: required_evidence
                    .iter()
                    .map(|name| name.parse())
                    .collect::<Result<_, _>>()?,
                paths,
                priority: priority.parse()?,
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
                goal_id: goal_id.map(GoalId::new),
                limit: usize::try_from(limit)?,
                before: before.map(TaskId::new),
                full,
            })?)?
        }
        Command::Show { id, full, events } => {
            let detail = queue.show(TaskId::new(id))?;
            if full {
                serde_json::to_value(detail)?
            } else {
                dagq::view::task_detail(&detail, events)
            }
        }
        Command::Ready { id, bypass_review } => {
            let action = if bypass_review {
                TaskAction::BypassReview
            } else {
                TaskAction::Ready
            };
            serde_json::to_value(queue.transition(TaskId::new(id), action)?)?
        }
        Command::Submit {
            tasks,
            goals,
            proposal,
        } => {
            use dagq::application::lifecycle::{CMUX_WORKSPACE_ENV, PLANNER_ORIGIN_ENV};
            let origin = match env::var(PLANNER_ORIGIN_ENV) {
                Ok(origin) if !origin.is_empty() => origin.parse()?,
                _ => PlannerOrigin::Person,
            };
            serde_json::to_value(
                queue.submit(Submission {
                    tasks: tasks.into_iter().map(TaskId::new).collect(),
                    goals: goals.into_iter().map(GoalId::new).collect(),
                    proposal: proposal.map(ProposalId::new),
                    owner: PlannerOwner {
                        origin,
                        workspace_id: env::var(CMUX_WORKSPACE_ENV)
                            .ok()
                            .filter(|id| !id.trim().is_empty()),
                    },
                })?,
            )?
        }
        Command::Lint { tasks, proposals } => {
            let mut targets: Vec<TaskId> = tasks.into_iter().map(TaskId::new).collect();
            for id in proposals {
                targets.extend_from_slice(queue.show_proposal(ProposalId::new(id))?.task_ids());
            }
            let mut seen = std::collections::HashSet::new();
            targets.retain(|id| seen.insert(*id));
            let input = queue.lint_input(&targets)?;
            json!({"tasks": targets, "violations": dagq::domain::lint::lint(&input)})
        }
        Command::Proposal { command } => match command {
            ProposalCommand::List { all } => json!({"proposals": queue.proposals(all)?}),
            ProposalCommand::Show { id } => {
                serde_json::to_value(queue.show_proposal(ProposalId::new(id))?)?
            }
        },
        Command::Draft { id } => {
            serde_json::to_value(queue.transition(TaskId::new(id), TaskAction::Draft)?)?
        }
        Command::Cancel { id, duplicate_of } => serde_json::to_value(match duplicate_of {
            Some(target) => queue.cancel_duplicate(TaskId::new(id), TaskId::new(target))?,
            None => queue.transition(TaskId::new(id), TaskAction::Cancel)?,
        })?,
        Command::Dependency { command } => {
            let id =
                match command {
                    DependencyCommand::Add {
                        task,
                        predecessor,
                        goal,
                    } => {
                        match (predecessor, goal) {
                            (_, Some(goal)) => {
                                queue.add_goal_dependency(TaskId::new(task), GoalId::new(goal))?
                            }
                            (Some(predecessor), None) => {
                                queue.add_dependency(TaskId::new(task), TaskId::new(predecessor))?
                            }
                            (None, None) => unreachable!("clap requires a predecessor or --goal"),
                        }
                        task
                    }
                    DependencyCommand::Remove {
                        task,
                        predecessor,
                        goal,
                    } => {
                        match (predecessor, goal) {
                            (_, Some(goal)) => queue
                                .remove_goal_dependency(TaskId::new(task), GoalId::new(goal))?,
                            (Some(predecessor), None) => queue
                                .remove_dependency(TaskId::new(task), TaskId::new(predecessor))?,
                            (None, None) => unreachable!("clap requires a predecessor or --goal"),
                        }
                        task
                    }
                };
            serde_json::to_value(queue.show(TaskId::new(id))?)?
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
            GoalCommand::Ready { id } => serde_json::to_value(queue.ready_goal(GoalId::new(id))?)?,
            GoalCommand::List => serde_json::to_value(queue.list_goals()?)?,
            GoalCommand::Show { id, full } => {
                let detail = queue.show_goal(GoalId::new(id))?;
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
                GoalId::new(id),
                GoalEdit {
                    title,
                    description,
                    acceptance,
                    constraints,
                    doc,
                },
            )?)?,
            GoalCommand::Close { id, verdict } => serde_json::to_value(
                queue.close_goal(GoalId::new(id), verdict.parse::<GoalVerdict>()?)?,
            )?,
        },
        Command::SetGoal {
            task,
            goal,
            none: _,
        } => serde_json::to_value(queue.set_goal(TaskId::new(task), goal.map(GoalId::new))?)?,
        Command::SetPaths {
            task,
            paths,
            none: _,
        } => serde_json::to_value(queue.set_paths(TaskId::new(task), paths)?)?,
        Command::Edit {
            task,
            title,
            description,
            acceptance,
            context,
            verification_commands,
            no_verify,
            required_evidence,
            no_evidence,
            paths,
            no_paths,
        } => {
            // A list flag replaces the list; its --no- flag empties it.
            let replaced =
                |values: Vec<String>, none: bool| (none || !values.is_empty()).then_some(values);
            let required_evidence = replaced(required_evidence, no_evidence)
                .map(|names| names.iter().map(|name| name.parse()).collect())
                .transpose()?;
            serde_json::to_value(queue.edit_task(
                TaskId::new(task),
                TaskEdit {
                    title,
                    description,
                    acceptance,
                    verification_commands: replaced(verification_commands, no_verify),
                    required_evidence,
                    paths: replaced(paths, no_paths),
                    context,
                },
            )?)?
        }
        Command::SetPriority { task, level } => {
            serde_json::to_value(queue.set_priority(TaskId::new(task), level.parse()?)?)?
        }
        Command::Note {
            task,
            run,
            goal,
            text,
            kind,
        } => {
            let target = match (task, run, goal) {
                (Some(task), _, _) => NoteTarget::Task(TaskId::new(task)),
                (_, Some(run), _) => NoteTarget::Run(RunId::new(run)?),
                (_, _, goal) => NoteTarget::Goal(GoalId::new(goal.context("note needs a target")?)),
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
            goal_id: goal_id.map(GoalId::new),
            task_id: task_id.map(TaskId::new),
            since: since.map(EventId::new),
            limit: usize::try_from(limit)?,
        })?)?,
        Command::Search {
            query,
            status,
            kinds,
            goal_id,
            limit,
            full,
        } => serde_json::to_value(
            queue.search(&SearchQuery {
                terms: query,
                kinds: kinds
                    .iter()
                    .map(|kind| kind.parse())
                    .collect::<Result<_, _>>()?,
                statuses: status
                    .iter()
                    .map(|value| search::parse_status(value))
                    .collect::<Result<_, _>>()?,
                goal_id: goal_id.map(GoalId::new),
                limit: usize::try_from(limit)?,
                full,
            })?,
        )?,
        Command::Candidates => {
            let graph = dependency_graph(queue.graph_input()?, None);
            serde_json::to_value(claim_candidates(queue.candidates()?, &graph))?
        }
        Command::Graph { goal_id } => serde_json::to_value(dependency_graph(
            queue.graph_input()?,
            goal_id.map(GoalId::new),
        ))?,
        Command::Status { role: r } => one_shot.status_for(&db, parse_role(r)?)?,
        Command::Ask {
            command: Some(AskCommand::Close { id }),
            ..
        } => serde_json::to_value(queue.close_ask(AskId::new(id))?)?,
        Command::Ask {
            command: None,
            kind,
            question,
            options,
            task_id,
            run,
            cmux,
        } => {
            use dagq::infrastructure::adapters::{Cmux, executable};
            // A missing cmux fails only the notification, not the ask.
            dagq::compose::ask(
                &db,
                &cwd,
                NewAsk {
                    kind: kind.unwrap_or_default().parse::<AskKind>()?,
                    task_id: task_id.map(TaskId::new),
                    run_id: run.map(RunId::new).transpose()?,
                    question: question.unwrap_or_default(),
                    options,
                    // The session's role; a person at a plain terminal has none.
                    asked_by: role.unwrap_or_else(|| "human".into()),
                },
                &Cmux {
                    executable: executable(&cmux).unwrap_or(cmux),
                },
            )?
        }
        Command::Answer { id, text } => serde_json::to_value(queue.answer(AskId::new(id), &text)?)?,
        Command::Asks { open, role: r, all } => {
            json!({"asks": queue.asks(dagq::application::AskQuery {
            all,
            open,
            role: parse_role(r)?,
        })?})
        }
        Command::Events { after, limit, all } => {
            dagq::watch::events(&db, EventId::new(after), limit as usize, all)?
        }
        Command::Watch {
            after,
            timeout,
            interval,
            role: r,
        } => dagq::watch::watch(
            &db,
            &dagq::watch::WatchOptions {
                after: after.map(EventId::new),
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
            log_dir: _,
            observe_interval,
            observe_daily,
            runtime_planners,
            planner_timeout,
            plugin_dir,
        } => {
            use dagq::compose::SuperviseOptions;
            use dagq::infrastructure::adapters::{Cmux, executable};
            let options = SuperviseOptions {
                stop: install_stop_signal()?,
                // A one-shot pass observes only when asked to.
                observe_interval: Duration::from_secs(observe_interval.unwrap_or(if once {
                    0
                } else {
                    3600
                })),
                observe_daily,
                generators,
                runtime_planners: usize::from(runtime_planners),
                planner_timeout: Duration::from_secs(planner_timeout),
                plugin_dir,
                ..SuperviseOptions::new(usize::from(parallel), once)
            };
            dagq::compose::supervise(
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
            use dagq::application::lifecycle::{QUEUE_ENV, ROLE_ENV, UpEnvironment, UpOptions};
            use dagq::infrastructure::adapters::{SOCKET_PASSWORD_ENV, claude_global_config};
            use dagq::infrastructure::{
                adapters::{Cmux, SystemProcesses, executable},
                launchd::Launchctl,
            };
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
            one_shot.up(
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
        Command::Plan {
            plugin_dir,
            repo,
            cmux,
            claude,
        } => {
            use dagq::infrastructure::adapters::{Cmux, executable};
            one_shot.plan(
                &location,
                &checkout(repo),
                &Cmux {
                    executable: executable(&cmux)?,
                },
                &dagq::compose::PlanOptions {
                    claude: executable(&claude)?,
                    plugin_dir,
                    runner: env::current_exe()?,
                },
            )?
        }
        Command::Planners { all, cmux } => {
            use dagq::infrastructure::adapters::{Cmux, executable};
            one_shot.planners(
                &db,
                &Cmux {
                    executable: executable(&cmux)?,
                },
                all,
            )?
        }
        Command::Down { wait, force, cmux } => {
            use dagq::application::lifecycle::DownOptions;
            use dagq::infrastructure::{
                adapters::{Cmux, SystemProcesses, executable},
                launchd::Launchctl,
            };
            // cmux is only needed to close an in-cmux supervisor's
            // workspace, so a queue without one still goes down when cmux
            // is not installed; the unresolved name then fails only there.
            one_shot.down(
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
            use dagq::{
                application::integrate::IntegrateTarget, infrastructure::adapters::GitRepository,
            };
            let target = match (id, next) {
                (Some(id), false) => IntegrateTarget::Task(TaskId::new(id)),
                _ => IntegrateTarget::Next,
            };
            let repo = checkout(repo);
            let remote = if no_push {
                None
            } else {
                Some(GitRepository::inspect(&repo)?)
            };
            one_shot.integrate(
                &db,
                target,
                &repo,
                remote
                    .as_ref()
                    .map(|r| r as &dyn dagq::application::MainRemote),
            )?
        }
        Command::Review { id } => dagq::compose::review(&db, TaskId::new(id))?,
        Command::Stats {
            since,
            goal_id,
            full,
            cmux,
        } => {
            use dagq::infrastructure::adapters::{Cmux, executable};
            // A missing cmux leaves only `workspace_mismatch` unjudged.
            let cmux = executable(&cmux).ok().map(|executable| Cmux { executable });
            one_shot.stats(
                &db,
                &dagq::domain::stats::StatsQuery {
                    since: since.map(EventId::new),
                    goal_id: goal_id.map(GoalId::new),
                    full,
                },
                cmux.as_ref()
                    .map(|cmux| cmux as &dyn dagq::application::stats::WorkspaceListing),
            )?
        }
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
                    since: since.map(EventId::new),
                    dry_run,
                    timeout: Duration::from_secs(timeout),
                    dagq: env::current_exe()?,
                },
            )?
        }
        Command::Doctor { full } => one_shot.doctor(&db, full)?,
        Command::Recover { run } => one_shot.recover(&db, &RunId::new(run)?)?,
        Command::Session {
            run,
            lease,
            claude,
            resume,
        } => dagq::compose::session(&db, &RunId::new(run)?, &lease, &claude, resume)?,
        Command::PlannerSession {
            planner,
            claude,
            plugin_dir,
        } => dagq::compose::planner_session(
            &db,
            dagq::domain::PlannerId::new(planner),
            &claude,
            plugin_dir.as_deref(),
        )?,
    })
}

/// The subscriber of this process's progress and diagnostic events
/// (ADR-0033): the long-running and landing processes (`supervise`,
/// `integrate`, `observe` and the session wrapper) keep a JSON Lines file
/// in the queue's `logs/` (a supervisor's `--log-dir` if given); every
/// other command prints its messages on stderr only.
fn install_telemetry(command: &Command, location: &QueueLocation) {
    use dagq::infrastructure::telemetry::Telemetry;
    let file = match command {
        Command::Supervise { log_dir, .. } => Some((
            "supervise",
            log_dir.clone().unwrap_or_else(|| location.log_dir.clone()),
        )),
        Command::Integrate { .. } => Some(("integrate", location.log_dir.clone())),
        Command::Observe { .. } => Some(("observe", location.log_dir.clone())),
        Command::Session { .. } => Some(("session", location.log_dir.clone())),
        Command::PlannerSession { .. } => Some(("planner-session", location.log_dir.clone())),
        _ => None,
    };
    let telemetry = match file {
        Some((process, dir)) => Telemetry::open(&dir, process),
        None => Telemetry::stderr(),
    };
    telemetry.install();
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
            // The command's own failure, in its log file too; stderr gets
            // the error JSON below as always.
            tracing::error!(
                target: "dagq::telemetry::exit",
                error = %format_args!("{error:#}"),
                "dagq exited with an error: {error:#}"
            );
            eprintln!("{}", json!({"error": format!("{error:#}")}));
            ExitCode::FAILURE
        }
    }
}
