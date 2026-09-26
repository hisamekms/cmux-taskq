mod common;

use common::Bounded;
use common::cli::*;

use std::{
    path::Path,
    process::{Command, Output},
};

use dagq::infrastructure::sqlite::SqliteQueue;
use serde_json::Value;

#[test]
fn version_works_outside_a_repository_and_without_a_queue() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .arg("--version")
        .current_dir(dir.path())
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!("{} {}", env!("CARGO_PKG_NAME"), dagq::VERSION)
    );
}

/// `--version` names the build (ADR-0045 decision 2): a development version
/// carries the commit it was built from, and a release its version alone.
#[test]
fn version_is_the_build_identifier() {
    let package = env!("CARGO_PKG_VERSION");
    if dagq::build_id::is_prerelease(package) {
        let metadata = dagq::VERSION
            .strip_prefix(&format!("{package}+"))
            .unwrap_or_else(|| panic!("{} lacks build metadata", dagq::VERSION));
        let commit = metadata.strip_suffix(".dirty").unwrap_or(metadata);
        assert!(
            commit == dagq::build_id::UNKNOWN_COMMIT
                || (commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit())),
            "{}",
            dagq::VERSION
        );
    } else {
        assert_eq!(dagq::VERSION, package);
    }
}

/// Runs `binary` (a copy of this one, as `claim` leaves a run's wrapper in
/// `runs/<id>/runner`) against `db`.
fn run_copy(binary: &Path, db: &Path, args: &[&str]) -> Output {
    Command::new(binary)
        .env_remove("DAGQ_ROLE")
        .arg("--db")
        .arg(db)
        .args(args)
        .bounded_output()
        .unwrap()
}

