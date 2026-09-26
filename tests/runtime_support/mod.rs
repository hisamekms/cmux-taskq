//! The fixture of the runtime tests (`tests/runtime_*.rs`): the stub agent
//! and workspace, the supervisor options and the helpers several of them use.
#![allow(dead_code, unused_imports)]

use crate::common;
pub use crate::common::Bounded;
pub use anyhow::{Result, bail, ensure};
pub use dagq::{
    VERSION,
    application::{
        AgentProvider, Clock, CommandSpec, Exit, Generators, IdGenerator, MainRemote, Spawned,
        Spawner, Streams, SupervisorEnvironment, TaskStore, WorkspaceBackend, WorkspaceTags,
        dependency_graph,
    },
    domain::{
        AskId, AskKind, CommitSha, EventId, EvidenceCheck, GoalEdit, GoalId, MAX_RESUME_ATTEMPTS,
        NewAsk, NewGoal, NewTask, Priority, ReasonCode, RunId, RunStatus, SessionRole, Task,
        TaskAction, TaskId, TaskRun, TaskStatus,
        resume::CONFLICT_ONLY_RESUME_LIMIT,
        search::{SearchKind, SearchQuery, SearchRef},
    },
    infrastructure::{
        adapters::{GitRepository, shell_join, workspace_handle},
        asks::AskQuery,
        clock::{self, SystemClock},
        location::QueueLocation,
        process,
        run_files::LocalRunFiles,
        sqlite::SqliteQueue,
        telemetry::Telemetry,
    },
    runtime::{self, IntegrateTarget, SuperviseOptions},
};
pub use rusqlite::Connection;
pub use serde_json::{Value, json};
pub use std::{
    collections::HashMap,
    fs,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, LazyLock, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
pub use tempfile::TempDir;

/// A full Git object ID as the runtime takes it.
pub fn sha(commit: &str) -> CommitSha {
    CommitSha::try_from(commit).unwrap()
}

pub fn git(repo: &Path, args: &[&str]) {
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

/// The first lines of every stub agent: a stub whose test process is gone
/// (killed, or ended while the session still ran) kills its own process
/// group, the one [`StubSpawner`] made, with every child in it.
macro_rules! watchdog {
    () => {
        r#"
( while kill -0 $$ 2>/dev/null; do kill -0 "$PPID" 2>/dev/null || kill -s KILL -- -$$; sleep 0.2; done ) &
"#
    };
}

/// A test's directory with its repository and queue. Dropping it, when the
/// test returns or panics, kills every stub agent started on its queue with
/// all their children, so none outlives the test (task 317). The test is
/// timed while it is held (task 324).
pub struct Fixture {
    pub db: PathBuf,
    pub dir: TempDir,
    pub _test: common::Waiting,
}

impl Fixture {
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        kill_stubs(&self.db);
    }
}

/// The process groups of the stub agents started per queue; a queue whose
/// fixture was dropped maps to `None` and starts no more.
pub static STUBS: LazyLock<Mutex<HashMap<PathBuf, Option<Vec<u32>>>>> =
    LazyLock::new(Default::default);

pub fn stubs() -> MutexGuard<'static, HashMap<PathBuf, Option<Vec<u32>>>> {
    STUBS.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn kill_stubs(db: &Path) {
    let groups = stubs().insert(db.into(), None).flatten();
    for group in groups.unwrap_or_default() {
        // SAFETY: kill(2) takes no pointer; a negative pid names the group.
        unsafe { libc::kill(-(group as libc::pid_t), libc::SIGKILL) };
    }
}

/// Starts the stub agents of the sessions on `db`, as `LocalSpawner` would
/// start Claude, but each in a process group of its own, which [`Fixture`]
/// kills, and with no stream of the test process: a stub left running does
/// not hold the pipe of `cargo test | grep` open.
pub struct StubSpawner {
    pub db: PathBuf,
}

impl Spawner for StubSpawner {
    fn spawn(&self, spec: &CommandSpec, streams: Streams<'_>) -> Result<Box<dyn Spawned>> {
        assert!(matches!(streams, Streams::Inherit), "the agent's terminal");
        let mut stubs = stubs();
        let Some(groups) = stubs.entry(self.db.clone()).or_insert(Some(Vec::new())) else {
            bail!("the test's fixture is gone");
        };
        let child = process::command(spec)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        groups.push(child.id());
        Ok(Box::new(Stub(child)))
    }
}

pub struct Stub(pub Child);

impl Spawned for Stub {
    fn id(&self) -> u32 {
        self.0.id()
    }
    fn try_wait(&mut self) -> Result<Option<Exit>> {
        Ok(self.0.try_wait()?.map(process::exit))
    }
    fn kill(&mut self) -> Result<()> {
        Ok(self.0.kill()?)
    }
    fn wait(&mut self) -> Result<Exit> {
        Ok(process::exit(self.0.wait()?))
    }
}

/// Whether `pid` runs: neither gone nor a zombie.
pub fn running(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .bounded_output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

pub fn fixture() -> (Fixture, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo's directory");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    fs::write(repo.join("seed.txt"), "fixture\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "seed"]);
    let db = dir.path().join("queue's data.db");
    let mut queue = SqliteQueue::init(&db).unwrap();
    add_ready_task(&mut queue, "test task", &[]);
    (
        Fixture {
            db: db.clone(),
            dir,
            _test: common::test(),
        },
        repo,
        db,
    )
}

pub fn add_ready_task(queue: &mut SqliteQueue, title: &str, dependencies: &[TaskId]) -> TaskId {
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            dependencies: dependencies.to_vec(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    task.id()
}

/// Shell prelude for the fake agent: `receipt COMMIT [RUN_ID]` writes an
/// atomically renamed receipt claiming success with evidence on every check,
/// `idle` mimics Claude's Stop hook (`idle_bg` with background work still
/// running, `idle_bg_done` once it ended, as Claude Code 2.1.281 writes
/// `background_tasks`), and `await_exit` blocks until the test
/// workspace delivers the supervisor's exit request.
pub const AGENT_PRELUDE: &str = concat!(
    watchdog!(),
    r#"
test -f seed.txt || exit 99
printf 'fixture log\n' > "$LOG"
receipt() {
  printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"passed","evidence_or_reason":"ran"},"e2e":{"status":"not_applicable","evidence_or_reason":"no e2e surface"},"subagent_review":{"status":"passed","evidence_or_reason":"reviewed"},"summary":"done"}' "${2:-$RUN_ID}" "$1" > "$RECEIPT.tmp"
  mv "$RECEIPT.tmp" "$RECEIPT"
}
idle() {
  printf '{"session_id":"%s","hook_event_name":"Stop","stop_hook_active":false}' "$RUN_ID" > "$IDLE.tmp"
  mv "$IDLE.tmp" "$IDLE"
}
idle_bg() {
  printf '{"session_id":"%s","hook_event_name":"Stop","stop_hook_active":false,"background_tasks":[{"id":"b1","type":"shell","status":"running","description":"cargo test","command":"cargo test"}]}' "$RUN_ID" > "$IDLE.tmp"
  mv "$IDLE.tmp" "$IDLE"
}
idle_bg_done() {
  printf '{"session_id":"%s","hook_event_name":"Stop","stop_hook_active":false,"background_tasks":[]}' "$RUN_ID" > "$IDLE.tmp"
  mv "$IDLE.tmp" "$IDLE"
}
await_exit() { while [ ! -f "$EXIT" ]; do sleep 0.05; done; }
commit() { printf 'change by %s\n' "$RUN_ID" > change.txt && git add change.txt && git commit -q -m "$1"; }
"#
);

pub const VALID_AGENT: &str = "commit work; receipt \"$(git rev-parse HEAD)\"";

/// The first workspace the test backend hands out; see `workspace_id`.
pub const WORKSPACE_ID: &str = "01234567-89ab-4def-8123-000000000000";

pub fn workspace_id(n: usize) -> String {
    format!("01234567-89ab-4def-8123-{n:012x}")
}

/// Shell prelude for a resumed session: `await_message` blocks until the
/// supervisor's resolution request arrived (the test backend writes it to
/// `$MESSAGE`) and sets `$MAIN` to the main it names; `receipt` / `idle` /
/// `await_exit` are the worker's.
pub const RESUME_PRELUDE: &str = concat!(
    watchdog!(),
    r#"
receipt() {
  printf '{"run_id":"%s","result":"%s","commit":"%s","tests":{"status":"passed","evidence_or_reason":"reran after the rebase"},"e2e":{"status":"not_applicable","evidence_or_reason":"no e2e surface"},"subagent_review":{"status":"not_applicable","evidence_or_reason":"resumed session"},"summary":"%s"}' "$RUN_ID" "${2:-succeeded}" "$1" "${3:-resolved}" > "$RECEIPT.tmp"
  mv "$RECEIPT.tmp" "$RECEIPT"
}
idle() {
  printf '{"session_id":"%s","hook_event_name":"Stop","stop_hook_active":false}' "$RUN_ID" > "$IDLE.tmp"
  mv "$IDLE.tmp" "$IDLE"
}
idle_bg() {
  printf '{"session_id":"%s","hook_event_name":"Stop","stop_hook_active":false,"background_tasks":[{"id":"b1","type":"shell","status":"running","description":"cargo test","command":"cargo test"}]}' "$RUN_ID" > "$IDLE.tmp"
  mv "$IDLE.tmp" "$IDLE"
}
idle_bg_done() {
  printf '{"session_id":"%s","hook_event_name":"Stop","stop_hook_active":false,"background_tasks":[]}' "$RUN_ID" > "$IDLE.tmp"
  mv "$IDLE.tmp" "$IDLE"
}
await_exit() { while [ ! -f "$EXIT" ]; do sleep 0.05; done; }
await_message() {
  while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
  MAIN=$(sed -n 's/.*main is now \([0-9a-f]*\) .*/\1/p' "$MESSAGE" | head -n 1)
}
# The supervisor polls `git status` in the worktree. It runs with
# GIT_OPTIONAL_LOCKS=0 now, but a git that took index.lock for a moment
# there made a session's `git add` or `git commit` fail (and failed tests
# under load), so the scripts still guard against the lock. `resolve` therefore looks at the rebase after every step, as a
# person would: it resolves the conflict while the file or the index needs
# it, skips a pick that is already in HEAD, and continues until the rebase
# is over. A command that only found the lock is retried.
unlocked() {
  locked=0
  until out=$("$@" 2>&1); do
    case $out in *index.lock*) ;; *) return 1 ;; esac
    locked=$((locked + 1))
    [ "$locked" -lt 100 ] || return 1
    sleep 0.05
  done
}
resolve() {
  unlocked git rebase -q "$MAIN" && return
  steps=0
  while [ -d "$(git rev-parse --git-path rebase-merge)" ]; do
    steps=$((steps + 1))
    [ "$steps" -lt 100 ] || return 1
    if [ "$(cat change.txt)" != 'resolved by the resumed session' ] || [ -n "$(git ls-files -u)" ]; then
      printf 'resolved by the resumed session\n' > change.txt
      git add change.txt >/dev/null 2>&1
    elif git diff --cached --quiet HEAD; then
      git rebase --skip >/dev/null 2>&1
    else
      GIT_EDITOR=true git rebase --continue >/dev/null 2>&1
    fi || sleep 0.05
  done
}
"#
);

