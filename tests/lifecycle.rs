//! `up` and `down` against fakes for launchd, cmux and process signals: the
//! idempotent start in either mode (launchd or `--in-cmux`), the
//! out-of-cmux connection preflight, the pruning of dead registrations, the
//! inbox workspace decisions, the planners `plan` opens, and every `down`
//! outcome. The real
//! launchd and cmux path is `tests/e2e.rs`.

mod common;

use common::Bounded;

use anyhow::{Result, bail};
use dagq::{
    VERSION,
    application::{
        AgentState, DetachedRefusal, LaunchAgent, ProcessControl, SupervisorEnvironment, TaskStore,
        WorkspaceBackend, WorkspaceTags,
    },
    domain::{NewTask, SessionRole, SupervisorMode, Task, TaskAction, TaskRun},
    infrastructure::{
        adapters::{Cmux, GitRepository, SOCKET_PASSWORD_ENV, detach, process_alive, shell_quote},
        location::QueueLocation,
        sqlite::SqliteQueue,
    },
    lifecycle::{
        self, DownOptions, INBOX_ROLE, PLANNER_ROLE, QUEUE_ENV, ROLE_ENV, UpEnvironment, UpOptions,
        inbox_command,
    },
    runtime::{inbox_prompt, planner_prompt},
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};
use tempfile::TempDir;

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

/// Disposable repository, queue, home and stub `claude`, all under one temp dir.
struct Fixture {
    _dir: TempDir,
    repo: PathBuf,
    location: QueueLocation,
    environment: UpEnvironment,
    options: UpOptions,
    /// Times the test while held (task 324).
    _test: common::Waiting,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("my repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    fs::write(repo.join("seed.txt"), "fixture\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "seed"]);
    let db = dir.path().join("queue's dir").join("queue.db");
    let home = dir.path().join("home");
    let location = QueueLocation::explicit_in(&db, &home);
    location.prepare().unwrap();
    SqliteQueue::init(&db).unwrap();
    let claude = dir.path().join("claude-stub");
    fs::write(&claude, "#!/bin/sh\nprintf 'claude-stub 0.0.0\\n'\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let cmux = dir.path().join("cmux-stub");
    fs::write(&cmux, "#!/bin/sh\nprintf 'PONG\\n'\n").unwrap();
    fs::set_permissions(&cmux, fs::Permissions::from_mode(0o755)).unwrap();
    // Claude Code has accepted the folder trust dialog at the repository root.
    let claude_config = dir.path().join("claude.json");
    let root = repo.canonicalize().unwrap();
    fs::write(
        &claude_config,
        json!({"projects": {root.to_str().unwrap(): {"hasTrustDialogAccepted": true}}}).to_string(),
    )
    .unwrap();
    Fixture {
        repo,
        location,
        _test: common::test(),
        environment: UpEnvironment {
            role: None,
            queue: None,
            path: "/usr/bin:/bin:/home/u/.local/bin".into(),
            socket_password: None,
            current_exe: "/opt/bin/dagq".into(),
            claude_config: Some(claude_config),
        },
        options: UpOptions {
            parallel: 2,
            in_cmux: false,
            no_wait: false,
            plugin_dir: Some(dir.path().to_path_buf()),
            cmux,
            claude,
            startup_timeout: Duration::from_secs(5),
            handoff_timeout: Duration::from_secs(5),
            poll: Duration::from_millis(20),
        },
        _dir: dir,
    }
}

/// Records installs and uninstalls; an install registers a supervisor with
/// this process's PID, the way a started `supervise` would within seconds.
struct FakeLaunchd {
    db: PathBuf,
    registers_on_install: bool,
    loaded: Mutex<bool>,
    /// The agent's process while loaded, as `launchctl print` would show it.
    agent_pid: Mutex<Option<u32>>,
    installs: Mutex<Vec<(String, PathBuf, String)>>,
    uninstalls: Mutex<Vec<(String, PathBuf)>>,
}

impl FakeLaunchd {
    fn new(db: &Path) -> Self {
        Self {
            db: db.into(),
            registers_on_install: true,
            loaded: Mutex::new(false),
            agent_pid: Mutex::new(None),
            installs: Mutex::new(Vec::new()),
            uninstalls: Mutex::new(Vec::new()),
        }
    }
    fn load(&self, pid: Option<u32>) {
        *self.loaded.lock().unwrap() = true;
        *self.agent_pid.lock().unwrap() = pid;
    }
}

impl LaunchAgent for FakeLaunchd {
    fn install(&self, label: &str, path: &Path, contents: &str) -> Result<()> {
        self.installs
            .lock()
            .unwrap()
            .push((label.into(), path.into(), contents.into()));
        self.load(Some(std::process::id()));
        if self.registers_on_install {
            SqliteQueue::open(&self.db)?.register_supervisor(
                &uuid::Uuid::new_v4().to_string(),
                std::process::id(),
                2,
                VERSION,
            )?;
        }
        Ok(())
    }
    fn uninstall(&self, label: &str, path: &Path) -> Result<AgentState> {
        self.uninstalls
            .lock()
            .unwrap()
            .push((label.into(), path.into()));
        let loaded = std::mem::replace(&mut *self.loaded.lock().unwrap(), false);
        let pid = self.agent_pid.lock().unwrap().take();
        Ok(AgentState {
            loaded,
            pid: if loaded { pid } else { None },
        })
    }
}

/// Liveness by an explicit dead set, which only the test writes: a signal
/// never moves a PID into it, the way a real signal does not make the
/// process disappear by the next syscall.
#[derive(Default)]
struct FakeProcesses {
    dead: Mutex<HashSet<u32>>,
    terminated: Mutex<Vec<u32>>,
    interrupted: Mutex<Vec<u32>>,
    killed: Mutex<Vec<u32>>,
}

impl ProcessControl for FakeProcesses {
    fn alive(&self, pid: u32) -> bool {
        !self.dead.lock().unwrap().contains(&pid)
    }
    fn terminate(&self, pid: u32) -> Result<()> {
        self.terminated.lock().unwrap().push(pid);
        Ok(())
    }
    fn interrupt(&self, pid: u32) -> Result<()> {
        self.interrupted.lock().unwrap().push(pid);
        Ok(())
    }
    // SIGKILL returns before the target is reaped, so `kill -0` still
    // succeeds right after it; the fake does not pretend otherwise.
    fn kill(&self, pid: u32) -> Result<()> {
        self.killed.lock().unwrap().push(pid);
        Ok(())
    }
}

/// Named workspaces only; the run-bound methods are the supervisor's and
/// must not be reached by `up`. Admits or refuses the detached connection
/// as configured and records the environment `up` proved it with.
#[derive(Default)]
struct FakeCmux {
    calls: AtomicUsize,
    workspaces: Mutex<Vec<(String, PathBuf, String, String)>>,
    refuses_detached: bool,
    /// The ping could not be run at all (not a refusal).
    detached_unreachable: bool,
    detached_preflights: Mutex<Vec<SupervisorEnvironment>>,
    closed: Mutex<Vec<String>>,
    /// Queue a `[…]supervisor` workspace registers a supervisor in,
    /// the way the `supervise` cmux runs in its terminal would.
    registers_supervisor_in: Option<PathBuf>,
    /// The tags each workspace was opened with, in `workspaces` order.
    tags: Mutex<Vec<WorkspaceTags>>,
    /// Every `ensure_group` call, as (external ID, name).
    groups: Mutex<Vec<(String, String)>>,
    /// `workspace-group create` fails.
    group_fails: bool,
    /// Every color, status pill and pin call, as (call, workspace, what).
    looks: Mutex<Vec<(String, String, String)>>,
    /// cmux refuses every color, status pill and pin call.
    look_fails: bool,
    /// `workspace create` fails.
    create_fails: bool,
    /// Workspaces created so far, so a UUID is never handed out twice.
    created: AtomicUsize,
}

impl FakeCmux {
    /// Rename a workspace the way a person would in cmux's sidebar.
    fn rename(&self, id: &str, title: &str) {
        for workspace in self.workspaces.lock().unwrap().iter_mut() {
            if workspace.2 == id {
                workspace.0 = title.into();
            }
        }
    }

    fn look(&self, call: &str, id: &str, what: String) -> Result<()> {
        self.looks
            .lock()
            .unwrap()
            .push((call.into(), id.into(), what));
        if self.look_fails {
            bail!("{call} refused")
        }
        Ok(())
    }

    /// The look calls made on `id`, in order.
    fn looks_of(&self, id: &str) -> Vec<(String, String)> {
        self.looks
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, workspace, _)| workspace == id)
            .map(|(call, _, what)| (call.clone(), what.clone()))
            .collect()
    }

    /// An open workspace put there directly, the way one opened by an
    /// earlier process is: nothing registers for it.
    fn open(&self, name: &str, cwd: &Path, id: &str) {
        self.workspaces.lock().unwrap().push((
            name.into(),
            cwd.into(),
            id.into(),
            "supervise".into(),
        ));
        self.tags.lock().unwrap().push(WorkspaceTags::default());
    }
}

impl WorkspaceBackend for FakeCmux {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn preflight_detached(&self, environment: &SupervisorEnvironment) -> Result<()> {
        self.detached_preflights
            .lock()
            .unwrap()
            .push(environment.clone());
        if self.refuses_detached {
            return Err(DetachedRefusal {
                reason: "\"cmux\" ping from outside cmux failed: only processes started inside cmux can connect".into(),
            }
            .into());
        }
        if self.detached_unreachable {
            bail!("\"/bin/sh\" did not finish within 60s")
        }
        Ok(())
    }
    fn create(&self, _: &Task, _: &TaskRun, _: &str, _: &WorkspaceTags) -> Result<String> {
        bail!("up does not create run workspaces")
    }
    fn create_resume(&self, _: &Task, _: &TaskRun, _: &str, _: &WorkspaceTags) -> Result<String> {
        bail!("up does not resume runs")
    }
    fn send_text(&self, _: &str, _: &str) -> Result<()> {
        bail!("up never types into a terminal")
    }
    fn send_enter(&self, _: &str) -> Result<()> {
        bail!("up never types into a terminal")
    }
    fn capture(&self, _: &str) -> Result<String> {
        bail!("not used")
    }
    fn close(&self, workspace_id: &str) -> Result<()> {
        self.closed.lock().unwrap().push(workspace_id.to_owned());
        let mut workspaces = self.workspaces.lock().unwrap();
        let Some(index) = workspaces
            .iter()
            .position(|(_, _, id, _)| id == workspace_id)
        else {
            bail!("no such workspace: {workspace_id}")
        };
        workspaces.remove(index);
        self.tags.lock().unwrap().remove(index);
        Ok(())
    }
    fn set_color(&self, workspace_id: &str, color: &str) -> Result<()> {
        self.look("set-color", workspace_id, color.into())
    }
    fn set_status(&self, workspace_id: &str, key: &str, value: &str, icon: &str) -> Result<()> {
        self.look("set-status", workspace_id, format!("{key}={value} {icon}"))
    }
    fn pin(&self, workspace_id: &str) -> Result<()> {
        self.look("pin", workspace_id, String::new())
    }
    fn send_exit(&self, _: &str) -> Result<()> {
        bail!("not used")
    }
    fn notify(&self, _: &str, _: &str, _: Option<&str>) -> Result<()> {
        bail!("up does not notify")
    }
    fn exists(&self, workspace_id: &str) -> Result<bool> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .workspaces
            .lock()
            .unwrap()
            .iter()
            .any(|(_, _, id, _)| id == workspace_id))
    }
    fn listed_workspace_ids(&self) -> Result<Vec<String>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .workspaces
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, id, _)| id.clone())
            .collect())
    }
    fn ensure_group(&self, external_id: &str, name: &str) -> Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.groups
            .lock()
            .unwrap()
            .push((external_id.into(), name.into()));
        if self.group_fails {
            bail!("workspace-group create failed")
        }
        Ok(format!("group-{external_id}"))
    }
    fn create_named(
        &self,
        name: &str,
        cwd: &Path,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.create_fails {
            bail!("workspace create failed")
        }
        let mut workspaces = self.workspaces.lock().unwrap();
        let id = format!(
            "01234567-89ab-4def-8123-{:012x}",
            self.created.fetch_add(1, Ordering::SeqCst)
        );
        workspaces.push((name.into(), cwd.into(), id.clone(), command.into()));
        self.tags.lock().unwrap().push(tags.clone());
        if let Some(db) = self.registers_supervisor_in.as_deref()
            && name.ends_with("]supervisor")
        {
            let mut queue = SqliteQueue::open(db)?;
            // A supervisor that was merely slow can start heartbeating
            // again while `up` waits; every registration that is already
            // there does so here, so the wait must tell them apart.
            for registration in queue.supervisors()? {
                queue.heartbeat(&registration.token)?;
            }
            queue.register_supervisor(
                &uuid::Uuid::new_v4().to_string(),
                std::process::id(),
                2,
                VERSION,
            )?;
        }
        Ok(id)
    }
}

fn up(
    fixture: &Fixture,
    cmux: &FakeCmux,
    launchd: &FakeLaunchd,
    processes: &FakeProcesses,
) -> Value {
    lifecycle::up(
        &fixture.location,
        &fixture.repo,
        cmux,
        launchd,
        processes,
        &fixture.environment,
        &fixture.options,
    )
    .unwrap()
}

fn try_up(
    fixture: &Fixture,
    cmux: &FakeCmux,
    launchd: &FakeLaunchd,
    processes: &FakeProcesses,
) -> Result<Value> {
    lifecycle::up(
        &fixture.location,
        &fixture.repo,
        cmux,
        launchd,
        processes,
        &fixture.environment,
        &fixture.options,
    )
}

/// Rewrite the version a registration recorded, standing in for a
/// supervisor of another build (`Some`) or one that registered before the
/// column existed (`None`).
fn set_binary_version(db: &Path, token: &str, version: Option<&str>) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE supervisors SET binary_version=?2 WHERE token=?1",
            rusqlite::params![token, version],
        )
        .unwrap();
}

