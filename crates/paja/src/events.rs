//! The append-only event log: one JSON object per line, the core's single source of truth.
//!
//! Nothing is ever rewritten. Current state (which environments exist, and later the queue and
//! the inbox) is derived by replaying the log.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::home::HomeLock;

/// Version of the event envelope. Bump only with a migration plan: old logs must stay readable.
pub const EVENT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Envelope version.
    pub v: u32,
    /// Position in the log, starting at 1 and never reused.
    pub seq: u64,
    /// Milliseconds since the Unix epoch when the event was written.
    pub ts_ms: u64,
    /// What happened, as `area.verb`, for example `env.created`.
    pub kind: String,
    /// Event-specific payload.
    pub data: Value,
}

#[derive(Debug, Clone)]
pub struct EventLog {
    path: PathBuf,
}

impl EventLog {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Read every event in order. A missing log is an empty log.
    pub fn read_all(&self) -> Result<Vec<Event>> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("opening {}", self.path.display()))
            }
        };
        let mut events = Vec::new();
        for (index, line) in BufReader::new(file).lines().enumerate() {
            let line = line.with_context(|| format!("reading {}", self.path.display()))?;
            if line.trim().is_empty() {
                continue;
            }
            let event: Event = serde_json::from_str(&line).with_context(|| {
                format!(
                    "{} line {} is not a valid event",
                    self.path.display(),
                    index + 1
                )
            })?;
            events.push(event);
        }
        Ok(events)
    }

    /// Append one event. Requires the home lock so sequence numbers stay unique.
    pub fn append(&self, _lock: &HomeLock, kind: &str, data: Value) -> Result<Event> {
        let seq = self.read_all()?.last().map_or(1, |last| last.seq + 1);
        let event = Event {
            v: EVENT_VERSION,
            seq,
            ts_ms: now_ms(),
            kind: kind.to_string(),
            data,
        };
        let mut line = serde_json::to_string(&event)?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        file.write_all(line.as_bytes())
            .with_context(|| format!("writing {}", self.path.display()))?;
        file.sync_data()
            .with_context(|| format!("syncing {}", self.path.display()))?;
        Ok(event)
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::Home;
    use serde_json::json;

    #[test]
    fn appends_in_order_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        let log = home.events();
        assert!(log.read_all().unwrap().is_empty());

        let lock = home.lock().unwrap();
        let first = log.append(&lock, "test.one", json!({"a": 1})).unwrap();
        let second = log.append(&lock, "test.two", json!({})).unwrap();
        drop(lock);

        assert_eq!((first.seq, second.seq), (1, 2));
        let all = log.read_all().unwrap();
        assert_eq!(all, vec![first, second]);
    }

    #[test]
    fn rejects_a_corrupt_line_with_its_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, "not json\n").unwrap();
        let err = EventLog::new(path).read_all().unwrap_err();
        assert!(format!("{err:#}").contains("line 1"));
    }
}
