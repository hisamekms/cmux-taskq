//! Running the `dagq` binary against a queue for the CLI tests
//! (`tests/cli_*.rs`), and the queue they start from.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;
use tempfile::TempDir;

use super::Bounded;

/// A fresh queue, `queue.db` in a new temporary directory, after `init`.
/// The directory lives as long as the returned guard.
pub fn queue() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    (dir, db)
}

pub fn invoke(db: &Path, args: &[&str]) -> Output {
    invoke_as(None, db, args)
}

/// Run with `DAGQ_ROLE` set to `role`, or unset: the tests do not inherit
/// the role of the session running them. A stub `cmux` next to the queue
/// comes first on PATH, so `ask` never notifies the person running the
/// tests; it appends its arguments to [`notifications`] instead.
pub fn invoke_as(role: Option<&str>, db: &Path, args: &[&str]) -> Output {
    let bin = db.parent().unwrap().join("bin");
    if !bin.join("cmux").exists() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&bin).unwrap();
        let stub = bin.join("cmux");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n",
                bin.join("notifications").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command.env("PATH", path).env_remove("DAGQ_ROLE");
    if let Some(role) = role {
        command.env("DAGQ_ROLE", role);
    }
    command
        .arg("--db")
        .arg(db)
        .args(args)
        .bounded_output()
        .unwrap()
}

/// Every argument the stub `cmux` of `db`'s directory was called with, one per line.
pub fn notifications(db: &Path) -> String {
    std::fs::read_to_string(db.parent().unwrap().join("bin/notifications")).unwrap_or_default()
}

pub fn ok_as(role: &str, db: &Path, args: &[&str]) -> Value {
    let output = invoke_as(Some(role), db, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

pub fn ok(db: &Path, args: &[&str]) -> Value {
    let output = invoke(db, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The domain's refusal of `args`, as the CLI prints it.
pub fn refused(db: &Path, args: &[&str]) -> String {
    let output = invoke(db, args);
    assert!(!output.status.success(), "{args:?} succeeded");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    error["error"].as_str().unwrap().to_owned()
}

/// `submit` as a planner session would run it: in a cmux workspace, with
/// the planner's origin when the runtime opened it.
pub fn submit_from(
    db: &Path,
    workspace: Option<&str>,
    origin: Option<&str>,
    args: &[&str],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command
        .env_remove("DAGQ_ROLE")
        .env_remove("CMUX_WORKSPACE_ID")
        .env_remove("DAGQ_PLANNER_ORIGIN");
    if let Some(workspace) = workspace {
        command.env("CMUX_WORKSPACE_ID", workspace);
    }
    if let Some(origin) = origin {
        command.env("DAGQ_PLANNER_ORIGIN", origin);
    }
    command
        .arg("--db")
        .arg(db)
        .arg("submit")
        .args(args)
        .bounded_output()
        .unwrap()
}
