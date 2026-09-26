//! Runtime tests: a session idle with a receipt for an older commit is asked
//! once to rewrite it (task 357).
mod common;
mod runtime_support;

use runtime_support::*;

/// Commit a second change after the receipt, leaving it for the older commit.
const COMMIT_AGAIN: &str =
    "printf 'more\\n' > more.txt && git add more.txt && git commit -q -m more";

/// Wait for the request to rewrite the receipt to be typed.
const AWAIT_NUDGE: &str = "while ! grep -q 'cannot accept a receipt for another commit' \"$MESSAGE\" 2>/dev/null; do sleep 0.05; done";

/// The texts typed into the sessions that ask for the receipt's rewrite.
fn nudges(backend: &TestWorkspace) -> Vec<String> {
    backend
        .texts()
        .into_iter()
        .map(|(_, text)| text)
        .filter(|text| text.contains("cannot accept a receipt for another commit"))
        .collect()
}

/// Task 205's worker: it wrote its receipt, committed again and went idle.
/// The supervisor asks it once to rewrite the receipt for its clean HEAD;
/// it does, and the run goes on to validation and review like any other.
#[test]
fn a_session_idle_with_a_stale_receipt_is_asked_once_and_the_rewrite_goes_on() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "commit work; old=\"$(git rev-parse HEAD)\"; receipt \"$old\"; {COMMIT_AGAIN}; idle\n{AWAIT_NUDGE}\nreceipt \"$(git rev-parse HEAD)\"; idle; await_exit"
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    let head = run.result_commit().unwrap().to_string();
    let old = git_out(&repo, &["rev-parse", &format!("{head}~1")]);
    let texts = nudges(&backend);
    assert_eq!(texts.len(), 1, "{:?}", backend.texts());
    for expected in [
        run.id().to_string(),
        format!("names commit {old} while the clean worktree HEAD is {head}"),
        format!("rewrite the receipt at {}", run.receipt_path().unwrap()),
        "do not run /exit".to_owned(),
    ] {
        assert!(
            texts[0].contains(&expected),
            "{expected:?} not in {}",
            texts[0]
        );
    }
    let nudged = payloads(&detail, "stale_receipt_nudged");
    assert_eq!(nudged.len(), 1, "{nudged:?}");
    assert_eq!(nudged[0]["phase"], "session");
    assert_eq!(nudged[0]["receipt_commit"], old);
    assert_eq!(nudged[0]["head"], head);
    assert_eq!(
        payloads(&detail, "stale_receipt_resolved"),
        [&json!({"phase": "session", "outcome": "rewritten"})]
    );
    // The stall watch took no part in it.
    assert!(payloads(&detail, "stall_nudged").is_empty());
}

/// A session that answers the request without rewriting its receipt is not
/// asked again: the run goes on as before, and validation rejects the
/// receipt for the older commit.
#[test]
fn a_stale_receipt_left_as_it_is_goes_on_as_before() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "commit work; receipt \"$(git rev-parse HEAD)\"; {COMMIT_AGAIN}; idle\n{AWAIT_NUDGE}\nidle; await_exit"
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(nudges(&backend).len(), 1, "{:?}", backend.texts());
    assert_eq!(
        payloads(&detail, "stale_receipt_resolved"),
        [&json!({"phase": "session", "outcome": "unchanged"})]
    );
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(validated[0]["accepted"], false, "{}", validated[0]);
    assert!(
        validated[0].to_string().contains("commit_mismatch"),
        "{}",
        validated[0]
    );
}

/// A session whose receipt already names its HEAD is not asked.
#[test]
fn a_receipt_for_the_head_is_not_asked_to_be_rewritten() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(nudges(&backend).is_empty());
    assert!(payloads(&detail, "stale_receipt_nudged").is_empty());
}

/// Task 205 itself: the resumed session rebased onto main (a clean head on
/// top of it) and went idle with the receipt still naming the old commit.
/// Asked once, it rewrites the receipt and the attempt resolves the run,
/// which lands.
#[test]
fn a_resumed_session_idle_after_its_rebase_with_the_old_receipt_is_asked_once() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        &format!("await_message; resolve; idle\n{AWAIT_NUDGE}\nreceipt \"$(git rev-parse HEAD)\"; idle; await_exit"),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    let landed = detail.runs[0].clone();
    assert_landed(&repo, &landed, "second", &first_landed);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(nudges(&backend).len(), 1, "{:?}", backend.texts());
    let nudged = payloads(&detail, "stale_receipt_nudged");
    assert_eq!(nudged.len(), 1, "{nudged:?}");
    assert_eq!(nudged[0]["phase"], "resume");
    assert_eq!(nudged[0]["attempt"], 1);
    assert_eq!(nudged[0]["receipt_commit"], json!(run.result_commit()));
    assert_eq!(
        payloads(&detail, "stale_receipt_resolved"),
        [&json!({"phase": "resume", "attempt": 1, "outcome": "rewritten"})]
    );
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["outcome"], "resolved");
}

/// A resumed session that answers the request without rewriting the receipt
/// ends its attempt as before (unresolved); the next attempt is asked anew
/// only if it leaves a stale receipt again, which this one does not.
#[test]
fn a_resumed_session_that_leaves_the_old_receipt_ends_its_attempt_as_before() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (_run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        &format!(
            "await_message; mark=\"$(dirname \"$RECEIPT\")/attempted\"\nif [ -f \"$mark\" ]; then receipt \"$(git rev-parse HEAD)\"; idle; await_exit; exit 0; fi\n: > \"$mark\"; resolve; idle\n{AWAIT_NUDGE}\nidle; await_exit"
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(nudges(&backend).len(), 1, "{:?}", backend.texts());
    assert_eq!(
        payloads(&detail, "stale_receipt_resolved"),
        [&json!({"phase": "resume", "attempt": 1, "outcome": "unchanged"})]
    );
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 2, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["outcome"], "unresolved");
    assert_eq!(finished[1]["outcome"], "resolved");
}
