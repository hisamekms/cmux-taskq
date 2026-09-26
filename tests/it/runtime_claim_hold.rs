//! Runtime tests: holding new claims while the load average is above
//! `--max-load` (task 327).
use crate::runtime_support;

use runtime_support::*;
use std::sync::atomic::{AtomicBool, AtomicU64};

/// The load average this file's supervisor reads, as `f64` bits.
static LOAD: AtomicU64 = AtomicU64::new(0);

fn load() -> Option<f64> {
    Some(f64::from_bits(LOAD.load(Ordering::SeqCst)))
}

fn set_load(value: f64) {
    LOAD.store(value.to_bits(), Ordering::SeqCst);
}

fn queue_events(db: &Path, kind: &str) -> Vec<Value> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| event.payload)
        .collect()
}

fn alert_kinds(stats: &Value) -> Vec<&str> {
    stats["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|alert| alert["kind"].as_str())
        .collect()
}

/// Above `--max-load` the ready task is not claimed and `claim_held` is
/// recorded once; `status` shows the hold on the supervisor and `stats`
/// raises `claim_held` for the free slots instead of `idle_slots`. Once
/// the load falls back the task is claimed, `claim_resumed` is recorded
/// and `stats` counts the hold by its reason. A supervisor started after
/// the holder stopped, the load still high, records the hold anew under
/// its own token, so `status` and `stats` keep showing it.
#[test]
fn no_run_is_claimed_while_the_load_is_above_the_threshold() {
    let (_dir, repo, db) = fixture();
    set_load(40.0);
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    // A one-shot supervisor holds, records it, and stops (`down --wait`).
    let once = SuperviseOptions {
        max_load: Some(16.0),
        load_average: load,
        ..supervise_options(2, true)
    };
    let outcome = supervise_with(&db, &repo, &backend, &once).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert_eq!(queue_events(&db, "claim_held").len(), 1);
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["claim_holds"]["held"], Value::Null, "{stats}");

    // The next supervisor starts while the load is still high.
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        max_load: Some(16.0),
        load_average: load,
        stop: stop.clone(),
        ..supervise_options(2, false)
    };
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |_| {
        queue_events(&db, "claim_held").len() == 2
    });
    // Several more passes: still nothing claimed, and the hold is not
    // recorded again.
    thread::sleep(TEST_TICK * 6);
    let mut held = queue_events(&db, "claim_held");
    assert_eq!(held.len(), 2, "{held:?}");
    assert_ne!(held[0]["supervisor"], held[1]["supervisor"]);
    let held = held.split_off(1);
    assert_eq!(held[0]["reason"], "load_average");
    assert_eq!(held[0]["value"], 40.0);
    assert_eq!(held[0]["threshold"], 16.0);
    assert!(
        held[0]["message"].as_str().unwrap().contains("--max-load"),
        "{held:?}"
    );
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        let detail = queue.show(TaskId::new(1)).unwrap();
        assert!(detail.runs.is_empty());
        assert_eq!(detail.task.status(), TaskStatus::Ready);
    }
    let status = runtime::status(&db).unwrap();
    let hold = &status["supervisors"][0]["claim_hold"];
    assert_eq!(hold["reason"], "load_average", "{status}");
    assert_eq!(hold["supervisor"], held[0]["supervisor"]);
    assert!(hold["since"].is_string());
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["claim_holds"]["held"]["reason"], "load_average");
    assert_eq!(
        stats["claim_holds"]["held"]["supervisor"],
        held[0]["supervisor"]
    );
    assert_eq!(stats["claim_holds"]["held"]["threshold"], 16.0);
    let kinds = alert_kinds(&stats);
    assert!(kinds.contains(&"claim_held"), "{stats}");
    assert!(!kinds.contains(&"idle_slots"), "{stats}");
    let alert = stats["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|alert| alert["kind"] == "claim_held")
        .unwrap();
    assert_eq!(alert["value"], 2);

    // The load falls back: the task is claimed and runs to its rest.
    set_load(1.0);
    wait_until(&db, Duration::from_secs(60), |queue| {
        queue
            .show(TaskId::new(1))
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| queue.run_lease(run.id()).unwrap().is_none())
    });
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let resumed = queue_events(&db, "claim_resumed");
    assert_eq!(resumed.len(), 1, "{resumed:?}");
    assert_eq!(resumed[0]["reason"], "load_average");
    assert_eq!(resumed[0]["supervisor"], held[0]["supervisor"]);
    assert_eq!(queue_events(&db, "claim_held").len(), 2);
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["claim_holds"]["count"], 2, "{stats}");
    assert_eq!(
        stats["claim_holds"]["by_reason"]["load_average"]["count"],
        2
    );
    assert_eq!(stats["claim_holds"]["held"], Value::Null);
    assert!(!alert_kinds(&stats).contains(&"claim_held"), "{stats}");
    assert!(
        runtime::status(&db).unwrap()["supervisors"]
            .as_array()
            .unwrap()
            .iter()
            .all(|supervisor| supervisor.get("claim_hold").is_none())
    );
}

/// Without `--max-load` (the library's default) no load holds claims.
#[test]
fn no_threshold_holds_nothing() {
    let (_dir, repo, db) = fixture();
    fn very_high() -> Option<f64> {
        Some(1000.0)
    }
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = SuperviseOptions {
        load_average: very_high,
        ..supervise_options(1, true)
    };
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    backend.join();
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 1, "{outcome}");
    assert!(queue_events(&db, "claim_held").is_empty());
}
