//! `dagq install`: a person's way to replace the fixed binary and the
//! queue's supervisor with it, without waiting for the sessions (ADR-0045
//! decisions 11–14). The new binary is built (or given), checked, the
//! queue's compatible migrations applied with it, and put in place of the
//! old one by a rename that keeps the old one as `<name>.previous`; then
//! every live supervisor that takes a handoff is asked to exec it. A build
//! whose migrations would break the old binary goes through the drain
//! instead, and only when asked to. `--rollback` does the same with
//! `<name>.previous`.
//!
//! The binaries are built, run and moved through [`Binaries`], the queue
//! is read through [`QueueOpener`]; [`crate::compose`] builds the adapters.

use super::{Clock, ProcessControl, Queue, QueueOpener, RunFiles, lifecycle::hand_off, path_text};
use crate::domain::{HEARTBEAT_TIMEOUT_SECS, SupervisorMode, SupervisorRegistration};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// One migration a binary would apply to a queue (`migrate --check`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMigration {
    pub version: i64,
    pub compatible: bool,
}

/// What a binary reports about a queue's schema (`migrate --check`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaCheck {
    pub pending: Vec<PendingMigration>,
    /// Whether the binary opens the queue as it is.
    pub opens: bool,
}

/// Building, running and moving `dagq` binaries.
pub trait Binaries {
    /// Build the checkout for release; the binary built.
    fn build(&self, checkout: &Path) -> Result<PathBuf>;
    /// The build identifier `binary --version` reports.
    fn version(&self, binary: &Path) -> Result<String>;
    /// Start `binary` on a throwaway queue (`init` and a read).
    fn probe(&self, binary: &Path) -> Result<()>;
    /// Whether `binary` continues a supervisor after an exec (it knows
    /// `supervise --handoff-token`); a binary before ADR-0045 exits on it.
    fn takes_handoff(&self, binary: &Path) -> bool;
    /// `binary --db <db> migrate --check`.
    fn schema(&self, binary: &Path, db: &Path) -> Result<SchemaCheck>;
    /// `binary --db <db> migrate`, its report.
    fn migrate(&self, binary: &Path, db: &Path) -> Result<Value>;
    /// Put a copy of `source` at `target` by a rename in `target`'s
    /// directory, keeping what was there as [`previous_path`] (ADR-0045
    /// decision 11).
    fn replace(&self, source: &Path, target: &Path) -> Result<()>;
    /// Put [`previous_path`] back at `target` after a failed handoff.
    fn restore(&self, target: &Path) -> Result<()>;
    /// Run `binary` with `arguments`; what it printed.
    fn run(&self, binary: &Path, arguments: &[String]) -> Result<Value>;
    /// Point the automatic update's `checkout` at `commit`: a detached
    /// worktree of the repository `repository` belongs to, added when
    /// missing (ADR-0045 decision 17).
    fn checkout(&self, repository: &Path, checkout: &Path, commit: &str) -> Result<()> {
        let _ = (repository, checkout, commit);
        bail!("these binaries cannot check a commit out")
    }
    /// Build `checkout` for release into `target_dir`, running `command`
    /// (a shell command) in place of `cargo build --release --locked` when
    /// given, with what it prints appended to `log`; the binary built.
    fn build_into(
        &self,
        checkout: &Path,
        target_dir: &Path,
        command: Option<&str>,
        log: &Path,
    ) -> Result<PathBuf> {
        let _ = (checkout, target_dir, command, log);
        bail!("these binaries cannot build a checkout")
    }
}

/// Where the binary `install` puts in place comes from.
#[derive(Debug, Clone)]
pub enum Source {
    /// Build this checkout.
    Checkout(PathBuf),
    /// A binary built already.
    Binary(PathBuf),
    /// The binary the last install replaced (`<target>.previous`).
    Rollback,
}

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub source: Source,
    /// The fixed binary to replace.
    pub target: PathBuf,
    /// Go through the drain when the new binary's migrations would break
    /// the old one.
    pub allow_breaking: bool,
    /// Arguments of the `up` that starts the supervisor again after such a
    /// drain, beside `--db`, `--parallel` and `--in-cmux` (taken from the
    /// drained supervisor): `--cmux`, `--claude`, `--plugin-dir`.
    pub restart: Vec<String>,
    pub handoff_timeout: Duration,
    pub poll: Duration,
}

