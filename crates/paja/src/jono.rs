//! Jono: the work queue. Work comes from GitHub issues labelled `paja`; `sync` imports them
//! into the event log, and the queue's state is derived by replaying it like everything else.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ahjo::Untampered;
use crate::events::Event;
use crate::home::{Home, RunLock};
use crate::mestari::{StepUsage, EV_ROUND};

/// Most issues one sync reads. A sync that hits it refuses to close anything.
pub const FETCH_LIMIT: usize = 1000;

/// The label that puts an issue in Jono.
pub const LABEL: &str = "paja";

pub const EV_IMPORTED: &str = "work.imported";
pub const EV_CHANGED: &str = "work.changed";
pub const EV_CLOSED: &str = "work.closed";
pub const EV_REOPENED: &str = "work.reopened";
pub const EV_STATUS: &str = "work.status";
/// A person answered an escalated item's question.
pub const EV_ANSWERED: &str = "work.answered";
/// A sync found which of an item's dependencies are still open: `{"number", "on": [...]}`.
pub const EV_WAITING: &str = "work.waiting";

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// What every agent step of the item's runs spent, added up from the `run.round` events.
    /// None until a step is recorded.
    #[serde(default)]
    pub usage: Option<Usage>,
    /// What a person answered to the item's escalations, oldest first. Every later run gets
    /// them in its prompts, since agents can't read the issue's comments.
    #[serde(default)]
    pub answers: Vec<Answer>,
    /// The issues this one depends on (`Depends on #N` lines in its body) that were still open
    /// at the last sync. Jono doesn't start an item until this is empty.
    #[serde(default)]
    pub waiting_on: Vec<u64>,
}

/// A person's answer to a question Paja asked about an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    /// The question, as the escalation asked it, when it was known.
    #[serde(default)]
    pub question: Option<String>,
    pub answer: String,
}

/// An item's agent usage, totalled over every recorded step. Steps whose result had no usage
/// fields (or no cost) count in `unknown_steps` and add nothing, so a total is never padded
/// with zeros it didn't see.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub steps: u64,
    pub unknown_steps: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub num_turns: u64,
    pub duration_ms: u64,
    /// Claude Code's estimate, in US dollars.
    pub cost_usd: f64,
}

