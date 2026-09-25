//! Follow-up triage (ADR-0037): the verdict a headless job prints about a
//! follow_up draft task, and the runtime's own rules on it. The job only
//! reads the queue; the runtime applies the verdict (and a person's answer
//! to the `follow_up` ask it may open) in one transaction.

use serde::{Deserialize, Serialize};

use super::{DomainError, EvidenceCheck, NewTask, TaskId, scope::validate_path_globs};

// The verdict of the follow-up triage: `adopt` replaces the draft with the
// complete task it proposes, `ready` in the same goal; `drop` cancels the
// draft; `ask` waits for a person in a `follow_up` ask.
string_enum!(FollowUpDecision {
    Adopt => "adopt",
    Drop => "drop",
    Ask => "ask",
});

/// The complete task a follow-up triage proposes in place of the draft
/// (ADR-0037 decision 4). Only `title` is required by the schema; the rest
/// is checked by [`FollowUpProposal::check`] and the override rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowUpProposal {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub acceptance: String,
    #[serde(default)]
    pub verification_commands: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    /// Receipt checks the task requires: `tests`, `e2e` or
    /// `subagent_review`. Kept as text so an unknown name makes the
    /// proposal invalid rather than the whole verdict unreadable.
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<i64>,
    #[serde(default)]
    pub context: String,
}

impl FollowUpProposal {
    /// Whether the proposal can be registered as it stands, apart from its
    /// dependencies, which only the store can check: a non-blank title and
    /// verification commands, valid path globs, known evidence names and
    /// positive task IDs. The error says what is wrong.
    pub fn check(&self) -> Result<(), String> {
        if self.title.trim().is_empty() {
            return Err("the proposed task has a blank title".to_owned());
        }
        if self
            .verification_commands
            .iter()
            .any(|c| c.trim().is_empty())
        {
            return Err("the proposed task has a blank verification command".to_owned());
        }
        validate_path_globs(&self.paths)
            .map_err(|error| format!("the proposed task's paths are invalid: {error}"))?;
        for name in &self.evidence {
            name.parse::<EvidenceCheck>().map_err(|_| {
                format!("the proposed task requires unknown evidence {name:?} (tests, e2e or subagent_review)")
            })?;
        }
        if let Some(id) = self.depends_on.iter().find(|id| **id <= 0) {
            return Err(format!("the proposed task depends on task {id}"));
        }
        Ok(())
    }

    /// The task to register for the proposal, in `goal_id`, its context
    /// behind `origin`. Call [`FollowUpProposal::check`] first.
    pub fn new_task(&self, goal_id: Option<super::GoalId>, origin: &str) -> NewTask {
        let context = if self.context.trim().is_empty() {
            origin.to_owned()
        } else {
            format!("{origin}\n\n{}", self.context)
        };
        NewTask {
            title: self.title.trim().to_owned(),
            description: self.description.clone(),
            acceptance: self.acceptance.clone(),
            verification_commands: self.verification_commands.clone(),
            dependencies: self.depends_on.iter().map(|id| TaskId::new(*id)).collect(),
            goal_dependencies: Vec::new(),
            priority: Default::default(),
            goal_id,
            context,
            required_evidence: self
                .evidence
                .iter()
                .filter_map(|name| name.parse().ok())
                .collect(),
            paths: self.paths.clone(),
        }
    }
}

/// What the follow-up triage prints on stdout: one JSON object. `task` is
/// required for `adopt`, may be offered with `ask` (so a person can choose
/// `adopt`) and is ignored for `drop`; `question` is what `ask` puts to a
/// person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowUpVerdict {
    pub verdict: FollowUpDecision,
    pub reason: String,
    #[serde(default)]
    pub task: Option<FollowUpProposal>,
    #[serde(default)]
    pub question: String,
}

impl FollowUpVerdict {
    /// The verdict in the job's stdout, found the way
    /// [`super::ReviewVerdict::parse`] finds the review's.
    pub fn parse(stdout: &str) -> Result<Self, String> {
        super::parse_json_object(stdout)
            .map_err(|error| format!("the follow-up triage printed no verdict JSON: {error}"))
    }
}

/// The options of the `follow_up` ask, which the supervisor applies once
/// answered (ADR-0037 decision 7). `adopt` is offered only with a valid
/// proposal.
pub const FOLLOW_UP_OPTIONS: &[&str] = &["adopt", "cancel", "keep_draft"];

/// `follow_up_triage_started` events after which a draft is not started
/// again: the next attempt records `follow_up_triage_failed` instead.
pub const MAX_FOLLOW_UP_TRIAGE_ATTEMPTS: usize = 3;

/// A follow-up this many steps from a person's judgement is not adopted
/// without one (ADR-0037 decision 6).
pub const FOLLOW_UP_ASK_DEPTH: i64 = 2;

/// Where a draft and its proposal stand when an `adopt` is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FollowUpFacts {
    /// The draft belongs to a goal that is not closed.
    pub goal_open: bool,
    /// The draft's `follow_up_depth`.
    pub depth: i64,
}

