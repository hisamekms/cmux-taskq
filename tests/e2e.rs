//! End-to-end happy paths through the real binary, real Git, real cmux and
//! real launchd, from `add` to the squash landing by `integrate`, and from
//! `up` to `down`. Claude is replaced by a stub script that does what the
//! prompt asks: change, commit, write the receipt; the test itself plays the
//! session that resolves a conflict. Requires a running cmux, so it is
//! ignored by default: `cargo test --locked --test e2e -- --ignored --nocapture`.
use serde_json::Value;
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_cmux-taskq");
/// Longer than the supervisor's own 120 s exit-request timeout so its error
/// surfaces first.
const SUPERVISE_TIMEOUT: Duration = Duration::from_secs(180);

/// Stand-in for Claude Code. It accepts the argv the Claude adapter builds and
/// follows the prompt: work in the cwd worktree, commit, publish the receipt by
/// atomic rename. Then it behaves like an idle interactive session: it writes
/// the idle marker the Stop hook would write and waits for `/exit` on its
/// terminal, which the supervisor types through cmux.
const STUB: &str = r#"#!/bin/sh
set -eu
if [ "${1:-}" = "--version" ]; then
  printf 'claude-stub 0.0.0\n'
  exit 0
fi
session_id= debug_file= add_dir= settings= prompt=
while [ $# -gt 0 ]; do
  case "$1" in
    --session-id) session_id=$2; shift 2 ;;
    --debug-file) debug_file=$2; shift 2 ;;
    --add-dir) add_dir=$2; shift 2 ;;
    --settings) settings=$2; shift 2 ;;
    --) shift; prompt=$1; shift; break ;;
    *) printf 'stub: unexpected argument %s\n' "$1" >&2; exit 64 ;;
  esac
done
[ $# -eq 0 ] || { printf 'stub: trailing arguments after the prompt\n' >&2; exit 64; }
[ -n "$session_id" ] && [ -n "$debug_file" ] && [ -n "$add_dir" ] && [ -n "$settings" ] && [ -n "$prompt" ] \
  || { printf 'stub: missing arguments\n' >&2; exit 64; }
grep -q '"Stop"' "$settings" || { printf 'stub: settings lack a Stop hook\n' >&2; exit 64; }
{
  printf 'argv: --session-id %s --debug-file %s --add-dir %s --settings %s\n' "$session_id" "$debug_file" "$add_dir" "$settings"
  printf 'cwd: %s\n' "$(pwd)"
} > "$debug_file"
run_id=$(printf '%s\n' "$prompt" | sed -n 's/^You are executing cmux-taskq task [0-9]*, run \(.*\)\.$/\1/p')
[ "$run_id" = "$session_id" ] || { printf 'stub: prompt run %s != session %s\n' "$run_id" "$session_id" >&2; exit 65; }
receipt=$(printf '%s\n' "$prompt" | sed -n 's/^Write a completion receipt to \(.*\) using a temporary file in the same directory.*/\1/p')
[ -n "$receipt" ] || { printf 'stub: prompt does not name the receipt path\n' >&2; exit 65; }
printf 'written by the stub agent for %s\n' "$session_id" > e2e.txt
git add e2e.txt
git commit -q -m 'feat: e2e stub change'
sh -c 'test -f seed.txt'
commit=$(git rev-parse HEAD)
printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"passed","evidence_or_reason":"test -f seed.txt exited 0"},"e2e":{"status":"not_applicable","evidence_or_reason":"stub agent"},"subagent_review":{"status":"not_applicable","evidence_or_reason":"stub agent"},"summary":"added e2e.txt"}\n' \
  "$session_id" "$commit" > "$receipt.tmp"
mv "$receipt.tmp" "$receipt"
printf 'receipt submitted\n'
sleep 2
idle="$add_dir/idle.json"
printf '{"hook_event_name":"Stop","session_id":"%s","stop_hook_active":false}\n' "$session_id" > "$idle.tmp"
mv "$idle.tmp" "$idle"
printf 'idle; waiting for /exit\n'
while read -r line; do
  [ "$line" = "/exit" ] && break
done
printf 'bye\n'
"#;

fn cmux_executable() -> PathBuf {
    env::var_os("CMUX_TASKQ_E2E_CMUX")
        .map(PathBuf::from)
        .unwrap_or_else(|| "cmux".into())
}

