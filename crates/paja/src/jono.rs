//! Jono: the work queue. Work comes from GitHub issues labelled `paja`; `sync` imports them
//! into the event log, and the queue's state is derived by replaying it like everything else.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::events::Event;
use crate::home::Home;

/// Most issues one sync reads. A sync that hits it refuses to close anything.
pub const FETCH_LIMIT: usize = 1000;

/// The label that puts an issue in Jono.
pub const LABEL: &str = "paja";

pub const EV_IMPORTED: &str = "work.imported";
pub const EV_CHANGED: &str = "work.changed";
pub const EV_CLOSED: &str = "work.closed";
pub const EV_REOPENED: &str = "work.reopened";
pub const EV_STATUS: &str = "work.status";

/// Where an item is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Waiting to be picked up.
    Queued,
    /// Being worked on by a run.
    Active,
    /// Landed on `main`.
    Landed,
    /// Waiting on a person's answer on the issue.
    Escalated,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Active => "active",
            Status::Landed => "landed",
            Status::Escalated => "escalated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    /// The GitHub issue number.
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    /// Whether the issue is open and labelled `paja`. A closed issue keeps its status, so an
    /// escalation's question or a landing survives the issue closing and reopening.
    pub open: bool,
    pub status: Status,
    /// Why the status last changed, for example the escalation question.
    #[serde(default)]
    pub note: Option<String>,
}

/// An issue as `gh issue list --json number,title,body,url` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub url: String,
}

/// What a sync changed.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncReport {
    pub imported: Vec<u64>,
    pub changed: Vec<u64>,
    pub closed: Vec<u64>,
    pub reopened: Vec<u64>,
}

#[derive(Deserialize)]
struct Number {
    number: u64,
}

#[derive(Deserialize)]
struct StatusChange {
    number: u64,
    status: Status,
    note: Option<String>,
}

/// The queue, derived by replaying the event log.
pub fn replay(events: &[Event]) -> Result<BTreeMap<u64, WorkItem>> {
    let mut items = BTreeMap::new();
    let bad = |event: &Event| format!("event {} ({}) has unexpected data", event.seq, event.kind);
    for event in events {
        match event.kind.as_str() {
            EV_IMPORTED => {
                let issue: Issue =
                    serde_json::from_value(event.data.clone()).with_context(|| bad(event))?;
                items.insert(
                    issue.number,
                    WorkItem {
                        number: issue.number,
                        title: issue.title,
                        body: issue.body,
                        url: issue.url,
                        open: true,
                        status: Status::Queued,
                        note: None,
                    },
                );
            }
            EV_CHANGED => {
                let issue: Issue =
                    serde_json::from_value(event.data.clone()).with_context(|| bad(event))?;
                if let Some(item) = items.get_mut(&issue.number) {
                    item.title = issue.title;
                    item.body = issue.body;
                    item.url = issue.url;
                }
            }
            EV_CLOSED | EV_REOPENED => {
                let n: Number =
                    serde_json::from_value(event.data.clone()).with_context(|| bad(event))?;
                if let Some(item) = items.get_mut(&n.number) {
                    item.open = event.kind == EV_REOPENED;
                    // A landed issue opened again (say after a revert) needs work again.
                    if item.open && item.status == Status::Landed {
                        item.status = Status::Queued;
                        item.note = None;
                    }
                }
            }
            EV_STATUS => {
                let change: StatusChange =
                    serde_json::from_value(event.data.clone()).with_context(|| bad(event))?;
                if let Some(item) = items.get_mut(&change.number) {
                    item.status = change.status;
                    item.note = change.note;
                }
            }
            _ => {}
        }
    }
    Ok(items)
}

pub fn list(home: &Home) -> Result<Vec<WorkItem>> {
    Ok(replay(&home.events().read_all()?)?.into_values().collect())
}

