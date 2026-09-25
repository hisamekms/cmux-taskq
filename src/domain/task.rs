//! The task aggregate: its state, the rules that create and restore it, and
//! the commands and queries on it. The fields are private, so a task changes
//! only through the functions here; the store saves what they return.

use serde::Serialize;

use super::{
    DomainError, EvidenceCheck, GoalId, NewTask, Priority, TaskEdit, TaskId, TaskRecord,
    TaskStatus, require,
    scope::{dedup_globs, validate_path_globs},
};

/// What moves a task between statuses by hand or by plan review; only a
/// claim marks it in progress and only a landing completes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskAction {
    /// A retry: an in-progress task whose runs all failed or were
    /// interrupted goes back to ready unchanged, which needs no plan review
    /// (ADR-0041 decision 8). A draft or submitted task is refused: plan
    /// review ([`Self::Approve`]) or a person's bypass readies it.
    Ready,
    Draft,
    Cancel,
    /// `dagq submit`: a draft enters plan review in a proposal.
    Submit,
    /// Plan review passed the task's proposal: the one path to ready for a
    /// submitted task besides the bypass.
    Approve,
    /// `ready --bypass-review`: a person skips plan review.
    BypassReview,
}

impl TaskStatus {
    /// `unfinished_run` is whether the task still owns a run that is executing,
    /// awaiting or undergoing integration, or waiting for a session. An in-progress task whose runs have all failed or
    /// been interrupted may be retried or canceled by hand; a retry is a new run.
    pub fn transition(self, action: TaskAction, unfinished_run: bool) -> Result<Self, DomainError> {
        match (self, action) {
            (Self::Draft, TaskAction::Submit) => Ok(Self::Submitted),
            (Self::Submitted, TaskAction::Approve)
            | (Self::Draft | Self::Submitted, TaskAction::BypassReview) => Ok(Self::Ready),
            (Self::Draft | Self::Submitted, TaskAction::Ready) => {
                Err(DomainError::ReadyNeedsPlanReview { status: self })
            }
            (Self::Ready | Self::Submitted, TaskAction::Draft) => Ok(Self::Draft),
            (Self::Draft | Self::Submitted | Self::Ready, TaskAction::Cancel) => Ok(Self::Canceled),
            (Self::InProgress, _) if unfinished_run => {
                Err(DomainError::TaskHasUnfinishedRun { action })
            }
            (Self::InProgress, TaskAction::Ready) => Ok(Self::Ready),
            (Self::InProgress, TaskAction::Draft) => Ok(Self::Draft),
            (Self::InProgress, TaskAction::Cancel) => Ok(Self::Canceled),
            _ => Err(DomainError::TransitionNotAllowed {
                status: self,
                action,
            }),
        }
    }

    /// Dependencies, the goal, the paths and the priority may change only
    /// before the task is claimed; plan review adds dependencies and lowers
    /// priorities of submitted tasks (ADR-0041 decision 11).
    pub fn dependencies_editable(self) -> bool {
        matches!(self, Self::Draft | Self::Submitted | Self::Ready)
    }

    /// Whether `dagq edit` may change the content of the task: a draft or a
    /// submitted task (ADR-0041 decision 9). A ready task goes back to
    /// submitted to be edited (decision 14).
    pub fn content_editable(self) -> bool {
        matches!(self, Self::Draft | Self::Submitted)
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Canceled)
    }
}

/// A unit of work the queue runs. `Serialize` is the JSON the CLI prints;
/// there is no `Deserialize`: a task is built by [`Task::new`] or
/// [`Task::restore`] only.
#[derive(Debug, Clone, Serialize)]
pub struct Task {
    id: TaskId,
    title: String,
    description: String,
    acceptance: String,
    verification_commands: Vec<String>,
    /// Receipt checks validation requires to be `passed` with evidence
    /// (ADR-0019 decision 5); a receipt without them parks the run as
    /// `needs_session`.
    required_evidence: Vec<EvidenceCheck>,
    /// Globs of the paths a run may change (ADR-0029): validation and
    /// `integrate` refuse a diff with a path none of them matches. Empty:
    /// no limit.
    paths: Vec<String>,
    /// How urgently a person wants it claimed (ADR-0040 decision 4).
    priority: Priority,
    status: TaskStatus,
    goal_id: Option<GoalId>,
    context: String,
    created_at: String,
    updated_at: String,
}