/// Fail loudly, never skip, when cmux is missing: the test would prove nothing.
fn preflight(cmux: &Path) -> String {
    let hint = "the e2e test needs a running cmux; put cmux on PATH or set CMUX_TASKQ_E2E_CMUX";
    let ping = Command::new(cmux)
        .arg("ping")
        .output()
        .unwrap_or_else(|error| panic!("cannot run {}: {error}; {hint}", cmux.display()));
    assert!(
        ping.status.success() && String::from_utf8_lossy(&ping.stdout).trim() == "PONG",
        "cmux ping failed ({}): {}{}; {hint}",
        ping.status,
        String::from_utf8_lossy(&ping.stdout),
        String::from_utf8_lossy(&ping.stderr)
    );
    let version = Command::new(cmux).arg("--version").output().unwrap();
    String::from_utf8_lossy(&version.stdout).trim().to_owned()
}

fn git(repo: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}

/// The queue is resolved the way a user's shell would: from the repository as
/// the working directory, with `XDG_DATA_HOME` pointed at the disposable
/// directory instead of the developer's real data home.
struct Env {
    repo: PathBuf,
    data_home: PathBuf,
}

fn taskq(env: &Env, args: &[&str]) -> Value {
    taskq_with(env, &[], args)
}

fn taskq_with(env: &Env, extra: &[(&str, &Path)], args: &[&str]) -> Value {
    let mut command = Command::new(BIN);
    command
        .current_dir(&env.repo)
        .env("XDG_DATA_HOME", &env.data_home)
        .args(args);
    for (key, value) in extra {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "cmux-taskq {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn workspace_listed(cmux: &Path, id: &str) -> bool {
    let output = Command::new(cmux)
        .args(["--json", "--id-format", "uuids", "workspace", "list"])
        .output()
        .unwrap();
    assert!(output.status.success(), "cmux workspace list failed");
    let list: Value = serde_json::from_slice(&output.stdout).unwrap();
    list["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w["id"].as_str().is_some_and(|w| w.eq_ignore_ascii_case(id)))
}

/// Closes the workspaces the supervisor created if they are still open when
/// the test ends, on success and on panic alike. On the happy path the
/// supervisor has already closed them; cmux 0.64 also closes a workspace by
/// itself once its command exits. Either way "not listed" is the expected
/// state, not a failure.
struct WorkspaceGuard {
    cmux: PathBuf,
    ids: Vec<String>,
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        for id in &self.ids {
            if !workspace_listed(&self.cmux, id) {
                eprintln!("workspace {id} already closed");
                continue;
            }
            match Command::new(&self.cmux)
                .args(["workspace", "close"])
                .arg(id)
                .output()
            {
                Ok(output) if output.status.success() => eprintln!("closed workspace {id}"),
                Ok(output) => eprintln!(
                    "closing workspace {id} failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ),
                Err(error) => eprintln!("closing workspace {id} failed: {error}"),
            }
        }
    }
}

/// Kills a still-running supervisor when an assertion fails mid-run.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn reader(mut source: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut text = String::new();
        source.read_to_string(&mut text).unwrap();
        text
    })
}

/// Disposable repository, queue and stub agent, all outside this repository.
struct Fixture {
    _dir: tempfile::TempDir,
    cmux: PathBuf,
    repo: PathBuf,
    stub: PathBuf,
    base: String,
    db: PathBuf,
    env: Env,
}

