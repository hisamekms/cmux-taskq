//! One window's KPIs (ADR-0051 decisions 1–7): the runs that finished in
//! it as [`stats`] derives them, split into strata by the task's kind and
//! the attributes of the claim, and the KPIs no run carries (asks,
//! attentions, backend failures, load, verification commands, slots,
//! findings, sessions of the other kinds) over the whole window.
use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;
use serde_json::Value;

use super::{ALL, Axis, KpiInput, Kpis, Measure, UNKNOWN, float};
use crate::domain::{
    DraftOrigin, EventId, GoalId, RunEvent, RunId, TaskId, TaskKind, event_attention,
    marks::{self, Mark},
    plan_quality::plan_quality,
    stats::{
        self, Cursor, LiveSnapshot, RunStats, SlotSnapshot, StatsQuery, asks::human_waits,
        landing::PHASES, measures::verification_durations, timestamp_millis,
    },
    waiting::{RUN_SLOT_REGAINED, RUN_WAITING_STARTED},
};

/// A sample of what the supervisor could claim (ADR-0051 decision 3):
/// `candidates`, `free_slots` and `ready`, recorded when they change.
pub const CANDIDATES_SAMPLED: &str = "candidates_sampled";

/// The KPIs no record exists for yet, and why.
const NOT_RECORDED: &str = "not_recorded";
const NO_SAMPLES: &str = "no_samples";

/// One window's KPIs.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct WindowKpis {
    /// The runs that finished in the window.
    pub runs: usize,
    /// Per KPI, per stratum (`all`, `kind=runtime`, `parallel=3`, ...).
    pub kpis: Kpis<Measure>,
    /// The breakdowns next to the KPIs: resumes per reason, asks per reason
    /// category, backend failures per op, repairs per layer, the phases'
    /// share of the long tail of `wait_to_land`, the findings opened and
    /// resolved, and the time slots starved.
    pub details: BTreeMap<&'static str, Value>,
    /// The KPIs whose records do not exist, and why.
    pub unavailable: BTreeMap<&'static str, &'static str>,
}

/// A run's hold on a slot: from its claim to its end, without the waits
/// for a person outside the slots (ADR-0062).
struct Occupancy {
    task_id: TaskId,
    spans: Vec<(i64, i64)>,
}

/// What every window reads, derived once.
pub(super) struct Context<'a> {
    pub events: &'a [RunEvent],
    goals: &'a HashMap<TaskId, Option<GoalId>>,
    kinds: &'a HashMap<TaskId, Option<TaskKind>>,
    goal_id: Option<GoalId>,
    /// Unix seconds.
    now: i64,
    cores: Option<usize>,
    /// When each task first became `ready`.
    first_ready: HashMap<TaskId, i64>,
    runs_per_task: HashMap<TaskId, HashSet<RunId>>,
    revises: HashMap<RunId, usize>,
    /// The `integrate` attempts that reached their verification.
    verify_attempts: HashMap<RunId, usize>,
    /// When each finished run (of the goal) finished, ascending.
    pub finishes: Vec<i64>,
    /// The supervisors' lives: start, end and `parallel`.
    supervisors: Vec<(i64, i64, i64)>,
    occupancy: Vec<Occupancy>,
    pub marks: Vec<Mark>,
    pub min_samples: usize,
}

fn event_ms(event: &RunEvent) -> Option<i64> {
    timestamp_millis(&event.created_at)
}

fn overlap((from, to): (i64, i64), start: i64, end: i64) -> i64 {
    (to.min(end) - from.max(start)).max(0)
}

