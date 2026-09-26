//! The drafts the runtime and its jobs register next to the landings of
//! the same window (task 470): the landings (`run_integrated`), the drafts
//! registered by origin (`follow_up` / `goal_gap`), how they were settled
//! (adopted, canceled, kept as a draft), the drafts still waiting at the
//! window's end with the oldest one's age, and the drafts registered per
//! landing, so that whether the drafts pile up faster than they are
//! settled can be read from one place. Derived from `run_events` and the
//! origin each such draft has (`draft_origins`); no table of its own.
use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde_json::Value;

use super::{EventId, RunEvent, TaskId, timestamp_millis};
use crate::domain::DraftOrigin;

/// The drafts of a window.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DraftFlow {
    /// The landings of the window (`run_integrated`), as the `landings`
    /// KPI counts them (ADR-0051 decision 1).
    pub landings: i64,
    /// Over every origin.
    #[serde(flatten)]
    pub all: OriginFlow,
    /// `registered` ÷ `landings`, to two decimals; null without a landing.
    pub drafts_per_landing: Option<f64>,
    /// `registered` ÷ (`adopted` + `canceled`): above 1 the drafts come in
    /// faster than they leave. Null when none left.
    pub inflow_per_outflow: Option<f64>,
    /// Per origin (`follow_up`, `goal_gap`), the same counts.
    pub by_origin: BTreeMap<&'static str, OriginFlow>,
}

/// The drafts of one origin (or of all) in a window.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct OriginFlow {
    /// Registered in the window (their `task_created`).
    pub registered: i64,
    /// Left `draft` for the first time in the window to anything but
    /// `canceled` (submitted, or readied past plan review).
    pub adopted: i64,
    /// Left `draft` for the first time in the window by being canceled.
    pub canceled: i64,
    /// `keep_draft` answers in the window to an ask about the draft.
    pub kept_draft: i64,
    /// Still `draft` at the window's end, wherever they were registered.
    pub backlog: i64,
    /// How long the oldest of `backlog` had waited at the window's end,
    /// in seconds.
    pub oldest_backlog_secs: Option<i64>,
    pub oldest_backlog_task_id: Option<TaskId>,
}

impl OriginFlow {
    fn waiting(&mut self, task_id: TaskId, age: i64) {
        self.backlog += 1;
        if self.oldest_backlog_secs.is_none_or(|oldest| age > oldest) {
            self.oldest_backlog_secs = Some(age);
            self.oldest_backlog_task_id = Some(task_id);
        }
    }
}

/// `numerator` ÷ `denominator` to two decimals; `None` for a zero
/// denominator.
fn ratio(numerator: i64, denominator: i64) -> Option<f64> {
    (denominator > 0).then(|| (numerator as f64 * 100.0 / denominator as f64).round() / 100.0)
}

/// A draft the runtime or a job registered, as `run_events` shows it.
#[derive(Default)]
struct Draft {
    origin: Option<DraftOrigin>,
    created: Option<(EventId, i64)>,
    /// Its first `task_status_changed` away from `draft`: the event and
    /// whether it canceled the draft.
    settled: Option<(EventId, bool)>,
    /// Its status as of the window's end: whether it is `draft`.
    draft_at_end: bool,
}

