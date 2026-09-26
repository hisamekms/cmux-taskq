//! The automatic update of the fixed binary (ADR-0045 decision 17). A
//! supervisor registered with `auto_update` (`up --auto-update`) looks at
//! main on its passes; when main moved past the last commit it updated to
//! (or the commit its own build names) and the commits in between change
//! the runtime ([`RUNTIME_PATHS`]), it starts the update job ([`run`], the
//! hidden `auto-update` command) in a session of its own and goes on
//! supervising. The job builds that commit in the queue's own checkout
//! and target (`<queue dir>/update/`, never a person's checkout), then does
//! what `install` does ([`super::install::install`]: the check, the
//! compatible migrations, the swap that keeps `.previous`, the handoff), and
//! watches the supervisor that exec'd the new binary heartbeat on. A build
//! that fails, a check that fails or a supervisor that does not come back
//! under the new binary puts the old binary back (and starts the
//! supervisor again when it is gone) and opens the `update_failed` ask for
//! the inbox; a build whose migrations would break the old binary is not
//! installed, and waits in the `approve_update` ask for a person to drain
//! and install it. Every step is a row of `binary_updates`; the job's own
//! output goes to the queue's `logs/`.

use super::{
    Clock, ProcessControl, Queue, QueueOpener, RunFiles,
    install::{self, Binaries, InstallOptions, Source, previous_path},
};
use crate::domain::{
    APPROVE_UPDATE_OPTIONS, APPROVE_UPDATE_SUBJECT, BinaryUpdate, HEARTBEAT_TIMEOUT_SECS,
    SupervisorRegistration, UPDATE_FAILED_OPTIONS, UPDATE_FAILED_SUBJECT,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

/// The supervisor started the job for `commit_sha` (`pid`, `log`, `base`).
pub const UPDATE_STARTED: &str = "update_started";
/// The job built the commit (`binary`).
pub const UPDATE_BUILT: &str = "update_built";
/// The supervisor runs the new binary (`version`, `previous_version`).
pub const UPDATE_INSTALLED: &str = "update_installed";
/// The build, its check or the handoff failed (`stage`, `error`, `restored`,
/// `supervisor`, `ask_id`).
pub const UPDATE_FAILED: &str = "update_failed";
/// The build brings a breaking migration and waits for a person
/// (`version`, `migrations`, `binary`, `ask_id`).
pub const UPDATE_AWAITING_APPROVAL: &str = "update_awaiting_approval";
/// The supervisor applied the answer of an `update_failed` ask
/// (`ask_id`, `answer`).
pub const UPDATE_ANSWERED: &str = "update_answered";
/// A `retry` answer: the next check builds main's head again.
pub const UPDATE_RETRY: &str = "update_retry";
/// `asked_by` of the update's asks.
pub const UPDATE_ASKER: &str = "supervisor";

/// The paths whose change makes a landing change the runtime, relative to
/// the repository root: a directory ends with `/`. `build.rs` embeds the
/// build identifier.
pub const RUNTIME_PATHS: &[&str] = &[
    "src/",
    "migrations/",
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
];

/// Whether any of `paths` (repository-relative) is part of the runtime.
pub fn changes_runtime(paths: &[String]) -> bool {
    paths.iter().any(|path| {
        RUNTIME_PATHS
            .iter()
            .any(|runtime| match runtime.strip_suffix('/') {
                Some(dir) => path.starts_with(runtime) || path == dir,
                None => path == runtime,
            })
    })
}

/// The commit a build identifier names: `<commit>` of
/// `X.Y.Z-dev+<commit>[.dirty]`. `None` for a release (`X.Y.Z`) or a build
/// that did not know its commit (`+unknown`).
pub fn build_commit(version: &str) -> Option<&str> {
    let (_, metadata) = version.split_once('+')?;
    let commit = metadata.strip_suffix(".dirty").unwrap_or(metadata);
    (commit != crate::build_id::UNKNOWN_COMMIT && !commit.is_empty()).then_some(commit)
}

/// Where the update keeps its checkout, target and a build waiting for a
/// person, under the queue's directory.
#[derive(Debug, Clone)]
pub struct UpdatePaths {
    /// `<queue dir>/update/checkout`: a detached worktree of the repository.
    pub checkout: PathBuf,
    /// `<queue dir>/update/target`: the checkout's `CARGO_TARGET_DIR`.
    pub target: PathBuf,
    /// `<queue dir>/update/staged/dagq`: a build with a breaking
    /// migration, kept for the `install` a person runs.
    pub staged: PathBuf,
}

impl UpdatePaths {
    pub fn under(queue_dir: &Path) -> Self {
        let root = queue_dir.join("update");
        Self {
            checkout: root.join("checkout"),
            target: root.join("target"),
            staged: root.join("staged").join("dagq"),
        }
    }
}

/// Whether `update` is a step of a job still working: it is
/// `update_started` or `update_built` and the job's process lives.
pub fn in_progress(update: &BinaryUpdate, processes: &dyn ProcessControl) -> bool {
    matches!(update.kind.as_str(), UPDATE_STARTED | UPDATE_BUILT)
        && job_pid(update).is_some_and(|pid| processes.alive(pid))
}

/// The newest step a job wrote (`updates` newest first): the answers of
/// the asks (`update_answered`, `update_retry`) are skipped, so an answer
/// written while a job still works does not hide it.
pub fn latest_job_step(updates: &[BinaryUpdate]) -> Option<&BinaryUpdate> {
    updates
        .iter()
        .find(|update| !matches!(update.kind.as_str(), UPDATE_ANSWERED | UPDATE_RETRY))
}

/// The pid of the job that wrote `update`, if it recorded one.
pub fn job_pid(update: &BinaryUpdate) -> Option<u32> {
    update
        .payload
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
}

/// The automatic update as `status` shows it: whether a live supervisor has
/// it on, and where the latest update stands (`state`: `building`,
/// `installing`, `interrupted` when its job died, `installed`, `failed`,
/// `awaiting_approval`, `retry_requested`, `skipped`; `idle` when none
/// ran), with its commit, time and details. `updates` is newest first.
pub fn status(
    registrations: &[SupervisorRegistration],
    updates: &[BinaryUpdate],
    processes: &dyn ProcessControl,
    now: i64,
) -> Value {
    let enabled = registrations.iter().any(|registration| {
        registration.auto_update
            && processes.alive(registration.pid)
            && now - registration.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS
    });
    let Some(latest) = updates.first() else {
        return json!({"enabled": enabled, "state": "idle"});
    };
    let latest = match latest_job_step(updates) {
        Some(step) if in_progress(step, processes) => step,
        _ => latest,
    };
    let state = match latest.kind.as_str() {
        UPDATE_STARTED if in_progress(latest, processes) => "building",
        UPDATE_BUILT if in_progress(latest, processes) => "installing",
        UPDATE_STARTED | UPDATE_BUILT => "interrupted",
        UPDATE_INSTALLED => "installed",
        UPDATE_FAILED => "failed",
        UPDATE_AWAITING_APPROVAL => "awaiting_approval",
        UPDATE_RETRY => "retry_requested",
        UPDATE_ANSWERED => "skipped",
        _ => "unknown",
    };
    // The commit is the one the latest job worked on; an answer's row
    // names none.
    let commit = updates.iter().find_map(|update| update.commit.clone());
    json!({
        "enabled": enabled,
        "state": state,
        "commit": commit,
        "at": latest.created_at,
        "last": latest.payload,
    })
}

/// What the supervisor decides from main and the update log on a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// Build `commit`; `base` is what the runtime change was measured from.
    Build {
        commit: String,
        base: Option<String>,
    },
    /// Nothing to build.
    Idle,
}

