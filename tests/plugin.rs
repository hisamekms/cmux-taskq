//! The Claude Code plugin in `plugins/claude-dagq` is data plus one launcher
//! script and one hook script. These tests catch a broken manifest, skill
//! frontmatter, hook, or launcher before `claude plugin validate` or a real
//! session would.

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
    assert_eq!(
        names,
        ["dagq", "dagq-inbox", "dagq-planner", "dagq-recover"]
    );
    for dir in &dirs {
        let skill = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        // A skill is reloaded on every use and after each compaction, so its
        // body stays small; lists of fields and states live in reference/.
        assert!(
            skill.len() <= 8 * 1024,
            "{}: SKILL.md is {} bytes",
            dir.display(),
            skill.len()
        );
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

/// Every `reference/<file>.md` a skill names exists (in its own directory,
/// or in the skill a `skills/<name>/reference/` path names), and every file
/// under a skill's `reference/` is named by its SKILL.md, so none is
/// unreachable.
#[test]
fn skills_point_at_their_reference_files() {
    let mut with_reference = Vec::new();
    for dir in skill_dirs() {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let skill = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        let reference = dir.join("reference");
        if reference.is_dir() {
            with_reference.push(name.clone());
            for entry in fs::read_dir(&reference).unwrap() {
                let file = entry.unwrap().file_name().to_string_lossy().into_owned();
                assert!(file.ends_with(".md"), "{name}: {file}");
                assert!(skill.contains(&file), "{name} never names reference/{file}");
            }
        }
        for (index, _) in skill.match_indices("reference/") {
            let file: String = skill[index + "reference/".len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
                .collect();
            let file = file.trim_end_matches('.');
            let owner = skill[..index]
                .strip_suffix('/')
                .and_then(|before| before.rsplit_once("skills/"))
                .map(|(_, other)| other)
                .filter(|other| !other.contains('/'))
                .map_or(reference.clone(), |other| {
                    plugin_root().join("skills").join(other).join("reference")
                });
            assert!(
                owner.join(file).is_file(),
                "{name}: {}",
                owner.join(file).display()
            );
        }
    }
    with_reference.sort();
    assert_eq!(with_reference, ["dagq", "dagq-inbox", "dagq-recover"]);
    // The watch loop never lands a run on its own.
    let inbox = fs::read_to_string(plugin_root().join("skills/dagq-inbox/SKILL.md")).unwrap();
    assert!(inbox.contains("\"$DAGQ\" watch --role inbox --after <cursor>"));
    assert!(inbox.contains("run_in_background"));
    assert!(!inbox.contains("\"$DAGQ\" integrate"));
    let review =
        fs::read_to_string(plugin_root().join("skills/dagq-recover/reference/review-by-hand.md"))
            .unwrap();
    assert!(review.contains("\"$DAGQ\" review ID"));
    assert!(review.contains("On `pass` and their word, `\"$DAGQ\" integrate ID`"));
    assert!(review.contains("--option land --option send_back --option cancel"));
    assert!(review.contains("git push origin main"));
}

/// ADR-0024: the roles are the supervisor, the worker, the planner, the
/// inbox and the observer. Every attention reaches the person through the
/// inbox, which decides nothing and acts only on the person's word through
/// `dagq-recover`; registering and closing goals and tasks is the planner's.
#[test]
fn skills_split_the_roles_of_inbox_planner_and_recover() {
    let read = |name: &str| {
        fs::read_to_string(plugin_root().join(format!("skills/{name}/SKILL.md"))).unwrap()
    };
    for name in ["dagq-inbox", "dagq-recover"] {
        let skill = read(name);
        for forbidden in [
            "\"$DAGQ\" goal close",
            "\"$DAGQ\" goal add",
            "\"$DAGQ\" add ",
        ] {
            assert!(!skill.contains(forbidden), "{name} mentions {forbidden}");
        }
    }
    for dir in skill_dirs() {
        let skill = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert!(
            !skill.to_lowercase().contains("maintain"),
            "{}",
            dir.display()
        );
    }

    let inbox = read("dagq-inbox");
    assert!(inbox.contains("\"$DAGQ\" status --role inbox"));
    assert!(inbox.contains("\"$DAGQ\" asks --open --role inbox"));
    assert!(inbox.contains("\"$DAGQ\" answer <id> --text"));
    assert!(inbox.contains("Add no recommendation of your own"));
    assert!(inbox.contains("Only when the person says so"));
    for next in [
        "`read the answer of ask <id> and close it`",
        "`restart supervisor`",
        "`review by hand`",
        "`push main`",
        "`triage by hand`",
        "`send the answer of ask <id> to the worker and close it`",
    ] {
        assert!(inbox.contains(next), "dagq-inbox lacks {next}");
    }
    assert!(inbox.contains("skills/dagq-recover/reference/session.md"));
    assert!(inbox.contains("reference/status.md"));

    let recover = read("dagq-recover");
    for section in [
        "## 3. Recover (only without a supervisor)",
        "## 4. Triage by hand",
        "## 5. Start, stop and update the runtime",
        "## 6. Review by hand, and a failed push",
        "## 7. A run's session",
    ] {
        assert!(recover.contains(section), "dagq-recover lacks {section}");
    }
    assert!(recover.contains("\"$DAGQ\" up --plugin-dir \"$CLAUDE_PLUGIN_ROOT\""));
    assert!(recover.contains("\"$DAGQ\" down --wait"));
    let stuck_exit =
        fs::read_to_string(plugin_root().join("skills/dagq-recover/reference/stuck-exit.md"))
            .unwrap();
    for step in [
        "\"Background work is running\"",
        "git -C <worktree_path> status --porcelain",
        "`jq -r .commit <receipt_path>` equals `git -C <worktree_path> rev-parse HEAD`",
        "select \"Exit and stop tasks\"",
        "## Answer `wait`, or anything else",
        "\"$DAGQ\" asks --role inbox",
    ] {
        assert!(stuck_exit.contains(step), "stuck-exit.md lacks {step}");
    }

    let planner = read("dagq-planner");
    assert!(planner.contains("skills/dagq/SKILL.md"));
    assert!(planner.contains("skills/dagq/reference/goal-close.md"));
    assert!(planner.contains("skills/dagq/reference/observer.md"));
    assert!(planner.contains("skills/dagq-recover/SKILL.md"));
    assert!(planner.contains("follow_ups"));
    assert!(planner.contains("\"$DAGQ\" goal close ID --verdict achieved"));
    assert!(planner.contains("Never: `integrate`, `review`, `answer`"));
    let dagq = read("dagq");
    assert!(dagq.contains("A goal is closed once, by the planner"));
}

/// The runtime resumes `needs_session` runs (ADR-0019): no skill or
/// reference file tells a session to open a resume session itself.
#[test]
fn no_skill_opens_a_resume_session() {
    for dir in skill_dirs() {
        let mut files = vec![dir.join("SKILL.md")];
        if let Ok(entries) = fs::read_dir(dir.join("reference")) {
            files.extend(entries.map(|entry| entry.unwrap().path()));
        }
        for file in files {
            let text = fs::read_to_string(&file).unwrap();
            for forbidden in ["claude --resume", "workspace create"] {
                assert!(
                    !text.contains(forbidden),
                    "{} mentions {forbidden}",
                    file.display()
                );
            }
        }
    }
    let recover = fs::read_to_string(plugin_root().join("skills/dagq-recover/SKILL.md")).unwrap();
    assert!(recover.contains("a `needs_session` run is resumed (`resuming (runtime)`)"));
    assert!(recover.contains("never open a resume workspace yourself"));
}

fn hooks_manifest() -> Value {
    serde_json::from_str(&fs::read_to_string(plugin_root().join("hooks/hooks.json")).unwrap())
        .unwrap()
}

#[test]
fn hooks_json_runs_the_session_start_script_on_compact_and_clear_only() {
    let hooks = hooks_manifest();
    let events = hooks["hooks"].as_object().expect("hooks object");
    assert_eq!(events.keys().collect::<Vec<_>>(), ["SessionStart"]);
    let groups = events["SessionStart"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    // startup is the session prompt's job; resume keeps its context.
    let matcher = groups[0]["matcher"].as_str().unwrap();
    let mut sources: Vec<&str> = matcher.split('|').collect();
    sources.sort();
    assert_eq!(sources, ["clear", "compact"]);
    let commands = groups[0]["hooks"].as_array().unwrap();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["type"], "command");
    assert_eq!(
        commands[0]["command"],
        "${CLAUDE_PLUGIN_ROOT}/hooks/session-start.sh"
    );
    let script = plugin_root().join("hooks/session-start.sh");
    let mode = fs::metadata(&script).unwrap().permissions().mode();
    assert_ne!(mode & 0o111, 0, "session-start.sh must be executable");
}

/// Runs the SessionStart hook with a clean environment plus `env`.
fn session_start(env: &[(&str, &str)], data_home: &Path, cwd: &Path) -> Output {
    let mut command = Command::new(plugin_root().join("hooks/session-start.sh"));
    command
        .env_clear()
        .env("XDG_DATA_HOME", data_home)
        .env("PATH", "/usr/bin:/bin")
        .current_dir(cwd);
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
}

/// Splits the hook's stdout for `role` into its leading line of text, which
/// keeps Claude Code from reading the output as the hook's control JSON, and
/// the status JSON after it.
fn hook_status(output: &Output, role: &str) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stderr, b"");
    let text = String::from_utf8(output.stdout.clone()).unwrap();
    let (header, status) = text.split_once('\n').expect("a header line");
    assert!(
        serde_json::from_str::<Value>(&text).is_err(),
        "the whole stdout must not parse as JSON: {text}"
    );
    assert_eq!(
        header,
        format!(
            "This session is the dagq {role} (DAGQ_ROLE={role}); follow the dagq-{role} skill of the dagq plugin. The queue status for this role (dagq status --role {role}):"
        )
    );
    serde_json::from_str(status).unwrap()
}