pub struct TestProvider {
    pub script: String,
    /// The queue, for a script that runs `$DAGQ --db "$DB" ...` as a worker would.
    pub db: PathBuf,
}

impl AgentProvider for TestProvider {
    fn resume_command(&self, run: &TaskRun) -> Result<CommandSpec> {
        let run_dir = run.run_dir().unwrap();
        let mut command = CommandSpec::new("/bin/sh");
        command
            .current_dir(run.worktree_path().unwrap())
            .env("RUN_ID", run.id().as_str())
            .env("RECEIPT", run.receipt_path().unwrap())
            .env("IDLE", run.idle_marker_path().unwrap())
            .env("EXIT", exit_request_path(run_dir))
            .env("MESSAGE", resume_message_path(run_dir))
            .arg("-c")
            .arg(format!("{RESUME_PRELUDE}\n{}", self.script));
        Ok(command)
    }
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn wait_interval(&self) -> Duration {
        TEST_TICK
    }
    fn review_command(&self, _: &TaskRun, _: &str) -> Result<CommandSpec> {
        unreachable!("sessions do not review")
    }
    // `headless_command` keeps the default refusal: a run's provider has no
    // headless job, which the observer test relies on.
    fn command(&self, run: &TaskRun, prompt: &str) -> Result<CommandSpec> {
        assert!(prompt.contains("Acceptance criteria:"));
        assert!(prompt.contains("Verification commands (run in the worktree):"));
        // Every context section is present whether or not it has entries.
        assert!(prompt.contains("Goal"));
        assert!(prompt.contains("Context"));
        assert!(prompt.contains("Predecessor tasks"));
        assert!(prompt.contains("Sibling tasks in progress"));
        // A question goes to the queue as an ask, not to the terminal.
        assert!(prompt.contains(&format!(
            "`dagq ask --run {} --kind worker_question --because scope --question '...'`",
            run.id()
        )));
        // Background work is stopped before the receipt, or /exit stalls.
        assert!(prompt.contains(runtime::STOP_BACKGROUND), "{prompt}");
        let mut command = CommandSpec::new("/bin/sh");
        command
            .current_dir(run.worktree_path().unwrap())
            .env("RUN_ID", run.id().as_str())
            .env("RECEIPT", run.receipt_path().unwrap())
            .env("LOG", run.log_path().unwrap())
            .env("BASE", run.base_commit().as_str())
            .env("IDLE", run.idle_marker_path().unwrap())
            .env("EXIT", exit_request_path(run.run_dir().unwrap()))
            .env("MESSAGE", resume_message_path(run.run_dir().unwrap()))
            .env("DAGQ", env!("CARGO_BIN_EXE_dagq"))
            .env("DB", &self.db)
            .arg("-c")
            .arg(format!("{AGENT_PRELUDE}\n{}", self.script));
        Ok(command)
    }
}

/// Claude Code's empty input box, the screen `capture` returns unless a
/// test sets another: the session takes input, and what the supervisor
/// typed left the box.
pub const READY_SCREEN: &str = "\
⏺ Done.

──────────────────────────────────────────────────────────────────────
❯ 
──────────────────────────────────────────────────────────────────────
  ? for shortcuts
";

/// [`READY_SCREEN`] once the session took a text: at work on it.
pub const WORKING_SCREEN: &str = "\
⏺ Done.

✻ Working… (3s · esc to interrupt)

──────────────────────────────────────────────────────────────────────
❯ 
──────────────────────────────────────────────────────────────────────
  ? for shortcuts
";

