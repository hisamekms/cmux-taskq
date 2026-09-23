use crate::{
    application::{
        AgentProvider, DetachedRefusal, MainRemote, ProcessControl, SupervisorEnvironment,
        WorkspaceBackend, WorkspaceTags,
    },
    domain::{SessionRole, Task, TaskRun},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    env,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

/// Shell boundaries are cmux's terminal startup command and Claude's hook command.
/// Quote every argument independently, including paths containing apostrophes.
pub fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|s| shell_quote(s))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\"'\"'"))
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

/// `kill -0` semantics: a process we may not signal (EPERM) still exists.
/// Run `command`, an outer shell that backgrounds a process printing
/// `pid=N` as its first stdout line and exits at once, and return what that
/// process wrote after the pid line and to stderr once it is gone (its exit
/// closes the pipes). The orphan is not this process's child, so a deadline
/// is kept by hand and the pid is killed when it passes.
fn orphan_output(command: &mut Command, timeout: Duration) -> Result<(String, String)> {
    let label = format!("{:?}", command.get_program());
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("start {label}"))?;
    let stdout = child.stdout.take().context("stdout unavailable")?;
    let mut stderr = child.stderr.take().context("stderr unavailable")?;
    let (lines, received) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let err = thread::spawn(move || {
        let mut text = String::new();
        stderr.read_to_string(&mut text).map(|_| text)
    });
    let status = child.wait().with_context(|| format!("wait for {label}"))?;
    ensure!(status.success(), "{label} failed ({status})");
    let deadline = Instant::now() + timeout;
    let mut pid = None;
    let mut reply = Vec::new();
    loop {
        match received.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(line) => {
                let line = line.context("read the orphan's stdout")?;
                if pid.is_some() {
                    reply.push(line);
                } else {
                    pid = Some(
                        line.strip_prefix("pid=")
                            .and_then(|pid| pid.parse::<u32>().ok())
                            .with_context(|| format!("{label} did not report a pid: {line}"))?,
                    );
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                if let Some(pid) = pid {
                    let _ = signal(pid, libc::SIGKILL);
                }
                anyhow::bail!("{label} did not finish within {timeout:?}");
            }
        }
    }
    let stderr = err
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader failed"))?
        .context("read the orphan's stderr")?;
    Ok((reply.join("\n"), stderr))
}

pub fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 performs no action beyond the existence and permission check.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Signals through `libc::kill`, the way `process_alive` checks liveness.
pub struct SystemProcesses;

impl ProcessControl for SystemProcesses {
    fn alive(&self, pid: u32) -> bool {
        process_alive(pid)
    }

    fn terminate(&self, pid: u32) -> Result<()> {
        signal(pid, libc::SIGTERM)
    }

    fn interrupt(&self, pid: u32) -> Result<()> {
        signal(pid, libc::SIGINT)
    }

    fn kill(&self, pid: u32) -> Result<()> {
        signal(pid, libc::SIGKILL)
    }
}

fn signal(pid: u32, signal: libc::c_int) -> Result<()> {
    let pid = libc::pid_t::try_from(pid).context("pid does not fit a pid_t")?;
    // SAFETY: kill(2) with a valid pid and signal has no memory effects here.
    ensure!(
        unsafe { libc::kill(pid, signal) } == 0,
        "signal {signal} to pid {pid}: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

/// How long [`output`] lets a command run, and so each cmux call.
pub const OUTPUT_TIMEOUT: Duration = Duration::from_secs(30);

/// The 1-minute load average (getloadavg(3)); `None` when it is unavailable.
pub fn load_average() -> Option<f64> {
    let mut loads = [0f64; 1];
    // SAFETY: getloadavg writes at most `nelem` doubles into the buffer.
    let written = unsafe { libc::getloadavg(loads.as_mut_ptr(), 1) };
    (written >= 1 && loads[0].is_finite()).then_some(loads[0])
}

pub fn output(command: &mut Command) -> Result<String> {
    let (status, stdout, stderr) = capture(command, OUTPUT_TIMEOUT)?;
    ensure!(
        status.success(),
        "{:?} failed ({status}): {stderr}",
        command.get_program()
    );
    Ok(stdout)
}

/// Run to completion with a deadline; the caller interprets the exit status.
pub fn capture(command: &mut Command, timeout: Duration) -> Result<(ExitStatus, String, String)> {
    let (status, stdout, stderr) = capture_bytes(command, timeout)?;
    Ok((
        status,
        String::from_utf8(stdout).context("command output is not UTF-8")?,
        stderr,
    ))
}

/// [`capture`] without the UTF-8 requirement on stdout.
fn capture_bytes(
    command: &mut Command,
    timeout: Duration,
) -> Result<(ExitStatus, Vec<u8>, String)> {
    let label = format!("{:?}", command.get_program());
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("start {label}"))?;
    let mut stdout = child.stdout.take().context("stdout unavailable")?;
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let err = read_stderr(&mut child)?;
    let status = wait_with_deadline(&mut child, &label, timeout)?;
    let stdout = out
        .join()
        .map_err(|_| anyhow::anyhow!("stdout reader failed"))??;
    Ok((status, stdout, join_stderr(err)?))
}

type StderrReader = thread::JoinHandle<std::io::Result<Vec<u8>>>;

fn read_stderr(child: &mut Child) -> Result<StderrReader> {
    let mut stderr = child.stderr.take().context("stderr unavailable")?;
    Ok(thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    }))
}

fn join_stderr(reader: StderrReader) -> Result<String> {
    let stderr = reader
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader failed"))??;
    Ok(String::from_utf8_lossy(&stderr).into_owned())
}

/// Deadline of the Git commands that gather a review: a large diff takes far
/// longer to produce than the 30 seconds of [`output`].
pub const REVIEW_TIMEOUT: Duration = Duration::from_secs(300);