#[test]
fn migrate_is_explicit_and_older_binaries_keep_working_within_the_floor() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let raw = rusqlite::Connection::open(&db).unwrap();
    let version = || -> i64 {
        raw.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    };
    // A queue left at schema 23 by the binary before the floor table.
    for migration in &dagq::infrastructure::schema::MIGRATIONS[..23] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch("PRAGMA application_id = 1129599281; PRAGMA user_version = 23;")
        .unwrap();
    raw.execute_batch(
        "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('old task','','','[]','draft');",
    )
    .unwrap();
    // Commands that change the queue need `migrate` first.
    for args in [&["add", "new task"][..], &["ready", "1"], &["init"]] {
        let error = refused(&db, args);
        assert!(error.contains("run `dagq migrate`"), "{args:?}: {error}");
    }
    // Commands that only read open it read-only and see it as migrated in
    // memory; the file stays at schema 23 (ADR-0045 decision 18).
    assert_eq!(ok(&db, &["list"])["total"], 1);
    assert_eq!(ok(&db, &["show", "1"])["task"]["title"], "old task");
    for args in [&["status"][..], &["graph"], &["stats"], &["goal", "list"]] {
        ok(&db, args);
    }
    assert_eq!(version(), 23);
    let check = ok(&db, &["migrate", "--check"]);
    // `doctor` reports the schema as `migrate --check` does next to its
    // runs and supervisors (ADR-0045 decision 5).
    let doctor = ok(&db, &["doctor"]);
    assert_eq!(doctor["schema"], check);
    assert_eq!(doctor["runs"], serde_json::json!([]));
    assert_eq!(check["schema_version"], 23);
    assert_eq!(check["binary_schema_version"], SqliteQueue::SCHEMA_VERSION);
    assert_eq!(check["opens"], false);
    // Every migration after 23, up to the last one the binary lists, with
    // its declaration (ADR-0067 decision 4).
    let migrations = dagq::infrastructure::schema::MIGRATIONS;
    let pending: Vec<Value> = (24..=SqliteQueue::SCHEMA_VERSION)
        .map(|version| {
            serde_json::json!({
                "version": version,
                "compatible": dagq::infrastructure::schema::is_compatible(
                    migrations[usize::try_from(version - 1).unwrap()]
                ),
            })
        })
        .collect();
    assert_eq!(check["pending"], Value::Array(pending));
    assert_eq!(
        check["pending"].as_array().unwrap()[..3],
        serde_json::json!([
            {"version": 24, "compatible": false},
            {"version": 25, "compatible": false},
            {"version": 26, "compatible": true}
        ])
        .as_array()
        .unwrap()[..]
    );
    assert_eq!(version(), 23);
    let migrated = ok(&db, &["migrate"]);
    assert_eq!(migrated["previous_version"], 23);
    assert_eq!(migrated["schema_version"], SqliteQueue::SCHEMA_VERSION);
    assert_eq!(
        migrated["floor"],
        dagq::infrastructure::schema::floor_for(SqliteQueue::SCHEMA_VERSION)
    );
    assert_eq!(migrated["commit_messages_filled"], 0);
    let backup = migrated["backup"].as_str().unwrap();
    assert!(Path::new(backup).starts_with(dir.path().canonicalize().unwrap().join("backups")));
    assert_eq!(version(), SqliteQueue::SCHEMA_VERSION);
    let doctor = ok(&db, &["doctor"]);
    assert_eq!(doctor["schema"]["opens"], true);
    assert_eq!(doctor["schema"]["pending"], serde_json::json!([]));
    ok(&db, &["add", "task one"]);

    // A later binary's compatible migration: this binary, and the wrapper
    // copy of it a running run uses, now are the older binaries.
    let runner = dir.path().join("runs/run-1/runner");
    std::fs::create_dir_all(runner.parent().unwrap()).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_dagq"), &runner).unwrap();
    raw.execute_batch(&format!(
        "ALTER TABLE tasks ADD COLUMN future_hint TEXT;
         CREATE TABLE future_things (id INTEGER PRIMARY KEY);
         PRAGMA user_version = {};",
        SqliteQueue::SCHEMA_VERSION + 1
    ))
    .unwrap();
    for args in [
        &["add", "task two"][..],
        &["show", "2"],
        &["list"],
        &["status"],
        &["doctor"],
        &["migrate"],
    ] {
        let output = run_copy(&runner, &db, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(ok(&db, &["list"])["total"], 3);
    assert_eq!(version(), SqliteQueue::SCHEMA_VERSION + 1);

    // A later breaking migration raises the floor: the older binaries stop
    // with the reason, and leave the queue alone.
    raw.execute_batch(&format!(
        "UPDATE schema_floor SET floor = {0}; PRAGMA user_version = {0};",
        SqliteQueue::SCHEMA_VERSION + 2
    ))
    .unwrap();
    for args in [
        &["list"][..],
        &["migrate"],
        &[
            "session",
            "--run",
            "run-1",
            "--lease",
            "token",
            "--claude",
            "/bin/false",
        ],
    ] {
        let output = run_copy(&runner, &db, args);
        assert!(!output.status.success(), "{args:?} succeeded");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        let error = error["error"].as_str().unwrap();
        assert!(
            error.contains(&format!(
                "unsupported queue schema version {0}: the queue refuses binaries older than schema {0}",
                SqliteQueue::SCHEMA_VERSION + 2
            )),
            "{args:?}: {error}"
        );
    }
    // `doctor` still reports the schema that refuses the binary, and why.
    let output = run_copy(&runner, &db, &["doctor"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doctor: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        doctor["schema"]["schema_version"],
        SqliteQueue::SCHEMA_VERSION + 2
    );
    assert_eq!(doctor["schema"]["floor"], SqliteQueue::SCHEMA_VERSION + 2);
    assert_eq!(doctor["schema"]["opens"], false);
    assert!(doctor.get("runs").is_none());
    assert!(
        doctor["error"]
            .as_str()
            .unwrap()
            .contains("unsupported queue schema version"),
        "{doctor}"
    );
    assert_eq!(version(), SqliteQueue::SCHEMA_VERSION + 2);
}

/// Only when [`a_wait_past_its_limit_fails_with_the_test_and_the_condition`]
/// runs it: a wait that never ends, timed with a short limit.
#[test]
#[ignore = "run by a_wait_past_its_limit_fails_with_the_test_and_the_condition"]
fn deadline_probe() {
    if std::env::var_os("DAGQ_DEADLINE_PROBE").is_none() {
        return;
    }
    let _waiting = common::within(
        std::time::Duration::from_millis(300),
        "the probe's condition to hold",
    );
    loop {
        std::thread::park();
    }
}

/// A wait past its limit ends the test binary as a failure, naming the test
/// and the condition it waited for (task 324), instead of hanging.
#[test]
fn a_wait_past_its_limit_fails_with_the_test_and_the_condition() {
    let started = std::time::Instant::now();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "deadline_probe",
            "--ignored",
            "--test-threads",
            "2",
        ])
        .env("DAGQ_DEADLINE_PROBE", "1")
        .bounded_output()
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(common::TIMED_OUT), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "test deadline_probe timed out: the probe's condition to hold did not happen within 300ms"
        ),
        "{stderr}"
    );
}

