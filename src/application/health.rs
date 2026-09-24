//! `status`, `doctor` and `recover` (ADR-0016, ADR-0024 decision 3): how
//! the supervisors and the unfinished runs stand, what waits for a person
//! (`attention`), and the recovery of an orphaned run. A run's health is
//! its lease, its registered processes and its files. Everything reads the
//! queue through [`Queue`]; liveness comes through [`ProcessControl`] and
//! the files through [`RunFiles`].

use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;

use super::{AskQuery, Clock, ProcessControl, Queue, RunFiles, TRIAGE_ASKER};
use crate::domain::{
    ASK_EVENT_KINDS, AskId, AskKind, Attention, AttentionNext, HEARTBEAT_TIMEOUT_SECS,
    LANDING_OPTIONS, ReasonCode, RunEvent, RunId, RunLease, RunProcess, RunStatus, SessionRole,
    SupervisorMode, SupervisorPulse, SupervisorRegistration, TRIAGE_OPTIONS, TaskId, TaskRun,
    TriageState, event_attention, heartbeat_stale, reason, run_attention, supervisor_attention,
    triage_state,
};

/// Health of one run's lease as `status` and `doctor` report it.
#[derive(Debug, Clone, Serialize)]
pub struct LeaseHealth {
    pub pid: u32,
    pub alive: bool,
    pub heartbeat_at: i64,
    pub heartbeat_age_secs: i64,
    pub stale: bool,
}

/// Health of one registered wrapper/agent process. `alive` is only checked
/// while the wrapper has not reported an exit, because a dead PID may be reused.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessHealth {
    pub role: String,
    pub pid: u32,
    pub alive: Option<bool>,
    pub heartbeat_at: i64,
    pub heartbeat_age_secs: i64,
    pub heartbeat_stale: bool,
    pub exited_at: Option<i64>,
    pub exit_code: Option<i32>,
}

/// One unfinished run. `blockers` lists why `recover` would refuse it; an
/// empty list means it is recoverable now. Only this run's own lease and
/// processes count; other runs never block it.
#[derive(Debug, Clone, Serialize)]
pub struct RunHealth {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub status: RunStatus,
    pub workspace_id: Option<String>,
    pub worktree_path: Option<String>,
    pub worktree_exists: Option<bool>,
    pub run_dir: Option<String>,
    pub run_dir_exists: Option<bool>,
    pub receipt_exists: Option<bool>,
    pub last_error: Option<String>,
    pub lease: Option<LeaseHealth>,
    pub processes: Vec<ProcessHealth>,
    pub blockers: Vec<String>,
    pub recoverable: bool,
}

impl RunHealth {
    /// The run in `doctor`'s default output: whether it can be recovered and
    /// where it is, with `blockers` counted (`blocker_count`) and the lease
    /// reduced to `lease_stale` (null without a lease).
    pub fn summary(&self) -> Value {
        json!({
            "run_id": self.run_id,
            "task_id": self.task_id,
            "status": self.status,
            "lease_stale": self.lease.as_ref().map(|lease| lease.stale),
            "recoverable": self.recoverable,
            "blocker_count": self.blockers.len(),
            "workspace_id": self.workspace_id,
            "worktree_path": self.worktree_path,
        })
    }
}

