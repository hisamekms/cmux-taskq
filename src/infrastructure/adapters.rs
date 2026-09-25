use crate::{
    application::{
        AgentProvider, CommandSpec, DetachedRefusal, MainRemote, PlannerCommand, ProcessControl,
        Repository, SupervisorEnvironment, WorkspaceBackend, WorkspaceTags,
        stats::WorkspaceListing,
    },
    domain::{CommitSha, Task, TaskId, TaskRun, stats::ListedWorkspace},
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

use crate::application::naming::repository_name;
pub use crate::application::{
    SOCKET_PASSWORD_ENV,
    naming::{
        ask_notification_title, inbox_workspace_name, planner_workspace_name,
        resume_workspace_description, shell_join, shell_quote, supervisor_workspace_name,
        workspace_description, workspace_group_name,
    },
    path_text,
};

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

/// The full message of `commit` in the repository whose Git directory is
/// `git_dir`; `None` when Git cannot read it there.
pub fn commit_message(git_dir: &Path, commit: &str) -> Option<String> {
    let git = executable(Path::new("git")).ok()?;
    output(Command::new(&git).arg("--git-dir").arg(git_dir).args([
        "show",
        "-s",
        "--format=%B",
        commit,
        "--",
    ]))
    .ok()
    .map(|message| message.trim_end().to_owned())
}

fn main_head(git: &Path, root: &Path) -> Result<CommitSha> {
    object_id(
        &output(Command::new(git).arg("-C").arg(root).args([
            "rev-parse",
            "--verify",
            "refs/heads/main^{commit}",
        ]))?,
        "main commit",
    )
}

/// The commit Git printed (a full object ID, surrounded by whitespace).
fn object_id(text: &str, field: &'static str) -> Result<CommitSha> {
    Ok(CommitSha::parse(text.trim(), field)?)
}

pub use crate::application::DiffNumbers;

#[derive(Clone)]
pub struct GitRepository {
    pub root: PathBuf,
    pub common_dir: PathBuf,
    /// `refs/heads/main` at inspection time; `main_head` rereads it.
    pub base_commit: CommitSha,
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

    /// A read-only `git -C <worktree>` for a worktree a session may be
    /// working in. `GIT_OPTIONAL_LOCKS=0` keeps `git status` from taking
    /// `index.lock` to write back the index it refreshed, which would make
    /// the session's own `git add` / `rebase --continue` fail on the lock
    /// or have its index overwritten by a stale one.
    fn read_worktree(&self, worktree: &Path) -> Command {
        let mut command = Command::new(&self.git);
        command
            .env("GIT_OPTIONAL_LOCKS", "0")
            .arg("-C")
            .arg(worktree);
        command
    }

    /// Current `refs/heads/main`, read again so that a task unblocked by an
    /// integration starts from the main that contains its predecessor.
    pub fn main_head(&self) -> Result<CommitSha> {
        main_head(&self.git, &self.root)
    }

    /// Symbolic HEAD of a worktree, or None when detached.
    pub fn current_branch(&self, worktree: &Path) -> Result<Option<String>> {
        let (status, stdout, stderr) = capture(
            self.read_worktree(worktree)
                .args(["symbolic-ref", "--quiet", "HEAD"]),
            Duration::from_secs(30),
        )?;
        match status.code() {
            Some(0) => Ok(Some(stdout.trim().to_owned())),
            Some(1) => Ok(None),
            _ => bail!("git symbolic-ref failed ({status}): {stderr}"),
        }
    }

    pub fn head(&self, worktree: &Path) -> Result<CommitSha> {
        object_id(
            &output(
                self.read_worktree(worktree)
                    .args(["rev-parse", "--verify", "HEAD^{commit}"]),
            )?,
            "HEAD",
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

    /// `git merge-base <a> <b>`: their best common ancestor, `None` when
    /// they share no history.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<CommitSha>> {
        let (status, stdout, stderr) = capture(
            Command::new(&self.git)
                .arg("-C")
                .arg(&self.root)
                .args(["merge-base", a, b]),
            Duration::from_secs(30),
        )?;
        match status.code() {
            Some(0) => Ok(Some(object_id(&stdout, "merge base")?)),
            Some(1) => Ok(None),
            _ => bail!("git merge-base failed ({status}): {stderr}"),
        }
    }

    /// The paths that conflict when `head` is merged with `main` (over
    /// their merge base), judged by `git merge-tree --write-tree` in the
    /// object store alone: no worktree, index or ref moves (ADR-0027
    /// decision 4). Empty when they merge cleanly.
    pub fn merge_conflicts(&self, main: &str, head: &str) -> Result<Vec<String>> {
        let (status, stdout, stderr) = capture(
            Command::new(&self.git).arg("-C").arg(&self.root).args([
                "merge-tree",
                "--write-tree",
                "--name-only",
                "--no-messages",
                "-z",
                main,
                head,
            ]),
            Duration::from_secs(5 * 60),
        )?;
        match status.code() {
            Some(0) => Ok(Vec::new()),
            // The tree's OID, then each conflicted path, NUL-terminated and
            // never quoted.
            Some(1) => {
                let mut paths: Vec<String> = Vec::new();
                for path in stdout.split('\0').skip(1).take_while(|p| !p.is_empty()) {
                    if !paths.iter().any(|p| p == path) {
                        paths.push(path.to_owned());
                    }
                }
                Ok(paths)
            }
            _ => bail!("git merge-tree failed ({status}): {stderr}"),
        }
    }

    /// Porcelain status including untracked files; empty means clean.
    pub fn status(&self, worktree: &Path) -> Result<String> {
        output(self.read_worktree(worktree).args([
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
                .arg(run.branch().context("missing branch")?)
                .arg(run.worktree_path().context("missing worktree")?)
                .arg(run.base_commit().as_str()),
        )
    }

    /// Whether a `git rebase` was left half-done in the worktree (by a
    /// crashed landing or an unfinished session).
    pub fn rebase_in_progress(&self, worktree: &Path) -> Result<bool> {
        let paths = output(self.read_worktree(worktree).args([
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

    /// Paths with unresolved conflicts in the worktree. The plumbing
    /// `diff-files`, since porcelain `git diff` writes a refreshed index
    /// back even under `GIT_OPTIONAL_LOCKS=0`.
    pub fn conflicted_files(&self, worktree: &Path) -> Result<Vec<String>> {
        Ok(output(self.read_worktree(worktree).args([
            "diff-files",
            "--name-only",
            "--diff-filter=U",
        ]))?
        .lines()
        .map(str::to_owned)
        .collect())
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
    pub fn landed_task_ids(&self, base: &str, head: &str) -> Result<Vec<TaskId>> {
        let text = review_output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "log",
            "--reverse",
            "--format=%(trailers:key=Dagq-Task,valueonly)",
            &format!("{base}..{head}"),
        ]))?;
        let mut ids: Vec<TaskId> = Vec::new();
        for id in text
            .lines()
            .filter_map(|line| line.trim().parse().ok().map(TaskId::new))
        {
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

    /// The paths whose content differs between the trees of `from` and
    /// `to` (`git diff --name-only --no-renames`): both sides of a rename,
    /// never quoted.
    pub fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let names = output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            from,
            to,
            "--",
        ]))?;
        Ok(names
            .split('\0')
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect())
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
    pub fn commit_tree(
        &self,
        tree: &str,
        parent: &str,
        paragraphs: &[String],
    ) -> Result<CommitSha> {
        let mut command = Command::new(&self.git);
        command
            .arg("-C")
            .arg(&self.root)
            .args(["commit-tree", tree, "-p", parent]);
        for paragraph in paragraphs {
            command.arg("-m").arg(paragraph);
        }
        object_id(&output(&mut command)?, "new commit")
    }

    pub fn update_ref(&self, name: &str, value: &str) -> Result<()> {
        output(Command::new(&self.git).arg("-C").arg(&self.root).args([
            "update-ref",
            name,
            value,
        ]))?;
        Ok(())
    }

    pub fn ref_exists(&self, name: &str) -> Result<Option<CommitSha>> {
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
            Some(0) => Ok(Some(object_id(&stdout, "ref")?)),
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

    /// Remove a run's worktree and branch. Administered from the main
    /// working tree, since `root` may be the worktree being removed. A
    /// branch already gone is left at that.
    pub fn remove_worktree_and_branch(&self, worktree: &Path, branch: &str) -> Result<()> {
        let primary = self.primary_worktree()?;
        output(
            Command::new(&self.git)
                .arg("-C")
                .arg(&primary)
                .args(["worktree", "remove", "--force"])
                .arg(worktree),
        )?;
        let reference = format!("refs/heads/{}", branch.trim_start_matches("refs/heads/"));
        let (status, _, _) = capture(
            Command::new(&self.git)
                .arg("-C")
                .arg(&primary)
                .args(["show-ref", "--verify", "--quiet", &reference]),
            OUTPUT_TIMEOUT,
        )?;
        if status.success() {
            output(
                Command::new(&self.git)
                    .arg("-C")
                    .arg(&primary)
                    .args(["branch", "-D", branch]),
            )?;
        }
        Ok(())
    }

    /// Whether Git tracks any file at or under `path` in `worktree`.
    pub fn tracks(&self, worktree: &Path, path: &str) -> Result<bool> {
        let listed = output(
            Command::new(&self.git)
                .arg("-C")
                .arg(worktree)
                .args(["ls-files", "--"])
                .arg(path),
        )?;
        Ok(!listed.trim().is_empty())
    }
}

/// How long `integrate` waits for `git push` before counting it as failed.
const PUSH_TIMEOUT: Duration = Duration::from_secs(300);

/// The Git port over the inherent methods above, which callers that hold a
/// `GitRepository` keep using directly.
impl Repository for GitRepository {
    fn main_head(&self) -> Result<CommitSha> {
        GitRepository::main_head(self)
    }
    fn current_branch(&self, worktree: &Path) -> Result<Option<String>> {
        GitRepository::current_branch(self, worktree)
    }
    fn head(&self, worktree: &Path) -> Result<CommitSha> {
        GitRepository::head(self, worktree)
    }
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        GitRepository::is_ancestor(self, ancestor, descendant)
    }
    fn merge_base(&self, a: &str, b: &str) -> Result<Option<CommitSha>> {
        GitRepository::merge_base(self, a, b)
    }
    fn status(&self, worktree: &Path) -> Result<String> {
        GitRepository::status(self, worktree)
    }
    fn rebase_in_progress(&self, worktree: &Path) -> Result<bool> {
        GitRepository::rebase_in_progress(self, worktree)
    }
    fn rebase_abort(&self, worktree: &Path) -> Result<()> {
        GitRepository::rebase_abort(self, worktree)
    }
    fn rebase(&self, worktree: &Path, onto: &str) -> Result<std::result::Result<(), String>> {
        GitRepository::rebase(self, worktree, onto)
    }
    fn conflicted_files(&self, worktree: &Path) -> Result<Vec<String>> {
        GitRepository::conflicted_files(self, worktree)
    }
    fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>> {
        GitRepository::changed_paths(self, from, to)
    }
    fn tree_of(&self, commit: &str) -> Result<String> {
        GitRepository::tree_of(self, commit)
    }
    fn commit_tree(&self, tree: &str, parent: &str, paragraphs: &[String]) -> Result<CommitSha> {
        GitRepository::commit_tree(self, tree, parent, paragraphs)
    }
    fn update_ref(&self, name: &str, value: &str) -> Result<()> {
        GitRepository::update_ref(self, name, value)
    }
    fn advance_main(&self, from: &str, to: &str) -> Result<()> {
        GitRepository::advance_main(self, from, to)
    }
    fn repair_worktree(&self, worktree: &Path) -> Result<()> {
        GitRepository::repair_worktree(self, worktree)
    }
    fn remove_worktree_and_branch(&self, worktree: &Path, branch: &str) -> Result<()> {
        GitRepository::remove_worktree_and_branch(self, worktree, branch)
    }
    fn tracks(&self, worktree: &Path, path: &str) -> Result<bool> {
        GitRepository::tracks(self, worktree, path)
    }
    fn main_checkout(&self) -> Result<Option<PathBuf>> {
        GitRepository::main_checkout(self)
    }
    fn log_oneline(&self, base: &str, head: &str) -> Result<String> {
        GitRepository::log_oneline(self, base, head)
    }
    fn diff_stat(&self, base: &str, head: &str) -> Result<String> {
        GitRepository::diff_stat(self, base, head)
    }
    fn diff_numbers(&self, base: &str, head: &str) -> Result<DiffNumbers> {
        GitRepository::diff_numbers(self, base, head)
    }
    fn diff_to_file(&self, base: &str, head: &str, path: &Path) -> Result<()> {
        let file = fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
        GitRepository::diff_to(self, base, head, &file)
    }
    fn create_worktree(&self, run: &TaskRun) -> Result<String> {
        GitRepository::create_worktree(self, run)
    }
    fn merge_conflicts(&self, main: &str, head: &str) -> Result<Vec<String>> {
        GitRepository::merge_conflicts(self, main, head)
    }
    fn landed_task_ids(&self, base: &str, head: &str) -> Result<Vec<TaskId>> {
        GitRepository::landed_task_ids(self, base, head)
    }
}

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
            Path::new(run.worktree_path().context("missing worktree")?),
            command,
            tags,
        )?;
        // Persist the returned handle before resolving its stable UUID.
        fs::write(
            Path::new(run.run_dir().context("missing run directory")?).join("workspace-create.txt"),
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
            Path::new(run.worktree_path().context("missing worktree")?),
            command,
            tags,
        )?;
        self.identify(workspace_handle(&raw)?)
    }

    /// `cmux send` reads `\n`, `\r` and `\t` as keys, so the text goes as
    /// one line with backslashes replaced; Enter submits it once the agent
    /// had [`paste_settle`] to take the paste in (an Enter in the middle of
    /// a long paste is taken as part of it, task 285).
    fn send_text(&self, workspace_id: &str, text: &str) -> Result<()> {
        let line = single_line(text);
        output(Command::new(&self.executable).args([
            "send",
            "--workspace",
            workspace_id,
            "--",
            &line,
        ]))?;
        thread::sleep(paste_settle(line.chars().count()));
        self.send_enter(workspace_id)
    }

    fn send_enter(&self, workspace_id: &str) -> Result<()> {
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

    /// cmux refuses to close a pinned workspace ("protected", cmux
    /// 0.64.25), so the pin goes first. An unpin that fails (the
    /// workspace is already gone, say) does not stop the close, whose own
    /// error is the one reported.
    fn close(&self, workspace_id: &str) -> Result<()> {
        let _ = self.workspace_action(workspace_id, &["unpin"]);
        let raw = output(
            Command::new(&self.executable)
                .args(["workspace", "close"])
                .arg(workspace_id),
        )?;
        workspace_handle(&raw).context("cmux did not confirm the workspace close")?;
        Ok(())
    }

    fn set_color(&self, workspace_id: &str, color: &str) -> Result<()> {
        self.workspace_action(workspace_id, &["set-color", "--color", color])
    }

    fn set_status(&self, workspace_id: &str, key: &str, value: &str, icon: &str) -> Result<()> {
        output(Command::new(&self.executable).args([
            "set-status",
            key,
            value,
            "--icon",
            icon,
            "--workspace",
            workspace_id,
        ]))?;
        Ok(())
    }

    fn pin(&self, workspace_id: &str) -> Result<()> {
        self.workspace_action(workspace_id, &["pin"])
    }

    /// Type `/exit` at Claude's prompt exactly as a person would.
    fn send_exit(&self, workspace_id: &str) -> Result<()> {
        output(Command::new(&self.executable).args([
            "send",
            "--workspace",
            workspace_id,
            "--",
            "/exit",
        ]))?;
        thread::sleep(paste_settle("/exit".len()));
        self.send_enter(workspace_id)
    }

    fn exists(&self, workspace_id: &str) -> Result<bool> {
        Ok(workspace_listed(&self.workspace_listing()?, workspace_id))
    }

    fn listed_workspace_ids(&self) -> Result<Vec<String>> {
        Ok(listed_workspaces(&self.workspace_listing()?)?
            .into_iter()
            .map(|workspace| workspace.id)
            .collect())
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

impl WorkspaceListing for Cmux {
    fn list_workspaces(&self) -> Result<Vec<ListedWorkspace>> {
        listed_workspaces(&self.workspace_listing()?)
    }
}

impl Cmux {
    /// Every window's `cmux --json --id-format uuids workspace list`, merged
    /// into one listing. Without `--window` cmux lists only the caller's
    /// window, so a workspace a person moved to another window would look
    /// closed and `up` would open a second one.
    fn workspace_listing(&self) -> Result<Value> {
        let windows = output(Command::new(&self.executable).args([
            "--json",
            "--id-format",
            "uuids",
            "list-windows",
        ]))?;
        let windows: Value = serde_json::from_str(&windows).context("decode cmux list-windows")?;
        merged_workspace_listing(&windows, |window| {
            let listing = output(Command::new(&self.executable).args([
                "--json",
                "--id-format",
                "uuids",
                "workspace",
                "list",
                "--window",
                window,
            ]))?;
            serde_json::from_str(&listing).context("decode cmux workspace list")
        })
    }

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

    /// `cmux workspace-action --action <action> [flags…] --workspace <id>`.
    fn workspace_action(&self, workspace_id: &str, action: &[&str]) -> Result<()> {
        output(
            Command::new(&self.executable)
                .args(["workspace-action", "--action"])
                .args(action)
                .args(["--workspace", workspace_id]),
        )?;
        Ok(())
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

/// One `workspace list`-shaped listing of the workspaces of every window in
/// a `cmux --json --id-format uuids list-windows` reply, `list` giving each
/// window's listing by its UUID. A window that cannot be listed fails the
/// whole listing: a workspace missed there would look closed.
pub fn merged_workspace_listing(
    windows: &Value,
    mut list: impl FnMut(&str) -> Result<Value>,
) -> Result<Value> {
    let mut workspaces = Vec::new();
    for window in windows
        .as_array()
        .context("cmux list-windows is not a list")?
    {
        let id = window
            .get("id")
            .and_then(Value::as_str)
            .context("cmux listed a window without an ID")?;
        let listing = list(id).with_context(|| format!("list the workspaces of window {id}"))?;
        workspaces.extend(
            listing
                .get("workspaces")
                .and_then(Value::as_array)
                .with_context(|| format!("cmux workspace list of window {id} has no workspaces"))?
                .iter()
                .cloned(),
        );
    }
    Ok(serde_json::json!({ "workspaces": workspaces }))
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

/// Every workspace of a `cmux --json --id-format uuids workspace list`
/// reply, with its description.
pub fn listed_workspaces(listing: &Value) -> Result<Vec<ListedWorkspace>> {
    listing
        .get("workspaces")
        .and_then(Value::as_array)
        .context("cmux workspace list has no workspaces")?
        .iter()
        .map(|workspace| {
            Ok(ListedWorkspace {
                id: workspace
                    .get("id")
                    .and_then(Value::as_str)
                    .context("cmux listed a workspace without an ID")?
                    .to_owned(),
                description: workspace
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
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
    let repo = Path::new(run.repo_path().context("missing repository path")?);
    Ok(format!(
        "[{}]worker#{} - {}",
        repository_name(repo),
        run.task_id(),
        task.title()
    ))
}

/// `text` as one line for `cmux send`: line breaks and tabs become spaces
/// and backslashes slashes, so nothing in it reads as a key.
/// How long the agent is given to take in `chars` typed characters
/// before the Enter that submits them: 300 ms and 1 ms for every 10
/// characters, at most 3 s (1.6 s for a 13 KB resolution request).
pub fn paste_settle(chars: usize) -> Duration {
    Duration::from_millis((300 + chars as u64 / 10).min(3000))
}

pub fn single_line(text: &str) -> String {
    text.split(['\n', '\r', '\t'])
        .filter(|part| !part.trim().is_empty())
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join(" ")
        .replace('\\', "/")
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

    fn command(&self, run: &TaskRun, prompt: &str) -> Result<CommandSpec> {
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        let settings = run_dir.join("claude-settings.json");
        fs::write(&settings, stop_hook_settings(&run.idle_marker_path()?)?)
            .with_context(|| format!("write {}", settings.display()))?;
        let mut command = CommandSpec::new(&self.executable);
        command
            .current_dir(run.worktree_path().context("missing worktree")?)
            .arg("--session-id")
            .arg(run.id().as_str())
            .arg("--debug-file")
            .arg(run.log_path().context("missing log path")?)
            .arg("--add-dir")
            .arg(run_dir)
            .arg("--settings")
            .arg(&settings)
            .arg("--")
            .arg(prompt);
        Ok(command)
    }

    /// `claude --resume <run-id>` in the worktree with the run's settings
    /// (its `Stop` hook), so the resumed session opens like the worker did.
    fn resume_command(&self, run: &TaskRun) -> Result<CommandSpec> {
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        let settings = run_dir.join("claude-settings.json");
        fs::write(&settings, stop_hook_settings(&run.idle_marker_path()?)?)
            .with_context(|| format!("write {}", settings.display()))?;
        let mut command = CommandSpec::new(&self.executable);
        command
            .current_dir(run.worktree_path().context("missing worktree")?)
            .arg("--resume")
            .arg(run.id().as_str())
            .arg("--debug-file")
            .arg(run_dir.join("claude-resume.log"))
            .arg("--add-dir")
            .arg(run_dir)
            .arg("--settings")
            .arg(&settings);
        Ok(command)
    }

    /// `claude` in the checkout with the planner directory's settings (its
    /// `Stop` hook writes the idle marker there), its debug file, the
    /// directory added, and the plugin directory when one was given.
    fn planner_command(&self, planner: &PlannerCommand<'_>) -> Result<CommandSpec> {
        let settings = planner.dir.join("claude-settings.json");
        fs::write(&settings, stop_hook_settings(&planner.idle_marker())?)
            .with_context(|| format!("write {}", settings.display()))?;
        let mut command = CommandSpec::new(&self.executable);
        command
            .current_dir(planner.cwd)
            .arg("--debug-file")
            .arg(planner.dir.join("claude.log"))
            .arg("--add-dir")
            .arg(planner.dir)
            .arg("--settings")
            .arg(&settings);
        if let Some(dir) = planner.plugin_dir {
            command.arg("--plugin-dir").arg(dir);
        }
        command.arg("--").arg(planner.prompt);
        Ok(command)
    }

    /// `claude -p` (print mode): no terminal, no trust dialog; a tool that
    /// needs permission and is not in `allowed_tools` is refused.
    fn headless_command(
        &self,
        cwd: &Path,
        prompt: &str,
        allowed_tools: &[&str],
    ) -> Result<CommandSpec> {
        let mut command = CommandSpec::new(&self.executable);
        command.current_dir(cwd).arg("-p");
        if !allowed_tools.is_empty() {
            command.arg("--allowedTools").args(allowed_tools);
        }
        command.arg("--").arg(prompt);
        Ok(command)
    }
    /// `claude -p` in the worktree with `claude-review-settings.json` of
    /// the run directory: the worker's settings without its `Stop` hook, so
    /// the review never writes the live session's idle marker. It may only
    /// read (`Read`, `Grep`, `Glob` allowed; `Bash`, `Edit`, `Write`,
    /// `NotebookEdit` disallowed); `review.md` is in the run directory.
    fn review_command(&self, run: &TaskRun, prompt: &str) -> Result<CommandSpec> {
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        let settings = run_dir.join("claude-review-settings.json");
        fs::write(&settings, review_settings()?)
            .with_context(|| format!("write {}", settings.display()))?;
        let mut command = CommandSpec::new(&self.executable);
        command
            .current_dir(run.worktree_path().context("missing worktree")?)
            .arg("-p")
            .arg("--debug-file")
            .arg(run_dir.join("claude-review.log"))
            .arg("--add-dir")
            .arg(run_dir)
            .arg("--settings")
            .arg(&settings)
            .arg("--allowedTools")
            .arg("Read,Grep,Glob")
            // The live worker session owns the worktree: the review never
            // edits it or runs commands in it.
            .arg("--disallowedTools")
            .arg("Bash,Edit,Write,NotebookEdit")
            .arg("--")
            .arg(prompt);
        Ok(command)
    }
}

/// Settings of the headless review: no hooks, and the same `autoMode`
/// environment as a run session (see [`stop_hook_settings`]).
pub fn review_settings() -> Result<String> {
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "autoMode": {
            "environment": ["$defaults"]
        }
    }))?)
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
    use crate::domain::{PlannerId, ProposalId, Provider, RunId, RunStatus, SessionRole};

    #[test]
    fn a_long_paste_gets_more_time_before_its_enter() {
        assert_eq!(paste_settle(5), Duration::from_millis(300));
        assert_eq!(paste_settle(13_000), Duration::from_millis(1600));
        assert_eq!(paste_settle(1_000_000), Duration::from_secs(3));
        assert_eq!(single_line("a\\b\n\n  c\t"), "a/b   c");
    }

    #[test]
    fn system_processes_signal_a_child_and_see_it_gone() {
        let processes = SystemProcesses;
        for send in [
            SystemProcesses::terminate,
            SystemProcesses::interrupt,
            SystemProcesses::kill,
        ] {
            let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
            assert!(processes.alive(child.id()));
            send(&processes, child.id()).unwrap();
            assert!(!child.wait().unwrap().success());
            assert!(!processes.alive(child.id()));
            let error = send(&processes, child.id()).unwrap_err().to_string();
            assert!(error.contains(&format!("pid {}", child.id())), "{error}");
        }
        assert!(!process_alive(u32::MAX));
        assert!(processes.kill(u32::MAX).is_err());
    }

    fn git_in(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
    }

    /// A repository on `main` with one committed `change.txt`.
    fn committed_repository() -> (tempfile::TempDir, GitRepository) {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@example.com"],
            &["config", "user.name", "t"],
        ] {
            assert!(git_in(dir.path(), args).status.success());
        }
        fs::write(dir.path().join("change.txt"), "0\n").unwrap();
        assert!(git_in(dir.path(), &["add", "change.txt"]).status.success());
        assert!(
            git_in(dir.path(), &["commit", "-q", "-m", "c"])
                .status
                .success()
        );
        let git = GitRepository::inspect(dir.path()).unwrap();
        (dir, git)
    }

    /// The supervisor's `status` leaves the index as it found it, where a
    /// plain `git status` refreshes the stale stat data and writes the
    /// index back under `index.lock`.
    #[test]
    fn worktree_status_does_not_write_back_the_index() {
        let (dir, git) = committed_repository();
        let index = dir.path().join(".git/index");
        let before = fs::read(&index).unwrap();
        // Same content, new stat data: the index entry is stale.
        std::thread::sleep(Duration::from_millis(20));
        fs::remove_file(dir.path().join("change.txt")).unwrap();
        fs::write(dir.path().join("change.txt"), "0\n").unwrap();

        assert_eq!(git.status(dir.path()).unwrap(), "");
        assert_eq!(
            git.current_branch(dir.path()).unwrap().as_deref(),
            Some("refs/heads/main")
        );
        git.head(dir.path()).unwrap();
        assert!(!git.rebase_in_progress(dir.path()).unwrap());
        assert!(git.conflicted_files(dir.path()).unwrap().is_empty());
        assert_eq!(fs::read(&index).unwrap(), before, "index was rewritten");

        assert!(
            git_in(dir.path(), &["status", "--porcelain"])
                .status
                .success()
        );
        assert_ne!(
            fs::read(&index).unwrap(),
            before,
            "a plain git status should refresh the stale index"
        );
    }

    /// A session's `git add` never meets the supervisor's `index.lock`
    /// while `status` polls the same worktree in a tight loop, and the
    /// index ends with what the session staged.
    #[test]
    fn worktree_status_polling_does_not_block_a_sessions_git_add() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (dir, git) = committed_repository();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let poller = {
            let (stop, git, path) = (stop.clone(), git.clone(), dir.path().to_owned());
            std::thread::spawn(move || {
                let mut polls = 0;
                loop {
                    git.status(&path).unwrap();
                    polls += 1;
                    if stop.load(Ordering::Relaxed) {
                        break polls;
                    }
                }
            })
        };
        for round in 1..=40 {
            fs::write(dir.path().join("change.txt"), format!("{round}\n")).unwrap();
            let add = git_in(dir.path(), &["add", "change.txt"]);
            assert!(
                add.status.success(),
                "round {round}: {}",
                String::from_utf8_lossy(&add.stderr)
            );
        }
        stop.store(true, Ordering::Relaxed);
        assert!(poller.join().unwrap() > 0);
        assert!(!dir.path().join(".git/index.lock").exists());
        let staged = git_in(dir.path(), &["show", ":change.txt"]);
        assert_eq!(String::from_utf8_lossy(&staged.stdout), "40\n");
        assert_eq!(git.status(dir.path()).unwrap(), "M  change.txt\n");
    }

    #[test]
    fn a_default_push_report_is_skipped_for_origin() {
        let report = crate::domain::PushReport::default();
        assert_eq!(report.outcome, crate::domain::PushResult::Skipped);
        assert_eq!(report.remote, "origin");
        assert!(report.error.is_none() && report.reason.is_none());
    }

    #[test]
    fn claude_headless_command_prints_with_only_the_allowed_tools() {
        let claude = ClaudeCode {
            executable: "/bin/claude".into(),
        };
        let command = claude
            .headless_command(Path::new("/tmp/obs"), "observe", &["Bash(dagq:*)"])
            .unwrap();
        assert_eq!(command.get_program(), "/bin/claude");
        assert_eq!(command.get_current_dir(), Some(Path::new("/tmp/obs")));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["-p", "--allowedTools", "Bash(dagq:*)", "--", "observe"]
        );
        let bare = claude
            .headless_command(Path::new("/tmp"), "p", &[])
            .unwrap();
        assert_eq!(bare.get_args().collect::<Vec<_>>(), ["-p", "--", "p"]);
    }

    fn run(repo_path: Option<&str>) -> TaskRun {
        TaskRun::restore(record(repo_path)).unwrap()
    }

    fn record(repo_path: Option<&str>) -> crate::domain::RunRecord {
        crate::domain::RunRecord {
            id: RunId::new("0d8e3f1a-7c1b-4e35-9a11-3f6d2c9b8e47").unwrap(),
            task_id: TaskId::new(15),
            status: RunStatus::Claimed,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            base_commit: CommitSha::try_from("a".repeat(40)).unwrap(),
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
        Task::restore(crate::domain::TaskRecord {
            id: TaskId::new(15),
            title: title.into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            status: crate::domain::TaskStatus::InProgress,
            goal_id: None,
            context: String::new(),
            created_at: "2026-09-22 00:00:00".into(),
            updated_at: "2026-09-22 00:00:00".into(),
        })
        .unwrap()
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
        // A root with no basename falls back to the path itself.
        assert_eq!(
            planner_workspace_name(Path::new("/"), PlannerId::new(1), None),
            "[/]planner#1"
        );
        assert_eq!(
            supervisor_workspace_name(Path::new("/home/u/ghq/dagq")),
            "[dagq]supervisor"
        );
        assert_eq!(
            planner_workspace_name(
                Path::new("/home/u/ghq/dagq"),
                PlannerId::new(3),
                Some(ProposalId::new(7))
            ),
            "[dagq]planner#3 - proposal 7"
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
                Some(&RunId::new("0d8e3f1a-7c1b-4e35-9a11-3f6d2c9b8e47").unwrap()),
                Some(TaskId::new(15))
            ),
            "dagq role=worker queue=77067154921b9014 run=0d8e3f1a-7c1b-4e35-9a11-3f6d2c9b8e47 task=15"
        );
        assert_eq!(
            workspace_description(SessionRole::Inbox, "abc", None, None),
            "dagq role=inbox queue=abc"
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
                {"id": "4AC63CB7-3BE1-40A1-BCC4-CA0461685F01", "title": "[dagq]inbox"},
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
        assert!(!workspace_listed(&listing, "[dagq]inbox"));
        assert!(!workspace_listed(&serde_json::json!({}), "x"));
    }

    #[test]
    fn listed_workspaces_read_the_ids_and_descriptions() {
        let listing = serde_json::json!({"workspaces": [
            {"id": "A", "description": "dagq role=worker queue=q run=r task=1"},
            {"id": "B", "description": null}
        ]});
        assert_eq!(
            listed_workspaces(&listing).unwrap(),
            [
                ListedWorkspace {
                    id: "A".into(),
                    description: Some("dagq role=worker queue=q run=r task=1".into()),
                },
                ListedWorkspace {
                    id: "B".into(),
                    description: None,
                },
            ]
        );
        assert!(listed_workspaces(&serde_json::json!({})).is_err());
        assert!(listed_workspaces(&serde_json::json!({"workspaces": [{}]})).is_err());
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
    /// reads the UUID listing of every window, both through the real argv.
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
  list-windows) echo '[{{"id":"A"}},{{"id":"B"}}]' ;;
  workspace)
    case "$7" in
      A) echo '{{"window_id":"A","workspaces":[{{"id":"W-0"}}]}}' ;;
      B) echo '{{"window_id":"B","workspaces":[{{"id":"W-1"}}]}}' ;;
      *) echo 'Error: unavailable: TabManager not available' >&2; exit 1 ;;
    esac ;;
esac
"#,
                log = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let cmux = Cmux { executable };
        assert_eq!(cmux.ensure_group("abc", "[dagq]").unwrap(), "G-1");
        assert!(cmux.exists("W-0").unwrap());
        assert!(cmux.exists("w-1").unwrap(), "a workspace of another window");
        assert!(!cmux.exists("W-2").unwrap());
        assert_eq!(cmux.list_workspaces().unwrap().len(), 2);
        let calls = fs::read_to_string(&log).unwrap();
        assert!(
            calls.contains(
                "--json --id-format uuids workspace-group create --name [dagq] --external-id abc"
            ),
            "{calls}"
        );
        assert!(calls.contains("--json --id-format uuids list-windows"));
        assert!(calls.contains("--json --id-format uuids workspace list --window A"));
        assert!(calls.contains("--json --id-format uuids workspace list --window B"));
    }

    /// The windows' listings are concatenated; a window that cannot be
    /// listed, or a reply of the wrong shape, fails the whole listing.
    #[test]
    fn merged_workspace_listing_joins_every_window() {
        let windows = serde_json::json!([{"id": "A"}, {"id": "B"}]);
        let merged = merged_workspace_listing(&windows, |window| {
            Ok(serde_json::json!({"window_id": window, "workspaces": [{"id": format!("{window}-1")}]}))
        })
        .unwrap();
        assert_eq!(
            merged,
            serde_json::json!({"workspaces": [{"id": "A-1"}, {"id": "B-1"}]})
        );
        assert!(workspace_listed(&merged, "b-1"));
        assert_eq!(
            merged_workspace_listing(&serde_json::json!([]), |_| unreachable!()).unwrap(),
            serde_json::json!({"workspaces": []})
        );
        let error = merged_workspace_listing(&windows, |window| {
            ensure!(window == "A", "TabManager not available");
            Ok(serde_json::json!({"workspaces": []}))
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("window B"), "{error:#}");
        assert!(merged_workspace_listing(&serde_json::json!({}), |_| unreachable!()).is_err());
        assert!(merged_workspace_listing(&serde_json::json!([{}]), |_| unreachable!()).is_err());
        assert!(merged_workspace_listing(&windows, |_| Ok(serde_json::json!({}))).is_err());
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
        let mut record = record(Some("/home/u/ghq/dagq"));
        record.status = RunStatus::Starting;
        record.worktree_path = Some(dir.path().display().to_string());
        record.run_dir = Some(run_dir.display().to_string());
        let run = TaskRun::restore(record).unwrap();
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
