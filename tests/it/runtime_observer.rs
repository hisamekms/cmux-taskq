//! Runtime tests: The observer.
use crate::runtime_support;

use runtime_support::*;

/// The observer's provider double: the headless job is a shell script in the
/// observation's directory, with the environment `observe` gives the agent.
struct ObserverProvider {
    script: String,
}

impl AgentProvider for ObserverProvider {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn command(&self, _: &TaskRun, _: &str) -> Result<CommandSpec> {
        bail!("the observer has no run")
    }
    fn resume_command(&self, _: &TaskRun) -> Result<CommandSpec> {
        bail!("the observer has no run")
    }
    fn headless_command(&self, cwd: &Path, prompt: &str, allowed: &[&str]) -> Result<CommandSpec> {
        assert!(prompt.contains("You are the observer"), "{prompt}");
        assert_eq!(allowed, ["Bash(dagq:*)"]);
        let mut command = CommandSpec::new("/bin/sh");
        command.current_dir(cwd).arg("-c").arg(&self.script);
        Ok(command)
    }
    fn review_command(&self, _: &TaskRun, _: &str) -> Result<CommandSpec> {
        bail!("the observer reviews no run")
    }
}

fn observe_options(mode: dagq::observer::ObserveMode) -> dagq::observer::ObserveOptions {
    dagq::observer::ObserveOptions {
        mode,
        since: None,
        dry_run: false,
        timeout: Duration::from_secs(60),
        dagq: PathBuf::from(env!("CARGO_BIN_EXE_dagq")),
    }
}

fn queue_events(db: &Path, kind: &str) -> Vec<Value> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT payload FROM run_events WHERE kind=?1 ORDER BY id")
        .unwrap()
        .query_map([kind], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|p| serde_json::from_str(&p.unwrap()).unwrap())
        .collect()
}