/// The health of `run` at `now`: a live process of the run, a fresh lease
/// or a live lease holder blocks its recovery.
pub fn run_health(
    run: &TaskRun,
    processes: &[RunProcess],
    lease: Option<LeaseHealth>,
    now: i64,
    control: &dyn ProcessControl,
    files: &dyn RunFiles,
) -> RunHealth {
    let mut blockers = Vec::new();
    let processes: Vec<ProcessHealth> = processes
        .iter()
        .map(|process| {
            let age = now - process.heartbeat_at;
            let alive = process
                .exited_at
                .is_none()
                .then(|| control.alive(process.pid));
            if alive == Some(true) {
                blockers.push(format!("{} pid {} is alive", process.role, process.pid));
            }
            ProcessHealth {
                role: process.role.clone(),
                pid: process.pid,
                alive,
                heartbeat_at: process.heartbeat_at,
                heartbeat_age_secs: age,
                heartbeat_stale: process.exited_at.is_none() && age > HEARTBEAT_TIMEOUT_SECS,
                exited_at: process.exited_at,
                exit_code: process.exit_code,
            }
        })
        .collect();
    if let Some(lease) = &lease {
        if !lease.stale {
            blockers.push(format!(
                "lease heartbeat is {}s old (limit {HEARTBEAT_TIMEOUT_SECS}s)",
                lease.heartbeat_age_secs
            ));
        }
        if lease.alive {
            blockers.push(format!("supervisor pid {} is alive", lease.pid));
        }
    }
    let exists = |path: Option<&str>| path.map(|p| files.exists(Path::new(p)));
    RunHealth {
        run_id: run.id().clone(),
        task_id: run.task_id(),
        status: run.status(),
        workspace_id: run.workspace_id().map(str::to_owned),
        worktree_path: run.worktree_path().map(str::to_owned),
        worktree_exists: exists(run.worktree_path()),
        run_dir: run.run_dir().map(str::to_owned),
        run_dir_exists: exists(run.run_dir()),
        receipt_exists: exists(run.receipt_path()),
        last_error: run.last_error().map(str::to_owned),
        lease,
        processes,
        recoverable: blockers.is_empty(),
        blockers,
    }
}

/// A process that owns runs, as `status` and `doctor` report it: a resident
/// `supervise` through its registration (`registered`, with `parallel` and
/// `started_at`), or an `integrate` process through the lease it holds
/// (`registered: false`). `run_ids` are the leases carrying its token, and
/// they share its heartbeat. `stale` is a registration or lease that no
/// working process stands behind: a dead pid or a heartbeat older than
/// `HEARTBEAT_TIMEOUT_SECS`. `mode` is how `up` started it (`launchd`, or
/// `in_cmux` with the `workspace_id` it runs in); a supervisor started by
/// hand and an `integrate` process have none. Nothing here is deleted
/// automatically.
#[derive(Debug, Clone, Serialize)]
pub struct SupervisorHealth {
    pub pid: u32,
    pub alive: bool,
    pub registered: bool,
    pub mode: Option<SupervisorMode>,
    pub workspace_id: Option<String>,
    /// The `dagq` version the registered process runs; `None` for a
    /// lease holder without a registration, or a registration older than
    /// the column (ADR-0014).
    pub binary_version: Option<String>,
    pub parallel: Option<u32>,
    pub started_at: Option<i64>,
    pub heartbeat_at: i64,
    pub heartbeat_age_secs: i64,
    pub stale: bool,
    pub run_ids: Vec<RunId>,
}

