//! The fixture and the fakes for launchd, cmux and process signals for the
//! lifecycle tests (`tests/lifecycle_*.rs`).

use super::Bounded;

use anyhow::{Result, bail};
use dagq::{
    VERSION,
    application::{
        AgentState, DetachedRefusal, LaunchAgent, ProcessControl, SupervisorEnvironment,
        WorkspaceBackend, WorkspaceTags,
    },
    domain::{SupervisorMode, Task, TaskRun},
    infrastructure::{location::QueueLocation, sqlite::SqliteQueue},
    lifecycle::{self, DownOptions, UpEnvironment, UpOptions},
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
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

/// Disposable repository, queue, home and stub `claude`, all under one temp dir.
pub struct Fixture {
    pub _dir: TempDir,
    pub repo: PathBuf,
    pub location: QueueLocation,
    pub environment: UpEnvironment,
    pub options: UpOptions,
    /// Times the test while held (task 324).
    pub _test: super::Waiting,
}

pub fn fixture() -> Fixture {
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
        _test: super::test(),
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
            auto_update: false,
            poll: Duration::from_millis(20),
        },
        _dir: dir,
    }
}

/// Records installs and uninstalls; an install registers a supervisor with
/// this process's PID, the way a started `supervise` would within seconds.
pub struct FakeLaunchd {
    pub db: PathBuf,
    pub registers_on_install: bool,
    pub loaded: Mutex<bool>,
    /// The agent's process while loaded, as `launchctl print` would show it.
    pub agent_pid: Mutex<Option<u32>>,
    pub installs: Mutex<Vec<(String, PathBuf, String)>>,
    pub uninstalls: Mutex<Vec<(String, PathBuf)>>,
}

impl FakeLaunchd {
    pub fn new(db: &Path) -> Self {
        Self {
            db: db.into(),
            registers_on_install: true,
            loaded: Mutex::new(false),
            agent_pid: Mutex::new(None),
            installs: Mutex::new(Vec::new()),
            uninstalls: Mutex::new(Vec::new()),
        }
    }
    pub fn load(&self, pid: Option<u32>) {
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
pub struct FakeProcesses {
    pub dead: Mutex<HashSet<u32>>,
    pub terminated: Mutex<Vec<u32>>,
    pub interrupted: Mutex<Vec<u32>>,
    pub killed: Mutex<Vec<u32>>,
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
pub struct FakeCmux {
    pub calls: AtomicUsize,
    pub workspaces: Mutex<Vec<(String, PathBuf, String, String)>>,
    pub refuses_detached: bool,
    /// The ping could not be run at all (not a refusal).
    pub detached_unreachable: bool,
    pub detached_preflights: Mutex<Vec<SupervisorEnvironment>>,
    pub closed: Mutex<Vec<String>>,
    /// Queue a `[…]supervisor` workspace registers a supervisor in,
    /// the way the `supervise` cmux runs in its terminal would.
    pub registers_supervisor_in: Option<PathBuf>,
    /// The tags each workspace was opened with, in `workspaces` order.
    pub tags: Mutex<Vec<WorkspaceTags>>,
    /// Every `ensure_group` call, as (external ID, name).
    pub groups: Mutex<Vec<(String, String)>>,
    /// `workspace-group create` fails.
    pub group_fails: bool,
    /// Every color, status pill and pin call, as (call, workspace, what).
    pub looks: Mutex<Vec<(String, String, String)>>,
    /// cmux refuses every color, status pill and pin call.
    pub look_fails: bool,
    /// `workspace create` fails.
    pub create_fails: bool,
    /// Workspaces created so far, so a UUID is never handed out twice.
    pub created: AtomicUsize,
}

impl FakeCmux {
    /// Rename a workspace the way a person would in cmux's sidebar.
    pub fn rename(&self, id: &str, title: &str) {
        for workspace in self.workspaces.lock().unwrap().iter_mut() {
            if workspace.2 == id {
                workspace.0 = title.into();
            }
        }
    }

    pub fn look(&self, call: &str, id: &str, what: String) -> Result<()> {
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
    pub fn looks_of(&self, id: &str) -> Vec<(String, String)> {
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
    pub fn open(&self, name: &str, cwd: &Path, id: &str) {
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

pub fn up(
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

pub fn try_up(
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

/// Poll `condition` until it holds. On the deadline the supervisor's pid
/// is marked dead before the panic: `up`'s drain loop is otherwise
/// unbounded, so a helper thread that merely panicked would leave the main
/// thread waiting for a registration nothing is going to remove, and
/// `thread::scope` would never join.
pub fn wait_until(processes: &FakeProcesses, pid: u32, condition: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !condition() {
        if std::time::Instant::now() >= deadline {
            processes.dead.lock().unwrap().insert(pid);
            panic!("the condition never held; released the drain so the test can fail");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// The mode recorded on a registration, read straight from the queue.
pub fn remaining_mode(queue: &SqliteQueue, token: &str) -> Option<SupervisorMode> {
    queue
        .supervisors()
        .unwrap()
        .into_iter()
        .find(|registration| registration.token == token)
        .and_then(|registration| registration.mode)
}

pub fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// The `backend_call_failed` events of the fixture's queue, oldest first.
pub fn backend_failures(fixture: &Fixture) -> Vec<dagq::domain::RunEvent> {
    SqliteQueue::open(&fixture.location.db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "backend_call_failed")
        .collect()
}

pub fn down(
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

/// A registration of an older build that takes a handoff (ADR-0045
/// decision 10), with a run in flight under its token.
pub fn handoff_supervisor(fixture: &Fixture, token: &str, mode: SupervisorMode) -> SqliteQueue {
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    queue
        .register_supervisor(token, std::process::id(), 4, "0.0.1")
        .unwrap();
    queue.accept_handoff(token).unwrap();
    queue.set_supervisor_mode(token, mode, None).unwrap();
    queue
}
