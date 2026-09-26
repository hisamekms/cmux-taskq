//! Plan review (ADR-0041 decisions 11-15, 17) through the supervisor loop,
//! with the headless plan review played by a stub provider that prints a
//! scripted verdict and a cmux double that only records what it is asked.
//! No task is claimed in these tests: every task that becomes ready waits
//! for a draft blocker.

use crate::common;

use common::Bounded;

use anyhow::{Result, bail};
use dagq::{
    application::{
        AgentProvider, CommandSpec, SupervisorEnvironment, TaskStore, WorkspaceBackend,
        WorkspaceTags, planner_idle_marker,
    },
    domain::{
        AskKind, DraftOrigin, NewAsk, NewGoal, NewTask, PlannerOrigin, PlannerOwner, Priority,
        ProposalId, ProposalStatus, Submission, Task, TaskAction, TaskEdit, TaskId, TaskRun,
        TaskStatus,
    },
    infrastructure::{
        clock,
        location::{plan_reviews_dir, planners_dir},
        sqlite::SqliteQueue,
    },
    runtime::{self, SuperviseOptions},
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::Duration,
};
use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Fixture {
    _dir: TempDir,
    repo: PathBuf,
    db: PathBuf,
    claude: PathBuf,
    /// Times the test while held (task 324).
    _test: common::Waiting,
}

/// A repository with one commit on main, a queue next to it, and a draft
/// task (1) every other task depends on, so none is ever claimed.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    fs::write(repo.join("seed.txt"), "seed\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "seed"]);
    let db = dir.path().join("queue").join("queue.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let mut queue = SqliteQueue::init(&db).unwrap();
    let blocker = add(&mut queue, "blocker", &[], Priority::Normal);
    assert_eq!(blocker, TaskId::new(1));
    let claude = dir.path().join("claude-stub");
    fs::write(&claude, "#!/bin/sh\nprintf 'stub\\n'\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    Fixture {
        _dir: dir,
        _test: common::test(),
        repo,
        db,
        claude,
    }
}