/// The commit an update is measured from: the one the latest job worked
/// on, else the commit this build names, else `fallback` (main when the
/// supervisor first looked).
pub fn base_commit(
    updates: &[BinaryUpdate],
    version: &str,
    fallback: Option<&str>,
) -> Option<String> {
    updates
        .iter()
        .find(|update| update.kind == UPDATE_STARTED)
        .and_then(|update| update.commit.clone())
        .or_else(|| build_commit(version).map(str::to_owned))
        .or_else(|| fallback.map(str::to_owned))
}

/// Whether the latest word in the log is a `retry` answer after the latest
/// job: build main's head again whatever it changed.
pub fn retry_requested(updates: &[BinaryUpdate]) -> bool {
    updates
        .iter()
        .find(|update| matches!(update.kind.as_str(), UPDATE_STARTED | UPDATE_RETRY))
        .is_some_and(|update| update.kind == UPDATE_RETRY)
}

/// The update job's settings: the commit to build, the supervisor that
/// asked for it, where the binary goes and how long each wait may take.
#[derive(Debug, Clone)]
pub struct JobOptions {
    pub commit: String,
    /// The supervisor that started the job, whose handoff it watches.
    pub token: String,
    /// The fixed binary to replace: the supervisor's own.
    pub target: PathBuf,
    /// A checkout of the repository the update's worktree is added from.
    pub repository: PathBuf,
    pub paths: UpdatePaths,
    /// Where the build's output is appended.
    pub log: PathBuf,
    /// A shell command in place of `cargo build --release --locked`
    /// (tests), run in the checkout with `CARGO_TARGET_DIR` set.
    pub build_command: Option<String>,
    /// The `up` arguments the question of a breaking build hands a person,
    /// beside `--db`: `--cmux`, `--claude`, `--plugin-dir`.
    pub restart: Vec<String>,
    /// How long the supervisor may take to exec the new binary (it
    /// finishes a validation or landing in progress first).
    pub handoff_timeout: Duration,
    /// How long the new supervisor may take to heartbeat on after it took
    /// its registration back (ADR-0045 decision 13).
    pub watch_timeout: Duration,
    pub poll: Duration,
    /// This process, recorded on its steps.
    pub pid: u32,
}

