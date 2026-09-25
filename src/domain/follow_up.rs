//! Drafts the runtime opens a planner for (ADR-0041 decision 16, which
//! replaces the follow-up triage job of ADR-0037): where such a draft came
//! from, how many planners one draft gets, and the runtime's rule on which
//! drafts a planner of the runtime's may not submit without a person.
//!
//! `integrate` registers a draft from each landed receipt's `follow_ups`
//! (origin `follow_up`); a job that judges a goal may register one for a gap
//! it found (origin `goal_gap`). A draft a person registers with `add` has
//! no origin, and no planner is opened for it.

use serde::{Deserialize, Serialize};

use super::DomainError;

// Where a draft the runtime or a job registered came from.
string_enum!(DraftOrigin {
    FollowUp => "follow_up",
    GoalGap => "goal_gap",
});

/// The options of the `planner_question` ask a planner of the runtime's
/// opens about its draft when it cannot decide (ADR-0041 decision 16). The
/// answer is typed into that planner's workspace and the planner applies
/// it; `keep_draft` leaves the draft for a person's planner, and no planner
/// of the runtime's is opened for it again.
pub const PLANNER_QUESTION_OPTIONS: &[&str] = &["adopt", "cancel", "keep_draft"];

/// Planners the runtime opens for one draft that none of them decided
/// (its session ended with the draft still waiting); the next time the
/// draft is recorded as exhausted instead (`draft_planner_exhausted`, the
/// inbox's attention).
pub const MAX_DRAFT_PLANNERS: usize = 3;

/// A follow-up this many steps from a person's judgement is not submitted
/// by a planner of the runtime's without one (ADR-0037 decision 6, kept by
/// ADR-0041 decision 16).
pub const FOLLOW_UP_ASK_DEPTH: i64 = 2;

/// Where a follow_up draft stands when a planner of the runtime's submits
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FollowUpFacts {
    /// The draft belongs to a goal that is not closed.
    pub goal_open: bool,
    /// The draft's `follow_up_depth`.
    pub depth: i64,
}

/// Why a planner of the runtime's may not submit a follow_up draft without
/// a person's `adopt` answer (ADR-0041 decision 16), or `None`: the draft's
/// goal is closed or missing, or the draft is [`FOLLOW_UP_ASK_DEPTH`] or
/// more follow-ups from a person.
pub fn adopt_needs_person(facts: FollowUpFacts) -> Option<String> {
    if !facts.goal_open {
        Some("its goal is closed (or it has none), so a person decides whether it belongs to a new goal".to_owned())
    } else if facts.depth >= FOLLOW_UP_ASK_DEPTH {
        Some(format!(
            "it is a follow-up {} steps from a person's judgement (at most {} is submitted without one)",
            facts.depth,
            FOLLOW_UP_ASK_DEPTH - 1
        ))
    } else {
        None
    }
}

/// A draft the runtime opens a planner for: the task, where it came from
/// and the material its origin recorded, and how many planners the runtime
/// opened for it already.
#[derive(Debug, Clone, Serialize)]
pub struct DraftTarget {
    pub task: super::Task,
    pub origin: DraftOrigin,
    pub material: serde_json::Value,
    pub planners: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_follow_up_needs_a_person_for_a_closed_goal_or_depth_two() {
        let open = FollowUpFacts {
            goal_open: true,
            depth: 1,
        };
        assert_eq!(adopt_needs_person(open), None);
        assert!(
            adopt_needs_person(FollowUpFacts {
                goal_open: false,
                ..open
            })
            .unwrap()
            .contains("closed")
        );
        assert!(
            adopt_needs_person(FollowUpFacts { depth: 2, ..open })
                .unwrap()
                .contains("2 steps")
        );
    }

    #[test]
    fn origins_parse() {
        assert_eq!(
            "goal_gap".parse::<DraftOrigin>().unwrap(),
            DraftOrigin::GoalGap
        );
        assert_eq!(DraftOrigin::FollowUp.as_str(), "follow_up");
        assert!("person".parse::<DraftOrigin>().is_err());
    }
}
