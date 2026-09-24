//! The observer job (ADR-0024 decision 4): a headless agent run that reads
//! `stats`, the latest notes, the open asks and the dependency graph, and
//! may write only notes, `blocked` asks and draft goals. The CLI refuses
//! everything else under `DAGQ_ROLE=observer`. The supervisor starts it on
//! a timer (`--observe-interval`, `--observe-daily`); `observe` starts it
//! by hand.
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::{
    application::{AgentProvider, TaskStore, dependency_graph},
    domain::{NoteQuery, stats::StatsQuery},
    infrastructure::{adapters::shell_join, asks::AskQuery, sqlite::SqliteQueue},
    lifecycle::{OBSERVER_ROLE, QUEUE_ENV, ROLE_ENV},
};

/// Notes the prompt carries.
pub const PROMPT_NOTES: usize = 20;
/// How long one observer run may take before it is killed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// The tools the observer may use beyond reading: the queue CLI only.
pub const ALLOWED_TOOLS: &[&str] = &["Bash(dagq:*)"];

/// The supervisor starts the observations on its timer, so their modes
/// belong to its use case.
pub use crate::application::supervise::{DAILY_WINDOW_SECS, ObserveMode};

#[derive(Debug, Clone)]
pub struct ObserveOptions {
    pub mode: ObserveMode,
    /// The cursor to read `stats` past; by default the hourly observation's
    /// saved cursor, or for the daily one the last event 24 hours ago.
    pub since: Option<i64>,
    /// Build and return the prompt without starting the agent.
    pub dry_run: bool,
    pub timeout: Duration,
    /// The `dagq` binary the agent calls; its directory goes first on PATH.
    pub dagq: PathBuf,
}

/// `<queue dir>/observer`: one directory per observation and the cursor.
pub fn observer_dir(db: &Path) -> PathBuf {
    db.parent().unwrap_or(Path::new(".")).join("observer")
}

/// The hourly observation's cursor, if one was saved.
pub fn read_cursor(db: &Path) -> Result<Option<i64>> {
    let path = observer_dir(db).join("cursor");
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text.trim().parse().with_context(|| {
            format!("parse the observer cursor in {}", path.display())
        })?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn write_cursor(db: &Path, cursor: i64) -> Result<()> {
    let dir = observer_dir(db);
    fs::create_dir_all(&dir)?;
    let temporary = dir.join(format!(".cursor.{}.tmp", std::process::id()));
    fs::write(&temporary, format!("{cursor}\n"))?;
    fs::rename(&temporary, dir.join("cursor"))?;
    Ok(())
}

/// Run one observation: gather the inputs, start the agent headless with
/// `DAGQ_ROLE=observer`, wait for it, then record `observe_finished` with
/// what it wrote and, for a succeeded hourly one, save the new cursor.
pub fn observe(db: &Path, provider: &dyn AgentProvider, options: &ObserveOptions) -> Result<Value> {
    let db = db
        .canonicalize()
        .context("queue must already be initialized")?;
    let queue = SqliteQueue::open(&db)?;
    let started = queue.generators().clock.now();
    let since = match (options.since, options.mode) {
        (Some(since), _) => Some(since),
        (None, ObserveMode::Hourly) => read_cursor(&db)?,
        (None, ObserveMode::Daily) => Some(queue.event_id_before(started - DAILY_WINDOW_SECS)?),
    };
    let stats = crate::runtime::stats(
        &db,
        &StatsQuery {
            since,
            ..StatsQuery::default()
        },
    )?;
    let cursor = stats["next_cursor"].as_i64().unwrap_or_default();
    let notes = queue.notes(&NoteQuery {
        goal_id: None,
        task_id: None,
        since: None,
        limit: PROMPT_NOTES,
    })?;
    let asks = queue.asks(AskQuery {
        open: true,
        ..AskQuery::default()
    })?;
    let graph = dependency_graph(queue.graph_input()?, None);
    let input = json!({
        "stats": stats,
        "notes": notes.notes,
        "open_asks": asks,
        "graph": {"candidates": graph.candidates, "critical": graph.critical},
    });
    let command = shell_join(&[
        "dagq".into(),
        "--db".into(),
        db.to_string_lossy().into_owned(),
    ]);
    let prompt = observer_prompt(options.mode, &command, since, &input)?;
    if options.dry_run {
        return Ok(json!({
            "dry_run": true,
            "mode": options.mode.as_str(),
            "since": since,
            "cursor": cursor,
            "prompt": prompt,
        }));
    }
    let dir = observation_dir(&db, started)?;
    fs::write(dir.join("prompt.md"), &prompt)?;
    fs::write(
        dir.join("input.json"),
        serde_json::to_string_pretty(&input)?,
    )?;
    let (ask_mark, goal_mark) = queue.ask_and_goal_high_water()?;
    let event_mark = queue.record_queue_event(
        "observe_started",
        json!({"mode": options.mode.as_str(), "since": since, "dir": dir}),
    )?;
    tracing::info!(
        mode = options.mode.as_str(),
        since,
        "observer ({}) started",
        options.mode.as_str()
    );
    let clock = Instant::now();
    // `failed`: the agent exited non-zero or by a signal; `error`: it could
    // not start or ran past the timeout.
    let (outcome, exit_code, error) = match run_agent(provider, &db, &dir, &prompt, options) {
        Ok(Some(0)) => ("succeeded", Some(0), None),
        Ok(code) => ("failed", code, None),
        Err(error) => ("error", None, Some(format!("{error:#}"))),
    };
    let (notes, asks, goals) = queue.written_by(OBSERVER_ROLE, event_mark, ask_mark, goal_mark)?;
    let saved = outcome == "succeeded" && options.mode == ObserveMode::Hourly;
    if saved {
        write_cursor(&db, cursor)?;
    }
    let payload = json!({
        "mode": options.mode.as_str(),
        "outcome": outcome,
        "exit_code": exit_code,
        "error": error,
        "since": since,
        "cursor": cursor,
        "cursor_saved": saved,
        "notes": notes,
        "asks": asks,
        "goals": goals,
        "duration_secs": clock.elapsed().as_secs(),
        "dir": dir,
    });
    queue.record_queue_event("observe_finished", payload.clone())?;
    tracing::info!(
        mode = options.mode.as_str(),
        outcome,
        exit_code,
        error,
        notes,
        asks,
        goals,
        "observer ({}) finished: {outcome}",
        options.mode.as_str()
    );
    Ok(payload)
}

/// `<queue dir>/observer/<started_at>/`, suffixed when one already exists
/// for that second.
fn observation_dir(db: &Path, started: i64) -> Result<PathBuf> {
    let root = observer_dir(db);
    fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    for n in 0.. {
        let name = if n == 0 {
            started.to_string()
        } else {
            format!("{started}-{n}")
        };
        let dir = root.join(name);
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(|| format!("create {}", dir.display())),
        }
    }
    unreachable!("the suffixes do not run out")
}

