//! The append-only event log: one JSON object per line, the core's single source of truth.
//!
//! Nothing is ever rewritten; the one exception is a write cut off mid-event, see `repair_tail`.
//! Current state (which environments exist, and later the queue and the inbox) is derived by
//! replaying the log.

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
    /// Bytes after the last newline that are not a whole event are a write that was cut off
    /// (crash, full disk). It was never acknowledged, so it is skipped rather than treated as
    /// corruption. Any bad line that ends in a newline is an error naming its line.
    pub fn read_all(&self) -> Result<Vec<Event>> {
        let Some(bytes) = self.read_bytes()? else {
            return Ok(Vec::new());
        };
        let (complete, tail) = split_tail(&bytes);
        let mut events = Vec::new();
        for (index, line) in complete.split(|&b| b == b'\n').enumerate() {
            let at = || format!("{} line {}", self.path.display(), index + 1);
            let line = std::str::from_utf8(line)
                .with_context(|| format!("{} is not valid UTF-8", at()))?;
            if line.trim().is_empty() {
                continue;
            }
            let event: Event = serde_json::from_str(line)
                .with_context(|| format!("{} is not a valid event", at()))?;
            events.push(event);
        }
        if let Tail::Event(event) = classify_tail(tail) {
            events.push(event);
        }
        Ok(events)
    }

    /// The last `limit` events (all of them when `limit` is None), oldest first.
    pub fn tail(&self, limit: Option<usize>) -> Result<Vec<Event>> {
        let mut events = self.read_all()?;
        if let Some(limit) = limit {
            let skip = events.len().saturating_sub(limit);
            events.drain(..skip);
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

    fn read_bytes(&self) -> Result<Option<Vec<u8>>> {
        match fs::read(&self.path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    /// Before appending, make sure the log ends on a line boundary: finish a whole event that
    /// lost its newline, or cut off an unfinished write and keep its bytes in
    /// `events.jsonl.torn`. Only that unacknowledged tail is ever touched; a newline-terminated
    /// line is never changed or dropped.
    fn repair_tail(&self) -> Result<()> {
        let Some(bytes) = self.read_bytes()? else {
            return Ok(());
        };
        let (complete, tail) = split_tail(&bytes);
        let repair = match classify_tail(tail) {
            Tail::None => return Ok(()),
            Tail::Event(_) => Repair::AddNewline,
            Tail::Torn => {
                // Keep the dropped bytes beside the log so nothing vanishes unseen.
                let aside = self.path.with_extension("jsonl.torn");
                let mut kept = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&aside)
                    .with_context(|| format!("opening {}", aside.display()))?;
                kept.write_all(tail)
                    .and_then(|()| kept.write_all(b"\n"))
                    .and_then(|()| kept.sync_data())
                    .with_context(|| format!("saving a torn write to {}", aside.display()))?;
                Repair::Truncate(complete.len() as u64)
            }
        };
        let mut file = OpenOptions::new()
            .write(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        let ctx = || format!("repairing the last line of {}", self.path.display());
        match repair {
            Repair::AddNewline => {
                file.seek(SeekFrom::End(0)).with_context(ctx)?;
                file.write_all(b"\n").with_context(ctx)?;
            }
            Repair::Truncate(len) => file.set_len(len).with_context(ctx)?,
        }
        file.sync_data().with_context(ctx)?;
        Ok(())
    }
}

enum Repair {
    AddNewline,
    Truncate(u64),
}

/// What the bytes after the last newline are.
///
/// `append` writes each event and its newline in one call and reports success only after
/// that, so bytes after the last newline were never acknowledged: they are either a whole
/// event that only lost its newline, or a write that stopped part-way (or junk such as the
/// NUL padding a power cut can leave).
enum Tail {
    /// The log ends on a newline.
    None,
    /// A whole event that only lost its newline.
    Event(Event),
    /// Anything else: an unacknowledged, unfinished write.
    Torn,
}

fn classify_tail(tail: &[u8]) -> Tail {
    if tail.is_empty() {
        return Tail::None;
    }
    match std::str::from_utf8(tail)
        .ok()
        .and_then(|text| serde_json::from_str::<Event>(text).ok())
    {
        Some(event) => Tail::Event(event),
        None => Tail::Torn,
    }
}

/// Split the log into its newline-terminated part and the bytes after the last newline.
fn split_tail(bytes: &[u8]) -> (&[u8], &[u8]) {
    let cut = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    bytes.split_at(cut)
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

    #[test]
    fn junk_after_the_last_newline_is_set_aside_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        let log = home.events();
        let lock = home.lock().unwrap();
        log.append(&lock, "test.one", json!({"ratio": 1.5}))
            .unwrap();
        let path = dir.path().join("events.jsonl");
        let good = std::fs::read(&path).unwrap();
        // NUL padding from a power cut, and a number cut after its '.'.
        for junk in [
            &b"\0\0\0\0"[..],
            b"{\"v\":1,\"seq\":2,\"data\":{\"r\":1.",
            b"{}\xc3",
        ] {
            let mut bytes = good.clone();
            bytes.extend_from_slice(junk);
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(log.read_all().unwrap().len(), 1);
            log.repair_tail().unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), good, "after {junk:?}");
        }
        let aside = std::fs::read(dir.path().join("events.jsonl.torn")).unwrap();
        assert!(aside.starts_with(b"\0\0\0\0\n"));
    }

    #[test]
    fn a_bad_line_names_its_line_even_for_bad_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, b"\n\xff\n").unwrap();
        let err = EventLog::new(path).read_all().unwrap_err();
        assert!(format!("{err:#}").contains("line 2"), "{err:#}");
    }

    #[test]
    fn a_write_cut_inside_a_character_is_torn() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        let log = home.events();
        let lock = home.lock().unwrap();
        log.append(&lock, "test.one", json!({})).unwrap();
        let path = dir.path().join("events.jsonl");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{\"v\":1,\"seq\":2,\"kind\":\"\xc3");
        std::fs::write(&path, &bytes).unwrap();

        assert_eq!(log.read_all().unwrap().len(), 1);
        assert_eq!(log.append(&lock, "test.two", json!({})).unwrap().seq, 2);
        assert_eq!(log.read_all().unwrap().len(), 2);
    }
}