/// Like [`output`] with [`REVIEW_TIMEOUT`], reading stdout lossily so text in
/// any encoding (Latin-1 files, non-UTF-8 commit messages) does not fail.
fn review_output(command: &mut Command) -> Result<String> {
    let (status, stdout, stderr) = capture_bytes(command, REVIEW_TIMEOUT)?;
    ensure!(
        status.success(),
        "{:?} failed ({status}): {stderr}",
        command.get_program()
    );
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// Run `command` with its stdout appended to `file` as raw bytes, never
/// holding it in memory, under [`REVIEW_TIMEOUT`].
fn review_output_to(command: &mut Command, file: &fs::File) -> Result<()> {
    let label = format!("{:?}", command.get_program());
    let mut child = command
        .stdout(Stdio::from(file.try_clone()?))
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("start {label}"))?;
    let err = read_stderr(&mut child)?;
    let status = wait_with_deadline(&mut child, &label, REVIEW_TIMEOUT)?;
    let stderr = join_stderr(err)?;
    ensure!(status.success(), "{label} failed ({status}): {stderr}");
    Ok(())
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

pub fn run_shell_to_log(
    script: &str,
    cwd: &Path,
    env: &[(String, String)],
    log: &Path,
) -> Result<ExitStatus> {
    let file = fs::File::create(log).with_context(|| format!("create {}", log.display()))?;
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .envs(env.iter().map(|(key, value)| (key, value)))
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

/// Canonical Git common directory of the repository containing `path`. Every
/// worktree of a repository, including run worktrees, resolves to the same one.
pub fn git_common_dir(path: &Path) -> Result<PathBuf> {
    let git = executable(Path::new("git"))?;
    let raw = output(Command::new(&git).arg("-C").arg(path).args([
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ]))
    .with_context(|| format!("{} is not inside a Git repository", path.display()))?;
    PathBuf::from(raw.trim())
        .canonicalize()
        .context("resolve Git common directory")
}

fn main_head(git: &Path, root: &Path) -> Result<String> {
    Ok(output(Command::new(git).arg("-C").arg(root).args([
        "rev-parse",
        "--verify",
        "refs/heads/main^{commit}",
    ]))?
    .trim()
    .to_owned())
}

/// The size of a diff, as `git diff --numstat` counts it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct DiffNumbers {
    pub files_changed: u64,
    pub insertions: u64,
    pub deletions: u64,
}

#[derive(Clone)]
pub struct GitRepository {
    pub root: PathBuf,
    pub common_dir: PathBuf,
    /// `refs/heads/main` at inspection time; `main_head` rereads it.
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
        let common_dir = git_common_dir(&root)?;
        let base_commit = main_head(&git, &root)?;
        Ok(Self {
            root,
            common_dir,
            base_commit,
            git,
        })
    }

    /// Current `refs/heads/main`, read again so that a task unblocked by an
    /// integration starts from the main that contains its predecessor.
    pub fn main_head(&self) -> Result<String> {
        main_head(&self.git, &self.root)
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

    /// Whether a `git rebase` was left half-done in the worktree (by a
    /// crashed landing or an unfinished session).
    pub fn rebase_in_progress(&self, worktree: &Path) -> Result<bool> {
        let paths = output(Command::new(&self.git).arg("-C").arg(worktree).args([
            "rev-parse",
            "--git-path",
            "rebase-merge",
            "--git-path",
            "rebase-apply",
        ]))?;
        Ok(paths.lines().any(|p| worktree.join(p.trim()).exists()))
    }

    pub fn rebase_abort(&self, worktree: &Path) -> Result<()> {
        output(
            Command::new(&self.git)
                .arg("-C")
                .arg(worktree)
                .args(["rebase", "--abort"]),
        )?;
        Ok(())
    }

    /// Rebase the worktree's branch onto `onto`. `Ok(Err(output))` is a
    /// conflict (or any other rebase failure) with the rebase still in
    /// progress if Git left one; the caller decides whether to abort it.
    pub fn rebase(&self, worktree: &Path, onto: &str) -> Result<std::result::Result<(), String>> {
        let (status, stdout, stderr) = capture(
            Command::new(&self.git)
                .arg("-C")
                .arg(worktree)
                .env("GIT_TERMINAL_PROMPT", "0")
                .args(["rebase", "--no-autostash", "--no-verify", onto]),
            Duration::from_secs(10 * 60),
        )?;
        Ok(if status.success() {
            Ok(())
        } else {
            Err(format!("{stdout}{stderr}"))
        })
    }

    /// Paths with unresolved conflicts in the worktree.
    pub fn conflicted_files(&self, worktree: &Path) -> Result<Vec<String>> {
        Ok(
            output(Command::new(&self.git).arg("-C").arg(worktree).args([
                "diff",
                "--name-only",
                "--diff-filter=U",
            ]))?
            .lines()
            .map(str::to_owned)
            .collect(),
        )
    }

    /// `git log --oneline <base>..<head>`: the commits a run added. Messages
    /// that are not UTF-8 are read lossily.
    pub fn log_oneline(&self, base: &str, head: &str) -> Result<String> {
        review_output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "log",
            "--oneline",
            "--no-decorate",
            "--no-color",
            &format!("{base}..{head}"),
        ]))
    }

    /// The task IDs in the `Dagq-Task` trailers of `<base>..<head>`, oldest
    /// landing first: the tasks `integrate` put on `main` since `base`.
    pub fn landed_task_ids(&self, base: &str, head: &str) -> Result<Vec<i64>> {
        let text = review_output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "log",
            "--reverse",
            "--format=%(trailers:key=Dagq-Task,valueonly)",
            &format!("{base}..{head}"),
        ]))?;
        let mut ids: Vec<i64> = Vec::new();
        for id in text.lines().filter_map(|line| line.trim().parse().ok()) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// `git diff <args> <base>...<head>`: the change since the merge base,
    /// without color, external diff drivers or textconv filters.
    fn diff_since(&self, base: &str, head: &str, args: &[&str]) -> Command {
        let mut command = Command::new(&self.git);
        command
            .arg("-C")
            .arg(&self.root)
            .args(["diff", "--no-color", "--no-ext-diff", "--no-textconv"])
            .args(args)
            .arg(format!("{base}...{head}"))
            .arg("--");
        command
    }

    /// `git diff --stat <base>...<head>`, read lossily.
    pub fn diff_stat(&self, base: &str, head: &str) -> Result<String> {
        review_output(&mut self.diff_since(base, head, &["--stat"]))
    }

    /// Full `git diff <base>...<head>` appended to `file` as Git's raw bytes,
    /// whatever the encoding of the files; the diff never passes through memory.
    pub fn diff_to(&self, base: &str, head: &str, file: &fs::File) -> Result<()> {
        review_output_to(&mut self.diff_since(base, head, &[]), file)
    }

    /// Files changed, lines inserted and lines deleted in
    /// `<base>...<head>`, summed from `--numstat` (binary files count as a
    /// changed file with no lines).
    pub fn diff_numbers(&self, base: &str, head: &str) -> Result<DiffNumbers> {
        let numstat = review_output(&mut self.diff_since(base, head, &["--numstat"]))?;
        let mut numbers = DiffNumbers::default();
        for line in numstat.lines().filter(|line| !line.is_empty()) {
            let mut fields = line.split('\t');
            numbers.files_changed += 1;
            numbers.insertions += fields.next().and_then(|n| n.parse().ok()).unwrap_or(0);
            numbers.deletions += fields.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        }
        Ok(numbers)
    }

    pub fn tree_of(&self, commit: &str) -> Result<String> {
        Ok(
            output(Command::new(&self.git).arg("-C").arg(&self.root).args([
                "rev-parse",
                "--verify",
                &format!("{commit}^{{tree}}"),
            ]))?
            .trim()
            .to_owned(),
        )
    }

    /// One commit with `tree` on top of `parent`; `paragraphs` become the
    /// message separated by blank lines. No hook runs and no checkout changes.
    pub fn commit_tree(&self, tree: &str, parent: &str, paragraphs: &[String]) -> Result<String> {
        let mut command = Command::new(&self.git);
        command
            .arg("-C")
            .arg(&self.root)
            .args(["commit-tree", tree, "-p", parent]);
        for paragraph in paragraphs {
            command.arg("-m").arg(paragraph);
        }
        Ok(output(&mut command)?.trim().to_owned())
    }

    pub fn update_ref(&self, name: &str, value: &str) -> Result<()> {
        output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "update-ref",
            name,
            value,
        ]))?;
        Ok(())
    }

    pub fn ref_exists(&self, name: &str) -> Result<Option<String>> {
        let (status, stdout, stderr) = capture(
            Command::new(&self.git).arg("-C").arg(&self.root).args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{name}^{{commit}}"),
            ]),
            Duration::from_secs(30),
        )?;
        match status.code() {
            Some(0) => Ok(Some(stdout.trim().to_owned())),
            Some(1) => Ok(None),
            _ => bail!("git rev-parse failed ({status}): {stderr}"),
        }
    }

    /// `git worktree list --porcelain` as (path, block) pairs, the main
    /// working tree first.
    fn worktrees(&self) -> Result<Vec<(PathBuf, String)>> {
        let listing = output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "worktree",
            "list",
            "--porcelain",
        ]))?;
        Ok(listing
            .split("\n\n")
            .filter_map(|block| {
                let path = block.lines().next()?.strip_prefix("worktree ")?;
                Some((PathBuf::from(path), block.to_owned()))
            })
            .collect())
    }

    /// The worktree that has `main` checked out, if any.
    pub fn main_checkout(&self) -> Result<Option<PathBuf>> {
        Ok(self
            .worktrees()?
            .into_iter()
            .find(|(_, block)| block.lines().any(|line| line == "branch refs/heads/main"))
            .map(|(path, _)| path))
    }

    /// The main working tree, from which linked worktrees are administered;
    /// `root` may itself be the linked worktree being removed.
    fn primary_worktree(&self) -> Result<PathBuf> {
        Ok(self
            .worktrees()?
            .into_iter()
            .next()
            .map(|(path, _)| path)
            .unwrap_or_else(|| self.root.clone()))
    }

    /// Fast-forward `refs/heads/main` from `from` to `to`. Where `main` is
    /// checked out the merge goes through that worktree so its index and
    /// files move with the ref (local changes that collide make it fail);
    /// otherwise the ref is updated with `from` as the expected old value.
    pub fn advance_main(&self, from: &str, to: &str) -> Result<()> {
        match self.main_checkout()? {
            Some(checkout) => {
                output(
                    Command::new(&self.git)
                        .arg("-C")
                        .arg(&checkout)
                        .env("GIT_TERMINAL_PROMPT", "0")
                        .args(["merge", "--ff-only", to]),
                )
                .with_context(|| format!("fast-forward main in {}", checkout.display()))?;
            }
            None => {
                output(Command::new(&self.git).arg("-C").arg(&self.root).args([
                    "update-ref",
                    "refs/heads/main",
                    to,
                    from,
                ]))?;
            }
        }
        Ok(())
    }

    /// Point the repository's record of a linked worktree back at `worktree`
    /// after the directory moved (`git worktree repair`); a no-op otherwise.
    pub fn repair_worktree(&self, worktree: &Path) -> Result<()> {
        let primary = self.primary_worktree()?;
        output(
            Command::new(&self.git)
                .arg("-C")
                .arg(&primary)
                .args(["worktree", "repair"])
                .arg(worktree),
        )
        .with_context(|| {
            format!(
                "repair worktree {} (was its record pruned after the queue moved? see ADR-0017)",
                worktree.display()
            )
        })?;
        Ok(())
    }

    /// Remove a landed run's worktree and branch. Administered from the
    /// main working tree, since `root` may be the worktree being removed.
    pub fn remove_worktree_and_branch(&self, worktree: &Path, branch: &str) -> Result<()> {
        let primary = self.primary_worktree()?;
        output(
            Command::new(&self.git)
                .arg("-C")
                .arg(&primary)
                .args(["worktree", "remove", "--force"])
                .arg(worktree),
        )?;
        output(
            Command::new(&self.git)
                .arg("-C")
                .arg(&primary)
                .args(["branch", "-D", branch]),
        )?;
        Ok(())
    }
}

