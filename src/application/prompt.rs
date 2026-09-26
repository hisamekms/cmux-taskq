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
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::{
    RunFiles, TaskListItem, fenced,
    integrate::{integrate_logs, log_names},
    or_none, tail,
};
use crate::domain::{
    Ask, CommitSha, DraftOrigin, DraftTarget, Goal, GoalId, GoalPredecessor, GoalTask,
    LintViolation, MAX_DRAFT_PLANNERS, MAX_PLAN_REVISES, MAX_RESUME_ATTEMPTS, MAX_REVISE_ATTEMPTS,
    Predecessor, Proposal, ProposalId, Receipt, RunEvent, RunId, RunStatus, TRIAGE_RETRY_FAILURES,
    Task, TaskDetail, TaskId, TaskRun,
    recovery::{ProcessInfo, RecoveryAlert},
    related::RelatedTask,
    resume,
    search::SearchHit,
    stats::conflicts::ConflictHotspot,
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

/// The run a retry carries over (ADR-0047 decision 24): its resumes were
/// used up on conflicts with main after its review passed, so the next run
/// of its task starts from its commit instead of from scratch.
#[derive(Debug, Clone, Serialize)]
pub struct Inheritance {
    pub run_id: RunId,
    /// The commit the run's own commits start after: its base, until the
    /// caller narrows it to the merge base of the head and the current main
    /// (a resume that rebased part of the way put main's commits under it).
    pub base: CommitSha,
    /// The run's head, kept under `refs/dagq/runs/<run-id>`.
    pub head: String,
    pub branch: Option<String>,
    pub receipt_path: Option<String>,
    /// Its receipt's summary, whitespace collapsed; `(receipt unavailable)`
    /// when it cannot be read.
    pub summary: String,
}

impl Inheritance {
    /// What the next run of `previous`'s task inherits, when `previous` was
    /// ended by the retry that carries its branch over
    /// ([`crate::domain::resume::retried_with_inheritance`]): the head that
    /// retry recorded, and the summary of its receipt.
    pub fn of(files: &dyn RunFiles, previous: &TaskRun, events: &[RunEvent]) -> Option<Self> {
        if !resume::retried_with_inheritance(events) {
            return None;
        }
        let inherit = &events
            .iter()
            .rev()
            .find(|e| resume::is_inherit_retry(e))?
            .payload["inherit"];
        let head = inherit["head"].as_str()?.to_owned();
        let receipt_path = previous.receipt_path().map(str::to_owned);
        let summary = receipt_path
            .as_deref()
            .and_then(|path| files.read_to_string(Path::new(path)).ok())
            .and_then(|text| Receipt::parse(&text).ok())
            .map(|receipt| {
                receipt
                    .summary
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|summary| !summary.is_empty())
            .unwrap_or_else(|| "(receipt unavailable)".to_owned());
        Some(Self {
            run_id: previous.id().clone(),
            base: previous.base_commit().clone(),
            head,
            branch: inherit["branch"].as_str().map(str::to_owned),
            receipt_path,
            summary,
        })
    }

    /// The prompt's section on it: start from its commit, bring it onto the
    /// current main, resolve the conflicts, verify and write the receipt.
    fn section(&self) -> String {
        format!(
            "Carried over from run {run}: its review passed, but its landing kept conflicting with main until its resumes were used up, so this run starts from its work instead of from scratch. \
             Its work is commit {head} (kept as refs/dagq/runs/{run}{branch}); its own commits are {base}..{head}. \
             Bring them onto your base, the current main (for example `git cherry-pick {base}..{head}` in your worktree), resolve the conflicts keeping what both sides meant, rerun your checks in the worktree as above, and write the receipt for your own head. \
             Its receipt ({receipt}) summary: {summary}\n",
            run = self.run_id,
            head = self.head,
            base = self.base,
            branch = self
                .branch
                .as_deref()
                .map(|branch| format!(", branch {branch}"))
                .unwrap_or_default(),
            receipt = self.receipt_path.as_deref().unwrap_or("no receipt path"),
            summary = self.summary,
        )
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

/// Which checks a session runs in its worktree before the receipt, given
/// how it names the task's verification commands (`above`, or the JSON
/// list in a one-line request). The verification of record for a commit is
/// integrate's single run of the verification commands after its rebase
/// (ADR-0049 decision 1), so the worker runs what the repository's own
/// instructions ask of it (they may leave a slow gate such as a coverage
/// run to integrate), and the verification commands only when the
/// repository says nothing. The runtime names no tool here: dagq runs in
/// any repository.
pub(crate) fn local_checks(verify: &str) -> String {
    format!(
        "Run in the worktree the checks the repository's instructions (AGENTS.md or CLAUDE.md) ask a worker to run, which may leave some of the verification commands to integrate; when the instructions name no such checks, run the verification commands {verify}."
    )
}

/// Text of `prompt.txt`. `goal` is the task's goal as it reads at claim
/// time, `predecessors` the task's direct dependencies, `goal_predecessors`
/// the goals it depends on (in the Predecessor section) and `siblings` the
/// other tasks executing at claim time (`siblings_in_progress`), and
/// `inherited` the run a retry carries over, if any. The Goal,
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
    inherited: Option<&Inheritance>,
) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let inherited = inherited.map(Inheritance::section).unwrap_or_default();
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
         Implement the task, run the checks described below, and commit the result.\n\
         Do not merge, push, close the workspace, or modify the queue/runtime files.\n\
         Perform applicable unit tests, E2E, and subagent review. Record evidence or an explicit reason when not applicable.\n\
         Task title: {title}\nDescription:\n{description}\nAcceptance criteria:\n{acceptance}\n\
         Verification commands (integrate runs them once after rebasing onto main; that run is the verification of record for the commit):\n{verification}\n\
         {local_checks}\n\
         {evidence}{paths}{goal}{context}{predecessors}{siblings}{inherited}\
         Your assignment is this task only. Do not change what a sibling task owns; if you find work outside this task, record it in the receipt as follow_ups instead of doing it.\n\
         Write a completion receipt to {receipt} using a temporary file in the same directory and atomic rename.\n\
         Receipt JSON: {{\"run_id\":\"{run_id}\",\"result\":\"succeeded or failed\",\"commit\":\"full Git SHA of the branch head\",\"tests\":{{\"status\":\"passed, failed or not_applicable\",\"evidence_or_reason\":\"...\"}},\"e2e\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"subagent_review\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"summary\":\"...\",\"follow_ups\":[{{\"title\":\"...\",\"description\":\"...\"}}]}}\n\
         Each of tests, e2e and subagent_review needs evidence when passed and a reason when not_applicable.\n\
         follow_ups is optional: an array of work you found outside this task, each with a title and a description, for the planner to decide on; omit it when there is none.\n\
         You may write this receipt outside the worktree. Keep the worktree clean after committing.\n\
         The supervisor rejects the run unless the commit is the clean head of your branch on top of the base commit, and integrate runs the verification commands itself after rebasing onto main.\n\
         When you need a decision you cannot make from the task and the repository, do not write the question to the terminal and wait: run `dagq ask --run {run_id} --kind worker_question --because scope --question '...'` in the worktree (one ask at a time, with everything you need decided in its question), report briefly that you asked, and stop. `--because` says why a person is needed: `scope` (the acceptance or the scope changes) or `discard` (whether to throw work away); a question that fits neither is yours to decide and record in the receipt's summary, or, when it leads outside the task, a failed receipt saying why. The answer arrives in this terminal as `answer to ask <id>: ...`; continue from it.\n\
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
        local_checks = local_checks("above"),
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
         A fix that changes the plan's intent (acceptance, scope, the relation to the goal) needs a person: raise it to the inbox with `dagq ask --task ID --kind planner_question --because scope` as the skill describes, stop, and continue from the answer typed into this terminal.\n\
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
         3. Ask: when you cannot decide without a person (the plan's intent, its scope, whether it belongs to this goal or a new one), run `dagq ask --task {id} --kind planner_question --because scope --question '<everything the person needs, with your recommendation>' --option adopt --option cancel --option keep_draft`, report briefly and stop. The answer arrives in this terminal as `answer to ask <id>: ...`: on adopt do 1, on cancel do 2 (the note names the ask), on keep_draft leave the draft as it is and stop.\n\
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
    /// A run that waited to land, parked by the landing recheck after
    /// another landing moved main (ADR-0068 decision 3): rebase, like
    /// `Landing`, and run the failed recheck command again.
    Recheck,
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
        ResumeKind::Recheck => format!(
            "dagq: run {} (task {}) was waiting to land, and after another landing moved main the supervisor's landing recheck found that it no longer lands, so the run is needs_session before anyone answers for it.",
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
    let checks = local_checks(&serde_json::to_string(task.verification_commands())?);
    if request.kind == ResumeKind::EvidenceMissing {
        lines.push(
            "1. Run the checks the reason names as missing and write their evidence into the receipt."
                .to_owned(),
        );
        lines.push(format!("2. If that changes files, commit them. {checks}"));
    } else if request.kind == ResumeKind::ScopeViolation {
        lines.push(format!(
            "1. Take the changes to the paths the reason names out of the run branch: restore each to its state at git merge-base HEAD {} (delete the ones that did not exist there) and commit; if the task cannot be done without them, write the receipt with result failed and say which paths it needs.",
            request.main
        ));
        lines.push(format!("2. {checks}"));
    } else if request.kind == ResumeKind::SentBack {
        lines.push(format!(
            "1. Fix the findings in the reason and commit; if main moved, git rebase {} first.",
            request.main
        ));
        lines.push(format!("2. {checks}"));
    } else if request.kind == ResumeKind::Triage {
        lines.push(format!(
            "1. Do what the reason asks in this worktree and commit; if main moved, git rebase {} first.",
            request.main
        ));
        lines.push(format!("2. {checks}"));
    } else {
        lines.push(format!(
            "1. In this worktree run git rebase {} and resolve the conflicts.",
            request.main
        ));
        // Only integrate's deferral can name a failed verification command;
        // the precheck's reason is always a conflict.
        let reproduce = match request.kind {
            ResumeKind::Landing => {
                " If the reason is a verification command that failed after integrate's rebase, you may also run that command in the worktree to reproduce and fix the failure."
            }
            ResumeKind::Recheck => {
                " If the reason is a command that failed on main with the run merged in (git found no conflict), run that command in the worktree after the rebase to reproduce and fix the failure."
            }
            _ => "",
        };
        lines.push(format!("2. {checks}{reproduce} Commit the result."));
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

/// The one fixed request the supervisor types into a session that went idle
/// with a receipt naming `receipt_commit` while its clean worktree HEAD is
/// `head`, a new commit on top of its base (task 357): rewrite the receipt
/// for the head, or fix the worktree first.
pub(crate) fn stale_receipt_nudge(
    run: &TaskRun,
    receipt_commit: &str,
    head: &CommitSha,
) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    Ok([
        format!(
            "dagq: run {} went idle, but its receipt names commit {receipt_commit} while the clean worktree HEAD is {head} (for example after a rebase or a new commit). The supervisor cannot accept a receipt for another commit.",
            run.id()
        ),
        "Steps:".to_owned(),
        format!(
            "1. If HEAD is the work you mean to submit, rewrite the receipt at {receipt} with commit {head} (git rev-parse HEAD), writing a temporary file in the same directory and renaming it. Otherwise fix the worktree, commit, and rewrite the receipt with the new HEAD."
        ),
        format!("2. {STOP_BACKGROUND}"),
        "3. Do not merge or push. When done, report briefly and stop; do not run /exit.".to_owned(),
        "If the receipt stays as it is, the run goes on as before and validation judges it."
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
        "2. If you need a decision, run `dagq ask --run {} --kind worker_question --because scope --question '...'` (or `--because discard` for whether to throw work away) and stop.",
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

/// What the recovery job of a run that ended `failed` or `interrupted`
/// reads beyond a live session's material (ADR-0047 decision 39, as the
/// triage read it before): the run's error, receipt, verification logs,
/// final screen and events, the task's earlier runs with their rounds, and
/// the rules the runtime holds `retry` and `resume` to. `dir` is where the
/// run's files are.
pub fn ended_run_material(
    files: &dyn RunFiles,
    detail: &TaskDetail,
    run: &TaskRun,
    resumes: crate::domain::resume::ResumeCount,
    dir: &Path,
) -> String {
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
    let mut material = format!(
        "Last error of the run:\n{}\n\n",
        or_none(run.last_error().unwrap_or_default())
    );
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
            let actions: Vec<String> = detail
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
                if actions.is_empty() {
                    String::new()
                } else {
                    format!(" (recovered: {})", actions.join(", "))
                }
            )
        })
        .collect();
    material.push_str(&format!(
        "\nEarlier runs of the task:\n{}\n\n",
        if earlier.is_empty() {
            "none".to_owned()
        } else {
            earlier.join("\n")
        }
    ));
    let retry_rule = if failures >= TRIAGE_RETRY_FAILURES {
        format!(
            "This task has {failures} failed or interrupted runs, this one included: do not choose retry (the runtime escalates it)."
        )
    } else {
        format!(
            "This task has {failures} failed or interrupted run(s), this one included; from {TRIAGE_RETRY_FAILURES} on, the runtime does not apply retry."
        )
    };
    let resume_rule = if resumes.exhausted() {
        format!(
            "The run was resumed {} time(s) and its resumes are used up: do not choose resume.",
            resumes.total()
        )
    } else {
        format!(
            "The run was resumed {} time(s) ({} of at most {MAX_RESUME_ATTEMPTS} counted); resume needs the run's worktree.",
            resumes.total(),
            resumes.counted
        )
    };
    material.push_str(&format!("Rules: {retry_rule} {resume_rule}\n"));
    material
}

/// What the runtime read for a recovery job (ADR-0047 decision 39), at the
/// time of the alert.
pub struct RecoveryMaterial<'a> {
    pub alert: RecoveryAlert,
    /// For a run that ended: [`ended_run_material`]. `None` for a live
    /// session.
    pub ended: Option<String>,
    /// The alert's own facts (`recovery_requested`'s payload).
    pub facts: &'a Value,
    pub workspace: &'a str,
    /// The screen's excerpt, or why it could not be read.
    pub screen: &'a str,
    /// The processes that belong to the run (see
    /// [`crate::domain::recovery::run_processes`]), or why they could not
    /// be listed.
    pub processes: std::result::Result<Vec<ProcessInfo>, String>,
    pub git_status: &'a str,
    pub head: &'a str,
    /// The receipt's `commit`, when there is a receipt.
    pub receipt_commit: Option<&'a str>,
    /// The run's earlier recovery verdicts and automatic repairs.
    pub history: &'a [Value],
    /// The actions that apply to this alert.
    pub allowed: &'a [&'a str],
}

/// What each allowed action does, for the recovery prompt.
fn recovery_action_help(action: &str) -> &'static str {
    match action {
        "stop_processes" => {
            "{\"action\": \"stop_processes\", \"pids\": [pid, ...]}: stop these processes (SIGTERM, then SIGKILL after a grace). Only processes listed below as the run's own are allowed; any other pid makes the whole verdict an escalation. Use it for a background process the session waits for that will not end by itself (an orphan holding a pipe, a hung test). Never the session's own wrapper or agent."
        }
        "send_instruction" => {
            "{\"action\": \"send_instruction\", \"instruction\": string}: type this instruction into the session once (it must be idle at its prompt), for example to stop a background command it waits for and rerun the tests."
        }
        "wait" => {
            "{\"action\": \"wait\", \"recheck_after_secs\": n}: do nothing now; if the alert still holds after n seconds (at most 3600), another recovery job runs. The work looks healthy and is only slow, or what holds it passes by itself."
        }
        "retry" => {
            "{\"action\": \"retry\"}: make the task ready again for a new run from the current main. Only for a run whose branch holds no commit of its own (nothing is thrown away); a run with commits needs retry_inherit or a person. Use it when the failure came from the environment (the machine slept, a process was killed, the session never started, an outage)."
        }
        "retry_inherit" => {
            "{\"action\": \"retry_inherit\"}: make the task ready again for a new run that carries this run's branch over onto the current main. Only for a run whose branch has commits, and once per task."
        }
        "resume" => {
            "{\"action\": \"resume\", \"instruction\": string}: send the run back to a session of its own in its worktree, with the instruction (what to do: fix the failing test, commit and rewrite the receipt, rebase) added to the resolution request. Only while its resumes are not used up."
        }
        "answer_known_dialog" => {
            "{\"action\": \"answer_known_dialog\", \"dialog\": \"background_work\" | \"settings_panel\"}: send the fixed keys to one of the known dialogs on the screen (Background work is running, only after the supervisor's /exit with a clean worktree and the receipt at HEAD; the Settings / Usage panel). Never another dialog."
        }
        "close_and_proceed" => {
            "{\"action\": \"close_and_proceed\"}: close the session's workspace and go on to land. Only when the review passed, the worktree is clean and the receipt names its HEAD, the reviewed commit."
        }
        _ => "",
    }
}

/// What the recovery job of an alert is asked (ADR-0047 decisions 39 and
/// 40): the alert, the task, the screen, the run's processes, the
/// worktree's state and the run's earlier repairs, for a run that ended
/// also its error, receipt, logs and events, then the allowed actions and
/// the verdict schema.
pub fn recovery_prompt(
    task: &Task,
    run: &TaskRun,
    attempt: usize,
    material: &RecoveryMaterial<'_>,
) -> Result<String> {
    let processes = match &material.processes {
        Ok(processes) if processes.is_empty() => "none".to_owned(),
        Ok(processes) => processes
            .iter()
            .map(|p| {
                format!(
                    "- pid {} (parent {}, running {}s, cwd {}): {}",
                    p.pid,
                    p.ppid,
                    p.elapsed_secs,
                    p.cwd.as_deref().unwrap_or("unknown"),
                    tail(&p.command, 300)
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Err(error) => format!("(the processes could not be listed: {error})"),
    };
    let history = if material.history.is_empty() {
        "none".to_owned()
    } else {
        material
            .history
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let actions = material
        .allowed
        .iter()
        .map(|action| format!("- {}", recovery_action_help(action)))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!(
        "You are dagq's recovery job (attempt {attempt}) for run {run_id} of task {task_id} ({title}), {state}. The supervisor raised the alert {alert}: {meaning}\n\
         Decide whether the runtime can repair it with one of the allowed actions below, or whether a person has to look.\n\
         Read only: the material below, and the files it names if you need more (the worktree is {worktree}). Do not change any file and do not run commands; the runtime applies your verdict.\n\n\
         Task description:\n{description}\n\n\
         Acceptance criteria:\n{acceptance}\n\n\
         Alert facts:\n{facts}\n\n\
         {ended}\
         Last lines of the session's screen:\n{screen}\n\n\
         Processes of the run (working directory in the worktree, or under the session's wrapper; the wrapper and the agent themselves are not listed):\n{processes}\n\n\
         Worktree: HEAD {head}, receipt commit {receipt}, git status:\n{status}\n\n\
         Earlier recovery verdicts and repairs of this run:\n{history}\n\n\
         Allowed actions:\n{actions}\n\
         Not allowed, ever: cancelling the task, retrying a run that has commits, editing the task, landing without review, writing to main, pushing, deleting branches or worktrees, touching anything outside this run's worktree and workspace, writing the queue database, sending keys to a dialog that is not a known one. If the repair needs any of these, escalate.\n\n\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"repair\" | \"escalate\", \"confidence\": \"high\" | \"low\", \"diagnosis\": string, \"actions\": [action, ...], \"question\": string, \"options\": [string, ...], \"reason_category\": \"recovery_failed\" | \"discard\" | \"scope\"}}\n\
         diagnosis says what you found in one or two sentences. repair needs at least one action and is applied only with confidence high; with confidence low, or with escalate, a person is asked, with your actions as the recommendation, question as the question and options added to theirs. reason_category says why a person is needed: recovery_failed when you cannot repair it or are not sure, discard when the work would be thrown away, scope when it needs a permission you do not have.\n",
        run_id = run.id(),
        task_id = task.id(),
        title = task.title(),
        state = match &material.ended {
            Some(_) => format!("which ended {}; its session is gone", run.status().as_str()),
            None => format!(
                "whose session in workspace {} is still running",
                material.workspace
            ),
        },
        ended = material.ended.as_deref().unwrap_or_default(),
        alert = material.alert.as_str(),
        meaning = match material.alert {
            RecoveryAlert::LongBackground =>
                "background work the session started has run longer than the threshold, and the session waits for it.",
            RecoveryAlert::Failed =>
                "the run failed (its receipt said failed, its validation or landing failed, or its session exited without finishing).",
            RecoveryAlert::Interrupted =>
                "the run's session died and the supervisor recovered the run as interrupted.",
            RecoveryAlert::ResumeExhausted =>
                "the run still needed a session after its last resume, so the supervisor stopped resuming it.",
            RecoveryAlert::StuckExit =>
                "the session did not exit after the supervisor's /exit (or the /exit never reached it), and the runtime's own repairs did not apply.",
            RecoveryAlert::PromptWaiting =>
                "the session waits at a dialog the runtime does not answer by itself.",
            RecoveryAlert::Stalled => "the session looks stuck.",
        },
        worktree = run.worktree_path().unwrap_or("none"),
        description = or_none(task.description()),
        acceptance = or_none(task.acceptance()),
        facts = fenced("json", &serde_json::to_string_pretty(material.facts)?),
        screen = fenced("text", or_none(material.screen.trim())),
        head = material.head,
        receipt = material.receipt_commit.unwrap_or("(no receipt)"),
        status = fenced("text", or_none(material.git_status.trim())),
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
    let checks = local_checks(&serde_json::to_string(task.verification_commands())?);
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
    lines.push(format!("2. {checks}"));
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

/// Candidates of each kind (related tasks, search hits) the plan review
/// prompt lists at most for one task of the proposal.
pub const DUPLICATE_CANDIDATES: usize = 5;

/// Where the plan review starts looking for duplicates and changes already
/// made for one task of the proposal (goal 29): the tasks `dagq related`
/// ranks highest with the clues that relate them, and the tasks and landed
/// commits `dagq search` finds for the words of its title, in any status,
/// none of them the proposal's own.
#[derive(Debug, Clone, Serialize)]
pub struct DuplicateCandidates {
    pub task_id: TaskId,
    pub related: Vec<RelatedTask>,
    pub search: Vec<SearchHit>,
}

/// Expected files the summary of a ready or in-progress task lists at
/// most; the hotspot table reads them all.
pub const SUMMARY_EXPECTED_FILES: usize = 10;

/// What the headless plan review reads (ADR-0041 decision 10): the
/// proposal and its tasks, the goals they belong to, what `dagq lint`
/// found, the other proposals not yet ready (oldest submission first), the
/// ready and in-progress tasks, the asks a person answered before, the
/// files that conflict often and each task's duplicate candidates.
pub struct PlanReviewMaterial<'a> {
    pub proposal: &'a Proposal,
    pub tasks: &'a [TaskDetail],
    pub goals: &'a [Goal],
    pub lint: &'a [LintViolation],
    /// Other submitted or revising proposals with their tasks.
    pub others: &'a [(Proposal, Vec<Task>)],
    /// Ready and in-progress tasks, with their long fields; the prompt
    /// lists each in summary and gives the long fields only of those it
    /// has a reason to (task 591).
    pub queued: &'a [TaskListItem],
    /// Ready and in-progress tasks past the limit of `queued`.
    pub queued_left_out: usize,
    /// The files each task of the proposal and of `queued` is expected to
    /// touch (ADR-0069 decisions 1, 2).
    pub expected: &'a BTreeMap<TaskId, Vec<String>>,
    /// Asks a person answered, newest first.
    pub precedents: &'a [Ask],
    /// The files the landings conflicted in most (`stats`
    /// `conflict_hotspots`), that main still has.
    pub hotspots: &'a [ConflictHotspot],
    /// One entry per task of the proposal, in the order of `tasks`.
    pub candidates: &'a [DuplicateCandidates],
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
    let no_files = Vec::new();
    let expected = |id: TaskId| material.expected.get(&id).unwrap_or(&no_files);
    let touching = |path: &str, ids: &mut dyn Iterator<Item = TaskId>| {
        ids.filter(|&id| crate::domain::claim_defer::touches(expected(id), path))
            .collect::<Vec<_>>()
    };
    // Each hotspot with the tasks expected to touch it; a queued task on
    // the same hotspot as a task of the proposal is given in full.
    let mut full = BTreeSet::new();
    let mut hotspots = Vec::new();
    for file in material.hotspots {
        let path = file.renamed_to.as_deref().unwrap_or(&file.path);
        let own = touching(path, &mut material.tasks.iter().map(|d| d.task.id()));
        let queued = touching(path, &mut material.queued.iter().map(|item| item.id));
        if !own.is_empty() {
            full.extend(queued.iter().copied());
        }
        hotspots.push(serde_json::json!({
            "path": path,
            "conflicts": file.conflicts, "tasks": file.tasks,
            "landings": file.landings, "ratio": file.ratio,
            "last_conflict_at": file.last_conflict_at, "alert": file.alert,
            "proposal_tasks": own, "queued_tasks": queued,
        }));
    }
    let hotspots = json_lines(hotspots);
    for candidates in material.candidates {
        full.extend(candidates.related.iter().map(|task| TaskId::new(task.id)));
        full.extend(
            candidates
                .search
                .iter()
                .filter_map(|hit| match (hit.kind, &hit.id) {
                    (
                        crate::domain::search::SearchKind::Task,
                        crate::domain::search::SearchRef::Id(id),
                    ) => Some(TaskId::new(*id)),
                    _ => hit.task_id.map(TaskId::new),
                }),
        );
    }
    let summaries = material
        .queued
        .iter()
        .map(|item| {
            let files = expected(item.id);
            let mut summary = serde_json::json!({
                "id": item.id, "status": item.status, "priority": item.priority,
                "goal_id": item.goal_id, "title": item.title,
                "paths": item.details.as_ref().map(|details| &details.paths).unwrap_or(&no_files),
                "dependencies": item.dependencies, "goal_dependencies": item.goal_dependencies,
                "expected_files": files.iter().take(SUMMARY_EXPECTED_FILES).collect::<Vec<_>>(),
            });
            if files.len() > SUMMARY_EXPECTED_FILES {
                summary["more_expected_files"] = (files.len() - SUMMARY_EXPECTED_FILES).into();
            }
            if full.contains(&item.id) {
                summary["full_text_below"] = true.into();
            }
            summary
        })
        .collect();
    let mut queued = json_lines(summaries);
    if material.queued_left_out > 0 {
        queued.push_str(&format!(
            "\n({} more ready or in-progress tasks, those of the lowest IDs, are left out of this list)",
            material.queued_left_out
        ));
    }
    let queued_full = json_lines(
        material
            .queued
            .iter()
            .filter(|item| full.contains(&item.id))
            .map(serde_json::to_value)
            .collect::<serde_json::Result<_>>()?,
    );
    let own_expected = json_lines(
        material
            .tasks
            .iter()
            .map(|detail| {
                serde_json::json!({
                    "task_id": detail.task.id(),
                    "expected_files": expected(detail.task.id()),
                })
            })
            .collect(),
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
    let candidates = json_lines(
        material
            .candidates
            .iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<_>>()?,
    );
    let predicted = material
        .tasks
        .iter()
        .filter(|detail| detail.task.status() == crate::domain::TaskStatus::Submitted)
        .map(|detail| detail.task.id().to_string())
        .collect::<Vec<_>>();
    let predicted = if predicted.is_empty() {
        "(none)".to_owned()
    } else {
        predicted.join(", ")
    };
    Ok(format!(
        "You are the plan review of dagq proposal {id}: decide whether the queue may run its tasks as written, before they become ready.\n\
         Read only. Do not change any file and do not run dagq commands that write.\n\n\
         First read the repository's own rules in {repo}: AGENTS.md and CLAUDE.md, docs/adr/README.md (the ADR index) and the ADRs and design documents the tasks name. \
         Apply what they say (the verification each kind of change needs, the declared paths, how ADR numbers are assigned, ...); the runtime has no such rules of its own.\n\n\
         The proposal was submitted {submitted} and was sent back {revises} time(s) before (at most {max}; a revise past that goes to a person as a concern).\n\n\
         Tasks of the proposal:\n{tasks}\n\n\
         Files each task of the proposal is expected to touch (its declared paths; without them, the files the landings of its 3 most related completed tasks changed; a guess, so check it against the source):\n{own_expected}\n\n\
         Goals they belong to (description, acceptance, constraints; constraints win over a task's description):\n{goals}\n\n\
         The mechanical checks (`dagq lint`) found:\n{lint}\n\n\
         Other proposals not ready yet:\n{others}\n\n\
         Ready and in-progress tasks, in summary, newest first (expected_files as for the proposal's tasks, and for an in-progress task also what its run changed so far; full_text_below marks a task given in full below):\n{queued}\n\n\
         In full, the ready and in-progress tasks among the candidates below or expected to touch a hotspot a task of the proposal is expected to touch:\n{queued_full}\n\n\
         Asks a person answered before (newest first):\n{precedents}\n\n\
         Files the landings conflicted in most lately (`dagq stats` conflict_hotspots: conflicts, tasks, landings on main that changed the file, their ratio; alert when over the thresholds), each with the tasks of the proposal (proposal_tasks) and the ready and in-progress tasks (queued_tasks) expected to touch it:\n{hotspots}\n\n\
         Candidates of duplicates and of changes already made, one line per task of the proposal (related: the tasks `dagq related` ranks highest, in any status, with the clues that relate them; search: the tasks and landed commits `dagq search` finds for the words of the task's title, with their status; at most {most} of each, none of the proposal's own tasks; an empty list means none was found):\n{candidates}\n\n\
         Check the meaning of the plan:\n\
         - a task that repeats another task (ready, in progress, in another proposal, or already landed on main); start from its candidates above, a completed or canceled one included, and judge from their titles, clues and the source whether the task really repeats one;\n\
         - a task whose change is already on main (read the source; a completed candidate or a landed commit is where to look);\n\
         - a contradiction with an ADR or with the goal's constraints;\n\
         - an acceptance criterion that contradicts the task's own description or a sibling task's acceptance (for example a change of a type whose acceptance says a test file that uses the type is not changed);\n\
         - tasks that change the same files without a dependency between them, above all a file listed as conflicting often: for each hotspot whose proposal_tasks and queued_tasks are both non-empty, add a dependency (add_dependency, the task of the proposal waiting for the queued one) or say in summary why none is needed; you may read the source to see which files a task of the proposal really touches;\n\
         - a task that partly repeats a ready or in-progress task (the overlap goes once the scope of one is cut): not pass but revise, saying in the reason which part to cut and which of the two keeps it;\n\
         - a contradiction with another proposal: with one submitted before this one, send this one back; with one submitted after, pass this one (the later one is checked against it);\n\
         - a ready task that has to change for this proposal to hold: name it in reopen, and the runtime takes it out of the claim for a planner to fix; an in-progress task is never changed: send this proposal back asking for a task that fixes it after it lands and depends on it;\n\
         - every finding of `dagq lint` is one to fix.\n\n\
         Decide one verdict:\n\
         - pass: the tasks may run as written, after the actions below.\n\
         - revise: findings the planner can fix without a person's judgment (wording, acceptance, verification, paths, a split, a scope that partly overlaps another task, a missing task or dependency). Each reason says what to change.\n\
         - concern: findings that need a person's judgment: a doubtful duplicate, a change that looks already done, a contradiction with an ADR or the goal's constraints, a change of the plan's intent.\n\
         When a finding is of the same kind as an answered ask above, put that ask's id in precedents and say in the reason how the person answered then.\n\n\
         actions are the only changes you make yourself, and only with pass: add_dependency (a task of the proposal waits for another task), lower_priority (never raise one), cancel_duplicate (only an obvious duplicate; a doubtful one, or a change that looks already made, is a concern). Everything else is the planner's.\n\n\
         Whatever the verdict, also estimate the weight of each submitted task of the proposal (tasks {predicted}), one entry per task in predictions, from what you read: \
         a worker (one Claude Opus session in its own Git worktree) implements the task, runs fmt, clippy and the tests of the change (e2e when the runtime changes), commits and writes a receipt; \
         then a headless review (pass / revise / concern) and `integrate`'s verification after the rebase onto main follow, and a failure, a conflict or missing evidence resumes the run. \
         size is S, M or L; nature is mechanical, implementation, design_judgment or investigation; uncertainty is 0 to 1 (1 the least certain); \
         expected_output_tokens is the output tokens (thinking included) of one worker run: a small one about 5000, a large one about 250000, the median about 35000; \
         rework_probability is 0 to 1, the chance the run is resumed or review answers revise or concern; reason is one sentence. The estimate is recorded only and changes nothing of the verdict.\n\n\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"pass\" | \"revise\" | \"concern\", \"reasons\": [string], \"summary\": string, \
         \"actions\": [{{\"action\": \"add_dependency\", \"task_id\": int, \"depends_on\": int}} | {{\"action\": \"lower_priority\", \"task_id\": int, \"priority\": \"low\" | \"normal\" | \"high\" | \"urgent\"}} | {{\"action\": \"cancel_duplicate\", \"task_id\": int, \"duplicate_of\": int}}], \
         \"reopen\": [{{\"task_id\": int, \"reason\": string}}], \"precedents\": [int], \
         \"predictions\": [{{\"task_id\": int, \"size\": \"S\" | \"M\" | \"L\", \"nature\": \"mechanical\" | \"implementation\" | \"design_judgment\" | \"investigation\", \"uncertainty\": number, \"expected_output_tokens\": int, \"rework_probability\": number, \"reason\": string}}]}}\n\
         reasons lists each finding (empty for pass); summary is one or two sentences; actions, reopen and precedents may be empty; predictions has one entry for each submitted task and no other.\n",
        id = proposal.id(),
        repo = material.repo_root.display(),
        submitted = proposal.submitted_at(),
        revises = proposal.revise_count(),
        max = MAX_PLAN_REVISES,
        most = DUPLICATE_CANDIDATES,
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
        verified_task(id, title, status, Vec::new())
    }

    fn verified_task(
        id: i64,
        title: &str,
        status: TaskStatus,
        verification_commands: Vec<String>,
    ) -> Task {
        Task::restore(TaskRecord {
            id: TaskId::new(id),
            title: title.into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands,
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            kind: None,
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
        let text = prompt(&waiting, &own_run, None, &[], &[landed, empty], &[], None).unwrap();
        assert!(
            text.contains(&format!(
                "Predecessor tasks (their changes are already in your base commit):\n\
                 - goal 4 (closed as achieved): upstream goal; its completed tasks:\n  \
                 - task 2: upstream work; result commit {SHA}; summary: {summary}\n\
                 - goal 4 (closed as achieved): upstream goal; its completed tasks:\n  - none\n"
            )),
            "{text}"
        );
        let alone = prompt(&waiting, &own_run, None, &[], &[], &[], None).unwrap();
        assert!(alone.contains("Predecessor tasks: none\n"));
        assert!(!alone.contains("Carried over from run"));
    }

    /// The worker, resume and revise prompts show the verification commands
    /// as integrate's to run and send the session to the repository's own
    /// instructions for its checks, with the verification commands as the
    /// default (task 510).
    #[test]
    fn sessions_run_the_repository_checks_and_leave_the_verification_to_integrate() {
        let verified = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
        let own_run = run(7, RunStatus::Claimed, None);
        let checks = "the repository's instructions (AGENTS.md or CLAUDE.md) ask a worker to run";

        let worker = prompt(&verified, &own_run, None, &[], &[], &[], None).unwrap();
        assert!(worker.contains(
            "Verification commands (integrate runs them once after rebasing onto main; that run is the verification of record for the commit):\n[\n  \"make gate\"\n]\n"
        ));
        assert!(worker.contains(checks), "{worker}");
        assert!(worker.contains(
            "when the instructions name no such checks, run the verification commands above."
        ));
        assert!(!worker.contains("run in the worktree):"));
        let inheritance = Inheritance {
            run_id: RunId::new(RUN).unwrap(),
            base: CommitSha::try_from(SHA).unwrap(),
            head: SHA.into(),
            branch: None,
            receipt_path: None,
            summary: "earlier".into(),
        };
        let retried = prompt(&verified, &own_run, None, &[], &[], &[], Some(&inheritance)).unwrap();
        let (before, carried) = retried.split_once("Carried over from run").unwrap();
        assert!(before.contains(checks));
        assert!(carried.contains("rerun your checks in the worktree as above"));

        let default = r#"when the instructions name no such checks, run the verification commands ["make gate"]."#;
        let reproduce = "If the reason is a verification command that failed after integrate's rebase, you may also run that command in the worktree";
        for kind in [
            ResumeKind::Landing,
            ResumeKind::EvidenceMissing,
            ResumeKind::SentBack,
            ResumeKind::ScopeViolation,
            ResumeKind::Precheck,
            ResumeKind::Triage,
        ] {
            let request = ResumeRequest {
                main: CommitSha::try_from(SHA).unwrap(),
                reason: "why".into(),
                kind,
            };
            let text = resume_request(&verified, &own_run, &request, &[]).unwrap();
            assert!(text.contains(checks), "{kind:?}: {text}");
            assert!(text.contains(default), "{kind:?}: {text}");
            assert!(!text.contains("Rerun the verification commands"), "{text}");
            assert_eq!(
                text.contains(reproduce),
                kind == ResumeKind::Landing,
                "{kind:?}: {text}"
            );
        }

        let revise = revise_request(&verified, &own_run, 1, &["fix it".into()]).unwrap();
        assert!(revise.contains(&format!("2. {}", local_checks(r#"["make gate"]"#))));
        assert!(revise.contains(default), "{revise}");
    }

    /// A task as plan review reads it, with long fields of the size of this
    /// queue's (about 3 KB, task 591's measure of prompt 157).
    fn long_task(id: i64, status: TaskStatus, paths: &[&str]) -> Task {
        Task::restore(TaskRecord {
            id: TaskId::new(id),
            title: format!("task {id}"),
            description: format!("description of {id} ").repeat(100),
            acceptance: "acceptance ".repeat(60),
            verification_commands: vec!["cargo test".into()],
            required_evidence: Vec::new(),
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            priority: Default::default(),
            kind: None,
            status,
            goal_id: None,
            context: "context ".repeat(80),
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap()
    }

    /// The plan review prompt with `queued` ready tasks: the proposal's
    /// task 1000 touches `src/hot.rs`, and so does the ready task 5.
    fn plan_prompt(queued: i64, left_out: usize) -> (String, usize) {
        use crate::domain::{
            PlannerOrigin, PlannerOwner, ProposalRecord, ProposalStatus, related::RelatedTask,
        };
        let proposal = Proposal::restore(ProposalRecord {
            id: ProposalId::new(1),
            status: ProposalStatus::Submitted,
            owner: PlannerOwner {
                origin: PlannerOrigin::Person,
                workspace_id: None,
            },
            submitted_at: "now".into(),
            revise_count: 0,
            task_ids: vec![TaskId::new(1000)],
            goal_ids: Vec::new(),
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
        let tasks = [TaskDetail {
            task: long_task(1000, TaskStatus::Submitted, &["src/hot.rs"]),
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            duplicate_of: None,
            duplicates: Vec::new(),
            runs: Vec::new(),
            events: Vec::new(),
            processes: Vec::new(),
        }];
        let items: Vec<TaskListItem> = (1..=queued)
            .map(|id| {
                let paths: &[&str] = if id == 5 { &["src/*.rs"] } else { &[] };
                TaskListItem::new(
                    long_task(id, TaskStatus::Ready, paths),
                    vec![TaskId::new(1)],
                    Vec::new(),
                    None,
                    None,
                    true,
                )
            })
            .collect();
        let mut expected = BTreeMap::new();
        expected.insert(TaskId::new(1000), vec!["src/hot.rs".to_owned()]);
        for item in &items {
            let files = match item.id.as_i64() {
                5 => vec!["src/*.rs".to_owned()],
                6 => (0..30).map(|n| format!("src/other{n}.rs")).collect(),
                _ => vec!["src/cold.rs".to_owned()],
            };
            expected.insert(item.id, files);
        }
        let hotspot = |path: &str| ConflictHotspot {
            path: path.into(),
            conflicts: 3,
            tasks: 2,
            task_ids: Vec::new(),
            landings: Some(4),
            ratio: Some(0.75),
            last_conflict_at: "then".into(),
            state: "present",
            renamed_to: None,
            alert: true,
        };
        let candidates = [DuplicateCandidates {
            task_id: TaskId::new(1000),
            related: vec![RelatedTask {
                id: 7,
                status: "ready".into(),
                title: "task 7".into(),
                score: 1.0,
                clues: Vec::new(),
                duplicate_of: None,
            }],
            search: Vec::new(),
        }];
        let old_size = items
            .iter()
            .map(|item| serde_json::to_string(item).unwrap().len())
            .sum();
        let prompt = plan_review_prompt(&PlanReviewMaterial {
            proposal: &proposal,
            tasks: &tasks,
            goals: &[],
            lint: &[],
            others: &[],
            queued: &items,
            queued_left_out: left_out,
            expected: &expected,
            precedents: &[],
            hotspots: &[hotspot("src/hot.rs"), hotspot("src/cold.rs")],
            candidates: &candidates,
            repo_root: Path::new("/repo"),
        })
        .unwrap();
        (prompt, old_size)
    }

    #[test]
    fn plan_review_lists_the_queue_in_summary_and_in_full_only_what_it_meets() {
        let (prompt, _) = plan_prompt(10, 3);
        let lines: Vec<Value> = prompt
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let summary = |id: i64| {
            lines
                .iter()
                .find(|line| line["id"] == id && line.get("expected_files").is_some())
                .unwrap_or_else(|| panic!("no summary of {id} in {prompt}"))
        };
        let full: Vec<i64> = lines
            .iter()
            .filter(|line| line.get("description").is_some() && line["id"] != 1000)
            .map(|line| line["id"].as_i64().unwrap())
            .collect();
        // In full: the related candidate (7) and the task on the proposal's
        // hotspot (5); not the ones on the hotspot nobody of the proposal
        // touches.
        assert_eq!(full, [5, 7]);
        assert_eq!(summary(5)["full_text_below"], true);
        assert_eq!(summary(5)["paths"], json!(["src/*.rs"]));
        assert_eq!(summary(2).get("full_text_below"), None);
        assert_eq!(summary(2).get("description"), None);
        assert_eq!(summary(2)["expected_files"], json!(["src/cold.rs"]));
        assert_eq!(summary(2)["dependencies"], json!([1]));
        // A long list of expected files is cut, with the count left out.
        assert_eq!(
            summary(6)["expected_files"].as_array().unwrap().len(),
            SUMMARY_EXPECTED_FILES
        );
        assert_eq!(summary(6)["more_expected_files"], 20);
        // Each hotspot with the tasks expected to touch it.
        let hot = |path: &str| {
            lines
                .iter()
                .find(|line| line["path"] == path)
                .unwrap_or_else(|| panic!("no {path} in {prompt}"))
        };
        assert_eq!(hot("src/hot.rs")["proposal_tasks"], json!([1000]));
        assert_eq!(hot("src/hot.rs")["queued_tasks"], json!([5]));
        assert_eq!(hot("src/cold.rs")["proposal_tasks"], json!([]));
        assert_eq!(
            hot("src/cold.rs")["queued_tasks"].as_array().unwrap().len(),
            9
        );
        assert!(
            prompt.contains("(3 more ready or in-progress tasks, those of the lowest IDs, are left out of this list)"),
            "{prompt}"
        );
        assert!(prompt.contains("not pass but revise, saying in the reason which part to cut"));
        assert!(prompt.contains("for each hotspot whose proposal_tasks and queued_tasks are both non-empty, add a dependency"));
        let (prompt, _) = plan_prompt(4, 0);
        assert!(!prompt.contains("are left out of this list"), "{prompt}");
    }

    #[test]
    fn plan_review_prompt_of_a_queue_of_170_ready_tasks_is_a_fraction_of_their_full_text() {
        let (prompt, full_text) = plan_prompt(170, 0);
        println!(
            "prompt {} bytes, full text of the queue {full_text} bytes",
            prompt.len()
        );
        // Before task 591 the list alone was the full text of every task.
        assert!(full_text > 500_000, "{full_text}");
        assert!(
            prompt.len() * 8 < full_text,
            "{} bytes against {full_text}",
            prompt.len()
        );
    }
}