fn fixture() -> Fixture {
    let cmux = cmux_executable();
    let cmux_version = preflight(&cmux);
    eprintln!("cmux: {cmux_version}");
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.name", "e2e"]);
    git(&repo, &["config", "user.email", "e2e@example.invalid"]);
    fs::write(repo.join("seed.txt"), "fixture\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "seed"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let stub = dir.path().join("claude-stub");
    fs::write(&stub, STUB).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let env = Env {
        repo: repo.clone(),
        data_home: dir.path().join("data"),
    };
    let init = taskq(&env, &["init"]);
    assert_eq!(
        init["schema_version"],
        cmux_taskq::infrastructure::sqlite::SqliteQueue::SCHEMA_VERSION
    );
    let db = PathBuf::from(init["db"].as_str().unwrap());
    assert!(db.starts_with(env.data_home.join("cmux-taskq")));
    assert_eq!(taskq(&env, &["locate"])["db_exists"], true);
    Fixture {
        _dir: dir,
        cmux,
        repo,
        stub,
        base,
        db,
        env,
    }
}

/// Register a ready task whose acceptance the stub agent satisfies.
fn add_ready_task(env: &Env, title: &str, dependencies: &[&str]) -> String {
    let mut args = vec![
        "add",
        title,
        "--description",
        "Add e2e.txt to the worktree",
        "--acceptance",
        "e2e.txt is committed and seed.txt still exists",
        "--verify",
        "test -f seed.txt",
        "--verify",
        "test -f e2e.txt",
    ];
    for dependency in dependencies {
        args.extend(["--depends-on", dependency]);
    }
    let id = taskq(env, &args)["id"].to_string();
    assert_eq!(taskq(env, &["ready", &id])["status"], "ready");
    id
}

/// What one `supervise` pass produced, plus what the test observed while it ran.
struct Pass {
    outcome: Value,
    stderr: String,
    /// Workspace id per task, in the order they were first seen.
    workspaces: Vec<(String, String)>,
    /// Whether every workspace was listed by cmux at one moment; for one task
    /// this is simply "it was listed".
    listed_together: bool,
}

/// Run `supervise --once` with the given extra arguments and watch the runs of
/// `tasks` until it exits: their workspace ids must appear in the queue and in
/// cmux's own list before the sessions end.
fn supervise_once(
    fixture: &Fixture,
    extra: &[&str],
    tasks: &[&str],
    guard: &mut WorkspaceGuard,
) -> Pass {
    let started = Instant::now();
    let mut child = ChildGuard(
        Command::new(BIN)
            .current_dir(&fixture.repo)
            .env("XDG_DATA_HOME", &fixture.env.data_home)
            .arg("supervise")
            .arg("--once")
            .args(extra)
            .arg("--cmux")
            .arg(&fixture.cmux)
            .arg("--claude")
            .arg(&fixture.stub)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = reader(child.0.stdout.take().unwrap());
    let stderr = reader(child.0.stderr.take().unwrap());
    let mut workspaces: Vec<(String, String)> = Vec::new();
    let mut listed_together = false;
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < SUPERVISE_TIMEOUT,
            "supervise did not finish within {SUPERVISE_TIMEOUT:?}"
        );
        for task in tasks {
            if workspaces.iter().any(|(t, _)| t == task) {
                continue;
            }
            let detail = taskq(&fixture.env, &["show", task]);
            if let Some(id) = detail["runs"]
                .as_array()
                .unwrap()
                .last()
                .and_then(|r| r["workspace_id"].as_str())
            {
                uuid::Uuid::parse_str(id).expect("workspace id is a UUID");
                eprintln!(
                    "task {task} workspace {id} registered after {:?}",
                    started.elapsed()
                );
                workspaces.push((task.to_string(), id.to_owned()));
                guard.ids.push(id.to_owned());
            }
        }
        if workspaces.len() == tasks.len() && !listed_together {
            listed_together = workspaces
                .iter()
                .all(|(_, id)| workspace_listed(&fixture.cmux, id));
        }
        thread::sleep(Duration::from_millis(200));
    };
    let supervise_took = started.elapsed();
    let stdout = stdout.join().unwrap();
    let stderr = stderr.join().unwrap();
    eprintln!("supervise finished in {supervise_took:?}\n{stderr}");
    assert!(status.success(), "supervise failed ({status}): {stderr}");
    let outcome: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(outcome["outcome"], "finished", "{outcome}");
    Pass {
        outcome,
        stderr,
        workspaces,
        listed_together,
    }
}

