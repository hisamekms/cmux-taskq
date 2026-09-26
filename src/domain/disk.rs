//! The free disk space a run needs (ADR-0047 decision 44, task 377): the
//! supervisor claims no new run, and starts no landing's verification,
//! while the free space of the queue's directory (where the run worktrees
//! are) is below what one needs. What one needs follows from what the
//! recent runs built: the largest `bytes` of the latest `build_outputs_removed`
//! events (the `target/` an ended run left, task 376), times a factor per
//! step, never below `min_free_bytes`. The `[disk]` table of `dagq.toml`
//! sets them.

use serde::Serialize;

/// The run event whose `bytes` measure what a run built.
pub const BUILD_OUTPUTS_REMOVED: &str = "build_outputs_removed";
/// The `subject` of the `cost` ask about the disk (ADR-0047 decision 42).
pub const DISK_SUBJECT: &str = "disk";
/// The options of the disk ask (ADR-0047 decision 44): `done` once a
/// person freed the disk, `wait` to leave the queue waiting.
pub const DISK_OPTIONS: &[&str] = &["done", "wait"];
/// The `repair` of the `auto_repaired` a cleanup for the disk records.
pub const DISK_CLEANUP: &str = "disk_cleanup";

/// Default number of the latest `build_outputs_removed` read.
pub const DEFAULT_SAMPLE_RUNS: i64 = 20;
/// Default factor of the largest build a claim needs free.
pub const DEFAULT_CLAIM_FACTOR: f64 = 2.0;
/// Default factor of the largest build a landing's verification needs free.
pub const DEFAULT_INTEGRATE_FACTOR: f64 = 1.5;

/// The `[disk]` table of `dagq.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DiskConfig {
    /// How many of the latest `build_outputs_removed` the largest is taken of.
    pub sample_runs: i64,
    /// A claim needs the largest build times this free.
    pub claim_factor: f64,
    /// A landing's verification needs the largest build times this free.
    pub integrate_factor: f64,
    /// The least free bytes either needs, also before any build was
    /// measured; `None` checks nothing until one was.
    pub min_free_bytes: Option<u64>,
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            sample_runs: DEFAULT_SAMPLE_RUNS,
            claim_factor: DEFAULT_CLAIM_FACTOR,
            integrate_factor: DEFAULT_INTEGRATE_FACTOR,
            min_free_bytes: None,
        }
    }
}

impl DiskConfig {
    /// The setting names of the `[disk]` table.
    pub const KEYS: [&str; 4] = [
        "sample_runs",
        "claim_factor",
        "integrate_factor",
        "min_free_bytes",
    ];
    /// The settings that are factors (positive numbers, not whole ones).
    pub const FACTORS: [&str; 2] = ["claim_factor", "integrate_factor"];

    /// The whole-number setting `key` set to `value`; `None` for a key
    /// that is not one.
    pub fn set_whole(&mut self, key: &str, value: i64) -> Option<()> {
        match key {
            "sample_runs" => self.sample_runs = value,
            "min_free_bytes" => self.min_free_bytes = Some(u64::try_from(value).ok()?),
            _ => return None,
        }
        Some(())
    }

    /// The factor `key` set to `value`; `None` for a key that is not one.
    pub fn set_factor(&mut self, key: &str, value: f64) -> Option<()> {
        match key {
            "claim_factor" => self.claim_factor = value,
            "integrate_factor" => self.integrate_factor = value,
            _ => return None,
        }
        Some(())
    }

    /// What a claim and a landing need free, given the `bytes` of the
    /// latest `build_outputs_removed` (at most `sample_runs` of them).
    pub fn needs(&self, builds: &[u64]) -> DiskNeeds {
        let largest = builds.iter().copied().max();
        let need = |factor: f64| {
            let scaled = largest.map(|bytes| (bytes as f64 * factor).ceil() as u64);
            match (scaled, self.min_free_bytes) {
                (Some(scaled), Some(least)) => Some(scaled.max(least)),
                (scaled, least) => scaled.or(least),
            }
        };
        DiskNeeds {
            largest_build: largest,
            claim: need(self.claim_factor),
            landing: need(self.integrate_factor),
        }
    }
}

/// The free bytes a claim and a landing need; `None` checks nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DiskNeeds {
    /// The largest build of the recent runs they follow from.
    pub largest_build: Option<u64>,
    pub claim: Option<u64>,
    pub landing: Option<u64>,
}

/// `bytes` in GiB with one decimal, for messages.
pub fn gib(bytes: f64) -> String {
    format!("{:.1} GiB", bytes / f64::from(1u32 << 30))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_needs_follow_the_largest_recent_build() {
        let config = DiskConfig::default();
        assert_eq!(config.needs(&[]), DiskNeeds::default());
        let needs = config.needs(&[100, 400, 250]);
        assert_eq!(needs.largest_build, Some(400));
        assert_eq!(needs.claim, Some(800));
        assert_eq!(needs.landing, Some(600));
        // The least free bytes apply before any build and as a floor.
        let floor = DiskConfig {
            min_free_bytes: Some(700),
            ..config
        };
        assert_eq!(floor.needs(&[]).claim, Some(700));
        assert_eq!(floor.needs(&[]).landing, Some(700));
        assert_eq!(floor.needs(&[400]).claim, Some(800));
        assert_eq!(floor.needs(&[400]).landing, Some(700));
        let odd = DiskConfig {
            integrate_factor: 1.5,
            ..config
        };
        assert_eq!(odd.needs(&[3]).landing, Some(5));
    }

    #[test]
    fn the_settings_are_set_by_name() {
        let mut config = DiskConfig::default();
        assert_eq!(config.set_whole("sample_runs", 5), Some(()));
        assert_eq!(config.set_whole("min_free_bytes", 9), Some(()));
        assert_eq!(config.set_whole("claim_factor", 9), None);
        assert_eq!(config.set_factor("claim_factor", 3.0), Some(()));
        assert_eq!(config.set_factor("integrate_factor", 2.5), Some(()));
        assert_eq!(config.set_factor("sample_runs", 2.5), None);
        assert_eq!(
            config,
            DiskConfig {
                sample_runs: 5,
                claim_factor: 3.0,
                integrate_factor: 2.5,
                min_free_bytes: Some(9),
            }
        );
        assert_eq!(gib(1.5 * f64::from(1u32 << 30)), "1.5 GiB");
    }
}