/// How long `integrate` waits for `git push` before counting it as failed.
const PUSH_TIMEOUT: Duration = Duration::from_secs(300);

/// Run against (and from) the common directory, since `root`, or the
/// working directory, may be a run worktree that the landing removed
/// before the push.
impl MainRemote for GitRepository {
    fn has_remote(&self, remote: &str) -> Result<bool> {
        let remotes = output(
            Command::new(&self.git)
                .current_dir(&self.common_dir)
                .arg("--git-dir")
                .arg(&self.common_dir)
                .arg("remote"),
        )?;
        Ok(remotes.lines().any(|line| line.trim() == remote))
    }

    fn push_main(&self, remote: &str) -> Result<()> {
        let (status, stdout, stderr) = capture(
            Command::new(&self.git)
                .current_dir(&self.common_dir)
                .arg("--git-dir")
                .arg(&self.common_dir)
                .env("GIT_TERMINAL_PROMPT", "0")
                .args(["push", remote, "refs/heads/main:refs/heads/main"]),
            PUSH_TIMEOUT,
        )?;
        ensure!(
            status.success(),
            "git push {remote} main failed ({status}): {}",
            format!("{}\n{}", stderr.trim(), stdout.trim()).trim()
        );
        Ok(())
    }
}

pub struct Cmux {
    pub executable: PathBuf,
}

