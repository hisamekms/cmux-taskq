//! The quality of the plans (ADR-0079 decision 7, task 579): what a
//! proposal was like when plan review took it (its features, recorded on
//! `plan_review_started`), the model and effort of the plan review session
//! that judged it (its `session_closed`), and what came of it: the revises
//! of plan review, the tasks canceled as duplicates after they were ready,
//! the follow-ups adopted into it and canceled later, and the task-caused
//! rework of its tasks' runs. Pure: derived from `run_events` only, split
//! into strata by the model, the effort and each feature, for `kpi`.
use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

use super::{
    EventId, RunEvent, TaskId,
    sessions::{PLAN_REVIEW, SESSION_CLOSED, SESSION_OPENED},
};

/// Where a proposal came from: a follow-up draft, a goal's gap, an
/// observer's finding, a planner of the runtime's (a plan review sent it
/// back while its planner was closed, or a draft), or a person's planner.
pub const FOLLOW_UP: &str = "follow_up";
pub const GOAL_GAP: &str = "goal_gap";
pub const OBSERVER: &str = "observer";

/// Below it a proposal's closest existing task is `low`; see
/// [`related_tier`].
pub const RELATED_MID: f64 = 3.0;
/// From it on the closest existing task is `high`.
pub const RELATED_HIGH: f64 = 6.0;

/// The stratum of every proposal and plan review.
const ALL: &str = "all";
const UNKNOWN: &str = "unknown";

/// What a proposal was like when plan review took it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProposalFeatures {
    /// [`FOLLOW_UP`], [`GOAL_GAP`], [`OBSERVER`], or the owner's origin
    /// (`runtime` / `person`).
    pub origin: String,
    /// 0, 1 for a follow-up of a task, 2 for a follow-up of a follow-up...:
    /// the deepest of its tasks.
    pub follow_up_depth: i64,
    /// The highest `dagq related` score between one of its tasks and a task
    /// outside it; `None` when none scores.
    pub related_score: Option<f64>,
    /// How many times plan review had sent it back.
    pub revise_count: i64,
}

impl ProposalFeatures {
    /// The `features` of a `plan_review_started`.
    pub fn payload(&self) -> Value {
        json!({
            "origin": self.origin,
            "follow_up_depth": self.follow_up_depth,
            "related_score": self.related_score,
            "related": related_tier(self.related_score),
            "revise_count": self.revise_count,
        })
    }
}

/// The origin of a proposal from what its tasks and goals came from, the
/// most specific first.
pub fn origin(
    follow_up: bool,
    goal_gap: bool,
    finding: bool,
    owner: super::PlannerOrigin,
) -> String {
    if follow_up {
        FOLLOW_UP
    } else if goal_gap {
        GOAL_GAP
    } else if finding {
        OBSERVER
    } else {
        owner.as_str()
    }
    .to_owned()
}

/// `low` (below [`RELATED_MID`]), `mid` or `high` (from [`RELATED_HIGH`]);
/// `low` too when nothing scores.
pub fn related_tier(score: Option<f64>) -> &'static str {
    match score {
        Some(score) if score >= RELATED_HIGH => "high",
        Some(score) if score >= RELATED_MID => "mid",
        _ => "low",
    }
}

/// The measures of one stratum of a window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Quality {
    /// Plan reviews that finished with a verdict in the window.
    pub reviews: usize,
    /// Of them, those that sent the proposal back.
    pub revises: usize,
    /// Proposals whose tasks first became `ready` in the window.
    pub proposals: usize,
    /// Their tasks canceled as duplicates after being `ready` (so far).
    pub duplicate_cancels_after_ready: usize,
    /// Their follow-up tasks (adopted by being submitted with them).
    pub follow_ups: usize,
    /// Of them, those canceled since.
    pub follow_ups_canceled_after_adoption: usize,
    /// Their tasks that have a run.
    pub tasks_run: usize,
    /// Of them, those with task-caused rework (ADR-0079 decision 1).
    pub tasks_reworked: usize,
}

/// One plan review, as its events recorded it.
#[derive(Default)]
struct Review {
    proposal: Option<i64>,
    /// Its `plan_review_started`.
    started: Option<EventId>,
    features: Value,
    session: Option<EventId>,
    model: Option<String>,
    effort: Option<String>,
    /// Its `plan_review_finished`: the event and its decision.
    finished: Option<(EventId, String)>,
    anchor: Option<TaskId>,
}

