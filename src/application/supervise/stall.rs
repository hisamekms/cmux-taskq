//! A worker's session idle without a receipt (ADR-0043 decision 1): the
//! [`StallWatch`] of the first session nudges it once and, if it stays
//! idle, raises it to the inbox as a `stalled` ask, recording how each
//! detection ended (`stall_resolved`, decision 3).
//!
//! Only the first session ([`SessionWatch`]) needs it. A resumed session
//! and one sent a revise or a conflict request already end their phase
//! when they go idle without the receipt they were asked for (`/exit`, or
//! the `approve_landing` ask), and at the resume timeout otherwise.

use super::*;

/// The setting the idle detections are judged by.
pub(super) const IDLE_THRESHOLD: &str = "idle_without_receipt_secs";

/// The setting the `long_background` alert is judged by.
pub(super) const BACKGROUND_THRESHOLD: &str = "background_alert_secs";

/// The options of a `stalled` ask: leave the session alone and ask again
/// if it stays idle, or have a person step in.
pub(super) const STALLED_OPTIONS: [&str; 2] = ["wait", "intervene"];

/// The answers the runtime writes into a `stalled` ask it closes.
pub(super) const STALL_MOVED_CLOSED: &str = "the session moved on; closed by the runtime";

pub(super) const STALL_EXITED_CLOSED: &str = "the session exited; closed by the runtime";

/// The phase whose idle the watch judges.
const PHASE: &str = "session";

/// The nudge sent in this phase (at most one).
#[derive(Debug, Clone, Copy)]
struct Nudge {
    /// When it was recorded, on the files' wall clock.
    at: SystemTime,
    /// How long the session had been idle then.
    detected_after_secs: i64,
    /// Its `stall_resolved` is recorded.
    settled: bool,
}

/// The `stalled` ask of this phase that nobody closed.
#[derive(Debug, Clone, Copy)]
struct Asked {
    id: AskId,
    /// When it was opened: an idle marker newer than this is a turn the
    /// session took since.
    at: SystemTime,
    detected_after_secs: i64,
    /// Its answer is applied (its `stall_resolved` is recorded).
    applied: bool,
    /// The setting of the detection it came from: [`IDLE_THRESHOLD`], or
    /// `background_alert_secs` for a recovery job's escalation.
    threshold: &'static str,
}

/// Receipt-less idle of one session, judged each tick.
#[derive(Debug, Clone, Default)]
pub(super) struct StallWatch {
    nudge: Option<Nudge>,
    /// The latest text the supervisor typed into the session: a marker no
    /// newer is not the end of a turn that answered it.
    last_input: Option<SystemTime>,
    asked: Option<Asked>,
    /// The answer `wait` restarts the count here.
    wait_from: Option<SystemTime>,
    /// After `intervene` (or an ask a person closed), no ask until the
    /// session ends a turn after this.
    held: Option<SystemTime>,
}

/// Seconds from `from` to `to`, zero when `to` is earlier.
fn secs_between(from: SystemTime, to: SystemTime) -> i64 {
    to.duration_since(from)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The run's `stalled` ask `id`, or its latest one, closed or not.
fn stalled_ask(
    queue: &dyn Queue,
    run: &RunId,
    id: Option<AskId>,
) -> Result<Option<crate::domain::Ask>> {
    Ok(queue
        .asks(crate::application::AskQuery {
            all: true,
            ..Default::default()
        })?
        .into_iter()
        .rfind(|ask| {
            ask.kind == AskKind::Stalled
                && ask.run_id.as_ref() == Some(run)
                && id.is_none_or(|id| ask.id == id)
        }))
}

fn at_unix(secs: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).unwrap_or(0))
}

fn at_event(event: &crate::domain::RunEvent) -> Option<SystemTime> {
    crate::domain::stats::timestamp_millis(&event.created_at)
        .map(|ms| UNIX_EPOCH + Duration::from_millis(u64::try_from(ms).unwrap_or(0)))
}

