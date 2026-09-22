use crate::{
    application::{AgentProvider, WorkspaceBackend},
    domain::TaskRun,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

/// The only shell boundary is cmux's terminal startup command.
/// Quote every argument independently, including paths containing apostrophes.
pub fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|s| format!("'{}'", s.replace('\'', "'\"'\"'")))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .context("runtime paths must be valid UTF-8")
}

pub fn executable(path: &Path) -> Result<PathBuf> {
    let candidate = if path.components().count() > 1 || path.is_absolute() {
        path.to_owned()
    } else {
        env::split_paths(&env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join(path))
            .find(|p| p.is_file())
            .with_context(|| format!("{} was not found on PATH", path.display()))?
    };
    candidate
        .canonicalize()
        .with_context(|| format!("resolve executable {}", candidate.display()))
}

pub fn output(command: &mut Command) -> Result<String> {
    let (status, stdout, stderr) = capture(command, Duration::from_secs(30))?;
    ensure!(
        status.success(),
        "{:?} failed ({status}): {stderr}",
        command.get_program()
    );
    Ok(stdout)
}

/// Run to completion with a deadline; the caller interprets the exit status.
pub fn capture(command: &mut Command, timeout: Duration) -> Result<(ExitStatus, String, String)> {
    let label = format!("{:?}", command.get_program());
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("start {label}"))?;
    let mut stdout = child.stdout.take().context("stdout unavailable")?;
    let mut stderr = child.stderr.take().context("stderr unavailable")?;
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let err = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let status = wait_with_deadline(&mut child, &label, timeout)?;
    let stdout = out
        .join()
        .map_err(|_| anyhow::anyhow!("stdout reader failed"))??;
    let stderr = err
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader failed"))??;
    Ok((
        status,
        String::from_utf8(stdout).context("command output is not UTF-8")?,
        String::from_utf8_lossy(&stderr).into_owned(),
    ))
}

fn wait_with_deadline(child: &mut Child, label: &str, timeout: Duration) -> Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{label} timed out; external resources may have been created");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Verification commands are task-defined shell lines whose full output belongs
/// in the run directory, not in the event payload.
pub const VERIFICATION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub fn run_shell_to_log(script: &str, cwd: &Path, log: &Path) -> Result<ExitStatus> {
    let file = fs::File::create(log).with_context(|| format!("create {}", log.display()))?;
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file.try_clone()?))
        .stderr(Stdio::from(file))
        .spawn()
        .with_context(|| format!("start verification command {script:?}"))?;
    wait_with_deadline(
        &mut child,
        &format!("verification command {script:?}"),
        VERIFICATION_TIMEOUT,
    )
}

pub struct GitRepository {
    pub root: PathBuf,
    pub common_dir: PathBuf,
    pub base_commit: String,
    git: PathBuf,
}

impl GitRepository {
    pub fn inspect(path: &Path) -> Result<Self> {
        let git = executable(Path::new("git"))?;
        let root = PathBuf::from(
            output(
                Command::new(&git)
                    .arg("-C")
                    .arg(path)
                    .args(["rev-parse", "--show-toplevel"]),
            )?
            .trim(),
        )
        .canonicalize()?;
        let common_dir = PathBuf::from(
            output(Command::new(&git).arg("-C").arg(&root).args([
                "rev-parse",
                "--path-format=absolute",
                "--git-common-dir",
            ]))?
            .trim(),
        )
        .canonicalize()?;
        let base_commit = output(Command::new(&git).arg("-C").arg(&root).args([
            "rev-parse",
            "--verify",
            "refs/heads/main^{commit}",
        ]))?
        .trim()
        .to_owned();
        Ok(Self {
            root,
            common_dir,
            base_commit,
            git,
        })
    }

    /// Symbolic HEAD of a worktree, or None when detached.
    pub fn current_branch(&self, worktree: &Path) -> Result<Option<String>> {
        let (status, stdout, stderr) = capture(
            Command::new(&self.git).arg("-C").arg(worktree).args([
                "symbolic-ref",
                "--quiet",
                "HEAD",
            ]),
            Duration::from_secs(30),
        )?;
        match status.code() {
            Some(0) => Ok(Some(stdout.trim().to_owned())),
            Some(1) => Ok(None),
            _ => bail!("git symbolic-ref failed ({status}): {stderr}"),
        }
    }

