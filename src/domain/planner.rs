//! A planner session (ADR-0041 decisions 1, 6, 12, 13): an on-demand cmux
//! workspace where a planner writes goals and tasks and submits them as a
//! proposal. A person opens one with `dagq plan`, the runtime opens one for
//! a proposal it sends back or a follow_up draft. Each is recorded apart
//! (`planners`), with the pids and heartbeat of its session wrapper, so its
//! liveness and idleness are judged the way a worker's are.

use serde::Serialize;

use super::{PlannerId, PlannerOrigin, PlannerState, ProposalId, heartbeat_stale};

/// One planner as the queue records it. Times are Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannerSession {
    pub id: PlannerId,
    /// Who opened it: `person` (`dagq plan`) or `runtime`.
    pub origin: PlannerOrigin,
    /// The proposal the runtime opened it for; a person's planner has none
    /// until it submits (the proposal then names its workspace).
    pub proposal_id: Option<ProposalId>,
    /// The cmux workspace's UUID, once cmux created it (ADR-0026).
    pub workspace_id: Option<String>,
    pub wrapper_pid: Option<u32>,
    pub agent_pid: Option<u32>,
    pub heartbeat_at: Option<i64>,
    /// The agent's exit code, recorded by the wrapper.
    pub exit_code: Option<i32>,
    pub exited_at: Option<i64>,
    /// When the queue gave the planner up: its workspace failed to open or
    /// is gone.
    pub closed_at: Option<i64>,
    pub error: Option<String>,
    pub created_at: i64,
}

/// How long a planner may take from its record to its wrapper's
/// registration before it counts as `lost`, as a run's startup is bounded.
pub const PLANNER_STARTUP_SECS: i64 = 120;

/// What was looked at to judge a planner: whether cmux still lists its
/// workspace, whether its wrapper's process is alive, the idle marker its
/// agent's `Stop` hook wrote and whether its screen shows the agent at work
/// (`None` when the screen could not be read).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannerProbe {
    pub now: i64,
    pub workspace_listed: bool,
    pub wrapper_alive: bool,
    pub idle: Option<IdleProbe>,
    pub working: Option<bool>,
}

/// The idle marker of a planner's agent: when it was written (Unix
/// seconds) and whether the agent left background work running then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleProbe {
    pub since: i64,
    pub background_running: bool,
}

impl PlannerSession {
    /// The planner's state from what `probe` saw. A closed planner or one
    /// whose workspace cmux no longer lists is `closed`; an agent that
    /// exited in a workspace still open is `exited`; a wrapper whose process
    /// is gone or whose heartbeat is older than [`HEARTBEAT_TIMEOUT_SECS`]
    /// without a recorded exit is `lost`; before the wrapper registers it is
    /// `opening`, and `lost` once [`PLANNER_STARTUP_SECS`] passed. A live agent is `working` while its screen shows a turn,
    /// and `idle` once its `Stop` hook wrote the marker with no background
    /// work left; without a marker yet it is `working`.
    pub fn state(&self, probe: &PlannerProbe) -> PlannerState {
        if self.closed_at.is_some() || (self.workspace_id.is_some() && !probe.workspace_listed) {
            return PlannerState::Closed;
        }
        if self.workspace_id.is_none() || self.wrapper_pid.is_none() {
            // A workspace that never ran its wrapper (cmux could not start
            // it, or the opening process died before it recorded the
            // workspace) is not opening any more.
            return if probe.now - self.created_at > PLANNER_STARTUP_SECS {
                PlannerState::Lost
            } else {
                PlannerState::Opening
            };
        }
        if self.exited_at.is_some() {
            return PlannerState::Exited;
        }
        let age = self.heartbeat_at.map_or(i64::MAX, |at| probe.now - at);
        if heartbeat_stale(probe.wrapper_alive, age) {
            return PlannerState::Lost;
        }
        if probe.working == Some(true) {
            return PlannerState::Working;
        }
        match probe.idle {
            Some(idle) if !idle.background_running => PlannerState::Idle,
            _ => PlannerState::Working,
        }
    }
}

impl PlannerState {
    /// Whether the planner's session can still take text: it is opening,
    /// working or idle in a workspace cmux lists.
    pub fn alive(self) -> bool {
        matches!(self, Self::Opening | Self::Working | Self::Idle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::HEARTBEAT_TIMEOUT_SECS;

    fn session() -> PlannerSession {
        PlannerSession {
            id: PlannerId::new(1),
            origin: PlannerOrigin::Person,
            proposal_id: None,
            workspace_id: Some("W".into()),
            wrapper_pid: Some(10),
            agent_pid: Some(11),
            heartbeat_at: Some(100),
            exit_code: None,
            exited_at: None,
            closed_at: None,
            error: None,
            created_at: 90,
        }
    }

    fn probe() -> PlannerProbe {
        PlannerProbe {
            now: 110,
            workspace_listed: true,
            wrapper_alive: true,
            idle: None,
            working: None,
        }
    }

    #[test]
    fn a_planner_is_judged_like_a_worker_session() {
        let live = session();
        assert_eq!(live.state(&probe()), PlannerState::Working);
        let idle = IdleProbe {
            since: 105,
            background_running: false,
        };
        let idle_probe = PlannerProbe {
            idle: Some(idle),
            ..probe()
        };
        assert_eq!(live.state(&idle_probe), PlannerState::Idle);
        // The screen of a new turn wins over an older marker.
        assert_eq!(
            live.state(&PlannerProbe {
                working: Some(true),
                ..idle_probe
            }),
            PlannerState::Working
        );
        assert_eq!(
            live.state(&PlannerProbe {
                idle: Some(IdleProbe {
                    background_running: true,
                    ..idle
                }),
                ..probe()
            }),
            PlannerState::Working
        );
        // A dead wrapper, or one silent past the heartbeat timeout, is lost.
        assert_eq!(
            live.state(&PlannerProbe {
                wrapper_alive: false,
                ..probe()
            }),
            PlannerState::Lost
        );
        assert_eq!(
            live.state(&PlannerProbe {
                now: 100 + HEARTBEAT_TIMEOUT_SECS + 1,
                ..probe()
            }),
            PlannerState::Lost
        );
        assert!(PlannerState::Idle.alive() && PlannerState::Working.alive());
        assert!(!PlannerState::Lost.alive());
    }

    #[test]
    fn a_planner_opens_exits_and_closes() {
        let opening = PlannerSession {
            workspace_id: None,
            wrapper_pid: None,
            ..session()
        };
        assert_eq!(opening.state(&probe()), PlannerState::Opening);
        assert!(PlannerState::Opening.alive());
        let unregistered = PlannerSession {
            wrapper_pid: None,
            ..session()
        };
        assert_eq!(unregistered.state(&probe()), PlannerState::Opening);
        assert_eq!(
            opening.state(&PlannerProbe {
                now: 90 + PLANNER_STARTUP_SECS + 1,
                ..probe()
            }),
            PlannerState::Lost
        );
        let exited = PlannerSession {
            exited_at: Some(108),
            exit_code: Some(0),
            ..session()
        };
        assert_eq!(exited.state(&probe()), PlannerState::Exited);
        assert!(!PlannerState::Exited.alive());
        // A workspace cmux no longer lists, or a closed record, is closed.
        assert_eq!(
            session().state(&PlannerProbe {
                workspace_listed: false,
                ..probe()
            }),
            PlannerState::Closed
        );
        let closed = PlannerSession {
            closed_at: Some(109),
            ..session()
        };
        assert_eq!(closed.state(&probe()), PlannerState::Closed);
        assert!(!PlannerState::Closed.alive());
    }
}
