//! The Claude Code plugin in `plugins/claude-dagq` is data plus one launcher
//! script. These tests catch a broken manifest, skill frontmatter, or launcher
//! before `claude plugin validate` or a real session would.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;

fn repository_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn plugin_root() -> PathBuf {
    repository_root().join("plugins/claude-dagq")
}

fn plugin_manifest() -> Value {
    serde_json::from_str(
        &fs::read_to_string(plugin_root().join(".claude-plugin/plugin.json")).unwrap(),
    )
    .unwrap()
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
    let manifest = plugin_manifest();
    assert_eq!(manifest["name"], "claude-dagq");
    assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
    assert!(manifest["description"].as_str().unwrap().contains("dagq"));
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
    assert_eq!(names, ["dagq", "dagq-maintain", "dagq-recover"]);
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
            skill.contains("${CLAUDE_PLUGIN_ROOT}/bin/dagq"),
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
    let mut command = Command::new(plugin_root().join("bin/dagq"));
    command
        .env_remove("DAGQ_BIN")
        .env_remove("DAGQ_DB")
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
    let binary = env!("CARGO_BIN_EXE_dagq");
    let env = [("DAGQ_BIN", binary)];
    let git_dir = repo.join(".git").canonicalize().unwrap();
    let hash = dagq::infrastructure::location::repository_hash(&git_dir);
    let expected_db = data_home.join("dagq").join(&hash).join("queue.db");

    let resolved = stdout_json(&launcher(&env, &data_home, &repo, &["--resolve"]));
    assert_eq!(resolved["binary"], binary);
    assert_eq!(resolved["binary_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(resolved["plugin_version"], plugin_manifest()["version"]);
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
    let env_db = [("DAGQ_BIN", binary), ("DAGQ_DB", other.to_str().unwrap())];
    let init = stdout_json(&launcher(&env_db, &data_home, &repo, &["init"]));
    assert_eq!(init["db"], other.to_str().unwrap());
    let resolved = stdout_json(&launcher(&env_db, &data_home, dir.path(), &["--resolve"]));
    assert_eq!(resolved["db"], other.to_str().unwrap());
    assert_eq!(resolved["source"], "db_flag");
    assert_eq!(resolved["repo"], "");
    // Pass-through flags need no database.
    let version = launcher(&env, &data_home, dir.path(), &["--version"]);
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("dagq "));
}

#[test]
fn launcher_reports_missing_binary_and_repository_as_json_errors() {
    let dir = tempfile::tempdir().unwrap();
    let data_home = dir.path().join("xdg");
    let missing = launcher(&[], &data_home, dir.path(), &["--resolve"]);
    assert!(!missing.status.success());
    let error: Value = serde_json::from_slice(&missing.stderr).unwrap();
    let message = error["error"].as_str().unwrap();
    assert!(
        message.contains("https://github.com/hisamekms/dagq/releases"),
        "{message}"
    );
    assert!(
        message.contains(&format!(
            "dagq-v{}-aarch64-apple-darwin.tar.gz",
            plugin_manifest()["version"].as_str().unwrap()
        )),
        "{message}"
    );
    assert!(message.contains("SHA256SUMS"), "{message}");
    assert!(message.contains("~/.local/bin"), "{message}");
    assert!(message.contains("cargo build --locked"), "{message}");
    assert!(message.contains("DAGQ_BIN"), "{message}");

    let bogus = dir.path().join("not-executable");
    fs::write(&bogus, "").unwrap();
    let bad = launcher(
        &[("DAGQ_BIN", bogus.to_str().unwrap())],
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
        &[("DAGQ_BIN", env!("CARGO_BIN_EXE_dagq"))],
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

#[test]
fn the_marketplace_offers_this_repository_s_plugin_from_its_own_path() {
    let marketplace: Value = serde_json::from_str(
        &fs::read_to_string(repository_root().join(".claude-plugin/marketplace.json")).unwrap(),
    )
    .unwrap();
    // `claude plugin marketplace add hisamekms/dagq` reads this file, and
    // `claude plugin install claude-dagq@dagq` the entry below.
    assert_eq!(marketplace["name"], "dagq");
    assert_eq!(marketplace["owner"]["name"], "hisamekms");
    let plugins = marketplace["plugins"].as_array().expect("plugins");
    assert_eq!(plugins.len(), 1);
    let entry = &plugins[0];
    assert_eq!(entry["name"], plugin_manifest()["name"]);
    let source = entry["source"].as_str().expect("a path source");
    assert_eq!(source, "./plugins/claude-dagq");
    assert_eq!(
        repository_root().join(source.trim_start_matches("./")),
        plugin_root()
    );
    assert!(plugin_root().join(".claude-plugin/plugin.json").is_file());
}

/// A stub that answers only `--version` and `locate`, which is all `--resolve`
/// asks of the binary. It lets the version comparison be tested without
/// building a second dagq.
fn fake_binary(dir: &Path, name: &str, version: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(
        &path,
        format!(
            "#!/bin/sh\ncase \"$1\" in\n\
             --version) echo 'dagq {version}' ;;\n\
             locate) printf '{{\\n  \"db\": \"/fake/queue.db\"\\n}}\\n' ;;\n\
             esac\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn launcher_warns_only_when_plugin_and_binary_differ_in_major_minor() {
    let dir = tempfile::tempdir().unwrap();
    let data_home = dir.path().join("xdg");
    let plugin_version = plugin_manifest()["version"].as_str().unwrap().to_string();
    let mut parts = plugin_version.split('.');
    let major: u64 = parts.next().unwrap().parse().unwrap();
    let minor: u64 = parts.next().unwrap().parse().unwrap();

    // Same major.minor, different patch: a resolution with nothing on stderr.
    let same = format!("{major}.{minor}.99");
    let binary = fake_binary(dir.path(), "same", &same);
    let output = launcher(
        &[("DAGQ_BIN", binary.to_str().unwrap())],
        &data_home,
        dir.path(),
        &["--resolve"],
    );
    let resolved = stdout_json(&output);
    assert_eq!(resolved["plugin_version"], plugin_version);
    assert_eq!(resolved["binary_version"], same);
    assert_eq!(resolved["db"], "/fake/queue.db");
    assert_eq!(
        output.stderr,
        b"",
        "matching versions must not warn: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // One minor apart: the same resolution on stdout plus a warning, exit 0.
    let other = format!("{major}.{}.0", minor + 1);
    let binary = fake_binary(dir.path(), "other", &other);
    let output = launcher(
        &[("DAGQ_BIN", binary.to_str().unwrap())],
        &data_home,
        dir.path(),
        &["--resolve"],
    );
    let resolved = stdout_json(&output);
    assert_eq!(resolved["binary_version"], other);
    let warning: Value = serde_json::from_slice(&output.stderr).unwrap();
    let message = warning["warning"].as_str().expect("warning");
    assert!(message.contains(&plugin_version), "{message}");
    assert!(message.contains(&other), "{message}");
    assert!(message.contains("claude plugin update"), "{message}");
    assert!(
        message.contains("https://github.com/hisamekms/dagq/releases"),
        "{message}"
    );
}