impl SupervisorHealth {
    /// The supervisor in `doctor`'s default output, one line's worth.
    pub fn summary(&self) -> Value {
        json!({
            "pid": self.pid,
            "alive": self.alive,
            "registered": self.registered,
            "mode": self.mode,
            "workspace_id": self.workspace_id,
            "binary_version": self.binary_version,
            "heartbeat_age_secs": self.heartbeat_age_secs,
            "stale": self.stale,
            "run_ids": self.run_ids,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub checked_at: i64,
    pub supervisors: Vec<SupervisorHealth>,
    pub runs: Vec<RunHealth>,
}

/// Characters of an ask's question `status` keeps before `…`.
const ASK_QUESTION_CHARS: usize = 200;

/// A reason or error is cut to this many characters in the attention.
const REASON_CHARS: usize = 300;

/// `status --role`: registered supervisors, lease holders and the
/// unfinished runs with their leases, without inspecting the runs'
/// processes, plus what waits for a person narrowed to what `role` acts on
/// (`attention`, ADR-0022; `None` is all of it), every open ask with its
/// question cut to 200 characters, and the newest event id (`cursor`) to
/// `watch` from (ADR-0016).
pub fn status(
    queue: &dyn Queue,
    control: &dyn ProcessControl,
    clock: &dyn Clock,
    role: Option<SessionRole>,
) -> Result<Value> {
    // Read before the state it describes, so a transition in between is
    // seen again by `watch --after cursor` rather than missed.
    let cursor = queue.latest_event_id()?;
    let now = clock.now();
    let registrations = queue.supervisors()?;
    let leases = queue.run_leases()?;
    let runs = queue
        .active_runs()?
        .into_iter()
        .map(|run| {
            let lease = leases
                .iter()
                .find(|l| l.run_id == *run.id())
                .map(|l| lease_health(l, now, control));
            let mut entry = json!({
                "run_id": run.id(),
                "task_id": run.task_id(),
                "status": run.status(),
                "workspace_id": run.workspace_id(),
                "worktree_path": run.worktree_path(),
                "lease": lease,
            });
            // Why it waits or failed (ADR-0034), for a run that has an error.
            if let Some(code) = reason::run_error_code(&run, &queue.run_events(run.id())?) {
                entry["last_error_code"] = json!(code);
            }
            Ok(entry)
        })
        .collect::<Result<Vec<_>>>()?;
    let asks = queue
        .asks(AskQuery {
            open: true,
            ..Default::default()
        })?
        .into_iter()
        .map(|ask| {
            json!({
                "id": ask.id,
                "kind": ask.kind,
                "question": truncate(&ask.question, ASK_QUESTION_CHARS)
                    .unwrap_or(ask.question),
                "task_id": ask.task_id,
                "run_id": ask.run_id,
                "asked_by": ask.asked_by,
                "age_secs": now - ask.created_at,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "checked_at": now,
        "supervisors": supervisors(&registrations, &leases, now, control),
        "runs": runs,
        "attention": attention(queue, &registrations, now, control)?
            .into_iter()
            .filter(|_| for_role(role))
            .collect::<Vec<_>>(),
        "asks": asks,
        "cursor": cursor,
    }))
}

/// `doctor`: with `full`, every registered supervisor and every unfinished
/// run with its lease, processes and paths; without it, one line's worth
/// per run and per supervisor ([`RunHealth::summary`],
/// [`SupervisorHealth::summary`]). Reads only.
pub fn doctor(
    queue: &dyn Queue,
    control: &dyn ProcessControl,
    files: &dyn RunFiles,
    clock: &dyn Clock,
    full: bool,
) -> Result<Value> {
    let now = clock.now();
    let registrations = queue.supervisors()?;
    let leases = queue.run_leases()?;
    let runs = queue
        .active_runs()?
        .into_iter()
        .map(|run| {
            let processes = queue.processes(run.id())?;
            let lease = leases
                .iter()
                .find(|l| l.run_id == *run.id())
                .map(|l| lease_health(l, now, control));
            Ok(run_health(&run, &processes, lease, now, control, files))
        })
        .collect::<Result<Vec<_>>>()?;
    let supervisors = supervisors(&registrations, &leases, now, control);
    if !full {
        return Ok(json!({
            "checked_at": now,
            "supervisors": supervisors.iter().map(SupervisorHealth::summary).collect::<Vec<_>>(),
            "runs": runs.iter().map(RunHealth::summary).collect::<Vec<_>>(),
        }));
    }
    Ok(serde_json::to_value(DoctorReport {
        checked_at: now,
        supervisors,
        runs,
    })?)
}

/// Mark an orphaned run `interrupted` (or a run whose `integrate` process
/// died `awaiting_integration` again) and drop its lease, after checking that
/// nothing registered for it is still alive. Never reruns, never deletes the
/// worktree or workspace, leaves the task `in_progress`, and does not touch
/// any other run.
pub fn recover(
    queue: &mut dyn Queue,
    control: &dyn ProcessControl,
    files: &dyn RunFiles,
    clock: &dyn Clock,
    id: &RunId,
) -> Result<Value> {
    let run = queue.run(id)?;
    ensure!(
        matches!(
            run.status(),
            RunStatus::Claimed
                | RunStatus::Starting
                | RunStatus::Running
                | RunStatus::Validating
                | RunStatus::Integrating
        ),
        "run {id} is {}; only unfinished runs can be recovered",
        run.status().as_str()
    );
    let now = clock.now();
    let lease = queue.run_lease(id)?.map(|l| lease_health(&l, now, control));
    let processes = queue.processes(run.id())?;
    let health = run_health(&run, &processes, lease, now, control, files);
    ensure!(
        health.recoverable,
        "refusing to recover run {id}: {}",
        health.blockers.join("; ")
    );
    let report = json!({"run": health});
    let run = queue.recover_run(run.id(), processes.len(), report)?;
    Ok(json!({"outcome": "recovered", "run": run}))
}

/// The health of one lease at `now`.
pub fn lease_health(lease: &RunLease, now: i64, control: &dyn ProcessControl) -> LeaseHealth {
    let age = now - lease.heartbeat_at;
    LeaseHealth {
        pid: lease.pid,
        alive: control.alive(lease.pid),
        heartbeat_at: lease.heartbeat_at,
        heartbeat_age_secs: age,
        stale: age > HEARTBEAT_TIMEOUT_SECS,
    }
}

/// Whether a lease no longer has a working process behind it: its pid is
/// dead or its heartbeat is older than `HEARTBEAT_TIMEOUT_SECS`.
fn lease_is_stale(lease: &RunLease, now: i64, control: &dyn ProcessControl) -> bool {
    !control.alive(lease.pid) || now - lease.heartbeat_at > HEARTBEAT_TIMEOUT_SECS
}

/// Registered supervisors in registration order, then any other lease
/// holder (an `integrate` process) in lease order; leases join by token.
pub fn supervisors(
    registrations: &[SupervisorRegistration],
    leases: &[RunLease],
    now: i64,
    control: &dyn ProcessControl,
) -> Vec<SupervisorHealth> {
    let health = |pid: u32, heartbeat_at: i64, registration: Option<&SupervisorRegistration>| {
        let alive = control.alive(pid);
        let age = now - heartbeat_at;
        SupervisorHealth {
            pid,
            alive,
            registered: registration.is_some(),
            mode: registration.and_then(|r| r.mode),
            workspace_id: registration.and_then(|r| r.workspace_id.clone()),
            binary_version: registration.and_then(|r| r.binary_version.clone()),
            parallel: registration.map(|r| r.parallel),
            started_at: registration.map(|r| r.started_at),
            heartbeat_at,
            heartbeat_age_secs: age,
            stale: heartbeat_stale(alive, age),
            run_ids: Vec::new(),
        }
    };
    let mut entries: Vec<(&str, SupervisorHealth)> = registrations
        .iter()
        .map(|r| (r.token.as_str(), health(r.pid, r.heartbeat_at, Some(r))))
        .collect();
    for lease in leases {
        let index = match entries.iter().position(|(token, _)| *token == lease.token) {
            Some(index) => index,
            None => {
                entries.push((
                    lease.token.as_str(),
                    health(lease.pid, lease.heartbeat_at, None),
                ));
                entries.len() - 1
            }
        };
        let entry = &mut entries[index].1;
        entry.run_ids.push(lease.run_id.clone());
        if !entry.registered {
            // Every lease of one process carries the same heartbeat; the
            // freshest one stands for the process.
            let age = now - lease.heartbeat_at;
            if age < entry.heartbeat_age_secs {
                entry.heartbeat_at = lease.heartbeat_at;
                entry.heartbeat_age_secs = age;
                entry.stale = heartbeat_stale(entry.alive, age);
            }
        }
    }
    entries.into_iter().map(|(_, health)| health).collect()
}

/// The health of every registered supervisor, in registration order.
pub fn pulses(
    registrations: &[SupervisorRegistration],
    now: i64,
    control: &dyn ProcessControl,
) -> Vec<SupervisorPulse> {
    registrations
        .iter()
        .map(|r| SupervisorPulse::judge(r, control.alive(r.pid), now))
        .collect()
}

/// Whether attention is for `role`: all of it is the inbox's
/// ([`crate::domain::ATTENTION_ROLE`]), and without a role everything is
/// shown. The supervisors' health follows the same rule: the person
/// restarts them.
pub fn for_role(role: Option<SessionRole>) -> bool {
    role.is_none_or(|role| role == crate::domain::ATTENTION_ROLE)
}

/// `text` cut to `limit` characters with `…` appended, or `None` when it fits.
pub fn truncate(text: &str, limit: usize) -> Option<String> {
    let mut chars = text.char_indices();
    let (end, _) = chars.nth(limit)?;
    Some(format!("{}…", &text[..end]))
}

/// `text` cut to [`REASON_CHARS`] characters, with `…` when it was longer.
pub fn truncate_reason(text: &str) -> String {
    truncate(text, REASON_CHARS).unwrap_or_else(|| text.to_owned())
}

/// One event as the inbox reads it: the row's ids and kind, and from the
/// payload only `status`, `exit_code`, the reason `code` and a truncated `reason` (from
/// `reason`, `message` or `error`). Paths and receipts are left out.
/// An attention event also carries its `next`.
pub fn compact_event(event: &RunEvent) -> Value {
    let mut value = json!({"id": event.id, "kind": event.kind});
    let object = value.as_object_mut().expect("object literal");
    if let Some(task_id) = event.task_id {
        object.insert("task_id".into(), json!(task_id));
    }
    if let Some(goal_id) = event.goal_id {
        object.insert("goal_id".into(), json!(goal_id));
    }
    if let Some(run_id) = &event.run_id {
        object.insert("run_id".into(), json!(run_id));
    }
    let payload = &event.payload;
    if let Some(status) = payload.get("status").or_else(|| payload.get("to")) {
        object.insert("status".into(), status.clone());
    }
    if let Some(code) = payload.get("exit_code") {
        object.insert("exit_code".into(), code.clone());
    }
    if let Some(ask_id) = payload.get("ask_id") {
        object.insert("ask_id".into(), ask_id.clone());
    }
    if let Some(code) = payload.get(reason::CODE_KEY) {
        object.insert(reason::CODE_KEY.into(), code.clone());
    }
    if let Some(reason) = ["reason", "message", "error"]
        .iter()
        .find_map(|key| payload.get(*key).and_then(Value::as_str))
    {
        object.insert("reason".into(), json!(truncate_reason(reason)));
    }
    if let Some(next) = event_attention(&event.kind, payload) {
        object.insert("next".into(), json!(next));
    }
    object.insert("created_at".into(), json!(event.created_at));
    value
}

/// What waits for a person now: stale or missing supervisors first,
/// then the latest run of every `in_progress` task that rests where only a
/// person or the supervisor moves it on or that is unfinished without a lease,
/// then every landed run whose push of `main` failed with no successful push
/// since, then every ask nobody closed: an open one as `ask_opened` for the
/// inbox, an answered one as `ask_answered` for the person to act on
/// through the inbox (ADR-0022, ADR-0024 decision 6).
/// `kind` is the event that brought the run there (for a run without
/// a lease, its latest `runtime_error`).
pub fn attention(
    queue: &dyn Queue,
    registrations: &[SupervisorRegistration],
    now: i64,
    control: &dyn ProcessControl,
) -> Result<Vec<Attention>> {
    let mut attention = supervisor_attention(&pulses(registrations, now, control));
    for mut run in queue.latest_runs_in_progress()? {
        let leased = queue.run_lease(run.id())?.is_some();
        if !leased {
            // A supervisor leaves the unfinished statuses before it releases
            // the lease, so a run read before a release and its lease read
            // after it would look abandoned: judge it by its status now.
            run = queue.run(run.id())?;
        }
        // A run whose review raised a concern waits in its
        // `approve_landing` ask, which is the attention (ADR-0027).
        if run.status() == RunStatus::AwaitingIntegration
            && !leased
            && queue.has_unclosed_ask(run.id(), AskKind::ApproveLanding)?
        {
            continue;
        }
        let events = queue.run_events(run.id())?;
        let exit_pending = events
            .iter()
            .rev()
            .find(|e| matches!(e.kind.as_str(), "exit_request_timed_out" | "session_exited"))
            .is_some_and(|e| e.kind == "exit_request_timed_out");
        let Some(next) = run_attention(run.status(), exit_pending, false, leased) else {
            continue;
        };
        // A failed or interrupted run is the supervisor's triage until it
        // finished (its verdict moved the task or the run on, or its ask is
        // the attention) or failed (a person's).
        let next = match (next, triage_state(&events)) {
            (AttentionNext::Triaging, TriageState::Finished) => continue,
            (AttentionNext::Triaging, TriageState::Failed) => AttentionNext::TriageByHand,
            (next, _) => next,
        };
        let kind = events
            .iter()
            .rev()
            .find(|e| match next {
                // The error the owner gave up with, whatever its payload.
                AttentionNext::RecoverRun => e.kind == "runtime_error",
                // Whatever parked the run for a session last.
                AttentionNext::Resuming => {
                    e.payload.get("status").and_then(Value::as_str)
                        == Some(RunStatus::NeedsSession.as_str())
                }
                // An ask about the run is its own attention, not the run's.
                _ => {
                    !ASK_EVENT_KINDS.contains(&e.kind.as_str())
                        && event_attention(&e.kind, &e.payload).is_some()
                }
            })
            .map_or_else(|| run.status().as_str().to_owned(), |e| e.kind.clone());
        // After a failed headless review the run is a person's to review.
        let next = match next {
            AttentionNext::ReviewAndIntegrate if kind == "review_failed" => {
                AttentionNext::ReviewByHand
            }
            next => next,
        };
        attention.push(Attention {
            run_id: Some(run.id().clone()),
            task_id: Some(run.task_id()),
            pid: None,
            ask_id: None,
            status: run.status().as_str().into(),
            kind,
            last_error: run.last_error().map(truncate_reason),
            last_error_code: reason::run_error_code(&run, &events),
            next,
        });
    }
    for run in queue.runs_with_pending_push()? {
        let Some(next) = run_attention(run.status(), false, true, false) else {
            continue;
        };
        let error = queue
            .run_events(run.id())?
            .into_iter()
            .rev()
            .find(|e| e.kind == "push_failed")
            .and_then(|e| {
                e.payload
                    .get("error")
                    .and_then(Value::as_str)
                    .map(truncate_reason)
            });
        attention.push(Attention {
            run_id: Some(run.id().clone()),
            task_id: Some(run.task_id()),
            pid: None,
            ask_id: None,
            status: run.status().as_str().into(),
            kind: "push_failed".into(),
            last_error_code: error.as_ref().map(|_| ReasonCode::PushFailed),
            last_error: error,
            next,
        });
    }
    for ask in queue.asks(AskQuery::default())? {
        let (status, kind, next) = if ask.is_open() {
            (
                "open",
                "ask_opened",
                AttentionNext::AnswerAsk { ask_id: ask.id },
            )
        } else if ask.kind == AskKind::WorkerQuestion
            && let Some(run_id) = ask.run_id.as_ref()
        {
            // The supervisor holding a running worker's lease types the
            // answer into its terminal; a failed send, a run no longer
            // running or one nobody supervises leaves it to the inbox.
            let failed = queue.run_events(run_id)?.iter().any(|e| {
                e.kind == "ask_delivery_failed"
                    && e.payload
                        .get("ask_id")
                        .and_then(Value::as_i64)
                        .map(AskId::new)
                        == Some(ask.id)
            });
            if failed {
                (
                    "answered",
                    "ask_delivery_failed",
                    AttentionNext::DeliverAnswer { ask_id: ask.id },
                )
            } else if queue.run(run_id)?.status() == RunStatus::Running
                && queue
                    .run_lease(run_id)?
                    .is_some_and(|lease| !lease_is_stale(&lease, now, control))
            {
                (
                    "answered",
                    "ask_answered",
                    AttentionNext::DeliveringAnswer { ask_id: ask.id },
                )
            } else {
                (
                    "answered",
                    "ask_answered",
                    AttentionNext::DeliverAnswer { ask_id: ask.id },
                )
            }
        } else if ask.kind == AskKind::Decide
            && ask.asked_by == TRIAGE_ASKER
            && let Some(run_id) = ask.run_id.as_ref()
            && matches!(
                queue.run(run_id)?.status(),
                RunStatus::Failed | RunStatus::Interrupted
            )
            && ask
                .answer
                .as_deref()
                .is_some_and(|answer| TRIAGE_OPTIONS.contains(&answer.trim()))
        {
            // The supervisor retries, resumes or cancels the triaged run.
            (
                "answered",
                "ask_answered",
                AttentionNext::ApplyingAnswer { ask_id: ask.id },
            )
        } else if ask.kind == AskKind::ApproveLanding
            && let Some(run_id) = ask.run_id.as_ref()
            && queue.run(run_id)?.status() == RunStatus::AwaitingIntegration
            && ask
                .answer
                .as_deref()
                .is_some_and(|answer| LANDING_OPTIONS.contains(&answer.trim()))
        {
            // The supervisor lands, sends back or cancels the run itself.
            (
                "answered",
                "ask_answered",
                AttentionNext::ApplyingAnswer { ask_id: ask.id },
            )
        } else {
            (
                "answered",
                "ask_answered",
                AttentionNext::ReadAnswer { ask_id: ask.id },
            )
        };
        attention.push(Attention {
            run_id: ask.run_id,
            task_id: ask.task_id,
            pid: None,
            ask_id: Some(ask.id),
            status: status.into(),
            kind: kind.into(),
            last_error: None,
            last_error_code: None,
            next,
        });
    }
    Ok(attention)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    /// Pid 1 is alive, every other pid is dead.
    struct OnlyOne;

    impl ProcessControl for OnlyOne {
        fn alive(&self, pid: u32) -> bool {
            pid == 1
        }
        fn terminate(&self, _: u32) -> Result<()> {
            Ok(())
        }
        fn interrupt(&self, _: u32) -> Result<()> {
            Ok(())
        }
        fn kill(&self, _: u32) -> Result<()> {
            Ok(())
        }
    }

    fn registration(token: &str, pid: u32, heartbeat_at: i64) -> SupervisorRegistration {
        SupervisorRegistration {
            token: token.into(),
            pid,
            parallel: 2,
            started_at: 0,
            heartbeat_at,
            mode: Some(SupervisorMode::Launchd),
            workspace_id: None,
            binary_version: Some("1.0.0".into()),
        }
    }

    fn lease(run: &str, token: &str, pid: u32, heartbeat_at: i64) -> RunLease {
        RunLease {
            run_id: RunId::new(run).unwrap(),
            token: token.into(),
            pid,
            heartbeat_at,
        }
    }

    #[test]
    fn supervisors_join_leases_by_token_and_take_liveness_from_the_port() {
        let now = 1_000;
        let health = supervisors(
            &[registration("a", 1, now - 5), registration("b", 2, now - 5)],
            &[
                lease("r1", "a", 1, now - 5),
                // An `integrate` process: two leases, the freshest counts.
                lease("r2", "c", 3, now - 100),
                lease("r3", "c", 3, now - 10),
            ],
            now,
            &OnlyOne,
        );
        assert_eq!(health.len(), 3);
        assert!(health[0].alive && !health[0].stale && health[0].registered);
        assert_eq!(health[0].run_ids, vec![RunId::new("r1").unwrap()]);
        // Dead pid: stale whatever its heartbeat.
        assert!(!health[1].alive && health[1].stale);
        assert!(!health[2].registered);
        assert_eq!(health[2].heartbeat_age_secs, 10);
        assert_eq!(health[2].run_ids.len(), 2);
        assert!(health[2].stale, "a dead lease holder is stale");
    }

    #[test]
    fn a_lease_is_stale_by_age_and_its_holder_is_judged_by_the_port() {
        let fresh = lease_health(&lease("r", "a", 1, 90), 100, &OnlyOne);
        assert!(fresh.alive && !fresh.stale);
        let old = lease_health(
            &lease("r", "a", 2, 100 - HEARTBEAT_TIMEOUT_SECS - 1),
            100,
            &OnlyOne,
        );
        assert!(!old.alive && old.stale);
        assert!(lease_is_stale(&lease("r", "a", 2, 100), 100, &OnlyOne));
        assert!(!lease_is_stale(&lease("r", "a", 1, 100), 100, &OnlyOne));
    }

    #[test]
    fn attention_is_the_inboxs_and_reasons_are_cut() {
        assert!(for_role(None));
        assert!(for_role(Some(SessionRole::Inbox)));
        assert!(!for_role(Some(SessionRole::Planner)));
        assert_eq!(truncate_reason("short"), "short");
        let long = "x".repeat(REASON_CHARS + 5);
        assert_eq!(
            truncate_reason(&long),
            format!("{}…", "x".repeat(REASON_CHARS))
        );
    }
}