/// Count the drafts with `after < id <= upto` whose task `counts` accepts:
/// a task is such a draft when `origins` gives it an origin or a
/// `follow_up_registered` names it. The backlog is the drafts still
/// `draft` at `upto`, aged to `end` (unix milliseconds).
pub fn draft_flow(
    events: &[RunEvent],
    origins: &HashMap<TaskId, DraftOrigin>,
    after: EventId,
    upto: EventId,
    end: i64,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> DraftFlow {
    let mut drafts: HashMap<TaskId, Draft> = origins
        .iter()
        .map(|(&task_id, &origin)| {
            (
                task_id,
                Draft {
                    origin: Some(origin),
                    ..Draft::default()
                },
            )
        })
        .collect();
    for event in events.iter().filter(|e| e.kind == "follow_up_registered") {
        if let Some(task_id) = event.payload.get("task_id").and_then(Value::as_i64) {
            drafts
                .entry(TaskId::new(task_id))
                .or_default()
                .origin
                .get_or_insert(DraftOrigin::FollowUp);
        }
    }
    let mut flow = DraftFlow::default();
    let mut kept: Vec<TaskId> = Vec::new();
    for event in events {
        let in_window = event.id > after && event.id <= upto && counts(event.task_id);
        if event.kind == "run_integrated" && in_window {
            flow.landings += 1;
        }
        let Some(draft) = event.task_id.and_then(|task_id| drafts.get_mut(&task_id)) else {
            continue;
        };
        match event.kind.as_str() {
            "task_created" => {
                if let Some(at) = timestamp_millis(&event.created_at) {
                    draft.created = Some((event.id, at));
                }
                draft.draft_at_end = event.id <= upto;
            }
            "task_status_changed" => {
                let to = event.payload["to"].as_str();
                if event.payload["from"] == "draft" && draft.settled.is_none() {
                    draft.settled = Some((event.id, to == Some("canceled")));
                }
                if event.id <= upto {
                    draft.draft_at_end = to == Some("draft");
                }
            }
            "ask_answered" if in_window && event.payload["option"] == "keep_draft" => {
                kept.extend(event.task_id);
            }
            _ => {}
        }
    }
    let mut add = |task_id: TaskId, update: &dyn Fn(&mut OriginFlow)| {
        let origin = drafts[&task_id].origin.unwrap_or(DraftOrigin::FollowUp);
        update(&mut flow.all);
        update(flow.by_origin.entry(origin.as_str()).or_default());
    };
    for task_id in kept {
        add(task_id, &|f| f.kept_draft += 1);
    }
    let window = |id: EventId| id > after && id <= upto;
    let mut ids: Vec<TaskId> = drafts.keys().copied().collect();
    ids.sort();
    for task_id in ids.into_iter().filter(|&id| counts(Some(id))) {
        let draft = &drafts[&task_id];
        let Some((created, at)) = draft.created else {
            continue;
        };
        if window(created) {
            add(task_id, &|f| f.registered += 1);
        }
        match draft.settled {
            Some((id, true)) if window(id) => add(task_id, &|f| f.canceled += 1),
            Some((id, false)) if window(id) => add(task_id, &|f| f.adopted += 1),
            _ => {}
        }
        if draft.draft_at_end {
            let age = (end - at).max(0) / 1000;
            add(task_id, &|f| f.waiting(task_id, age));
        }
    }
    flow.drafts_per_landing = ratio(flow.all.registered, flow.landings);
    flow.inflow_per_outflow = ratio(flow.all.registered, flow.all.adopted + flow.all.canceled);
    flow
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: i64, task_id: i64, kind: &str, payload: Value, secs: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(task_id)),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: format!("1970-01-01T00:{:02}:{:02}.000Z", secs / 60, secs % 60),
        }
    }

    fn changed(id: i64, task_id: i64, from: &str, to: &str, secs: i64) -> RunEvent {
        event(
            id,
            task_id,
            "task_status_changed",
            json!({"from": from, "to": to}),
            secs,
        )
    }

    /// Two landings brought three follow_up drafts and a job one goal_gap
    /// draft: one adopted, one canceled, one kept, two still waiting at the
    /// window's end; what happened after it, and a person's task, are left
    /// out.
    #[test]
    fn counts_the_drafts_against_the_landings_of_the_window() {
        let registered = |id, task| {
            event(
                id,
                1,
                "follow_up_registered",
                json!({"task_id": task}),
                id * 10,
            )
        };
        let events = [
            // Before the window: task 9 registered and adopted.
            event(1, 9, "task_created", json!({}), 10),
            registered(2, 9),
            changed(3, 9, "draft", "submitted", 30),
            // The window: 4..=20.
            event(4, 1, "run_integrated", json!({}), 40),
            event(5, 10, "task_created", json!({}), 50),
            registered(6, 10),
            event(7, 11, "task_created", json!({}), 70),
            registered(8, 11),
            event(9, 2, "run_integrated", json!({}), 90),
            event(10, 12, "task_created", json!({}), 100),
            registered(11, 12),
            event(12, 20, "task_created", json!({}), 120),
            event(13, 30, "task_created", json!({}), 130),
            changed(14, 10, "draft", "submitted", 140),
            changed(15, 11, "draft", "canceled", 150),
            event(
                16,
                12,
                "ask_answered",
                json!({"kind": "planner_question", "option": "keep_draft"}),
                160,
            ),
            // Adopted, then sent back to draft by a revise: settled once.
            changed(17, 10, "submitted", "draft", 170),
            changed(18, 10, "draft", "submitted", 180),
            changed(19, 30, "draft", "submitted", 190),
            event(20, 1, "run_claimed", json!({}), 200),
            // After the window.
            changed(21, 12, "draft", "canceled", 210),
            event(22, 3, "run_integrated", json!({}), 220),
        ];
        let origins = HashMap::from([(TaskId::new(20), DraftOrigin::GoalGap)]);
        let flow = draft_flow(
            &events,
            &origins,
            EventId::new(3),
            EventId::new(20),
            200_000,
            |_| true,
        );
        assert_eq!(flow.landings, 2);
        assert_eq!(
            flow.all,
            OriginFlow {
                registered: 4,
                adopted: 1,
                canceled: 1,
                kept_draft: 1,
                backlog: 2,
                oldest_backlog_secs: Some(100),
                oldest_backlog_task_id: Some(TaskId::new(12)),
            }
        );
        assert_eq!(flow.drafts_per_landing, Some(2.0));
        assert_eq!(flow.inflow_per_outflow, Some(2.0));
        let follow_up = &flow.by_origin["follow_up"];
        assert_eq!(
            (
                follow_up.registered,
                follow_up.backlog,
                follow_up.kept_draft
            ),
            (3, 1, 1)
        );
        let gap = &flow.by_origin["goal_gap"];
        assert_eq!((gap.registered, gap.backlog), (1, 1));
        assert_eq!(gap.oldest_backlog_secs, Some(80));
        let json = serde_json::to_value(&flow).unwrap();
        assert_eq!(json["registered"], 4);
        assert_eq!(json["by_origin"]["goal_gap"]["oldest_backlog_task_id"], 20);

        // Later, task 12 is canceled and nothing waits; a third landing.
        let later = draft_flow(
            &events,
            &origins,
            EventId::new(20),
            EventId::new(22),
            230_000,
            |_| true,
        );
        assert_eq!(later.landings, 1);
        assert_eq!((later.all.registered, later.all.canceled), (0, 1));
        assert_eq!(later.all.backlog, 1);
        assert_eq!(later.drafts_per_landing, Some(0.0));
        assert_eq!(later.inflow_per_outflow, Some(0.0));

        // Only the tasks `counts` accepts.
        let one = draft_flow(
            &events,
            &origins,
            EventId::new(3),
            EventId::new(20),
            200_000,
            |task| task == Some(TaskId::new(20)),
        );
        assert_eq!((one.landings, one.all.registered), (0, 1));
        assert_eq!(one.drafts_per_landing, None);
        assert_eq!(one.inflow_per_outflow, None);
        assert!(!one.by_origin.contains_key("follow_up"));
    }
}
