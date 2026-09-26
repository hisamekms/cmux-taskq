//! Queue resolution from the working directory: one queue per repository under
//! the user data directory, shared by all of its worktrees.

use crate::common;

use common::Bounded;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use dagq::infrastructure::{location::repository_hash, sqlite::SqliteQueue};
use serde_json::Value;
use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) {
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn repository(dir: &Path, name: &str) -> PathBuf {
    let repo = dir.join(name);
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "seed"]);
    repo
}

/// Run the binary from `cwd` with a controlled environment; `env` overrides
/// `XDG_DATA_HOME`/`HOME` (both removed first) so no real queue is touched.
fn invoke(cwd: &Path, env: &[(&str, &Path)], args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command
        .env_remove("XDG_DATA_HOME")
        .env_remove("HOME")
        .current_dir(cwd)
        .args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    command.bounded_output().unwrap()
}

fn ok(cwd: &Path, env: &[(&str, &Path)], args: &[&str]) -> Value {
    let output = invoke(cwd, env, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn error(cwd: &Path, env: &[(&str, &Path)], args: &[&str]) -> String {
    let output = invoke(cwd, env, args);
    assert!(!output.status.success(), "{args:?} succeeded unexpectedly");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    error["error"].as_str().unwrap().to_owned()
}

fn expected_db(data_home: &Path, repo: &Path) -> PathBuf {
    let common_dir = repo.join(".git").canonicalize().unwrap();
    data_home
        .join("dagq")
        .join(repository_hash(&common_dir))
        .join("queue.db")
}

fn fixture() -> (TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let data_home = dir.path().join("xdg data");
    let repo = repository(dir.path(), "repo");
    (dir, data_home, repo)
}

#[test]
fn every_worktree_of_a_repository_shares_one_queue_under_the_data_home() {
    let (dir, data_home, repo) = fixture();
    let env = [("XDG_DATA_HOME", data_home.as_path())];
    let db = expected_db(&data_home, &repo);
    let common_dir = repo.join(".git").canonicalize().unwrap();

    // Nothing is created before init, and locate never opens the queue.
    let located = ok(&repo, &env, &["locate"]);
    assert_eq!(located["db"], db.to_str().unwrap());
    assert_eq!(located["db_exists"], false);
    assert_eq!(located["source"], "repository");
    assert_eq!(located["git_common_dir"], common_dir.to_str().unwrap());
    assert_eq!(located["queue_dir"], db.parent().unwrap().to_str().unwrap());
    assert_eq!(
        located["log_dir"],
        db.parent().unwrap().join("logs").to_str().unwrap()
    );
    // The LaunchAgent is named after the queue, lives under HOME, and is
    // reported before `up` writes it.
    let hash = db.parent().unwrap().file_name().unwrap().to_str().unwrap();
    assert_eq!(located["label"], format!("com.dagq.{hash}"));
    let home = dir.path().join("home");
    let with_home = ok(
        &repo,
        &[
            ("XDG_DATA_HOME", data_home.as_path()),
            ("HOME", home.as_path()),
        ],
        &["locate"],
    );
    assert_eq!(
        with_home["launch_agent"],
        home.join("Library/LaunchAgents")
            .join(format!("com.dagq.{hash}.plist"))
            .to_str()
            .unwrap()
    );
    assert!(!Path::new(with_home["launch_agent"].as_str().unwrap()).exists());
    assert_eq!(
        located["runs_dir"],
        db.parent().unwrap().join("runs").to_str().unwrap()
    );
    assert!(error(&repo, &env, &["list"]).contains("use init"));
    assert!(!data_home.exists());

    let init = ok(&repo, &env, &["init"]);
    assert_eq!(init["db"], db.to_str().unwrap());
    assert_eq!(init["source"], "repository");
    assert_eq!(init["git_common_dir"], common_dir.to_str().unwrap());
    assert!(db.is_file());
    assert_eq!(
        fs::read_to_string(db.with_file_name("repository")).unwrap(),
        format!("{}\n", common_dir.display())
    );
    assert_eq!(
        SqliteQueue::open(&db)
            .unwrap()
            .repository_binding()
            .unwrap()
            .as_deref(),
        common_dir.to_str()
    );
    let added = ok(&repo, &env, &["add", "shared task"]);
    assert_eq!(ok(&repo, &env, &["locate"])["db_exists"], true);

    // A subdirectory, a second worktree, and a run worktree under the queue's
    // own runs directory all resolve to the same queue.
    let nested = repo.join("src/deep");
    fs::create_dir_all(&nested).unwrap();
    let second = dir.path().join("second worktree");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            second.to_str().unwrap(),
        ],
    );
    let run_worktree = db
        .parent()
        .unwrap()
        .join("runs/00000000-0000-4000-8000-000000000000/worktree");
    fs::create_dir_all(run_worktree.parent().unwrap()).unwrap();
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "dagq/fake-run",
            run_worktree.to_str().unwrap(),
        ],
    );
    for cwd in [&nested, &second, &run_worktree] {
        assert_eq!(ok(cwd, &env, &["locate"])["db"], db.to_str().unwrap());
        let listed = ok(cwd, &env, &["list"]);
        assert_eq!(
            listed["tasks"].as_array().unwrap().len(),
            1,
            "{}",
            cwd.display()
        );
        assert_eq!(listed["tasks"][0]["id"], added["id"]);
    }
    ok(
        &second,
        &env,
        &["ready", &added["id"].to_string(), "--bypass-review"],
    );
    assert_eq!(
        ok(&run_worktree, &env, &["candidates"])[0]["id"],
        added["id"]
    );
    // Re-running init keeps the queue and its binding.
    assert_eq!(ok(&second, &env, &["init"])["db"], db.to_str().unwrap());
    assert_eq!(ok(&repo, &env, &["list"])["total"], 1);
}

