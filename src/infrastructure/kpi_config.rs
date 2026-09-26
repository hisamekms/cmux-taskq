//! The `[kpi]` tables (ADR-0051 decisions 17 and 19): the settings and
//! targets in the repository's `dagq.toml` (the repository's policy,
//! committed) and in the host's `host.toml` (`<queue dir>/host.toml` and
//! `$XDG_CONFIG_HOME/dagq/host.toml`, never committed; the queue's file
//! wins key by key and target by target). The same subset of TOML as the
//! rest of `dagq.toml`, parsed by hand:
//!
//! ```toml
//! [kpi]
//! min_samples = 5
//! breach_periods = 3
//! breach_weeks = 2
//! max_improvement_proposals = 2   # dagq.toml only
//!
//! [kpi.targets."phase.work"]      # one target per KPI and stratum
//! kind = "runtime"                # every run without it
//! stat = "median"                 # median, p90 or value
//! max = 3600                      # and/or min
//!
//! [kpi.targets."phase.work".docs] # a second stratum of the same KPI
//! kind = "docs"
//! max = 600
//! ```
//!
//! `host.toml`'s other tables (`[push]`, `[report]`) are read elsewhere
//! and skipped here.
use anyhow::{Context, Result, bail, ensure};
use std::{env, fs, path::Path};

use super::run_env::{parse_positive, parse_string, strip_comment};
use crate::domain::{
    TaskKind,
    kpi::{KpiSettings, Stat, Target, UNKNOWN},
};

pub const HOST_FILE_NAME: &str = "host.toml";
const KPI_TABLE: &str = "kpi";
const TARGETS_PREFIX: &str = "kpi.targets.";

/// The `[kpi]` tables of one file as they are read, line by line.
#[derive(Debug, Default)]
pub struct KpiTables {
    settings: KpiSettings,
    /// Any `[kpi]` table was seen.
    seen_any: bool,
    /// The headers seen, to refuse one defined twice.
    headers: Vec<String>,
    /// The keys of the current table seen.
    keys: Vec<String>,
    /// The table the lines are in: `[kpi]` (`None`) or a target.
    target: Option<Target>,
    in_kpi: bool,
}

impl KpiTables {
    /// Start the table `name` (inside `[...]`) if it is one of `[kpi]`'s;
    /// `false` leaves it to the caller.
    pub fn header(&mut self, name: &str) -> Result<bool> {
        let target = if name == KPI_TABLE {
            None
        } else if let Some(rest) = name.strip_prefix(TARGETS_PREFIX) {
            Some(target_name(rest)?)
        } else {
            self.close()?;
            self.in_kpi = false;
            return Ok(false);
        };
        self.close()?;
        ensure!(
            !self.headers.iter().any(|seen| seen == name),
            "[{name}] is defined twice"
        );
        self.headers.push(name.to_owned());
        self.seen_any = true;
        self.in_kpi = true;
        self.keys.clear();
        self.target = target.map(|kpi| Target {
            kpi,
            kind: None,
            stat: None,
            min: None,
            max: None,
        });
        Ok(true)
    }

    /// Whether the lines are in one of `[kpi]`'s tables.
    pub fn active(&self) -> bool {
        self.in_kpi
    }