impl Task {
    /// A task registered now as `id`: the creation rules of [`NewTask`]
    /// hold, it starts as a draft, and its required checks and path globs
    /// are kept once each. `new.dependencies` are edges the store records
    /// next to the task; they are not part of it.
    pub fn new(id: TaskId, new: NewTask, created_at: String) -> Result<Self, DomainError> {
        new.validate()?;
        require_positive(id)?;
        Ok(Self {
            id,
            required_evidence: new.required_evidence(),
            paths: dedup_globs(&new.paths),
            priority: new.priority,
            title: new.title,
            description: new.description,
            acceptance: new.acceptance,
            verification_commands: new.verification_commands,
            status: TaskStatus::Draft,
            goal_id: new.goal_id,
            context: new.context,
            updated_at: created_at.clone(),
            created_at,
        })
    }

    /// A stored task as it was saved. Only what every stored task satisfies
    /// is checked (a positive ID and goal ID, a title that is not blank); the
    /// creation rules are not applied again.
    pub fn restore(record: TaskRecord) -> Result<Self, DomainError> {
        require_positive(record.id)?;
        require(!record.title.trim().is_empty(), || DomainError::Blank {
            field: "task title",
        })?;
        require(record.goal_id.is_none_or(|id| id.as_i64() > 0), || {
            DomainError::NonPositiveId { field: "goal ID" }
        })?;
        Ok(Self {
            id: record.id,
            title: record.title,
            description: record.description,
            acceptance: record.acceptance,
            verification_commands: record.verification_commands,
            required_evidence: record.required_evidence,
            paths: record.paths,
            priority: record.priority,
            status: record.status,
            goal_id: record.goal_id,
            context: record.context,
            created_at: record.created_at,
            updated_at: record.updated_at,
        })
    }