/// Poll `condition` until it holds. On the deadline the supervisor's pid
/// is marked dead before the panic: `up`'s drain loop is otherwise
/// unbounded, so a helper thread that merely panicked would leave the main
/// thread waiting for a registration nothing is going to remove, and
/// `thread::scope` would never join.
fn wait_until(processes: &FakeProcesses, pid: u32, condition: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !condition() {
        if std::time::Instant::now() >= deadline {
            processes.dead.lock().unwrap().insert(pid);
            panic!("the condition never held; released the drain so the test can fail");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// A ready task claimed by `token`, so the queue has a run in flight.
fn claim_a_run(fixture: &Fixture, queue: &mut SqliteQueue, token: &str) -> String {
    let task = queue
        .add(NewTask {
            title: "in flight".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: vec![],
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
    let repository = GitRepository::inspect(&fixture.repo).unwrap();
    match queue
        .claim_for_supervisor(&repository.base_commit, token)
        .unwrap()
    {
        dagq::domain::ClaimOutcome::Claimed { run } => run.id().to_string(),
        outcome => panic!("expected a claim, got {outcome:?}"),
    }
}

/// The mode recorded on a registration, read straight from the queue.
fn remaining_mode(queue: &SqliteQueue, token: &str) -> Option<SupervisorMode> {
    queue
        .supervisors()
        .unwrap()
        .into_iter()
        .find(|registration| registration.token == token)
        .and_then(|registration| registration.mode)
}

fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// A program the `[run.env]` of `dagq.toml` names that the PATH of `up`
/// does not find stops `up` before it starts a supervisor (ADR-0049
/// decision 9); found, `up` reports where.
#[test]
fn up_refuses_a_run_env_program_its_path_does_not_find() {
    use std::os::unix::fs::PermissionsExt;
    let mut fixture = fixture();
    let bin = fixture.repo.parent().unwrap().join("tools");
    fs::create_dir(&bin).unwrap();
    fs::write(
        fixture.repo.join("dagq.toml"),
        "[run.env]\nRUSTC_WRAPPER = 'sccache'\n",
    )
    .unwrap();
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let error = format!(
        "{:#}",
        try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
    );
    assert!(
        error.contains("RUSTC_WRAPPER = \"sccache\"")
            && error.contains("PATH: /usr/bin:/bin")
            && error.contains("the supervisor was not started"),
        "{error}"
    );
    assert!(
        SqliteQueue::open(&fixture.location.db)
            .unwrap()
            .supervisors()
            .unwrap()
            .is_empty()
    );
    let tool = bin.join("sccache");
    fs::write(&tool, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    fixture.environment.path = format!("{}:/usr/bin:/bin", bin.display());
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(
        report["run_env"]["programs"][0]["resolved"],
        tool.to_str().unwrap(),
        "{report}"
    );
}

#[test]
fn up_starts_the_agent_and_the_sessions_once_and_reuses_them_after() {
    let fixture = fixture();
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let root = GitRepository::inspect(&fixture.repo).unwrap().root;

    let first = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(first["supervisor"]["outcome"], "started", "{first}");
    assert_eq!(first["supervisor"]["mode"], "launchd");
    assert_eq!(first["supervisor"]["workspace_id"], Value::Null);
    assert_eq!(first["supervisor"]["pid"], json!(std::process::id()));
    assert_eq!(
        first["supervisor"]["plist"],
        json!(fixture.location.launch_agent)
    );
    assert_eq!(
        first["supervisor"]["log_dir"],
        json!(fixture.location.log_dir)
    );
    // The one resident session is the inbox (ADR-0041 decision 6): `up`
    // opens no planner, and the report names no other session.
    let keys: Vec<&String> = first.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "doctor",
            "inbox",
            "migrated",
            "pruned_supervisors",
            "retired_sessions",
            "supervisor",
            "warnings"
        ]
    );
    assert_eq!(first["retired_sessions"], 0);
    assert_eq!(first["pruned_supervisors"], json!([]));
    assert_eq!(first["doctor"]["unfinished_runs"], json!([]));
    assert_eq!(first["doctor"]["awaiting_integration"], json!([]));
    assert_eq!(first["doctor"]["needs_session"], json!([]));

    // The agent definition launchd received.
    let installs = launchd.installs.lock().unwrap();
    assert_eq!(installs.len(), 1);
    let (label, path, contents) = &installs[0];
    assert_eq!(label, &fixture.location.label);
    assert!(label.starts_with("com.dagq."));
    assert_eq!(path, &fixture.location.launch_agent);
    assert_eq!(
        path,
        &fixture
            ._dir
            .path()
            .join("home/Library/LaunchAgents")
            .join(format!("{label}.plist"))
    );
    let db = fixture.location.db.canonicalize().unwrap();
    // The planners the runtime opens load the plugin `up` was given.
    let plugin_dir = fixture
        .options
        .plugin_dir
        .as_ref()
        .unwrap()
        .canonicalize()
        .unwrap();
    let string = |text: &str| format!("<string>{text}</string>");
    assert!(contents.contains(&format!("<key>Label</key>\n\t{}", string(label))));
    let arguments = [
        "/opt/bin/dagq",
        "--db",
        db.to_str().unwrap(),
        "supervise",
        "--parallel",
        "2",
        "--log-dir",
        fixture.location.log_dir.to_str().unwrap(),
        "--cmux",
        fixture.options.cmux.to_str().unwrap(),
        "--claude",
        fixture.options.claude.to_str().unwrap(),
        "--plugin-dir",
        plugin_dir.to_str().unwrap(),
    ]
    .iter()
    .map(|argument| format!("\t\t{}\n", string(argument)))
    .collect::<String>();
    assert!(
        contents.contains(&format!(
            "<key>ProgramArguments</key>\n\t<array>\n{arguments}\t</array>"
        )),
        "{contents}"
    );
    assert!(contents.contains(&format!(
        "<key>WorkingDirectory</key>\n\t{}",
        string(root.to_str().unwrap())
    )));
    assert!(contents.contains(
        "<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>PATH</key>\n\t\t<string>/usr/bin:/bin:/home/u/.local/bin</string>\n\t</dict>"
    ));
    // No password was exported, so none is stored, and the connection was
    // proved with exactly the environment the plist carries.
    assert!(!contents.contains(SOCKET_PASSWORD_ENV));
    assert_eq!(
        *cmux.detached_preflights.lock().unwrap(),
        vec![SupervisorEnvironment {
            path: "/usr/bin:/bin:/home/u/.local/bin".into(),
            socket_password: None,
        }]
    );
    assert!(contents.contains("<key>KeepAlive</key>\n\t<true/>"));
    assert!(contents.contains("<key>RunAtLoad</key>\n\t<true/>"));
    let launchd_log = fixture.location.log_dir.join("launchd.log");
    assert!(contents.contains(&format!(
        "<key>StandardOutPath</key>\n\t{}",
        string(launchd_log.to_str().unwrap())
    )));
    drop(installs);

    // Each session workspace: repository root as cwd, role and queue in
    // the workspace's own environment (not a prefix of the command), the
    // plugin directory and the prompt on the command, the queue's group, and
    // its UUID recorded in the queue.
    let hash = fixture.location.hash();
    assert_eq!(
        *cmux.groups.lock().unwrap(),
        vec![(hash.clone(), "[my repo]".to_owned())]
    );
    assert_eq!(first["warnings"], json!([]));
    let workspaces = cmux.workspaces.lock().unwrap();
    assert_eq!(workspaces.len(), 1);
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    assert_eq!(queue.session_workspace(SessionRole::Planner).unwrap(), None);
    let tags = cmux.tags.lock().unwrap();
    for (index, (key, role, opening)) in [("inbox", SessionRole::Inbox, "You are the inbox of")]
        .into_iter()
        .enumerate()
    {
        let (name, cwd, id, command) = &workspaces[index];
        assert_eq!(name, &format!("[my repo]{key}"));
        assert_eq!(cwd, &root);
        assert_eq!(
            first[key],
            json!({"outcome": "created", "workspace_id": id, "name": name})
        );
        assert!(
            command.starts_with(&format!("'{}'", fixture.options.claude.display())),
            "{command}"
        );
        assert!(command.contains("'--plugin-dir'"), "{command}");
        assert!(command.contains(opening), "{command}");
        assert!(!command.contains("DAGQ_"), "{command}");
        assert_eq!(
            tags[index],
            WorkspaceTags {
                env: vec![
                    ("DAGQ_ROLE".into(), key.into()),
                    ("DAGQ_QUEUE".into(), db.to_str().unwrap().into()),
                ],
                description: Some(format!("dagq role={key} queue={hash}")),
                group: Some(format!("group-{hash}")),
            }
        );
        assert_eq!(
            queue.session_workspace(role).unwrap().as_deref(),
            Some(id.as_str())
        );
    }
    drop(tags);
    drop(workspaces);

    // A person renames the inbox workspace; it is still the one.
    let inbox_id = first["inbox"]["workspace_id"].as_str().unwrap();
    cmux.rename(inbox_id, "my own title");
    let second = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(second["supervisor"]["outcome"], "reused", "{second}");
    assert_eq!(second["supervisor"]["mode"], "launchd");
    assert_eq!(second["supervisor"]["pid"], json!(std::process::id()));
    assert_eq!(second["inbox"]["outcome"], "reused", "{second}");
    assert_eq!(
        second["inbox"]["workspace_id"],
        first["inbox"]["workspace_id"]
    );
    assert_eq!(second["inbox"]["name"], first["inbox"]["name"]);
    assert_eq!(second["pruned_supervisors"], json!([]));
    assert_eq!(launchd.installs.lock().unwrap().len(), 1);
    assert!(launchd.uninstalls.lock().unwrap().is_empty());
    assert_eq!(cmux.workspaces.lock().unwrap().len(), 1);
    // Reusing everything asks for no group.
    assert_eq!(cmux.groups.lock().unwrap().len(), 1);
    // A reused supervisor already reaches cmux; nothing is proved again.
    assert_eq!(cmux.detached_preflights.lock().unwrap().len(), 1);
    assert_eq!(
        SqliteQueue::open(&fixture.location.db)
            .unwrap()
            .supervisors()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn up_fails_when_the_started_supervisor_never_registers() {
    let fixture = fixture();
    let cmux = FakeCmux::default();
    let mut launchd = FakeLaunchd::new(&fixture.location.db);
    launchd.registers_on_install = false;
    let mut options = fixture.options.clone();
    options.startup_timeout = Duration::from_millis(100);
    let error = lifecycle::up(
        &fixture.location,
        &fixture.repo,
        &cmux,
        &launchd,
        &FakeProcesses::default(),
        &fixture.environment,
        &options,
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("did not register within 0s"), "{message}");
    assert!(message.contains("launchd.log"), "{message}");
    // The agent stays loaded for inspection; no session workspace was opened.
    assert!(*launchd.loaded.lock().unwrap());
    assert!(cmux.workspaces.lock().unwrap().is_empty());
}

/// `up` may run from a linked worktree: trust is still read at the main
/// checkout's root, the key Claude Code uses for every worktree.
#[test]
fn up_from_a_linked_worktree_checks_the_trust_of_the_main_checkout() {
    let mut fixture = fixture();
    let worktree = fixture._dir.path().join("linked");
    git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-q",
            worktree.to_str().unwrap(),
            "-b",
            "linked",
        ],
    );
    fixture.repo = worktree;
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let result = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(result["supervisor"]["outcome"], "started", "{result}");
}

/// Run worktrees take Claude Code's folder trust from the repository root,
/// so an untrusted root would stop every run session at the trust dialog:
/// `up` refuses before it starts anything and says how to trust the root.
#[test]
fn up_fails_when_claude_code_has_not_trusted_the_repository() {
    let mut fixture = fixture();
    let config = fixture.environment.claude_config.clone().unwrap();
    let root = fixture.repo.canonicalize().unwrap();
    let cases = [
        // No config at all, no config path, another project trusted only,
        // and the root recorded with the dialog not accepted.
        None,
        Some(None),
        Some(Some(
            json!({"projects": {"/elsewhere": {"hasTrustDialogAccepted": true}}}),
        )),
        Some(Some(
            json!({"projects": {root.to_str().unwrap(): {"hasTrustDialogAccepted": false}}}),
        )),
    ];
    for case in cases {
        match &case {
            None => fixture.environment.claude_config = None,
            Some(written) => {
                fixture.environment.claude_config = Some(config.clone());
                match written {
                    Some(value) => fs::write(&config, value.to_string()).unwrap(),
                    None => {
                        let _ = fs::remove_file(&config);
                    }
                }
            }
        }
        let cmux = FakeCmux::default();
        let launchd = FakeLaunchd::new(&fixture.location.db);
        let processes = FakeProcesses::default();
        let message = format!(
            "{:#}",
            try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
        );
        assert!(
            message.starts_with(&format!(
                "Claude Code has not trusted the repository {}",
                root.display()
            )),
            "{case:?}: {message}"
        );
        assert!(message.contains("Yes, I trust this folder"), "{message}");
        assert!(launchd.installs.lock().unwrap().is_empty());
        assert!(cmux.workspaces.lock().unwrap().is_empty());
        assert_eq!(cmux.detached_preflights.lock().unwrap().len(), 0);
    }

    // A config that cannot be parsed is an error of its own, not a trust verdict.
    fixture.environment.claude_config = Some(config.clone());
    fs::write(&config, "not json").unwrap();
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let message = format!(
        "{:#}",
        try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
    );
    assert!(message.contains("parse Claude Code config"), "{message}");
}

/// cmux admits only its own terminals' children unless a socket password is
/// configured; a supervisor launchd starts is neither, so `up` proves the
/// connection first and stops before launchd sees anything.
#[test]
fn up_fails_before_writing_the_plist_when_cmux_refuses_the_detached_ping() {
    let fixture = fixture();
    let cmux = FakeCmux {
        refuses_detached: true,
        ..FakeCmux::default()
    };
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let error = lifecycle::up(
        &fixture.location,
        &fixture.repo,
        &cmux,
        &launchd,
        &processes,
        &fixture.environment,
        &fixture.options,
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.starts_with("cmux refused a connection from outside its own terminals"),
        "{message}"
    );
    assert!(
        message.contains("socket password in cmux Settings"),
        "{message}"
    );
    assert!(message.contains("export CMUX_SOCKET_PASSWORD"), "{message}");
    assert!(message.contains("run `up --in-cmux`"), "{message}");
    assert!(
        message.ends_with("only processes started inside cmux can connect"),
        "{message}"
    );
    assert_eq!(cmux.detached_preflights.lock().unwrap().len(), 1);
    // No plist, no launchd call, no session workspace, and no plist file.
    assert!(launchd.installs.lock().unwrap().is_empty());
    assert!(launchd.uninstalls.lock().unwrap().is_empty());
    assert!(!*launchd.loaded.lock().unwrap());
    assert!(!fixture.location.launch_agent.exists());
    assert!(cmux.workspaces.lock().unwrap().is_empty());
    assert_eq!(cmux.calls.load(Ordering::SeqCst), 0);
    assert!(
        SqliteQueue::open(&fixture.location.db)
            .unwrap()
            .supervisors()
            .unwrap()
            .is_empty()
    );

    // A ping that could not be run or did not answer is not a refusal and
    // does not send the operator to the password; it still stops `up`.
    let cmux = FakeCmux {
        detached_unreachable: true,
        ..FakeCmux::default()
    };
    let error = lifecycle::up(
        &fixture.location,
        &fixture.repo,
        &cmux,
        &launchd,
        &processes,
        &fixture.environment,
        &fixture.options,
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert_eq!(
        message,
        "cmux could not be asked whether it admits a connection from outside its own terminals: \"/bin/sh\" did not finish within 60s"
    );
    assert!(launchd.installs.lock().unwrap().is_empty());
    assert!(cmux.workspaces.lock().unwrap().is_empty());
}

/// A password exported by the invoking shell is what the connection is
/// proved with and what the agent stores, and nothing else changes.
#[test]
fn up_proves_the_connection_with_the_exported_password_and_stores_it_in_the_plist() {
    let mut fixture = fixture();
    fixture.environment.socket_password = Some("hunter2 & <co>".into());
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(report["inbox"]["outcome"], "created");
    assert_eq!(
        *cmux.detached_preflights.lock().unwrap(),
        vec![SupervisorEnvironment {
            path: "/usr/bin:/bin:/home/u/.local/bin".into(),
            socket_password: Some("hunter2 & <co>".into()),
        }]
    );
    let installs = launchd.installs.lock().unwrap();
    assert_eq!(installs.len(), 1);
    let contents = &installs[0].2;
    assert!(
        contents.contains(
            "<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>PATH</key>\n\t\t<string>/usr/bin:/bin:/home/u/.local/bin</string>\n\t\t<key>CMUX_SOCKET_PASSWORD</key>\n\t\t<string>hunter2 &amp; &lt;co&gt;</string>\n\t</dict>"
        ),
        "{contents}"
    );
    // The password is in no session's command: the inbox session is a
    // cmux terminal's child and needs none.
    let workspaces = cmux.workspaces.lock().unwrap();
    assert!(workspaces.iter().all(|w| !w.3.contains("hunter2")));
}

/// The detached environment keeps nothing of the cmux session `up` runs
/// in: every inherited `CMUX_*` variable is removed (the socket capability
/// and the workspace, surface and socket paths among them), PATH is the
/// agent's, and the password is the exported one or absent.
#[test]
fn detached_command_drops_every_inherited_cmux_variable_but_the_password() {
    let inherited = [
        "CMUX_SOCKET_CAPABILITY",
        "CMUX_SOCKET_PATH",
        "CMUX_WORKSPACE_ID",
        "CMUX_SURFACE_ID",
        "CMUX_SOCKET_PASSWORD",
        "HOME",
        "PATH",
        "NOT_CMUX_",
    ]
    .map(OsString::from);
    let environment = SupervisorEnvironment {
        path: "/agent/bin".into(),
        socket_password: None,
    };
    let mut command = Command::new("cmux");
    detach(&mut command, inherited.clone(), &environment);
    let envs: Vec<(String, Option<String>)> = command
        .get_envs()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.map(|value| value.to_string_lossy().into_owned()),
            )
        })
        .collect();
    assert_eq!(
        envs,
        vec![
            ("CMUX_SOCKET_CAPABILITY".to_owned(), None),
            ("CMUX_SOCKET_PASSWORD".to_owned(), None),
            ("CMUX_SOCKET_PATH".to_owned(), None),
            ("CMUX_SURFACE_ID".to_owned(), None),
            ("CMUX_WORKSPACE_ID".to_owned(), None),
            ("PATH".to_owned(), Some("/agent/bin".to_owned())),
        ]
    );

    let mut command = Command::new("cmux");
    detach(
        &mut command,
        inherited,
        &SupervisorEnvironment {
            path: "/agent/bin".into(),
            socket_password: Some("pw".into()),
        },
    );
    let password = command
        .get_envs()
        .find(|(name, _)| *name == SOCKET_PASSWORD_ENV)
        .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()));
    assert_eq!(password, Some(Some("pw".to_owned())));
    assert_eq!(
        command
            .get_envs()
            .filter(|(name, value)| name.to_string_lossy().starts_with("CMUX_") && value.is_some())
            .count(),
        1
    );
}

/// The real adapter's `notify` is `cmux notify --title … --body …`, with
/// `--workspace` only when a target is given; a failing cmux is an error.
#[test]
fn the_cmux_adapter_notifies_with_title_body_and_an_optional_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let dump = dir.path().join("args.txt");
    let stub = dir.path().join("cmux-stub");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n[ \"$1\" = notify ] || exit 2\n[ \"$3\" != fail ]\n",
            dump.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let cmux = Cmux { executable: stub };
    let args = || fs::read_to_string(&dump).unwrap();
    cmux.notify("dagq repo: task 1 failed", "a task\nnext: x", Some("WS"))
        .unwrap();
    assert_eq!(
        args(),
        "notify\n--title\ndagq repo: task 1 failed\n--body\na task\nnext: x\n--workspace\nWS\n"
    );
    cmux.notify("title", "body", None).unwrap();
    assert_eq!(args(), "notify\n--title\ntitle\n--body\nbody\n");
    assert!(cmux.notify("fail", "body", None).is_err());
}

/// The real adapter types a resolution request as one line (line breaks
/// and tabs, which `cmux send` would read as keys, become spaces, and
/// backslashes slashes) and submits it with Enter, and opens a resume
/// workspace in the run's worktree named like the run's worker workspace
/// (`[<repo>]worker#<task-id> - <task title>`, ADR-0028).
#[test]
fn the_cmux_adapter_sends_one_line_and_names_the_resume_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let dump = dir.path().join("args.txt");
    let stub = dir.path().join("cmux-stub");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\ncase \"$1\" in workspace) echo 'OK workspace:7' ;; --json) echo '{{\"caller\":{{\"workspace_id\":\"01234567-89ab-4def-8123-000000000007\"}}}}' ;; esac\n",
            dump.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let cmux = Cmux { executable: stub };
    cmux.send_text("WS", "line one\n\tline two\\n\n").unwrap();
    assert_eq!(
        fs::read_to_string(&dump).unwrap(),
        "send\n--workspace\nWS\n--\nline one line two/n\nsend-key\n--workspace\nWS\n--\nenter\n"
    );
    fs::remove_file(&dump).unwrap();
    let run = TaskRun::restore(dagq::domain::RunRecord {
        id: dagq::domain::RunId::new("run-1").unwrap(),
        task_id: dagq::domain::TaskId::new(3),
        status: dagq::domain::RunStatus::NeedsSession,
        requested_provider: dagq::domain::Provider::Claude,
        actual_provider: dagq::domain::Provider::Claude,
        base_commit: dagq::domain::CommitSha::try_from("0".repeat(40)).unwrap(),
        repo_path: Some("/src/my-repo".into()),
        run_dir: Some(dir.path().to_string_lossy().into_owned()),
        worktree_path: Some(dir.path().to_string_lossy().into_owned()),
        branch: None,
        workspace_id: None,
        receipt_path: None,
        log_path: None,
        result_commit: None,
        last_error: None,
        workspace_closed_at: None,
        created_at: String::new(),
    })
    .unwrap();
    assert_eq!(
        cmux.create_resume(
            &Task::restore(dagq::domain::TaskRecord {
                id: dagq::domain::TaskId::new(3),
                title: "fix it".into(),
                description: String::new(),
                acceptance: String::new(),
                verification_commands: Vec::new(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
                priority: Default::default(),
                status: dagq::domain::TaskStatus::InProgress,
                goal_id: None,
                context: String::new(),
                created_at: String::new(),
                updated_at: String::new(),
            })
            .unwrap(),
            &run,
            "runner session --resume",
            &WorkspaceTags {
                env: vec![("DAGQ_ROLE".into(), "worker".into())],
                description: Some(
                    dagq::infrastructure::adapters::resume_workspace_description(&run)
                ),
                group: Some("group:1".into()),
            },
        )
        .unwrap(),
        "01234567-89ab-4def-8123-000000000007"
    );
    let args = fs::read_to_string(&dump).unwrap();
    assert!(
        args.starts_with(&format!(
            "workspace\ncreate\n--name\n[my-repo]worker#3 - fix it\n--description\nrun run-1 resume\n--env\nDAGQ_ROLE=worker\n--group\ngroup:1\n--command\nrunner session --resume\n--focus\nfalse\n--cwd\n{}\n",
            dir.path().display()
        )),
        "{args}"
    );
}

