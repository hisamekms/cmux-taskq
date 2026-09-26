//! The `[kpi]` settings and targets (ADR-0051 decisions 17–19): what the
//! repository's `dagq.toml` and the host's `host.toml` say, merged with
//! the host winning (except `max_improvement_proposals`), and the targets
//! judged over the periods: `missed` for a judged period off target,
//! `breach` once `breach_periods` judged days (`breach_weeks` weeks) in a
//! row are off it. A period with too few samples is skipped without
//! breaking the streak; a judged period on target ends it.
use std::collections::BTreeMap;

use serde::Serialize;

use super::{ALL, ConfigReport, Kpis, Measure};

pub const DEFAULT_MIN_SAMPLES: usize = 5;
pub const DEFAULT_BREACH_PERIODS: usize = 3;
pub const DEFAULT_BREACH_WEEKS: usize = 2;
pub const DEFAULT_MAX_IMPROVEMENT_PROPOSALS: usize = 2;

/// The statistic of a KPI a target bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stat {
    Median,
    P90,
    Value,
}

impl Stat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Median => "median",
            Self::P90 => "p90",
            Self::Value => "value",
        }
    }
}

impl std::str::FromStr for Stat {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "median" => Ok(Self::Median),
            "p90" => Ok(Self::P90),
            "value" => Ok(Self::Value),
            _ => Err(format!("{text:?} is not a stat; use median, p90 or value")),
        }
    }
}

/// One `[kpi.targets."<kpi>"]` table.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub kpi: String,
    /// The kind of task it bounds; every run (`all`) without one.
    pub kind: Option<String>,
    /// The value, or a spread's median, without one.
    pub stat: Option<Stat>,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl Target {
    /// The stratum it bounds: `all` or `kind=<kind>`.
    pub fn stratum(&self) -> String {
        self.kind
            .as_ref()
            .map_or_else(|| ALL.to_owned(), |kind| format!("kind={kind}"))
    }
}

/// What one file's `[kpi]` tables set; a key it does not set is `None`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KpiSettings {
    pub min_samples: Option<usize>,
    pub breach_periods: Option<usize>,
    pub breach_weeks: Option<usize>,
    pub max_improvement_proposals: Option<usize>,
    pub targets: Vec<Target>,
}

impl KpiSettings {
    /// These settings with `top`'s over them, key by key and target by
    /// target (a target is its KPI and stratum): the queue's `host.toml`
    /// over the host-wide one.
    pub fn overlay(mut self, top: Self) -> Self {
        self.min_samples = top.min_samples.or(self.min_samples);
        self.breach_periods = top.breach_periods.or(self.breach_periods);
        self.breach_weeks = top.breach_weeks.or(self.breach_weeks);
        self.max_improvement_proposals = top
            .max_improvement_proposals
            .or(self.max_improvement_proposals);
        for target in top.targets {
            self.targets
                .retain(|kept| (&kept.kpi, kept.stratum()) != (&target.kpi, target.stratum()));
            self.targets.push(target);
        }
        self
    }
}

/// A target and the file it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct SourcedTarget {
    pub target: Target,
    /// `repository` or `host`.
    pub source: &'static str,
}

/// The settings the KPIs are judged by.
#[derive(Debug, Clone, PartialEq)]
pub struct KpiConfig {
    pub min_samples: usize,
    pub breach_periods: usize,
    pub breach_weeks: usize,
    pub max_improvement_proposals: usize,
    pub targets: Vec<SourcedTarget>,
    pub sources: BTreeMap<&'static str, &'static str>,
}

impl Default for KpiConfig {
    fn default() -> Self {
        Self::merge(None, None)
    }
}

