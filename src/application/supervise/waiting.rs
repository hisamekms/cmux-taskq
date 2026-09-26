//! Runs that wait for a person outside the slots (ADR-0062): a run whose
//! live session waits only for the answer of one of its asks leaves its
//! slot, keeps its lease and is watched without anything sent to its
//! session, within `--max-waiting`; once the wait ends it goes back to a
//! slot (at once when a person already moved the session, else when one is
//! free, before any new work) and its phase goes on where it stopped.

use super::*;
use crate::domain::{
    Ask,
    waiting::{
        RUN_SLOT_REGAINED, RUN_WAITING_ASK_ADDED, RUN_WAITING_DEFERRED, RUN_WAITING_ENDED,
        RUN_WAITING_STARTED, WaitCause, WaitPhase, WaitState, consumed_asks, deferred_asks,
        waits_for,
    },
};

/// The wait of one slot's run.
pub(super) struct Waiting {
    /// The asks the wait holds, oldest first.
    pub(super) asks: Vec<(AskId, AskKind)>,
    /// When it started, on the queue's clock (unix seconds).
    pub(super) started_at: i64,
    /// When its first ask was opened (or it started, if earlier), on the
    /// run files' clock: a marker newer than this is a session that moved.
    pub(super) since: SystemTime,
    /// When the screen was last read for the dialog the run waits at.
    pub(super) checked: Option<Instant>,
    /// When and how it ended, for a run that waits for a free slot.
    pub(super) ended: Option<(i64, WaitCause)>,
}

impl Slot {
    pub(super) fn new(run: TaskRun, phase: Phase) -> Self {
        Self {
            run,
            phase,
            waiting: None,
            consumed: Vec::new(),
            deferred: Vec::new(),
        }
    }

    /// Whether the slot's run is out of the slots: waiting, or waiting to
    /// go back.
    pub(super) fn out_of_slot(&self) -> bool {
        self.waiting.is_some()
    }

    fn waiting_now(&self) -> bool {
        self.waiting.as_ref().is_some_and(|w| w.ended.is_none())
    }

    /// The phase a run may wait in (decision 1): the first session before
    /// any `/exit`, with a heartbeating wrapper and no recovery job running,
    /// or the `/exit` after a verdict, with a session.
    fn wait_phase(&self) -> Option<WaitPhase> {
        match &self.phase {
            Phase::Session(watch)
                if watch.exit_requested.is_none() && !watch.silent && !watch.recovery.running() =>
            {
                Some(WaitPhase::Session)
            }
            Phase::Exiting(watch) if watch.session.is_some() => Some(WaitPhase::Exit),
            _ => None,
        }
    }

    /// The workspace of the session the run waits in.
    fn workspace(&self) -> Option<String> {
        match &self.phase {
            Phase::Session(watch) => Some(watch.workspace.clone()),
            Phase::Exiting(watch) => watch.session.as_ref().map(|s| s.workspace.clone()),
            _ => None,
        }
    }
}

/// The time a session that moved is judged from: the second its first
/// ask was opened in. A turn the session took since then (the one that
/// ended a `stalled` ask's idle, or a person's answer to a dialog) counts
/// even when the wait itself began a tick later.
fn asked_since(asked_at: i64) -> SystemTime {
    // Ask times are whole seconds: a marker in the second the ask was
    // opened is taken as older, as the stall watch does.
    UNIX_EPOCH + Duration::from_secs(u64::try_from(asked_at + 1).unwrap_or(0))
}