    pub fn head(&self, worktree: &Path) -> Result<String> {
        Ok(
            output(Command::new(&self.git).arg("-C").arg(worktree).args([
                "rev-parse",
                "--verify",
                "HEAD^{commit}",
            ]))?
            .trim()
            .to_owned(),
        )
    }

    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let (status, _, stderr) = capture(
            Command::new(&self.git).arg("-C").arg(&self.root).args([
                "merge-base",
                "--is-ancestor",
                ancestor,
                descendant,
            ]),
            Duration::from_secs(30),
        )?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!("git merge-base failed ({status}): {stderr}"),
        }
    }

    /// Porcelain status including untracked files; empty means clean.
    pub fn status(&self, worktree: &Path) -> Result<String> {
        output(Command::new(&self.git).arg("-C").arg(worktree).args([
            "status",
            "--porcelain",
            "--untracked-files=all",
        ]))
    }

    pub fn create_worktree(&self, run: &TaskRun) -> Result<String> {
        output(
            Command::new(&self.git)
                .arg("-C")
                .arg(&self.root)
                .args(["worktree", "add", "-b"])
                .arg(run.branch.as_ref().context("missing branch")?)
                .arg(run.worktree_path.as_ref().context("missing worktree")?)
                .arg(&run.base_commit),
        )
    }
}

pub struct Cmux {
    pub executable: PathBuf,
}

impl WorkspaceBackend for Cmux {
    fn preflight(&self) -> Result<()> {
        let reply = output(Command::new(&self.executable).arg("ping"))?;
        ensure!(
            reply.trim() == "PONG",
            "unexpected cmux ping response: {reply}"
        );
        Ok(())
    }

    fn create(&self, run: &TaskRun, command: &str) -> Result<String> {
        let raw = output(
            Command::new(&self.executable)
                .arg("new-workspace")
                .arg("--name")
                .arg(format!("taskq {} {}", run.task_id, run.id))
                .arg("--cwd")
                .arg(run.worktree_path.as_ref().context("missing worktree")?)
                .arg("--command")
                .arg(command)
                .args(["--focus", "false"]),
        )?;
        // Persist the returned handle before resolving its stable UUID.
        fs::write(
            Path::new(run.run_dir.as_ref().context("missing run directory")?)
                .join("workspace-create.txt"),
            &raw,
        )?;
        let handle = workspace_handle(&raw)?;
        let identity = output(
            Command::new(&self.executable)
                .args(["--json", "--id-format", "uuids", "identify", "--workspace"])
                .arg(handle),
        )?;
        let identity: Value =
            serde_json::from_str(&identity).context("decode cmux workspace identity")?;
        let id = identity
            .pointer("/caller/workspace_id")
            .and_then(Value::as_str)
            .context("cmux did not return the requested workspace UUID")?;
        uuid::Uuid::parse_str(id).context("invalid cmux workspace UUID")?;
        Ok(id.into())
    }

    fn capture(&self, workspace_id: &str) -> Result<String> {
        output(Command::new(&self.executable).args([
            "read-screen",
            "--workspace",
            workspace_id,
            "--scrollback",
            "--lines",
            "2000",
        ]))
    }
}

pub fn workspace_handle(raw: &str) -> Result<&str> {
    let handle = raw
        .lines()
        .find_map(|line| line.strip_prefix("OK "))
        .context("unrecognized cmux workspace creation response; inspect workspace-create.txt")?
        .trim();
    let suffix = handle
        .strip_prefix("workspace:")
        .context("unexpected cmux workspace handle")?;
    ensure!(
        !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()),
        "invalid cmux workspace handle"
    );
    Ok(handle)
}

pub struct ClaudeCode {
    pub executable: PathBuf,
}

impl AgentProvider for ClaudeCode {
    fn preflight(&self) -> Result<()> {
        output(Command::new(&self.executable).arg("--version"))?;
        Ok(())
    }

    fn command(&self, run: &TaskRun, prompt: &str) -> Result<Command> {
        let mut command = Command::new(&self.executable);
        command
            .current_dir(run.worktree_path.as_ref().context("missing worktree")?)
            .arg("--session-id")
            .arg(&run.id)
            .arg("--debug-file")
            .arg(run.log_path.as_ref().context("missing log path")?)
            .arg("--add-dir")
            .arg(run.run_dir.as_ref().context("missing run directory")?)
            .arg("--")
            .arg(prompt)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        Ok(command)
    }
}
