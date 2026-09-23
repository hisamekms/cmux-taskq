//! Where a queue lives. Without `--db`, the queue of the repository containing
//! the working directory is `<data home>/dagq/<hash>/queue.db`, where the
//! hash identifies the repository's Git common directory. Runs, worktrees and
//! their logs live next to the database in `runs/`, the supervisor's logs in
//! `logs/`, and the LaunchAgent that keeps the supervisor resident is named
//! after the queue under `~/Library/LaunchAgents`.
use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use super::adapters::git_common_dir;

pub const DATA_DIR_NAME: &str = "dagq";
pub const DB_FILE_NAME: &str = "queue.db";
pub const RUNS_DIR_NAME: &str = "runs";
/// Supervisor logs (`supervisor-<started_at>-<pid>.log`, `launchd.log`).
pub const LOGS_DIR_NAME: &str = "logs";
/// LaunchAgent labels are `com.dagq.<queue hash>`.
pub const LAUNCH_AGENT_PREFIX: &str = "com.dagq";
/// Human-readable pointer back from a hashed queue directory to its repository.
pub const REPOSITORY_FILE_NAME: &str = "repository";
const HASH_HEX_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueSource {
    /// `--db PATH` was given; nothing about the working directory is assumed.
    DbFlag,
    /// Resolved from the repository containing the working directory.
    Repository,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueLocation {
    pub db: PathBuf,
    pub queue_dir: PathBuf,
    pub runs_dir: PathBuf,
    pub log_dir: PathBuf,
    /// launchd label of the queue's supervisor agent.
    pub label: String,
    /// The agent's plist, whether or not `up` has written it yet.
    pub launch_agent: PathBuf,
    pub source: QueueSource,
    /// Canonical Git common directory the queue belongs to. Set only when
    /// resolved from the repository; `--db` queues are bound by `supervise`.
    pub git_common_dir: Option<PathBuf>,
}

impl QueueLocation {
    /// `--db PATH` wins; otherwise the repository containing `cwd` decides.
    pub fn resolve(db: Option<&Path>, cwd: &Path) -> Result<Self> {
        match db {
            Some(db) => Ok(Self::explicit(db)),
            None => {
                let common_dir = git_common_dir(cwd)
                    .context("run from inside the repository the queue belongs to, or pass --db")?;
                Ok(Self::for_repository(&common_dir, &data_home()?))
            }
        }
    }

    /// An explicit queue file is identified by the hash of its own path.
    pub fn explicit(db: &Path) -> Self {
        Self::explicit_in(db, &home_dir())
    }

    pub fn explicit_in(db: &Path, home: &Path) -> Self {
        let queue_dir = db.parent().map(Path::to_path_buf).unwrap_or_default();
        // `up` and `down` must agree on the label however the path was
        // spelled, so the hash is of the canonical path (of the directory
        // plus the file name when the file does not exist yet).
        let label = format!("{LAUNCH_AGENT_PREFIX}.{}", repository_hash(&canonical(db)));
        Self {
            runs_dir: queue_dir.join(RUNS_DIR_NAME),
            log_dir: queue_dir.join(LOGS_DIR_NAME),
            launch_agent: launch_agent_path(home, &label),
            label,
            db: db.to_path_buf(),
            queue_dir,
            source: QueueSource::DbFlag,
            git_common_dir: None,
        }
    }

    /// `common_dir` must already be canonical so the hash is stable across
    /// symlinks and worktrees.
    pub fn for_repository(common_dir: &Path, data_home: &Path) -> Self {
        Self::for_repository_in(common_dir, data_home, &home_dir())
    }

    pub fn for_repository_in(common_dir: &Path, data_home: &Path, home: &Path) -> Self {
        let hash = repository_hash(common_dir);
        let queue_dir = data_home.join(DATA_DIR_NAME).join(&hash);
        let label = format!("{LAUNCH_AGENT_PREFIX}.{hash}");
        Self {
            db: queue_dir.join(DB_FILE_NAME),
            runs_dir: queue_dir.join(RUNS_DIR_NAME),
            log_dir: queue_dir.join(LOGS_DIR_NAME),
            launch_agent: launch_agent_path(home, &label),
            label,
            queue_dir,
            source: QueueSource::Repository,
            git_common_dir: Some(common_dir.to_path_buf()),
        }
    }

    /// The queue hash that names the queue's workspace group and appears in
    /// its workspaces' descriptions (ADR-0026). A repository queue's
    /// directory is named after it, and so it stays when the same queue is
    /// reached by `--db` (as `up` starts `supervise`): a `queue.db` beside
    /// the `repository` file `prepare` writes is such a queue. Any other
    /// `--db` queue is identified by the hash its label carries.
    pub fn hash(&self) -> String {
        if self.source == QueueSource::DbFlag
            && self.db.file_name() == Some(DB_FILE_NAME.as_ref())
            && self.queue_dir.join(REPOSITORY_FILE_NAME).is_file()
            && let Some(name) = canonical(&self.queue_dir).file_name()
        {
            return name.to_string_lossy().into_owned();
        }
        self.label
            .strip_prefix(LAUNCH_AGENT_PREFIX)
            .and_then(|rest| rest.strip_prefix('.'))
            .unwrap_or(&self.label)
            .to_owned()
    }

    /// Create the queue directory before `init`. A repository queue also gets a
    /// `repository` file naming its Git common directory for humans.
    pub fn prepare(&self) -> Result<()> {
        fs::create_dir_all(&self.queue_dir)
            .with_context(|| format!("create {}", self.queue_dir.display()))?;
        if let Some(common_dir) = &self.git_common_dir {
            let text = common_dir
                .to_str()
                .context("runtime paths must be valid UTF-8")?;
            fs::write(
                self.queue_dir.join(REPOSITORY_FILE_NAME),
                format!("{text}\n"),
            )?;
        }
        Ok(())
    }
}

/// Runs of the queue at `db` live in `runs/` next to it.
pub fn runs_dir(db: &Path) -> PathBuf {
    QueueLocation::explicit(db).runs_dir
}

/// The canonical form of `path` when it exists, else of its nearest existing
/// parent joined with the rest; the path itself when nothing exists.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            canonical(parent).join(name)
        }
        _ => path.to_path_buf(),
    }
}

