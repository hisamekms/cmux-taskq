//! The repository's `dagq.toml` (ADR-0023 decision 3): `[run.env]` holds
//! environment variables every run gets, in its worker workspace and in the
//! verification commands; its values are strings in which
//! `${DAGQ_QUEUE_DIR}` and `${DAGQ_RUN_DIR}` are expanded. `[stall]` holds
//! the thresholds of the stalled-session checks in seconds (ADR-0043
//! decision 4). `[conflicts]` holds the thresholds of the
//! `conflict_hotspot` alert of `stats` (goal 31). The file is parsed by
//! hand: the format is these tables of `KEY = value` lines, a subset of
//! TOML that needs no parser crate.
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    application::{Exit, Verifier},
    domain::{stall::StallConfig, stats::ConflictConfig},
};

pub const CONFIG_FILE_NAME: &str = "dagq.toml";
pub const QUEUE_DIR_VAR: &str = "DAGQ_QUEUE_DIR";
pub const RUN_DIR_VAR: &str = "DAGQ_RUN_DIR";
const RUN_ENV_TABLE: &str = "run.env";
const STALL_TABLE: &str = "stall";
const CONFLICTS_TABLE: &str = "conflicts";
const TABLES: [&str; 3] = [RUN_ENV_TABLE, STALL_TABLE, CONFLICTS_TABLE];
/// Names the runtime itself sets on a workspace (`DAGQ_ROLE`, `DAGQ_QUEUE`)
/// and may set later; `[run.env]` cannot override them.
const RESERVED_PREFIX: &str = "DAGQ_";

/// `[run.env]` as written, in file order, values unexpanded.
pub fn parse_run_env(text: &str) -> Result<Vec<(String, String)>> {
    Ok(parse_config(text)?.run_env)
}

/// What the file holds: `[run.env]`, `[stall]` (ADR-0043 decision 4) and
/// `[conflicts]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// `[run.env]` as written, in file order, values unexpanded.
    pub run_env: Vec<(String, String)>,
    /// `[stall]`, the defaults for the keys it does not set.
    pub stall: StallConfig,
    /// `[conflicts]`, the defaults for the keys it does not set.
    pub conflicts: ConflictConfig,
}

/// Parse the whole file.
pub fn parse_config(text: &str) -> Result<Config> {
    let mut table: Option<&str> = None;
    let mut seen: Vec<&str> = Vec::new();
    let mut config = Config::default();
    let mut stall_keys: Vec<String> = Vec::new();
    let mut conflict_keys: Vec<String> = Vec::new();
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
                .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: unclosed table header"))?
                .trim();
            let known = TABLES.iter().find(|table| **table == name).with_context(|| {
                format!(
                    "{CONFIG_FILE_NAME}:{number}: unknown table [{name}]; only [{RUN_ENV_TABLE}], [{STALL_TABLE}] and [{CONFLICTS_TABLE}] are supported"
                )
            })?;
            ensure!(
                !seen.contains(known),
                "{CONFIG_FILE_NAME}:{number}: [{name}] is defined twice"
            );
            seen.push(known);
            table = Some(known);
            continue;
        }
        let (key, rest) = line
            .split_once('=')
            .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: expected KEY = value"))?;
        let key = key.trim();
        match table {
            Some(RUN_ENV_TABLE) => {
                ensure!(
                    is_env_name(key),
                    "{CONFIG_FILE_NAME}:{number}: {key:?} is not an environment variable name"
                );
                ensure!(
                    !key.starts_with(RESERVED_PREFIX),
                    "{CONFIG_FILE_NAME}:{number}: {key} uses the reserved prefix {RESERVED_PREFIX}"
                );
                ensure!(
                    config.run_env.iter().all(|(existing, _)| existing != key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let value = parse_string(rest.trim())
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                config.run_env.push((key.to_owned(), value));
            }
            Some(CONFLICTS_TABLE) => {
                ensure!(
                    ConflictConfig::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{CONFLICTS_TABLE}]; the keys are {}",
                    ConflictConfig::KEYS.join(", ")
                );
                ensure!(
                    !conflict_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let value = parse_positive(rest.trim(), "number")
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                config.conflicts.set(key, value);
                conflict_keys.push(key.to_owned());
            }
            Some(_) => {
                ensure!(
                    StallConfig::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{STALL_TABLE}]; the keys are {}",
                    StallConfig::KEYS.join(", ")
                );
                ensure!(
                    !stall_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let secs = parse_positive(rest.trim(), "number of seconds")
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                config.stall.set(key, secs);
                stall_keys.push(key.to_owned());
            }
            None => bail!(
                "{CONFIG_FILE_NAME}:{number}: a key outside [{RUN_ENV_TABLE}], [{STALL_TABLE}] or [{CONFLICTS_TABLE}]"
            ),
        }
    }
    Ok(config)
}

