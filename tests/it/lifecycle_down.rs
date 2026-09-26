//! `down` against fakes for launchd, cmux and process signals: every
//! outcome in either mode (interrupted, drained, force-killed, not
//! running), and the workspace of an in-cmux supervisor it closes.

use crate::common;

use common::lifecycle::*;

use anyhow::Result;
use dagq::{
    VERSION,
    application::{ProcessControl, WorkspaceBackend, WorkspaceTags},
    domain::{SessionRole, SupervisorMode},
    infrastructure::sqlite::SqliteQueue,
    lifecycle::{self, DownOptions},
};
use serde_json::json;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::Duration,
};

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
