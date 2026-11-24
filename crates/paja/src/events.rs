//! The append-only event log: one JSON object per line, the core's single source of truth.
//!
//! Nothing is ever rewritten (the one exception is a torn final write, see `repair_tail`). Current state (which environments exist, and later the queue and
//! the inbox) is derived by replaying the log.

use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
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
    ///
    /// A last line with no trailing newline that does not parse is a write that was cut off
    /// (crash, full disk). It was never acknowledged, so it is skipped rather than treated as
    /// corruption. Any other bad line is an error.
    pub fn read_all(&self) -> Result<Vec<Event>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", self.path.display()))
            }
        };
        let (complete, torn) = split_torn_tail(&text);
        let mut events = Vec::new();
        for (index, line) in complete.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let event: Event = serde_json::from_str(line).with_context(|| {
                format!(
                    "{} line {} is not a valid event",
                    self.path.display(),
                    index + 1
                )
            })?;
            events.push(event);
        }
        if let Some(Ok(event)) = torn.map(serde_json::from_str::<Event>) {
            // Fully written apart from its newline: it counts.
            events.push(event);
        }
        Ok(events)
    }

    /// Append one event. Requires the home lock so sequence numbers stay unique.
    pub fn append(&self, _lock: &HomeLock, kind: &str, data: Value) -> Result<Event> {
        self.repair_tail()?;
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

    /// Before appending, make sure the log ends on a line boundary: finish a complete event
    /// that lost its newline, or cut off a torn write that never completed. Only that
    /// unacknowledged tail is ever touched; earlier events are never changed.
    fn repair_tail(&self) -> Result<()> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", self.path.display()))
            }
        };
        let (complete, torn) = split_torn_tail(&text);
        let Some(torn) = torn else { return Ok(()) };
        let file = OpenOptions::new()
            .write(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        if serde_json::from_str::<Event>(torn).is_ok() {
            let mut file = file;
            let ctx = || format!("repairing the last line of {}", self.path.display());
            file.seek(SeekFrom::End(0)).with_context(ctx)?;
            file.write_all(b"\n").with_context(ctx)?;
            file.sync_data().with_context(ctx)?;
        } else {
            file.set_len(complete.len() as u64)
                .with_context(|| format!("dropping torn write in {}", self.path.display()))?;
            file.sync_data()
                .with_context(|| format!("syncing {}", self.path.display()))?;
        }
        Ok(())
    }
}

/// Split the log into its newline-terminated part and a trailing fragment without a newline.
fn split_torn_tail(text: &str) -> (&str, Option<&str>) {
    match text.rfind('\n') {
        Some(i) if i + 1 == text.len() => (text, None),
        Some(i) => (&text[..=i], Some(&text[i + 1..])),
        None if text.is_empty() => (text, None),
        None => ("", Some(text)),
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

    #[test]
    fn a_torn_last_write_is_skipped_and_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        let log = home.events();
        let lock = home.lock().unwrap();
        log.append(&lock, "test.one", json!({})).unwrap();
        let path = dir.path().join("events.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"v\":1,\"seq\":2,\"ts_");
        std::fs::write(&path, &text).unwrap();

        assert_eq!(log.read_all().unwrap().len(), 1);
        let next = log.append(&lock, "test.two", json!({})).unwrap();
        assert_eq!(next.seq, 2);
        assert_eq!(log.read_all().unwrap().len(), 2);
    }
}
