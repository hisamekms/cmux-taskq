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
    process::{Command, Stdio},
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
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{label} timed out; external resources may have been created");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let stdout = out
        .join()
        .map_err(|_| anyhow::anyhow!("stdout reader failed"))??;
    let stderr = err
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader failed"))??;
    ensure!(
        status.success(),
        "{label} failed ({status}): {}",
        String::from_utf8_lossy(&stderr)
    );
    String::from_utf8(stdout).context("command output is not UTF-8")
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