#[test]
fn another_repository_gets_its_own_queue_and_outside_a_repository_fails() {
    let (dir, data_home, repo) = fixture();
    let env = [("XDG_DATA_HOME", data_home.as_path())];
    let other = repository(dir.path(), "other");
    let db = expected_db(&data_home, &repo);
    let other_db = expected_db(&data_home, &other);
    assert_ne!(db, other_db);
    assert_eq!(
        db.parent().unwrap().parent(),
        other_db.parent().unwrap().parent()
    );

    ok(&repo, &env, &["init"]);
    ok(&repo, &env, &["add", "only here"]);
    assert_eq!(
        ok(&other, &env, &["locate"])["db"],
        other_db.to_str().unwrap()
    );
    assert!(error(&other, &env, &["list"]).contains("use init"));
    assert!(!other_db.exists());
    ok(&other, &env, &["init"]);
    assert_eq!(ok(&other, &env, &["list"])["total"], 0);
    assert_eq!(ok(&repo, &env, &["list"])["total"], 1);

    let outside = dir.path().join("plain");
    fs::create_dir(&outside).unwrap();
    for args in [&["locate"][..], &["list"], &["init"]] {
        let message = error(&outside, &env, args);
        assert!(message.contains("pass --db"), "{message}");
        assert!(message.contains("not inside a Git repository"), "{message}");
    }
    assert!(!outside.join(".git").exists());
}

#[test]
fn db_flag_overrides_the_repository_queue_and_stays_unbound() {
    let (dir, data_home, repo) = fixture();
    let env = [("XDG_DATA_HOME", data_home.as_path())];
    let explicit = dir.path().join("elsewhere/nested/explicit.db");
    let flag = explicit.to_str().unwrap();

    let located = ok(&repo, &env, &["--db", flag, "locate"]);
    assert_eq!(located["db"], flag);
    assert_eq!(located["source"], "db_flag");
    assert_eq!(located["git_common_dir"], Value::Null);
    assert_eq!(
        located["runs_dir"],
        explicit.parent().unwrap().join("runs").to_str().unwrap()
    );
    // init creates the missing directories for an explicit path too.
    let init = ok(&repo, &env, &["--db", flag, "init"]);
    assert_eq!(init["db"], flag);
    assert_eq!(init["source"], "db_flag");
    assert_eq!(init["git_common_dir"], Value::Null);
    assert!(explicit.is_file());
    assert!(!explicit.with_file_name("repository").exists());
    assert!(!data_home.exists());
    // A --db queue is not bound until a supervisor claims it, so it can be used
    // from any directory, including outside a repository.
    let queue = SqliteQueue::open(&explicit).unwrap();
    assert_eq!(queue.repository_binding().unwrap(), None);
    ok(&repo, &env, &["--db", flag, "add", "explicit task"]);
    assert_eq!(ok(dir.path(), &env, &["--db", flag, "list"])["total"], 1);
    assert!(!expected_db(&data_home, &repo).exists());
}

