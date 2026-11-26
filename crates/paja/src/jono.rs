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
    // An item no sync has judged yet (imported before Jono read dependencies) waits on every
    // dependency its body names, so a claim before the next sync can't start it too early.
    let recorded = waiting_recorded(events);
    for item in items.values_mut() {
        if !recorded.contains(&item.number) {
            item.waiting_on = dependencies(item.number, &item.body);
        }
    }
    Ok(items)
}

/// The items a `work.waiting` event was ever recorded for.
fn waiting_recorded(events: &[Event]) -> BTreeSet<u64> {
    events
        .iter()
        .filter(|e| e.kind == EV_WAITING)
        .filter_map(|e| e.data["number"].as_u64())
        .collect()
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
/// `Depends on` or `Blocked by` (any case, after optional quote, list or number markers and
/// bold), for example `Depends on #12, #14`. Lines inside fenced code blocks don't count, and
/// neither does a reference to another repository (`other/repo#12`), which Jono can't check.
/// An issue never depends on itself.
pub fn dependencies(number: u64, body: &str) -> Vec<u64> {
    let mut found = BTreeSet::new();
    // Lines outside fenced code blocks. An unclosed fence hides nothing: its lines count too,
    // so a stray fence can't make an item start early.
    let mut counted: Vec<&str> = Vec::new();
    // The open fence's character and length, and the lines inside it so far.
    let mut fence: Option<(char, usize)> = None;
    let mut inside: Vec<&str> = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next().filter(|c| *c == '`' || *c == '~');
        let run = marker.map_or(0, |m| trimmed.chars().take_while(|c| *c == m).count());
        match (fence, marker) {
            // A backtick fence's info string can't hold a backtick (CommonMark), so such a line
            // is text. The opening line is held with the fence's lines, so it counts too if the
            // fence never closes.
            (None, Some(m)) if run >= 3 && !(m == '`' && trimmed[run..].contains('`')) => {
                fence = Some((m, run));
                // Its info string, which is what would count.
                inside.push(&trimmed[run..]);
            }
            // Closed only by the same character, at least as many times, and nothing else.
            (Some((open, len)), Some(m))
                if m == open && run >= len && trimmed[run..].trim().is_empty() =>
            {
                fence = None;
                inside.clear();
            }
            (Some(_), _) => inside.push(line),
            (None, _) => counted.push(line),
        }
    }
    counted.extend(inside);
    for line in counted {
        let trimmed = line.trim_start();
        // Quote and bullet markers, a numbered-list marker (`1.`, `2)`), then bold or italics.
        let line = trimmed.trim_start_matches(['>', '-', '*', '+', ' ', '\t']);
        let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        let line = match line[digits..].chars().next() {
            Some('.' | ')') if digits > 0 => &line[digits + 1..],
            _ => line,
        };
        let line = line.trim_start_matches([' ', '\t', '*', '_']);
        let lower = line.to_ascii_lowercase();
        let Some(rest) = ["depends on", "blocked by"]
            .iter()
            .find_map(|lead| lower.strip_prefix(lead))
            // A whole phrase, not the start of a longer word (`depends onboarding`).
            .filter(|rest| !rest.starts_with(|c: char| c.is_ascii_alphanumeric()))
        else {
            continue;
        };
        let mut parts = rest.split('#');
        let mut before = parts.next().unwrap_or("");
        for part in parts {
            // `owner/repo#12` names another repository's issue: the word right before the `#`
            // has a name on both sides of a `/`. `#12/#14` and `#12-#14` are local.
            let word = before
                .rsplit(|c: char| c.is_whitespace() || ",;:()[]".contains(c))
                .next()
                .unwrap_or("");
            let named = |side: &str| {
                !side.is_empty()
                    && side
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            };
            let other_repo = word
                .split_once('/')
                .is_some_and(|(owner, repo)| named(owner) && named(repo));
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            if let (Ok(n), false) = (digits.parse::<u64>(), other_repo) {
                if n != number {
                    found.insert(n);
                }
            }
            before = part;
        }
    }
    found.into_iter().collect()
}

/// Queued items that wait on open dependencies, as `#179 waits on #178`, for a run that found
/// nothing ready. None when no queued item is waiting.
pub fn waiting(home: &Home) -> Result<Option<String>> {
    let items = list(home)?;
    let mut waiting: Vec<String> = items
        .iter()
        .filter(|item| is_waiting(item))
        .map(|item| {
            let on: Vec<String> = item.waiting_on.iter().map(|n| format!("#{n}")).collect();
            format!("#{} waits on {}", item.number, on.join(", "))
        })
        .collect();
    waiting.extend(cycle_note(&items));
    Ok((!waiting.is_empty()).then(|| waiting.join("; ")))
}