impl<'a> Context<'a> {
    pub fn new(input: &KpiInput<'a>, goal_id: Option<GoalId>) -> Self {
        let events = input.events;
        let now_ms = input.now * 1000;
        let mut first_ready = HashMap::new();
        let mut runs_per_task: HashMap<TaskId, HashSet<RunId>> = HashMap::new();
        let mut revises: HashMap<RunId, usize> = HashMap::new();
        let mut verify_attempts: HashMap<RunId, usize> = HashMap::new();
        let mut claims: Vec<(RunId, TaskId, i64)> = Vec::new();
        let mut ends: HashMap<RunId, i64> = HashMap::new();
        let mut waits: HashMap<RunId, Vec<(i64, Option<i64>)>> = HashMap::new();
        for event in events {
            let at = event_ms(event);
            if event.kind == "task_status_changed"
                && event.payload["to"] == "ready"
                && let (Some(task_id), Some(at)) = (event.task_id, at)
            {
                first_ready.entry(task_id).or_insert(at);
            }
            let (Some(run_id), Some(task_id)) = (&event.run_id, event.task_id) else {
                continue;
            };
            match event.kind.as_str() {
                "run_claimed" => {
                    if runs_per_task
                        .entry(task_id)
                        .or_default()
                        .insert(run_id.clone())
                        && let Some(at) = at
                    {
                        claims.push((run_id.clone(), task_id, at));
                    }
                }
                "revise_requested" => *revises.entry(run_id.clone()).or_default() += 1,
                "integration_rebased" => {
                    *verify_attempts.entry(run_id.clone()).or_default() += 1;
                }
                RUN_WAITING_STARTED => {
                    if let Some(at) = at {
                        waits.entry(run_id.clone()).or_default().push((at, None));
                    }
                }
                RUN_SLOT_REGAINED => {
                    if let Some(wait) = waits
                        .get_mut(run_id)
                        .and_then(|waits| waits.last_mut())
                        .filter(|wait| wait.1.is_none())
                    {
                        wait.1 = at;
                    }
                }
                _ => {}
            }
            let status = event.payload.get("status").and_then(Value::as_str);
            if (event.kind == "run_integrated"
                || matches!(status, Some("succeeded" | "failed" | "interrupted")))
                && let Some(at) = at
            {
                ends.entry(run_id.clone()).or_insert(at);
            }
        }
        let occupancy = claims
            .into_iter()
            .map(|(run_id, task_id, claimed)| {
                let end = ends.get(&run_id).copied().unwrap_or(now_ms);
                let mut spans = Vec::new();
                let mut from = claimed;
                for &(left, back) in waits.get(&run_id).into_iter().flatten() {
                    if left >= end {
                        break;
                    }
                    spans.push((from, left.max(from)));
                    from = back.unwrap_or(end).min(end);
                }
                spans.push((from, end.max(from)));
                Occupancy { task_id, spans }
            })
            .collect();
        // A supervisor's life ends at its stop, or at the next start of any
        // supervisor (a handoff, or a restart after it went stale without a
        // stop), or now.
        let starts: Vec<(i64, i64, Option<&str>)> = events
            .iter()
            .filter(|event| event.kind == marks::SUPERVISOR_STARTED)
            .filter_map(|event| {
                Some((
                    event_ms(event)?,
                    event.payload.get("parallel").and_then(Value::as_i64)?,
                    event.payload.get("supervisor").and_then(Value::as_str),
                ))
            })
            .collect();
        let supervisors = starts
            .iter()
            .enumerate()
            .map(|(index, &(start, parallel, supervisor))| {
                let next = starts.get(index + 1).map_or(now_ms, |next| next.0);
                let stop = events
                    .iter()
                    .filter(|event| {
                        event.kind == marks::SUPERVISOR_STOPPED
                            && supervisor.is_some()
                            && event.payload.get("supervisor").and_then(Value::as_str) == supervisor
                    })
                    .filter_map(event_ms)
                    .find(|&at| at >= start)
                    .unwrap_or(now_ms);
                (start, next.min(stop).min(now_ms).max(start), parallel)
            })
            .collect();
        let mut context = Self {
            events,
            goals: input.goals,
            kinds: input.kinds,
            goal_id,
            now: input.now,
            cores: input.cores,
            first_ready,
            runs_per_task,
            revises,
            verify_attempts,
            finishes: Vec::new(),
            supervisors,
            occupancy,
            marks: marks::marks(events, None, None),
            min_samples: input.config.min_samples,
        };
        let everything = context.stats(None, None);
        let times: HashMap<EventId, i64> = events
            .iter()
            .filter_map(|event| Some((event.id, event_ms(event)?)))
            .collect();
        let mut finishes: Vec<i64> = everything
            .runs
            .iter()
            .filter_map(|run| times.get(&run.finished_event_id?).copied())
            .collect();
        finishes.sort_unstable();
        context.finishes = finishes;
        context
    }