/// Why the runtime turns an `adopt` into an ask (ADR-0037 decision 6), or
/// `None` to adopt: the draft's goal is closed or missing, the draft is
/// [`FOLLOW_UP_ASK_DEPTH`] or more follow-ups from a person, or the proposal
/// has no acceptance. An invalid proposal (decision 4) is checked by the
/// caller, which knows the dependencies too.
pub fn adopt_override(facts: FollowUpFacts, proposal: &FollowUpProposal) -> Option<String> {
    if !facts.goal_open {
        Some("the draft's goal is closed (or it has none), so a person decides whether it belongs to a new goal".to_owned())
    } else if facts.depth >= FOLLOW_UP_ASK_DEPTH {
        Some(format!(
            "the draft is a follow-up {} steps from a person's judgement (at most {} is adopted without one)",
            facts.depth,
            FOLLOW_UP_ASK_DEPTH - 1
        ))
    } else if proposal.acceptance.trim().is_empty() {
        Some("the proposed task has no acceptance criteria".to_owned())
    } else {
        None
    }
}

/// How the runtime applied a verdict or an answer to a draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowUpAction {
    /// A new `ready` task replaced the draft, which is canceled.
    Adopted,
    /// The draft is canceled.
    Dropped,
    /// A `follow_up` ask waits for a person; the draft stays.
    Asked,
    /// A person kept the draft for the planner.
    KeptDraft,
    /// The draft had already moved on (a person readied or canceled it):
    /// nothing was applied, and an ask about it was closed.
    Closed,
}

impl FollowUpAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Adopted => "adopted",
            Self::Dropped => "dropped",
            Self::Asked => "asked",
            Self::KeptDraft => "kept_draft",
            Self::Closed => "closed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal() -> FollowUpProposal {
        FollowUpProposal {
            title: "fix".into(),
            description: "d".into(),
            acceptance: "a".into(),
            verification_commands: vec!["cargo test".into()],
            paths: vec!["docs/**".into()],
            evidence: vec!["e2e".into()],
            depends_on: vec![3],
            context: "c".into(),
        }
    }

    #[test]
    fn verdict_parses_the_whole_text_or_the_outermost_object_and_rejects_unknown_fields() {
        let verdict = FollowUpVerdict::parse(
            "Here:\n```json\n{\"verdict\":\"adopt\",\"reason\":\"r\",\"task\":{\"title\":\"t\",\"acceptance\":\"a\"}}\n```",
        )
        .unwrap();
        assert_eq!(verdict.verdict, FollowUpDecision::Adopt);
        let task = verdict.task.unwrap();
        assert_eq!(task.title, "t");
        assert!(task.depends_on.is_empty());
        let drop = FollowUpVerdict::parse("{\"verdict\":\"drop\",\"reason\":\"done\"}").unwrap();
        assert_eq!(drop.verdict, FollowUpDecision::Drop);
        assert!(drop.task.is_none() && drop.question.is_empty());
        for bad in [
            "{\"verdict\":\"drop\",\"reason\":\"r\",\"extra\":1}",
            "{\"verdict\":\"adopt\",\"reason\":\"r\",\"task\":{\"title\":\"t\",\"owner\":\"x\"}}",
            "{\"verdict\":\"maybe\",\"reason\":\"r\"}",
            "no json",
        ] {
            let error = FollowUpVerdict::parse(bad).unwrap_err();
            assert!(error.contains("follow-up triage"), "{error}");
        }
    }

    type Spoil = fn(&mut FollowUpProposal);

    #[test]
    fn proposal_check_names_what_is_wrong() {
        proposal().check().unwrap();
        let cases: [(Spoil, &str); 5] = [
            (|p| p.title = " ".into(), "blank title"),
            (
                |p| p.verification_commands = vec![String::new()],
                "blank verification",
            ),
            (|p| p.paths = vec!["/abs".into()], "paths are invalid"),
            (|p| p.evidence = vec!["lint".into()], "unknown evidence"),
            (|p| p.depends_on = vec![0], "depends on task 0"),
        ];
        for (spoil, expected) in cases {
            let mut p = proposal();
            spoil(&mut p);
            let error = p.check().unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn new_task_puts_the_origin_before_the_context() {
        let task = proposal().new_task(Some(super::super::GoalId::new(2)), "origin");
        assert_eq!(task.context, "origin\n\nc");
        assert_eq!(task.required_evidence, vec![EvidenceCheck::E2e]);
        assert_eq!(task.dependencies, vec![TaskId::new(3)]);
        task.validate().unwrap();
        let mut bare = proposal();
        bare.context = " ".into();
        assert_eq!(bare.new_task(None, "origin").context, "origin");
    }

    #[test]
    fn adopt_is_overridden_for_a_closed_goal_depth_two_or_no_acceptance() {
        let open = FollowUpFacts {
            goal_open: true,
            depth: 1,
        };
        assert_eq!(adopt_override(open, &proposal()), None);
        let closed = adopt_override(
            FollowUpFacts {
                goal_open: false,
                ..open
            },
            &proposal(),
        )
        .unwrap();
        assert!(closed.contains("closed"), "{closed}");
        let deep = adopt_override(FollowUpFacts { depth: 2, ..open }, &proposal()).unwrap();
        assert!(deep.contains("2 steps"), "{deep}");
        let mut empty = proposal();
        empty.acceptance = "  ".into();
        let blank = adopt_override(open, &empty).unwrap();
        assert!(blank.contains("acceptance"), "{blank}");
    }

    #[test]
    fn action_names() {
        for (action, name) in [
            (FollowUpAction::Adopted, "adopted"),
            (FollowUpAction::Dropped, "dropped"),
            (FollowUpAction::Asked, "asked"),
            (FollowUpAction::KeptDraft, "kept_draft"),
            (FollowUpAction::Closed, "closed"),
        ] {
            assert_eq!(action.as_str(), name);
            assert_eq!(serde_json::to_value(action).unwrap(), name);
        }
    }
}
