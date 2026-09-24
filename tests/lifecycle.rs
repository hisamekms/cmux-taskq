//! `up` and `down` against fakes for launchd, cmux and process signals: the
//! idempotent start in either mode (launchd or `--in-cmux`), the
//! out-of-cmux connection preflight, the pruning of dead registrations, the
//! inbox and planner workspace decisions, and every `down` outcome. The real
//! launchd and cmux path is `tests/e2e.rs`.
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
        inbox_command, planner_command,
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
        .output()
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
        let mut workspaces = self.workspaces.lock().unwrap();
        let id = format!("01234567-89ab-4def-8123-{:012x}", workspaces.len());
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
            dependencies: vec![],
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    queue.transition(task.id(), TaskAction::Ready).unwrap();
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
    // The resident sessions are the inbox and the planner (ADR-0024
    // decision 6); the report names no other.
    let keys: Vec<&String> = first.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "doctor",
            "inbox",
            "planner",
            "pruned_supervisors",
            "supervisor",
            "warnings"
        ]
    );
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
    assert_eq!(workspaces.len(), 2);
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let tags = cmux.tags.lock().unwrap();
    for (index, (key, role, opening)) in [
        ("inbox", SessionRole::Inbox, "You are the inbox of"),
        ("planner", SessionRole::Planner, "You are the planner of"),
    ]
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
    for key in ["inbox", "planner"] {
        assert_eq!(second[key]["outcome"], "reused", "{second}");
        assert_eq!(second[key]["workspace_id"], first[key]["workspace_id"]);
        assert_eq!(second[key]["name"], first[key]["name"]);
    }
    assert_eq!(second["pruned_supervisors"], json!([]));
    assert_eq!(launchd.installs.lock().unwrap().len(), 1);
    assert!(launchd.uninstalls.lock().unwrap().is_empty());
    assert_eq!(cmux.workspaces.lock().unwrap().len(), 2);
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
    // The password is in no session's command: the inbox and planner
    // sessions are a cmux terminal's children and need none.
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
            dependencies: vec![],
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    queue.transition(task.id(), TaskAction::Ready).unwrap();
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
                dependencies: vec![],
                goal_id: None,
                context: String::new(),
            })
            .unwrap();
        queue.transition(task.id(), TaskAction::Ready).unwrap();
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
/// resident session ADR-0024 retired) is forgotten by `up`, which opens only
/// the inbox and the planner; the workspace itself is left open for a
/// person to close. A session of a role `up` does not open (a worker's)
/// skips nothing.
#[test]
fn up_forgets_a_retired_session_workspace_and_opens_only_the_inbox_and_the_planner() {
    let mut fixture = fixture();
    fixture.environment.role = Some("worker".into());
    fixture.environment.queue = Some(fixture.location.db.clone());
    let cmux = FakeCmux::default();
    let retired = "01234567-89ab-4def-8123-0000000000ee";
    cmux.open("[my repo]retired", &fixture.repo, retired);
    let raw = Connection::open(&fixture.location.db).unwrap();
    raw.execute(
        "INSERT INTO session_workspaces(role,workspace_id) VALUES ('retired',?1)",
        [retired],
    )
    .unwrap();
    drop(raw);
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started");
    assert_eq!(report["inbox"]["outcome"], "created", "{report}");
    assert_eq!(report["planner"]["outcome"], "created", "{report}");
    assert_eq!(report.get("retired"), None);
    let names: Vec<String> = cmux
        .workspaces
        .lock()
        .unwrap()
        .iter()
        .map(|workspace| workspace.0.clone())
        .collect();
    assert_eq!(
        names,
        ["[my repo]retired", "[my repo]inbox", "[my repo]planner"]
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
    assert_eq!(roles, ["inbox", "planner"]);
    // Forgetting is idempotent.
    assert_eq!(
        SqliteQueue::open(&fixture.location.db)
            .unwrap()
            .forget_retired_session_workspaces()
            .unwrap(),
        0
    );
}

/// Inside the inbox or the planner session of this queue, `up` skips that
/// one workspace and still opens (or reuses) the others; the role of a
/// session of another queue does not count.
#[test]
fn up_skips_the_inbox_and_the_planner_inside_their_own_sessions() {
    for (role, key, other) in [
        (INBOX_ROLE, "inbox", "planner"),
        (PLANNER_ROLE, "planner", "inbox"),
    ] {
        let mut fixture = fixture();
        fixture.environment.role = Some(role.into());
        fixture.environment.queue = Some(fixture.location.db.clone());
        let cmux = FakeCmux::default();
        let launchd = FakeLaunchd::new(&fixture.location.db);
        let processes = FakeProcesses::default();
        let report = up(&fixture, &cmux, &launchd, &processes);
        assert_eq!(
            report[key],
            json!({"outcome": "skipped", "workspace_id": null, "name": format!("[my repo]{key}")})
        );
        assert_eq!(report[other]["outcome"], "created", "{report}");
        let queue = SqliteQueue::open(&fixture.location.db).unwrap();
        let session_role = |name: &str| match name {
            "inbox" => SessionRole::Inbox,
            _ => SessionRole::Planner,
        };
        assert_eq!(queue.session_workspace(session_role(key)).unwrap(), None);
        assert!(
            queue
                .session_workspace(session_role(other))
                .unwrap()
                .is_some()
        );
        assert!(
            cmux.workspaces
                .lock()
                .unwrap()
                .iter()
                .all(|workspace| workspace.0 != format!("[my repo]{key}"))
        );

        // A second `up` from the same session reuses the others and still
        // skips its own.
        let second = up(&fixture, &cmux, &launchd, &processes);
        assert_eq!(second[key]["outcome"], "skipped", "{second}");
        assert_eq!(second[other]["outcome"], "reused", "{second}");
        assert_eq!(cmux.workspaces.lock().unwrap().len(), 1);

        // From a session of that role in another queue, this queue's
        // workspace is opened.
        fixture.environment.queue = Some(fixture._dir.path().join("elsewhere.db"));
        let third = up(&fixture, &cmux, &launchd, &processes);
        assert_eq!(third[key]["outcome"], "created", "{third}");
        assert_eq!(cmux.workspaces.lock().unwrap().len(), 2);
    }
}

/// An inbox or planner workspace that was closed is forgotten and opened
/// again under a new UUID, and `down` closes neither session's workspace,
/// only the supervisor's.
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
    assert_eq!(second["planner"]["outcome"], "reused", "{second}");
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
    assert_eq!(names, ["[my repo]planner", "[my repo]inbox"]);
    for role in [SessionRole::Inbox, SessionRole::Planner] {
        assert!(queue.session_workspace(role).unwrap().is_some(), "{role:?}");
    }
}

