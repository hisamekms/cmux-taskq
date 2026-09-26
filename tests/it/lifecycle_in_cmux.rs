//! `up --in-cmux` against fakes for launchd, cmux and process signals: the
//! supervisor started in a cmux workspace, told apart from a silent one,
//! and the workspace it refuses to open twice or forgets once closed.

use crate::common;

use common::lifecycle::*;

use dagq::{
    VERSION,
    domain::{SessionRole, SupervisorMode},
    infrastructure::{
        adapters::{GitRepository, shell_quote},
        sqlite::SqliteQueue,
    },
    lifecycle,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::Path;

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
            "'/opt/bin/dagq' '--db' {} 'supervise' '--parallel' '2' '--log-dir' {} '--cmux' {} '--claude' {} '--mode' 'in_cmux' '--plugin-dir' {}",
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