#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn happy_path_runs_a_stub_agent_through_cmux_and_lands_on_main() {
    let fixture = fixture();
    let Fixture {
        cmux,
        repo,
        base,
        db,
        env,
        ..
    } = &fixture;
    let task_id = add_ready_task(env, "e2e stub task", &[]);
    assert_eq!(taskq(env, &["candidates"]).as_array().unwrap().len(), 1);

    let mut guard = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };
    let pass = supervise_once(&fixture, &[], &[&task_id], &mut guard);
    let workspace = pass.workspaces[0].1.clone();
    assert!(
        pass.listed_together,
        "workspace {workspace} never appeared in cmux workspace list"
    );
    let outcome = &pass.outcome;
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 1, "{outcome}");
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    assert_eq!(
        outcome["runs"][0]["status"], "awaiting_integration",
        "{outcome}"
    );
    let stderr = &pass.stderr;
    let base = base.as_str();
    let repo = repo.as_path();
    let db = db.as_path();

    let detail = taskq(env, &["show", &task_id]);
    assert_eq!(detail["task"]["status"], "in_progress");
    let runs = detail["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    let run_id = run["id"].as_str().unwrap();
    assert_eq!(run["status"], "awaiting_integration");
    assert_eq!(run["workspace_id"], workspace.as_str());
    assert_eq!(run["base_commit"], base);
    assert!(run["last_error"].is_null());
    assert_eq!(run["branch"], format!("taskq/{run_id}"));
    assert!(run["workspace_closed_at"].is_number(), "{run}");
    assert!(
        !workspace_listed(cmux, &workspace),
        "workspace {workspace} is still open after the run was accepted"
    );
    assert!(stderr.contains("awaiting_integration"), "{stderr}");

    // The run lives next to the queue, and its worktree resolves the same queue.
    let run_dir = Path::new(run["run_dir"].as_str().unwrap());
    assert_eq!(
        run_dir,
        db.canonicalize()
            .unwrap()
            .with_file_name("runs")
            .join(run_id)
    );
    let worktree = Path::new(run["worktree_path"].as_str().unwrap());
    assert_eq!(worktree, run_dir.join("worktree"));
    let from_worktree = Env {
        repo: worktree.to_path_buf(),
        data_home: env.data_home.clone(),
    };
    assert_eq!(
        taskq(&from_worktree, &["locate"])["db"],
        db.to_str().unwrap()
    );
    let head = git(worktree, &["rev-parse", "HEAD"]);
    assert_eq!(git(repo, &["rev-parse", "main"]), base); // Not merged by the supervisor.
    assert_ne!(head, base);
    assert_eq!(run["result_commit"], head.as_str());
    assert_eq!(
        git(worktree, &["symbolic-ref", "HEAD"]),
        format!("refs/heads/taskq/{run_id}")
    );
    assert_eq!(git(worktree, &["status", "--porcelain"]), "");
    assert_eq!(
        fs::read_to_string(worktree.join("e2e.txt")).unwrap(),
        format!("written by the stub agent for {run_id}\n")
    );
    assert!(!repo.join("e2e.txt").exists()); // main is untouched.

    let receipt: Value =
        serde_json::from_str(&fs::read_to_string(run["receipt_path"].as_str().unwrap()).unwrap())
            .unwrap();
    assert_eq!(receipt["run_id"], run_id);
    assert_eq!(receipt["commit"], head.as_str());
    let log = fs::read_to_string(run["log_path"].as_str().unwrap()).unwrap();
    assert!(
        log.contains(&format!(
            "argv: --session-id {run_id} --debug-file {} --add-dir {run_dir} --settings {run_dir}/claude-settings.json",
            run["log_path"].as_str().unwrap(),
            run_dir = run["run_dir"].as_str().unwrap()
        )),
        "{log}"
    );

    let kinds: Vec<&str> = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    for expected in [
        "lease_acquired",
        "worktree_created",
        "workspace_created",
        "wrapper_started",
        "agent_started",
        "receipt_observed",
        "session_idle_observed",
        "exit_requested",
        "session_exited",
        "supervision_finished",
        "verification_command",
        "validation_finished",
        "workspace_closed",
        "lease_released",
    ] {
        assert!(kinds.contains(&expected), "missing {expected} in {kinds:?}");
    }
    let events = detail["events"].as_array().unwrap();
    let event = |kind: &str| events.iter().find(|e| e["kind"] == kind).unwrap();
    assert_eq!(
        event("workspace_created")["payload"]["workspace_id"],
        workspace.as_str()
    );
    assert_eq!(
        event("session_idle_observed")["payload"]["session_id"],
        run_id
    );
    assert_eq!(
        event("exit_requested")["payload"]["workspace_id"],
        workspace.as_str()
    );
    assert!(!kinds.contains(&"exit_request_timed_out"), "{kinds:?}");
    assert_eq!(event("session_exited")["payload"]["exit_code"], 0);
    assert_eq!(
        event("supervision_finished")["payload"]["status"],
        "validating"
    );
    let verifications: Vec<&Value> = events
        .iter()
        .filter(|e| e["kind"] == "verification_command")
        .collect();
    assert_eq!(verifications.len(), 2);
    assert!(verifications.iter().all(|e| e["payload"]["exit_code"] == 0));
    let finished = event("validation_finished");
    assert_eq!(finished["payload"]["status"], "awaiting_integration");
    assert_eq!(finished["payload"]["result_commit"], head.as_str());
    assert_eq!(finished["payload"]["receipt"]["summary"], "added e2e.txt");
    assert!(!kinds.contains(&"cleanup_failed"), "{kinds:?}");

    let processes = detail["processes"].as_array().unwrap();
    assert_eq!(processes.len(), 2);
    assert!(processes.iter().all(|p| p["exit_code"] == 0));

    // The task stays taken until integration; the lease is gone.
    assert_eq!(taskq(env, &["candidates"]).as_array().unwrap().len(), 0);
    let status = taskq(env, &["status"]);
    assert_eq!(status["supervisors"], Value::Array(vec![]), "{status}");
    assert_eq!(status["runs"], Value::Array(vec![]), "{status}");

    // Landing is the runtime's job: one squash commit on main with the run's tree.
    let integrated = taskq(env, &["integrate", &task_id]);
    assert_eq!(integrated["outcome"], "integrated", "{integrated}");
    assert_eq!(integrated["task"]["status"], "completed");
    assert_eq!(integrated["run"]["status"], "integrated");
    let main = git(repo, &["rev-parse", "main"]);
    assert_ne!(main, head);
    assert_eq!(integrated["run"]["result_commit"], main.as_str());
    assert_eq!(git(repo, &["rev-parse", "main^"]), base);
    assert_eq!(
        git(repo, &["rev-parse", "main^{tree}"]),
        git(repo, &["rev-parse", &format!("{head}^{{tree}}")])
    );
    assert_eq!(
        git(repo, &["log", "-1", "--format=%B", "main"]),
        format!("e2e stub task\n\nadded e2e.txt\n\nTaskq-Task: {task_id}\nTaskq-Run: {run_id}")
    );
    assert_eq!(git(repo, &["status", "--porcelain"]), ""); // The checkout moved with main.
    assert!(repo.join("e2e.txt").exists());
    assert_eq!(
        git(repo, &["rev-parse", &format!("refs/taskq/runs/{run_id}")]),
        head
    );
    assert!(!worktree.exists(), "landed worktree was not removed");
    assert_eq!(
        git(repo, &["branch", "--list", &format!("taskq/{run_id}")]),
        ""
    );
    let detail = taskq(env, &["show", &task_id]);
    assert_eq!(detail["task"]["status"], "completed");
    assert_eq!(detail["runs"][0]["status"], "integrated");
    let kinds: Vec<&str> = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    for expected in [
        "integration_started",
        "integration_rebased",
        "run_integrated",
        "worktree_removed",
    ] {
        assert!(kinds.contains(&expected), "missing {expected} in {kinds:?}");
    }
    assert!(!kinds.contains(&"cleanup_failed"), "{kinds:?}");
    assert_eq!(
        taskq(env, &["integrate", "--next"])["outcome"],
        "no_run_awaiting"
    );
    assert_eq!(taskq(env, &["status"])["runs"], Value::Array(vec![]));
}

