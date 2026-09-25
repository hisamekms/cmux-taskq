//! The worker's prompt (`prompt.txt`): the task, its goal, the summaries
//! of its landed predecessors and of the goals it waited for, the tasks running alongside it and what the
//! receipt must hold. Built from what the queue returned at claim time.
//! Also the initial prompts of the inbox session `up` opens and of the
//! planner sessions a person or the runtime opens, and what the supervisor asks of an agent: the headless review and
//! triage, and the requests it types into a live session (a resume, a
//! revise, a receipt that does not match).

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

use super::{
    RunFiles, TaskListItem, fenced,
    integrate::{integrate_logs, log_names},
    or_none, tail,
};
use crate::domain::{
    Ask, CommitSha, DraftOrigin, DraftTarget, Goal, GoalId, GoalPredecessor, GoalTask,
    LintViolation, MAX_DRAFT_PLANNERS, MAX_PLAN_REVISES, MAX_RESUME_ATTEMPTS, MAX_REVISE_ATTEMPTS,
    Predecessor, Proposal, ProposalId, Receipt, RunStatus, TRIAGE_RETRY_FAILURES, Task, TaskDetail,
    TaskId, TaskRun,
};

/// What the prompt says about one direct predecessor: the task, the squash
/// commit `integrate` put on `main` for it, and the summary its agent wrote.
#[derive(Debug, Clone, Serialize)]
pub struct PredecessorSummary {
    pub task_id: TaskId,
    pub title: String,
    /// `result_commit` of the integrated run; `(not landed)` without one.
    pub result_commit: String,
    /// `summary` of the integrated run's receipt, whitespace collapsed;
    /// `(receipt unavailable)` when the receipt cannot be read or parsed.
    pub summary: String,
}