/// Start the agent in `dir` with its output in `output.log`, and wait for
/// it up to the timeout (then kill it: an error). The exit code, or `None`
/// when a signal ended it.
fn run_agent(
    provider: &dyn AgentProvider,
    db: &Path,
    dir: &Path,
    prompt: &str,
    options: &ObserveOptions,
) -> Result<Option<i32>> {
    let log = fs::File::create(dir.join("output.log"))?;
    let mut command = crate::infrastructure::process::command(&provider.headless_command(
        dir,
        prompt,
        ALLOWED_TOOLS,
    )?);
    let mut path = std::env::var_os("PATH").unwrap_or_default();
    if let Some(bin) = options.dagq.parent() {
        let mut paths = vec![bin.to_path_buf()];
        paths.extend(std::env::split_paths(&path));
        path = std::env::join_paths(paths)?;
    }
    command
        .env(ROLE_ENV, OBSERVER_ROLE)
        .env(QUEUE_ENV, db)
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let mut child = command.spawn().context("start the observer agent")?;
    let deadline = Instant::now() + options.timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "the observer did not finish within {}s",
                options.timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// The observer's instructions and inputs.
pub fn observer_prompt(
    mode: ObserveMode,
    dagq: &str,
    since: Option<i64>,
    input: &Value,
) -> Result<String> {
    let window = match (mode, since) {
        (ObserveMode::Daily, _) => {
            "This is the daily observation: the stats cover the runs that finished in the last 24 hours. \
             Look for trends rather than single incidents: failures, resumes or waits that recur across tasks and goals, \
             alerts that keep coming back in the notes, and whether earlier notes' problems went away."
                .to_owned()
        }
        (ObserveMode::Hourly, Some(since)) => format!(
            "This is the hourly observation: the stats cover the runs that finished after event {since}."
        ),
        (ObserveMode::Hourly, None) => {
            "This is the first hourly observation: the stats cover the latest finished runs.".to_owned()
        }
    };
    Ok(format!(
        "You are the observer of the dagq queue (ADR-0024 decision 4), started headless by the supervisor.\n\
         Your job is to observe whether dagq is running well, not to fix it.\n\
         {window}\n\
         \n\
         Do:\n\
         - Record what is not going well as a note on the task, run or goal it concerns: `{dagq} note --task ID|--run ID|--goal ID --kind <slug> --text '...'` (a lowercase slug such as stall, failure, wait, capacity).\n\
         - Raise each alert of the stats to the inbox as a blocked ask: `{dagq} ask --kind blocked --question '...' --option '...' [--task ID | --run ID]`, with your reading of it and the next moves a person can choose as options (leave it, act from the inbox or planner, register a goal). \
           An alert with no task (idle_slots, backend_failures) is an ask without --task and --run; put every such alert in its question. \
           Raise the same alert only once: do not ask when an open ask below already covers it or a note shows you raised it before.\n\
         - For a problem that recurs, register an improvement as a draft goal: `{dagq} goal add --draft 'title' --description '...' --acceptance '...'`, and cite the ids of the observations (note event ids) that are its evidence in the description. \
           You may add draft tasks to that draft goal with `{dagq} add --goal ID ...`. A person adopts or rejects it in the planner.\n\
         - Read more when needed: `{dagq} stats`, `{dagq} notes`, `{dagq} show ID`, `{dagq} events --all`, `{dagq} asks`, `{dagq} graph`, `{dagq} goal show ID`.\n\
         \n\
         Do not:\n\
         - Resolve individual stalls, answer asks, or change the state of runs, tasks or goals (ready, cancel, integrate, recover, goal ready/close); the queue refuses those from your environment.\n\
         - Edit files or run anything but the queue commands above.\n\
         \n\
         When you are done, print one line saying how many notes, asks and draft goals you wrote.\n\
         \n\
         Inputs (JSON: stats, the latest {PROMPT_NOTES} notes, the open asks, and the graph's candidates and critical chain):\n\
         ```json\n{}\n```\n",
        serde_json::to_string_pretty(input)?
    ))
}
