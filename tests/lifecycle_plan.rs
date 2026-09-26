//! The planners against fakes for cmux: the prompts of the inbox and the
//! planner, the workspaces `plan` opens and records, the planner the
//! runtime opens for a proposal, and how a planner session is judged.

mod common;

use common::lifecycle::*;

use anyhow::{Result, bail};
use dagq::{
    application::{TaskStore, WorkspaceBackend, WorkspaceTags},
    domain::{NewTask, SessionRole, TaskRun},
    infrastructure::{
        adapters::{GitRepository, shell_quote},
        sqlite::SqliteQueue,
    },
    lifecycle::{self, QUEUE_ENV, ROLE_ENV, inbox_command},
    runtime::{inbox_prompt, planner_prompt},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

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
            kind: None,
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
