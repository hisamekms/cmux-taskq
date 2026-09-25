//! The goal aggregate: its state, the rules that create and restore it, and
//! the commands and queries on it. The fields are private, so a goal changes
//! only through the functions here; the store saves what they return.

use serde::Serialize;

use super::{
    DomainError, GoalEdit, GoalId, GoalRecord, GoalStatus, GoalVerdict, NewGoal, TaskStatus,
    TaskStatusCounts, require,
};

impl GoalVerdict {
    /// `achieved` needs every task finished; `abandoned` only needs nothing
    /// executing, so unstarted tasks stay in the queue as they are.
    pub fn allows(self, task: TaskStatus) -> bool {
        match self {
            Self::Achieved => task.is_terminal(),
            Self::Abandoned => task != TaskStatus::InProgress,
        }
    }
}

/// A higher-level problem that several tasks solve together (ADR-0009). The
/// description, acceptance and constraints align the judgement of sibling
/// tasks; `doc` is a path inside the repository. There are no verification
/// commands: machine checks belong to a task that depends on the others.
/// `Serialize` is the JSON the CLI prints; there is no `Deserialize`: a goal
/// is built by [`Goal::new`] or [`Goal::restore`] only.
#[derive(Debug, Clone, Serialize)]
pub struct Goal {
    id: GoalId,
    title: String,
    description: String,
    acceptance: String,
    constraints: String,
    doc: Option<String>,
    status: GoalStatus,
    /// Set together with `verdict` by the one close.
    closed_at: Option<String>,
    verdict: Option<GoalVerdict>,
    created_at: String,
    updated_at: String,
}

const GOAL_TITLE_BLANK: DomainError = DomainError::Blank {
    field: "goal title",
};

impl Goal {
    /// A goal registered now as `id`: open, or a draft when `new.draft`
    /// (ADR-0024 decision 5), and not closed. A blank `doc` is no reference.
    pub fn new(id: GoalId, new: NewGoal, created_at: String) -> Result<Self, DomainError> {
        new.validate()?;
        require_positive(id)?;
        Ok(Self {
            id,
            title: new.title,
            description: new.description,
            acceptance: new.acceptance,
            constraints: new.constraints,
            doc: new.doc.filter(|d| !d.trim().is_empty()),
            status: if new.draft {
                GoalStatus::Draft
            } else {
                GoalStatus::Open
            },
            closed_at: None,
            verdict: None,
            updated_at: created_at.clone(),
            created_at,
        })
    }

    /// A stored goal as it was saved. Checked: a positive ID, a title that
    /// is not blank, and a close time exactly when there is a verdict.
    pub fn restore(record: GoalRecord) -> Result<Self, DomainError> {
        require_positive(record.id)?;
        require(!record.title.trim().is_empty(), || GOAL_TITLE_BLANK)?;
        require(
            record.closed_at.is_some() == record.verdict.is_some(),
            || DomainError::GoalCloseInconsistent { goal_id: record.id },
        )?;
        Ok(Self {
            id: record.id,
            title: record.title,
            description: record.description,
            acceptance: record.acceptance,
            constraints: record.constraints,
            doc: record.doc,
            status: record.status,
            closed_at: record.closed_at,
            verdict: record.verdict,
            created_at: record.created_at,
            updated_at: record.updated_at,
        })
    }