/// Claude Code's input box still holding `text` after its Enter.
pub fn pending_screen(text: &str) -> String {
    format!(
        "⏺ Done.\n\n{rule}\n❯ {}\n{rule}\n  ? for shortcuts\n",
        dagq::infrastructure::adapters::single_line(text),
        rule = "─".repeat(70)
    )
}

/// The runner's shell line before Claude Code draws its input box.
pub const BOOT_SCREEN: &str =
    "worktree on dagq/run\n❯ '/run/runner' '--db' '/queue.db' 'session' '--resume'\n";

/// The test backend delivers an exit request as a file the fake agent polls for.
pub fn exit_request_path(run_dir: &str) -> PathBuf {
    Path::new(run_dir).join("exit-requested")
}

/// ... and the text typed into a resumed session the same way.
pub fn resume_message_path(run_dir: &str) -> PathBuf {
    Path::new(run_dir).join("resume-message")
}

/// One session the test backend started, keyed by its workspace id.
pub struct TestSession {
    pub run_id: RunId,
    pub run_dir: String,
    pub worker: Option<thread::JoinHandle<Result<Value>>>,
}

/// Starts the session wrapper on a thread per workspace, with the agent
/// script chosen per task, and records exits and closes per workspace.
pub struct TestWorkspace {
    pub db: PathBuf,
    pub fail: bool,
    pub close_fail: bool,
    pub script: String,
    pub scripts: Mutex<HashMap<TaskId, String>>,
    pub exit_timeout: Duration,
    pub registration_timeout: Duration,
    /// `create` opens the workspace but starts no session, so its wrapper
    /// never registers.
    pub no_session: bool,
    pub resume_timeout: Duration,
    /// `send_exit` returns only after the wrapper recorded its exit, as a
    /// slow `cmux send` does when the session exits on the first keystroke.
    pub exit_returns_after_session: bool,
    pub prompt_wait: Duration,
    /// What `capture` returns, and how often it was asked.
    pub screen: Mutex<String>,
    pub captures: AtomicUsize,
    /// `send_enter` calls: Enters sent again after a submit (task 285).
    pub enters: AtomicUsize,
    /// This many Enters leave a typed text in the input box (the screen
    /// shows it there until the last one), as a long paste does.
    pub swallowed_enters: AtomicUsize,
    /// This many texts are typed but never reach the session, as one typed
    /// before Claude Code's input box is drawn.
    pub dropped_texts: AtomicUsize,
    pub start_wait: Duration,
    pub exits_sent: AtomicUsize,
    pub sessions: Mutex<Vec<(String, TestSession)>>,
    pub closed: Mutex<Vec<String>>,
    /// `notify` calls as (title, body, workspace); the supervisor sends
    /// none, `ask` one per new ask (ADR-0022).
    pub notifications: Mutex<Vec<(String, String, Option<String>)>>,
    /// The tags each run workspace was opened with.
    pub tags: Mutex<Vec<WorkspaceTags>>,
    /// Every `ensure_group` call, as (external ID, name).
    pub groups: Mutex<Vec<(String, String)>>,
    /// `workspace-group create` fails.
    pub group_fails: bool,
    /// `send_exit` delivers the request but reports a timeout, the way
    /// `cmux send` does when cmux answers too late under load.
    pub send_times_out: bool,
    /// Resumed-session script per task; a resume of any other task fails.
    pub resume_scripts: Mutex<HashMap<TaskId, String>>,
    /// `create_resume` calls: the workspace name and the command.
    pub resumes: Mutex<Vec<(String, String)>>,
    /// `send_text` calls: the workspace and the text.
    pub texts: Mutex<Vec<(String, String)>>,
    /// `send_text` records the call and then fails, as a `cmux send` to a
    /// workspace that went away does.
    pub text_fails: bool,
    /// `exists` fails, as `cmux workspace list` does when cmux is gone.
    pub exists_fails: bool,
    /// Workspaces cmux lists although this backend did not open them (a
    /// run's workspace from an earlier supervisor), until they are closed.
    pub listed: Mutex<Vec<String>>,
    /// Workspaces cmux does not list for now although they are open.
    pub hidden: Mutex<Vec<String>>,
    /// This many captures time out, as `cmux read-screen` does under load.
    pub capture_timeouts: AtomicUsize,
    /// This many `send_exit` calls time out before the `/exit` reaches the
    /// session (task 354).
    pub exit_unsent: AtomicUsize,
    /// `close` ends the session in the workspace, as closing a cmux
    /// workspace kills its terminal, instead of requiring it gone.
    pub close_ends_session: bool,
    /// `close` times out and leaves the workspace and its session as they
    /// are, as cmux does under load.
    pub close_times_out: bool,
    /// `send_key` calls: the keys a known dialog was answered with. Enter on
    /// the "Background work is running" screen lets a held session exit,
    /// and Escape closes the Settings panel.
    pub keys: Mutex<Vec<String>>,
}

impl TestWorkspace {
    pub fn new(db: &Path, fail: bool, script: &str) -> Self {
        Self {
            db: db.into(),
            fail,
            close_fail: false,
            script: script.into(),
            scripts: Mutex::new(HashMap::new()),
            exit_timeout: Duration::from_secs(120),
            registration_timeout: Duration::from_secs(45),
            no_session: false,
            resume_timeout: Duration::from_secs(120),
            exit_returns_after_session: false,
            prompt_wait: Duration::from_secs(90),
            screen: Mutex::new(READY_SCREEN.into()),
            captures: AtomicUsize::new(0),
            enters: AtomicUsize::new(0),
            swallowed_enters: AtomicUsize::new(0),
            dropped_texts: AtomicUsize::new(0),
            start_wait: Duration::from_secs(60),
            exits_sent: AtomicUsize::new(0),
            sessions: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
            notifications: Mutex::new(Vec::new()),
            tags: Mutex::new(Vec::new()),
            groups: Mutex::new(Vec::new()),
            group_fails: false,
            send_times_out: false,
            resume_scripts: Mutex::new(HashMap::new()),
            resumes: Mutex::new(Vec::new()),
            texts: Mutex::new(Vec::new()),
            text_fails: false,
            exists_fails: false,
            listed: Mutex::new(Vec::new()),
            hidden: Mutex::new(Vec::new()),
            capture_timeouts: AtomicUsize::new(0),
            exit_unsent: AtomicUsize::new(0),
            close_ends_session: false,
            close_times_out: false,
            keys: Mutex::new(Vec::new()),
        }
    }
    /// Let cmux list `workspace` as if an earlier supervisor opened it.
    pub fn list(&self, workspace: &str) {
        self.listed.lock().unwrap().push(workspace.into());
    }
    /// Resumed-session script for one task.
    pub fn resume_script_for(&self, task_id: i64, script: &str) {
        self.resume_scripts
            .lock()
            .unwrap()
            .insert(TaskId::new(task_id), script.into());
    }
    pub fn texts(&self) -> Vec<(String, String)> {
        self.texts.lock().unwrap().clone()
    }
    /// Agent script for one task; other tasks use the default script.
    pub fn script_for(&self, task_id: i64, script: &str) {
        self.scripts
            .lock()
            .unwrap()
            .insert(TaskId::new(task_id), script.into());
    }
    pub fn closed(&self) -> Vec<String> {
        self.closed.lock().unwrap().clone()
    }
    /// Wait for every session wrapper started so far to return successfully.
    pub fn join(&self) {
        let workers: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .iter_mut()
            .filter_map(|(id, s)| s.worker.take().map(|worker| (id.clone(), worker)))
            .collect();
        for (id, worker) in workers {
            joined(
                worker,
                format!("the session wrapper of workspace {id} to return (its stub agent to exit)"),
            )
            .unwrap();
        }
    }
    pub fn session_run_dir(&self, workspace_id: &str) -> String {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| id == workspace_id)
            .map(|(_, s)| s.run_dir.clone())
            .expect("workspace was created")
    }
}