impl PredecessorSummary {
    /// The receipt is read where the run left it after landing (its planned
    /// `receipt_path`, else `<run_dir>/receipt.json`); a missing or
    /// unreadable one is described, never an error, so the successor still starts.
    pub fn from_predecessor(files: &dyn RunFiles, predecessor: &Predecessor) -> Self {
        let run = predecessor.integrated_run.as_ref();
        let summary = run
            .and_then(|run| {
                run.receipt_path()
                    .map(PathBuf::from)
                    .or_else(|| run.run_dir().map(|dir| Path::new(dir).join("receipt.json")))
            })
            .and_then(|path| files.read_to_string(&path).ok())
            .and_then(|text| Receipt::parse(&text).ok())
            .map(|receipt| {
                receipt
                    .summary
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .map(|summary| {
                if summary.is_empty() {
                    "(no summary)".to_owned()
                } else {
                    summary
                }
            })
            .unwrap_or_else(|| "(receipt unavailable)".to_owned());
        Self {
            task_id: predecessor.task.id(),
            title: predecessor.task.title().to_owned(),
            result_commit: run
                .and_then(|run| run.result_commit())
                .map_or_else(|| "(not landed)".to_owned(), CommitSha::to_string),
            summary,
        }
    }
}

/// Characters of a receipt summary the prompt keeps for each task of a goal
/// the task depended on: a goal may hold many tasks, so each is a hint of
/// what landed, not the whole account.
pub const GOAL_TASK_SUMMARY_CHARS: usize = 200;

/// What the prompt says about one goal the task depended on (ADR-0038):
/// its title and its completed tasks, each summarized like a predecessor
/// with the summary cut to [`GOAL_TASK_SUMMARY_CHARS`].
#[derive(Debug, Clone, Serialize)]
pub struct GoalPredecessorSummary {
    pub goal_id: GoalId,
    pub title: String,
    pub tasks: Vec<PredecessorSummary>,
}

impl GoalPredecessorSummary {
    pub fn from_goal_predecessor(files: &dyn RunFiles, predecessor: &GoalPredecessor) -> Self {
        Self {
            goal_id: predecessor.goal.id(),
            title: predecessor.goal.title().to_owned(),
            tasks: predecessor
                .tasks
                .iter()
                .map(|task| {
                    let mut summary = PredecessorSummary::from_predecessor(files, task);
                    if let Some(cut) =
                        super::health::truncate(&summary.summary, GOAL_TASK_SUMMARY_CHARS)
                    {
                        summary.summary = cut;
                    }
                    summary
                })
                .collect(),
        }
    }
}

/// The other tasks a worker is told are executing alongside it: of the
/// `in_progress` tasks (ID order), those sharing the task's goal, or all of
/// them when the task has no goal; the task itself is never listed.
pub fn siblings_in_progress(task: &Task, in_progress: Vec<Task>) -> Vec<Task> {
    in_progress
        .into_iter()
        .filter(|other| other.id() != task.id())
        .filter(|other| task.goal_id().is_none() || other.goal_id() == task.goal_id())
        .collect()
}

/// The line in the worker prompt and the resume request that asks the
/// session to stop its own background work before the receipt: a leftover
/// background shell makes Claude Code answer the supervisor's `/exit` with a
/// confirmation screen, and the exit request times out.
pub const STOP_BACKGROUND: &str = "Before writing the receipt, stop every background process you started (run_in_background shells, wait loops, watches); if any is left, /exit stops at a confirmation screen.";

/// What a worker reads before it starts, and nothing more: everything else
/// about its run is in the prompt, and reading the queue or the whole docs
/// tree only delays the first commit (goal 11, decision 4).
pub const WORKER_READING: &str = "Read first, and only: the worker section of the repository instructions (AGENTS.md), the task context below and the documents it names, the goal doc if there is one, and the predecessor summaries below. \
Do not run `dagq list` or `dagq show`, and skip the rest of the docs tree; open other files only when the task needs them.\n";

/// Text of `prompt.txt`. `goal` is the task's goal as it reads at claim
/// time, `predecessors` the task's direct dependencies, `goal_predecessors`
/// the goals it depends on (in the Predecessor section) and `siblings` the
/// other tasks executing at claim time (`siblings_in_progress`). The Goal,
/// Context, Predecessor and Sibling sections are always present, `none`
/// when empty, so the prompt keeps one shape whether or not a task has a
/// goal, a context, dependencies or company.
pub fn prompt(
    task: &Task,
    run: &TaskRun,
    goal: Option<&Goal>,
    predecessors: &[PredecessorSummary],
    goal_predecessors: &[GoalPredecessorSummary],
    siblings: &[Task],
) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let goal = match goal {
        None => "Goal: none, this task stands alone\n".to_owned(),
        Some(goal) => format!(
            "Goal (the higher-level problem this task and its sibling tasks solve together):\n\
             Goal ID: {id}\nGoal title: {title}\nGoal description:\n{description}\n\
             Goal acceptance:\n{acceptance}\nGoal constraints:\n{constraints}\n\
             Goal doc: {doc}\n",
            id = goal.id(),
            title = goal.title(),
            description = goal.description(),
            acceptance = goal.acceptance(),
            constraints = goal.constraints(),
            doc = goal
                .doc()
                .map(|doc| format!(
                    "{doc} (a path in the repository; read it for the full picture)"
                ))
                .unwrap_or_else(|| "none".to_owned()),
        ),
    };
    let context = if task.context().trim().is_empty() {
        "Context: none\n".to_owned()
    } else {
        format!(
            "Context (why this task exists and what to read first):\n{}\n",
            task.context()
        )
    };
    let predecessors = if predecessors.is_empty() && goal_predecessors.is_empty() {
        "Predecessor tasks: none\n".to_owned()
    } else {
        let mut text =
            "Predecessor tasks (their changes are already in your base commit):\n".to_owned();
        for predecessor in predecessors {
            text.push_str(&format!(
                "- task {}: {}; result commit {}; summary: {}\n",
                predecessor.task_id,
                predecessor.title,
                predecessor.result_commit,
                predecessor.summary
            ));
        }
        for goal in goal_predecessors {
            text.push_str(&format!(
                "- goal {} (closed as achieved): {}; its completed tasks:\n",
                goal.goal_id, goal.title
            ));
            if goal.tasks.is_empty() {
                text.push_str("  - none\n");
            }
            for task in &goal.tasks {
                text.push_str(&format!(
                    "  - task {}: {}; result commit {}; summary: {}\n",
                    task.task_id, task.title, task.result_commit, task.summary
                ));
            }
        }
        text
    };
    let siblings = if siblings.is_empty() {
        "Sibling tasks in progress: none\n".to_owned()
    } else {
        let mut text =
            "Sibling tasks in progress (other tasks executing now, each owning its own scope):\n"
                .to_owned();
        for other in siblings {
            text.push_str(&format!("- task {}: {}\n", other.id(), other.title()));
        }
        text
    };
    // Known up front, so the receipt carries it (ADR-0019 decision 5).
    let evidence = if task.required_evidence().is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = task
            .required_evidence()
            .iter()
            .map(|c| c.as_str())
            .collect();
        format!(
            "Required evidence: {} (each must be passed with evidence in the receipt, or the run waits for a session to add it)\n",
            names.join(", ")
        )
    };
    // The declared scope (ADR-0029): changing anything else parks the run.
    let paths = if task.paths().is_empty() {
        String::new()
    } else {
        format!(
            "Paths you may change (globs from the repository root; `*` stays in one directory, `**` spans any depth): {}. A commit that changes any other path is not accepted: the run waits for a session to take it out. If the task needs another path, ask instead of changing it.\n",
            task.paths().join(", ")
        )
    };
    Ok(format!(
        "You are executing dagq task {task_id}, run {run_id}.\n\
         Work only in the assigned Git worktree.\n\
         {reading}\
         Implement the task, run the required verification commands, and commit the result.\n\
         Do not merge, push, close the workspace, or modify the queue/runtime files.\n\
         Perform applicable unit tests, E2E, and subagent review. Record evidence or an explicit reason when not applicable.\n\
         Task title: {title}\nDescription:\n{description}\nAcceptance criteria:\n{acceptance}\n\
         Verification commands (run in the worktree):\n{verification}\n\
         {evidence}{paths}{goal}{context}{predecessors}{siblings}\
         Your assignment is this task only. Do not change what a sibling task owns; if you find work outside this task, record it in the receipt as follow_ups instead of doing it.\n\
         Write a completion receipt to {receipt} using a temporary file in the same directory and atomic rename.\n\
         Receipt JSON: {{\"run_id\":\"{run_id}\",\"result\":\"succeeded or failed\",\"commit\":\"full Git SHA of the branch head\",\"tests\":{{\"status\":\"passed, failed or not_applicable\",\"evidence_or_reason\":\"...\"}},\"e2e\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"subagent_review\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"summary\":\"...\",\"follow_ups\":[{{\"title\":\"...\",\"description\":\"...\"}}]}}\n\
         Each of tests, e2e and subagent_review needs evidence when passed and a reason when not_applicable.\n\
         follow_ups is optional: an array of work you found outside this task, each with a title and a description, for the planner to decide on; omit it when there is none.\n\
         You may write this receipt outside the worktree. Keep the worktree clean after committing.\n\
         The supervisor rejects the run unless the commit is the clean head of your branch on top of the base commit, and integrate reruns the verification commands itself after rebasing onto main.\n\
         When you need a decision you cannot make from the task and the repository, do not write the question to the terminal and wait: run `dagq ask --run {run_id} --kind worker_question --question '...'` in the worktree (one ask at a time, with everything you need decided in its question), report briefly that you asked, and stop. The answer arrives in this terminal as `answer to ask <id>: ...`; continue from it.\n\
         {stop_background}\n\
         After submitting, report the outcome briefly and stop; do not run /exit yourself. Once you are idle the supervisor ends the session, and a person can still send /exit. A receipt does not itself end the session.\n",
        task_id = task.id(),
        run_id = run.id(),
        reading = WORKER_READING,
        stop_background = STOP_BACKGROUND,
        title = task.title(),
        description = task.description(),
        acceptance = task.acceptance(),
        verification = serde_json::to_string_pretty(&task.verification_commands())?,
    ))
}

/// The initial prompt of the inbox session that `up` opens in the
/// `[<repo>]inbox` workspace (ADR-0022): it relays each open ask to a person
/// and writes the person's answer back, deciding nothing itself. Every
/// other attention is the inbox's too (ADR-0024 decision 6): it reports it
/// and does only what the person says.
pub fn inbox_prompt(db: &Path) -> Result<String> {
    Ok(format!(
        "You are the inbox of the dagq queue at {db}: you relay its asks and attention to a person and never decide anything yourself.\n\
         Start with `dagq status --role inbox` and follow the dagq-inbox skill of the dagq plugin: run `dagq watch --role inbox --after <cursor>` in the background, wake when it returns and watch again from the cursor it returns.\n\
         On ask_opened, read the ask with `dagq asks --open --role inbox`, show the person its question and options (use AskUserQuestion when it is available), then write the person's answer with `dagq answer ID --text '<answer>'`. Report any other attention (an answered ask, a stopped supervisor, a failed review or triage) to the person and do only what they say, as the skill describes.\n\
         Never open the queue database directly; use the dagq CLI only.\n",
        db = super::path_text(db)?,
    ))
}