/// A positive integer (a `what`), followed by nothing but an optional comment.
fn parse_positive(text: &str, what: &str) -> Result<i64> {
    let digits = strip_comment(text);
    ensure!(!digits.is_empty(), "missing value");
    let value: i64 = digits
        .replace('_', "")
        .parse()
        .with_context(|| format!("expected a whole {what}, not {digits}"))?;
    ensure!(value > 0, "must be a positive {what}, not {value}");
    Ok(value)
}

/// `[stall]` of the `dagq.toml` in `root` (ADR-0043 decision 4), `None`
/// when there is no file; no table or no key is the default.
pub fn load_stall_config(root: &Path) -> Result<Option<StallConfig>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(Some(
        parse_config(&text)
            .with_context(|| format!("parse {}", path.display()))?
            .stall,
    ))
}

/// `[conflicts]` of the `dagq.toml` in `root`, `None` when there is no
/// file; no table or no key is the default.
pub fn load_conflict_config(root: &Path) -> Result<Option<ConflictConfig>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(Some(
        parse_config(&text)
            .with_context(|| format!("parse {}", path.display()))?
            .conflicts,
    ))
}

fn read_config(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// `value` with `${DAGQ_QUEUE_DIR}` and `${DAGQ_RUN_DIR}` replaced. Any other
/// `$` text stays as written: the value is not a shell word.
pub fn expand(value: &str, queue_dir: &str, run_dir: &str) -> String {
    value
        .replace(&format!("${{{QUEUE_DIR_VAR}}}"), queue_dir)
        .replace(&format!("${{{RUN_DIR_VAR}}}"), run_dir)
}

/// The expanded `[run.env]` of the `dagq.toml` in `root`; no file is an
/// empty table.
pub fn load_run_env(
    root: &Path,
    queue_dir: &Path,
    run_dir: &Path,
) -> Result<Vec<(String, String)>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(Vec::new());
    };
    let queue_dir = path_str(queue_dir)?;
    let run_dir = path_str(run_dir)?;
    Ok(parse_run_env(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .into_iter()
        .map(|(key, value)| {
            let value = expand(&value, queue_dir, run_dir);
            (key, value)
        })
        .collect())
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("{} is not UTF-8", path.display()))
}

