//! The repository's `dagq.toml` (ADR-0023 decision 3): `[run.env]` holds
//! environment variables every run gets, in its worker workspace and in the
//! verification commands. Only that table exists; its values are strings in
//! which `${DAGQ_QUEUE_DIR}` and `${DAGQ_RUN_DIR}` are expanded. The file is
//! parsed by hand: the format is one table of `KEY = "value"` lines, a subset
//! of TOML that needs no parser crate.
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::application::{Exit, Verifier};

pub const CONFIG_FILE_NAME: &str = "dagq.toml";
pub const QUEUE_DIR_VAR: &str = "DAGQ_QUEUE_DIR";
pub const RUN_DIR_VAR: &str = "DAGQ_RUN_DIR";
const RUN_ENV_TABLE: &str = "run.env";
/// Names the runtime itself sets on a workspace (`DAGQ_ROLE`, `DAGQ_QUEUE`)
/// and may set later; `[run.env]` cannot override them.
const RESERVED_PREFIX: &str = "DAGQ_";

/// `[run.env]` as written, in file order, values unexpanded.
pub fn parse_run_env(text: &str) -> Result<Vec<(String, String)>> {
    let mut table: Option<String> = None;
    let mut env: Vec<(String, String)> = Vec::new();
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
            ensure!(
                name == RUN_ENV_TABLE,
                "{CONFIG_FILE_NAME}:{number}: unknown table [{name}]; only [{RUN_ENV_TABLE}] is supported"
            );
            ensure!(
                table.is_none(),
                "{CONFIG_FILE_NAME}:{number}: [{RUN_ENV_TABLE}] is defined twice"
            );
            table = Some(name.to_owned());
            continue;
        }
        let (key, rest) = line
            .split_once('=')
            .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: expected KEY = \"value\""))?;
        ensure!(
            table.is_some(),
            "{CONFIG_FILE_NAME}:{number}: a key outside [{RUN_ENV_TABLE}]"
        );
        let key = key.trim();
        ensure!(
            is_env_name(key),
            "{CONFIG_FILE_NAME}:{number}: {key:?} is not an environment variable name"
        );
        ensure!(
            !key.starts_with(RESERVED_PREFIX),
            "{CONFIG_FILE_NAME}:{number}: {key} uses the reserved prefix {RESERVED_PREFIX}"
        );
        ensure!(
            env.iter().all(|(existing, _)| existing != key),
            "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
        );
        let value = parse_string(rest.trim())
            .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
        env.push((key.to_owned(), value));
    }
    Ok(env)
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
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
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