pub struct Ports<'a> {
    pub binaries: &'a dyn Binaries,
    pub files: &'a dyn RunFiles,
    pub processes: &'a dyn ProcessControl,
    pub clock: &'a dyn Clock,
    /// Opens the queue at a database path.
    pub queues: &'a dyn Fn(&Path) -> Arc<dyn QueueOpener>,
    /// `down --wait` on the queue, for a breaking migration's drain.
    pub down: &'a dyn Fn() -> Result<Value>,
}

/// Where the binary a replacement kept goes: `<target>.previous`.
pub fn previous_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".previous");
    target.with_file_name(name)
}

/// Replace `options.target` with the binary of `options.source` and hand
/// the queue at `db`'s live supervisors over to it (see the module). `db`
/// is `None`, or names no file, outside a queue: then only the binary is
/// replaced.
pub fn install(ports: &Ports, db: Option<&Path>, options: &InstallOptions) -> Result<Value> {
    let Ports {
        binaries, files, ..
    } = *ports;
    let target = &options.target;
    let previous = previous_path(target);
    let source = match &options.source {
        Source::Checkout(checkout) => binaries
            .build(checkout)
            .with_context(|| format!("build {}", checkout.display()))?,
        Source::Binary(binary) => binary.clone(),
        Source::Rollback => {
            ensure!(
                files.is_file(&previous),
                "no previous binary at {} to roll back to",
                previous.display()
            );
            previous.clone()
        }
    };
    let version = binaries
        .version(&source)
        .with_context(|| format!("{} does not report its version", source.display()))?;
    binaries
        .probe(&source)
        .with_context(|| format!("{} does not start on a throwaway queue", source.display()))?;
    let replaced_version = if files.is_file(target) {
        binaries.version(target).ok()
    } else {
        None
    };
    let db = db.filter(|db| files.is_file(db));
    let Some(db) = db else {
        binaries.replace(&source, target)?;
        return Ok(json!({
            "outcome": "installed",
            "target": target,
            "version": version,
            "previous": previous,
            "previous_version": replaced_version,
            "migrated": Value::Null,
            "supervisors": [],
        }));
    };
    let schema = binaries.schema(&source, db)?;
    let breaking: Vec<String> = schema
        .pending
        .iter()
        .filter(|migration| !migration.compatible)
        .map(|migration| migration.version.to_string())
        .collect();
    if !breaking.is_empty() {
        ensure!(
            options.allow_breaking,
            "{version} brings breaking migration(s) {} the running supervisor and its runs' \
wrappers could not open the queue after: nothing was replaced. `install --allow-breaking` drains \
the supervisor (waits for its runs), migrates with a backup and starts it again",
            breaking.join(", ")
        );
        return install_breaking(ports, db, &source, &version, replaced_version, options);
    }
    ensure!(
        schema.opens || !schema.pending.is_empty(),
        "the queue refuses {version}: a breaking migration after it was applied, so it cannot \
open the queue again; nothing was replaced. The queue as it was before that migration is in the \
backups/ directory next to it"
    );
    let migrated = if schema.pending.is_empty() {
        Value::Null
    } else {
        binaries.migrate(&source, db)?
    };
    let queue = (ports.queues)(db).open()?;
    let live = live_supervisors(ports, &*queue)?;
    let (takes, cannot): (Vec<_>, Vec<_>) = live
        .into_iter()
        .partition(|registration| registration.handoff_accepted);
    // An exec of a binary that does not know the handoff would end the
    // supervisor and orphan its sessions' leases.
    ensure!(
        takes.is_empty() || binaries.takes_handoff(&source),
        "{version} cannot continue a supervisor after an exec (it predates the handoff), so the \
running supervisor cannot be handed over to it; nothing was replaced. Stop the supervisor \
(`down --wait`), put the binary in place and run `up`"
    );
    binaries.replace(&source, target)?;
    let handed = if takes.is_empty() {
        Vec::new()
    } else {
        match hand_off(
            &*queue,
            ports.processes,
            ports.clock,
            &takes,
            target,
            &version,
            options.handoff_timeout,
            options.poll,
        ) {
            Ok(handed) => handed,
            Err(error) => {
                let restored = binaries.restore(target);
                return Err(match restored {
                    Ok(()) => error.context(format!(
                        "the handoff to {version} failed; the binary it replaced is back at {}",
                        target.display()
                    )),
                    Err(restore) => error.context(format!(
                        "the handoff to {version} failed, and the binary it replaced could not be \
put back at {} ({restore:#}); it is at {}",
                        target.display(),
                        previous.display()
                    )),
                });
            }
        }
    };
    Ok(json!({
        "outcome": "installed",
        "target": target,
        "version": version,
        "previous": previous,
        "previous_version": replaced_version,
        "migrated": migrated,
        "supervisors": handed,
        // A supervisor of a binary before ADR-0045 takes no handoff;
        // `up` drains and replaces it.
        "not_handed_off": cannot
            .iter()
            .map(|registration| json!({
                "token": registration.token,
                "pid": registration.pid,
                "version": registration.binary_version,
                "next": "run `up` to drain and replace it",
            }))
            .collect::<Vec<_>>(),
    }))
}

