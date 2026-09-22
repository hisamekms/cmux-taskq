//! End-to-end happy path through the real binary, real Git, and real cmux.
//! Claude is replaced by a stub script that does what the prompt asks: change,
//! commit, write the receipt. Requires a running cmux, so it is ignored by default:
//! `cargo test --locked --test e2e -- --ignored --nocapture`.
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
printf 'written by the stub agent\n' > e2e.txt
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
    let output = Command::new(BIN)
        .current_dir(&env.repo)
        .env("XDG_DATA_HOME", &env.data_home)
        .args(args)
        .output()
        .unwrap();
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

/// Closes the workspace the supervisor created if it is still open when the
/// test ends, on success and on panic alike. On the happy path the supervisor
/// has already closed it; cmux 0.64 also closes a workspace by itself once its
/// command exits. Either way "not listed" is the expected state, not a failure.
struct WorkspaceGuard {
    cmux: PathBuf,
    id: Option<String>,
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        let Some(id) = &self.id else {
            return;
        };
        if !workspace_listed(&self.cmux, id) {
            eprintln!("workspace {id} already closed");
            return;
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

#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn happy_path_runs_a_stub_agent_through_cmux_and_awaits_integration() {
    let cmux = cmux_executable();
    let cmux_version = preflight(&cmux);
    eprintln!("cmux: {cmux_version}");

    // Disposable repository, queue and stub, all outside this repository.
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
    assert_eq!(init["schema_version"], 4);
    let db = PathBuf::from(init["db"].as_str().unwrap());
    assert!(db.starts_with(env.data_home.join("cmux-taskq")));
    assert_eq!(taskq(&env, &["locate"])["db_exists"], true);
    let task = taskq(
        &env,
        &[
            "add",
            "e2e stub task",
            "--description",
            "Add e2e.txt to the worktree",
            "--acceptance",
            "e2e.txt is committed and seed.txt still exists",
            "--verify",
            "test -f seed.txt",
            "--verify",
            "test -f e2e.txt",
        ],
    );
    let task_id = task["id"].to_string();
    assert_eq!(taskq(&env, &["ready", &task_id])["status"], "ready");
    assert_eq!(taskq(&env, &["candidates"]).as_array().unwrap().len(), 1);

    let started = Instant::now();
    let mut child = ChildGuard(
        Command::new(BIN)
            .current_dir(&repo)
            .env("XDG_DATA_HOME", &env.data_home)
            .arg("supervise")
            .arg("--cmux")
            .arg(&cmux)
            .arg("--claude")
            .arg(&stub)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = reader(child.0.stdout.take().unwrap());
    let stderr = reader(child.0.stderr.take().unwrap());

    // Watch the run while the supervisor is busy: the workspace id must appear
    // in the queue and in cmux's own list before the session ends.
    let mut guard = WorkspaceGuard {
        cmux: cmux.clone(),
        id: None,
    };
    let mut workspace_seen_at = None;
    let mut listed = false;
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < SUPERVISE_TIMEOUT,
            "supervise did not finish within {SUPERVISE_TIMEOUT:?}"
        );
        if guard.id.is_none() {
            let detail = taskq(&env, &["show", &task_id]);
            if let Some(id) = detail["runs"][0]["workspace_id"].as_str() {
                uuid::Uuid::parse_str(id).expect("workspace id is a UUID");
                guard.id = Some(id.to_owned());
                workspace_seen_at = Some(started.elapsed());
            }
        }
        if let Some(id) = &guard.id {
            listed |= workspace_listed(&cmux, id);
        }
        thread::sleep(Duration::from_millis(200));
    };
    let supervise_took = started.elapsed();
    let stdout = stdout.join().unwrap();
    let stderr = stderr.join().unwrap();
    eprintln!(
        "workspace registered after {workspace_seen_at:?}; supervise finished in {supervise_took:?}\n{stderr}"
    );
    assert!(status.success(), "supervise failed ({status}): {stderr}");
    let workspace = guard.id.clone().expect("workspace id was recorded");
    assert!(
        listed,
        "workspace {workspace} never appeared in cmux workspace list"
    );

    let outcome: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(outcome["outcome"], "finished", "{outcome}");
    assert_eq!(
        outcome["run"]["status"], "awaiting_integration",
        "{outcome}"
    );

    let detail = taskq(&env, &["show", &task_id]);
    assert_eq!(detail["task"]["status"], "in_progress");
    let runs = detail["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    let run_id = run["id"].as_str().unwrap();
    assert_eq!(run["status"], "awaiting_integration");
    assert_eq!(run["workspace_id"], workspace.as_str());
    assert_eq!(run["base_commit"], base.as_str());
    assert!(run["last_error"].is_null());
    assert_eq!(run["branch"], format!("taskq/{run_id}"));
    assert!(run["workspace_closed_at"].is_number(), "{run}");
    assert!(
        !workspace_listed(&cmux, &workspace),
        "workspace {workspace} is still open after the run was accepted"
    );

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
    assert_eq!(git(&repo, &["rev-parse", "main"]), base); // Not merged by the supervisor.
    assert_ne!(head, base);
    assert_eq!(run["result_commit"], head.as_str());
    assert_eq!(
        git(worktree, &["symbolic-ref", "HEAD"]),
        format!("refs/heads/taskq/{run_id}")
    );
    assert_eq!(git(worktree, &["status", "--porcelain"]), "");
    assert_eq!(
        fs::read_to_string(worktree.join("e2e.txt")).unwrap(),
        "written by the stub agent\n"
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

    // The slot stays taken until integration; the lease is gone.
    assert_eq!(taskq(&env, &["candidates"]).as_array().unwrap().len(), 0);
    assert!(taskq(&env, &["status"])["supervisor"].is_null());

    // Integration is manual: nothing happens until main contains the commit.
    let not_yet = taskq(&env, &["integrate", &task_id]);
    assert_eq!(not_yet["outcome"], "not_integrated", "{not_yet}");
    assert_eq!(not_yet["main"], base.as_str());
    assert_eq!(
        taskq(&env, &["show", &task_id])["task"]["status"],
        "in_progress"
    );
    git(&repo, &["merge", "--ff-only", &format!("taskq/{run_id}")]);
    assert_eq!(git(&repo, &["rev-parse", "main"]), head);
    let integrated = taskq(&env, &["integrate", &task_id]);
    assert_eq!(integrated["outcome"], "integrated", "{integrated}");
    assert_eq!(integrated["task"]["status"], "completed");
    assert_eq!(integrated["run"]["status"], "integrated");
    let detail = taskq(&env, &["show", &task_id]);
    assert_eq!(detail["task"]["status"], "completed");
    assert_eq!(detail["runs"][0]["status"], "integrated");
    assert!(
        detail["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "run_integrated")
    );
    assert!(worktree.exists()); // Kept until the operator removes it.
}