#[test]
fn session_start_hook_prints_status_only_in_the_sessions_up_opens() {
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
    stdout_json(&launcher(
        &[("DAGQ_BIN", binary)],
        &data_home,
        &repo,
        &["init"],
    ));

    // No role, or another role: nothing at all, whatever else is set.
    for env in [
        vec![("DAGQ_BIN", binary)],
        vec![("DAGQ_BIN", binary), ("DAGQ_ROLE", "worker")],
        vec![("DAGQ_BIN", binary), ("DAGQ_ROLE", "observer")],
        vec![("DAGQ_BIN", binary), ("DAGQ_ROLE", "supervisor")],
        vec![("DAGQ_BIN", binary), ("DAGQ_ROLE", "inboxes")],
        vec![("DAGQ_ROLE", "")],
    ] {
        let output = session_start(&env, &data_home, &repo);
        assert!(output.status.success(), "{env:?}");
        assert_eq!(output.stdout, b"", "{env:?}");
        assert_eq!(output.stderr, b"", "{env:?}");
    }

    // The inbox gets status, with all the attention and the cursor.
    let inbox_env = [("DAGQ_BIN", binary), ("DAGQ_ROLE", "inbox")];
    let status = hook_status(&session_start(&inbox_env, &data_home, &repo), "inbox");
    assert!(status["supervisors"].is_array());
    assert!(status["attention"].is_array());
    assert_eq!(status["attention"][0]["kind"], "supervisor_stopped");
    assert!(status["cursor"].is_number());

    // The inbox and the planner get the status of their role: every
    // attention is the inbox's, none the planner's.
    stdout_json(&launcher(
        &[("DAGQ_BIN", binary), ("PATH", "/usr/bin:/bin")],
        &data_home,
        &repo,
        &[
            "ask",
            "--kind",
            "blocked",
            "--question",
            "stuck?",
            "--cmux",
            "/usr/bin/true",
        ],
    ));
    let inbox = hook_status(
        &session_start(
            &[("DAGQ_BIN", binary), ("DAGQ_ROLE", "inbox")],
            &data_home,
            &repo,
        ),
        "inbox",
    );
    let kinds = |status: &Value| -> Vec<String> {
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["kind"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(kinds(&inbox), ["supervisor_stopped", "ask_opened"]);
    assert_eq!(inbox["asks"][0]["question"], "stuck?");
    assert!(inbox["cursor"].is_number());
    let planner = hook_status(
        &session_start(
            &[("DAGQ_BIN", binary), ("DAGQ_ROLE", "planner")],
            &data_home,
            &repo,
        ),
        "planner",
    );
    assert!(kinds(&planner).is_empty(), "{planner}");
    assert!(planner["cursor"].is_number());

    // `up` names the queue in DAGQ_QUEUE, which works outside the repository.
    let db = stdout_json(&launcher(
        &[("DAGQ_BIN", binary)],
        &data_home,
        &repo,
        &["locate"],
    ))["db"]
        .as_str()
        .unwrap()
        .to_string();
    let with_queue = [
        ("DAGQ_BIN", binary),
        ("DAGQ_ROLE", "planner"),
        ("DAGQ_QUEUE", db.as_str()),
    ];
    let status = hook_status(
        &session_start(&with_queue, &data_home, dir.path()),
        "planner",
    );
    assert!(status["cursor"].is_number());

    // A failure is one line of explanation, never a failed session start.
    let one_line = |output: &Output| {
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout.clone()).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        text
    };
    let missing = one_line(&session_start(&[("DAGQ_ROLE", "inbox")], &data_home, &repo));
    assert!(missing.contains("dagq was not found"), "{missing}");
    let bogus = dir.path().join("not-executable");
    fs::write(&bogus, "").unwrap();
    let bad = one_line(&session_start(
        &[
            ("DAGQ_ROLE", "inbox"),
            ("DAGQ_BIN", bogus.to_str().unwrap()),
        ],
        &data_home,
        &repo,
    ));
    assert!(bad.contains("not an executable"), "{bad}");
    let outside = one_line(&session_start(&inbox_env, &data_home, dir.path()));
    assert!(outside.starts_with("dagq status failed: "), "{outside}");
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
    assert_eq!(resolved["binary_version"], dagq::VERSION);
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