    /// One `KEY = value` line of the current table; `rest` is what follows `=`.
    pub fn entry(&mut self, key: &str, rest: &str) -> Result<()> {
        ensure!(
            !self.keys.iter().any(|seen| seen == key),
            "{key} is defined twice"
        );
        self.keys.push(key.to_owned());
        let count = || -> Result<usize> {
            usize::try_from(parse_positive(rest, "number")?).context("too large")
        };
        match &mut self.target {
            None => {
                let settings = &mut self.settings;
                let slot = match key {
                    "min_samples" => &mut settings.min_samples,
                    "breach_periods" => &mut settings.breach_periods,
                    "breach_weeks" => &mut settings.breach_weeks,
                    "max_improvement_proposals" => &mut settings.max_improvement_proposals,
                    _ => bail!(
                        "unknown key {key} in [kpi]; the keys are min_samples, breach_periods, breach_weeks, max_improvement_proposals"
                    ),
                };
                *slot = Some(count().with_context(|| format!("value of {key}"))?);
            }
            Some(target) => match key {
                "kind" => {
                    let kind = parse_string(rest).context("value of kind")?;
                    ensure!(
                        kind == UNKNOWN || kind.parse::<TaskKind>().is_ok(),
                        "kind {kind:?} is not docs, plugin, runtime, ci or unknown"
                    );
                    target.kind = Some(kind);
                }
                "stat" => {
                    let stat = parse_string(rest).context("value of stat")?;
                    target.stat = Some(stat.parse::<Stat>().map_err(anyhow::Error::msg)?);
                }
                "min" => target.min = Some(number(rest).context("value of min")?),
                "max" => target.max = Some(number(rest).context("value of max")?),
                _ => bail!("unknown key {key} in a target; the keys are kind, stat, min, max"),
            },
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if let Some(target) = self.target.take() {
            ensure!(
                target.min.is_some() || target.max.is_some(),
                "the target of {} has neither min nor max",
                target.kpi
            );
            ensure!(
                !self
                    .settings
                    .targets
                    .iter()
                    .any(|kept| { (&kept.kpi, kept.stratum()) == (&target.kpi, target.stratum()) }),
                "{} has two targets for {}",
                target.kpi,
                target.stratum()
            );
            self.settings.targets.push(target);
        }
        Ok(())
    }

    /// What the tables set; `None` when the file had none.
    pub fn finish(mut self) -> Result<Option<KpiSettings>> {
        self.close()?;
        Ok(self.seen_any.then_some(self.settings))
    }
}

/// The KPI a target table names: `"phase.work"` (optionally followed by
/// `.<label>` for another stratum) or a bare `first_pass_rate`.
fn target_name(rest: &str) -> Result<String> {
    let name = match rest.strip_prefix('"') {
        Some(quoted) => {
            let (name, after) = quoted
                .split_once('"')
                .context("unterminated KPI name in a target header")?;
            ensure!(
                after.is_empty()
                    || after
                        .strip_prefix('.')
                        .is_some_and(|label| !label.is_empty() && !label.contains('.')),
                "a target header is [kpi.targets.\"<kpi>\"] or [kpi.targets.\"<kpi>\".<label>]"
            );
            name
        }
        None => {
            ensure!(
                !rest.contains(['.', '"']),
                "quote a KPI name with a dot: [kpi.targets.\"{rest}\"]"
            );
            rest
        }
    };
    ensure!(!name.trim().is_empty(), "a target needs a KPI name");
    Ok(name.to_owned())
}

/// A number, whole or with a fraction, followed by nothing but a comment.
fn number(text: &str) -> Result<f64> {
    let digits = strip_comment(text).replace('_', "");
    ensure!(!digits.is_empty(), "missing value");
    let value: f64 = digits
        .parse()
        .with_context(|| format!("expected a number, not {digits}"))?;
    ensure!(value.is_finite(), "expected a finite number");
    Ok(value)
}

/// The `[kpi]` tables of a `host.toml`'s text, `label` naming the file in
/// errors; the other tables are skipped.
pub fn parse_host_kpi(text: &str, label: &str) -> Result<Option<KpiSettings>> {
    let mut tables = KpiTables::default();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let name = strip_comment(header)
                .strip_suffix(']')
                .with_context(|| format!("{label}:{number}: unclosed table header"))?
                .trim();
            tables
                .header(name)
                .with_context(|| format!("{label}:{number}"))?;
            continue;
        }
        if !tables.active() {
            continue;
        }
        let (key, rest) = line
            .split_once('=')
            .with_context(|| format!("{label}:{number}: expected KEY = value"))?;
        tables
            .entry(key.trim(), rest.trim())
            .with_context(|| format!("{label}:{number}"))?;
    }
    tables.finish().with_context(|| label.to_owned())
}

fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// The host-wide `host.toml`: `$XDG_CONFIG_HOME/dagq/host.toml`, or
/// `~/.config/dagq/host.toml`.
pub fn host_wide_file() -> Option<std::path::PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
        .map(|dir| dir.join("dagq").join(HOST_FILE_NAME))
}

