//! The mechanical checks of a plan (ADR-0041 decision 10): fixed rules
//! `dagq lint` applies to a set of tasks, usually the members of one
//! proposal, as input to plan review. Only rules that hold in any
//! repository live here; a repository's own rules (which verification a
//! kind of change needs, ADR numbers) are read from its documents by the
//! plan review prompt, not built into the runtime.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::{GoalId, GoalVerdict, Task, TaskId, TaskStatus, scope::validate_path_globs};

/// A rule [`lint`] checks; the snake-case name is the `code` it reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LintCode {
    /// The task reaches itself over task and goal dependencies.
    DependencyCycle,
    /// A predecessor is already completed: the dependency waits for nothing.
    DependsOnCompleted,
    /// A predecessor was canceled: it never completes, so the task never runs.
    DependsOnCanceled,
    /// A predecessor outside the linted set is a draft: nothing submitted
    /// it, so it never becomes ready unless someone does.
    DependsOnDraft,
    /// A goal the task waits for was closed as abandoned, which never
    /// releases it.
    DependsOnAbandonedGoal,
    /// The task declares no paths, so it may change anything, and has no
    /// verification commands, so nothing checks the change.
    UnscopedWithoutVerification,
    /// The task declares paths but has no verification commands.
    NoVerification,
    /// A path glob [`validate_path_globs`] refuses.
    InvalidPathGlob,
    /// The acceptance criteria are blank.
    BlankAcceptance,
    /// Another task of the linted set has the same title (ignoring case
    /// and surrounding whitespace).
    DuplicateTitle,
}

/// One broken rule: its code, the task it is about and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LintViolation {
    pub code: LintCode,
    pub task_id: TaskId,
    pub reason: String,
}

/// A task of the queue as the dependency rules see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintNode {
    pub status: TaskStatus,
    pub goal_id: Option<GoalId>,
    /// Direct predecessors.
    pub depends_on: Vec<TaskId>,
    /// Goals the task waits for.
    pub goal_dependencies: Vec<GoalId>,
}

/// One read of the queue for [`lint`]: the tasks to check and, for the
/// dependency rules, every task and the verdict of every goal.
#[derive(Debug, Clone, Default)]
pub struct LintInput {
    /// The tasks to check, in the order to report them.
    pub targets: Vec<Task>,
    /// Every task of the queue.
    pub nodes: BTreeMap<TaskId, LintNode>,
    /// Every goal of the queue with its verdict (`None`: not closed).
    pub goals: BTreeMap<GoalId, Option<GoalVerdict>>,
}

/// Check every rule on `input.targets`. The violations come per target in
/// its order, and within a target in [`LintCode`] order, then by the
/// dependency they name; an empty list means the tasks pass.
pub fn lint(input: &LintInput) -> Vec<LintViolation> {
    let members: BTreeSet<TaskId> = input.targets.iter().map(Task::id).collect();
    let mut goal_members: BTreeMap<GoalId, Vec<TaskId>> = BTreeMap::new();
    for (id, node) in &input.nodes {
        if let Some(goal_id) = node.goal_id {
            goal_members.entry(goal_id).or_default().push(*id);
        }
    }
    let mut violations = Vec::new();
    for task in &input.targets {
        let mut found = Vec::new();
        if let Some(node) = input.nodes.get(&task.id()) {
            if in_cycle(task.id(), &input.nodes, &goal_members) {
                found.push(violation(
                    LintCode::DependencyCycle,
                    task,
                    "it depends on itself through its dependencies".into(),
                ));
            }
            predecessor_rules(task, node, &members, input, &mut found);
            for goal_id in &node.goal_dependencies {
                if input.goals.get(goal_id) == Some(&Some(GoalVerdict::Abandoned)) {
                    found.push(violation(
                        LintCode::DependsOnAbandonedGoal,
                        task,
                        format!("it waits for goal {goal_id}, which was closed as abandoned"),
                    ));
                }
            }
        }
        content_rules(task, &mut found);
        violations.extend(found);
    }
    violations.extend(duplicate_titles(&input.targets));
    violations.sort_by_key(|v| {
        (
            input.targets.iter().position(|t| t.id() == v.task_id),
            v.code,
        )
    });
    violations
}

fn violation(code: LintCode, task: &Task, reason: String) -> LintViolation {
    LintViolation {
        code,
        task_id: task.id(),
        reason,
    }
}

