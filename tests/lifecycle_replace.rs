//! `up` replacing a supervisor of another version or build against fakes
//! for launchd, cmux and process signals: the drain and restart in either
//! mode, the `--no-wait` refusal, the reuse of this binary's own version,
//! the handoff without a drain, and the migrations `up` applies or refuses.

mod common;

use common::lifecycle::*;

use dagq::{
    VERSION,
    application::TaskStore,
    domain::{NewTask, SessionRole, SupervisorMode, TaskAction},
    infrastructure::{adapters::GitRepository, sqlite::SqliteQueue},
    lifecycle,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{fs, path::Path, thread, time::Duration};

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
            kind: None,
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
        // covered by tests/queue_migration.rs and the e2e).
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
        auto_update: false,
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
/// with the way to it, before it starts or touches anything. A migration
/// after 0031 is breaking, so a queue before it is refused even when the
/// migration it lacks first (0031) is compatible.
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
             ALTER TABLE supervisors DROP COLUMN auto_update;
             DROP TABLE binary_updates;
             ALTER TABLE tasks DROP COLUMN kind;
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
    // The breaking ones after 0031, however many came since (ADR-0067
    // decision 4).
    let breaking: Vec<String> = dagq::infrastructure::schema::MIGRATIONS
        .iter()
        .enumerate()
        .skip(30)
        .filter(|(_, migration)| !dagq::infrastructure::schema::is_compatible(migration))
        .map(|(index, _)| (index + 1).to_string())
        .collect();
    assert!(!breaking.is_empty());
    assert!(
        error.contains(&format!("breaking migration(s) {}", breaking.join(", "))),
        "{error}"
    );
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

    // The compatible automatic-update migration (0033) pending, with the
    // task kind's (0034): `up` applies them and goes on when no breaking one
    // follows; otherwise it names only the breaking ones (ADR-0048 added
    // 0035). Versions are
    // looked up rather than written, so a later migration does not rewrite
    // this test (ADR-0067 decision 4).
    let auto_update = dagq::infrastructure::schema::MIGRATIONS
        .iter()
        .position(|migration| migration.contains("CREATE TABLE binary_updates"))
        .unwrap() as i64
        + 1;
    let breaking_after: Vec<String> = dagq::infrastructure::schema::MIGRATIONS
        .iter()
        .enumerate()
        .skip(auto_update as usize)
        .filter(|(_, migration)| !dagq::infrastructure::schema::is_compatible(migration))
        .map(|(index, _)| (index + 1).to_string())
        .collect();
    Connection::open(db)
        .unwrap()
        .execute_batch(&format!(
            "ALTER TABLE supervisors DROP COLUMN auto_update;
             DROP TABLE binary_updates;
             ALTER TABLE tasks DROP COLUMN kind;
             PRAGMA user_version = {};",
            auto_update - 1
        ))
        .unwrap();
    if breaking_after.is_empty() {
        let report = up(&fixture, &cmux, &launchd, &processes);
        assert_eq!(
            report["migrated"]["applied"][0]["version"], auto_update,
            "{report}"
        );
        assert_eq!(report["supervisor"]["auto_update"], false, "{report}");
    } else {
        let error = format!(
            "{:#}",
            try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
        );
        assert!(
            error.contains(&format!(
                "breaking migration(s) {} ",
                breaking_after.join(", ")
            )),
            "{error}"
        );
        SqliteQueue::migrate(db, None, 0).unwrap();
        let report = up(&fixture, &cmux, &launchd, &processes);
        assert_eq!(report["migrated"], Value::Null, "{report}");
        assert_eq!(report["supervisor"]["auto_update"], false, "{report}");
    }

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