/// The initial prompt of a planner session a person opens with `dagq plan`
/// (ADR-0041 decisions 1, 6): it turns the person's problems into goals and
/// tasks, hands them over as the skill says (a proposal for plan review),
/// and closes a goal once its tasks meet the acceptance.
pub fn planner_prompt(db: &Path) -> Result<String> {
    Ok(format!(
        "You are a planner of the dagq queue at {db}: listen to the person's problems and turn them into goals and tasks.\n\
         Follow the dagq-planner skill of the dagq plugin: register them and submit them for plan review as its dagq skill describes. You do not land runs or answer asks.\n\
         When every task of a goal is completed, check their receipts against the goal's acceptance and close the goal (`dagq goal close ID --verdict achieved`).\n\
         Never open the queue database directly; use the dagq CLI only.\n",
        db = super::path_text(db)?,
    ))
}

/// The initial prompt of a planner the runtime opens for a proposal plan
/// review sent back while its own planner was closed (ADR-0041 decision
/// 12): the proposal, its tasks, and the reasons to fix. No person watches
/// the session, so what needs one goes to the inbox as an ask (decision 13).
pub fn runtime_planner_prompt(
    db: &Path,
    proposal: ProposalId,
    tasks: &[Task],
    reasons: &[String],
) -> Result<String> {
    let tasks = if tasks.is_empty() {
        "(none)".to_owned()
    } else {
        tasks
            .iter()
            .map(|task| {
                format!(
                    "- task {} ({}): {}",
                    task.id(),
                    task.status().as_str(),
                    task.title()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let reasons = if reasons.is_empty() {
        "(none given)".to_owned()
    } else {
        reasons
            .iter()
            .map(|reason| format!("- {reason}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(format!(
        "You are a planner the dagq runtime opened for proposal {proposal} of the queue at {db}; no person watches this session.\n\
         Plan review sent the proposal back. Its reasons:\n{reasons}\n\
         Its tasks:\n{tasks}\n\
         Follow the dagq-planner skill of the dagq plugin: read the proposal with `dagq proposal show {proposal}` and each task with `dagq show ID`, fix what the reasons point at, and submit it again with `dagq submit --proposal {proposal}`.\n\
         A fix that changes the plan's intent (acceptance, scope, the relation to the goal) needs a person: raise it to the inbox with `dagq ask --task ID --kind planner_question` as the skill describes, stop, and continue from the answer typed into this terminal.\n\
         Never open the queue database directly; use the dagq CLI only.\n",
        db = super::path_text(db)?,
    ))
}

/// What the initial prompt of a planner the runtime opens for a draft
/// (ADR-0041 decision 16) shows it: the draft and where it came from, the
/// source task and its landed receipt for a follow_up, the goal and its
/// other tasks, and the answer of its `planner_question` it carries when
/// the planner that asked is gone.
pub struct DraftPlannerMaterial<'a> {
    pub db: &'a Path,
    pub target: &'a DraftTarget,
    /// Which planner of the runtime's this is for the draft (1-based).
    pub attempt: usize,
    /// The task whose run's receipt proposed a follow_up.
    pub source: Option<&'a Task>,
    /// That run's landed receipt.
    pub receipt: Option<&'a Value>,
    pub goal: Option<&'a Goal>,
    pub goal_closed: bool,
    /// The goal's other tasks.
    pub siblings: &'a [GoalTask],
    pub answer: Option<&'a Ask>,
}

/// The initial prompt of a planner the runtime opens for a draft the
/// runtime or a job registered (ADR-0041 decision 16): the material, and
/// the three things it may do with the draft — submit it completed
/// (adopt), cancel it with a note (drop), or ask the inbox a
/// `planner_question` and apply the answer typed into its terminal.
pub fn draft_planner_prompt(material: &DraftPlannerMaterial<'_>) -> Result<String> {
    let target = material.target;
    let task = &target.task;
    let id = task.id();
    let mut out = format!(
        "You are a planner the dagq runtime opened for draft task {id} of the queue at {db}; no person watches this session. The {origin} draft is not ready: the runtime or a job registered it, and you decide what becomes of it (planner {attempt} of at most {max} the runtime opens for it).\n",
        db = super::path_text(material.db)?,
        origin = target.origin.as_str(),
        attempt = material.attempt,
        max = MAX_DRAFT_PLANNERS,
    );
    out.push_str(&format!(
        "\n## The draft\n\nTask {id}: {title}\n\n### Description\n\n{description}\n\n### Context\n\n{context}\n",
        title = task.title(),
        description = or_none(task.description()),
        context = or_none(task.context()),
    ));
    out.push_str(&format!(
        "\n## Where it came from: {}\n\n",
        target.origin.as_str()
    ));
    match target.origin {
        DraftOrigin::FollowUp => {
            out.push_str(&format!(
                "The receipt of run {run} of task {source} proposed it as a follow_up: work its worker found outside that task.\n",
                run = target.material["source_run_id"].as_str().unwrap_or("(unknown)"),
                source = target.material["source_task_id"],
            ));
            if let Some(source) = material.source {
                out.push_str(&format!(
                    "\n### Source task {sid}: {title} ({status})\n\n{description}\n\nAcceptance:\n{acceptance}\n\nVerification: {verify}\nPaths: {paths}\nEvidence: {evidence}\n",
                    sid = source.id(),
                    title = source.title(),
                    status = source.status().as_str(),
                    description = or_none(source.description()),
                    acceptance = or_none(source.acceptance()),
                    verify = list_or_none(source.verification_commands()),
                    paths = list_or_none(source.paths()),
                    evidence = list_or_none(
                        &source
                            .required_evidence()
                            .iter()
                            .map(|check| check.as_str().to_owned())
                            .collect::<Vec<_>>()
                    ),
                ));
            }
            if let Some(receipt) = material.receipt {
                out.push_str(&format!(
                    "\n### The landed receipt\n\nSummary:\n{summary}\n\nIts follow_ups:\n{follow_ups}",
                    summary = or_none(receipt["summary"].as_str().unwrap_or_default()),
                    follow_ups = fenced(
                        "json",
                        &serde_json::to_string_pretty(&receipt["follow_ups"])?
                    ),
                ));
            }
        }
        DraftOrigin::GoalGap => {
            out.push_str(
                "A job that judged the goal below against its acceptance found this gap. Its findings:\n",
            );
            out.push_str(&fenced(
                "json",
                &serde_json::to_string_pretty(&target.material)?,
            ));
        }
    }
    match material.goal {
        Some(goal) => {
            out.push_str(&format!(
                "\n## Goal {gid}: {title}{closed}\n\n{description}\n\nAcceptance:\n{acceptance}\n\nConstraints:\n{constraints}\n\nDoc: {doc}\n\nIts other tasks:\n{tasks}\n",
                gid = goal.id(),
                title = goal.title(),
                closed = if material.goal_closed { " (closed)" } else { "" },
                description = or_none(goal.description()),
                acceptance = or_none(goal.acceptance()),
                constraints = or_none(goal.constraints()),
                doc = goal.doc().unwrap_or("(none)"),
                tasks = if material.siblings.is_empty() {
                    "(none)".to_owned()
                } else {
                    material
                        .siblings
                        .iter()
                        .map(|t| format!("- task {} ({}): {}", t.id, t.status.as_str(), t.title))
                        .collect::<Vec<_>>()
                        .join("\n")
                },
            ));
        }
        None => out.push_str(
            "\n## Goal\n\nThe draft belongs to no goal (the source's goal was closed, or it had none).\n",
        ),
    }
    out.push_str(&format!(
        "\n## What to do\n\n\
         Follow the dagq-planner skill of the dagq plugin. Read the repository's AGENTS.md (or CLAUDE.md) for its rules on verification, paths, evidence and ADR numbers. Look for tasks that already cover the draft or code that already does it (`dagq search '<words>'`, `dagq show ID`, the source) before you decide. Then do exactly one of these three:\n\
         1. Adopt: complete the draft with `dagq edit {id}` (acceptance, `--verify`, `--paths`, `--evidence`, and `--context` beginning with `{context_head}`), add its dependencies with `dagq dependency add`, check it with `dagq lint {id}` and submit it with `dagq submit {id}`. Plan review checks it before it becomes ready.\n\
         2. Drop: when it is already done, duplicated or not worth doing, cancel it with `dagq cancel {id}` and record why with `dagq note --task {id} --text '<why>'`.\n\
         3. Ask: when you cannot decide without a person (the plan's intent, its scope, whether it belongs to this goal or a new one), run `dagq ask --task {id} --kind planner_question --question '<everything the person needs, with your recommendation>' --option adopt --option cancel --option keep_draft`, report briefly and stop. The answer arrives in this terminal as `answer to ask <id>: ...`: on adopt do 1, on cancel do 2 (the note names the ask), on keep_draft leave the draft as it is and stop.\n\
         The runtime refuses your submit of a follow_up draft whose goal is closed or that is two follow-ups from a person's judgement unless a person answered adopt: ask then.\n\
         When you are done, report the outcome in one or two sentences and stop; the runtime ends this session. Do not work on anything but this draft. Never open the queue database directly; use the dagq CLI only.\n",
        context_head = match target.origin {
            DraftOrigin::FollowUp => format!(
                "follow-up draft（task {} の run {} の receipt が提案）",
                target.material["source_task_id"],
                target.material["source_run_id"].as_str().unwrap_or("?"),
            ),
            DraftOrigin::GoalGap => format!(
                "goal gap draft（goal {} の判断が提案）",
                task.goal_id().map_or("?".to_owned(), |g| g.to_string())
            ),
        },
    ));
    if let Some(answer) = material.answer {
        out.push_str(&format!(
            "\nThe planner before you asked a person (ask {aid}) and is gone:\n{question}\n\nanswer to ask {aid}: {text}\n\nApply this answer as step 3 says.\n",
            aid = answer.id,
            question = answer.question,
            text = answer.answer.as_deref().unwrap_or_default(),
        ));
    }
    Ok(out)
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_owned()
    } else {
        items.join(", ")
    }
}

/// What the resolution request tells a resumed session.
pub(crate) struct ResumeRequest {
    /// The `main` head the session rebases onto.
    pub main: CommitSha,
    pub reason: String,
    pub kind: ResumeKind,
}

/// Why the run waits for a session, which decides the request's steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeKind {
    /// A landing was deferred (a conflict, failed verification): rebase.
    Landing,
    /// Validation's `evidence_missing`: add the evidence instead.
    EvidenceMissing,
    /// A person sent a review's concern back (`landing_decided`): fix
    /// the findings.
    SentBack,
    /// The diff changes paths outside the task's `paths` (validation's
    /// `scope_violation`, or a landing deferred for it): take them out.
    ScopeViolation,
    /// A passed run's live session, before its `/exit`: the precheck found
    /// that it conflicts with main (ADR-0027 decision 4). Rebase, like
    /// `Landing`.
    Precheck,
    /// The triage of a `failed` / `interrupted` run sent it back to its
    /// session (`triage_finished` with action `resume`, or a person's
    /// `resume` answer, `triage_decided`): do what the reason asks.
    Triage,
}

/// The fixed resolution request the supervisor types into a resumed
/// session (ADR-0019 decision 1), or into a passed run's live session whose
/// head conflicts with main (ADR-0027 decision 4), one instruction per
/// line; the backend sends it as one line.
pub(crate) fn resume_request(
    task: &Task,
    run: &TaskRun,
    request: &ResumeRequest,
    landed: &[PredecessorSummary],
) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let mut lines = vec![match request.kind {
        ResumeKind::EvidenceMissing => format!(
            "dagq: the supervisor's validation of run {} (task {}) found required evidence missing from the receipt, so the run is needs_session.",
            run.id(),
            task.id()
        ),
        ResumeKind::SentBack => format!(
            "dagq: the supervisor's review of run {} (task {}) raised findings a person sent back to you, so the run is needs_session.",
            run.id(),
            task.id()
        ),
        ResumeKind::ScopeViolation => format!(
            "dagq: run {} (task {}) changes paths outside the task's --paths ({}), so the run is needs_session.",
            run.id(),
            task.id(),
            task.paths().join(", ")
        ),
        ResumeKind::Landing => format!(
            "dagq: integrate could not land run {} (task {}) and returned needs_session.",
            run.id(),
            task.id()
        ),
        ResumeKind::Precheck => format!(
            "dagq: the supervisor's review of run {} (task {}) passed, but integrate would conflict with main, so the run was not landed.",
            run.id(),
            task.id()
        ),
        ResumeKind::Triage => format!(
            "dagq: run {} (task {}) failed or was interrupted, and the supervisor's triage sent it back to this session to finish, so the run is needs_session.",
            run.id(),
            task.id()
        ),
    }];
    lines.push(format!("Reason: {}", request.reason));
    lines.push(format!(
        "main is now {} (your base commit was {}).",
        request.main,
        run.base_commit()
    ));
    if landed.is_empty() {
        lines.push("Tasks landed on main since your base: none.".to_owned());
    } else {
        lines.push("Tasks landed on main since your base:".to_owned());
        for task in landed {
            lines.push(format!(
                "- task {}: {}; summary: {}",
                task.task_id, task.title, task.summary
            ));
        }
    }
    lines.push("Steps:".to_owned());
    let verify = serde_json::to_string(task.verification_commands())?;
    if request.kind == ResumeKind::EvidenceMissing {
        lines.push(
            "1. Run the checks the reason names as missing and write their evidence into the receipt."
                .to_owned(),
        );
        lines.push(format!(
            "2. If that changes files, commit them and rerun the verification commands {verify}."
        ));
    } else if request.kind == ResumeKind::ScopeViolation {
        lines.push(format!(
            "1. Take the changes to the paths the reason names out of the run branch: restore each to its state at git merge-base HEAD {} (delete the ones that did not exist there) and commit; if the task cannot be done without them, write the receipt with result failed and say which paths it needs.",
            request.main
        ));
        lines.push(format!("2. Rerun the verification commands {verify}."));
    } else if request.kind == ResumeKind::SentBack {
        lines.push(format!(
            "1. Fix the findings in the reason and commit; if main moved, git rebase {} first.",
            request.main
        ));
        lines.push(format!("2. Rerun the verification commands {verify}."));
    } else if request.kind == ResumeKind::Triage {
        lines.push(format!(
            "1. Do what the reason asks in this worktree and commit; if main moved, git rebase {} first.",
            request.main
        ));
        lines.push(format!("2. Rerun the verification commands {verify}."));
    } else {
        lines.push(format!(
            "1. In this worktree run git rebase {} and resolve the conflicts.",
            request.main
        ));
        lines.push(format!(
            "2. Rerun the verification commands {verify} and commit the result."
        ));
    }
    lines.push("3. Keep the worktree clean.".to_owned());
    lines.push(format!("4. {STOP_BACKGROUND}"));
    lines.push(format!(
        "5. Rewrite the receipt at {receipt} with the new head commit, writing a temporary file in the same directory and renaming it."
    ));
    lines.push(
        "6. If the change is no longer needed, write the receipt with result failed and the reason in summary."
            .to_owned(),
    );
    lines.push(
        "7. Do not merge or push. When done, report briefly and stop; do not run /exit.".to_owned(),
    );
    Ok(lines.join("\n"))
}

/// The fixed request the supervisor types into the live session when the
/// receipt it rewrote for a revise or a conflict request does not name its clean worktree HEAD.
pub(crate) fn revise_mismatch_request(run: &TaskRun, label: &str, why: &str) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    Ok([
        format!(
            "dagq: the receipt you rewrote for {label} of run {} cannot be accepted: {why}.",
            run.id()
        ),
        "Steps:".to_owned(),
        "1. Commit every change you meant to make, so the worktree is clean.".to_owned(),
        format!(
            "2. Rewrite the receipt at {receipt} with the current HEAD commit (git rev-parse HEAD), writing a temporary file in the same directory and renaming it."
        ),
        format!("3. {STOP_BACKGROUND}"),
        "4. Do not merge or push. When done, report briefly and stop; do not run /exit."
            .to_owned(),
    ]
    .join("\n"))
}