fn add(queue: &mut SqliteQueue, title: &str, deps: &[TaskId], priority: Priority) -> TaskId {
    queue
        .add(NewTask {
            kind: None,
            title: title.into(),
            description: format!("{title}: change the type of Foo"),
            acceptance: format!("{title} works; tests/cli.rs is not changed"),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority,
            dependencies: deps.to_vec(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap()
        .id()
}

/// Submit `tasks` as a new proposal owned by `workspace` (a person's
/// planner, or none).
fn submit(queue: &mut SqliteQueue, tasks: &[TaskId], workspace: Option<&str>) -> ProposalId {
    queue
        .submit(Submission {
            tasks: tasks.to_vec(),
            goals: Vec::new(),
            proposal: None,
            owner: PlannerOwner {
                origin: PlannerOrigin::Person,
                workspace_id: workspace.map(str::to_owned),
            },
        })
        .unwrap()
        .id()
}

/// The headless plan review: each job prints the next verdict (the last
/// repeats) and its prompt is kept.
struct StubReviewer {
    verdicts: Mutex<Vec<String>>,
    prompts: Mutex<Vec<String>>,
    /// A task the first job's run edits through the queue at `.0`, as
    /// `dagq edit` would while the job runs.
    edit: Mutex<Option<(PathBuf, TaskId)>>,
}

impl StubReviewer {
    fn new(verdicts: &[Value]) -> Self {
        Self {
            verdicts: Mutex::new(verdicts.iter().map(Value::to_string).collect()),
            prompts: Mutex::new(Vec::new()),
            edit: Mutex::new(None),
        }
    }
    /// The first job edits `task` of the queue at `db` while it runs.
    fn editing(self, db: &Path, task: TaskId) -> Self {
        *self.edit.lock().unwrap() = Some((db.to_owned(), task));
        self
    }
    /// A job that fails: it exits non-zero.
    fn failing() -> Self {
        Self {
            verdicts: Mutex::new(vec!["FAIL".into()]),
            prompts: Mutex::new(Vec::new()),
            edit: Mutex::new(None),
        }
    }
    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

impl AgentProvider for StubReviewer {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn command(&self, _: &TaskRun, _: &str) -> Result<CommandSpec> {
        unreachable!("no run starts in these tests")
    }
    fn resume_command(&self, _: &TaskRun) -> Result<CommandSpec> {
        unreachable!("no run starts in these tests")
    }
    fn headless_command(&self, cwd: &Path, prompt: &str, tools: &[&str]) -> Result<CommandSpec> {
        assert_eq!(tools, ["Read", "Grep", "Glob"]);
        self.prompts.lock().unwrap().push(prompt.into());
        // The job has started: its row and its event are in the queue.
        if let Some((db, task)) = self.edit.lock().unwrap().take() {
            SqliteQueue::open(&db)?.edit_task(
                task,
                TaskEdit {
                    description: Some("edited while its review ran".into()),
                    ..TaskEdit::default()
                },
            )?;
        }
        let mut verdicts = self.verdicts.lock().unwrap();
        let verdict = if verdicts.len() > 1 {
            verdicts.remove(0)
        } else {
            verdicts[0].clone()
        };
        let script = if verdict == "FAIL" {
            "echo 'model unavailable' >&2; exit 3".to_owned()
        } else {
            format!("printf '%s\\n' '{verdict}'")
        };
        let mut command = CommandSpec::new("/bin/sh");
        command.current_dir(cwd).arg("-c").arg(script);
        Ok(command)
    }
    fn review_command(&self, _: &TaskRun, _: &str) -> Result<CommandSpec> {
        unreachable!("no run is reviewed in these tests")
    }
    fn review_timeout(&self) -> Duration {
        Duration::from_secs(30)
    }
}

/// cmux as far as plan review uses it: workspaces it lists (the ones
/// `listed` and those it opened, until closed), texts typed, workspaces
/// opened by name, exits sent and notifications.
#[derive(Default)]
struct PlanWorkspace {
    listed: Mutex<Vec<String>>,
    opened: Mutex<Vec<(String, String)>>,
    texts: Mutex<Vec<(String, String)>>,
    exits: Mutex<Vec<String>>,
    closed: Mutex<Vec<String>>,
    notifications: Mutex<Vec<(String, String)>>,
}

impl PlanWorkspace {
    fn listing(workspaces: &[&str]) -> Self {
        let backend = Self::default();
        *backend.listed.lock().unwrap() = workspaces.iter().map(|w| (*w).to_owned()).collect();
        backend
    }
    fn texts(&self) -> Vec<(String, String)> {
        self.texts.lock().unwrap().clone()
    }
    fn opened(&self) -> Vec<(String, String)> {
        self.opened.lock().unwrap().clone()
    }
}

impl WorkspaceBackend for PlanWorkspace {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn preflight_detached(&self, _: &SupervisorEnvironment) -> Result<()> {
        Ok(())
    }
    fn create(&self, _: &Task, _: &TaskRun, _: &str, _: &WorkspaceTags) -> Result<String> {
        bail!("no run starts in these tests")
    }
    fn create_resume(&self, _: &Task, _: &TaskRun, _: &str, _: &WorkspaceTags) -> Result<String> {
        bail!("no run resumes in these tests")
    }
    fn send_text(&self, workspace_id: &str, text: &str) -> Result<()> {
        self.texts
            .lock()
            .unwrap()
            .push((workspace_id.into(), text.into()));
        Ok(())
    }
    fn send_enter(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn capture(&self, _: &str) -> Result<String> {
        Ok(String::new())
    }
    fn close(&self, workspace_id: &str) -> Result<()> {
        self.closed.lock().unwrap().push(workspace_id.into());
        self.listed.lock().unwrap().retain(|w| w != workspace_id);
        Ok(())
    }
    fn set_color(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn set_status(&self, _: &str, _: &str, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn pin(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn send_exit(&self, workspace_id: &str) -> Result<()> {
        self.exits.lock().unwrap().push(workspace_id.into());
        Ok(())
    }
    fn listed_workspace_ids(&self) -> Result<Vec<String>> {
        Ok(self.listed.lock().unwrap().clone())
    }
    fn exists(&self, workspace_id: &str) -> Result<bool> {
        Ok(self
            .listed
            .lock()
            .unwrap()
            .iter()
            .any(|w| w == workspace_id))
    }
    fn create_named(&self, name: &str, _: &Path, _: &str, _: &WorkspaceTags) -> Result<String> {
        let mut opened = self.opened.lock().unwrap();
        let id = format!("RT{}", opened.len() + 1);
        opened.push((id.clone(), name.into()));
        self.listed.lock().unwrap().push(id.clone());
        Ok(id)
    }
    fn ensure_group(&self, _: &str, _: &str) -> Result<String> {
        Ok("GROUP".into())
    }
    fn notify(&self, title: &str, body: &str, _: Option<&str>) -> Result<()> {
        self.notifications
            .lock()
            .unwrap()
            .push((title.into(), body.into()));
        Ok(())
    }
    fn submit_check_interval(&self) -> Duration {
        Duration::from_millis(5)
    }
}

fn options(runtime_planners: usize, planner_timeout: Duration) -> SuperviseOptions {
    SuperviseOptions {
        tick: Duration::from_millis(20),
        idle_poll: Duration::from_millis(20),
        generators: clock::system(),
        runtime_planners,
        planner_timeout,
        ..SuperviseOptions::new(2, true)
    }
}

fn supervise(fx: &Fixture, backend: &PlanWorkspace, reviewer: &StubReviewer) -> Value {
    supervise_with(
        fx,
        backend,
        reviewer,
        &options(1, Duration::from_secs(3600)),
    )
}

fn supervise_with(
    fx: &Fixture,
    backend: &PlanWorkspace,
    reviewer: &StubReviewer,
    options: &SuperviseOptions,
) -> Value {
    runtime::supervise_with_reviewer(
        &fx.db,
        &fx.repo,
        backend,
        &fx.claude,
        reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        options,
    )
    .unwrap()
}

fn status(queue: &mut SqliteQueue, id: TaskId) -> TaskStatus {
    queue.show(id).unwrap().task.status()
}

/// The payloads of the task's events of `kind`.
fn events(queue: &mut SqliteQueue, id: TaskId, kind: &str) -> Vec<Value> {
    queue
        .show(id)
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.payload)
        .collect()
}

fn proposal_column(db: &Path, id: ProposalId, column: &str) -> Value {
    let connection = Connection::open(db).unwrap();
    connection
        .query_row(
            &format!("SELECT {column} FROM proposals WHERE id=?1"),
            [id.as_i64()],
            |r| {
                Ok(match r.get_ref(0)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(n) => json!(n),
                    rusqlite::types::ValueRef::Text(t) => {
                        json!(String::from_utf8_lossy(t).into_owned())
                    }
                    _ => Value::Null,
                })
            },
        )
        .unwrap()
}

/// A person's planner in workspace `workspace`, alive (its wrapper is this
/// test process) and idle.
fn idle_person_planner(queue: &SqliteQueue, db: &Path, workspace: &str) {
    let planner = queue.open_planner(PlannerOrigin::Person, None).unwrap();
    queue
        .planner_workspace_created(planner.id, workspace)
        .unwrap();
    queue
        .register_planner_wrapper(planner.id, std::process::id())
        .unwrap();
    let dir = planners_dir(db).join(planner.id.to_string());
    fs::create_dir_all(&dir).unwrap();
    fs::write(planner_idle_marker(&dir), "{}").unwrap();
}

#[test]
fn a_passing_plan_review_readies_the_proposal_with_its_actions() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let two = add(&mut queue, "two", &[blocker], Priority::Normal);
    let three = add(&mut queue, "three", &[blocker], Priority::Normal);
    let proposal = submit(&mut queue, &[two, three], None);
    // Landings conflicted in a file main has and in one it no longer has:
    // the prompt lists the first as a hotspot (goal 31).
    Connection::open(&fx.db)
        .unwrap()
        .execute(
            "INSERT INTO run_events(task_id, kind, payload) VALUES (?1, 'conflict_precheck', ?2)",
            rusqlite::params![
                blocker.as_i64(),
                json!({"main": "m", "conflicts": ["seed.txt", "gone.txt"]}).to_string()
            ],
        )
        .unwrap();
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "pass", "reasons": [], "summary": "sound",
        "actions": [
            {"action": "add_dependency", "task_id": three, "depends_on": two},
            {"action": "lower_priority", "task_id": two, "priority": "low"}
        ]
    })]);
    let backend = PlanWorkspace::default();
    let outcome = supervise(&fx, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(status(&mut queue, two), TaskStatus::Ready);
    assert_eq!(status(&mut queue, three), TaskStatus::Ready);
    assert_eq!(
        queue.show_proposal(proposal).unwrap().status(),
        ProposalStatus::Accepted
    );
    assert_eq!(queue.show(three).unwrap().dependencies, [blocker, two]);
    assert_eq!(queue.show(two).unwrap().task.priority(), Priority::Low);
    // The events of the proposal are on its first task.
    let started = events(&mut queue, two, "plan_review_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["proposal_id"], json!(proposal));
    // The job runs in the repository's checkout, and its Claude session's
    // span has that cwd (ADR-0048).
    assert_eq!(
        started[0]["cwd"],
        fx.repo.canonicalize().unwrap().to_str().unwrap()
    );
    let opened = events(&mut queue, two, "session_opened");
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0]["kind"], "plan_review");
    assert_eq!(opened[0]["cwd"], started[0]["cwd"]);
    let finished = events(&mut queue, two, "plan_review_finished");
    assert_eq!(finished[0]["decision"], "pass");
    assert_eq!(finished[0]["summary"], "sound");
    // The prompt, kept in the job's directory, points the job at the
    // repository's rules and carries the tasks, the lint result and the
    // checks, the acceptance one included.
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 1);
    let dir = plan_reviews_dir(&fx.db).join(started[0]["plan_review_id"].to_string());
    assert_eq!(
        fs::read_to_string(dir.join("prompt.txt")).unwrap(),
        prompts[0]
    );
    for expected in [
        "You are the plan review of dagq proposal 1",
        "AGENTS.md and CLAUDE.md, docs/adr/README.md",
        "\"title\":\"two\"",
        "depends_on_draft",
        "an acceptance criterion that contradicts the task's own description or a sibling task's acceptance",
        "cancel_duplicate (only an obvious duplicate; a doubtful one, or a change that looks already made, is a concern)",
        "Files the landings conflicted in most lately",
        "\"path\":\"seed.txt\"",
    ] {
        assert!(
            prompts[0].contains(expected),
            "{expected:?} not in {}",
            prompts[0]
        );
    }
    assert!(!prompts[0].contains("gone.txt"), "{}", prompts[0]);
    // Reviewed once: a second pass finds nothing to review.
    supervise(&fx, &backend, &reviewer);
    assert_eq!(reviewer.prompts().len(), 1);
}

