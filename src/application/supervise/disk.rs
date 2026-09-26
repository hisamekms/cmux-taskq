//! The free disk space a claim and a landing need (ADR-0047 decision 44,
//! task 377): on every pass the supervisor reads the free bytes of the
//! queue's directory and what a claim and a landing need
//! ([`DiskConfig::needs`]). Short of either, it cleans what the ended runs
//! left ([`Supervisor::clean_ended_worktrees`] and `git worktree prune`)
//! and reads again, recording `auto_repaired` (`repair: disk_cleanup`)
//! when that freed something. Still short, it opens the queue's one `cost`
//! ask about the disk (`subject: disk`) once, and the runs whose landing
//! waits for the disk join it. The claims are held through
//! [`Supervisor::hold_claims`] (`claim_held`, reason `disk_space`), the
//! landings through `landing_held` / `landing_resumed`; a run waiting to
//! land stays awaiting integration and starts no verification.

use super::*;
use crate::domain::{
    Ask,
    claim_hold::LANDINGS,
    disk::{BUILD_OUTPUTS_REMOVED, DISK_CLEANUP, DISK_OPTIONS, DISK_SUBJECT, DiskNeeds, gib},
};

/// How long the needs read from the recent builds are kept.
const NEEDS_INTERVAL: Duration = Duration::from_secs(60);
/// Least time between two cleanups for the disk.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

/// The answer the runtime closes the disk ask with once there is room.
const DISK_ENOUGH_CLOSED: &str = "the free disk space is enough again; closed by the runtime";

/// The question of the disk ask; the runs whose landing waits follow it.
const DISK_QUESTION: &str = "The free disk space of the queue's directory stays below what the runs need, and cleaning what the ended runs left (their build outputs, the worktrees of completed and canceled tasks, `git worktree prune`) did not free enough: {free} free, a new run needs {claim} and a landing's verification {landing} (the largest build of the recent runs times [disk] claim_factor / integrate_factor of dagq.toml). No new run is claimed and no run lands until there is room; the runs in flight go on. Free disk space (for example the worktrees of failed runs nobody looks at any more, or other files on that disk) and answer `done`, or answer `wait` to leave the queue waiting: the supervisor resumes by itself once there is room.";

/// What the supervisor knows of the disk between passes.
#[derive(Debug, Default)]
pub(super) struct DiskWatch {
    /// The needs and when they were read.
    needs: Option<(Instant, DiskNeeds)>,
    /// When the disk was last cleaned for room; `None` while there is room.
    cleaned: Option<Instant>,
    /// The disk ask of this shortage was opened (or answered `wait`): it is
    /// not opened again until there is room, or a person answers `done`.
    asked: bool,
    /// The runs added to the disk ask.
    joined: Vec<RunId>,
    /// There is not room for a landing's verification.
    pub(super) landing_short: bool,
}