/// The one nudge the supervisor types into a worker's session that stayed
/// idle without a receipt for `idle_secs` (ADR-0043 decision 1): commit and
/// write the receipt, ask with `dagq ask`, or say what background work it
/// waits for. `background` names the tasks its idle marker lists as running.
pub(crate) fn stall_nudge(
    run: &TaskRun,
    idle_secs: i64,
    background: &[crate::domain::stall::BackgroundTask],
) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let minutes = idle_secs / 60;
    let mut lines = vec![format!(
        "dagq: run {} has been idle for {minutes} minutes without a receipt.",
        run.id()
    )];
    if background.is_empty() {
        lines.push("No background task was running when you stopped.".to_owned());
    } else {
        lines.push("Background tasks still running when you stopped:".to_owned());
        for task in background {
            lines.push(format!("- {}: {}", task.description, task.command));
        }
    }
    lines.push("Do one of these now:".to_owned());
    lines.push(format!(
        "1. If the work is done, commit it and write the receipt at {receipt} (a temporary file in the same directory, then rename)."
    ));
    lines.push(format!(
        "2. If you need a decision, run `dagq ask --run {} --kind worker_question --question '...'` and stop.",
        run.id()
    ));
    lines.push(
        "3. If you are waiting for background work, write here what you wait for, when it should end, and what you will do if it does not return; then go on with the work."
            .to_owned(),
    );
    lines.push(
        "If nothing changes, the supervisor asks a person to look at this session.".to_owned(),
    );
    Ok(lines.join("\n"))
}

