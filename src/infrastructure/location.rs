//! Where a queue lives. Without `--db`, the queue of the repository containing
//! the working directory is `<data home>/cmux-taskq/<hash>/queue.db`, where the
//! hash identifies the repository's Git common directory. Runs, worktrees and
//! logs live next to the database in `runs/`.
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

pub const DATA_DIR_NAME: &str = "cmux-taskq";
pub const DB_FILE_NAME: &str = "queue.db";
pub const RUNS_DIR_NAME: &str = "runs";
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

    pub fn explicit(db: &Path) -> Self {
        let queue_dir = db.parent().map(Path::to_path_buf).unwrap_or_default();
        Self {
            runs_dir: queue_dir.join(RUNS_DIR_NAME),
            db: db.to_path_buf(),
            queue_dir,
            source: QueueSource::DbFlag,
            git_common_dir: None,
        }
    }

    /// `common_dir` must already be canonical so the hash is stable across
    /// symlinks and worktrees.
    pub fn for_repository(common_dir: &Path, data_home: &Path) -> Self {
        let queue_dir = data_home
            .join(DATA_DIR_NAME)
            .join(repository_hash(common_dir));
        Self {
            db: queue_dir.join(DB_FILE_NAME),
            runs_dir: queue_dir.join(RUNS_DIR_NAME),
            queue_dir,
            source: QueueSource::Repository,
            git_common_dir: Some(common_dir.to_path_buf()),
        }
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
            Path::new("/data/cmux-taskq").join(&hash).join("queue.db")
        );
        assert_eq!(
            location.runs_dir,
            Path::new("/data/cmux-taskq").join(&hash).join("runs")
        );
        assert_eq!(location.source, QueueSource::Repository);
        assert_eq!(
            location.git_common_dir.as_deref(),
            Some(Path::new("/repo/.git"))
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
        assert_eq!(
            QueueLocation::explicit(Path::new("bare.db")).runs_dir,
            Path::new("runs")
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