pub fn get(home: &Home, number: u64) -> Result<WorkItem> {
    replay(&home.events().read_all()?)?
        .remove(&number)
        .with_context(|| {
            format!(
                "no work item #{number}; run `paja work sync` to import issues labelled `{LABEL}`"
            )
        })
}

fn is_ready(item: &WorkItem) -> bool {
    item.open && item.status == Status::Queued
}

/// The next item to work on: the oldest open, queued issue. None when the queue is empty.
pub fn next(home: &Home) -> Result<Option<WorkItem>> {
    Ok(list(home)?.into_iter().find(is_ready))
}

/// Take the next item and mark it active in one step under the home lock, so two runs
/// never pick up the same item.
pub fn claim(home: &Home) -> Result<Option<WorkItem>> {
    let lock = home.lock()?;
    let log = home.events();
    let Some(mut item) = replay(&log.read_all()?)?.into_values().find(is_ready) else {
        return Ok(None);
    };
    log.append(
        &lock,
        EV_STATUS,
        json!({ "number": item.number, "status": Status::Active, "note": null }),
    )?;
    item.status = Status::Active;
    item.note = None;
    Ok(Some(item))
}

/// Read a status name as the CLI and MCP tools take it.
pub fn parse_status(name: &str) -> Result<Status> {
    serde_json::from_value(json!(name)).map_err(|_| {
        anyhow::anyhow!("unknown status {name:?}; use queued, active, landed or escalated")
    })
}

/// Record a status change, for example when a run picks an item up or escalates it.
pub fn set_status(
    home: &Home,
    number: u64,
    status: Status,
    note: Option<&str>,
) -> Result<WorkItem> {
    let lock = home.lock()?;
    let log = home.events();
    let mut item = replay(&log.read_all()?)?.remove(&number).with_context(|| {
        format!("no work item #{number}; run `paja work sync` to import issues labelled `{LABEL}`")
    })?;
    log.append(
        &lock,
        EV_STATUS,
        json!({ "number": number, "status": status, "note": note }),
    )?;
    item.status = status;
    item.note = note.map(str::to_string);
    Ok(item)
}

/// Import the open issues labelled `paja` from the GitHub repository of the checkout at `repo`.
pub fn sync(home: &Home, repo: &Path) -> Result<SyncReport> {
    if !repo.is_dir() {
        bail!(
            "{} is not a directory; pass --repo with a path inside the checkout",
            repo.display()
        );
    }
    // gh runs git in the checkout to find its GitHub repository.
    crate::ahjo::ensure_untampered(home, repo)?;
    let issues = fetch_issues(repo)?;
    apply(home, &issues)
}

/// Bring the queue in line with the current open issues. Separate from `sync` so it can be
/// tested without GitHub.
pub fn apply(home: &Home, open: &[Issue]) -> Result<SyncReport> {
    let lock = home.lock()?;
    let log = home.events();
    let items = replay(&log.read_all()?)?;
    let mut report = SyncReport::default();
    for issue in open {
        match items.get(&issue.number) {
            None => {
                log.append(&lock, EV_IMPORTED, serde_json::to_value(issue)?)?;
                report.imported.push(issue.number);
            }
            Some(item) => {
                if !item.open {
                    log.append(&lock, EV_REOPENED, json!({ "number": issue.number }))?;
                    report.reopened.push(issue.number);
                }
                if (&item.title, &item.body, &item.url) != (&issue.title, &issue.body, &issue.url) {
                    log.append(&lock, EV_CHANGED, serde_json::to_value(issue)?)?;
                    report.changed.push(issue.number);
                }
            }
        }
    }
    for item in items.values() {
        let still_open = open.iter().any(|issue| issue.number == item.number);
        if item.open && !still_open {
            log.append(&lock, EV_CLOSED, json!({ "number": item.number }))?;
            report.closed.push(item.number);
        }
    }
    Ok(report)
}