#[test]
fn a_revise_goes_to_the_live_planner_with_the_precedents_and_times_out_to_the_inbox() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    // A person answered the same kind of mismatch before.
    let earlier = queue
        .ask(NewAsk {
            kind: AskKind::Blocked,
            task_id: None,
            run_id: None,
            question: "task 9 changes a type but its acceptance says tests/cli.rs is not changed"
                .into(),
            options: Vec::new(),
            asked_by: "observer".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
        })
        .unwrap()
        .ask
        .id;
    queue
        .answer(
            earlier,
            "drop that acceptance line: the tests follow the type",
        )
        .unwrap();
    idle_person_planner(&queue, &fx.db, "PW");
    let task = add(&mut queue, "retype", &[blocker], Priority::Normal);
    let proposal = submit(&mut queue, &[task], Some("PW"));
    let reason = format!(
        "task {task} changes the type of Foo, yet its acceptance says tests/cli.rs is not changed"
    );
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "revise", "reasons": [reason], "summary": "acceptance contradicts the description",
        "precedents": [earlier]
    })]);
    let backend = PlanWorkspace::listing(&["PW"]);
    supervise(&fx, &backend, &reviewer);
    assert!(
        reviewer.prompts()[0].contains(&format!(
            "precedent: ask {earlier} (blocked) asked: task 9 changes a type"
        )),
        "{}",
        reviewer.prompts()[0]
    );
    let revising = queue.show_proposal(proposal).unwrap();
    assert_eq!(revising.status(), ProposalStatus::Revising);
    assert_eq!(revising.revise_count(), 1);
    assert_eq!(status(&mut queue, task), TaskStatus::Draft);
    // The planner that owns the proposal got the reasons and the precedent,
    // and no planner was opened for it.
    let texts = backend.texts();
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert_eq!(texts[0].0, "PW");
    for expected in [
        format!("Plan review sent proposal {proposal} back."),
        reason.clone(),
        format!("precedent: ask {earlier}"),
        "a person answered: drop that acceptance line".to_owned(),
        format!("dagq submit --proposal {proposal}"),
    ] {
        assert!(
            texts[0].1.contains(&expected),
            "{expected:?} not in {}",
            texts[0].1
        );
    }
    assert!(backend.opened().is_empty());
    let sent = events(&mut queue, task, "plan_revise_sent");
    assert_eq!(sent[0]["opened"], false);
    assert_eq!(sent[0]["workspace_id"], "PW");

    // Past the planner timeout without a resubmission, the inbox is told
    // once, and it shows as the planner's attention.
    std::thread::sleep(Duration::from_millis(1100));
    let quick = options(1, Duration::ZERO);
    supervise_with(&fx, &backend, &reviewer, &quick);
    supervise_with(&fx, &backend, &reviewer, &quick);
    let unresponsive = events(&mut queue, task, "planner_unresponsive");
    assert_eq!(unresponsive.len(), 1);
    let status_now = runtime::status(&fx.db).unwrap();
    let attention = status_now["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "planner_unresponsive")
        .cloned()
        .unwrap_or_else(|| panic!("{status_now}"));
    assert_eq!(attention["next"], "check the planner");
    assert_eq!(attention["task_id"], json!(task));
    let watched = dagq::watch::events(&fx.db, dagq::domain::EventId::new(0), 100, false).unwrap();
    assert!(
        watched["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "planner_unresponsive" && e["next"] == "check the planner"),
        "{watched}"
    );
    assert_eq!(backend.texts().len(), 1, "the revise is not sent twice");
    assert_eq!(
        reviewer.prompts().len(),
        1,
        "a revising proposal is not reviewed"
    );

    // Submitting it again clears the revise; the next review passes.
    queue
        .submit(Submission {
            tasks: Vec::new(),
            goals: Vec::new(),
            proposal: Some(proposal),
            owner: PlannerOwner {
                origin: PlannerOrigin::Person,
                workspace_id: Some("PW".into()),
            },
        })
        .unwrap();
    assert_eq!(
        proposal_column(&fx.db, proposal, "unresponsive_at"),
        Value::Null
    );
    let passing = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    supervise(&fx, &backend, &passing);
    assert_eq!(status(&mut queue, task), TaskStatus::Ready);
}

#[test]
fn a_revise_without_a_live_planner_opens_planners_within_the_limit() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let first = add(&mut queue, "first", &[blocker], Priority::Normal);
    let second = add(&mut queue, "second", &[blocker], Priority::Normal);
    let one = submit(&mut queue, &[first], None);
    let two = submit(&mut queue, &[second], Some("GONE"));
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "revise", "reasons": ["split it"], "summary": "too big"
    })]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    assert_eq!(reviewer.prompts().len(), 2);
    for proposal in [one, two] {
        assert_eq!(
            queue.show_proposal(proposal).unwrap().status(),
            ProposalStatus::Revising
        );
    }
    // One runtime planner at a time: the older proposal got it, the other
    // waits.
    let opened = backend.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(
        opened[0].1.ends_with(&format!("proposal {one}")),
        "{opened:?}"
    );
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners.len(), 1);
    assert_eq!(planners[0].origin, PlannerOrigin::Runtime);
    assert_eq!(planners[0].proposal_id, Some(one));
    let prompt = fs::read_to_string(
        planners_dir(&fx.db)
            .join(planners[0].id.to_string())
            .join("prompt.txt"),
    )
    .unwrap();
    assert!(prompt.contains("- split it"), "{prompt}");
    assert_eq!(
        events(&mut queue, first, "plan_revise_sent")[0]["opened"],
        true
    );
    assert!(events(&mut queue, second, "plan_revise_sent").is_empty());
    assert_eq!(proposal_column(&fx.db, two, "revise_sent_at"), Value::Null);

    // The planner's session ends without submitting: its workspace is
    // closed and the revise of proposal one goes to a new planner first;
    // the limit still holds for proposal two.
    queue.register_planner_wrapper(planners[0].id, 1).unwrap();
    queue.planner_exited(planners[0].id, 1, 0).unwrap();
    supervise(&fx, &backend, &reviewer);
    assert!(backend.closed.lock().unwrap().contains(&"RT1".to_owned()));
    assert!(queue.planner(planners[0].id).unwrap().closed_at.is_some());
    assert_eq!(events(&mut queue, first, "plan_revise_lost").len(), 1);
    let open = queue.planners(false).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].proposal_id, Some(one));
    assert_eq!(backend.opened().len(), 2);
    assert_eq!(events(&mut queue, first, "plan_revise_sent").len(), 2);
    assert!(events(&mut queue, second, "plan_revise_sent").is_empty());

    // The revise still waiting for a planner past the timeout is told to
    // the inbox, once.
    std::thread::sleep(Duration::from_millis(1100));
    let quick = options(1, Duration::ZERO);
    supervise_with(&fx, &backend, &reviewer, &quick);
    supervise_with(&fx, &backend, &reviewer, &quick);
    let waiting = events(&mut queue, second, "planner_unresponsive");
    assert_eq!(waiting.len(), 1, "{waiting:?}");
    assert_eq!(waiting[0]["planner_id"], Value::Null);
}

#[test]
fn a_concern_asks_the_inbox_and_the_supervisor_applies_the_answers() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let kept = add(&mut queue, "kept", &[blocker], Priority::Normal);
    let dropped = add(&mut queue, "dropped", &[blocker], Priority::Normal);
    let returned = add(&mut queue, "returned", &[blocker], Priority::Normal);
    let proposals = [kept, dropped, returned].map(|task| submit(&mut queue, &[task], None));
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "concern", "reasons": ["looks already implemented"], "summary": "maybe done"
    })]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 3);
    for (ask, task) in asks.iter().zip([kept, dropped, returned]) {
        assert_eq!(ask.kind, AskKind::ApprovePlan);
        assert_eq!(ask.task_id, Some(task));
        assert_eq!(ask.options, ["ready", "send_back", "cancel"]);
        assert!(
            ask.question.contains("looks already implemented"),
            "{}",
            ask.question
        );
        assert_eq!(status(&mut queue, task), TaskStatus::Submitted);
    }
    assert_eq!(backend.notifications.lock().unwrap().len(), 3);
    // Held for the person: not reviewed again.
    supervise(&fx, &backend, &reviewer);
    assert_eq!(reviewer.prompts().len(), 3);

    queue.answer(asks[0].id, "ready").unwrap();
    queue.answer(asks[1].id, "cancel").unwrap();
    queue
        .answer(asks[2].id, "send_back: split the parser out first")
        .unwrap();
    // The runtime applies them: none is the inbox's to act on.
    let status_now = runtime::status(&fx.db).unwrap();
    for ask in &asks {
        let entry = status_now["attention"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["ask_id"] == json!(ask.id))
            .cloned()
            .unwrap();
        assert_eq!(
            entry["next"],
            format!("applying the answer of ask {} (runtime)", ask.id)
        );
    }
    supervise(&fx, &backend, &reviewer);
    assert_eq!(status(&mut queue, kept), TaskStatus::Ready);
    assert_eq!(
        queue.show_proposal(proposals[0]).unwrap().status(),
        ProposalStatus::Accepted
    );
    assert_eq!(status(&mut queue, dropped), TaskStatus::Canceled);
    assert_eq!(
        queue.show_proposal(proposals[1]).unwrap().status(),
        ProposalStatus::Canceled
    );
    assert_eq!(status(&mut queue, returned), TaskStatus::Draft);
    let reasons = proposal_column(&fx.db, proposals[2], "revise_reasons");
    let reasons: Vec<String> = serde_json::from_str(reasons.as_str().unwrap()).unwrap();
    assert_eq!(
        reasons,
        [
            format!(
                "a person sent the proposal back in ask {}: split the parser out first",
                asks[2].id
            ),
            "looks already implemented".to_owned()
        ]
    );
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    assert_eq!(
        events(&mut queue, kept, "plan_decided")[0]["answer"],
        "ready"
    );
    // The one sent back went to a planner of the runtime's.
    assert_eq!(backend.opened().len(), 1);

    // Withdrawn while revising, it drops the revise: no planner is sent
    // it again, and its draft joins a new proposal.
    queue.withdraw_proposal(proposals[2]).unwrap();
    assert_eq!(
        proposal_column(&fx.db, proposals[2], "revise_reasons"),
        Value::Null
    );
    supervise(&fx, &backend, &reviewer);
    assert_eq!(backend.opened().len(), 1);
    assert_eq!(status(&mut queue, returned), TaskStatus::Draft);
    let again = submit(&mut queue, &[returned], None);
    assert_ne!(again, proposals[2]);
    assert_eq!(status(&mut queue, returned), TaskStatus::Submitted);
}

