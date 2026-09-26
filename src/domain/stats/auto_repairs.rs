//! The irregularities the runtime and the recovery job repaired without a
//! person (ADR-0047 decisions 38 and 45, task 362): the `auto_repaired`
//! events of the window by `layer` and `repair`, and per day next to the
//! asks opened that day, so that whether fewer irregularities reach the
//! inbox can be read from one place. Derived from `auto_repaired` /
//! `ask_opened` like the rest of `stats`.
use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::{EventId, RunEvent, TaskId, asks::UNKNOWN};

/// The repairs of a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AutoRepairStats {
    pub count: i64,
    /// Per `layer` (`runtime` / `recovery`), the repairs by `repair`.
    pub by_layer: BTreeMap<String, LayerRepairs>,
    /// Per UTC day (`YYYY-MM-DD`) of the window, the repairs and the asks
    /// opened, side by side.
    pub by_day: BTreeMap<String, DayCounts>,
}

/// The repairs of one layer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LayerRepairs {
    pub count: i64,
    pub by_repair: BTreeMap<String, i64>,
}

/// One day of the window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DayCounts {
    /// The `auto_repaired` events of the day.
    pub auto_repaired: i64,
    /// Those by `repair`.
    pub by_repair: BTreeMap<String, i64>,
    /// The `ask_opened` events of the day: what reached a person.
    pub asks_opened: i64,
    /// Those by why a person was needed (`reason_category`), `unknown` for
    /// an ask opened before the reason was kept.
    pub asks_by_reason: BTreeMap<String, i64>,
}

/// Count the `auto_repaired` and `ask_opened` events with `after < id <=
/// upto` whose task `counts` accepts.
pub fn auto_repairs(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> AutoRepairStats {
    let mut stats = AutoRepairStats::default();
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
        let day = || event.created_at.get(..10).unwrap_or(UNKNOWN).to_owned();
        let payload = &event.payload;
        match event.kind.as_str() {
            "auto_repaired" => {
                let repair = text(payload, "repair");
                stats.count += 1;
                let layer = stats.by_layer.entry(text(payload, "layer")).or_default();
                layer.count += 1;
                *layer.by_repair.entry(repair.clone()).or_default() += 1;
                let day = stats.by_day.entry(day()).or_default();
                day.auto_repaired += 1;
                *day.by_repair.entry(repair).or_default() += 1;
            }
            "ask_opened" => {
                let day = stats.by_day.entry(day()).or_default();
                day.asks_opened += 1;
                *day.asks_by_reason
                    .entry(text(payload, "reason_category"))
                    .or_default() += 1;
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

    fn event(id: i64, task_id: Option<i64>, kind: &str, payload: Value, at: &str) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: task_id.map(TaskId::new),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: at.to_owned(),
        }
    }

    /// Repairs count by layer and repair, and per day next to the asks
    /// opened that day; events outside the window or of a task not
    /// counted are left out.
    #[test]
    fn counts_the_repairs_by_layer_repair_and_day_next_to_the_asks() {
        let day1 = "2026-09-25T10:00:00.000Z";
        let day2 = "2026-09-26T01:00:00.000Z";
        let repaired = |layer: &str, repair: &str| json!({"layer": layer, "repair": repair});
        let events = [
            event(
                1,
                Some(1),
                "auto_repaired",
                repaired("runtime", "dialog_answered"),
                day1,
            ),
            event(
                2,
                Some(1),
                "auto_repaired",
                repaired("runtime", "submit_enter_retry"),
                day1,
            ),
            event(
                3,
                Some(2),
                "auto_repaired",
                repaired("recovery", "stop_processes"),
                day2,
            ),
            event(
                4,
                Some(1),
                "ask_opened",
                json!({"kind": "decide", "reason_category": "recovery_failed"}),
                day1,
            ),
            event(5, None, "ask_opened", json!({"kind": "blocked"}), day2),
            event(6, Some(1), "run_claimed", json!({}), day2),
            event(7, Some(1), "auto_repaired", json!({}), day2),
            event(
                8,
                Some(1),
                "auto_repaired",
                repaired("runtime", "inherit_retry"),
                day2,
            ),
        ];
        let all = auto_repairs(&events, EventId::new(0), EventId::new(7), |_| true);
        assert_eq!(all.count, 4);
        assert_eq!(all.by_layer["runtime"].count, 2);
        assert_eq!(all.by_layer["runtime"].by_repair["submit_enter_retry"], 1);
        assert_eq!(all.by_layer["recovery"].by_repair["stop_processes"], 1);
        assert_eq!(all.by_layer[UNKNOWN].by_repair[UNKNOWN], 1);
        let first = &all.by_day["2026-09-25"];
        assert_eq!((first.auto_repaired, first.asks_opened), (2, 1));
        assert_eq!(first.asks_by_reason["recovery_failed"], 1);
        let second = &all.by_day["2026-09-26"];
        assert_eq!((second.auto_repaired, second.asks_opened), (2, 1));
        assert_eq!(second.asks_by_reason[UNKNOWN], 1);
        assert_eq!(second.by_repair["stop_processes"], 1);

        let task_one = auto_repairs(&events, EventId::new(1), EventId::new(8), |task| {
            task == Some(TaskId::new(1))
        });
        assert_eq!(task_one.count, 3);
        assert!(!task_one.by_layer.contains_key("recovery"));
        assert_eq!(task_one.by_day["2026-09-26"].asks_opened, 0);
        assert_eq!(task_one.by_day["2026-09-26"].by_repair["inherit_retry"], 1);
    }
}