/// The real adapter runs `cmux ping` in that environment and outside
/// cmux's process tree: a stub cmux dumps what it was given and who its
/// parent is, and this test process (which may itself run inside cmux)
/// leaks none of its `CMUX_*` variables into it, while launchd (pid 1) is
/// the stub's parent, as it is the LaunchAgent supervisor's.
#[test]
fn the_cmux_adapter_pings_orphaned_with_the_detached_environment() {
    let dir = tempfile::tempdir().unwrap();
    let dump = dir.path().join("env.txt");
    let stub = dir.path().join("cmux-stub");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\n[ \"$1\" = ping ] || exit 2\n/usr/bin/env > '{0}'\necho \"PARENT=$PPID\" >> '{0}'\nprintf 'PONG\\n'\n",
            dump.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let cmux = Cmux {
        executable: stub.clone(),
    };
    cmux.preflight_detached(&SupervisorEnvironment {
        path: "/usr/bin:/bin".into(),
        socket_password: Some("pw".into()),
    })
    .unwrap();
    let seen: Vec<(String, String)> = fs::read_to_string(&dump)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    let cmux_variables: Vec<&(String, String)> = seen
        .iter()
        .filter(|(name, _)| name.starts_with("CMUX_"))
        .collect();
    assert_eq!(
        cmux_variables,
        vec![&("CMUX_SOCKET_PASSWORD".to_owned(), "pw".to_owned())],
        "{seen:?}"
    );
    assert!(seen.contains(&("PATH".to_owned(), "/usr/bin:/bin".to_owned())));
    assert!(
        seen.contains(&("PARENT".to_owned(), "1".to_owned())),
        "{seen:?}"
    );

    // Without an exported password none reaches the stub either.
    cmux.preflight_detached(&SupervisorEnvironment {
        path: "/usr/bin:/bin".into(),
        socket_password: None,
    })
    .unwrap();
    assert!(
        !fs::read_to_string(&dump)
            .unwrap()
            .lines()
            .any(|line| line.starts_with("CMUX_"))
    );

    // A refusal is cmux's stderr; a wrong reply is reported as such.
    let environment = SupervisorEnvironment {
        path: "/usr/bin:/bin".into(),
        socket_password: None,
    };
    fs::write(
        &stub,
        "#!/bin/sh\necho 'only processes started inside cmux can connect' >&2\nexit 1\n",
    )
    .unwrap();
    let error = cmux.preflight_detached(&environment).unwrap_err();
    assert!(error.is::<DetachedRefusal>(), "{error:#}");
    let error = format!("{error:#}");
    assert!(error.contains("ping from outside cmux failed"), "{error}");
    assert!(
        error.ends_with("only processes started inside cmux can connect"),
        "{error}"
    );
    fs::write(&stub, "#!/bin/sh\necho PING\n").unwrap();
    let error = cmux.preflight_detached(&environment).unwrap_err();
    assert!(error.is::<DetachedRefusal>(), "{error:#}");
    assert!(
        format!("{error:#}").ends_with("unexpected response: PING"),
        "{error:#}"
    );

    // A ping that hangs is killed at the deadline and reported.
    let pid_file = dir.path().join("pid.txt");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec /bin/sleep 60\n",
            pid_file.display()
        ),
    )
    .unwrap();
    let error = cmux
        .preflight_detached_within(&environment, Duration::from_millis(300))
        .unwrap_err();
    assert!(!error.is::<DetachedRefusal>(), "{error:#}");
    assert!(
        format!("{error:#}").ends_with("did not finish within 300ms"),
        "{error:#}"
    );
    let pid: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(!process_alive(pid), "sleep {pid} outlived the deadline");
}

/// Two registrations whose processes are gone, one whose process lives and
/// holds a lease: `up` drops the dead ones only and reuses the live one.
#[test]
fn up_prunes_dead_registrations_and_keeps_live_ones_and_leases() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let dead = [dead_pid(), dead_pid()];
    queue
        .register_supervisor("dead-1", dead[0], 4, VERSION)
        .unwrap();
    queue
        .register_supervisor("dead-2", dead[1], 1, VERSION)
        .unwrap();
    queue
        .register_supervisor("live", std::process::id(), 3, VERSION)
        .unwrap();
    let task = queue
        .add(NewTask {
            title: "held".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: vec![],
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
    let repository = GitRepository::inspect(&fixture.repo).unwrap();
    queue
        .claim_for_supervisor(&repository.base_commit, "live")
        .unwrap();
    let leases_before = queue.run_leases().unwrap();
    assert_eq!(leases_before.len(), 1);

    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    processes.dead.lock().unwrap().extend(dead);
    let report = up(&fixture, &cmux, &launchd, &processes);
    let pruned = report["pruned_supervisors"].as_array().unwrap();
    assert_eq!(pruned.len(), 2, "{report}");
    assert_eq!(pruned[0], json!({"token": "dead-1", "pid": dead[0]}));
    assert_eq!(pruned[1], json!({"token": "dead-2", "pid": dead[1]}));
    assert_eq!(report["supervisor"]["outcome"], "reused");
    assert_eq!(report["supervisor"]["token"], "live");
    // Nobody started this one through `up`, so it has no mode and no
    // workspace, and `up` does not claim one for it.
    assert_eq!(report["supervisor"]["mode"], Value::Null, "{report}");
    assert_eq!(report["supervisor"]["workspace_id"], Value::Null);
    assert_eq!(remaining_mode(&queue, "live"), None);
    assert!(launchd.installs.lock().unwrap().is_empty());
    let remaining = queue.supervisors().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].token, "live");
    assert_eq!(remaining[0].parallel, 3);
    let leases_after = queue.run_leases().unwrap();
    assert_eq!(leases_after.len(), 1);
    assert_eq!(leases_after[0].run_id, leases_before[0].run_id);
    assert_eq!(leases_after[0].token, "live");
    // The claimed run is reported as unfinished with a live lease.
    let unfinished = report["doctor"]["unfinished_runs"].as_array().unwrap();
    assert_eq!(unfinished.len(), 1);
    assert_eq!(unfinished[0]["run_id"], json!(leases_before[0].run_id));
    assert_eq!(unfinished[0]["task_id"], json!(task.id()));
    assert_eq!(unfinished[0]["status"], "claimed");
    assert_eq!(unfinished[0]["lease_stale"], false);

    // A live registration that stopped heartbeating is neither pruned nor reused.
    Connection::open(&fixture.location.db)
        .unwrap()
        .execute("UPDATE supervisors SET heartbeat_at=1700000000", [])
        .unwrap();
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(report["pruned_supervisors"], json!([]));
    assert_eq!(queue.supervisors().unwrap().len(), 2);
}

#[test]
fn up_reports_runs_that_wait_for_a_person_or_the_supervisor() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let repository = GitRepository::inspect(&fixture.repo).unwrap();
    let mut claim = |title: &str| {
        let task = queue
            .add(NewTask {
                title: title.into(),
                description: String::new(),
                acceptance: String::new(),
                verification_commands: vec![],
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
        let dagq::domain::ClaimOutcome::Claimed { run } = queue
            .claim_for_supervisor(&repository.base_commit, "gone")
            .unwrap()
        else {
            panic!()
        };
        run
    };
    let awaiting = claim("awaiting");
    let parked = claim("parked");
    let orphan = claim("orphan");
    let raw = Connection::open(&fixture.location.db).unwrap();
    raw.execute(
        "UPDATE task_runs SET status='awaiting_integration' WHERE id=?1",
        [&awaiting.id()],
    )
    .unwrap();
    raw.execute(
        "UPDATE task_runs SET status='needs_session', last_error='rebase conflicted' WHERE id=?1",
        [&parked.id()],
    )
    .unwrap();
    raw.execute(
        "DELETE FROM run_leases WHERE run_id IN (?1, ?2)",
        [&awaiting.id(), &parked.id()],
    )
    .unwrap();
    // The orphan keeps a lease whose owner is dead.
    let dead = dead_pid();
    raw.execute(
        "UPDATE run_leases SET pid=?2 WHERE run_id=?1",
        rusqlite::params![orphan.id(), dead],
    )
    .unwrap();
    drop(raw);

    let processes = FakeProcesses::default();
    processes.dead.lock().unwrap().insert(dead);
    let report = up(
        &fixture,
        &FakeCmux::default(),
        &FakeLaunchd::new(&fixture.location.db),
        &processes,
    );
    assert_eq!(
        report["doctor"]["awaiting_integration"],
        json!([{"run_id": awaiting.id(), "task_id": awaiting.task_id(), "last_error": null}])
    );
    assert_eq!(
        report["doctor"]["needs_session"],
        json!([{"run_id": parked.id(), "task_id": parked.task_id(), "last_error": "rebase conflicted"}])
    );
    assert_eq!(
        report["doctor"]["unfinished_runs"],
        json!([{"run_id": orphan.id(), "task_id": orphan.task_id(), "status": "claimed", "lease_stale": true}])
    );
}

/// A workspace the queue recorded for a role `up` no longer opens (the
/// maintainer ADR-0024 retired, the resident planner ADR-0041 decision 6
/// retired) is forgotten by `up`, which opens only the inbox; the
/// workspaces themselves are left open for a person to close. A session of
/// a role `up` does not open (a worker's) skips nothing.
#[test]
fn up_forgets_the_retired_and_the_resident_planner_workspaces_and_opens_only_the_inbox() {
    let mut fixture = fixture();
    fixture.environment.role = Some("worker".into());
    fixture.environment.queue = Some(fixture.location.db.clone());
    let cmux = FakeCmux::default();
    let retired = "01234567-89ab-4def-8123-0000000000ee";
    let planner = "01234567-89ab-4def-8123-0000000000ef";
    cmux.open("[my repo]retired", &fixture.repo, retired);
    cmux.open("[my repo]planner", &fixture.repo, planner);
    let raw = Connection::open(&fixture.location.db).unwrap();
    raw.execute(
        "INSERT INTO session_workspaces(role,workspace_id) VALUES ('retired',?1),('planner',?2)",
        [retired, planner],
    )
    .unwrap();
    drop(raw);
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started");
    assert_eq!(report["inbox"]["outcome"], "created", "{report}");
    assert_eq!(report.get("planner"), None, "{report}");
    assert_eq!(report["retired_sessions"], 2, "{report}");
    let names: Vec<String> = cmux
        .workspaces
        .lock()
        .unwrap()
        .iter()
        .map(|workspace| workspace.0.clone())
        .collect();
    assert_eq!(
        names,
        ["[my repo]retired", "[my repo]planner", "[my repo]inbox"]
    );
    assert!(cmux.closed.lock().unwrap().is_empty());
    let raw = Connection::open(&fixture.location.db).unwrap();
    let roles: Vec<String> = raw
        .prepare("SELECT role FROM session_workspaces ORDER BY role")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(roles, ["inbox"]);
    // Forgetting is idempotent.
    assert_eq!(
        SqliteQueue::open(&fixture.location.db)
            .unwrap()
            .forget_retired_session_workspaces()
            .unwrap(),
        0
    );
    let second = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(second["retired_sessions"], 0, "{second}");
}

/// Inside the inbox session of this queue, `up` skips that workspace; the
/// role of a session of another queue does not count. From a planner
/// session `up` opens the inbox and no planner.
#[test]
fn up_skips_the_inbox_inside_its_own_session() {
    let mut fixture = fixture();
    fixture.environment.role = Some(INBOX_ROLE.into());
    fixture.environment.queue = Some(fixture.location.db.clone());
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(
        report["inbox"],
        json!({"outcome": "skipped", "workspace_id": null, "name": "[my repo]inbox"})
    );
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    assert_eq!(queue.session_workspace(SessionRole::Inbox).unwrap(), None);
    assert!(cmux.workspaces.lock().unwrap().is_empty());

    // A second `up` from the same session still skips it.
    let second = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(second["inbox"]["outcome"], "skipped", "{second}");
    assert!(cmux.workspaces.lock().unwrap().is_empty());

    // From an inbox session of another queue, this queue's inbox is opened.
    fixture.environment.queue = Some(fixture._dir.path().join("elsewhere.db"));
    let third = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(third["inbox"]["outcome"], "created", "{third}");
    assert_eq!(cmux.workspaces.lock().unwrap().len(), 1);

    // A planner session is not skipped for anything: it opens no planner.
    let mut fixture = self::fixture();
    fixture.environment.role = Some(PLANNER_ROLE.into());
    fixture.environment.queue = Some(fixture.location.db.clone());
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["inbox"]["outcome"], "created", "{report}");
    assert_eq!(report.get("planner"), None, "{report}");
    assert_eq!(cmux.workspaces.lock().unwrap().len(), 1);
}

/// An inbox workspace that was closed is forgotten and opened again under
/// a new UUID, and `down` closes no session's workspace, only the
/// supervisor's.
#[test]
fn up_reopens_a_closed_inbox_and_down_leaves_the_sessions_open() {
    let mut fixture = fixture();
    fixture.options.in_cmux = true;
    let cmux = FakeCmux {
        registers_supervisor_in: Some(fixture.location.db.clone()),
        ..FakeCmux::default()
    };
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let first = up(&fixture, &cmux, &launchd, &processes);
    let inbox = first["inbox"]["workspace_id"].as_str().unwrap().to_owned();
    cmux.close(&inbox).unwrap();

    let second = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(second["inbox"]["outcome"], "created", "{second}");
    let reopened = second["inbox"]["workspace_id"].as_str().unwrap();
    assert_ne!(reopened, inbox);
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    assert_eq!(
        queue
            .session_workspace(SessionRole::Inbox)
            .unwrap()
            .as_deref(),
        Some(reopened)
    );

    // The supervisor is gone; `down` closes its workspace and nothing else.
    processes
        .dead
        .lock()
        .unwrap()
        .insert(first["supervisor"]["pid"].as_u64().unwrap() as u32);
    let report = down(&fixture, &cmux, &launchd, &processes, false, false);
    assert_eq!(report["outcome"], "not_running", "{report}");
    assert_eq!(
        cmux.closed.lock().unwrap().as_slice(),
        [
            inbox,
            first["supervisor"]["workspace_id"]
                .as_str()
                .unwrap()
                .to_owned()
        ]
    );
    let names: Vec<String> = cmux
        .workspaces
        .lock()
        .unwrap()
        .iter()
        .map(|workspace| workspace.0.clone())
        .collect();
    assert_eq!(names, ["[my repo]inbox"]);
    assert!(
        queue
            .session_workspace(SessionRole::Inbox)
            .unwrap()
            .is_some()
    );
}

/// A group cmux cannot make does not stop `up`: the workspace opens outside
/// it and the result says why.
#[test]
fn up_warns_and_goes_on_when_the_workspace_group_cannot_be_made() {
    let fixture = fixture();
    let cmux = FakeCmux {
        group_fails: true,
        ..FakeCmux::default()
    };
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["inbox"]["outcome"], "created", "{report}");
    assert_eq!(cmux.tags.lock().unwrap()[0].group, None);
    let warnings = report["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{report}");
    let warning = warnings[0].as_str().unwrap();
    assert!(
        warning.contains("workspace-group create failed"),
        "{warning}"
    );
    assert!(warning.contains(&fixture.location.hash()), "{warning}");
    // The failed call is in the queue, on no run and no task (task 109).
    let failures = backend_failures(&fixture);
    assert_eq!(failures.len(), 1, "{failures:?}");
    let failure = &failures[0];
    assert_eq!((failure.task_id, failure.run_id.as_ref()), (None, None));
    assert_eq!(failure.payload["op"], "ensure_group");
    assert_eq!(failure.payload["workspace_id"], Value::Null);
    assert_eq!(failure.payload["timeout_secs"], 30);
    assert_eq!(failure.payload["error"], "workspace-group create failed");
    assert!(failure.payload["load_avg"].is_f64() || failure.payload["load_avg"].is_null());
    assert!(failure.payload["slots"].is_i64());
    assert!(failure.payload.get("parallel").is_some());
}

