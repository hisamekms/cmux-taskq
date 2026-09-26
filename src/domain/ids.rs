//! Domain primitives for the identifiers the queue hands out and the Git
//! commits it records (ADR-0013 decision 4). Each wraps the raw value in a
//! private field, so a task ID cannot stand in for a goal ID, and a
//! [`RunId`] or [`CommitSha`] that exists has passed its check. They
//! serialize as the bare value, so JSON output is unchanged; the SQLite
//! conversions live in `infrastructure`.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use super::DomainError;

/// Equality with the raw text, both ways, for a value that is compared
/// with what Git or a receipt printed without checking that text first.
macro_rules! eq_text {
    ($name:ident) => {
        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool {
                self.0 == *other
            }
        }

        impl PartialEq<$name> for str {
            fn eq(&self, other: &$name) -> bool {
                self == other.0
            }
        }

        impl PartialEq<$name> for &str {
            fn eq(&self, other: &$name) -> bool {
                *self == other.0
            }
        }

        impl PartialEq<$name> for String {
            fn eq(&self, other: &$name) -> bool {
                *self == other.0
            }
        }
    };
}

/// The ID of a task: the `tasks.id` rowid, as the CLI takes and prints it.
/// Any `i64` is a well-formed ID; whether one names a task is the store's to
/// answer, and a new task's dependencies are checked for positivity by
/// [`super::NewTask::validate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(i64);

impl TaskId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The ID of a goal: the `goals.id` rowid. Distinct from [`TaskId`] so the two
/// cannot be swapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GoalId(i64);

impl GoalId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

