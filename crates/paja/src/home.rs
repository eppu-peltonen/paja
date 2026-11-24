//! Paja's home directory: where the event log, the lock and the environments live.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::events::EventLog;

/// How long to wait for another `paja` process to release the home lock.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

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
                let home = std::env::var_os("HOME").context("neither PAJA_HOME nor HOME is set")?;
                PathBuf::from(home).join(".paja")
            }
        };
        Self::at(root)
    }

    /// Use `root` as the home directory, creating it if needed.
    pub fn at(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
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
    /// sequence numbers and port blocks are never handed out twice.
    pub fn lock(&self) -> Result<HomeLock> {
        let path = self.root.join("lock");
        let started = Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    // The pid is only there to help a person find a stuck holder.
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(HomeLock { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if started.elapsed() > LOCK_TIMEOUT {
                        bail!(
                            "timed out waiting for {} (if no other paja process is running, delete it)",
                            path.display()
                        );
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("creating {}", path.display()))
                }
            }
        }
    }
}

/// Held while writing; released when dropped.
#[derive(Debug)]
pub struct HomeLock {
    path: PathBuf,
}

impl Drop for HomeLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