#[test]
fn observe_records_findings_and_a_blocked_ask_and_advances_the_cursor() {
    use dagq::observer::{ObserveMode, observe, read_cursor};
    let (_dir, _repo, db) = fixture();
    // `dagq` is first on PATH and the queue is in DAGQ_QUEUE; the state
    // changes the prompt forbids are refused by the CLI itself.
    let provider = ObserverProvider {
        script: r#"
set -e
printf '%s' "$DAGQ_ROLE" > role.txt
q() { dagq --db "$DAGQ_QUEUE" "$@" > /dev/null; }
q finding record --kind stall --task 1 --summary 'task 1 waits for a slot' --evidence 1
q finding record --kind stall --task 1 --summary 'task 1 waits for a slot' --evidence 1 --evidence 2
q finding record --kind capacity --queue --subject idle_slots --summary 'slots idle'
q ask --kind blocked --because recovery_failed --finding 2 --question 'slots idle while task 1 is ready' --option 'leave it' --cmux /usr/bin/true
q ask --kind blocked --because recovery_failed --finding 2 --question 'the same alert again' --cmux /usr/bin/true
if q ready 1 2> ready.err; then exit 3; fi
if q ask --kind decide --because recovery_failed --task 1 --question 'decide?' 2> ask.err; then exit 4; fi
if q goal ready 1 2> goal.err; then exit 5; fi
if q note --task 1 --text 'seen' 2> note.err; then exit 6; fi
if q goal add --draft 'claim faster' 2> draft.err; then exit 7; fi
echo 'recorded 2 findings, updated 1, wrote 1 ask'
"#
        .into(),
    };
    assert_eq!(read_cursor(&db).unwrap(), None);
    let first = observe(&db, &provider, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(first["outcome"], "succeeded", "{first}");
    assert_eq!(
        (
            &first["findings_recorded"],
            &first["findings_updated"],
            &first["asks"]
        ),
        (&json!(2), &json!(1), &json!(1))
    );
    assert_eq!(first["since"], Value::Null);
    let cursor = first["cursor"].as_i64().unwrap();
    assert!(cursor >= 0);
    assert_eq!(read_cursor(&db).unwrap(), Some(EventId::new(cursor)));
    let dir = PathBuf::from(first["dir"].as_str().unwrap());
    assert_eq!(
        dir.parent().unwrap(),
        db.canonicalize()
            .unwrap()
            .parent()
            .unwrap()
            .join("observer")
    );
    assert_eq!(
        fs::read_to_string(dir.join("role.txt")).unwrap(),
        "observer"
    );
    for denied in ["ready.err", "ask.err", "goal.err", "note.err", "draft.err"] {
        assert!(
            fs::read_to_string(dir.join(denied))
                .unwrap()
                .contains("observer may not change queue state"),
            "{denied}"
        );
    }
    assert!(
        fs::read_to_string(dir.join("output.log"))
            .unwrap()
            .contains("recorded 2 findings")
    );
    assert!(
        fs::read_to_string(dir.join("prompt.md"))
            .unwrap()
            .contains("\"stats\"")
    );
    // Nothing changed state: the task is still ready and no goal was added.
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Ready
    );
    assert!(queue.show_goal(GoalId::new(1)).is_err());
    let asks = queue
        .asks(dagq::infrastructure::asks::AskQuery::default())
        .unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind.as_str(), "blocked");
    assert_eq!(asks[0].task_id, None);
    assert_eq!(asks[0].asked_by, "observer");
    assert_eq!(asks[0].finding_id, Some(dagq::domain::FindingId::new(2)));
    let findings = queue
        .findings(&dagq::domain::FindingQuery::default())
        .unwrap();
    assert_eq!(findings.len(), 2);
    let stall = findings.iter().find(|f| f.finding.kind == "stall").unwrap();
    assert_eq!(stall.finding.occurrences, 2);
    assert_eq!(stall.finding.recorded_by, "observer");
    assert_eq!(queue_events(&db, "observe_started").len(), 1);
    assert_eq!(
        queue_events(&db, "observe_finished"),
        std::slice::from_ref(&first)
    );

    // The next observation reads past the saved cursor; a failed one keeps it.
    let failing = ObserverProvider {
        script: "exit 7".into(),
    };
    let second = observe(&db, &failing, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(second["since"], cursor);
    assert_eq!(second["outcome"], "failed");
    assert_eq!(second["exit_code"], 7);
    assert_eq!(second["cursor_saved"], false);
    assert_eq!(read_cursor(&db).unwrap(), Some(EventId::new(cursor)));

    // A dry run only returns the prompt.
    let dry = observe(
        &db,
        &failing,
        &dagq::observer::ObserveOptions {
            dry_run: true,
            since: Some(EventId::new(0)),
            ..observe_options(ObserveMode::Daily)
        },
    )
    .unwrap();
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["since"], 0);
    let prompt = dry["prompt"].as_str().unwrap();
    assert!(prompt.contains("daily observation"), "{prompt}");
    assert!(prompt.contains("ask --kind blocked"), "{prompt}");
    assert!(prompt.contains("finding record --kind"), "{prompt}");
    assert!(prompt.contains("--finding ID"), "{prompt}");
    assert!(!prompt.contains("goal add --draft"), "{prompt}");
    assert!(prompt.contains("\"findings\""), "{prompt}");
    assert!(
        prompt.contains("slots idle while task 1 is ready"),
        "{prompt}"
    );
    assert!(prompt.contains("task 1 waits for a slot"), "{prompt}");
    assert!(prompt.contains("idle_slots"), "{prompt}");
    assert_eq!(queue_events(&db, "observe_started").len(), 2);

    // An agent that cannot start is an error outcome, not a failed observe.
    let broken = observe(
        &db,
        &TestProvider {
            script: String::new(),
            db: db.clone(),
        },
        &observe_options(ObserveMode::Daily),
    )
    .unwrap();
    assert_eq!(broken["outcome"], "error");
    assert!(
        broken["error"]
            .as_str()
            .unwrap()
            .contains("no headless execution")
    );
    // The daily one reads the last 24 hours and leaves the cursor alone.
    assert_eq!(broken["since"], 0);
    assert_eq!(read_cursor(&db).unwrap(), Some(EventId::new(cursor)));
}

