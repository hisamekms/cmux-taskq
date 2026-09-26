//! The predictions of `stats` (ADR-0079 decision 2): next to each run, the
//! last weight plan review predicted for its task before the run started
//! (`task_weight_predicted`), where its `expected_output_tokens` fell among
//! the latest predictions then, and what the run turned out to be. The
//! accuracy (a rank correlation, the lower third's hits) is computed from
//! these rows, not here.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::{EventId, RunEvent, RunId, RunStats, TaskId};
use crate::domain::{
    plan_quality,
    prediction::{PREDICTION_WINDOW, percentile, window},
    sessions::RUN_SESSION,
    worktime::MODEL,
};

/// The prediction a run is read against.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunPrediction {
    /// The prediction as recorded: `size`, `nature`, `uncertainty`,
    /// `expected_output_tokens`, `rework_probability`, `reason`.
    #[serde(flatten)]
    pub prediction: serde_json::Map<String, Value>,
    pub proposal_id: Option<i64>,
    pub plan_review_id: Option<i64>,
    /// The model and effort of the plan review that predicted it.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Where `expected_output_tokens` fell (0 to 100) among the last
    /// prediction of each other task, at most [`PREDICTION_WINDOW`] of the
    /// latest, recorded before the run started; null with none.
    pub percentile: Option<f64>,
    /// How many predictions `percentile` compared it with.
    pub percentile_of: usize,
}

/// What a run turned out to be, next to its prediction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RunActual {
    /// The output tokens of its own sessions (worker, resume, revise); null
    /// when none recorded them.
    pub output_tokens: Option<i64>,
    /// The model's seconds in its own sessions' work breakdown; null when
    /// none recorded one.
    pub model_secs: Option<i64>,
    pub resumes: i64,
    /// Its resumes per reason.
    pub resume_reasons: BTreeMap<String, i64>,
    pub review_verdict: Option<String>,
    /// Task-caused rework (ADR-0079 decision 1): `integrate`'s verification
    /// failed, review raised a concern, or it was sent back to revise.
    pub task_rework: bool,
}

/// One recorded prediction.
struct Recorded<'a> {
    id: EventId,
    task_id: TaskId,
    tokens: u64,
    event: &'a RunEvent,
}

/// Give each of `runs` its prediction and actual. `first_event` is where
/// each run starts; a prediction counts when it was recorded before that.
pub fn attach(
    events: &[RunEvent],
    first_event: &std::collections::HashMap<&str, EventId>,
    runs: &mut [&mut RunStats],
) {
    let recorded: Vec<Recorded<'_>> = events
        .iter()
        .filter(|event| event.kind == "task_weight_predicted")
        .filter_map(|event| {
            Some(Recorded {
                id: event.id,
                task_id: event.task_id?,
                tokens: event.payload["prediction"]["expected_output_tokens"].as_u64()?,
                event,
            })
        })
        .collect();
    let history: Vec<(TaskId, u64)> = recorded.iter().map(|r| (r.task_id, r.tokens)).collect();
    let mut reworked: std::collections::HashSet<&RunId> = std::collections::HashSet::new();
    for event in events {
        if let Some(run_id) = &event.run_id
            && plan_quality::rework(event)
        {
            reworked.insert(run_id);
        }
    }
    for run in runs.iter_mut() {
        run.actual = actual(run, reworked.contains(&run.run_id));
        let Some(&start) = first_event.get(run.run_id.as_str()) else {
            continue;
        };
        let before = &recorded[..recorded.partition_point(|r| r.id < start)];
        let Some(last) = before.iter().rev().find(|r| r.task_id == run.task_id) else {
            continue;
        };
        let others = window(&history[..before.len()], run.task_id, PREDICTION_WINDOW);
        let payload = &last.event.payload;
        let text = |key: &str| payload[key].as_str().map(str::to_owned);
        run.prediction = Some(RunPrediction {
            prediction: payload["prediction"]
                .as_object()
                .cloned()
                .unwrap_or_default(),
            proposal_id: payload["proposal_id"].as_i64(),
            plan_review_id: payload["plan_review_id"].as_i64(),
            model: text("model"),
            effort: text("effort"),
            percentile: percentile(last.tokens, &others),
            percentile_of: others.len(),
        });
    }
}

