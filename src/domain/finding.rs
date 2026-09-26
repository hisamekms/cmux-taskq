//! Findings (ADR-0044 decision 18): what the observer found wrong, kept as
//! one record per problem. The same kind, target and subject is one
//! problem: recording it again adds an occurrence and its evidence to the
//! existing finding instead of writing another one, and a record that
//! brings nothing new changes nothing.
use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use super::{
    AskId, DomainError, EventId, FindingId, GoalId, ProposalId, ProposalStatus, RunEvent, RunId,
    TaskId, error::require,
};

string_enum!(FindingStatus {
    Open => "open",
    Proposed => "proposed",
    Resolved => "resolved",
    Dismissed => "dismissed",
});

impl FindingStatus {
    /// `open` and `proposed` still ask for a remedy; `findings` lists them
    /// by default and the same problem may have one of them only.
    pub const fn is_unsettled(self) -> bool {
        matches!(self, Self::Open | Self::Proposed)
    }
}

string_enum!(Impact {
    High => "high",
    Normal => "normal",
    Low => "low",
});

impl Impact {
    /// Larger is more severe; `findings` lists the larger first.
    pub const fn rank(self) -> i64 {
        match self {
            Self::High => 2,
            Self::Normal => 1,
            Self::Low => 0,
        }
    }
}

/// What a finding is about: a run (and its task), a task, a goal, or the
/// queue as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingTarget {
    Queue,
    Goal(GoalId),
    Task(TaskId),
    Run(RunId),
}

impl FindingTarget {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Goal(_) => "goal",
            Self::Task(_) => "task",
            Self::Run(_) => "run",
        }
    }
}

/// A finding as stored. `task_id` is also set for a run target (the run's
/// task); the other IDs are set only for their own target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub id: FindingId,
    pub kind: String,
    /// `run`, `task`, `goal` or `queue`.
    pub target: String,
    pub task_id: Option<TaskId>,
    pub run_id: Option<RunId>,
    pub goal_id: Option<GoalId>,
    /// What tells the problem apart within its target (a path, an alert,
    /// a threshold's name); empty when the target is enough.
    pub subject: String,
    pub summary: String,
    pub detail: String,
    pub impact: Impact,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub occurrences: i64,
    /// The run events that show it, oldest record first.
    pub evidence: Vec<EventId>,
    pub status: FindingStatus,
    /// Why it was last resolved or dismissed.
    pub status_reason: Option<String>,
    pub proposal_id: Option<ProposalId>,
    /// Why a proposal should remedy it (ADR-0044 decision 19); set once.
    pub propose_reason: Option<String>,
    pub propose_requested_at: Option<i64>,
    pub recorded_by: String,
    pub updated_at: i64,
}

/// A finding to record: a new one, or an occurrence of an existing one.
#[derive(Debug, Clone)]
pub struct NewFinding {
    /// A lowercase slug: stall, failure, wait, capacity, threshold,
    /// conflict_hotspot and whatever the observation needs.
    pub kind: String,
    pub target: FindingTarget,
    pub subject: String,
    pub summary: String,
    /// `None` keeps an existing finding's detail (a new one gets none).
    pub detail: Option<String>,
    /// `None` keeps an existing finding's impact (a new one is `normal`).
    pub impact: Option<Impact>,
    pub evidence: Vec<EventId>,
    /// Ask for a proposal, with the reason.
    pub propose: Option<String>,
    /// `DAGQ_ROLE` of the writer, or `human`.
    pub by: String,
}

impl NewFinding {
    pub fn validate(&self) -> Result<(), DomainError> {
        require(
            !self.kind.is_empty()
                && self.kind.len() <= 64
                && self.kind.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'
                }),
            || DomainError::InvalidFindingKind {
                kind: self.kind.clone(),
            },
        )?;
        require(!self.summary.trim().is_empty(), || DomainError::Blank {
            field: "finding summary",
        })?;
        require(
            self.propose.as_ref().is_none_or(|r| !r.trim().is_empty()),
            || DomainError::Blank {
                field: "propose reason",
            },
        )?;
        require(!self.by.trim().is_empty(), || DomainError::Blank {
            field: "recorded_by",
        })?;
        require(self.evidence.iter().all(|id| id.as_i64() > 0), || {
            DomainError::NonPositiveId {
                field: "evidence event ID",
            }
        })
    }

    /// The evidence without repeats, in the given order.
    pub fn distinct_evidence(&self) -> Vec<EventId> {
        let mut seen = Vec::new();
        for id in &self.evidence {
            if !seen.contains(id) {
                seen.push(*id);
            }
        }
        seen
    }
}

