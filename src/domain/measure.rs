//! What a run is measured under (goal 21, task 197): the binaries and the
//! toolchain it was claimed with, the supervisor's parallel and slots and
//! the load at the claim ([`ClaimAttributes`], on `run_claimed`), and the
//! load average over each interval of the run ([`LoadWindow`], on the event
//! that ends the interval). Versions, counts and loads are domain values;
//! no path goes in (ADR-0032's classification of the records).

use serde::Serialize;

/// The versions of the host a run is claimed on: Claude Code's (from the
/// versioned file `--claude` resolves to; null when that path names no
/// version) and the host's `rustc` (`release` and `host` of `rustc -vV`,
/// run in the main checkout so its toolchain file applies; null when it
/// cannot be run).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HostVersions {
    pub claude_version: Option<String>,
    pub rustc_release: Option<String>,
    pub rustc_host: Option<String>,
}

impl HostVersions {
    /// `release` and `host` of the output of `rustc -vV`.
    pub fn with_rustc_verbose(mut self, output: &str) -> Self {
        let field = |name: &str| {
            output.lines().find_map(|line| {
                line.strip_prefix(name)
                    .and_then(|rest| rest.strip_prefix(':'))
                    .map(|value| value.trim().to_owned())
                    .filter(|value| !value.is_empty())
            })
        };
        self.rustc_release = field("release");
        self.rustc_host = field("host");
        self
    }
}

/// The attributes `run_claimed` carries for a run the supervisor claimed:
/// the build identifier of the claiming binary (ADR-0045 decision 2), the
/// host's versions, the supervisor's `parallel`, the slots it held before
/// this claim and the 1-minute load average (null when unavailable).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClaimAttributes {
    pub dagq_version: String,
    #[serde(flatten)]
    pub host: HostVersions,
    pub parallel: usize,
    pub slots: usize,
    pub load_avg: Option<f64>,
}

/// The load average over an interval: its mean and maximum, null when no
/// sample was taken (the load could not be read, or the interval began
/// before this supervisor watched it and ended at once).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, serde::Deserialize)]
pub struct LoadSummary {
    pub load_avg_mean: Option<f64>,
    pub load_avg_max: Option<f64>,
}

/// The samples of the 1-minute load average taken over one interval.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LoadWindow {
    sum: f64,
    count: u32,
    max: Option<f64>,
}

impl LoadWindow {
    /// Add one sample; `None` (the load could not be read) is skipped.
    pub fn add(&mut self, sample: Option<f64>) {
        let Some(load) = sample.filter(|load| load.is_finite()) else {
            return;
        };
        self.sum += load;
        self.count += 1;
        self.max = Some(self.max.map_or(load, |max| max.max(load)));
    }

    /// The mean and maximum of the samples so far, to two decimals.
    pub fn summary(&self) -> LoadSummary {
        LoadSummary {
            load_avg_mean: (self.count > 0).then(|| round2(self.sum / f64::from(self.count))),
            load_avg_max: self.max.map(round2),
        }
    }

    /// [`Self::summary`], and start the next interval empty.
    pub fn take(&mut self) -> LoadSummary {
        let summary = self.summary();
        *self = Self::default();
        summary
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// The lower bounds of the load bands `stats` groups by; a band runs to
/// the next bound, and the last one has none.
pub const LOAD_BANDS: [f64; 6] = [0.0, 4.0, 8.0, 16.0, 32.0, 64.0];

/// The band of a load average: `0-4`, `4-8`, `8-16`, `16-32`, `32-64` or
/// `64+`. A negative load falls in the first.
pub fn load_band(load: f64) -> &'static str {
    const NAMES: [&str; 6] = ["0-4", "4-8", "8-16", "16-32", "32-64", "64+"];
    let index = LOAD_BANDS
        .iter()
        .rposition(|&bound| load >= bound)
        .unwrap_or(0);
    NAMES[index]
}

/// The position of a band [`load_band`] names, for ordering; bands it does
/// not name sort last.
pub fn load_band_order(band: &str) -> usize {
    LOAD_BANDS
        .iter()
        .position(|&bound| load_band(bound) == band)
        .unwrap_or(LOAD_BANDS.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_window_reports_mean_and_max_and_starts_again_when_taken() {
        let mut window = LoadWindow::default();
        assert_eq!(window.summary(), LoadSummary::default());
        window.add(Some(2.0));
        window.add(None);
        window.add(Some(f64::NAN));
        window.add(Some(5.0));
        window.add(Some(3.333));
        assert_eq!(
            window.take(),
            LoadSummary {
                load_avg_mean: Some(3.44),
                load_avg_max: Some(5.0)
            }
        );
        assert_eq!(window.summary(), LoadSummary::default());
        assert_eq!(
            serde_json::to_value(window.summary()).unwrap(),
            json!({"load_avg_mean": null, "load_avg_max": null})
        );
    }

    #[test]
    fn loads_fall_in_bands_ordered_by_their_bound() {
        assert_eq!(load_band(-1.0), "0-4");
        assert_eq!(load_band(0.0), "0-4");
        assert_eq!(load_band(3.99), "0-4");
        assert_eq!(load_band(4.0), "4-8");
        assert_eq!(load_band(12.0), "8-16");
        assert_eq!(load_band(16.0), "16-32");
        assert_eq!(load_band(60.0), "32-64");
        assert_eq!(load_band(151.0), "64+");
        assert!(load_band_order("0-4") < load_band_order("4-8"));
        assert!(load_band_order("32-64") < load_band_order("64+"));
        assert_eq!(load_band_order("other"), LOAD_BANDS.len());
    }

    #[test]
    fn rustc_verbose_gives_release_and_host() {
        let output = "rustc 1.90.0 (1159e78c4 2025-09-14)\nbinary: rustc\ncommit-hash: 1159e78c4\nhost: aarch64-apple-darwin\nrelease: 1.90.0\nLLVM version: 20.1.8\n";
        let versions = HostVersions {
            claude_version: Some("2.1.0".to_owned()),
            ..HostVersions::default()
        }
        .with_rustc_verbose(output);
        assert_eq!(versions.claude_version.as_deref(), Some("2.1.0"));
        assert_eq!(versions.rustc_release.as_deref(), Some("1.90.0"));
        assert_eq!(versions.rustc_host.as_deref(), Some("aarch64-apple-darwin"));
        let none = HostVersions::default().with_rustc_verbose("error: no toolchain\nhost:\n");
        assert_eq!(none, HostVersions::default());
    }

    #[test]
    fn claim_attributes_flatten_the_host_versions() {
        let attributes = ClaimAttributes {
            dagq_version: "0.5.0-dev+abc".to_owned(),
            host: HostVersions {
                claude_version: None,
                rustc_release: Some("1.90.0".to_owned()),
                rustc_host: Some("aarch64-apple-darwin".to_owned()),
            },
            parallel: 3,
            slots: 1,
            load_avg: Some(7.5),
        };
        assert_eq!(
            serde_json::to_value(attributes).unwrap(),
            json!({
                "dagq_version": "0.5.0-dev+abc",
                "claude_version": null,
                "rustc_release": "1.90.0",
                "rustc_host": "aarch64-apple-darwin",
                "parallel": 3,
                "slots": 1,
                "load_avg": 7.5,
            })
        );
    }
}
