//! End-to-end happy paths through the real binary, real Git, real cmux and
//! real launchd, from `add` to the squash landing by `integrate`, from
//! `up` to `down`, and from a killed supervisor to the adoption of its run.
//! Claude is replaced by a stub script that does what the prompt asks:
//! change, commit, write the receipt; the test itself plays the session
//! that resolves a conflict. Requires a running cmux, so it is ignored by
//! default: `cargo test --locked --test e2e -- --ignored --nocapture`.
use serde_json::Value;
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_dagq");
/// The version the binary under test records on its registration.
const VERSION: &str = dagq::VERSION;
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
run_id=$(printf '%s\n' "$prompt" | sed -n 's/^You are executing dagq task [0-9]*, run \(.*\)\.$/\1/p')
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
    env::var_os("DAGQ_E2E_CMUX")
        .map(PathBuf::from)
        .unwrap_or_else(|| "cmux".into())
}

/// Fail loudly, never skip, when cmux is missing: the test would prove nothing.
fn preflight(cmux: &Path) -> String {
    let hint = "the e2e test needs a running cmux; put cmux on PATH or set DAGQ_E2E_CMUX";
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

fn dagq(env: &Env, args: &[&str]) -> Value {
    dagq_with(env, &[], args)
}

fn dagq_with(env: &Env, extra: &[(&str, &Path)], args: &[&str]) -> Value {
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
        "dagq {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The workspace's entry in `cmux --json workspace list`, while it is listed.
fn listed_workspace(cmux: &Path, id: &str) -> Option<Value> {
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
        .find(|w| w["id"].as_str().is_some_and(|w| w.eq_ignore_ascii_case(id)))
        .cloned()
}

fn workspace_listed(cmux: &Path, id: &str) -> bool {
    listed_workspace(cmux, id).is_some()
}

/// cmux confirms a `workspace close` before the workspace leaves its
/// listing, so "gone" is waited for rather than asserted on the first look.
fn wait_until_not_listed(cmux: &Path, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while workspace_listed(cmux, id) {
        assert!(
            Instant::now() < deadline,
            "workspace {id} is still listed 30s after it was closed"
        );
        thread::sleep(Duration::from_millis(200));
    }
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
    let init = dagq(&env, &["init"]);
    assert_eq!(
        init["schema_version"],
        dagq::infrastructure::sqlite::SqliteQueue::SCHEMA_VERSION
    );
    let db = PathBuf::from(init["db"].as_str().unwrap());
    assert!(db.starts_with(env.data_home.join("dagq")));
    assert_eq!(dagq(&env, &["locate"])["db_exists"], true);
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
    let id = dagq(env, &args)["id"].to_string();
    assert_eq!(dagq(env, &["ready", &id])["status"], "ready");
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
    /// Each workspace's cmux listing entry from the moment they were listed together.
    listings: Vec<Value>,
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
    let mut listings = Vec::new();
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
            let detail = dagq(&fixture.env, &["show", task, "--full"]);
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
            let entries: Vec<Value> = workspaces
                .iter()
                .filter_map(|(_, id)| listed_workspace(&fixture.cmux, id))
                .collect();
            listed_together = entries.len() == workspaces.len();
            if listed_together {
                listings = entries;
            }
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
        listings,
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
    assert_eq!(dagq(env, &["candidates"]).as_array().unwrap().len(), 1);

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

    let detail = dagq(env, &["show", &task_id, "--full"]);
    assert_eq!(detail["task"]["status"], "in_progress");
    let runs = detail["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    let run_id = run["id"].as_str().unwrap();
    assert_eq!(run["status"], "awaiting_integration");
    assert_eq!(run["workspace_id"], workspace.as_str());
    assert_eq!(run["base_commit"], base);
    assert!(run["last_error"].is_null());
    // ADR-0018: the name carries the repository, task and title; the run
    // ID lives in the description.
    let listing = &pass.listings[0];
    assert_eq!(
        listing["custom_title"],
        format!(
            "[{}]dagq#{task_id} e2e stub task",
            repo.file_name().unwrap().to_string_lossy()
        ),
        "{listing}"
    );
    assert_eq!(listing["description"], format!("run {run_id}"), "{listing}");
    assert_eq!(run["branch"], format!("dagq/{run_id}"));
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
        dagq(&from_worktree, &["locate"])["db"],
        db.to_str().unwrap()
    );
    let head = git(worktree, &["rev-parse", "HEAD"]);
    assert_eq!(git(repo, &["rev-parse", "main"]), base); // Not merged by the supervisor.
    assert_ne!(head, base);
    assert_eq!(run["result_commit"], head.as_str());
    assert_eq!(
        git(worktree, &["symbolic-ref", "HEAD"]),
        format!("refs/heads/dagq/{run_id}")
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
    assert_eq!(dagq(env, &["candidates"]).as_array().unwrap().len(), 0);
    let status = dagq(env, &["status"]);
    assert_eq!(status["supervisors"], Value::Array(vec![]), "{status}");
    assert_eq!(status["runs"], Value::Array(vec![]), "{status}");

    // Landing is the runtime's job: one squash commit on main with the run's tree.
    let integrated = dagq(env, &["integrate", &task_id]);
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
        format!("e2e stub task\n\nadded e2e.txt\n\nDagq-Task: {task_id}\nDagq-Run: {run_id}")
    );
    assert_eq!(git(repo, &["status", "--porcelain"]), ""); // The checkout moved with main.
    assert!(repo.join("e2e.txt").exists());
    assert_eq!(
        git(repo, &["rev-parse", &format!("refs/dagq/runs/{run_id}")]),
        head
    );
    assert!(!worktree.exists(), "landed worktree was not removed");
    assert_eq!(
        git(repo, &["branch", "--list", &format!("dagq/{run_id}")]),
        ""
    );
    let detail = dagq(env, &["show", &task_id, "--full"]);
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
        dagq(env, &["integrate", "--next"])["outcome"],
        "no_run_awaiting"
    );
    assert_eq!(dagq(env, &["status"])["runs"], Value::Array(vec![]));
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
    assert_eq!(dagq(env, &["candidates"]).as_array().unwrap().len(), 2);

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
        let detail = dagq(env, &["show", task, "--full"]);
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
    let detail = dagq(env, &["show", &third, "--full"]);
    assert_eq!(detail["task"]["status"], "ready");
    assert_eq!(detail["runs"], Value::Array(vec![]));
    assert_eq!(dagq(env, &["candidates"]).as_array().unwrap().len(), 0);
    assert_eq!(
        dagq(env, &["doctor", "--full"])["runs"],
        Value::Array(vec![])
    );

    // Land the first task; the dependent becomes claimable from the landed main.
    let first_run = dagq(env, &["show", &first, "--full"])["runs"][0].clone();
    let first_commit = first_run["result_commit"].as_str().unwrap().to_owned();
    assert_eq!(dagq(env, &["integrate", &first])["outcome"], "integrated");
    let first_landed = git(repo, &["rev-parse", "main"]);
    assert_ne!(first_landed, first_commit);
    assert_eq!(git(repo, &["rev-parse", "main^"]), base.as_str());
    assert_eq!(dagq(env, &["candidates"])[0]["id"].to_string(), third);
    let pass = supervise_once(&fixture, &["--parallel", "2"], &[&third], &mut guard);
    assert_eq!(pass.outcome["runs"].as_array().unwrap().len(), 1);
    let run = dagq(env, &["show", &third, "--full"])["runs"][0].clone();
    assert_eq!(run["status"], "awaiting_integration", "{run}");
    assert_eq!(run["base_commit"], first_landed.as_str());
    assert_eq!(git(repo, &["rev-parse", "main"]), first_landed);
    assert_eq!(dagq(env, &["status"])["supervisors"], Value::Array(vec![]));

    // The merge queue is FIFO by validation time: --next takes the second
    // task first. It rewrote the same file as the first, so the runtime
    // cannot rebase it and parks it for a session; the next --next lands the
    // dependent, which sits on the first landing.
    let parked = dagq(env, &["integrate", "--next"]);
    assert_eq!(parked["outcome"], "needs_session", "{parked}");
    assert_eq!(parked["run"]["task_id"].to_string(), second);
    assert!(
        parked["reason"]
            .as_str()
            .unwrap()
            .contains("conflicted in e2e.txt"),
        "{parked}"
    );
    let next = dagq(env, &["integrate", "--next"]);
    assert_eq!(next["outcome"], "integrated", "{next}");
    assert_eq!(next["task"]["id"].to_string(), third);
    let third_landed = git(repo, &["rev-parse", "main"]);
    assert_eq!(git(repo, &["rev-parse", "main^"]), first_landed);

    // The session resolves the parked run on top of main and rewrites its receipt.
    let run = dagq(env, &["show", &second, "--full"])["runs"][0].clone();
    assert_eq!(run["status"], "needs_session");
    let worktree = Path::new(run["worktree_path"].as_str().unwrap());
    assert_eq!(
        git(worktree, &["rev-parse", "HEAD"]),
        run["result_commit"].as_str().unwrap()
    );
    assert_eq!(
        dagq(env, &["integrate", "--next"])["outcome"],
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
    let landed = dagq(env, &["integrate", &second]);
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
        let detail = dagq(env, &["show", task, "--full"]);
        assert_eq!(detail["task"]["status"], "completed", "{task}");
        assert!(
            !Path::new(detail["runs"][0]["worktree_path"].as_str().unwrap()).exists(),
            "{task}"
        );
    }
    assert_eq!(
        git(repo, &["for-each-ref", "refs/dagq/runs/"])
            .lines()
            .count(),
        3
    );
}

/// The supervisor is killed while the stub worker is running (the task 15
/// incident: a binary update or a `kill` took the resident supervisor with
/// it). The wrapper keeps heartbeating in its cmux workspace and the stub
/// writes its receipt regardless. The next `supervise --once` adopts the
/// run from the dead supervisor's stale lease (ADR-0012), sends `/exit`
/// once, validates it, and `integrate` lands it: nothing is redone.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn killed_supervisor_run_is_adopted_by_the_next_supervisor_and_lands() {
    let fixture = fixture();
    let Fixture {
        cmux, repo, env, ..
    } = &fixture;
    let task_id = add_ready_task(env, "e2e adopted task", &[]);
    let mut guard = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };

    // A resident supervisor starts the run; it is killed once the worker runs.
    let mut victim = ChildGuard(
        Command::new(BIN)
            .current_dir(&fixture.repo)
            .env("XDG_DATA_HOME", &fixture.env.data_home)
            .args(["supervise", "--parallel", "1"])
            .arg("--cmux")
            .arg(&fixture.cmux)
            .arg("--claude")
            .arg(&fixture.stub)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let victim_stderr = reader(victim.0.stderr.take().unwrap());
    let victim_pid = victim.0.id();
    let started = Instant::now();
    let run = loop {
        assert!(
            victim.0.try_wait().unwrap().is_none(),
            "the supervisor exited before the worker started"
        );
        assert!(
            started.elapsed() < SUPERVISE_TIMEOUT,
            "the worker did not start within {SUPERVISE_TIMEOUT:?}"
        );
        let detail = dagq(env, &["show", &task_id, "--full"]);
        if let Some(run) = detail["runs"].as_array().unwrap().last()
            && run["status"] == "running"
        {
            break run.clone();
        }
        thread::sleep(Duration::from_millis(200));
    };
    let run_id = run["id"].as_str().unwrap().to_owned();
    let workspace = run["workspace_id"].as_str().unwrap().to_owned();
    guard.ids.push(workspace.clone());
    eprintln!(
        "worker of run {run_id} started after {:?}; killing supervisor {victim_pid}",
        started.elapsed()
    );
    victim.0.kill().unwrap();
    victim.0.wait().unwrap();
    eprintln!(
        "killed supervisor stderr:\n{}",
        victim_stderr.join().unwrap()
    );
    assert!(!pid_alive(victim_pid));

    // What the maintainer sees before anyone adopts: the registration and
    // the lease are stale by pid, the wrapper is alive, and the run keeps going.
    let status = dagq(env, &["status"]);
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 1, "{status}");
    assert_eq!(supervisors[0]["pid"], victim_pid);
    assert_eq!(supervisors[0]["alive"], false);
    assert_eq!(supervisors[0]["stale"], true);
    assert_eq!(
        supervisors[0]["run_ids"],
        Value::Array(vec![Value::String(run_id.clone())])
    );
    assert_eq!(status["runs"][0]["run_id"], run_id.as_str());
    assert_eq!(status["runs"][0]["lease"]["pid"], victim_pid);
    assert_eq!(status["runs"][0]["lease"]["alive"], false);
    let doctor = dagq(env, &["doctor", "--full"]);
    assert_eq!(doctor["runs"][0]["recoverable"], false, "{doctor}");
    let wrapper = doctor["runs"][0]["processes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["role"] == "wrapper")
        .unwrap()
        .clone();
    assert_eq!(wrapper["alive"], true, "{doctor}");
    assert!(workspace_listed(cmux, &workspace));

    // The next supervisor adopts the run instead of leaving it to recover.
    let pass = supervise_once(&fixture, &["--parallel", "1"], &[&task_id], &mut guard);
    let outcome = &pass.outcome;
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 1, "{outcome}");
    assert_eq!(outcome["runs"][0]["id"], run_id.as_str());
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert!(
        pass.stderr
            .contains(&format!("run {run_id} adopted from supervisor")),
        "{}",
        pass.stderr
    );
    assert_eq!(pass.workspaces, vec![(task_id.clone(), workspace.clone())]);

    let detail = dagq(env, &["show", &task_id, "--full"]);
    assert_eq!(detail["runs"].as_array().unwrap().len(), 1); // Not rerun.
    let run = &detail["runs"][0];
    assert_eq!(run["status"], "awaiting_integration");
    assert_eq!(run["workspace_id"], workspace.as_str());
    assert!(run["last_error"].is_null(), "{run}");
    assert!(run["workspace_closed_at"].is_number(), "{run}");
    assert!(!workspace_listed(cmux, &workspace));
    let events = detail["events"].as_array().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    let count = |kind: &str| kinds.iter().filter(|k| **k == kind).count();
    assert_eq!(count("run_adopted"), 1, "{kinds:?}");
    assert_eq!(count("exit_requested"), 1, "{kinds:?}");
    assert_eq!(count("session_exited"), 1, "{kinds:?}");
    assert_eq!(count("validation_finished"), 1, "{kinds:?}");
    assert!(!kinds.contains(&"run_recovered"), "{kinds:?}");
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    let adopted = events.iter().find(|e| e["kind"] == "run_adopted").unwrap();
    assert_eq!(adopted["payload"]["previous_pid"], victim_pid);
    assert_eq!(adopted["payload"]["wrapper"]["pid"], wrapper["pid"]);
    assert_eq!(adopted["payload"]["wrapper"]["alive"], true);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("agent_started") < position("run_adopted"));
    assert!(position("run_adopted") < position("exit_requested"));
    assert!(position("exit_requested") < position("session_exited"));
    // The adopter deregistered on exit; the killed one's row stays for `up` to prune.
    let status = dagq(env, &["status"]);
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 1, "{status}");
    assert_eq!(supervisors[0]["pid"], victim_pid);
    assert_eq!(supervisors[0]["run_ids"], Value::Array(vec![]));
    assert_eq!(status["runs"], Value::Array(vec![]));

    let integrated = dagq(env, &["integrate", &task_id]);
    assert_eq!(integrated["outcome"], "integrated", "{integrated}");
    assert_eq!(integrated["task"]["status"], "completed");
    assert_eq!(
        fs::read_to_string(repo.join("e2e.txt")).unwrap(),
        format!("written by the stub agent for {run_id}\n")
    );
    assert_eq!(
        git(
            repo,
            &["rev-list", "--count", &format!("{}..main", fixture.base)]
        ),
        "1"
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
    let located = dagq_with(env, &[("HOME", home.as_path())], &["locate"]);
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
        panic!("dagq up: {}", String::from_utf8_lossy(&output.stderr));
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
        format!("dagq {repo_name} maintainer")
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
        log.contains(&format!(
            "started: version {VERSION}, pid {pid}, parallel 2"
        )),
        "{log}"
    );

    let status = dagq(env, &["status"]);
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 1, "{status}");
    assert_eq!(supervisors[0]["pid"], pid);
    assert_eq!(supervisors[0]["registered"], true);
    assert_eq!(supervisors[0]["alive"], true);
    assert_eq!(supervisors[0]["stale"], false);
    assert_eq!(supervisors[0]["parallel"], 2);
    // The supervisor recorded the build it runs, which is what the next
    // `up` compares itself against (ADR-0014).
    assert_eq!(supervisors[0]["binary_version"], VERSION, "{status}");
    assert_eq!(status["runs"], Value::Array(vec![]));

    // Idempotent: nothing is started or opened twice.
    let second = dagq_with(env, &[("HOME", home.as_path())], &up_args);
    assert_eq!(second["supervisor"]["outcome"], "reused", "{second}");
    assert_eq!(second["supervisor"]["version"], VERSION, "{second}");
    assert_eq!(second["supervisor"]["pid"], pid);
    assert_eq!(second["maintainer"]["outcome"], "reused", "{second}");
    assert_eq!(second["maintainer"]["workspace_id"], maintainer.as_str());
    assert_eq!(second["pruned_supervisors"], Value::Array(vec![]));

    let started = Instant::now();
    let down = dagq_with(env, &[("HOME", home.as_path())], &["down", "--wait"]);
    eprintln!("down --wait took {:?}: {down}", started.elapsed());
    assert_eq!(down["outcome"], "stopped", "{down}");
    assert_eq!(down["pid"], pid);
    assert_eq!(down["launch_agent_unloaded"], true);
    assert!(!pid_alive(pid), "supervisor {pid} is still alive");
    assert!(!agent.loaded(), "the agent is still loaded");
    assert!(!plist.exists(), "the plist was not removed");
    let status = dagq(env, &["status"]);
    assert_eq!(status["supervisors"], Value::Array(vec![]), "{status}");
    let log = fs::read_to_string(&logs[0]).unwrap();
    assert!(
        log.contains("exiting: {\"errors\":[],\"outcome\":\"stopped\""),
        "{log}"
    );
    // The maintainer workspace is left open by `down`; the guard closes it.
    assert!(workspace_listed(cmux, &maintainer));
    let again = dagq_with(env, &[("HOME", home.as_path())], &["down"]);
    assert_eq!(again["outcome"], "not_running", "{again}");
}