/// What recording an existing finding again changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingUpdate {
    pub finding: Finding,
    /// Evidence the finding did not hold yet.
    pub added_evidence: Vec<EventId>,
    /// New evidence arrived: one more occurrence, seen now.
    pub occurred: bool,
    /// A `resolved` finding occurred again and is `open` once more.
    pub reopened: bool,
    /// The fields that changed.
    pub changed: Vec<&'static str>,
}

/// What `finding record` returns: the finding as it is now, whether this
/// record created it, and the fields it changed (empty: nothing new, so
/// nothing was written).
#[derive(Debug, Clone, Serialize)]
pub struct FindingOutcome {
    #[serde(flatten)]
    pub finding: Finding,
    pub created: bool,
    pub changed: Vec<&'static str>,
}

/// One row of `findings`: the finding, the status of its proposal, its
/// open asks and, with `--full`, its evidence events.
#[derive(Debug, Clone, Serialize)]
pub struct FindingView {
    #[serde(flatten)]
    pub finding: Finding,
    pub proposal_status: Option<ProposalStatus>,
    pub open_asks: Vec<AskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_events: Option<Vec<RunEvent>>,
}

/// Record `new` onto `existing`, the finding of the same kind, target and
/// subject. New evidence is one more occurrence seen `now`; a `resolved`
/// finding that occurs again is `open` once more (its remedy did not hold),
/// while a `dismissed` one only counts the occurrence: nobody chose to
/// remedy it, so its text, impact and proposal mark stay. `None` when
/// nothing would change, so an unchanged finding is not written again
/// (ADR-0044 decision 21).
pub fn merge(existing: &Finding, new: &NewFinding, now: i64) -> Option<FindingUpdate> {
    let mut finding = existing.clone();
    let added: Vec<EventId> = new
        .distinct_evidence()
        .into_iter()
        .filter(|id| !existing.evidence.contains(id))
        .collect();
    let occurred = !added.is_empty();
    let mut changed = Vec::new();
    if occurred {
        finding.evidence.extend(&added);
        finding.occurrences += 1;
        finding.last_seen_at = now;
        changed.extend(["evidence", "occurrences", "last_seen_at"]);
    }
    let reopened = occurred && existing.status == FindingStatus::Resolved;
    if reopened {
        finding.status = FindingStatus::Open;
        finding.status_reason = None;
        changed.push("status");
    }
    if finding.status != FindingStatus::Dismissed {
        if new.summary != finding.summary {
            finding.summary.clone_from(&new.summary);
            changed.push("summary");
        }
        if let Some(detail) = &new.detail
            && *detail != finding.detail
        {
            finding.detail.clone_from(detail);
            changed.push("detail");
        }
        if let Some(impact) = new.impact
            && impact != finding.impact
        {
            finding.impact = impact;
            changed.push("impact");
        }
        if let Some(reason) = &new.propose
            && finding.propose_reason.is_none()
            && finding.status == FindingStatus::Open
        {
            finding.propose_reason = Some(reason.clone());
            finding.propose_requested_at = Some(now);
            changed.push("propose_reason");
        }
    }
    if changed.is_empty() {
        return None;
    }
    finding.updated_at = now;
    Some(FindingUpdate {
        finding,
        added_evidence: added,
        occurred,
        reopened,
        changed,
    })
}

/// Whether `finding` may become `to` by hand: `resolved` from `open` or
/// `proposed` (the problem no longer occurs), `dismissed` from any status
/// but itself (nobody will remedy it).
pub fn check_transition(finding: &Finding, to: FindingStatus) -> Result<(), DomainError> {
    let allowed = match to {
        FindingStatus::Resolved => finding.status.is_unsettled(),
        FindingStatus::Dismissed => finding.status != FindingStatus::Dismissed,
        FindingStatus::Open | FindingStatus::Proposed => false,
    };
    require(allowed, || DomainError::FindingNotInStatus {
        finding_id: finding.id,
        status: finding.status,
        to,
    })
}

/// `findings` order: the larger impact first, then more occurrences, then
/// the latest seen, then the newest.
pub fn by_impact(a: &Finding, b: &Finding) -> Ordering {
    b.impact
        .rank()
        .cmp(&a.impact.rank())
        .then(b.occurrences.cmp(&a.occurrences))
        .then(b.last_seen_at.cmp(&a.last_seen_at))
        .then(b.id.cmp(&a.id))
}

/// Which findings `findings` lists: the unsettled ones (`open`,
/// `proposed`) unless `all` or `statuses` says otherwise, narrowed to
/// `kinds` and a target.
#[derive(Debug, Clone, Default)]
pub struct FindingQuery {
    pub id: Option<FindingId>,
    pub all: bool,
    pub statuses: Vec<FindingStatus>,
    pub kinds: Vec<String>,
    pub target: Option<FindingTarget>,
    /// Include the evidence events in full.
    pub full: bool,
}