/// `up` colors the inbox Amber, puts a `dagq_role` pill with the role's
/// icon on it and pins it (ADR-0031), on the
/// workspace it creates and again on the one it reuses, addressed by the
/// recorded UUID. The in-cmux supervisor's workspace keeps cmux's look.
#[test]
fn up_colors_labels_and_pins_the_inbox_on_every_up() {
    let mut fixture = fixture();
    fixture.options.in_cmux = true;
    let cmux = FakeCmux {
        registers_supervisor_in: Some(fixture.location.db.clone()),
        ..FakeCmux::default()
    };
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let look = |color: &str, pill: &str| {
        vec![
            ("set-color".to_owned(), color.to_owned()),
            ("set-status".to_owned(), pill.to_owned()),
            ("pin".to_owned(), String::new()),
        ]
    };
    let inbox_look = look("Amber", "dagq_role=inbox tray");

    let first = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(first["warnings"], json!([]), "{first}");
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let recorded = |role| queue.session_workspace(role).unwrap().unwrap();
    let inbox = recorded(SessionRole::Inbox);
    assert_eq!(first["inbox"]["workspace_id"], inbox.as_str());
    assert_eq!(cmux.looks_of(&inbox), inbox_look);
    let supervisor = first["supervisor"]["workspace_id"].as_str().unwrap();
    assert!(cmux.looks_of(supervisor).is_empty());

    let second = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(second["inbox"]["outcome"], "reused", "{second}");
    assert_eq!(
        cmux.looks_of(&inbox),
        [inbox_look.clone(), inbox_look.clone()].concat()
    );

    // From inside the inbox, `up` skips opening it but still marks the
    // recorded workspace, so one an older binary opened gets its look.
    fixture.environment.role = Some(INBOX_ROLE.into());
    fixture.environment.queue = Some(fixture.location.db.clone());
    let third = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(third["inbox"]["outcome"], "skipped", "{third}");
    assert_eq!(
        cmux.looks_of(&inbox),
        [inbox_look.clone(), inbox_look.clone(), inbox_look].concat()
    );
}

/// A color, pill or pin cmux refuses does not stop `up`: the workspaces
/// are still created and recorded, each refusal is a warning naming what
/// could not be set, and the failed call is recorded like any other.
#[test]
fn up_warns_and_goes_on_when_cmux_refuses_the_look() {
    let fixture = fixture();
    let cmux = FakeCmux {
        look_fails: true,
        ..FakeCmux::default()
    };
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["inbox"]["outcome"], "created", "{report}");
    let warnings: Vec<&str> = report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|warning| warning.as_str().unwrap())
        .collect();
    assert_eq!(warnings.len(), 3, "{report}");
    let inbox = report["inbox"]["workspace_id"].as_str().unwrap();
    assert_eq!(
        warnings[0],
        format!("cmux could not set the color of the inbox workspace {inbox}: set-color refused")
    );
    assert!(
        warnings[1].contains("status pill of the inbox"),
        "{}",
        warnings[1]
    );
    assert!(warnings[2].contains("pin of the inbox"), "{}", warnings[2]);
    let ops: Vec<Value> = backend_failures(&fixture)
        .iter()
        .map(|failure| failure.payload["op"].clone())
        .collect();
    assert_eq!(ops, ["set_color", "set_status", "pin"]);
}

/// The real adapter's look calls are `workspace-action --action set-color
/// --color <c>`, `set-status <key> <value> --icon <i>` and
/// `workspace-action --action pin`, each with `--workspace <uuid>`; `close`
/// unpins before it closes, since cmux refuses to close a pinned
/// workspace, and an unpin that fails does not stop the close.
#[test]
fn the_cmux_adapter_marks_a_workspace_and_unpins_it_before_closing() {
    let dir = tempfile::tempdir().unwrap();
    let dump = dir.path().join("args.txt");
    let stub = dir.path().join("cmux-stub");
    // The unpin of `GONE` and the close of `STUCK` fail.
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\nprintf '%s ' \"$@\" >> '{}'\necho >> '{}'\ncase \"$*\" in *'unpin --workspace GONE') exit 1 ;; 'workspace close STUCK') exit 1 ;; 'workspace close'*) echo \"OK workspace:3\" ;; *) echo OK ;; esac\n",
            dump.display(),
            dump.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let cmux = Cmux { executable: stub };
    let calls = || {
        let calls = fs::read_to_string(&dump).unwrap_or_default();
        let _ = fs::remove_file(&dump);
        calls
    };
    cmux.set_color("WS", "Amber").unwrap();
    cmux.set_status("WS", "dagq_role", "inbox", "tray").unwrap();
    cmux.pin("WS").unwrap();
    assert_eq!(
        calls(),
        "workspace-action --action set-color --color Amber --workspace WS \n\
set-status dagq_role inbox --icon tray --workspace WS \n\
workspace-action --action pin --workspace WS \n"
    );
    cmux.close("WS").unwrap();
    assert_eq!(
        calls(),
        "workspace-action --action unpin --workspace WS \nworkspace close WS \n"
    );
    cmux.close("GONE").unwrap();
    assert_eq!(
        calls(),
        "workspace-action --action unpin --workspace GONE \nworkspace close GONE \n"
    );
    assert!(cmux.close("STUCK").is_err());
    assert!(cmux.set_color("GONE", "Blue").is_ok());
}

/// The `backend_call_failed` events of the fixture's queue, oldest first.
fn backend_failures(fixture: &Fixture) -> Vec<dagq::domain::RunEvent> {
    SqliteQueue::open(&fixture.location.db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "backend_call_failed")
        .collect()
}

#[test]
fn up_requires_cmux_claude_and_an_initialized_queue() {
    let fixture = fixture();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    struct NoCmux;
    impl WorkspaceBackend for NoCmux {
        fn preflight(&self) -> Result<()> {
            bail!("cmux ping failed")
        }
        fn preflight_detached(&self, _: &SupervisorEnvironment) -> Result<()> {
            unreachable!()
        }
        fn create(&self, _: &Task, _: &TaskRun, _: &str, _: &WorkspaceTags) -> Result<String> {
            unreachable!()
        }
        fn create_resume(
            &self,
            _: &Task,
            _: &TaskRun,
            _: &str,
            _: &WorkspaceTags,
        ) -> Result<String> {
            unreachable!()
        }
        fn send_text(&self, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        fn send_enter(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn capture(&self, _: &str) -> Result<String> {
            unreachable!()
        }
        fn close(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn set_color(&self, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        fn set_status(&self, _: &str, _: &str, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        fn pin(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn send_exit(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn exists(&self, _: &str) -> Result<bool> {
            unreachable!()
        }
        fn listed_workspace_ids(&self) -> Result<Vec<String>> {
            unreachable!()
        }
        fn create_named(&self, _: &str, _: &Path, _: &str, _: &WorkspaceTags) -> Result<String> {
            unreachable!()
        }
        fn ensure_group(&self, _: &str, _: &str) -> Result<String> {
            unreachable!()
        }
        fn notify(&self, _: &str, _: &str, _: Option<&str>) -> Result<()> {
            unreachable!()
        }
    }
    let error = lifecycle::up(
        &fixture.location,
        &fixture.repo,
        &NoCmux,
        &launchd,
        &processes,
        &fixture.environment,
        &fixture.options,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("cmux ping failed"));
    let mut options = fixture.options.clone();
    options.claude = fixture._dir.path().join("missing-claude");
    let error = lifecycle::up(
        &fixture.location,
        &fixture.repo,
        &FakeCmux::default(),
        &launchd,
        &processes,
        &fixture.environment,
        &options,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("missing-claude"), "{error:#}");
    let uninitialized =
        QueueLocation::explicit_in(&fixture._dir.path().join("nowhere.db"), fixture._dir.path());
    let error = lifecycle::up(
        &uninitialized,
        &fixture.repo,
        &FakeCmux::default(),
        &launchd,
        &processes,
        &fixture.environment,
        &fixture.options,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("queue must already be initialized"));
    assert!(launchd.installs.lock().unwrap().is_empty());
}

/// `--in-cmux` starts `supervise` in its own cmux workspace: launchd is
/// untouched, no out-of-cmux connection is proved (the supervisor is a
/// child of a cmux terminal), and the mode and workspace are recorded on
/// the registration so `status` and `down` can read them back.
#[test]
fn up_in_cmux_starts_the_supervisor_in_a_workspace_and_leaves_launchd_alone() {
    let mut fixture = fixture();
    fixture.options.in_cmux = true;
    let cmux = FakeCmux {
        registers_supervisor_in: Some(fixture.location.db.clone()),
        ..FakeCmux::default()
    };
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let root = GitRepository::inspect(&fixture.repo).unwrap().root;

    let first = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(first["supervisor"]["outcome"], "started", "{first}");
    assert_eq!(first["supervisor"]["mode"], "in_cmux");
    assert_eq!(first["supervisor"]["pid"], json!(std::process::id()));
    assert_eq!(first["supervisor"]["name"], "[my repo]supervisor");
    assert_eq!(first["supervisor"]["plist"], Value::Null);
    assert_eq!(
        first["supervisor"]["log_dir"],
        json!(fixture.location.log_dir)
    );
    assert_eq!(first["inbox"]["outcome"], "created");
    assert_eq!(first.get("planner"), None);

    // Nothing about launchd happened, and nothing was proved about a
    // connection from outside cmux; that is the point of the mode.
    assert!(launchd.installs.lock().unwrap().is_empty());
    assert!(launchd.uninstalls.lock().unwrap().is_empty());
    assert!(!*launchd.loaded.lock().unwrap());
    assert!(!fixture.location.launch_agent.exists());
    assert!(cmux.detached_preflights.lock().unwrap().is_empty());

    // The supervisor workspace runs this binary's `supervise` on this
    // queue from the repository root, with the queue's log directory.
    let workspaces = cmux.workspaces.lock().unwrap();
    assert_eq!(workspaces.len(), 2, "{workspaces:?}");
    let (name, cwd, id, command) = &workspaces[0];
    assert_eq!(name, "[my repo]supervisor");
    assert_eq!(cwd, &root);
    assert_eq!(first["supervisor"]["workspace_id"], json!(id));
    let db = fixture.location.db.canonicalize().unwrap();
    let quoted = |path: &Path| shell_quote(path.to_str().unwrap());
    assert_eq!(
        command,
        &format!(
            "'/opt/bin/dagq' '--db' {} 'supervise' '--parallel' '2' '--log-dir' {} '--cmux' {} '--claude' {} '--plugin-dir' {}",
            quoted(&db),
            quoted(&fixture.location.log_dir),
            quoted(&fixture.options.cmux),
            quoted(&fixture.options.claude),
            quoted(
                &fixture
                    .options
                    .plugin_dir
                    .as_ref()
                    .unwrap()
                    .canonicalize()
                    .unwrap()
            ),
        )
    );
    // The fixture's queue directory has an apostrophe: cmux types this
    // into a login shell, so every argument is quoted on its own.
    assert!(command.contains(r#"queue'"'"'s dir"#), "{command}");
    assert_eq!(workspaces[1].0, "[my repo]inbox");
    let tags = cmux.tags.lock().unwrap();
    assert_eq!(
        tags[0].env,
        vec![
            ("DAGQ_ROLE".to_owned(), "supervisor".to_owned()),
            ("DAGQ_QUEUE".to_owned(), db.to_str().unwrap().to_owned()),
        ]
    );
    let hash = fixture.location.hash();
    assert_eq!(
        tags[0].description.as_deref(),
        Some(format!("dagq role=supervisor queue={hash}").as_str())
    );
    // Every workspace joins the one group, asked for once.
    assert_eq!(tags[0].group, Some(format!("group-{hash}")));
    assert!(tags.iter().all(|tag| tag.group == tags[0].group));
    assert_eq!(cmux.groups.lock().unwrap().len(), 1);
    drop(tags);
    assert_eq!(
        SqliteQueue::open(&fixture.location.db)
            .unwrap()
            .session_workspace(SessionRole::Supervisor)
            .unwrap()
            .as_deref(),
        Some(id.as_str())
    );
    drop(workspaces);
    // Titles are for people: renaming both changes nothing below.
    cmux.rename(first["supervisor"]["workspace_id"].as_str().unwrap(), "sv");
    cmux.rename(first["inbox"]["workspace_id"].as_str().unwrap(), "ib");

    // The registration carries the mode and the workspace, and `status`
    // reports both.
    let registrations = SqliteQueue::open(&fixture.location.db)
        .unwrap()
        .supervisors()
        .unwrap();
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0].mode, Some(SupervisorMode::InCmux));
    assert_eq!(
        registrations[0].workspace_id.as_deref(),
        first["supervisor"]["workspace_id"].as_str()
    );
    let status = dagq::runtime::status(&fixture.location.db).unwrap();
    assert_eq!(status["supervisors"][0]["mode"], "in_cmux", "{status}");
    assert_eq!(
        status["supervisors"][0]["workspace_id"],
        first["supervisor"]["workspace_id"]
    );
    let doctor = dagq::runtime::doctor(&fixture.location.db, true).unwrap();
    assert_eq!(doctor["supervisors"][0]["mode"], "in_cmux", "{doctor}");

    // Idempotent: the live registration is reused with the mode it was
    // started in, and no second workspace is opened.
    let second = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(second["supervisor"]["outcome"], "reused", "{second}");
    assert_eq!(second["supervisor"]["mode"], "in_cmux");
    assert_eq!(
        second["supervisor"]["workspace_id"],
        first["supervisor"]["workspace_id"]
    );
    assert_eq!(second["inbox"]["outcome"], "reused", "{second}");
    assert_eq!(cmux.workspaces.lock().unwrap().len(), 2);
    assert!(launchd.installs.lock().unwrap().is_empty());
}

/// A registration that is alive but no longer heartbeating is neither
/// pruned nor reused, and it may start heartbeating again while `up` waits
/// for the supervisor it just started. `up` must not take it for the one
/// it started: the mode and workspace it writes would land on a supervisor
/// that never ran in that workspace, and `down` would later interrupt the
/// wrong process while closing the right one's workspace.
#[test]
fn up_in_cmux_does_not_mistake_a_silent_supervisor_for_the_one_it_started() {
    let mut fixture = fixture();
    fixture.options.in_cmux = true;
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    // Registered before `up`, alive, last heartbeat far in the past, and
    // first in `started_at` order.
    queue
        .register_supervisor("silent", std::process::id(), 4, VERSION)
        .unwrap();
    Connection::open(&fixture.location.db)
        .unwrap()
        .execute("UPDATE supervisors SET heartbeat_at=1700000000", [])
        .unwrap();
    let cmux = FakeCmux {
        registers_supervisor_in: Some(fixture.location.db.clone()),
        ..FakeCmux::default()
    };
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();

    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(report["supervisor"]["mode"], "in_cmux");
    assert_ne!(report["supervisor"]["token"], "silent", "{report}");
    // The silent registration is untouched; the new one carries the mode
    // and the workspace `up` opened.
    assert_eq!(remaining_mode(&queue, "silent"), None);
    let started = queue
        .supervisors()
        .unwrap()
        .into_iter()
        .find(|registration| registration.token != "silent")
        .expect("the started supervisor is registered");
    assert_eq!(started.mode, Some(SupervisorMode::InCmux));
    assert_eq!(
        started.workspace_id,
        report["supervisor"]["workspace_id"]
            .as_str()
            .map(str::to_owned)
    );
}

/// cmux keeps a workspace open after its command exits, so a supervisor
/// that crashed (or one that is alive but silent, which `up` never reuses)
/// leaves `[<repo>]supervisor` behind. `up --in-cmux` stops rather
/// than open a second one; closing it is a person's call.
#[test]
fn up_in_cmux_refuses_to_open_a_second_supervisor_workspace() {
    let mut fixture = fixture();
    fixture.options.in_cmux = true;
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    // Recorded as the queue's supervisor workspace by the `up` that
    // opened it, and renamed since: the title plays no part.
    let leftover = "01234567-89ab-4def-8123-0000000000cc";
    cmux.open("renamed by hand", &fixture.repo, leftover);
    SqliteQueue::open(&fixture.location.db)
        .unwrap()
        .register_session_workspace(SessionRole::Supervisor, leftover)
        .unwrap();
    let error = lifecycle::up(
        &fixture.location,
        &fixture.repo,
        &cmux,
        &launchd,
        &processes,
        &fixture.environment,
        &fixture.options,
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains(leftover), "{message}");
    assert!(message.contains("supervisor workspace"), "{message}");
    assert!(
        message.contains(&format!("cmux workspace close {leftover}")),
        "{message}"
    );
    // Nothing was started and no session workspace was opened.
    assert_eq!(cmux.workspaces.lock().unwrap().len(), 1);
    assert!(launchd.installs.lock().unwrap().is_empty());
    assert!(
        SqliteQueue::open(&fixture.location.db)
            .unwrap()
            .supervisors()
            .unwrap()
            .is_empty()
    );
}

/// A recorded supervisor workspace that cmux no longer lists (a person
/// closed it after reading it) is forgotten, and `up --in-cmux` opens a new
/// one and records that instead.
#[test]
fn up_in_cmux_forgets_a_supervisor_workspace_that_was_closed() {
    let mut fixture = fixture();
    fixture.options.in_cmux = true;
    let cmux = FakeCmux {
        registers_supervisor_in: Some(fixture.location.db.clone()),
        ..FakeCmux::default()
    };
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    queue
        .register_session_workspace(
            SessionRole::Supervisor,
            "01234567-89ab-4def-8123-0000000000ee",
        )
        .unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(
        queue.session_workspace(SessionRole::Supervisor).unwrap(),
        report["supervisor"]["workspace_id"]
            .as_str()
            .map(str::to_owned)
    );
}

/// An in-cmux supervisor has no service manager to signal it, so `down`
/// sends the SIGINT itself and closes its workspace once the process is
/// gone: never while it drains, after the drain under `--wait`, and after
/// the kill under `--force`.
#[test]
fn down_interrupts_an_in_cmux_supervisor_and_closes_its_workspace_once_it_is_gone() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    let cmux = FakeCmux::default();
    let workspace = cmux
        .create_named(
            "[my repo]supervisor",
            &fixture.repo,
            "supervise",
            &WorkspaceTags::default(),
        )
        .unwrap();
    queue
        .register_supervisor("in-cmux", pid, 2, VERSION)
        .unwrap();
    queue
        .set_supervisor_mode("in-cmux", SupervisorMode::InCmux, Some(&workspace))
        .unwrap();
    queue
        .register_session_workspace(SessionRole::Supervisor, &workspace)
        .unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();

    // Default: SIGINT and return, leaving the workspace open so the drain
    // is not cut short.
    let report = down(&fixture, &cmux, &launchd, &processes, false, false);
    assert_eq!(report["outcome"], "draining", "{report}");
    assert_eq!(report["pid"], pid);
    assert_eq!(report["launch_agent_unloaded"], false);
    assert_eq!(processes.interrupted.lock().unwrap().as_slice(), &[pid]);
    assert!(processes.terminated.lock().unwrap().is_empty());
    assert_eq!(
        report["supervisor_workspaces"],
        json!([{
            "workspace_id": workspace,
            "outcome": "left_open",
            "reason": format!(
                "supervisor pid {pid} is still draining; `down --wait` closes it"
            ),
        }])
    );
    assert!(cmux.closed.lock().unwrap().is_empty());

    // `--wait` waits for the drain, then closes the workspace it left.
    // The supervisor deregisters at the end of its drain and only then
    // exits, so its pid is still reported alive when the wait returns; the
    // close must not depend on that.
    let processes = FakeProcesses::default();
    let db = fixture.location.db.clone();
    let report = thread::scope(|scope| {
        scope.spawn(move || {
            thread::sleep(Duration::from_millis(200));
            SqliteQueue::open(&db)
                .unwrap()
                .deregister_supervisor("in-cmux")
                .unwrap();
        });
        down(&fixture, &cmux, &launchd, &processes, true, false)
    });
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["pid"], pid);
    assert_eq!(processes.interrupted.lock().unwrap().as_slice(), &[pid]);
    assert_eq!(
        report["supervisor_workspaces"],
        json!([{"workspace_id": workspace, "outcome": "closed"}])
    );
    assert_eq!(
        cmux.closed.lock().unwrap().as_slice(),
        std::slice::from_ref(&workspace)
    );
    assert!(cmux.workspaces.lock().unwrap().is_empty());
    assert!(processes.alive(pid), "the fake never reaped the pid");
    // The supervisor removed its own row at the end of the drain, and the
    // record of its workspace went with the close.
    assert!(queue.supervisors().unwrap().is_empty());
    assert_eq!(
        queue.session_workspace(SessionRole::Supervisor).unwrap(),
        None
    );
}

/// `--force` kills the in-cmux supervisor, drops its registration and
/// closes its workspace in one go; a close cmux refuses is reported
/// instead of failing the stop that already happened.
#[test]
fn down_force_kills_an_in_cmux_supervisor_and_closes_or_reports_its_workspace() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    let cmux = FakeCmux::default();
    let workspace = cmux
        .create_named(
            "[my repo]supervisor",
            &fixture.repo,
            "supervise",
            &WorkspaceTags::default(),
        )
        .unwrap();
    queue
        .register_supervisor("in-cmux", pid, 2, VERSION)
        .unwrap();
    queue
        .set_supervisor_mode("in-cmux", SupervisorMode::InCmux, Some(&workspace))
        .unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = down(&fixture, &cmux, &launchd, &processes, false, true);
    assert_eq!(report["outcome"], "killed", "{report}");
    assert_eq!(processes.interrupted.lock().unwrap().as_slice(), &[pid]);
    assert_eq!(processes.killed.lock().unwrap().as_slice(), &[pid]);
    assert_eq!(
        report["supervisor_workspaces"],
        json!([{"workspace_id": workspace, "outcome": "closed"}])
    );
    assert!(queue.supervisors().unwrap().is_empty());

    // cmux refusing the close (the workspace is already gone) leaves the
    // reason in the report; the supervisor is stopped either way.
    queue.register_supervisor("again", pid, 2, VERSION).unwrap();
    queue
        .set_supervisor_mode("again", SupervisorMode::InCmux, Some(&workspace))
        .unwrap();
    let processes = FakeProcesses::default();
    let report = down(&fixture, &cmux, &launchd, &processes, false, true);
    assert_eq!(report["outcome"], "killed", "{report}");
    assert_eq!(
        report["supervisor_workspaces"][0]["outcome"], "close_failed",
        "{report}"
    );
    assert_eq!(
        report["supervisor_workspaces"][0]["reason"],
        format!("no such workspace: {workspace}")
    );
    assert!(queue.supervisors().unwrap().is_empty());
    // The refused close is recorded without a run (task 109).
    let failures = backend_failures(&fixture);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].run_id, None);
    assert_eq!(failures[0].payload["op"], "close");
    assert_eq!(failures[0].payload["workspace_id"], json!(workspace));
    assert_eq!(
        failures[0].payload["error"],
        format!("no such workspace: {workspace}")
    );
}

fn down(
    fixture: &Fixture,
    cmux: &FakeCmux,
    launchd: &FakeLaunchd,
    processes: &FakeProcesses,
    wait: bool,
    force: bool,
) -> Value {
    lifecycle::down(
        &fixture.location,
        cmux,
        launchd,
        processes,
        &DownOptions {
            wait,
            force,
            poll: Duration::from_millis(20),
        },
    )
    .unwrap()
}

#[test]
fn down_reports_not_running_without_a_live_registration_and_still_unloads_the_agent() {
    let fixture = fixture();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let cmux = FakeCmux::default();
    let processes = FakeProcesses::default();
    let report = down(&fixture, &cmux, &launchd, &processes, false, false);
    assert_eq!(
        report,
        json!({"outcome": "not_running", "launch_agent_unloaded": false,
               "pruned_supervisors": [], "supervisor_workspaces": []})
    );
    // A dead registration is not a running supervisor either; a loaded
    // agent with no registered process (a crash loop) is unloaded.
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let dead = dead_pid();
    queue.register_supervisor("dead", dead, 1, VERSION).unwrap();
    processes.dead.lock().unwrap().insert(dead);
    launchd.load(None);
    let report = down(&fixture, &cmux, &launchd, &processes, true, false);
    assert_eq!(
        report,
        json!({"outcome": "not_running", "launch_agent_unloaded": true,
               "pruned_supervisors": [], "supervisor_workspaces": []})
    );
    assert_eq!(
        launchd.uninstalls.lock().unwrap().as_slice(),
        &[
            (
                fixture.location.label.clone(),
                fixture.location.launch_agent.clone()
            ),
            (
                fixture.location.label.clone(),
                fixture.location.launch_agent.clone()
            ),
        ]
    );
    assert!(processes.terminated.lock().unwrap().is_empty());
    assert!(processes.killed.lock().unwrap().is_empty());
    // The dead row is `up`'s to prune; only `down --force` removes it too.
    assert_eq!(queue.supervisors().unwrap().len(), 1);
    let report = down(&fixture, &cmux, &launchd, &processes, false, true);
    assert_eq!(
        report,
        json!({"outcome": "not_running", "launch_agent_unloaded": false,
               "pruned_supervisors": [{"token": "dead", "pid": dead}],
               "supervisor_workspaces": []})
    );
    assert!(queue.supervisors().unwrap().is_empty());
    assert!(processes.killed.lock().unwrap().is_empty());
}

/// A queue can hold supervisors of both modes at once: the launchd one is
/// left to the bootout's SIGTERM, the in-cmux one is interrupted here, and
/// only the in-cmux one has a workspace to close.
#[test]
fn down_stops_a_launchd_and_an_in_cmux_supervisor_in_one_call() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let agent_pid = std::process::id();
    let in_cmux_pid = dead_pid(); // any pid the fake treats as alive
    let cmux = FakeCmux::default();
    let workspace = cmux
        .create_named(
            "[my repo]supervisor",
            &fixture.repo,
            "supervise",
            &WorkspaceTags::default(),
        )
        .unwrap();
    queue
        .register_supervisor("agent", agent_pid, 4, VERSION)
        .unwrap();
    queue
        .set_supervisor_mode("agent", SupervisorMode::Launchd, None)
        .unwrap();
    queue
        .register_supervisor("in-cmux", in_cmux_pid, 2, VERSION)
        .unwrap();
    queue
        .set_supervisor_mode("in-cmux", SupervisorMode::InCmux, Some(&workspace))
        .unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    launchd.load(Some(agent_pid));
    let processes = FakeProcesses::default();

    let report = down(&fixture, &cmux, &launchd, &processes, false, true);
    assert_eq!(report["outcome"], "killed", "{report}");
    assert_eq!(report["pids"], json!([agent_pid, in_cmux_pid]));
    assert_eq!(report["launch_agent_unloaded"], true);
    // launchd's bootout carries the agent's SIGTERM; only the in-cmux
    // supervisor is signalled from here, and with SIGINT.
    assert!(processes.terminated.lock().unwrap().is_empty());
    assert_eq!(
        processes.interrupted.lock().unwrap().as_slice(),
        &[in_cmux_pid]
    );
    assert_eq!(
        report["supervisor_workspaces"],
        json!([{"workspace_id": workspace, "outcome": "closed"}])
    );
    assert!(queue.supervisors().unwrap().is_empty());
}

#[test]
fn down_unloads_the_agent_and_returns_while_the_supervisor_drains() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    queue
        .register_supervisor("resident", pid, 4, VERSION)
        .unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let cmux = FakeCmux::default();
    launchd.load(Some(pid));
    let processes = FakeProcesses::default();
    let report = down(&fixture, &cmux, &launchd, &processes, false, false);
    assert_eq!(
        report,
        json!({"outcome": "draining", "pid": pid, "pids": [pid],
               "launch_agent_unloaded": true, "supervisor_workspaces": []})
    );
    assert_eq!(
        launchd.uninstalls.lock().unwrap().as_slice(),
        &[(
            fixture.location.label.clone(),
            fixture.location.launch_agent.clone()
        )]
    );
    // launchd delivered the SIGTERM; nothing was signalled or removed here.
    assert!(processes.terminated.lock().unwrap().is_empty());
    assert!(processes.killed.lock().unwrap().is_empty());
    assert_eq!(queue.supervisors().unwrap().len(), 1);

    // A supervisor started by hand has no agent: it gets the SIGTERM directly.
    let report = down(&fixture, &cmux, &launchd, &processes, false, false);
    assert_eq!(report["outcome"], "draining");
    assert_eq!(report["launch_agent_unloaded"], false);
    assert_eq!(processes.terminated.lock().unwrap().as_slice(), &[pid]);

    // Both at once: the agent's own process is left to launchd's SIGTERM
    // (a second one would end its drain), the hand-started one is signalled.
    let by_hand = dead_pid(); // any pid the fake treats as alive
    queue
        .register_supervisor("by-hand", by_hand, 1, VERSION)
        .unwrap();
    launchd.load(Some(pid));
    let processes = FakeProcesses::default();
    let report = down(&fixture, &cmux, &launchd, &processes, false, false);
    assert_eq!(report["outcome"], "draining");
    assert_eq!(report["pids"], json!([pid, by_hand]));
    assert_eq!(processes.terminated.lock().unwrap().as_slice(), &[by_hand]);
    // When launchd cannot say which process is the agent's, nobody is signalled.
    launchd.load(None);
    let processes = FakeProcesses::default();
    down(&fixture, &cmux, &launchd, &processes, false, false);
    assert!(processes.terminated.lock().unwrap().is_empty());
}