/// What the headless reviewer is asked (ADR-0023 decision 2, ADR-0027
/// decision 2): where the material is, the task's acceptance, the verdict
/// schema and where `revise` ends and `concern` begins.
pub fn review_prompt(task: &Task, run: &TaskRun, review_path: &str) -> String {
    format!(
        "You review run {run_id} of dagq task {task_id} ({title}) before it lands on main.\n\
         Read the review material at {review_path}: the task, its goal, the receipt, the commits and the full diff. Read the worktree if you need more. Do not change any file.\n\n\
         Acceptance criteria of the task:\n{acceptance}\n\n\
         Decide one verdict:\n\
         - pass: the diff meets the acceptance criteria and the task's instructions and nothing needs fixing.\n\
         - revise: findings the worker can fix without a person's judgment: missing tests or evidence, lint, fmt or clippy findings, a receipt that disagrees with the diff where fixing the diff settles it, or an obvious gap inside the instructed scope.\n\
         - concern: findings that need a person's judgment: a mismatch with the acceptance criteria, changes the task did not ask for, or a finding that involves a judgment call.\n\n\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"pass\" | \"revise\" | \"concern\", \"reasons\": [string], \"summary\": string}}\n\
         reasons lists each finding (empty for pass); summary is one or two sentences.\n",
        run_id = run.id(),
        task_id = task.id(),
        title = task.title(),
        acceptance = or_none(task.acceptance()),
    )
}

/// The tools the headless triage may use beyond what needs no permission:
/// reading only.
pub const TRIAGE_TOOLS: &[&str] = &["Read", "Grep", "Glob"];

/// Bytes of each log, receipt and screen the triage prompt carries (their
/// ends).
const TRIAGE_TAIL_BYTES: usize = 3000;

/// Logs of a run directory the triage reads: the latest integrate
/// attempt's `integrate-<attempt>-verify-N.log` (see [`integrate_logs`]) and
/// `verify-N.log`, at most this many.
const TRIAGE_LOGS: usize = 8;