/// `up --in-cmux` needs no socket password: the supervisor runs inside a
/// cmux workspace of its own, so it is a child of a cmux terminal like any
/// other client. Nothing about launchd is touched, `status` reports the
/// mode, and `down --wait` interrupts the supervisor and closes the
/// workspace once it has drained.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn up_in_cmux_starts_a_supervisor_in_a_workspace_that_down_wait_stops_and_closes() {
    let fixture = fixture();
    let Fixture {
        cmux,
        repo,
        stub,
        env,
        ..
    } = &fixture;
    // A disposable HOME, so a stray plist could only land there; none should.
    let home = fixture._dir.path().join("home");
    fs::create_dir(&home).unwrap();
    let located = dagq_with(env, &[("HOME", home.as_path())], &["locate"]);
    let plist = PathBuf::from(located["launch_agent"].as_str().unwrap());
    let label = located["label"].as_str().unwrap().to_owned();
    let log_dir = PathBuf::from(located["log_dir"].as_str().unwrap());
    let mut workspaces = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };

    let up_args = [
        "up",
        "--in-cmux",
        "--parallel",
        "2",
        "--cmux",
        cmux.to_str().unwrap(),
        "--claude",
        stub.to_str().unwrap(),
    ];
    let started = Instant::now();
    let first = dagq_with(env, &[("HOME", home.as_path())], &up_args);
    eprintln!("up --in-cmux took {:?}: {first}", started.elapsed());
    assert_eq!(first["supervisor"]["outcome"], "started", "{first}");
    assert_eq!(first["supervisor"]["mode"], "in_cmux");
    assert_eq!(first["supervisor"]["plist"], Value::Null);
    let repo_name = repo.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        first["supervisor"]["name"],
        format!("dagq {repo_name} supervisor")
    );
    let supervisor_workspace = first["supervisor"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    uuid::Uuid::parse_str(&supervisor_workspace).expect("workspace id is a UUID");
    workspaces.ids.push(supervisor_workspace.clone());
    assert!(workspace_listed(cmux, &supervisor_workspace));
    let pid = u32::try_from(first["supervisor"]["pid"].as_u64().unwrap()).unwrap();
    assert!(pid_alive(pid));
    let maintainer = first["maintainer"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    workspaces.ids.push(maintainer.clone());
    assert_eq!(first["maintainer"]["outcome"], "created", "{first}");

    // launchd knows nothing about this queue, and no plist was written.
    assert!(!plist.exists(), "{} exists", plist.display());
    assert!(
        !Command::new("launchctl")
            .args(["print", &format!("gui/{}/{label}", uid())])
            .output()
            .unwrap()
            .status
            .success(),
        "launchd has an agent for {label}"
    );
    // The supervisor in the workspace writes to the queue's log directory,
    // exactly as the launchd-run one does: `--log-dir` is on its command.
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
    assert_eq!(logs.len(), 1, "{logs:?} in {}", log_dir.display());
    assert!(fs::read_to_string(&logs[0]).unwrap().contains(&format!(
        "started: version {VERSION}, pid {pid}, parallel 2"
    )));

    let status = dagq(env, &["status"]);
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 1, "{status}");
    assert_eq!(supervisors[0]["pid"], pid);
    assert_eq!(supervisors[0]["mode"], "in_cmux");
    assert_eq!(
        supervisors[0]["workspace_id"],
        supervisor_workspace.as_str()
    );
    assert_eq!(supervisors[0]["stale"], false);
    let doctor = dagq(env, &["doctor", "--full"]);
    assert_eq!(doctor["supervisors"][0]["mode"], "in_cmux", "{doctor}");
    assert_eq!(
        doctor["supervisors"][0]["binary_version"], VERSION,
        "{doctor}"
    );

    // Idempotent: the live supervisor is of this binary's own version, so
    // it is reused with its mode and workspace and nothing is replaced.
    let second = dagq_with(env, &[("HOME", home.as_path())], &up_args);
    assert_eq!(second["supervisor"]["outcome"], "reused", "{second}");
    assert_eq!(second["supervisor"]["version"], VERSION, "{second}");
    assert_eq!(second["supervisor"]["mode"], "in_cmux");
    assert_eq!(
        second["supervisor"]["workspace_id"],
        supervisor_workspace.as_str()
    );
    assert_eq!(second["maintainer"]["workspace_id"], maintainer.as_str());

    let started = Instant::now();
    let down = dagq_with(env, &[("HOME", home.as_path())], &["down", "--wait"]);
    eprintln!("down --wait took {:?}: {down}", started.elapsed());
    assert_eq!(down["outcome"], "stopped", "{down}");
    assert_eq!(down["pid"], pid);
    assert_eq!(down["launch_agent_unloaded"], false);
    assert_eq!(
        down["supervisor_workspaces"],
        serde_json::json!([{"workspace_id": supervisor_workspace, "outcome": "closed"}]),
        "{down}"
    );
    assert!(!pid_alive(pid), "supervisor {pid} is still alive");
    wait_until_not_listed(cmux, &supervisor_workspace);
    assert_eq!(dagq(env, &["status"])["supervisors"], Value::Array(vec![]));
    // The maintainer workspace is left open by `down`; the guard closes it.
    assert!(workspace_listed(cmux, &maintainer));
}