/// The one `CMUX_*` variable a detached process may carry: cmux's CLI
/// reads its socket password from it.
pub const SOCKET_PASSWORD_ENV: &str = "CMUX_SOCKET_PASSWORD";

/// How long the detached ping may take. Without `CMUX_SOCKET_PATH` cmux's
/// CLI discovers the socket on its own, which has taken up to 11 seconds
/// (cmux 0.64.25) before the reply or the refusal came.
pub const DETACHED_PING_TIMEOUT: Duration = Duration::from_secs(60);

/// Give `command` the environment a launchd-started supervisor has: every
/// `CMUX_*` variable in `inherited` (the socket capability, the workspace
/// and surface IDs, the socket path) removed, PATH replaced, and the socket
/// password set only when the invoking shell exported it.
pub fn detach(
    command: &mut Command,
    inherited: impl IntoIterator<Item = OsString>,
    environment: &SupervisorEnvironment,
) {
    for name in inherited {
        if name.to_string_lossy().starts_with("CMUX_") {
            command.env_remove(name);
        }
    }
    command.env("PATH", &environment.path);
    if let Some(password) = &environment.socket_password {
        command.env(SOCKET_PASSWORD_ENV, password);
    }
}

fn expect_pong(reply: &str) -> Result<()> {
    ensure!(
        reply.trim() == "PONG",
        "unexpected cmux ping response: {reply}"
    );
    Ok(())
}

impl WorkspaceBackend for Cmux {
    fn preflight(&self) -> Result<()> {
        expect_pong(&output(Command::new(&self.executable).arg("ping"))?)
    }

    fn preflight_detached(&self, environment: &SupervisorEnvironment) -> Result<()> {
        self.preflight_detached_within(environment, DETACHED_PING_TIMEOUT)
    }

    fn call_timeout(&self) -> Duration {
        OUTPUT_TIMEOUT
    }

    fn create(
        &self,
        task: &Task,
        run: &TaskRun,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        let raw = self.create_workspace(
            &run_workspace_name(task, run)?,
            Path::new(run.worktree_path.as_ref().context("missing worktree")?),
            command,
            tags,
        )?;
        // Persist the returned handle before resolving its stable UUID.
        fs::write(
            Path::new(run.run_dir.as_ref().context("missing run directory")?)
                .join("workspace-create.txt"),
            &raw,
        )?;
        self.identify(workspace_handle(&raw)?)
    }

    fn create_resume(
        &self,
        task: &Task,
        run: &TaskRun,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        let raw = self.create_workspace(
            &run_workspace_name(task, run)?,
            Path::new(run.worktree_path.as_ref().context("missing worktree")?),
            command,
            tags,
        )?;
        self.identify(workspace_handle(&raw)?)
    }