impl Supervisor<'_> {
    /// Check the free disk space for this pass (see the module): clean
    /// and ask when short, close the disk ask when there is room, and
    /// record the landings' hold. The claims' hold is judged when the
    /// supervisor claims ([`Self::hold_claims`]), from the same reading.
    pub(super) fn check_disk(&mut self) -> Result<()> {
        let unclosed: Vec<Ask> = self
            .queue
            .asks(AskQuery::default())?
            .into_iter()
            .filter(is_disk_ask)
            .collect();
        self.apply_disk_answers(&unclosed)?;
        let needs = self.disk_needs()?;
        let mut free = self.free_bytes();
        let short = |free: Option<u64>, need: Option<u64>| matches!((free, need), (Some(free), Some(need)) if free < need);
        let most = needs.claim.max(needs.landing);
        if short(free, most) {
            free = self.clean_for_disk(free, most)?;
        }
        self.free = free;
        let landings: Vec<RunId> = self
            .slots
            .iter()
            .filter(|slot| matches!(slot.phase, Phase::AwaitingSlot))
            .map(|slot| slot.run.id().clone())
            .collect();
        self.disk.landing_short = short(free, needs.landing);
        if short(free, most) {
            let held: Vec<RunId> = if self.disk.landing_short {
                landings.clone()
            } else {
                Vec::new()
            };
            let open = unclosed.iter().any(|ask| ask.is_open());
            self.ask_for_disk(free, &needs, &held, open)?;
        } else if unclosed.iter().any(|ask| ask.is_open()) || self.disk.asked {
            self.disk.cleaned = None;
            self.disk.asked = false;
            self.disk.joined.clear();
            for ask in self.queue.close_hold_asks(
                AskReason::Cost,
                Some(DISK_SUBJECT),
                DISK_ENOUGH_CLOSED,
            )? {
                info!(ask_id = %ask.id, "the free disk space is enough again: closed the disk ask {}", ask.id);
            }
        } else {
            self.disk.cleaned = None;
        }
        let hold = if landings.is_empty() {
            None
        } else {
            ClaimHold::judge(&HoldInputs {
                free_bytes: free,
                needed_bytes: needs.landing,
                ..HoldInputs::default()
            })
        };
        self.record_hold(claim_hold::LANDINGS, hold.as_ref())?;
        Ok(())
    }
    /// The free bytes of the queue's directory, where the run worktrees
    /// are; `None` when they cannot be read.
    pub(super) fn free_bytes(&self) -> Option<u64> {
        (self.free_space)(&self.layout.runs_dir)
            .or_else(|| self.layout.db.parent().and_then(self.free_space))
    }
    /// What a claim and a landing need, read again every
    /// [`NEEDS_INTERVAL`] from the latest `build_outputs_removed`.
    pub(super) fn disk_needs(&mut self) -> Result<DiskNeeds> {
        if let Some((at, needs)) = self.disk.needs
            && at.elapsed() < NEEDS_INTERVAL
        {
            return Ok(needs);
        }
        let limit = usize::try_from(self.disk_config.sample_runs).unwrap_or(0);
        let builds: Vec<u64> = self
            .queue
            .latest_events_of(BUILD_OUTPUTS_REMOVED, limit)?
            .iter()
            .filter_map(|event| event.payload.get("bytes").and_then(Value::as_u64))
            .collect();
        let needs = self.disk_config.needs(&builds);
        self.disk.needs = Some((Instant::now(), needs));
        Ok(needs)
    }
    /// Clean what the ended runs left for room (at most once every
    /// [`CLEANUP_INTERVAL`]), record `auto_repaired` when it freed
    /// something, and read the free bytes again.
    fn clean_for_disk(&mut self, free: Option<u64>, needed: Option<u64>) -> Result<Option<u64>> {
        if self
            .disk
            .cleaned
            .is_some_and(|at| at.elapsed() < CLEANUP_INTERVAL)
        {
            return Ok(free);
        }
        self.disk.cleaned = Some(Instant::now());
        let removed = match self.clean_ended_worktrees(None) {
            Ok(removed) => removed,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the worktrees of ended runs could not all be cleaned for disk space: {error:#}");
                Cleaned::default()
            }
        };
        if let Err(error) = self.repository.prune_worktrees() {
            warn!(error = %format_args!("{error:#}"), "git worktree prune failed: {error:#}");
        }
        let after = self.free_bytes();
        if removed.bytes > 0 {
            info!(
                "the free disk space was short: removed {} bytes of what {} ended run(s) left",
                removed.bytes,
                removed.runs.len()
            );
            self.queue.record_queue_event(
                "auto_repaired",
                json!({
                    "repair": DISK_CLEANUP,
                    "layer": "runtime",
                    "conditions": {
                        "free_bytes": free,
                        "needed_bytes": needed,
                        "free_bytes_after": after,
                    },
                    "detail": {"bytes": removed.bytes, "runs": removed.runs},
                    "bytes": removed.bytes,
                    "supervisor": self.token,
                }),
            )?;
        }
        Ok(after)
    }
    /// Open the disk ask of this shortage, or add the runs whose landing
    /// waits for the disk to the open one; once opened it is not opened
    /// again until there is room or a person answers `done`.
    fn ask_for_disk(
        &mut self,
        free: Option<u64>,
        needs: &DiskNeeds,
        runs: &[RunId],
        open: bool,
    ) -> Result<()> {
        let joining: Vec<Option<RunId>> = if self.disk.asked {
            if !open {
                // Answered `wait`: nothing more until there is room.
                return Ok(());
            }
            runs.iter()
                .filter(|run| !self.disk.joined.contains(run))
                .cloned()
                .map(Some)
                .collect()
        } else if runs.is_empty() {
            vec![None]
        } else {
            runs.iter().cloned().map(Some).collect()
        };
        let show = |bytes: Option<u64>| bytes.map_or_else(|| "unknown".into(), |b| gib(b as f64));
        let question = DISK_QUESTION
            .replace("{free}", &show(free))
            .replace("{claim}", &show(needs.claim))
            .replace("{landing}", &show(needs.landing));
        for run in joining {
            let (outcome, value) = ask::hold(
                &mut *self.queue,
                &self.layout.repo_root,
                NewHold {
                    reason_category: AskReason::Cost,
                    subject: Some(DISK_SUBJECT.into()),
                    run_id: run.clone(),
                    question: question.clone(),
                    options: DISK_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
                    asked_by: SessionRole::Supervisor.as_str().into(),
                },
                self.cmux,
            )?;
            if outcome.created {
                warn!(ask_id = %outcome.ask.id, "the free disk space stays short after the cleanup: opened the disk ask {} (notified: {})", outcome.ask.id, value["notified"]);
            }
            if let Some(run) = run {
                self.disk.joined.push(run);
            }
        }
        self.disk.asked = true;
        Ok(())
    }
    /// Close the answered disk asks: `done` (a person freed the disk)
    /// cleans and checks again at once and may ask again; `wait` leaves
    /// the queue waiting without asking again.
    fn apply_disk_answers(&mut self, unclosed: &[Ask]) -> Result<()> {
        for ask in unclosed.iter().filter(|ask| ask.answered_at.is_some()) {
            let answer = ask.answer.as_deref().unwrap_or_default().trim().to_owned();
            self.queue.close_ask(ask.id)?;
            info!(ask_id = %ask.id, "applied the answer {answer:?} of the disk ask {}", ask.id);
            if answer == "done" {
                self.disk.asked = false;
                self.disk.cleaned = None;
                self.disk.joined.clear();
            }
        }
        Ok(())
    }
    /// Record `kinds`' held or resumed event when `hold` differs from the
    /// hold in place on the queue (task 327); whether it holds.
    pub(super) fn record_hold(
        &mut self,
        kinds: claim_hold::HoldKinds,
        hold: Option<&ClaimHold>,
    ) -> Result<bool> {
        let last = self.queue.latest_queue_event(&kinds.kinds())?;
        // The supervisors running now: a hold another one recorded is in
        // place only while it runs (its registration's heartbeat is fresh).
        let now = self.generators.clock.now();
        let live: Vec<String> = self
            .queue
            .supervisors()?
            .into_iter()
            .filter(|registration| {
                !heartbeat_stale(
                    self.processes.alive(registration.pid),
                    now - registration.heartbeat_at,
                )
            })
            .map(|registration| registration.token)
            .collect();
        if let Some((kind, payload)) =
            claim_hold::transition_of(kinds, hold, last.as_ref(), &self.token, |holder| {
                live.iter().any(|token| token == holder)
            })
        {
            self.queue.record_queue_event(kind, payload)?;
            match (hold, kinds == LANDINGS) {
                (Some(hold), _) => warn!("{}", hold.message_for(kinds)),
                (None, false) => info!("claims resume: nothing holds them any more"),
                (None, true) => info!("landings resume: there is room for their verification"),
            }
        }
        Ok(hold.is_some())
    }
}

/// The queue's `cost` ask about the disk.
fn is_disk_ask(ask: &Ask) -> bool {
    ask.kind == AskKind::QueueHold
        && ask.reason_category == AskReason::Cost
        && ask.subject.as_deref() == Some(DISK_SUBJECT)
}

/// What [`Supervisor::clean_ended_worktrees`] removed.
#[derive(Debug, Default)]
pub(super) struct Cleaned {
    pub(super) bytes: u64,
    pub(super) runs: Vec<RunId>,
}

impl Cleaned {
    pub(super) fn add(&mut self, run: &RunId, bytes: u64) {
        if bytes > 0 {
            self.bytes += bytes;
            self.runs.push(run.clone());
        }
    }
}