/// The drain of ADR-0045 decision 14: stop the live supervisors and wait
/// for their runs (`down --wait`), migrate with the new binary (which
/// backs the queue up first), replace the binary, and start the supervisor
/// again with the new binary's `up`, in the mode and with the parallelism
/// the drained one had.
fn install_breaking(
    ports: &Ports,
    db: &Path,
    source: &Path,
    version: &str,
    replaced_version: Option<String>,
    options: &InstallOptions,
) -> Result<Value> {
    let binaries = ports.binaries;
    let live = {
        let queue = (ports.queues)(db).open()?;
        live_supervisors(ports, &*queue)?
    };
    let drained = if live.is_empty() {
        Value::Null
    } else {
        (ports.down)()?
    };
    let migrated = binaries.migrate(source, db).with_context(|| {
        format!(
            "the supervisor was drained but {version} could not migrate the queue; nothing was \
replaced, and `up` starts the old binary again"
        )
    })?;
    binaries.replace(source, &options.target)?;
    let started = match live.first() {
        None => Value::Null,
        Some(drained) => {
            let mut arguments = vec![
                "--db".to_owned(),
                path_text(db)?,
                "up".to_owned(),
                "--parallel".to_owned(),
                drained.parallel.to_string(),
            ];
            if live
                .iter()
                .any(|registration| registration.mode == Some(SupervisorMode::InCmux))
            {
                arguments.push("--in-cmux".to_owned());
            }
            arguments.extend(options.restart.iter().cloned());
            binaries.run(&options.target, &arguments).with_context(|| {
                format!(
                    "{version} is installed and the queue migrated, but its `up` failed; run it \
again"
                )
            })?
        }
    };
    Ok(json!({
        "outcome": "installed",
        "target": options.target,
        "version": version,
        "previous": previous_path(&options.target),
        "previous_version": replaced_version,
        "migrated": migrated,
        "drained": drained,
        "up": started,
    }))
}

/// The registrations whose process lives and heartbeats: the ones a
/// replacement asks (a silent one cannot pick the request up, ADR-0045
/// decision 16).
fn live_supervisors(ports: &Ports, queue: &dyn Queue) -> Result<Vec<SupervisorRegistration>> {
    let now = ports.clock.now();
    Ok(queue
        .supervisors()?
        .into_iter()
        .filter(|registration| {
            ports.processes.alive(registration.pid)
                && now - registration.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS
        })
        .collect())
}

/// The version `dagq --version` printed: its last word.
pub fn parse_version(output: &str) -> Result<String> {
    match output.split_whitespace().last() {
        Some(version) => Ok(version.to_owned()),
        None => bail!("no version in {output:?}"),
    }
}