    /// `cmux send` reads `\n`, `\r` and `\t` as keys, so the text goes as
    /// one line with backslashes replaced; Enter submits it.
    fn send_text(&self, workspace_id: &str, text: &str) -> Result<()> {
        output(Command::new(&self.executable).args([
            "send",
            "--workspace",
            workspace_id,
            "--",
            &single_line(text),
        ]))?;
        output(Command::new(&self.executable).args([
            "send-key",
            "--workspace",
            workspace_id,
            "--",
            "enter",
        ]))?;
        Ok(())
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

    fn close(&self, workspace_id: &str) -> Result<()> {
        let raw = output(
            Command::new(&self.executable)
                .args(["workspace", "close"])
                .arg(workspace_id),
        )?;
        workspace_handle(&raw).context("cmux did not confirm the workspace close")?;
        Ok(())
    }

    /// Type `/exit` at Claude's prompt exactly as the maintainer would.
    fn send_exit(&self, workspace_id: &str) -> Result<()> {
        output(Command::new(&self.executable).args([
            "send",
            "--workspace",
            workspace_id,
            "--",
            "/exit",
        ]))?;
        output(Command::new(&self.executable).args([
            "send-key",
            "--workspace",
            workspace_id,
            "--",
            "enter",
        ]))?;
        Ok(())
    }

    fn exists(&self, workspace_id: &str) -> Result<bool> {
        let listing = output(Command::new(&self.executable).args([
            "--json",
            "--id-format",
            "uuids",
            "workspace",
            "list",
        ]))?;
        let listing: Value =
            serde_json::from_str(&listing).context("decode cmux workspace list")?;
        Ok(workspace_listed(&listing, workspace_id))
    }

    fn create_named(
        &self,
        name: &str,
        cwd: &Path,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        let raw = self.create_workspace(name, cwd, command, tags)?;
        self.identify(workspace_handle(&raw)?)
    }

    fn ensure_group(&self, external_id: &str, name: &str) -> Result<String> {
        let reply = output(Command::new(&self.executable).args([
            "--json",
            "--id-format",
            "uuids",
            "workspace-group",
            "create",
            "--name",
            name,
            "--external-id",
            external_id,
        ]))?;
        let reply: Value =
            serde_json::from_str(&reply).context("decode cmux workspace-group create")?;
        Ok(created_group_id(&reply)?.to_owned())
    }

    fn notify(&self, title: &str, body: &str, workspace: Option<&str>) -> Result<()> {
        let mut command = Command::new(&self.executable);
        command.args(["notify", "--title", title, "--body", body]);
        if let Some(workspace) = workspace {
            command.args(["--workspace", workspace]);
        }
        output(&mut command)?;
        Ok(())
    }
}

impl Cmux {
    /// The detached ping with an explicit deadline. cmux admits a client by
    /// its ancestry, not its environment: a child of one of its terminals
    /// gets through however its variables look, a process under launchd
    /// does not (verified against cmux 0.64.25). So besides the scrubbed
    /// environment the ping runs orphaned, the way the LaunchAgent's
    /// supervisor does: an outer `sh` in a session of its own backgrounds
    /// an inner one and exits; the inner one waits until the outer is gone
    /// (so launchd is its parent before cmux looks), prints its pid and
    /// becomes `cmux ping` by `exec`, so the pid is cmux's and can be
    /// killed at the deadline.
    pub fn preflight_detached_within(
        &self,
        environment: &SupervisorEnvironment,
        timeout: Duration,
    ) -> Result<()> {
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                r#"/bin/sh -c 'while kill -0 "$1" 2>/dev/null; do sleep 0.01; done; printf "pid=%s\n" "$$"; exec "$0" ping' "$0" "$$" &"#,
            ])
            .arg(&self.executable);
        // SAFETY: setsid only detaches the child from this session and
        // terminal; it allocates nothing and is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        detach(
            &mut command,
            env::vars_os().map(|(name, _)| name),
            environment,
        );
        let (reply, stderr) = orphan_output(&mut command, timeout)?;
        if reply.trim() == "PONG" {
            return Ok(());
        }
        Err(DetachedRefusal {
            reason: format!(
                "{:?} ping from outside cmux failed: {}",
                self.executable,
                if stderr.trim().is_empty() {
                    format!("unexpected response: {reply}")
                } else {
                    stderr.trim().to_owned()
                }
            ),
        }
        .into())
    }

    /// `cmux workspace create`; the raw reply carries the `OK workspace:N` handle.
    fn create_workspace(
        &self,
        name: &str,
        cwd: &Path,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        let mut create = Command::new(&self.executable);
        create.args(workspace_create_arguments(name, command, tags));
        output(create.arg("--cwd").arg(cwd))
    }

    /// Resolve a numeric handle to the workspace's stable UUID.
    fn identify(&self, handle: &str) -> Result<String> {
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
}

/// `workspace create` and its flags but `--cwd` (a path, added by the
/// caller): the title, the description, one `--env KEY=VALUE` per variable
/// and the group from `tags`, and the command, opened without focus.
pub fn workspace_create_arguments(name: &str, command: &str, tags: &WorkspaceTags) -> Vec<String> {
    let mut arguments: Vec<String> = ["workspace", "create", "--name", name]
        .map(str::to_owned)
        .into();
    if let Some(description) = &tags.description {
        arguments.extend(["--description".into(), description.clone()]);
    }
    for (key, value) in &tags.env {
        arguments.extend(["--env".into(), format!("{key}={value}")]);
    }
    if let Some(group) = &tags.group {
        arguments.extend(["--group".into(), group.clone()]);
    }
    arguments.extend([
        "--command".into(),
        command.into(),
        "--focus".into(),
        "false".into(),
    ]);
    arguments
}

/// Whether a `cmux --json --id-format uuids workspace list` reply lists the
/// workspace `id` (UUIDs compared without regard to case).
pub fn workspace_listed(listing: &Value, id: &str) -> bool {
    listing
        .get("workspaces")
        .and_then(Value::as_array)
        .is_some_and(|workspaces| {
            workspaces.iter().any(|workspace| {
                workspace
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|listed| listed.eq_ignore_ascii_case(id))
            })
        })
}

/// The group's UUID in a `cmux --json --id-format uuids workspace-group
/// create` reply, which is the same whether the call created the group or
/// found it by its external ID.
pub fn created_group_id(reply: &Value) -> Result<&str> {
    reply
        .pointer("/group/id")
        .and_then(Value::as_str)
        .context("cmux did not return the workspace group's ID")
}

/// Workspaces are named per repository because one cmux serves several
/// queues, and every name is `[<repo>]<role>` with no space after the
/// bracket, where `<repo>` is the basename of the repository root (the path
/// itself when it has none). A worker is
/// `[<repo>]worker#<task-id> - <task title>`, `<repo>` being the root the run
/// was planned from and the title the task's, unabridged (ADR-0028, which
/// replaces the name ADR-0018 chose).
/// The resume workspace of a `needs_session` run (goal 8's automatic resume)
/// takes this same name, with `run <run-id> resume` as its description.
/// Names are for people only: the runtime identifies workspaces by UUID
/// (ADR-0026).
pub fn run_workspace_name(task: &Task, run: &TaskRun) -> Result<String> {
    let repo = Path::new(run.repo_path.as_ref().context("missing repository path")?);
    Ok(format!(
        "[{}]worker#{} - {}",
        repository_name(repo),
        run.task_id,
        task.title
    ))
}

