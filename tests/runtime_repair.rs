//! Runtime tests: Repairs of long background work.
mod common;
mod runtime_support;

use runtime_support::*;

/// The worker of the `long_background` tests: it commits, leaves an orphan
/// `sleep` in its worktree (its pid in `bg.pid` of the run directory), goes
/// idle with background work running, and writes its receipt once the
/// orphan is gone.
const ORPHAN_AGENT: &str = r#"
commit work
bg="$(dirname "$RECEIPT")/bg.pid"
( sleep 300 >/dev/null 2>&1 & echo $! > "$bg.tmp"; mv "$bg.tmp" "$bg" )
idle_bg
pid=$(cat "$bg")
while kill -0 "$pid" 2>/dev/null; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#;

/// Supervise the fixture's task with [`ORPHAN_AGENT`], a background alert
/// after one second, and `recovery` as the recovery job's script, on a
/// thread.
fn supervise_long_background(
    db: &Path,
    repo: &Path,
    recovery: &str,
) -> (
    Arc<TestWorkspace>,
    Arc<TestReviewer>,
    thread::JoinHandle<Result<Value>>,
) {
    let backend = Arc::new(TestWorkspace::new(db, false, ORPHAN_AGENT));
    let reviewer = Arc::new(
        TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[recovery.to_owned()]),
    );
    let options = SuperviseOptions {
        stall: Some(dagq::domain::stall::StallConfig {
            background_alert_secs: 1,
            ..Default::default()
        }),
        ..supervise_options(4, true)
    };
    let supervisor = {
        let (db, repo, backend, reviewer) = (
            db.to_owned(),
            repo.to_owned(),
            backend.clone(),
            reviewer.clone(),
        );
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    (backend, reviewer, supervisor)
}

/// A recovery job's script that prints `verdict` with `PID` replaced by the
/// orphan's pid.
fn recovery_verdict(verdict: &Value) -> String {
    let text = verdict.to_string().replace("\"PID\"", "$(cat bg.pid)");
    format!("printf '%s\\n' \"{}\"", text.replace('"', "\\\""))
}

/// Task 360 (ADR-0047 decisions 39 and 40): background work past its
/// threshold starts the recovery job with the run's processes; its repair
/// stops only the orphan of the run's worktree, recorded as
/// `auto_repaired`, and the session goes on to its receipt without an ask.
#[test]
fn a_long_background_alert_is_repaired_by_stopping_the_orphan_of_the_worktree() {
    let (_dir, repo, db) = fixture();
    let (backend, reviewer, supervisor) = supervise_long_background(
        &db,
        &repo,
        &recovery_verdict(&json!({
            "verdict": "repair",
            "confidence": "high",
            "diagnosis": "an orphan sleep holds the session",
            "actions": [{"action": "stop_processes", "pids": ["PID"]}],
        })),
    );
    let outcome = supervisor.join().unwrap().unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    let pid: u32 = fs::read_to_string(Path::new(run.run_dir().unwrap()).join("bg.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!pid_alive(pid));
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["alert"], "long_background");
    assert_eq!(requested[0]["attempt"], 1);
    assert_eq!(requested[0]["threshold_secs"], 1);
    assert_eq!(requested[0]["background_tasks"][0]["command"], "cargo test");
    let prompts = reviewer.triage_prompts();
    assert_eq!(prompts.len(), 1);
    let (prompt, cwd) = &prompts[0];
    assert_eq!(cwd, Path::new(run.run_dir().unwrap()));
    for part in [
        "long_background",
        &format!("- pid {pid} (parent "),
        "stop_processes",
        "\"verdict\": \"repair\" | \"escalate\"",
    ] {
        assert!(prompt.contains(part), "{part}: {prompt}");
    }
    let repaired = payloads(&detail, "auto_repaired");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["layer"], "recovery");
    assert_eq!(repaired[0]["repair"], "stop_processes");
    assert_eq!(repaired[0]["processes"][0]["pid"], pid);
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["escalated"], false);
    assert_eq!(finished[0]["applied"], json!(["stop_processes"]));
    assert_eq!(finished[0]["confidence"], "high");
    assert!(stalled_asks(&queue).is_empty());
}

