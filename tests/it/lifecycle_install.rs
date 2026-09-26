//! `dagq install` and the auto-update job against fake binaries, launchd,
//! cmux and process signals: the migrate, replace and handoff, the restore
//! on a failed handoff, the drain for a breaking migration, and the job's
//! watch, restore and asks.

use crate::common;

use common::lifecycle::*;

use anyhow::{Result, bail};
use dagq::{VERSION, domain::SupervisorMode, infrastructure::sqlite::SqliteQueue};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::Duration,
};

/// [`Binaries`] that build, run and move nothing: what each call was, the
/// schema it reports, and whether it answers a probe.
struct FakeBinaries {
    schema: dagq::application::install::SchemaCheck,
    calls: Mutex<Vec<String>>,
    up: Mutex<Vec<Vec<String>>>,
}

impl FakeBinaries {
    fn new(pending: &[(i64, bool)], opens: bool) -> Self {
        Self {
            schema: dagq::application::install::SchemaCheck {
                pending: pending
                    .iter()
                    .map(
                        |&(version, compatible)| dagq::application::install::PendingMigration {
                            version,
                            compatible,
                        },
                    )
                    .collect(),
                opens,
            },
            calls: Mutex::default(),
            up: Mutex::default(),
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn note(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
}

impl dagq::application::install::Binaries for FakeBinaries {
    fn build(&self, checkout: &Path) -> Result<PathBuf> {
        self.note(format!("build {}", checkout.display()));
        Ok(checkout.join("target/release/dagq"))
    }
    fn version(&self, binary: &Path) -> Result<String> {
        Ok(if binary.ends_with("dagq.previous") {
            "0.0.1".into()
        } else {
            VERSION.into()
        })
    }
    fn probe(&self, binary: &Path) -> Result<()> {
        self.note(format!("probe {}", binary.display()));
        Ok(())
    }
    fn takes_handoff(&self, binary: &Path) -> bool {
        !binary.ends_with("old/dagq")
    }
    fn schema(&self, _: &Path, _: &Path) -> Result<dagq::application::install::SchemaCheck> {
        Ok(self.schema.clone())
    }
    fn migrate(&self, binary: &Path, _: &Path) -> Result<Value> {
        self.note(format!("migrate {}", binary.display()));
        Ok(json!({"applied": self.schema.pending.len()}))
    }
    fn replace(&self, source: &Path, target: &Path) -> Result<()> {
        self.note(format!("replace {} {}", source.display(), target.display()));
        Ok(())
    }
    fn restore(&self, target: &Path) -> Result<()> {
        self.note(format!("restore {}", target.display()));
        Ok(())
    }
    fn run(&self, binary: &Path, arguments: &[String]) -> Result<Value> {
        self.note(format!("run {}", binary.display()));
        self.up.lock().unwrap().push(arguments.to_vec());
        Ok(json!({"supervisor": {"outcome": "started"}}))
    }
}

fn install_with(
    fixture: &Fixture,
    binaries: &FakeBinaries,
    processes: &FakeProcesses,
    down: &dyn Fn() -> Result<Value>,
    options: &dagq::application::install::InstallOptions,
) -> Result<Value> {
    let queues = |db: &Path| -> std::sync::Arc<dyn dagq::application::QueueOpener> {
        std::sync::Arc::new(dagq::infrastructure::runtime_store::SqliteOpener {
            db: db.to_owned(),
            generators: dagq::infrastructure::clock::system(),
        })
    };
    dagq::application::install::install(
        &dagq::application::install::Ports {
            binaries,
            files: &dagq::infrastructure::run_files::LocalRunFiles,
            processes,
            clock: &dagq::infrastructure::clock::SystemClock,
            queues: &queues,
            down,
        },
        Some(&fixture.location.db),
        options,
    )
}

fn install_options(
    source: dagq::application::install::Source,
) -> dagq::application::install::InstallOptions {
    dagq::application::install::InstallOptions {
        source,
        target: "/opt/bin/dagq".into(),
        allow_breaking: false,
        restart: vec!["--cmux".into(), "/opt/cmux".into()],
        handoff_timeout: Duration::from_secs(5),
        poll: Duration::from_millis(20),
    }
}

/// `install` builds the checkout, probes the build, applies compatible
/// migrations with it, puts it in place and hands the live supervisors that
/// take a handoff over to it; one of an older binary is left for `up` to
/// drain. A handoff that fails puts the replaced binary back.
#[test]
fn install_migrates_replaces_and_hands_over_and_restores_on_a_failed_handoff() {
    use dagq::application::install::Source;
    let fixture = fixture();
    let queue = handoff_supervisor(&fixture, "new", SupervisorMode::InCmux);
    let mut old = SqliteQueue::open(&fixture.location.db).unwrap();
    old.register_supervisor("older", 424_243, 1, "0.0.1")
        .unwrap();
    let binaries = FakeBinaries::new(&[(27, true)], false);
    let processes = FakeProcesses::default();
    let no_down = || -> Result<Value> { panic!("no drain for compatible migrations") };
    let report = thread::scope(|scope| {
        scope.spawn(|| {
            let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
            wait_until(&processes, std::process::id(), || {
                queue.handoff_request("new").unwrap().is_some()
            });
            queue
                .resume_registration("new", std::process::id(), VERSION)
                .unwrap();
        });
        install_with(
            &fixture,
            &binaries,
            &processes,
            &no_down,
            &install_options(Source::Checkout("/src/dagq".into())),
        )
        .unwrap()
    });
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(report["migrated"], json!({"applied": 1}));
    assert_eq!(report["supervisors"][0]["token"], "new");
    assert_eq!(report["not_handed_off"][0]["token"], "older");
    assert_eq!(report["previous"], "/opt/bin/dagq.previous");
    assert_eq!(
        binaries.calls(),
        [
            "build /src/dagq",
            "probe /src/dagq/target/release/dagq",
            "migrate /src/dagq/target/release/dagq",
            "replace /src/dagq/target/release/dagq /opt/bin/dagq",
        ]
    );

    // Nobody takes this one: the handoff times out and the binary is put back.
    let binaries = FakeBinaries::new(&[], true);
    let mut options = install_options(Source::Binary("/built/dagq".into()));
    options.handoff_timeout = Duration::from_millis(200);
    let error = format!(
        "{:#}",
        install_with(&fixture, &binaries, &processes, &no_down, &options).unwrap_err()
    );
    assert!(error.contains("the handoff to"), "{error}");
    assert!(error.contains("is back at /opt/bin/dagq"), "{error}");
    assert_eq!(queue.handoff_request("new").unwrap(), None);

    // A binary that predates the handoff would end the supervisor it is
    // exec'd in: nothing is replaced.
    let older = FakeBinaries::new(&[], true);
    let error = format!(
        "{:#}",
        install_with(
            &fixture,
            &older,
            &processes,
            &no_down,
            &install_options(Source::Binary("/old/dagq".into())),
        )
        .unwrap_err()
    );
    assert!(error.contains("predates the handoff"), "{error}");
    assert_eq!(older.calls(), ["probe /old/dagq"]);
    assert_eq!(
        binaries.calls(),
        [
            "probe /built/dagq",
            "replace /built/dagq /opt/bin/dagq",
            "restore /opt/bin/dagq",
        ]
    );
    drop(queue);
}

/// A build with a breaking migration is refused without `--allow-breaking`
/// and replaces nothing; with it, the supervisor is drained, the queue
/// migrated, the binary replaced and `up` run with the drained supervisor's
/// mode and parallelism. A rollback past a breaking migration is refused,
/// and so is one without a previous binary.
#[test]
fn install_drains_only_for_a_breaking_migration_when_allowed() {
    use dagq::application::install::Source;
    let fixture = fixture();
    let _queue = handoff_supervisor(&fixture, "live", SupervisorMode::InCmux);
    let processes = FakeProcesses::default();
    let binaries = FakeBinaries::new(&[(27, true), (28, false)], false);
    let drained = Mutex::new(0);
    let down = || -> Result<Value> {
        *drained.lock().unwrap() += 1;
        Ok(json!({"outcome": "stopped"}))
    };
    let error = format!(
        "{:#}",
        install_with(
            &fixture,
            &binaries,
            &processes,
            &down,
            &install_options(Source::Binary("/built/dagq".into())),
        )
        .unwrap_err()
    );
    assert!(error.contains("breaking migration(s) 28"), "{error}");
    assert!(error.contains("--allow-breaking"), "{error}");
    assert_eq!(binaries.calls(), ["probe /built/dagq"]);
    assert_eq!(*drained.lock().unwrap(), 0);

    let mut options = install_options(Source::Binary("/built/dagq".into()));
    options.allow_breaking = true;
    let report = install_with(&fixture, &binaries, &processes, &down, &options).unwrap();
    assert_eq!(*drained.lock().unwrap(), 1);
    assert_eq!(report["drained"]["outcome"], "stopped", "{report}");
    assert_eq!(report["up"]["supervisor"]["outcome"], "started");
    assert_eq!(
        binaries.calls()[2..],
        [
            "migrate /built/dagq",
            "replace /built/dagq /opt/bin/dagq",
            "run /opt/bin/dagq",
        ]
    );
    let db = fixture.location.db.to_str().unwrap();
    assert_eq!(
        binaries.up.lock().unwrap()[0],
        [
            "--db",
            db,
            "up",
            "--parallel",
            "4",
            "--in-cmux",
            "--cmux",
            "/opt/cmux"
        ]
    );

    let binaries = FakeBinaries::new(&[], false);
    let dir = tempfile::tempdir().unwrap();
    let mut options = install_options(Source::Rollback);
    options.target = dir.path().join("dagq");
    let error = format!(
        "{:#}",
        install_with(&fixture, &binaries, &processes, &down, &options).unwrap_err()
    );
    assert!(error.contains("no previous binary"), "{error}");
    fs::write(dir.path().join("dagq.previous"), "").unwrap();
    let error = format!(
        "{:#}",
        install_with(&fixture, &binaries, &processes, &down, &options).unwrap_err()
    );
    assert!(error.contains("the queue refuses 0.0.1"), "{error}");
    assert_eq!(
        dagq::application::install::parse_version("dagq 1.2.3\n").unwrap(),
        "1.2.3"
    );
    assert!(dagq::application::install::parse_version("").is_err());
}

/// [`Binaries`] for the automatic update's job: the build leaves a real
/// file (or fails), everything under `old` (the fixed binary and its
/// `.previous`) is the old build `0.0.1`, anything else this build.
struct UpdateBinaries {
    old: PathBuf,
    built: PathBuf,
    build_fails: bool,
    pending: Vec<(i64, bool)>,
    calls: Mutex<Vec<String>>,
}

impl UpdateBinaries {
    fn new(dir: &Path, build_fails: bool, pending: &[(i64, bool)]) -> Self {
        let built = dir.join("built").join("dagq");
        fs::create_dir_all(built.parent().unwrap()).unwrap();
        fs::write(&built, "new build").unwrap();
        Self {
            old: dir.join("bin"),
            built,
            build_fails,
            pending: pending.to_vec(),
            calls: Mutex::default(),
        }
    }
    fn note(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl dagq::application::install::Binaries for UpdateBinaries {
    fn build(&self, _: &Path) -> Result<PathBuf> {
        bail!("the job builds into its own target")
    }
    fn version(&self, binary: &Path) -> Result<String> {
        Ok(if binary.starts_with(&self.old) {
            "0.0.1".into()
        } else {
            VERSION.into()
        })
    }
    fn probe(&self, _: &Path) -> Result<()> {
        Ok(())
    }
    fn takes_handoff(&self, _: &Path) -> bool {
        true
    }
    fn schema(&self, _: &Path, _: &Path) -> Result<dagq::application::install::SchemaCheck> {
        Ok(dagq::application::install::SchemaCheck {
            pending: self
                .pending
                .iter()
                .map(
                    |&(version, compatible)| dagq::application::install::PendingMigration {
                        version,
                        compatible,
                    },
                )
                .collect(),
            opens: self.pending.is_empty(),
        })
    }
    fn migrate(&self, _: &Path, _: &Path) -> Result<Value> {
        self.note("migrate".into());
        Ok(json!({"applied": self.pending.len()}))
    }
    fn replace(&self, _: &Path, target: &Path) -> Result<()> {
        self.note(format!("replace {}", target.display()));
        Ok(())
    }
    fn restore(&self, target: &Path) -> Result<()> {
        self.note(format!("restore {}", target.display()));
        Ok(())
    }
    fn run(&self, _: &Path, _: &[String]) -> Result<Value> {
        bail!("the job runs no binary")
    }
    fn checkout(&self, _: &Path, checkout: &Path, commit: &str) -> Result<()> {
        self.note(format!("checkout {} {commit}", checkout.display()));
        Ok(())
    }
    fn build_into(&self, _: &Path, target: &Path, _: Option<&str>, _: &Path) -> Result<PathBuf> {
        self.note(format!("build {}", target.display()));
        if self.build_fails {
            bail!("cargo build failed");
        }
        Ok(self.built.clone())
    }
}

/// The supervisor the job updates: registered under a pid of its own, which
/// the fake process control keeps alive until the test says otherwise.
const UPDATED_PID: u32 = 424_242;

fn run_update_job(
    fixture: &Fixture,
    binaries: &UpdateBinaries,
    processes: &FakeProcesses,
    restarted: &Mutex<Vec<String>>,
) -> Value {
    use dagq::application::update;
    let queues = |db: &Path| -> std::sync::Arc<dyn dagq::application::QueueOpener> {
        std::sync::Arc::new(dagq::infrastructure::runtime_store::SqliteOpener {
            db: db.to_owned(),
            generators: dagq::infrastructure::clock::system(),
        })
    };
    let restart = |registration: &dagq::domain::SupervisorRegistration| -> Result<Value> {
        restarted.lock().unwrap().push(registration.token.clone());
        Ok(json!({"by": "test"}))
    };
    let dir = fixture._dir.path();
    update::run(
        &update::JobPorts {
            binaries,
            files: &dagq::infrastructure::run_files::LocalRunFiles,
            processes,
            clock: &dagq::infrastructure::clock::SystemClock,
            queues: &queues,
            restart: &restart,
        },
        &fixture.location.db,
        &update::JobOptions {
            commit: "c0ffee".repeat(6) + "c0ff",
            token: "auto".into(),
            target: dir.join("bin").join("dagq"),
            repository: fixture.repo.clone(),
            paths: update::UpdatePaths::under(&dir.join("queue-dir")),
            log: dir.join("build.log"),
            build_command: None,
            restart: vec!["--cmux".into(), "/opt/cmux".into()],
            handoff_timeout: Duration::from_secs(5),
            watch_timeout: Duration::from_secs(5),
            poll: Duration::from_millis(20),
            pid: 7,
        },
    )
    .unwrap()
}

fn auto_supervisor(fixture: &Fixture) -> SqliteQueue {
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    queue
        .register_supervisor("auto", UPDATED_PID, 2, "0.0.1")
        .unwrap();
    queue.accept_handoff("auto").unwrap();
    queue.set_auto_update("auto", true).unwrap();
    queue
        .set_supervisor_mode("auto", SupervisorMode::InCmux, Some("ws-auto"))
        .unwrap();
    queue
}

/// Take the handoff the way the exec'd binary does, and heartbeat on
/// (`alive`) or die a moment later.
fn take_and_heartbeat(fixture: &Fixture, processes: &FakeProcesses, alive: bool) {
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    wait_until(processes, UPDATED_PID, || {
        queue.handoff_request("auto").unwrap().is_some()
    });
    queue
        .resume_registration("auto", UPDATED_PID, VERSION)
        .unwrap();
    thread::sleep(Duration::from_millis(1100));
    if alive {
        queue.heartbeat("auto").unwrap();
    } else {
        processes.dead.lock().unwrap().insert(UPDATED_PID);
    }
}

/// The automatic update's job (ADR-0045 decisions 13, 17): a build that
/// the supervisor takes and heartbeats on is installed; one it dies on is
/// put back and the supervisor started again; a failed build replaces
/// nothing; each failure opens the `update_failed` ask, a newer one
/// superseding the older; a breaking migration waits in `approve_update`.
#[test]
fn the_update_job_installs_watches_restores_and_asks() {
    let fixture = fixture();
    let queue = auto_supervisor(&fixture);
    let processes = FakeProcesses::default();
    let restarted = Mutex::new(Vec::new());
    let dir = fixture._dir.path();
    let target = dir.join("bin").join("dagq");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "old build").unwrap();

    let binaries = UpdateBinaries::new(dir, false, &[(40, true)]);
    let report = thread::scope(|scope| {
        scope.spawn(|| take_and_heartbeat(&fixture, &processes, true));
        run_update_job(&fixture, &binaries, &processes, &restarted)
    });
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(report["version"], VERSION);
    assert_eq!(
        binaries.calls(),
        [
            format!(
                "checkout {} {}",
                dir.join("queue-dir/update/checkout").display(),
                "c0ffee".repeat(6) + "c0ff"
            ),
            format!("build {}", dir.join("queue-dir/update/target").display()),
            "migrate".to_owned(),
            format!("replace {}", target.display()),
        ]
    );
    let kinds = |queue: &SqliteQueue| {
        queue
            .binary_updates(10)
            .unwrap()
            .into_iter()
            .map(|u| u.kind)
            .collect::<Vec<_>>()
    };
    assert_eq!(kinds(&queue), ["update_installed", "update_built"]);

    // The new binary dies after it took the handoff: back to the old one,
    // and the gone in-cmux supervisor is started again.
    let binaries = UpdateBinaries::new(dir, false, &[]);
    let report = thread::scope(|scope| {
        scope.spawn(|| take_and_heartbeat(&fixture, &processes, false));
        run_update_job(&fixture, &binaries, &processes, &restarted)
    });
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["stage"], "watch", "{report}");
    assert_eq!(report["restored"]["restored"], true, "{report}");
    assert_eq!(report["supervisor"]["state"], "restarted", "{report}");
    assert_eq!(*restarted.lock().unwrap(), ["auto"]);
    assert!(
        binaries
            .calls()
            .contains(&format!("restore {}", target.display()))
    );
    let first_ask = report["ask_id"].as_i64().unwrap();

    // A failed build replaces nothing, and its ask replaces the older one.
    processes.dead.lock().unwrap().clear();
    let binaries = UpdateBinaries::new(dir, true, &[]);
    let report = run_update_job(&fixture, &binaries, &processes, &restarted);
    assert_eq!(report["stage"], "build", "{report}");
    assert!(binaries.calls().iter().all(|c| !c.starts_with("replace")));
    let asks = queue
        .asks(dagq::application::AskQuery {
            all: true,
            ..Default::default()
        })
        .unwrap();
    let older = asks.iter().find(|a| a.id.as_i64() == first_ask).unwrap();
    assert_eq!(older.answer.as_deref(), Some("superseded"));
    assert_eq!(older.answered_by.as_deref(), Some("runtime"));
    assert!(older.closed_at.is_some());
    let open: Vec<_> = asks.iter().filter(|a| a.is_open()).collect();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].subject.as_deref(), Some("update_failed"));
    assert_eq!(open[0].options, ["retry", "skip"]);
    assert!(
        open[0].question.contains("failed at its build"),
        "{}",
        open[0].question
    );

    // A breaking migration: kept for a person, nothing replaced.
    let binaries = UpdateBinaries::new(dir, false, &[(40, true), (41, false)]);
    let report = run_update_job(&fixture, &binaries, &processes, &restarted);
    assert_eq!(report["outcome"], "awaiting_approval", "{report}");
    assert_eq!(report["migrations"], json!([41]));
    let staged = dir.join("queue-dir/update/staged/dagq");
    assert_eq!(fs::read_to_string(&staged).unwrap(), "new build");
    assert!(
        report["command"]
            .as_str()
            .unwrap()
            .contains("--allow-breaking --cmux /opt/cmux")
    );
    assert!(binaries.calls().iter().all(|c| !c.starts_with("replace")));
    let asks = queue.asks(dagq::application::AskQuery::default()).unwrap();
    let approve = asks
        .iter()
        .find(|a| a.subject.as_deref() == Some("approve_update"))
        .unwrap();
    assert_eq!(approve.options, ["install", "skip"]);
    assert_eq!(kinds(&queue)[0], "update_awaiting_approval");

    // `status` reads where the update stands.
    let status = dagq::compose::status(&fixture.location.db).unwrap();
    assert_eq!(
        status["auto_update"]["state"], "awaiting_approval",
        "{status}"
    );
}