impl WorkspaceBackend for TestWorkspace {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn preflight_detached(&self, _: &SupervisorEnvironment) -> Result<()> {
        unreachable!("only up preflights the detached connection")
    }
    fn create(
        &self,
        task: &Task,
        run: &TaskRun,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        assert_eq!(task.id(), run.task_id());
        self.tags.lock().unwrap().push(tags.clone());
        assert!(
            Path::new(run.worktree_path().unwrap())
                .join("seed.txt")
                .exists()
        );
        assert!(command.contains("'\"'\"'")); // Database path contains an apostrophe.
        if self.fail {
            bail!("injected workspace creation failure");
        }
        let token: String = Connection::open(&self.db)?.query_row(
            "SELECT token FROM run_leases WHERE run_id=?1",
            [&run.id()],
            |r| r.get(0),
        )?;
        let db = self.db.clone();
        let id = run.id().clone();
        let script = self
            .scripts
            .lock()
            .unwrap()
            .get(&run.task_id())
            .cloned()
            .unwrap_or_else(|| self.script.clone());
        let mut sessions = self.sessions.lock().unwrap();
        let workspace = workspace_id(sessions.len());
        if self.no_session {
            sessions.push((
                workspace.clone(),
                TestSession {
                    run_id: run.id().clone(),
                    run_dir: run.run_dir().unwrap().to_owned(),
                    worker: None,
                },
            ));
            return Ok(workspace);
        }
        let worker = thread::spawn(move || {
            let provider = TestProvider {
                script,
                db: db.clone(),
            };
            let spawner = StubSpawner { db: db.clone() };
            runtime::session_with_provider(&db, &id, &token, &provider, &spawner)
        });
        sessions.push((
            workspace.clone(),
            TestSession {
                run_id: run.id().clone(),
                run_dir: run.run_dir().unwrap().to_owned(),
                worker: Some(worker),
            },
        ));
        Ok(workspace)
    }
    fn create_resume(
        &self,
        task: &Task,
        run: &TaskRun,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        assert_eq!(task.id(), run.task_id());
        assert!(command.ends_with(" '--resume'"), "{command}");
        // The worker's env (ADR-0026) and `run <run-id> resume` (ADR-0028).
        assert!(
            tags.env
                .iter()
                .any(|(k, v)| k == "DAGQ_ROLE" && v == "worker"),
            "{:?}",
            tags.env
        );
        assert!(tags.env.iter().any(|(k, _)| k == "DAGQ_QUEUE"));
        assert_eq!(
            tags.description.as_deref(),
            Some(format!("run {} resume", run.id()).as_str())
        );
        let script = self
            .resume_scripts
            .lock()
            .unwrap()
            .get(&run.task_id())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no resume script for task {}", run.task_id()))?;
        let token: String = Connection::open(&self.db)?.query_row(
            "SELECT token FROM run_leases WHERE run_id=?1",
            [&run.id()],
            |r| r.get(0),
        )?;
        let run_dir = run.run_dir().unwrap().to_owned();
        // The worker's session left its exit request and any earlier
        // resume its message behind.
        let _ = fs::remove_file(exit_request_path(&run_dir));
        let _ = fs::remove_file(resume_message_path(&run_dir));
        self.resumes.lock().unwrap().push((
            dagq::infrastructure::adapters::run_workspace_name(task, run)?,
            command.into(),
        ));
        let db = self.db.clone();
        let id = run.id().clone();
        let mut sessions = self.sessions.lock().unwrap();
        let workspace = workspace_id(sessions.len());
        let worker = thread::spawn(move || {
            let provider = TestProvider {
                script,
                db: db.clone(),
            };
            let spawner = StubSpawner { db: db.clone() };
            runtime::resume_session_with_provider(&db, &id, &token, &provider, &spawner)
        });
        sessions.push((
            workspace.clone(),
            TestSession {
                run_id: run.id().clone(),
                run_dir,
                worker: Some(worker),
            },
        ));
        Ok(workspace)
    }
    fn send_text(&self, workspace_id: &str, text: &str) -> Result<()> {
        self.texts
            .lock()
            .unwrap()
            .push((workspace_id.into(), text.into()));
        if self.text_fails {
            bail!("injected cmux send failure");
        }
        if self.swallowed_enters.load(Ordering::SeqCst) > 0 {
            *self.screen.lock().unwrap() = pending_screen(text);
        }
        if self
            .dropped_texts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Ok(());
        }
        // The session got it and works on it.
        let mut screen = self.screen.lock().unwrap();
        if *screen == READY_SCREEN {
            *screen = WORKING_SCREEN.into();
        }
        drop(screen);
        let path = resume_message_path(&self.session_run_dir(workspace_id));
        fs::write(path.with_extension("tmp"), text)?;
        fs::rename(path.with_extension("tmp"), path)?;
        Ok(())
    }
    fn send_enter(&self, _: &str) -> Result<()> {
        self.enters.fetch_add(1, Ordering::SeqCst);
        if self
            .swallowed_enters
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            == Ok(1)
        {
            *self.screen.lock().unwrap() = READY_SCREEN.into();
        }
        Ok(())
    }
    fn send_key(&self, workspace_id: &str, key: &str) -> Result<()> {
        self.keys.lock().unwrap().push(key.into());
        let mut screen = self.screen.lock().unwrap();
        match key {
            "enter" if screen.contains("Background work is running") => {
                *screen = READY_SCREEN.into();
                release_held_session(&self.session_run_dir(workspace_id));
            }
            "escape" if screen.contains("Settings:") => *screen = WORK_SCREEN.into(),
            _ => (),
        }
        Ok(())
    }
    fn resume_prompt_delay(&self) -> Duration {
        Duration::ZERO
    }
    fn submit_check_interval(&self) -> Duration {
        Duration::from_millis(10)
    }
    fn start_wait(&self) -> Duration {
        self.start_wait
    }
    fn resume_timeout(&self) -> Duration {
        self.resume_timeout
    }
    fn capture(&self, _: &str) -> Result<String> {
        self.captures.fetch_add(1, Ordering::SeqCst);
        if self
            .capture_timeouts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            bail!("cmux read-screen failed: Error: Command timed out");
        }
        Ok(self.screen.lock().unwrap().clone())
    }
    fn retry_backoff(&self) -> Duration {
        Duration::from_millis(10)
    }
    fn close(&self, workspace_id: &str) -> Result<()> {
        ensure!(
            !self.close_times_out,
            "cmux close-workspace failed: Command timed out"
        );
        // The session must have exited (or died, its wrapper's pid gone)
        // before the supervisor gives up the workspace. A workspace this
        // backend did not create (an orphan's) has no session here.
        let run_id = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| id == workspace_id)
            .map(|(_, s)| s.run_id.clone());
        let connection = Connection::open(&self.db)?;
        if self.close_ends_session
            && let Some(run_id) = &run_id
        {
            fs::write(exit_request_path(&self.session_run_dir(workspace_id)), "")?;
            let started = Instant::now();
            while !connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1 AND role='wrapper' AND exited_at IS NOT NULL)",
                [run_id],
                |r| r.get::<_, bool>(0),
            )? {
                ensure!(
                    started.elapsed() < Duration::from_secs(30),
                    "session did not end with its workspace"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
        if let Some(run_id) = &run_id {
            let exited: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1 AND role='wrapper' AND exited_at IS NOT NULL)",
                [run_id],
                |r| r.get(0),
            )?;
            assert!(exited);
        } else {
            let live: Vec<u32> = connection
                .prepare(
                    "SELECT p.pid FROM run_processes p JOIN task_runs r ON r.id=p.run_id
                     WHERE r.workspace_id=?1 AND p.role='wrapper' AND p.exited_at IS NULL",
                )?
                .query_map([workspace_id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            assert!(live.into_iter().all(|pid| !pid_alive(pid)));
        }
        if self.close_fail {
            bail!("injected workspace close failure");
        }
        self.closed.lock().unwrap().push(workspace_id.into());
        Ok(())
    }

    fn set_color(&self, _: &str, _: &str) -> Result<()> {
        unreachable!("only up colors a workspace")
    }
    fn set_status(&self, _: &str, _: &str, _: &str, _: &str) -> Result<()> {
        unreachable!("only up puts a status pill on a workspace")
    }
    fn pin(&self, _: &str) -> Result<()> {
        unreachable!("only up pins a workspace")
    }
    fn send_exit(&self, workspace_id: &str) -> Result<()> {
        self.exits_sent.fetch_add(1, Ordering::SeqCst);
        if self
            .exit_unsent
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            bail!("\"cmux\" send did not finish within 30s");
        }
        let run_dir = self.session_run_dir(workspace_id);
        fs::write(exit_request_path(&run_dir), "")?;
        if self.send_times_out {
            // The /exit got there: the transcript shows it.
            *self.screen.lock().unwrap() = format!("❯ /exit\n{READY_SCREEN}");
            bail!("\"cmux\" send did not finish within 30s");
        }
        if self.exit_returns_after_session {
            let run_id = self
                .sessions
                .lock()
                .unwrap()
                .iter()
                .find(|(id, _)| id == workspace_id)
                .map(|(_, s)| s.run_id.clone())
                .expect("workspace was created");
            let connection = Connection::open(&self.db)?;
            let started = Instant::now();
            while !connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1 AND role='wrapper' AND exited_at IS NOT NULL)",
                [&run_id],
                |r| r.get::<_, bool>(0),
            )? {
                ensure!(
                    started.elapsed() < Duration::from_secs(30),
                    "session did not exit"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(())
    }
    fn exit_timeout(&self) -> Duration {
        self.exit_timeout
    }
    fn registration_timeout(&self) -> Duration {
        self.registration_timeout
    }
    fn prompt_wait(&self) -> Duration {
        self.prompt_wait
    }
    // A workspace is listed from its creation until it is closed, as cmux
    // does; one this backend never opened is not.
    fn exists(&self, workspace_id: &str) -> Result<bool> {
        ensure!(!self.exists_fails, "injected workspace list failure");
        Ok(self
            .listed_workspace_ids()?
            .iter()
            .any(|listed| listed == workspace_id))
    }
    fn listed_workspace_ids(&self) -> Result<Vec<String>> {
        ensure!(!self.exists_fails, "injected workspace list failure");
        let closed = self.closed();
        let mut listed: Vec<String> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        listed.extend(self.listed.lock().unwrap().iter().cloned());
        let hidden = self.hidden.lock().unwrap();
        listed.retain(|id| !closed.contains(id) && !hidden.contains(id));
        Ok(listed)
    }
    fn create_named(&self, _: &str, _: &Path, _: &str, _: &WorkspaceTags) -> Result<String> {
        bail!("not used by the supervisor")
    }
    fn ensure_group(&self, external_id: &str, name: &str) -> Result<String> {
        self.groups
            .lock()
            .unwrap()
            .push((external_id.into(), name.into()));
        if self.group_fails {
            bail!("workspace-group create failed")
        }
        Ok(format!("group-{external_id}"))
    }
    fn notify(&self, title: &str, body: &str, workspace: Option<&str>) -> Result<()> {
        self.notifications.lock().unwrap().push((
            title.into(),
            body.into(),
            workspace.map(Into::into),
        ));
        Ok(())
    }
}