/// `gh`, or the program named by `PAJA_GH` (tests point it at a fake).
pub fn gh_program() -> String {
    std::env::var("PAJA_GH").unwrap_or_else(|_| "gh".to_string())
}

fn fetch_issues(repo: &Path) -> Result<Vec<Issue>> {
    let gh = gh_program();
    let output = Command::new(&gh)
        .current_dir(repo)
        .args([
            "issue",
            "list",
            "--label",
            LABEL,
            "--state",
            "open",
            "--limit",
            &FETCH_LIMIT.to_string(),
            "--json",
            "number,title,body,url",
        ])
        .output()
        .with_context(|| {
            format!("running `{gh}` (install the GitHub CLI and run `gh auth login`)")
        })?;
    if !output.status.success() {
        bail!(
            "`{gh} issue list` failed in {}: {}; check `gh auth status` and that this checkout has a GitHub remote",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let issues: Vec<Issue> = serde_json::from_slice(&output.stdout).with_context(|| {
        format!("`{gh} issue list` printed something that is not a list of issues")
    })?;
    if issues.len() >= FETCH_LIMIT {
        // A full page may be cut short, and closing what is missing would drop real work.
        bail!(
            "{FETCH_LIMIT} or more open issues are labelled `{LABEL}`, more than one sync reads; \
             close or unlabel some, then rerun"
        );
    }
    Ok(issues)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(number: u64, title: &str) -> Issue {
        Issue {
            number,
            title: title.into(),
            body: String::new(),
            url: format!("https://example.com/{number}"),
        }
    }

    #[test]
    fn sync_imports_changes_closes_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();

        let first = apply(&home, &[issue(2, "b"), issue(1, "a")]).unwrap();
        assert_eq!(first.imported, vec![2, 1]);
        assert_eq!(next(&home).unwrap().unwrap().number, 1, "oldest first");
        assert!(apply(&home, &[issue(2, "b"), issue(1, "a")])
            .unwrap()
            .imported
            .is_empty());

        let second = apply(&home, &[issue(2, "b2")]).unwrap();
        assert_eq!((second.changed, second.closed), (vec![2], vec![1]));
        assert!(!get(&home, 1).unwrap().open);
        assert_eq!(next(&home).unwrap().unwrap().number, 2);
        assert_eq!(get(&home, 2).unwrap().title, "b2");

        let third = apply(&home, &[issue(1, "a"), issue(2, "b2")]).unwrap();
        assert_eq!(third.reopened, vec![1]);
        assert!(get(&home, 1).unwrap().open);
        assert_eq!(next(&home).unwrap().unwrap().number, 1);
    }

    #[test]
    fn status_survives_close_and_reopen_but_a_reopened_landing_is_requeued() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        apply(&home, &[issue(1, "a"), issue(2, "b")]).unwrap();
        set_status(&home, 1, Status::Landed, None).unwrap();
        set_status(&home, 2, Status::Escalated, Some("which API?")).unwrap();
        assert!(next(&home).unwrap().is_none());

        apply(&home, &[]).unwrap();
        apply(&home, &[issue(1, "a"), issue(2, "b")]).unwrap();
        let escalated = get(&home, 2).unwrap();
        assert_eq!(escalated.status, Status::Escalated);
        assert_eq!(escalated.note.as_deref(), Some("which API?"));
        assert_eq!(
            get(&home, 1).unwrap().status,
            Status::Queued,
            "reverted work is queued"
        );
        assert!(format!("{:#}", get(&home, 9).unwrap_err()).contains("paja work sync"));
    }

    #[test]
    fn claim_hands_each_item_out_once() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        apply(&home, &[issue(1, "a"), issue(2, "b")]).unwrap();
        assert_eq!(claim(&home).unwrap().unwrap().number, 1);
        assert_eq!(claim(&home).unwrap().unwrap().number, 2);
        assert!(claim(&home).unwrap().is_none());
        assert_eq!(get(&home, 1).unwrap().status, Status::Active);
    }
}