pub struct JobPorts<'a> {
    pub binaries: &'a dyn Binaries,
    pub files: &'a dyn RunFiles,
    pub processes: &'a dyn ProcessControl,
    pub clock: &'a dyn Clock,
    /// Opens the queue at a database path.
    pub queues: &'a dyn Fn(&Path) -> Arc<dyn QueueOpener>,
    /// Start the supervisor of this registration again with the binary
    /// in place, after it died or was stopped (ADR-0045 decision 13);
    /// what was done.
    pub restart: &'a dyn Fn(&SupervisorRegistration) -> Result<Value>,
}

/// Build `options.commit` and put it in place of the supervisor's binary
/// (see the module). Every outcome is a row of `binary_updates` and the
/// value returned: `installed`, `awaiting_approval` or `failed`; an error
/// is only a queue that could not be written.
pub fn run(ports: &JobPorts, db: &Path, options: &JobOptions) -> Result<Value> {
    let mut queue = (ports.queues)(db).open()?;
    let queue: &mut dyn Queue = &mut *queue;
    let commit = options.commit.as_str();
    let pid = options.pid;
    let paths = &options.paths;
    let built = ports
        .binaries
        .checkout(&options.repository, &paths.checkout, commit)
        .and_then(|()| {
            ports.binaries.build_into(
                &paths.checkout,
                &paths.target,
                options.build_command.as_deref(),
                &options.log,
            )
        });
    let binary = match built {
        Ok(binary) => binary,
        Err(error) => return failed(queue, options, "build", &error, json!({})),
    };
    queue.record_binary_update(
        UPDATE_BUILT,
        Some(commit),
        json!({"pid": pid, "binary": binary, "log": options.log}),
    )?;
    let schema = match ports.binaries.schema(&binary, db) {
        Ok(schema) => schema,
        Err(error) => return failed(queue, options, "check", &error, json!({})),
    };
    let breaking: Vec<i64> = schema
        .pending
        .iter()
        .filter(|migration| !migration.compatible)
        .map(|migration| migration.version)
        .collect();
    if !breaking.is_empty() {
        return match stage(ports, &binary, &paths.staged) {
            Ok(version) => awaiting_approval(queue, db, options, &version, &breaking),
            Err(error) => failed(queue, options, "check", &error, json!({})),
        };
    }
    let before = registration(&*queue, &options.token)?;
    let no_drain = || -> Result<Value> { bail!("the automatic update never drains") };
    let installed = install::install(
        &install::Ports {
            binaries: ports.binaries,
            files: ports.files,
            processes: ports.processes,
            clock: ports.clock,
            queues: ports.queues,
            down: &no_drain,
        },
        Some(db),
        &InstallOptions {
            source: Source::Binary(binary.clone()),
            target: options.target.clone(),
            allow_breaking: false,
            restart: Vec::new(),
            handoff_timeout: options.handoff_timeout,
            poll: options.poll,
        },
    );
    let report = match installed {
        Ok(report) => report,
        Err(error) => {
            // `install` put the old binary back itself if it had replaced
            // it; the supervisor may be gone with the new one.
            let supervisor = bring_back(ports, &*queue, before.as_ref())?;
            return failed(
                queue,
                options,
                "install",
                &error,
                json!({"supervisor": supervisor}),
            );
        }
    };
    let version = report["version"].as_str().unwrap_or_default().to_owned();
    if let Err(error) = watch(ports, &*queue, &options.token, &version, options) {
        let restored = restore(ports, options, report["previous_version"].as_str());
        let supervisor = bring_back(ports, &*queue, before.as_ref())?;
        return failed(
            queue,
            options,
            "watch",
            &error,
            json!({"restored": restored, "supervisor": supervisor, "version": version}),
        );
    }
    let payload = json!({
        "pid": pid,
        "version": version,
        "previous_version": report["previous_version"],
        "migrated": report["migrated"],
        "supervisors": report["supervisors"],
        "log": options.log,
    });
    queue.record_binary_update(UPDATE_INSTALLED, Some(commit), payload.clone())?;
    let mut value = payload;
    value["outcome"] = json!("installed");
    value["commit"] = json!(commit);
    Ok(value)
}

