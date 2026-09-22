//! `up` and `down` against fakes for launchd, cmux and process signals: the
//! idempotent start, the out-of-cmux connection preflight, the pruning of
//! dead registrations, the maintainer workspace decision, and every `down`
//! outcome. The real launchd and cmux path is `tests/e2e.rs`.
use anyhow::{Result, bail};
use cmux_taskq::{
    application::{
        AgentState, DetachedRefusal, LaunchAgent, ProcessControl, SupervisorEnvironment, TaskQueue,
        WorkspaceBackend,
    },
    domain::{NewTask, TaskAction, TaskRun},
    infrastructure::{
        adapters::{Cmux, GitRepository, SOCKET_PASSWORD_ENV, detach, process_alive},
        location::QueueLocation,
        sqlite::SqliteQueue,
    },
    lifecycle::{
        self, DownOptions, MAINTAINER_ROLE, QUEUE_ENV, ROLE_ENV, UpEnvironment, UpOptions,
        maintainer_command,
    },
    runtime::maintainer_prompt,
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
    Fixture {
        repo,
        location,
        environment: UpEnvironment {
            role: None,
            queue: None,
            path: "/usr/bin:/bin:/home/u/.local/bin".into(),
            socket_password: None,
            current_exe: "/opt/bin/cmux-taskq".into(),
        },
        options: UpOptions {
            parallel: 2,
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

/// Liveness by an explicit dead set; `kill` moves the PID into it.
#[derive(Default)]
struct FakeProcesses {
    dead: Mutex<HashSet<u32>>,
    terminated: Mutex<Vec<u32>>,
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
    fn kill(&self, pid: u32) -> Result<()> {
        self.killed.lock().unwrap().push(pid);
        self.dead.lock().unwrap().insert(pid);
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
    fn create(&self, _: &TaskRun, _: &str) -> Result<String> {
        bail!("up does not create run workspaces")
    }
    fn capture(&self, _: &str) -> Result<String> {
        bail!("not used")
    }
    fn close(&self, _: &str) -> Result<()> {
        bail!("up never closes a workspace")
    }
    fn send_exit(&self, _: &str) -> Result<()> {
        bail!("not used")
    }
    fn find_named(&self, name: &str) -> Result<Option<String>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .workspaces
            .lock()
            .unwrap()
            .iter()
            .find(|(n, ..)| n == name)
            .map(|(_, _, id, _)| id.clone()))
    }
    fn create_named(&self, name: &str, cwd: &Path, command: &str) -> Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut workspaces = self.workspaces.lock().unwrap();
        let id = format!("01234567-89ab-4def-8123-{:012x}", workspaces.len());
        workspaces.push((name.into(), cwd.into(), id.clone(), command.into()));
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

fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn up_starts_the_agent_and_the_maintainer_once_and_reuses_them_after() {
    let fixture = fixture();
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let root = GitRepository::inspect(&fixture.repo).unwrap().root;

    let first = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(first["supervisor"]["outcome"], "started", "{first}");
    assert_eq!(first["supervisor"]["pid"], json!(std::process::id()));
    assert_eq!(
        first["supervisor"]["plist"],
        json!(fixture.location.launch_agent)
    );
    assert_eq!(
        first["supervisor"]["log_dir"],
        json!(fixture.location.log_dir)
    );
    assert_eq!(first["maintainer"]["outcome"], "created");
    assert_eq!(first["maintainer"]["name"], "taskq my repo maintainer");
    assert_eq!(first["pruned_supervisors"], json!([]));
    assert_eq!(first["doctor"]["unfinished_runs"], json!([]));
    assert_eq!(first["doctor"]["awaiting_integration"], json!([]));
    assert_eq!(first["doctor"]["needs_session"], json!([]));

    // The agent definition launchd received.
    let installs = launchd.installs.lock().unwrap();
    assert_eq!(installs.len(), 1);
    let (label, path, contents) = &installs[0];
    assert_eq!(label, &fixture.location.label);
    assert!(label.starts_with("com.cmux-taskq."));
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
        "/opt/bin/cmux-taskq",
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

    // The maintainer workspace: repository root as cwd, role and queue in
    // the environment, the plugin directory and the prompt on the command.
    let workspaces = cmux.workspaces.lock().unwrap();
    assert_eq!(workspaces.len(), 1);
    let (name, cwd, id, command) = &workspaces[0];
    assert_eq!(name, "taskq my repo maintainer");
    assert_eq!(cwd, &root);
    assert_eq!(first["maintainer"]["workspace_id"], json!(id));
    assert!(command.starts_with("'env' 'CMUX_TASKQ_ROLE=maintainer' 'CMUX_TASKQ_QUEUE="));
    assert!(command.contains(&format!("'{}'", fixture.options.claude.display())));
    assert!(command.contains("'--plugin-dir'"));
    assert!(command.contains("You are the maintainer session"));
    drop(workspaces);

    let second = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(second["supervisor"]["outcome"], "reused", "{second}");
    assert_eq!(second["supervisor"]["pid"], json!(std::process::id()));
    assert_eq!(second["maintainer"]["outcome"], "reused");
    assert_eq!(
        second["maintainer"]["workspace_id"],
        first["maintainer"]["workspace_id"]
    );
    assert_eq!(second["pruned_supervisors"], json!([]));
    assert_eq!(launchd.installs.lock().unwrap().len(), 1);
    assert!(launchd.uninstalls.lock().unwrap().is_empty());
    assert_eq!(cmux.workspaces.lock().unwrap().len(), 1);
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
    // The agent stays loaded for inspection; no maintainer workspace was opened.
    assert!(*launchd.loaded.lock().unwrap());
    assert!(cmux.workspaces.lock().unwrap().is_empty());
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
    assert!(message.contains("in-cmux"), "{message}");
    assert!(
        message.ends_with("only processes started inside cmux can connect"),
        "{message}"
    );
    assert_eq!(cmux.detached_preflights.lock().unwrap().len(), 1);
    // No plist, no launchd call, no maintainer workspace, and no plist file.
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
    // does not send the maintainer to the password; it still stops `up`.
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
    assert_eq!(report["maintainer"]["outcome"], "created");
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
    // The password is not in the maintainer's command: the maintainer
    // session is a cmux terminal's child and needs none.
    let workspaces = cmux.workspaces.lock().unwrap();
    assert!(!workspaces[0].3.contains("hunter2"));
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
    queue.register_supervisor("dead-1", dead[0], 4).unwrap();
    queue.register_supervisor("dead-2", dead[1], 1).unwrap();
    queue
        .register_supervisor("live", std::process::id(), 3)
        .unwrap();
    let task = queue
        .add(NewTask {
            title: "held".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: vec![],
            dependencies: vec![],
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    queue.transition(task.id, TaskAction::Ready).unwrap();
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
    assert_eq!(unfinished[0]["task_id"], json!(task.id));
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
fn up_reports_runs_that_wait_for_the_maintainer() {
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
                dependencies: vec![],
                goal_id: None,
                context: String::new(),
            })
            .unwrap();
        queue.transition(task.id, TaskAction::Ready).unwrap();
        let cmux_taskq::domain::ClaimOutcome::Claimed { run } = queue
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
        [&awaiting.id],
    )
    .unwrap();
    raw.execute(
        "UPDATE task_runs SET status='needs_session', last_error='rebase conflicted' WHERE id=?1",
        [&parked.id],
    )
    .unwrap();
    raw.execute(
        "DELETE FROM run_leases WHERE run_id IN (?1, ?2)",
        [&awaiting.id, &parked.id],
    )
    .unwrap();
    // The orphan keeps a lease whose owner is dead.
    let dead = dead_pid();
    raw.execute(
        "UPDATE run_leases SET pid=?2 WHERE run_id=?1",
        rusqlite::params![orphan.id, dead],
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
        json!([{"run_id": awaiting.id, "task_id": awaiting.task_id, "last_error": null}])
    );
    assert_eq!(
        report["doctor"]["needs_session"],
        json!([{"run_id": parked.id, "task_id": parked.task_id, "last_error": "rebase conflicted"}])
    );
    assert_eq!(
        report["doctor"]["unfinished_runs"],
        json!([{"run_id": orphan.id, "task_id": orphan.task_id, "status": "claimed", "lease_stale": true}])
    );
}

/// Inside the maintainer session of this queue `up` opens no workspace
/// and does not even ask cmux; inside one of another queue it does.
#[test]
fn up_skips_the_maintainer_workspace_inside_a_maintainer_session_of_the_same_queue() {
    let mut fixture = fixture();
    fixture.environment.role = Some(MAINTAINER_ROLE.into());
    fixture.environment.queue = Some(fixture.location.db.clone());
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started");
    assert_eq!(
        report["maintainer"],
        json!({"outcome": "skipped", "workspace_id": null, "name": "taskq my repo maintainer"})
    );
    assert_eq!(cmux.calls.load(Ordering::SeqCst), 0);
    assert!(cmux.workspaces.lock().unwrap().is_empty());

    // The same role for another queue: this queue still needs its maintainer.
    fixture.environment.queue = Some(fixture._dir.path().join("elsewhere.db"));
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["maintainer"]["outcome"], "created", "{report}");
    assert_eq!(cmux.workspaces.lock().unwrap().len(), 1);
    // The role alone, without a queue, does not count either.
    let cmux = FakeCmux::default();
    fixture.environment.queue = None;
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["maintainer"]["outcome"], "created", "{report}");
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
        fn create(&self, _: &TaskRun, _: &str) -> Result<String> {
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
        fn find_named(&self, _: &str) -> Result<Option<String>> {
            unreachable!()
        }
        fn create_named(&self, _: &str, _: &Path, _: &str) -> Result<String> {
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

fn down(
    fixture: &Fixture,
    launchd: &FakeLaunchd,
    processes: &FakeProcesses,
    wait: bool,
    force: bool,
) -> Value {
    lifecycle::down(
        &fixture.location,
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
    let processes = FakeProcesses::default();
    let report = down(&fixture, &launchd, &processes, false, false);
    assert_eq!(
        report,
        json!({"outcome": "not_running", "launch_agent_unloaded": false, "pruned_supervisors": []})
    );
    // A dead registration is not a running supervisor either; a loaded
    // agent with no registered process (a crash loop) is unloaded.
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let dead = dead_pid();
    queue.register_supervisor("dead", dead, 1).unwrap();
    processes.dead.lock().unwrap().insert(dead);
    launchd.load(None);
    let report = down(&fixture, &launchd, &processes, true, false);
    assert_eq!(
        report,
        json!({"outcome": "not_running", "launch_agent_unloaded": true, "pruned_supervisors": []})
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
    let report = down(&fixture, &launchd, &processes, false, true);
    assert_eq!(
        report,
        json!({"outcome": "not_running", "launch_agent_unloaded": false, "pruned_supervisors": [{"token": "dead", "pid": dead}]})
    );
    assert!(queue.supervisors().unwrap().is_empty());
    assert!(processes.killed.lock().unwrap().is_empty());
}

#[test]
fn down_unloads_the_agent_and_returns_while_the_supervisor_drains() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    queue.register_supervisor("resident", pid, 4).unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    launchd.load(Some(pid));
    let processes = FakeProcesses::default();
    let report = down(&fixture, &launchd, &processes, false, false);
    assert_eq!(
        report,
        json!({"outcome": "draining", "pid": pid, "pids": [pid], "launch_agent_unloaded": true})
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
    let report = down(&fixture, &launchd, &processes, false, false);
    assert_eq!(report["outcome"], "draining");
    assert_eq!(report["launch_agent_unloaded"], false);
    assert_eq!(processes.terminated.lock().unwrap().as_slice(), &[pid]);

    // Both at once: the agent's own process is left to launchd's SIGTERM
    // (a second one would end its drain), the hand-started one is signalled.
    let by_hand = dead_pid(); // any pid the fake treats as alive
    queue.register_supervisor("by-hand", by_hand, 1).unwrap();
    launchd.load(Some(pid));
    let processes = FakeProcesses::default();
    let report = down(&fixture, &launchd, &processes, false, false);
    assert_eq!(report["outcome"], "draining");
    assert_eq!(report["pids"], json!([pid, by_hand]));
    assert_eq!(processes.terminated.lock().unwrap().as_slice(), &[by_hand]);
    // When launchd cannot say which process is the agent's, nobody is signalled.
    launchd.load(None);
    let processes = FakeProcesses::default();
    down(&fixture, &launchd, &processes, false, false);
    assert!(processes.terminated.lock().unwrap().is_empty());
}

#[test]
fn down_wait_returns_stopped_once_the_registration_is_gone_or_the_process_died() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    queue.register_supervisor("resident", pid, 4).unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
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
    let report = down(&fixture, &launchd, &processes, true, false);
    drain.join().unwrap();
    assert_eq!(
        report,
        json!({"outcome": "stopped", "pid": pid, "pids": [pid], "launch_agent_unloaded": true})
    );
    assert!(processes.killed.lock().unwrap().is_empty());

    // A supervisor killed by launchd's ExitTimeOut leaves its row; the dead
    // PID ends the wait just the same (and `up` prunes the row later).
    queue.register_supervisor("second", pid, 4).unwrap();
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
        fn kill(&self, _: u32) -> Result<()> {
            unreachable!()
        }
    }
    let report = lifecycle::down(
        &fixture.location,
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
        json!({"outcome": "stopped", "pid": pid, "pids": [pid], "launch_agent_unloaded": true})
    );
    // The row stays: the process, not `down`, removes a registration.
    assert_eq!(queue.supervisors().unwrap().len(), 1);
}

#[test]
fn down_force_kills_after_the_unload_and_drops_the_registration() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    queue.register_supervisor("resident", pid, 4).unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    launchd.load(Some(pid));
    let processes = FakeProcesses::default();
    let report = down(&fixture, &launchd, &processes, false, true);
    assert_eq!(
        report,
        json!({"outcome": "killed", "pid": pid, "pids": [pid], "launch_agent_unloaded": true})
    );
    assert_eq!(launchd.uninstalls.lock().unwrap().len(), 1);
    assert_eq!(processes.killed.lock().unwrap().as_slice(), &[pid]);
    assert!(queue.supervisors().unwrap().is_empty());
    assert!(!*launchd.loaded.lock().unwrap());
}

#[test]
fn maintainer_prompt_names_the_queue_the_logs_the_skill_and_the_rules() {
    let prompt =
        maintainer_prompt(Path::new("/data/q/queue.db"), Path::new("/data/q/logs")).unwrap();
    assert!(prompt.starts_with(
        "You are the maintainer session of the cmux-taskq queue at /data/q/queue.db.\n"
    ));
    assert!(prompt.contains("supervisor is the resident `cmux-taskq supervise` process"));
    assert!(prompt.contains("maintainer is this session"));
    assert!(prompt.contains("worker is the Claude session of one run"));
    assert!(prompt.contains("logs to /data/q/logs"));
    assert!(prompt.contains("taskq-maintain skill"));
    assert!(prompt.contains("run status and doctor"));
    assert!(prompt.contains(
        "stale supervisors, unfinished runs, runs awaiting_integration and runs in needs_session"
    ));
    assert!(prompt.contains("wait for the user's instructions"));
    assert!(
        prompt.contains("If the taskq-maintain skill is not available in this session, say so")
    );
    assert!(prompt.contains("Never open or edit the queue database directly"));

    // The workspace command carries the role, the queue, the plugin and the
    // prompt, each shell-quoted on its own.
    let command = maintainer_command(
        Path::new("/data/q's/queue.db"),
        Path::new("/data/q's/logs"),
        Path::new("/opt/claude"),
        Some(Path::new("/plugins/claude-taskq")),
    )
    .unwrap();
    assert!(command.starts_with(
        "'env' 'CMUX_TASKQ_ROLE=maintainer' 'CMUX_TASKQ_QUEUE=/data/q'\"'\"'s/queue.db' '/opt/claude' '--plugin-dir' '/plugins/claude-taskq' '--' 'You are the maintainer session"
    ), "{command}");
    assert_eq!(ROLE_ENV, "CMUX_TASKQ_ROLE");
    assert_eq!(QUEUE_ENV, "CMUX_TASKQ_QUEUE");
    let bare = maintainer_command(
        Path::new("/data/q/queue.db"),
        Path::new("/data/q/logs"),
        Path::new("/opt/claude"),
        None,
    )
    .unwrap();
    assert!(!bare.contains("--plugin-dir"));
    assert!(bare.contains("'/opt/claude' '--' 'You are the maintainer session"));
}