/// /bin/sh --version is not portable; a tiny standalone provider preflight stub.
pub fn claude_stub(db: &Path) -> PathBuf {
    let stub = db.parent().unwrap().join("claude-stub");
    fs::write(&stub, "#!/bin/sh\nprintf 'test provider\\n'\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    stub
}

/// The supervisor's pass and idle intervals and the wrapper's wait interval
/// in these tests: short, so a run's steps follow each other without a
/// second's pause.
pub const TEST_TICK: Duration = Duration::from_millis(50);

/// Supervisor options with the test tick and the [`SteadyClock`].
pub fn supervise_options(parallel: usize, once: bool) -> SuperviseOptions {
    SuperviseOptions {
        tick: TEST_TICK,
        idle_poll: TEST_TICK,
        generators: Generators {
            clock: Arc::new(SteadyClock(SystemTime::now(), Instant::now())),
            ..clock::system()
        },
        ..SuperviseOptions::new(parallel, once)
    }
}

/// The clock of one supervisor in these tests: the wall clock when its
/// options were made, advanced by the monotonic clock. The supervisor's
/// heartbeat thread waits its 2 seconds on the monotonic clock, which stops
/// while the host sleeps (`CLOCK_UPTIME_RAW` on macOS), while the wall
/// clock jumps by the time asleep; a jump past `HEARTBEAT_TIMEOUT_SECS`
/// made the loop's next lease-checked write fail with "run lease is missing
/// or stale" before the heartbeat caught up. This clock does not jump, and
/// agrees with the wall clock again for the next supervisor, so the times
/// a test writes with `unixepoch()` before it supervises still hold.
pub struct SteadyClock(pub SystemTime, pub Instant);

impl Clock for SteadyClock {
    fn system_time(&self) -> SystemTime {
        self.0 + self.1.elapsed()
    }
}

/// One pass of the parallel supervisor: claim whatever is ready, finish it, exit.
pub fn supervise(db: &Path, repo: &Path, backend: &TestWorkspace) -> Result<Value> {
    supervise_with(db, repo, backend, &supervise_options(4, true))
}

pub fn supervise_with(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    options: &SuperviseOptions,
) -> Result<Value> {
    let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
    runtime::supervise(
        db,
        repo,
        backend,
        &claude_stub(db),
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        options,
    )
}

/// The asks other than `approve_landing`: those the supervisor opens for a
/// run whose stand-in review printed no verdict (task 328) go with every
/// run these tests leave awaiting integration.
pub fn other_asks(queue: &mut SqliteQueue, all: bool) -> Vec<dagq::domain::Ask> {
    queue
        .asks(AskQuery {
            all,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .filter(|ask| ask.kind != AskKind::ApproveLanding)
        .collect()
}

/// Join `thread`, failing the test with `what` if it has not returned
/// within [`common::STEP_LIMIT`]; a panic in it fails the test as is.
pub fn joined<T>(thread: thread::JoinHandle<T>, what: impl Into<String>) -> T {
    let _waiting = common::within(common::STEP_LIMIT, what);
    thread
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Poll the queue until `condition` holds or the deadline passes.
pub fn wait_until(
    db: &Path,
    timeout: Duration,
    mut condition: impl FnMut(&mut SqliteQueue) -> bool,
) {
    let started = Instant::now();
    let mut queue = SqliteQueue::open(db).unwrap();
    while !condition(&mut queue) {
        assert!(
            started.elapsed() < timeout,
            "condition not met within {timeout:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Run one fake agent script through supervise and return the task detail.
pub fn run_agent(script: &str) -> (Fixture, PathBuf, dagq::domain::TaskDetail) {
    run_agent_with(script, false)
}

pub fn run_agent_with(
    script: &str,
    close_fail: bool,
) -> (Fixture, PathBuf, dagq::domain::TaskDetail) {
    let (dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, script);
    backend.close_fail = close_fail;
    let outcome = supervise(&db, &repo, &backend).unwrap();
    // These scripts exit on their own, like a person's /exit; nothing was requested.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 1);
    assert_eq!(outcome["errors"], json!([]));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    let run = &detail.runs[0];
    assert_eq!(outcome["runs"][0]["id"], json!(run.id()));
    // Runs live in `runs/` next to the (canonicalized) database, worktree inside.
    let run_dir = db
        .canonicalize()
        .unwrap()
        .with_file_name("runs")
        .join(run.id().as_str());
    assert_eq!(Path::new(run.run_dir().unwrap()), run_dir);
    assert_eq!(
        Path::new(run.worktree_path().unwrap()),
        run_dir.join("worktree")
    );
    // Every outcome keeps the worktree; only an accepted run closes its workspace.
    assert!(Path::new(run.worktree_path().unwrap()).exists());
    assert_eq!(run.workspace_id(), Some(WORKSPACE_ID));
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    if run.status() == RunStatus::AwaitingIntegration && !close_fail {
        assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
        assert!(run.workspace_closed_at().is_some());
        assert!(kinds.contains(&"workspace_closed"));
    } else {
        assert!(backend.closed().is_empty());
        assert!(run.workspace_closed_at().is_none());
        assert!(!kinds.contains(&"workspace_closed"));
    }
    assert_eq!(kinds.contains(&"cleanup_failed"), close_fail);
    // A run at rest is reported through `watch`, not a notification
    // (ADR-0022); only the `approve_landing` ask of an accepted run whose
    // stand-in review printed no verdict notifies (task 328).
    assert_eq!(
        backend.notifications.lock().unwrap().len(),
        usize::from(run.status() == RunStatus::AwaitingIntegration),
        "{:?}",
        backend.notifications.lock().unwrap()
    );
    assert!(
        backend
            .notifications
            .lock()
            .unwrap()
            .iter()
            .all(|n| n.0.ends_with("approve_landing"))
    );
    // The run workspace carries its role and queue in its environment, a
    // description naming the run and task, and the queue's group.
    let canonical = db.canonicalize().unwrap();
    let hash = QueueLocation::explicit(&canonical).hash();
    assert_eq!(
        *backend.tags.lock().unwrap(),
        vec![WorkspaceTags {
            env: vec![
                ("DAGQ_ROLE".into(), "worker".into()),
                ("DAGQ_QUEUE".into(), canonical.to_str().unwrap().into()),
            ],
            description: Some(format!(
                "dagq role=worker queue={hash} run={} task={}",
                run.id(),
                run.task_id()
            )),
            group: Some(format!("group-{hash}")),
        }]
    );
    assert_eq!(backend.groups.lock().unwrap().len(), 1);
    // The run came to rest: its lease is gone, and the task still owns it.
    assert!(kinds.contains(&"lease_acquired"));
    assert!(kinds.contains(&"lease_released"));
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(queue.candidates().unwrap().is_empty());
    (dir, db, detail)
}

/// The prompt the run's agent was started with, as `provision` wrote it.
pub fn read_prompt(run: &TaskRun) -> String {
    fs::read_to_string(Path::new(run.run_dir().unwrap()).join("prompt.txt")).unwrap()
}

pub fn rejection_reason(detail: &dagq::domain::TaskDetail) -> String {
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Failed);
    let event = detail
        .events
        .iter()
        .find(|e| e.kind == "validation_finished")
        .unwrap();
    assert_eq!(event.payload["status"], "failed");
    assert_eq!(event.payload["accepted"], false);
    let reason = event.payload["reason"].as_str().unwrap().to_owned();
    assert_eq!(run.last_error(), Some(reason.as_str()));
    reason
}

pub fn event_kinds(detail: &dagq::domain::TaskDetail) -> Vec<&str> {
    detail.events.iter().map(|e| e.kind.as_str()).collect()
}

/// Fake agent that ignores the supervisor's `/exit` (as when a dialog holds
/// it back) and ends only once the test writes `$EXIT.held`, the way a person
/// would answer the dialog and exit.
/// Blocks a fake session until the test calls `release_held_session`.
pub const HOLD: &str = "while [ ! -f \"$EXIT.held\" ]; do sleep 0.05; done";

pub const HELD_AGENT: &str = "commit work; receipt \"$(git rev-parse HEAD)\"; idle; while [ ! -f \"$EXIT.held\" ]; do sleep 0.05; done";

pub fn release_held_session(run_dir: &str) {
    fs::write(Path::new(run_dir).join("exit-requested.held"), "").unwrap();
}

/// A screen of ordinary work.
pub const WORK_SCREEN: &str =
    "⏺ Bash(cargo test)\n  ⎿  test result: ok\n\n│ ❯ \n  ? for shortcuts\n";

/// Fake agent that works (no receipt, no idle marker) until the test writes
/// `$EXIT.go`, then finishes like `VALID_AGENT` and waits for `/exit`.
pub const PROMPTED_AGENT: &str = "while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit";

/// A session stopped at a login that ran out (task 266).
pub const LOGIN_SCREEN: &str = "\
⏺ Bash(cargo test)
  ⎿  API Error: 401 {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"OAuth token has expired.\"}} · Please run /login

│ ❯ 
  ? for shortcuts
";

/// The attention entries of `status` for one ask.
pub fn ask_attention(status: &Value, ask_id: AskId) -> Vec<Value> {
    status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["ask_id"] == ask_id.as_i64())
        .cloned()
        .collect()
}

/// The `backend_call_failed` events of a task, oldest first.
pub fn backend_failures(detail: &dagq::domain::TaskDetail) -> Vec<&dagq::domain::RunEvent> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == "backend_call_failed")
        .collect()
}

/// Whether a process with `pid` exists.
pub fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .bounded_status()
        .unwrap()
        .success()
}

/// A `sleep 60` with no stream of the test process: one a failing test
/// leaves behind does not hold the pipe of `cargo test | grep` open.
pub fn sleeper() -> Child {
    Command::new("sleep")
        .arg("60")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// A PID that certainly belonged to a process that has already exited.
pub fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// Register a run the way `supervise` does under `token`, with the given PIDs
/// as wrapper and agent, but with no supervisor loop watching it. Returns the
/// running run.
pub fn orphan_run(repo: &Path, db: &Path, token: &str, wrapper: u32, agent: u32) -> TaskRun {
    use dagq::{
        domain::ClaimOutcome,
        infrastructure::{
            adapters::{GitRepository, path_text},
            runtime_store::RunPlan,
        },
    };
    let repository = GitRepository::inspect(repo).unwrap();
    let mut queue = SqliteQueue::open(db).unwrap();
    queue
        .bind_repository(&path_text(&repository.common_dir).unwrap())
        .unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&repository.base_commit, token)
        .unwrap()
    else {
        panic!()
    };
    let run_dir = dagq::infrastructure::location::runs_dir(db).join(run.id().as_str());
    fs::create_dir_all(&run_dir).unwrap();
    queue
        .plan_run(
            run.id(),
            token,
            &RunPlan {
                repo_path: path_text(&repository.root).unwrap(),
                run_dir: path_text(&run_dir).unwrap(),
                branch: format!("dagq/{}", run.id()),
                worktree_path: path_text(&run_dir.join("worktree")).unwrap(),
                receipt_path: path_text(&run_dir.join("receipt.json")).unwrap(),
                log_path: path_text(&run_dir.join("claude.debug.log")).unwrap(),
            },
        )
        .unwrap();
    let run = queue.run(run.id()).unwrap();
    repository.create_worktree(&run).unwrap();
    queue
        .workspace_created(run.id(), token, &format!("ws-{}", run.task_id()))
        .unwrap();
    queue.register_wrapper(run.id(), token, wrapper).unwrap();
    queue.register_agent(run.id(), wrapper, agent).unwrap();
    let run = queue.run(run.id()).unwrap();
    assert_eq!(run.status(), RunStatus::Running);
    run
}

pub fn git_out(repo: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}

/// `integrate` as the CLI runs it without `--no-push`: pushing through the
/// real Git adapter, which finds no origin in the fixtures.
pub fn integrate(db: &Path, task_id: i64, repo: &Path) -> Result<Value> {
    let remote = GitRepository::inspect(repo).ok();
    runtime::integrate(
        db,
        IntegrateTarget::Task(TaskId::new(task_id)),
        repo,
        remote.as_ref().map(|r| r as &dyn MainRemote),
    )
}

pub fn integrate_next(db: &Path, repo: &Path) -> Value {
    let remote = GitRepository::inspect(repo).unwrap();
    runtime::integrate(db, IntegrateTarget::Next, repo, Some(&remote)).unwrap()
}

pub fn events_of(db: &Path, run_id: &RunId, kind: &str) -> Vec<Value> {
    SqliteQueue::open(db)
        .unwrap()
        .run_events(run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.payload)
        .collect()
}

/// Rewrite the run's receipt the way a resumed session would after its work.
pub fn write_receipt(run: &TaskRun, commit: &str, result: &str, summary: &str) {
    write_receipt_json(run, session_receipt(run, commit, result, summary));
}

/// A session's receipt for `commit`, with the evidence it would give.
pub fn session_receipt(run: &TaskRun, commit: &str, result: &str, summary: &str) -> Value {
    json!({
        "run_id": run.id(), "result": result, "commit": commit,
        "tests": {"status": "passed", "evidence_or_reason": "reran"},
        "e2e": {"status": "not_applicable", "evidence_or_reason": "none"},
        "subagent_review": {"status": "not_applicable", "evidence_or_reason": "session"},
        "summary": summary,
    })
}

pub fn write_receipt_json(run: &TaskRun, receipt: Value) {
    let path = Path::new(run.receipt_path().unwrap());
    fs::write(path.with_extension("tmp"), receipt.to_string()).unwrap();
    fs::rename(path.with_extension("tmp"), path).unwrap();
}

/// The payloads of the `verification_command` events the landing recorded
/// (`phase: integration`), oldest first. Validation runs no verification
/// command (ADR-0023 decision 1).
pub fn integration_verifications(detail: &dagq::domain::TaskDetail) -> Vec<&Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == "verification_command" && e.payload["phase"] == "integration")
        .map(|e| &e.payload)
        .collect()
}

/// The `integration_receipt` events of the task's run, oldest first.
pub fn integration_receipts(detail: &dagq::domain::TaskDetail) -> Vec<&Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == "integration_receipt")
        .map(|e| &e.payload)
        .collect()
}