/// Two independent tasks run in two cmux workspaces at once; the task that
/// depends on one of them waits for its integration and then starts from
/// the main that contains it.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn two_independent_tasks_run_concurrently_and_a_dependent_follows_integration() {
    let fixture = fixture();
    let Fixture {
        cmux,
        repo,
        base,
        env,
        ..
    } = &fixture;
    let first = add_ready_task(env, "e2e first", &[]);
    let second = add_ready_task(env, "e2e second", &[]);
    let third = add_ready_task(env, "e2e dependent", &[&first]);
    assert_eq!(taskq(env, &["candidates"]).as_array().unwrap().len(), 2);

    let mut guard = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };
    let pass = supervise_once(
        &fixture,
        &["--parallel", "2"],
        &[&first, &second],
        &mut guard,
    );
    assert!(
        pass.listed_together,
        "both workspaces were never open at the same time: {:?}",
        pass.workspaces
    );
    assert_ne!(pass.workspaces[0].1, pass.workspaces[1].1);
    let outcome = &pass.outcome;
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    let runs = outcome["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2, "{outcome}");
    assert!(
        runs.iter().all(|r| r["status"] == "awaiting_integration"),
        "{outcome}"
    );
    for task in [&first, &second] {
        let detail = taskq(env, &["show", task]);
        assert_eq!(detail["task"]["status"], "in_progress");
        let run = &detail["runs"][0];
        assert_eq!(run["status"], "awaiting_integration");
        assert_eq!(run["base_commit"], base.as_str());
        assert!(run["workspace_closed_at"].is_number(), "{run}");
        assert!(run["last_error"].is_null(), "{run}");
        let worktree = Path::new(run["worktree_path"].as_str().unwrap());
        assert_eq!(git(worktree, &["status", "--porcelain"]), "");
    }
    // The dependent never started: awaiting integration is not completion.
    let detail = taskq(env, &["show", &third]);
    assert_eq!(detail["task"]["status"], "ready");
    assert_eq!(detail["runs"], Value::Array(vec![]));
    assert_eq!(taskq(env, &["candidates"]).as_array().unwrap().len(), 0);
    assert_eq!(taskq(env, &["doctor"])["runs"], Value::Array(vec![]));

    // Land the first task; the dependent becomes claimable from the landed main.
    let first_run = taskq(env, &["show", &first])["runs"][0].clone();
    let first_commit = first_run["result_commit"].as_str().unwrap().to_owned();
    assert_eq!(taskq(env, &["integrate", &first])["outcome"], "integrated");
    let first_landed = git(repo, &["rev-parse", "main"]);
    assert_ne!(first_landed, first_commit);
    assert_eq!(git(repo, &["rev-parse", "main^"]), base.as_str());
    assert_eq!(taskq(env, &["candidates"])[0]["id"].to_string(), third);
    let pass = supervise_once(&fixture, &["--parallel", "2"], &[&third], &mut guard);
    assert_eq!(pass.outcome["runs"].as_array().unwrap().len(), 1);
    let run = taskq(env, &["show", &third])["runs"][0].clone();
    assert_eq!(run["status"], "awaiting_integration", "{run}");
    assert_eq!(run["base_commit"], first_landed.as_str());
    assert_eq!(git(repo, &["rev-parse", "main"]), first_landed);
    assert_eq!(taskq(env, &["status"])["supervisors"], Value::Array(vec![]));

    // The merge queue is FIFO by validation time: --next takes the second
    // task first. It rewrote the same file as the first, so the runtime
    // cannot rebase it and parks it for a session; the next --next lands the
    // dependent, which sits on the first landing.
    let parked = taskq(env, &["integrate", "--next"]);
    assert_eq!(parked["outcome"], "needs_session", "{parked}");
    assert_eq!(parked["run"]["task_id"].to_string(), second);
    assert!(
        parked["reason"]
            .as_str()
            .unwrap()
            .contains("conflicted in e2e.txt"),
        "{parked}"
    );
    let next = taskq(env, &["integrate", "--next"]);
    assert_eq!(next["outcome"], "integrated", "{next}");
    assert_eq!(next["task"]["id"].to_string(), third);
    let third_landed = git(repo, &["rev-parse", "main"]);
    assert_eq!(git(repo, &["rev-parse", "main^"]), first_landed);

    // The session resolves the parked run on top of main and rewrites its receipt.
    let run = taskq(env, &["show", &second])["runs"][0].clone();
    assert_eq!(run["status"], "needs_session");
    let worktree = Path::new(run["worktree_path"].as_str().unwrap());
    assert_eq!(
        git(worktree, &["rev-parse", "HEAD"]),
        run["result_commit"].as_str().unwrap()
    );
    assert_eq!(
        taskq(env, &["integrate", "--next"])["outcome"],
        "no_run_awaiting"
    );
    let rebase = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rebase", &third_landed])
        .output()
        .unwrap();
    assert!(!rebase.status.success());
    fs::write(worktree.join("e2e.txt"), "resolved by the session\n").unwrap();
    git(worktree, &["add", "e2e.txt"]);
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(worktree)
            .env("GIT_EDITOR", "true")
            .args(["rebase", "--continue"])
            .status()
            .unwrap()
            .success()
    );
    let resolved = git(worktree, &["rev-parse", "HEAD"]);
    let receipt_path = Path::new(run["receipt_path"].as_str().unwrap());
    let mut receipt: Value =
        serde_json::from_str(&fs::read_to_string(receipt_path).unwrap()).unwrap();
    receipt["commit"] = Value::String(resolved.clone());
    receipt["summary"] = Value::String("resolved e2e.txt".into());
    fs::write(receipt_path.with_extension("tmp"), receipt.to_string()).unwrap();
    fs::rename(receipt_path.with_extension("tmp"), receipt_path).unwrap();
    let landed = taskq(env, &["integrate", &second]);
    assert_eq!(landed["outcome"], "integrated", "{landed}");
    assert_eq!(git(repo, &["rev-parse", "main^"]), third_landed);
    assert_eq!(
        git(repo, &["rev-list", "--count", &format!("{base}..main")]),
        "3"
    );
    assert_eq!(
        fs::read_to_string(repo.join("e2e.txt")).unwrap(),
        "resolved by the session\n"
    );
    for task in [&first, &second, &third] {
        let detail = taskq(env, &["show", task]);
        assert_eq!(detail["task"]["status"], "completed", "{task}");
        assert!(
            !Path::new(detail["runs"][0]["worktree_path"].as_str().unwrap()).exists(),
            "{task}"
        );
    }
    assert_eq!(
        git(repo, &["for-each-ref", "refs/taskq/runs/"])
            .lines()
            .count(),
        3
    );
}