impl StallWatch {
    /// Rebuild the watch of an adopted run from its events and its
    /// `stalled` ask, so a nudge or an ask is never repeated.
    pub(super) fn adopt(queue: &dyn Queue, run: &TaskRun) -> Result<Self> {
        let events = queue.run_events(run.id())?;
        let resolved = |detection: &str, ask: Option<AskId>| {
            events.iter().any(|e| {
                e.kind == "stall_resolved"
                    && e.payload["phase"] == PHASE
                    && e.payload["detection"] == detection
                    && ask.is_none_or(|id| e.payload["ask_id"] == json!(id))
            })
        };
        let mut watch = Self::default();
        if let Some(event) = events
            .iter()
            .rev()
            .find(|e| e.kind == "stall_nudged" && e.payload["phase"] == PHASE)
        {
            let at = at_event(event).unwrap_or(UNIX_EPOCH);
            watch.nudge = Some(Nudge {
                at,
                detected_after_secs: event.payload["idle_secs"].as_i64().unwrap_or(0),
                settled: resolved("nudge", None),
            });
            watch.input_sent(at);
        }
        // An answer the previous supervisor typed is an input too.
        if let Some(at) = events
            .iter()
            .rev()
            .find(|e| e.kind == "ask_delivered")
            .and_then(at_event)
        {
            watch.input_sent(at);
        }
        let latest_outcome = |outcome: &str| {
            events
                .iter()
                .rev()
                .find(|e| {
                    e.kind == "stall_resolved"
                        && e.payload["phase"] == PHASE
                        && e.payload["outcome"] == outcome
                })
                .and_then(at_event)
        };
        // A person stepped in on an ask closed since.
        watch.held = latest_outcome("answered_intervene");
        watch.wait_from = latest_outcome("answered_wait");
        // Times in the ask are whole seconds: a marker in the same second
        // is taken as older. The latest ask closed without its outcome
        // recorded was closed while no supervisor watched: its `wait`
        // counts again from its close, anything else holds.
        if let Some(ask) = stalled_ask(queue, run.id(), None)?
            && let Some(closed) = ask.closed_at
            && !resolved("ask", Some(ask.id))
        {
            if ask.answer.as_deref().map(str::trim) == Some("wait") {
                watch.wait_from = watch.wait_from.max(Some(at_unix(closed)));
            } else {
                watch.held = watch.held.max(Some(at_unix(closed + 1)));
            }
            if let Some(nudge) = &mut watch.nudge {
                nudge.settled = true;
            }
        }
        if let Some(ask) = queue.unclosed_stalled_ask(run.id())? {
            let applied = ask.answered_at.is_some() && resolved("ask", Some(ask.id));
            // A recovery job's escalation names its ask (ADR-0047).
            let recovery = events
                .iter()
                .any(|e| e.kind == "recovery_finished" && e.payload["ask_id"] == json!(ask.id));
            watch.asked = Some(Asked {
                id: ask.id,
                at: at_unix(ask.created_at + 1),
                detected_after_secs: 0,
                applied,
                threshold: if recovery {
                    BACKGROUND_THRESHOLD
                } else {
                    IDLE_THRESHOLD
                },
            });
            if applied {
                watch.held = watch.held.max(ask.answered_at.map(|at| at_unix(at + 1)));
            }
            // An ask follows the nudge, which escalated to it.
            if let Some(nudge) = &mut watch.nudge {
                nudge.settled = true;
            }
        }
        Ok(watch)
    }

    /// The supervisor typed a text into the session at `at`.
    pub(super) fn input_sent(&mut self, at: SystemTime) {
        self.last_input = Some(self.last_input.map_or(at, |last| last.max(at)));
    }

    /// Whether the session ended a turn (its idle marker `marker`) since the
    /// last text the supervisor typed.
    pub(super) fn turn_since_input(&self, marker: SystemTime) -> bool {
        self.last_input.is_none_or(|at| marker > at)
    }

    /// A recovery job escalated the alert of `threshold` to the `stalled`
    /// ask `id` (ADR-0047 decision 40): the watch follows it like its own,
    /// closing it once the session moves on and applying its answer.
    pub(super) fn escalated(
        &mut self,
        id: AskId,
        at: SystemTime,
        detected_after_secs: i64,
        threshold: &'static str,
    ) {
        self.asked = Some(Asked {
            id,
            at,
            detected_after_secs,
            applied: false,
            threshold,
        });
    }

