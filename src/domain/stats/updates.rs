//! The automatic update of the fixed binary (ADR-0073 decision 17, task
//! 496): its `update_*` queue events of the window by kind, the failures by
//! the stage they failed at, and the builds installed, so how often the
//! binary was replaced and how often it failed can be read next to the
//! runs. Derived from `run_events` like the rest of `stats`.
use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::{
    super::{UPDATE_EVENT_KINDS, UPDATE_FAILED, UPDATE_INSTALLED},
    EventId, RunEvent, TaskId,
    asks::UNKNOWN,
};

/// The steps of the automatic update in a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct UpdateStats {
    /// Every `update_*` event.
    pub count: i64,
    /// Those by kind (`update_started`, `update_installed`, ...).
    pub by_kind: BTreeMap<String, i64>,
    /// The `update_failed` events by `stage` (`build`, `check`, `install`,
    /// `watch`, `interrupted`).
    pub failed_by_stage: BTreeMap<String, i64>,
    /// The build identifiers `update_installed` put in place, oldest first.
    pub installed: Vec<String>,
}

/// Count the `update_*` events with `after < id <= upto`. They belong to no
/// task, so they count only when `counts` accepts no task (no `--goal`).
pub fn updates(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> UpdateStats {
    let mut stats = UpdateStats::default();
    for event in events.iter().filter(|event| {
        event.id > after
            && event.id <= upto
            && UPDATE_EVENT_KINDS.contains(&event.kind.as_str())
            && counts(event.task_id)
    }) {
        stats.count += 1;
        *stats.by_kind.entry(event.kind.clone()).or_default() += 1;
        let text = |key: &str| event.payload.get(key).and_then(Value::as_str);
        match event.kind.as_str() {
            UPDATE_FAILED => {
                *stats
                    .failed_by_stage
                    .entry(text("stage").unwrap_or(UNKNOWN).to_owned())
                    .or_default() += 1;
            }
            UPDATE_INSTALLED => stats
                .installed
                .push(text("version").unwrap_or(UNKNOWN).to_owned()),
            _ => {}
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: "2026-09-26T01:00:00.000Z".to_owned(),
        }
    }

    /// The steps count by kind, the failures by stage and the installs by
    /// version; other kinds and events outside the window are left out, and
    /// none counts for a goal.
    #[test]
    fn counts_the_steps_by_kind_the_failures_by_stage_and_the_installs() {
        let events = [
            event(1, "update_started", json!({"commit": "a"})),
            event(2, "update_built", json!({"commit": "a"})),
            event(3, "update_installed", json!({"version": "0.4.0-dev+a"})),
            event(4, "update_started", json!({"commit": "b"})),
            event(5, "update_failed", json!({"stage": "build"})),
            event(6, "update_retry", json!({"answer": "retry"})),
            event(7, "update_failed", json!({})),
            event(8, "run_claimed", json!({})),
            event(9, "update_installed", json!({"version": "0.4.0-dev+c"})),
        ];
        let all = updates(&events, EventId::new(0), EventId::new(8), |_| true);
        assert_eq!(all.count, 7);
        assert_eq!(all.by_kind["update_started"], 2);
        assert_eq!(all.by_kind["update_failed"], 2);
        assert!(!all.by_kind.contains_key("run_claimed"));
        assert_eq!(all.failed_by_stage["build"], 1);
        assert_eq!(all.failed_by_stage[UNKNOWN], 1);
        assert_eq!(all.installed, vec!["0.4.0-dev+a".to_owned()]);

        let later = updates(&events, EventId::new(5), EventId::new(9), |_| true);
        assert_eq!(later.count, 3);
        assert_eq!(later.installed, vec!["0.4.0-dev+c".to_owned()]);

        let goal = updates(&events, EventId::new(0), EventId::new(9), |task| {
            task.is_some()
        });
        assert_eq!(goal, UpdateStats::default());
    }
}