#[test]
fn observe_kills_an_agent_past_its_timeout() {
    use dagq::observer::{ObserveMode, observe};
    let (_dir, _repo, db) = fixture();
    let slow = ObserverProvider {
        script: "sleep 30".into(),
    };
    let started = Instant::now();
    let outcome = observe(
        &db,
        &slow,
        &dagq::observer::ObserveOptions {
            timeout: Duration::from_millis(300),
            ..observe_options(ObserveMode::Hourly)
        },
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(outcome["outcome"], "error");
    assert!(
        outcome["error"]
            .as_str()
            .unwrap()
            .contains("did not finish")
    );
    assert_eq!(outcome["cursor_saved"], false);
}

/// A Claude Code stand-in for the supervisor's observer: `--version` for
/// the preflight, and in print mode (`-p`) a finding through the queue CLI it
/// finds first on PATH.
fn observer_claude_stub(db: &Path) -> PathBuf {
    let stub = db.parent().unwrap().join("claude-observer-stub");
    fs::write(
        &stub,
        r#"#!/bin/sh
if [ "$1" = "-p" ]; then
  mode=hourly
  case "$*" in *"daily observation"*) mode=daily ;; esac
  exec dagq --db "$DAGQ_QUEUE" finding record --goal 1 --kind observed --subject "$mode" --summary "observed by $DAGQ_ROLE"
fi
printf 'test provider\n'
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    stub
}

#[test]
fn supervisor_starts_the_observer_on_its_interval_without_a_run_slot() {
    let (_dir, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        queue
            .transition(TaskId::new(1), TaskAction::Cancel)
            .unwrap();
        queue
            .add_goal(NewGoal {
                title: "observed".into(),
                description: String::new(),
                acceptance: String::new(),
                constraints: String::new(),
                doc: None,
                draft: false,
            })
            .unwrap();
    }
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = SuperviseOptions {
        observe_interval: Duration::from_secs(3600),
        observe_daily: true,
        ..SuperviseOptions::new(1, true)
    };
    let supervise_observed = || {
        runtime::supervise(
            &db,
            &repo,
            &backend,
            &observer_claude_stub(&db),
            Path::new(env!("CARGO_BIN_EXE_dagq")),
            &options,
        )
        .unwrap()
    };
    // Nothing was ever observed: the daily observation is due, then the
    // hourly one; `--once` waits for each before it exits.
    let outcome = supervise_observed();
    assert_eq!(outcome["runs"], json!([]));
    let finished = queue_events(&db, "observe_finished");
    assert_eq!(
        finished
            .iter()
            .map(|f| (f["mode"].as_str().unwrap(), f["outcome"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        [("daily", "succeeded"), ("hourly", "succeeded")]
    );
    assert!(finished.iter().all(|f| f["findings_recorded"] == 1));
    let mut findings = SqliteQueue::open(&db)
        .unwrap()
        .findings(&dagq::domain::FindingQuery {
            target: Some(dagq::domain::FindingTarget::Goal(GoalId::new(1))),
            ..Default::default()
        })
        .unwrap();
    findings.sort_by_key(|f| f.finding.id);
    assert_eq!(
        findings
            .iter()
            .map(|f| (f.finding.subject.as_str(), f.finding.summary.as_str()))
            .collect::<Vec<_>>(),
        [
            ("daily", "observed by observer"),
            ("hourly", "observed by observer")
        ]
    );
    assert!(dagq::observer::read_cursor(&db).unwrap().is_some());
    // Within the interval nothing is due again, even for another supervisor.
    supervise_observed();
    assert_eq!(queue_events(&db, "observe_started").len(), 2);
    // An interval of 0 disables the observer.
    let disabled = SuperviseOptions {
        observe_interval: Duration::ZERO,
        ..options.clone()
    };
    fs::remove_file(db.parent().unwrap().join("observer/cursor")).unwrap();
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_events SET created_at='2000-01-01T00:00:00.000Z' WHERE kind LIKE 'observe_%'",
            [],
        )
        .unwrap();
    runtime::supervise(
        &db,
        &repo,
        &backend,
        &observer_claude_stub(&db),
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &disabled,
    )
    .unwrap();
    assert_eq!(queue_events(&db, "observe_started").len(), 2);
    // Once the interval passed, the next pass observes again.
    supervise_observed();
    assert_eq!(queue_events(&db, "observe_started").len(), 4);
}
