//! `kpi` (ADR-0051 decision 9): the queue's reads that
//! [`crate::domain::kpi::kpi`] derives the KPIs from. Reads only.
use anyhow::{Result, anyhow};

use super::Queue;
use crate::domain::kpi::{Kpi, KpiConfig, KpiInput, KpiQuery, kpi as derive};

/// Where the host is: the time zone at `now` (seconds east of UTC) and the
/// logical cores.
#[derive(Debug, Clone, Copy)]
pub struct Host {
    pub utc_offset_secs: i64,
    pub cores: Option<usize>,
}

/// The KPIs `query` asks for, at the unix second `now`, judged by `config`.
pub fn kpi(
    queue: &dyn Queue,
    now: i64,
    host: Host,
    config: &KpiConfig,
    query: &KpiQuery,
) -> Result<Kpi> {
    let events = queue.all_events()?;
    let goals = queue.task_goals()?;
    let kinds = queue.task_kinds()?;
    derive(
        &KpiInput {
            events: &events,
            goals: &goals,
            kinds: &kinds,
            now,
            utc_offset_secs: host.utc_offset_secs,
            cores: host.cores,
            config,
        },
        query,
    )
    .map_err(|error| anyhow!(error))
}