/// Assert that `main` is linear, `commits` long on top of `seed`, and that
/// its head carries the landing of `run` with the same tree as `source`.
pub fn assert_landed(repo: &Path, run: &TaskRun, task_title: &str, expected_parent: &str) {
    let main = git_out(repo, &["rev-parse", "main"]);
    assert_eq!(run.status(), RunStatus::Integrated);
    assert_eq!(
        run.result_commit().map(CommitSha::as_str),
        Some(main.as_str())
    );
    assert_eq!(git_out(repo, &["rev-parse", "main^"]), expected_parent);
    assert_eq!(
        git_out(repo, &["rev-list", "--parents", "-1", "main"])
            .split(' ')
            .count(),
        2
    );
    let history = format!("refs/dagq/runs/{}", run.id());
    let source = git_out(repo, &["rev-parse", &history]);
    assert_eq!(
        git_out(repo, &["rev-parse", "main^{tree}"]),
        git_out(repo, &["rev-parse", &format!("{source}^{{tree}}")])
    );
    let message = git_out(repo, &["log", "-1", "--format=%B", "main"]);
    assert!(message.starts_with(task_title), "{message}");
    assert!(message.contains("\n\nDagq-Task: "), "{message}");
    assert!(
        message.ends_with(&format!("Dagq-Run: {}", run.id())),
        "{message}"
    );
    // Worktree and branch are gone; the run's history stays under the ref.
    assert!(!Path::new(run.worktree_path().unwrap()).exists());
    assert!(!git_out(repo, &["branch", "--list", run.branch().unwrap()]).contains("dagq/"));
}