impl Supervisor<'_> {
    /// The slots in use: every run but the waiting ones and those waiting
    /// to go back (decision 5).
    pub(super) fn used_slots(&self) -> usize {
        self.slots.iter().filter(|slot| !slot.out_of_slot()).count()
    }

    /// The runs held against `--max-waiting`: the waiting ones and those
    /// waiting to go back, whose sessions are open all the same.
    fn waiting_runs(&self) -> usize {
        self.slots.iter().filter(|slot| slot.out_of_slot()).count()
    }

    /// Move the runs in the slots that wait for a person into waits, the
    /// oldest ask first, while the limit allows (decision 7); a run past
    /// the limit stays in its slot and records `run_waiting_deferred` once
    /// per ask.
    pub(super) fn start_waits(&mut self) {
        if self.max_waiting == 0 {
            return;
        }
        let mut candidates = Vec::new();
        for index in 0..self.slots.len() {
            match self.wait_candidate(&self.slots[index]) {
                Ok(Some(ask)) => candidates.push((ask.id, ask.kind, ask.created_at, index)),
                Ok(None) => {}
                Err(error) => {
                    let run = self.slots[index].run.id().clone();
                    warn!(run_id = %run, error = %format_args!("{error:#}"), "run {run}: whether it waits for a person could not be read: {error:#}");
                }
            }
        }
        candidates.sort_by_key(|(id, _, _, _)| *id);
        for (id, kind, created_at, index) in candidates {
            let result = if self.waiting_runs() < self.max_waiting {
                self.start_wait(index, id, kind, created_at)
            } else {
                self.defer_wait(index, id, kind)
            };
            if let Err(error) = result {
                let run = self.slots[index].run.id().clone();
                warn!(run_id = %run, error = %format_args!("{error:#}"), "run {run}: its wait could not be recorded: {error:#}");
            }
        }
    }

    /// The oldest ask the slot's run would wait for now: open (neither
    /// answered nor closed), of the table's kinds for its phase, not held
    /// by a wait that ended, while its session lives and the queue does
    /// not hold it.
    fn wait_candidate(&self, slot: &Slot) -> Result<Option<Ask>> {
        if slot.out_of_slot() {
            return Ok(None);
        }
        let Some(phase) = slot.wait_phase() else {
            return Ok(None);
        };
        let asks: Vec<Ask> = self
            .queue
            .unclosed_run_asks(slot.run.id())?
            .into_iter()
            .filter(|ask| {
                ask.is_open() && waits_for(phase, ask.kind) && !slot.consumed.contains(&ask.id)
            })
            .collect();
        if asks.is_empty()
            || !session_alive(self, slot.run.id())?
            || self.queue.hold_of(slot.run.id())?.is_some()
        {
            return Ok(None);
        }
        Ok(asks.into_iter().next())
    }

    fn start_wait(&mut self, index: usize, id: AskId, kind: AskKind, asked_at: i64) -> Result<()> {
        let waiting = self.waiting_runs() + 1;
        let slot = &self.slots[index];
        let phase = slot.wait_phase().context("the run left its phase")?;
        // The slot's copy can be older than the status (`starting`).
        let run = self.queue.run(slot.run.id())?;
        self.queue.record_runtime_event(
            run.id(),
            RUN_WAITING_STARTED,
            json!({
                "ask_id": id,
                "ask_kind": kind,
                "phase": phase.as_str(),
                "status": run.status().as_str(),
                "waiting": waiting,
                "limit": self.max_waiting,
            }),
        )?;
        info!(run_id = %run.id(), ask_id = %id, "run {} waits for a person in its {} ask {id} outside the slots ({waiting} of {} waiting)", run.id(), kind.as_str(), self.max_waiting);
        self.slots[index].waiting = Some(Waiting {
            asks: vec![(id, kind)],
            started_at: self.generators.clock.now(),
            since: asked_since(asked_at).min(self.files.now()),
            checked: None,
            ended: None,
        });
        Ok(())
    }

    fn defer_wait(&mut self, index: usize, id: AskId, kind: AskKind) -> Result<()> {
        if self.slots[index].deferred.contains(&id) {
            return Ok(());
        }
        let run = self.slots[index].run.clone();
        self.queue.record_runtime_event(
            run.id(),
            RUN_WAITING_DEFERRED,
            json!({
                "ask_id": id,
                "ask_kind": kind,
                "waiting": self.waiting_runs(),
                "limit": self.max_waiting,
            }),
        )?;
        info!(run_id = %run.id(), ask_id = %id, "run {} waits for its {} ask {id} in its slot: {} runs wait already", run.id(), kind.as_str(), self.max_waiting);
        self.slots[index].deferred.push(id);
        Ok(())
    }

    /// One look at a run out of the slots (decision 6). Nothing is sent to
    /// its session and its status does not change: the lease, the session
    /// (its end, a dead or silent wrapper), its asks (an answer, one more
    /// of the table), its markers (a session that moved) and, for a dialog,
    /// its screen. A run waiting to go back only keeps its lease checked.
    pub(super) fn watch_waiting(&mut self, slot: &mut Slot) -> Result<Step> {
        if !self.queue.holds_lease(slot.run.id(), &self.token)? {
            return Ok(Step::Disowned);
        }
        if !slot.waiting_now() {
            return Ok(Step::Continue);
        }
        let run = slot.run.clone();
        let phase = slot.wait_phase().unwrap_or(WaitPhase::Session);
        let workspace = slot.workspace().unwrap_or_default();
        let processes = self.queue.processes(run.id())?;
        let now = self.generators.clock.now();
        let wrapper = processes.iter().find(|p| p.role == "wrapper");
        let Some(wrapper) =
            wrapper.filter(|w| w.exited_at.is_none() && !wrapper_dead(self, w, now))
        else {
            // Nobody needs to send /exit to a session that ended, nor
            // answer its dialog, while it waits for a slot.
            close_answer_prompt_asks(self, &run, PROMPT_EXITED_CLOSED)?;
            for ask in self
                .queue
                .close_stuck_exit_asks(run.id(), STUCK_EXIT_CLOSED)?
            {
                info!(run_id = %run.id(), ask_id = %ask.id, "session of {} exited while it waited; closed its stuck_exit ask {}", run.id(), ask.id);
            }
            if let Phase::Session(watch) = &mut slot.phase {
                watch.stall.ended(self, &run)?;
            }
            self.end_wait(slot, WaitCause::SessionExited, None)?;
            return Ok(Step::Continue);
        };
        let wrapper = wrapper.clone();
        let silent = match &mut slot.phase {
            Phase::Session(watch) => &mut watch.silent,
            Phase::Exiting(watch) => &mut watch.silent,
            _ => return Ok(Step::Continue),
        };
        let pulse = wrapper_pulse(
            self,
            &run,
            &wrapper,
            &workspace,
            silent,
            "wrapper heartbeat expired; session may still be alive",
        )?;
        match pulse {
            // The next look sees its exit.
            WrapperPulse::Exited => return Ok(Step::Continue),
            // The session is sent /exit from its slot.
            WrapperPulse::Silent if phase == WaitPhase::Session => {
                self.end_wait(slot, WaitCause::WrapperSilent, None)?;
                return Ok(Step::Continue);
            }
            _ => {}
        }
        // A login that ran out holds the queue: the run goes back, and no
        // new work starts while the hold is open (ADR-0047 decision 42).
        if self.queue.hold_of(run.id())?.is_some() {
            self.end_wait(slot, WaitCause::QueueHold, None)?;
            return Ok(Step::Continue);
        }
        self.add_waiting_asks(slot, phase)?;
        let held = slot
            .waiting
            .as_ref()
            .map(|w| w.asks.clone())
            .unwrap_or_default();
        for (id, kind) in &held {
            if *kind != AskKind::WorkerQuestion {
                continue;
            }
            let ask = self.queue.read_ask(*id)?;
            if ask.answered_at.is_some() || ask.closed_at.is_some() {
                self.end_wait(slot, WaitCause::Answered, Some((*id, *kind)))?;
                return Ok(Step::Continue);
            }
        }
        let held_kind = |kind: AskKind| held.iter().find(|(_, k)| *k == kind).copied();
        // A receipt the first session wrote during the wait moved it on,
        // whatever it waited for (its `SessionWatch` closes the asks).
        if let Phase::Session(watch) = &slot.phase
            && !watch.receipt_seen
            && self.files.is_file(&watch.receipt_path)
        {
            self.end_wait(slot, WaitCause::SessionMoved, held.first().copied())?;
            return Ok(Step::Continue);
        }
        let idle_marker = run.idle_marker_path()?;
        // A receipt ends a stall by itself: no `stalled` ask follows it.
        if let Phase::Session(watch) = &mut slot.phase
            && held_kind(AskKind::Stalled).is_some()
            && !watch.receipt_seen
            && !self.files.is_file(&watch.receipt_path)
        {
            let dialog = watch.prompt_hash.is_some();
            watch
                .stall
                .poll_quiet(self, &run, &workspace, &idle_marker, dialog)?;
        }
        // A person who answered the dialog or typed into the session.
        let moves = held_kind(AskKind::AnswerPrompt).or(held_kind(AskKind::Stalled));
        let since = slot.waiting.as_ref().map_or(UNIX_EPOCH, |w| w.since);
        if let Some(ask) = moves
            && self.session_moved(&run, &idle_marker, since)
        {
            self.end_wait(slot, WaitCause::SessionMoved, Some(ask))?;
            return Ok(Step::Continue);
        }
        if phase == WaitPhase::Session
            && let Some(ask) = held_kind(AskKind::AnswerPrompt)
            && let Some(cause) = self.dialog_ended(slot, &run, &workspace)?
        {
            self.end_wait(slot, cause, Some(ask))?;
        }
        Ok(Step::Continue)
    }

    /// Add to the wait the asks of the table opened since (decision 2).
    fn add_waiting_asks(&mut self, slot: &mut Slot, phase: WaitPhase) -> Result<()> {
        let asks = self.queue.unclosed_run_asks(slot.run.id())?;
        for ask in asks {
            let Some(waiting) = &mut slot.waiting else {
                return Ok(());
            };
            if !ask.is_open()
                || !waits_for(phase, ask.kind)
                || slot.consumed.contains(&ask.id)
                || waiting.asks.iter().any(|(id, _)| *id == ask.id)
            {
                continue;
            }
            waiting.asks.push((ask.id, ask.kind));
            self.queue.record_runtime_event(
                slot.run.id(),
                RUN_WAITING_ASK_ADDED,
                json!({"ask_id": ask.id, "ask_kind": ask.kind}),
            )?;
            info!(run_id = %slot.run.id(), ask_id = %ask.id, "run {}: its {} ask {} joins its wait", slot.run.id(), ask.kind.as_str(), ask.id);
        }
        Ok(())
    }

    /// Whether the session took a turn or an input, or wrote its receipt,
    /// after `since`.
    fn session_moved(&self, run: &TaskRun, idle_marker: &Path, since: SystemTime) -> bool {
        let mut markers = vec![idle_marker.to_path_buf()];
        if let Some(dir) = run.run_dir() {
            markers.push(Path::new(dir).join(super::super::stats::PROMPT_SUBMIT_MARKER));
        }
        if let Some(receipt) = run.receipt_path() {
            markers.push(PathBuf::from(receipt));
        }
        markers
            .iter()
            .any(|path| self.files.modified(path).is_ok_and(|at| at > since))
    }

    /// Read the screen of a session that waits at a dialog, as often as
    /// [`SessionWatch::watch_prompt`] does: a login that ran out holds the
    /// queue (`queue_hold`), and a screen without the dialog clears it
    /// (`dialog_cleared`). No key is sent.
    fn dialog_ended(
        &mut self,
        slot: &mut Slot,
        run: &TaskRun,
        workspace: &str,
    ) -> Result<Option<WaitCause>> {
        let interval = self.cmux.prompt_wait().min(PROMPT_CHECK_INTERVAL);
        let Some(waiting) = &mut slot.waiting else {
            return Ok(None);
        };
        if waiting.checked.is_some_and(|at| at.elapsed() < interval) {
            return Ok(None);
        }
        waiting.checked = Some(Instant::now());
        let screen = match self.cmux.capture(workspace) {
            Ok(screen) => screen,
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "screen of {} could not be read for its dialog: {error:#}", run.id());
                return Ok(None);
            }
        };
        let Phase::Session(watch) = &mut slot.phase else {
            return Ok(None);
        };
        if self.signals.auth_required(&screen) && raise_auth(self, run, workspace, &screen)? {
            watch.clear_prompt(self, run)?;
            return Ok(Some(WaitCause::QueueHold));
        }
        if self.signals.detect_prompt(&screen).is_none() {
            watch.clear_prompt(self, run)?;
            return Ok(Some(WaitCause::DialogCleared));
        }
        Ok(None)
    }

    /// End the slot's wait: record `run_waiting_ended` and, when a person
    /// already moved the session, put the run back in its slot at once;
    /// otherwise it waits for a free slot ([`Self::return_waiting_runs`]).
    fn end_wait(
        &mut self,
        slot: &mut Slot,
        cause: WaitCause,
        ask: Option<(AskId, AskKind)>,
    ) -> Result<()> {
        let Some(waiting) = &mut slot.waiting else {
            return Ok(());
        };
        let now = self.generators.clock.now();
        self.queue.record_runtime_event(
            slot.run.id(),
            RUN_WAITING_ENDED,
            json!({
                "ask_id": ask.map(|(id, _)| id),
                "ask_kind": ask.map(|(_, kind)| kind),
                "cause": cause.as_str(),
                "waited_secs": now - waiting.started_at,
            }),
        )?;
        info!(run_id = %slot.run.id(), "run {}'s wait ended ({}) after {}s", slot.run.id(), cause.as_str(), now - waiting.started_at);
        slot.consumed.extend(waiting.asks.iter().map(|(id, _)| *id));
        waiting.ended = Some((now, cause));
        if cause.returns_at_once() {
            // The slot is out of `self.slots` while it is watched.
            let used = self.used_slots() + 1;
            self.regain_slot(slot, used)?;
        }
        Ok(())
    }

    /// Put a run whose wait ended back in its slot, recording
    /// `run_slot_regained`; `used` is the slots in use with it.
    fn regain_slot(&mut self, slot: &mut Slot, used: usize) -> Result<()> {
        match slot.waiting.take() {
            Some(waiting) => self.record_regained(slot.run.id(), &waiting, used),
            None => Ok(()),
        }
    }

    fn record_regained(&mut self, run: &RunId, waiting: &Waiting, used: usize) -> Result<()> {
        let now = self.generators.clock.now();
        let ended_at = waiting.ended.map_or(now, |(at, _)| at);
        self.queue.record_runtime_event(
            run,
            RUN_SLOT_REGAINED,
            json!({
                "slot_wait_secs": now - ended_at,
                "over_parallel": used > self.parallel,
            }),
        )?;
        info!(run_id = %run, "run {run} is back in a slot ({used} of {} used)", self.parallel);
        Ok(())
    }

    /// Put the runs whose wait ended back in the slots, the earliest end
    /// first, while a slot is free (decision 8): before any new work, and
    /// also while the supervisor drains, whose end waits for them.
    pub(super) fn return_waiting_runs(&mut self) {
        let mut returning: Vec<(i64, usize)> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slot.waiting
                    .as_ref()
                    .and_then(|w| w.ended)
                    .map(|(at, _)| (at, index))
            })
            .collect();
        returning.sort_unstable();
        for (_, index) in returning {
            if self.used_slots() >= self.parallel {
                break;
            }
            let Some(waiting) = self.slots[index].waiting.take() else {
                continue;
            };
            let run = self.slots[index].run.id().clone();
            let used = self.used_slots();
            if let Err(error) = self.record_regained(&run, &waiting, used) {
                warn!(run_id = %run, error = %format_args!("{error:#}"), "run {run}: its return to a slot could not be recorded: {error:#}");
            }
        }
    }

    /// Rebuild the wait of a run taken over after an adoption or a handoff
    /// from its events (decision 11): the asks of its ended waits, its
    /// deferred asks, and a wait that lasts or waits for a slot. A wait
    /// the run cannot keep (its phase was rebuilt as another, or, with
    /// `as_waiting` false, the limit is reached) ends as `phase_changed`
    /// and the run is in its slot.
    pub(super) fn restore_waiting(&mut self, slot: &mut Slot, as_waiting: bool) -> Result<()> {
        let events = self.queue.run_events(slot.run.id())?;
        slot.consumed = consumed_asks(&events);
        slot.deferred = deferred_asks(&events);
        let Some(state) = WaitState::of(&events) else {
            return Ok(());
        };
        let asks: Vec<(AskId, AskKind)> = state
            .asks
            .iter()
            .filter_map(|(id, kind)| kind.parse().ok().map(|kind| (*id, kind)))
            .collect();
        let since_secs = state.since_ms.div_euclid(1000);
        let asked_at = match asks.first() {
            Some((id, _)) => self.queue.read_ask(*id)?.created_at,
            None => since_secs,
        };
        let mut waiting = Waiting {
            asks,
            started_at: since_secs,
            since: asked_since(asked_at),
            checked: None,
            ended: state.ended.map(|(ms, cause)| (ms.div_euclid(1000), cause)),
        };
        let keeps = slot.wait_phase().is_some() && (as_waiting || waiting.ended.is_some());
        if keeps {
            slot.waiting = Some(waiting);
            return Ok(());
        }
        let now = self.generators.clock.now();
        if waiting.ended.is_none() {
            self.queue.record_runtime_event(
                slot.run.id(),
                RUN_WAITING_ENDED,
                json!({
                    "ask_id": null,
                    "ask_kind": null,
                    "cause": WaitCause::PhaseChanged.as_str(),
                    "waited_secs": now - waiting.started_at,
                }),
            )?;
            // Over the limit, the same asks may start a wait later: they
            // are not taken as consumed here.
            waiting.ended = Some((now, WaitCause::PhaseChanged));
        }
        slot.waiting = Some(waiting);
        let used = self.used_slots() + 1;
        self.regain_slot(slot, used)
    }

    /// Whether the run waits by its events, and has room to be adopted as
    /// a wait (decision 11): a run that waits needs no free slot while the
    /// waits are under their limit.
    pub(super) fn adopts_as_waiting(&self, run: &RunId) -> Result<bool> {
        Ok(self.max_waiting > 0
            && self.waiting_runs() < self.max_waiting
            && WaitState::of(&self.queue.run_events(run)?).is_some_and(|s| s.ended.is_none()))
    }
}
