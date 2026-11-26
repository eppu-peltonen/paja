//! Paja's home directory: where the event log, the lock and the environments live.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::events::EventLog;

/// How long to wait for another `paja` process to release the home lock. Generous, because a
/// writer may hold it through a full checkout of a large repository.
const LOCK_TIMEOUT: Duration = Duration::from_secs(300);

/// The directory Paja keeps its state in: `$PAJA_HOME`, or `~/.paja` when unset.
#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// Resolve the home directory from the environment and create it if needed.
    pub fn from_env() -> Result<Self> {
        let root = match std::env::var_os("PAJA_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => {
                let home = crate::platform::home_dir().context(
                    "PAJA_HOME is unset and no home directory was found (HOME, or on Windows an \
                     absolute HOME, HOMEDRIVE and HOMEPATH, or USERPROFILE); set PAJA_HOME",
                )?;
                home.join(".paja")
            }
        };
        Self::at(root)
    }

    /// Use `root` as the home directory, creating it if needed.
    pub fn at(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        // Stored paths must not depend on the cwd of whichever process reads them later.
        let root =
            std::path::absolute(&root).with_context(|| format!("resolving {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn events(&self) -> EventLog {
        EventLog::new(self.root.join("events.jsonl"))
    }

    pub fn envs_dir(&self) -> PathBuf {
        self.root.join("envs")
    }

    /// Take the home lock. Every write to the event log happens while holding it, so
    /// sequence numbers and port blocks are never handed out twice. It is an OS file lock, so
    /// the operating system releases it if the process dies, even on Ctrl-C or a crash.
    pub fn lock(&self) -> Result<HomeLock> {
        let path = self.root.join("lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(HomeLock { _file: file }),
                Err(TryLockError::WouldBlock) => {
                    if started.elapsed() > LOCK_TIMEOUT {
                        bail!(
                            "timed out after {}s waiting for another paja process to release {}",
                            LOCK_TIMEOUT.as_secs(),
                            path.display()
                        );
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(TryLockError::Error(err)) => {
                    return Err(err).with_context(|| format!("locking {}", path.display()))
                }
            }
        }
    }
}

impl Home {
    /// Take the lock a `paja run` holds while it works on item `number` of `repo`
    /// (`owner/name`), without waiting. None when another live run holds it. Like the home lock
    /// it is an OS file lock, so it is free again once that run's process is gone, however it
    /// stopped.
    pub fn try_run_lock(&self, repo: &str, number: u64) -> Result<Option<RunLock>> {
        self.try_runs_lock(repo, &number.to_string())
    }

    /// Take the lock a `paja run --workers` pool holds for `repo` for as long as it runs,
    /// without waiting. None when a live pool holds it, so one repository has one pool.
    pub fn try_pool_lock(&self, repo: &str) -> Result<Option<RunLock>> {
        self.try_runs_lock(repo, "pool")
    }

    /// Take the lock a run holds while it lands a change in `repo`, without waiting. None when
    /// another live run is landing there, so one change lands at a time and each lands on the
    /// one before it.
    pub fn try_land_lock(&self, repo: &str) -> Result<Option<RunLock>> {
        self.try_runs_lock(repo, "land")
    }

    /// A lock file `runs/<owner>@<name>@<tail>.lock`, taken without waiting.
    fn try_runs_lock(&self, repo: &str, tail: &str) -> Result<Option<RunLock>> {
        let dir = self.root.join("runs");
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        // `owner/name` as one file name: GitHub names hold only letters, digits and `-_.`, and
        // `@` can't appear in them, so two repositories never share a file.
        let repo: String = repo
            .chars()
            .map(|c| match c {
                '/' => '@',
                c if c.is_ascii_alphanumeric() || "-_.".contains(c) => c,
                _ => '_',
            })
            .collect();
        let path = dir.join(format!("{repo}@{tail}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(RunLock { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(err)) => {
                Err(err).with_context(|| format!("locking {}", path.display()))
            }
        }
    }
}

/// Held by the `paja run` working on an item, for as long as it works on it (or by a pool, for
/// as long as it runs).
#[derive(Debug)]
pub struct RunLock {
    _file: File,
}

/// Held while writing; the lock is released when this is dropped or the process exits.
#[derive(Debug)]
pub struct HomeLock {
    _file: File,
}