/// A validated run plus a ready dependent task, before any landing.
pub fn awaiting_run() -> (Fixture, PathBuf, PathBuf, TaskRun) {
    let (dir, db, detail) = run_agent(VALID_AGENT);
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let dependent = queue
        .add(NewTask {
            title: "dependent".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: vec![],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            dependencies: vec![TaskId::new(1)],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    queue
        .transition(dependent.id(), TaskAction::BypassReview)
        .unwrap();
    assert!(queue.candidates().unwrap().is_empty());
    let repo = dir.path().join("repo's directory");
    (dir, repo, db, run)
}

/// Two tasks rewrite `change.txt`: the first lands, and a person's
/// `integrate` of the second (its approval) conflicts and parks it as
/// `needs_session`. Returns the parked run and the landed main.
pub fn parked_conflict(repo: &Path, db: &Path, backend: &TestWorkspace) -> (TaskRun, String) {
    let mut queue = SqliteQueue::open(db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    supervise(db, repo, backend).unwrap();
    backend.join();
    assert_eq!(integrate(db, 1, repo).unwrap()["outcome"], "integrated");
    let first_landed = git_out(repo, &["rev-parse", "main"]);
    let parked = integrate(db, 2, repo).unwrap();
    assert_eq!(parked["outcome"], "needs_session", "{parked}");
    let run = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::NeedsSession);
    (run, first_landed)
}

/// Make the conflict that parked the run count toward the resume attempts,
/// as a failed verification would (ADR-0047 decision 24: a resume of a run
/// parked only by a conflict after its landing was approved is not one of
/// them), for tests of what happens once the attempts are used up.
pub fn count_resumes_of_parked(db: &Path) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE run_events SET payload=json_set(payload,'$.code','verification_failed')
             WHERE kind='integration_deferred'",
            [],
        )
        .unwrap();
}

pub fn payloads<'a>(detail: &'a dagq::domain::TaskDetail, kind: &str) -> Vec<&'a Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| &e.payload)
        .collect()
}

pub const IDLE_AGENT: &str = "commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit";

