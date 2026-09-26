//! [`Binaries`] on this machine: `cargo build --release --locked` in a
//! checkout, a binary run as a child process for its version, a throwaway
//! queue and its migrations, and the replacement of a file by a rename in
//! its directory (ADR-0045 decisions 11, 12).

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::application::install::{
    Binaries, PendingMigration, SchemaCheck, parse_version, previous_path,
};

pub struct LocalBinaries;

/// Run `binary` with `arguments`; its stdout when it exits 0, or an error
/// with what it wrote to stderr.
fn output(binary: &Path, arguments: &[&str]) -> Result<String> {
    let output = Command::new(binary)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run {}", binary.display()))?;
    ensure!(
        output.status.success(),
        "{} {} exited with {}: {}",
        binary.display(),
        arguments.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn json_output(binary: &Path, arguments: &[&str]) -> Result<Value> {
    let text = output(binary, arguments)?;
    serde_json::from_str(&text).with_context(|| format!("read what {} printed", binary.display()))
}

fn text(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("{} is not UTF-8", path.display()))
}

impl Binaries for LocalBinaries {
    fn build(&self, checkout: &Path) -> Result<PathBuf> {
        let status = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["build", "--release", "--locked"])
            .current_dir(checkout)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .context("run cargo build")?;
        ensure!(
            status.success(),
            "cargo build --release exited with {status}"
        );
        let target = match env::var_os("CARGO_TARGET_DIR") {
            Some(dir) => checkout.join(dir),
            None => checkout.join("target"),
        };
        Ok(target.join("release").join("dagq"))
    }

    fn version(&self, binary: &Path) -> Result<String> {
        parse_version(&output(binary, &["--version"])?)
    }

    fn probe(&self, binary: &Path) -> Result<()> {
        let dir = env::temp_dir().join(format!(
            "dagq-install-probe-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let db = dir.join("queue.db");
        let result = (|| {
            json_output(binary, &["--db", text(&db)?, "init"])?;
            json_output(binary, &["--db", text(&db)?, "list"])?;
            Ok(())
        })();
        let _ = fs::remove_dir_all(&dir);
        result
    }

    fn takes_handoff(&self, binary: &Path) -> bool {
        // Clap refuses an unknown argument before it prints the help.
        output(binary, &["supervise", "--handoff-token", "probe", "--help"]).is_ok()
    }

    fn schema(&self, binary: &Path, db: &Path) -> Result<SchemaCheck> {
        let report = json_output(binary, &["--db", text(db)?, "migrate", "--check"])?;
        let pending = report["pending"]
            .as_array()
            .context("migrate --check reported no pending list")?
            .iter()
            .map(|migration| {
                Ok(PendingMigration {
                    version: migration["version"]
                        .as_i64()
                        .context("a pending migration without a version")?,
                    compatible: migration["compatible"] == true,
                })
            })
            .collect::<Result<_>>()?;
        Ok(SchemaCheck {
            pending,
            opens: report["opens"] == true,
        })
    }

    fn migrate(&self, binary: &Path, db: &Path) -> Result<Value> {
        json_output(binary, &["--db", text(db)?, "migrate"])
    }

    fn replace(&self, source: &Path, target: &Path) -> Result<()> {
        let dir = target
            .parent()
            .with_context(|| format!("{} has no directory", target.display()))?;
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let name = target
            .file_name()
            .with_context(|| format!("{} has no file name", target.display()))?
            .to_string_lossy()
            .into_owned();
        let pid = std::process::id();
        let staged = dir.join(format!(".{name}.install-{pid}"));
        fs::copy(source, &staged)
            .with_context(|| format!("copy {} to {}", source.display(), staged.display()))?;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
        if !target.exists() {
            fs::rename(&staged, target)
                .with_context(|| format!("move the new binary to {}", target.display()))?;
            return Ok(());
        }
        // The binary in place keeps its inode (a running process keeps
        // using it) under a second name, which then becomes `.previous`.
        let kept = dir.join(format!(".{name}.previous-{pid}"));
        let _ = fs::remove_file(&kept);
        if fs::hard_link(target, &kept).is_err() {
            fs::copy(target, &kept)
                .with_context(|| format!("keep {} as {}", target.display(), kept.display()))?;
        }
        fs::rename(&staged, target)
            .with_context(|| format!("move the new binary to {}", target.display()))?;
        fs::rename(&kept, previous_path(target))
            .with_context(|| format!("keep the replaced binary next to {}", target.display()))?;
        Ok(())
    }

    fn restore(&self, target: &Path) -> Result<()> {
        let previous = previous_path(target);
        if !previous.is_file() {
            bail!("no previous binary at {}", previous.display());
        }
        fs::rename(&previous, target)
            .with_context(|| format!("move {} back to {}", previous.display(), target.display()))
    }

    fn run(&self, binary: &Path, arguments: &[String]) -> Result<Value> {
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        json_output(binary, &arguments)
    }
}