impl Review {
    /// Its strata: `all`, the model and effort of its session, and each of
    /// the proposal's features.
    fn strata(&self) -> Vec<String> {
        let text = |value: Option<&str>| value.unwrap_or(UNKNOWN).to_owned();
        let feature = |key: &str| match &self.features[key] {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            _ => UNKNOWN.to_owned(),
        };
        vec![
            ALL.to_owned(),
            format!("model={}", text(self.model.as_deref())),
            format!("effort={}", text(self.effort.as_deref())),
            format!("origin={}", feature("origin")),
            format!("follow_up_depth={}", feature("follow_up_depth")),
            format!("related={}", feature("related")),
            format!("revise_count={}", feature("revise_count")),
        ]
    }
}

/// What became of a task, over every event.
#[derive(Default)]
struct TaskFate {
    proposal: Option<i64>,
    ready: bool,
    /// Left `draft` without being canceled (task 470's adoption).
    adopted: bool,
    follow_up: bool,
    duplicate_after_ready: bool,
    canceled_after_adoption: bool,
    run: bool,
    reworked: bool,
}

/// Task-caused rework (ADR-0079 decision 1): `integrate`'s verification
/// failed, review raised a concern, or the run was sent back to revise.
/// Conflicts and kills are not the task's.
pub fn rework(event: &RunEvent) -> bool {
    match event.kind.as_str() {
        "integration_deferred" => event.payload["code"] == "verification_failed",
        "review_finished" => event.payload["verdict"] == "concern",
        "revise_requested" => true,
        _ => false,
    }
}

