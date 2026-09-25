//! End-to-end happy paths through the real binary, real Git, real cmux and
//! real launchd, from `add` to the squash landing by `integrate`, from
//! `up` to `down`, and from a killed supervisor to the adoption of its run.
//! Claude is replaced by a stub script that does what the prompt asks:
//! change, commit, write the receipt; resumed with `--resume` it reads the
//! supervisor's resolution request from its terminal and resolves the
//! conflict. Requires a running cmux, so it is ignored by
//! default: `cargo test --locked --test e2e -- --ignored --nocapture`.
//!
//! The launchd `up` / `down` test is temporarily off even under `--ignored`:
//! no project runs the launchd mode now, and without a cmux socket password
//! its preflight always stops `up`. It returns at once, printing why, unless
//! `DAGQ_E2E_LAUNCHD=1` is set; the in-cmux `up` / `down` test always runs.
use serde_json::{Value, json};
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
session_id= debug_file= add_dir= settings= prompt= resume= headless= tools= denied=
while [ $# -gt 0 ]; do
  case "$1" in
    -p) headless=1; shift ;;
    --allowedTools) tools=$2; shift 2 ;;
    --disallowedTools) denied=$2; shift 2 ;;
    --session-id) session_id=$2; shift 2 ;;
    --resume) resume=$2; shift 2 ;;
    --debug-file) debug_file=$2; shift 2 ;;
    --add-dir) add_dir=$2; shift 2 ;;
    --settings) settings=$2; shift 2 ;;
    --) shift; prompt=$1; shift; break ;;
    *) printf 'stub: unexpected argument %s\n' "$1" >&2; exit 64 ;;
  esac
done
[ $# -eq 0 ] || { printf 'stub: trailing arguments after the prompt\n' >&2; exit 64; }
if [ -n "$headless" ]; then
  # The supervisor's headless review (ADR-0027): read review.md, print the
  # verdict JSON on stdout. Only a task that says E2E-REVIEW-PASS passes;
  # any other review fails, and its run waits for a review by hand.
  [ -z "$session_id" ] && [ -n "$debug_file" ] && [ -n "$add_dir" ] && [ -n "$settings" ] && [ -n "$prompt" ] \
    && [ "$tools" = "Read,Grep,Glob" ] && [ "$denied" = "Bash,Edit,Write,NotebookEdit" ] || { printf 'stub: bad review arguments\n' >&2; exit 64; }
  ! grep -q '"Stop"' "$settings" || { printf 'stub: review settings would write the idle marker\n' >&2; exit 64; }
  review=$(printf '%s\n' "$prompt" | sed -n 's/^Read the review material at \(.*\): the task.*/\1/p')
  [ -f "$review" ] || { printf 'stub: no review material at %s\n' "$review" >&2; exit 65; }
  printf 'argv: -p --debug-file %s --add-dir %s --settings %s\nreview: %s\n' "$debug_file" "$add_dir" "$settings" "$review" > "$debug_file"
  if grep -q 'E2E-REVIEW-PASS' "$review"; then
    printf '{"verdict":"pass","reasons":[],"summary":"the stub reviewer found e2e.txt committed"}\n'
    exit 0
  fi
  printf 'stub: no verdict for this task\n' >&2
  exit 3
fi
if [ -n "$resume" ]; then
  # A resumed needs_session run: wait for the supervisor's resolution
  # request on the terminal, rebase onto the main it names, resolve the
  # conflict, rewrite the receipt, go idle and wait for /exit. The request
  # is one long line, longer than a canonical-mode tty line may be.
  [ -z "$session_id" ] && [ -z "$prompt" ] && [ -n "$debug_file" ] && [ -n "$add_dir" ] && [ -n "$settings" ] \
    || { printf 'stub: bad resume arguments\n' >&2; exit 64; }
  grep -q '"Stop"' "$settings" || { printf 'stub: settings lack a Stop hook\n' >&2; exit 64; }
  printf 'argv: --resume %s --debug-file %s --add-dir %s --settings %s\n' "$resume" "$debug_file" "$add_dir" "$settings" > "$debug_file"
  printf 'resumed %s; waiting for the resolution request\n' "$resume"
  # Claude Code's input box: the supervisor types the request only once it
  # is drawn (task 285).
  rule=──────────────────────────────────────────────────
  printf '%s\n\342\235\257 \n%s\n  ? for shortcuts\n' "$rule" "$rule"
  stty -icanon min 1
  IFS= read -r request
  stty icanon
  printf '%s\n' "$request" > "$add_dir/resume-request-seen.txt"
  main=$(printf '%s\n' "$request" | sed -n 's/.*main is now \([0-9a-f]*\) .*/\1/p')
  [ -n "$main" ] || { printf 'stub: the request names no main\n' >&2; exit 65; }
  if ! git rebase -q "$main" >/dev/null 2>&1; then
    printf 'resolved by the session\n' > e2e.txt
    git add e2e.txt
    GIT_EDITOR=true git rebase --continue >/dev/null
  fi
  sh -c 'test -f seed.txt'
  commit=$(git rev-parse HEAD)
  receipt="$add_dir/receipt.json"
  printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"passed","evidence_or_reason":"test -f seed.txt exited 0 after the rebase"},"e2e":{"status":"not_applicable","evidence_or_reason":"stub agent"},"subagent_review":{"status":"not_applicable","evidence_or_reason":"stub agent"},"summary":"resolved e2e.txt"}\n' \
    "$resume" "$commit" > "$receipt.tmp"
  mv "$receipt.tmp" "$receipt"
  printf 'receipt rewritten for %s\n' "$commit"
  sleep 1
  idle="$add_dir/idle.json"
  printf '{"hook_event_name":"Stop","session_id":"%s","stop_hook_active":false}\n' "$resume" > "$idle.tmp"
  mv "$idle.tmp" "$idle"
  printf 'idle; waiting for /exit\n'
  while read -r line; do
    [ "$line" = "/exit" ] && break
  done
  printf 'bye\n'
  exit 0
fi
[ -n "$session_id" ] && [ -n "$debug_file" ] && [ -n "$add_dir" ] && [ -n "$settings" ] && [ -n "$prompt" ] \
  || { printf 'stub: missing arguments\n' >&2; exit 64; }
