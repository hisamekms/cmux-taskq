//! Whether the programs the `[run.env]` of `dagq.toml` names resolve
//! (ADR-0049 decision 9): a `RUSTC_WRAPPER` that cannot be executed makes
//! every cargo command of a run fail, so `up` refuses to start a
//! supervisor, the supervisor claims and lands nothing, and `integrate`
//! runs no verification command while one is missing. Which variables
//! name a program and how a value resolves live with the reading of the
//! file (`infrastructure::run_env`); this is what a check found and what
//! it means.

use serde::Serialize;
use serde_json::{Value, json};

/// Recorded on the queue when the supervisor's check turns from found to
/// missing: an attention (`install tool`) for the inbox.
pub const RUN_ENV_PROGRAM_MISSING: &str = "run_env_program_missing";
/// Recorded when every program resolves again after a
/// [`RUN_ENV_PROGRAM_MISSING`]; the attention ends with it.
pub const RUN_ENV_PROGRAM_FOUND: &str = "run_env_program_found";
/// The two kinds, for reading the latest of them.
pub const RUN_ENV_PROGRAM_KINDS: [&str; 2] = [RUN_ENV_PROGRAM_MISSING, RUN_ENV_PROGRAM_FOUND];

/// One variable of `[run.env]` that names a program, and where it resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunEnvProgram {
    pub variable: String,
    /// The value, `${DAGQ_*}` expanded.
    pub value: String,
    /// The executable file it resolved to; `None` when there is none.
    pub resolved: Option<String>,
}

/// What one check of `[run.env]` found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RunEnvCheck {
    /// Whether the repository has a `dagq.toml` at all; without one there is
    /// nothing to check.
    pub config: bool,
    /// The `PATH` a value without a `/` was looked up in.
    pub path: String,
    /// Every variable that names a program, in file order; a variable with
    /// an empty value names none.
    pub programs: Vec<RunEnvProgram>,
}

impl RunEnvCheck {
    /// The programs that did not resolve.
    pub fn missing(&self) -> Vec<&RunEnvProgram> {
        self.programs
            .iter()
            .filter(|program| program.resolved.is_none())
            .collect()
    }

    /// Why nothing that runs cargo for a run may start, or `None` when every
    /// program resolved: each missing variable with its value, the PATH,
    /// and what a person does about it.
    pub fn missing_message(&self) -> Option<String> {
        let missing = self.missing();
        if missing.is_empty() {
            return None;
        }
        let names = missing
            .iter()
            .map(|program| format!("{} = {:?}", program.variable, program.value))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "the [run.env] of dagq.toml names a program that cannot be executed: {names} (PATH: {}); \
install it where this PATH finds it, or register a task that takes the variable out of dagq.toml (ADR-0049 decision 9)",
            self.path
        ))
    }

    /// The event the supervisor records when its check differs from the
    /// latest one on the queue (`last`, a kind of [`RUN_ENV_PROGRAM_KINDS`]):
    /// missing after anything but missing, found after missing, else none.
    pub fn transition(&self, last: Option<&str>) -> Option<(&'static str, Value)> {
        let missing = self.missing();
        let programs = serde_json::to_value(&self.programs).unwrap_or(Value::Null);
        match missing.first() {
            Some(first) if last != Some(RUN_ENV_PROGRAM_MISSING) => Some((
                RUN_ENV_PROGRAM_MISSING,
                json!({
                    "variable": first.variable,
                    "value": first.value,
                    "path": self.path,
                    "programs": programs,
                    "message": self.missing_message(),
                }),
            )),
            None if last == Some(RUN_ENV_PROGRAM_MISSING) => Some((
                RUN_ENV_PROGRAM_FOUND,
                json!({"path": self.path, "programs": programs}),
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(variable: &str, value: &str, resolved: Option<&str>) -> RunEnvProgram {
        RunEnvProgram {
            variable: variable.into(),
            value: value.into(),
            resolved: resolved.map(str::to_owned),
        }
    }

    fn check(programs: Vec<RunEnvProgram>) -> RunEnvCheck {
        RunEnvCheck {
            config: true,
            path: "/usr/bin:/bin".into(),
            programs,
        }
    }

    #[test]
    fn nothing_to_check_is_satisfied_and_records_nothing() {
        let none = RunEnvCheck::default();
        assert!(none.missing().is_empty());
        assert_eq!(none.missing_message(), None);
        assert_eq!(none.transition(None), None);
        assert_eq!(none.transition(Some(RUN_ENV_PROGRAM_FOUND)), None);
    }

    #[test]
    fn a_missing_program_is_named_with_its_value_and_the_path() {
        let found = check(vec![
            program("RUSTC_WRAPPER", "sccache", None),
            program("RUSTDOC", "/usr/bin/rustdoc", Some("/usr/bin/rustdoc")),
            program("RUSTC", "/opt/rustc", None),
        ]);
        assert_eq!(found.missing().len(), 2);
        let message = found.missing_message().unwrap();
        assert!(
            message.contains("RUSTC_WRAPPER = \"sccache\", RUSTC = \"/opt/rustc\"")
                && message.contains("(PATH: /usr/bin:/bin)")
                && message.contains("takes the variable out of dagq.toml"),
            "{message}"
        );
        assert!(!message.contains("RUSTDOC"), "{message}");
    }

    #[test]
    fn records_only_the_changes_between_missing_and_found() {
        let missing = check(vec![program("RUSTC_WRAPPER", "sccache", None)]);
        let found = check(vec![program(
            "RUSTC_WRAPPER",
            "sccache",
            Some("/bin/sccache"),
        )]);
        for last in [None, Some(RUN_ENV_PROGRAM_FOUND)] {
            let (kind, payload) = missing.transition(last).unwrap();
            assert_eq!(kind, RUN_ENV_PROGRAM_MISSING);
            assert_eq!(payload["variable"], "RUSTC_WRAPPER");
            assert_eq!(payload["value"], "sccache");
            assert_eq!(payload["path"], "/usr/bin:/bin");
            assert_eq!(payload["programs"][0]["resolved"], Value::Null);
        }
        assert_eq!(missing.transition(Some(RUN_ENV_PROGRAM_MISSING)), None);
        let (kind, payload) = found.transition(Some(RUN_ENV_PROGRAM_MISSING)).unwrap();
        assert_eq!(kind, RUN_ENV_PROGRAM_FOUND);
        assert_eq!(payload["programs"][0]["resolved"], "/bin/sccache");
        assert_eq!(found.transition(None), None);
        assert_eq!(found.transition(Some(RUN_ENV_PROGRAM_FOUND)), None);
    }
}