/// The plan quality of the window `after < id <= upto`, per stratum, of
/// the events whose task `counts` accepts. Plan reviews count in the window
/// they finished in; a proposal counts in the window its first task became
/// `ready` in, with the strata of the last plan review that started before
/// that (a proposal readied without one is `unknown` but in `all`), and its
/// tasks' fates so far.
pub fn plan_quality(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> BTreeMap<String, Quality> {
    let mut reviews: BTreeMap<i64, Review> = BTreeMap::new();
    let mut sessions: HashMap<EventId, i64> = HashMap::new();
    let mut tasks: HashMap<TaskId, TaskFate> = HashMap::new();
    let follow_ups: HashSet<i64> = events
        .iter()
        .filter(|e| e.kind == "follow_up_registered")
        .filter_map(|e| e.payload.get("task_id").and_then(Value::as_i64))
        .collect();
    // Proposal → the first time one of its tasks became ready.
    let mut accepted: BTreeMap<i64, EventId> = BTreeMap::new();
    for event in events {
        let id = |key: &str| event.payload.get(key).and_then(Value::as_i64);
        match event.kind.as_str() {
            "plan_review_started" => {
                if let Some(review_id) = id("plan_review_id") {
                    let review = reviews.entry(review_id).or_default();
                    review.proposal = id("proposal_id");
                    review.started = Some(event.id);
                    review.features = event.payload["features"].clone();
                    review.anchor = event.task_id;
                }
            }
            "plan_review_finished" => {
                if let Some(review_id) = id("plan_review_id") {
                    let decision = event.payload["decision"]
                        .as_str()
                        .unwrap_or(UNKNOWN)
                        .to_owned();
                    let review = reviews.entry(review_id).or_default();
                    review.finished = Some((event.id, decision));
                    review.proposal = review.proposal.or(id("proposal_id"));
                    review.anchor = review.anchor.or(event.task_id);
                }
            }
            SESSION_OPENED if event.payload["kind"] == PLAN_REVIEW => {
                if let Some(review_id) = id("plan_review_id") {
                    sessions.insert(event.id, review_id);
                    reviews.entry(review_id).or_default().session = Some(event.id);
                }
            }
            SESSION_CLOSED if event.payload["kind"] == PLAN_REVIEW => {
                let review = id("opened_event_id")
                    .and_then(|opened| sessions.get(&EventId::new(opened)))
                    .and_then(|review_id| reviews.get_mut(review_id));
                if let Some(review) = review {
                    let text = |key: &str| event.payload[key].as_str().map(str::to_owned);
                    review.model = text("model");
                    review.effort = text("effort");
                }
            }
            _ => {}
        }
        let Some(task_id) = event.task_id else {
            continue;
        };
        let fate = tasks.entry(task_id).or_default();
        match event.kind.as_str() {
            "task_submitted" => fate.proposal = id("proposal_id"),
            "task_created" => fate.follow_up = follow_ups.contains(&task_id.as_i64()),
            "task_status_changed" => {
                let from = event.payload["from"].as_str();
                let to = event.payload["to"].as_str();
                if from == Some("draft") && to != Some("canceled") {
                    fate.adopted = true;
                }
                if to == Some("canceled") {
                    if fate.ready && event.payload.get("duplicate_of").is_some_and(Value::is_i64) {
                        fate.duplicate_after_ready = true;
                    }
                    if fate.adopted {
                        fate.canceled_after_adoption = true;
                    }
                }
                if to == Some("ready") && !fate.ready {
                    fate.ready = true;
                    if let Some(proposal) = fate.proposal {
                        accepted.entry(proposal).or_insert(event.id);
                    }
                }
            }
            "task_canceled_as_duplicate" if fate.ready => fate.duplicate_after_ready = true,
            "run_claimed" => fate.run = true,
            _ if rework(event) => fate.reworked = true,
            _ => {}
        }
    }
    let mut quality: BTreeMap<String, Quality> = BTreeMap::new();
    quality.insert(ALL.to_owned(), Quality::default());
    let window = |id: EventId| id > after && id <= upto;
    for review in reviews.values() {
        let Some((finished, decision)) = &review.finished else {
            continue;
        };
        if !window(*finished) || !counts(review.anchor) {
            continue;
        }
        for stratum in review.strata() {
            let entry = quality.entry(stratum).or_default();
            entry.reviews += 1;
            entry.revises += usize::from(decision == "revise");
        }
    }
    let unjudged = Review::default();
    for (&proposal, &at) in accepted.iter().filter(|(_, at)| window(**at)) {
        let members: Vec<(&TaskId, &TaskFate)> = tasks
            .iter()
            .filter(|(task, fate)| fate.proposal == Some(proposal) && counts(Some(**task)))
            .collect();
        if members.is_empty() {
            continue;
        }
        // A pass readies the tasks before it records its finish, in one
        // transaction: the judge is the last review started before.
        let judge = reviews
            .values()
            .filter(|review| review.proposal == Some(proposal))
            .filter(|review| review.finished.is_some())
            .filter(|review| review.started.is_some_and(|id| id < at))
            .max_by_key(|review| review.started)
            .unwrap_or(&unjudged);
        let strata = if std::ptr::eq(judge, &unjudged) {
            vec![ALL.to_owned()]
        } else {
            judge.strata()
        };
        let count = |test: fn(&TaskFate) -> bool| members.iter().filter(|(_, f)| test(f)).count();
        for stratum in strata {
            let entry = quality.entry(stratum).or_default();
            entry.proposals += 1;
            entry.duplicate_cancels_after_ready += count(|f| f.duplicate_after_ready);
            entry.follow_ups += count(|f| f.follow_up && f.adopted);
            entry.follow_ups_canceled_after_adoption +=
                count(|f| f.follow_up && f.canceled_after_adoption);
            entry.tasks_run += count(|f| f.run);
            entry.tasks_reworked += count(|f| f.run && f.reworked);
        }
    }
    quality
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{PlannerOrigin, RunId};

    fn event(id: i64, task: Option<i64>, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: task.map(TaskId::new),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: format!("1970-01-01T00:00:{:02}.000Z", id % 60),
        }
    }

    #[test]
    fn features_name_the_origin_and_the_tier_of_the_closest_task() {
        assert_eq!(origin(true, true, true, PlannerOrigin::Person), FOLLOW_UP);
        assert_eq!(origin(false, true, true, PlannerOrigin::Person), GOAL_GAP);
        assert_eq!(origin(false, false, true, PlannerOrigin::Person), OBSERVER);
        assert_eq!(
            origin(false, false, false, PlannerOrigin::Runtime),
            "runtime"
        );
        assert_eq!(related_tier(None), "low");
        assert_eq!(related_tier(Some(2.9)), "low");
        assert_eq!(related_tier(Some(3.0)), "mid");
        assert_eq!(related_tier(Some(6.0)), "high");
        let features = ProposalFeatures {
            origin: "person".into(),
            follow_up_depth: 0,
            related_score: Some(4.5),
            revise_count: 1,
        };
        assert_eq!(
            features.payload(),
            json!({"origin": "person", "follow_up_depth": 0, "related_score": 4.5,
                   "related": "mid", "revise_count": 1})
        );
    }

    /// Two proposals: 1 was sent back once by a medium review and then
    /// passed by a high one; its tasks 10 (a follow-up canceled later as a
    /// duplicate) and 11 (reworked) are judged by the high review. 2 was
    /// readied without a review: only `all`.
    #[test]
    fn the_quality_of_a_window_is_split_by_the_judging_session_and_the_features() {
        let features = |revise_count: i64| {
            json!({"origin": "follow_up", "follow_up_depth": 1, "related_score": 7.0,
                   "related": "high", "revise_count": revise_count})
        };
        let review = |id: i64, review: i64, f: Value| {
            event(
                id,
                Some(10),
                "plan_review_started",
                json!({"proposal_id": 1, "plan_review_id": review, "features": f}),
            )
        };
        let opened = |id: i64, review: i64| {
            event(
                id,
                Some(10),
                SESSION_OPENED,
                json!({"kind": PLAN_REVIEW, "plan_review_id": review}),
            )
        };
        let closed = |id: i64, opened: i64, effort: &str| {
            event(
                id,
                Some(10),
                SESSION_CLOSED,
                json!({"kind": PLAN_REVIEW, "opened_event_id": opened,
                       "model": "claude-opus-5-5", "effort": effort}),
            )
        };
        let finished = |id: i64, review: i64, decision: &str| {
            event(
                id,
                Some(10),
                "plan_review_finished",
                json!({"proposal_id": 1, "plan_review_id": review, "decision": decision}),
            )
        };
        let changed = |id: i64, task: i64, from: &str, to: &str| {
            event(
                id,
                Some(task),
                "task_status_changed",
                json!({"from": from, "to": to}),
            )
        };
        let submitted = |id: i64, task: i64, proposal: i64| {
            event(
                id,
                Some(task),
                "task_submitted",
                json!({"proposal_id": proposal}),
            )
        };
        let run_event = |id: i64, task: i64, kind: &str, payload: Value| {
            let mut e = event(id, Some(task), kind, payload);
            e.run_id = Some(RunId::new(format!("run-{task}")).unwrap());
            e
        };
        let events = vec![
            event(1, Some(9), "follow_up_registered", json!({"task_id": 10})),
            event(2, Some(10), "task_created", json!({})),
            event(3, Some(11), "task_created", json!({})),
            changed(4, 10, "draft", "submitted"),
            submitted(5, 10, 1),
            changed(6, 11, "draft", "submitted"),
            submitted(7, 11, 1),
            review(8, 1, features(0)),
            opened(9, 1),
            finished(10, 1, "revise"),
            closed(11, 9, "medium"),
            // The window starts here.
            review(20, 2, features(1)),
            opened(21, 2),
            // A pass readies the tasks before it records its finish.
            changed(22, 10, "submitted", "ready"),
            changed(23, 11, "submitted", "ready"),
            finished(24, 2, "pass"),
            closed(25, 21, "high"),
            run_event(26, 11, "run_claimed", json!({})),
            run_event(
                27,
                11,
                "integration_deferred",
                json!({"code": "rebase_conflict"}),
            ),
            run_event(
                28,
                11,
                "integration_deferred",
                json!({"code": "verification_failed"}),
            ),
            run_event(29, 10, "run_claimed", json!({})),
            run_event(30, 10, "review_finished", json!({"verdict": "pass"})),
            event(
                31,
                Some(10),
                "task_status_changed",
                json!({"from": "ready", "to": "canceled", "duplicate_of": 3}),
            ),
            // Proposal 2, readied past no review.
            submitted(32, 12, 2),
            changed(33, 12, "submitted", "ready"),
            run_event(34, 12, "run_claimed", json!({})),
            run_event(35, 12, "revise_requested", json!({})),
        ];
        let quality = plan_quality(&events, EventId::new(19), EventId::new(40), |_| true);
        let expected_high = Quality {
            reviews: 1,
            revises: 0,
            proposals: 1,
            duplicate_cancels_after_ready: 1,
            follow_ups: 1,
            follow_ups_canceled_after_adoption: 1,
            tasks_run: 2,
            tasks_reworked: 1,
        };
        assert_eq!(quality["effort=high"], expected_high);
        assert_eq!(quality["model=claude-opus-5-5"], expected_high);
        assert_eq!(quality["origin=follow_up"], expected_high);
        assert_eq!(quality["related=high"], expected_high);
        assert_eq!(quality["revise_count=1"], expected_high);
        assert_eq!(quality["follow_up_depth=1"], expected_high);
        assert_eq!(
            quality["all"],
            Quality {
                proposals: 2,
                tasks_run: 3,
                tasks_reworked: 2,
                ..expected_high
            }
        );
        assert!(!quality.contains_key("effort=medium"));
        // The earlier window has the revise of the medium review only.
        let earlier = plan_quality(&events, EventId::new(0), EventId::new(19), |_| true);
        assert_eq!(
            earlier["effort=medium"],
            Quality {
                reviews: 1,
                revises: 1,
                ..Quality::default()
            }
        );
        assert_eq!(earlier["revise_count=0"].revises, 1);
        // A goal's tasks only.
        let none = plan_quality(&events, EventId::new(0), EventId::new(40), |_| false);
        assert_eq!(none.len(), 1);
        assert_eq!(none["all"], Quality::default());
    }
}
