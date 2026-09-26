use crate::common;

use common::cli::*;

mod stats {
    use std::collections::HashMap;

    use dagq::domain::{
        EventId, GoalId, RunEvent, RunId, TaskId,
        stats::{LiveSnapshot, SlotSnapshot, StatsQuery, stats, timestamp_millis},
    };
    use serde_json::{Value, json};

    use super::{invoke, ok};

    /// Builds run events one after another; `at` is minutes after 12:00.
    #[derive(Default)]
    struct Events(Vec<RunEvent>);

    impl Events {
        fn push(&mut self, task: i64, run: Option<&str>, kind: &str, minute: i64, payload: Value) {
            self.0.push(RunEvent {
                id: EventId::new(i64::try_from(self.0.len()).unwrap() + 1),
                task_id: Some(TaskId::new(task)),
                goal_id: None,
                run_id: run.map(|run| RunId::new(run).unwrap()),
                kind: kind.to_owned(),
                payload,
                created_at: format!(
                    "2026-09-23T{:02}:{:02}:00.000Z",
                    12 + minute / 60,
                    minute % 60
                ),
            });
        }

        fn run(&mut self, task: i64, run: &str, kind: &str, minute: i64) {
            self.push(task, Some(run), kind, minute, json!({}));
        }

        fn status(&mut self, task: i64, run: &str, kind: &str, minute: i64, status: &str) {
            self.push(task, Some(run), kind, minute, json!({"status": status}));
        }

        fn last_id(&self) -> i64 {
            self.0.last().unwrap().id.as_i64()
        }
    }

    /// Task → goal, as the queue maps them.
    fn goals<const N: usize>(pairs: [(i64, Option<i64>); N]) -> HashMap<TaskId, Option<GoalId>> {
        pairs
            .into_iter()
            .map(|(task, goal)| (TaskId::new(task), goal.map(GoalId::new)))
            .collect()
    }

    /// 12:00 plus `minute` minutes, in unix seconds.
    fn at(minute: i64) -> i64 {
        timestamp_millis("2026-09-23T12:00:00Z").unwrap() / 1000 + minute * 60
    }

    fn value(stats: &impl serde::Serialize) -> Value {
        serde_json::to_value(stats).unwrap()
    }