/// Unloads the LaunchAgent the test bootstrapped if it is still there when
/// the test ends, so a failed assertion does not leave a supervisor
/// restarting forever against a deleted queue.
struct AgentGuard {
    label: String,
    plist: PathBuf,
}

impl AgentGuard {
    fn loaded(&self) -> bool {
        Command::new("launchctl")
            .args(["print", &format!("gui/{}/{}", uid(), self.label)])
            .output()
            .unwrap()
            .status
            .success()
    }
}

impl Drop for AgentGuard {
    fn drop(&mut self) {
        if self.loaded() {
            let output = Command::new("launchctl")
                .args(["bootout", &format!("gui/{}/{}", uid(), self.label)])
                .output()
                .unwrap();
            eprintln!(
                "booted out {} ({}): {}",
                self.label,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        if self.plist.exists() {
            let _ = fs::remove_file(&self.plist);
            eprintln!("removed {}", self.plist.display());
        }
    }
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .unwrap()
        .status
        .success()
}

/// `up` bootstraps the supervisor as a LaunchAgent of the real launchd and
/// opens the maintainer workspace in the real cmux; `status` lists the
/// supervisor through its registration; `down --wait` unloads the agent
/// and returns once the supervisor has drained and deregistered.
#[test]
#[ignore = "needs a running cmux and launchd; run with --ignored"]
fn up_starts_a_launchd_supervisor_that_status_lists_and_down_wait_stops_it() {
    let fixture = fixture();
    let Fixture {
        cmux,
        repo,
        stub,
        db,
        env,
        ..
    } = &fixture;
    // The agent's plist goes under a disposable HOME, not the developer's.
    let home = fixture._dir.path().join("home");
    fs::create_dir(&home).unwrap();
    let located = taskq_with(env, &[("HOME", home.as_path())], &["locate"]);
    let label = located["label"].as_str().unwrap().to_owned();
    let plist = PathBuf::from(located["launch_agent"].as_str().unwrap());
    assert!(plist.starts_with(&home));
    let log_dir = PathBuf::from(located["log_dir"].as_str().unwrap());
    let agent = AgentGuard {
        label: label.clone(),
        plist: plist.clone(),
    };
    let mut workspaces = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };

    let up_args = [
        "up",
        "--parallel",
        "2",
        "--cmux",
        cmux.to_str().unwrap(),
        "--claude",
        stub.to_str().unwrap(),
    ];
    let started = Instant::now();
    // A supervisor that never registers is diagnosed from launchd.log,
    // which `up` names in its error; show it before failing.
    let output = {
        let mut command = Command::new(BIN);
        command
            .current_dir(&env.repo)
            .env("XDG_DATA_HOME", &env.data_home)
            .env("HOME", &home)
            .args(up_args);
        command.output().unwrap()
    };
    if !output.status.success() {
        let launchd_log = log_dir.join("launchd.log");
        eprintln!(
            "launchd.log:\n{}",
            fs::read_to_string(&launchd_log).unwrap_or_else(|_| "(not written)".into())
        );
        panic!("cmux-taskq up: {}", String::from_utf8_lossy(&output.stderr));
    }
    let first: Value = serde_json::from_slice(&output.stdout).unwrap();
    eprintln!("up took {:?}: {first}", started.elapsed());
    assert_eq!(first["supervisor"]["outcome"], "started", "{first}");
    let pid = u32::try_from(first["supervisor"]["pid"].as_u64().unwrap()).unwrap();
    assert!(pid_alive(pid));
    assert_eq!(first["supervisor"]["plist"], plist.to_str().unwrap());
    assert_eq!(first["supervisor"]["log_dir"], log_dir.to_str().unwrap());
    assert_eq!(first["pruned_supervisors"], Value::Array(vec![]));
    assert_eq!(first["maintainer"]["outcome"], "created", "{first}");
    let maintainer = first["maintainer"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    uuid::Uuid::parse_str(&maintainer).expect("workspace id is a UUID");
    workspaces.ids.push(maintainer.clone());
    assert!(workspace_listed(cmux, &maintainer));
    let repo_name = repo.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        first["maintainer"]["name"],
        format!("taskq {repo_name} maintainer")
    );
    // launchd knows the agent, and the plist is what `up` described.
    assert!(
        agent.loaded(),
        "launchctl print gui/{}/{label} failed",
        uid()
    );
    let contents = fs::read_to_string(&plist).unwrap();
    assert!(contents.contains(&format!("<string>{label}</string>")));
    assert!(contents.contains("<string>supervise</string>"));
    assert!(contents.contains(&format!("<string>{}</string>", db.display())));
    assert!(contents.contains("<key>KeepAlive</key>\n\t<true/>"));
    // The launchd-run supervisor wrote its own log.
    let logs: Vec<PathBuf> = fs::read_dir(&log_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with(&format!("-{pid}.log"))
        })
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?}");
    let log = fs::read_to_string(&logs[0]).unwrap();
    assert!(
        log.contains(&format!("started: pid {pid}, parallel 2")),
        "{log}"
    );

    let status = taskq(env, &["status"]);
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 1, "{status}");
    assert_eq!(supervisors[0]["pid"], pid);
    assert_eq!(supervisors[0]["registered"], true);
    assert_eq!(supervisors[0]["alive"], true);
    assert_eq!(supervisors[0]["stale"], false);
    assert_eq!(supervisors[0]["parallel"], 2);
    assert_eq!(status["runs"], Value::Array(vec![]));

    // Idempotent: nothing is started or opened twice.
    let second = taskq_with(env, &[("HOME", home.as_path())], &up_args);
    assert_eq!(second["supervisor"]["outcome"], "reused", "{second}");
    assert_eq!(second["supervisor"]["pid"], pid);
    assert_eq!(second["maintainer"]["outcome"], "reused", "{second}");
    assert_eq!(second["maintainer"]["workspace_id"], maintainer.as_str());
    assert_eq!(second["pruned_supervisors"], Value::Array(vec![]));

    let started = Instant::now();
    let down = taskq_with(env, &[("HOME", home.as_path())], &["down", "--wait"]);
    eprintln!("down --wait took {:?}: {down}", started.elapsed());
    assert_eq!(down["outcome"], "stopped", "{down}");
    assert_eq!(down["pid"], pid);
    assert_eq!(down["launch_agent_unloaded"], true);
    assert!(!pid_alive(pid), "supervisor {pid} is still alive");
    assert!(!agent.loaded(), "the agent is still loaded");
    assert!(!plist.exists(), "the plist was not removed");
    let status = taskq(env, &["status"]);
    assert_eq!(status["supervisors"], Value::Array(vec![]), "{status}");
    let log = fs::read_to_string(&logs[0]).unwrap();
    assert!(
        log.contains("exiting: {\"errors\":[],\"outcome\":\"stopped\""),
        "{log}"
    );
    // The maintainer workspace is left open by `down`; the guard closes it.
    assert!(workspace_listed(cmux, &maintainer));
    let again = taskq_with(env, &[("HOME", home.as_path())], &["down"]);
    assert_eq!(again["outcome"], "not_running", "{again}");
}