fn is_env_name(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// The text before a `#` comment, for lines that hold no string.
fn strip_comment(text: &str) -> &str {
    text.split_once('#')
        .map_or(text, |(before, _)| before)
        .trim()
}

/// A TOML literal (`'...'`) or basic (`"..."`) string, followed by nothing
/// but an optional comment.
fn parse_string(text: &str) -> Result<String> {
    let mut chars = text.chars();
    let quote = chars.next().context("missing value")?;
    let mut value = String::new();
    match quote {
        '\'' => loop {
            match chars.next() {
                Some('\'') => break,
                Some(c) => value.push(c),
                None => bail!("unterminated string"),
            }
        },
        '"' => loop {
            match chars.next() {
                Some('"') => break,
                Some('\\') => value.push(match chars.next() {
                    Some('\\') => '\\',
                    Some('"') => '"',
                    Some('n') => '\n',
                    Some('t') => '\t',
                    other => bail!("unsupported escape \\{}", other.unwrap_or(' ')),
                }),
                Some(c) => value.push(c),
                None => bail!("unterminated string"),
            }
        },
        _ => bail!("expected a quoted string"),
    }
    let rest = chars.as_str().trim();
    ensure!(
        rest.is_empty() || rest.starts_with('#'),
        "unexpected text after the string: {rest}"
    );
    Ok(value)
}

/// The verification port for `integrate`: `[run.env]` from the `dagq.toml`
/// of `checkout` (the main checkout, since `integrate` may be called from
/// any worktree of the repository) with the directory of the queue `db`,
/// and each command in `/bin/sh` with its output in the log.
pub struct ShellVerifier {
    pub checkout: PathBuf,
    pub db: PathBuf,
}

impl Verifier for ShellVerifier {
    fn run_env(&self, run_dir: &Path) -> Result<Vec<(String, String)>> {
        let queue_dir = self
            .db
            .parent()
            .context("queue database has no directory")?;
        load_run_env(&self.checkout, queue_dir, run_dir)
    }

    fn run_to_log(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<Exit> {
        super::adapters::run_shell_to_log(command, cwd, env, log).map(super::process::exit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_the_run_env_table_in_file_order() {
        let text = r#"
# build cache shared by every run
[run.env]  # the only table
CARGO_TARGET_DIR = '${DAGQ_QUEUE_DIR}/target'
RUST_LOG="info" # trailing comment
QUOTED = "a \"b\" \\ c\td\n"
LITERAL = 'no \n escapes # here'
"#;
        assert_eq!(
            parse_run_env(text).unwrap(),
            pairs(&[
                ("CARGO_TARGET_DIR", "${DAGQ_QUEUE_DIR}/target"),
                ("RUST_LOG", "info"),
                ("QUOTED", "a \"b\" \\ c\td\n"),
                ("LITERAL", "no \\n escapes # here"),
            ])
        );
    }

    #[test]
    fn empty_text_and_empty_table_are_no_env() {
        assert!(parse_run_env("").unwrap().is_empty());
        assert!(parse_run_env("# nothing\n[run.env]\n").unwrap().is_empty());
        assert_eq!(
            parse_run_env("\u{feff}[run.env]\nA = 'x'").unwrap(),
            pairs(&[("A", "x")])
        );
    }

    #[test]
    fn rejects_what_the_subset_does_not_support() {
        for (text, message) in [
            ("[run.env\nA = 'x'", "unclosed table header"),
            ("[build]\nA = 'x'", "unknown table [build]"),
            (
                "[stall]\nother_secs = 1",
                "unknown key other_secs in [stall]",
            ),
            (
                "[stall]\nsend_confirm_secs = 0",
                "positive number of seconds",
            ),
            (
                "[stall]\nsend_confirm_secs = -5",
                "positive number of seconds",
            ),
            (
                "[stall]\nsend_confirm_secs = '60'",
                "whole number of seconds",
            ),
            ("[stall]\nsend_confirm_secs = ", "missing value"),
            (
                "[stall]\nsend_confirm_secs = 1\nsend_confirm_secs = 2",
                "send_confirm_secs is defined twice",
            ),
            ("[stall]\n[stall]", "[stall] is defined twice"),
            ("[conflicts]\nother = 1", "unknown key other in [conflicts]"),
            (
                "[conflicts]\nhotspot_conflicts = 0",
                "positive number, not 0",
            ),
            (
                "[conflicts]\nhotspot_conflicts = 1\nhotspot_conflicts = 2",
                "hotspot_conflicts is defined twice",
            ),
            ("A = 'x'", "a key outside [run.env]"),
            ("[run.env]\nA 'x'", "expected KEY"),
            ("[run.env]\n1A = 'x'", "not an environment variable name"),
            ("[run.env]\nA-B = 'x'", "not an environment variable name"),
            ("[run.env]\nDAGQ_ROLE = 'x'", "reserved prefix"),
            ("[run.env]\nA = 'x'\nA = 'y'", "A is defined twice"),
            ("[run.env]\n[run.env]", "[run.env] is defined twice"),
            ("[run.env]\nA = x", "expected a quoted string"),
            ("[run.env]\nA = ", "missing value"),
            ("[run.env]\nA = 'x", "unterminated string"),
            ("[run.env]\nA = \"x", "unterminated string"),
            ("[run.env]\nA = \"\\q\"", "unsupported escape \\q"),
            ("[run.env]\nA = 'x' y", "unexpected text after the string"),
        ] {
            let error = format!("{:#}", parse_run_env(text).unwrap_err());
            assert!(error.contains(message), "{text:?}: {error}");
        }
    }

    #[test]
    fn parses_the_stall_table_next_to_the_run_env() {
        let text = "[stall] # thresholds\nidle_without_receipt_secs = 600 # ten minutes\nbackground_alert_secs = 3_600\n[run.env]\nA = 'x'\n";
        let config = parse_config(text).unwrap();
        assert_eq!(config.run_env, pairs(&[("A", "x")]));
        assert_eq!(
            config.stall,
            StallConfig {
                idle_without_receipt_secs: 600,
                send_confirm_secs: crate::domain::stall::DEFAULT_SEND_CONFIRM_SECS,
                background_alert_secs: 3600,
            }
        );
        assert_eq!(parse_config("").unwrap().stall, StallConfig::default());
    }

    #[test]
    fn loads_the_stall_table_of_the_file_in_the_root() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_stall_config(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[stall]\nsend_confirm_secs = 30\n",
        )
        .unwrap();
        assert_eq!(
            load_stall_config(dir.path())
                .unwrap()
                .unwrap()
                .send_confirm_secs,
            30
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[stall]\nx = 1\n").unwrap();
        let error = format!("{:#}", load_stall_config(dir.path()).unwrap_err());
        assert!(
            error.contains("parse ") && error.contains("unknown key"),
            "{error}"
        );
    }

    #[test]
    fn loads_the_conflicts_table_of_the_file_in_the_root() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_conflict_config(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[conflicts]\nhotspot_ratio_percent = 50 # half\n[stall]\nsend_confirm_secs = 30\n",
        )
        .unwrap();
        assert_eq!(
            load_conflict_config(dir.path()).unwrap().unwrap(),
            ConflictConfig {
                hotspot_ratio_percent: 50,
                ..ConflictConfig::default()
            }
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[conflicts]\nx = 1\n").unwrap();
        assert!(load_conflict_config(dir.path()).is_err());
    }

    #[test]
    fn errors_name_the_line() {
        let error = format!("{:#}", parse_run_env("[run.env]\n\nA = 1").unwrap_err());
        assert!(error.starts_with("dagq.toml:3: value of A"), "{error}");
    }

    #[test]
    fn expands_only_the_queue_and_run_directories() {
        assert_eq!(
            expand(
                "${DAGQ_QUEUE_DIR}/target:${DAGQ_RUN_DIR}/tmp:${HOME}:$DAGQ_RUN_DIR:${DAGQ_QUEUE_DIR}",
                "/q",
                "/q/runs/r"
            ),
            "/q/target:/q/runs/r/tmp:${HOME}:$DAGQ_RUN_DIR:/q"
        );
    }

    #[test]
    fn loads_and_expands_the_file_in_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Path::new("/data/dagq/abc");
        let run = Path::new("/data/dagq/abc/runs/r1");
        assert!(load_run_env(dir.path(), queue, run).unwrap().is_empty());
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[run.env]\nCARGO_TARGET_DIR = '${DAGQ_QUEUE_DIR}/target'\nTMPDIR = \"${DAGQ_RUN_DIR}/tmp\"\n",
        )
        .unwrap();
        assert_eq!(
            load_run_env(dir.path(), queue, run).unwrap(),
            pairs(&[
                ("CARGO_TARGET_DIR", "/data/dagq/abc/target"),
                ("TMPDIR", "/data/dagq/abc/runs/r1/tmp"),
            ])
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[other]\n").unwrap();
        let error = format!("{:#}", load_run_env(dir.path(), queue, run).unwrap_err());
        assert!(
            error.contains("parse ") && error.contains("unknown table"),
            "{error}"
        );
    }

    #[test]
    fn unreadable_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(CONFIG_FILE_NAME)).unwrap();
        let error = format!(
            "{:#}",
            load_run_env(dir.path(), Path::new("/q"), Path::new("/r")).unwrap_err()
        );
        assert!(error.contains("read "), "{error}");
    }
}