fn registration(queue: &dyn Queue, token: &str) -> Result<Option<SupervisorRegistration>> {
    Ok(queue
        .supervisors()?
        .into_iter()
        .find(|registration| registration.token == token))
}

/// Check a build that waits for a person as `install` would (its version
/// and a start on a throwaway queue) and keep a copy of it, so a later build
/// in the target does not replace what the person is asked about.
fn stage(ports: &JobPorts, binary: &Path, staged: &Path) -> Result<String> {
    let version = ports.binaries.version(binary)?;
    ports.binaries.probe(binary)?;
    if let Some(dir) = staged.parent() {
        ports
            .files
            .create_dir_all(dir)
            .with_context(|| format!("create {}", dir.display()))?;
    }
    ports
        .files
        .copy(binary, staged)
        .with_context(|| format!("keep the build at {}", staged.display()))?;
    Ok(version)
}

/// Wait for the supervisor `token` to heartbeat on under `version` after
/// the handoff: a heartbeat later than the one it took its registration
/// back with, within the watch timeout (ADR-0045 decision 13).
fn watch(
    ports: &JobPorts,
    queue: &dyn Queue,
    token: &str,
    version: &str,
    options: &JobOptions,
) -> Result<()> {
    let deadline = Instant::now() + options.watch_timeout;
    let mut first = None;
    loop {
        let Some(current) = registration(queue, token)? else {
            bail!("supervisor {token} deregistered after it took the handoff to {version}");
        };
        ensure!(
            ports.processes.alive(current.pid),
            "supervisor {token} (pid {}) exited after it took the handoff to {version}",
            current.pid
        );
        ensure!(
            current.binary_version.as_deref() == Some(version),
            "supervisor {token} runs {} instead of {version}",
            current.binary_version.as_deref().unwrap_or("(unrecorded)")
        );
        match first {
            None => first = Some(current.heartbeat_at),
            Some(first) if current.heartbeat_at > first => return Ok(()),
            Some(_) => {}
        }
        ensure!(
            Instant::now() < deadline,
            "supervisor {token} did not heartbeat within {}s of taking the handoff to {version}",
            options.watch_timeout.as_secs()
        );
        thread::sleep(options.poll);
    }
}