    pub fn id(&self) -> TaskId {
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

    pub fn verification_commands(&self) -> &[String] {
        &self.verification_commands
    }

    pub fn required_evidence(&self) -> &[EvidenceCheck] {
        &self.required_evidence
    }

    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    pub fn priority(&self) -> Priority {
        self.priority
    }

    pub fn status(&self) -> TaskStatus {
        self.status
    }

    pub fn goal_id(&self) -> Option<GoalId> {
        self.goal_id
    }

    pub fn context(&self) -> &str {
        &self.context
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    pub fn updated_at(&self) -> &str {
        &self.updated_at
    }

    /// The title, consuming the task.
    pub fn into_title(self) -> String {
        self.title
    }
}

fn require_positive(id: TaskId) -> Result<(), DomainError> {
    require(id.as_i64() > 0, || DomainError::NonPositiveId {
        field: "task ID",
    })
}

/// Whether the task's dependencies, goal and paths may still change: only
/// before it is claimed.
pub fn dependencies_editable(task: &Task) -> bool {
    task.status.dependencies_editable()
}

/// Rejects a change to `what` of a task that is already claimed or finished.
fn require_editable(task: &Task, what: &'static str) -> Result<(), DomainError> {
    require(dependencies_editable(task), || {
        DomainError::TaskNotEditable { what }
    })
}

/// Apply a user's `action` to `task`; `unfinished_run` is whether it still
/// owns an unfinished run (see [`TaskStatus::transition`]).
pub fn transition(
    mut task: Task,
    action: TaskAction,
    unfinished_run: bool,
) -> Result<Task, DomainError> {
    task.status = task.status.transition(action, unfinished_run)?;
    Ok(task)
}

/// A supervisor takes `task` for a new run: only a ready task is claimed,
/// and it stays `in_progress` until its run lands or a person moves it.
pub fn claim(mut task: Task) -> Result<Task, DomainError> {
    require(task.status == TaskStatus::Ready, || {
        DomainError::TaskNotClaimable {
            task_id: task.id,
            status: task.status,
        }
    })?;
    task.status = TaskStatus::InProgress;
    Ok(task)
}

/// Move `task` to `goal_id`, or out of any goal. Whether the goal takes tasks
/// is [`super::goal::check_accepts_tasks`], which the caller applies to the
/// goal it reads.
pub fn set_goal(mut task: Task, goal_id: Option<GoalId>) -> Result<Task, DomainError> {
    require_editable(&task, "the goal")?;
    task.goal_id = goal_id;
    Ok(task)
}

/// Replace the path globs `task` may change (ADR-0029), each kept once.
pub fn set_paths(mut task: Task, paths: Vec<String>) -> Result<Task, DomainError> {
    validate_path_globs(&paths)?;
    require_editable(&task, "the paths")?;
    task.paths = dedup_globs(&paths);
    Ok(task)
}

/// Give `task` another priority (ADR-0040 decision 4): only before it is
/// claimed, so a running run is never preempted.
pub fn set_priority(mut task: Task, priority: Priority) -> Result<Task, DomainError> {
    require_editable(&task, "the priority")?;
    task.priority = priority;
    Ok(task)
}

/// `task` with the fields of `edit` replaced (`dagq edit`): only while its
/// status keeps the content editable, with the creation rules of
/// [`NewTask`] for what changes; the required checks and globs are kept
/// once each.
pub fn edit(mut task: Task, edit: TaskEdit) -> Result<Task, DomainError> {
    edit.validate()?;
    require(task.status.content_editable(), || {
        DomainError::TaskContentNotEditable {
            task_id: task.id,
            status: task.status,
        }
    })?;
    if let Some(title) = edit.title {
        task.title = title;
    }
    if let Some(description) = edit.description {
        task.description = description;
    }
    if let Some(acceptance) = edit.acceptance {
        task.acceptance = acceptance;
    }
    if let Some(commands) = edit.verification_commands {
        task.verification_commands = commands;
    }
    if let Some(checks) = edit.required_evidence {
        task.required_evidence = Vec::new();
        for check in checks {
            if !task.required_evidence.contains(&check) {
                task.required_evidence.push(check);
            }
        }
    }
    if let Some(paths) = edit.paths {
        task.paths = dedup_globs(&paths);
    }
    if let Some(context) = edit.context {
        task.context = context;
    }
    Ok(task)
}

/// A task never depends on itself; checked before either task is read.
pub fn check_not_self(task_id: TaskId, predecessor_id: TaskId) -> Result<(), DomainError> {
    require(task_id != predecessor_id, || DomainError::SelfDependency)
}

/// Whether `task` may gain or lose a predecessor.
pub fn check_dependencies_editable(task: &Task) -> Result<(), DomainError> {
    require_editable(task, "dependencies")
}

/// `creates_cycle` is whether `predecessor_id` already depends on `task_id`,
/// directly or not; the store finds it over the whole dependency graph,
/// where a task also waits for the goals it depends on and a goal for its
/// tasks (ADR-0038).
pub fn check_acyclic(
    task_id: TaskId,
    predecessor_id: TaskId,
    creates_cycle: bool,
) -> Result<(), DomainError> {
    require(!creates_cycle, || DomainError::DependencyCycle {
        task_id,
        predecessor_id,
    })
}

/// A task never depends on the goal it belongs to: the goal already waits
/// for it.
pub fn check_not_own_goal(task: &Task, goal_id: GoalId) -> Result<(), DomainError> {
    require(task.goal_id != Some(goal_id), || {
        DomainError::OwnGoalDependency { goal_id }
    })
}

/// `creates_cycle` is whether `goal_id` already waits for `task_id`,
/// directly or not, over the same graph as [`check_acyclic`].
pub fn check_goal_acyclic(
    task_id: TaskId,
    goal_id: GoalId,
    creates_cycle: bool,
) -> Result<(), DomainError> {
    require(!creates_cycle, || DomainError::GoalDependencyCycle {
        task_id,
        goal_id,
    })
}

/// Whether `task`, moved to `goal_id`, keeps the graph acyclic:
/// `depends_on_goal` is whether it depends on that goal directly, and
/// `creates_cycle` whether it waits for the goal at all (the store finds it
/// over the whole graph). A direct dependency is a dependency on its own
/// goal; an indirect one a cycle through the goal's membership.
pub fn check_membership_acyclic(
    task: &Task,
    goal_id: GoalId,
    depends_on_goal: bool,
    creates_cycle: bool,
) -> Result<(), DomainError> {
    require(!depends_on_goal, || DomainError::OwnGoalDependency {
        goal_id,
    })?;
    require(!creates_cycle, || DomainError::GoalMembershipCycle {
        task_id: task.id,
        goal_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_task() -> NewTask {
        NewTask {
            title: "t".into(),
            description: "d".into(),
            acceptance: "a".into(),
            verification_commands: vec!["cargo test".into()],
            required_evidence: vec![EvidenceCheck::E2e, EvidenceCheck::E2e],
            paths: vec!["docs/**".into(), "docs/**".into()],
            priority: Default::default(),
            dependencies: vec![TaskId::new(1)],
            goal_dependencies: Vec::new(),
            goal_id: Some(GoalId::new(2)),
            context: "c".into(),
        }
    }

    fn record(status: TaskStatus) -> TaskRecord {
        TaskRecord {
            id: TaskId::new(5),
            title: "t".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            status,
            goal_id: None,
            context: String::new(),
            created_at: "c".into(),
            updated_at: "u".into(),
        }
    }

    #[test]
    fn claim_takes_only_a_ready_task() {
        let task = claim(Task::restore(record(TaskStatus::Ready)).unwrap()).unwrap();
        assert_eq!(task.status(), TaskStatus::InProgress);
        let error = claim(task).unwrap_err();
        assert_eq!(
            error,
            DomainError::TaskNotClaimable {
                task_id: TaskId::new(5),
                status: TaskStatus::InProgress,
            }
        );
        assert_eq!(error.to_string(), "task 5 is in_progress, not ready");
    }

    #[test]
    fn a_new_task_is_a_draft_with_each_check_and_glob_once() {
        let task = Task::new(TaskId::new(3), new_task(), "now".into()).unwrap();
        assert_eq!(task.id(), TaskId::new(3));
        assert_eq!(task.status(), TaskStatus::Draft);
        assert_eq!(task.required_evidence(), [EvidenceCheck::E2e]);
        assert_eq!(task.paths(), ["docs/**"]);
        assert_eq!(task.goal_id(), Some(GoalId::new(2)));
        assert_eq!((task.created_at(), task.updated_at()), ("now", "now"));
        assert_eq!(
            (task.title(), task.description(), task.acceptance()),
            ("t", "d", "a")
        );
        assert_eq!(task.verification_commands(), ["cargo test"]);
        assert_eq!(task.context(), "c");
        assert_eq!(
            serde_json::to_value(&task).unwrap()["status"],
            serde_json::json!("draft")
        );
        assert_eq!(task.into_title(), "t");

        let blank = NewTask {
            title: " ".into(),
            ..new_task()
        };
        assert_eq!(
            Task::new(TaskId::new(3), blank, "now".into())
                .unwrap_err()
                .to_string(),
            "task title must not be blank"
        );
        assert_eq!(
            Task::new(TaskId::new(0), new_task(), "now".into())
                .unwrap_err()
                .to_string(),
            "task ID must be positive"
        );
    }

    #[test]
    fn restore_keeps_the_stored_state_and_checks_its_invariants() {
        let task = Task::restore(record(TaskStatus::InProgress)).unwrap();
        assert_eq!(task.status(), TaskStatus::InProgress);
        assert_eq!((task.created_at(), task.updated_at()), ("c", "u"));
        assert_eq!(
            Task::restore(TaskRecord {
                title: "".into(),
                ..record(TaskStatus::Ready)
            })
            .unwrap_err(),
            DomainError::Blank {
                field: "task title"
            }
        );
        assert_eq!(
            Task::restore(TaskRecord {
                id: TaskId::new(-1),
                ..record(TaskStatus::Ready)
            })
            .unwrap_err()
            .to_string(),
            "task ID must be positive"
        );
        assert_eq!(
            Task::restore(TaskRecord {
                goal_id: Some(GoalId::new(0)),
                ..record(TaskStatus::Ready)
            })
            .unwrap_err()
            .to_string(),
            "goal ID must be positive"
        );
    }

    #[test]
    fn transition_follows_the_status_rules() {
        let ready = transition(
            Task::restore(record(TaskStatus::Draft)).unwrap(),
            TaskAction::BypassReview,
            false,
        )
        .unwrap();
        assert_eq!(ready.status(), TaskStatus::Ready);
        let canceled = transition(ready, TaskAction::Cancel, false).unwrap();
        assert_eq!(canceled.status(), TaskStatus::Canceled);
        assert_eq!(
            transition(canceled, TaskAction::Ready, false).unwrap_err(),
            DomainError::TransitionNotAllowed {
                status: TaskStatus::Canceled,
                action: TaskAction::Ready
            }
        );
        let in_progress = Task::restore(record(TaskStatus::InProgress)).unwrap();
        assert!(transition(in_progress.clone(), TaskAction::Draft, true).is_err());
        assert_eq!(
            transition(in_progress.clone(), TaskAction::Draft, false)
                .unwrap()
                .status(),
            TaskStatus::Draft
        );
        // A retry returns an in-progress task to ready without plan review.
        assert_eq!(
            transition(in_progress.clone(), TaskAction::Ready, false)
                .unwrap()
                .status(),
            TaskStatus::Ready
        );
        assert!(transition(in_progress, TaskAction::BypassReview, false).is_err());
    }

    #[test]
    fn only_plan_review_or_a_bypass_readies_a_draft_or_submitted_task() {
        let draft = || Task::restore(record(TaskStatus::Draft)).unwrap();
        let submitted = transition(draft(), TaskAction::Submit, false).unwrap();
        assert_eq!(submitted.status(), TaskStatus::Submitted);
        assert_eq!(
            serde_json::to_value(submitted.status()).unwrap(),
            serde_json::json!("submitted")
        );
        for task in [draft(), submitted.clone()] {
            let status = task.status();
            let error = transition(task, TaskAction::Ready, false).unwrap_err();
            assert_eq!(error, DomainError::ReadyNeedsPlanReview { status });
            assert!(error.to_string().contains("pass --bypass-review"));
        }
        assert_eq!(
            transition(submitted.clone(), TaskAction::Approve, false)
                .unwrap()
                .status(),
            TaskStatus::Ready
        );
        assert_eq!(
            transition(submitted.clone(), TaskAction::BypassReview, false)
                .unwrap()
                .status(),
            TaskStatus::Ready
        );
        assert_eq!(
            transition(submitted.clone(), TaskAction::Draft, false)
                .unwrap()
                .status(),
            TaskStatus::Draft
        );
        assert_eq!(
            transition(submitted.clone(), TaskAction::Cancel, false)
                .unwrap()
                .status(),
            TaskStatus::Canceled
        );
        // Plan review approves only a submitted task, and submits only a draft.
        assert!(transition(draft(), TaskAction::Approve, false).is_err());
        assert_eq!(
            transition(submitted, TaskAction::Submit, false)
                .unwrap_err()
                .to_string(),
            "cannot apply Submit to task in submitted state"
        );
        let ready = Task::restore(record(TaskStatus::Ready)).unwrap();
        assert!(transition(ready, TaskAction::Submit, false).is_err());
        // Nothing claims a submitted task.
        let waiting = Task::restore(record(TaskStatus::Submitted)).unwrap();
        assert!(claim(waiting.clone()).is_err());
        assert!(waiting.status().content_editable());
        assert!(dependencies_editable(&waiting));
        assert_eq!(
            set_priority(waiting, Priority::Low).unwrap().priority(),
            Priority::Low
        );
    }

    #[test]
    fn only_a_draft_or_ready_task_changes_its_goal_paths_or_dependencies() {
        let ready = Task::restore(record(TaskStatus::Ready)).unwrap();
        assert!(dependencies_editable(&ready));
        check_dependencies_editable(&ready).unwrap();
        let moved = set_goal(ready, Some(GoalId::new(4))).unwrap();
        assert_eq!(moved.goal_id(), Some(GoalId::new(4)));
        let scoped = set_paths(moved, vec!["src/**".into(), "src/**".into()]).unwrap();
        assert_eq!(scoped.paths(), ["src/**"]);
        assert_eq!(scoped.priority(), Priority::Normal);
        let urgent = set_priority(scoped, Priority::Urgent).unwrap();
        assert_eq!(urgent.priority(), Priority::Urgent);
        assert_eq!(
            serde_json::to_value(&urgent).unwrap()["priority"],
            serde_json::json!("urgent")
        );
        assert!(matches!(
            set_paths(urgent, vec![" ".into()]),
            Err(DomainError::InvalidPathGlob { .. })
        ));
        let draft = Task::restore(record(TaskStatus::Draft)).unwrap();
        assert_eq!(
            set_priority(draft, Priority::Low).unwrap().priority(),
            Priority::Low
        );

        let claimed = || Task::restore(record(TaskStatus::InProgress)).unwrap();
        assert!(!dependencies_editable(&claimed()));
        assert_eq!(
            set_goal(claimed(), None).unwrap_err().to_string(),
            "the goal can only be changed for draft, submitted or ready tasks"
        );
        assert_eq!(
            set_paths(claimed(), Vec::new()).unwrap_err().to_string(),
            "the paths can only be changed for draft, submitted or ready tasks"
        );
        for status in [
            TaskStatus::InProgress,
            TaskStatus::Completed,
            TaskStatus::Canceled,
        ] {
            let task = Task::restore(record(status)).unwrap();
            assert_eq!(
                set_priority(task, Priority::Interrupt)
                    .unwrap_err()
                    .to_string(),
                "the priority can only be changed for draft, submitted or ready tasks"
            );
        }
        assert_eq!(
            check_dependencies_editable(&claimed())
                .unwrap_err()
                .to_string(),
            "dependencies can only be changed for draft, submitted or ready tasks"
        );
    }

    #[test]
    fn edit_replaces_the_given_fields_of_a_draft_or_submitted_task_only() {
        let draft = Task::new(TaskId::new(3), new_task(), "now".into()).unwrap();
        let kept = edit(draft.clone(), TaskEdit::default()).unwrap();
        assert_eq!(
            serde_json::to_value(&kept).unwrap(),
            serde_json::to_value(&draft).unwrap()
        );
        let edited = edit(
            draft,
            TaskEdit {
                title: Some("t2".into()),
                description: Some("d2".into()),
                acceptance: Some("a2".into()),
                verification_commands: Some(Vec::new()),
                required_evidence: Some(vec![
                    EvidenceCheck::Tests,
                    EvidenceCheck::E2e,
                    EvidenceCheck::Tests,
                ]),
                paths: Some(vec!["src/**".into(), "src/**".into()]),
                context: Some("c2".into()),
            },
        )
        .unwrap();
        assert_eq!(
            (edited.title(), edited.description(), edited.acceptance()),
            ("t2", "d2", "a2")
        );
        assert!(edited.verification_commands().is_empty());
        assert_eq!(
            edited.required_evidence(),
            [EvidenceCheck::Tests, EvidenceCheck::E2e]
        );
        assert_eq!(edited.paths(), ["src/**"]);
        assert_eq!(edited.context(), "c2");
        assert_eq!(edited.status(), TaskStatus::Draft);

        let invalid = [
            (
                TaskEdit {
                    title: Some(" ".into()),
                    ..TaskEdit::default()
                },
                "task title must not be blank",
            ),
            (
                TaskEdit {
                    verification_commands: Some(vec!["".into()]),
                    ..TaskEdit::default()
                },
                "verification commands must not be blank",
            ),
        ];
        for (change, message) in invalid {
            assert!(!change.is_empty());
            assert_eq!(
                edit(edited.clone(), change).unwrap_err().to_string(),
                message
            );
        }
        assert!(matches!(
            edit(
                edited,
                TaskEdit {
                    paths: Some(vec!["/abs".into()]),
                    ..TaskEdit::default()
                }
            ),
            Err(DomainError::InvalidPathGlob { .. })
        ));
        assert!(TaskEdit::default().is_empty());

        for status in [
            TaskStatus::Ready,
            TaskStatus::InProgress,
            TaskStatus::Completed,
            TaskStatus::Canceled,
        ] {
            assert!(!status.content_editable());
            let task = Task::restore(record(status)).unwrap();
            let change = TaskEdit {
                context: Some("x".into()),
                ..TaskEdit::default()
            };
            assert_eq!(
                edit(task, change).unwrap_err(),
                DomainError::TaskContentNotEditable {
                    task_id: TaskId::new(5),
                    status,
                }
            );
        }
        let ready = Task::restore(record(TaskStatus::Ready)).unwrap();
        assert_eq!(
            edit(
                ready,
                TaskEdit {
                    title: Some("x".into()),
                    ..TaskEdit::default()
                }
            )
            .unwrap_err()
            .to_string(),
            "task 5 is ready; only a draft or submitted task can be edited"
        );
        let submitted = Task::restore(record(TaskStatus::Submitted)).unwrap();
        let edited = edit(
            submitted,
            TaskEdit {
                acceptance: Some("a3".into()),
                ..TaskEdit::default()
            },
        )
        .unwrap();
        assert_eq!(
            (edited.acceptance(), edited.status()),
            ("a3", TaskStatus::Submitted)
        );
    }

    #[test]
    fn a_dependency_is_neither_on_itself_nor_a_cycle() {
        check_not_self(TaskId::new(1), TaskId::new(2)).unwrap();
        assert_eq!(
            check_not_self(TaskId::new(1), TaskId::new(1))
                .unwrap_err()
                .to_string(),
            "a task cannot depend on itself"
        );
        check_acyclic(TaskId::new(1), TaskId::new(2), false).unwrap();
        assert_eq!(
            check_acyclic(TaskId::new(1), TaskId::new(2), true)
                .unwrap_err()
                .to_string(),
            "dependency 1 -> 2 would create a cycle"
        );
    }

    #[test]
    fn a_goal_dependency_is_neither_on_the_own_goal_nor_a_cycle() {
        let in_goal = Task::restore(TaskRecord {
            goal_id: Some(GoalId::new(4)),
            ..record(TaskStatus::Ready)
        })
        .unwrap();
        check_not_own_goal(&in_goal, GoalId::new(3)).unwrap();
        assert_eq!(
            check_not_own_goal(&in_goal, GoalId::new(4))
                .unwrap_err()
                .to_string(),
            "a task cannot depend on its own goal 4; the goal already waits for it"
        );
        check_goal_acyclic(TaskId::new(5), GoalId::new(3), false).unwrap();
        assert_eq!(
            check_goal_acyclic(TaskId::new(5), GoalId::new(3), true)
                .unwrap_err()
                .to_string(),
            "dependency 5 -> goal 3 would create a cycle"
        );
        check_membership_acyclic(&in_goal, GoalId::new(3), false, false).unwrap();
        assert_eq!(
            check_membership_acyclic(&in_goal, GoalId::new(3), true, true).unwrap_err(),
            DomainError::OwnGoalDependency {
                goal_id: GoalId::new(3)
            }
        );
        assert_eq!(
            check_membership_acyclic(&in_goal, GoalId::new(3), false, true)
                .unwrap_err()
                .to_string(),
            "moving task 5 to goal 3 would create a cycle: the task already waits for the goal"
        );
        let mut own = new_task();
        own.goal_dependencies = vec![GoalId::new(2)];
        assert_eq!(
            own.validate().unwrap_err(),
            DomainError::OwnGoalDependency {
                goal_id: GoalId::new(2)
            }
        );
        own.goal_dependencies = vec![GoalId::new(0)];
        assert_eq!(
            own.validate().unwrap_err().to_string(),
            "goal dependency IDs must be positive"
        );
    }
}