/// The recorded planner workspace is the one `up` reuses, whatever it is
/// called; one cmux no longer has is forgotten and opened again, and a
/// workspace that merely carries the planner's title is not taken for it.
#[test]
fn up_opens_the_planner_again_when_its_recorded_workspace_is_gone() {
    let fixture = fixture();
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let first = up(&fixture, &cmux, &launchd, &processes);
    let id = first["planner"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    cmux.close(&id).unwrap();
    // Someone opened a workspace with the planner's title by hand.
    cmux.open(
        "[my repo]planner",
        &fixture.repo,
        "01234567-89ab-4def-8123-0000000000dd",
    );

    let second = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(second["planner"]["outcome"], "created", "{second}");
    assert_eq!(second["inbox"]["outcome"], "reused", "{second}");
    let reopened = second["planner"]["workspace_id"].as_str().unwrap();
    assert_ne!(reopened, id);
    assert_ne!(reopened, "01234567-89ab-4def-8123-0000000000dd");
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    assert_eq!(
        queue
            .session_workspace(SessionRole::Planner)
            .unwrap()
            .as_deref(),
        Some(reopened)
    );
    // The group is asked for again by the same external ID; cmux returns
    // the same group.
    let groups = cmux.groups.lock().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0], groups[1]);
    drop(groups);
    assert!(
        queue
            .remove_session_workspace(SessionRole::Planner)
            .unwrap()
    );
    assert!(
        !queue
            .remove_session_workspace(SessionRole::Planner)
            .unwrap()
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
        fn capture(&self, _: &str) -> Result<String> {
            unreachable!()
        }
        fn close(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn send_exit(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn exists(&self, _: &str) -> Result<bool> {
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
    assert_eq!(first["planner"]["outcome"], "created");

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
    assert_eq!(workspaces.len(), 3, "{workspaces:?}");
    let (name, cwd, id, command) = &workspaces[0];
    assert_eq!(name, "[my repo]supervisor");
    assert_eq!(cwd, &root);
    assert_eq!(first["supervisor"]["workspace_id"], json!(id));
    let db = fixture.location.db.canonicalize().unwrap();
    let quoted = |path: &Path| shell_quote(path.to_str().unwrap());
    assert_eq!(
        command,
        &format!(
            "'/opt/bin/dagq' '--db' {} 'supervise' '--parallel' '2' '--log-dir' {} '--cmux' {} '--claude' {}",
            quoted(&db),
            quoted(&fixture.location.log_dir),
            quoted(&fixture.options.cmux),
            quoted(&fixture.options.claude),
        )
    );
    // The fixture's queue directory has an apostrophe: cmux types this
    // into a login shell, so every argument is quoted on its own.
    assert!(command.contains(r#"queue'"'"'s dir"#), "{command}");
    assert_eq!(workspaces[1].0, "[my repo]inbox");
    assert_eq!(workspaces[2].0, "[my repo]planner");
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
    assert_eq!(second["planner"]["outcome"], "reused", "{second}");
    assert_eq!(cmux.workspaces.lock().unwrap().len(), 3);
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
    drain.join().unwrap();
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
    assert!(planner.starts_with("You are the planner of the dagq queue at /data/q/queue.db:"));
    assert!(planner.lines().count() <= 5, "{planner}");
    assert!(planner.contains("the person's problems"));
    assert!(planner.contains("dagq-planner skill"));
    assert!(planner.contains("dagq skill describes"));
    assert!(planner.contains("You do not land runs or answer asks"));
    assert!(planner.contains("make the tasks ready"));
    assert!(planner.contains("check their receipts against the goal's acceptance"));
    assert!(planner.contains("`dagq goal close ID --verdict achieved`"));
    // Observer notes and draft goals are a later goal's; until then the
    // prompt says nothing about them.
    assert!(!planner.contains("note"), "{planner}");
    assert!(!planner.contains("draft"), "{planner}");

    for (command, opening) in [
        (
            inbox_command(db, Path::new("/opt/claude"), Some(Path::new("/p"))).unwrap(),
            "You are the inbox of",
        ),
        (
            planner_command(db, Path::new("/opt/claude"), None).unwrap(),
            "You are the planner of",
        ),
    ] {
        assert!(command.starts_with("'/opt/claude' '"), "{command}");
        assert!(command.contains(&format!("'--' '{opening}")), "{command}");
    }
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
/// place (ADR-0014). A registration older than the `binary_version` column
/// has no version at all, which is not this one either, so it is replaced
/// the same way.
#[test]
fn up_drains_and_replaces_a_launchd_supervisor_of_another_version() {
    for previous in [Some("0.0.1"), None] {
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