/// Put the replaced binary back at the target, only when `.previous` is the
/// build the supervisor ran before (ADR-0045 decision 13): a `.previous`
/// of another build was not put there by this update.
fn restore(ports: &JobPorts, options: &JobOptions, previous_version: Option<&str>) -> Value {
    let previous = previous_path(&options.target);
    let kept = ports.binaries.version(&previous).ok();
    if kept.is_none() || kept.as_deref() != previous_version {
        return json!({
            "restored": false,
            "reason": format!(
                "{} is {} rather than the replaced {}",
                previous.display(),
                kept.as_deref().unwrap_or("missing"),
                previous_version.unwrap_or("(unknown)")
            ),
        });
    }
    match ports.binaries.restore(&options.target) {
        Ok(()) => json!({"restored": true, "version": kept}),
        Err(error) => json!({"restored": false, "reason": format!("{error:#}")}),
    }
}

/// After a failed install or watch, make sure a supervisor serves the
/// queue with the binary in place: one that still heartbeats under the
/// build it had (its exec failed and it went on) is left alone; one that
/// lives but does not (the new binary hangs) is stopped; and one that is
/// gone is started again ([`JobPorts::restart`]). What was found and done.
fn bring_back(
    ports: &JobPorts,
    queue: &dyn Queue,
    before: Option<&SupervisorRegistration>,
) -> Result<Value> {
    let Some(before) = before else {
        return Ok(json!({"state": "not_registered"}));
    };
    let Some(current) = registration(queue, &before.token)? else {
        return Ok(json!({"state": "deregistered"}));
    };
    let now = ports.clock.now();
    let alive = ports.processes.alive(current.pid);
    if alive
        && now - current.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS
        && current.handoff_binary.is_none()
        && current.binary_version == before.binary_version
    {
        return Ok(json!({"state": "running", "version": current.binary_version}));
    }
    if alive {
        let _ = ports.processes.terminate(current.pid);
        let deadline = Instant::now() + Duration::from_secs(10);
        while ports.processes.alive(current.pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        if ports.processes.alive(current.pid) {
            let _ = ports.processes.kill(current.pid);
        }
    }
    Ok(match (ports.restart)(&current) {
        Ok(restarted) => json!({"state": "restarted", "stopped": alive, "restart": restarted}),
        Err(error) => json!({
            "state": "stopped",
            "stopped": alive,
            "error": format!("{error:#}"),
        }),
    })
}

/// Record `update_failed` and open the `update_failed` ask with what
/// failed and what became of the binary and the supervisor.
fn failed(
    queue: &mut dyn Queue,
    options: &JobOptions,
    stage: &str,
    error: &anyhow::Error,
    details: Value,
) -> Result<Value> {
    let commit = &options.commit;
    let short = &commit[..commit.len().min(12)];
    let error = format!("{error:#}");
    let mut situation = match stage {
        "build" => "Nothing was replaced.".to_owned(),
        "check" => "The build did not pass its check, so nothing was replaced.".to_owned(),
        _ => format!(
            "If the new binary had been put in place, the one it replaced is back at {} unless \
said otherwise below.",
            options.target.display()
        ),
    };
    if let Some(supervisor) = details.get("supervisor") {
        situation.push_str(&format!(" The supervisor: {supervisor}."));
    }
    if let Some(restored) = details.get("restored") {
        situation.push_str(&format!(" The binary: {restored}."));
    }
    let question = format!(
        "The automatic update to main's {short} failed at its {stage}: {error}\n\n{situation} \
The job's log is {}.\n\nAnswer `retry` to build main's head again at the supervisor's next check \
(after fixing what failed), or `skip` to wait for the next landing that changes the runtime. If \
no supervisor serves the queue now, `up` starts one.",
        options.log.display()
    );
    let ask = queue
        .open_update_ask(
            UPDATE_FAILED_SUBJECT,
            &question,
            UPDATE_FAILED_OPTIONS,
            UPDATE_ASKER,
        )?
        .id;
    let mut payload = json!({
        "pid": options.pid,
        "stage": stage,
        "error": error,
        "log": options.log,
        "ask_id": ask,
    });
    if let (Some(object), Value::Object(details)) = (payload.as_object_mut(), details) {
        object.extend(details);
    }
    queue.record_binary_update(UPDATE_FAILED, Some(commit), payload.clone())?;
    payload["outcome"] = json!("failed");
    payload["commit"] = json!(commit);
    Ok(payload)
}

/// Open the `approve_update` ask for a build with a breaking migration,
/// naming the command that drains and installs it.
fn awaiting_approval(
    queue: &mut dyn Queue,
    db: &Path,
    options: &JobOptions,
    version: &str,
    breaking: &[i64],
) -> Result<Value> {
    let commit = &options.commit;
    let staged = &options.paths.staged;
    let mut command = format!(
        "dagq --db {} install --from {} --to {} --allow-breaking",
        db.display(),
        staged.display(),
        options.target.display()
    );
    for argument in &options.restart {
        command.push(' ');
        command.push_str(argument);
    }
    let migrations = breaking
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let question = format!(
        "main's {} built as {version}, and it brings breaking migration(s) {migrations}: the \
running supervisor and its runs' wrappers could not open the queue after them, so it was not \
installed. Answer `install` and run `{command}` from the inbox to drain the supervisor (it waits \
for its runs), back the queue up, migrate and start it again with the new binary; or `skip` to \
leave it. The build is kept at {}.",
        &commit[..commit.len().min(12)],
        staged.display()
    );
    let ask = queue
        .open_update_ask(
            APPROVE_UPDATE_SUBJECT,
            &question,
            APPROVE_UPDATE_OPTIONS,
            UPDATE_ASKER,
        )?
        .id;
    let payload = json!({
        "pid": options.pid,
        "version": version,
        "migrations": breaking,
        "binary": staged,
        "command": command,
        "ask_id": ask,
    });
    queue.record_binary_update(UPDATE_AWAITING_APPROVAL, Some(commit), payload.clone())?;
    let mut value = payload;
    value["outcome"] = json!("awaiting_approval");
    value["commit"] = json!(commit);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_landing_changes_the_runtime_when_it_touches_its_sources_or_manifests() {
        let paths = |paths: &[&str]| paths.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();
        assert!(changes_runtime(&paths(&["docs/a.md", "src/main.rs"])));
        assert!(changes_runtime(&paths(&["migrations/0032_x.sql"])));
        assert!(changes_runtime(&paths(&["Cargo.lock"])));
        assert!(changes_runtime(&paths(&["build.rs"])));
        assert!(!changes_runtime(&paths(&["docs/src/a.md", "README.md"])));
        assert!(!changes_runtime(&paths(&["srcs/a.rs", "Cargo.toml.bak"])));
        assert!(!changes_runtime(&[]));
    }

    #[test]
    fn a_build_identifier_names_its_commit_unless_a_release_or_unknown() {
        assert_eq!(build_commit("0.4.0-dev+abc123"), Some("abc123"));
        assert_eq!(build_commit("0.4.0-dev+abc123.dirty"), Some("abc123"));
        assert_eq!(build_commit("0.4.0-dev+unknown"), None);
        assert_eq!(build_commit("0.4.0"), None);
    }
}