impl KpiConfig {
    /// The repository's settings (`dagq.toml`) with the host's
    /// (`host.toml`) over them. `max_improvement_proposals` is the
    /// repository's alone (decision 19); no target is built in (decision 17).
    pub fn merge(repository: Option<&KpiSettings>, host: Option<&KpiSettings>) -> Self {
        let mut sources = BTreeMap::new();
        let mut pick = |name: &'static str, get: fn(&KpiSettings) -> Option<usize>, default| {
            let (value, source) = match (host.and_then(get), repository.and_then(get)) {
                (Some(value), _) => (value, "host"),
                (None, Some(value)) => (value, "repository"),
                (None, None) => (default, "default"),
            };
            sources.insert(name, source);
            value
        };
        let min_samples = pick("min_samples", |s| s.min_samples, DEFAULT_MIN_SAMPLES);
        let breach_periods = pick(
            "breach_periods",
            |s| s.breach_periods,
            DEFAULT_BREACH_PERIODS,
        );
        let breach_weeks = pick("breach_weeks", |s| s.breach_weeks, DEFAULT_BREACH_WEEKS);
        let (max_improvement_proposals, source) =
            match repository.and_then(|s| s.max_improvement_proposals) {
                Some(value) => (value, "repository"),
                None => (DEFAULT_MAX_IMPROVEMENT_PROPOSALS, "default"),
            };
        sources.insert("max_improvement_proposals", source);
        let mut targets: Vec<SourcedTarget> = Vec::new();
        for (settings, source) in [(repository, "repository"), (host, "host")] {
            for target in settings.map(|s| s.targets.as_slice()).unwrap_or_default() {
                targets.retain(|kept| {
                    (&kept.target.kpi, kept.target.stratum()) != (&target.kpi, target.stratum())
                });
                targets.push(SourcedTarget {
                    target: target.clone(),
                    source,
                });
            }
        }
        targets.sort_by(|a, b| {
            (&a.target.kpi, a.target.stratum()).cmp(&(&b.target.kpi, b.target.stratum()))
        });
        Self {
            min_samples,
            breach_periods,
            breach_weeks,
            max_improvement_proposals,
            targets,
            sources,
        }
    }

    pub fn report(&self) -> ConfigReport {
        ConfigReport {
            min_samples: self.min_samples,
            breach_periods: self.breach_periods,
            breach_weeks: self.breach_weeks,
            max_improvement_proposals: self.max_improvement_proposals,
            sources: self.sources.clone(),
        }
    }
}

/// A period a target is judged on.
pub struct JudgedPeriod<'a> {
    pub label: &'a str,
    pub kpis: &'a Kpis<Measure>,
    pub partial: bool,
    /// Listed in the output, not only looked back on.
    pub listed: bool,
}

/// One target on one listed period.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TargetPeriod {
    pub period: String,
    pub value: Option<f64>,
    pub n: usize,
    pub judged: bool,
    /// Why not judged: `partial`, `no_value` or `small_sample`.
    pub reason: Option<&'static str>,
    /// On target; null when not judged.
    pub met: Option<bool>,
}

/// A target and how the periods stand against it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TargetReport {
    pub kpi: String,
    pub stratum: String,
    pub stat: &'static str,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// `repository` or `host`.
    pub source: &'static str,
    /// `ok` (the latest judged period is on target), `missed` (off target,
    /// fewer periods in a row than a breach), `breach` or `not_judged` (no
    /// period could be judged).
    pub state: &'static str,
    /// The judged periods off target in a row, up to the latest.
    pub streak: usize,
    /// The first period of the breach.
    pub breach_since: Option<String>,
    pub periods: Vec<TargetPeriod>,
}

/// Judge every target of `config` over `periods` (oldest first), a breach
/// after `breach_after` judged periods off target in a row.
pub fn judge(
    config: &KpiConfig,
    periods: &[JudgedPeriod<'_>],
    breach_after: usize,
) -> Vec<TargetReport> {
    config
        .targets
        .iter()
        .map(|sourced| {
            let target = &sourced.target;
            let stratum = target.stratum();
            let mut stat = target.stat;
            let mut streak: Vec<&str> = Vec::new();
            let mut any_judged = false;
            let mut listed = Vec::new();
            for period in periods {
                let measure = period.kpis.get(&target.kpi).and_then(|s| s.get(&stratum));
                let resolved = stat.or_else(|| measure.map(Measure::primary_stat));
                stat = stat.or(resolved);
                let value = measure.zip(resolved).and_then(|(m, s)| m.stat(s));
                let reason = if period.partial {
                    Some("partial")
                } else if value.is_none() {
                    Some("no_value")
                } else if !measure.is_some_and(|m| m.enough(config.min_samples)) {
                    Some("small_sample")
                } else {
                    None
                };
                let met = value.filter(|_| reason.is_none()).map(|value| {
                    target.min.is_none_or(|min| value >= min)
                        && target.max.is_none_or(|max| value <= max)
                });
                match met {
                    Some(true) => {
                        any_judged = true;
                        streak.clear();
                    }
                    Some(false) => {
                        any_judged = true;
                        streak.push(period.label);
                    }
                    None => {}
                }
                if period.listed {
                    listed.push(TargetPeriod {
                        period: period.label.to_owned(),
                        value,
                        n: measure.map_or(0, |m| m.n),
                        judged: met.is_some(),
                        reason,
                        met,
                    });
                }
            }
            let breach = !streak.is_empty() && streak.len() >= breach_after.max(1);
            TargetReport {
                kpi: target.kpi.clone(),
                stratum,
                stat: stat.unwrap_or(Stat::Value).as_str(),
                min: target.min,
                max: target.max,
                source: sourced.source,
                state: if breach {
                    "breach"
                } else if !streak.is_empty() {
                    "missed"
                } else if any_judged {
                    "ok"
                } else {
                    "not_judged"
                },
                streak: streak.len(),
                breach_since: breach.then(|| streak[0].to_owned()),
                periods: listed,
            }
        })
        .collect()
}