/// Withdrawing a proposal held for a concern closes its `approve_plan`
/// ask, so the answer never reaches the proposal the task joins next; a
/// withdrawn draft with an origin waits for a planner of the runtime's
/// again.
#[test]
fn a_withdrawn_proposal_closes_its_concern_and_its_drafts_are_free() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let task = add(&mut queue, "doubtful", &[blocker], Priority::Normal);
    queue
        .record_draft_origin(task, DraftOrigin::FollowUp, &json!({"run": "r"}))
        .unwrap();
    assert!(
        queue
            .planner_drafts()
            .unwrap()
            .iter()
            .any(|d| d.task.id() == task)
    );
    let first = submit(&mut queue, &[task], None);
    assert!(
        queue
            .planner_drafts()
            .unwrap()
            .iter()
            .all(|d| d.task.id() != task)
    );
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "concern", "reasons": ["looks already implemented"], "summary": "maybe"
    })]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind, AskKind::ApprovePlan);

    queue.withdraw_proposal(first).unwrap();
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    let closed = queue.read_ask(asks[0].id).unwrap();
    assert_eq!(closed.answer.as_deref(), Some("withdrawn"));
    assert!(closed.closed_at.is_some());
    assert_eq!(
        events(&mut queue, task, "ask_answered")[0]["runtime_closed"],
        true
    );
    assert_eq!(closed.answered_by.as_deref(), Some("runtime"));
    assert_eq!(
        events(&mut queue, task, "ask_answered")[0]["answered_by"],
        "runtime"
    );
    assert_eq!(status(&mut queue, task), TaskStatus::Draft);
    assert!(
        queue
            .planner_drafts()
            .unwrap()
            .iter()
            .any(|d| d.task.id() == task)
    );

    // Submitted again and held for a concern again, it gets a new ask.
    let second = submit(&mut queue, &[task], None);
    assert_ne!(second, first);
    supervise(&fx, &backend, &reviewer);
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert_ne!(asks[0].id, closed.id);
    assert_eq!(status(&mut queue, task), TaskStatus::Submitted);
}

#[test]
fn a_revise_past_the_limit_is_a_concern() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "stubborn", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], None);
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "revise", "reasons": ["still vague"], "summary": "vague"
    })]);
    let backend = PlanWorkspace::default();
    let again = |queue: &mut SqliteQueue| {
        queue
            .submit(Submission {
                tasks: Vec::new(),
                goals: Vec::new(),
                proposal: Some(proposal),
                owner: PlannerOwner {
                    origin: PlannerOrigin::Runtime,
                    workspace_id: None,
                },
            })
            .unwrap();
    };
    supervise(&fx, &backend, &reviewer);
    again(&mut queue);
    supervise(&fx, &backend, &reviewer);
    again(&mut queue);
    supervise(&fx, &backend, &reviewer);
    assert_eq!(reviewer.prompts().len(), 3);
    assert_eq!(status(&mut queue, task), TaskStatus::Submitted);
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert!(
        asks[0].question.contains(
            "It answered revise, but proposal 1 was sent back 2 times already (at most 2)."
        ),
        "{}",
        asks[0].question
    );
    let finished = events(&mut queue, task, "plan_review_finished");
    assert_eq!(finished[2]["verdict"], "revise");
    assert_eq!(finished[2]["decision"], "concern");
}

#[test]
fn a_failed_plan_review_waits_for_a_person_and_is_not_retried() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "unlucky", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], None);
    let reviewer = StubReviewer::failing();
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    supervise(&fx, &backend, &reviewer);
    assert_eq!(reviewer.prompts().len(), 1);
    assert_eq!(status(&mut queue, task), TaskStatus::Submitted);
    assert_eq!(proposal_column(&fx.db, proposal, "review_hold"), "failed");
    let failed = events(&mut queue, task, "plan_review_failed");
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("model unavailable"),
        "{failed:?}"
    );
    let status_now = runtime::status(&fx.db).unwrap();
    let attention = status_now["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "plan_review_failed")
        .cloned()
        .unwrap_or_else(|| panic!("{status_now}"));
    assert_eq!(attention["next"], "plan review by hand");
    assert_eq!(attention["task_id"], json!(task));
    // A verdict the runtime cannot apply fails the same way.
    let other = add(&mut queue, "raised", &[TaskId::new(1)], Priority::Normal);
    submit(&mut queue, &[other], None);
    let raising = StubReviewer::new(&[json!({
        "verdict": "pass", "reasons": [], "summary": "ok",
        "actions": [{"action": "lower_priority", "task_id": other, "priority": "urgent"}]
    })]);
    supervise(&fx, &backend, &raising);
    assert_eq!(status(&mut queue, other), TaskStatus::Submitted);
    let failed = events(&mut queue, other, "plan_review_failed");
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("may only lower the priority"),
        "{failed:?}"
    );

    // A person has the first one reviewed again: it goes as it is and
    // passes; the other one is readied with the bypass, which ends its
    // proposal and its attention.
    let again = queue
        .submit(Submission {
            tasks: Vec::new(),
            goals: Vec::new(),
            proposal: Some(proposal),
            owner: PlannerOwner {
                origin: PlannerOrigin::Person,
                workspace_id: None,
            },
        })
        .unwrap();
    assert_eq!(again.status(), ProposalStatus::Submitted);
    assert_eq!(events(&mut queue, task, "proposal_resubmitted").len(), 1);
    queue.transition(other, TaskAction::BypassReview).unwrap();
    let passing = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    supervise(&fx, &backend, &passing);
    assert_eq!(passing.prompts().len(), 1);
    assert_eq!(status(&mut queue, task), TaskStatus::Ready);
    let settled = events(&mut queue, other, "proposal_settled");
    assert_eq!(settled[0]["status"], "accepted");
    let status_now = runtime::status(&fx.db).unwrap();
    assert!(
        !status_now["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "plan_review_failed"),
        "{status_now}"
    );
    // A proposal not held is not submitted again as it is.
    assert!(
        queue
            .submit(Submission {
                tasks: Vec::new(),
                goals: Vec::new(),
                proposal: Some(proposal),
                owner: PlannerOwner {
                    origin: PlannerOrigin::Person,
                    workspace_id: None,
                },
            })
            .is_err()
    );
}