/// What the headless triage is asked (ADR-0024 decision 3): the task, the
/// run's error, receipt, verification logs, final screen and events, the
/// task's earlier runs, the verdict schema and the rule that a task with
/// [`TRIAGE_RETRY_FAILURES`] failed or interrupted runs is not retried.
/// `dir` is where the run's files are.
pub fn triage_prompt(
    files: &dyn RunFiles,
    detail: &TaskDetail,
    run: &TaskRun,
    resumes: usize,
    dir: &Path,
) -> Result<String> {
    let task = &detail.task;
    let failures = detail
        .runs
        .iter()
        .filter(|r| matches!(r.status(), RunStatus::Failed | RunStatus::Interrupted))
        .count();
    let read = |path: &Path| {
        files
            .read(path)
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    };
    let mut material = String::new();
    let receipt = run.receipt_path().map(Path::new).and_then(read);
    material.push_str(&format!(
        "Receipt ({}):\n{}\n",
        run.receipt_path().unwrap_or("none"),
        fenced(
            "json",
            or_none(tail(
                receipt.as_deref().unwrap_or_default().trim(),
                TRIAGE_TAIL_BYTES
            ))
        )
    ));
    let (latest, earlier) = integrate_logs(files, dir);
    let mut logs = latest;
    // `verify-N.log` is what validation wrote before ADR-0023.
    let mut validation: Vec<PathBuf> = log_names(files, dir)
        .into_iter()
        .filter(|(name, _)| name.starts_with("verify-") && name.ends_with(".log"))
        .map(|(_, path)| path)
        .collect();
    validation.sort();
    logs.extend(validation);
    logs.truncate(TRIAGE_LOGS);
    if !earlier.is_empty() {
        material.push_str(&format!(
            "Logs of earlier integrate attempts (not shown): {}\n",
            earlier
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if logs.is_empty() {
        material.push_str("Verification logs: none\n");
    }
    for log in &logs {
        let text = read(log).unwrap_or_default();
        material.push_str(&format!(
            "Verification log {} (end):\n{}\n",
            log.display(),
            fenced("text", or_none(tail(text.trim(), TRIAGE_TAIL_BYTES)))
        ));
    }
    let screen = read(&dir.join("terminal-final.txt"));
    material.push_str(&format!(
        "Final screen of the session (end of terminal-final.txt):\n{}\n",
        fenced(
            "text",
            or_none(tail(
                screen.as_deref().unwrap_or_default().trim(),
                TRIAGE_TAIL_BYTES
            ))
        )
    ));
    let events: Vec<Value> = detail
        .events
        .iter()
        .filter(|e| e.run_id.as_ref() == Some(run.id()))
        .map(super::health::compact_event)
        .collect();
    let events = &events[events.len().saturating_sub(40)..];
    material.push_str(&format!(
        "Events of the run (the last {}):\n{}\n",
        events.len(),
        fenced(
            "json",
            &events
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        )
    ));
    let earlier: Vec<String> = detail
        .runs
        .iter()
        .filter(|r| *r.id() != *run.id())
        .map(|r| {
            let verdicts: Vec<String> = detail
                .events
                .iter()
                .filter(|e| e.run_id.as_ref() == Some(r.id()) && e.kind == "triage_finished")
                .map(|e| format!("{}", e.payload.get("action").unwrap_or(&Value::Null)))
                .collect();
            format!(
                "- run {} {}: {}{}",
                r.id(),
                r.status().as_str(),
                or_none(tail(r.last_error().unwrap_or_default(), 300)),
                if verdicts.is_empty() {
                    String::new()
                } else {
                    format!(" (triaged: {})", verdicts.join(", "))
                }
            )
        })
        .collect();
    let retry_rule = if failures >= TRIAGE_RETRY_FAILURES {
        format!(
            "This task has {failures} failed or interrupted runs, this one included: do not answer retry (the supervisor turns it into ask)."
        )
    } else {
        format!(
            "This task has {failures} failed or interrupted run(s), this one included; from {TRIAGE_RETRY_FAILURES} on, retry is not allowed and the supervisor turns it into ask."
        )
    };
    let resume_rule = if resumes >= MAX_RESUME_ATTEMPTS {
        format!("The run was resumed {resumes} times already: do not answer resume.")
    } else {
        format!(
            "The run was resumed {resumes} time(s) (at most {MAX_RESUME_ATTEMPTS}); resume needs the run's worktree."
        )
    };
    Ok(format!(
        "You triage run {run_id} of dagq task {task_id} ({title}), which ended {status}. Decide what the supervisor does next.\n\
         Read only: the material below, and the files it names if you need more (the run directory is {dir}, the worktree {worktree}). Do not change any file.\n\n\
         Task description:\n{description}\n\n\
         Acceptance criteria:\n{acceptance}\n\n\
         Last error of the run:\n{last_error}\n\n\
         {material}\n\
         Earlier runs of the task:\n{earlier}\n\n\
         Decide one verdict:\n\
         - retry: the failure is transient or came from the environment (the machine slept, a process was killed, the session never started, an outage), and a new run from the current main is likely to succeed. The task goes back to ready and a new run starts from scratch; this run's work is not reused.\n\
         - resume: this run's worktree holds useful work that its own session can finish with a concrete instruction (fix the failing test, commit and rewrite the receipt, rebase). instruction is what the session must do, written to it.\n\
         - ask: a person has to decide: the task's instructions or acceptance look wrong or impossible, the same failure repeats, the work is no longer needed, or you cannot tell. instruction is the question for the person.\n\
         Rules: {retry_rule} {resume_rule}\n\n\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"retry\" | \"resume\" | \"ask\", \"reason\": string, \"instruction\": string}}\n\
         reason is one or two sentences on why; instruction may be empty for retry.\n",
        run_id = run.id(),
        task_id = task.id(),
        title = task.title(),
        status = run.status().as_str(),
        dir = dir.display(),
        worktree = run.worktree_path().unwrap_or("none"),
        description = or_none(task.description()),
        acceptance = or_none(task.acceptance()),
        last_error = or_none(run.last_error().unwrap_or_default()),
        earlier = if earlier.is_empty() {
            "none".to_owned()
        } else {
            earlier.join("\n")
        },
    ))
}

/// The fixed request the supervisor types into the live session for a
/// `revise` verdict (ADR-0027 decision 2), one instruction per line; the
/// backend sends it as one line.
pub(crate) fn revise_request(
    task: &Task,
    run: &TaskRun,
    attempt: usize,
    reasons: &[String],
) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let verify = serde_json::to_string(task.verification_commands())?;
    let mut lines = vec![format!(
        "dagq: the supervisor's review of run {} (task {}) asks for changes (revise {attempt} of {MAX_REVISE_ATTEMPTS}).",
        run.id(),
        task.id()
    )];
    lines.push("Findings:".to_owned());
    for reason in reasons {
        lines.push(format!("- {reason}"));
    }
    lines.push("Steps:".to_owned());
    lines.push("1. Fix the findings in this worktree and commit.".to_owned());
    lines.push(format!("2. Run the verification commands {verify}."));
    lines.push("3. Keep the worktree clean.".to_owned());
    lines.push(format!("4. {STOP_BACKGROUND}"));
    lines.push(format!(
        "5. Rewrite the receipt at {receipt} with the new head commit, writing a temporary file in the same directory and renaming it."
    ));
    lines.push(
        "6. Do not merge or push. When done, report briefly and stop; do not run /exit.".to_owned(),
    );
    Ok(lines.join("\n"))
}

/// The tools the headless plan review may use beyond what needs no
/// permission: reading only, like the triage.
pub const PLAN_REVIEW_TOOLS: &[&str] = &["Read", "Grep", "Glob"];

/// Characters of a precedent's question and answer the plan review prompt
/// and the revise request quote.
const PRECEDENT_CHARS: usize = 400;

/// What the headless plan review reads (ADR-0041 decision 10): the
/// proposal and its tasks, the goals they belong to, what `dagq lint`
/// found, the other proposals not yet ready (oldest submission first), the
/// ready and in-progress tasks, and the asks a person answered before.
pub struct PlanReviewMaterial<'a> {
    pub proposal: &'a Proposal,
    pub tasks: &'a [TaskDetail],
    pub goals: &'a [Goal],
    pub lint: &'a [LintViolation],
    /// Other submitted or revising proposals with their tasks.
    pub others: &'a [(Proposal, Vec<Task>)],
    /// Ready and in-progress tasks, with their long fields.
    pub queued: &'a [TaskListItem],
    /// Asks a person answered, newest first.
    pub precedents: &'a [Ask],
    pub repo_root: &'a Path,
}

/// One line quoting an answered ask as a precedent.
pub fn precedent_line(ask: &Ask) -> String {
    let cut = |text: &str| {
        super::health::truncate(text, PRECEDENT_CHARS).unwrap_or_else(|| text.to_owned())
    };
    format!(
        "precedent: ask {id}{task} ({kind}) asked: {question} — a person answered: {answer}",
        id = ask.id,
        task = ask
            .task_id
            .map(|task| format!(" about task {task}"))
            .unwrap_or_default(),
        kind = ask.kind.as_str(),
        question = cut(&ask.question.replace('\n', " ")),
        answer = cut(ask.answer.as_deref().unwrap_or("(none)")),
    )
}

/// What the headless plan review is asked: the material, the checks, the
/// fixes it may make itself and the verdict schema (ADR-0041 decisions 10,
/// 11, 14, 15). The repository's own rules are not in the runtime: the job
/// reads them from the repository's documents.
pub fn plan_review_prompt(material: &PlanReviewMaterial<'_>) -> Result<String> {
    let proposal = material.proposal;
    let json_lines = |values: Vec<Value>| {
        if values.is_empty() {
            "(none)".to_owned()
        } else {
            fenced(
                "json",
                &values
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
    };
    let tasks = json_lines(
        material
            .tasks
            .iter()
            .map(|detail| {
                let mut task = serde_json::to_value(&detail.task)?;
                task["dependencies"] = serde_json::to_value(&detail.dependencies)?;
                task["goal_dependencies"] = serde_json::to_value(&detail.goal_dependencies)?;
                Ok(task)
            })
            .collect::<Result<_>>()?,
    );
    let goals = json_lines(
        material
            .goals
            .iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<_>>()?,
    );
    let lint = json_lines(
        material
            .lint
            .iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<_>>()?,
    );
    let others = if material.others.is_empty() {
        "(none)".to_owned()
    } else {
        material
            .others
            .iter()
            .map(|(other, tasks)| {
                let earlier =
                    (other.submitted_at(), other.id()) < (proposal.submitted_at(), proposal.id());
                let tasks = tasks
                    .iter()
                    .map(|task| {
                        serde_json::json!({
                            "id": task.id(), "status": task.status(), "title": task.title(),
                            "description": task.description(), "acceptance": task.acceptance(),
                            "paths": task.paths(),
                        })
                        .to_string()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "Proposal {} ({}, submitted {}, {} this one):\n{}",
                    other.id(),
                    other.status().as_str(),
                    other.submitted_at(),
                    if earlier { "before" } else { "after" },
                    fenced("json", &tasks)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let queued = json_lines(
        material
            .queued
            .iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<_>>()?,
    );
    let precedents = if material.precedents.is_empty() {
        "(none)".to_owned()
    } else {
        material
            .precedents
            .iter()
            .map(|ask| format!("- {}", precedent_line(ask)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(format!(
        "You are the plan review of dagq proposal {id}: decide whether the queue may run its tasks as written, before they become ready.\n\
         Read only. Do not change any file and do not run dagq commands that write.\n\n\
         First read the repository's own rules in {repo}: AGENTS.md and CLAUDE.md, docs/adr/README.md (the ADR index) and the ADRs and design documents the tasks name. \
         Apply what they say (the verification each kind of change needs, the declared paths, how ADR numbers are assigned, ...); the runtime has no such rules of its own.\n\n\
         The proposal was submitted {submitted} and was sent back {revises} time(s) before (at most {max}; a revise past that goes to a person as a concern).\n\n\
         Tasks of the proposal:\n{tasks}\n\n\
         Goals they belong to (description, acceptance, constraints; constraints win over a task's description):\n{goals}\n\n\
         The mechanical checks (`dagq lint`) found:\n{lint}\n\n\
         Other proposals not ready yet:\n{others}\n\n\
         Ready and in-progress tasks:\n{queued}\n\n\
         Asks a person answered before (newest first):\n{precedents}\n\n\
         Check the meaning of the plan:\n\
         - a task that repeats another task (ready, in progress, in another proposal, or already landed on main);\n\
         - a task whose change is already on main (read the source);\n\
         - a contradiction with an ADR or with the goal's constraints;\n\
         - an acceptance criterion that contradicts the task's own description or a sibling task's acceptance (for example a change of a type whose acceptance says a test file that uses the type is not changed);\n\
         - tasks that change the same files without a dependency between them;\n\
         - a contradiction with another proposal: with one submitted before this one, send this one back; with one submitted after, pass this one (the later one is checked against it);\n\
         - a ready task that has to change for this proposal to hold: name it in reopen, and the runtime takes it out of the claim for a planner to fix; an in-progress task is never changed: send this proposal back asking for a task that fixes it after it lands and depends on it;\n\
         - every finding of `dagq lint` is one to fix.\n\n\
         Decide one verdict:\n\
         - pass: the tasks may run as written, after the actions below.\n\
         - revise: findings the planner can fix without a person's judgment (wording, acceptance, verification, paths, a split, a missing task or dependency). Each reason says what to change.\n\
         - concern: findings that need a person's judgment: a doubtful duplicate, a change that looks already done, a contradiction with an ADR or the goal's constraints, a change of the plan's intent.\n\
         When a finding is of the same kind as an answered ask above, put that ask's id in precedents and say in the reason how the person answered then.\n\n\
         actions are the only changes you make yourself, and only with pass: add_dependency (a task of the proposal waits for another task), lower_priority (never raise one), cancel_duplicate (only an obvious duplicate; a doubtful one is a concern). Everything else is the planner's.\n\n\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"pass\" | \"revise\" | \"concern\", \"reasons\": [string], \"summary\": string, \
         \"actions\": [{{\"action\": \"add_dependency\", \"task_id\": int, \"depends_on\": int}} | {{\"action\": \"lower_priority\", \"task_id\": int, \"priority\": \"low\" | \"normal\" | \"high\" | \"urgent\"}} | {{\"action\": \"cancel_duplicate\", \"task_id\": int, \"duplicate_of\": int}}], \
         \"reopen\": [{{\"task_id\": int, \"reason\": string}}], \"precedents\": [int]}}\n\
         reasons lists each finding (empty for pass); summary is one or two sentences; actions, reopen and precedents may be empty.\n",
        id = proposal.id(),
        repo = material.repo_root.display(),
        submitted = proposal.submitted_at(),
        revises = proposal.revise_count(),
        max = MAX_PLAN_REVISES,
    ))
}

/// What the supervisor types into the live planner a revise goes back to
/// (ADR-0041 decisions 12, 13).
pub fn plan_revise_request(proposal: ProposalId, reasons: &[String]) -> String {
    let reasons = if reasons.is_empty() {
        "- (none given)".to_owned()
    } else {
        reasons
            .iter()
            .map(|reason| format!("- {reason}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Plan review sent proposal {proposal} back. Fix what these reasons point at:\n{reasons}\n\
         Then submit it again with `dagq submit --proposal {proposal}`. A fix that changes the plan's intent (acceptance, scope, the relation to the goal) needs the person: ask them here first."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::memory_files::MemoryFiles;
    use crate::domain::{
        GoalRecord, GoalStatus, GoalVerdict, Provider, RunId, RunRecord, TaskRecord, TaskStatus,
    };
    use serde_json::json;
    use std::time::UNIX_EPOCH;

    const SHA: &str = "1111111111111111111111111111111111111111";
    const RUN: &str = "00000000-0000-4000-8000-000000000001";

    fn task(id: i64, title: &str, status: TaskStatus) -> Task {
        Task::restore(TaskRecord {
            id: TaskId::new(id),
            title: title.into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            status,
            goal_id: None,
            context: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap()
    }

    fn run(task_id: i64, status: RunStatus, result_commit: Option<&str>) -> TaskRun {
        TaskRun::restore(RunRecord {
            id: RunId::new(RUN).unwrap(),
            task_id: TaskId::new(task_id),
            status,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            base_commit: CommitSha::try_from(SHA).unwrap(),
            branch: Some(format!("dagq/{RUN}")),
            worktree_path: Some("/runs/run/worktree".into()),
            workspace_id: None,
            receipt_path: Some("/runs/run/receipt.json".into()),
            log_path: None,
            result_commit: result_commit.map(|sha| CommitSha::try_from(sha).unwrap()),
            repo_path: None,
            run_dir: Some("/runs/run".into()),
            last_error: None,
            workspace_closed_at: None,
            created_at: String::new(),
        })
        .unwrap()
    }

    #[test]
    fn goal_dependencies_share_the_predecessor_section_with_short_summaries() {
        let files = MemoryFiles::default();
        let receipt = json!({
            "run_id": RUN,
            "result": "succeeded",
            "commit": SHA,
            "tests": {"status": "passed", "evidence_or_reason": "cargo test"},
            "e2e": {"status": "not_applicable", "evidence_or_reason": "none"},
            "subagent_review": {"status": "not_applicable", "evidence_or_reason": "small"},
            "summary": "word ".repeat(100),
        });
        files.put(
            Path::new("/runs/run/receipt.json"),
            UNIX_EPOCH,
            &receipt.to_string(),
        );
        let goal = Goal::restore(GoalRecord {
            id: GoalId::new(4),
            title: "upstream goal".into(),
            description: String::new(),
            acceptance: String::new(),
            constraints: String::new(),
            doc: None,
            status: GoalStatus::Open,
            closed_at: Some("2026-09-25T00:00:00Z".into()),
            verdict: Some(GoalVerdict::Achieved),
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
        let landed = GoalPredecessorSummary::from_goal_predecessor(
            &files,
            &GoalPredecessor {
                goal: goal.clone(),
                tasks: vec![Predecessor {
                    task: task(2, "upstream work", TaskStatus::Completed),
                    integrated_run: Some(run(2, RunStatus::Integrated, Some(SHA))),
                }],
            },
        );
        let summary = landed.tasks[0].summary.clone();
        assert_eq!(summary.chars().count(), GOAL_TASK_SUMMARY_CHARS + 1);
        assert!(summary.ends_with('…'));
        let empty = GoalPredecessorSummary::from_goal_predecessor(
            &files,
            &GoalPredecessor {
                goal,
                tasks: Vec::new(),
            },
        );

        let waiting = task(9, "downstream", TaskStatus::InProgress);
        let own_run = run(9, RunStatus::Claimed, None);
        let text = prompt(&waiting, &own_run, None, &[], &[landed, empty], &[]).unwrap();
        assert!(
            text.contains(&format!(
                "Predecessor tasks (their changes are already in your base commit):\n\
                 - goal 4 (closed as achieved): upstream goal; its completed tasks:\n  \
                 - task 2: upstream work; result commit {SHA}; summary: {summary}\n\
                 - goal 4 (closed as achieved): upstream goal; its completed tasks:\n  - none\n"
            )),
            "{text}"
        );
        let alone = prompt(&waiting, &own_run, None, &[], &[], &[]).unwrap();
        assert!(alone.contains("Predecessor tasks: none\n"));
    }
}