#[test]
fn data_home_falls_back_to_home_when_xdg_data_home_is_unset_or_relative() {
    let (dir, data_home, repo) = fixture();
    let home = dir.path().join("home");
    let fallback = home.join(".local/share");
    let db = expected_db(&fallback, &repo);

    let env = [("HOME", home.as_path())];
    assert_eq!(ok(&repo, &env, &["locate"])["db"], db.to_str().unwrap());
    let relative = [
        ("HOME", home.as_path()),
        ("XDG_DATA_HOME", Path::new("relative/data")),
    ];
    assert_eq!(
        ok(&repo, &relative, &["locate"])["db"],
        db.to_str().unwrap()
    );
    let empty = [("HOME", home.as_path()), ("XDG_DATA_HOME", Path::new(""))];
    assert_eq!(ok(&repo, &empty, &["locate"])["db"], db.to_str().unwrap());
    let absolute = [
        ("HOME", home.as_path()),
        ("XDG_DATA_HOME", data_home.as_path()),
    ];
    assert_eq!(
        ok(&repo, &absolute, &["locate"])["db"],
        expected_db(&data_home, &repo).to_str().unwrap()
    );
    assert!(!repo.join("relative").exists());

    ok(&repo, &env, &["init"]);
    assert!(db.is_file());
    let message = error(&repo, &[], &["locate"]);
    assert!(message.contains("XDG_DATA_HOME and HOME"), "{message}");
}

#[test]
fn a_repository_queue_bound_elsewhere_is_refused_by_every_command() {
    let (_dir, data_home, repo) = fixture();
    let env = [("XDG_DATA_HOME", data_home.as_path())];
    // Plant a queue where this repository resolves to, but bound to another
    // repository, as a hash collision or a copied data directory would.
    let db = expected_db(&data_home, &repo);
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    SqliteQueue::init(&db)
        .unwrap()
        .bind_repository("/somewhere/else/.git")
        .unwrap();

    for args in [
        &["list"][..],
        &["add", "x"],
        &["status"],
        &["doctor"],
        &["init"],
    ] {
        let message = error(&repo, &env, args);
        assert!(
            message.contains("bound to another Git repository: /somewhere/else/.git"),
            "{args:?}: {message}"
        );
    }
    assert_eq!(
        SqliteQueue::open(&db)
            .unwrap()
            .repository_binding()
            .unwrap()
            .as_deref(),
        Some("/somewhere/else/.git")
    );
    // The same file is still usable through --db, which performs no binding check
    // until a supervisor claims it.
    assert_eq!(
        ok(&repo, &env, &["--db", db.to_str().unwrap(), "list"])["total"],
        0
    );
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.bind_repository("/somewhere/else/.git").is_ok());
    assert!(queue.bind_repository("/third/.git").is_err());
    assert!(queue.assert_repository("/somewhere/else/.git").is_ok());
    assert!(queue.assert_repository("/third/.git").is_err());
    drop(queue);

    // `doctor` reports the schema of a queue whose floor refuses this binary
    // (ADR-0045 decision 5), and still checks the binding first.
    let raw = rusqlite::Connection::open(&db).unwrap();
    raw.execute_batch(&format!(
        "UPDATE schema_floor SET floor = {0}; PRAGMA user_version = {0};",
        SqliteQueue::SCHEMA_VERSION + 1
    ))
    .unwrap();
    let message = error(&repo, &env, &["doctor"]);
    assert!(
        message.contains("bound to another Git repository: /somewhere/else/.git"),
        "{message}"
    );
    raw.execute("DELETE FROM queue_repository", []).unwrap();
    let doctor = ok(&repo, &env, &["doctor"]);
    assert_eq!(doctor["schema"]["floor"], SqliteQueue::SCHEMA_VERSION + 1);
    assert!(
        doctor["error"]
            .as_str()
            .unwrap()
            .contains("install a newer dagq")
    );
}

/// A moved repository resolves to a new queue directory. `rebind --db` on
/// the old queue binds it to the new common directory and names where to
/// move it (`move_to`); after the move every command works from the new
/// checkout. `init` never rebinds (ADR-0020).
#[test]
fn rebind_with_db_then_move_the_queue_directory() {
    let (dir, data_home, repo) = fixture();
    let env = [("XDG_DATA_HOME", data_home.as_path())];
    ok(&repo, &env, &["init"]);
    ok(&repo, &env, &["add", "kept"]);
    let old_db = expected_db(&data_home, &repo);
    let old_common_dir = repo.join(".git").canonicalize().unwrap();
    let moved = dir.path().join("renamed");
    fs::rename(&repo, &moved).unwrap();
    let new_db = expected_db(&data_home, &moved);
    let new_common_dir = moved.join(".git").canonicalize().unwrap();
    assert_ne!(old_db, new_db);

    // The new checkout resolves to a queue that does not exist yet.
    assert!(error(&moved, &env, &["list"]).contains("use init"));
    assert!(error(&moved, &env, &["rebind"]).contains("initialized"));

    let old = old_db.to_str().unwrap();
    let rebound = ok(&moved, &env, &["--db", old, "rebind"]);
    assert_eq!(rebound["outcome"], "rebound");
    assert_eq!(
        rebound["previous_git_common_dir"],
        old_common_dir.to_str().unwrap()
    );
    assert_eq!(rebound["git_common_dir"], new_common_dir.to_str().unwrap());
    assert_eq!(
        rebound["move_to"],
        new_db.parent().unwrap().to_str().unwrap()
    );
    assert_eq!(
        fs::read_to_string(old_db.with_file_name("repository")).unwrap(),
        format!("{}\n", new_common_dir.display())
    );

    fs::rename(old_db.parent().unwrap(), new_db.parent().unwrap()).unwrap();
    assert_eq!(ok(&moved, &env, &["list"])["total"], 1);
    ok(&moved, &env, &["status"]);
    assert_eq!(
        ok(&moved, &env, &["init"])["git_common_dir"],
        new_common_dir.to_str().unwrap()
    );
    let again = ok(&moved, &env, &["rebind"]);
    assert_eq!(again["outcome"], "unchanged");
    assert_eq!(again["move_to"], Value::Null);
}