#[test]
fn a_proposal_with_an_interrupt_task_is_reviewed_first() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let plain = add(&mut queue, "plain", &[blocker], Priority::Normal);
    let urgent = add(&mut queue, "urgent", &[blocker], Priority::Interrupt);
    let older = submit(&mut queue, &[plain], None);
    let newer = submit(&mut queue, &[urgent], None);
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    supervise(&fx, &PlanWorkspace::default(), &reviewer);
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[0].contains(&format!("dagq proposal {newer}:")));
    assert!(prompts[1].contains(&format!("dagq proposal {older}:")));
    // The later one saw the earlier one as a proposal submitted before it.
    assert!(prompts[1].contains("\"title\":\"plain\""));
}

#[test]
fn a_ready_task_the_review_reopens_leaves_the_claim_for_a_planner() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let ready = add(&mut queue, "ready one", &[blocker], Priority::Normal);
    queue.transition(ready, TaskAction::BypassReview).unwrap();
    let running = add(&mut queue, "new one", &[blocker], Priority::Normal);
    // A ready task still in a proposal under review stays where it is.
    let held = add(&mut queue, "held one", &[blocker], Priority::Normal);
    let pending = add(&mut queue, "pending one", &[blocker], Priority::Normal);
    let proposal = submit(&mut queue, &[running], None);
    let active = submit(&mut queue, &[held, pending], None);
    queue.transition(held, TaskAction::BypassReview).unwrap();
    let reviewer = StubReviewer::new(&[
        json!({
            "verdict": "pass", "reasons": [], "summary": "ok",
            "reopen": [{"task_id": ready, "reason": "it must use the new API"},
                       {"task_id": blocker, "reason": "not ready"},
                       {"task_id": held, "reason": "in review"}]
        }),
        json!({"verdict": "pass", "reasons": [], "summary": "ok"}),
    ]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    assert_eq!(status(&mut queue, running), TaskStatus::Ready);
    // Out of the claim, in a proposal of its own, with a planner of the
    // runtime's that has the reason.
    assert_eq!(status(&mut queue, ready), TaskStatus::Submitted);
    let reopened = events(&mut queue, ready, "task_reopened");
    let own = ProposalId::new(reopened[0]["proposal_id"].as_i64().unwrap());
    assert_ne!(own, proposal);
    assert_eq!(
        queue.show_proposal(own).unwrap().status(),
        ProposalStatus::Revising
    );
    let finished = events(&mut queue, running, "plan_review_finished");
    assert_eq!(finished[0]["reopened"][0]["task_id"], json!(ready));
    assert!(
        finished[0]["reopen_skipped"][0]
            .as_str()
            .unwrap()
            .contains("not ready"),
        "{finished:?}"
    );
    assert!(
        finished[0]["reopen_skipped"][1]
            .as_str()
            .unwrap()
            .contains(&format!("is in proposal {active}")),
        "{finished:?}"
    );
    assert_eq!(status(&mut queue, held), TaskStatus::Ready);
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners[0].proposal_id, Some(own));
    let prompt = fs::read_to_string(
        planners_dir(&fx.db)
            .join(planners[0].id.to_string())
            .join("prompt.txt"),
    )
    .unwrap();
    assert!(prompt.contains("it must use the new API"), "{prompt}");
    queue.remove_dependency(ready, blocker).unwrap();
    assert!(queue.candidates().unwrap().is_empty());

    // Its planner submits it again as it is: the submitted task goes back
    // to plan review, and a pass readies it.
    queue
        .submit(Submission {
            tasks: Vec::new(),
            goals: Vec::new(),
            proposal: Some(own),
            owner: PlannerOwner {
                origin: PlannerOrigin::Runtime,
                workspace_id: planners[0].workspace_id.clone(),
            },
        })
        .unwrap();
    let passing = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    queue.add_dependency(ready, blocker).unwrap();
    supervise(&fx, &backend, &passing);
    assert_eq!(status(&mut queue, ready), TaskStatus::Ready);
    // Its planner, idle with nothing left to do, is asked to exit.
    let dir = planners_dir(&fx.db).join(planners[0].id.to_string());
    queue
        .register_planner_wrapper(planners[0].id, std::process::id())
        .unwrap();
    fs::write(planner_idle_marker(&dir), "{}").unwrap();
    supervise(&fx, &backend, &passing);
    assert_eq!(
        *backend.exits.lock().unwrap(),
        [planners[0].workspace_id.clone().unwrap()]
    );
}

/// A draft as the runtime or a job registers it: `origin` with `material`.
fn runtime_draft(
    queue: &mut SqliteQueue,
    title: &str,
    goal: Option<dagq::domain::GoalId>,
    origin: DraftOrigin,
    material: Value,
) -> TaskId {
    let id = queue
        .add(NewTask {
            title: title.into(),
            description: format!("{title}: found outside the task"),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Priority::Normal,
            kind: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: goal,
            context: String::new(),
        })
        .unwrap()
        .id();
    queue.record_draft_origin(id, origin, &material).unwrap();
    id
}

fn open_goal(queue: &mut SqliteQueue) -> dagq::domain::GoalId {
    queue
        .add_goal(NewGoal {
            title: "tidy the queue".into(),
            description: "d".into(),
            acceptance: "every draft is decided".into(),
            constraints: "no new tables".into(),
            doc: None,
            draft: false,
        })
        .unwrap()
        .id()
}

fn planner_prompt(db: &Path, planner: dagq::domain::PlannerId) -> String {
    fs::read_to_string(
        planners_dir(db)
            .join(planner.to_string())
            .join("prompt.txt"),
    )
    .unwrap()
}

/// Make `planner` alive (its wrapper is this test process) and idle.
fn idle(queue: &SqliteQueue, db: &Path, planner: dagq::domain::PlannerId) {
    queue
        .register_planner_wrapper(planner, std::process::id())
        .unwrap();
    let dir = planners_dir(db).join(planner.to_string());
    fs::write(planner_idle_marker(&dir), "{}").unwrap();
}