impl fmt::Display for GoalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The ID of a proposal: the `proposals.id` rowid (ADR-0041 decision 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProposalId(i64);

impl ProposalId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

impl fmt::Display for ProposalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The ID of a finding: the `findings.id` rowid (ADR-0044 decision 18).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FindingId(i64);

impl FindingId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

impl fmt::Display for FindingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The ID of a planner session: the `planners.id` rowid (ADR-0041
/// decisions 1, 6). Its workspace title and directory carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlannerId(i64);

impl PlannerId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

impl fmt::Display for PlannerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The ID of an ask: the `asks.id` rowid, as `answer`, `close` and
/// `deliver` take it. Distinct from [`TaskId`] and [`EventId`] so an ask's
/// ID cannot be passed where another rowid is meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AskId(i64);

impl AskId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

impl fmt::Display for AskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The ID of a `run_events` row, which is also the cursor `status` hands out
/// and `watch` and `stats` read past: events are numbered in the order they
/// were recorded, and 0 is the cursor before the first one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(i64);

impl EventId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The ID of a run: a UUID the runtime generates at claim time, also the
/// name of the run's directory and of its branch `dagq/<run id>`. It is never
/// blank.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    pub fn new(id: impl Into<String>) -> Result<Self, DomainError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(DomainError::Blank { field: "run ID" });
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

eq_text!(RunId);

impl TryFrom<String> for RunId {
    type Error = DomainError;

    fn try_from(id: String) -> Result<Self, DomainError> {
        Self::new(id)
    }
}

impl TryFrom<&str> for RunId {
    type Error = DomainError;

    fn try_from(id: &str) -> Result<Self, DomainError> {
        Self::new(id)
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RunId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RunId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// A full Git object ID: 40 (SHA-1) or 64 (SHA-256) hexadecimal digits.
/// Abbreviated and symbolic names (`HEAD`, `main`) are not commits here.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CommitSha(String);

impl CommitSha {
    /// Checks `commit`, naming it `field` in the error (`base commit`,
    /// `receipt commit`, ...).
    pub fn parse(commit: impl Into<String>, field: &'static str) -> Result<Self, DomainError> {
        let commit = commit.into();
        if matches!(commit.len(), 40 | 64) && commit.bytes().all(|c| c.is_ascii_hexdigit()) {
            Ok(Self(commit))
        } else {
            Err(DomainError::InvalidCommit { field })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

eq_text!(CommitSha);

impl TryFrom<String> for CommitSha {
    type Error = DomainError;

    fn try_from(commit: String) -> Result<Self, DomainError> {
        Self::parse(commit, "commit")
    }
}

impl TryFrom<&str> for CommitSha {
    type Error = DomainError;

    fn try_from(commit: &str) -> Result<Self, DomainError> {
        Self::parse(commit, "commit")
    }
}

impl fmt::Display for CommitSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for CommitSha {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for CommitSha {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA1: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn ids_print_and_serialize_as_the_bare_value() {
        let task = TaskId::new(7);
        let goal = GoalId::new(3);
        let run = RunId::new("r-1").unwrap();
        let commit = CommitSha::parse(SHA1, "base commit").unwrap();
        assert_eq!(task.to_string(), "7");
        assert_eq!(goal.to_string(), "3");
        assert_eq!(run.to_string(), "r-1");
        assert_eq!(commit.to_string(), SHA1);
        assert_eq!(
            serde_json::to_string(&(task, goal, &run, &commit)).unwrap(),
            format!("[7,3,\"r-1\",\"{SHA1}\"]")
        );
        assert_eq!(task.as_i64(), 7);
        assert_eq!(goal.as_i64(), 3);
        assert_eq!(run.as_str(), "r-1");
        assert_eq!(commit.as_str(), SHA1);
        let (text, owned): (&str, String) = ("r-1", "r-1".into());
        assert_eq!(run, text);
        assert_eq!(run, *text);
        assert_eq!(run, owned);
        assert_eq!(text, run);
        assert_eq!(*text, run);
        assert_eq!(owned, run);
        assert_eq!(commit, SHA1);
        assert_ne!("0".repeat(40), commit);
    }

    #[test]
    fn a_run_id_is_never_blank() {
        assert_eq!(
            RunId::new(" ").unwrap_err().to_string(),
            "run ID must not be blank"
        );
        assert!(RunId::try_from("").is_err());
        assert!(serde_json::from_str::<RunId>("\"\"").is_err());
        assert_eq!(
            serde_json::from_str::<RunId>("\"abc\"").unwrap(),
            RunId::try_from(String::from("abc")).unwrap()
        );
    }

    #[test]
    fn a_commit_is_a_full_hexadecimal_object_id() {
        assert_eq!(
            CommitSha::parse("abc", "base commit")
                .unwrap_err()
                .to_string(),
            "base commit: must be a full 40- or 64-character hexadecimal Git object ID"
        );
        assert!(CommitSha::try_from("HEAD").is_err());
        assert!(CommitSha::try_from(SHA1.replace('0', "g")).is_err());
        assert!(CommitSha::try_from("a".repeat(64)).is_ok());
        assert!(serde_json::from_str::<CommitSha>("\"main\"").is_err());
        assert_eq!(
            serde_json::from_str::<CommitSha>(&format!("\"{SHA1}\"")).unwrap(),
            CommitSha::try_from(SHA1).unwrap()
        );
    }

    #[test]
    fn integer_ids_order_by_value() {
        assert!(TaskId::new(1) < TaskId::new(2));
        assert!(GoalId::new(2) > GoalId::new(1));
        assert!(AskId::new(1) < AskId::new(2));
        assert!(EventId::new(0) < EventId::new(1));
        assert_eq!(serde_json::from_str::<TaskId>("5").unwrap(), TaskId::new(5));
        assert_eq!(serde_json::from_str::<GoalId>("5").unwrap(), GoalId::new(5));
        assert_eq!(serde_json::from_str::<AskId>("5").unwrap(), AskId::new(5));
        assert_eq!(
            serde_json::from_str::<EventId>("5").unwrap(),
            EventId::new(5)
        );
        assert_eq!(AskId::new(4).to_string(), "4");
        assert_eq!(EventId::new(9).to_string(), "9");
        assert_eq!(AskId::new(4).as_i64(), 4);
        assert_eq!(EventId::new(9).as_i64(), 9);
        assert_eq!(
            serde_json::to_string(&(AskId::new(4), EventId::new(9))).unwrap(),
            "[4,9]"
        );
    }
}
