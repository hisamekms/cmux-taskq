//! `up` against fakes for launchd, cmux and process signals: the idempotent
//! start in launchd mode, the preflights (the `[run.env]` programs, Claude
//! Code's folder trust, the out-of-cmux connection), the pruning of dead
//! registrations, the runs it reports, and the inbox workspace decisions
//! and its look. The real launchd and cmux path is `tests/e2e.rs`.

mod common;

use common::lifecycle::*;

use anyhow::{Result, bail};
use dagq::{
    VERSION,
    application::{SupervisorEnvironment, TaskStore, WorkspaceBackend, WorkspaceTags},
    domain::{NewTask, SessionRole, Task, TaskAction, TaskRun},
    infrastructure::{
        adapters::{GitRepository, SOCKET_PASSWORD_ENV},
        location::QueueLocation,
        sqlite::SqliteQueue,
    },
    lifecycle::{self, INBOX_ROLE, PLANNER_ROLE},
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{fs, path::Path, sync::atomic::Ordering, time::Duration};

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