grep -q '"Stop"' "$settings" || { printf 'stub: settings lack a Stop hook\n' >&2; exit 64; }
{
  printf 'argv: --session-id %s --debug-file %s --add-dir %s --settings %s\n' "$session_id" "$debug_file" "$add_dir" "$settings"
  printf 'cwd: %s\n' "$(pwd)"
  printf 'env: DAGQ_ROLE=%s DAGQ_QUEUE=%s\n' "${DAGQ_ROLE:-}" "${DAGQ_QUEUE:-}"
  printf 'run env: E2E_SHARED=%s E2E_RUN_DIR=%s\n' "${E2E_SHARED:-}" "${E2E_RUN_DIR:-}"
} > "$debug_file"
run_id=$(printf '%s\n' "$prompt" | sed -n 's/^You are executing dagq task [0-9]*, run \(.*\)\.$/\1/p')
[ "$run_id" = "$session_id" ] || { printf 'stub: prompt run %s != session %s\n' "$run_id" "$session_id" >&2; exit 65; }
receipt=$(printf '%s\n' "$prompt" | sed -n 's/^Write a completion receipt to \(.*\) using a temporary file in the same directory.*/\1/p')
[ -n "$receipt" ] || { printf 'stub: prompt does not name the receipt path\n' >&2; exit 65; }
case "$prompt" in
  *E2E-ASK*)
    # A question: register it as a worker_question ask, go idle, and wait
    # for the supervisor to type the answer into this terminal.
    "$add_dir/runner" --db "$DAGQ_QUEUE" ask --run "$session_id" --kind worker_question \
      --question 'Which word goes into answer.txt?' > "$add_dir/ask.json"
    # The new ask notified a person through the real cmux.
    grep -Eq '"notified": *true' "$add_dir/ask.json" || { printf 'stub: ask did not notify\n' >&2; exit 66; }
    idle="$add_dir/idle.json"
    printf '{"hook_event_name":"Stop","session_id":"%s","stop_hook_active":false}\n' "$session_id" > "$idle.tmp"
    mv "$idle.tmp" "$idle"
    printf 'asked; waiting for the answer\n'
    answer=
    while IFS= read -r line; do
      case "$line" in 'answer to ask '*) answer=$line; break ;; esac
    done
    [ -n "$answer" ] || { printf 'stub: no answer arrived\n' >&2; exit 66; }
    printf '%s\n' "$answer" > answer.txt
    git add answer.txt
    ;;
esac
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

const E2E_DAGQ_TOML: &str =
    "[run.env]\nE2E_SHARED = '${DAGQ_QUEUE_DIR}/shared'\nE2E_RUN_DIR = '${DAGQ_RUN_DIR}'\n";

/// A verification command that records the `[run.env]` it ran with in the
/// run directory it names, and fails without it.
const VERIFY_RUN_ENV: &str =
    r#"printf 'verify env: %s\n' "$E2E_SHARED" >> "${E2E_RUN_DIR:?}/verify-env.txt""#;

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

