//! The repository's `dagq.toml` (ADR-0023 decision 3): `[run.env]` holds
//! environment variables every run gets, in its worker workspace and in the
//! verification commands; its values are strings in which
//! `${DAGQ_QUEUE_DIR}` and `${DAGQ_RUN_DIR}` are expanded. `[stall]` holds
//! the thresholds of the stalled-session checks in seconds (ADR-0043
//! decision 4). `[conflicts]` holds the thresholds of the
//! `conflict_hotspot` alert of `stats` (goal 31). `[recheck]` holds the
//! `command` the landing recheck runs on main's tree with a waiting run
//! merged in (ADR-0068 decision 2). The file is parsed by
//! hand: the format is these tables of `KEY = value` lines, a subset of
//! TOML that needs no parser crate.
use anyhow::{Context, Result, bail, ensure};
use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    application::{Exit, Verifier},
    domain::{
        run_env::{RunEnvCheck, RunEnvProgram},
        stall::StallConfig,
        stats::ConflictConfig,
    },
};

pub const CONFIG_FILE_NAME: &str = "dagq.toml";
pub const QUEUE_DIR_VAR: &str = "DAGQ_QUEUE_DIR";
pub const RUN_DIR_VAR: &str = "DAGQ_RUN_DIR";
const RUN_ENV_TABLE: &str = "run.env";
const STALL_TABLE: &str = "stall";
const CONFLICTS_TABLE: &str = "conflicts";
const RECHECK_TABLE: &str = "recheck";
const TABLES: [&str; 4] = [RUN_ENV_TABLE, STALL_TABLE, CONFLICTS_TABLE, RECHECK_TABLE];
/// The one key of `[recheck]`.
const RECHECK_COMMAND: &str = "command";
/// Names the runtime itself sets on a workspace (`DAGQ_ROLE`, `DAGQ_QUEUE`)
/// and may set later; `[run.env]` cannot override them.
const RESERVED_PREFIX: &str = "DAGQ_";

/// The variables of `[run.env]` cargo executes as a program (ADR-0049
/// decision 9): `up`, the supervisor, `integrate` and `doctor` check that
/// each one's value resolves. A tool other than cargo's is not inferred;
/// naming one needs an ADR that adds a table declaring it.
pub const PROGRAM_VARIABLES: [&str; 8] = [
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTC",
    "RUSTDOC",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_RUSTC",
    "CARGO_BUILD_RUSTDOC",
];

/// `[run.env]` as written, in file order, values unexpanded.
pub fn parse_run_env(text: &str) -> Result<Vec<(String, String)>> {
    Ok(parse_config(text)?.run_env)
}