#[test]
fn drafts_of_the_runtime_get_planners_within_the_limit_and_a_persons_draft_none() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let source = add(&mut queue, "source", &[], Priority::Normal);
    let follow_up = runtime_draft(
        &mut queue,
        "follow",
        Some(goal),
        DraftOrigin::FollowUp,
        json!({"source_task_id": source.as_i64(), "source_run_id": null, "index": 0}),
    );
    let gap = runtime_draft(
        &mut queue,
        "gap",
        Some(goal),
        DraftOrigin::GoalGap,
        json!({"findings": ["the acceptance names a check nobody runs"]}),
    );
    // A person's draft (the fixture's blocker, and this one) gets none.
    let mine = add(&mut queue, "mine", &[], Priority::Normal);
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();

    // One runtime planner at a time: the older draft first.
    supervise(&fx, &backend, &reviewer);
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners.len(), 1, "{planners:?}");
    assert_eq!(planners[0].origin, PlannerOrigin::Runtime);
    assert_eq!(planners[0].draft_task_id, Some(follow_up));
    let opened = backend.opened();
    assert!(
        opened[0].1.ends_with(&format!("draft task {follow_up}")),
        "{opened:?}"
    );
    let prompt = planner_prompt(&fx.db, planners[0].id);
    for expected in [
        format!("draft task {follow_up}"),
        "## Where it came from: follow_up".to_owned(),
        "### Source task".to_owned(),
        "source: change the type of Foo".to_owned(),
        "## Goal".to_owned(),
        "every draft is decided".to_owned(),
        "no new tables".to_owned(),
        format!("- task {gap} (draft): gap"),
        format!("dagq submit {follow_up}"),
        format!("dagq cancel {follow_up}"),
        format!("dagq ask --task {follow_up} --kind planner_question --because scope"),
        "follow-up draft（task".to_owned(),
    ] {
        assert!(prompt.contains(&expected), "{expected}\n{prompt}");
    }
    let opened_events = events(&mut queue, follow_up, "draft_planner_opened");
    assert_eq!(opened_events.len(), 1);
    assert_eq!(opened_events[0]["attempt"], 1);
    assert_eq!(opened_events[0]["origin"], "follow_up");
    // The limit holds while that planner is at work.
    supervise(&fx, &backend, &reviewer);
    assert_eq!(queue.planners(false).unwrap().len(), 1);

    // A higher limit opens one for the goal's gap too, never for a
    // person's draft.
    supervise_with(
        &fx,
        &backend,
        &reviewer,
        &options(3, Duration::from_secs(3600)),
    );
    let planners = queue.planners(false).unwrap();
    assert_eq!(
        planners.iter().map(|p| p.draft_task_id).collect::<Vec<_>>(),
        [Some(follow_up), Some(gap)]
    );
    let prompt = planner_prompt(&fx.db, planners[1].id);
    assert!(
        prompt.contains("## Where it came from: goal_gap")
            && prompt.contains("the acceptance names a check nobody runs"),
        "{prompt}"
    );
    for draft in [TaskId::new(1), mine] {
        assert!(events(&mut queue, draft, "draft_planner_opened").is_empty());
    }

    // The follow_up's planner drops it (cancels it) and goes idle: the
    // runtime asks it to exit, and no planner is opened for it again.
    queue.transition(follow_up, TaskAction::Cancel).unwrap();
    idle(&queue, &fx.db, planners[0].id);
    supervise(&fx, &backend, &reviewer);
    assert_eq!(*backend.exits.lock().unwrap(), ["RT1".to_owned()]);
    queue
        .planner_exited(planners[0].id, std::process::id(), 0)
        .unwrap();
    supervise_with(
        &fx,
        &backend,
        &reviewer,
        &options(3, Duration::from_secs(3600)),
    );
    assert!(queue.planner(planners[0].id).unwrap().closed_at.is_some());
    assert_eq!(
        events(&mut queue, follow_up, "draft_planner_opened").len(),
        1
    );

    // The gap's planners end without deciding it: another is opened each
    // time, three in all, and then the inbox is told.
    for attempt in 2..=4 {
        let open = queue
            .planners(false)
            .unwrap()
            .into_iter()
            .find(|p| p.draft_task_id == Some(gap))
            .unwrap();
        queue.register_planner_wrapper(open.id, 1).unwrap();
        queue.planner_exited(open.id, 1, 0).unwrap();
        // One pass closes it, the next opens the next one.
        supervise(&fx, &backend, &reviewer);
        supervise(&fx, &backend, &reviewer);
        let opened = events(&mut queue, gap, "draft_planner_opened");
        assert_eq!(opened.len(), attempt.min(3), "{opened:?}");
    }
    let exhausted = events(&mut queue, gap, "draft_planner_exhausted");
    assert_eq!(exhausted.len(), 1, "{exhausted:?}");
    assert!(queue.planners(false).unwrap().is_empty());
    let status = runtime::status_for(&fx.db, None).unwrap();
    let attention: Vec<_> = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["task_id"] == gap.as_i64())
        .collect();
    assert_eq!(attention.len(), 1, "{status}");
    assert_eq!(attention[0]["next"], "decide the draft in a planner");
    assert_eq!(attention[0]["kind"], "draft_planner_exhausted");
    // Nothing of this took a plan review.
    assert!(reviewer.prompts().is_empty());
}

#[test]
fn a_planner_question_answer_is_typed_into_its_planner_or_carried_by_a_new_one() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let first = runtime_draft(
        &mut queue,
        "first",
        Some(goal),
        DraftOrigin::FollowUp,
        json!({"source_task_id": 1, "source_run_id": null, "index": 0}),
    );
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    let planner = queue.planners(false).unwrap()[0].clone();
    assert_eq!(planner.draft_task_id, Some(first));

    // The planner cannot decide: it asks and stops.
    let asked = queue
        .ask(NewAsk {
            kind: AskKind::PlannerQuestion,
            task_id: Some(first),
            run_id: None,
            question: "is this in the goal?".into(),
            options: vec!["adopt".into(), "cancel".into(), "keep_draft".into()],
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
        })
        .unwrap()
        .ask;
    idle(&queue, &fx.db, planner.id);
    // Waiting for the answer, it is not asked to exit.
    supervise(&fx, &backend, &reviewer);
    assert!(backend.exits.lock().unwrap().is_empty());
    assert!(backend.texts().is_empty());
    queue.answer(asked.id, "cancel").unwrap();
    supervise(&fx, &backend, &reviewer);
    assert_eq!(
        backend.texts(),
        [(
            "RT1".to_owned(),
            format!("answer to ask {}: cancel", asked.id)
        )]
    );
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    assert_eq!(events(&mut queue, first, "ask_delivered").len(), 1);
    // It is at work on the answer: not asked to exit yet.
    assert!(backend.exits.lock().unwrap().is_empty());

    // A draft whose planner is gone before the answer: a new planner
    // carries it.
    let second = runtime_draft(
        &mut queue,
        "second",
        Some(goal),
        DraftOrigin::FollowUp,
        json!({"source_task_id": 1, "source_run_id": null, "index": 1}),
    );
    let asked = queue
        .ask(NewAsk {
            kind: AskKind::PlannerQuestion,
            task_id: Some(second),
            run_id: None,
            question: "split it?".into(),
            options: vec!["adopt".into(), "cancel".into(), "keep_draft".into()],
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
        })
        .unwrap()
        .ask;
    // The open question keeps the draft from a planner of its own.
    queue.transition(first, TaskAction::Cancel).unwrap();
    queue
        .planner_exited(planner.id, std::process::id(), 0)
        .unwrap();
    supervise(&fx, &backend, &reviewer);
    let left = queue.planners(false).unwrap();
    assert!(left.is_empty(), "{left:?}");
    queue.answer(asked.id, "adopt").unwrap();
    supervise(&fx, &backend, &reviewer);
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners.len(), 1);
    assert_eq!(planners[0].draft_task_id, Some(second));
    let prompt = planner_prompt(&fx.db, planners[0].id);
    assert!(
        prompt.contains(&format!("answer to ask {}: adopt", asked.id))
            && prompt.contains("split it?"),
        "{prompt}"
    );
    assert!(queue.asks(Default::default()).unwrap().is_empty());

    // keep_draft leaves the draft to a person's planner: no runtime
    // planner is opened for it again.
    let asked = queue
        .ask(NewAsk {
            kind: AskKind::PlannerQuestion,
            task_id: Some(second),
            run_id: None,
            question: "keep it?".into(),
            options: vec!["adopt".into(), "cancel".into(), "keep_draft".into()],
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
        })
        .unwrap()
        .ask;
    queue.answer(asked.id, "keep_draft").unwrap();
    queue.register_planner_wrapper(planners[0].id, 1).unwrap();
    queue.planner_exited(planners[0].id, 1, 0).unwrap();
    for _ in 0..3 {
        supervise(&fx, &backend, &reviewer);
    }
    assert!(queue.planners(false).unwrap().is_empty());
    let opened = events(&mut queue, second, "draft_planner_opened");
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert_eq!(opened[0]["ask_id"], json!(asked.id.as_i64() - 1));
    assert_eq!(events(&mut queue, second, "planner_answer_closed").len(), 1);
    assert!(queue.asks(Default::default()).unwrap().is_empty());
}