#[test]
fn down_wait_returns_stopped_once_the_registration_is_gone_or_the_process_died() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    queue
        .register_supervisor("resident", pid, 4, VERSION)
        .unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let cmux = FakeCmux::default();
    launchd.load(Some(pid));
    let processes = FakeProcesses::default();
    // The supervisor deregisters itself at the end of its drain.
    let db = fixture.location.db.clone();
    let drain = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        SqliteQueue::open(&db)
            .unwrap()
            .deregister_supervisor("resident")
            .unwrap();
    });
    let report = down(&fixture, &cmux, &launchd, &processes, true, false);
    {
        let _waiting = common::within(common::STEP_LIMIT, "the drain thread to return");
        drain.join().unwrap();
    }
    assert_eq!(
        report,
        json!({"outcome": "stopped", "pid": pid, "pids": [pid],
               "launch_agent_unloaded": true, "supervisor_workspaces": []})
    );
    assert!(processes.killed.lock().unwrap().is_empty());

    // A supervisor killed by launchd's ExitTimeOut leaves its row; the dead
    // PID ends the wait just the same (and `up` prunes the row later).
    queue
        .register_supervisor("second", pid, 4, VERSION)
        .unwrap();
    launchd.load(Some(pid));
    struct DiesLater {
        polls: AtomicUsize,
    }
    impl ProcessControl for DiesLater {
        fn alive(&self, _: u32) -> bool {
            self.polls.fetch_add(1, Ordering::SeqCst) < 3
        }
        fn terminate(&self, _: u32) -> Result<()> {
            unreachable!()
        }
        fn interrupt(&self, _: u32) -> Result<()> {
            unreachable!()
        }
        fn kill(&self, _: u32) -> Result<()> {
            unreachable!()
        }
    }
    let report = lifecycle::down(
        &fixture.location,
        &cmux,
        &launchd,
        &DiesLater {
            polls: AtomicUsize::new(0),
        },
        &DownOptions {
            wait: true,
            force: false,
            poll: Duration::from_millis(20),
        },
    )
    .unwrap();
    assert_eq!(
        report,
        json!({"outcome": "stopped", "pid": pid, "pids": [pid],
               "launch_agent_unloaded": true, "supervisor_workspaces": []})
    );
    // The row stays: the process, not `down`, removes a registration.
    assert_eq!(queue.supervisors().unwrap().len(), 1);
}

#[test]
fn down_force_kills_after_the_unload_and_drops_the_registration() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    queue
        .register_supervisor("resident", pid, 4, VERSION)
        .unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let cmux = FakeCmux::default();
    launchd.load(Some(pid));
    let processes = FakeProcesses::default();
    let report = down(&fixture, &cmux, &launchd, &processes, false, true);
    assert_eq!(
        report,
        json!({"outcome": "killed", "pid": pid, "pids": [pid],
               "launch_agent_unloaded": true, "pruned_supervisors": [],
               "supervisor_workspaces": []})
    );
    assert_eq!(launchd.uninstalls.lock().unwrap().len(), 1);
    assert_eq!(processes.killed.lock().unwrap().as_slice(), &[pid]);
    assert!(queue.supervisors().unwrap().is_empty());
    assert!(!*launchd.loaded.lock().unwrap());
}

#[test]
fn inbox_and_planner_prompts_name_the_queue_and_their_one_job() {
    let db = Path::new("/data/q/queue.db");
    let inbox = inbox_prompt(db).unwrap();
    assert!(inbox.starts_with("You are the inbox of the dagq queue at /data/q/queue.db:"));
    assert!(inbox.lines().count() <= 5, "{inbox}");
    assert!(inbox.contains("never decide anything yourself"));
    assert!(inbox.contains("Start with `dagq status --role inbox`"));
    assert!(inbox.contains("dagq-inbox skill"));
    assert!(inbox.contains("`dagq watch --role inbox --after <cursor>` in the background"));
    assert!(inbox.contains("watch again from the cursor it returns"));
    assert!(inbox.contains("On ask_opened, read the ask with `dagq asks --open --role inbox`"));
    assert!(inbox.contains("show the person its question and options"));
    assert!(inbox.contains("AskUserQuestion"));
    assert!(inbox.contains("`dagq answer ID --text '<answer>'`"));
    // Every attention is the inbox's now (ADR-0024 decision 6).
    assert!(inbox.contains("a stopped supervisor"), "{inbox}");
    assert!(inbox.contains("an answered ask"), "{inbox}");
    // The inbox relays a stuck_exit ask like any other; it acts on nothing.
    assert!(!inbox.contains("stuck_exit"));
    assert!(!inbox.contains("/exit"));
    assert!(inbox.contains("Never open the queue database directly"));

    let planner = planner_prompt(db).unwrap();
    assert!(planner.starts_with("You are a planner of the dagq queue at /data/q/queue.db:"));
    assert!(planner.lines().count() <= 5, "{planner}");
    assert!(planner.contains("the person's problems"));
    assert!(planner.contains("dagq-planner skill"));
    assert!(planner.contains("dagq skill describes"));
    assert!(planner.contains("You do not land runs or answer asks"));
    // A planner submits for plan review; only plan review makes tasks ready
    // (ADR-0041 decision 8).
    assert!(planner.contains("submit them for plan review"), "{planner}");
    assert!(!planner.contains("ready"), "{planner}");
    assert!(planner.contains("check their receipts against the goal's acceptance"));
    assert!(planner.contains("`dagq goal close ID --verdict achieved`"));
    // Observer notes and draft goals are a later goal's; until then the
    // prompt says nothing about them.
    assert!(!planner.contains("note"), "{planner}");
    assert!(!planner.contains("draft"), "{planner}");

    let command = inbox_command(db, Path::new("/opt/claude"), Some(Path::new("/p"))).unwrap();
    assert!(command.starts_with("'/opt/claude' '"), "{command}");
    assert!(command.contains("'--' 'You are the inbox of"), "{command}");
    assert_eq!(ROLE_ENV, "DAGQ_ROLE");
    assert_eq!(QUEUE_ENV, "DAGQ_QUEUE");
    assert!(
        inbox_command(db, Path::new("/opt/claude"), Some(Path::new("/p")))
            .unwrap()
            .contains("'--plugin-dir' '/p'")
    );
}