    fn counts(&self, task_id: Option<TaskId>) -> bool {
        self.goal_id.is_none_or(|goal| {
            task_id.is_some_and(|task| self.goals.get(&task).copied().flatten() == Some(goal))
        })
    }

    fn kind_of(&self, task_id: Option<TaskId>) -> &'static str {
        task_id
            .and_then(|task| self.kinds.get(&task).copied().flatten())
            .map_or(UNKNOWN, TaskKind::as_str)
    }

    /// What `stats` derives for the runs that finished after `after` up
    /// to `upto`, every one of them.
    fn stats(&self, after: Option<EventId>, upto: Option<EventId>) -> stats::Stats {
        let mut stats = stats::stats(
            self.events,
            self.goals,
            self.now,
            SlotSnapshot::default(),
            &StatsQuery {
                since: after.map(Cursor::Event),
                until: upto.map(Cursor::Event),
                goal_id: self.goal_id,
                full: true,
            },
            &LiveSnapshot::default(),
        );
        stats::with_kinds(&mut stats, self.kinds);
        stats
    }

    /// The value of `axis` for `run`.
    fn axis_value(&self, run: &RunStats, axis: Axis) -> String {
        let measures = &run.measures;
        let text = |value: &Option<String>| value.clone().unwrap_or_else(|| UNKNOWN.to_owned());
        match axis {
            Axis::Kind => run.kind.map_or(UNKNOWN, TaskKind::as_str).to_owned(),
            Axis::Build => text(&measures.dagq_version),
            Axis::Claude => text(&measures.claude_version),
            Axis::Parallel => measures
                .claim_parallel
                .map_or_else(|| UNKNOWN.to_owned(), |parallel| parallel.to_string()),
            Axis::Slot => match (measures.claim_slots, measures.claim_parallel) {
                (Some(slots), Some(parallel)) if parallel > 0 => {
                    let used = float(slots) / float(parallel);
                    if used < 0.5 {
                        "low"
                    } else if used < 1.0 {
                        "mid"
                    } else {
                        "full"
                    }
                }
                _ => UNKNOWN,
            }
            .to_owned(),
            Axis::Load => match (measures.claim_load_avg, self.cores) {
                (Some(load), Some(cores)) if cores > 0 => {
                    #[allow(clippy::cast_precision_loss)]
                    let per_core = load / cores as f64;
                    if per_core < 1.0 {
                        "low"
                    } else if per_core < 2.0 {
                        "mid"
                    } else if per_core < 4.0 {
                        "high"
                    } else {
                        "extreme"
                    }
                }
                _ => UNKNOWN,
            }
            .to_owned(),
            Axis::Toolchain => match (&measures.rustc_release, &measures.rustc_host) {
                (Some(release), Some(host)) => format!("{release} {host}"),
                _ => UNKNOWN.to_owned(),
            },
        }
    }

    /// The KPIs of the window after `start` up to `end` (unix ms), the
    /// runs split by `axes`.
    pub fn window(&self, start: i64, end: i64, axes: &[Axis]) -> WindowKpis {
        let events = self.events;
        let after = Cursor::Time(start).event_id(events);
        let upto = Cursor::Time(end).event_id(events);
        let stats = self.stats(Some(after), Some(upto));
        let in_window =
            |event: &&RunEvent| event.id > after && event.id <= upto && self.counts(event.task_id);
        let mut kpis: Kpis<Measure> = BTreeMap::new();
        let mut put = |name: &str, stratum: &str, measure: Measure| {
            kpis.entry(name.to_owned())
                .or_default()
                .insert(stratum.to_owned(), measure);
        };

        let mut groups: BTreeMap<String, Vec<&RunStats>> = BTreeMap::new();
        groups.insert(ALL.to_owned(), Vec::new());
        for run in &stats.runs {
            groups.entry(ALL.to_owned()).or_default().push(run);
            for &axis in axes {
                let key = format!("{}={}", axis.as_str(), self.axis_value(run, axis));
                groups.entry(key).or_default().push(run);
            }
        }
        let mut landings: BTreeMap<&str, usize> = BTreeMap::new();
        for (stratum, runs) in &groups {
            for (name, measure) in self.run_kpis(runs) {
                if name == "landings" {
                    landings.insert(stratum, measure.n);
                }
                put(name.as_str(), stratum, measure);
            }
        }
        let landed = |stratum: &str| landings.get(stratum).copied().unwrap_or(0);

        // Asks: all of them, and those of a task per its kind (decision 7).
        let mut asks: BTreeMap<String, usize> = BTreeMap::new();
        for event in events
            .iter()
            .filter(in_window)
            .filter(|e| e.kind == "ask_opened")
        {
            *asks.entry(ALL.to_owned()).or_default() += 1;
            if event.task_id.is_some() && axes.contains(&Axis::Kind) {
                *asks
                    .entry(format!("kind={}", self.kind_of(event.task_id)))
                    .or_default() += 1;
            }
        }
        for stratum in groups
            .keys()
            .filter(|s| *s == ALL || s.starts_with("kind="))
        {
            let opened = asks.get(stratum).copied().unwrap_or(0);
            put(
                "asks_per_landing",
                stratum,
                Measure::ratio(float(opened as i64), landed(stratum)),
            );
        }
        let attentions = events
            .iter()
            .filter(in_window)
            .filter(|event| event_attention(&event.kind, &event.payload).is_some())
            .count();
        put(
            "attentions_per_landing",
            ALL,
            Measure::ratio(float(attentions as i64), landed(ALL)),
        );
        let (to_answer, to_apply) = human_waits(events, after, upto, |task| self.counts(task));
        put("ask_wait", ALL, Measure::secs(to_answer));
        put("ask_apply_wait", ALL, Measure::secs(to_apply));

        // Infrastructure.
        put(
            "backend_failures_per_run",
            ALL,
            Measure::ratio(float(stats.backend_failures.count), stats.runs.len()),
        );
        let claim_loads: Vec<f64> = events
            .iter()
            .filter(in_window)
            .filter(|event| event.kind == "run_claimed")
            .filter_map(|event| event.payload.get("load_avg").and_then(Value::as_f64))
            .collect();
        let max_load = claim_loads
            .iter()
            .copied()
            .chain(stats.backend_failures.max_load_avg)
            .reduce(f64::max);
        put(
            "max_load_avg",
            ALL,
            Measure::spread(claim_loads).with_value(max_load),
        );
        put(
            "auto_repairs",
            ALL,
            Measure::count(usize::try_from(stats.auto_repairs.count).unwrap_or(0)),
        );
        for (command, secs) in verification_durations(events, after, upto, |t| self.counts(t)) {
            put(
                &format!("verify_command.{command}"),
                ALL,
                Measure::spread(secs),
            );
        }

        // Slots and what could be claimed.
        let (slot_usage, runs_holding) = self.slot_usage(start, end);
        put("slot_usage", ALL, Measure::total(slot_usage, runs_holding));
        let mut unavailable = BTreeMap::new();
        let mut details: BTreeMap<&'static str, Value> = BTreeMap::new();
        match self.candidates(start, end) {
            Some((mean, max, starved, samples)) => {
                let mut measure = Measure::total(Some(mean), samples);
                measure.max = Some(max);
                measure.has_spread = true;
                put("candidates", ALL, measure);
                details.insert("candidates", serde_json::json!({"starved_secs": starved}));
            }
            None => {
                put("candidates", ALL, Measure::total(None, 0));
                unavailable.insert("candidates", NO_SAMPLES);
            }
        }

        // Improvements.
        let findings = self.findings(after, upto);
        put("findings_open", ALL, Measure::count(findings.open_at_end));
        put(
            "finding_resolve_time",
            ALL,
            Measure::secs(findings.resolve_secs.iter().copied()),
        );
        details.insert(
            "findings",
            serde_json::json!({"recorded": findings.recorded, "resolved": findings.resolve_secs.len()}),
        );
        put("improvement_proposals", ALL, Measure::total(None, 0));
        unavailable.insert("improvement_proposals", NOT_RECORDED);

        // Sessions of the kinds no run is measured by one of (decision 1):
        // their totals over the window.
        for (kind, sessions) in &stats.sessions.by_kind {
            if *kind != "worker" {
                put(
                    &format!("session_open.{kind}"),
                    ALL,
                    Measure::total(
                        Some(float(sessions.open.summary.total)),
                        sessions.open.summary.count,
                    ),
                );
                put(
                    &format!("session_active.{kind}"),
                    ALL,
                    Measure::total(
                        (sessions.active.summary.count > 0)
                            .then(|| float(sessions.active.summary.total)),
                        sessions.active.summary.count,
                    ),
                );
            }
            put(
                &format!("session_active_ratio.{kind}"),
                ALL,
                Measure::total(
                    sessions
                        .active_ratio
                        .map(|ratio| float(ratio.thousandths()) / 1000.0),
                    sessions.active.summary.count,
                ),
            );
        }

        // The quality of the plans (ADR-0079 decision 7): split by the
        // judging plan review session's model and effort and by the
        // proposal's features.
        for (stratum, quality) in plan_quality(events, after, upto, |t| self.counts(t)) {
            put(
                "plan.revise_rate",
                &stratum,
                Measure::ratio(float(quality.revises as i64), quality.reviews),
            );
            put(
                "plan.duplicate_cancels_after_ready",
                &stratum,
                Measure::total(
                    Some(float(quality.duplicate_cancels_after_ready as i64)),
                    quality.proposals,
                ),
            );
            put(
                "plan.follow_up_canceled_after_adoption",
                &stratum,
                Measure::total(
                    Some(float(quality.follow_ups_canceled_after_adoption as i64)),
                    quality.follow_ups,
                ),
            );
            put(
                "plan.task_rework_rate",
                &stratum,
                Measure::ratio(float(quality.tasks_reworked as i64), quality.tasks_run),
            );
        }
        // The follow-ups adopted as `stats`' `draft_flow` counts them
        // (task 470): whose plan rejected one is not recorded.
        let follow_ups = stats
            .draft_flow
            .by_origin
            .get(DraftOrigin::FollowUp.as_str())
            .cloned()
            .unwrap_or_default();
        put(
            "plan.follow_up_adoption_rate",
            ALL,
            Measure::ratio(
                float(follow_ups.adopted),
                usize::try_from(follow_ups.adopted + follow_ups.canceled).unwrap_or(0),
            ),
        );

        let breakdown = &stats.overall.land_phases;
        let tail: i64 = breakdown.phases.iter().map(|phase| phase.tail_total).sum();
        let share: BTreeMap<&str, Option<f64>> = PHASES
            .iter()
            .zip(&breakdown.phases)
            .map(|(name, phase)| {
                (
                    *name,
                    (tail > 0).then(|| super::round3(float(phase.tail_total) / float(tail))),
                )
            })
            .collect();
        details.insert("land_phase_tail_share", serde_json::json!(share));
        details.insert("resume_outcomes", json(&stats.overall.resume_outcomes));
        details.insert(
            "asks_by_reason_category",
            json(&stats.asks.opened.by_reason_category),
        );
        details.insert(
            "backend_failures_by_op",
            json(&stats.backend_failures.by_op),
        );
        details.insert("auto_repairs_by_layer", json(&stats.auto_repairs.by_layer));

        WindowKpis {
            runs: stats.runs.len(),
            kpis,
            details,
            unavailable,
        }
    }

    /// The KPIs a run carries, over `runs` (all finished in the window).
    fn run_kpis(&self, runs: &[&RunStats]) -> Vec<(String, Measure)> {
        let landed: Vec<&RunStats> = runs
            .iter()
            .copied()
            .filter(|run| run.status.as_deref() == Some("integrated"))
            .collect();
        let revises = |run: &RunStats| self.revises.get(&run.run_id).copied().unwrap_or(0);
        let deferred = |run: &RunStats, code: &str| {
            usize::try_from(run.retries.deferrals.get(code).copied().unwrap_or(0)).unwrap_or(0)
        };
        let secs = |interval: fn(&RunStats) -> Option<i64>| {
            Measure::secs(landed.iter().filter_map(|run| interval(run)))
        };
        let mut kpis = vec![
            ("landings".to_owned(), Measure::count(landed.len())),
            (
                "lead_time".to_owned(),
                Measure::secs(landed.iter().filter_map(|run| {
                    let ready = self.first_ready.get(&run.task_id)?;
                    Some((timestamp_millis(run.landed_at.as_deref()?)? - ready) / 1000)
                })),
            ),
            ("phase.startup".to_owned(), secs(|run| run.startup)),
            ("phase.work".to_owned(), secs(|run| run.work)),
            ("phase.validate".to_owned(), secs(|run| run.validate)),
            (
                "phase.wait_to_land".to_owned(),
                secs(|run| run.wait_to_land),
            ),
        ];
        for (index, phase) in PHASES.iter().enumerate() {
            kpis.push((
                format!("land_phase.{phase}"),
                Measure::secs(
                    landed
                        .iter()
                        .filter_map(|run| Some(run.land_phases.as_ref()?.secs[index])),
                ),
            ));
        }
        kpis.push((
            "land_phase.push".to_owned(),
            Measure::secs(
                landed
                    .iter()
                    .filter_map(|run| run.land_phases.as_ref()?.push),
            ),
        ));
        // A task completes with the run that lands it.
        let mut tasks = HashSet::new();
        let completed: Vec<&&RunStats> = landed
            .iter()
            .filter(|run| tasks.insert(run.task_id))
            .collect();
        let first_pass = completed
            .iter()
            .filter(|run| {
                self.runs_per_task.get(&run.task_id).map_or(0, HashSet::len) == 1
                    && run.resumes == 0
                    && revises(run) == 0
                    && run.retries.deferrals.is_empty()
            })
            .count();
        kpis.push((
            "first_pass_rate".to_owned(),
            Measure::ratio(float(first_pass as i64), completed.len()),
        ));
        kpis.push((
            "revise_rate".to_owned(),
            Measure::ratio(
                float(landed.iter().filter(|run| revises(run) > 0).count() as i64),
                landed.len(),
            ),
        ));
        let attempted: Vec<&&RunStats> = runs
            .iter()
            .filter(|run| run.retries.integrate_attempts > 0)
            .collect();
        kpis.push((
            "conflict_rate".to_owned(),
            Measure::ratio(
                float(
                    attempted
                        .iter()
                        .filter(|run| deferred(run, "rebase_conflict") > 0)
                        .count() as i64,
                ),
                attempted.len(),
            ),
        ));
        let verified: usize = runs
            .iter()
            .map(|run| self.verify_attempts.get(&run.run_id).copied().unwrap_or(0))
            .sum();
        let failed_verifications: usize = runs
            .iter()
            .map(|run| deferred(run, "verification_failed"))
            .sum();
        kpis.push((
            "verification_failed_rate".to_owned(),
            Measure::ratio(float(failed_verifications.min(verified) as i64), verified),
        ));
        kpis.push((
            "resumes_per_run".to_owned(),
            Measure::ratio(
                float(runs.iter().map(|run| run.resumes).sum::<i64>()),
                runs.len(),
            ),
        ));
        kpis.push((
            "failed_rate".to_owned(),
            Measure::ratio(
                float(
                    runs.iter()
                        .filter(|run| {
                            matches!(run.status.as_deref(), Some("failed" | "interrupted"))
                        })
                        .count() as i64,
                ),
                runs.len(),
            ),
        ));
        // A run finished before the sessions were recorded has none and is
        // not a sample (decision 4).
        kpis.push((
            "session_open.worker".to_owned(),
            Measure::secs(
                runs.iter()
                    .filter_map(|run| Some(run.sessions.get("worker")?.open)),
            ),
        ));
        kpis.push((
            "session_active.worker".to_owned(),
            Measure::secs(
                runs.iter()
                    .filter_map(|run| run.sessions.get("worker")?.active),
            ),
        ));
        kpis
    }

    /// The share of the live supervisors' slots the runs held in the
    /// window (decision 3), and how many runs held one; null without a
    /// supervisor's start in it. A run's hold counts only while a
    /// supervisor was alive, so the runs before the starts were recorded
    /// (and a run whose end was never recorded) do not count.
    fn slot_usage(&self, start: i64, end: i64) -> (Option<f64>, usize) {
        let capacity: i64 = self
            .supervisors
            .iter()
            .map(|&(from, to, parallel)| parallel * overlap((from, to), start, end))
            .sum();
        // The times any supervisor was alive in the window, merged.
        let mut alive: Vec<(i64, i64)> = self
            .supervisors
            .iter()
            .map(|&(from, to, _)| (from.max(start), to.min(end)))
            .filter(|(from, to)| from < to)
            .collect();
        alive.sort_unstable();
        let mut merged: Vec<(i64, i64)> = Vec::new();
        for (from, to) in alive {
            match merged.last_mut() {
                Some(last) if from <= last.1 => last.1 = last.1.max(to),
                _ => merged.push((from, to)),
            }
        }
        let mut held = 0;
        let mut runs = 0;
        for occupancy in self
            .occupancy
            .iter()
            .filter(|occupancy| self.counts(Some(occupancy.task_id)))
        {
            let time: i64 = occupancy
                .spans
                .iter()
                .flat_map(|&span| {
                    merged
                        .iter()
                        .map(move |&(from, to)| overlap(span, from, to))
                })
                .sum();
            if time > 0 {
                held += time;
                runs += 1;
            }
        }
        ((capacity > 0).then(|| float(held) / float(capacity)), runs)
    }

    /// The time-weighted mean and the maximum of `candidates` in the
    /// window, the seconds free slots had no candidate while ready tasks
    /// waited, and the samples that cover it; `None` without a sample.
    fn candidates(&self, start: i64, end: i64) -> Option<(f64, f64, i64, usize)> {
        let samples: Vec<(i64, &Value)> = self
            .events
            .iter()
            .filter(|event| event.kind == CANDIDATES_SAMPLED)
            .filter_map(|event| Some((event_ms(event)?, &event.payload)))
            .collect();
        let mut weighted = 0.0;
        let mut covered = 0;
        let mut max: Option<f64> = None;
        let mut starved = 0;
        let mut used = 0;
        for (index, (at, payload)) in samples.iter().enumerate() {
            let until = samples
                .get(index + 1)
                .map_or(self.now * 1000, |next| next.0);
            let time = overlap((*at, until), start, end);
            if time == 0 {
                continue;
            }
            let number = |key: &str| payload.get(key).and_then(Value::as_i64).unwrap_or(0);
            let candidates = float(number("candidates"));
            weighted += candidates * float(time);
            covered += time;
            used += 1;
            max = Some(max.map_or(candidates, |max| max.max(candidates)));
            if number("free_slots") > 0 && number("candidates") == 0 && number("ready") > 0 {
                starved += time;
            }
        }
        (covered > 0).then(|| {
            (
                weighted / float(covered),
                max.unwrap_or(0.0),
                starved / 1000,
                used,
            )
        })
    }

    /// The findings (ADR-0047 decision 18) as their events tell them up to
    /// `upto`: how many were open or proposed then, how many were recorded
    /// after `after`, and how long those resolved after `after` took from
    /// their first sighting.
    fn findings(&self, after: EventId, upto: EventId) -> Findings {
        let mut status: HashMap<i64, (String, Option<i64>)> = HashMap::new();
        let mut result = Findings::default();
        for event in self
            .events
            .iter()
            .filter(|event| event.id <= upto && self.counts(event.task_id))
        {
            let Some(id) = event.payload.get("finding_id").and_then(Value::as_i64) else {
                continue;
            };
            match event.kind.as_str() {
                "finding_recorded" => {
                    status.insert(id, ("open".to_owned(), event_ms(event)));
                    if event.id > after {
                        result.recorded += 1;
                    }
                }
                "finding_status_changed" => {
                    let to = event.payload["to"].as_str().unwrap_or(UNKNOWN).to_owned();
                    let entry = status.entry(id).or_insert((String::new(), None));
                    if to == "resolved"
                        && event.id > after
                        && let (Some(seen), Some(at)) = (entry.1, event_ms(event))
                    {
                        result.resolve_secs.push((at - seen) / 1000);
                    }
                    entry.0 = to;
                }
                _ => {}
            }
        }
        result.open_at_end = status
            .values()
            .filter(|(status, _)| matches!(status.as_str(), "open" | "proposed"))
            .count();
        result
    }
}

#[derive(Default)]
struct Findings {
    open_at_end: usize,
    recorded: usize,
    resolve_secs: Vec<i64>,
}

fn json(value: &impl Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}