/// `dagq role=<role> queue=<queue hash>[ run=<run-id>][ task=<id>]`: the one
/// machine-readable description line every workspace of a queue carries,
/// for people reading `cmux workspace list`; the runtime never reads it back
/// (ADR-0026).
pub fn workspace_description(
    role: SessionRole,
    queue_hash: &str,
    run: Option<&str>,
    task: Option<i64>,
) -> String {
    let mut description = format!("dagq role={} queue={queue_hash}", role.as_str());
    if let Some(run) = run {
        description.push_str(&format!(" run={run}"));
    }
    if let Some(task) = task {
        description.push_str(&format!(" task={task}"));
    }
    description
}

/// `[<repo>]`: the name of the workspace group a queue's workspaces join.
pub fn workspace_group_name(repo_root: &Path) -> String {
    format!("[{}]", repository_name(repo_root))
}

/// `run <run-id> resume`: the description of the workspace the supervisor
/// resumes a `needs_session` run's session in; its title is the worker's
/// (`run_workspace_name`, ADR-0028).
pub fn resume_workspace_description(run: &TaskRun) -> String {
    format!("run {} resume", run.id)
}

/// `text` as one line for `cmux send`: line breaks and tabs become spaces
/// and backslashes slashes, so nothing in it reads as a key.
pub fn single_line(text: &str) -> String {
    text.split(['\n', '\r', '\t'])
        .filter(|part| !part.trim().is_empty())
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join(" ")
        .replace('\\', "/")
}

/// `[<repo>]maintainer`: the one resident Claude session of a repository's
/// queue (ADR-0028).
pub fn maintainer_workspace_name(repo_root: &Path) -> String {
    role_workspace_name(repo_root, SessionRole::Maintainer)
}

/// `[<repo>]supervisor`: the workspace `up --in-cmux` runs `supervise`
/// in when launchd cannot reach cmux (ADR-0011). The launchd mode has no
/// workspace at all.
pub fn supervisor_workspace_name(repo_root: &Path) -> String {
    role_workspace_name(repo_root, SessionRole::Supervisor)
}

/// `[<repo>]planner`: the session that talks with a person to register goals
/// and tasks, which `up` opens next to the maintainer's.
pub fn planner_workspace_name(repo_root: &Path) -> String {
    role_workspace_name(repo_root, SessionRole::Planner)
}

/// `[<repo>]inbox`: the session where a person answers the maintainer's
/// asks, which `up` opens next to the maintainer's.
pub fn inbox_workspace_name(repo_root: &Path) -> String {
    role_workspace_name(repo_root, SessionRole::Inbox)
}

fn role_workspace_name(repo_root: &Path, role: SessionRole) -> String {
    format!("[{}]{}", repository_name(repo_root), role.as_str())
}

fn repository_name(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
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
        let run_dir = Path::new(run.run_dir.as_ref().context("missing run directory")?);
        let settings = run_dir.join("claude-settings.json");
        fs::write(&settings, stop_hook_settings(&run.idle_marker_path()?)?)
            .with_context(|| format!("write {}", settings.display()))?;
        let mut command = Command::new(&self.executable);
        command
            .current_dir(run.worktree_path.as_ref().context("missing worktree")?)
            .arg("--session-id")
            .arg(&run.id)
            .arg("--debug-file")
            .arg(run.log_path.as_ref().context("missing log path")?)
            .arg("--add-dir")
            .arg(run_dir)
            .arg("--settings")
            .arg(&settings)
            .arg("--")
            .arg(prompt)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        Ok(command)
    }

    /// `claude --resume <run-id>` in the worktree with the run's settings
    /// (its `Stop` hook), so the resumed session opens like the worker did.
    fn resume_command(&self, run: &TaskRun) -> Result<Command> {
        let run_dir = Path::new(run.run_dir.as_ref().context("missing run directory")?);
        let settings = run_dir.join("claude-settings.json");
        fs::write(&settings, stop_hook_settings(&run.idle_marker_path()?)?)
            .with_context(|| format!("write {}", settings.display()))?;
        let mut command = Command::new(&self.executable);
        command
            .current_dir(run.worktree_path.as_ref().context("missing worktree")?)
            .arg("--resume")
            .arg(&run.id)
            .arg("--debug-file")
            .arg(run_dir.join("claude-resume.log"))
            .arg("--add-dir")
            .arg(run_dir)
            .arg("--settings")
            .arg(&settings)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        Ok(command)
    }
}

/// Claude Code's global config, where the folder trust of each project is
/// kept: `$CLAUDE_CONFIG_DIR/.claude.json` when that is set (non-empty),
/// else `~/.claude.json`. `None` when neither can be named.
pub fn claude_global_config(config_dir: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    match (config_dir.filter(|dir| !dir.is_empty()), home) {
        (Some(dir), _) => Some(Path::new(dir).join(".claude.json")),
        (None, Some(home)) if !home.is_empty() => Some(Path::new(home).join(".claude.json")),
        _ => None,
    }
}

/// Whether Claude Code has recorded the folder trust dialog as accepted for
/// the repository at `root` in its global config (`config`). Every run
/// worktree resolves to its repository's root for the trust check, so this
/// one key decides whether run sessions stop at the dialog
/// (docs/design/provider-lifecycle.md). A missing config trusts nothing.
pub fn claude_trusts_repository(config: &Path, root: &Path) -> Result<bool> {
    let text = match fs::read_to_string(config) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("read {}", config.display())),
    };
    let config_json: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parse Claude Code config {}", config.display()))?;
    let accepted = |key: &Path| {
        key.to_str().is_some_and(|key| {
            config_json["projects"][key]["hasTrustDialogAccepted"].as_bool() == Some(true)
        })
    };
    Ok(accepted(root) || root.canonicalize().is_ok_and(|real| accepted(&real)))
}