/// An open, queued item held back by open dependencies.
fn is_waiting(item: &WorkItem) -> bool {
    item.open && item.status == Status::Queued && !item.waiting_on.is_empty()
}

/// The "nothing can start" line for a dependency cycle among the waiting ones of `items` (as
/// `list` returns them), for `paja work list` and for a run that found nothing ready.
pub fn cycle_note(items: &[WorkItem]) -> Option<String> {
    let waiting: Vec<&WorkItem> = items.iter().filter(|item| is_waiting(item)).collect();
    cycle(&waiting).map(|cycle| {
        let shown: Vec<String> = cycle.iter().map(|n| format!("#{n}")).collect();
        format!(
            "{} wait on each other, so none can start; remove one `Depends on` line",
            shown.join(" -> ")
        )
    })
}

/// A dependency cycle among waiting items, as the numbers along it with the first repeated at
/// the end (`[3, 5, 3]`), or None.
fn cycle(items: &[&WorkItem]) -> Option<Vec<u64>> {
    cycle_search(items, usize::MAX).0
}

/// `cycle`, with how many times the search entered an item. It gives up, finding nothing, once
/// it has entered more than `limit`, so a test that bounds the work fails rather than hangs.
fn cycle_search(items: &[&WorkItem], limit: usize) -> (Option<Vec<u64>>, usize) {
    let mut entered = 0;
    let edges: BTreeMap<u64, &[u64]> = items
        .iter()
        .map(|item| (item.number, item.waiting_on.as_slice()))
        .collect();
    for &start in edges.keys() {
        // Depth first from `start`, keeping the path.
        let mut path = vec![start];
        // Each item is entered once per start, so the search stays linear in the edges.
        let mut seen = BTreeSet::from([start]);
        let mut stack = vec![(start, 0usize)];
        while let Some((node, next)) = stack.pop() {
            let deps = edges.get(&node).copied().unwrap_or_default();
            if let Some(&dep) = deps.get(next) {
                stack.push((node, next + 1));
                if dep == start {
                    path.push(start);
                    return (Some(path), entered);
                }
                if edges.contains_key(&dep) && seen.insert(dep) {
                    entered += 1;
                    if entered > limit {
                        return (None, entered);
                    }
                    path.push(dep);
                    stack.push((dep, 0));
                }
            } else {
                path.pop();
            }
        }
    }
    (None, entered)
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
    let events = log.read_all()?;
    let items = replay(&events)?;
    let recorded = waiting_recorded(&events);
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
        let named = dependencies(issue.number, &issue.body);
        let on: Vec<u64> = named
            .iter()
            .copied()
            // A dependency Paja has landed counts as done even before GitHub closes it, unless
            // this very sync finds it opened again.
            .filter(|n| {
                let reopened =
                    items.get(n).is_some_and(|i| !i.open) && open.iter().any(|i| i.number == *n);
                let landed = items.get(n).is_some_and(|i| i.status == Status::Landed);
                (!landed || reopened)
                    && (open.iter().any(|i| i.number == *n) || open_outside.contains(n))
            })
            .collect();
        let before = items
            .get(&issue.number)
            .map(|item| item.waiting_on.as_slice())
            .unwrap_or_default();
        // The first judgement of an item that names dependencies is recorded even when it
        // matches, so replay stops assuming it waits on all of them.
        let first = !recorded.contains(&issue.number) && !named.is_empty();
        if on != before || first {
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
    crate::platform::own_process_group(Command::new(&gh).env("GH_PROMPT_DISABLED", "1"))
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

/// Whether issue or pull request `number` of the checkout's GitHub repository is open (a
/// merged pull request is closed). One that can't be read (a typo, a deleted issue) counts as
/// open, so the item waits and `paja work list` shows why.
fn issue_is_open(ok: &Untampered, number: u64) -> Result<bool> {
    let path = format!("repos/{{owner}}/{{repo}}/issues/{number}");
    let output = gh_output(ok, &["api", &path, "--jq", ".state"])?;
    let state = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_ascii_lowercase();
    Ok(!(output.status.success() && state == "closed"))
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
        // Numbered, quoted and plus-bulleted lines count; a longer word, a fenced code block
        // and another repository's issue don't.
        let body = "1. Depends on #3\n> Blocked by #4\n+ depends on #6\n2) **Depends on** #7\n\
                    depends onboarding #5\n```\nDepends on #8\n```\n\
                    Depends on other/repo#9 and #10, org/r#11";
        assert_eq!(dependencies(1, body), vec![3, 4, 6, 7, 10]);
        // A range or list written with `/` or `-` is local, not another repository.
        assert_eq!(
            dependencies(1, "Depends on #12/#14 and #15-#16"),
            vec![12, 14, 15, 16]
        );
        // An unclosed fence hides nothing, and a `~~~` fence isn't closed by backticks.
        assert_eq!(dependencies(1, "```\nDepends on #3"), vec![3]);
        assert_eq!(
            dependencies(1, "~~~\nDepends on #3\n```\nDepends on #4"),
            vec![3, 4]
        );
        assert_eq!(
            dependencies(1, "~~~\nDepends on #3\n~~~\nDepends on #4"),
            vec![4]
        );
        // A number that isn't a list marker leaves the line alone.
        assert!(dependencies(1, "10 depends on #3").is_empty());
        // An unclosed fence's opening line counts, and a backtick line with a backtick after
        // its run isn't a fence, so it hides nothing.
        assert_eq!(dependencies(1, "~~~ Depends on #3"), vec![3]);
        assert_eq!(dependencies(1, "```x``` and\nDepends on #3\n```"), vec![3]);
    }

    #[test]
    fn a_dependency_cycle_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        let mut a = issue(3, "A");
        a.body = "Depends on #5".into();
        let mut b = issue(5, "B");
        b.body = "Depends on #3".into();
        let mut c = issue(7, "C");
        c.body = "Depends on #3".into();
        apply(&home, &[a, b, c]).unwrap();
        let shown = waiting(&home).unwrap().unwrap();
        assert!(
            shown.contains("#3 -> #5 -> #3 wait on each other"),
            "{shown}"
        );
        assert!(shown.contains("#7 waits on #3"), "{shown}");
        assert!(cycle_note(&list(&home).unwrap())
            .unwrap()
            .starts_with("#3 -> #5 -> #3"));
    }

    #[test]
    fn the_cycle_search_stays_fast_on_a_dense_graph() {
        // Every item waits on every later one: no cycle, and many paths.
        let items: Vec<WorkItem> = (1..=60u64)
            .map(|n| {
                serde_json::from_value(serde_json::json!({
                    "number": n, "title": "", "body": "", "url": "",
                    "open": true, "status": "queued",
                    "waiting_on": (n + 1..=60).collect::<Vec<u64>>()
                }))
                .unwrap()
            })
            .collect();
        let items: Vec<&WorkItem> = items.iter().collect();
        // Each start enters every other item at most once; a search that followed every path
        // would enter them exponentially many times, and stops at the limit.
        let (found, entered) = cycle_search(&items, 60 * 60);
        assert!(entered <= 60 * 60, "{entered}");
        assert!(found.is_none());
    }

    #[test]
    fn an_item_no_sync_judged_waits_on_every_dependency_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        // Imported by an older Paja, which recorded no `work.waiting`.
        let lock = home.lock().unwrap();
        let mut old = issue(2, "Old");
        old.body = "Depends on #1".into();
        home.events()
            .append(&lock, EV_IMPORTED, serde_json::to_value(&old).unwrap())
            .unwrap();
        drop(lock);
        assert_eq!(get(&home, 2).unwrap().waiting_on, vec![1]);
        assert!(claim(&home).unwrap().is_none());
        // The first sync records its judgement, even though #1 is closed.
        apply(&home, &[old.clone()]).unwrap();
        assert!(get(&home, 2).unwrap().waiting_on.is_empty());
        assert_eq!(claim(&home).unwrap().unwrap().number, 2);
        // A new import whose dependency is already closed doesn't wait either.
        let mut new = issue(4, "New");
        new.body = "Depends on #1".into();
        apply(&home, &[old, new]).unwrap();
        assert!(get(&home, 4).unwrap().waiting_on.is_empty());
    }

    #[test]
    fn a_landed_dependency_reopened_in_the_same_sync_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        let first = issue(1, "First");
        let mut second = issue(2, "Second");
        second.body = "Depends on #1".into();
        apply(&home, &[first.clone(), second.clone()]).unwrap();
        set_status(&home, 1, Status::Landed, None).unwrap();
        // GitHub closes #1, then it is reopened (a revert, say) before the next sync.
        apply(&home, &[second.clone()]).unwrap();
        assert!(get(&home, 2).unwrap().waiting_on.is_empty());
        apply(&home, &[first, second]).unwrap();
        assert_eq!(get(&home, 2).unwrap().waiting_on, vec![1]);
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