fn predecessor_rules(
    task: &Task,
    node: &LintNode,
    members: &BTreeSet<TaskId>,
    input: &LintInput,
    found: &mut Vec<LintViolation>,
) {
    for predecessor in &node.depends_on {
        let Some(status) = input.nodes.get(predecessor).map(|p| p.status) else {
            continue;
        };
        let rule = match status {
            TaskStatus::Completed => Some((LintCode::DependsOnCompleted, "is already completed")),
            TaskStatus::Canceled => Some((
                LintCode::DependsOnCanceled,
                "was canceled and never completes",
            )),
            TaskStatus::Draft if !members.contains(predecessor) => Some((
                LintCode::DependsOnDraft,
                "is a draft that nothing submitted",
            )),
            _ => None,
        };
        if let Some((code, why)) = rule {
            found.push(violation(
                code,
                task,
                format!("it depends on task {predecessor}, which {why}"),
            ));
        }
    }
}

fn content_rules(task: &Task, found: &mut Vec<LintViolation>) {
    if task.verification_commands().is_empty() {
        found.push(if task.paths().is_empty() {
            violation(
                LintCode::UnscopedWithoutVerification,
                task,
                "it declares no paths, so it may change anything, and no verification command checks the change".into(),
            )
        } else {
            violation(
                LintCode::NoVerification,
                task,
                "no verification command checks the change within its paths".into(),
            )
        });
    }
    for glob in task.paths() {
        if let Err(error) = validate_path_globs(std::slice::from_ref(glob)) {
            found.push(violation(
                LintCode::InvalidPathGlob,
                task,
                error.to_string(),
            ));
        }
    }
    if task.acceptance().trim().is_empty() {
        found.push(violation(
            LintCode::BlankAcceptance,
            task,
            "its acceptance criteria are blank".into(),
        ));
    }
}

/// Each task whose title another target shares, naming the others.
fn duplicate_titles(targets: &[Task]) -> Vec<LintViolation> {
    let mut by_title: BTreeMap<String, Vec<TaskId>> = BTreeMap::new();
    for task in targets {
        by_title
            .entry(task.title().trim().to_lowercase())
            .or_default()
            .push(task.id());
    }
    targets
        .iter()
        .filter_map(|task| {
            let same = &by_title[&task.title().trim().to_lowercase()];
            let others: Vec<String> = same
                .iter()
                .filter(|id| **id != task.id())
                .map(ToString::to_string)
                .collect();
            (!others.is_empty()).then(|| {
                violation(
                    LintCode::DuplicateTitle,
                    task,
                    if others.len() == 1 {
                        format!("task {} has the same title", others[0])
                    } else {
                        format!("tasks {} have the same title", others.join(", "))
                    },
                )
            })
        })
        .collect()
}

