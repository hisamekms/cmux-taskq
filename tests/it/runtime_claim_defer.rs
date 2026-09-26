//! Runtime tests: deferring the claim of a task whose files meet a run in
//! flight on a conflict hotspot (ADR-0069).
use crate::runtime_support;

use dagq::{application::DraftPlannerStore, domain::stats::ConflictConfig};
use runtime_support::*;

const HOT: &str = "docs/hot.md";

fn add_task(queue: &mut SqliteQueue, title: &str, paths: &[&str], priority: Priority) -> TaskId {
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            // The stand-in worker commits change.txt.
            paths: paths
                .iter()
                .map(|path| (*path).to_owned())
                .chain(["change.txt".to_owned()])
                .collect(),
            priority,
            kind: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    task.id()
}

fn events(db: &Path, kind: &str) -> Vec<(Option<TaskId>, Value)> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| (event.task_id, event.payload))
        .collect()
}

fn runs_of(db: &Path, task: TaskId) -> usize {
    SqliteQueue::open(db)
        .unwrap()
        .show(task)
        .unwrap()
        .runs
        .len()
}

fn options(defer_max_secs: i64) -> SuperviseOptions {
    SuperviseOptions {
        conflicts: Some(ConflictConfig {
            defer_max_secs,
            ..ConflictConfig::default()
        }),
        ..supervise_options(3, true)
    }
}

/// A task whose declared paths meet a run in flight on a hotspot is not
/// claimed, and the next candidate that does not meet is; the deferral is
/// recorded once with the files and the run in the way, and `status` and
/// `stats` show it. A task of interrupt priority is claimed over the same
/// files, and the deferred task is claimed once its deferral has lasted
/// `defer_max_secs`, which holds across supervisors.
#[test]
fn a_task_meeting_a_run_on_a_hotspot_waits_and_the_next_one_is_claimed() {
    let (_dir, repo, db) = fixture();
    fs::create_dir(repo.join("docs")).unwrap();
    fs::write(repo.join(HOT), "hot\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "hot file"]);
    let (hot, near, apart) = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        // Task 1 (the fixture's) declares no paths and has no related
        // landing: nothing is expected of it.
        let hot = add_task(&mut queue, "edits the hot file", &[HOT], Priority::Normal);
        let near = add_task(&mut queue, "edits the docs", &["docs/**"], Priority::Normal);
        let apart = add_task(
            &mut queue,
            "edits elsewhere",
            &["other.txt"],
            Priority::Normal,
        );
        // Three conflicts on the hot file make it an alert of
        // `conflict_hotspots`; a draft task carries them.
        let old = queue
            .add(NewTask {
                title: "landed long ago".into(),
                description: "d".into(),
                acceptance: "a".into(),
                verification_commands: Vec::new(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
                priority: Priority::Normal,
                kind: None,
                dependencies: Vec::new(),
                goal_dependencies: Vec::new(),
                goal_id: None,
                context: String::new(),
            })
            .unwrap()
            .id();
        for main in ["m1", "m2", "m3"] {
            queue
                .record_task_event(
                    old,
                    "integration_deferred",
                    json!({"conflicts": [HOT], "main": main}),
                )
                .unwrap();
        }
        (hot, near, apart)
    };
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(
        stats["conflict_hotspots"]["files"][0]["alert"], true,
        "{stats}"
    );

    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &options(3600)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(runs_of(&db, TaskId::new(1)), 1);
    assert_eq!(runs_of(&db, hot), 1);
    assert_eq!(runs_of(&db, apart), 1, "the next candidate is claimed");
    assert_eq!(runs_of(&db, near), 0, "the task meeting the hot run waits");
    let deferred = events(&db, "claim_deferred");
    assert_eq!(deferred.len(), 1, "{deferred:?}");
    let (task, payload) = &deferred[0];
    assert_eq!(*task, Some(near));
    assert_eq!(payload["reason"], "hot_files");
    assert_eq!(payload["files"], json!([HOT]));
    assert_eq!(payload["runs"].as_array().unwrap().len(), 1, "{payload}");
    assert_eq!(payload["runs"][0]["task_id"], json!(hot));
    assert_eq!(payload["max_secs"], 3600);
    assert!(payload["message"].as_str().unwrap().contains(HOT));

    let status = runtime::status(&db).unwrap();
    let open = status["claim_deferrals"].as_array().unwrap();
    assert_eq!(open.len(), 1, "{status}");
    assert_eq!(open[0]["task_id"], json!(near));
    assert_eq!(open[0]["files"], json!([HOT]));
    assert!(open[0]["since"].is_string());
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    let deferrals = &stats["claim_deferrals"];
    assert_eq!(deferrals["count"], 1, "{deferrals}");
    assert_eq!(deferrals["by_file"][HOT], 1);
    assert_eq!(deferrals["deferred"][0]["task_id"], json!(near));

    // An interrupt over the same file is claimed; the deferred task still
    // waits and is not recorded again.
    let interrupt = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_task(&mut queue, "stops the line", &[HOT], Priority::Interrupt)
    };
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &options(3600)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(runs_of(&db, interrupt), 1, "an interrupt is not deferred");
    assert_eq!(runs_of(&db, near), 0);
    assert_eq!(events(&db, "claim_deferred").len(), 1);
    assert!(events(&db, "claim_deferral_ended").is_empty());

    // Past the limit, counted from the first deferral, the task is claimed.
    thread::sleep(Duration::from_millis(1100));
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &options(1)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(runs_of(&db, near), 1, "an expired deferral is claimed");
    let ended = events(&db, "claim_deferral_ended");
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0].0, Some(near));
    assert_eq!(ended[0].1["why"], "expired");
    assert!(ended[0].1["deferred_secs"].as_i64().unwrap() >= 1);
    assert_eq!(events(&db, "claim_deferred").len(), 1);
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["claim_deferrals"], json!([]), "{status}");
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["claim_deferrals"]["by_end"]["expired"]["count"], 1);
    assert_eq!(stats["claim_deferrals"]["deferred"], json!([]));
}