    /// Record how a detection ended, once.
    #[allow(clippy::too_many_arguments)]
    fn resolved(
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        detection: &str,
        ask: Option<(AskId, &'static str)>,
        detected_after_secs: i64,
        detected_at: SystemTime,
        outcome: &str,
    ) -> Result<()> {
        let threshold = ask.map_or(IDLE_THRESHOLD, |(_, threshold)| threshold);
        let mut payload = json!({
            "phase": PHASE,
            "detection": detection,
            "threshold": threshold,
            "threshold_secs": if threshold == BACKGROUND_THRESHOLD {
                sv.stall.background_alert_secs
            } else {
                sv.stall.idle_without_receipt_secs
            },
            "detected_after_secs": detected_after_secs,
            "outcome": outcome,
            "resolved_after_secs": secs_between(detected_at, sv.files.now()),
        });
        if let Some((id, _)) = ask {
            payload["ask_id"] = json!(id);
        }
        sv.queue
            .record_runtime_event(run.id(), "stall_resolved", payload)?;
        info!(run_id = %run.id(), "stall of {} ({detection}) ended: {outcome}", run.id());
        Ok(())
    }

    /// The session wrote its receipt (`receipt`) or asked a
    /// `worker_question`: a nudge not settled yet resolved it, and a
    /// `stalled` ask is closed (resolved by itself if nobody answered it).
    pub(super) fn settle(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        receipt: bool,
    ) -> Result<()> {
        if self.nudge.is_none_or(|n| n.settled) && self.asked.is_none() {
            return Ok(());
        }
        if !receipt && !sv.queue.has_unclosed_worker_question(run.id())? {
            return Ok(());
        }
        if let Some(nudge) = &mut self.nudge
            && !nudge.settled
        {
            nudge.settled = true;
            let nudge = *nudge;
            Self::resolved(
                sv,
                run,
                "nudge",
                None,
                nudge.detected_after_secs,
                nudge.at,
                "resolved_by_nudge",
            )?;
        }
        self.close(sv, run, STALL_MOVED_CLOSED, "resolved_by_itself")
    }

    /// The session ended: whatever is not settled ends with the run.
    pub(super) fn ended(&mut self, sv: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        if let Some(nudge) = &mut self.nudge
            && !nudge.settled
        {
            nudge.settled = true;
            let nudge = *nudge;
            Self::resolved(
                sv,
                run,
                "nudge",
                None,
                nudge.detected_after_secs,
                nudge.at,
                "run_ended",
            )?;
        }
        self.close(sv, run, STALL_EXITED_CLOSED, "run_ended")
    }

    /// Close the run's `stalled` ask, recording `outcome` for one whose
    /// answer was not applied.
    fn close(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        answer: &str,
        outcome: &str,
    ) -> Result<()> {
        let Some(asked) = self.asked.take() else {
            return Ok(());
        };
        for ask in sv.queue.close_stalled_asks(run.id(), answer)? {
            info!(ask_id = %ask.id, run_id = %run.id(), "closed the stalled ask {} of {}: {answer}", ask.id, run.id());
        }
        if !asked.applied {
            Self::resolved(
                sv,
                run,
                "ask",
                Some((asked.id, asked.threshold)),
                asked.detected_after_secs,
                asked.at,
                outcome,
            )?;
        }
        Ok(())
    }

    /// One observation of a session with a fresh wrapper, no receipt and no
    /// `/exit` requested. `dialog` is a `prompt_waiting` not cleared.
    /// Returns what was typed into the session, for the check that it was
    /// taken ([`StartCheck`]).
    pub(super) fn poll(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle_marker: &Path,
        dialog: bool,
    ) -> Result<Option<StartCheck>> {
        let now = sv.files.now();
        let idle = IdleMarker::read(&*sv.files, sv.signals, idle_marker)?;
        let marker = idle.as_ref().map(IdleMarker::modified);
        if self.asked.is_some() {
            self.watch_ask(sv, run, marker, now)?;
            return Ok(None);
        }
        let Some(idle) = idle else {
            return Ok(None);
        };
        let modified = idle.modified();
        // The session has not ended a turn since the last text it was sent,
        // or since a person stepped in.
        if self.last_input.is_some_and(|at| modified <= at)
            || self.held.is_some_and(|at| modified <= at)
        {
            return Ok(None);
        }
        let from = self.wait_from.map_or(modified, |at| at.max(modified));
        let threshold = sv.stall.idle_without_receipt_secs;
        if secs_between(from, now) < threshold {
            return Ok(None);
        }
        // A dialog or an ask the session waits at is not a stall.
        if dialog
            || sv.queue.has_unclosed_worker_question(run.id())?
            || sv.queue.has_unclosed_ask(run.id(), AskKind::AnswerPrompt)?
        {
            return Ok(None);
        }
        // A session stopped at a login that ran out waits for a person to
        // log in, in the queue's one authentication ask (ADR-0047 decision
        // 42), not for a nudge or a stalled ask of its own.
        if sv.queue.hold_of(run.id())?.is_some() {
            return Ok(None);
        }
        if let Ok(screen) = sv.cmux.capture(workspace)
            && sv.signals.auth_required(&screen)
            && raise_auth(sv, run, workspace, &screen)?
        {
            return Ok(None);
        }
        let idle_secs = secs_between(modified, now);
        match self.nudge {
            None => self.send_nudge(sv, run, workspace, &idle, idle_secs, now),
            Some(nudge) => {
                self.open_ask(sv, run, workspace, &idle, idle_secs, nudge, now)?;
                Ok(None)
            }
        }
    }

    /// Record `stall_nudged` and type the nudge, once in the phase. A nudge
    /// that could not be typed is noted; the ask follows on the next tick.
    fn send_nudge(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle: &IdleMarker,
        idle_secs: i64,
        now: SystemTime,
    ) -> Result<Option<StartCheck>> {
        let background = idle.background_tasks();
        sv.queue.record_runtime_event(
            run.id(),
            "stall_nudged",
            json!({
                "phase": PHASE,
                "idle_secs": idle_secs,
                "threshold_secs": sv.stall.idle_without_receipt_secs,
                "background_running": idle.background_running(),
                "background_tasks": background,
                "workspace_id": workspace,
            }),
        )?;
        self.nudge = Some(Nudge {
            at: now,
            detected_after_secs: idle_secs,
            settled: false,
        });
        let text = stall_nudge(run, idle_secs, background)?;
        let sent_at = sv.files.now();
        match submit(sv, run, workspace, Input::Text(&text), "nudge") {
            Ok(submission) => {
                self.input_sent(sent_at);
                info!(run_id = %run.id(), "run {} was idle without a receipt for {idle_secs}s; nudged it in workspace {workspace}", run.id());
                Ok(Some(StartCheck::new("nudge", &text, sent_at, &submission)))
            }
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "the nudge of {} could not be typed into workspace {workspace}: {error:#}; asking the inbox instead", run.id());
                Ok(None)
            }
        }
    }

    /// Raise the session, still idle without a receipt after its nudge, as
    /// a `stalled` ask to the inbox.
    #[allow(clippy::too_many_arguments)]
    fn open_ask(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle: &IdleMarker,
        idle_secs: i64,
        nudge: Nudge,
        now: SystemTime,
    ) -> Result<()> {
        let background = match idle.background_tasks() {
            [] if idle.background_running() => {
                "background work was running (not listed)".to_owned()
            }
            [] => "no background task was running".to_owned(),
            tasks => tasks
                .iter()
                .map(|t| format!("- {} ({}): {}", t.description, t.id, t.command))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        let screen = match sv.cmux.capture(workspace) {
            Ok(screen) => sv.signals.screen_excerpt(&screen),
            Err(error) => format!("(the screen could not be read: {error:#})"),
        };
        let question = format!(
            "The session of run {run_id} (task {task_id}) in workspace {workspace} has been idle without a receipt for {idle_secs}s (reason: idle_without_receipt, phase: {PHASE}), although the supervisor nudged it {nudged}s ago to write its receipt, ask a worker_question or say what it waits for. Answer `wait` to leave the session alone (the supervisor asks again if it stays idle for another {threshold}s), or `intervene` to step in yourself (read the screen, stop or check its background work, type an instruction, or stop the run and recover it; see the dagq-recover skill). This ask closes itself once the session moves on.\n\nBackground tasks when it stopped:\n{background}\n\nLast lines of the screen:\n{screen}",
            run_id = run.id(),
            task_id = run.task_id(),
            nudged = secs_between(nudge.at, now),
            threshold = sv.stall.idle_without_receipt_secs,
        );
        let outcome = ask::ask(
            &mut *sv.queue,
            &sv.layout.repo_root,
            NewAsk {
                kind: AskKind::Stalled,
                task_id: Some(run.task_id()),
                run_id: Some(run.id().clone()),
                question,
                options: STALLED_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
                asked_by: SessionRole::Supervisor.as_str().into(),
                reason_category: AskReason::RecoveryFailed,
            },
            sv.cmux,
        )?;
        let id = AskId::new(outcome["id"].as_i64().context("ask returned no id")?);
        warn!(ask_id = %id, run_id = %run.id(), "run {} stays idle without a receipt after its nudge; stalled ask {id} (notified: {})", run.id(), outcome["notified"]);
        self.asked = Some(Asked {
            id,
            at: now,
            detected_after_secs: idle_secs,
            applied: false,
            threshold: IDLE_THRESHOLD,
        });
        if !nudge.settled {
            self.nudge = Some(Nudge {
                settled: true,
                ..nudge
            });
            Self::resolved(
                sv,
                run,
                "nudge",
                None,
                nudge.detected_after_secs,
                nudge.at,
                "escalated",
            )?;
        }
        Ok(())
    }

    /// Follow the `stalled` ask: close it when the session ended a turn
    /// since (by itself, or after a person stepped in), and apply its
    /// answer: `wait` restarts the count and closes it; anything else is a
    /// person stepping in, and no ask follows until the session ends a turn
    /// after it.
    fn watch_ask(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        marker: Option<SystemTime>,
        now: SystemTime,
    ) -> Result<()> {
        let Some(asked) = self.asked else {
            return Ok(());
        };
        let Some(ask) = sv.queue.unclosed_stalled_ask(run.id())? else {
            // Someone closed it: its answer `wait` counts again, anything
            // else (or none) is a person who took the session over.
            self.asked = None;
            let wait = stalled_ask(&*sv.queue, run.id(), Some(asked.id))?
                .is_some_and(|ask| ask.answer.as_deref().map(str::trim) == Some("wait"));
            if wait {
                self.wait_from = Some(now);
            } else {
                self.held = Some(now);
            }
            if !asked.applied {
                Self::resolved(
                    sv,
                    run,
                    "ask",
                    Some((asked.id, asked.threshold)),
                    asked.detected_after_secs,
                    asked.at,
                    if wait {
                        "answered_wait"
                    } else {
                        "answered_intervene"
                    },
                )?;
            }
            return Ok(());
        };
        let moved = |since: SystemTime| marker.is_some_and(|m| m > since);
        match ask.answer.as_deref().map(str::trim) {
            None if moved(asked.at) => {
                self.close(sv, run, STALL_MOVED_CLOSED, "resolved_by_itself")?;
            }
            None => (),
            Some(_) if asked.applied => {
                if self.held.is_none_or(moved) {
                    self.close(sv, run, STALL_MOVED_CLOSED, "resolved_by_itself")?;
                }
            }
            Some("wait") => {
                // Closed first: a supervisor that stops in between leaves
                // a closed `wait` its adopter reads as one.
                sv.queue.close_ask(ask.id)?;
                Self::resolved(
                    sv,
                    run,
                    "ask",
                    Some((ask.id, asked.threshold)),
                    asked.detected_after_secs,
                    asked.at,
                    "answered_wait",
                )?;
                self.asked = None;
                self.wait_from = Some(now);
                info!(ask_id = %ask.id, run_id = %run.id(), "stalled ask {} of {} answered wait; counting its idle again", ask.id, run.id());
            }
            Some(_) => {
                Self::resolved(
                    sv,
                    run,
                    "ask",
                    Some((ask.id, asked.threshold)),
                    asked.detected_after_secs,
                    asked.at,
                    "answered_intervene",
                )?;
                self.asked = Some(Asked {
                    applied: true,
                    ..asked
                });
                self.held = Some(now);
                info!(ask_id = %ask.id, run_id = %run.id(), "stalled ask {} of {} answered for a person to step in; no ask until the session moves", ask.id, run.id());
            }
        }
        Ok(())
    }
}