/// A live supervisor of another build is not reused: `up` unloads its
/// agent, waits for the drain and starts one of its own version in its
/// place (ADR-0014). The whole build identifier is compared (ADR-0045
/// decision 3), so a build of the same package version from another commit
/// is replaced too. A registration older than the `binary_version` column
/// has no version at all, which is not this one either, so it is replaced
/// the same way.
#[test]
fn up_drains_and_replaces_a_launchd_supervisor_of_another_version() {
    let other_commit = format!("{}+{}", env!("CARGO_PKG_VERSION"), "0".repeat(40));
    assert_ne!(other_commit, VERSION);
    for previous in [Some("0.0.1"), Some(other_commit.as_str()), None] {
        let fixture = fixture();
        let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
        let pid = std::process::id();
        queue.register_supervisor("old", pid, 4, VERSION).unwrap();
        queue
            .set_supervisor_mode("old", SupervisorMode::Launchd, None)
            .unwrap();
        set_binary_version(&fixture.location.db, "old", previous);
        let cmux = FakeCmux::default();
        let launchd = FakeLaunchd::new(&fixture.location.db);
        // The agent is loaded and its process is the registered one, so
        // launchd's bootout carries the SIGTERM and `up` sends none.
        launchd.load(Some(pid));
        let processes = FakeProcesses::default();

        let report = thread::scope(|scope| {
            scope.spawn(|| {
                // The supervisor stops claiming once the bootout's SIGTERM
                // reaches it, finishes its runs and removes its own row at
                // the end of the drain.
                wait_until(&processes, pid, || {
                    !launchd.uninstalls.lock().unwrap().is_empty()
                });
                SqliteQueue::open(&fixture.location.db)
                    .unwrap()
                    .deregister_supervisor("old")
                    .unwrap();
            });
            up(&fixture, &cmux, &launchd, &processes)
        });

        let supervisor = &report["supervisor"];
        assert_eq!(supervisor["outcome"], "restarted", "{report}");
        assert_eq!(supervisor["version"], VERSION, "{report}");
        assert_eq!(supervisor["previous_version"], json!(previous), "{report}");
        assert_eq!(supervisor["mode"], "launchd");
        assert_ne!(supervisor["token"], "old", "{report}");
        assert_eq!(
            supervisor["replaced"],
            json!([{
                "token": "old",
                "pid": pid,
                "mode": "launchd",
                "workspace_id": Value::Null,
                "version": previous,
            }])
        );
        // No in-cmux supervisor was replaced, so nothing was closed.
        assert_eq!(supervisor["supervisor_workspaces"], json!([]));
        // The old agent went before the new one was written, and the
        // SIGTERM bootout already delivered was not repeated.
        assert_eq!(launchd.uninstalls.lock().unwrap().len(), 1);
        assert_eq!(launchd.installs.lock().unwrap().len(), 1);
        // Asked once, before the drain, and not asked again by the start.
        assert_eq!(cmux.detached_preflights.lock().unwrap().len(), 1);
        assert!(processes.terminated.lock().unwrap().is_empty());
        assert!(processes.interrupted.lock().unwrap().is_empty());
        assert!(processes.killed.lock().unwrap().is_empty());
        // Only the started supervisor is registered, carrying the version
        // the fake `supervise` recorded for itself (the real write is
        // covered by tests/queue.rs and the e2e).
        let registrations = queue.supervisors().unwrap();
        assert_eq!(registrations.len(), 1, "{registrations:?}");
        assert_eq!(registrations[0].binary_version.as_deref(), Some(VERSION));
        assert_eq!(registrations[0].mode, Some(SupervisorMode::Launchd));
        assert_eq!(registrations[0].token, supervisor["token"]);
    }
}

/// A supervisor that was not signalled by launchd's bootout (one started
/// by hand, so the agent holds no process) gets the SIGTERM from `up`
/// itself, the way `down` sends it.
#[test]
fn up_terminates_a_replaced_supervisor_that_launchd_did_not_signal() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    queue
        .register_supervisor("by-hand", pid, 1, VERSION)
        .unwrap();
    set_binary_version(&fixture.location.db, "by-hand", Some("0.0.1"));
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();

    let report = thread::scope(|scope| {
        scope.spawn(|| {
            wait_until(&processes, pid, || {
                !processes.terminated.lock().unwrap().is_empty()
            });
            SqliteQueue::open(&fixture.location.db)
                .unwrap()
                .deregister_supervisor("by-hand")
                .unwrap();
        });
        up(&fixture, &cmux, &launchd, &processes)
    });
    assert_eq!(report["supervisor"]["outcome"], "restarted", "{report}");
    // It had no mode of its own, and the replacement reports it as such.
    assert_eq!(report["supervisor"]["replaced"][0]["mode"], Value::Null);
    assert_eq!(processes.terminated.lock().unwrap().as_slice(), &[pid]);
    assert!(processes.interrupted.lock().unwrap().is_empty());
}

/// `--in-cmux` replaces an in-cmux supervisor the way `down` stops one:
/// SIGINT, wait for the drain, close the workspace — and only then open a
/// new one, which needs the same name.
#[test]
fn up_in_cmux_replaces_an_in_cmux_supervisor_of_another_version() {
    let mut fixture = fixture();
    fixture.options.in_cmux = true;
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    let cmux = FakeCmux {
        registers_supervisor_in: Some(fixture.location.db.clone()),
        ..FakeCmux::default()
    };
    // The workspace the replaced supervisor runs in, put there directly:
    // going through `create_named` would register a supervisor for it.
    let workspace = "01234567-89ab-4def-8123-0000000000ff".to_owned();
    cmux.open("[my repo]supervisor", &fixture.repo, &workspace);
    // The `up` that started it recorded the same workspace; this one is
    // closed by the replacement, so it must not refuse it.
    queue
        .register_session_workspace(SessionRole::Supervisor, &workspace)
        .unwrap();
    queue.register_supervisor("old", pid, 2, VERSION).unwrap();
    queue
        .set_supervisor_mode("old", SupervisorMode::InCmux, Some(&workspace))
        .unwrap();
    set_binary_version(&fixture.location.db, "old", Some("0.0.1"));
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();

    let report = thread::scope(|scope| {
        scope.spawn(|| {
            // SIGINT is the only signal an in-cmux supervisor gets.
            wait_until(&processes, pid, || {
                !processes.interrupted.lock().unwrap().is_empty()
            });
            SqliteQueue::open(&fixture.location.db)
                .unwrap()
                .deregister_supervisor("old")
                .unwrap();
        });
        up(&fixture, &cmux, &launchd, &processes)
    });

    let supervisor = &report["supervisor"];
    assert_eq!(supervisor["outcome"], "restarted", "{report}");
    assert_eq!(supervisor["mode"], "in_cmux", "{report}");
    assert_eq!(supervisor["previous_version"], "0.0.1");
    assert_eq!(supervisor["version"], VERSION);
    assert_eq!(processes.interrupted.lock().unwrap().as_slice(), &[pid]);
    assert!(processes.terminated.lock().unwrap().is_empty());
    assert_eq!(
        supervisor["supervisor_workspaces"],
        json!([{"workspace_id": workspace, "outcome": "closed"}])
    );
    // The old workspace was closed before the new one was opened, so the
    // name was free and `up` did not refuse it.
    assert_eq!(
        cmux.closed.lock().unwrap().as_slice(),
        std::slice::from_ref(&workspace)
    );
    assert_ne!(supervisor["workspace_id"], json!(workspace), "{report}");
    assert_eq!(
        queue.session_workspace(SessionRole::Supervisor).unwrap(),
        supervisor["workspace_id"].as_str().map(str::to_owned)
    );
    let started = queue
        .supervisors()
        .unwrap()
        .into_iter()
        .find(|registration| registration.token != "old")
        .expect("the started supervisor is registered");
    assert_eq!(started.mode, Some(SupervisorMode::InCmux));
    assert_eq!(started.binary_version.as_deref(), Some(VERSION));
    assert_eq!(
        started.workspace_id.as_deref(),
        supervisor["workspace_id"].as_str()
    );
    // launchd was left alone apart from clearing any agent of this queue.
    assert!(launchd.installs.lock().unwrap().is_empty());
}

/// `up --no-wait` will not sit through a drain: with runs in flight it
/// refuses, naming them, and leaves the old supervisor and its agent
/// exactly as they were. With nothing in flight there is nothing to wait
/// for and the replacement goes ahead.
#[test]
fn up_no_wait_refuses_to_replace_while_runs_are_in_flight() {
    let mut fixture = fixture();
    fixture.options.no_wait = true;
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    queue.register_supervisor("old", pid, 4, VERSION).unwrap();
    queue
        .set_supervisor_mode("old", SupervisorMode::Launchd, None)
        .unwrap();
    set_binary_version(&fixture.location.db, "old", Some("0.0.1"));
    let run_id = claim_a_run(&fixture, &mut queue, "old");
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    launchd.load(Some(pid));
    let processes = FakeProcesses::default();

    let error = format!(
        "{:#}",
        try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
    );
    assert!(error.contains("1 run(s) are still in flight"), "{error}");
    assert!(error.contains(&run_id), "{error}");
    assert!(
        error.contains("0.0.1") && error.contains(VERSION),
        "{error}"
    );
    // Nothing was stopped, signalled, written or opened.
    assert!(launchd.uninstalls.lock().unwrap().is_empty());
    assert!(launchd.installs.lock().unwrap().is_empty());
    assert!(processes.terminated.lock().unwrap().is_empty());
    assert!(processes.interrupted.lock().unwrap().is_empty());
    assert!(cmux.workspaces.lock().unwrap().is_empty());
    let registrations = queue.supervisors().unwrap();
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0].token, "old");

    // The run comes to rest; now the drain is immediate and `--no-wait`
    // replaces the supervisor without waiting for anything.
    Connection::open(&fixture.location.db)
        .unwrap()
        .execute_batch(&format!(
            "UPDATE task_runs SET status='interrupted' WHERE id='{run_id}';
             DELETE FROM run_leases WHERE run_id='{run_id}';"
        ))
        .unwrap();
    let report = thread::scope(|scope| {
        scope.spawn(|| {
            wait_until(&processes, pid, || {
                !launchd.uninstalls.lock().unwrap().is_empty()
            });
            SqliteQueue::open(&fixture.location.db)
                .unwrap()
                .deregister_supervisor("old")
                .unwrap();
        });
        up(&fixture, &cmux, &launchd, &processes)
    });
    assert_eq!(report["supervisor"]["outcome"], "restarted", "{report}");
    assert_eq!(report["supervisor"]["previous_version"], "0.0.1");
}

/// The version is what decides: a live supervisor of this binary's own
/// build is reused, with nothing stopped, signalled or written.
#[test]
fn up_reuses_a_supervisor_of_this_binary_version() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    queue.register_supervisor("live", pid, 4, VERSION).unwrap();
    queue
        .set_supervisor_mode("live", SupervisorMode::Launchd, None)
        .unwrap();
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    launchd.load(Some(pid));
    let processes = FakeProcesses::default();

    let report = up(&fixture, &cmux, &launchd, &processes);
    let supervisor = &report["supervisor"];
    assert_eq!(supervisor["outcome"], "reused", "{report}");
    assert_eq!(supervisor["token"], "live");
    assert_eq!(supervisor["version"], VERSION);
    // Nothing was replaced, so the replacement fields are absent rather
    // than null (indexing a missing key would read as null either way).
    let supervisor = supervisor.as_object().unwrap();
    assert!(!supervisor.contains_key("previous_version"), "{report}");
    assert!(!supervisor.contains_key("replaced"), "{report}");
    assert!(launchd.uninstalls.lock().unwrap().is_empty());
    assert!(launchd.installs.lock().unwrap().is_empty());
    assert!(processes.terminated.lock().unwrap().is_empty());
    assert!(processes.interrupted.lock().unwrap().is_empty());
    assert_eq!(queue.supervisors().unwrap().len(), 1);
}

/// The replacement asks cmux whether it will admit the new supervisor
/// before it stops the old one. A refusal there must leave the working
/// supervisor alone: draining it first and failing afterwards would leave
/// the queue with nothing serving it.
#[test]
fn up_proves_the_detached_connection_before_draining_the_old_supervisor() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    queue.register_supervisor("old", pid, 4, VERSION).unwrap();
    queue
        .set_supervisor_mode("old", SupervisorMode::Launchd, None)
        .unwrap();
    set_binary_version(&fixture.location.db, "old", Some("0.0.1"));
    let cmux = FakeCmux {
        refuses_detached: true,
        ..FakeCmux::default()
    };
    let launchd = FakeLaunchd::new(&fixture.location.db);
    launchd.load(Some(pid));
    let processes = FakeProcesses::default();

    let error = format!(
        "{:#}",
        try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
    );
    assert!(error.contains(lifecycle::DETACHED_CMUX_HINT), "{error}");
    assert_eq!(cmux.detached_preflights.lock().unwrap().len(), 1);
    assert!(launchd.uninstalls.lock().unwrap().is_empty());
    assert!(launchd.installs.lock().unwrap().is_empty());
    assert!(processes.terminated.lock().unwrap().is_empty());
    assert!(processes.interrupted.lock().unwrap().is_empty());
    let registrations = queue.supervisors().unwrap();
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0].token, "old");
}

/// `--in-cmux` needs the name `[<repo>]supervisor`, and a workspace
/// the replacement will not close holds it: a crashed in-cmux supervisor
/// whose registration an earlier `up` pruned leaves one behind. That has
/// to be found before the drain, or a working supervisor is spent and the
/// queue is left with nothing serving it.
#[test]
fn up_in_cmux_refuses_a_leftover_supervisor_workspace_before_draining() {
    let mut fixture = fixture();
    fixture.options.in_cmux = true;
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    let cmux = FakeCmux::default();
    // Left by a supervisor that is no longer registered at all.
    let orphan = "01234567-89ab-4def-8123-0000000000aa".to_owned();
    cmux.open("[my repo]supervisor", &fixture.repo, &orphan);
    queue
        .register_session_workspace(SessionRole::Supervisor, &orphan)
        .unwrap();
    // The supervisor being replaced runs under launchd, so the drain would
    // close nothing and the name would still be taken.
    queue.register_supervisor("old", pid, 4, VERSION).unwrap();
    queue
        .set_supervisor_mode("old", SupervisorMode::Launchd, None)
        .unwrap();
    set_binary_version(&fixture.location.db, "old", Some("0.0.1"));
    let launchd = FakeLaunchd::new(&fixture.location.db);
    launchd.load(Some(pid));
    let processes = FakeProcesses::default();

    let error = format!(
        "{:#}",
        try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
    );
    assert!(error.contains(&orphan), "{error}");
    assert!(error.contains("cmux workspace close"), "{error}");
    // The working supervisor and its agent are untouched.
    assert!(launchd.uninstalls.lock().unwrap().is_empty());
    assert!(processes.terminated.lock().unwrap().is_empty());
    assert!(processes.interrupted.lock().unwrap().is_empty());
    assert!(cmux.closed.lock().unwrap().is_empty());
    let registrations = queue.supervisors().unwrap();
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0].token, "old");
}

/// `--no-wait` bounds the drain instead of waiting forever: the runs were
/// the reason a drain is long and there were none, but a supervisor can
/// still fail to stop. It errors with what is still registered, and says
/// the stop is already under way.
#[test]
fn up_no_wait_gives_up_on_a_supervisor_that_does_not_stop() {
    let mut fixture = fixture();
    fixture.options.no_wait = true;
    fixture.options.startup_timeout = Duration::from_millis(300);
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    queue
        .register_supervisor("wedged", pid, 4, VERSION)
        .unwrap();
    queue
        .set_supervisor_mode("wedged", SupervisorMode::Launchd, None)
        .unwrap();
    set_binary_version(&fixture.location.db, "wedged", Some("0.0.1"));
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    launchd.load(Some(pid));
    let processes = FakeProcesses::default();

    // Nothing ever removes the registration, the way a loop wedged on a
    // hung cmux or git call keeps its row while its heartbeat runs on.
    let error = format!(
        "{:#}",
        try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
    );
    assert!(error.contains("did not stop within"), "{error}");
    assert!(error.contains("wedged"), "{error}");
    assert!(error.contains(&format!("pid {pid}")), "{error}");
    // It was asked to stop before `up` gave up, so the message tells the
    // operator to come back rather than pretending nothing happened.
    assert_eq!(launchd.uninstalls.lock().unwrap().len(), 1);
    assert!(error.contains("run `up` again"), "{error}");
    assert!(launchd.installs.lock().unwrap().is_empty());
}

/// A supervisor can die with its row intact: launchd's `ExitTimeOut`
/// SIGKILL, or the heartbeat failure the runtime deliberately leaves the
/// row for. The replacement drops those rows, so none is left pointing at
/// the in-cmux workspace it has just closed (the next `down` would retry
/// that close and report `close_failed`).
#[test]
fn up_drops_the_row_of_a_replaced_supervisor_that_died_without_deregistering() {
    let mut fixture = fixture();
    fixture.options.in_cmux = true;
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    // A pid of its own, so killing it does not also kill the supervisor
    // the fake cmux registers (which runs as this process).
    let pid = 424_242;
    let cmux = FakeCmux {
        registers_supervisor_in: Some(fixture.location.db.clone()),
        ..FakeCmux::default()
    };
    let workspace = "01234567-89ab-4def-8123-0000000000bb".to_owned();
    cmux.open("[my repo]supervisor", &fixture.repo, &workspace);
    queue
        .register_supervisor("killed", pid, 2, VERSION)
        .unwrap();
    queue
        .set_supervisor_mode("killed", SupervisorMode::InCmux, Some(&workspace))
        .unwrap();
    set_binary_version(&fixture.location.db, "killed", Some("0.0.1"));
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();

    let report = thread::scope(|scope| {
        scope.spawn(|| {
            wait_until(&processes, pid, || {
                !processes.interrupted.lock().unwrap().is_empty()
            });
            // It dies without removing its own row.
            processes.dead.lock().unwrap().insert(pid);
        });
        up(&fixture, &cmux, &launchd, &processes)
    });
    assert_eq!(report["supervisor"]["outcome"], "restarted", "{report}");
    assert_eq!(
        report["supervisor"]["supervisor_workspaces"],
        json!([{"workspace_id": workspace, "outcome": "closed"}])
    );
    // Only the started supervisor is left; no row points at the workspace
    // this `up` closed.
    let registrations = queue.supervisors().unwrap();
    assert_eq!(registrations.len(), 1, "{registrations:?}");
    assert_ne!(registrations[0].token, "killed");
    assert_eq!(
        registrations[0].workspace_id,
        report["supervisor"]["workspace_id"]
            .as_str()
            .map(str::to_owned)
    );
}

/// What `plan` opens a planner with in these tests: the fixture's Claude
/// Code stub and plugin directory, and a stand-in for this binary.
fn plan_options(fixture: &Fixture) -> lifecycle::PlanOptions {
    let runner = fixture._dir.path().join("dagq-binary");
    fs::write(&runner, "#!/bin/sh\n").unwrap();
    lifecycle::PlanOptions {
        claude: fixture.options.claude.clone(),
        plugin_dir: fixture.options.plugin_dir.clone(),
        runner,
    }
}

