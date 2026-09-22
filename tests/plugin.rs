//! The Claude Code plugin in `plugins/claude-taskq` is data plus one launcher
//! script. These tests catch a broken manifest, skill frontmatter, or launcher
//! before `claude plugin validate` or a real session would.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;

fn plugin_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/claude-taskq")
}

fn skill_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(plugin_root().join("skills"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Frontmatter must start on the first line and close with `---`.
fn frontmatter(skill: &str) -> Vec<(String, String)> {
    let mut lines = skill.lines();
    assert_eq!(lines.next(), Some("---"), "frontmatter must open on line 1");
    let mut fields = Vec::new();
    for line in lines {
        if line == "---" {
            return fields;
        }
        let (key, value) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("frontmatter line without key: {line:?}"));
        fields.push((key.trim().to_string(), value.trim().to_string()));
    }
    panic!("frontmatter never closed");
}

#[test]
fn manifest_names_the_plugin_and_tracks_the_crate_version() {
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(plugin_root().join(".claude-plugin/plugin.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["name"], "claude-taskq");
    assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        manifest["description"]
            .as_str()
            .unwrap()
            .contains("cmux-taskq")
    );
    // Component paths are optional; if given they must stay inside the plugin.
    for key in ["skills", "commands", "agents", "hooks"] {
        if let Some(path) = manifest[key].as_str() {
            assert!(
                path.starts_with("./") && !path.contains(".."),
                "{key}: {path}"
            );
        }
    }
}

#[test]
fn every_skill_has_valid_frontmatter_and_uses_the_launcher() {
    let dirs = skill_dirs();
    let names: Vec<String> = dirs
        .iter()
        .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["taskq", "taskq-maintain", "taskq-recover"]);
    for dir in &dirs {
        let skill = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        let fields = frontmatter(&skill);
        let get = |key: &str| {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        let name = get("name").expect("name");
        assert_eq!(name, dir.file_name().unwrap().to_string_lossy());
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{name} must be kebab-case"
        );
        let description = get("description").expect("description");
        assert!(
            (40..=1024).contains(&description.len()),
            "{name}: description length {}",
            description.len()
        );
        assert!(
            skill.contains("${CLAUDE_PLUGIN_ROOT}/bin/taskq"),
            "{name} must call the launcher"
        );
        assert!(
            !skill.contains("sqlite3 "),
            "{name} must not open the database directly"
        );
    }
}

/// `XDG_DATA_HOME` is always pointed away from the developer's real queues.
fn launcher(env: &[(&str, &str)], data_home: &Path, cwd: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(plugin_root().join("bin/taskq"));
    command
        .env_remove("CMUX_TASKQ_BIN")
        .env_remove("CMUX_TASKQ_DB")
        .env("XDG_DATA_HOME", data_home)
        .env("PATH", "/usr/bin:/bin")
        .current_dir(cwd)
        .args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn stdout_json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn launcher_resolves_the_binary_and_the_repository_queue_under_the_data_home() {
    let dir = tempfile::tempdir().unwrap();
    let data_home = dir.path().join("xdg");
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    let binary = env!("CARGO_BIN_EXE_cmux-taskq");
    let env = [("CMUX_TASKQ_BIN", binary)];
    let git_dir = repo.join(".git").canonicalize().unwrap();
    let hash = cmux_taskq::infrastructure::location::repository_hash(&git_dir);
    let expected_db = data_home.join("cmux-taskq").join(&hash).join("queue.db");

    let resolved = stdout_json(&launcher(&env, &data_home, &repo, &["--resolve"]));
    assert_eq!(resolved["binary"], binary);
    assert_eq!(
        resolved["version"],
        format!("cmux-taskq {}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(resolved["db"], expected_db.to_str().unwrap());
    assert_eq!(resolved["db_exists"], false);
    assert_eq!(resolved["source"], "repository");
    assert_eq!(resolved["git_common_dir"], git_dir.to_str().unwrap());
    assert_eq!(
        resolved["runs_dir"],
        expected_db.with_file_name("runs").to_str().unwrap()
    );
    assert_eq!(
        resolved["repo"],
        repo.canonicalize().unwrap().to_str().unwrap()
    );

    // `init` creates the missing directory; other commands do not.
    let listing = launcher(&env, &data_home, &repo, &["list"]);
    assert!(!listing.status.success());
    assert!(!data_home.exists());
    let init = stdout_json(&launcher(&env, &data_home, &repo, &["init"]));
    assert_eq!(init["db"], expected_db.to_str().unwrap());
    let added = stdout_json(&launcher(
        &env,
        &data_home,
        &repo,
        &[
            "add",
            "plugin smoke",
            "--acceptance",
            "shown",
            "--verify",
            "true",
        ],
    ));
    assert_eq!(added["status"], "draft");
    let id = added["id"].to_string();
    stdout_json(&launcher(&env, &data_home, &repo, &["ready", &id]));
    let shown = stdout_json(&launcher(&env, &data_home, &repo, &["show", &id]));
    assert_eq!(shown["task"]["status"], "ready");
    assert_eq!(shown["task"]["verification_commands"][0], "true");
    assert_eq!(
        stdout_json(&launcher(&env, &data_home, &repo, &["--resolve"]))["db_exists"],
        true
    );
    // A worktree of the same repository shares the queue.
    let worktree = dir.path().join("wt");
    assert!(
        Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["commit", "-q", "--allow-empty", "-m", "init"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["worktree", "add", "-q", "--detach"])
            .arg(&worktree)
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        stdout_json(&launcher(&env, &data_home, &worktree, &["candidates"]))[0]["id"],
        added["id"]
    );
    // An explicit database path wins over the convention, for --resolve too.
    let other = dir.path().join("other.db");
    let env_db = [
        ("CMUX_TASKQ_BIN", binary),
        ("CMUX_TASKQ_DB", other.to_str().unwrap()),
    ];
    let init = stdout_json(&launcher(&env_db, &data_home, &repo, &["init"]));
    assert_eq!(init["db"], other.to_str().unwrap());
    let resolved = stdout_json(&launcher(&env_db, &data_home, dir.path(), &["--resolve"]));
    assert_eq!(resolved["db"], other.to_str().unwrap());
    assert_eq!(resolved["source"], "db_flag");
    assert_eq!(resolved["repo"], "");
    // Pass-through flags need no database.
    let version = launcher(&env, &data_home, dir.path(), &["--version"]);
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("cmux-taskq "));
}

#[test]
fn launcher_reports_missing_binary_and_repository_as_json_errors() {
    let dir = tempfile::tempdir().unwrap();
    let data_home = dir.path().join("xdg");
    let missing = launcher(&[], &data_home, dir.path(), &["--resolve"]);
    assert!(!missing.status.success());
    let error: Value = serde_json::from_slice(&missing.stderr).unwrap();
    let message = error["error"].as_str().unwrap();
    assert!(message.contains("cargo build --locked"), "{message}");
    assert!(message.contains("CMUX_TASKQ_BIN"), "{message}");

    let bogus = dir.path().join("not-executable");
    fs::write(&bogus, "").unwrap();
    let bad = launcher(
        &[("CMUX_TASKQ_BIN", bogus.to_str().unwrap())],
        &data_home,
        dir.path(),
        &["list"],
    );
    assert!(!bad.status.success());
    let error: Value = serde_json::from_slice(&bad.stderr).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("not an executable")
    );

    // Outside a repository the binary itself explains how to point at a queue.
    let outside = launcher(
        &[("CMUX_TASKQ_BIN", env!("CARGO_BIN_EXE_cmux-taskq"))],
        &data_home,
        dir.path(),
        &["list"],
    );
    assert!(!outside.status.success());
    let error: Value = serde_json::from_slice(&outside.stderr).unwrap();
    let message = error["error"].as_str().unwrap();
    assert!(message.contains("--db"), "{message}");
    assert!(message.contains("not inside a Git repository"), "{message}");
    assert!(!data_home.exists());
}
