//! Queue resolution from the working directory: one queue per repository under
//! the user data directory, shared by all of its worktrees.
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
        .output()
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
    command.output().unwrap()
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
        assert_eq!(listed.as_array().unwrap().len(), 1, "{}", cwd.display());
        assert_eq!(listed[0]["id"], added["id"]);
    }
    ok(&second, &env, &["ready", &added["id"].to_string()]);
    assert_eq!(
        ok(&run_worktree, &env, &["candidates"])[0]["id"],
        added["id"]
    );
    // Re-running init keeps the queue and its binding.
    assert_eq!(ok(&second, &env, &["init"])["db"], db.to_str().unwrap());
    assert_eq!(ok(&repo, &env, &["list"]).as_array().unwrap().len(), 1);
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
    assert_eq!(ok(&other, &env, &["list"]), serde_json::json!([]));
    assert_eq!(ok(&repo, &env, &["list"]).as_array().unwrap().len(), 1);

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
    assert_eq!(
        ok(dir.path(), &env, &["--db", flag, "list"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
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
        ok(&repo, &env, &["--db", db.to_str().unwrap(), "list"]),
        serde_json::json!([])
    );
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.bind_repository("/somewhere/else/.git").is_ok());
    assert!(queue.bind_repository("/third/.git").is_err());
    assert!(queue.assert_repository("/somewhere/else/.git").is_ok());
    assert!(queue.assert_repository("/third/.git").is_err());
}