    #[test]
    fn timestamps_parse_the_queue_format() {
        assert_eq!(timestamp_millis("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(timestamp_millis("1970-01-02T00:00:01.5Z"), Some(86_401_500));
        assert_eq!(
            timestamp_millis("2026-09-23 12:00:00"),
            Some(1_790_164_800_000)
        );
        assert_eq!(
            timestamp_millis("2000-02-29T00:00:00Z"),
            Some(951_782_400_000)
        );
        for bad in [
            "",
            "2026-09-23",
            "2026-13-01T00:00:00Z",
            "2026-09-23Tx:00:00Z",
        ] {
            assert_eq!(timestamp_millis(bad), None, "{bad}");
        }
    }

    /// What each run was claimed with and the load over its intervals
    /// (task 197): on the run, per version and load band, and per
    /// verification command; the cmux failures per load band. Runs from
    /// before any of it was recorded have nulls, and the older items stay.
    #[test]
    fn versions_loads_and_verification_times_come_from_the_events() {
        let mut events = Events::default();
        let claim = |dagq: &str, load: f64| {
            json!({
                "from": "ready", "to": "in_progress", "provider": "claude",
                "dagq_version": dagq, "claude_version": "2.1.0",
                "rustc_release": "1.93.0", "rustc_host": "aarch64-apple-darwin",
                "parallel": 3, "slots": 1, "load_avg": load,
            })
        };
        let load = |mean: f64, max: f64| json!({"load_avg_mean": mean, "load_avg_max": max});
        let verify = |command: &str, secs: f64, exit_code: i64| {
            json!({
                "phase": "integration", "command": command, "exit_code": exit_code,
                "duration_secs": secs, "load_avg_mean": 20.0, "load_avg_max": 30.0,
            })
        };
        events.push(1, Some("a"), "run_claimed", 0, claim("0.4.0-dev+aaa", 2.5));
        events.push(1, Some("a"), "receipt_observed", 10, load(5.0, 9.0));
        events.push(
            1,
            Some("a"),
            "validation_finished",
            12,
            json!({"status": "awaiting_integration", "load_avg_mean": 3.0, "load_avg_max": 4.0}),
        );
        events.push(
            1,
            Some("a"),
            "verification_command",
            20,
            verify("cargo fmt --all --check", 2.0, 0),
        );
        events.push(
            1,
            Some("a"),
            "verification_command",
            22,
            verify("cargo llvm-cov", 300.0, 0),
        );
        events.push(
            1,
            Some("a"),
            "verification_command",
            22,
            json!({"phase": "recheck", "command": "cargo llvm-cov", "duration_secs": 1.0}),
        );
        events.run(1, "a", "run_integrated", 30);
        events.push(
            2,
            Some("b"),
            "run_claimed",
            40,
            claim("0.4.0-dev+bbb", 70.0),
        );
        events.push(
            2,
            Some("b"),
            "backend_call_failed",
            41,
            json!({"op": "capture", "load_avg": 70.0}),
        );
        events.push(
            2,
            Some("b"),
            "backend_call_failed",
            42,
            json!({"op": "capture", "load_avg": 5.0}),
        );
        events.push(
            2,
            Some("b"),
            "backend_call_failed",
            42,
            json!({"op": "capture", "load_avg": null}),
        );
        events.push(
            2,
            Some("b"),
            "verification_command",
            50,
            verify("cargo llvm-cov", 100.0, 101),
        );
        events.push(
            2,
            Some("b"),
            "verification_command",
            51,
            verify("cargo llvm-cov", 200.0, 0),
        );
        events.run(2, "b", "run_integrated", 60);
        events.run(3, "c", "run_claimed", 70);
        events.status(3, "c", "validation_finished", 80, "failed");
        let report = value(&stats(
            &events.0,
            &goals([(1, None), (2, None), (3, None)]),
            at(120),
            SlotSnapshot::default(),
            &StatsQuery::default(),
            &LiveSnapshot::default(),
        ));
        let runs = report["runs"].as_array().unwrap();
        let a = &runs[0];
        assert_eq!(a["dagq_version"], "0.4.0-dev+aaa");
        assert_eq!(a["claude_version"], "2.1.0");
        assert_eq!(a["rustc_release"], "1.93.0");
        assert_eq!(a["rustc_host"], "aarch64-apple-darwin");
        assert_eq!(a["claim_parallel"], 3);
        assert_eq!(a["claim_slots"], 1);
        assert_eq!(a["claim_load_avg"], 2.5);
        assert_eq!(
            a["load"]["work"],
            json!({"mean": 5.0, "max": 9.0, "band": "4-8"})
        );
        assert_eq!(
            a["load"]["validate"],
            json!({"mean": 3.0, "max": 4.0, "band": "0-4"})
        );
        assert_eq!(
            a["load"]["verify"],
            json!({"mean": 20.0, "max": 30.0, "band": "16-32"})
        );
        assert_eq!(a["load_band"], "4-8");
        // Only the claim's load: the band is its.
        assert_eq!(runs[1]["load_band"], "64+");
        assert_eq!(runs[1]["load"]["work"], Value::Null);
        // A run claimed without any of it.
        let c = &runs[2];
        for key in [
            "dagq_version",
            "claude_version",
            "rustc_release",
            "claim_load_avg",
            "load_band",
        ] {
            assert_eq!(c[key], Value::Null, "{key}");
        }
        assert_eq!(
            c["load"],
            json!({"work": null, "validate": null, "verify": null})
        );
        // The older items are all still there.
        for key in [
            "work",
            "validate",
            "wait_to_land",
            "startup",
            "status",
            "kind",
        ] {
            assert!(c.get(key).is_some(), "{key}");
        }

        let versions = &report["versions"];
        let dagq = versions["dagq"].as_array().unwrap();
        assert_eq!(dagq.len(), 3);
        assert_eq!(dagq[0]["version"], "0.4.0-dev+aaa");
        assert_eq!(dagq[0]["runs"], 1);
        assert_eq!(dagq[0]["work"]["median"], 600);
        assert_eq!(dagq[1]["version"], "0.4.0-dev+bbb");
        assert_eq!(dagq[2]["version"], Value::Null);
        assert_eq!(versions["claude"][0]["version"], "2.1.0");
        assert_eq!(versions["claude"][0]["runs"], 2);
        assert_eq!(
            versions["rustc"][0]["version"],
            "1.93.0 aarch64-apple-darwin"
        );
        let bands = report["load_bands"].as_array().unwrap();
        let names = bands.iter().map(|b| b["band"].clone()).collect::<Vec<_>>();
        assert_eq!(names, [json!("4-8"), json!("64+"), Value::Null]);
        assert_eq!(bands[0]["runs"], 1);

        assert_eq!(
            report["backend_failures"]["by_load_band"],
            json!([{"band": "4-8", "count": 1}, {"band": "64+", "count": 1}])
        );
        assert_eq!(report["backend_failures"]["count"], 3);
        assert_eq!(
            report["verification_commands"],
            json!([
                {"command": "cargo fmt --all --check", "count": 1, "failed": 0, "total_secs": 2.0, "median_secs": 2.0},
                {"command": "cargo llvm-cov", "count": 3, "failed": 1, "total_secs": 600.0, "median_secs": 200.0},
            ])
        );
    }

    #[test]
    fn runs_goals_and_alerts_come_from_the_event_sequence() {
        let mut events = Events::default();
        // Task 1 (goal 7): integrated after 10 min of work, 2 of validation,
        // 20 waiting to land; startup 3 min; one resume and a pass review.
        events.run(1, "a", "run_claimed", 0);
        events.run(1, "a", "agent_started", 1);
        events.run(1, "a", "first_commit_observed", 4);
        events.run(1, "a", "receipt_observed", 10);
        events.status(1, "a", "validation_finished", 12, "awaiting_integration");
        events.run(1, "a", "resume_started", 13);
        events.push(
            1,
            Some("a"),
            "review_finished",
            14,
            json!({"verdict": "pass"}),
        );
        events.run(1, "a", "integration_started", 30);
        events.run(1, "a", "run_integrated", 32);
        // Task 2 (goal 7): 40 min of work — twice the goal median is 2 × 25 —
        // then three needs_session and the landing.
        events.run(2, "b", "run_claimed", 0);
        events.run(2, "b", "receipt_observed", 40);
        events.status(2, "b", "validation_finished", 41, "awaiting_integration");
        for minute in [42, 43, 44] {
            events.status(2, "b", "integration_deferred", minute, "needs_session");
        }
        // An `integrate` that errors puts `needs_session` back; not a new park.
        events.status(2, "b", "integration_error", 44, "needs_session");
        // A resume that ends with the run still parked is not a new park either.
        events.status(2, "b", "resume_finished", 44, "needs_session");
        events.run(2, "b", "run_integrated", 45);
        // Task 3 (no goal) failed twice in two runs; no receipt the second time.
        events.run(3, "c1", "run_claimed", 0);
        events.run(3, "c1", "receipt_observed", 5);
        events.status(3, "c1", "validation_finished", 6, "failed");
        events.run(3, "c2", "run_claimed", 10);
        events.status(3, "c2", "supervision_finished", 15, "failed");
        // Task 4 (goal 7) still waits to land since minute 50; an ask on it
        // has been open since minute 55, another was answered.
        events.run(4, "d", "run_claimed", 46);
        events.run(4, "d", "receipt_observed", 48);
        events.status(4, "d", "validation_finished", 50, "awaiting_integration");
        // A failed landing attempt does not restart the wait.
        events.run(4, "d", "integration_started", 52);
        events.status(4, "d", "integration_error", 53, "awaiting_integration");
        events.push(4, Some("d"), "ask_opened", 55, json!({"ask_id": 1}));
        events.push(4, None, "ask_opened", 56, json!({"ask_id": 2}));
        events.push(4, None, "ask_answered", 57, json!({"ask_id": 2}));
        let goals = goals([(1, Some(7)), (2, Some(7)), (3, None), (4, Some(7))]);
        let slots = SlotSnapshot {
            free_slots: 2,
            candidates: 0,
            ready: 1,
        };

        let report = value(&stats(
            &events.0,
            &goals,
            at(120),
            slots,
            &StatsQuery::default(),
            &LiveSnapshot::default(),
        ));
        let runs = report["runs"].as_array().unwrap();
        let ids = runs.iter().map(|r| r["run_id"].clone()).collect::<Vec<_>>();
        // Finished runs in the order they finished; `d` is still in flight.
        assert_eq!(ids, [json!("a"), json!("b"), json!("c1"), json!("c2")]);
        assert_eq!(
            runs[0],
            json!({
                "run_id": "a", "task_id": 1, "goal_id": 7, "status": "integrated",
                "finished_event_id": 9, "work": 600, "validate": 120,
                "wait_to_land": 1200, "startup": 180, "resumes": 1,
                "review_verdict": "pass", "needs_session": 0, "failed": 0,
                // Goal 36: the wait to land by phase, from the events.
                "land_phases": {
                    "exit": 1080, "review": 0, "revise": 0, "conflict": 0, "ask": 0,
                    "resume": 0, "landing_queue": 0, "rebase": 120, "verify": 0,
                    "push": null,
                },
                // Task 466: the times, the landings and the resumes. The
                // title comes from the queue, not the events.
                "title": null,
                // Goal 21: the kind comes from the queue too.
                "kind": null,
                "claimed_at": "2026-09-23T12:00:00.000Z",
                "validated_at": "2026-09-23T12:12:00.000Z",
                "landed_at": "2026-09-23T12:32:00.000Z",
                "integrate_attempts": 1, "deferrals": {}, "conflict_files": [],
                "broken_by": [], "broke_runs": 0,
                "resume_attempts": [{
                    "attempt": 1, "reason": "unknown",
                    "started_at": "2026-09-23T12:13:00.000Z",
                    "secs": null, "resolved": null,
                }],
                // Task 575: no prediction; the actual repeats the counts.
                "prediction": null,
                "actual": {
                    "output_tokens": null, "model_secs": null, "resumes": 1,
                    "resume_reasons": {"unknown": 1}, "review_verdict": "pass",
                    "task_rework": false,
                },
                // Task 197: nothing was recorded at the claim nor over
                // the intervals.
                "dagq_version": null, "claude_version": null,
                "rustc_release": null, "rustc_host": null,
                "claim_parallel": null, "claim_slots": null, "claim_load_avg": null,
                "load": {"work": null, "validate": null, "verify": null},
                "load_band": null,
                // ADR-0048: the run's Claude sessions per kind; these
                // events recorded none.
                "sessions": {},
                // Task 514: no session recorded its work breakdown.
                "work_breakdown": null,
                // Task 199: nor its tokens.
                "tokens": null,
            })
        );
        // Parked three times, then the landing: the wait is the resume's.
        assert_eq!(runs[1]["land_phases"]["resume"], 180);
        assert!(runs[2]["land_phases"].is_null());
        assert_eq!(runs[1]["needs_session"], 3);
        assert!(runs[1]["startup"].is_null());
        assert!(runs[1]["review_verdict"].is_null());
        assert_eq!(runs[2]["status"], "failed");
        assert_eq!(runs[3]["work"], Value::Null);
        let mut goal_reports = report["goals"].clone();
        let breakdown = goal_reports[0]["land_phases"].take();
        assert_eq!(breakdown["runs"], 2);
        assert_eq!(breakdown["tail_threshold"], 1200);
        assert_eq!(breakdown["tail_runs"], 1);
        assert_eq!(
            breakdown["exit"],
            json!({"count": 2, "total": 1140, "median": 570, "p90": 1080, "max": 1080, "tail_total": 1080})
        );
        assert_eq!(breakdown["resume"]["tail_total"], 0);
        assert_eq!(breakdown["push"]["count"], 0);
        assert_eq!(goal_reports[1]["land_phases"]["runs"], 0);
        assert_eq!(goal_reports[0]["resume_outcomes"]["attempts"], 1);
        assert_eq!(
            goal_reports[0]["resume_outcomes"]["by_reason"]["unknown"]["resolved_percent"],
            Value::Null
        );
        for goal in goal_reports.as_array_mut().unwrap() {
            goal.as_object_mut().unwrap().remove("land_phases");
            goal.as_object_mut().unwrap().remove("resume_outcomes");
        }
        assert_eq!(report["overall"]["land_phases"]["runs"], 2);
        assert_eq!(
            goal_reports,
            json!([
                {"goal_id": 7, "runs": 2,
                 "work": {"count": 2, "total": 3000, "median": 1500},
                 "validate": {"count": 2, "total": 180, "median": 90},
                 "wait_to_land": {"count": 2, "total": 1440, "median": 720},
                 "startup": {"count": 1, "total": 180, "median": 180},
                 "sessions": {},
                 "work_breakdown": {"runs": 0, "total_secs": 0, "categories": {}, "commands": {},
                                    "verification_repeats": 0, "runs_with_repeats": 0,
                                    "test_with_llvm_cov": 0},
                 "tokens": {"runs": 0, "input": {"count": 0, "total": 0, "median": null}, "output": {"count": 0, "total": 0, "median": null}, "cache_read": {"count": 0, "total": 0, "median": null},
                            "cache_creation": {"count": 0, "total": 0, "median": null}, "total": {"count": 0, "total": 0, "median": null},
                            "cost_usd": {"count": 0, "total": null, "median": null}}},
                {"goal_id": null, "runs": 2,
                 "work": {"count": 1, "total": 300, "median": 300},
                 "validate": {"count": 1, "total": 60, "median": 60},
                 "wait_to_land": {"count": 0, "total": 0, "median": null},
                 "startup": {"count": 0, "total": 0, "median": null},
                 "sessions": {},
                 "work_breakdown": {"runs": 0, "total_secs": 0, "categories": {}, "commands": {},
                                    "verification_repeats": 0, "runs_with_repeats": 0,
                                    "test_with_llvm_cov": 0},
                 "tokens": {"runs": 0, "input": {"count": 0, "total": 0, "median": null}, "output": {"count": 0, "total": 0, "median": null}, "cache_read": {"count": 0, "total": 0, "median": null},
                            "cache_creation": {"count": 0, "total": 0, "median": null}, "total": {"count": 0, "total": 0, "median": null},
                            "cost_usd": {"count": 0, "total": null, "median": null}}},
            ])
        );
        assert_eq!(report["overall"]["runs"], 4);
        assert_eq!(report["overall"]["work"]["median"], 600);
        assert_eq!(report["next_cursor"], events.last_id());
        let alerts = report["alerts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                (
                    a["kind"].as_str().unwrap(),
                    a["run_id"].clone(),
                    a["value"].as_i64().unwrap(),
                    a["threshold"].as_i64().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            alerts,
            [
                ("awaiting_integration", json!("a"), 1200, 900),
                ("needs_session", json!("b"), 3, 3),
                ("awaiting_integration", json!("d"), 4200, 900),
                ("task_failed", json!("c2"), 2, 2),
                ("ask_unanswered", json!("d"), 3900, 3600),
                ("idle_slots", Value::Null, 2, 0),
            ]
        );
        // Task 2's 40 minutes are under twice goal 7's median (2 × 25 minutes).
        assert_eq!(report["alerts"][2]["task_id"], 4);
        // The longest phase of each wait: `a` waited on its session's exit
        // (no `landing_queued` was recorded), `d` on the ask still open.
        assert_eq!(report["alerts"][0]["phase"], "exit");
        assert_eq!(report["alerts"][2]["phase"], "ask");
        assert!(report["alerts"][1].get("phase").is_none());
        assert!(report["alerts"][5]["task_id"].is_null());

        // --goal keeps the runs and alerts of goal 7's tasks only.
        let goal = value(&stats(
            &events.0,
            &goals,
            at(120),
            SlotSnapshot::default(),
            &StatsQuery {
                goal_id: Some(GoalId::new(7)),
                ..Default::default()
            },
            &LiveSnapshot::default(),
        ));
        assert_eq!(goal["runs"].as_array().unwrap().len(), 2);
        assert_eq!(goal["goals"].as_array().unwrap().len(), 1);
        assert!(
            goal["alerts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|a| a["task_id"] != 3 && a["kind"] != "idle_slots")
        );

        // --since: only runs that finished after the cursor.
        let since = value(&stats(
            &events.0,
            &goals,
            at(120),
            SlotSnapshot::default(),
            &StatsQuery {
                since: Some(EventId::new(runs[1]["finished_event_id"].as_i64().unwrap()).into()),
                ..Default::default()
            },
            &LiveSnapshot::default(),
        ));
        let ids = since["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["run_id"].clone())
            .collect::<Vec<_>>();
        assert_eq!(ids, [json!("c1"), json!("c2")]);
        assert_eq!(since["next_cursor"], events.last_id());

        // Task 3's first failure is before the cursor, and still counts.
        let later = value(&stats(
            &events.0,
            &goals,
            at(120),
            SlotSnapshot::default(),
            &StatsQuery {
                since: Some(
                    EventId::new(since["runs"][0]["finished_event_id"].as_i64().unwrap()).into(),
                ),
                ..Default::default()
            },
            &LiveSnapshot::default(),
        ));
        assert_eq!(later["runs"].as_array().unwrap().len(), 1);
        assert!(later["alerts"].as_array().unwrap().contains(&json!({
            "kind": "task_failed", "task_id": 3, "run_id": "c2", "value": 2, "threshold": 2
        })));
    }

    #[test]
    fn backend_failures_are_counted_per_window_with_the_highest_load() {
        let mut events = Events::default();
        let failure = |op: &str, load: Value, slots: i64| {
            json!({"op": op, "workspace_id": "w", "timeout_secs": 30, "error": "timed out",
                   "load_avg": load, "slots": slots, "parallel": 4})
        };
        events.run(1, "a", "run_claimed", 0);
        events.push(
            1,
            Some("a"),
            "backend_call_failed",
            1,
            failure("close", json!(23.5), 3),
        );
        events.status(1, "a", "supervision_finished", 2, "failed");
        let cursor = events.last_id();
        events.run(2, "b", "run_claimed", 3);
        events.push(
            2,
            Some("b"),
            "backend_call_failed",
            4,
            failure("send_exit", json!(34.25), 4),
        );
        events.push(
            2,
            Some("b"),
            "backend_call_failed",
            5,
            failure("send_exit", Value::Null, 2),
        );
        // A call for no run (up's group): no task, no run.
        events.push(
            1,
            None,
            "backend_call_failed",
            6,
            failure("ensure_group", json!(1.0), 0),
        );
        events.0.last_mut().unwrap().task_id = None;
        events.status(2, "b", "supervision_finished", 7, "failed");
        let goals = goals([(1, None), (2, Some(5))]);
        let run = |query: StatsQuery| {
            value(&stats(
                &events.0,
                &goals,
                at(8),
                SlotSnapshot::default(),
                &query,
                &LiveSnapshot::default(),
            ))
        };
        let alert = json!({"kind": "backend_failures", "task_id": null, "run_id": null,
                           "value": 4, "threshold": 2});

        let all = run(StatsQuery::default());
        assert_eq!(
            all["backend_failures"],
            json!({"count": 4, "by_op": {"close": 1, "ensure_group": 1, "send_exit": 2},
                   "max_load_avg": 34.25, "max_slots": 4,
                   "by_load_band": [{"band": "0-4", "count": 1}, {"band": "16-32", "count": 1},
                                    {"band": "32-64", "count": 1}]})
        );
        assert!(all["alerts"].as_array().unwrap().contains(&alert));

        // Past the cursor: three failures, the close is before it.
        let since = run(StatsQuery {
            since: Some(EventId::new(cursor).into()),
            ..Default::default()
        });
        assert_eq!(since["backend_failures"]["count"], 3);
        assert_eq!(since["backend_failures"]["by_op"]["close"], Value::Null);
        assert!(
            since["alerts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["kind"] == "backend_failures" && a["value"] == 3)
        );

        // --goal keeps only the failures of its runs; one is no alert.
        let goal = run(StatsQuery {
            goal_id: Some(GoalId::new(5)),
            since: Some(EventId::new(cursor).into()),
            ..Default::default()
        });
        assert_eq!(goal["backend_failures"]["count"], 2);
        let only_close = run(StatsQuery {
            goal_id: Some(GoalId::new(9)),
            full: true,
            ..Default::default()
        });
        assert_eq!(
            only_close["backend_failures"],
            json!({"count": 0, "by_op": {}, "max_load_avg": null, "max_slots": null, "by_load_band": []})
        );

        // Nothing past the last event: an empty window.
        let empty = run(StatsQuery {
            since: Some(EventId::new(events.last_id()).into()),
            ..Default::default()
        });
        assert_eq!(empty["backend_failures"]["count"], 0);
        assert!(
            !empty["alerts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["kind"] == "backend_failures")
        );
    }

    #[test]
    fn work_over_the_goal_median_is_an_alert() {
        let mut events = Events::default();
        for (task, work) in [(1, 10), (2, 10), (3, 30)] {
            let run = format!("r{task}");
            events.run(task, &run, "run_claimed", 0);
            events.run(task, &run, "receipt_observed", work);
            events.status(
                task,
                &run,
                "validation_finished",
                work,
                "awaiting_integration",
            );
            events.run(task, &run, "run_integrated", work + 1);
        }
        let goals = goals([(1, Some(1)), (2, Some(1)), (3, Some(1))]);
        let report = value(&stats(
            &events.0,
            &goals,
            at(60),
            SlotSnapshot::default(),
            &StatsQuery::default(),
            &LiveSnapshot::default(),
        ));
        assert_eq!(
            report["alerts"],
            json!([{"kind": "work_over_median", "task_id": 3, "run_id": "r3",
                    "value": 1800, "threshold": 1200}])
        );
    }

    #[test]
    fn at_most_fifty_runs_unless_full_and_since_pages_forward() {
        let mut events = Events::default();
        for task in 1..=60 {
            let run = format!("r{task}");
            events.run(task, &run, "run_claimed", 0);
            events.status(task, &run, "supervision_finished", 1, "failed");
        }
        let goals = HashMap::new();
        let run = |query: StatsQuery| {
            value(&stats(
                &events.0,
                &goals,
                at(2),
                SlotSnapshot::default(),
                &query,
                &LiveSnapshot::default(),
            ))
        };
        let latest = run(StatsQuery::default());
        assert_eq!(latest["runs"].as_array().unwrap().len(), 50);
        assert_eq!(latest["runs"][0]["run_id"], "r11");
        assert_eq!(latest["next_cursor"], events.last_id());
        assert_eq!(
            run(StatsQuery {
                full: true,
                ..Default::default()
            })["runs"]
                .as_array()
                .unwrap()
                .len(),
            60
        );
        let first = run(StatsQuery {
            since: Some(EventId::new(0).into()),
            ..Default::default()
        });
        assert_eq!(first["runs"].as_array().unwrap().len(), 50);
        assert_eq!(first["runs"][49]["run_id"], "r50");
        assert_eq!(first["next_cursor"], 100);
        let rest = run(StatsQuery {
            since: Some(EventId::new(100).into()),
            ..Default::default()
        });
        assert_eq!(rest["runs"].as_array().unwrap().len(), 10);
        assert_eq!(rest["runs"][0]["run_id"], "r51");
        assert_eq!(rest["next_cursor"], events.last_id());
        assert_eq!(
            run(StatsQuery {
                since: Some(EventId::new(events.last_id()).into()),
                ..Default::default()
            })["runs"],
            json!([])
        );
    }

    #[test]
    fn cli_stats_reads_the_queue_and_since_returns_only_new_runs() {
        use dagq::{
            domain::{ClaimOutcome, CommitSha},
            infrastructure::sqlite::SqliteQueue,
        };
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("queue.db");
        ok(&db, &["init"]);
        let empty = ok(&db, &["stats"]);
        assert_eq!(empty["runs"], json!([]));
        assert_eq!(empty["alerts"], json!([]));
        assert_eq!(empty["overall"]["work"]["median"], Value::Null);
        ok(&db, &["goal", "add", "measured"]);
        ok(&db, &["add", "first", "--goal", "1", "--kind", "runtime"]);
        ok(&db, &["add", "second"]);
        ok(&db, &["ready", "1", "--bypass-review"]);
        ok(&db, &["ready", "2", "--bypass-review"]);
        let base = "0123456789abcdef0123456789abcdef01234567";
        let finish = |queue: &mut SqliteQueue| {
            let ClaimOutcome::Claimed { run } = queue
                .claim_for_supervisor(&CommitSha::try_from(base).unwrap(), "t")
                .unwrap()
            else {
                panic!("nothing to claim");
            };
            queue
                .record_runtime_event(run.id(), "receipt_observed", json!({}))
                .unwrap();
            queue
                .record_runtime_event(run.id(), "validation_finished", json!({"status": "failed"}))
                .unwrap();
            run.id().clone()
        };
        let mut queue = SqliteQueue::open(&db).unwrap();
        let first = finish(&mut queue);
        let report = ok(&db, &["stats"]);
        assert_eq!(report["runs"][0]["run_id"], first.as_str());
        assert_eq!(report["runs"][0]["goal_id"], 1);
        assert_eq!(report["runs"][0]["failed"], 1);
        assert!(report["runs"][0]["work"].is_i64());
        assert_eq!(report["runs"][0]["kind"], "runtime");
        assert_eq!(report["kinds"][0]["kind"], "runtime");
        assert_eq!(report["kinds"][0]["runs"], 1);
        let cursor = report["next_cursor"].as_i64().unwrap();
        assert_eq!(cursor, ok(&db, &["status"])["cursor"].as_i64().unwrap());

        let second = finish(&mut queue);
        let since = ok(&db, &["stats", "--since", &cursor.to_string()]);
        assert_eq!(since["runs"].as_array().unwrap().len(), 1);
        assert_eq!(since["runs"][0]["run_id"], second.as_str());
        assert!(since["next_cursor"].as_i64().unwrap() > cursor);
        assert_eq!(
            ok(&db, &["stats", "--full"])["runs"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        // Per kind, by name, then the runs of tasks without a kind.
        let kinds = ok(&db, &["stats", "--full"])["kinds"].clone();
        assert_eq!(kinds.as_array().unwrap().len(), 2);
        assert_eq!(kinds[0]["kind"], "runtime");
        assert_eq!(kinds[1]["kind"], Value::Null);
        assert_eq!(kinds[1]["runs"], 1);
        assert!(kinds[1]["work"]["median"].is_i64());
        let goal = ok(&db, &["stats", "--goal", "1"]);
        assert_eq!(goal["runs"].as_array().unwrap().len(), 1);
        assert_eq!(goal["goals"][0]["goal_id"], 1);
        assert!(!invoke(&db, &["stats", "--since", "x"]).status.success());
    }
}

/// `dagq mark` records a person's or a planner's mark, `--retract` a
/// retraction, and `dagq marks` lists them with the marks derived from the
/// claims: a change of the host's toolchain between two claims is a
/// `derived:toolchain` mark and writes no event (ADR-0051 decisions 10, 12).
#[test]
fn marks_are_recorded_retracted_and_derived_from_the_claims() {
    use dagq::domain::{ClaimOutcome, CommitSha};
    use dagq::infrastructure::sqlite::SqliteQueue;
    use serde_json::json;
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    ok(&db, &["add", "second"]);
    ok(&db, &["ready", "1", "--bypass-review"]);
    ok(&db, &["ready", "2", "--bypass-review"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    for (host, at) in [
        ("x86_64-apple-darwin", "01"),
        ("aarch64-apple-darwin", "02"),
    ] {
        let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&base, "t").unwrap() else {
            panic!("nothing to claim");
        };
        // The attributes a supervisor's claim records (task 197).
        conn.execute(
            "UPDATE run_events SET created_at=?3, payload=json_set(payload,
               '$.dagq_version','v1','$.parallel',3,'$.rustc_release','1.90.0','$.rustc_host',?2)
             WHERE run_id=?1 AND kind='run_claimed'",
            [
                run.id().as_str(),
                host,
                &format!("2026-09-26T{at}:00:00.000Z"),
            ],
        )
        .unwrap();
    }
    let events_before = ok(&db, &["events", "--after", "0", "--all"])["cursor"].clone();

    let marked = ok_as(
        "planner",
        &db,
        &[
            "mark",
            "parallel 4→3",
            "--note",
            "load",
            "--at",
            "2026-09-26T09:30:00+09:00",
        ],
    );
    assert_eq!(marked["kind"], "mark_recorded");
    assert_eq!(marked["label"], "parallel 4→3");
    assert_eq!(marked["at"], "2026-09-26T00:30:00.000Z");
    assert_eq!(marked["detail"]["by"], "planner");
    assert_eq!(marked["detail"]["note"], "load");
    let id = marked["id"].as_i64().unwrap();
    assert_eq!(id, events_before.as_i64().unwrap() + 1);

    let listed = ok(&db, &["marks"])["marks"].clone();
    let kinds: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|mark| mark["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["mark_recorded", "derived:toolchain"]);
    assert_eq!(listed[1]["id"], serde_json::Value::Null);
    assert_eq!(listed[1]["at"], "2026-09-26T02:00:00.000Z");
    assert_eq!(listed[1]["detail"]["from"], "1.90.0 x86_64-apple-darwin");
    assert_eq!(listed[1]["detail"]["to"], "1.90.0 aarch64-apple-darwin");
    assert_eq!(
        ok(&db, &["marks", "--since", "2026-09-26T01:00:00Z"])["marks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let retracted = ok(&db, &["mark", "--retract", &id.to_string()]);
    assert_eq!(retracted["kind"], "mark_retracted");
    assert_eq!(retracted["detail"]["mark"], json!(id));
    assert_eq!(retracted["detail"]["by"], "human");
    let listed = ok(&db, &["marks"])["marks"].clone();
    assert_eq!(listed[0]["retracted_by"], retracted["id"]);
    assert!(refused(&db, &["mark", "--retract", &id.to_string()]).contains("already retracted"));
    assert!(refused(&db, &["mark", "--retract", "1"]).contains("not a mark"));
    assert!(refused(&db, &["mark", " "]).contains("needs a label"));
    assert!(
        refused(&db, &["mark", "later", "--at", "2999-01-01T00:00:00Z"]).contains("in the future")
    );

    // The observer and the jobs read marks but may not record one.
    for role in ["observer", "reviewer"] {
        assert!(!invoke_as(Some(role), &db, &["mark", "x"]).status.success());
        assert_eq!(ok_as(role, &db, &["marks"])["marks"], listed);
    }
}