/// `~/Library/LaunchAgents/<label>.plist`, where launchd looks for per-user agents.
pub fn launch_agent_path(home: &Path, label: &str) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{label}.plist"))
}

/// `$HOME`, or an empty path when it is unset: the LaunchAgent path is then
/// relative, and `up` refuses to write it.
pub fn home_dir() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

/// SHA-256 of the canonical common directory, shortened; stable across
/// binaries and platforms.
pub fn repository_hash(common_dir: &Path) -> String {
    let digest = Sha256::digest(common_dir.as_os_str().as_encoded_bytes());
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()[..HASH_HEX_LEN]
        .to_owned()
}

/// `$XDG_DATA_HOME`, or `~/.local/share`.
pub fn data_home() -> Result<PathBuf> {
    data_home_from(env::var_os("XDG_DATA_HOME"), env::var_os("HOME"))
}

/// The XDG base directory spec says a relative or empty `XDG_DATA_HOME` must be
/// ignored, so it falls through to `HOME`, which must be absolute as well.
fn data_home_from(xdg: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    let absolute = |value: Option<OsString>| value.map(PathBuf::from).filter(|p| p.is_absolute());
    if let Some(xdg) = absolute(xdg) {
        return Ok(xdg);
    }
    let home = absolute(home).context("XDG_DATA_HOME and HOME are unset or not absolute")?;
    Ok(home.join(".local").join("share"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_short() {
        let hash = repository_hash(Path::new("/tmp/repo/.git"));
        assert_eq!(hash.len(), HASH_HEX_LEN);
        assert_eq!(hash, repository_hash(Path::new("/tmp/repo/.git")));
        assert_ne!(hash, repository_hash(Path::new("/tmp/repo2/.git")));
        // `printf '%s' /tmp/repo/.git | shasum -a 256`
        assert_eq!(hash, "fbf971f8b891f789");
    }

    #[test]
    fn data_home_ignores_relative_or_empty_xdg_and_falls_back_to_home() {
        let some = |s: &str| Some(OsString::from(s));
        assert_eq!(
            data_home_from(some("/xdg"), some("/home/u")).unwrap(),
            Path::new("/xdg")
        );
        for bad in ["", "relative/data"] {
            assert_eq!(
                data_home_from(some(bad), some("/home/u")).unwrap(),
                Path::new("/home/u/.local/share")
            );
        }
        assert_eq!(
            data_home_from(None, some("/home/u")).unwrap(),
            Path::new("/home/u/.local/share")
        );
        assert!(data_home_from(None, None).is_err());
        assert!(data_home_from(some("rel"), some("")).is_err());
    }

    #[test]
    fn repository_queue_lives_under_the_data_home() {
        let location = QueueLocation::for_repository(Path::new("/repo/.git"), Path::new("/data"));
        let hash = repository_hash(Path::new("/repo/.git"));
        assert_eq!(
            location.db,
            Path::new("/data/dagq").join(&hash).join("queue.db")
        );
        assert_eq!(
            location.runs_dir,
            Path::new("/data/dagq").join(&hash).join("runs")
        );
        assert_eq!(location.source, QueueSource::Repository);
        assert_eq!(
            location.git_common_dir.as_deref(),
            Some(Path::new("/repo/.git"))
        );
        let location = QueueLocation::for_repository_in(
            Path::new("/repo/.git"),
            Path::new("/data"),
            Path::new("/home/u"),
        );
        assert_eq!(
            location.log_dir,
            Path::new("/data/dagq").join(&hash).join("logs")
        );
        assert_eq!(location.label, format!("com.dagq.{hash}"));
        assert_eq!(
            location.launch_agent,
            Path::new("/home/u/Library/LaunchAgents").join(format!("com.dagq.{hash}.plist"))
        );
    }

    #[test]
    fn explicit_db_keeps_runs_next_to_it() {
        let location = QueueLocation::explicit(Path::new("/x/y/other.db"));
        assert_eq!(location.queue_dir, Path::new("/x/y"));
        assert_eq!(location.runs_dir, Path::new("/x/y/runs"));
        assert_eq!(runs_dir(Path::new("/x/y/other.db")), Path::new("/x/y/runs"));
        assert_eq!(location.source, QueueSource::DbFlag);
        assert!(location.git_common_dir.is_none());
        assert_eq!(location.log_dir, Path::new("/x/y/logs"));
        assert_eq!(
            QueueLocation::explicit(Path::new("bare.db")).runs_dir,
            Path::new("runs")
        );
        // The agent of an explicit queue is named after the file, not a repository.
        let explicit = QueueLocation::explicit_in(Path::new("/x/y/other.db"), Path::new("/home/u"));
        let hash = repository_hash(Path::new("/x/y/other.db"));
        assert_eq!(explicit.label, format!("com.dagq.{hash}"));
        assert_eq!(
            explicit.launch_agent,
            Path::new("/home/u/Library/LaunchAgents").join(format!("com.dagq.{hash}.plist"))
        );
        assert_ne!(
            explicit.label,
            QueueLocation::explicit_in(Path::new("/x/z/other.db"), Path::new("/home/u")).label
        );
        // The label does not depend on how the path was spelled.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("q").join("queue.db");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        let via_dots = dir.path().join("q").join("..").join("q").join("queue.db");
        assert_eq!(
            QueueLocation::explicit_in(&real, Path::new("/home/u")).label,
            QueueLocation::explicit_in(&via_dots, Path::new("/home/u")).label
        );
        fs::write(&real, "").unwrap();
        assert_eq!(
            QueueLocation::explicit_in(&real, Path::new("/home/u")).label,
            QueueLocation::explicit_in(&via_dots, Path::new("/home/u")).label
        );
        // Without HOME the plist path is relative; `up` refuses to write it.
        assert!(
            !QueueLocation::explicit_in(Path::new("/x/y/other.db"), Path::new(""))
                .launch_agent
                .is_absolute()
        );
    }

    /// The workspace group of a repository queue has the same external ID
    /// whether `up` resolved the queue from the repository or `supervise`
    /// was given its database by `--db`.
    #[test]
    fn queue_hash_is_the_repository_hash_however_the_queue_is_reached() {
        let dir = tempfile::tempdir().unwrap();
        let repository = QueueLocation::for_repository(Path::new("/repo/.git"), dir.path());
        let hash = repository_hash(Path::new("/repo/.git"));
        assert_eq!(repository.hash(), hash);
        // Before `prepare` writes the `repository` pointer, `--db` cannot
        // tell the directory from any other and hashes the path.
        let by_db = QueueLocation::explicit(&repository.db);
        assert_ne!(by_db.hash(), hash);
        repository.prepare().unwrap();
        assert_eq!(QueueLocation::explicit(&repository.db).hash(), hash);
        // A symlink to the directory names the same queue.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&repository.queue_dir, &link).unwrap();
        assert_eq!(
            QueueLocation::explicit(&link.join(DB_FILE_NAME)).hash(),
            hash
        );
        // Another file name in that directory is a queue of its own.
        let other = QueueLocation::explicit(&repository.queue_dir.join("other.db"));
        assert_eq!(
            other.hash(),
            repository_hash(&canonical(&repository.queue_dir.join("other.db")))
        );
    }

    #[test]
    fn prepare_creates_the_directory_and_repository_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let location = QueueLocation::for_repository(Path::new("/repo/.git"), dir.path());
        location.prepare().unwrap();
        assert!(location.queue_dir.is_dir());
        assert_eq!(
            fs::read_to_string(location.queue_dir.join(REPOSITORY_FILE_NAME)).unwrap(),
            "/repo/.git\n"
        );
        let explicit = QueueLocation::explicit(&dir.path().join("nested/explicit.db"));
        explicit.prepare().unwrap();
        assert!(explicit.queue_dir.is_dir());
        assert!(!explicit.queue_dir.join(REPOSITORY_FILE_NAME).exists());
    }
}