impl Usage {
    fn add(&mut self, step: &StepUsage) {
        self.steps += 1;
        let tokens = [
            step.input_tokens,
            step.output_tokens,
            step.cache_read_input_tokens,
            step.cache_creation_input_tokens,
        ];
        if tokens.iter().any(Option::is_none) || step.cost_usd.is_none() {
            self.unknown_steps += 1;
        }
        self.input_tokens += step.input_tokens.unwrap_or(0);
        self.output_tokens += step.output_tokens.unwrap_or(0);
        self.cache_read_input_tokens += step.cache_read_input_tokens.unwrap_or(0);
        self.cache_creation_input_tokens += step.cache_creation_input_tokens.unwrap_or(0);
        self.num_turns += step.num_turns.unwrap_or(0);
        self.duration_ms += step.duration_ms.unwrap_or(0);
        self.cost_usd += step.cost_usd.unwrap_or(0.0);
    }
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
struct Waiting {
    number: u64,
    on: Vec<u64>,
}

#[derive(Deserialize)]
struct Number {
    number: u64,
}

/// The part of a `run.round` event the queue reads. Rounds recorded before usage was kept have
/// no `steps`. Each step is read on its own, so one Paja can't read doesn't stop the queue.
#[derive(Deserialize)]
struct Round {
    number: u64,
    #[serde(default)]
    steps: Value,
}

#[derive(Deserialize)]
struct Answered {
    number: u64,
    #[serde(flatten)]
    answer: Answer,
}

#[derive(Deserialize)]
struct StatusChange {
    number: u64,
    status: Status,
    note: Option<String>,
    /// Set when `paja run` claimed the item, so a run that stopped can be told apart from a
    /// person's or another worker's claim. Absent in older logs.
    #[serde(default)]
    run: bool,
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
                        usage: None,
                        answers: Vec::new(),
                        waiting_on: Vec::new(),
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
            EV_WAITING => {
                let waiting: Waiting =
                    serde_json::from_value(event.data.clone()).with_context(|| bad(event))?;
                if let Some(item) = items.get_mut(&waiting.number) {
                    item.waiting_on = waiting.on;
                }
            }
            EV_ANSWERED => {
                let answered: Answered =
                    serde_json::from_value(event.data.clone()).with_context(|| bad(event))?;
                if let Some(item) = items.get_mut(&answered.number) {
                    item.answers.push(answered.answer);
                }
            }
            EV_ROUND => {
                let round: Round =
                    serde_json::from_value(event.data.clone()).with_context(|| bad(event))?;
                // Steps that aren't a list count as one step whose usage is unknown.
                let steps = match round.steps {
                    Value::Null => Vec::new(),
                    Value::Array(steps) => steps,
                    other => vec![other],
                };
                if let (Some(item), false) = (items.get_mut(&round.number), steps.is_empty()) {
                    let usage = item.usage.get_or_insert_with(Usage::default);
                    for step in steps {
                        // Usage is informational: a step that can't be read counts as one whose
                        // usage is unknown.
                        match serde_json::from_value::<StepUsage>(step) {
                            Ok(step) => usage.add(&step),
                            Err(_) => {
                                usage.steps += 1;
                                usage.unknown_steps += 1;
                            }
                        }
                    }
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
    item.open && item.status == Status::Queued && item.waiting_on.is_empty()
}

/// The issues an issue's body says it depends on: every `#N` on a line that starts with
/// `Depends on` or `Blocked by` (any case, after an optional list marker), for example
/// `Depends on #12, #14`. An issue never depends on itself.
pub fn dependencies(number: u64, body: &str) -> Vec<u64> {
    let mut found = BTreeSet::new();
    for line in body.lines() {
        let line = line.trim_start().trim_start_matches(['-', '*', ' ']);
        let lower = line.to_ascii_lowercase();
        let Some(rest) = ["depends on", "blocked by"]
            .iter()
            .find_map(|lead| lower.strip_prefix(lead))
        else {
            continue;
        };
        for part in rest.split('#').skip(1) {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(n) = digits.parse::<u64>() {
                if n != number {
                    found.insert(n);
                }
            }
        }
    }
    found.into_iter().collect()
}

/// Queued items that wait on open dependencies, as `#179 waits on #178`, for a run that found
/// nothing ready. None when no queued item is waiting.
pub fn waiting(home: &Home) -> Result<Option<String>> {
    let waiting: Vec<String> = list(home)?
        .into_iter()
        .filter(|item| item.open && item.status == Status::Queued && !item.waiting_on.is_empty())
        .map(|item| {
            let on: Vec<String> = item.waiting_on.iter().map(|n| format!("#{n}")).collect();
            format!("#{} waits on {}", item.number, on.join(", "))
        })
        .collect();
    Ok((!waiting.is_empty()).then(|| waiting.join("; ")))
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

/// An item `paja run` claimed: the item, the lock that keeps other runs off it while this one
/// works, and whether it was resumed from a run that stopped before finishing it.
pub struct RunClaim {
    pub item: WorkItem,
    pub lock: RunLock,
    pub resumed: bool,
}

/// What `paja run` works on next, under the home lock. An item an earlier `paja run` left
/// active, and whose run is gone (stopped by Ctrl-C, a closed terminal or a reboot), comes
/// first, so it is resumed instead of being skipped for good. Otherwise the next queued item is
/// claimed. Items a person or another worker claimed with `paja work claim` are never taken.
pub fn claim_for_run(home: &Home) -> Result<Option<RunClaim>> {
    let lock = home.lock()?;
    let log = home.events();
    let events = log.read_all()?;
    // Which items' latest status change was a run's claim.
    let mut by_run = BTreeMap::new();
    // Replaying first refuses a status change it can't read, so every one parses here.
    let items = replay(&events)?;
    for event in events.iter().filter(|e| e.kind == EV_STATUS) {
        if let Ok(change) = serde_json::from_value::<StatusChange>(event.data.clone()) {
            by_run.insert(change.number, change.run && change.status == Status::Active);
        }
    }
    for item in items.values() {
        let left =
            item.open && item.status == Status::Active && by_run.get(&item.number) == Some(&true);
        if !left {
            continue;
        }
        if let Some(run_lock) = home.try_run_lock(item.number)? {
            return Ok(Some(RunClaim {
                item: item.clone(),
                lock: run_lock,
                resumed: true,
            }));
        }
    }
    for item in items.values().filter(|item| is_ready(item)) {
        // A queued item a live run still holds (set back by hand mid-run) waits for that run.
        let Some(run_lock) = home.try_run_lock(item.number)? else {
            continue;
        };
        log.append(
            &lock,
            EV_STATUS,
            json!({ "number": item.number, "status": Status::Active, "note": null, "run": true }),
        )?;
        let mut item = item.clone();
        item.status = Status::Active;
        item.note = None;
        return Ok(Some(RunClaim {
            item,
            lock: run_lock,
            resumed: false,
        }));
    }
    Ok(None)
}

/// Read a status name as the CLI and MCP tools take it.
pub fn parse_status(name: &str) -> Result<Status> {
    serde_json::from_value(json!(name)).map_err(|_| {
        anyhow::anyhow!("unknown status {name:?}; use queued, active, landed or escalated")
    })
}

/// Record a status change, for example when a run picks an item up or escalates it. An
/// escalated item is queued again only through [`answer`], so its next run gets the answer.
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
    if item.status == Status::Escalated && status == Status::Queued {
        // Queuing it this way would start a run that never sees the person's answer.
        bail!(
            "work item #{number} is waiting on an answer; answer with `paja work answer {number} \
             \"<your answer>\"` so the next run sees it (if you changed the issue instead, run \
             `paja work sync` and answer \"see the updated issue\")"
        );
    }
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
    let ok = crate::ahjo::untampered(home, repo)?;
    let issues = fetch_issues(&ok)?;
    // A dependency outside the queue (another label, or already closed) is asked about once
    // per sync; one in the queue is open by definition.
    let queued: BTreeSet<u64> = issues.iter().map(|issue| issue.number).collect();
    let outside: BTreeSet<u64> = issues
        .iter()
        .flat_map(|issue| dependencies(issue.number, &issue.body))
        .filter(|n| !queued.contains(n))
        .collect();
    let mut open_outside = BTreeSet::new();
    for number in outside {
        if issue_is_open(&ok, number)? {
            open_outside.insert(number);
        }
    }
    apply_with(home, &issues, &open_outside)
}

/// Bring the queue in line with the current open issues. Separate from `sync` so it can be
/// tested without GitHub.
pub fn apply(home: &Home, open: &[Issue]) -> Result<SyncReport> {
    apply_with(home, open, &BTreeSet::new())
}

/// [`apply`], where `open_outside` are the dependencies outside `open` that are still open.
pub fn apply_with(home: &Home, open: &[Issue], open_outside: &BTreeSet<u64>) -> Result<SyncReport> {
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
        let on: Vec<u64> = dependencies(issue.number, &issue.body)
            .into_iter()
            // A dependency Paja has landed counts as done even before GitHub closes it.
            .filter(|n| {
                let landed = items.get(n).is_some_and(|i| i.status == Status::Landed);
                !landed && (open.iter().any(|i| i.number == *n) || open_outside.contains(n))
            })
            .collect();
        let before = items
            .get(&issue.number)
            .map(|item| item.waiting_on.as_slice())
            .unwrap_or_default();
        if on != before {
            log.append(
                &lock,
                EV_WAITING,
                json!({ "number": issue.number, "on": on }),
            )?;
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

/// Answer an escalated item: record the person's answer in the event log and queue the item
/// again, in one step under the home lock, then post the answer on the issue so it is visible
/// there too. The next run passes every recorded answer to its agents.
pub fn answer(home: &Home, repo: &Path, number: u64, text: &str) -> Result<WorkItem> {
    if text.trim().is_empty() {
        bail!("the answer for #{number} is empty; say what Paja should do");
    }
    // gh runs git in the checkout to find its GitHub repository.
    let ok = crate::ahjo::untampered(home, repo)?;
    let item = {
        let lock = home.lock()?;
        let log = home.events();
        let mut item = replay(&log.read_all()?)?.remove(&number).with_context(|| {
            format!(
                "no work item #{number}; run `paja work sync` to import issues labelled `{LABEL}`"
            )
        })?;
        if item.status != Status::Escalated {
            bail!(
                "work item #{number} is {}, not escalated, so there is no question to answer; \
                 check `paja work show {number}`",
                item.status.as_str()
            );
        }
        let answer = Answer {
            question: item.note.clone(),
            answer: text.to_string(),
        };
        log.append(
            &lock,
            EV_ANSWERED,
            json!({ "number": number, "question": answer.question, "answer": answer.answer }),
        )?;
        log.append(
            &lock,
            EV_STATUS,
            json!({ "number": number, "status": Status::Queued, "note": null }),
        )?;
        item.answers.push(answer);
        item.status = Status::Queued;
        item.note = None;
        item
    };
    gh(
        &ok,
        &[
            "issue",
            "comment",
            &number.to_string(),
            "--body",
            &format!("**Answer to Paja's question.**\n\n{text}"),
        ],
    )
    .with_context(|| {
        format!(
            "the answer is recorded and #{number} is queued, so the next run uses it, but posting \
             it on the issue failed; post it there by hand if others should see it"
        )
    })?;
    Ok(item)
}

/// A GitHub issue filed for Jono.
#[derive(Debug, Clone, Serialize)]
pub struct Filed {
    pub url: String,
}

/// File a new GitHub issue labelled `paja`, for example a spec a person approved. The next sync
/// imports it as queued work.
pub fn file(home: &Home, repo: &Path, title: &str, body: &str) -> Result<Filed> {
    if title.trim().is_empty() {
        bail!("the issue needs a title; pass --title");
    }
    let ok = crate::ahjo::untampered(home, repo)?;
    let url = gh(
        &ok,
        &[
            "issue", "create", "--label", LABEL, "--title", title, "--body", body,
        ],
    )?;
    Ok(Filed {
        url: url.lines().last().unwrap_or_default().trim().to_string(),
    })
}

/// Run gh in the checkout the tamper lock allowed (`ok`). Every gh Jono runs goes through
/// here, and taking the proof means no command can run it without the check.
fn gh_output(ok: &Untampered, args: &[&str]) -> Result<std::process::Output> {
    let gh = gh_program();
    Command::new(&gh)
        .current_dir(ok.path())
        .args(args)
        .output()
        .with_context(|| format!("running `{gh}` (install the GitHub CLI and run `gh auth login`)"))
}

fn gh(ok: &Untampered, args: &[&str]) -> Result<String> {
    let repo = ok.path();
    if !repo.is_dir() {
        bail!(
            "{} is not a directory; pass --repo with a path inside the checkout",
            repo.display()
        );
    }
    let gh = gh_program();
    let output = gh_output(ok, args)?;
    if !output.status.success() {
        bail!(
            "`{gh} {} {}` failed in {}: {}; check `gh auth status` and that this checkout has a \
             GitHub remote",
            args[0],
            args[1],
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `gh`, or the program named by `PAJA_GH` (tests point it at a fake).
pub fn gh_program() -> String {
    std::env::var("PAJA_GH").unwrap_or_else(|_| "gh".to_string())
}

fn fetch_issues(ok: &Untampered) -> Result<Vec<Issue>> {
    let repo = ok.path();
    let gh = gh_program();
    let output = gh_output(
        ok,
        &[
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
        ],
    )?;
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

/// Whether issue `number` of the checkout's GitHub repository is open. One that can't be read
/// (a typo, a deleted issue) counts as open, so the item waits and `paja work list` shows why.
fn issue_is_open(ok: &Untampered, number: u64) -> Result<bool> {
    let output = gh_output(
        ok,
        &["issue", "view", &number.to_string(), "--json", "state"],
    )?;
    let state = serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .and_then(|v| v["state"].as_str().map(str::to_ascii_uppercase));
    Ok(!(output.status.success() && state.as_deref() == Some("CLOSED")))
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
    fn dependencies_come_from_depends_on_and_blocked_by_lines() {
        let body = "Intro mentions #3.\nDepends on #12, #14\n- **Blocked by** #9 and #12\n\
                    depends on #5 (itself)\nNot a dependency: see #20";
        assert_eq!(dependencies(5, body), vec![9, 12, 14]);
        assert!(dependencies(1, "").is_empty());
    }

    #[test]
    fn an_item_waits_until_its_dependencies_close_or_land() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        let first = issue(1, "Desk app");
        let mut second = issue(2, "Desk feature");
        second.body = "Depends on #1 and #7".into();
        let third = issue(3, "Other");
        let outside: BTreeSet<u64> = [7].into();
        apply_with(
            &home,
            &[first.clone(), second.clone(), third.clone()],
            &outside,
        )
        .unwrap();
        assert_eq!(get(&home, 2).unwrap().waiting_on, vec![1, 7]);
        // The oldest ready item comes first; #2 is skipped while it waits.
        assert_eq!(claim(&home).unwrap().unwrap().number, 1);
        assert_eq!(claim(&home).unwrap().unwrap().number, 3);
        assert!(claim(&home).unwrap().is_none());
        assert_eq!(
            waiting(&home).unwrap().as_deref(),
            Some("#2 waits on #1, #7")
        );
        // #7 closes outside the queue and #1 lands before GitHub closes it.
        set_status(&home, 1, Status::Landed, None).unwrap();
        apply_with(&home, &[first, second.clone(), third], &BTreeSet::new()).unwrap();
        assert!(get(&home, 2).unwrap().waiting_on.is_empty());
        assert_eq!(claim(&home).unwrap().unwrap().number, 2);
        // An unchanged sync records nothing new.
        let before = home.events().read_all().unwrap().len();
        apply_with(&home, &[second], &BTreeSet::new()).unwrap();
        let after = home.events().read_all().unwrap();
        assert!(after[before..].iter().all(|e| e.kind != EV_WAITING));
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
    fn an_escalated_item_is_queued_again_only_by_an_answer() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        apply(&home, &[issue(1, "a")]).unwrap();
        set_status(&home, 1, Status::Escalated, Some("which API?")).unwrap();
        let err = format!(
            "{:#}",
            set_status(&home, 1, Status::Queued, None).unwrap_err()
        );
        assert!(err.contains("paja work answer 1"), "{err}");
        let item = get(&home, 1).unwrap();
        assert_eq!(item.status, Status::Escalated, "nothing was recorded");
        assert_eq!(item.note.as_deref(), Some("which API?"));
        // Other moves out of escalated still work, for example closing the matter by hand.
        assert_eq!(
            set_status(&home, 1, Status::Landed, None).unwrap().status,
            Status::Landed
        );
    }

    #[test]
    fn a_run_resumes_only_an_item_whose_run_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        apply(&home, &[issue(1, "a"), issue(2, "b"), issue(3, "c")]).unwrap();
        // A worker's claim is never taken over by a run.
        assert_eq!(claim(&home).unwrap().unwrap().number, 1);
        let first = claim_for_run(&home).unwrap().unwrap();
        assert_eq!((first.item.number, first.resumed), (2, false));
        // While its run lives, the item is not handed out again.
        let next = claim_for_run(&home).unwrap().unwrap();
        assert_eq!((next.item.number, next.resumed), (3, false));
        assert!(claim_for_run(&home).unwrap().is_none());
        // Once that run is gone, its item is resumed.
        drop(first);
        let again = claim_for_run(&home).unwrap().unwrap();
        assert_eq!((again.item.number, again.resumed), (2, true));
        assert_eq!(again.item.status, Status::Active);
        // An item that left `active` is never resumed.
        drop(again);
        set_status(&home, 2, Status::Escalated, Some("which API?")).unwrap();
        assert!(claim_for_run(&home).unwrap().is_none());
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

    #[test]
    fn usage_adds_up_rounds_and_reads_old_rounds_without_steps() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        apply(&home, &[issue(1, "One")]).unwrap();
        let lock = home.lock().unwrap();
        let log = home.events();
        // A round recorded before usage was kept.
        log.append(
            &lock,
            EV_ROUND,
            json!({ "number": 1, "round": 1, "verdict": "changes", "detail": "" }),
        )
        .unwrap();
        assert_eq!(get(&home, 1).unwrap().usage, None);
        let known = json!({ "step": "build", "input_tokens": 10, "output_tokens": 2,
            "cache_read_input_tokens": 100, "cache_creation_input_tokens": 5, "cost_usd": 0.5,
            "num_turns": 3, "duration_ms": 1000, "session_id": "a" });
        let unknown = json!({ "step": "review", "reviewer": 1, "input_tokens": null,
            "output_tokens": null, "cache_read_input_tokens": null,
            "cache_creation_input_tokens": null, "cost_usd": null, "num_turns": 2,
            "duration_ms": null, "session_id": null });
        log.append(
            &lock,
            EV_ROUND,
            json!({ "number": 1, "round": 2, "steps": [known.clone(), unknown] }),
        )
        .unwrap();
        log.append(
            &lock,
            EV_ROUND,
            json!({ "number": 1, "round": 3, "steps": [known] }),
        )
        .unwrap();
        drop(lock);
        let usage = get(&home, 1).unwrap().usage.unwrap();
        assert_eq!(usage.steps, 3);
        assert_eq!(usage.unknown_steps, 1);
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.cache_read_input_tokens, 200);
        assert_eq!(usage.num_turns, 8);
        assert_eq!(usage.cost_usd, 1.0);

        // A step Paja can't read counts as unknown; it doesn't stop the queue.
        let lock = home.lock().unwrap();
        for steps in [json!([{ "step": 7 }]), json!("not a list")] {
            log.append(
                &lock,
                EV_ROUND,
                json!({ "number": 1, "round": 4, "steps": steps }),
            )
            .unwrap();
        }
        drop(lock);
        let usage = get(&home, 1).unwrap().usage.unwrap();
        assert_eq!((usage.steps, usage.unknown_steps), (5, 3));
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(list(&home).unwrap().len(), 1);
    }

    #[test]
    fn answers_are_kept_with_their_questions() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        apply(&home, &[issue(1, "One")]).unwrap();
        let lock = home.lock().unwrap();
        home.events()
            .append(
                &lock,
                EV_ANSWERED,
                json!({ "number": 1, "question": "Which API?", "answer": "v2" }),
            )
            .unwrap();
        // An answer recorded without its question still counts.
        home.events()
            .append(
                &lock,
                EV_ANSWERED,
                json!({ "number": 1, "answer": "Keep it small" }),
            )
            .unwrap();
        drop(lock);
        let answers = get(&home, 1).unwrap().answers;
        assert_eq!(
            answers,
            [
                Answer {
                    question: Some("Which API?".into()),
                    answer: "v2".into()
                },
                Answer {
                    question: None,
                    answer: "Keep it small".into()
                },
            ]
        );
    }
}