fn actual(run: &RunStats, task_rework: bool) -> RunActual {
    let output_tokens = run.tokens.as_ref().and_then(|tokens| {
        let own: Vec<i64> = tokens
            .by_kind
            .iter()
            .filter(|(kind, _)| RUN_SESSION.contains(&kind.as_str()))
            .map(|(_, totals)| totals.output)
            .collect();
        (!own.is_empty()).then(|| own.iter().sum())
    });
    let mut resume_reasons = BTreeMap::new();
    for attempt in &run.retries.resume_attempts {
        *resume_reasons.entry(attempt.reason.clone()).or_default() += 1;
    }
    RunActual {
        output_tokens,
        model_secs: run
            .work_breakdown
            .as_ref()
            .map(|work| work.secs.get(MODEL).copied().unwrap_or(0)),
        resumes: run.resumes,
        resume_reasons,
        review_verdict: run.review_verdict.clone(),
        task_rework,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;
    use crate::domain::stats::{LiveSnapshot, SlotSnapshot, StatsQuery, stats};

    const R1: &str = "11111111-1111-4111-8111-111111111111";
    const R2: &str = "22222222-2222-4222-8222-222222222222";
    const R3: &str = "33333333-3333-4333-8333-333333333333";

    fn event(id: i64, task: i64, run: Option<&str>, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(task)),
            goal_id: None,
            run_id: run.map(|run| RunId::new(run).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: "2027-01-15T08:00:00.000Z".to_owned(),
        }
    }

    fn predicted(id: i64, task: i64, tokens: u64) -> RunEvent {
        event(
            id,
            task,
            None,
            "task_weight_predicted",
            json!({"proposal_id": 7, "plan_review_id": id, "attempt": 1,
                   "prediction": {"size": "S", "nature": "mechanical", "uncertainty": 0.2,
                                  "expected_output_tokens": tokens, "rework_probability": 0.1,
                                  "reason": "small"},
                   "model": "claude-opus-5-5", "effort": "medium"}),
        )
    }

    /// A run is read against its task's last prediction before it started,
    /// ranked among the other tasks' latest; a later prediction is the next
    /// run's. Its actual sums its own sessions' output and flags the
    /// task-caused rework, not a conflict's resume.
    #[test]
    fn runs_are_read_next_to_the_prediction_they_started_with() {
        // Only a run's own sessions record a work breakdown.
        let closed = |id: i64, task: i64, run: &str, kind: &str, output: i64| {
            event(
                id,
                task,
                Some(run),
                "session_closed",
                json!({"kind": kind, "reason": "exited", "opened_event_id": id - 1,
                       "tokens": {"input": 1, "output": output},
                       "work": {"total_secs": 100, "secs": {"model": 40, "tool": 60}}}),
            )
        };
        let opened = |id: i64, task: i64, run: &str, kind: &str| {
            event(id, task, Some(run), "session_opened", json!({"kind": kind}))
        };
        let events = vec![
            predicted(1, 2, 10_000),
            predicted(2, 3, 30_000),
            predicted(3, 4, 50_000),
            // Task 4 predicted again: its last value counts.
            predicted(4, 4, 20_000),
            predicted(5, 5, 15_000),
            event(10, 5, Some(R1), "run_claimed", json!({})),
            opened(11, 5, R1, "worker"),
            closed(12, 5, R1, "worker", 800),
            opened(13, 5, R1, "review"),
            event(
                14,
                5,
                Some(R1),
                "session_closed",
                json!({"kind": "review", "reason": "exited", "opened_event_id": 13,
                       "tokens": {"input": 1, "output": 50}}),
            ),
            event(
                15,
                5,
                Some(R1),
                "review_finished",
                json!({"verdict": "revise"}),
            ),
            event(16, 5, Some(R1), "revise_requested", json!({})),
            event(17, 5, Some(R1), "run_integrated", json!({})),
            event(20, 3, Some(R2), "run_claimed", json!({})),
            event(
                21,
                3,
                Some(R2),
                "integration_deferred",
                json!({"code": "rebase_conflict", "status": "needs_session"}),
            ),
            event(22, 3, Some(R2), "resume_started", json!({"attempt": 1})),
            event(23, 3, Some(R2), "run_integrated", json!({})),
            // Recorded after R2 started: R3 of task 6 has none before it.
            predicted(24, 6, 1_000),
            event(25, 7, Some(R3), "run_claimed", json!({})),
            event(26, 7, Some(R3), "run_integrated", json!({})),
        ];
        let all = stats(
            &events,
            &HashMap::new(),
            1_800_000_000,
            SlotSnapshot::default(),
            &StatsQuery {
                full: true,
                ..StatsQuery::default()
            },
            &LiveSnapshot::default(),
        );
        let json = serde_json::to_value(&all).unwrap();
        let r1 = &json["runs"][0];
        assert_eq!(r1["run_id"], R1);
        let prediction = &r1["prediction"];
        assert_eq!(prediction["size"], "S");
        assert_eq!(prediction["nature"], "mechanical");
        assert_eq!(prediction["expected_output_tokens"], 15_000);
        assert_eq!(prediction["rework_probability"], 0.1);
        assert_eq!(prediction["plan_review_id"], 5);
        assert_eq!(prediction["proposal_id"], 7);
        assert_eq!(prediction["model"], "claude-opus-5-5");
        assert_eq!(prediction["effort"], "medium");
        // Among 20000 (task 4's last), 30000 and 10000: one below.
        assert_eq!(prediction["percentile_of"], 3);
        assert_eq!(prediction["percentile"], 33.3);
        assert_eq!(
            r1["actual"],
            json!({"output_tokens": 800, "model_secs": 40, "resumes": 0,
                   "resume_reasons": {}, "review_verdict": "revise", "task_rework": true})
        );
        let r2 = &json["runs"][1];
        assert_eq!(r2["prediction"]["expected_output_tokens"], 30_000);
        assert_eq!(r2["prediction"]["percentile_of"], 3);
        assert_eq!(r2["prediction"]["percentile"], 100.0);
        assert_eq!(r2["actual"]["resumes"], 1);
        assert_eq!(
            r2["actual"]["resume_reasons"],
            json!({"rebase_conflict": 1})
        );
        assert_eq!(r2["actual"]["task_rework"], false);
        assert_eq!(r2["actual"]["output_tokens"], Value::Null);
        assert_eq!(r2["actual"]["model_secs"], Value::Null);
        let r3 = &json["runs"][2];
        assert_eq!(r3["prediction"], Value::Null);
        assert_eq!(r3["actual"]["task_rework"], false);
    }
}