/// Record in `home`'s Claude Code config that the folder trust dialog was
/// accepted at `repo`, as `up` requires before it starts anything.
fn trust_repository(home: &Path, repo: &Path) {
    let root = repo.canonicalize().unwrap();
    fs::write(
        home.join(".claude.json"),
        json!({"projects": {root.to_str().unwrap(): {"hasTrustDialogAccepted": true}}}).to_string(),
    )
    .unwrap();
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
        .env_remove("CLAUDE_CONFIG_DIR")
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
            // cmux refuses to close a pinned workspace (the inbox and the
            // planner are pinned, ADR-0031).
            let _ = Command::new(&self.cmux)
                .args(["workspace-action", "--action", "unpin", "--workspace", id])
                .output();
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

/// The workspace is pinned, has the sidebar color `color` (cmux lists a
/// named color by its hex value) and carries the status pill `pill` as
/// `cmux list-status` prints it.
fn assert_look(cmux: &Path, id: &str, color: &str, pill: &str) {
    let listed = listed_workspace(cmux, id).unwrap();
    assert_eq!(listed["pinned"], true, "{listed}");
    assert_eq!(listed["custom_color"], color, "{listed}");
    let status = Command::new(cmux)
        .args(["list-status", "--workspace", id])
        .output()
        .unwrap();
    assert!(status.status.success(), "{status:?}");
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(status.lines().any(|line| line == pill), "{status}");
}

/// The queue's workspace group in `cmux --json workspace-group list`,
/// found by its external ID (the queue hash).
fn listed_group(cmux: &Path, external_id: &str) -> Option<Value> {
    let output = Command::new(cmux)
        .args(["--json", "--id-format", "uuids", "workspace-group", "list"])
        .output()
        .unwrap();
    assert!(output.status.success(), "cmux workspace-group list failed");
    let list: Value = serde_json::from_slice(&output.stdout).unwrap();
    list["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["external_id"] == external_id)
        .cloned()
}

/// `cmux workspace env <id> --json`: the environment the workspace was
/// created with.
fn workspace_env(cmux: &Path, id: &str) -> Value {
    let output = Command::new(cmux)
        .args(["workspace", "env", id, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "cmux workspace env {id}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["env"].clone()
}

/// Deletes the queue's workspace group and closes what is left in it (the
/// anchor cmux generated with it) when the test ends.
struct GroupGuard {
    cmux: PathBuf,
    external_id: String,
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        let Some(group) = listed_group(&self.cmux, &self.external_id) else {
            return;
        };
        let id = group["id"].as_str().unwrap_or_default().to_owned();
        match Command::new(&self.cmux)
            .args(["workspace-group", "delete", &id, "--close-workspaces"])
            .output()
        {
            Ok(output) if output.status.success() => eprintln!("deleted workspace group {id}"),
            Ok(output) => eprintln!(
                "deleting workspace group {id} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) => eprintln!("deleting workspace group {id} failed: {error}"),
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

/// The file in a fixture's temporary directory that its test process holds an
/// exclusive `flock` on for as long as the fixture lives. The lock goes away
/// with the process however it ends, SIGKILL included, so a directory whose
/// owner file can be locked by someone else belongs to a dead test.
const OWNER_FILE: &str = "e2e-owner";
/// A fixture directory without an owner file, left by an e2e from before the
/// owner file existed, is swept only once it is this old.
const UNMARKED_SWEEP_AGE: Duration = Duration::from_secs(60 * 60);

/// Take ownership of a fresh fixture directory: lock the owner file under a
/// temporary name and rename it into place, so a concurrent sweep never sees
/// an owner file that is not yet locked.
fn claim_fixture_dir(dir: &Path) -> fs::File {
    let staging = dir.join(format!("{OWNER_FILE}.tmp"));
    let mut file = fs::File::create(&staging).unwrap();
    file.lock().unwrap();
    use std::io::Write;
    writeln!(file, "{}", std::process::id()).unwrap();
    fs::rename(&staging, dir.join(OWNER_FILE)).unwrap();
    file
}

/// Clean up what earlier e2e tests left behind when their process died before
/// its guards and `TempDir` could drop (SIGTERM, SIGKILL): the processes
/// running from or on their temporary directory, the cmux workspaces whose
/// `DAGQ_QUEUE` or `E2E_SHARED` points into it, the workspace groups named
/// after its queue hashes, and the directory itself. A directory counts as
/// left behind only when its owner lock is free (the owner process is gone),
/// or, without an owner file, when it has the fixture's shape and is older
/// than [`UNMARKED_SWEEP_AGE`]. The sweep holds that lock while it works, so
/// concurrent sweeps never take the same directory. Nothing outside those
/// directories is touched: the production queue's workspaces carry no such
/// env and its group has another external ID.
fn sweep_abandoned_fixtures(cmux: &Path) {
    let Ok(entries) = fs::read_dir(env::temp_dir()) else {
        return;
    };
    let mut abandoned = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !entry.file_name().to_string_lossy().starts_with(".tmp") {
            continue;
        }
        if let Some(lock) = claim_abandoned(&dir) {
            abandoned.push((dir, lock));
        }
    }
    if abandoned.is_empty() {
        return;
    }
    let prefixes: Vec<(PathBuf, Vec<PathBuf>)> = abandoned
        .iter()
        .map(|(dir, _)| {
            let mut forms = vec![dir.clone()];
            if let Ok(real) = dir.canonicalize()
                && real != *dir
            {
                forms.push(real);
            }
            (dir.clone(), forms)
        })
        .collect();
    let inside = |value: &str| {
        prefixes
            .iter()
            .flat_map(|(_, forms)| forms)
            .any(|form| Path::new(value).starts_with(form))
    };
    for (dir, _) in &abandoned {
        eprintln!("e2e sweep: {} was left by a dead e2e", dir.display());
    }
    kill_processes_inside(&inside);
    for (dir, _) in &abandoned {
        for hash in queue_hashes(dir) {
            delete_group(cmux, &hash);
        }
    }
    close_workspaces_inside(cmux, &inside);
    for (dir, lock) in abandoned {
        match fs::remove_dir_all(&dir) {
            Ok(()) => eprintln!("e2e sweep: removed {}", dir.display()),
            Err(error) => eprintln!("e2e sweep: removing {} failed: {error}", dir.display()),
        }
        drop(lock);
    }
}

/// The locked owner file of `dir` when `dir` is a fixture directory whose
/// owner is gone, `None` when it is live, not a fixture, or taken by another
/// sweep.
fn claim_abandoned(dir: &Path) -> Option<fs::File> {
    let owner = dir.join(OWNER_FILE);
    let file = if owner.exists() {
        fs::File::open(&owner).ok()?
    } else {
        // An e2e from before the owner file: recognize the fixture by its
        // stub agent and queue data home, and wait until it is old.
        if !dir.join("claude-stub").is_file() || !dir.join("data").is_dir() {
            return None;
        }
        let age = fs::metadata(dir).ok()?.modified().ok()?.elapsed().ok()?;
        if age < UNMARKED_SWEEP_AGE {
            return None;
        }
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&owner)
            .ok()?
    };
    file.try_lock().ok()?;
    Some(file)
}

/// The queue hashes under the fixture's data home, which are the external
/// IDs of the queues' workspace groups.
fn queue_hashes(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir.join("data").join("dagq")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

/// Terminate the processes an abandoned directory started: those whose
/// program is in it (the runner copy), the stub agent (`/bin/sh
/// <dir>/claude-stub`), and a `dagq` binary given its queue or stub
/// (`--db`, `--claude`), like the temporary queue's supervisor. `ps` joins
/// argv with spaces, so only these shapes are matched: a word of some other
/// process's arguments, like a Claude prompt that quotes such a path, never
/// makes it a target.
fn kill_processes_inside(inside: &dyn Fn(&str) -> bool) {
    let Ok(output) = Command::new("ps")
        .args(["-axww", "-o", "pid=,command="])
        .output()
    else {
        eprintln!("e2e sweep: ps failed; left processes alone");
        return;
    };
    let me = std::process::id();
    let mut victims = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut words = line.split_whitespace();
        let Some(pid) = words.next().and_then(|pid| pid.parse::<u32>().ok()) else {
            continue;
        };
        let argv: Vec<&str> = words.collect();
        let program = argv.first().is_some_and(|word| inside(word));
        let stub = argv.first() == Some(&"/bin/sh")
            && argv
                .get(1)
                .is_some_and(|word| word.ends_with("/claude-stub") && inside(word));
        let dagq = argv
            .first()
            .is_some_and(|word| Path::new(word).file_name() == Some("dagq".as_ref()))
            && argv
                .windows(2)
                .any(|pair| matches!(pair[0], "--db" | "--claude") && inside(pair[1]));
        if pid != me && (program || stub || dagq) {
            eprintln!("e2e sweep: terminating process {pid}: {}", argv.join(" "));
            victims.push(pid);
        }
    }
    for pid in &victims {
        // SAFETY: kill has no memory preconditions.
        unsafe { libc::kill(*pid as libc::pid_t, libc::SIGTERM) };
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while victims.iter().any(|pid| pid_alive(*pid)) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    for pid in victims.iter().filter(|pid| pid_alive(**pid)) {
        eprintln!("e2e sweep: killing process {pid}, still alive after SIGTERM");
        // SAFETY: as above.
        unsafe { libc::kill(*pid as libc::pid_t, libc::SIGKILL) };
    }
}

fn delete_group(cmux: &Path, external_id: &str) {
    let Some(group) = try_listed_group(cmux, external_id) else {
        return;
    };
    let id = group["id"].as_str().unwrap_or_default().to_owned();
    let name = group["name"].as_str().unwrap_or_default().to_owned();
    match Command::new(cmux)
        .args(["workspace-group", "delete", &id, "--close-workspaces"])
        .output()
    {
        Ok(output) if output.status.success() => {
            eprintln!("e2e sweep: deleted workspace group {id} {name} (queue {external_id})")
        }
        Ok(output) => eprintln!(
            "e2e sweep: deleting workspace group {id} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => eprintln!("e2e sweep: deleting workspace group {id} failed: {error}"),
    }
}

/// Close each listed workspace whose `DAGQ_QUEUE` or `E2E_SHARED` points
/// into an abandoned directory. A workspace without those variables, like
/// every workspace of the production queue other than its runs, is skipped.
fn close_workspaces_inside(cmux: &Path, inside: &dyn Fn(&str) -> bool) {
    let Some(list) = cmux_json(
        cmux,
        &["--json", "--id-format", "uuids", "workspace", "list"],
    ) else {
        eprintln!("e2e sweep: cmux workspace list failed; left workspaces alone");
        return;
    };
    for workspace in list["workspaces"].as_array().into_iter().flatten() {
        let Some(id) = workspace["id"].as_str() else {
            continue;
        };
        let Some(env) = cmux_json(cmux, &["workspace", "env", id, "--json"]) else {
            continue;
        };
        let points_inside = ["DAGQ_QUEUE", "E2E_SHARED"]
            .iter()
            .any(|key| env["env"][key].as_str().is_some_and(inside));
        if !points_inside {
            continue;
        }
        let title = workspace["title"].as_str().unwrap_or_default();
        match Command::new(cmux).args(["workspace", "close", id]).output() {
            Ok(output) if output.status.success() => {
                eprintln!("e2e sweep: closed workspace {id} {title}")
            }
            Ok(output) => eprintln!(
                "e2e sweep: closing workspace {id} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) => eprintln!("e2e sweep: closing workspace {id} failed: {error}"),
        }
    }
}

fn try_listed_group(cmux: &Path, external_id: &str) -> Option<Value> {
    let list = cmux_json(
        cmux,
        &["--json", "--id-format", "uuids", "workspace-group", "list"],
    )?;
    list["groups"]
        .as_array()?
        .iter()
        .find(|group| group["external_id"] == external_id)
        .cloned()
}

fn cmux_json(cmux: &Path, args: &[&str]) -> Option<Value> {
    let output = Command::new(cmux).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// Disposable repository, queue and stub agent, all outside this repository.
struct Fixture {
    group: GroupGuard,
    _dir: tempfile::TempDir,
    cmux: PathBuf,
    repo: PathBuf,
    stub: PathBuf,
    base: String,
    db: PathBuf,
    env: Env,
    /// Held for the fixture's lifetime, and dropped after `_dir` is gone;
    /// see [`OWNER_FILE`].
    _owner: fs::File,
}

fn fixture() -> Fixture {
    let cmux = cmux_executable();
    let cmux_version = preflight(&cmux);
    eprintln!("cmux: {cmux_version}");
    sweep_abandoned_fixtures(&cmux);
    let dir = tempfile::tempdir().unwrap();
    let owner = claim_fixture_dir(dir.path());
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.name", "e2e"]);
    git(&repo, &["config", "user.email", "e2e@example.invalid"]);
    fs::write(repo.join("seed.txt"), "fixture\n").unwrap();
    // ADR-0023 decision 3: every run gets this env in its workspace and
    // its verification commands.
    fs::write(repo.join("dagq.toml"), E2E_DAGQ_TOML).unwrap();
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
        // A repository queue's directory is named after the queue hash,
        // which is the external ID of its workspace group.
        group: GroupGuard {
            cmux: cmux.clone(),
            external_id: db
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned(),
        },
        _dir: dir,
        cmux,
        repo,
        stub,
        base,
        db,
        env,
        _owner: owner,
    }
}

impl Fixture {
    fn group(&self) -> Option<Value> {
        listed_group(&self.cmux, &self.group.external_id)
    }
}

/// Register a ready task whose acceptance the stub agent satisfies.
fn add_ready_task(env: &Env, title: &str, dependencies: &[&str]) -> String {
    add_ready_task_verifying(env, title, dependencies, &[])
}

fn add_ready_task_verifying(
    env: &Env,
    title: &str,
    dependencies: &[&str],
    verify: &[&str],
) -> String {
    add_ready_task_described(
        env,
        title,
        "Add e2e.txt to the worktree",
        dependencies,
        verify,
    )
}

fn add_ready_task_described(
    env: &Env,
    title: &str,
    description: &str,
    dependencies: &[&str],
    verify: &[&str],
) -> String {
    let mut args = vec![
        "add",
        title,
        "--description",
        description,
        "--acceptance",
        "e2e.txt is committed and seed.txt still exists",
        "--verify",
        "test -f seed.txt",
        "--verify",
        "test -f e2e.txt",
    ];
    for command in verify {
        args.extend(["--verify", command]);
    }
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
    // The stub reviewer passes a task that says E2E-REVIEW-PASS.
    let task_id = add_ready_task_described(
        env,
        "e2e stub task",
        "Add e2e.txt to the worktree. E2E-REVIEW-PASS",
        &[],
        &[VERIFY_RUN_ENV],
    );
    assert_eq!(dagq(env, &["candidates"]).as_array().unwrap().len(), 1);
    // The supervisor pushes what it lands to the repository's bare origin.
    let origin = repo.parent().unwrap().join("origin.git");
    git(
        repo.parent().unwrap(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    git(repo, &["remote", "add", "origin", origin.to_str().unwrap()]);

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
    // Accepted, reviewed and landed by the supervisor alone (ADR-0027).
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let stderr = &pass.stderr;
    let base = base.as_str();
    let repo = repo.as_path();
    let db = db.as_path();

    let detail = dagq(env, &["show", &task_id, "--full"]);
    assert_eq!(detail["task"]["status"], "completed");
    let runs = detail["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    let run_id = run["id"].as_str().unwrap();
    assert_eq!(run["status"], "integrated");
    assert_eq!(run["workspace_id"], workspace.as_str());
    assert_eq!(run["base_commit"], base);
    assert!(run["last_error"].is_null());
    // ADR-0018: the name carries the repository, task and title; the run
    // ID lives in the description.
    let listing = &pass.listings[0];
    assert_eq!(
        listing["custom_title"],
        format!(
            "[{}]worker#{task_id} - e2e stub task",
            repo.file_name().unwrap().to_string_lossy()
        ),
        "{listing}"
    );
    assert_eq!(
        listing["description"],
        format!(
            "dagq role=worker queue={} run={run_id} task={task_id}",
            fixture.group.external_id
        ),
        "{listing}"
    );
    assert_eq!(run["branch"], format!("dagq/{run_id}"));
    assert!(run["workspace_closed_at"].is_number(), "{run}");
    assert!(
        !workspace_listed(cmux, &workspace),
        "workspace {workspace} is still open after the run landed"
    );
    assert!(stderr.contains("review 1: pass"), "{stderr}");
    assert!(stderr.contains("integrated"), "{stderr}");

    // The run lives next to the queue.
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
    // The run's own history is kept under its ref; the landing is one
    // squash commit on main with its tree, pushed to origin.
    let head = git(repo, &["rev-parse", &format!("refs/dagq/runs/{run_id}")]);
    assert_ne!(head, base);
    let main = git(repo, &["rev-parse", "main"]);
    assert_ne!(main, head);
    assert_eq!(run["result_commit"], main.as_str());
    assert_eq!(git(&origin, &["rev-parse", "main"]), main);
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
    assert_eq!(
        fs::read_to_string(repo.join("e2e.txt")).unwrap(),
        format!("written by the stub agent for {run_id}\n")
    );
    assert!(!worktree.exists(), "landed worktree was not removed");
    assert_eq!(
        git(repo, &["branch", "--list", &format!("dagq/{run_id}")]),
        ""
    );

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
    // The run workspace's `--env` reached the agent through its shell.
    assert!(
        log.contains(&format!(
            "env: DAGQ_ROLE=worker DAGQ_QUEUE={}",
            fixture.db.canonicalize().unwrap().display()
        )),
        "{log}"
    );
    // dagq.toml's [run.env], expanded, reached the agent's shell; the
    // verification commands get it at the landing, the only place they run.
    let shared = db.canonicalize().unwrap().with_file_name("shared");
    assert!(
        log.contains(&format!(
            "run env: E2E_SHARED={} E2E_RUN_DIR={}",
            shared.display(),
            run_dir.display()
        )),
        "{log}"
    );
    assert_eq!(
        fs::read_to_string(run_dir.join("verify-env.txt")).unwrap(),
        format!("verify env: {}\n", shared.display())
    );
    // The headless review ran `claude -p` with the run directory's hook-less
    // settings and read review.md there.
    let review_log = fs::read_to_string(run_dir.join("claude-review.log")).unwrap();
    assert!(
        review_log.contains(&format!(
            "argv: -p --debug-file {run_dir}/claude-review.log --add-dir {run_dir} --settings {run_dir}/claude-review-settings.json",
            run_dir = run_dir.display()
        )),
        "{review_log}"
    );
    assert!(
        review_log.contains(&format!("review: {}/review.md", run_dir.display())),
        "{review_log}"
    );
    // The run workspace joined the queue's group, made by its external ID.
    assert!(
        fixture.group().is_some(),
        "no workspace group for the queue"
    );

    let events = detail["events"].as_array().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    let position = |kind: &str| {
        kinds
            .iter()
            .position(|k| *k == kind)
            .unwrap_or_else(|| panic!("missing {kind} in {kinds:?}"))
    };
    // The session stays open through validation and the review; /exit is
    // sent on the passing verdict, and the landing follows the close.
    let order = [
        "lease_acquired",
        "worktree_created",
        "workspace_created",
        "wrapper_started",
        "agent_started",
        "receipt_observed",
        "session_idle_observed",
        "supervision_finished",
        "validation_finished",
        "review_started",
        "review_finished",
        "exit_requested",
        "session_exited",
        "workspace_closed",
        "integration_started",
        "integration_rebased",
        "run_integrated",
        "worktree_removed",
        "push_finished",
    ];
    for pair in order.windows(2) {
        assert!(
            position(pair[0]) < position(pair[1]),
            "{} before {}: {kinds:?}",
            pair[0],
            pair[1]
        );
    }
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
        event("supervision_finished")["payload"],
        serde_json::json!({"status": "validating", "exit_code": null, "session_live": true})
    );
    assert_eq!(
        event("review_started")["payload"]["session_live"],
        true,
        "the session must be alive during the review"
    );
    let review = &event("review_finished")["payload"];
    assert_eq!(review["verdict"], "pass");
    assert_eq!(review["attempt"], 1);
    assert_eq!(
        review["summary"],
        "the stub reviewer found e2e.txt committed"
    );
    assert_eq!(
        event("exit_requested")["payload"]["workspace_id"],
        workspace.as_str()
    );
    assert!(!kinds.contains(&"exit_request_timed_out"), "{kinds:?}");
    assert!(!kinds.contains(&"integration_approved"), "{kinds:?}");
    assert_eq!(event("session_exited")["payload"]["exit_code"], 0);
    let finished = event("validation_finished");
    assert_eq!(finished["payload"]["status"], "awaiting_integration");
    assert_eq!(finished["payload"]["result_commit"], head.as_str());
    assert_eq!(finished["payload"]["receipt"]["summary"], "added e2e.txt");
    // The verification commands ran once, after the rebase, with the run env.
    let verifications: Vec<&Value> = events
        .iter()
        .filter(|e| e["kind"] == "verification_command")
        .collect();
    assert_eq!(verifications.len(), 3);
    assert!(
        verifications
            .iter()
            .all(|e| e["payload"]["exit_code"] == 0 && e["payload"]["phase"] == "integration")
    );
    assert!(!kinds.contains(&"cleanup_failed"), "{kinds:?}");

    let processes = detail["processes"].as_array().unwrap();
    assert_eq!(processes.len(), 2);
    assert!(processes.iter().all(|p| p["exit_code"] == 0));

    assert_eq!(dagq(env, &["candidates"]).as_array().unwrap().len(), 0);
    let status = dagq(env, &["status"]);
    assert_eq!(status["supervisors"], Value::Array(vec![]), "{status}");
    assert_eq!(status["runs"], Value::Array(vec![]), "{status}");
    // Only the stopped `--once` supervisor is left; no run waits for anyone.
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["run_id"].is_null()),
        "{status}"
    );
    assert_eq!(
        dagq(env, &["integrate", "--next"])["outcome"],
        "no_run_awaiting"
    );
}

/// The stub worker asks a `worker_question` (its task says `E2E-ASK`) and
/// goes idle; once the ask is answered the supervisor types the answer into
/// the worker's cmux terminal, closes the ask, and the worker commits it.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn a_worker_question_is_answered_through_the_worker_terminal() {
    let fixture = fixture();
    let Fixture { cmux, env, .. } = &fixture;
    let task_id = dagq(
        env,
        &[
            "add",
            "e2e asking task",
            "--description",
            "E2E-ASK: ask which word goes into answer.txt, then add e2e.txt",
            "--acceptance",
            "answer.txt holds the answer and e2e.txt is committed",
            "--verify",
            "test -f answer.txt",
            "--verify",
            "test -f e2e.txt",
        ],
    )["id"]
        .to_string();
    assert_eq!(dagq(env, &["ready", &task_id])["status"], "ready");
    let mut guard = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };
    // Answers the ask as the inbox would, once the worker registered it.
    let answerer = {
        let env = Env {
            repo: env.repo.clone(),
            data_home: env.data_home.clone(),
        };
        thread::spawn(move || {
            let started = Instant::now();
            loop {
                assert!(
                    started.elapsed() < SUPERVISE_TIMEOUT,
                    "the worker asked nothing"
                );
                let asks = dagq(&env, &["asks", "--open"]);
                if let Some(ask) = asks["asks"].as_array().unwrap().first() {
                    assert_eq!(ask["kind"], "worker_question", "{ask}");
                    assert_eq!(ask["asked_by"], "worker", "{ask}");
                    let id = ask["id"].to_string();
                    dagq(&env, &["answer", &id, "--text", "blue"]);
                    return ask["id"].as_i64().unwrap();
                }
                thread::sleep(Duration::from_millis(200));
            }
        })
    };
    let pass = supervise_once(&fixture, &[], &[&task_id], &mut guard);
    let ask_id = answerer.join().unwrap();
    assert_eq!(
        pass.outcome["errors"],
        Value::Array(vec![]),
        "{}",
        pass.outcome
    );
    assert_eq!(
        pass.outcome["runs"][0]["status"], "awaiting_integration",
        "{}",
        pass.outcome
    );
    let detail = dagq(env, &["show", &task_id, "--full"]);
    let run = &detail["runs"][0];
    let worktree = Path::new(run["worktree_path"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(worktree.join("answer.txt")).unwrap(),
        format!("answer to ask {ask_id}: blue\n")
    );
    let delivered: Vec<&Value> = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "ask_delivered")
        .collect();
    assert_eq!(delivered.len(), 1, "{detail}");
    assert_eq!(delivered[0]["payload"]["ask_id"], ask_id);
    let asks = dagq(env, &["asks", "--all"]);
    assert!(asks["asks"][0]["closed_at"].is_number(), "{asks}");
    assert!(dagq(env, &["asks"])["asks"].as_array().unwrap().is_empty());
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

    // The integrate call approved it, so the next supervisor pass resumes
    // its session (the stub plays Claude), which resolves the conflict on
    // top of main and rewrites the receipt; the runtime then lands it.
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
    let status = dagq(env, &["status"]);
    let attention = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_id"] == run["id"])
        .cloned()
        .unwrap();
    assert_eq!(attention["next"], "resuming (runtime)", "{status}");
    let pass = supervise_once(&fixture, &["--parallel", "2"], &[], &mut guard);
    let outcome = &pass.outcome;
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    assert!(
        pass.stderr.contains("resolution request sent"),
        "{}",
        pass.stderr
    );
    let detail = dagq(env, &["show", &second, "--full"]);
    let run = &detail["runs"][0];
    assert_eq!(run["status"], "integrated", "{detail}");
    let run_dir = Path::new(run["run_dir"].as_str().unwrap());
    let seen = fs::read_to_string(run_dir.join("resume-request-seen.txt")).unwrap();
    assert!(
        seen.contains(&format!("main is now {third_landed} ")),
        "{seen}"
    );
    assert!(seen.contains("task 1: e2e first"), "{seen}");
    assert!(seen.contains("task 3: e2e dependent"), "{seen}");
    let kinds: Vec<&str> = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    for kind in [
        "integration_approved",
        "resume_started",
        "resume_finished",
        "run_integrated",
    ] {
        assert!(kinds.contains(&kind), "{kind} missing: {kinds:?}");
    }
    let finished = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "resume_finished")
        .unwrap();
    assert_eq!(finished["payload"]["outcome"], "resolved", "{finished}");
    assert_eq!(finished["payload"]["workspace_closed"], true, "{finished}");
    let resume_workspace = finished["payload"]["workspace_id"].as_str().unwrap();
    wait_until_not_listed(cmux, resume_workspace);
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

    // What the inbox sees before anyone adopts: the registration and
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
    // cmux accepted the close; its list can lag behind it for a moment.
    wait_until_not_listed(cmux, &workspace);
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
    assert!(
        position("agent_started") < position("run_adopted"),
        "{kinds:?}"
    );
    assert!(
        position("run_adopted") < position("exit_requested"),
        "{kinds:?}"
    );
    // `exit_requested` is recorded before `/exit` is sent, so a session that
    // exits before the send returns still lands after it.
    assert!(
        position("exit_requested") < position("session_exited"),
        "{kinds:?}"
    );
    // The adopter deregistered on exit; the killed one's row stays for `up` to prune.
    let status = dagq(env, &["status"]);
    let supervisors = status["supervisors"].as_array().unwrap();
    let diagnosis = format!(
        "victim pid {victim_pid}; supervisors {supervisors:#?}; adopter stderr:\n{}",
        pass.stderr
    );
    assert_eq!(supervisors.len(), 1, "{diagnosis}");
    assert_eq!(supervisors[0]["pid"], victim_pid, "{diagnosis}");
    assert_eq!(
        supervisors[0]["run_ids"],
        Value::Array(vec![]),
        "{diagnosis}"
    );
    assert_eq!(status["runs"], Value::Array(vec![]));

    let integrated = dagq(env, &["integrate", &task_id]);
    assert_eq!(integrated["outcome"], "integrated", "{integrated}");
    assert_eq!(integrated["task"]["status"], "completed");
    // No origin in this repository: the push is skipped and the landing stands.
    assert_eq!(integrated["push"]["outcome"], "skipped", "{integrated}");
    assert_eq!(
        integrated["push"]["reason"],
        "the repository has no remote origin"
    );
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

/// The records of a JSON Lines log; each line must be one JSON object.
fn log_records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect()
}

fn log_messages(path: &Path) -> Vec<String> {
    log_records(path)
        .iter()
        .map(|record| record["message"].as_str().unwrap().to_owned())
        .collect()
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Whether the launchd `up` / `down` e2e runs. It is temporarily off unless
/// `DAGQ_E2E_LAUNCHD=1`: no project runs the launchd mode now, and without a
/// cmux socket password `up`'s preflight always stops it, which would fail
/// every `--ignored` run. To bring it back, drop this check.
fn launchd_e2e_enabled() -> bool {
    if env::var("DAGQ_E2E_LAUNCHD").as_deref() == Ok("1") {
        return true;
    }
    eprintln!(
        "skipping the launchd up/down e2e: the launchd mode is temporarily out of the \
         default e2e because no project runs it now and, without a cmux socket password, \
         its preflight always stops `up`; set DAGQ_E2E_LAUNCHD=1 to run it"
    );
    false
}

/// `up` bootstraps the supervisor as a LaunchAgent of the real launchd and
/// opens the inbox and planner workspaces in the real cmux; `status` lists the
/// supervisor through its registration; `down --wait` unloads the agent
/// and returns once the supervisor has drained and deregistered.
///
/// Runs only with `DAGQ_E2E_LAUNCHD=1`; see [`launchd_e2e_enabled`].
#[test]
#[ignore = "needs a running cmux and launchd; run with --ignored and DAGQ_E2E_LAUNCHD=1"]
fn up_starts_a_launchd_supervisor_that_status_lists_and_down_wait_stops_it() {
    if !launchd_e2e_enabled() {
        return;
    }
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
    trust_repository(&home, &env.repo);
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
            .env_remove("CLAUDE_CONFIG_DIR")
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
    let repo_name = repo.file_name().unwrap().to_str().unwrap();
    // The inbox and the planner are the sessions `up` opens (ADR-0024
    // decision 6).
    let sessions: Vec<String> = ["inbox", "planner"]
        .into_iter()
        .map(|key| {
            assert_eq!(first[key]["outcome"], "created", "{first}");
            assert_eq!(first[key]["name"], format!("[{repo_name}]{key}"));
            let id = first[key]["workspace_id"].as_str().unwrap().to_owned();
            uuid::Uuid::parse_str(&id).expect("workspace id is a UUID");
            workspaces.ids.push(id.clone());
            assert!(workspace_listed(cmux, &id));
            id
        })
        .collect();
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
                .ends_with(&format!("-{pid}.jsonl"))
        })
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?}");
    let log = log_messages(&logs[0]);
    assert!(
        log.iter().any(|m| m.contains(&format!(
            "started: version {VERSION}, pid {pid}, parallel 2"
        ))),
        "{log:?}"
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
    for (key, id) in ["inbox", "planner"].into_iter().zip(&sessions) {
        assert_eq!(second[key]["outcome"], "reused", "{second}");
        assert_eq!(second[key]["workspace_id"], id.as_str());
    }
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
    let log = log_messages(&logs[0]);
    assert!(
        log.iter()
            .any(|m| m.contains("exiting: {\"errors\":[],\"outcome\":\"stopped\"")),
        "{log:?}"
    );
    // The inbox and planner workspaces are left open by `down`; the guard
    // closes them.
    for id in &sessions {
        assert!(workspace_listed(cmux, id));
    }
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
    trust_repository(&home, &env.repo);
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
        format!("[{repo_name}]supervisor")
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
    let [inbox, planner] = ["inbox", "planner"].map(|key| {
        assert_eq!(first[key]["outcome"], "created", "{first}");
        let id = first[key]["workspace_id"].as_str().unwrap().to_owned();
        workspaces.ids.push(id.clone());
        id
    });

    // Every workspace carries its role and the queue in its own
    // environment, and all joined the queue's group (ADR-0026).
    let db = fixture.db.canonicalize().unwrap();
    for (id, role) in [
        (&supervisor_workspace, "supervisor"),
        (&inbox, "inbox"),
        (&planner, "planner"),
    ] {
        let env = workspace_env(cmux, id);
        assert_eq!(env["DAGQ_ROLE"], role, "{env}");
        assert_eq!(env["DAGQ_QUEUE"], db.to_str().unwrap(), "{env}");
    }
    let group = fixture.group().expect("the queue's workspace group exists");
    assert_eq!(group["name"], format!("[{repo_name}]"), "{group}");
    let members: Vec<String> = group["member_workspace_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_ascii_lowercase())
        .collect();
    for id in [&supervisor_workspace, &inbox, &planner] {
        assert!(members.contains(&id.to_ascii_lowercase()), "{group}");
    }
    assert_eq!(
        listed_workspace(cmux, &inbox).unwrap()["description"],
        format!("dagq role=inbox queue={}", fixture.group.external_id)
    );
    // The inbox is Amber and the planner Blue, each pinned with its role's
    // pill (ADR-0031).
    for (id, color, pill) in [
        (&inbox, "#7D6608", "dagq_role=inbox icon=tray"),
        (&planner, "#1565C0", "dagq_role=planner icon=map"),
    ] {
        assert_look(cmux, id, color, pill);
    }
    assert_eq!(
        listed_workspace(cmux, &supervisor_workspace).unwrap()["pinned"],
        false
    );
    // A person renames the inbox workspace; `up` still knows it.
    let rename = Command::new(cmux)
        .args(["workspace", "rename", &inbox, "--title", "renamed by hand"])
        .output()
        .unwrap();
    assert!(rename.status.success(), "{rename:?}");

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
    // The supervisor in the workspace writes its JSON Lines log to the
    // queue's log directory, exactly as the launchd-run one does
    // (ADR-0033): the file outlives the workspace.
    let logs: Vec<PathBuf> = fs::read_dir(&log_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_str().unwrap();
            name.starts_with("supervise-") && name.ends_with(&format!("-{pid}.jsonl"))
        })
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?} in {}", log_dir.display());
    let supervisor_log = logs[0].clone();
    assert!(
        log_messages(&supervisor_log)
            .iter()
            .any(|m| m.contains(&format!(
                "started: version {VERSION}, pid {pid}, parallel 2"
            )))
    );

    // The in-cmux supervisor lands a task, and its integrate progress and
    // the failed push of main (origin does not exist) are records in that
    // file, not only lines on the workspace's screen.
    let task_id = add_ready_task_described(
        env,
        "logged landing",
        "Add e2e.txt to the worktree. E2E-REVIEW-PASS",
        &[],
        &[],
    );
    let missing_origin = repo.parent().unwrap().join("missing-origin.git");
    git(
        repo,
        &["remote", "add", "origin", missing_origin.to_str().unwrap()],
    );
    let deadline = Instant::now() + Duration::from_secs(180);
    let run_id = loop {
        let detail = dagq(env, &["show", &task_id, "--full"]);
        if let Some(id) = detail["runs"][0]["workspace_id"].as_str()
            && !workspaces.ids.iter().any(|known| known == id)
        {
            workspaces.ids.push(id.to_owned());
        }
        if detail["task"]["status"] == "completed" {
            break detail["runs"][0]["id"].as_str().unwrap().to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "task {task_id} did not land: {detail}\n{:?}",
            log_messages(&supervisor_log)
        );
        thread::sleep(Duration::from_millis(500));
    };
    // main is pushed after the task completes; wait for its record.
    let pushed = format!("run {run_id}: push of main failed");
    let records = loop {
        let records = log_records(&supervisor_log);
        if records
            .iter()
            .any(|r| r["message"].as_str().unwrap().contains(&pushed))
        {
            break records;
        }
        assert!(Instant::now() < deadline, "no push record: {records:#?}");
        thread::sleep(Duration::from_millis(200));
    };
    let find = |needle: &str| {
        records
            .iter()
            .find(|r| r["message"].as_str().unwrap().contains(needle))
            .unwrap_or_else(|| panic!("no record with {needle:?} in {records:#?}"))
            .clone()
    };
    let integrating = find(&format!(
        "run {run_id} integrating task {task_id} onto main"
    ));
    assert_eq!(integrating["level"], "INFO");
    assert_eq!(integrating["target"], "dagq::application::integrate");
    assert_eq!(integrating["fields"]["op"], "integrate");
    assert_eq!(integrating["fields"]["run_id"], run_id.as_str());
    assert_eq!(integrating["fields"]["task_id"], task_id.as_str());
    assert_eq!(integrating["spans"][0]["name"], "integrate");
    let landed = find(&format!("task {task_id} landed as"));
    assert_eq!(landed["fields"]["run_id"], run_id.as_str());
    let push = find(&pushed);
    assert_eq!(push["level"], "WARN");
    assert_eq!(push["fields"]["op"], "push");
    assert!(push["fields"]["error"].as_str().is_some(), "{push}");
    eprintln!("in-cmux supervisor log {}:", supervisor_log.display());
    for record in [&integrating, &landed, &push] {
        eprintln!("{record}");
    }

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
    for (key, id) in [("inbox", &inbox), ("planner", &planner)] {
        assert_eq!(second[key]["outcome"], "reused", "{second}");
        assert_eq!(second[key]["workspace_id"], id.as_str());
    }
    // The look is put back on a reused workspace that lost it.
    let unpin = Command::new(cmux)
        .args([
            "workspace-action",
            "--action",
            "unpin",
            "--workspace",
            &planner,
        ])
        .output()
        .unwrap();
    assert!(unpin.status.success(), "{unpin:?}");
    let third = dagq_with(env, &[("HOME", home.as_path())], &up_args);
    assert_eq!(third["planner"]["outcome"], "reused", "{third}");
    assert_eq!(third["warnings"], serde_json::json!([]), "{third}");
    assert_look(cmux, &planner, "#1565C0", "dagq_role=planner icon=map");

    // cmux refuses to close a pinned workspace; dagq's close unpins it
    // first, so `down` closes the supervisor's workspace even when a person
    // pinned it.
    let pin = Command::new(cmux)
        .args([
            "workspace-action",
            "--action",
            "pin",
            "--workspace",
            &supervisor_workspace,
        ])
        .output()
        .unwrap();
    assert!(pin.status.success(), "{pin:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while listed_workspace(cmux, &supervisor_workspace).unwrap()["pinned"] != true {
        assert!(Instant::now() < deadline, "the pin never showed up");
        thread::sleep(Duration::from_millis(200));
    }
    let refused = Command::new(cmux)
        .args(["workspace", "close", &supervisor_workspace])
        .output()
        .unwrap();
    eprintln!("cmux workspace close of a pinned workspace: {refused:?}");
    assert!(!refused.status.success(), "{refused:?}");

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
    // The inbox and planner workspaces are left open by `down`; the guard
    // closes them.
    for id in [&inbox, &planner] {
        assert!(workspace_listed(cmux, id));
    }
}
