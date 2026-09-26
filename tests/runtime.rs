mod common;

use common::Bounded;

use anyhow::{Result, bail, ensure};
use dagq::{
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
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
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
use tempfile::TempDir;

/// A full Git object ID as the runtime takes it.
fn sha(commit: &str) -> CommitSha {
    CommitSha::try_from(commit).unwrap()
}

fn git(repo: &Path, args: &[&str]) {
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
struct Fixture {
    db: PathBuf,
    dir: TempDir,
    _test: common::Waiting,
}
impl Fixture {
    fn path(&self) -> &Path {
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
static STUBS: LazyLock<Mutex<HashMap<PathBuf, Option<Vec<u32>>>>> = LazyLock::new(Default::default);

fn stubs() -> MutexGuard<'static, HashMap<PathBuf, Option<Vec<u32>>>> {
    STUBS.lock().unwrap_or_else(PoisonError::into_inner)
}

fn kill_stubs(db: &Path) {
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
struct StubSpawner {
    db: PathBuf,
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

struct Stub(Child);
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
fn running(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .bounded_output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

/// A stub agent and the children it started die with the test's fixture
/// when the test ends while the stub still runs, here by a panic, and the
/// stub holds none of the test process's streams (task 317). To check a
/// whole run by hand: after `cargo test --locked`,
/// `ps -axo pid,ppid,command | grep -e 'test -f seed.txt' -e await_exit`
/// lists no stub.
#[test]
fn a_stub_agent_dies_with_its_fixture_and_holds_no_stream_of_the_test() {
    let out = tempfile::tempdir().unwrap();
    let (pids, streams) = (out.path().join("pids"), out.path().join("streams"));
    let (stub_pid, db) = {
        let (pids, streams) = (pids.clone(), streams.clone());
        let stub = Arc::new(Mutex::new(None));
        let started = stub.clone();
        let panicked = std::panic::catch_unwind(move || {
            let (_dir, _repo, db) = fixture();
            let mut spec = CommandSpec::new("/bin/sh");
            spec.env("PIDS", &pids)
                .env("STREAMS", &streams)
                .arg("-c")
                .arg(concat!(
                    watchdog!(),
                    r#"
for fd in 0 1 2; do [ /dev/fd/$fd -ef /dev/null ] && printf '%s ' null >> "$STREAMS.tmp"; done
mv "$STREAMS.tmp" "$STREAMS"
sleep 300 &
printf '%s %s\n' $$ $! > "$PIDS.tmp"
mv "$PIDS.tmp" "$PIDS"
while :; do sleep 0.05; done
"#
                ));
            let child = StubSpawner { db: db.clone() }
                .spawn(&spec, Streams::Inherit)
                .unwrap();
            *started.lock().unwrap() = Some((child.id(), db));
            let begun = Instant::now();
            while !pids.exists() {
                assert!(begun.elapsed() < Duration::from_secs(30));
                thread::sleep(Duration::from_millis(20));
            }
            panic!("the test fails while its stub runs");
        });
        assert!(panicked.is_err());
        stub.lock().unwrap().take().unwrap()
    };
    assert_eq!(fs::read_to_string(&streams).unwrap(), "null null null ");
    let text = fs::read_to_string(&pids).unwrap();
    let (shell, sleep) = text.trim().split_once(' ').unwrap();
    assert_eq!(shell, stub_pid.to_string());
    let sleep: u32 = sleep.parse().unwrap();
    // The shell is this process's child: reaped here once killed.
    let begun = Instant::now();
    loop {
        // SAFETY: waitpid(2) with a null status pointer writes nothing.
        let reaped =
            unsafe { libc::waitpid(stub_pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
        if reaped == stub_pid as libc::pid_t {
            break;
        }
        assert!(
            begun.elapsed() < Duration::from_secs(10),
            "stub {stub_pid} lives on"
        );
        thread::sleep(Duration::from_millis(20));
    }
    while running(sleep) {
        assert!(
            begun.elapsed() < Duration::from_secs(10),
            "sleep {sleep} lives on"
        );
        thread::sleep(Duration::from_millis(20));
    }
    // The fixture's queue starts no more stubs.
    let error = StubSpawner { db }
        .spawn(
            CommandSpec::new("/bin/sh").arg("-c").arg("exit 0"),
            Streams::Inherit,
        )
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "the test's fixture is gone");
}

fn fixture() -> (Fixture, PathBuf, PathBuf) {
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

fn add_ready_task(queue: &mut SqliteQueue, title: &str, dependencies: &[TaskId]) -> TaskId {
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
const AGENT_PRELUDE: &str = concat!(
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
const VALID_AGENT: &str = "commit work; receipt \"$(git rev-parse HEAD)\"";
/// The first workspace the test backend hands out; see `workspace_id`.
const WORKSPACE_ID: &str = "01234567-89ab-4def-8123-000000000000";

fn workspace_id(n: usize) -> String {
    format!("01234567-89ab-4def-8123-{n:012x}")
}

/// Shell prelude for a resumed session: `await_message` blocks until the
/// supervisor's resolution request arrived (the test backend writes it to
/// `$MESSAGE`) and sets `$MAIN` to the main it names; `receipt` / `idle` /
/// `await_exit` are the worker's.
const RESUME_PRELUDE: &str = concat!(
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

struct TestProvider {
    script: String,
    /// The queue, for a script that runs `$DAGQ --db "$DB" ...` as a worker would.
    db: PathBuf,
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
const READY_SCREEN: &str = "\
⏺ Done.

──────────────────────────────────────────────────────────────────────
❯ 
──────────────────────────────────────────────────────────────────────
  ? for shortcuts
";

/// [`READY_SCREEN`] once the session took a text: at work on it.
const WORKING_SCREEN: &str = "\
⏺ Done.

✻ Working… (3s · esc to interrupt)

──────────────────────────────────────────────────────────────────────
❯ 
──────────────────────────────────────────────────────────────────────
  ? for shortcuts
";

/// Claude Code's input box still holding `text` after its Enter.
fn pending_screen(text: &str) -> String {
    format!(
        "⏺ Done.\n\n{rule}\n❯ {}\n{rule}\n  ? for shortcuts\n",
        dagq::infrastructure::adapters::single_line(text),
        rule = "─".repeat(70)
    )
}

/// The runner's shell line before Claude Code draws its input box.
const BOOT_SCREEN: &str =
    "worktree on dagq/run\n❯ '/run/runner' '--db' '/queue.db' 'session' '--resume'\n";

/// The test backend delivers an exit request as a file the fake agent polls for.
fn exit_request_path(run_dir: &str) -> PathBuf {
    Path::new(run_dir).join("exit-requested")
}

/// ... and the text typed into a resumed session the same way.
fn resume_message_path(run_dir: &str) -> PathBuf {
    Path::new(run_dir).join("resume-message")
}

/// One session the test backend started, keyed by its workspace id.
struct TestSession {
    run_id: RunId,
    run_dir: String,
    worker: Option<thread::JoinHandle<Result<Value>>>,
}

/// Starts the session wrapper on a thread per workspace, with the agent
/// script chosen per task, and records exits and closes per workspace.
struct TestWorkspace {
    db: PathBuf,
    fail: bool,
    close_fail: bool,
    script: String,
    scripts: Mutex<HashMap<TaskId, String>>,
    exit_timeout: Duration,
    registration_timeout: Duration,
    /// `create` opens the workspace but starts no session, so its wrapper
    /// never registers.
    no_session: bool,
    resume_timeout: Duration,
    /// `send_exit` returns only after the wrapper recorded its exit, as a
    /// slow `cmux send` does when the session exits on the first keystroke.
    exit_returns_after_session: bool,
    prompt_wait: Duration,
    /// What `capture` returns, and how often it was asked.
    screen: Mutex<String>,
    captures: AtomicUsize,
    /// `send_enter` calls: Enters sent again after a submit (task 285).
    enters: AtomicUsize,
    /// This many Enters leave a typed text in the input box (the screen
    /// shows it there until the last one), as a long paste does.
    swallowed_enters: AtomicUsize,
    /// This many texts are typed but never reach the session, as one typed
    /// before Claude Code's input box is drawn.
    dropped_texts: AtomicUsize,
    start_wait: Duration,
    exits_sent: AtomicUsize,
    sessions: Mutex<Vec<(String, TestSession)>>,
    closed: Mutex<Vec<String>>,
    /// `notify` calls as (title, body, workspace); the supervisor sends
    /// none, `ask` one per new ask (ADR-0022).
    notifications: Mutex<Vec<(String, String, Option<String>)>>,
    /// The tags each run workspace was opened with.
    tags: Mutex<Vec<WorkspaceTags>>,
    /// Every `ensure_group` call, as (external ID, name).
    groups: Mutex<Vec<(String, String)>>,
    /// `workspace-group create` fails.
    group_fails: bool,
    /// `send_exit` delivers the request but reports a timeout, the way
    /// `cmux send` does when cmux answers too late under load.
    send_times_out: bool,
    /// Resumed-session script per task; a resume of any other task fails.
    resume_scripts: Mutex<HashMap<TaskId, String>>,
    /// `create_resume` calls: the workspace name and the command.
    resumes: Mutex<Vec<(String, String)>>,
    /// `send_text` calls: the workspace and the text.
    texts: Mutex<Vec<(String, String)>>,
    /// `send_text` records the call and then fails, as a `cmux send` to a
    /// workspace that went away does.
    text_fails: bool,
    /// `exists` fails, as `cmux workspace list` does when cmux is gone.
    exists_fails: bool,
    /// Workspaces cmux lists although this backend did not open them (a
    /// run's workspace from an earlier supervisor), until they are closed.
    listed: Mutex<Vec<String>>,
    /// Workspaces cmux does not list for now although they are open.
    hidden: Mutex<Vec<String>>,
    /// This many captures time out, as `cmux read-screen` does under load.
    capture_timeouts: AtomicUsize,
    /// This many `send_exit` calls time out before the `/exit` reaches the
    /// session (task 354).
    exit_unsent: AtomicUsize,
    /// `close` ends the session in the workspace, as closing a cmux
    /// workspace kills its terminal, instead of requiring it gone.
    close_ends_session: bool,
    /// `close` times out and leaves the workspace and its session as they
    /// are, as cmux does under load.
    close_times_out: bool,
}
impl TestWorkspace {
    fn new(db: &Path, fail: bool, script: &str) -> Self {
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
        }
    }
    /// Let cmux list `workspace` as if an earlier supervisor opened it.
    fn list(&self, workspace: &str) {
        self.listed.lock().unwrap().push(workspace.into());
    }
    /// Resumed-session script for one task.
    fn resume_script_for(&self, task_id: i64, script: &str) {
        self.resume_scripts
            .lock()
            .unwrap()
            .insert(TaskId::new(task_id), script.into());
    }
    fn texts(&self) -> Vec<(String, String)> {
        self.texts.lock().unwrap().clone()
    }
    /// Agent script for one task; other tasks use the default script.
    fn script_for(&self, task_id: i64, script: &str) {
        self.scripts
            .lock()
            .unwrap()
            .insert(TaskId::new(task_id), script.into());
    }
    fn closed(&self) -> Vec<String> {
        self.closed.lock().unwrap().clone()
    }
    /// Wait for every session wrapper started so far to return successfully.
    fn join(&self) {
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
    fn session_run_dir(&self, workspace_id: &str) -> String {
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
fn claude_stub(db: &Path) -> PathBuf {
    let stub = db.parent().unwrap().join("claude-stub");
    fs::write(&stub, "#!/bin/sh\nprintf 'test provider\\n'\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    stub
}

/// The supervisor's pass and idle intervals and the wrapper's wait interval
/// in these tests: short, so a run's steps follow each other without a
/// second's pause.
const TEST_TICK: Duration = Duration::from_millis(50);

/// Supervisor options with the test tick and the [`SteadyClock`].
fn supervise_options(parallel: usize, once: bool) -> SuperviseOptions {
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
struct SteadyClock(SystemTime, Instant);

impl Clock for SteadyClock {
    fn system_time(&self) -> SystemTime {
        self.0 + self.1.elapsed()
    }
}

/// One pass of the parallel supervisor: claim whatever is ready, finish it, exit.
fn supervise(db: &Path, repo: &Path, backend: &TestWorkspace) -> Result<Value> {
    supervise_with(db, repo, backend, &supervise_options(4, true))
}

fn supervise_with(
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
fn other_asks(queue: &mut SqliteQueue, all: bool) -> Vec<dagq::domain::Ask> {
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
fn joined<T>(thread: thread::JoinHandle<T>, what: impl Into<String>) -> T {
    let _waiting = common::within(common::STEP_LIMIT, what);
    thread
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Poll the queue until `condition` holds or the deadline passes.
fn wait_until(db: &Path, timeout: Duration, mut condition: impl FnMut(&mut SqliteQueue) -> bool) {
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
fn run_agent(script: &str) -> (Fixture, PathBuf, dagq::domain::TaskDetail) {
    run_agent_with(script, false)
}

fn run_agent_with(script: &str, close_fail: bool) -> (Fixture, PathBuf, dagq::domain::TaskDetail) {
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
fn read_prompt(run: &TaskRun) -> String {
    fs::read_to_string(Path::new(run.run_dir().unwrap()).join("prompt.txt")).unwrap()
}

fn rejection_reason(detail: &dagq::domain::TaskDetail) -> String {
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

/// While the agent works, a later binary applies a compatible migration
/// (ADR-0045 decision 6): the run's wrapper, the supervisor and the CLI the
/// worker runs are then older than the queue, and carry the run to its
/// receipt anyway.
#[test]
fn a_compatible_migration_during_a_run_leaves_the_run_working() {
    let newer = SqliteQueue::SCHEMA_VERSION + 1;
    let script = format!(
        "sqlite3 -cmd '.timeout 5000' \"$DB\" \\
           'ALTER TABLE task_runs ADD COLUMN future_hint TEXT;
            CREATE TABLE future_things (id INTEGER PRIMARY KEY);
            PRAGMA user_version = {newer};' || exit 97
         \"$DAGQ\" --db \"$DB\" show 1 > /dev/null || exit 98
         {VALID_AGENT}"
    );
    let (_dir, db, detail) = run_agent(&script);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(run.last_error().is_none());
    assert!(
        detail
            .processes
            .iter()
            .all(|p| p.exited_at.is_some() && p.exit_code == Some(0))
    );
    assert_eq!(
        SqliteQueue::open(&db).unwrap().schema_version().unwrap(),
        newer
    );
}

#[test]
fn valid_receipt_is_verified_and_awaits_integration() {
    let (_dir, db, detail) = run_agent(VALID_AGENT);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(run.last_error().is_none());
    let commit = run.result_commit().unwrap();
    assert_ne!(commit, run.base_commit());
    let head = Command::new("git")
        .arg("-C")
        .arg(run.worktree_path().unwrap())
        .args(["rev-parse", "HEAD"])
        .bounded_output()
        .unwrap();
    assert_eq!(String::from_utf8(head.stdout).unwrap().trim(), commit);
    assert_eq!(
        fs::read_to_string(run.log_path().unwrap()).unwrap(),
        "fixture log\n"
    );
    assert!(Path::new(run.run_dir().unwrap()).join("runner").exists());
    assert_eq!(detail.processes.len(), 2);
    assert!(
        detail
            .processes
            .iter()
            .all(|p| p.exited_at.is_some() && p.exit_code == Some(0))
    );
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"agent_started"));
    assert!(kinds.contains(&"receipt_observed"));
    // The only task has no goal, no context, no predecessor, and nothing else was in progress.
    let prompt = read_prompt(run);
    assert!(
        prompt.contains("Goal: none, this task stands alone\n"),
        "{prompt}"
    );
    assert!(prompt.contains("Context: none\n"), "{prompt}");
    assert!(prompt.contains("Predecessor tasks: none\n"), "{prompt}");
    assert!(
        prompt.contains("Sibling tasks in progress: none\n"),
        "{prompt}"
    );
    // Validation checks only the receipt: the verification commands wait
    // for integrate's rebase (ADR-0023 decision 1).
    assert!(!kinds.contains(&"verification_command"), "{kinds:?}");
    assert!(
        !Path::new(run.run_dir().unwrap())
            .join("verify-1.log")
            .exists()
    );
    let finished = detail
        .events
        .iter()
        .find(|e| e.kind == "validation_finished")
        .unwrap();
    assert_eq!(finished.payload["status"], "awaiting_integration");
    assert_eq!(finished.payload["result_commit"], json!(commit));
    assert_eq!(
        finished.payload["receipt"]["e2e"]["status"],
        "not_applicable"
    );
    // The workspace is closed only after validation succeeded; the branch stays.
    let closed = detail
        .events
        .iter()
        .find(|e| e.kind == "workspace_closed")
        .unwrap();
    assert!(closed.id > finished.id);
    assert_eq!(closed.payload["workspace_id"], WORKSPACE_ID);
    assert_eq!(
        closed.payload["closed_at"],
        json!(run.workspace_closed_at().unwrap())
    );
    let branch = Command::new("git")
        .arg("-C")
        .arg(run.worktree_path().unwrap())
        .args(["symbolic-ref", "HEAD"])
        .bounded_output()
        .unwrap();
    assert_eq!(
        String::from_utf8(branch.stdout).unwrap().trim(),
        format!("refs/heads/{}", run.branch().unwrap())
    );
    // The task still owns its slot until integration; no second run starts.
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(queue.show(TaskId::new(1)).unwrap().runs.len(), 1);
    assert!(queue.candidates().unwrap().is_empty());
}

#[test]
fn failed_workspace_close_is_recorded_without_changing_run_status() {
    let (_dir, _db, detail) = run_agent_with(VALID_AGENT, true);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(run.result_commit().is_some());
    let error = run.last_error().unwrap();
    assert!(
        error.contains("injected workspace close failure"),
        "{error}"
    );
    assert!(error.contains(WORKSPACE_ID));
    let failed = detail
        .events
        .iter()
        .find(|e| e.kind == "cleanup_failed")
        .unwrap();
    assert_eq!(failed.payload["workspace_id"], WORKSPACE_ID);
    assert_eq!(failed.payload["message"], json!(error));
    // Validation itself was accepted; the failure is confined to cleanup.
    let finished = detail
        .events
        .iter()
        .find(|e| e.kind == "validation_finished")
        .unwrap();
    assert_eq!(finished.payload["accepted"], true);
    assert!(failed.id > finished.id);
}

#[test]
fn missing_receipt_fails_validation() {
    let (_dir, _db, detail) = run_agent("commit work");
    assert!(rejection_reason(&detail).contains("receipt was not submitted"));
    assert!(detail.runs[0].result_commit().is_none());
    assert!(
        !detail
            .events
            .iter()
            .any(|e| e.kind == "verification_command")
    );
}

#[test]
fn run_id_mismatch_fails_validation() {
    let (_dir, _db, detail) = run_agent("commit work; receipt \"$(git rev-parse HEAD)\" other-run");
    assert!(rejection_reason(&detail).contains("run_id other-run does not match"));
    assert!(detail.runs[0].result_commit().is_none());
}

#[test]
fn receipt_without_new_commit_fails_validation() {
    let (_dir, _db, detail) = run_agent("receipt \"$BASE\"");
    assert!(rejection_reason(&detail).contains("no commit was made"));
}

#[test]
fn receipt_commit_that_is_not_branch_head_fails_validation() {
    let (_dir, _db, detail) = run_agent("commit work; receipt \"$BASE\"");
    assert!(rejection_reason(&detail).contains("is not the head of dagq/"));
}

#[test]
fn dirty_worktree_fails_validation() {
    let (_dir, _db, detail) = run_agent(
        "commit work; printf 'scratch\n' > untracked.txt; receipt \"$(git rev-parse HEAD)\"",
    );
    let reason = rejection_reason(&detail);
    assert!(reason.contains("worktree is not clean"));
    assert!(reason.contains("untracked.txt"));
    // The verified commit is still recorded for inspection.
    assert!(detail.runs[0].result_commit().is_some());
}

/// Validation does not run the verification commands, so a commit that
/// breaks them is accepted there and parked as `needs_session` by
/// integrate, whose run of the commands is the only one (ADR-0023).
#[test]
fn failing_verification_command_passes_validation_and_needs_a_session_at_integrate() {
    let (_dir, db, detail) = run_agent(
        "git rm -q seed.txt && git commit -q -m 'drop seed'; receipt \"$(git rev-parse HEAD)\"",
    );
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(!event_kinds(&detail).contains(&"verification_command"));
    let repo = Path::new(&db).parent().unwrap().join("repo's directory");
    let main = git_out(&repo, &["rev-parse", "main"]);
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let reason = outcome["reason"].as_str().unwrap();
    assert!(
        reason.contains("verification command \"test -f seed.txt\" exited with 1"),
        "{reason}"
    );
    assert!(reason.contains("integrate-1-verify-1.log"), "{reason}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    // The failure is classified, with the command's index and exit code (ADR-0034).
    let deferred = payloads(&detail, "integration_deferred");
    assert_eq!(deferred[0]["code"], "verification_failed");
    assert_eq!(deferred[0]["index"], 1);
    assert_eq!(deferred[0]["exit_code"], 1);
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 1, "{verifications:?}");
    assert_eq!(verifications[0]["exit_code"], 1);
    assert_eq!(verifications[0]["attempt"], 1);
    // Nothing landed.
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main);

    // A second integrate of the same run keeps the first attempt's log: each
    // attempt writes its own `integrate-<attempt>-verify-N.log`.
    let run = &detail.runs[0];
    let run_dir = Path::new(run.run_dir().unwrap());
    let first = run_dir.join("integrate-1-verify-1.log");
    fs::write(&first, "the first attempt's output\n").unwrap();
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    assert!(
        outcome["reason"]
            .as_str()
            .unwrap()
            .contains("integrate-2-verify-1.log"),
        "{outcome}"
    );
    let second = run_dir.join("integrate-2-verify-1.log");
    assert!(second.exists());
    assert_eq!(
        fs::read_to_string(&first).unwrap(),
        "the first attempt's output\n"
    );
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 2, "{verifications:?}");
    assert_eq!(verifications[1]["attempt"], 2);
    assert_eq!(
        verifications[1]["log_path"],
        json!(second.to_str().unwrap())
    );

    // The triage reads the latest attempt's log and names the earlier one.
    let run = &detail.runs[0];
    let prompt = runtime::triage_prompt(&detail, run, 0, run_dir).unwrap();
    assert!(
        prompt.contains(&format!("Verification log {} (end)", second.display())),
        "{prompt}"
    );
    assert!(
        !prompt.contains(&format!("Verification log {} (end)", first.display())),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!(
            "Logs of earlier integrate attempts (not shown): {}",
            first.display()
        )),
        "{prompt}"
    );
    // review.md names the latest attempt's logs.
    let head = run.result_commit().cloned().unwrap();
    write_receipt_json(run, session_receipt(run, head.as_str(), "succeeded", "s"));
    runtime::review(&db, TaskId::new(1)).unwrap();
    let review = fs::read_to_string(run_dir.join("review.md")).unwrap();
    assert!(
        review.contains(&format!("latest attempt: {}", second.display())),
        "{review}"
    );
}

/// Integrate's verification logs are numbered per attempt; a run directory
/// with the name used before (`integrate-verify-N.log`) is still read, as
/// the attempt before the numbered ones.
#[test]
fn integrate_logs_are_kept_per_attempt_and_old_names_are_read() {
    let dir = TempDir::new().unwrap();
    let run_dir = dir.path();
    assert_eq!(runtime::next_integrate_attempt(run_dir), 1);
    assert_eq!(runtime::integrate_logs(run_dir), (vec![], vec![]));
    assert_eq!(
        runtime::review_logs_hint(Some(run_dir.to_str().unwrap())),
        format!(
            "{}/integrate-<attempt>-verify-N.log (one set per integrate attempt, written when integrate runs the verification commands after its rebase); none yet",
            run_dir.display()
        )
    );
    assert_eq!(runtime::review_logs_hint(None), "(no run directory)");
    for name in [
        "integrate-verify-1.log",
        "integrate-verify-2.log",
        "verify-1.log",
        "integrate-x-verify-1.log",
        "integrate-verify-y.log",
        "notes.txt",
    ] {
        fs::write(run_dir.join(name), name).unwrap();
    }
    assert_eq!(
        runtime::integrate_logs(run_dir),
        (
            vec![
                run_dir.join("integrate-verify-1.log"),
                run_dir.join("integrate-verify-2.log")
            ],
            vec![]
        )
    );
    assert_eq!(runtime::next_integrate_attempt(run_dir), 1);
    assert_eq!(
        runtime::integrate_verify_log(run_dir, 1, 2),
        run_dir.join("integrate-1-verify-2.log")
    );
    for name in [
        "integrate-1-verify-1.log",
        "integrate-10-verify-2.log",
        "integrate-10-verify-10.log",
        "integrate-2-verify-1.log",
    ] {
        fs::write(run_dir.join(name), name).unwrap();
    }
    let (latest, earlier) = runtime::integrate_logs(run_dir);
    assert_eq!(
        latest,
        vec![
            run_dir.join("integrate-10-verify-2.log"),
            run_dir.join("integrate-10-verify-10.log")
        ]
    );
    assert_eq!(
        earlier,
        vec![
            run_dir.join("integrate-verify-1.log"),
            run_dir.join("integrate-verify-2.log"),
            run_dir.join("integrate-1-verify-1.log"),
            run_dir.join("integrate-2-verify-1.log")
        ]
    );
    assert_eq!(runtime::next_integrate_attempt(run_dir), 11);
    assert!(
        runtime::review_logs_hint(Some(run_dir.to_str().unwrap())).ends_with(&format!(
            "latest attempt: {}, {}",
            run_dir.join("integrate-10-verify-2.log").display(),
            run_dir.join("integrate-10-verify-10.log").display()
        ))
    );
    assert_eq!(runtime::next_integrate_attempt(&run_dir.join("missing")), 1);
}

#[test]
fn receipt_structure_is_checked_before_git() {
    use dagq::domain::Receipt;
    let valid = r#"{"run_id":"r","result":"succeeded","commit":"0123456789abcdef0123456789abcdef01234567",
        "tests":{"status":"passed","evidence_or_reason":"cargo test"},
        "e2e":{"status":"not_applicable","evidence_or_reason":"library only"},
        "subagent_review":{"status":"passed","evidence_or_reason":"no findings"},"summary":"ok"}"#;
    Receipt::parse(valid)
        .unwrap()
        .check(&RunId::new("r").unwrap())
        .unwrap();
    let cases = [
        (valid.replace("\"r\"", "\"other\""), "does not match"),
        (
            valid.replace("succeeded", "failed"),
            "agent reported result failed",
        ),
        (
            valid.replace("library only", " "),
            "e2e is not_applicable without evidence or reason",
        ),
        (
            valid.replace(
                "\"passed\",\"evidence_or_reason\":\"no findings\"",
                "\"failed\",\"evidence_or_reason\":\"bug\"",
            ),
            "subagent_review as failed: bug",
        ),
        (
            valid.replace("0123456789abcdef0123456789abcdef01234567", "0123456"),
            "receipt commit",
        ),
    ];
    for (text, expected) in cases {
        let error = format!(
            "{:#}",
            Receipt::parse(&text)
                .unwrap()
                .check(&RunId::new("r").unwrap())
                .unwrap_err()
        );
        assert!(error.contains(expected), "{error}");
    }
    assert!(Receipt::parse("{\"run_id\":\"r\"}").is_err());
    assert!(Receipt::parse(&valid.replace("passed", "maybe")).is_err());
    // follow_ups is optional and only its shape is checked.
    let without = Receipt::parse(valid).unwrap();
    assert!(without.follow_ups.is_none());
    assert!(
        !serde_json::to_string(&without)
            .unwrap()
            .contains("follow_ups")
    );
    let with = valid.replace(
        "\"summary\":\"ok\"",
        "\"summary\":\"ok\",\"follow_ups\":[{\"title\":\"next\",\"description\":\"later\"}]",
    );
    let receipt = Receipt::parse(&with).unwrap();
    receipt.check(&RunId::new("r").unwrap()).unwrap();
    assert_eq!(receipt.follow_ups.as_ref().unwrap()[0]["title"], "next");
    assert_eq!(
        serde_json::to_value(&receipt).unwrap()["follow_ups"][0]["description"],
        "later"
    );
    Receipt::parse(&valid.replace("\"summary\":\"ok\"", "\"summary\":\"ok\",\"follow_ups\":[]"))
        .unwrap()
        .check(&RunId::new("r").unwrap())
        .unwrap();
    let error = format!(
        "{:#}",
        Receipt::parse(&valid.replace(
            "\"summary\":\"ok\"",
            "\"summary\":\"ok\",\"follow_ups\":{\"title\":\"next\"}"
        ))
        .unwrap()
        .check(&RunId::new("r").unwrap())
        .unwrap_err()
    );
    assert!(error.contains("follow_ups must be an array"), "{error}");
}

fn event_kinds(detail: &dagq::domain::TaskDetail) -> Vec<&str> {
    detail.events.iter().map(|e| e.kind.as_str()).collect()
}

#[test]
fn idle_marker_after_receipt_triggers_exit_request_and_run_finishes() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_leases().unwrap().is_empty());
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("receipt_observed") < position("session_idle_observed"));
    assert!(position("session_idle_observed") < position("exit_requested"));
    assert!(position("exit_requested") < position("session_exited"));
    assert!(!kinds.contains(&"exit_request_timed_out"));
    // The idle evidence names the hook and session that produced it.
    let idle = detail
        .events
        .iter()
        .find(|e| e.kind == "session_idle_observed")
        .unwrap();
    assert_eq!(idle.payload["hook_event_name"], "Stop");
    assert_eq!(idle.payload["session_id"], json!(run.id()));
    assert_eq!(
        idle.payload["marker_path"],
        json!(run.idle_marker_path().unwrap())
    );
    assert!(
        idle.payload["marker_modified"].as_i64().unwrap()
            >= idle.payload["receipt_modified"].as_i64().unwrap()
    );
    let requested = detail
        .events
        .iter()
        .find(|e| e.kind == "exit_requested")
        .unwrap();
    assert_eq!(requested.payload["workspace_id"], WORKSPACE_ID);
    assert_eq!(requested.payload["timeout_secs"], 120);
}

/// The session exits, and its wrapper records `session_exited`, before
/// `send_exit` returns (a slow `cmux send` under load): `exit_requested` is
/// still recorded first, so the events read in causal order.
#[test]
fn exit_requested_precedes_a_session_exit_that_beats_the_send() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    backend.exit_returns_after_session = true;
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(
        position("exit_requested") < position("session_exited"),
        "{kinds:?}"
    );
}

#[test]
fn missing_or_stale_idle_marker_does_not_request_exit() {
    // No marker at all, then a marker older than the receipt (an earlier turn).
    // Both sessions end by themselves, as with a person's /exit.
    for script in [
        "commit work; receipt \"$(git rev-parse HEAD)\"; sleep 1",
        "idle; touch -t 200001010000 \"$IDLE\"; commit work; receipt \"$(git rev-parse HEAD)\"; sleep 1",
    ] {
        let (_dir, repo, db) = fixture();
        let backend = TestWorkspace::new(&db, false, script);
        let outcome = supervise(&db, &repo, &backend).unwrap();
        backend.join();
        assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
        assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
        let mut queue = SqliteQueue::open(&db).unwrap();
        let detail = queue.show(TaskId::new(1)).unwrap();
        let kinds = event_kinds(&detail);
        assert!(kinds.contains(&"receipt_observed"));
        assert!(!kinds.contains(&"session_idle_observed"));
        assert!(!kinds.contains(&"exit_requested"));
    }
}

/// Fake agent that ignores the supervisor's `/exit` (as when a dialog holds
/// it back) and ends only once the test writes `$EXIT.held`, the way a person
/// would answer the dialog and exit.
/// Blocks a fake session until the test calls `release_held_session`.
const HOLD: &str = "while [ ! -f \"$EXIT.held\" ]; do sleep 0.05; done";
const HELD_AGENT: &str = "commit work; receipt \"$(git rev-parse HEAD)\"; idle; while [ ! -f \"$EXIT.held\" ]; do sleep 0.05; done";

fn release_held_session(run_dir: &str) {
    fs::write(Path::new(run_dir).join("exit-requested.held"), "").unwrap();
}

/// A dialog screen as Claude Code draws it, and a screen of ordinary work.
const DIALOG_SCREEN: &str = "\
 Auto mode is available

 ❯ 1. Yes, turn on auto mode
   2. No, keep asking

 Esc to cancel
";
const WORK_SCREEN: &str = "⏺ Bash(cargo test)\n  ⎿  test result: ok\n\n│ ❯ \n  ? for shortcuts\n";

/// Fake agent that works (no receipt, no idle marker) until the test writes
/// `$EXIT.go`, then finishes like `VALID_AGENT` and waits for `/exit`.
const PROMPTED_AGENT: &str = "while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit";

/// Commits once, waits for `$EXIT.go` before a second commit and the receipt.
const TWO_COMMIT_AGENT: &str = "commit first; while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; printf 'more\\n' >> change.txt; git commit -q -am second; receipt \"$(git rev-parse HEAD)\"";

/// The supervisor records `first_commit_observed` once, while the session
/// still works, when the worktree's HEAD first leaves the base commit; a
/// later commit records nothing more. It sits between `agent_started` and
/// `receipt_observed`, which is what `stats` reads as `startup`.
#[test]
fn the_first_commit_is_observed_once_while_the_session_works() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, TWO_COMMIT_AGENT));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    let observed = |queue: &mut SqliteQueue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap())
            .iter()
            .filter(|k| **k == "first_commit_observed")
            .count()
    };
    wait_until(&db, Duration::from_secs(30), |queue| observed(queue) == 1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let first = git_out(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(first, *run.base_commit());
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(!event_kinds(&detail).contains(&"receipt_observed"));

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");

    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(observed(&mut queue), 1, "{kinds:?}");
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("agent_started") < position("first_commit_observed"));
    assert!(position("first_commit_observed") < position("receipt_observed"));
    let payload = events_of(&db, run.id(), "first_commit_observed").remove(0);
    assert_eq!(payload["commit"], first.as_str());
    assert_eq!(payload["base_commit"], run.base_commit().as_str());
    let head = queue.show(TaskId::new(1)).unwrap().runs[0]
        .result_commit()
        .cloned()
        .unwrap();
    assert_ne!(head, first, "the second commit is the result");
}

/// A session that runs past `prompt_wait` has its screen read: an ordinary
/// screen records nothing, a dialog is recorded as `prompt_waiting` once and
/// raised to the inbox as an `answer_prompt` ask with the screen's excerpt
/// (ADR-0024's Consequences), and the screen going back to work records
/// `prompt_cleared` and closes the ask. No key is sent.
#[test]
fn a_dialog_on_the_screen_is_asked_once_and_cleared() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, PROMPTED_AGENT);
    backend.prompt_wait = Duration::from_millis(300);
    *backend.screen.lock().unwrap() = WORK_SCREEN.into();
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    let prompts = |queue: &mut SqliteQueue, kind: &str| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap())
            .iter()
            .filter(|k| **k == kind)
            .count()
    };
    // Ordinary work is read but not recorded.
    let started = Instant::now();
    while backend.captures.load(Ordering::SeqCst) < 2 {
        assert!(started.elapsed() < Duration::from_secs(30));
        thread::sleep(Duration::from_millis(20));
    }
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(prompts(&mut queue, "prompt_waiting"), 0);
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert!(run_attention_of(&runtime::status(&db).unwrap(), run.id()).is_none());

    *backend.screen.lock().unwrap() = DIALOG_SCREEN.into();
    wait_until(&db, Duration::from_secs(30), |queue| {
        prompts(queue, "prompt_waiting") == 1
    });
    // The same screen is read again but not recorded again.
    let captured = backend.captures.load(Ordering::SeqCst);
    let started = Instant::now();
    while backend.captures.load(Ordering::SeqCst) < captured + 2 {
        assert!(started.elapsed() < Duration::from_secs(30));
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(prompts(&mut queue, "prompt_waiting"), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let waiting = detail
        .events
        .iter()
        .find(|e| e.kind == "prompt_waiting")
        .unwrap();
    assert_eq!(waiting.payload["workspace_id"], WORKSPACE_ID);
    assert_eq!(waiting.payload["prompt"], "choice");
    assert_eq!(
        waiting.payload["excerpt"],
        "Auto mode is available\n ❯ 1. Yes, turn on auto mode\n   2. No, keep asking\n Esc to cancel"
    );
    assert_eq!(waiting.payload["screen_hash"].as_str().unwrap().len(), 64);
    // The dialog is an ask for the inbox, not an attention of the run.
    let asks = queue
        .asks(dagq::infrastructure::asks::AskQuery {
            open: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = &asks[0];
    assert_eq!(ask.kind, dagq::domain::AskKind::AnswerPrompt);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.task_id, Some(run.task_id()));
    assert_eq!(ask.asked_by, "supervisor");
    assert!(ask.options.is_empty());
    assert!(
        ask.question.contains(&format!(
            "waits at a choice dialog in workspace {WORKSPACE_ID}"
        )),
        "{}",
        ask.question
    );
    assert!(
        ask.question
            .ends_with("Auto mode is available\n ❯ 1. Yes, turn on auto mode\n   2. No, keep asking\n Esc to cancel"),
        "{}",
        ask.question
    );
    let status = runtime::status_for(&db, Some(dagq::domain::SessionRole::Inbox)).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "ask_opened" && a["ask_id"] == ask.id.as_i64())
    );
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["kind"] != "prompt_waiting")
    );
    // The same dialog is not asked about twice.
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);

    // Someone answers the dialog: the screen goes back to work.
    *backend.screen.lock().unwrap() = WORK_SCREEN.into();
    wait_until(&db, Duration::from_secs(30), |queue| {
        prompts(queue, "prompt_cleared") == 1
    });
    assert!(run_attention_of(&runtime::status(&db).unwrap(), run.id()).is_none());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    // The runtime closed the ask: it is no attention, and its answer says why.
    let closed = queue.read_ask(ask.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(
        closed.answer.as_deref(),
        Some("the dialog is gone; closed by the runtime")
    );
    assert!(
        runtime::status(&db).unwrap()["attention"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a.get("ask_id").is_none())
    );

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(
        event_kinds(&detail)
            .iter()
            .filter(|k| k.starts_with("prompt_"))
            .count(),
        2
    );
}

/// A session stopped at a login that ran out (task 266).
const LOGIN_SCREEN: &str = "\
⏺ Bash(cargo test)
  ⎿  API Error: 401 {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"OAuth token has expired.\"}} · Please run /login

│ ❯ 
  ? for shortcuts
";

/// Two sessions that stop at the same login that ran out are one
/// `authentication` ask for the inbox (ADR-0047 decision 42): the first
/// opens it with one notification, the second joins its `affected`, each
/// records `auth_required`, and neither is an `answer_prompt` ask. `status`
/// and `watch` show the reason.
#[test]
fn sessions_stopped_at_the_same_login_share_one_authentication_ask() {
    let (_dir, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "second task", &[]);
    }
    let mut backend = TestWorkspace::new(&db, false, PROMPTED_AGENT);
    backend.prompt_wait = Duration::from_millis(300);
    *backend.screen.lock().unwrap() = LOGIN_SCREEN.into();
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    let auth_events = |queue: &mut SqliteQueue| {
        [TaskId::new(1), TaskId::new(2)]
            .iter()
            .map(|id| {
                event_kinds(&queue.show(*id).unwrap())
                    .iter()
                    .filter(|k| **k == "auth_required")
                    .count()
            })
            .collect::<Vec<_>>()
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        auth_events(queue) == [1, 1]
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let open = queue
        .asks(dagq::infrastructure::asks::AskQuery {
            open: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    let ask = &open[0];
    assert_eq!(ask.kind, dagq::domain::AskKind::QueueHold);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::Authentication);
    assert_eq!((ask.task_id, ask.run_id.as_ref()), (None, None));
    let runs: Vec<String> = [TaskId::new(1), TaskId::new(2)]
        .iter()
        .map(|id| queue.show(*id).unwrap().runs[0].id().as_str().to_owned())
        .collect();
    let mut affected = ask.affected.clone();
    affected.sort();
    let mut expected = runs.clone();
    expected.sort();
    assert_eq!(affected, expected);
    for run in &runs {
        assert!(ask.question.contains(run.as_str()), "{}", ask.question);
    }
    assert_eq!(ask.options, ["done", "cancel_affected"]);
    // One notification for both runs.
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);
    let status = runtime::status_for(&db, Some(dagq::domain::SessionRole::Inbox)).unwrap();
    let entry = status["asks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == ask.id.as_i64())
        .unwrap()
        .clone();
    assert_eq!(entry["reason_category"], "authentication");
    assert_eq!(entry["affected"].as_array().unwrap().len(), 2);
    let attention = ask_attention(&status, ask.id);
    assert_eq!(attention.len(), 1, "{status}");
    assert_eq!(attention[0]["reason_category"], "authentication");
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    let opened: Vec<&Value> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "ask_opened")
        .collect();
    assert_eq!(opened.len(), 1, "{events}");
    assert_eq!(opened[0]["reason_category"], "authentication");
    // A login is no dialog to answer.
    for id in [TaskId::new(1), TaskId::new(2)] {
        assert!(
            !event_kinds(&queue.show(id).unwrap()).contains(&"prompt_waiting"),
            "{id}"
        );
    }

    // The person logs in; the sessions go back to work and finish.
    *backend.screen.lock().unwrap() = WORK_SCREEN.into();
    let answered = queue.answer(ask.id, "done").unwrap();
    assert_eq!(
        answered.reason_category,
        dagq::domain::AskReason::Authentication
    );
    for id in [TaskId::new(1), TaskId::new(2)] {
        let run = queue.show(id).unwrap().runs[0].clone();
        fs::write(
            exit_request_path(run.run_dir().unwrap()).with_extension("go"),
            "",
        )
        .unwrap();
    }
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(auth_events(&mut queue), [1, 1]);
}

/// A worker that registers a `worker_question` ask, goes idle once
/// `$EXIT.idle` exists and then waits for the answer in `$MESSAGE` (the
/// test backend's terminal); it commits the answer it got.
const ASKING_AGENT: &str = r#"
"$DAGQ" --db "$DB" ask --run "$RUN_ID" --kind worker_question --because scope --question 'Which word?' --cmux /usr/bin/true > /dev/null || exit 70
while [ ! -f "$EXIT.idle" ]; do sleep 0.05; done
idle
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
cp "$MESSAGE" answer.txt
git add answer.txt
git commit -q -m answer
receipt "$(git rev-parse HEAD)"
idle
await_exit
"#;

/// The attention entries of `status` for one ask.
fn ask_attention(status: &Value, ask_id: AskId) -> Vec<Value> {
    status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["ask_id"] == ask_id.as_i64())
        .cloned()
        .collect()
}

/// A worker's `dagq ask` shows in `status` as an open `worker_question`;
/// while it is unclosed the screen is not read for a dialog. Its answer is
/// not typed until the worker went idle after asking, then it is typed once
/// into the worker's terminal as `answer to ask <id>: ...`, the ask is
/// closed, and `ask_delivered` is recorded.
#[test]
fn an_answered_worker_question_is_typed_into_the_idle_worker_and_closed() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, ASKING_AGENT);
    backend.prompt_wait = Duration::from_millis(300);
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(Default::default()).unwrap().is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue.asks(Default::default()).unwrap().remove(0);
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(ask.kind.as_str(), "worker_question");
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["asks"][0]["id"], ask.id.as_i64(), "{status}");
    assert_eq!(
        ask_attention(&status, ask.id)[0]["next"],
        format!("answer ask {}", ask.id)
    );

    // A dialog-like screen while the ask is unclosed is not read or recorded.
    *backend.screen.lock().unwrap() = DIALOG_SCREEN.into();
    // A poll that looked for the ask just before it was registered is over.
    thread::sleep(Duration::from_millis(200));
    let captured = backend.captures.load(Ordering::SeqCst);
    // Well past `prompt_wait`, when the screen would otherwise be read.
    thread::sleep(Duration::from_millis(1000));
    assert_eq!(backend.captures.load(Ordering::SeqCst), captured);
    let status = runtime::status(&db).unwrap();
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["kind"] != "prompt_waiting"),
        "{status}"
    );

    // Answered while the worker has not gone idle since asking: not typed.
    queue.answer(ask.id, "use blue").unwrap();
    thread::sleep(Duration::from_millis(500));
    assert!(backend.texts().is_empty());
    let status = runtime::status(&db).unwrap();
    let attention = ask_attention(&status, ask.id);
    assert_eq!(attention.len(), 1, "{status}");
    assert_eq!(attention[0]["kind"], "ask_answered");
    assert_eq!(
        attention[0]["next"],
        format!("delivering the answer of ask {} (runtime)", ask.id)
    );
    // The answer of a worker_question does not wake the inbox.
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["kind"] != "ask_answered"),
        "{events}"
    );

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("idle"),
        "",
    )
    .unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue.read_ask(ask.id).unwrap().closed_at.is_some()
    });
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    // Typed once, with its prefix.
    assert_eq!(
        backend.texts(),
        vec![(
            WORKSPACE_ID.to_owned(),
            format!("answer to ask {}: use blue", ask.id)
        )]
    );
    let worktree = Path::new(run.worktree_path().unwrap());
    assert_eq!(
        fs::read_to_string(worktree.join("answer.txt")).unwrap(),
        format!("answer to ask {}: use blue", ask.id)
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    let delivered = payloads(&detail, "ask_delivered");
    assert_eq!(
        delivered,
        vec![&json!({"ask_id": ask.id, "workspace_id": WORKSPACE_ID})]
    );
    assert!(payloads(&detail, "prompt_waiting").is_empty());
    assert!(ask_attention(&runtime::status(&db).unwrap(), ask.id).is_empty());
}

/// A send that fails is not retried: `ask_delivery_failed` is recorded
/// once, the ask stays unclosed and surfaces for the inbox to deliver.
#[test]
fn a_failed_answer_delivery_is_left_to_the_inbox() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "\"$DAGQ\" --db \"$DB\" ask --run \"$RUN_ID\" --kind worker_question --because scope --question 'Which?' --cmux /usr/bin/true >/dev/null; idle; {HOLD}; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit"
        ),
    );
    backend.text_fails = true;
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(Default::default()).unwrap().is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue.asks(Default::default()).unwrap().remove(0);
    queue.answer(ask.id, "blue").unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        !payloads(&queue.show(TaskId::new(1)).unwrap(), "ask_delivery_failed").is_empty()
    });
    // Several passes later the send was not retried.
    thread::sleep(Duration::from_millis(500));
    assert_eq!(backend.texts().len(), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let failed = payloads(&detail, "ask_delivery_failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["ask_id"], ask.id.as_i64());
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("injected cmux send failure")
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_none());
    let status = runtime::status(&db).unwrap();
    let attention = ask_attention(&status, ask.id);
    assert_eq!(attention.len(), 1, "{status}");
    assert_eq!(attention[0]["kind"], "ask_delivery_failed");
    assert_eq!(
        attention[0]["next"],
        format!(
            "send the answer of ask {} to the worker and close it",
            ask.id
        )
    );
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "ask_delivery_failed"),
        "{events}"
    );

    let run = detail.runs[0].clone();
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.texts().len(), 1);
    // The run is at rest: the inbox delivers by hand, then closes it.
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        ask_attention(&status, ask.id)[0]["next"],
        format!(
            "send the answer of ask {} to the worker and close it",
            ask.id
        )
    );
    queue.close_ask(ask.id).unwrap();
    assert!(ask_attention(&runtime::status(&db).unwrap(), ask.id).is_empty());

    // Answered after the run stopped running: nobody types it, so its
    // `ask_answered` wakes the inbox.
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let late = queue
        .ask(dagq::domain::NewAsk {
            kind: "worker_question".parse().unwrap(),
            task_id: None,
            run_id: Some(run.id().clone()),
            question: "Late?".into(),
            options: vec![],
            asked_by: "worker".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
        })
        .unwrap()
        .ask;
    queue.answer(late.id, "yes").unwrap();
    let events = dagq::watch::events(&db, EventId::new(cursor), 100, false).unwrap();
    let answered: Vec<&Value> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "ask_answered")
        .collect();
    assert_eq!(answered.len(), 1, "{events}");
    assert_eq!(
        answered[0]["next"],
        format!(
            "send the answer of ask {} to the worker and close it",
            late.id
        )
    );
}

/// An unanswered `/exit` is recorded once and raised as one `stuck_exit` ask
/// to the inbox, notified once through the ask path, but the supervisor
/// keeps the lease and keeps watching: when the session ends later, the ask
/// is closed by the runtime and the run moves on as the verdict said. The
/// `/exit` comes after the validation and the review (ADR-0027; here a
/// failed review, the stand-in `claude` printing no verdict), so the run is
/// already `awaiting_integration` and the question says what follows.
#[test]
fn unanswered_exit_request_times_out_and_keeps_the_run() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, HELD_AGENT);
    backend.exit_timeout = Duration::from_secs(1);
    let screen = (1..=20)
        .map(|n| format!("line {n}"))
        .chain(["❯ 1. Exit anyway".into(), "  2. Cancel".into()])
        .collect::<Vec<_>>()
        .join("\n");
    *backend.screen.lock().unwrap() = screen;
    let backend = Arc::new(backend);
    SqliteQueue::open(&db)
        .unwrap()
        .register_session_workspace(SessionRole::Inbox, "inbox-ws")
        .unwrap();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"exit_request_timed_out")
    });
    // Let a few more polls pass: the timeout is not recorded again and the
    // run is not given up.
    thread::sleep(Duration::from_millis(500));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(run.last_error().is_none());
    assert!(queue.run_lease(run.id()).unwrap().is_some());
    let kinds = event_kinds(&detail);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "exit_request_timed_out")
            .count(),
        1
    );
    assert!(!kinds.contains(&"runtime_error"));
    assert!(!kinds.contains(&"session_exited"));
    // Nothing after the verdict happens until the session exits.
    assert!(!kinds.contains(&"review_failed"));
    let timed_out = detail
        .events
        .iter()
        .find(|e| e.kind == "exit_request_timed_out")
        .unwrap();
    assert_eq!(
        timed_out.payload,
        json!({"code": "exit_timeout", "workspace_id": WORKSPACE_ID, "timeout_secs": 1})
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    // One stuck_exit ask by the supervisor, with the screen's last 15 lines.
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = &asks[0];
    assert_eq!(ask.kind, AskKind::StuckExit);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.task_id, Some(TaskId::new(1)));
    assert_eq!(ask.asked_by, "supervisor");
    assert_eq!(ask.options, ["exit", "wait"]);
    assert!(ask.is_open());
    assert!(ask.question.contains(run.id().as_str()), "{}", ask.question);
    assert!(ask.question.contains("task 1"), "{}", ask.question);
    assert!(ask.question.contains(WORKSPACE_ID), "{}", ask.question);
    assert!(ask.question.contains("line 8\n"), "{}", ask.question);
    assert!(!ask.question.contains("line 7\n"), "{}", ask.question);
    assert!(ask.question.ends_with("  2. Cancel"), "{}", ask.question);
    assert!(
        ask.question.contains(
            "The run stays awaiting_integration under the supervisor after its validation and review, and opens an approve_landing ask for the person about its failed review once the session exits"
        ),
        "{}",
        ask.question
    );
    assert!(!ask.question.contains("stays running"), "{}", ask.question);
    assert_eq!(
        kinds.iter().filter(|k| **k == "ask_opened").count(),
        1,
        "{kinds:?}"
    );
    // Notified once, to the inbox, by the ask; no run transition notifies.
    {
        let notifications = backend.notifications.lock().unwrap();
        assert_eq!(notifications.len(), 1, "{notifications:?}");
        assert!(
            notifications[0]
                .0
                .ends_with(&format!("ask #{} stuck_exit", ask.id)),
            "{notifications:?}"
        );
        assert!(
            notifications[0]
                .1
                .ends_with(&format!("task 1 run {}", run.id()))
        );
        assert_eq!(notifications[0].2.as_deref(), Some("inbox-ws"));
    }
    // The ask is the attention, for the inbox; nobody is told to send
    // /exit, and recovery is refused while the supervisor holds the lease.
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    let attention = status["attention"].as_array().unwrap();
    assert!(
        attention.iter().all(|a| a["next"] != "send /exit"),
        "{status}"
    );
    assert!(
        attention
            .iter()
            .any(|a| a["kind"] == "ask_opened" && a["next"] == format!("answer ask {}", ask.id)),
        "{status}"
    );
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    let events = events["events"].as_array().unwrap();
    assert!(events.iter().all(|e| e["next"] != "send /exit"));
    assert!(runtime::recover(&db, run.id()).is_err());

    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_leases().unwrap().is_empty());
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("validation_finished") < position("exit_requested"));
    assert!(position("exit_request_timed_out") < position("session_exited"));
    assert!(position("session_exited") < position("workspace_closed"));
    assert!(position("workspace_closed") < position("review_failed"));
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "exit_request_timed_out")
            .count(),
        1
    );
    assert!(!kinds.contains(&"runtime_error"));
    // The runtime closed the ask when the session exited; the closing
    // answer is no attention, and nothing else was notified.
    let closed = queue.read_ask(ask.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(
        closed.answer.as_deref(),
        Some("the session exited; closed by the runtime")
    );
    assert!(position("session_exited") < position("ask_answered"));
    assert!(position("ask_answered") < position("workspace_closed"));
    assert!(position("ask_answered") < position("review_failed"));
    // The only open ask is the one of the failed review, the only other
    // notification.
    let open = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].kind, AskKind::ApproveLanding);
    assert!(position("review_retried") < position("review_failed"));
    assert_eq!(backend.notifications.lock().unwrap().len(), 2);
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["kind"] != "ask_answered"),
        "{events}"
    );
    // The session exited, so the attention is the ask of the failed review.
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["ask_id"] == json!(open[0].id)),
        "{status}"
    );
}

#[test]
fn claude_stop_hook_settings_publish_the_idle_marker() {
    use dagq::infrastructure::adapters::{ClaudeCode, stop_hook_settings};
    let dir = tempfile::tempdir().unwrap();
    let run_dir = dir.path().join("run's dir");
    fs::create_dir(&run_dir).unwrap();
    let run = TaskRun::restore(dagq::domain::RunRecord {
        id: RunId::new("11111111-2222-4333-8444-555555555555").unwrap(),
        task_id: TaskId::new(1),
        status: RunStatus::Starting,
        requested_provider: dagq::domain::Provider::Claude,
        actual_provider: dagq::domain::Provider::Claude,
        base_commit: sha("0123456789abcdef0123456789abcdef01234567"),
        branch: Some("dagq/x".into()),
        worktree_path: Some(dir.path().to_str().unwrap().into()),
        workspace_id: None,
        receipt_path: Some(run_dir.join("receipt.json").to_str().unwrap().into()),
        log_path: Some(run_dir.join("claude.debug.log").to_str().unwrap().into()),
        result_commit: None,
        repo_path: None,
        run_dir: Some(run_dir.to_str().unwrap().into()),
        last_error: None,
        workspace_closed_at: None,
        created_at: String::new(),
    })
    .unwrap();
    let command = ClaudeCode {
        executable: "claude".into(),
    }
    .command(&run, "prompt")
    .unwrap();
    let args: Vec<String> = command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let settings = run_dir.join("claude-settings.json");
    assert!(args.contains(&"--settings".to_string()));
    assert!(args.contains(&settings.to_str().unwrap().to_string()));
    let text = fs::read_to_string(&settings).unwrap();
    assert_eq!(
        text,
        stop_hook_settings(&run.idle_marker_path().unwrap()).unwrap()
    );
    let parsed: Value = serde_json::from_str(&text).unwrap();
    // A non-empty auto mode environment from flag settings keeps the
    // "Teach auto mode" dialog away; `$defaults` keeps the built-in entries.
    assert_eq!(parsed["autoMode"]["environment"], json!(["$defaults"]));
    let hook = &parsed["hooks"]["Stop"][0]["hooks"][0];
    assert_eq!(hook["type"], "command");
    // Run the hook exactly as Claude would: shell command, event JSON on stdin.
    let payload =
        r#"{"session_id":"11111111-2222-4333-8444-555555555555","hook_event_name":"Stop"}"#;
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(hook["command"].as_str().unwrap())
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            let _waiting = common::within(common::STEP_LIMIT, "the Stop hook to exit");
            child.stdin.take().unwrap().write_all(payload.as_bytes())?;
            child.wait()
        })
        .unwrap();
    assert!(status.success());
    assert_eq!(
        fs::read_to_string(run.idle_marker_path().unwrap()).unwrap(),
        payload
    );
    assert!(!run_dir.join("idle.json.tmp").exists());
    // Each marker is also appended, with the time, to the log next to it.
    let log = fs::read_to_string(run_dir.join("idle.log")).unwrap();
    let (secs, marker) = log.strip_suffix('\n').unwrap().split_once('\t').unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(now.abs_diff(secs.parse().unwrap()) < 60, "{log}");
    assert_eq!(marker, payload);
}

#[test]
fn failed_agent_retains_worktree_and_does_not_complete_task() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; exit 7",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "failed");
    // A nonzero session exit is final; the receipt is not validated and the
    // workspace stays open for inspection.
    assert_eq!(outcome["runs"][0]["result_commit"], Value::Null);
    assert_eq!(outcome["runs"][0]["workspace_closed_at"], Value::Null);
    assert_eq!(
        outcome["runs"][0]["last_error"],
        "session exited with code 7"
    );
    assert!(backend.closed().is_empty());
    // A failed run is reported through `watch`, not a notification (ADR-0022).
    assert!(backend.notifications.lock().unwrap().is_empty());
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(
        detail.runs[0].last_error(),
        Some("session exited with code 7")
    );
    assert!(Path::new(detail.runs[0].worktree_path().unwrap()).exists());
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(queue.candidates().unwrap().is_empty());
    // A failed run does not free the task automatically, but a person may give up on it.
    assert_eq!(runtime::doctor(&db, true).unwrap()["runs"], json!([]));
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Canceled
    );
}

#[test]
fn provisioning_failure_retains_the_run_and_stops_claiming_other_tasks() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "untouched", &[]);
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    let error = format!("{:#}", supervise(&db, &repo, &backend).unwrap_err());
    assert!(error.contains("injected workspace"), "{error}");
    assert!(error.contains("claiming stopped"), "{error}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Starting);
    assert!(run.last_error().unwrap().contains("injected workspace"));
    assert!(Path::new(run.worktree_path().unwrap()).exists());
    // The environment is suspect: the second candidate was left alone.
    assert!(queue.show(TaskId::new(2)).unwrap().runs.is_empty());
    assert_eq!(queue.candidates().unwrap()[0].id(), TaskId::new(2));
    // The run is disowned, so nothing has to be stopped before recovering it;
    // the drained loop took its registration with it.
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(queue.supervisors().unwrap().is_empty());
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["supervisors"], json!([]));
    assert_eq!(report["runs"][0]["recoverable"], true);
    assert_eq!(
        runtime::recover(&db, run.id()).unwrap()["run"]["status"],
        "interrupted"
    );
    assert_eq!(queue.show(TaskId::new(1)).unwrap().runs.len(), 1);
}

/// The `backend_call_failed` events of a task, oldest first.
fn backend_failures(detail: &dagq::domain::TaskDetail) -> Vec<&dagq::domain::RunEvent> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == "backend_call_failed")
        .collect()
}

/// A failed backend call carries the call, the error and the load it
/// failed under: the load average (or null), the supervisor's slots held
/// and its `--parallel` (task 109).
fn assert_backend_failure(
    event: &dagq::domain::RunEvent,
    op: &str,
    workspace: Option<&str>,
    error: &str,
    run_id: &RunId,
) {
    assert_eq!(event.run_id.as_ref(), Some(run_id));
    assert_eq!(event.payload["op"], op, "{:?}", event.payload);
    assert_eq!(event.payload["workspace_id"], json!(workspace));
    assert_eq!(event.payload["timeout_secs"], 30);
    assert!(
        event.payload["error"].as_str().unwrap().contains(error),
        "{:?}",
        event.payload
    );
    assert!(event.payload["load_avg"].is_f64() || event.payload["load_avg"].is_null());
    assert_eq!(event.payload["slots"], 1);
    assert_eq!(event.payload["parallel"], 4);
}

/// cmux failing to create, close or send is recorded as
/// `backend_call_failed` on the run, next to (and before) what the
/// supervisor already recorded for it: the abandon's `runtime_error`, and
/// `cleanup_failed`; `stats` counts them and raises `backend_failures`.
#[test]
fn failed_backend_calls_are_recorded_with_the_load_and_counted_by_stats() {
    // create: the provisioning failure abandons the run.
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap_err();
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let run = &detail.runs[0];
    let failures = backend_failures(&detail);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_backend_failure(
        failures[0],
        "create",
        None,
        "injected workspace creation failure",
        run.id(),
    );
    let abandoned = detail
        .events
        .iter()
        .find(|e| e.kind == "runtime_error")
        .unwrap();
    assert!(failures[0].id < abandoned.id);
    // The abandon carries the backend call's code and op (ADR-0034).
    assert_eq!(failures[0].payload["code"], "backend_failed");
    assert_eq!(abandoned.payload["code"], "backend_failed");
    assert_eq!(abandoned.payload["op"], "create");

    // close: `cleanup_failed` stays as it was, and the failure is recorded too.
    let (_dir, db, detail) = run_agent_with(VALID_AGENT, true);
    let run = &detail.runs[0];
    let failures = backend_failures(&detail);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_backend_failure(
        failures[0],
        "close",
        Some(WORKSPACE_ID),
        "injected workspace close failure",
        run.id(),
    );
    let cleanup = detail
        .events
        .iter()
        .find(|e| e.kind == "cleanup_failed")
        .unwrap();
    assert_eq!(
        cleanup
            .payload
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["code", "message", "op", "workspace_id"]
    );
    assert_eq!(cleanup.payload["code"], "backend_failed");
    assert_eq!(cleanup.payload["op"], "close");
    assert!(failures[0].id < cleanup.id);
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["backend_failures"]["count"], 1, "{stats}");
    assert!(
        !stats["alerts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "backend_failures")
    );

    // send: the /exit that timed out is not typed again, and the run goes
    // on, its screen read for whether the /exit got there (task 326).
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    backend.send_times_out = true;
    let cursor = runtime::status(&db).unwrap()["cursor"].as_i64().unwrap();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let failures = backend_failures(&detail);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_backend_failure(
        failures[0],
        "send_exit",
        Some(WORKSPACE_ID),
        "did not finish within 30s",
        run.id(),
    );
    // cmux's timeout is told apart from its other failures.
    assert_eq!(failures[0].payload["code"], "backend_timeout");
    // One of up to three attempts, not made again: the screen shows the
    // /exit got there (task 354).
    assert_eq!(failures[0].payload["attempt"], 1);
    assert_eq!(failures[0].payload["max_attempts"], 3);
    assert_eq!(failures[0].payload["retry_after_ms"], Value::Null);
    assert!(!detail.events.iter().any(|e| e.kind == "runtime_error"));

    // capture: a timeout is read again after a backoff, each failed
    // attempt recorded with its number and the backoff that followed.
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    backend.capture_timeouts.store(2, Ordering::SeqCst);
    let recording = runtime::RecordingBackend::new(&backend, db.clone(), None);
    assert_eq!(recording.capture(WORKSPACE_ID).unwrap(), READY_SCREEN);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let retried: Vec<_> = backend_failures(&detail)
        .into_iter()
        .filter(|e| e.payload["op"] == "capture")
        .map(|e| {
            (
                e.payload["attempt"].clone(),
                e.payload["max_attempts"].clone(),
                e.payload["retry_after_ms"].clone(),
                e.payload["code"].clone(),
            )
        })
        .collect();
    assert_eq!(
        retried,
        [
            (json!(1), json!(3), json!(10), json!("backend_timeout")),
            (json!(2), json!(3), json!(20), json!("backend_timeout")),
        ]
    );
    // Recorded on the run whose workspace it read.
    assert_eq!(retried.len(), 2);

    // A second failure in the same window is an alert.
    backend.exists_fails = true;
    let recording = runtime::RecordingBackend::new(&backend, db.clone(), None);
    assert!(recording.exists(WORKSPACE_ID).is_err());
    let stats = runtime::stats(
        &db,
        &dagq::domain::stats::StatsQuery {
            since: Some(EventId::new(cursor)),
            ..Default::default()
        },
    )
    .unwrap();
    let failures = &stats["backend_failures"];
    assert_eq!(failures["count"], 4, "{stats}");
    assert_eq!(
        failures["by_op"],
        json!({"capture": 2, "exists": 1, "send_exit": 1})
    );
    // The codes of the window, per code and per kind.
    let codes = &stats["reason_codes"];
    // `backend_call_failed` is `backend_failures`' to count, not again here.
    assert_eq!(codes["by_kind"].get("backend_call_failed"), None, "{stats}");
    assert_eq!(failures["max_slots"], 1);
    assert!(failures["max_load_avg"].is_f64() || failures["max_load_avg"].is_null());
    assert!(stats["alerts"].as_array().unwrap().contains(&json!({
        "kind": "backend_failures", "task_id": null, "run_id": null,
        "value": 4, "threshold": 2
    })));
}

/// A workspace group cmux cannot make leaves a warning in the supervisor
/// log, and the run opens outside any group (ADR-0026).
#[test]
fn a_workspace_group_cmux_cannot_make_is_a_logged_warning() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "grouped", &[]);
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.group_fails = true;
    let options = supervise_options(1, true);
    let (telemetry, captured) = Telemetry::capture();
    let outcome = telemetry
        .in_scope(|| supervise_with(&db, &repo, &backend, &options))
        .unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().runs[0].status(),
        RunStatus::AwaitingIntegration
    );
    assert_eq!(backend.tags.lock().unwrap()[0].group, None);
    let log = captured.text();
    assert!(
        log.contains("warning: cmux workspace group")
            && log.contains("workspace-group create failed")
            && log.contains("\"level\":\"WARN\""),
        "{log}"
    );
    // The group belongs to no run, so its failure is recorded without one.
    let failures: Vec<_> = queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "backend_call_failed")
        .collect();
    // One per run workspace opened.
    assert_eq!(failures.len(), backend.groups.lock().unwrap().len());
    for failure in failures {
        assert_eq!(
            (failure.task_id, failure.run_id.as_ref().map(RunId::as_str)),
            (None, None)
        );
        assert_eq!(failure.payload["op"], "ensure_group");
        assert_eq!(failure.payload["slots"], 1);
        assert_eq!(failure.payload["parallel"], 1);
    }
}

/// With one slot the supervisor claims the candidate whose completion
/// releases the most unfinished tasks before the older task 1 (ADR-0023),
/// and records no event for the reordering.
#[test]
fn supervisor_claims_the_candidate_that_releases_the_most_tasks_first() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let root = add_ready_task(&mut queue, "root", &[]);
    let middle = add_ready_task(&mut queue, "middle", &[root]);
    add_ready_task(&mut queue, "leaf", &[middle]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(1, true)).unwrap();
    let claimed: Vec<i64> = outcome["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["task_id"].as_i64().unwrap())
        .collect();
    assert_eq!(claimed, [root.as_i64(), 1]);
    let mut claim_event = |task: TaskId| {
        let detail = queue.show(task).unwrap();
        assert!(!event_kinds(&detail).contains(&"claim_reordered"));
        detail
            .events
            .iter()
            .find(|event| event.kind == "run_claimed")
            .unwrap()
            .id
    };
    assert!(claim_event(root) < claim_event(TaskId::new(1)));
    // The same order ties back to ID once nothing is released.
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(1, true)).unwrap();
    assert_eq!(outcome["runs"][0]["task_id"], 1);
    assert_eq!(outcome["runs"][1]["task_id"], 2);
}

/// The supervisor claims in the order `candidates` and `graph` show: the
/// highest effective priority first, whatever the ID or unblocks, and a
/// candidate that an urgent ready task waits for inherits urgent
/// (ADR-0040 decision 4).
#[test]
fn supervisor_claims_by_effective_priority_like_candidates_and_graph() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let low = add_ready_task(&mut queue, "later", &[]);
    queue.set_priority(low, Priority::Low).unwrap();
    let base = add_ready_task(&mut queue, "base", &[]);
    let waiter = add_ready_task(&mut queue, "urgent waiter", &[base]);
    queue.set_priority(waiter, Priority::Urgent).unwrap();
    let high = add_ready_task(&mut queue, "high", &[]);
    queue.set_priority(high, Priority::High).unwrap();
    let expected = [base, high, TaskId::new(1), low];
    let candidates: Vec<TaskId> = queue.candidates().unwrap().iter().map(|t| t.id()).collect();
    assert_eq!(candidates, expected);
    assert_eq!(
        dependency_graph(queue.graph_input().unwrap(), None).candidates,
        expected
    );
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &supervise_options(1, true)).unwrap();
    let claimed: Vec<TaskId> = outcome["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| TaskId::new(run["task_id"].as_i64().unwrap()))
        .collect();
    assert_eq!(claimed, expected);
}

#[test]
fn claim_creates_a_lease_that_only_its_owner_can_use_or_release() {
    use dagq::{domain::ClaimOutcome, infrastructure::runtime_store::RunPlan};
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.bind_repository("/repo/one/.git").unwrap();
    assert!(queue.bind_repository("/repo/two/.git").is_err());
    let base = "0123456789abcdef0123456789abcdef01234567";
    let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&sha(base), "first").unwrap()
    else {
        panic!()
    };
    assert!(matches!(
        queue.claim_for_supervisor(&sha(base), "first").unwrap(),
        ClaimOutcome::NoReadyTask
    ));
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_eq!(lease.pid, std::process::id());
    assert_eq!(queue.run_leases().unwrap().len(), 1);
    // An idle supervisor heartbeats nothing; the owner heartbeats its runs.
    assert_eq!(queue.heartbeat_leases("second").unwrap(), 0);
    assert_eq!(queue.heartbeat_leases("first").unwrap(), 1);
    let plan = RunPlan {
        repo_path: "/test".into(),
        run_dir: "/run".into(),
        branch: "dagq/test".into(),
        worktree_path: "/run/worktree".into(),
        receipt_path: "/run/receipt.json".into(),
        log_path: "/run/log".into(),
    };
    assert!(queue.plan_run(run.id(), "second", &plan).is_err());
    let raw = Connection::open(&db).unwrap();
    raw.execute("UPDATE run_leases SET heartbeat_at=0", [])
        .unwrap();
    assert!(queue.release_lease(run.id(), "second").is_err());
    assert_eq!(queue.run_lease(run.id()).unwrap().unwrap().heartbeat_at, 0);
    // A stale lease of the writer's own token is renewed, not refused
    // (ADR-0039 decision 7).
    queue.plan_run(run.id(), "first", &plan).unwrap();
    assert!(queue.run_lease(run.id()).unwrap().unwrap().heartbeat_at > 0);
    queue.release_lease(run.id(), "first").unwrap();
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert!(queue.release_lease(run.id(), "first").is_err());
    // The token stays on the run as a record of who executed it.
    let raw_token: String = raw
        .query_row(
            "SELECT supervisor_token FROM task_runs WHERE id=?1",
            [&run.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(raw_token, "first");
}

/// A clock the test moves by hand, in whole seconds.
#[derive(Clone)]
struct ManualClock(Arc<AtomicI64>);

impl ManualClock {
    fn at(secs: i64) -> Self {
        Self(Arc::new(AtomicI64::new(secs)))
    }

    fn set(&self, secs: i64) {
        self.0.store(secs, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn system_time(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.0.load(Ordering::SeqCst) as u64)
    }
}

/// IDs handed out in order.
struct FixedIds(Mutex<Vec<&'static str>>);

impl IdGenerator for FixedIds {
    fn uuid(&self) -> String {
        self.0.lock().unwrap().remove(0).to_owned()
    }
}

#[test]
fn an_injected_clock_decides_lease_staleness_and_injected_ids_name_the_run() {
    use dagq::{
        domain::{ClaimOutcome, HEARTBEAT_TIMEOUT_SECS},
        infrastructure::runtime_store::{RunPlan, lease_is_stale},
    };
    const T: i64 = 1_900_000_000;
    const RUN: &str = "11111111-1111-4111-8111-111111111111";
    let (_dir, _repo, db) = fixture();
    let clock = ManualClock::at(T);
    let mut queue = SqliteQueue::open(&db).unwrap().with_generators(Generators {
        clock: Arc::new(clock.clone()),
        ids: Arc::new(FixedIds(Mutex::new(vec![RUN]))),
    });
    let registration = queue.register_supervisor("first", 1, 1, VERSION).unwrap();
    assert_eq!((registration.started_at, registration.heartbeat_at), (T, T));
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&sha("0123456789abcdef0123456789abcdef01234567"), "first")
        .unwrap()
    else {
        panic!()
    };
    // The run ID, the claim time and the first heartbeat come from the
    // generators, the times in the form the columns always had.
    assert_eq!(run.id().as_str(), RUN);
    assert_eq!(run.created_at(), "2030-03-17T17:46:40.000Z");
    let task = queue.show(run.task_id()).unwrap().task;
    assert_eq!(task.updated_at(), "2030-03-17T17:46:40.000Z");
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_eq!(lease.heartbeat_at, T);
    assert!(!lease_is_stale(&lease, T + HEARTBEAT_TIMEOUT_SECS));
    assert!(lease_is_stale(&lease, T + HEARTBEAT_TIMEOUT_SECS + 1));
    let plan = RunPlan {
        repo_path: "/test".into(),
        run_dir: "/run".into(),
        branch: "dagq/test".into(),
        worktree_path: "/run/worktree".into(),
        receipt_path: "/run/receipt.json".into(),
        log_path: "/run/log".into(),
    };
    // The store stamps heartbeats by the same clock, both the process
    // heartbeat and the renewal of a lease-guarded write.
    clock.set(T + HEARTBEAT_TIMEOUT_SECS + 1);
    assert_eq!(queue.heartbeat("first").unwrap(), 1);
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_eq!(lease.heartbeat_at, T + HEARTBEAT_TIMEOUT_SECS + 1);
    assert_eq!(
        queue.supervisors().unwrap()[0].heartbeat_at,
        T + HEARTBEAT_TIMEOUT_SECS + 1
    );
    clock.set(T + HEARTBEAT_TIMEOUT_SECS + 5);
    queue.plan_run(run.id(), "first", &plan).unwrap();
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_eq!(lease.heartbeat_at, T + HEARTBEAT_TIMEOUT_SECS + 5);
}

/// ADR-0039 decision 7: a host sleep jumps the wall clock 120 s past the
/// last heartbeat between two lease-guarded writes. While the lease row
/// still carries the supervisor's token, the next write renews it and goes
/// on, so no other supervisor adopts the run afterwards. A supervisor that
/// stays asleep until another one adopted its stale lease (the adoption of a
/// dead supervisor's lease works as before) is refused and writes nothing.
#[test]
fn a_lease_of_its_own_token_is_renewed_after_a_host_sleep_until_another_supervisor_adopts_it() {
    use dagq::{
        domain::ClaimOutcome,
        infrastructure::runtime_store::{RunPlan, lease_is_stale},
    };
    const T: i64 = 1_900_000_000;
    const SLEEP: i64 = 120;
    let (_dir, _repo, db) = fixture();
    let clock = ManualClock::at(T);
    let mut queue = SqliteQueue::open(&db).unwrap().with_generators(Generators {
        clock: Arc::new(clock.clone()),
        ids: Arc::new(FixedIds(Mutex::new(vec![
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
        ]))),
    });
    add_ready_task(&mut queue, "second", &[]);
    let plan = |run: &TaskRun| RunPlan {
        repo_path: "/test".into(),
        run_dir: format!("/run/{}", run.id()),
        branch: format!("dagq/{}", run.id()),
        worktree_path: format!("/run/{}/worktree", run.id()),
        receipt_path: format!("/run/{}/receipt.json", run.id()),
        log_path: format!("/run/{}/log", run.id()),
    };
    let base = sha("0123456789abcdef0123456789abcdef01234567");
    let start = |queue: &mut SqliteQueue, token: &str| {
        let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&base, token).unwrap()
        else {
            panic!()
        };
        queue.plan_run(run.id(), token, &plan(&run)).unwrap();
        run
    };

    // The sleeper claims and plans at T, then the host sleeps 120 s.
    let run = start(&mut queue, "sleeper");
    clock.set(T + SLEEP);
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert!(lease_is_stale(&lease, T + SLEEP));
    // Woken up, it goes on with the run: every write renews the lease.
    queue
        .workspace_created(run.id(), "sleeper", "ws-1")
        .unwrap();
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_eq!(
        (lease.token.as_str(), lease.heartbeat_at),
        ("sleeper", T + SLEEP)
    );
    queue
        .register_wrapper(run.id(), "sleeper", std::process::id())
        .unwrap();
    queue
        .register_agent(run.id(), std::process::id(), std::process::id())
        .unwrap();
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Running);
    // The renewed lease is fresh, so another supervisor does not adopt it.
    assert!(
        queue
            .adopt_run(run.id(), "sleeper", "other", 2, json!({}))
            .unwrap()
            .is_none()
    );
    assert!(queue.holds_lease(run.id(), "sleeper").unwrap());
    assert_eq!(supervisor_token_of(&db, &run), "sleeper");
    assert!(!queue.has_run_event(run.id(), "run_adopted").unwrap());

    // A second run whose supervisor sleeps until another one adopts it.
    let taken = start(&mut queue, "late");
    queue.workspace_created(taken.id(), "late", "ws-2").unwrap();
    queue
        .register_wrapper(taken.id(), "late", std::process::id())
        .unwrap();
    queue
        .register_agent(taken.id(), std::process::id(), std::process::id())
        .unwrap();
    clock.set(T + 2 * SLEEP);
    let adopted = queue
        .adopt_run(taken.id(), "late", "adopter", 3, json!({}))
        .unwrap()
        .unwrap();
    assert_eq!(adopted.status(), RunStatus::Running);
    let events = queue.show(taken.task_id()).unwrap().events.len();
    // Woken up after the adoption, the late supervisor is refused and
    // writes nothing: the lease, the run and its events stay the adopter's.
    let error = queue.finish_supervision(taken.id(), "late").unwrap_err();
    assert_eq!(
        error.to_string(),
        "run lease is missing or held by another supervisor"
    );
    let lease = queue.run_lease(taken.id()).unwrap().unwrap();
    assert_eq!((lease.token.as_str(), lease.pid), ("adopter", 3));
    assert_eq!(supervisor_token_of(&db, &taken), "adopter");
    assert_eq!(queue.run(taken.id()).unwrap().status(), RunStatus::Running);
    assert_eq!(queue.show(taken.task_id()).unwrap().events.len(), events);
    // The adopter's own writes go on.
    queue
        .finish_supervision_live(taken.id(), "adopter")
        .unwrap();
    assert!(!queue.holds_lease(taken.id(), "late").unwrap());
}

#[test]
fn shell_arguments_round_trip_without_expansion_and_cmux_handles_are_strict() {
    let value = "a'b $HOME $(echo injected) `echo injected`\nmore";
    let result = Command::new("/bin/sh")
        .arg("-c")
        .arg(shell_join(&["printf".into(), "%s".into(), value.into()]))
        .bounded_output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(String::from_utf8(result.stdout).unwrap(), value);
    assert_eq!(workspace_handle("OK workspace:7\n").unwrap(), "workspace:7");
    assert!(workspace_handle("OK workspace:7; touch file").is_err());
    assert!(workspace_handle("OK surface:7").is_err());
}

#[test]
fn migration_from_v1_preserves_task_and_initializes_runtime_tables() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("old.db");
    let raw = Connection::open(&db).unwrap();
    raw.execute_batch(include_str!("../migrations/0001_queue.sql"))
        .unwrap();
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", 1).unwrap();
    raw.execute("INSERT INTO tasks(title,description,acceptance,verification_commands) VALUES ('preserved','','','[]')", []).unwrap();
    assert!(SqliteQueue::open(&db).is_err());
    SqliteQueue::migrate(&db, None, 0).unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(queue.schema_version().unwrap(), SqliteQueue::SCHEMA_VERSION);
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.title(),
        "preserved"
    );
    assert!(queue.run_leases().unwrap().is_empty());
}

#[test]
fn wrapper_registration_is_one_shot_and_rejects_other_owners() {
    use dagq::{
        domain::ClaimOutcome,
        infrastructure::runtime_store::{RunPlan, Validation},
    };
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&sha("0123456789abcdef0123456789abcdef01234567"), "owner")
        .unwrap()
    else {
        panic!()
    };
    queue
        .plan_run(
            run.id(),
            "owner",
            &RunPlan {
                repo_path: "/test".into(),
                run_dir: "/run".into(),
                branch: "dagq/test".into(),
                worktree_path: "/run/worktree".into(),
                receipt_path: "/run/receipt.json".into(),
                log_path: "/run/log".into(),
            },
        )
        .unwrap();
    assert!(queue.register_wrapper(run.id(), "owner", 10).is_err()); // Workspace not attached yet.
    queue
        .workspace_created(run.id(), "owner", "workspace")
        .unwrap();
    assert!(queue.register_wrapper(run.id(), "other-owner", 10).is_err());
    let raw = Connection::open(&db).unwrap();
    raw.execute("UPDATE run_leases SET heartbeat_at=0", [])
        .unwrap();
    // A stale lease of the owner's token is renewed, not refused (ADR-0039
    // decision 7).
    queue.register_wrapper(run.id(), "owner", 10).unwrap();
    assert!(queue.run_lease(run.id()).unwrap().unwrap().heartbeat_at > 0);
    assert!(queue.register_wrapper(run.id(), "owner", 11).is_err());
    assert!(queue.register_agent(run.id(), 11, 12).is_err());
    queue.register_agent(run.id(), 10, 12).unwrap();
    assert!(queue.finish_supervision(run.id(), "owner").is_err()); // Still live.
    queue.wrapper_exited(run.id(), 10, 0).unwrap();
    assert!(queue.heartbeat_wrapper(run.id(), 10).is_err());
    assert_eq!(
        queue
            .finish_supervision(run.id(), "owner")
            .unwrap()
            .status(),
        RunStatus::Validating
    );
    let validation = Validation {
        accepted: false,
        result_commit: None,
        reason: Some("receipt was not submitted".into()),
        code: Some(ReasonCode::ReceiptMissing),
        receipt: Value::Null,
        evidence_missing: Vec::new(),
        scope_violation: Vec::new(),
        allowed_paths: Vec::new(),
    };
    assert!(
        queue
            .finish_validation(run.id(), "other-owner", &validation)
            .is_err()
    );
    let failed = queue
        .finish_validation(run.id(), "owner", &validation)
        .unwrap();
    assert_eq!(failed.status(), RunStatus::Failed);
    assert_eq!(failed.last_error(), Some("receipt was not submitted"));
    assert!(
        queue
            .finish_validation(run.id(), "owner", &validation)
            .is_err()
    ); // Terminal.
    // A failed run never records a workspace close or cleanup failure.
    assert!(queue.workspace_closed(run.id(), "owner").is_err());
    assert!(
        queue
            .cleanup_failed(run.id(), "owner", "late", &ReasonCode::BackendFailed.into())
            .is_err()
    );
}

#[test]
fn workspace_close_is_recorded_once_and_only_for_accepted_runs() {
    use dagq::{
        domain::ClaimOutcome,
        infrastructure::runtime_store::{RunPlan, Validation},
    };
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&sha("0123456789abcdef0123456789abcdef01234567"), "owner")
        .unwrap()
    else {
        panic!()
    };
    queue
        .plan_run(
            run.id(),
            "owner",
            &RunPlan {
                repo_path: "/test".into(),
                run_dir: "/run".into(),
                branch: "dagq/test".into(),
                worktree_path: "/run/worktree".into(),
                receipt_path: "/run/receipt.json".into(),
                log_path: "/run/log".into(),
            },
        )
        .unwrap();
    queue
        .workspace_created(run.id(), "owner", WORKSPACE_ID)
        .unwrap();
    queue.register_wrapper(run.id(), "owner", 10).unwrap();
    queue.register_agent(run.id(), 10, 12).unwrap();
    // Still running: neither close nor cleanup failure may be recorded.
    assert!(queue.workspace_closed(run.id(), "owner").is_err());
    assert!(
        queue
            .cleanup_failed(
                run.id(),
                "owner",
                "early",
                &ReasonCode::BackendFailed.into()
            )
            .is_err()
    );
    queue.wrapper_exited(run.id(), 10, 0).unwrap();
    queue.finish_supervision(run.id(), "owner").unwrap();
    let accepted = queue
        .finish_validation(
            run.id(),
            "owner",
            &Validation {
                accepted: true,
                result_commit: Some(sha("89abcdef0123456789abcdef0123456789abcdef")),
                reason: None,
                code: None,
                receipt: Value::Null,
                evidence_missing: Vec::new(),
                scope_violation: Vec::new(),
                allowed_paths: Vec::new(),
            },
        )
        .unwrap();
    assert_eq!(accepted.status(), RunStatus::AwaitingIntegration);
    assert!(accepted.workspace_closed_at().is_none());
    assert!(queue.workspace_closed(run.id(), "other-owner").is_err());
    let failed = queue
        .cleanup_failed(
            run.id(),
            "owner",
            "cmux down",
            &ReasonCode::BackendFailed.into(),
        )
        .unwrap();
    assert_eq!(failed.status(), RunStatus::AwaitingIntegration);
    assert_eq!(failed.last_error(), Some("cmux down"));
    assert!(failed.workspace_closed_at().is_none());
    // A later successful close clears nothing but records the close once.
    let closed = queue.workspace_closed(run.id(), "owner").unwrap();
    assert!(closed.workspace_closed_at().is_some());
    assert_eq!(closed.status(), RunStatus::AwaitingIntegration);
    assert!(queue.workspace_closed(run.id(), "owner").is_err());
    assert!(
        queue
            .cleanup_failed(run.id(), "owner", "late", &ReasonCode::BackendFailed.into())
            .is_err()
    );
    let kinds: Vec<String> = queue
        .show(TaskId::new(1))
        .unwrap()
        .events
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert_eq!(kinds.iter().filter(|k| *k == "workspace_closed").count(), 1);
    assert_eq!(kinds.iter().filter(|k| *k == "cleanup_failed").count(), 1);
}

#[test]
fn no_ready_task_ends_a_once_pass_without_creating_a_run_or_lease() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(TaskId::new(1), TaskAction::Draft).unwrap();
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["runs"], json!([]));
    assert_eq!(outcome["errors"], json!([]));
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(queue.supervisors().unwrap().is_empty());
    assert!(queue.show(TaskId::new(1)).unwrap().runs.is_empty());
    assert!(supervise_with(&db, &repo, &backend, &supervise_options(0, true)).is_err());
    assert!(queue.supervisors().unwrap().is_empty());
}

/// A resident supervisor that holds no run is still listed by `status` and
/// `doctor` through its registration, which its heartbeat refreshes and a
/// graceful stop removes.
#[test]
fn resident_supervisor_without_runs_is_listed_until_it_stops() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(TaskId::new(1), TaskAction::Draft).unwrap();
    assert_eq!(runtime::status(&db).unwrap()["supervisors"], json!([]));
    let backend = Arc::new(TestWorkspace::new(&db, true, VALID_AGENT));
    let options = supervise_options(3, false);
    let supervisor = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(10), |queue| {
        queue.supervisors().unwrap().len() == 1
    });
    let registered = queue.supervisors().unwrap().remove(0);
    assert_eq!(registered.pid, std::process::id());
    assert_eq!(registered.parallel, 3);
    for report in [
        runtime::status(&db).unwrap(),
        runtime::doctor(&db, true).unwrap(),
    ] {
        assert_eq!(report["runs"], json!([]));
        assert_eq!(report["supervisors"].as_array().unwrap().len(), 1);
        let entry = &report["supervisors"][0];
        assert_eq!(entry["pid"], json!(std::process::id()));
        assert_eq!(entry["alive"], true);
        assert_eq!(entry["registered"], true);
        assert_eq!(entry["parallel"], 3);
        assert_eq!(entry["started_at"], json!(registered.started_at));
        assert_eq!(entry["stale"], false);
        assert!(entry["heartbeat_age_secs"].as_i64().unwrap() <= 5);
        assert_eq!(entry["run_ids"], json!([]));
    }
    // The heartbeat keeps the registration fresh while nothing runs.
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE supervisors SET heartbeat_at=0", [])
        .unwrap();
    wait_until(&db, Duration::from_secs(10), |queue| {
        queue.supervisors().unwrap()[0].heartbeat_at > 0
    });
    assert!(queue.run_leases().unwrap().is_empty());

    options.stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["outcome"], "stopped");
    assert_eq!(outcome["runs"], json!([]));
    assert!(queue.supervisors().unwrap().is_empty());
    assert_eq!(runtime::status(&db).unwrap()["supervisors"], json!([]));
    assert_eq!(
        runtime::doctor(&db, true).unwrap()["supervisors"],
        json!([])
    );

    // An error out of the loop itself (here: main vanished before a claim)
    // ends the process with nothing active, so it deregisters too.
    let options = supervise_options(1, false);
    let supervisor = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(10), |queue| {
        queue.supervisors().unwrap().len() == 1
    });
    git(&repo, &["update-ref", "-d", "refs/heads/main"]);
    queue
        .transition(TaskId::new(1), TaskAction::BypassReview)
        .unwrap();
    let error = format!(
        "{:#}",
        joined(supervisor, "the supervisor thread to return").unwrap_err()
    );
    assert!(error.contains("Needed a single revision"), "{error}");
    assert!(queue.supervisors().unwrap().is_empty());
    assert!(queue.show(TaskId::new(1)).unwrap().runs.is_empty());
}

/// A supervisor's progress goes to its process's JSON Lines file in the
/// log directory (ADR-0033): one record per line, with the startup facts,
/// the progress messages that also go to stderr, their run and task IDs as
/// fields, and the final result.
#[test]
fn supervise_records_its_progress_as_json_lines() {
    let (dir, repo, db) = fixture();
    let log_dir = dir.path().join("logs").join("nested");
    let options = supervise_options(2, true);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let telemetry = Telemetry::open(&log_dir, "supervise");
    let outcome = telemetry
        .in_scope(|| supervise_with(&db, &repo, &backend, &options))
        .unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    let pid = std::process::id();
    let path = telemetry.path.clone().unwrap();
    let name = path.file_name().unwrap().to_str().unwrap();
    assert!(
        name.starts_with("supervise-") && name.ends_with(&format!("Z-{pid}.jsonl")),
        "{name}"
    );
    assert_eq!(path.parent().unwrap(), log_dir);
    let text = fs::read_to_string(&path).unwrap();
    let records: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records[0]["message"], "dagq supervise started");
    let messages: Vec<&str> = records
        .iter()
        .map(|r| r["message"].as_str().unwrap())
        .collect();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs.remove(0);
    let token: String = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT supervisor_token FROM task_runs WHERE id=?1",
            [&run.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        messages.contains(&format!(
            "supervisor {token} started: version {VERSION}, pid {pid}, parallel 2, db {}, repository {}",
            db.canonicalize().unwrap().display(),
            repo.canonicalize().unwrap().display()
        ).as_str()),
        "{text}"
    );
    let running = records
        .iter()
        .find(|r| {
            r["message"]
                == format!(
                    "task 1 running in workspace {WORKSPACE_ID}; run {}",
                    run.id()
                )
        })
        .unwrap();
    assert_eq!(running["fields"]["run_id"], run.id().as_str());
    assert_eq!(running["fields"]["task_id"], "1");
    assert_eq!(running["level"], "INFO");
    assert_eq!(running["target"], "dagq::application::supervise::session");
    assert!(text.contains(&format!("receipt received for {}", run.id())));
    assert!(text.contains(&format!("run {} is awaiting_integration", run.id())));
    assert!(messages.iter().any(|m| m.starts_with(&format!(
        "supervisor {token} exiting: {{\"errors\":[],\"outcome\":\"finished\""
    ))));
}

/// A registration whose process died, or whose heartbeat stopped, is
/// reported as stale by `status` and `doctor` and left for a person;
/// neither a later supervisor nor `recover` removes it, and an `integrate`
/// or orphaned lease holder is listed next to it without a registration.
#[test]
fn killed_supervisor_registration_is_reported_stale_and_never_deleted() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let dead = dead_pid();
    let killed = queue
        .register_supervisor("killed", dead, 4, VERSION)
        .unwrap();
    // Killed a moment ago: the heartbeat is fresh, the pid is gone.
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["runs"], json!([]));
    let entry = &status["supervisors"][0];
    assert_eq!(entry["pid"], json!(dead));
    assert_eq!(entry["alive"], false);
    assert_eq!(entry["stale"], true);
    assert_eq!(entry["registered"], true);
    assert_eq!(entry["parallel"], 4);
    // The build the process ran, which `up` compares against its own.
    assert_eq!(entry["binary_version"], VERSION);
    assert_eq!(entry["heartbeat_at"], json!(killed.heartbeat_at));
    assert!(entry["heartbeat_age_secs"].as_i64().unwrap() <= 5);
    // Alive but silent: stale by heartbeat age alone.
    queue
        .register_supervisor("hung", std::process::id(), 1, VERSION)
        .unwrap();
    let raw = Connection::open(&db).unwrap();
    raw.execute(
        "UPDATE supervisors SET heartbeat_at=1700000000 WHERE token='hung'",
        [],
    )
    .unwrap();
    drop(raw);
    let doctor = runtime::doctor(&db, true).unwrap();
    assert_eq!(doctor["supervisors"].as_array().unwrap().len(), 2);
    let hung = &doctor["supervisors"][1];
    assert_eq!(hung["alive"], true);
    assert_eq!(hung["stale"], true);
    assert!(hung["heartbeat_age_secs"].as_i64().unwrap() > 30);
    assert_eq!(hung["run_ids"], json!([]));
    let compact = runtime::doctor(&db, false).unwrap();
    let keys: Vec<&String> = compact["supervisors"][1]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(
        keys,
        [
            "alive",
            "binary_version",
            "heartbeat_age_secs",
            "mode",
            "pid",
            "registered",
            "run_ids",
            "stale",
            "workspace_id"
        ]
    );
    assert_eq!(compact["supervisors"][1]["stale"], true);

    // A run owned without a registration (an `integrate` process, or a
    // supervisor from before the registry) is still attributed to its lease.
    let orphan = orphan_run(&repo, &db, "owner", std::process::id(), std::process::id());
    let status = runtime::status(&db).unwrap();
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 3);
    assert_eq!(supervisors[2]["registered"], false);
    assert_eq!(supervisors[2]["parallel"], Value::Null);
    // A lease holder without a registration recorded no version either.
    assert_eq!(supervisors[2]["binary_version"], Value::Null);
    assert_eq!(supervisors[2]["started_at"], Value::Null);
    assert_eq!(supervisors[2]["pid"], json!(std::process::id()));
    assert_eq!(supervisors[2]["alive"], true);
    assert_eq!(supervisors[2]["stale"], false);
    assert_eq!(supervisors[2]["run_ids"], json!([orphan.id()]));
    assert_eq!(status["runs"][0]["lease"]["pid"], json!(std::process::id()));
    // A registered supervisor's leases join it by token rather than by pid.
    queue
        .register_supervisor("owner", std::process::id(), 2, VERSION)
        .unwrap();
    let status = runtime::status(&db).unwrap();
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 3);
    assert_eq!(supervisors[2]["registered"], true);
    assert_eq!(supervisors[2]["parallel"], 2);
    assert_eq!(supervisors[2]["run_ids"], json!([orphan.id()]));

    // Recovery of the run and a later supervisor's own registration and
    // deregistration leave the stale rows alone.
    queue
        .wrapper_exited(orphan.id(), std::process::id(), 0)
        .unwrap();
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases", [])
        .unwrap();
    assert_eq!(
        runtime::recover(&db, orphan.id()).unwrap()["run"]["status"],
        "interrupted"
    );
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    assert_eq!(
        supervise(&db, &repo, &backend).unwrap()["outcome"],
        "finished"
    );
    let tokens: Vec<String> = queue
        .supervisors()
        .unwrap()
        .into_iter()
        .map(|s| s.token)
        .collect();
    assert_eq!(tokens, ["killed", "hung", "owner"]);
    assert_eq!(
        runtime::doctor(&db, true).unwrap()["supervisors"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

/// Whether a process with `pid` exists.
fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .bounded_status()
        .unwrap()
        .success()
}

/// A `sleep 60` with no stream of the test process: one a failing test
/// leaves behind does not hold the pipe of `cargo test | grep` open.
fn sleeper() -> Child {
    Command::new("sleep")
        .arg("60")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// A PID that certainly belonged to a process that has already exited.
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// Register a run the way `supervise` does under `token`, with the given PIDs
/// as wrapper and agent, but with no supervisor loop watching it. Returns the
/// running run.
fn orphan_run(repo: &Path, db: &Path, token: &str, wrapper: u32, agent: u32) -> TaskRun {
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

/// Workspaces as the test lists them, or a failing cmux.
struct Listing(Result<Vec<(&'static str, String)>, &'static str>);

impl dagq::application::stats::WorkspaceListing for Listing {
    fn list_workspaces(&self) -> Result<Vec<dagq::domain::stats::ListedWorkspace>> {
        match &self.0 {
            Ok(workspaces) => Ok(workspaces
                .iter()
                .map(|(id, description)| dagq::domain::stats::ListedWorkspace {
                    id: (*id).to_owned(),
                    description: Some(description.clone()),
                })
                .collect()),
            Err(message) => bail!("{message}"),
        }
    }
}

/// Task 182 (ADR-0043 decision 5): a running worker that stopped with a
/// background `cargo test` left running and no receipt is in `stats`'s
/// `running_alerts`, judged by the `[stall]` of the main checkout's
/// `dagq.toml`, with the cmux workspaces that do not match the runs.
#[test]
fn stats_raise_running_alerts_for_a_worker_idle_without_a_receipt() {
    let (_dir, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        "[stall]\nidle_without_receipt_secs = 600\nbackground_alert_secs = 3600\n",
    )
    .unwrap();
    let run = orphan_run(&repo, &db, "owner", dead_pid(), dead_pid());
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    fs::write(
        run_dir.join("idle.json"),
        r#"{"hook_event_name":"Stop","background_tasks":[{"id":"b1","type":"shell","status":"running","description":"cargo test","command":"cargo test --locked"}]}"#,
    )
    .unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let one_shot = |late: i64| {
        runtime::OneShot::new(Generators {
            clock: Arc::new(ManualClock::at(now + late)),
            ids: Arc::new(FixedIds(Mutex::new(vec![]))),
        })
    };
    let hash = QueueLocation::explicit(&db).hash();
    let left = Listing(Ok(vec![
        (
            "WS-LEFT",
            format!(
                "dagq role=worker queue={hash} run=99999999-9999-4999-8999-999999999999 task=7"
            ),
        ),
        (
            "WS-OTHER",
            "dagq role=worker queue=other run=x task=1".to_owned(),
        ),
    ]));

    // Past 600 seconds of idle, under the hour of background work.
    let stats = one_shot(700)
        .stats(&db, &Default::default(), Some(&left))
        .unwrap();
    assert_eq!(stats["stall_config"]["source"], "file", "{stats}");
    assert_eq!(stats["stall_config"]["idle_without_receipt_secs"], 600);
    assert_eq!(
        stats["workspace_check"],
        json!({"status": "checked", "workspaces": 2})
    );
    let alerts = stats["running_alerts"].as_array().unwrap();
    let idle = &alerts[0];
    assert_eq!(idle["kind"], "idle_without_receipt", "{stats}");
    assert_eq!(idle["run_id"], run.id().as_str());
    assert_eq!(idle["phase"], "session");
    assert_eq!(idle["threshold"], 600);
    assert!(idle["value"].as_i64().unwrap() >= 699, "{idle}");
    assert_eq!(idle["nudged"], false);
    assert_eq!(idle["asked"], false);
    assert_eq!(
        idle["background_tasks"][0]["command"],
        "cargo test --locked"
    );
    let mismatches: Vec<_> = alerts
        .iter()
        .filter(|alert| alert["kind"] == "workspace_mismatch")
        .map(|alert| (alert["reason"].clone(), alert["workspace_id"].clone()))
        .collect();
    assert_eq!(
        mismatches,
        [
            (json!("run_without_workspace"), json!("ws-1")),
            (json!("workspace_without_run"), json!("WS-LEFT")),
        ]
    );
    assert!(
        !alerts
            .iter()
            .any(|alert| alert["kind"] == "long_background")
    );
    // The finished-run alerts are where they were.
    assert!(stats["alerts"].as_array().unwrap().is_empty(), "{stats}");

    // Hours later the background work is an alert too; a cmux that cannot
    // be asked leaves only the workspaces unjudged.
    let stats = one_shot(4 * 3600)
        .stats(
            &db,
            &Default::default(),
            Some(&Listing(Err("cmux is gone"))),
        )
        .unwrap();
    let kinds: Vec<_> = stats["running_alerts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|alert| alert["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(kinds, ["idle_without_receipt", "long_background"]);
    assert_eq!(stats["workspace_check"]["status"], "unavailable");
    assert!(
        stats["workspace_check"]["reason"]
            .as_str()
            .unwrap()
            .contains("cmux is gone")
    );

    // A receipt of the session ends the idle alert; without cmux nothing
    // is said about the workspaces.
    fs::write(run_dir.join("receipt.json"), "{}").unwrap();
    let stats = one_shot(700).stats(&db, &Default::default(), None).unwrap();
    assert_eq!(stats["running_alerts"], json!([]), "{stats}");
    assert_eq!(stats["workspace_check"]["status"], "unavailable");
}

/// Task 331 (ADR-0043 decision 5): background work is timed from the first
/// idle marker that listed it as running, from the hook's log, so a session
/// that keeps taking turns does not restart the count.
#[test]
fn stats_time_background_work_from_its_first_marker() {
    let (_dir, repo, db) = fixture();
    let run = orphan_run(&repo, &db, "owner", dead_pid(), dead_pid());
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let running = |ids: &[&str]| {
        let tasks: Vec<Value> = ids
            .iter()
            .map(|id| json!({"id": id, "status": "running", "description": id, "command": "sleep"}))
            .collect();
        json!({"hook_event_name": "Stop", "background_tasks": tasks}).to_string()
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    fs::write(run_dir.join("idle.json"), running(&["b1", "b2"])).unwrap();
    let background = |log: String| {
        fs::write(run_dir.join("idle.log"), log).unwrap();
        let stats = runtime::OneShot::new(Generators {
            clock: Arc::new(ManualClock::at(now + 60)),
            ids: Arc::new(FixedIds(Mutex::new(vec![]))),
        })
        .stats(&db, &Default::default(), None)
        .unwrap();
        stats["running_alerts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|alert| alert["kind"] == "long_background")
            .cloned()
    };

    // b1 first listed 50 minutes ago, through turns taken since; a line
    // that is not a marker is skipped.
    let alert = background(format!(
        "{}\t{}\nnot a marker\n{}\t{}\n{now}\t{}\n",
        now - 3000,
        running(&["b1"]),
        now - 100,
        running(&["b1", "b2"]),
        running(&["b1", "b2"]),
    ))
    .expect("long_background");
    assert_eq!(alert["run_id"], run.id().as_str());
    assert_eq!(alert["threshold"], 1800);
    assert!(
        (3060..3065).contains(&alert["value"].as_i64().unwrap()),
        "{alert}"
    );
    assert_eq!(alert["background_tasks"][1]["id"], "b2");

    // A marker without b1 ended it: the b1 listed since started later.
    assert!(
        background(format!(
            "{}\t{}\n{}\t{}\n{}\t{}\n",
            now - 3000,
            running(&["b1"]),
            now - 1000,
            running(&[]),
            now - 900,
            running(&["b1", "b2"]),
        ))
        .is_none()
    );
    // Without a log (a session started before the hook kept one), the
    // marker's time is all there is.
    fs::remove_file(run_dir.join("idle.log")).unwrap();
    let stats = runtime::OneShot::new(Generators {
        clock: Arc::new(ManualClock::at(now + 1900)),
        ids: Arc::new(FixedIds(Mutex::new(vec![]))),
    })
    .stats(&db, &Default::default(), None)
    .unwrap();
    assert!(
        stats["running_alerts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|alert| alert["kind"] == "long_background"),
        "{stats}"
    );
}

#[test]
fn recover_requires_dead_processes_and_stale_lease_then_allows_a_new_run() {
    let (_dir, repo, db) = fixture();
    let mut wrapper = sleeper();
    let mut agent = sleeper();
    let run = orphan_run(&repo, &db, "owner", wrapper.id(), agent.id());
    let mut queue = SqliteQueue::open(&db).unwrap();

    // Everything is alive: doctor says so and recover refuses.
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["supervisors"][0]["pid"], json!(std::process::id()));
    assert_eq!(report["supervisors"][0]["stale"], false);
    assert_eq!(report["supervisors"][0]["alive"], true);
    assert_eq!(report["supervisors"][0]["run_ids"], json!([run.id()]));
    let health = &report["runs"][0];
    assert_eq!(health["run_id"], json!(run.id()));
    assert_eq!(health["status"], "running");
    assert_eq!(health["workspace_id"], "ws-1");
    assert_eq!(health["lease"]["stale"], false);
    assert_eq!(health["lease"]["alive"], true);
    assert_eq!(health["worktree_exists"], true);
    assert_eq!(health["run_dir_exists"], true);
    assert_eq!(health["receipt_exists"], false);
    assert_eq!(health["recoverable"], false);
    let processes = health["processes"].as_array().unwrap();
    assert_eq!(processes.len(), 2);
    assert!(processes.iter().all(|p| p["alive"] == true));
    let error = format!("{:#}", runtime::recover(&db, run.id()).unwrap_err());
    assert!(
        error.contains("wrapper pid") && error.contains("lease heartbeat"),
        "{error}"
    );
    assert!(
        queue
            .transition(TaskId::new(1), TaskAction::BypassReview)
            .is_err()
    );

    // The supervisor is gone (stale heartbeat, dead PID) but the session is not.
    let raw = Connection::open(&db).unwrap();
    raw.execute("UPDATE run_leases SET heartbeat_at=0, pid=?1", [dead_pid()])
        .unwrap();
    raw.execute("UPDATE run_processes SET heartbeat_at=0", [])
        .unwrap();
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["supervisors"][0]["stale"], true);
    assert_eq!(report["supervisors"][0]["alive"], false);
    assert_eq!(report["runs"][0]["lease"]["stale"], true);
    assert_eq!(report["runs"][0]["processes"][0]["heartbeat_stale"], true);
    let error = format!("{:#}", runtime::recover(&db, run.id()).unwrap_err());
    assert!(
        error.contains("agent pid") && !error.contains("supervisor"),
        "{error}"
    );
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Running);
    assert!(queue.run_lease(run.id()).unwrap().is_some());

    // The session processes are gone too: recovery is allowed and explicit.
    agent.kill().unwrap();
    agent.wait().unwrap();
    wrapper.kill().unwrap();
    wrapper.wait().unwrap();
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["runs"][0]["recoverable"], true);
    assert_eq!(report["runs"][0]["blockers"], json!([]));
    let outcome = runtime::recover(&db, run.id()).unwrap();
    assert_eq!(outcome["outcome"], "recovered");
    assert_eq!(outcome["run"]["status"], "interrupted");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(detail.runs[0].status(), RunStatus::Interrupted);
    assert!(queue.run_leases().unwrap().is_empty());
    let recovered = detail
        .events
        .iter()
        .find(|e| e.kind == "run_recovered")
        .unwrap();
    assert_eq!(recovered.payload["previous_status"], "running");
    assert_eq!(recovered.payload["lease_deleted"], true);
    assert_eq!(recovered.payload["run"]["processes"][0]["alive"], false);
    assert_eq!(recovered.payload["run"]["lease"]["stale"], true);
    // Registrations and resources are left as observed.
    assert!(detail.processes.iter().all(|p| p.exited_at.is_none()));
    assert!(Path::new(run.worktree_path().unwrap()).exists());
    assert!(runtime::recover(&db, run.id()).is_err()); // No longer unfinished.
    assert_eq!(runtime::doctor(&db, true).unwrap()["runs"], json!([]));
    assert!(queue.candidates().unwrap().is_empty());

    // Retry is a separate decision: ready again, then a second run with new paths.
    queue.transition(TaskId::new(1), TaskAction::Ready).unwrap();
    assert_eq!(queue.candidates().unwrap().len(), 1);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2);
    assert_eq!(detail.runs[0].status(), RunStatus::Interrupted);
    assert_eq!(detail.runs[0].worktree_path(), run.worktree_path());
    assert!(Path::new(run.worktree_path().unwrap()).exists());
    assert_ne!(detail.runs[1].worktree_path(), run.worktree_path());
    assert!(
        queue
            .transition(TaskId::new(1), TaskAction::BypassReview)
            .is_err()
    ); // Awaiting integration still owns the task.
}

#[test]
fn recover_ignores_exited_processes_and_tolerates_a_missing_lease() {
    let (_dir, repo, db) = fixture();
    // The wrapper reported its exit before the supervisor died; its live PID
    // (this test process) must not block recovery.
    let pid = std::process::id();
    let run = orphan_run(&repo, &db, "owner", pid, pid);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.wrapper_exited(run.id(), pid, 0).unwrap();
    let error = format!("{:#}", runtime::recover(&db, run.id()).unwrap_err());
    assert!(
        error.contains("supervisor pid") && !error.contains("wrapper pid"),
        "{error}"
    );
    let report = runtime::doctor(&db, true).unwrap();
    assert!(
        report["runs"][0]["processes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["alive"].is_null() && p["heartbeat_stale"] == false)
    );
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases", [])
        .unwrap();
    assert_eq!(
        runtime::doctor(&db, true).unwrap()["supervisors"],
        json!([])
    );
    let outcome = runtime::recover(&db, run.id()).unwrap();
    assert_eq!(outcome["run"]["status"], "interrupted");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let recovered = detail
        .events
        .iter()
        .find(|e| e.kind == "run_recovered")
        .unwrap();
    assert_eq!(recovered.payload["lease_deleted"], false);
    assert_eq!(recovered.payload["run"]["lease"], Value::Null);
    // The task can be edited again before a retry.
    assert!(
        queue
            .add_dependency(TaskId::new(1), TaskId::new(1))
            .is_err()
    );
    queue.transition(TaskId::new(1), TaskAction::Draft).unwrap();
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Draft
    );
    assert!(runtime::recover(&db, &RunId::new("no-such-run").unwrap()).is_err());
}

fn git_out(repo: &Path, args: &[&str]) -> String {
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
fn integrate(db: &Path, task_id: i64, repo: &Path) -> Result<Value> {
    let remote = GitRepository::inspect(repo).ok();
    runtime::integrate(
        db,
        IntegrateTarget::Task(TaskId::new(task_id)),
        repo,
        remote.as_ref().map(|r| r as &dyn MainRemote),
    )
}

fn integrate_next(db: &Path, repo: &Path) -> Value {
    let remote = GitRepository::inspect(repo).unwrap();
    runtime::integrate(db, IntegrateTarget::Next, repo, Some(&remote)).unwrap()
}

/// A Git remote double: `origin` exists unless `missing`, and a push fails
/// with `failure` when set. Every push is counted.
#[derive(Default)]
struct TestRemote {
    missing: bool,
    failure: Option<String>,
    pushes: Mutex<Vec<String>>,
}

impl MainRemote for TestRemote {
    fn has_remote(&self, remote: &str) -> Result<bool> {
        Ok(!self.missing && remote == "origin")
    }

    fn push_main(&self, remote: &str) -> Result<()> {
        self.pushes.lock().unwrap().push(remote.to_owned());
        match &self.failure {
            Some(failure) => bail!("{failure}"),
            None => Ok(()),
        }
    }
}

fn integrate_with(db: &Path, repo: &Path, remote: Option<&dyn MainRemote>) -> Value {
    runtime::integrate(db, IntegrateTarget::Task(TaskId::new(1)), repo, remote).unwrap()
}

fn events_of(db: &Path, run_id: &RunId, kind: &str) -> Vec<Value> {
    SqliteQueue::open(db)
        .unwrap()
        .run_events(run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.payload)
        .collect()
}

#[test]
fn integrate_pushes_the_landed_main_to_origin() {
    let (_dir, repo, db, run) = awaiting_run();
    let remote = TestRemote::default();
    let outcome = integrate_with(&db, &repo, Some(&remote));
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(
        outcome["push"],
        json!({"outcome": "pushed", "remote": "origin", "error": null})
    );
    assert_eq!(*remote.pushes.lock().unwrap(), ["origin"]);
    let landed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(
        events_of(&db, run.id(), "push_finished"),
        [json!({"remote": "origin", "commit": landed})]
    );
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    // The landing's message is searchable with its task and run (ADR-0046).
    let subject = git_out(&repo, &["log", "-1", "--format=%s", "main"]);
    let page = SqliteQueue::open(&db)
        .unwrap()
        .search(&SearchQuery {
            terms: subject.clone(),
            kinds: vec![SearchKind::Commit],
            limit: 5,
            ..SearchQuery::default()
        })
        .unwrap();
    assert_eq!(page.total, 1, "{subject}");
    let hit = &page.hits[0];
    assert_eq!(hit.id, SearchRef::Commit(landed));
    assert_eq!(hit.title, subject.trim());
    assert_eq!(
        (hit.task_id, hit.run_id.as_deref(), hit.status.as_deref()),
        (Some(1), Some(run.id().as_str()), Some("completed"))
    );
}

#[test]
fn a_failed_push_keeps_the_landing_and_waits_as_attention() {
    let (_dir, repo, db, run) = awaiting_run();
    let remote = TestRemote {
        failure: Some("rejected: fetch first".into()),
        ..TestRemote::default()
    };
    let outcome = integrate_with(&db, &repo, Some(&remote));
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(outcome["run"]["status"], "integrated");
    assert_eq!(outcome["task"]["status"], "completed");
    assert_eq!(outcome["push"]["outcome"], "failed");
    assert_eq!(outcome["push"]["remote"], "origin");
    assert_eq!(outcome["push"]["error"], "rejected: fetch first");
    let landed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(
        events_of(&db, run.id(), "push_failed"),
        [
            json!({"code": "push_failed", "remote": "origin", "commit": landed, "error": "rejected: fetch first"})
        ]
    );
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Completed
    );
    drop(queue);

    // `status` keeps it as an attention on the integrated run, and `events`
    // reports the push_failed event with its next.
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        run_attention_of(&status, run.id()).unwrap(),
        &json!({
            "run_id": run.id(), "task_id": 1, "status": "integrated",
            "kind": "push_failed", "last_error": "rejected: fetch first",
            "last_error_code": "push_failed", "next": "push main",
        })
    );
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    let failed = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "push_failed")
        .unwrap();
    assert_eq!(failed["next"], "push main");
    assert_eq!(failed["reason"], "rejected: fetch first");

    // A later successful push carries this landing too and clears it.
    SqliteQueue::open(&db)
        .unwrap()
        .record_runtime_event(run.id(), "push_finished", json!({"remote": "origin"}))
        .unwrap();
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
}

#[test]
fn no_push_and_a_missing_origin_skip_the_push() {
    let (_dir, repo, db, run) = awaiting_run();
    let remote = TestRemote::default();
    let outcome = integrate_with(&db, &repo, None);
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(
        outcome["push"],
        json!({"outcome": "skipped", "remote": "origin", "error": null, "reason": "--no-push"})
    );
    assert!(remote.pushes.lock().unwrap().is_empty());
    let skipped = events_of(&db, run.id(), "push_skipped");
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["reason"], "--no-push");

    let (_dir, repo, db, run) = awaiting_run();
    let remote = TestRemote {
        missing: true,
        ..TestRemote::default()
    };
    let outcome = integrate_with(&db, &repo, Some(&remote));
    assert_eq!(outcome["push"]["outcome"], "skipped", "{outcome}");
    assert_eq!(
        outcome["push"]["reason"],
        "the repository has no remote origin"
    );
    assert!(remote.pushes.lock().unwrap().is_empty());
    assert_eq!(events_of(&db, run.id(), "push_skipped").len(), 1);
}

/// The real Git adapter pushes main to a bare origin, and reports Git's
/// error when origin cannot take it.
#[test]
fn git_adapter_pushes_main_to_a_bare_origin() {
    let (dir, repo, db, run) = awaiting_run();
    let origin = dir.path().join("origin.git");
    let made = Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(&origin)
        .bounded_output()
        .unwrap();
    assert!(made.status.success());
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["push"]["outcome"], "pushed", "{outcome}");
    assert_eq!(
        git_out(&origin, &["rev-parse", "main"]),
        git_out(&repo, &["rev-parse", "main"])
    );
    assert_eq!(events_of(&db, run.id(), "push_finished").len(), 1);

    // An origin that is not a repository fails the push with Git's message.
    let adapter = GitRepository::inspect(&repo).unwrap();
    git(
        &repo,
        &["remote", "set-url", "origin", "/nonexistent/origin.git"],
    );
    assert!(adapter.has_remote("origin").unwrap());
    assert!(!adapter.has_remote("upstream").unwrap());
    let error = format!("{:#}", adapter.push_main("origin").unwrap_err());
    assert!(error.contains("git push origin main failed"), "{error}");
}

/// A task whose fake agent commits `file` with `content`; verification
/// commands default to the fixture's `test -f seed.txt`.
fn add_file_task(
    queue: &mut SqliteQueue,
    backend: &TestWorkspace,
    title: &str,
    file: &str,
    content: &str,
    verify: &[&str],
) -> TaskId {
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: verify.iter().map(|v| (*v).to_owned()).collect(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            dependencies: vec![],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    backend.script_for(
        task.id().as_i64(),
        &format!(
            "printf '{content}\\n' > '{file}' && git add '{file}' && git commit -q -m '{title}'; receipt \"$(git rev-parse HEAD)\""
        ),
    );
    task.id()
}

/// Rewrite the run's receipt the way a resumed session would after its work.
fn write_receipt(run: &TaskRun, commit: &str, result: &str, summary: &str) {
    write_receipt_json(run, session_receipt(run, commit, result, summary));
}

/// A session's receipt for `commit`, with the evidence it would give.
fn session_receipt(run: &TaskRun, commit: &str, result: &str, summary: &str) -> Value {
    json!({
        "run_id": run.id(), "result": result, "commit": commit,
        "tests": {"status": "passed", "evidence_or_reason": "reran"},
        "e2e": {"status": "not_applicable", "evidence_or_reason": "none"},
        "subagent_review": {"status": "not_applicable", "evidence_or_reason": "session"},
        "summary": summary,
    })
}

fn write_receipt_json(run: &TaskRun, receipt: Value) {
    let path = Path::new(run.receipt_path().unwrap());
    fs::write(path.with_extension("tmp"), receipt.to_string()).unwrap();
    fs::rename(path.with_extension("tmp"), path).unwrap();
}

/// The payloads of the `verification_command` events the landing recorded
/// (`phase: integration`), oldest first. Validation runs no verification
/// command (ADR-0023 decision 1).
fn integration_verifications(detail: &dagq::domain::TaskDetail) -> Vec<&Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == "verification_command" && e.payload["phase"] == "integration")
        .map(|e| &e.payload)
        .collect()
}

/// The `integration_receipt` events of the task's run, oldest first.
fn integration_receipts(detail: &dagq::domain::TaskDetail) -> Vec<&Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == "integration_receipt")
        .map(|e| &e.payload)
        .collect()
}

/// Assert that `main` is linear, `commits` long on top of `seed`, and that
/// its head carries the landing of `run` with the same tree as `source`.
fn assert_landed(repo: &Path, run: &TaskRun, task_title: &str, expected_parent: &str) {
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
fn awaiting_run() -> (Fixture, PathBuf, PathBuf, TaskRun) {
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

#[test]
fn review_writes_the_run_material_to_review_md_and_returns_only_its_size() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let goal = queue
        .add_goal(NewGoal {
            title: "goal title".into(),
            description: String::new(),
            acceptance: "goal acceptance".into(),
            constraints: "goal constraints".into(),
            doc: None,
            draft: false,
        })
        .unwrap();
    queue.set_goal(TaskId::new(1), Some(goal.id())).unwrap();
    // A task without a run to review is refused.
    let error = format!("{:#}", runtime::review(&db, TaskId::new(1)).unwrap_err());
    assert!(
        error.contains("task 1 (ready) has no run awaiting integration or a session"),
        "{error}"
    );
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let head = run.result_commit().cloned().unwrap();
    let mut receipt = session_receipt(&run, head.as_str(), "succeeded", "summary of the change");
    receipt["follow_ups"] = json!([{"title": "later work", "description": "outside the task"}]);
    write_receipt_json(&run, receipt);

    let outcome = runtime::review(&db, TaskId::new(1)).unwrap();
    let path = Path::new(run.run_dir().unwrap()).join("review.md");
    assert_eq!(
        outcome,
        json!({
            "run_id": run.id(),
            "task_id": 1,
            "path": path.to_str().unwrap(),
            "base": run.base_commit(),
            "head": head,
            "files_changed": 1,
            "insertions": 1,
            "deletions": 0,
        })
    );
    assert!(!outcome.to_string().contains("diff --git"));
    assert!(!path.with_file_name(".review.md.tmp").exists());
    let text = fs::read_to_string(&path).unwrap();
    let sections = [
        "# Review of task 1: test task",
        "## Task",
        "### Description\n\nsmall change",
        "### Acceptance\n\nworks",
        "### Verification commands\n\n```sh\ntest -f seed.txt\n```",
        "## Goal 1: goal title",
        "### Goal acceptance\n\ngoal acceptance",
        "### Goal constraints\n\ngoal constraints",
        "## Receipt",
        "### Summary\n\nsummary of the change",
        "### Tests: passed\n\nreran",
        "### E2E: not_applicable\n\nnone",
        "### Subagent review: not_applicable\n\nsession",
        "### Follow-ups\n\n- later work: outside the task",
        "## Commits",
        "## Diffstat",
        "## Diff",
    ];
    let mut at = 0;
    for section in sections {
        let found = text[at..]
            .find(section)
            .unwrap_or_else(|| panic!("{section:?} missing after byte {at}:\n{text}"));
        at += found + section.len();
    }
    let commits = &text[text.find("## Commits").unwrap()..text.find("## Diffstat").unwrap()];
    assert!(commits.contains(&head.as_str()[..7]), "{commits}");
    assert!(commits.contains(" work\n"), "{commits}");
    let stat = &text[text.find("## Diffstat").unwrap()..text.find("## Diff\n").unwrap()];
    assert!(stat.contains("change.txt | 1 +"), "{stat}");
    let diff = &text[text.find("## Diff\n").unwrap()..];
    assert!(
        diff.contains("```diff\ndiff --git a/change.txt b/change.txt"),
        "{diff}"
    );
    assert!(diff.contains(&format!("+change by {}", run.id())), "{diff}");
    assert!(diff.ends_with("\n```\n"), "{diff}");

    // A landed run is no longer reviewable.
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let error = format!("{:#}", runtime::review(&db, TaskId::new(1)).unwrap_err());
    assert!(error.contains("task 1 (completed) has no run"), "{error}");
}

/// A run whose file is Latin-1 text with a backtick run and whose commit
/// message is not UTF-8 still gets its review: Git's raw bytes go into
/// review.md under a longer fence, and the commit list is read lossily.
#[test]
fn review_writes_a_non_utf8_diff_as_raw_bytes() {
    let (_dir, db, detail) = run_agent(
        r#"printf 'caf\351 ````\n' > latin1.txt && git add latin1.txt && git commit -q -m "$(printf 'caf\351')"; receipt "$(git rev-parse HEAD)""#,
    );
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let outcome = runtime::review(&db, TaskId::new(1)).unwrap();
    assert_eq!(outcome["files_changed"], 1, "{outcome}");
    assert_eq!(outcome["insertions"], 1, "{outcome}");
    let run_dir = Path::new(run.run_dir().unwrap());
    let bytes = fs::read(run_dir.join("review.md")).unwrap();
    assert!(String::from_utf8(bytes.clone()).is_err());
    let text = String::from_utf8_lossy(&bytes);
    let commits = &text[text.find("## Commits").unwrap()..text.find("## Diffstat").unwrap()];
    // Git may re-encode the message on output; either way it is listed.
    assert!(commits.contains(" caf"), "{commits}");
    let diff_at = bytes.windows(8).position(|w| w == b"## Diff\n").unwrap();
    let diff = &bytes[diff_at..];
    let needle = b"+caf\xe9 ````\n";
    assert!(diff.windows(needle.len()).any(|w| w == needle), "{text}");
    assert!(
        text[text.find("## Diff\n").unwrap()..]
            .contains("`````diff\ndiff --git a/latin1.txt b/latin1.txt"),
        "{text}"
    );
    assert!(diff.ends_with(b"\n`````\n"), "{text}");
    // No temporary file is left beside review.md (the others are the
    // supervisor's headless review's).
    let leftovers: Vec<_> = fs::read_dir(run_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with(".review.md"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(run_dir.join("review.md").is_file());
}

#[test]
fn conflict_free_run_lands_as_one_squash_commit_and_releases_dependents() {
    let (dir, repo, db, run) = awaiting_run();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(seed, *run.base_commit());
    let source = run.result_commit().cloned().unwrap();
    // Another repository is refused even though it also has a main branch.
    let other = dir.path().join("other");
    fs::create_dir(&other).unwrap();
    git(&other, &["init", "-b", "main"]);
    git(&other, &["config", "user.name", "test"]);
    git(&other, &["config", "user.email", "test@example.invalid"]);
    git(&other, &["commit", "--allow-empty", "-m", "unrelated"]);
    let error = format!("{:#}", integrate(&db, 1, &other).unwrap_err());
    assert!(error.contains("the queue is bound to"), "{error}");
    assert!(integrate(&db, 1, &dir.path().join("missing")).is_err());

    // Landing from the run's own worktree resolves the same repository,
    // and the push that follows the worktree's removal still reaches Git.
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let outcome = integrate(&db, 1, &worktree).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(outcome["push"]["outcome"], "skipped", "{outcome}");
    // Main has not moved, so the rebase was a no-op; the verification
    // commands still run here, their only run for this commit (ADR-0023).
    assert_eq!(outcome["verification_skipped"], json!(false), "{outcome}");
    assert_eq!(outcome["task"]["status"], "completed");
    assert_eq!(outcome["run"]["status"], "integrated");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let landed = detail.runs[0].clone();
    assert_landed(&repo, &landed, "test task", &seed);
    assert!(landed.last_error().is_none());
    // No rebase was needed: the landed tree is the validated tree, and the
    // history ref points at the validated commit.
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", run.id())]
        ),
        source
    );
    assert_eq!(
        git_out(&repo, &["rev-parse", "main^{tree}"]),
        git_out(&repo, &["rev-parse", &format!("{source}^{{tree}}")])
    );
    let message = git_out(&repo, &["log", "-1", "--format=%B", "main"]);
    assert_eq!(
        message,
        format!("test task\n\ndone\n\nDagq-Task: 1\nDagq-Run: {}", run.id())
    );
    // The main checkout moved with the ref.
    assert_eq!(
        git_out(&repo, &["rev-parse", "HEAD"]),
        landed.result_commit().cloned().unwrap()
    );
    assert_eq!(git_out(&repo, &["status", "--porcelain"]), "");
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        format!("change by {}\n", run.id())
    );
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().rposition(|k| *k == kind).unwrap();
    assert!(position("validation_finished") < position("integration_started"));
    assert!(position("integration_started") < position("integration_rebased"));
    // The only verification commands are the landing's, after the rebase.
    let first = kinds
        .iter()
        .position(|k| *k == "verification_command")
        .unwrap();
    assert!(position("integration_rebased") < first);
    assert!(position("verification_command") < position("run_integrated"));
    assert!(!kinds.contains(&"integration_verification_skipped"));
    assert!(position("run_integrated") < position("worktree_removed"));
    assert!(!kinds.contains(&"cleanup_failed"));
    let integrated = detail
        .events
        .iter()
        .find(|e| e.kind == "run_integrated")
        .unwrap();
    assert_eq!(integrated.run_id.as_ref(), Some(run.id()));
    assert_eq!(
        integrated.payload["result_commit"],
        json!(landed.result_commit())
    );
    assert_eq!(integrated.payload["source_commit"], json!(source));
    assert_eq!(integrated.payload["main_before"], json!(seed));
    assert_eq!(
        integrated.payload["history_ref"],
        json!(format!("refs/dagq/runs/{}", run.id()))
    );
    assert_eq!(integrated.payload["verification_skipped"], json!(false));
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 1, "{verifications:?}");
    assert_eq!(verifications[0]["command"], "test -f seed.txt");
    assert_eq!(verifications[0]["exit_code"], 0);
    assert!(
        detail
            .events
            .iter()
            .all(|e| e.kind != "verification_command" || e.payload["phase"] == "integration"),
        "{:?}",
        event_kinds(&detail)
    );
    let run_dir = Path::new(run.run_dir().unwrap());
    assert!(run_dir.join("integrate-1-verify-1.log").exists());
    assert!(!run_dir.join("verify-1.log").exists());
    let changed = detail
        .events
        .iter()
        .find(|e| e.kind == "task_status_changed" && e.payload["to"] == "completed")
        .unwrap();
    assert_eq!(changed.run_id.as_ref(), Some(run.id()));
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(
        queue
            .candidates()
            .unwrap()
            .iter()
            .map(|t| t.id())
            .collect::<Vec<_>>(),
        [TaskId::new(2)]
    );

    // Integration is one-shot, at every layer.
    let error = format!("{:#}", integrate(&db, 1, &repo).unwrap_err());
    assert!(error.contains("no run awaiting integration"), "{error}");
    assert!(queue.begin_integration(run.id(), "x", &sha(&seed)).is_err());
    let error = format!("{:#}", integrate(&db, 2, &repo).unwrap_err());
    assert!(error.contains("task 2 (ready) has no run"), "{error}");
    assert!(integrate(&db, 99, &repo).is_err());
    assert_eq!(integrate_next(&db, &repo)["outcome"], "no_run_awaiting");
    let raw = Connection::open(&db).unwrap();
    assert!(
        raw.execute(
            "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
             VALUES ('again',1,'integrated','claude','claude',?1)",
            [&run.base_commit()],
        )
        .is_err()
    );
    assert_eq!(queue.show(TaskId::new(1)).unwrap().runs.len(), 1);
}

/// Move everything the queue at `db` owns (the database with its WAL files,
/// `runs/`, logs) from its directory into `to`, leaving the repository. The
/// runs keep the absolute paths they stored at claim time, as every queue
/// written before ADR-0017 does. Returns the database's new path.
fn move_queue(db: &Path, repo: &Path, to: &Path) -> PathBuf {
    fs::create_dir(to).unwrap();
    for entry in fs::read_dir(db.parent().unwrap()).unwrap() {
        let path = entry.unwrap().path();
        if path != repo && path != to {
            fs::rename(&path, to.join(path.file_name().unwrap())).unwrap();
        }
    }
    to.join(db.file_name().unwrap())
}

#[test]
fn moved_queue_directory_resolves_run_paths_and_lands_awaiting_runs() {
    let (dir, repo, db, run) = awaiting_run();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    let old_runs = dagq::infrastructure::location::runs_dir(&db.canonicalize().unwrap());
    // An unfinished run next to the awaiting one, for `status` and `doctor`.
    let other = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "unfinished", &[])
    };
    let unfinished = orphan_run(&repo, &db, "old-supervisor", dead_pid(), dead_pid());
    assert_eq!(unfinished.task_id(), other);

    let moved = dir.path().join("moved queue");
    let db = move_queue(&db, &repo, &moved);
    let runs = moved.canonicalize().unwrap().join("runs");
    assert!(!old_runs.exists());
    // The database still holds the paths of the old location: nothing is
    // migrated, they are resolved again from the run ID on every read.
    let raw = Connection::open(&db).unwrap();
    let stored: String = raw
        .query_row(
            "SELECT worktree_path FROM task_runs WHERE id=?1",
            [&run.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, run.worktree_path().unwrap().to_owned());
    assert!(stored.starts_with(old_runs.to_str().unwrap()), "{stored}");
    // Git still records the worktrees at their old paths.
    assert!(git_out(&repo, &["worktree", "list"]).contains("prunable"));

    let text = |path: PathBuf| Some(path.to_str().unwrap().to_owned());
    let expected = |id: &RunId| dagq::domain::RunPaths::new(&runs, id);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let shown = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let paths = expected(run.id());
    assert_eq!(shown.run_dir(), text(paths.run_dir.clone()).as_deref());
    assert_eq!(
        shown.worktree_path(),
        text(paths.worktree.clone()).as_deref()
    );
    assert_eq!(shown.receipt_path(), text(paths.receipt.clone()).as_deref());
    assert_eq!(shown.log_path(), text(paths.log.clone()).as_deref());
    assert_eq!(shown.repo_path(), run.repo_path());
    assert!(paths.worktree.is_dir() && paths.receipt.is_file());

    let status = runtime::status(&db).unwrap();
    let entry = &status["runs"].as_array().unwrap()[0];
    assert_eq!(entry["run_id"], json!(unfinished.id()), "{status}");
    assert_eq!(
        entry["worktree_path"],
        json!(text(expected(unfinished.id()).worktree)),
        "{status}"
    );
    let doctor = runtime::doctor(&db, true).unwrap();
    let health = &doctor["runs"].as_array().unwrap()[0];
    assert_eq!(health["run_id"], json!(unfinished.id()), "{doctor}");
    assert_eq!(
        health["worktree_path"],
        json!(text(expected(unfinished.id()).worktree))
    );
    assert_eq!(health["worktree_exists"], json!(true), "{doctor}");
    assert_eq!(
        health["run_dir"],
        json!(text(expected(unfinished.id()).run_dir))
    );
    assert_eq!(health["run_dir_exists"], json!(true), "{doctor}");

    // The awaiting run lands from the new location, and its worktree, whose
    // Git record is repaired on the way, is removed with its branch.
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let landed = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_landed(&repo, &landed, "test task", &seed);
    let kinds = event_kinds(&queue.show(TaskId::new(1)).unwrap())
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"worktree_removed".to_owned()), "{kinds:?}");
    assert!(!kinds.contains(&"cleanup_failed".to_owned()), "{kinds:?}");
    let listing = git_out(&repo, &["worktree", "list", "--porcelain"]);
    assert!(!listing.contains(run.id().as_str()), "{listing}");
}

/// A dependent's prompt names each predecessor with the commit `integrate`
/// landed and the summary its receipt carried, and lists the other tasks in
/// progress at claim time without the task itself.
#[test]
fn prompt_describes_landed_predecessors_and_sibling_tasks_in_progress() {
    let (_dir, repo, db, run) = awaiting_run();
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let landed = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(landed.status(), RunStatus::Integrated);
    let landed_commit = landed.result_commit().cloned().unwrap();
    assert_eq!(landed_commit, git_out(&repo, &["rev-parse", "main"]));
    // The squash commit, not the run's validated head, is what the prompt names.
    assert_ne!(landed_commit, *run.result_commit().unwrap());
    add_ready_task(&mut queue, "independent", &[]);

    // The queue's read-only view the prompt is built from.
    let predecessors = queue.predecessors(TaskId::new(2)).unwrap();
    assert_eq!(predecessors.len(), 1);
    assert_eq!(predecessors[0].task.id(), TaskId::new(1));
    assert_eq!(predecessors[0].task.title(), "test task");
    let integrated = predecessors[0].integrated_run.as_ref().unwrap();
    assert_eq!(integrated.id(), run.id());
    assert_eq!(
        integrated.result_commit().map(CommitSha::as_str),
        Some(landed_commit.as_str())
    );
    assert!(queue.predecessors(TaskId::new(3)).unwrap().is_empty());
    assert!(queue.tasks_in_progress().unwrap().is_empty());

    // The dependent (task 2) is claimed before the independent task 3.
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2);

    let dependent = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(dependent.status(), RunStatus::AwaitingIntegration);
    assert_eq!(*dependent.base_commit(), landed_commit);
    let prompt = read_prompt(&dependent);
    assert!(
        prompt.contains(&format!(
            "Predecessor tasks (their changes are already in your base commit):\n\
             - task 1: test task; result commit {landed_commit}; summary: done\n"
        )),
        "{prompt}"
    );
    assert!(!prompt.contains("Predecessor tasks: none"), "{prompt}");
    // Nothing else was in progress when task 2 was claimed; it is not listed itself.
    assert!(
        prompt.contains("Sibling tasks in progress: none\n"),
        "{prompt}"
    );
    assert!(!prompt.contains("- task 2: dependent"), "{prompt}");

    let independent = queue.show(TaskId::new(3)).unwrap().runs[0].clone();
    assert_eq!(independent.status(), RunStatus::AwaitingIntegration);
    let prompt = read_prompt(&independent);
    assert!(prompt.contains("Predecessor tasks: none\n"), "{prompt}");
    assert!(
        prompt.contains(
            "Sibling tasks in progress (other tasks executing now, each owning its own scope):\n\
             - task 2: dependent\n"
        ),
        "{prompt}"
    );
    assert!(!prompt.contains("- task 3: independent"), "{prompt}");
    // A completed task is not in progress.
    assert!(!prompt.contains("- task 1: test task"), "{prompt}");
    // The sections sit between the verification commands and the receipt contract.
    let position = |needle: &str| prompt.find(needle).unwrap();
    assert!(position("Verification commands") < position("Predecessor tasks"));
    assert!(position("Predecessor tasks") < position("Sibling tasks in progress"));
    assert!(position("Sibling tasks in progress") < position("Write a completion receipt"));
}

/// A ready task with a goal and a context, registered on the queue.
fn add_ready_task_in(
    queue: &mut SqliteQueue,
    title: &str,
    goal_id: Option<GoalId>,
    context: &str,
) -> TaskId {
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            dependencies: vec![],
            goal_dependencies: Vec::new(),
            goal_id,
            context: context.into(),
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    task.id()
}

/// The section headings of a prompt in order of appearance, so tests can
/// compare the shape of prompts with and without a goal.
fn section_order(prompt: &str) -> Vec<usize> {
    [
        "Task title:",
        "Verification commands",
        "Goal",
        "Context",
        "Predecessor tasks",
        "Sibling tasks in progress",
        "Your assignment is this task only.",
        "Write a completion receipt",
    ]
    .iter()
    .map(|heading| {
        prompt
            .find(heading)
            .unwrap_or_else(|| panic!("{heading}: {prompt}"))
    })
    .collect()
}

/// A task's prompt carries its goal as a Goal section (ID, title,
/// description, acceptance, constraints and the doc path, unread) and its
/// context as a Context section; a task without either says so in the same
/// place, so both prompts have the same sequence of sections.
#[test]
fn prompt_describes_the_goal_and_the_context_and_keeps_one_shape_without_them() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let goal = queue
        .add_goal(NewGoal {
            title: "goal title".into(),
            description: "goal description\nsecond line".into(),
            acceptance: "goal acceptance".into(),
            constraints: "goal constraints".into(),
            doc: Some("docs/plans/goal.md".into()),
            draft: false,
        })
        .unwrap();
    fs::write(repo.join("unrelated.md"), "not read\n").unwrap();
    add_ready_task_in(
        &mut queue,
        "grouped",
        Some(goal.id()),
        "why this task exists\nread docs/design/x.md first",
    );
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2);

    let alone = read_prompt(&queue.show(TaskId::new(1)).unwrap().runs[0]);
    assert!(
        alone.contains("Goal: none, this task stands alone\n"),
        "{alone}"
    );
    assert!(alone.contains("Context: none\n"), "{alone}");
    assert!(!alone.contains("goal title"), "{alone}");

    let grouped = read_prompt(&queue.show(TaskId::new(2)).unwrap().runs[0]);
    assert!(
        grouped.contains(&format!(
            "Goal (the higher-level problem this task and its sibling tasks solve together):\n\
             Goal ID: {}\nGoal title: goal title\nGoal description:\ngoal description\nsecond line\n\
             Goal acceptance:\ngoal acceptance\nGoal constraints:\ngoal constraints\n\
             Goal doc: docs/plans/goal.md (a path in the repository; read it for the full picture)\n",
            goal.id()
        )),
        "{grouped}"
    );
    // The doc is named by path only; the prompt never embeds its content.
    assert!(!grouped.contains("not read"), "{grouped}");
    assert!(
        grouped.contains(
            "Context (why this task exists and what to read first):\n\
             why this task exists\nread docs/design/x.md first\n"
        ),
        "{grouped}"
    );
    assert!(!grouped.contains("Goal: none"), "{grouped}");
    assert!(!grouped.contains("Context: none"), "{grouped}");
    // Both prompts have the same sections in the same order.
    let order = section_order(&grouped);
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{grouped}");
    let order = section_order(&alone);
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{alone}");
    // The receipt example shows the optional follow_ups, and the scope rule names it.
    for prompt in [&alone, &grouped] {
        assert!(
            prompt.contains(
                "\"summary\":\"...\",\"follow_ups\":[{\"title\":\"...\",\"description\":\"...\"}]}\n"
            ),
            "{prompt}"
        );
        assert!(
            prompt.contains(
                "Your assignment is this task only. Do not change what a sibling task owns; \
                 if you find work outside this task, record it in the receipt as follow_ups instead of doing it.\n"
            ),
            "{prompt}"
        );
        assert!(prompt.contains("follow_ups is optional"), "{prompt}");
        // The worker reads only what its run needs, never the queue.
        assert!(prompt.contains(runtime::WORKER_READING), "{prompt}");
        assert!(
            prompt.contains("the worker section of the repository instructions"),
            "{prompt}"
        );
        assert!(prompt.contains("Do not run `dagq list`"), "{prompt}");
        assert!(
            !prompt.contains("Read its repository instructions"),
            "{prompt}"
        );
    }
}

/// The Sibling section of a task with a goal lists only the in-progress
/// tasks of that goal; a task without a goal still sees every task in
/// progress. Claims in one pass go in ID order, so a prompt lists the
/// siblings claimed before it.
#[test]
fn prompt_lists_only_the_siblings_of_the_same_goal_in_progress() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    // Task 1 (no goal) is in progress before the grouped tasks are claimed.
    assert_eq!(
        supervise(&db, &repo, &backend).unwrap()["errors"],
        json!([])
    );
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::InProgress
    );
    let a = queue
        .add_goal(NewGoal {
            title: "goal a".into(),
            ..NewGoal::default()
        })
        .unwrap();
    let b = queue
        .add_goal(NewGoal {
            title: "goal b".into(),
            ..NewGoal::default()
        })
        .unwrap();
    assert_eq!(
        add_ready_task_in(&mut queue, "a first", Some(a.id()), ""),
        TaskId::new(2)
    );
    assert_eq!(
        add_ready_task_in(&mut queue, "b only", Some(b.id()), ""),
        TaskId::new(3)
    );
    assert_eq!(
        add_ready_task_in(&mut queue, "a second", Some(a.id()), ""),
        TaskId::new(4)
    );
    assert_eq!(
        add_ready_task_in(&mut queue, "alone", None, ""),
        TaskId::new(5)
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 4);

    let mut prompt_of =
        |task_id: i64| read_prompt(&queue.show(TaskId::new(task_id)).unwrap().runs[0]);
    // Task 2: task 1 is in progress but belongs to no goal, so it is not a sibling.
    let first = prompt_of(2);
    assert!(
        first.contains("Sibling tasks in progress: none\n"),
        "{first}"
    );
    assert!(!first.contains("- task 1: test task"), "{first}");
    // Task 3: nothing of goal b is in progress; goal a's task 2 is not listed.
    let only = prompt_of(3);
    assert!(only.contains("Sibling tasks in progress: none\n"), "{only}");
    assert!(!only.contains("- task 2: a first"), "{only}");
    // Task 4: its sibling task 2 is listed, task 3 of goal b and task 1 are not.
    let second = prompt_of(4);
    assert!(
        second.contains(
            "Sibling tasks in progress (other tasks executing now, each owning its own scope):\n\
             - task 2: a first\n"
        ),
        "{second}"
    );
    assert!(!second.contains("- task 3: b only"), "{second}");
    assert!(!second.contains("- task 1: test task"), "{second}");
    assert!(!second.contains("- task 4: a second"), "{second}");
    // Task 5 has no goal and sees every task in progress except itself.
    let alone = prompt_of(5);
    assert!(
        alone.contains(
            "Sibling tasks in progress (other tasks executing now, each owning its own scope):\n\
             - task 1: test task\n- task 2: a first\n- task 3: b only\n- task 4: a second\n"
        ),
        "{alone}"
    );
    assert!(!alone.contains("- task 5: alone"), "{alone}");
}

/// `prompt.txt` is a snapshot taken at claim time: a run claimed before a
/// goal edit keeps the old wording, and a run claimed after it gets the new.
#[test]
fn prompt_snapshots_the_goal_at_claim_time() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let goal = queue
        .add_goal(NewGoal {
            title: "before edit".into(),
            acceptance: "old acceptance".into(),
            ..NewGoal::default()
        })
        .unwrap();
    add_ready_task_in(&mut queue, "early", Some(goal.id()), "");
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    assert_eq!(
        supervise(&db, &repo, &backend).unwrap()["errors"],
        json!([])
    );
    backend.join();
    let early = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    let before = read_prompt(&early);
    assert!(before.contains("Goal title: before edit\n"), "{before}");
    assert!(
        before.contains("Goal acceptance:\nold acceptance\n"),
        "{before}"
    );

    queue
        .edit_goal(
            goal.id(),
            GoalEdit {
                title: Some("after edit".into()),
                acceptance: Some("new acceptance".into()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
    add_ready_task_in(&mut queue, "late", Some(goal.id()), "");
    assert_eq!(
        supervise(&db, &repo, &backend).unwrap()["errors"],
        json!([])
    );
    backend.join();
    let late = queue.show(TaskId::new(3)).unwrap().runs[0].clone();
    let after = read_prompt(&late);
    assert!(after.contains("Goal title: after edit\n"), "{after}");
    assert!(
        after.contains("Goal acceptance:\nnew acceptance\n"),
        "{after}"
    );
    assert!(!after.contains("before edit"), "{after}");
    // The earlier run's prompt was not rewritten by the edit or the later claim.
    assert_eq!(read_prompt(&early), before);
    // The late run also sees the early one as a sibling still in progress.
    assert!(after.contains("- task 2: early\n"), "{after}");
}

/// A predecessor whose receipt is gone from its run directory is still
/// named in the prompt; the successor's run starts and runs as usual.
#[test]
fn successor_starts_when_the_predecessor_receipt_is_unavailable() {
    let (_dir, repo, db, run) = awaiting_run();
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let receipt = Path::new(run.receipt_path().unwrap());
    assert_eq!(
        receipt,
        Path::new(run.run_dir().unwrap()).join("receipt.json")
    );
    fs::remove_file(receipt).unwrap();

    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["errors"], json!([]));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    let dependent = &detail.runs[0];
    assert_eq!(dependent.status(), RunStatus::AwaitingIntegration);
    let kinds = event_kinds(&detail);
    assert!(kinds.contains(&"agent_started"), "{kinds:?}");
    let landed_commit = queue.show(TaskId::new(1)).unwrap().runs[0]
        .result_commit()
        .cloned()
        .unwrap();
    let prompt = read_prompt(dependent);
    assert!(
        prompt.contains(&format!(
            "- task 1: test task; result commit {landed_commit}; summary: (receipt unavailable)\n"
        )),
        "{prompt}"
    );
    // A corrupt receipt is described the same way.
    let corrupt = queue.predecessors(TaskId::new(2)).unwrap();
    fs::write(receipt, "not json").unwrap();
    let summary = runtime::PredecessorSummary::from_predecessor(&LocalRunFiles, &corrupt[0]);
    assert_eq!(summary.summary, "(receipt unavailable)");
    assert_eq!(summary.result_commit, landed_commit);
    assert_eq!(
        (summary.task_id, summary.title.as_str()),
        (TaskId::new(1), "test task")
    );
    // A predecessor completed without an integrated run has neither.
    let by_hand = dagq::domain::Predecessor {
        task: corrupt[0].task.clone(),
        integrated_run: None,
    };
    let summary = runtime::PredecessorSummary::from_predecessor(&LocalRunFiles, &by_hand);
    assert_eq!(summary.result_commit, "(not landed)");
    assert_eq!(summary.summary, "(receipt unavailable)");
}

/// Two accepted runs land in the order their validation finished; the
/// second is rebased onto the first landing and main stays linear with one
/// commit per task, whether or not a checkout has main checked out.
#[test]
fn runs_land_fifo_by_validation_time_and_later_ones_are_rebased() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(TaskId::new(1), TaskAction::Draft).unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let a = add_file_task(
        &mut queue,
        &backend,
        "task a",
        "a.txt",
        "a",
        &["test -f seed.txt"],
    );
    let b = add_file_task(
        &mut queue,
        &backend,
        "task b",
        "b.txt",
        "b",
        &["test -f seed.txt"],
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2);
    let seed = git_out(&repo, &["rev-parse", "main"]);
    let run_a = queue.show(a).unwrap().runs[0].clone();
    let run_b = queue.show(b).unwrap().runs[0].clone();
    // FIFO is by validation time, not task id.
    let mut validated_at = |run: &TaskRun| {
        queue
            .show(run.task_id())
            .unwrap()
            .events
            .iter()
            .find(|e| e.kind == "validation_finished")
            .unwrap()
            .id
    };
    let (first, second) = if validated_at(&run_a) < validated_at(&run_b) {
        (run_a.clone(), run_b.clone())
    } else {
        (run_b.clone(), run_a.clone())
    };
    assert_eq!(
        queue.next_awaiting_integration().unwrap().unwrap().id(),
        first.id()
    );

    let outcome = integrate_next(&db, &repo);
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(outcome["run"]["id"], json!(first.id()));
    let first_landed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(
        queue.next_awaiting_integration().unwrap().unwrap().id(),
        second.id()
    );

    // No checkout has main now: the ref is updated directly.
    git(&repo, &["checkout", "-q", "--detach"]);
    let outcome = integrate_next(&db, &repo);
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(outcome["run"]["id"], json!(second.id()));
    assert_eq!(integrate_next(&db, &repo)["outcome"], "no_run_awaiting");
    let second_landed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), first_landed); // Detached HEAD untouched.
    git(&repo, &["checkout", "-q", "main"]);

    let landed_second = queue.show(second.task_id()).unwrap().runs[0].clone();
    assert_landed(&repo, &landed_second, "task ", &first_landed);
    let landed_first = queue.show(first.task_id()).unwrap().runs[0].clone();
    assert_eq!(
        landed_first.result_commit().map(CommitSha::as_str),
        Some(first_landed.as_str())
    );
    // Linear: seed → first → second, one commit per task, both files present.
    assert_eq!(
        git_out(&repo, &["rev-list", "--first-parent", "main"])
            .lines()
            .collect::<Vec<_>>(),
        [second_landed.as_str(), first_landed.as_str(), seed.as_str()]
    );
    assert!(repo.join("a.txt").exists() && repo.join("b.txt").exists());
    // The second run was rebased: its history ref sits on the first landing.
    let history = git_out(
        &repo,
        &["rev-parse", &format!("refs/dagq/runs/{}", second.id())],
    );
    assert_ne!(history, second.result_commit().cloned().unwrap());
    assert_eq!(
        git_out(&repo, &["rev-parse", &format!("{history}^")]),
        first_landed
    );
    let rebased = queue
        .show(second.task_id())
        .unwrap()
        .events
        .into_iter()
        .find(|e| e.kind == "integration_rebased")
        .unwrap();
    assert_eq!(rebased.payload["main"], json!(first_landed));
    assert_eq!(
        rebased.payload["head_before"],
        json!(second.result_commit())
    );
    assert_eq!(rebased.payload["head_after"], json!(history));
    for task in [a, b] {
        assert_eq!(
            queue.show(task).unwrap().task.status(),
            TaskStatus::Completed
        );
    }
    assert!(queue.run_leases().unwrap().is_empty());
}

/// Both runs change the same file: the second cannot be rebased by the
/// runtime and waits for a session, which resolves, reruns verification and
/// rewrites the receipt; then it lands like any other run.
#[test]
fn conflicting_run_needs_a_session_and_lands_after_the_session_resolves_it() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let first_landed = git_out(&repo, &["rev-parse", "main"]);

    let run = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    let source = run.result_commit().cloned().unwrap();
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    assert_eq!(outcome["main"], json!(first_landed));
    let reason = outcome["reason"].as_str().unwrap();
    assert!(reason.contains("conflicted in change.txt"), "{reason}");
    assert!(
        reason.contains(&format!("git rebase {first_landed}")),
        "{reason}"
    );
    let parked = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(parked.status(), RunStatus::NeedsSession);
    assert_eq!(parked.last_error(), Some(reason));
    assert_eq!(
        parked.result_commit().map(CommitSha::as_str),
        Some(source.as_str())
    );
    // A parked run is reviewable against its own base.
    let review = runtime::review(&db, TaskId::new(2)).unwrap();
    assert_eq!(review["head"], json!(source));
    assert_eq!(review["base"], json!(seed));
    // The rebase was aborted: the worktree is back on its validated head, clean.
    assert_eq!(git_out(&worktree, &["rev-parse", "HEAD"]), source);
    assert_eq!(git_out(&worktree, &["status", "--porcelain"]), "");
    assert!(!worktree.join(".git").join("rebase-merge").exists());
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);
    let detail = queue.show(TaskId::new(2)).unwrap();
    let deferred = detail
        .events
        .iter()
        .find(|e| e.kind == "integration_deferred")
        .unwrap();
    assert_eq!(deferred.payload["status"], "needs_session");
    assert_eq!(deferred.payload["code"], "rebase_conflict");
    assert_eq!(deferred.payload["conflicts"], json!(["change.txt"]));
    assert_eq!(deferred.payload["aborted"], true);
    assert!(
        deferred.payload["output_tail"]
            .as_str()
            .unwrap()
            .contains("CONFLICT")
    );
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert!(queue.run_leases().unwrap().is_empty());
    // A parked run still owns its task and is not picked by --next.
    assert!(
        queue
            .transition(TaskId::new(2), TaskAction::BypassReview)
            .is_err()
    );
    assert_eq!(integrate_next(&db, &repo)["outcome"], "no_run_awaiting");
    assert!(queue.candidates().unwrap().is_empty());
    assert_eq!(runtime::doctor(&db, true).unwrap()["runs"], json!([]));

    // Nothing changed in the worktree: the runtime tries again and parks it again.
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");

    // The session resolves the conflict on top of main.
    let rebase = Command::new("git")
        .arg("-C")
        .arg(&worktree)
        .args(["rebase", &first_landed])
        .bounded_output()
        .unwrap();
    assert!(!rebase.status.success());
    fs::write(worktree.join("change.txt"), "resolved by the session\n").unwrap();
    git(&worktree, &["add", "change.txt"]);
    let status = Command::new("git")
        .arg("-C")
        .arg(&worktree)
        .env("GIT_EDITOR", "true")
        .args(["rebase", "--continue"])
        .bounded_status()
        .unwrap();
    assert!(status.success());
    let resolved = git_out(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(resolved, source);

    // Until the receipt names the new head, the session is not done.
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let reason = outcome["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("receipt commit {source} is not the head")),
        "{reason}"
    );
    assert_eq!(git_out(&worktree, &["rev-parse", "HEAD"]), resolved); // Left as the session made it.
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);

    // Every receipt that passed its checks was recorded, even when the
    // landing then stopped: the stale one names the validated head.
    let detail = queue.show(TaskId::new(2)).unwrap();
    let recorded = integration_receipts(&detail);
    assert_eq!(recorded.len(), 3, "{recorded:?}");
    assert!(recorded.iter().all(|p| p["commit"] == json!(source)));
    assert!(recorded.iter().all(|p| p["main"] == json!(first_landed)));

    let mut rewritten = session_receipt(&parked, &resolved, "succeeded", "resolved");
    rewritten["tests"]["evidence_or_reason"] = json!("cargo test after the rebase: 12 passed");
    rewritten["follow_ups"] = json!([
        {"title": "dedupe change.txt", "description": "both tasks wrote it"}
    ]);
    write_receipt_json(&parked, rewritten.clone());
    // The session's head sits on the landed main, so the review is taken
    // against that main and leaves out the first task's landing.
    let review = runtime::review(&db, TaskId::new(2)).unwrap();
    assert_eq!(review["base"], json!(first_landed));
    assert_eq!(review["head"], json!(resolved));
    assert_eq!(review["files_changed"], json!(1));
    let text = fs::read_to_string(review["path"].as_str().unwrap()).unwrap();
    assert!(text.contains("+resolved by the session"), "{text}");
    let commits = &text[text.find("## Commits").unwrap()..text.find("## Diffstat").unwrap()];
    let listed = &commits[commits.find("```").unwrap()..];
    assert_eq!(listed.matches(" work\n").count(), 1, "{commits}");
    assert!(!listed.contains(&first_landed[..7]), "{commits}");
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    // The session rebased the branch itself, so this rebase is a no-op; the
    // verification commands run here all the same, as on every landing.
    assert_eq!(outcome["verification_skipped"], json!(false), "{outcome}");
    let landed = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_landed(&repo, &landed, "second", &first_landed);
    // The receipt the session rewrote is what the DB keeps for the landing,
    // while validation_finished still holds the one from before the conflict.
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert!(!event_kinds(&detail).contains(&"integration_verification_skipped"));
    let rebased = detail
        .events
        .iter()
        .rev()
        .find(|e| e.kind == "integration_rebased")
        .unwrap();
    assert_eq!(rebased.payload["head_before"], json!(resolved));
    assert_eq!(rebased.payload["head_after"], json!(resolved));
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(verifications[0]["exit_code"], 0);
    let recorded = integration_receipts(&detail);
    assert_eq!(recorded.len(), 4, "{recorded:?}");
    let last = recorded[3];
    assert_eq!(last["commit"], json!(resolved));
    assert_eq!(last["main"], json!(first_landed));
    assert_eq!(last["receipt"], rewritten);
    assert_eq!(last["receipt"]["commit"], json!(resolved));
    assert_eq!(
        last["receipt"]["tests"]["evidence_or_reason"],
        json!("cargo test after the rebase: 12 passed")
    );
    assert_eq!(
        last["receipt"]["follow_ups"],
        json!([{"title": "dedupe change.txt", "description": "both tasks wrote it"}])
    );
    let validated = detail
        .events
        .iter()
        .find(|e| e.kind == "validation_finished")
        .unwrap();
    assert_eq!(validated.payload["receipt"]["commit"], json!(source));
    assert!(validated.payload["receipt"].get("follow_ups").is_none());
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", run.id())]
        ),
        resolved
    );
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "resolved by the session\n"
    );
    assert_eq!(
        git_out(&repo, &["rev-list", "--count", &format!("{seed}..main")]),
        "2"
    );
    assert_eq!(
        git_out(&repo, &["log", "-1", "--format=%b", "main"])
            .lines()
            .next(),
        Some("resolved")
    );
    assert_eq!(
        queue.show(TaskId::new(2)).unwrap().task.status(),
        TaskStatus::Completed
    );
    // The parked attempts were before the landing; the reason is cleared.
    assert!(landed.last_error().is_none());
    let detail = queue.show(TaskId::new(2)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "integration_deferred")
            .count(),
        3
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "integration_started")
            .count(),
        4
    );
    assert_eq!(kinds.iter().filter(|k| **k == "run_integrated").count(), 1);
}

/// Two tasks rewrite `change.txt`: the first lands, and a person's
/// `integrate` of the second (its approval) conflicts and parks it as
/// `needs_session`. Returns the parked run and the landed main.
fn parked_conflict(repo: &Path, db: &Path, backend: &TestWorkspace) -> (TaskRun, String) {
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

fn payloads<'a>(detail: &'a dagq::domain::TaskDetail, kind: &str) -> Vec<&'a Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| &e.payload)
        .collect()
}

/// The supervisor resumes a `needs_session` run whose `integrate` was
/// called (ADR-0019 decision 1): it opens a workspace named like the worker's
/// with the worker's wrapper, types the resolution request, sends `/exit`
/// once the session rewrote its receipt for the worktree head and went
/// idle, closes the workspace and lands the run. The first attempt only
/// rewrites the receipt, so the landing conflicts again and the run comes
/// back for a second attempt, which resolves it.
#[test]
fn approved_needs_session_run_is_resumed_until_the_runtime_lands_it() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let reason = run.last_error().unwrap().to_owned();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(
        payloads(&detail, "integration_approved"),
        [&json!({"status": "awaiting_integration", "pid": std::process::id(), "push": true})]
    );
    // The inbox is told the runtime takes it from here.
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        run_attention_of(&status, run.id()).unwrap()["next"],
        "resuming (runtime)"
    );

    backend.resume_script_for(
        2,
        "await_message; mark=\"$(dirname \"$RECEIPT\")/attempted\"; if [ -f \"$mark\" ]; then resolve; else : > \"$mark\"; fi; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = queue.show(TaskId::new(2)).unwrap();
    let landed = detail.runs[0].clone();
    assert_landed(&repo, &landed, "second", &first_landed);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "resolved by the resumed session\n"
    );
    assert!(queue.run_leases().unwrap().is_empty());
    let started = payloads(&detail, "resume_started");
    assert_eq!(started.len(), 2, "{:?}", event_kinds(&detail));
    assert_eq!(
        started[0],
        &json!({"attempt": 1, "reason": reason, "main": first_landed})
    );
    assert_eq!(started[1]["attempt"], 2);
    assert!(
        started[1]["reason"]
            .as_str()
            .unwrap()
            .contains("conflicted in change.txt")
    );
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 2);
    for (index, payload) in finished.iter().enumerate() {
        assert_eq!(payload["attempt"], index + 1);
        assert_eq!(payload["outcome"], "resolved");
        assert_eq!(payload["status"], "needs_session");
        assert_eq!(payload["approved"], true);
        assert_eq!(payload["workspace_closed"], true);
    }
    assert_eq!(finished[0]["head"], json!(run.result_commit()));
    assert_eq!(
        finished[1]["head"],
        json!(git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", run.id())]
        ))
    );
    // Each resume ends before its landing starts; one landing conflicted.
    let kinds = event_kinds(&detail);
    let resumed = kinds
        .iter()
        .skip_while(|k| **k != "resume_started")
        .filter(|k| {
            matches!(
                **k,
                "resume_started"
                    | "resume_finished"
                    | "integration_started"
                    | "integration_deferred"
                    | "run_integrated"
            )
        })
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(
        resumed,
        [
            "resume_started",
            "resume_finished",
            "integration_started",
            "integration_deferred",
            "resume_started",
            "resume_finished",
            "integration_started",
            "run_integrated",
        ]
    );
    // Same wrapper and runtime snapshot as the worker, under the resume name.
    let resumes = backend.resumes.lock().unwrap().clone();
    assert_eq!(resumes.len(), 2);
    let run_dir = Path::new(run.run_dir().unwrap());
    for (name, command) in &resumes {
        assert_eq!(name, "[repo's directory]worker#2 - second");
        assert!(
            command.contains(&shell_join(&[
                "session".into(),
                "--run".into(),
                run.id().to_string()
            ])),
            "{command}"
        );
        assert!(
            command.starts_with(&shell_join(&[run_dir
                .join("runner")
                .to_string_lossy()
                .into_owned()])),
            "{command}"
        );
    }
    let texts = backend.texts();
    assert_eq!(texts.len(), 2);
    let text = &texts[0].1;
    for expected in [
        format!(
            "dagq: integrate could not land run {} (task 2) and returned needs_session.",
            run.id()
        ),
        format!("Reason: {reason}"),
        format!(
            "main is now {first_landed} (your base commit was {}).",
            run.base_commit()
        ),
        "Tasks landed on main since your base:\n- task 1: test task; summary: done".to_owned(),
        format!("git rebase {first_landed}"),
        "[\"test -f seed.txt\"]".to_owned(),
        format!(
            "Rewrite the receipt at {} with the new head commit",
            run.receipt_path().unwrap()
        ),
        "result failed".to_owned(),
        format!("4. {}", runtime::STOP_BACKGROUND),
        "Do not merge or push. When done, report briefly and stop; do not run /exit.".to_owned(),
    ] {
        assert!(text.contains(&expected), "{expected:?} not in {text}");
    }
    assert_eq!(
        &fs::read_to_string(run_dir.join("resume-1.txt")).unwrap(),
        text
    );
    assert!(run_dir.join("terminal-resume-2.txt").is_file());
    // /exit once per resumed session; both resume workspaces were closed.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 2);
    let closed = backend.closed();
    assert!(
        texts
            .iter()
            .all(|(workspace, _)| closed.contains(workspace))
    );
    // Nothing waits for a person: the watch sees no attention.
    assert_eq!(
        dagq::watch::events(&db, EventId::new(cursor), 100, false).unwrap()["events"],
        json!([])
    );
    assert!(run_attention_of(&runtime::status(&db).unwrap(), run.id()).is_none());
}

/// A `needs_session` run that no `integrate` approved (here standing in for
/// validation's `evidence_missing`) is resumed with the evidence request
/// and, resolved, keeps its resumed session open through validation and the
/// supervisor's review like the worker's (ADR-0027 decision 3). The
/// stand-in `claude` prints no verdict, so the review is retried, fails,
/// and the run waits in an `approve_landing` ask.
#[test]
fn unapproved_resumed_run_is_validated_and_reviewed_with_its_session_open() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let mut queue = SqliteQueue::open(&db).unwrap();
    Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM run_events WHERE run_id=?1 AND kind='integration_approved'",
            [&run.id()],
        )
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            "evidence_missing",
            json!({"status": "needs_session", "reason": "e2e has no evidence"}),
        )
        .unwrap();
    backend.resume_script_for(
        2,
        "await_message; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");

    let detail = queue.show(TaskId::new(2)).unwrap();
    let back = detail.runs[0].clone();
    assert_eq!(back.status(), RunStatus::AwaitingIntegration);
    assert_eq!(back.result_commit(), run.result_commit());
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["outcome"], "resolved");
    assert_eq!(finished[0]["status"], "validating");
    assert_eq!(finished[0]["approved"], false);
    assert_eq!(finished[0]["workspace_closed"], false);
    assert_eq!(finished[0]["session_live"], true);
    let resume_workspace = finished[0]["workspace_id"].as_str().unwrap().to_owned();
    let kinds = event_kinds(&detail);
    let after: Vec<&str> = kinds
        .iter()
        .skip_while(|k| **k != "resume_finished")
        .filter(|k| {
            matches!(
                **k,
                "validation_finished"
                    | "review_started"
                    | "review_retried"
                    | "exit_requested"
                    | "session_exited"
                    | "workspace_closed"
                    | "review_failed"
            )
        })
        .copied()
        .collect();
    assert_eq!(
        after,
        [
            "validation_finished",
            "review_started",
            "review_retried",
            "review_started",
            "exit_requested",
            "session_exited",
            "workspace_closed",
            "review_failed",
        ]
    );
    assert!(backend.closed().contains(&resume_workspace));
    assert_eq!(
        payloads(&detail, "review_started").last().unwrap()["workspace_id"],
        json!(resume_workspace)
    );
    assert!(
        !event_kinds(&detail).contains(&"integration_started") || {
            // Only the two landings by hand before the resume.
            payloads(&detail, "integration_started").len() == 1
        }
    );
    let text = &backend.texts()[0].1;
    assert!(
        text.contains("found required evidence missing from the receipt"),
        "{text}"
    );
    assert!(text.contains("Reason: e2e has no evidence"), "{text}");
    assert!(
        text.contains("Run the checks the reason names as missing"),
        "{text}"
    );
    assert!(!text.contains("git rebase"), "{text}");
    assert!(text.contains(runtime::STOP_BACKGROUND), "{text}");
    // The inbox is woken only by the ask of the failed review (task 328).
    let events = dagq::watch::events(&db, EventId::new(cursor), 100, false).unwrap();
    assert_eq!(events["events"].as_array().unwrap().len(), 1, "{events}");
    assert_eq!(events["events"][0]["kind"], "ask_opened", "{events}");
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    // Integrating it now is the approval.
    assert_eq!(
        integrate(&db, 2, &repo).unwrap()["outcome"],
        "needs_session"
    );
    assert_eq!(
        payloads(&queue.show(TaskId::new(2)).unwrap(), "integration_approved"),
        [&json!({"status": "awaiting_integration", "pid": std::process::id(), "push": true})]
    );
}

/// Stand in for an earlier resume the supervisor judged `unresolved`
/// (task 122): one `resume_started` / `resume_finished` pair under another
/// token after the run was parked.
fn unresolved_attempt(db: &Path, run: &TaskRun, main: &str) {
    let mut queue = SqliteQueue::open(db).unwrap();
    let (_, attempt) = queue
        .begin_resume(run.id(), "earlier", &sha(main), None, 3)
        .unwrap()
        .unwrap();
    assert_eq!(attempt, 1);
    queue
        .finish_resume(
            run.id(),
            "earlier",
            None,
            None,
            false,
            json!({"attempt": 1, "outcome": "unresolved", "exhausted": false}),
        )
        .unwrap();
}

/// Resolve the parked conflict in the run's worktree on top of `main` as
/// that session did, and return the new head.
fn resolve_in_worktree(run: &TaskRun, main: &str) -> String {
    let worktree = Path::new(run.worktree_path().unwrap());
    let rebase = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rebase", main])
        .bounded_output()
        .unwrap();
    assert!(!rebase.status.success());
    fs::write(worktree.join("change.txt"), "resolved by the session\n").unwrap();
    git(worktree, &["add", "change.txt"]);
    let status = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .env("GIT_EDITOR", "true")
        .args(["rebase", "--continue"])
        .bounded_status()
        .unwrap();
    assert!(status.success());
    git_out(worktree, &["rev-parse", "HEAD"])
}

/// A parked run whose earlier resume already rebased it onto main and
/// rewrote the receipt for its clean head (judged `unresolved` all the
/// same): the supervisor opens no session and uses no attempt, records
/// `resume_skipped` and, its integrate approved, lands it. The backend has
/// no resume script, so a resume would have failed.
#[test]
fn an_approved_run_resolved_by_an_earlier_resume_lands_without_a_session() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    unresolved_attempt(&db, &run, &first_landed);
    let resolved = resolve_in_worktree(&run, &first_landed);
    write_receipt(&run, &resolved, "succeeded", "resolved");

    let mut queue = SqliteQueue::open(&db).unwrap();
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert!(backend.resumes.lock().unwrap().is_empty());
    assert!(backend.texts().is_empty());
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    assert_eq!(
        payloads(&detail, "resume_skipped"),
        [
            &json!({"head": resolved, "main": first_landed, "approved": true, "status": "needs_session"})
        ]
    );
    let kinds = event_kinds(&detail);
    let after: Vec<&str> = kinds
        .iter()
        .skip_while(|k| **k != "resume_finished")
        .filter(|k| {
            matches!(
                **k,
                "resume_finished"
                    | "resume_skipped"
                    | "resume_started"
                    | "integration_started"
                    | "run_integrated"
            )
        })
        .copied()
        .collect();
    assert_eq!(
        after,
        [
            "resume_finished",
            "resume_skipped",
            "integration_started",
            "run_integrated"
        ]
    );
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(
        dagq::watch::events(&db, EventId::new(cursor), 100, false).unwrap()["events"],
        json!([])
    );
}

/// The same run without an approving `integrate` is validated and reviewed
/// without a session: the stand-in `claude` prints no verdict, so it waits
/// in `awaiting_integration` for a person's answer to the `approve_landing`
/// ask of its failed review.
#[test]
fn an_unapproved_run_resolved_by_an_earlier_resume_is_validated_without_a_session() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM run_events WHERE run_id=?1 AND kind='integration_approved'",
            [&run.id()],
        )
        .unwrap();
    unresolved_attempt(&db, &run, &first_landed);
    let resolved = resolve_in_worktree(&run, &first_landed);
    write_receipt(&run, &resolved, "succeeded", "resolved");

    let mut queue = SqliteQueue::open(&db).unwrap();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = queue.show(TaskId::new(2)).unwrap();
    let back = &detail.runs[0];
    assert_eq!(back.status(), RunStatus::AwaitingIntegration);
    assert_eq!(
        back.result_commit().map(CommitSha::as_str),
        Some(resolved.as_str())
    );
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);
    assert!(backend.resumes.lock().unwrap().is_empty());
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    assert_eq!(
        payloads(&detail, "resume_skipped"),
        [
            &json!({"head": resolved, "main": first_landed, "approved": false, "status": "validating"})
        ]
    );
    let kinds = event_kinds(&detail);
    let after: Vec<&str> = kinds
        .iter()
        .skip_while(|k| **k != "resume_skipped")
        .filter(|k| {
            matches!(
                **k,
                "resume_skipped"
                    | "validation_finished"
                    | "review_started"
                    | "review_retried"
                    | "review_failed"
            )
        })
        .copied()
        .collect();
    // The unreadable verdict is reviewed once more (task 328).
    assert_eq!(
        after,
        [
            "resume_skipped",
            "validation_finished",
            "review_started",
            "review_retried",
            "review_started",
            "review_failed"
        ]
    );
    assert_eq!(
        payloads(&detail, "review_started").last().unwrap()["workspace_id"],
        Value::Null
    );
    assert!(queue.run_leases().unwrap().is_empty());
}

/// A skipped run the landing parks again (its verification fails on the
/// resolved head) is resumed with a session next, not skipped again.
#[test]
fn a_run_parked_again_after_a_skip_is_resumed_not_skipped() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    unresolved_attempt(&db, &run, &first_landed);
    let resolved = resolve_in_worktree(&run, &first_landed);
    write_receipt(&run, &resolved, "succeeded", "resolved");
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands='[\"false\"]' WHERE id=2",
            [],
        )
        .unwrap();

    // The resumes after the second park fail (the backend has no script)
    // until the attempts are used up; none is skipped. Whether one pass of
    // `--once` makes both depends on whether it reads the run parked again
    // before it reaps the landing's slot, so passes are run until the last
    // attempt started (each pass makes at least one).
    let mut queue = SqliteQueue::open(&db).unwrap();
    let mut errors = Vec::new();
    for _ in 0..MAX_RESUME_ATTEMPTS {
        let outcome = supervise(&db, &repo, &backend).unwrap();
        errors.extend(outcome["errors"].as_array().unwrap().iter().cloned());
        let detail = queue.show(TaskId::new(2)).unwrap();
        if payloads(&detail, "resume_started")
            .last()
            .is_some_and(|p| p["attempt"] == MAX_RESUME_ATTEMPTS)
        {
            break;
        }
    }
    assert_eq!(errors.len(), 2, "{errors:?}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);
    assert_eq!(payloads(&detail, "resume_skipped").len(), 1);
    let kinds = event_kinds(&detail);
    let after: Vec<&str> = kinds
        .iter()
        .skip_while(|k| **k != "resume_skipped")
        .filter(|k| {
            matches!(
                **k,
                "resume_skipped" | "integration_deferred" | "resume_started" | "resume_finished"
            )
        })
        .copied()
        .collect();
    assert_eq!(
        after,
        [
            "resume_skipped",
            "integration_deferred",
            "resume_started",
            "resume_finished",
            "resume_started",
            "resume_finished"
        ]
    );
    assert_eq!(
        payloads(&detail, "resume_started").last().unwrap()["attempt"],
        3
    );
}

/// An unapproved run a supervisor moved on by `resume_skipped` and then
/// died holding (no session, no wrapper registration since) is adopted by
/// the next supervisor and validated and reviewed with no session.
#[test]
fn a_skipped_run_whose_supervisor_died_is_adopted() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM run_events WHERE run_id=?1 AND kind='integration_approved'",
            [&run.id()],
        )
        .unwrap();
    // The attempt clears the worker's process rows: no wrapper is left.
    unresolved_attempt(&db, &run, &first_landed);
    let resolved = resolve_in_worktree(&run, &first_landed);
    write_receipt(&run, &resolved, "succeeded", "resolved");
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.processes(run.id()).unwrap().is_empty());
    let skipped = queue
        .skip_resume(
            run.id(),
            "dead",
            &sha(&resolved),
            &sha(&first_landed),
            false,
        )
        .unwrap()
        .unwrap();
    assert_eq!(skipped.status(), RunStatus::Validating);
    // Taken already: a second skip or resume finds it leased.
    assert!(
        queue
            .skip_resume(
                run.id(),
                "other",
                &sha(&resolved),
                &sha(&first_landed),
                false
            )
            .unwrap()
            .is_none()
    );
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET pid=?2 WHERE run_id=?1",
            rusqlite::params![run.id(), dead_pid()],
        )
        .unwrap();

    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(backend.resumes.lock().unwrap().is_empty());
    let adopted = payloads(&detail, "run_adopted");
    assert_eq!(adopted.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(adopted[0]["wrapper"], Value::Null);
    assert_eq!(
        payloads(&detail, "review_started").last().unwrap()["workspace_id"],
        Value::Null
    );
    assert!(queue.run_leases().unwrap().is_empty());
}

/// Takes one condition of the skip away from a resolved run (the repo, the
/// queue, the run and its resolved head).
type Spoil = fn(&Path, &Path, &TaskRun, &str);

/// A parked run lacking any one condition of the skip is resumed as
/// before: the resume uses an attempt (and fails here, the backend having
/// no resume script) and no `resume_skipped` is recorded. `resumed` says
/// whether an unresolved resume came before. One test per condition, so
/// the conditions run in parallel (task 324: the eight in one test took
/// over a minute).
fn a_run_missing_a_condition_of_the_skip_is_resumed(case: &str, resumed: bool, spoil: Spoil) {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    if case == "a person sent it back" {
        SqliteQueue::open(&db)
            .unwrap()
            .record_runtime_event(
                run.id(),
                "landing_decided",
                json!({"status": "needs_session", "reason": "findings sent back"}),
            )
            .unwrap();
    }
    if resumed {
        unresolved_attempt(&db, &run, &first_landed);
    }
    let resolved = resolve_in_worktree(&run, &first_landed);
    write_receipt(&run, &resolved, "succeeded", "resolved");
    spoil(&repo, &db, &run, &resolved);

    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(
        outcome["errors"].as_array().unwrap().len(),
        1,
        "{case}: {outcome}"
    );
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert!(
        payloads(&detail, "resume_skipped").is_empty(),
        "{case}: {:?}",
        event_kinds(&detail)
    );
    let started = payloads(&detail, "resume_started");
    assert_eq!(started.len(), usize::from(resumed) + 1, "{case}");
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.last().unwrap()["outcome"], "error", "{case}");
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession, "{case}");
}

#[test]
fn a_run_not_resumed_since_it_was_parked_is_resumed() {
    a_run_missing_a_condition_of_the_skip_is_resumed(
        "no resume since it was parked",
        false,
        |_, _, _, _| {},
    );
}

#[test]
fn a_run_a_person_sent_back_is_resumed() {
    // Recorded before the unresolved attempt.
    a_run_missing_a_condition_of_the_skip_is_resumed(
        "a person sent it back",
        true,
        |_, _, _, _| {},
    );
}

#[test]
fn a_run_whose_receipt_names_the_old_head_is_resumed() {
    a_run_missing_a_condition_of_the_skip_is_resumed(
        "the receipt names the old head",
        true,
        |_, _, run, _| {
            write_receipt(
                run,
                run.result_commit().unwrap().as_str(),
                "succeeded",
                "stale",
            );
        },
    );
}

#[test]
fn a_run_with_a_dirty_worktree_is_resumed() {
    a_run_missing_a_condition_of_the_skip_is_resumed(
        "the worktree is dirty",
        true,
        |_, _, run, _| {
            let worktree = Path::new(run.worktree_path().unwrap());
            fs::write(worktree.join("stray.txt"), "left over\n").unwrap();
        },
    );
}

#[test]
fn a_run_main_moved_past_is_resumed() {
    a_run_missing_a_condition_of_the_skip_is_resumed(
        "main moved past the head",
        true,
        |repo, _, _, _| {
            git(repo, &["commit", "-q", "--allow-empty", "-m", "moved on"]);
        },
    );
}

#[test]
fn a_run_with_another_runs_receipt_is_resumed() {
    a_run_missing_a_condition_of_the_skip_is_resumed(
        "the receipt is another run's",
        true,
        |_, _, run, resolved| {
            let mut receipt = session_receipt(run, resolved, "succeeded", "resolved");
            receipt["run_id"] = json!("another-run");
            write_receipt_json(run, receipt);
        },
    );
}

#[test]
fn a_run_whose_receipt_reports_failed_is_resumed() {
    a_run_missing_a_condition_of_the_skip_is_resumed(
        "the receipt reports failed",
        true,
        |_, _, run, resolved| {
            write_receipt(run, resolved, "failed", "gave up");
        },
    );
}

#[test]
fn a_run_missing_the_required_evidence_is_resumed() {
    a_run_missing_a_condition_of_the_skip_is_resumed(
        "the required evidence is missing",
        true,
        |_, db, run, _| {
            Connection::open(db)
                .unwrap()
                .execute(
                    "UPDATE tasks SET required_evidence='[\"e2e\"]' WHERE id=?1",
                    [run.task_id()],
                )
                .unwrap();
        },
    );
}

/// A resume that cannot start, or a session that cannot resolve the run,
/// uses up an attempt; after the third the supervisor stops resuming it and
/// hands it to a person (ADR-0024's Consequences): the run becomes `failed`
/// with a `decide` ask for the inbox (`retry` or `cancel`), recorded as the
/// runtime's `triage_finished` so no headless triage runs, and the answer is
/// applied like a triage's. The sessions behave like Claude: they
/// never exit by themselves, so the supervisor sends `/exit` once when one
/// goes idle without a resolving receipt, or when one never goes idle within
/// the resume timeout.
#[test]
fn resuming_stops_after_three_attempts() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.resume_timeout = Duration::from_secs(1);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let reason = run.last_error().unwrap().to_owned();

    // No resume script: the workspace cannot be opened.
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"].as_array().unwrap().len(), 1, "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["outcome"], "error");
    assert_eq!(finished[0]["exhausted"], false);
    // The failed cmux call itself is recorded on the run (task 109).
    let failures = backend_failures(&detail);
    assert_eq!(failures.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(failures[0].run_id.as_ref(), Some(run.id()));
    assert_eq!(failures[0].payload["op"], "create_resume");
    assert!(
        finished[0]["error"]
            .as_str()
            .unwrap()
            .contains("no resume script")
    );
    assert_eq!(detail.runs[0].last_error(), Some(reason.as_str()));
    assert!(queue.run_leases().unwrap().is_empty());

    // The second attempt answers without resolving and goes idle; the third
    // never goes idle. Neither exits until it is asked to.
    backend.resume_script_for(
        2,
        "await_message; mark=\"$(dirname \"$RECEIPT\")/went-idle\"; if [ ! -f \"$mark\" ]; then : > \"$mark\"; idle; fi; await_exit",
    );
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert_eq!(
        detail.runs[0].last_error(),
        Some(format!("resumed 3 times (at most 3) and still needs a session: {reason}").as_str())
    );
    let started = payloads(&detail, "resume_started");
    assert_eq!(
        started
            .iter()
            .map(|p| p["attempt"].clone())
            .collect::<Vec<_>>(),
        [json!(1), json!(2), json!(3)]
    );
    assert!(started.iter().all(|p| p["reason"] == json!(reason)));
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 3);
    assert_eq!(finished[1]["outcome"], "unresolved");
    assert_eq!(finished[1]["exhausted"], false);
    assert_eq!(finished[2]["outcome"], "unresolved");
    assert_eq!(finished[2]["exhausted"], true);
    assert_eq!(finished[2]["status"], "needs_session");
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 2);
    assert_eq!(backend.closed().len(), 2 + 2); // two workers, two resumes
    // The used-up run goes to the inbox as the triage's `decide` ask, and
    // no headless triage runs for it.
    let asks = other_asks(&mut queue, false);
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::Decide);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.asked_by, "supervisor");
    assert_eq!(ask.options, ["retry", "cancel"]);
    assert!(
        ask.question
            .contains("was resumed 3 times (at most 3) and still needs a session"),
        "{}",
        ask.question
    );
    assert!(ask.question.contains(&reason), "{}", ask.question);
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["by"], "runtime");
    assert_eq!(finished[0]["action"], "ask");
    assert_eq!(finished[0]["ask_id"], json!(ask.id));
    assert_eq!(finished[0]["previous_status"], "needs_session");
    assert_eq!(finished[0]["status"], "failed");
    assert!(payloads(&detail, "triage_started").is_empty());
    // The ask is the one attention; the exhausted resume is none.
    let events = dagq::watch::events(&db, EventId::new(cursor), 100, false).unwrap();
    let listed = events["events"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{events}");
    assert_eq!(listed[0]["kind"], "ask_opened");
    assert_eq!(listed[0]["next"], format!("answer ask {}", ask.id));
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    // No fourth attempt, no second ask.
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert_eq!(
        payloads(&queue.show(TaskId::new(2)).unwrap(), "resume_started").len(),
        3
    );
    assert_eq!(other_asks(&mut queue, false).len(), 1);

    // The person cancels the task; the supervisor applies it.
    queue.answer(ask.id, "cancel").unwrap();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    assert_eq!(
        payloads(&detail, "triage_decided")[0]["answer"],
        json!("cancel")
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
}

/// A resumed session that does not exit within the exit timeout of `/exit`
/// is let go as `unresolved` (its lease released, its workspace kept), so the
/// supervisor's slot and a drain are not held forever. While it runs, the run
/// is the supervisor's (`resuming (runtime)`) and is not resumed again; once it
/// ended, the next pass closes the workspace it left and resumes the run.
/// Task 285: the resolution request waits for Claude Code's input box. A
/// booting session's screen gets nothing; past the registration timeout
/// the run records `input_not_ready` and asks the inbox once, and the
/// request goes as soon as the box is drawn. The ask closes when the
/// session exits.
#[test]
fn a_resumed_session_gets_its_request_only_once_its_input_box_is_ready() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.registration_timeout = Duration::from_secs(2);
    *backend.screen.lock().unwrap() = BOOT_SCREEN.into();
    let mut queue = SqliteQueue::open(&db).unwrap();
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = thread::scope(|scope| {
        scope.spawn(|| {
            let started = Instant::now();
            // Nothing is typed while the session boots.
            while started.elapsed() < Duration::from_secs(4) {
                assert!(backend.texts().is_empty());
                thread::sleep(Duration::from_millis(20));
            }
            *backend.screen.lock().unwrap() = READY_SCREEN.into();
        });
        supervise(&db, &repo, &backend).unwrap()
    });
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(backend.texts().len(), 1);
    let not_ready = payloads(&detail, "input_not_ready");
    assert_eq!(not_ready.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(not_ready[0]["waited_secs"], 2);
    assert_eq!(not_ready[0]["prompt"], Value::Null);
    assert!(
        not_ready[0]["excerpt"]
            .as_str()
            .unwrap()
            .contains("'session'")
    );
    let asks = other_asks(&mut queue, true);
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::AnswerPrompt);
    assert_eq!(asks[0].run_id.as_ref(), Some(run.id()));
    assert!(
        asks[0].question.contains("input box is not ready"),
        "{}",
        asks[0].question
    );
    assert_eq!(
        asks[0].answer.as_deref(),
        Some("the input box got ready and the request was sent; closed by the runtime")
    );
    assert!(payloads(&detail, "submit_retried").is_empty());
}

/// Task 285: a request whose Enter a long paste swallowed stays in the
/// input box; Enter alone goes again, the text is typed once, and the run
/// goes on as usual.
#[test]
fn a_request_left_in_the_input_box_gets_enter_again_not_the_text() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (_run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.swallowed_enters.store(2, Ordering::SeqCst);
    let mut queue = SqliteQueue::open(&db).unwrap();
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(backend.texts().len(), 1);
    assert_eq!(backend.enters.load(Ordering::SeqCst), 2);
    let retried = payloads(&detail, "submit_retried");
    assert_eq!(retried.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(retried[0]["what"], "resolution request");
    assert_eq!(retried[0]["input"], "text");
    assert_eq!(retried[0]["retries"], 2);
    assert_eq!(retried[0]["submitted"], true);
    assert!(payloads(&detail, "submit_unconfirmed").is_empty());
    assert!(other_asks(&mut queue, true).is_empty());
}

/// Task 285: a request still in the input box after the Enters sent again
/// is recorded and raised to the inbox as an `answer_prompt` ask; `/exit`
/// left there gets Enter again too but is never typed twice.
#[test]
fn a_request_stuck_in_the_input_box_is_asked_to_the_inbox() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    backend.swallowed_enters.store(1000, Ordering::SeqCst);
    let mut queue = SqliteQueue::open(&db).unwrap();
    // The fake session still reads the request, so the run resolves.
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    let unconfirmed = payloads(&detail, "submit_unconfirmed");
    assert_eq!(unconfirmed.len(), 2, "{:?}", event_kinds(&detail));
    assert_eq!(unconfirmed[0]["input"], "text");
    assert_eq!(unconfirmed[0]["retries"], 3);
    assert!(
        unconfirmed[0]["excerpt"]
            .as_str()
            .unwrap()
            .contains("do not run /exit")
    );
    assert_eq!(unconfirmed[1]["input"], "exit");
    // Three Enters after the request and three after /exit, sent once.
    assert_eq!(backend.enters.load(Ordering::SeqCst), 6);
    assert_eq!(backend.texts().len(), 1);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let asks = other_asks(&mut queue, true);
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::AnswerPrompt);
    assert_eq!(asks[0].run_id.as_ref(), Some(run.id()));
    assert!(
        asks[0].question.contains(
            "resolution request the supervisor typed stays in the input box after 4 Enters"
        ),
        "{}",
        asks[0].question
    );
    assert!(asks[0].closed_at.is_some());
}

/// Task 285: a request the session never got (typed into a box that lost
/// it) shows no sign of work within `start_wait`; with the input box empty
/// it is sent once more, and the run goes on without waiting out the
/// resume timeout.
#[test]
fn a_lost_request_is_sent_again_after_no_sign_of_work() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (_run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.start_wait = Duration::from_secs(1);
    backend.dropped_texts.store(1, Ordering::SeqCst);
    let mut queue = SqliteQueue::open(&db).unwrap();
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    let texts = backend.texts();
    assert_eq!(texts.len(), 2);
    assert_eq!(texts[0], texts[1]);
    let resent = payloads(&detail, "submit_resent");
    assert_eq!(resent.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(resent[0]["what"], "resolution request");
    assert_eq!(resent[0]["waited_secs"], 1);
    assert!(payloads(&detail, "submit_not_started").is_empty());
    assert!(other_asks(&mut queue, true).is_empty());
}

/// Task 285: a request lost twice is not sent a third time: the run
/// records `submit_not_started` and asks the inbox.
#[test]
fn a_request_lost_twice_is_asked_to_the_inbox() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    backend.start_wait = Duration::from_secs(1);
    backend.resume_timeout = Duration::from_secs(4);
    backend.dropped_texts.store(usize::MAX, Ordering::SeqCst);
    let mut queue = SqliteQueue::open(&db).unwrap();
    // It never gets the request, and exits at the /exit of the resume timeout.
    backend.resume_script_for(2, "await_exit");
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    // Each resume (three, all unresolved) sends it twice and asks once.
    let resumes = payloads(&detail, "resume_started").len();
    assert_eq!(resumes, 3, "{:?}", event_kinds(&detail));
    assert_eq!(backend.texts().len(), 2 * resumes);
    let not_started = payloads(&detail, "submit_not_started");
    assert_eq!(not_started.len(), resumes, "{:?}", event_kinds(&detail));
    assert!(not_started.iter().all(|p| p["resent"] == true));
    assert_eq!(payloads(&detail, "submit_resent").len(), resumes);
    let asks = queue
        .asks(AskQuery {
            all: true,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .filter(|a| a.kind == AskKind::AnswerPrompt)
        .collect::<Vec<_>>();
    assert_eq!(asks.len(), resumes, "{asks:?}");
    assert_eq!(asks[0].run_id.as_ref(), Some(run.id()));
    // Each closes once its session exited.
    assert!(asks.iter().all(|a| a.closed_at.is_some()));
    assert!(
        asks[0].question.contains(
            "showed no sign of work within 1s of the resolution request the supervisor sent twice"
        ),
        "{}",
        asks[0].question
    );
}

#[test]
fn a_resumed_session_that_ignores_exit_is_let_go() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.exit_timeout = Duration::from_secs(1);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let mut queue = SqliteQueue::open(&db).unwrap();
    backend.resume_script_for(2, &format!("await_message; idle; {HOLD}"));
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["outcome"], "unresolved");
    assert_eq!(finished[0]["exit_timed_out"], true);
    assert_eq!(finished[0]["workspace_closed"], false);
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let kept = finished[0]["workspace_id"].as_str().unwrap().to_owned();
    assert!(!backend.closed().contains(&kept));
    assert_eq!(
        run_attention_of(&runtime::status(&db).unwrap(), run.id()).unwrap()["next"],
        "resuming (runtime)"
    );
    // Its dialog does not go away by itself: one stuck_exit ask goes to the
    // inbox (task 147), as for the worker's session.
    let asks = other_asks(&mut queue, false);
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::StuckExit);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert!(ask.question.contains(&kept), "{}", ask.question);
    assert!(
        ask.question.contains(
            "The run stays needs_session, and the supervisor resumes it again once the session exits"
        ),
        "{}",
        ask.question
    );
    // A pass while the session still runs neither resumes the run nor asks
    // again, nor closes the ask.
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert_eq!(other_asks(&mut queue, false).len(), 1);
    assert!(queue.read_ask(ask.id).unwrap().is_open());

    // Its session ends; the workspace it left no longer blocks the run.
    release_held_session(run.run_dir().unwrap());
    backend.join();
    assert_eq!(
        run_attention_of(&runtime::status(&db).unwrap(), run.id()).unwrap()["next"],
        "resuming (runtime)"
    );
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(backend.closed().contains(&kept));
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(payloads(&detail, "resume_started").len(), 2);
    // The next pass closed the ask of the session that ended.
    let closed = queue.read_ask(ask.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(
        closed.answer.as_deref(),
        Some("the session exited; closed by the runtime")
    );
    assert!(other_asks(&mut queue, false).is_empty());
    // One ask about the session, besides the one of the run's failed
    // stand-in review before it was integrated by hand (task 328).
    let opened: Vec<&Value> = payloads(&detail, "ask_opened")
        .into_iter()
        .filter(|p| p["kind"] != "approve_landing")
        .collect();
    assert_eq!(opened.len(), 1, "{opened:?}");
}

/// The supervisor never resumes a run next to the live session of a
/// supervisor that died mid-resume: the run shows as `resuming (runtime)`,
/// keeps a person's `integrate` out, and once that session exited the next
/// supervisor resumes and lands the run.
#[test]
fn a_session_nobody_watches_blocks_the_resume_until_it_ends() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let mut queue = SqliteQueue::open(&db).unwrap();
    // A supervisor started a resume, its session registered, and then the
    // supervisor died: its lease goes stale while the session lives on.
    let (_, attempt) = queue
        .begin_resume(run.id(), "dead-supervisor", &sha(&first_landed), None, 3)
        .unwrap()
        .unwrap();
    assert_eq!(attempt, 1);
    assert!(
        queue
            .begin_resume(run.id(), "another", &sha(&first_landed), None, 3)
            .unwrap()
            .is_none()
    );
    queue
        .register_resume_wrapper(run.id(), "dead-supervisor", std::process::id())
        .unwrap();
    assert!(
        queue
            .register_resume_wrapper(run.id(), "dead-supervisor", std::process::id())
            .is_err()
    );
    age_lease(&db, &run, 60);
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        run_attention_of(&status, run.id()).unwrap()["next"],
        "resuming (runtime)"
    );
    let refused = integrate(&db, 2, &repo).unwrap_err();
    assert!(
        format!("{refused:#}").contains("run is still leased"),
        "{refused:#}"
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert_eq!(
        payloads(&queue.show(TaskId::new(2)).unwrap(), "resume_started").len(),
        1
    );

    // Its session ends; the next supervisor takes the stale lease over.
    queue
        .wrapper_exited(run.id(), std::process::id(), 0)
        .unwrap();
    assert_eq!(
        run_attention_of(&runtime::status(&db).unwrap(), run.id()).unwrap()["next"],
        "resuming (runtime)"
    );
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    let started = payloads(&detail, "resume_started");
    assert_eq!(
        started.len(),
        2,
        "{:?}",
        detail
            .events
            .iter()
            .map(|e| (&e.kind, &e.payload))
            .collect::<Vec<_>>()
    );
    assert_eq!(started[1]["attempt"], 2);
    let acquired = detail
        .events
        .iter()
        .filter(|e| e.kind == "lease_acquired" && e.payload["reason"] == "resume")
        .map(|e| e.payload["previous_token"].clone())
        .collect::<Vec<_>>();
    assert_eq!(acquired, [json!(null), json!("dead-supervisor")]);
}

/// A resumed session that finds the change no longer needed writes a failed
/// receipt; the run ends `failed` without landing.
#[test]
fn resumed_session_with_a_failed_receipt_fails_the_run() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        "await_message; receipt \"$(git rev-parse HEAD)\" failed 'already on main'; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "failed", "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert_eq!(
        detail.runs[0].last_error(),
        Some("session reported the run as failed: already on main")
    );
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished[0]["outcome"], "failed");
    assert_eq!(finished[0]["status"], "failed");
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);
    assert!(Path::new(run.worktree_path().unwrap()).exists());
    // The failed run goes to the triage; the stub `claude` prints no
    // verdict, so the triage fails and the run waits for a person.
    let failed = payloads(&detail, "triage_failed");
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("no verdict JSON"),
        "{}",
        failed[0]
    );
    assert_eq!(
        run_attention_of(&runtime::status(&db).unwrap(), run.id()).unwrap()["next"],
        "triage by hand"
    );
}

/// A session that finds the change no longer needed writes a failed receipt
/// with the reason; the run ends without touching main and the task can be
/// retried or canceled.
#[test]
fn failed_receipt_from_a_session_ends_the_run_without_landing() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let main = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(
        integrate(&db, 2, &repo).unwrap()["outcome"],
        "needs_session"
    );
    let run = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    write_receipt(
        &run,
        run.result_commit().map(CommitSha::as_str).unwrap(),
        "failed",
        "already covered by task 1",
    );
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "failed", "{outcome}");
    let reason = outcome["reason"].as_str().unwrap();
    assert!(reason.contains("already covered by task 1"), "{reason}");
    let failed = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(failed.status(), RunStatus::Failed);
    assert_eq!(failed.last_error(), Some(reason));
    assert!(Path::new(failed.worktree_path().unwrap()).exists());
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main);
    assert!(
        git_out(&repo, &["for-each-ref", "refs/dagq/runs/"])
            .lines()
            .count()
            == 1
    );
    assert_eq!(
        queue.show(TaskId::new(2)).unwrap().task.status(),
        TaskStatus::InProgress
    );
    assert!(queue.run_leases().unwrap().is_empty());
    let detail = queue.show(TaskId::new(2)).unwrap();
    let failed_event = detail
        .events
        .iter()
        .find(|e| e.kind == "integration_failed")
        .unwrap();
    assert_eq!(failed_event.payload["status"], "failed");
    assert_eq!(failed_event.payload["reason"], json!(reason));
    assert_eq!(
        failed_event.payload["receipt"],
        session_receipt(
            &run,
            run.result_commit().map(CommitSha::as_str).unwrap(),
            "failed",
            "already covered by task 1"
        )
    );
    // A failed receipt is not a receipt for a landing: only the first
    // (conflicting) attempt recorded one.
    assert_eq!(integration_receipts(&detail).len(), 1);
    // Retry or give up is a person's call, as after any failed run.
    queue
        .transition(TaskId::new(2), TaskAction::Cancel)
        .unwrap();
}

/// The rebase applies cleanly but the earlier landing broke this run's
/// verification (a semantic conflict): the run is parked with the rebased
/// tree in place so a session can fix it on top of main.
#[test]
fn verification_failure_after_rebase_needs_a_session_and_keeps_the_rebased_tree() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(TaskId::new(1), TaskAction::Draft).unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let breaker = queue
        .add(NewTask {
            title: "drop seed".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            dependencies: vec![],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap()
        .id();
    queue.transition(breaker, TaskAction::BypassReview).unwrap();
    backend.script_for(
        breaker.as_i64(),
        "git rm -q seed.txt && git commit -q -m 'drop seed'; receipt \"$(git rev-parse HEAD)\"",
    );
    let victim = add_file_task(
        &mut queue,
        &backend,
        "needs seed",
        "v.txt",
        "v",
        &["test -f seed.txt"],
    );
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(
        integrate(&db, breaker.as_i64(), &repo).unwrap()["outcome"],
        "integrated"
    );
    let main = git_out(&repo, &["rev-parse", "main"]);
    assert!(!repo.join("seed.txt").exists());

    let run = queue.show(victim).unwrap().runs[0].clone();
    let outcome = integrate(&db, victim.as_i64(), &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let reason = outcome["reason"].as_str().unwrap();
    assert!(
        reason.contains("\"test -f seed.txt\" exited with 1 after the rebase"),
        "{reason}"
    );
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let head = git_out(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(head, run.result_commit().cloned().unwrap());
    assert_eq!(git_out(&worktree, &["rev-parse", "HEAD^"]), main);
    assert_eq!(git_out(&worktree, &["status", "--porcelain"]), "");
    assert!(
        Path::new(run.run_dir().unwrap())
            .join("integrate-1-verify-1.log")
            .exists()
    );
    let parked = queue.show(victim).unwrap().runs[0].clone();
    assert_eq!(parked.status(), RunStatus::NeedsSession);
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main);
    // The receipt was read and recorded before the verification failed.
    let detail = queue.show(victim).unwrap();
    let recorded = integration_receipts(&detail);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(
        recorded[0]["commit"],
        json!(run.result_commit().cloned().unwrap())
    );
    assert_eq!(recorded[0]["main"], json!(main));
    assert_eq!(recorded[0]["receipt"]["run_id"], json!(run.id()));
    assert_eq!(recorded[0]["receipt"]["result"], "succeeded");
    assert_eq!(
        recorded[0]["receipt"]["commit"],
        json!(run.result_commit().cloned().unwrap())
    );
    let kinds = event_kinds(&detail);
    let receipt_at = kinds
        .iter()
        .position(|k| *k == "integration_receipt")
        .unwrap();
    let deferred_at = kinds
        .iter()
        .rposition(|k| *k == "integration_deferred")
        .unwrap();
    assert!(receipt_at < deferred_at, "{kinds:?}");
    // The session restores what the verification needs and reports the new head.
    fs::write(worktree.join("seed.txt"), "restored\n").unwrap();
    git(&worktree, &["add", "seed.txt"]);
    git(&worktree, &["commit", "-q", "-m", "restore seed"]);
    write_receipt(
        &parked,
        &git_out(&worktree, &["rev-parse", "HEAD"]),
        "succeeded",
        "restored seed",
    );
    assert_eq!(
        integrate(&db, victim.as_i64(), &repo).unwrap()["outcome"],
        "integrated"
    );
    let landed = queue.show(victim).unwrap().runs[0].clone();
    assert_landed(&repo, &landed, "needs seed", &main);
    assert!(repo.join("seed.txt").exists() && repo.join("v.txt").exists());
    let detail = queue.show(victim).unwrap();
    let recorded = integration_receipts(&detail);
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    assert_eq!(
        recorded[1]["commit"],
        json!(git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", run.id())]
        ))
    );
}

/// One run lands at a time. An `integrate` process that dies leaves the run
/// `integrating` with a stale lease; `recover` puts it back in the queue
/// instead of interrupting it, and the next `integrate` lands it. A landing
/// that cannot fast-forward the main checkout gives the slot back too.
#[test]
fn integration_slot_is_exclusive_and_an_abandoned_landing_is_recoverable() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let other = add_file_task(
        &mut queue,
        &backend,
        "other",
        "o.txt",
        "o",
        &["test -f seed.txt"],
    );
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let seed = git_out(&repo, &["rev-parse", "main"]);

    // Take the slot by hand, as a crashed `integrate` would have.
    let taken = queue
        .begin_integration(run.id(), "crashed", &sha(&seed))
        .unwrap();
    assert_eq!(taken.status(), RunStatus::Integrating);
    assert!(
        queue
            .transition(TaskId::new(1), TaskAction::BypassReview)
            .is_err()
    );
    let error = format!("{:#}", integrate(&db, other.as_i64(), &repo).unwrap_err());
    assert!(
        error.contains(&format!("run {} is integrating", run.id())),
        "{error}"
    );
    let error = format!("{:#}", integrate(&db, 1, &repo).unwrap_err());
    assert!(error.contains("is already integrating"), "{error}");
    assert!(
        queue
            .begin_integration(run.id(), "again", &sha(&seed))
            .is_err()
    );
    assert_eq!(
        queue.show(other).unwrap().runs[0].status(),
        RunStatus::AwaitingIntegration
    );
    // It is visible while alive, and recoverable once its process is gone.
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["runs"][0]["run_id"], json!(run.id()));
    assert_eq!(report["runs"][0]["status"], "integrating");
    assert_eq!(report["runs"][0]["recoverable"], false);
    assert!(runtime::recover(&db, run.id()).is_err());
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE run_leases SET heartbeat_at=0, pid=?1", [dead_pid()])
        .unwrap();
    assert_eq!(
        runtime::doctor(&db, true).unwrap()["runs"][0]["recoverable"],
        true
    );
    let recovered = runtime::recover(&db, run.id()).unwrap();
    assert_eq!(recovered["run"]["status"], "awaiting_integration");
    let event = queue
        .show(TaskId::new(1))
        .unwrap()
        .events
        .into_iter()
        .find(|e| e.kind == "run_recovered")
        .unwrap();
    assert_eq!(event.payload["previous_status"], "integrating");
    assert_eq!(event.payload["status"], "awaiting_integration");
    assert!(queue.run_leases().unwrap().is_empty());

    // A local change in the main checkout that collides with the landing
    // makes the fast-forward fail; the run goes back to the queue.
    fs::write(repo.join("change.txt"), "uncommitted local edit\n").unwrap();
    let error = format!("{:#}", integrate(&db, 1, &repo).unwrap_err());
    assert!(error.contains("fast-forward main"), "{error}");
    assert!(
        error.contains("returned to awaiting_integration"),
        "{error}"
    );
    let returned = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(returned.status(), RunStatus::AwaitingIntegration);
    assert!(returned.last_error().unwrap().contains("before main moved"));
    assert!(event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"integration_error"));
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), seed);
    assert!(queue.run_leases().unwrap().is_empty());
    fs::remove_file(repo.join("change.txt")).unwrap();
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let landed = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_landed(&repo, &landed, "test task", &seed);
    assert_eq!(
        integrate(&db, other.as_i64(), &repo).unwrap()["outcome"],
        "integrated"
    );
    assert_eq!(
        git_out(&repo, &["rev-list", "--count", &format!("{seed}..main")]),
        "2"
    );
}

const IDLE_AGENT: &str = "commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit";

/// Two independent tasks execute at the same time under one resident
/// supervisor; the dependent task waits for `integrate` and is then picked up
/// by the same loop with the new `main` as its base.
#[test]
fn independent_tasks_run_concurrently_and_a_dependent_starts_after_integration() {
    let (dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "independent", &[]);
    add_ready_task(&mut queue, "dependent", &[TaskId::new(1)]);
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let options = supervise_options(4, false);
    let supervisor = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };

    // Both independent runs are alive at once; the dependent has none. The
    // wait is as long as the later ones: under a loaded host (parallel
    // `cargo llvm-cov` runs) starting two runs took longer than 20 seconds.
    wait_until(&db, Duration::from_secs(30), |queue| {
        let running: Vec<TaskRun> = queue
            .active_runs()
            .unwrap()
            .into_iter()
            .filter(|r| r.status() == RunStatus::Running)
            .collect();
        running.len() == 2
            && running
                .iter()
                .all(|r| queue.run_lease(r.id()).unwrap().is_some())
    });
    assert!(queue.show(TaskId::new(3)).unwrap().runs.is_empty());
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["supervisors"].as_array().unwrap().len(), 1);
    assert_eq!(
        status["supervisors"][0]["run_ids"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(status["runs"].as_array().unwrap().len(), 2);
    assert!(status["runs"][0]["lease"]["pid"].is_number());
    let doctor = runtime::doctor(&db, true).unwrap();
    assert_eq!(doctor["runs"].as_array().unwrap().len(), 2);
    assert!(
        doctor["runs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["recoverable"] == false)
    );

    // Accepted and past the supervisor's review (the stand-in `claude`
    // prints no verdict, so each waits for a review by hand).
    wait_until(&db, Duration::from_secs(30), |queue| {
        [1, 2].iter().all(|task| {
            queue.show(TaskId::new(*task)).unwrap().runs[0].status()
                == RunStatus::AwaitingIntegration
        }) && queue.run_leases().unwrap().is_empty()
    });
    // Awaiting integration does not satisfy the dependency; the loop idles.
    thread::sleep(Duration::from_millis(500));
    assert!(queue.show(TaskId::new(3)).unwrap().runs.is_empty());
    assert!(queue.candidates().unwrap().is_empty());
    let first = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let second = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_ne!(first.workspace_id(), second.workspace_id());
    assert_eq!(first.base_commit(), second.base_commit());
    assert!(queue.run_leases().unwrap().is_empty());

    // Landing unblocks the dependent; the resident loop claims it from the landed main.
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let landed = git_out(&repo, &["rev-parse", "main"]);
    assert_ne!(landed, first.result_commit().cloned().unwrap());
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().runs[0]
            .result_commit()
            .map(CommitSha::as_str),
        Some(landed.as_str())
    );
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue
            .show(TaskId::new(3))
            .unwrap()
            .runs
            .first()
            .is_some_and(|r| r.status() == RunStatus::AwaitingIntegration)
    });
    let third = queue.show(TaskId::new(3)).unwrap().runs[0].clone();
    assert_eq!(*third.base_commit(), landed);
    assert_ne!(third.base_commit(), second.base_commit());

    // A graceful stop ends the loop once nothing is active.
    options.stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "stopped");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 3);
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 3);
    let mut closed = backend.closed();
    closed.sort();
    assert_eq!(closed, [workspace_id(0), workspace_id(1), workspace_id(2)]);
    assert_eq!(runtime::doctor(&db, true).unwrap()["runs"], json!([]));
    // The independent second run conflicts with the first (same file) and
    // waits for a session; the dependent, built on the landing, lands cleanly.
    assert_eq!(
        integrate(&db, 2, &repo).unwrap()["outcome"],
        "needs_session"
    );
    assert_eq!(integrate(&db, 3, &repo).unwrap()["outcome"], "integrated");
    assert_eq!(
        git_out(&repo, &["rev-list", "--count", &format!("{landed}..main")]),
        "1"
    );
    drop(dir);
}

/// A run that does not answer the exit request keeps its lease without
/// disturbing the run next to it, and is validated once its session ends.
#[test]
fn a_timed_out_run_is_kept_while_the_other_run_is_accepted() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "healthy", &[]);
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.script_for(1, HELD_AGENT);
    backend.exit_timeout = Duration::from_secs(1);
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"exit_request_timed_out")
            && queue
                .show(TaskId::new(2))
                .unwrap()
                .runs
                .first()
                .is_some_and(|r| {
                    r.status() == RunStatus::AwaitingIntegration
                        && queue.run_lease(r.id()).unwrap().is_none()
                })
    });
    let stuck = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let healthy = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert!(healthy.last_error().is_none());
    assert!(healthy.workspace_closed_at().is_some());
    // The stuck session held back the /exit that followed its review, so
    // its run is already accepted and its supervisor still holds it.
    assert_eq!(stuck.status(), RunStatus::AwaitingIntegration);
    assert!(stuck.last_error().is_none());
    assert!(queue.run_lease(stuck.id()).unwrap().is_some());
    // Its stuck_exit ask is the attention, not the run (task 104).
    wait_until(&db, Duration::from_secs(10), |queue| {
        queue.asks(AskQuery::default()).unwrap().iter().any(|a| {
            a.kind == AskKind::StuckExit
                && a.run_id.as_ref().map(RunId::as_str) == Some(stuck.id().as_str())
        })
    });
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, stuck.id()).is_none(), "{status}");
    assert!(runtime::recover(&db, stuck.id()).is_err());

    release_held_session(stuck.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2);
    for task in [1, 2] {
        assert_eq!(
            queue.show(TaskId::new(task)).unwrap().runs[0].status(),
            RunStatus::AwaitingIntegration
        );
    }
    assert!(queue.run_leases().unwrap().is_empty());
}

/// A nonzero session exit and a rejected receipt in one pass leave the
/// accepted run untouched; every run releases its lease.
#[test]
fn failed_runs_in_the_same_pass_do_not_affect_the_accepted_run() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "crashes", &[]);
    add_ready_task(&mut queue, "no receipt", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.script_for(2, "commit work; exit 7");
    backend.script_for(3, "commit work");
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]));
    let mut statuses: Vec<(i64, String)> = outcome["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["task_id"].as_i64().unwrap(),
                r["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    statuses.sort();
    assert_eq!(
        statuses,
        [
            (1, "awaiting_integration".to_owned()),
            (2, "failed".to_owned()),
            (3, "failed".to_owned())
        ]
    );
    assert!(
        queue.show(TaskId::new(3)).unwrap().runs[0]
            .last_error()
            .unwrap()
            .contains("receipt was not submitted")
    );
    assert_eq!(backend.closed(), [workspace_id(0)]);
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(runtime::doctor(&db, true).unwrap()["runs"], json!([]));
    // Failed tasks can be retried independently; the accepted one still owns its slot.
    queue.transition(TaskId::new(2), TaskAction::Ready).unwrap();
    assert!(queue.transition(TaskId::new(1), TaskAction::Ready).is_err());
    assert_eq!(queue.candidates().unwrap()[0].id(), TaskId::new(2));
}

/// Recovering one orphaned run touches neither the lease nor the processes of
/// the run that shares its supervisor.
#[test]
fn recovering_one_orphaned_run_leaves_the_other_running() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "still alive", &[]);
    let mut dead_wrapper = sleeper();
    let mut dead_agent = sleeper();
    let mut live_wrapper = sleeper();
    let mut live_agent = sleeper();
    let orphan = orphan_run(&repo, &db, "owner", dead_wrapper.id(), dead_agent.id());
    let survivor = orphan_run(&repo, &db, "owner", live_wrapper.id(), live_agent.id());
    // One supervisor, two leases; it died and took nothing with it.
    let raw = Connection::open(&db).unwrap();
    raw.execute("UPDATE run_leases SET heartbeat_at=0, pid=?1", [dead_pid()])
        .unwrap();
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["supervisors"].as_array().unwrap().len(), 1);
    assert_eq!(
        report["supervisors"][0]["run_ids"],
        json!([orphan.id(), survivor.id()])
    );
    assert_eq!(report["supervisors"][0]["stale"], true);
    assert_eq!(report["runs"].as_array().unwrap().len(), 2);
    assert!(
        report["runs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["recoverable"] == false && r["lease"]["stale"] == true)
    );
    for child in [&mut dead_wrapper, &mut dead_agent] {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    let report = runtime::doctor(&db, true).unwrap();
    assert_eq!(report["runs"][0]["recoverable"], true);
    assert_eq!(report["runs"][1]["recoverable"], false);
    assert_eq!(
        runtime::recover(&db, orphan.id()).unwrap()["run"]["status"],
        "interrupted"
    );
    // The survivor keeps its lease, processes and status; only the orphan changed.
    assert_eq!(
        queue.run(survivor.id()).unwrap().status(),
        RunStatus::Running
    );
    let leases = queue.run_leases().unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].run_id, *survivor.id());
    assert_eq!(queue.processes(survivor.id()).unwrap().len(), 2);
    let error = format!("{:#}", runtime::recover(&db, survivor.id()).unwrap_err());
    assert!(error.contains("wrapper pid"), "{error}");
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["runs"].as_array().unwrap().len(), 1);
    assert_eq!(status["runs"][0]["run_id"], json!(survivor.id()));
    for child in [&mut live_wrapper, &mut live_agent] {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    assert_eq!(
        runtime::recover(&db, survivor.id()).unwrap()["run"]["status"],
        "interrupted"
    );
    assert!(queue.run_leases().unwrap().is_empty());
}

/// Provision and start a run under `token` exactly as `supervise` does
/// (claim, plan, prompt, worktree, workspace with the wrapper thread of the
/// test backend), but with no loop or heartbeat behind the token: the state
/// a supervisor leaves when it is killed after the session started. Returns
/// the `running` run once the wrapper registered its agent.
fn start_run_under_dead_supervisor(
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
        runtime::prompt(&task, &run, None, &[], &[], &[]).unwrap(),
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
fn age_lease(db: &Path, run: &TaskRun, seconds: i64) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET heartbeat_at=unixepoch()-?2 WHERE run_id=?1",
            rusqlite::params![run.id(), seconds],
        )
        .unwrap();
}

fn adoption_events(detail: &dagq::domain::TaskDetail) -> Vec<&Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == "run_adopted")
        .map(|e| &e.payload)
        .collect()
}

fn supervisor_token_of(db: &Path, run: &TaskRun) -> String {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT supervisor_token FROM task_runs WHERE id=?1",
            [&run.id()],
            |r| r.get(0),
        )
        .unwrap()
}

/// A `running` run whose supervisor stopped heartbeating (lease 31 s old)
/// while its wrapper keeps heartbeating is adopted by the next supervisor
/// with a free slot: the lease and `supervisor_token` move to the adopter,
/// `run_adopted` records what was taken over, and the adopter drives the
/// run through receipt, idle, one exit request, validation and close.
#[test]
fn stale_lease_of_a_live_wrapper_is_adopted_and_driven_to_awaiting_integration() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    age_lease(&db, &run, 31);
    let mut queue = SqliteQueue::open(&db).unwrap();
    // Nothing else is claimable: the pass exists only for the adoption.
    assert!(queue.candidates().unwrap().is_empty());
    assert_eq!(
        runtime::status(&db).unwrap()["runs"][0]["lease"]["stale"],
        true
    );

    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished", "{outcome}");
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(outcome["runs"][0]["id"], json!(run.id()));
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);

    let detail = queue.show(TaskId::new(1)).unwrap();
    let adopted_run = &detail.runs[0];
    assert_eq!(adopted_run.status(), RunStatus::AwaitingIntegration);
    assert!(adopted_run.last_error().is_none());
    assert!(adopted_run.result_commit().is_some());
    assert!(adopted_run.workspace_closed_at().is_some());
    assert!(queue.run_leases().unwrap().is_empty());
    let adopted = adoption_events(&detail);
    assert_eq!(adopted.len(), 1, "{adopted:?}");
    let payload = adopted[0];
    assert_eq!(payload["previous_token"], "dead-supervisor");
    assert_eq!(payload["previous_pid"], json!(std::process::id()));
    let age = payload["previous_heartbeat_age_secs"].as_i64().unwrap();
    assert!(age >= 31, "{payload}");
    assert_eq!(payload["wrapper"]["pid"], json!(std::process::id()));
    assert_eq!(payload["wrapper"]["alive"], true);
    assert_eq!(payload["wrapper"]["exited_at"], Value::Null);
    assert_eq!(payload["pid"], json!(std::process::id()));
    // The adopter's token replaced the claimer's on the run.
    let adopter = payload["token"].as_str().unwrap();
    assert_ne!(adopter, "dead-supervisor");
    assert_eq!(supervisor_token_of(&db, &run), adopter);
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("lease_acquired") < position("run_adopted"));
    assert!(position("run_adopted") < position("receipt_observed"));
    // The session stays open through validation and the review (the
    // stand-in `claude` prints no verdict, so the review fails); /exit
    // follows (ADR-0027).
    assert!(position("receipt_observed") < position("session_idle_observed"));
    assert!(position("session_idle_observed") < position("supervision_finished"));
    assert!(position("supervision_finished") < position("validation_finished"));
    assert!(position("validation_finished") < position("review_started"));
    assert!(position("review_started") < position("exit_requested"));
    assert!(position("exit_requested") < position("session_exited"));
    assert!(position("session_exited") < position("workspace_closed"));
    assert!(position("workspace_closed") < position("review_failed"));
    assert!(position("review_failed") < position("lease_released"));
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert!(!kinds.contains(&"runtime_error"));
    assert!(!kinds.contains(&"run_recovered"));
    // The adoption is a fact of this run's history, not a second run.
    assert_eq!(detail.runs.len(), 1);
    let event = detail
        .events
        .iter()
        .find(|e| e.kind == "run_adopted")
        .unwrap();
    assert_eq!(event.run_id.as_ref(), Some(run.id()));
}

/// The incident of task 15: the supervisor was killed a moment ago, so its
/// lease pid is dead while its heartbeat is still fresh. The pid rule alone
/// makes the lease stale, and the run is adopted without waiting for the TTL.
#[test]
fn dead_supervisor_pid_with_a_fresh_heartbeat_is_adopted() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "killed");
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET pid=?2, heartbeat_at=unixepoch() WHERE run_id=?1",
            rusqlite::params![run.id(), dead_pid()],
        )
        .unwrap();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(
        outcome["runs"][0]["status"], "awaiting_integration",
        "{outcome}"
    );
    assert_eq!(outcome["errors"], json!([]));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let adopted = adoption_events(&detail);
    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0]["previous_token"], "killed");
    assert_ne!(adopted[0]["previous_pid"], json!(std::process::id()));
    assert!(adopted[0]["previous_heartbeat_age_secs"].as_i64().unwrap() < 30);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
}

/// Everything adoption must leave alone: a fresh lease; a stale lease whose
/// wrapper is dead or silent (that is `recover`'s case); `claimed` /
/// `starting` runs; runs without a lease row; and an `integrating` run. A
/// supervisor pass over them adopts nothing and writes no `run_adopted`
/// event. The runs whose dead supervisor's lease is stale and whose
/// processes are all gone (the dead wrapper, `starting`, `claimed`) it
/// recovers itself (task 236); the silent wrapper's live process and the
/// fresh lease still block that.
#[test]
fn fresh_leases_dead_wrappers_early_runs_leaseless_and_integrating_runs_are_not_adopted() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.transition(TaskId::new(1), TaskAction::Draft).unwrap();
    for title in [
        "fresh",
        "dead wrapper",
        "silent wrapper",
        "starting",
        "leaseless",
        "claimed",
    ] {
        add_ready_task(&mut queue, title, &[]);
    }
    let raw = Connection::open(&db).unwrap();
    // Fresh lease, live wrapper (children that heartbeat nothing but are alive; the
    // lease is what decides here).
    let mut children: Vec<std::process::Child> = Vec::new();
    let mut spawn = || {
        let child = sleeper();
        let pid = child.id();
        children.push(child);
        pid
    };
    let fresh = orphan_run(&repo, &db, "fresh-owner", spawn(), spawn());
    // Stale lease, wrapper dead: recoverable, never adopted.
    let dead_wrapper = orphan_run(&repo, &db, "gone", dead_pid(), dead_pid());
    // Stale lease, wrapper alive but silent for longer than the TTL.
    let silent_wrapper = orphan_run(&repo, &db, "gone", spawn(), spawn());
    raw.execute(
        "UPDATE run_leases SET heartbeat_at=unixepoch()-31, pid=?1 WHERE token='gone'",
        [dead_pid()],
    )
    .unwrap();
    raw.execute(
        "UPDATE run_processes SET heartbeat_at=unixepoch()-31 WHERE run_id IN (?1, ?2)",
        [&dead_wrapper.id(), &silent_wrapper.id()],
    )
    .unwrap();
    // `starting` with a stale lease: register_wrapper needs the claimer's token.
    use dagq::{domain::ClaimOutcome, infrastructure::runtime_store::RunPlan};
    let base = queue.run(fresh.id()).unwrap().base_commit().clone();
    let ClaimOutcome::Claimed { run: starting } =
        queue.claim_for_supervisor(&base, "gone-early").unwrap()
    else {
        panic!()
    };
    queue
        .plan_run(
            starting.id(),
            "gone-early",
            &RunPlan {
                repo_path: "/test".into(),
                run_dir: "/run".into(),
                branch: "dagq/starting".into(),
                worktree_path: "/run/worktree".into(),
                receipt_path: "/run/receipt.json".into(),
                log_path: "/run/log".into(),
            },
        )
        .unwrap();
    // Its wrapper exited before the agent registered: still `starting`,
    // which only `recover` handles (task 236).
    let early_wrapper = dead_pid();
    queue
        .workspace_created(starting.id(), "gone-early", "ws-starting")
        .unwrap();
    queue
        .register_wrapper(starting.id(), "gone-early", early_wrapper)
        .unwrap();
    queue
        .wrapper_exited(starting.id(), early_wrapper, 1)
        .unwrap();
    let starting = queue.run(starting.id()).unwrap();
    assert_eq!(starting.status(), RunStatus::Starting);
    // `running` without a lease: abandoned by a runtime error or recovered.
    let leaseless = orphan_run(&repo, &db, "abandoned", spawn(), spawn());
    queue
        .abandon_run(
            leaseless.id(),
            "abandoned",
            "exit request timed out",
            &ReasonCode::Other.into(),
        )
        .unwrap();
    assert!(queue.run_lease(leaseless.id()).unwrap().is_none());
    // `claimed` with a stale lease.
    let ClaimOutcome::Claimed { run: claimed } =
        queue.claim_for_supervisor(&base, "gone-early").unwrap()
    else {
        panic!()
    };
    raw.execute(
        "UPDATE run_leases SET heartbeat_at=0, pid=?1 WHERE token='gone-early'",
        [dead_pid()],
    )
    .unwrap();
    assert!(queue.candidates().unwrap().is_empty());
    let before = runtime::doctor(&db, true).unwrap();
    let health = |report: &Value, run: &TaskRun| -> Value {
        report["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["run_id"] == json!(run.id()))
            .cloned()
            .unwrap()
    };
    assert_eq!(health(&before, &dead_wrapper)["recoverable"], true);
    assert_eq!(health(&before, &silent_wrapper)["recoverable"], false);
    assert_eq!(health(&before, &fresh)["recoverable"], false);
    assert_eq!(health(&before, &starting)["recoverable"], true);
    assert_eq!(health(&before, &claimed)["recoverable"], true);

    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["outcome"], "finished", "{outcome}");
    assert_eq!(outcome["errors"], json!([]));
    for run in [
        &fresh,
        &dead_wrapper,
        &silent_wrapper,
        &starting,
        &leaseless,
        &claimed,
    ] {
        let detail = queue.show(run.task_id()).unwrap();
        assert!(
            adoption_events(&detail).is_empty(),
            "run {} of task {} was adopted",
            run.id(),
            run.task_id()
        );
    }
    for run in [&fresh, &silent_wrapper, &leaseless] {
        assert_eq!(queue.run(run.id()).unwrap().status(), run.status());
        assert!(
            payloads(&queue.show(run.task_id()).unwrap(), "run_recovered").is_empty(),
            "run {} was recovered",
            run.id()
        );
    }
    for run in [&dead_wrapper, &starting, &claimed] {
        assert_ne!(queue.run(run.id()).unwrap().status(), run.status());
        let detail = queue.show(run.task_id()).unwrap();
        let recovered = payloads(&detail, "run_recovered");
        assert_eq!(recovered.len(), 1, "run {}", run.id());
        assert_eq!(recovered[0]["by"], "supervisor");
        assert_eq!(recovered[0]["previous_status"], json!(run.status()));
        assert_eq!(recovered[0]["status"], "interrupted");
        assert_eq!(recovered[0]["lease_deleted"], true);
        assert!(queue.run_lease(run.id()).unwrap().is_none());
    }
    // Leases, tokens and doctor's verdicts of the rest are exactly as
    // before the pass.
    assert_eq!(
        queue.run_lease(fresh.id()).unwrap().unwrap().token,
        "fresh-owner"
    );
    assert_eq!(
        queue.run_lease(silent_wrapper.id()).unwrap().unwrap().token,
        "gone"
    );
    assert!(queue.run_lease(leaseless.id()).unwrap().is_none());
    let after = runtime::doctor(&db, true).unwrap();
    for run in [&fresh, &silent_wrapper] {
        assert_eq!(
            health(&after, run)["recoverable"],
            health(&before, run)["recoverable"]
        );
        assert_eq!(
            health(&after, run)["lease"]["stale"],
            health(&before, run)["lease"]["stale"]
        );
    }
    for child in &mut children {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

/// An `integrating` run is never adopted, however stale its lease: a
/// crashed landing is `recover`'s case and goes back to the merge queue.
#[test]
fn integrating_run_with_a_stale_lease_is_not_adopted() {
    let (_dir, repo, db, run) = awaiting_run();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let main = git_out(&repo, &["rev-parse", "main"]);
    queue
        .begin_integration(run.id(), "crashed", &sha(&main))
        .unwrap();
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE run_leases SET heartbeat_at=0, pid=?1", [dead_pid()])
        .unwrap();
    assert!(queue.candidates().unwrap().is_empty()); // The dependent still waits.
    let backend = TestWorkspace::new(&db, true, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::Integrating
    );
    assert_eq!(queue.run_lease(run.id()).unwrap().unwrap().token, "crashed");
    assert!(adoption_events(&queue.show(TaskId::new(1)).unwrap()).is_empty());
    assert_eq!(
        runtime::recover(&db, run.id()).unwrap()["run"]["status"],
        "awaiting_integration"
    );
}

/// The previous supervisor already asked the session to exit: the adopter
/// rebuilds that from the `exit_requested` event and does not send `/exit`
/// again, and its receipt observation is not repeated either.
#[test]
fn adopter_does_not_repeat_an_exit_request_the_previous_supervisor_sent() {
    let (_dir, repo, db) = fixture();
    // The session ends on its own once the adopter has watched it for a
    // while, as it would after the /exit that was already typed.
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        &format!("commit work; receipt \"$(git rev-parse HEAD)\"; idle; {HOLD}"),
    ));
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let receipt = PathBuf::from(run.receipt_path().unwrap());
    wait_until(&db, Duration::from_secs(10), |_| receipt.is_file());
    // What the previous supervisor recorded before it died.
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_runtime_event(
            run.id(),
            "receipt_observed",
            json!({"path": run.receipt_path(), "validated": false}),
        )
        .unwrap();
    queue
        .record_runtime_event(run.id(), "session_idle_observed", json!({}))
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            "exit_requested",
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
        )
        .unwrap();
    age_lease(&db, &run, 31);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !adoption_events(&queue.show(TaskId::new(1)).unwrap()).is_empty()
    });
    // Several passes over the idle session send nothing.
    thread::sleep(Duration::from_millis(500));
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(
        outcome["runs"][0]["status"], "awaiting_integration",
        "{outcome}"
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(adoption_events(&detail).len(), 1);
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert_eq!(
        kinds.iter().filter(|k| **k == "receipt_observed").count(),
        1
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "session_idle_observed")
            .count(),
        1
    );
    assert!(!kinds.contains(&"exit_request_timed_out"));
}

/// The exit timeout of an adopted run restarts at adoption: a session that
/// still ignores the earlier request is reported after the adopter's own
/// timeout, with one `exit_requested` event in total, and the adopter keeps
/// the run until the session ends.
#[test]
fn adopted_exit_request_times_out_from_the_adoption() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_timeout = Duration::from_secs(1);
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_runtime_event(
            run.id(),
            "exit_requested",
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
        )
        .unwrap();
    age_lease(&db, &run, 31);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"exit_request_timed_out")
    });
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Running);
    let lease = queue.run_lease(run.id()).unwrap().unwrap();
    assert_ne!(lease.token, "dead-supervisor");
    // Let the fake session out, the way a person answering it would.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(adoption_events(&detail).len(), 1);
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "exit_request_timed_out")
            .count(),
        1
    );
    assert!(!kinds.contains(&"runtime_error"));
    assert!(queue.run_leases().unwrap().is_empty());
}

/// A run whose exit request already timed out under the previous supervisor
/// is adopted without recording the timeout again, and is validated once
/// its session ends.
#[test]
fn adopted_run_does_not_record_an_exit_timeout_twice() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_timeout = Duration::from_secs(1);
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let mut queue = SqliteQueue::open(&db).unwrap();
    for kind in ["exit_requested", "exit_request_timed_out"] {
        queue
            .record_runtime_event(
                run.id(),
                kind,
                json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
            )
            .unwrap();
    }
    age_lease(&db, &run, 31);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !adoption_events(&queue.show(TaskId::new(1)).unwrap()).is_empty()
    });
    // Well past the adopter's own timeout.
    thread::sleep(Duration::from_millis(1500));
    let kinds = event_kinds(&queue.show(TaskId::new(1)).unwrap())
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "exit_request_timed_out")
            .count(),
        1
    );
    assert!(queue.run_lease(run.id()).unwrap().is_some());
    // The timeout the dead supervisor recorded without its ask gets one
    // stuck_exit ask from the adopter, once.
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::StuckExit);
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "exit_request_timed_out")
            .count(),
        1
    );
    assert!(!kinds.contains(&"runtime_error"));
    assert!(queue.read_ask(asks[0].id).unwrap().closed_at.is_some());
    // The other notification is the ask of its stand-in review.
    let notifications = backend.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 2, "{notifications:?}");
    assert!(notifications[1].0.ends_with("approve_landing"));
}

/// An adopted run whose timeout already has a stuck_exit ask (answered by
/// the inbox here, the session still up) is not asked again; the runtime
/// only closes the answered ask once the session exits.
#[test]
fn adopted_run_does_not_ask_about_its_exit_twice() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_timeout = Duration::from_secs(1);
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let mut queue = SqliteQueue::open(&db).unwrap();
    for kind in ["exit_requested", "exit_request_timed_out"] {
        queue
            .record_runtime_event(
                run.id(),
                kind,
                json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
            )
            .unwrap();
    }
    let asked = queue
        .ask(NewAsk {
            kind: AskKind::StuckExit,
            task_id: None,
            run_id: Some(run.id().clone()),
            question: "send /exit".into(),
            options: Vec::new(),
            asked_by: "supervisor".into(),
            reason_category: dagq::domain::AskReason::RecoveryFailed,
            finding_id: None,
        })
        .unwrap()
        .ask;
    queue.answer(asked.id, "sent /exit").unwrap();
    age_lease(&db, &run, 31);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !adoption_events(&queue.show(TaskId::new(1)).unwrap()).is_empty()
    });
    thread::sleep(Duration::from_millis(2500));
    assert_eq!(
        queue
            .asks(AskQuery {
                all: true,
                ..Default::default()
            })
            .unwrap()
            .len(),
        1
    );
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let closed = queue.read_ask(asked.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(closed.answer.as_deref(), Some("sent /exit"));
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "ask_answered").count(), 1);
    // Only the ask of its stand-in review notifies.
    let notifications = backend.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1, "{notifications:?}");
    assert!(notifications[0].0.ends_with("approve_landing"));
}

/// The supervisor died after the wrapper reported its exit but before
/// `supervision_finished`, and, separately, while a run was `validating`.
/// Both are adopted: the first finishes supervision from the recorded exit,
/// the second restarts validation from the receipt and worktree.
#[test]
fn exited_wrapper_and_validating_runs_are_adopted_and_validated() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "validating", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    // The first session goes idle after its receipt and then ends on its own
    // (a person's /exit by the old procedure): the adopter must not
    // send /exit to a session that already exited.
    backend.script_for(
        1,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; sleep 1",
    );
    let exited = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-a");
    let validating = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-b");
    backend.join(); // Both sessions end by themselves.
    for run in [&exited, &validating] {
        assert!(event_kinds(&queue.show(run.task_id()).unwrap()).contains(&"session_exited"));
        assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Running);
    }
    queue.finish_supervision(validating.id(), "dead-b").unwrap();
    assert_eq!(
        queue.run(validating.id()).unwrap().status(),
        RunStatus::Validating
    );
    age_lease(&db, &exited, 31);
    age_lease(&db, &validating, 31);

    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    let mut closed = backend.closed();
    closed.sort();
    assert_eq!(closed, [workspace_id(0), workspace_id(1)]);
    for run in [&exited, &validating] {
        let detail = queue.show(run.task_id()).unwrap();
        let after = &detail.runs[0];
        assert_eq!(
            after.status(),
            RunStatus::AwaitingIntegration,
            "{}",
            run.id()
        );
        assert!(after.last_error().is_none(), "{after:?}");
        assert!(!event_kinds(&detail).contains(&"exit_requested"));
        assert!(after.result_commit().is_some());
        assert!(after.workspace_closed_at().is_some());
        let adopted = adoption_events(&detail);
        assert_eq!(adopted.len(), 1);
        assert_eq!(adopted[0]["wrapper"]["alive"], Value::Null);
        assert!(adopted[0]["wrapper"]["exited_at"].is_number());
        let kinds = event_kinds(&detail);
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == "supervision_finished")
                .count(),
            1
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == "validation_finished")
                .count(),
            1
        );
        // Validation checks the receipt only; integrate runs the commands.
        assert!(!kinds.contains(&"verification_command"));
    }
    let detail = queue.show(exited.task_id()).unwrap();
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("session_exited") < position("run_adopted"));
    assert!(position("run_adopted") < position("supervision_finished"));
    let detail = queue.show(validating.task_id()).unwrap();
    let kinds = event_kinds(&detail);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("supervision_finished") < position("run_adopted"));
    assert!(position("run_adopted") < position("validation_finished"));
    assert!(queue.run_leases().unwrap().is_empty());
}

/// Two supervisors with free slots find the same stale lease at once: the
/// transaction lets exactly one of them adopt, the other sees no lease
/// under the old token and moves on. One `run_adopted` event, one owner.
#[test]
fn two_supervisors_racing_for_one_stale_lease_adopt_it_once() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    age_lease(&db, &run, 31);
    let racers: Vec<_> = (0..2)
        .map(|_| {
            let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
            thread::spawn(move || supervise(&db, &repo, &backend))
        })
        .collect();
    let outcomes: Vec<Value> = racers
        .into_iter()
        .map(|racer| joined(racer, "a racing supervisor thread to return").unwrap())
        .collect();
    backend.join();
    let driven: Vec<&Value> = outcomes
        .iter()
        .filter(|o| !o["runs"].as_array().unwrap().is_empty())
        .collect();
    assert_eq!(driven.len(), 1, "{outcomes:?}");
    assert_eq!(driven[0]["runs"][0]["status"], "awaiting_integration");
    assert!(outcomes.iter().all(|o| o["errors"] == json!([])));
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let adopted = adoption_events(&detail);
    assert_eq!(adopted.len(), 1, "{adopted:?}");
    assert_eq!(supervisor_token_of(&db, &run), adopted[0]["token"]);
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);

    // The queue method itself: a second adoption under the old token, or
    // one against a fresh lease, takes nothing.
    add_ready_task(&mut queue, "second", &[]);
    add_ready_task(&mut queue, "early", &[]);
    let second = orphan_run(&repo, &db, "fresh", std::process::id(), std::process::id());
    assert!(
        queue
            .adopt_run(second.id(), "fresh", "eager", 1, json!({}))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        queue.run_lease(second.id()).unwrap().unwrap().token,
        "fresh"
    );
    age_lease(&db, &second, 31);
    let taken = queue
        .adopt_run(second.id(), "fresh", "first", 1, json!({"pid": 1}))
        .unwrap()
        .unwrap();
    assert_eq!(taken.status(), RunStatus::Running);
    assert!(
        queue
            .adopt_run(second.id(), "fresh", "second", 2, json!({}))
            .unwrap()
            .is_none()
    );
    let lease = queue.run_lease(second.id()).unwrap().unwrap();
    assert_eq!((lease.token.as_str(), lease.pid), ("first", 1));
    assert!(SystemClock.now() - lease.heartbeat_at <= 5);
    assert_eq!(supervisor_token_of(&db, &second), "first");
    assert!(queue.holds_lease(second.id(), "first").unwrap());
    assert!(!queue.holds_lease(second.id(), "fresh").unwrap());
    assert!(queue.has_run_event(second.id(), "run_adopted").unwrap());
    assert!(
        !queue
            .has_run_event(second.id(), "receipt_observed")
            .unwrap()
    );
    let payload = &adoption_events(&queue.show(second.task_id()).unwrap())[0].clone();
    assert_eq!(payload["wrapper"], json!({"pid": 1}));
    assert_eq!(payload["previous_token"], "fresh");
    assert_eq!(payload["previous_pid"], json!(std::process::id()));
    // A `starting` run is refused by the method too, stale or not.
    use dagq::domain::ClaimOutcome;
    let ClaimOutcome::Claimed { run: early } = queue
        .claim_for_supervisor(second.base_commit(), "early")
        .unwrap()
    else {
        panic!()
    };
    age_lease(&db, &early, 31);
    assert!(
        queue
            .adopt_run(early.id(), "early", "eager", 1, json!({}))
            .unwrap()
            .is_none()
    );
    // The status/doctor lists the adopted run under the adopter's token like any other.
    let status = runtime::status(&db).unwrap();
    let holders: Vec<&Value> = status["supervisors"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| {
            s["run_ids"]
                .as_array()
                .unwrap()
                .contains(&json!(second.id()))
        })
        .collect();
    assert_eq!(holders.len(), 1);
    assert_eq!(holders[0]["pid"], 1);
}

/// A resident supervisor whose lease was taken over (here by hand, as an
/// adopter or `recover` would) drops the run from its slots without writing
/// anything more about it, while the adopter drives the run to the end.
#[test]
fn a_supervisor_that_lost_its_lease_stops_touching_the_run() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let options = supervise_options(2, false);
    let original = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(20), |queue| {
        queue
            .active_runs()
            .unwrap()
            .first()
            .is_some_and(|r| r.status() == RunStatus::Running)
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.active_runs().unwrap().remove(0);
    // Draining: the original keeps driving its run but adopts and claims
    // nothing more, so it cannot take the lease back once it loses it.
    options.stop.store(true, Ordering::SeqCst);
    // The lease changes hands: another token, stale, as a killed
    // supervisor's would look to an adopter.
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET token='taken', heartbeat_at=unixepoch()-31 WHERE run_id=?1",
            [&run.id()],
        )
        .unwrap();
    // The original notices within a tick, drops the run and, draining with
    // nothing active, exits.
    let outcome = joined(original, "the first supervisor thread to return").unwrap();
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Running);
    assert_eq!(queue.run_lease(run.id()).unwrap().unwrap().token, "taken");
    let adopter = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(
        adopter["runs"][0]["status"], "awaiting_integration",
        "{adopter}"
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(outcome["outcome"], "stopped");
    assert_eq!(outcome["runs"], json!([]));
    assert_eq!(outcome["errors"][0]["run_id"], json!(run.id()));
    assert!(
        outcome["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("held by another process"),
        "{outcome}"
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(detail.runs[0].last_error().is_none());
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    let adopted = adoption_events(&detail);
    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0]["previous_token"], "taken");
    assert!(queue.supervisors().unwrap().is_empty());
}

fn watch_role(
    db: &Path,
    after: Option<i64>,
    timeout: Duration,
    role: dagq::domain::SessionRole,
) -> Value {
    use dagq::watch::{WatchOptions, watch};
    watch(
        db,
        &WatchOptions {
            after: after.map(EventId::new),
            timeout,
            interval: Duration::from_millis(50),
            role: Some(role),
        },
    )
    .unwrap()
}

/// The one notification is `ask`'s (ADR-0022 decision 5): a run reaching
/// awaiting_integration sends none, a new ask sends one to the inbox
/// workspace `up` recorded, and a repeated ask none.
#[test]
fn only_a_new_ask_notifies_and_it_goes_to_the_inbox() {
    use dagq::domain::{AskKind, NewAsk, SessionRole};
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "commit work; receipt \"$(git rev-parse HEAD)\"");
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let run_id = outcome["runs"][0]["id"].as_str().unwrap().to_owned();
    // The supervisor's own ask #1, of the failed stand-in review (task
    // 328), notified; it is closed so that the run can be asked again.
    {
        let mut notifications = backend.notifications.lock().unwrap();
        assert_eq!(notifications.len(), 1, "{notifications:?}");
        assert!(notifications[0].0.ends_with("ask #1 approve_landing"));
        notifications.clear();
    }
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .answer(dagq::domain::AskId::new(1), "withdrawn")
        .unwrap();
    queue.close_ask(dagq::domain::AskId::new(1)).unwrap();

    let new_ask = |question: &str| NewAsk {
        kind: AskKind::ApproveLanding,
        task_id: None,
        run_id: Some(RunId::new(run_id.clone()).unwrap()),
        question: question.into(),
        options: vec!["land".into()],
        asked_by: "worker".into(),
        reason_category: dagq::domain::AskReason::Scope,
        finding_id: None,
    };
    // Without an inbox the notification names no workspace; the bound
    // repository's main checkout names the queue.
    let other = repo.parent().unwrap().join("elsewhere");
    let asked = runtime::ask(&db, &other, new_ask(&"長".repeat(250)), &backend).unwrap();
    assert_eq!(asked["created"], true);
    assert_eq!(asked["notified"], true);
    let repo_name = repo.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(
        *backend.notifications.lock().unwrap(),
        vec![(
            format!("[{repo_name}] ask #2 approve_landing"),
            format!("{}…\ntask 1 run {run_id}", "長".repeat(200)),
            None
        )]
    );
    // The same run and kind again: the open ask, no notification.
    let again = runtime::ask(&db, &repo, new_ask("again"), &backend).unwrap();
    assert_eq!(again["created"], false);
    assert_eq!(again["notified"], false);
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);

    // With the inbox recorded, a new ask goes to its workspace.
    let queue = queue;
    queue
        .register_session_workspace(SessionRole::Inbox, "INBOX-UUID")
        .unwrap();
    let asked = runtime::ask(
        &db,
        &other,
        NewAsk {
            kind: AskKind::Decide,
            task_id: Some(TaskId::new(1)),
            run_id: None,
            question: "which?".into(),
            options: Vec::new(),
            asked_by: "worker".into(),
            reason_category: dagq::domain::AskReason::RecoveryFailed,
            finding_id: None,
        },
        &backend,
    )
    .unwrap();
    assert_eq!(asked["notified"], true);
    assert_eq!(
        backend.notifications.lock().unwrap()[1],
        (
            format!("[{repo_name}] ask #3 decide"),
            "which?\ntask 1".into(),
            Some("INBOX-UUID".into())
        )
    );
    // The observer's blocked ask on no task notifies the inbox too, with
    // the question alone as its body.
    let blocked = runtime::ask(
        &db,
        &other,
        NewAsk {
            kind: AskKind::Blocked,
            task_id: None,
            run_id: None,
            question: "slots idle".into(),
            options: Vec::new(),
            asked_by: "observer".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
        },
        &backend,
    )
    .unwrap();
    assert_eq!(blocked["task_id"], Value::Null);
    assert_eq!(blocked["notified"], true);
    assert_eq!(
        backend.notifications.lock().unwrap()[2],
        (
            format!("[{repo_name}] ask #4 blocked"),
            "slots idle".into(),
            Some("INBOX-UUID".into())
        )
    );
    assert_eq!(backend.notifications.lock().unwrap().len(), 3);
}

#[test]
fn asks_of_a_run_are_attention_for_the_inbox_until_closed() {
    use dagq::domain::{AskKind, NewAsk, SessionRole};
    let (_dir, _repo, db, run) = awaiting_run();
    let mut queue = SqliteQueue::open(&db).unwrap();
    // The ask of the failed stand-in review, closed unanswered: the run
    // falls back to a review by hand (task 328).
    let failed_review = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(failed_review.len(), 1);
    assert_eq!(failed_review[0].kind, AskKind::ApproveLanding);
    queue.answer(failed_review[0].id, "withdrawn").unwrap();
    queue.close_ask(failed_review[0].id).unwrap();
    let before = queue.latest_event_id().unwrap().as_i64();
    // A `decide` ask: the supervisor applies an `approve_landing` answer
    // itself (see a_third_review_that_does_not_pass_asks_a_person_and_land_lands_it).
    let new_ask = |question: &str| NewAsk {
        kind: AskKind::Decide,
        task_id: None,
        run_id: Some(run.id().clone()),
        question: question.into(),
        options: vec!["land".into(), "send back".into()],
        asked_by: "worker".into(),
        reason_category: dagq::domain::AskReason::RecoveryFailed,
        finding_id: None,
    };

    // An inbox watch started before the ask wakes on ask_opened alone.
    let watcher = {
        let db = db.clone();
        thread::spawn(move || {
            watch_role(
                &db,
                Some(before),
                Duration::from_secs(20),
                SessionRole::Inbox,
            )
        })
    };
    let opened = queue.ask(new_ask(&"q".repeat(250))).unwrap();
    assert!(opened.created);
    assert_eq!(opened.ask.task_id, Some(run.task_id()));
    let woke = joined(watcher, "the watch thread to return");
    assert_eq!(
        woke["events"],
        json!([{"id": before + 1, "kind": "ask_opened", "task_id": 1, "run_id": run.id(),
                "ask_id": opened.ask.id, "next": format!("answer ask {}", opened.ask.id),
                "reason_category": "recovery_failed",
                "created_at": woke["events"][0]["created_at"]}])
    );
    assert_eq!(woke["supervisors_changed"], false);
    // The same run and kind is registered once.
    let again = queue.ask(new_ask("other")).unwrap();
    assert!(!again.created);
    assert_eq!(again.ask.id, opened.ask.id);
    assert_eq!(queue.latest_event_id().unwrap().as_i64(), before + 1);

    // status lists the open ask; all attention is the inbox's
    // (ADR-0024 decision 6).
    let status = runtime::status_for(&db, Some(SessionRole::Inbox)).unwrap();
    let ask_attention: Vec<&Value> = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a.get("ask_id").is_some())
        .collect();
    assert_eq!(
        ask_attention,
        [
            &json!({"run_id": run.id(), "task_id": 1, "ask_id": opened.ask.id, "status": "open",
                "kind": "ask_opened", "last_error": null, "reason_category": "recovery_failed",
                "next": format!("answer ask {}", opened.ask.id)})
        ]
    );
    let asks = status["asks"].as_array().unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0]["kind"], "decide");
    assert_eq!(asks[0]["asked_by"], "worker");
    assert_eq!(asks[0]["run_id"], json!(run.id()));
    assert!(asks[0]["age_secs"].as_i64().unwrap() >= 0);
    assert_eq!(
        asks[0]["question"].as_str().unwrap().chars().count(),
        201,
        "200 characters and the ellipsis"
    );
    // The run's own attention keeps the event that brought it there (the
    // stand-in `claude` printed no verdict, so its review failed, and its
    // ask was closed).
    assert_eq!(
        run_attention_of(&status, run.id()).unwrap()["kind"],
        "review_failed"
    );
    assert_eq!(
        run_attention_of(&status, run.id()).unwrap()["next"],
        "review by hand"
    );
    assert!(
        runtime::status_for(&db, Some(SessionRole::Planner)).unwrap()["attention"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // The planner's watch never wakes.
    let quiet = watch_role(
        &db,
        Some(before),
        Duration::from_millis(200),
        SessionRole::Planner,
    );
    assert_eq!(quiet["events"], json!([]));
    assert_eq!(quiet["cursor"], json!(before));

    // The answer is the inbox's attention until the ask is closed.
    let answered = queue.answer(opened.ask.id, "land").unwrap();
    assert_eq!(answered.answer.as_deref(), Some("land"));
    let events = queue.run_events(run.id()).unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.kind, "ask_answered");
    assert_eq!(last.payload["ask_id"], json!(opened.ask.id));
    let woke = watch_role(
        &db,
        Some(before + 1),
        Duration::from_secs(20),
        SessionRole::Inbox,
    );
    assert_eq!(woke["events"][0]["kind"], "ask_answered");
    assert_eq!(
        woke["events"][0]["next"],
        format!("read the answer of ask {} and close it", opened.ask.id)
    );
    let status = runtime::status_for(&db, None).unwrap();
    assert_eq!(status["asks"], json!([]));
    assert!(status["attention"].as_array().unwrap().iter().any(|a| {
        a["kind"] == "ask_answered"
            && a["status"] == "answered"
            && a["ask_id"] == opened.ask.id.as_i64()
    }));
    queue.close_ask(opened.ask.id).unwrap();
    assert!(
        runtime::status(&db).unwrap()["attention"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a.get("ask_id").is_none())
    );
    // A closed ask frees its (run, kind) for a new one.
    assert!(queue.ask(new_ask("again")).unwrap().created);
}

fn watch_for(db: &Path, after: Option<i64>, timeout: Duration) -> Value {
    use dagq::watch::{WatchOptions, watch};
    watch(
        db,
        &WatchOptions {
            after: after.map(EventId::new),
            timeout,
            interval: Duration::from_millis(50),
            role: None,
        },
    )
    .unwrap()
}

/// `watch` in a thread, started once it has read its baseline.
fn spawn_watch(db: &Path, after: Option<i64>) -> thread::JoinHandle<Value> {
    let db = db.to_owned();
    let handle = thread::spawn(move || watch_for(&db, after, Duration::from_secs(20)));
    thread::sleep(Duration::from_millis(300));
    handle
}

/// The run's own attention; an ask about the run (with `ask_id`) is not.
fn run_attention_of<'a>(status: &'a Value, run_id: &RunId) -> Option<&'a Value> {
    status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_id"] == run_id.as_str() && a["ask_id"].is_null())
}

#[test]
fn attention_events_are_read_past_a_cursor_and_wake_watch() {
    let (_dir, repo, db, run) = awaiting_run();
    let queue = SqliteQueue::open(&db).unwrap();
    let latest = queue.latest_event_id().unwrap().as_i64();

    // `status` derives the attention from the queue as it is now. The
    // accepted run is the supervisor's to review; the stand-in `claude`
    // printed no verdict, so the review failed and the run waits for a
    // person in the `approve_landing` ask opened with the failure.
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["cursor"], json!(latest));
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    let ask = queue.asks(AskQuery::default()).unwrap()[0].id;
    assert!(
        status["attention"].as_array().unwrap().iter().any(|a| a
            == &json!({
                "run_id": run.id(), "task_id": 1, "ask_id": ask, "status": "open",
                "kind": "ask_opened", "last_error": null, "next": format!("answer ask {ask}"),
                "reason_category": "scope",
            })),
        "{status}"
    );
    // `supervise --once` exited, so nothing supervises the queue.
    assert_eq!(status["attention"][0]["kind"], "supervisor_stopped");
    assert_eq!(status["attention"][0]["next"], "restart supervisor");

    // `events` defaults to attention, compact and without paths.
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    assert_eq!(events["cursor"], json!(latest));
    let listed = events["events"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{events}");
    assert_eq!(listed[0]["kind"], "ask_opened");
    assert_eq!(listed[0]["ask_id"], json!(ask));
    assert_eq!(listed[0]["next"], format!("answer ask {ask}"));
    assert_eq!(listed[0]["run_id"], json!(run.id()));
    let all = dagq::watch::events(&db, EventId::new(0), 1000, true).unwrap();
    let all_events = all["events"].as_array().unwrap();
    assert_eq!(all_events.len() as i64, latest);
    assert_eq!(all["cursor"], json!(latest));
    let ids: Vec<i64> = all_events
        .iter()
        .map(|e| e["id"].as_i64().unwrap())
        .collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "oldest first");
    let text = all.to_string();
    assert!(!text.contains(run.worktree_path().unwrap()), "{text}");
    assert!(!text.contains("\"receipt\""), "{text}");
    // A limit leaves the cursor on the last event returned.
    let page = dagq::watch::events(&db, EventId::new(0), 2, true).unwrap();
    assert_eq!(page["events"].as_array().unwrap().len(), 2);
    assert_eq!(page["cursor"], json!(ids[1]));
    let rest = dagq::watch::events(&db, EventId::new(ids[1]), 1000, true).unwrap();
    assert_eq!(rest["events"][0]["id"], json!(ids[2]));
    assert_eq!(
        dagq::watch::events(&db, EventId::new(latest), 100, false).unwrap(),
        json!({"events": [], "cursor": latest})
    );

    // An attention event already past the cursor returns at once.
    let started = Instant::now();
    let woke = watch_for(&db, Some(0), Duration::from_secs(20));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(woke["events"], events["events"]);
    assert_eq!(woke["cursor"], json!(latest));
    assert_eq!(woke["supervisors_changed"], false);
    assert_eq!(woke["supervisors"], json!([]));
    // Nothing new: the timeout returns empty and keeps the cursor.
    let started = Instant::now();
    let quiet = watch_for(&db, Some(latest), Duration::from_millis(300));
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(
        quiet,
        json!({"events": [], "supervisors_changed": false, "supervisors": [], "cursor": latest})
    );

    // A landing parked for a session the supervisor will resume wakes
    // nobody (ADR-0019) ...
    fs::remove_file(run.receipt_path().unwrap()).unwrap();
    let before = queue.latest_event_id().unwrap().as_i64();
    assert_eq!(
        integrate(&db, 1, &repo).unwrap()["outcome"],
        "needs_session"
    );
    assert_eq!(
        dagq::watch::events(&db, EventId::new(before), 100, false).unwrap()["events"],
        json!([])
    );
    // ... and neither does one whose resumes are used up: the supervisor
    // hands that run to a person as a `decide` ask (ADR-0024's
    // Consequences), whose `ask_opened` is the attention.
    for attempt in 1..=3 {
        queue
            .record_runtime_event(run.id(), "resume_started", json!({"attempt": attempt}))
            .unwrap();
        queue
            .record_runtime_event(
                run.id(),
                "resume_finished",
                json!({"attempt": attempt, "outcome": "unresolved", "status": "needs_session", "exhausted": attempt == 3}),
            )
            .unwrap();
    }
    let before = queue.latest_event_id().unwrap().as_i64();
    assert_eq!(
        integrate(&db, 1, &repo).unwrap()["outcome"],
        "needs_session"
    );
    let deferred = dagq::watch::events(&db, EventId::new(before), 100, true).unwrap();
    let deferred = deferred["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "integration_deferred")
        .unwrap()
        .clone();
    assert_eq!(deferred["status"], "needs_session");
    assert_eq!(deferred.get("next"), None, "{deferred}");
    assert_eq!(
        dagq::watch::events(&db, EventId::new(before), 100, false).unwrap()["events"],
        json!([])
    );
    let latest = queue.latest_event_id().unwrap().as_i64();
    let status = runtime::status(&db).unwrap();
    let parked = run_attention_of(&status, run.id()).unwrap();
    assert_eq!(parked["status"], "needs_session");
    assert_eq!(parked["kind"], "integration_deferred");
    assert_eq!(parked["next"], "resuming (runtime)");
    assert!(
        parked["last_error"]
            .as_str()
            .unwrap()
            .contains("receipt is missing")
    );
    assert_eq!(status["cursor"], json!(latest));

    // A canceled task needs nobody, whatever its last run was.
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE tasks SET status='canceled' WHERE id=1", [])
        .unwrap();
    assert!(run_attention_of(&runtime::status(&db).unwrap(), run.id()).is_none());
}

/// A session ended by a signal (exit 143, SIGTERM) is classified as
/// `session_killed` with its exit code and signal (ADR-0034), and `status`,
/// `show` and `stats` report the code next to the unchanged free text.
#[test]
fn a_session_killed_by_a_signal_is_classified_in_status_show_and_stats() {
    let (_dir, db, detail) = run_agent("commit work; receipt \"$(git rev-parse HEAD)\"; exit 143");
    let run = &detail.runs[0];
    assert_eq!(run.last_error(), Some("session exited with code 143"));
    let finished = payloads(&detail, "supervision_finished");
    assert_eq!(
        finished[0],
        &json!({"status": "failed", "exit_code": 143, "code": "session_killed", "signal": 15})
    );
    let status = runtime::status(&db).unwrap();
    let failed = run_attention_of(&status, run.id()).unwrap();
    assert_eq!(failed["last_error"], "session exited with code 143");
    assert_eq!(failed["last_error_code"], "session_killed");
    let view = dagq::view::task_detail(&detail, 10);
    assert_eq!(
        view["runs"][0]["last_error_code"], "session_killed",
        "{view}"
    );
    assert!(
        view["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["payload"]["code"] == "session_killed"),
        "{view}"
    );
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(
        stats["reason_codes"]["by_code"]["session_killed"], 1,
        "{stats}"
    );
    assert_eq!(
        stats["reason_codes"]["by_kind"]["supervision_finished"],
        json!({"session_killed": 1})
    );
    // `watch` / `events` keep the code in their compact form.
    let events = dagq::watch::events(&db, EventId::new(0), 1000, true).unwrap();
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "supervision_finished" && e["code"] == "session_killed"),
        "{events}"
    );
}

#[test]
fn status_reports_failed_runs_and_unanswered_exit_requests() {
    let (_dir, db, detail) = run_agent("commit work; receipt \"$(git rev-parse HEAD)\"; exit 7");
    let run = &detail.runs[0];
    let status = runtime::status(&db).unwrap();
    let failed = run_attention_of(&status, run.id()).unwrap();
    assert_eq!(failed["status"], "failed");
    // The failed run itself is the supervisor's triage; its triage failed
    // (the stub `claude` prints no verdict), which is a person's.
    assert_eq!(failed["kind"], "triage_failed");
    assert_eq!(failed["last_error"], "session exited with code 7");
    assert_eq!(failed["last_error_code"], "session_exit_code");
    assert_eq!(failed["next"], "triage by hand");
    let events = dagq::watch::events(&db, EventId::new(0), 100, false).unwrap();
    assert_eq!(events["events"].as_array().unwrap().len(), 1, "{events}");
    assert_eq!(events["events"][0]["kind"], "triage_failed");
    assert_eq!(events["events"][0]["next"], "triage by hand");
    // Before its triage, the failed run is the supervisor's.
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_events WHERE kind='triage_failed'", [])
        .unwrap();
    let status = runtime::status(&db).unwrap();
    let pending = run_attention_of(&status, run.id()).unwrap();
    assert_eq!(pending["kind"], "failed");
    assert_eq!(pending["next"], "triaging (runtime)");

    // A running run whose /exit request went unanswered, until its session exits.
    let (_dir, repo, db) = fixture();
    let pid = std::process::id();
    let orphan = orphan_run(&repo, &db, "owner", pid, pid);
    assert!(run_attention_of(&runtime::status(&db).unwrap(), orphan.id()).is_none());
    let mut queue = SqliteQueue::open(&db).unwrap();
    let watcher = spawn_watch(&db, None);
    queue
        .record_runtime_event(
            orphan.id(),
            "exit_request_timed_out",
            json!({"workspace_id": "ws-1", "timeout_secs": 120}),
        )
        .unwrap();
    // The timeout alone is no attention: the supervisor's stuck_exit ask is.
    queue
        .ask(NewAsk {
            kind: AskKind::StuckExit,
            task_id: None,
            run_id: Some(orphan.id().clone()),
            question: "send /exit".into(),
            options: Vec::new(),
            asked_by: "supervisor".into(),
            reason_category: dagq::domain::AskReason::RecoveryFailed,
            finding_id: None,
        })
        .unwrap();
    let woke = joined(watcher, "the watch thread to return");
    assert_eq!(woke["events"].as_array().unwrap().len(), 1, "{woke}");
    assert_eq!(woke["events"][0]["kind"], "ask_opened");
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, orphan.id()).is_none(), "{status}");
    queue.wrapper_exited(orphan.id(), pid, 0).unwrap();
    assert!(run_attention_of(&runtime::status(&db).unwrap(), orphan.id()).is_none());
}

/// A run the supervisor gives up (here: its wrapper never registers) keeps
/// its status without a lease; with its session gone, the supervisor itself
/// recovers it on its next pass and triages it (ADR-0024 decision 3). One
/// whose session may still live waits for `recover`. A `runtime_error` that
/// releases no lease is only a note.
#[test]
fn an_abandoned_run_is_recovered_and_triaged_by_the_supervisor() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.no_session = true;
    backend.registration_timeout = Duration::from_secs(1);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    let errors = outcome["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{outcome}");
    assert!(
        errors[0]["message"]
            .as_str()
            .unwrap()
            .contains("wrapper did not register within 1 seconds"),
        "{outcome}"
    );
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::Interrupted);
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let events = queue.run_events(run.id()).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    let error = position(&kinds, "runtime_error");
    assert_eq!(events[error].payload["lease_released"], true);
    let recovered = position(&kinds, "run_recovered");
    assert!(error < recovered, "{kinds:?}");
    assert_eq!(events[recovered].payload["by"], "supervisor");
    assert_eq!(events[recovered].payload["previous_status"], "starting");
    assert_eq!(events[recovered].payload["run"]["blockers"], json!([]));
    assert!(recovered < position(&kinds, "triage_started"), "{kinds:?}");
    assert_eq!(
        outcome["triaged"][0]["run_id"],
        json!(run.id()),
        "{outcome}"
    );
    assert_eq!(outcome["triaged"][0]["status"], "interrupted");

    // The stub `claude` prints no verdict: a person triages the run.
    let status = runtime::status(&db).unwrap();
    let waiting = run_attention_of(&status, run.id()).unwrap();
    assert_eq!(waiting["status"], "interrupted");
    assert_eq!(waiting["kind"], "triage_failed");
    assert_eq!(waiting["next"], "triage by hand");
    let woke = watch_for(&db, Some(cursor), Duration::from_secs(20));
    let kinds: Vec<&Value> = woke["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| &e["kind"])
        .collect();
    assert_eq!(
        kinds,
        [&json!("runtime_error"), &json!("triage_failed")],
        "{woke}"
    );

    // A run nobody leases whose session may still live is not recovered:
    // it waits for `recover`.
    add_ready_task(&mut queue, "abandoned", &[]);
    let pid = std::process::id();
    let abandoned = orphan_run(&repo, &db, "owner", pid, pid);
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases WHERE run_id=?1", [&abandoned.id()])
        .unwrap();
    let status = runtime::status(&db).unwrap();
    let still = run_attention_of(&status, abandoned.id()).unwrap();
    assert_eq!(still["next"], "recover run");
    supervise(&db, &repo, &backend).unwrap();
    assert_eq!(
        queue.run(abandoned.id()).unwrap().status(),
        RunStatus::Running
    );
    assert!(
        !queue
            .has_run_event(abandoned.id(), "run_recovered")
            .unwrap()
    );

    // A runtime error recorded on a leased run is not an attention.
    add_ready_task(&mut queue, "noted", &[]);
    let pid = std::process::id();
    let noted = orphan_run(&repo, &db, "owner", pid, pid);
    let cursor = queue.latest_event_id().unwrap().as_i64();
    queue
        .record_runtime_error(noted.id(), "a passing error", &ReasonCode::Other.into())
        .unwrap();
    assert!(run_attention_of(&runtime::status(&db).unwrap(), noted.id()).is_none());
    assert_eq!(
        dagq::watch::events(&db, EventId::new(cursor), 100, false).unwrap()["events"],
        json!([])
    );
    let quiet = watch_for(&db, Some(cursor), Duration::from_millis(300));
    assert_eq!(quiet["events"], json!([]));
}

#[test]
fn watch_returns_when_supervisor_registrations_or_health_change() {
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let pid = std::process::id();

    // A supervisor registers.
    let watcher = spawn_watch(&db, Some(cursor));
    queue.register_supervisor("first", pid, 2, VERSION).unwrap();
    let woke = joined(watcher, "the watch thread to return");
    assert_eq!(woke["events"], json!([]));
    assert_eq!(woke["supervisors_changed"], true);
    assert_eq!(woke["cursor"], json!(cursor));
    assert_eq!(woke["supervisors"][0]["pid"], json!(pid));
    assert_eq!(woke["supervisors"][0]["stale"], false);
    // A healthy supervisor clears the stopped attention.
    let status = runtime::status(&db).unwrap();
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["next"] != "restart supervisor"),
        "{status}"
    );

    // Its heartbeat goes stale.
    let watcher = spawn_watch(&db, Some(cursor));
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE supervisors SET heartbeat_at=0", [])
        .unwrap();
    let woke = joined(watcher, "the watch thread to return");
    assert_eq!(woke["supervisors_changed"], true);
    assert_eq!(woke["supervisors"][0]["stale"], true);
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["attention"][0]["kind"], "supervisor_stale");
    assert_eq!(status["attention"][0]["status"], "stale");
    assert_eq!(status["attention"][0]["pid"], json!(pid));
    assert_eq!(status["attention"][0]["next"], "restart supervisor");

    // A stale supervisor that stays stale does not wake a watch.
    let quiet = watch_for(&db, Some(cursor), Duration::from_millis(300));
    assert_eq!(quiet["supervisors_changed"], false);

    // Its registration disappears.
    let watcher = spawn_watch(&db, Some(cursor));
    assert!(queue.deregister_supervisor("first").unwrap());
    let woke = joined(watcher, "the watch thread to return");
    assert_eq!(woke["supervisors_changed"], true);
    assert_eq!(woke["supervisors"], json!([]));
    assert_eq!(
        runtime::status(&db).unwrap()["attention"][0]["kind"],
        "supervisor_stopped"
    );
}

/// The repository moves after a run was validated: every check against the
/// old binding fails until `rebind`, which is refused while a supervisor or
/// an `integrate` lives, and afterwards the queue lists, reports and lands
/// the awaiting run from the new checkout (ADR-0020).
#[test]
fn rebind_follows_a_moved_repository_and_the_awaiting_run_lands() {
    use dagq::infrastructure::adapters::{GitRepository, path_text};
    let (dir, repo, db, run) = awaiting_run();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    let old_common_dir = path_text(&GitRepository::inspect(&repo).unwrap().common_dir).unwrap();
    let moved = dir.path().join("moved repo");
    fs::rename(&repo, &moved).unwrap();
    let new_common_dir = path_text(&GitRepository::inspect(&moved).unwrap().common_dir).unwrap();
    let worktree = PathBuf::from(run.worktree_path().unwrap().to_owned());
    // The run worktree's `.git` file still points into the old repository.
    assert!(
        !Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .arg("status")
            .bounded_output()
            .unwrap()
            .status
            .success()
    );

    // Nothing rebinds implicitly.
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.assert_repository(&new_common_dir).is_err());
    assert!(queue.bind_repository(&new_common_dir).is_err());
    let refused = integrate(&db, 1, &moved).unwrap_err().to_string();
    assert!(refused.contains("the queue is bound to"), "{refused}");

    // A live supervisor, even one of another binary, blocks the rebind.
    queue
        .register_supervisor("live", std::process::id(), 1, "0.0.1")
        .unwrap();
    let refused = runtime::rebind(&db, &moved).unwrap_err().to_string();
    assert!(refused.contains("supervisor is running"), "{refused}");
    assert_eq!(
        queue.repository_binding().unwrap().as_deref(),
        Some(old_common_dir.as_str())
    );
    // A registration left behind by a dead one does not.
    assert!(queue.deregister_supervisor("live").unwrap());
    queue
        .register_supervisor("dead", dead_pid(), 1, VERSION)
        .unwrap();

    let rebound = runtime::rebind(&db, &moved).unwrap();
    assert_eq!(rebound["outcome"], "rebound", "{rebound}");
    assert_eq!(rebound["previous_git_common_dir"], json!(old_common_dir));
    assert_eq!(rebound["git_common_dir"], json!(new_common_dir));
    assert_eq!(
        rebound["worktrees"],
        json!([{"run_id": run.id(), "worktree_path": worktree, "repaired": true, "error": null}]),
        "{rebound}"
    );
    assert_eq!(
        queue.repository_binding().unwrap().as_deref(),
        Some(new_common_dir.as_str())
    );
    queue.assert_repository(&new_common_dir).unwrap();
    assert!(queue.assert_repository(&old_common_dir).is_err());
    // The change is recorded next to the supervisor logs.
    let log = fs::read_to_string(
        db.canonicalize()
            .unwrap()
            .with_file_name("logs")
            .join(runtime::REBIND_LOG),
    )
    .unwrap();
    let entry: Value = serde_json::from_str(log.trim()).unwrap();
    assert_eq!(entry["previous_git_common_dir"], json!(old_common_dir));
    assert_eq!(entry["git_common_dir"], json!(new_common_dir));
    // The worktree works again, so a resumed session could use it.
    git(&worktree, &["status"]);
    // Rebinding to the same repository changes nothing and logs nothing.
    let again = runtime::rebind(&db, &moved).unwrap();
    assert_eq!(again["outcome"], "unchanged", "{again}");
    assert_eq!(
        fs::read_to_string(
            db.canonicalize()
                .unwrap()
                .with_file_name("logs")
                .join(runtime::REBIND_LOG),
        )
        .unwrap(),
        log
    );

    let listed = queue
        .list(&dagq::application::TaskQuery {
            status: dagq::application::StatusFilter::Any,
            goal_id: None,
            limit: 20,
            before: None,
            full: false,
        })
        .unwrap();
    assert_eq!(serde_json::to_value(listed).unwrap()["total"], 2);
    runtime::status(&db).unwrap();
    let outcome = integrate(&db, 1, &moved).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let landed = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_landed(&moved, &landed, "test task", &seed);
}

/// An `integrate` in progress holds the old repository's paths as well.
#[test]
fn rebind_is_refused_while_a_run_is_integrating() {
    let (dir, repo, db, run) = awaiting_run();
    let main = git_out(&repo, &["rev-parse", "main"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .begin_integration(run.id(), "integrator", &sha(&main))
        .unwrap();
    let other = dir.path().join("other");
    fs::create_dir(&other).unwrap();
    git(&other, &["init", "-q", "-b", "main"]);
    git(
        &other,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "seed",
        ],
    );
    let refused = runtime::rebind(&db, &other).unwrap_err().to_string();
    assert!(refused.contains("is integrating"), "{refused}");
}

#[test]
fn integrate_registers_the_landed_follow_ups_as_draft_tasks_of_the_goal_once() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let goal = queue
        .add_goal(NewGoal {
            title: "goal".into(),
            description: String::new(),
            acceptance: "done".into(),
            constraints: String::new(),
            doc: None,
            draft: false,
        })
        .unwrap();
    queue.set_goal(TaskId::new(1), Some(goal.id())).unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let head = run.result_commit().cloned().unwrap();
    let follow_ups = json!([
        {"title": "later work", "description": "outside the task"},
        {"title": "  ", "description": "no title, not a task"},
        {"title": "more work", "description": ""},
        {"title": "no description"}
    ]);

    // A receipt that does not name the head parks the run: nothing landed,
    // so nothing is registered.
    let mut stale = session_receipt(&run, run.base_commit().as_str(), "succeeded", "stale");
    stale["follow_ups"] = follow_ups.clone();
    write_receipt_json(&run, stale);
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    assert!(events_of(&db, run.id(), "follow_up_registered").is_empty());
    assert_eq!(
        queue.list(&Default::default()).unwrap().total,
        1,
        "only the task itself"
    );

    // The landing registers each titled follow-up as a draft task of the goal.
    let mut receipt = session_receipt(&run, head.as_str(), "succeeded", "landed");
    receipt["follow_ups"] = follow_ups.clone();
    write_receipt_json(&run, receipt);
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(
        outcome["follow_ups"],
        json!([
            {"task_id": 2, "title": "later work"},
            {"task_id": 3, "title": "more work"}
        ])
    );
    let context = format!(
        "task 1（test task）の run {} の receipt が提案した follow_up",
        run.id()
    );
    for (id, title, description) in [(2, "later work", "outside the task"), (3, "more work", "")] {
        let detail = queue.show(TaskId::new(id)).unwrap();
        assert_eq!(detail.task.status(), TaskStatus::Draft);
        assert_eq!(detail.task.title(), title);
        assert_eq!(detail.task.description(), description);
        assert_eq!(detail.task.goal_id(), Some(goal.id()));
        assert_eq!(detail.task.context(), context);
        assert_eq!(detail.task.acceptance(), "");
        assert!(detail.task.verification_commands().is_empty());
        assert!(detail.dependencies.is_empty());
    }
    assert_eq!(
        events_of(&db, run.id(), "follow_up_registered"),
        vec![
            json!({"task_id": 2, "title": "later work", "index": 0}),
            json!({
                "task_id": null, "title": "  ", "index": 1,
                "skipped": "title is not a non-blank string",
                "follow_up": {"title": "  ", "description": "no title, not a task"},
            }),
            json!({"task_id": 3, "title": "more work", "index": 2}),
            json!({
                "task_id": null, "title": "no description", "index": 3,
                "skipped": "description is not a string",
                "follow_up": {"title": "no description"},
            }),
        ]
    );
    // Drafts are not picked up by the supervisor.
    assert!(queue.candidates().unwrap().is_empty());

    // The run is integrated: another integrate finds nothing to land, and
    // registering the same run's follow-ups again adds nothing.
    assert!(integrate(&db, 1, &repo).is_err());
    let task = queue.show(TaskId::new(1)).unwrap().task;
    assert!(
        runtime::register_follow_ups(&mut queue, &task, run.id(), Some(&follow_ups)).is_empty()
    );
    assert_eq!(queue.list(&Default::default()).unwrap().total, 2);
    assert_eq!(events_of(&db, run.id(), "follow_up_registered").len(), 4);

    // A closed goal takes no task: a new follow-up is registered without it.
    queue
        .close_goal(goal.id(), dagq::domain::GoalVerdict::Abandoned)
        .unwrap();
    let mut extended = follow_ups.as_array().unwrap().clone();
    extended.push(json!({"title": "after the goal", "description": "d"}));
    let added = runtime::register_follow_ups(&mut queue, &task, run.id(), Some(&json!(extended)));
    assert_eq!(added.len(), 1);
    let detail = queue.show(added[0].task_id).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Draft);
    assert_eq!(detail.task.goal_id(), None);
    assert_eq!(
        events_of(&db, run.id(), "follow_up_registered")[4],
        json!({"task_id": added[0].task_id, "title": "after the goal", "index": 4, "goal_closed": true})
    );
    // Nothing to register without follow_ups.
    assert!(runtime::register_follow_ups(&mut queue, &task, run.id(), None).is_empty());
}

#[test]
fn dagq_toml_run_env_reaches_the_workspace_and_the_verification_commands() {
    let (_dir, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        "[run.env]\nSHARED = '${DAGQ_QUEUE_DIR}/target'\nRUN_TMP = \"${DAGQ_RUN_DIR}\"\n",
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-m", "run env"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let task = queue
        .add(NewTask {
            title: "env task".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: vec![
                r#"printf '%s %s\n' "$SHARED" "$RUN_TMP" >> "$RUN_TMP/verify-env.txt""#.into(),
            ],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            dependencies: vec![],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    drop(queue);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = SqliteQueue::open(&db)
        .unwrap()
        .show(task.id())
        .unwrap()
        .runs[0]
        .clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let canonical = db.canonicalize().unwrap();
    let queue_dir = canonical.parent().unwrap().to_str().unwrap().to_owned();
    let run_dir = run.run_dir().unwrap().to_owned();
    // The workspace gets the expanded table after the runtime's own names.
    assert_eq!(
        backend.tags.lock().unwrap()[0].env,
        vec![
            ("DAGQ_ROLE".to_owned(), "worker".to_owned()),
            (
                "DAGQ_QUEUE".to_owned(),
                canonical.to_str().unwrap().to_owned()
            ),
            ("SHARED".to_owned(), format!("{queue_dir}/target")),
            ("RUN_TMP".to_owned(), run_dir.clone()),
        ]
    );
    // Validation runs no verification command (ADR-0023 decision 1).
    let seen = Path::new(&run_dir).join("verify-env.txt");
    assert!(!seen.exists());

    // `integrate` runs the command once after its rebase, with the env,
    // even when called from the run's own worktree.
    fs::write(repo.join("other.txt"), "main moved\n").unwrap();
    git(&repo, &["add", "other.txt"]);
    git(&repo, &["commit", "-m", "main moved"]);
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let outcome = integrate(&db, task.id().as_i64(), &worktree).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let line = format!("{queue_dir}/target {run_dir}\n");
    assert_eq!(fs::read_to_string(&seen).unwrap(), line);
}

#[test]
fn a_broken_dagq_toml_stops_provisioning_before_the_workspace() {
    let (_dir, repo, db) = fixture();
    fs::write(repo.join("dagq.toml"), "[build]\n").unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    // Like any provisioning failure it stops claiming; no workspace opens.
    let error = format!("{:#}", supervise(&db, &repo, &backend).unwrap_err());
    assert!(error.contains("unknown table [build]"), "{error}");
    assert!(backend.tags.lock().unwrap().is_empty());
}

/// A fixture whose only ready task requires `evidence` in the receipt.
fn evidence_fixture(evidence: &[EvidenceCheck]) -> (Fixture, PathBuf, PathBuf) {
    let (dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let task = queue
        .add(NewTask {
            title: "needs evidence".into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: evidence.to_vec(),
            paths: Vec::new(),
            priority: Default::default(),
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    assert_eq!(task.id(), TaskId::new(2));
    assert_eq!(task.required_evidence(), evidence);
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    (dir, repo, db)
}

/// A receipt function for scripts: `receipt_e2e COMMIT STATUS EVIDENCE`
/// claims success with the given `e2e` check.
const RECEIPT_E2E: &str = r#"receipt_e2e() {
  printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"passed","evidence_or_reason":"ran"},"e2e":{"status":"%s","evidence_or_reason":"%s"},"subagent_review":{"status":"passed","evidence_or_reason":"reviewed"},"summary":"done"}' "$RUN_ID" "$1" "$2" "$3" > "$RECEIPT.tmp"
  mv "$RECEIPT.tmp" "$RECEIPT"
}
"#;

/// A task that requires `e2e` evidence gets a receipt without it parked as
/// `needs_session` (`evidence_missing`), not failed; the supervisor resumes
/// the session with the evidence request, and the rewritten receipt with
/// the evidence brings the run to `awaiting_integration`.
#[test]
fn missing_required_evidence_parks_the_run_for_a_resumed_session() {
    let (_dir, repo, db) = evidence_fixture(&[EvidenceCheck::E2e]);
    // The worker claims e2e passed but gives no evidence: without the
    // requirement that fails the receipt, with it the run waits.
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!("{RECEIPT_E2E}commit work; receipt_e2e \"$(git rev-parse HEAD)\" passed ' '"),
    );
    backend.resume_script_for(
        2,
        &format!(
            "{RECEIPT_E2E}await_message; receipt_e2e \"$(git rev-parse HEAD)\" passed 'cargo test --test e2e: 3 passed'; idle; await_exit"
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    let run = &detail.runs[0];
    // The worker knew up front.
    let prompt = read_prompt(run);
    assert!(prompt.contains("Required evidence: e2e ("), "{prompt}");
    // Validation parked it instead of failing it; the resolved resume is
    // validated again with its session open (ADR-0027 decision 3).
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 2);
    assert_eq!(validated[1]["status"], "awaiting_integration");
    assert_eq!(validated[0]["status"], "needs_session");
    assert_eq!(validated[0]["accepted"], false);
    assert_eq!(validated[0]["reason"], "evidence missing: e2e");
    assert_eq!(validated[0]["evidence_missing"], json!(["e2e"]));
    assert_eq!(validated[0]["code"], "evidence_missing");
    assert_eq!(validated[1].get("code"), None);
    assert!(validated[0]["result_commit"].is_string());
    assert_eq!(
        payloads(&detail, "evidence_missing"),
        [
            &json!({"code": "evidence_missing", "checks": ["e2e"], "reason": "evidence missing: e2e"})
        ]
    );
    // The worker's workspace was closed: the resume opens its own.
    assert!(event_kinds(&detail).contains(&"workspace_closed"));
    assert!(backend.closed().contains(&WORKSPACE_ID.to_owned()));
    // The resume asked for the missing check, not a rebase.
    let text = &backend.texts()[0].1;
    assert!(
        text.contains("found required evidence missing from the receipt"),
        "{text}"
    );
    assert!(text.contains("Reason: evidence missing: e2e"), "{text}");
    assert!(!text.contains("git rebase"), "{text}");
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["outcome"], "resolved");
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_leases().unwrap().is_empty());
    // It lands now that the receipt carries the evidence.
    let landed = integrate(&db, 2, &repo).unwrap();
    assert_eq!(landed["outcome"], "integrated", "{landed}");
}

/// A required check the receipt reports as `failed` is missing evidence
/// too: the run waits for a session instead of failing, and a resume that
/// reruns the check brings it to `awaiting_integration`.
#[test]
fn a_required_check_reported_failed_parks_the_run_instead_of_failing_it() {
    let (_dir, repo, db) = evidence_fixture(&[EvidenceCheck::E2e]);
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "{RECEIPT_E2E}commit work; receipt_e2e \"$(git rev-parse HEAD)\" failed 'cmux was not running'"
        ),
    );
    backend.resume_script_for(
        2,
        &format!(
            "{RECEIPT_E2E}await_message; receipt_e2e \"$(git rev-parse HEAD)\" passed 'e2e: 5 passed'; idle; await_exit"
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated[0]["status"], "needs_session");
    assert_eq!(validated[0]["reason"], "evidence missing: e2e");
    assert_eq!(
        payloads(&detail, "evidence_missing"),
        [
            &json!({"code": "evidence_missing", "checks": ["e2e"], "reason": "evidence missing: e2e"})
        ]
    );
    assert_eq!(
        payloads(&detail, "resume_finished")[0]["outcome"],
        "resolved"
    );
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
}

/// With the required evidence in the first receipt, validation accepts the
/// run as it would without a requirement.
#[test]
fn required_evidence_present_in_the_receipt_awaits_integration() {
    let (_dir, repo, db) = evidence_fixture(&[EvidenceCheck::E2e, EvidenceCheck::Tests]);
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "{RECEIPT_E2E}commit work; receipt_e2e \"$(git rev-parse HEAD)\" passed 'e2e: 3 passed'"
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(read_prompt(run).contains("Required evidence: e2e, tests ("));
    assert!(!event_kinds(&detail).contains(&"evidence_missing"));
    assert!(
        !payloads(&detail, "validation_finished")[0]
            .as_object()
            .unwrap()
            .contains_key("evidence_missing")
    );
}

/// A resumed session that comes back without the evidence has not resolved
/// the run: every attempt is `unresolved`, and once the resumes are used up
/// the run is `failed` and waits for a person's `decide` ask. An `integrate`
/// of such a run does not land either: it defers the run with the missing
/// `checks`.
#[test]
fn a_resume_or_integrate_without_the_required_evidence_does_not_land() {
    let (_dir, repo, db) = evidence_fixture(&[EvidenceCheck::E2e]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.resume_script_for(
        2,
        "await_message; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 3);
    assert!(finished.iter().all(|f| f["outcome"] == "unresolved"));
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::Decide);
    // Parked for a session again (as an older runtime left it), the run is
    // still not landed by `integrate`.
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='needs_session' WHERE id=?1",
            [&detail.runs[0].id()],
        )
        .unwrap();
    let before = git_out(&repo, &["rev-parse", "main"]);
    let deferred = integrate(&db, 2, &repo).unwrap();
    assert_eq!(deferred["outcome"], "needs_session", "{deferred}");
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), before);
    let detail = queue.show(TaskId::new(2)).unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::NeedsSession);
    assert_eq!(run.last_error(), Some("evidence missing: e2e"));
    let parked = payloads(&detail, "integration_deferred");
    assert_eq!(parked.last().unwrap()["checks"], json!(["e2e"]));
}

/// A fixture whose only ready task declares `paths` (ADR-0029).
fn scope_fixture(paths: &[&str]) -> (Fixture, PathBuf, PathBuf) {
    let (dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let task = queue
        .add(NewTask {
            title: "scoped".into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            priority: Default::default(),
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    assert_eq!(task.id(), TaskId::new(2));
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    (dir, repo, db)
}

/// A run of a task declaring `docs/**` that changes `change.txt` is parked
/// by validation as `needs_session` (`scope_violation`, with the paths),
/// not accepted; the supervisor resumes the session with a request to take
/// the path out, and the resolved run is validated again and lands.
#[test]
fn a_change_outside_the_declared_paths_parks_the_run_for_a_resumed_session() {
    let (_dir, repo, db) = scope_fixture(&["docs/**", "*.md"]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.resume_script_for(
        2,
        "await_message; unlocked git rm -q change.txt && mkdir -p docs && printf 'doc\\n' > docs/a.md && unlocked git add docs && unlocked git commit -q -m 'keep to docs'; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    let run = &detail.runs[0];
    let prompt = read_prompt(run);
    assert!(
        prompt.contains("Paths you may change (globs from the repository root; `*` stays in one directory, `**` spans any depth): docs/**, *.md."),
        "{prompt}"
    );
    let reason = "changed paths outside the task's --paths: change.txt";
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 2);
    assert_eq!(validated[0]["status"], "needs_session");
    assert_eq!(validated[0]["accepted"], false);
    assert_eq!(validated[0]["reason"], reason);
    assert_eq!(validated[0]["scope_violation"], json!(["change.txt"]));
    assert_eq!(validated[0]["allowed_paths"], json!(["docs/**", "*.md"]));
    assert!(validated[0]["result_commit"].is_string());
    assert_eq!(
        payloads(&detail, "scope_violation"),
        [
            &json!({"code": "scope_violation", "paths": ["change.txt"], "allowed": ["docs/**", "*.md"], "reason": reason})
        ]
    );
    assert!(!event_kinds(&detail).contains(&"evidence_missing"));
    // The resume asked to take the path out, not for a rebase or evidence.
    let text = &backend.texts()[0].1;
    assert!(
        text.contains("changes paths outside the task's --paths (docs/**, *.md)"),
        "{text}"
    );
    assert!(text.contains(&format!("Reason: {reason}")), "{text}");
    assert!(text.contains("Take the changes to the paths"), "{text}");
    assert_eq!(
        payloads(&detail, "resume_finished")[0]["outcome"],
        "resolved"
    );
    assert_eq!(validated[1]["status"], "awaiting_integration");
    assert!(
        !validated[1]
            .as_object()
            .unwrap()
            .contains_key("scope_violation")
    );
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let landed = integrate(&db, 2, &repo).unwrap();
    assert_eq!(landed["outcome"], "integrated", "{landed}");
    assert_eq!(
        git_out(&repo, &["show", "--name-only", "--format=", "main"]).trim(),
        "docs/a.md"
    );
}

/// A run that changes only declared paths is validated and landed as if
/// the task declared none.
#[test]
fn a_change_inside_the_declared_paths_awaits_integration_and_lands() {
    let (_dir, repo, db) = scope_fixture(&["*.txt"]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(!event_kinds(&detail).contains(&"scope_violation"));
    let landed = integrate(&db, 2, &repo).unwrap();
    assert_eq!(landed["outcome"], "integrated", "{landed}");
}

/// Validation diffs from where the branch forked from the current main,
/// not from the base commit: a branch rebased onto a main that gained
/// `src/lib.rs` from another task changes only `change.txt` itself.
#[test]
fn a_branch_rebased_onto_a_moved_main_is_held_only_to_its_own_changes() {
    let (_dir, repo, db) = scope_fixture(&["*.txt"]);
    // Another task lands src/lib.rs on main while the worker runs, and the
    // worker rebases onto it before its receipt.
    let backend = TestWorkspace::new(
        &db,
        false,
        "git switch -q -c side main && mkdir -p src && printf 'x\\n' > src/lib.rs && git add src && git commit -q -m other && git update-ref refs/heads/main HEAD && git switch -q - && git branch -q -D side && commit work && git rebase -q main; receipt \"$(git rev-parse HEAD)\"",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    assert!(!event_kinds(&detail).contains(&"scope_violation"));
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    let landed = integrate(&db, 2, &repo).unwrap();
    assert_eq!(landed["outcome"], "integrated", "{landed}");
    assert_eq!(
        git_out(&repo, &["show", "--name-only", "--format=", "main"]).trim(),
        "change.txt"
    );
}

/// `integrate` holds the diff it squashes (main..rebased head) to the
/// task's paths after its rebase: a commit outside them that a session
/// added after validation defers the run to `needs_session` without moving
/// main or running the verification commands, and the supervisor's resume
/// asks to take it out and then lands the approved run.
#[test]
fn integrate_refuses_a_rebased_diff_outside_the_declared_paths() {
    let (_dir, repo, db) = scope_fixture(&["*.txt"]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    // main moves, so the landing rebases.
    fs::write(repo.join("other.txt"), "main moved\n").unwrap();
    git(&repo, &["add", "other.txt"]);
    git(&repo, &["commit", "-m", "main moved"]);
    let main = git_out(&repo, &["rev-parse", "main"]);
    // After validation the branch gains a path outside `*.txt`.
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    fs::create_dir_all(worktree.join("src")).unwrap();
    fs::write(worktree.join("src/lib.rs"), "// out of scope\n").unwrap();
    git(&worktree, &["add", "src"]);
    git(&worktree, &["commit", "-m", "outside"]);
    write_receipt(
        &run,
        &git_out(&worktree, &["rev-parse", "HEAD"]),
        "succeeded",
        "more",
    );

    let deferred = integrate(&db, 2, &repo).unwrap();
    assert_eq!(deferred["outcome"], "needs_session", "{deferred}");
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main);
    let detail = queue.show(TaskId::new(2)).unwrap();
    let parked = &detail.runs[0];
    assert_eq!(parked.status(), RunStatus::NeedsSession);
    let reason = parked.last_error().unwrap();
    assert!(
        reason.starts_with(&format!(
            "changed paths outside the task's --paths: src/lib.rs after the rebase onto main {}",
            main.trim()
        )),
        "{reason}"
    );
    assert!(event_kinds(&detail).contains(&"integration_rebased"));
    let payload = payloads(&detail, "integration_deferred")[0];
    assert_eq!(payload["scope_violation"], json!(["src/lib.rs"]));
    assert_eq!(payload["allowed"], json!(["*.txt"]));
    assert!(integration_verifications(&detail).is_empty());

    // The supervisor resumes it with the scope request and lands it.
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.resume_script_for(
        2,
        "await_message; unlocked git rm -q -r src && unlocked git commit -q -m 'back to scope'; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let text = &backend.texts()[0].1;
    assert!(
        text.contains("changes paths outside the task's --paths (*.txt)"),
        "{text}"
    );
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(
        detail.task.status(),
        TaskStatus::Completed,
        "{:?}",
        event_kinds(&detail)
    );
    assert_eq!(
        git_out(&repo, &["show", "--name-only", "--format=", "main"]).trim(),
        "change.txt"
    );
}

/// The observer's provider double: the headless job is a shell script in the
/// observation's directory, with the environment `observe` gives the agent.
struct ObserverProvider {
    script: String,
}
impl AgentProvider for ObserverProvider {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn command(&self, _: &TaskRun, _: &str) -> Result<CommandSpec> {
        bail!("the observer has no run")
    }
    fn resume_command(&self, _: &TaskRun) -> Result<CommandSpec> {
        bail!("the observer has no run")
    }
    fn headless_command(&self, cwd: &Path, prompt: &str, allowed: &[&str]) -> Result<CommandSpec> {
        assert!(prompt.contains("You are the observer"), "{prompt}");
        assert_eq!(allowed, ["Bash(dagq:*)"]);
        let mut command = CommandSpec::new("/bin/sh");
        command.current_dir(cwd).arg("-c").arg(&self.script);
        Ok(command)
    }
    fn review_command(&self, _: &TaskRun, _: &str) -> Result<CommandSpec> {
        bail!("the observer reviews no run")
    }
}

fn observe_options(mode: dagq::observer::ObserveMode) -> dagq::observer::ObserveOptions {
    dagq::observer::ObserveOptions {
        mode,
        since: None,
        dry_run: false,
        timeout: Duration::from_secs(60),
        dagq: PathBuf::from(env!("CARGO_BIN_EXE_dagq")),
    }
}

fn queue_events(db: &Path, kind: &str) -> Vec<Value> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT payload FROM run_events WHERE kind=?1 ORDER BY id")
        .unwrap()
        .query_map([kind], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|p| serde_json::from_str(&p.unwrap()).unwrap())
        .collect()
}

#[test]
fn observe_records_findings_and_a_blocked_ask_and_advances_the_cursor() {
    use dagq::observer::{ObserveMode, observe, read_cursor};
    let (_dir, _repo, db) = fixture();
    // `dagq` is first on PATH and the queue is in DAGQ_QUEUE; the state
    // changes the prompt forbids are refused by the CLI itself.
    let provider = ObserverProvider {
        script: r#"
set -e
printf '%s' "$DAGQ_ROLE" > role.txt
q() { dagq --db "$DAGQ_QUEUE" "$@" > /dev/null; }
q finding record --kind stall --task 1 --summary 'task 1 waits for a slot' --evidence 1
q finding record --kind stall --task 1 --summary 'task 1 waits for a slot' --evidence 1 --evidence 2
q finding record --kind capacity --queue --subject idle_slots --summary 'slots idle'
q ask --kind blocked --because recovery_failed --finding 2 --question 'slots idle while task 1 is ready' --option 'leave it' --cmux /usr/bin/true
q ask --kind blocked --because recovery_failed --finding 2 --question 'the same alert again' --cmux /usr/bin/true
if q ready 1 2> ready.err; then exit 3; fi
if q ask --kind decide --because recovery_failed --task 1 --question 'decide?' 2> ask.err; then exit 4; fi
if q goal ready 1 2> goal.err; then exit 5; fi
if q note --task 1 --text 'seen' 2> note.err; then exit 6; fi
if q goal add --draft 'claim faster' 2> draft.err; then exit 7; fi
echo 'recorded 2 findings, updated 1, wrote 1 ask'
"#
        .into(),
    };
    assert_eq!(read_cursor(&db).unwrap(), None);
    let first = observe(&db, &provider, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(first["outcome"], "succeeded", "{first}");
    assert_eq!(
        (
            &first["findings_recorded"],
            &first["findings_updated"],
            &first["asks"]
        ),
        (&json!(2), &json!(1), &json!(1))
    );
    assert_eq!(first["since"], Value::Null);
    let cursor = first["cursor"].as_i64().unwrap();
    assert!(cursor >= 0);
    assert_eq!(read_cursor(&db).unwrap(), Some(EventId::new(cursor)));
    let dir = PathBuf::from(first["dir"].as_str().unwrap());
    assert_eq!(
        dir.parent().unwrap(),
        db.canonicalize()
            .unwrap()
            .parent()
            .unwrap()
            .join("observer")
    );
    assert_eq!(
        fs::read_to_string(dir.join("role.txt")).unwrap(),
        "observer"
    );
    for denied in ["ready.err", "ask.err", "goal.err", "note.err", "draft.err"] {
        assert!(
            fs::read_to_string(dir.join(denied))
                .unwrap()
                .contains("observer may not change queue state"),
            "{denied}"
        );
    }
    assert!(
        fs::read_to_string(dir.join("output.log"))
            .unwrap()
            .contains("recorded 2 findings")
    );
    assert!(
        fs::read_to_string(dir.join("prompt.md"))
            .unwrap()
            .contains("\"stats\"")
    );
    // Nothing changed state: the task is still ready and no goal was added.
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Ready
    );
    assert!(queue.show_goal(GoalId::new(1)).is_err());
    let asks = queue
        .asks(dagq::infrastructure::asks::AskQuery::default())
        .unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind.as_str(), "blocked");
    assert_eq!(asks[0].task_id, None);
    assert_eq!(asks[0].asked_by, "observer");
    assert_eq!(asks[0].finding_id, Some(dagq::domain::FindingId::new(2)));
    let findings = queue
        .findings(&dagq::domain::FindingQuery::default())
        .unwrap();
    assert_eq!(findings.len(), 2);
    let stall = findings.iter().find(|f| f.finding.kind == "stall").unwrap();
    assert_eq!(stall.finding.occurrences, 2);
    assert_eq!(stall.finding.recorded_by, "observer");
    assert_eq!(queue_events(&db, "observe_started").len(), 1);
    assert_eq!(
        queue_events(&db, "observe_finished"),
        std::slice::from_ref(&first)
    );

    // The next observation reads past the saved cursor; a failed one keeps it.
    let failing = ObserverProvider {
        script: "exit 7".into(),
    };
    let second = observe(&db, &failing, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(second["since"], cursor);
    assert_eq!(second["outcome"], "failed");
    assert_eq!(second["exit_code"], 7);
    assert_eq!(second["cursor_saved"], false);
    assert_eq!(read_cursor(&db).unwrap(), Some(EventId::new(cursor)));

    // A dry run only returns the prompt.
    let dry = observe(
        &db,
        &failing,
        &dagq::observer::ObserveOptions {
            dry_run: true,
            since: Some(EventId::new(0)),
            ..observe_options(ObserveMode::Daily)
        },
    )
    .unwrap();
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["since"], 0);
    let prompt = dry["prompt"].as_str().unwrap();
    assert!(prompt.contains("daily observation"), "{prompt}");
    assert!(prompt.contains("ask --kind blocked"), "{prompt}");
    assert!(prompt.contains("finding record --kind"), "{prompt}");
    assert!(prompt.contains("--finding ID"), "{prompt}");
    assert!(!prompt.contains("goal add --draft"), "{prompt}");
    assert!(prompt.contains("\"findings\""), "{prompt}");
    assert!(
        prompt.contains("slots idle while task 1 is ready"),
        "{prompt}"
    );
    assert!(prompt.contains("task 1 waits for a slot"), "{prompt}");
    assert!(prompt.contains("idle_slots"), "{prompt}");
    assert_eq!(queue_events(&db, "observe_started").len(), 2);

    // An agent that cannot start is an error outcome, not a failed observe.
    let broken = observe(
        &db,
        &TestProvider {
            script: String::new(),
            db: db.clone(),
        },
        &observe_options(ObserveMode::Daily),
    )
    .unwrap();
    assert_eq!(broken["outcome"], "error");
    assert!(
        broken["error"]
            .as_str()
            .unwrap()
            .contains("no headless execution")
    );
    // The daily one reads the last 24 hours and leaves the cursor alone.
    assert_eq!(broken["since"], 0);
    assert_eq!(read_cursor(&db).unwrap(), Some(EventId::new(cursor)));
}

#[test]
fn observe_kills_an_agent_past_its_timeout() {
    use dagq::observer::{ObserveMode, observe};
    let (_dir, _repo, db) = fixture();
    let slow = ObserverProvider {
        script: "sleep 30".into(),
    };
    let started = Instant::now();
    let outcome = observe(
        &db,
        &slow,
        &dagq::observer::ObserveOptions {
            timeout: Duration::from_millis(300),
            ..observe_options(ObserveMode::Hourly)
        },
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(outcome["outcome"], "error");
    assert!(
        outcome["error"]
            .as_str()
            .unwrap()
            .contains("did not finish")
    );
    assert_eq!(outcome["cursor_saved"], false);
}

/// A Claude Code stand-in for the supervisor's observer: `--version` for
/// the preflight, and in print mode (`-p`) a finding through the queue CLI it
/// finds first on PATH.
fn observer_claude_stub(db: &Path) -> PathBuf {
    let stub = db.parent().unwrap().join("claude-observer-stub");
    fs::write(
        &stub,
        r#"#!/bin/sh
if [ "$1" = "-p" ]; then
  mode=hourly
  case "$*" in *"daily observation"*) mode=daily ;; esac
  exec dagq --db "$DAGQ_QUEUE" finding record --goal 1 --kind observed --subject "$mode" --summary "observed by $DAGQ_ROLE"
fi
printf 'test provider\n'
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    stub
}

#[test]
fn supervisor_starts_the_observer_on_its_interval_without_a_run_slot() {
    let (_dir, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        queue
            .transition(TaskId::new(1), TaskAction::Cancel)
            .unwrap();
        queue
            .add_goal(NewGoal {
                title: "observed".into(),
                description: String::new(),
                acceptance: String::new(),
                constraints: String::new(),
                doc: None,
                draft: false,
            })
            .unwrap();
    }
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = SuperviseOptions {
        observe_interval: Duration::from_secs(3600),
        observe_daily: true,
        ..SuperviseOptions::new(1, true)
    };
    let supervise_observed = || {
        runtime::supervise(
            &db,
            &repo,
            &backend,
            &observer_claude_stub(&db),
            Path::new(env!("CARGO_BIN_EXE_dagq")),
            &options,
        )
        .unwrap()
    };
    // Nothing was ever observed: the daily observation is due, then the
    // hourly one; `--once` waits for each before it exits.
    let outcome = supervise_observed();
    assert_eq!(outcome["runs"], json!([]));
    let finished = queue_events(&db, "observe_finished");
    assert_eq!(
        finished
            .iter()
            .map(|f| (f["mode"].as_str().unwrap(), f["outcome"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        [("daily", "succeeded"), ("hourly", "succeeded")]
    );
    assert!(finished.iter().all(|f| f["findings_recorded"] == 1));
    let mut findings = SqliteQueue::open(&db)
        .unwrap()
        .findings(&dagq::domain::FindingQuery {
            target: Some(dagq::domain::FindingTarget::Goal(GoalId::new(1))),
            ..Default::default()
        })
        .unwrap();
    findings.sort_by_key(|f| f.finding.id);
    assert_eq!(
        findings
            .iter()
            .map(|f| (f.finding.subject.as_str(), f.finding.summary.as_str()))
            .collect::<Vec<_>>(),
        [
            ("daily", "observed by observer"),
            ("hourly", "observed by observer")
        ]
    );
    assert!(dagq::observer::read_cursor(&db).unwrap().is_some());
    // Within the interval nothing is due again, even for another supervisor.
    supervise_observed();
    assert_eq!(queue_events(&db, "observe_started").len(), 2);
    // An interval of 0 disables the observer.
    let disabled = SuperviseOptions {
        observe_interval: Duration::ZERO,
        ..options.clone()
    };
    fs::remove_file(db.parent().unwrap().join("observer/cursor")).unwrap();
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_events SET created_at='2000-01-01T00:00:00.000Z' WHERE kind LIKE 'observe_%'",
            [],
        )
        .unwrap();
    runtime::supervise(
        &db,
        &repo,
        &backend,
        &observer_claude_stub(&db),
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &disabled,
    )
    .unwrap();
    assert_eq!(queue_events(&db, "observe_started").len(), 2);
    // Once the interval passed, the next pass observes again.
    supervise_observed();
    assert_eq!(queue_events(&db, "observe_started").len(), 4);
}

/// Stands in for the headless reviewer (ADR-0027): each review runs the
/// next script with `/bin/sh -c` in the worktree (the last one repeats) and
/// records its prompt; `timeout` is the review timeout.
struct TestReviewer {
    scripts: Mutex<Vec<String>>,
    prompts: Mutex<Vec<String>>,
    timeout: Duration,
    /// Scripts of the headless triage, one per triage in order; without
    /// one left, a triage cannot start.
    triages: Mutex<Vec<String>>,
    /// The triage prompts and the directories they ran in.
    triage_prompts: Mutex<Vec<(String, PathBuf)>>,
}

impl TestReviewer {
    fn new(scripts: &[String]) -> Self {
        Self {
            scripts: Mutex::new(scripts.to_vec()),
            prompts: Mutex::new(Vec::new()),
            timeout: Duration::from_secs(60),
            triages: Mutex::new(Vec::new()),
            triage_prompts: Mutex::new(Vec::new()),
        }
    }
    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
    fn with_triages(self, scripts: &[String]) -> Self {
        *self.triages.lock().unwrap() = scripts.to_vec();
        self
    }
    fn triage_prompts(&self) -> Vec<(String, PathBuf)> {
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
fn verdict(decision: &str, reasons: &[&str], summary: &str) -> String {
    let json = json!({"verdict": decision, "reasons": reasons, "summary": summary});
    format!("printf '%s\\n' '{json}'")
}

fn supervise_reviewed(
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
fn position(kinds: &[&str], kind: &str) -> usize {
    kinds
        .iter()
        .position(|k| *k == kind)
        .unwrap_or_else(|| panic!("no {kind} in {kinds:?}"))
}

/// The worker goes idle after its receipt and never exits by itself; each
/// time a text arrives in its terminal it appends a line, commits, rewrites
/// the receipt and goes idle again, `revises` times.
fn revising_agent(revises: usize) -> String {
    format!(
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
         for n in $(seq 1 {revises}); do \
           while [ ! -f \"$MESSAGE\" ]; do sleep 0.1; done; rm \"$MESSAGE\"; \
           printf 'fix %s\\n' \"$n\" >> change.txt; git commit -q -am \"fix $n\"; \
           receipt \"$(git rev-parse HEAD)\"; idle; \
         done; await_exit"
    )
}

/// The `send_exit` attempts that failed, as (attempt, max_attempts,
/// retry_after_ms).
fn exit_attempts(detail: &dagq::domain::TaskDetail) -> Vec<(Value, Value, Value)> {
    backend_failures(detail)
        .into_iter()
        .filter(|e| e.payload["op"] == "send_exit")
        .map(|e| {
            (
                e.payload["attempt"].clone(),
                e.payload["max_attempts"].clone(),
                e.payload["retry_after_ms"].clone(),
            )
        })
        .collect()
}

/// A `/exit` that cmux timed out before it reached the session (the screen
/// shows the input box and no trace of it) is sent again after a backoff,
/// and the run goes on without being given up (task 354).
#[test]
fn an_exit_that_timed_out_before_reaching_the_session_is_sent_again() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(1, Ordering::SeqCst);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 2);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(exit_attempts(&detail), [(json!(1), json!(3), json!(10))]);
    let kinds = event_kinds(&detail);
    // The session exited on the second /exit: nothing more to record.
    assert!(position(&kinds, "exit_requested") < position(&kinds, "session_exited"));
    for kind in ["exit_unsent", "exit_request_timed_out", "runtime_error"] {
        assert!(!kinds.contains(&kind), "{kinds:?}");
    }
}

/// A `/exit` that never got there on any attempt leaves a run whose review
/// passed and whose receipt still holds against a clean worktree to land:
/// the workspace is closed instead of waiting on a session that was never
/// asked, and `exit_unsent` records it (task 354).
#[test]
fn an_exit_that_never_got_there_closes_a_sound_passed_run_and_lands_it() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(usize::MAX, Ordering::SeqCst);
    backend.close_ends_session = true;
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    // Typed once and twice again, never more.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 3);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert_eq!(
        exit_attempts(&detail),
        [
            (json!(1), json!(3), json!(10)),
            (json!(2), json!(3), json!(20)),
            (json!(3), json!(3), Value::Null),
        ]
    );
    assert_eq!(
        payloads(&detail, "exit_unsent"),
        [
            &json!({"code": "backend_timeout", "workspace_id": WORKSPACE_ID, "attempts": 3, "action": "close_and_land"})
        ]
    );
    let kinds = event_kinds(&detail);
    for (earlier, later) in [
        ("review_finished", "exit_requested"),
        ("exit_requested", "exit_unsent"),
        ("exit_unsent", "workspace_closed"),
        ("workspace_closed", "run_integrated"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    for kind in ["exit_request_timed_out", "runtime_error"] {
        assert!(!kinds.contains(&kind), "{kinds:?}");
    }
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
}

/// A `/exit` that never got there leaves a run that does not land on its
/// own (here its review failed) to the person, as a session that held its
/// `/exit` back is: the `stuck_exit` ask, at once, and the run kept (task
/// 354).
#[test]
fn an_exit_that_never_got_there_asks_for_a_run_that_cannot_land() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(usize::MAX, Ordering::SeqCst);
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_lease(run.id()).unwrap().is_some());
    assert_eq!(
        payloads(&detail, "exit_unsent"),
        [&json!({
            "code": "backend_timeout", "workspace_id": WORKSPACE_ID, "attempts": 3,
            "action": "ask", "held": "the run does not land after its exit",
        })]
    );
    assert_eq!(
        payloads(&detail, "exit_request_timed_out"),
        [
            &json!({"code": "exit_timeout", "workspace_id": WORKSPACE_ID, "timeout_secs": 120, "unsent": true})
        ]
    );
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::StuckExit);
    assert!(
        asks[0].question.contains("(exit_unsent)"),
        "{}",
        asks[0].question
    );
    // The person has the session exit; the run goes on as before.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 3);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "exit_unsent") < position(&kinds, "session_exited"));
    assert!(position(&kinds, "session_exited") < position(&kinds, "review_failed"));
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert!(queue.read_ask(asks[0].id).unwrap().closed_at.is_some());
}

/// A sound passed run whose workspace cannot be closed either is not
/// landed with its session alive: the `stuck_exit` ask says why (task 354).
#[test]
fn an_exit_that_never_got_there_asks_when_the_workspace_cannot_be_closed() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(usize::MAX, Ordering::SeqCst);
    backend.close_times_out = true;
    let backend = Arc::new(backend);
    let reviewer = Arc::new(TestReviewer::new(&[verdict(
        "pass",
        &[],
        "meets the acceptance",
    )]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || {
            let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &supervise_options(4, true),
            )
        })
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    let unsent = payloads(&detail, "exit_unsent");
    assert_eq!(unsent[0]["action"], "ask", "{unsent:?}");
    assert!(
        unsent[0]["held"]
            .as_str()
            .unwrap()
            .starts_with("its workspace could not be closed"),
        "{unsent:?}"
    );
    assert!(run.workspace_closed_at().is_none());
    // The person has the session exit; the run lands without a close.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
}

/// A passed run whose worktree changed after its review does not land
/// without its session's exit: the `/exit` that never got there is the
/// `stuck_exit` ask, saying why (task 354).
#[test]
fn an_exit_that_never_got_there_asks_for_a_passed_run_whose_worktree_changed() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(usize::MAX, Ordering::SeqCst);
    let backend = Arc::new(backend);
    // The review runs in the worktree; this one leaves a file behind.
    let reviewer = Arc::new(TestReviewer::new(&[format!(
        "printf 'x\\n' > stray.txt; {}",
        verdict("pass", &[], "meets the acceptance")
    )]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || supervise_reviewed(&db, &repo, &backend, &reviewer))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    let unsent = payloads(&detail, "exit_unsent");
    assert_eq!(unsent.len(), 1, "{unsent:?}");
    assert_eq!(unsent[0]["action"], "ask");
    let held = unsent[0]["held"].as_str().unwrap();
    assert!(
        held.starts_with("its receipt no longer holds: worktree is not clean"),
        "{held}"
    );
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks[0].kind, AskKind::StuckExit);
    assert!(asks[0].question.contains(held), "{}", asks[0].question);
    // The person cleans up and has the session exit: the run lands.
    fs::remove_file(Path::new(run.worktree_path().unwrap()).join("stray.txt")).unwrap();
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 3);
}

/// A receipt accepted with the session still open is reviewed before the
/// session is asked to exit (ADR-0027 decision 1); on `pass` the supervisor
/// sends `/exit`, closes the workspace and lands the run without anyone
/// calling `integrate`.
#[test]
fn a_passing_review_exits_the_live_session_and_lands_it() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert!(queue.run_leases().unwrap().is_empty());
    let kinds = event_kinds(&detail);
    // The session lives through validation and review; /exit comes after
    // the verdict, the landing after the close.
    for (earlier, later) in [
        ("session_idle_observed", "supervision_finished"),
        ("supervision_finished", "validation_finished"),
        ("validation_finished", "review_started"),
        ("review_started", "review_finished"),
        ("review_finished", "exit_requested"),
        ("exit_requested", "session_exited"),
        ("session_exited", "workspace_closed"),
        ("workspace_closed", "integration_started"),
        ("integration_started", "run_integrated"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    assert!(!kinds.contains(&"integration_approved"), "{kinds:?}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
    let supervised = payloads(&detail, "supervision_finished");
    assert_eq!(
        supervised[0],
        &json!({"status": "validating", "exit_code": null, "session_live": true})
    );
    let started = payloads(&detail, "review_started");
    assert_eq!(
        started,
        [&json!({"attempt": 1, "workspace_id": WORKSPACE_ID, "session_live": true})]
    );
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["verdict"], "pass");
    assert_eq!(finished[0]["reasons"], json!([]));
    assert_eq!(finished[0]["summary"], "meets the acceptance");
    assert_eq!(finished[0]["attempt"], 1);
    assert!(finished[0]["duration_secs"].is_u64());
    // The reviewer reads review.md and is told the acceptance, the schema
    // and the verdicts.
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 1);
    let run_dir = Path::new(run.run_dir().unwrap());
    let review_md = run_dir.join("review.md");
    assert!(review_md.is_file());
    for expected in [
        format!("Read the review material at {}", review_md.display()),
        "Acceptance criteria of the task:\nworks".to_owned(),
        r#"{"verdict": "pass" | "revise" | "concern", "reasons": [string], "summary": string}"#
            .to_owned(),
        "- revise: findings the worker can fix without a person's judgment".to_owned(),
        "- concern: findings that need a person's judgment".to_owned(),
    ] {
        assert!(
            prompts[0].contains(&expected),
            "{expected:?} not in {}",
            prompts[0]
        );
    }
    assert_eq!(
        fs::read_to_string(run_dir.join("review-prompt-1.txt")).unwrap(),
        prompts[0]
    );
    assert!(run_dir.join("terminal-final.txt").is_file());
    // Nothing waits for anyone.
    assert!(run_attention_of(&runtime::status(&db).unwrap(), run.id()).is_none());
}

/// A `revise` verdict goes to the live session as a fixed request; once
/// the session rewrote its receipt for a new head and went idle, the run is
/// validated and reviewed again, and lands on `pass` (ADR-0027 decision 2).
#[test]
fn a_revise_verdict_is_fixed_by_the_live_session_and_reviewed_again() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &revising_agent(1));
    let reviewer = TestReviewer::new(&[
        verdict("revise", &["add a line to change.txt"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        format!("change by {}\nfix 1\n", run.id())
    );
    let requested = payloads(&detail, "revise_requested");
    assert_eq!(requested.len(), 1);
    assert_eq!(requested[0]["attempt"], 1);
    assert_eq!(requested[0]["reasons"], json!(["add a line to change.txt"]));
    let revised = payloads(&detail, "revise_finished");
    assert_eq!(revised.len(), 1);
    let head = git_out(
        &repo,
        &["rev-parse", &format!("refs/dagq/runs/{}", run.id())],
    );
    assert_eq!(revised[0], &json!({"attempt": 1, "head": head}));
    let verdicts: Vec<&Value> = payloads(&detail, "review_finished")
        .iter()
        .map(|p| &p["verdict"])
        .collect();
    assert_eq!(verdicts, [&json!("revise"), &json!("pass")]);
    assert_eq!(payloads(&detail, "validation_finished").len(), 2);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "revise_requested") < position(&kinds, "revise_finished"));
    assert!(position(&kinds, "revise_finished") < position(&kinds, "exit_requested"));
    // One /exit, after the second review; the request named the findings.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let texts = backend.texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0].0, WORKSPACE_ID);
    let text = &texts[0].1;
    for expected in [
        format!(
            "dagq: the supervisor's review of run {} (task 1) asks for changes (revise 1 of 2).",
            run.id()
        ),
        "Findings:\n- add a line to change.txt".to_owned(),
        "[\"test -f seed.txt\"]".to_owned(),
        format!(
            "Rewrite the receipt at {} with the new head commit",
            run.receipt_path().unwrap()
        ),
        runtime::STOP_BACKGROUND.to_owned(),
        "Do not merge or push. When done, report briefly and stop; do not run /exit.".to_owned(),
    ] {
        assert!(text.contains(&expected), "{expected:?} not in {text}");
    }
    let run_dir = Path::new(run.run_dir().unwrap());
    assert_eq!(
        &fs::read_to_string(run_dir.join("revise-1.txt")).unwrap(),
        text
    );
}

/// Revise is sent at most twice; a third review that does not pass becomes
/// an `approve_landing` ask after `/exit` and the close. `land` then lands
/// the run as an approved one.
#[test]
fn a_third_review_that_does_not_pass_asks_a_person_and_land_lands_it() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &revising_agent(2));
    let reviewer = TestReviewer::new(&[verdict("revise", &["still short"], "not yet")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(payloads(&detail, "revise_requested").len(), 2);
    assert_eq!(payloads(&detail, "revise_finished").len(), 2);
    assert_eq!(payloads(&detail, "review_finished").len(), 3);
    assert_eq!(backend.texts().len(), 2);
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
    assert!(queue.run_leases().unwrap().is_empty());
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "workspace_closed") < position(&kinds, "ask_opened"));
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    let ask = &asks[0];
    assert_eq!(ask.kind, dagq::domain::AskKind::ApproveLanding);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.options, ["land", "send_back", "cancel"]);
    assert_eq!(ask.asked_by, "supervisor");
    assert!(
        ask.question.contains(
            "returned revise (the review still asks for changes after 2 revises): not yet"
        ),
        "{}",
        ask.question
    );
    assert!(ask.question.contains("\n- still short"), "{}", ask.question);
    // The ask is the attention, for the inbox; the run itself is not one.
    let status = runtime::status(&db).unwrap();
    assert!(
        run_attention_of(&status, run.id()).is_none() || {
            let entries: Vec<&Value> = status["attention"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|a| a["run_id"] == json!(run.id()))
                .collect();
            entries.iter().all(|a| a["ask_id"] == json!(ask.id))
        }
    );
    let entry = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["ask_id"] == json!(ask.id))
        .unwrap();
    assert_eq!(entry["next"], format!("answer ask {}", ask.id));

    queue.answer(ask.id, "land").unwrap();
    let status = runtime::status(&db).unwrap();
    let entry = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["ask_id"] == json!(ask.id))
        .unwrap();
    assert_eq!(
        entry["next"],
        format!("applying the answer of ask {} (runtime)", ask.id)
    );
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed_run(&detail.runs[0], &repo, &base);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let approved = payloads(&detail, "integration_approved");
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0]["ask_id"], ask.id.as_i64());
    // No fourth review.
    assert_eq!(reviewer.prompts().len(), 3);
}

fn assert_landed_run(run: &TaskRun, repo: &Path, base: &str) {
    assert_landed(repo, run, "test task", base);
}

/// A `concern` exits and closes the session and asks a person; `send_back`
/// parks the run for a resume whose request names the findings, and the
/// resumed session goes through validation and review like the worker's
/// (ADR-0027 decision 3): the review passes and the supervisor lands it.
#[test]
fn a_concern_sent_back_is_resumed_reviewed_again_and_landed() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[
        verdict(
            "concern",
            &["changes a file the task did not name"],
            "scope",
        ),
        verdict("pass", &[], "fixed"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(backend.texts().is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "review_finished") < position(&kinds, "exit_requested"));
    assert!(position(&kinds, "workspace_closed") < position(&kinds, "ask_opened"));
    let ask = queue.asks(Default::default()).unwrap()[0].clone();
    assert!(
        ask.question.contains("returned concern: scope"),
        "{}",
        ask.question
    );
    // The ask was opened through `runtime::ask`, which notifies the inbox.
    let notified = backend.notifications.lock().unwrap().clone();
    assert_eq!(notified.len(), 1, "{notified:?}");
    assert!(
        notified[0].1.contains(&format!("run {}", run.id())),
        "{notified:?}"
    );

    queue.answer(ask.id, "send_back").unwrap();
    backend.resume_script_for(
        1,
        "await_message; printf 'narrowed\\n' > change.txt; unlocked git commit -q -am narrowed; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let landed = detail.runs[0].clone();
    assert_landed_run(&landed, &repo, &base);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "narrowed\n"
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let decided = payloads(&detail, "landing_decided");
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0]["answer"], "send_back");
    assert_eq!(decided[0]["status"], "needs_session");
    let text = &backend.texts()[0].1;
    assert!(
        text.contains("raised findings a person sent back to you"),
        "{text}"
    );
    assert!(
        text.contains("changes a file the task did not name"),
        "{text}"
    );
    assert!(text.contains("Fix the findings in the reason"), "{text}");
    // The resumed session stayed open through the review: validation,
    // review, then /exit and the close of its workspace, then the landing.
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1, "{finished:?} {:?}", event_kinds(&detail));
    assert_eq!(finished[0]["outcome"], "resolved");
    assert_eq!(finished[0]["status"], "validating");
    assert_eq!(finished[0]["workspace_closed"], false);
    assert_eq!(finished[0]["session_live"], true);
    let resume_workspace = finished[0]["workspace_id"].as_str().unwrap().to_owned();
    let kinds = event_kinds(&detail);
    let after_resume: Vec<&str> = kinds
        .iter()
        .skip_while(|k| **k != "resume_finished")
        .filter(|k| {
            matches!(
                **k,
                "validation_finished"
                    | "review_started"
                    | "review_finished"
                    | "exit_requested"
                    | "workspace_closed"
                    | "integration_started"
                    | "run_integrated"
            )
        })
        .copied()
        .collect();
    assert_eq!(
        after_resume,
        [
            "validation_finished",
            "review_started",
            "review_finished",
            "exit_requested",
            "workspace_closed",
            "integration_started",
            "run_integrated",
        ]
    );
    let closed = payloads(&detail, "workspace_closed");
    assert_eq!(
        closed.last().unwrap(),
        &&json!({"workspace_id": resume_workspace, "resume_attempt": 1})
    );
    assert!(backend.closed().contains(&resume_workspace));
    assert_eq!(payloads(&detail, "review_started")[1]["session_live"], true);
    assert_eq!(reviewer.prompts().len(), 2);
}

/// `cancel` fails the run and cancels its task.
#[test]
fn a_concern_canceled_fails_the_run_and_cancels_the_task() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("concern", &["not wanted"], "drop it")]);
    supervise_reviewed(&db, &repo, &backend, &reviewer);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue.asks(Default::default()).unwrap()[0].clone();
    queue.answer(ask.id, "cancel").unwrap();
    supervise_reviewed(&db, &repo, &backend, &reviewer);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert_eq!(
        detail.runs[0].last_error(),
        Some(format!("canceled by ask {}", ask.id).as_str())
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert_eq!(reviewer.prompts().len(), 1);
}

/// A headless review that fails (a non-zero exit, stdout without a verdict,
/// or the timeout) exits and closes the session, and in the step that
/// records `review_failed` opens an `approve_landing` ask with the failure
/// and where the review material is (task 328). Stdout without a readable
/// verdict (here an unescaped quote, or prose) is reviewed
/// once more first; a job that failed is not. The ask, not the failure, is
/// the attention, and its answer is applied by the supervisor: `land`
/// lands the run, `cancel` fails it and cancels the task.
#[test]
fn a_failed_review_closes_the_session_and_asks_a_person_in_the_same_step() {
    let unquoted = r#"printf '%s\n' '{"verdict":"pass","reasons":[],"summary":"says "fine""}'"#;
    for (script, timeout, expected, reviews, answer) in [
        (
            "echo broken >&2; exit 3",
            60,
            "exited with exit status: 3: broken",
            1,
            "land",
        ),
        (
            "echo 'no verdict here'",
            60,
            "the review printed no verdict JSON",
            2,
            "cancel",
        ),
        (
            unquoted,
            60,
            "the review printed no verdict JSON",
            2,
            "land",
        ),
        ("sleep 30", 1, "did not finish within 1 seconds", 1, "land"),
    ] {
        let (_dir, repo, db) = fixture();
        let base = git_out(&repo, &["rev-parse", "main"]);
        let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
        let mut reviewer = TestReviewer::new(&[script.to_owned()]);
        reviewer.timeout = Duration::from_secs(timeout);
        let cursor = SqliteQueue::open(&db)
            .unwrap()
            .latest_event_id()
            .unwrap()
            .as_i64();
        let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
        let mut queue = SqliteQueue::open(&db).unwrap();
        let detail = queue.show(TaskId::new(1)).unwrap();
        let run = detail.runs[0].clone();
        assert!(queue.run_leases().unwrap().is_empty());
        assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
        let kinds = event_kinds(&detail);
        assert!(!kinds.contains(&"review_finished"), "{kinds:?}");
        assert_eq!(reviewer.prompts().len(), reviews, "{script}");
        assert_eq!(payloads(&detail, "review_started").len(), reviews);
        let retried = payloads(&detail, "review_retried");
        assert_eq!(retried.len(), reviews - 1, "{kinds:?}");
        if let Some(retried) = retried.first() {
            assert_eq!(retried["attempt"], 1);
            assert!(retried["error"].as_str().unwrap().contains(expected));
        }
        assert!(position(&kinds, "workspace_closed") < position(&kinds, "ask_opened"));
        assert!(position(&kinds, "ask_opened") < position(&kinds, "review_failed"));
        let failed = payloads(&detail, "review_failed");
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0]["attempt"], reviews);
        assert_eq!(failed[0]["status"], "awaiting_integration");
        let error = failed[0]["error"].as_str().unwrap();
        assert!(error.contains(expected), "{error}");
        // The ask carries the failure and the material.
        let asks = queue.asks(Default::default()).unwrap();
        assert_eq!(asks.len(), 1, "{asks:?}");
        let ask = &asks[0];
        assert_eq!(failed[0]["ask_id"], ask.id.as_i64());
        assert_eq!(ask.kind, dagq::domain::AskKind::ApproveLanding);
        assert_eq!(ask.run_id.as_ref(), Some(run.id()));
        assert_eq!(ask.options, ["land", "send_back", "cancel"]);
        assert_eq!(ask.asked_by, "supervisor");
        let run_dir = run.run_dir().unwrap();
        for part in [
            format!("failed and gave no verdict (review {reviews}): "),
            expected.to_owned(),
            format!("Review material: {run_dir}/review.md"),
            format!("{run_dir}/review-{reviews}.out"),
        ] {
            assert!(ask.question.contains(&part), "{part} in {}", ask.question);
        }
        {
            let notifications = backend.notifications.lock().unwrap();
            assert_eq!(notifications.len(), 1, "{notifications:?}");
            assert!(notifications[0].0.ends_with("approve_landing"));
        }
        // The ask is the attention, and the only event that wakes the inbox.
        let status = runtime::status(&db).unwrap();
        assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
        assert!(
            status["attention"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["ask_id"] == json!(ask.id)
                    && a["next"] == format!("answer ask {}", ask.id)),
            "{status}"
        );
        let events = dagq::watch::events(&db, EventId::new(cursor), 100, false).unwrap();
        let events = events["events"].as_array().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["kind"], "ask_opened");
        // The supervisor applies the answer.
        queue.answer(ask.id, answer).unwrap();
        let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        let detail = queue.show(TaskId::new(1)).unwrap();
        assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
        if answer == "land" {
            assert_landed_run(&detail.runs[0], &repo, &base);
            assert_eq!(detail.task.status(), TaskStatus::Completed);
            assert_eq!(
                payloads(&detail, "integration_approved")[0]["ask_id"],
                ask.id.as_i64()
            );
        } else {
            assert_eq!(detail.runs[0].status(), RunStatus::Failed);
            assert_eq!(detail.task.status(), TaskStatus::Canceled);
        }
        // No review after the answer.
        assert_eq!(reviewer.prompts().len(), reviews);
    }
}

/// A review whose stdout holds no readable verdict is reviewed once more
/// with the same input (task 328); a verdict from the retry goes on as
/// usual, here a pass that lands without anyone asked.
#[test]
fn an_unreadable_verdict_is_reviewed_again_and_a_readable_one_lands() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[
        r#"printf '%s\n' '{"verdict":"pass","reasons":[],"summary":"says "fine""}'"#.to_owned(),
        verdict("pass", &[], "meets the acceptance"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed_run(&detail.runs[0], &repo, &base);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 2);
    // The same input, so the same prompt.
    assert_eq!(prompts[0], prompts[1]);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"review_failed"), "{kinds:?}");
    let retried = payloads(&detail, "review_retried");
    assert_eq!(retried.len(), 1);
    assert_eq!(retried[0]["attempt"], 1);
    assert!(
        retried[0]["error"]
            .as_str()
            .unwrap()
            .contains("the review printed no verdict JSON")
    );
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["attempt"], 2);
    assert_eq!(finished[0]["verdict"], "pass");
    assert!(position(&kinds, "review_retried") < position(&kinds, "review_finished"));
    assert!(
        queue
            .asks(AskQuery {
                all: true,
                ..Default::default()
            })
            .unwrap()
            .is_empty()
    );
    assert!(backend.notifications.lock().unwrap().is_empty());
}

/// A revise whose rewritten receipt does not name the clean worktree HEAD
/// (here the commit before the fix) is not handed to validation, which
/// would fail the run and its work: the live session is asked to rewrite
/// it for HEAD, and once it does the run is reviewed again and lands
/// (task 107, description (d)).
#[test]
fn a_revise_receipt_for_another_commit_is_sent_back_to_the_session_until_it_names_head() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let script = "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
        while [ ! -f \"$MESSAGE\" ]; do sleep 0.1; done; rm \"$MESSAGE\"; \
        before=$(git rev-parse HEAD); printf 'fix\\n' >> change.txt; git commit -q -am fix; \
        receipt \"$before\"; idle; \
        while [ ! -f \"$MESSAGE\" ]; do sleep 0.1; done; rm \"$MESSAGE\"; \
        receipt \"$(git rev-parse HEAD)\"; idle; await_exit";
    let backend = TestWorkspace::new(&db, false, script);
    let reviewer = TestReviewer::new(&[
        verdict("revise", &["add a line"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        format!("change by {}\nfix\n", run.id())
    );
    let rejected = payloads(&detail, "revise_receipt_rejected");
    assert_eq!(rejected.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(rejected[0]["attempt"], 1);
    assert!(
        rejected[0]["reason"]
            .as_str()
            .unwrap()
            .contains("but the worktree HEAD is"),
        "{}",
        rejected[0]
    );
    // Validation saw only the receipt for HEAD: nothing failed.
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 2);
    assert!(validated.iter().all(|v| v["accepted"] == true));
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "revise_receipt_rejected") < position(&kinds, "revise_finished"));
    let texts = backend.texts();
    assert_eq!(texts.len(), 2);
    assert!(
        texts[1].1.contains(&format!(
            "dagq: the receipt you rewrote for revise 1 of run {} cannot be accepted",
            run.id()
        )),
        "{}",
        texts[1].1
    );
    assert!(texts[1].1.contains("git rev-parse HEAD"), "{}", texts[1].1);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
}

/// A supervisor died while it waited for the session to exit after a
/// passing review, with the exit timeout recorded and no `stuck_exit` ask
/// made yet. The adopter reviews nothing again and sends no second `/exit`
/// (ADR-0027), asks about the stuck exit once with what follows (the
/// landing), closes that ask once the session exits, and lands the run.
#[test]
fn adopted_run_waiting_for_its_exit_after_a_pass_asks_once_and_lands() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let idle = run.idle_marker_path().unwrap();
    wait_until(&db, Duration::from_secs(20), |_| idle.is_file());
    let head = git_out(
        Path::new(run.worktree_path().unwrap()),
        &["rev-parse", "HEAD"],
    );
    // What the dead supervisor got through: validation, a passing review,
    // the /exit and its timeout.
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), head],
        )
        .unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    for (kind, payload) in [
        (
            "validation_finished",
            json!({"status": "awaiting_integration"}),
        ),
        ("review_started", json!({"attempt": 1})),
        (
            "review_finished",
            json!({"verdict": "pass", "reasons": [], "summary": "ok", "attempt": 1}),
        ),
        (
            "exit_requested",
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
        ),
        (
            "exit_request_timed_out",
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
        ),
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    age_lease(&db, &run, 31);
    let reviewer = Arc::new(TestReviewer::new(&[verdict("concern", &["x"], "never")]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &supervise_options(4, true),
            )
        })
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    thread::sleep(Duration::from_millis(1500));
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::StuckExit);
    assert!(
        ask.question
            .contains("The run stays awaiting_integration under the supervisor after its validation and review, and lands on main once the session exits"),
        "{}",
        ask.question
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    // The person's /exit reaches the session.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert!(reviewer.prompts().is_empty(), "reviewed again");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert_eq!(kinds.iter().filter(|k| **k == "review_started").count(), 1);
    let closed = queue.read_ask(ask.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(
        closed.answer.as_deref(),
        Some("the session exited; closed by the runtime")
    );
    assert_eq!(
        queue
            .asks(AskQuery {
                all: true,
                ..Default::default()
            })
            .unwrap()
            .len(),
        1
    );
}

/// Waits until the run's idle marker shows background work running.
fn wait_for_background(run: &TaskRun) {
    let marker = run.idle_marker_path().unwrap();
    let started = Instant::now();
    while !fs::read_to_string(&marker).is_ok_and(|text| text.contains("\"running\"")) {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "no background marker"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Writes the Stop hook's marker for the run as the session would, with
/// `background_tasks` as given.
fn write_idle_marker(run: &TaskRun, background_tasks: Value) {
    let marker = run.idle_marker_path().unwrap();
    let hook = json!({
        "session_id": run.id(),
        "hook_event_name": "Stop",
        "stop_hook_active": false,
        "background_tasks": background_tasks,
    });
    let tmp = marker.with_extension("tmp");
    fs::write(&tmp, hook.to_string()).unwrap();
    fs::rename(&tmp, &marker).unwrap();
}

/// Lets the supervisor poll a while: what it did not do by then it holds.
const HOLD_PERIOD: Duration = Duration::from_millis(600);

/// The session goes idle after its receipt with background work still
/// running (task 147): the supervisor does not take it for idle, so neither
/// validation nor `/exit` starts, until the work ended and the Stop hook
/// wrote a marker with empty `background_tasks`.
#[test]
fn background_work_holds_the_first_session_until_it_ends() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle_bg; \
         while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; idle_bg_done; await_exit",
    ));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"receipt_observed")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    wait_for_background(&run);
    thread::sleep(HOLD_PERIOD);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Running);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"session_idle_observed"), "{kinds:?}");
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "session_idle_observed") < position(&kinds, "exit_requested"));
    assert!(!kinds.contains(&"exit_request_timed_out"));
}

/// A marker whose background work is over (`completed`) or that names none
/// is idle as before.
#[test]
fn a_marker_without_running_background_work_is_idle() {
    for tasks in [
        json!([]),
        json!([{"id": "b1", "type": "shell", "status": "completed"}]),
    ] {
        let script = format!(
            "commit work; receipt \"$(git rev-parse HEAD)\"; \
             printf '%s' '{}' > \"$IDLE.tmp\"; mv \"$IDLE.tmp\" \"$IDLE\"; await_exit",
            json!({"session_id": "s", "hook_event_name": "Stop", "background_tasks": tasks})
        );
        let (_dir, repo, db) = fixture();
        let backend = TestWorkspace::new(&db, false, &script);
        let outcome = supervise(&db, &repo, &backend).unwrap();
        backend.join();
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
        assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    }
}

/// After the review, the `/exit` waits while the session's marker shows
/// background work running (the session took a turn up again after its
/// receipt), and goes once the work ended.
#[test]
fn background_work_holds_the_exit_after_the_review() {
    let (dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let gate = dir.path().join("review-gate");
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let reviewer = Arc::new(TestReviewer::new(&[format!(
        "while [ ! -f {} ]; do sleep 0.05; done; {}",
        shell_join(&[gate.to_string_lossy().into_owned()]),
        verdict("pass", &[], "meets the acceptance")
    )]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || supervise_reviewed(&db, &repo, &backend, &reviewer))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"review_started")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    write_idle_marker(
        &run,
        json!([{"id": "b1", "type": "shell", "status": "running", "command": "cargo test"}]),
    );
    fs::write(&gate, "").unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"review_finished")
    });
    thread::sleep(HOLD_PERIOD);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);

    write_idle_marker(&run, json!([]));
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_landed(
        &repo,
        &queue.show(TaskId::new(1)).unwrap().runs[0],
        "test task",
        &base,
    );
}

/// A resumed session that rewrote its receipt and stopped with background
/// work running is not asked to exit until the work ended.
#[test]
fn background_work_holds_the_resumed_session_until_it_ends() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let sent_before = backend.exits_sent.load(Ordering::SeqCst);
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle_bg; \
         while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; idle_bg_done; await_exit",
    );
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_for_background(&run);
    thread::sleep(HOLD_PERIOD);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert!(payloads(&detail, "resume_finished").is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), sent_before);

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), sent_before + 1);
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(
        payloads(&detail, "resume_finished")[0]["outcome"],
        "resolved"
    );
}

/// A session revising its work that stops with background work running is
/// not taken for done: the revise waits until the work ended.
#[test]
fn background_work_holds_the_revise_until_it_ends() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
         while [ ! -f \"$MESSAGE\" ]; do sleep 0.05; done; rm \"$MESSAGE\"; \
         printf 'fix\\n' >> change.txt; git commit -q -am fix; \
         receipt \"$(git rev-parse HEAD)\"; idle_bg; \
         while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; idle_bg_done; await_exit",
    ));
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("revise", &["add a line"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || supervise_reviewed(&db, &repo, &backend, &reviewer))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"revise_requested")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    wait_for_background(&run);
    thread::sleep(HOLD_PERIOD);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"revise_finished"), "{kinds:?}");
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
}

/// A revise the session will not finish (it goes idle without rewriting the
/// receipt) ends in `/exit`; a session that holds that `/exit` back past the
/// exit timeout raises one `stuck_exit` ask, closed by the runtime once the
/// session exits, and the run then goes on to its `approve_landing` ask.
#[test]
fn a_revise_session_that_holds_exit_back_raises_a_stuck_exit_ask() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
             while [ ! -f \"$MESSAGE\" ]; do sleep 0.05; done; idle; {HOLD}"
        ),
    );
    backend.exit_timeout = Duration::from_secs(1);
    let backend = Arc::new(backend);
    let reviewer = Arc::new(TestReviewer::new(&[verdict(
        "revise",
        &["add a line"],
        "one gap",
    )]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || supervise_reviewed(&db, &repo, &backend, &reviewer))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    thread::sleep(HOLD_PERIOD);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::StuckExit);
    assert!(
        ask.question
            .contains("opens an approve_landing ask for the person once the session exits"),
        "{}",
        ask.question
    );
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "revise_requested") < position(&kinds, "exit_requested"));
    assert!(!kinds.contains(&"revise_finished"), "{kinds:?}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);

    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let closed = queue.read_ask(ask.id).unwrap();
    assert!(closed.closed_at.is_some());
    let open = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].kind, AskKind::ApproveLanding);
}

/// Background work that never ends does not hold the run forever: past the
/// resume timeout from the receipt the run goes on to validation (its
/// `session_idle_observed` saying the work still ran), and past it again the
/// `/exit` goes, where a dialog would become a `stuck_exit` ask.
#[test]
fn background_work_that_never_ends_is_waited_for_up_to_the_resume_timeout() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle_bg; await_exit",
    );
    backend.resume_timeout = Duration::from_secs(1);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let idle = payloads(&detail, "session_idle_observed");
    assert_eq!(idle.len(), 1);
    assert_eq!(idle[0]["background_running"], true);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "session_idle_observed") < position(&kinds, "exit_requested"));
}

/// A reviewer script that moves main in the main checkout with a change to
/// `change.txt` that conflicts with the run's, then passes the run.
fn moving_main_then_pass() -> String {
    format!(
        "cd \"$(git rev-parse --path-format=absolute --git-common-dir)/..\" && \
         printf 'main moved by %s\\n' $$ > change.txt && git add change.txt && \
         git commit -q -m 'main moves' && {}",
        verdict("pass", &[], "meets the acceptance")
    )
}

/// The worker goes idle after its receipt and never exits by itself; each
/// time a conflict request arrives in its terminal it rebases onto the main
/// the request names, resolves `change.txt`, rewrites the receipt and goes
/// idle again, `requests` times.
fn rebasing_agent(requests: usize) -> String {
    format!(
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; {RESUME_PRELUDE}\n\
         for n in $(seq 1 {requests}); do \
           await_message; rm \"$MESSAGE\"; resolve || exit 1; \
           receipt \"$(git rev-parse HEAD)\"; idle; \
         done; await_exit"
    )
}

/// A passed run whose head conflicts with the main that moved during its
/// review is not asked to exit (ADR-0027 decision 4): `git merge-tree` finds
/// the conflict without touching the worktree, `conflict_precheck` is
/// recorded, and the live session gets the resume's resolution request. It
/// rebases and rewrites its receipt; the run is validated and reviewed
/// again, the second precheck finds no conflict, and the run lands without
/// a `needs_session` or a resume.
#[test]
fn a_passed_run_that_conflicts_with_main_is_rebased_by_its_live_session_and_lands() {
    let (_dir, repo, db) = fixture();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &rebasing_agent(1));
    let reviewer = TestReviewer::new(&[
        moving_main_then_pass(),
        verdict("pass", &[], "still meets it"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let run = detail.runs[0].clone();
    let moved = git_out(&repo, &["rev-parse", "main~1"]);
    assert_eq!(git_out(&repo, &["rev-parse", "main~2"]), seed);
    assert_landed(&repo, &run, "test task", &moved);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "resolved by the resumed session\n"
    );
    let validated: Vec<&Value> = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 2);
    let source = validated[0]["receipt"]["commit"].as_str().unwrap();
    let prechecks = payloads(&detail, "conflict_precheck");
    assert_eq!(prechecks.len(), 1);
    let precheck = prechecks[0];
    assert_eq!(precheck["main"], json!(moved));
    // The head that passed, untouched by the precheck.
    assert_eq!(precheck["head"], json!(source));
    assert_eq!(precheck["merge_base"], json!(seed));
    assert_eq!(precheck["conflicts"], json!(["change.txt"]));
    assert_eq!(precheck["attempt"], 1);
    assert_eq!(precheck["requested"], true);
    assert!(precheck["sent_at"].is_i64());
    let resolved = payloads(&detail, "conflict_resolved");
    let head = git_out(
        &repo,
        &["rev-parse", &format!("refs/dagq/runs/{}", run.id())],
    );
    assert_eq!(resolved, [&json!({"attempt": 1, "head": head})]);
    assert_eq!(
        git_out(&repo, &["rev-parse", &format!("{head}~1")]),
        moved,
        "the session rebased onto the main the request named"
    );
    let verdicts: Vec<&Value> = payloads(&detail, "review_finished")
        .iter()
        .map(|p| &p["verdict"])
        .collect();
    assert_eq!(verdicts, [&json!("pass"), &json!("pass")]);
    let kinds = event_kinds(&detail);
    for (earlier, later) in [
        ("review_finished", "conflict_precheck"),
        ("conflict_precheck", "conflict_resolved"),
        ("conflict_resolved", "exit_requested"),
        ("exit_requested", "workspace_closed"),
        ("workspace_closed", "integration_started"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    for absent in ["resume_started", "integration_deferred", "revise_requested"] {
        assert!(!kinds.contains(&absent), "{absent} in {kinds:?}");
    }
    assert_eq!(payloads(&detail, "integration_started").len(), 1);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
    // The request is the resume's, for the live session.
    let texts = backend.texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0].0, WORKSPACE_ID);
    let text = &texts[0].1;
    for expected in [
        format!(
            "dagq: the supervisor's review of run {} (task 1) passed, but integrate would conflict with main, so the run was not landed.",
            run.id()
        ),
        format!(
            "Reason: git merge-tree finds that main {moved} conflicts with the run in change.txt"
        ),
        format!("main is now {moved} (your base commit was {seed})."),
        "Tasks landed on main since your base: none.".to_owned(),
        format!("1. In this worktree run git rebase {moved} and resolve the conflicts."),
        "[\"test -f seed.txt\"]".to_owned(),
        "3. Keep the worktree clean.".to_owned(),
        runtime::STOP_BACKGROUND.to_owned(),
        format!(
            "Rewrite the receipt at {} with the new head commit",
            run.receipt_path().unwrap()
        ),
        "Do not merge or push. When done, report briefly and stop; do not run /exit.".to_owned(),
    ] {
        assert!(text.contains(&expected), "{expected:?} not in {text}");
    }
    let run_dir = Path::new(run.run_dir().unwrap());
    assert_eq!(
        &fs::read_to_string(run_dir.join("conflict-1.txt")).unwrap(),
        text
    );
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(run_attention_of(&runtime::status(&db).unwrap(), run.id()).is_none());
}

/// A passed run that merges cleanly with main is not sent anything: no
/// `conflict_precheck`, one `/exit`, and the landing, as before.
#[test]
fn a_passed_run_that_merges_cleanly_with_main_lands_without_a_request() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    // Main moves during the review, but in another file.
    let reviewer = TestReviewer::new(&[format!(
        "cd \"$(git rev-parse --path-format=absolute --git-common-dir)/..\" && \
         printf 'other\\n' > other.txt && git add other.txt && \
         git commit -q -m 'main moves elsewhere' && {}",
        verdict("pass", &[], "meets the acceptance")
    )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let moved = git_out(&repo, &["rev-parse", "main~1"]);
    assert_landed(&repo, &detail.runs[0], "test task", &moved);
    assert!(repo.join("other.txt").is_file());
    assert!(payloads(&detail, "conflict_precheck").is_empty());
    assert!(backend.texts().is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(reviewer.prompts().len(), 1);
}

/// Conflict requests and resumes share the run's `MAX_RESUME_ATTEMPTS`:
/// when main keeps moving into the run, the precheck after the third
/// request exits the session and opens an `approve_landing` ask instead of
/// a fourth request.
#[test]
fn conflict_requests_past_the_limit_ask_a_person() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, &rebasing_agent(3));
    let reviewer = TestReviewer::new(&[moving_main_then_pass()]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    let prechecks = payloads(&detail, "conflict_precheck");
    let requested: Vec<&Value> = prechecks.iter().map(|p| &p["requested"]).collect();
    assert_eq!(
        requested,
        [&json!(true), &json!(true), &json!(true), &json!(false)]
    );
    assert_eq!(prechecks[3]["attempt"], 4);
    assert!(prechecks[3].get("sent_at").is_none());
    assert!(
        prechecks[3]["asked"]
            .as_str()
            .unwrap()
            .ends_with("after 3 conflict requests and 0 resumes")
    );
    assert_eq!(payloads(&detail, "conflict_resolved").len(), 3);
    assert_eq!(payloads(&detail, "review_finished").len(), 4);
    assert_eq!(backend.texts().len(), 3);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
    assert!(queue.run_leases().unwrap().is_empty());
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "workspace_closed") < position(&kinds, "ask_opened"));
    assert!(!kinds.contains(&"integration_started"), "{kinds:?}");
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    let ask = &asks[0];
    assert_eq!(ask.kind, dagq::domain::AskKind::ApproveLanding);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert!(
        ask.question.contains("returned pass (git merge-tree finds that main")
            && ask
                .question
                .contains("conflicts with the run in change.txt, after 3 conflict requests and 0 resumes): meets the acceptance"),
        "{}",
        ask.question
    );
}

/// A triage script that prints the verdict JSON (no apostrophes in the
/// texts: the script quotes the JSON with them).
fn triage(decision: &str, reason: &str, instruction: &str) -> String {
    let json = json!({"verdict": decision, "reason": reason, "instruction": instruction});
    format!("printf '%s\\n' '{json}'")
}

/// A failed run is triaged by the supervisor (ADR-0024 decision 3): `retry`
/// makes the task `ready`, closes the run's workspace and the next pass
/// runs the task again. A second failure is not retried whatever the
/// triage answers: it becomes a `decide` ask for the inbox.
#[test]
fn a_failed_run_triaged_retry_runs_again_and_a_second_failure_is_asked() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; exit 7",
    );
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "unused")]).with_triages(&[
        triage("retry", "the session died on its own", ""),
        triage("retry", "it died again", ""),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["triaged"].as_array().unwrap().len(), 2, "{outcome}");
    assert!(
        reviewer.prompts().is_empty(),
        "a failed run is not reviewed"
    );
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(detail.runs.len(), 2);
    let (first, second) = (&detail.runs[0], &detail.runs[1]);
    assert_eq!(first.status(), RunStatus::Failed);
    assert_eq!(second.status(), RunStatus::Failed);
    assert!(queue.run_leases().unwrap().is_empty());

    // The first triage: retry readied the task, then the workspace closed.
    let first_events = queue.run_events(first.id()).unwrap();
    let kinds: Vec<&str> = first_events.iter().map(|e| e.kind.as_str()).collect();
    let started = position(&kinds, "triage_started");
    assert_eq!(first_events[started].payload["status"], "failed");
    assert_eq!(first_events[started].payload["attempt"], 1);
    let finished = &first_events[position(&kinds, "triage_finished")].payload;
    assert_eq!(finished["verdict"], "retry");
    assert_eq!(finished["action"], "retry");
    assert_eq!(finished["status"], "failed");
    assert_eq!(finished["failures"], 1);
    assert_eq!(finished["reason"], "the session died on its own");
    assert!(finished["overridden"].is_null());
    let closed = &first_events[position(&kinds, "workspace_closed")].payload;
    assert_eq!(closed["workspace_id"], WORKSPACE_ID);
    assert_eq!(closed["by"], "triage");
    assert!(position(&kinds, "triage_finished") < position(&kinds, "workspace_closed"));
    assert!(first.workspace_closed_at().is_some());
    assert!(backend.closed().contains(&WORKSPACE_ID.to_owned()));
    let readied = detail.events.iter().position(|e| {
        e.kind == "task_status_changed"
            && e.payload["from"] == "in_progress"
            && e.payload["to"] == "ready"
    });
    let second_claimed = detail
        .events
        .iter()
        .position(|e| e.run_id.as_ref().map(RunId::as_str) == Some(second.id().as_str()))
        .unwrap();
    assert!(readied.unwrap() < second_claimed);

    // The second triage answered retry too, but the task failed twice.
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 2);
    assert_eq!(finished[1]["verdict"], "retry");
    assert_eq!(finished[1]["action"], "ask");
    assert_eq!(finished[1]["failures"], 2);
    assert!(
        finished[1]["overridden"]
            .as_str()
            .unwrap()
            .contains("has 2 failed or interrupted runs"),
        "{}",
        finished[1]
    );
    let ask = queue
        .read_ask(AskId::new(finished[1]["ask_id"].as_i64().unwrap()))
        .unwrap();
    assert_eq!(ask.kind, AskKind::Decide);
    assert_eq!(ask.run_id.as_ref(), Some(second.id()));
    assert_eq!(ask.asked_by, "supervisor");
    assert_eq!(ask.options, ["retry", "resume", "cancel"]);
    assert!(ask.is_open());
    assert!(
        ask.question.contains("is not retried without a person"),
        "{}",
        ask.question
    );
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);
    assert!(backend.closed().contains(&workspace_id(1)));

    // The prompts carry the task, the error and the retry rule, and the
    // second one the first run with its triage.
    let prompts = reviewer.triage_prompts();
    assert_eq!(prompts.len(), 2);
    let (prompt, dir) = &prompts[0];
    assert_eq!(dir, Path::new(first.run_dir().unwrap()));
    for expected in [
        "of dagq task 1 (test task), which ended failed",
        "Acceptance criteria:\nworks",
        "Last error of the run:\nsession exited with code 7",
        "\"result\":\"succeeded\"",
        "Final screen of the session",
        "from 2 on, retry is not allowed",
        "Earlier runs of the task:\nnone",
        "{\"verdict\": \"retry\" | \"resume\" | \"ask\"",
    ] {
        assert!(prompt.contains(expected), "{expected:?} in {prompt}");
    }
    assert!(dir.join("triage-prompt-1.txt").is_file());
    assert!(dir.join("triage-1.out").is_file());
    let (prompt, _) = &prompts[1];
    assert!(
        prompt.contains(
            "This task has 2 failed or interrupted runs, this one included: do not answer retry"
        ),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!(
            "- run {} failed: session exited with code 7 (triaged: \"retry\")",
            first.id()
        )),
        "{prompt}"
    );

    // Neither run is an attention: the ask is.
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, second.id()).is_none(), "{status}");
    assert_eq!(
        ask_attention(&status, ask.id)[0]["next"],
        format!("answer ask {}", ask.id)
    );
}

/// `resume`: the run becomes `needs_session` with the triage's instruction,
/// its workspace is closed, and the supervisor resumes its session with a
/// request naming the instruction; the resumed run is validated, reviewed
/// and landed like any other.
#[test]
fn a_failed_run_triaged_resume_is_resumed_in_its_session_and_lands() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, "commit work; exit 7");
    backend.resume_script_for(
        1,
        "await_message; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let reviewer =
        TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]).with_triages(&[triage(
            "resume",
            "the work is committed but the receipt is missing",
            "write the receipt for your commit",
        )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1);
    assert_landed_run(&detail.runs[0], &repo, &base);
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["action"], "resume");
    assert_eq!(finished[0]["status"], "needs_session");
    assert_eq!(
        finished[0]["instruction"],
        "write the receipt for your commit"
    );
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "triage_finished") < position(&kinds, "workspace_closed"));
    assert!(position(&kinds, "workspace_closed") < position(&kinds, "resume_started"));
    assert_eq!(
        payloads(&detail, "resume_started")[0]["reason"],
        "write the receipt for your commit"
    );
    assert_eq!(backend.closed()[0], WORKSPACE_ID);
    let text = &backend.texts()[0].1;
    for expected in [
        "the supervisor's triage sent it back to this session to finish",
        "Reason: write the receipt for your commit",
        "1. Do what the reason asks in this worktree and commit",
    ] {
        assert!(text.contains(expected), "{expected:?} in {text}");
    }
}

/// `ask`: a `decide` ask for the inbox, the run stays `failed`; a cmux
/// failure closing the workspace is recorded and the triage goes on. The
/// supervisor applies the answer (`cancel` here) and closes the ask.
#[test]
fn a_triage_ask_waits_for_a_person_and_the_supervisor_applies_the_answer() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, "commit work; exit 7");
    backend.close_fail = true;
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "unused")]).with_triages(&[triage(
        "ask",
        "the acceptance cannot be met",
        "Is task 1 still wanted?",
    )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Failed);
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished[0]["action"], "ask");
    assert_eq!(finished[0]["status"], "failed");
    let ask = queue
        .read_ask(AskId::new(finished[0]["ask_id"].as_i64().unwrap()))
        .unwrap();
    assert!(
        ask.question
            .contains("asks a person: Is task 1 still wanted?"),
        "{}",
        ask.question
    );
    assert!(
        ask.question
            .contains("Reason: the acceptance cannot be met")
    );
    assert!(
        ask.question
            .contains("Last error: session exited with code 7")
    );
    assert!(ask.question.contains("triage-prompt-1.txt"));
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);
    // The close failed: recorded, the workspace kept, the ask still made.
    let cleanup = payloads(&detail, "cleanup_failed");
    assert_eq!(cleanup.len(), 1);
    assert_eq!(cleanup[0]["workspace_id"], WORKSPACE_ID);
    assert!(
        cleanup[0]["message"]
            .as_str()
            .unwrap()
            .contains("injected workspace close failure")
    );
    assert!(run.workspace_closed_at().is_none());
    assert!(!event_kinds(&detail).contains(&"workspace_closed"));
    assert_eq!(run.last_error(), Some("session exited with code 7"));
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");

    // An answer the supervisor applies is its own, not a person's.
    queue.answer(ask.id, "cancel").unwrap();
    let answered = queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .rfind(|e| e.kind == "ask_answered")
        .unwrap();
    assert_eq!(answered.payload["runtime_delivers"], true);
    assert_eq!(
        ask_attention(&runtime::status(&db).unwrap(), ask.id)[0]["next"],
        format!("applying the answer of ask {} (runtime)", ask.id)
    );
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    let decided = payloads(&detail, "triage_decided");
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0]["answer"], "cancel");
    assert_eq!(decided[0]["ask_id"], ask.id.as_i64());
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert!(ask_attention(&runtime::status(&db).unwrap(), ask.id).is_empty());
}

/// The answers `resume` and `retry` of a triage's ask, applied by the
/// queue: `resume` parks the run for a session with the reason, `retry`
/// readies the task; an answer outside the options, a leased run, a run
/// that is not failed and an ask already closed are refused.
#[test]
fn triage_answers_resume_the_run_or_ready_the_task() {
    let (_dir, db, detail) = run_agent("commit work; exit 7");
    let run = detail.runs[0].clone();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = |queue: &mut SqliteQueue| {
        queue
            .ask(NewAsk {
                kind: AskKind::Decide,
                task_id: None,
                run_id: Some(run.id().clone()),
                question: "what now?".into(),
                options: vec!["retry".into(), "resume".into(), "cancel".into()],
                asked_by: "supervisor".into(),
                reason_category: dagq::domain::AskReason::RecoveryFailed,
                finding_id: None,
            })
            .unwrap()
            .ask
    };
    let first = ask(&mut queue);
    queue.answer(first.id, "resume").unwrap();
    assert_eq!(queue.triage_answers().unwrap()[0].id, first.id);
    assert!(
        queue
            .decide_triage(run.id(), first.id, "land", "x")
            .is_err()
    );
    let parked = queue
        .decide_triage(run.id(), first.id, "resume", "fix the test")
        .unwrap();
    assert_eq!(parked.status(), RunStatus::NeedsSession);
    assert_eq!(parked.last_error(), Some("fix the test"));
    assert!(queue.read_ask(first.id).unwrap().closed_at.is_some());
    assert!(queue.triage_answers().unwrap().is_empty());
    assert!(
        queue
            .decide_triage(run.id(), first.id, "retry", "x")
            .unwrap_err()
            .to_string()
            .contains("not failed or interrupted")
    );

    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='failed' WHERE id=?1",
            [&run.id()],
        )
        .unwrap();
    // An ask already applied (closed) is not applied twice, by another
    // supervisor or later.
    assert!(
        queue
            .decide_triage(run.id(), first.id, "retry", "x")
            .unwrap_err()
            .to_string()
            .contains("not an answered, unclosed ask")
    );
    let second = ask(&mut queue);
    queue.answer(second.id, "retry").unwrap();
    Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO run_leases(run_id,token,pid) VALUES (?1,'other',?2)",
            rusqlite::params![run.id(), std::process::id()],
        )
        .unwrap();
    assert!(
        queue
            .decide_triage(run.id(), second.id, "retry", "x")
            .unwrap_err()
            .to_string()
            .contains("is leased")
    );
    // A stale lease (its triage's supervisor stalled) does not block the
    // answer, and it goes with it: woken up, that supervisor cannot renew
    // it and write after the decision (ADR-0039 decision 7).
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE run_leases SET heartbeat_at=0", [])
        .unwrap();
    queue
        .decide_triage(run.id(), second.id, "retry", "x")
        .unwrap();
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Ready
    );
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert!(
        queue
            .finish_triage(
                run.id(),
                "other",
                &dagq::application::TriageAction::Retry,
                json!({}),
            )
            .unwrap_err()
            .to_string()
            .contains("run lease is missing")
    );
}

/// A run whose wrapper died and that nobody leases is recovered by the
/// supervisor (ADR-0024 decision 3, amending ADR-0012) and triaged as
/// `interrupted`; `retry` runs the task again, and it lands.
#[test]
fn a_dead_run_nobody_leases_is_recovered_triaged_and_retried() {
    let (_dir, repo, db) = fixture();
    let orphan = orphan_run(&repo, &db, "owner", dead_pid(), dead_pid());
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases", [])
        .unwrap();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")])
        .with_triages(&[triage("retry", "the machine restarted", "")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2);
    assert_eq!(detail.runs[0].id(), orphan.id());
    assert_eq!(detail.runs[0].status(), RunStatus::Interrupted);
    assert_landed_run(&detail.runs[1], &repo, &base);
    let events = queue.run_events(orphan.id()).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    let recovered = &events[position(&kinds, "run_recovered")].payload;
    assert_eq!(recovered["by"], "supervisor");
    assert_eq!(recovered["previous_status"], "running");
    assert_eq!(recovered["lease_deleted"], false);
    assert_eq!(
        events[position(&kinds, "triage_started")].payload["status"],
        "interrupted"
    );
    assert_eq!(
        events[position(&kinds, "triage_finished")].payload["action"],
        "retry"
    );
    // Its workspace is one cmux does not list: nothing to close.
    assert!(!kinds.contains(&"workspace_closed"));
    assert!(!kinds.contains(&"cleanup_failed"));
    let (prompt, _) = &reviewer.triage_prompts()[0];
    assert!(prompt.contains("which ended interrupted"), "{prompt}");
}

/// A run whose supervisor and wrapper both died keeps the dead supervisor's
/// stale lease. Nobody adopts it (its wrapper is dead), so the next
/// supervisor recovers it itself as it does a run nobody leases (task 236),
/// triages it as `interrupted`, and `retry` runs the task again to landing.
#[test]
fn a_dead_run_whose_dead_supervisor_still_leases_it_is_recovered_and_retried() {
    let (_dir, repo, db) = fixture();
    let orphan = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET pid=?1, heartbeat_at=unixepoch()-31",
            [dead_pid()],
        )
        .unwrap();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")])
        .with_triages(&[triage("retry", "the machine restarted", "")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2);
    assert_eq!(detail.runs[0].id(), orphan.id());
    assert_eq!(detail.runs[0].status(), RunStatus::Interrupted);
    assert_landed_run(&detail.runs[1], &repo, &base);
    let events = queue.run_events(orphan.id()).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(!kinds.contains(&"run_adopted"), "{kinds:?}");
    let recovered = &events[position(&kinds, "run_recovered")].payload;
    assert_eq!(recovered["by"], "supervisor");
    assert_eq!(recovered["previous_status"], "running");
    assert_eq!(recovered["lease_deleted"], true);
    assert_eq!(recovered["run"]["lease"]["alive"], false);
    assert_eq!(
        events[position(&kinds, "triage_finished")].payload["action"],
        "retry"
    );
}

/// Commit a change in the worktree of `run` (a `running` orphan), write its
/// receipt and move it to `awaiting_integration` the way validation does,
/// leaving its lease and wrapper registration as they are: a run whose
/// supervisor died during its review.
fn validated_orphan(db: &Path, run: &TaskRun) -> String {
    let worktree = Path::new(run.worktree_path().unwrap());
    fs::write(
        worktree.join("change.txt"),
        format!("change by {}\n", run.id()),
    )
    .unwrap();
    git(worktree, &["add", "change.txt"]);
    git(worktree, &["commit", "-q", "-m", "work"]);
    let head = git_out(worktree, &["rev-parse", "HEAD"]);
    write_receipt(run, &head, "succeeded", "done");
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), head],
        )
        .unwrap();
    SqliteQueue::open(db)
        .unwrap()
        .record_runtime_event(
            run.id(),
            "validation_finished",
            json!({"status": "awaiting_integration"}),
        )
        .unwrap();
    head
}

/// Make the wrapper of `run` one that died without recording its exit (a
/// dead pid and a heartbeat past the TTL) and the lease one a dead
/// supervisor left behind.
fn kill_supervisor_and_wrapper(db: &Path, run: &TaskRun) {
    let raw = Connection::open(db).unwrap();
    raw.execute(
        "UPDATE run_processes SET pid=?2, heartbeat_at=unixepoch()-31 WHERE run_id=?1",
        rusqlite::params![run.id(), dead_pid()],
    )
    .unwrap();
    raw.execute(
        "UPDATE run_leases SET pid=?2, heartbeat_at=unixepoch()-31 WHERE run_id=?1",
        rusqlite::params![run.id(), dead_pid()],
    )
    .unwrap();
}

/// The supervisor and the session's wrapper both died while an
/// `awaiting_integration` run was under review (task 236): the next
/// supervisor adopts it all the same, since its review needs no session,
/// reviews it with the session taken for ended, sends no `/exit` and lands
/// it, with nobody touching the queue.
#[test]
fn an_awaiting_run_whose_supervisor_and_wrapper_died_is_adopted_reviewed_and_landed() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let run = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    validated_orphan(&db, &run);
    kill_supervisor_and_wrapper(&db, &run);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1);
    assert_landed_run(&detail.runs[0], &repo, &base);
    let adopted = adoption_events(&detail);
    assert_eq!(adopted.len(), 1, "{adopted:?}");
    assert_eq!(adopted[0]["previous_token"], "dead-supervisor");
    assert_eq!(adopted[0]["wrapper"]["alive"], false);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"run_recovered"), "{kinds:?}");
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    assert_eq!(
        payloads(&detail, "review_started")[0]["session_live"],
        false
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    assert!(queue.run_leases().unwrap().is_empty());
}

/// A `revise` verdict for such a run has no session to revise it: the run
/// is asked about (`approve_landing`) instead of being given up.
#[test]
fn a_revise_for_an_adopted_run_whose_wrapper_died_asks_a_person() {
    let (_dir, repo, db) = fixture();
    let run = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    validated_orphan(&db, &run);
    kill_supervisor_and_wrapper(&db, &run);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("revise", &["add a test"], "one gap")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::ApproveLanding);
    assert!(
        asks[0]
            .question
            .contains("the session had ended, so nobody could revise the run"),
        "{}",
        asks[0].question
    );
    assert!(backend.texts().is_empty());
    assert!(queue.run_leases().unwrap().is_empty());
}

/// Without a supervisor, `recover` takes the stale lease of an
/// `awaiting_integration` run whose supervisor and wrapper died (task 236):
/// the run stays `awaiting_integration` and `integrate` lands it. A run
/// awaiting integration that nobody leases has nothing to recover, and a
/// live supervisor's lease is refused as before.
#[test]
fn recover_releases_the_stale_lease_of_an_awaiting_run_for_integrate() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let run = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    validated_orphan(&db, &run);
    let refused = runtime::recover(&db, run.id()).unwrap_err().to_string();
    assert!(refused.contains("lease heartbeat is"), "{refused}");
    kill_supervisor_and_wrapper(&db, &run);
    let error = integrate(&db, 1, &repo).unwrap_err().to_string();
    assert!(error.contains("run is still leased"), "{error}");
    let recovered = runtime::recover(&db, run.id()).unwrap();
    assert_eq!(recovered["run"]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let recovered = payloads(&queue.show(TaskId::new(1)).unwrap(), "run_recovered")[0].clone();
    assert_eq!(recovered["previous_status"], "awaiting_integration");
    assert_eq!(recovered["status"], "awaiting_integration");
    assert_eq!(recovered["lease_deleted"], true);
    let again = runtime::recover(&db, run.id()).unwrap_err().to_string();
    assert!(
        again.contains("only unfinished runs, or a run awaiting integration that is still leased"),
        "{again}"
    );
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed_run(&detail.runs[0], &repo, &base);
}

/// Make the live wrapper of `run_id` go silent the way a wrapper whose
/// heartbeat stopped does while its process lives on: its row names another
/// live process (`stand_in`, so the in-test wrapper's heartbeats no longer
/// match and fail), and once any heartbeat in flight has landed its
/// heartbeat is made older than the timeout. Returns the wrapper's own pid.
fn silence_wrapper(db: &Path, run_id: &RunId, stand_in: u32) -> u32 {
    let raw = Connection::open(db).unwrap();
    let pid: u32 = raw
        .query_row(
            "SELECT pid FROM run_processes WHERE run_id=?1 AND role='wrapper' AND exited_at IS NULL",
            [run_id],
            |r| r.get(0),
        )
        .unwrap();
    raw.execute(
        "UPDATE run_processes SET pid=?2 WHERE run_id=?1 AND role='wrapper' AND exited_at IS NULL",
        rusqlite::params![run_id, stand_in],
    )
    .unwrap();
    thread::sleep(TEST_TICK * 4);
    raw.execute(
        "UPDATE run_processes SET heartbeat_at=unixepoch()-31 WHERE run_id=?1 AND role='wrapper' AND exited_at IS NULL",
        [run_id],
    )
    .unwrap();
    pid
}

/// Give the silenced wrapper its own pid back, so it heartbeats and records
/// its exit again.
fn revive_wrapper(db: &Path, run_id: &RunId, pid: u32) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE run_processes SET pid=?2 WHERE run_id=?1 AND role='wrapper' AND exited_at IS NULL",
            rusqlite::params![run_id, pid],
        )
        .unwrap();
}

/// A live process to stand in for a silent wrapper; killed on drop.
struct StandIn(std::process::Child);
impl StandIn {
    fn new() -> Self {
        Self(
            Command::new("sleep")
                .arg("600")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
    fn pid(&self) -> u32 {
        self.0.id()
    }
}
impl Drop for StandIn {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The worker's wrapper stops heartbeating while its process lives on (task
/// 170): the supervisor records `wrapper_heartbeat_expired`, sends the
/// session the single `/exit` a finished one gets, and when it does not
/// exit within the exit timeout opens a `stuck_exit` ask to the inbox that
/// says why, keeping the run and its lease. Once the session exits the ask
/// is closed and the run goes on to validating as usual.
#[test]
fn a_silent_wrapper_with_a_live_session_is_asked_to_exit_then_raised_to_the_inbox() {
    let (_dir, repo, db) = fixture();
    // A receipt but no idle marker: nothing else would ask it to exit.
    let mut backend = TestWorkspace::new(
        &db,
        false,
        &format!("commit work; receipt \"$(git rev-parse HEAD)\"; await_exit; {HOLD}"),
    );
    backend.exit_timeout = Duration::from_secs(1);
    let backend = Arc::new(backend);
    SqliteQueue::open(&db)
        .unwrap()
        .register_session_workspace(SessionRole::Inbox, "inbox-ws")
        .unwrap();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"receipt_observed")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let stand_in = StandIn::new();
    let own = silence_wrapper(&db, run.id(), stand_in.pid());
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Running);
    assert!(run.last_error().is_none());
    assert!(queue.run_lease(run.id()).unwrap().is_some());
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("wrapper_heartbeat_expired") < position("exit_requested"));
    assert!(position("exit_requested") < position("exit_request_timed_out"));
    let expired = payloads(&detail, "wrapper_heartbeat_expired");
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0]["pid"], stand_in.pid());
    assert_eq!(expired[0]["workspace_id"], WORKSPACE_ID);
    assert!(expired[0]["heartbeat_age_secs"].as_i64().unwrap() > 30);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::StuckExit);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.options, ["exit", "wait"]);
    assert!(
        ask.question.contains(
            "Its wrapper stopped heartbeating while its process lived on (wrapper_heartbeat_expired), so the supervisor sent the /exit. The run stays running, and goes on to validating once the session exits"
        ),
        "{}",
        ask.question
    );
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);

    revive_wrapper(&db, run.id(), own);
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("exit_request_timed_out") < position("session_exited"));
    assert!(position("session_exited") < position("validation_finished"));
    assert_eq!(payloads(&detail, "wrapper_heartbeat_expired").len(), 1);
    let closed = queue.read_ask(ask.id).unwrap();
    assert_eq!(
        closed.answer.as_deref(),
        Some("the session exited; closed by the runtime")
    );
}

/// A wrapper whose heartbeat expired and whose process is gone is handled
/// as before: nothing is asked to exit, and the run is given up with the
/// heartbeat error.
#[test]
fn a_silent_wrapper_whose_process_is_gone_is_given_up_as_before() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        &format!("commit work; receipt \"$(git rev-parse HEAD)\"; {HOLD}"),
    ));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"receipt_observed")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let own = silence_wrapper(&db, run.id(), dead_pid());
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"runtime_error")
    });
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"wrapper_heartbeat_expired"), "{kinds:?}");
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    assert_eq!(
        payloads(&detail, "runtime_error")[0]["message"],
        "wrapper heartbeat expired; session may still be alive"
    );
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());
    // Let the in-test wrapper finish so the supervisor's pass can end.
    revive_wrapper(&db, run.id(), own);
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert!(
        outcome["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["message"] == "wrapper heartbeat expired; session may still be alive"),
        "{outcome}"
    );
}

/// A resumed session whose wrapper goes silent is sent the `/exit` too, and
/// is let go with a `stuck_exit` ask when it does not exit, as a resumed
/// session that ignores `/exit` is.
#[test]
fn a_resumed_session_with_a_silent_wrapper_is_asked_to_exit_then_let_go() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.exit_timeout = Duration::from_secs(1);
    let backend = Arc::new(backend);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(2, &format!("await_message; {HOLD}"));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |_| {
        resume_message_path(run.run_dir().unwrap()).exists()
    });
    let stand_in = StandIn::new();
    let own = silence_wrapper(&db, run.id(), stand_in.pid());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert_eq!(payloads(&detail, "wrapper_heartbeat_expired").len(), 1);
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1, "{kinds:?}");
    assert_eq!(finished[0]["outcome"], "unresolved");
    assert_eq!(finished[0]["exit_timed_out"], true);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let asks = other_asks(&mut queue, false);
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::StuckExit);
    assert!(
        asks[0].question.contains(
            "(wrapper_heartbeat_expired), so the supervisor sent the /exit. The run stays needs_session, and the supervisor resumes it again once the session exits"
        ),
        "{}",
        asks[0].question
    );
    revive_wrapper(&db, run.id(), own);
    release_held_session(run.run_dir().unwrap());
    backend.join();
}

/// A session kept open through its review whose wrapper goes silent while
/// the supervisor waits for its `/exit` is not given up either: the silence
/// is recorded, the `stuck_exit` ask does not blame the silence for an
/// `/exit` sent before it, and the run moves on as its verdict said once the
/// session exits.
#[test]
fn a_silent_wrapper_after_the_review_waits_for_the_exit_with_an_ask() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, HELD_AGENT);
    backend.exit_timeout = Duration::from_secs(3);
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"exit_requested")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let stand_in = StandIn::new();
    let own = silence_wrapper(&db, run.id(), stand_in.pid());
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert_eq!(payloads(&detail, "wrapper_heartbeat_expired").len(), 1);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let ask = queue.asks(AskQuery::default()).unwrap()[0].clone();
    assert!(
        ask.question
            .contains("exit back. The run stays awaiting_integration under the supervisor"),
        "{}",
        ask.question
    );
    assert!(
        !ask.question.contains("wrapper_heartbeat_expired"),
        "{}",
        ask.question
    );
    revive_wrapper(&db, run.id(), own);
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(kinds.contains(&"review_failed"), "{kinds:?}");
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
}

/// A silent wrapper that dies without recording its exit after the `/exit`
/// leaves no session to exit: the run is given up with the heartbeat error
/// as before, and the `stuck_exit` ask the silence raised is closed.
#[test]
fn a_silent_wrapper_that_dies_after_the_exit_closes_its_ask() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        &format!("commit work; receipt \"$(git rev-parse HEAD)\"; await_exit; {HOLD}"),
    );
    backend.exit_timeout = Duration::from_secs(1);
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"receipt_observed")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let stand_in = StandIn::new();
    let own = silence_wrapper(&db, run.id(), stand_in.pid());
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    let ask = queue.asks(AskQuery::default()).unwrap()[0].clone();
    drop(stand_in);
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"runtime_error")
    });
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(
        payloads(&detail, "runtime_error")[0]["message"],
        "wrapper heartbeat expired; session may still be alive"
    );
    let closed = queue.read_ask(ask.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(
        closed.answer.as_deref(),
        Some("the session exited; closed by the runtime")
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    revive_wrapper(&db, run.id(), own);
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert!(
        outcome["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["message"] == "wrapper heartbeat expired; session may still be alive"),
        "{outcome}"
    );
}

#[test]
fn a_refused_run_transition_keeps_the_domain_reason_beside_the_old_error() {
    use dagq::{domain::ClaimOutcome, infrastructure::runtime_store::REFUSALS_LOG};
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let base = "0123456789abcdef0123456789abcdef01234567";
    let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&sha(base), "owner").unwrap()
    else {
        panic!()
    };
    let run_dir = dagq::infrastructure::location::runs_dir(&db.canonicalize().unwrap())
        .join(run.id().as_str());
    fs::create_dir_all(&run_dir).unwrap();
    // A claimed run is not awaiting integration: the error keeps the
    // store's message, and the domain's reason goes to the run's log.
    let error = queue.restart_validation(run.id(), "owner").unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "run is not awaiting integration under this supervisor"
    );
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Claimed);
    let log = fs::read_to_string(run_dir.join(REFUSALS_LOG)).unwrap();
    let line = log.lines().next().unwrap();
    assert!(line.starts_with('['), "{line}");
    assert!(
        line.ends_with(
            "] run is not awaiting integration under this supervisor: \
             cannot validate again a run in claimed state"
        ),
        "{line}"
    );
    // A refusal whose run directory is gone still fails the same way.
    fs::remove_dir_all(&run_dir).unwrap();
    let error = queue.restart_validation(run.id(), "owner").unwrap_err();
    assert_eq!(
        error.to_string(),
        "run is not awaiting integration under this supervisor"
    );
    assert!(!run_dir.exists());
}

/// `integrate` reads its time and its token from the generators `main`
/// hands it, not from the queue's own clock and UUIDs: a lease heartbeat
/// fresh to the injected clock holds the run, a stale one lets it through,
/// and only the injected token takes over a lease stored under it.
#[test]
fn integrate_takes_its_time_and_token_from_the_injected_generators() {
    let (_dir, db, detail) = run_agent(
        "git rm -q seed.txt && git commit -q -m 'drop seed'; receipt \"$(git rev-parse HEAD)\"",
    );
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let repo = Path::new(&db).parent().unwrap().join("repo's directory");
    Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,'held',?2,1000)",
            rusqlite::params![run.id(), std::process::id()],
        )
        .unwrap();
    let clock = ManualClock::at(1_005);
    let one_shot = |ids: Vec<&'static str>| {
        runtime::OneShot::new(Generators {
            clock: Arc::new(clock.clone()),
            ids: Arc::new(FixedIds(Mutex::new(ids))),
        })
    };
    let target = || IntegrateTarget::Task(TaskId::new(1));

    // Five seconds after the heartbeat by the injected clock: still held.
    let held = one_shot(vec![])
        .integrate(&db, target(), &repo, None)
        .unwrap_err();
    assert!(
        format!("{held:#}").contains("is held by the supervisor"),
        "{held:#}"
    );

    // Long after it the lease is stale, but a lease is only taken over
    // under its own token.
    clock.set(1_000 + 10 * dagq::domain::HEARTBEAT_TIMEOUT_SECS);
    let leased = one_shot(vec!["other"])
        .integrate(&db, target(), &repo, None)
        .unwrap_err();
    assert!(
        format!("{leased:#}").contains("run is still leased"),
        "{leased:#}"
    );
    let outcome = one_shot(vec!["held"])
        .integrate(&db, target(), &repo, None)
        .unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let started: i64 = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM run_events WHERE run_id=?1 AND kind='integration_started'",
            [run.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(started, 1);
}

/// `status` and `doctor` measure everything to the injected clock's now,
/// read once per call.
#[test]
fn status_and_doctor_measure_to_the_injected_clock() {
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue
        .ask(NewAsk {
            kind: AskKind::Blocked,
            task_id: Some(TaskId::new(1)),
            run_id: None,
            question: "which way?".into(),
            options: vec![],
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
        })
        .unwrap()
        .ask;
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE asks SET created_at=1000", [])
        .unwrap();
    let one_shot = runtime::OneShot::new(Generators {
        clock: Arc::new(ManualClock::at(1_042)),
        ids: Arc::new(FixedIds(Mutex::new(vec![]))),
    });
    let status = one_shot.status_for(&db, None).unwrap();
    assert_eq!(status["checked_at"], 1_042, "{status}");
    assert_eq!(status["asks"][0]["id"], ask.id.as_i64(), "{status}");
    assert_eq!(status["asks"][0]["age_secs"], 42, "{status}");
    let doctor = one_shot.doctor(&db, false).unwrap();
    assert_eq!(doctor["checked_at"], 1_042, "{doctor}");
}

#[test]
fn a_follow_up_draft_records_its_origin_and_its_planner_question_is_delivered_by_the_runtime() {
    use dagq::{
        application::{PlannerAnswerRoute, integrate::register_follow_ups},
        domain::{
            DraftOrigin, NewAsk, PLANNER_QUESTION_OPTIONS, PlannerOrigin, PlannerOwner, Submission,
        },
    };
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let mut queue = SqliteQueue::init(&db).unwrap();
    let source = queue
        .add(NewTask {
            title: "source".into(),
            description: String::new(),
            acceptance: "a".into(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            priority: Default::default(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    queue
        .transition(source.id(), TaskAction::BypassReview)
        .unwrap();
    let dagq::domain::ClaimOutcome::Claimed { run } = queue
        .claim(&sha("0123456789abcdef0123456789abcdef01234567"))
        .unwrap()
    else {
        panic!("nothing claimed");
    };
    // integrate registers the draft one follow-up deeper than its source,
    // with where it came from.
    queue.set_follow_up_depth(source.id(), 1).unwrap();
    let registered = register_follow_ups(
        &mut queue,
        &source,
        run.id(),
        Some(&json!([{"title": "follow", "description": "d"}])),
    );
    let draft = registered[0].task_id;
    assert_eq!(queue.follow_up_depth(draft).unwrap(), 2);
    let (origin, material) = queue.draft_origin(draft).unwrap().unwrap();
    assert_eq!(origin, DraftOrigin::FollowUp);
    assert_eq!(material["source_task_id"], source.id().as_i64());
    assert_eq!(material["source_run_id"], run.id().as_str());
    let targets = queue.planner_drafts().unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].task.id(), draft);

    // A planner of the runtime's may not submit it: it has no goal and is
    // two follow-ups from a person.
    queue
        .edit_task(
            draft,
            dagq::domain::TaskEdit {
                acceptance: Some("works".into()),
                verification_commands: Some(vec!["true".into()]),
                ..Default::default()
            },
        )
        .unwrap();
    let runtime_owner = PlannerOwner {
        origin: PlannerOrigin::Runtime,
        workspace_id: Some("RT".into()),
    };
    let refused = queue
        .submit(Submission {
            tasks: vec![draft],
            goals: Vec::new(),
            proposal: None,
            owner: runtime_owner.clone(),
        })
        .unwrap_err()
        .to_string();
    assert!(refused.contains("planner_question"), "{refused}");

    // So it asks; the question keeps the draft from other planners, and its
    // answer goes to a new planner (none works on the draft).
    let asked = queue
        .ask(NewAsk {
            kind: AskKind::PlannerQuestion,
            task_id: Some(draft),
            run_id: None,
            question: "adopt it?".into(),
            options: PLANNER_QUESTION_OPTIONS
                .iter()
                .map(|o| (*o).into())
                .collect(),
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
        })
        .unwrap()
        .ask;
    assert!(queue.planner_drafts().unwrap().is_empty());
    let answered = queue.answer(asked.id, "adopt").unwrap();
    assert_eq!(
        queue.planner_answer_route(&answered).unwrap(),
        PlannerAnswerRoute::NewPlanner
    );
    let status = runtime::status_for(&db, Some(SessionRole::Inbox)).unwrap();
    let attention = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["ask_id"] == asked.id.as_i64())
        .cloned()
        .unwrap();
    assert_eq!(
        attention["next"],
        format!("delivering the answer of ask {} (runtime)", asked.id)
    );
    let events = dagq::watch::events(&db, dagq::domain::EventId::new(0), 100, false).unwrap();
    assert!(
        !events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "ask_answered"),
        "{events}"
    );

    // With the person's adopt the runtime's planner submits it, and the
    // follow-up counts from 0 again.
    queue
        .submit(Submission {
            tasks: vec![draft],
            goals: Vec::new(),
            proposal: None,
            owner: runtime_owner,
        })
        .unwrap();
    assert_eq!(queue.follow_up_depth(draft).unwrap(), 0);
    let adopted: Vec<_> = queue
        .show(draft)
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind == "follow_up_adopted")
        .collect();
    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0].payload["by"], "person");
    assert_eq!(adopted[0].payload["ask_id"], asked.id.as_i64());
    assert_eq!(adopted[0].payload["source_task_id"], source.id().as_i64());
    // The draft moved on: its answer is closed by the runtime, not typed.
    assert_eq!(
        queue.planner_answer_route(&answered).unwrap(),
        PlannerAnswerRoute::Close
    );
}

/// Supervisor options whose receipt-less idle threshold is one second.
fn stall_options() -> SuperviseOptions {
    SuperviseOptions {
        stall: Some(dagq::domain::stall::StallConfig {
            idle_without_receipt_secs: 1,
            ..Default::default()
        }),
        ..supervise_options(4, true)
    }
}

/// The payloads of the task-less `stall_config_loaded` events.
fn stall_configs(db: &Path) -> Vec<Value> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT payload FROM run_events WHERE kind='stall_config_loaded' ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
        .collect()
}

fn stalled_asks(queue: &SqliteQueue) -> Vec<dagq::domain::Ask> {
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

/// Task 182: the worker committed and stopped while its background
/// `cargo test` ran, without a receipt. Past the threshold the supervisor
/// types one nudge (recorded with the background work), the session
/// answers it with its receipt, and the nudge is recorded as what resolved
/// it. No ask opens.
#[test]
fn a_receiptless_idle_is_nudged_once_and_the_receipt_resolves_it() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        r#"
commit work; idle_bg
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
cp "$MESSAGE" "$MESSAGE.seen"
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#,
    );
    let outcome = supervise_with(&db, &repo, &backend, &stall_options()).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    let texts = backend.texts();
    assert_eq!(texts.len(), 1, "{texts:?}");
    let nudge = &texts[0].1;
    assert!(nudge.contains(run.id().as_str()), "{nudge}");
    assert!(nudge.contains("without a receipt"), "{nudge}");
    assert!(nudge.contains("- cargo test: cargo test"), "{nudge}");
    assert!(nudge.contains("--kind worker_question"), "{nudge}");
    let nudged = payloads(&detail, "stall_nudged");
    assert_eq!(nudged.len(), 1, "{nudged:?}");
    assert_eq!(nudged[0]["phase"], "session");
    assert_eq!(nudged[0]["threshold_secs"], 1);
    assert_eq!(nudged[0]["background_running"], true);
    assert_eq!(nudged[0]["background_tasks"][0]["command"], "cargo test");
    assert!(nudged[0]["idle_secs"].as_i64().unwrap() >= 1);
    let resolved = payloads(&detail, "stall_resolved");
    assert_eq!(resolved.len(), 1, "{resolved:?}");
    assert_eq!(resolved[0]["detection"], "nudge");
    assert_eq!(resolved[0]["outcome"], "resolved_by_nudge");
    assert_eq!(resolved[0]["threshold"], "idle_without_receipt_secs");
    assert_eq!(resolved[0]["threshold_secs"], 1);
    assert!(stalled_asks(&queue).is_empty());
    // The supervisor recorded the thresholds it ran with.
    let configs = stall_configs(&db);
    assert_eq!(configs.len(), 1);
    assert_eq!(configs[0]["idle_without_receipt_secs"], 1);
    assert_eq!(configs[0]["send_confirm_secs"], 60);
    assert_eq!(configs[0]["background_alert_secs"], 1800);
}

/// A session idle without a receipt because its login ran out is neither
/// nudged nor raised as `stalled`: it joins the authentication ask
/// (ADR-0047 decision 42). Once that is answered, the idle counts again and
/// the nudge tells the session to go on.
#[test]
fn an_idle_session_at_a_login_that_ran_out_waits_in_the_authentication_ask() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        r#"
commit work; idle
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#,
    );
    *backend.screen.lock().unwrap() = LOGIN_SCREEN.into();
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &stall_options()))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"auth_required")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    // Well past the threshold, still no nudge and no stalled ask.
    thread::sleep(Duration::from_millis(1500));
    assert!(backend.texts().is_empty());
    assert!(stalled_asks(&queue).is_empty());
    let hold = queue
        .asks(AskQuery {
            open: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hold.len(), 1, "{hold:?}");
    assert_eq!(
        hold[0].reason_category,
        dagq::domain::AskReason::Authentication
    );
    // The error stays on the screen after the person logged in: the
    // answered ask is not opened again, and the nudge goes out.
    queue.answer(hold[0].id, "done").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.texts().len(), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "auth_required").len(), 1);
    assert_eq!(payloads(&detail, "stall_nudged").len(), 1);
    let asks = queue
        .asks(AskQuery {
            all: true,
            ..Default::default()
        })
        .unwrap();
    let holds = asks.iter().filter(|a| a.kind == AskKind::QueueHold).count();
    assert_eq!(holds, 1, "{asks:?}");
}

/// Task 182 to the end: the session takes the nudge and stops again with
/// its background work still running, so one `stalled` ask opens with the
/// background work and the screen. `wait` closes it and counts again, so
/// a second one opens without a second nudge; `intervene` leaves it to a
/// person and no third one follows. The receipt closes the answered ask.
#[test]
fn a_session_idle_after_its_nudge_gets_one_stalled_ask_and_its_answers_are_applied() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        r#"
commit work; idle_bg
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
idle_bg
while [ ! -f "$EXIT.go" ]; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#,
    ));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &stall_options()))
    };
    let mut queue = SqliteQueue::open(&db).unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        !stalled_asks(queue).is_empty()
    });
    let first = stalled_asks(&queue).remove(0);
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(first.run_id.as_ref(), Some(run.id()));
    assert_eq!(first.asked_by, "supervisor");
    assert_eq!(first.options, ["wait", "intervene"]);
    for part in [
        "idle_without_receipt",
        "- cargo test (b1): cargo test",
        "? for shortcuts",
        "`intervene`",
    ] {
        assert!(first.question.contains(part), "{part}: {}", first.question);
    }
    assert_eq!(backend.notifications.lock().unwrap().len(), 1);
    // Not asked twice, nor nudged again.
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(stalled_asks(&queue).len(), 1);
    assert_eq!(backend.texts().len(), 1);

    queue.answer(first.id, "wait").unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        stalled_asks(queue).len() == 2
    });
    assert!(queue.read_ask(first.id).unwrap().closed_at.is_some());
    let second = stalled_asks(&queue).remove(1);
    assert!(second.is_open());
    assert_eq!(backend.texts().len(), 1);

    queue.answer(second.id, "intervene").unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        payloads(&queue.show(TaskId::new(1)).unwrap(), "stall_resolved").len() == 3
    });
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(stalled_asks(&queue).len(), 2);
    assert!(queue.read_ask(second.id).unwrap().closed_at.is_none());
    assert_eq!(
        ask_attention(&runtime::status(&db).unwrap(), second.id)[0]["next"],
        format!("read the answer of ask {} and close it", second.id)
    );

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let closed = queue.read_ask(second.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(closed.answer.as_deref(), Some("intervene"));
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "stall_nudged").len(), 1);
    let resolved: Vec<(&Value, &Value, &Value)> = payloads(&detail, "stall_resolved")
        .into_iter()
        .map(|p| (&p["detection"], &p["outcome"], &p["threshold_secs"]))
        .collect();
    assert_eq!(
        resolved,
        [
            (&json!("nudge"), &json!("escalated"), &json!(1)),
            (&json!("ask"), &json!("answered_wait"), &json!(1)),
            (&json!("ask"), &json!("answered_intervene"), &json!(1)),
        ]
    );
}

/// A worker idle at its own `worker_question` waits for a person, not
/// stalled: nothing is typed until the answer, and no nudge follows it.
#[test]
fn a_worker_idle_at_its_question_is_not_nudged() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        r#"
"$DAGQ" --db "$DB" ask --run "$RUN_ID" --kind worker_question --because scope --question 'Which word?' --cmux /usr/bin/true > /dev/null || exit 70
idle
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
commit work; receipt "$(git rev-parse HEAD)"; idle; await_exit
"#,
    ));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &stall_options()))
    };
    let mut queue = SqliteQueue::open(&db).unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(Default::default()).unwrap().is_empty()
    });
    let ask = queue.asks(Default::default()).unwrap().remove(0);
    thread::sleep(Duration::from_millis(2500));
    assert!(backend.texts().is_empty(), "{:?}", backend.texts());
    queue.answer(ask.id, "blue").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.texts().len(), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(payloads(&detail, "stall_nudged").is_empty());
    assert!(payloads(&detail, "stall_resolved").is_empty());
    assert!(stalled_asks(&queue).is_empty());
}

/// The supervisor that nudged the session and opened its `stalled` ask
/// died: the adopter types nothing and asks nothing again, and closes the
/// ask itself once the session moves on with its receipt.
#[test]
fn an_adopted_stalled_session_is_neither_nudged_nor_asked_again() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        r#"
commit work; idle_bg
while [ ! -f "$EXIT.go" ]; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#,
    ));
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let marker = run.idle_marker_path().unwrap();
    wait_until(&db, Duration::from_secs(30), |_| marker.exists());
    thread::sleep(Duration::from_millis(1100));
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_runtime_event(
            run.id(),
            "stall_nudged",
            json!({"phase": "session", "idle_secs": 1200, "threshold_secs": 1200}),
        )
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            "stall_resolved",
            json!({"phase": "session", "detection": "nudge", "outcome": "escalated"}),
        )
        .unwrap();
    let asked = queue
        .ask(NewAsk {
            kind: AskKind::Stalled,
            task_id: None,
            run_id: Some(run.id().clone()),
            question: "the session is idle without a receipt".into(),
            options: vec!["wait".into(), "intervene".into()],
            asked_by: "supervisor".into(),
            reason_category: dagq::domain::AskReason::RecoveryFailed,
            finding_id: None,
        })
        .unwrap()
        .ask;
    age_lease(&db, &run, 31);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &stall_options()))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !adoption_events(&queue.show(TaskId::new(1)).unwrap()).is_empty()
    });
    thread::sleep(Duration::from_millis(2500));
    assert!(backend.texts().is_empty(), "{:?}", backend.texts());
    assert_eq!(stalled_asks(&queue).len(), 1);
    assert!(queue.read_ask(asked.id).unwrap().is_open());

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let closed = queue.read_ask(asked.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(
        closed.answer.as_deref(),
        Some("the session moved on; closed by the runtime")
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "stall_nudged").len(), 1);
    let resolved = payloads(&detail, "stall_resolved");
    assert_eq!(resolved.len(), 2, "{resolved:?}");
    assert_eq!(resolved[1]["detection"], "ask");
    assert_eq!(resolved[1]["outcome"], "resolved_by_itself");
    assert_eq!(resolved[1]["ask_id"], json!(asked.id));
}

/// The inbox answered `wait` and closed the `stalled` ask itself: the
/// supervisor still takes it as `wait` and asks again once the session
/// stays idle, rather than as a person stepping in.
#[test]
fn a_stalled_ask_closed_after_wait_is_asked_again() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        r#"
commit work; idle_bg
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
idle_bg
while [ ! -f "$EXIT.go" ]; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#,
    ));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &stall_options()))
    };
    let mut queue = SqliteQueue::open(&db).unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        !stalled_asks(queue).is_empty()
    });
    let first = stalled_asks(&queue).remove(0);
    queue.answer(first.id, "wait").unwrap();
    queue.close_ask(first.id).unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        stalled_asks(queue).len() == 2
    });
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let outcomes: Vec<&Value> = payloads(&detail, "stall_resolved")
        .into_iter()
        .map(|p| &p["outcome"])
        .collect();
    assert_eq!(
        outcomes,
        [
            &json!("escalated"),
            &json!("answered_wait"),
            &json!("resolved_by_itself")
        ]
    );
    assert!(
        stalled_asks(&queue)
            .iter()
            .all(|ask| ask.closed_at.is_some())
    );
}

/// Supervisor options that sweep the workspaces of ended runs on every pass.
fn sweeping_options() -> SuperviseOptions {
    SuperviseOptions {
        sweep_interval: Duration::ZERO,
        ..supervise_options(4, true)
    }
}

/// The `workspace_closed` payloads of a run.
fn closes_of(queue: &SqliteQueue, run: &TaskRun) -> Vec<Value> {
    queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "workspace_closed")
        .map(|e| e.payload)
        .collect()
}

/// Task 180: the supervisor's sweep closes the worker workspace of a failed
/// run the triage never takes: its task was canceled, or made ready and
/// run again, or a newer run of the in-progress task took its place. The
/// latest failed run of an in-progress task is the triage's and stays
/// open, a workspace cmux does not list gets no event, and worktrees and
/// branches stay.
#[test]
fn the_sweep_closes_the_workspaces_of_failed_runs_the_triage_does_not_take() {
    let (_dir, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "second task", &[]);
        add_ready_task(&mut queue, "third task", &[]);
    }
    let backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; exit 7",
    );
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first: Vec<TaskRun> = (1..=3)
        .map(|task| queue.show(TaskId::new(task)).unwrap().runs[0].clone())
        .collect();
    for run in &first {
        // The stub `claude` gives no verdict: each waits for a person.
        assert_eq!(run.status(), RunStatus::Failed);
        assert!(run.workspace_closed_at().is_none());
    }
    assert!(backend.closed().is_empty());
    let workspace = |run: &TaskRun| run.workspace_id().unwrap().to_owned();

    // A person cancels task 1 and runs task 2 again; task 3 waits. While
    // cmux does not list task 2's first workspace, nothing is recorded of it.
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    queue.transition(TaskId::new(2), TaskAction::Ready).unwrap();
    backend.hidden.lock().unwrap().push(workspace(&first[1]));
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();
    let closed = closes_of(&queue, &first[0]);
    assert_eq!(
        closed,
        [json!({"workspace_id": workspace(&first[0]), "by": "supervisor", "reason": "superseded"})]
    );
    assert!(
        queue
            .run(first[0].id())
            .unwrap()
            .workspace_closed_at()
            .is_some()
    );
    assert!(backend.closed().contains(&workspace(&first[0])));
    assert!(closes_of(&queue, &first[1]).is_empty());
    let task2 = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(task2.runs.len(), 2);
    assert_eq!(task2.task.status(), TaskStatus::InProgress);

    // Listed again, the first run of task 2 is no longer its task's
    // latest: the next sweep closes it; the other runs are left alone.
    backend.hidden.lock().unwrap().clear();
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();
    assert_eq!(
        closes_of(&queue, &first[1]),
        [json!({"workspace_id": workspace(&first[1]), "by": "supervisor", "reason": "superseded"})]
    );
    assert_eq!(closes_of(&queue, &first[0]).len(), 1);
    // The triage's runs: task 3's only run, task 2's latest.
    assert!(closes_of(&queue, &first[2]).is_empty());
    assert!(!backend.closed().contains(&workspace(&first[2])));
    let latest = queue.show(TaskId::new(2)).unwrap().runs[1].clone();
    assert_eq!(latest.status(), RunStatus::Failed);
    assert!(!backend.closed().contains(&workspace(&latest)));
    // The run of the task that was readied keeps its worktree and branch;
    // the canceled task's go (task 376).
    let run = &first[1];
    assert!(Path::new(run.worktree_path().unwrap()).is_dir());
    git(
        &repo,
        &["rev-parse", "--verify", "--quiet", run.branch().unwrap()],
    );
    assert!(!Path::new(first[0].worktree_path().unwrap()).exists());
}

/// Task 180: a landed run's workspaces are swept too, however it landed:
/// a resume workspace the run's close left open, and the worker workspace
/// of a run landed by hand without its close. A cmux failure records
/// `cleanup_failed` and the sweep goes on to the next; a workspace cmux
/// does not list gets no event.
#[test]
fn the_sweep_closes_every_workspace_left_open_by_a_landed_run() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::Integrated);
    // A resume workspace left open, one cmux no longer lists, and the
    // worker workspace nobody closed, as when a person integrated the run.
    for (workspace, attempt) in [("resume-ws", 1), ("gone-ws", 2)] {
        queue
            .record_runtime_event(
                run.id(),
                "workspace_created",
                json!({"workspace_id": workspace, "resume_attempt": attempt}),
            )
            .unwrap();
    }
    backend.list("resume-ws");
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET workspace_id='hand-ws', workspace_closed_at=NULL WHERE id=?1",
            [run.id()],
        )
        .unwrap();
    backend.list("hand-ws");
    let before = closes_of(&queue, &run).len();

    backend.close_fail = true;
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    let failures = |run: &TaskRun| -> Vec<Value> {
        queue
            .run_events(run.id())
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "cleanup_failed")
            .map(|e| e.payload)
            .collect()
    };
    let failed = failures(&run);
    let workspaces: Vec<&Value> = failed.iter().map(|f| &f["workspace_id"]).collect();
    assert_eq!(
        workspaces,
        [&json!("hand-ws"), &json!("resume-ws")],
        "{failed:?}"
    );
    assert!(failed.iter().all(|f| f["by"] == "supervisor"));
    assert_eq!(closes_of(&queue, &run).len(), before);

    backend.close_fail = false;
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    let closed = closes_of(&queue, &run);
    assert_eq!(
        closed[before..],
        [
            json!({"workspace_id": "hand-ws", "by": "supervisor", "reason": "ended"}),
            json!({"workspace_id": "resume-ws", "by": "supervisor", "reason": "ended"}),
        ]
    );
    assert!(queue.run(run.id()).unwrap().workspace_closed_at().is_some());
    assert!(backend.closed().contains(&"resume-ws".to_owned()));
    assert!(backend.closed().contains(&"hand-ws".to_owned()));
    assert!(!closed.iter().any(|c| c["workspace_id"] == "gone-ws"));

    // Nothing is listed any more: another sweep records nothing.
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    assert_eq!(closes_of(&queue, &run).len(), closed.len());
    assert_eq!(failures(&run).len(), 2);
}

/// A worker script that commits, leaves build outputs in its worktree and
/// fails.
const BUILDING_AGENT: &str = "commit work; mkdir -p target/debug/deps llvm-cov-target; \
     head -c 65536 /dev/zero > target/debug/deps/big; ln target/debug/deps/big target/debug/big; \
     echo p > llvm-cov-target/profraw; receipt \"$(git rev-parse HEAD)\"; exit 7";

/// The payloads of a run's events of `kind`.
fn payloads_of(queue: &SqliteQueue, run: &TaskRun, kind: &str) -> Vec<Value> {
    queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.payload)
        .collect()
}

/// Whether the branch exists in `repo`.
fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .bounded_output()
        .unwrap()
        .status
        .success()
}

/// Task 376: a run that ends loses the build outputs of its worktree at
/// once (`target/`, `llvm-cov-target/`), recorded with the bytes they took;
/// its sources and commit stay. Once its task is canceled the worktree and
/// branch go; a task made ready again keeps its runs' worktrees; the sweep
/// removes build outputs left behind later; a tracked `target/` stays; the
/// checkout the supervisor was given is never touched.
#[test]
fn ended_runs_lose_their_build_outputs_and_canceled_tasks_their_worktrees() {
    let (_dir, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "second task", &[]);
        add_ready_task(&mut queue, "third task", &[]);
    }
    // The main checkout's own build outputs are not the runtime's.
    fs::create_dir_all(repo.join("target/debug")).unwrap();
    fs::write(repo.join("target/debug/mine"), "keep").unwrap();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first: Vec<TaskRun> = (1..=3)
        .map(|task| queue.show(TaskId::new(task)).unwrap().runs[0].clone())
        .collect();
    for run in &first {
        assert_eq!(run.status(), RunStatus::Failed);
        let worktree = Path::new(run.worktree_path().unwrap());
        assert!(!worktree.join("target").exists(), "{}", worktree.display());
        assert!(!worktree.join("llvm-cov-target").exists());
        assert!(worktree.join("change.txt").is_file());
        assert!(Path::new(run.run_dir().unwrap()).is_dir());
        let removed = payloads_of(&queue, run, "build_outputs_removed");
        assert_eq!(removed.len(), 1, "{removed:?}");
        assert_eq!(
            removed[0]["paths"],
            json!([
                worktree.join("target").to_string_lossy(),
                worktree.join("llvm-cov-target").to_string_lossy()
            ])
        );
        assert_eq!(removed[0]["by"], "supervisor");
        // The hard link is counted once.
        let bytes = removed[0]["bytes"].as_u64().unwrap();
        assert!((65536..2 * 65536).contains(&bytes), "{bytes}");
    }
    assert!(repo.join("target/debug/mine").is_file());

    // Task 3 tracks a file under `target/`; build outputs appear again in
    // task 2's worktree; a person cancels task 1 and readies task 2.
    let tracked = Path::new(first[2].worktree_path().unwrap());
    fs::create_dir_all(tracked.join("target")).unwrap();
    fs::write(tracked.join("target/tracked.txt"), "source").unwrap();
    git(tracked, &["add", "target/tracked.txt"]);
    git(tracked, &["commit", "-q", "-m", "track target"]);
    let left = Path::new(first[1].worktree_path().unwrap()).join("target/debug");
    fs::create_dir_all(&left).unwrap();
    fs::write(left.join("left"), "x").unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    queue.transition(TaskId::new(2), TaskAction::Ready).unwrap();
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();

    let canceled = &first[0];
    assert!(!Path::new(canceled.worktree_path().unwrap()).exists());
    assert!(!branch_exists(&repo, canceled.branch().unwrap()));
    let removed = payloads_of(&queue, canceled, "worktree_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["path"], canceled.worktree_path().unwrap());
    assert_eq!(removed[0]["branch"], canceled.branch().unwrap());
    assert_eq!(removed[0]["reason"], "task_canceled");
    assert_eq!(removed[0]["by"], "supervisor");
    assert!(removed[0]["bytes"].as_u64().unwrap() > 0);
    assert!(Path::new(canceled.run_dir().unwrap()).is_dir());

    // Task 2 runs again: its first run keeps its worktree and branch but
    // loses what was built there since.
    let retried = &first[1];
    let worktree = Path::new(retried.worktree_path().unwrap());
    assert!(worktree.join("change.txt").is_file());
    assert!(!worktree.join("target").exists());
    assert!(branch_exists(&repo, retried.branch().unwrap()));
    assert_eq!(
        payloads_of(&queue, retried, "build_outputs_removed").len(),
        2
    );
    assert_eq!(queue.show(TaskId::new(2)).unwrap().runs.len(), 2);

    assert!(tracked.join("target/tracked.txt").is_file());
    assert_eq!(
        payloads_of(&queue, &first[2], "build_outputs_removed").len(),
        1
    );
    assert!(repo.join("target/debug/mine").is_file());

    // Nothing is left: another sweep records nothing.
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();
    assert_eq!(payloads_of(&queue, canceled, "worktree_removed").len(), 1);
    assert_eq!(
        payloads_of(&queue, retried, "build_outputs_removed").len(),
        2
    );
    assert!(payloads_of(&queue, canceled, "cleanup_failed").is_empty());
}

/// Task 376: once a task completes, the worktree and branch of its
/// earlier failed run go too, and a worktree the landing could not remove
/// is removed by the sweep; the landed run keeps its run directory.
#[test]
fn a_completed_task_loses_the_worktrees_of_all_its_runs() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let failed = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(failed.status(), RunStatus::Failed);
    assert!(Path::new(failed.worktree_path().unwrap()).is_dir());

    queue.transition(TaskId::new(1), TaskAction::Ready).unwrap();
    backend.script_for(1, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let landed = detail.runs[1].clone();
    assert_eq!(landed.status(), RunStatus::Integrated);

    assert!(!Path::new(failed.worktree_path().unwrap()).exists());
    assert!(!branch_exists(&repo, failed.branch().unwrap()));
    let removed = payloads_of(&queue, &failed, "worktree_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["reason"], "task_completed");
    assert!(Path::new(failed.run_dir().unwrap()).is_dir());
    assert!(!Path::new(landed.worktree_path().unwrap()).exists());

    // A landed run whose worktree is still there (its removal failed, or a
    // person put it back) is removed by the sweep; its branch was already
    // gone.
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            landed.worktree_path().unwrap(),
            "main",
        ],
    );
    fs::create_dir_all(Path::new(landed.worktree_path().unwrap()).join("target")).unwrap();
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();
    assert!(!Path::new(landed.worktree_path().unwrap()).exists());
    let removed = payloads_of(&queue, &landed, "worktree_removed");
    assert_eq!(
        removed.last().unwrap()["reason"],
        "task_completed",
        "{removed:?}"
    );
    assert!(payloads_of(&queue, &landed, "cleanup_failed").is_empty());
    assert!(Path::new(landed.run_dir().unwrap()).is_dir());
}

/// Task 376: build outputs that cannot be removed record `cleanup_failed`
/// once per process and are removed by a later sweep.
#[test]
fn build_outputs_that_cannot_be_removed_are_retried_by_the_sweep() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let locked = Path::new(run.worktree_path().unwrap()).join("target/locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(locked.join("file"), "x").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    let failed = payloads_of(&queue, &run, "cleanup_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["path"], run.worktree_path().unwrap());
    assert_eq!(failed[0]["by"], "supervisor");
    assert!(locked.join("file").is_file());
    assert_eq!(payloads_of(&queue, &run, "build_outputs_removed").len(), 1);

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    assert!(!locked.exists());
    assert_eq!(payloads_of(&queue, &run, "build_outputs_removed").len(), 2);
}

/// The worker of the `long_background` tests: it commits, leaves an orphan
/// `sleep` in its worktree (its pid in `bg.pid` of the run directory), goes
/// idle with background work running, and writes its receipt once the
/// orphan is gone.
const ORPHAN_AGENT: &str = r#"
commit work
bg="$(dirname "$RECEIPT")/bg.pid"
( sleep 300 >/dev/null 2>&1 & echo $! > "$bg.tmp"; mv "$bg.tmp" "$bg" )
idle_bg
pid=$(cat "$bg")
while kill -0 "$pid" 2>/dev/null; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#;

/// Supervise the fixture's task with [`ORPHAN_AGENT`], a background alert
/// after one second, and `recovery` as the recovery job's script, on a
/// thread.
fn supervise_long_background(
    db: &Path,
    repo: &Path,
    recovery: &str,
) -> (
    Arc<TestWorkspace>,
    Arc<TestReviewer>,
    thread::JoinHandle<Result<Value>>,
) {
    let backend = Arc::new(TestWorkspace::new(db, false, ORPHAN_AGENT));
    let reviewer = Arc::new(
        TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[recovery.to_owned()]),
    );
    let options = SuperviseOptions {
        stall: Some(dagq::domain::stall::StallConfig {
            background_alert_secs: 1,
            ..Default::default()
        }),
        ..supervise_options(4, true)
    };
    let supervisor = {
        let (db, repo, backend, reviewer) = (
            db.to_owned(),
            repo.to_owned(),
            backend.clone(),
            reviewer.clone(),
        );
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    (backend, reviewer, supervisor)
}

/// A recovery job's script that prints `verdict` with `PID` replaced by the
/// orphan's pid.
fn recovery_verdict(verdict: &Value) -> String {
    let text = verdict.to_string().replace("\"PID\"", "$(cat bg.pid)");
    format!("printf '%s\\n' \"{}\"", text.replace('"', "\\\""))
}

/// Task 360 (ADR-0047 decisions 39 and 40): background work past its
/// threshold starts the recovery job with the run's processes; its repair
/// stops only the orphan of the run's worktree, recorded as
/// `auto_repaired`, and the session goes on to its receipt without an ask.
#[test]
fn a_long_background_alert_is_repaired_by_stopping_the_orphan_of_the_worktree() {
    let (_dir, repo, db) = fixture();
    let (backend, reviewer, supervisor) = supervise_long_background(
        &db,
        &repo,
        &recovery_verdict(&json!({
            "verdict": "repair",
            "confidence": "high",
            "diagnosis": "an orphan sleep holds the session",
            "actions": [{"action": "stop_processes", "pids": ["PID"]}],
        })),
    );
    let outcome = supervisor.join().unwrap().unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    let pid: u32 = fs::read_to_string(Path::new(run.run_dir().unwrap()).join("bg.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!pid_alive(pid));
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["alert"], "long_background");
    assert_eq!(requested[0]["attempt"], 1);
    assert_eq!(requested[0]["threshold_secs"], 1);
    assert_eq!(requested[0]["background_tasks"][0]["command"], "cargo test");
    let prompts = reviewer.triage_prompts();
    assert_eq!(prompts.len(), 1);
    let (prompt, cwd) = &prompts[0];
    assert_eq!(cwd, Path::new(run.run_dir().unwrap()));
    for part in [
        "long_background",
        &format!("- pid {pid} (parent "),
        "stop_processes",
        "\"verdict\": \"repair\" | \"escalate\"",
    ] {
        assert!(prompt.contains(part), "{part}: {prompt}");
    }
    let repaired = payloads(&detail, "auto_repaired");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["layer"], "recovery");
    assert_eq!(repaired[0]["repair"], "stop_processes");
    assert_eq!(repaired[0]["processes"][0]["pid"], pid);
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["escalated"], false);
    assert_eq!(finished[0]["applied"], json!(["stop_processes"]));
    assert_eq!(finished[0]["confidence"], "high");
    assert!(stalled_asks(&queue).is_empty());
}

/// Wait for the `stalled` ask of a `long_background` test, check that the
/// orphan still runs, then stop it as a person would and let the run end.
fn escalated_long_background(
    db: &Path,
    backend: &TestWorkspace,
    supervisor: thread::JoinHandle<Result<Value>>,
) -> (dagq::domain::Ask, dagq::domain::TaskDetail) {
    wait_until(db, Duration::from_secs(30), |queue| {
        !stalled_asks(queue).is_empty()
    });
    let mut queue = SqliteQueue::open(db).unwrap();
    let ask = stalled_asks(&queue).remove(0);
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let pid: u32 = fs::read_to_string(Path::new(run.run_dir().unwrap()).join("bg.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid_alive(pid), "the orphan was stopped");
    assert!(payloads(&queue.show(TaskId::new(1)).unwrap(), "auto_repaired").is_empty());
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .unwrap();
    let outcome = supervisor.join().unwrap().unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    (ask, queue.show(TaskId::new(1)).unwrap())
}

/// A repair that names a process outside the run is not applied at all:
/// the runtime refuses the verdict and asks the inbox (`recovery_failed`);
/// the ask closes itself once the session moves on.
#[test]
fn a_recovery_repair_of_a_process_outside_the_run_becomes_an_ask() {
    let (_dir, repo, db) = fixture();
    let mut outsider = std::process::Command::new("/bin/sleep")
        .arg("300")
        .current_dir(repo.parent().unwrap())
        .spawn()
        .unwrap();
    let (backend, _reviewer, supervisor) = supervise_long_background(
        &db,
        &repo,
        &recovery_verdict(&json!({
            "verdict": "repair",
            "confidence": "high",
            "diagnosis": "two sleeps",
            "actions": [{"action": "stop_processes", "pids": ["PID", outsider.id()]}],
        })),
    );
    let (ask, detail) = escalated_long_background(&db, &backend, supervisor);
    assert!(pid_alive(outsider.id()));
    let _ = outsider.kill();
    let _ = outsider.wait();
    assert_eq!(ask.options, ["wait", "intervene"]);
    for part in [
        "alert: long_background",
        "Why a person: recovery_failed",
        &format!(
            "pid {} is not one of the run's own processes",
            outsider.id()
        ),
        "Diagnosis: two sleeps",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["escalated"], true);
    assert_eq!(finished[0]["reason_category"], "recovery_failed");
    assert_eq!(finished[0]["ask_id"], json!(ask.id));
    assert_eq!(ask.reason_category, dagq::domain::AskReason::RecoveryFailed);
    let queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let resolved = payloads(&detail, "stall_resolved");
    assert_eq!(resolved.len(), 1, "{resolved:?}");
    assert_eq!(resolved[0]["threshold"], "background_alert_secs");
    assert_eq!(resolved[0]["outcome"], "resolved_by_itself");
}

/// A repair the job is not sure of is not applied: it becomes an ask with
/// the job's actions as the recommendation and its options added.
#[test]
fn a_recovery_repair_of_low_confidence_becomes_an_ask() {
    let (_dir, repo, db) = fixture();
    let (backend, _reviewer, supervisor) = supervise_long_background(
        &db,
        &repo,
        &recovery_verdict(&json!({
            "verdict": "repair",
            "confidence": "low",
            "diagnosis": "maybe a slow test",
            "actions": [{"action": "stop_processes", "pids": ["PID"]}],
            "options": ["stop it"],
        })),
    );
    let (ask, detail) = escalated_long_background(&db, &backend, supervisor);
    assert_eq!(ask.options, ["wait", "intervene", "stop it"]);
    for part in [
        "confidence low",
        "Recommended: [{\"action\":\"stop_processes\"",
        "Why a person: recovery_failed",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished[0]["escalated"], true);
    assert_eq!(finished[0]["confidence"], "low");
}

/// The only registered supervisor.
fn only_registration(db: &Path) -> dagq::domain::SupervisorRegistration {
    let registrations = SqliteQueue::open(db).unwrap().supervisors().unwrap();
    assert_eq!(registrations.len(), 1, "{registrations:?}");
    registrations.into_iter().next().unwrap()
}

/// Supervise until `condition` holds on the queue, then ask the supervisor
/// to hand off (ADR-0045 decision 10) and return its outcome and token.
fn hand_off_when(
    db: &Path,
    repo: &Path,
    backend: &Arc<TestWorkspace>,
    condition: impl FnMut(&mut SqliteQueue) -> bool,
) -> (Value, String) {
    let supervisor = {
        let (db, repo, backend) = (db.to_owned(), repo.to_owned(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(db, Duration::from_secs(30), condition);
    let registration = only_registration(db);
    assert!(registration.handoff_accepted, "{registration:?}");
    assert_eq!(registration.handoff_binary, None);
    let queue = SqliteQueue::open(db).unwrap();
    assert!(
        queue
            .request_handoff(&registration.token, "/next/dagq")
            .unwrap()
    );
    assert_eq!(
        queue
            .handoff_request(&registration.token)
            .unwrap()
            .as_deref(),
        Some("/next/dagq")
    );
    let outcome = joined(supervisor, "the supervisor asked to hand off").unwrap();
    assert_eq!(outcome["outcome"], "handoff", "{outcome}");
    assert_eq!(outcome["binary"], "/next/dagq");
    assert_eq!(outcome["token"], json!(registration.token));
    // The registration stays for the exec'd process, request and all.
    let kept = only_registration(db);
    assert_eq!(kept.token, registration.token);
    assert_eq!(kept.handoff_binary.as_deref(), Some("/next/dagq"));
    (outcome, registration.token)
}

/// The supervisor the exec'd binary runs: the same token, continued.
fn supervise_after_handoff(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    token: &str,
) -> Result<Value> {
    supervise_with(
        db,
        repo,
        backend,
        &SuperviseOptions {
            handoff_token: Some(token.to_owned()),
            ..supervise_options(4, true)
        },
    )
}

/// A handoff does not wait for the session (ADR-0045 decision 10): the
/// supervisor ends its loop while the worker still works, keeping its
/// registration and the run's lease, and the process that continues it
/// under the same token takes the run over without an adoption and
/// drives it to its receipt, one `/exit` and validation.
#[test]
fn a_handoff_leaves_the_session_running_and_the_next_process_drives_it_on() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, PROMPTED_AGENT));
    let (outcome, token) = hand_off_when(&db, &repo, &backend, |queue| {
        queue
            .show(TaskId::new(1))
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| run.status() == RunStatus::Running)
    });
    assert_eq!(outcome["handed_over"], 1, "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::Running);
    assert_eq!(queue.run_lease(run.id()).unwrap().unwrap().token, token);
    // The session was not asked anything.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);

    let next = {
        let (db, repo, backend, token) = (db.clone(), repo.clone(), backend.clone(), token.clone());
        thread::spawn(move || supervise_after_handoff(&db, &repo, &backend, &token))
    };
    // The registration is taken back before the run moves on.
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue
            .supervisors()
            .unwrap()
            .first()
            .is_some_and(|r| r.handoff_binary.is_none())
    });
    let taken = only_registration(&db);
    assert_eq!(taken.token, token);
    assert_eq!(taken.pid, std::process::id());
    assert_eq!(taken.binary_version.as_deref(), Some(VERSION));
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(next, "the supervisor after the handoff").unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished", "{outcome}");
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);

    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"run_adopted"), "{kinds:?}");
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert_eq!(kinds.iter().filter(|k| **k == "lease_acquired").count(), 1);
    assert_eq!(supervisor_token_of(&db, &run), token);
    assert_eq!(detail.runs.len(), 1);
    assert!(detail.runs[0].result_commit().is_some());
    let handed = events_of(&db, run.id(), runtime::SUPERVISOR_HANDED_OFF);
    assert_eq!(handed.len(), 1);
    assert_eq!(handed[0]["status"], "running");
    assert_eq!(handed[0]["state"], Value::Null);
    assert_eq!(handed[0]["supervisor"], json!(token));
    assert_eq!(handed[0]["previous_version"], VERSION);
    assert_eq!(handed[0]["version"], VERSION);
    // The continued supervisor ended like any other and removed its row.
    assert!(queue.supervisors().unwrap().is_empty());
}

/// A run rejected by validation waits for its session to take the `/exit`;
/// a handoff in that wait carries the request over in the run's
/// `handoff.json`, so the next process sends no second `/exit` and lets
/// the run rest once the session exits.
#[test]
fn a_handoff_while_a_rejected_run_waits_for_its_exit_sends_no_second_exit() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        &format!(
            "commit work; printf 'scratch\\n' > untracked.txt; receipt \"$(git rev-parse HEAD)\"; idle; {HOLD}"
        ),
    ));
    let (outcome, token) = hand_off_when(&db, &repo, &backend, |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"exit_requested")
    });
    assert_eq!(outcome["handed_over"], 1, "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::Failed);
    let snapshot = Path::new(run.run_dir().unwrap()).join("handoff.json");
    let written: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
    assert_eq!(written["phase"], "exit");
    assert_eq!(written["requested"], true);
    assert_eq!(written["close"], false);

    let next = {
        let (db, repo, backend, token) = (db.clone(), repo.clone(), backend.clone(), token.clone());
        thread::spawn(move || supervise_after_handoff(&db, &repo, &backend, &token))
    };
    wait_until(&db, Duration::from_secs(30), |_| !snapshot.exists());
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(next, "the supervisor after the handoff").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "failed");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert!(!kinds.contains(&"run_adopted"), "{kinds:?}");
}

/// A handoff that finds a leased run it has nothing to rebuild from (a
/// resting run without a `handoff.json`) gives the lease back, and a
/// registration that is gone refuses the continuation.
#[test]
fn after_a_handoff_a_run_without_state_gives_its_lease_back() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .register_supervisor("gone-by", std::process::id(), 1, "0.0.1")
        .unwrap();
    // No handoff for a registration that does not take one.
    assert!(!queue.request_handoff("gone-by", "/next/dagq").unwrap());
    queue.accept_handoff("gone-by").unwrap();
    assert!(queue.request_handoff("gone-by", "/next/dagq").unwrap());
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "gone-by");
    backend.join();
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='succeeded' WHERE id=?1",
            [run.id()],
        )
        .unwrap();
    assert_eq!(queue.runs_leased_by("gone-by").unwrap().len(), 1);
    let outcome = supervise_after_handoff(&db, &repo, &backend, "gone-by").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert!(queue.runs_leased_by("gone-by").unwrap().is_empty());
    let error = supervise_after_handoff(&db, &repo, &backend, "gone-by").unwrap_err();
    assert!(
        format!("{error:#}").contains("is no longer registered"),
        "{error:#}"
    );
}