/// Per-run Claude settings. The `Stop` hook publishes the hook's stdin JSON
/// as the idle marker. Each finished response replaces the marker
/// atomically, so its modification time tells the supervisor whether the
/// agent went idle after writing the receipt. `SessionEnd` is not used:
/// session exit is confirmed by the wrapper's exit code instead.
///
/// `autoMode.environment: ["$defaults"]` keeps the built-in classifier
/// environment and, being a non-empty environment from flag settings, keeps
/// the "Teach auto mode about your environment?" dialog from opening in a
/// run session (docs/design/provider-lifecycle.md).
pub fn stop_hook_settings(idle_marker: &Path) -> Result<String> {
    let marker = path_text(idle_marker)?;
    let command = format!(
        "cat > {tmp} && mv -f {tmp} {marker}",
        tmp = shell_quote(&format!("{marker}.tmp")),
        marker = shell_quote(&marker),
    );
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "hooks": {
            "Stop": [{
                "hooks": [{"type": "command", "command": command, "timeout": 10}]
            }]
        },
        "autoMode": {
            "environment": ["$defaults"]
        }
    }))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Provider, RunStatus};

    fn run(repo_path: Option<&str>) -> TaskRun {
        TaskRun {
            id: "0d8e3f1a-7c1b-4e35-9a11-3f6d2c9b8e47".into(),
            task_id: 15,
            status: RunStatus::Claimed,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            base_commit: "a".repeat(40),
            branch: None,
            worktree_path: None,
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit: None,
            repo_path: repo_path.map(str::to_owned),
            run_dir: None,
            last_error: None,
            workspace_closed_at: None,
            created_at: "2026-09-22 00:00:00".into(),
        }
    }

    fn task(title: &str) -> Task {
        Task {
            id: 15,
            title: title.into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            status: crate::domain::TaskStatus::InProgress,
            goal_id: None,
            context: String::new(),
            created_at: "2026-09-22 00:00:00".into(),
            updated_at: "2026-09-22 00:00:00".into(),
        }
    }

    /// One cmux serves several repositories, so every workspace name
    /// carries the repository (the basename of its root). A worker's name
    /// carries the task and its title as is; the run ID goes to the
    /// description instead (ADR-0018, ADR-0028).
    #[test]
    fn workspace_names_carry_the_repository_and_the_task() {
        let title = "Set last_error when a run fails";
        assert_eq!(
            run_workspace_name(&task(title), &run(Some("/home/u/ghq/dagq"))).unwrap(),
            "[dagq]worker#15 - Set last_error when a run fails"
        );
        // The title is neither trimmed nor shortened.
        let long = format!("  {}  ", "x".repeat(200));
        assert_eq!(
            run_workspace_name(&task(&long), &run(Some("/tmp/my repo/"))).unwrap(),
            format!("[my repo]worker#15 - {long}")
        );
        assert!(
            !run_workspace_name(&task(title), &run(Some("/home/u/ghq/dagq")))
                .unwrap()
                .contains("0d8e3f1a")
        );
        assert!(
            run_workspace_name(&task(title), &run(None))
                .unwrap_err()
                .to_string()
                .contains("missing repository path")
        );
        assert_eq!(
            maintainer_workspace_name(Path::new("/home/u/ghq/dagq")),
            "[dagq]maintainer"
        );
        // A root with no basename falls back to the path itself.
        assert_eq!(maintainer_workspace_name(Path::new("/")), "[/]maintainer");
        assert_eq!(
            supervisor_workspace_name(Path::new("/home/u/ghq/dagq")),
            "[dagq]supervisor"
        );
        assert_eq!(
            planner_workspace_name(Path::new("/home/u/ghq/dagq")),
            "[dagq]planner"
        );
        assert_eq!(
            inbox_workspace_name(Path::new("/tmp/my repo/")),
            "[my repo]inbox"
        );
    }

    /// Every workspace of a queue says what it is in one line; the run and
    /// the task appear only where the workspace has them (ADR-0026).
    #[test]
    fn workspace_descriptions_are_one_machine_readable_line() {
        assert_eq!(
            workspace_description(
                SessionRole::Worker,
                "77067154921b9014",
                Some("0d8e3f1a-7c1b-4e35-9a11-3f6d2c9b8e47"),
                Some(15)
            ),
            "dagq role=worker queue=77067154921b9014 run=0d8e3f1a-7c1b-4e35-9a11-3f6d2c9b8e47 task=15"
        );
        assert_eq!(
            workspace_description(SessionRole::Maintainer, "abc", None, None),
            "dagq role=maintainer queue=abc"
        );
        assert_eq!(
            workspace_description(SessionRole::Supervisor, "abc", None, None),
            "dagq role=supervisor queue=abc"
        );
        assert_eq!(
            workspace_group_name(Path::new("/home/u/ghq/dagq")),
            "[dagq]"
        );
        assert_eq!(workspace_group_name(Path::new("/")), "[/]");
    }

    /// A workspace is found by its UUID, never its title, and cmux may
    /// print the UUID in either case.
    #[test]
    fn workspace_listed_matches_the_id_only() {
        let listing = serde_json::json!({
            "window_id": "W",
            "workspaces": [
                {"id": "4AC63CB7-3BE1-40A1-BCC4-CA0461685F01", "title": "[dagq]maintainer"},
                {"title": "no id"}
            ]
        });
        assert!(workspace_listed(
            &listing,
            "4AC63CB7-3BE1-40A1-BCC4-CA0461685F01"
        ));
        assert!(workspace_listed(
            &listing,
            "4ac63cb7-3be1-40a1-bcc4-ca0461685f01"
        ));
        assert!(!workspace_listed(&listing, "[dagq]maintainer"));
        assert!(!workspace_listed(&serde_json::json!({}), "x"));
    }

    #[test]
    fn created_group_id_reads_the_group_uuid() {
        let reply = serde_json::json!({
            "created": false,
            "group": {"id": "F5FCC58F-D44B-4CA0-871F-D4C6AED704D6", "external_id": "abc"}
        });
        assert_eq!(
            created_group_id(&reply).unwrap(),
            "F5FCC58F-D44B-4CA0-871F-D4C6AED704D6"
        );
        assert!(created_group_id(&serde_json::json!({"created": true})).is_err());
    }

    /// `ensure_group` asks for the group by its external ID and `exists`
    /// reads the UUID listing, both through the real argv.
    #[cfg(unix)]
    #[test]
    fn ensure_group_and_exists_call_cmux() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args.log");
        let executable = dir.path().join("cmux");
        fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