/// Whether `start` reaches itself over the edges the queue refuses to
/// close into a loop when a dependency is added: a task waits for its
/// predecessors, and for every task of each goal it depends on, whatever
/// the statuses and verdicts.
fn in_cycle(
    start: TaskId,
    nodes: &BTreeMap<TaskId, LintNode>,
    goal_members: &BTreeMap<GoalId, Vec<TaskId>>,
) -> bool {
    let successors = |id: TaskId| -> Vec<TaskId> {
        let Some(node) = nodes.get(&id) else {
            return Vec::new();
        };
        node.depends_on
            .iter()
            .chain(
                node.goal_dependencies
                    .iter()
                    .flat_map(|goal_id| goal_members.get(goal_id).into_iter().flatten()),
            )
            .copied()
            .collect()
    };
    let mut seen = BTreeSet::new();
    let mut stack = successors(start);
    while let Some(id) = stack.pop() {
        if id == start {
            return true;
        }
        if seen.insert(id) {
            stack.extend(successors(id));
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EvidenceCheck, NewTask};

    fn task(id: i64, title: &str) -> Task {
        Task::new(
            TaskId::new(id),
            NewTask {
                title: title.into(),
                description: "d".into(),
                acceptance: "a".into(),
                verification_commands: vec!["cargo test".into()],
                dependencies: vec![],
                goal_dependencies: vec![],
                goal_id: None,
                context: String::new(),
                required_evidence: vec![EvidenceCheck::E2e],
                paths: vec![],
                priority: Default::default(),
            },
            "t".into(),
        )
        .unwrap()
    }

    fn node(status: TaskStatus, depends_on: &[i64]) -> LintNode {
        LintNode {
            status,
            goal_id: None,
            depends_on: depends_on.iter().copied().map(TaskId::new).collect(),
            goal_dependencies: vec![],
        }
    }

    /// Targets 1..=n (submitted, no dependencies) plus `others`.
    fn input(targets: Vec<Task>, others: Vec<(i64, LintNode)>) -> LintInput {
        let mut nodes: BTreeMap<TaskId, LintNode> = targets
            .iter()
            .map(|t| (t.id(), node(TaskStatus::Submitted, &[])))
            .collect();
        nodes.extend(others.into_iter().map(|(id, n)| (TaskId::new(id), n)));
        LintInput {
            targets,
            nodes,
            goals: BTreeMap::new(),
        }
    }

    fn codes(input: &LintInput) -> Vec<(i64, LintCode)> {
        lint(input)
            .into_iter()
            .map(|v| (v.task_id.as_i64(), v.code))
            .collect()
    }

    #[test]
    fn a_sound_plan_has_no_violations() {
        let plan = input(
            vec![task(1, "a"), task(2, "b")],
            vec![(3, node(TaskStatus::Ready, &[]))],
        );
        assert!(lint(&plan).is_empty());
        assert!(lint(&LintInput::default()).is_empty());
    }

    #[test]
    fn dependency_cycle_over_tasks_and_goals() {
        let mut plan = input(vec![task(1, "a"), task(2, "b")], vec![]);
        plan.nodes.get_mut(&TaskId::new(1)).unwrap().depends_on = vec![TaskId::new(3)];
        plan.nodes
            .insert(TaskId::new(3), node(TaskStatus::Ready, &[1]));
        assert_eq!(codes(&plan), [(1, LintCode::DependencyCycle)]);
        let violation = &lint(&plan)[0];
        assert_eq!(
            violation.reason,
            "it depends on itself through its dependencies"
        );

        // Through a goal: 2 waits for goal 7, whose task 4 waits for 2.
        let mut plan = input(vec![task(2, "b")], vec![]);
        plan.nodes
            .get_mut(&TaskId::new(2))
            .unwrap()
            .goal_dependencies = vec![GoalId::new(7)];
        let mut member = node(TaskStatus::Draft, &[2]);
        member.goal_id = Some(GoalId::new(7));
        plan.nodes.insert(TaskId::new(4), member);
        plan.goals.insert(GoalId::new(7), None);
        assert_eq!(codes(&plan), [(2, LintCode::DependencyCycle)]);
        // Like the queue's own check, a goal's verdict does not cut the loop.
        plan.goals
            .insert(GoalId::new(7), Some(GoalVerdict::Achieved));
        assert_eq!(codes(&plan), [(2, LintCode::DependencyCycle)]);

        // A task that leads into a loop without being on it is not reported.
        let mut plan = input(vec![task(1, "a")], vec![]);
        plan.nodes.get_mut(&TaskId::new(1)).unwrap().depends_on = vec![TaskId::new(5)];
        plan.nodes
            .insert(TaskId::new(5), node(TaskStatus::Ready, &[6]));
        plan.nodes
            .insert(TaskId::new(6), node(TaskStatus::Ready, &[5]));
        assert!(codes(&plan).is_empty());
    }

    #[test]
    fn dependencies_on_completed_canceled_and_outside_drafts() {
        let mut plan = input(vec![task(1, "a"), task(2, "b")], vec![]);
        plan.nodes.get_mut(&TaskId::new(1)).unwrap().depends_on =
            [5, 6, 7, 8, 9].map(TaskId::new).to_vec();
        plan.nodes
            .insert(TaskId::new(5), node(TaskStatus::Completed, &[]));
        plan.nodes
            .insert(TaskId::new(6), node(TaskStatus::Canceled, &[]));
        plan.nodes
            .insert(TaskId::new(7), node(TaskStatus::Draft, &[]));
        plan.nodes
            .insert(TaskId::new(8), node(TaskStatus::Ready, &[]));
        plan.nodes
            .insert(TaskId::new(9), node(TaskStatus::InProgress, &[]));
        // A draft inside the linted set is submitted together: no violation.
        plan.nodes
            .insert(TaskId::new(2), node(TaskStatus::Draft, &[]));
        plan.nodes
            .get_mut(&TaskId::new(1))
            .unwrap()
            .depends_on
            .push(TaskId::new(2));
        let found = lint(&plan);
        assert_eq!(
            found
                .iter()
                .map(|v| (v.code, v.reason.as_str()))
                .collect::<Vec<_>>(),
            [
                (
                    LintCode::DependsOnCompleted,
                    "it depends on task 5, which is already completed"
                ),
                (
                    LintCode::DependsOnCanceled,
                    "it depends on task 6, which was canceled and never completes"
                ),
                (
                    LintCode::DependsOnDraft,
                    "it depends on task 7, which is a draft that nothing submitted"
                ),
            ]
        );
    }

    #[test]
    fn same_code_violations_keep_the_order_of_the_dependencies() {
        let mut plan = input(vec![task(1, "a")], vec![]);
        plan.nodes.get_mut(&TaskId::new(1)).unwrap().depends_on =
            vec![TaskId::new(4), TaskId::new(3)];
        plan.nodes
            .insert(TaskId::new(3), node(TaskStatus::Completed, &[]));
        plan.nodes
            .insert(TaskId::new(4), node(TaskStatus::Completed, &[]));
        let reasons: Vec<String> = lint(&plan).into_iter().map(|v| v.reason).collect();
        assert_eq!(
            reasons,
            [
                "it depends on task 4, which is already completed",
                "it depends on task 3, which is already completed"
            ]
        );
    }

    #[test]
    fn dependency_on_an_abandoned_goal() {
        let mut plan = input(vec![task(1, "a")], vec![]);
        plan.nodes
            .get_mut(&TaskId::new(1))
            .unwrap()
            .goal_dependencies = vec![GoalId::new(3), GoalId::new(4), GoalId::new(5)];
        plan.goals
            .insert(GoalId::new(3), Some(GoalVerdict::Abandoned));
        plan.goals
            .insert(GoalId::new(4), Some(GoalVerdict::Achieved));
        plan.goals.insert(GoalId::new(5), None);
        let found = lint(&plan);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].code, LintCode::DependsOnAbandonedGoal);
        assert_eq!(
            found[0].reason,
            "it waits for goal 3, which was closed as abandoned"
        );
    }

    fn with(task: Task, edit: impl FnOnce(&mut crate::domain::TaskRecord)) -> Task {
        let mut record = crate::domain::TaskRecord {
            id: task.id(),
            title: task.title().into(),
            description: task.description().into(),
            acceptance: task.acceptance().into(),
            verification_commands: task.verification_commands().to_vec(),
            required_evidence: task.required_evidence().to_vec(),
            paths: task.paths().to_vec(),
            priority: task.priority(),
            status: task.status(),
            goal_id: task.goal_id(),
            context: task.context().into(),
            created_at: task.created_at().into(),
            updated_at: task.updated_at().into(),
        };
        edit(&mut record);
        Task::restore(record).unwrap()
    }

    #[test]
    fn verification_missing_with_and_without_paths() {
        let unscoped = with(task(1, "a"), |r| r.verification_commands.clear());
        let scoped = with(task(2, "b"), |r| {
            r.verification_commands.clear();
            r.paths = vec!["docs/**".into()];
        });
        assert_eq!(
            codes(&input(vec![unscoped, scoped], vec![])),
            [
                (1, LintCode::UnscopedWithoutVerification),
                (2, LintCode::NoVerification)
            ]
        );
    }

    #[test]
    fn invalid_path_glob() {
        let bad = with(task(1, "a"), |r| {
            r.paths = vec!["docs/**".into(), "../x".into(), "/abs".into()]
        });
        let found = lint(&input(vec![bad], vec![]));
        assert_eq!(
            found.iter().map(|v| v.code).collect::<Vec<_>>(),
            [LintCode::InvalidPathGlob, LintCode::InvalidPathGlob]
        );
        assert!(found[0].reason.contains("../x"), "{}", found[0].reason);
        assert!(found[1].reason.contains("/abs"), "{}", found[1].reason);
    }

    #[test]
    fn blank_acceptance() {
        let blank = with(task(1, "a"), |r| r.acceptance = "  \n".into());
        assert_eq!(
            codes(&input(vec![blank], vec![])),
            [(1, LintCode::BlankAcceptance)]
        );
    }

    #[test]
    fn duplicate_titles_inside_the_set() {
        let plan = input(
            vec![
                task(1, "Add lint"),
                task(2, "other"),
                task(3, " add LINT "),
                task(4, "add lint"),
            ],
            vec![],
        );
        let found = lint(&plan);
        assert_eq!(
            found
                .iter()
                .map(|v| (v.task_id.as_i64(), v.code, v.reason.as_str()))
                .collect::<Vec<_>>(),
            [
                (
                    1,
                    LintCode::DuplicateTitle,
                    "tasks 3, 4 have the same title"
                ),
                (
                    3,
                    LintCode::DuplicateTitle,
                    "tasks 1, 4 have the same title"
                ),
                (
                    4,
                    LintCode::DuplicateTitle,
                    "tasks 1, 3 have the same title"
                ),
            ]
        );
    }

    #[test]
    fn violations_come_per_target_in_code_order_and_serialize_as_json() {
        let first = with(task(2, "same"), |r| r.acceptance.clear());
        let second = task(1, "same");
        let found = lint(&input(vec![first, second], vec![]));
        assert_eq!(
            found
                .iter()
                .map(|v| (v.task_id.as_i64(), v.code))
                .collect::<Vec<_>>(),
            [
                (2, LintCode::BlankAcceptance),
                (2, LintCode::DuplicateTitle),
                (1, LintCode::DuplicateTitle)
            ]
        );
        assert_eq!(
            serde_json::to_value(&found[0]).unwrap(),
            serde_json::json!({
                "code": "blank_acceptance",
                "task_id": 2,
                "reason": "its acceptance criteria are blank"
            })
        );
    }
}