#[test]
fn a_verdict_on_a_task_edited_during_its_review_is_not_applied_and_the_review_runs_again() {
    for first in [
        json!({"verdict": "pass", "reasons": [], "summary": "sound", "actions": []}),
        json!({"verdict": "revise", "reasons": ["split it"], "summary": "too big"}),
        json!({"verdict": "concern", "reasons": ["maybe done"], "summary": "doubtful"}),
    ] {
        let fx = fixture();
        let mut queue = SqliteQueue::open(&fx.db).unwrap();
        let blocker = TaskId::new(1);
        let two = add(&mut queue, "two", &[blocker], Priority::Normal);
        let three = add(&mut queue, "three", &[blocker], Priority::Normal);
        let proposal = submit(&mut queue, &[two, three], None);
        let reviewer = StubReviewer::new(&[
            first.clone(),
            json!({"verdict": "pass", "reasons": [], "summary": "fine now", "actions": []}),
        ])
        .editing(&fx.db, three);
        let backend = PlanWorkspace::default();
        let outcome = supervise(&fx, &backend, &reviewer);
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        // The first verdict is dropped with why; the second job reads the
        // edited task and its pass is applied.
        let prompts = reviewer.prompts();
        assert_eq!(prompts.len(), 2, "{first}");
        assert!(!prompts[0].contains("edited while its review ran"));
        assert!(
            prompts[1].contains("edited while its review ran"),
            "{}",
            prompts[1]
        );
        let discarded = events(&mut queue, two, "plan_review_discarded");
        assert_eq!(discarded.len(), 1, "{first}");
        assert_eq!(discarded[0]["verdict"], first["verdict"]);
        assert_eq!(discarded[0]["edited"], json!([three]));
        let finished = events(&mut queue, two, "plan_review_finished");
        assert_eq!(finished.len(), 1, "{first}");
        assert_eq!(finished[0]["summary"], "fine now");
        assert_eq!(finished[0]["attempt"], 1);
        assert_eq!(status(&mut queue, two), TaskStatus::Ready);
        assert_eq!(status(&mut queue, three), TaskStatus::Ready);
        assert_eq!(
            queue.show_proposal(proposal).unwrap().status(),
            ProposalStatus::Accepted
        );
        assert!(queue.asks(Default::default()).unwrap().is_empty());
        let (outcome, error): (String, String) = Connection::open(&fx.db)
            .unwrap()
            .query_row(
                "SELECT outcome, error FROM plan_reviews ORDER BY id LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(outcome, "interrupted");
        assert!(error.contains(&format!("task {three}")), "{error}");
    }
}

#[test]
fn edits_before_a_review_or_to_another_proposal_leave_its_verdict_applied() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let early = add(&mut queue, "early", &[blocker], Priority::Normal);
    let other = add(&mut queue, "other", &[blocker], Priority::Normal);
    let reviewed = submit(&mut queue, &[early], None);
    let later = submit(&mut queue, &[other], None);
    // Edited before its review: the review reads the new contents.
    queue
        .edit_task(
            early,
            TaskEdit {
                description: Some("edited before its review".into()),
                ..TaskEdit::default()
            },
        )
        .unwrap();
    // The first job (of `reviewed`) edits the task of `later`.
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "pass", "reasons": [], "summary": "sound", "actions": []}),
    ])
    .editing(&fx.db, other);
    let backend = PlanWorkspace::default();
    let outcome = supervise(&fx, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(
        prompts[0].contains("edited before its review"),
        "{}",
        prompts[0]
    );
    assert!(
        prompts[1].contains("edited while its review ran"),
        "{}",
        prompts[1]
    );
    for (task, proposal) in [(early, reviewed), (other, later)] {
        assert_eq!(status(&mut queue, task), TaskStatus::Ready);
        assert_eq!(
            queue.show_proposal(proposal).unwrap().status(),
            ProposalStatus::Accepted
        );
        assert!(events(&mut queue, task, "plan_review_discarded").is_empty());
    }
}