fn planners_dir(fixture: &Fixture) -> PathBuf {
    fixture
        .location
        .db
        .canonicalize()
        .unwrap()
        .parent()
        .unwrap()
        .join("planners")
}

/// `plan` opens a new planner workspace on every call, next to the ones
/// already open (ADR-0041 decision 6): each is its own `planners` row with
/// its workspace UUID, a title `[<repo>]planner#<id>`, the planner's role,
/// queue, origin and ID in the workspace's environment, the queue's group,
/// the Blue look without a pin, and a directory holding its prompt and the
/// wrapper binary its workspace runs. A planner whose workspace a person
/// closed is closed in the queue by the next `plan`; `up` opens none.
#[test]
fn plan_opens_a_new_planner_workspace_on_every_call_and_records_each() {
    let fixture = fixture();
    let cmux = FakeCmux::default();
    let options = plan_options(&fixture);
    let db = fixture.location.db.canonicalize().unwrap();
    let root = GitRepository::inspect(&fixture.repo).unwrap().root;
    let hash = fixture.location.hash();

    let first = lifecycle::plan(&fixture.location, &fixture.repo, &cmux, &options).unwrap();
    let second = lifecycle::plan(&fixture.location, &fixture.repo, &cmux, &options).unwrap();
    for (report, id) in [(&first, 1), (&second, 2)] {
        assert_eq!(report["planner"]["id"], id, "{report}");
        assert_eq!(report["planner"]["origin"], "person");
        assert_eq!(report["planner"]["proposal_id"], Value::Null);
        assert_eq!(report["name"], format!("[my repo]planner#{id}"));
        assert_eq!(report["warnings"], json!([]));
        let dir = planners_dir(&fixture).join(id.to_string());
        assert_eq!(report["dir"], json!(dir));
        let prompt = fs::read_to_string(dir.join("prompt.txt")).unwrap();
        assert!(
            prompt.starts_with("You are a planner of the dagq queue at"),
            "{prompt}"
        );
        assert!(dir.join("runner").is_file());
    }
    let first_id = first["planner"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let second_id = second["planner"]["workspace_id"].as_str().unwrap();
    assert_ne!(first_id, second_id);

    let workspaces = cmux.workspaces.lock().unwrap();
    assert_eq!(workspaces.len(), 2);
    let tags = cmux.tags.lock().unwrap();
    for (index, id) in [(0, 1), (1, 2)] {
        let (name, cwd, workspace, command) = &workspaces[index];
        assert_eq!(name, &format!("[my repo]planner#{id}"));
        assert_eq!(cwd, &root);
        let dir = planners_dir(&fixture).join(id.to_string());
        let quoted = |path: &Path| shell_quote(path.to_str().unwrap());
        assert_eq!(
            command,
            &format!(
                "{} '--db' {} 'planner-session' '--planner' '{id}' '--claude' {} '--plugin-dir' {}",
                quoted(&dir.join("runner")),
                quoted(&db),
                quoted(&options.claude),
                quoted(&options.plugin_dir.as_ref().unwrap().canonicalize().unwrap()),
            )
        );
        assert_eq!(
            tags[index],
            WorkspaceTags {
                env: vec![
                    ("DAGQ_ROLE".into(), "planner".into()),
                    ("DAGQ_QUEUE".into(), db.to_str().unwrap().into()),
                    ("DAGQ_PLANNER_ORIGIN".into(), "person".into()),
                    ("DAGQ_PLANNER_ID".into(), id.to_string()),
                ],
                description: Some(format!("dagq role=planner queue={hash} planner={id}")),
                group: Some(format!("group-{hash}")),
            }
        );
        // Blue with the planner's pill, and not pinned: planners come and go.
        assert_eq!(
            cmux.looks_of(workspace),
            [
                ("set-color".to_owned(), "Blue".to_owned()),
                ("set-status".to_owned(), "dagq_role=planner map".to_owned()),
            ]
        );
    }
    drop(tags);
    drop(workspaces);
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let recorded: Vec<Option<String>> = queue
        .planners(false)
        .unwrap()
        .into_iter()
        .map(|planner| planner.workspace_id)
        .collect();
    assert_eq!(
        recorded,
        [Some(first_id.clone()), Some(second_id.to_owned())]
    );
    // No planner is a session workspace of `up`'s.
    assert_eq!(queue.session_workspace(SessionRole::Planner).unwrap(), None);

    // A person closes the first planner; the next `plan` opens a third and
    // gives no record up on a listing that shows one cmux window only.
    cmux.close(&first_id).unwrap();
    let third = lifecycle::plan(&fixture.location, &fixture.repo, &cmux, &options).unwrap();
    assert_eq!(third["planner"]["id"], 3, "{third}");
    assert_eq!(queue.planners(false).unwrap().len(), 3);

    // `up` opens the inbox and leaves the planners alone.
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let report = up(&fixture, &cmux, &launchd, &FakeProcesses::default());
    assert_eq!(report.get("planner"), None, "{report}");
    assert_eq!(queue.planners(false).unwrap().len(), 3);
    assert_eq!(cmux.workspaces.lock().unwrap().len(), 3);
}

/// A planner workspace cmux does not open leaves a closed record with the
/// error, and `plan` fails with it; a missing plugin directory stops
/// `plan` before anything is recorded.
#[test]
fn plan_closes_the_record_of_a_planner_whose_workspace_did_not_open() {
    let fixture = fixture();
    let cmux = FakeCmux {
        create_fails: true,
        ..FakeCmux::default()
    };
    let options = plan_options(&fixture);
    let error = lifecycle::plan(&fixture.location, &fixture.repo, &cmux, &options).unwrap_err();
    assert!(
        format!("{error:#}").contains("workspace create failed"),
        "{error:#}"
    );
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    assert!(queue.planners(false).unwrap().is_empty());
    let all = queue.planners(true).unwrap();
    assert_eq!(all.len(), 1);
    assert!(all[0].closed_at.is_some());
    assert!(
        all[0]
            .error
            .as_deref()
            .unwrap()
            .contains("workspace create failed"),
        "{:?}",
        all[0].error
    );

    let missing = lifecycle::PlanOptions {
        plugin_dir: Some(fixture._dir.path().join("no such plugin")),
        ..options
    };
    let error = lifecycle::plan(
        &fixture.location,
        &fixture.repo,
        &FakeCmux::default(),
        &missing,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("plugin directory"),
        "{error:#}"
    );
    assert_eq!(queue.planners(true).unwrap().len(), 1);
}

/// The runtime opens a planner for a proposal plan review sent back
/// (ADR-0041 decision 12): the proposal's planner is recorded as the
/// runtime's, its title names the proposal, and its first message carries
/// the proposal, its tasks and the reasons. A proposal that does not exist
/// opens nothing.
#[test]
fn the_runtime_opens_a_planner_for_a_proposal_with_its_reasons() {
    use dagq::application::planner::{PlannerLaunch, open_runtime_planner};
    use dagq::domain::{PlannerOrigin, PlannerOwner, ProposalId, Submission};
    let fixture = fixture();
    let cmux = FakeCmux::default();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let task = queue
        .add(NewTask {
            title: "planned change".into(),
            description: "d".into(),
            acceptance: "a".into(),
            verification_commands: vec![],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            dependencies: vec![],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    let proposal = queue
        .submit(Submission {
            tasks: vec![task.id()],
            goals: vec![],
            proposal: None,
            owner: PlannerOwner {
                origin: PlannerOrigin::Person,
                workspace_id: Some("closed-planner".into()),
            },
        })
        .unwrap();
    let task = queue.show(task.id()).unwrap().task;
    let db = fixture.location.db.canonicalize().unwrap();
    let root = GitRepository::inspect(&fixture.repo).unwrap().root;
    let runner = plan_options(&fixture).runner;
    let planners = planners_dir(&fixture);
    let launch = PlannerLaunch {
        queue: &queue,
        cmux: &cmux,
        files: &dagq::infrastructure::run_files::LocalRunFiles,
        db: &db,
        queue_hash: "hash",
        planners_dir: &planners,
        repo_root: &root,
        runner: &runner,
        claude: &fixture.options.claude,
        plugin_dir: None,
    };
    let reasons = vec!["the acceptance is not testable".to_owned()];
    let opened = open_runtime_planner(
        &launch,
        proposal.id(),
        std::slice::from_ref(&task),
        &reasons,
    )
    .unwrap();
    assert_eq!(opened.planner.origin, PlannerOrigin::Runtime);
    assert_eq!(opened.planner.proposal_id, Some(proposal.id()));
    assert_eq!(
        opened.name,
        format!("[my repo]planner#1 - proposal {}", proposal.id())
    );
    let prompt = fs::read_to_string(opened.dir.join("prompt.txt")).unwrap();
    assert!(
        prompt.starts_with(&format!(
            "You are a planner the dagq runtime opened for proposal {}",
            proposal.id()
        )),
        "{prompt}"
    );
    assert!(
        prompt.contains("- the acceptance is not testable"),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("- task {} (submitted): planned change", task.id())),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("`dagq submit --proposal {}`", proposal.id())),
        "{prompt}"
    );
    let tags = cmux.tags.lock().unwrap();
    assert!(
        tags[0]
            .env
            .contains(&("DAGQ_PLANNER_ORIGIN".into(), "runtime".into())),
        "{:?}",
        tags[0]
    );
    let command = &cmux.workspaces.lock().unwrap()[0].3;
    assert!(!command.contains("--plugin-dir"), "{command}");
    drop(tags);
    assert_eq!(
        queue.planner(opened.planner.id).unwrap().proposal_id,
        Some(proposal.id())
    );
    // Nothing to fix and no reasons still makes a prompt that says so.
    let empty =
        dagq::application::prompt::runtime_planner_prompt(&db, proposal.id(), &[], &[]).unwrap();
    assert!(
        empty.contains("(none given)") && empty.contains("(none)"),
        "{empty}"
    );

    let missing = open_runtime_planner(&launch, ProposalId::new(99), &[], &reasons);
    assert!(missing.is_err());
    assert_eq!(queue.planners(true).unwrap().len(), 1);
}

/// Stands in for Claude Code in a planner's workspace: it goes idle the
/// way the `Stop` hook marks it, then exits with `code`.
struct PlannerAgent {
    code: i32,
}

impl dagq::application::AgentProvider for PlannerAgent {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn command(&self, _: &TaskRun, _: &str) -> Result<dagq::application::CommandSpec> {
        bail!("not a run")
    }
    fn resume_command(&self, _: &TaskRun) -> Result<dagq::application::CommandSpec> {
        bail!("not a run")
    }
    fn review_command(&self, _: &TaskRun, _: &str) -> Result<dagq::application::CommandSpec> {
        bail!("not a run")
    }
    fn wait_interval(&self) -> Duration {
        Duration::from_millis(20)
    }
    fn planner_command(
        &self,
        planner: &dagq::application::PlannerCommand<'_>,
    ) -> Result<dagq::application::CommandSpec> {
        assert!(planner.prompt.starts_with("You are a planner of"));
        assert_eq!(planner.plugin_dir, Some(Path::new("/plugins")));
        let marker = planner.idle_marker();
        let mut command = dagq::application::CommandSpec::new("/bin/sh");
        command.current_dir(planner.cwd).arg("-c").arg(format!(
            "sleep 0.2; printf '{{\"hook_event_name\":\"Stop\"}}' > {marker}; exit {code}",
            marker = shell_quote(marker.to_str().unwrap()),
            code = self.code,
        ));
        Ok(command)
    }
}

/// A provider without planner sessions (the default refusal).
struct NoPlanner;

impl dagq::application::AgentProvider for NoPlanner {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn command(&self, _: &TaskRun, _: &str) -> Result<dagq::application::CommandSpec> {
        bail!("not a run")
    }
    fn resume_command(&self, _: &TaskRun) -> Result<dagq::application::CommandSpec> {
        bail!("not a run")
    }
    fn review_command(&self, _: &TaskRun, _: &str) -> Result<dagq::application::CommandSpec> {
        bail!("not a run")
    }
}

/// A planner's session wrapper registers itself and its agent, heartbeats
/// and records the agent's exit, and its state is judged from those, its
/// workspace and the idle marker the agent's `Stop` hook writes, as a
/// worker's is: opening before the wrapper, working, idle, exited, lost
/// when the wrapper is gone without an exit, closed with its workspace.
#[test]
fn a_planner_session_is_judged_alive_and_idle_like_a_worker() {
    use dagq::application::planner::{PlannerProbes, planner_views};
    use dagq::domain::{PlannerId, PlannerState};
    use dagq::infrastructure::{clock::SystemClock, run_files::LocalRunFiles};
    let fixture = fixture();
    let cmux = FakeCmux::default();
    let options = plan_options(&fixture);
    let opened = lifecycle::plan(&fixture.location, &fixture.repo, &cmux, &options).unwrap();
    let id = PlannerId::new(opened["planner"]["id"].as_i64().unwrap());
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let processes = FakeProcesses::default();
    let signals = dagq::infrastructure::adapters::ClaudeCode {
        executable: "claude".into(),
    };
    let planners = planners_dir(&fixture);
    let probes = PlannerProbes {
        cmux: &cmux,
        processes: &processes,
        files: &LocalRunFiles,
        signals: &signals,
        clock: &SystemClock,
        planners_dir: &planners,
    };
    let state = |all: bool| -> Vec<(PlannerState, bool, bool)> {
        planner_views(&queue, &probes, all)
            .unwrap()
            .into_iter()
            .map(|view| (view.state, view.alive, view.idle_since.is_some()))
            .collect()
    };
    assert_eq!(state(false), [(PlannerState::Opening, true, false)]);

    // The wrapper runs the agent, which goes idle and exits.
    let db = fixture.location.db.canonicalize().unwrap();
    let result = dagq::compose::planner_session_with_provider(
        &db,
        id,
        &PlannerAgent { code: 3 },
        Some(Path::new("/plugins")),
    )
    .unwrap();
    assert_eq!(result, json!({"planner_id": 1, "exit_code": 3}));
    let planner = queue.planner(id).unwrap();
    assert_eq!(planner.wrapper_pid, Some(std::process::id()));
    assert!(planner.agent_pid.is_some());
    assert_eq!(planner.exit_code, Some(3));
    assert!(planner.heartbeat_at.is_some());
    assert!(planners.join("1/idle.json").is_file());
    assert_eq!(state(false), [(PlannerState::Exited, false, false)]);
    // An agent that cannot start is recorded as an exit of 127.
    let third = lifecycle::plan(&fixture.location, &fixture.repo, &cmux, &options).unwrap();
    let third_id = PlannerId::new(third["planner"]["id"].as_i64().unwrap());
    let error =
        dagq::compose::planner_session_with_provider(&db, third_id, &NoPlanner, None).unwrap_err();
    assert!(
        format!("{error:#}").contains("no planner session"),
        "{error:#}"
    );
    assert_eq!(queue.planner(third_id).unwrap().exit_code, Some(127));
    queue.close_planner(third_id, None).unwrap();
    // One session per planner.
    assert!(
        dagq::compose::planner_session_with_provider(&db, id, &PlannerAgent { code: 0 }, None)
            .is_err()
    );

    // A live session: working until the Stop hook marks it idle.
    let second = lifecycle::plan(&fixture.location, &fixture.repo, &cmux, &options).unwrap();
    let second_id = PlannerId::new(second["planner"]["id"].as_i64().unwrap());
    queue.register_planner_wrapper(second_id, 4242).unwrap();
    queue.heartbeat_planner(second_id, 4242).unwrap();
    assert_eq!(state(false)[1], (PlannerState::Working, true, false));
    fs::write(
        planners.join(format!("{second_id}/idle.json")),
        r#"{"hook_event_name":"Stop","background_tasks":[]}"#,
    )
    .unwrap();
    assert_eq!(state(false)[1], (PlannerState::Idle, true, true));
    // Background work left running is not idle.
    fs::write(
        planners.join(format!("{second_id}/idle.json")),
        r#"{"hook_event_name":"Stop","background_tasks":[{"status":"running"}]}"#,
    )
    .unwrap();
    assert_eq!(state(false)[1], (PlannerState::Working, true, false));
    // Its wrapper died without recording an exit.
    processes.dead.lock().unwrap().insert(4242);
    assert_eq!(state(false)[1], (PlannerState::Lost, false, false));
    // A person closed its workspace.
    cmux.close(second["planner"]["workspace_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(state(false)[1], (PlannerState::Closed, false, false));
    queue.close_planner(second_id, None).unwrap();
    assert_eq!(state(false).len(), 1);
    assert_eq!(state(true).len(), 3);

    // `planners` reads the same through the real processes.
    let listed = dagq::lifecycle::planners(&fixture.location.db, &cmux, true).unwrap();
    let states: Vec<&str> = listed["planners"]
        .as_array()
        .unwrap()
        .iter()
        .map(|planner| planner["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["exited", "closed", "closed"], "{listed}");
    assert_eq!(listed["planners"][0]["alive"], false);
    assert_eq!(
        listed["planners"][0]["workspace_id"],
        opened["planner"]["workspace_id"]
    );
}

/// A registration of an older build that takes a handoff (ADR-0045
/// decision 10), with a run in flight under its token.
fn handoff_supervisor(fixture: &Fixture, token: &str, mode: SupervisorMode) -> SqliteQueue {
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    queue
        .register_supervisor(token, std::process::id(), 4, "0.0.1")
        .unwrap();
    queue.accept_handoff(token).unwrap();
    queue.set_supervisor_mode(token, mode, None).unwrap();
    queue
}

/// Stand in for the supervisor `token`: once asked, it "execs" by taking
/// its registration back under `version`, the way the exec'd binary does.
fn take_the_handoff(fixture: &Fixture, processes: &FakeProcesses, token: &str, version: &str) {
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    wait_until(processes, std::process::id(), || {
        queue.handoff_request(token).unwrap().is_some()
    });
    assert_eq!(
        queue.handoff_request(token).unwrap().as_deref(),
        Some("/opt/bin/dagq")
    );
    queue
        .resume_registration(token, std::process::id(), version)
        .unwrap();
}

/// `up` hands a supervisor of another build that takes a handoff over to
/// this binary instead of draining it (ADR-0045 decision 15): nothing is
/// signalled, no agent or workspace is touched, the run in flight keeps
/// its lease, and the report names the supervisor that is now this build
/// under the same token and pid. `--no-wait` changes nothing here.
#[test]
fn up_hands_a_supervisor_of_another_build_over_without_draining_it() {
    for no_wait in [false, true] {
        let mut fixture = fixture();
        fixture.options.in_cmux = true;
        fixture.options.no_wait = no_wait;
        let mut queue = handoff_supervisor(&fixture, "old", SupervisorMode::InCmux);
        let run_id = claim_a_run(&fixture, &mut queue, "old");
        let cmux = FakeCmux::default();
        let launchd = FakeLaunchd::new(&fixture.location.db);
        let processes = FakeProcesses::default();

        let report = thread::scope(|scope| {
            scope.spawn(|| take_the_handoff(&fixture, &processes, "old", VERSION));
            up(&fixture, &cmux, &launchd, &processes)
        });
        let supervisor = &report["supervisor"];
        assert_eq!(supervisor["outcome"], "restarted", "{report}");
        assert_eq!(supervisor["handoff"], true);
        assert_eq!(supervisor["token"], "old");
        assert_eq!(supervisor["pid"], json!(std::process::id()));
        assert_eq!(supervisor["mode"], "in_cmux");
        assert_eq!(supervisor["version"], VERSION);
        assert_eq!(supervisor["previous_version"], "0.0.1");
        assert_eq!(supervisor["replaced"][0]["version"], "0.0.1");
        assert_eq!(report["migrated"], Value::Null);
        assert!(processes.terminated.lock().unwrap().is_empty());
        assert!(processes.interrupted.lock().unwrap().is_empty());
        assert!(launchd.uninstalls.lock().unwrap().is_empty());
        assert!(launchd.installs.lock().unwrap().is_empty());
        assert!(cmux.closed.lock().unwrap().is_empty());
        let registrations = queue.supervisors().unwrap();
        assert_eq!(registrations.len(), 1);
        assert_eq!(registrations[0].binary_version.as_deref(), Some(VERSION));
        assert_eq!(registrations[0].handoff_binary, None);
        let leases = queue.run_leases().unwrap();
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].run_id.as_str(), run_id);
        assert_eq!(leases[0].token, "old");
    }
}

/// A supervisor that comes back under its old build (the exec failed and
/// it went on) or stops heartbeating mid-handoff fails `up` with what
/// happened; one that never picks the request up fails it at the timeout.
#[test]
fn up_reports_a_handoff_that_did_not_happen() {
    let fixture = fixture();
    let queue = handoff_supervisor(&fixture, "old", SupervisorMode::Launchd);
    // The agent starts this binary's path, so a launchd supervisor is
    // handed over rather than drained.
    let launchd = FakeLaunchd::new(&fixture.location.db);
    fs::create_dir_all(fixture.location.launch_agent.parent().unwrap()).unwrap();
    fs::write(
        &fixture.location.launch_agent,
        "<key>ProgramArguments</key>\n\t<array>\n\t\t<string>/opt/bin/dagq</string>\n",
    )
    .unwrap();
    let cmux = FakeCmux::default();
    let processes = FakeProcesses::default();
    let error = thread::scope(|scope| {
        scope.spawn(|| take_the_handoff(&fixture, &processes, "old", "0.0.1"));
        format!(
            "{:#}",
            try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
        )
    });
    assert!(error.contains("came back as 0.0.1"), "{error}");
    assert!(
        error.contains("the exec of /opt/bin/dagq failed"),
        "{error}"
    );

    // Nobody takes the request, and the supervisor stops heartbeating.
    let mut fixture = fixture;
    fixture.options.handoff_timeout = Duration::from_millis(200);
    let error = format!(
        "{:#}",
        try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
    );
    assert!(error.contains("did not take the handoff"), "{error}");
    // The request is withdrawn, so the supervisor does not exec that path later.
    assert_eq!(queue.handoff_request("old").unwrap(), None);
    Connection::open(&fixture.location.db)
        .unwrap()
        .execute(
            "UPDATE supervisors SET heartbeat_at=unixepoch()-60, binary_version='0.0.2'",
            [],
        )
        .unwrap();
    queue.request_handoff("old", "/opt/bin/dagq").unwrap();
    let registration = queue.supervisors().unwrap().remove(0);
    let error = format!(
        "{:#}",
        lifecycle::hand_off(
            &queue,
            &processes,
            &dagq::infrastructure::clock::SystemClock,
            &[registration],
            Path::new("/opt/bin/dagq"),
            VERSION,
            Duration::from_secs(5),
            Duration::from_millis(20),
        )
        .unwrap_err()
    );
    assert!(error.contains("stopped heartbeating"), "{error}");
    queue.deregister_supervisor("old").unwrap();
    let error = format!(
        "{:#}",
        lifecycle::hand_off(
            &queue,
            &processes,
            &dagq::infrastructure::clock::SystemClock,
            &[gone_registration()],
            Path::new("/opt/bin/dagq"),
            VERSION,
            Duration::from_secs(5),
            Duration::from_millis(20),
        )
        .unwrap_err()
    );
    assert!(error.contains("cannot take a handoff"), "{error}");
}

fn gone_registration() -> dagq::domain::SupervisorRegistration {
    dagq::domain::SupervisorRegistration {
        token: "gone".into(),
        pid: 1,
        parallel: 1,
        started_at: 0,
        heartbeat_at: 0,
        mode: None,
        workspace_id: None,
        binary_version: None,
        handoff_accepted: true,
        handoff_binary: None,
    }
}

/// A supervisor that cannot take a handoff (a binary before ADR-0045), or
/// a launchd one whose agent starts another binary, is drained as before.
#[test]
fn up_drains_a_supervisor_whose_agent_starts_another_binary() {
    let fixture = fixture();
    let queue = handoff_supervisor(&fixture, "old", SupervisorMode::Launchd);
    fs::create_dir_all(fixture.location.launch_agent.parent().unwrap()).unwrap();
    fs::write(
        &fixture.location.launch_agent,
        "<key>ProgramArguments</key>\n\t<array>\n\t\t<string>/elsewhere/dagq</string>\n",
    )
    .unwrap();
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    launchd.load(Some(std::process::id()));
    let processes = FakeProcesses::default();
    let report = thread::scope(|scope| {
        scope.spawn(|| {
            wait_until(&processes, std::process::id(), || {
                !launchd.uninstalls.lock().unwrap().is_empty()
            });
            SqliteQueue::open(&fixture.location.db)
                .unwrap()
                .deregister_supervisor("old")
                .unwrap();
        });
        up(&fixture, &cmux, &launchd, &processes)
    });
    assert_eq!(report["supervisor"]["outcome"], "restarted", "{report}");
    assert_eq!(report["supervisor"].get("handoff"), None, "{report}");
    assert_eq!(queue.handoff_request("old").unwrap(), None);
}

/// `up` applies the queue's pending migrations first only when every one
/// of them is compatible (ADR-0045 decision 15), and refuses a breaking one
/// with the way to it, before it starts or touches anything. The last
/// migration (0032) is breaking, so a queue before it is refused even when
/// the migration it lacks first (0031) is compatible.
#[test]
fn up_applies_compatible_migrations_and_refuses_breaking_ones() {
    let fixture = fixture();
    let db = &fixture.location.db;
    // The queue as the binary before the handoff columns left it.
    Connection::open(db)
        .unwrap()
        .execute_batch(
            "ALTER TABLE supervisors DROP COLUMN handoff_accepted;
             ALTER TABLE supervisors DROP COLUMN handoff_binary;
             ALTER TABLE supervisors DROP COLUMN handoff_requested_at;
             PRAGMA user_version = 30;",
        )
        .unwrap();
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(db);
    let processes = FakeProcesses::default();
    let error = format!(
        "{:#}",
        try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
    );
    assert!(error.contains("breaking migration(s) 32"), "{error}");
    let version: i64 = Connection::open(db)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 30);
    // Migrated, it starts.
    SqliteQueue::migrate(db, None, 0).unwrap();
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(report["migrated"], Value::Null, "{report}");

    Connection::open(db)
        .unwrap()
        .execute_batch("PRAGMA user_version = 24;")
        .unwrap();
    let error = format!(
        "{:#}",
        try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
    );
    assert!(error.contains("breaking migration(s) 25"), "{error}");
    assert!(error.contains("install --allow-breaking"), "{error}");
}

/// [`Binaries`] that build, run and move nothing: what each call was, the
/// schema it reports, and whether it answers a probe.
struct FakeBinaries {
    schema: dagq::application::install::SchemaCheck,
    calls: Mutex<Vec<String>>,
    up: Mutex<Vec<Vec<String>>>,
}

impl FakeBinaries {
    fn new(pending: &[(i64, bool)], opens: bool) -> Self {
        Self {
            schema: dagq::application::install::SchemaCheck {
                pending: pending
                    .iter()
                    .map(
                        |&(version, compatible)| dagq::application::install::PendingMigration {
                            version,
                            compatible,
                        },
                    )
                    .collect(),
                opens,
            },
            calls: Mutex::default(),
            up: Mutex::default(),
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn note(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
}

impl dagq::application::install::Binaries for FakeBinaries {
    fn build(&self, checkout: &Path) -> Result<PathBuf> {
        self.note(format!("build {}", checkout.display()));
        Ok(checkout.join("target/release/dagq"))
    }
    fn version(&self, binary: &Path) -> Result<String> {
        Ok(if binary.ends_with("dagq.previous") {
            "0.0.1".into()
        } else {
            VERSION.into()
        })
    }
    fn probe(&self, binary: &Path) -> Result<()> {
        self.note(format!("probe {}", binary.display()));
        Ok(())
    }
    fn takes_handoff(&self, binary: &Path) -> bool {
        !binary.ends_with("old/dagq")
    }
    fn schema(&self, _: &Path, _: &Path) -> Result<dagq::application::install::SchemaCheck> {
        Ok(self.schema.clone())
    }
    fn migrate(&self, binary: &Path, _: &Path) -> Result<Value> {
        self.note(format!("migrate {}", binary.display()));
        Ok(json!({"applied": self.schema.pending.len()}))
    }
    fn replace(&self, source: &Path, target: &Path) -> Result<()> {
        self.note(format!("replace {} {}", source.display(), target.display()));
        Ok(())
    }
    fn restore(&self, target: &Path) -> Result<()> {
        self.note(format!("restore {}", target.display()));
        Ok(())
    }
    fn run(&self, binary: &Path, arguments: &[String]) -> Result<Value> {
        self.note(format!("run {}", binary.display()));
        self.up.lock().unwrap().push(arguments.to_vec());
        Ok(json!({"supervisor": {"outcome": "started"}}))
    }
}

fn install_with(
    fixture: &Fixture,
    binaries: &FakeBinaries,
    processes: &FakeProcesses,
    down: &dyn Fn() -> Result<Value>,
    options: &dagq::application::install::InstallOptions,
) -> Result<Value> {
    let queues = |db: &Path| -> std::sync::Arc<dyn dagq::application::QueueOpener> {
        std::sync::Arc::new(dagq::infrastructure::runtime_store::SqliteOpener {
            db: db.to_owned(),
            generators: dagq::infrastructure::clock::system(),
        })
    };
    dagq::application::install::install(
        &dagq::application::install::Ports {
            binaries,
            files: &dagq::infrastructure::run_files::LocalRunFiles,
            processes,
            clock: &dagq::infrastructure::clock::SystemClock,
            queues: &queues,
            down,
        },
        Some(&fixture.location.db),
        options,
    )
}

fn install_options(
    source: dagq::application::install::Source,
) -> dagq::application::install::InstallOptions {
    dagq::application::install::InstallOptions {
        source,
        target: "/opt/bin/dagq".into(),
        allow_breaking: false,
        restart: vec!["--cmux".into(), "/opt/cmux".into()],
        handoff_timeout: Duration::from_secs(5),
        poll: Duration::from_millis(20),
    }
}

/// `install` builds the checkout, probes the build, applies compatible
/// migrations with it, puts it in place and hands the live supervisors that
/// take a handoff over to it; one of an older binary is left for `up` to
/// drain. A handoff that fails puts the replaced binary back.
#[test]
fn install_migrates_replaces_and_hands_over_and_restores_on_a_failed_handoff() {
    use dagq::application::install::Source;
    let fixture = fixture();
    let queue = handoff_supervisor(&fixture, "new", SupervisorMode::InCmux);
    let mut old = SqliteQueue::open(&fixture.location.db).unwrap();
    old.register_supervisor("older", 424_243, 1, "0.0.1")
        .unwrap();
    let binaries = FakeBinaries::new(&[(27, true)], false);
    let processes = FakeProcesses::default();
    let no_down = || -> Result<Value> { panic!("no drain for compatible migrations") };
    let report = thread::scope(|scope| {
        scope.spawn(|| {
            let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
            wait_until(&processes, std::process::id(), || {
                queue.handoff_request("new").unwrap().is_some()
            });
            queue
                .resume_registration("new", std::process::id(), VERSION)
                .unwrap();
        });
        install_with(
            &fixture,
            &binaries,
            &processes,
            &no_down,
            &install_options(Source::Checkout("/src/dagq".into())),
        )
        .unwrap()
    });
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(report["migrated"], json!({"applied": 1}));
    assert_eq!(report["supervisors"][0]["token"], "new");
    assert_eq!(report["not_handed_off"][0]["token"], "older");
    assert_eq!(report["previous"], "/opt/bin/dagq.previous");
    assert_eq!(
        binaries.calls(),
        [
            "build /src/dagq",
            "probe /src/dagq/target/release/dagq",
            "migrate /src/dagq/target/release/dagq",
            "replace /src/dagq/target/release/dagq /opt/bin/dagq",
        ]
    );

    // Nobody takes this one: the handoff times out and the binary is put back.
    let binaries = FakeBinaries::new(&[], true);
    let mut options = install_options(Source::Binary("/built/dagq".into()));
    options.handoff_timeout = Duration::from_millis(200);
    let error = format!(
        "{:#}",
        install_with(&fixture, &binaries, &processes, &no_down, &options).unwrap_err()
    );
    assert!(error.contains("the handoff to"), "{error}");
    assert!(error.contains("is back at /opt/bin/dagq"), "{error}");
    assert_eq!(queue.handoff_request("new").unwrap(), None);

    // A binary that predates the handoff would end the supervisor it is
    // exec'd in: nothing is replaced.
    let older = FakeBinaries::new(&[], true);
    let error = format!(
        "{:#}",
        install_with(
            &fixture,
            &older,
            &processes,
            &no_down,
            &install_options(Source::Binary("/old/dagq".into())),
        )
        .unwrap_err()
    );
    assert!(error.contains("predates the handoff"), "{error}");
    assert_eq!(older.calls(), ["probe /old/dagq"]);
    assert_eq!(
        binaries.calls(),
        [
            "probe /built/dagq",
            "replace /built/dagq /opt/bin/dagq",
            "restore /opt/bin/dagq",
        ]
    );
    drop(queue);
}

/// A build with a breaking migration is refused without `--allow-breaking`
/// and replaces nothing; with it, the supervisor is drained, the queue
/// migrated, the binary replaced and `up` run with the drained supervisor's
/// mode and parallelism. A rollback past a breaking migration is refused,
/// and so is one without a previous binary.
#[test]
fn install_drains_only_for_a_breaking_migration_when_allowed() {
    use dagq::application::install::Source;
    let fixture = fixture();
    let _queue = handoff_supervisor(&fixture, "live", SupervisorMode::InCmux);
    let processes = FakeProcesses::default();
    let binaries = FakeBinaries::new(&[(27, true), (28, false)], false);
    let drained = Mutex::new(0);
    let down = || -> Result<Value> {
        *drained.lock().unwrap() += 1;
        Ok(json!({"outcome": "stopped"}))
    };
    let error = format!(
        "{:#}",
        install_with(
            &fixture,
            &binaries,
            &processes,
            &down,
            &install_options(Source::Binary("/built/dagq".into())),
        )
        .unwrap_err()
    );
    assert!(error.contains("breaking migration(s) 28"), "{error}");
    assert!(error.contains("--allow-breaking"), "{error}");
    assert_eq!(binaries.calls(), ["probe /built/dagq"]);
    assert_eq!(*drained.lock().unwrap(), 0);

    let mut options = install_options(Source::Binary("/built/dagq".into()));
    options.allow_breaking = true;
    let report = install_with(&fixture, &binaries, &processes, &down, &options).unwrap();
    assert_eq!(*drained.lock().unwrap(), 1);
    assert_eq!(report["drained"]["outcome"], "stopped", "{report}");
    assert_eq!(report["up"]["supervisor"]["outcome"], "started");
    assert_eq!(
        binaries.calls()[2..],
        [
            "migrate /built/dagq",
            "replace /built/dagq /opt/bin/dagq",
            "run /opt/bin/dagq",
        ]
    );
    let db = fixture.location.db.to_str().unwrap();
    assert_eq!(
        binaries.up.lock().unwrap()[0],
        [
            "--db",
            db,
            "up",
            "--parallel",
            "4",
            "--in-cmux",
            "--cmux",
            "/opt/cmux"
        ]
    );

    let binaries = FakeBinaries::new(&[], false);
    let dir = tempfile::tempdir().unwrap();
    let mut options = install_options(Source::Rollback);
    options.target = dir.path().join("dagq");
    let error = format!(
        "{:#}",
        install_with(&fixture, &binaries, &processes, &down, &options).unwrap_err()
    );
    assert!(error.contains("no previous binary"), "{error}");
    fs::write(dir.path().join("dagq.previous"), "").unwrap();
    let error = format!(
        "{:#}",
        install_with(&fixture, &binaries, &processes, &down, &options).unwrap_err()
    );
    assert!(error.contains("the queue refuses 0.0.1"), "{error}");
    assert_eq!(
        dagq::application::install::parse_version("dagq 1.2.3\n").unwrap(),
        "1.2.3"
    );
    assert!(dagq::application::install::parse_version("").is_err());
}