/// The other order: move the queue directory first, then `rebind` without
/// `--db`. Until then every command, `init` included, refuses the queue;
/// a live supervisor refuses the rebind itself.
#[test]
fn move_the_queue_directory_then_rebind() {
    let (dir, data_home, repo) = fixture();
    let env = [("XDG_DATA_HOME", data_home.as_path())];
    ok(&repo, &env, &["init"]);
    ok(&repo, &env, &["add", "kept"]);
    let old_db = expected_db(&data_home, &repo);
    let old_common_dir = repo.join(".git").canonicalize().unwrap();
    let moved = dir.path().join("renamed");
    fs::rename(&repo, &moved).unwrap();
    let new_db = expected_db(&data_home, &moved);
    fs::rename(old_db.parent().unwrap(), new_db.parent().unwrap()).unwrap();

    for args in [&["list"][..], &["status"], &["init"], &["add", "x"]] {
        let message = error(&moved, &env, args);
        assert!(
            message.contains("bound to another Git repository"),
            "{args:?}: {message}"
        );
    }

    let supervisor = SqliteQueue::open(&new_db)
        .unwrap()
        .register_supervisor("live", std::process::id(), 1, "0.0.1")
        .unwrap();
    let message = error(&moved, &env, &["rebind"]);
    assert!(message.contains("supervisor is running"), "{message}");
    assert_eq!(
        SqliteQueue::open(&new_db)
            .unwrap()
            .repository_binding()
            .unwrap()
            .as_deref(),
        old_common_dir.to_str()
    );
    SqliteQueue::open(&new_db)
        .unwrap()
        .deregister_supervisor(&supervisor.token)
        .unwrap();

    let rebound = ok(&moved, &env, &["rebind"]);
    assert_eq!(rebound["outcome"], "rebound");
    assert_eq!(rebound["move_to"], Value::Null);
    assert_eq!(rebound["worktrees"], serde_json::json!([]));
    assert_eq!(ok(&moved, &env, &["list"])["total"], 1);
    ok(&moved, &env, &["doctor"]);
}

/// A `--db` queue that was never supervised has no binding; `rebind --repo`
/// from outside any repository binds it, and the log records no previous one.
#[test]
fn rebind_binds_an_unbound_db_queue_to_the_repo_flag() {
    let (dir, data_home, repo) = fixture();
    let env = [("XDG_DATA_HOME", data_home.as_path())];
    let db = dir.path().join("queues").join("q.db");
    let db_text = db.to_str().unwrap();
    ok(dir.path(), &env, &["--db", db_text, "init"]);
    let rebound = ok(
        dir.path(),
        &env,
        &["--db", db_text, "rebind", "--repo", repo.to_str().unwrap()],
    );
    let common_dir = repo.join(".git").canonicalize().unwrap();
    assert_eq!(rebound["outcome"], "rebound");
    assert_eq!(rebound["previous_git_common_dir"], Value::Null);
    assert_eq!(rebound["git_common_dir"], common_dir.to_str().unwrap());
    assert_eq!(
        rebound["move_to"],
        expected_db(&data_home, &repo)
            .parent()
            .unwrap()
            .to_str()
            .unwrap()
    );
    let log = fs::read_to_string(db.with_file_name("logs").join("rebind.jsonl")).unwrap();
    let entry: Value = serde_json::from_str(log.trim()).unwrap();
    assert_eq!(entry["previous_git_common_dir"], Value::Null);
    // No `repository` pointer is created for a `--db` queue.
    assert!(!db.with_file_name("repository").exists());
}