/// The host's `[kpi]` tables: the host-wide file's with the queue's
/// (`<queue_dir>/host.toml`) over them; `None` when neither has one.
pub fn load_host_kpi(queue_dir: &Path, host_wide: Option<&Path>) -> Result<Option<KpiSettings>> {
    let mut merged: Option<KpiSettings> = None;
    for path in host_wide
        .into_iter()
        .chain([queue_dir.join(HOST_FILE_NAME).as_path()])
    {
        let Some(text) = read(path)? else { continue };
        if let Some(settings) = parse_host_kpi(&text, &path.display().to_string())? {
            merged = Some(match merged {
                Some(base) => base.overlay(settings),
                None => settings,
            });
        }
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_settings_and_targets_and_skips_other_tables() {
        let text = "\u{feff}# host\n[push]\ncommand = [\"x\"]\n\n[kpi]\nmin_samples = 3 # few\nbreach_weeks = 1\n\n[kpi.targets.\"phase.work\"]\nkind = \"runtime\"\nstat = \"p90\"\nmax = 3_600\n\n[kpi.targets.\"phase.work\".docs]\nkind = \"docs\"\nmax = 60\n\n[kpi.targets.first_pass_rate]\nmin = 0.6\n[report]\nkeep_daily_days = 90\n";
        let settings = parse_host_kpi(text, "host.toml").unwrap().unwrap();
        assert_eq!(settings.min_samples, Some(3));
        assert_eq!(settings.breach_weeks, Some(1));
        assert_eq!(settings.breach_periods, None);
        assert_eq!(settings.targets.len(), 3);
        assert_eq!(settings.targets[0].kpi, "phase.work");
        assert_eq!(settings.targets[0].stratum(), "kind=runtime");
        assert_eq!(settings.targets[0].stat, Some(Stat::P90));
        assert_eq!(settings.targets[0].max, Some(3600.0));
        assert_eq!(settings.targets[1].stratum(), "kind=docs");
        assert_eq!(settings.targets[2].kpi, "first_pass_rate");
        assert_eq!(settings.targets[2].min, Some(0.6));
        assert_eq!(parse_host_kpi("[push]\na = 1\n", "h").unwrap(), None);
    }

    #[test]
    fn refuses_what_the_tables_do_not_support() {
        let error = |text: &str| format!("{:#}", parse_host_kpi(text, "h").unwrap_err());
        assert!(error("[kpi]\nfoo = 1\n").contains("unknown key foo"));
        assert!(error("[kpi]\nmin_samples = 0\n").contains("positive"));
        assert!(error("[kpi]\nmin_samples = 1\nmin_samples = 2\n").contains("twice"));
        assert!(error("[kpi]\n[kpi]\n").contains("defined twice"));
        assert!(error("[kpi.targets.x]\nkind = \"docs\"\n").contains("neither min nor max"));
        assert!(error("[kpi.targets.x]\nmax = 1\nkind = \"web\"\n").contains("is not docs"));
        assert!(error("[kpi.targets.x]\nmax = 1\nstat = \"mean\"\n").contains("not a stat"));
        assert!(error("[kpi.targets.x]\nmax = one\n").contains("expected a number"));
        assert!(error("[kpi.targets.x]\nmax = 1\nwho = 1\n").contains("unknown key who"));
        assert!(error("[kpi.targets.phase.work]\nmax = 1\n").contains("quote"));
        assert!(error("[kpi.targets.\"x]\nmax = 1\n").contains("unterminated"));
        assert!(error("[kpi.targets.\"x\".a.b]\nmax = 1\n").contains("target header"));
        assert!(error("[kpi.targets.\"\"]\nmax = 1\n").contains("needs a KPI name"));
        assert!(
            error("[kpi.targets.x]\nmax = 1\n[kpi.targets.\"x\".again]\nmin = 0\n")
                .contains("two targets")
        );
        assert!(error("[kpi\n").contains("unclosed"));
        assert!(error("[kpi]\nmin_samples\n").contains("expected KEY = value"));
    }

    #[test]
    fn the_queue_file_wins_over_the_host_wide_one() {
        let dir = tempfile::tempdir().unwrap();
        let wide = dir.path().join("wide.toml");
        fs::write(
            &wide,
            "[kpi]\nmin_samples = 9\nbreach_periods = 4\n[kpi.targets.landings]\nmin = 1\n[kpi.targets.revise_rate]\nmax = 0.5\n",
        )
        .unwrap();
        assert_eq!(load_host_kpi(dir.path(), None).unwrap(), None);
        fs::write(
            dir.path().join(HOST_FILE_NAME),
            "[kpi]\nmin_samples = 2\n[kpi.targets.landings]\nmin = 3\n",
        )
        .unwrap();
        let settings = load_host_kpi(dir.path(), Some(&wide)).unwrap().unwrap();
        assert_eq!(settings.min_samples, Some(2));
        assert_eq!(settings.breach_periods, Some(4));
        let landings: Vec<_> = settings
            .targets
            .iter()
            .filter(|target| target.kpi == "landings")
            .collect();
        assert_eq!(landings.len(), 1);
        assert_eq!(landings[0].min, Some(3.0));
        assert_eq!(settings.targets.len(), 2);
        assert!(host_wide_file().is_some_and(|path| path.ends_with("dagq/host.toml")));
    }
}
