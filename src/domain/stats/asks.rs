//! The asks that reached a person (task 325): how many were opened, of
//! which kind and by whom, and how many were answered, by whom and with
//! which option. Derived from `ask_opened` / `ask_answered` like the rest of
//! `stats`; an `ask_answered` recorded before its answerer was kept counts
//! as [`UNKNOWN`].
use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::{EventId, RunEvent, TaskId};

/// The answerer (or asker) of an event that does not name one.
pub const UNKNOWN: &str = "unknown";

/// The asks opened and answered in a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AskStats {
    pub opened: OpenedAsks,
    pub answered: AnsweredAsks,
}

/// The `ask_opened` events of the window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OpenedAsks {
    pub count: i64,
    pub by_kind: BTreeMap<String, i64>,
    /// By the role that registered the ask (`asked_by`).
    pub by_asked_by: BTreeMap<String, i64>,
}

/// The `ask_answered` events of the window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AnsweredAsks {
    pub count: i64,
    pub by_kind: BTreeMap<String, i64>,
    /// By who answered: `person`, a session role (`inbox`, `planner`),
    /// `runtime`, or `unknown` for an answer recorded before it was kept.
    pub by_answered_by: BTreeMap<String, i64>,
    /// Per ask kind, what the answers chose.
    pub choices: BTreeMap<String, Choices>,
}

/// What the answers to one kind of ask chose.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Choices {
    /// Per option text, the answers that chose it.
    pub by_option: BTreeMap<String, i64>,
    /// Answers that chose no option.
    pub free: i64,
    /// Answers recorded before the choice was kept.
    pub unknown: i64,
}

/// Count the `ask_opened` and `ask_answered` events with `after < id <=
/// upto` whose task `counts` accepts.
pub fn asks(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> AskStats {
    let mut stats = AskStats::default();
    let text = |payload: &Value, key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or(UNKNOWN)
            .to_owned()
    };
    for event in events
        .iter()
        .filter(|event| event.id > after && event.id <= upto && counts(event.task_id))
    {
        let payload = &event.payload;
        match event.kind.as_str() {
            "ask_opened" => {
                let opened = &mut stats.opened;
                opened.count += 1;
                *opened.by_kind.entry(text(payload, "kind")).or_default() += 1;
                *opened
                    .by_asked_by
                    .entry(text(payload, "asked_by"))
                    .or_default() += 1;
            }
            "ask_answered" => {
                let answered = &mut stats.answered;
                let kind = text(payload, "kind");
                answered.count += 1;
                *answered.by_kind.entry(kind.clone()).or_default() += 1;
                *answered
                    .by_answered_by
                    .entry(text(payload, "answered_by"))
                    .or_default() += 1;
                let choices = answered.choices.entry(kind).or_default();
                match payload.get("option").and_then(Value::as_str) {
                    Some(option) => *choices.by_option.entry(option.to_owned()).or_default() += 1,
                    None if payload.get("answered_by").is_some() => choices.free += 1,
                    None => choices.unknown += 1,
                }
            }
            _ => {}
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: i64, task_id: Option<i64>, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: task_id.map(TaskId::new),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
        }
    }

    /// Opened asks count by kind and asker, answered ones by answerer and
    /// choice; an answer recorded before the answerer was kept is unknown,
    /// and events outside the window or of a task not counted are left out.
    #[test]
    fn counts_the_asks_opened_and_answered_in_the_window() {
        let events = [
            event(
                1,
                Some(1),
                "ask_opened",
                json!({"ask_id": 1, "kind": "decide", "asked_by": "triage"}),
            ),
            event(
                2,
                Some(1),
                "ask_opened",
                json!({"ask_id": 2, "kind": "approve_landing", "asked_by": "supervisor"}),
            ),
            event(
                3,
                None,
                "ask_opened",
                json!({"ask_id": 3, "kind": "blocked"}),
            ),
            event(
                4,
                Some(1),
                "ask_answered",
                json!({"ask_id": 2, "kind": "approve_landing", "answered_by": "inbox", "option_index": 0, "option": "land"}),
            ),
            event(
                5,
                Some(1),
                "ask_answered",
                json!({"ask_id": 1, "kind": "decide", "answered_by": "person", "option_index": null}),
            ),
            event(
                6,
                Some(2),
                "ask_answered",
                json!({"ask_id": 4, "kind": "stuck_exit", "runtime_closed": true, "answered_by": "runtime", "option_index": null}),
            ),
            event(
                7,
                Some(1),
                "ask_answered",
                json!({"ask_id": 5, "kind": "approve_landing"}),
            ),
            event(8, Some(1), "run_claimed", json!({})),
            event(
                9,
                Some(1),
                "ask_opened",
                json!({"ask_id": 6, "kind": "decide", "asked_by": "triage"}),
            ),
        ];
        let all = asks(&events, EventId::new(0), EventId::new(8), |_| true);
        assert_eq!(all.opened.count, 3);
        assert_eq!(all.opened.by_kind["decide"], 1);
        assert_eq!(all.opened.by_asked_by[UNKNOWN], 1);
        assert_eq!(all.opened.by_asked_by["supervisor"], 1);
        assert_eq!(all.answered.count, 4);
        assert_eq!(all.answered.by_kind["approve_landing"], 2);
        assert_eq!(
            all.answered.by_answered_by,
            BTreeMap::from([
                ("inbox".to_owned(), 1),
                ("person".to_owned(), 1),
                ("runtime".to_owned(), 1),
                (UNKNOWN.to_owned(), 1),
            ])
        );
        assert_eq!(
            all.answered.choices["approve_landing"],
            Choices {
                by_option: BTreeMap::from([("land".to_owned(), 1)]),
                free: 0,
                unknown: 1,
            }
        );
        assert_eq!(all.answered.choices["decide"].free, 1);

        let task_one = asks(&events, EventId::new(1), EventId::new(9), |task| {
            task == Some(TaskId::new(1))
        });
        assert_eq!(task_one.opened.count, 2);
        assert_eq!(task_one.opened.by_kind["decide"], 1);
        assert_eq!(task_one.answered.count, 3);
        assert!(!task_one.answered.by_kind.contains_key("stuck_exit"));
    }
}