/// A task of `title`, `description` and `acceptance`, waiting for the
/// blocker.
fn add_text(queue: &mut SqliteQueue, title: &str, description: &str, acceptance: &str) -> TaskId {
    queue
        .add(NewTask {
            kind: None,
            title: title.into(),
            description: description.into(),
            acceptance: acceptance.into(),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Priority::Normal,
            dependencies: vec![TaskId::new(1)],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap()
        .id()
}

#[test]
fn the_prompt_lists_each_tasks_duplicate_candidates_but_not_the_proposals_own() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    // Landed: completed with a commit on main; and one canceled.
    let done = add_text(
        &mut queue,
        "runtime: search index for the plan review",
        "adds src/infrastructure/search_index.rs",
        "it finds tasks",
    );
    let dropped = add_text(
        &mut queue,
        "plan review candidates, first try",
        "abandoned",
        "none",
    );
    queue.transition(dropped, TaskAction::Cancel).unwrap();
    let raw = Connection::open(&fx.db).unwrap();
    raw.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    raw.execute(
        "UPDATE tasks SET status = 'completed' WHERE id = ?1",
        [done.as_i64()],
    )
    .unwrap();
    raw.execute(
        "INSERT INTO landed_commits (run_id, task_id, commit_sha, message, landed_at)
         VALUES ('run-landed', ?1, 'abc123', 'runtime: search index in src/infrastructure/search_index.rs', 'now')",
        [done.as_i64()],
    )
    .unwrap();
    let asked = add_text(
        &mut queue,
        "runtime: search index candidates in the plan review",
        "change src/infrastructure/search_index.rs",
        "the candidates are listed",
    );
    let twin = add_text(
        &mut queue,
        "runtime: search index candidates twin",
        "also src/infrastructure/search_index.rs",
        "twin",
    );
    let lone = add_text(&mut queue, "qwertyuiop", "zxcvbnm", "asdfghjkl");
    submit(&mut queue, &[asked, twin, lone], None);
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "pass", "reasons": [], "summary": "sound", "actions": []}),
    ]);
    let outcome = supervise(&fx, &PlanWorkspace::default(), &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let prompt = reviewer.prompts().remove(0);
    assert!(
        prompt.contains("Candidates of duplicates and of changes already made"),
        "{prompt}"
    );
    let candidates: Vec<Value> = prompt
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line.get("related").is_some())
        .collect();
    assert_eq!(
        candidates
            .iter()
            .map(|c| c["task_id"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        [asked.as_i64(), twin.as_i64(), lone.as_i64()]
    );
    let own = [asked.as_i64(), twin.as_i64(), lone.as_i64()];
    for entry in &candidates {
        for related in entry["related"].as_array().unwrap() {
            assert!(!own.contains(&related["id"].as_i64().unwrap()), "{entry}");
        }
        for hit in entry["search"].as_array().unwrap() {
            let task = hit["task_id"].as_i64().or(hit["id"].as_i64()).unwrap();
            assert!(!own.contains(&task), "{entry}");
        }
    }
    // The related task that landed, with its clues and status.
    let first = &candidates[0];
    let related = first["related"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == json!(done))
        .unwrap_or_else(|| panic!("{first}"));
    assert_eq!(related["status"], "completed");
    assert!(
        related["clues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["value"] == "src/infrastructure/search_index.rs"),
        "{related}"
    );
    // Search finds the landed task, its commit and the canceled task.
    let search = first["search"].as_array().unwrap();
    let find = |kind: &str, id: Value| {
        search
            .iter()
            .find(|hit| hit["kind"] == kind && hit["id"] == id)
            .unwrap_or_else(|| panic!("{kind} {id} not in {first}"))
    };
    assert_eq!(find("task", json!(done))["status"], "completed");
    assert_eq!(find("task", json!(dropped))["status"], "canceled");
    let commit = find("commit", json!("abc123"));
    assert_eq!(commit["task_id"], json!(done));
    assert_eq!(commit["status"], "completed");
    assert!(search.len() <= 5 && first["related"].as_array().unwrap().len() <= 5);
    // A task nothing resembles has empty lists.
    assert_eq!(candidates[2]["related"], json!([]), "{}", candidates[2]);
    assert_eq!(candidates[2]["search"], json!([]), "{}", candidates[2]);
}

/// A task that declares `paths`, waiting for the draft blocker.
fn add_paths(queue: &mut SqliteQueue, title: &str, paths: &[&str]) -> TaskId {
    queue
        .add(NewTask {
            kind: None,
            title: title.into(),
            description: format!("{title}: the long description"),
            acceptance: format!("{title} works"),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            priority: Priority::Normal,
            dependencies: vec![TaskId::new(1)],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap()
        .id()
}

/// The plan review prompt of a proposal whose task touches the hotspot
/// `seed.txt`, with a ready task on another file and, with `meet`, one on
/// the hotspot too: the one ready task, the one on the hotspot and the
/// JSON lines of the prompt.
fn hotspot_prompt(meet: bool) -> (TaskId, Option<TaskId>, TaskId, Vec<Value>) {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let apart = add_paths(&mut queue, "qwertyuiop apart", &["other.txt"]);
    let on_hot = meet.then(|| add_paths(&mut queue, "asdfghjkl hot", &["seed.txt"]));
    for id in std::iter::once(apart).chain(on_hot) {
        queue.transition(id, TaskAction::BypassReview).unwrap();
    }
    let own = add_paths(&mut queue, "zxcvbnm own", &["seed.txt"]);
    submit(&mut queue, &[own], None);
    Connection::open(&fx.db)
        .unwrap()
        .execute(
            "INSERT INTO run_events(task_id, kind, payload) VALUES (1, 'conflict_precheck', ?1)",
            [json!({"main": "m", "conflicts": ["seed.txt"]}).to_string()],
        )
        .unwrap();
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "pass", "reasons": [], "summary": "sound", "actions": []}),
    ]);
    let outcome = supervise(&fx, &PlanWorkspace::default(), &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let prompt = reviewer.prompts().remove(0);
    for expected in [
        "Ready and in-progress tasks, in summary",
        "Files each task of the proposal is expected to touch",
        "not pass but revise, saying in the reason which part to cut",
    ] {
        assert!(prompt.contains(expected), "{expected:?} not in {prompt}");
    }
    assert!(!prompt.contains("are left out of this list"), "{prompt}");
    let lines = prompt
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    (apart, on_hot, own, lines)
}

/// Task 591: the ready tasks are listed in summary with their expected
/// files, each hotspot names the tasks expected to touch it, and only a
/// ready task on the same hotspot as the proposal's is given in full.
#[test]
fn the_prompt_lists_ready_tasks_in_summary_and_in_full_those_on_the_proposals_hotspot() {
    for meet in [true, false] {
        let (apart, on_hot, own, lines) = hotspot_prompt(meet);
        let find = |what: &dyn Fn(&Value) -> bool| lines.iter().find(|line| what(line)).cloned();
        let summary = |id: TaskId| {
            find(&|line| line["id"] == json!(id) && line.get("expected_files").is_some())
        };
        let full =
            |id: TaskId| find(&|line| line["id"] == json!(id) && line.get("description").is_some());
        let hot = find(&|line| line["path"] == "seed.txt" && line.get("conflicts").is_some())
            .unwrap_or_else(|| panic!("no hotspot in {lines:?}"));
        assert_eq!(hot["proposal_tasks"], json!([own]), "{hot}");
        let apart_summary = summary(apart).unwrap_or_else(|| panic!("{lines:?}"));
        assert_eq!(apart_summary["expected_files"], json!(["other.txt"]));
        assert_eq!(apart_summary["status"], "ready");
        assert_eq!(apart_summary["dependencies"], json!([1]));
        assert_eq!(apart_summary.get("description"), None);
        assert!(full(apart).is_none(), "{lines:?}");
        let own_files =
            find(&|line| line["task_id"] == json!(own) && line.get("expected_files").is_some())
                .unwrap_or_else(|| panic!("{lines:?}"));
        assert_eq!(own_files["expected_files"], json!(["seed.txt"]));
        match on_hot {
            Some(on_hot) => {
                assert_eq!(hot["queued_tasks"], json!([on_hot]), "{hot}");
                assert_eq!(summary(on_hot).unwrap()["full_text_below"], true);
                let full = full(on_hot).unwrap_or_else(|| panic!("{lines:?}"));
                assert_eq!(full["description"], "asdfghjkl hot: the long description");
            }
            None => assert_eq!(hot["queued_tasks"], json!([]), "{hot}"),
        }
    }
}

/// Plan review predicts the weight of each submitted task (ADR-0079
/// decision 2): recorded per task, again on a later review; predictions
/// missing, malformed or not covering the tasks are not recorded, and the
/// verdict is applied all the same.
#[test]
fn plan_review_records_each_tasks_predicted_weight_and_goes_on_without_one() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let two = add(&mut queue, "two", &[blocker], Priority::Normal);
    let three = add(&mut queue, "three", &[blocker], Priority::Normal);
    let proposal = submit(&mut queue, &[two, three], None);
    let predict = |task: TaskId, tokens: u64| {
        json!({"task_id": task, "size": "M", "nature": "implementation", "uncertainty": 0.4,
               "expected_output_tokens": tokens, "rework_probability": 0.2, "reason": "a module"})
    };
    let again = |queue: &mut SqliteQueue| {
        queue
            .submit(Submission {
                tasks: Vec::new(),
                goals: Vec::new(),
                proposal: Some(proposal),
                owner: PlannerOwner {
                    origin: PlannerOrigin::Runtime,
                    workspace_id: None,
                },
            })
            .unwrap();
    };
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "revise", "reasons": ["vague"], "summary": "vague",
               "predictions": [predict(two, 40_000), predict(three, 9_000)]}),
        // Task three is missing: none is recorded.
        json!({"verdict": "revise", "reasons": ["still vague"], "summary": "vague",
               "predictions": [predict(two, 1_000)]}),
        // Not the shape of a prediction: none is recorded, the verdict passes.
        json!({"verdict": "pass", "reasons": [], "summary": "sound",
               "predictions": [{"task_id": two, "size": "XL"}]}),
    ]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    // The prompt asks for one prediction per submitted task.
    let prompt = &reviewer.prompts()[0];
    for expected in [
        "estimate the weight of each submitted task of the proposal (tasks 2, 3)",
        "\"predictions\": [{\"task_id\": int, \"size\": \"S\" | \"M\" | \"L\"",
        "expected_output_tokens is the output tokens (thinking included) of one worker run",
    ] {
        assert!(prompt.contains(expected), "{expected:?} not in {prompt}");
    }
    let first = events(&mut queue, two, "plan_review_finished");
    assert_eq!(first[0]["prediction_error"], Value::Null);
    let recorded = events(&mut queue, two, "task_weight_predicted");
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["proposal_id"], json!(proposal));
    assert_eq!(
        recorded[0]["plan_review_id"],
        events(&mut queue, two, "plan_review_started")[0]["plan_review_id"]
    );
    assert_eq!(
        recorded[0]["prediction"],
        json!({"size": "M", "nature": "implementation", "uncertainty": 0.4,
               "expected_output_tokens": 40_000, "rework_probability": 0.2,
               "reason": "a module"})
    );
    // The stub's session wrote no transcript naming a model.
    assert_eq!(recorded[0]["model"], Value::Null);
    assert_eq!(recorded[0]["effort"], Value::Null);
    let three_recorded = events(&mut queue, three, "task_weight_predicted");
    assert_eq!(three_recorded.len(), 1);
    assert_eq!(
        three_recorded[0]["prediction"]["expected_output_tokens"],
        9_000
    );

    again(&mut queue);
    supervise(&fx, &backend, &reviewer);
    let finished = events(&mut queue, two, "plan_review_finished");
    assert_eq!(finished[1]["decision"], "revise");
    assert_eq!(finished[1]["prediction_error"], "task 3 is not predicted");
    assert_eq!(events(&mut queue, two, "task_weight_predicted").len(), 1);

    again(&mut queue);
    supervise(&fx, &backend, &reviewer);
    let finished = events(&mut queue, two, "plan_review_finished");
    assert_eq!(finished[2]["decision"], "pass");
    assert!(
        finished[2]["prediction_error"]
            .as_str()
            .unwrap()
            .starts_with("the predictions are malformed"),
        "{}",
        finished[2]
    );
    assert_eq!(status(&mut queue, two), TaskStatus::Ready);
    assert_eq!(status(&mut queue, three), TaskStatus::Ready);
    assert_eq!(events(&mut queue, two, "task_weight_predicted").len(), 1);
    assert_eq!(events(&mut queue, three, "task_weight_predicted").len(), 1);
    // No run was claimed, so nothing predicted at a claim either.
    assert!(queue.show(two).unwrap().runs.is_empty());
}