    pub fn id(&self) -> GoalId {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn acceptance(&self) -> &str {
        &self.acceptance
    }

    pub fn constraints(&self) -> &str {
        &self.constraints
    }

    pub fn doc(&self) -> Option<&str> {
        self.doc.as_deref()
    }

    pub fn status(&self) -> GoalStatus {
        self.status
    }

    pub fn closed_at(&self) -> Option<&str> {
        self.closed_at.as_deref()
    }

    pub fn verdict(&self) -> Option<GoalVerdict> {
        self.verdict
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    pub fn updated_at(&self) -> &str {
        &self.updated_at
    }

    pub fn is_closed(&self) -> bool {
        self.closed_at.is_some()
    }

    /// An unclosed draft: the only goal `goal ready` opens.
    pub fn is_draft(&self) -> bool {
        self.status == GoalStatus::Draft && !self.is_closed()
    }

    /// The title, consuming the goal.
    pub fn into_title(self) -> String {
        self.title
    }
}

fn require_positive(id: GoalId) -> Result<(), DomainError> {
    require(id.as_i64() > 0, || DomainError::NonPositiveId {
        field: "goal ID",
    })
}

fn require_open(goal: &Goal) -> Result<(), DomainError> {
    require(!goal.is_closed(), || DomainError::GoalAlreadyClosed {
        goal_id: goal.id,
        verdict: goal.verdict,
    })
}

/// Tasks join and move between open goals only; a closed goal is a record.
pub fn check_accepts_tasks(goal: &Goal) -> Result<(), DomainError> {
    require(!goal.is_closed(), || DomainError::GoalClosed {
        goal_id: goal.id,
        verdict: goal.verdict,
    })
}

/// `goal` with the fields of `edit` replaced; a title must stay non-blank
/// and an empty `doc` clears the reference.
pub fn edit(mut goal: Goal, edit: GoalEdit) -> Result<Goal, DomainError> {
    if let Some(title) = edit.title {
        require(!title.trim().is_empty(), || GOAL_TITLE_BLANK)?;
        goal.title = title;
    }
    if let Some(description) = edit.description {
        goal.description = description;
    }
    if let Some(acceptance) = edit.acceptance {
        goal.acceptance = acceptance;
    }
    if let Some(constraints) = edit.constraints {
        goal.constraints = constraints;
    }
    if let Some(doc) = edit.doc {
        goal.doc = Some(doc).filter(|d| !d.trim().is_empty());
    }
    Ok(goal)
}

/// `goal ready`: a draft that is not closed opens, so its tasks become
/// candidates.
pub fn ready(mut goal: Goal) -> Result<Goal, DomainError> {
    require_open(&goal)?;
    require(goal.status == GoalStatus::Draft, || {
        DomainError::GoalNotDraft { goal_id: goal.id }
    })?;
    goal.status = GoalStatus::Open;
    Ok(goal)
}

/// Close `goal` with `verdict` at `closed_at`, its tasks numbering `counts`
/// by status. A goal is closed once; the rejection names the statuses that
/// do not allow the verdict.
pub fn close(
    mut goal: Goal,
    verdict: GoalVerdict,
    counts: &TaskStatusCounts,
    closed_at: String,
) -> Result<Goal, DomainError> {
    require_open(&goal)?;
    let blocking: Vec<(TaskStatus, usize)> = [
        (TaskStatus::Draft, counts.draft),
        (TaskStatus::Submitted, counts.submitted),
        (TaskStatus::Ready, counts.ready),
        (TaskStatus::InProgress, counts.in_progress),
    ]
    .into_iter()
    .filter(|(status, n)| *n > 0 && !verdict.allows(*status))
    .collect();
    require(blocking.is_empty(), || DomainError::GoalCloseBlocked {
        goal_id: goal.id,
        verdict,
        blocking,
    })?;
    goal.verdict = Some(verdict);
    goal.updated_at = closed_at.clone();
    goal.closed_at = Some(closed_at);
    Ok(goal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(status: GoalStatus, verdict: Option<GoalVerdict>) -> GoalRecord {
        GoalRecord {
            id: GoalId::new(7),
            title: "g".into(),
            description: String::new(),
            acceptance: String::new(),
            constraints: String::new(),
            doc: None,
            status,
            closed_at: verdict.map(|_| "2026-09-23T00:00:00Z".into()),
            verdict,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn goal(verdict: Option<GoalVerdict>) -> Goal {
        Goal::restore(record(GoalStatus::Open, verdict)).unwrap()
    }

    #[test]
    fn a_new_goal_is_open_or_a_draft_and_not_closed() {
        let new = NewGoal {
            title: "g".into(),
            doc: Some(" ".into()),
            ..NewGoal::default()
        };
        let goal = Goal::new(GoalId::new(1), new.clone(), "now".into()).unwrap();
        assert_eq!(goal.status(), GoalStatus::Open);
        assert!(!goal.is_closed());
        assert_eq!(goal.doc(), None);
        assert_eq!((goal.created_at(), goal.updated_at()), ("now", "now"));
        let draft = Goal::new(
            GoalId::new(1),
            NewGoal {
                draft: true,
                doc: Some("docs/g.md".into()),
                ..new
            },
            "now".into(),
        )
        .unwrap();
        assert!(draft.is_draft());
        assert_eq!(draft.doc(), Some("docs/g.md"));
        assert_eq!(
            Goal::new(GoalId::new(1), NewGoal::default(), "now".into())
                .unwrap_err()
                .to_string(),
            "goal title must not be blank"
        );
        assert_eq!(
            serde_json::to_value(&draft).unwrap(),
            serde_json::json!({
                "id": 1, "title": "g", "description": "", "acceptance": "",
                "constraints": "", "doc": "docs/g.md", "status": "draft",
                "closed_at": null, "verdict": null,
                "created_at": "now", "updated_at": "now"
            })
        );
    }

    #[test]
    fn restore_checks_the_id_title_and_close() {
        let closed = goal(Some(GoalVerdict::Achieved));
        assert_eq!(closed.closed_at(), Some("2026-09-23T00:00:00Z"));
        assert_eq!(closed.verdict(), Some(GoalVerdict::Achieved));
        assert_eq!(
            Goal::restore(GoalRecord {
                verdict: None,
                ..record(GoalStatus::Open, Some(GoalVerdict::Achieved))
            })
            .unwrap_err()
            .to_string(),
            "goal 7 has a close time without a verdict or a verdict without a close time"
        );
        assert_eq!(
            Goal::restore(GoalRecord {
                title: " ".into(),
                ..record(GoalStatus::Open, None)
            })
            .unwrap_err(),
            GOAL_TITLE_BLANK
        );
        assert_eq!(
            Goal::restore(GoalRecord {
                id: GoalId::new(0),
                ..record(GoalStatus::Open, None)
            })
            .unwrap_err()
            .to_string(),
            "goal ID must be positive"
        );
    }

    #[test]
    fn only_an_unclosed_draft_goal_becomes_ready() {
        let draft = Goal::restore(record(GoalStatus::Draft, None)).unwrap();
        assert!(draft.is_draft());
        assert_eq!(ready(draft).unwrap().status(), GoalStatus::Open);
        assert_eq!(
            ready(goal(None)).unwrap_err().to_string(),
            "goal 7 is not a draft"
        );
        let closed =
            Goal::restore(record(GoalStatus::Draft, Some(GoalVerdict::Abandoned))).unwrap();
        assert!(!closed.is_draft());
        assert_eq!(
            ready(closed).unwrap_err().to_string(),
            "goal 7 is already closed as abandoned"
        );
    }

    #[test]
    fn a_closed_goal_takes_no_tasks() {
        check_accepts_tasks(&goal(None)).unwrap();
        assert_eq!(
            check_accepts_tasks(&goal(Some(GoalVerdict::Abandoned)))
                .unwrap_err()
                .to_string(),
            "goal 7 is closed as abandoned; create a new goal for further work"
        );
    }

    #[test]
    fn an_edit_replaces_the_given_fields() {
        let edited = edit(
            goal(None),
            GoalEdit {
                title: Some("new".into()),
                description: Some("d".into()),
                acceptance: Some("a".into()),
                constraints: Some("c".into()),
                doc: Some("x.md".into()),
            },
        )
        .unwrap();
        assert_eq!(
            (
                edited.title(),
                edited.description(),
                edited.acceptance(),
                edited.constraints(),
                edited.doc()
            ),
            ("new", "d", "a", "c", Some("x.md"))
        );
        let cleared = edit(
            edited,
            GoalEdit {
                doc: Some(String::new()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
        assert_eq!(cleared.doc(), None);
        assert_eq!(cleared.into_title(), "new");
        assert_eq!(
            edit(
                goal(None),
                GoalEdit {
                    title: Some(" ".into()),
                    ..GoalEdit::default()
                }
            )
            .unwrap_err()
            .to_string(),
            "goal title must not be blank"
        );
    }

    #[test]
    fn a_goal_closes_once_and_only_when_its_tasks_allow_the_verdict() {
        let mut counts = TaskStatusCounts::default();
        counts.count(TaskStatus::Submitted, 1);
        counts.count(TaskStatus::Ready, 2);
        counts.count(TaskStatus::InProgress, 1);
        assert_eq!((counts.total, counts.submitted), (4, 1));
        let at = || "2026-09-24T00:00:00Z".to_owned();
        assert_eq!(
            close(goal(None), GoalVerdict::Achieved, &counts, at())
                .unwrap_err()
                .to_string(),
            "goal 7 cannot be closed as achieved: 1 task(s) submitted, 2 task(s) ready, \
             1 task(s) in_progress"
        );
        assert_eq!(
            close(goal(None), GoalVerdict::Abandoned, &counts, at()).unwrap_err(),
            DomainError::GoalCloseBlocked {
                goal_id: GoalId::new(7),
                verdict: GoalVerdict::Abandoned,
                blocking: vec![(TaskStatus::InProgress, 1)]
            }
        );
        counts.in_progress = 0;
        let closed = close(goal(None), GoalVerdict::Abandoned, &counts, at()).unwrap();
        assert!(closed.is_closed());
        assert_eq!(closed.verdict(), Some(GoalVerdict::Abandoned));
        assert_eq!(closed.closed_at(), Some("2026-09-24T00:00:00Z"));
        assert_eq!(closed.updated_at(), "2026-09-24T00:00:00Z");
        assert_eq!(
            close(closed, GoalVerdict::Achieved, &counts, at())
                .unwrap_err()
                .to_string(),
            "goal 7 is already closed as abandoned"
        );
    }
}
