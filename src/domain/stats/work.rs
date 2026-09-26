//! The work breakdown of `stats` (task 514): what a run's own sessions
//! (worker, resume, revise) spent their time on, from the `work` their
//! `session_closed` recorded ([`crate::domain::worktime`]), per run and,
//! as totals, medians and shares, per goal, kind and overall.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::{median, sessions::Ratio};

/// Runs and failures of one kind of heavy command.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CommandCount {
    pub runs: i64,
    pub failed: i64,
}

/// A run's work breakdown: its sessions that recorded one, summed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RunWork {
    /// Sessions (spans) with a breakdown.
    pub sessions: usize,
    pub total_secs: i64,
    /// Seconds per category; categories without time are not listed.
    pub secs: BTreeMap<String, i64>,
    /// The heavy commands per kind.
    pub commands: BTreeMap<String, CommandCount>,
    /// Commands that ran a check `integrate` runs again (the task's
    /// llvm-cov gate, whole `cargo test` or e2e test).
    pub verification_repeats: i64,
    /// Whole `cargo test` runs in a run that also ran llvm-cov: the tests
    /// run twice over.
    pub test_with_llvm_cov: i64,
}

/// The work breakdown of a run from the `work` of its spans; `None` when
/// none recorded one.
pub fn per_run<'a>(works: impl IntoIterator<Item = &'a Value>) -> Option<RunWork> {
    let mut run = RunWork::default();
    let (mut full_tests, mut llvm_cov) = (0, 0);
    for work in works {
        run.sessions += 1;
        run.total_secs += work["total_secs"].as_i64().unwrap_or(0);
        for (category, secs) in work["secs"].as_object().into_iter().flatten() {
            *run.secs.entry(category.clone()).or_default() += secs.as_i64().unwrap_or(0);
        }
        for (category, count) in work["commands"].as_object().into_iter().flatten() {
            let entry = run.commands.entry(category.clone()).or_default();
            entry.runs += count["runs"].as_i64().unwrap_or(0);
            entry.failed += count["failed"].as_i64().unwrap_or(0);
        }
        run.verification_repeats += work["verification_repeats"].as_i64().unwrap_or(0);
        full_tests += work["full_tests"].as_i64().unwrap_or(0);
        llvm_cov += work["llvm_cov_runs"].as_i64().unwrap_or(0);
    }
    run.test_with_llvm_cov = if llvm_cov > 0 { full_tests } else { 0 };
    (run.sessions > 0).then_some(run)
}

/// One category over a set of runs: its seconds in total, the median of
/// the runs' (0 for a run without it), and its share of their time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CategoryShare {
    pub total: i64,
    pub median: Option<i64>,
    pub share: Option<Ratio>,
}

/// The work breakdown of a set of runs; only runs with one are counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WorkShares {
    pub runs: usize,
    pub total_secs: i64,
    pub categories: BTreeMap<String, CategoryShare>,
    pub commands: BTreeMap<String, CommandCount>,
    pub verification_repeats: i64,
    /// Runs with at least one verification repeat.
    pub runs_with_repeats: usize,
    pub test_with_llvm_cov: i64,
}

/// The work breakdown of `runs`.
pub fn shares<'a>(runs: impl IntoIterator<Item = &'a RunWork>) -> WorkShares {
    let runs: Vec<&RunWork> = runs.into_iter().collect();
    let mut shares = WorkShares {
        runs: runs.len(),
        total_secs: runs.iter().map(|r| r.total_secs).sum(),
        verification_repeats: runs.iter().map(|r| r.verification_repeats).sum(),
        runs_with_repeats: runs.iter().filter(|r| r.verification_repeats > 0).count(),
        test_with_llvm_cov: runs.iter().map(|r| r.test_with_llvm_cov).sum(),
        ..WorkShares::default()
    };
    let categories: std::collections::BTreeSet<&String> =
        runs.iter().flat_map(|r| r.secs.keys()).collect();
    for category in categories {
        let mut values: Vec<i64> = runs
            .iter()
            .map(|r| r.secs.get(category).copied().unwrap_or(0))
            .collect();
        let total = values.iter().sum();
        shares.categories.insert(
            category.clone(),
            CategoryShare {
                total,
                median: median(&mut values),
                share: Ratio::of(total, shares.total_secs),
            },
        );
    }
    for run in &runs {
        for (category, count) in &run.commands {
            let entry = shares.commands.entry(category.clone()).or_default();
            entry.runs += count.runs;
            entry.failed += count.failed;
        }
    }
    shares
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_run_sums_its_sessions_and_a_set_of_runs_shares_them() {
        let worker = json!({
            "total_secs": 100,
            "secs": {"model": 40, "test": 60},
            "commands": {"test": {"runs": 2, "failed": 1}},
            "verification_repeats": 1,
            "full_tests": 1,
            "llvm_cov_runs": 0,
        });
        let resume = json!({
            "total_secs": 50,
            "secs": {"model": 10, "llvm_cov": 40},
            "commands": {"llvm_cov": {"runs": 1, "failed": 0}},
            "verification_repeats": 1,
            "full_tests": 0,
            "llvm_cov_runs": 1,
        });
        let run = per_run([&worker, &resume]).unwrap();
        assert_eq!(run.sessions, 2);
        assert_eq!(run.total_secs, 150);
        assert_eq!(run.secs["model"], 50);
        assert_eq!(run.commands["test"], CommandCount { runs: 2, failed: 1 });
        assert_eq!(run.verification_repeats, 2);
        // A whole cargo test in a run that also ran llvm-cov.
        assert_eq!(run.test_with_llvm_cov, 1);
        assert_eq!(per_run([&worker]).unwrap().test_with_llvm_cov, 0);
        assert_eq!(per_run([]), None);

        let other = per_run([&json!({"total_secs": 50, "secs": {"model": 50}})]).unwrap();
        let shares = shares([&run, &other]);
        assert_eq!(shares.runs, 2);
        assert_eq!(shares.total_secs, 200);
        assert_eq!(shares.categories["model"].total, 100);
        assert_eq!(shares.categories["model"].share, Ratio::of(1, 2));
        // The run without a test counts 0 in the median.
        assert_eq!(shares.categories["test"].median, Some(30));
        assert_eq!(shares.commands["llvm_cov"].runs, 1);
        assert_eq!(shares.verification_repeats, 2);
        assert_eq!(shares.runs_with_repeats, 1);
        assert_eq!(shares.test_with_llvm_cov, 1);
        let json = serde_json::to_value(&shares).unwrap();
        assert_eq!(json["categories"]["model"]["share"], json!(0.5));
        assert_eq!(super::shares([]), WorkShares::default());
    }
}