printf '%s ' "$@" >> '{log}'; printf '\n' >> '{log}'
case "$4" in
  workspace-group) echo '{{"created":true,"group":{{"id":"G-1"}}}}' ;;
  workspace) echo '{{"workspaces":[{{"id":"W-1"}}]}}' ;;
esac
"#,
                log = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let cmux = Cmux { executable };
        assert_eq!(cmux.ensure_group("abc", "[dagq]").unwrap(), "G-1");
        assert!(cmux.exists("w-1").unwrap());
        assert!(!cmux.exists("W-2").unwrap());
        let calls = fs::read_to_string(&log).unwrap();
        assert!(
            calls.contains(
                "--json --id-format uuids workspace-group create --name [dagq] --external-id abc"
            ),
            "{calls}"
        );
        assert!(calls.contains("--json --id-format uuids workspace list"));
    }

    /// `create` passes the run's name and its tags (description, env,
    /// group) to `cmux workspace create`, keeps the raw reply in the run directory and returns the
    /// UUID `identify` resolves.
    #[cfg(unix)]
    #[test]
    fn create_names_the_run_workspace_and_tags_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args.log");
        let executable = dir.path().join("cmux");
        fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >> '{log}'; done
printf -- '--\n' >> '{log}'
case "$1" in
  workspace) echo "OK workspace:7" ;;
  --json) echo '{{"caller":{{"workspace_id":"4AC63CB7-3BE1-40A1-BCC4-CA0461685F01"}}}}' ;;
esac
"#,
                log = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let run_dir = dir.path().join("run");
        fs::create_dir(&run_dir).unwrap();
        let mut run = run(Some("/home/u/ghq/dagq"));
        run.worktree_path = Some(dir.path().display().to_string());
        run.run_dir = Some(run_dir.display().to_string());
        let cmux = Cmux { executable };
        let tags = WorkspaceTags {
            env: vec![
                ("DAGQ_ROLE".into(), "worker".into()),
                ("DAGQ_QUEUE".into(), "/q/queue.db".into()),
            ],
            description: Some("dagq role=worker queue=abc".into()),
            group: Some("G-1".into()),
        };
        let id = cmux
            .create(
                &task("Set last_error when a run fails"),
                &run,
                "true",
                &tags,
            )
            .unwrap();
        assert_eq!(id, "4AC63CB7-3BE1-40A1-BCC4-CA0461685F01");
        assert_eq!(
            fs::read_to_string(run_dir.join("workspace-create.txt")).unwrap(),
            "OK workspace:7\n"
        );
        let calls = fs::read_to_string(&log).unwrap();
        let create: Vec<&str> = calls.split("--\n").next().unwrap().lines().collect();
        assert_eq!(
            create,
            [
                "workspace",
                "create",
                "--name",
                "[dagq]worker#15 - Set last_error when a run fails",
                "--description",
                "dagq role=worker queue=abc",
                "--env",
                "DAGQ_ROLE=worker",
                "--env",
                "DAGQ_QUEUE=/q/queue.db",
                "--group",
                "G-1",
                "--command",
                "true",
                "--focus",
                "false",
                "--cwd",
                &dir.path().display().to_string(),
            ]
        );
        assert!(calls.contains("identify\n--workspace\nworkspace:7\n"));
    }

    #[test]
    fn review_output_reads_non_utf8_lossily_where_output_refuses_it() {
        let latin1 = || {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", r"printf 'caf\351\n'"]);
            command
        };
        let error = format!("{:#}", output(&mut latin1()).unwrap_err());
        assert!(error.contains("command output is not UTF-8"), "{error}");
        assert_eq!(review_output(&mut latin1()).unwrap(), "caf\u{fffd}\n");
        let error = review_output(Command::new("/bin/sh").args(["-c", "echo no >&2; exit 3"]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("failed") && error.contains("no"), "{error}");
    }

    #[test]
    fn review_output_to_streams_raw_bytes_into_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let file = fs::File::create(&path).unwrap();
        review_output_to(
            Command::new("/bin/sh").args(["-c", r"printf 'caf\351\n'"]),
            &file,
        )
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"caf\xe9\n");
        let error = review_output_to(
            Command::new("/bin/sh").args(["-c", "echo broken >&2; exit 2"]),
            &file,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("broken"), "{error}");
    }

    #[test]
    fn claude_global_config_prefers_the_config_dir_over_home() {
        assert_eq!(
            claude_global_config(Some("/cfg"), Some("/home/u")),
            Some(PathBuf::from("/cfg/.claude.json"))
        );
        assert_eq!(
            claude_global_config(Some(""), Some("/home/u")),
            Some(PathBuf::from("/home/u/.claude.json"))
        );
        assert_eq!(claude_global_config(None, Some("")), None);
        assert_eq!(claude_global_config(None, None), None);
    }
}