/// Provision and start a run under `token` exactly as `supervise` does
/// (claim, plan, prompt, worktree, workspace with the wrapper thread of the
/// test backend), but with no loop or heartbeat behind the token: the state
/// a supervisor leaves when it is killed after the session started. Returns
/// the `running` run once the wrapper registered its agent.
pub fn start_run_under_dead_supervisor(
    repo: &Path,
    db: &Path,
    backend: &TestWorkspace,
    token: &str,
) -> TaskRun {
    use dagq::{
        domain::ClaimOutcome,
        infrastructure::{
            adapters::{GitRepository, path_text},
            location::runs_dir,
            runtime_store::RunPlan,
        },
    };
    let repository = GitRepository::inspect(repo).unwrap();
    let mut queue = SqliteQueue::open(db).unwrap();
    queue
        .bind_repository(&path_text(&repository.common_dir).unwrap())
        .unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&repository.main_head().unwrap(), token)
        .unwrap()
    else {
        panic!("no candidate to claim")
    };
    let run_dir = runs_dir(&db.canonicalize().unwrap()).join(run.id().as_str());
    queue
        .plan_run(
            run.id(),
            token,
            &RunPlan {
                repo_path: path_text(&repository.root).unwrap(),
                run_dir: path_text(&run_dir).unwrap(),
                branch: format!("dagq/{}", run.id()),
                worktree_path: path_text(&run_dir.join("worktree")).unwrap(),
                receipt_path: path_text(&run_dir.join("receipt.json")).unwrap(),
                log_path: path_text(&run_dir.join("claude.debug.log")).unwrap(),
            },
        )
        .unwrap();
    fs::create_dir_all(&run_dir).unwrap();
    let run = queue.run(run.id()).unwrap();
    let task = queue.show(run.task_id()).unwrap().task;
    fs::write(
        run_dir.join("prompt.txt"),
        runtime::prompt(&task, &run, None, &[], &[], &[], None).unwrap(),
    )
    .unwrap();
    repository.create_worktree(&run).unwrap();
    let command = shell_join(&[
        "runner".into(),
        "--db".into(),
        path_text(db).unwrap(),
        "session".into(),
    ]);
    let workspace = backend
        .create(&task, &run, &command, &WorkspaceTags::default())
        .unwrap();
    queue
        .workspace_created(run.id(), token, &workspace)
        .unwrap();
    wait_until(db, Duration::from_secs(10), |queue| {
        queue.run(run.id()).unwrap().status() == RunStatus::Running
    });
    queue.run(run.id()).unwrap()
}

/// Age the lease of `run` so it is stale by heartbeat while its pid (this
/// test process) is alive, like a supervisor that stopped heartbeating.
pub fn age_lease(db: &Path, run: &TaskRun, seconds: i64) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET heartbeat_at=unixepoch()-?2 WHERE run_id=?1",
            rusqlite::params![run.id(), seconds],
        )
        .unwrap();
}

pub fn adoption_events(detail: &dagq::domain::TaskDetail) -> Vec<&Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == "run_adopted")
        .map(|e| &e.payload)
        .collect()
}

pub fn supervisor_token_of(db: &Path, run: &TaskRun) -> String {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT supervisor_token FROM task_runs WHERE id=?1",
            [&run.id()],
            |r| r.get(0),
        )
        .unwrap()
}

/// The run's own attention; an ask about the run (with `ask_id`) is not.
pub fn run_attention_of<'a>(status: &'a Value, run_id: &RunId) -> Option<&'a Value> {
    status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_id"] == run_id.as_str() && a["ask_id"].is_null())
}

/// Stands in for the headless reviewer (ADR-0027): each review runs the
/// next script with `/bin/sh -c` in the worktree (the last one repeats) and
/// records its prompt; `timeout` is the review timeout.
pub struct TestReviewer {
    pub scripts: Mutex<Vec<String>>,
    pub prompts: Mutex<Vec<String>>,
    pub timeout: Duration,
    /// Scripts of the headless triage, one per triage in order; without
    /// one left, a triage cannot start.
    pub triages: Mutex<Vec<String>>,
    /// The triage prompts and the directories they ran in.
    pub triage_prompts: Mutex<Vec<(String, PathBuf)>>,
}

impl TestReviewer {
    pub fn new(scripts: &[String]) -> Self {
        Self {
            scripts: Mutex::new(scripts.to_vec()),
            prompts: Mutex::new(Vec::new()),
            timeout: Duration::from_secs(60),
            triages: Mutex::new(Vec::new()),
            triage_prompts: Mutex::new(Vec::new()),
        }
    }
    pub fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
    pub fn with_triages(self, scripts: &[String]) -> Self {
        *self.triages.lock().unwrap() = scripts.to_vec();
        self
    }
    pub fn triage_prompts(&self) -> Vec<(String, PathBuf)> {
        self.triage_prompts.lock().unwrap().clone()
    }
}

impl AgentProvider for TestReviewer {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn command(&self, _: &TaskRun, _: &str) -> Result<CommandSpec> {
        unreachable!("the reviewer starts no session")
    }
    fn resume_command(&self, _: &TaskRun) -> Result<CommandSpec> {
        unreachable!("the reviewer starts no session")
    }
    // A run that fails under these tests is triaged by this provider too:
    // with no triage script left, the triage fails and the run waits for a
    // person.
    fn headless_command(&self, cwd: &Path, prompt: &str, tools: &[&str]) -> Result<CommandSpec> {
        assert_eq!(tools, runtime::TRIAGE_TOOLS);
        let mut triages = self.triages.lock().unwrap();
        ensure!(!triages.is_empty(), "the test reviewer has no triage left");
        self.triage_prompts
            .lock()
            .unwrap()
            .push((prompt.into(), cwd.into()));
        let mut command = CommandSpec::new("/bin/sh");
        command.current_dir(cwd).arg("-c").arg(triages.remove(0));
        Ok(command)
    }
    fn review_command(&self, run: &TaskRun, prompt: &str) -> Result<CommandSpec> {
        self.prompts.lock().unwrap().push(prompt.into());
        let mut scripts = self.scripts.lock().unwrap();
        let script = if scripts.len() > 1 {
            scripts.remove(0)
        } else {
            scripts[0].clone()
        };
        let mut command = CommandSpec::new("/bin/sh");
        command
            .current_dir(run.worktree_path().unwrap())
            .arg("-c")
            .arg(script);
        Ok(command)
    }
    fn review_timeout(&self) -> Duration {
        self.timeout
    }
}

/// A reviewer script that prints the verdict JSON.
pub fn verdict(decision: &str, reasons: &[&str], summary: &str) -> String {
    let json = json!({"verdict": decision, "reasons": reasons, "summary": summary});
    format!("printf '%s\\n' '{json}'")
}

pub fn supervise_reviewed(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    reviewer: &TestReviewer,
) -> Value {
    let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
    let outcome = runtime::supervise_with_reviewer(
        db,
        repo,
        backend,
        &claude_stub(db),
        reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &supervise_options(4, true),
    )
    .unwrap();
    backend.join();
    outcome
}

/// The position of the first event of `kind`, which must exist.
pub fn position(kinds: &[&str], kind: &str) -> usize {
    kinds
        .iter()
        .position(|k| *k == kind)
        .unwrap_or_else(|| panic!("no {kind} in {kinds:?}"))
}

pub fn assert_landed_run(run: &TaskRun, repo: &Path, base: &str) {
    assert_landed(repo, run, "test task", base);
}

/// A triage script that prints the verdict JSON (no apostrophes in the
/// texts: the script quotes the JSON with them).
pub fn triage(decision: &str, reason: &str, instruction: &str) -> String {
    let json = json!({"verdict": decision, "reason": reason, "instruction": instruction});
    format!("printf '%s\\n' '{json}'")
}

pub fn stalled_asks(queue: &SqliteQueue) -> Vec<dagq::domain::Ask> {
    queue
        .asks(AskQuery {
            all: true,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .filter(|ask| ask.kind == AskKind::Stalled)
        .collect()
}