impl FindingQuery {
    /// Whether the query lists `finding`, apart from its target.
    pub fn admits(&self, finding: &Finding) -> bool {
        let status = if let Some(id) = self.id {
            finding.id == id
        } else if !self.statuses.is_empty() {
            self.statuses.contains(&finding.status)
        } else {
            self.all || finding.status.is_unsettled()
        };
        status && (self.kinds.is_empty() || self.kinds.contains(&finding.kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(status: FindingStatus) -> Finding {
        Finding {
            id: FindingId::new(1),
            kind: "stall".into(),
            target: "queue".into(),
            task_id: None,
            run_id: None,
            goal_id: None,
            subject: "idle_slots".into(),
            summary: "slots idle".into(),
            detail: "no candidates".into(),
            impact: Impact::Normal,
            first_seen_at: 10,
            last_seen_at: 10,
            occurrences: 1,
            evidence: vec![EventId::new(5)],
            status,
            status_reason: None,
            proposal_id: None,
            propose_reason: None,
            propose_requested_at: None,
            recorded_by: "observer".into(),
            updated_at: 10,
        }
    }

    fn record(evidence: &[i64]) -> NewFinding {
        NewFinding {
            kind: "stall".into(),
            target: FindingTarget::Queue,
            subject: "idle_slots".into(),
            summary: "slots idle".into(),
            detail: None,
            impact: None,
            evidence: evidence.iter().copied().map(EventId::new).collect(),
            propose: None,
            by: "observer".into(),
        }
    }

    #[test]
    fn a_finding_needs_a_slug_kind_a_summary_and_positive_evidence() {
        record(&[1]).validate().unwrap();
        for kind in ["", "Stall", "a b", &"k".repeat(65)] {
            assert!(matches!(
                NewFinding {
                    kind: kind.into(),
                    ..record(&[])
                }
                .validate(),
                Err(DomainError::InvalidFindingKind { .. })
            ));
        }
        let blank = |new: NewFinding| new.validate().unwrap_err().to_string();
        assert_eq!(
            blank(NewFinding {
                summary: " ".into(),
                ..record(&[])
            }),
            "finding summary must not be blank"
        );
        assert_eq!(
            blank(NewFinding {
                propose: Some("".into()),
                ..record(&[])
            }),
            "propose reason must not be blank"
        );
        assert_eq!(
            blank(NewFinding {
                by: "".into(),
                ..record(&[])
            }),
            "recorded_by must not be blank"
        );
        assert!(matches!(
            record(&[0]).validate(),
            Err(DomainError::NonPositiveId { .. })
        ));
        assert_eq!(
            record(&[3, 2, 3]).distinct_evidence(),
            vec![EventId::new(3), EventId::new(2)]
        );
    }

    #[test]
    fn new_evidence_is_one_more_occurrence_and_nothing_new_changes_nothing() {
        let existing = stored(FindingStatus::Open);
        // Evidence it already holds, same text: not written again.
        assert_eq!(merge(&existing, &record(&[5]), 20), None);
        assert_eq!(merge(&existing, &record(&[]), 20), None);
        let update = merge(&existing, &record(&[5, 7, 8, 7]), 20).unwrap();
        assert!(update.occurred && !update.reopened);
        assert_eq!(
            update.added_evidence,
            vec![EventId::new(7), EventId::new(8)]
        );
        assert_eq!(update.finding.occurrences, 2);
        assert_eq!(
            update.finding.evidence,
            [5, 7, 8].map(EventId::new).to_vec()
        );
        assert_eq!(
            (update.finding.first_seen_at, update.finding.last_seen_at),
            (10, 20)
        );
        assert_eq!(update.finding.updated_at, 20);
        // A new reading without new evidence updates the text only.
        let reread = merge(
            &existing,
            &NewFinding {
                summary: "slots idle for 5h".into(),
                detail: Some("the planner is closed".into()),
                impact: Some(Impact::High),
                propose: Some("recurs daily".into()),
                ..record(&[])
            },
            30,
        )
        .unwrap();
        assert!(!reread.occurred);
        assert_eq!(
            reread.changed,
            vec!["summary", "detail", "impact", "propose_reason"]
        );
        assert_eq!(reread.finding.occurrences, 1);
        assert_eq!(reread.finding.last_seen_at, 10);
        assert_eq!(reread.finding.propose_requested_at, Some(30));
        // The proposal mark is set once.
        let marked = reread.finding;
        assert_eq!(
            merge(
                &marked,
                &NewFinding {
                    summary: marked.summary.clone(),
                    propose: Some("again".into()),
                    ..record(&[])
                },
                40
            ),
            None
        );
    }

    #[test]
    fn a_resolved_finding_reopens_and_a_dismissed_one_only_counts() {
        let resolved = Finding {
            status_reason: Some("gone".into()),
            ..stored(FindingStatus::Resolved)
        };
        assert_eq!(merge(&resolved, &record(&[5]), 20), None);
        let update = merge(&resolved, &record(&[9]), 20).unwrap();
        assert!(update.reopened);
        assert_eq!(update.finding.status, FindingStatus::Open);
        assert_eq!(update.finding.status_reason, None);

        let dismissed = stored(FindingStatus::Dismissed);
        let quiet = NewFinding {
            summary: "other words".into(),
            impact: Some(Impact::High),
            propose: Some("now".into()),
            ..record(&[])
        };
        assert_eq!(merge(&dismissed, &quiet, 20), None);
        let counted = merge(
            &dismissed,
            &NewFinding {
                evidence: vec![EventId::new(9)],
                ..quiet
            },
            20,
        )
        .unwrap();
        assert!(!counted.reopened);
        assert_eq!(counted.finding.status, FindingStatus::Dismissed);
        assert_eq!(counted.finding.occurrences, 2);
        assert_eq!(counted.finding.summary, "slots idle");
        assert_eq!(counted.finding.impact, Impact::Normal);
        assert_eq!(counted.finding.propose_reason, None);

        // A proposed finding keeps its status and takes no new mark.
        let proposed = stored(FindingStatus::Proposed);
        let update = merge(
            &proposed,
            &NewFinding {
                propose: Some("x".into()),
                ..record(&[6])
            },
            20,
        )
        .unwrap();
        assert_eq!(update.finding.status, FindingStatus::Proposed);
        assert_eq!(update.finding.propose_reason, None);
    }

    #[test]
    fn resolve_needs_an_unsettled_finding_and_dismiss_any_other() {
        use FindingStatus::*;
        for (from, to, ok) in [
            (Open, Resolved, true),
            (Proposed, Resolved, true),
            (Resolved, Resolved, false),
            (Dismissed, Resolved, false),
            (Open, Dismissed, true),
            (Proposed, Dismissed, true),
            (Resolved, Dismissed, true),
            (Dismissed, Dismissed, false),
            (Resolved, Open, false),
            (Open, Proposed, false),
        ] {
            assert_eq!(
                check_transition(&stored(from), to).is_ok(),
                ok,
                "{from:?} -> {to:?}"
            );
        }
        assert_eq!(
            check_transition(&stored(Dismissed), Resolved)
                .unwrap_err()
                .to_string(),
            "finding 1 is dismissed; it cannot become resolved"
        );
    }

    #[test]
    fn findings_list_the_larger_impact_then_more_occurrences_then_the_latest() {
        let finding = |id, impact, occurrences, last_seen_at| Finding {
            id: FindingId::new(id),
            impact,
            occurrences,
            last_seen_at,
            ..stored(FindingStatus::Open)
        };
        let mut findings = [
            finding(1, Impact::Low, 9, 90),
            finding(2, Impact::Normal, 1, 10),
            finding(3, Impact::Normal, 3, 10),
            finding(4, Impact::High, 1, 10),
            finding(5, Impact::Normal, 3, 20),
            finding(6, Impact::Normal, 3, 20),
        ];
        findings.sort_by(by_impact);
        assert_eq!(
            findings.iter().map(|f| f.id.as_i64()).collect::<Vec<_>>(),
            vec![4, 6, 5, 3, 2, 1]
        );
        assert_eq!(
            [Impact::High, Impact::Normal, Impact::Low].map(Impact::rank),
            [2, 1, 0]
        );
    }

    #[test]
    fn the_query_lists_unsettled_findings_unless_told_otherwise() {
        let open = stored(FindingStatus::Open);
        let resolved = stored(FindingStatus::Resolved);
        let query = FindingQuery::default();
        assert!(query.admits(&open) && !query.admits(&resolved));
        let all = FindingQuery {
            all: true,
            ..FindingQuery::default()
        };
        assert!(all.admits(&resolved));
        let only = FindingQuery {
            statuses: vec![FindingStatus::Resolved],
            ..FindingQuery::default()
        };
        assert!(only.admits(&resolved) && !only.admits(&open));
        let by_id = FindingQuery {
            id: Some(FindingId::new(1)),
            ..FindingQuery::default()
        };
        assert!(by_id.admits(&resolved));
        let kinds = FindingQuery {
            kinds: vec!["failure".into()],
            ..FindingQuery::default()
        };
        assert!(!kinds.admits(&open));
        assert_eq!(
            [
                FindingTarget::Queue,
                FindingTarget::Goal(GoalId::new(1)),
                FindingTarget::Task(TaskId::new(1)),
                FindingTarget::Run(RunId::new("r").unwrap()),
            ]
            .map(|t| t.name()),
            ["queue", "goal", "task", "run"]
        );
    }
}
