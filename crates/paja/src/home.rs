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
        self.lock_within(LOCK_TIMEOUT)
    }

    /// Take the home lock as [`Home::lock`] does, but give up after `limit`: for a write that
    /// matters less than not holding up the caller, such as a Posti notice's record.
    pub fn lock_within(&self, limit: Duration) -> Result<HomeLock> {
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
                    if started.elapsed() > limit {
                        bail!(
                            "timed out after {}s waiting for another paja process to release {}",
                            limit.as_secs(),
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

    /// Take the lock Posti holds for `repo` while it decides which skipped-role notices are new
    /// and sends them, waiting up to `limit`. None when another process still holds it then, so
    /// a pool's lanes starting together send each notice once. It orders processes and stores
    /// nothing: whether a role was told is read from the log.
    pub fn posti_lock_within(&self, repo: &str, limit: Duration) -> Result<Option<RunLock>> {
        let started = Instant::now();
        loop {
            if let Some(lock) = self.try_runs_lock(repo, "posti")? {
                return Ok(Some(lock));
            }
            if started.elapsed() > limit {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Take the lock a run holds while it lands a change in `repo`, without waiting. None when
    /// another live run is landing there, so one change lands at a time and each lands on the
    /// one before it.
    pub fn try_land_lock(&self, repo: &str) -> Result<Option<RunLock>> {
        self.try_runs_lock(repo, "land")
    }

    /// Take the lock a `paja triage` pass holds for `repo` while it runs, without waiting. None
    /// when another live pass holds it, so two passes never share the triage checkout.
    pub fn try_triage_lock(&self, repo: &str) -> Result<Option<RunLock>> {
        self.try_runs_lock(repo, "triage")
    }

    /// Whether a live run holds item `number` of `repo`'s run lock, found without writing: a
    /// lock file that isn't there yet is held by no one, so a probe never creates one.
    pub fn run_held(&self, repo: &str, number: u64) -> Result<bool> {
        let path = self.runs_lock_path(repo, &number.to_string());
        let file = match OpenOptions::new().read(true).open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err).with_context(|| format!("opening {}", path.display())),
        };
        match file.try_lock() {
            // Free: the probe lets go at once (the file closes here).
            Ok(()) => Ok(false),
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(err)) => {
                Err(err).with_context(|| format!("locking {}", path.display()))
            }
        }
    }

    /// Mark `repo` as having a `paja run` for as long as the returned lock lives: a shared lock,
    /// so a pool and its lanes, or runs on different items, all hold it together. It covers the
    /// gaps no item's run lock does (a loop sleeping between items, a run still starting).
    pub fn hold_repo_run(&self, repo: &str) -> Result<RunLock> {
        let dir = self.root.join("runs");
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = self.runs_lock_path(repo, "any");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        // A probe (`repo_run_held`) holds it exclusively for an instant; wait that out.
        let started = Instant::now();
        loop {
            match file.try_lock_shared() {
                Ok(()) => return Ok(RunLock { _file: file }),
                Err(TryLockError::WouldBlock) => {
                    if started.elapsed() > Duration::from_secs(5) {
                        bail!(
                            "{} stayed locked exclusively for 5s; end any other `paja` \
                             process using this home and run again",
                            path.display()
                        );
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(TryLockError::Error(err)) => {
                    return Err(err).with_context(|| format!("locking {}", path.display()))
                }
            }
        }
    }

    /// Whether any live `paja run` holds [`Self::hold_repo_run`]'s marker for `repo`, found
    /// without writing.
    pub fn repo_run_held(&self, repo: &str) -> Result<bool> {
        let path = self.runs_lock_path(repo, "any");
        let file = match OpenOptions::new().read(true).open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err).with_context(|| format!("opening {}", path.display())),
        };
        match file.try_lock() {
            Ok(()) => Ok(false),
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(err)) => {
                Err(err).with_context(|| format!("locking {}", path.display()))
            }
        }
    }

    /// The path of lock file `runs/<owner>@<name>@<tail>.lock`.
    fn runs_lock_path(&self, repo: &str, tail: &str) -> PathBuf {
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
        self.root.join("runs").join(format!("{repo}@{tail}.lock"))
    }

    /// A lock file `runs/<owner>@<name>@<tail>.lock`, taken without waiting.
    fn try_runs_lock(&self, repo: &str, tail: &str) -> Result<Option<RunLock>> {
        let dir = self.root.join("runs");
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = self.runs_lock_path(repo, tail);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_taken_within_a_limit_gives_up_at_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        let held = home.lock().unwrap();
        let started = Instant::now();
        let err = home.lock_within(Duration::from_millis(50)).err().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it waited past the limit"
        );
        assert!(format!("{err:#}").contains("timed out after"), "{err:#}");
        drop(held);
        assert!(home.lock_within(Duration::from_millis(50)).is_ok());
    }

    #[test]
    fn the_repo_run_marker_is_shared_by_runs_and_seen_by_a_probe() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        let repo = "owner/name";
        assert!(!home.repo_run_held(repo).unwrap());
        let first = home.hold_repo_run(repo).unwrap();
        let second = home.hold_repo_run(repo).unwrap();
        assert!(home.repo_run_held(repo).unwrap());
        assert!(!home.repo_run_held("owner/other").unwrap());
        drop(first);
        assert!(home.repo_run_held(repo).unwrap());
        drop(second);
        assert!(!home.repo_run_held(repo).unwrap());
    }

    #[test]
    fn run_held_sees_only_a_live_run_lock() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        let repo = "owner/name";

        // No lock file yet: free, and the probe creates neither the file nor `runs/`.
        assert!(!home.run_held(repo, 7).unwrap());
        assert!(!home.root().join("runs").exists());

        // A live run holds it: held. Another item of the same repository stays free.
        let lock = home
            .try_run_lock(repo, 7)
            .unwrap()
            .expect("the lock is free");
        assert!(home.run_held(repo, 7).unwrap());
        assert!(!home.run_held(repo, 8).unwrap());
        // The probe took nothing, so the lock is still the run's.
        assert!(home.try_run_lock(repo, 7).unwrap().is_none());

        // The run is gone but its file is left behind: free again.
        drop(lock);
        assert!(home.runs_lock_path(repo, "7").exists());
        // A child another test forked in the instant before the drop shares the lock until it
        // execs, so the lock may be let go a moment later. `run_held` probes by taking the lock
        // for an instant, and a child forked during a probe shares that too, so the next run
        // may find it held for a moment as well: both checks wait out such a child.
        let deadline = Instant::now() + Duration::from_secs(10);
        let wait = |what: &str| {
            assert!(Instant::now() < deadline, "{what}");
            thread::sleep(Duration::from_millis(20));
        };
        while home.run_held(repo, 7).unwrap() {
            wait("the dropped run lock is still held");
        }
        while home.try_run_lock(repo, 7).unwrap().is_none() {
            wait("a new run can't take the dropped run lock");
        }
    }
}