/// `install` puts a binary in place by a rename that keeps the old one as
/// `<name>.previous`, and hands a running supervisor over to it (ADR-0045
/// decisions 10, 11, 14): the supervisor process execs the new file under
/// its own pid and token and goes on. `--rollback` swaps the two back the
/// same way; without a previous binary it refuses.
#[test]
fn install_hands_a_running_supervisor_over_under_its_pid_and_rolls_back() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue").join("queue.db");
    ok(&db, &["init"]);
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "seed",
        ],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .bounded_status()
                .unwrap()
                .success()
        );
    }
    let stub = |name: &str, text: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\nprintf '{text}\\n'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let (cmux, claude) = (stub("cmux", "PONG"), stub("claude", "stub 1.0"));
    // The fixed binary the supervisor runs, and the one `install` replaces.
    let fixed = dir.path().join("bin").join("dagq");
    std::fs::create_dir_all(fixed.parent().unwrap()).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_dagq"), &fixed).unwrap();
    let previous = dir.path().join("bin").join("dagq.previous");

    let missing = invoke(
        &db,
        &["install", "--rollback", "--to", fixed.to_str().unwrap()],
    );
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("no previous binary"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );

    let mut supervisor = Command::new(&fixed)
        .arg("--db")
        .arg(&db)
        .args(["supervise", "--observe-interval", "0", "--repo"])
        .arg(&repo)
        .arg("--cmux")
        .arg(&cmux)
        .arg("--claude")
        .arg(&claude)
        .env_remove("DAGQ_ROLE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let registered = || SqliteQueue::open(&db).unwrap().supervisors().unwrap();
    let started = std::time::Instant::now();
    while registered().is_empty() {
        assert!(
            started.elapsed().as_secs() < 30,
            "the supervisor never registered"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let token = registered()[0].token.clone();
    assert_eq!(registered()[0].pid, supervisor.id());
    assert!(registered()[0].handoff_accepted);

    for args in [&["--from", env!("CARGO_BIN_EXE_dagq")][..], &["--rollback"]] {
        let mut full = vec![
            "install",
            "--to",
            fixed.to_str().unwrap(),
            "--handoff-timeout",
            "60",
        ];
        full.extend_from_slice(args);
        let report = ok(&db, &full);
        assert_eq!(report["outcome"], "installed", "{report}");
        assert_eq!(report["version"], dagq::VERSION);
        assert_eq!(report["previous"], previous.to_str().unwrap());
        assert_eq!(report["migrated"], Value::Null);
        assert_eq!(
            report["supervisors"][0]["token"],
            token.as_str(),
            "{report}"
        );
        assert_eq!(report["supervisors"][0]["pid"], supervisor.id());
        assert!(previous.is_file());
        let registration = registered().remove(0);
        assert_eq!(registration.token, token);
        assert_eq!(registration.pid, supervisor.id());
        assert_eq!(registration.handoff_binary, None);
        assert_eq!(registration.binary_version.as_deref(), Some(dagq::VERSION));
        assert!(
            supervisor.try_wait().unwrap().is_none(),
            "the supervisor exited"
        );
    }

    // SIGINT drains the continued supervisor like any other.
    unsafe { libc::kill(supervisor.id() as i32, libc::SIGINT) };
    let exit = {
        let _waiting = common::within(common::STEP_LIMIT, "the supervisor to drain on SIGINT");
        supervisor.wait().unwrap()
    };
    assert!(exit.success());
    assert!(registered().is_empty());
}

/// `supervise --auto-update` builds and installs the runtime of every
/// landing on main that changes it (ADR-0045 decision 17): its job builds
/// the commit in the queue's own checkout (a stub build copies a binary),
/// puts it in place like `install` and hands the supervisor over to it
/// under its pid and token. A commit that changes no runtime path starts
/// no job. A build whose supervisor dies at its start puts the old binary
/// back and opens the `update_failed` ask; `status` shows each step.
#[test]
fn auto_update_installs_each_runtime_landing_and_puts_a_broken_build_back() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue").join("queue.db");
    ok(&db, &["init"]);
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .bounded_output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    let commit = |path: &str| {
        let file = repo.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, path).unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", path]);
        git(&["rev-parse", "HEAD"])
    };
    git(&["init", "-q", "-b", "main"]);
    let seed = commit("seed.txt");
    let stub = |name: &str, text: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let cmux = stub("cmux", "#!/bin/sh\nprintf 'PONG\\n'\n");
    let claude = stub("claude", "#!/bin/sh\nprintf 'stub 1.0\\n'\n");
    let bin = env!("CARGO_BIN_EXE_dagq");
    // Like the real binary for every check `install` makes, but a
    // supervisor it runs dies at once.
    let broken = stub(
        "broken-dagq",
        &format!(
            "#!/bin/sh\ncase \" $* \" in *\" --handoff-token probe \"*) exec '{bin}' \"$@\";; esac\n\
for a in \"$@\"; do if [ \"$a\" = supervise ]; then echo 'broken build' >&2; exit 3; fi; done\n\
exec '{bin}' \"$@\"\n"
        ),
    );
    let build = format!(
        "mkdir -p \"$CARGO_TARGET_DIR/release\" && if [ -f src/broken ]; then cp '{}' \
\"$CARGO_TARGET_DIR/release/dagq\"; else cp '{bin}' \"$CARGO_TARGET_DIR/release/dagq\"; fi",
        broken.display()
    );
    let fixed = dir.path().join("bin").join("dagq");
    std::fs::create_dir_all(fixed.parent().unwrap()).unwrap();
    std::fs::copy(bin, &fixed).unwrap();
    let mut supervisor = Command::new(&fixed)
        .arg("--db")
        .arg(&db)
        .args(["supervise", "--observe-interval", "0", "--repo"])
        .arg(&repo)
        .arg("--cmux")
        .arg(&cmux)
        .arg("--claude")
        .arg(&claude)
        .args(["--auto-update", "--update-interval", "1"])
        .arg("--update-build-command")
        .arg(&build)
        .env_remove("DAGQ_ROLE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let registered = || SqliteQueue::open(&db).unwrap().supervisors().unwrap();
    let wait = |what: &str, done: &mut dyn FnMut() -> bool| {
        let started = std::time::Instant::now();
        while !done() {
            assert!(
                started.elapsed().as_secs() < 90,
                "{what} did not happen within 90s; status: {}",
                ok(&db, &["status"])
            );
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    };
    wait("the registration", &mut || !registered().is_empty());
    let token = registered()[0].token.clone();
    assert!(registered()[0].auto_update);
    let updates = || SqliteQueue::open(&db).unwrap().binary_updates(100).unwrap();
    let installed = |sha: &str| {
        updates()
            .iter()
            .any(|u| u.kind == "update_installed" && u.commit.as_deref() == Some(sha))
    };

    // The test binary names a commit this repository does not have, so the
    // first look builds main's head.
    wait("the update to the seed", &mut || installed(&seed));
    let status = ok(&db, &["status"]);
    assert_eq!(status["auto_update"]["enabled"], true, "{status}");
    assert_eq!(status["auto_update"]["state"], "installed", "{status}");
    assert_eq!(status["auto_update"]["commit"], seed.as_str(), "{status}");
    assert_eq!(status["supervisors"][0]["auto_update"], true, "{status}");
    assert_eq!(status["version"], dagq::VERSION);
    assert!(fixed.with_file_name("dagq.previous").is_file());
    let registration = registered().remove(0);
    assert_eq!(
        (registration.token.as_str(), registration.pid),
        (token.as_str(), supervisor.id())
    );
    assert!(
        supervisor.try_wait().unwrap().is_none(),
        "the supervisor exited"
    );
    let checkout = db.parent().unwrap().join("update").join("checkout");
    assert!(checkout.join("seed.txt").is_file());

    // Documentation changes no runtime path: no job.
    let started = |sha: &str| {
        updates()
            .iter()
            .any(|u| u.kind == "update_started" && u.commit.as_deref() == Some(sha))
    };
    let docs = commit("docs/notes.md");
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert!(!started(&docs), "{:?}", updates());

    let source = commit("src/lib.rs");
    wait("the update to the source change", &mut || {
        installed(&source)
    });
    assert!(
        supervisor.try_wait().unwrap().is_none(),
        "the supervisor exited"
    );
    assert_eq!(registered()[0].pid, supervisor.id());

    // A build whose supervisor dies at its start: the binary it replaced
    // is put back and the inbox is asked.
    let broken_commit = commit("src/broken");
    wait("the failed update", &mut || {
        // Reaps the supervisor once the broken build's exec ended it.
        let _ = supervisor.try_wait();
        updates().iter().any(|u| {
            u.kind == "update_failed" && u.commit.as_deref() == Some(broken_commit.as_str())
        })
    });
    let failed = updates()
        .into_iter()
        .find(|u| u.kind == "update_failed")
        .unwrap();
    assert_eq!(failed.payload["stage"], "install", "{failed:?}");
    assert_eq!(
        failed.payload["supervisor"]["state"], "stopped",
        "{failed:?}"
    );
    assert_eq!(
        std::fs::read(&fixed).unwrap(),
        std::fs::read(bin).unwrap(),
        "the binary in place is not the one the broken build replaced"
    );
    let status = ok(&db, &["status"]);
    assert_eq!(status["auto_update"]["state"], "failed", "{status}");
    let asks = ok(&db, &["asks"]);
    let ask = asks["asks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ask| ask["subject"] == "update_failed")
        .unwrap_or_else(|| panic!("no update_failed ask: {asks}"));
    assert_eq!(ask["kind"], "blocked");
    assert_eq!(ask["options"], serde_json::json!(["retry", "skip"]));
    assert!(
        ask["question"]
            .as_str()
            .unwrap()
            .contains("failed at its install")
    );
    let exit = {
        let _waiting = common::within(common::STEP_LIMIT, "the broken supervisor to exit");
        supervisor.wait().unwrap()
    };
    assert_eq!(exit.code(), Some(3));
}