/// What the file holds: `[run.env]`, `[stall]` (ADR-0043 decision 4),
/// `[conflicts]` and `[recheck]` (ADR-0068 decision 2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// `[run.env]` as written, in file order, values unexpanded.
    pub run_env: Vec<(String, String)>,
    /// `[stall]`, the defaults for the keys it does not set.
    pub stall: StallConfig,
    /// `[conflicts]`, the defaults for the keys it does not set.
    pub conflicts: ConflictConfig,
    /// `[recheck] command`; none checks the merge only.
    pub recheck_command: Option<String>,
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
                    "{CONFIG_FILE_NAME}:{number}: unknown table [{name}]; only [{RUN_ENV_TABLE}], [{STALL_TABLE}], [{CONFLICTS_TABLE}] and [{RECHECK_TABLE}] are supported"
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
            Some(RECHECK_TABLE) => {
                ensure!(
                    key == RECHECK_COMMAND,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{RECHECK_TABLE}]; the key is {RECHECK_COMMAND}"
                );
                ensure!(
                    config.recheck_command.is_none(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let command = parse_string(rest.trim())
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                ensure!(
                    !command.trim().is_empty(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is blank"
                );
                config.recheck_command = Some(command);
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
                "{CONFIG_FILE_NAME}:{number}: a key outside [{RUN_ENV_TABLE}], [{STALL_TABLE}], [{CONFLICTS_TABLE}] or [{RECHECK_TABLE}]"
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

/// `[recheck] command` of the `dagq.toml` in `root`; no file, no table or
/// no key is none.
pub fn load_recheck_command(root: &Path) -> Result<Option<String>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .recheck_command)
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

/// `[run.env]` of the `dagq.toml` in `root` as written, values unexpanded;
/// `None` without the file.
pub fn load_run_env_table(root: &Path) -> Result<Option<Vec<(String, String)>>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    parse_run_env(&text)
        .map(Some)
        .with_context(|| format!("parse {}", path.display()))
}

/// The file in the queue's directory holding its `[run.env]` hash salt.
pub const RUN_ENV_SALT_FILE: &str = "run-env-salt";

/// The salt in `<queue_dir>/run-env-salt`, made (random, owner-only) when
/// there is none. It is written to a temporary file and linked into place,
/// so the file is never seen empty, and two processes making it at once
/// keep the first one's. A lost file gets a new salt, and the next
/// `run_env_changed` names every key once.
pub fn run_env_salt(queue_dir: &Path) -> Result<String> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let path = queue_dir.join(RUN_ENV_SALT_FILE);
    let read = || -> Result<String> {
        let salt = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        ensure!(!salt.trim().is_empty(), "{} is empty", path.display());
        Ok(salt.trim().to_owned())
    };
    if path.exists() {
        return read();
    }
    let salt = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let temporary = queue_dir.join(format!(
        ".{RUN_ENV_SALT_FILE}.{}",
        uuid::Uuid::new_v4().simple()
    ));
    let made = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .and_then(|mut file| file.write_all(salt.as_bytes()))
        .with_context(|| format!("write {}", temporary.display()))
        .and_then(|()| match fs::hard_link(&temporary, &path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error).with_context(|| format!("link {}", path.display())),
        });
    let _ = fs::remove_file(&temporary);
    if made? { Ok(salt) } else { read() }
}

