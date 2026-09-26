//! The tokens of `stats` (task 199): what the Claude sessions used, from
//! the `tokens` their `session_closed` recorded
//! ([`crate::domain::tokens`]), per run (and per kind of session in it),
//! as totals and medians per goal, kind of task and overall, and per kind
//! of session over the window.

use std::collections::BTreeMap;

use serde::{Serialize, Serializer};
use serde_json::Value;

use super::{Summary, median};

/// US dollars, kept in millionths so that sums stay exact; serialized as a
/// number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Usd(i64);

impl Usd {
    fn of(value: &Value) -> Option<Self> {
        #[allow(clippy::cast_possible_truncation)]
        value.as_f64().map(|usd| Self((usd * 1e6).round() as i64))
    }
}

impl Serialize for Usd {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[allow(clippy::cast_precision_loss)]
        serializer.serialize_f64(self.0 as f64 / 1e6)
    }
}

/// The tokens of some sessions, summed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TokenTotals {
    /// Sessions (spans) that recorded their tokens.
    pub sessions: usize,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    /// All four kinds together.
    pub total: i64,
    /// The sum of the cost Claude Code gave; null when no session had one.
    pub cost_usd: Option<Usd>,
    /// Sessions whose cost was recorded.
    pub cost_sessions: usize,
}

impl TokenTotals {
    /// Add the `tokens` of one `session_closed`.
    pub fn add(&mut self, tokens: &Value) {
        let count = |key: &str| tokens[key].as_i64().unwrap_or(0);
        self.sessions += 1;
        self.input += count("input");
        self.output += count("output");
        self.cache_read += count("cache_read");
        self.cache_creation += count("cache_creation");
        self.total = self.input + self.output + self.cache_read + self.cache_creation;
        if let Some(cost) = Usd::of(&tokens["cost_usd"]) {
            self.cost_sessions += 1;
            self.cost_usd = Some(Usd(self.cost_usd.unwrap_or_default().0 + cost.0));
        }
    }
}

/// A run's tokens: all its sessions', and per kind of session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RunTokens {
    #[serde(flatten)]
    pub totals: TokenTotals,
    pub by_kind: BTreeMap<String, TokenTotals>,
}

/// The tokens of a run from its spans' (kind, `tokens`); `None` when none
/// recorded them.
pub fn per_run<'a>(spans: impl IntoIterator<Item = (&'a str, &'a Value)>) -> Option<RunTokens> {
    let mut run = RunTokens::default();
    for (kind, tokens) in spans {
        run.totals.add(tokens);
        run.by_kind.entry(kind.to_owned()).or_default().add(tokens);
    }
    (run.totals.sessions > 0).then_some(run)
}

/// The cost over a set of runs: of the runs that have one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CostSummary {
    pub count: usize,
    pub total: Option<Usd>,
    pub median: Option<Usd>,
}

/// The tokens of a set of runs: per kind of token the count of runs, their
/// sum and the median of the runs'; only runs with tokens are counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TokenSummary {
    pub runs: usize,
    pub input: Summary,
    pub output: Summary,
    pub cache_read: Summary,
    pub cache_creation: Summary,
    pub total: Summary,
    pub cost_usd: CostSummary,
}

/// The tokens of `runs`.
pub fn summary<'a>(runs: impl IntoIterator<Item = &'a RunTokens>) -> TokenSummary {
    let runs: Vec<&TokenTotals> = runs.into_iter().map(|run| &run.totals).collect();
    let of = |value: fn(&TokenTotals) -> i64| {
        let mut values: Vec<i64> = runs.iter().map(|run| value(run)).collect();
        Summary {
            count: values.len(),
            total: values.iter().sum(),
            median: median(&mut values),
        }
    };
    let mut costs: Vec<i64> = runs
        .iter()
        .filter_map(|run| run.cost_usd.map(|usd| usd.0))
        .collect();
    TokenSummary {
        runs: runs.len(),
        input: of(|r| r.input),
        output: of(|r| r.output),
        cache_read: of(|r| r.cache_read),
        cache_creation: of(|r| r.cache_creation),
        total: of(|r| r.total),
        cost_usd: CostSummary {
            count: costs.len(),
            total: (!costs.is_empty()).then(|| Usd(costs.iter().sum())),
            median: median(&mut costs).map(Usd),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_run_sums_its_sessions_and_a_set_of_runs_has_medians() {
        let worker = json!({"input": 10, "output": 20, "cache_read": 300, "cache_creation": 40,
                            "messages": 3, "cost_usd": 0.25});
        let review = json!({"input": 1, "output": 2, "cache_read": 3, "cache_creation": 4});
        let run = per_run([
            ("worker", &worker),
            ("review", &review),
            ("worker", &worker),
        ])
        .unwrap();
        assert_eq!(run.totals.sessions, 3);
        assert_eq!(run.totals.input, 21);
        assert_eq!(run.totals.total, 750);
        assert_eq!(run.totals.cost_usd, Some(Usd(500_000)));
        assert_eq!(run.totals.cost_sessions, 2);
        assert_eq!(run.by_kind["worker"].output, 40);
        assert_eq!(run.by_kind["review"].cost_usd, None);
        assert_eq!(per_run([]), None);

        let other = per_run([("worker", &review)]).unwrap();
        let summary = summary([&run, &other]);
        assert_eq!(summary.runs, 2);
        assert_eq!(summary.input.total, 22);
        assert_eq!(summary.input.median, Some(11));
        assert_eq!(summary.total.count, 2);
        assert_eq!(summary.cost_usd.count, 1);
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["cost_usd"]["total"], json!(0.5));
        assert_eq!(json["cost_usd"]["median"], json!(0.5));
        let run = serde_json::to_value(&run).unwrap();
        assert_eq!(run["total"], 750);
        assert_eq!(run["by_kind"]["review"]["total"], 10);
        assert_eq!(super::summary([]), TokenSummary::default());
    }
}
