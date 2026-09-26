use crate::common;

use common::cli::*;

use serde_json::Value;

/// ADR-0041 decisions 7 and 8: `submit` bundles drafts into a proposal
/// owned by the planner's workspace; a submitted task is never claimed,
/// and only plan review or `ready --bypass-review` makes it ready.
#[test]
fn submit_bundles_drafts_into_a_proposal_that_plan_review_or_a_bypass_readies() {
    let (_dir, db) = queue();
    ok(&db, &["goal", "add", "planned", "--draft"]);
    ok(&db, &["add", "in goal", "--goal", "1"]);
    ok(&db, &["add", "alone"]);
    ok(&db, &["add", "left out"]);

    let submitted = submit_from(&db, Some("W-1"), None, &["2", "--goal", "1"]);
    assert!(
        submitted.status.success(),
        "{}",
        String::from_utf8_lossy(&submitted.stderr)
    );
    let proposal: Value = serde_json::from_slice(&submitted.stdout).unwrap();
    assert_eq!(proposal["id"], 1);
    assert_eq!(proposal["status"], "submitted");
    assert_eq!(proposal["task_ids"], serde_json::json!([1, 2]));
    assert_eq!(proposal["goal_ids"], serde_json::json!([1]));
    assert_eq!(
        proposal["owner"],
        serde_json::json!({"origin": "person", "workspace_id": "W-1"})
    );
    assert_eq!(ok(&db, &["show", "1"])["task"]["status"], "submitted");
    let listed = ok(&db, &["list", "--status", "submitted"]);
    assert_eq!(listed["total"], 2);
    assert_eq!(ok(&db, &["candidates"]), serde_json::json!([]));
    let graph = ok(&db, &["graph"]);
    assert_eq!(graph["candidates"], serde_json::json!([]));
    assert_eq!(ok(&db, &["status"])["proposals"][0]["id"], 1);
    assert_eq!(ok(&db, &["proposal", "list"])["proposals"][0]["id"], 1);
    assert_eq!(
        ok(&db, &["proposal", "show", "1"])["task_ids"],
        serde_json::json!([1, 2])
    );
    assert_eq!(ok(&db, &["goal", "list"])[0]["tasks"]["submitted"], 1);
    // A submitted task is still edited in place.
    ok(&db, &["edit", "1", "--acceptance", "sharper"]);

    // Without the bypass, ready is plan review's.
    for id in ["1", "3"] {
        let refused = invoke(&db, &["ready", id]);
        assert!(!refused.status.success());
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("pass --bypass-review"),
            "{}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }
    let again = submit_from(&db, None, None, &["1"]);
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("task 1 already belongs to proposal 1")
    );
    let bad_origin = submit_from(&db, None, Some("robot"), &["3"]);
    assert!(!bad_origin.status.success());
    assert!(!invoke(&db, &["submit"]).status.success(), "no members");

    let runtime = submit_from(&db, None, Some("runtime"), &["3"]);
    let runtime: Value = serde_json::from_slice(&runtime.stdout).unwrap();
    assert_eq!(
        runtime["owner"],
        serde_json::json!({"origin": "runtime", "workspace_id": null})
    );

    let bypassed = ok(&db, &["ready", "1", "--bypass-review"]);
    assert_eq!(bypassed["status"], "ready");
    let shown = ok(&db, &["show", "1", "--full"]);
    assert!(
        shown["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "review_bypassed")
    );
    assert_eq!(
        ok(&db, &["candidates"]).as_array().unwrap().len(),
        0,
        "draft goal"
    );
    ok(&db, &["draft", "2"]);
    assert_eq!(ok(&db, &["show", "2"])["task"]["status"], "draft");
}

/// `proposal withdraw` releases a submitted proposal's goal and tasks as
/// drafts for another proposal; the observer and the reviewer may not run it.
#[test]
fn proposal_withdraw_releases_the_members_for_another_proposal() {
    let (_dir, db) = queue();
    ok(&db, &["goal", "add", "planned", "--draft"]);
    ok(&db, &["add", "in goal", "--goal", "1"]);
    ok(&db, &["add", "alone"]);
    let submitted = submit_from(&db, Some("W-1"), None, &["2", "--goal", "1"]);
    assert!(submitted.status.success());

    for role in ["observer", "reviewer"] {
        let refused = invoke_as(Some(role), &db, &["proposal", "withdraw", "1"]);
        assert!(!refused.status.success(), "{role}");
        ok_as(role, &db, &["proposal", "show", "1"]);
    }
    let withdrawn = ok(&db, &["proposal", "withdraw", "1"]);
    assert_eq!(withdrawn["status"], "canceled");
    assert_eq!(withdrawn["task_ids"], serde_json::json!([1, 2]));
    for id in ["1", "2"] {
        assert_eq!(ok(&db, &["show", id])["task"]["status"], "draft");
    }
    assert_eq!(
        ok(&db, &["proposal", "list"]),
        serde_json::json!({"proposals": []})
    );
    let again = invoke(&db, &["proposal", "withdraw", "1"]);
    assert!(!again.status.success());
    assert!(
        String::from_utf8_lossy(&again.stderr)
            .contains("only a submitted or revising proposal is withdrawn")
    );

    let resubmitted = submit_from(&db, Some("W-2"), None, &["2", "--goal", "1"]);
    let resubmitted: Value = serde_json::from_slice(&resubmitted.stdout).unwrap();
    assert_eq!(resubmitted["id"], 2);
    assert_eq!(resubmitted["task_ids"], serde_json::json!([1, 2]));
    assert_eq!(resubmitted["goal_ids"], serde_json::json!([1]));
}

/// ADR-0041 decision 10: `lint` checks the fixed rules of TASKs and of a
/// proposal's members, printing each violation with its code, and an empty
/// list when the plan passes. It only reads, so the observer and the
/// reviewer may run it.
#[test]
fn lint_reports_violations_by_code_and_passes_a_sound_plan() {
    let (_dir, db) = queue();
    let sound = ["--acceptance", "works", "--verify", "cargo test"];
    ok(&db, &[&["add", "done"][..], &sound].concat());
    ok(&db, &["ready", "1", "--bypass-review"]);
    ok(&db, &["cancel", "1"]);
    ok(&db, &[&["add", "sound"][..], &sound].concat());
    ok(&db, &["add", "loose", "--depends-on", "1"]);
    ok(
        &db,
        &[&["add", "Sound", "--depends-on", "2"][..], &sound].concat(),
    );

    let passed = ok(&db, &["lint", "2"]);
    assert_eq!(passed, serde_json::json!({"tasks": [2], "violations": []}));

    let submitted = submit_from(&db, None, None, &["3", "4"]);
    assert!(submitted.status.success());
    let linted = ok(&db, &["lint", "2", "--proposal", "1"]);
    assert_eq!(linted["tasks"], serde_json::json!([2, 3, 4]));
    let found: Vec<(i64, &str)> = linted["violations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| (v["task_id"].as_i64().unwrap(), v["code"].as_str().unwrap()))
        .collect();
    assert_eq!(
        found,
        [
            (2, "duplicate_title"),
            (3, "depends_on_canceled"),
            (3, "unscoped_without_verification"),
            (3, "blank_acceptance"),
            (4, "duplicate_title"),
        ]
    );
    assert_eq!(
        linted["violations"][1]["reason"],
        "it depends on task 1, which was canceled and never completes"
    );
    // A task given both directly and through its proposal is linted once.
    assert_eq!(
        ok(&db, &["lint", "4", "--proposal", "1"])["tasks"],
        serde_json::json!([4, 3])
    );
    assert_eq!(
        ok_as("observer", &db, &["lint", "2"])["violations"],
        serde_json::json!([])
    );
    assert_eq!(
        ok_as("reviewer", &db, &["lint", "2"])["violations"],
        serde_json::json!([])
    );

    // A missing task or proposal is an error, and lint needs a target.
    assert!(!invoke(&db, &["lint", "99"]).status.success());
    assert!(!invoke(&db, &["lint", "--proposal", "9"]).status.success());
    assert!(!invoke(&db, &["lint"]).status.success());
}