/// Where `value` resolves as a program: a value with a `/` is that path, one
/// without is looked up in each directory of `path` in order, like a shell
/// does; either must be an executable file.
pub fn resolve_program(value: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    if value.contains('/') {
        let candidate = PathBuf::from(value);
        return is_executable(&candidate).then_some(candidate);
    }
    env::split_paths(path?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(value))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Resolve every variable of `env` (expanded) that names a program
/// ([`PROGRAM_VARIABLES`]) in `path`. An empty value names none (cargo runs
/// no wrapper for an empty `RUSTC_WRAPPER`).
pub fn check_programs(env: &[(String, String)], path: Option<&OsStr>) -> RunEnvCheck {
    RunEnvCheck {
        config: true,
        path: path
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        programs: env
            .iter()
            .filter(|(key, value)| PROGRAM_VARIABLES.contains(&key.as_str()) && !value.is_empty())
            .map(|(key, value)| RunEnvProgram {
                variable: key.clone(),
                value: value.clone(),
                resolved: resolve_program(value, path)
                    .map(|resolved| resolved.to_string_lossy().into_owned()),
            })
            .collect(),
    }
}

/// Check the programs of the `[run.env]` in the `dagq.toml` of `root` in
/// `path`, the values expanded for the run whose directory is `run_dir`.
/// Without a run (`up`, a claim, `doctor`) a value that names
/// `${DAGQ_RUN_DIR}` is not checked: nothing is in a run directory before
/// the run exists. No file is nothing to check.
pub fn check_run_env_programs(
    root: &Path,
    queue_dir: &Path,
    run_dir: Option<&Path>,
    path: Option<&OsStr>,
) -> Result<RunEnvCheck> {
    let file = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&file)? else {
        return Ok(RunEnvCheck::default());
    };
    let queue_dir = path_str(queue_dir)?;
    let run_dir = run_dir.map(path_str).transpose()?;
    let run_dir_var = format!("${{{RUN_DIR_VAR}}}");
    let env: Vec<(String, String)> = parse_run_env(&text)
        .with_context(|| format!("parse {}", file.display()))?
        .into_iter()
        .filter(|(_, value)| run_dir.is_some() || !value.contains(&run_dir_var))
        .map(|(key, value)| {
            let value = expand(&value, queue_dir, run_dir.unwrap_or_default());
            (key, value)
        })
        .collect();
    Ok(check_programs(&env, path))
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

    fn run_env_programs(&self, run_dir: Option<&Path>) -> Result<RunEnvCheck> {
        let queue_dir = self
            .db
            .parent()
            .context("queue database has no directory")?;
        check_run_env_programs(
            &self.checkout,
            queue_dir,
            run_dir,
            env::var_os("PATH").as_deref(),
        )
    }

    fn run_env_table(&self) -> Result<Option<Vec<(String, String)>>> {
        load_run_env_table(&self.checkout)
    }

    fn run_env_salt(&self) -> Result<String> {
        let queue_dir = self
            .db
            .parent()
            .context("queue database has no directory")?;
        run_env_salt(queue_dir)
    }

    fn recheck_command(&self) -> Result<Option<String>> {
        load_recheck_command(&self.checkout)
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

    #[test]
    fn the_run_env_salt_is_made_once_and_kept_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let salt = run_env_salt(dir.path()).unwrap();
        assert_eq!(salt.len(), 64);
        assert_eq!(run_env_salt(dir.path()).unwrap(), salt);
        let file = dir.path().join(RUN_ENV_SALT_FILE);
        let mode = fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::write(&file, "").unwrap();
        assert!(run_env_salt(dir.path()).is_err());
        fs::remove_file(&file).unwrap();
        assert_ne!(run_env_salt(dir.path()).unwrap(), salt);
    }

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
            ("[recheck]\nargs = 'x'", "unknown key args in [recheck]"),
            (
                "[recheck]\ncommand = 'x'\ncommand = 'y'",
                "command is defined twice",
            ),
            ("[recheck]\ncommand = ' '", "command is blank"),
            ("[recheck]\ncommand = x", "expected a quoted string"),
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
    fn reads_the_recheck_command() {
        let config =
            parse_config("[recheck]\ncommand = \"cargo check --locked\" # fast\n").unwrap();
        assert_eq!(
            config.recheck_command.as_deref(),
            Some("cargo check --locked")
        );
        assert_eq!(parse_config("").unwrap().recheck_command, None);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_recheck_command(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[run.env]\nA = 'x'\n[recheck]\ncommand = 'make check'\n",
        )
        .unwrap();
        assert_eq!(
            load_recheck_command(dir.path()).unwrap().as_deref(),
            Some("make check")
        );
        let verifier = ShellVerifier {
            checkout: dir.path().to_owned(),
            db: dir.path().join("queue.db"),
        };
        assert_eq!(
            verifier.recheck_command().unwrap().as_deref(),
            Some("make check")
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[recheck]\nnope = 1\n").unwrap();
        assert!(load_recheck_command(dir.path()).is_err());
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

    fn executable(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn resolves_a_program_by_path_or_in_the_path_list() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let tool = executable(&bin, "tool");
        fs::write(bin.join("plain"), "not executable").unwrap();
        fs::create_dir(bin.join("folder")).unwrap();
        let path = env::join_paths(["", "/nonexistent", bin.to_str().unwrap()]).unwrap();
        assert_eq!(resolve_program("tool", Some(&path)), Some(tool.clone()));
        assert_eq!(resolve_program(tool.to_str().unwrap(), None), Some(tool));
        for missing in ["plain", "folder", "absent"] {
            assert_eq!(resolve_program(missing, Some(&path)), None, "{missing}");
        }
        assert_eq!(resolve_program("tool", None), None);
        assert_eq!(
            resolve_program(bin.join("plain").to_str().unwrap(), Some(&path)),
            None
        );
    }

    #[test]
    fn checks_only_the_variables_cargo_executes_with_a_value() {
        let dir = tempfile::tempdir().unwrap();
        executable(dir.path(), "sccache");
        let path = dir.path().as_os_str();
        let check = check_programs(
            &pairs(&[
                ("RUSTC_WRAPPER", "sccache"),
                ("SCCACHE_IGNORE_SERVER_IO_ERROR", "1"),
                ("RUSTC_WORKSPACE_WRAPPER", ""),
                ("CARGO_BUILD_RUSTDOC", "missing-rustdoc"),
            ]),
            Some(path),
        );
        assert!(check.config);
        assert_eq!(check.path, dir.path().to_str().unwrap());
        assert_eq!(
            check.programs,
            vec![
                RunEnvProgram {
                    variable: "RUSTC_WRAPPER".into(),
                    value: "sccache".into(),
                    resolved: Some(dir.path().join("sccache").to_str().unwrap().into()),
                },
                RunEnvProgram {
                    variable: "CARGO_BUILD_RUSTDOC".into(),
                    value: "missing-rustdoc".into(),
                    resolved: None,
                },
            ]
        );
        assert_eq!(check.missing().len(), 1);
    }

    #[test]
    fn checks_the_run_env_of_the_file_in_the_root() {
        let root = tempfile::tempdir().unwrap();
        let queue = tempfile::tempdir().unwrap();
        let bin = queue.path().join("bin");
        fs::create_dir(&bin).unwrap();
        executable(&bin, "sccache");
        let path = bin.as_os_str();
        // No dagq.toml: nothing to check, whatever the PATH holds.
        let none = check_run_env_programs(root.path(), queue.path(), None, None).unwrap();
        assert_eq!(none, RunEnvCheck::default());
        assert!(none.missing_message().is_none());
        // A [run.env] without a program: nothing missing.
        fs::write(root.path().join(CONFIG_FILE_NAME), "[run.env]\nA = 'x'\n").unwrap();
        let plain = check_run_env_programs(root.path(), queue.path(), None, Some(path)).unwrap();
        assert!(plain.config && plain.programs.is_empty());
        // Found on the PATH, by name and by an expanded path.
        fs::write(
            root.path().join(CONFIG_FILE_NAME),
            "[run.env]\nRUSTC_WRAPPER = 'sccache'\nRUSTC_WORKSPACE_WRAPPER = '${DAGQ_QUEUE_DIR}/bin/sccache'\nRUSTDOC = '${DAGQ_RUN_DIR}/rustdoc'\n",
        )
        .unwrap();
        let found = check_run_env_programs(root.path(), queue.path(), None, Some(path)).unwrap();
        assert_eq!(found.programs.len(), 2, "{found:?}");
        assert!(found.missing().is_empty(), "{found:?}");
        // With a run, the run directory is expanded and checked too.
        let run = queue.path().join("runs/r1");
        fs::create_dir_all(&run).unwrap();
        let in_run =
            check_run_env_programs(root.path(), queue.path(), Some(&run), Some(path)).unwrap();
        assert_eq!(in_run.missing()[0].variable, "RUSTDOC");
        assert_eq!(
            in_run.missing()[0].value,
            run.join("rustdoc").to_str().unwrap()
        );
        // Missing from the PATH.
        let missing = check_run_env_programs(
            root.path(),
            queue.path(),
            None,
            Some(OsStr::new("/nonexistent")),
        )
        .unwrap();
        assert_eq!(missing.missing()[0].variable, "RUSTC_WRAPPER");
        assert!(
            missing
                .missing_message()
                .unwrap()
                .contains("RUSTC_WRAPPER = \"sccache\"")
        );
        fs::write(root.path().join(CONFIG_FILE_NAME), "[other]\n").unwrap();
        assert!(check_run_env_programs(root.path(), queue.path(), None, Some(path)).is_err());
    }

    #[test]
    fn the_shell_verifier_checks_on_the_process_path() {
        let root = tempfile::tempdir().unwrap();
        let queue = tempfile::tempdir().unwrap();
        let verifier = ShellVerifier {
            checkout: root.path().to_path_buf(),
            db: queue.path().join("queue.sqlite3"),
        };
        assert!(!verifier.run_env_programs(None).unwrap().config);
        fs::write(
            root.path().join(CONFIG_FILE_NAME),
            "[run.env]\nRUSTC_WRAPPER = '/nonexistent/sccache'\nRUSTC = 'sh'\n",
        )
        .unwrap();
        let check = verifier.run_env_programs(Some(queue.path())).unwrap();
        assert_eq!(check.programs.len(), 2);
        assert_eq!(check.missing().len(), 1, "{check:?}");
        assert_eq!(check.missing()[0].variable, "RUSTC_WRAPPER");
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