/// Wait for the `stalled` ask of a `long_background` test, check that the
/// orphan still runs, then stop it as a person would and let the run end.
fn escalated_long_background(
    db: &Path,
    backend: &TestWorkspace,
    supervisor: thread::JoinHandle<Result<Value>>,
) -> (dagq::domain::Ask, dagq::domain::TaskDetail) {
    wait_until(db, Duration::from_secs(30), |queue| {
        !stalled_asks(queue).is_empty()
    });
    let mut queue = SqliteQueue::open(db).unwrap();
    let ask = stalled_asks(&queue).remove(0);
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let pid: u32 = fs::read_to_string(Path::new(run.run_dir().unwrap()).join("bg.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid_alive(pid), "the orphan was stopped");
    assert!(payloads(&queue.show(TaskId::new(1)).unwrap(), "auto_repaired").is_empty());
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .unwrap();
    let outcome = supervisor.join().unwrap().unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    (ask, queue.show(TaskId::new(1)).unwrap())
}

/// A repair that names a process outside the run is not applied at all:
/// the runtime refuses the verdict and asks the inbox (`recovery_failed`);
/// the ask closes itself once the session moves on.
#[test]
fn a_recovery_repair_of_a_process_outside_the_run_becomes_an_ask() {
    let (_dir, repo, db) = fixture();
    let mut outsider = std::process::Command::new("/bin/sleep")
        .arg("300")
        .current_dir(repo.parent().unwrap())
        .spawn()
        .unwrap();
    let (backend, _reviewer, supervisor) = supervise_long_background(
        &db,
        &repo,
        &recovery_verdict(&json!({
            "verdict": "repair",
            "confidence": "high",
            "diagnosis": "two sleeps",
            "actions": [{"action": "stop_processes", "pids": ["PID", outsider.id()]}],
        })),
    );
    let (ask, detail) = escalated_long_background(&db, &backend, supervisor);
    assert!(pid_alive(outsider.id()));
    let _ = outsider.kill();
    let _ = outsider.wait();
    assert_eq!(ask.options, ["wait", "intervene"]);
    for part in [
        "alert: long_background",
        "Why a person: recovery_failed",
        &format!(
            "pid {} is not one of the run's own processes",
            outsider.id()
        ),
        "Diagnosis: two sleeps",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["escalated"], true);
    assert_eq!(finished[0]["reason_category"], "recovery_failed");
    assert_eq!(finished[0]["ask_id"], json!(ask.id));
    assert_eq!(ask.reason_category, dagq::domain::AskReason::RecoveryFailed);
    let queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let resolved = payloads(&detail, "stall_resolved");
    assert_eq!(resolved.len(), 1, "{resolved:?}");
    assert_eq!(resolved[0]["threshold"], "background_alert_secs");
    assert_eq!(resolved[0]["outcome"], "resolved_by_itself");
}

/// A repair the job is not sure of is not applied: it becomes an ask with
/// the job's actions as the recommendation and its options added.
#[test]
fn a_recovery_repair_of_low_confidence_becomes_an_ask() {
    let (_dir, repo, db) = fixture();
    let (backend, _reviewer, supervisor) = supervise_long_background(
        &db,
        &repo,
        &recovery_verdict(&json!({
            "verdict": "repair",
            "confidence": "low",
            "diagnosis": "maybe a slow test",
            "actions": [{"action": "stop_processes", "pids": ["PID"]}],
            "options": ["stop it"],
        })),
    );
    let (ask, detail) = escalated_long_background(&db, &backend, supervisor);
    assert_eq!(ask.options, ["wait", "intervene", "stop it"]);
    for part in [
        "confidence low",
        "Recommended: [{\"action\":\"stop_processes\"",
        "Why a person: recovery_failed",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished[0]["escalated"], true);
    assert_eq!(finished[0]["confidence"], "low");
}
